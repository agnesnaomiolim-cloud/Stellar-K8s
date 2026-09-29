//! Stellar Telemetry — PromQL Metrics Exporter for Soroban Gas Profiling
//.
// This crate provides an asynchronous log parser and Prometheus metrics
// exporter for monitoring Soroban smart contract CPU and memory consumption.
//
// # Quick start
//
// ```no_run
// use stellar_telemetry::exporter::run_exporter;
//
// #tokio::main]
// async fn main() -> Result<(), Box<dyn StdError>> {
//     run_exporter("0.0.0.0:9100").await
// }
// ```
//
// # Modules
//
// - [`calibration`] - Dynamic host function pricing calibrator that benchmarks
//   core Soroban host functions and exports calibrated pricing metrics.
// - [`parser`] — Zero-copy async log parser for Soroban RPC invocation streams.
// - [`exporter`] — Prometheus metrics exporter with an HTTP `/metrics` endpoint.

pub mod calibration;
pub mod exporter;
pub mod parser;
