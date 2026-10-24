// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Bounded source-time merging, independent of ordinary FIFO pipe scheduling.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use drasi_core::computation::{ComputationIndexProvider, ComputationTransaction};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{sync::watch, time::Instant};

use super::{output_bindings::OutputBindingState, producer_progress::GraphInputProgress, *};

mod recovery;

/// A late event is strictly older than the last event emitted on `out`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LateEventPolicy {
    /// Persist/hold the late input and fail. Explicit stopped-owner discard is
    /// required before processing can resume.
    #[default]
    FailAndRetain,
    /// Publish the late input on `late`, never on the ordered `out` port.
    Route,
    /// Acknowledge and drop the late input, with a warning and a counter.
    Discard,
}

/// Fixed source membership and bounds for one merger. All ports use the supplied
/// schema. `sources` order is the tie-breaker for equal buffered source times.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceTimeMergeDefinition {
    pub graph_id: String,
    pub id: ComponentId,
    pub sources: Vec<StreamId>,
    pub output_stream: StreamId,
    pub late_output_stream: Option<StreamId>,
    /// Allowed backwards movement behind each source's greatest observed time.
    pub reorder_window_ms: u64,
    /// Maximum residence time before an event and all earlier buffered events
    /// become eligible. Downstream backpressure can still delay delivery.
    pub max_wait_ms: u64,
    /// Quiet sources stop constraining early release after this interval.
    /// None keeps every source in the early-release calculation.
    pub idle_timeout_ms: Option<u64>,
    pub max_buffered_events: NonZeroUsize,
    /// Complete binary input-envelope bytes, including held and pending events.
    pub max_buffered_bytes: NonZeroUsize,
    #[serde(default)]
    pub late_policy: LateEventPolicy,
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum SourceTimeMergeError {
    #[error("source-time merger requires an explicit source-change timestamp")]
    MissingSourceTime,
    #[error("source-time merger received an undeclared stream: {0}")]
    UnknownSource(StreamId),
    #[error("source-time merger buffer capacity exceeded; input was not admitted")]
    Capacity,
    #[error("source-time merger requires reconstruction after interrupted storage")]
    RecoveryRequired,
    #[error("source-time merge replay receipt expired for {stream}, sequence {sequence}")]
    ReceiptExpired { stream: StreamId, sequence: u64 },
    #[error("source-time merge replay has conflicting content or source identity")]
    ReplayConflict,
    #[error("late event from {stream} at {time} precedes emitted source time {emitted}")]
    Late {
        stream: StreamId,
        time: DateTime<Utc>,
        emitted: DateTime<Utc>,
    },
}

/// Coalescing observations, not an event history or a delivery acknowledgement.
#[derive(Debug, Clone, Default)]
pub struct SourceTimeMergeSnapshot {
    pub running: bool,
    pub persistent: bool,
    pub buffered_events: usize,
    pub buffered_bytes: usize,
    pub last_emitted_time: Option<DateTime<Utc>>,
    pub emitted: u64,
    pub late_events: u64,
    pub discarded: u64,
    pub replayed_inputs: u64,
    pub held_event: Option<ChangeEnvelope>,
}

type Key = (DateTime<Utc>, usize, u64);

const MAX_SOURCE_IDENTITY_BYTES: usize = 4096;

