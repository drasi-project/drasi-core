// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    result::Result,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex,
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::computation::{ComputationIndexes, ComputationTransaction};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};

use super::*;

/// A new subscription's cut. Reopening an existing subscriber keeps its cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubscriptionStart {
    Earliest,
    Latest,
    After(u64),
}

/// One producer stream and its required subscribers. Disconnection does not
/// remove a subscriber's retention obligation; retirement is explicit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QosChannelDefinition {
    pub stream: StreamId,
    pub capacity: NonZeroUsize,
    pub durable: bool,
    pub retention: RetentionPolicy,
    pub subscribers: BTreeMap<String, SubscriptionStart>,
}

impl QosChannelDefinition {
    pub fn validate(&self) -> Result<(), PipeError> {
        if self.subscribers.is_empty() {
            return Err(backend("QoS channel requires at least one subscriber"));
        }
        for subscriber in self.subscribers.keys() {
            data::validate_identifier("QoS subscriber", subscriber)?;
        }
        if self.capacity.get() > tokio::sync::Semaphore::MAX_PERMITS {
            return Err(PipeError::InvalidCapacity);
        }
        Ok(())
    }

    pub fn pipe(&self, resource: ResourceId, subscriber: impl Into<String>) -> QosPipeConfig {
        QosPipeConfig {
            resource,
            subscriber: subscriber.into(),
            definition: self.clone(),
            gap_policy: ReplayGapPolicy::Strict,
        }
    }

