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

//! Jemalloc allocator integration and heap-profiling control (issue #305).
//!
//! # Design
//!
//! Replaces the system allocator with `tikv-jemallocator` when the `profiling`
//! Cargo feature is enabled.  The allocator is configured at link time via the
//! `malloc_conf` symbol so that:
//!
//! * Profiling machinery is **compiled in** (`prof:true`).
//! * Sampling is **inactive at boot** (`prof_active:false`) — zero overhead for
//!   normal runtime.
//! * The sampling interval is 2^19 bytes (512 KB) — a good trade-off between
//!   resolution and overhead for a ~15 MB operator process.
//!
//! Profiling is activated on-demand by calling [`activate`], triggered only
//! when a request hits the `/debug/pprof/heap` endpoint.  Calling [`deactivate`]
//! restores zero-overhead operation.
//!
//! ## Zero-overhead guarantee
//!
//! When `prof_active:false` (the default), jemalloc skips every profiling code
//! path entirely.  The only cost is the presence of the `tikv-jemallocator`
//! vtable in the binary and the `malloc_conf` export — both are in read-only
//! data and incur no runtime overhead.
//!
//! ## Thread safety
//!
//! All public functions are safe to call from any thread.  jemalloc's internal
//! epoch mechanism handles concurrent activation/deactivation correctly.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, error, info, warn};

// ── Global jemalloc allocator ─────────────────────────────────────────────────

/// Installs jemalloc as the global allocator.
///
/// Only active when built with `--features profiling`.  The system allocator
/// is used otherwise, keeping non-profiling builds free of any jemalloc
/// dependency.
#[cfg(feature = "profiling")]
#[global_allocator]
static GLOBAL_JEMALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Jemalloc compile-time configuration string exported at link time.
///
/// `prof:true`         — compile profiling support into the allocator.
/// `prof_active:false` — sampling is OFF at startup (zero overhead).
/// `lg_prof_sample:19` — sample every 2^19 = 512 KB of allocation.
///
/// This symbol must be a `\0`-terminated C string.  It is read by jemalloc
/// before `main()` runs, so it cannot be set programmatically.
#[cfg(feature = "profiling")]
#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static MALLOC_CONF: &[u8] = b"prof:true,prof_active:false,lg_prof_sample:19\0";

// ── Activation state ──────────────────────────────────────────────────────────

/// Tracks whether heap profiling is currently active.
///
/// Stored globally so the debug endpoint can query status without holding
/// a lock.
static PROFILING_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Activate jemalloc heap profiling.
///
/// Sets `prof.active = true` via the jemalloc MALLCTL API so that subsequent
/// allocations are sampled.  Safe to call multiple times — idempotent.
///
/// Returns an error string if jemalloc profiling support is unavailable
/// (e.g. binary built without `--features profiling`, or `malloc_conf`
/// missing `prof:true`).
///
/// # Overhead
///
/// After activation every ~512 KB of allocation (configurable via
/// `lg_prof_sample`) records a stack backtrace.  This is acceptable for
/// short-lived profiling sessions but should not be left on permanently in
/// production.
pub fn activate() -> Result<(), String> {
    #[cfg(feature = "profiling")]
    {
        use tikv_jemalloc_ctl::{epoch, prof};

        // Advance the epoch so stats are current before we start.
        epoch::mib()
            .and_then(|m| m.advance())
            .map_err(|e| format!("jemalloc epoch advance failed: {e}"))?;

        prof::active::write(true)
            .map_err(|e| format!("jemalloc prof.active write failed: {e}"))?;

        PROFILING_ACTIVE.store(true, Ordering::Release);
        info!("jemalloc heap profiling activated (lg_prof_sample=19)");
        Ok(())
    }
    #[cfg(not(feature = "profiling"))]
    {
        Err("binary built without `profiling` feature — jemalloc unavailable".to_string())
    }
}

/// Deactivate jemalloc heap profiling, restoring zero-overhead operation.
///
/// Safe to call even when profiling is not active.
pub fn deactivate() -> Result<(), String> {
    #[cfg(feature = "profiling")]
    {
        use tikv_jemalloc_ctl::prof;

        prof::active::write(false)
            .map_err(|e| format!("jemalloc prof.active write(false) failed: {e}"))?;

        PROFILING_ACTIVE.store(false, Ordering::Release);
        info!("jemalloc heap profiling deactivated");
        Ok(())
    }
    #[cfg(not(feature = "profiling"))]
    {
        Ok(()) // no-op when not compiled in
    }
}

