//! Reconcile logic for the ephemeral Kubernetes storage snapshot controller
//! (issue #243).
//!
//! The controller watches `VolumeSnapshot`-style resources, quiesces the
//! Stellar Core database through [`WriteCoordinator`] so the provider snapshot
//! is crash-consistent, asks the cloud provider for that snapshot through
//! [`CloudVolumeApi`], clones a fresh volume from it and renders the
//! `PersistentVolumeClaim` that lets a new Horizon/Stellar Core pod boot from
//! the clone instead of replaying the ledger from genesis.
//!
//! Reconciling the same resource twice is a no-op: results are memoised by the
//! deterministic snapshot name derived from the resource name, and a second
//! regeneration of the same resource returns `unchanged = true` without
//! touching the coordinator or the provider.

use std::collections::HashMap;

use super::cloud_api::{
    render_json_label_map, render_json_object, render_json_string, CloneRequest, CloudVolumeApi,
    SnapshotError, SnapshotHandle, SnapshotRequest, SnapshotResult, VolumeHandle, VolumeProvider,
    WriteCoordinator, SNAPSHOT_API_GROUP,
};

/// Cloud scope the provider adapters address resources in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloudScope {
    /// AWS region, e.g. `us-east-1`.
    pub region: String,
    /// GCP project id; ignored by the AWS adapter.
    pub project: String,
    /// GCP zone, e.g. `us-central1-a`; ignored by the AWS adapter.
    pub zone: String,
}

impl CloudScope {
    pub fn new(
        region: impl Into<String>,
        project: impl Into<String>,
        zone: impl Into<String>,
    ) -> Self {
        Self {
            region: region.into(),
            project: project.into(),
            zone: zone.into(),
        }
    }
}

/// A watched `VolumeSnapshot`-style resource spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeSnapshotSpec {
    /// Resource name; snapshot and claim names are derived from it.
    pub name: String,
    /// Namespace the generated claim lands in.
    pub namespace: String,
    /// Provider identifier, e.g. `aws-ebs` or `gcp-pd`.
    pub provider: String,
    /// Live volume/PVC the database writes to.
    pub source_volume: String,
    /// Requested clone size as a Kubernetes quantity, e.g. `10Gi`.
    pub size: String,
    /// Overrides [`VolumeProvider::default_storage_class`] when set.
    pub storage_class: Option<String>,
    /// Set when an operator cancelled the resource before it was actioned.
    pub cancelled: bool,
    pub labels: Vec<(String, String)>,
}

/// `spec.dataSource` of the generated claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimDataSource {
    pub api_group: String,
    pub kind: String,
    pub name: String,
}

/// A generated `PersistentVolumeClaim` manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistentVolumeClaim {
    pub name: String,
    pub namespace: String,
    pub storage_class: String,
    pub size_gib: u64,
    pub access_mode: String,
    pub data_source: ClaimDataSource,
    pub labels: Vec<(String, String)>,
}

impl PersistentVolumeClaim {
    /// `spec.resources.requests.storage` in canonical `<n>Gi` form.
    pub fn storage_request(&self) -> String {
        format!("{}Gi", self.size_gib)
    }

    /// Renders the claim as a single-line JSON manifest.
    pub fn to_json(&self) -> String {
        let metadata = render_json_object(&[
            ("name", render_json_string(&self.name)),
            ("namespace", render_json_string(&self.namespace)),
            ("labels", render_json_label_map(&self.labels)),
        ]);
        let resources = render_json_object(&[(
            "requests",
            render_json_object(&[("storage", render_json_string(&self.storage_request()))]),
        )]);
        let data_source = render_json_object(&[
            ("apiGroup", render_json_string(&self.data_source.api_group)),
            ("kind", render_json_string(&self.data_source.kind)),
            ("name", render_json_string(&self.data_source.name)),
        ]);
        let spec = render_json_object(&[
            (
                "accessModes",
                format!("[{}]", render_json_string(&self.access_mode)),
            ),
            ("storageClassName", render_json_string(&self.storage_class)),
            ("resources", resources),
            ("dataSource", data_source),
        ]);

        render_json_object(&[
            ("apiVersion", render_json_string("v1")),
            ("kind", render_json_string("PersistentVolumeClaim")),
            ("metadata", metadata),
            ("spec", spec),
        ])
    }

