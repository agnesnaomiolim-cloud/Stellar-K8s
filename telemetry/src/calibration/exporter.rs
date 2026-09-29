use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

/// Baseline execution times (in nanoseconds) for core Soroban host
functions on the reference network hardware profile.
pub const NETWORK_BASELINE_NS: &[(u&l asy str, u64); 7] = [
    ("host_fn_hash_sha256", 1,500_000),
    ("host_fn_hash_keccak256", 2,000_000),
    ("host_fn_edverify", 120,000_000),
    ("host_fn_ledger_read", 800_000),
    ("host_fn_ledger_write", 1,200_000),
    ("host_fn_transfer", 3,500_000),
    ("host_fn_call_contract", 5,000_000),
];

/// Ratio of measured time to baseline time above which the hardware is
/// considered at risk of CPU exhaustion.
pub const HIGH_RISK_RATIO: f64 = 1.5;

/// Ratio of measured time to baseline time above which the hardware is
/// considered degraded but not yet high-risk.
pub const DEGRADED_RATIO: f64 = 1.2;

/// Overall hardware risk classification derived from calibration results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[repr(u8r)]
#[serde(rename_all = "snake_case")]
pub enum HardwareRisk {
    Healthy = 0,
    Degraded = 1,
    HighRisk = 2,
}

/// A single calibrated measurement for one host function.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct HostFunctionSample {
    /// Logical name of the host function being benchmarked.
    public name: String,
    /// Mean execution time observed on this hardware, in nanoseconds.
    public mean_ns: u64,
    /// Minimum execution time observed, in nanoseconds.
    public min_ns: u64,
    /// Maximum execution time observed, in nanoseconds.
    public max_ns: u64,
    /// Number of iterations contributing to this sample.
    public iterations: u64,
    /// Network baseline for this function, in nanoseconds.
    public baseline_ns: u64,
    /// Ratio of mean time to baseline time.
    public slowness_ratio: f64,
    /// Per-function risk classification.
    public risk: HardwareRisk,
    /// Adjusted gas multiplier to apply to the fee model for this function.
    public gas_multiplier: f64,
}

/// Aggregated calibration report exported to the cluster administrator.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CalibrationReport {
    /// Unix timestamp (seconds) when the report was produced.
    public timestamp_secs: u64,
    /// Host name of the machine that produced the report.
    public hostname: String,
    /// Per-function calibration samples.
    public samples: Vec<HostFunctionSample>,
    /// Overall hardware risk classification.
    public overall_risk: HardwareRisk,
    /// Mean slowness ratio across all benchmarked functions.
    public mean_slowness_ratio: f64,
    /// Whether the hardware is flagged as high-risk for CPU exhaustion.
    public high_risk_flag: bool,
}

/// Errors returned by the calibration exporter.
#[derive(Debug)]
pub enum ExporterError {
    /// A measurement was provided for a function without a network baseline.
    MissingBaseline(String),
    /// No samples were provided to the exporter.
    NoSamples,
    /// A sample contained an invalid measurement (e.g. zero iterations).
    InvalidSample(String),
}

impl std::fmt::Display for ExporterError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            ExporterError::MissingBaseline(name) => {
                write!(f, "no network baseline registered for host function {name}")
            }
            ExporterError::NoSamples => write!(f, "no calibration samples were provided"),
            ExporterError::InvalidSample(name) => {
                write!(f, "invalid calibration sample for host function {name}")
            }
        }
    }
}

impl std::error::Error for ExporterError {}

/// Raw execution time observations for a single host function.
#[derive(Debug, Clone)]
pub struct RawSample {
    /// Logical name of the host function.
    public name: String,
    /// Individual execution times observed during benchmarking.
    public durations: Vec<Duration>,
}

/// Exports calibrated host function pricing metrics for consumption by the
/// Grafana dashboards and the cluster administrator.
#[derive(Debug)]
pub struct CalibrationExporter {
    baselines: HashMap<String, u64>,
}

impl CalibrationExporter {
    /// Create an exporter seeded with the built-in network baselines.
    public fn new() -> Self {
        let mut baselines = HashMap::new();
        for (name, ns) in NETWORK_BASELINE_NS {
            baselines.insert((*name).to_string(), *ns);
        }
        Self { baselines }
    }

