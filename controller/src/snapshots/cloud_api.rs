//! Provider-agnostic cloud volume API for the ephemeral snapshot controller
//! (issue #243).
//!
//! This module performs no I/O. It defines the two traits the reconciler
//! depends on -- [`CloudVolumeApi`] for the snapshot/clone calls and
//! [`WriteCoordinator`] for the `fsync`/pause handshake with Stellar Core --
//! together with the AWS EBS and GCP PD request/response value types and the
//! pure mapping logic that turns those provider-shaped payloads into the
//! provider-neutral [`SnapshotHandle`] and [`VolumeHandle`] consumed by
//! `snapshots::volume`.

use std::fmt;

/// Storage class that claims carved out of an EBS snapshot bind to.
pub const AWS_EBS_STORAGE_CLASS: &str = "ebs-sc";
/// Storage class that claims carved out of a GCP PD snapshot bind to.
pub const GCP_PD_STORAGE_CLASS: &str = "pd-standard";
/// API group of the `VolumeSnapshot` custom resource used as a claim source.
pub const SNAPSHOT_API_GROUP: &str = "snapshot.storage.k8s.io";

/// Result alias for the snapshot subsystem.
pub type SnapshotResult<T> = Result<T, SnapshotError>;

/// Errors surfaced by the snapshot subsystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SnapshotError {
    /// The resource did not reference a source volume to snapshot.
    MissingSourceVolume,
    /// The requested size was empty, non-numeric or used an unknown suffix.
    MalformedSize { requested: String },
    /// The resource named a provider this controller has no adapter for.
    UnsupportedProvider { provider: String },
    /// A resource with the same snapshot name but a different source volume is
    /// already tracked; actioning both would produce an inconsistent clone.
    DuplicateSnapshot { name: String },
    /// The resource carries a cancellation marker and must not be actioned.
    SnapshotCancelled { name: String },
    /// A provider-specific payload was handed to the wrong adapter.
    ProviderMismatch { expected: String, actual: String },
    /// The provider reported a failure or returned an unparseable payload.
    Provider { provider: String, message: String },
    /// The pause/fsync/resume handshake with the database failed.
    WriteCoordination {
        volume_id: String,
        operation: String,
        message: String,
    },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSourceVolume => {
                write!(
                    f,
                    "volume snapshot resource does not reference a source volume"
                )
            }
            Self::MalformedSize { requested } => write!(f, "malformed volume size {requested:?}"),
            Self::UnsupportedProvider { provider } => {
                write!(f, "unsupported volume provider {provider:?}")
            }
            Self::DuplicateSnapshot { name } => {
                write!(
                    f,
                    "snapshot {name:?} is already bound to a different source volume"
                )
            }
            Self::SnapshotCancelled { name } => {
                write!(
                    f,
                    "snapshot {name:?} was cancelled and will not be actioned"
                )
            }
            Self::ProviderMismatch { expected, actual } => {
                write!(f, "provider mismatch: expected {expected}, got {actual}")
            }
            Self::Provider { provider, message } => {
                write!(f, "{provider} provider error: {message}")
            }
            Self::WriteCoordination {
                volume_id,
                operation,
                message,
            } => write!(f, "failed to {operation} writes on {volume_id}: {message}"),
        }
    }
}

impl std::error::Error for SnapshotError {}

/// Cloud providers the controller ships an adapter for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeProvider {
    /// Amazon Elastic Block Store, snapshotted through EC2 `CreateSnapshot`.
    AwsEbs,
    /// Google Compute Engine persistent disks, snapshotted through
    /// `compute.disks.createSnapshot`.
    GcpPd,
}

impl VolumeProvider {
    /// Parses the provider string carried by a `VolumeSnapshot` spec.
    pub fn parse(raw: &str) -> SnapshotResult<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "aws" | "ebs" | "aws-ebs" => Ok(Self::AwsEbs),
            "gcp" | "gce" | "pd" | "gcp-pd" => Ok(Self::GcpPd),
            _ => Err(SnapshotError::UnsupportedProvider {
                provider: raw.trim().to_string(),
            }),
        }
    }

    /// Canonical provider identifier.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AwsEbs => "aws-ebs",
            Self::GcpPd => "gcp-pd",
        }
    }

    /// Default storage class a generated claim binds to.
    pub fn default_storage_class(self) -> &'static str {
        match self {
            Self::AwsEbs => AWS_EBS_STORAGE_CLASS,
            Self::GcpPd => GCP_PD_STORAGE_CLASS,
        }
    }
}

