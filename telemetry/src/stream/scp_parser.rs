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

//! SCP Peering Stream Parser
//!
//! Polls the stellar-core HTTP endpoint (`/peers` and `/scp`) to build a live
//! snapshot of the validator topology.  The snapshot is serialised to a
//! [`TopologyFrame`] and pushed into a broadcast channel so the WebSocket
//! server can fan it out to all connected dashboard clients.
//!
//! ## Wire format consumed
//!
//! The stellar-core `/peers` endpoint returns JSON like:
//! ```json
//! {
//!   "authenticated_peers": {
//!     "inbound": [ { "id": "GXXX", "address": "1.2.3.4:11625", ... } ],
//!     "outbound": [ { "id": "GYYY", "address": "5.6.7.8:11625", ... } ]
//!   }
//! }
//! ```
//!
//! The `/scp` endpoint returns JSON like:
//! ```json
//! {
//!   "ledger": 42,
//!   "slots": {
//!     "0": { "phase": "EXTERNALIZE", "value": "..." }
//!   }
//! }
//! ```
//!
//! These two are joined into a single [`TopologyFrame`] containing
//! [`ValidatorNode`] and [`PeerEdge`] arrays suitable for direct consumption
//! by the Three.js topology renderer.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Consensus phase reported by stellar-core SCP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ConsensusPhase {
    Prepare,
    Confirm,
    Externalize,
    /// Node is not participating in the current slot.
    Offline,
}

impl Default for ConsensusPhase {
    fn default() -> Self {
        ConsensusPhase::Offline
    }
}

/// Node health derived from consensus state and peer connectivity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeHealth {
    /// Fully synced and externalising ledgers.
    Synced,
    /// Participating in SCP but not yet externalising.
    Syncing,
    /// Reachable but not in consensus.
    Degraded,
    /// No peering data received.
    Partitioned,
}

impl Default for NodeHealth {
    fn default() -> Self {
        NodeHealth::Partitioned
    }
}

/// Represents a single validator in the topology graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidatorNode {
    /// Stellar public key (G…).
    pub id: String,
    /// Human-readable label (may be equal to `id` when no alias is known).
    pub label: String,
    /// IP/hostname reported by the peer.
    pub address: String,
    /// Current consensus phase.
    pub phase: ConsensusPhase,
    /// Derived node health for visualisation colouring.
    pub health: NodeHealth,
    /// Peer count (inbound + outbound).
    pub peer_count: u32,
    /// Latest ledger sequence this node externalised (`0` if unknown).
    pub ledger_seq: u64,
    /// Unix timestamp (ms) of last update.
    pub updated_at_ms: u64,
}

/// A directed peering edge between two validators.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerEdge {
    pub source: String,
    pub target: String,
    /// Whether this edge represents an inbound or outbound connection from
    /// the perspective of the local node.
    pub direction: EdgeDirection,
    /// Latency hint in milliseconds (`None` when not available).
    pub latency_ms: Option<u32>,
}

/// Direction of a peering link from the local node's perspective.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeDirection {
    Inbound,
    Outbound,
}

/// A complete topology snapshot delivered to WebSocket clients on every tick.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopologyFrame {
    /// Monotonically-increasing sequence number (useful for idempotent
    /// processing in the browser).
    pub seq: u64,
    /// Unix timestamp (ms) when this frame was generated.
    pub timestamp_ms: u64,
    /// All known validators (local + peers).
    pub nodes: Vec<ValidatorNode>,
    /// All peering edges.
    pub edges: Vec<PeerEdge>,
    /// Latest ledger known to the local node.
    pub local_ledger: u64,
    /// `true` when the local node detects a possible network partition
    /// (i.e. SCP slots have stalled).
    pub partition_detected: bool,
}