struct Buffered {
    envelope: ChangeEnvelope,
    bytes: usize,
    deadline: Instant,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceState {
    sequence: u64,
    digest: [u8; 32],
    progress_key: Option<String>,
    producer: Option<GraphProducerIdentity>,
    max_time: Option<DateTime<Utc>>,
}

struct Pending {
    input: Buffered,
    late: bool,
    logical_sequence: u64,
}

struct State {
    sources: Vec<SourceState>,
    active_at: Vec<Instant>,
    buffer: BTreeMap<Key, Buffered>,
    deadlines: BTreeSet<(Instant, Key)>,
    bytes: usize,
    pending: Option<Pending>,
    held: Option<Key>,
    last_emitted: Option<DateTime<Utc>>,
    logical_sequence: u64,
    late_sequence: u64,
    emission_sequence: u64,
    late_events: u64,
    discarded: u64,
    identity: GraphProducerIdentity,
    late_identity: Option<GraphProducerIdentity>,
    destinations: OutputBindings,
}

#[derive(Clone, Copy, Default)]
struct Schedule {
    pending: bool,
    deadline: Option<Instant>,
}

struct MergeWakeup(watch::Sender<Schedule>);

#[async_trait]
impl WakeupSource for MergeWakeup {
    async fn wait(&self) -> anyhow::Result<()> {
        let mut receiver = self.0.subscribe();
        loop {
            let schedule = *receiver.borrow_and_update();
            match schedule.deadline {
                Some(deadline) => tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => return Ok(()),
                    changed = receiver.changed() => changed?,
                },
                None => receiver.changed().await?,
            }
        }
    }

    async fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(self.0.borrow().pending)
    }
}

/// One output event per bounded continuation. Inputs are admitted in each
/// producer's sequence order, but buffered/emitted by source time. Output owns a
/// new stream/sequence; original identities, timestamps and payloads are retained.
pub struct SourceTimeMergeTransformer {
    definition: SourceTimeMergeDefinition,
    descriptor: ComponentDescriptor,
    codec: BinaryEnvelopeCodec,
    schema: Arc<Schema>,
    state: State,
    transaction: Option<Arc<ComputationTransaction>>,
    bindings: Arc<OutputBindingState>,
    failed: Arc<AtomicBool>,
    wakeup: Arc<MergeWakeup>,
    observations: watch::Sender<SourceTimeMergeSnapshot>,
    running: bool,
    loaded: bool,
    in_flight: Option<OutputEnvelope>,
    replayed_inputs: u64,
}

impl SourceTimeMergeTransformer {
    pub fn new(definition: SourceTimeMergeDefinition, schema: Arc<Schema>) -> anyhow::Result<Self> {
        Self::construct(definition, schema, false)
    }

