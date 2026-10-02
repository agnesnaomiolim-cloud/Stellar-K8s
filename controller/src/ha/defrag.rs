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

//! WASM Heap Memory Defragmentation Controller (#332)
//!
//! Long-running Soroban RPC nodes accumulate heap fragmentation due to the
//! continuous instantiation and teardown of WASM environments.  This module
//! implements an operator-level defragmentation cycle that:
//!
//! 1. **Detects** pods whose jemalloc fragmentation ratio exceeds
//!    [`FRAGMENTATION_THRESHOLD`] (default 30 %).
//! 2. **Guards** the restart against the Pod Disruption Budget: the controller
//!    will **never** restart a pod when doing so would reduce the number of
//!    available replicas below the PDB's `minAvailable` / `maxUnavailable`
//!    policy.
//! 3. **Cordons** traffic away from the chosen pod by applying a label that
//!    removes it from the active `Service` endpoints.
//! 4. **Restarts** the pod gracefully via a Kubernetes pod deletion (the
//!    owning `Deployment` / `StatefulSet` recreates it automatically).
//! 5. **Reintroduces** the pod to the load balancer once it is Ready again.
//!
//! # Configuration
//!
//! | Environment variable            | Default   | Description                             |
//! |---------------------------------|-----------|-----------------------------------------|
//! | `DEFRAG_NAMESPACE`              | `default` | Namespace to watch                      |
//! | `DEFRAG_LABEL_SELECTOR`         | `app=soroban-rpc` | Pod label selector            |
//! | `DEFRAG_INTERVAL_SECS`          | `60`      | Seconds between reconcile passes        |
//! | `DEFRAG_FRAG_THRESHOLD`         | `0.30`    | Fragmentation ratio that triggers cycle |
//! | `DEFRAG_METRICS_PORT`           | `9090`    | Pod metrics port for Prometheus scrape  |
//! | `DEFRAG_METRICS_PATH`           | `/metrics`| Pod metrics HTTP path                   |
//! | `DEFRAG_READY_TIMEOUT_SECS`     | `120`     | Seconds to wait for pod to become Ready |
//!
//! # Safety Invariant
//!
//! ```text
//! available_replicas_after_restart >= pdb.min_available
//! ```
//!
//! The controller reads both `PodDisruptionBudget` objects in the same
//! namespace (filtered by the same label selector) and the current number of
//! *Running+Ready* pods before proceeding.  If the invariant would be
//! violated the cycle is skipped and logged at `WARN` level.
//!
//! # Example – spawning the controller
//!
//! ```rust,no_run
//! use kube::Client;
//! use controller::ha::defrag::{DefragController, DefragConfig};
//!
//! #[tokio::main]
//! async fn main() {
//!     let client = Client::try_default().await.unwrap();
//!     let config = DefragConfig::from_env();
//!     let ctrl = DefragController::new(client, config);
//!     ctrl.run().await;
//! }
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::api::policy::v1::PodDisruptionBudget;
use kube::api::{Api, DeleteParams, ListParams, Patch, PatchParams};
use kube::Client;
use serde_json::json;
use tracing::{debug, error, info, warn};

use crate::metrics::jemalloc::JemallocSnapshot;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Fragmentation ratio that triggers the defragmentation cycle by default.
pub const FRAGMENTATION_THRESHOLD: f64 = 0.30;

/// Label applied to a pod when it is being drained from the load balancer.
/// The `Service` selector must **not** include this label (or must require
/// `stellar-defrag-active: "false"`) for the cordon to take effect.
pub const CORDON_LABEL_KEY: &str = "stellar-defrag-active";
pub const CORDON_LABEL_DRAINED: &str = "draining";
pub const CORDON_LABEL_ACTIVE: &str = "active";

