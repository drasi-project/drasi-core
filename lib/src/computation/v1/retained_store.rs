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

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::computation::ComputationIndexes;
use drasi_core::interface::OutboxPageLimits;
use tokio::sync::{MappedMutexGuard, Mutex, MutexGuard, Notify};

use super::journal_budget::JournalBudget;
use super::{ChangeEnvelope, EnvelopeCodec, PipeError, ResourceCleanup};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RetentionPolicy {
    Backpressure,
    /// Explicitly lossy retention; receivers must detect the unavailable position.
    PruneOldest,
}

pub struct StoredEnvelope {
    pub position: u64,
    pub envelope: ChangeEnvelope,
}

/// One bounded journal and consumer-progress boundary. Sharing one journal
/// between simultaneously active bindings is rejected by generation leases.
#[async_trait]
pub trait RetainedEnvelopeStore: ResourceCleanup + Send + Sync {
    fn capacity(&self) -> NonZeroUsize;
    /// Optional full binary-envelope byte quota, configured on the journal owner.
    fn max_bytes(&self) -> Option<NonZeroUsize> {
        None
    }
    fn durable(&self) -> bool;
    fn durability(&self) -> drasi_core::computation::StorageDurability {
        if self.durable() {
            drasi_core::computation::StorageDurability::UNKNOWN
        } else {
            drasi_core::computation::StorageDurability::VOLATILE
        }
    }
    fn retention_policy(&self) -> RetentionPolicy;
    fn acquire_generation(&self) -> Result<u64, PipeError>;
    fn revoke_generation(&self, generation: u64);
    fn generation(&self) -> u64;
    fn notify(&self) -> &Notify;
    async fn append(&self, generation: u64, envelope: &ChangeEnvelope) -> Result<u64, PipeError>;
    async fn next(&self, generation: u64, after: u64) -> Result<Option<StoredEnvelope>, PipeError>;
    async fn progress(&self, generation: u64) -> Result<u64, PipeError>;
    async fn acknowledge(&self, generation: u64, position: u64) -> Result<(), PipeError>;
}

fn generation(counter: &AtomicU64) -> Result<u64, PipeError> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .map(|value| value + 1)
        .map_err(|_| PipeError::Backend(anyhow::anyhow!("pipe generation exhausted")))
}

fn check_generation(counter: &AtomicU64, generation: u64) -> Result<(), PipeError> {
    if generation != 0 && counter.load(Ordering::Acquire) == generation {
        Ok(())
    } else {
        Err(PipeError::Closed)
    }
}

#[derive(Default)]
struct MemoryState {
    entries: BTreeMap<u64, ChangeEnvelope>,
    head: u64,
    progress: u64,
    budget: Option<JournalBudget>,
}

pub struct MemoryEnvelopeStore {
    state: std::sync::Mutex<MemoryState>,
    capacity: NonZeroUsize,
    max_bytes: Option<NonZeroUsize>,
    policy: RetentionPolicy,
    generation: AtomicU64,
    epoch: AtomicU64,
    notify: Notify,
    closed: AtomicBool,
}

impl MemoryEnvelopeStore {
    pub fn new(capacity: NonZeroUsize, policy: RetentionPolicy) -> Self {
        Self::with_budget(capacity, policy, None)
    }

    /// Budget retained history, including acknowledged entries until pruning.
    /// Oversized envelopes require an otherwise empty retained window.
    pub fn new_with_byte_budget(
        capacity: NonZeroUsize,
        policy: RetentionPolicy,
        max_bytes: NonZeroUsize,
    ) -> Self {
        Self::with_budget(capacity, policy, Some(max_bytes))
    }

    fn with_budget(
        capacity: NonZeroUsize,
        policy: RetentionPolicy,
        max_bytes: Option<NonZeroUsize>,
    ) -> Self {
        Self {
            state: std::sync::Mutex::new(MemoryState {
                budget: max_bytes.map(JournalBudget::new),
                ..MemoryState::default()
            }),
            capacity,
            max_bytes,
            policy,
            generation: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            notify: Notify::new(),
            closed: AtomicBool::new(false),
        }
    }

