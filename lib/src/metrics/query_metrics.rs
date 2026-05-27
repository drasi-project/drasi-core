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
    outbox_size: AtomicUsize,
    outbox_earliest_seq: AtomicU64,
    outbox_latest_seq: AtomicU64,
    result_seq_advances: AtomicU64,
    live_results_count: AtomicUsize,
    outer_transaction_duration_ns_last: AtomicU64,
    outer_transaction_duration_ns_max: AtomicU64,
    snapshot_fetch_count: AtomicU64,
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
            snapshot_fetch_count: AtomicU64::new(0),
        }
    }

    /// Record an outer-transaction duration (nanoseconds).
    ///
    /// Updates last and max atomically.
    pub fn record_transaction_duration_ns(&self, duration_ns: u64) {
        self.outer_transaction_duration_ns_last
            .store(duration_ns, Ordering::Relaxed);

        // Update max using compare-exchange loop
        let mut current_max = self
            .outer_transaction_duration_ns_max
            .load(Ordering::Relaxed);
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
        self.outbox_earliest_seq
            .store(earliest_seq, Ordering::Relaxed);
        self.outbox_latest_seq.store(latest_seq, Ordering::Relaxed);
    }

    /// Increment the result sequence advance counter.
    pub fn record_seq_advance(&self) {
        self.result_seq_advances.fetch_add(1, Ordering::Relaxed);
    }

    /// Store the current live results count.
    pub fn record_live_results_count(&self, count: usize) {
        self.live_results_count.store(count, Ordering::Relaxed);
    }

    /// Increment the snapshot fetch counter.
    pub fn record_snapshot_fetch(&self) {
        self.snapshot_fetch_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Load the latest outbox sequence (for checkpoint lag calculation).
    pub fn load_outbox_latest_seq(&self) -> u64 {
        self.outbox_latest_seq.load(Ordering::Relaxed)
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
            snapshot_fetch_count: self.snapshot_fetch_count.load(Ordering::Relaxed),
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
    /// Current number of entries in the outbox ring buffer.
    pub outbox_size: usize,
    /// Sequence of the oldest entry in the outbox (0 if empty).
    pub outbox_earliest_seq: u64,
    /// Sequence of the newest entry in the outbox (= `as_of_sequence`).
    pub outbox_latest_seq: u64,
    /// Number of times a new result advanced the sequence counter.
    pub result_seq_advances: u64,
    /// Current number of live (non-deleted) results tracked by the query.
    pub live_results_count: usize,
    /// Duration of the most recent outer transaction (nanoseconds).
    pub outer_transaction_duration_ns_last: u64,
    /// Maximum outer-transaction duration observed (nanoseconds).
    pub outer_transaction_duration_ns_max: u64,
    /// Number of times a reaction fetched a snapshot from this query.
    pub snapshot_fetch_count: u64,
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
        assert_eq!(snap.snapshot_fetch_count, 0);
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
                    m.record_seq_advance();
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(m.snapshot().result_seq_advances, 8000);
    }
}
