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

//! Stellar Kafka Event Streaming Bridge Daemon.

use clap::Parser;
use event_bridge::config::BridgeConfig;
use event_bridge::ingestion::{
    CaptiveCoreStreamSource, HorizonRedisSource, LedgerStreamSource, SyntheticStreamGenerator,
};
use event_bridge::kafka_producer::{KafkaEventProducer, KafkaProducerConfig};
use event_bridge::run_bridge;
use event_bridge::xdr_transformer::XdrTransformer;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::INFO.into()),
        )
        .json()
        .init();

    let config = BridgeConfig::parse();

    info!(
        "Initializing Stellar Kafka Event Bridge v{}",
        env!("CARGO_PKG_VERSION")
    );

    let stop_signal = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop_signal);

    // Setup graceful shutdown on Ctrl-C / SIGTERM
    tokio::spawn(async move {
        if let Ok(()) = tokio::signal::ctrl_c().await {
            info!("Received shutdown signal. Stopping Kafka Event Bridge gracefully...");
            stop_clone.store(true, Ordering::Relaxed);
        }
    });

    let producer_config = KafkaProducerConfig {
        bootstrap_servers: config.kafka_brokers.clone(),
        topic_ledgers: config.topic_ledgers.clone(),
        topic_transfers: config.topic_transfers.clone(),
        topic_contracts: config.topic_contracts.clone(),
        partition_strategy: config.partitioning_strategy(),
        num_partitions: config.num_partitions,
        enable_idempotence: config.enable_exactly_once,
        transactional_id: config.transactional_id.clone(),
        linger_ms: 5,
        batch_size_bytes: 1048576,
        dedup_window_size: 200_000,
        acks: "all".to_string(),
    };

    let transformer = Arc::new(XdrTransformer::new(config.serialization_format()));
    let producer = Arc::new(KafkaEventProducer::new_simulated(
        producer_config,
        transformer,
    ));

    let source: Box<dyn LedgerStreamSource> = match config.source.as_str() {
        "redis" => Box::new(HorizonRedisSource::new(
            &config.horizon_redis_url,
            "stellar:ledger:*",
        )),
        "synthetic" => Box::new(SyntheticStreamGenerator::new(
            config.target_rate_eps,
            config.max_events,
        )),
        _ => Box::new(CaptiveCoreStreamSource::new(
            &config.captive_core_pipe,
            1,
        )),
    };

    run_bridge(config, producer, source, stop_signal).await?;
    Ok(())
}
