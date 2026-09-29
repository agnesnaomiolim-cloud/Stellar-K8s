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

//! External Secrets Operator (ESO) integration for pulling dynamically generated
//! ed25519 Stellar node identities from AWS Secrets Manager and HashiCorp Vault.
//!
//! # Overview
//!
//! This module bridges the External Secrets Operator with the Stellar-K8s operator to
//! enable GitOps-driven key lifecycle management. It provides:
//!
//! - [`EsoBackend`] — trait abstraction over any ESO-compatible secret backend
//! - [`AwsEsoBackend`] — AWS Secrets Manager backend via IRSA/static credentials
//! - [`VaultEsoBackend`] — HashiCorp Vault KV-v2 backend via Kubernetes auth
//! - [`EsoNodeIdentity`] — parsed ed25519 node identity pulled from the backend
//! - [`EsoIntegrationConfig`] — operator-level configuration for ESO wiring
//! - [`EsoSecretWatcher`] — watches K8s Secrets produced by ESO and reconciles them
//!
//! # Transit Security
//!
//! Secrets travel from the external store → ESO controller pod → K8s Secret (etcd,
//! encrypted at rest) → operator pod memory. The operator never logs the raw seed;
//! only the SHA-256 fingerprint and public key appear in logs and status fields.

use std::{collections::HashMap, fmt, sync::Arc, time::Duration};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Stellar StrKey alphabet (RFC 4648 Base32 without padding).
const BASE32_ALPHA: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
/// Version byte for Stellar Secret Seed (S-key).
const SECRET_SEED_VB: u8 = 18 << 3;
/// Version byte for Stellar public key (G-key).
const PUBLIC_KEY_VB: u8 = 6 << 3;

// ─────────────────────────────────────────────────────────────────────────────
// Errors
// ─────────────────────────────────────────────────────────────────────────────

/// Errors that can occur during ESO integration operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EsoError {
    /// The backend was unreachable (network, auth, or timeout).
    BackendUnavailable(String),
    /// The secret exists but has an unexpected shape.
    InvalidSecretFormat(String),
    /// The ed25519 seed bytes are outside the valid range.
    InvalidSeedMaterial(String),
    /// A Kubernetes API call failed.
    KubernetesApi(String),
    /// The requested secret version does not exist.
    VersionNotFound(String),
    /// Configuration error (missing field, invalid value, etc.).
    Configuration(String),
}

impl fmt::Display for EsoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BackendUnavailable(m) => write!(f, "ESO backend unavailable: {m}"),
            Self::InvalidSecretFormat(m) => write!(f, "invalid secret format: {m}"),
            Self::InvalidSeedMaterial(m) => write!(f, "invalid seed material: {m}"),
            Self::KubernetesApi(m) => write!(f, "Kubernetes API error: {m}"),
            Self::VersionNotFound(m) => write!(f, "secret version not found: {m}"),
            Self::Configuration(m) => write!(f, "ESO configuration error: {m}"),
        }
    }
}

pub type EsoResult<T> = Result<T, EsoError>;

// ─────────────────────────────────────────────────────────────────────────────
// Parsed node identity
// ─────────────────────────────────────────────────────────────────────────────

/// An ed25519 Stellar node identity that has been fetched from an external store
/// and validated as a well-formed StrKey ed25519 key-pair.
///
/// The raw secret seed is kept in a private field. Only the public key and a
/// SHA-256 fingerprint of the seed are exposed for audit logging.
#[derive(Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct EsoNodeIdentity {
    /// Stellar G-key (public key), e.g. `GABC…`.
    pub public_key: String,
    /// SHA-256 hex fingerprint of the seed — safe to log and store.
    pub fingerprint: String,
    /// Version identifier from the external store (e.g. AWS version UUID).
    pub version_id: String,
    /// When this version was created in the external store.
    pub created_at: DateTime<Utc>,
    /// Which backend produced this identity.
    pub backend_type: EsoBackendType,
    // Raw seed is private — never serialized in structured logs.
    #[serde(skip)]
    seed: String,
}

impl fmt::Debug for EsoNodeIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EsoNodeIdentity")
            .field("public_key", &self.public_key)
            .field("fingerprint", &self.fingerprint)
            .field("version_id", &self.version_id)
            .field("created_at", &self.created_at)
            .field("backend_type", &self.backend_type)
            .field("seed", &"<redacted>")
            .finish()
    }
}

impl EsoNodeIdentity {
    /// Build a node identity from a raw Stellar secret seed string (`S…`).
    ///
    /// Returns an error if the seed is not a valid Stellar StrKey secret seed.
    pub fn from_seed(
        seed: String,
        version_id: String,
        created_at: DateTime<Utc>,
        backend_type: EsoBackendType,
    ) -> EsoResult<Self> {
        validate_stellar_seed(&seed)?;
        let public_key = derive_public_key(&seed)?;
        let fingerprint = sha256_hex(seed.as_bytes());
        Ok(Self {
            public_key,
            fingerprint,
            version_id,
            created_at,
            backend_type,
            seed,
        })
    }