/// Provider-neutral description of the snapshot to take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotRequest {
    pub provider: VolumeProvider,
    /// Identifier of the live volume/PVC the database currently writes to.
    pub source_volume: String,
    /// Name the resulting snapshot is registered under.
    pub snapshot_name: String,
    /// Requested size in gibibytes.
    pub size_gib: u64,
    /// AWS region; unused by the GCP adapter.
    pub region: String,
    /// GCP project id; unused by the AWS adapter.
    pub project: String,
    /// GCP zone; unused by the AWS adapter.
    pub zone: String,
    pub labels: Vec<(String, String)>,
}

/// Provider-neutral handle for a snapshot that exists in the cloud account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotHandle {
    pub snapshot_id: String,
    pub provider: VolumeProvider,
    pub source_volume: String,
    pub size_gib: u64,
    /// `false` while the provider is still materialising the snapshot.
    pub ready: bool,
}

/// Provider-neutral description of the volume clone to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloneRequest {
    pub provider: VolumeProvider,
    pub snapshot_id: String,
    pub target_volume_name: String,
    pub size_gib: u64,
    pub region: String,
    pub project: String,
    pub zone: String,
}

/// Provider-neutral handle for the volume cloned from a snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeHandle {
    pub volume_id: String,
    pub provider: VolumeProvider,
    pub source_snapshot_id: String,
    pub size_gib: u64,
    pub ready: bool,
}

/// Crash-consistent snapshot and volume-clone operations.
///
/// Implementations are thin adapters over the cloud SDKs; keeping the trait
/// synchronous and provider-neutral is what lets `snapshots::volume` be tested
/// without a cluster or network access.
pub trait CloudVolumeApi {
    /// Requests a crash-consistent snapshot of `request.source_volume`.
    fn create_snapshot(&mut self, request: &SnapshotRequest) -> SnapshotResult<SnapshotHandle>;

    /// Creates a brand-new volume initialised from `request.snapshot_id`.
    fn clone_volume(&mut self, request: &CloneRequest) -> SnapshotResult<VolumeHandle>;
}

/// Coordination handshake with the database that owns the source volume.
///
/// A cloud snapshot is only crash-consistent if in-flight writes have reached
/// durable storage, so the reconciler pauses writes, asks the database to
/// `fsync`, and only then asks the provider for the snapshot. `fsync` defaulting
/// to a no-op keeps the trait usable for providers (or tests) that only support
/// the pause/resume half of the handshake.
pub trait WriteCoordinator {
    /// Stops accepting new writes on `volume_id`.
    fn pause_writes(&mut self, volume_id: &str) -> SnapshotResult<()>;

    /// Flushes pending writes to durable storage.
    fn fsync(&mut self, _volume_id: &str) -> SnapshotResult<()> {
        Ok(())
    }

    /// Resumes accepting writes on `volume_id`.
    fn resume_writes(&mut self, volume_id: &str) -> SnapshotResult<()>;
}

/// AWS EC2 `CreateSnapshot` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsEbsCreateSnapshotRequest {
    pub region: String,
    pub volume_id: String,
    pub description: String,
    pub tags: Vec<(String, String)>,
}

impl AwsEbsCreateSnapshotRequest {
    /// EC2 API version the rendered query string targets.
    pub const API_VERSION: &'static str = "2016-11-15";

    /// Maps a provider-neutral request onto the EC2 payload.
    pub fn from_snapshot_request(request: &SnapshotRequest) -> SnapshotResult<Self> {
        if request.provider != VolumeProvider::AwsEbs {
            return Err(SnapshotError::ProviderMismatch {
                expected: VolumeProvider::AwsEbs.as_str().to_string(),
                actual: request.provider.as_str().to_string(),
            });
        }
        Ok(Self {
            region: request.region.clone(),
            volume_id: request.source_volume.clone(),
            description: format!("crash-consistent snapshot for {}", request.snapshot_name),
            tags: request.labels.clone(),
        })
    }

    /// Query-string key/value pairs for `?Action=CreateSnapshot`.
    pub fn to_query_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("Action".to_string(), "CreateSnapshot".to_string()),
            ("Version".to_string(), Self::API_VERSION.to_string()),
            ("VolumeId".to_string(), self.volume_id.clone()),
            ("Description".to_string(), self.description.clone()),
        ];
        for (index, (key, value)) in self.tags.iter().enumerate() {
            let ordinal = index + 1;
            pairs.push((
                format!("TagSpecification.{ordinal}.ResourceType"),
                "snapshot".to_string(),
            ));
            pairs.push((format!("TagSpecification.{ordinal}.Tag.1.Key"), key.clone()));
            pairs.push((
                format!("TagSpecification.{ordinal}.Tag.1.Value"),
                value.clone(),
            ));
        }
        pairs
    }

    /// Renders the request as an EC2 query string.
    pub fn to_query_string(&self) -> String {
        self.to_query_pairs()
            .iter()
            .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&")
    }
}

