//! GitOps deployment engine for Stellar clusters.
//!
//! This module turns a Git repository into the single source of truth for
//! Stellar-K8s configuration. A polling loop watches a tracking branch; on
//! every new commit SHA the engine renders the declared ConfigMaps and patches
//! the live Captive Core / Horizon configuration, then arms a health watchdog.
//! If the new configuration breaks node sync, the engine automatically rolls
//! back to the previous known-good commit.
//!
//! # Module layout
//!
//! - [`github`] — rate-limit-aware GitHub API client
//! - [`sync`] — desired-state rendering and ConfigMap reconciliation
//! - [`health_check`] — Prometheus-based node health monitor and rollback
//!
//! # Thrash protection
//!
//! The engine is deliberately conservative when the world is unhappy:
//!
//! - GitHub rate limits pause *polling* (and therefore cluster mutations)
//!   until the quota resets — see [`GitHubClient::rate_limit_backoff`].
//! - Network partitions keep the last-known-good state applied while the loop
//!   retries with exponential backoff.
//! - A commit that caused a rollback is pinned in [`GitOpsEngineState`]; the
//!   same SHA is never re-applied automatically, preventing a bad commit from
//!   flip-flopping the cluster.
//! - Only the leader applies changes; replicas run the loop inert
//!   ([`GitOpsEngine::run`] checks [`is_leader`]).
//!
//! # Usage
//!
//! ```rust,no_run
//! use std::sync::Arc;
//! use stellar_k8s::controller::gitops::{GitOpsConfig, GitOpsEngine};
//!
//! # async fn example() -> stellar_k8s::Result<()> {
//! let config = GitOpsConfig {
//!     repo: "my-org/stellar-config".to_string(),
//!     branch: "main".to_string(),
//!     manifests_path: "clusters/prod".to_string(),
//!     token: None,
//!     ..Default::default()
//! };
//! let engine = GitOpsEngine::new(config);
//! // Runs until the process shuts down. Requires a leader-election signal.
//! // engine.run(client, is_leader).await?;
//! # Ok(())
//! # }
//! ```

pub mod github;
pub mod health_check;
pub mod sync;

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};

use crate::error::Result;

use github::{Fetch, GitHubClient};

/// How often the engine polls the tracking branch for new commits.
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 30;

/// Grace period after applying a commit before health is evaluated.
pub const DEFAULT_HEALTH_GRACE_PERIOD_SECS: u64 = 120;

/// Maximum number of recent events retained in engine state.
pub const MAX_TRACKED_EVENTS: usize = 100;

/// GitOps engine configuration.
///
/// Persisted from the operator CLI/ConfigMap; all durations are in seconds so
/// the struct stays YAML/JSON friendly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct GitOpsConfig {
    /// GitHub repository in `owner/repo` format acting as source of truth.
    pub repo: String,
    /// Tracking branch to poll (e.g. `main` or a dedicated `gitops` branch).
    pub branch: String,
    /// Repository path containing the manifest tree.
    pub manifests_path: String,
    /// Optional GitHub token (raises the API rate limit to 5000/h).
    pub token: Option<String>,
    /// Seconds between branch polls.
    pub poll_interval_secs: u64,
    /// Seconds to wait after applying a commit before judging its health.
    pub health_grace_period_secs: u64,
    /// Seconds a node must be continuously unhealthy before rollback fires.
    pub unhealthy_threshold_secs: u64,
}

impl Default for GitOpsConfig {
    fn default() -> Self {
        Self {
            repo: String::new(),
            branch: "main".to_string(),
            manifests_path: "clusters".to_string(),
            token: None,
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            health_grace_period_secs: DEFAULT_HEALTH_GRACE_PERIOD_SECS,
            unhealthy_threshold_secs: DEFAULT_HEALTH_GRACE_PERIOD_SECS,
        }
    }
}