    /// Renders the claim as a YAML manifest.
    pub fn to_yaml(&self) -> String {
        let mut rendered = String::new();
        rendered.push_str("apiVersion: v1\n");
        rendered.push_str("kind: PersistentVolumeClaim\n");
        rendered.push_str("metadata:\n");
        rendered.push_str(&format!("  name: {}\n", render_yaml_scalar(&self.name)));
        rendered.push_str(&format!(
            "  namespace: {}\n",
            render_yaml_scalar(&self.namespace)
        ));
        if !self.labels.is_empty() {
            rendered.push_str("  labels:\n");
            for (key, value) in &self.labels {
                rendered.push_str(&format!(
                    "    {}: {}\n",
                    render_yaml_scalar(key),
                    render_yaml_scalar(value)
                ));
            }
        }
        rendered.push_str("spec:\n");
        rendered.push_str("  accessModes:\n");
        rendered.push_str(&format!(
            "    - {}\n",
            render_yaml_scalar(&self.access_mode)
        ));
        rendered.push_str(&format!(
            "  storageClassName: {}\n",
            render_yaml_scalar(&self.storage_class)
        ));
        rendered.push_str("  resources:\n");
        rendered.push_str("    requests:\n");
        rendered.push_str(&format!("      storage: {}\n", self.storage_request()));
        rendered.push_str("  dataSource:\n");
        rendered.push_str(&format!(
            "    apiGroup: {}\n",
            render_yaml_scalar(&self.data_source.api_group)
        ));
        rendered.push_str(&format!(
            "    kind: {}\n",
            render_yaml_scalar(&self.data_source.kind)
        ));
        rendered.push_str(&format!(
            "    name: {}\n",
            render_yaml_scalar(&self.data_source.name)
        ));
        rendered
    }
}

/// A spec that passed validation, with the derived names and claim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedSnapshot {
    pub spec_name: String,
    pub provider: VolumeProvider,
    pub source_volume: String,
    pub size_gib: u64,
    pub snapshot_name: String,
    pub labels: Vec<(String, String)>,
    pub claim: PersistentVolumeClaim,
}

/// Phase the resource reached during the last reconcile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotPhase {
    /// Nothing has been requested yet.
    Pending,
    /// Writes are paused and the provider snapshot is in flight.
    Snapshotting,
    /// The snapshot exists but no clone has been made from it yet.
    SnapshotReady,
    /// Snapshot, clone and claim manifest are all available.
    ClaimReady,
    /// The last reconcile failed; the error is reported separately.
    Failed,
}

/// A Kubernetes-style status condition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Condition {
    pub kind: String,
    pub status: String,
    pub reason: String,
    pub message: String,
}

/// Structured result of reconciling one resource.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotOutcome {
    pub name: String,
    pub phase: SnapshotPhase,
    pub snapshot: Option<SnapshotHandle>,
    pub volume: Option<VolumeHandle>,
    pub claim: Option<PersistentVolumeClaim>,
    pub conditions: Vec<Condition>,
    /// `true` when this outcome was replayed from a previous reconcile.
    pub unchanged: bool,
    pub requeue: bool,
}

impl SnapshotOutcome {
    fn ready(
        validated: &ValidatedSnapshot,
        snapshot: SnapshotHandle,
        volume: VolumeHandle,
    ) -> Self {
        let conditions = vec![
            Condition {
                kind: "SnapshotReady".to_string(),
                status: "True".to_string(),
                reason: "SnapshotCreated".to_string(),
                message: format!(
                    "provider snapshot {} of volume {} is available for cloning",
                    snapshot.snapshot_id, snapshot.source_volume
                ),
            },
            Condition {
                kind: "ClaimProvisioned".to_string(),
                status: "True".to_string(),
                reason: "ClaimRendered".to_string(),
                message: format!(
                    "claim {} initialises from snapshot {} through storage class {}",
                    validated.claim.name, validated.snapshot_name, validated.claim.storage_class
                ),
            },
        ];

        Self {
            name: validated.spec_name.clone(),
            phase: SnapshotPhase::ClaimReady,
            snapshot: Some(snapshot),
            volume: Some(volume),
            claim: Some(validated.claim.clone()),
            conditions,
            unchanged: false,
            requeue: false,
        }
    }
}

