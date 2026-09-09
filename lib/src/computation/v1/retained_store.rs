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
use drasi_core::computation::ComputationIndexes;
use tokio::sync::{Mutex, Notify};

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
    fn durable(&self) -> bool;
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
}

pub struct MemoryEnvelopeStore {
    state: std::sync::Mutex<MemoryState>,
    capacity: NonZeroUsize,
    policy: RetentionPolicy,
    generation: AtomicU64,
    epoch: AtomicU64,
    notify: Notify,
    closed: AtomicBool,
}

impl MemoryEnvelopeStore {
    pub fn new(capacity: NonZeroUsize, policy: RetentionPolicy) -> Self {
        Self {
            state: std::sync::Mutex::new(MemoryState::default()),
            capacity,
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
        if state.entries.len() >= self.capacity.get() {
            let oldest = state
                .entries
                .keys()
                .next()
                .copied()
                .expect("nonempty bounded history");
            if self.policy == RetentionPolicy::Backpressure && oldest > state.progress {
                return Err(PipeError::CapacityExhausted);
            }
            state.entries.remove(&oldest);
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
        self.notify.notify_waiters();
        Ok(())
    }
}

/// Durable envelope storage over a complete computation index/output bundle.
/// The store exclusively owns this bundle's journal and consumer checkpoint.
pub struct IndexedEnvelopeStore {
    indexes: ComputationIndexes,
    codec: Arc<EnvelopeCodec>,
    key: String,
    capacity: NonZeroUsize,
    policy: RetentionPolicy,
    lock: Mutex<()>,
    generation: AtomicU64,
    epoch: AtomicU64,
    fenced: AtomicBool,
    notify: Notify,
}

impl IndexedEnvelopeStore {
    pub fn try_new(
        indexes: ComputationIndexes,
        codec: Arc<EnvelopeCodec>,
        key: impl Into<String>,
        capacity: NonZeroUsize,
        policy: RetentionPolicy,
    ) -> Result<Self, PipeError> {
        let key = key.into();
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
            policy,
            lock: Mutex::new(()),
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

    async fn checkpoint(&self) -> Result<u64, PipeError> {
        Ok(self
            .indexes
            .checkpoint_store()
            .expect("validated checkpoint")
            .read_checkpoint(&self.consumer_key())
            .await
            .map_err(|error| PipeError::Backend(error.into()))?
            .map(|checkpoint| checkpoint.sequence)
            .unwrap_or(0))
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
    fn capacity(&self) -> NonZeroUsize {
        self.capacity
    }
    fn durable(&self) -> bool {
        true
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
        let _lock = self.lock.lock().await;
        self.check()?;
        check_generation(&self.generation, generation)?;
        let checkpoint = self
            .indexes
            .checkpoint_store()
            .expect("validated checkpoint");
        let outbox = self.indexes.outbox_writer().expect("validated outbox");
        let entries = outbox
            .read_from(&self.key, 0)
            .await
            .map_err(|error| PipeError::Backend(error.into()))?;
        let remove = entries
            .len()
            .saturating_add(1)
            .saturating_sub(self.capacity.get());
        if remove > 0 && self.policy == RetentionPolicy::Backpressure {
            let last_removed = entries.get(remove - 1).ok_or_else(|| {
                PipeError::Backend(anyhow::anyhow!("invalid retained capacity accounting"))
            })?;
            if last_removed.0 > self.checkpoint().await? {
                return Err(PipeError::CapacityExhausted);
            }
        }
        let head = checkpoint
            .read_result_sequence(&self.key)
            .await
            .map_err(|error| PipeError::Backend(error.into()))?
            .unwrap_or(0);
        let position = head
            .checked_add(1)
            .ok_or_else(|| PipeError::Backend(anyhow::anyhow!("retained position exhausted")))?;
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
            .append(&self.key, position, &bytes)
            .await
            .map_err(|error| PipeError::Backend(error.into()))?;
        outbox
            .trim_to_capacity(&self.key, self.capacity.get())
            .await
            .map_err(|error| PipeError::Backend(error.into()))?;
        checkpoint
            .write_result_sequence(&self.key, position)
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
        fence.complete = true;
        self.notify.notify_waiters();
        Ok(position)
    }

    async fn next(&self, generation: u64, after: u64) -> Result<Option<StoredEnvelope>, PipeError> {
        let _lock = self.lock.lock().await;
        self.check()?;
        check_generation(&self.generation, generation)?;
        let entries = self
            .indexes
            .outbox_writer()
            .expect("validated outbox")
            .read_from(&self.key, 0)
            .await
            .map_err(|error| PipeError::Backend(error.into()))?;
        if let Some((oldest, _)) = entries.first() {
            if after < oldest.saturating_sub(1) {
                return Err(PipeError::PositionUnavailable {
                    requested: after,
                    oldest: *oldest,
                });
            }
        }
        entries
            .into_iter()
            .find(|(position, _)| *position > after)
            .map(|(position, bytes)| {
                Ok(StoredEnvelope {
                    position,
                    envelope: self
                        .codec
                        .decode(&bytes)
                        .map_err(|error| PipeError::Backend(error.into()))?,
                })
            })
            .transpose()
    }

    async fn progress(&self, generation: u64) -> Result<u64, PipeError> {
        let _lock = self.lock.lock().await;
        self.check()?;
        check_generation(&self.generation, generation)?;
        self.checkpoint().await
    }

    async fn acknowledge(&self, generation: u64, position: u64) -> Result<(), PipeError> {
        let _lock = self.lock.lock().await;
        self.check()?;
        check_generation(&self.generation, generation)?;
        let checkpoint = self
            .indexes
            .checkpoint_store()
            .expect("validated checkpoint");
        let current = self.checkpoint().await?;
        let head = checkpoint
            .read_result_sequence(&self.key)
            .await
            .map_err(|error| PipeError::Backend(error.into()))?
            .unwrap_or(0);
        if position < current || position > head {
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
        fence.complete = true;
        self.notify.notify_waiters();
        Ok(())
    }
}

#[async_trait]
impl ResourceCleanup for IndexedEnvelopeStore {
    async fn shutdown(&self) -> anyhow::Result<()> {
        let _lock = self.lock.try_lock().map_err(|_| {
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
        Ok(())
    }
}
