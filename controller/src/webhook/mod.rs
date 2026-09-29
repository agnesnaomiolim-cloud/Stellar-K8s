//! Vault-backed Kubernetes Mutating Admission Webhook.
///
/// This module implements a mutating admission webhook that intercepts Stellar Core
/// validator pod creation requests and injects an init-container that
/// authenticates to HashiCorp Vault via the pod's Kubernetes Service Account
/// token. The retrieved validator seed is written into a memory-only
/// (`tmpfs`) `emptyDir` volume that is shared with the Stellar Core container.
/// The seed is never written to persistent disk and never exposed in a
/// container environment variable.

pub mod auth;
pub mod vault_injector;

pub use auth::{VaultAuthConfig, VaultAuthenticator};
pub use vault_injector::{
    AdmissionRequest, AdmissionResponse, JsonPatch, Operation,
    PatchType, VaultInjector, VaultInjectorConfig, VaultInjectorError,
	VAULT_INJECTOR_ANNOTATION_PREFIX, VAULT_INJECTOR_LABEL,
	VAULT_INJECTOR_ENABLED_ANNOTATION, VAULT_INJECTOR_VOLUME_NAME,
};
