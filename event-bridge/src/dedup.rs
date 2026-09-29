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

//! Exactly-once delivery deduplication and ledger checkpointing.
//!
//! Enterprise financial systems require strict exactly-once guarantees.
//! This module maintains a sliding deduplication window of transaction and
//! event identifiers along with a persistent ledger watermark to eliminate
//! duplicates during upstream network retries or bridge failovers.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

/// Deduplication decision for an incoming event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DedupStatus {
    /// Event has never been seen before and is accepted for publishing.
    Accepted,
    /// Event was previously committed and must be dropped to prevent duplicate processing.
    Duplicate,
}

/// Sliding window deduplication manager with watermark tracking.
#[derive(Debug)]
pub struct EventDeduplicator {
    window_capacity: usize,
    committed_ledger_watermark: AtomicU64,
    seen_events: RwLock<HashSet<String>>,
    event_order: RwLock<VecDeque<String>>,
    duplicates_detected: AtomicU64,
    events_accepted: AtomicU64,
}

impl EventDeduplicator {
    /// Creates a deduplicator with the specified window capacity (e.g. 200,000 events).
    pub fn new(window_capacity: usize) -> Self {
        Self {
            window_capacity,
            committed_ledger_watermark: AtomicU64::new(0),
            seen_events: RwLock::new(HashSet::with_capacity(window_capacity)),
            event_order: RwLock::new(VecDeque::with_capacity(window_capacity)),
            duplicates_detected: AtomicU64::new(0),
            events_accepted: AtomicU64::new(0),
        }
    }

    /// Checks and records an event ID. Returns Accepted if first seen, Duplicate otherwise.
    pub fn check_and_record(&self, event_id: &str) -> DedupStatus {
        {
            let seen = self.seen_events.read().unwrap();
            if seen.contains(event_id) {
                self.duplicates_detected.fetch_add(1, Ordering::Relaxed);
                return DedupStatus::Duplicate;
            }
        }

        let mut seen = self.seen_events.write().unwrap();
        let mut order = self.event_order.write().unwrap();

        // Double-check under write lock
        if seen.contains(event_id) {
            self.duplicates_detected.fetch_add(1, Ordering::Relaxed);
            return DedupStatus::Duplicate;
        }

        // Evict oldest if window is full
        if seen.len() >= self.window_capacity {
            if let Some(oldest) = order.pop_front() {
                seen.remove(&oldest);
            }
        }

        seen.insert(event_id.to_string());
        order.push_back(event_id.to_string());
        self.events_accepted.fetch_add(1, Ordering::Relaxed);

        DedupStatus::Accepted
    }

    /// Updates the committed ledger watermark monotonically.
    pub fn commit_ledger_watermark(&self, ledger_seq: u64) {
        let mut current = self.committed_ledger_watermark.load(Ordering::Relaxed);
        while ledger_seq > current {
            match self.committed_ledger_watermark.compare_exchange_weak(
                current,
                ledger_seq,
                Ordering::SeqCst,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    /// Returns the highest ledger watermark committed to Kafka.
    pub fn watermark(&self) -> u64 {
        self.committed_ledger_watermark.load(Ordering::Relaxed)
    }

    /// Returns total duplicates detected and dropped.
    pub fn duplicates_dropped(&self) -> u64 {
        self.duplicates_detected.load(Ordering::Relaxed)
    }

    /// Returns total new events accepted.
    pub fn accepted_count(&self) -> u64 {
        self.events_accepted.load(Ordering::Relaxed)
    }

    /// Clear seen history (e.g. for testing).
    pub fn reset(&self) {
        let mut seen = self.seen_events.write().unwrap();
        let mut order = self.event_order.write().unwrap();
        seen.clear();
        order.clear();
        self.committed_ledger_watermark.store(0, Ordering::Relaxed);
        self.duplicates_detected.store(0, Ordering::Relaxed);
        self.events_accepted.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dedup_sliding_window() {
        let dedup = EventDeduplicator::new(5);

        assert_eq!(dedup.check_and_record("evt-1"), DedupStatus::Accepted);
        assert_eq!(dedup.check_and_record("evt-2"), DedupStatus::Accepted);
        assert_eq!(dedup.check_and_record("evt-1"), DedupStatus::Duplicate);

        assert_eq!(dedup.duplicates_dropped(), 1);
        assert_eq!(dedup.accepted_count(), 2);

        // Fill window to evict evt-1
        assert_eq!(dedup.check_and_record("evt-3"), DedupStatus::Accepted);
        assert_eq!(dedup.check_and_record("evt-4"), DedupStatus::Accepted);
        assert_eq!(dedup.check_and_record("evt-5"), DedupStatus::Accepted);
        assert_eq!(dedup.check_and_record("evt-6"), DedupStatus::Accepted); // evicts evt-1

        // evt-1 was evicted from 5-item window
        assert_eq!(dedup.check_and_record("evt-1"), DedupStatus::Accepted);
    }

    #[test]
    fn test_monotonic_watermark() {
        let dedup = EventDeduplicator::new(100);
        assert_eq!(dedup.watermark(), 0);

        dedup.commit_ledger_watermark(500);
        assert_eq!(dedup.watermark(), 500);

        // Stale or lower watermark does not overwrite
        dedup.commit_ledger_watermark(499);
        assert_eq!(dedup.watermark(), 500);

        dedup.commit_ledger_watermark(501);
        assert_eq!(dedup.watermark(), 501);
    }
}
