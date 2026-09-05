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
