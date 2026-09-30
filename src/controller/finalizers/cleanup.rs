use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::{PersistentVolume, PersistentVolumeClaim, VolumeAttachment};
use kube::{api::{Api, Patch, PatchParams}, Client, ResourceExt};
use serde_json::json;
use tracing::{debug, error, warn};

use crate::controller::reconciler::cleanup_stellar_node;
use crate::controller::{finalizers, reconciler::ControllerState};
use crate::crd::StellarNode;

use super::cloud_verify::{
    identity_for_pv, volume_is_safe_to_finalize, CloudStorageApi, CloudVolumeState,
    RealCloudStorageApi, VolumeIdentity,
};

const RECOVERY_INTERVAL: Duration = Duration::from_secs(30);
const VOLUME_IDENTITY_ANNOTATION: &str = "stellar.org/finalizer-volume-identity";

pub(crate) async fn run_recovery_controller(state: Arc<ControllerState>) {
    let cloud = RealCloudStorageApi::new();
    let mut interval = tokio::time::interval(RECOVERY_INTERVAL);
    loop {
        interval.tick().await;
        if !state.is_leader.load(Ordering::Relaxed) || state.dry_run {
            continue;
        }
        if let Err(error) = recover_deleting_nodes(&state.client, &state, &cloud).await {
            error!("Finalizer recovery scan failed: {error}");
        }
    }
}

pub(crate) async fn recover_deleting_nodes(
    client: &Client,
    state: &Arc<ControllerState>,
    cloud: &impl CloudStorageApi,
) -> anyhow::Result<()> {
    let nodes: Api<StellarNode> = match &state.watch_namespace {
        Some(namespace) => Api::namespaced(client.clone(), namespace),
        None => Api::all(client.clone()),
    };
    let list = nodes.list(&Default::default()).await?;
    for node in list.items {
        if !finalizers::is_being_deleted(&node) || !finalizers::has_finalizer(&node) {
            continue;
        }

        debug!(node = %node.name_any(), "Recovering a deleting StellarNode");
        if let Err(error) = cleanup_stellar_node(
            client.clone(),
            Arc::new(node.clone()),
            state.clone(),
        )
        .await
        {
            warn!(node = %node.name_any(), "Recovery cleanup failed: {error}");
            continue;
        }

        if !storage_cleanup_complete(client, &node, cloud).await {
            warn!(
                node = %node.name_any(),
                "Cloud storage is attached, not deleted, or could not be verified; retaining finalizer"
            );
            continue;
        }

        if let Err(error) = finalizers::remove_finalizer(client, &node).await {
            warn!(node = %node.name_any(), "Could not remove recovered finalizer: {error}");
        }
    }
    Ok(())
}

pub(crate) async fn storage_cleanup_complete(
    client: &Client,
    node: &StellarNode,
    cloud: &impl CloudStorageApi,
) -> bool {
    match storage_cleanup_complete_inner(client, node, cloud).await {
        Ok(complete) => complete,
        Err(error) => {
            warn!(node = %node.name_any(), "Storage verification failed closed: {error}");
            false
        }
    }
}

async fn storage_cleanup_complete_inner(
    client: &Client,
    node: &StellarNode,
    cloud: &impl CloudStorageApi,
) -> anyhow::Result<bool> {
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let pvc_name = super::super::resources::resource_name(node, "data");
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let pvc = pvcs.get_opt(&pvc_name).await?;

    let pv_name = match pvc
        .as_ref()
        .and_then(|claim| claim.spec.as_ref())
        .and_then(|spec| spec.volume_name.as_deref())
    {
        Some(name) => Some(name.to_string()),
        None => find_pv_by_claim(client, &namespace, &pvc_name).await?,
    };

    let Some(pv_name) = pv_name else {
        if !node.spec.should_delete_pvc() {
            return Ok(true);
        }
        return match stored_volume_identity(node)? {
            Some(StoredVolumeIdentity::NeverBound) => Ok(pvc.is_none()),
            Some(StoredVolumeIdentity::Cloud(identity)) => {
                Ok(cloud.state(&identity).await? == CloudVolumeState::Deleted)
            }
            None => Ok(false),
        };
    };

    let pvs: Api<PersistentVolume> = Api::all(client.clone());
    let Some(pv) = pvs.get_opt(&pv_name).await? else {
        return match stored_volume_identity(node)? {
            Some(StoredVolumeIdentity::Cloud(identity)) => {
                Ok(cloud.state(&identity).await? == CloudVolumeState::Deleted)
            }
            _ => Ok(false),
        };
    };

    if has_active_volume_attachment(client, &pv_name).await? {
        return Ok(false);
    }

    volume_is_safe_to_finalize(&pv, node.spec.should_delete_pvc(), cloud).await
}

