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
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock, RwLock, Weak,
    },
};

use async_trait::async_trait;
use tokio::sync::Mutex;

use super::{
    ComponentDescriptor, ComponentId, ComputationComponent, EnvelopeSink, InputEnvelope,
    OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId, QueryChangeCodec,
    QueryHistoryError, QueryIdentityError, QueryRecoveryIdentity, QueryRecoveryView, QueryResults,
    SinkCompletion, StreamId, SystemMetadata, Transformer, WakeupSource,
};

#[async_trait]
pub trait ConsumerProgressStore: Send + Sync {
    async fn load(&self, query: &str) -> anyhow::Result<Option<ConsumerCheckpoint>>;
    async fn commit_handled(
        &self,
        query: &str,
        checkpoint: ConsumerCheckpoint,
    ) -> anyhow::Result<()>;
    /// Commit an explicitly handled replacement or accepted skip, conditional on
    /// the cursor that authorized it. Ordinary commits must not change identity.
    async fn commit_recovery(
        &self,
        _query: &str,
        _previous: Option<ConsumerCheckpoint>,
        _checkpoint: ConsumerCheckpoint,
    ) -> anyhow::Result<()> {
        Err(ConsumerRecoveryError::RecoveryCommitUnsupported.into())
    }
    async fn allocate_sequence(&self, stream: &StreamId) -> anyhow::Result<u64>;
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerCheckpoint {
    pub sequence: u64,
    pub generation: u64,
    /// Never infer this from a query ID or a matching sequence/generation.
    pub identity: QueryRecoveryIdentity,
}

#[derive(Debug, thiserror::Error)]
pub enum ConsumerRecoveryError {
    #[error(transparent)]
    Identity(#[from] QueryIdentityError),
    #[error("consumer checkpoint regression")]
    CheckpointRegression,
    #[error("consumer checkpoint changed before the recovery decision committed")]
    CheckpointConflict,
    #[error("consumer checkpoint {sequence} is ahead of query head {latest}")]
    CheckpointAhead { sequence: u64, latest: u64 },
    #[error("query generation differs: expected {expected}, received {actual}")]
    GenerationMismatch { expected: u64, actual: u64 },
    #[error("unexplained consumer sequence gap: handled {handled}, received {received}")]
    Gap { handled: u64, received: u64 },
    #[error("query reset requires an explicitly authorized replacement or skip")]
    ResetRequired,
    #[error("consumer has not enabled explicit skipped-reset decisions")]
    SkipNotAllowed,
    #[error("consumer progress store does not support conditional recovery commits")]
    RecoveryCommitUnsupported,
    #[error("invalid consumer checkpoint: {0}")]
    InvalidCheckpoint(String),
    #[error("invalid consumer recovery decision: {0}")]
    InvalidDecision(String),
    #[error("invalid live query input: {0}")]
    InvalidInput(String),
    #[error("consumer sequence exhausted")]
    SequenceExhausted,
}

impl ConsumerCheckpoint {
    fn validate(&self, query: &str) -> anyhow::Result<()> {
        self.identity.validate()?;
        if self.identity.query_id() != query {
            return Err(QueryIdentityError::Mismatch.into());
        }
        Ok(())
    }