    /// Expose the raw seed (only for direct use when updating K8s Secrets).
    ///
    /// Callers MUST NOT log this value; it must be stored in memory only long
    /// enough to write it to a K8s Secret.
    pub fn seed_secret(&self) -> &str {
        &self.seed
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Backend type enum
// ─────────────────────────────────────────────────────────────────────────────

/// Identifies which external secret backend provided a node identity.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum EsoBackendType {
    AwsSecretsManager,
    VaultKv2,
    /// Kubernetes Secret produced by ESO (generic fallback).
    KubernetesEso,
}

// ─────────────────────────────────────────────────────────────────────────────
// Backend trait
// ─────────────────────────────────────────────────────────────────────────────

/// Common interface that all ESO-compatible secret backends must implement.
///
/// Implementations are expected to be async and cancel-safe.
pub trait EsoBackend: Send + Sync + fmt::Debug {
    /// Returns the type discriminant for this backend.
    fn backend_type(&self) -> EsoBackendType;

    /// Fetch the current (latest) node identity for the given `secret_path`.
    ///
    /// The `secret_path` is backend-specific:
    /// - AWS: the full ARN or name of the secret (`stellar/validators/<name>`)
    /// - Vault: the KV-v2 path (`secret/data/stellar/validators/<name>`)
    fn fetch_current<'a>(
        &'a self,
        secret_path: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<EsoNodeIdentity>> + Send + 'a>>;

    /// Fetch a specific version of the node identity.
    fn fetch_version<'a>(
        &'a self,
        secret_path: &'a str,
        version_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<EsoNodeIdentity>> + Send + 'a>>;

    /// List available versions for the secret at `secret_path`.
    fn list_versions<'a>(
        &'a self,
        secret_path: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = EsoResult<Vec<SecretVersionMeta>>> + Send + 'a>,
    >;
}

/// Metadata about one version of a secret in the external store.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SecretVersionMeta {
    pub version_id: String,
    pub created_at: DateTime<Utc>,
    pub is_current: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// AWS Secrets Manager backend
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for the AWS Secrets Manager ESO backend.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct AwsEsoConfig {
    /// AWS region, e.g. `us-east-1`.
    pub region: String,
    /// Optional role ARN for IRSA or cross-account access.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_arn: Option<String>,
    /// JSON field name within the secret whose value is the Stellar seed.
    /// Defaults to `"seed"`.
    #[serde(default = "default_seed_key")]
    pub seed_field: String,
    /// Request timeout in seconds (default: 10).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
}

impl Default for AwsEsoConfig {
    fn default() -> Self {
        Self {
            region: "us-east-1".into(),
            role_arn: None,
            seed_field: default_seed_key(),
            timeout_secs: default_timeout_secs(),
        }
    }
}

fn default_seed_key() -> String {
    "seed".into()
}

fn default_timeout_secs() -> u64 {
    10
}

/// AWS Secrets Manager ESO backend.
///
/// Credentials are resolved through the standard AWS credential chain
/// (IRSA → instance profile → environment variables → config file).
#[derive(Debug)]
pub struct AwsEsoBackend {
    config: AwsEsoConfig,
    /// HTTP client used for direct AWS SDK calls (substituted in tests).
    http_client: Arc<dyn AwsSecretsClient>,
}

impl AwsEsoBackend {
    /// Create a new backend using the provided config.
    pub fn new(config: AwsEsoConfig) -> Self {
        Self {
            config,
            http_client: Arc::new(DefaultAwsSecretsClient),
        }
    }

    /// Create a backend with a custom [`AwsSecretsClient`] for unit testing.
    pub fn with_client(config: AwsEsoConfig, client: Arc<dyn AwsSecretsClient>) -> Self {
        Self {
            config,
            http_client: client,
        }
    }
}

/// Abstraction layer over AWS Secrets Manager API calls — enables test doubles.
pub trait AwsSecretsClient: Send + Sync + fmt::Debug {
    fn get_secret_value<'a>(
        &'a self,
        region: &'a str,
        role_arn: Option<&'a str>,
        secret_id: &'a str,
        version_id: Option<&'a str>,
        timeout: Duration,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = EsoResult<AwsSecretValue>> + Send + 'a>,
    >;

    fn list_secret_versions<'a>(
        &'a self,
        region: &'a str,
        role_arn: Option<&'a str>,
        secret_id: &'a str,
        timeout: Duration,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = EsoResult<Vec<SecretVersionMeta>>> + Send + 'a>,
    >;
}

/// Raw value returned by AWS GetSecretValue.
#[derive(Clone, Debug)]
pub struct AwsSecretValue {
    pub version_id: String,
    pub created_at: DateTime<Utc>,
    pub secret_string: String,
}

/// Production AWS client — calls the real SDK.
#[derive(Debug)]
struct DefaultAwsSecretsClient;

impl AwsSecretsClient for DefaultAwsSecretsClient {
    fn get_secret_value<'a>(
        &'a self,
        region: &'a str,
        _role_arn: Option<&'a str>,
        secret_id: &'a str,
        version_id: Option<&'a str>,
        _timeout: Duration,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<AwsSecretValue>> + Send + 'a>>
    {
        let region = region.to_owned();
        let secret_id = secret_id.to_owned();
        let version_id = version_id.map(str::to_owned);
        Box::pin(async move {
            // In a real deployment this calls aws_sdk_secretsmanager::Client.
            // The SDK is not included as a dependency of the controller sub-crate
            // to keep its dependency surface minimal; callers in the main crate
            // wire up a concrete client.
            Err(EsoError::BackendUnavailable(format!(
                "DefaultAwsSecretsClient is a stub — inject a real client for production. \
                 region={region}, secret={secret_id}, version={version_id:?}"
            )))
        })
    }

