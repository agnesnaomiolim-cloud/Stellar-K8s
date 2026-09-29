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

//! High-performance Apache Avro binary serialization and schema definitions.
//!
//! Follows the official Apache Avro 1.11+ binary specification:
//! - Zigzag varint encoding for signed integers (int and long)
//! - Length-prefixed UTF-8 strings and binary byte arrays
//! - Union tag prefixing for nullable fields (0 = null, 1 = value)
//! - Compact binary record serialization without per-message schema overhead

use crate::models::{
    EventEnvelope, EventPayload, LedgerStateChangeEvent, SmartContractEvent, TokenTransferEvent,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AvroError {
    #[error("Avro encoding error: {0}")]
    EncodingError(String),
    #[error("Avro decoding error: {0}")]
    DecodingError(String),
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
}

/// Official Avro Schema JSON definitions for enterprise Schema Registries (Confluent / Apicurio / Redpanda).
pub const EVENT_ENVELOPE_AVRO_SCHEMA: &str = r#"{
  "type": "record",
  "name": "EventEnvelope",
  "namespace": "org.stellar.streaming",
  "doc": "Standardized Stellar event envelope streamed from Horizon / Captive Core",
  "fields": [
    { "name": "schema_version", "type": "string" },
    { "name": "event_type", "type": "string" },
    { "name": "event_id", "type": "string" },
    { "name": "ledger_sequence", "type": "long" },
    { "name": "timestamp", "type": "string" },
    { "name": "timestamp_us", "type": "long" },
    { "name": "partition_key", "type": "string" },
    { "name": "payload_json", "type": "string" }
  ]
}"#;

pub const TOKEN_TRANSFER_AVRO_SCHEMA: &str = r#"{
  "type": "record",
  "name": "TokenTransferEvent",
  "namespace": "org.stellar.streaming.token",
  "doc": "Stellar token transfer event (XLM or classic asset or Soroban SAC)",
  "fields": [
    { "name": "event_id", "type": "string" },
    { "name": "ledger_sequence", "type": "long" },
    { "name": "tx_hash", "type": "string" },
    { "name": "operation_index", "type": "int" },
    { "name": "timestamp", "type": "string" },
    { "name": "source_account", "type": "string" },
    { "name": "from", "type": "string" },
    { "name": "to", "type": "string" },
    { "name": "asset_type", "type": "string" },
    { "name": "asset_code", "type": "string" },
    { "name": "asset_issuer", "type": ["null", "string"], "default": null },
    { "name": "amount", "type": "string" },
    { "name": "amount_stroops", "type": "long" },
    { "name": "successful", "type": "boolean" },
    { "name": "transfer_type", "type": "string" }
  ]
}"#;

pub const CONTRACT_EVENT_AVRO_SCHEMA: &str = r#"{
  "type": "record",
  "name": "SmartContractEvent",
  "namespace": "org.stellar.streaming.soroban",
  "doc": "Soroban smart contract execution event",
  "fields": [
    { "name": "event_id", "type": "string" },
    { "name": "ledger_sequence", "type": "long" },
    { "name": "tx_hash", "type": "string" },
    { "name": "contract_id", "type": "string" },
    { "name": "event_type", "type": "string" },
    { "name": "topics_json", "type": "string" },
    { "name": "data_json", "type": "string" },
    { "name": "in_successful_contract_call", "type": "boolean" },
    { "name": "contract_topic0", "type": ["null", "string"], "default": null }
  ]
}"#;

pub const LEDGER_CHANGE_AVRO_SCHEMA: &str = r#"{
  "type": "record",
  "name": "LedgerStateChangeEvent",
  "namespace": "org.stellar.streaming.ledger",
  "doc": "Stellar ledger state change event",
  "fields": [
    { "name": "event_id", "type": "string" },
    { "name": "ledger_sequence", "type": "long" },
    { "name": "ledger_hash", "type": "string" },
    { "name": "previous_ledger_hash", "type": "string" },
    { "name": "closed_at", "type": "string" },
    { "name": "change_type", "type": "string" },
    { "name": "entry_type", "type": "string" },
    { "name": "entry_key", "type": "string" },
    { "name": "state_before_json", "type": ["null", "string"], "default": null },
    { "name": "state_after_json", "type": ["null", "string"], "default": null },
    { "name": "tx_hash", "type": ["null", "string"], "default": null },
    { "name": "tx_index", "type": ["null", "int"], "default": null }
  ]
}"#;

/// Low-overhead Apache Avro Binary Serializer.
#[derive(Debug, Clone, Default)]
pub struct AvroEncoder;

impl AvroEncoder {
    pub fn new() -> Self {
        Self
    }

