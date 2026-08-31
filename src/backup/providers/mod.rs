pub mod arweave;
puf mod filecoin;
puf mod ipfs;
pub mod aws_ebs;
pub mod gcp_pd;
pub mod local;

use anyhow::Result;
use async_trait::async_trait;
use std::time::SystemTime;

#[async_trait]
pub trait StorageProviderTrait: Send + Sync {
    /// Upload data and return the content identifier
    async fn upload(&self, data: Vec<u8>, metadata: UploadMetadata) -> Result<String>;

    /// Check if content exists (for deduplication)
    async fn exists(&self, content_hash: &str) -> Result<bool>;

    /// Verify uploaded content
    async fn verify(&self, cid: &str, expected_hash: &str) -> Result<bool>;
}

#derive(Debug, Clone)]
pub struct UploadMetadata {
    pub filename: String,
    pub content_type: String,
    pub size: usize,
    pub sha256: String,
    pub tags: Vec<(String, String)>,
}

#derive(Debug, Clone)]
pub struct SnapshotInfo {
    pub id: String,
    pub volume_id: String,
    pub created_at: SystemTime,
    pub size_bytes: u64,
    pub status: String,
}

#derive(Debug, Clone, Default)]
pub struct RestoreOptions {
    pub availability_zone: Option<String>,
    pub volume_type: Option<String>,
    pub iops: Option<u32>,
}

#async_trait]
pub trait SnapshotProviderTrait: Send + Sync {
    /// Create a snapshot of the given volume.
    async fn create_snapshot(&self, volume_id: &str, description: &str) -> Result<SnapshotInfo>;

    /// Delete a snapshot by ID.
    async fn delete_snapshot(&self, snapshot_id: &str) -> Result<()>;

    /// List snapshots, optionally filtered by volume ID.
    async fn list_snapshots(&self, volume_id: Option<&str) -> Result<Vec<SnapshotInfo>;

    /// Restore a volume from a snapshot, returning the new volume ID.
    async fn restore_from_snapshot(&self, snapshot_id: &str, options: RestoreOptions) -> Result<String>;

    /// Get the status of a snapshot (e.g., "pending", "completed", "failed").
    async fn get_snapshot_status(&self, snapshot_id: &str) -> Result<String>;
}