/// Lifecycle states EC2 reports for a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwsSnapshotState {
    Pending,
    Completed,
    Error,
}

impl AwsSnapshotState {
    /// Parses the `state` field of a `CreateSnapshot`/`DescribeSnapshots` reply.
    pub fn parse(raw: &str) -> SnapshotResult<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "pending" => Ok(Self::Pending),
            "completed" => Ok(Self::Completed),
            "error" => Ok(Self::Error),
            _ => Err(SnapshotError::Provider {
                provider: VolumeProvider::AwsEbs.as_str().to_string(),
                message: format!("unrecognised snapshot state {normalized:?}"),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Completed => "completed",
            Self::Error => "error",
        }
    }
}

/// Subset of the EC2 `CreateSnapshot` reply the controller needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsEbsSnapshotResponse {
    pub snapshot_id: String,
    pub state: AwsSnapshotState,
    pub volume_id: String,
    pub volume_size_gib: u64,
}

impl AwsEbsSnapshotResponse {
    /// Maps the EC2 reply onto the provider-neutral handle.
    pub fn into_handle(self) -> SnapshotResult<SnapshotHandle> {
        if self.state == AwsSnapshotState::Error {
            return Err(SnapshotError::Provider {
                provider: VolumeProvider::AwsEbs.as_str().to_string(),
                message: format!("snapshot {} entered the error state", self.snapshot_id),
            });
        }
        Ok(SnapshotHandle {
            snapshot_id: self.snapshot_id,
            provider: VolumeProvider::AwsEbs,
            source_volume: self.volume_id,
            size_gib: self.volume_size_gib,
            ready: self.state == AwsSnapshotState::Completed,
        })
    }
}

/// AWS EC2 `CreateVolume` request targeting an existing snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsEbsCreateVolumeRequest {
    pub region: String,
    pub availability_zone: String,
    pub snapshot_id: String,
    pub volume_type: String,
    pub size_gib: u64,
    pub tags: Vec<(String, String)>,
}

impl AwsEbsCreateVolumeRequest {
    /// `gp3` gives the clones the same baseline throughput as the primaries.
    pub const VOLUME_TYPE: &'static str = "gp3";

    /// Maps a provider-neutral clone request onto the EC2 payload.
    pub fn from_clone_request(request: &CloneRequest) -> SnapshotResult<Self> {
        if request.provider != VolumeProvider::AwsEbs {
            return Err(SnapshotError::ProviderMismatch {
                expected: VolumeProvider::AwsEbs.as_str().to_string(),
                actual: request.provider.as_str().to_string(),
            });
        }
        Ok(Self {
            region: request.region.clone(),
            availability_zone: request.zone.clone(),
            snapshot_id: request.snapshot_id.clone(),
            volume_type: Self::VOLUME_TYPE.to_string(),
            size_gib: request.size_gib,
            tags: vec![("Name".to_string(), request.target_volume_name.clone())],
        })
    }

    /// Query-string key/value pairs for `?Action=CreateVolume`.
    pub fn to_query_pairs(&self) -> Vec<(String, String)> {
        let mut pairs = vec![
            ("Action".to_string(), "CreateVolume".to_string()),
            (
                "Version".to_string(),
                AwsEbsCreateSnapshotRequest::API_VERSION.to_string(),
            ),
            (
                "AvailabilityZone".to_string(),
                self.availability_zone.clone(),
            ),
            ("SnapshotId".to_string(), self.snapshot_id.clone()),
            ("VolumeType".to_string(), self.volume_type.clone()),
            ("Size".to_string(), self.size_gib.to_string()),
        ];
        for (index, (key, value)) in self.tags.iter().enumerate() {
            let ordinal = index + 1;
            pairs.push((
                format!("TagSpecification.{ordinal}.ResourceType"),
                "volume".to_string(),
            ));
            pairs.push((format!("TagSpecification.{ordinal}.Tag.1.Key"), key.clone()));
            pairs.push((
                format!("TagSpecification.{ordinal}.Tag.1.Value"),
                value.clone(),
            ));
        }
        pairs
    }

    /// Renders the request as an EC2 query string.
    pub fn to_query_string(&self) -> String {
        self.to_query_pairs()
            .iter()
            .map(|(key, value)| format!("{}={}", percent_encode(key), percent_encode(value)))
            .collect::<Vec<_>>()
            .join("&")
    }
}

/// Lifecycle states EC2 reports for a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AwsVolumeState {
    Creating,
    Available,
    Error,
}

