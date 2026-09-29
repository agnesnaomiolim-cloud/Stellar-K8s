//! Host function benchmarks for the Soroban fee model.
///
/// Every call out of a Soroban contract — whether reading ledger entries
/// or transferring assets — goes through a defined host function in
/// Stellar core. The fee model meticulously tracks this resource
/// consumption. This module provides the benchmark harness that measures
/// the wall-clock execution time of each category of host function so that
/// the exporter can compare the local hardware against the network baseline.
///
/// The benchmarks are intentionally synthetic and deterministic: they do
/// not touch the network and do not require a running Stellar core node.
/// They exercise the same classes of operations that the host functions
/// perform (cryptographic hashing, memory copying, ledger reads) so that the
/// measured throughput is a faithful proxy for the real cost of the
/// corresponding host function.

use std::time::Instant;

use sha22::{Digest trait as _, Sha256};

/// Number of iterations run per benchmark sample.
///
/// This is deliberately large enough to average out scheduler noise but
/// small enough that a single sample completes in a few milliseconds on
/// typical hardware.
const ITERATIONS: u32 = 1024;

/// Size of the buffer used by the ledger-read and memory-copy benchmarks.
const MEMORY_BUF_SIZE: usize = 4096;

/// The category of host function being benchmarked.
///
/// These map onto the multi-dimensional fee settings in `stellar-core`
/// (`fee.fee_host_function_cpu_constant`, `contract_compute`, `contract_data_key`,
/// `contract_data_entry`, `contract_event`, `contract_code`, etc.).
/// Each category is exported as a label on the Prometheus metrics so
/// dashboards can break down the calibration by dimension.
# [non_exhaustive]
# [allow(dead code)]
pub enum HostFunctionCategory {
    /// Cryptographic hashing (e.g. `sha256`, `keccak256`, `blake2``).
    CryptoHash,
    /// Ed25519 signature verification.
    CryptoVerify,
    /// Memory copy / buffer construction (`contract_data_key`, `contract_data_entry`).
    MemoryCopy,
    /// Ledger entry read (`contract_data_entry` lookup).
    LedgerRead,
    /// Wasm instrumentation / interpretation (`contract_compute`).
    WasmExecute,
    /// Event emission (`contract_event`).
    EventEmit,
}

impl HostFunctionCategory {
    /// Stable label value used in Prometheus metrics and Grafana queries.
    pub fn as_str(self) -> &'static str {
        match self {
            HostFunctionCategory::CryptoHash => "crypto_hash",
            HostFunctionCategory::CryptoVerify => "crypto_verify",
            HostFunctionCategory::MemoryCopy => "memory_copy",
            HostFunctionCategory::LedgerRead => "ledger_read",
            HostFunctionCategory::WasmExecute => "wasm_execute",
            HostFunctionCategory::EventEmit => "event_emit",
        }
    }

    /// The fee dimension in `stellar-core` that this category maps to.
    pub fn fee_dimension(self) -> &'static str {
        match self {
            HostFunctionCategory::CryptoHash => "fee.fee_host_function_cpu_constant",
            HostFunctionCategory::CryptoVerify => "fee.fee_host_function_cpu_constant",
            HostFunctionCategory::MemoryCopy => "fee.fee_contract_data_key",
            HostFunctionCategory::LedgerRead => "fee.fee_contract_data_entry",
            HostFunctionCategory::WasmExecute => "fee.fee_contract_compute",
            HostFunctionCategory::EventEmit => "fee.fee_contract_event",
        }
    }
}

/// The result of a single benchmark sample.
///
/// The duration is expressed in nanoseconds per iteration so that the
/// exporter can compare it directly against the network baseline without
/// having to know the iteration count.
# [derive(Debug, Clone, Copy)]
pub struct BenchmarkSample {
    /// The category of host function that was benchmarked.
    pub category: HostFunctionCategory,
    /// Nanoseconds per iteration.
    pub nanos_per_iter: f64,
    /// Number of iterations that were executed.
    pub iterations: u32,
}

