//! Envoy proxy stats ingestion for the WebGL traffic-routing heatmap.
//!
//! This module polls the Envoy admin API (`/stats?format=json`) on each
//! Soroban RPC sidecar and emits normalised [`PodTrafficSnapshot`] frames over
//! a broadcast channel so the dashboard WebSocket layer can fan-out updates to
//! connected browsers in real time.
//!
//! # Design
//!
//! ```text
//!  ┌────────────────────────┐          ┌─────────────────────────┐
//!  │  EnvoyStatsStreamer     │ --tick-> │  Envoy Admin /stats     │
//!  │  (tokio task per pod)   │ <-parse- │  ?format=json           │
//!  └────────────┬───────────┘          └─────────────────────────┘
//!               │ broadcast::Sender<PodTrafficSnapshot>
//!               ▼
//!  WebSocket handler → browser (Web Worker → WebGL heatmap)
//! ```
//!
//! # Metrics extracted
//!
//! | Envoy stat key (prefix) | Mapped field |
//! |---|---|
//! | `downstream_cx_active` | `active_connections` |
//! | `downstream_rq_total` | `total_requests` |
//! | `downstream_rq_active` | `active_requests` |
//! | `upstream_cx_overflow` | `upstream_overflow` |
//! | `process_cpu_seconds_total` (admin) | `cpu_percent` (derived) |
//!
//! # Heatmap colour mapping
//!
//! The `heat_level` field is a normalised `f32` in `[0.0, 1.0]`:
//! - `0.0` → blue  (idle)
//! - `0.5` → yellow (moderate)
//! - `1.0` → white/red (overloaded)
//!
//! Callers should derive the render colour from this single scalar.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::time;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A point-in-time traffic snapshot for a single Envoy-sidecar pod.
///
/// Serialised as JSON and pushed to the browser via WebSocket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PodTrafficSnapshot {
    /// Kubernetes pod name (e.g. `soroban-rpc-7d9c8b-xk2pq`).
    pub pod_name: String,
    /// Kubernetes namespace.
    pub namespace: String,
    /// Geographic region label, if set via pod annotation
    /// `topology.kubernetes.io/region`.
    pub region: Option<String>,
    /// Number of active downstream TCP connections.
    pub active_connections: u64,
    /// Cumulative downstream requests since process start.
    pub total_requests: u64,
    /// Requests currently in flight.
    pub active_requests: u64,
    /// Upstream connection pool overflow events (indicates saturation).
    pub upstream_overflow: u64,
    /// Normalised load level in `[0.0, 1.0]` used for heatmap colouring.
    /// Derived from `active_requests / saturation_threshold`.
    pub heat_level: f32,
    /// Milliseconds since Unix epoch when this snapshot was captured.
    pub captured_at_ms: u64,
}

/// Configuration for a single Envoy sidecar target.
#[derive(Debug, Clone)]
pub struct EnvoyTarget {
    /// Pod name (used as snapshot identifier).
    pub pod_name: String,
    /// Kubernetes namespace.
    pub namespace: String,
    /// Fully-qualified admin endpoint URL, e.g.
    /// `http://10.0.1.42:15000/stats?format=json`.
    pub admin_url: String,
    /// Optional region label for geographic grouping in the heatmap.
    pub region: Option<String>,
    /// Request count that maps to `heat_level = 1.0` (overloaded).
    /// Defaults to [`DEFAULT_SATURATION_THRESHOLD`].
    pub saturation_threshold: u64,
}

/// Request count at which `heat_level` saturates to `1.0`.
pub const DEFAULT_SATURATION_THRESHOLD: u64 = 500;

// ---------------------------------------------------------------------------
// Internal Envoy JSON structures
// ---------------------------------------------------------------------------

/// Top-level response from `GET /stats?format=json`.
#[derive(Debug, Deserialize)]
struct EnvoyStatsResponse {
    stats: Vec<EnvoyStat>,
}

#[derive(Debug, Deserialize)]
struct EnvoyStat {
    name: String,
    value: Option<serde_json::Value>,
}

// ---------------------------------------------------------------------------
// Streamer
// ---------------------------------------------------------------------------

/// Polls a set of Envoy admin endpoints and broadcasts [`PodTrafficSnapshot`]
/// frames at a configurable interval.
pub struct EnvoyStatsStreamer {
    targets: Vec<EnvoyTarget>,
    interval: Duration,
    client: Client,
    tx: broadcast::Sender<PodTrafficSnapshot>,
}

impl EnvoyStatsStreamer {
    /// Create a new streamer.
    ///
    /// `capacity` is the broadcast channel buffer size (number of snapshots
    /// that can be queued before slow receivers start dropping frames).
    pub fn new(
        targets: Vec<EnvoyTarget>,
        interval: Duration,
        capacity: usize,
    ) -> (Self, broadcast::Receiver<PodTrafficSnapshot>) {
        let (tx, rx) = broadcast::channel(capacity);
        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client");

        (
            Self {
                targets,
                interval,
                client,
                tx,
            },
            rx,
        )
    }

