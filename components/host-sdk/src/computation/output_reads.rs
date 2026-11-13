// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Host consumer of the per-output read extension. Each declared provider is a
//! real shared `SnapshotProvider`/`ReplayProvider` over one producer-owned
//! handle. Reads never use the component's data-operation slot, so they are
//! independent of pending `next`/`transform` work. A provider keeps no reference
//! to the component; a fence ends reads on stop, restart and release.

use super::proxy::{native_operation_cause, RemoteComponent};
mod recovery;
mod transfer;
use anyhow::Context as _;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_plugin_abi::{self as abi, output_reads as ext};
use drasi_computation_plugin_sdk::{
    metadata::FactoryMetadata,
    output_reads::{Outcome, WireCursor, WireQueryHead},
    transport::{
        checked_table, take_status, Failure, OperationCancellation, OperationFuture, ReceivedBytes,
    },
};
use drasi_lib::computation::v1::*;
use serde::Deserialize;
use std::result::Result;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex, RwLock,
};
use tokio::sync::watch;
#[cfg(test)]
use {
    drasi_computation_plugin_sdk::output_reads::{WireQueryView, WireReplay, WireSnapshot},
    std::collections::HashSet,
};

/// Ends reads that straddle a stop/restart (`advance`) or the proxy's release
/// (`retire`), whatever the native library does.
pub(super) struct Fence {
    generation: AtomicU64,
    retired: AtomicBool,
    updating: AtomicBool,
    changed: watch::Sender<()>,
    recovery_ready: AtomicBool,
    recovery_changed: watch::Sender<()>,
    operations: Mutex<Vec<OperationCancellation>>,
}

impl Fence {
    pub(super) fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            retired: AtomicBool::new(false),
            updating: AtomicBool::new(false),
            changed: watch::channel(()).0,
            recovery_ready: AtomicBool::new(false),
            recovery_changed: watch::channel(()).0,
            operations: Mutex::new(Vec::new()),
        }
    }

    pub(super) fn advance(&self) {
        self.cancel_operations();
        self.recovery_ready.store(false, Ordering::Release);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.changed.send_modify(|_| ());
        self.recovery_changed.send_modify(|_| ());
    }

    pub(super) fn retire(&self) {
        self.cancel_operations();
        self.retired.store(true, Ordering::Release);
        self.changed.send_modify(|_| ());
        self.recovery_changed.send_modify(|_| ());
    }

    pub(super) fn enter(&self) -> Result<(u64, watch::Receiver<()>), OutputReadError> {
        let changed = self.changed.subscribe();
        if self.retired.load(Ordering::Acquire) {
            return Err(OutputReadError::Retired);
        }
        if self.updating.load(Ordering::Acquire) {
            return Err(OutputReadError::NotReady(
                "native output update is not complete".into(),
            ));
        }
        Ok((self.generation.load(Ordering::Acquire), changed))
    }

    pub(super) fn track(&self, operation: &OperationFuture) {
        let mut operations = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        operations.retain(OperationCancellation::is_active);
        operations.push(operation.cancellation());
    }

    fn cancel_operations(&self) {
        let operations = std::mem::take(
            &mut *self
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for operation in operations {
            let _ = operation.cancel();
        }
    }

    pub(super) fn is_current(&self, generation: u64) -> bool {
        !self.retired.load(Ordering::Acquire)
            && !self.updating.load(Ordering::Acquire)
            && self.generation.load(Ordering::Acquire) == generation
    }
}

/// One producer-owned provider handle, released exactly once when dropped.
struct ReadHandle(ext::OutputReadHandle);
// The ABI permits concurrent bounded requests and release on any thread.
unsafe impl Send for ReadHandle {}
unsafe impl Sync for ReadHandle {}

impl ReadHandle {
    unsafe fn new(raw: ext::OutputReadHandle) -> Result<Self, Failure> {
        let table = unsafe { checked_table(raw.vtable)? };
        if raw.state.is_null() || table.release.is_none() {
            return Err(Failure::protocol("incomplete native output read handle"));
        }
        let handle = Self(raw);
        // Dropping an incomplete handle releases it.
        if table.request.is_none() {
            return Err(Failure::protocol("incomplete native output read handle"));
        }
        Ok(handle)
    }

    fn begin(&self, code: u32, input: &[u8]) -> Result<OperationFuture, Failure> {
        let table = unsafe { &*self.0.vtable };
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(table.request.expect("validated")(
                self.0.state,
                code,
                abi::BorrowedBytes::new(input),
                &mut operation,
            ))?;
            OperationFuture::new(operation)
        }
    }

    fn transfer(&self, code: u32, input: &[u8]) -> Result<Self, Failure> {
        let table = unsafe { &*self.0.vtable };
        let begin = table
            .transfer
            .ok_or_else(|| Failure::protocol("native output provider omitted bounded transfers"))?;
        let mut handle = ext::OutputReadHandle::null();
        unsafe {
            take_status(begin(
                self.0.state,
                code,
                abi::BorrowedBytes::new(input),
                &mut handle,
            ))?;
            Self::new(handle)
        }
    }
}

impl Drop for ReadHandle {
    fn drop(&mut self) {
        let table = unsafe { &*self.0.vtable };
        unsafe { table.release.expect("validated")(self.0.state) };
    }
}

pub(super) fn failure_error(failure: Failure) -> OutputReadError {
    match failure.code {
        abi::status::CLOSED => OutputReadError::Retired,
        abi::status::PROTOCOL => invalid(format!(
            "native output read protocol error: {}",
            failure.message
        )),
        ext::status::NOT_ACTIVE | abi::status::BUSY => OutputReadError::NotReady(failure.message),
        _ => OutputReadError::Failed(native_operation_cause(failure)),
    }
}