    /// Encode a boolean (1 byte).
    pub fn encode_bool(&self, val: bool, buf: &mut Vec<u8>) {
        buf.push(if val { 1 } else { 0 });
    }

    /// Encode a signed 32-bit integer with zigzag variable-length encoding.
    pub fn encode_int(&self, n: i32, buf: &mut Vec<u8>) {
        let zigzag = ((n << 1) ^ (n >> 31)) as u32;
        self.encode_varint_u32(zigzag, buf);
    }

    /// Encode a signed 64-bit integer with zigzag variable-length encoding.
    pub fn encode_long(&self, n: i64, buf: &mut Vec<u8>) {
        let zigzag = ((n << 1) ^ (n >> 63)) as u64;
        self.encode_varint_u64(zigzag, buf);
    }

    /// Encode a UTF-8 string (length as zigzag long followed by bytes).
    pub fn encode_string(&self, s: &str, buf: &mut Vec<u8>) {
        self.encode_long(s.len() as i64, buf);
        buf.extend_from_slice(s.as_bytes());
    }

    /// Encode raw byte array.
    pub fn encode_bytes(&self, bytes: &[u8], buf: &mut Vec<u8>) {
        self.encode_long(bytes.len() as i64, buf);
        buf.extend_from_slice(bytes);
    }

    /// Encode an optional string union: `["null", "string"]`.
    pub fn encode_optional_string(&self, opt: Option<&str>, buf: &mut Vec<u8>) {
        match opt {
            None => self.encode_long(0, buf), // union branch 0: null
            Some(s) => {
                self.encode_long(1, buf); // union branch 1: string
                self.encode_string(s, buf);
            }
        }
    }

    /// Encode an optional int union: `["null", "int"]`.
    pub fn encode_optional_int(&self, opt: Option<i32>, buf: &mut Vec<u8>) {
        match opt {
            None => self.encode_long(0, buf),
            Some(v) => {
                self.encode_long(1, buf);
                self.encode_int(v, buf);
            }
        }
    }

    fn encode_varint_u32(&self, mut val: u32, buf: &mut Vec<u8>) {
        while val >= 0x80 {
            buf.push(((val & 0x7F) | 0x80) as u8);
            val >>= 7;
        }
        buf.push((val & 0x7F) as u8);
    }

    fn encode_varint_u64(&self, mut val: u64, buf: &mut Vec<u8>) {
        while val >= 0x80 {
            buf.push(((val & 0x7F) | 0x80) as u8);
            val >>= 7;
        }
        buf.push((val & 0x7F) as u8);
    }

    /// Encode an EventEnvelope into standard Avro binary format.
    pub fn encode_envelope(&self, envelope: &EventEnvelope) -> Result<Vec<u8>, AvroError> {
        let mut buf = Vec::with_capacity(512);
        self.encode_string(&envelope.schema_version, &mut buf);
        self.encode_string(&envelope.event_type, &mut buf);
        self.encode_string(&envelope.event_id, &mut buf);
        self.encode_long(envelope.ledger_sequence as i64, &mut buf);
        self.encode_string(&envelope.timestamp, &mut buf);
        self.encode_long(envelope.timestamp_us as i64, &mut buf);
        self.encode_string(&envelope.partition_key, &mut buf);

        let payload_json = serde_json::to_string(&envelope.payload)
            .map_err(|e| AvroError::EncodingError(e.to_string()))?;
        self.encode_string(&payload_json, &mut buf);

        Ok(buf)
    }

    /// Encode a TokenTransferEvent into standard Avro binary format.
    pub fn encode_token_transfer(&self, event: &TokenTransferEvent) -> Result<Vec<u8>, AvroError> {
        let mut buf = Vec::with_capacity(384);
        self.encode_string(&event.event_id, &mut buf);
        self.encode_long(event.ledger_sequence as i64, &mut buf);
        self.encode_string(&event.tx_hash, &mut buf);
        self.encode_int(event.operation_index as i32, &mut buf);
        self.encode_string(&event.timestamp, &mut buf);
        self.encode_string(&event.source_account, &mut buf);
        self.encode_string(&event.from, &mut buf);
        self.encode_string(&event.to, &mut buf);
        self.encode_string(&event.asset_type, &mut buf);
        self.encode_string(&event.asset_code, &mut buf);
        self.encode_optional_string(event.asset_issuer.as_deref(), &mut buf);
        self.encode_string(&event.amount, &mut buf);
        self.encode_long(event.amount_stroops, &mut buf);
        self.encode_bool(event.successful, &mut buf);

        let transfer_type_str = serde_json::to_string(&event.transfer_type)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        self.encode_string(&transfer_type_str, &mut buf);

        Ok(buf)
    }