    /// Subscribe to the snapshot stream.
    ///
    /// Multiple dashboard WebSocket handlers can each hold a receiver without
    /// any contention on the polling loop.
    pub fn subscribe(&self) -> broadcast::Receiver<PodTrafficSnapshot> {
        self.tx.subscribe()
    }

    /// Start the polling loop.  This method runs indefinitely and should be
    /// spawned as a background tokio task:
    ///
    /// ```rust,no_run
    /// # use stellar_telemetry::stream::envoy_stats::{EnvoyStatsStreamer, EnvoyTarget, DEFAULT_SATURATION_THRESHOLD};
    /// # use std::time::Duration;
    /// # #[tokio::main] async fn main() {
    /// let targets = vec![EnvoyTarget {
    ///     pod_name: "soroban-rpc-abc".into(),
    ///     namespace: "stellar".into(),
    ///     admin_url: "http://10.0.1.1:15000/stats?format=json".into(),
    ///     region: Some("us-east-1".into()),
    ///     saturation_threshold: DEFAULT_SATURATION_THRESHOLD,
    /// }];
    /// let (streamer, _rx) = EnvoyStatsStreamer::new(targets, Duration::from_secs(2), 256);
    /// tokio::spawn(async move { streamer.run().await });
    /// # }
    /// ```
    pub async fn run(self) {
        let mut ticker = time::interval(self.interval);
        ticker.set_missed_tick_behavior(time::MissedTickBehavior::Skip);

        info!(
            targets = self.targets.len(),
            interval_ms = self.interval.as_millis(),
            "Envoy stats streamer started"
        );

        loop {
            ticker.tick().await;

            // Poll all targets concurrently.
            let futs: Vec<_> = self
                .targets
                .iter()
                .map(|t| poll_target(&self.client, t))
                .collect();

            let results = futures::future::join_all(futs).await;

            for result in results {
                match result {
                    Ok(snapshot) => {
                        debug!(
                            pod = %snapshot.pod_name,
                            heat = snapshot.heat_level,
                            "Envoy snapshot captured"
                        );
                        // Ignore send errors — they just mean no subscribers yet.
                        let _ = self.tx.send(snapshot);
                    }
                    Err(e) => {
                        warn!("Failed to poll Envoy target: {e}");
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internal polling logic
// ---------------------------------------------------------------------------

/// Poll one Envoy admin endpoint and return a normalised snapshot.
async fn poll_target(
    client: &Client,
    target: &EnvoyTarget,
) -> Result<PodTrafficSnapshot, EnvoyPollError> {
    let start = Instant::now();

    let resp = client
        .get(&target.admin_url)
        .send()
        .await
        .map_err(|e| EnvoyPollError::Http(target.pod_name.clone(), e.to_string()))?;

    if !resp.status().is_success() {
        return Err(EnvoyPollError::BadStatus(
            target.pod_name.clone(),
            resp.status().as_u16(),
        ));
    }

    let body: EnvoyStatsResponse = resp
        .json()
        .await
        .map_err(|e| EnvoyPollError::Parse(target.pod_name.clone(), e.to_string()))?;

    let elapsed = start.elapsed();
    debug!(
        pod = %target.pod_name,
        latency_ms = elapsed.as_millis(),
        stats_count = body.stats.len(),
        "Envoy stats fetched"
    );

    let stats = parse_stats(&body.stats);

    let active_requests = stats
        .get("downstream_rq_active")
        .copied()
        .unwrap_or(0);

    let heat_level = compute_heat_level(active_requests, target.saturation_threshold);

    let captured_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    Ok(PodTrafficSnapshot {
        pod_name: target.pod_name.clone(),
        namespace: target.namespace.clone(),
        region: target.region.clone(),
        active_connections: stats.get("downstream_cx_active").copied().unwrap_or(0),
        total_requests: stats.get("downstream_rq_total").copied().unwrap_or(0),
        active_requests,
        upstream_overflow: stats.get("upstream_cx_overflow").copied().unwrap_or(0),
        heat_level,
        captured_at_ms,
    })
}

/// Extract the subset of Envoy stats we care about into a flat `HashMap`.
fn parse_stats(raw: &[EnvoyStat]) -> HashMap<String, u64> {
    // Keys we want to extract from the Envoy stats stream.
    const KEYS: &[&str] = &[
        "downstream_cx_active",
        "downstream_rq_total",
        "downstream_rq_active",
        "upstream_cx_overflow",
    ];

    let mut out = HashMap::with_capacity(KEYS.len());

    for stat in raw {
        for &key in KEYS {
            if stat.name.ends_with(key) {
                if let Some(v) = stat.value.as_ref().and_then(|v| v.as_u64()) {
                    out.insert(key.to_string(), v);
                }
            }
        }
    }

    out
}

/// Map an absolute request count to a normalised `[0.0, 1.0]` heat level.
///
/// Uses a simple linear ramp clamped to `[0.0, 1.0]`.  Downstream callers can
/// apply a non-linear colour curve (e.g. square-root) for perceptual uniformity.
#[inline]
fn compute_heat_level(active_requests: u64, threshold: u64) -> f32 {
    if threshold == 0 {
        return 0.0;
    }
    (active_requests as f32 / threshold as f32).min(1.0)
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur while polling a single Envoy sidecar.
#[derive(Debug, thiserror::Error)]
pub enum EnvoyPollError {
    #[error("HTTP error for pod {0}: {1}")]
    Http(String, String),
    #[error("Non-success HTTP status for pod {0}: {1}")]
    BadStatus(String, u16),
    #[error("JSON parse error for pod {0}: {1}")]
    Parse(String, String),
}

// ---------------------------------------------------------------------------
// Discovery helper
// ---------------------------------------------------------------------------

/// Build [`EnvoyTarget`] entries from a simple map of
/// `pod_name → admin_url` pairs.
///
/// In a real deployment you would source this from the Kubernetes API
/// (pod IPs + well-known Envoy admin port 15000) or from Istio's xDS.
pub fn targets_from_map(
    pod_map: &HashMap<String, String>,
    namespace: &str,
    saturation_threshold: u64,
) -> Vec<EnvoyTarget> {
    pod_map
        .iter()
        .map(|(pod_name, admin_url)| EnvoyTarget {
            pod_name: pod_name.clone(),
            namespace: namespace.to_string(),
            admin_url: admin_url.clone(),
            region: None,
            saturation_threshold,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heat_level_idle() {
        assert_eq!(compute_heat_level(0, 500), 0.0);
    }

    #[test]
    fn heat_level_half() {
        let v = compute_heat_level(250, 500);
        assert!((v - 0.5).abs() < 1e-6, "expected 0.5 got {v}");
    }

    #[test]
    fn heat_level_saturated() {
        assert_eq!(compute_heat_level(1000, 500), 1.0);
    }

    #[test]
    fn heat_level_zero_threshold() {
        assert_eq!(compute_heat_level(100, 0), 0.0);
    }

    #[test]
    fn parse_stats_extracts_known_keys() {
        let raw = vec![
            EnvoyStat {
                name: "http.ingress.downstream_cx_active".to_string(),
                value: Some(serde_json::json!(42)),
            },
            EnvoyStat {
                name: "http.ingress.downstream_rq_active".to_string(),
                value: Some(serde_json::json!(7)),
            },
            EnvoyStat {
                name: "http.ingress.downstream_rq_total".to_string(),
                value: Some(serde_json::json!(1234)),
            },
            EnvoyStat {
                name: "cluster.outbound.upstream_cx_overflow".to_string(),
                value: Some(serde_json::json!(0)),
            },
            EnvoyStat {
                name: "some.other.stat".to_string(),
                value: Some(serde_json::json!(99)),
            },
        ];

        let stats = parse_stats(&raw);
        assert_eq!(stats.get("downstream_cx_active"), Some(&42));
        assert_eq!(stats.get("downstream_rq_active"), Some(&7));
        assert_eq!(stats.get("downstream_rq_total"), Some(&1234));
        assert_eq!(stats.get("upstream_cx_overflow"), Some(&0));
        assert!(!stats.contains_key("some.other.stat"));
    }

    #[test]
    fn targets_from_map_basic() {
        let mut map = HashMap::new();
        map.insert(
            "soroban-rpc-abc".to_string(),
            "http://10.0.1.1:15000/stats?format=json".to_string(),
        );
        let targets = targets_from_map(&map, "stellar", 500);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].pod_name, "soroban-rpc-abc");
        assert_eq!(targets[0].namespace, "stellar");
        assert_eq!(targets[0].saturation_threshold, 500);
    }

    #[tokio::test]
    async fn streamer_subscribe_and_send() {
        let (streamer, mut rx) =
            EnvoyStatsStreamer::new(vec![], Duration::from_secs(60), 16);

        let snapshot = PodTrafficSnapshot {
            pod_name: "test-pod".to_string(),
            namespace: "stellar".to_string(),
            region: None,
            active_connections: 10,
            total_requests: 100,
            active_requests: 5,
            upstream_overflow: 0,
            heat_level: 0.01,
            captured_at_ms: 0,
        };

        streamer.tx.send(snapshot.clone()).unwrap();
        let received = rx.recv().await.unwrap();
        assert_eq!(received.pod_name, "test-pod");
        assert_eq!(received.heat_level, 0.01);
    }
}
