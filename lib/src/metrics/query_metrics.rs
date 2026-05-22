// Copyright 2025 The Drasi Authors.
//
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

//! Per-query output metrics: outbox health, sequence progression, snapshot tracking.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Lock-free metrics for a single query's output state.
///
/// Tracks outbox size/bounds, sequence advance rate, live result cardinality,
/// outer-transaction duration, and active snapshot clone lifetime.
#[derive(Debug)]
pub struct QueryOutputMetrics {
    /// Current number of entries in the outbox ring buffer.
    pub outbox_size: AtomicUsize,
    /// Sequence of the oldest entry in the outbox (0 if empty).
    pub outbox_earliest_seq: AtomicU64,
    /// Sequence of the newest entry in the outbox (= `as_of_sequence`).
    pub outbox_latest_seq: AtomicU64,
    /// Total number of sequence advances (non-empty diff emissions).
    pub result_seq_advances: AtomicU64,
    /// Current cardinality of the live results map.
    pub live_results_count: AtomicUsize,
    /// Last outer-transaction duration in nanoseconds.
    pub outer_transaction_duration_ns_last: AtomicU64,
    /// Maximum outer-transaction duration observed (nanoseconds).
    pub outer_transaction_duration_ns_max: AtomicU64,
    /// Total number of outer transactions processed.
    pub outer_transaction_count: AtomicU64,
    /// Number of active (outstanding) snapshot clones.
    pub active_snapshot_clones: AtomicUsize,
    /// Age of the oldest outstanding snapshot in milliseconds.
    pub oldest_snapshot_age_ms: AtomicU64,
    /// Mutations accumulated since the oldest snapshot was taken.
    pub mutations_since_oldest_snapshot: AtomicU64,
}

impl QueryOutputMetrics {
    /// Create a new zeroed metrics instance.
    pub fn new() -> Self {
        Self {
            outbox_size: AtomicUsize::new(0),
            outbox_earliest_seq: AtomicU64::new(0),
            outbox_latest_seq: AtomicU64::new(0),
            result_seq_advances: AtomicU64::new(0),
            live_results_count: AtomicUsize::new(0),
            outer_transaction_duration_ns_last: AtomicU64::new(0),
            outer_transaction_duration_ns_max: AtomicU64::new(0),
            outer_transaction_count: AtomicU64::new(0),
            active_snapshot_clones: AtomicUsize::new(0),
            oldest_snapshot_age_ms: AtomicU64::new(0),
            mutations_since_oldest_snapshot: AtomicU64::new(0),
        }
    }

    /// Record an outer-transaction duration (nanoseconds).
    ///
    /// Updates last, max, and count atomically.
    pub fn record_transaction_duration_ns(&self, duration_ns: u64) {
        self.outer_transaction_duration_ns_last
            .store(duration_ns, Ordering::Relaxed);
        self.outer_transaction_count
            .fetch_add(1, Ordering::Relaxed);

        // Update max using compare-exchange loop
        let mut current_max = self.outer_transaction_duration_ns_max.load(Ordering::Relaxed);
        while duration_ns > current_max {
            match self
                .outer_transaction_duration_ns_max
                .compare_exchange_weak(
                    current_max,
                    duration_ns,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                Ok(_) => break,
                Err(actual) => current_max = actual,
            }
        }
    }

    /// Update outbox metrics after a push/pop cycle.
    pub fn update_outbox(&self, size: usize, earliest_seq: u64, latest_seq: u64) {
        self.outbox_size.store(size, Ordering::Relaxed);
        self.outbox_earliest_seq.store(earliest_seq, Ordering::Relaxed);
        self.outbox_latest_seq.store(latest_seq, Ordering::Relaxed);
    }

    /// Take a point-in-time snapshot of all metrics.
    pub fn snapshot(&self) -> QueryOutputMetricsSnapshot {
        QueryOutputMetricsSnapshot {
            outbox_size: self.outbox_size.load(Ordering::Relaxed),
            outbox_earliest_seq: self.outbox_earliest_seq.load(Ordering::Relaxed),
            outbox_latest_seq: self.outbox_latest_seq.load(Ordering::Relaxed),
            result_seq_advances: self.result_seq_advances.load(Ordering::Relaxed),
            live_results_count: self.live_results_count.load(Ordering::Relaxed),
            outer_transaction_duration_ns_last: self
                .outer_transaction_duration_ns_last
                .load(Ordering::Relaxed),
            outer_transaction_duration_ns_max: self
                .outer_transaction_duration_ns_max
                .load(Ordering::Relaxed),
            outer_transaction_count: self.outer_transaction_count.load(Ordering::Relaxed),
            active_snapshot_clones: self.active_snapshot_clones.load(Ordering::Relaxed),
            oldest_snapshot_age_ms: self.oldest_snapshot_age_ms.load(Ordering::Relaxed),
            mutations_since_oldest_snapshot: self
                .mutations_since_oldest_snapshot
                .load(Ordering::Relaxed),
        }
    }
}

impl Default for QueryOutputMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Point-in-time snapshot of [`QueryOutputMetrics`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryOutputMetricsSnapshot {
    pub outbox_size: usize,
    pub outbox_earliest_seq: u64,
    pub outbox_latest_seq: u64,
    pub result_seq_advances: u64,
    pub live_results_count: usize,
    pub outer_transaction_duration_ns_last: u64,
    pub outer_transaction_duration_ns_max: u64,
    pub outer_transaction_count: u64,
    pub active_snapshot_clones: usize,
    pub oldest_snapshot_age_ms: u64,
    pub mutations_since_oldest_snapshot: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn new_metrics_are_zero() {
        let m = QueryOutputMetrics::new();
        let snap = m.snapshot();
        assert_eq!(snap.outbox_size, 0);
        assert_eq!(snap.outbox_earliest_seq, 0);
        assert_eq!(snap.outbox_latest_seq, 0);
        assert_eq!(snap.result_seq_advances, 0);
        assert_eq!(snap.live_results_count, 0);
        assert_eq!(snap.outer_transaction_duration_ns_last, 0);
        assert_eq!(snap.outer_transaction_duration_ns_max, 0);
        assert_eq!(snap.outer_transaction_count, 0);
        assert_eq!(snap.active_snapshot_clones, 0);
        assert_eq!(snap.oldest_snapshot_age_ms, 0);
        assert_eq!(snap.mutations_since_oldest_snapshot, 0);
    }

    #[test]
    fn record_transaction_duration_updates_max() {
        let m = QueryOutputMetrics::new();
        m.record_transaction_duration_ns(100);
        m.record_transaction_duration_ns(500);
        m.record_transaction_duration_ns(200);

        let snap = m.snapshot();
        assert_eq!(snap.outer_transaction_duration_ns_last, 200);
        assert_eq!(snap.outer_transaction_duration_ns_max, 500);
        assert_eq!(snap.outer_transaction_count, 3);
    }

    #[test]
    fn update_outbox_stores_values() {
        let m = QueryOutputMetrics::new();
        m.update_outbox(5, 10, 14);

        let snap = m.snapshot();
        assert_eq!(snap.outbox_size, 5);
        assert_eq!(snap.outbox_earliest_seq, 10);
        assert_eq!(snap.outbox_latest_seq, 14);
    }

    #[test]
    fn concurrent_increments_are_safe() {
        let m = Arc::new(QueryOutputMetrics::new());
        let mut handles = vec![];

        for _ in 0..8 {
            let m = m.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    m.result_seq_advances.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(m.snapshot().result_seq_advances, 8000);
    }
}
