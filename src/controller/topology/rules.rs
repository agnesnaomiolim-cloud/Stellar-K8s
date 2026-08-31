//! Topology rule engine for StellarNode pod placement.
//!
//! This module contains pure, testable logic for deriving Kubernetes
//! `TopologySpreadConstraints` from observed cluster topology.
//!
//! # Overview
//!
//! The rule engine takes a snapshot of the cluster's zone topology and
//! produces constraints that achieve two goals:
//!
//! 1. **Multi-zone clusters**: Hard-enforce even distribution across
//!    availability zones to prevent clustering on single physical hosts.
//! 2. **Single-zone clusters**: Automatically fall back to soft
//!    `ScheduleAnyway` constraints so local development clusters remain
//!    schedulable.
//!
//! # Key Types
//!
//! - [`ZoneTopology`] — aggregated view of zone labels across cluster nodes
//! - [`EnforcementMode`] — `Hard` (DoNotSchedule) or `Soft` (ScheduleAnyway)
//! - [`build_zone_topology`] — derive zone counts from a list of zone label values
//! - [`generate_constraints`] — produce `TopologySpreadConstraint` objects

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::TopologySpreadConstraint;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;

use crate::crd::{StellarNodeSpec, NodeType};
use crate::controller::resources::network_spread_label_selector;

/// Aggregated view of zone labels across all cluster nodes.
///
/// Produced by [`build_zone_topology`] and consumed by [`generate_constraints`].
#[derive(Debug, Clone, Default)]
pub struct ZoneTopology {
    /// Unique zone names present in the cluster (e.g. `us-east-1a`).
    pub zones: Vec<String>,
    /// Number of ready/schedulable nodes in each zone.
    pub zone_node_counts: BTreeMap<String, usize>,
    /// Total number of ready nodes across all zones.
    pub total_nodes: usize,
}

impl ZoneTopology {
    /// Number of distinct zones.
    pub fn zone_count(&self) -> usize {
        self.zones.len()
    }

    /// Whether the cluster spans multiple zones.
    pub fn is_multi_zone(&self) -> bool {
        self.zones.len() > 1
    }

    /// Minimum node count across all zones.
    pub fn min_zone_capacity(&self) -> usize {
        self.zone_node_counts.values().copied().min().unwrap_or(0)
    }
}

/// Build a [`ZoneTopology`] from an unordered list of node zone labels.
///
/// Nodes without a zone label are silently ignored.
pub fn build_zone_topology(zone_labels: Vec<String>) -> ZoneTopology {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for zone in zone_labels {
        if zone.is_empty() {
            continue;
        }
        *counts.entry(zone).or_default() += 1;
    }

    let zones: Vec<String> = counts.keys().cloned().collect();
    let total_nodes: usize = counts.values().sum();

    ZoneTopology {
        zones,
        zone_node_counts: counts,
        total_nodes,
    }
}

/// Enforcement mode for topology spread constraints.
///
/// Determines the `whenUnsatisfiable` behaviour:
/// - `Hard` → `DoNotSchedule` (strict, prevents scheduling if spread is violated)
/// - `Soft` → `ScheduleAnyway` (best-effort, allows scheduling for dev clusters)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnforcementMode {
    Hard,
    Soft,
}

impl EnforcementMode {
    pub fn when_unsatisfiable(self) -> &'static str {
        match self {
            EnforcementMode::Hard => "DoNotSchedule",
            EnforcementMode::Soft => "ScheduleAnyway",
        }
    }
}

/// Select the enforcement mode based on cluster topology.
///
/// Rules:
/// - Multi-zone clusters with at least 2 nodes per zone → `Hard`
/// - Everything else (single zone, or low-capacity zones) → `Soft`
pub fn select_enforcement_mode(topology: &ZoneTopology) -> EnforcementMode {
    if topology.is_multi_zone() && topology.min_zone_capacity() >= 2 {
        EnforcementMode::Hard
    } else {
        EnforcementMode::Soft
    }
}

