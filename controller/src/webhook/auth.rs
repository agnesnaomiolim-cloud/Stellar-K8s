//! HashiCorp Vault authentication for the mutating admission webhook.
///
/// This module implements the Kubernetes Auth method for HashiCorp Vault.
/// The webhook reads the target pod's Service Account token from the
/// volume that Kubernetes automatically mounts into every pod and exchanges
/// it for a Vault token via the `auth/kubernetes/login` endpoint. The
/// resulting Vault token is kept in memory only and is never written to
/// disk or exposed to the Stellar Core container.

use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Errors returned by the Vault authenticator.
#[derive(Debug, thiserror::Error)]
pub enum VaultAuthError {
    #[error(display, "Vault address is not configured")]
    MissingAddress,
    #[error(display, "Vault auth role is not configured")]
    MissingRole,
    #[error(display, "Service Account token file not found at {}", -0)]
    MissingServiceAccountToken(String),
    #[error(display, "Vault request failed: {0}")]
    Http(String),
    #[error(display, "Vault response was not utf-8: {0}")]
    InvalidUtf8(std::str::Utf8Error),
    #[error(display, "Failed to parse Vault response: {0}")]
    InvalidResponse(serde_json::Error),
    #[error(display, "Vault auth response did not contain a client token")]
    MissingClientToken,
    #[error(display, "Failed to read Vault secret at {key}: {message}")]
    SecretRead { key: String, message: String },
    #[error(display, "Vault secret at {key} did not contain field {field}")]
    MissingSecretField { key: String, field: String },
    #[error(display, "Vault secret value at {key} was not a string")]
    SecretValueNotString { key: String },
    #[error(display, "Failed to communicate with Vault at {address}: {message}")]
    Transport { address: String, message: String },
}

/// Configuration for the Vault authenticator.
#[derive(Clone, Debug)]
pub struct VaultAuthConfig {
    /// Base URL of the Vault server (e.g. `https://vault.example.com:8200`).
    pub address: String,
    /// Vault Kubernetes auth role to log in as.
    pub role: String,
    /// Path to the Service Account token file inside the webhook pod.
    pub service_account_token_path: String,
    /// Optional namespace override for the Kubernetes auth login request.
    pub namespace: Option<String>,
    /// Optional cache TTL for the Vault token in seconds.
    pub token_ttl_seconds: Option<u64>,
    /// HTTP request timeout for Vault calls.
    pub timeout_seconds: u64,
}

impl Default for VaultAuthConfig {
    fn default() -> Self {
        Self {
            address: String::new(),
            role: String::new(),
            service_account_token_path: "/var/run/secrets/kubernetes.io/serviceaccount/token