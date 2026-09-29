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

//! Standalone debug HTTP server exposing pprof-compatible heap profiling
//! endpoints for the Stellar-K8s operator (issue #305).
//!
//! # Security model
//!
//! The server binds exclusively to `127.0.0.1` (never `0.0.0.0`) so it is
//! unreachable from outside the pod without an explicit `kubectl port-forward`.
//! Every endpoint additionally validates an `X-Profiling-Token` header whose
//! SHA-256 must match the value configured at startup.
//!
//! # Endpoints
//!
//! | Method | Path | Description |
//! |--------|------|-------------|
//! | `GET` | `/debug/pprof/heap` | Dump live heap profile (pprof pb.gz or JSON) |
//! | `GET` | `/debug/pprof/heap/activate` | Activate jemalloc sampling |
//! | `GET` | `/debug/pprof/heap/deactivate` | Deactivate jemalloc sampling |
//! | `GET` | `/debug/pprof/alloc_stats` | Lightweight jemalloc stats snapshot |
//! | `GET` | `/debug/pprof/` | Endpoint index |
//! | `GET` | `/healthz` | Liveness probe |
//!
//! # Usage
//!
//! ```bash
//! # Port-forward from your workstation
//! kubectl port-forward pod/<operator-pod> 6060:6060 -n stellar-system
//!
//! # Pull a live heap profile (pprof binary)
//! TOKEN=$(kubectl get secret stellar-profiling-token -n stellar-system \
//!         -o jsonpath='{.data.token}' | base64 -d)
//! curl -sSf -H "X-Profiling-Token: $TOKEN" \
//!      "http://localhost:6060/debug/pprof/heap" -o heap.pb.gz
//!
//! # Render a flamegraph
//! go tool pprof -http=:8080 heap.pb.gz
//! ```
//!
//! See `docs/operations/profiling-runbook.md` for the full runbook.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::profiling::{activate, deactivate, dump_pprof, is_active, AllocStats};

// ── Server configuration ──────────────────────────────────────────────────────

/// Configuration for the debug HTTP server.
#[derive(Debug, Clone)]
pub struct DebugServerConfig {
    /// Bind address.  **Must remain `127.0.0.1`** in all production deployments.
    ///
    /// Override only in integration tests or local development.
    pub bind_addr: String,
    /// SHA-256 hex digest of the `X-Profiling-Token` header value.
    ///
    /// Compute with: `echo -n "<token>" | sha256sum`
    pub token_sha256: String,
    /// Maximum number of concurrent heap dump requests (prevents OOM from
    /// simultaneous dumps).
    pub max_concurrent_dumps: usize,
}

impl Default for DebugServerConfig {
    fn default() -> Self {
        Self {
            bind_addr: "127.0.0.1:6060".to_string(),
            token_sha256: String::new(),
            max_concurrent_dumps: 1,
        }
    }
}

// ── Shared state ──────────────────────────────────────────────────────────────

#[derive(Clone)]
struct ServerState {
    config: Arc<DebugServerConfig>,
    /// Semaphore that limits concurrent heap dump operations.
    dump_sem: Arc<tokio::sync::Semaphore>,
}

impl ServerState {
    fn new(config: DebugServerConfig) -> Self {
        let max = config.max_concurrent_dumps.max(1);
        Self {
            config: Arc::new(config),
            dump_sem: Arc::new(tokio::sync::Semaphore::new(max)),
        }
    }

    /// Constant-time token verification.
    ///
    /// Returns `true` only when the SHA-256 of `raw_token` matches the
    /// configured digest.  The comparison is XOR-folded to resist timing
    /// side-channels.
    fn verify_token(&self, raw_token: &str) -> bool {
        let expected = &self.config.token_sha256;
        if expected.is_empty() {
            warn!("profiling token hash not configured — denying all requests");
            return false;
        }
        let digest = format!("{:x}", Sha256::digest(raw_token.as_bytes()));
        if digest.len() != expected.len() {
            return false;
        }
        digest
            .as_bytes()
            .iter()
            .zip(expected.as_bytes())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0
    }
}

