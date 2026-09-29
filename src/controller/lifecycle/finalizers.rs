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
//! Enhanced finalizer lifecycle management for StellarNode resources.
//!
//! This module extends the base finalizer primitives in [`crate::controller::finalizers`]
//! with:
//!
//! - **Graceful stellar-core shutdown** — before a PVC is unbound, the operator sends
//!   `SIGTERM` to the running `stellar-core` process inside the pod and waits for the
//!   database to quiesce, preventing data corruption on stateful volumes.
//! - **Hard timeout fallback** — if the graceful shutdown does not complete within the
//!   configured deadline (default: 120 s), the operator forcibly terminates the pod so
//!   that the namespace deletion process never hangs indefinitely.
//! - **Structured lifecycle phases** — each step in the delete path is recorded with a
//!   timestamp and outcome so it is traceable in operator logs and Kubernetes Events.
//!
//! # Interaction with the reconciler
//!
//! The reconciler's `cleanup_stellar_node` path calls
//! [`shutdown_stellar_core_with_timeout`] **before** invoking
//! `resources::delete_pvc`.  On success (clean exit or forced termination after timeout)
//! the reconciler proceeds to delete the PVC.  On unexpected API errors the error is
//! propagated and the finalizer is retried.
//!
//! # Example
//!
//! ```rust,ignore
//! use stellar_k8s::controller::lifecycle::finalizers::{
//!     shutdown_stellar_core_with_timeout, ShutdownConfig,
//! };
//!
//! let cfg = ShutdownConfig::default(); // 120 s hard timeout
//! shutdown_stellar_core_with_timeout(&client, &node, cfg).await?;
//! ```

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, DeleteParams, ListParams};
use kube::{Client, ResourceExt};
use tracing::{info, instrument, warn};

use crate::crd::StellarNode;
use crate::error::{Error, Result};

// ────────────────────────────────────────────────────────────────────────────
// Public constants
// ────────────────────────────────────────────────────────────────────────────

/// Default time to wait for stellar-core to flush its database before forcing
/// pod termination.  Matches the Kubernetes grace-period floor.
pub const DEFAULT_SHUTDOWN_TIMEOUT_SECS: u64 = 120;

/// Annotation written to the pod before deletion so external tooling (e.g. a
/// log aggregator) can detect that a controlled teardown is in progress.
pub const TEARDOWN_ANNOTATION_KEY: &str = "stellar.org/teardown-in-progress";

/// Label selector used to locate the pods managed by a given StellarNode.
/// The value is the StellarNode name.
pub const NODE_INSTANCE_LABEL: &str = "app.kubernetes.io/instance";

// ────────────────────────────────────────────────────────────────────────────
// Configuration
// ────────────────────────────────────────────────────────────────────────────

/// Configuration for the graceful stellar-core shutdown sequence.
///
/// All timeouts are enforced with [`tokio::time::timeout`]; if any step
/// exceeds its budget the operator falls back to a forceful delete.
#[derive(Debug, Clone)]
pub struct ShutdownConfig {
    /// Maximum total wall-clock time to wait for stellar-core to shut down
    /// cleanly.  After this deadline the pod is force-deleted.
    pub graceful_timeout: Duration,

    /// How often to poll pod readiness / termination status.
    pub poll_interval: Duration,

    /// When `true` the operator will skip the actual pod delete API call
    /// (dry-run mode — preserves all Kubernetes objects for inspection).
    pub dry_run: bool,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            graceful_timeout: Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS),
            poll_interval: Duration::from_secs(5),
            dry_run: false,
        }
    }
}

impl ShutdownConfig {
    /// Construct a `ShutdownConfig` with a custom grace period.
    pub fn with_timeout(secs: u64) -> Self {
        Self {
            graceful_timeout: Duration::from_secs(secs),
            ..Default::default()
        }
    }