pub(super) fn invalid(message: impl std::fmt::Display) -> OutputReadError {
    OutputReadError::invalid(message.to_string())
}

/// What every provider of one output shares: its identity, declared schema,
/// codec and retirement fence, plus the checks of everything a producer sends.
pub(super) struct Context {
    pub(super) endpoint: OutputEndpoint,
    pub(super) capability: OutputCapability,
    pub(super) schema: Arc<Schema>,
    pub(super) codec: Arc<BinaryEnvelopeCodec>,
    pub(super) fence: Arc<Fence>,
}

struct Reader {
    handle: ReadHandle,
    context: Context,
}

impl std::ops::Deref for Reader {
    type Target = Context;

    fn deref(&self) -> &Context {
        &self.context
    }
}

impl Reader {
    async fn read(&self, code: u32, input: &[u8]) -> Result<ReceivedBytes, OutputReadError> {
        let (generation, mut changed) = self.fence.enter()?;
        let operation = self.handle.begin(code, input).map_err(failure_error)?;
        self.fence.track(&operation);
        let bytes = tokio::select! {
            biased;
            _ = changed.changed() => return Err(OutputReadError::Retired),
            result = operation => result.map_err(failure_error)?,
        };
        if !self.fence.is_current(generation) {
            return Err(OutputReadError::Retired);
        }
        if bytes.len() > ext::MAX_READ_BYTES {
            return Err(invalid(
                "native output read reply exceeds its negotiated bound",
            ));
        }
        Ok(bytes)
    }
}

impl Context {
    pub(super) fn outcome<'a, T: Deserialize<'a>>(
        &self,
        bytes: &'a [u8],
    ) -> Result<T, OutputReadError> {
        decode_bounded_messagepack::<Outcome<T>>(bytes, ext::MAX_READ_BYTES)
            .map_err(|error| invalid(format!("malformed native output read reply: {error}")))?
            .into_result(&self.endpoint, self.capability)
    }

    fn check_cursor(&self, cursor: &OutputCursor) -> Result<(), OutputReadError> {
        if cursor.producer().component_id() == &self.endpoint.component {
            Ok(())
        } else {
            Err(invalid(format!(
                "native output {} reported another component's position",
                self.endpoint
            )))
        }
    }

    #[cfg(test)]
    pub(super) fn snapshot_from(&self, bytes: &[u8]) -> Result<OutputSnapshot, OutputReadError> {
        let wire: WireSnapshot<'_> = self.outcome(bytes)?;
        self.snapshot_wire(wire)
    }

    #[cfg(test)]
    fn snapshot_wire(&self, wire: WireSnapshot<'_>) -> Result<OutputSnapshot, OutputReadError> {
        if &wire.schema != self.schema.descriptor() {
            return Err(invalid(
                "snapshot schema differs from the declared output schema",
            ));
        }
        if wire.records.len() > ext::MAX_SNAPSHOT_RECORDS {
            return Err(invalid("snapshot exceeds its negotiated record bound"));
        }
        let cursor = wire.cursor.into_cursor()?;
        self.check_cursor(&cursor)?;
        let mut seen = HashSet::with_capacity(wire.records.len());
        let mut records = Vec::with_capacity(wire.records.len());
        for record in &wire.records {
            let value = self.record(record)?;
            if !seen.insert(value.identity().clone()) {
                return Err(invalid("snapshot contains a duplicate record identity"));
            }
            records.push((value.identity().clone(), value));
        }
        OutputSnapshot::try_new(
            cursor,
            wire.dataset,
            wire.schema,
            records.into_iter().collect(),
        )
    }

    fn record(
        &self,
        wire: &drasi_computation_plugin_sdk::output_reads::WireRecord<'_>,
    ) -> Result<Record, OutputReadError> {
        if wire.image != 0 {
            return Err(invalid(
                "snapshot contains a record that is not a full image",
            ));
        }
        let identity = RecordId::try_new(&*wire.namespace, Bytes::copy_from_slice(&wire.identity))
            .map_err(invalid)?;
        Record::try_new(
            &self.schema,
            identity,
            RecordImage::Full,
            Bytes::copy_from_slice(&wire.payload),
        )
        .map_err(invalid)
    }

    fn envelope(&self, bytes: &[u8]) -> Result<ChangeEnvelope, OutputReadError> {
        let envelope = self.codec.decode(bytes).map_err(invalid)?;
        if envelope.changes().schema() != self.schema.descriptor() {
            return Err(invalid(
                "replayed envelope has another schema than the output",
            ));
        }
        Ok(envelope)
    }

    fn head_from(&self, bytes: &[u8]) -> Result<OutputCursor, OutputReadError> {
        let wire: WireCursor = self.outcome(bytes)?;
        let head = wire.into_cursor()?;
        self.check_cursor(&head)?;
        Ok(head)
    }

    #[cfg(test)]
    fn replay_from(
        &self,
        bytes: &[u8],
        after: &OutputCursor,
    ) -> Result<OutputReplay, OutputReadError> {
        let wire: WireReplay<'_> = self.replay_outcome(bytes, after)?;
        self.replay_wire(wire, after)
    }

    fn replay_outcome<'a, T: Deserialize<'a>>(
        &self,
        bytes: &'a [u8],
        after: &OutputCursor,
    ) -> Result<T, OutputReadError> {
        self.outcome(bytes).map_err(|error| match error {
            OutputReadError::Gap { requested, .. } if requested != after.sequence() => {
                invalid("native replay reported a gap for another cursor")
            }
            OutputReadError::EpochChanged { requested, current }
                if &*requested != after.producer()
                    || current.validate().is_err()
                    || current.component_id() != &self.endpoint.component =>
            {
                invalid("native replay reported an inconsistent epoch change")
            }
            error => error,
        })
    }

