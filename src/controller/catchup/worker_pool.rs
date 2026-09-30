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

//! Async worker pool for the parallelized catchup downloader.
//!
//! [`WorkerPool`] manages a fixed set of Tokio tasks that drain a shared
//! work queue. It is intentionally generic so it can be reused by other
//! subsystems that need bounded parallelism.
//!
//! # Bandwidth Constraints
//!
//! The pool does **not** enforce bandwidth limits itself — that responsibility
//! belongs to [`super::downloader::BandwidthLimiter`]. The pool only controls
//! *concurrency* (how many jobs run simultaneously) via a [`tokio::sync::Semaphore`].

use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use anyhow::Result;
use tokio::sync::{mpsc, Semaphore};
use tracing::{debug, error, info};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for a [`WorkerPool`].
#[derive(Debug, Clone)]
pub struct WorkerPoolConfig {
    /// Number of parallel worker tasks to spawn.
    pub num_workers: usize,
}

impl Default for WorkerPoolConfig {
    fn default() -> Self {
        Self { num_workers: 8 }
    }
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------

/// Live statistics exposed by a running [`WorkerPool`].
#[derive(Debug, Default, Clone)]
pub struct WorkerPoolStats {
    /// Total jobs dispatched to the pool since creation.
    pub jobs_dispatched: u64,
    /// Jobs that completed successfully.
    pub jobs_succeeded: u64,
    /// Jobs that failed (all retries exhausted).
    pub jobs_failed: u64,
}

// ---------------------------------------------------------------------------
// Internal message types
// ---------------------------------------------------------------------------

type BoxedJob = Box<dyn FnOnce() -> futures::future::BoxFuture<'static, Result<()>> + Send>;

/// Internal message sent through the work channel.
enum WorkMsg {
    Job(BoxedJob),
    Shutdown,
}

// ---------------------------------------------------------------------------
// WorkerPool
// ---------------------------------------------------------------------------

/// A bounded pool of async worker tasks that process submitted closures.
///
/// The pool is created via [`WorkerPool::new`] and shuts down when dropped
/// (all workers receive a `Shutdown` sentinel and terminate cleanly).
pub struct WorkerPool {
    sender: mpsc::Sender<WorkMsg>,
    semaphore: Arc<Semaphore>,
    stats: Arc<PoolStats>,
    _workers: Vec<tokio::task::JoinHandle<()>>,
}

struct PoolStats {
    dispatched: AtomicU64,
    succeeded: AtomicU64,
    failed: AtomicU64,
}

impl WorkerPool {
    /// Create a new pool with `cfg.num_workers` background tasks.
    pub fn new(cfg: WorkerPoolConfig) -> Self {
        let (tx, rx) = mpsc::channel::<WorkMsg>(cfg.num_workers * 4);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let semaphore = Arc::new(Semaphore::new(cfg.num_workers));
        let stats = Arc::new(PoolStats {
            dispatched: AtomicU64::new(0),
            succeeded: AtomicU64::new(0),
            failed: AtomicU64::new(0),
        });

        let mut handles = Vec::with_capacity(cfg.num_workers);
        for worker_id in 0..cfg.num_workers {
            let rx = rx.clone();
            let stats = stats.clone();

            let handle = tokio::spawn(async move {
                debug!(worker = worker_id, "worker started");
                loop {
                    let msg = {
                        let mut guard = rx.lock().await;
                        guard.recv().await
                    };

                    match msg {
                        Some(WorkMsg::Job(job_fn)) => {
                            let future = job_fn();
                            match future.await {
                                Ok(()) => {
                                    stats.succeeded.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(e) => {
                                    error!(worker = worker_id, error = %e, "job failed");
                                    stats.failed.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        }
                        Some(WorkMsg::Shutdown) | None => {
                            info!(worker = worker_id, "worker shutting down");
                            break;
                        }
                    }
                }
            });
            handles.push(handle);
        }

        Self {
            sender: tx,
            semaphore,
            stats,
            _workers: handles,
        }
    }

    /// Submit an async closure to the pool.  Returns immediately; the closure
    /// will be executed by the next available worker.
    ///
    /// If the internal queue is full this will block (apply back-pressure)
    /// until a slot is available.
    pub async fn submit<F, Fut>(&self, job: F) -> Result<()>
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let boxed: BoxedJob = Box::new(move || Box::pin(job()));
        self.stats.dispatched.fetch_add(1, Ordering::Relaxed);
        self.sender
            .send(WorkMsg::Job(boxed))
            .await
            .map_err(|_| anyhow::anyhow!("worker pool has been shut down"))?;
        Ok(())
    }

    /// Acquire a semaphore permit — used externally to cap concurrency without
    /// going through the job queue (e.g. for direct spawns in the downloader).
    pub fn semaphore(&self) -> Arc<Semaphore> {
        self.semaphore.clone()
    }

    /// Snapshot the current pool statistics.
    pub fn stats(&self) -> WorkerPoolStats {
        WorkerPoolStats {
            jobs_dispatched: self.stats.dispatched.load(Ordering::Relaxed),
            jobs_succeeded: self.stats.succeeded.load(Ordering::Relaxed),
            jobs_failed: self.stats.failed.load(Ordering::Relaxed),
        }
    }

    /// Gracefully shut down all workers after draining the queue.
    ///
    /// Calling this is optional; workers also stop when the pool is dropped.
    pub async fn shutdown(self) {
        // Send one shutdown sentinel per worker
        let n = self._workers.len();
        for _ in 0..n {
            let _ = self.sender.send(WorkMsg::Shutdown).await;
        }
        for handle in self._workers {
            let _ = handle.await;
        }
        info!("worker pool shut down cleanly");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering as AOrdering};

    #[tokio::test]
    async fn test_worker_pool_basic_submission() {
        let pool = WorkerPool::new(WorkerPoolConfig { num_workers: 2 });
        let counter = Arc::new(AtomicUsize::new(0));

        for _ in 0..10 {
            let c = counter.clone();
            pool.submit(move || async move {
                c.fetch_add(1, AOrdering::Relaxed);
                Ok(())
            })
            .await
            .unwrap();
        }

        // Give workers time to process
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let stats = pool.stats();
        assert_eq!(stats.jobs_dispatched, 10);
        // Not all may have completed yet (no await on completion), but none
        // should have failed.
        assert_eq!(stats.jobs_failed, 0);
    }

    #[test]
    fn test_worker_pool_config_default() {
        let cfg = WorkerPoolConfig::default();
        assert_eq!(cfg.num_workers, 8);
    }

    #[test]
    fn test_worker_pool_stats_default() {
        let stats = WorkerPoolStats::default();
        assert_eq!(stats.jobs_dispatched, 0);
        assert_eq!(stats.jobs_succeeded, 0);
        assert_eq!(stats.jobs_failed, 0);
    }
}