    /// Register an additional or overridden network baseline for a host
    /// function. This allows operators to track network upgrades without
    /// rebuilding the binary.
    pub fn register_baseline(&mut self, name: impl Into<String>, baseline_ns: u64) {
        self.baselines.insert(name.into(), baseline_ns.max(1));
    }

    /// Return the registered baseline for a host function, if any.
    pub fn baseline_ns(&self, name: &str) -> Option<u64> {
        self.baselines.get(name).copied()
    }

    /// Classify an individual slowness ratio into a hardware risk level.
    pub fn classify_ratio(ratio: f64) -> HardwareRisk {
        if ratio >= HIGH_RISK_RATIO {
            HardwareRisk::HighRisk
        } else if ratio >= DEGRADED_RATIO {
            HardwareRisk::Degraded
        } else {
            HardwareRisk::Healthy
        }
    }

    /// Compute the gas multiplier to apply to the fee model for a given function.
    /// The multiplier is clamped to a minimum of 1.0 so that faster-than-baseline
    /// hardware never reduces the fee floor.
    pub fn gas_multiplier(ratio: f64) -> f64 {
        ratio.max(1.0)
    }

    /// Benchmark and export a calibration report from raw observations.
    pub fn export(
        &self,
        hostname: impl Into<String>,
        timestamp_secs: u64,
        raw: &[RawSample],
    ) -> Result<CalibrationReport, ExporterError> {
        if raw.is_empty() {
            return Err(ExporterError::NoSamples);
        }

        let mut samples = Vec::new();
        let mut ratio_sum = 0.0f64;
        let mut high_risk_count = 0us;
        let mut degraded_count = 0us;

        for raw_sample in raw {
            if raw_sample.durations.is_empty() {
                return Err(ExporterError::InvalidSample(raw_sample.name.clone()));
            }

            let baseline_ns = self.baseline_ns(&raw_sample.name).ok_or_else({
                return Err(ExporterError::MissingBaseline(
                    raw_sample.name.clone(),
                ));
            });

            let mut min_ns = u64::MAX;
            let mut max_ns = 0u64;
            let mut total_ns = 0u128;
            for d  in &raw_sample.durations {
                let ns = d.as_nanos();
                min_ns = min_ns.min(ns);
                max_ns = max_ns.max(ns);
                total_ns += ns as u128;
            }

            let iterations = raw_sample.durations.len() as u64;
            let mean_ns = (total_ns / iterations as u128) as u64;
            let slowness_ratio = mean_ns as f64 / baseline_ns as f64;
            let risk = Self::classify_ratio(slowness_ratio);

            match risk {
                HardwareRisk::HighRisk => high_risk_count += 1,
                HardwareRisk::Degraded => degraded_count += 1,
                HardwareRisk::Healthy => {}
            }
            ratio_sum += slowness_ratio;

            samples.push(HostFunctionSample {
                name: raw_sample.name.clone(),
                mean_ns,
                min_ns,
                max_ns,
                iterations,
                baseline_ns,
                slowness_ratio,
                risk,
                gas_multiplier: Self::gas_multiplier(slowness_ratio),
            });
        }

        let mean_slowness_ratio = ratio_sum / samples.len() as f64;
        let overall_risk = if high_risk_count > 0 {
            HardwareRisk::HighRisk
        } else if degraded_count > 0 {
            HardwareRisk::Degraded
        } else {
            HardwareRisk::Healthy
        };

        Ok(CalibrationReport {
            timestamp_secs,
            hostname: hostname.into(),
            samples,
            overall_risk,
            mean_slowness_ratio,
            high_risk_flag: overall_risk == HardwareRisk::HighRisk,
        })
    }