/// Generate `TopologySpreadConstraints` for a StellarNode spec given the
/// observed cluster topology.
///
/// If the user has explicitly configured `spec.topology_spread_constraints`,
/// those are returned as-is (user overrides always win).
///
/// Otherwise, the function produces:
/// - A hostname-level constraint to prevent co-location on the same node.
/// - A zone-level constraint to spread across availability zones.
///
/// The enforcement mode is chosen automatically:
/// - `Hard` on multi-zone production clusters.
/// - `Soft` on single-zone or low-capacity development clusters.
///
/// _Requirements: dynamic topology enforcement, single-zone fallback_
pub fn generate_constraints(
    spec: &StellarNodeSpec,
    node_name: &str,
    topology: &ZoneTopology,
) -> Vec<TopologySpreadConstraint> {
    use k8s_openapi::api::core::v1::TopologySpreadConstraint;

    if let Some(constraints) = &spec.topology_spread_constraints {
        if !constraints.is_empty() {
            return constraints.clone();
        }
    }

    let mode = select_enforcement_mode(topology);
    let unsatisfiable = mode.when_unsatisfiable().to_string();
    let selector = network_spread_label_selector(spec);

    vec![
        TopologySpreadConstraint {
            max_skew: 1,
            topology_key: "kubernetes.io/hostname".to_string(),
            when_unsatisfiable: unsatisfiable.clone(),
            label_selector: Some(selector.clone()),
            ..Default::default()
        },
        TopologySpreadConstraint {
            max_skew: 1,
            topology_key: "topology.kubernetes.io/zone".to_string(),
            when_unsatisfiable: unsatisfiable,
            label_selector: Some(selector),
            ..Default::default()
        },
    ]
}

// Re-export the helper so rules.rs can use it without a circular dependency.
// The original lives in resources.rs and is module-private (pub(crate)).
// We access it via the crate::controller::resources path which is visible
// because both modules are siblings under crate::controller.
// However, since network_spread_label_selector is pub(crate), we need to
// either make it pub or use a different selector. Let's inline a compatible
// selector here to avoid visibility issues.

