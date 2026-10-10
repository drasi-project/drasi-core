// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    result::Result,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as StdMutex, OnceLock,
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::computation::{ComputationIndexes, ComputationTransaction};
use drasi_core::interface::OutboxPageLimits;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};

use super::journal_budget::{JournalAppend, JournalBudget};
use super::*;

mod admission;
mod ingress;
mod recovery;
mod replay;
mod retirement;
mod shared;
pub use admission::{
    AdmissionOptions, AdmissionReceipt, AdmissionRejection, ProducerSession, ProducerStatus,
};
pub use ingress::SourceAdmission;
pub use recovery::QosRecoveryOptions;
pub use replay::{ReplayOptions, ReplayRejection};
pub(crate) use retirement::QosRetirement;
pub(crate) use shared::QosReservation;

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
            shared_storage: None,
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
    /// Keep the graph-owned provider live independently of the producer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_storage: Option<ResourceId>,
}

impl QosPipeConfig {
    pub fn with_shared_storage(mut self, resource: ResourceId) -> Self {
        self.shared_storage = Some(resource);
        self
    }
}

impl PipeProvider for QosPipeConfig {
    fn specification(&self) -> Option<DesiredPipe> {
        Some(DesiredPipe::Qos(self.clone()))
    }
    fn multicast_subscription(&self) -> Option<(ResourceId, String)> {
        Some((self.resource.clone(), self.subscriber.clone()))
    }
    fn resource_dependencies(&self) -> BTreeMap<ResourceId, ResourceRole> {
        let mut resources = BTreeMap::from([(self.resource.clone(), ResourceRole::StateStore)]);
        if let Some(storage) = &self.shared_storage {
            resources.insert(storage.clone(), ResourceRole::IndexBackend);
        }
        resources
    }
    fn capabilities(&self) -> Result<PipeCapabilities, PipeError> {
        self.definition.validate()?;
        if self.shared_storage.as_ref() == Some(&self.resource) {
            return Err(backend(
                "shared storage and QoS journal need distinct resource IDs",
            ));
        }
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
            if channel.is_shared() != self.shared_storage.is_some() {
                return Err(backend(
                    "shared QoS requires its graph-owned storage dependency",
                ));
            }
            if let Some(storage) = &self.shared_storage {
                let provider = resources
                    .get(storage)
                    .ok_or_else(|| backend("shared QoS storage resource is unavailable"))?
                    .get::<super::QueryIndexProviderResource>()
                    .map_err(|error| PipeError::Backend(error.into()))?;
                let store = channel.storage()?.ok_or(PipeError::Closed)?;
                if !provider
                    .0
                    .transaction_group()
                    .is_some_and(|group| group.contains(&store.transaction))
                {
                    return Err(backend("QoS journal belongs to a different storage group"));
                }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    admission: Option<admission::AdmissionState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    replay: Option<replay::ReplayState>,
}

struct State {
    metadata: Metadata,
    entries: BTreeMap<u64, ChangeEnvelope>,
    oldest: Option<u64>,
    retained: usize,
    source_position: Option<Bytes>,
    page_limits: Option<drasi_core::interface::OutboxPageLimits>,
    budget: Option<JournalBudget>,
}

impl State {
    fn prepare_append(
        &self,
        definition: &QosChannelDefinition,
        envelope: &ChangeEnvelope,
    ) -> Result<(u64, Option<JournalAppend>), PipeError> {
        let remove = self.retained.saturating_sub(definition.capacity.get() - 1);
        let budget = self
            .budget
            .as_ref()
            .map(|budget| budget.prepare(envelope, remove))
            .transpose()?;
        let remove = budget.as_ref().map_or(remove, |append| append.remove);
        if remove > 0 && definition.retention == RetentionPolicy::Backpressure {
            let last_removed = self
                .oldest
                .and_then(|oldest| oldest.checked_add(remove as u64 - 1))
                .ok_or_else(|| backend("invalid QoS capacity accounting"))?;
            if self
                .metadata
                .cursors
                .values()
                .any(|cursor| !cursor.retired && cursor.position < last_removed)
            {
                return Err(PipeError::CapacityExhausted);
            }
        }
        let position = self
            .metadata
            .head
            .checked_add(1)
            .ok_or_else(|| backend("QoS journal sequence exhausted"))?;
        Ok((
            self.oldest
                .and_then(|oldest| oldest.checked_add(remove as u64))
                .unwrap_or(position),
            budget,
        ))
    }

    fn append_committed(
        &mut self,
        position: u64,
        envelope: ChangeEnvelope,
        retain_from: u64,
        budget: Option<JournalAppend>,
    ) {
        self.source_position = envelope.system().source_position().cloned();
        if self.page_limits.is_some() {
            if self
                .entries
                .first_key_value()
                .is_some_and(|(first, _)| *first < retain_from)
            {
                self.entries.clear();
            }
        } else {
            self.entries.retain(|position, _| *position >= retain_from);
            self.entries.insert(position, envelope);
        }
        self.oldest = Some(retain_from);
        self.retained = (position - retain_from + 1) as usize;
        if let Some(append) = budget {
            self.budget
                .as_mut()
                .expect("configured journal budget")
                .apply(append);
        }
    }
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
    transaction: Arc<ComputationTransaction>,
    key: String,
    codec: EnvelopeCodec,
    shared: bool,
}

impl Persistent {
    async fn read_records(
        &self,
        after: u64,
        limits: Option<drasi_core::interface::OutboxPageLimits>,
    ) -> Result<Vec<(u64, Vec<u8>)>, drasi_core::interface::IndexError> {
        let outbox = self
            .transaction
            .resources()
            .outbox_writer()
            .expect("validated transaction");
        match limits {
            Some(limits) => {
                let records = outbox.read_page(&self.key, after, limits).await?;
                limits.validate_page(&records)?;
                Ok(records)
            }
            None => outbox.read_from(&self.key, after).await,
        }
    }
}

/// A shared bounded journal: one append, independent subscriber completions.
///
/// Volatile and persistent profiles have identical handoff semantics. Persistent
/// appends commit source metadata with the event. Completion is a separate short
/// transaction, never a transaction held open while business code is running.
pub struct QosChannel {
    definition: QosChannelDefinition,
    max_bytes: Option<NonZeroUsize>,
    durability: drasi_core::interface::StorageDurability,
    state: Mutex<State>,
    bindings: StdMutex<Bindings>,
    persistent: StdMutex<Option<Arc<Persistent>>>,
    closed: AtomicBool,
    admission_bound: AtomicBool,
    replay_identity: OnceLock<uuid::Uuid>,
    configuring_replay: AtomicBool,
    changed: Notify,
    shared: Option<shared::SharedChannel>,
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

/// Resource-level options for a non-shared persistent journal. The budget counts
/// each retained envelope once, independently of its subscriber count.
#[derive(Debug, Clone, Default)]
pub struct QosJournalOptions {
    pub recovery: Option<QosRecoveryOptions>,
    pub max_bytes: Option<NonZeroUsize>,
    pub page_limits: Option<drasi_core::interface::OutboxPageLimits>,
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
        admission: None,
        replay: None,
    })
}

impl QosChannel {
    pub fn volatile(definition: QosChannelDefinition) -> Result<Arc<Self>, PipeError> {
        Self::volatile_options(definition, None)
    }