impl AwsVolumeState {
    pub fn parse(raw: &str) -> SnapshotResult<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "creating" => Ok(Self::Creating),
            "available" | "in-use" => Ok(Self::Available),
            "error" => Ok(Self::Error),
            _ => Err(SnapshotError::Provider {
                provider: VolumeProvider::AwsEbs.as_str().to_string(),
                message: format!("unrecognised volume state {normalized:?}"),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Available => "available",
            Self::Error => "error",
        }
    }
}

/// Subset of the EC2 `CreateVolume` reply the controller needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AwsEbsVolumeResponse {
    pub volume_id: String,
    pub state: AwsVolumeState,
    pub size_gib: u64,
    pub snapshot_id: String,
}

impl AwsEbsVolumeResponse {
    /// Maps the EC2 reply onto the provider-neutral handle.
    pub fn into_handle(self) -> SnapshotResult<VolumeHandle> {
        if self.state == AwsVolumeState::Error {
            return Err(SnapshotError::Provider {
                provider: VolumeProvider::AwsEbs.as_str().to_string(),
                message: format!("volume {} entered the error state", self.volume_id),
            });
        }
        Ok(VolumeHandle {
            volume_id: self.volume_id,
            provider: VolumeProvider::AwsEbs,
            source_snapshot_id: self.snapshot_id,
            size_gib: self.size_gib,
            ready: self.state == AwsVolumeState::Available,
        })
    }
}

/// GCP `compute.disks.createSnapshot` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcpPdCreateSnapshotRequest {
    pub project: String,
    pub zone: String,
    pub disk: String,
    pub snapshot_name: String,
    pub description: String,
    pub labels: Vec<(String, String)>,
}

impl GcpPdCreateSnapshotRequest {
    /// Maps a provider-neutral request onto the compute payload.
    pub fn from_snapshot_request(request: &SnapshotRequest) -> SnapshotResult<Self> {
        if request.provider != VolumeProvider::GcpPd {
            return Err(SnapshotError::ProviderMismatch {
                expected: VolumeProvider::GcpPd.as_str().to_string(),
                actual: request.provider.as_str().to_string(),
            });
        }
        Ok(Self {
            project: request.project.clone(),
            zone: request.zone.clone(),
            disk: request.source_volume.clone(),
            snapshot_name: request.snapshot_name.clone(),
            description: format!("crash-consistent snapshot for {}", request.snapshot_name),
            labels: request.labels.clone(),
        })
    }

    /// Relative resource path for the REST call.
    pub fn resource_path(&self) -> String {
        format!(
            "projects/{}/zones/{}/disks/{}/createSnapshot",
            self.project, self.zone, self.disk
        )
    }

    /// JSON body for the REST call.
    pub fn to_json_body(&self) -> String {
        format!(
            "{{\"name\":{},\"description\":{},\"labels\":{}}}",
            render_json_string(&self.snapshot_name),
            render_json_string(&self.description),
            render_json_label_map(&self.labels)
        )
    }
}

/// Lifecycle states GCP reports for a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcpSnapshotStatus {
    Creating,
    Ready,
    Failed,
}

impl GcpSnapshotStatus {
    pub fn parse(raw: &str) -> SnapshotResult<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "creating" | "uploading" => Ok(Self::Creating),
            "ready" => Ok(Self::Ready),
            "failed" | "deleting" => Ok(Self::Failed),
            _ => Err(SnapshotError::Provider {
                provider: VolumeProvider::GcpPd.as_str().to_string(),
                message: format!("unrecognised snapshot status {normalized:?}"),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

/// Subset of the GCP snapshot resource the controller needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcpPdSnapshotResponse {
    pub name: String,
    pub status: GcpSnapshotStatus,
    pub disk_size_gb: u64,
    pub source_disk: String,
}

impl GcpPdSnapshotResponse {
    /// Maps the GCP snapshot resource onto the provider-neutral handle.
    pub fn into_handle(self) -> SnapshotResult<SnapshotHandle> {
        if self.status == GcpSnapshotStatus::Failed {
            return Err(SnapshotError::Provider {
                provider: VolumeProvider::GcpPd.as_str().to_string(),
                message: format!("snapshot {} entered the failed state", self.name),
            });
        }
        Ok(SnapshotHandle {
            snapshot_id: self.name,
            provider: VolumeProvider::GcpPd,
            source_volume: self.source_disk,
            size_gib: self.disk_size_gb,
            ready: self.status == GcpSnapshotStatus::Ready,
        })
    }
}

/// GCP `compute.disks.insert` request cloning a snapshot into a new disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcpPdCreateDiskRequest {
    pub project: String,
    pub zone: String,
    pub disk_name: String,
    pub source_snapshot: String,
    pub size_gb: u64,
    pub disk_type: String,
}

impl GcpPdCreateDiskRequest {
    /// `pd-balanced` matches the CSI default for the generated claim.
    pub const DISK_TYPE: &'static str = "pd-balanced";

