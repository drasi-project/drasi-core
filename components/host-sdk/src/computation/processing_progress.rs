// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    ComponentId, PortId, SourceProgressKey, SourceProgressProvider, SourceProgressSnapshot,
    SourceProgressUpdates, SourceProgressView, StreamId, BOOTSTRAP_COMPLETE_CHECKPOINT,
    MAX_SOURCE_POSITION_BYTES,
};
use drasi_core::{
    computation::{ComputationIndexes, ComputationTransaction},
    interface::{CheckpointStore, FailureMode, SourceCheckpoint},
};
use drasi_core_types::CheckpointKey;
use drasi_lib::computation::v1::{
    CommittedProgressPublication, GraphInputProgress, QuerySourceProgress,
};
use tokio::sync::watch;

pub(super) const PREFIX: &str = "\0computation:processing-input:v1:";
pub(super) const TRANSPORT_PREFIX: &str = "\0computation:processing-transport:v1:";
pub(super) const RESET_GENERATION: &str = "\0computation:processing-reset-generation:v1";
const INPUT_PRODUCER: &str = "\0native-query:input-producer:v1:";

fn producer_key(stream: &StreamId) -> CheckpointKey {
    CheckpointKey::Stream(stream.clone()).associated(INPUT_PRODUCER)
}

async fn read_producer(
    store: &dyn CheckpointStore,
    stream: &StreamId,
) -> Result<Option<drasi_computation_contracts::GraphProducerIdentity>> {
    store
        .read_checkpoint(&producer_key(stream))
        .await?
        .map(|saved| {
            anyhow::ensure!(saved.sequence == 1, "invalid input producer version");
            let producer: drasi_computation_contracts::GraphProducerIdentity =
                serde_json::from_slice(
                    saved
                        .source_position
                        .as_deref()
                        .context("missing input producer identity")?,
                )?;
            producer.validate()?;
            Ok(producer)
        })
        .transpose()
}

pub(super) fn reserved(key: &str) -> bool {
    key == RESET_GENERATION
        || key.starts_with(PREFIX)
        || key.starts_with(TRANSPORT_PREFIX)
        || key.starts_with(INPUT_PRODUCER)
        || key == BOOTSTRAP_COMPLETE_CHECKPOINT
}

pub(super) struct Progress {
    graph: String,
    component: ComponentId,
    port: PortId,
    current: Mutex<Arc<Epoch>>,
    publication: Arc<CommittedProgressPublication>,
    clock:
        std::sync::OnceLock<std::sync::Weak<drasi_lib::computation::v1::QuerySchedulingResource>>,
    pub(super) serial: tokio::sync::Mutex<()>,
    volatile_store: Option<Arc<dyn CheckpointStore>>,
    non_atomic: bool,
    deferred: Mutex<Option<Box<DeferredProgress>>>,
    transport: std::sync::OnceLock<Arc<std::sync::atomic::AtomicU64>>,
}

#[derive(Clone)]
struct DeferredProgress {
    generation: u64,
    snapshot: Option<SourceProgressSnapshot>,
    bootstrap: Option<bool>,
}

pub(super) struct Epoch {
    graph: String,
    component: ComponentId,
    publication: Arc<CommittedProgressPublication>,
    generation: u64,
    revoked: Arc<AtomicBool>,
}

impl Epoch {
    fn new(
        graph: String,
        component: ComponentId,
        publication: Arc<CommittedProgressPublication>,
    ) -> Result<Self> {
        let generation = publication.generation()?;
        Ok(Self {
            graph,
            component,
            publication,
            generation,
            revoked: Arc::new(AtomicBool::new(false)),
        })
    }

    fn ensure_current(&self) -> Result<()> {
        anyhow::ensure!(
            !self.revoked.load(Ordering::Acquire),
            "native committed progress view is revoked"
        );
        Ok(())
    }

    fn fail(&self, additional_failure: Option<&AtomicBool>) {
        self.publication.fail(self.generation, additional_failure);
    }
}

