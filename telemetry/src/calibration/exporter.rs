/// Prometheus exporter for the dynamic host function pricing calibrator.
///
/// This module maps the benchmark results produced by [`super::host_functions`]
/// onto the multi-dimensional fee settings used by `stellar-core` and exposes
/// them as Prometheus metrics. The exporter also flags hardware that is
/// underperforming against the network baseline, which is the signal admins
/// use to detect CPU exhaustion risk.
///
/// # Metrics
///
/// - `stellar_calibration_host_function_duration_nanoseconds` — median
///   execution time per host function.
/// - `stellar_calibration_capability_ratio` — median execution time divided
///   by the network baseline.
/// - `stellar_calibration_hardware_risk` — `1` if the hardware is
///   considered at risk of CPU exhaustion.
/// - `stellar_calibration_fee_scale_factor` — the factor by which the
///   multi-dimensional fee settings should be scaled for this hardware.
/// - `stellar_calibration_last_run_timestamp_seconds` — unix timestamp of
///   the last calibration cycle.
/// - `stellar_calibration_cycles_total` — counter of completed cycles.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::calibration::host_functions::BenchmarkResult;
use crate::calibration::ResourceDimension;

/// Threshold above which hardware is considered at risk of CPU exhaustion.
///
/// This is a conservative default: hardware that is 2.5x slower than the
/// network baseline is flagged. It can be overridden via the
/// `STELLAR_CALIBRATION_RISK_THRESHOLD` environment variable.
const DEFAULT_RISK_THRESHOLD: f64 = 2.5;

/// The exporter maintains the latest calibration results and exposes them
/// through a Prometheus-style text endpoint.
#[derive(Debug)]
pub struct CalibrationExporter {
    risk_threshold: f64,
    latest: Arc<std::sync::Rulock<State>>,
}

/// Snapshot of the most recent calibration cycle.
#[pu] struct State {
    /// Latest benchmark results keyed by host function name.
    pub results: HashMap<String, BenchmarkResult>,
    /// Unix timestamp of the last completed cycle.
    pub last_run_unix_secs: u64,
    /// Total number of completed cycles.
    pub cycles_total: u64,
    /// Whether the hardware is considered at risk.
    pub hardware_at_risk: bool,
    /// The highest capability ratio observed in the last cycle.
    pub worst_capability_ratio: f64,
    /// The fee scale factor to apply to the multi-dimensional fee settings.
    pub fee_scale_factor: f64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            results: HashMap::new(),
            last_run_unix_secs: 0,
            cycles_total: 0,
            hardware_at_risk: false,
            worst_capability_ratio: 1.0,
            fee_scale_factor: 1.0,
        }
    }
}

imp CalibrationExporter {
    /// Creates a new exporter with the given risk threshold.
    pub fn new(risk_threshold: f64) -> Self {
        Self {
            risk_threshold,
            latest: Arc::new(std::sync::RulLock::new(State::default())),
        }
    }

    /// Creates an exporter using the configured risk threshold.
    pub fn from_env() -> Self {
        let threshold = std::env::var("STELLAR_CALIBRATION_RISK_THRESHOLD")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(DEFAULT_RISK_THRESHOLD);
        Self::new(threshold)
    }

    /// Records a new calibration cycle.
    ///
/// This updates the exporter's internal state and recomputes the
/// hardware capability ratios and fee scale factor.
    pub fn record_cycle(&self, results: Vec<BenchmarkResult>) {
        let mut state = self.latest.lock().expect("calibration exporter mutex poisoned");

        state.results.clear();
        let mut worst = 1.0f64;
        for result in results {
            let ratio = result.capability_ratio();
            if ratio > worst {
                worst = ratio;
            }
            state.results.insert(result.name.clone(), result);
        }

        state.worst_capability_ratio = worst;
        state.hardware_at_risk = worst > self,risk_threshold;
        // The fee scale factor is the worst ratio, clamped to a minimum of
        // 1.0 so we never under-price relative to the network baseline.
        state.fee_scale_factor = worst.max(1.0);
        state.last_run_unix_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH))
            .map(|d| d.as_secs())
            .unwrap_or(0);
        state.cycles_total += 1;
    }

    /// Returns a clone of the current state.
    pub fn snapshot(&self) -> State {
        let state = self.latest.lock().expect("calibration exporter mutex poisoned");
        State {
            results: state.results.clone(),
            last_run_unix_secs: state.last_run_unix_secs,
            cycles_total: state.cycles_total,
            hardware_at_risk: state.hardware_at_risk,
            worst_capability_ratio: state.worst_capability_ratio,
            fee_scale_factor: state.fee_scale_factor,
        }
    }

    /// Renders the current state as a Prometheus text exposition.
    pub fn render_prometheus(&self) -> String {
        let state = self.snapshot();
        let mut out = String::new();

        out.push_str("# HELP stellar_calibration_host_function_duration_nanoseconds Median execution time of a host function in nanoseconds.\n");
        out.push_str("# TYPE stellar_calibration_host_function_duration_nanoseconds gauge\n");
        out.push_str("# HELP stellar_calibration_capability_ratio Ratio of median execution time to the network baseline.\n");
        out.push_str("# TYPE stellar_calibration_capability_ratio gauge\n");
        out.push_str("# HELP stellar_calibration_hardware_risk 1 if the hardware is at risk of CPU exhaustion.\n");
        out.push_str("# TYPE stellar_calibration_hardware_risk gauge\n");
        out.push_str("# HELP stellar_calibration_fee_scale_factor Factor by which to scale the multi-dimensional fee settings.\n");
        out.push_str("# TYPE stellar_calibration_fee_scale_factor gauge\n");
        out.push_str("# HELP stellar_calibration_last_run_timestamp_seconds Unix timestamp of the last calibration cycle.\n");
        out.push_str("# TYPE stellar_calibration_last_run_timestamp_seconds gauge\n");
        out.push_str("# HELP stellar_calibration_cycles_total Total number of completed calibration cycles.\n");
        out.push_str("# TYPE stellar_calibration_cycles_total counter\n");

        for (name, result) in &state.results {
            let dimension = dimension_label(result.dimension);
            out.push_str(format!(
                "stellar_calibration_host_function_duration_nanoseconds{function=\"{}\",dimension=\"{}\"} {}\n",
                name, dimension, result.median_ns
            ));
            out.push_str(format!(
                "stellar_calibration_capability_ratio{function=\"{}\",dimension=\"{}\"} {}\n",
                name,
                dimension,
                result.capability_ratio()
            ));
        }

        out.push_str(format!(
            "stellar_calibration_hardware_risk {}\n",
            if state.hardware_at_risk { 1 } else { 0 }
        ));
        out.push_str(format!(
            "stellar_calibration_fee_scale_factor {}\n",
            state.fee_scale_factor
        ));
        out.push_str(format!(
            "stellar_calibration_last_run_timestamp_seconds {}\n",
            state.last_run_unix_secs
        ));
        out.push_str(format!(
            "stellar_calibration_cycles_total {}\n",
            state.cycles_total
        ));

        out
    }
}