/// Deterministic snapshot name for a resource.
pub fn snapshot_name_for(resource_name: &str) -> String {
    format!("{resource_name}-snapshot")
}

/// Deterministic claim name for a resource.
pub fn claim_name_for(resource_name: &str) -> String {
    format!("{resource_name}-clone")
}

/// Validates a resource and derives the names and claim manifest.
///
/// Every rejection maps onto an explicit [`SnapshotError`] so the controller can
/// surface it as a status condition instead of silently skipping the resource.
pub fn validate(spec: &VolumeSnapshotSpec) -> SnapshotResult<ValidatedSnapshot> {
    if spec.cancelled {
        return Err(SnapshotError::SnapshotCancelled {
            name: spec.name.clone(),
        });
    }

    let source_volume = spec.source_volume.trim();
    if source_volume.is_empty() {
        return Err(SnapshotError::MissingSourceVolume);
    }

    let provider = VolumeProvider::parse(&spec.provider)?;
    let size_gib = parse_size_gib(&spec.size)?;

    let snapshot_name = snapshot_name_for(&spec.name);
    let namespace = if spec.namespace.trim().is_empty() {
        "default".to_string()
    } else {
        spec.namespace.trim().to_string()
    };
    let storage_class = spec
        .storage_class
        .as_deref()
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| provider.default_storage_class().to_string());

    let claim = PersistentVolumeClaim {
        name: claim_name_for(&spec.name),
        namespace,
        storage_class,
        size_gib,
        access_mode: "ReadWriteOnce".to_string(),
        data_source: ClaimDataSource {
            api_group: SNAPSHOT_API_GROUP.to_string(),
            kind: "VolumeSnapshot".to_string(),
            name: snapshot_name.clone(),
        },
        labels: spec.labels.clone(),
    };

    Ok(ValidatedSnapshot {
        spec_name: spec.name.clone(),
        provider,
        source_volume: source_volume.to_string(),
        size_gib,
        snapshot_name,
        labels: spec.labels.clone(),
        claim,
    })
}

const KIB: u64 = 1024;
const MIB: u64 = 1024 * KIB;
const GIB: u64 = 1024 * MIB;
const TIB: u64 = 1024 * GIB;

/// Parses a Kubernetes-style quantity into whole gibibytes, rounding up so a
/// clone is never requested smaller than the source volume.
///
/// Accepts binary suffixes (`Ki`/`Mi`/`Gi`/`Ti`, with or without the trailing
/// `B`), their decimal spellings, and a bare byte count. Anything else --
/// negative, fractional, zero or an unknown suffix -- is a malformed size.
pub fn parse_size_gib(raw: &str) -> SnapshotResult<u64> {
    let trimmed = raw.trim();
    let malformed = || SnapshotError::MalformedSize {
        requested: trimmed.to_string(),
    };

    if trimmed.is_empty() {
        return Err(malformed());
    }

    let normalized = trimmed.to_ascii_lowercase();
    let (digits, unit) = split_size_suffix(&normalized);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(malformed());
    }

    let value: u64 = match digits.parse() {
        Ok(value) => value,
        Err(_) => return Err(malformed()),
    };
    let bytes = match value.checked_mul(unit) {
        Some(bytes) => bytes,
        None => return Err(malformed()),
    };
    if bytes == 0 {
        return Err(malformed());
    }

    let whole = bytes / GIB;
    if bytes % GIB == 0 {
        Ok(whole)
    } else {
        Ok(whole + 1)
    }
}

fn split_size_suffix(normalized: &str) -> (&str, u64) {
    const SUFFIXES: [(&str, u64); 16] = [
        ("tib", TIB),
        ("tb", TIB),
        ("ti", TIB),
        ("t", TIB),
        ("gib", GIB),
        ("gb", GIB),
        ("gi", GIB),
        ("g", GIB),
        ("mib", MIB),
        ("mb", MIB),
        ("mi", MIB),
        ("m", MIB),
        ("kib", KIB),
        ("kb", KIB),
        ("ki", KIB),
        ("k", KIB),
    ];

    for (suffix, multiplier) in SUFFIXES {
        if let Some(rest) = normalized.strip_suffix(suffix) {
            return (rest, multiplier);
        }
    }

    (normalized, 1)
}