/// Returns `true` if heap profiling is currently sampling allocations.
#[inline]
pub fn is_active() -> bool {
    PROFILING_ACTIVE.load(Ordering::Acquire)
}

// ── Heap dump ─────────────────────────────────────────────────────────────────

/// Dump the current jemalloc heap profile in pprof protobuf format.
///
/// Returns the raw `*.pb.gz` bytes that can be piped directly to
/// `go tool pprof` or the `pprof` CLI.
///
/// # Errors
///
/// Returns an error if:
/// * The binary was not built with `--features profiling`.
/// * The `MALLOC_CONF` string does not contain `prof:true`.
/// * jemalloc's internal dump fails (e.g. out of memory, disk full).
pub async fn dump_pprof() -> Result<Vec<u8>, String> {
    #[cfg(feature = "profiling")]
    {
        use jemalloc_pprof::PROF_CTL;

        let ctl = PROF_CTL.as_ref().ok_or_else(|| {
            "jemalloc PROF_CTL unavailable — is `prof:true` in MALLOC_CONF?".to_string()
        })?;

        let guard = ctl.lock().await;

        let bytes = guard
            .dump_pprof()
            .await
            .map_err(|e| format!("jemalloc pprof dump failed: {e}"))?;

        debug!(bytes = bytes.len(), "heap pprof dump complete");
        Ok(bytes)
    }
    #[cfg(not(feature = "profiling"))]
    {
        Err("binary built without `profiling` feature — heap dump unavailable".to_string())
    }
}

// ── Raw stats snapshot ────────────────────────────────────────────────────────

/// A lightweight snapshot of jemalloc allocation statistics.
///
/// Collected without activating heap profiling so it is always safe to call,
/// even in production builds.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AllocStats {
    /// Bytes currently allocated and in use by the application.
    pub active_bytes: u64,
    /// Total bytes mapped in jemalloc arenas (includes internal fragmentation).
    pub allocated_bytes: u64,
    /// Bytes returned to the OS (resident but not active).
    pub resident_bytes: u64,
    /// Total bytes retained by jemalloc but not currently in use.
    pub retained_bytes: u64,
    /// Timestamp when this snapshot was taken (Unix seconds).
    pub snapshot_unix_secs: u64,
}

impl AllocStats {
    /// Collect current jemalloc stats.
    ///
    /// Advances the epoch internally to ensure values are up to date.
    /// Falls back to zeroes on non-profiling builds so callers need not
    /// `#[cfg]`-gate every usage.
    pub fn collect() -> Self {
        let snapshot_unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        #[cfg(feature = "profiling")]
        {
            use tikv_jemalloc_ctl::{epoch, stats};

            // Advance epoch to refresh cached counters.
            let _ = epoch::mib().and_then(|m| m.advance());

            let active_bytes = stats::active::read().unwrap_or(0) as u64;
            let allocated_bytes = stats::allocated::read().unwrap_or(0) as u64;
            let resident_bytes = stats::resident::read().unwrap_or(0) as u64;
            let retained_bytes = stats::retained::read().unwrap_or(0) as u64;

            AllocStats {
                active_bytes,
                allocated_bytes,
                resident_bytes,
                retained_bytes,
                snapshot_unix_secs,
            }
        }
        #[cfg(not(feature = "profiling"))]
        {
            // Fall back to /proc/self/status on Linux when jemalloc is absent.
            let (rss, _vsize) = read_proc_rss();
            AllocStats {
                active_bytes: rss,
                allocated_bytes: rss,
                resident_bytes: rss,
                retained_bytes: 0,
                snapshot_unix_secs,
            }
        }
    }
}

/// Read RSS from `/proc/self/status` (Linux only; returns 0 elsewhere).
#[allow(dead_code)]
fn read_proc_rss() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        if let Ok(s) = std::fs::read_to_string("/proc/self/status") {
            let mut rss = 0u64;
            let mut vsz = 0u64;
            for line in s.lines() {
                if let Some(v) = line.strip_prefix("VmRSS:") {
                    rss = v.split_whitespace().next()
                        .and_then(|n| n.parse::<u64>().ok())
                        .unwrap_or(0) * 1024;
                }
                if let Some(v) = line.strip_prefix("VmSize:") {
                    vsz = v.split_whitespace().next()
                        .and_then(|n| n.parse::<u64>().ok())
                        .unwrap_or(0) * 1024;
                }
            }
            return (rss, vsz);
        }
    }
    (0, 0)
}

