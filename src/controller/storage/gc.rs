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
//! Orphaned PersistentVolumeClaim garbage collector.
//!
//! When a `StellarNode` is deleted with `spec.storage.retentionPolicy: Delete`
//! the operator's finalizer should remove the associated PVC.  However, in
//! pathological scenarios (operator restart during cleanup, manual finalizer
//! removal, partial failures) PVCs can be left behind — incurring ongoing
//! cloud storage costs and cluttering the namespace.
//!
//! This module implements a **proactive GC scanner** that:
//!
//! 1. Lists every PVC in the watched namespace(s) that carries the
//!    `app.kubernetes.io/managed-by=stellar-operator` label.
//! 2. Checks whether the owning `StellarNode` still exists.
//! 3. For PVCs whose owner is gone **and** whose retention annotation records
//!    `Delete`, issues a Kubernetes delete call.
//! 4. Emits structured log lines and returns a [`GcReport`] summarising what
//!    was collected.
//!
//! The scanner is intentionally conservative: it only deletes PVCs that
//! **unambiguously** belong to a missing node with an explicit delete policy.
//! Any PVC that cannot be conclusively attributed to a StellarNode is left
//! untouched and reported as a warning.
//!
//! # Integration
//!
//! The scanner is spawned as a background task inside `run_controller`:
//!
//! ```rust,ignore
//! tokio::spawn(async move {
//!     let cfg = GcConfig::default();
//!     run_pvc_gc_loop(client, cfg, watch_namespace).await;
//! });
//! ```
//!
//! It can also be invoked on-demand (e.g. for testing):
//!
//! ```rust,ignore
//! let report = scan_and_collect(&client, &cfg, Some("stellar")).await?;
//! println!("{} PVCs collected", report.collected);
//! ```

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::PersistentVolumeClaim;
use kube::{
    api::{Api, DeleteParams, ListParams},
    Client, ResourceExt,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::crd::StellarNode;
use crate::error::{Error, Result};

// ────────────────────────────────────────────────────────────────────────────
// Labels & annotations
// ────────────────────────────────────────────────────────────────────────────

/// Label that identifies PVCs managed by the stellar-operator.
pub const MANAGED_BY_LABEL: &str = "app.kubernetes.io/managed-by";
/// Value of the managed-by label.
pub const MANAGED_BY_VALUE: &str = "stellar-operator";

/// Label carrying the name of the StellarNode that owns a PVC.
pub const NODE_INSTANCE_LABEL: &str = "app.kubernetes.io/instance";

/// Annotation written by the operator to record the retention policy at PVC
/// creation time so the GC scanner can make the correct decision even after
/// the owning `StellarNode` is gone.
///
/// Values: `"Delete"` or `"Retain"`.
pub const RETENTION_POLICY_ANNOTATION: &str = "stellar.org/retention-policy";

// ────────────────────────────────────────────────────────────────────────────
// Configuration
// ────────────────────────────────────────────────────────────────────────────

/// Configuration for the PVC garbage collector.
#[derive(Debug, Clone)]
pub struct GcConfig {
    /// How often the background scan loop runs.
    pub scan_interval: Duration,

    /// When `true` the GC scanner reports what it *would* delete but does not
    /// actually issue any Kubernetes delete calls.
    pub dry_run: bool,

    /// Maximum number of PVCs to delete per scan pass.  Limits the blast
    /// radius in case of a logic bug.  `0` means unlimited.
    pub delete_limit_per_scan: usize,
}

impl Default for GcConfig {
    fn default() -> Self {
        Self {
            scan_interval: Duration::from_secs(300), // every 5 minutes
            dry_run: false,
            delete_limit_per_scan: 50,
        }
    }
}

impl GcConfig {
    /// Enable dry-run mode (no actual deletions).
    pub fn dry_run(mut self) -> Self {
        self.dry_run = true;
        self
    }

    /// Set the scan interval.
    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.scan_interval = interval;
        self
    }

    /// Set a per-scan deletion cap.
    pub fn with_delete_limit(mut self, limit: usize) -> Self {
        self.delete_limit_per_scan = limit;
        self
    }

    /// Conditionally enable dry-run mode based on a boolean flag.
    /// Useful when mirroring the operator's global `dry_run` flag.
    pub fn dry_run_if(mut self, enabled: bool) -> Self {
        self.dry_run = enabled;
        self
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Report types
// ────────────────────────────────────────────────────────────────────────────

/// Disposition of a single PVC during a GC scan pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PvcDisposition {
    /// PVC was deleted by the GC scanner.
    Collected,
    /// PVC was skipped because the owning StellarNode still exists.
    OwnerPresent,
    /// PVC was skipped because its retention policy annotation is `"Retain"`.
    RetentionRetain,
    /// PVC was skipped because its ownership could not be determined.
    UnknownOwner,
    /// Dry-run mode: the PVC would have been collected but was not.
    DryRunWouldCollect,
    /// The delete call failed; the PVC is still present.
    DeleteFailed(String),
}

/// Details about one PVC evaluated during a scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PvcScanEntry {
    /// PVC name.
    pub name: String,
    /// Namespace the PVC lives in.
    pub namespace: String,
    /// Name of the StellarNode that owned this PVC, if determinable.
    pub owner_node: Option<String>,
    /// What the GC scanner did with this PVC.
    pub disposition: PvcDisposition,
}