/// Reconciles `VolumeSnapshot`-style resources into claim manifests.
pub struct SnapshotReconciler<C: CloudVolumeApi, W: WriteCoordinator> {
    cloud: C,
    coordinator: W,
    scope: CloudScope,
    records: HashMap<String, Record>,
}

struct Record {
    source_volume: String,
    outcome: SnapshotOutcome,
}

impl<C: CloudVolumeApi, W: WriteCoordinator> SnapshotReconciler<C, W> {
    pub fn new(cloud: C, coordinator: W, scope: CloudScope) -> Self {
        Self {
            cloud,
            coordinator,
            scope,
            records: HashMap::new(),
        }
    }

    /// Reconciles one resource.
    ///
    /// Validation failures are returned as [`SnapshotError`]; everything else
    /// yields a [`SnapshotOutcome`] that is memoised for the next reconcile of
    /// the same resource.
    pub fn reconcile(&mut self, spec: &VolumeSnapshotSpec) -> SnapshotResult<SnapshotOutcome> {
        let validated = validate(spec)?;

        if let Some(record) = self.records.get(&validated.snapshot_name) {
            if record.source_volume != validated.source_volume {
                return Err(SnapshotError::DuplicateSnapshot {
                    name: validated.snapshot_name.clone(),
                });
            }

            let mut outcome = record.outcome.clone();
            outcome.unchanged = true;
            outcome.requeue = false;
            return Ok(outcome);
        }

        let outcome = self.execute(&validated)?;
        self.records.insert(
            validated.snapshot_name.clone(),
            Record {
                source_volume: validated.source_volume.clone(),
                outcome: outcome.clone(),
            },
        );
        Ok(outcome)
    }

    fn execute(&mut self, validated: &ValidatedSnapshot) -> SnapshotResult<SnapshotOutcome> {
        let request = SnapshotRequest {
            provider: validated.provider,
            source_volume: validated.source_volume.clone(),
            snapshot_name: validated.snapshot_name.clone(),
            size_gib: validated.size_gib,
            region: self.scope.region.clone(),
            project: self.scope.project.clone(),
            zone: self.scope.zone.clone(),
            labels: validated.labels.clone(),
        };

        // Quiesce the database first: the provider snapshot is only
        // crash-consistent once in-flight writes have hit durable storage.
        self.coordinator.pause_writes(&validated.source_volume)?;

        // Writes are paused from here on, so every path below must resume them.
        let snapshot_result = match self.coordinator.fsync(&validated.source_volume) {
            Ok(()) => self.cloud.create_snapshot(&request),
            Err(error) => Err(error),
        };
        let resume_result = self.coordinator.resume_writes(&validated.source_volume);
        let snapshot = match snapshot_result {
            Ok(snapshot) => {
                resume_result?;
                snapshot
            }
            Err(error) => return Err(error),
        };

        let clone = self.cloud.clone_volume(&CloneRequest {
            provider: validated.provider,
            snapshot_id: snapshot.snapshot_id.clone(),
            target_volume_name: format!("{}-volume", validated.spec_name),
            size_gib: validated.size_gib,
            region: self.scope.region.clone(),
            project: self.scope.project.clone(),
            zone: self.scope.zone.clone(),
        })?;

        Ok(SnapshotOutcome::ready(validated, snapshot, clone))
    }
}