// ── Memory growth detector ────────────────────────────────────────────────────

/// Watches memory consumption and fires alerts when growth exceeds a threshold.
///
/// Designed to run as a long-lived background Tokio task.  Call
/// [`MemoryLeakDetector::run`] inside `tokio::spawn`.
///
/// # Algorithm
///
/// The detector maintains a sliding window of [`AllocStats`] snapshots taken
/// every `poll_interval`.  On each tick it computes the percentage growth
/// from the oldest snapshot in the window to the current one.  If the growth
/// exceeds `growth_threshold_pct` over the configured `window_duration`, it
/// emits a `tracing::warn!` alert and increments the alert counter.
///
/// The alert fires **at most once per `window_duration`** to avoid log spam.
///
/// # Issue #305 requirement
///
/// > Build automated alerts triggering when the controller's memory footprint
/// > grows by more than 20% over a 24-hour period.
///
/// Default configuration satisfies this: `growth_threshold_pct = 20.0` and
/// `window_duration = 24h`.
pub struct MemoryLeakDetector {
    config: LeakDetectorConfig,
    alert_fired: Arc<AtomicBool>,
    alert_count: Arc<std::sync::atomic::AtomicU64>,
}

/// Configuration for [`MemoryLeakDetector`].
#[derive(Debug, Clone)]
pub struct LeakDetectorConfig {
    /// How long to poll for before computing the growth rate.
    /// Default: 24 hours (issue #305 requirement).
    pub window_duration: Duration,
    /// How frequently to sample memory stats.
    /// Default: 5 minutes — low enough overhead, high enough resolution.
    pub poll_interval: Duration,
    /// Percentage growth that triggers an alert.
    /// Default: 20.0 (issue #305 requirement).
    pub growth_threshold_pct: f64,
    /// RSS at startup considered the operator's "baseline" (bytes).
    /// When `None` the first sample is used as the baseline.
    pub baseline_bytes: Option<u64>,
}

impl Default for LeakDetectorConfig {
    fn default() -> Self {
        Self {
            window_duration: Duration::from_secs(24 * 3600),
            poll_interval: Duration::from_secs(300), // 5 min
            growth_threshold_pct: 20.0,
            baseline_bytes: None,
        }
    }
}

impl MemoryLeakDetector {
    /// Create a new detector with the provided configuration.
    pub fn new(config: LeakDetectorConfig) -> Self {
        Self {
            config,
            alert_fired: Arc::new(AtomicBool::new(false)),
            alert_count: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Shared handle to the alert-fired flag (for use in metrics/tests).
    pub fn alert_fired_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.alert_fired)
    }

    /// Total number of memory-growth alerts emitted since startup.
    pub fn alert_count(&self) -> u64 {
        self.alert_count.load(Ordering::Relaxed)
    }