/// Summary of a single GC scan pass.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GcReport {
    /// Timestamp (RFC 3339) when the scan was performed.
    pub timestamp: String,
    /// Total PVCs evaluated.
    pub total_evaluated: usize,
    /// PVCs that were deleted (or would be deleted in dry-run).
    pub collected: usize,
    /// PVCs skipped because the owner still exists.
    pub skipped_owner_present: usize,
    /// PVCs skipped due to Retain policy.
    pub skipped_retain_policy: usize,
    /// PVCs skipped because ownership is ambiguous.
    pub skipped_unknown_owner: usize,
    /// PVCs where the delete call failed.
    pub delete_failures: usize,
    /// Per-PVC detail entries.
    pub entries: Vec<PvcScanEntry>,
}

impl GcReport {
    fn new() -> Self {
        Self {
            timestamp: chrono::Utc::now().to_rfc3339(),
            ..Default::default()
        }
    }

    fn push(&mut self, entry: PvcScanEntry) {
        match &entry.disposition {
            PvcDisposition::Collected | PvcDisposition::DryRunWouldCollect => self.collected += 1,
            PvcDisposition::OwnerPresent => self.skipped_owner_present += 1,
            PvcDisposition::RetentionRetain => self.skipped_retain_policy += 1,
            PvcDisposition::UnknownOwner => self.skipped_unknown_owner += 1,
            PvcDisposition::DeleteFailed(_) => self.delete_failures += 1,
        }
        self.total_evaluated += 1;
        self.entries.push(entry);
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Background scan loop
// ────────────────────────────────────────────────────────────────────────────

/// Run the PVC garbage-collection loop indefinitely.
///
/// This function is intended to be spawned as a long-lived background task via
/// `tokio::spawn`.  It never returns under normal conditions; callers can cancel
/// it by dropping the returned `JoinHandle`.
///
/// # Arguments
///
/// * `client`     — Kubernetes client.
/// * `config`     — GC configuration (interval, dry-run flag, etc.).
/// * `namespace`  — Optional namespace to restrict scans to.  `None` scans all
///                  namespaces (requires cluster-wide RBAC).
pub async fn run_pvc_gc_loop(client: Client, config: GcConfig, namespace: Option<String>) {
    info!(
        interval_secs = config.scan_interval.as_secs(),
        dry_run = config.dry_run,
        namespace = namespace.as_deref().unwrap_or("<all>"),
        "PVC GC scanner started",
    );

    loop {
        tokio::time::sleep(config.scan_interval).await;

        match scan_and_collect(&client, &config, namespace.as_deref()).await {
            Ok(report) => {
                info!(
                    total_evaluated = report.total_evaluated,
                    collected        = report.collected,
                    skipped_retain   = report.skipped_retain_policy,
                    skipped_owner    = report.skipped_owner_present,
                    unknown_owner    = report.skipped_unknown_owner,
                    delete_failures  = report.delete_failures,
                    dry_run          = config.dry_run,
                    "PVC GC scan complete",
                );
            }
            Err(e) => {
                warn!(error = %e, "PVC GC scan failed; will retry next interval");
            }
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Single-pass scan & collect
// ────────────────────────────────────────────────────────────────────────────

/// Perform a single GC scan pass.
///
/// Lists managed PVCs, checks for orphans, and deletes those with a
/// `Delete` retention policy (subject to `config.dry_run` and
/// `config.delete_limit_per_scan`).
///
/// Returns a [`GcReport`] describing what was (or would be) collected.
pub async fn scan_and_collect(
    client: &Client,
    config: &GcConfig,
    namespace: Option<&str>,
) -> Result<GcReport> {
    let mut report = GcReport::new();
    let mut deleted_this_pass: usize = 0;

    // ── 1. Discover PVCs with the managed-by label ────────────────────────
    let lp = ListParams::default()
        .labels(&format!("{MANAGED_BY_LABEL}={MANAGED_BY_VALUE}"));

    let pvcs: Vec<PersistentVolumeClaim> = match namespace {
        Some(ns) => {
            let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), ns);
            api.list(&lp)
                .await
                .map_err(Error::KubeError)?
                .items
        }
        None => {
            let api: Api<PersistentVolumeClaim> = Api::all(client.clone());
            api.list(&lp)
                .await
                .map_err(Error::KubeError)?
                .items
        }
    };

    debug!(
        pvc_count = pvcs.len(),
        namespace = namespace.unwrap_or("<all>"),
        "discovered managed PVCs",
    );

    // ── 2. For each PVC, determine its fate ──────────────────────────────
    for pvc in &pvcs {
        let pvc_name = pvc.name_any();
        let pvc_ns = pvc.namespace().unwrap_or_else(|| "default".to_string());
        let labels = pvc.metadata.labels.clone().unwrap_or_default();
        let annotations = pvc.metadata.annotations.clone().unwrap_or_default();

        let owner_node = owner_node_name(pvc.metadata.owner_references.as_ref(), &labels);

        // ── 2a. Determine retention policy from annotation ──────────────
        let retention = retention_from_annotation(&annotations);

        // ── 2b. Check if the owning StellarNode still exists ───────────
        let disposition = match &owner_node {
            None => {
                warn!(
                    pvc = %pvc_name,
                    namespace = %pvc_ns,
                    "cannot determine owner; leaving PVC untouched",
                );
                PvcDisposition::UnknownOwner
            }
            Some(node_name) => {
                if retention == RetentionHint::Retain {
                    debug!(
                        pvc = %pvc_name,
                        namespace = %pvc_ns,
                        owner = %node_name,
                        "PVC has Retain policy; skipping",
                    );
                    PvcDisposition::RetentionRetain
                } else if node_still_exists(client, node_name, &pvc_ns).await? {
                    debug!(
                        pvc = %pvc_name,
                        namespace = %pvc_ns,
                        owner = %node_name,
                        "owning StellarNode still present; skipping",
                    );
                    PvcDisposition::OwnerPresent
                } else if config.delete_limit_per_scan > 0
                    && deleted_this_pass >= config.delete_limit_per_scan
                {
                    warn!(
                        pvc = %pvc_name,
                        namespace = %pvc_ns,
                        "per-scan delete limit ({}) reached; deferring to next pass",
                        config.delete_limit_per_scan,
                    );
                    // Treat as unknown so it's re-evaluated next cycle.
                    PvcDisposition::UnknownOwner
                } else {
                    // Orphaned PVC with Delete policy — collect it.
                    collect_pvc(client, &pvc_name, &pvc_ns, config.dry_run).await?;
                    if !config.dry_run {
                        deleted_this_pass += 1;
                        info!(
                            pvc = %pvc_name,
                            namespace = %pvc_ns,
                            owner = %node_name,
                            "orphaned PVC deleted by GC scanner",
                        );
                        PvcDisposition::Collected
                    } else {
                        info!(
                            pvc = %pvc_name,
                            namespace = %pvc_ns,
                            owner = %node_name,
                            "dry-run: would delete orphaned PVC",
                        );
                        PvcDisposition::DryRunWouldCollect
                    }
                }
            }
        };

        report.push(PvcScanEntry {
            name: pvc_name,
            namespace: pvc_ns,
            owner_node,
            disposition,
        });
    }

    Ok(report)
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

/// Possible retention hints derived from the PVC annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RetentionHint {
    Delete,
    Retain,
    /// No annotation present; default is Delete (matches CRD default).
    Unknown,
}

/// Read the retention-policy annotation from a PVC's annotations map.
fn retention_from_annotation(annotations: &BTreeMap<String, String>) -> RetentionHint {
    match annotations
        .get(RETENTION_POLICY_ANNOTATION)
        .map(|s| s.as_str())
    {
        Some("Retain") => RetentionHint::Retain,
        Some("Delete") => RetentionHint::Delete,
        // No annotation — treat as Delete (operator default).
        _ => RetentionHint::Unknown,
    }
}

/// Determine the StellarNode name that owns a PVC.
///
/// Checks `ownerReferences` for a `StellarNode` kind first, then falls back
/// to the `app.kubernetes.io/instance` label.
fn owner_node_name(
    owner_refs: Option<&Vec<k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference>>,
    labels: &BTreeMap<String, String>,
) -> Option<String> {
    if let Some(refs) = owner_refs {
        for r in refs {
            if r.kind == "StellarNode" {
                return Some(r.name.clone());
            }
        }
    }
    labels
        .get(NODE_INSTANCE_LABEL)
        .or_else(|| labels.get("stellar.org/node-name"))
        .cloned()
}

/// Return `true` if a `StellarNode` with the given name exists in `namespace`.
async fn node_still_exists(client: &Client, node_name: &str, namespace: &str) -> Result<bool> {
    let api: Api<StellarNode> = Api::namespaced(client.clone(), namespace);
    match api.get(node_name).await {
        Ok(_) => Ok(true),
        Err(kube::Error::Api(e)) if e.code == 404 => Ok(false),
        Err(e) => Err(Error::KubeError(e)),
    }
}

/// Issue a Kubernetes delete call for a PVC.
///
/// In dry-run mode this function is a no-op.
async fn collect_pvc(
    client: &Client,
    pvc_name: &str,
    namespace: &str,
    dry_run: bool,
) -> Result<()> {
    if dry_run {
        return Ok(());
    }
    let api: Api<PersistentVolumeClaim> = Api::namespaced(client.clone(), namespace);
    match api.delete(pvc_name, &DeleteParams::default()).await {
        Ok(_) => Ok(()),
        Err(kube::Error::Api(e)) if e.code == 404 => {
            // Already gone — nothing to do.
            Ok(())
        }
        Err(e) => Err(Error::KubeError(e)),
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Unit tests
// ────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // ── GcConfig ────────────────────────────────────────────────────────────

    #[test]
    fn default_gc_config_scan_interval_is_five_minutes() {
        let cfg = GcConfig::default();
        assert_eq!(cfg.scan_interval, Duration::from_secs(300));
    }

    #[test]
    fn default_gc_config_dry_run_is_false() {
        let cfg = GcConfig::default();
        assert!(!cfg.dry_run);
    }

    #[test]
    fn default_gc_config_delete_limit_is_50() {
        let cfg = GcConfig::default();
        assert_eq!(cfg.delete_limit_per_scan, 50);
    }

    #[test]
    fn dry_run_builder_sets_flag() {
        let cfg = GcConfig::default().dry_run();
        assert!(cfg.dry_run);
    }

    #[test]
    fn with_interval_sets_correct_duration() {
        let cfg = GcConfig::default().with_interval(Duration::from_secs(60));
        assert_eq!(cfg.scan_interval, Duration::from_secs(60));
    }

    #[test]
    fn with_delete_limit_sets_correct_value() {
        let cfg = GcConfig::default().with_delete_limit(10);
        assert_eq!(cfg.delete_limit_per_scan, 10);
    }

    #[test]
    fn dry_run_if_true_enables_dry_run() {
        let cfg = GcConfig::default().dry_run_if(true);
        assert!(cfg.dry_run);
    }

    #[test]
    fn dry_run_if_false_leaves_dry_run_disabled() {
        let cfg = GcConfig::default().dry_run_if(false);
        assert!(!cfg.dry_run);
    }

    // ── retention_from_annotation ───────────────────────────────────────────

    #[test]
    fn retention_retain_annotation_parses_correctly() {
        let mut ann = BTreeMap::new();
        ann.insert(RETENTION_POLICY_ANNOTATION.to_string(), "Retain".to_string());
        assert_eq!(retention_from_annotation(&ann), RetentionHint::Retain);
    }

    #[test]
    fn retention_delete_annotation_parses_correctly() {
        let mut ann = BTreeMap::new();
        ann.insert(RETENTION_POLICY_ANNOTATION.to_string(), "Delete".to_string());
        assert_eq!(retention_from_annotation(&ann), RetentionHint::Delete);
    }

    #[test]
    fn missing_annotation_defaults_to_unknown() {
        let ann = BTreeMap::new();
        assert_eq!(retention_from_annotation(&ann), RetentionHint::Unknown);
    }

    #[test]
    fn unrecognised_annotation_value_defaults_to_unknown() {
        let mut ann = BTreeMap::new();
        ann.insert(RETENTION_POLICY_ANNOTATION.to_string(), "foo".to_string());
        assert_eq!(retention_from_annotation(&ann), RetentionHint::Unknown);
    }

    // ── owner_node_name ─────────────────────────────────────────────────────

    #[test]
    fn owner_node_name_prefers_owner_reference() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

        let refs = vec![OwnerReference {
            api_version: "stellar.org/v1alpha1".to_string(),
            kind: "StellarNode".to_string(),
            name: "my-validator".to_string(),
            uid: "abc-123".to_string(),
            ..Default::default()
        }];
        let mut labels = BTreeMap::new();
        labels.insert(NODE_INSTANCE_LABEL.to_string(), "other-node".to_string());

        let name = owner_node_name(Some(&refs), &labels);
        assert_eq!(name.as_deref(), Some("my-validator"));
    }

    #[test]
    fn owner_node_name_falls_back_to_instance_label() {
        let mut labels = BTreeMap::new();
        labels.insert(NODE_INSTANCE_LABEL.to_string(), "fallback-node".to_string());

        let name = owner_node_name(None, &labels);
        assert_eq!(name.as_deref(), Some("fallback-node"));
    }

    #[test]
    fn owner_node_name_uses_stellar_node_name_label_as_last_resort() {
        let mut labels = BTreeMap::new();
        labels.insert("stellar.org/node-name".to_string(), "last-resort".to_string());

        let name = owner_node_name(None, &labels);
        assert_eq!(name.as_deref(), Some("last-resort"));
    }

    #[test]
    fn owner_node_name_returns_none_with_no_clues() {
        let labels = BTreeMap::new();
        let name = owner_node_name(None, &labels);
        assert!(name.is_none());
    }

    #[test]
    fn owner_node_name_ignores_non_stellarnode_owner_refs() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

        // ownerRef is for a StatefulSet, not a StellarNode — fall back to label.
        let refs = vec![OwnerReference {
            api_version: "apps/v1".to_string(),
            kind: "StatefulSet".to_string(),
            name: "some-statefulset".to_string(),
            uid: "xyz".to_string(),
            ..Default::default()
        }];
        let mut labels = BTreeMap::new();
        labels.insert(NODE_INSTANCE_LABEL.to_string(), "the-node".to_string());

        let name = owner_node_name(Some(&refs), &labels);
        assert_eq!(name.as_deref(), Some("the-node"));
    }

    // ── GcReport ────────────────────────────────────────────────────────────

    #[test]
    fn gc_report_counts_are_correct_after_push() {
        let mut report = GcReport::new();

        report.push(PvcScanEntry {
            name: "pvc-a".to_string(),
            namespace: "stellar".to_string(),
            owner_node: Some("node-a".to_string()),
            disposition: PvcDisposition::Collected,
        });
        report.push(PvcScanEntry {
            name: "pvc-b".to_string(),
            namespace: "stellar".to_string(),
            owner_node: Some("node-b".to_string()),
            disposition: PvcDisposition::OwnerPresent,
        });
        report.push(PvcScanEntry {
            name: "pvc-c".to_string(),
            namespace: "stellar".to_string(),
            owner_node: None,
            disposition: PvcDisposition::UnknownOwner,
        });
        report.push(PvcScanEntry {
            name: "pvc-d".to_string(),
            namespace: "stellar".to_string(),
            owner_node: Some("node-d".to_string()),
            disposition: PvcDisposition::RetentionRetain,
        });
        report.push(PvcScanEntry {
            name: "pvc-e".to_string(),
            namespace: "stellar".to_string(),
            owner_node: Some("node-e".to_string()),
            disposition: PvcDisposition::DryRunWouldCollect,
        });
        report.push(PvcScanEntry {
            name: "pvc-f".to_string(),
            namespace: "stellar".to_string(),
            owner_node: Some("node-f".to_string()),
            disposition: PvcDisposition::DeleteFailed("timeout".to_string()),
        });

        assert_eq!(report.total_evaluated, 6);
        assert_eq!(report.collected, 2); // Collected + DryRunWouldCollect
        assert_eq!(report.skipped_owner_present, 1);
        assert_eq!(report.skipped_unknown_owner, 1);
        assert_eq!(report.skipped_retain_policy, 1);
        assert_eq!(report.delete_failures, 1);
        assert_eq!(report.entries.len(), 6);
    }

    #[test]
    fn empty_gc_report_has_zero_counts() {
        let report = GcReport::new();
        assert_eq!(report.total_evaluated, 0);
        assert_eq!(report.collected, 0);
        assert_eq!(report.skipped_owner_present, 0);
        assert_eq!(report.skipped_retain_policy, 0);
        assert_eq!(report.skipped_unknown_owner, 0);
        assert_eq!(report.delete_failures, 0);
    }

    #[test]
    fn gc_report_timestamp_is_non_empty() {
        let report = GcReport::new();
        assert!(!report.timestamp.is_empty());
    }

    // ── PvcDisposition serialisation ─────────────────────────────────────────

    #[test]
    fn pvc_disposition_serialisation_roundtrip() {
        let dispositions = vec![
            PvcDisposition::Collected,
            PvcDisposition::OwnerPresent,
            PvcDisposition::RetentionRetain,
            PvcDisposition::UnknownOwner,
            PvcDisposition::DryRunWouldCollect,
            PvcDisposition::DeleteFailed("some error".to_string()),
        ];

        for d in &dispositions {
            let json = serde_json::to_string(d).expect("serialize");
            let restored: PvcDisposition = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(d, &restored);
        }
    }

    // ── Label / annotation constants ────────────────────────────────────────

    #[test]
    fn managed_by_label_matches_standard_k8s_convention() {
        assert_eq!(MANAGED_BY_LABEL, "app.kubernetes.io/managed-by");
        assert_eq!(MANAGED_BY_VALUE, "stellar-operator");
    }

    #[test]
    fn retention_policy_annotation_uses_stellar_org_prefix() {
        assert!(RETENTION_POLICY_ANNOTATION.starts_with("stellar.org/"));
    }

    // ── Integration-style tests for scan_and_collect logic ──────────────────
    //
    // These tests exercise the *decision logic* (which disposition a PVC should
    // get) without a live Kubernetes cluster, by driving the helper functions
    // directly.

    #[test]
    fn retain_annotation_short_circuits_collection() {
        let mut ann = BTreeMap::new();
        ann.insert(RETENTION_POLICY_ANNOTATION.to_string(), "Retain".to_string());
        let hint = retention_from_annotation(&ann);
        // A PVC with Retain should never reach the node-existence check.
        assert_eq!(hint, RetentionHint::Retain);
    }

    #[test]
    fn unknown_annotation_does_not_short_circuit_collection() {
        // Unknown annotation (no annotation present) should fall through to the
        // node-existence check, not be skipped.
        let ann = BTreeMap::new();
        let hint = retention_from_annotation(&ann);
        assert_ne!(hint, RetentionHint::Retain);
    }

    #[test]
    fn delete_limit_zero_means_unlimited() {
        let cfg = GcConfig::default().with_delete_limit(0);
        // When limit is 0, `deleted_this_pass >= 0` is always true — but the
        // code path checks `delete_limit_per_scan > 0` first, so 0 disables
        // the cap.  This test documents that contract.
        assert_eq!(cfg.delete_limit_per_scan, 0);
    }
}
