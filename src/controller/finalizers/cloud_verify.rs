use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use aws_config::{BehaviorVersion, Region};
use aws_sdk_ec2::Client as Ec2Client;
use k8s_openapi::api::core::v1::PersistentVolume;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::OnceCell;
use tracing::warn;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum VolumeIdentity {
    AwsEbs { volume_id: String, region: String },
    GcpPd {
        project: String,
        scope: GcpDiskScope,
        location: String,
        disk: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum GcpDiskScope {
    Zone,
    Region,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CloudVolumeState {
    Attached,
    Detached,
    Deleted,
}

#[async_trait]
pub(crate) trait CloudStorageApi: Send + Sync {
    async fn state(&self, volume: &VolumeIdentity) -> Result<CloudVolumeState>;
}

#[derive(Clone, Default)]
pub(crate) struct RealCloudStorageApi {
    http: reqwest::Client,
}

impl RealCloudStorageApi {
    pub(crate) fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }

    async fn aws_state(&self, volume_id: &str, region: &str) -> Result<CloudVolumeState> {
        static AWS_CONFIG: OnceCell<aws_config::SdkConfig> = OnceCell::const_new();
        let shared_config = AWS_CONFIG
            .get_or_init(|| async { aws_config::load_defaults(BehaviorVersion::latest()).await })
            .await;
        let config = aws_sdk_ec2::config::Builder::from(shared_config)
            .region(Region::new(region.to_string()))
            .build();
        let client = Ec2Client::from_conf(config);

        match client
            .describe_volumes()
            .volume_ids(volume_id)
            .send()
            .await
        {
            Ok(response) => {
                let volume = response
                    .volumes()
                    .first()
                    .ok_or_else(|| anyhow!("EC2 returned no record for {volume_id}"))?;
                if volume.attachments().is_empty() {
                    Ok(CloudVolumeState::Detached)
                } else {
                    Ok(CloudVolumeState::Attached)
                }
            }
            Err(error)
                if error
                    .as_service_error()
                    .and_then(|service_error| service_error.code())
                    == Some("InvalidVolume.NotFound") =>
            {
                Ok(CloudVolumeState::Deleted)
            }
            Err(error) => Err(anyhow!("EC2 DescribeVolumes failed: {error}")),
        }
    }

    async fn gcp_state(
        &self,
        project: &str,
        scope: &GcpDiskScope,
        location: &str,
        disk: &str,
    ) -> Result<CloudVolumeState> {
        let access_token = match std::env::var("GOOGLE_OAUTH_ACCESS_TOKEN") {
            Ok(token) if !token.trim().is_empty() => token,
            _ => {
                let response = self
                    .http
                    .get("http://metadata.google.internal/computeMetadata/v1/instance/service-accounts/default/token")
                    .header("Metadata-Flavor", "Google")
                    .send()
                    .await?
                    .error_for_status()?;
                response.json::<MetadataToken>().await?.access_token
            }
        };

        let scope_path = match scope {
            GcpDiskScope::Zone => "zones",
            GcpDiskScope::Region => "regions",
        };
        let url = format!(
            "https://compute.googleapis.com/compute/v1/projects/{project}/{scope_path}/{location}/disks/{disk}"
        );
        let response = self
            .http
            .get(url)
            .bearer_auth(access_token)
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(CloudVolumeState::Deleted);
        }
        let response = response.error_for_status()?;
        let disk: Value = response.json().await?;
        let users = disk
            .get("users")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("GCE disk response omitted its users field"))?;
        if users.is_empty() {
            Ok(CloudVolumeState::Detached)
        } else {
            Ok(CloudVolumeState::Attached)
        }
    }
}

#[async_trait]
impl CloudStorageApi for RealCloudStorageApi {
    async fn state(&self, volume: &VolumeIdentity) -> Result<CloudVolumeState> {
        match volume {
            VolumeIdentity::AwsEbs { volume_id, region } => {
                self.aws_state(volume_id, region).await
            }
            VolumeIdentity::GcpPd {
                project,
                scope,
                location,
                disk,
            } => self.gcp_state(project, scope, location, disk).await,
        }
    }
}

pub(crate) fn identity_for_pv(pv: &PersistentVolume) -> Result<VolumeIdentity> {
    let spec = pv
        .spec
        .as_ref()
        .ok_or_else(|| anyhow!("PersistentVolume has no spec"))?;

    if let Some(csi) = &spec.csi {
        match csi.driver.as_str() {
            "ebs.csi.aws.com" => {
                let volume_id = csi
                    .volume_handle
                    .rsplit('/')
                    .next()
                    .filter(|id| id.starts_with("vol-"))
                    .ok_or_else(|| anyhow!("invalid EBS CSI volume handle"))?;
                let region = aws_region(pv, &csi.volume_handle)?;
                return Ok(VolumeIdentity::AwsEbs {
                    volume_id: volume_id.to_string(),
                    region,
                });
            }
            "pd.csi.storage.gke.io" => return parse_gcp_disk_handle(&csi.volume_handle),
            other => bail!("unsupported CSI storage driver {other}; refusing finalizer removal"),
        }
    }

    if let Some(ebs) = &spec.aws_elastic_block_store {
        let volume_id = ebs
            .volume_id
            .rsplit('/')
            .next()
            .filter(|id| id.starts_with("vol-"))
            .ok_or_else(|| anyhow!("invalid in-tree EBS volume ID"))?;
        return Ok(VolumeIdentity::AwsEbs {
            volume_id: volume_id.to_string(),
            region: aws_region(pv, &ebs.volume_id)?,
        });
    }

    if spec.gce_persistent_disk.is_some() {
        bail!("in-tree GCE PD volume lacks a verifiable project and location")
    }

    bail!("PersistentVolume does not identify a supported cloud block volume")
}

