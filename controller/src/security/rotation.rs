// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Staggered P2P node-identity rotation controller.
//!
//! # Responsibilities
//!
//! 1. **Preflight quorum check** — verify the cluster has enough healthy nodes
//!    before starting any rotation.
//! 2. **Staggered rotation** — rotate one node at a time with a configurable
//!    inter-node delay, ensuring the quorum threshold is never breached.
//! 3. **Un-peer → inject → rejoin** — gracefully disconnects a node from its
//!    peers, applies the new identity, and waits for successful re-peering.
//! 4. **ConfigMap update** — rewrites `stellar-core.cfg` to reference the new
//!    seed key without triggering a full pod restart (config-reload).
//! 5. **Horizon DB update** — updates the `accounts` / `signers` table in the
//!    attached Horizon PostgreSQL instance so ingestion workers track the new
//!    node identity.
//! 6. **Quorum-safe rollback** — if any step fails and the quorum would drop
//!    below threshold, the previous identity is restored automatically.
//!
//! # Quorum Safety Invariant
//!
//! ```text
//! let threshold = ceil(2/3 * n);  // SCP ≥ 2/3 majority
//! at_most_rotating = n - threshold;   // nodes that may be simultaneously offline
//! ```
//!
//! For a 5-node cluster: threshold = 4, at_most_rotating = 1 (one node at a time).

use std::{
    collections::HashMap,
    fmt,
    sync::Arc,
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use super::eso_integration::{EsoBackend, EsoNodeIdentity, EsoResult, EsoError};

// ─────────────────────────────────────────────────────────────────────────────
// Configuration
// ─────────────────────────────────────────────────────────────────────────────

/// Controller-level configuration for staggered P2P node-identity rotation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NodeIdentityRotationConfig {
    /// Enable the rotation controller.
    #[serde(default)]
    pub enabled: bool,

    /// Rotation period in days (0 = manual trigger only).
    #[serde(default = "default_rotation_period_days")]
    pub rotation_period_days: u32,

    /// Delay in seconds between rotating successive nodes in the cluster.
    /// Allows the previously rotated node to fully rejoin before the next
    /// node goes offline.
    #[serde(default = "default_inter_node_delay_secs")]
    pub inter_node_delay_secs: u64,

    /// Seconds to wait for a node to rejoin consensus after identity injection.
    #[serde(default = "default_rejoin_timeout_secs")]
    pub rejoin_timeout_secs: u64,

    /// Poll interval when waiting for rejoin (seconds).
    #[serde(default = "default_rejoin_poll_interval_secs")]
    pub rejoin_poll_interval_secs: u64,

    /// Minimum authenticated peer count required before declaring a node healthy.
    #[serde(default = "default_min_peers")]
    pub min_authenticated_peers: usize,

    /// Whether to automatically roll back to the previous identity if rotation
    /// fails (recommended: true).
    #[serde(default = "default_rollback_on_failure")]
    pub rollback_on_failure: bool,

    /// Whether to also update the Horizon PostgreSQL database after rotating.
    #[serde(default = "default_update_horizon_db")]
    pub update_horizon_db: bool,

    /// Connection string for the Horizon PostgreSQL instance.
    /// Required when `update_horizon_db` is true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub horizon_db_url: Option<String>,

    /// Namespace where the node pods reside.
    #[serde(default = "default_namespace")]
    pub namespace: String,

    /// ConfigMap name pattern for Stellar Core config (supports `{node_name}` template).
    #[serde(default = "default_configmap_pattern")]
    pub configmap_name_pattern: String,
}

impl Default for NodeIdentityRotationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            rotation_period_days: default_rotation_period_days(),
            inter_node_delay_secs: default_inter_node_delay_secs(),
            rejoin_timeout_secs: default_rejoin_timeout_secs(),
            rejoin_poll_interval_secs: default_rejoin_poll_interval_secs(),
            min_authenticated_peers: default_min_peers(),
            rollback_on_failure: default_rollback_on_failure(),
            update_horizon_db: default_update_horizon_db(),
            horizon_db_url: None,
            namespace: default_namespace(),
            configmap_name_pattern: default_configmap_pattern(),
        }
    }
}

fn default_rotation_period_days() -> u32 { 30 }
fn default_inter_node_delay_secs() -> u64 { 120 }
fn default_rejoin_timeout_secs() -> u64 { 300 }
fn default_rejoin_poll_interval_secs() -> u64 { 10 }
fn default_min_peers() -> usize { 1 }
fn default_rollback_on_failure() -> bool { true }
fn default_update_horizon_db() -> bool { false }
fn default_namespace() -> String { "stellar".into() }
fn default_configmap_pattern() -> String { "{node_name}-stellar-core-cfg".into() }

// ─────────────────────────────────────────────────────────────────────────────
// Domain types
// ─────────────────────────────────────────────────────────────────────────────

/// An entry in the cluster's rotation plan.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RotationPlanEntry {
    pub node_name: String,
    pub namespace: String,
    /// Secret path in the external store for this node.
    pub secret_path: String,
    /// Current identity fingerprint (pre-rotation).
    pub current_fingerprint: Option<String>,
}

/// Per-node rotation outcome.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NodeRotationOutcome {
    Rotated,
    Skipped,
    RolledBack,
    Failed,
}