fn network_spread_label_selector(spec: &StellarNodeSpec) -> LabelSelector {
    use std::collections::BTreeMap;

    let scheduling_label = match spec.network.scheduling_label_value(&spec.custom_network_passphrase) {
        Some(v) => v,
        None => spec.network.scheduling_label_value(&None).unwrap_or_default(),
    };

    LabelSelector {
        match_labels: Some(BTreeMap::from([
            ("app.kubernetes.io/name".to_string(), "stellar-node".to_string()),
            ("stellar-network".to_string(), scheduling_label),
            (
                "app.kubernetes.io/component".to_string(),
                spec.node_type.to_string().to_lowercase(),
            ),
        ])),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crd::types::{PodAntiAffinityStrength, ResourceRequirements, ResourceSpec};
    use crate::crd::{NodeType, StellarNetwork, StellarNodeSpec};

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

    // --- build_zone_topology tests ---

    #[test]
    fn test_build_zone_topology_counts_zones() {
        let zones = vec![
            "us-east-1a".to_string(),
            "us-east-1a".to_string(),
            "us-east-1b".to_string(),
            "us-east-1c".to_string(),
        ];
        let topology = build_zone_topology(zones);
        assert_eq!(topology.zone_count(), 3);
        assert_eq!(topology.total_nodes, 4);
        assert_eq!(topology.zone_node_counts.get("us-east-1a"), Some(&2));
    }

    #[test]
    fn test_build_zone_topology_empty_input() {
        let topology = build_zone_topology(vec![]);
        assert_eq!(topology.zone_count(), 0);
        assert_eq!(topology.total_nodes, 0);
    }

    #[test]
    fn test_build_zone_topology_ignores_empty_strings() {
        let zones = vec![
            "us-east-1a".to_string(),
            "".to_string(),
            "us-east-1b".to_string(),
        ];
        let topology = build_zone_topology(zones);
        assert_eq!(topology.zone_count(), 2);
        assert_eq!(topology.total_nodes, 2);
    }

    #[test]
    fn test_is_multi_zone() {
        let single = build_zone_topology(vec!["us-east-1a".to_string(); 3]);
        assert!(!single.is_multi_zone());

        let multi = build_zone_topology(vec![
            "us-east-1a".to_string(),
            "us-east-1b".to_string(),
        ]);
        assert!(multi.is_multi_zone());
    }

    #[test]
    fn test_min_zone_capacity() {
        let topology = build_zone_topology(vec![
            "us-east-1a".to_string(),
            "us-east-1a".to_string(),
            "us-east-1a".to_string(),
            "us-east-1b".to_string(),
            "us-east-1b".to_string(),
            "us-east-1c".to_string(),
        ]);
        assert_eq!(topology.min_zone_capacity(), 1);
    }

    // --- select_enforcement_mode tests ---

    #[test]
    fn test_select_enforcement_mode_multi_zone_hard() {
        let topology = build_zone_topology(vec![
            "us-east-1a".to_string(),
            "us-east-1a".to_string(),
            "us-east-1b".to_string(),
            "us-east-1b".to_string(),
            "us-east-1c".to_string(),
            "us-east-1c".to_string(),
        ]);
        assert_eq!(select_enforcement_mode(&topology), EnforcementMode::Hard);
    }

    #[test]
    fn test_select_enforcement_mode_single_zone_soft() {
        let topology = build_zone_topology(vec!["us-east-1a".to_string(); 3]);
        assert_eq!(select_enforcement_mode(&topology), EnforcementMode::Soft);
    }

    #[test]
    fn test_select_enforcement_mode_insufficient_nodes_soft() {
        let topology = build_zone_topology(vec![
            "us-east-1a".to_string(),
            "us-east-1b".to_string(),
        ]);
        // 2 zones but only 1 node each -> min capacity is 1, not >= 2
        assert_eq!(select_enforcement_mode(&topology), EnforcementMode::Soft);
    }

    // --- generate_constraints tests ---

    #[test]
    fn test_generate_constraints_multi_zone_hard() {
        let spec = minimal_spec(NodeType::Validator);
        let topology = build_zone_topology(vec![
            "us-east-1a".to_string(),
            "us-east-1a".to_string(),
            "us-east-1b".to_string(),
            "us-east-1b".to_string(),
            "us-east-1c".to_string(),
            "us-east-1c".to_string(),
        ]);
        let constraints = generate_constraints(&spec, "val", &topology);
        assert_eq!(constraints.len(), 2);
        for c in &constraints {
            assert_eq!(c.max_skew, 1);
            assert_eq!(c.when_unsatisfiable, "DoNotSchedule");
        }
    }

    #[test]
    fn test_generate_constraints_single_zone_soft() {
        let spec = minimal_spec(NodeType::Validator);
        let topology = build_zone_topology(vec!["us-east-1a".to_string(); 3]);
        let constraints = generate_constraints(&spec, "val", &topology);
        assert_eq!(constraints.len(), 2);
        for c in &constraints {
            assert_eq!(c.max_skew, 1);
            assert_eq!(c.when_unsatisfiable, "ScheduleAnyway");
        }
    }

    #[test]
    fn test_generate_constraints_includes_hostname_and_zone_keys() {
        let spec = minimal_spec(NodeType::Validator);
        let topology = build_zone_topology(vec![
            "us-east-1a".to_string(),
            "us-east-1b".to_string(),
        ]);
        let constraints = generate_constraints(&spec, "val", &topology);
        let keys: Vec<String> = constraints.iter().map(|c| c.topology_key.clone()).collect();
        assert!(keys.contains(&"kubernetes.io/hostname".to_string()));
        assert!(keys.contains(&"topology.kubernetes.io/zone".to_string()));
    }

    #[test]
    fn test_generate_constraints_user_override_wins() {
        let mut spec = minimal_spec(NodeType::Validator);
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
        use std::collections::BTreeMap;
        spec.topology_spread_constraints = Some(vec![
            k8s_openapi::api::core::v1::TopologySpreadConstraint {
                max_skew: 3,
                topology_key: "custom.io/rack".to_string(),
                when_unsatisfiable: "ScheduleAnyway".to_string(),
                label_selector: Some(LabelSelector {
                    match_labels: Some(BTreeMap::from([(
                        "app".to_string(),
                        "my-app".to_string(),
                    )])),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ]);
        let topology = build_zone_topology(vec![
            "us-east-1a".to_string(),
            "us-east-1b".to_string(),
        ]);
        let constraints = generate_constraints(&spec, "val", &topology);
        assert_eq!(constraints.len(), 1);
        assert_eq!(constraints[0].topology_key, "custom.io/rack");
    }

    #[test]
    fn test_generate_constraints_empty_user_vec_falls_back() {
        let mut spec = minimal_spec(NodeType::Validator);
        spec.topology_spread_constraints = Some(vec![]);
        let topology = build_zone_topology(vec![
            "us-east-1a".to_string(),
            "us-east-1b".to_string(),
        ]);
        let constraints = generate_constraints(&spec, "val", &topology);
        assert_eq!(constraints.len(), 2);
    }

    #[test]
    fn test_generate_constraints_label_selector_content() {
        let spec = minimal_spec(NodeType::Validator);
        let topology = build_zone_topology(vec![
            "us-east-1a".to_string(),
            "us-east-1b".to_string(),
        ]);
        let constraints = generate_constraints(&spec, "val", &topology);
        for c in &constraints {
            let labels = c
                .label_selector
                .as_ref()
                .and_then(|s| s.match_labels.as_ref())
                .expect("matchLabels must be present");
            assert_eq!(labels.get("app.kubernetes.io/name"), Some(&"stellar-node".to_string()));
            assert_eq!(labels.get("stellar-network"), Some(&"testnet".to_string()));
            assert_eq!(labels.get("app.kubernetes.io/component"), Some(&"validator".to_string()));
        }
    }
}