    /// Atomically retain admissions, buffer, frontier and unconfirmed output.
    /// Every outgoing branch must provide durable acceptance, replay and explicit
    /// acknowledgement. Separate-store QoS replay tracking deduplicates retries.
    pub async fn new_durable(
        definition: SourceTimeMergeDefinition,
        schema: Arc<Schema>,
        provider: Arc<dyn ComputationIndexProvider>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !provider.is_volatile() && provider.transaction_group().is_none(),
            "time merge requires its own persistent transaction owner"
        );
        let mut result = Self::construct(definition, schema, true)?;
        let indexes = provider
            .create_indexes(&result.definition.graph_id, result.definition.id.as_str())
            .await?;
        anyhow::ensure!(
            indexes.cleanup().is_some()
                && indexes
                    .checkpoint_store()
                    .is_some_and(|store| store.is_persistent()),
            "time merge requires persistent checkpoints and asynchronous cleanup"
        );
        let transaction = Arc::new(ComputationTransaction::try_new(indexes)?);
        result
            .bindings
            .bind_standalone_owner(&provider, Arc::downgrade(&transaction))?;
        result.transaction = Some(transaction);
        result.publish();
        Ok(result)
    }

    fn construct(
        definition: SourceTimeMergeDefinition,
        schema: Arc<Schema>,
        persistent: bool,
    ) -> anyhow::Result<Self> {
        data::validate_identifier("merge graph", &definition.graph_id)?;
        anyhow::ensure!(
            definition.graph_id.len() <= 256
                && definition.id.as_str().len() <= 256
                && definition.output_stream.as_str().len() <= 256
                && definition
                    .late_output_stream
                    .as_ref()
                    .map_or(true, |stream| stream.as_str().len() <= 256),
            "merge producer identifiers must not exceed 256 bytes"
        );
        anyhow::ensure!(
            !definition.sources.is_empty() && definition.sources.len() <= 256,
            "time merge requires 1..=256 declared source streams"
        );
        let mut streams = BTreeSet::new();
        for stream in &definition.sources {
            anyhow::ensure!(streams.insert(stream), "duplicate merge source stream");
        }
        anyhow::ensure!(
            streams.insert(&definition.output_stream),
            "merge output cannot also be an input"
        );
        if let Some(late) = &definition.late_output_stream {
            anyhow::ensure!(streams.insert(late), "duplicate merge late-output stream");
        }
        anyhow::ensure!(
            (definition.late_policy == LateEventPolicy::Route)
                == definition.late_output_stream.is_some(),
            "a late output stream is required exactly when the late policy is Route"
        );
        anyhow::ensure!(
            definition.max_wait_ms > 0 && definition.idle_timeout_ms != Some(0),
            "merge wait and idle timeout must be positive"
        );
        anyhow::ensure!(
            i64::try_from(definition.reorder_window_ms).is_ok(),
            "merge reorder window is out of range"
        );
        for millis in [Some(definition.max_wait_ms), definition.idle_timeout_ms]
            .into_iter()
            .flatten()
        {
            anyhow::ensure!(
                Instant::now()
                    .checked_add(Duration::from_millis(millis))
                    .is_some(),
                "merge timer deadline overflows"
            );
        }
        anyhow::ensure!(
            schema.descriptor() != QueryChangeCodec::schema().descriptor(),
            "source-time merge consumes source events, not query results"
        );
        let requirements =
            PipeRequirements::new([PipeCapability::FifoPerStream, PipeCapability::Backpressure]);
        let output_requirements = if persistent {
            PipeRequirements::new([
                PipeCapability::DurableAcceptance,
                PipeCapability::Replay,
                PipeCapability::ExplicitAcknowledgement,
                PipeCapability::FifoPerStream,
                PipeCapability::Backpressure,
            ])
        } else {
            requirements.clone()
        };
        let mut ports = vec![
            PortDescriptor::new(
                port("in"),
                PortDirection::Input,
                schema.descriptor().clone(),
                requirements,
            ),
            PortDescriptor::new(
                port("out"),
                PortDirection::Output,
                schema.descriptor().clone(),
                output_requirements.clone(),
            ),
        ];
        if definition.late_output_stream.is_some() {
            ports.push(PortDescriptor::new(
                port("late"),
                PortDirection::Output,
                schema.descriptor().clone(),
                output_requirements,
            ));
        }
        let descriptor = ComponentDescriptor::try_new(definition.id.clone(), ports)?;
        let codec_limit = definition
            .max_buffered_bytes
            .get()
            .checked_mul(4)
            .and_then(|bytes| bytes.checked_add(65536))
            .and_then(NonZeroUsize::new)
            .ok_or_else(|| anyhow::anyhow!("merge envelope limit overflow"))?;
        let mut codec = BinaryEnvelopeCodec::new(codec_limit);
        codec.register_schema(schema.clone())?;
        let identity = |stream: StreamId| {
            GraphProducerIdentity::new(
                definition.graph_id.clone(),
                definition.graph_id.clone(),
                definition.id.clone(),
                stream,
                persistent,
            )
        };
        let state = State {
            sources: vec![SourceState::default(); definition.sources.len()],
            active_at: vec![Instant::now(); definition.sources.len()],
            buffer: BTreeMap::new(),
            deadlines: BTreeSet::new(),
            bytes: 0,
            pending: None,
            held: None,
            last_emitted: None,
            logical_sequence: 0,
            late_sequence: 0,
            emission_sequence: 0,
            late_events: 0,
            discarded: 0,
            identity: identity(definition.output_stream.clone())?,
            late_identity: definition
                .late_output_stream
                .clone()
                .map(identity)
                .transpose()?,
            destinations: OutputBindings::default(),
        };
        let failed = Arc::new(AtomicBool::new(false));
        Ok(Self {
            definition,
            descriptor,
            codec,
            schema,
            state,
            transaction: None,
            bindings: OutputBindingState::new(failed.clone()),
            failed,
            wakeup: Arc::new(MergeWakeup(watch::channel(Schedule::default()).0)),
            observations: watch::channel(SourceTimeMergeSnapshot::default()).0,
            running: false,
            loaded: false,
            in_flight: None,
            replayed_inputs: 0,
        })
    }

    pub fn subscribe(&self) -> watch::Receiver<SourceTimeMergeSnapshot> {
        self.observations.subscribe()
    }

    /// Explicit administrative loss acknowledgement. Stop the owner first, or
    /// reconstruct it against the same storage. Only the held late event is
    /// removed; queued events and the emitted frontier remain unchanged.
    pub async fn discard_held_event(&mut self) -> anyhow::Result<()> {
        anyhow::ensure!(
            !self.running,
            "stop the merger before resolving a held event"
        );
        self.check_storage()?;
        self.load().await?;
        let key = self
            .state
            .held
            .ok_or_else(|| anyhow::anyhow!("no held late event"))?;
        let mut guard = self.guard();
        let buffered = self.remove(key)?;
        self.state.bytes -= buffered.bytes;
        self.state.held = None;
        self.state.discarded = self
            .state
            .discarded
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("discard counter overflow"))?;
        self.save().await?;
        log::warn!(
            "source-time merger {} explicitly discarded held input {:?} at {}",
            self.definition.id,
            buffered.envelope.id(),
            key.0
        );
        guard.complete = true;
        self.publish();
        Ok(())
    }

    fn check_storage(&self) -> anyhow::Result<()> {
        if self.failed.load(Ordering::Acquire) {
            return Err(SourceTimeMergeError::RecoveryRequired.into());
        }
        Ok(())
    }

    fn check_running(&self) -> anyhow::Result<()> {
        self.check_storage()?;
        anyhow::ensure!(self.running, "source-time merger is not running");
        self.check_held()
    }

    fn check_held(&self) -> anyhow::Result<()> {
        if let Some(key) = self.state.held {
            return Err(self.late_error(key).into());
        }
        Ok(())
    }

    fn late_error(&self, key: Key) -> SourceTimeMergeError {
        SourceTimeMergeError::Late {
            stream: self.definition.sources[key.1].clone(),
            time: key.0,
            emitted: self
                .state
                .last_emitted
                .expect("held event has an emitted frontier"),
        }
    }

    fn guard(&self) -> MutationGuard {
        MutationGuard {
            failed: self.failed.clone(),
            complete: false,
        }
    }

    fn count(&self) -> usize {
        self.state.buffer.len() + usize::from(self.state.pending.is_some())
    }

    fn publish(&self) {
        self.bindings.pending(self.count() > 0);
        self.observations.send_replace(SourceTimeMergeSnapshot {
            running: self.running,
            persistent: self.transaction.is_some(),
            buffered_events: self.count(),
            buffered_bytes: self.state.bytes,
            last_emitted_time: self.state.last_emitted,
            emitted: self.state.logical_sequence,
            late_events: self.state.late_events,
            discarded: self.state.discarded,
            replayed_inputs: self.replayed_inputs,
            held_event: self
                .state
                .held
                .and_then(|key| self.state.buffer.get(&key))
                .map(|entry| entry.envelope.clone()),
        });
        let now = Instant::now();
        let deadline = if !self.running || self.state.held.is_some() || self.in_flight.is_some() {
            None
        } else if self.state.pending.is_some() || self.eligible(now) {
            Some(now)
        } else {
            let residence = self.state.deadlines.first().map(|(deadline, _)| *deadline);
            let idle = self.definition.idle_timeout_ms.and_then(|millis| {
                self.state
                    .active_at
                    .iter()
                    .map(|at| *at + Duration::from_millis(millis))
                    .filter(|deadline| *deadline > now)
                    .min()
            });
            if self.state.buffer.is_empty() {
                None
            } else {
                residence.into_iter().chain(idle).min()
            }
        };
        self.wakeup.0.send_replace(Schedule {
            pending: self.count() > 0,
            deadline,
        });
    }

    fn eligible(&self, now: Instant) -> bool {
        let Some((&key, _)) = self.state.buffer.first_key_value() else {
            return false;
        };
        // An expired event releases all earlier buffered events, not just itself.
        if self
            .state
            .deadlines
            .first()
            .is_some_and(|(deadline, _)| *deadline <= now)
        {
            return true;
        }
        let mut active = false;
        for (source, at) in self.state.sources.iter().zip(&self.state.active_at) {
            if self
                .definition
                .idle_timeout_ms
                .is_some_and(|ms| now.duration_since(*at) >= Duration::from_millis(ms))
            {
                continue;
            }
            active = true;
            let Some(high) = source.max_time else {
                return false;
            };
            let Some(cutoff) = high.checked_sub_signed(chrono::Duration::milliseconds(
                self.definition.reorder_window_ms as i64,
            )) else {
                return false;
            };
            if key.0 > cutoff {
                return false;
            }
        }
        active
    }

    fn remove(&mut self, key: Key) -> anyhow::Result<Buffered> {
        let buffered = self
            .state
            .buffer
            .remove(&key)
            .ok_or_else(|| anyhow::anyhow!("missing merge buffer entry"))?;
        self.state.deadlines.remove(&(buffered.deadline, key));
        Ok(buffered)
    }

    async fn emit(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        anyhow::ensure!(
            self.in_flight.is_none(),
            "confirm merge output before continuing"
        );
        if self.state.pending.is_none() {
            if !self.eligible(Instant::now()) {
                self.publish();
                return Ok(Vec::new());
            }
            let key = *self
                .state
                .buffer
                .first_key_value()
                .expect("eligible buffer")
                .0;
            let input = self.remove(key)?;
            self.state.logical_sequence = self
                .state
                .logical_sequence
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("merge output sequence overflow"))?;
            self.state.last_emitted = Some(key.0);
            self.state.pending = Some(Pending {
                input,
                late: false,
                logical_sequence: self.state.logical_sequence,
            });
        }
        self.state.emission_sequence = self
            .state
            .emission_sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("merge transport sequence overflow"))?;
        let pending = self.state.pending.as_ref().expect("pending emission");
        let identity = if pending.late {
            self.state
                .late_identity
                .as_ref()
                .expect("configured late output")
        } else {
            &self.state.identity
        };
        let input = &pending.input.envelope;
        let metadata = SystemMetadata::new(identity.stream().clone(), self.state.emission_sequence)
            .with_timestamp(input.system().timestamp().expect("validated source time"));
        let mut envelope = input.derive(
            emission_id(identity.stream(), self.state.emission_sequence)?,
            input.changes().clone(),
            metadata,
        );
        GraphProducerProgress::annotate(&mut envelope, identity, pending.logical_sequence)?;
        if pending.late {
            envelope.append_annotation(ContextEntry::try_new(
                self.definition.id.clone(),
                "drasi.time-merge.late-after",
                ContextValue::String(Arc::from(
                    self.state.last_emitted.expect("late frontier").to_rfc3339(),
                )),
            )?)?;
        }
        let output = OutputEnvelope {
            port: port(if pending.late { "late" } else { "out" }),
            envelope,
        };
        self.save().await?;
        self.in_flight = Some(output.clone());
        self.publish();
        Ok(vec![output])
    }
}