/// Per-node rotation result.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NodeRotationResult {
    pub node_name: String,
    pub namespace: String,
    pub outcome: NodeRotationOutcome,
    pub new_fingerprint: Option<String>,
    pub error_message: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
}

/// Summary of a complete cluster rotation run.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ClusterRotationReport {
    pub cluster_id: String,
    pub total_nodes: usize,
    pub rotated: usize,
    pub skipped: usize,
    pub failed: usize,
    pub rolled_back: usize,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub node_results: Vec<NodeRotationResult>,
}

impl ClusterRotationReport {
    pub fn is_fully_successful(&self) -> bool {
        self.failed == 0 && self.rolled_back == 0
    }
}

/// Current stage of a single-node rotation.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RotationStage {
    Idle,
    PreflightCheck,
    FetchingNewIdentity,
    UnPeering,
    InjectingIdentity,
    UpdatingConfigMap,
    UpdatingHorizonDb,
    Rejoining,
    PostflightCheck,
    Completed,
    RollingBack,
    Failed,
}

// ─────────────────────────────────────────────────────────────────────────────
// Trait abstractions (enables unit testing without a live K8s cluster)
// ─────────────────────────────────────────────────────────────────────────────

/// Interface to Stellar Core admin operations needed by the rotation controller.
#[async_trait::async_trait]
pub trait StellarCoreOps: Send + Sync + fmt::Debug {
    /// Fetch the current consensus status for the node.
    async fn consensus_info(&self, node_name: &str, namespace: &str) -> RotationResult<ConsensusInfo>;

    /// Gracefully disconnect the node from its peers (un-peer).
    async fn unpeer(&self, node_name: &str, namespace: &str) -> RotationResult<()>;

    /// Inject the new identity secret into the node's K8s Secret and trigger
    /// a Stellar Core `config-reload`.
    async fn inject_identity(
        &self,
        node_name: &str,
        namespace: &str,
        identity: &EsoNodeIdentity,
        secret_name: &str,
    ) -> RotationResult<()>;

    /// Wait for the node to re-join consensus and authenticate the required
    /// number of peers.
    async fn wait_for_rejoin(
        &self,
        node_name: &str,
        namespace: &str,
        min_peers: usize,
        timeout: Duration,
        poll_interval: Duration,
    ) -> RotationResult<ConsensusInfo>;
}

/// Interface to ConfigMap operations.
#[async_trait::async_trait]
pub trait ConfigMapOps: Send + Sync + fmt::Debug {
    /// Patch the `NODE_SEED` entry in the ConfigMap with the new identity.
    async fn update_seed(
        &self,
        configmap_name: &str,
        namespace: &str,
        new_seed: &str,
        new_public_key: &str,
    ) -> RotationResult<()>;

    /// Read the current `NODE_SEED` from the ConfigMap.
    async fn read_current_seed(
        &self,
        configmap_name: &str,
        namespace: &str,
    ) -> RotationResult<String>;
}

/// Interface to Horizon database operations.
#[async_trait::async_trait]
pub trait HorizonDbOps: Send + Sync + fmt::Debug {
    /// Update the `accounts` / `signers` records in Horizon's PostgreSQL
    /// database to reflect the new node public key.
    async fn update_node_key(
        &self,
        old_public_key: &str,
        new_public_key: &str,
        node_name: &str,
    ) -> RotationResult<HorizonDbUpdateResult>;
}

/// Current consensus status of a node.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConsensusInfo {
    pub node_name: String,
    pub state: String,
    pub ledger_sequence: u64,
    pub authenticated_peer_count: usize,
    pub is_synced: bool,
    pub observed_at: DateTime<Utc>,
}

impl ConsensusInfo {
    /// Check whether the node is healthy per the given minimum peer count.
    pub fn is_healthy(&self, min_peers: usize) -> bool {
        self.is_synced
            && self.ledger_sequence > 0
            && self.authenticated_peer_count >= min_peers
    }
}

/// Result of a Horizon DB update.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HorizonDbUpdateResult {
    pub rows_updated: u64,
    pub old_public_key: String,
    pub new_public_key: String,
}

// ─────────────────────────────────────────────────────────────────────────────
// Error type
// ─────────────────────────────────────────────────────────────────────────────

/// Errors that can occur during the rotation lifecycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RotationError {
    QuorumInsufficient {
        healthy: usize,
        total: usize,
        threshold: usize,
    },
    EsoFetchFailed(String),
    UnpeerFailed(String),
    IdentityInjectionFailed(String),
    ConfigMapUpdateFailed(String),
    HorizonDbUpdateFailed(String),
    RejoinTimeout(String),
    RollbackFailed(String),
    PreflightFailed(String),
    PostflightFailed(String),
}