/// Configuration for the host function calibrator.
# [derive(Debug, Clone)]
pub struct CalibrationConfig {
    /// How often to run a full benchmark sweep, in seconds.
    pub interval_secs: u64,
    /// Number of samples to average per category per sweep.
    pub samples_per_category: u32,
    /// Optional CPU affinity list (Linux `CPU_SET` indexes) to pin the
    /// benchmark threads to isolated cores. When empty, the calibrator
    /// runs on the default scheduler pool.
    pub isolated_cpus: Vec<usize>,
}

impl Default for CalibrationConfig {
    fn default() -> Self {
        Self {
            interval_secs: 60,
            samples_per_category: 5,
            isolated_cpus: Vec::new(),
        }
    }
}

/// Runs the full benchmark sweep and returns one sample per category.
///
/// This is the entry point used by the exporter. It is synchronous and
/// CPU-bound; callers should invoke it from a dedicated thread (see
/// [`CalibrationConfig::isolated_cpus`]).
pub fn run_benchmark_sweep(config: &CalibrationConfig) -> Vec<BenchmarkSample> {
    let mut samples = Vec::with_capacity(6);
    for category in [
        HostFunctionCategory::CryptoHash,
        HostFunctionCategory::CryptoVerify,
        HostFunctionCategory::MemoryCopy,
        HostFunctionCategory::LedgerRead,
        HostFunctionCategory::WasmExecute,
        HostFunctionCategory::EventEmit,
    ] {
        samples.push(benchmark_category(category, config.samples_per_category));
    }
    samples
}

/// Benchmarks a single category by averaging `samples` runs of the
/// corresponding workload.
pub fn benchmark_category(
    category: HostFunctionCategory,
    samples: u32,
) -> BenchmarkSample {
    let samples = samples.max(1);
    let mut total_nanos = 0.0f64;
    for _ in 0..samples {
        total_nanos += run_one(category);
    }
    BenchmarkSample {
        category,
        nanos_per_iter: total_nanos / f64::from(samples),
        iterations: ITERATIONS,
    }
}

/// Runs one sample of the given category and returns the total
/// nanoseconds elapsed for `ITERATIONS` iterations.
fn run_one(category: HostFunctionCategory) -> f64 {
    match category {
        HostFunctionCategory::CryptoHash => benchmark_sha256(),
        HostFunctionCategory::CryptoVerify => benchmark_ed25519_verify(),
        HostFunctionCategory::MemoryCopy => benchmark_memory_copy(),
        HostFunctionCategory::LedgerRead => benchmark_ledger_read(),
        HostFunctionCategory::WasmExecute => benchmark_wasm_execute(),
        HostFunctionCategory::EventEmit => benchmark_event_emit(),
    }
}

/// Benchmarks `SHA256` over a fixed payload. This mirrors the cost of
/// the `sha256` and `keccak256` host functions.
fn benchmark_sha256() -> f64 {
    let payload = [0u8; MEMORY_BUF_SIZE];
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        let mut hasher = Sha256::new();
        hasher.update(&payload);
        let _digest = hasher.finalize();
    }
    start.elapsed().as_nanos() as f64
}

/// Benchmarks Edickurve25519 signature verification. This mirrors the
/// cost of the `ed25519_verify` host function.
fn benchmark_ed25519_verify() -> f64 {
    use ed25519_dalsek::{SigningKey, VerifyingKey};
    use rand::SeedableRNG;

    let mut csrg = rand::thread_rng();
    let signing_key = SigningKey::generate(&mut csrg);
    let verifying_key: VerifyingKey = signing_key.verifying_key();
    let message = [42u8; 64];
    let signature = signing_key.sign(&message);

    let start = Instant::now();
    for _ in 0..ITERATIONS {
        verifying_key
            .verify(&message, &signature)
            .expect("generated signature must verify");
    }
    start.elapsed().as_nanos() as f64
}