// ---------------------------------------------------------------------------
// Internal stellar-core JSON shapes
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct PeerEntry {
    id: Option<String>,
    address: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AuthenticatedPeers {
    inbound: Option<Vec<PeerEntry>>,
    outbound: Option<Vec<PeerEntry>>,
}

#[derive(Debug, Deserialize)]
struct PeersResponse {
    authenticated_peers: Option<AuthenticatedPeers>,
}

#[derive(Debug, Deserialize)]
struct SlotInfo {
    phase: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ScpResponse {
    ledger: Option<u64>,
    slots: Option<HashMap<String, SlotInfo>>,
}

// ---------------------------------------------------------------------------
// Parser / poller
// ---------------------------------------------------------------------------

/// Configuration for the SCP topology poller.
#[derive(Debug, Clone)]
pub struct ScpPollerConfig {
    /// Base URL of the stellar-core HTTP endpoint, e.g. `http://localhost:11626`.
    pub core_url: String,
    /// How often to poll stellar-core for fresh data.
    pub poll_interval: Duration,
    /// Public key of the *local* node (used as the root node in the graph).
    pub local_node_id: String,
}

impl Default for ScpPollerConfig {
    fn default() -> Self {
        Self {
            core_url: "http://localhost:11626".to_owned(),
            poll_interval: Duration::from_secs(2),
            local_node_id: "LOCAL".to_owned(),
        }
    }
}

/// Long-running task that polls stellar-core and publishes [`TopologyFrame`]s.
///
/// Returns a [`broadcast::Receiver`] that callers can subscribe to.  The
/// task runs until the process exits.
pub async fn start_scp_poller(
    config: ScpPollerConfig,
    capacity: usize,
) -> broadcast::Receiver<TopologyFrame> {
    let (tx, rx) = broadcast::channel::<TopologyFrame>(capacity);
    tokio::spawn(async move {
        run_poller(config, tx).await;
    });
    rx
}

/// Public entry point for the poller when the caller owns the [`broadcast::Sender`].
///
/// Useful when you need to share the sender with other components (e.g. the
/// standalone binary which also creates a [`TopologyWsState`] from the same sender).
pub async fn run_poller_with_sender(config: ScpPollerConfig, tx: broadcast::Sender<TopologyFrame>) {
    run_poller(config, tx).await;
}

async fn run_poller(config: ScpPollerConfig, tx: broadcast::Sender<TopologyFrame>) {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("failed to build reqwest client");

    let mut seq: u64 = 0;
    let mut last_ledger: u64 = 0;
    let mut stall_ticks: u32 = 0;

    info!(
        core_url = %config.core_url,
        interval_ms = config.poll_interval.as_millis(),
        "SCP topology poller starting"
    );

    loop {
        match poll_once(&client, &config, seq, last_ledger, stall_ticks).await {
            Ok(frame) => {
                // Detect stalls: if ledger has not advanced for >5 polls assume partition.
                if frame.local_ledger > last_ledger {
                    last_ledger = frame.local_ledger;
                    stall_ticks = 0;
                } else {
                    stall_ticks = stall_ticks.saturating_add(1);
                }

                let receiver_count = tx.receiver_count();
                if receiver_count > 0 {
                    if let Err(e) = tx.send(frame) {
                        warn!("broadcast send failed (no active receivers?): {e}");
                    }
                }

                seq = seq.wrapping_add(1);
                debug!(seq, ledger = last_ledger, stall_ticks, "topology frame emitted");
            }
            Err(e) => {
                error!("SCP poll error: {e}");
            }
        }

        sleep(config.poll_interval).await;
    }
}

async fn poll_once(
    client: &reqwest::Client,
    config: &ScpPollerConfig,
    seq: u64,
    last_ledger: u64,
    stall_ticks: u32,
) -> Result<TopologyFrame, Box<dyn std::error::Error + Send + Sync>> {
    let peers_url = format!("{}/peers", config.core_url);
    let scp_url = format!("{}/scp", config.core_url);

    // Fetch both endpoints concurrently.
    let (peers_result, scp_result) = tokio::join!(
        client.get(&peers_url).send(),
        client.get(&scp_url).send(),
    );

    // Parse peers.
    let peers_resp: PeersResponse = peers_result?.json().await.unwrap_or(PeersResponse {
        authenticated_peers: None,
    });

    // Parse SCP state.
    let scp_resp: ScpResponse = scp_result?.json().await.unwrap_or(ScpResponse {
        ledger: None,
        slots: None,
    });

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;

    let local_ledger = scp_resp.ledger.unwrap_or(last_ledger);

    // Derive local node phase from the latest SCP slot.
    let local_phase = scp_resp
        .slots
        .as_ref()
        .and_then(|slots| {
            // The latest slot key is numerically largest.
            slots.values().last()
        })
        .and_then(|slot| slot.phase.as_deref())
        .map(phase_from_str)
        .unwrap_or(ConsensusPhase::Offline);

    let local_health = health_from_phase(&local_phase, stall_ticks);

    // Build edges and collect unique peer IDs.
    let auth = peers_resp.authenticated_peers.as_ref();
    let inbound = auth
        .and_then(|a| a.inbound.as_deref())
        .unwrap_or_default();
    let outbound = auth
        .and_then(|a| a.outbound.as_deref())
        .unwrap_or_default();

    let mut edges: Vec<PeerEdge> = Vec::new();
    let mut peer_ids: HashSet<String> = HashSet::new();
    let mut peer_addresses: HashMap<String, String> = HashMap::new();

    for peer in inbound {
        let id = peer.id.clone().unwrap_or_else(|| "unknown".to_owned());
        let addr = peer.address.clone().unwrap_or_default();
        edges.push(PeerEdge {
            source: id.clone(),
            target: config.local_node_id.clone(),
            direction: EdgeDirection::Inbound,
            latency_ms: None,
        });
        peer_ids.insert(id.clone());
        peer_addresses.insert(id, addr);
    }

    for peer in outbound {
        let id = peer.id.clone().unwrap_or_else(|| "unknown".to_owned());
        let addr = peer.address.clone().unwrap_or_default();
        edges.push(PeerEdge {
            source: config.local_node_id.clone(),
            target: id.clone(),
            direction: EdgeDirection::Outbound,
            latency_ms: None,
        });
        peer_ids.insert(id.clone());
        peer_addresses.insert(id, addr);
    }

    // Build node list: local node + all peers.
    let mut nodes: Vec<ValidatorNode> = Vec::with_capacity(peer_ids.len() + 1);

    // Local node first.
    nodes.push(ValidatorNode {
        id: config.local_node_id.clone(),
        label: "local".to_owned(),
        address: "localhost:11626".to_owned(),
        phase: local_phase,
        health: local_health,
        peer_count: (inbound.len() + outbound.len()) as u32,
        ledger_seq: local_ledger,
        updated_at_ms: now_ms,
    });

    // Peer nodes (we only know their ID and address; assume Synced if reachable).
    for id in &peer_ids {
        let addr = peer_addresses.get(id).cloned().unwrap_or_default();
        nodes.push(ValidatorNode {
            id: id.clone(),
            label: short_label(id),
            address: addr,
            phase: ConsensusPhase::Externalize,
            health: NodeHealth::Synced,
            peer_count: 0, // unknown from this vantage point
            ledger_seq: local_ledger, // optimistic assumption
            updated_at_ms: now_ms,
        });
    }

    let partition_detected = stall_ticks >= 5;

    Ok(TopologyFrame {
        seq,
        timestamp_ms: now_ms,
        nodes,
        edges,
        local_ledger,
        partition_detected,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn phase_from_str(s: &str) -> ConsensusPhase {
    match s.to_uppercase().as_str() {
        "PREPARE" => ConsensusPhase::Prepare,
        "CONFIRM" => ConsensusPhase::Confirm,
        "EXTERNALIZE" => ConsensusPhase::Externalize,
        _ => ConsensusPhase::Offline,
    }
}

fn health_from_phase(phase: &ConsensusPhase, stall_ticks: u32) -> NodeHealth {
    if stall_ticks >= 5 {
        return NodeHealth::Partitioned;
    }
    match phase {
        ConsensusPhase::Externalize => NodeHealth::Synced,
        ConsensusPhase::Confirm | ConsensusPhase::Prepare => NodeHealth::Syncing,
        ConsensusPhase::Offline => NodeHealth::Degraded,
    }
}

/// Return the first 8 chars of a public key as a human-readable label.
fn short_label(key: &str) -> String {
    key.chars().take(8).collect()
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_from_str_roundtrip() {
        assert_eq!(phase_from_str("EXTERNALIZE"), ConsensusPhase::Externalize);
        assert_eq!(phase_from_str("prepare"), ConsensusPhase::Prepare);
        assert_eq!(phase_from_str("CONFIRM"), ConsensusPhase::Confirm);
        assert_eq!(phase_from_str("unknown"), ConsensusPhase::Offline);
    }

    #[test]
    fn health_derives_correctly() {
        assert_eq!(
            health_from_phase(&ConsensusPhase::Externalize, 0),
            NodeHealth::Synced
        );
        assert_eq!(
            health_from_phase(&ConsensusPhase::Externalize, 5),
            NodeHealth::Partitioned
        );
        assert_eq!(
            health_from_phase(&ConsensusPhase::Prepare, 2),
            NodeHealth::Syncing
        );
        assert_eq!(
            health_from_phase(&ConsensusPhase::Offline, 0),
            NodeHealth::Degraded
        );
    }

    #[test]
    fn short_label_truncates() {
        assert_eq!(short_label("GABCDEFGHIJK"), "GABCDEFG");
        assert_eq!(short_label("GA"), "GA");
    }

    #[test]
    fn topology_frame_serialises_to_json() {
        let frame = TopologyFrame {
            seq: 1,
            timestamp_ms: 1_700_000_000_000,
            nodes: vec![ValidatorNode {
                id: "GABC".to_owned(),
                label: "GABC".to_owned(),
                address: "127.0.0.1:11626".to_owned(),
                phase: ConsensusPhase::Externalize,
                health: NodeHealth::Synced,
                peer_count: 3,
                ledger_seq: 100,
                updated_at_ms: 1_700_000_000_000,
            }],
            edges: vec![PeerEdge {
                source: "GABC".to_owned(),
                target: "GDEF".to_owned(),
                direction: EdgeDirection::Outbound,
                latency_ms: Some(12),
            }],
            local_ledger: 100,
            partition_detected: false,
        };

        let json = serde_json::to_string(&frame).expect("serialise");
        assert!(json.contains("\"partition_detected\":false"));
        assert!(json.contains("EXTERNALIZE"));
    }
}