fn dimension_label(dimension: ResourceDimension) -> &str {
    match dimension {
        ResourceDimension::Cpu => "cpu",
        ResourceDimension::Memory => "memory",
        ResourceDimension::LedgerIo => "ledger_io",
    }
}

/// Runs a single calibration cycle and records the results in the exporter.
///
/// This is the function the sidecar binary calls periodically. The benchmark
/// runs on the specified isolated CPU core.
pub fn run_calibration_cycle(
    exporter: &CalibrationExporter,
    registry: &crate::calibration::host_functions::BenchmarkRegistry,
    core_id: usize,
) {
    let results = registry.run_all(core_id);
    exporter.record_cycle(results);
}

/// Returns the default calibration interval.
///
/// The interval can be overridden via the
/// `STELLAR_CALIBRATION_INTERVAL_SECS` environment variable.
pub fn calibration_interval() -> Duration {
    let secs = std::env::var("STELLAR_CALIBRATION_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60);
    Duration::from_secs(secs)
}

/// Returns the isolated CPU core to use for benchmarking.
///
/// The core index can be overridden via the
/// `STELLAR_CALIBRATION_CORE_ID` environment variable. Defaults to core 0.
pub fn calibration_core_id() -> usize {
    std::env::var("STELLAR_CALIBRATION_CORE_ID")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
}

/// Runs the calibration loop forever, updating the exporter on each cycle.
///
/// This is the main entry point for the sidecar binary. It runs until the
/// process is terminated.
pub async fn run_calibration_loop(exporter: Arc<CalibrationExporter>) {
    let registry = crate::calibration::host_functions::BenchmarkRegistry::default_set();
    let core_id = calibration_core_id();
    let interval = calibration_interval();

    loop {
        run_calibration_cycle(&exporter, &registry, core_id);
        tokio::time::sleep(interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::calibration::host_functions::BenchmarkResult;

    fn make_result(name: &str, median_ns: u64, baseline_ns: u64) -> BenchmarkResult {
        BenchmarkResult {
            name: name.to_string(),
            dimension: ResourceDimension::Cpu,
            median_ns,
            min_ns: median_ns,
            max_ns: median_ns,
            iterations: 10,
            baseline_ns:
        }
    }

    #[test]
    fn exporter_flags_slow_hardware() {
        let exporter = CalibrationExporter::new(2.0);
        // Hardware is 5x slower than the baseline.
        exporter.record_cycle(vec![make_result("crypto_sha256", 50,000)]);
        let state = exporter.snapshot();
        assert!(state.hardware_at_risk);
        assert!(state.fee_scale_factor >= 5.0);
    }

    #test]
    fn exporter_does_not_flag_fast_hardware() {
        let exporter = CalibrationExporter::new(2.0);
        exporter.record_cycle(vec![make_result("crypto_sha256", 5,000)]);
        let state = exporter.snapshot();
        assert!(!state.hardware_at_risk);
        assert_eq(state.fee_scale_factor, 1.0);
    }

    #test]
    fn prometheus_output_contains_metrics() {
        let exporter = CalibrationExporter::new(2.0);
        exporter.record_cycle(vec![make_result("crypto_sha256", 50,000)]);
        let out = exporter.render_prometheus();
        assert!(out.contains("stellar_calibration_host_function_duration_nanoseconds"));
        assert!(out.contains("stellar_calibration_hardware_risk 1"));
    }
}
