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
//! Crate-level error type for the P2P firewall.

use thiserror::Error;

/// All errors that can be produced by the p2p-firewall crate.
#[derive(Debug, Error)]
pub enum FirewallError {
    /// Failed to open/bind a raw socket or eBPF hook.
    #[error("interceptor init failed: {0}")]
    InterceptorInit(String),

    /// The XDR analyzer encountered an irrecoverable parse failure.
    #[error("XDR parse error: {0}")]
    XdrParse(String),

    /// Ban-manager failed to apply an iptables/NetworkPolicy rule.
    #[error("ban enforcement failed for {ip}: {reason}")]
    BanEnforcement { ip: String, reason: String },

    /// Metrics HTTP server failed to start.
    #[error("metrics server error: {0}")]
    MetricsServer(String),

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    /// Unexpected internal error.
    #[error("internal error: {0}")]
    Internal(String),
}
