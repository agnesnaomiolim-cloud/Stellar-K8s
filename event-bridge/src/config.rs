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

//! Configuration options for Kafka Event Streaming Bridge.

use clap::Parser;
use serde::{Deserialize, Serialize};

use crate::kafka_producer::PartitioningStrategy;
use crate::xdr_transformer::SerializationFormat;

/// CLI command line arguments and environment configuration.
#[derive(Parser, Debug, Clone, Serialize, Deserialize)]
#[command(
    name = "event-bridge",
    about = "Kafka Event Streaming Bridge for Stellar Horizon and Captive Core",
    version
)]
pub struct BridgeConfig {
    /// Kafka bootstrap broker list.
    #[arg(
        long,
        env = "KAFKA_BOOTSTRAP_SERVERS",
        default_value = "localhost:9092"
    )]
    pub kafka_brokers: String,

    /// Kafka topic for ledger state change events.
    #[arg(
        long,
        env = "KAFKA_TOPIC_LEDGERS",
        default_value = "stellar.ledger.changes"
    )]
    pub topic_ledgers: String,

    /// Kafka topic for token transfer events.
    #[arg(
        long,
        env = "KAFKA_TOPIC_TRANSFERS",
        default_value = "stellar.token.transfers"
    )]
    pub topic_transfers: String,

    /// Kafka topic for smart contract events.
    #[arg(
        long,
        env = "KAFKA_TOPIC_CONTRACTS",
        default_value = "stellar.contract.events"
    )]
    pub topic_contracts: String,

    /// Source type: "captive-core", "redis", or "synthetic".
    #[arg(long, env = "BRIDGE_SOURCE", default_value = "captive-core")]
    pub source: String,

    /// Path to Captive Core execution stream pipe or socket.
    #[arg(
        long,
        env = "CAPTIVE_CORE_PIPE",
        default_value = "/var/run/stellar/captive-core.pipe"
    )]
    pub captive_core_pipe: String,

    /// Redis URL for Horizon cache stream.
    #[arg(
        long,
        env = "HORIZON_REDIS_URL",
        default_value = "redis://127.0.0.1:6379"
    )]
    pub horizon_redis_url: String,

    /// Serialization format: "json" or "avro".
    #[arg(long, env = "SERIALIZATION_FORMAT", default_value = "json")]
    pub format: String,

    /// Enable exactly-once delivery semantics (idempotent producer + deduplication).
    #[arg(long, env = "ENABLE_EXACTLY_ONCE", default_value = "true")]
    pub enable_exactly_once: bool,

    /// Transactional ID prefix for Kafka transactions.
    #[arg(long, env = "TRANSACTIONAL_ID", default_value = "stellar-bridge-tx")]
    pub transactional_id: Option<String>,

    /// In-memory queue capacity before applying backpressure.
    #[arg(long, env = "BUFFER_CAPACITY", default_value = "10000")]
    pub buffer_capacity: usize,

    /// Number of topic partitions for routing.
    #[arg(long, env = "NUM_PARTITIONS", default_value = "16")]
    pub num_partitions: usize,

    /// Target generation rate for synthetic/benchmarking mode (events per second).
    #[arg(long, env = "TARGET_RATE_EPS", default_value = "5000")]
    pub target_rate_eps: u64,

    /// Total events to stream (0 for continuous).
    #[arg(long, env = "MAX_EVENTS", default_value = "0")]
    pub max_events: u64,
}

impl BridgeConfig {
    pub fn serialization_format(&self) -> SerializationFormat {
        match self.format.to_lowercase().as_str() {
            "avro" => SerializationFormat::Avro,
            _ => SerializationFormat::Json,
        }
    }

    pub fn partitioning_strategy(&self) -> PartitioningStrategy {
        PartitioningStrategy::AssetOrContract
    }
}