struct MutationGuard {
    failed: Arc<AtomicBool>,
    complete: bool,
}
impl Drop for MutationGuard {
    fn drop(&mut self) {
        if !self.complete {
            self.failed.store(true, Ordering::Release);
        }
    }
}

fn port(name: &str) -> PortId {
    PortId::try_new(name).expect("fixed merge port")
}

#[async_trait]
impl ComputationComponent for SourceTimeMergeTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    fn recovery_contract(&self) -> ComponentRecovery {
        self.transaction
            .as_ref()
            .map_or_else(ComponentRecovery::default, |transaction| {
                ComponentRecovery::transactional(transaction.resources())
                    .expect("validated transaction")
                    .with_output_bindings(self.bindings.clone())
            })
    }

    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(
            serde_json::json!({ "source_time_merge": self.definition, "persistent": self.transaction.is_some() }),
        )
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.check_storage()?;
        if self.running {
            return self.check_held();
        }
        self.load().await?;
        self.check_held()?;
        self.state.active_at.fill(Instant::now());
        self.running = true;
        self.in_flight = None;
        self.publish();
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.running = false;
        self.in_flight = None;
        self.publish();
        if let Some(transaction) = &self.transaction {
            let mut guard = self.guard();
            if self.failed.load(Ordering::Acquire) || transaction.recovery_required() {
                transaction.shutdown().await?;
            } else {
                transaction.quiesce().await?;
            }
            guard.complete = true;
        }
        Ok(())
    }
}