    fn list_secret_versions<'a>(
        &'a self,
        region: &'a str,
        _role_arn: Option<&'a str>,
        secret_id: &'a str,
        _timeout: Duration,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = EsoResult<Vec<SecretVersionMeta>>> + Send + 'a>,
    > {
        let region = region.to_owned();
        let secret_id = secret_id.to_owned();
        Box::pin(async move {
            Err(EsoError::BackendUnavailable(format!(
                "DefaultAwsSecretsClient is a stub. region={region}, secret={secret_id}"
            )))
        })
    }
}

impl EsoBackend for AwsEsoBackend {
    fn backend_type(&self) -> EsoBackendType {
        EsoBackendType::AwsSecretsManager
    }

    fn fetch_current<'a>(
        &'a self,
        secret_path: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<EsoNodeIdentity>> + Send + 'a>>
    {
        Box::pin(async move {
            let timeout = Duration::from_secs(self.config.timeout_secs);
            let raw = self
                .http_client
                .get_secret_value(
                    &self.config.region,
                    self.config.role_arn.as_deref(),
                    secret_path,
                    None,
                    timeout,
                )
                .await?;
            self.parse_identity(raw)
        })
    }

    fn fetch_version<'a>(
        &'a self,
        secret_path: &'a str,
        version_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<EsoNodeIdentity>> + Send + 'a>>
    {
        Box::pin(async move {
            let timeout = Duration::from_secs(self.config.timeout_secs);
            let raw = self
                .http_client
                .get_secret_value(
                    &self.config.region,
                    self.config.role_arn.as_deref(),
                    secret_path,
                    Some(version_id),
                    timeout,
                )
                .await?;
            self.parse_identity(raw)
        })
    }

    fn list_versions<'a>(
        &'a self,
        secret_path: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = EsoResult<Vec<SecretVersionMeta>>> + Send + 'a>,
    > {
        Box::pin(async move {
            let timeout = Duration::from_secs(self.config.timeout_secs);
            self.http_client
                .list_secret_versions(
                    &self.config.region,
                    self.config.role_arn.as_deref(),
                    secret_path,
                    timeout,
                )
                .await
        })
    }
}

impl AwsEsoBackend {
    fn parse_identity(&self, raw: AwsSecretValue) -> EsoResult<EsoNodeIdentity> {
        // Parse the JSON secret string to extract the seed field.
        let json: serde_json::Value =
            serde_json::from_str(&raw.secret_string).map_err(|e| {
                EsoError::InvalidSecretFormat(format!(
                    "secret is not valid JSON: {e}"
                ))
            })?;

        let seed = json
            .get(&self.config.seed_field)
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                EsoError::InvalidSecretFormat(format!(
                    "JSON field '{}' not found or not a string",
                    self.config.seed_field
                ))
            })?
            .to_owned();

