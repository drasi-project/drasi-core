// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Query-scoped native bootstrap. Factories construct providers without I/O;
//! the query owns preparation, snapshot polling, final commit and cleanup.
//!
//! A bootstrap factory is a declared graph component like a source or sink:
//!
//! - [`BootstrapFactory::describe`] is pure and returns the provider's
//!   [`BootstrapOutputDescription`]: how its sessions complete (quiescent or
//!   durable), the coordination protocol it requires of a paired source, its
//!   element namespace and, when it must match, its element identity. The
//!   Host forwards it unchanged and the graph checks the provider's edge when
//!   it binds.
//! - A provider is either [`QuiescentBootstrapProvider`] (records only) or
//!   [`DurableBootstrapProvider`] (watermarks, completion state, progress),
//!   matching the declared completion; every hook of each is required.
//! - The graph relation supplies the served source and stream
//!   ([`BootstrapSession::served`]) and the requesting query's declared labels
//!   ([`BootstrapSession::labels`]); providers never read them from their own
//!   configuration. A paired provider is created by the source library's
//!   coordinator, which uses the source's own settings.

use crate::{
    abi::{self, bootstrap::*},
    metadata::ConfigSchema,
    transaction::RetainedTransaction,
    transport::{self, Failure},
    wire::{self, CreateRequest},
    NativeSourceProgress,
};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_contracts::{
    decode_bounded_messagepack, encode_bounded_messagepack, BinaryEnvelopeCodec,
    BootstrapPreparation, BootstrapState, BootstrapWatermark, ComponentId,
    ComputationBootstrapSnapshot, ImplementationIdentity, PluginIdentity, SourceProgressView,
    StreamId,
};
pub use drasi_computation_contracts::{
    BootstrapCompletionKind, BootstrapLabels, BootstrapOutputDescription, BootstrapProtocol,
    ElementIdentity, ElementNamespace,
};
use drasi_core_types::interface::StorageDurability;
use futures_core::Stream;
use serde::{Deserialize, Serialize};
use std::{
    ffi::c_void,
    future::poll_fn,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    task::Poll,
};

pub type StateBytes = serde_bytes::ByteBuf;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapFactoryMetadata {
    pub implementation: ImplementationIdentity,
    pub configuration_version: u32,
    pub configuration: ConfigSchema,
    pub source_progress: bool,
    /// The coordination protocol a paired factory requires of the source it
    /// serves. A paired factory's providers are created by that source
    /// library's coordinator through [`BootstrapFactory::create_with_source_bootstrap`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pairing: Option<BootstrapProtocol>,
}
impl BootstrapFactoryMetadata {
    pub fn validate(&self, plugin: &PluginIdentity) -> anyhow::Result<()> {
        self.implementation.validate()?;
        anyhow::ensure!(
            self.implementation.plugin.as_ref() == Some(plugin),
            "bootstrap factory plugin provenance mismatch"
        );
        anyhow::ensure!(
            self.configuration_version > 0,
            "invalid bootstrap configuration version"
        );
        for name in self.configuration.fields.keys() {
            drasi_computation_contracts::validate_identifier("configuration field", name)?;
        }
        anyhow::ensure!(
            self.pairing.is_none() || self.source_progress,
            "a paired bootstrap factory requires the query's actual source progress"
        );
        Ok(())
    }

    /// Check a factory's declared output against this metadata: the pairing
    /// is the factory's, a paired or progress-bound provider completes
    /// durably, and a paired provider serves its source's namespace.
    pub fn validate_description(
        &self,
        description: &BootstrapOutputDescription,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            description.pairing == self.pairing,
            "bootstrap description pairing differs from the factory's declared pairing"
        );
        anyhow::ensure!(
            matches!(
                description.completion,
                BootstrapCompletionKind::Quiescent | BootstrapCompletionKind::Durable
            ),
            "a bootstrap provider declares a quiescent or durable completion"
        );
        anyhow::ensure!(
            !self.source_progress || description.completion == BootstrapCompletionKind::Durable,
            "a progress-bound bootstrap provider must complete durably"
        );
        anyhow::ensure!(
            self.pairing.is_none() || description.namespace == ElementNamespace::ServedSource,
            "a paired bootstrap provider serves its source's element namespace"
        );
        anyhow::ensure!(
            description.namespace != ElementNamespace::Undeclared,
            "a bootstrap provider declares its element namespace"
        );
        Ok(())
    }

    pub fn validate_request(&self, request: &CreateRequest) -> anyhow::Result<()> {
        anyhow::ensure!(
            request.implementation == self.implementation
                && request.configuration_version == self.configuration_version,
            "bootstrap implementation/configuration version mismatch"
        );
        if let Some(scope) = &request.scope {
            drasi_computation_contracts::validate_identifier("graph", &scope.graph_id)?;
            drasi_computation_contracts::validate_identifier("instance", &scope.instance_id)?;
        }
        self.configuration.validate(&request.configuration)
    }
}