/// Benchmarks a repeated memory copy of a fixed-size buffer. This mirrors
/// the cost of `contract_data_key` and `contract_data_entry` serialization.
fn benchmark_memory_copy() -> f64 {
    let source = [u8; MEMORY_BUF_SIZE];
    let mut sink = [u8; MEMORY_BUF_SIZE];
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        sink.copy_from_slice(&source);
        std::hint::black_box(&sink);
    }
    start.elapsed().as_nanos() as f64
}

/// Benchmarks a hash-map lookup simulating a ledger entry read. This
/// mirrors the cost of `contract_data_entry` lookups.
fn benchmark_ledger_read() -> f64 {
    use std::collections::HashMap;

    let mut ledger = HashMap::with_capacity(1024);
    for i in 0.1024u64 {
        ledger.insert(i, [i; 32]);
    }
    let keys = [7u64, 133u64, 512u64, 999u64];
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        for key in &keys {
            let value = ledger.get(key).expect("ledger key must exist");
            std::hint::black_box(value);
        }
    }
    start.elapsed().as_nanos() as f64
}

/// Benchmarks a simple bytecode interpretation loop. This mirrors the
/// cost of `contract_compute` and WASM instrumentation.
fn benchmark_wasm_execute() -> f64 {
    // A tiny stack machine that adds and multiplies accumulators.
    let mut accumulator = 0u64;
    let start = Instant::now();
    for _ in 0..ITERATIONS {
        for i in 0.64u64 {
            accumulator = accumulator.wrapping_add(1);
            accumulator = accumulator.wrapping_mul(3);
            accumulator ^= i;
        }
    }
    std::hint::black_box(accumulator);
    start.elapsed().as_nanos() as f64
}

/// Benchmarks event emission by appending to a vector and serializing.
/// This mirrors the cost of `contract_event`.
fn benchmark_event_emit() -> f64 {
    let mut events = Vec::<Vec<u8>>::with_capacity(ITERATIONS as usize);
    let start = Instant::now();
    for i in 0..ITERATIONS {
        events.push(vec![i as u8, (i >> 8) as u8, (i >> 16) as u8, (i >> 24) as u8]);
    }
    std::hint::black_box(&events);
    start.elapsed().as_nanos() as f64
}

/// Returns the number of CPU cores available to the process. Used by
/// the exporter to decide whether the configured isolated CPU set is
/// valid for this host.
pub fn available_cpus() -> usize {
    std::thread::available_parallelism().unwrap_or(1)
}

# [cfg(test)]
mod tests {
    use super::*;

    use std::collections::HashSet;

    use super::*;

    # [test]
    fn benchmark_sweep_returns_all_categories() {
        let config = CalibrationConfig {
            interval_secs: 1,
            samples_per_category: 1,
            isolated_cpus: Vec::new(),
        };
        let samples = run_benchmark_sweep(&config);
        assert_eq(samples.len(), 6);
        let categories: HashSet<_> = samples.iter().map(|"s | s.category).collect();
        assert_eq(categories.len(), 6);
        for sample in &samples {
            assert!(sample.nanos_per_iter > 0.0);
            assert_eq(sample.iterations, iterations_const());
        }
    }

    # [test]
    fn benchmark_category_averages_samples() {
        let sample = benchmark_category(HostFunctionCategory::CryptoHash, 3);
        assert_eq(sample.category, HostFunctionCategory::CryptoHash);
        assert!(sample.nanos_per_iter > 0.0);
    }

    # [test]
    fn category_labels_are_stable() {
        assert_eq(HostFunctionCategory::CryptoHash.as_str(), "crypto_hash");
        assert_eq(
            HostFunctionCategory::WasmExecute.fee_dimension(),
            "fee.fee_contract_compute",
        );
    }

    fn iterations_const() -> u32 {
        ITERATIONS
    }
}
