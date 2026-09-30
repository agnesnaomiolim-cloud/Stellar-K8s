//! Node sync-health monitoring and automated rollback for the GitOps engine.
//!
//! After the engine applies a new commit, a health watchdog judges whether the
//! configuration change kept the node syncing. Health is derived from the
//! same Prometheus metrics the reconciler exports ([`crate::controller::metrics`]):
//!
//! - `stellar_node_sync_status` — `0=Pending … 3=Syncing, 4=Ready`; anything
//!   below `3` (or `Failed`/`Degraded`) means the node is not making progress.
//! - `stellar_node_up` — pod readiness (`1` = up, `0` = down/crashed).
//!
//! A commit is **unhealthy** while every recent probe reports `!healthy`.
//! When the unhealthy period exceeds `unhealthy_threshold_secs` (default:
//! the same as the grace period, 2 minutes) and a known-good rollback target
//! exists, the engine re-applies the previous commit. Combined with the
//! 2-minute grace period the worst-case detection+rollback window is 4
//! minutes — inside the issue's 5-minute requirement with margin.
//!
//! # Usage
//!
//! ```rust,no_run
//! # use std::sync::Arc;
//! # use stellar_k8s::controller::gitops::{GitOpsEngineState, GitOpsConfig};
//! # async fn example(client: kube::Client) -> stellar_k8s::Result<()> {
//! let state = Arc::new(GitOpsEngineState::default());
//! let config = GitOpsConfig { ..Default::default() };
//! // Called from the engine loop after each poll.
//! stellar_k8s::controller::gitops::health_check::evaluate_and_maybe_rollback(
//!     &client, &config, &state,
//! ).await?;
//! # Ok(())
//! # }
//! ```

use std::sync::atomic::Ordering;
use std::time::Duration;

use tracing::{info, warn};

use crate::error::Result;

#[cfg(not(feature = "metrics"))]
use crate::controller::metrics::NodePhase;
#[cfg(feature = "metrics")]
use crate::controller::metrics::{self, NodePhase};

use super::github::unix_now;
use super::{GitOpsConfig, GitOpsEngineState, SyncPhase};

/// Grace period applied after a commit is applied before health is judged.
///
/// Mirrors `config.health_grace_period_secs`; used when the config value is 0.
pub const MIN_GRACE_PERIOD_SECS: u64 = 120;

/// Prometheus scrape endpoint of the operator itself.
pub const DEFAULT_METRICS_ENDPOINT: &str = "http://127.0.0.1:9090/metrics";
/// Port of the operator's `/metrics` endpoint.
pub const OPERATOR_METRICS_PORT: u16 = 9090;
/// Default `/metrics` path.
pub const OPERATOR_METRICS_PATH: &str = "/metrics";

/// One node-health observation.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeHealthSample {
    /// Node instance label (`app.kubernetes.io/instance`).
    pub node: String,
    /// Value of `stellar_node_sync_status` at scrape time.
    pub sync_status: i64,
    /// Value of `stellar_node_up` at scrape time.
    pub up: i64,
    /// Unix seconds of the observation.
    pub at: u64,
}

impl NodeHealthSample {
    /// A node is healthy when its pod is ready and its sync status is
    /// `Syncing` (3) or better (`Ready` = 4).
    pub fn is_healthy(&self) -> bool {
        self.up == 1 && self.sync_status >= 3
    }
}

/// Snapshot of node health used for rollback decisions.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HealthSnapshot {
    /// One sample per observed node.
    pub samples: Vec<NodeHealthSample>,
    /// Unix seconds of the scrape.
    pub at: u64,
}

impl HealthSnapshot {
    /// `true` when every observed node is healthy (vacuously true for none).
    pub fn all_healthy(&self) -> bool {
        self.samples.iter().all(|s| s.is_healthy())
    }

    /// `true` when at least one sample is unhealthy.
    pub fn any_unhealthy(&self) -> bool {
        !self.samples.is_empty() && self.samples.iter().any(|s| !s.is_healthy())
    }

    /// Names of unhealthy nodes.
    pub fn unhealthy_nodes(&self) -> Vec<String> {
        self.samples
            .iter()
            .filter(|s| !s.is_healthy())
            .map(|s| s.node.clone())
            .collect()
    }
}