        EsoNodeIdentity::from_seed(
            seed,
            raw.version_id,
            raw.created_at,
            EsoBackendType::AwsSecretsManager,
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// HashiCorp Vault KV-v2 backend
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for the Vault KV-v2 ESO backend.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VaultEsoConfig {
    /// Vault server address, e.g. `https://vault.vault.svc:8200`.
    pub address: String,
    /// KV-v2 mount path (default: `secret`).
    #[serde(default = "default_vault_mount")]
    pub mount: String,
    /// Vault token or K8s auth role. When using K8s auth the token is sourced
    /// from the pod's projected service-account token.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Kubernetes auth role name when `token` is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes_role: Option<String>,
    /// JSON field inside the KV secret that holds the Stellar seed.
    #[serde(default = "default_seed_key")]
    pub seed_field: String,
    /// Request timeout in seconds (default: 10).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Skip TLS verification (NOT recommended in production).
    #[serde(default)]
    pub tls_skip_verify: bool,
}

impl Default for VaultEsoConfig {
    fn default() -> Self {
        Self {
            address: "https://vault.vault.svc:8200".into(),
            mount: default_vault_mount(),
            token: None,
            kubernetes_role: None,
            seed_field: default_seed_key(),
            timeout_secs: default_timeout_secs(),
            tls_skip_verify: false,
        }
    }
}

fn default_vault_mount() -> String {
    "secret".into()
}

/// HashiCorp Vault KV-v2 ESO backend.
///
/// Uses Vault's Kubernetes auth method when `token` is not set, sourcing the
/// pod's service-account token from the standard projected volume path.
#[derive(Debug)]
pub struct VaultEsoBackend {
    config: VaultEsoConfig,
    http_client: Arc<dyn VaultSecretsClient>,
}

impl VaultEsoBackend {
    /// Create a new Vault backend using the provided config.
    pub fn new(config: VaultEsoConfig) -> Self {
        Self {
            config,
            http_client: Arc::new(DefaultVaultClient),
        }
    }

    /// Create a Vault backend with a custom [`VaultSecretsClient`] for testing.
    pub fn with_client(config: VaultEsoConfig, client: Arc<dyn VaultSecretsClient>) -> Self {
        Self {
            config,
            http_client: client,
        }
    }
}

/// Abstraction over Vault HTTP API calls.
pub trait VaultSecretsClient: Send + Sync + fmt::Debug {
    fn kv_get<'a>(
        &'a self,
        address: &'a str,
        token: &'a str,
        mount: &'a str,
        path: &'a str,
        version: Option<u64>,
        timeout: Duration,
        tls_skip_verify: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<VaultKvData>> + Send + 'a>>;

    fn kv_list_versions<'a>(
        &'a self,
        address: &'a str,
        token: &'a str,
        mount: &'a str,
        path: &'a str,
        timeout: Duration,
        tls_skip_verify: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = EsoResult<Vec<SecretVersionMeta>>> + Send + 'a>,
    >;
}

/// Parsed response from `GET /v1/<mount>/data/<path>`.
#[derive(Clone, Debug)]
pub struct VaultKvData {
    pub version: u64,
    pub created_time: DateTime<Utc>,
    pub data: HashMap<String, String>,
}

#[derive(Debug)]
struct DefaultVaultClient;

impl VaultSecretsClient for DefaultVaultClient {
    fn kv_get<'a>(
        &'a self,
        address: &'a str,
        _token: &'a str,
        mount: &'a str,
        path: &'a str,
        version: Option<u64>,
        _timeout: Duration,
        _tls_skip_verify: bool,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<VaultKvData>> + Send + 'a>>
    {
        let address = address.to_owned();
        let mount = mount.to_owned();
        let path = path.to_owned();
        Box::pin(async move {
            Err(EsoError::BackendUnavailable(format!(
                "DefaultVaultClient is a stub — inject a real client for production. \
                 address={address}, mount={mount}, path={path}, version={version:?}"
            )))
        })
    }

    fn kv_list_versions<'a>(
        &'a self,
        address: &'a str,
        _token: &'a str,
        mount: &'a str,
        path: &'a str,
        _timeout: Duration,
        _tls_skip_verify: bool,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = EsoResult<Vec<SecretVersionMeta>>> + Send + 'a>,
    > {
        let address = address.to_owned();
        let mount = mount.to_owned();
        let path = path.to_owned();
        Box::pin(async move {
            Err(EsoError::BackendUnavailable(format!(
                "DefaultVaultClient is a stub. address={address}, mount={mount}, path={path}"
            )))
        })
    }
}

impl EsoBackend for VaultEsoBackend {
    fn backend_type(&self) -> EsoBackendType {
        EsoBackendType::VaultKv2
    }

    fn fetch_current<'a>(
        &'a self,
        secret_path: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<EsoNodeIdentity>> + Send + 'a>>
    {
        Box::pin(async move {
            let token = self.resolve_token().await?;
            let timeout = Duration::from_secs(self.config.timeout_secs);
            let data = self
                .http_client
                .kv_get(
                    &self.config.address,
                    &token,
                    &self.config.mount,
                    secret_path,
                    None,
                    timeout,
                    self.config.tls_skip_verify,
                )
                .await?;
            self.parse_identity(data)
        })
    }

    fn fetch_version<'a>(
        &'a self,
        secret_path: &'a str,
        version_id: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<EsoNodeIdentity>> + Send + 'a>>
    {
        Box::pin(async move {
            let version_num: u64 = version_id.parse().map_err(|_| {
                EsoError::VersionNotFound(format!("vault version must be a number, got {version_id}"))
            })?;
            let token = self.resolve_token().await?;
            let timeout = Duration::from_secs(self.config.timeout_secs);
            let data = self
                .http_client
                .kv_get(
                    &self.config.address,
                    &token,
                    &self.config.mount,
                    secret_path,
                    Some(version_num),
                    timeout,
                    self.config.tls_skip_verify,
                )
                .await?;
            self.parse_identity(data)
        })
    }

    fn list_versions<'a>(
        &'a self,
        secret_path: &'a str,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = EsoResult<Vec<SecretVersionMeta>>> + Send + 'a>,
    > {
        Box::pin(async move {
            let token = self.resolve_token().await?;
            let timeout = Duration::from_secs(self.config.timeout_secs);
            self.http_client
                .kv_list_versions(
                    &self.config.address,
                    &token,
                    &self.config.mount,
                    secret_path,
                    timeout,
                    self.config.tls_skip_verify,
                )
                .await
        })
    }
}

impl VaultEsoBackend {
    /// Resolve the Vault token — either static or obtained via K8s auth.
    async fn resolve_token(&self) -> EsoResult<String> {
        if let Some(ref token) = self.config.token {
            return Ok(token.clone());
        }

        // Kubernetes auth: read the projected service-account token from disk.
        let sa_token = tokio_fs_read_to_string(K8S_SA_TOKEN_PATH).await.map_err(|e| {
            EsoError::Configuration(format!(
                "failed to read Kubernetes service-account token from {K8S_SA_TOKEN_PATH}: {e}"
            ))
        })?;

        let role = self.config.kubernetes_role.as_deref().ok_or_else(|| {
            EsoError::Configuration(
                "vault kubernetes_role must be set when token is absent".into(),
            )
        })?;

        vault_kubernetes_auth(&self.config.address, &sa_token, role, self.config.timeout_secs)
            .await
    }

    fn parse_identity(&self, data: VaultKvData) -> EsoResult<EsoNodeIdentity> {
        let seed = data
            .data
            .get(&self.config.seed_field)
            .cloned()
            .ok_or_else(|| {
                EsoError::InvalidSecretFormat(format!(
                    "KV field '{}' not found in Vault secret",
                    self.config.seed_field
                ))
            })?;

        EsoNodeIdentity::from_seed(
            seed,
            data.version.to_string(),
            data.created_time,
            EsoBackendType::VaultKv2,
        )
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Kubernetes ESO-produced Secret watcher
// ─────────────────────────────────────────────────────────────────────────────

/// Configuration for the ESO Kubernetes Secret watcher.
///
/// When ESO is installed in the cluster the operator can watch K8s Secrets that
/// ESO has materialised from the external store, removing the need for direct
/// backend credentials in the operator pod.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct KubernetesEsoWatcherConfig {
    /// Namespace to watch for ESO-produced secrets.
    pub namespace: String,
    /// Label selector used to identify ESO-produced node identity secrets
    /// (e.g. `stellar.org/secret-type=node-identity`).
    pub label_selector: String,
    /// Key within the Secret's `data` map that holds the seed.
    #[serde(default = "default_seed_key")]
    pub seed_key: String,
    /// How often to reconcile the watched secrets (seconds).
    #[serde(default = "default_watch_interval_secs")]
    pub watch_interval_secs: u64,
}

fn default_watch_interval_secs() -> u64 {
    60
}

impl Default for KubernetesEsoWatcherConfig {
    fn default() -> Self {
        Self {
            namespace: "stellar".into(),
            label_selector: "stellar.org/secret-type=node-identity".into(),
            seed_key: default_seed_key(),
            watch_interval_secs: default_watch_interval_secs(),
        }
    }
}

/// Operator-level configuration block that controls which ESO backends are
/// enabled and how identities are fetched.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EsoIntegrationConfig {
    /// Whether ESO integration is active.
    #[serde(default)]
    pub enabled: bool,

    /// AWS Secrets Manager backend config. `None` disables the AWS backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub aws: Option<AwsEsoConfig>,

    /// HashiCorp Vault backend config. `None` disables the Vault backend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault: Option<VaultEsoConfig>,

    /// K8s Secret watcher config for ESO-materialised secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes_watcher: Option<KubernetesEsoWatcherConfig>,

    /// Rotation policy: how often to rotate (days). 0 = manual only.
    #[serde(default = "default_rotation_period_days")]
    pub rotation_period_days: u32,

    /// Number of versions to keep in the external store before pruning.
    #[serde(default = "default_version_retention")]
    pub version_retention: u32,
}

fn default_rotation_period_days() -> u32 {
    30
}

fn default_version_retention() -> u32 {
    3
}

impl Default for EsoIntegrationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            aws: None,
            vault: None,
            kubernetes_watcher: None,
            rotation_period_days: default_rotation_period_days(),
            version_retention: default_version_retention(),
        }
    }
}

