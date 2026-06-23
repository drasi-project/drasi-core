// Copyright 2026 The Drasi Authors.
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

//! Per-query checkpoint persistence for the gRPC reaction.
//!
//! Implements the at-least-once contract from the Reaction Recovery design: a
//! per-query `(sequence, config_hash)` checkpoint is advanced **only after a
//! batch of results has been successfully delivered (acked)**. A crash between
//! send and ack leaves the checkpoint behind, so the un-acked results replay
//! from the query outbox on restart.
//!
//! Deduplication of replayed events is **not** done here — the host
//! `ReactionManager` forwarder already filters events with
//! `sequence <= checkpoint.sequence` (seeded from the persisted checkpoint at
//! startup) before they reach the reaction's priority queue. This helper only
//! persists checkpoints, which the forwarder deliberately does not do.
//!
//! Checkpoints are persisted only when a state store is configured. The gRPC
//! reaction is non-durable (`is_durable() = false`), so without a store it is a
//! no-op and the reaction reprocesses from the start on restart.

use std::collections::HashMap;

use drasi_lib::reactions::common::base::ReactionBase;
use drasi_lib::ReactionCheckpoint;

/// Tracks the last persisted `(sequence, config_hash)` per query so checkpoint
/// advances move forward monotonically and preserve `config_hash`.
///
/// The `config_hash` is read **lazily on the first advance for a query**, not at
/// construction. The host persists the startup-seed checkpoint (carrying the
/// real `config_hash`) *after* the reaction's `start()` runs, so reading it
/// eagerly would capture a stale `config_hash` of `0` and a subsequent write
/// would clobber the seed — causing a false `config_hash` mismatch (and
/// recovery) on the next restart. Seeding lazily, after the bootstrap gate has
/// opened, picks up the correct value.
pub(crate) struct CheckpointState {
    /// Cache of the last known checkpoint per query, seeded lazily from the store.
    checkpoints: HashMap<String, ReactionCheckpoint>,
    has_store: bool,
}

impl CheckpointState {
    /// Capture whether a durable store is configured. Checkpoints are seeded
    /// lazily on first advance (see the struct docs).
    pub(crate) async fn load(base: &ReactionBase) -> Self {
        let has_store = base.state_store().await.is_some();
        Self {
            checkpoints: HashMap::new(),
            has_store,
        }
    }

    /// Seed the in-memory checkpoint for `query_id` from the store on first use,
    /// capturing the host-persisted `config_hash` and sequence baseline.
    async fn ensure_seeded(&mut self, base: &ReactionBase, query_id: &str) -> anyhow::Result<()> {
        if self.checkpoints.contains_key(query_id) {
            return Ok(());
        }
        let seed = if self.has_store {
            base.read_checkpoint(query_id).await?
        } else {
            None
        };
        self.checkpoints.insert(
            query_id.to_string(),
            seed.unwrap_or(ReactionCheckpoint {
                sequence: 0,
                config_hash: 0,
            }),
        );
        Ok(())
    }

    /// Advance `query_id`'s checkpoint to `sequence` when it moves forward,
    /// persisting it if a store is configured and preserving the host-seeded
    /// `config_hash`. Returns `Err` only when the durable write fails so the
    /// caller can apply the reaction's recovery policy.
    pub(crate) async fn advance(
        &mut self,
        base: &ReactionBase,
        query_id: &str,
        sequence: u64,
    ) -> anyhow::Result<()> {
        self.ensure_seeded(base, query_id).await?;
        let current = &self.checkpoints[query_id];
        if sequence <= current.sequence {
            return Ok(());
        }
        let cp = ReactionCheckpoint {
            sequence,
            config_hash: current.config_hash,
        };
        if self.has_store {
            base.write_checkpoint(query_id, &cp).await?;
        }
        self.checkpoints.insert(query_id.to_string(), cp);
        Ok(())
    }
}

/// Per-query checkpoint candidates for one delivered batch.
///
/// * `completed` — the max sequence whose **terminal** item (the last item of
///   its originating `QueryResult`) is in the batch. Safe to checkpoint once the
///   batch is acked: a `QueryResult` split across batches only advances once the
///   batch holding its terminal item lands.
/// * `seen` — the max sequence of **any** item, used only to advance past a
///   dropped batch under the `AutoSkipGap` policy (which accepts loss).
pub(crate) fn batch_checkpoint_candidates<I>(
    items: I,
) -> (HashMap<String, u64>, HashMap<String, u64>)
where
    I: IntoIterator<Item = (String, u64, bool)>,
{
    let mut completed: HashMap<String, u64> = HashMap::new();
    let mut seen: HashMap<String, u64> = HashMap::new();
    for (query_id, sequence, is_terminal) in items {
        let e = seen.entry(query_id.clone()).or_insert(0);
        *e = (*e).max(sequence);
        if is_terminal {
            let e = completed.entry(query_id).or_insert(0);
            *e = (*e).max(sequence);
        }
    }
    (completed, seen)
}