    fn from_view(view: &QueryRecoveryView) -> Self {
        Self {
            sequence: view.snapshot.as_of_sequence,
            generation: view.snapshot.generation,
            identity: view.identity.clone(),
        }
    }
}

fn validate_commit(
    previous: Option<&ConsumerCheckpoint>,
    next: &ConsumerCheckpoint,
    recovery: bool,
) -> anyhow::Result<()> {
    if let Some(previous) = previous {
        if previous.identity != next.identity {
            if recovery {
                return Ok(());
            }
            return Err(QueryIdentityError::Mismatch.into());
        }
        if previous.sequence > next.sequence || previous.generation > next.generation {
            return Err(ConsumerRecoveryError::CheckpointRegression.into());
        }
        if previous.generation != next.generation && !recovery {
            return Err(ConsumerRecoveryError::ResetRequired.into());
        }
    }
    Ok(())
}

#[derive(Default)]
pub struct MemoryConsumerProgress {
    handled: Mutex<HashMap<String, ConsumerCheckpoint>>,
    sequences: Mutex<HashMap<String, u64>>,
}

#[async_trait]
impl ConsumerProgressStore for MemoryConsumerProgress {
    async fn load(&self, query: &str) -> anyhow::Result<Option<ConsumerCheckpoint>> {
        Ok(self.handled.lock().await.get(query).cloned())
    }
    async fn commit_handled(
        &self,
        query: &str,
        checkpoint: ConsumerCheckpoint,
    ) -> anyhow::Result<()> {
        checkpoint.validate(query)?;
        let mut handled = self.handled.lock().await;
        validate_commit(handled.get(query), &checkpoint, false)?;
        handled.insert(query.to_owned(), checkpoint);
        Ok(())
    }
    async fn commit_recovery(
        &self,
        query: &str,
        previous: Option<ConsumerCheckpoint>,
        checkpoint: ConsumerCheckpoint,
    ) -> anyhow::Result<()> {
        checkpoint.validate(query)?;
        let mut handled = self.handled.lock().await;
        if handled.get(query) != previous.as_ref() {
            return Err(ConsumerRecoveryError::CheckpointConflict.into());
        }
        validate_commit(previous.as_ref(), &checkpoint, true)?;
        handled.insert(query.to_owned(), checkpoint);
        Ok(())
    }
    async fn allocate_sequence(&self, stream: &StreamId) -> anyhow::Result<u64> {
        let mut sequences = self.sequences.lock().await;
        let previous = sequences.get(stream.as_str()).copied().unwrap_or(0);
        let next = previous
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("replay producer sequence exhausted"))?;
        sequences.insert(stream.to_string(), next);
        Ok(next)
    }
}

/// A graph/consumer-scoped view over an explicitly supplied state store.
/// Handler side effects and checkpoint persistence are not one external transaction.
/// Views sharing the same provider and partition serialize writes in-process.
/// Unidentified legacy records and unknown versions fail instead of seeding a cursor.
pub struct StateStoreConsumerProgress {
    provider: Arc<dyn crate::state_store::StateStoreProvider>,
    partition: String,
    lock: Arc<Mutex<()>>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointRecord {
    version: u32,
    checkpoint: ConsumerCheckpoint,
}

fn state_store_lock(
    provider: &Arc<dyn crate::state_store::StateStoreProvider>,
    partition: &str,
) -> anyhow::Result<Arc<Mutex<()>>> {
    type Locks = HashMap<(usize, String), Weak<Mutex<()>>>;
    static LOCKS: OnceLock<std::sync::Mutex<Locks>> = OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("consumer progress lock registry poisoned"))?;
    locks.retain(|_, lock| lock.strong_count() > 0);
    let key = (
        Arc::as_ptr(provider) as *const () as usize,
        partition.to_owned(),
    );
    if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    let lock = Arc::new(Mutex::new(()));
    locks.insert(key, Arc::downgrade(&lock));
    Ok(lock)
}

impl StateStoreConsumerProgress {
    pub fn new(
        graph: &str,
        consumer: &str,
        provider: Arc<dyn crate::state_store::StateStoreProvider>,
    ) -> anyhow::Result<Self> {
        super::data::validate_identifier("graph", graph)?;
        super::data::validate_identifier("consumer", consumer)?;
        let encode = |value: &str| {
            value
                .bytes()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let partition = format!(
            "computation-consumer:{}:{}",
            encode(graph),
            encode(consumer)
        );
        let lock = state_store_lock(&provider, &partition)?;
        Ok(Self {
            provider,
            partition,
            lock,
        })
    }
    async fn read(&self, key: &str) -> anyhow::Result<Option<u64>> {
        self.provider
            .get(&self.partition, key)
            .await?
            .map(|bytes| {
                Ok(u64::from_be_bytes(bytes.try_into().map_err(|_| {
                    ConsumerRecoveryError::InvalidCheckpoint("invalid producer sequence".into())
                })?))
            })
            .transpose()
    }

    async fn write_checkpoint(
        &self,
        query: &str,
        checkpoint: ConsumerCheckpoint,
    ) -> anyhow::Result<()> {
        self.provider
            .set(
                &self.partition,
                &format!("handled:{query}"),
                serde_json::to_vec(&CheckpointRecord {
                    version: 2,
                    checkpoint,
                })?,
            )
            .await?;
        Ok(())
    }
}

#[async_trait]
impl ConsumerProgressStore for StateStoreConsumerProgress {
    async fn load(&self, query: &str) -> anyhow::Result<Option<ConsumerCheckpoint>> {
        self.provider
            .get(&self.partition, &format!("handled:{query}"))
            .await?
            .map(|bytes| {
                if bytes.len() == 16 {
                    return Err(ConsumerRecoveryError::InvalidCheckpoint(
                        "unidentified legacy cursor requires explicit removal".into(),
                    )
                    .into());
                }
                let record: CheckpointRecord = serde_json::from_slice(&bytes)
                    .map_err(|error| ConsumerRecoveryError::InvalidCheckpoint(error.to_string()))?;
                if record.version != 2 {
                    return Err(ConsumerRecoveryError::InvalidCheckpoint(format!(
                        "unsupported record version {}",
                        record.version
                    ))
                    .into());
                }
                record.checkpoint.validate(query)?;
                Ok(record.checkpoint)
            })
            .transpose()
    }
    async fn commit_handled(
        &self,
        query: &str,
        checkpoint: ConsumerCheckpoint,
    ) -> anyhow::Result<()> {
        checkpoint.validate(query)?;
        let _lock = self.lock.lock().await;
        validate_commit(self.load(query).await?.as_ref(), &checkpoint, false)?;
        self.write_checkpoint(query, checkpoint).await
    }
    async fn commit_recovery(
        &self,
        query: &str,
        previous: Option<ConsumerCheckpoint>,
        checkpoint: ConsumerCheckpoint,
    ) -> anyhow::Result<()> {
        checkpoint.validate(query)?;
        let _lock = self.lock.lock().await;
        if self.load(query).await? != previous {
            return Err(ConsumerRecoveryError::CheckpointConflict.into());
        }
        validate_commit(previous.as_ref(), &checkpoint, true)?;
        self.write_checkpoint(query, checkpoint).await
    }
    async fn allocate_sequence(&self, stream: &StreamId) -> anyhow::Result<u64> {
        let _lock = self.lock.lock().await;
        let key = format!("producer:{stream}");
        let sequence = self
            .read(&key)
            .await?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("replay sequence exhausted"))?;
        self.provider
            .set(&self.partition, &key, sequence.to_be_bytes().to_vec())
            .await?;
        Ok(sequence)
    }
}

pub struct ConsumerProgressResource(pub Arc<dyn ConsumerProgressStore>);
pub struct QueryResultsResource(pub QueryResults);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConsumerRecoveryPolicy {
    Strict,
    AutoReset,
    AutoSkipGap,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsumerRecoveryAction {
    ReplaceSnapshot,
    Skip,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumerRecoveryDecision {
    version: u32,
    pub action: ConsumerRecoveryAction,
    #[serde(deserialize_with = "required_previous")]
    pub previous: Option<ConsumerCheckpoint>,
}

fn required_previous<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<ConsumerCheckpoint>, D::Error> {
    <Option<ConsumerCheckpoint> as serde::Deserialize>::deserialize(deserializer)
}

impl ConsumerRecoveryDecision {
    const ANNOTATION: &'static str = "drasi.consumer-recovery-decision.v1";

    pub fn from_envelope(envelope: &super::ChangeEnvelope) -> anyhow::Result<Option<Self>> {
        let Some(entry) = envelope
            .annotations()
            .entries()
            .find(|entry| entry.key() == Self::ANNOTATION)
        else {
            return Ok(None);
        };
        let super::ContextValue::Bytes(bytes) = entry.value() else {
            return Err(
                ConsumerRecoveryError::InvalidDecision("annotation is not bytes".into()).into(),
            );
        };
        let decision: Self = serde_json::from_slice(&bytes)
            .map_err(|error| ConsumerRecoveryError::InvalidDecision(error.to_string()))?;
        if decision.version != 1 {
            return Err(ConsumerRecoveryError::InvalidDecision(format!(
                "unsupported decision version {}",
                decision.version
            ))
            .into());
        }
        Ok(Some(decision))
    }

    fn annotate(
        &self,
        envelope: &mut super::ChangeEnvelope,
        component: &ComponentId,
    ) -> anyhow::Result<()> {
        envelope.append_annotation(super::ContextEntry::try_new(
            component.clone(),
            Self::ANNOTATION,
            super::ContextValue::Bytes(Arc::from(serde_json::to_vec(self)?)),
        )?)?;
        Ok(())
    }
}

fn read_cursor(
    cursor: &RwLock<Option<ConsumerCheckpoint>>,
) -> anyhow::Result<Option<ConsumerCheckpoint>> {
    Ok(cursor
        .read()
        .map_err(|_| anyhow::anyhow!("consumer replay cursor poisoned"))?
        .clone())
}

struct ReplayWakeup {
    results: QueryResults,
    pending: Arc<AtomicBool>,
    cursor: Arc<RwLock<Option<ConsumerCheckpoint>>>,
}
#[async_trait]
impl WakeupSource for ReplayWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        loop {
            let changed = self.results.notify.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.pending.load(Ordering::Acquire) {
                self.results.wait_recovery_ready().await?;
                return Ok(());
            }
            if self.has_pending().await? {
                return Ok(());
            }
            changed.await;
        }
    }
    async fn has_pending(&self) -> anyhow::Result<bool> {
        if self.pending.load(Ordering::Acquire) {
            return Ok(true);
        }
        let view = match self.results.recovery_view(None) {
            Ok(view) => view,
            Err(QueryHistoryError::NotReady) => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        Ok(read_cursor(&self.cursor)?.as_ref() != Some(&ConsumerCheckpoint::from_view(&view)))
    }
}

/// A query-row transformer that joins an initial snapshot/retained suffix to the
/// already bound live pipe. Consumer checkpoints are written only by actual sinks.
pub struct QueryReplayTransformer {
    descriptor: ComponentDescriptor,
    query_id: String,
    results: QueryResults,
    progress: Arc<dyn ConsumerProgressStore>,
    policy: ConsumerRecoveryPolicy,
    stream: StreamId,
    pending: Arc<AtomicBool>,
    cursor: Arc<RwLock<Option<ConsumerCheckpoint>>>,
}

impl QueryReplayTransformer {
    pub fn new(
        id: ComponentId,
        query_id: String,
        stream: StreamId,
        results: QueryResults,
        progress: Arc<dyn ConsumerProgressStore>,
        policy: ConsumerRecoveryPolicy,
    ) -> Self {
        let descriptor = ComponentDescriptor::try_new(
            id,
            vec![
                PortDescriptor::new(
                    PortId::try_new("in").expect("port"),
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                ),
                PortDescriptor::new(
                    PortId::try_new("out").expect("port"),
                    PortDirection::Output,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                ),
            ],
        )
        .expect("replay descriptor");
        Self {
            descriptor,
            query_id,
            results,
            progress,
            policy,
            stream,
            pending: Arc::new(AtomicBool::new(true)),
            cursor: Arc::new(RwLock::new(None)),
        }
    }