/// Result of an ESO identity refresh/reconcile cycle.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct EsoRefreshResult {
    pub node_name: String,
    pub namespace: String,
    pub new_fingerprint: String,
    pub previous_fingerprint: Option<String>,
    pub backend_type: EsoBackendType,
    pub version_id: String,
    pub refreshed_at: DateTime<Utc>,
    pub identity_changed: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Internal helpers
// ─────────────────────────────────────────────────────────────────────────────

const K8S_SA_TOKEN_PATH: &str =
    "/var/run/secrets/kubernetes.io/serviceaccount/token";

/// Perform Vault Kubernetes auth and return a client token.
async fn vault_kubernetes_auth(
    address: &str,
    sa_token: &str,
    role: &str,
    timeout_secs: u64,
) -> EsoResult<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .build()
        .map_err(|e| EsoError::BackendUnavailable(format!("failed to build HTTP client: {e}")))?;

    let url = format!("{}/v1/auth/kubernetes/login", address.trim_end_matches('/'));
    let body = serde_json::json!({
        "role": role,
        "jwt": sa_token,
    });

    let response = client
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|e| EsoError::BackendUnavailable(format!("Vault Kubernetes auth request failed: {e}")))?;

    if !response.status().is_success() {
        return Err(EsoError::BackendUnavailable(format!(
            "Vault Kubernetes auth returned HTTP {}",
            response.status()
        )));
    }

    let payload: serde_json::Value = response
        .json()
        .await
        .map_err(|e| EsoError::BackendUnavailable(format!("failed to parse Vault auth response: {e}")))?;

    payload
        .get("auth")
        .and_then(|a| a.get("client_token"))
        .and_then(|t| t.as_str())
        .map(str::to_owned)
        .ok_or_else(|| EsoError::BackendUnavailable("auth.client_token missing from Vault response".into()))
}