    /// Maps a provider-neutral clone request onto the compute payload.
    pub fn from_clone_request(request: &CloneRequest) -> SnapshotResult<Self> {
        if request.provider != VolumeProvider::GcpPd {
            return Err(SnapshotError::ProviderMismatch {
                expected: VolumeProvider::GcpPd.as_str().to_string(),
                actual: request.provider.as_str().to_string(),
            });
        }
        Ok(Self {
            project: request.project.clone(),
            zone: request.zone.clone(),
            disk_name: request.target_volume_name.clone(),
            source_snapshot: request.snapshot_id.clone(),
            size_gb: request.size_gib,
            disk_type: Self::DISK_TYPE.to_string(),
        })
    }

    /// Relative resource path for the REST call.
    pub fn resource_path(&self) -> String {
        format!("projects/{}/zones/{}/disks", self.project, self.zone)
    }

    /// Self-link the snapshot must be referenced by when cloning.
    pub fn source_snapshot_self_link(&self) -> String {
        format!(
            "projects/{}/global/snapshots/{}",
            self.project, self.source_snapshot
        )
    }

    /// Self-link of the disk type the clone is created with.
    pub fn disk_type_self_link(&self) -> String {
        format!(
            "projects/{}/zones/{}/diskTypes/{}",
            self.project, self.zone, self.disk_type
        )
    }

    /// JSON body for the REST call.
    pub fn to_json_body(&self) -> String {
        format!(
            "{{\"name\":{},\"sizeGb\":{},\"type\":{},\"sourceSnapshot\":{}}}",
            render_json_string(&self.disk_name),
            render_json_string(&self.size_gb.to_string()),
            render_json_string(&self.disk_type_self_link()),
            render_json_string(&self.source_snapshot_self_link())
        )
    }
}

/// Lifecycle states GCP reports for a disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GcpDiskStatus {
    Creating,
    Ready,
    Failed,
}

impl GcpDiskStatus {
    pub fn parse(raw: &str) -> SnapshotResult<Self> {
        let normalized = raw.trim().to_ascii_lowercase();
        match normalized.as_str() {
            "creating" | "restoring" => Ok(Self::Creating),
            "ready" => Ok(Self::Ready),
            "failed" => Ok(Self::Failed),
            _ => Err(SnapshotError::Provider {
                provider: VolumeProvider::GcpPd.as_str().to_string(),
                message: format!("unrecognised disk status {normalized:?}"),
            }),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Ready => "ready",
            Self::Failed => "failed",
        }
    }
}

/// Subset of the GCP disk resource the controller needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GcpPdDiskResponse {
    pub name: String,
    pub status: GcpDiskStatus,
    pub size_gb: u64,
    pub source_snapshot: String,
}

impl GcpPdDiskResponse {
    /// Maps the GCP disk resource onto the provider-neutral handle.
    pub fn into_handle(self) -> SnapshotResult<VolumeHandle> {
        if self.status == GcpDiskStatus::Failed {
            return Err(SnapshotError::Provider {
                provider: VolumeProvider::GcpPd.as_str().to_string(),
                message: format!("disk {} entered the failed state", self.name),
            });
        }
        Ok(VolumeHandle {
            volume_id: self.name,
            provider: VolumeProvider::GcpPd,
            source_snapshot_id: self.source_snapshot,
            size_gib: self.size_gb,
            ready: self.status == GcpDiskStatus::Ready,
        })
    }
}

/// Renders `value` as a JSON string literal, escaping the characters that would
/// otherwise break the hand-built provider payloads.
pub fn render_json_string(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len() + 2);
    rendered.push('"');
    for character in value.chars() {
        match character {
            '"' => rendered.push_str("\\\""),
            '\\' => rendered.push_str("\\\\"),
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            '\t' => rendered.push_str("\\t"),
            other if (other as u32) < 0x20 => {
                rendered.push_str(&format!("\\u{:04x}", other as u32));
            }
            other => rendered.push(other),
        }
    }
    rendered.push('"');
    rendered
}

/// Renders an ordered label list as a JSON object literal.
pub fn render_json_label_map(labels: &[(String, String)]) -> String {
    let mut rendered = String::from("{");
    for (index, (key, value)) in labels.iter().enumerate() {
        if index > 0 {
            rendered.push(',');
        }
        rendered.push_str(&render_json_string(key));
        rendered.push(':');
        rendered.push_str(&render_json_string(value));
    }
    rendered.push('}');
    rendered
}

