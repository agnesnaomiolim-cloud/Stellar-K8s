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

//! Stellar Telemetry — PromQL Metrics Exporter and SCP Topology Dashboard
//!
//! This crate provides:
//! * An asynchronous log parser and Prometheus metrics exporter for monitoring
//!   Soroban smart contract CPU and memory consumption.
//! * A live SCP peering stream parser that polls stellar-core HTTP endpoints
//!   and fans out topology frames over a WebSocket broadcast channel.
//! * An axum WebSocket server exposing `/ws/scp-topology` for the Three.js
//!   WebGL dashboard.
//!
//! # Quick start – Prometheus exporter
//!
//! ```no_run
//! use stellar_telemetry::exporter::run_exporter;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     run_exporter("0.0.0.0:9100").await
//! }
//! ```
//!
//! # Quick start – SCP topology dashboard
//!
//! ```no_run
//! use stellar_telemetry::stream::scp_parser::{start_scp_poller, ScpPollerConfig};
//! use stellar_telemetry::ws_server::{TopologyWsState, serve_topology_ws};
//!
//! #[tokio::main]
//! async fn main() {
//!     let rx = start_scp_poller(ScpPollerConfig::default(), 64).await;
//!     // Extract sender by subscribing first; real usage passes the sender directly.
//!     let (tx, _rx) = tokio::sync::broadcast::channel(64);
//!     let state = TopologyWsState::from_sender(tx);
//!     serve_topology_ws("0.0.0.0:8765", state).await;
//! }
//! ```
//!
//! # Modules
//!
//! - [`parser`]     — Zero-copy async log parser for Soroban RPC invocation streams.
//! - [`exporter`]   — Prometheus metrics exporter with an HTTP `/metrics` endpoint.
//! - [`stream`]     — Live data stream adapters (SCP topology poller).
//! - [`ws_server`]  — axum WebSocket server for the topology dashboard.

pub mod exporter;
pub mod gas_exporter;
pub mod parser;
pub mod scp;
pub mod stream;
pub mod ws_server;