    pub fn volatile_with_byte_budget(
        definition: QosChannelDefinition,
        max_bytes: NonZeroUsize,
    ) -> Result<Arc<Self>, PipeError> {
        Self::volatile_options(definition, Some(max_bytes))
    }

    fn volatile_options(
        definition: QosChannelDefinition,
        max_bytes: Option<NonZeroUsize>,
    ) -> Result<Arc<Self>, PipeError> {
        definition.validate()?;
        if definition.durable {
            return Err(backend("durable QoS requires persistent storage"));
        }
        let metadata = initial(&definition)?;
        Ok(Arc::new(Self {
            definition,
            max_bytes,
            durability: drasi_core::interface::StorageDurability::VOLATILE,
            state: Mutex::new(State {
                metadata,
                entries: BTreeMap::new(),
                oldest: None,
                retained: 0,
                source_position: None,
                page_limits: None,
                budget: max_bytes.map(JournalBudget::new),
            }),
            bindings: StdMutex::new(Bindings::default()),
            persistent: StdMutex::new(None),
            closed: AtomicBool::new(false),
            admission_bound: AtomicBool::new(false),
            replay_identity: OnceLock::new(),
            configuring_replay: AtomicBool::new(false),
            changed: Notify::new(),
            shared: None,
        }))
    }

    pub async fn persistent(
        definition: QosChannelDefinition,
        indexes: ComputationIndexes,
        codec: EnvelopeCodec,
        key: impl Into<String>,
    ) -> Result<Arc<Self>, PipeError> {
        Self::persistent_with_options(
            definition,
            indexes,
            codec,
            key,
            QosJournalOptions::default(),
        )
        .await
    }

    /// Require the configured recovery mode on both initial creation and reopen.
    /// A missing service setting is Disabled, not permission to load another mode.
    pub async fn persistent_with_recovery(
        definition: QosChannelDefinition,
        indexes: ComputationIndexes,
        codec: EnvelopeCodec,
        key: impl Into<String>,
        recovery: QosRecoveryOptions,
    ) -> Result<Arc<Self>, PipeError> {
        Self::persistent_with_options(
            definition,
            indexes,
            codec,
            key,
            QosJournalOptions {
                recovery: Some(recovery),
                max_bytes: None,
                page_limits: None,
            },
        )
        .await
    }

