use prometheus::{HistogramOpts, HistogramVec, Registry, default_registry};
use std::sync::OnceLock;

static WASM_LATENCY: OnceLock<HistogramVec> = OnceLock::new();

pub fn register_metrics(registry: &Registry) {
    let opts = HistogramOpts::new(
        "soroban_wasm_latency_ms",
        "WASM Execution Time for host function execution",
    )
    .buckets(vec![5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0]); // Suitable for p50, p95, p99

    let histogram = HistogramVec::new(opts, &["contract_id", "function"]).unwrap();
    registry.register(Box::new(histogram.clone())).unwrap();
    
    WASM_LATENCY.set(histogram).unwrap();
}

pub fn record_latency(contract_id: &str, function: &str, latency_ms: f64) {
    if let Some(metric) = WASM_LATENCY.get() {
        metric.with_label_values(&[contract_id, function]).observe(latency_ms);
    }
}