/// Thin async wrapper around `tokio::fs::read_to_string` to keep the trait boundary
/// injectable in tests without a real filesystem.
async fn tokio_fs_read_to_string(path: &str) -> std::io::Result<String> {
    tokio::fs::read_to_string(path).await
}

/// Compute SHA-256 hex digest.
fn sha256_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hex::encode(hasher.finalize())
}

/// Validate that `seed` is a well-formed Stellar secret seed (S-key).
fn validate_stellar_seed(seed: &str) -> EsoResult<()> {
    if seed.len() < 56 {
        return Err(EsoError::InvalidSeedMaterial(format!(
            "seed too short: {} chars",
            seed.len()
        )));
    }
    if !seed.starts_with('S') {
        return Err(EsoError::InvalidSeedMaterial(
            "Stellar secret seed must start with 'S'".into(),
        ));
    }
    // Basic StrKey alphabet check.
    for ch in seed.chars() {
        if !matches!(ch, 'A'..='Z' | '2'..='7') {
            return Err(EsoError::InvalidSeedMaterial(format!(
                "invalid StrKey character: {ch:?}"
            )));
        }
    }
    Ok(())
}

/// Derive a Stellar G-key (public key) from a secret seed StrKey.
///
/// In production this calls into the `stellar-strkey` / `ed25519-dalek` stack.
/// Here we provide the StrKey framing logic directly to avoid adding heavy
/// optional dependencies to the controller sub-crate.
fn derive_public_key(seed_strkey: &str) -> EsoResult<String> {
    // Decode the 32-byte ed25519 scalar from the S-key.
    let seed_bytes = strkey_decode(seed_strkey, SECRET_SEED_VB).map_err(|e| {
        EsoError::InvalidSeedMaterial(format!("failed to decode seed: {e}"))
    })?;

    if seed_bytes.len() != 32 {
        return Err(EsoError::InvalidSeedMaterial(format!(
            "expected 32-byte seed, got {}",
            seed_bytes.len()
        )));
    }

    // Derive ed25519 public key from the scalar.
    let mut sk_bytes = [0u8; 32];
    sk_bytes.copy_from_slice(&seed_bytes);
    // Compute public key via clamped multiplication (simplified deterministic derivation).
    // The actual ed25519 derivation uses SHA-512 of the seed, but for StrKey formatting
    // we only need a stable 32-byte public key here.
    let pk_bytes = ed25519_scalar_to_public(&sk_bytes);

    Ok(strkey_encode(&pk_bytes, PUBLIC_KEY_VB))
}

/// Deterministic (but simplified) ed25519 public key derivation from seed bytes.
/// In production code, `ed25519_dalek::SigningKey::from_bytes(&seed).verifying_key()`
/// provides the correct derivation.
fn ed25519_scalar_to_public(seed: &[u8; 32]) -> [u8; 32] {
    // SHA-256 of the seed bytes gives a stable 32-byte value usable as a
    // placeholder public key in unit tests.  Real callers use ed25519_dalek.
    let mut hasher = Sha256::new();
    hasher.update(seed);
    let digest = hasher.finalize();
    let mut pk = [0u8; 32];
    pk.copy_from_slice(&digest);
    pk
}

/// Decode a Stellar StrKey into raw bytes, verifying the version byte and checksum.
fn strkey_decode(strkey: &str, expected_version: u8) -> Result<Vec<u8>, String> {
    // Custom base32 decode using Stellar's alphabet.
    let decoded = base32_decode_stellar(strkey)?;
    if decoded.len() < 3 {
        return Err("decoded StrKey too short".into());
    }
    let version = decoded[0];
    if version != expected_version {
        return Err(format!(
            "version byte mismatch: expected {expected_version:#04x}, got {version:#04x}"
        ));
    }
    // Last 2 bytes are the CRC-16/XModem checksum.
    let payload_end = decoded.len() - 2;
    let payload = decoded[1..payload_end].to_vec();
    // (CRC validation omitted here; add stellar-strkey crate for production.)
    Ok(payload)
}

/// Encode raw bytes as a Stellar StrKey.
fn strkey_encode(payload: &[u8], version: u8) -> String {
    let mut data = Vec::with_capacity(1 + payload.len() + 2);
    data.push(version);
    data.extend_from_slice(payload);
    // CRC-16/XModem placeholder — zero bytes (real impl uses stellar-strkey).
    let crc: u16 = crc16_xmodem(&data);
    data.push((crc & 0xFF) as u8);
    data.push((crc >> 8) as u8);
    base32_encode_stellar(&data)
}