    #[cfg(test)]
    fn replay_wire(
        &self,
        wire: WireReplay<'_>,
        after: &OutputCursor,
    ) -> Result<OutputReplay, OutputReadError> {
        if wire.envelopes.len() > ext::MAX_REPLAY_ENVELOPES {
            return Err(invalid("replay exceeds its negotiated envelope bound"));
        }
        let reported_after = wire.after.into_cursor()?;
        let head = wire.head.into_cursor()?;
        self.check_cursor(&head)?;
        if &reported_after != after {
            return Err(invalid("replay answers a different cursor than requested"));
        }
        let mut envelopes = Vec::with_capacity(wire.envelopes.len());
        for frame in &wire.envelopes {
            envelopes.push(self.envelope(&frame.envelope)?);
        }
        OutputReplay::try_new(reported_after, head, envelopes)
    }
}

struct NativeSnapshotProvider(Reader);

#[async_trait]
impl SnapshotProvider for NativeSnapshotProvider {
    async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
        match self.0.transfer(ext::operation::SNAPSHOT, &[], None).await? {
            transfer::Read::Snapshot(snapshot) => Ok(snapshot),
            _ => Err(invalid(
                "native snapshot transfer returned another read kind",
            )),
        }
    }
}

struct NativeReplayProvider(Reader);

#[async_trait]
impl ReplayProvider for NativeReplayProvider {
    async fn committed_head(&self) -> Result<OutputCursor, OutputReadError> {
        let bytes = self.0.read(ext::operation::COMMITTED_HEAD, &[]).await?;
        self.0.head_from(&bytes)
    }

    async fn replay_after(&self, after: &OutputCursor) -> Result<OutputReplay, OutputReadError> {
        let request = encode_bounded_messagepack(&WireCursor::new(after), ext::MAX_REQUEST_BYTES)
            .map_err(invalid)?;
        match self
            .0
            .transfer(ext::operation::REPLAY_AFTER, &request, Some(after))
            .await?
        {
            transfer::Read::Replay(replay) => Ok(replay),
            _ => Err(invalid("native replay transfer returned another read kind")),
        }
    }
}

struct NativeQueryProvider {
    reader: Reader,
    description: RwLock<drasi_computation_contracts::QueryDescription>,
    observations: bool,
    recovery_views: bool,
}

impl NativeQueryProvider {
    fn current_description(
        &self,
    ) -> Result<drasi_computation_contracts::QueryDescription, OutputReadError> {
        self.description
            .read()
            .map(|description| description.clone())
            .map_err(|_| invalid("native query description poisoned"))
    }

    fn validate_cursor(&self, cursor: &OutputCursor) -> Result<(), OutputReadError> {
        self.reader.check_cursor(cursor)?;
        let description = self.current_description()?;
        if cursor.producer().stream() != &description.output_stream
            || cursor.producer().graph_id() != description.graph_id
            || cursor.producer().persistent() == description.volatile
        {
            return Err(invalid(
                "query output disagrees with its construction description",
            ));
        }
        Ok(())
    }

    fn validate_replay(
        &self,
        replay: &OutputReplay,
        generation: u64,
    ) -> Result<(), OutputReadError> {
        self.validate_cursor(replay.head())?;
        let producer = replay.head().producer();
        let description = self.current_description()?;
        for envelope in replay.envelopes() {
            let identity = QueryRecoveryIdentity::from_query_output(envelope).map_err(invalid)?;
            let progress = GraphProducerProgress::from_envelope(envelope)
                .map_err(invalid)?
                .ok_or_else(|| invalid("query replay has no producer progress"))?;
            if identity.configuration_hash() != description.configuration_hash
                || identity.graph_id() != producer.graph_id()
                || identity.construction_scope() != producer.construction_scope()
                || identity.query_id() != producer.component_id()
                || identity.incarnation()
                    != (!producer.persistent()).then_some(producer.incarnation())
                || QueryChangeCodec::query_generation(envelope).map_err(invalid)? != generation
                || QueryChangeCodec::query_sequence(envelope).map_err(invalid)?
                    != progress.sequence()
                || envelope.system().sequence() != progress.sequence()
            {
                return Err(invalid(
                    "query replay has inconsistent identity, configuration or generation",
                ));
            }
        }
        Ok(())
    }

    #[cfg(test)]
    fn view_from(
        &self,
        bytes: &[u8],
    ) -> Result<(OutputCursor, drasi_computation_contracts::QueryOutputView), OutputReadError> {
        let wire: WireQueryView<'_> = self.reader.outcome(bytes)?;
        let snapshot = self.reader.snapshot_wire(wire.snapshot)?;
        let after = wire.retained.after.clone().into_cursor()?;
        let replay = self.reader.replay_wire(wire.retained, &after)?;
        self.view(snapshot, replay, wire.generation, wire.config_hash)
    }