impl GitOpsConfig {
    /// Validate the configuration before the engine starts.
    pub fn validate(&self) -> Result<()> {
        if self.repo.trim().is_empty() {
            return Err(crate::error::Error::ConfigError(
                "gitops.repo must not be empty".to_string(),
            ));
        }
        if !self.repo.contains('/') || self.repo.split('/').count() != 2 {
            return Err(crate::error::Error::ConfigError(format!(
                "gitops.repo must be in owner/repo format, got '{}'",
                self.repo
            )));
        }
        if self.branch.trim().is_empty() {
            return Err(crate::error::Error::ConfigError(
                "gitops.branch must not be empty".to_string(),
            ));
        }
        if self.poll_interval_secs == 0 {
            return Err(crate::error::Error::ConfigError(
                "gitops.poll_interval_secs must be > 0".to_string(),
            ));
        }
        Ok(())
    }
}

/// Lifecycle of the engine with respect to one commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncPhase {
    /// No commit applied yet (fresh start).
    Idle,
    /// A new commit was detected and is being applied.
    Applying,
    /// Commit applied; within the grace period before health is judged.
    Observing,
    /// Commit applied and node health confirmed good.
    Healthy,
    /// Commit applied but health degraded; rollback in progress.
    RollingBack,
    /// Reverted to the previous known-good commit.
    RolledBack,
}

impl std::fmt::Display for SyncPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            SyncPhase::Idle => "idle",
            SyncPhase::Applying => "applying",
            SyncPhase::Observing => "observing",
            SyncPhase::Healthy => "healthy",
            SyncPhase::RollingBack => "rolling_back",
            SyncPhase::RolledBack => "rolled_back",
        };
        write!(f, "{s}")
    }
}

/// A notable engine event, kept in a bounded history for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitOpsEvent {
    /// Unix seconds when the event occurred.
    pub at: u64,
    /// Commit SHA the event relates to (short form).
    pub commit: String,
    /// Machine-readable phase at event time.
    pub phase: SyncPhase,
    /// Human-readable description.
    pub message: String,
}

/// Shared, thread-safe engine state.
///
/// Exposed through the REST API (`/api/v1/gitops/status`) and used by the
/// health watchdog to decide rollbacks.
#[derive(Debug)]
pub struct GitOpsEngineState {
    /// SHA currently applied to the cluster.
    pub current_commit: Mutex<Option<String>>,
    /// Last SHA known to produce a healthy cluster (rollback target).
    pub last_good_commit: Mutex<Option<String>>,
    /// SHAs that caused a rollback — never re-applied automatically.
    pub banned_commits: Mutex<Vec<String>>,
    /// Current phase.
    pub phase: Mutex<SyncPhase>,
    /// Unix seconds when the current commit was applied.
    pub applied_at: Mutex<Option<u64>>,
    /// Consecutive unhealthy seconds observed for the current commit.
    pub unhealthy_since: AtomicU64,
    /// Total successful syncs (new commits applied).
    pub syncs_total: AtomicU64,
    /// Total rollbacks executed.
    pub rollbacks_total: AtomicU64,
    /// Total polls skipped because of GitHub rate limits.
    pub rate_limit_pauses_total: AtomicU64,
    /// Bounded recent-event history (newest last).
    pub events: Mutex<Vec<GitOpsEvent>>,
    /// Set when the loop has been asked to stop.
    pub shutdown: AtomicBool,
}

impl Default for GitOpsEngineState {
    fn default() -> Self {
        Self {
            current_commit: Mutex::new(None),
            last_good_commit: Mutex::new(None),
            banned_commits: Mutex::new(Vec::new()),
            phase: Mutex::new(SyncPhase::Idle),
            applied_at: Mutex::new(None),
            unhealthy_since: AtomicU64::new(0),
            syncs_total: AtomicU64::new(0),
            rollbacks_total: AtomicU64::new(0),
            rate_limit_pauses_total: AtomicU64::new(0),
            events: Mutex::new(Vec::new()),
            shutdown: AtomicBool::new(false),
        }
    }
}

impl GitOpsEngineState {
    /// Record an event in the bounded history and log it.
    pub fn record_event(&self, commit: &str, phase: &SyncPhase, message: impl Into<String>) {
        let event = GitOpsEvent {
            at: github::unix_now(),
            commit: short_sha(commit),
            phase: phase.clone(),
            message: message.into(),
        };
        let mut events = self.events.lock().unwrap();
        events.push(event);
        let overflow = events.len().saturating_sub(MAX_TRACKED_EVENTS);
        if overflow > 0 {
            events.drain(0..overflow);
        }
        debug!(commit = %commit, phase = %phase, "gitops event");
    }