    async fn forward(&self, envelope: &super::ChangeEnvelope) -> anyhow::Result<OutputEnvelope> {
        let sequence = self.progress.allocate_sequence(&self.stream).await?;
        Ok(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: envelope.derive(
                super::emission_id(&self.stream, sequence)?,
                envelope.changes().clone(),
                SystemMetadata::new(self.stream.clone(), sequence).with_timestamp(
                    envelope
                        .system()
                        .timestamp()
                        .unwrap_or_else(chrono::Utc::now),
                ),
            ),
        })
    }

    fn validate_envelope(
        &self,
        envelope: &super::ChangeEnvelope,
        view: &QueryRecoveryView,
    ) -> anyhow::Result<u64> {
        let identity = QueryRecoveryIdentity::from_envelope(envelope)?;
        if identity != view.identity
            || identity.query_id() != self.query_id
            || QueryChangeCodec::metadata(envelope)?.query_id != self.query_id
        {
            return Err(QueryIdentityError::Mismatch.into());
        }
        let generation = QueryChangeCodec::query_generation(envelope)?;
        if generation != view.snapshot.generation {
            return Err(ConsumerRecoveryError::GenerationMismatch {
                expected: view.snapshot.generation,
                actual: generation,
            }
            .into());
        }
        if QueryChangeCodec::is_snapshot(envelope)
            || QueryChangeCodec::is_progress_only(envelope)
            || ConsumerRecoveryDecision::from_envelope(envelope)?.is_some()
        {
            return Err(ConsumerRecoveryError::InvalidInput(
                "consumer recovery control is not a live producer event".into(),
            )
            .into());
        }
        let sequence = QueryChangeCodec::query_sequence(envelope)?;
        if sequence == 0 || sequence > view.snapshot.as_of_sequence {
            return Err(ConsumerRecoveryError::InvalidInput(
                "sequence lies outside committed producer output".into(),
            )
            .into());
        }
        Ok(sequence)
    }