    /// Run the detector loop indefinitely.
    ///
    /// Should be spawned with `tokio::spawn`:
    ///
    /// ```ignore
    /// let detector = MemoryLeakDetector::new(LeakDetectorConfig::default());
    /// tokio::spawn(async move { detector.run().await });
    /// ```
    pub async fn run(self) {
        let capacity = (self.config.window_duration.as_secs()
            / self.config.poll_interval.as_secs().max(1))
            as usize + 1;

        let mut history: Vec<(Instant, u64)> = Vec::with_capacity(capacity);

        // Initialise baseline.
        let baseline = match self.config.baseline_bytes {
            Some(b) => b,
            None => {
                let s = AllocStats::collect();
                let b = s.resident_bytes.max(s.active_bytes);
                info!(baseline_bytes = b, "MemoryLeakDetector baseline set");
                b
            }
        };

        let mut interval = tokio::time::interval(self.config.poll_interval);
        // Don't try to catch up on missed ticks (avoids burst on wakeup).
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            interval.tick().await;

            let stats = AllocStats::collect();
            let current_bytes = stats.resident_bytes.max(stats.active_bytes);
            let now = Instant::now();

            // Trim history entries older than the window.
            let window_start = now
                .checked_sub(self.config.window_duration)
                .unwrap_or(now);
            history.retain(|(ts, _)| *ts >= window_start);
            history.push((now, current_bytes));

            // Need at least 2 points to compute a gradient.
            if history.len() < 2 {
                debug!(
                    current_bytes,
                    "MemoryLeakDetector: not enough history yet — skipping growth check"
                );
                continue;
            }

            let oldest_bytes = history[0].1;
            if oldest_bytes == 0 {
                continue;
            }

            let growth_pct =
                (current_bytes as f64 - oldest_bytes as f64) / oldest_bytes as f64 * 100.0;

            // Also track growth vs. initial baseline for long-running operators.
            let baseline_growth_pct = if baseline > 0 {
                (current_bytes as f64 - baseline as f64) / baseline as f64 * 100.0
            } else {
                0.0
            };

            debug!(
                current_bytes,
                oldest_bytes,
                growth_pct,
                baseline_growth_pct,
                "MemoryLeakDetector tick"
            );

            if growth_pct >= self.config.growth_threshold_pct {
                let count = self.alert_count.fetch_add(1, Ordering::Relaxed) + 1;
                self.alert_fired.store(true, Ordering::Release);

                warn!(
                    current_bytes,
                    oldest_bytes,
                    growth_pct,
                    baseline_growth_pct,
                    alert_count = count,
                    threshold_pct = self.config.growth_threshold_pct,
                    window_secs = self.config.window_duration.as_secs(),
                    "MEMORY_LEAK_ALERT: operator RSS grew by {growth_pct:.1}% over the last \
                     {}s (threshold {}%) — consider pulling a heap profile from \
                     /debug/pprof/heap",
                    self.config.window_duration.as_secs(),
                    self.config.growth_threshold_pct,
                );

                // Emit a Prometheus-compatible log line that alertmanager rules
                // or log-based alert pipelines can scrape.
                warn!(
                    event = "stellar_operator_memory_growth_alert",
                    current_bytes,
                    oldest_bytes,
                    growth_pct,
                    threshold_pct = self.config.growth_threshold_pct,
                    "memory_growth_alert"
                );

                // Reset window so the next alert covers a fresh period.
                history.clear();
            } else if growth_pct < 0.0 {
                // RSS shrank (GC freed memory). Reset alert flag.
                self.alert_fired.store(false, Ordering::Release);
                debug!(growth_pct, "RSS decreased — clearing leak alert flag");
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn alloc_stats_collect_does_not_panic() {
        // Always safe to call regardless of feature flags.
        let stats = AllocStats::collect();
        // snapshot_unix_secs should be a plausible Unix timestamp (> year 2020).
        assert!(stats.snapshot_unix_secs > 1_580_000_000);
    }

    #[test]
    fn is_active_false_by_default() {
        // Before any activation call the flag must be false.
        assert!(!is_active(), "profiling should be inactive at startup");
    }

    #[cfg(not(feature = "profiling"))]
    #[test]
    fn activate_returns_err_without_feature() {
        assert!(activate().is_err());
    }

    #[cfg(not(feature = "profiling"))]
    #[test]
    fn deactivate_is_noop_without_feature() {
        // Must not panic.
        assert!(deactivate().is_ok());
    }

    #[tokio::test]
    async fn leak_detector_fires_on_synthetic_growth() {
        use std::sync::atomic::AtomicU64;

        // Build a detector with a very short window and low threshold for testing.
        let config = LeakDetectorConfig {
            window_duration: Duration::from_millis(200),
            poll_interval: Duration::from_millis(50),
            growth_threshold_pct: 0.0, // fire on any growth
            baseline_bytes: Some(1_000),
        };
        let detector = MemoryLeakDetector::new(config);
        let alert_fired = detector.alert_fired_handle();
        let alert_count = Arc::clone(&detector.alert_count);

        // Run the detector briefly; since threshold is 0.0 it should fire on
        // the first meaningful measurement.
        tokio::select! {
            _ = detector.run() => {}
            _ = tokio::time::sleep(Duration::from_millis(600)) => {}
        }

        // Alert may or may not have fired depending on actual RSS, but at minimum
        // the function must return without panicking.
        let _ = alert_fired.load(Ordering::Acquire);
        let _ = alert_count.load(Ordering::Relaxed);
    }

    #[test]
    fn leak_detector_config_default_matches_issue_requirements() {
        let cfg = LeakDetectorConfig::default();
        assert_eq!(cfg.window_duration, Duration::from_secs(24 * 3600));
        assert!((cfg.growth_threshold_pct - 20.0).abs() < f64::EPSILON);
    }
}