impl SourceProgressProvider for Epoch {
    fn graph_id(&self) -> &str {
        &self.graph
    }
    fn component_id(&self) -> &ComponentId {
        &self.component
    }

    fn snapshot(&self) -> Result<Arc<SourceProgressSnapshot>> {
        self.ensure_current()?;
        let snapshot = self.publication.reader().snapshot();
        self.ensure_current()?;
        Ok(snapshot)
    }

    fn subscribe(&self) -> Result<Box<dyn SourceProgressUpdates>> {
        self.ensure_current()?;
        Ok(Box::new(Updates {
            revoked: self.revoked.clone(),
            receiver: self.publication.reader().subscribe(),
        }))
    }
}

struct Updates {
    revoked: Arc<AtomicBool>,
    receiver: watch::Receiver<Arc<SourceProgressSnapshot>>,
}

#[async_trait]
impl SourceProgressUpdates for Updates {
    fn snapshot(&mut self) -> Result<Arc<SourceProgressSnapshot>> {
        anyhow::ensure!(
            !self.revoked.load(Ordering::Acquire),
            "native committed progress subscription is revoked"
        );
        let snapshot = self.receiver.borrow_and_update().clone();
        anyhow::ensure!(
            !self.revoked.load(Ordering::Acquire),
            "native committed progress subscription is revoked"
        );
        Ok(snapshot)
    }
    async fn changed(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.revoked.load(Ordering::Acquire),
            "native committed progress subscription is revoked"
        );
        self.receiver.changed().await?;
        anyhow::ensure!(
            !self.revoked.load(Ordering::Acquire),
            "native committed progress subscription is revoked"
        );
        Ok(())
    }
}

pub(crate) struct Call {
    epoch: Arc<Epoch>,
    complete: bool,
    failure: Option<Arc<AtomicBool>>,
}

impl Call {
    pub(super) fn with_failure(mut self, failure: Arc<AtomicBool>) -> Self {
        self.failure = Some(failure);
        self
    }
    pub(crate) fn complete(mut self) {
        self.complete = true;
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        if !self.complete {
            self.epoch.fail(self.failure.as_deref());
        }
    }
}

impl Progress {
    pub(super) fn new(graph: String, component: ComponentId, port: PortId) -> Result<Self> {
        Self::with_reader(graph, component, port, None, false, false, false)
    }

    pub(super) fn with_reader(
        graph: String,
        component: ComponentId,
        port: PortId,
        reader: Option<Arc<QuerySourceProgress>>,
        volatile: bool,
        non_atomic: bool,
        replacement: bool,
    ) -> Result<Self> {
        let reader = match reader {
            Some(reader) => {
                anyhow::ensure!(
                    reader.graph_id() == graph && reader.component_id() == &component,
                    "processing progress belongs to another graph or component"
                );
                reader
            }
            None => Arc::new(QuerySourceProgress::new(graph.clone(), component.clone())?),
        };
        let publication = Arc::new(if volatile && replacement {
            CommittedProgressPublication::for_replacement_volatile_reader(reader)?
        } else if non_atomic && replacement {
            CommittedProgressPublication::for_replacement_non_atomic_reader(reader)?
        } else if replacement {
            CommittedProgressPublication::for_replacement_reader(reader)?
        } else if volatile {
            CommittedProgressPublication::for_volatile_reader(reader)?
        } else if non_atomic {
            CommittedProgressPublication::for_non_atomic_reader(reader)?
        } else {
            CommittedProgressPublication::for_reader(reader)?
        });
        Ok(Self {
            current: Mutex::new(Arc::new(Epoch::new(
                graph.clone(),
                component.clone(),
                publication.clone(),
            )?)),
            publication,
            clock: std::sync::OnceLock::new(),
            graph,
            component,
            port,
            serial: tokio::sync::Mutex::new(()),
            transport: std::sync::OnceLock::new(),
            non_atomic,
            deferred: Mutex::new(None),
            volatile_store: volatile.then(|| Arc::new(
                drasi_core::in_memory_index::in_memory_checkpoint_store::InMemoryCheckpointStore::new(),
            ) as Arc<dyn CheckpointStore>),
        })
    }

