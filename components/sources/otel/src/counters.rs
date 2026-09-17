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

//! Admission counters for accepted, rejected, dropped, and expired records.

use std::sync::atomic::{AtomicU64, Ordering};

/// Process-lifetime OTLP admission counters.
#[derive(Debug, Default)]
pub struct OtelCounters {
    /// Records that produced at least one graph change.
    pub accepted: AtomicU64,
    /// Records rejected by allowlist, schema, or feedback-loop filter.
    pub rejected: AtomicU64,
    /// Records dropped because a cardinality cap was reached.
    pub dropped: AtomicU64,
    /// Graph elements deleted by the TTL sweeper.
    pub expired: AtomicU64,
}

impl OtelCounters {
    pub fn snapshot(&self) -> OtelCounterSnapshot {
        OtelCounterSnapshot {
            accepted: self.accepted.load(Ordering::Relaxed),
            rejected: self.rejected.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            expired: self.expired.load(Ordering::Relaxed),
        }
    }
}

/// Point-in-time copy of [`OtelCounters`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtelCounterSnapshot {
    pub accepted: u64,
    pub rejected: u64,
    pub dropped: u64,
    pub expired: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reads_atomics() {
        let counters = OtelCounters::default();
        counters.accepted.fetch_add(1, Ordering::Relaxed);
        counters.rejected.fetch_add(2, Ordering::Relaxed);
        counters.dropped.fetch_add(3, Ordering::Relaxed);
        counters.expired.fetch_add(4, Ordering::Relaxed);
        assert_eq!(
            counters.snapshot(),
            OtelCounterSnapshot {
                accepted: 1,
                rejected: 2,
                dropped: 3,
                expired: 4,
            }
        );
    }
}
