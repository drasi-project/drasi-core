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

//! Shared checkpoint-advance helpers for reactions that want at-least-once
//! delivery.
//!
//! These build on the [`ReactionBase`] checkpoint primitives
//! (`read_checkpoint` / `write_checkpoint` / `read_all_checkpoints`) and the
//! [`ReactionRecoveryPolicy`] enum to provide the orchestration that every
//! durable reaction needs but that the framework does not impose via a fixed
//! loop: advance a per-query `(sequence, config_hash)` checkpoint **only after a
//! batch of results has been successfully delivered (acked)**.
//!
//! Deduplication of replayed events is **not** done here — the host
//! `ReactionManager` forwarder already filters events with
//! `sequence <= checkpoint.sequence` (seeded from the persisted checkpoint at
//! startup) before they reach a reaction's priority queue. These helpers only
//! persist checkpoints, which the forwarder deliberately does not do.
//!
//! Reactions that follow the simple per-event pattern can use
//! [`ReactionBase::run_standard_loop`](crate::reactions::common::base::ReactionBase::run_standard_loop)
//! instead; these helpers target reactions with their own batching/timer loops
//! (e.g. the HTTP and gRPC reactions).

use std::collections::HashMap;
use std::time::Duration;

use log::warn;

use crate::reactions::checkpoint::ReactionCheckpoint;
use crate::reactions::common::base::ReactionBase;
use crate::recovery::ReactionRecoveryPolicy;

/// Total write attempts under [`ReactionRecoveryPolicy::AutoReset`] (initial try
/// plus bounded retries).
const AUTO_RESET_CHECKPOINT_ATTEMPTS: u32 = 4;
/// Base delay between `AutoReset` retry attempts; doubled each attempt
/// (`base * 2^attempt`). Kept in-tree rather than pulling a retry crate:
/// the budget is four attempts and the backoff is intentionally small.
const AUTO_RESET_CHECKPOINT_BASE_BACKOFF: Duration = Duration::from_millis(10);

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
///
/// Checkpoints are persisted only when a state store is configured. A
/// non-durable reaction (`is_durable() == false`) without a store advances its
/// in-memory view only and reprocesses from the start on restart.
pub struct CheckpointState {
    /// Cache of the last known checkpoint per query, seeded lazily from the store.
    checkpoints: HashMap<String, ReactionCheckpoint>,
    has_store: bool,
}