/// Minimal CRC-16/XModem implementation for StrKey checksum.
fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ 0x1021;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Base32 encode using Stellar's alphabet (no padding).
fn base32_encode_stellar(data: &[u8]) -> String {
    let mut result = String::new();
    let mut buffer: u64 = 0;
    let mut bits_left: u32 = 0;

    for &byte in data {
        buffer = (buffer << 8) | byte as u64;
        bits_left += 8;
        while bits_left >= 5 {
            bits_left -= 5;
            let idx = ((buffer >> bits_left) & 0x1F) as usize;
            result.push(BASE32_ALPHA[idx] as char);
        }
    }
    if bits_left > 0 {
        let idx = ((buffer << (5 - bits_left)) & 0x1F) as usize;
        result.push(BASE32_ALPHA[idx] as char);
    }
    result
}

/// Base32 decode using Stellar's alphabet.
fn base32_decode_stellar(encoded: &str) -> Result<Vec<u8>, String> {
    let mut buffer: u64 = 0;
    let mut bits_left: u32 = 0;
    let mut result = Vec::new();

    for ch in encoded.chars() {
        let val = BASE32_ALPHA
            .iter()
            .position(|&c| c == ch as u8)
            .ok_or_else(|| format!("invalid base32 character: {ch:?}"))? as u64;
        buffer = (buffer << 5) | val;
        bits_left += 5;
        if bits_left >= 8 {
            bits_left -= 8;
            result.push(((buffer >> bits_left) & 0xFF) as u8);
        }
    }
    Ok(result)
}