pub(crate) async fn volume_is_safe_to_finalize(
    pv: &PersistentVolume,
    delete_requested: bool,
    cloud: &impl CloudStorageApi,
) -> Result<bool> {
    let identity = identity_for_pv(pv)?;
    let state = cloud.state(&identity).await?;
    Ok(if delete_requested {
        state == CloudVolumeState::Deleted
    } else {
        matches!(state, CloudVolumeState::Detached | CloudVolumeState::Deleted)
    })
}

fn parse_gcp_disk_handle(handle: &str) -> Result<VolumeIdentity> {
    let parts: Vec<&str> = handle.trim_matches('/').split('/').collect();
    let (project, scope, location, disk) = match parts.as_slice() {
        ["projects", project, "zones", zone, "disks", disk] => {
            (*project, GcpDiskScope::Zone, *zone, *disk)
        }
        ["projects", project, "regions", region, "disks", disk] => {
            (*project, GcpDiskScope::Region, *region, *disk)
        }
        _ => bail!("invalid GCP PD CSI volume handle"),
    };
    if [project, location, disk].iter().any(|part| part.is_empty()) {
        bail!("incomplete GCP PD CSI volume handle")
    }
    Ok(VolumeIdentity::GcpPd {
        project: project.to_string(),
        scope,
        location: location.to_string(),
        disk: disk.to_string(),
    })
}

fn aws_region(pv: &PersistentVolume, handle: &str) -> Result<String> {
    if let Some(zone) = handle
        .strip_prefix("aws://")
        .and_then(|rest| rest.split('/').next())
    {
        if let Some(region) = region_from_zone(zone) {
            return Ok(region);
        }
    }

    let pv_json = serde_json::to_value(pv)?;
    let terms = pv_json
        .pointer("/spec/nodeAffinity/required/nodeSelectorTerms")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("EBS PV has no node affinity containing its region"))?;
    for term in terms {
        if let Some(expressions) = term
            .get("matchExpressions")
            .and_then(Value::as_array)
        {
            for expression in expressions {
                if expression.get("key").and_then(Value::as_str)
                    == Some("topology.kubernetes.io/region")
                {
                    if let Some(region) = expression
                        .get("values")
                        .and_then(Value::as_array)
                        .and_then(|values| values.first())
                        .and_then(Value::as_str)
                    {
                        return Ok(region.to_string());
                    }
                }
                if expression.get("key").and_then(Value::as_str)
                    == Some("topology.kubernetes.io/zone")
                {
                    if let Some(zone) = expression
                        .get("values")
                        .and_then(Value::as_array)
                        .and_then(|values| values.first())
                        .and_then(Value::as_str)
                    {
                        if let Some(region) = region_from_zone(zone) {
                            return Ok(region);
                        }
                    }
                }
            }
        }
    }
    bail!("cannot determine EBS region; refusing to treat a regional 404 as deletion")
}

fn region_from_zone(zone: &str) -> Option<String> {
    let (region, suffix) = zone.rsplit_once('-')?;
    if suffix.len() == 1 && suffix.as_bytes()[0].is_ascii_lowercase() {
        Some(region.to_string())
    } else {
        None
    }
}

#[derive(Deserialize)]
struct MetadataToken {
    access_token: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct MockCloudApi {
        state: CloudVolumeState,
        seen: Mutex<Vec<VolumeIdentity>>,
    }

    #[async_trait]
    impl CloudStorageApi for MockCloudApi {
        async fn state(&self, identity: &VolumeIdentity) -> Result<CloudVolumeState> {
            self.seen.lock().unwrap().push(identity.clone());
            Ok(self.state)
        }
    }

    fn gcp_pv() -> PersistentVolume {
        serde_json::from_value(serde_json::json!({
            "spec": { "csi": {
                "driver": "pd.csi.storage.gke.io",
                "volumeHandle": "projects/test-project/zones/us-central1-a/disks/validator-data"
            }}
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn retained_gcp_disk_requires_positive_detached_state() {
        let api = MockCloudApi {
            state: CloudVolumeState::Detached,
            seen: Mutex::new(Vec::new()),
        };
        assert!(volume_is_safe_to_finalize(&gcp_pv(), false, &api)
            .await
            .unwrap());
        assert_eq!(api.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn delete_policy_requires_provider_confirmation_of_deletion() {
        let api = MockCloudApi {
            state: CloudVolumeState::Detached,
            seen: Mutex::new(Vec::new()),
        };
        assert!(!volume_is_safe_to_finalize(&gcp_pv(), true, &api)
            .await
            .unwrap());
    }

    #[tokio::test]
    async fn attached_volume_never_allows_finalizer_removal() {
        let api = MockCloudApi {
            state: CloudVolumeState::Attached,
            seen: Mutex::new(Vec::new()),
        };
        assert!(!volume_is_safe_to_finalize(&gcp_pv(), false, &api)
            .await
            .unwrap());
    }

    #[test]
    fn gcp_handle_parses_zonal_and_regional_disks() {
        assert!(matches!(
            parse_gcp_disk_handle("projects/p/regions/us-central1/disks/d").unwrap(),
            VolumeIdentity::GcpPd { scope: GcpDiskScope::Region, .. }
        ));
        assert!(parse_gcp_disk_handle("p/zones/z/disks/d").is_err());
    }
}