    async fn decision(
        &self,
        view: &QueryRecoveryView,
        previous: Option<ConsumerCheckpoint>,
        action: ConsumerRecoveryAction,
        target_sequence: u64,
    ) -> anyhow::Result<OutputEnvelope> {
        let sequence = self.progress.allocate_sequence(&self.stream).await?;
        let system =
            SystemMetadata::new(self.stream.clone(), sequence).with_timestamp(chrono::Utc::now());
        let mut envelope = match action {
            ConsumerRecoveryAction::ReplaceSnapshot => QueryChangeCodec::snapshot_envelope(
                &self.query_id,
                &view.snapshot,
                self.descriptor.id(),
                system,
            )?,
            ConsumerRecoveryAction::Skip => {
                log::warn!(
                    "Consumer {} explicitly skips to query {} generation {} sequence {}",
                    self.descriptor.id(),
                    self.query_id,
                    view.snapshot.generation,
                    target_sequence
                );
                QueryChangeCodec::progress_envelope(
                    &self.query_id,
                    target_sequence,
                    view.snapshot.generation,
                    self.descriptor.id(),
                    system,
                )?
            }
        };
        view.identity
            .annotate(&mut envelope, self.descriptor.id())?;
        ConsumerRecoveryDecision {
            version: 1,
            action,
            previous,
        }
        .annotate(&mut envelope, self.descriptor.id())?;
        Ok(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope,
        })
    }

