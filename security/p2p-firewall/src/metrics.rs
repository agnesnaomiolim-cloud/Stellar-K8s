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
//! Prometheus metrics definitions for the P2P firewall.
//!
//! # Metrics exposed
//!
//! | Metric name                              | Type    | Description                                   |
//! |------------------------------------------|---------|-----------------------------------------------|
//! | `p2p_firewall_packets_inspected_total`   | Counter | Total packets inspected by the firewall        |
//! | `p2p_firewall_threats_detected_total`    | Counter | Threats detected, labelled by `kind`           |
//! | `p2p_firewall_bans_active_total`         | Counter | Cumulative bans issued (ever)                  |
//! | `p2p_firewall_bans_expired_total`        | Counter | Bans that have expired / been revoked          |
//! | `p2p_firewall_analysis_latency_ns`       | Gauge   | Last single-packet analysis latency (ns)       |
//!
//! All metrics use the `p2p_firewall_` prefix to avoid collision with the
//! existing `scp_sniffer_*` and `ebpf_*` metric families.
//!
//! # Scrape endpoint
//!
//! The firewall starts an Axum HTTP server on `metrics_addr` (default
//! `0.0.0.0:9437`) that serves the encoded Prometheus text at `GET /metrics`.

use crate::analyzer::AnalysisResult;
use axum::{extract::State, routing::get, Router};
use prometheus_client::{
    encoding::{text::encode, EncodeLabelSet},
    metrics::{counter::Counter, family::Family, gauge::Gauge},
    registry::Registry,
};
use std::sync::{Arc, Mutex};
use tracing::{error, info};

// ─── Label sets ───────────────────────────────────────────────────────────────

/// Label set for the per-threat-kind counter.
#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ThreatLabels {
    /// Threat kind string (e.g. `flood_attack`, `malformed_too_small`).
    pub kind: String,
}

// ─── Metrics struct ───────────────────────────────────────────────────────────

/// Prometheus metrics for the P2P firewall.
///
/// This struct is constructed once and shared via `Arc` throughout the crate.
pub struct FirewallMetrics {
    /// Total packets inspected.
    pub packets_inspected: Counter,
    /// Threats detected, labelled by kind.
    pub threats_detected: Family<ThreatLabels, Counter>,
    /// Cumulative bans issued.
    pub bans_active: Counter,
    /// Bans that have been removed (expired or manual).
    pub bans_expired: Counter,
    /// Last analysis latency in nanoseconds.
    pub analysis_latency_ns: Gauge,
    /// The prometheus-client registry (for encoding).
    registry: Mutex<Registry>,
}

impl FirewallMetrics {
    /// Construct and register all metrics.
    pub fn new() -> Self {
        let mut registry = Registry::default();

        let packets_inspected = Counter::default();
        registry.register(
            "p2p_firewall_packets_inspected",
            "Total SCP packets inspected by the firewall",
            packets_inspected.clone(),
        );

        let threats_detected: Family<ThreatLabels, Counter> = Family::default();
        registry.register(
            "p2p_firewall_threats_detected",
            "Threats detected by the firewall, labelled by kind",
            threats_detected.clone(),
        );

        let bans_active = Counter::default();
        registry.register(
            "p2p_firewall_bans_active",
            "Cumulative number of IP bans issued by the firewall",
            bans_active.clone(),
        );

        let bans_expired = Counter::default();
        registry.register(
            "p2p_firewall_bans_expired",
            "Number of IP bans that have expired or been manually revoked",
            bans_expired.clone(),
        );

        let analysis_latency_ns: Gauge = Gauge::default();
        registry.register(
            "p2p_firewall_analysis_latency_ns",
            "Latency of the last single-packet analysis in nanoseconds",
            analysis_latency_ns.clone(),
        );

        Self {
            packets_inspected,
            threats_detected,
            bans_active,
            bans_expired,
            analysis_latency_ns,
            registry: Mutex::new(registry),
        }
    }

    /// Record a detected threat, incrementing the labelled counter.
    pub fn record_threat(&self, result: &AnalysisResult) {
        let kind = result.kind.to_string();
        self.threats_detected
            .get_or_create(&ThreatLabels { kind })
            .inc();
    }

