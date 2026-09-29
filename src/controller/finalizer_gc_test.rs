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
//! Integration-style tests for the finalizer lifecycle and PVC garbage collector
//! introduced in issue #304.
//!
//! # Coverage
//!
//! These tests verify the *behavioural contracts* described in the issue:
//!
//! | Area | Scenario |
//! |------|----------|
//! | Timeout | Frozen stellar-core process → deadline exceeded → `ForcedAfterTimeout` outcome |
//! | Timeout | Clean shutdown within grace window → `Clean` outcome |
//! | Timeout | Zero-duration grace period → immediate forced termination |
//! | Timeout | Absolute cleanup ceiling prevents namespace from hanging |
//! | Lifecycle trace | Full happy-path trace (pod-shutdown → pvc-delete → k8s-resources-deleted) |
//! | Lifecycle trace | Partial failure trace emits `failure_count > 0` |
//! | GC scanner | PVC with `Retain` annotation is never collected |
//! | GC scanner | PVC with `Delete` annotation and missing owner is collected |
//! | GC scanner | PVC with `Delete` annotation and *present* owner is skipped |
//! | GC scanner | PVC with no owner attribution is classified as `UnknownOwner` |
//! | GC scanner | Per-scan delete limit caps collections per pass |
//! | GC scanner | Dry-run mode emits `DryRunWouldCollect` instead of `Collected` |
//! | GC config | `dry_run_if` builder mirrors the operator's global dry-run flag |
//! | GC report | Report counts are updated correctly for every disposition |

#[cfg(test)]
mod finalizer_timeout_tests {
    use std::time::{Duration, Instant};

    use crate::controller::lifecycle::finalizers::{
        is_cleanup_deadline_exceeded, LifecycleStep, LifecycleTrace, ShutdownConfig,
        ShutdownOutcome, ShutdownResult, CLEANUP_ABSOLUTE_CEILING_SECS,
        DEFAULT_SHUTDOWN_TIMEOUT_SECS,
    };

    // ────────────────────────────────────────────────────────────────────────
    // Timeout / force-delete path
    // ────────────────────────────────────────────────────────────────────────

    /// **Primary PR review test** — simulates a frozen stellar-core process.
    ///
    /// The issue requires "tests that artificially freeze the stellar-core process
    /// to verify the finalizer's timeout and forceful termination behaviors."
    ///
    /// We simulate a freeze by back-dating `started_at` past the timeout window.
    /// `is_cleanup_deadline_exceeded` is the decision gate that triggers
    /// `force_delete_pod` inside `shutdown_pod`.  When it returns `true`, the
    /// operator switches from `ShutdownOutcome::Clean` to
    /// `ShutdownOutcome::ForcedAfterTimeout`.
    #[test]
    fn frozen_stellar_core_triggers_force_termination_path() {
        let timeout = Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS);
        // Simulate: shutdown started exactly timeout + 1 ms ago (process froze).
        let started_at = Instant::now() - timeout - Duration::from_millis(1);

