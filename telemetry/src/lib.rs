//! Stellar Telemetry — PromQL Metrics Exporter, Heap Profiling & Memory Leak
//! Detection for Soroban Gas Profiling (issue #305).
//!
//! This crate provides:
//! * An asynchronous log parser and Prometheus metrics exporter for monitoring
//!   Soroban smart contract CPU and memory consumption.
//! * A jemalloc-backed heap profiling subsystem with a `/debug/pprof/heap`
//!   HTTP endpoint and an in-process memory-leak detector.
//!
//! # Quick start — metrics exporter
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
//! # Quick start — debug profiling server (issue #305)
//!
//! ```no_run
//! use stellar_telemetry::server::debug::{run_debug_server, DebugServerConfig};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let config = DebugServerConfig {
//!         // SHA-256 of the token stored in your K8s Secret.
//!         token_sha256: std::env::var("PROFILING_TOKEN_SHA256").unwrap_or_default(),
//!         ..Default::default()  // binds to 127.0.0.1:6060
//!     };
//!     run_debug_server(config).await
//! }
//! ```
//!
//! # Quick start — background memory-leak detector (issue #305)
//!
//! ```no_run
//! use stellar_telemetry::profiling::{MemoryLeakDetector, LeakDetectorConfig};
//!
//! #[tokio::main]
//! async fn main() {
//!     let detector = MemoryLeakDetector::new(LeakDetectorConfig::default());
//!     // Fires a tracing::warn! when RSS grows > 20 % over 24 hours.
//!     tokio::spawn(async move { detector.run().await });
//! }
//! ```
//!
//! # Modules
//!
//! | Module | Purpose |
//! |--------|---------|
//! | [`parser`]   | Zero-copy async log parser for Soroban RPC invocation streams |
//! | [`exporter`] | Prometheus metrics exporter with `/metrics` HTTP endpoint |
//! | [`profiling`] | jemalloc allocator, heap-dump helpers, MemoryLeakDetector |
//! | [`server`]   | Axum-based debug HTTP server (`/debug/pprof/heap`, etc.) |

pub mod exporter;
pub mod parser;
pub mod profiling;
pub mod server;
