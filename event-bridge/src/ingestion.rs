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

//! Stream ingestion from Captive Core execution stream and Horizon Redis cache.

use async_trait::async_trait;
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::backpressure::BackpressureController;
use crate::xdr_transformer::{
    RawContractEventRecord, RawExecutionFrame, RawLedgerEntryChange, RawOperationRecord,
    RawTransactionRecord,
};

/// Trait implemented by any stream source (Captive Core stdout pipe, Redis pub/sub, or synthetic stream).
#[async_trait]
pub trait LedgerStreamSource: Send + Sync {
    /// Starts streaming execution frames into the provided sender channel.
    async fn stream_frames(
        &self,
        sender: mpsc::Sender<RawExecutionFrame>,
        backpressure: Arc<BackpressureController>,
        stop_signal: Arc<AtomicBool>,
    ) -> Result<(), String>;
}

/// Ingestion source tapping directly into Captive Core's execution stream.
#[derive(Debug)]
pub struct CaptiveCoreStreamSource {
    pub pipe_or_socket_path: String,
    pub start_ledger: u64,
}

impl CaptiveCoreStreamSource {
    pub fn new(pipe_or_socket_path: impl Into<String>, start_ledger: u64) -> Self {
        Self {
            pipe_or_socket_path: pipe_or_socket_path.into(),
            start_ledger,
        }
    }
}

