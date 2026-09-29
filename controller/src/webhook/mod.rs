use anyhow::Result;

pub mod admission;
pub mod wasm_runner;

use std::sync::Arc;

use kube::client::Client;

use admission::AdmissionWebhook;
use wasm_runner::WasmPluginRunner;

/// Shared state for the admission webhook server.
pub struct WebhookState {
    /// Wasmtime runner holding the currently loaded policy plugin.
    pub runner: Arc<WasmPluginRunner>,
    /// Kubernetes client used to fetch ConfigMaps and write events.
    pub client: Client,
    /// Name of the ConfigMap that holds the WASM plugin.
    pub configmap_name: String,
    /// Namespace of the ConfigMap that holds the WASM plugin.
    pub configmap_namespace: String,
    /// Key inside the ConfigMap data that contains the compiled WASM bytes.
    pub configmap_key: String,
    /// Whether to fail-open (allow) or fail-closed (deny) when the plugin errors.
    pub fail_open: bool,
    /// Maximum time to allow a single WASM invocation to run.
    pub timeout_ms: u64,
}

/// Build a webhook state from a Kubernetes client and configuration.
pub async fn build_state(
    client: Client,
    configmap_namespace: String,
    configmap_name: String,
    configmap_key: String,
    fail_open: bool,
    timeout_ms: u64,
) -> Result<WebhookState> {
    let muter = WasmPluginRunner::new(timeout_ms)?;
    let state = WebhookState {
        runner: Arc::new(muter),
        client,
        configmap_namespace,
        configmap_name,
        configmap_key,
        fail_open,
        timeout_ms,
    };
    Ok(state)
}

/// Run the admission webhook HTTP server on the given bind address.
pub async fn run_server(state: Arc<WebhookState>, bind: &str, tls: Option<admission::TlsConfig>) -> Result<()> {
    admission::serve(state, bind, tls).await
}