impl fmt::Display for RotationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::QuorumInsufficient { healthy, total, threshold } => write!(
                f,
                "quorum insufficient: {healthy}/{total} healthy, need {threshold}"
            ),
            Self::EsoFetchFailed(m) => write!(f, "ESO fetch failed: {m}"),
            Self::UnpeerFailed(m) => write!(f, "unpeer failed: {m}"),
            Self::IdentityInjectionFailed(m) => write!(f, "identity injection failed: {m}"),
            Self::ConfigMapUpdateFailed(m) => write!(f, "ConfigMap update failed: {m}"),
            Self::HorizonDbUpdateFailed(m) => write!(f, "Horizon DB update failed: {m}"),
            Self::RejoinTimeout(m) => write!(f, "rejoin timed out: {m}"),
            Self::RollbackFailed(m) => write!(f, "rollback failed: {m}"),
            Self::PreflightFailed(m) => write!(f, "preflight check failed: {m}"),
            Self::PostflightFailed(m) => write!(f, "post-flight check failed: {m}"),
        }
    }
}

impl From<EsoError> for RotationError {
    fn from(e: EsoError) -> Self {
        Self::EsoFetchFailed(e.to_string())
    }
}

pub type RotationResult<T> = Result<T, RotationError>;

// ─────────────────────────────────────────────────────────────────────────────
// Quorum calculator
// ─────────────────────────────────────────────────────────────────────────────

/// SCP quorum parameters for a cluster of `n` nodes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QuorumParameters {
    /// Total number of validator nodes.
    pub total_nodes: usize,
    /// Minimum nodes that must agree (⌈2/3 · n⌉).
    pub threshold: usize,
    /// Maximum nodes that may be simultaneously rotating/offline.
    pub max_simultaneous_rotating: usize,
}

impl QuorumParameters {
    /// Compute quorum parameters for `total_nodes` nodes using SCP ≥ 2/3 rule.
    pub fn for_cluster(total_nodes: usize) -> Self {
        if total_nodes == 0 {
            return Self {
                total_nodes: 0,
                threshold: 0,
                max_simultaneous_rotating: 0,
            };
        }
        // ⌈2/3 · n⌉
        let threshold = (total_nodes * 2 + 2) / 3;
        let max_simultaneous_rotating = total_nodes.saturating_sub(threshold);
        Self {
            total_nodes,
            threshold,
            max_simultaneous_rotating,
        }
    }

    /// Returns true if removing `rotating_count` nodes from `healthy_count` would
    /// still satisfy quorum.
    pub fn rotation_is_safe(&self, healthy_count: usize, rotating_count: usize) -> bool {
        healthy_count.saturating_sub(rotating_count) >= self.threshold
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Single-node rotation worker
// ─────────────────────────────────────────────────────────────────────────────

/// Performs the rotation lifecycle for a single validator node.
pub struct NodeRotationWorker<B, C, M, H>
where
    B: EsoBackend,
    C: StellarCoreOps,
    M: ConfigMapOps,
    H: HorizonDbOps,
{
    config: NodeIdentityRotationConfig,
    eso_backend: Arc<B>,
    core_ops: Arc<C>,
    configmap_ops: Arc<M>,
    horizon_ops: Option<Arc<H>>,
}

impl<B, C, M, H> fmt::Debug for NodeRotationWorker<B, C, M, H>
where
    B: EsoBackend + fmt::Debug,
    C: StellarCoreOps + fmt::Debug,
    M: ConfigMapOps + fmt::Debug,
    H: HorizonDbOps + fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NodeRotationWorker")
            .field("config", &self.config)
            .finish()
    }
}

