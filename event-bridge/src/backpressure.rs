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

//! Memory backpressure control and flow regulation.
//!
//! Enforces bounded memory utilization when streaming 5,000+ events per second.
//! Provides reactive backpressure feedback to stream consumers (Captive Core pipe / Redis)
//! to prevent memory bloat and out-of-memory panics.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Notify;

/// Current flow state of the memory buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackpressureState {
    /// Buffer is healthy (< 60% capacity). Full throughput enabled.
    Normal,
    /// Buffer utilization elevated (60% - 85%). Warning logged.
    Elevated,
    /// Buffer saturated (> 85%). Readers throttled until drained below low watermark.
    Throttled,
}

/// Dynamic backpressure controller managing stream ingestion flow.
#[derive(Debug, Clone)]
pub struct BackpressureController {
    capacity: usize,
    low_watermark: usize,
    high_watermark: usize,
    current_occupancy: Arc<AtomicUsize>,
    throttled: Arc<AtomicBool>,
    notify_drain: Arc<Notify>,
    total_ingested: Arc<AtomicU64>,
    total_produced: Arc<AtomicU64>,
    backpressure_pause_count: Arc<AtomicU64>,
    last_rate_time: Arc<tokio::sync::Mutex<Instant>>,
    last_rate_count: Arc<AtomicU64>,
    current_eps: Arc<AtomicU64>,
}

impl BackpressureController {
    /// Creates a controller with the given item capacity (e.g. 10,000 items).
    pub fn new(capacity: usize) -> Self {
        let high_watermark = (capacity as f64 * 0.85) as usize;
        let low_watermark = (capacity as f64 * 0.40) as usize;
        Self {
            capacity,
            low_watermark,
            high_watermark,
            current_occupancy: Arc::new(AtomicUsize::new(0)),
            throttled: Arc::new(AtomicBool::new(false)),
            notify_drain: Arc::new(Notify::new()),
            total_ingested: Arc::new(AtomicU64::new(0)),
            total_produced: Arc::new(AtomicU64::new(0)),
            backpressure_pause_count: Arc::new(AtomicU64::new(0)),
            last_rate_time: Arc::new(tokio::sync::Mutex::new(Instant::now())),
            last_rate_count: Arc::new(AtomicU64::new(0)),
            current_eps: Arc::new(AtomicU64::new(0)),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn current_occupancy(&self) -> usize {
        self.current_occupancy.load(Ordering::Relaxed)
    }

    pub fn occupancy_ratio(&self) -> f64 {
        self.current_occupancy() as f64 / self.capacity.max(1) as f64
    }

    pub fn state(&self) -> BackpressureState {
        let occ = self.current_occupancy();
        if occ >= self.high_watermark {
            BackpressureState::Throttled
        } else if occ >= (self.capacity as f64 * 0.60) as usize {
            BackpressureState::Elevated
        } else {
            BackpressureState::Normal
        }
    }

    pub fn is_throttled(&self) -> bool {
        self.throttled.load(Ordering::Relaxed)
    }

    /// Called before enqueueing a new event. If the buffer is saturated,
    /// this future suspends the caller until occupancy drops below the low watermark.
    pub async fn wait_for_capacity(&self) {
        if self.current_occupancy() >= self.high_watermark {
            self.throttled.store(true, Ordering::Release);
            self.backpressure_pause_count.fetch_add(1, Ordering::Relaxed);
            while self.current_occupancy() > self.low_watermark {
                self.notify_drain.notified().await;
            }
            self.throttled.store(false, Ordering::Release);
        }
    }

    /// Records an event pushed into the buffer.
    pub fn record_enqueue(&self, count: usize) {
        self.current_occupancy.fetch_add(count, Ordering::Relaxed);
        self.total_ingested.fetch_add(count as u64, Ordering::Relaxed);
        if self.current_occupancy() >= self.high_watermark {
            self.throttled.store(true, Ordering::Release);
        }
    }

    /// Records an event consumed and dispatched to Kafka.
    pub fn record_dequeue(&self, count: usize) {
        let prev = self.current_occupancy.fetch_sub(count, Ordering::Relaxed);
        self.total_produced.fetch_add(count as u64, Ordering::Relaxed);

        let new_occ = prev.saturating_sub(count);
        if self.throttled.load(Ordering::Acquire) && new_occ <= self.low_watermark {
            self.throttled.store(false, Ordering::Release);
            self.notify_drain.notify_waiters();
        }
    }

    /// Updates throughput calculation (events per second).
    pub async fn update_throughput_sample(&self) -> u64 {
        let mut last_time = self.last_rate_time.lock().await;
        let now = Instant::now();
        let elapsed = now.duration_since(*last_time);
        if elapsed >= Duration::from_millis(500) {
            let current_total = self.total_produced.load(Ordering::Relaxed);
            let prev_total = self.last_rate_count.swap(current_total, Ordering::Relaxed);
            let delta = current_total.saturating_sub(prev_total);
            let eps = (delta as f64 / elapsed.as_secs_f64()) as u64;
            self.current_eps.store(eps, Ordering::Relaxed);
            *last_time = now;
            eps
        } else {
            self.current_eps.load(Ordering::Relaxed)
        }
    }

    pub fn current_eps(&self) -> u64 {
        self.current_eps.load(Ordering::Relaxed)
    }

    pub fn total_ingested(&self) -> u64 {
        self.total_ingested.load(Ordering::Relaxed)
    }

    pub fn total_produced(&self) -> u64 {
        self.total_produced.load(Ordering::Relaxed)
    }

    pub fn backpressure_pause_count(&self) -> u64 {
        self.backpressure_pause_count.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_backpressure_thresholds() {
        let controller = BackpressureController::new(100);
        assert_eq!(controller.state(), BackpressureState::Normal);
        assert!(!controller.is_throttled());

        controller.record_enqueue(65);
        assert_eq!(controller.state(), BackpressureState::Elevated);

        controller.record_enqueue(25); // total 90 (>= 85 high watermark)
        assert_eq!(controller.state(), BackpressureState::Throttled);
        assert!(controller.is_throttled());

        // Dequeue down to 40 (low watermark)
        controller.record_dequeue(50); // now 40
        assert_eq!(controller.current_occupancy(), 40);
        assert!(!controller.is_throttled());
        assert_eq!(controller.state(), BackpressureState::Normal);
    }
}