    fn check(&self, generation: u64) -> Result<(), PipeError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PipeError::Closed);
        }

        check_generation(&self.generation, generation)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>, PipeError> {
        self.state.lock().map_err(|error| {
            PipeError::Backend(anyhow::anyhow!("memory journal state poisoned: {error}"))
        })
    }
}

#[async_trait]
impl RetainedEnvelopeStore for MemoryEnvelopeStore {
    fn max_bytes(&self) -> Option<NonZeroUsize> {
        self.max_bytes
    }
    fn capacity(&self) -> NonZeroUsize {
        self.capacity
    }
    fn durable(&self) -> bool {
        false
    }
    fn retention_policy(&self) -> RetentionPolicy {
        self.policy
    }
    fn acquire_generation(&self) -> Result<u64, PipeError> {
        let _lock = self.lock()?;
        if self.closed.load(Ordering::Acquire) {
            return Err(PipeError::Closed);
        }
        let generation = generation(&self.epoch)?;
        self.generation.store(generation, Ordering::Release);
        self.notify.notify_waiters();
        Ok(generation)
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    fn revoke_generation(&self, generation: u64) {
        match self.lock() {
            Ok(_state) => {
                let _ = self.generation.compare_exchange(
                    generation,
                    0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            Err(error) => log::error!("Cannot revoke poisoned memory journal generation: {error}"),
        }
        self.notify.notify_waiters();
    }
    fn notify(&self) -> &Notify {
        &self.notify
    }

    async fn append(&self, generation: u64, envelope: &ChangeEnvelope) -> Result<u64, PipeError> {
        let mut state = self.lock()?;
        self.check(generation)?;
        let position = state
            .head
            .checked_add(1)
            .ok_or_else(|| PipeError::Backend(anyhow::anyhow!("retained position exhausted")))?;
        let remove = state.entries.len().saturating_sub(self.capacity.get() - 1);
        let budget = state
            .budget
            .as_ref()
            .map(|budget| budget.prepare(envelope, remove))
            .transpose()?;
        let remove = budget.as_ref().map_or(remove, |append| append.remove);
        if remove > 0 {
            let last_removed = state
                .entries
                .keys()
                .nth(remove - 1)
                .expect("bounded journal accounting");
            if self.policy == RetentionPolicy::Backpressure && *last_removed > state.progress {
                return Err(PipeError::CapacityExhausted);
            }
        }
        for _ in 0..remove {
            state.entries.pop_first();
        }
        if let Some(append) = budget {
            state
                .budget
                .as_mut()
                .expect("configured budget")
                .apply(append);
        }
        state.entries.insert(position, envelope.clone());
        state.head = position;
        drop(state);
        self.notify.notify_waiters();
        Ok(position)
    }

    async fn next(&self, generation: u64, after: u64) -> Result<Option<StoredEnvelope>, PipeError> {
        let state = self.lock()?;
        self.check(generation)?;
        if let Some(oldest) = state.entries.keys().next().copied() {
            if after < oldest.saturating_sub(1) {
                return Err(PipeError::PositionUnavailable {
                    requested: after,
                    oldest,
                });
            }
        }
        let Some(start) = after.checked_add(1) else {
            return Ok(None);
        };
        Ok(state
            .entries
            .range(start..)
            .next()
            .map(|(position, envelope)| StoredEnvelope {
                position: *position,
                envelope: envelope.clone(),
            }))
    }

    async fn progress(&self, generation: u64) -> Result<u64, PipeError> {
        let state = self.lock()?;
        self.check(generation)?;
        Ok(state.progress)
    }

    async fn acknowledge(&self, generation: u64, position: u64) -> Result<(), PipeError> {
        let mut state = self.lock()?;
        self.check(generation)?;
        if position < state.progress || position > state.head {
            return Err(PipeError::Backend(anyhow::anyhow!(
                "invalid retained acknowledgement position"
            )));
        }
        state.progress = position;
        drop(state);
        self.notify.notify_waiters();
        Ok(())
    }
}

#[async_trait]
impl ResourceCleanup for MemoryEnvelopeStore {
    async fn shutdown(&self) -> anyhow::Result<()> {
        let mut state = self.lock()?;
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.generation.store(0, Ordering::Release);
        state.entries.clear();
        state.budget = None;
        self.notify.notify_waiters();
        Ok(())
    }
}

/// Durable envelope storage over a complete computation index/output bundle.
/// The store exclusively owns this bundle's journal and consumer checkpoint.
/// Committed bytes and progress are loaded once and updated only after commit.
/// A smaller reopened capacity retains old obligations until they can be pruned.
pub struct IndexedEnvelopeStore {
    indexes: ComputationIndexes,
    codec: Arc<EnvelopeCodec>,
    key: String,
    capacity: NonZeroUsize,
    max_bytes: Option<NonZeroUsize>,
    page_limits: Option<OutboxPageLimits>,
    policy: RetentionPolicy,
    lock: Mutex<Option<IndexedState>>,
    generation: AtomicU64,
    epoch: AtomicU64,
    fenced: AtomicBool,
    notify: Notify,
}

struct IndexedState {
    entries: BTreeMap<u64, Bytes>,
    oldest: Option<u64>,
    retained: usize,
    head: u64,
    progress: u64,
    budget: Option<JournalBudget>,
}

/// Storage-owner policy, independent of a pipe's handling/capability declaration.
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexedJournalOptions {
    pub max_bytes: Option<NonZeroUsize>,
    pub page_limits: Option<OutboxPageLimits>,
}

impl IndexedEnvelopeStore {
    pub fn try_new(
        indexes: ComputationIndexes,
        codec: Arc<EnvelopeCodec>,
        key: impl Into<String>,
        capacity: NonZeroUsize,
        policy: RetentionPolicy,
    ) -> Result<Self, PipeError> {
        Self::try_new_with_options(
            indexes,
            codec,
            key,
            capacity,
            policy,
            IndexedJournalOptions::default(),
        )
    }

    /// Reconstruct all existing obligations before enforcing the configured byte
    /// quota on new appends. Budgeting does not change the persisted envelope codec.
    pub fn try_new_with_byte_budget(
        indexes: ComputationIndexes,
        codec: Arc<EnvelopeCodec>,
        key: impl Into<String>,
        capacity: NonZeroUsize,
        policy: RetentionPolicy,
        max_bytes: NonZeroUsize,
    ) -> Result<Self, PipeError> {
        Self::try_new_with_options(
            indexes,
            codec,
            key,
            capacity,
            policy,
            IndexedJournalOptions {
                max_bytes: Some(max_bytes),
                page_limits: None,
            },
        )
    }

    pub fn try_new_with_options(
        indexes: ComputationIndexes,
        codec: Arc<EnvelopeCodec>,
        key: impl Into<String>,
        capacity: NonZeroUsize,
        policy: RetentionPolicy,
        options: IndexedJournalOptions,
    ) -> Result<Self, PipeError> {
        let key = key.into();
        let IndexedJournalOptions {
            max_bytes,
            page_limits,
        } = options;
        super::data::validate_identifier("retained store", &key)?;
        indexes
            .atomic_result_transaction()
            .map_err(|error| PipeError::Backend(error.into()))?;
        if indexes.cleanup().is_none() {
            return Err(PipeError::Backend(anyhow::anyhow!(
                "durable retained store requires an explicit asynchronous cleanup owner"
            )));
        }
        if !indexes
            .checkpoint_store()
            .is_some_and(|store| store.is_persistent())
        {
            return Err(PipeError::Backend(anyhow::anyhow!(
                "durable pipe requires persistent storage"
            )));
        }
        Ok(Self {
            indexes,
            codec,
            key,
            capacity,
            max_bytes,
            page_limits,
            policy,
            lock: Mutex::new(None),
            generation: AtomicU64::new(0),
            epoch: AtomicU64::new(0),
            fenced: AtomicBool::new(false),
            notify: Notify::new(),
        })
    }

    fn check(&self) -> Result<(), PipeError> {
        if self.fenced.load(Ordering::Acquire) {
            Err(PipeError::Backend(anyhow::anyhow!(
                "retained store requires cleanup and recovery"
            )))
        } else {
            Ok(())
        }
    }

    fn consumer_key(&self) -> String {
        format!("computation-pipe-consumer:{}", self.key)
    }

    async fn load(&self) -> Result<IndexedState, PipeError> {
        let checkpoints = self
            .indexes
            .checkpoint_store()
            .expect("validated checkpoint");
        let progress = checkpoints
            .read_checkpoint(&self.consumer_key())
            .await
            .map_err(|error| PipeError::Backend(error.into()))?
            .map(|checkpoint| checkpoint.sequence)
            .unwrap_or(0);
        let head = checkpoints
            .read_result_sequence(&self.key)
            .await
            .map_err(|error| PipeError::Backend(error.into()))?
            .unwrap_or(0);
        let outbox = self.indexes.outbox_writer().expect("validated outbox");
        let mut entries = BTreeMap::new();
        let mut budget = self.max_bytes.map(JournalBudget::new);
        let mut previous: Option<u64> = None;
        let mut oldest = None;
        let mut retained = 0usize;
        loop {
            let records = match self.page_limits {
                Some(limits) => {
                    let records = outbox
                        .read_page(&self.key, previous.unwrap_or(0), limits)
                        .await
                        .map_err(|error| PipeError::Backend(error.into()))?;
                    limits
                        .validate_page(&records)
                        .map_err(|error| PipeError::Backend(error.into()))?;
                    records
                }
                None => outbox
                    .read_from(&self.key, 0)
                    .await
                    .map_err(|error| PipeError::Backend(error.into()))?,
            };
            if records.is_empty() {
                break;
            }
            let mut page = BTreeMap::new();
            for (position, bytes) in records {
                if position == 0
                    || previous.is_some_and(|value| value.checked_add(1) != Some(position))
                {
                    return Err(PipeError::Backend(anyhow::anyhow!(
                        "retained journal positions are not consecutive"
                    )));
                }
                let envelope = self
                    .codec
                    .decode(&bytes)
                    .map_err(|error| PipeError::Backend(error.into()))?;
                if let Some(budget) = &mut budget {
                    budget.restore(&envelope)?;
                }
                oldest.get_or_insert(position);
                retained = retained.checked_add(1).ok_or_else(|| {
                    PipeError::Backend(anyhow::anyhow!("retained count overflow"))
                })?;
                page.insert(position, Bytes::from(bytes));
                previous = Some(position);
            }
            if self.page_limits.is_none()
                || entries.is_empty()
                || progress
                    .checked_add(1)
                    .is_some_and(|position| page.contains_key(&position))
            {
                entries = page;
            }
            if self.page_limits.is_none() {
                break;
            }
        }
        if previous.unwrap_or(0) != head || progress > head {
            return Err(PipeError::Backend(anyhow::anyhow!(
                "retained journal head or consumer progress is inconsistent"
            )));
        }
        Ok(IndexedState {
            entries,
            oldest,
            retained,
            head,
            progress,
            budget,
        })
    }

    async fn lock_state(
        &self,
        generation: u64,
    ) -> Result<MappedMutexGuard<'_, IndexedState>, PipeError> {
        let mut state = self.lock.lock().await;
        self.check()?;
        check_generation(&self.generation, generation)?;
        if state.is_none() {
            *state = Some(self.load().await?);
        }
        check_generation(&self.generation, generation)?;
        Ok(MutexGuard::map(state, |state| {
            state.as_mut().expect("loaded retained state")
        }))
    }
}

struct WriteFence<'a> {
    store: &'a IndexedEnvelopeStore,
    complete: bool,
}

impl Drop for WriteFence<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.store.fenced.store(true, Ordering::Release);
            if let Some(cleanup) = self.store.indexes.cleanup() {
                cleanup.cancel();
            }
            self.store.notify.notify_waiters();
        }
    }
}