    /// Mark a commit as banned (caused a rollback).
    pub fn ban_commit(&self, commit: &str) {
        let mut banned = self.banned_commits.lock().unwrap();
        if !banned.iter().any(|c| c == commit) {
            banned.push(commit.to_string());
        }
    }

    /// `true` when the SHA was previously rolled back.
    pub fn is_banned(&self, commit: &str) -> bool {
        self.banned_commits
            .lock()
            .unwrap()
            .iter()
            .any(|c| c == commit)
    }
}

/// Truncate a SHA to the conventional 7-char short form.
pub fn short_sha(sha: &str) -> String {
    sha.chars().take(7).collect()
}

/// The GitOps deployment engine.
pub struct GitOpsEngine {
    config: GitOpsConfig,
    client: GitHubClient,
    state: std::sync::Arc<GitOpsEngineState>,
}

impl GitOpsEngine {
    /// Create an engine with an explicitly provided GitHub client (tests inject
    /// a client pointed at a mock server).
    pub fn with_client(
        config: GitOpsConfig,
        client: GitHubClient,
        state: std::sync::Arc<GitOpsEngineState>,
    ) -> Self {
        Self {
            config,
            client,
            state,
        }
    }

    /// Create an engine from configuration, building the default GitHub client.
    pub fn new(config: GitOpsConfig) -> Self {
        let client = GitHubClient::new(config.token.clone(), None, None)
            .expect("default GitHub client construction cannot fail");
        Self::with_client(
            config,
            client,
            std::sync::Arc::new(GitOpsEngineState::default()),
        )
    }

    /// Shared engine state handle.
    pub fn state(&self) -> std::sync::Arc<GitOpsEngineState> {
        self.state.clone()
    }

    /// Engine configuration.
    pub fn config(&self) -> &GitOpsConfig {
        &self.config
    }

    /// Request loop shutdown (idempotent).
    pub fn shutdown(&self) {
        self.state.shutdown.store(true, Ordering::SeqCst);
    }

    /// Run the main polling loop until shutdown.
    ///
    /// Each iteration:
    /// 1. Waits for the poll interval (or shutdown).
    /// 2. Pauses if GitHub rate limits are exhausted (no cluster contact).
    /// 3. Fetches the head commit of the tracking branch.
    /// 4. Applies new, non-banned commits via [`sync::apply_commit`].
    /// 5. Lets [`health_check`] judge the applied commit and roll back when
    ///    node sync health fails for longer than the threshold.
    ///
    /// Network errors never abort the loop: the last-known-good state remains
    /// applied while polling retries with exponential backoff.
    pub async fn run(
        &self,
        k8s: kube::Client,
        is_leader: std::sync::Arc<AtomicBool>,
    ) -> Result<()> {
        self.config.validate()?;
        info!(
            repo = %self.config.repo,
            branch = %self.config.branch,
            path = %self.config.manifests_path,
            interval_secs = self.config.poll_interval_secs,
            "Starting GitOps deployment engine"
        );

        let mut backoff_secs = self.config.poll_interval_secs;
        loop {
            if self.state.shutdown.load(Ordering::SeqCst) {
                info!("GitOps engine shutting down");
                return Ok(());
            }

            // Only the leader mutates cluster state; replicas idle cheaply.
            if !is_leader.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(self.config.poll_interval_secs)).await;
                continue;
            }

            tokio::time::sleep(Duration::from_secs(
                backoff_secs.min(self.config.poll_interval_secs),
            ))
            .await;

            // 1. Respect the GitHub rate limit *before* touching anything.
            if let Some(wait) = self.client.rate_limit_backoff(github::unix_now()) {
                self.state
                    .rate_limit_pauses_total
                    .fetch_add(1, Ordering::SeqCst);
                #[cfg(feature = "metrics")]
                crate::controller::metrics::record_gitops_rate_limit_pause();
                warn!(
                    wait_secs = wait.as_secs(),
                    "GitHub rate limit exhausted; pausing GitOps polling (cluster state frozen)"
                );
                tokio::time::sleep(wait.min(Duration::from_secs(300))).await;
                continue;
            }