    pub async fn persistent_with_options(
        definition: QosChannelDefinition,
        indexes: ComputationIndexes,
        codec: EnvelopeCodec,
        key: impl Into<String>,
        options: QosJournalOptions,
    ) -> Result<Arc<Self>, PipeError> {
        definition.validate()?;
        if let Some(recovery) = &options.recovery {
            recovery.validate_storage(&definition, indexes.durability())?;
        }
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
        Self::open_persistent(definition, transaction, codec, key, None, options).await
    }

    /// A journal member of an explicitly owned storage group. Its replay record,
    /// events and cursors share the producer's transaction, not its lifetime.
    pub async fn shared(
        definition: QosChannelDefinition,
        group: &drasi_core::computation::ComputationTransactionGroup,
        journal: &str,
        codec: EnvelopeCodec,
        replay: ReplayOptions,
    ) -> Result<Arc<Self>, PipeError> {
        Self::open_shared(definition, group, journal, codec, replay, None).await
    }

    /// Bounds the decoded journal cache. Startup still validates every record
    /// under the shared storage gate, and refills exclude concurrent group writes.
    pub async fn shared_with_page_limits(
        definition: QosChannelDefinition,
        group: &drasi_core::computation::ComputationTransactionGroup,
        journal: &str,
        codec: EnvelopeCodec,
        replay: ReplayOptions,
        page_limits: OutboxPageLimits,
    ) -> Result<Arc<Self>, PipeError> {
        Self::open_shared(definition, group, journal, codec, replay, Some(page_limits)).await
    }

    async fn open_shared(
        definition: QosChannelDefinition,
        group: &drasi_core::computation::ComputationTransactionGroup,
        journal: &str,
        codec: EnvelopeCodec,
        replay: ReplayOptions,
        page_limits: Option<OutboxPageLimits>,
    ) -> Result<Arc<Self>, PipeError> {
        definition.validate()?;
        replay.validate(&definition)?;
        group
            .durability()
            .require(replay.failure_scope)
            .map_err(|error| PipeError::Backend(error.into()))?;
        let transaction = group
            .journal_transaction(journal)
            .map_err(|error| PipeError::Backend(error.into()))?;
        Self::open_persistent(
            definition,
            transaction,
            codec,
            "events".into(),
            Some(replay),
            QosJournalOptions {
                page_limits,
                ..QosJournalOptions::default()
            },
        )
        .await
    }

    async fn open_persistent(
        definition: QosChannelDefinition,
        transaction: ComputationTransaction,
        codec: EnvelopeCodec,
        key: String,
        shared_replay: Option<ReplayOptions>,
        options: QosJournalOptions,
    ) -> Result<Arc<Self>, PipeError> {
        if shared_replay.is_some() && (options.max_bytes.is_some() || options.recovery.is_some()) {
            return Err(backend(
                "shared transactional QoS requires its own replay policy and does not support byte budgets",
            ));
        }
        let store = Arc::new(Persistent {
            transaction: Arc::new(transaction),
            key,
            codec,
            shared: shared_replay.is_some(),
        });
        let load = Self::load_persistent(definition, store.clone(), shared_replay, options);
        if store.shared {
            store
                .transaction
                .run(async {
                    load.await
                        .map_err(|error| drasi_core::interface::IndexError::other(error).into())
                })
                .await
                .map_err(|error| PipeError::Backend(error.into()))
        } else {
            load.await
        }
    }