    async fn recover(
        &mut self,
        input: Option<&super::ChangeEnvelope>,
    ) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.results.wait_recovery_ready().await?;
        let initial = self.pending.load(Ordering::Acquire);
        let previous = if initial {
            self.progress.load(&self.query_id).await?
        } else {
            read_cursor(&self.cursor)?
        };
        if let Some(previous) = &previous {
            previous.validate(&self.query_id)?;
        }
        let input_sequence = input.map(QueryChangeCodec::query_sequence).transpose()?;
        let replay_after = previous.as_ref().and_then(|previous| {
            (initial
                || input_sequence.is_none()
                || input_sequence.is_some_and(|sequence| {
                    sequence > previous.sequence
                        && previous.sequence.checked_add(1) != Some(sequence)
                }))
            .then_some(previous.sequence)
        });
        let view = self.results.recovery_view(replay_after)?;
        if view.identity.query_id() != self.query_id {
            return Err(QueryIdentityError::Mismatch.into());
        }
        if let Some(input) = input {
            self.validate_envelope(input, &view)?;
        }
        let target = ConsumerCheckpoint::from_view(&view);
        let mut next = target.clone();
        let mut output = Vec::new();
        match &previous {
            None => {
                output.push(
                    self.decision(
                        &view,
                        None,
                        ConsumerRecoveryAction::ReplaceSnapshot,
                        target.sequence,
                    )
                    .await?,
                );
            }
            Some(previous)
                if previous.identity != target.identity
                    || previous.generation != target.generation =>
            {
                if previous.identity == target.identity {
                    if previous.generation > target.generation {
                        return Err(ConsumerRecoveryError::GenerationMismatch {
                            expected: previous.generation,
                            actual: target.generation,
                        }
                        .into());
                    }
                    if previous.sequence > target.sequence {
                        return Err(ConsumerRecoveryError::CheckpointAhead {
                            sequence: previous.sequence,
                            latest: target.sequence,
                        }
                        .into());
                    }
                }
                let action = match self.policy {
                    ConsumerRecoveryPolicy::Strict => {
                        return Err(if previous.identity != target.identity {
                            QueryIdentityError::Mismatch.into()
                        } else {
                            ConsumerRecoveryError::ResetRequired.into()
                        });
                    }
                    ConsumerRecoveryPolicy::AutoReset => ConsumerRecoveryAction::ReplaceSnapshot,
                    ConsumerRecoveryPolicy::AutoSkipGap => ConsumerRecoveryAction::Skip,
                };
                output.push(
                    self.decision(&view, Some(previous.clone()), action, target.sequence)
                        .await?,
                );
            }
            Some(previous) => {
                if previous.sequence > target.sequence {
                    return Err(ConsumerRecoveryError::CheckpointAhead {
                        sequence: previous.sequence,
                        latest: target.sequence,
                    }
                    .into());
                }
                if !initial && input_sequence.is_some_and(|sequence| sequence <= previous.sequence)
                {
                    return Ok(Vec::new());
                }
                if target.sequence > previous.sequence {
                    if let Some(history) = &view.retained {
                        let mut last = None;
                        for envelope in history {
                            let sequence = self.validate_envelope(envelope, &view)?;
                            if last.is_some_and(|last: u64| last.checked_add(1) != Some(sequence)) {
                                return Err(ConsumerRecoveryError::InvalidInput(
                                    "retained suffix has an interior gap".into(),
                                )
                                .into());
                            }
                            last = Some(sequence);
                        }
                        if last.is_some_and(|last| last != target.sequence) {
                            return Err(ConsumerRecoveryError::InvalidInput(
                                "retained suffix does not end at committed query head".into(),
                            )
                            .into());
                        }
                        let first = history
                            .first()
                            .map(QueryChangeCodec::query_sequence)
                            .transpose()?;
                        let gap = previous.sequence.checked_add(1) != first;
                        if gap {
                            match self.policy {
                                ConsumerRecoveryPolicy::Strict => {
                                    return Err(QueryHistoryError::Unavailable {
                                        requested: previous.sequence,
                                        oldest: view.oldest_sequence.unwrap_or(target.sequence),
                                        latest: target.sequence,
                                    }
                                    .into());
                                }
                                ConsumerRecoveryPolicy::AutoReset => {
                                    output.push(
                                        self.decision(
                                            &view,
                                            Some(previous.clone()),
                                            ConsumerRecoveryAction::ReplaceSnapshot,
                                            target.sequence,
                                        )
                                        .await?,
                                    );
                                }
                                ConsumerRecoveryPolicy::AutoSkipGap => {
                                    let skipped_to = first
                                        .map(|first| {
                                            first
                                                .checked_sub(1)
                                                .ok_or(ConsumerRecoveryError::SequenceExhausted)
                                        })
                                        .transpose()?
                                        .unwrap_or(target.sequence);
                                    output.push(
                                        self.decision(
                                            &view,
                                            Some(previous.clone()),
                                            ConsumerRecoveryAction::Skip,
                                            skipped_to,
                                        )
                                        .await?,
                                    );
                                }
                            }
                        }
                        if !gap || self.policy == ConsumerRecoveryPolicy::AutoSkipGap {
                            for envelope in history {
                                output.push(self.forward(envelope).await?);
                            }
                        }
                    } else if let Some(input) = input {
                        let sequence = self.validate_envelope(input, &view)?;
                        if previous.sequence.checked_add(1) != Some(sequence) {
                            return Err(ConsumerRecoveryError::Gap {
                                handled: previous.sequence,
                                received: sequence,
                            }
                            .into());
                        }
                        output.push(self.forward(input).await?);
                        next.sequence = sequence;
                    }
                }
            }
        }
        let current = self.results.recovery_view(None)?;
        if current.identity != view.identity
            || current.snapshot.generation != view.snapshot.generation
        {
            return Err(QueryIdentityError::Mismatch.into());
        }
        *self
            .cursor
            .write()
            .map_err(|_| anyhow::anyhow!("consumer replay cursor poisoned"))? = Some(next);
        self.pending.store(false, Ordering::Release);
        Ok(output)
    }
}

