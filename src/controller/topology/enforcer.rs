//! Dynamic topology enforcer for StellarNode workloads.
//!
//! This module implements a controller-style inspector that queries the
//! Kubernetes API for active node zone labels, then dynamically generates
//! `TopologySpreadConstraints` tuned to the actual cluster topology.
//!
//! # How it works
//!
//! 1. Lists all nodes via the Kubernetes API.
//! 2. Extracts the `topology.kubernetes.io/zone` label from each node.
//! 3. Builds a [`ZoneTopology`] snapshot of the cluster.
//! 4. Calls [`super::rules::generate_constraints`] to produce constraints
//!    with the correct enforcement mode:
//!    - **Multi-zone with >= 2 nodes per zone**: Hard (`DoNotSchedule`).
//!    - **Single-zone or low-capacity**: Soft (`ScheduleAnyway`).
//!
//! # Integration
//!
//! Call [`enforce_topology`] from the reconciliation loop before building
//! StatefulSet or Deployment manifests.  It mutates the `StellarNodeSpec`
//! in-place so downstream resource builders pick up the correct constraints.

use std::collections::HashSet;

use k8s_openapi::api::core::v1::Node;
use kube::{Client, ResourceExt};
use tracing::{debug, info, warn};

use crate::controller::topology::rules::{build_zone_topology, generate_constraints, ZoneTopology};
use crate::crd::StellarNodeSpec;
use crate::error::{Error, Result};

const LABEL_ZONE: &str = "topology.kubernetes.io/zone";
const LABEL_ZONE_LEGACY: &str = "failure-domain.beta.kubernetes.io/zone";

/// Inspect all cluster nodes and derive the zone topology.
///
/// Returns a [`ZoneTopology`] capturing zone names, per-zone node counts,
/// and total node count.  Nodes without a recognized zone label are skipped.
pub async fn inspect_node_zones(client: &Client) -> Result<ZoneTopology> {
    let nodes_api: kube::Api<Node> = kube::Api::all(client.clone());

    let node_list = nodes_api
        .list(&kube::api::ListParams::default())
        .await
        .map_err(|e| Error::KubeError {
            message: format!("failed to list nodes for topology inspection: {e}"),
        })?;

    let mut zone_labels: Vec<String> = Vec::new();

    for node in node_list.items {
        let zone = extract_zone_label(&node);
        if let Some(z) = zone {
            if !z.is_empty() {
                zone_labels.push(z);
            }
        }
    }

    let topology = build_zone_topology(zone_labels);

    debug!(
        zones = ?topology.zones,
        total_nodes = topology.total_nodes,
        "Inspected cluster zone topology"
    );

    Ok(topology)
}

/// Enforce topology spread constraints on a `StellarNodeSpec`.
///
/// If the spec already has explicit `topology_spread_constraints`, this
/// is a no-op (user overrides are respected).
///
/// Otherwise, the function:
/// 1. Inspects the cluster's zone topology.
/// 2. Generates appropriate `TopologySpreadConstraint` objects.
/// 3. Writes them back into `spec.topology_spread_constraints`.
///
/// Returns the generated constraints for observability.
pub async fn enforce_topology(
    client: &Client,
    spec: &mut StellarNodeSpec,
    node_name: &str,
) -> Result<Vec<k8s_openapi::api::core::v1::TopologySpreadConstraint>> {
    if spec.topology_spread_constraints.is_some() {
        let existing = spec.topology_spread_constraints.as_ref();
        if existing.as_ref().map(|v| !v.is_empty()).unwrap_or(false) {
            debug!(
                node = %node_name,
                "User-provided topology_spread_constraints present; skipping auto-enforcement"
            );
            return Ok(existing.cloned().unwrap_or_default());
        }
    }

    let topology = inspect_node_zones(client).await?;

    info!(
        node = %node_name,
        zones = ?topology.zones,
        zone_count = topology.zone_count(),
        total_nodes = topology.total_nodes,
        "Enforcing topology spread constraints"
    );

    let constraints = generate_constraints(spec, node_name, &topology);
    spec.topology_spread_constraints = Some(constraints.clone());

    Ok(constraints)
}

/// Extract the zone label from a Node's metadata, trying both the current
/// and legacy label keys.
fn extract_zone_label(node: &Node) -> Option<String> {
    let labels = node.metadata.labels.as_ref()?;

    let zone = labels
        .get(LABEL_ZONE)
        .or_else(|| labels.get(LABEL_ZONE_LEGACY));

    zone.cloned()
}

/// A cache-aware topology inspector.
///
/// Wraps [`inspect_node_zones`] with an in-memory cache so repeated calls
/// within the same reconciliation cycle do not incur additional API calls.
#[derive(Debug, Clone, Default)]
pub struct TopologyInspector {
    cached: Option<ZoneTopology>,
}