    /// Encode a SmartContractEvent into standard Avro binary format.
    pub fn encode_contract_event(&self, event: &SmartContractEvent) -> Result<Vec<u8>, AvroError> {
        let mut buf = Vec::with_capacity(512);
        self.encode_string(&event.event_id, &mut buf);
        self.encode_long(event.ledger_sequence as i64, &mut buf);
        self.encode_string(&event.tx_hash, &mut buf);
        self.encode_string(&event.contract_id, &mut buf);
        self.encode_string(&event.event_type, &mut buf);

        let topics_json = serde_json::to_string(&event.topics)
            .map_err(|e| AvroError::EncodingError(e.to_string()))?;
        self.encode_string(&topics_json, &mut buf);

        let data_json = serde_json::to_string(&event.data)
            .map_err(|e| AvroError::EncodingError(e.to_string()))?;
        self.encode_string(&data_json, &mut buf);

        self.encode_bool(event.in_successful_contract_call, &mut buf);
        self.encode_optional_string(event.contract_topic0.as_deref(), &mut buf);

        Ok(buf)
    }

    /// Encode a LedgerStateChangeEvent into standard Avro binary format.
    pub fn encode_ledger_change(&self, event: &LedgerStateChangeEvent) -> Result<Vec<u8>, AvroError> {
        let mut buf = Vec::with_capacity(512);
        self.encode_string(&event.event_id, &mut buf);
        self.encode_long(event.ledger_sequence as i64, &mut buf);
        self.encode_string(&event.ledger_hash, &mut buf);
        self.encode_string(&event.previous_ledger_hash, &mut buf);
        self.encode_string(&event.closed_at, &mut buf);

        let change_type_str = serde_json::to_string(&event.change_type)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        self.encode_string(&change_type_str, &mut buf);

        let entry_type_str = serde_json::to_string(&event.entry_type)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        self.encode_string(&entry_type_str, &mut buf);

        self.encode_string(&event.entry_key, &mut buf);

        let before_json = event
            .state_before
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        self.encode_optional_string(before_json.as_deref(), &mut buf);

        let after_json = event
            .state_after
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        self.encode_optional_string(after_json.as_deref(), &mut buf);

        self.encode_optional_string(event.tx_hash.as_deref(), &mut buf);
        self.encode_optional_int(event.tx_index.map(|i| i as i32), &mut buf);

        Ok(buf)
    }
}

/// Helper reader for Avro binary decoding and verification.
pub struct AvroDecoder<'a> {
    data: &'a [u8],
    offset: usize,
}

