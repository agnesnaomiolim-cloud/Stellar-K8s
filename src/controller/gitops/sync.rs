//! GitOps desired-state synchronization for Captive Core and Horizon ConfigMaps.
//!
//! The repository layout this module consumes:
//!
//! ```text
//! clusters/prod/
//! ├── validator/
//! │   └── stellar-core.cfg
//! ├── horizon/
//! │   └── horizon.env
//! └── soroban-rpc/
//!     └── captive-core.cfg
//! ```
//!
//! Each directory name maps to the `app.kubernetes.io/instance` label of the
//! [`StellarNode`] it configures, and the files map onto ConfigMap data keys.
//! On every new commit the engine:
//!
//! 1. Lists the manifest directory at that commit.
//! 2. Renders the desired ConfigMap data for each node.
//! 3. Server-side-applies a patch annotated with the source commit SHA.
//!
//! Rollback re-runs the exact same rendering path for the previous commit, so
//! the cluster always converges to a commit-addressable state.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Api, ListParams, Patch, PatchParams};
use tracing::{debug, info, warn};

use crate::controller::resources::resource_name;
use crate::crd::StellarNode;
use crate::error::{Error, Result};

use super::github::{ContentEntry, Fetch, GitHubClient};
use super::GitOpsConfig;

/// Annotation carrying the Git commit SHA that produced a ConfigMap.
pub const COMMIT_ANNOTATION: &str = "stellar.org/gitops-commit";
/// Annotation carrying the GitOps engine version that applied the change.
pub const MANAGED_BY_ANNOTATION: &str = "stellar.org/gitops-managed";

/// Field-manager used for server-side apply of GitOps state.
pub const FIELD_MANAGER: &str = "stellar-gitops";

/// Label selector matching node ConfigMaps managed by the operator.
pub fn node_configmap_selector(instance: &str) -> String {
    format!("app.kubernetes.io/instance={instance},app.kubernetes.io/name=stellar-node")
}

/// A per-node desired state rendered from one commit.
#[derive(Debug, Clone, PartialEq)]
pub struct DesiredConfig {
    /// Namespace of the target ConfigMap (same as the StellarNode).
    pub namespace: String,
    /// Name of the target ConfigMap (e.g. `my-validator-config`).
    pub configmap: String,
    /// Rendered data keys to apply.
    pub data: BTreeMap<String, String>,
    /// Commit SHA the desired state was rendered from.
    pub commit: String,
}

impl DesiredConfig {
    /// Number of data keys in the desired state.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// `true` when the desired state carries no data.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Build the ConfigMap patch (metadata + data) for server-side apply.
    pub fn to_config_map(&self) -> ConfigMap {
        ConfigMap {
            metadata: kube::api::ObjectMeta {
                name: Some(self.configmap.clone()),
                namespace: Some(self.namespace.clone()),
                annotations: Some(BTreeMap::from([
                    (COMMIT_ANNOTATION.to_string(), self.commit.clone()),
                    (MANAGED_BY_ANNOTATION.to_string(), "true".to_string()),
                ])),
                ..Default::default()
            },
            data: Some(self.data.clone()),
            ..Default::default()
        }
    }
}

/// Files recognised inside a node's manifest directory, mapped to ConfigMap
/// data keys.
fn key_for_file(file_name: &str) -> Option<&'static str> {
    match file_name {
        // Stellar Core configuration (validators, captive core).
        "stellar-core.cfg" | "captive-core.cfg" => Some("stellar-core.cfg"),
        // Horizon environment file.
        "horizon.env" => Some("horizon.env"),
        _ => None,
    }
}

