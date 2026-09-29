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

//! WebSocket server exposing the `/ws/scp-topology` endpoint.
//!
//! The server accepts WebSocket upgrade requests and then fans out
//! [`TopologyFrame`] JSON messages from the SCP poller broadcast channel to
//! every connected client.
//!
//! # Usage
//!
//! ```no_run
//! use stellar_telemetry::ws_server::{TopologyWsState, serve_topology_ws};
//! use tokio::sync::broadcast;
//!
//! #[tokio::main]
//! async fn main() {
//!     let (tx, _rx) = broadcast::channel(64);
//!     let state = TopologyWsState::from_sender(tx);
//!     serve_topology_ws("0.0.0.0:8765", state).await;
//! }
//! ```

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::Method;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::broadcast;
use tokio::time::timeout;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, info, warn};

use crate::stream::scp_parser::TopologyFrame;

// ---------------------------------------------------------------------------
// Shared application state
// ---------------------------------------------------------------------------

/// Clone-able application state threaded through axum via [`State`].
#[derive(Clone)]
pub struct TopologyWsState {
    /// Subscriber end of the broadcast channel fed by the SCP poller.
    ///
    /// Each call to `subscribe()` returns a new independent receiver so that
    /// every WebSocket connection gets its own copy of the frames.
    sender: Arc<broadcast::Sender<TopologyFrame>>,
}

impl TopologyWsState {
    /// Create state from an existing broadcast *receiver* (the sender is
    /// reconstructed by cloning the channel internals).
    ///
    /// # Note
    /// The receiver passed here is discarded immediately; it is only used to
    /// extract the `Sender` handle.
    pub fn from_sender(sender: broadcast::Sender<TopologyFrame>) -> Self {
        Self {
            sender: Arc::new(sender),
        }
    }

    /// Subscribe to the frame stream.
    pub fn subscribe(&self) -> broadcast::Receiver<TopologyFrame> {
        self.sender.subscribe()
    }
}

// ---------------------------------------------------------------------------
// Axum router
// ---------------------------------------------------------------------------

/// Build and return an axum [`Router`] containing the `/ws/scp-topology` route.
///
/// Attach this to your top-level router with `Router::merge`.
pub fn topology_router(state: TopologyWsState) -> Router {
    let cors = CorsLayer::new()
        .allow_methods([Method::GET])
        .allow_origin(Any);

    Router::new()
        .route("/ws/scp-topology", get(ws_upgrade_handler))
        .layer(cors)
        .with_state(state)
}

/// Stand-alone server – useful for running the dashboard server independently
/// of the main operator binary.
pub async fn serve_topology_ws(addr: &str, state: TopologyWsState) {
    let app = topology_router(state)
        .route("/healthz", get(|| async { "ok" }));

    info!(addr, "SCP topology WebSocket server starting");

    let listener = TcpListener::bind(addr)
        .await
        .expect("bind failed");

    axum::serve(listener, app)
        .await
        .expect("server error");
}

// ---------------------------------------------------------------------------
// WebSocket upgrade handler
// ---------------------------------------------------------------------------

/// Axum handler: upgrades an HTTP request to a WebSocket connection.
async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    State(state): State<TopologyWsState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_ws_client(socket, state))
}

/// Per-connection coroutine.  Receives frames from the broadcast channel and
/// forwards them as JSON text messages to the client.
async fn handle_ws_client(mut socket: WebSocket, state: TopologyWsState) {
    let mut rx = state.subscribe();
    let mut ping_interval = tokio::time::interval(Duration::from_secs(20));

    info!("WebSocket client connected");

    loop {
        tokio::select! {
            // New topology frame available – send to client.
            result = rx.recv() => {
                match result {
                    Ok(frame) => {
                        match serde_json::to_string(&frame) {
                            Ok(json) => {
                                if let Err(e) = socket.send(Message::Text(json.into())).await {
                                    debug!("client disconnected: {e}");
                                    break;
                                }
                            }
                            Err(e) => error!("frame serialisation error: {e}"),
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("WebSocket client lagged {n} frames – resuming from latest");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        info!("broadcast channel closed – shutting down WebSocket");
                        break;
                    }
                }
            }

            // Periodic ping to keep the connection alive through proxies.
            _ = ping_interval.tick() => {
                if let Err(e) = socket.send(Message::Ping(vec![].into())).await {
                    debug!("ping failed – client disconnected: {e}");
                    break;
                }
            }

            // Incoming message from client (e.g., pong or close).
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Close(_))) | None => {
                        info!("WebSocket client sent close frame");
                        break;
                    }
                    Some(Ok(Message::Pong(_))) => {
                        debug!("pong received");
                    }
                    Some(Err(e)) => {
                        warn!("WebSocket recv error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
        }
    }

    info!("WebSocket client disconnected");
}

// ---------------------------------------------------------------------------
// Integration tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stream::scp_parser::{
        ConsensusPhase, EdgeDirection, NodeHealth, PeerEdge, TopologyFrame, ValidatorNode,
    };

    fn make_frame(seq: u64) -> TopologyFrame {
        TopologyFrame {
            seq,
            timestamp_ms: 1_700_000_000_000,
            nodes: vec![ValidatorNode {
                id: "GABC".to_owned(),
                label: "local".to_owned(),
                address: "127.0.0.1:11626".to_owned(),
                phase: ConsensusPhase::Externalize,
                health: NodeHealth::Synced,
                peer_count: 2,
                ledger_seq: seq * 100,
                updated_at_ms: 1_700_000_000_000,
            }],
            edges: vec![PeerEdge {
                source: "GABC".to_owned(),
                target: "GDEF".to_owned(),
                direction: EdgeDirection::Outbound,
                latency_ms: None,
            }],
            local_ledger: seq * 100,
            partition_detected: false,
        }
    }

    #[test]
    fn topology_frame_roundtrips_json() {
        let frame = make_frame(1);
        let json  = serde_json::to_string(&frame).unwrap();
        let back: TopologyFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seq, 1);
        assert_eq!(back.nodes.len(), 1);
        assert_eq!(back.edges.len(), 1);
    }

    #[tokio::test]
    async fn broadcast_delivers_frame_to_subscriber() {
        let (tx, mut rx) = broadcast::channel::<TopologyFrame>(4);
        let state = TopologyWsState::from_sender(tx.clone());
        let mut sub = state.subscribe();

        // Publish a frame.
        let frame = make_frame(42);
        tx.send(frame.clone()).unwrap();

        // Both the original receiver and the new subscriber get it.
        let received = timeout(Duration::from_millis(100), sub.recv())
            .await
            .expect("timeout")
            .expect("recv error");
        assert_eq!(received.seq, 42);

        // Original rx also receives it.
        let received2 = timeout(Duration::from_millis(100), rx.recv())
            .await
            .expect("timeout")
            .expect("recv error 2");
        assert_eq!(received2.seq, 42);
    }
}