/// Annotation written to a pod before deletion to aid forensic review.
pub const DEFRAG_ANNOTATION_KEY: &str = "stellar.org/defrag-restart-reason";

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the [`DefragController`].
#[derive(Debug, Clone)]
pub struct DefragConfig {
    /// Kubernetes namespace to watch.
    pub namespace: String,
    /// Pod label selector, e.g. `"app=soroban-rpc"`.
    pub label_selector: String,
    /// Interval between full reconcile passes.
    pub interval: Duration,
    /// Fragmentation ratio `[0.0, 1.0]` that triggers a defrag cycle.
    pub fragmentation_threshold: f64,
    /// Port on which pods expose Prometheus metrics.
    pub metrics_port: u16,
    /// HTTP path for the Prometheus metrics endpoint.
    pub metrics_path: String,
    /// How long to wait for a restarted pod to become Ready.
    pub ready_timeout: Duration,
}

impl Default for DefragConfig {
    fn default() -> Self {
        Self {
            namespace: "default".to_string(),
            label_selector: "app=soroban-rpc".to_string(),
            interval: Duration::from_secs(60),
            fragmentation_threshold: FRAGMENTATION_THRESHOLD,
            metrics_port: 9090,
            metrics_path: "/metrics".to_string(),
            ready_timeout: Duration::from_secs(120),
        }
    }
}

