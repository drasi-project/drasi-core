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

//! Global lifecycle metrics: startup rejections, resets, hash mismatches.

use std::sync::atomic::{AtomicU64, Ordering};

/// Lock-free metrics for reaction lifecycle events.
///
/// Tracks startup validation rejections, AutoReset completions, and
/// configuration hash mismatches. A single instance is shared across
/// all reactions in a `ReactionManager`.
#[derive(Debug)]
pub struct LifecycleMetrics {
    /// Durable reaction rejected because no state store is configured.
    pub startup_rejection_durable_no_store: AtomicU64,
    /// Durable reaction rejected because state store is volatile.
    pub startup_rejection_durable_on_volatile: AtomicU64,
    /// `needs_snapshot_on_fresh_start + AutoSkipGap` rejected.
    pub startup_rejection_snapshot_skip_gap: AtomicU64,
    /// `!needs_snapshot_on_fresh_start + AutoReset` rejected.
    pub startup_rejection_no_snapshot_auto_reset: AtomicU64,
    /// Number of successful AutoReset recovery completions.
    pub auto_reset_completions: AtomicU64,
    /// Number of config hash mismatches detected during bootstrap.
    pub hash_mismatch_count: AtomicU64,
}

impl LifecycleMetrics {
    /// Create a new zeroed metrics instance.
    pub fn new() -> Self {
        Self {
            startup_rejection_durable_no_store: AtomicU64::new(0),
            startup_rejection_durable_on_volatile: AtomicU64::new(0),
            startup_rejection_snapshot_skip_gap: AtomicU64::new(0),
            startup_rejection_no_snapshot_auto_reset: AtomicU64::new(0),
            auto_reset_completions: AtomicU64::new(0),
            hash_mismatch_count: AtomicU64::new(0),
        }
    }

    /// Take a point-in-time snapshot of all metrics.
    pub fn snapshot(&self) -> LifecycleMetricsSnapshot {
        LifecycleMetricsSnapshot {
            startup_rejection_durable_no_store: self
                .startup_rejection_durable_no_store
                .load(Ordering::Relaxed),
            startup_rejection_durable_on_volatile: self
                .startup_rejection_durable_on_volatile
                .load(Ordering::Relaxed),
            startup_rejection_snapshot_skip_gap: self
                .startup_rejection_snapshot_skip_gap
                .load(Ordering::Relaxed),
            startup_rejection_no_snapshot_auto_reset: self
                .startup_rejection_no_snapshot_auto_reset
                .load(Ordering::Relaxed),
            auto_reset_completions: self.auto_reset_completions.load(Ordering::Relaxed),
            hash_mismatch_count: self.hash_mismatch_count.load(Ordering::Relaxed),
        }
    }
}

impl Default for LifecycleMetrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Point-in-time snapshot of [`LifecycleMetrics`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleMetricsSnapshot {
    pub startup_rejection_durable_no_store: u64,
    pub startup_rejection_durable_on_volatile: u64,
    pub startup_rejection_snapshot_skip_gap: u64,
    pub startup_rejection_no_snapshot_auto_reset: u64,
    pub auto_reset_completions: u64,
    pub hash_mismatch_count: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    #[test]
    fn new_metrics_are_zero() {
        let m = LifecycleMetrics::new();
        let snap = m.snapshot();
        assert_eq!(snap.startup_rejection_durable_no_store, 0);
        assert_eq!(snap.startup_rejection_durable_on_volatile, 0);
        assert_eq!(snap.startup_rejection_snapshot_skip_gap, 0);
        assert_eq!(snap.startup_rejection_no_snapshot_auto_reset, 0);
        assert_eq!(snap.auto_reset_completions, 0);
        assert_eq!(snap.hash_mismatch_count, 0);
    }

    #[test]
    fn increment_individual_counters() {
        let m = LifecycleMetrics::new();
        m.startup_rejection_durable_no_store
            .fetch_add(1, Ordering::Relaxed);
        m.startup_rejection_durable_on_volatile
            .fetch_add(2, Ordering::Relaxed);
        m.auto_reset_completions.fetch_add(3, Ordering::Relaxed);
        m.hash_mismatch_count.fetch_add(4, Ordering::Relaxed);

        let snap = m.snapshot();
        assert_eq!(snap.startup_rejection_durable_no_store, 1);
        assert_eq!(snap.startup_rejection_durable_on_volatile, 2);
        assert_eq!(snap.auto_reset_completions, 3);
        assert_eq!(snap.hash_mismatch_count, 4);
    }

    #[test]
    fn concurrent_increments_are_safe() {
        let m = Arc::new(LifecycleMetrics::new());
        let mut handles = vec![];

        for _ in 0..8 {
            let m = m.clone();
            handles.push(thread::spawn(move || {
                for _ in 0..1000 {
                    m.hash_mismatch_count.fetch_add(1, Ordering::Relaxed);
                }
            }));
        }

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(m.snapshot().hash_mismatch_count, 8000);
    }
}
