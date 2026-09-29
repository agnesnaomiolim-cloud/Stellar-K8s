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

//! Security sub-modules for the Stellar-K8s controller sub-crate.
//!
//! This module groups all security-related controller logic:
//!
//! - [`eso_integration`] — External Secrets Operator integration for pulling
//!   ed25519 node identities from AWS Secrets Manager and HashiCorp Vault.
//! - [`rotation`] — Staggered P2P node-identity rotation controller with
//!   quorum-safety guarantees, ConfigMap patching, and Horizon DB updates.

pub mod eso_integration;
pub mod rotation;

// ── Re-exports: ESO integration ──────────────────────────────────────────────

pub use eso_integration::{
    AwsEsoBackend,
    AwsEsoConfig,
    AwsSecretValue,
    AwsSecretsClient,
    EsoBackend,
    EsoBackendType,
    EsoError,
    EsoIntegrationConfig,
    EsoNodeIdentity,
    EsoRefreshResult,
    EsoResult,
    KubernetesEsoWatcherConfig,
    SecretVersionMeta,
    VaultEsoBackend,
    VaultEsoConfig,
    VaultKvData,
    VaultSecretsClient,
};

// ── Re-exports: Rotation controller ─────────────────────────────────────────

pub use rotation::{
    ClusterRotationController,
    ClusterRotationReport,
    ConfigMapOps,
    ConsensusInfo,
    HorizonDbOps,
    HorizonDbUpdateResult,
    NodeIdentityRotationConfig,
    NodeRotationOutcome,
    NodeRotationResult,
    NodeRotationWorker,
    QuorumParameters,
    RotationError,
    RotationPlanEntry,
    RotationResult,
    RotationScheduler,
    RotationStage,
    StellarCoreOps,
};