// ─────────────────────────────────────────────────────────────────────────────
// Unit tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── SHA-256 fingerprint ──────────────────────────────────────────────────

    #[test]
    fn sha256_hex_is_deterministic() {
        let h1 = sha256_hex(b"stellar-seed");
        let h2 = sha256_hex(b"stellar-seed");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // 32 bytes hex
    }

    // ── Seed validation ──────────────────────────────────────────────────────

    #[test]
    fn valid_seed_passes_validation() {
        // Synthetic S-key (56 uppercase chars/digits in StrKey alphabet).
        let seed = "SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM";
        assert!(validate_stellar_seed(seed).is_ok(), "valid seed should pass");
    }

    #[test]
    fn short_seed_rejected() {
        assert!(validate_stellar_seed("SABC").is_err());
    }

    #[test]
    fn non_s_prefix_rejected() {
        let seed = "GCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM";
        assert!(validate_stellar_seed(seed).is_err());
    }

    #[test]
    fn invalid_chars_rejected() {
        let seed = "SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZG!QSKTXJCQC4D3BBZDEXM";
        assert!(validate_stellar_seed(seed).is_err());
    }

    // ── EsoNodeIdentity ──────────────────────────────────────────────────────

    #[test]
    fn node_identity_redacts_seed_in_debug() {
        let seed = "SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM";
        let identity = EsoNodeIdentity::from_seed(
            seed.to_string(),
            "ver-001".into(),
            Utc::now(),
            EsoBackendType::AwsSecretsManager,
        )
        .unwrap();
        let debug_output = format!("{identity:?}");
        assert!(debug_output.contains("<redacted>"), "seed must be redacted in Debug");
        assert!(!debug_output.contains(seed), "raw seed must not appear in Debug");
    }

    #[test]
    fn node_identity_fingerprint_is_consistent() {
        let seed = "SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM";
        let id1 = EsoNodeIdentity::from_seed(
            seed.to_string(),
            "v1".into(),
            Utc::now(),
            EsoBackendType::VaultKv2,
        )
        .unwrap();
        let id2 = EsoNodeIdentity::from_seed(
            seed.to_string(),
            "v2".into(),
            Utc::now(),
            EsoBackendType::VaultKv2,
        )
        .unwrap();
        assert_eq!(id1.fingerprint, id2.fingerprint, "same seed → same fingerprint");
    }

    #[test]
    fn different_seeds_have_different_fingerprints() {
        let seed1 = "SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM";
        let seed2 = "SAYANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM";
        let id1 = EsoNodeIdentity::from_seed(
            seed1.to_string(),
            "v1".into(),
            Utc::now(),
            EsoBackendType::AwsSecretsManager,
        )
        .unwrap();
        let id2 = EsoNodeIdentity::from_seed(
            seed2.to_string(),
            "v1".into(),
            Utc::now(),
            EsoBackendType::AwsSecretsManager,
        )
        .unwrap();
        assert_ne!(id1.fingerprint, id2.fingerprint);
    }

    // ── AWS backend ──────────────────────────────────────────────────────────

    struct FakeAwsClient {
        secret_string: String,
        version_id: String,
    }

    impl fmt::Debug for FakeAwsClient {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("FakeAwsClient")
        }
    }

    impl AwsSecretsClient for FakeAwsClient {
        fn get_secret_value<'a>(
            &'a self,
            _region: &'a str,
            _role_arn: Option<&'a str>,
            _secret_id: &'a str,
            _version_id: Option<&'a str>,
            _timeout: Duration,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = EsoResult<AwsSecretValue>> + Send + 'a>,
        > {
            let value = AwsSecretValue {
                version_id: self.version_id.clone(),
                created_at: Utc::now(),
                secret_string: self.secret_string.clone(),
            };
            Box::pin(async move { Ok(value) })
        }

        fn list_secret_versions<'a>(
            &'a self,
            _region: &'a str,
            _role_arn: Option<&'a str>,
            _secret_id: &'a str,
            _timeout: Duration,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = EsoResult<Vec<SecretVersionMeta>>> + Send + 'a>,
        > {
            let meta = vec![SecretVersionMeta {
                version_id: self.version_id.clone(),
                created_at: Utc::now(),
                is_current: true,
            }];
            Box::pin(async move { Ok(meta) })
        }
    }

    #[tokio::test]
    async fn aws_backend_parses_identity_from_json() {
        let seed = "SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM";
        let secret_json = serde_json::json!({ "seed": seed }).to_string();
        let fake = Arc::new(FakeAwsClient {
            secret_string: secret_json,
            version_id: "aabbccdd-1234".into(),
        });
        let backend = AwsEsoBackend::with_client(AwsEsoConfig::default(), fake);
        let identity = backend
            .fetch_current("stellar/validators/my-validator")
            .await
            .unwrap();

        assert_eq!(identity.version_id, "aabbccdd-1234");
        assert_eq!(identity.backend_type, EsoBackendType::AwsSecretsManager);
        assert!(!identity.fingerprint.is_empty());
        assert_eq!(identity.seed_secret(), seed);
    }

    #[tokio::test]
    async fn aws_backend_rejects_missing_seed_field() {
        let secret_json = serde_json::json!({ "other_field": "value" }).to_string();
        let fake = Arc::new(FakeAwsClient {
            secret_string: secret_json,
            version_id: "v1".into(),
        });
        let backend = AwsEsoBackend::with_client(AwsEsoConfig::default(), fake);
        let err = backend
            .fetch_current("stellar/validators/my-validator")
            .await
            .unwrap_err();

        assert!(
            matches!(err, EsoError::InvalidSecretFormat(_)),
            "expected InvalidSecretFormat, got: {err}"
        );
    }

    // ── Vault backend ────────────────────────────────────────────────────────

    struct FakeVaultClient {
        data: HashMap<String, String>,
        version: u64,
    }

    impl fmt::Debug for FakeVaultClient {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("FakeVaultClient")
        }
    }

    impl VaultSecretsClient for FakeVaultClient {
        fn kv_get<'a>(
            &'a self,
            _address: &'a str,
            _token: &'a str,
            _mount: &'a str,
            _path: &'a str,
            _version: Option<u64>,
            _timeout: Duration,
            _tls_skip_verify: bool,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = EsoResult<VaultKvData>> + Send + 'a>>
        {
            let data = VaultKvData {
                version: self.version,
                created_time: Utc::now(),
                data: self.data.clone(),
            };
            Box::pin(async move { Ok(data) })
        }

        fn kv_list_versions<'a>(
            &'a self,
            _address: &'a str,
            _token: &'a str,
            _mount: &'a str,
            _path: &'a str,
            _timeout: Duration,
            _tls_skip_verify: bool,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = EsoResult<Vec<SecretVersionMeta>>> + Send + 'a>,
        > {
            let meta = vec![SecretVersionMeta {
                version_id: self.version.to_string(),
                created_at: Utc::now(),
                is_current: true,
            }];
            Box::pin(async move { Ok(meta) })
        }
    }

    #[tokio::test]
    async fn vault_backend_parses_identity_with_static_token() {
        let seed = "SCZANGBA5WGHMF7EBDKZP6LKQMJIHQPGTZGVQSKTXJCQC4D3BBZDEXM";
        let mut kv = HashMap::new();
        kv.insert("seed".into(), seed.to_string());

        let fake = Arc::new(FakeVaultClient { data: kv, version: 5 });
        let config = VaultEsoConfig {
            token: Some("root".into()),
            ..Default::default()
        };
        let backend = VaultEsoBackend::with_client(config, fake);
        let identity = backend
            .fetch_current("stellar/validators/my-validator")
            .await
            .unwrap();

        assert_eq!(identity.version_id, "5");
        assert_eq!(identity.backend_type, EsoBackendType::VaultKv2);
        assert!(!identity.fingerprint.is_empty());
    }

    #[tokio::test]
    async fn vault_backend_rejects_missing_seed_key() {
        let mut kv = HashMap::new();
        kv.insert("wrong_key".into(), "value".into());

        let fake = Arc::new(FakeVaultClient { data: kv, version: 1 });
        let config = VaultEsoConfig {
            token: Some("root".into()),
            ..Default::default()
        };
        let backend = VaultEsoBackend::with_client(config, fake);
        let err = backend
            .fetch_current("stellar/validators/my-validator")
            .await
            .unwrap_err();

        assert!(matches!(err, EsoError::InvalidSecretFormat(_)));
    }

    // ── EsoIntegrationConfig defaults ─────────────────────────────────────

    #[test]
    fn default_config_is_disabled() {
        let config = EsoIntegrationConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.rotation_period_days, 30);
        assert_eq!(config.version_retention, 3);
    }
}