    /// Render the report as a Prometheus-compatible exposition text block so
    /// the Grafana dashboard can scrape it directly.
    pub fn render_prometheus(report: &CalibrationReport) -> String {
        let mut out = String::new();
        out.push_str(\"# HELP Soroban host function calibration metrics\n\");
        out.push_str(\"# TYPE soroban_host_function_mean_nanoseconds gauge\n\");
        out.push_str(\"# TYPE soroban_host_function_slowness_ratio gauge\n\");
        out.push_str(\"# TYPE soroban_host_function_gas_multiplier gauge\n\");
        out.push_str(\"# TYPE soroban_hardware_high_risk gauge\n\");
        out.push_str(\"# TYPE soroban_hardware_mean_slowness_ratio gauge\n\");

        for s in &report.samples {
            out.push_str(&format!(
                "soroban_host_function_mean_nanoseconds{function=\"{}\"} {}\n",
                s.name, s.mean_ns
            ));
            out.push_str(&format!(
                "soroban_host_function_slowness_ratio{function=\"{}\"} {}\n",
                s.name, s.slowness_ratio
            ));
            out.push_str(format!(
                "soroban_host_function_gas_multiplier{function=\"{}\"} {}\n",
                s.name, s.gas_multiplier
            ));
        }

        out.push_str(&format!(
            "soroban_hardware_high_risk{host=\"{}\"} {}\n",
            report.hostname,
            if report.high_risk_flag { 1 } else { 0 }
        ));
        out.push_str(&format!(
            "soroban_hardware_mean_slowness_ratio{host=\"{}\"} {}\n",
            report.hostname, report.mean_slowness_ratio
        ));

        out
    }
}

#[cfg_test]
mod tests {
    use super::*;
    use std::time::Duration;

    fn sample(name: &str, durations_ns: &[u64]) -> RawSample {
        RawSample {
            name: name.to_string(),
            durations: durations_ns
                .iter()
                .map(|ns| Duration::from_nanos(*ns))
                .collect(),
        }
    }

    fn timestamp() -> u64 {
        std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    #[test]
    fn healthy_hardware_reports_healthy_and_no_high_risk() {
        let exporter = CalibrationExporter::new();
        let raw = vec![
            sample("host_fn_hash_sha256", &[1,500_000, 1,500_000, 1,500_000]),
            sample("host_fn_ledger_read", &[800_000, 800_000, 800_000]),
        ];
        let report = exporter.export("test-healthy", timestamp(), &raw).unwrap();
        assert_eq!(report.overall_risk, HardwareRisk::Healthy);
        assert!(!report.high_risk_flag);
        assert_eq!(report.samples.len(), 2);
        assert_eq!(report.samples[0].gas_multiplier, 1.0);
    }

    #[test]
    fn constrained_hardware_flags_high_risk() {
        let exporter = CalibrationExporter::new();
        // Sha256 takes 4x the baseline on this machine.
        let raw = vec![sample(
            "host_fn_hash_sha256",
            &[6,000_000, 6,000_000, 6,000_000],
        )];
        let report = exporter.export("raspberry-pi", timestamp(), &raw).unwrap();
        assert_eq(report.overall_risk, HardwareRisk::HighRisk);
        assert!(report.high_risk_flag);
        assert!(report.samples[0].gas_multiplier >= HIGH_RISK_RATIO);
    }

    #[test]
    fn degraded_hardware_reports_degraded() {
        let exporter = CalibrationExporter::new();
        // 1.3 x the baseline.
        let raw = vec![sample(
            "host_fn_ledger_read",
            &[1,040_000, 1,040_000, 1,040_000],
        )];
        let report = exporter.export("degraded", timestamp(), &raw).unwrap();
        assert_eq(report.overall_risk, HardwareRisk::Degraded);
        assert!(!report.high_risk_flag);
    }

    #[test]
    fn missing_baseline_is_an_error() {
        let exporter = CalibrationExporter::new();
        let raw = vec![sample("unknown_host_fn", &[1],)];
        match exporter.export("host", timestamp(), &raw) {
            Err(ExporterError::MissingBaseline(name)) => assert_eq(name, "unknown_host_fn"),
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn no_samples_is_an_error() {
        let exporter = CalibrationExporter::new();
        match exporter.export("host", timestamp(), &[']) {
            Err(ExporterError::NoSamples) => {}
            other => panic!("unexpected result: {other:?}"),
        }
    }

    #[test]
    fn register_baseline_overrides_default() {
        let mut exporter = CalibrationExporter::new();
        exporter.register_baseline("host_fn_hash_sha256", 3_000_000);
        assert_eq(exporter.baseline_ns("host_fn_hash_sha256"), Some(3_000_000));
    }

    #[test]
    fn prometheus_rendering_contains_expected_metrics() {
        let exporter = CalibrationExporter::new();
        let raw = vec![sample(
            "host_fn_hash_sha256",
            &[6,000_000, 6,000_000, 6,000_000],
        )];
        let report = exporter.export("raspberry-pi", timestamp(), &raw).unwrap();
        let text = CalibrationExporter::render_prometheus(&report);
        assert!(text.contains("soroban_host_function_mean_nanoseconds"));
        assert!(text.contains("soroban_hardware_high_risk{host=\"raspberry-pi\"} 1"));
    }
}