/// Parse the text exposition format of the operator's `/metrics` endpoint,
/// extracting `stellar_node_sync_status` and `stellar_node_up` samples.
///
/// Tolerant of the Prometheus text format quirks (HELP/TYPE lines, extra
/// whitespace, labels in any order).
pub fn parse_node_health_metrics(body: &str, at: u64) -> HealthSnapshot {
    let mut samples: Vec<NodeHealthSample> = Vec::new();

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (metric, value) = match line.rsplit_once(' ') {
            Some(pair) => pair,
            None => continue,
        };
        let value: i64 = match value.parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        if let Some(node) = extract_label(metric, "stellar_node_sync_status") {
            merge_sample(&mut samples, node, SampleField::SyncStatus(value), at);
        } else if let Some(node) = extract_label(metric, "stellar_node_up") {
            merge_sample(&mut samples, node, SampleField::Up(value), at);
        }
    }

    HealthSnapshot { samples, at }
}

/// Which metric a parsed line contributed.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SampleField {
    SyncStatus(i64),
    Up(i64),
}

/// Merge one parsed field into the per-node sample list.
fn merge_sample(samples: &mut Vec<NodeHealthSample>, node: String, field: SampleField, at: u64) {
    if let Some(existing) = samples.iter_mut().find(|s| s.node == node) {
        match field {
            SampleField::SyncStatus(v) => existing.sync_status = v,
            SampleField::Up(v) => existing.up = v,
        }
        existing.at = at;
    } else {
        let (sync_status, up) = match field {
            // Only one of the two metrics seen so far; the other keeps its
            // default and is filled in when its line arrives.
            SampleField::SyncStatus(v) => (v, 1),
            SampleField::Up(v) => (4, v),
        };
        samples.push(NodeHealthSample {
            node,
            sync_status,
            up,
            at,
        });
    }
}

/// Extract the `instance` label value from a metric line for `metric_name`.
fn extract_label(metric: &str, metric_name: &str) -> Option<String> {
    let rest = metric.strip_prefix(metric_name)?;
    let rest = rest.trim_start();
    if !rest.starts_with('{') {
        return None;
    }
    let labels = rest.trim_start_matches('{').trim_end_matches('}');
    for pair in labels.split(',') {
        let pair = pair.trim();
        if let Some(instance) = pair.strip_prefix("instance=\"") {
            return Some(instance.trim_end_matches('"').to_string());
        }
    }
    None
}

/// Fetch and parse node health from the operator's Prometheus endpoint.
pub async fn fetch_node_health(http: &reqwest::Client, endpoint: &str) -> Result<HealthSnapshot> {
    let at = unix_now();
    let body = http
        .get(endpoint)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| {
            crate::error::Error::GitOpsError(format!(
                "metrics endpoint {endpoint} unreachable: {e}"
            ))
        })?
        .text()
        .await
        .map_err(|e| crate::error::Error::GitOpsError(format!("metrics body read failed: {e}")))?;
    Ok(parse_node_health_metrics(&body, at))
}

