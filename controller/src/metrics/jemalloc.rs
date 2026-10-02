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

//! jemalloc Heap-Memory Metrics for the WASM Defragmentation Controller
//!
//! This module reads jemalloc allocator statistics via the `tikv-jemalloc-ctl`
//! crate and exposes them as typed structs that the defragmentation controller
//! consumes to determine whether a pod's heap is fragmented beyond the 30 %
//! threshold defined in issue #332.
//!
//! # Fragmentation Ratio
//!
//! The fragmentation ratio is defined as:
//!
//! ```text
//! fragmentation_ratio = 1.0 - (active_bytes / resident_bytes)
//! ```
//!
//! | Value | Meaning                                         |
//! |-------|-------------------------------------------------|
//! | 0.00  | No fragmentation; resident == active            |
//! | 0.30  | 30 % of resident memory is unusable overhead   |
//! | 1.00  | Fully fragmented (degenerate / impossible case) |
//!
//! A ratio ≥ 0.30 triggers the defragmentation cycle defined in
//! `crate::ha::defrag`.
//!
//! # Metric Collection Without a Live Allocator
//!
//! When the operator itself is **not** linked against jemalloc (e.g. in unit
//! tests or in environments that only have the system allocator), every
//! `tikv_jemalloc_ctl` call returns an error.  `JemallocSnapshot::collect()`
//! handles this gracefully by returning `Ok(JemallocSnapshot::zeroed())`.
//!
//! # Remote Pod Metrics
//!
//! For Soroban RPC pods the operator cannot call jemalloc APIs directly. The
//! defragmentation controller instead scrapes the pod's Prometheus `/metrics`
//! endpoint and delegates parsing to `JemallocSnapshot::from_prometheus_text`.

use std::time::{Duration, Instant};

use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Public snapshot type
// ---------------------------------------------------------------------------

/// A point-in-time snapshot of jemalloc heap statistics.
///
/// All byte fields are **bytes** (not kilobytes / megabytes).  Consumers
/// should treat a zeroed snapshot (all fields == 0) as "metrics unavailable".
#[derive(Debug, Clone, PartialEq)]
pub struct JemallocSnapshot {
    /// Bytes in active extents that have been allocated by the application.
    /// Corresponds to `stats.active` in jemalloc.
    pub active_bytes: u64,

    /// Bytes in physically resident data pages mapped by the allocator.
    /// Corresponds to `stats.resident` in jemalloc.
    pub resident_bytes: u64,

    /// Bytes currently allocated and in use by the application.
    /// Corresponds to `stats.allocated` in jemalloc.
    pub allocated_bytes: u64,

    /// Bytes in mapped virtual-address space that have been retained by the
    /// allocator but are not yet released to the OS.
    /// Corresponds to `stats.retained` in jemalloc.
    pub retained_bytes: u64,

    /// Number of jemalloc arenas (one per thread by default).
    pub num_arenas: u32,

    /// Fragmentation ratio in `[0.0, 1.0]`.  Computed as
    /// `1.0 - (active_bytes / resident_bytes)`, or `0.0` when
    /// `resident_bytes == 0`.
    pub fragmentation_ratio: f64,

    /// Wall-clock timestamp at which this snapshot was captured.
    pub captured_at: Instant,
}

impl JemallocSnapshot {
    /// Return a zeroed snapshot that signals "metrics unavailable".
    pub fn zeroed() -> Self {
        Self {
            active_bytes: 0,
            resident_bytes: 0,
            allocated_bytes: 0,
            retained_bytes: 0,
            num_arenas: 0,
            fragmentation_ratio: 0.0,
            captured_at: Instant::now(),
        }
    }