    fn capabilities(&self) -> Result<PipeCapabilities, PipeError> {
        let mut capabilities = vec![
            PipeCapability::FifoPerStream,
            PipeCapability::RetainedHistory,
            PipeCapability::ExplicitAcknowledgement,
        ];
        if self.durable {
            capabilities.extend([PipeCapability::DurableAcceptance, PipeCapability::Replay]);
        }
        if self.retention == RetentionPolicy::Backpressure {
            capabilities.push(PipeCapability::Backpressure);
        }
        Ok(PipeCapabilities::try_new(
            capabilities,
            Some(self.capacity),
        )?)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QosPipeConfig {
    pub resource: ResourceId,
    pub subscriber: String,
    pub definition: QosChannelDefinition,
    pub gap_policy: ReplayGapPolicy,
}

impl PipeProvider for QosPipeConfig {
    fn specification(&self) -> Option<DesiredPipe> {
        Some(DesiredPipe::Qos(self.clone()))
    }
    fn multicast_subscription(&self) -> Option<(ResourceId, String)> {
        Some((self.resource.clone(), self.subscriber.clone()))
    }
    fn resource_dependencies(&self) -> BTreeMap<ResourceId, ResourceRole> {
        BTreeMap::from([(self.resource.clone(), ResourceRole::StateStore)])
    }
    fn capabilities(&self) -> Result<PipeCapabilities, PipeError> {
        self.definition.validate()?;
        if !self.definition.subscribers.contains_key(&self.subscriber) {
            return Err(backend("QoS pipe names an undeclared subscriber"));
        }
        if self.gap_policy == ReplayGapPolicy::SkipWithNotification
            && self.definition.retention != RetentionPolicy::PruneOldest
        {
            return Err(backend("gap skipping requires explicitly lossy retention"));
        }
        self.definition.capabilities()
    }
    fn validate_resources(
        &self,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<(), PipeError> {
        self.capabilities()?;
        if let Some(resource) = resources.get(&self.resource) {
            let channel = resource
                .get::<QosChannel>()
                .map_err(|error| backend(error.to_string()))?;
            if channel.definition != self.definition {
                return Err(backend("QoS channel differs from its pipe declaration"));
            }
        }
        Ok(())
    }
    fn create(&self) -> Result<ProvidedPipe, PipeError> {
        Err(backend("QoS pipe requires its declared channel resource"))
    }
    fn create_with_resources(
        &self,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<ProvidedPipe, PipeError> {
        self.validate_resources(resources)?;
        let channel = resources
            .get(&self.resource)
            .ok_or_else(|| backend("QoS channel resource is unavailable"))?
            .get::<QosChannel>()
            .map_err(|error| backend(error.to_string()))?;
        channel.bind(&self.subscriber, self.gap_policy)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    position: u64,
    retired: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    version: u32,
    definition: QosChannelDefinition,
    head: u64,
    producer_sequence: Option<u64>,
    cursors: BTreeMap<String, Cursor>,
}

struct State {
    metadata: Metadata,
    entries: BTreeMap<u64, ChangeEnvelope>,
}

#[derive(Default)]
struct Bindings {
    generation: u64,
    consumers: BTreeMap<String, Arc<Binding>>,
}

struct Binding {
    generation: u64,
    closed: AtomicBool,
    cancelled: AtomicBool,
    pending: AtomicBool,
}

struct Persistent {
    transaction: ComputationTransaction,
    key: String,
    codec: EnvelopeCodec,
}

/// A shared bounded journal: one append, independent subscriber completions.
///
/// Volatile and persistent profiles have identical handoff semantics. Persistent
/// appends commit source metadata with the event. Completion is a separate short
/// transaction, never a transaction held open while business code is running.
pub struct QosChannel {
    definition: QosChannelDefinition,
    state: Mutex<State>,
    bindings: StdMutex<Bindings>,
    persistent: Option<Persistent>,
    closed: AtomicBool,
    changed: Notify,
}

#[derive(Debug, Clone)]
pub struct QosChannelProgress {
    pub accepted: u64,
    pub earliest_available: Option<u64>,
    pub processed: BTreeMap<String, u64>,
    pub retired: Vec<String>,
    pub producer_sequence: Option<u64>,
    pub source_position: Option<Bytes>,
}

fn backend(message: impl Into<String>) -> PipeError {
    PipeError::Backend(anyhow::anyhow!(message.into()))
}

fn initial(definition: &QosChannelDefinition) -> Result<Metadata, PipeError> {
    let mut cursors = BTreeMap::new();
    for (id, start) in &definition.subscribers {
        if matches!(start, SubscriptionStart::After(position) if *position != 0) {
            return Err(backend("new QoS journal cannot start beyond its head"));
        }
        cursors.insert(
            id.clone(),
            Cursor {
                position: 0,
                retired: false,
            },
        );
    }
    Ok(Metadata {
        version: 1,
        definition: definition.clone(),
        head: 0,
        producer_sequence: None,
        cursors,
    })
}

impl QosChannel {
    pub fn volatile(definition: QosChannelDefinition) -> Result<Arc<Self>, PipeError> {
        definition.validate()?;
        if definition.durable {
            return Err(backend("durable QoS requires persistent storage"));
        }
        let metadata = initial(&definition)?;
        Ok(Arc::new(Self {
            definition,
            state: Mutex::new(State {
                metadata,
                entries: BTreeMap::new(),
            }),
            bindings: StdMutex::new(Bindings::default()),
            persistent: None,
            closed: AtomicBool::new(false),
            changed: Notify::new(),
        }))
    }

    pub async fn persistent(
        definition: QosChannelDefinition,
        indexes: ComputationIndexes,
        codec: EnvelopeCodec,
        key: impl Into<String>,
    ) -> Result<Arc<Self>, PipeError> {
        definition.validate()?;
        let key = key.into();
        data::validate_identifier("QoS journal", &key)?;
        if !definition.durable
            || !indexes
                .checkpoint_store()
                .is_some_and(|store| store.is_persistent())
        {
            return Err(backend(
                "persistent QoS requires durable checkpoints and a durable declaration",
            ));
        }
        let transaction =
            ComputationTransaction::try_new(indexes).map_err(|error| backend(error.to_string()))?;
        let resources = transaction.resources();
        let checkpoint = resources.checkpoint_store().expect("validated transaction");
        let saved = checkpoint
            .read_checkpoint(&key)
            .await
            .map_err(|error| backend(error.to_string()))?;
        let records = resources
            .outbox_writer()
            .expect("validated transaction")
            .read_from(&key, 0)
            .await
            .map_err(|error| backend(error.to_string()))?;
        let mut metadata = match saved {
            Some(saved) => {
                let bytes = saved
                    .source_position
                    .ok_or_else(|| backend("QoS metadata is missing"))?;
                let metadata: Metadata =
                    serde_json::from_slice(&bytes).map_err(|error| backend(error.to_string()))?;
                if metadata.version != 1
                    || metadata.definition.stream != definition.stream
                    || !metadata.definition.durable
                    || metadata.head != saved.sequence
                    || metadata
                        .definition
                        .subscribers
                        .keys()
                        .any(|id| !metadata.cursors.contains_key(id))
                    || metadata.cursors.iter().any(|(id, cursor)| {
                        !cursor.retired && !metadata.definition.subscribers.contains_key(id)
                    })
                    || metadata
                        .cursors
                        .values()
                        .any(|cursor| cursor.position > metadata.head)
                {
                    return Err(backend(
                        "QoS journal configuration or progress is inconsistent",
                    ));
                }
                metadata
            }
            None => {
                if !records.is_empty() {
                    return Err(backend("QoS records have no committed metadata"));
                }
                let metadata = initial(&definition)?;
                let bytes = Bytes::from(
                    serde_json::to_vec(&metadata).map_err(|error| backend(error.to_string()))?,
                );
                transaction
                    .run(async {
                        checkpoint.stage_checkpoint(&key, 0, Some(&bytes)).await?;
                        Ok(())
                    })
                    .await
                    .map_err(|error| backend(error.to_string()))?;
                metadata
            }
        };
        let mut entries = BTreeMap::new();
        let mut previous = None;
        for (position, bytes) in records {
            if position == 0 || previous.is_some_and(|previous| previous + 1 != position) {
                return Err(backend("QoS journal contains a sequence gap"));
            }
            let envelope = codec
                .decode(&bytes)
                .map_err(|error| backend(error.to_string()))?;
            if envelope.system().stream() != &definition.stream {
                return Err(backend("QoS journal contains another producer stream"));
            }
            entries.insert(position, envelope);
            previous = Some(position);
        }
        if entries.len() > metadata.definition.capacity.get()
            || previous != (metadata.head > 0).then_some(metadata.head)
            || entries
                .last_key_value()
                .map(|(_, envelope)| envelope.system().sequence())
                != metadata.producer_sequence
        {
            return Err(backend("QoS journal head and retained data disagree"));
        }
        if metadata.definition.retention == RetentionPolicy::Backpressure {
            if let Some(oldest) = entries.keys().next() {
                if metadata
                    .cursors
                    .values()
                    .any(|cursor| !cursor.retired && cursor.position < oldest - 1)
                {
                    return Err(backend("lossless QoS journal has pruned required history"));
                }
            }
        }
        if metadata.definition != definition {
            let oldest = entries.keys().next().copied().unwrap_or(1);
            for (id, start) in &definition.subscribers {
                if metadata
                    .cursors
                    .get(id)
                    .is_some_and(|cursor| cursor.retired)
                    && !metadata.definition.subscribers.contains_key(id)
                {
                    return Err(backend("a retired QoS subscriber requires a new identity"));
                }
                if !metadata.cursors.contains_key(id) {
                    let position = match start {
                        SubscriptionStart::Earliest => oldest.saturating_sub(1),
                        SubscriptionStart::Latest => metadata.head,
                        SubscriptionStart::After(position) => *position,
                    };
                    if position > metadata.head || position < oldest.saturating_sub(1) {
                        return Err(backend("new QoS subscriber requested unavailable history"));
                    }
                    metadata.cursors.insert(
                        id.clone(),
                        Cursor {
                            position,
                            retired: false,
                        },
                    );
                }
            }
            for (id, cursor) in &mut metadata.cursors {
                if !definition.subscribers.contains_key(id) {
                    cursor.retired = true;
                }
            }
            let floor = oldest.max(
                metadata
                    .head
                    .saturating_sub(definition.capacity.get() as u64)
                    .saturating_add(1),
            );
            if definition.retention == RetentionPolicy::Backpressure
                && metadata
                    .cursors
                    .values()
                    .any(|cursor| !cursor.retired && cursor.position < floor.saturating_sub(1))
            {
                return Err(backend(
                    "QoS reconfiguration would discard required history",
                ));
            }
            metadata.definition = definition.clone();
            let bytes = Bytes::from(
                serde_json::to_vec(&metadata).map_err(|error| PipeError::Backend(error.into()))?,
            );
            transaction
                .run(async {
                    resources
                        .outbox_writer()
                        .expect("validated transaction")
                        .trim_before(&key, floor)
                        .await?;
                    checkpoint
                        .stage_checkpoint(&key, metadata.head, Some(&bytes))
                        .await?;
                    Ok(())
                })
                .await
                .map_err(|error| PipeError::Backend(error.into()))?;
            entries.retain(|position, _| *position >= floor);
        }
        Ok(Arc::new(Self {
            definition,
            state: Mutex::new(State { metadata, entries }),
            bindings: StdMutex::new(Bindings::default()),
            persistent: Some(Persistent {
                transaction,
                key,
                codec,
            }),
            closed: AtomicBool::new(false),
            changed: Notify::new(),
        }))
    }

    pub fn resource(self: &Arc<Self>) -> ResourceHandle {
        ResourceHandle::new(ResourceRole::StateStore, self.clone()).with_cleanup(self.clone())
    }

    fn check(&self) -> Result<(), PipeError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PipeError::Closed);
        }
        if self
            .persistent
            .as_ref()
            .is_some_and(|store| store.transaction.recovery_required())
        {
            return Err(backend(
                "QoS journal requires shutdown and reconstruction after an interrupted transaction",
            ));
        }
        Ok(())
    }

    pub async fn progress(&self) -> Result<QosChannelProgress, PipeError> {
        let state = self.state.lock().await;
        self.check()?;
        Ok(QosChannelProgress {
            accepted: state.metadata.head,
            earliest_available: state.entries.keys().next().copied(),
            processed: state
                .metadata
                .cursors
                .iter()
                .map(|(id, cursor)| (id.clone(), cursor.position))
                .collect(),
            retired: state
                .metadata
                .cursors
                .iter()
                .filter(|(_, cursor)| cursor.retired)
                .map(|(id, _)| id.clone())
                .collect(),
            producer_sequence: state.metadata.producer_sequence,
            source_position: state
                .entries
                .last_key_value()
                .and_then(|(_, envelope)| envelope.system().source_position().cloned()),
        })
    }

    async fn persist(
        &self,
        metadata: &Metadata,
        append: Option<(u64, &ChangeEnvelope, u64)>,
    ) -> Result<(), PipeError> {
        let Some(store) = &self.persistent else {
            return Ok(());
        };
        let bytes =
            Bytes::from(serde_json::to_vec(metadata).map_err(|error| backend(error.to_string()))?);
        let record = append
            .map(|(position, envelope, floor)| {
                store
                    .codec
                    .encode(envelope)
                    .map(|bytes| (position, bytes, floor))
            })
            .transpose()
            .map_err(|error| backend(error.to_string()))?;
        let result = store
            .transaction
            .run(async {
                if let Some((position, bytes, floor)) = &record {
                    store
                        .transaction
                        .resources()
                        .outbox_writer()
                        .expect("validated transaction")
                        .append_and_trim(&store.key, *position, bytes, *floor)
                        .await?;
                }
                store
                    .transaction
                    .resources()
                    .checkpoint_store()
                    .expect("validated transaction")
                    .stage_checkpoint(&store.key, metadata.head, Some(&bytes))
                    .await?;
                Ok(())
            })
            .await;
        result.map_err(|error| {
            if !store.transaction.recovery_required() {
                PipeError::Backend(error.into())
            } else if append.is_some() {
                PipeError::AcceptanceUnknown {
                    source: error.into(),
                }
            } else {
                PipeError::AcknowledgementUnknown {
                    source: error.into(),
                }
            }
        })
    }

    /// Commit one immutable event to every required subscriber's journal.
    /// The producer must serialize its stream and retain an event while retrying.
    pub async fn publish(&self, envelope: &ChangeEnvelope) -> Result<EnqueueReceipt, PipeError> {
        self.publish_bound(envelope, None).await
    }

    async fn publish_bound(
        &self,
        envelope: &ChangeEnvelope,
        endpoint: Option<&Endpoint>,
    ) -> Result<EnqueueReceipt, PipeError> {
        if envelope.system().stream() != &self.definition.stream {
            return Err(backend("QoS channel received an event from another stream"));
        }
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let mut state = self.state.lock().await;
            self.check()?;
            if let Some(endpoint) = endpoint {
                endpoint.check()?;
                if endpoint.binding.closed.load(Ordering::Acquire) {
                    return Err(PipeError::Closed);
                }
            }
            let sequence = envelope.system().sequence();
            if state
                .metadata
                .producer_sequence
                .is_some_and(|saved| sequence <= saved)
            {
                let retained = state
                    .entries
                    .iter()
                    .find(|(_, saved)| saved.system().sequence() == sequence);
                if let Some((position, saved)) = retained {
                    // Full comparison includes source position and branch context.
                    let same = if let Some(store) = &self.persistent {
                        store
                            .codec
                            .encode(saved)
                            .map_err(|error| backend(error.to_string()))?
                            == store
                                .codec
                                .encode(envelope)
                                .map_err(|error| backend(error.to_string()))?
                    } else {
                        Arc::ptr_eq(saved.event(), envelope.event())
                            && saved.context_identity() == envelope.context_identity()
                            && saved
                                .annotations()
                                .entries()
                                .eq(envelope.annotations().entries())
                    };
                    if same {
                        return Ok(
                            EnqueueReceipt::new(envelope.id().clone()).with_position(*position)
                        );
                    }
                }
                return Err(backend("QoS producer sequence regressed, changed, or is outside retained retry history"));
            }
            let full = state.entries.len() == self.definition.capacity.get();
            let floor = state.entries.keys().next().copied().unwrap_or(1);
            let blocked = full
                && self.definition.retention == RetentionPolicy::Backpressure
                && state
                    .metadata
                    .cursors
                    .values()
                    .any(|cursor| !cursor.retired && cursor.position < floor);
            if blocked {
                drop(state);
                notified.await;
                continue;
            }
            let position = state
                .metadata
                .head
                .checked_add(1)
                .ok_or_else(|| backend("QoS journal sequence exhausted"))?;
            let retain_from = if full { floor + 1 } else { floor };
            let mut metadata = state.metadata.clone();
            metadata.head = position;
            metadata.producer_sequence = Some(sequence);
            let _wake = WakeOnDrop(&self.changed);
            self.persist(&metadata, Some((position, envelope, retain_from)))
                .await?;
            state.entries.retain(|position, _| *position >= retain_from);
            state.entries.insert(position, envelope.clone());
            state.metadata = metadata;
            if endpoint.is_some_and(|endpoint| endpoint.check().is_err()) {
                return Err(PipeError::AcceptanceUnknown {
                    source: anyhow::anyhow!("QoS endpoint revoked during acceptance"),
                });
            }
            return Ok(EnqueueReceipt::new(envelope.id().clone()).with_position(position));
        }
    }

    /// Abandon a disconnected subscriber's remaining obligation explicitly.
    /// A retired identity cannot be reused as a new subscription.
    pub async fn retire(&self, subscriber: &str) -> Result<(), PipeError> {
        let mut state = self.state.lock().await;
        self.check()?;
        {
            let bindings = self
                .bindings
                .lock()
                .map_err(|_| backend("QoS bindings poisoned"))?;
            if bindings
                .consumers
                .get(subscriber)
                .is_some_and(|binding| !binding.cancelled.load(Ordering::Acquire))
            {
                return Err(backend("disconnect a QoS subscriber before retiring it"));
            }
        }
        let mut metadata = state.metadata.clone();
        let cursor = metadata
            .cursors
            .get_mut(subscriber)
            .ok_or_else(|| backend("unknown QoS subscriber"))?;
        cursor.retired = true;
        let _wake = WakeOnDrop(&self.changed);
        self.persist(&metadata, None).await?;
        state.metadata = metadata;
        Ok(())
    }

    fn bind(
        self: &Arc<Self>,
        subscriber: &str,
        gap_policy: ReplayGapPolicy,
    ) -> Result<ProvidedPipe, PipeError> {
        self.check()?;
        if !self.definition.subscribers.contains_key(subscriber) {
            return Err(backend("unknown QoS subscriber"));
        }
        let mut bindings = self
            .bindings
            .lock()
            .map_err(|_| backend("QoS bindings poisoned"))?;
        if bindings
            .consumers
            .get(subscriber)
            .is_some_and(|binding| !binding.cancelled.load(Ordering::Acquire))
        {
            return Err(backend("QoS subscriber already has a live owner"));
        }
        bindings.generation = bindings
            .generation
            .checked_add(1)
            .ok_or_else(|| backend("QoS binding generation exhausted"))?;
        let binding = Arc::new(Binding {
            generation: bindings.generation,
            closed: AtomicBool::new(false),
            cancelled: AtomicBool::new(false),
            pending: AtomicBool::new(false),
        });
        bindings
            .consumers
            .insert(subscriber.to_owned(), binding.clone());
        let endpoint = Arc::new(Endpoint {
            channel: self.clone(),
            subscriber: subscriber.to_owned(),
            binding,
        });
        Ok(ProvidedPipe {
            control: endpoint.clone(),
            pipe: Box::new(QosPipe {
                capabilities: self.definition.capabilities()?,
                sender: Arc::new(Sender(endpoint.clone())),
                receiver: Some(Receiver {
                    endpoint,
                    gap_policy,
                }),
            }),
        })
    }
}

struct WakeOnDrop<'a>(&'a Notify);
impl Drop for WakeOnDrop<'_> {
    fn drop(&mut self) {
        self.0.notify_waiters();
    }
}