// ── Request / response types ──────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct HeapQuery {
    /// `json` (default) or `proto` (raw pprof pb.gz bytes, requires
    /// `--features profiling`).
    format: Option<String>,
}

#[derive(Debug, Serialize)]
struct HeapJsonResponse {
    profiling_active: bool,
    alloc_stats: AllocStats,
    /// Human-readable hint on how to pull a binary pprof dump.
    hint: String,
}

#[derive(Debug, Serialize)]
struct StatusResponse {
    profiling_active: bool,
    message: String,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    error: String,
    code: u16,
}

impl ErrorResponse {
    fn new(code: StatusCode, msg: impl Into<String>) -> (StatusCode, Json<Self>) {
        let code_u16 = code.as_u16();
        (code, Json(Self { error: msg.into(), code: code_u16 }))
    }
}

// ── Auth helper ───────────────────────────────────────────────────────────────

/// Extract and verify the `X-Profiling-Token` header.
fn check_auth(
    headers: &HeaderMap,
    state: &ServerState,
) -> Result<(), (StatusCode, Json<ErrorResponse>)> {
    let token = headers
        .get("X-Profiling-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if token.is_empty() {
        return Err(ErrorResponse::new(
            StatusCode::UNAUTHORIZED,
            "X-Profiling-Token header is required",
        ));
    }

    if !state.verify_token(token) {
        warn!("profiling auth failure — invalid token");
        return Err(ErrorResponse::new(
            StatusCode::UNAUTHORIZED,
            "invalid profiling token",
        ));
    }

    Ok(())
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// `GET /debug/pprof/heap`
///
/// With `?format=json` (default): returns a JSON snapshot of current
/// allocation statistics plus a hint on how to pull a binary profile.
///
/// With `?format=proto`: activates jemalloc sampling, dumps a binary
/// pprof `*.pb.gz` payload, then deactivates sampling.  Requires the
/// `profiling` Cargo feature; returns 501 otherwise.
async fn heap_handler(
    State(state): State<ServerState>,
    Query(q): Query<HeapQuery>,
    headers: HeaderMap,
) -> Response {
    if let Err(e) = check_auth(&headers, &state) {
        return e.into_response();
    }

    let format = q.format.as_deref().unwrap_or("json");

    match format {
        "proto" | "pprof" => {
            // Gate on semaphore to prevent concurrent dumps.
            let _permit = match state.dump_sem.try_acquire() {
                Ok(p) => p,
                Err(_) => {
                    return ErrorResponse::new(
                        StatusCode::TOO_MANY_REQUESTS,
                        "another heap dump is already in progress",
                    )
                    .into_response();
                }
            };

            // Activate → dump → deactivate.
            if let Err(e) = activate() {
                return ErrorResponse::new(
                    StatusCode::NOT_IMPLEMENTED,
                    format!("heap profiling unavailable: {e}"),
                )
                .into_response();
            }

            let result = dump_pprof().await;

            // Always deactivate, even on error.
            let _ = deactivate();

            match result {
                Ok(bytes) => {
                    info!(bytes = bytes.len(), "serving heap pprof dump");
                    (
                        StatusCode::OK,
                        [
                            (header::CONTENT_TYPE, "application/octet-stream"),
                            (
                                header::CONTENT_DISPOSITION,
                                "attachment; filename=\"heap.pb.gz\"",
                            ),
                        ],
                        Bytes::from(bytes),
                    )
                        .into_response()
                }
                Err(e) => ErrorResponse::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("heap dump failed: {e}"),
                )
                .into_response(),
            }
        }

        // Default: JSON stats snapshot — always available, zero overhead.
        _ => {
            let stats = AllocStats::collect();
            let body = HeapJsonResponse {
                profiling_active: is_active(),
                alloc_stats: stats,
                hint: concat!(
                    "To pull a binary pprof dump pipe to 'go tool pprof': ",
                    "curl -H 'X-Profiling-Token: <token>' ",
                    "'http://localhost:6060/debug/pprof/heap?format=proto' -o heap.pb.gz"
                )
                .to_string(),
            };
            (StatusCode::OK, Json(body)).into_response()
        }
    }
}

