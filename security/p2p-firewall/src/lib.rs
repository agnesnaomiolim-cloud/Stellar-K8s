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
//!
//! # p2p-firewall
//!
//! Real-time threat detection firewall for Stellar P2P gossip traffic on port 11625.
//!
//! ## Architecture
//!
//! ```text
//! TCP port 11625
//!       │
//!       ▼
//! ┌─────────────┐     raw bytes      ┌──────────────┐    verdict
//! │  interceptor│ ──────────────────▶│   analyzer   │──────────────┐
//! │  (ebpf mod) │                   └──────────────┘              │
//! └─────────────┘                                                  ▼
//!                                                        ┌──────────────────┐
//!                                                        │   ban_manager    │
//!                                                        │  (blacklist +    │
//!                                                        │   K8s NetPolicy) │
//!                                                        └──────────────────┘
//!                                                                  │
//!                                                                  ▼
//!                                                        ┌──────────────────┐
//!                                                        │    metrics       │
//!                                                        │  (Prometheus)    │
//!                                                        └──────────────────┘
//! ```
//!
//! ## Key modules
//!
//! | Module          | Purpose                                                     |
//! |-----------------|-------------------------------------------------------------|
//! | `analyzer`      | XDR packet parsing + heuristics (malformed, flood, handshake)|
//! | `interceptor`   | Userspace raw-socket / eBPF shim that feeds packets         |
//! | `ban_manager`   | In-memory blacklist, auto-expiry, K8s/iptables enforcement  |
//! | `metrics`       | Prometheus metrics registry and counters                    |
//! | `error`         | Crate-level error enum                                      |

pub mod analyzer;
pub mod ban_manager;
pub mod error;
pub mod interceptor;
pub mod metrics;

pub use analyzer::{AnalysisResult, PacketAnalyzer, ThreatKind};
pub use ban_manager::{BanEntry, BanManager, BanManagerConfig};
pub use error::FirewallError;
pub use interceptor::{InterceptorConfig, PacketInterceptor};
pub use metrics::FirewallMetrics;

use std::net::IpAddr;
use std::sync::Arc;

/// Top-level firewall configuration.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FirewallConfig {
    /// Network interface to monitor (e.g. `eth0`).
    pub interface: String,
    /// SCP gossip port to monitor (default 11625).
    pub scp_port: u16,
    /// Metrics HTTP endpoint (e.g. `0.0.0.0:9437`).
    pub metrics_addr: String,
    /// Ban manager configuration.
    pub ban: BanManagerConfig,
    /// Maximum packets per second before flood detection triggers.
    pub flood_pps_threshold: u64,
    /// How many rapid handshake failures trigger a ban.
    pub handshake_fail_threshold: u32,
    /// Window seconds for rate counters.
    pub rate_window_secs: u64,
}

impl Default for FirewallConfig {
    fn default() -> Self {
        Self {
            interface: "eth0".to_string(),
            scp_port: 11625,
            metrics_addr: "0.0.0.0:9437".to_string(),
            ban: BanManagerConfig::default(),
            flood_pps_threshold: 5000,
            handshake_fail_threshold: 10,
            rate_window_secs: 10,
        }
    }
}

/// Initialise and wire up all firewall components, returning handles that the
/// caller can await or store.
///
/// This function does not block; it spawns background Tokio tasks and returns
/// immediately.  Call [`FirewallHandle::shutdown`] to stop all tasks.
pub struct FirewallHandle {
    pub ban_manager: Arc<BanManager>,
    pub metrics: Arc<FirewallMetrics>,
    _shutdown_tx: tokio::sync::broadcast::Sender<()>,
}

impl FirewallHandle {
    /// Trigger graceful shutdown of all firewall background tasks.
    pub fn shutdown(&self) {
        let _ = self._shutdown_tx.send(());
    }

    /// Whether the given IP is currently banned.
    pub fn is_banned(&self, ip: IpAddr) -> bool {
        self.ban_manager.is_banned(ip)
    }
}

/// Build and start the firewall from a [`FirewallConfig`].
pub async fn start(config: FirewallConfig) -> Result<FirewallHandle, FirewallError> {
    let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);

    let metrics = Arc::new(FirewallMetrics::new());
    let ban_manager = Arc::new(BanManager::new(config.ban.clone(), Arc::clone(&metrics)));

    // Spawn the ban-expiry sweeper.
    let bm_clone = Arc::clone(&ban_manager);
    let mut rx = shutdown_tx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = tokio::time::sleep(tokio::time::Duration::from_secs(30)) => {
                    bm_clone.sweep_expired();
                }
                _ = rx.recv() => break,
            }
        }
    });

    // Spawn the interceptor → analyzer → ban pipeline.
    let interceptor_cfg = InterceptorConfig {
        interface: config.interface.clone(),
        port: config.scp_port,
    };
    let analyzer = Arc::new(PacketAnalyzer::new(
        config.flood_pps_threshold,
        config.handshake_fail_threshold,
        config.rate_window_secs,
    ));
    let bm_clone2 = Arc::clone(&ban_manager);
    let metrics_clone = Arc::clone(&metrics);
    let mut rx2 = shutdown_tx.subscribe();

    tokio::spawn(async move {
        let interceptor = PacketInterceptor::new(interceptor_cfg);
        let mut rx_pkts = interceptor.start();
        loop {
            tokio::select! {
                Some(pkt) = rx_pkts.recv() => {
                    metrics_clone.packets_inspected.inc();
                    if let Some(result) = analyzer.analyze(&pkt) {
                        metrics_clone.record_threat(&result);
                        if result.should_ban {
                            bm_clone2.ban(pkt.src_ip, result.kind.clone());
                        }
                    }
                }
                _ = rx2.recv() => break,
            }
        }
    });

    Ok(FirewallHandle {
        ban_manager,
        metrics,
        _shutdown_tx: shutdown_tx,
    })
}
