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

//! Stellar Kafka Event Streaming Bridge.
//!
//! Taps directly into Captive Core's execution stream or Horizon's Redis cache,
//! transforms XDR ledger state changes, token transfers, and smart contract events
//! into standard JSON or Avro formats, and pushes them asynchronously to Kafka
//! partitioned by asset or contract ID with exactly-once delivery guarantees.

pub mod avro;
pub mod backpressure;
pub mod config;
pub mod dedup;
pub mod fidelity;
pub mod ingestion;
pub mod kafka_producer;
pub mod models;
pub mod xdr_transformer;

pub use avro::AvroEncoder;
pub use backpressure::{BackpressureController, BackpressureState};
pub use config::BridgeConfig;
pub use dedup::{DedupStatus, EventDeduplicator};
pub use fidelity::{FidelityReport, FidelityValidator};
pub use ingestion::{
    CaptiveCoreStreamSource, HorizonRedisSource, LedgerStreamSource, SyntheticStreamGenerator,
};
pub use kafka_producer::{
    DeliveryReport, KafkaEventProducer, KafkaProducerConfig, PartitionSelector,
    PartitioningStrategy, ProducerError,
};
pub use models::{
    EventEnvelope, EventPayload, LedgerChangeType, LedgerEntryType, LedgerStateChangeEvent,
    SmartContractEvent, TokenTransferEvent, TransferType,
};
pub use xdr_transformer::{RawExecutionFrame, SerializationFormat, TransformError, XdrTransformer};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Orchestrates the event bridge pipeline:
/// Source -> Bounded Channel (with Backpressure) -> XdrTransformer -> KafkaEventProducer
pub async fn run_bridge(
    config: BridgeConfig,
    producer: Arc<KafkaEventProducer>,
    source: Box<dyn LedgerStreamSource>,
    stop_signal: Arc<AtomicBool>,
) -> Result<(), String> {
    info!("Starting Kafka Event Streaming Bridge...");
    info!("Source: {}, Brokers: {}", config.source, config.kafka_brokers);
    info!(
        "Topics - Ledgers: {}, Transfers: {}, Contracts: {}",
        config.topic_ledgers, config.topic_transfers, config.topic_contracts
    );

    let format = config.serialization_format();
    let transformer = Arc::new(XdrTransformer::new(format));
    let backpressure = Arc::new(BackpressureController::new(config.buffer_capacity));

    // Bounded channel to prevent uncontrolled memory growth
    let (tx, mut rx) = mpsc::channel::<RawExecutionFrame>(config.buffer_capacity);

    // Spawn stream source reader task
    let source_stop = Arc::clone(&stop_signal);
    let source_bp = Arc::clone(&backpressure);
    let source_handle = tokio::spawn(async move {
        if let Err(e) = source.stream_frames(tx, source_bp, source_stop).await {
            error!("Stream source error: {e}");
        }
    });

    // Worker pipeline: dequeue, transform, and publish
    let worker_producer = Arc::clone(&producer);
    let worker_transformer = Arc::clone(&transformer);
    let worker_bp = Arc::clone(&backpressure);
    let worker_stop = Arc::clone(&stop_signal);

    let worker_handle = tokio::spawn(async move {
        while !worker_stop.load(Ordering::Relaxed) {
            match tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
                Ok(Some(frame)) => {
                    match worker_transformer.transform_frame(&frame) {
                        Ok(envelopes) => {
                            if let Err(e) = worker_producer.publish_batch(&envelopes).await {
                                warn!("Failed to publish batch to Kafka: {e}");
                            }
                        }
                        Err(e) => {
                            error!("Transformation error for ledger {}: {e}", frame.ledger_sequence);
                        }
                    }
                    worker_bp.record_dequeue(1);
                }
                Ok(None) => break, // Channel closed
                Err(_) => {
                    // Timeout, continue loop
                    continue;
                }
            }
        }
    });

    // Monitoring loop for throughput and backpressure
    let mon_bp = Arc::clone(&backpressure);
    let mon_stop = Arc::clone(&stop_signal);
    let mon_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        while !mon_stop.load(Ordering::Relaxed) {
            interval.tick().await;
            let eps = mon_bp.update_throughput_sample().await;
            let occupancy = mon_bp.current_occupancy();
            let ratio = mon_bp.occupancy_ratio() * 100.0;
            if mon_bp.is_throttled() {
                warn!(
                    "[Backpressure Active] Rate: {} eps | Buffer: {}/{} ({:.1}%)",
                    eps, occupancy, mon_bp.capacity(), ratio
                );
            } else if eps > 0 {
                info!(
                    "Bridge streaming: {} eps | In-flight: {} ({:.1}%) | Total: {}",
                    eps, occupancy, ratio, mon_bp.total_produced()
                );
            }
        }
    });

    let _ = tokio::join!(source_handle, worker_handle, mon_handle);
    info!("Kafka Event Streaming Bridge stopped cleanly.");
    Ok(())
}