#[async_trait]
impl ComputationComponent for QueryReplayTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        *self
            .cursor
            .write()
            .map_err(|_| anyhow::anyhow!("consumer replay cursor poisoned"))? = None;
        self.pending.store(true, Ordering::Release);
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Transformer for QueryReplayTransformer {
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        Some(Arc::new(ReplayWakeup {
            results: self.results.clone(),
            pending: self.pending.clone(),
            cursor: self.cursor.clone(),
        }))
    }
    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.recover(None).await
    }
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.recover(Some(&input.envelope)).await
    }
}

/// Native handling checkpoint adapter. Acceptance-only legacy queues are rejected.
pub struct CheckpointedSink {
    inner: Box<dyn EnvelopeSink>,
    progress: Arc<dyn ConsumerProgressStore>,
    accept_skipped_resets: bool,
}

impl CheckpointedSink {
    pub fn new(
        inner: Box<dyn EnvelopeSink>,
        progress: Arc<dyn ConsumerProgressStore>,
    ) -> anyhow::Result<Self> {
        if inner.completion() != SinkCompletion::Handled {
            anyhow::bail!("handled checkpoints cannot wrap an acceptance-only sink");
        }
        Ok(Self {
            inner,
            progress,
            accept_skipped_resets: false,
        })
    }

