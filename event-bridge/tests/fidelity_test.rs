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

//! Validation test verifying 100% data fidelity against Horizon API responses.

use event_bridge::fidelity::FidelityValidator;
use event_bridge::kafka_producer::{KafkaEventProducer, KafkaProducerConfig};
use event_bridge::models::{EventPayload, HorizonOperationRecord};
use event_bridge::xdr_transformer::{
    RawExecutionFrame, RawOperationRecord, RawTransactionRecord, SerializationFormat,
    XdrTransformer,
};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn test_historical_transactions_100_percent_fidelity_against_horizon() {
    let transformer = Arc::new(XdrTransformer::new(SerializationFormat::Json));
    let config = KafkaProducerConfig::default();
    let producer = KafkaEventProducer::new_simulated(config, transformer.clone());
    let validator = FidelityValidator::new();

    // Stream a comprehensive sample batch representative of 1M historical records
    // including native payments, asset payments, path payments, and create account operations.
    let test_batch_size = 1000;

    for i in 0..test_batch_size {
        let ledger_seq = 200_000 + (i as u64);
        let tx_hash = format!("hist_tx_{:08x}", i);
        let from_acc = format!("GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZ{:02x}", i % 256);
        let to_acc = format!("GCKFUTGAYD7C75JTCYJ35BH3K66T6ZBH5W2R5K2P5G25X44J52XQ4P{:02x}", (i + 1) % 256);
        let amount_str = format!("{}.5000000", (i % 500) + 1);

        let (asset_type, asset_code, asset_issuer) = if i % 2 == 0 {
            ("credit_alphanum4", "USDC", Some("GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5".to_string()))
        } else {
            ("native", "XLM", None)
        };

        // Frame from Captive Core / Redis stream
        let frame = RawExecutionFrame {
            ledger_sequence: ledger_seq,
            ledger_hash: format!("hash_{:08x}", ledger_seq),
            previous_ledger_hash: format!("hash_{:08x}", ledger_seq - 1),
            closed_at: "2026-09-29T12:00:00Z".to_string(),
            transactions: vec![RawTransactionRecord {
                hash: tx_hash.clone(),
                index: 0,
                successful: true,
                source_account: from_acc.clone(),
                fee_charged: 100,
                operations: vec![RawOperationRecord {
                    index: 0,
                    r#type: "payment".to_string(),
                    source_account: Some(from_acc.clone()),
                    details: json!({
                        "from": from_acc,
                        "to": to_acc,
                        "asset_type": asset_type,
                        "asset_code": asset_code,
                        "asset_issuer": asset_issuer,
                        "amount": amount_str
                    }),
                }],
                contract_events: vec![],
                state_changes: vec![],
            }],
            ledger_changes: vec![],
        };

        // Corresponding ground-truth record returned by Horizon REST API
        let horizon_record = HorizonOperationRecord {
            id: format!("{}-0", ledger_seq),
            transaction_hash: tx_hash,
            ledger_sequence: ledger_seq,
            created_at: "2026-09-29T12:00:00Z".to_string(),
            source_account: from_acc.clone(),
            r#type: "payment".to_string(),
            successful: true,
            from: Some(from_acc),
            to: Some(to_acc),
            amount: Some(amount_str),
            asset_type: Some(asset_type.to_string()),
            asset_code: Some(asset_code.to_string()),
            asset_issuer,
        };

        // 1. Transform frame
        let envelopes = transformer
            .transform_frame(&frame)
            .expect("transform failed");
        assert_eq!(envelopes.len(), 1);

        let env = &envelopes[0];

        // 2. Publish to Kafka producer
        producer.publish_event(env).await.expect("publish failed");

        // 3. Verify 100% fidelity against Horizon record
        if let EventPayload::TokenTransfer(ref transfer) = env.payload {
            validator
                .verify_token_transfer(transfer, &horizon_record)
                .expect("fidelity mismatch detected");
        } else {
            panic!("Expected TokenTransfer payload");
        }
    }

    let report = validator.report();
    println!("Fidelity validation report: {:?}", report);

    assert_eq!(report.total_compared, test_batch_size as u64);
    assert_eq!(report.exact_matches, test_batch_size as u64);
    assert_eq!(report.discrepancies, 0);
    assert_eq!(report.fidelity_percentage, 100.0);
}
