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

//! XDR state change and execution stream transformer.
//!
//! Transforms low-level Stellar XDR ledger close metadata, transaction results,
//! and contract events from Captive Core or Horizon Redis into standardized
//! JSON or Avro representations partitioned by asset or contract ID.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::avro::AvroEncoder;
use crate::models::{
    EventEnvelope, EventPayload, LedgerChangeType, LedgerEntryType, LedgerStateChangeEvent,
    SmartContractEvent, TokenTransferEvent, TransferType,
};

#[derive(Debug, Error)]
pub enum TransformError {
    #[error("JSON serialization error: {0}")]
    JsonError(#[from] serde_json::Error),
    #[error("Avro serialization error: {0}")]
    AvroError(#[from] crate::avro::AvroError),
    #[error("Invalid XDR format: {0}")]
    InvalidXdr(String),
    #[error("Missing required field: {0}")]
    MissingField(String),
    #[error("Parse error: {0}")]
    ParseError(String),
}

/// Target output format for transformed events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SerializationFormat {
    Json,
    Avro,
}

impl Default for SerializationFormat {
    fn default() -> Self {
        Self::Json
    }
}

/// Raw representation of a transaction or ledger frame from Captive Core or Horizon Redis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawExecutionFrame {
    pub ledger_sequence: u64,
    pub ledger_hash: String,
    pub previous_ledger_hash: String,
    pub closed_at: String,
    #[serde(default)]
    pub transactions: Vec<RawTransactionRecord>,
    #[serde(default)]
    pub ledger_changes: Vec<RawLedgerEntryChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawTransactionRecord {
    pub hash: String,
    pub index: u32,
    pub successful: bool,
    pub source_account: String,
    pub fee_charged: i64,
    #[serde(default)]
    pub operations: Vec<RawOperationRecord>,
    #[serde(default)]
    pub contract_events: Vec<RawContractEventRecord>,
    #[serde(default)]
    pub state_changes: Vec<RawLedgerEntryChange>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawOperationRecord {
    pub index: u32,
    pub r#type: String,
    pub source_account: Option<String>,
    #[serde(default)]
    pub details: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawContractEventRecord {
    pub contract_id: String,
    #[serde(default = "default_contract_event_type")]
    pub event_type: String,
    #[serde(default)]
    pub topics: Vec<Value>,
    #[serde(default)]
    pub topics_raw: Vec<String>,
    #[serde(default)]
    pub data: Value,
    #[serde(default)]
    pub data_raw: String,
    #[serde(default = "default_true")]
    pub in_successful_contract_call: bool,
}

fn default_contract_event_type() -> String {
    "contract".to_string()
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawLedgerEntryChange {
    pub r#type: String, // "created", "updated", "deleted", "state"
    pub entry_type: String, // "account", "trustline", "contract_data", etc.
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_before: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_after: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_hash: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_index: Option<u32>,
}

/// The XdrTransformer turns execution frames into strongly-typed Kafka event envelopes.
#[derive(Debug, Clone)]
pub struct XdrTransformer {
    format: SerializationFormat,
    avro_encoder: AvroEncoder,
}

impl Default for XdrTransformer {
    fn default() -> Self {
        Self::new(SerializationFormat::Json)
    }
}

impl XdrTransformer {
    pub fn new(format: SerializationFormat) -> Self {
        Self {
            format,
            avro_encoder: AvroEncoder::new(),
        }
    }

    pub fn format(&self) -> SerializationFormat {
        self.format
    }

    /// Transforms an execution frame into a batch of partitioned EventEnvelopes.
    pub fn transform_frame(&self, frame: &RawExecutionFrame) -> Result<Vec<EventEnvelope>, TransformError> {
        let timestamp = DateTime::parse_from_rfc3339(&frame.closed_at)
            .map(|dt| dt.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());

        let mut envelopes = Vec::new();

        // 1. Ledger-level state changes
        for change in &frame.ledger_changes {
            let event = self.transform_ledger_change(frame.ledger_sequence, &frame.ledger_hash, &frame.previous_ledger_hash, &frame.closed_at, change)?;
            let partition_key = event.entry_key.clone();
            let event_id = event.event_id.clone();
            envelopes.push(EventEnvelope::new(
                "ledger_change",
                event_id,
                frame.ledger_sequence,
                &timestamp,
                partition_key,
                EventPayload::LedgerChange(event),
            ));
        }

        // 2. Transactions, operations, and contract events
        for tx in &frame.transactions {
            // Process operations (token transfers)
            for op in &tx.operations {
                if let Some(transfer) = self.transform_token_transfer(frame.ledger_sequence, &frame.closed_at, tx, op)? {
                    let partition_key = transfer.asset_partition_key();
                    let event_id = transfer.event_id.clone();
                    envelopes.push(EventEnvelope::new(
                        "token_transfer",
                        event_id,
                        frame.ledger_sequence,
                        &timestamp,
                        partition_key,
                        EventPayload::TokenTransfer(transfer),
                    ));
                }
            }

            // Process Soroban smart contract events
            for contract_ev in &tx.contract_events {
                let event = self.transform_contract_event(frame.ledger_sequence, &tx.hash, contract_ev)?;
                let partition_key = event.contract_partition_key();
                let event_id = event.event_id.clone();
                envelopes.push(EventEnvelope::new(
                    "contract_event",
                    event_id,
                    frame.ledger_sequence,
                    &timestamp,
                    partition_key,
                    EventPayload::ContractEvent(event),
                ));
            }

            // Process transaction-level state changes
            for change in &tx.state_changes {
                let mut c = change.clone();
                if c.tx_hash.is_none() {
                    c.tx_hash = Some(tx.hash.clone());
                }
                if c.tx_index.is_none() {
                    c.tx_index = Some(tx.index);
                }
                let event = self.transform_ledger_change(frame.ledger_sequence, &frame.ledger_hash, &frame.previous_ledger_hash, &frame.closed_at, &c)?;
                let partition_key = event.entry_key.clone();
                let event_id = event.event_id.clone();
                envelopes.push(EventEnvelope::new(
                    "ledger_change",
                    event_id,
                    frame.ledger_sequence,
                    &timestamp,
                    partition_key,
                    EventPayload::LedgerChange(event),
                ));
            }
        }

        Ok(envelopes)
    }

    /// Serializes an EventEnvelope according to configured format (JSON or Avro).
    pub fn serialize_envelope(&self, envelope: &EventEnvelope) -> Result<Vec<u8>, TransformError> {
        match self.format {
            SerializationFormat::Json => {
                let json_bytes = serde_json::to_vec(envelope)?;
                Ok(json_bytes)
            }
            SerializationFormat::Avro => {
                let avro_bytes = self.avro_encoder.encode_envelope(envelope)?;
                Ok(avro_bytes)
            }
        }
    }

    /// Extracts deterministic token transfer from operation details.
    fn transform_token_transfer(
        &self,
        ledger_seq: u64,
        closed_at: &str,
        tx: &RawTransactionRecord,
        op: &RawOperationRecord,
    ) -> Result<Option<TokenTransferEvent>, TransformError> {
        let op_type = op.r#type.as_str();
        let details = &op.details;

        let (transfer_type, from, to, asset_type, asset_code, asset_issuer, amount_str) = match op_type {
            "payment" | "Payment" => {
                let from = details.get("from").or_else(|| details.get("source_account"))
                    .and_then(Value::as_str)
                    .unwrap_or(&tx.source_account)
                    .to_string();
                let to = details.get("to").and_then(Value::as_str)
                    .unwrap_or_default().to_string();
                let asset_type = details.get("asset_type").and_then(Value::as_str)
                    .unwrap_or("native").to_string();
                let asset_code = details.get("asset_code").and_then(Value::as_str)
                    .unwrap_or("XLM").to_string();
                let asset_issuer = details.get("asset_issuer").and_then(Value::as_str).map(String::from);
                let amount = details.get("amount").and_then(Value::as_str)
                    .unwrap_or("0.0000000").to_string();
                (TransferType::Payment, from, to, asset_type, asset_code, asset_issuer, amount)
            }
            "path_payment_strict_receive" | "PathPaymentStrictReceive" => {
                let from = details.get("from").or_else(|| details.get("source_account"))
                    .and_then(Value::as_str).unwrap_or(&tx.source_account).to_string();
                let to = details.get("to").and_then(Value::as_str).unwrap_or_default().to_string();
                let asset_type = details.get("asset_type").and_then(Value::as_str).unwrap_or("native").to_string();
                let asset_code = details.get("asset_code").and_then(Value::as_str).unwrap_or("XLM").to_string();
                let asset_issuer = details.get("asset_issuer").and_then(Value::as_str).map(String::from);
                let amount = details.get("amount").and_then(Value::as_str).unwrap_or("0.0000000").to_string();
                (TransferType::PathPaymentStrictReceive, from, to, asset_type, asset_code, asset_issuer, amount)
            }
            "path_payment_strict_send" | "PathPaymentStrictSend" => {
                let from = details.get("from").or_else(|| details.get("source_account"))
                    .and_then(Value::as_str).unwrap_or(&tx.source_account).to_string();
                let to = details.get("to").and_then(Value::as_str).unwrap_or_default().to_string();
                let asset_type = details.get("dest_asset_type").or_else(|| details.get("asset_type"))
                    .and_then(Value::as_str).unwrap_or("native").to_string();
                let asset_code = details.get("dest_asset_code").or_else(|| details.get("asset_code"))
                    .and_then(Value::as_str).unwrap_or("XLM").to_string();
                let asset_issuer = details.get("dest_asset_issuer").or_else(|| details.get("asset_issuer"))
                    .and_then(Value::as_str).map(String::from);
                let amount = details.get("dest_amount").or_else(|| details.get("amount"))
                    .and_then(Value::as_str).unwrap_or("0.0000000").to_string();
                (TransferType::PathPaymentStrictSend, from, to, asset_type, asset_code, asset_issuer, amount)
            }
            "create_account" | "CreateAccount" => {
                let from = op.source_account.as_deref().unwrap_or(&tx.source_account).to_string();
                let to = details.get("account").or_else(|| details.get("funder"))
                    .and_then(Value::as_str).unwrap_or_default().to_string();
                let amount = details.get("starting_balance").and_then(Value::as_str)
                    .unwrap_or("0.0000000").to_string();
                (TransferType::CreateAccount, from, to, "native".to_string(), "XLM".to_string(), None, amount)
            }
            "account_merge" | "AccountMerge" => {
                let from = op.source_account.as_deref().unwrap_or(&tx.source_account).to_string();
                let to = details.get("into").and_then(Value::as_str).unwrap_or_default().to_string();
                let amount = details.get("amount").and_then(Value::as_str).unwrap_or("0.0000000").to_string();
                (TransferType::AccountMerge, from, to, "native".to_string(), "XLM".to_string(), None, amount)
            }
            _ => return Ok(None),
        };

        let stroops = parse_stroops(&amount_str);
        let event_id = generate_event_id("token_transfer", ledger_seq, &tx.hash, op.index);

        Ok(Some(TokenTransferEvent {
            event_id,
            ledger_sequence: ledger_seq,
            tx_hash: tx.hash.clone(),
            operation_index: op.index,
            timestamp: closed_at.to_string(),
            source_account: tx.source_account.clone(),
            from,
            to,
            asset_type,
            asset_code,
            asset_issuer,
            amount: amount_str,
            amount_stroops: stroops,
            successful: tx.successful,
            transfer_type,
        }))
    }

    /// Transforms a Soroban smart contract event record.
    fn transform_contract_event(
        &self,
        ledger_seq: u64,
        tx_hash: &str,
        event: &RawContractEventRecord,
    ) -> Result<SmartContractEvent, TransformError> {
        let topic0 = event.topics.first().and_then(|v| {
            if let Some(s) = v.as_str() {
                Some(s.to_string())
            } else if let Some(sym) = v.get("symbol").and_then(Value::as_str) {
                Some(sym.to_string())
            } else {
                None
            }
        });

        let event_id = generate_event_id("contract_event", ledger_seq, tx_hash, 0);

        Ok(SmartContractEvent {
            event_id,
            ledger_sequence: ledger_seq,
            tx_hash: tx_hash.to_string(),
            contract_id: event.contract_id.clone(),
            event_type: event.event_type.clone(),
            topics: event.topics.clone(),
            topics_raw: event.topics_raw.clone(),
            data: event.data.clone(),
            data_raw: event.data_raw.clone(),
            in_successful_contract_call: event.in_successful_contract_call,
            contract_topic0: topic0,
        })
    }

    /// Transforms raw ledger state change.
    fn transform_ledger_change(
        &self,
        ledger_seq: u64,
        ledger_hash: &str,
        previous_ledger_hash: &str,
        closed_at: &str,
        change: &RawLedgerEntryChange,
    ) -> Result<LedgerStateChangeEvent, TransformError> {
        let change_type = match change.r#type.to_lowercase().as_str() {
            "created" => LedgerChangeType::Created,
            "updated" => LedgerChangeType::Updated,
            "deleted" => LedgerChangeType::Deleted,
            _ => LedgerChangeType::State,
        };

        let entry_type = match change.entry_type.to_lowercase().as_str() {
            "account" => LedgerEntryType::Account,
            "trustline" => LedgerEntryType::Trustline,
            "offer" => LedgerEntryType::Offer,
            "data" => LedgerEntryType::Data,
            "claimable_balance" => LedgerEntryType::ClaimableBalance,
            "liquidity_pool" => LedgerEntryType::LiquidityPool,
            "contract_data" => LedgerEntryType::ContractData,
            "contract_code" => LedgerEntryType::ContractCode,
            "config_setting" => LedgerEntryType::ConfigSetting,
            _ => LedgerEntryType::Ttl,
        };

        let event_id = format!(
            "change-{}-{}-{}",
            ledger_seq,
            &change.key[..change.key.len().min(16)],
            change.r#type
        );

        Ok(LedgerStateChangeEvent {
            event_id,
            ledger_sequence: ledger_seq,
            ledger_hash: ledger_hash.to_string(),
            previous_ledger_hash: previous_ledger_hash.to_string(),
            closed_at: closed_at.to_string(),
            change_type,
            entry_type,
            entry_key: change.key.clone(),
            state_before: change.state_before.clone(),
            state_after: change.state_after.clone(),
            tx_hash: change.tx_hash.clone(),
            tx_index: change.tx_index,
        })
    }
}

/// Converts a decimal string ("10.5000000") to integer stroops (1 stroop = 10^-7).
pub fn parse_stroops(amount: &str) -> i64 {
    let parts: Vec<&str> = amount.split('.').collect();
    let integer_part: i64 = parts[0].parse().unwrap_or(0);
    let fractional_part: i64 = if parts.len() > 1 {
        let frac_str = format!("{:0<7}", parts[1]);
        frac_str[..7].parse().unwrap_or(0)
    } else {
        0
    };

    if integer_part < 0 {
        integer_part * 10_000_000 - fractional_part
    } else {
        integer_part * 10_000_000 + fractional_part
    }
}

/// Generates a deterministic SHA-256 event ID for deduplication.
pub fn generate_event_id(prefix: &str, ledger_seq: u64, tx_hash: &str, index: u32) -> String {
    let mut hasher = Sha256::new();
    hasher.update(prefix.as_bytes());
    hasher.update(ledger_seq.to_be_bytes());
    hasher.update(tx_hash.as_bytes());
    hasher.update(index.to_be_bytes());
    let hash = hasher.finalize();
    format!("{}-{}", prefix, hex::encode(&hash[..16]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_stroops() {
        assert_eq!(parse_stroops("1.0000000"), 10_000_000);
        assert_eq!(parse_stroops("100.5"), 1_005_000_000);
        assert_eq!(parse_stroops("0.0000001"), 1);
        assert_eq!(parse_stroops("500"), 5_000_000_000);
    }

    #[test]
    fn test_transform_token_payment() {
        let transformer = XdrTransformer::new(SerializationFormat::Json);
        let frame = RawExecutionFrame {
            ledger_sequence: 500000,
            ledger_hash: "hash_500000".to_string(),
            previous_ledger_hash: "hash_499999".to_string(),
            closed_at: "2026-09-29T12:00:00Z".to_string(),
            transactions: vec![RawTransactionRecord {
                hash: "tx_hash_1".to_string(),
                index: 0,
                successful: true,
                source_account: "GA5Z...".to_string(),
                fee_charged: 100,
                operations: vec![RawOperationRecord {
                    index: 0,
                    r#type: "payment".to_string(),
                    source_account: Some("GA5Z...".to_string()),
                    details: json!({
                        "from": "GA5Z...",
                        "to": "GCKF...",
                        "asset_type": "credit_alphanum4",
                        "asset_code": "USDC",
                        "asset_issuer": "GBBD...",
                        "amount": "250.7500000"
                    }),
                }],
                contract_events: vec![],
                state_changes: vec![],
            }],
            ledger_changes: vec![],
        };

        let envelopes = transformer.transform_frame(&frame).expect("transform failed");
        assert_eq!(envelopes.len(), 1);

        let env = &envelopes[0];
        assert_eq!(env.event_type, "token_transfer");
        assert_eq!(env.partition_key, "USDC:GBBD...");

        if let EventPayload::TokenTransfer(ref transfer) = env.payload {
            assert_eq!(transfer.amount, "250.7500000");
            assert_eq!(transfer.amount_stroops, 2_507_500_000);
            assert_eq!(transfer.asset_code, "USDC");
            assert_eq!(transfer.from, "GA5Z...");
            assert_eq!(transfer.to, "GCKF...");
        } else {
            panic!("Expected TokenTransfer payload");
        }
    }

    #[test]
    fn test_transform_soroban_contract_event() {
        let transformer = XdrTransformer::new(SerializationFormat::Json);
        let frame = RawExecutionFrame {
            ledger_sequence: 500001,
            ledger_hash: "hash_500001".to_string(),
            previous_ledger_hash: "hash_500000".to_string(),
            closed_at: "2026-09-29T12:00:05Z".to_string(),
            transactions: vec![RawTransactionRecord {
                hash: "tx_contract_1".to_string(),
                index: 0,
                successful: true,
                source_account: "GA5Z...".to_string(),
                fee_charged: 1500,
                operations: vec![],
                contract_events: vec![RawContractEventRecord {
                    contract_id: "CA3D5KRYM6CB7OWQ6TWYRR3Z4T7GNZLKERYNZGGA5ZSEJYB37JRC5AVC".to_string(),
                    event_type: "contract".to_string(),
                    topics: vec![json!({"symbol": "transfer"}), json!("GA5Z..."), json!("GCKF...")],
                    topics_raw: vec!["AAAA...".to_string()],
                    data: json!({"amount": 1000000000}),
                    data_raw: "AQAA...".to_string(),
                    in_successful_contract_call: true,
                }],
                state_changes: vec![],
            }],
            ledger_changes: vec![],
        };

        let envelopes = transformer.transform_frame(&frame).expect("transform failed");
        assert_eq!(envelopes.len(), 1);

        let env = &envelopes[0];
        assert_eq!(env.event_type, "contract_event");
        assert_eq!(env.partition_key, "CA3D5KRYM6CB7OWQ6TWYRR3Z4T7GNZLKERYNZGGA5ZSEJYB37JRC5AVC");

        if let EventPayload::ContractEvent(ref ce) = env.payload {
            assert_eq!(ce.contract_id, "CA3D5KRYM6CB7OWQ6TWYRR3Z4T7GNZLKERYNZGGA5ZSEJYB37JRC5AVC");
            assert_eq!(ce.contract_topic0, Some("transfer".to_string()));
            assert!(ce.in_successful_contract_call);
        } else {
            panic!("Expected ContractEvent payload");
        }
    }
}