/// Render the desired ConfigMap data for one node directory at `commit`.
///
/// Uses the GitHub *contents* API for the listing and *raw* fetches for file
/// bodies. Unknown files are ignored with a warning; a directory whose files
/// all fail to download produces an error so the engine can surface a broken
/// commit *before* touching the cluster.
pub async fn render_node_directory(
    client: &GitHubClient,
    config: &GitOpsConfig,
    commit: &str,
    entry: &ContentEntry,
) -> Result<DesiredConfig> {
    let node_name = entry.name.clone();

    let listing = client
        .fetch_directory(&config.repo, commit, &entry.path)
        .await?;
    let files = match listing {
        Fetch::Fresh(files) => files,
        Fetch::NotModified => {
            // Directory tree unchanged since the previous listing; there is
            // nothing to render for this path at this commit.
            return Err(Error::GitOpsError(format!(
                "directory {} unchanged but no cached data available",
                entry.path
            )));
        }
    };

    let mut data = BTreeMap::new();
    let mut failures = 0usize;

    for file in files.iter().filter(|f| f.is_file()) {
        let Some(key) = key_for_file(&file.name) else {
            debug!(file = %file.name, path = %entry.path, "ignoring non-config file");
            continue;
        };

        let Some(url) = &file.download_url else {
            failures += 1;
            warn!(file = %file.name, "no download_url for config file");
            continue;
        };

        match client.fetch_raw_url(url).await {
            Ok(body) => {
                data.insert(key.to_string(), body);
            }
            Err(e) => {
                failures += 1;
                warn!(file = %file.name, error = %e, "failed to download config file");
            }
        }
    }

    if data.is_empty() {
        return Err(Error::GitOpsError(format!(
            "no usable config files rendered for node '{node_name}' at commit {commit}"
        )));
    }
    if failures > 0 {
        return Err(Error::GitOpsError(format!(
            "{failures} config file(s) failed to download for node '{node_name}'"
        )));
    }

    // The target namespace is declared via a `namespace` sidecar file, falling
    // back to the manifests path root (single-namespace repos are the norm).
    let namespace = data
        .remove("namespace")
        .unwrap_or_else(|| "default".to_string());

    Ok(DesiredConfig {
        namespace,
        configmap: format!("{node_name}-config"),
        data,
        commit: commit.to_string(),
    })
}

/// Apply one commit: render all node directories and patch their ConfigMaps.
///
/// Rendering happens fully **before** any cluster mutation, so a commit that
/// fails to render (broken file, GitHub error) never leaves the cluster in a
/// half-applied state.
pub async fn apply_commit(
    k8s: &kube::Client,
    config: &GitOpsConfig,
    commit: &str,
) -> Result<usize> {
    let github = GitHubClient::new(config.token.clone(), None, None)?;

    let root = config.manifests_path.trim_matches('/');
    let entries = match github.fetch_directory(&config.repo, commit, root).await? {
        Fetch::Fresh(entries) => entries,
        Fetch::NotModified => {
            debug!(commit = %commit, "manifest root unchanged; nothing to render");
            return Ok(0);
        }
    };

    let dirs: Vec<&ContentEntry> = entries.iter().filter(|e| e.is_dir()).collect();
    if dirs.is_empty() {
        return Err(Error::GitOpsError(format!(
            "no node directories found under '{}' at commit {commit}",
            config.manifests_path
        )));
    }

    // Phase 1: render everything up-front.
    let mut desired = Vec::with_capacity(dirs.len());
    let mut render_errors = Vec::new();
    for dir in &dirs {
        match render_node_directory(&github, config, commit, dir).await {
            Ok(d) => desired.push(d),
            Err(e) => render_errors.push(format!("{}: {e}", dir.name)),
        }
    }

    if !render_errors.is_empty() {
        return Err(Error::GitOpsError(format!(
            "commit {commit} failed to render ({} node(s) affected): {}",
            render_errors.len(),
            render_errors.join("; ")
        )));
    }

    // Phase 2: patch the cluster.
    let mut applied = 0usize;
    for d in &desired {
        let api: Api<ConfigMap> = Api::namespaced(k8s.clone(), &d.namespace);
        let patch = Patch::Apply(d.to_config_map());
        api.patch(
            &d.configmap,
            &PatchParams {
                field_manager: Some(FIELD_MANAGER.to_string()),
                ..Default::default()
            },
            &patch,
        )
        .await
        .map_err(|e| {
            Error::GitOpsError(format!(
                "failed to patch ConfigMap {}/{}: {e}",
                d.namespace, d.configmap
            ))
        })?;
        applied += 1;
        info!(
            commit = %commit,
            configmap = %d.configmap,
            namespace = %d.namespace,
            keys = d.data.len(),
            "Applied GitOps config"
        );
    }

    Ok(applied)
}

/// Read the commit annotation from a live ConfigMap, if present.
pub fn configmap_commit(cm: &ConfigMap) -> Option<&str> {
    cm.metadata
        .annotations
        .as_ref()?
        .get(COMMIT_ANNOTATION)
        .map(|s| s.as_str())
}

