// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Host consumer of the per-output read extension. Each declared provider is a
//! real shared `SnapshotProvider`/`ReplayProvider` over one producer-owned
//! handle. Reads never use the component's data-operation slot, so they are
//! independent of pending `next`/`transform` work. A provider keeps no reference
//! to the component; a fence ends reads on stop, restart and release.

use super::proxy::{native_operation_cause, RemoteComponent};
use anyhow::Context as _;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_plugin_abi::{self as abi, output_reads as ext};
use drasi_computation_plugin_sdk::{
    metadata::FactoryMetadata,
    output_reads::{Outcome, WireCursor, WireReplay, WireSnapshot},
    transport::{checked_table, take_status, Failure, OperationFuture, ReceivedBytes},
};
use drasi_lib::computation::v1::*;
use serde::Deserialize;
use std::result::Result;
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
};
use tokio::sync::watch;

/// Ends reads that straddle a stop/restart (`advance`) or the proxy's release
/// (`retire`), whatever the native library does.
pub(super) struct Fence {
    generation: AtomicU64,
    retired: AtomicBool,
    changed: watch::Sender<()>,
}

impl Fence {
    pub(super) fn new() -> Self {
        Self {
            generation: AtomicU64::new(0),
            retired: AtomicBool::new(false),
            changed: watch::channel(()).0,
        }
    }

    pub(super) fn advance(&self) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.changed.send_modify(|_| ());
    }

    pub(super) fn retire(&self) {
        self.retired.store(true, Ordering::Release);
        self.changed.send_modify(|_| ());
    }

    pub(super) fn enter(&self) -> Result<(u64, watch::Receiver<()>), OutputReadError> {
        let changed = self.changed.subscribe();
        if self.retired.load(Ordering::Acquire) {
            return Err(OutputReadError::Retired);
        }
        Ok((self.generation.load(Ordering::Acquire), changed))
    }

    pub(super) fn is_current(&self, generation: u64) -> bool {
        !self.retired.load(Ordering::Acquire)
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

    pub(super) fn snapshot_from(&self, bytes: &[u8]) -> Result<OutputSnapshot, OutputReadError> {
        let wire: WireSnapshot<'_> = self.outcome(bytes)?;
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
            if record.image != 0 {
                return Err(invalid(
                    "snapshot contains a record that is not a full image",
                ));
            }
            let identity =
                RecordId::try_new(&*record.namespace, Bytes::copy_from_slice(&record.identity))
                    .map_err(invalid)?;
            if !seen.insert(identity.clone()) {
                return Err(invalid("snapshot contains a duplicate record identity"));
            }
            let value = Record::try_new(
                &self.schema,
                identity.clone(),
                RecordImage::Full,
                Bytes::copy_from_slice(&record.payload),
            )
            .map_err(invalid)?;
            records.push((identity, value));
        }
        OutputSnapshot::try_new(
            cursor,
            wire.dataset,
            wire.schema,
            records.into_iter().collect(),
        )
    }

    fn head_from(&self, bytes: &[u8]) -> Result<OutputCursor, OutputReadError> {
        let wire: WireCursor = self.outcome(bytes)?;
        let head = wire.into_cursor()?;
        self.check_cursor(&head)?;
        Ok(head)
    }

    fn replay_from(
        &self,
        bytes: &[u8],
        after: &OutputCursor,
    ) -> Result<OutputReplay, OutputReadError> {
        let wire: WireReplay<'_> = self.outcome(bytes).map_err(|error| match error {
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
        })?;
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
            let envelope = self.codec.decode(&frame.envelope).map_err(invalid)?;
            if envelope.changes().schema() != self.schema.descriptor() {
                return Err(invalid(
                    "replayed envelope has another schema than the output",
                ));
            }
            envelopes.push(envelope);
        }
        OutputReplay::try_new(reported_after, head, envelopes)
    }
}

struct NativeSnapshotProvider(Reader);

#[async_trait]
impl SnapshotProvider for NativeSnapshotProvider {
    async fn snapshot(&self) -> Result<OutputSnapshot, OutputReadError> {
        let bytes = self.0.read(ext::operation::SNAPSHOT, &[]).await?;
        self.0.snapshot_from(&bytes)
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
        let bytes = self.0.read(ext::operation::REPLAY_AFTER, &request).await?;
        self.0.replay_from(&bytes, after)
    }
}

/// A proxy's captured providers and their retirement fence.
pub(super) struct NativeOutputReads {
    fence: Arc<Fence>,
    readers: OutputReaders,
}

impl NativeOutputReads {
    /// Open exactly the providers factory metadata declares. A declared provider
    /// the constructed component does not serve fails construction.
    pub(super) fn bind(
        extension: Option<ext::PluginOutputReadsV1>,
        follow: Option<abi::output_follow::PluginOutputFollowV1>,
        component: &Arc<RemoteComponent>,
        descriptor: &ComponentDescriptor,
        metadata: &FactoryMetadata,
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
        readers.validate(descriptor)?;
        Ok(Some(Self { fence, readers }))
    }

    pub(super) fn readers(&self) -> OutputReaders {
        self.readers.clone()
    }

    /// Stop/restart: reads begun before this call can no longer succeed.
    pub(super) fn advance(&self) {
        self.fence.advance();
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
    };

    fn reader(reply: Vec<u8>, capability: OutputCapability) -> (Reader, Arc<AtomicUsize>) {
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
                        ComponentId::try_new("component").unwrap(),
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

    fn producer(component: &str) -> GraphProducerIdentity {
        GraphProducerIdentity::new(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new(component).unwrap(),
            StreamId::try_new(format!("{component}/out")).unwrap(),
            true,
        )
        .unwrap()
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