/// What a processing loop should do after a **sustained** delivery failure
/// (i.e. after the send retry/reconnect budget is exhausted), per the
/// reaction's recovery policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureAction {
    /// Fail-stop: set the reaction to `Error` and stop without advancing the
    /// checkpoint, so the un-acked batch replays from the outbox on restart.
    Stop,
    /// Drop the failed batch, advance past it, and continue (favor uptime).
    SkipAndContinue,
}

impl FailureAction {
    pub(crate) fn from_policy(policy: drasi_lib::ReactionRecoveryPolicy) -> Self {
        match policy {
            // Skip the gap and keep running, accepting potential loss.
            drasi_lib::ReactionRecoveryPolicy::AutoSkipGap => FailureAction::SkipAndContinue,
            // Strict — and AutoReset, which startup validation rejects for these
            // non-snapshot reactions — fail-stop to preserve at-least-once.
            _ => FailureAction::Stop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::reactions::common::base::ReactionBaseParams;

    async fn store_backed_base(id: &str) -> ReactionBase {
        let base = ReactionBase::new(ReactionBaseParams::new(id, vec!["q1".to_string()]));
        let store = std::sync::Arc::new(drasi_lib::MemoryStateStoreProvider::new());
        let (graph, _rx) = drasi_lib::component_graph::ComponentGraph::new("inst");
        let ctx = drasi_lib::context::ReactionRuntimeContext::new(
            "inst",
            id,
            Some(store),
            graph.update_sender(),
            None,
        );
        base.initialize(ctx).await;
        base
    }

    #[tokio::test]
    async fn advance_persists_monotonically_and_preserves_seeded_config_hash() {
        let base = store_backed_base("ckpt-test").await;
        // The host seeds a checkpoint with a non-zero config_hash at startup.
        base.write_checkpoint(
            "q1",
            &ReactionCheckpoint {
                sequence: 3,
                config_hash: 99,
            },
        )
        .await
        .unwrap();

        let mut state = CheckpointState::load(&base).await;

        // A forward advance persists and preserves the seeded config_hash, even
        // though `load` ran before the seed was read (lazy seeding).
        state.advance(&base, "q1", 7).await.unwrap();
        let cp = base.read_checkpoint("q1").await.unwrap().unwrap();
        assert_eq!(cp.sequence, 7);
        assert_eq!(
            cp.config_hash, 99,
            "config_hash must be preserved, not zeroed"
        );

        // A non-forward advance is a no-op.
        state.advance(&base, "q1", 5).await.unwrap();
        assert_eq!(
            base.read_checkpoint("q1").await.unwrap().unwrap().sequence,
            7
        );
    }

    #[tokio::test]
    async fn advance_without_store_is_a_noop() {
        let base = ReactionBase::new(ReactionBaseParams::new("no-store", vec!["q1".to_string()]));
        let mut state = CheckpointState::load(&base).await;
        state.advance(&base, "q1", 7).await.unwrap();
        assert!(base.read_checkpoint("q1").await.unwrap().is_none());
    }

    #[test]
    fn candidates_advance_completed_only_for_terminal_items() {
        // One query, one result split into 3 items at seq 9; terminal is last.
        let (completed, seen) = batch_checkpoint_candidates([
            ("q1".to_string(), 9, false),
            ("q1".to_string(), 9, false),
            ("q1".to_string(), 9, true),
        ]);
        assert_eq!(completed.get("q1"), Some(&9));
        assert_eq!(seen.get("q1"), Some(&9));
    }

    #[test]
    fn candidates_do_not_advance_completed_for_a_split_tail() {
        // The terminal item of seq 9 is NOT in this batch (it lands later), so
        // `completed` must not reach 9 — only `seen` does.
        let (completed, seen) = batch_checkpoint_candidates([
            ("q1".to_string(), 8, true),  // seq 8 fully delivered
            ("q1".to_string(), 9, false), // seq 9 non-terminal head only
        ]);
        assert_eq!(completed.get("q1"), Some(&8));
        assert_eq!(seen.get("q1"), Some(&9));
    }

    #[test]
    fn candidates_track_multiple_queries_independently() {
        let (completed, seen) = batch_checkpoint_candidates([
            ("q1".to_string(), 4, true),
            ("q2".to_string(), 11, true),
            ("q1".to_string(), 5, false),
        ]);
        assert_eq!(completed.get("q1"), Some(&4));
        assert_eq!(completed.get("q2"), Some(&11));
        assert_eq!(seen.get("q1"), Some(&5));
        assert_eq!(seen.get("q2"), Some(&11));
    }
}