/// The source whose element namespace a provider serves and the stream its
/// records belong to. The graph relation supplies it: the source whose
/// bootstrap input the provider is connected to, and that source's output
/// stream.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServedSource {
    pub source: ComponentId,
    pub stream: StreamId,
}

/// What one bootstrap session asks of a provider, from the graph rather than
/// the provider's configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BootstrapSession {
    /// The served source, for a provider declaring
    /// [`ElementNamespace::ServedSource`]; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub served: Option<ServedSource>,
    /// The labels the requesting query reads; `None` asks for everything the
    /// provider is configured for. A configured selection is only an upper
    /// bound.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BootstrapLabels>,
}

impl BootstrapSession {
    pub fn new(served: Option<ServedSource>, labels: Option<BootstrapLabels>) -> Self {
        Self { served, labels }
    }

    /// The served source, required by providers declaring
    /// [`ElementNamespace::ServedSource`].
    pub fn served(&self) -> anyhow::Result<&ServedSource> {
        self.served.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "bootstrap provider serves a source's element namespace but no served source \
                 was supplied by its graph relation"
            )
        })
    }

    /// Whether the requesting query reads elements labelled `label`.
    pub fn selects(&self, label: &str) -> bool {
        drasi_computation_contracts::labels_select(self.labels.as_ref(), label)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(labels) = &self.labels {
            labels.validate()?;
        }
        Ok(())
    }
}

/// The bytes the host sends to a bootstrap factory's `create`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootstrapCreateRequest {
    pub request: CreateRequest,
    pub session: BootstrapSession,
}

/// The bytes the host sends to a source coordinator's `open`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceBootstrapOpen {
    pub query: ComponentId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<BootstrapLabels>,
}

/// A provider that only streams records: no watermarks, no completion state,
/// no progress binding and always-ready preparation. Every hook is required.
#[async_trait]
pub trait QuiescentBootstrapProvider: Send + Sync {
    /// Reject use by any query other than the one the provider was created
    /// for.
    fn validate_query_scope(
        &self,
        instance_id: &str,
        graph_id: &str,
        component_id: &ComponentId,
    ) -> anyhow::Result<()>;
    /// The snapshot. Its watermarks must be empty.
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot>;
    /// Finish asynchronous snapshot cleanup, including owned workers. Retain
    /// unfinished work on error; the host retries before reuse.
    async fn stop(&self) -> anyhow::Result<()>;
}

/// A provider whose completion establishes a durable handover: source
/// watermarks, completion state and/or the query's source progress. Every
/// hook is required, so a forgotten watermark or completion state is a
/// compile error rather than an empty handover.
#[async_trait]
pub trait DurableBootstrapProvider: Send + Sync {
    fn validate_query_scope(
        &self,
        instance_id: &str,
        graph_id: &str,
        component_id: &ComponentId,
    ) -> anyhow::Result<()>;
    /// The query progress reader the provider is bound to, if any. It must be
    /// the reader the factory or coordinator received.
    fn recovery_reader(&self) -> Option<SourceProgressView>;
    /// Establish readiness before the query decides whether its persisted
    /// state can be reused. `state` is the query-owned bootstrap state when
    /// the query has one.
    async fn prepare(
        &self,
        state: Option<&dyn BootstrapState>,
    ) -> anyhow::Result<BootstrapPreparation>;
    /// Request a prepared snapshot even after the query completed bootstrap.
    fn has_pending_snapshot(&self) -> anyhow::Result<bool>;
    async fn snapshot(
        &self,
        state: Option<&dyn BootstrapState>,
    ) -> anyhow::Result<ComputationBootstrapSnapshot>;
    /// Boundaries that become available only after the stream closes.
    async fn complete_snapshot(&self) -> anyhow::Result<Vec<BootstrapWatermark>>;
    /// Final handover state, committed atomically with the watermarks.
    fn completion_state(&self) -> anyhow::Result<Option<Bytes>>;
    async fn stop(&self) -> anyhow::Result<()>;
}

/// A constructed provider, typed by its declared completion.
#[derive(Clone)]
pub enum NativeBootstrapProvider {
    Quiescent(Arc<dyn QuiescentBootstrapProvider>),
    Durable(Arc<dyn DurableBootstrapProvider>),
}

impl NativeBootstrapProvider {
    pub fn completion(&self) -> BootstrapCompletionKind {
        match self {
            Self::Quiescent(_) => BootstrapCompletionKind::Quiescent,
            Self::Durable(_) => BootstrapCompletionKind::Durable,
        }
    }

    pub fn recovery_reader(&self) -> Option<SourceProgressView> {
        match self {
            Self::Quiescent(_) => None,
            Self::Durable(provider) => provider.recovery_reader(),
        }
    }

    pub fn validate_query_scope(
        &self,
        instance_id: &str,
        graph_id: &str,
        component_id: &ComponentId,
    ) -> anyhow::Result<()> {
        match self {
            Self::Quiescent(provider) => {
                provider.validate_query_scope(instance_id, graph_id, component_id)
            }
            Self::Durable(provider) => {
                provider.validate_query_scope(instance_id, graph_id, component_id)
            }
        }
    }