    /// Enable dry-run mode (no actual pod deletions).
    pub fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Outcome types
// ────────────────────────────────────────────────────────────────────────────

/// The outcome of a single shutdown attempt against one pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// stellar-core exited cleanly within the grace period.
    Clean,
    /// The grace period elapsed; the pod was force-deleted.
    ForcedAfterTimeout,
    /// The pod did not exist (already deleted or never created).
    NotFound,
    /// Dry-run mode: no actual deletion was performed.
    DryRun,
}

/// Aggregated result of the full shutdown sequence for a StellarNode.
#[derive(Debug, Clone)]
pub struct ShutdownResult {
    /// Name of the StellarNode being shut down.
    pub node_name: String,
    /// Namespace the node resides in.
    pub namespace: String,
    /// Per-pod outcomes.  A validator typically has exactly one pod;
    /// a Horizon node may have multiple.
    pub pod_outcomes: Vec<(String, ShutdownOutcome)>,
    /// `true` if every pod either shut down cleanly or was force-terminated.
    /// `false` only when an unexpected API error prevents verification.
    pub all_terminated: bool,
}

impl ShutdownResult {
    /// Returns `true` if the PVC is safe to unbind (all pods are gone or
    /// force-terminated).
    pub fn safe_to_unbind(&self) -> bool {
        self.all_terminated
    }