    /// Collect live jemalloc statistics from the current process.
    ///
    /// Calls `tikv_jemalloc_ctl::epoch::mib()` to refresh the stats epoch
    /// before reading, ensuring values are not stale.
    ///
    /// Returns `Ok(JemallocSnapshot::zeroed())` when jemalloc is unavailable
    /// (e.g. system allocator, unit-test environment).
    pub fn collect() -> Result<Self, JemallocError> {
        #[cfg(feature = "jemalloc")]
        {
            use tikv_jemalloc_ctl::{epoch, stats};

            // Refresh the stats epoch so we get up-to-date numbers.
            let epoch_mib = epoch::mib().map_err(|e| JemallocError::Ctl(e.to_string()))?;
            epoch_mib
                .advance()
                .map_err(|e| JemallocError::Ctl(e.to_string()))?;

            let active_bytes =
                stats::active::read().map_err(|e| JemallocError::Ctl(e.to_string()))? as u64;
            let resident_bytes =
                stats::resident::read().map_err(|e| JemallocError::Ctl(e.to_string()))? as u64;
            let allocated_bytes =
                stats::allocated::read().map_err(|e| JemallocError::Ctl(e.to_string()))? as u64;
            let retained_bytes =
                stats::retained::read().map_err(|e| JemallocError::Ctl(e.to_string()))? as u64;

            let fragmentation_ratio = compute_fragmentation(active_bytes, resident_bytes);

            debug!(
                active_bytes,
                resident_bytes,
                allocated_bytes,
                retained_bytes,
                fragmentation_ratio,
                "jemalloc snapshot collected"
            );

            Ok(Self {
                active_bytes,
                resident_bytes,
                allocated_bytes,
                retained_bytes,
                num_arenas: 0, // arena count requires per-arena iteration; omitted for now
                fragmentation_ratio,
                captured_at: Instant::now(),
            })
        }

        #[cfg(not(feature = "jemalloc"))]
        {
            debug!("jemalloc feature not enabled; returning zeroed snapshot");
            Ok(Self::zeroed())
        }
    }

    /// Parse a jemalloc snapshot from a Prometheus text-format scrape.
    ///
    /// This is used when the operator needs to evaluate the heap health of a
    /// *remote* Soroban RPC pod rather than its own process.  The pod must
    /// expose the following Prometheus metrics (as produced by the
    /// `jemalloc_pprof` or compatible exporter):
    ///
    /// - `jemalloc_active_bytes`
    /// - `jemalloc_resident_bytes`
    /// - `jemalloc_allocated_bytes`
    /// - `jemalloc_retained_bytes`
    ///
    /// Lines that are not recognised are silently ignored.
    pub fn from_prometheus_text(text: &str) -> Result<Self, JemallocError> {
        let mut active_bytes: Option<u64> = None;
        let mut resident_bytes: Option<u64> = None;
        let mut allocated_bytes: Option<u64> = None;
        let mut retained_bytes: Option<u64> = None;

        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') || line.is_empty() {
                continue;
            }

            let (name, value) = parse_prometheus_line(line)?;
            match name {
                "jemalloc_active_bytes" => active_bytes = Some(value),
                "jemalloc_resident_bytes" => resident_bytes = Some(value),
                "jemalloc_allocated_bytes" => allocated_bytes = Some(value),
                "jemalloc_retained_bytes" => retained_bytes = Some(value),
                _ => {}
            }
        }

        match (active_bytes, resident_bytes, allocated_bytes, retained_bytes) {
            (Some(a), Some(r), Some(al), Some(ret)) => {
                let fragmentation_ratio = compute_fragmentation(a, r);
                Ok(Self {
                    active_bytes: a,
                    resident_bytes: r,
                    allocated_bytes: al,
                    retained_bytes: ret,
                    num_arenas: 0,
                    fragmentation_ratio,
                    captured_at: Instant::now(),
                })
            }
            _ => {
                warn!("Prometheus scrape did not contain all required jemalloc metrics");
                Ok(Self::zeroed())
            }
        }
    }

    /// Returns `true` when the fragmentation ratio meets or exceeds the
    /// supplied threshold (expressed as a value in `[0.0, 1.0]`).
    ///
    /// # Example
    ///
    /// ```
    /// use controller::metrics::jemalloc::JemallocSnapshot;
    ///
    /// let snap = JemallocSnapshot { fragmentation_ratio: 0.35, ..JemallocSnapshot::zeroed() };
    /// assert!(snap.is_fragmented(0.30));
    /// assert!(!snap.is_fragmented(0.40));
    /// ```
    pub fn is_fragmented(&self, threshold: f64) -> bool {
        self.fragmentation_ratio >= threshold
    }

    /// Returns the age of this snapshot.
    pub fn age(&self) -> Duration {
        self.captured_at.elapsed()
    }
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur when reading jemalloc statistics.
#[derive(Debug, thiserror::Error)]
pub enum JemallocError {
    /// A `tikv_jemalloc_ctl` MIB call failed.
    #[error("jemalloc ctl error: {0}")]
    Ctl(String),