    pub async fn stop(&self) -> anyhow::Result<()> {
        match self {
            Self::Quiescent(provider) => provider.stop().await,
            Self::Durable(provider) => provider.stop().await,
        }
    }
}

/// A native bootstrap factory: a declared graph component.
pub trait BootstrapFactory: Send + Sync + 'static {
    fn metadata(&self) -> BootstrapFactoryMetadata;
    /// Pure: the provider's declared bootstrap output for one configuration.
    /// No I/O, clock or external state. Its pairing must equal the
    /// metadata's, and a paired or progress-bound provider completes durably.
    fn describe(
        &self,
        configuration: &serde_json::Value,
    ) -> anyhow::Result<BootstrapOutputDescription>;
    /// A paired factory's provider, opened from the source library's
    /// coordinator. Only factories declaring `pairing` are asked.
    fn create_with_source_bootstrap(
        &self,
        _request: &CreateRequest,
        _source: crate::NativeSourceBootstrap,
    ) -> anyhow::Result<crate::LinkedBootstrapProvider> {
        anyhow::bail!("bootstrap factory does not implement source pairing")
    }
    /// Configuration only. A progress binding, when declared, is the actual
    /// query-owned reader; it is not a private checkpoint store. The provider
    /// variant must match the declared completion. Only unpaired factories
    /// are asked.
    fn create(
        &self,
        request: &CreateRequest,
        session: &BootstrapSession,
        progress: Option<NativeSourceProgress>,
    ) -> anyhow::Result<NativeBootstrapProvider>;
}

/// The bootstrap factory of a paired family: its providers are created by the
/// source library's coordinator from the source's own settings, so it takes
/// no configuration. It declares `protocol` and a durable completion in the
/// served source's namespace; the graph refuses it on any source that does
/// not offer the same protocol.
pub struct PairedBootstrapFactory {
    implementation: ImplementationIdentity,
    protocol: BootstrapProtocol,
}

impl PairedBootstrapFactory {
    pub fn new(implementation: ImplementationIdentity, protocol: BootstrapProtocol) -> Self {
        Self {
            implementation,
            protocol,
        }
    }

    fn description(&self) -> BootstrapOutputDescription {
        BootstrapOutputDescription {
            completion: BootstrapCompletionKind::Durable,
            pairing: Some(self.protocol.clone()),
            namespace: ElementNamespace::ServedSource,
            identity: None,
        }
    }
}

impl BootstrapFactory for PairedBootstrapFactory {
    fn metadata(&self) -> BootstrapFactoryMetadata {
        BootstrapFactoryMetadata {
            implementation: self.implementation.clone(),
            configuration_version: 1,
            configuration: ConfigSchema::from_fields(std::iter::empty()),
            source_progress: true,
            pairing: Some(self.protocol.clone()),
        }
    }
    fn describe(
        &self,
        configuration: &serde_json::Value,
    ) -> anyhow::Result<BootstrapOutputDescription> {
        self.metadata().configuration.validate(configuration)?;
        Ok(self.description())
    }
    fn create_with_source_bootstrap(
        &self,
        request: &CreateRequest,
        source: crate::NativeSourceBootstrap,
    ) -> anyhow::Result<crate::LinkedBootstrapProvider> {
        self.metadata()
            .configuration
            .validate(&request.configuration)?;
        anyhow::ensure!(
            source.metadata().protocol == self.protocol,
            "paired source coordinates {} but this provider requires {}",
            source.metadata().protocol,
            self.protocol
        );
        source.open()
    }
    fn create(
        &self,
        _: &CreateRequest,
        _: &BootstrapSession,
        _: Option<NativeSourceProgress>,
    ) -> anyhow::Result<NativeBootstrapProvider> {
        anyhow::bail!(
            "paired bootstrap provider {} is created by its source's coordinator",
            self.protocol
        )
    }
}