    /// Accept explicit progress-only AutoSkipGap decisions, including their
    /// expected prior cursor. This never permits arbitrary live discontinuities.
    pub fn allow_skipped_resets(mut self) -> Self {
        self.accept_skipped_resets = true;
        self
    }
}

#[async_trait]
impl ComputationComponent for CheckpointedSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.inner.start().await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.inner.stop().await
    }
}

#[async_trait]
impl EnvelopeSink for CheckpointedSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn supports_snapshot(&self) -> bool {
        self.inner.supports_snapshot()
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let query = QueryChangeCodec::metadata(&input.envelope)?.query_id;
        let sequence = QueryChangeCodec::query_sequence(&input.envelope)?;
        let generation = QueryChangeCodec::query_generation(&input.envelope)?;
        let checkpoint = ConsumerCheckpoint {
            sequence,
            generation,
            identity: QueryRecoveryIdentity::from_envelope(&input.envelope)?,
        };
        checkpoint.validate(&query)?;
        let decision = ConsumerRecoveryDecision::from_envelope(&input.envelope)?;
        let snapshot = QueryChangeCodec::is_snapshot(&input.envelope);
        let progress_only = QueryChangeCodec::is_progress_only(&input.envelope);
        match &decision {
            Some(decision) => {
                if let Some(previous) = &decision.previous {
                    previous.validate(&query)?;
                }
                match decision.action {
                    ConsumerRecoveryAction::ReplaceSnapshot if snapshot && !progress_only => {}
                    ConsumerRecoveryAction::Skip
                        if progress_only
                            && !snapshot
                            && input.envelope.changes().operations().is_empty() =>
                    {
                        if !self.accept_skipped_resets {
                            return Err(ConsumerRecoveryError::SkipNotAllowed.into());
                        }
                    }
                    _ => {
                        return Err(ConsumerRecoveryError::InvalidDecision(
                            "decision does not match the envelope's control kind".into(),
                        )
                        .into());
                    }
                }
            }
            None if snapshot || progress_only => {
                return Err(ConsumerRecoveryError::ResetRequired.into());
            }
            None if sequence == 0 => {
                return Err(ConsumerRecoveryError::InvalidInput(
                    "live query sequences begin at one".into(),
                )
                .into());
            }
            None => {}
        }
        let handled = self.progress.load(&query).await?;
        if let Some(handled) = &handled {
            handled.validate(&query)?;
            if handled.identity == checkpoint.identity
                && handled.generation == generation
                && handled.sequence >= sequence
            {
                return Ok(());
            }
        }
        if let Some(decision) = decision {
            if handled != decision.previous {
                return Err(ConsumerRecoveryError::CheckpointConflict.into());
            }
            validate_commit(handled.as_ref(), &checkpoint, true)?;
            if decision.action == ConsumerRecoveryAction::ReplaceSnapshot {
                if !self.inner.supports_snapshot() {
                    anyhow::bail!("sink does not support explicit snapshot replacement");
                }
                self.inner.replace_snapshot(input).await?;
            }
            self.progress
                .commit_recovery(&query, handled, checkpoint)
                .await
        } else {
            validate_commit(handled.as_ref(), &checkpoint, false)?;
            let previous = handled.as_ref().map_or(0, |handled| handled.sequence);
            if previous
                .checked_add(1)
                .ok_or(ConsumerRecoveryError::SequenceExhausted)?
                != sequence
            {
                return Err(ConsumerRecoveryError::Gap {
                    handled: previous,
                    received: sequence,
                }
                .into());
            }
            self.inner.handle(input).await?;
            self.progress.commit_handled(&query, checkpoint).await
        }
    }
}