    /// The Prometheus text could not be parsed.
    #[error("prometheus parse error: {0}")]
    PrometheusParse(String),
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Compute the fragmentation ratio from active and resident byte counts.
#[inline]
fn compute_fragmentation(active_bytes: u64, resident_bytes: u64) -> f64 {
    if resident_bytes == 0 {
        return 0.0;
    }
    let ratio = 1.0 - (active_bytes as f64 / resident_bytes as f64);
    // Clamp to [0.0, 1.0] to handle minor jemalloc accounting quirks where
    // active can momentarily exceed resident.
    ratio.clamp(0.0, 1.0)
}

/// Parse a single Prometheus text line into `(metric_name, value_as_u64)`.
///
/// Supports lines of the form:
/// ```text
/// metric_name{label="value"} 12345
/// metric_name 12345
/// ```
fn parse_prometheus_line(line: &str) -> Result<(&str, u64), JemallocError> {
    // Split on the last whitespace-separated token which is the value.
    let mut parts = line.rsplitn(2, ' ');
    let value_str = parts.next().ok_or_else(|| {
        JemallocError::PrometheusParse(format!("no value in line: {line}"))
    })?;
    let name_part = parts.next().ok_or_else(|| {
        JemallocError::PrometheusParse(format!("no name in line: {line}"))
    })?;

    // Strip label set `{...}` from the metric name part.
    let name = if let Some(brace_pos) = name_part.find('{') {
        &name_part[..brace_pos]
    } else {
        name_part.trim()
    };

    // Parse the value; jemalloc byte counters are always non-negative integers.
    let value = value_str
        .trim()
        .parse::<f64>()
        .map(|f| f as u64)
        .map_err(|e| {
            JemallocError::PrometheusParse(format!("cannot parse '{value_str}' as u64: {e}"))
        })?;

    Ok((name, value))
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_fragmentation_ratio_zero_resident() {
        assert_eq!(compute_fragmentation(0, 0), 0.0);
    }

    #[test]
    fn test_fragmentation_ratio_no_fragmentation() {
        // active == resident → ratio should be 0.0
        assert!((compute_fragmentation(1_000_000, 1_000_000) - 0.0).abs() < 1e-9);
    }

    #[test]
    fn test_fragmentation_ratio_thirty_percent() {
        // active = 700k, resident = 1000k → 30 % fragmented
        let ratio = compute_fragmentation(700_000, 1_000_000);
        assert!((ratio - 0.3).abs() < 1e-9, "expected 0.3, got {ratio}");
    }

    #[test]
    fn test_fragmentation_ratio_clamped() {
        // active > resident (accounting glitch) → clamp to 0.0
        assert_eq!(compute_fragmentation(1_200_000, 1_000_000), 0.0);
    }

    #[test]
    fn test_is_fragmented_above_threshold() {
        let snap = JemallocSnapshot {
            fragmentation_ratio: 0.35,
            ..JemallocSnapshot::zeroed()
        };
        assert!(snap.is_fragmented(0.30));
    }

    #[test]
    fn test_is_fragmented_below_threshold() {
        let snap = JemallocSnapshot {
            fragmentation_ratio: 0.20,
            ..JemallocSnapshot::zeroed()
        };
        assert!(!snap.is_fragmented(0.30));
    }

    #[test]
    fn test_is_fragmented_at_threshold() {
        let snap = JemallocSnapshot {
            fragmentation_ratio: 0.30,
            ..JemallocSnapshot::zeroed()
        };
        assert!(snap.is_fragmented(0.30));
    }

    #[test]
    fn test_zeroed_snapshot() {
        let s = JemallocSnapshot::zeroed();
        assert_eq!(s.active_bytes, 0);
        assert_eq!(s.resident_bytes, 0);
        assert_eq!(s.fragmentation_ratio, 0.0);
        assert!(!s.is_fragmented(0.30));
    }

    #[test]
    fn test_collect_without_jemalloc_feature() {
        // When the jemalloc feature is not enabled the function must not panic
        // and must return a zeroed snapshot.
        let snap = JemallocSnapshot::collect().expect("collect must not fail");
        // In CI the feature is disabled, so we expect all zeros.
        #[cfg(not(feature = "jemalloc"))]
        assert_eq!(snap.active_bytes, 0);
    }

    #[test]
    fn test_parse_prometheus_text_full() {
        let text = r#"
# HELP jemalloc_active_bytes Active bytes
# TYPE jemalloc_active_bytes gauge
jemalloc_active_bytes 700000
# HELP jemalloc_resident_bytes Resident bytes
# TYPE jemalloc_resident_bytes gauge
jemalloc_resident_bytes 1000000
# HELP jemalloc_allocated_bytes Allocated bytes
# TYPE jemalloc_allocated_bytes gauge
jemalloc_allocated_bytes 650000
# HELP jemalloc_retained_bytes Retained bytes
# TYPE jemalloc_retained_bytes gauge
jemalloc_retained_bytes 50000
"#;
        let snap =
            JemallocSnapshot::from_prometheus_text(text).expect("parse must succeed");
        assert_eq!(snap.active_bytes, 700_000);
        assert_eq!(snap.resident_bytes, 1_000_000);
        assert_eq!(snap.allocated_bytes, 650_000);
        assert_eq!(snap.retained_bytes, 50_000);
        assert!(
            (snap.fragmentation_ratio - 0.3).abs() < 1e-9,
            "ratio: {}",
            snap.fragmentation_ratio
        );
        assert!(snap.is_fragmented(0.30));
    }

    #[test]
    fn test_parse_prometheus_text_with_labels() {
        let text = "jemalloc_active_bytes{pod=\"soroban-rpc-0\",namespace=\"stellar\"} 700000\n\
                    jemalloc_resident_bytes{pod=\"soroban-rpc-0\"} 1000000\n\
                    jemalloc_allocated_bytes 650000\n\
                    jemalloc_retained_bytes 50000\n";
        let snap = JemallocSnapshot::from_prometheus_text(text).expect("parse must succeed");
        assert_eq!(snap.active_bytes, 700_000);
        assert_eq!(snap.resident_bytes, 1_000_000);
    }

    #[test]
    fn test_parse_prometheus_text_missing_fields_returns_zeroed() {
        let text = "jemalloc_active_bytes 700000\n";
        let snap = JemallocSnapshot::from_prometheus_text(text).expect("should not error");
        assert_eq!(snap.active_bytes, 0, "zeroed when fields missing");
    }

    #[test]
    fn test_parse_prometheus_line_plain() {
        let (name, value) = parse_prometheus_line("jemalloc_active_bytes 123456").unwrap();
        assert_eq!(name, "jemalloc_active_bytes");
        assert_eq!(value, 123_456);
    }

    #[test]
    fn test_parse_prometheus_line_with_labels() {
        let (name, value) =
            parse_prometheus_line("jemalloc_active_bytes{pod=\"p0\"} 9876").unwrap();
        assert_eq!(name, "jemalloc_active_bytes");
        assert_eq!(value, 9_876);
    }
}
