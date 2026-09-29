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

//! Standardized event models and schemas for Stellar ledger streaming.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Envelope wrapping any Stellar event pushed to Kafka.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    /// Schema version for downstream consumer compatibility.
    pub schema_version: String,
    /// Type of the enclosed event: "ledger_change", "token_transfer", or "contract_event".
    pub event_type: String,
    /// Deterministic unique identifier for deduplication (SHA-256 derived).
    pub event_id: String,
    /// Ledger sequence number this event occurred in.
    pub ledger_sequence: u64,
    /// ISO 8601 UTC timestamp of ledger close.
    pub timestamp: String,
    /// Microsecond epoch timestamp for low-latency time-series indexing.
    pub timestamp_us: u64,
    /// Partition key used to route to Kafka (asset identifier or contract ID).
    pub partition_key: String,
    /// Enclosed specific event payload.
    pub payload: EventPayload,
}

impl EventEnvelope {
    pub fn new(
        event_type: impl Into<String>,
        event_id: impl Into<String>,
        ledger_sequence: u64,
        timestamp: &DateTime<Utc>,
        partition_key: impl Into<String>,
        payload: EventPayload,
    ) -> Self {
        Self {
            schema_version: "1.0.0".to_string(),
            event_type: event_type.into(),
            event_id: event_id.into(),
            ledger_sequence,
            timestamp: timestamp.to_rfc3339(),
            timestamp_us: timestamp.timestamp_micros() as u64,
            partition_key: partition_key.into(),
            payload,
        }
    }
}

/// Tagged enum of all supported event payloads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum EventPayload {
    #[serde(rename = "ledger_change")]
    LedgerChange(LedgerStateChangeEvent),
    #[serde(rename = "token_transfer")]
    TokenTransfer(TokenTransferEvent),
    #[serde(rename = "contract_event")]
    ContractEvent(SmartContractEvent),
}

/// Ledger entry state mutation (created, updated, deleted, state).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LedgerStateChangeEvent {
    pub event_id: String,
    pub ledger_sequence: u64,
    pub ledger_hash: String,
    pub previous_ledger_hash: String,
    pub closed_at: String,
    pub change_type: LedgerChangeType,
    pub entry_type: LedgerEntryType,
    pub entry_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_before: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_after: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_index: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerChangeType {
    Created,
    Updated,
    Deleted,
    State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LedgerEntryType {
    Account,
    Trustline,
    Offer,
    Data,
    ClaimableBalance,
    LiquidityPool,
    ContractData,
    ContractCode,
    ConfigSetting,
    Ttl,
}

/// Standardized token transfer event (XLM, SAC tokens, Classic Assets).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenTransferEvent {
    pub event_id: String,
    pub ledger_sequence: u64,
    pub tx_hash: String,
    pub operation_index: u32,
    pub timestamp: String,
    pub source_account: String,
    pub from: String,
    pub to: String,
    pub asset_type: String,
    pub asset_code: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_issuer: Option<String>,
    pub amount: String,
    pub amount_stroops: i64,
    pub successful: bool,
    pub transfer_type: TransferType,
}

impl TokenTransferEvent {
    /// Canonical asset key used for Kafka partition routing: e.g. "native" or "USDC:GBBD..."
    pub fn asset_partition_key(&self) -> String {
        if self.asset_type == "native" || self.asset_code == "XLM" {
            "native".to_string()
        } else if let Some(ref issuer) = self.asset_issuer {
            format!("{}:{}", self.asset_code, issuer)
        } else {
            self.asset_code.clone()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransferType {
    Payment,
    PathPaymentStrictReceive,
    PathPaymentStrictSend,
    CreateAccount,
    AccountMerge,
    ClaimClaimableBalance,
    ContractTokenTransfer,
}

/// Soroban smart contract execution event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SmartContractEvent {
    pub event_id: String,
    pub ledger_sequence: u64,
    pub tx_hash: String,
    pub contract_id: String,
    pub event_type: String,
    pub topics: Vec<Value>,
    pub topics_raw: Vec<String>,
    pub data: Value,
    pub data_raw: String,
    pub in_successful_contract_call: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contract_topic0: Option<String>,
}

impl SmartContractEvent {
    /// Canonical contract key used for Kafka partition routing: contract_id (e.g. C...)
    pub fn contract_partition_key(&self) -> String {
        self.contract_id.clone()
    }
}

/// Horizon API response structures for 100% fidelity comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HorizonOperationRecord {
    pub id: String,
    pub transaction_hash: String,
    pub ledger_sequence: u64,
    pub created_at: String,
    pub source_account: String,
    pub r#type: String,
    pub successful: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amount: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub asset_issuer: Option<String>,
}
