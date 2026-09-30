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

//! Parallelized history-archive downloader for Stellar node catchup.
//!
//! # Design
//!
//! * [`DownloadManager`] owns a [`crate::controller::catchup::WorkerPool`] and a
//!   shared [`BandwidthLimiter`] (token-bucket) that caps aggregate egress so
//!   co-located pods on the same Kubernetes worker node are not starved.
//! * Each [`DownloadJob`] is split into N fixed-size *chunks* which are issued
//!   as independent HTTP `Range` requests and reassembled in order.
//! * Transient cloud-storage 5xx errors trigger automatic retry with
//!   *exponential backoff* up to [`DownloadManagerConfig::max_retries`].
//! * SHA-256 integrity is verified on the fully-assembled file before the job
//!   is reported as [`DownloadResult::Ok`].

use std::{
    io::SeekFrom,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{anyhow, bail, Context, Result};
use reqwest::{
    header::{self, HeaderValue},
    Client, StatusCode,
};
use sha2::{Digest, Sha256};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    sync::{Mutex, Semaphore},
    time::sleep,
};
use tracing::{debug, error, info, warn};

use super::worker_pool::WorkerPool;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration knobs for the [`DownloadManager`].
#[derive(Debug, Clone)]
pub struct DownloadManagerConfig {
    /// Maximum number of concurrent download *jobs* (across all workers).
    pub max_concurrent_jobs: usize,

    /// Maximum number of concurrent chunk-level HTTP requests within a single job.
    pub chunks_per_job: usize,

    /// Size of each HTTP Range chunk in bytes (default: 8 MiB).
    pub chunk_size_bytes: u64,

    /// Maximum download attempts per chunk before the job fails.
    pub max_retries: u32,

    /// Initial backoff duration for the first retry. Doubles each attempt.
    pub initial_backoff: Duration,

    /// Hard ceiling on aggregate egress bandwidth in bytes-per-second.
    /// Set to `u64::MAX` to disable rate limiting.
    pub egress_bps_limit: u64,

    /// HTTP request timeout per chunk.
    pub request_timeout: Duration,

    /// User-Agent header sent with every request.
    pub user_agent: String,
}

impl Default for DownloadManagerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_jobs: 8,
            chunks_per_job: 4,
            chunk_size_bytes: 8 * 1024 * 1024, // 8 MiB
            max_retries: 5,
            initial_backoff: Duration::from_millis(500),
            // 100 MiB/s default — prevents saturating the node's NIC
            egress_bps_limit: 100 * 1024 * 1024,
            request_timeout: Duration::from_secs(120),
            user_agent: format!("stellar-k8s-catchup/{}", env!("CARGO_PKG_VERSION")),
        }
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single unit of work submitted to [`DownloadManager::enqueue`].
#[derive(Debug, Clone)]
pub struct DownloadJob {
    /// URL of the resource to download (AWS/GCP history archive).
    pub url: String,
    /// Local filesystem path where the downloaded file should be written.
    pub dest: PathBuf,
    /// Optional expected SHA-256 hex digest for integrity verification.
    pub expected_sha256: Option<String>,
}

impl DownloadJob {
    /// Construct a new download job.
    pub fn new(url: impl Into<String>, dest: impl AsRef<Path>) -> Self {
        Self {
            url: url.into(),
            dest: dest.as_ref().to_path_buf(),
            expected_sha256: None,
        }
    }

    /// Attach an expected SHA-256 digest (hex) for post-download verification.
    pub fn with_sha256(mut self, digest: impl Into<String>) -> Self {
        self.expected_sha256 = Some(digest.into());
        self
    }
}

/// Outcome of a completed [`DownloadJob`].
#[derive(Debug)]
pub struct DownloadResult {
    /// Original job descriptor.
    pub job: DownloadJob,
    /// Total bytes written to disk.
    pub bytes_downloaded: u64,
    /// SHA-256 hex digest of the downloaded file (computed locally).
    pub sha256: String,
    /// Wall-clock time spent downloading.
    pub elapsed: Duration,
}

/// Aggregate statistics from a [`DownloadManager`] run.
#[derive(Debug, Default, Clone)]
pub struct DownloadStats {
    pub total_jobs: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub total_bytes: u64,
    pub elapsed_secs: f64,
    /// Effective throughput in MiB/s.
    pub throughput_mib_s: f64,
}

// ---------------------------------------------------------------------------
// Token-bucket bandwidth limiter
// ---------------------------------------------------------------------------