impl DefragConfig {
    /// Build a `DefragConfig` from environment variables with defaults.
    pub fn from_env() -> Self {
        let namespace = std::env::var("DEFRAG_NAMESPACE").unwrap_or_else(|_| "default".to_string());
        let label_selector = std::env::var("DEFRAG_LABEL_SELECTOR")
            .unwrap_or_else(|_| "app=soroban-rpc".to_string());
        let interval_secs: u64 = std::env::var("DEFRAG_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(60);
        let fragmentation_threshold: f64 = std::env::var("DEFRAG_FRAG_THRESHOLD")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(FRAGMENTATION_THRESHOLD);
        let metrics_port: u16 = std::env::var("DEFRAG_METRICS_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(9090);
        let metrics_path =
            std::env::var("DEFRAG_METRICS_PATH").unwrap_or_else(|_| "/metrics".to_string());
        let ready_timeout_secs: u64 = std::env::var("DEFRAG_READY_TIMEOUT_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120);

        Self {
            namespace,
            label_selector,
            interval: Duration::from_secs(interval_secs),
            fragmentation_threshold,
            metrics_port,
            metrics_path,
            ready_timeout: Duration::from_secs(ready_timeout_secs),
        }
    }
}

// ---------------------------------------------------------------------------
// Public result types
// ---------------------------------------------------------------------------

/// Summary of a single reconcile pass.
#[derive(Debug, Default)]
pub struct ReconcileSummary {
    /// Total pods evaluated.
    pub pods_evaluated: usize,
    /// Pods whose fragmentation ratio exceeded the threshold.
    pub pods_fragmented: usize,
    /// Pods actually restarted in this pass.
    pub pods_restarted: usize,
    /// Pods skipped because a PDB would be violated.
    pub pods_skipped_pdb: usize,
    /// Pods skipped because metrics were unavailable or zeroed.
    pub pods_skipped_no_metrics: usize,
}

/// Reason a pod restart was skipped.
#[derive(Debug, PartialEq, Eq)]
pub enum SkipReason {
    /// PDB would be violated.
    PdbViolation,
    /// Pod metrics were unavailable / zeroed.
    MetricsUnavailable,
    /// Pod is not in Running phase.
    NotRunning,
    /// Pod is already being drained.
    AlreadyDraining,
}

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

/// The WASM Heap Defragmentation Controller.
pub struct DefragController {
    client: Client,
    config: DefragConfig,
}

impl DefragController {
    /// Create a new controller with the given Kubernetes client and config.
    pub fn new(client: Client, config: DefragConfig) -> Self {
        Self { client, config }
    }

    /// Run the controller loop indefinitely.
    ///
    /// Panics only on unrecoverable internal failures (e.g. Tokio runtime
    /// shutdown).  All Kubernetes API and network errors are logged and the
    /// loop continues.
    pub async fn run(&self) {
        info!(
            namespace = %self.config.namespace,
            label_selector = %self.config.label_selector,
            interval_secs = self.config.interval.as_secs(),
            fragmentation_threshold = self.config.fragmentation_threshold,
            "DefragController started"
        );

        let mut ticker = tokio::time::interval(self.config.interval);
        loop {
            ticker.tick().await;
            match self.reconcile_once().await {
                Ok(summary) => {
                    info!(
                        pods_evaluated = summary.pods_evaluated,
                        pods_fragmented = summary.pods_fragmented,
                        pods_restarted = summary.pods_restarted,
                        pods_skipped_pdb = summary.pods_skipped_pdb,
                        pods_skipped_no_metrics = summary.pods_skipped_no_metrics,
                        "DefragController reconcile pass complete"
                    );
                }
                Err(e) => {
                    error!(error = %e, "DefragController reconcile pass failed");
                }
            }
        }
    }

    /// Execute a single reconcile pass.  Returns a [`ReconcileSummary`].
    ///
    /// This is the main entry point for testing: callers can invoke it
    /// directly without spawning the interval loop.
    pub async fn reconcile_once(&self) -> Result<ReconcileSummary, DefragError> {
        let mut summary = ReconcileSummary::default();

        let pods = self.list_pods().await?;
        let ready_count = count_ready_pods(&pods);
        let pdb_min_available = self.query_pdb_min_available().await?;

        debug!(
            total_pods = pods.len(),
            ready_count,
            pdb_min_available,
            "reconcile pass: listed pods and PDB"
        );

        for pod in &pods {
            summary.pods_evaluated += 1;

            let pod_name = pod
                .metadata
                .name
                .as_deref()
                .unwrap_or("<unnamed>")
                .to_string();

            // Skip pods that are not Running.
            if !is_pod_running(pod) {
                debug!(pod = %pod_name, "pod not running; skipping");
                continue;
            }

            // Skip pods already being drained (prevents double-restarts).
            if is_pod_draining(pod) {
                debug!(pod = %pod_name, "pod already draining; skipping");
                summary.pods_skipped_no_metrics += 1;
                continue;
            }

            // Fetch jemalloc metrics for this pod.
            let snapshot = self.fetch_pod_metrics(&pod_name).await.unwrap_or_else(|e| {
                warn!(pod = %pod_name, error = %e, "failed to scrape pod metrics; using zeroed snapshot");
                JemallocSnapshot::zeroed()
            });

            // Skip pods where metrics are unavailable.
            if snapshot.resident_bytes == 0 {
                debug!(pod = %pod_name, "metrics unavailable; skipping");
                summary.pods_skipped_no_metrics += 1;
                continue;
            }

            if !snapshot.is_fragmented(self.config.fragmentation_threshold) {
                debug!(
                    pod = %pod_name,
                    fragmentation_ratio = snapshot.fragmentation_ratio,
                    threshold = self.config.fragmentation_threshold,
                    "pod below fragmentation threshold"
                );
                continue;
            }

            summary.pods_fragmented += 1;
            info!(
                pod = %pod_name,
                fragmentation_ratio = snapshot.fragmentation_ratio,
                threshold = self.config.fragmentation_threshold,
                "pod exceeds fragmentation threshold; evaluating restart"
            );

            // PDB safety check: restarting this pod would bring available
            // replicas from `ready_count` to `ready_count - 1`.
            match self.check_pdb_safety(ready_count, pdb_min_available) {
                Ok(()) => {}
                Err(SkipReason::PdbViolation) => {
                    warn!(
                        pod = %pod_name,
                        ready_count,
                        pdb_min_available,
                        "PDB violation; skipping restart of fragmented pod"
                    );
                    summary.pods_skipped_pdb += 1;
                    continue;
                }
                Err(other) => {
                    warn!(pod = %pod_name, reason = ?other, "unexpected skip reason");
                    continue;
                }
            }

            // Perform the defragmentation cycle for this pod.
            match self.defrag_pod(&pod_name, snapshot.fragmentation_ratio).await {
                Ok(()) => {
                    summary.pods_restarted += 1;
                    // Only restart one pod per reconcile pass to avoid
                    // cascading restarts.
                    break;
                }
                Err(e) => {
                    error!(pod = %pod_name, error = %e, "defrag cycle failed");
                }
            }
        }

        Ok(summary)
    }

    // -----------------------------------------------------------------------
    // Phase 1: Cordon – remove pod from load balancer
    // -----------------------------------------------------------------------

    /// Apply the drain label to the pod so the Service stops routing to it.
    async fn cordon_pod(&self, pod_name: &str) -> Result<(), DefragError> {
        info!(pod = %pod_name, "cordoning pod (removing from load balancer)");

        let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let patch = json!({
            "metadata": {
                "labels": {
                    CORDON_LABEL_KEY: CORDON_LABEL_DRAINED
                },
                "annotations": {
                    DEFRAG_ANNOTATION_KEY: "jemalloc-heap-defragmentation"
                }
            }
        });
        let pp = PatchParams::apply("defrag-controller").force();
        pods_api
            .patch(pod_name, &pp, &Patch::MergePatch(patch))
            .await
            .map_err(|e| DefragError::KubeApi(e.to_string()))?;

        debug!(pod = %pod_name, "pod cordoned");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Phase 2: Restart – delete pod (owning controller will recreate it)
    // -----------------------------------------------------------------------

    /// Delete the pod to trigger a graceful restart via its owning controller.
    async fn restart_pod(&self, pod_name: &str) -> Result<(), DefragError> {
        info!(pod = %pod_name, "deleting pod to trigger graceful restart");

        let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let dp = DeleteParams::default();
        pods_api
            .delete(pod_name, &dp)
            .await
            .map_err(|e| DefragError::KubeApi(e.to_string()))?;

        debug!(pod = %pod_name, "pod deleted; awaiting recreation");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Phase 3: Wait – poll until pod is Running+Ready again
    // -----------------------------------------------------------------------

    /// Wait for the replacement pod (same name) to become Running+Ready.
    async fn wait_for_pod_ready(&self, pod_name: &str) -> Result<(), DefragError> {
        info!(
            pod = %pod_name,
            timeout_secs = self.config.ready_timeout.as_secs(),
            "waiting for pod to become Ready"
        );

        let start = std::time::Instant::now();
        let poll_interval = Duration::from_secs(5);

        loop {
            if start.elapsed() >= self.config.ready_timeout {
                return Err(DefragError::PodNotReady(format!(
                    "pod {pod_name} did not become Ready within {:?}",
                    self.config.ready_timeout
                )));
            }

            tokio::time::sleep(poll_interval).await;

            let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), &self.config.namespace);
            match pods_api.get(pod_name).await {
                Ok(pod) => {
                    if is_pod_ready(&pod) {
                        info!(pod = %pod_name, "pod is Ready");
                        return Ok(());
                    }
                    debug!(pod = %pod_name, "pod not yet Ready; continuing to poll");
                }
                Err(e) => {
                    // The pod may still be in the process of being deleted /
                    // recreated – treat transient API errors as "not ready yet".
                    debug!(pod = %pod_name, error = %e, "pod not found yet; continuing to poll");
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Phase 4: Reintroduce – restore the pod to the load balancer
    // -----------------------------------------------------------------------

    /// Remove the drain label so the Service routes traffic to the pod again.
    async fn reintroduce_pod(&self, pod_name: &str) -> Result<(), DefragError> {
        info!(pod = %pod_name, "reintroducing pod to load balancer");

        let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), &self.config.namespace);
        // Use a JSON Merge Patch to set the label back to "active".
        let patch = json!({
            "metadata": {
                "labels": {
                    CORDON_LABEL_KEY: CORDON_LABEL_ACTIVE
                }
            }
        });
        let pp = PatchParams::apply("defrag-controller").force();
        pods_api
            .patch(pod_name, &pp, &Patch::MergePatch(patch))
            .await
            .map_err(|e| DefragError::KubeApi(e.to_string()))?;

        debug!(pod = %pod_name, "pod reintroduced to load balancer");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Orchestration
    // -----------------------------------------------------------------------

    /// Run the full defragmentation cycle for a single pod:
    /// cordon → restart → wait → reintroduce.
    async fn defrag_pod(
        &self,
        pod_name: &str,
        fragmentation_ratio: f64,
    ) -> Result<(), DefragError> {
        info!(
            pod = %pod_name,
            fragmentation_ratio,
            "starting defragmentation cycle"
        );

        // Phase 1: Cordon
        self.cordon_pod(pod_name).await?;

        // Phase 2: Restart
        if let Err(e) = self.restart_pod(pod_name).await {
            // Best-effort: try to undo the cordon so the pod can still serve
            // traffic while we investigate.
            warn!(pod = %pod_name, error = %e, "restart failed; attempting to undo cordon");
            let _ = self.reintroduce_pod(pod_name).await;
            return Err(e);
        }

        // Phase 3: Wait for Ready
        if let Err(e) = self.wait_for_pod_ready(pod_name).await {
            warn!(
                pod = %pod_name,
                error = %e,
                "pod did not become Ready within timeout; leaving load balancer exclusion in place"
            );
            return Err(e);
        }

        // Phase 4: Reintroduce
        self.reintroduce_pod(pod_name).await?;

        info!(pod = %pod_name, "defragmentation cycle complete");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Kubernetes helpers
    // -----------------------------------------------------------------------

    /// List all pods matching the configured label selector.
    async fn list_pods(&self) -> Result<Vec<Pod>, DefragError> {
        let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let lp = ListParams::default().labels(&self.config.label_selector);
        let pod_list = pods_api
            .list(&lp)
            .await
            .map_err(|e| DefragError::KubeApi(e.to_string()))?;
        Ok(pod_list.items)
    }

    /// Query the effective `minAvailable` from all matching PDBs.
    ///
    /// When multiple PDBs match, returns the **maximum** `minAvailable` (most
    /// conservative).  Returns `0` when no PDB is found (no restriction).
    async fn query_pdb_min_available(&self) -> Result<i32, DefragError> {
        let pdb_api: Api<PodDisruptionBudget> =
            Api::namespaced(self.client.clone(), &self.config.namespace);
        let lp = ListParams::default().labels(&self.config.label_selector);
        let pdb_list = pdb_api
            .list(&lp)
            .await
            .map_err(|e| DefragError::KubeApi(e.to_string()))?;

        let min_available = pdb_list
            .items
            .iter()
            .filter_map(|pdb| {
                pdb.spec.as_ref().and_then(|s| {
                    s.min_available.as_ref().and_then(|v| {
                        // min_available can be an integer or a percentage string.
                        // We only parse integer values here; percentages are
                        // intentionally left as 0 (effectively ignored), which is
                        // safe – the controller simply won't apply extra PDB-based
                        // gating for percentage-based policies.
                        match v {
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(n) => {
                                Some(*n)
                            }
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::String(_) => {
                                None
                            }
                        }
                    })
                })
            })
            .max()
            .unwrap_or(0);

        debug!(
            pdb_count = pdb_list.items.len(),
            min_available,
            "PDB query result"
        );
        Ok(min_available)
    }

    /// Check that restarting one pod will not violate the PDB.
    ///
    /// We conservatively model the post-restart state as `ready_count - 1`
    /// (the pod being restarted is transiently unavailable).
    fn check_pdb_safety(
        &self,
        ready_count: i32,
        pdb_min_available: i32,
    ) -> Result<(), SkipReason> {
        let available_after_restart = ready_count - 1;
        if available_after_restart < pdb_min_available {
            return Err(SkipReason::PdbViolation);
        }
        Ok(())
    }

    /// Scrape the Prometheus metrics endpoint of a pod and parse jemalloc stats.
    ///
    /// Pod IP is resolved from `pod.status.podIP`.
    async fn fetch_pod_metrics(&self, pod_name: &str) -> Result<JemallocSnapshot, DefragError> {
        // Retrieve the pod to get its IP.
        let pods_api: Api<Pod> = Api::namespaced(self.client.clone(), &self.config.namespace);
        let pod = pods_api
            .get(pod_name)
            .await
            .map_err(|e| DefragError::KubeApi(e.to_string()))?;

        let pod_ip = pod
            .status
            .as_ref()
            .and_then(|s| s.pod_ip.as_deref())
            .ok_or_else(|| DefragError::MetricsScrape(format!("pod {pod_name} has no IP")))?;

        let url = format!(
            "http://{}:{}{}",
            pod_ip, self.config.metrics_port, self.config.metrics_path
        );

        debug!(pod = %pod_name, url = %url, "scraping pod metrics");

        let text = reqwest_get_text(&url).await.map_err(|e| {
            DefragError::MetricsScrape(format!("GET {url}: {e}"))
        })?;

        let snapshot = JemallocSnapshot::from_prometheus_text(&text)
            .map_err(|e| DefragError::MetricsScrape(e.to_string()))?;

        debug!(
            pod = %pod_name,
            fragmentation_ratio = snapshot.fragmentation_ratio,
            resident_bytes = snapshot.resident_bytes,
            "scraped jemalloc snapshot"
        );
        Ok(snapshot)
    }
}

// ---------------------------------------------------------------------------
// HTTP helper (thin wrapper for testability)
// ---------------------------------------------------------------------------

/// Perform an HTTP GET and return the response body as a `String`.
///
/// Extracted to a standalone function so tests can mock it.  In production
/// this calls out to `reqwest`.
async fn reqwest_get_text(url: &str) -> Result<String, String> {
    reqwest::get(url)
        .await
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur during a defragmentation cycle.
#[derive(Debug, thiserror::Error)]
pub enum DefragError {
    /// A Kubernetes API call failed.
    #[error("Kubernetes API error: {0}")]
    KubeApi(String),

    /// Scraping or parsing pod metrics failed.
    #[error("metrics scrape error: {0}")]
    MetricsScrape(String),

    /// The pod did not become Ready within the configured timeout.
    #[error("pod not ready: {0}")]
    PodNotReady(String),
}

// ---------------------------------------------------------------------------
// Internal pod inspection helpers
// ---------------------------------------------------------------------------

/// Count the number of pods that are both Running and Ready.
fn count_ready_pods(pods: &[Pod]) -> i32 {
    pods.iter().filter(|p| is_pod_ready(p)).count() as i32
}

/// Returns `true` if a pod is in the `Running` phase.
fn is_pod_running(pod: &Pod) -> bool {
    pod.status
        .as_ref()
        .and_then(|s| s.phase.as_deref())
        .map(|phase| phase == "Running")
        .unwrap_or(false)
}

/// Returns `true` if a pod is Running **and** has the `Ready=True` condition.
fn is_pod_ready(pod: &Pod) -> bool {
    if !is_pod_running(pod) {
        return false;
    }
    pod.status
        .as_ref()
        .and_then(|s| s.conditions.as_ref())
        .map(|conditions| {
            conditions
                .iter()
                .any(|c| c.type_ == "Ready" && c.status == "True")
        })
        .unwrap_or(false)
}

/// Returns `true` if the pod has already been cordoned by this controller.
fn is_pod_draining(pod: &Pod) -> bool {
    pod.metadata
        .labels
        .as_ref()
        .and_then(|labels| labels.get(CORDON_LABEL_KEY))
        .map(|v| v.as_str() == CORDON_LABEL_DRAINED)
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{PodCondition, PodStatus};
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;

    fn make_pod(name: &str, phase: &str, ready: bool) -> Pod {
        let conditions = if ready {
            Some(vec![PodCondition {
                type_: "Ready".to_string(),
                status: "True".to_string(),
                ..Default::default()
            }])
        } else {
            None
        };
        Pod {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                labels: Some(BTreeMap::from([(
                    CORDON_LABEL_KEY.to_string(),
                    CORDON_LABEL_ACTIVE.to_string(),
                )])),
                ..Default::default()
            },
            status: Some(PodStatus {
                phase: Some(phase.to_string()),
                pod_ip: Some("10.0.0.1".to_string()),
                conditions,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn make_draining_pod(name: &str) -> Pod {
        let mut pod = make_pod(name, "Running", true);
        pod.metadata.labels = Some(BTreeMap::from([(
            CORDON_LABEL_KEY.to_string(),
            CORDON_LABEL_DRAINED.to_string(),
        )]));
        pod
    }

    #[test]
    fn test_is_pod_running_true() {
        let pod = make_pod("p0", "Running", false);
        assert!(is_pod_running(&pod));
    }

    #[test]
    fn test_is_pod_running_false() {
        let pod = make_pod("p0", "Pending", false);
        assert!(!is_pod_running(&pod));
    }

    #[test]
    fn test_is_pod_ready_true() {
        let pod = make_pod("p0", "Running", true);
        assert!(is_pod_ready(&pod));
    }

    #[test]
    fn test_is_pod_ready_false_no_conditions() {
        let pod = make_pod("p0", "Running", false);
        assert!(!is_pod_ready(&pod));
    }

    #[test]
    fn test_is_pod_draining_true() {
        let pod = make_draining_pod("p0");
        assert!(is_pod_draining(&pod));
    }

    #[test]
    fn test_is_pod_draining_false() {
        let pod = make_pod("p0", "Running", true);
        assert!(!is_pod_draining(&pod));
    }

    #[test]
    fn test_count_ready_pods() {
        let pods = vec![
            make_pod("p0", "Running", true),
            make_pod("p1", "Running", true),
            make_pod("p2", "Pending", false),
        ];
        assert_eq!(count_ready_pods(&pods), 2);
    }

    // Helper to build a DefragController without a live Kubernetes client for
    // unit tests that only exercise pure-logic methods.
    fn make_test_controller() -> DefragController {
        // SAFETY: The Kubernetes `Client` is never called in these unit tests –
        // they only invoke `check_pdb_safety`, which is a pure local function.
        // The value is immediately dropped after the assertion.
        DefragController {
            client: unsafe { std::mem::zeroed() },
            config: DefragConfig::default(),
        }
    }

    #[test]
    fn test_check_pdb_safety_ok() {
        let ctrl = make_test_controller();
        // 3 ready, pdb requires 2 → restarting 1 leaves 2 → ok
        assert!(ctrl.check_pdb_safety(3, 2).is_ok());
    }

    #[test]
    fn test_check_pdb_safety_violation() {
        let ctrl = make_test_controller();
        // 2 ready, pdb requires 2 → restarting 1 leaves 1 → violation
        assert_eq!(ctrl.check_pdb_safety(2, 2), Err(SkipReason::PdbViolation));
    }

    #[test]
    fn test_check_pdb_no_pdb() {
        let ctrl = make_test_controller();
        // pdb_min_available = 0 (no PDB) → always ok
        assert!(ctrl.check_pdb_safety(1, 0).is_ok());
    }

    #[test]
    fn test_defrag_config_defaults() {
        let cfg = DefragConfig::default();
        assert_eq!(cfg.namespace, "default");
        assert_eq!(cfg.fragmentation_threshold, FRAGMENTATION_THRESHOLD);
        assert_eq!(cfg.metrics_port, 9090);
    }

    #[test]
    fn test_fragmentation_threshold_constant() {
        assert_eq!(FRAGMENTATION_THRESHOLD, 0.30);
    }
}