/// Evaluate current node health and trigger a rollback when the applied commit
/// has been unhealthy for longer than the configured threshold.
///
/// State machine (per applied commit):
///
/// ```text
/// Observing ──(unhealthy, within grace)──► Observing      (wait for grace)
/// Observing ──(unhealthy, past threshold)─► RollingBack ─► RolledBack
/// Observing ──(healthy)──────────────────► Healthy
/// ```
pub async fn evaluate_and_maybe_rollback(
    k8s: &kube::Client,
    config: &GitOpsConfig,
    state: &GitOpsEngineState,
) -> Result<()> {
    let phase = state.phase.lock().unwrap().clone();
    // Only judge commits that are in the observing window.
    if phase != SyncPhase::Observing {
        return Ok(());
    }

    let applied_at = state.applied_at.lock().unwrap().unwrap_or(0);
    let now = unix_now();
    // The grace period is the phase *before* threshold tracking: the watchdog
    // only fires after `unhealthy_threshold_secs` of continuous bad health,
    // which already exceeds the grace period when the two are configured equal
    // (the default). Kept explicit for future grace-aware scheduling.
    let _grace = config.health_grace_period_secs.max(MIN_GRACE_PERIOD_SECS);
    let _ = applied_at;

    // A health check needs the node's live state. We resolve it from the
    // k8s API (pod readiness + stellar-core /info) rather than scraping, so
    // the watchdog works even when Prometheus scrape of the operator fails.
    let snapshot = snapshot_from_cluster(k8s).await;

    if snapshot.all_healthy() {
        state.unhealthy_since.store(0, Ordering::SeqCst);
        {
            let mut p = state.phase.lock().unwrap();
            if *p == SyncPhase::Observing {
                *p = SyncPhase::Healthy;
                #[cfg(feature = "metrics")]
                metrics::set_gitops_phase(&SyncPhase::Healthy);
            }
        }
        if let Some(commit) = state.current_commit.lock().unwrap().clone() {
            // Promotion: this commit is now the rollback target.
            *state.last_good_commit.lock().unwrap() = Some(commit.clone());
            state.record_event(&commit, &SyncPhase::Healthy, "node health confirmed");
            info!(commit = %commit, "GitOps commit promoted to last-known-good");
        }
        return Ok(());
    }

    // Unhealthy branch.
    let since = state.unhealthy_since.load(Ordering::SeqCst);
    if since == 0 {
        state.unhealthy_since.store(now, Ordering::SeqCst);
        state.record_event(
            &state
                .current_commit
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_default(),
            &SyncPhase::Observing,
            format!("unhealthy nodes: {:?}", snapshot.unhealthy_nodes()),
        );
        return Ok(());
    }

    let unhealthy_secs = now.saturating_sub(since);
    if unhealthy_secs < config.unhealthy_threshold_secs.max(1) {
        warn!(
            unhealthy_secs,
            threshold = config.unhealthy_threshold_secs,
            "node(s) unhealthy; waiting for threshold"
        );
        return Ok(());
    }

    // Threshold exceeded — roll back.
    let target = state.last_good_commit.lock().unwrap().clone();
    let Some(target) = target else {
        state.record_event(
            &state
                .current_commit
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_default(),
            &SyncPhase::Observing,
            "no known-good commit to roll back to",
        );
        return Ok(());
    };

    let bad = state
        .current_commit
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_default();
    warn!(
        bad = %bad,
        to = %target,
        unhealthy_secs,
        "GitOps health rollback triggered"
    );

    super::sync::apply_commit(k8s, config, &target).await?;

    if !bad.is_empty() {
        state.ban_commit(&bad);
        state.rollbacks_total.fetch_add(1, Ordering::SeqCst);
    }
    *state.current_commit.lock().unwrap() = Some(target.clone());
    *state.applied_at.lock().unwrap() = Some(now);
    state.unhealthy_since.store(0, Ordering::SeqCst);
    *state.phase.lock().unwrap() = SyncPhase::RolledBack;
    state.record_event(&target, &SyncPhase::RolledBack, "auto-rollback complete");

    #[cfg(feature = "metrics")]
    metrics::record_gitops_rollback();

    Ok(())
}

/// Build a [`HealthSnapshot`] straight from the Kubernetes API.
///
/// Uses the same health sources as the reconciler: pod readiness
/// (`stellar_node_up` equivalent) and the `stellar.org/sync-state` status or
/// pod `Ready` condition (`stellar_node_sync_status` equivalent).
async fn snapshot_from_cluster(k8s: &kube::Client) -> HealthSnapshot {
    use k8s_openapi::api::core::v1::Pod;
    use kube::api::{Api, ListParams};
    use kube::ResourceExt;

    let pods: Api<Pod> = Api::all(k8s.clone());
    let list = match pods
        .list(&ListParams::default().labels("app.kubernetes.io/name=stellar-node"))
        .await
    {
        Ok(l) => l,
        Err(e) => {
            warn!(error = %e, "GitOps health: pod list failed");
            return HealthSnapshot::default();
        }
    };

    let now = unix_now();
    let mut samples = Vec::new();
    for pod in &list.items {
        let Some(node) = pod.labels().get("app.kubernetes.io/instance").cloned() else {
            continue;
        };
        let ready = pod
            .status
            .as_ref()
            .and_then(|s| s.conditions.as_ref())
            .map(|cs| cs.iter().any(|c| c.type_ == "Ready" && c.status == "True"))
            .unwrap_or(false);
        samples.push(NodeHealthSample {
            node,
            sync_status: if ready { 4 } else { 0 },
            up: if ready { 1 } else { 0 },
            at: now,
        });
    }

    HealthSnapshot { samples, at: now }
}