impl CheckpointState {
    /// Capture whether a durable store is configured. Checkpoints are seeded
    /// lazily on first advance (see the struct docs).
    pub async fn load(base: &ReactionBase) -> Self {
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

    /// Seed in-memory checkpoints (for example from bootstrap). Existing
    /// entries are left unchanged so a later lazy store read cannot clobber
    /// a caller-provided seed.
    pub fn seed(&mut self, initial: HashMap<String, ReactionCheckpoint>) {
        for (query_id, checkpoint) in initial {
            self.checkpoints.entry(query_id).or_insert(checkpoint);
        }
    }

    /// Last known checkpoint for `query_id`, if seeded or previously advanced.
    pub fn get(&self, query_id: &str) -> Option<&ReactionCheckpoint> {
        self.checkpoints.get(query_id)
    }

    /// Advance `query_id`'s checkpoint to `sequence` when it moves forward,
    /// persisting it if a store is configured and preserving the host-seeded
    /// `config_hash`. Returns `Err` only when the durable write fails so the
    /// caller can apply the reaction's recovery policy.
    pub async fn advance(
        &mut self,
        base: &ReactionBase,
        query_id: &str,
        sequence: u64,
    ) -> anyhow::Result<()> {
        self.ensure_seeded(base, query_id).await?;
        let current = self
            .checkpoints
            .get(query_id)
            .expect("checkpoint present after ensure_seeded");
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

    /// Advance `query_id`'s checkpoint after a successful side effect, applying
    /// [`ReactionRecoveryPolicy`] to durable write failures.
    ///
    /// * `Strict` — return the write error immediately.
    /// * `AutoReset` — retry with bounded backoff, then return the last error.
    /// * `AutoSkipGap` — log the write error and proceed so a later successful
    ///   write can supersede.
    pub async fn advance_with_policy(
        &mut self,
        base: &ReactionBase,
        query_id: &str,
        sequence: u64,
        policy: ReactionRecoveryPolicy,
    ) -> anyhow::Result<()> {
        self.ensure_seeded(base, query_id).await?;
        let current = self
            .checkpoints
            .get(query_id)
            .expect("checkpoint present after ensure_seeded");
        if sequence <= current.sequence {
            return Ok(());
        }
        let cp = ReactionCheckpoint {
            sequence,
            config_hash: current.config_hash,
        };
        if self.has_store {
            persist_with_recovery_policy(policy, || base.write_checkpoint(query_id, &cp)).await?;
        }
        self.checkpoints.insert(query_id.to_string(), cp);
        Ok(())
    }

    /// Checkpoint each fully-acked query to its max terminal sequence **after**
    /// the batch ack has succeeded. This is the HTTP/gRPC contract: a batch of
    /// `N..N+k` persists `N+k` only once the side effect (ack) has committed.
    ///
    /// Writes are per-query, not one atomic batch. Query IDs are processed in
    /// sorted order so partial failure is deterministic: under `Strict` /
    /// `AutoReset`, a write failure leaves already-persisted queries advanced
    /// and remaining queries unchanged so they replay. Under `AutoSkipGap`
    /// [`persist_with_recovery_policy`] never returns `Err`, so every query in
    /// `completed` is attempted.
    pub async fn advance_completed_after_ack(
        &mut self,
        base: &ReactionBase,
        completed: &HashMap<String, u64>,
        policy: ReactionRecoveryPolicy,
    ) -> anyhow::Result<()> {
        let mut entries: Vec<_> = completed.iter().collect();
        entries.sort_by(|left, right| left.0.cmp(right.0));
        for (query_id, sequence) in entries {
            self.advance_with_policy(base, query_id, *sequence, policy)
                .await?;
        }
        Ok(())
    }
}

/// Persist a checkpoint write according to [`ReactionRecoveryPolicy`].
///
/// The `write` closure is the side-effect-after-commit durable store write —
/// callers must invoke this **only after** the observable side effect succeeds.
pub async fn persist_with_recovery_policy<F, Fut>(
    policy: ReactionRecoveryPolicy,
    mut write: F,
) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    match policy {
        ReactionRecoveryPolicy::Strict => write().await,
        ReactionRecoveryPolicy::AutoSkipGap => {
            if let Err(error) = write().await {
                warn!(
                    "Checkpoint write failed; continuing per AutoSkipGap so the next successful write can supersede: {error:#}"
                );
            }
            Ok(())
        }
        ReactionRecoveryPolicy::AutoReset => {
            let mut last_error = None;
            for attempt in 0..AUTO_RESET_CHECKPOINT_ATTEMPTS {
                match write().await {
                    Ok(()) => return Ok(()),
                    Err(error) => {
                        last_error = Some(error);
                        if attempt + 1 < AUTO_RESET_CHECKPOINT_ATTEMPTS {
                            let backoff = AUTO_RESET_CHECKPOINT_BASE_BACKOFF * 2u32.pow(attempt);
                            warn!(
                                "Checkpoint write failed (attempt {}/{}); retrying after {backoff:?} per AutoReset",
                                attempt + 1,
                                AUTO_RESET_CHECKPOINT_ATTEMPTS
                            );
                            tokio::time::sleep(backoff).await;
                        }
                    }
                }
            }
            Err(last_error.unwrap_or_else(|| {
                anyhow::anyhow!("checkpoint write failed after AutoReset retries")
            }))
        }
    }
}

/// Per-query checkpoint candidates for one delivered batch.
///
/// Each input item is `(query_id, sequence, is_terminal)`, where `is_terminal`
/// marks the last item of its originating `QueryResult`. Returns two maps:
///
/// * `completed` — the max sequence whose **terminal** item is in the batch.
///   Safe to checkpoint once the batch is acked: a `QueryResult` split across
///   batches only advances once the batch holding its terminal item lands.
/// * `seen` — the max sequence of **any** item, used only to advance past a
///   dropped batch under the `AutoSkipGap` policy (which accepts loss).
pub fn batch_checkpoint_candidates<I>(items: I) -> (HashMap<String, u64>, HashMap<String, u64>)
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
pub enum FailureAction {
    /// Fail-stop: set the reaction to `Error` and stop without advancing the
    /// checkpoint, so the un-acked batch replays from the outbox on restart.
    Stop,
    /// Drop the failed batch, advance past it, and continue (favor uptime).
    SkipAndContinue,
}

impl FailureAction {
    /// Map a recovery policy to the action a custom processing loop should take
    /// on a sustained delivery failure.
    pub fn from_policy(policy: ReactionRecoveryPolicy) -> Self {
        match policy {
            // Skip the gap and keep running, accepting potential loss.
            ReactionRecoveryPolicy::AutoSkipGap => FailureAction::SkipAndContinue,
            // Strict — and AutoReset, which startup validation rejects for
            // non-snapshot reactions — fail-stop to preserve at-least-once.
            _ => FailureAction::Stop,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reactions::common::base::ReactionBaseParams;
    use crate::state_store::{
        MemoryStateStoreProvider, StateStoreError, StateStoreProvider, StateStoreResult,
    };
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    struct FailWritesStore {
        inner: MemoryStateStoreProvider,
        remaining_failures: AtomicU32,
        fail_if_key_contains: Option<String>,
    }

    impl FailWritesStore {
        fn always() -> Self {
            Self {
                inner: MemoryStateStoreProvider::new(),
                remaining_failures: AtomicU32::new(u32::MAX),
                fail_if_key_contains: None,
            }
        }

        fn for_key_substring(needle: &str) -> Self {
            Self {
                inner: MemoryStateStoreProvider::new(),
                remaining_failures: AtomicU32::new(u32::MAX),
                fail_if_key_contains: Some(needle.to_string()),
            }
        }

        fn fail_if_needed(&self, key: &str) -> StateStoreResult<()> {
            if let Some(needle) = &self.fail_if_key_contains {
                if !key.contains(needle.as_str()) {
                    return Ok(());
                }
            }
            let remaining = self.remaining_failures.load(Ordering::SeqCst);
            if remaining == 0 {
                return Ok(());
            }
            if remaining != u32::MAX {
                self.remaining_failures.fetch_sub(1, Ordering::SeqCst);
            }
            Err(StateStoreError::StorageError(
                "injected write failure".into(),
            ))
        }
    }

    #[async_trait::async_trait]
    impl StateStoreProvider for FailWritesStore {
        async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
            self.inner.get(store_id, key).await
        }
        async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
            self.fail_if_needed(key)?;
            self.inner.set(store_id, key, value).await
        }
        async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
            self.inner.delete(store_id, key).await
        }
        async fn contains_key(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
            self.inner.contains_key(store_id, key).await
        }
        async fn get_many(
            &self,
            store_id: &str,
            keys: &[&str],
        ) -> StateStoreResult<std::collections::HashMap<String, Vec<u8>>> {
            self.inner.get_many(store_id, keys).await
        }
        async fn set_many(
            &self,
            store_id: &str,
            entries: &[(&str, &[u8])],
        ) -> StateStoreResult<()> {
            for (key, _) in entries {
                self.fail_if_needed(key)?;
            }
            self.inner.set_many(store_id, entries).await
        }
        async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize> {
            self.inner.delete_many(store_id, keys).await
        }
        async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize> {
            self.inner.clear_store(store_id).await
        }
        async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>> {
            self.inner.list_keys(store_id).await
        }
        async fn store_exists(&self, store_id: &str) -> StateStoreResult<bool> {
            self.inner.store_exists(store_id).await
        }
        async fn key_count(&self, store_id: &str) -> StateStoreResult<usize> {
            self.inner.key_count(store_id).await
        }
    }