/// Renders an ordered list of already-rendered fields as a JSON object.
///
/// Values are emitted verbatim, so callers pass [`render_json_string`] output
/// for strings and nested [`render_json_object`] output for sub-objects.
pub fn render_json_object(fields: &[(&str, String)]) -> String {
    let mut rendered = String::from("{");
    for (index, (key, value)) in fields.iter().enumerate() {
        if index > 0 {
            rendered.push(',');
        }
        rendered.push_str(&render_json_string(key));
        rendered.push(':');
        rendered.push_str(value);
    }
    rendered.push('}');
    rendered
}

/// Percent-encodes a component for use in an EC2 query string.
fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_request(provider: VolumeProvider) -> SnapshotRequest {
        SnapshotRequest {
            provider,
            source_volume: "vol-db-primary".to_string(),
            snapshot_name: "horizon-snapshot".to_string(),
            size_gib: 10,
            region: "us-east-1".to_string(),
            project: "stellar-prod".to_string(),
            zone: "us-central1-a".to_string(),
            labels: vec![("app".to_string(), "horizon".to_string())],
        }
    }

    fn clone_request(provider: VolumeProvider) -> CloneRequest {
        CloneRequest {
            provider,
            snapshot_id: "snap-0abc".to_string(),
            target_volume_name: "horizon-clone".to_string(),
            size_gib: 10,
            region: "us-east-1".to_string(),
            project: "stellar-prod".to_string(),
            zone: "us-central1-a".to_string(),
        }
    }

    #[test]
    fn parses_known_providers_ignoring_case_and_padding() {
        assert_eq!(
            VolumeProvider::parse("aws-ebs").unwrap(),
            VolumeProvider::AwsEbs
        );
        assert_eq!(
            VolumeProvider::parse(" EBS ").unwrap(),
            VolumeProvider::AwsEbs
        );
        assert_eq!(
            VolumeProvider::parse("GCP-PD").unwrap(),
            VolumeProvider::GcpPd
        );
        assert_eq!(VolumeProvider::parse("pd").unwrap(), VolumeProvider::GcpPd);
        assert_eq!(VolumeProvider::AwsEbs.as_str(), "aws-ebs");
        assert_eq!(
            VolumeProvider::GcpPd.default_storage_class(),
            GCP_PD_STORAGE_CLASS
        );
    }

    #[test]
    fn rejects_unknown_and_empty_providers() {
        for raw in ["azure-disk", "", "aws_ebs"] {
            let error = VolumeProvider::parse(raw).unwrap_err();
            assert_eq!(
                error,
                SnapshotError::UnsupportedProvider {
                    provider: raw.trim().to_string()
                }
            );
            assert!(error.to_string().contains("unsupported volume provider"));
        }
    }

    #[test]
    fn aws_snapshot_request_renders_ec2_query_pairs() {
        let request = AwsEbsCreateSnapshotRequest::from_snapshot_request(&snapshot_request(
            VolumeProvider::AwsEbs,
        ))
        .unwrap();

        assert_eq!(request.region, "us-east-1");
        assert_eq!(request.volume_id, "vol-db-primary");

        let pairs = request.to_query_pairs();
        assert_eq!(
            pairs[0],
            ("Action".to_string(), "CreateSnapshot".to_string())
        );
        assert_eq!(
            pairs[1],
            (
                "Version".to_string(),
                AwsEbsCreateSnapshotRequest::API_VERSION.to_string()
            )
        );
        assert!(pairs.contains(&("VolumeId".to_string(), "vol-db-primary".to_string())));
        assert!(pairs.contains(&(
            "TagSpecification.1.ResourceType".to_string(),
            "snapshot".to_string()
        )));
        assert!(pairs.contains(&(
            "TagSpecification.1.Tag.1.Key".to_string(),
            "app".to_string()
        )));
        assert!(pairs.contains(&(
            "TagSpecification.1.Tag.1.Value".to_string(),
            "horizon".to_string()
        )));
        assert!(request
            .to_query_string()
            .starts_with("Action=CreateSnapshot&Version="));
    }

    #[test]
    fn aws_snapshot_response_maps_states_onto_the_handle() {
        let pending = AwsEbsSnapshotResponse {
            snapshot_id: "snap-1".to_string(),
            state: AwsSnapshotState::parse("pending").unwrap(),
            volume_id: "vol-db-primary".to_string(),
            volume_size_gib: 10,
        }
        .into_handle()
        .unwrap();
        assert!(!pending.ready);
        assert_eq!(pending.provider, VolumeProvider::AwsEbs);
        assert_eq!(pending.source_volume, "vol-db-primary");

        let completed = AwsEbsSnapshotResponse {
            snapshot_id: "snap-1".to_string(),
            state: AwsSnapshotState::parse("COMPLETED").unwrap(),
            volume_id: "vol-db-primary".to_string(),
            volume_size_gib: 10,
        }
        .into_handle()
        .unwrap();
        assert!(completed.ready);

        let failed = AwsEbsSnapshotResponse {
            snapshot_id: "snap-1".to_string(),
            state: AwsSnapshotState::Error,
            volume_id: "vol-db-primary".to_string(),
            volume_size_gib: 10,
        }
        .into_handle()
        .unwrap_err();
        assert!(matches!(failed, SnapshotError::Provider { .. }));

        assert!(matches!(
            AwsSnapshotState::parse("garbage"),
            Err(SnapshotError::Provider { .. })
        ));
    }

    #[test]
    fn aws_clone_request_targets_the_snapshot_and_zone() {
        let request =
            AwsEbsCreateVolumeRequest::from_clone_request(&clone_request(VolumeProvider::AwsEbs))
                .unwrap();

        assert_eq!(request.availability_zone, "us-central1-a");
        assert_eq!(request.volume_type, AwsEbsCreateVolumeRequest::VOLUME_TYPE);

        let pairs = request.to_query_pairs();
        assert!(pairs.contains(&("Action".to_string(), "CreateVolume".to_string())));
        assert!(pairs.contains(&("SnapshotId".to_string(), "snap-0abc".to_string())));
        assert!(pairs.contains(&("Size".to_string(), "10".to_string())));
        assert!(pairs.contains(&(
            "TagSpecification.1.Tag.1.Value".to_string(),
            "horizon-clone".to_string()
        )));
    }

    #[test]
    fn aws_volume_response_maps_states_onto_the_handle() {
        let available = AwsEbsVolumeResponse {
            volume_id: "vol-clone-1".to_string(),
            state: AwsVolumeState::parse("in-use").unwrap(),
            size_gib: 10,
            snapshot_id: "snap-0abc".to_string(),
        }
        .into_handle()
        .unwrap();
        assert!(available.ready);
        assert_eq!(available.source_snapshot_id, "snap-0abc");

        let creating = AwsEbsVolumeResponse {
            volume_id: "vol-clone-1".to_string(),
            state: AwsVolumeState::Creating,
            size_gib: 10,
            snapshot_id: "snap-0abc".to_string(),
        }
        .into_handle()
        .unwrap();
        assert!(!creating.ready);

        assert!(matches!(
            AwsVolumeState::parse("who knows"),
            Err(SnapshotError::Provider { .. })
        ));
    }

    #[test]
    fn gcp_snapshot_request_renders_path_and_body() {
        let request = GcpPdCreateSnapshotRequest::from_snapshot_request(&snapshot_request(
            VolumeProvider::GcpPd,
        ))
        .unwrap();

        assert_eq!(
            request.resource_path(),
            "projects/stellar-prod/zones/us-central1-a/disks/vol-db-primary/createSnapshot"
        );
        assert_eq!(
            request.to_json_body(),
            "{\"name\":\"horizon-snapshot\",\"description\":\"crash-consistent snapshot for \
             horizon-snapshot\",\"labels\":{\"app\":\"horizon\"}}"
        );
    }

    #[test]
    fn gcp_disk_body_references_the_snapshot_self_link() {
        let request =
            GcpPdCreateDiskRequest::from_clone_request(&clone_request(VolumeProvider::GcpPd))
                .unwrap();

        assert_eq!(
            request.resource_path(),
            "projects/stellar-prod/zones/us-central1-a/disks"
        );
        assert_eq!(
            request.source_snapshot_self_link(),
            "projects/stellar-prod/global/snapshots/snap-0abc"
        );
        assert_eq!(
            request.disk_type_self_link(),
            "projects/stellar-prod/zones/us-central1-a/diskTypes/pd-balanced"
        );
        assert_eq!(
            request.to_json_body(),
            "{\"name\":\"horizon-clone\",\"sizeGb\":\"10\",\"type\":\"projects/stellar-prod/zones/\
             us-central1-a/diskTypes/pd-balanced\",\"sourceSnapshot\":\"projects/stellar-prod/global/\
             snapshots/snap-0abc\"}"
        );
    }

    #[test]
    fn gcp_responses_map_statuses_onto_handles() {
        let ready = GcpPdSnapshotResponse {
            name: "horizon-snapshot".to_string(),
            status: GcpSnapshotStatus::parse("READY").unwrap(),
            disk_size_gb: 10,
            source_disk: "vol-db-primary".to_string(),
        }
        .into_handle()
        .unwrap();
        assert!(ready.ready);
        assert_eq!(ready.provider, VolumeProvider::GcpPd);

        let creating = GcpPdSnapshotResponse {
            name: "horizon-snapshot".to_string(),
            status: GcpSnapshotStatus::parse("uploading").unwrap(),
            disk_size_gb: 10,
            source_disk: "vol-db-primary".to_string(),
        }
        .into_handle()
        .unwrap();
        assert!(!creating.ready);

        let failed = GcpPdSnapshotResponse {
            name: "horizon-snapshot".to_string(),
            status: GcpSnapshotStatus::Failed,
            disk_size_gb: 10,
            source_disk: "vol-db-primary".to_string(),
        }
        .into_handle()
        .unwrap_err();
        assert!(matches!(failed, SnapshotError::Provider { .. }));

        let disk = GcpPdDiskResponse {
            name: "horizon-clone".to_string(),
            status: GcpDiskStatus::parse("ready").unwrap(),
            size_gb: 10,
            source_snapshot: "horizon-snapshot".to_string(),
        }
        .into_handle()
        .unwrap();
        assert!(disk.ready);
        assert_eq!(disk.provider, VolumeProvider::GcpPd);

        assert!(matches!(
            GcpDiskStatus::parse("nope"),
            Err(SnapshotError::Provider { .. })
        ));
        assert!(matches!(
            GcpSnapshotStatus::parse("nope"),
            Err(SnapshotError::Provider { .. })
        ));
    }

    #[test]
    fn cross_provider_payloads_are_rejected() {
        let aws_on_gcp = GcpPdCreateSnapshotRequest::from_snapshot_request(&snapshot_request(
            VolumeProvider::AwsEbs,
        ))
        .unwrap_err();
        assert_eq!(
            aws_on_gcp,
            SnapshotError::ProviderMismatch {
                expected: "gcp-pd".to_string(),
                actual: "aws-ebs".to_string(),
            }
        );

        let gcp_on_aws = AwsEbsCreateSnapshotRequest::from_snapshot_request(&snapshot_request(
            VolumeProvider::GcpPd,
        ))
        .unwrap_err();
        assert_eq!(
            gcp_on_aws,
            SnapshotError::ProviderMismatch {
                expected: "aws-ebs".to_string(),
                actual: "gcp-pd".to_string(),
            }
        );

        assert!(
            AwsEbsCreateVolumeRequest::from_clone_request(&clone_request(VolumeProvider::GcpPd))
                .is_err()
        );
        assert!(
            GcpPdCreateDiskRequest::from_clone_request(&clone_request(VolumeProvider::AwsEbs))
                .is_err()
        );
    }

    #[test]
    fn json_helpers_escape_reserved_characters() {
        assert_eq!(render_json_string("plain"), "\"plain\"");
        assert_eq!(render_json_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(render_json_string("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(
            render_json_label_map(&[
                ("app".to_string(), "ho\"rizon".to_string()),
                ("tier".to_string(), "core".to_string()),
            ]),
            "{\"app\":\"ho\\\"rizon\",\"tier\":\"core\"}"
        );
        assert_eq!(render_json_label_map(&[]), "{}");

        assert_eq!(
            render_json_object(&[
                ("apiVersion", render_json_string("v1")),
                (
                    "metadata",
                    render_json_object(&[("name", render_json_string("horizon"))])
                ),
            ]),
            "{\"apiVersion\":\"v1\",\"metadata\":{\"name\":\"horizon\"}}"
        );
        assert_eq!(render_json_object(&[]), "{}");
    }

    #[test]
    fn percent_encoding_escapes_only_reserved_bytes() {
        assert_eq!(percent_encode("stellar core"), "stellar%20core");
        assert_eq!(percent_encode("a-b_c.d~e"), "a-b_c.d~e");
        assert_eq!(percent_encode("us-east-1"), "us-east-1");
    }

    #[test]
    fn write_coordinator_fsync_defaults_to_a_noop() {
        struct PauseOnly;

        impl WriteCoordinator for PauseOnly {
            fn pause_writes(&mut self, _volume_id: &str) -> SnapshotResult<()> {
                Ok(())
            }

            fn resume_writes(&mut self, _volume_id: &str) -> SnapshotResult<()> {
                Ok(())
            }
        }

        let mut coordinator = PauseOnly;
        assert_eq!(coordinator.fsync("vol-db-primary"), Ok(()));
    }
}
