//! Stellar Telemetry — PromQL Metrics Exporter for Soroban Gas Profiling
//! and Envoy Traffic Heatmap streaming.
//!
//! This crate provides:
//!
//! * An asynchronous log parser and Prometheus metrics exporter for monitoring
//!   Soroban smart contract CPU and memory consumption.
//! * A real-time Envoy proxy stats streamer that feeds the WebGL traffic-routing
//!   heatmap displayed in the dashboard.
//!
//! # Quick start
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
//! # Modules
//!
//! - [`parser`]  — Zero-copy async log parser for Soroban RPC invocation streams.
//! - [`exporter`] — Prometheus metrics exporter with an HTTP `/metrics` endpoint.
//! - [`stream`]  — Real-time data-ingestion streams (Envoy stats, etc.) that
//!                 publish [`stream::envoy_stats::PodTrafficSnapshot`] frames
//!                 over broadcast channels for the dashboard WebSocket layer.

pub mod exporter;
pub mod parser;
pub mod stream;