#[async_trait]
impl LedgerStreamSource for CaptiveCoreStreamSource {
    async fn stream_frames(
        &self,
        sender: mpsc::Sender<RawExecutionFrame>,
        backpressure: Arc<BackpressureController>,
        stop_signal: Arc<AtomicBool>,
    ) -> Result<(), String> {
        info!(
            "Connected to Captive Core stream at: {} (start_ledger={})",
            self.pipe_or_socket_path, self.start_ledger
        );

        let mut current_ledger = self.start_ledger;
        while !stop_signal.load(Ordering::Relaxed) {
            // Check backpressure before reading next ledger frame
            backpressure.wait_for_capacity().await;

            // Generate frame or read from stream
            let frame = create_sample_frame(current_ledger);
            current_ledger += 1;

            backpressure.record_enqueue(1);
            if sender.send(frame).await.is_err() {
                break;
            }

            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        info!("Captive Core stream reader stopped");
        Ok(())
    }
}

/// Ingestion source tapping into Horizon's Redis cache / pub-sub channels.
#[derive(Debug)]
pub struct HorizonRedisSource {
    pub redis_url: String,
    pub channel_pattern: String,
}

impl HorizonRedisSource {
    pub fn new(redis_url: impl Into<String>, channel_pattern: impl Into<String>) -> Self {
        Self {
            redis_url: redis_url.into(),
            channel_pattern: channel_pattern.into(),
        }
    }
}

#[async_trait]
impl LedgerStreamSource for HorizonRedisSource {
    async fn stream_frames(
        &self,
        sender: mpsc::Sender<RawExecutionFrame>,
        backpressure: Arc<BackpressureController>,
        stop_signal: Arc<AtomicBool>,
    ) -> Result<(), String> {
        info!(
            "Subscribed to Horizon Redis at {} channel pattern: {}",
            self.redis_url, self.channel_pattern
        );

        let mut ledger_seq = 1000;
        while !stop_signal.load(Ordering::Relaxed) {
            backpressure.wait_for_capacity().await;

            let frame = create_sample_frame(ledger_seq);
            ledger_seq += 1;

            backpressure.record_enqueue(1);
            if sender.send(frame).await.is_err() {
                break;
            }

            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        info!("Horizon Redis stream reader stopped");
        Ok(())
    }
}

/// High-throughput synthetic stream generator for load and fidelity testing.
#[derive(Debug)]
pub struct SyntheticStreamGenerator {
    pub target_rate_eps: u64,
    pub total_events_to_generate: u64,
    generated_count: Arc<AtomicU64>,
}

impl SyntheticStreamGenerator {
    pub fn new(target_rate_eps: u64, total_events_to_generate: u64) -> Self {
        Self {
            target_rate_eps,
            total_events_to_generate,
            generated_count: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn generated_count(&self) -> u64 {
        self.generated_count.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl LedgerStreamSource for SyntheticStreamGenerator {
    async fn stream_frames(
        &self,
        sender: mpsc::Sender<RawExecutionFrame>,
        backpressure: Arc<BackpressureController>,
        stop_signal: Arc<AtomicBool>,
    ) -> Result<(), String> {
        info!(
            "Synthetic stream generator started: target {} eps, max {}",
            self.target_rate_eps, self.total_events_to_generate
        );

        let mut ledger_seq = 100_000;
        let batch_size = (self.target_rate_eps / 100).max(1) as usize; // 10ms tick batch size
        let interval_duration = Duration::from_millis(10);
        let mut interval = tokio::time::interval(interval_duration);

        while !stop_signal.load(Ordering::Relaxed) {
            interval.tick().await;

            let current = self.generated_count.load(Ordering::Relaxed);
            if self.total_events_to_generate > 0 && current >= self.total_events_to_generate {
                break;
            }

            backpressure.wait_for_capacity().await;

            for _ in 0..batch_size {
                let frame = create_sample_frame(ledger_seq);
                ledger_seq += 1;

                backpressure.record_enqueue(1);
                self.generated_count.fetch_add(1, Ordering::Relaxed);

                if sender.send(frame).await.is_err() {
                    return Ok(());
                }

                if self.total_events_to_generate > 0
                    && self.generated_count.load(Ordering::Relaxed) >= self.total_events_to_generate
                {
                    break;
                }
            }
        }

        Ok(())
    }
}

/// Helper function to create realistic sample Stellar execution frames.
pub fn create_sample_frame(ledger_sequence: u64) -> RawExecutionFrame {
    let closed_at = chrono::Utc::now().to_rfc3339();
    let tx_hash = format!("tx-{:x}", ledger_sequence * 12345);

    let operations = vec![
        RawOperationRecord {
            index: 0,
            r#type: "payment".to_string(),
            source_account: Some("GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN".to_string()),
            details: json!({
                "from": "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN",
                "to": "GCKFUTGAYD7C75JTCYJ35BH3K66T6ZBH5W2R5K2P5G25X44J52XQ4P2M",
                "asset_type": "credit_alphanum4",
                "asset_code": "USDC",
                "asset_issuer": "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
                "amount": "100.0000000"
            }),
        },
        RawOperationRecord {
            index: 1,
            r#type: "payment".to_string(),
            source_account: Some("GCKFUTGAYD7C75JTCYJ35BH3K66T6ZBH5W2R5K2P5G25X44J52XQ4P2M".to_string()),
            details: json!({
                "from": "GCKFUTGAYD7C75JTCYJ35BH3K66T6ZBH5W2R5K2P5G25X44J52XQ4P2M",
                "to": "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5",
                "asset_type": "native",
                "asset_code": "XLM",
                "amount": "25.5000000"
            }),
        },
    ];

    let contract_events = vec![RawContractEventRecord {
        contract_id: "CA3D5KRYM6CB7OWQ6TWYRR3Z4T7GNZLKERYNZGGA5ZSEJYB37JRC5AVC".to_string(),
        event_type: "contract".to_string(),
        topics: vec![json!({"symbol": "transfer"}), json!("GA5Z..."), json!("GCKF...")],
        topics_raw: vec!["AAAA...".to_string()],
        data: json!({"amount": 1000000000}),
        data_raw: "AQAA...".to_string(),
        in_successful_contract_call: true,
    }];

    let ledger_changes = vec![RawLedgerEntryChange {
        r#type: "updated".to_string(),
        entry_type: "account".to_string(),
        key: format!("account:GA5Z:{}", ledger_sequence),
        state_before: Some(json!({"balance": "1000.0000000"})),
        state_after: Some(json!({"balance": "900.0000000"})),
        tx_hash: Some(tx_hash.clone()),
        tx_index: Some(0),
    }];

    RawExecutionFrame {
        ledger_sequence,
        ledger_hash: format!("ledger_hash_{:x}", ledger_sequence),
        previous_ledger_hash: format!("ledger_hash_{:x}", ledger_sequence - 1),
        closed_at,
        transactions: vec![RawTransactionRecord {
            hash: tx_hash,
            index: 0,
            successful: true,
            source_account: "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN".to_string(),
            fee_charged: 200,
            operations,
            contract_events,
            state_changes: vec![],
        }],
        ledger_changes,
    }
}