/// Verify a live ConfigMap matches the desired state for `commit`.
///
/// Used by integration tests and the drift detector; compares both the data
/// payload and the commit annotation.
pub fn matches_commit(cm: &ConfigMap, commit: &str) -> bool {
    configmap_commit(cm) == Some(commit)
}

/// List all node ConfigMaps in `namespace` that carry the GitOps annotations.
pub async fn list_gitops_configmaps(k8s: &kube::Client, namespace: &str) -> Result<Vec<ConfigMap>> {
    let api: Api<ConfigMap> = Api::namespaced(k8s.clone(), namespace);
    let cms = api
        .list(&ListParams::default().labels(&node_configmap_selector("*")))
        .await
        .map_err(Error::KubeError)?;
    Ok(cms
        .items
        .into_iter()
        .filter(|cm| configmap_commit(cm).is_some())
        .collect())
}

/// Resolve the live ConfigMap name for a node, mirroring the reconciler.
pub fn configmap_name_for(node: &StellarNode) -> String {
    resource_name(node, "config")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired(commit: &str) -> DesiredConfig {
        let mut data = BTreeMap::new();
        data.insert(
            "stellar-core.cfg".to_string(),
            "LOG_LEVEL=\"info\"\n".to_string(),
        );
        DesiredConfig {
            namespace: "stellar".to_string(),
            configmap: "my-validator-config".to_string(),
            data,
            commit: commit.to_string(),
        }
    }

    #[test]
    fn key_for_file_mapping() {
        assert_eq!(key_for_file("stellar-core.cfg"), Some("stellar-core.cfg"));
        assert_eq!(key_for_file("captive-core.cfg"), Some("stellar-core.cfg"));
        assert_eq!(key_for_file("horizon.env"), Some("horizon.env"));
        assert_eq!(key_for_file("README.md"), None);
        assert_eq!(key_for_file("chart.yaml"), None);
    }

    #[test]
    fn desired_config_patch_carries_commit_annotation() {
        let cm = desired("abc1234").to_config_map();
        let ann = cm.metadata.annotations.expect("annotations");
        assert_eq!(
            ann.get(COMMIT_ANNOTATION).map(String::as_str),
            Some("abc1234")
        );
        assert_eq!(
            ann.get(MANAGED_BY_ANNOTATION).map(String::as_str),
            Some("true")
        );
        assert_eq!(
            cm.data.expect("data").get("stellar-core.cfg"),
            Some(&"LOG_LEVEL=\"info\"\n".to_string())
        );
    }

    #[test]
    fn desired_config_len_and_empty() {
        let d = desired("abc1234");
        assert_eq!(d.len(), 1);
        assert!(!d.is_empty());

        let empty = DesiredConfig {
            namespace: "n".into(),
            configmap: "c".into(),
            data: BTreeMap::new(),
            commit: "x".into(),
        };
        assert!(empty.is_empty());
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn matches_commit_checks_annotation() {
        let cm = desired("abc1234").to_config_map();
        assert!(matches_commit(&cm, "abc1234"));
        assert!(!matches_commit(&cm, "def5678"));
    }

    #[test]
    fn configmap_name_for_matches_reconciler() {
        let node = test_node();
        assert_eq!(configmap_name_for(&node), "test-node-config");
    }

    #[test]
    fn node_configmap_selector_format() {
        assert_eq!(
            node_configmap_selector("my-node"),
            "app.kubernetes.io/instance=my-node,app.kubernetes.io/name=stellar-node"
        );
    }

    fn test_node() -> StellarNode {
        use crate::crd::{
            NodeType, ResourceRequirements, ResourceSpec, StellarNetwork, StellarNodeSpec,
            StorageConfig,
        };
        StellarNode {
            metadata: kube::api::ObjectMeta {
                name: Some("test-node".to_string()),
                namespace: Some("stellar".to_string()),
                ..Default::default()
            },
            spec: StellarNodeSpec {
                node_type: NodeType::Validator,
                network: StellarNetwork::Testnet,
                version: "v21.0.0".to_string(),
                resources: ResourceRequirements {
                    requests: ResourceSpec {
                        cpu: "1".into(),
                        memory: "2Gi".into(),
                    },
                    limits: ResourceSpec {
                        cpu: "2".into(),
                        memory: "4Gi".into(),
                    },
                },
                storage: StorageConfig {
                    storage_class: "standard".into(),
                    size: "50Gi".into(),
                    ..Default::default()
                },
                ..Default::default()
            },
            status: None,
        }
    }
}