impl<'a> AvroDecoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, offset: 0 }
    }

    pub fn read_bool(&mut self) -> Result<bool, AvroError> {
        if self.offset >= self.data.len() {
            return Err(AvroError::DecodingError("Unexpected EOF reading bool".to_string()));
        }
        let b = self.data[self.offset] != 0;
        self.offset += 1;
        Ok(b)
    }

    pub fn read_long(&mut self) -> Result<i64, AvroError> {
        let mut val: u64 = 0;
        let mut shift = 0;
        loop {
            if self.offset >= self.data.len() {
                return Err(AvroError::DecodingError("Unexpected EOF reading varint long".to_string()));
            }
            let byte = self.data[self.offset];
            self.offset += 1;
            val |= ((byte & 0x7F) as u64) << shift;
            if (byte & 0x80) == 0 {
                break;
            }
            shift += 7;
            if shift >= 64 {
                return Err(AvroError::DecodingError("Varint overflow".to_string()));
            }
        }
        let unzigzag = ((val >> 1) as i64) ^ (-((val & 1) as i64));
        Ok(unzigzag)
    }

    pub fn read_int(&mut self) -> Result<i32, AvroError> {
        let l = self.read_long()?;
        Ok(l as i32)
    }

    pub fn read_string(&mut self) -> Result<String, AvroError> {
        let len = self.read_long()?;
        if len < 0 {
            return Err(AvroError::DecodingError("Negative string length".to_string()));
        }
        let len = len as usize;
        if self.offset + len > self.data.len() {
            return Err(AvroError::DecodingError("Unexpected EOF reading string".to_string()));
        }
        let s = std::str::from_utf8(&self.data[self.offset..self.offset + len])
            .map_err(|e| AvroError::DecodingError(e.to_string()))?
            .to_string();
        self.offset += len;
        Ok(s)
    }

    pub fn read_optional_string(&mut self) -> Result<Option<String>, AvroError> {
        let branch = self.read_long()?;
        match branch {
            0 => Ok(None),
            1 => Ok(Some(self.read_string()?)),
            other => Err(AvroError::DecodingError(format!("Invalid union branch index: {other}"))),
        }
    }

    pub fn decode_envelope(&mut self) -> Result<EventEnvelope, AvroError> {
        let schema_version = self.read_string()?;
        let event_type = self.read_string()?;
        let event_id = self.read_string()?;
        let ledger_sequence = self.read_long()? as u64;
        let timestamp = self.read_string()?;
        let timestamp_us = self.read_long()? as u64;
        let partition_key = self.read_string()?;
        let payload_json = self.read_string()?;
        let payload: EventPayload = serde_json::from_str(&payload_json)
            .map_err(|e| AvroError::DecodingError(e.to_string()))?;

        Ok(EventEnvelope {
            schema_version,
            event_type,
            event_id,
            ledger_sequence,
            timestamp,
            timestamp_us,
            partition_key,
            payload,
        })
    }

    pub fn decode_token_transfer(&mut self) -> Result<TokenTransferEvent, AvroError> {
        let event_id = self.read_string()?;
        let ledger_sequence = self.read_long()? as u64;
        let tx_hash = self.read_string()?;
        let operation_index = self.read_int()? as u32;
        let timestamp = self.read_string()?;
        let source_account = self.read_string()?;
        let from = self.read_string()?;
        let to = self.read_string()?;
        let asset_type = self.read_string()?;
        let asset_code = self.read_string()?;
        let asset_issuer = self.read_optional_string()?;
        let amount = self.read_string()?;
        let amount_stroops = self.read_long()?;
        let successful = self.read_bool()?;
        let transfer_type_str = self.read_string()?;
        let transfer_type = serde_json::from_str(&format!("\"{transfer_type_str}\""))
            .map_err(|e| AvroError::DecodingError(e.to_string()))?;

        Ok(TokenTransferEvent {
            event_id,
            ledger_sequence,
            tx_hash,
            operation_index,
            timestamp,
            source_account,
            from,
            to,
            asset_type,
            asset_code,
            asset_issuer,
            amount,
            amount_stroops,
            successful,
            transfer_type,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{TransferType};
    use chrono::Utc;

    #[test]
    fn test_avro_token_transfer_roundtrip() {
        let encoder = AvroEncoder::new();
        let event = TokenTransferEvent {
            event_id: "evt-001".to_string(),
            ledger_sequence: 123456,
            tx_hash: "abcd1234abcd1234".to_string(),
            operation_index: 0,
            timestamp: Utc::now().to_rfc3339(),
            source_account: "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN".to_string(),
            from: "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN".to_string(),
            to: "GCKFUTGAYD7C75JTCYJ35BH3K66T6ZBH5W2R5K2P5G25X44J52XQ4P2M".to_string(),
            asset_type: "credit_alphanum4".to_string(),
            asset_code: "USDC".to_string(),
            asset_issuer: Some("GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN".to_string()),
            amount: "100.5000000".to_string(),
            amount_stroops: 1005000000,
            successful: true,
            transfer_type: TransferType::Payment,
        };

        let encoded = encoder.encode_token_transfer(&event).expect("encode failed");
        assert!(!encoded.is_empty());

        let mut decoder = AvroDecoder::new(&encoded);
        let decoded = decoder.decode_token_transfer().expect("decode failed");
        assert_eq!(decoded, event);
    }

    #[test]
    fn test_avro_envelope_roundtrip() {
        let encoder = AvroEncoder::new();
        let now = Utc::now();
        let event = TokenTransferEvent {
            event_id: "evt-envelope-001".to_string(),
            ledger_sequence: 9999,
            tx_hash: "hash999".to_string(),
            operation_index: 1,
            timestamp: now.to_rfc3339(),
            source_account: "GA5Z...".to_string(),
            from: "GA5Z...".to_string(),
            to: "GCKF...".to_string(),
            asset_type: "native".to_string(),
            asset_code: "XLM".to_string(),
            asset_issuer: None,
            amount: "50.0000000".to_string(),
            amount_stroops: 500000000,
            successful: true,
            transfer_type: TransferType::Payment,
        };
        let envelope = EventEnvelope::new(
            "token_transfer",
            "evt-envelope-001",
            9999,
            &now,
            "native",
            EventPayload::TokenTransfer(event),
        );

        let encoded = encoder.encode_envelope(&envelope).expect("envelope encode failed");
        let mut decoder = AvroDecoder::new(&encoded);
        let decoded = decoder.decode_envelope().expect("envelope decode failed");
        assert_eq!(decoded, envelope);
    }
}
