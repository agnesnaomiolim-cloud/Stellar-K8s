/// Dynamic Host Function Pricing Calibrator
///
/// This module implements a sidecar that continuously benchmarks the
/// execution time of core Soroban host functions and exports calibrated
/// pricing metrics via Prometheus for Grafana consumption.
///
/// The calibrator is designed to run on isolated CPU cores so that it
/// never interferes with live Byzantine Fault Tolerant consensus execution.
///
/// # Example
///
/// ```no_run
/// use stellar_telemetry::calibration::exporter::run_calibrator;
+// use stellar_telemetry::calibration::host_functions::CalibrationConfig;
+//
/// #[tokio::main]
+/// async fn main() -> Result<, Box<dyn StdError>> {
+///     let config = CalibrationConfig::default();
+///     run_calibrator("0.0.0.0:9103", config).await
+/// }
+/// ```

pub mod host_functions;
pub mod exporter;