/// Quotes a YAML scalar when leaving it bare would change the document.
fn render_yaml_scalar(value: &str) -> String {
    let needs_quoting = value.is_empty()
        || value
            .chars()
            .any(|character| matches!(character, ':' | '#' | '"' | '\'' | '\n' | '\t'))
        || value.starts_with('-')
        || value.starts_with(' ')
        || value.ends_with(' ');

    if needs_quoting {
        let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
        format!("\"{escaped}\"")
    } else {
        value.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// Shared, ordered record of everything the reconciler touched.
    #[derive(Clone, Debug, Default)]
    struct EventLog(Rc<RefCell<Vec<String>>>);

    impl EventLog {
        fn new() -> Self {
            Self(Rc::new(RefCell::new(Vec::new())))
        }

        fn record(&self, entry: impl Into<String>) {
            self.0.borrow_mut().push(entry.into());
        }

        fn entries(&self) -> Vec<String> {
            self.0.borrow().clone()
        }
    }

    struct FakeCloud {
        log: EventLog,
        fail_snapshot: bool,
        fail_clone: bool,
    }

    impl CloudVolumeApi for FakeCloud {
        fn create_snapshot(&mut self, request: &SnapshotRequest) -> SnapshotResult<SnapshotHandle> {
            self.log
                .record(format!("snapshot:{}", request.source_volume));
            if self.fail_snapshot {
                return Err(SnapshotError::Provider {
                    provider: request.provider.as_str().to_string(),
                    message: "snapshot quota exceeded".to_string(),
                });
            }
            Ok(SnapshotHandle {
                snapshot_id: format!("snap-{}", request.source_volume),
                provider: request.provider,
                source_volume: request.source_volume.clone(),
                size_gib: request.size_gib,
                ready: true,
            })
        }

        fn clone_volume(&mut self, request: &CloneRequest) -> SnapshotResult<VolumeHandle> {
            self.log.record(format!("clone:{}", request.snapshot_id));
            if self.fail_clone {
                return Err(SnapshotError::Provider {
                    provider: request.provider.as_str().to_string(),
                    message: "volume quota exceeded".to_string(),
                });
            }
            Ok(VolumeHandle {
                volume_id: format!("vol-from-{}", request.snapshot_id),
                provider: request.provider,
                source_snapshot_id: request.snapshot_id.clone(),
                size_gib: request.size_gib,
                ready: true,
            })
        }
    }

    struct FakeCoordinator {
        log: EventLog,
        fail_pause: bool,
        fail_fsync: bool,
        fail_resume: bool,
    }

    impl FakeCoordinator {
        fn coordination_error(&self, operation: &str) -> SnapshotError {
            SnapshotError::WriteCoordination {
                volume_id: "pvc-db-0".to_string(),
                operation: operation.to_string(),
                message: "stellar-core refused the request".to_string(),
            }
        }
    }

    impl WriteCoordinator for FakeCoordinator {
        fn pause_writes(&mut self, volume_id: &str) -> SnapshotResult<()> {
            self.log.record(format!("pause:{volume_id}"));
            if self.fail_pause {
                return Err(self.coordination_error("pause"));
            }
            Ok(())
        }

        fn fsync(&mut self, volume_id: &str) -> SnapshotResult<()> {
            self.log.record(format!("fsync:{volume_id}"));
            if self.fail_fsync {
                return Err(self.coordination_error("fsync"));
            }
            Ok(())
        }

        fn resume_writes(&mut self, volume_id: &str) -> SnapshotResult<()> {
            self.log.record(format!("resume:{volume_id}"));
            if self.fail_resume {
                return Err(self.coordination_error("resume"));
            }
            Ok(())
        }
    }

    struct Harness {
        reconciler: SnapshotReconciler<FakeCloud, FakeCoordinator>,
        log: EventLog,
    }

    impl Harness {
        fn new(
            fail_snapshot: bool,
            fail_clone: bool,
            fail_pause: bool,
            fail_fsync: bool,
            fail_resume: bool,
        ) -> Self {
            let log = EventLog::new();
            let cloud = FakeCloud {
                log: log.clone(),
                fail_snapshot,
                fail_clone,
            };
            let coordinator = FakeCoordinator {
                log: log.clone(),
                fail_pause,
                fail_fsync,
                fail_resume,
            };
            let reconciler = SnapshotReconciler::new(
                cloud,
                coordinator,
                CloudScope::new("us-east-1", "stellar-prod", "us-central1-a"),
            );
            Self { reconciler, log }
        }

        fn healthy() -> Self {
            Self::new(false, false, false, false, false)
        }
    }

    fn spec() -> VolumeSnapshotSpec {
        VolumeSnapshotSpec {
            name: "horizon-db".to_string(),
            namespace: "stellar".to_string(),
            provider: "aws-ebs".to_string(),
            source_volume: "pvc-db-0".to_string(),
            size: "10Gi".to_string(),
            storage_class: None,
            cancelled: false,
            labels: vec![("app".to_string(), "horizon".to_string())],
        }
    }

    #[test]
    fn reconcile_builds_claim_from_snapshot() {
        let mut harness = Harness::healthy();
        let outcome = harness.reconciler.reconcile(&spec()).unwrap();

        assert_eq!(outcome.phase, SnapshotPhase::ClaimReady);
        assert!(!outcome.unchanged);
        assert!(!outcome.requeue);
        assert_eq!(
            outcome.snapshot.as_ref().unwrap().snapshot_id,
            "snap-pvc-db-0"
        );
        assert!(outcome.snapshot.as_ref().unwrap().ready);
        assert_eq!(
            outcome.volume.as_ref().unwrap().source_snapshot_id,
            "snap-pvc-db-0"
        );

        let claim = outcome.claim.as_ref().unwrap();
        assert_eq!(claim.name, "horizon-db-clone");
        assert_eq!(claim.namespace, "stellar");
        assert_eq!(claim.storage_class, "ebs-sc");
        assert_eq!(claim.storage_request(), "10Gi");
        assert_eq!(claim.access_mode, "ReadWriteOnce");
        assert_eq!(claim.data_source.api_group, SNAPSHOT_API_GROUP);
        assert_eq!(claim.data_source.kind, "VolumeSnapshot");
        assert_eq!(claim.data_source.name, "horizon-db-snapshot");

        assert_eq!(outcome.conditions.len(), 2);
        assert_eq!(outcome.conditions[0].kind, "SnapshotReady");
        assert_eq!(outcome.conditions[1].kind, "ClaimProvisioned");
        assert_eq!(outcome.conditions[1].status, "True");
    }

    #[test]
    fn claim_renders_the_snapshot_data_source_in_yaml() {
        let mut harness = Harness::healthy();
        let outcome = harness.reconciler.reconcile(&spec()).unwrap();
        let claim = outcome.claim.unwrap();

        let yaml = claim.to_yaml();
        assert!(yaml.starts_with("apiVersion: v1\nkind: PersistentVolumeClaim\n"));
        assert!(yaml.contains("  name: horizon-db-clone\n"));
        assert!(yaml.contains("  namespace: stellar\n"));
        assert!(yaml.contains("  labels:\n    app: horizon\n"));
        assert!(yaml.contains("storageClassName: ebs-sc\n"));
        assert!(yaml.contains("      storage: 10Gi\n"));
        assert!(yaml.contains(
            "  dataSource:\n    apiGroup: snapshot.storage.k8s.io\n    kind: VolumeSnapshot\n    \
             name: horizon-db-snapshot\n"
        ));
    }

    #[test]
    fn claim_renders_the_snapshot_data_source_in_json() {
        let mut harness = Harness::healthy();
        let outcome = harness.reconciler.reconcile(&spec()).unwrap();

        assert_eq!(
            outcome.claim.unwrap().to_json(),
            "{\"apiVersion\":\"v1\",\"kind\":\"PersistentVolumeClaim\",\"metadata\":{\"name\":\
             \"horizon-db-clone\",\"namespace\":\"stellar\",\"labels\":{\"app\":\"horizon\"}},\
             \"spec\":{\"accessModes\":[\"ReadWriteOnce\"],\"storageClassName\":\"ebs-sc\",\
             \"resources\":{\"requests\":{\"storage\":\"10Gi\"}},\"dataSource\":{\"apiGroup\":\
             \"snapshot.storage.k8s.io\",\"kind\":\"VolumeSnapshot\",\"name\":\
             \"horizon-db-snapshot\"}}}"
        );
    }

    #[test]
    fn pauses_flushes_snapshots_resumes_then_clones() {
        let mut harness = Harness::healthy();
        harness.reconciler.reconcile(&spec()).unwrap();

        assert_eq!(
            harness.log.entries(),
            vec![
                "pause:pvc-db-0",
                "fsync:pvc-db-0",
                "snapshot:pvc-db-0",
                "resume:pvc-db-0",
                "clone:snap-pvc-db-0",
            ]
        );
    }

    #[test]
    fn resumes_writes_when_the_snapshot_fails() {
        let mut harness = Harness::new(true, false, false, false, false);
        let error = harness.reconciler.reconcile(&spec()).unwrap_err();

        assert!(matches!(error, SnapshotError::Provider { .. }));
        assert_eq!(
            harness.log.entries(),
            vec![
                "pause:pvc-db-0",
                "fsync:pvc-db-0",
                "snapshot:pvc-db-0",
                "resume:pvc-db-0",
            ]
        );
    }

    #[test]
    fn resumes_writes_when_the_flush_fails() {
        let mut harness = Harness::new(false, false, false, true, false);
        let error = harness.reconciler.reconcile(&spec()).unwrap_err();

        assert!(matches!(error, SnapshotError::WriteCoordination { .. }));
        assert_eq!(
            harness.log.entries(),
            vec!["pause:pvc-db-0", "fsync:pvc-db-0", "resume:pvc-db-0"]
        );
    }

    #[test]
    fn does_not_resume_when_the_pause_never_happened() {
        let mut harness = Harness::new(false, false, true, false, false);
        let error = harness.reconciler.reconcile(&spec()).unwrap_err();

        assert!(matches!(error, SnapshotError::WriteCoordination { .. }));
        assert_eq!(harness.log.entries(), vec!["pause:pvc-db-0"]);
    }

    #[test]
    fn surfaces_a_resume_failure_after_successful_snapshot() {
        let mut harness = Harness::new(false, false, false, false, true);
        let error = harness.reconciler.reconcile(&spec()).unwrap_err();

        assert!(matches!(error, SnapshotError::WriteCoordination { .. }));
        assert_eq!(
            harness.log.entries(),
            vec![
                "pause:pvc-db-0",
                "fsync:pvc-db-0",
                "snapshot:pvc-db-0",
                "resume:pvc-db-0",
            ]
        );
    }

    #[test]
    fn clone_failure_leaves_writes_resumed() {
        let mut harness = Harness::new(false, true, false, false, false);
        let error = harness.reconciler.reconcile(&spec()).unwrap_err();

        assert!(matches!(error, SnapshotError::Provider { .. }));
        let entries = harness.log.entries();
        assert_eq!(
            entries.last().map(String::as_str),
            Some("clone:snap-pvc-db-0")
        );
        assert!(entries.contains(&"resume:pvc-db-0".to_string()));
    }

    #[test]
    fn reconcile_is_idempotent() {
        let mut harness = Harness::healthy();
        let first = harness.reconciler.reconcile(&spec()).unwrap();
        let second = harness.reconciler.reconcile(&spec()).unwrap();

        assert!(!first.unchanged);
        assert!(second.unchanged);
        assert_eq!(second.phase, SnapshotPhase::ClaimReady);
        assert_eq!(second.snapshot, first.snapshot);
        assert_eq!(second.volume, first.volume);
        assert_eq!(second.claim, first.claim);

        // The provider and the coordinator were only driven once.
        assert_eq!(
            harness.log.entries(),
            vec![
                "pause:pvc-db-0",
                "fsync:pvc-db-0",
                "snapshot:pvc-db-0",
                "resume:pvc-db-0",
                "clone:snap-pvc-db-0",
            ]
        );
    }

    #[test]
    fn duplicate_snapshot_name_with_a_different_source_is_rejected() {
        let mut harness = Harness::healthy();
        harness.reconciler.reconcile(&spec()).unwrap();

        let mut conflicting = spec();
        conflicting.source_volume = "pvc-db-1".to_string();
        let error = harness.reconciler.reconcile(&conflicting).unwrap_err();

        assert_eq!(
            error,
            SnapshotError::DuplicateSnapshot {
                name: "horizon-db-snapshot".to_string()
            }
        );
        // The conflicting reconcile never touched the provider.
        assert_eq!(harness.log.entries().len(), 5);
    }

    #[test]
    fn rejects_cancelled_snapshots() {
        let mut harness = Harness::healthy();
        let mut cancelled = spec();
        cancelled.cancelled = true;

        let error = harness.reconciler.reconcile(&cancelled).unwrap_err();
        assert_eq!(
            error,
            SnapshotError::SnapshotCancelled {
                name: "horizon-db".to_string()
            }
        );
        assert!(harness.log.entries().is_empty());
    }

    #[test]
    fn rejects_missing_source_volume() {
        let mut harness = Harness::healthy();
        let mut missing = spec();
        missing.source_volume = "   ".to_string();

        let error = harness.reconciler.reconcile(&missing).unwrap_err();
        assert_eq!(error, SnapshotError::MissingSourceVolume);
        assert!(harness.log.entries().is_empty());
    }

    #[test]
    fn rejects_unsupported_providers_without_touching_the_cloud() {
        let mut harness = Harness::healthy();
        let mut unsupported = spec();
        unsupported.provider = "azure-disk".to_string();

        let error = harness.reconciler.reconcile(&unsupported).unwrap_err();
        assert_eq!(
            error,
            SnapshotError::UnsupportedProvider {
                provider: "azure-disk".to_string()
            }
        );
        assert!(harness.log.entries().is_empty());
    }

    #[test]
    fn rejects_malformed_sizes() {
        let mut harness = Harness::healthy();
        for malformed in ["", "   ", "ten", "10Gigabytes", "-5Gi", "1.5Gi", "0", "0Gi"] {
            let mut invalid = spec();
            invalid.size = malformed.to_string();

            let error = harness.reconciler.reconcile(&invalid).unwrap_err();
            assert_eq!(
                error,
                SnapshotError::MalformedSize {
                    requested: malformed.trim().to_string()
                },
                "size {malformed:?} should be rejected"
            );
        }
        assert!(harness.log.entries().is_empty());
    }

    #[test]
    fn parses_supported_size_units_into_gibibytes() {
        assert_eq!(parse_size_gib("10Gi").unwrap(), 10);
        assert_eq!(parse_size_gib(" 20gib ").unwrap(), 20);
        assert_eq!(parse_size_gib("3072Mi").unwrap(), 3);
        assert_eq!(parse_size_gib("1TiB").unwrap(), 1024);
        assert_eq!(parse_size_gib("1073741824").unwrap(), 1);
    }

    #[test]
    fn gcp_resources_bind_to_the_pd_storage_class() {
        let mut harness = Harness::healthy();
        let mut gcp = spec();
        gcp.provider = "gcp-pd".to_string();

        let outcome = harness.reconciler.reconcile(&gcp).unwrap();
        let claim = outcome.claim.unwrap();
        assert_eq!(claim.storage_class, "pd-standard");
        assert_eq!(outcome.snapshot.unwrap().provider, VolumeProvider::GcpPd);
    }

    #[test]
    fn explicit_storage_class_overrides_the_provider_default() {
        let mut harness = Harness::healthy();
        let mut custom = spec();
        custom.storage_class = Some("  fast-ssd  ".to_string());

        let outcome = harness.reconciler.reconcile(&custom).unwrap();
        assert_eq!(outcome.claim.unwrap().storage_class, "fast-ssd");
    }

    #[test]
    fn validate_defaults_the_namespace_and_derives_names() {
        let mut anonymous = spec();
        anonymous.namespace = String::new();

        let validated = validate(&anonymous).unwrap();
        assert_eq!(validated.claim.namespace, "default");
        assert_eq!(validated.snapshot_name, "horizon-db-snapshot");
        assert_eq!(validated.claim.name, "horizon-db-clone");
        assert_eq!(validated.source_volume, "pvc-db-0");
        assert_eq!(validated.size_gib, 10);
        assert_eq!(validated.provider, VolumeProvider::AwsEbs);
    }

    #[test]
    fn yaml_scalars_are_quoted_only_when_needed() {
        assert_eq!(render_yaml_scalar("horizon"), "horizon");
        assert_eq!(render_yaml_scalar(""), "\"\"");
        assert_eq!(render_yaml_scalar("a: b"), "\"a: b\"");
        assert_eq!(render_yaml_scalar("core #1"), "\"core #1\"");
    }
}