            // 2. Poll the tracking branch.
            match self
                .client
                .fetch_latest_commit(&self.config.repo, &self.config.branch)
                .await
            {
                Ok(Fetch::Fresh(sha)) => {
                    backoff_secs = self.config.poll_interval_secs;
                    if let Err(e) = self.process_commit(&k8s, &sha).await {
                        error!(commit = %sha, error = %e, "Failed to process new commit");
                        self.state.record_event(
                            &sha,
                            &self.current_phase(),
                            format!("apply failed: {e}"),
                        );
                    }
                }
                Ok(Fetch::NotModified) => {
                    backoff_secs = self.config.poll_interval_secs;
                    debug!("Tracking branch unchanged (304)");
                }
                Err(e) => {
                    // Network partition / API outage: exponential backoff, keep
                    // last-known-good state, never thrash the cluster.
                    backoff_secs = (backoff_secs * 2).min(self.config.poll_interval_secs * 8);
                    warn!(
                        error = %e,
                        retry_after_secs = backoff_secs,
                        "GitOps poll failed; retaining last-known-good state"
                    );
                }
            }

            // 3. Health watchdog for the currently applied commit.
            if let Err(e) =
                health_check::evaluate_and_maybe_rollback(&k8s, &self.config, &self.state).await
            {
                warn!(error = %e, "GitOps health evaluation failed");
            }
        }
    }

    /// Apply a freshly detected commit if it is new and not banned.
    async fn process_commit(&self, k8s: &kube::Client, sha: &str) -> Result<()> {
        {
            let current = self.state.current_commit.lock().unwrap();
            if current.as_deref() == Some(sha) {
                return Ok(());
            }
        }
        if self.state.is_banned(sha) {
            warn!(
                commit = %sha,
                "Skipping banned commit (previously caused a rollback)"
            );
            return Ok(());
        }

        self.state
            .record_event(sha, &SyncPhase::Applying, "new commit detected");
        *self.state.phase.lock().unwrap() = SyncPhase::Applying;
        #[cfg(feature = "metrics")]
        crate::controller::metrics::set_gitops_phase(&SyncPhase::Applying);

        sync::apply_commit(k8s, &self.config, sha).await?;

        *self.state.current_commit.lock().unwrap() = Some(sha.to_string());
        *self.state.applied_at.lock().unwrap() = Some(github::unix_now());
        self.state.unhealthy_since.store(0, Ordering::SeqCst);
        *self.state.phase.lock().unwrap() = SyncPhase::Observing;
        self.state.syncs_total.fetch_add(1, Ordering::SeqCst);
        self.state.record_event(
            sha,
            &SyncPhase::Observing,
            "commit applied; observing node health",
        );
        #[cfg(feature = "metrics")]
        {
            crate::controller::metrics::set_gitops_phase(&SyncPhase::Observing);
            crate::controller::metrics::record_gitops_sync();
        }
        Ok(())
    }

    /// Execute a rollback to `target` (the last known-good commit).
    pub async fn rollback_to(&self, k8s: &kube::Client, target: &str) -> Result<()> {
        let bad = {
            let current = self.state.current_commit.lock().unwrap();
            current.clone().unwrap_or_default()
        };

        *self.state.phase.lock().unwrap() = SyncPhase::RollingBack;
        self.state.record_event(
            &bad,
            &SyncPhase::RollingBack,
            format!("health degraded; rolling back to {target}"),
        );
        #[cfg(feature = "metrics")]
        crate::controller::metrics::set_gitops_phase(&SyncPhase::RollingBack);

        sync::apply_commit(k8s, &self.config, target).await?;

        if !bad.is_empty() {
            self.state.ban_commit(&bad);
            self.state.rollbacks_total.fetch_add(1, Ordering::SeqCst);
        }
        *self.state.current_commit.lock().unwrap() = Some(target.to_string());
        *self.state.applied_at.lock().unwrap() = Some(github::unix_now());
        self.state.unhealthy_since.store(0, Ordering::SeqCst);
        *self.state.phase.lock().unwrap() = SyncPhase::RolledBack;
        self.state
            .record_event(target, &SyncPhase::RolledBack, "rollback complete");
        #[cfg(feature = "metrics")]
        {
            crate::controller::metrics::set_gitops_phase(&SyncPhase::RolledBack);
            crate::controller::metrics::record_gitops_rollback();
        }
        info!(from = %bad, to = %target, "GitOps rollback complete");
        Ok(())
    }

    fn current_phase(&self) -> SyncPhase {
        self.state.phase.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_config() -> GitOpsConfig {
        GitOpsConfig {
            repo: "acme/stellar-config".to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn config_defaults() {
        let c = GitOpsConfig::default();
        assert_eq!(c.branch, "main");
        assert_eq!(c.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
        assert_eq!(c.health_grace_period_secs, DEFAULT_HEALTH_GRACE_PERIOD_SECS);
        assert!(c.token.is_none());
    }

    #[test]
    fn config_validation_rejects_bad_repo() {
        let mut c = valid_config();
        assert!(c.validate().is_ok());

        c.repo = String::new();
        assert!(c.validate().is_err());

        c.repo = "just-a-name".to_string();
        assert!(c.validate().is_err());

        c.repo = "a/b/c".to_string();
        assert!(c.validate().is_err());

        c.repo = "acme/stellar-config".to_string();
        c.branch = "  ".to_string();
        assert!(c.validate().is_err());

        c.branch = "main".to_string();
        c.poll_interval_secs = 0;
        assert!(c.validate().is_err());
    }

    #[test]
    fn config_serializes_camel_case() {
        let c = valid_config();
        let json = serde_json::to_string(&c).unwrap();
        assert!(json.contains("\"pollIntervalSecs\""));
        assert!(json.contains("\"manifestsPath\""));
        let round: GitOpsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(round, c);
    }

    #[test]
    fn short_sha_truncates() {
        assert_eq!(short_sha("0123456789abcdef"), "0123456");
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
    }

    #[test]
    fn sync_phase_display() {
        assert_eq!(SyncPhase::Idle.to_string(), "idle");
        assert_eq!(SyncPhase::RolledBack.to_string(), "rolled_back");
        assert_eq!(SyncPhase::RollingBack.to_string(), "rolling_back");
    }

    #[test]
    fn state_event_history_is_bounded() {
        let state = GitOpsEngineState::default();
        for i in 0..(MAX_TRACKED_EVENTS + 50) {
            state.record_event(&format!("sha{i}"), &SyncPhase::Healthy, format!("e{i}"));
        }
        let events = state.events.lock().unwrap();
        assert_eq!(events.len(), MAX_TRACKED_EVENTS);
        // Newest events retained (loop ran 0..=MAX_TRACKED_EVENTS + 49).
        let last_idx = MAX_TRACKED_EVENTS + 49;
        assert!(events
            .last()
            .unwrap()
            .message
            .contains(&format!("e{last_idx}")));
    }

    #[test]
    fn state_ban_and_query_commits() {
        let state = GitOpsEngineState::default();
        assert!(!state.is_banned("deadbee"));
        state.ban_commit("deadbee");
        state.ban_commit("deadbee"); // idempotent
        assert!(state.is_banned("deadbee"));
        assert_eq!(state.banned_commits.lock().unwrap().len(), 1);
    }

    #[test]
    fn engine_shutdown_flag_is_idempotent() {
        let engine = GitOpsEngine::new(valid_config());
        assert!(!engine.state().shutdown.load(Ordering::SeqCst));
        engine.shutdown();
        engine.shutdown();
        assert!(engine.state().shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn engine_exposes_config_and_state() {
        let cfg = valid_config();
        let engine = GitOpsEngine::new(cfg.clone());
        assert_eq!(engine.config(), &cfg);
        let s = engine.state();
        assert_eq!(*s.phase.lock().unwrap(), SyncPhase::Idle);
    }
}
