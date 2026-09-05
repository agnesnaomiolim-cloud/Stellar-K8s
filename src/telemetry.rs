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
//! OpenTelemetry initialization and utilities
//!
//! Provides functions to set up distributed tracing with OTLP export and
//! trace-ID injection into structured JSON logs.
//!
//! # Trace ID in logs
//!
//! [`OtelTraceIdLayer`] is a thin `tracing_subscriber::Layer` that reads the
//! active OTel span from the current tracing span's extensions and appends
//! `trace_id` and `span_id` W3C hex fields to every log event.  This lets
//! operators correlate log lines with traces in Honeycomb / Jaeger / Tempo.

pub mod ceff;
pub mod metrics;

use opentelemetry::global;
use opentelemetry_sdk::trace::{Tracer, TracerProvider};
use opentelemetry_otlp::WithExportConfig;
use tracing_opentelemetry::OpenTelemetryLayer;
use tracing_subscriber::Layer;

/// A span processor that scrubs sensitive information from span attributes
#[derive(Debug)]
struct ScrubbingProcessor {
    inner: std::sync::Mutex<Box<dyn SpanProcessor + Send + Sync>>,
}

impl ScrubbingProcessor {
    fn new(inner: Box<dyn SpanProcessor + Send + Sync>) -> Self {
        ScrubbingProcessor {
            inner: std::sync::Mutex::new(inner),
        }
    }

    fn scrub_attributes(&self, attributes: &mut [KeyValue]) {
        for kv in attributes.iter_mut() {
            let key = kv.key.as_str();
            if is_sensitive_attribute(key) {
                kv.value = opentelemetry::Value::String("[REDACTED]".into());
            }
        }
    }
}

impl SpanProcessor for ScrubbingProcessor {
    fn on_start(&self, span: &mut opentelemetry_sdk::trace::Span, cx: &opentelemetry::Context) {
        if let Ok(inner) = self.inner.lock() {
            inner.on_start(span, cx);
        }
    }

    fn on_end(&self, mut span: SpanData) {
        self.scrub_attributes(&mut span.attributes);
        if let Ok(inner) = self.inner.lock() {
            inner.on_end(span);
        }
    }

    fn force_flush(&self) -> TraceResult<()> {
        if let Ok(inner) = self.inner.lock() {
            inner.force_flush()
        } else {
            Ok(())
        }
    }

    fn shutdown(&mut self) -> TraceResult<()> {
        if let Ok(mut inner) = self.inner.lock() {
            inner.shutdown()
        } else {
            Ok(())
        }
    }
}

fn is_sensitive_attribute(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    matches!(
        key.as_str(),
        "net.peer.ip"
            | "net.host.ip"
            | "http.client_ip"
            | "k8s.cluster.name"
            | "host.name"
            | "http.request.header.authorization"
            | "http.request.header.cookie"
            | "http.request.header.set-cookie"
            | "http.request.header.x-api-key"
            | "http.request.header.x-auth-token"
            | "authorization"
            | "cookie"
            | "set-cookie"
            | "x-api-key"
            | "x-auth-token"
            | "password"
            | "token"
            | "secret"
            | "private_key"
    ) || key.contains("password")
        || key.contains("authorization")
        || (key.contains("private") && key.contains("key"))
}

thread_local! {
    static TRACE_CONTEXT: std::cell::RefCell<Option<(String, String)>> =
        const { std::cell::RefCell::new(None) };
}

/// Current W3C `trace_id` / `span_id` if a span is active.
pub fn current_trace_context() -> Option<(String, String)> {
    TRACE_CONTEXT.with(|cell| cell.borrow().clone())
}

fn store_trace_context(trace_id: String, span_id: String) {
    TRACE_CONTEXT.with(|cell| {
        *cell.borrow_mut() = Some((trace_id, span_id));
    });
}

/// W3C Trace Context injector for `http::HeaderMap` / reqwest headers.
pub struct HeaderInjector<'a>(pub &'a mut http::HeaderMap);

impl opentelemetry::propagation::Injector for HeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if is_sensitive_attribute(key) {
            return;
        }
        if let (Ok(name), Ok(val)) = (
            http::HeaderName::from_bytes(key.as_bytes()),
            http::HeaderValue::from_str(&value),
        ) {
            self.0.insert(name, val);
        }
    }
}

/// W3C Trace Context extractor for incoming HTTP headers.
pub struct HeaderExtractor<'a>(pub &'a http::HeaderMap);

impl opentelemetry::propagation::Extractor for HeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|k| k.as_str()).collect()
    }
}

/// Inject the current span context into outbound HTTP headers.
pub fn inject_trace_headers(headers: &mut http::HeaderMap) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;
    let cx = tracing::Span::current().context();
    global::get_text_map_propagator(|propagator| {
        propagator.inject_context(&cx, &mut HeaderInjector(headers));
    });
}

/// Extract a parent context from incoming HTTP headers (W3C `traceparent`).
pub fn extract_parent_context(headers: &http::HeaderMap) -> opentelemetry::Context {
    global::get_text_map_propagator(|propagator| propagator.extract(&HeaderExtractor(headers)))
}

fn sampler_from_env() -> Sampler {
    let kind =
        env::var("OTEL_TRACES_SAMPLER").unwrap_or_else(|_| "parentbased_traceidratio".into());
    let ratio = env::var("OTEL_TRACES_SAMPLER_ARG")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);
    match kind.as_str() {
        "always_on" => Sampler::AlwaysOn,
        "always_off" => Sampler::AlwaysOff,
        "traceidratio" => Sampler::TraceIdRatioBased(ratio),
        _ => Sampler::ParentBased(Box::new(Sampler::TraceIdRatioBased(ratio))),
    }
}

fn service_name_from_env() -> String {
    env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| "stellar-operator".to_string())
}

/// HTTP middleware: join or start a trace, record safe span attributes, never
/// capture Authorization / cookies / bodies.
#[cfg(any(feature = "rest-api", feature = "admission-webhook"))]
pub async fn http_trace_middleware(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use tracing::Instrument;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let parent = extract_parent_context(request.headers());
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let service = service_name_from_env();

    let span = tracing::info_span!(
        "http.request",
        otel.name = %format!("{method} {path}"),
        otel.kind = "server",
        http.method = %method,
        http.route = %path,
        http.status_code = tracing::field::Empty,
        service.name = %service,
    );
    span.set_parent(parent);

    async move {
        let response = next.run(request).await;
        tracing::Span::current().record("http.status_code", response.status().as_u16());
        response
    }
    .instrument(span)
    .await
}

// ---------------------------------------------------------------------------
// Trace-ID injection layer
// ---------------------------------------------------------------------------

/// A `tracing_subscriber` layer that appends `trace_id` and `span_id` W3C hex
/// fields to every JSON log event when an active OTel span is present.
///
/// Add this layer **after** the `fmt::layer()` so the fields appear in the
/// same JSON object:
///
///