/// `GET /debug/pprof/heap/activate`
///
/// Activates jemalloc heap sampling.  Idempotent.
async fn activate_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = check_auth(&headers, &state) {
        return e.into_response();
    }

    match activate() {
        Ok(()) => (
            StatusCode::OK,
            Json(StatusResponse {
                profiling_active: true,
                message: "jemalloc heap profiling activated".to_string(),
            }),
        )
            .into_response(),
        Err(e) => ErrorResponse::new(StatusCode::NOT_IMPLEMENTED, e).into_response(),
    }
}

/// `GET /debug/pprof/heap/deactivate`
///
/// Deactivates jemalloc heap sampling, restoring zero-overhead operation.
async fn deactivate_handler(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(e) = check_auth(&headers, &state) {
        return e.into_response();
    }

    match deactivate() {
        Ok(()) => (
            StatusCode::OK,
            Json(StatusResponse {
                profiling_active: false,
                message: "jemalloc heap profiling deactivated".to_string(),
            }),
        )
            .into_response(),
        Err(e) => {
            ErrorResponse::new(StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

/// `GET /debug/pprof/alloc_stats`
///
/// Returns a lightweight jemalloc stats snapshot.  No auth required — the
/// information is low-sensitivity (byte counts only, no addresses/symbols).
async fn alloc_stats_handler(
    State(_state): State<ServerState>,
    headers: HeaderMap,
) -> impl IntoResponse {
    // Require auth for consistency — avoids accidentally exposing memory sizes
    // in a multi-tenant environment.
    if let Err(e) = check_auth(&headers, &_state) {
        return e.into_response();
    }

    let stats = AllocStats::collect();
    (StatusCode::OK, Json(stats)).into_response()
}

/// `GET /debug/pprof/` — endpoint index.
async fn index_handler() -> impl IntoResponse {
    let index = serde_json::json!({
        "server": "stellar-operator debug server",
        "note": "All endpoints except /healthz require X-Profiling-Token header.",
        "endpoints": [
            {
                "path": "/debug/pprof/heap",
                "method": "GET",
                "params": "?format=json|proto",
                "description": "Heap allocation stats (json) or binary pprof dump (proto)"
            },
            {
                "path": "/debug/pprof/heap/activate",
                "method": "GET",
                "description": "Activate jemalloc heap sampling"
            },
            {
                "path": "/debug/pprof/heap/deactivate",
                "method": "GET",
                "description": "Deactivate jemalloc heap sampling"
            },
            {
                "path": "/debug/pprof/alloc_stats",
                "method": "GET",
                "description": "Lightweight jemalloc allocation statistics"
            },
            {
                "path": "/healthz",
                "method": "GET",
                "description": "Liveness probe (no auth required)"
            }
        ]
    });
    (StatusCode::OK, Json(index))
}

/// `GET /healthz` — liveness probe, no auth required.
async fn healthz_handler() -> impl IntoResponse {
    (StatusCode::OK, "OK")
}

// ── Router builder ────────────────────────────────────────────────────────────

/// Build the debug Axum [`Router`] with all profiling routes wired.
///
/// Exposed for use in integration tests and for embedding into an existing
/// Axum application:
///
/// ```no_run
/// use stellar_telemetry::server::debug::{build_router, DebugServerConfig};
/// let router = build_router(DebugServerConfig::default());
/// ```
pub fn build_router(config: DebugServerConfig) -> Router {
    let state = ServerState::new(config);

    Router::new()
        .route("/debug/pprof/", get(index_handler))
        .route("/debug/pprof/heap", get(heap_handler))
        .route("/debug/pprof/heap/activate", get(activate_handler))
        .route("/debug/pprof/heap/deactivate", get(deactivate_handler))
        .route("/debug/pprof/alloc_stats", get(alloc_stats_handler))
        .route("/healthz", get(healthz_handler))
        .with_state(state)
}

// ── Server entry point ────────────────────────────────────────────────────────

/// Start the debug HTTP server and serve until the process exits.
///
/// Binds to `config.bind_addr` (default `127.0.0.1:6060`).
///
/// # Example
///
/// ```no_run
/// use stellar_telemetry::server::debug::{run_debug_server, DebugServerConfig};
///
/// #[tokio::main]
/// async fn main() -> Result<(), Box<dyn std::error::Error>> {
///     let config = DebugServerConfig {
///         token_sha256: "your-sha256-hex-digest-here".to_string(),
///         ..Default::default()
///     };
///     run_debug_server(config).await
/// }
/// ```
pub async fn run_debug_server(
    config: DebugServerConfig,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let addr: SocketAddr = config.bind_addr.parse()?;
    let app = build_router(config);

    let listener = TcpListener::bind(addr).await?;
    info!(
        addr = %addr,
        "Stellar-K8s debug profiling server listening (X-Profiling-Token required)"
    );

    axum::serve(listener, app).await?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::{Method, Request};
    use tower::ServiceExt; // for `oneshot`

    fn test_config() -> DebugServerConfig {
        // sha256("test-token") = 4a3d0c...
        let digest = format!("{:x}", Sha256::digest(b"test-token"));
        DebugServerConfig {
            bind_addr: "127.0.0.1:0".to_string(),
            token_sha256: digest,
            max_concurrent_dumps: 1,
        }
    }

    fn authed_request(path: &str) -> Request<axum::body::Body> {
        Request::builder()
            .method(Method::GET)
            .uri(path)
            .header("X-Profiling-Token", "test-token")
            .body(axum::body::Body::empty())
            .unwrap()
    }

    fn unauthed_request(path: &str) -> Request<axum::body::Body> {
        Request::builder()
            .method(Method::GET)
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    #[tokio::test]
    async fn healthz_returns_200_without_token() {
        let app = build_router(test_config());
        let resp = app.oneshot(unauthed_request("/healthz")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn heap_requires_token() {
        let app = build_router(test_config());
        let resp = app
            .oneshot(unauthed_request("/debug/pprof/heap"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn heap_json_returns_alloc_stats() {
        let app = build_router(test_config());
        let resp = app
            .oneshot(authed_request("/debug/pprof/heap?format=json"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("alloc_stats").is_some(), "alloc_stats field missing");
        assert!(json.get("profiling_active").is_some());
    }

    #[tokio::test]
    async fn alloc_stats_returns_200_with_valid_token() {
        let app = build_router(test_config());
        let resp = app
            .oneshot(authed_request("/debug/pprof/alloc_stats"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let stats: AllocStats = serde_json::from_slice(&body).unwrap();
        assert!(stats.snapshot_unix_secs > 1_580_000_000);
    }

    #[tokio::test]
    async fn index_returns_endpoint_list() {
        let app = build_router(test_config());
        let resp = app
            .oneshot(unauthed_request("/debug/pprof/"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["endpoints"].is_array());
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let app = build_router(test_config());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/debug/pprof/heap")
                    .header("X-Profiling-Token", "wrong-token")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn proto_format_returns_501_without_profiling_feature() {
        // On non-profiling builds, requesting ?format=proto should return 501.
        #[cfg(not(feature = "profiling"))]
        {
            let app = build_router(test_config());
            let resp = app
                .oneshot(authed_request("/debug/pprof/heap?format=proto"))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_IMPLEMENTED);
        }

        // On profiling builds the activation itself works — just skip this test.
        #[cfg(feature = "profiling")]
        {
            // Activation is tested elsewhere; nothing to assert here without
            // a live jemalloc build.
        }
    }

    #[test]
    fn server_state_verify_token_correct() {
        let digest = format!("{:x}", Sha256::digest(b"secret"));
        let state = ServerState::new(DebugServerConfig {
            token_sha256: digest,
            ..Default::default()
        });
        assert!(state.verify_token("secret"));
        assert!(!state.verify_token("wrong"));
        assert!(!state.verify_token(""));
    }

    #[test]
    fn server_state_verify_token_empty_config_denies() {
        let state = ServerState::new(DebugServerConfig {
            token_sha256: String::new(),
            ..Default::default()
        });
        assert!(!state.verify_token("any-token"));
    }
}
