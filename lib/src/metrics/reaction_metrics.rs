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

//! Per-reaction per-query metrics: checkpoint lag, dedup, gap detection, recovery.

use std::sync::atomic::{AtomicU64, Ordering};

/// Lock-free metrics for a single reaction's subscription to a single query.
///
/// Tracks checkpoint progress, deduplication, gap detection, recovery policy
/// triggers, and bootstrap API invocation counts.
#[derive(Debug)]
pub struct ReactionMetrics {
    /// Last checkpointed sequence for this query subscription.
    pub checkpoint_sequence: AtomicU64,
    /// Gap between query's `as_of_sequence` and this reaction's checkpoint.
    pub checkpoint_lag: AtomicU64,
    /// Events skipped because `result.sequence <= checkpoint.sequence`.
    pub dedup_skip_count: AtomicU64,
    /// Number of gap detections (sequence jump or broadcast lag).
    pub gap_detection_count: AtomicU64,
    /// Times Strict recovery policy was triggered.
    pub recovery_strict_count: AtomicU64,
    /// Times AutoReset recovery policy was triggered.
    pub recovery_auto_reset_count: AtomicU64,
    /// Times AutoSkipGap recovery policy was triggered.
    pub recovery_auto_skip_gap_count: AtomicU64,
    /// Number of `fetch_snapshot` invocations for this subscription.
    pub fetch_snapshot_count: AtomicU64,
    /// Number of `fetch_outbox` invocations for this subscription.
    pub fetch_outbox_count: AtomicU64,
}

impl ReactionMetrics {
    /// Create a new zeroed metrics instance.
    pub fn new() -> Self {
        Self {
            checkpoint_sequence: AtomicU64::new(0),
            checkpoint_lag: AtomicU64::new(0),
            dedup_skip_count: AtomicU64::new(0),
            gap_detection_count: AtomicU64::new(0),
            recovery_strict_count: AtomicU64::new(0),
            recovery_auto_reset_count: AtomicU64::new(0),
            recovery_auto_skip_gap_count: AtomicU64::new(0),
            fetch_snapshot_count: AtomicU64::new(0),
            fetch_outbox_count: AtomicU64::new(0),
        }
    }

    /// Record a checkpoint advance.
    pub fn record_checkpoint(&self, sequence: u64, query_latest_seq: u64) {
        self.checkpoint_sequence.store(sequence, Ordering::Relaxed);
        self.checkpoint_lag
            .store(query_latest_seq.saturating_sub(sequence), Ordering::Relaxed);
    }

    /// Record a recovery policy trigger.
    pub fn record_recovery_trigger(&self, policy: &crate::recovery::ReactionRecoveryPolicy) {
        match policy {
            crate::recovery::ReactionRecoveryPolicy::Strict => {
                self.recovery_strict_count.fetch_add(1, Ordering::Relaxed);
            }
            crate::recovery::ReactionRecoveryPolicy::AutoReset => {
                self.recovery_auto_reset_count
                    .fetch_add(1, Ordering::Relaxed);
            }
            crate::recovery::ReactionRecoveryPolicy::AutoSkipGap => {
                self.recovery_auto_skip_gap_count
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Take a point-in-time snapshot of all metrics.
    pub fn snapshot(&self) -> ReactionMetricsSnapshot {
        ReactionMetricsSnapshot {
            checkpoint_sequence: self.checkpoint_sequence.load(Ordering::Relaxed),
            checkpoint_lag: self.checkpoint_lag.load(Ordering::Relaxed),
            dedup_skip_count: self.dedup_skip_count.load(Ordering::Relaxed),
            gap_detection_count: self.gap_detection_count.load(Ordering::Relaxed),
            recovery_strict_count: self.recovery_strict_count.load(Ordering::Relaxed),
            recovery_auto_reset_count: self.recovery_auto_reset_count.load(Ordering::Relaxed),
            recovery_auto_skip_gap_count: self.recovery_auto_skip_gap_count.load(Ordering::Relaxed),
            fetch_snapshot_count: self.fetch_snapshot_count.load(Ordering::Relaxed),
            fetch_outbox_count: self.fetch_outbox_count.load(Ordering::Relaxed),
        }
    }
}

impl Default for ReactionMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Point-in-time snapshot of [`ReactionMetrics`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactionMetricsSnapshot {
    pub checkpoint_sequence: u64,
    pub checkpoint_lag: u64,
    pub dedup_skip_count: u64,
    pub gap_detection_count: u64,
    pub recovery_strict_count: u64,
    pub recovery_auto_reset_count: u64,
    pub recovery_auto_skip_gap_count: u64,
    pub fetch_snapshot_count: u64,
    pub fetch_outbox_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::ReactionRecoveryPolicy;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn new_metrics_are_zero() {
        let m = ReactionMetrics::new();
        let snap = m.snapshot();
        assert_eq!(snap.checkpoint_sequence, 0);
        assert_eq!(snap.checkpoint_lag, 0);
        assert_eq!(snap.dedup_skip_count, 0);
        assert_eq!(snap.gap_detection_count, 0);
        assert_eq!(snap.recovery_strict_count, 0);
        assert_eq!(snap.recovery_auto_reset_count, 0);
        assert_eq!(snap.recovery_auto_skip_gap_count, 0);
        assert_eq!(snap.fetch_snapshot_count, 0);
        assert_eq!(snap.fetch_outbox_count, 0);
    }

    #[test]
    fn record_checkpoint_updates_lag() {
        let m = ReactionMetrics::new();
        m.record_checkpoint(50, 100);

        let snap = m.snapshot();
        assert_eq!(snap.checkpoint_sequence, 50);
        assert_eq!(snap.checkpoint_lag, 50);
    }

    #[test]
    fn record_recovery_trigger_increments_correct_variant() {
        let m = ReactionMetrics::new();
        m.record_recovery_trigger(&ReactionRecoveryPolicy::Strict);
        m.record_recovery_trigger(&ReactionRecoveryPolicy::AutoReset);
        m.record_recovery_trigger(&ReactionRecoveryPolicy::AutoReset);
        m.record_recovery_trigger(&ReactionRecoveryPolicy::AutoSkipGap);

        let snap = m.snapshot();
        assert_eq!(snap.recovery_strict_count, 1);
        assert_eq!(snap.recovery_auto_reset_count, 2);
        assert_eq!(snap.recovery_auto_skip_gap_count, 1);
    }

    #[test]
    fn concurrent_dedup_increments_are_safe() {
        let m = Arc::new(ReactionMetrics::new());
        let mut handles = vec![];

        for _ in 0..8 {
            let m = m.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    m.dedup_skip_count.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(m.snapshot().dedup_skip_count, 8000);
    }
}
