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

//! Exactly-once delivery semantics and duplicate rejection tests.

use event_bridge::dedup::DedupStatus;
use event_bridge::kafka_producer::{
    KafkaEventProducer, KafkaProducerConfig, ProducerError,
};
use event_bridge::models::{EventEnvelope, EventPayload, TokenTransferEvent, TransferType};
use event_bridge::xdr_transformer::{SerializationFormat, XdrTransformer};
use chrono::Utc;
use std::sync::Arc;

#[tokio::test]
async fn test_exactly_once_prevents_duplicate_events() {
    let config = KafkaProducerConfig {
        enable_idempotence: true,
        dedup_window_size: 10_000,
        ..Default::default()
    };
    let transformer = Arc::new(XdrTransformer::new(SerializationFormat::Json));
    let producer = KafkaEventProducer::new_simulated(config, transformer);

    let now = Utc::now();
    let transfer = TokenTransferEvent {
        event_id: "evt-fin-tx-001".to_string(),
        ledger_sequence: 50_000,
        tx_hash: "0xdeadbeef1234".to_string(),
        operation_index: 0,
        timestamp: now.to_rfc3339(),
        source_account: "GA5Z...".to_string(),
        from: "GA5Z...".to_string(),
        to: "GCKF...".to_string(),
        asset_type: "credit_alphanum4".to_string(),
        asset_code: "USDC".to_string(),
        asset_issuer: Some("GBBD...".to_string()),
        amount: "10000.0000000".to_string(),
        amount_stroops: 100_000_000_000,
        successful: true,
        transfer_type: TransferType::Payment,
    };

    let envelope = EventEnvelope::new(
        "token_transfer",
        "evt-fin-tx-001",
        50_000,
        &now,
        "USDC:GBBD...",
        EventPayload::TokenTransfer(transfer),
    );

    // Initial transmission: successfully published
    let report1 = producer.publish_event(&envelope).await;
    assert!(report1.is_ok());
    assert_eq!(producer.total_produced(), 1);
    assert_eq!(producer.total_duplicates_dropped(), 0);
    assert_eq!(producer.committed_watermark(), 50_000);

    // Simulated network retry / duplicate transmission: MUST be dropped
    let report2 = producer.publish_event(&envelope).await;
    assert!(matches!(report2, Err(ProducerError::DuplicateRejected(_))));
    assert_eq!(producer.total_produced(), 1);
    assert_eq!(producer.total_duplicates_dropped(), 1);

    // Third retry: also dropped
    let report3 = producer.publish_event(&envelope).await;
    assert!(matches!(report3, Err(ProducerError::DuplicateRejected(_))));
    assert_eq!(producer.total_produced(), 1);
    assert_eq!(producer.total_duplicates_dropped(), 2);

    // Downstream simulated cluster only received 1 record
    let backend = producer.simulated_backend().unwrap();
    assert_eq!(backend.total_messages("stellar.token.transfers").await, 1);
}

#[tokio::test]
async fn test_batch_transaction_atomic_dedup() {
    let config = KafkaProducerConfig::default();
    let transformer = Arc::new(XdrTransformer::new(SerializationFormat::Json));
    let producer = KafkaEventProducer::new_simulated(config, transformer);

    let now = Utc::now();
    let mut batch = Vec::new();
    for i in 0..10 {
        let event_id = format!("evt-batch-{}", i % 5); // 0..4 repeated twice
        let transfer = TokenTransferEvent {
            event_id: event_id.clone(),
            ledger_sequence: 60_000 + (i as u64),
            tx_hash: format!("tx-batch-{}", i),
            operation_index: 0,
            timestamp: now.to_rfc3339(),
            source_account: "GA5Z...".to_string(),
            from: "GA5Z...".to_string(),
            to: "GCKF...".to_string(),
            asset_type: "native".to_string(),
            asset_code: "XLM".to_string(),
            asset_issuer: None,
            amount: "1.0000000".to_string(),
            amount_stroops: 10_000_000,
            successful: true,
            transfer_type: TransferType::Payment,
        };
        batch.push(EventEnvelope::new(
            "token_transfer",
            event_id,
            60_000 + (i as u64),
            &now,
            "native",
            EventPayload::TokenTransfer(transfer),
        ));
    }

    let reports = producer.publish_batch(&batch).await.expect("batch failed");

    // Out of 10 items, exactly 5 unique were published and 5 duplicates were dropped!
    assert_eq!(reports.len(), 5);
    assert_eq!(producer.total_produced(), 5);
    assert_eq!(producer.total_duplicates_dropped(), 5);
}