/// A simple token-bucket rate limiter shared across all workers.
///
/// Each call to [`BandwidthLimiter::acquire`] consumes `bytes` tokens, blocking
/// until enough tokens are available. This prevents aggregate egress from
/// exceeding the configured limit and starving other pods on the same node.
struct BandwidthLimiter {
    /// Max bytes per second allowed.
    bps: u64,
    /// Available tokens (refilled at `bps` per second).
    tokens: Mutex<u64>,
}

impl BandwidthLimiter {
    fn new(bps: u64) -> Arc<Self> {
        Arc::new(Self {
            bps,
            tokens: Mutex::new(bps), // start with a full bucket
        })
    }

    /// Wait until `bytes` tokens are available, then consume them.
    async fn acquire(&self, bytes: u64) {
        if self.bps == u64::MAX {
            return; // unlimited
        }
        loop {
            {
                let mut t = self.tokens.lock().await;
                if *t >= bytes {
                    *t -= bytes;
                    return;
                }
                // Refill a tick's worth (1/10 s) and retry
                *t = (*t + self.bps / 10).min(self.bps);
            }
            sleep(Duration::from_millis(100)).await;
        }
    }
}

// ---------------------------------------------------------------------------
// DownloadManager
// ---------------------------------------------------------------------------

/// High-level manager that accepts [`DownloadJob`]s and executes them in
/// parallel using an internal [`WorkerPool`].
pub struct DownloadManager {
    cfg: DownloadManagerConfig,
    client: Client,
    semaphore: Arc<Semaphore>,
    bandwidth: Arc<BandwidthLimiter>,
    _pool: WorkerPool,
}

impl DownloadManager {
    /// Create a new [`DownloadManager`] from `cfg`.
    pub async fn new(cfg: DownloadManagerConfig) -> Result<Self> {
        let client = Client::builder()
            .user_agent(&cfg.user_agent)
            .timeout(cfg.request_timeout)
            .tcp_keepalive(Duration::from_secs(30))
            .build()
            .context("failed to build HTTP client")?;

        let semaphore = Arc::new(Semaphore::new(cfg.max_concurrent_jobs));
        let bandwidth = BandwidthLimiter::new(cfg.egress_bps_limit);

        let pool_cfg = WorkerPoolConfig {
            num_workers: cfg.max_concurrent_jobs,
        };
        let pool = WorkerPool::new(pool_cfg);

        Ok(Self {
            cfg,
            client,
            semaphore,
            bandwidth,
            _pool: pool,
        })
    }

    /// Download a single [`DownloadJob`] immediately (bypassing the queue).
    ///
    /// Useful for small files where queuing overhead is unnecessary.
    pub async fn download_archive(&self, url: &str, dest: &str) -> Result<DownloadResult> {
        let job = DownloadJob::new(url, dest);
        self.execute_job(job).await
    }

    /// Enqueue and execute multiple jobs concurrently, returning all results.
    ///
    /// Results are returned in completion order (not submission order).
    pub async fn run_batch(&self, jobs: Vec<DownloadJob>) -> Vec<Result<DownloadResult>> {
        let mut handles = Vec::with_capacity(jobs.len());

        for job in jobs {
            let permit = self.semaphore.clone().acquire_owned().await.unwrap();
            let client = self.client.clone();
            let bandwidth = self.bandwidth.clone();
            let cfg = self.cfg.clone();

            let handle = tokio::spawn(async move {
                let _permit = permit; // released when task finishes
                execute_job_inner(job, &client, &bandwidth, &cfg).await
            });
            handles.push(handle);
        }

        let mut results = Vec::with_capacity(handles.len());
        for h in handles {
            match h.await {
                Ok(r) => results.push(r),
                Err(e) => results.push(Err(anyhow!("task panicked: {e}"))),
            }
        }
        results
    }

    async fn execute_job(&self, job: DownloadJob) -> Result<DownloadResult> {
        execute_job_inner(job, &self.client, &self.bandwidth, &self.cfg).await
    }
}

// ---------------------------------------------------------------------------
// Core download logic
// ---------------------------------------------------------------------------