    async fn load_persistent(
        definition: QosChannelDefinition,
        store: Arc<Persistent>,
        shared_replay: Option<ReplayOptions>,
        options: QosJournalOptions,
    ) -> Result<Arc<Self>, PipeError> {
        let QosJournalOptions {
            recovery,
            max_bytes,
            page_limits,
        } = options;
        let transaction = &store.transaction;
        let key = &store.key;
        let codec = &store.codec;
        let resources = transaction.resources();
        if !resources
            .checkpoint_store()
            .is_some_and(|checkpoint| checkpoint.is_persistent())
        {
            return Err(backend("persistent QoS requires persistent checkpoints"));
        }
        let read = async {
            let saved = store.read_metadata().await?;
            let records = store.read_records(0, page_limits).await?;
            Ok::<_, drasi_core::interface::IndexError>((saved, records))
        };
        let (saved, records) = if let Some(options) = &shared_replay {
            async {
                let (mut saved, records) = read.await?;
                if saved.is_none() {
                    if !records.is_empty() {
                        return Err(drasi_core::interface::IndexError::CorruptedData);
                    }
                    let mut metadata =
                        initial(&definition).map_err(drasi_core::interface::IndexError::other)?;
                    metadata.replay = Some(replay::ReplayState::new(options.clone()));
                    let bytes = Bytes::from(
                        serde_json::to_vec(&metadata)
                            .map_err(drasi_core::interface::IndexError::other)?,
                    );
                    store.stage_metadata(0, &bytes).await?;
                    saved = Some(drasi_core::interface::SourceCheckpoint::new(0, Some(bytes)));
                }
                Ok::<_, drasi_core::interface::IndexError>((saved, records))
            }
            .await
            .map_err(|error| PipeError::Backend(error.into()))?
        } else {
            read.await
                .map_err(|error| PipeError::Backend(error.into()))?
        };
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
                let mut metadata = initial(&definition)?;
                if let Some(recovery) = &recovery {
                    recovery.prepare(&mut metadata)?;
                }
                let bytes = Bytes::from(
                    serde_json::to_vec(&metadata).map_err(|error| backend(error.to_string()))?,
                );
                transaction
                    .run(async {
                        store.stage_metadata(0, &bytes).await?;
                        Ok(())
                    })
                    .await
                    .map_err(|error| backend(error.to_string()))?;
                metadata
            }
        };
        let recovery_changed = recovery
            .as_ref()
            .map(|recovery| recovery.prepare(&mut metadata))
            .transpose()?
            .unwrap_or(false);
        if let Some(options) = &shared_replay {
            if metadata.admission.is_some()
                || !metadata.replay.as_ref().is_some_and(|replay| {
                    replay.options() == *options && replay.identity().is_some()
                })
            {
                return Err(backend(
                    "shared QoS requires its original replay settings and journal identity",
                ));
            }
        }
        let mut entries = BTreeMap::new();
        let mut budget = max_bytes.map(JournalBudget::new);
        let mut previous: Option<u64> = None;
        let mut producer_sequence = None;
        let mut source_position = None;
        let mut retained = 0usize;
        let mut oldest = None;
        let mut replay_scan = replay::ReplayScan::default();
        if let Some(admission) = &metadata.admission {
            admission.validate(&definition, metadata.head)?;
            resources
                .durability()
                .require(admission.failure_scope())
                .map_err(|error| PipeError::Backend(error.into()))?;
        }
        if let Some(replay) = &metadata.replay {
            if metadata.admission.is_some() {
                return Err(backend(
                    "client admission and output replay tracking are mutually exclusive",
                ));
            }
            replay.validate(&definition, metadata.head)?;
            resources
                .durability()
                .require(replay.failure_scope())
                .map_err(|error| PipeError::Backend(error.into()))?;
        }
        let mut records = records;
        loop {
            if records.is_empty() {
                break;
            }
            let mut page = BTreeMap::new();
            for (position, bytes) in records {
                if position == 0
                    || previous.is_some_and(|previous| previous.checked_add(1) != Some(position))
                {
                    return Err(backend("QoS journal contains a sequence gap"));
                }
                let envelope = codec
                    .decode(&bytes)
                    .map_err(|error| backend(error.to_string()))?;
                if envelope.system().stream() != &definition.stream {
                    return Err(backend("QoS journal contains another producer stream"));
                }
                if let Some(admission) = &metadata.admission {
                    admission.validate_entry(position, &envelope)?;
                }
                if let Some(replay) = &metadata.replay {
                    replay.validate_entry(
                        metadata.head,
                        position,
                        &envelope,
                        codec,
                        &mut replay_scan,
                    )?;
                }
                if let Some(budget) = &mut budget {
                    budget.restore(&envelope)?;
                }
                oldest.get_or_insert(position);
                retained = retained
                    .checked_add(1)
                    .ok_or_else(|| backend("QoS retained count overflow"))?;
                producer_sequence = Some(envelope.system().sequence());
                source_position = envelope.system().source_position().cloned();
                page.insert(position, envelope);
                previous = Some(position);
            }
            if entries.is_empty() {
                entries = page;
            }
            if page_limits.is_none() {
                break;
            }
            records = store
                .read_records(previous.unwrap_or(0), page_limits)
                .await
                .map_err(|error| PipeError::Backend(error.into()))?;
        }
        if retained > metadata.definition.capacity.get()
            || previous != (metadata.head > 0).then_some(metadata.head)
            || producer_sequence != metadata.producer_sequence
        {
            return Err(backend("QoS journal head and retained data disagree"));
        }
        if metadata.definition.retention == RetentionPolicy::Backpressure {
            if let Some(oldest) = oldest {
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
            if metadata.admission.is_some() || metadata.replay.is_some() {
                return Err(backend(
                    "receipt-tracked channel settings cannot change without an explicit migration",
                ));
            }
            let first = oldest.unwrap_or(1);
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
                        SubscriptionStart::Earliest => first.saturating_sub(1),
                        SubscriptionStart::Latest => metadata.head,
                        SubscriptionStart::After(position) => *position,
                    };
                    if position > metadata.head || position < first.saturating_sub(1) {
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
            let floor = first.max(
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
                        .trim_before(key, floor)
                        .await?;
                    store.stage_metadata(metadata.head, &bytes).await?;
                    Ok(())
                })
                .await
                .map_err(|error| PipeError::Backend(error.into()))?;
            entries.retain(|position, _| *position >= floor);
            oldest = oldest.map(|oldest| oldest.max(floor));
            retained = oldest.map_or(0, |oldest| (metadata.head - oldest + 1) as usize);
            if let Some(budget) = &mut budget {
                budget.retain_last(retained);
            }
        }
        if recovery_changed {
            let bytes = Bytes::from(
                serde_json::to_vec(&metadata).map_err(|error| PipeError::Backend(error.into()))?,
            );
            transaction
                .run(async {
                    store.stage_metadata(metadata.head, &bytes).await?;
                    Ok(())
                })
                .await
                .map_err(|error| PipeError::Backend(error.into()))?;
        }
        let replay_options = metadata.replay.as_ref().map(|replay| replay.options());
        let replay_identity = metadata
            .replay
            .as_ref()
            .and_then(|replay| replay.identity())
            .map_or_else(OnceLock::new, OnceLock::from);
        let durability = resources.durability();
        let channel = Arc::new_cyclic(|owner| Self {
            definition,
            max_bytes,
            durability,
            state: Mutex::new(State {
                metadata,
                entries,
                oldest,
                retained,
                source_position,
                page_limits,
                budget,
            }),
            bindings: StdMutex::new(Bindings::default()),
            persistent: StdMutex::new(Some(store)),
            closed: AtomicBool::new(false),
            admission_bound: AtomicBool::new(false),
            replay_identity,
            configuring_replay: AtomicBool::new(false),
            changed: Notify::new(),
            shared: shared_replay
                .as_ref()
                .map(|_| shared::SharedChannel::new(owner.clone())),
        });
        if let Some(options) = replay_options {
            channel.enable_replay(options).await?;
        }
        Ok(channel)
    }

    pub fn resource(self: &Arc<Self>) -> ResourceHandle {
        ResourceHandle::new(ResourceRole::StateStore, self.clone()).with_cleanup(self.clone())
    }

    pub fn durability(&self) -> drasi_core::interface::StorageDurability {
        self.durability
    }

    pub fn definition(&self) -> &QosChannelDefinition {
        &self.definition
    }

    pub fn max_bytes(&self) -> Option<NonZeroUsize> {
        self.max_bytes
    }

    pub(super) fn matches_definition(&self, pipe: &QosPipeConfig) -> bool {
        self.definition == pipe.definition
            && self.definition.subscribers.contains_key(&pipe.subscriber)
    }

    fn storage(&self) -> Result<Option<Arc<Persistent>>, PipeError> {
        Ok(self
            .persistent
            .lock()
            .map_err(|_| backend("QoS storage poisoned"))?
            .clone())
    }

    fn check(&self) -> Result<(), PipeError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(PipeError::Closed);
        }
        if self
            .storage()?
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
            earliest_available: state.oldest,
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
            source_position: state.source_position.clone(),
        })
    }

    async fn cache_after(&self, state: &mut State, after: u64) -> Result<(), PipeError> {
        let Some(limits) = state.page_limits else {
            return Ok(());
        };
        let Some(start) = after
            .checked_add(1)
            .filter(|start| *start <= state.metadata.head)
        else {
            return Ok(());
        };
        if state.entries.contains_key(&start) {
            return Ok(());
        }
        let store = self.storage()?.ok_or(PipeError::Closed)?;
        let records = store
            .read_records(after, Some(limits))
            .await
            .map_err(|error| PipeError::Backend(error.into()))?;
        self.check()?;
        let mut entries = BTreeMap::new();
        let mut expected = Some(start);
        let mut replay_scan = replay::ReplayScan::default();
        for (position, bytes) in records {
            if Some(position) != expected || position > state.metadata.head {
                return Err(backend("QoS page positions are inconsistent"));
            }
            let envelope = store
                .codec
                .decode(&bytes)
                .map_err(|error| PipeError::Backend(error.into()))?;
            if envelope.system().stream() != &self.definition.stream {
                return Err(backend("QoS page contains another producer stream"));
            }
            if let Some(admission) = &state.metadata.admission {
                admission.validate_entry(position, &envelope)?;
            }
            if let Some(replay) = &state.metadata.replay {
                replay.validate_entry(
                    state.metadata.head,
                    position,
                    &envelope,
                    &store.codec,
                    &mut replay_scan,
                )?;
            }
            entries.insert(position, envelope);
            expected = position.checked_add(1);
        }
        if entries.is_empty() {
            return Err(backend("QoS page ended before journal head"));
        }
        state.entries = entries;
        Ok(())
    }

    async fn refill_shared_delivery(
        &self,
        endpoint: &Arc<Endpoint>,
    ) -> Result<Option<Delivery>, PipeError> {
        let store = self.storage()?.ok_or(PipeError::Closed)?;
        store
            .transaction
            .run(async {
                // Writers take the group gate before channel state. Never invert it.
                let mut state = self.state.lock().await;
                endpoint
                    .check()
                    .map_err(drasi_core::interface::IndexError::other)?;
                let cursor = &state.metadata.cursors[&endpoint.subscriber];
                if cursor.retired {
                    return Err(drasi_core::interface::IndexError::other(backend(
                        "QoS subscriber is retired",
                    ))
                    .into());
                }
                let after = cursor.position;
                if !endpoint.binding.pending.load(Ordering::Acquire)
                    && state.oldest.map_or(true, |oldest| after >= oldest - 1)
                {
                    self.cache_after(&mut state, after)
                        .await
                        .map_err(drasi_core::interface::IndexError::other)?;
                    endpoint
                        .check()
                        .map_err(drasi_core::interface::IndexError::other)?;
                    return Ok(endpoint.delivery(&state, after));
                }
                Ok(None)
            })
            .await
            .map_err(|error| PipeError::Backend(error.into()))
    }

    async fn retained_sequence(
        &self,
        state: &mut State,
        sequence: u64,
    ) -> Result<Option<u64>, PipeError> {
        let mut after = state.oldest.map_or(0, |oldest| oldest - 1);
        loop {
            self.cache_after(state, after).await?;
            if let Some((position, _)) = state
                .entries
                .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
                .find(|(_, saved)| saved.system().sequence() == sequence)
            {
                return Ok(Some(*position));
            }
            if state.page_limits.is_none() {
                return Ok(None);
            }
            let Some((last, _)) = state.entries.last_key_value() else {
                return Ok(None);
            };
            if *last >= state.metadata.head {
                return Ok(None);
            }
            after = *last;
        }
    }

    async fn persist(
        &self,
        metadata: &Metadata,
        append: Option<(u64, &ChangeEnvelope, u64)>,
    ) -> Result<(), PipeError> {
        let Some(store) = self.storage()? else {
            return Ok(());
        };
        if store.shared {
            return Err(backend(
                "shared QoS writes must use the group transaction path",
            ));
        }
        let bytes =
            Bytes::from(serde_json::to_vec(metadata).map_err(|error| backend(error.to_string()))?);
        if (metadata.admission.is_some() || metadata.replay.is_some())
            && bytes.len() > admission::MAX_METADATA_BYTES
        {
            return Err(backend("receipt metadata exceeds its 4 MiB bound"));
        }
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
        if self.shared.is_some() {
            return self.publish_shared(envelope, endpoint).await;
        }
        if envelope.system().stream() != &self.definition.stream {
            return Err(backend("QoS channel received an event from another stream"));
        }
        let mut replay_candidate = None;
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
            if let Some(replay) = &state.metadata.replay {
                let candidate = match &replay_candidate {
                    Some(candidate) => candidate,
                    None => replay_candidate.insert(replay::Candidate::new(
                        envelope,
                        &self.storage()?.ok_or(PipeError::Closed)?.codec,
                    )?),
                };
                if let Some(position) = replay.check(candidate)? {
                    return Ok(EnqueueReceipt::new(envelope.id().clone()).with_position(position));
                }
                if let Some(previous) = state.metadata.producer_sequence {
                    if sequence <= previous {
                        return Err(ReplayRejection::TransportSequence {
                            previous,
                            received: sequence,
                        }
                        .into());
                    }
                }
            } else if state
                .metadata
                .producer_sequence
                .is_some_and(|saved| sequence <= saved)
            {
                if let Some(position) = self.retained_sequence(&mut state, sequence).await? {
                    if let Some(endpoint) = endpoint {
                        endpoint.check()?;
                    }
                    let saved = &state.entries[&position];
                    // Full comparison includes source position and branch context.
                    let same = if let Some(store) = self.storage()? {
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
                            EnqueueReceipt::new(envelope.id().clone()).with_position(position)
                        );
                    }
                }
                return Err(backend("QoS producer sequence regressed, changed, or is outside retained retry history"));
            }
            if state.metadata.admission.is_some() {
                return Err(backend(
                    "this channel requires a registered producer session for new admission",
                ));
            }
            let (retain_from, budget) = match state.prepare_append(&self.definition, envelope) {
                Ok(prepared) => prepared,
                Err(PipeError::CapacityExhausted) => {
                    drop(state);
                    notified.await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let position = state
                .metadata
                .head
                .checked_add(1)
                .ok_or_else(|| backend("QoS journal sequence exhausted"))?;
            let mut metadata = state.metadata.clone();
            metadata.head = position;
            metadata.producer_sequence = Some(sequence);
            if let (Some(replay), Some(candidate)) = (&mut metadata.replay, &replay_candidate) {
                replay.record(candidate, position);
            }
            let _wake = WakeOnDrop(&self.changed);
            self.persist(&metadata, Some((position, envelope, retain_from)))
                .await?;
            state.append_committed(position, envelope.clone(), retain_from, budget);
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
        if self.shared.is_some() {
            return self.retire_shared(subscriber).await;
        }
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
        if self.configuring_replay.load(Ordering::Acquire) {
            return Err(backend("output replay configuration is still committing"));
        }
        if bindings
            .consumers
            .get(subscriber)
            .is_some_and(|binding| !binding.cancelled.load(Ordering::Acquire))
        {
            return Err(backend("QoS subscriber already has a live owner"));
        }
        if let Some(shared) = &self.shared {
            if shared.subscriber_write_pending(subscriber)? {
                return Err(backend(
                    "QoS subscriber has an unfinished progress operation",
                ));
            }
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
        if let Some(store) = self.storage()? {
            store.transaction.cancel_retirement()?;
        }
        let _state = self.state.lock().await;
        if let Some(store) = self.storage()? {
            store.transaction.shutdown().await?;
        }
        self.persistent
            .lock()
            .map_err(|_| backend("QoS storage poisoned"))?
            .take();
        Ok(())
    }
}

struct Endpoint {
    channel: Arc<QosChannel>,
    subscriber: String,
    binding: Arc<Binding>,
}
impl Endpoint {
    fn delivery(self: &Arc<Self>, state: &State, after: u64) -> Option<Delivery> {
        let (position, envelope) = state
            .entries
            .range((std::ops::Bound::Excluded(after), std::ops::Bound::Unbounded))
            .next()?;
        let delivery = Delivery::new(
            envelope.clone(),
            Some(Box::new(Ack {
                endpoint: self.clone(),
                position: *position,
            })),
        );
        self.binding.pending.store(true, Ordering::Release);
        Some(delivery)
    }

    fn check_writable(&self) -> Result<(), PipeError> {
        self.check()?;
        if self.binding.closed.load(Ordering::Acquire) {
            return Err(PipeError::Closed);
        }
        Ok(())
    }

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
        if self.channel.shared.is_some() {
            return self.channel.complete_shared(self, position, skip).await;
        }
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
            let mut state = self.endpoint.channel.state.lock().await;
            self.endpoint.check()?;
            let cursor = &state.metadata.cursors[&self.endpoint.subscriber];
            if cursor.retired {
                return Err(backend("QoS subscriber is retired"));
            }
            if self.endpoint.binding.pending.load(Ordering::Acquire) {
                drop(state);
                self.endpoint.channel.wait_changed(notified).await?;
                continue;
            }
            if let Some(oldest) = state.oldest {
                if cursor.position < oldest - 1 {
                    let requested = cursor.position;
                    drop(state);
                    if self.gap_policy == ReplayGapPolicy::Strict {
                        return Err(PipeError::PositionUnavailable { requested, oldest });
                    }
                    self.endpoint.complete(oldest - 1, true).await?;
                    log::warn!(
                        "QoS subscriber {} explicitly skips positions {}..{}",
                        self.endpoint.subscriber,
                        requested + 1,
                        oldest
                    );
                    continue;
                }
            }
            let after = cursor.position;
            if self.endpoint.channel.shared.is_some()
                && state.page_limits.is_some()
                && after.checked_add(1).is_some_and(|next| {
                    next <= state.metadata.head && !state.entries.contains_key(&next)
                })
            {
                drop(state);
                let delivery = self
                    .endpoint
                    .channel
                    .refill_shared_delivery(&self.endpoint)
                    .await?;
                self.endpoint.check()?;
                if delivery.is_some() {
                    return Ok(delivery);
                }
                continue;
            }
            self.endpoint.channel.cache_after(&mut state, after).await?;
            self.endpoint.check()?;
            if let Some(delivery) = self.endpoint.delivery(&state, after) {
                return Ok(Some(delivery));
            }
            if self.endpoint.binding.closed.load(Ordering::Acquire) {
                return Ok(None);
            }
            drop(state);
            self.endpoint.channel.wait_changed(notified).await?;
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

#[cfg(test)]
mod foundation_tests {
    use super::*;

    fn definition() -> QosChannelDefinition {
        QosChannelDefinition {
            stream: StreamId::try_new("test/out").expect("stream"),
            capacity: NonZeroUsize::new(2).expect("capacity"),
            durable: false,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
        }
    }

    #[test]
    fn invalid_qos_definitions_never_construct_a_channel_or_claim_a_supported_profile() {
        let mut invalid = definition();
        invalid.subscribers.clear();
        assert!(QosChannel::volatile(invalid).is_err());
        for id in ["", "a b", "a\nb"] {
            let mut invalid = definition();
            invalid.subscribers = BTreeMap::from([(id.into(), SubscriptionStart::Earliest)]);
            assert!(QosChannel::volatile(invalid).is_err());
        }
        let mut invalid = definition();
        invalid.capacity = NonZeroUsize::new(usize::MAX).expect("capacity");
        assert!(matches!(
            QosChannel::volatile(invalid),
            Err(PipeError::InvalidCapacity)
        ));
        let mut invalid = definition();
        invalid.durable = true;
        assert!(QosChannel::volatile(invalid).is_err());
        let mut invalid = definition();
        invalid
            .subscribers
            .insert("consumer".into(), SubscriptionStart::After(1));
        assert!(QosChannel::volatile(invalid).is_err());
        let mut config = definition().pipe(ResourceId::try_new("queue").expect("id"), "unknown");
        assert!(config.capabilities().is_err());
        config.subscriber = "consumer".into();
        config.gap_policy = ReplayGapPolicy::SkipWithNotification;
        assert!(config.capabilities().is_err());
        for start in [
            SubscriptionStart::Earliest,
            SubscriptionStart::Latest,
            SubscriptionStart::After(0),
        ] {
            let mut valid = definition();
            valid.subscribers.insert("consumer".into(), start);
            assert_eq!(
                QosChannel::volatile(valid).expect("valid").durability(),
                drasi_core::interface::StorageDurability::VOLATILE
            );
        }
    }

    #[tokio::test]
    async fn qos_rejects_reused_producer_identity_invalid_progress_and_counter_exhaustion() {
        let channel = QosChannel::volatile(definition()).expect("channel");
        let mut pipe = channel
            .bind("consumer", ReplayGapPolicy::Strict)
            .expect("binding");
        assert!(channel.bind("unknown", ReplayGapPolicy::Strict).is_err());
        assert!(channel.bind("consumer", ReplayGapPolicy::Strict).is_err());
        assert!(channel.retire("consumer").await.is_err());
        assert!(channel.retire("unknown").await.is_err());
        let event = super::super::pipe::test_envelope(1);
        assert_eq!(
            channel.publish(&event).await.expect("append").position(),
            Some(1)
        );
        assert_eq!(
            channel.publish(&event).await.expect("retry").position(),
            Some(1)
        );
        assert!(
            channel
                .publish(&super::super::pipe::test_envelope(1))
                .await
                .is_err(),
            "an independently constructed volatile event is not the same immutable retry"
        );
        let wrong_stream = event.derive(
            event.id().clone(),
            event.changes().clone(),
            SystemMetadata::new(StreamId::try_new("another/out").expect("stream"), 2),
        );
        assert!(channel.publish(&wrong_stream).await.is_err());
        let endpoint = Endpoint {
            channel: channel.clone(),
            subscriber: "consumer".into(),
            binding: channel.bindings.lock().expect("bindings").consumers["consumer"].clone(),
        };
        assert!(endpoint.complete(2, false).await.is_err());
        assert!(endpoint.complete(0, false).await.is_err());
        assert_eq!(
            channel.progress().await.expect("progress").processed["consumer"],
            0
        );
        let mut receiver = pipe.pipe.take_receiver().expect("receiver");
        receiver
            .receive()
            .await
            .expect("receive")
            .expect("event")
            .into_parts()
            .1
            .expect("ack")
            .complete(HandlingOutcome::Handled)
            .await
            .expect("handled");
        assert!(endpoint.complete(0, false).await.is_err());
        pipe.control.cancel();
        channel.retire("consumer").await.expect("retired");
        assert_eq!(
            channel.progress().await.expect("progress").retired,
            ["consumer"]
        );
        channel.bindings.lock().expect("bindings").generation = u64::MAX;
        assert!(channel.bind("consumer", ReplayGapPolicy::Strict).is_err());
        channel.state.lock().await.metadata.head = u64::MAX;
        assert!(channel
            .publish(&super::super::pipe::test_envelope(2))
            .await
            .is_err());
        channel.shutdown().await.expect("shutdown");
        assert!(matches!(channel.progress().await, Err(PipeError::Closed)));
    }

    #[tokio::test]
    async fn poisoned_qos_ownership_is_an_error_not_an_empty_journal() {
        for storage in [false, true] {
            let channel = QosChannel::volatile(definition()).expect("channel");
            let mut pipe = channel
                .bind("consumer", ReplayGapPolicy::Strict)
                .expect("binding");
            let mut receiver = pipe.pipe.take_receiver().expect("receiver");
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                if storage {
                    let _guard = channel.persistent.lock().expect("storage");
                    panic!("injected storage ownership failure");
                } else {
                    let _guard = channel.bindings.lock().expect("bindings");
                    panic!("injected binding ownership failure");
                }
            }))
            .is_err());
            assert!(channel.bind("consumer", ReplayGapPolicy::Strict).is_err());
            assert!(receiver.receive().await.is_err());
            assert!(pipe
                .pipe
                .sender()
                .send(super::super::pipe::test_envelope(1))
                .await
                .is_err());
            assert!(pipe.control.is_idle().await.is_err());
            assert!(channel.retire("consumer").await.is_err());
            if storage {
                assert!(channel.progress().await.is_err());
                assert!(channel.shutdown().await.is_err());
            } else {
                channel
                    .shutdown()
                    .await
                    .expect("storage still has a cleanup owner");
            }
        }
    }
}