#[async_trait]
impl Transformer for SourceTimeMergeTransformer {
    async fn bind_output_destinations(&mut self, bindings: &OutputBindings) -> anyhow::Result<()> {
        self.check_running()?;
        anyhow::ensure!(
            !bindings.has_shared(),
            "time merger cannot join a producer's shared storage group"
        );
        self.state
            .destinations
            .validate_change(bindings, self.count() > 0)?;
        let mut guard = self.guard();
        self.state.destinations = bindings.clone();
        self.save().await?;
        self.bindings.publish(bindings.clone(), self.count() > 0)?;
        guard.complete = true;
        Ok(())
    }

    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        Some(self.wakeup.clone())
    }

    fn has_pending_emissions(&self) -> bool {
        self.running
            && self.state.held.is_none()
            && self.in_flight.is_none()
            && (self.state.pending.is_some() || self.eligible(Instant::now()))
    }

    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.continue_transform().await
    }

    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.check_running()?;
        let mut guard = self.guard();
        let result = self.emit().await?;
        guard.complete = true;
        Ok(result)
    }

    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.check_running()?;
        anyhow::ensure!(input.port == port("in"), "unknown time merge input port");
        anyhow::ensure!(
            self.state.pending.is_none(),
            "drain pending merge output before new input"
        );
        let envelope = input.envelope;
        anyhow::ensure!(
            !GraphChangeCodec::is_futures_due(&envelope),
            "source-time merge cannot reorder scheduled query control notifications"
        );
        let time = envelope
            .system()
            .timestamp()
            .ok_or(SourceTimeMergeError::MissingSourceTime)?;
        let rank = self
            .definition
            .sources
            .iter()
            .position(|source| source == envelope.system().stream())
            .ok_or_else(|| {
                SourceTimeMergeError::UnknownSource(envelope.system().stream().clone())
            })?;
        data::validate_schema(self.schema.descriptor(), envelope.changes().schema())?;
        if BinaryEnvelopeCodec::encoded_size(&envelope)? > self.definition.max_buffered_bytes.get()
        {
            return Err(SourceTimeMergeError::Capacity.into());
        }
        let bytes = self.codec.encode(&envelope)?;
        // Full-fidelity validation also rejects records made with a foreign validator.
        self.codec.decode(&bytes)?;
        let progress = GraphInputProgress::from_envelope(&envelope)?;
        anyhow::ensure!(
            progress.sequence > 0,
            "merge input sequence must be positive"
        );
        progress.validate_durability(self.transaction.is_some())?;
        let producer = GraphProducerProgress::from_envelope(&envelope)?
            .map(|progress| progress.identity().clone());
        anyhow::ensure!(
            progress.key.len() + serde_json::to_vec(&producer)?.len() <= MAX_SOURCE_IDENTITY_BYTES,
            "merge source progress identity exceeds 4096 bytes"
        );
        let digest: [u8; 32] = Sha256::digest(self.codec.encode(&envelope.reemit(1)?)?).into();
        let source = &self.state.sources[rank];
        if source.sequence > 0
            && (source.progress_key.as_ref() != Some(&progress.key) || source.producer != producer)
        {
            return Err(SourceTimeMergeError::ReplayConflict.into());
        }
        if progress.sequence <= source.sequence {
            if progress.sequence < source.sequence {
                return Err(SourceTimeMergeError::ReceiptExpired {
                    stream: envelope.system().stream().clone(),
                    sequence: progress.sequence,
                }
                .into());
            }
            anyhow::ensure!(
                source.digest == digest,
                SourceTimeMergeError::ReplayConflict
            );
            self.replayed_inputs = self.replayed_inputs.saturating_add(1);
            self.publish();
            return Ok(Vec::new());
        }
        anyhow::ensure!(
            producer
                .as_ref()
                .map_or(true, |identity| !identity.persistent())
                || source.sequence.checked_add(1) == Some(progress.sequence),
            "merge input logical progress has a gap"
        );
        if self.count() >= self.definition.max_buffered_events.get()
            || bytes.len()
                > self
                    .definition
                    .max_buffered_bytes
                    .get()
                    .saturating_sub(self.state.bytes)
        {
            return Err(SourceTimeMergeError::Capacity.into());
        }
        let mut guard = self.guard();
        let now = Instant::now();
        let key = (time, rank, progress.sequence);
        let late = self.state.last_emitted.is_some_and(|last| time < last);
        let source = &mut self.state.sources[rank];
        source.sequence = progress.sequence;
        source.digest = digest;
        source.progress_key = Some(progress.key);
        source.producer = producer;
        source.max_time = Some(source.max_time.map_or(time, |previous| previous.max(time)));
        self.state.active_at[rank] = now;
        self.state.bytes += bytes.len();
        let buffered = Buffered {
            envelope,
            bytes: bytes.len(),
            deadline: now + Duration::from_millis(self.definition.max_wait_ms),
        };
        if late {
            self.state.late_events = self
                .state
                .late_events
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("late counter overflow"))?;
            match self.definition.late_policy {
                LateEventPolicy::FailAndRetain => {
                    self.state.deadlines.insert((buffered.deadline, key));
                    self.state.buffer.insert(key, buffered);
                    self.state.held = Some(key);
                    self.save().await?;
                    self.publish();
                    guard.complete = true;
                    return Err(self.late_error(key).into());
                }
                LateEventPolicy::Route => {
                    self.state.late_sequence = self
                        .state
                        .late_sequence
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("merge output sequence overflow"))?;
                    self.state.pending = Some(Pending {
                        input: buffered,
                        late: true,
                        logical_sequence: self.state.late_sequence,
                    });
                }
                LateEventPolicy::Discard => {
                    self.state.bytes -= buffered.bytes;
                    self.state.discarded = self
                        .state
                        .discarded
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("discard counter overflow"))?;
                    log::warn!(
                        "source-time merger {} discarded late input {:?} at {}",
                        self.definition.id,
                        buffered.envelope.id(),
                        time
                    );
                }
            }
        } else {
            self.state.deadlines.insert((buffered.deadline, key));
            self.state.buffer.insert(key, buffered);
        }
        if self.state.pending.is_none() && !self.eligible(Instant::now()) {
            self.save().await?;
        }
        let result = self.emit().await?;
        guard.complete = true;
        Ok(result)
    }

    async fn delivery_completed(&mut self, outputs: &[OutputEnvelope]) -> anyhow::Result<()> {
        self.check_running()?;
        if outputs.is_empty() {
            anyhow::ensure!(
                self.in_flight.is_none(),
                "missing merge delivery confirmation"
            );
            return Ok(());
        }
        let expected = self
            .in_flight
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no merge output awaiting confirmation"))?;
        anyhow::ensure!(
            outputs.len() == 1
                && outputs[0].port == expected.port
                && self.codec.encode(&outputs[0].envelope.reemit(1)?)?
                    == self.codec.encode(&expected.envelope.reemit(1)?)?
                && outputs[0].envelope.system() == expected.envelope.system(),
            "merge delivery confirmation does not match pending output"
        );
        let mut guard = self.guard();
        let pending = self.state.pending.take().expect("in-flight pending output");
        self.state.bytes -= pending.input.bytes;
        self.save().await?;
        self.in_flight = None;
        guard.complete = true;
        self.publish();
        Ok(())
    }
}