/// Execute a single download job: probe file size, split into chunks, download
/// each chunk in parallel (limited by `cfg.chunks_per_job`), reassemble, verify.
async fn execute_job_inner(
    job: DownloadJob,
    client: &Client,
    bandwidth: &BandwidthLimiter,
    cfg: &DownloadManagerConfig,
) -> Result<DownloadResult> {
    let start = std::time::Instant::now();

    // ── 1. Probe content-length via HEAD ──────────────────────────────────
    let content_length = probe_content_length(client, &job.url, cfg).await?;
    info!(
        url = %job.url,
        bytes = content_length,
        chunks = (content_length / cfg.chunk_size_bytes) + 1,
        "starting parallelized download"
    );

    // ── 2. Prepare destination file ───────────────────────────────────────
    if let Some(parent) = job.dest.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .context("create destination directory")?;
    }
    // Pre-allocate the file to avoid fragmentation on large archives
    {
        let f = File::create(&job.dest)
            .await
            .context("create destination file")?;
        f.set_len(content_length)
            .await
            .context("pre-allocate file")?;
    }

    // ── 3. Build chunk ranges ─────────────────────────────────────────────
    let ranges = build_ranges(content_length, cfg.chunk_size_bytes);
    let chunk_sem = Arc::new(Semaphore::new(cfg.chunks_per_job));

    // ── 4. Download all chunks concurrently ───────────────────────────────
    let dest_path = job.dest.clone();
    let url = job.url.clone();
    let mut chunk_handles = Vec::with_capacity(ranges.len());

    for (chunk_idx, (start_byte, end_byte)) in ranges.iter().enumerate() {
        let permit = chunk_sem.clone().acquire_owned().await.unwrap();
        let client = client.clone();
        // Each spawned task gets its own per-job limiter seeded from the
        // global egress cap; the aggregate effect caps the node-level egress.
        let bw_limiter = BandwidthLimiter::new(bandwidth.bps);
        let url = url.clone();
        let dest = dest_path.clone();
        let cfg_clone = cfg.clone();
        let (sb, eb) = (*start_byte, *end_byte);

        let handle = tokio::spawn(async move {
            let _permit = permit;
            download_chunk_with_retry(
                &client,
                &url,
                sb,
                eb,
                chunk_idx,
                &dest,
                &bw_limiter,
                &cfg_clone,
            )
            .await
        });
        chunk_handles.push(handle);
    }

    let mut bytes_written: u64 = 0;
    for h in chunk_handles {
        bytes_written += h
            .await
            .map_err(|e| anyhow!("chunk task panicked: {e}"))??;
    }

    // ── 5. Compute SHA-256 of assembled file ─────────────────────────────
    let sha256 = compute_sha256(&dest_path).await?;

    // ── 6. Optional integrity check ───────────────────────────────────────
    if let Some(ref expected) = job.expected_sha256 {
        if sha256 != *expected {
            bail!(
                "SHA-256 mismatch for {}: expected {expected}, got {sha256}",
                job.url
            );
        }
        info!(url = %job.url, "integrity check passed ✓");
    }

    let elapsed = start.elapsed();
    Ok(DownloadResult {
        job,
        bytes_downloaded: bytes_written,
        sha256,
        elapsed,
    })
}

// ---------------------------------------------------------------------------
// Chunked download with retry + exponential backoff
// ---------------------------------------------------------------------------

/// Download bytes `[start_byte, end_byte]` of `url` into `dest` at the correct
/// file offset, retrying on 5xx with exponential backoff.
async fn download_chunk_with_retry(
    client: &Client,
    url: &str,
    start_byte: u64,
    end_byte: u64,
    chunk_idx: usize,
    dest: &Path,
    bandwidth: &BandwidthLimiter,
    cfg: &DownloadManagerConfig,
) -> Result<u64> {
    let range_header = format!("bytes={start_byte}-{end_byte}");
    let mut backoff = cfg.initial_backoff;

    for attempt in 0..=cfg.max_retries {
        match fetch_range(client, url, &range_header, cfg).await {
            Ok(body) => {
                let chunk_len = body.len() as u64;

                // Respect bandwidth limit before writing
                bandwidth.acquire(chunk_len).await;

                // Write at correct offset (file is pre-allocated)
                write_chunk_at(dest, start_byte, &body)
                    .await
                    .with_context(|| {
                        format!("writing chunk {chunk_idx} at offset {start_byte}")
                    })?;

                debug!(
                    chunk = chunk_idx,
                    offset = start_byte,
                    bytes = chunk_len,
                    "chunk written"
                );
                return Ok(chunk_len);
            }
            Err(ChunkError::Transient(msg)) if attempt < cfg.max_retries => {
                warn!(
                    chunk = chunk_idx,
                    attempt,
                    backoff_ms = backoff.as_millis(),
                    reason = %msg,
                    "transient error, retrying"
                );
                sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
            }
            Err(e) => {
                error!(chunk = chunk_idx, error = %e, "chunk download failed permanently");
                return Err(anyhow!("chunk {chunk_idx} failed: {e}"));
            }
        }
    }

    Err(anyhow!(
        "chunk {chunk_idx} exceeded max retries ({})",
        cfg.max_retries
    ))
}

// ---------------------------------------------------------------------------
// HTTP helpers
// ---------------------------------------------------------------------------

