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

//! Transformer unit and integration tests.

use event_bridge::avro::{AvroDecoder, AvroEncoder};
use event_bridge::models::{
    EventPayload, LedgerChangeType, LedgerEntryType, TransferType,
};
use event_bridge::xdr_transformer::{
    parse_stroops, RawContractEventRecord, RawExecutionFrame, RawLedgerEntryChange,
    RawOperationRecord, RawTransactionRecord, SerializationFormat, XdrTransformer,
};
use serde_json::json;

#[test]
fn test_stroop_precision_math() {
    assert_eq!(parse_stroops("0.0000001"), 1);
    assert_eq!(parse_stroops("1.0000000"), 10_000_000);
    assert_eq!(parse_stroops("100.1234567"), 1_001_234_567);
    assert_eq!(parse_stroops("922337203685.4775807"), 9223372036854775807);
}

#[test]
fn test_transform_path_payment_strict_receive() {
    let transformer = XdrTransformer::new(SerializationFormat::Json);
    let frame = RawExecutionFrame {
        ledger_sequence: 12345,
        ledger_hash: "lh_12345".to_string(),
        previous_ledger_hash: "lh_12344".to_string(),
        closed_at: "2026-09-29T12:00:00Z".to_string(),
        transactions: vec![RawTransactionRecord {
            hash: "tx_pp_recv".to_string(),
            index: 0,
            successful: true,
            source_account: "GA5Z...".to_string(),
            fee_charged: 100,
            operations: vec![RawOperationRecord {
                index: 0,
                r#type: "path_payment_strict_receive".to_string(),
                source_account: Some("GA5Z...".to_string()),
                details: json!({
                    "from": "GA5Z...",
                    "to": "GCKF...",
                    "asset_type": "credit_alphanum12",
                    "asset_code": "EUROCLO",
                    "asset_issuer": "GBBD...",
                    "amount": "85.2500000"
                }),
            }],
            contract_events: vec![],
            state_changes: vec![],
        }],
        ledger_changes: vec![],
    };

    let envelopes = transformer.transform_frame(&frame).unwrap();
    assert_eq!(envelopes.len(), 1);
    assert_eq!(envelopes[0].partition_key, "EUROCLO:GBBD...");

    if let EventPayload::TokenTransfer(ref transfer) = envelopes[0].payload {
        assert_eq!(transfer.transfer_type, TransferType::PathPaymentStrictReceive);
        assert_eq!(transfer.amount, "85.2500000");
        assert_eq!(transfer.amount_stroops, 852_500_000);
    } else {
        panic!("Expected TokenTransfer payload");
    }
}

#[test]
fn test_transform_create_account_and_merge() {
    let transformer = XdrTransformer::new(SerializationFormat::Json);
    let frame = RawExecutionFrame {
        ledger_sequence: 12346,
        ledger_hash: "lh_12346".to_string(),
        previous_ledger_hash: "lh_12345".to_string(),
        closed_at: "2026-09-29T12:00:05Z".to_string(),
        transactions: vec![RawTransactionRecord {
            hash: "tx_ca".to_string(),
            index: 0,
            successful: true,
            source_account: "GA5Z...".to_string(),
            fee_charged: 100,
            operations: vec![
                RawOperationRecord {
                    index: 0,
                    r#type: "create_account".to_string(),
                    source_account: Some("GA5Z...".to_string()),
                    details: json!({
                        "account": "GCKF...",
                        "starting_balance": "50.0000000"
                    }),
                },
                RawOperationRecord {
                    index: 1,
                    r#type: "account_merge".to_string(),
                    source_account: Some("GCKF...".to_string()),
                    details: json!({
                        "into": "GA5Z...",
                        "amount": "49.9999900"
                    }),
                },
            ],
            contract_events: vec![],
            state_changes: vec![],
        }],
        ledger_changes: vec![],
    };

    let envelopes = transformer.transform_frame(&frame).unwrap();
    assert_eq!(envelopes.len(), 2);

    assert_eq!(envelopes[0].partition_key, "native");
    assert_eq!(envelopes[1].partition_key, "native");
}

#[test]
fn test_transform_ledger_state_changes() {
    let transformer = XdrTransformer::new(SerializationFormat::Json);
    let frame = RawExecutionFrame {
        ledger_sequence: 12347,
        ledger_hash: "lh_12347".to_string(),
        previous_ledger_hash: "lh_12346".to_string(),
        closed_at: "2026-09-29T12:00:10Z".to_string(),
        transactions: vec![],
        ledger_changes: vec![
            RawLedgerEntryChange {
                r#type: "created".to_string(),
                entry_type: "trustline".to_string(),
                key: "trustline:GA5Z:USDC".to_string(),
                state_before: None,
                state_after: Some(json!({"limit": "1000000.0000000"})),
                tx_hash: None,
                tx_index: None,
            },
            RawLedgerEntryChange {
                r#type: "deleted".to_string(),
                entry_type: "offer".to_string(),
                key: "offer:12345678".to_string(),
                state_before: Some(json!({"price": "1.0"})),
                state_after: None,
                tx_hash: None,
                tx_index: None,
            },
        ],
    };

    let envelopes = transformer.transform_frame(&frame).unwrap();
    assert_eq!(envelopes.len(), 2);

    if let EventPayload::LedgerChange(ref c1) = envelopes[0].payload {
        assert_eq!(c1.change_type, LedgerChangeType::Created);
        assert_eq!(c1.entry_type, LedgerEntryType::Trustline);
    } else {
        panic!("Expected LedgerChange");
    }

    if let EventPayload::LedgerChange(ref c2) = envelopes[1].payload {
        assert_eq!(c2.change_type, LedgerChangeType::Deleted);
        assert_eq!(c2.entry_type, LedgerEntryType::Offer);
    } else {
        panic!("Expected LedgerChange");
    }
}

#[test]
fn test_avro_binary_serialization_of_transformed_events() {
    let transformer = XdrTransformer::new(SerializationFormat::Avro);
    let frame = RawExecutionFrame {
        ledger_sequence: 12348,
        ledger_hash: "lh_12348".to_string(),
        previous_ledger_hash: "lh_12347".to_string(),
        closed_at: "2026-09-29T12:00:15Z".to_string(),
        transactions: vec![RawTransactionRecord {
            hash: "tx_avro".to_string(),
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
                    "asset_type": "native",
                    "asset_code": "XLM",
                    "amount": "12.3456700"
                }),
            }],
            contract_events: vec![],
            state_changes: vec![],
        }],
        ledger_changes: vec![],
    };

    let envelopes = transformer.transform_frame(&frame).unwrap();
    assert_eq!(envelopes.len(), 1);

    // Serialize to Avro binary
    let avro_bytes = transformer.serialize_envelope(&envelopes[0]).unwrap();
    assert!(!avro_bytes.is_empty());

    // Decode and verify
    let mut decoder = AvroDecoder::new(&avro_bytes);
    let decoded = decoder.decode_envelope().unwrap();
    assert_eq!(decoded.schema_version, "1.0.0");
    assert_eq!(decoded.event_type, "token_transfer");
    assert_eq!(decoded.partition_key, "native");
}