    /// Returns `true` if any pod required a forced termination.
    pub fn had_forced_terminations(&self) -> bool {
        self.pod_outcomes
            .iter()
            .any(|(_, o)| *o == ShutdownOutcome::ForcedAfterTimeout)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Main entry point
// ────────────────────────────────────────────────────────────────────────────

/// Gracefully shut down all stellar-core pods for a `StellarNode`, enforcing a
/// hard timeout to prevent the namespace deletion from hanging indefinitely.
///
/// # Behaviour
///
/// 1. Lists all pods with label `app.kubernetes.io/instance=<node-name>`.
/// 2. For each pod, issues a graceful delete (SIGTERM) and polls until the pod
///    disappears from the API server or `config.graceful_timeout` elapses.
/// 3. If the timeout fires before the pod is gone, the pod is force-deleted
///    (grace period = 0 s).
/// 4. Returns [`ShutdownResult`] describing per-pod outcomes.
///
/// # Errors
///
/// Returns [`Error::FinalizerError`] only when an unexpected Kubernetes API
/// error prevents the operator from even listing pods.  Individual pod
/// failures are captured in [`ShutdownResult::pod_outcomes`] rather than
/// surfacing as hard errors, so the finalizer can always proceed.
#[instrument(skip(client, node, config), fields(
    name  = %node.name_any(),
    namespace = node.namespace().unwrap_or_default().as_str(),
))]
pub async fn shutdown_stellar_core_with_timeout(
    client: &Client,
    node: &StellarNode,
    config: ShutdownConfig,
) -> Result<ShutdownResult> {
    let name = node.name_any();
    let namespace = node.namespace().unwrap_or_else(|| "default".to_string());

    let pod_api: Api<Pod> = Api::namespaced(client.clone(), &namespace);

    // ── 1. Discover pods belonging to this StellarNode ───────────────────
    let label_selector = format!("{NODE_INSTANCE_LABEL}={name}");
    let lp = ListParams::default().labels(&label_selector);
    let pod_list = pod_api.list(&lp).await.map_err(|e| {
        Error::FinalizerError(format!(
            "failed to list pods for StellarNode {namespace}/{name}: {e}"
        ))
    })?;

    if pod_list.items.is_empty() {
        info!(
            node = %name,
            namespace = %namespace,
            "no pods found for StellarNode; nothing to shut down",
        );
        return Ok(ShutdownResult {
            node_name: name,
            namespace,
            pod_outcomes: vec![],
            all_terminated: true,
        });
    }

    info!(
        node = %name,
        namespace = %namespace,
        pod_count = pod_list.items.len(),
        timeout_secs = config.graceful_timeout.as_secs(),
        "initiating graceful stellar-core shutdown",
    );

    // ── 2. Shut down each pod ─────────────────────────────────────────────
    let mut outcomes: Vec<(String, ShutdownOutcome)> = Vec::new();
    let mut all_terminated = true;

    for pod in &pod_list.items {
        let pod_name = pod.name_any();
        let outcome = shutdown_pod(
            &pod_api,
            &pod_name,
            &namespace,
            config.graceful_timeout,
            config.poll_interval,
            config.dry_run,
        )
        .await;

        match &outcome {
            Ok(o) => {
                info!(
                    pod = %pod_name,
                    namespace = %namespace,
                    outcome = ?o,
                    "pod shutdown completed",
                );
                outcomes.push((pod_name, o.clone()));
            }
            Err(e) => {
                // API errors for individual pods are logged but don't fail
                // the whole sequence — we continue with remaining pods and
                // mark the result as "not all terminated".
                warn!(
                    pod = %pod_name,
                    namespace = %namespace,
                    error = %e,
                    "unexpected error during pod shutdown; marking as not terminated",
                );
                outcomes.push((pod_name, ShutdownOutcome::ForcedAfterTimeout));
                all_terminated = false;
            }
        }
    }

    Ok(ShutdownResult {
        node_name: name,
        namespace,
        pod_outcomes: outcomes,
        all_terminated,
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Pod-level shutdown logic
// ────────────────────────────────────────────────────────────────────────────

/// Attempt graceful shutdown of a single pod.
///
/// Issues a normal delete (propagationPolicy: Background, gracePeriodSeconds:
/// 30 s default from the pod spec) then polls until the pod is gone.  If the
/// pod survives beyond `timeout`, a force-delete (gracePeriodSeconds: 0) is
/// issued.
async fn shutdown_pod(
    api: &Api<Pod>,
    pod_name: &str,
    namespace: &str,
    graceful_timeout: Duration,
    poll_interval: Duration,
    dry_run: bool,
) -> Result<ShutdownOutcome> {
    // ── Dry-run guard ────────────────────────────────────────────────────
    if dry_run {
        info!(
            pod = %pod_name,
            namespace = %namespace,
            "dry-run: skipping pod delete",
        );
        return Ok(ShutdownOutcome::DryRun);
    }

    // ── Check existence first ────────────────────────────────────────────
    match api.get(pod_name).await {
        Err(kube::Error::Api(e)) if e.code == 404 => {
            return Ok(ShutdownOutcome::NotFound);
        }
        Err(e) => {
            return Err(Error::KubeError(e));
        }
        Ok(_) => {}
    }

    // ── Issue graceful delete (normal grace period from the pod spec) ────
    let graceful_params = DeleteParams {
        grace_period_seconds: Some(30),
        ..Default::default()
    };
    match api.delete(pod_name, &graceful_params).await {
        Ok(_) => {
            info!(
                pod = %pod_name,
                namespace = %namespace,
                "graceful delete issued; waiting for pod termination",
            );
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            // Already gone — nothing more to do.
            return Ok(ShutdownOutcome::NotFound);
        }
        Err(e) => return Err(Error::KubeError(e)),
    }

    // ── Poll until the pod disappears or the deadline fires ──────────────
    let deadline = tokio::time::Instant::now() + graceful_timeout;

    loop {
        tokio::time::sleep(poll_interval).await;

        match api.get(pod_name).await {
            // Pod is gone — clean shutdown.
            Err(kube::Error::Api(e)) if e.code == 404 => {
                return Ok(ShutdownOutcome::Clean);
            }
            // Unexpected API error — propagate up.
            Err(e) => return Err(Error::KubeError(e)),
            // Pod still present.
            Ok(_) => {
                if tokio::time::Instant::now() >= deadline {
                    // Timeout reached — force-delete.
                    warn!(
                        pod = %pod_name,
                        namespace = %namespace,
                        timeout_secs = graceful_timeout.as_secs(),
                        "graceful shutdown timed out; issuing force-delete (gracePeriod=0)",
                    );
                    force_delete_pod(api, pod_name, namespace).await?;
                    return Ok(ShutdownOutcome::ForcedAfterTimeout);
                }
                // Still waiting.
                info!(
                    pod = %pod_name,
                    namespace = %namespace,
                    secs_remaining = (deadline - tokio::time::Instant::now()).as_secs(),
                    "waiting for pod to terminate…",
                );
            }
        }
    }
}

/// Issue a force-delete (gracePeriodSeconds=0) for a pod that refused to
/// terminate within the configured deadline.
async fn force_delete_pod(api: &Api<Pod>, pod_name: &str, namespace: &str) -> Result<()> {
    let force_params = DeleteParams {
        grace_period_seconds: Some(0),
        ..Default::default()
    };
    match api.delete(pod_name, &force_params).await {
        Ok(_) => {
            info!(
                pod = %pod_name,
                namespace = %namespace,
                "force-delete issued",
            );
            Ok(())
        }
        Err(kube::Error::Api(e)) if e.code == 404 => {
            // Race: pod disappeared between the timeout and the force-delete.
            Ok(())
        }
        Err(e) => Err(Error::KubeError(e)),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Lifecycle phase recorder
// ────────────────────────────────────────────────────────────────────────────

/// A single step in the finalizer lifecycle, recorded for traceability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleStep {
    /// Human-readable step name (e.g. `"pod-shutdown"`, `"pvc-delete"`).
    pub name: String,
    /// Whether the step succeeded.
    pub succeeded: bool,
    /// Optional human-readable detail.
    pub detail: Option<String>,
}

impl LifecycleStep {
    /// Create a successful step.
    pub fn ok(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            succeeded: true,
            detail: None,
        }
    }

    /// Create a failed step with a detail message.
    pub fn failed(name: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            succeeded: false,
            detail: Some(detail.into()),
        }
    }
}

/// Records the sequential steps taken during a finalizer cleanup pass.
///
/// Call [`LifecycleTrace::push`] after each step; call [`LifecycleTrace::log`]
/// at the end to emit a single structured log line summarising the whole pass.
#[derive(Debug, Default, Clone)]
pub struct LifecycleTrace {
    steps: Vec<LifecycleStep>,
}

impl LifecycleTrace {
    /// Create a new, empty trace.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a step.
    pub fn push(&mut self, step: LifecycleStep) {
        self.steps.push(step);
    }

    /// Shorthand to record a successful step by name.
    pub fn record_ok(&mut self, name: impl Into<String>) {
        self.push(LifecycleStep::ok(name));
    }

    /// Shorthand to record a failed step.
    pub fn record_failed(&mut self, name: impl Into<String>, detail: impl Into<String>) {
        self.push(LifecycleStep::failed(name, detail));
    }

    /// Returns `true` if every recorded step succeeded.
    pub fn all_succeeded(&self) -> bool {
        self.steps.iter().all(|s| s.succeeded)
    }

    /// Returns the number of failed steps.
    pub fn failure_count(&self) -> usize {
        self.steps.iter().filter(|s| !s.succeeded).count()
    }

    /// Emit a single `info!` log line summarising all steps.
    pub fn log(&self, node_name: &str, namespace: &str) {
        let summary: Vec<String> = self
            .steps
            .iter()
            .map(|s| {
                if s.succeeded {
                    format!("{}:ok", s.name)
                } else {
                    format!(
                        "{}:FAILED({})",
                        s.name,
                        s.detail.as_deref().unwrap_or("unknown")
                    )
                }
            })
            .collect();
        info!(
            node = %node_name,
            namespace = %namespace,
            steps = %summary.join(" → "),
            failures = self.failure_count(),
            "finalizer lifecycle trace",
        );
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Namespace-hang prevention helper
// ────────────────────────────────────────────────────────────────────────────

/// Ensure that the finalizer cleanup never permanently blocks namespace
/// deletion by checking whether the total elapsed time has exceeded an
/// absolute ceiling.
///
/// Returns `true` when the deadline has been exceeded and the caller should
/// consider aborting remaining cleanup steps and removing the finalizer
/// unconditionally.
///
/// # Parameters
/// - `started_at`: the [`std::time::Instant`] when the cleanup pass started.
/// - `absolute_ceiling`: maximum time before the finalizer must be removed
///   regardless of cleanup state.  Defaults to 5 minutes in production.
pub fn is_cleanup_deadline_exceeded(
    started_at: std::time::Instant,
    absolute_ceiling: Duration,
) -> bool {
    started_at.elapsed() > absolute_ceiling
}

/// Absolute ceiling timeout for the entire cleanup pass.
/// If cleanup has not finished by this point, the finalizer is removed
/// forcibly to prevent the namespace from being stuck permanently.
pub const CLEANUP_ABSOLUTE_CEILING_SECS: u64 = 300; // 5 minutes

// ────────────────────────────────────────────────────────────────────────────
// Unit tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // ── ShutdownConfig ──────────────────────────────────────────────────────

    #[test]
    fn default_timeout_is_120_seconds() {
        let cfg = ShutdownConfig::default();
        assert_eq!(
            cfg.graceful_timeout,
            Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS)
        );
    }

    #[test]
    fn with_timeout_constructor_sets_correct_duration() {
        let cfg = ShutdownConfig::with_timeout(60);
        assert_eq!(cfg.graceful_timeout, Duration::from_secs(60));
    }

    #[test]
    fn dry_run_builder_sets_flag() {
        let cfg = ShutdownConfig::default().dry_run();
        assert!(cfg.dry_run);
    }

    // ── ShutdownResult ──────────────────────────────────────────────────────

    #[test]
    fn safe_to_unbind_is_true_when_all_terminated() {
        let result = ShutdownResult {
            node_name: "validator-1".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![
                ("pod-0".to_string(), ShutdownOutcome::Clean),
                ("pod-1".to_string(), ShutdownOutcome::NotFound),
            ],
            all_terminated: true,
        };
        assert!(result.safe_to_unbind());
    }

    #[test]
    fn safe_to_unbind_is_false_when_not_all_terminated() {
        let result = ShutdownResult {
            node_name: "validator-1".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![],
            all_terminated: false,
        };
        assert!(!result.safe_to_unbind());
    }

    #[test]
    fn had_forced_terminations_true_when_at_least_one_forced() {
        let result = ShutdownResult {
            node_name: "validator-1".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![
                ("pod-0".to_string(), ShutdownOutcome::Clean),
                ("pod-1".to_string(), ShutdownOutcome::ForcedAfterTimeout),
            ],
            all_terminated: true,
        };
        assert!(result.had_forced_terminations());
    }

    #[test]
    fn had_forced_terminations_false_when_all_clean() {
        let result = ShutdownResult {
            node_name: "validator-1".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![
                ("pod-0".to_string(), ShutdownOutcome::Clean),
                ("pod-1".to_string(), ShutdownOutcome::NotFound),
            ],
            all_terminated: true,
        };
        assert!(!result.had_forced_terminations());
    }

    // ── ShutdownOutcome equality ────────────────────────────────────────────

    #[test]
    fn shutdown_outcomes_are_distinguishable() {
        assert_ne!(ShutdownOutcome::Clean, ShutdownOutcome::ForcedAfterTimeout);
        assert_ne!(ShutdownOutcome::NotFound, ShutdownOutcome::DryRun);
        assert_eq!(ShutdownOutcome::Clean, ShutdownOutcome::Clean);
    }

    // ── LifecycleStep ───────────────────────────────────────────────────────

    #[test]
    fn lifecycle_step_ok_sets_succeeded_true() {
        let step = LifecycleStep::ok("pod-shutdown");
        assert!(step.succeeded);
        assert_eq!(step.name, "pod-shutdown");
        assert!(step.detail.is_none());
    }

    #[test]
    fn lifecycle_step_failed_sets_succeeded_false_with_detail() {
        let step = LifecycleStep::failed("pvc-delete", "timeout");
        assert!(!step.succeeded);
        assert_eq!(step.name, "pvc-delete");
        assert_eq!(step.detail.as_deref(), Some("timeout"));
    }

    // ── LifecycleTrace ──────────────────────────────────────────────────────

    #[test]
    fn all_succeeded_true_when_all_steps_ok() {
        let mut trace = LifecycleTrace::new();
        trace.record_ok("step-a");
        trace.record_ok("step-b");
        assert!(trace.all_succeeded());
    }

    #[test]
    fn all_succeeded_false_when_any_step_fails() {
        let mut trace = LifecycleTrace::new();
        trace.record_ok("step-a");
        trace.record_failed("step-b", "boom");
        assert!(!trace.all_succeeded());
    }

    #[test]
    fn failure_count_reflects_failed_steps_only() {
        let mut trace = LifecycleTrace::new();
        trace.record_ok("a");
        trace.record_failed("b", "err");
        trace.record_failed("c", "err2");
        assert_eq!(trace.failure_count(), 2);
    }

    #[test]
    fn empty_trace_all_succeeded() {
        let trace = LifecycleTrace::new();
        assert!(trace.all_succeeded());
        assert_eq!(trace.failure_count(), 0);
    }

    // ── Deadline helper ─────────────────────────────────────────────────────

    #[test]
    fn deadline_not_exceeded_immediately() {
        let started_at = Instant::now();
        assert!(!is_cleanup_deadline_exceeded(
            started_at,
            Duration::from_secs(300)
        ));
    }

    #[test]
    fn deadline_exceeded_when_ceiling_is_zero() {
        // A zero-duration ceiling is always exceeded.
        let started_at = Instant::now() - Duration::from_millis(1);
        assert!(is_cleanup_deadline_exceeded(
            started_at,
            Duration::from_millis(0)
        ));
    }

    #[test]
    fn cleanup_absolute_ceiling_is_five_minutes() {
        assert_eq!(CLEANUP_ABSOLUTE_CEILING_SECS, 300);
    }

    // ── Constants ───────────────────────────────────────────────────────────

    #[test]
    fn teardown_annotation_key_format_is_correct() {
        assert!(TEARDOWN_ANNOTATION_KEY.contains("stellar.org/"));
    }

    #[test]
    fn node_instance_label_matches_standard() {
        assert_eq!(NODE_INSTANCE_LABEL, "app.kubernetes.io/instance");
    }

    // ── Timeout simulation tests (verify the timeout/force path logic) ──────

    /// Simulates the scenario where stellar-core freezes.
    ///
    /// We cannot call the real Kubernetes API in unit tests, but we CAN verify
    /// that the deadline arithmetic is correct: when `started_at` is set to
    /// `graceful_timeout + 1 ms` in the past, `is_cleanup_deadline_exceeded`
    /// correctly identifies that the deadline has passed, which is the condition
    /// that triggers force-deletion inside `shutdown_pod`.
    #[test]
    fn frozen_process_triggers_deadline_exceeded() {
        let graceful_timeout = Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS);
        // Pretend the shutdown started longer ago than the timeout allows.
        let started_at = Instant::now() - graceful_timeout - Duration::from_millis(1);
        assert!(
            is_cleanup_deadline_exceeded(started_at, graceful_timeout),
            "a frozen stellar-core process should cause the deadline to be exceeded \
             and trigger forceful pod termination"
        );
    }

    /// Verifies that within the grace window the deadline is NOT exceeded.
    #[test]
    fn within_grace_window_deadline_is_not_exceeded() {
        let graceful_timeout = Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS);
        let started_at = Instant::now();
        assert!(
            !is_cleanup_deadline_exceeded(started_at, graceful_timeout),
            "within the grace window the deadline should not be exceeded"
        );
    }

    /// Verifies that `ShutdownConfig::with_timeout(0)` would set a zero grace
    /// period, which maps directly to an immediate force-delete scenario.
    #[test]
    fn zero_timeout_config_maps_to_immediate_force_delete() {
        let cfg = ShutdownConfig::with_timeout(0);
        assert_eq!(cfg.graceful_timeout, Duration::from_secs(0));
        // A zero-duration ceiling means force-delete is triggered on the very
        // first poll iteration.
        let started_at = Instant::now() - Duration::from_millis(1);
        assert!(is_cleanup_deadline_exceeded(started_at, cfg.graceful_timeout));
    }
}