#[async_trait]
impl RetainedEnvelopeStore for IndexedEnvelopeStore {
    fn max_bytes(&self) -> Option<NonZeroUsize> {
        self.max_bytes
    }
    fn capacity(&self) -> NonZeroUsize {
        self.capacity
    }
    fn durable(&self) -> bool {
        true
    }
    fn durability(&self) -> drasi_core::computation::StorageDurability {
        self.indexes.durability()
    }
    fn retention_policy(&self) -> RetentionPolicy {
        self.policy
    }
    fn acquire_generation(&self) -> Result<u64, PipeError> {
        let _lock = self
            .lock
            .try_lock()
            .map_err(|_| PipeError::Backend(anyhow::anyhow!("retained store is in use")))?;
        self.check()?;
        let generation = generation(&self.epoch)?;
        self.generation.store(generation, Ordering::Release);
        self.notify.notify_waiters();
        Ok(generation)
    }
    fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }
    fn revoke_generation(&self, generation: u64) {
        let _ =
            self.generation
                .compare_exchange(generation, 0, Ordering::AcqRel, Ordering::Acquire);
        self.notify.notify_waiters();
    }
    fn notify(&self) -> &Notify {
        &self.notify
    }

    async fn append(&self, generation: u64, envelope: &ChangeEnvelope) -> Result<u64, PipeError> {
        let bytes = self
            .codec
            .encode(envelope)
            .map_err(|error| PipeError::Backend(error.into()))?;
        let mut state = self.lock_state(generation).await?;
        let checkpoint = self
            .indexes
            .checkpoint_store()
            .expect("validated checkpoint");
        let outbox = self.indexes.outbox_writer().expect("validated outbox");
        let remove = state
            .retained
            .saturating_add(1)
            .saturating_sub(self.capacity.get());
        let budget = state
            .budget
            .as_ref()
            .map(|budget| budget.prepare(envelope, remove))
            .transpose()?;
        let remove = budget.as_ref().map_or(remove, |append| append.remove);
        if remove > 0 && self.policy == RetentionPolicy::Backpressure {
            let last_removed = state
                .oldest
                .and_then(|oldest| oldest.checked_add(remove as u64 - 1))
                .ok_or_else(|| {
                    PipeError::Backend(anyhow::anyhow!("invalid retained capacity accounting"))
                })?;
            if last_removed > state.progress {
                return Err(PipeError::CapacityExhausted);
            }
        }
        let position = state
            .head
            .checked_add(1)
            .ok_or_else(|| PipeError::Backend(anyhow::anyhow!("retained position exhausted")))?;
        let retain_from = state
            .oldest
            .and_then(|oldest| oldest.checked_add(remove as u64))
            .unwrap_or(position);
        let mut fence = WriteFence {
            store: self,
            complete: false,
        };
        self.indexes
            .indexes()
            .session_control
            .begin()
            .await
            .map_err(|error| PipeError::Backend(error.into()))?;
        outbox
            .append_and_trim(&self.key, position, &bytes, retain_from)
            .await
            .map_err(|error| PipeError::Backend(error.into()))?;
        checkpoint
            .stage_result_sequence(&self.key, position)
            .await
            .map_err(|error| PipeError::Backend(error.into()))?;
        check_generation(&self.generation, generation)?;
        self.indexes
            .indexes()
            .session_control
            .commit()
            .await
            .map_err(|error| PipeError::AcceptanceUnknown {
                source: error.into(),
            })?;
        if check_generation(&self.generation, generation).is_err() {
            return Err(PipeError::AcceptanceUnknown {
                source: anyhow::anyhow!("pipe generation revoked during durable commit"),
            });
        }
        if self.page_limits.is_some() {
            state.entries.retain(|position, _| *position >= retain_from);
        } else {
            for _ in 0..remove {
                state.entries.pop_first();
            }
        }
        if let Some(append) = budget {
            state
                .budget
                .as_mut()
                .expect("configured budget")
                .apply(append);
        }
        if self.page_limits.map_or(true, |limits| {
            let cached_bytes = state.entries.values().map(Bytes::len).sum();
            limits.admits(state.entries.len(), cached_bytes, bytes.len())
                && state
                    .entries
                    .last_key_value()
                    .map_or(true, |(last, _)| last.checked_add(1) == Some(position))
        }) {
            state.entries.insert(position, bytes);
        }
        state.retained = state.retained - remove + 1;
        state.oldest = Some(retain_from);
        state.head = position;
        fence.complete = true;
        self.notify.notify_waiters();
        Ok(position)
    }

    async fn next(&self, generation: u64, after: u64) -> Result<Option<StoredEnvelope>, PipeError> {
        let mut state = self.lock_state(generation).await?;
        if let Some(oldest) = state.oldest {
            if after < oldest.saturating_sub(1) {
                return Err(PipeError::PositionUnavailable {
                    requested: after,
                    oldest,
                });
            }
        }
        let Some(start) = after.checked_add(1) else {
            return Ok(None);
        };
        if start > state.head {
            return Ok(None);
        }
        if let Some(limits) = self.page_limits {
            if !state.entries.contains_key(&start) {
                let records = self
                    .indexes
                    .outbox_writer()
                    .expect("validated outbox")
                    .read_page(&self.key, after, limits)
                    .await
                    .map_err(|error| PipeError::Backend(error.into()))?;
                check_generation(&self.generation, generation)?;
                limits
                    .validate_page(&records)
                    .map_err(|error| PipeError::Backend(error.into()))?;
                let mut page = BTreeMap::new();
                let mut expected = Some(start);
                for (position, bytes) in records {
                    if Some(position) != expected || position > state.head {
                        return Err(PipeError::Backend(anyhow::anyhow!(
                            "retained page is inconsistent"
                        )));
                    }
                    self.codec
                        .decode(&bytes)
                        .map_err(|error| PipeError::Backend(error.into()))?;
                    page.insert(position, Bytes::from(bytes));
                    expected = position.checked_add(1);
                }
                if page.is_empty() {
                    return Err(PipeError::Backend(anyhow::anyhow!(
                        "retained page ended before journal head"
                    )));
                }
                state.entries = page;
            }
        }
        state
            .entries
            .range(start..)
            .next()
            .map(|(position, bytes)| {
                Ok(StoredEnvelope {
                    position: *position,
                    envelope: self
                        .codec
                        .decode(bytes)
                        .map_err(|error| PipeError::Backend(error.into()))?,
                })
            })
            .transpose()
    }

    async fn progress(&self, generation: u64) -> Result<u64, PipeError> {
        Ok(self.lock_state(generation).await?.progress)
    }

    async fn acknowledge(&self, generation: u64, position: u64) -> Result<(), PipeError> {
        let mut state = self.lock_state(generation).await?;
        let checkpoint = self
            .indexes
            .checkpoint_store()
            .expect("validated checkpoint");
        if position < state.progress || position > state.head {
            return Err(PipeError::Backend(anyhow::anyhow!(
                "invalid retained acknowledgement position"
            )));
        }
        let mut fence = WriteFence {
            store: self,
            complete: false,
        };
        self.indexes
            .indexes()
            .session_control
            .begin()
            .await
            .map_err(|error| PipeError::Backend(error.into()))?;
        checkpoint
            .stage_checkpoint(&self.consumer_key(), position, None)
            .await
            .map_err(|error| PipeError::Backend(error.into()))?;
        check_generation(&self.generation, generation)?;
        self.indexes
            .indexes()
            .session_control
            .commit()
            .await
            .map_err(|error| PipeError::AcknowledgementUnknown {
                source: error.into(),
            })?;
        if check_generation(&self.generation, generation).is_err() {
            return Err(PipeError::AcknowledgementUnknown {
                source: anyhow::anyhow!("pipe generation revoked during acknowledgement commit"),
            });
        }
        state.progress = position;
        fence.complete = true;
        self.notify.notify_waiters();
        Ok(())
    }
}