    fn view(
        &self,
        snapshot: OutputSnapshot,
        replay: OutputReplay,
        generation: u64,
        config_hash: u64,
    ) -> Result<(OutputCursor, drasi_computation_contracts::QueryOutputView), OutputReadError> {
        self.validate_cursor(snapshot.cursor())?;
        let producer = snapshot.cursor().producer();
        let description = self.current_description()?;
        if config_hash != description.configuration_hash {
            return Err(invalid("query view has inconsistent configuration"));
        }
        if replay.head() != snapshot.cursor() {
            return Err(invalid(
                "query view snapshot and history have different heads",
            ));
        }
        self.validate_replay(&replay, generation)?;
        let view = drasi_computation_contracts::QueryOutputView {
            snapshot: QueryResultSnapshot {
                projector: ResultNodeProjector::try_new(
                    snapshot.dataset(),
                    description.execution.result.label.as_deref(),
                )
                .map_err(invalid)?,
                as_of_sequence: snapshot.cursor().sequence(),
                generation,
                rows: snapshot.records().clone(),
            },
            retained: replay.into_envelopes(),
            config_hash,
            incarnation: (!producer.persistent()).then_some(producer.incarnation()),
        };
        Ok((snapshot.cursor().clone(), view))
    }
}

#[async_trait]
impl drasi_computation_contracts::QueryViewProvider for NativeQueryProvider {
    fn description(&self) -> drasi_computation_contracts::QueryDescription {
        self.description
            .read()
            .expect("native query description poisoned")
            .clone()
    }

    fn supports_observations(&self) -> bool {
        self.observations
    }

    fn supports_recovery_views(&self) -> bool {
        self.recovery_views
    }

    fn recovery_reader(
        self: Arc<Self>,
    ) -> Option<Arc<dyn drasi_computation_contracts::QueryRecoveryReader>> {
        self.recovery_views
            .then_some(self)
            .map(|reader| reader as Arc<dyn drasi_computation_contracts::QueryRecoveryReader>)
    }

    async fn query_recovery_observation(
        &self,
    ) -> Result<drasi_computation_contracts::QueryOutputObservation, OutputReadError> {
        self.require_recovery()?;
        self.read_observation(ext::operation::QUERY_RECOVERY_OBSERVATION)
            .await
    }

    async fn query_recovery_view(
        &self,
    ) -> Result<(OutputCursor, drasi_computation_contracts::QueryOutputView), OutputReadError> {
        self.require_recovery()?;
        self.read_view(ext::operation::QUERY_RECOVERY_VIEW).await
    }

    async fn query_observation(
        &self,
    ) -> Result<drasi_computation_contracts::QueryOutputObservation, OutputReadError> {
        if !self.observations {
            return Err(invalid("query observations were not negotiated"));
        }
        self.read_observation(ext::operation::QUERY_OBSERVATION)
            .await
    }

    async fn query_head(&self) -> Result<(OutputCursor, u64), OutputReadError> {
        let bytes = self.reader.read(ext::operation::QUERY_HEAD, &[]).await?;
        let wire: WireQueryHead = self.reader.outcome(&bytes)?;
        let cursor = wire.cursor.into_cursor()?;
        self.validate_cursor(&cursor)?;
        Ok((cursor, wire.generation))
    }

    async fn query_snapshot(&self) -> Result<(OutputSnapshot, u64), OutputReadError> {
        match self
            .reader
            .transfer(ext::operation::QUERY_SNAPSHOT, &[], None)
            .await?
        {
            transfer::Read::QuerySnapshot(snapshot, generation) => {
                self.validate_cursor(snapshot.cursor())?;
                Ok((snapshot, generation))
            }
            _ => Err(invalid(
                "native query snapshot transfer returned another read kind",
            )),
        }
    }

    async fn query_view(
        &self,
    ) -> Result<(OutputCursor, drasi_computation_contracts::QueryOutputView), OutputReadError> {
        self.read_view(ext::operation::QUERY_VIEW).await
    }

    async fn query_replay(
        &self,
        after: &OutputCursor,
    ) -> Result<(OutputReplay, u64), OutputReadError> {
        let request = encode_bounded_messagepack(&WireCursor::new(after), ext::MAX_REQUEST_BYTES)
            .map_err(invalid)?;
        match self
            .reader
            .transfer(ext::operation::QUERY_REPLAY, &request, Some(after))
            .await?
        {
            transfer::Read::QueryReplay(replay, generation) => {
                self.validate_replay(&replay, generation)?;
                Ok((replay, generation))
            }
            _ => Err(invalid(
                "native query replay transfer returned another read kind",
            )),
        }
    }
}

/// A proxy's captured providers and their retirement fence.
pub(super) struct NativeOutputReads {
    fence: Arc<Fence>,
    readers: OutputReaders,
    queries: std::collections::BTreeMap<PortId, Arc<NativeQueryProvider>>,
}

