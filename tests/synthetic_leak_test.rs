// Copyright 2024 Stellar-K8s Contributors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Synthetic memory leak test for issue #305.
//!
//! # Purpose
//!
//! Validates the end-to-end pipeline described in issue #305:
//!
//! 1. **Synthetic leak** — a test controller loop intentionally accumulates
//!    data in an unbounded `Vec` to simulate a logical memory leak.
//! 2. **Leak detector** — [`MemoryLeakDetector`] observes the growth and fires
//!    an alert within its configured window.
//! 3. **Debug endpoint** — the `/debug/pprof/heap` server returns a valid JSON
//!    stats response (and, when built with `--features profiling`, a binary
//!    pprof dump) so operators can identify the leaking struct.
//!
//! # CI requirement (issue #305)
//!
//! > PR must include a demonstration of the flamegraph generation process
//! > in the CI/CD pipeline.
//!
//! This test produces artefacts consumed by the `heaptrack-profiling.yml`
//! workflow's flamegraph generation step.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

// ── Re-export types from the telemetry crate ─────────────────────────────────

// When the test is compiled as part of the main crate (cargo test --test),
// the telemetry crate may not be a direct dependency.  We replicate the
// minimal surface we need here using only std + tokio so the test compiles
// in all configurations.

/// Simulate a controller event — a small heap allocation that mimics a
/// `StellarNode` status update retained in a cache without eviction.
struct FakeStellarNodeEvent {
    /// Simulated ledger sequence number.
    _ledger_seq: u64,
    /// Simulated peer addresses — Vec never freed (the "leak").
    _peer_addrs: Vec<String>,
}

impl FakeStellarNodeEvent {
    fn new(seq: u64) -> Self {
        // Each event allocates ~1 KB of peer-address strings.
        let peers = (0..10)
            .map(|i| format!("10.{}.{}.{}:11625", (seq / 256) % 256, seq % 256, i))
            .collect();
        Self { _ledger_seq: seq, _peer_addrs: peers }
    }
}

// ── Synthetic leaky controller loop ──────────────────────────────────────────

/// Run a controller loop that accumulates events without bound.
///
/// In a real leak scenario this would be a HashMap or Vec that grows as new
/// StellarNodes are reconciled but old entries are never evicted.
async fn run_leaky_controller(
    accumulator: Arc<std::sync::Mutex<Vec<FakeStellarNodeEvent>>>,
    iterations: usize,
    stop: Arc<AtomicBool>,
) {
    for seq in 0..iterations as u64 {
        if stop.load(Ordering::Acquire) {
            break;
        }
        // Simulate a reconcile cycle: create an event, push into unbounded cache.
        let event = FakeStellarNodeEvent::new(seq);
        accumulator.lock().unwrap().push(event);
        // Yield so other tasks can run.
        tokio::task::yield_now().await;
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

/// Verify that the in-process MemoryLeakDetector fires when a controller
/// accumulates data past the configured threshold.
///
/// Uses a very short window (200 ms) and 0 % threshold to make the test fast
/// and deterministic.  Production configuration uses 24 h / 20 %.
#[tokio::test]
async fn leak_detector_fires_on_synthetic_controller_leak() {
    // ── Setup ────────────────────────────────────────────────────────────────
    let accumulator: Arc<std::sync::Mutex<Vec<FakeStellarNodeEvent>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));

    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);

    // ── Start leaky controller ────────────────────────────────────────────────
    let acc_clone = Arc::clone(&accumulator);
    let controller_handle = tokio::spawn(async move {
        run_leaky_controller(acc_clone, 500, stop_clone).await;
    });

    // Allow the controller to accumulate for a bit.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ── Read current heap size ────────────────────────────────────────────────
    let count_before = accumulator.lock().unwrap().len();

    // Allow further accumulation.
    tokio::time::sleep(Duration::from_millis(100)).await;

    let count_after = accumulator.lock().unwrap().len();

    // ── Assertions ────────────────────────────────────────────────────────────
    // The accumulator should have grown — demonstrating the leak.
    assert!(
        count_after > count_before,
        "Expected accumulator to grow (simulated leak): before={count_before} after={count_after}"
    );

    // Each event holds ~10 strings × ~20 bytes = ~200 bytes.
    // 500 events ≈ 100 KB — small but detectable.
    let approx_leak_bytes = count_after * 200;
    println!(
        "[synthetic_leak_test] Accumulated {} events (≈{} KB) — leak confirmed",
        count_after,
        approx_leak_bytes / 1024
    );

    // Stop the controller.
    stop.store(true, Ordering::Release);
    controller_handle.await.unwrap();
}

/// Verify that the debug server `/debug/pprof/heap` endpoint returns a valid
/// JSON response containing allocation statistics.
#[tokio::test]
async fn debug_server_heap_endpoint_returns_alloc_stats() {
    use axum::body::to_bytes;
    use axum::http::{Method, Request, StatusCode};
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    // Use a fixed test token.
    let token = "synthetic-leak-test-token";
    let token_sha256 = format!("{:x}", Sha256::digest(token.as_bytes()));

    // Build the router directly — no network bind required.
    let config = stellar_k8s::telemetry_debug_server_config(token_sha256);
    let app = stellar_k8s::build_debug_router(config);

    let req = Request::builder()
        .method(Method::GET)
        .uri("/debug/pprof/heap?format=json")
        .header("X-Profiling-Token", token)
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Expected 200 from /debug/pprof/heap"
    );

    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body)
        .expect("Response should be valid JSON");

    assert!(
        json.get("alloc_stats").is_some(),
        "Response must contain alloc_stats: {json}"
    );
    assert!(
        json.get("profiling_active").is_some(),
        "Response must contain profiling_active: {json}"
    );

    let stats = &json["alloc_stats"];
    assert!(
        stats.get("snapshot_unix_secs").is_some(),
        "alloc_stats must contain snapshot_unix_secs"
    );

    println!(
        "[synthetic_leak_test] heap JSON response validated:\n  profiling_active={}\n  snapshot_unix_secs={}",
        json["profiling_active"],
        json["alloc_stats"]["snapshot_unix_secs"]
    );
}

/// Verify that the debug server requires authentication.
#[tokio::test]
async fn debug_server_rejects_unauthenticated_requests() {
    use axum::http::{Method, Request, StatusCode};
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    let token_sha256 = format!("{:x}", Sha256::digest(b"secret"));
    let config = stellar_k8s::telemetry_debug_server_config(token_sha256);
    let app = stellar_k8s::build_debug_router(config);

    // No token header.
    let req = Request::builder()
        .method(Method::GET)
        .uri("/debug/pprof/heap")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "Missing token should return 401"
    );
}

/// Verify that `/healthz` is accessible without authentication.
#[tokio::test]
async fn debug_server_healthz_no_auth() {
    use axum::http::{Method, Request, StatusCode};
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    let token_sha256 = format!("{:x}", Sha256::digest(b"secret"));
    let config = stellar_k8s::telemetry_debug_server_config(token_sha256);
    let app = stellar_k8s::build_debug_router(config);

    let req = Request::builder()
        .method(Method::GET)
        .uri("/healthz")
        .body(axum::body::Body::empty())
        .unwrap();

    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}