    pub(super) fn bind_owner(&self, owner: &Arc<ComputationTransaction>) -> Result<()> {
        self.publication.bind_owner(owner)
    }

    pub(super) fn is_volatile(&self) -> bool {
        self.volatile_store.is_some()
    }

    pub(super) fn is_non_atomic(&self) -> bool {
        self.non_atomic
    }

    pub(super) fn has_deferred(&self) -> Result<bool> {
        if !self.non_atomic {
            return Ok(false);
        }
        Ok(self
            .deferred
            .lock()
            .map_err(|_| anyhow::anyhow!("deferred input progress poisoned"))?
            .is_some())
    }

    pub(super) async fn discard_volatile_state(&self) -> Result<()> {
        if let Some(store) = &self.volatile_store {
            store.clear_checkpoints().await?;
        }
        Ok(())
    }

    pub(super) fn checkpoint_store(
        &self,
        resources: &ComputationIndexes,
    ) -> Result<Arc<dyn CheckpointStore>> {
        resources
            .checkpoint_store()
            .cloned()
            .or_else(|| self.volatile_store.clone())
            .context("missing input checkpoint store")
    }

    pub(super) fn reset_after_quiesce(&self, owner: &Arc<ComputationTransaction>) -> Result<()> {
        self.suspend()?;
        self.publication.reset_after_quiesce(owner)?;
        *self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("native progress epoch poisoned"))? =
            Arc::new(Epoch::new(
                self.graph.clone(),
                self.component.clone(),
                self.publication.clone(),
            )?);
        Ok(())
    }

    pub(super) fn bind_clock(
        &self,
        clock: &Arc<drasi_lib::computation::v1::QuerySchedulingResource>,
    ) -> Result<()> {
        self.clock
            .set(Arc::downgrade(clock))
            .map_err(|_| anyhow::anyhow!("query clock is already bound"))
    }

    fn physical_time(&self) -> Result<u64> {
        match self.clock.get() {
            Some(clock) => clock
                .upgrade()
                .context("query clock was released")?
                .physical_time(),
            None => Ok(u64::try_from(chrono::Utc::now().timestamp_millis())?),
        }
    }

    pub(super) fn bind_transport(&self, counter: Arc<std::sync::atomic::AtomicU64>) -> Result<()> {
        self.transport
            .set(counter)
            .map_err(|_| anyhow::anyhow!("query stream counter is already bound"))
    }

    fn next_emission(&self, stored: u64) -> Result<u64> {
        match self.transport.get() {
            Some(counter) => counter
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                    current.max(stored).checked_add(1)
                })
                .map(|previous| previous.max(stored) + 1)
                .map_err(|_| anyhow::anyhow!("query emission sequence exhausted")),
            None => stored
                .checked_add(1)
                .context("query emission sequence exhausted"),
        }
    }

    pub(super) fn owner_reader(&self) -> Arc<QuerySourceProgress> {
        self.publication.reader()
    }

    pub(super) fn epoch(&self) -> Result<Arc<Epoch>> {
        Ok(self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("native progress epoch poisoned"))?
            .clone())
    }

    pub(super) fn view(&self) -> Result<SourceProgressView> {
        Ok(SourceProgressView::new(self.epoch()?))
    }

    pub(super) fn call(&self) -> Result<Call> {
        let epoch = self.epoch()?;
        epoch.ensure_current()?;
        anyhow::ensure!(
            epoch.snapshot()?.failure.is_none(),
            "native committed progress is fenced"
        );
        Ok(Call {
            epoch,
            complete: false,
            failure: None,
        })
    }

    pub(super) fn cleanup_call(&self) -> Result<Call> {
        // Cleanup must remain possible after a failed call revoked or fenced this epoch.
        Ok(Call {
            epoch: self.epoch()?,
            complete: false,
            failure: None,
        })
    }

    pub(super) fn ready(&self) -> Result<()> {
        let epoch = self.epoch()?;
        epoch.ensure_current()?;
        self.publication.ready(epoch.generation)
    }

    pub(super) fn suspend(&self) -> Result<()> {
        let epoch = self.epoch()?;
        if !epoch.revoked.swap(true, Ordering::AcqRel) {
            self.publication.suspend(epoch.generation)?;
        }
        Ok(())
    }

    pub(super) fn fail(&self) -> Result<()> {
        let epoch = self.epoch()?;
        epoch.revoked.store(true, Ordering::Release);
        epoch.fail(None);
        Ok(())
    }

    pub(super) fn retire_closed(&self) -> Result<()> {
        self.epoch()?.revoked.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) fn reset(&self) -> Result<()> {
        self.suspend()?;
        self.publication.reset_after_close()?;
        *self
            .deferred
            .lock()
            .map_err(|_| anyhow::anyhow!("deferred input progress poisoned"))? = None;
        *self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("native progress epoch poisoned"))? =
            Arc::new(Epoch::new(
                self.graph.clone(),
                self.component.clone(),
                self.publication.clone(),
            )?);
        Ok(())
    }

    pub(super) fn key(&self, key: &SourceProgressKey) -> Result<CheckpointKey> {
        let key = match key {
            SourceProgressKey::Source(id) => CheckpointKey::Source(id.clone()),
            SourceProgressKey::Stream(stream) => CheckpointKey::Stream(stream.clone()),
        };
        Ok(key.associated(format!("{PREFIX}{}", serde_json::to_string(&self.port)?)))
    }

    fn transport_key(&self, stream: &StreamId) -> Result<CheckpointKey> {
        Ok(CheckpointKey::Stream(stream.clone()).associated(format!(
            "{TRANSPORT_PREFIX}{}",
            serde_json::to_string(&self.port)?
        )))
    }
}