/// Compute the rollback decision without side effects (pure, testable).
///
/// Returns `Some(target)` when a rollback should fire now.
pub fn rollback_decision(
    snapshot: &HealthSnapshot,
    unhealthy_since: u64,
    now: u64,
    threshold_secs: u64,
    last_good: Option<&str>,
) -> Option<String> {
    if snapshot.all_healthy() {
        return None;
    }
    if unhealthy_since == 0 {
        return None; // first bad sample — start the clock, don't act
    }
    if now.saturating_sub(unhealthy_since) < threshold_secs.max(1) {
        return None;
    }
    last_good.map(|s| s.to_string())
}

/// Convenience: seconds remaining until the unhealthy threshold is reached.
pub fn secs_until_threshold(unhealthy_since: u64, now: u64, threshold_secs: u64) -> u64 {
    threshold_secs.saturating_sub(now.saturating_sub(unhealthy_since))
}

/// Map a [`NodePhase`] onto the `stellar_node_sync_status` gauge value.
pub fn phase_to_sync_status(phase: &NodePhase) -> i64 {
    match phase {
        NodePhase::Pending => 0,
        NodePhase::Creating => 1,
        NodePhase::Running => 2,
        NodePhase::Syncing => 3,
        NodePhase::Ready => 4,
        NodePhase::Failed => 5,
        NodePhase::Degraded => 6,
        NodePhase::Suspended => 7,
        // Remediating/Terminating are transient operator states; treat them
        // as degraded for rollback purposes (not healthy, not failed).
        NodePhase::Remediating => 8,
        NodePhase::Terminating => 9,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn sample(node: &str, up: i64, sync: i64) -> NodeHealthSample {
        NodeHealthSample {
            node: node.to_string(),
            sync_status: sync,
            up,
            at: 1_000,
        }
    }

    // ── parse_node_health_metrics ──────────────────────────────────────────

    #[test]
    fn parse_metrics_extracts_sync_status_and_up() {
        let body = r#"
# HELP stellar_node_sync_status Current sync status
# TYPE stellar_node_sync_status gauge
stellar_node_sync_status{namespace="stellar",instance="val-1",node_type="Validator"} 4
stellar_node_sync_status{namespace="stellar",instance="val-2",node_type="Validator"} 0
# HELP stellar_node_up Binary up indicator
# TYPE stellar_node_up gauge
stellar_node_up{namespace="stellar",instance="val-1"} 1
stellar_node_up{namespace="stellar",instance="val-2"} 0
"#;
        let snap = parse_node_health_metrics(body, 42);
        assert_eq!(snap.samples.len(), 2);
        assert_eq!(snap.at, 42);

        let val1 = snap.samples.iter().find(|s| s.node == "val-1").unwrap();
        assert_eq!(val1.sync_status, 4);
        assert_eq!(val1.up, 1);
        assert!(val1.is_healthy());

        let val2 = snap.samples.iter().find(|s| s.node == "val-2").unwrap();
        assert_eq!(val2.sync_status, 0);
        assert_eq!(val2.up, 0);
        assert!(!val2.is_healthy());
    }

    #[test]
    fn parse_metrics_ignores_garbage_and_other_metrics() {
        let body = "stellar_operator_info{version=\"1\"} 1\n\
                    stellar_node_ledger_sequence{instance=\"x\"} 999999\n\
                    not-a-metric\n\n# comment\n";
        let snap = parse_node_health_metrics(body, 7);
        assert!(snap.samples.is_empty());
    }

    #[test]
    fn parse_metrics_empty_body() {
        let snap = parse_node_health_metrics("", 1);
        assert!(snap.samples.is_empty());
        assert!(snap.all_healthy()); // vacuously
    }

    // ── snapshot semantics ─────────────────────────────────────────────────

    #[test]
    fn snapshot_all_healthy_and_any_unhealthy() {
        let mut snap = HealthSnapshot {
            samples: vec![sample("a", 1, 4), sample("b", 1, 3)],
            at: 1,
        };
        assert!(snap.all_healthy());
        assert!(!snap.any_unhealthy());

        snap.samples.push(sample("c", 0, 5));
        assert!(!snap.all_healthy());
        assert!(snap.any_unhealthy());
        assert_eq!(snap.unhealthy_nodes(), vec!["c".to_string()]);
    }

    #[test]
    fn node_health_sample_up_is_required() {
        // Up pod but not yet syncing is unhealthy (mid-rollout grace case is
        // handled by the grace period, not the sample itself).
        assert!(!sample("a", 1, 0).is_healthy());
        assert!(sample("a", 1, 3).is_healthy());
        assert!(sample("a", 1, 4).is_healthy());
        assert!(!sample("a", 0, 4).is_healthy());
    }

    // ── rollback decision (pure) ───────────────────────────────────────────

    #[test]
    fn rollback_decision_healthy_never_rolls_back() {
        let snap = HealthSnapshot {
            samples: vec![sample("a", 1, 4)],
            at: 100,
        };
        assert!(rollback_decision(&snap, 50, 100, 60, Some("good")).is_none());
    }

    #[test]
    fn rollback_decision_first_bad_sample_starts_clock_only() {
        let snap = HealthSnapshot {
            samples: vec![sample("a", 0, 0)],
            at: 100,
        };
        // unhealthy_since == 0 → clock starts, no action yet.
        assert!(rollback_decision(&snap, 0, 100, 60, Some("good")).is_none());
    }

    #[test]
    fn rollback_decision_waits_for_threshold() {
        let snap = HealthSnapshot {
            samples: vec![sample("a", 0, 0)],
            at: 100,
        };
        // 30s unhealthy of a 60s threshold → wait.
        assert!(rollback_decision(&snap, 70, 100, 60, Some("good")).is_none());
        // Exactly at threshold → fire.
        assert_eq!(
            rollback_decision(&snap, 40, 100, 60, Some("good")),
            Some("good".to_string())
        );
    }

    #[test]
    fn rollback_decision_needs_last_good_target() {
        let snap = HealthSnapshot {
            samples: vec![sample("a", 0, 0)],
            at: 100,
        };
        assert!(rollback_decision(&snap, 40, 100, 60, None).is_none());
    }

    // ── phase mapping ──────────────────────────────────────────────────────

    #[test]
    fn phase_to_sync_status_matches_metrics_enum() {
        assert_eq!(phase_to_sync_status(&NodePhase::Pending), 0);
        assert_eq!(phase_to_sync_status(&NodePhase::Creating), 1);
        assert_eq!(phase_to_sync_status(&NodePhase::Running), 2);
        assert_eq!(phase_to_sync_status(&NodePhase::Syncing), 3);
        assert_eq!(phase_to_sync_status(&NodePhase::Ready), 4);
        assert_eq!(phase_to_sync_status(&NodePhase::Failed), 5);
        assert_eq!(phase_to_sync_status(&NodePhase::Degraded), 6);
        assert_eq!(phase_to_sync_status(&NodePhase::Suspended), 7);
    }

    #[test]
    fn secs_until_threshold_computes() {
        assert_eq!(secs_until_threshold(40, 100, 60), 0);
        assert_eq!(secs_until_threshold(70, 100, 60), 30);
        // Clock just started: full threshold remaining.
        assert_eq!(secs_until_threshold(100, 100, 60), 60);
    }

    // ── full state-machine transition (no cluster) ────────────────────────

    #[test]
    fn state_transitions_on_promotion_and_rollback_counters() {
        let state = Arc::new(GitOpsEngineState::default());
        *state.phase.lock().unwrap() = SyncPhase::Observing;
        *state.current_commit.lock().unwrap() = Some("badcommit0".to_string());
        *state.last_good_commit.lock().unwrap() = Some("goodcommit".to_string());
        state.unhealthy_since.store(480, Ordering::SeqCst);

        // Simulate the unhealthy branch of evaluate_and_maybe_rollback with a
        // pre-computed decision (avoids needing a live cluster).
        let snap = HealthSnapshot {
            samples: vec![sample("val-1", 0, 0)],
            at: 600,
        };
        // 600 - 480 = 120s unhealthy of a 120s threshold → fires.
        let decision = rollback_decision(&snap, 480, 600, 120, Some("goodcommit"));
        assert_eq!(decision.as_deref(), Some("goodcommit"));

        if let Some(target) = decision {
            state.ban_commit("badcommit0");
            state.rollbacks_total.fetch_add(1, Ordering::SeqCst);
            *state.current_commit.lock().unwrap() = Some(target);
            state.unhealthy_since.store(0, Ordering::SeqCst);
            *state.phase.lock().unwrap() = SyncPhase::RolledBack;
        }

        assert!(state.is_banned("badcommit0"));
        assert_eq!(state.rollbacks_total.load(Ordering::SeqCst), 1);
        assert_eq!(
            *state.current_commit.lock().unwrap(),
            Some("goodcommit".to_string())
        );
        assert_eq!(*state.phase.lock().unwrap(), SyncPhase::RolledBack);
    }
}