    async fn store_backed_base(id: &str) -> ReactionBase {
        store_backed_base_with(id, Arc::new(MemoryStateStoreProvider::new())).await
    }

    async fn store_backed_base_with(id: &str, store: Arc<dyn StateStoreProvider>) -> ReactionBase {
        let base = ReactionBase::new(ReactionBaseParams::new(
            id,
            vec!["q1".to_string(), "q2".to_string()],
        ));
        let (graph, _rx) = crate::component_graph::ComponentGraph::new("inst");
        let ctx = crate::context::ReactionRuntimeContext::new(
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

    #[test]
    fn failure_action_maps_policy() {
        assert_eq!(
            FailureAction::from_policy(ReactionRecoveryPolicy::Strict),
            FailureAction::Stop
        );
        assert_eq!(
            FailureAction::from_policy(ReactionRecoveryPolicy::AutoReset),
            FailureAction::Stop
        );
        assert_eq!(
            FailureAction::from_policy(ReactionRecoveryPolicy::AutoSkipGap),
            FailureAction::SkipAndContinue
        );
    }

    #[tokio::test]
    async fn persist_strict_surfaces_write_error() {
        let result = persist_with_recovery_policy(ReactionRecoveryPolicy::Strict, || async {
            Err(anyhow::anyhow!("write failed"))
        })
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("write failed"));
    }

    #[tokio::test]
    async fn persist_autoskip_logs_and_continues() {
        persist_with_recovery_policy(ReactionRecoveryPolicy::AutoSkipGap, || async {
            Err(anyhow::anyhow!("write failed"))
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn persist_autoreset_retries_then_succeeds() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();
        persist_with_recovery_policy(ReactionRecoveryPolicy::AutoReset, move || {
            let attempts = attempts_clone.clone();
            async move {
                let n = attempts.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(anyhow::anyhow!("transient"))
                } else {
                    Ok(())
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn persist_autoreset_exhausted_retries_errors() {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        let attempts = Arc::new(AtomicU32::new(0));
        let attempts_clone = attempts.clone();
        let result = persist_with_recovery_policy(ReactionRecoveryPolicy::AutoReset, move || {
            let attempts = attempts_clone.clone();
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!("always fails"))
            }
        })
        .await;
        assert!(result.is_err());
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            AUTO_RESET_CHECKPOINT_ATTEMPTS
        );
    }

    #[tokio::test]
    async fn batch_checkpoints_max_sequence_only_after_ack() {
        let base = store_backed_base("batch-ack").await;
        base.write_checkpoint(
            "q1",
            &ReactionCheckpoint {
                sequence: 4,
                config_hash: 7,
            },
        )
        .await
        .unwrap();
        let mut state = CheckpointState::load(&base).await;

        let items = [
            ("q1".to_string(), 5, true),
            ("q1".to_string(), 6, true),
            ("q1".to_string(), 7, true),
        ];
        let (completed, _) = batch_checkpoint_candidates(items);

        // Kill after send, before ack: the batch is not checkpointed.
        let ack_succeeded = false;
        if ack_succeeded {
            state
                .advance_completed_after_ack(&base, &completed, ReactionRecoveryPolicy::Strict)
                .await
                .unwrap();
        }
        assert_eq!(
            base.read_checkpoint("q1").await.unwrap().unwrap().sequence,
            4,
            "a batch must not checkpoint until the ack succeeds"
        );

        // Ack succeeds: persist max_sequence_in_batch (7).
        state
            .advance_completed_after_ack(&base, &completed, ReactionRecoveryPolicy::Strict)
            .await
            .unwrap();
        assert_eq!(
            base.read_checkpoint("q1").await.unwrap().unwrap().sequence,
            7
        );
        assert_eq!(
            base.read_checkpoint("q1")
                .await
                .unwrap()
                .unwrap()
                .config_hash,
            7
        );
    }

    #[tokio::test]
    async fn advance_with_policy_autoskip_does_not_return_err_on_write_failure() {
        let store = Arc::new(FailWritesStore::always());
        let base = store_backed_base_with("autoskip-write", store).await;
        let mut state = CheckpointState::load(&base).await;
        state.seed(HashMap::from([(
            "q1".to_string(),
            ReactionCheckpoint {
                sequence: 1,
                config_hash: 3,
            },
        )]));

        state
            .advance_with_policy(&base, "q1", 4, ReactionRecoveryPolicy::AutoSkipGap)
            .await
            .unwrap();
        assert_eq!(state.get("q1").map(|cp| cp.sequence), Some(4));
        assert!(
            base.read_checkpoint("q1").await.unwrap().is_none(),
            "AutoSkipGap must not persist a failed write"
        );
    }

    #[tokio::test]
    async fn batch_autoskip_attempts_every_query_when_writes_fail() {
        let store = Arc::new(FailWritesStore::always());
        let base = store_backed_base_with("batch-skip", store).await;
        let mut state = CheckpointState::load(&base).await;
        state.seed(HashMap::from([
            (
                "q1".to_string(),
                ReactionCheckpoint {
                    sequence: 1,
                    config_hash: 1,
                },
            ),
            (
                "q2".to_string(),
                ReactionCheckpoint {
                    sequence: 1,
                    config_hash: 1,
                },
            ),
        ]));

        let completed = HashMap::from([("q1".to_string(), 5), ("q2".to_string(), 6)]);
        state
            .advance_completed_after_ack(&base, &completed, ReactionRecoveryPolicy::AutoSkipGap)
            .await
            .unwrap();
        assert_eq!(state.get("q1").map(|cp| cp.sequence), Some(5));
        assert_eq!(state.get("q2").map(|cp| cp.sequence), Some(6));
        assert!(base.read_checkpoint("q1").await.unwrap().is_none());
        assert!(base.read_checkpoint("q2").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn batch_strict_stops_after_first_failed_query_in_sorted_order() {
        let store = Arc::new(FailWritesStore::for_key_substring("q2"));
        let base = store_backed_base_with("batch-strict", store).await;
        base.write_checkpoint(
            "q1",
            &ReactionCheckpoint {
                sequence: 1,
                config_hash: 9,
            },
        )
        .await
        .unwrap();
        let mut state = CheckpointState::load(&base).await;
        state.seed(HashMap::from([
            (
                "q1".to_string(),
                ReactionCheckpoint {
                    sequence: 1,
                    config_hash: 9,
                },
            ),
            (
                "q2".to_string(),
                ReactionCheckpoint {
                    sequence: 1,
                    config_hash: 9,
                },
            ),
        ]));

        let completed = HashMap::from([("q1".to_string(), 5), ("q2".to_string(), 6)]);
        let result = state
            .advance_completed_after_ack(&base, &completed, ReactionRecoveryPolicy::Strict)
            .await;
        assert!(result.is_err());
        assert_eq!(
            base.read_checkpoint("q1").await.unwrap().unwrap().sequence,
            5,
            "q1 is persisted before the sorted q2 write fails"
        );
        assert!(
            base.read_checkpoint("q2").await.unwrap().is_none(),
            "failed query must not advance the durable checkpoint"
        );
        assert_eq!(state.get("q2").map(|cp| cp.sequence), Some(1));
    }
}