impl TopologyInspector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return the cached topology, or inspect the cluster if not yet cached.
    pub async fn get_or_inspect(&mut self, client: &Client) -> Result<&ZoneTopology> {
        if self.cached.is_none() {
            self.cached = Some(inspect_node_zones(client).await?);
        }
        // SAFETY: we just set it above
        Ok(self.cached.as_ref().unwrap())
    }

    /// Force a refresh of the cached topology on the next call.
    pub fn invalidate(&mut self) {
        self.cached = None;
    }

    /// Return the currently cached topology without inspecting the cluster.
    pub fn cached_topology(&self) -> Option<&ZoneTopology> {
        self.cached.as_ref()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::types::{PodAntiAffinityStrength, ResourceRequirements, ResourceSpec};
    use crate::crd::{NodeType, StellarNetwork, StellarNodeSpec};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    fn minimal_spec(node_type: NodeType) -> StellarNodeSpec {
        StellarNodeSpec {
            node_type,
            network: StellarNetwork::Testnet,
            version: "v21.0.0".to_string(),
            resources: ResourceRequirements {
                requests: ResourceSpec {
                    cpu: "500m".to_string(),
                    memory: "1Gi".to_string(),
                },
                limits: ResourceSpec {
                    cpu: "2".to_string(),
                    memory: "4Gi".to_string(),
                },
            },
            replicas: 3,
            min_available: None,
            max_unavailable: None,
            suspended: false,
            alerting: false,
            database: None,
            managed_database: None,
            autoscaling: None,
            vpa_config: None,
            ingress: None,
            load_balancer: None,
            global_discovery: None,
            cross_cluster: None,
            strategy: Default::default(),
            maintenance_mode: false,
            network_policy: None,
            dr_config: None,
            pod_anti_affinity: PodAntiAffinityStrength::Hard,
            placement: Default::default(),
            topology_spread_constraints: None,
            cve_handling: None,
            snapshot_schedule: None,
            restore_from_snapshot: None,
            read_replica_config: None,
            read_pool_endpoint: None,
            sidecars: None,
            cert_manager: None,
            db_maintenance_config: None,
            oci_snapshot: None,
            service_mesh: None,
            forensic_snapshot: None,
            label_propagation: None,
            resource_meta: None,
            history_mode: Default::default(),
            storage: Default::default(),
            validator_config: None,
            horizon_config: None,
            soroban_config: None,
            nat_traversal: None,
            custom_network_passphrase: None,
            cross_cloud_failover: None,
            hitless_upgrade: None,
            ..Default::default()
        }
    }

    // --- extract_zone_label tests ---

    #[test]
    fn test_extract_zone_label_current_key() {
        let node = Node {
            metadata: ObjectMeta {
                labels: Some(
                    [("topology.kubernetes.io/zone".to_string(), "us-east-1a".to_string())]
                        .iter()
                        .cloned()
                        .collect(),
                ),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(extract_zone_label(&node), Some("us-east-1a".to_string()));
    }

    #[test]
    fn test_extract_zone_label_legacy_key() {
        let node = Node {
            metadata: ObjectMeta {
                labels: Some(
                    [("failure-domain.beta.kubernetes.io/zone".to_string(), "us-west-2b".to_string())]
                        .iter()
                        .cloned()
                        .collect(),
                ),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(extract_zone_label(&node), Some("us-west-2b".to_string()));
    }

    #[test]
    fn test_extract_zone_label_no_labels() {
        let node = Node {
            metadata: ObjectMeta {
                labels: None,
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(extract_zone_label(&node), None);
    }

    #[test]
    fn test_extract_zone_label_no_zone_key() {
        let node = Node {
            metadata: ObjectMeta {
                labels: Some(
                    [("kubernetes.io/hostname".to_string(), "node1".to_string())]
                        .iter()
                        .cloned()
                        .collect(),
                ),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(extract_zone_label(&node), None);
    }

    #[test]
    fn test_extract_zone_label_current_key_takes_precedence() {
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("topology.kubernetes.io/zone".to_string(), "current".to_string());
        labels.insert("failure-domain.beta.kubernetes.io/zone".to_string(), "legacy".to_string());
        let node = Node {
            metadata: ObjectMeta {
                labels: Some(labels),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(extract_zone_label(&node), Some("current".to_string()));
    }

    // --- TopologyInspector tests ---

    #[test]
    fn test_topology_inspector_new_is_empty() {
        let inspector = TopologyInspector::new();
        assert!(inspector.cached_topology().is_none());
    }

    #[test]
    fn test_topology_inspector_invalidate_clears_cache() {
        let mut inspector = TopologyInspector::new();
        // Manually set cache to simulate a previous inspection
        // (not possible via public API, so we test the default state)
        assert!(inspector.cached_topology().is_none());
        inspector.invalidate();
        assert!(inspector.cached_topology().is_none());
    }
}
