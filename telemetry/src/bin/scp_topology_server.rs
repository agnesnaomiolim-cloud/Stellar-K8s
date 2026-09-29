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

//! Standalone SCP Topology WebSocket server binary.
//!
//! Polls a local stellar-core HTTP endpoint and streams [`TopologyFrame`] JSON
//! to all connected WebSocket clients at `ws://<addr>/ws/scp-topology`.
//!
//! # Usage
//!
//! ```sh
//! scp-topology-server \
//!   --core-url http://localhost:11626 \
//!   --listen   0.0.0.0:8765 \
//!   --node-id  GABC...
//! ```

use std::time::Duration;

use stellar_telemetry::stream::scp_parser::{ScpPollerConfig, start_scp_poller};
use stellar_telemetry::ws_server::{TopologyWsState, serve_topology_ws};
use tokio::sync::broadcast;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

// ---------------------------------------------------------------------------
// CLI args (intentionally minimal – no clap dependency in this crate)
// ---------------------------------------------------------------------------

struct Args {
    core_url:  String,
    listen:    String,
    node_id:   String,
    poll_ms:   u64,
    capacity:  usize,
}

impl Args {
    fn from_env() -> Self {
        let mut core_url = "http://localhost:11626".to_owned();
        let mut listen   = "0.0.0.0:8765".to_owned();
        let mut node_id  = "LOCAL".to_owned();
        let mut poll_ms  = 2000u64;
        let mut capacity = 64usize;

        let mut args = std::env::args().skip(1);
        while let Some(key) = args.next() {
            match key.as_str() {
                "--core-url"  => { if let Some(v) = args.next() { core_url = v; } }
                "--listen"    => { if let Some(v) = args.next() { listen = v; } }
                "--node-id"   => { if let Some(v) = args.next() { node_id = v; } }
                "--poll-ms"   => { if let Some(v) = args.next() { poll_ms = v.parse().unwrap_or(poll_ms); } }
                "--capacity"  => { if let Some(v) = args.next() { capacity = v.parse().unwrap_or(capacity); } }
                "--help" | "-h" => {
                    eprintln!("Usage: scp-topology-server [--core-url URL] [--listen ADDR] [--node-id ID] [--poll-ms MS] [--capacity N]");
                    std::process::exit(0);
                }
                _ => {}
            }
        }

        Self { core_url, listen, node_id, poll_ms, capacity }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    // Initialise tracing with RUST_LOG fallback.
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let args = Args::from_env();

    let config = ScpPollerConfig {
        core_url:       args.core_url.clone(),
        poll_interval:  Duration::from_millis(args.poll_ms),
        local_node_id:  args.node_id.clone(),
    };

    tracing::info!(
        core_url  = %args.core_url,
        listen    = %args.listen,
        node_id   = %args.node_id,
        poll_ms   = args.poll_ms,
        "Starting SCP topology server"
    );

    // Start the poller – it returns a Receiver but we need the Sender for
    // TopologyWsState.  We create the channel manually so we keep both ends.
    let (tx, _initial_rx) = broadcast::channel(args.capacity);
    let tx_clone = tx.clone();

    // Spawn the poller with our sender.
    let poller_config = config.clone();
    tokio::spawn(async move {
        stellar_telemetry::stream::scp_parser::run_poller_with_sender(
            poller_config,
            tx_clone,
        ).await;
    });

    let state = TopologyWsState::from_sender(tx);
    serve_topology_ws(&args.listen, state).await;
}