pub struct QueryReplayFactory {
    descriptor: super::FactoryDescriptor,
}

impl Default for QueryReplayFactory {
    fn default() -> Self {
        Self {
            descriptor: super::FactoryDescriptor {
                implementation: super::ImplementationIdentity::try_new("drasi/query-replay", "1")
                    .expect("implementation"),
                role: super::ComponentRole::Transformer,
                configuration_version: 1,
                configuration: super::ConfigurationSchema {
                    fields: [("query_id", true), ("stream", true), ("recovery", false)]
                        .into_iter()
                        .map(|(name, required)| {
                            (
                                Arc::from(name),
                                super::ConfigurationField {
                                    value_type: super::ConfigurationType::String,
                                    required,
                                    secret: false,
                                },
                            )
                        })
                        .collect(),
                    allow_additional: false,
                },
                dependencies: std::collections::BTreeMap::from([
                    (
                        Arc::from("results"),
                        super::ResourceRequirement::exactly_one::<QueryResultsResource>(
                            super::ResourceRole::LiveResults,
                        ),
                    ),
                    (
                        Arc::from("progress"),
                        super::ResourceRequirement::exactly_one::<ConsumerProgressResource>(
                            super::ResourceRole::StateStore,
                        ),
                    ),
                ]),
            },
        }
    }
}

#[async_trait]
impl super::ComponentFactory for QueryReplayFactory {
    fn descriptor(&self) -> &super::FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &super::ComponentSpecification) -> anyhow::Result<()> {
        for port in spec.descriptor.ports() {
            super::data::validate_schema(QueryChangeCodec::schema().descriptor(), port.schema())?;
        }
        if spec.descriptor.ports().len() != 2
            || !spec
                .descriptor
                .ports()
                .iter()
                .any(|port| port.id().as_str() == "in" && port.direction() == PortDirection::Input)
            || !spec.descriptor.ports().iter().any(|port| {
                port.id().as_str() == "out" && port.direction() == PortDirection::Output
            })
        {
            anyhow::bail!("query replay requires one input and one output");
        }
        if let Some(super::ConfigurationValue::Literal(value)) = spec.configuration.get("stream") {
            StreamId::try_new(
                value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("invalid replay stream"))?,
            )?;
        }
        if let Some(super::ConfigurationValue::Literal(value)) = spec.configuration.get("recovery")
        {
            if !matches!(
                value.as_str(),
                Some("strict" | "auto_reset" | "auto_skip_gap")
            ) {
                anyhow::bail!("unknown consumer recovery policy");
            }
        }
        Ok(())
    }
    async fn create(
        &self,
        context: super::ConstructionContext,
    ) -> Result<super::ConstructedComponent, super::ComponentCreationError> {
        let results = context
            .resources::<QueryResultsResource>("results")
            .map_err(super::ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                super::ComponentCreationError::terminal(anyhow::anyhow!("missing query results"))
            })?;
        let progress = context
            .resources::<ConsumerProgressResource>("progress")
            .map_err(super::ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                super::ComponentCreationError::terminal(anyhow::anyhow!(
                    "missing consumer progress"
                ))
            })?;
        let config = context.configuration();
        let query = config
            .get("query_id")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                super::ComponentCreationError::terminal(anyhow::anyhow!("missing query id"))
            })?;
        let stream = config
            .get("stream")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| {
                super::ComponentCreationError::terminal(anyhow::anyhow!("missing stream"))
            })?;
        let policy = match config
            .get("recovery")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("strict")
        {
            "strict" => ConsumerRecoveryPolicy::Strict,
            "auto_reset" => ConsumerRecoveryPolicy::AutoReset,
            "auto_skip_gap" => ConsumerRecoveryPolicy::AutoSkipGap,
            _ => {
                return Err(super::ComponentCreationError::terminal(anyhow::anyhow!(
                    "unknown consumer recovery policy"
                )))
            }
        };
        let instance = QueryReplayTransformer::new(
            context.component_id.clone(),
            query.to_owned(),
            StreamId::try_new(stream).map_err(super::ComponentCreationError::terminal)?,
            results.0.clone(),
            progress.0.clone(),
            policy,
        );
        Ok(super::ConstructedComponent::transformer(Box::new(instance)))
    }
}