#[async_trait]
impl ResourceCleanup for IndexedEnvelopeStore {
    async fn shutdown(&self) -> anyhow::Result<()> {
        let mut state = self.lock.try_lock().map_err(|_| {
            anyhow::anyhow!("cancel or await retained store operations before shutdown")
        })?;
        self.fenced.store(true, Ordering::Release);
        self.generation.store(0, Ordering::Release);
        self.notify.notify_waiters();
        if let Some(cleanup) = self.indexes.cleanup() {
            cleanup.cancel();
            cleanup.shutdown().await?;
        }
        self.indexes.indexes().session_control.rollback()?;
        *state = None;
        Ok(())
    }
}

#[cfg(test)]
mod foundation_tests {
    use super::*;

    #[test]
    fn a_retained_memory_journal_does_not_promise_restart_survival() {
        use drasi_core::interface::{FailureMode, StorageDurability};
        let store = MemoryEnvelopeStore::new(
            NonZeroUsize::new(1).expect("capacity"),
            RetentionPolicy::Backpressure,
        );
        assert_eq!(store.durability(), StorageDurability::VOLATILE);
        assert!(store
            .durability()
            .require(FailureMode::ProcessRestart)
            .is_err());
    }

    #[tokio::test]
    async fn memory_journal_counters_and_progress_cannot_wrap_or_regress() {
        let store = MemoryEnvelopeStore::new(
            NonZeroUsize::new(1).expect("capacity"),
            RetentionPolicy::Backpressure,
        );
        store.epoch.store(u64::MAX, Ordering::Release);
        assert!(matches!(
            store.acquire_generation(),
            Err(PipeError::Backend(_))
        ));
        store.epoch.store(0, Ordering::Release);
        let generation = store.acquire_generation().expect("generation");
        assert!(matches!(store.progress(0).await, Err(PipeError::Closed)));
        store.state.lock().expect("state").head = u64::MAX;
        assert!(matches!(
            store
                .append(generation, &super::super::pipe::test_envelope(1))
                .await,
            Err(PipeError::Backend(_))
        ));
        assert!(store
            .next(generation, u64::MAX)
            .await
            .expect("end")
            .is_none());
        {
            let mut state = store.state.lock().expect("state");
            state.head = 5;
            state.progress = 3;
        }
        for invalid in [0, 2, 6, u64::MAX] {
            assert!(matches!(
                store.acknowledge(generation, invalid).await,
                Err(PipeError::Backend(_))
            ));
            assert_eq!(store.progress(generation).await.expect("unchanged"), 3);
        }
        store
            .acknowledge(generation, 3)
            .await
            .expect("idempotent completion");
        store.shutdown().await.expect("shutdown");
        store.shutdown().await.expect("idempotent shutdown");
        assert!(matches!(store.acquire_generation(), Err(PipeError::Closed)));
        assert!(matches!(
            store.progress(generation).await,
            Err(PipeError::Closed)
        ));
    }

    #[tokio::test]
    async fn poisoned_journal_does_not_turn_failed_storage_into_empty_history() {
        let store = MemoryEnvelopeStore::new(
            NonZeroUsize::new(1).expect("capacity"),
            RetentionPolicy::Backpressure,
        );
        let generation = store.acquire_generation().expect("generation");
        assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.state.lock().expect("lock");
            panic!("injected owner failure");
        }))
        .is_err());
        assert!(matches!(
            store.acquire_generation(),
            Err(PipeError::Backend(_))
        ));
        assert!(matches!(
            store.progress(generation).await,
            Err(PipeError::Backend(_))
        ));
        assert!(matches!(
            store.next(generation, 0).await,
            Err(PipeError::Backend(_))
        ));
        assert!(matches!(
            store
                .append(generation, &super::super::pipe::test_envelope(1))
                .await,
            Err(PipeError::Backend(_))
        ));
        assert!(matches!(
            store.acknowledge(generation, 1).await,
            Err(PipeError::Backend(_))
        ));
        store.revoke_generation(generation);
        assert!(store.shutdown().await.is_err());
    }
}
