//! Stellar Telemetry — PromQL Metrics Exporter for Soroban Gas Profiling
///
/// This crate provides an asynchronous log parser and Prometheus metrics
/// exporter for monitoring Soroban smart contract CPU and memory consumption.
///
/// It also hosts the dynamic host function pricing calibrator, which benchmarks
/// core Soroban host functions on isolated CPU cores and exports calibrated
/// fee-pricing metrics for Grafana dashboards.
///
/// # Quick start
///
/// ```no_run
/// use stellar_telemetry::exporter::run_exporter;
///
/// #tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     run_exporter("0.0.0.0:9100").await
/// }
/// ```
///
/// # Modules
///
/// - [`parser`] — Zero-copy async log parser for Soroban RPC invocation streams.
/// - [`exporter`] — Prometheus metrics exporter with an HTTP `/metrics` endpoint.
/// - [`calibration`] — Dynamic host function pricing calibrator and exporter.

pub mod calibration;
pub mod exporter;
pub mod parser;
