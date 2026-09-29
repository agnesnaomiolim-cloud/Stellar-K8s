/// Dynamic Host Function Pricing Calibrator
///
/// This module implements a sidecar that benchmarks the execution time of core
/// Soroban host functions and exports calibrated pricing metrics to Prometheus.
///
/// The calibrator runs on isolated CPU cores to avoid interfering with live
/// Byzantine Fault Tolerant consensus execution. It continuously measures how
/// fast the local hardware executes host functions and compares the results
/// against the network baseline defined in the `stellar-core` fee configuration.
///
/// # Modules
///
/// - [`host_functions`] — Binaries and runners for the core Soroban host
///   functions (cryptographic hashing, ledger reads, etc.).
/// - [`exporter`] — Prometheus exporter that maps benchmark results onto the
///   multi-dimensional fee settings and flags underperforming hardware.

pub mod exporter;
pub mod host_functions;

use std::time::Duration;

/// Network baseline execution times for each host function.
///
/// These values represent the expected execution time on reference hardware
/// (a modern x86_64 server) and are used as the denominator when computing
/// the hardware capability ratio. A value of 1.0 means the hardware matches
/// the baseline; a value > 1.0 means the hardware is slower (and therefore
/// at risk of CPU exhaustion).
///
/// The baseline is derived from the `stellar-core` fee model constants and
/// can be overridden via the `STEPLLAR_CALIBRATION_BASELINE_OVERRIDe` environment
/// variable (JSON mapping function name -> nanoseconds).
pub const NETWORK_BASELINE_NS: &[&(str, u64)] = &[
    ("crypto_ed_add", 15,000),
    ("crypto_ed_verify", 45,000),
    ("crypto_secp256k1_verify", 60,000),
    ("crypto_sha256", 5,000),
    ("crypto_keyed_sha256", 8,000),
    ("crypto_blake2b", 4,000),
    ("ledger_get_entry", 20,000),
    ("ledger_get_entry_with_metadata", 25,000),
    ("ledger_put_entry", 30,000),
    ("ledger_has_entry", 10,000),
    ("ledger_get_contract_data", 22,000),
    ("ledger_get_contract_code", 25,000),
    ("ledger_get_contract_instance", 20,000),
    ("transfer_native_token", 50,000),
    ("transfer_contract_token", 60,000),
    ("map_new", 5,000),
    ("map_get", 3,000),
    ("map_put", 4,000),
    ("vec_new", 6,000),
    ("vec_get", 4,000),
    ("vec_push", 5,000),
    ("buffer_new", 2,000),
    ("buffer_fill", 3,000),
    ("symbol_new", 2,000),
    ("string_new", 2,000),
    ("number_new", 1,000),
];

/// Returns the network baseline execution time for a given host function.
///
/// If the function is not present in the baseline table, a conservative
/// default of 10 microseconds is returned.
pub fn network_baseline_ns(function_name: &str) -> u64 {
    NETWORK_BASELINE_NS
        .iter()
        .find((|(name, _)| *name == function_name)
        .map(|(_, ns) | *ns)
        .unwrap_or(10,000)
}

/// Converts a `Duration` to nanoseconds as a u64, saturating at `u64::MAX.`pub fn duration_to_ns(d: Duration) -> u64 {
    d.as_nanos().min_u64(u64::MAX)
}