    /// Encode all metrics into Prometheus text format.
    pub fn encode(&self) -> String {
        let mut buf = String::new();
        let registry = self.registry.lock().unwrap();
        encode(&mut buf, &registry).unwrap_or_else(|e| {
            error!("failed to encode metrics: {}", e);
        });
        buf
    }
}

impl Default for FirewallMetrics {
    fn default() -> Self {
        Self::new()
    }
}

// ─── HTTP metrics server ──────────────────────────────────────────────────────

/// Shared state for the metrics HTTP server.
#[derive(Clone)]
struct MetricsState {
    metrics: Arc<FirewallMetrics>,
}

/// Serve `GET /metrics` in Prometheus text format.
async fn metrics_handler(State(state): State<MetricsState>) -> String {
    state.metrics.encode()
}

/// Start the metrics HTTP server.
///
/// Spawns an Axum server on `addr` (e.g. `"0.0.0.0:9437"`).  Returns an error
/// if the address cannot be bound.
pub async fn start_metrics_server(
    addr: &str,
    metrics: Arc<FirewallMetrics>,
) -> Result<(), crate::error::FirewallError> {
    let state = MetricsState { metrics };
    let app = Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/health", get(|| async { "ok" }))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| crate::error::FirewallError::MetricsServer(e.to_string()))?;

    info!(addr = %addr, "p2p-firewall metrics server listening");

    axum::serve(listener, app)
        .await
        .map_err(|e| crate::error::FirewallError::MetricsServer(e.to_string()))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::{AnalysisResult, ThreatKind};
    use std::net::{IpAddr, Ipv4Addr};

    fn ip() -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))
    }

    #[test]
    fn test_metrics_encode_contains_prefix() {
        let m = FirewallMetrics::new();
        let encoded = m.encode();
        assert!(
            encoded.contains("p2p_firewall_packets_inspected_total"),
            "encoded metrics should contain the counter name"
        );
    }

    #[test]
    fn test_packets_inspected_increments() {
        let m = FirewallMetrics::new();
        m.packets_inspected.inc();
        m.packets_inspected.inc();
        let encoded = m.encode();
        assert!(
            encoded.contains("p2p_firewall_packets_inspected_total 2"),
            "counter should be 2"
        );
    }

    #[test]
    fn test_record_threat_flood() {
        let m = FirewallMetrics::new();
        let result = AnalysisResult {
            peer_ip: ip(),
            kind: ThreatKind::FloodAttack { pps: 9001 },
            should_ban: true,
        };
        m.record_threat(&result);
        let encoded = m.encode();
        assert!(
            encoded.contains("p2p_firewall_threats_detected_total"),
            "threat counter should be present"
        );
        assert!(
            encoded.contains("flood_attack"),
            "flood_attack label should appear"
        );
    }

    #[test]
    fn test_record_threat_malformed() {
        let m = FirewallMetrics::new();
        let result = AnalysisResult {
            peer_ip: ip(),
            kind: ThreatKind::MalformedTooSmall,
            should_ban: true,
        };
        m.record_threat(&result);
        let encoded = m.encode();
        assert!(encoded.contains("malformed_too_small"));
    }

    #[test]
    fn test_bans_active_counter() {
        let m = FirewallMetrics::new();
        m.bans_active.inc();
        m.bans_active.inc();
        m.bans_active.inc();
        let encoded = m.encode();
        assert!(encoded.contains("p2p_firewall_bans_active_total 3"));
    }

    #[test]
    fn test_bans_expired_counter() {
        let m = FirewallMetrics::new();
        m.bans_expired.inc();
        let encoded = m.encode();
        assert!(encoded.contains("p2p_firewall_bans_expired_total 1"));
    }

    #[test]
    fn test_analysis_latency_gauge() {
        let m = FirewallMetrics::new();
        m.analysis_latency_ns.set(42);
        let encoded = m.encode();
        assert!(encoded.contains("p2p_firewall_analysis_latency_ns 42"));
    }

    #[test]
    fn test_health_and_metrics_routes_compile() {
        // Ensure the router builds without panicking.
        let metrics = Arc::new(FirewallMetrics::new());
        let state = MetricsState { metrics };
        let _app = Router::new()
            .route("/metrics", get(metrics_handler))
            .route("/health", get(|| async { "ok" }))
            .with_state(state);
    }
}
