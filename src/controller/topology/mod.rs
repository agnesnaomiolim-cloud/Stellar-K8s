//! Dynamic topology enforcement for StellarNode workloads.
//!
//! This module provides a controller-style inspector that queries the cluster
//! for actual node zone labels and dynamically generates `TopologySpreadConstraints`
//! for StatefulSet (and Deployment) manifests.
//!
//! # Sub-modules
//!
//! - [`rules`] — pure logic for constraint generation from zone topology
//! - [`enforcer`] — cluster-aware inspector that lists nodes and applies rules

pub mod enforcer;
pub mod rules;

pub use enforcer::{enforce_topology, inspect_node_zones, TopologyInspector};
pub use rules::{build_zone_topology, generate_constraints, EnforcementMode, ZoneTopology};