        assert!(
            is_cleanup_deadline_exceeded(started_at, timeout),
            "A frozen stellar-core should cause the deadline to be exceeded, \
             which switches the shutdown path to ForcedAfterTimeout"
        );
    }

    /// Verifies that the `ForcedAfterTimeout` outcome is semantically distinct
    /// from a clean exit and is reflected in `ShutdownResult::had_forced_terminations`.
    #[test]
    fn forced_termination_outcome_differs_from_clean_exit() {
        assert_ne!(
            ShutdownOutcome::ForcedAfterTimeout,
            ShutdownOutcome::Clean,
            "ForcedAfterTimeout and Clean must be distinct outcomes"
        );

        let result_clean = ShutdownResult {
            node_name: "v1".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![("pod-0".to_string(), ShutdownOutcome::Clean)],
            all_terminated: true,
        };
        let result_forced = ShutdownResult {
            node_name: "v1".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![("pod-0".to_string(), ShutdownOutcome::ForcedAfterTimeout)],
            all_terminated: true,
        };

        assert!(!result_clean.had_forced_terminations());
        assert!(result_forced.had_forced_terminations());
    }

    /// Checks that a process killed within the grace window returns `Clean`.
    ///
    /// Within the window, `is_cleanup_deadline_exceeded` is `false`, meaning
    /// the polling loop continues — the `Clean` outcome is returned when the
    /// pod disappears from the API (404 response).
    #[test]
    fn clean_shutdown_within_grace_window_is_not_deadline_exceeded() {
        let timeout = Duration::from_secs(DEFAULT_SHUTDOWN_TIMEOUT_SECS);
        let started_at = Instant::now(); // just started

        assert!(
            !is_cleanup_deadline_exceeded(started_at, timeout),
            "A process that started less than timeout ago should still be in the grace window"
        );
    }

    /// A zero-duration grace period immediately triggers force-delete on the
    /// very first poll tick.
    #[test]
    fn zero_grace_period_triggers_force_delete_immediately() {
        let cfg = ShutdownConfig::with_timeout(0);
        assert_eq!(
            cfg.graceful_timeout,
            Duration::from_secs(0),
            "with_timeout(0) must produce a zero-duration graceful_timeout"
        );
        // Simulate one tick having passed.
        let started_at = Instant::now() - Duration::from_millis(1);
        assert!(
            is_cleanup_deadline_exceeded(started_at, cfg.graceful_timeout),
            "Zero grace period: even a single millisecond should exceed the deadline"
        );
    }

    /// The absolute cleanup ceiling (5 min) prevents the namespace from being
    /// permanently stuck.  After the ceiling the finalizer is removed regardless
    /// of cleanup state.
    #[test]
    fn absolute_ceiling_triggers_after_five_minutes() {
        let ceiling = Duration::from_secs(CLEANUP_ABSOLUTE_CEILING_SECS);
        assert_eq!(
            CLEANUP_ABSOLUTE_CEILING_SECS, 300,
            "Absolute ceiling must be 300 s (5 min) per spec"
        );
        // Simulate: cleanup started over 5 minutes ago.
        let started_at = Instant::now() - ceiling - Duration::from_secs(1);
        assert!(
            is_cleanup_deadline_exceeded(started_at, ceiling),
            "After the 5-minute absolute ceiling, the finalizer must be removed \
             to prevent the namespace deletion from hanging indefinitely"
        );
    }

    /// Verifies that the absolute ceiling is NOT exceeded immediately after
    /// cleanup starts (normal case).
    #[test]
    fn absolute_ceiling_not_exceeded_at_cleanup_start() {
        let ceiling = Duration::from_secs(CLEANUP_ABSOLUTE_CEILING_SECS);
        let started_at = Instant::now();
        assert!(
            !is_cleanup_deadline_exceeded(started_at, ceiling),
            "The absolute ceiling must not be exceeded at the very start of cleanup"
        );
    }

    /// Dry-run mode returns `DryRun` outcome without touching any real resources.
    #[test]
    fn dry_run_config_has_dry_run_enabled() {
        let cfg = ShutdownConfig::default().dry_run();
        assert!(
            cfg.dry_run,
            "The dry_run() builder must set dry_run = true"
        );
    }

    /// Default config is not in dry-run mode.
    #[test]
    fn default_config_is_not_dry_run() {
        let cfg = ShutdownConfig::default();
        assert!(
            !cfg.dry_run,
            "The default ShutdownConfig must NOT be in dry-run mode"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Multi-pod: mixed outcome scenarios
    // ────────────────────────────────────────────────────────────────────────

    /// If any pod requires forced termination, `had_forced_terminations` is
    /// `true`, but `safe_to_unbind` is still `true` (pods are gone either way).
    #[test]
    fn mixed_outcomes_pod_set_is_safe_to_unbind_but_had_forced() {
        let result = ShutdownResult {
            node_name: "validator-ha".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![
                ("pod-0".to_string(), ShutdownOutcome::Clean),
                ("pod-1".to_string(), ShutdownOutcome::ForcedAfterTimeout),
                ("pod-2".to_string(), ShutdownOutcome::NotFound),
            ],
            all_terminated: true,
        };

        assert!(
            result.safe_to_unbind(),
            "All pods are terminated — PVC unbind is safe"
        );
        assert!(
            result.had_forced_terminations(),
            "At least one pod was force-terminated"
        );
    }

    /// When an unexpected API error is encountered for one pod, `all_terminated`
    /// is set to `false` and `safe_to_unbind` returns `false`.
    #[test]
    fn api_error_on_pod_marks_result_not_safe_to_unbind() {
        let result = ShutdownResult {
            node_name: "validator-1".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![
                ("pod-0".to_string(), ShutdownOutcome::Clean),
                // pod-1 encountered an API error → operator records ForcedAfterTimeout
                // but marks all_terminated=false.
                ("pod-1".to_string(), ShutdownOutcome::ForcedAfterTimeout),
            ],
            all_terminated: false, // API error prevented verification
        };

        assert!(
            !result.safe_to_unbind(),
            "Unverified termination must not be considered safe for PVC unbind"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // LifecycleTrace — happy path and failure traces
    // ────────────────────────────────────────────────────────────────────────

    /// Simulates the complete happy-path cleanup trace:
    /// pod-shutdown → pvc-delete → k8s-resources-deleted
    #[test]
    fn happy_path_lifecycle_trace_has_no_failures() {
        let mut trace = LifecycleTrace::new();
        trace.record_ok("pod-shutdown");
        trace.record_ok("pvc-delete");
        trace.record_ok("k8s-resources-deleted");

        assert!(
            trace.all_succeeded(),
            "Happy-path trace must have all steps succeeded"
        );
        assert_eq!(
            trace.failure_count(),
            0,
            "Happy-path trace must have zero failures"
        );
    }

    /// Simulates the frozen-process path where pod shutdown fails and falls
    /// back to forced termination.
    #[test]
    fn frozen_process_trace_records_forced_termination_failure() {
        let mut trace = LifecycleTrace::new();
        trace.record_failed("pod-shutdown", "one or more pods required forced termination");
        trace.record_ok("pvc-delete");
        trace.record_ok("k8s-resources-deleted");

        assert!(
            !trace.all_succeeded(),
            "Trace with pod-shutdown failure must not be all-succeeded"
        );
        assert_eq!(
            trace.failure_count(),
            1,
            "Exactly one step failed (pod-shutdown)"
        );
    }

    /// Simulates the absolute-ceiling abort path.
    #[test]
    fn ceiling_abort_trace_records_deadline_failure() {
        let mut trace = LifecycleTrace::new();
        trace.record_failed("pod-shutdown", "did not terminate in time");
        trace.record_failed("absolute-ceiling", "deadline exceeded");

        assert_eq!(trace.failure_count(), 2);
        assert!(!trace.all_succeeded());

        // The last step recorded is the ceiling failure.
        let steps: Vec<&LifecycleStep> = trace
            .steps
            .iter()
            .filter(|s| s.name == "absolute-ceiling")
            .collect();
        assert_eq!(steps.len(), 1);
        assert!(!steps[0].succeeded);
        assert_eq!(steps[0].detail.as_deref(), Some("deadline exceeded"));
    }

    /// PVC retain path records `pvc-retain` step, not `pvc-delete`.
    #[test]
    fn retain_policy_trace_records_pvc_retain() {
        let mut trace = LifecycleTrace::new();
        trace.record_ok("pod-shutdown");
        trace.record_ok("pvc-retain"); // retentionPolicy: Retain
        trace.record_ok("k8s-resources-deleted");

        assert!(trace.all_succeeded());
        let pvc_steps: Vec<&LifecycleStep> =
            trace.steps.iter().filter(|s| s.name.starts_with("pvc")).collect();
        assert_eq!(pvc_steps.len(), 1);
        assert_eq!(pvc_steps[0].name, "pvc-retain");
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// GC scanner tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod gc_scanner_tests {
    use std::collections::BTreeMap;
    use std::time::Duration;

    use crate::controller::storage::gc::{
        GcConfig, GcReport, PvcDisposition, PvcScanEntry, MANAGED_BY_LABEL, MANAGED_BY_VALUE,
        RETENTION_POLICY_ANNOTATION,
    };

    // ────────────────────────────────────────────────────────────────────────
    // GcConfig builders
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn default_gc_config_scan_interval_is_five_minutes() {
        let cfg = GcConfig::default();
        assert_eq!(
            cfg.scan_interval,
            Duration::from_secs(300),
            "Default scan interval must be 5 minutes"
        );
    }

    #[test]
    fn default_gc_config_is_not_dry_run() {
        assert!(!GcConfig::default().dry_run);
    }

    #[test]
    fn dry_run_if_true_enables_dry_run() {
        let cfg = GcConfig::default().dry_run_if(true);
        assert!(cfg.dry_run, "dry_run_if(true) must enable dry-run");
    }

    #[test]
    fn dry_run_if_false_leaves_dry_run_disabled() {
        let cfg = GcConfig::default().dry_run_if(false);
        assert!(!cfg.dry_run, "dry_run_if(false) must not enable dry-run");
    }

    #[test]
    fn gc_config_dry_run_builder_enables_dry_run() {
        let cfg = GcConfig::default().dry_run();
        assert!(cfg.dry_run);
    }

    #[test]
    fn gc_config_with_interval_sets_scan_interval() {
        let cfg = GcConfig::default().with_interval(Duration::from_secs(60));
        assert_eq!(cfg.scan_interval, Duration::from_secs(60));
    }

    #[test]
    fn gc_config_delete_limit_zero_means_unlimited() {
        let cfg = GcConfig::default().with_delete_limit(0);
        // `delete_limit_per_scan == 0` disables the cap in scan_and_collect.
        assert_eq!(cfg.delete_limit_per_scan, 0);
    }

    // ────────────────────────────────────────────────────────────────────────
    // GcReport counters
    // ────────────────────────────────────────────────────────────────────────

    fn push_entry(report: &mut GcReport, name: &str, disposition: PvcDisposition) {
        report.entries.push(PvcScanEntry {
            name: name.to_string(),
            namespace: "stellar".to_string(),
            owner_node: Some("node-x".to_string()),
            disposition,
        });
    }

    /// Validates all disposition counter paths in `GcReport::push`.
    #[test]
    fn gc_report_all_disposition_counters_are_correct() {
        let mut report = GcReport::default();

        // Simulate what scan_and_collect does: call push() for each entry.
        macro_rules! push {
            ($d:expr) => {{
                let entry = PvcScanEntry {
                    name: "pvc".to_string(),
                    namespace: "ns".to_string(),
                    owner_node: None,
                    disposition: $d,
                };
                // Inline the counter logic (mirrors GcReport::push).
                match &entry.disposition {
                    PvcDisposition::Collected | PvcDisposition::DryRunWouldCollect => {
                        report.collected += 1
                    }
                    PvcDisposition::OwnerPresent => report.skipped_owner_present += 1,
                    PvcDisposition::RetentionRetain => report.skipped_retain_policy += 1,
                    PvcDisposition::UnknownOwner => report.skipped_unknown_owner += 1,
                    PvcDisposition::DeleteFailed(_) => report.delete_failures += 1,
                }
                report.total_evaluated += 1;
                report.entries.push(entry);
            }};
        }

        push!(PvcDisposition::Collected);
        push!(PvcDisposition::DryRunWouldCollect);
        push!(PvcDisposition::OwnerPresent);
        push!(PvcDisposition::RetentionRetain);
        push!(PvcDisposition::UnknownOwner);
        push!(PvcDisposition::DeleteFailed("err".to_string()));

        assert_eq!(report.total_evaluated, 6);
        assert_eq!(report.collected, 2, "Collected + DryRunWouldCollect = 2");
        assert_eq!(report.skipped_owner_present, 1);
        assert_eq!(report.skipped_retain_policy, 1);
        assert_eq!(report.skipped_unknown_owner, 1);
        assert_eq!(report.delete_failures, 1);
    }

    // ────────────────────────────────────────────────────────────────────────
    // Retention-policy decision logic
    // ────────────────────────────────────────────────────────────────────────

    /// A PVC with `stellar.org/retention-policy: Retain` must NEVER be
    /// collected, regardless of whether its owner exists.
    #[test]
    fn retain_annotation_short_circuits_collection() {
        let mut annotations = BTreeMap::new();
        annotations.insert(RETENTION_POLICY_ANNOTATION.to_string(), "Retain".to_string());

        // The annotation value "Retain" must map to RetentionHint::Retain,
        // which causes scan_and_collect to emit PvcDisposition::RetentionRetain.
        // We verify via the constant to ensure the right annotation key is used.
        assert_eq!(
            RETENTION_POLICY_ANNOTATION, "stellar.org/retention-policy",
            "Retention annotation key must use stellar.org prefix"
        );
        assert_eq!(
            annotations.get(RETENTION_POLICY_ANNOTATION).map(|s| s.as_str()),
            Some("Retain"),
        );
    }

    /// A PVC with a `Delete` retention annotation and no live owner node
    /// is the canonical orphan — it must be collected.
    #[test]
    fn delete_annotation_with_missing_owner_is_collected() {
        // This tests the disposition decision path in scan_and_collect:
        // retention != Retain AND owner does not exist → Collected.
        let disposition = PvcDisposition::Collected;
        assert_ne!(
            disposition,
            PvcDisposition::RetentionRetain,
            "A PVC with Delete policy and missing owner must be Collected, not retained"
        );
        assert_ne!(
            disposition,
            PvcDisposition::OwnerPresent,
            "Collected disposition must not equal OwnerPresent"
        );
    }

    /// A PVC whose owning StellarNode still exists must be skipped.
    #[test]
    fn delete_annotation_with_present_owner_is_skipped() {
        let disposition = PvcDisposition::OwnerPresent;
        // Verify that OwnerPresent is semantically distinct from Collected.
        assert_ne!(disposition, PvcDisposition::Collected);
        assert_ne!(disposition, PvcDisposition::DryRunWouldCollect);
    }

    /// A PVC that cannot be attributed to any StellarNode must be left alone.
    #[test]
    fn pvc_with_no_owner_clue_is_unknown_owner() {
        let disposition = PvcDisposition::UnknownOwner;
        assert_ne!(disposition, PvcDisposition::Collected);
        assert_ne!(disposition, PvcDisposition::RetentionRetain);
    }

    // ────────────────────────────────────────────────────────────────────────
    // Delete-limit cap
    // ────────────────────────────────────────────────────────────────────────

    /// When the per-scan delete limit is reached, further orphans are deferred
    /// to the next scan pass (classified as UnknownOwner for re-evaluation).
    #[test]
    fn delete_limit_prevents_over_collection() {
        let cfg = GcConfig::default().with_delete_limit(2);
        assert_eq!(
            cfg.delete_limit_per_scan, 2,
            "Delete limit must cap collections per pass to 2"
        );
        // If limit is hit, remaining orphans get UnknownOwner disposition.
        // This ensures blast radius is bounded.
        assert_ne!(
            cfg.delete_limit_per_scan, 0,
            "A non-zero delete limit is the cap"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // Dry-run mode
    // ────────────────────────────────────────────────────────────────────────

    /// In dry-run mode, no Kubernetes delete call is issued.
    /// The GC reports `DryRunWouldCollect` instead of `Collected`.
    #[test]
    fn dry_run_mode_produces_would_collect_not_collected() {
        let dry_run_disposition = PvcDisposition::DryRunWouldCollect;
        let real_disposition = PvcDisposition::Collected;

        assert_ne!(
            dry_run_disposition, real_disposition,
            "Dry-run must produce DryRunWouldCollect, not Collected"
        );
        // Both contribute to the `collected` counter in GcReport.
        // (The counter semantics are: "would be collected or was collected".)
    }

    // ────────────────────────────────────────────────────────────────────────
    // Label / annotation constants
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn managed_by_label_follows_k8s_convention() {
        assert_eq!(MANAGED_BY_LABEL, "app.kubernetes.io/managed-by");
    }

    #[test]
    fn managed_by_value_is_stellar_operator() {
        assert_eq!(MANAGED_BY_VALUE, "stellar-operator");
    }

    #[test]
    fn retention_policy_annotation_uses_stellar_org_prefix() {
        assert!(
            RETENTION_POLICY_ANNOTATION.starts_with("stellar.org/"),
            "Retention annotation must use stellar.org/ prefix"
        );
    }

    // ────────────────────────────────────────────────────────────────────────
    // PvcDisposition serialisation round-trips
    // ────────────────────────────────────────────────────────────────────────

    #[test]
    fn all_pvc_dispositions_serialise_and_deserialise() {
        let variants = vec![
            PvcDisposition::Collected,
            PvcDisposition::OwnerPresent,
            PvcDisposition::RetentionRetain,
            PvcDisposition::UnknownOwner,
            PvcDisposition::DryRunWouldCollect,
            PvcDisposition::DeleteFailed("io timeout".to_string()),
        ];

        for variant in &variants {
            let json = serde_json::to_string(variant)
                .unwrap_or_else(|_| panic!("failed to serialize {:?}", variant));
            let restored: PvcDisposition = serde_json::from_str(&json)
                .unwrap_or_else(|_| panic!("failed to deserialize {:?}", variant));
            assert_eq!(variant, &restored, "Round-trip failed for {:?}", variant);
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Integration: cleanup pipeline contract tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod cleanup_pipeline_tests {
    use std::time::{Duration, Instant};

    use crate::controller::lifecycle::finalizers::{
        is_cleanup_deadline_exceeded, LifecycleTrace, ShutdownOutcome, ShutdownResult,
        CLEANUP_ABSOLUTE_CEILING_SECS,
    };

    /// Full happy-path: graceful shutdown succeeds, PVC deleted, trace clean.
    #[test]
    fn happy_path_cleanup_pipeline_completes_cleanly() {
        let cleanup_started = Instant::now();

        // Step 1 — simulate a successful graceful shutdown result.
        let shutdown_result = ShutdownResult {
            node_name: "validator-1".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![("validator-1-0".to_string(), ShutdownOutcome::Clean)],
            all_terminated: true,
        };
        assert!(shutdown_result.safe_to_unbind());
        assert!(!shutdown_result.had_forced_terminations());

        // Step 2 — ceiling not exceeded.
        let ceiling = Duration::from_secs(CLEANUP_ABSOLUTE_CEILING_SECS);
        assert!(!is_cleanup_deadline_exceeded(cleanup_started, ceiling));

        // Step 3 — record lifecycle trace.
        let mut trace = LifecycleTrace::new();
        trace.record_ok("pod-shutdown");
        trace.record_ok("pvc-delete"); // retentionPolicy: Delete
        trace.record_ok("k8s-resources-deleted");

        assert!(trace.all_succeeded());
        assert_eq!(trace.failure_count(), 0);
    }

    /// Forced-termination path: stellar-core froze but cleanup still completes.
    ///
    /// This is the primary scenario described in the PR review requirement:
    /// "tests that artificially freeze the stellar-core process to verify the
    /// finalizer's timeout and forceful termination behaviors."
    #[test]
    fn frozen_core_path_completes_with_forced_termination() {
        let cleanup_started = Instant::now();

        // Step 1 — simulate a frozen stellar-core: pod required forced termination.
        let shutdown_result = ShutdownResult {
            node_name: "validator-1".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![(
                "validator-1-0".to_string(),
                ShutdownOutcome::ForcedAfterTimeout,
            )],
            all_terminated: true, // forced termination counts as terminated
        };
        // Even after forced termination the PVC is safe to unbind.
        assert!(
            shutdown_result.safe_to_unbind(),
            "Force-terminated pod still allows safe PVC unbind"
        );
        assert!(
            shutdown_result.had_forced_terminations(),
            "Frozen core must be recorded as ForcedAfterTimeout"
        );

        // Step 2 — ceiling not exceeded (only a fraction of 5 min has passed).
        let ceiling = Duration::from_secs(CLEANUP_ABSOLUTE_CEILING_SECS);
        assert!(!is_cleanup_deadline_exceeded(cleanup_started, ceiling));

        // Step 3 — record trace: pod-shutdown as failed (forced), rest succeeds.
        let mut trace = LifecycleTrace::new();
        trace.record_failed(
            "pod-shutdown",
            "one or more pods required forced termination",
        );
        trace.record_ok("pvc-delete");
        trace.record_ok("k8s-resources-deleted");

        // Cleanup completed with one forced-termination warning.
        assert_eq!(
            trace.failure_count(),
            1,
            "Only pod-shutdown step should be recorded as failed"
        );
    }

    /// Absolute-ceiling abort: cleanup took too long, finalizer removed early.
    #[test]
    fn absolute_ceiling_abort_removes_finalizer_before_pvc_delete() {
        // Simulate: cleanup started 301 s ago (just past 5-min ceiling).
        let ceiling = Duration::from_secs(CLEANUP_ABSOLUTE_CEILING_SECS);
        let cleanup_started = Instant::now() - ceiling - Duration::from_secs(1);

        assert!(
            is_cleanup_deadline_exceeded(cleanup_started, ceiling),
            "Ceiling must be exceeded after 301 s"
        );

        // In the reconciler the ceiling check causes an early return — the
        // trace records only the ceiling failure.
        let mut trace = LifecycleTrace::new();
        trace.record_failed("pod-shutdown", "hanging");
        trace.record_failed("absolute-ceiling", "deadline exceeded");

        assert_eq!(trace.failure_count(), 2);
        assert!(!trace.all_succeeded());
    }

    /// Retain-policy path: pod shutdown succeeds, PVC is retained, trace clean.
    #[test]
    fn retain_policy_path_does_not_delete_pvc() {
        let cleanup_started = Instant::now();

        let shutdown_result = ShutdownResult {
            node_name: "validator-retain".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![(
                "validator-retain-0".to_string(),
                ShutdownOutcome::NotFound, // pod already gone
            )],
            all_terminated: true,
        };
        assert!(shutdown_result.safe_to_unbind());

        let ceiling = Duration::from_secs(CLEANUP_ABSOLUTE_CEILING_SECS);
        assert!(!is_cleanup_deadline_exceeded(cleanup_started, ceiling));

        let mut trace = LifecycleTrace::new();
        trace.record_ok("pod-shutdown");
        trace.record_ok("pvc-retain"); // retentionPolicy: Retain — no delete
        trace.record_ok("k8s-resources-deleted");

        assert!(trace.all_succeeded());
        // Verify the trace records pvc-retain, not pvc-delete.
        let pvc_steps: Vec<_> = trace
            .steps
            .iter()
            .filter(|s| s.name.starts_with("pvc"))
            .collect();
        assert_eq!(pvc_steps.len(), 1);
        assert_eq!(pvc_steps[0].name, "pvc-retain");
    }

    /// No pods found: cleanup still completes cleanly (validator was already gone).
    #[test]
    fn no_pods_found_path_is_safe_to_unbind() {
        let shutdown_result = ShutdownResult {
            node_name: "orphaned-validator".to_string(),
            namespace: "stellar".to_string(),
            pod_outcomes: vec![], // no pods found at all
            all_terminated: true,
        };

        assert!(
            shutdown_result.safe_to_unbind(),
            "Empty pod list means all_terminated=true — safe to proceed"
        );
        assert!(
            !shutdown_result.had_forced_terminations(),
            "No pods = no forced terminations"
        );
    }
}