impl NativeOutputReads {
    /// Open exactly the providers factory metadata declares. A declared provider
    /// the constructed component does not serve fails construction.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn bind(
        extension: Option<ext::PluginOutputReadsV2>,
        follow: Option<abi::output_follow::PluginOutputFollowV2>,
        component: &Arc<RemoteComponent>,
        descriptor: &ComponentDescriptor,
        metadata: &FactoryMetadata,
        queries: &std::collections::BTreeMap<PortId, drasi_computation_contracts::QueryDescription>,
        schemas: &[Arc<Schema>],
        codec: &Arc<BinaryEnvelopeCodec>,
    ) -> anyhow::Result<Option<Self>> {
        let capabilities = &metadata.capabilities;
        if !capabilities.declares_output_reads() && !capabilities.declares_output_follows() {
            return Ok(None);
        }
        let fence = Arc::new(Fence::new());
        let context = |port: &PortId, kind: OutputCapability| {
            let declared = descriptor
                .ports()
                .iter()
                .find(|declared| {
                    declared.id() == port && declared.direction() == PortDirection::Output
                })
                .with_context(|| format!("native output reader names undeclared output {port}"))?;
            let schema = schemas
                .iter()
                .find(|schema| schema.descriptor() == declared.schema())
                .cloned()
                .with_context(|| format!("native output {port} uses an unregistered schema"))?;
            Ok::<_, anyhow::Error>(Context {
                endpoint: OutputEndpoint::new(descriptor.id().clone(), port.clone()),
                capability: kind,
                schema,
                codec: codec.clone(),
                fence: fence.clone(),
            })
        };
        let open = |port: &PortId, capability: u32, kind: OutputCapability| {
            let extension = extension.context("native output reads were not negotiated")?;
            let context = context(port, kind)?;
            let mut raw = ext::OutputReadHandle::null();
            unsafe {
                take_status(extension.open.expect("validated")(
                    component.state(),
                    abi::BorrowedBytes::new(port.as_str().as_bytes()),
                    capability,
                    &mut raw,
                ))
                .map_err(native_operation_cause)?;
            }
            let handle = unsafe { ReadHandle::new(raw) }.map_err(native_operation_cause)?;
            Ok::<_, anyhow::Error>(Reader { handle, context })
        };
        let mut readers = OutputReaders::new();
        for port in &capabilities.output_snapshots {
            let reader = open(port, ext::capability::SNAPSHOT, OutputCapability::Snapshot)?;
            readers = readers.with_snapshot(port.clone(), Arc::new(NativeSnapshotProvider(reader)));
        }
        for port in &capabilities.output_replays {
            let reader = open(port, ext::capability::REPLAY, OutputCapability::Replay)?;
            readers = readers.with_replay(port.clone(), Arc::new(NativeReplayProvider(reader)));
        }
        for port in &capabilities.output_follows {
            let extension = follow.context("native output follow was not negotiated")?;
            let provider = super::output_follow::NativeFollowProvider::open(
                extension,
                component,
                port,
                context(port, OutputCapability::SnapshotFollow)?,
            )?;
            readers = readers.with_snapshot_follow(port.clone(), Arc::new(provider));
        }
        let mut query_providers = std::collections::BTreeMap::new();
        for port in &capabilities.output_queries {
            let description = queries
                .get(port)
                .context("native query output omitted its construction description")?
                .clone();
            let reader = open(port, ext::capability::QUERY, OutputCapability::Snapshot)?;
            let provider = Arc::new(NativeQueryProvider {
                reader,
                description: RwLock::new(description),
                observations: capabilities.query_observations,
                recovery_views: capabilities.query_recovery_views,
            });
            readers = readers.with_query(port.clone(), provider.clone());
            query_providers.insert(port.clone(), provider);
        }
        readers.validate(descriptor)?;
        Ok(Some(Self {
            fence,
            readers,
            queries: query_providers,
        }))
    }

    pub(super) fn refresh_queries(
        &self,
        descriptions: &std::collections::BTreeMap<
            PortId,
            drasi_computation_contracts::QueryDescription,
        >,
        data: ComponentUpdateData,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.queries.keys().eq(descriptions.keys()),
            "native update changed its declared query readers"
        );
        let mut descriptions = self
            .queries
            .values()
            .zip(descriptions.values())
            .map(|(provider, proposed)| {
                let current = provider
                    .description
                    .write()
                    .map_err(|_| anyhow::anyhow!("native query description poisoned"))?;
                anyhow::ensure!(
                    current.graph_id == proposed.graph_id
                        && current.output_stream == proposed.output_stream
                        && current.volatile == proposed.volatile
                        && (data != ComponentUpdateData::Preserve
                            || current.configuration_hash == proposed.configuration_hash),
                    "native update changed the identity or preservation contract of its query output"
                );
                Ok((current, proposed))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        for (current, proposed) in &mut descriptions {
            **current = (*proposed).clone();
        }
        Ok(())
    }

    pub(super) fn readers(&self) -> OutputReaders {
        self.readers.clone()
    }

    pub(super) fn begin_update(&self) {
        self.fence.updating.store(true, Ordering::Release);
        self.fence.advance();
    }

    pub(super) fn finish_update(&self) {
        self.fence.generation.fetch_add(1, Ordering::AcqRel);
        self.fence.updating.store(false, Ordering::Release);
        self.fence.changed.send_modify(|_| ());
        self.published();
    }

    /// Stop/restart: reads begun before this call can no longer succeed.
    pub(super) fn advance(&self) {
        self.fence.advance();
    }

    pub(super) fn recovery_ready(&self) {
        self.fence.recovery_ready.store(true, Ordering::Release);
        self.published();
    }

    pub(super) fn published(&self) {
        self.fence.recovery_changed.send_modify(|_| ());
    }

    /// Proxy release: every current and future read reports `Retired`.
    pub(super) fn retire(&self) {
        self.fence.retire();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::{
        output_reads::{WireFrame, WireRecord},
        transport::export_operation,
        wire,
    };
    use std::{borrow::Cow, collections::BTreeMap, sync::atomic::AtomicUsize};

    #[test]
    fn native_update_fence_blocks_reads_until_metadata_is_refreshed() -> anyhow::Result<()> {
        let fence = Arc::new(Fence::new());
        let readers = NativeOutputReads {
            fence: fence.clone(),
            readers: OutputReaders::default(),
            queries: BTreeMap::new(),
        };
        let (before, _) = fence.enter()?;
        readers.begin_update();
        assert!(matches!(fence.enter(), Err(OutputReadError::NotReady(_))));
        assert!(!fence.is_current(before));
        readers.refresh_queries(&BTreeMap::new(), ComponentUpdateData::Preserve)?;
        readers.finish_update();
        let (after, _) = fence.enter()?;
        assert_ne!(before, after);
        assert!(fence.is_current(after));
        readers.retire();
        readers.finish_update();
        assert!(matches!(fence.enter(), Err(OutputReadError::Retired)));
        Ok(())
    }

    struct Scenario {
        reply: Vec<u8>,
        released: Arc<AtomicUsize>,
    }

    unsafe extern "C" fn fake_release(state: *mut std::ffi::c_void) {
        let scenario = unsafe { Box::from_raw(state.cast::<Scenario>()) };
        scenario.released.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C" fn fake_request(
        state: *mut std::ffi::c_void,
        _code: u32,
        _input: abi::BorrowedBytes,
        out: *mut abi::OperationHandle,
    ) -> abi::Status {
        let reply = unsafe { &*state.cast::<Scenario>() }.reply.clone();
        unsafe { std::ptr::write(out, export_operation(async move { Ok(reply) }, None)) };
        abi::Status::ok()
    }

    static VTABLE: ext::OutputReadVTable = ext::OutputReadVTable {
        header: abi::Header::new::<ext::OutputReadVTable>(),
        release: Some(fake_release),
        request: Some(fake_request),
        transfer: None,
    };

    pub(super) fn reader(
        reply: Vec<u8>,
        capability: OutputCapability,
    ) -> (Reader, Arc<AtomicUsize>) {
        let released = Arc::new(AtomicUsize::new(0));
        let state = Box::into_raw(Box::new(Scenario {
            reply,
            released: released.clone(),
        }));
        let handle = unsafe {
            ReadHandle::new(ext::OutputReadHandle {
                state: state.cast(),
                vtable: &VTABLE,
            })
        }
        .unwrap();
        let schema = GraphChangeCodec::schema();
        let codec = Arc::new(wire::codec(std::slice::from_ref(&schema)).unwrap());
        (
            Reader {
                handle,
                context: Context {
                    endpoint: OutputEndpoint::new(
                        ComponentId::try_new("0a1c171a-8016-56dc-957c-1ccd5c7e2cf3").unwrap(),
                        PortId::try_new("out").unwrap(),
                    ),
                    capability,
                    schema,
                    codec,
                    fence: Arc::new(Fence::new()),
                },
            },
            released,
        )
    }

    pub(super) fn producer(component: &str) -> GraphProducerIdentity {
        let owner = ComponentId::try_new("b74c84a1-457f-5f17-8e90-599086fc4c64").unwrap();
        let component_id = if component == "component" {
            ComponentId::try_new("0a1c171a-8016-56dc-957c-1ccd5c7e2cf3").unwrap()
        } else {
            ComponentId::derive_owned(&owner, component).unwrap()
        };
        GraphProducerIdentity::new(
            "scope".into(),
            "graph".into(),
            component_id,
            StreamId::try_new(format!("{component}/out")).unwrap(),
            true,
        )
        .unwrap()
    }

    fn query_provider() -> NativeQueryProvider {
        let (reader, _) = reader(Vec::new(), OutputCapability::Snapshot);
        NativeQueryProvider {
            reader,
            observations: false,
            recovery_views: false,
            description: RwLock::new(drasi_computation_contracts::QueryDescription {
                graph_id: "graph".into(),
                query: "MATCH (n) RETURN n".into(),
                language: ComputationQueryLanguage::Cypher,
                execution: QueryExecutionSettings::default(),
                output_stream: StreamId::try_new("component/out").unwrap(),
                outbox_capacity: std::num::NonZeroUsize::new(8).unwrap(),
                bootstrap: false,
                auto_reset: false,
                volatile: false,
                configuration_hash: 17,
            }),
        }
    }

    fn query_envelope(
        producer: &GraphProducerIdentity,
        scope: &str,
        hash: u64,
        generation: u64,
    ) -> ChangeEnvelope {
        let component = producer.component_id();
        let mut envelope = QueryChangeCodec::encode_evaluation(
            None,
            component,
            SystemMetadata::new(producer.stream().clone(), 1),
            &ResultNodeProjector::try_new("dataset", None).unwrap(),
            &[
                drasi_core::evaluation::context::QueryPartEvaluationContext::Adding {
                    after: BTreeMap::from([("value".into(), serde_json::json!(1).into())]),
                    row_signature: 1,
                },
            ],
            QueryOutputMetadata {
                query_id: component.clone(),
                input_origin: None,
                timestamp: "2026-01-01T00:00:00Z".parse().unwrap(),
                metadata: Default::default(),
                profiling: None,
            },
        )
        .unwrap()
        .unwrap();
        QueryRecoveryIdentity::try_new(producer.graph_id(), component.clone(), hash, None)
            .unwrap()
            .with_construction_scope(scope)
            .unwrap()
            .annotate(&mut envelope, component)
            .unwrap();
        QueryChangeCodec::set_generation(&mut envelope, component, generation).unwrap();
        GraphProducerProgress::annotate(&mut envelope, producer, 1).unwrap();
        envelope
    }

    #[test]
    fn query_replay_rejects_mismatched_metadata_and_transport_sequences() {
        let provider = query_provider();
        let owner = producer("component");
        let after = OutputCursor::try_new(owner.clone(), 0).unwrap();
        let replay = |envelope| {
            OutputReplay::try_new(after.clone(), after.at_sequence(1), vec![envelope]).unwrap()
        };
        let valid = query_envelope(&owner, "scope", 17, 3);
        assert!(provider.validate_replay(&replay(valid.clone()), 3).is_ok());
        for (name, envelope) in [
            ("configuration", query_envelope(&owner, "scope", 18, 3)),
            ("scope", query_envelope(&owner, "another-scope", 17, 3)),
            ("generation", query_envelope(&owner, "scope", 17, 4)),
            ("transport sequence", valid.reemit(77).unwrap()),
        ] {
            assert!(
                matches!(
                    provider.validate_replay(&replay(envelope), 3),
                    Err(OutputReadError::Invalid(_))
                ),
                "{name} was accepted"
            );
        }
    }

    #[test]
    fn query_view_rejects_incoherent_heads_and_configuration() {
        let provider = query_provider();
        let head = OutputCursor::try_new(producer("component"), 0).unwrap();
        let reply = |snapshot_head: &OutputCursor, replay_head: &OutputCursor, hash| {
            wire::encode(&Outcome::Ok(WireQueryView {
                snapshot: WireSnapshot {
                    cursor: WireCursor::new(snapshot_head),
                    dataset: "dataset".into(),
                    schema: GraphChangeCodec::schema().descriptor().clone(),
                    records: Vec::new(),
                },
                retained: WireReplay {
                    after: WireCursor::new(replay_head),
                    head: WireCursor::new(replay_head),
                    envelopes: Vec::new(),
                },
                generation: 3,
                config_hash: hash,
            }))
            .unwrap()
        };
        let (_, view) = provider.view_from(&reply(&head, &head, 17)).unwrap();
        assert_eq!(view.snapshot.generation, 3);
        assert_eq!(view.config_hash, 17);
        assert_eq!(view.incarnation, None);
        for bytes in [
            reply(&head, &head.at_sequence(1), 17),
            reply(&head, &head, 18),
            reply(
                &head,
                &OutputCursor::try_new(producer("component"), 0).unwrap(),
                17,
            ),
        ] {
            assert!(matches!(
                provider.view_from(&bytes),
                Err(OutputReadError::Invalid(_))
            ));
        }
    }

    fn envelope(identity: &GraphProducerIdentity, logical: u64) -> ChangeEnvelope {
        use drasi_core::models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        };
        let mut envelope = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("s", &logical.to_string()),
                        labels: Arc::from(Vec::<Arc<str>>::new()),
                        effective_from: 1,
                    },
                    properties: ElementPropertyMap::from(serde_json::json!({})),
                },
            },
            identity.stream().clone(),
            500 + logical,
            None,
        )
        .unwrap();
        GraphProducerProgress::annotate(&mut envelope, identity, logical).unwrap();
        envelope
    }

    fn element_record(name: &str) -> Record {
        use drasi_core::models::{ElementMetadata, ElementReference};
        GraphChangeCodec::element_record(&drasi_computation_contracts::ElementImage::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("s", name),
                labels: Arc::from(Vec::<Arc<str>>::new()),
                effective_from: 1,
            },
            properties: BTreeMap::new(),
        })
        .unwrap()
    }

    fn wire_record(record: &Record, image: u32) -> WireRecord<'_> {
        WireRecord {
            namespace: Cow::Borrowed(record.identity().namespace()),
            identity: Cow::Borrowed(&record.identity().value()[..]),
            image,
            payload: Cow::Borrowed(&record.payload()[..]),
        }
    }

    fn snapshot_bytes(
        owner: &GraphProducerIdentity,
        schema: SchemaDescriptor,
        records: Vec<WireRecord<'_>>,
    ) -> Vec<u8> {
        wire::encode(&Outcome::Ok(WireSnapshot {
            cursor: WireCursor::new(&OutputCursor::try_new(owner.clone(), 4).unwrap()),
            dataset: "dataset".into(),
            schema,
            records,
        }))
        .unwrap()
    }

    #[test]
    fn handle_is_released_exactly_once_even_after_rejected_replies() {
        let (reader, released) = reader(Vec::new(), OutputCapability::Snapshot);
        assert!(reader.snapshot_from(&[0xc1]).is_err());
        assert_eq!(released.load(Ordering::SeqCst), 0);
        drop(reader);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn oversized_and_malformed_producer_buffers_are_explicit_errors() {
        let (oversized, released) = reader(
            vec![0u8; ext::MAX_READ_BYTES + 1],
            OutputCapability::Snapshot,
        );
        assert!(matches!(
            oversized.read(ext::operation::SNAPSHOT, &[]).await,
            Err(OutputReadError::Invalid(_))
        ));
        drop(oversized);
        assert_eq!(released.load(Ordering::SeqCst), 1);

        let (garbage, _) = reader(vec![0xc1, 0xff, 0x00], OutputCapability::Snapshot);
        let bytes = garbage.read(ext::operation::SNAPSHOT, &[]).await.unwrap();
        assert!(matches!(
            garbage.snapshot_from(&bytes),
            Err(OutputReadError::Invalid(_))
        ));
        let owner = producer("component");
        let record = element_record("a");
        let valid = snapshot_bytes(
            &owner,
            GraphChangeCodec::schema().descriptor().clone(),
            vec![wire_record(&record, 0)],
        );
        assert!(garbage.snapshot_from(&valid).is_ok());
        for cut in [1, valid.len() / 2, valid.len() - 1] {
            assert!(
                matches!(
                    garbage.snapshot_from(&valid[..cut]),
                    Err(OutputReadError::Invalid(_))
                ),
                "a truncated snapshot must not be accepted"
            );
        }
        let mut trailing = valid.clone();
        trailing.push(0xc0);
        assert!(garbage.snapshot_from(&trailing).is_err());
    }

    #[test]
    fn snapshot_rejects_inconsistent_records_schema_and_owner() {
        let (reader, _) = reader(Vec::new(), OutputCapability::Snapshot);
        let owner = producer("component");
        let schema = GraphChangeCodec::schema().descriptor().clone();
        let (first, second) = (element_record("a"), element_record("b"));

        let complete = snapshot_bytes(
            &owner,
            schema.clone(),
            vec![wire_record(&first, 0), wire_record(&second, 0)],
        );
        let snapshot = reader.snapshot_from(&complete).unwrap();
        assert_eq!(snapshot.records().len(), 2);
        assert_eq!(snapshot.records().get(first.identity()), Some(&first));
        assert_eq!(snapshot.cursor().sequence(), 4);

        let duplicate = snapshot_bytes(
            &owner,
            schema.clone(),
            vec![wire_record(&first, 0), wire_record(&first, 0)],
        );
        let patch = snapshot_bytes(&owner, schema.clone(), vec![wire_record(&first, 1)]);
        let foreign_owner = snapshot_bytes(&producer("other"), schema.clone(), Vec::new());
        let other_schema = SchemaDescriptor::try_new(
            SchemaId::try_new("other.schema").unwrap(),
            SchemaVersion::try_new(1).unwrap(),
            "none",
            Bytes::from_static(b"definition"),
        )
        .unwrap();
        let foreign_schema = snapshot_bytes(&owner, other_schema, Vec::new());
        let too_many = snapshot_bytes(
            &owner,
            schema.clone(),
            (0..=ext::MAX_SNAPSHOT_RECORDS)
                .map(|_| wire_record(&first, 0))
                .collect(),
        );
        for (name, bytes) in [
            ("duplicate identity", duplicate),
            ("patch image", patch),
            ("another component's cursor", foreign_owner),
            ("another schema", foreign_schema),
            ("too many records", too_many),
        ] {
            assert!(
                matches!(
                    reader.snapshot_from(&bytes),
                    Err(OutputReadError::Invalid(_))
                ),
                "{name} was accepted"
            );
        }
    }

    fn replay_bytes(
        reader: &Reader,
        after: &OutputCursor,
        head: &OutputCursor,
        envelopes: &[ChangeEnvelope],
    ) -> Vec<u8> {
        wire::encode(&Outcome::Ok(WireReplay {
            after: WireCursor::new(after),
            head: WireCursor::new(head),
            envelopes: envelopes
                .iter()
                .map(|envelope| WireFrame {
                    envelope: Cow::Owned(reader.codec.encode(envelope).unwrap()),
                })
                .collect(),
        }))
        .unwrap()
    }

    #[test]
    fn replay_requires_logical_progress_and_the_complete_tail() {
        let (reader, _) = reader(Vec::new(), OutputCapability::Replay);
        let owner = producer("component");
        let after = OutputCursor::try_new(owner.clone(), 0).unwrap();
        let head = after.at_sequence(2);
        let (first, second) = (envelope(&owner, 1), envelope(&owner, 2));

        let complete = replay_bytes(&reader, &after, &head, &[first.clone(), second.clone()]);
        let replay = reader.replay_from(&complete, &after).unwrap();
        assert_eq!(replay.envelopes().len(), 2);
        // Transport sequences are independent of the committed cursor.
        assert_eq!(replay.envelopes()[0].system().sequence(), 501);

        let empty_at_head = replay_bytes(&reader, &head, &head, &[]);
        assert!(reader.replay_from(&empty_at_head, &head).is_ok());

        let missing_tail = replay_bytes(&reader, &after, &head, &[]);
        let partial_tail = replay_bytes(&reader, &after, &head, std::slice::from_ref(&first));
        let wrong_cursor = replay_bytes(&reader, &after, &head, &[first, second]);
        let foreign = envelope(&producer("other"), 1);
        let foreign_head = replay_bytes(&reader, &after, &head, &[foreign]);
        for (name, bytes, requested) in [
            ("missing tail", missing_tail, &after),
            ("partial tail", partial_tail, &after),
            (
                "another requested cursor",
                wrong_cursor,
                &after.at_sequence(1),
            ),
            ("foreign producer", foreign_head, &after),
        ] {
            assert!(
                matches!(
                    reader.replay_from(&bytes, requested),
                    Err(OutputReadError::Invalid(_))
                ),
                "{name} was accepted"
            );
        }
    }

    #[test]
    fn typed_replay_errors_must_describe_the_requested_cursor() {
        let (reader, _) = reader(Vec::new(), OutputCapability::Replay);
        let owner = producer("component");
        let after = OutputCursor::try_new(owner.clone(), 3).unwrap();
        let encode = |outcome: Outcome<()>| wire::encode(&outcome).unwrap();

        let gap = reader.replay_from(
            &encode(Outcome::Gap {
                requested: 3,
                oldest: Some(5),
                latest: 9,
            }),
            &after,
        );
        assert!(matches!(
            gap,
            Err(OutputReadError::Gap {
                requested: 3,
                oldest: Some(5),
                latest: 9
            })
        ));
        let other_gap = reader.replay_from(
            &encode(Outcome::Gap {
                requested: 1,
                oldest: None,
                latest: 9,
            }),
            &after,
        );
        assert!(matches!(other_gap, Err(OutputReadError::Invalid(_))));

        let changed = producer("component");
        let epoch = reader.replay_from(
            &encode(Outcome::EpochChanged {
                requested: owner.clone(),
                current: changed.clone(),
            }),
            &after,
        );
        let Err(OutputReadError::EpochChanged { requested, current }) = epoch else {
            panic!("epoch change must stay typed")
        };
        assert_eq!((*requested, *current), (owner.clone(), changed));
        let inconsistent = reader.replay_from(
            &encode(Outcome::EpochChanged {
                requested: producer("component"),
                current: owner,
            }),
            &after,
        );
        assert!(matches!(inconsistent, Err(OutputReadError::Invalid(_))));
    }
}
