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

//! Per-source sequence-based dedup cache.
//!
//! In-memory mirror of the persistent `source_checkpoint:{source_id}` entries
//! maintained by the checkpoint-based recovery design
//! (see design doc 02 §4 — Dedup on Replay).
//!
//! Populated on startup from persisted checkpoints, and updated after each
//! successful commit. Used by the query processor loop to filter
//! already-processed events during replay (e.g., after crash recovery or when
//! a source rewinds to serve a late-joining subscriber).
//!
//! This is a pure data structure with no I/O — the processor loop owns it
//! and is single-threaded, so no synchronization is needed.

use std::collections::HashMap;

/// Per-source checkpoint cache used for dedup filtering during replay.
#[derive(Debug, Default, Clone)]
pub struct SequenceDedup {
    checkpoints: HashMap<String, u64>,
}

impl SequenceDedup {
    /// Construct a new cache seeded with the given per-source checkpoints
    /// (typically loaded from the persistent index on query startup).
    pub fn new(checkpoints: HashMap<String, u64>) -> Self {
        Self { checkpoints }
    }

    /// Returns `true` iff this event has already been processed for its source
    /// (i.e., its sequence is `<=` the stored checkpoint).
    ///
    /// Events without a sequence (`None`) always pass through — they originate
    /// from volatile sources that cannot be replayed, so dedup does not apply.
    pub fn should_skip(&self, source_id: &str, sequence: Option<u64>) -> bool {
        match sequence {
            Some(seq) => self.checkpoints.get(source_id).is_some_and(|&cp| seq <= cp),
            None => false,
        }
    }

    /// Advance the checkpoint for a source after a successful commit.
    ///
    /// Monotonic: older sequences are ignored. In practice events are
    /// processed in timestamp / sequence order, but this defensive guard
    /// means a regression cannot rewind the cache.
    pub fn advance(&mut self, source_id: &str, sequence: u64) {
        // Fast path: source already present — `get_mut` uses `Borrow<str>` so
        // no allocation occurs. Only allocate a new `String` on first-seen
        // source (bounded by the number of distinct sources).
        if let Some(entry) = self.checkpoints.get_mut(source_id) {
            if sequence > *entry {
                *entry = sequence;
            }
        } else {
            self.checkpoints.insert(source_id.to_string(), sequence);
        }
    }

    /// Get the current checkpoint for a given source, if any.
    pub fn checkpoint_for(&self, source_id: &str) -> Option<u64> {
        self.checkpoints.get(source_id).copied()
    }

    /// Borrow the full checkpoint map.
    pub fn all_checkpoints(&self) -> &HashMap<String, u64> {
        &self.checkpoints
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_dedup_passes_everything() {
        let d = SequenceDedup::default();
        assert!(!d.should_skip("s1", Some(1)));
        assert!(!d.should_skip("s1", Some(u64::MAX)));
        assert!(!d.should_skip("s1", None));
    }

    #[test]
    fn skips_events_at_or_below_checkpoint() {
        let mut checkpoints = HashMap::new();
        checkpoints.insert("s1".to_string(), 100u64);
        let d = SequenceDedup::new(checkpoints);

        assert!(d.should_skip("s1", Some(1)));
        assert!(d.should_skip("s1", Some(99)));
        assert!(d.should_skip("s1", Some(100))); // equal is a skip
    }

    #[test]
    fn passes_events_above_checkpoint() {
        let mut checkpoints = HashMap::new();
        checkpoints.insert("s1".to_string(), 100u64);
        let d = SequenceDedup::new(checkpoints);

        assert!(!d.should_skip("s1", Some(101)));
        assert!(!d.should_skip("s1", Some(u64::MAX)));
    }

    #[test]
    fn none_sequence_always_passes() {
        let mut checkpoints = HashMap::new();
        checkpoints.insert("s1".to_string(), 100u64);
        let d = SequenceDedup::new(checkpoints);

        assert!(!d.should_skip("s1", None));
    }

    #[test]
    fn unknown_source_passes() {
        let mut checkpoints = HashMap::new();
        checkpoints.insert("s1".to_string(), 100u64);
        let d = SequenceDedup::new(checkpoints);

        assert!(!d.should_skip("other", Some(1)));
        assert!(!d.should_skip("other", Some(200)));
    }

    #[test]
    fn advance_inserts_new_source() {
        let mut d = SequenceDedup::default();
        d.advance("s1", 42);
        assert_eq!(d.checkpoint_for("s1"), Some(42));
    }

    #[test]
    fn advance_updates_checkpoint() {
        let mut d = SequenceDedup::default();
        d.advance("s1", 42);
        d.advance("s1", 100);
        assert_eq!(d.checkpoint_for("s1"), Some(100));
    }

    #[test]
    fn advance_is_monotonic() {
        let mut d = SequenceDedup::default();
        d.advance("s1", 100);
        d.advance("s1", 42); // regression — should be ignored
        assert_eq!(d.checkpoint_for("s1"), Some(100));
    }

    #[test]
    fn advance_equal_is_noop() {
        let mut d = SequenceDedup::default();
        d.advance("s1", 100);
        d.advance("s1", 100);
        assert_eq!(d.checkpoint_for("s1"), Some(100));
    }

    #[test]
    fn per_source_isolation() {
        let mut d = SequenceDedup::default();
        d.advance("s1", 100);
        d.advance("s2", 50);

        assert_eq!(d.checkpoint_for("s1"), Some(100));
        assert_eq!(d.checkpoint_for("s2"), Some(50));

        assert!(d.should_skip("s1", Some(50)));
        assert!(!d.should_skip("s2", Some(75)));
    }

    #[test]
    fn all_checkpoints_returns_full_map() {
        let mut d = SequenceDedup::default();
        d.advance("a", 1);
        d.advance("b", 2);
        d.advance("c", 3);

        let map = d.all_checkpoints();
        assert_eq!(map.len(), 3);
        assert_eq!(map.get("a"), Some(&1));
        assert_eq!(map.get("b"), Some(&2));
        assert_eq!(map.get("c"), Some(&3));
    }

    #[test]
    fn checkpoint_for_unknown_returns_none() {
        let d = SequenceDedup::default();
        assert_eq!(d.checkpoint_for("missing"), None);
    }
}