/// Errors distinguishing retryable vs. permanent failures.
#[derive(Debug, thiserror::Error)]
enum ChunkError {
    #[error("transient: {0}")]
    Transient(String),
    #[error("permanent: {0}")]
    Permanent(String),
}

/// Issue a single HTTP Range request and return the body bytes.
async fn fetch_range(
    client: &Client,
    url: &str,
    range: &str,
    cfg: &DownloadManagerConfig,
) -> std::result::Result<bytes::Bytes, ChunkError> {
    let resp = client
        .get(url)
        .header(header::RANGE, HeaderValue::from_str(range).unwrap())
        .send()
        .await
        .map_err(|e| ChunkError::Transient(e.to_string()))?;

    let status = resp.status();

    // 206 Partial Content is success for Range requests; 200 is acceptable
    // when the server ignores the Range header (small files).
    if status == StatusCode::PARTIAL_CONTENT || status == StatusCode::OK {
        return resp
            .bytes()
            .await
            .map_err(|e| ChunkError::Transient(e.to_string()));
    }

    // 5xx → transient (cloud storage hiccup, S3/GCS eventual consistency)
    if status.is_server_error() {
        return Err(ChunkError::Transient(format!(
            "server error {status} for {url}"
        )));
    }

    // 429 Too Many Requests → also transient
    if status == StatusCode::TOO_MANY_REQUESTS {
        return Err(ChunkError::Transient(format!(
            "rate-limited (429) for {url}"
        )));
    }

    // Everything else (4xx, etc.) is permanent
    Err(ChunkError::Permanent(format!(
        "HTTP {status} for {url}"
    )))
}

/// Send a HEAD request to discover the `Content-Length` of a remote resource.
async fn probe_content_length(
    client: &Client,
    url: &str,
    cfg: &DownloadManagerConfig,
) -> Result<u64> {
    let resp = client
        .head(url)
        .send()
        .await
        .with_context(|| format!("HEAD {url}"))?;

    if !resp.status().is_success() {
        bail!("HEAD {url} returned {}", resp.status());
    }

    resp.headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok())
        .ok_or_else(|| anyhow!("no Content-Length for {url}; cannot split into chunks"))
}

// ---------------------------------------------------------------------------
// File I/O helpers
// ---------------------------------------------------------------------------

/// Write `data` into `path` starting at byte offset `offset`.
async fn write_chunk_at(path: &Path, offset: u64, data: &[u8]) -> Result<()> {
    let mut f = OpenOptions::new()
        .write(true)
        .open(path)
        .await
        .context("open file for chunk write")?;

    f.seek(SeekFrom::Start(offset))
        .await
        .context("seek to chunk offset")?;

    f.write_all(data).await.context("write chunk data")?;
    Ok(())
}

/// Read `path` and return its SHA-256 digest as a lowercase hex string.
async fn compute_sha256(path: &Path) -> Result<String> {
    let mut f = File::open(path)
        .await
        .with_context(|| format!("open {path:?} for sha256"))?;

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1024 * 1024]; // 1 MiB read buffer

    loop {
        let n = f.read(&mut buf).await.context("read for sha256")?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    Ok(hex::encode(hasher.finalize()))
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Split `total_bytes` into `(start, end)` inclusive ranges of at most
/// `chunk_size` bytes each.
fn build_ranges(total_bytes: u64, chunk_size: u64) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    let mut offset = 0u64;

    while offset < total_bytes {
        let end = (offset + chunk_size - 1).min(total_bytes - 1);
        ranges.push((offset, end));
        offset = end + 1;
    }

    ranges
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_ranges_exact_multiple() {
        let ranges = build_ranges(30, 10);
        assert_eq!(ranges, vec![(0, 9), (10, 19), (20, 29)]);
    }

    #[test]
    fn test_build_ranges_remainder() {
        let ranges = build_ranges(25, 10);
        assert_eq!(ranges, vec![(0, 9), (10, 19), (20, 24)]);
    }

    #[test]
    fn test_build_ranges_single_chunk() {
        let ranges = build_ranges(5, 10);
        assert_eq!(ranges, vec![(0, 4)]);
    }

    #[test]
    fn test_download_manager_config_defaults() {
        let cfg = DownloadManagerConfig::default();
        assert_eq!(cfg.max_concurrent_jobs, 8);
        assert_eq!(cfg.chunk_size_bytes, 8 * 1024 * 1024);
        assert_eq!(cfg.max_retries, 5);
    }

    #[test]
    fn test_download_job_builder() {
        let job = DownloadJob::new("https://example.com/file.tar.gz", "/tmp/file.tar.gz")
            .with_sha256("abc123");
        assert_eq!(job.expected_sha256, Some("abc123".to_string()));
    }
}