#[async_trait]
impl ResourceCleanup for QosChannel {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::Release);
        self.changed.notify_waiters();
        let _state = self.state.lock().await;
        if let Some(store) = &self.persistent {
            store.transaction.shutdown().await?;
        }
        Ok(())
    }
}

struct Endpoint {
    channel: Arc<QosChannel>,
    subscriber: String,
    binding: Arc<Binding>,
}
impl Endpoint {
    fn check(&self) -> Result<(), PipeError> {
        self.channel.check()?;
        let bindings = self
            .channel
            .bindings
            .lock()
            .map_err(|_| backend("QoS bindings poisoned"))?;
        if self.binding.cancelled.load(Ordering::Acquire)
            || !bindings
                .consumers
                .get(&self.subscriber)
                .is_some_and(|current| current.generation == self.binding.generation)
        {
            return Err(PipeError::Closed);
        }
        Ok(())
    }
    async fn complete(&self, position: u64, skip: bool) -> Result<(), PipeError> {
        let mut state = self.channel.state.lock().await;
        self.check()?;
        let mut metadata = state.metadata.clone();
        let cursor = metadata
            .cursors
            .get_mut(&self.subscriber)
            .expect("declared subscriber");
        if cursor.retired
            || position > metadata.head
            || position < cursor.position
            || !skip && cursor.position.checked_add(1) != Some(position)
        {
            return Err(backend("QoS acknowledgement would skip unprocessed work"));
        }
        cursor.position = position;
        let _wake = WakeOnDrop(&self.channel.changed);
        self.channel.persist(&metadata, None).await?;
        state.metadata = metadata;
        self.check()
            .map_err(|error| PipeError::AcknowledgementUnknown {
                source: error.into(),
            })?;
        Ok(())
    }
}