pub(super) struct Draft {
    progress: Arc<Progress>,
    epoch: Arc<Epoch>,
    state: Mutex<Option<SourceProgressSnapshot>>,
    bootstrap: Mutex<Option<bool>>,
    input_seen: AtomicBool,
    prepared_input: Mutex<Option<GraphInputProgress>>,
    resumed: AtomicBool,
}

impl Draft {
    pub(super) fn next_emission(&self, stored: u64) -> Result<u64> {
        self.epoch.ensure_current()?;
        self.progress.next_emission(stored)
    }

    pub(super) fn is_volatile(&self) -> bool {
        self.progress.is_volatile()
    }

    pub(super) fn is_non_atomic(&self) -> bool {
        self.progress.is_non_atomic()
    }

    pub(super) fn checkpoint_store(
        &self,
        resources: &ComputationIndexes,
    ) -> Result<Arc<dyn CheckpointStore>> {
        self.progress.checkpoint_store(resources)
    }

    pub(super) fn physical_time(&self) -> Result<u64> {
        self.epoch.ensure_current()?;
        self.progress.physical_time()
    }

    pub(super) fn new(progress: &Arc<Progress>) -> Result<Self> {
        let epoch = progress.epoch()?;
        epoch.ensure_current()?;
        anyhow::ensure!(
            epoch.snapshot()?.failure.is_none(),
            "native committed progress is fenced"
        );
        Ok(Self {
            progress: progress.clone(),
            epoch,
            state: Mutex::new(None),
            bootstrap: Mutex::new(None),
            input_seen: AtomicBool::new(false),
            prepared_input: Mutex::new(None),
            resumed: AtomicBool::new(false),
        })
    }

    pub(super) fn validate_completion(&self) -> Result<()> {
        anyhow::ensure!(
            self.prepared_input
                .lock()
                .map_err(|_| anyhow::anyhow!("prepared input progress poisoned"))?
                .is_none(),
            "prepared input progress was not staged"
        );
        anyhow::ensure!(
            !self.progress.has_deferred()? || self.resumed.load(Ordering::Acquire),
            "non-atomic output must resume its committed input progress"
        );
        Ok(())
    }

