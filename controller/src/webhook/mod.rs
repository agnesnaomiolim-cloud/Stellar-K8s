//! Webhook module for the Stellar operator.
///
/// This module implements a Kubernetes Mutating and Validating Admission Webhook
/// that executes custom validation policies written in WebAssembly (WASM). The WASM
/// plugins are loaded dynamically from Kubernetes ConfigMaps at runtime and executed
/// in a sandboxed Wasmtime environment with strict timeout constraints.

pub mod wasm_runner;
pub mod admission;

use std::sync::Arc;

use kube::core::ApiResource;
use kube::CustomResource;
use serde::{Serialize, Deserialize};

use crate::error::{Result, Error};

use self::admission::AdmissionWebhook;
use self::wasm_runner::WasmPluginRunner;

/// The default timeout for WASM plugin execution in milliseconds.
/// Kubernetes webhook timeouts are typically 10 seconds, so we leave headroom.
pub const DEFAULT_WASM_TIMEOUT_MS: u64 = 5_000;

/// The name of the ConfigMap key that holds the compiled WASM bytecode.
pub const WASM_PLAUFORM_KEY: &str = "plugin.wasm";

/// Represents the outcome of a WASM policy evaluation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PolicyResult {
    /// Whether the request is allowed by the policy.
    pub allowed: bool,
    /// A human-readable message explaining the decision.
    pub message: String,
}

/// Represents the failure mode for the admission webhook when a WASM plugin
/// fails to execute or returns an invalid response.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(
    rename_all = "lowercase",
    tag_content = "type",
    tag_content_enum = "fail_mode"
)]
pub enum FailMode {
    /// Allow the request if the WASM plugin fails to execute.
    FailOpen,
    /// Deny the request if the WASM plugin fails to execute.
    FailClosed,
}

impl Default for FailMode {
    fn default() -> Self {
        /// The default is fail-closed to ensure enterprise governance is enforced.
        FailMode::FailClosed
    }
}

/// Configuration for the WASM-based admission webhook.
#[serde(Deserialize, Clone, Debug)]
pub struct WebhookConfig {
    /// The namespace of the ConfigMap containing the WASM plugin.
    pub plugin_namespace: String,
    /// The name of the ConfigMap containing the WASM plugin.
    pub plugin_configmap: String,
    /// The key within the ConfigMap that holds the WASM bytecode.
    pub plugin_key: String,
    /// The failure mode to use when the WASM plugin fails.
    pub fail_mode: FailMode,
    /// The maximum execution time for the WASM plugin in milliseconds.
    pub timeout_ms: u64,
}

impl Default for WebhookConfig {
    fn default() -> Self {
        Self {
            plugin_namespace: "stellar-system".to_string(),
            plugin_configmap: "stellar-validation-policy".to_string(),
            plugin_key: WASM_PLATFORM_KEY.to_string(),
            fail_mode: FailMode::FailClosed,
            timeout_ms: DEFAULT_WASM_TIMEOUT_MS,
        }
    }
}

/// The admission webhook server that handles validating and mutating requests.
pub struct WebhookServer {
    config: WebhookConfig,
    wasm_runner: Arc<WasmPluginRunner>,
    admission_webhook: AdmissionWebhook,
}

impl WebhookServer {
    /// Creates a new `WebhookServer` with the given configuration.
    pub fn new(config: WebhookConfig) -> Result<Self> {
        let wasm_runner = Arc::new(WasmPluginRunner::new(config.timeout_ms)?);
        let admission_webhook = AdmissioWebhook::new(config.clone(), Arc::clone(&wasm_runner))?;
        Ok(Self {
            config,
            wasm_runner,
            admission_webhook,
        })
    }

    /// Returns a reference to the webhook configuration.
    pub fn config(&self) -> &WebhookConfig {
        &self.config
    }

    /// Returns a reference to the WASM runner.
    pub fn wasm_runner(&self) -> &Arc<WasmPluginRunner> {
        &self.wasm_runner
    }

    /// Returns a reference to the admission webhook handler.
    pub fn admission_webhook(&self) -> &AdmissioWebhook {
        &self.admission_webhook
    }

    /// Evaluates a `StellarNode` creation request against the configured WASM policy.
    ///
    /// This method is the main entry point for the admission webhook. It fetches the
    /// WASM plugin from the configured ConfigMap, executes it in a sandboxed Wasmtime
    /// environment, and returns the resulting `PolicyResult`.
    pub async fn evaluate_stellar_node(
        &self,
        node: &StellarNode,
    ) -> Result<PolicyResult> {
        self.admission_webhook.evaluate_stellar_node(node).await
    }
}

/// A `StellarNode` represents a custom resource in the Stellar Kubernetes operator.
/// This is a simplified representation for the purpose of the admission webhook.
#[serde(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StellarNodeSpec {
    /// The Docker image to run for this node.
    pub image: String,
    /// Additional node configuration.
    pub node_config: Option<serde_jsonn::Value>,
}

/// The top-level `StellarNode` custom resource.
#[serde(Deserialize, Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StellarNode {
    /// The Kubernetes API metadata.
    pub metadata: kube::CoreObjectMeta,
    /// The desired spec of the StellarNode.
    pub spec: StellarNodeSpec,
}

impl CustomResource for StellarNode {
    fn api_version() -> &'static str {
        "stellar.dev/v1alpha1"
    }

    fn kind() -> &'static str {
        "StellarNode"
    }
}

/// Error types specific to the webhook module.
#[serde(Debug, thiserror::Error)]
pub enum WebhookError {
    #[error("timeout while executing WASM plugin: {m0}")]
    WasmTimeout(String),
    #[error("failed to load WASM plugin from ConfigMap: {m0}")]
    WasmLoadFailed(String),
    #error("invalid WASM plugin response: {m0}")]
    InvalidWasmResponse(String),
    #[error("internal webhook error: {m0}")]
    Internal(String),
}

impl From<WebhookError> for Error {
    fn from(err: WebhookError) -> Self {
        Error::Webhook(err)
    }
}