pub(crate) async fn persist_volume_identity_before_delete(
    client: &Client,
    node: &StellarNode,
) -> anyhow::Result<()> {
    if !node.spec.should_delete_pvc()
        || node
            .metadata
            .annotations
            .as_ref()
            .is_some_and(|annotations| annotations.contains_key(VOLUME_IDENTITY_ANNOTATION))
    {
        return Ok(());
    }

    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());
    let pvc_name = super::super::resources::resource_name(node, "data");
    let pvcs: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), &namespace);
    let pvc = pvcs.get_opt(&pvc_name).await?;
    let pv_name = match pvc
        .as_ref()
        .and_then(|claim| claim.spec.as_ref())
        .and_then(|spec| spec.volume_name.as_deref())
    {
        Some(name) => Some(name.to_string()),
        None => find_pv_by_claim(client, &namespace, &pvc_name).await?,
    };

    let encoded_identity = if let Some(pv_name) = pv_name {
        let pvs: Api<PersistentVolume> = Api::all(client.clone());
        let pv = pvs
            .get_opt(&pv_name)
            .await?
            .ok_or_else(|| anyhow::anyhow!("bound PV {pv_name} disappeared before identity capture"))?;
        serde_json::to_string(&StoredVolumeIdentity::Cloud(identity_for_pv(&pv)?))?
    } else if pvc.is_some() {
        serde_json::to_string(&StoredVolumeIdentity::NeverBound)?
    } else {
        anyhow::bail!("PVC and PV are missing and no prior cloud volume identity was recorded")
    };

    let nodes: Api<StellarNode> = Api::namespaced(client.clone(), &namespace);
    nodes
        .patch(
            &node.name_any(),
            &PatchParams::default(),
            &Patch::Merge(&json!({
                "metadata": {
                    "annotations": {
                        VOLUME_IDENTITY_ANNOTATION: encoded_identity
                    }
                }
            })),
        )
        .await?;
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
enum StoredVolumeIdentity {
    NeverBound,
    Cloud(VolumeIdentity),
}

fn stored_volume_identity(node: &StellarNode) -> anyhow::Result<Option<StoredVolumeIdentity>> {
    node.metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(VOLUME_IDENTITY_ANNOTATION))
        .map(|value| serde_json::from_str(value).map_err(Into::into))
        .transpose()
}

async fn find_pv_by_claim(
    client: &Client,
    namespace: &str,
    claim_name: &str,
) -> anyhow::Result<Option<String>> {
    let pvs: Api<PersistentVolume> = Api::all(client.clone());
    let list = pvs.list(&Default::default()).await?;
    Ok(list.items.into_iter().find_map(|pv| {
        let claim = pv.spec.as_ref()?.claim_ref.as_ref()?;
        (claim.namespace.as_deref() == Some(namespace)
            && claim.name.as_deref() == Some(claim_name))
        .then(|| pv.name_any())
    }))
}

async fn has_active_volume_attachment(client: &Client, pv_name: &str) -> anyhow::Result<bool> {
    let attachments: Api<VolumeAttachment> = Api::all(client.clone());
    let list = attachments.list(&Default::default()).await?;
    Ok(list.items.iter().any(|attachment| {
        attachment
            .spec
            .as_ref()
            .and_then(|spec| spec.source.persistent_volume_name.as_deref())
            == Some(pv_name)
            && attachment
                .status
                .as_ref()
                .and_then(|status| status.attached)
                != Some(false)
    }))
}