#[async_trait]
impl PipeControl for Endpoint {
    fn close(&self) {
        self.binding.closed.store(true, Ordering::Release);
        self.channel.changed.notify_waiters();
    }
    fn cancel(&self) {
        self.binding.cancelled.store(true, Ordering::Release);
        self.channel.changed.notify_waiters();
    }
    async fn is_idle(&self) -> Result<bool, PipeError> {
        let state = self.channel.state.lock().await;
        self.check()?;
        Ok(!self.binding.pending.load(Ordering::Acquire)
            && state.metadata.cursors[&self.subscriber].position == state.metadata.head)
    }
}

struct Sender(Arc<Endpoint>);
impl Drop for Sender {
    fn drop(&mut self) {
        self.0.close();
    }
}
#[async_trait]
impl EnvelopeSender for Sender {
    async fn send(&self, envelope: ChangeEnvelope) -> Result<EnqueueReceipt, SendFailure> {
        let result = self.0.channel.publish_bound(&envelope, Some(&self.0)).await;
        result.map_err(|error| SendFailure { envelope, error })
    }
}

struct Ack {
    endpoint: Arc<Endpoint>,
    position: u64,
}
#[async_trait]
impl Acknowledgement for Ack {
    async fn complete(self: Box<Self>, outcome: HandlingOutcome) -> Result<(), PipeError> {
        match outcome {
            HandlingOutcome::Handled => self.endpoint.complete(self.position, false).await,
            HandlingOutcome::Failed { reason } => {
                log::warn!(
                    "QoS delivery {} remains unacknowledged: {reason}",
                    self.position
                );
                Ok(())
            }
        }
    }
}
impl Drop for Ack {
    fn drop(&mut self) {
        self.endpoint
            .binding
            .pending
            .store(false, Ordering::Release);
        self.endpoint.channel.changed.notify_waiters();
    }
}