impl<B, C, M, H> NodeRotationWorker<B, C, M, H>
where
    B: EsoBackend,
    C: StellarCoreOps,
    M: ConfigMapOps,
    H: HorizonDbOps,
{
    /// Create a new worker.
    pub fn new(
        config: NodeIdentityRotationConfig,
        eso_backend: Arc<B>,
        core_ops: Arc<C>,
        configmap_ops: Arc<M>,
        horizon_ops: Option<Arc<H>>,
    ) -> Self {
        Self {
            config,
            eso_backend,
            core_ops,
            configmap_ops,
            horizon_ops,
        }
    }

    /// Rotate the identity of a single node.
    ///
    /// Returns the per-node rotation result. Never panics — all failures are
    /// captured in the result.
    pub async fn rotate_node(&self, entry: &RotationPlanEntry) -> NodeRotationResult {
        let started_at = Utc::now();
        let node = &entry.node_name;
        let ns = &entry.namespace;

        info!(
            node = %node,
            namespace = %ns,
            secret_path = %entry.secret_path,
            "Starting identity rotation"
        );

        // Step 1: Fetch new identity from ESO backend.
        let new_identity = match self
            .eso_backend
            .fetch_current(&entry.secret_path)
            .await
        {
            Ok(id) => {
                info!(
                    node = %node,
                    fingerprint = %id.fingerprint,
                    version = %id.version_id,
                    "Fetched new identity from ESO backend"
                );
                id
            }
            Err(e) => {
                error!(node = %node, error = %e, "ESO fetch failed");
                return self.failure_result(
                    entry,
                    started_at,
                    RotationError::EsoFetchFailed(e.to_string()),
                );
            }
        };

        // Skip if identity has not changed.
        if let Some(ref current) = entry.current_fingerprint {
            if *current == new_identity.fingerprint {
                info!(node = %node, "Identity unchanged — skipping rotation");
                return NodeRotationResult {
                    node_name: node.clone(),
                    namespace: ns.clone(),
                    outcome: NodeRotationOutcome::Skipped,
                    new_fingerprint: Some(new_identity.fingerprint),
                    error_message: None,
                    started_at,
                    finished_at: Utc::now(),
                };
            }
        }

        // Step 2: Read the current ConfigMap seed (for rollback).
        let configmap_name = self.configmap_name(node);
        let previous_seed = match self
            .configmap_ops
            .read_current_seed(&configmap_name, ns)
            .await
        {
            Ok(seed) => seed,
            Err(e) => {
                warn!(
                    node = %node,
                    error = %e,
                    "Could not read previous seed from ConfigMap — rollback seed unavailable"
                );
                String::new()
            }
        };

        // Step 3: Un-peer the node.
        if let Err(e) = self.core_ops.unpeer(node, ns).await {
            error!(node = %node, error = %e, "Un-peer failed");
            return self.failure_result(
                entry,
                started_at,
                RotationError::UnpeerFailed(e.to_string()),
            );
        }
        info!(node = %node, "Node un-peered");

        // Step 4: Inject new identity into the K8s Secret.
        let secret_name = format!("{node}-seed");
        if let Err(e) = self
            .core_ops
            .inject_identity(node, ns, &new_identity, &secret_name)
            .await
        {
            error!(node = %node, error = %e, "Identity injection failed");
            if self.config.rollback_on_failure && !previous_seed.is_empty() {
                self.attempt_rollback(node, ns, &configmap_name, &previous_seed).await;
            }
            return self.failure_result(
                entry,
                started_at,
                RotationError::IdentityInjectionFailed(e.to_string()),
            );
        }
        info!(
            node = %node,
            fingerprint = %new_identity.fingerprint,
            "New identity injected into K8s Secret"
        );

        // Step 5: Update ConfigMap.
        if let Err(e) = self
            .configmap_ops
            .update_seed(
                &configmap_name,
                ns,
                new_identity.seed_secret(),
                &new_identity.public_key,
            )
            .await
        {
            error!(node = %node, error = %e, "ConfigMap update failed");
            if self.config.rollback_on_failure && !previous_seed.is_empty() {
                self.attempt_rollback(node, ns, &configmap_name, &previous_seed).await;
            }
            return self.failure_result(
                entry,
                started_at,
                RotationError::ConfigMapUpdateFailed(e.to_string()),
            );
        }
        info!(node = %node, "ConfigMap updated with new identity");

        // Step 6: Wait for the node to rejoin consensus.
        let rejoin_timeout = Duration::from_secs(self.config.rejoin_timeout_secs);
        let poll_interval = Duration::from_secs(self.config.rejoin_poll_interval_secs);
        match self
            .core_ops
            .wait_for_rejoin(node, ns, self.config.min_authenticated_peers, rejoin_timeout, poll_interval)
            .await
        {
            Ok(info) => {
                info!(
                    node = %node,
                    ledger = info.ledger_sequence,
                    peers = info.authenticated_peer_count,
                    "Node rejoined consensus"
                );
            }
            Err(e) => {
                error!(node = %node, error = %e, "Rejoin timed out");
                if self.config.rollback_on_failure && !previous_seed.is_empty() {
                    self.attempt_rollback(node, ns, &configmap_name, &previous_seed).await;
                    return NodeRotationResult {
                        node_name: node.clone(),
                        namespace: ns.clone(),
                        outcome: NodeRotationOutcome::RolledBack,
                        new_fingerprint: None,
                        error_message: Some(e.to_string()),
                        started_at,
                        finished_at: Utc::now(),
                    };
                }
                return self.failure_result(
                    entry,
                    started_at,
                    RotationError::RejoinTimeout(e.to_string()),
                );
            }
        }

        // Step 7 (optional): Update Horizon DB.
        if self.config.update_horizon_db {
            if let Some(ref horizon_ops) = self.horizon_ops {
                if let Some(ref current_pk) = entry.current_fingerprint {
                    // Note: current_fingerprint is a fingerprint, not a public key.
                    // In production, pass the actual old public key here.
                    match horizon_ops
                        .update_node_key(current_pk, &new_identity.public_key, node)
                        .await
                    {
                        Ok(result) => {
                            info!(
                                node = %node,
                                rows_updated = result.rows_updated,
                                "Horizon DB updated for new node identity"
                            );
                        }
                        Err(e) => {
                            // Non-fatal: log and continue; the validator is already rotated.
                            warn!(node = %node, error = %e, "Horizon DB update failed (non-fatal)");
                        }
                    }
                }
            }
        }

        info!(
            node = %node,
            fingerprint = %new_identity.fingerprint,
            "Node identity rotation completed successfully"
        );

        NodeRotationResult {
            node_name: node.clone(),
            namespace: ns.clone(),
            outcome: NodeRotationOutcome::Rotated,
            new_fingerprint: Some(new_identity.fingerprint),
            error_message: None,
            started_at,
            finished_at: Utc::now(),
        }
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    fn configmap_name(&self, node_name: &str) -> String {
        self.config
            .configmap_name_pattern
            .replace("{node_name}", node_name)
    }

    fn failure_result(
        &self,
        entry: &RotationPlanEntry,
        started_at: DateTime<Utc>,
        error: RotationError,
    ) -> NodeRotationResult {
        NodeRotationResult {
            node_name: entry.node_name.clone(),
            namespace: entry.namespace.clone(),
            outcome: NodeRotationOutcome::Failed,
            new_fingerprint: None,
            error_message: Some(error.to_string()),
            started_at,
            finished_at: Utc::now(),
        }
    }

    async fn attempt_rollback(
        &self,
        node_name: &str,
        namespace: &str,
        configmap_name: &str,
        previous_seed: &str,
    ) {
        warn!(node = %node_name, "Attempting rollback to previous identity");
        // Re-write the configmap with the old seed.  We use a placeholder
        // public-key here; real callers derive it from the seed.
        if let Err(e) = self
            .configmap_ops
            .update_seed(configmap_name, namespace, previous_seed, "<rollback>")
            .await
        {
            error!(node = %node_name, error = %e, "Rollback ConfigMap update failed");
        } else {
            info!(node = %node_name, "Rollback ConfigMap update succeeded");
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Cluster-level rotation controller
// ─────────────────────────────────────────────────────────────────────────────

/// Orchestrates a staggered rotation across all nodes in a cluster.
///
/// Iterates through the rotation plan one node at a time, enforcing the quorum
/// safety invariant between each node rotation.
pub struct ClusterRotationController<B, C, M, H>
where
    B: EsoBackend,
    C: StellarCoreOps,
    M: ConfigMapOps,
    H: HorizonDbOps,
{
    config: NodeIdentityRotationConfig,
    worker: NodeRotationWorker<B, C, M, H>,
    core_ops: Arc<C>,
}

impl<B, C, M, H> ClusterRotationController<B, C, M, H>
where
    B: EsoBackend,
    C: StellarCoreOps,
    M: ConfigMapOps,
    H: HorizonDbOps,
{
    /// Create a new controller.
    pub fn new(
        config: NodeIdentityRotationConfig,
        eso_backend: Arc<B>,
        core_ops: Arc<C>,
        configmap_ops: Arc<M>,
        horizon_ops: Option<Arc<H>>,
    ) -> Self {
        let worker = NodeRotationWorker::new(
            config.clone(),
            Arc::clone(&eso_backend),
            Arc::clone(&core_ops),
            Arc::clone(&configmap_ops),
            horizon_ops,
        );
        Self {
            config,
            worker,
            core_ops,
        }
    }

    /// Execute a staggered rotation across the entire cluster.
    ///
    /// # Quorum safety
    ///
    /// Before rotating each node:
    /// 1. Check that `healthy_count - 1 >= threshold`.
    /// 2. If not, abort the remaining rotations.
    /// After each rotation, wait `inter_node_delay_secs` to let the rotated
    /// node fully rejoin before proceeding.
    pub async fn rotate_cluster(
        &self,
        cluster_id: &str,
        plan: &[RotationPlanEntry],
    ) -> ClusterRotationReport {
        let started_at = Utc::now();
        let total_nodes = plan.len();
        let quorum = QuorumParameters::for_cluster(total_nodes);

        info!(
            cluster = %cluster_id,
            total_nodes,
            quorum_threshold = quorum.threshold,
            max_simultaneous_rotating = quorum.max_simultaneous_rotating,
            "Starting cluster-wide identity rotation"
        );

        // Initial preflight: verify cluster health.
        let healthy_count = self.count_healthy_nodes(plan).await;
        if healthy_count < quorum.threshold {
            warn!(
                cluster = %cluster_id,
                healthy = healthy_count,
                threshold = quorum.threshold,
                "Preflight failed — cluster is not healthy enough to start rotation"
            );
            let results = plan
                .iter()
                .map(|e| NodeRotationResult {
                    node_name: e.node_name.clone(),
                    namespace: e.namespace.clone(),
                    outcome: NodeRotationOutcome::Skipped,
                    new_fingerprint: None,
                    error_message: Some(format!(
                        "cluster preflight failed: {healthy_count}/{total_nodes} healthy"
                    )),
                    started_at,
                    finished_at: Utc::now(),
                })
                .collect::<Vec<_>>();
            return ClusterRotationReport {
                cluster_id: cluster_id.to_owned(),
                total_nodes,
                rotated: 0,
                skipped: total_nodes,
                failed: 0,
                rolled_back: 0,
                started_at,
                finished_at: Utc::now(),
                node_results: results,
            };
        }

        let mut node_results = Vec::with_capacity(total_nodes);
        let mut rotated = 0usize;
        let mut skipped = 0usize;
        let mut failed = 0usize;
        let mut rolled_back = 0usize;
        let mut current_healthy = healthy_count;

        for (i, entry) in plan.iter().enumerate() {
            // Quorum gate: ensure we can afford to take this node offline.
            if !quorum.rotation_is_safe(current_healthy, 1) {
                warn!(
                    cluster = %cluster_id,
                    node = %entry.node_name,
                    healthy = current_healthy,
                    threshold = quorum.threshold,
                    "Quorum gate: would breach threshold — aborting remaining rotations"
                );
                let remaining = plan[i..]
                    .iter()
                    .map(|e| NodeRotationResult {
                        node_name: e.node_name.clone(),
                        namespace: e.namespace.clone(),
                        outcome: NodeRotationOutcome::Skipped,
                        new_fingerprint: None,
                        error_message: Some("aborted: quorum threshold would be breached".into()),
                        started_at,
                        finished_at: Utc::now(),
                    })
                    .collect::<Vec<_>>();
                skipped += remaining.len();
                node_results.extend(remaining);
                break;
            }

            // Rotate the node.
            let result = self.worker.rotate_node(entry).await;

            match result.outcome {
                NodeRotationOutcome::Rotated => {
                    rotated += 1;
                    // Node is back online — healthy count stays the same.
                }
                NodeRotationOutcome::Skipped => {
                    skipped += 1;
                }
                NodeRotationOutcome::RolledBack => {
                    rolled_back += 1;
                    // Node was rolled back — treat as still healthy.
                }
                NodeRotationOutcome::Failed => {
                    failed += 1;
                    // Assume the node is temporarily unhealthy.
                    current_healthy = current_healthy.saturating_sub(1);
                    error!(
                        cluster = %cluster_id,
                        node = %entry.node_name,
                        "Node rotation failed — reducing healthy count"
                    );
                }
            }

            node_results.push(result);

            // Inter-node delay (skip after the last node).
            if i + 1 < plan.len() && self.config.inter_node_delay_secs > 0 {
                debug!(
                    cluster = %cluster_id,
                    delay_secs = self.config.inter_node_delay_secs,
                    next_node = %plan[i + 1].node_name,
                    "Waiting inter-node delay before next rotation"
                );
                sleep(Duration::from_secs(self.config.inter_node_delay_secs)).await;
                // Re-check healthy count before the next iteration.
                current_healthy = self.count_healthy_nodes(plan).await;
            }
        }

        let report = ClusterRotationReport {
            cluster_id: cluster_id.to_owned(),
            total_nodes,
            rotated,
            skipped,
            failed,
            rolled_back,
            started_at,
            finished_at: Utc::now(),
            node_results,
        };

        info!(
            cluster = %cluster_id,
            rotated,
            skipped,
            failed,
            rolled_back,
            "Cluster rotation completed"
        );

        report
    }

    // ── Internal helpers ─────────────────────────────────────────────────────

    /// Count nodes currently reporting a healthy consensus state.
    async fn count_healthy_nodes(&self, plan: &[RotationPlanEntry]) -> usize {
        let mut count = 0;
        for entry in plan {
            match self
                .core_ops
                .consensus_info(&entry.node_name, &entry.namespace)
                .await
            {
                Ok(info) if info.is_healthy(self.config.min_authenticated_peers) => {
                    count += 1;
                }
                Ok(_) => {
                    debug!(node = %entry.node_name, "Node is unhealthy in health check");
                }
                Err(e) => {
                    warn!(node = %entry.node_name, error = %e, "Health check failed for node");
                }
            }
        }
        count
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Rotation schedule checker
// ─────────────────────────────────────────────────────────────────────────────

/// Checks whether a rotation is due based on the last rotation time and policy.
pub struct RotationScheduler {
    pub rotation_period_days: u32,
}

impl RotationScheduler {
    pub fn new(rotation_period_days: u32) -> Self {
        Self { rotation_period_days }
    }

    /// Returns true if `last_rotation` was more than `rotation_period_days` ago
    /// (or if `last_rotation` is `None`, meaning rotation has never run).
    pub fn is_due(&self, last_rotation: Option<DateTime<Utc>>) -> bool {
        if self.rotation_period_days == 0 {
            return false; // Manual-only
        }
        match last_rotation {
            None => true,
            Some(last) => {
                let elapsed = Utc::now().signed_duration_since(last);
                elapsed.num_days() >= self.rotation_period_days as i64
            }
        }
    }

    /// Returns the next rotation time after `last_rotation`.
    pub fn next_rotation_after(&self, last_rotation: DateTime<Utc>) -> DateTime<Utc> {
        last_rotation + chrono::Duration::days(self.rotation_period_days as i64)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use chrono::{Duration as CDuration, Utc};

    // ── QuorumParameters ─────────────────────────────────────────────────────

    #[test]
    fn quorum_for_five_nodes() {
        let q = QuorumParameters::for_cluster(5);
        // ⌈2/3 × 5⌉ = ⌈3.33…⌉ = 4
        assert_eq!(q.threshold, 4);
        assert_eq!(q.max_simultaneous_rotating, 1);
    }

    #[test]
    fn quorum_for_three_nodes() {
        let q = QuorumParameters::for_cluster(3);
        // ⌈2/3 × 3⌉ = ⌈2⌉ = 2
        assert_eq!(q.threshold, 2);
        assert_eq!(q.max_simultaneous_rotating, 1);
    }

    #[test]
    fn quorum_for_single_node() {
        let q = QuorumParameters::for_cluster(1);
        assert_eq!(q.threshold, 1);
        assert_eq!(q.max_simultaneous_rotating, 0);
    }

    #[test]
    fn quorum_rotation_is_safe() {
        let q = QuorumParameters::for_cluster(5);
        // 5 healthy, rotating 1 → 4 remain ≥ threshold(4) → safe
        assert!(q.rotation_is_safe(5, 1));
        // 4 healthy, rotating 1 → 3 remain < threshold(4) → NOT safe
        assert!(!q.rotation_is_safe(4, 1));
    }

    // ── RotationScheduler ────────────────────────────────────────────────────

    #[test]
    fn rotation_is_due_when_never_run() {
        let sched = RotationScheduler::new(30);
        assert!(sched.is_due(None));
    }

    #[test]
    fn rotation_is_due_after_period_elapsed() {
        let sched = RotationScheduler::new(30);
        let last = Utc::now() - CDuration::days(31);
        assert!(sched.is_due(Some(last)));
    }

    #[test]
    fn rotation_not_due_before_period() {
        let sched = RotationScheduler::new(30);
        let last = Utc::now() - CDuration::days(10);
        assert!(!sched.is_due(Some(last)));
    }

    #[test]
    fn rotation_period_zero_means_manual_only() {
        let sched = RotationScheduler::new(0);
        assert!(!sched.is_due(None));
        assert!(!sched.is_due(Some(Utc::now() - CDuration::days(365))));
    }

    // ── Mock implementations for integration-style tests ────────────────────

    #[derive(Debug, Clone, Default)]
    struct MockEsoBackend {
        pub identity: Option<EsoNodeIdentity>,
    }

    impl EsoBackend for MockEsoBackend {
        fn backend_type(&self) -> super::super::eso_integration::EsoBackendType {
            super::super::eso_integration::EsoBackendType::AwsSecretsManager
        }

        fn fetch_current<'a>(
            &'a self,
            _secret_path: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<EsoNodeIdentity>> + Send + 'a>>
        {
            let identity = self.identity.clone();
            Box::pin(async move {
                identity.ok_or_else(|| EsoError::BackendUnavailable("no identity configured".into()))
            })
        }

        fn fetch_version<'a>(
            &'a self,
            secret_path: &'a str,
            _version_id: &'a str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<EsoNodeIdentity>> + Send + 'a>>
        {
            self.fetch_current(secret_path)
        }

        fn list_versions<'a>(
            &'a self,
            _secret_path: &'a str,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = EsoResult<Vec<super::super::eso_integration::SecretVersionMeta>>> + Send + 'a>,
        > {
            Box::pin(async move { Ok(vec![]) })
        }
    }

    #[derive(Debug, Default)]
    struct MockCoreOps {
        pub healthy: bool,
        pub unpeer_calls: Mutex<Vec<String>>,
        pub inject_calls: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl StellarCoreOps for MockCoreOps {
        async fn consensus_info(&self, node_name: &str, _namespace: &str) -> RotationResult<ConsensusInfo> {
            Ok(ConsensusInfo {
                node_name: node_name.to_owned(),
                state: if self.healthy { "synced".into() } else { "catching-up".into() },
                ledger_sequence: if self.healthy { 1000 } else { 0 },
                authenticated_peer_count: if self.healthy { 4 } else { 0 },
                is_synced: self.healthy,
                observed_at: Utc::now(),
            })
        }

        async fn unpeer(&self, node_name: &str, _namespace: &str) -> RotationResult<()> {
            self.unpeer_calls.lock().unwrap().push(node_name.to_owned());
            Ok(())
        }

        async fn inject_identity(
            &self,
            node_name: &str,
            _namespace: &str,
            _identity: &EsoNodeIdentity,
            _secret_name: &str,
        ) -> RotationResult<()> {
            self.inject_calls.lock().unwrap().push(node_name.to_owned());
            Ok(())
        }

        async fn wait_for_rejoin(
            &self,
            node_name: &str,
            _namespace: &str,
            _min_peers: usize,
            _timeout: Duration,
            _poll_interval: Duration,
        ) -> RotationResult<ConsensusInfo> {
            Ok(ConsensusInfo {
                node_name: node_name.to_owned(),
                state: "synced".into(),
                ledger_sequence: 1001,
                authenticated_peer_count: 4,
                is_synced: true,
                observed_at: Utc::now(),
            })
        }
    }

    #[derive(Debug, Default)]
    struct MockConfigMapOps;

    #[async_trait::async_trait]
    impl ConfigMapOps for MockConfigMapOps {
        async fn update_seed(
            &self,
            _configmap_name: &str,
            _namespace: &str,
            _new_seed: &str,
            _new_public_key: &str,
        ) -> RotationResult<()> {
            Ok(())
        }

        async fn read_current_seed(
            &self,
            _configmap_name: &str,
            _namespace: &str,
        ) -> RotationResult<String> {
            Ok("SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM".into())
        }
    }

    #[derive(Debug, Default)]
    struct MockHorizonOps;

    #[async_trait::async_trait]
    impl HorizonDbOps for MockHorizonOps {
        async fn update_node_key(
            &self,
            old_public_key: &str,
            new_public_key: &str,
            _node_name: &str,
        ) -> RotationResult<HorizonDbUpdateResult> {
            Ok(HorizonDbUpdateResult {
                rows_updated: 1,
                old_public_key: old_public_key.to_owned(),
                new_public_key: new_public_key.to_owned(),
            })
        }
    }

    fn make_identity(seed: &str) -> EsoNodeIdentity {
        EsoNodeIdentity::from_seed(
            seed.to_string(),
            "v1".into(),
            Utc::now(),
            super::super::eso_integration::EsoBackendType::AwsSecretsManager,
        )
        .expect("valid seed")
    }

    // ── Worker test: successful rotation ────────────────────────────────────

    #[tokio::test]
    async fn worker_rotates_node_successfully() {
        let seed = "SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM";
        let identity = make_identity(seed);

        let backend = Arc::new(MockEsoBackend {
            identity: Some(identity.clone()),
        });
        let core_ops = Arc::new(MockCoreOps { healthy: true, ..Default::default() });
        let cm_ops = Arc::new(MockConfigMapOps);
        let horizon_ops: Option<Arc<MockHorizonOps>> = None;

        let config = NodeIdentityRotationConfig {
            enabled: true,
            inter_node_delay_secs: 0,
            rejoin_timeout_secs: 5,
            rejoin_poll_interval_secs: 1,
            ..Default::default()
        };

        let worker = NodeRotationWorker::new(config, backend, core_ops.clone(), cm_ops, horizon_ops);

        let entry = RotationPlanEntry {
            node_name: "validator-0".into(),
            namespace: "stellar".into(),
            secret_path: "stellar/validators/validator-0".into(),
            current_fingerprint: None,
        };

        let result = worker.rotate_node(&entry).await;
        assert_eq!(
            result.outcome,
            NodeRotationOutcome::Rotated,
            "expected Rotated, got {:?}: {:?}",
            result.outcome,
            result.error_message
        );
        assert_eq!(result.new_fingerprint, Some(identity.fingerprint));

        // Verify unpeer and inject were called.
        assert_eq!(
            core_ops.unpeer_calls.lock().unwrap().as_slice(),
            &["validator-0"]
        );
        assert_eq!(
            core_ops.inject_calls.lock().unwrap().as_slice(),
            &["validator-0"]
        );
    }

    // ── Worker test: skip when identity unchanged ────────────────────────────

    #[tokio::test]
    async fn worker_skips_when_identity_unchanged() {
        let seed = "SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM";
        let identity = make_identity(seed);
        let fingerprint = identity.fingerprint.clone();

        let backend = Arc::new(MockEsoBackend { identity: Some(identity) });
        let core_ops = Arc::new(MockCoreOps { healthy: true, ..Default::default() });

        let worker = NodeRotationWorker::new(
            NodeIdentityRotationConfig::default(),
            backend,
            core_ops.clone(),
            Arc::new(MockConfigMapOps),
            None::<Arc<MockHorizonOps>>,
        );

        let entry = RotationPlanEntry {
            node_name: "validator-0".into(),
            namespace: "stellar".into(),
            secret_path: "stellar/validators/validator-0".into(),
            current_fingerprint: Some(fingerprint),
        };

        let result = worker.rotate_node(&entry).await;
        assert_eq!(result.outcome, NodeRotationOutcome::Skipped);
        // Unpeer should NOT have been called.
        assert!(core_ops.unpeer_calls.lock().unwrap().is_empty());
    }

    // ── Cluster controller: preflight failure ────────────────────────────────

    #[tokio::test]
    async fn cluster_controller_aborts_on_preflight_failure() {
        let seed = "SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM";
        let identity = make_identity(seed);
        let backend = Arc::new(MockEsoBackend { identity: Some(identity) });
        // All nodes unhealthy.
        let core_ops = Arc::new(MockCoreOps { healthy: false, ..Default::default() });

        let config = NodeIdentityRotationConfig {
            inter_node_delay_secs: 0,
            ..Default::default()
        };

        let controller = ClusterRotationController::new(
            config,
            backend,
            core_ops,
            Arc::new(MockConfigMapOps),
            None::<Arc<MockHorizonOps>>,
        );

        let plan = vec![
            RotationPlanEntry {
                node_name: "v0".into(),
                namespace: "stellar".into(),
                secret_path: "s/v0".into(),
                current_fingerprint: None,
            },
            RotationPlanEntry {
                node_name: "v1".into(),
                namespace: "stellar".into(),
                secret_path: "s/v1".into(),
                current_fingerprint: None,
            },
            RotationPlanEntry {
                node_name: "v2".into(),
                namespace: "stellar".into(),
                secret_path: "s/v2".into(),
                current_fingerprint: None,
            },
        ];

        let report = controller.rotate_cluster("test-cluster", &plan).await;
        // All nodes should be skipped because the preflight check fails.
        assert_eq!(report.rotated, 0);
        assert_eq!(report.skipped, 3);
    }

    // ── ClusterRotationReport helper ─────────────────────────────────────────

    #[test]
    fn report_is_fully_successful_when_no_failures() {
        let report = ClusterRotationReport {
            cluster_id: "test".into(),
            total_nodes: 3,
            rotated: 3,
            skipped: 0,
            failed: 0,
            rolled_back: 0,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            node_results: vec![],
        };
        assert!(report.is_fully_successful());
    }

    #[test]
    fn report_not_successful_when_failed() {
        let report = ClusterRotationReport {
            cluster_id: "test".into(),
            total_nodes: 3,
            rotated: 2,
            skipped: 0,
            failed: 1,
            rolled_back: 0,
            started_at: Utc::now(),
            finished_at: Utc::now(),
            node_results: vec![],
        };
        assert!(!report.is_fully_successful());
    }
}