    pub(super) fn defer(&self) -> Result<()> {
        self.epoch.ensure_current()?;
        anyhow::ensure!(
            self.is_non_atomic(),
            "deferred progress requires non-atomic publication"
        );
        let snapshot = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("staged input progress poisoned"))?
            .take();
        let bootstrap = *self
            .bootstrap
            .lock()
            .map_err(|_| anyhow::anyhow!("staged bootstrap progress poisoned"))?;
        let mut deferred = self
            .progress
            .deferred
            .lock()
            .map_err(|_| anyhow::anyhow!("deferred input progress poisoned"))?;
        anyhow::ensure!(
            deferred.is_none(),
            "non-atomic publication is already pending"
        );
        *deferred = Some(Box::new(DeferredProgress {
            generation: self.epoch.generation,
            snapshot,
            bootstrap,
        }));
        Ok(())
    }

    pub(super) fn resume(&self) -> Result<()> {
        self.epoch.ensure_current()?;
        anyhow::ensure!(
            self.is_non_atomic() && !self.resumed.load(Ordering::Acquire),
            "input progress can resume once during non-atomic output publication"
        );
        let deferred = self
            .progress
            .deferred
            .lock()
            .map_err(|_| anyhow::anyhow!("deferred input progress poisoned"))?;
        let deferred = deferred
            .as_ref()
            .context("no committed input progress is awaiting publication")?;
        anyhow::ensure!(
            deferred.generation == self.epoch.generation
                && !self.input_seen.load(Ordering::Acquire),
            "deferred progress belongs to another operation or activation"
        );
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("staged input progress poisoned"))?;
        anyhow::ensure!(state.is_none(), "cannot replace staged input progress");
        *state = deferred.snapshot.clone();
        *self
            .bootstrap
            .lock()
            .map_err(|_| anyhow::anyhow!("staged bootstrap progress poisoned"))? =
            deferred.bootstrap;
        self.resumed.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) async fn prepare_input(
        &self,
        resources: &ComputationIndexes,
        input: drasi_computation_contracts::InputEnvelope,
    ) -> Result<drasi_computation_plugin_sdk::processing::InputProgressDecision> {
        self.epoch.ensure_current()?;
        anyhow::ensure!(
            !self.progress.has_deferred()?,
            "finish pending non-atomic output before preparing another input"
        );
        anyhow::ensure!(
            input.port == self.progress.port && !self.input_seen.swap(true, Ordering::AcqRel),
            "processing accepts one prepared input on its declared progress port"
        );
        let (mut progress, saved, decision) = self.resolve_input(resources, &input).await?;
        if decision.duplicate {
            self.stage_input_transports(resources, &progress).await?;
        } else {
            progress.retain_valid_position(saved.as_ref(), &self.progress.component);
            *self
                .prepared_input
                .lock()
                .map_err(|_| anyhow::anyhow!("prepared input progress poisoned"))? = Some(progress);
        }
        Ok(decision)
    }

    pub(super) async fn validate_input(
        &self,
        resources: &ComputationIndexes,
        input: drasi_computation_contracts::InputEnvelope,
    ) -> Result<drasi_computation_plugin_sdk::processing::InputProgressDecision> {
        Ok(self.resolve_input(resources, &input).await?.2)
    }

    async fn resolve_input(
        &self,
        resources: &ComputationIndexes,
        input: &drasi_computation_contracts::InputEnvelope,
    ) -> Result<(
        GraphInputProgress,
        Option<SourceCheckpoint>,
        drasi_computation_plugin_sdk::processing::InputProgressDecision,
    )> {
        self.epoch.ensure_current()?;
        anyhow::ensure!(
            input.port == self.progress.port,
            "input uses another progress port"
        );
        let reader = self.progress.owner_reader();
        let progress = GraphInputProgress::for_query(&input.envelope, Some(&reader))?;
        let store = self.checkpoint_store(resources)?;
        let saved = store
            .read_checkpoint(&self.key(progress.identity())?)
            .await?;
        let producer = read_producer(store.as_ref(), progress.owner_stream()).await?;
        progress.validate_durability(store.is_persistent())?;
        progress.validate_owner(
            saved.as_ref().map(|saved| saved.sequence),
            producer.as_ref(),
        )?;
        let duplicate = progress.is_duplicate(saved.as_ref(), None, Some(&reader))?;
        let source_id = match progress.identity() {
            SourceProgressKey::Source(source) => Some(source.clone()),
            SourceProgressKey::Stream(_) => None,
        };
        Ok((
            progress,
            saved,
            drasi_computation_plugin_sdk::processing::InputProgressDecision {
                duplicate,
                source_id,
            },
        ))
    }

    async fn stage_input_transports(
        &self,
        resources: &ComputationIndexes,
        input: &GraphInputProgress,
    ) -> Result<()> {
        let mut transports = std::collections::BTreeMap::new();
        input.record_transport(&mut transports);
        for (stream, sequence) in transports {
            self.stage_transport(resources, stream, sequence).await?;
        }
        Ok(())
    }

    pub(super) async fn stage_prepared_input(&self, resources: &ComputationIndexes) -> Result<()> {
        let input = self
            .prepared_input
            .lock()
            .map_err(|_| anyhow::anyhow!("prepared input progress poisoned"))?
            .take()
            .context("no prepared input progress")?;
        if let Some(producer) = input.producer() {
            self.checkpoint_store(resources)?
                .stage_checkpoint(
                    &producer_key(input.owner_stream()),
                    1,
                    Some(&serde_json::to_vec(producer)?.into()),
                )
                .await?;
        }
        self.stage(
            resources,
            input.identity().clone(),
            input.checkpoint(),
            None,
        )
        .await?;
        self.stage_input_transports(resources, &input).await
    }

    pub(super) async fn bind_producer(
        &self,
        resources: &ComputationIndexes,
        producer: drasi_computation_contracts::GraphProducerIdentity,
        key: SourceProgressKey,
    ) -> Result<()> {
        self.epoch.ensure_current()?;
        producer.validate()?;
        anyhow::ensure!(
            (self.is_volatile() || producer.persistent())
                && key == SourceProgressKey::Stream(producer.stream().clone()),
            "persistent input progress requires its actual producer stream and epoch"
        );
        let store = self.checkpoint_store(resources)?;
        if let Some(saved) = read_producer(store.as_ref(), producer.stream()).await? {
            anyhow::ensure!(
                saved == producer,
                "input producer epoch changed; bootstrap reset is required"
            );
        } else {
            anyhow::ensure!(
                store.read_checkpoint(&self.key(&key)?).await?.is_none(),
                "cannot reinterpret raw progress as producer progress"
            );
            store
                .stage_checkpoint(
                    &producer_key(producer.stream()),
                    1,
                    Some(&serde_json::to_vec(&producer)?.into()),
                )
                .await?;
        }
        Ok(())
    }

    pub(super) fn key(&self, key: &SourceProgressKey) -> Result<CheckpointKey> {
        self.progress.key(key)
    }

    pub(super) async fn recover(&self, resources: &ComputationIndexes) -> Result<()> {
        let current = self.snapshot()?;
        anyhow::ensure!(
            !current.recovered && current.failure.is_none(),
            "committed input progress is already recovered or fenced"
        );
        let store = self.checkpoint_store(resources)?;
        if self.is_volatile() {
            anyhow::ensure!(
                !store.is_persistent(),
                "volatile progress cannot restore persistent checkpoints"
            );
        } else {
            if !self.is_non_atomic() {
                resources.atomic_result_transaction()?;
                resources
                    .durability()
                    .require(FailureMode::ProcessRestart)?;
            }
            anyhow::ensure!(
                store.is_persistent(),
                "input progress requires persistent checkpoints"
            );
        }
        let mut snapshot = SourceProgressSnapshot {
            recovered: true,
            persistent: !self.is_volatile(),
            ..Default::default()
        };
        for (key, checkpoint) in store.read_all_checkpoints().await? {
            if key.metadata_name() == Some(RESET_GENERATION) {
                anyhow::ensure!(
                    checkpoint.source_position.is_none(),
                    "invalid processing reset generation"
                );
                snapshot.reset_generation = checkpoint.sequence;
                continue;
            }
            if key.metadata_name() == Some(BOOTSTRAP_COMPLETE_CHECKPOINT) {
                anyhow::ensure!(
                    checkpoint.sequence <= 1 && checkpoint.source_position.is_none(),
                    "invalid committed bootstrap marker"
                );
                snapshot.bootstrap_complete = checkpoint.sequence == 1;
                *self
                    .bootstrap
                    .lock()
                    .map_err(|_| anyhow::anyhow!("staged bootstrap progress poisoned"))? =
                    Some(snapshot.bootstrap_complete);
                continue;
            }
            let CheckpointKey::Associated {
                checkpoint: identity,
                name,
            } = &key
            else {
                continue;
            };
            if let Some(encoded) = name.strip_prefix(TRANSPORT_PREFIX) {
                let port: PortId = serde_json::from_str(encoded)?;
                let CheckpointKey::Stream(stream) = identity.as_ref() else {
                    anyhow::bail!("stored input transport is not a stream checkpoint");
                };
                anyhow::ensure!(
                    port == self.progress.port
                        && key == self.progress.transport_key(stream)?
                        && checkpoint.source_position.is_none(),
                    "invalid stored input transport"
                );
                snapshot
                    .transport_sequences
                    .insert(stream.clone(), checkpoint.sequence);
                continue;
            }
            let Some(encoded) = name.strip_prefix(PREFIX) else {
                continue;
            };
            let port: PortId = serde_json::from_str(encoded)?;
            let input = match identity.as_ref() {
                CheckpointKey::Source(id) => SourceProgressKey::Source(id.clone()),
                CheckpointKey::Stream(stream) => SourceProgressKey::Stream(stream.clone()),
                _ => anyhow::bail!("stored input progress has an invalid identity domain"),
            };
            anyhow::ensure!(
                port == self.progress.port,
                "stored input progress belongs to another port"
            );
            anyhow::ensure!(
                key == self.progress.key(&input)?,
                "noncanonical stored input progress key"
            );
            Self::insert(&mut snapshot, input, checkpoint)?;
        }
        drasi_computation_plugin_sdk::progress::encode_snapshot(&snapshot)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("staged input progress poisoned"))?;
        anyhow::ensure!(
            state.as_ref().is_none_or(|snapshot| !snapshot.recovered),
            "progress recovery cannot replace staged progress"
        );
        *state = Some(snapshot);
        Ok(())
    }

    fn insert(
        snapshot: &mut SourceProgressSnapshot,
        key: SourceProgressKey,
        checkpoint: SourceCheckpoint,
    ) -> Result<()> {
        anyhow::ensure!(
            checkpoint
                .source_position
                .as_ref()
                .is_none_or(|position| position.len() <= MAX_SOURCE_POSITION_BYTES),
            "input progress position exceeds its byte limit"
        );
        if let Some(saved) = snapshot.checkpoints.get(&key) {
            anyhow::ensure!(
                checkpoint.sequence >= saved.sequence
                    && (checkpoint.sequence != saved.sequence
                        || checkpoint.source_position == saved.source_position),
                "input progress would regress or reinterpret a committed position"
            );
        }
        snapshot.checkpoints.insert(key, checkpoint);
        drasi_computation_plugin_sdk::progress::encode_snapshot(snapshot)?;
        Ok(())
    }

    pub(super) async fn stage(
        &self,
        resources: &ComputationIndexes,
        key: SourceProgressKey,
        checkpoint: SourceCheckpoint,
        transport: Option<(StreamId, u64)>,
    ) -> Result<()> {
        self.epoch.ensure_current()?;
        let mut snapshot = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("staged input progress poisoned"))?
            .as_ref()
            .cloned()
            .unwrap_or_else(|| self.epoch.publication.reader().snapshot().as_ref().clone());
        anyhow::ensure!(
            snapshot.recovered && snapshot.failure.is_none(),
            "input progress has not recovered or is fenced"
        );
        Self::insert(&mut snapshot, key.clone(), checkpoint.clone())?;
        self.checkpoint_store(resources)?
            .stage_checkpoint(
                &self.key(&key)?,
                checkpoint.sequence,
                checkpoint.source_position.as_ref(),
            )
            .await?;
        *self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("staged input progress poisoned"))? = Some(snapshot);
        if let Some((stream, sequence)) = transport {
            self.stage_transport(resources, stream, sequence).await?;
        }
        Ok(())
    }

    fn snapshot(&self) -> Result<SourceProgressSnapshot> {
        self.epoch.ensure_current()?;
        Ok(self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("staged input progress poisoned"))?
            .as_ref()
            .cloned()
            .unwrap_or_else(|| self.epoch.publication.reader().snapshot().as_ref().clone()))
    }

    pub(super) async fn stage_transport(
        &self,
        resources: &ComputationIndexes,
        stream: StreamId,
        sequence: u64,
    ) -> Result<()> {
        let mut snapshot = self.snapshot()?;
        anyhow::ensure!(
            snapshot.recovered && snapshot.failure.is_none(),
            "input progress is not recovered"
        );
        let sequence = snapshot
            .transport_sequences
            .get(&stream)
            .copied()
            .unwrap_or(0)
            .max(sequence);
        snapshot
            .transport_sequences
            .insert(stream.clone(), sequence);
        drasi_computation_plugin_sdk::progress::encode_snapshot(&snapshot)?;
        self.checkpoint_store(resources)?
            .stage_checkpoint(&self.progress.transport_key(&stream)?, sequence, None)
            .await?;
        *self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("staged input progress poisoned"))? = Some(snapshot);
        Ok(())
    }

    pub(super) async fn stage_bootstrap(
        &self,
        resources: &ComputationIndexes,
        complete: bool,
    ) -> Result<()> {
        let mut snapshot = self.snapshot()?;
        anyhow::ensure!(
            !snapshot.ready && snapshot.failure.is_none(),
            "bootstrap completion can change only before readiness"
        );
        self.checkpoint_store(resources)?
            .stage_checkpoint(
                &CheckpointKey::Metadata(BOOTSTRAP_COMPLETE_CHECKPOINT.into()),
                u64::from(complete),
                None,
            )
            .await?;
        *self
            .bootstrap
            .lock()
            .map_err(|_| anyhow::anyhow!("staged bootstrap progress poisoned"))? = Some(complete);
        if snapshot.recovered {
            snapshot.bootstrap_complete = complete;
            *self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("staged input progress poisoned"))? = Some(snapshot);
        }
        Ok(())
    }

    pub(super) fn ensure_resettable(&self) -> Result<()> {
        let snapshot = self.snapshot()?;
        anyhow::ensure!(
            !snapshot.ready && !snapshot.admitting && snapshot.failure.is_none(),
            "processing reset requires a healthy owner before readiness"
        );
        Ok(())
    }

    pub(super) fn reset(&self, generation: u64) -> Result<()> {
        *self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("staged input progress poisoned"))? =
            Some(SourceProgressSnapshot {
                reset_generation: generation,
                persistent: !self.is_volatile(),
                ..Default::default()
            });
        *self
            .bootstrap
            .lock()
            .map_err(|_| anyhow::anyhow!("staged bootstrap progress poisoned"))? = Some(false);
        Ok(())
    }

    pub(super) fn publish(&self) -> Result<()> {
        self.epoch.ensure_current()?;
        if let Some(snapshot) = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("staged input progress poisoned"))?
            .take()
        {
            self.epoch.publication.publish_committed(
                self.epoch.generation,
                snapshot,
                *self
                    .bootstrap
                    .lock()
                    .map_err(|_| anyhow::anyhow!("staged bootstrap progress poisoned"))?,
            )?;
        }
        if self.resumed.load(Ordering::Acquire) {
            let mut deferred = self
                .progress
                .deferred
                .lock()
                .map_err(|_| anyhow::anyhow!("deferred input progress poisoned"))?;
            anyhow::ensure!(
                deferred
                    .as_ref()
                    .is_some_and(|state| state.generation == self.epoch.generation),
                "deferred progress changed before output publication"
            );
            *deferred = None;
        }
        Ok(())
    }
}