struct Receiver {
    endpoint: Arc<Endpoint>,
    gap_policy: ReplayGapPolicy,
}
#[async_trait]
impl EnvelopeReceiver for Receiver {
    async fn receive(&mut self) -> Result<Option<Delivery>, PipeError> {
        loop {
            let notified = self.endpoint.channel.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let state = self.endpoint.channel.state.lock().await;
            self.endpoint.check()?;
            let cursor = &state.metadata.cursors[&self.endpoint.subscriber];
            if cursor.retired {
                return Err(backend("QoS subscriber is retired"));
            }
            if self.endpoint.binding.pending.load(Ordering::Acquire) {
                drop(state);
                notified.await;
                continue;
            }
            if let Some(oldest) = state.entries.keys().next().copied() {
                if cursor.position < oldest - 1 {
                    let requested = cursor.position;
                    drop(state);
                    if self.gap_policy == ReplayGapPolicy::Strict {
                        return Err(PipeError::PositionUnavailable { requested, oldest });
                    }
                    log::warn!(
                        "QoS subscriber {} explicitly skips positions {}..{}",
                        self.endpoint.subscriber,
                        requested + 1,
                        oldest
                    );
                    self.endpoint.complete(oldest - 1, true).await?;
                    continue;
                }
            }
            if let Some((position, envelope)) = state
                .entries
                .range((
                    std::ops::Bound::Excluded(cursor.position),
                    std::ops::Bound::Unbounded,
                ))
                .next()
            {
                let delivery = Delivery::new(
                    envelope.clone(),
                    Some(Box::new(Ack {
                        endpoint: self.endpoint.clone(),
                        position: *position,
                    })),
                );
                self.endpoint.binding.pending.store(true, Ordering::Release);
                return Ok(Some(delivery));
            }
            if self.endpoint.binding.closed.load(Ordering::Acquire) {
                return Ok(None);
            }
            drop(state);
            notified.await;
        }
    }
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.endpoint.cancel();
    }
}

struct QosPipe {
    capabilities: PipeCapabilities,
    sender: Arc<Sender>,
    receiver: Option<Receiver>,
}
impl Pipe for QosPipe {
    fn capabilities(&self) -> &PipeCapabilities {
        &self.capabilities
    }
    fn sender(&self) -> Arc<dyn EnvelopeSender> {
        self.sender.clone()
    }
    fn take_receiver(&mut self) -> Result<Box<dyn EnvelopeReceiver>, PipeError> {
        self.receiver
            .take()
            .map(|receiver| Box::new(receiver) as Box<dyn EnvelopeReceiver>)
            .ok_or(PipeError::ReceiverTaken)
    }
}