pub fn encode<T: Serialize>(value: &T) -> anyhow::Result<Vec<u8>> {
    Ok(encode_bounded_messagepack(value, MAX_CONTROL_BYTES)?)
}
pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> anyhow::Result<T> {
    Ok(decode_bounded_messagepack(bytes, MAX_CONTROL_BYTES)?)
}
pub fn validate_state(bytes: &[u8]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() <= MAX_STATE_BYTES,
        "native bootstrap state must contain 1..={MAX_STATE_BYTES} bytes"
    );
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub preparation: Preparation,
    pub pending: bool,
}
#[derive(Serialize, Deserialize)]
pub enum Preparation {
    Ready,
    RefreshVolatile,
    ResetRequired,
}
impl From<BootstrapPreparation> for Preparation {
    fn from(value: BootstrapPreparation) -> Self {
        match value {
            BootstrapPreparation::Ready => Self::Ready,
            BootstrapPreparation::RefreshVolatile => Self::RefreshVolatile,
            BootstrapPreparation::ResetRequired => Self::ResetRequired,
        }
    }
}
impl From<Preparation> for BootstrapPreparation {
    fn from(value: Preparation) -> Self {
        match value {
            Preparation::Ready => Self::Ready,
            Preparation::RefreshVolatile => Self::RefreshVolatile,
            Preparation::ResetRequired => Self::ResetRequired,
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Watermark {
    pub stream: StreamId,
    pub source_id: Option<ComponentId>,
    pub sequence: u64,
    pub position: Option<serde_bytes::ByteBuf>,
    pub producer: Option<drasi_computation_contracts::GraphProducerIdentity>,
}
impl From<BootstrapWatermark> for Watermark {
    fn from(value: BootstrapWatermark) -> Self {
        Self {
            stream: value.stream,
            source_id: value.source_id,
            sequence: value.sequence,
            position: value.position.map(|bytes| bytes.to_vec().into()),
            producer: value.producer,
        }
    }
}
impl From<Watermark> for BootstrapWatermark {
    fn from(value: Watermark) -> Self {
        Self {
            stream: value.stream,
            source_id: value.source_id,
            sequence: value.sequence,
            position: value.position.map(|bytes| bytes.into_vec().into()),
            producer: value.producer,
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completed {
    pub watermarks: Vec<Watermark>,
    pub state: Option<serde_bytes::ByteBuf>,
}
impl From<drasi_computation_contracts::BootstrapCompletion> for Completed {
    fn from(value: drasi_computation_contracts::BootstrapCompletion) -> Self {
        Self {
            watermarks: value.watermarks.into_iter().map(Into::into).collect(),
            state: value.state.map(|state| state.to_vec().into()),
        }
    }
}
impl TryFrom<Completed> for drasi_computation_contracts::BootstrapCompletion {
    type Error = anyhow::Error;
    fn try_from(value: Completed) -> anyhow::Result<Self> {
        validate_watermarks(&value.watermarks)?;
        if let Some(state) = &value.state {
            anyhow::ensure!(
                !state.is_empty()
                    && state.len() <= drasi_computation_contracts::MAX_BOOTSTRAP_STATE_BYTES,
                "bootstrap completion state exceeds its nonempty byte bound"
            );
        }
        Ok(Self {
            watermarks: value.watermarks.into_iter().map(Into::into).collect(),
            state: value.state.map(|state| state.into_vec().into()),
        })
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotStarted {
    pub generation: u64,
    pub watermarks: Vec<Watermark>,
}
pub fn validate_watermarks(values: &[Watermark]) -> anyhow::Result<()> {
    anyhow::ensure!(
        values.len() <= MAX_WATERMARKS,
        "native bootstrap watermark limit exceeded"
    );
    let mut keys = std::collections::BTreeSet::new();
    for value in values {
        drasi_computation_contracts::validate_bootstrap_watermark(
            &value.stream,
            value.source_id.as_ref(),
            value.position.as_ref().map(|position| position.as_slice()),
            value.producer.as_ref(),
        )?;
        let key = match &value.source_id {
            Some(source) => drasi_computation_contracts::SourceProgressKey::Source(source.clone()),
            None => drasi_computation_contracts::SourceProgressKey::Stream(value.stream.clone()),
        };
        anyhow::ensure!(keys.insert(key), "duplicate or empty bootstrap watermark");
    }
    Ok(())
}

struct RemoteState {
    requests: RetainedTransaction,
    durability: StorageDurability,
}
impl RemoteState {
    unsafe fn new(raw: *const BootstrapStateV1) -> anyhow::Result<Self> {
        let raw = unsafe { transport::checked_table(raw)? };
        anyhow::ensure!(raw.reserved == 0, "invalid bootstrap state reserved field");
        let durability =
            decode(unsafe { transport::borrowed_bytes(raw.durability, MAX_CONTROL_BYTES)? })?;
        Ok(Self {
            requests: unsafe { RetainedTransaction::new(&raw.requests)? },
            durability,
        })
    }
}
#[async_trait]
impl BootstrapState for RemoteState {
    fn durability(&self) -> StorageDurability {
        self.durability
    }
    async fn read(&self) -> anyhow::Result<Option<Bytes>> {
        let state: Option<serde_bytes::ByteBuf> = self.requests.call(READ_STATE, &()).await?;
        state
            .map(|bytes| {
                validate_state(&bytes)?;
                Ok(bytes.into_vec().into())
            })
            .transpose()
    }
    async fn write(&self, state: Bytes) -> anyhow::Result<()> {
        validate_state(&state)?;
        self.requests
            .call(WRITE_STATE, &serde_bytes::Bytes::new(&state))
            .await
    }
}

type SnapshotStream =
    Pin<Box<dyn Stream<Item = anyhow::Result<drasi_computation_contracts::ChangeEnvelope>> + Send>>;
struct SnapshotSlot {
    generation: u64,
    stream: Option<SnapshotStream>,
    waker: Option<std::task::Waker>,
}
struct Instance {
    provider: NativeBootstrapProvider,
    codec: Arc<BinaryEnvelopeCodec>,
    stream: Mutex<SnapshotSlot>,
    busy: AtomicBool,
    cleanup_required: AtomicBool,
    prepared: AtomicBool,
    reprepare_allowed: AtomicBool,
    started: AtomicBool,
    ended: AtomicBool,
    damaged: AtomicBool,
    reader: Option<SourceProgressView>,
}
impl Instance {
    fn validate_reader(&self) -> anyhow::Result<()> {
        anyhow::ensure!(
            match (&self.reader, self.provider.recovery_reader()) {
                (None, None) => true,
                (Some(expected), Some(actual)) => expected.same_owner(&actual),
                _ => false,
            },
            "native bootstrap changed its recovery reader"
        );
        Ok(())
    }
    fn cancel(&self, generation: u64) -> anyhow::Result<()> {
        let mut slot = self.stream.lock().unwrap_or_else(|error| {
            self.damaged.store(true, Ordering::Release);
            error.into_inner()
        });
        if generation != 0 && generation != slot.generation {
            return Err(Failure::closed().into());
        }
        self.cleanup_required.store(true, Ordering::Release);
        slot.generation = slot
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("native bootstrap generation exhausted"))?;
        let stream = slot.stream.take();
        let waker = slot.waker.take();
        drop(slot);
        if let Some(waker) = waker {
            waker.wake();
        }
        drop(stream);
        Ok(())
    }
    async fn call(
        &self,
        code: u32,
        generation: u64,
        state: Option<RemoteState>,
    ) -> anyhow::Result<Vec<u8>> {
        anyhow::ensure!(
            code == STOP
                || (!self.cleanup_required.load(Ordering::Acquire)
                    && !self.damaged.load(Ordering::Acquire)),
            "native bootstrap requires successful cleanup before reuse"
        );
        match code {
            PREPARE => {
                anyhow::ensure!(
                    !self.prepared.load(Ordering::Acquire)
                        || (self.reprepare_allowed.load(Ordering::Acquire)
                            && !self.started.load(Ordering::Acquire)),
                    "bootstrap already prepared"
                );
                self.validate_reader()?;
                let (preparation, pending) = match &self.provider {
                    // A quiescent provider is always ready and never asks for
                    // a snapshot after a completed bootstrap.
                    NativeBootstrapProvider::Quiescent(_) => (BootstrapPreparation::Ready, false),
                    NativeBootstrapProvider::Durable(provider) => {
                        let state = state.as_ref().map(|state| state as &dyn BootstrapState);
                        let preparation = provider.prepare(state).await?;
                        (preparation, provider.has_pending_snapshot()?)
                    }
                };
                self.validate_reader()?;
                self.reprepare_allowed.store(
                    preparation != BootstrapPreparation::Ready,
                    Ordering::Release,
                );
                self.prepared.store(true, Ordering::Release);
                encode(&Prepared {
                    preparation: preparation.into(),
                    pending,
                })
            }
            SNAPSHOT => {
                anyhow::ensure!(
                    self.prepared.load(Ordering::Acquire) && !self.started.load(Ordering::Acquire),
                    "bootstrap snapshot must follow preparation exactly once"
                );
                let ComputationBootstrapSnapshot {
                    changes,
                    watermarks,
                } = match &self.provider {
                    NativeBootstrapProvider::Quiescent(provider) => {
                        let snapshot = provider.snapshot().await?;
                        anyhow::ensure!(
                            snapshot.watermarks.is_empty(),
                            "a quiescent bootstrap provider returned watermarks"
                        );
                        snapshot
                    }
                    NativeBootstrapProvider::Durable(provider) => {
                        let state = state.as_ref().map(|state| state as &dyn BootstrapState);
                        provider.snapshot(state).await?
                    }
                };
                let watermarks: Vec<Watermark> = watermarks.into_iter().map(Into::into).collect();
                validate_watermarks(&watermarks)?;
                let mut slot = self
                    .stream
                    .lock()
                    .map_err(|_| anyhow::anyhow!("native bootstrap stream poisoned"))?;
                slot.generation = slot
                    .generation
                    .checked_add(1)
                    .ok_or_else(|| anyhow::anyhow!("native bootstrap generation exhausted"))?;
                let response = encode(&SnapshotStarted {
                    generation: slot.generation,
                    watermarks,
                })?;
                slot.stream = Some(changes);
                self.started.store(true, Ordering::Release);
                Ok(response)
            }
            NEXT => {
                let next = poll_fn(|cx| {
                    let mut stream = match self.stream.lock() {
                        Ok(stream) => stream,
                        Err(_) => {
                            return Poll::Ready(Err(anyhow::anyhow!(
                                "native bootstrap stream poisoned"
                            )))
                        }
                    };
                    if generation == 0 || generation != stream.generation {
                        return Poll::Ready(Err(Failure::closed().into()));
                    }
                    let Some(value) = stream.stream.as_mut() else {
                        return Poll::Ready(Err(anyhow::anyhow!(
                            "no active native bootstrap stream"
                        )));
                    };
                    match value.as_mut().poll_next(cx) {
                        Poll::Pending => {
                            stream.waker = Some(cx.waker().clone());
                            Poll::Pending
                        }
                        Poll::Ready(Some(value)) => Poll::Ready(value.map(Some)),
                        Poll::Ready(None) => {
                            stream.stream.take();
                            stream.waker = None;
                            self.ended.store(true, Ordering::Release);
                            Poll::Ready(Ok(None))
                        }
                    }
                })
                .await?;
                let encoded = next
                    .map(|envelope| self.codec.encode(&envelope).map(serde_bytes::ByteBuf::from))
                    .transpose()?;
                wire::encode(&encoded)
            }
            COMPLETE => {
                anyhow::ensure!(
                    self.ended.load(Ordering::Acquire),
                    "bootstrap stream has not completed"
                );
                let (watermarks, state) = match &self.provider {
                    NativeBootstrapProvider::Quiescent(_) => (Vec::new(), None),
                    NativeBootstrapProvider::Durable(provider) => (
                        provider
                            .complete_snapshot()
                            .await?
                            .into_iter()
                            .map(Watermark::from)
                            .collect::<Vec<_>>(),
                        provider.completion_state()?,
                    ),
                };
                validate_watermarks(&watermarks)?;
                if let Some(state) = &state {
                    validate_state(state)?;
                }
                let response = encode(&Completed {
                    watermarks,
                    state: state.map(|bytes| bytes.to_vec().into()),
                })?;
                self.ended.store(false, Ordering::Release);
                Ok(response)
            }
            STOP => {
                self.cancel(0)?;
                self.provider.stop().await?;
                self.prepared.store(false, Ordering::Release);
                self.reprepare_allowed.store(false, Ordering::Release);
                self.started.store(false, Ordering::Release);
                self.ended.store(false, Ordering::Release);
                self.cleanup_required.store(false, Ordering::Release);
                encode(&())
            }
            _ => anyhow::bail!("unsupported native bootstrap operation"),
        }
    }
}

struct OperationGuard {
    instance: Arc<Instance>,
    completed: bool,
}
impl Drop for OperationGuard {
    fn drop(&mut self) {
        struct Reset<'a>(&'a AtomicBool);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::Release);
            }
        }
        let _reset = Reset(&self.instance.busy);
        if !self.completed {
            // Poisoning prevents reuse even if dropping an ill-behaved stream panics.
            self.instance
                .cleanup_required
                .store(true, Ordering::Release);
            if self.instance.cancel(0).is_err() {
                self.instance.damaged.store(true, Ordering::Release);
            }
        }
    }
}

pub(crate) fn export_provider(
    provider: NativeBootstrapProvider,
    codec: Arc<BinaryEnvelopeCodec>,
    reader: Option<SourceProgressView>,
) -> BootstrapHandle {
    BootstrapHandle {
        state: Box::into_raw(Box::new(Arc::new(Instance {
            provider,
            codec,
            stream: Mutex::new(SnapshotSlot {
                generation: 0,
                stream: None,
                waker: None,
            }),
            busy: AtomicBool::new(false),
            cleanup_required: AtomicBool::new(false),
            prepared: AtomicBool::new(false),
            reprepare_allowed: AtomicBool::new(false),
            started: AtomicBool::new(false),
            ended: AtomicBool::new(false),
            damaged: AtomicBool::new(false),
            reader,
        })))
        .cast(),
        vtable: &VTABLE,
    }
}
unsafe fn instance<'a>(state: *mut c_void) -> Result<&'a Arc<Instance>, Failure> {
    if state.is_null() {
        return Err(Failure::protocol("null native bootstrap handle"));
    }
    Ok(unsafe { &*state.cast::<Arc<Instance>>() })
}
static VTABLE: BootstrapVTable = BootstrapVTable {
    header: abi::Header::new::<BootstrapVTable>(),
    release: Some(release),
    begin: Some(begin),
    cancel_snapshot: Some(cancel),
};
unsafe extern "C" fn release(state: *mut c_void) {
    transport::drop_boundary(|| unsafe { drop(Box::from_raw(state.cast::<Arc<Instance>>())) });
}
unsafe extern "C" fn cancel(state: *mut c_void, generation: u64) -> abi::Status {
    transport::status_boundary(|| {
        unsafe { instance(state)? }
            .cancel(generation)
            .map_err(Failure::from)
    })
}
unsafe extern "C" fn begin(
    raw: *mut c_void,
    code: u32,
    generation: u64,
    state: *const BootstrapStateV1,
    out: *mut abi::OperationHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if out.is_null()
            || !(PREPARE..=STOP).contains(&code)
            || (!state.is_null() && code != PREPARE && code != SNAPSHOT)
            || (code != NEXT && generation != 0)
        {
            return Err(Failure::protocol("invalid bootstrap operation arguments"));
        }

        let state = if state.is_null() {
            None
        } else {
            Some(unsafe { RemoteState::new(state) }.map_err(Failure::from)?)
        };
        let instance = unsafe { instance(raw)? }.clone();
        if code == NEXT {
            let slot = instance
                .stream
                .lock()
                .map_err(|_| Failure::failed("native bootstrap stream poisoned"))?;
            if generation == 0 || generation != slot.generation || slot.stream.is_none() {
                return Err(Failure::closed());
            }
        }
        instance
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                Failure::new(
                    abi::status::BUSY,
                    "native bootstrap operation already active",
                )
            })?;
        let mut guard = OperationGuard {
            instance,
            completed: false,
        };
        let runtime = crate::export::io_runtime()?;
        let operation = transport::export_operation(
            async move {
                let result = guard
                    .instance
                    .call(code, generation, state)
                    .await
                    .map_err(Failure::from);
                let result = result.and_then(|bytes| {
                    if code != STOP && guard.instance.cleanup_required.load(Ordering::Acquire) {
                        Err(Failure::cancelled())
                    } else {
                        Ok(bytes)
                    }
                });
                guard.completed = result.is_ok();
                result
            },
            Some(runtime),
        );
        unsafe { transport::write_out(out, operation) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RetryPreparation {
        initial: BootstrapPreparation,
        calls: std::sync::atomic::AtomicUsize,
    }
    #[async_trait]
    impl DurableBootstrapProvider for RetryPreparation {
        fn validate_query_scope(&self, _: &str, _: &str, _: &ComponentId) -> anyhow::Result<()> {
            Ok(())
        }
        fn recovery_reader(&self) -> Option<SourceProgressView> {
            None
        }
        async fn prepare(
            &self,
            _: Option<&dyn BootstrapState>,
        ) -> anyhow::Result<BootstrapPreparation> {
            Ok(if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                self.initial
            } else {
                BootstrapPreparation::Ready
            })
        }
        fn has_pending_snapshot(&self) -> anyhow::Result<bool> {
            Ok(false)
        }
        async fn snapshot(
            &self,
            _: Option<&dyn BootstrapState>,
        ) -> anyhow::Result<ComputationBootstrapSnapshot> {
            Ok(ComputationBootstrapSnapshot {
                changes: Box::pin(futures_util::stream::empty()),
                watermarks: Vec::new(),
            })
        }
        async fn complete_snapshot(&self) -> anyhow::Result<Vec<BootstrapWatermark>> {
            Ok(Vec::new())
        }
        fn completion_state(&self) -> anyhow::Result<Option<Bytes>> {
            Ok(None)
        }
        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// A quiescent provider that (wrongly) returns a watermark.
    struct LeakingWatermark;
    #[async_trait]
    impl QuiescentBootstrapProvider for LeakingWatermark {
        fn validate_query_scope(&self, _: &str, _: &str, _: &ComponentId) -> anyhow::Result<()> {
            Ok(())
        }
        async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
            Ok(ComputationBootstrapSnapshot {
                changes: Box::pin(futures_util::stream::empty()),
                watermarks: vec![BootstrapWatermark {
                    stream: StreamId::try_new("leak/out")?,
                    source_id: None,
                    sequence: 1,
                    position: None,
                    producer: None,
                }],
            })
        }
        async fn stop(&self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn quiescent_provider_is_always_ready_and_cannot_hand_over() -> anyhow::Result<()> {
        let provider = export_provider(
            NativeBootstrapProvider::Quiescent(Arc::new(LeakingWatermark)),
            Arc::new(wire::codec(&[
                drasi_computation_contracts::GraphChangeCodec::schema(),
            ])?),
            None,
        );
        let prepared: Prepared = invoke(&provider, PREPARE).await?;
        assert_eq!(
            BootstrapPreparation::from(prepared.preparation),
            BootstrapPreparation::Ready
        );
        assert!(!prepared.pending);
        let error = invoke::<SnapshotStarted>(&provider, SNAPSHOT)
            .await
            .err()
            .expect("watermarks are refused")
            .to_string();
        assert!(error.contains("quiescent"), "{error}");
        invoke::<()>(&provider, STOP).await?;
        unsafe { (*provider.vtable).release.expect("fixture release")(provider.state) };
        Ok(())
    }

    #[test]
    fn description_must_match_declared_pairing_and_completion() -> anyhow::Result<()> {
        let protocol = BootstrapProtocol::new("drasi.test/coordinated", 1);
        let factory = PairedBootstrapFactory::new(
            ImplementationIdentity::try_new("drasi.test/paired", "1")?,
            protocol.clone(),
        );
        let metadata = factory.metadata();
        let description = factory.describe(&serde_json::json!({}))?;
        metadata.validate_description(&description)?;
        assert!(
            factory
                .describe(&serde_json::json!({"source": "x"}))
                .is_err(),
            "a paired provider takes no source configuration"
        );
        let mut unpaired = description.clone();
        unpaired.pairing = None;
        assert!(metadata.validate_description(&unpaired).is_err());
        let mut quiescent = description.clone();
        quiescent.completion = BootstrapCompletionKind::Quiescent;
        assert!(metadata.validate_description(&quiescent).is_err());
        let mut own = description;
        own.namespace = ElementNamespace::Own;
        assert!(metadata.validate_description(&own).is_err());
        Ok(())
    }
    async fn invoke<T: serde::de::DeserializeOwned>(
        provider: &BootstrapHandle,
        code: u32,
    ) -> anyhow::Result<T> {
        let mut operation = abi::OperationHandle::null();
        let operation = unsafe {
            transport::take_status((*provider.vtable).begin.expect("fixture callback")(
                provider.state,
                code,
                0,
                std::ptr::null(),
                &mut operation,
            ))?;
            transport::OperationFuture::new(operation)?
        };
        decode(&operation.await?)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn bootstrap_non_ready_reprepare_is_allowed_only_before_ready_or_snapshot(
    ) -> anyhow::Result<()> {
        for initial in [
            BootstrapPreparation::RefreshVolatile,
            BootstrapPreparation::ResetRequired,
        ] {
            for snapshot_first in [false, true] {
                let provider = export_provider(
                    NativeBootstrapProvider::Durable(Arc::new(RetryPreparation {
                        initial,
                        calls: std::sync::atomic::AtomicUsize::new(0),
                    })),
                    Arc::new(wire::codec(&[
                        drasi_computation_contracts::GraphChangeCodec::schema(),
                    ])?),
                    None,
                );
                let first: Prepared = invoke(&provider, PREPARE).await?;
                assert_eq!(BootstrapPreparation::from(first.preparation), initial);
                if snapshot_first {
                    let _: SnapshotStarted = invoke(&provider, SNAPSHOT).await?;
                } else {
                    let second: Prepared = invoke(&provider, PREPARE).await?;
                    assert_eq!(
                        BootstrapPreparation::from(second.preparation),
                        BootstrapPreparation::Ready
                    );
                }
                assert!(invoke::<Prepared>(&provider, PREPARE).await.is_err());
                assert!(invoke::<SnapshotStarted>(&provider, SNAPSHOT)
                    .await
                    .is_err());
                invoke::<()>(&provider, STOP).await?;
                let prepared: Prepared = invoke(&provider, PREPARE).await?;
                assert_eq!(
                    BootstrapPreparation::from(prepared.preparation),
                    BootstrapPreparation::Ready
                );
                invoke::<()>(&provider, STOP).await?;
                unsafe { (*provider.vtable).release.expect("fixture release")(provider.state) };
            }
        }
        Ok(())
    }

    #[test]
    fn output_watermark_keeps_its_epoch_and_rejects_mixed_source_positions() -> anyhow::Result<()> {
        use drasi_computation_contracts::{GraphProducerIdentity, OutputCursor};
        let producer = GraphProducerIdentity::volatile(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new("0b751a8f-0746-486a-b991-73ba3c255ca1")?,
            StreamId::try_new("producer/out")?,
        )?;
        let cursor = OutputCursor::try_new(producer.clone(), 0)?;
        let wire: Watermark = BootstrapWatermark::from_output(&cursor).into();
        validate_watermarks(std::slice::from_ref(&wire))?;
        let decoded: Watermark = decode(&encode(&wire)?)?;
        let mut watermark: BootstrapWatermark = decoded.into();
        watermark.validate()?;
        assert_eq!(watermark.producer.as_ref(), Some(&producer));
        assert_eq!(watermark.sequence, 0);
        watermark.source_id = Some(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000029a")
                .expect("fixture UUID"),
        );
        assert!(watermark.validate().is_err());
        let mixed: Watermark = watermark.into();
        assert!(validate_watermarks(&[mixed]).is_err());
        Ok(())
    }

    #[test]
    fn bootstrap_wire_bounds_and_distinct_checkpoint_namespaces() -> anyhow::Result<()> {
        assert!(validate_state(&[]).is_err());
        validate_state(&vec![255; MAX_STATE_BYTES])?;
        assert!(validate_state(&vec![0; MAX_STATE_BYTES + 1]).is_err());
        let make = |source_id| Watermark {
            producer: None,
            stream: StreamId::try_new("same").unwrap(),
            source_id,
            sequence: u64::MAX,
            position: Some(vec![0, 255].into()),
        };
        let values = vec![
            make(None),
            make(Some(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000339")
                    .expect("fixture UUID"),
            )),
        ];
        validate_watermarks(&values)?;
        let values: Vec<Watermark> = decode(&encode(&values)?)?;
        assert_eq!(values[1].sequence, u64::MAX);
        assert_eq!(
            values[0].position.as_ref().map(|bytes| bytes.as_slice()),
            Some([0, 255].as_slice())
        );
        assert!(validate_watermarks(&[make(None), make(None)]).is_err());
        assert!(encode(&vec![255u8; MAX_CONTROL_BYTES]).is_err());
        assert!(decode::<Prepared>(&[0xc1]).is_err());
        Ok(())
    }
}
