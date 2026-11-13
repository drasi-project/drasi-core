// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Producer side of the per-output read extension. Providers are captured once
//! at construction from `NativeComponent::output_readers`. A read never takes the
//! component's processing lock or data-operation slot, so it is independent of a
//! pending `next`/`transform`. Every read is fenced by the component's
//! activation generation: stop, restart, release or replacement ends it.

use super::*;
use anyhow::Context as _;
mod transfer;
use crate::output_reads::{
    Outcome, OutputReadLimit, WireCursor, WireFrame, WireQueryHead, WireQueryReplay,
    WireQuerySnapshot, WireQueryView, WireRecord, WireReplay, WireSnapshot,
};
use drasi_computation_contracts::{
    encode_bounded_messagepack, BinaryEnvelopeCodecError, ComponentDescriptor, OutputCursor,
    OutputPortReaders, OutputReadError, OutputReaders, OutputReplay, OutputSnapshot, PortId,
    QueryDescription, QueryViewProvider, ReplayProvider, SchemaDescriptor, SnapshotProvider,
};
use serde::Serialize;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    sync::{atomic::AtomicUsize, Mutex},
    time::Duration,
};
pub(super) use transfer::Pages;

pub(super) static EXTENSION: abi::output_reads::PluginOutputReadsV2 =
    abi::output_reads::PluginOutputReadsV2 {
        header: Header::new::<abi::output_reads::PluginOutputReadsV2>(),
        version: abi::output_reads::VERSION,
        reserved: 0,
        open: Some(open),
    };

static HANDLE_VTABLE: abi::output_reads::OutputReadVTable = abi::output_reads::OutputReadVTable {
    header: Header::new::<abi::output_reads::OutputReadVTable>(),
    release: Some(release_handle),
    request: Some(request),
    transfer: Some(transfer::open),
};

pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Inactive,
    Active,
    Retained,
    Retired,
}

struct Lifecycle {
    generation: u64,
    phase: Phase,
}

/// The component's captured providers and their activation fence.
pub(super) struct ReadBinding {
    pub(super) descriptor: ComponentDescriptor,
    pub(super) codec: Arc<BinaryEnvelopeCodec>,
    pub(super) readers: OutputReaders,
    query_descriptions: Mutex<BTreeMap<PortId, QueryDescription>>,
    query_observations: bool,
    query_recovery_views: bool,
    lifecycle: Mutex<Lifecycle>,
    inflight: AtomicUsize,
    changed: tokio::sync::watch::Sender<()>,
    quiescence: tokio::sync::watch::Sender<()>,
    pub(super) damaged: Arc<transport::PanicState>,
    /// Weak registry so stop/release can end live leases promptly.
    pub(super) leases: Mutex<Vec<std::sync::Weak<super::output_follow::Lease>>>,
    pub(super) lease_count: AtomicUsize,
    transfers: Mutex<Vec<std::sync::Weak<transfer::Transfer>>>,
    transfer_count: AtomicUsize,
}

/// Validate the constructed component's actual registrations against its
/// declared metadata. Returns no binding when nothing is registered.
pub(super) fn capture(
    metadata: &FactoryMetadata,
    descriptor: &ComponentDescriptor,
    readers: OutputReaders,
    codec: Arc<BinaryEnvelopeCodec>,
    damaged: Arc<transport::PanicState>,
) -> anyhow::Result<Option<Arc<ReadBinding>>> {
    validate_registrations(metadata, descriptor, &readers)?;
    let capabilities = &metadata.capabilities;
    if !capabilities.declares_output_reads() && !capabilities.declares_output_follows() {
        return Ok(None);
    }
    let queries = describe_queries(&readers);
    Ok(Some(Arc::new(ReadBinding {
        descriptor: descriptor.clone(),
        codec,
        readers,
        query_descriptions: Mutex::new(queries),
        query_observations: capabilities.query_observations,
        query_recovery_views: capabilities.query_recovery_views,
        lifecycle: Mutex::new(Lifecycle {
            generation: 0,
            phase: Phase::Inactive,
        }),
        inflight: AtomicUsize::new(0),
        changed: tokio::sync::watch::channel(()).0,
        quiescence: tokio::sync::watch::channel(()).0,
        damaged,
        leases: Mutex::new(Vec::new()),
        lease_count: AtomicUsize::new(0),
        transfers: Mutex::new(Vec::new()),
        transfer_count: AtomicUsize::new(0),
    })))
}

fn validate_registrations(
    metadata: &FactoryMetadata,
    descriptor: &ComponentDescriptor,
    readers: &OutputReaders,
) -> anyhow::Result<()> {
    readers.validate(descriptor)?;
    fn declared(ports: &[PortId]) -> BTreeSet<PortId> {
        ports.iter().cloned().collect()
    }
    fn actual(
        readers: &OutputReaders,
        provided: fn(&OutputPortReaders) -> bool,
    ) -> BTreeSet<PortId> {
        readers
            .ports()
            .iter()
            .filter(|(_, registered)| provided(registered))
            .map(|(port, _)| port.clone())
            .collect()
    }
    let capabilities = &metadata.capabilities;
    if capabilities.query_observations {
        let port = capabilities
            .output_queries
            .first()
            .context("missing query observation port")?;
        anyhow::ensure!(
            readers
                .ports()
                .get(port)
                .and_then(|output| output.query())
                .is_some_and(|reader| reader.supports_observations()),
            "declared query observations have no implementation"
        );
    }
    if capabilities.query_recovery_views {
        anyhow::ensure!(
            readers
                .ports()
                .get(&capabilities.output_queries[0])
                .and_then(|port| port.query())
                .is_some_and(|reader| reader.supports_recovery_views()),
            "declared query recovery views have no implementation"
        );
    }
    anyhow::ensure!(
        declared(&capabilities.output_snapshots)
            == actual(readers, |port| port.snapshot().is_some()),
        "native output snapshot providers do not match the declared output_snapshots"
    );
    anyhow::ensure!(
        declared(&capabilities.output_replays) == actual(readers, |port| port.replay().is_some()),
        "native output replay providers do not match the declared output_replays"
    );
    anyhow::ensure!(
        declared(&capabilities.output_follows)
            == actual(readers, |port| port.snapshot_follow().is_some()),
        "native output snapshot-follow providers do not match the declared output_follows"
    );
    anyhow::ensure!(
        declared(&capabilities.output_queries) == actual(readers, |port| port.query().is_some()),
        "native query readers do not match the declared output_queries"
    );
    Ok(())
}

fn describe_queries(readers: &OutputReaders) -> BTreeMap<PortId, QueryDescription> {
    readers
        .ports()
        .iter()
        .filter_map(|(port, readers)| {
            readers
                .query()
                .map(|reader| (port.clone(), reader.description()))
        })
        .collect()
}

/// Prepare new descriptions without publishing a partial or invalid update.
/// The existing binding and provider Arcs remain the owners of all read handles.
pub(super) fn updated_queries(
    metadata: &FactoryMetadata,
    descriptor: &ComponentDescriptor,
    current: Option<&ReadBinding>,
    readers: OutputReaders,
) -> anyhow::Result<BTreeMap<PortId, QueryDescription>> {
    validate_registrations(metadata, descriptor, &readers)?;
    let Some(current) = current else {
        anyhow::ensure!(
            readers.is_empty(),
            "native update introduced output readers"
        );
        return Ok(BTreeMap::new());
    };
    fn same<T: ?Sized>(before: Option<&Arc<T>>, after: Option<&Arc<T>>) -> bool {
        match (before, after) {
            (Some(before), Some(after)) => Arc::ptr_eq(before, after),
            (None, None) => true,
            _ => false,
        }
    }
    anyhow::ensure!(
        current.readers.ports().len() == readers.ports().len(),
        "native update changed output reader registrations"
    );
    for (port, before) in current.readers.ports() {
        let after = readers
            .ports()
            .get(port)
            .context("native update removed an output reader")?;
        anyhow::ensure!(
            same(before.snapshot(), after.snapshot())
                && same(before.replay(), after.replay())
                && same(before.snapshot_follow(), after.snapshot_follow())
                && same(before.query(), after.query()),
            "native retained update replaced the registered output reader for {port}"
        );
    }
    Ok(describe_queries(&current.readers))
}

impl ReadBinding {
    pub(super) fn query_descriptions(&self) -> Result<BTreeMap<PortId, QueryDescription>, Failure> {
        Ok(self
            .query_descriptions
            .lock()
            .map_err(|_| Failure::protocol("native query description lock is poisoned"))?
            .clone())
    }

    pub(super) fn query_description(&self, port: &PortId) -> Result<QueryDescription, Failure> {
        self.query_descriptions
            .lock()
            .map_err(|_| Failure::protocol("native query description lock is poisoned"))?
            .get(port)
            .cloned()
            .ok_or_else(|| Failure::protocol("missing native query description"))
    }

    pub(super) fn publish_queries(
        &self,
        queries: BTreeMap<PortId, QueryDescription>,
    ) -> Result<(), Failure> {
        *self
            .query_descriptions
            .lock()
            .map_err(|_| Failure::protocol("native query description lock is poisoned"))? = queries;
        Ok(())
    }

    pub(super) async fn wait_quiescent(&self) -> anyhow::Result<()> {
        // Deactivation closes every provider-owned lease/transfer and advances
        // the generation before STOP runs. Retained ABI operation handles are
        // then read-only observations of retirement; their caller-controlled
        // lifetime must not block component cleanup.
        Ok(())
    }

    /// Called when START succeeds. Earlier reads can never become valid again.
    pub(super) fn activate(&self) -> anyhow::Result<()> {
        let mut lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| anyhow::anyhow!("native output read lifecycle lock is poisoned"))?;
        anyhow::ensure!(
            lifecycle.phase != Phase::Retired,
            "native output reads were retired"
        );
        lifecycle.generation = lifecycle
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("native output read generation exhausted"))?;
        lifecycle.phase = Phase::Active;
        self.changed.send_modify(|_| ());
        Ok(())
    }

    /// Called when STOP begins: in-flight reads are cancelled and new reads fail.
    pub(super) fn deactivate(&self) -> Result<(), Failure> {
        let mut exhausted = false;
        {
            let mut lifecycle = self.lock_to_fence(&self.lifecycle);
            if lifecycle.phase != Phase::Retired {
                if let Some(generation) = lifecycle.generation.checked_add(1) {
                    lifecycle.generation = generation;
                    lifecycle.phase = Phase::Inactive;
                } else {
                    lifecycle.phase = Phase::Retired;
                    exhausted = true;
                }
            }
            self.changed.send_modify(|_| ());
        }
        self.close_leases();
        if exhausted {
            Err(Failure::protocol(
                "native output read lifecycle generation exhausted",
            ))
        } else {
            Ok(())
        }
    }

    pub(super) fn retain_recovery(&self) -> anyhow::Result<()> {
        if self.query_recovery_views {
            let mut lifecycle = self
                .lifecycle
                .lock()
                .map_err(|_| anyhow::anyhow!("native output read lifecycle lock is poisoned"))?;
            anyhow::ensure!(
                lifecycle.phase != Phase::Retired,
                "native output reads were retired"
            );
            lifecycle.phase = Phase::Retained;
            self.changed.send_modify(|_| ());
        }
        Ok(())
    }

    /// Called when the component is released; permanent.
    pub(super) fn retire(&self) {
        {
            let mut lifecycle = self.lock_to_fence(&self.lifecycle);
            lifecycle.phase = Phase::Retired;
            self.changed.send_modify(|_| ());
        }
        self.close_leases();
    }

    /// Fencing and cleanup must still run when a panic poisoned a lock, so the
    /// guarded state is recovered for that purpose only. The poisoning is made
    /// observable: the component is quarantined and every later operation
    /// reports the panic state.
    fn lock_to_fence<'a, T>(&self, mutex: &'a Mutex<T>) -> std::sync::MutexGuard<'a, T> {
        mutex.lock().unwrap_or_else(|error| {
            self.damaged.quarantine();
            error.into_inner()
        })
    }

    /// A lease never survives its activation: its provider retention is released
    /// here, not when the holder eventually releases the handle.
    fn close_leases(&self) {
        let leases = std::mem::take(&mut *self.lock_to_fence(&self.leases));
        for lease in leases {
            if let Some(lease) = lease.upgrade() {
                lease.close();
            }
        }
        let transfers = std::mem::take(&mut *self.lock_to_fence(&self.transfers));
        for transfer in transfers {
            if let Some(transfer) = transfer.upgrade() {
                transfer.close();
            }
        }
    }

    pub(super) fn claim(
        self: &Arc<Self>,
    ) -> Result<(ReadGuard, tokio::sync::watch::Receiver<()>), Failure> {
        self.claim_generation(None)
    }

    /// A lease request must belong to the activation that opened the lease. An
    /// inactive or restarted component retires it rather than reporting not-ready.
    pub(super) fn claim_generation(
        self: &Arc<Self>,
        lease_generation: Option<u64>,
    ) -> Result<(ReadGuard, tokio::sync::watch::Receiver<()>), Failure> {
        self.claim_mode(lease_generation, false)
    }

    fn claim_read(
        self: &Arc<Self>,
        code: u32,
    ) -> Result<(ReadGuard, tokio::sync::watch::Receiver<()>), Failure> {
        self.claim_mode(None, recovery_request(code))
    }

    fn claim_mode(
        self: &Arc<Self>,
        lease_generation: Option<u64>,
        recovery: bool,
    ) -> Result<(ReadGuard, tokio::sync::watch::Receiver<()>), Failure> {
        let lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| Failure::protocol("native output read lifecycle lock is poisoned"))?;
        match lifecycle.phase {
            Phase::Retired => return Err(retired()),
            Phase::Retained if recovery && self.query_recovery_views => {}
            Phase::Inactive | Phase::Retained if lease_generation.is_some() => {
                return Err(retired())
            }
            Phase::Inactive | Phase::Retained => {
                return Err(Failure::new(
                    abi::output_reads::status::NOT_ACTIVE,
                    "native component is not active",
                ))
            }
            Phase::Active => {}
        }
        if lease_generation.is_some_and(|generation| generation != lifecycle.generation) {
            return Err(retired());
        }
        if self.inflight.fetch_add(1, Ordering::AcqRel) >= abi::output_reads::MAX_CONCURRENT_READS {
            self.inflight.fetch_sub(1, Ordering::AcqRel);
            return Err(Failure::new(
                abi::status::BUSY,
                "too many native output reads are active",
            ));
        }
        Ok((
            ReadGuard {
                binding: self.clone(),
                generation: lifecycle.generation,
                recovery,
            },
            self.changed.subscribe(),
        ))
    }

    fn open(
        self: &Arc<Self>,
        port: &PortId,
        capability: u32,
    ) -> Result<abi::output_reads::OutputReadHandle, Failure> {
        let schema = self
            .descriptor
            .ports()
            .iter()
            .find(|declared| declared.id() == port)
            .map(|declared| declared.schema().clone())
            .ok_or_else(|| {
                Failure::new(
                    abi::status::INVALID_ARGUMENT,
                    format!("unknown native output port {port}"),
                )
            })?;
        let registered = self.readers.ports().get(port).ok_or_else(|| {
            Failure::new(
                abi::status::INVALID_ARGUMENT,
                format!("native output port {port} registered no providers"),
            )
        })?;
        let provider = match capability {
            abi::output_reads::capability::SNAPSHOT => {
                Provider::Snapshot(registered.snapshot().cloned().ok_or_else(|| {
                    Failure::new(
                        abi::status::UNSUPPORTED,
                        format!("native output {port} has no snapshot provider"),
                    )
                })?)
            }
            abi::output_reads::capability::REPLAY => {
                Provider::Replay(registered.replay().cloned().ok_or_else(|| {
                    Failure::new(
                        abi::status::UNSUPPORTED,
                        format!("native output {port} has no replay provider"),
                    )
                })?)
            }
            abi::output_reads::capability::QUERY => {
                Provider::Query(registered.query().cloned().ok_or_else(|| {
                    Failure::new(
                        abi::status::UNSUPPORTED,
                        "native output has no query reader",
                    )
                })?)
            }
            _ => {
                return Err(Failure::new(
                    abi::status::INVALID_ARGUMENT,
                    "unknown native output read capability",
                ))
            }
        };
        if self
            .lifecycle
            .lock()
            .map_err(|_| Failure::protocol("native output read lifecycle lock is poisoned"))?
            .phase
            == Phase::Retired
        {
            return Err(retired());
        }
        Ok(abi::output_reads::OutputReadHandle {
            state: Box::into_raw(Box::new(Arc::new(HandleState {
                binding: self.clone(),
                provider,
                port: port.clone(),
                schema,
            })))
            .cast(),
            vtable: &HANDLE_VTABLE,
        })
    }
}

pub(super) fn retired() -> Failure {
    Failure::new(
        abi::status::CLOSED,
        "native output read belongs to a retired component or activation",
    )
}

pub(super) struct ReadGuard {
    binding: Arc<ReadBinding>,
    generation: u64,
    recovery: bool,
}

impl ReadGuard {
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    pub(super) fn validate(&self) -> Result<(), Failure> {
        let lifecycle = self
            .binding
            .lifecycle
            .lock()
            .map_err(|_| Failure::protocol("native output read lifecycle lock is poisoned"))?;
        if (lifecycle.phase == Phase::Active || self.recovery && lifecycle.phase == Phase::Retained)
            && lifecycle.generation == self.generation
        {
            Ok(())
        } else {
            Err(retired())
        }
    }
}

impl Drop for ReadGuard {
    fn drop(&mut self) {
        self.binding.inflight.fetch_sub(1, Ordering::AcqRel);
        self.binding.quiescence.send_modify(|_| ());
    }
}

pub(super) fn check_cursor(
    descriptor: &ComponentDescriptor,
    cursor: &OutputCursor,
) -> Result<(), OutputReadError> {
    if cursor.producer().component_id() == descriptor.id() {
        Ok(())
    } else {
        Err(OutputReadError::invalid(
            "provider reported another component's position",
        ))
    }
}

pub(super) fn check_snapshot(
    descriptor: &ComponentDescriptor,
    schema: &SchemaDescriptor,
    snapshot: &OutputSnapshot,
) -> Result<(), OutputReadError> {
    check_cursor(descriptor, snapshot.cursor())?;
    if snapshot.schema() != schema {
        return Err(OutputReadError::invalid(
            "snapshot schema differs from the declared output port schema",
        ));
    }
    Ok(())
}

pub(super) fn snapshot_reply(
    result: Result<OutputSnapshot, OutputReadError>,
) -> Result<Vec<u8>, Failure> {
    let snapshot = match result {
        Ok(snapshot) => snapshot,
        Err(error) => return finish(Outcome::<WireSnapshot<'_>>::from_result(Err(error))),
    };
    match encode_snapshot(&snapshot)? {
        Ok(bytes) => Ok(bytes),
        Err(message) => limit(message),
    }
}

/// The encoded successful snapshot reply, or the explicit limit that it exceeds.
pub(super) fn encode_snapshot(
    snapshot: &OutputSnapshot,
) -> Result<Result<Vec<u8>, String>, Failure> {
    let snapshot = match snapshot_wire(snapshot) {
        Ok(snapshot) => snapshot,
        Err(message) => return Ok(Err(message)),
    };
    match encode_bounded_messagepack(&Outcome::Ok(snapshot), abi::output_reads::MAX_READ_BYTES) {
        Ok(bytes) => Ok(Ok(bytes)),
        Err(BinaryEnvelopeCodecError::SizeLimit { limit: bytes }) => {
            Ok(Err(format!("encoded output read exceeds {bytes} bytes")))
        }
        Err(error) => Err(Failure::failed(error.to_string())),
    }
}

fn snapshot_wire(snapshot: &OutputSnapshot) -> Result<WireSnapshot<'_>, String> {
    if snapshot.records().len() > abi::output_reads::MAX_SNAPSHOT_RECORDS {
        return Err(format!(
            "snapshot has {} records; at most {} are supported",
            snapshot.records().len(),
            abi::output_reads::MAX_SNAPSHOT_RECORDS
        ));
    }
    let mut records: Vec<WireRecord<'_>> = snapshot
        .records()
        .values()
        .map(|record| WireRecord {
            namespace: Cow::Borrowed(record.identity().namespace()),
            identity: Cow::Borrowed(&record.identity().value()[..]),
            image: 0,
            payload: Cow::Borrowed(&record.payload()[..]),
        })
        .collect();
    records.sort_by(|left, right| {
        (&*left.namespace, &*left.identity).cmp(&(&*right.namespace, &*right.identity))
    });
    Ok(WireSnapshot {
        cursor: WireCursor::new(snapshot.cursor()),
        dataset: snapshot.dataset().to_owned(),
        schema: snapshot.schema().clone(),
        records,
    })
}

enum Provider {
    Snapshot(Arc<dyn SnapshotProvider>),
    Replay(Arc<dyn ReplayProvider>),
    Query(Arc<dyn QueryViewProvider>),
}

fn recovery_request(code: u32) -> bool {
    matches!(
        code,
        abi::output_reads::operation::QUERY_RECOVERY_OBSERVATION
            | abi::output_reads::operation::QUERY_RECOVERY_VIEW
    )
}

pub(super) enum CapturedRead {
    Snapshot(OutputSnapshot),
    Replay(OutputReplay),
    QuerySnapshot(OutputSnapshot, u64),
    QueryReplay(OutputReplay, u64),
    QueryView {
        snapshot: OutputSnapshot,
        retained: OutputReplay,
        generation: u64,
        config_hash: u64,
    },
}

/// One provider handle. It owns its provider reference and the binding, but not
/// the component, which it never keeps alive.
struct HandleState {
    binding: Arc<ReadBinding>,
    provider: Provider,
    port: PortId,
    schema: SchemaDescriptor,
}

impl HandleState {
    fn check_cursor(&self, cursor: &OutputCursor) -> Result<(), OutputReadError> {
        check_cursor(&self.binding.descriptor, cursor)
    }

    fn check_snapshot(&self, snapshot: &OutputSnapshot) -> Result<(), OutputReadError> {
        check_snapshot(&self.binding.descriptor, &self.schema, snapshot)
    }

    fn check_replay(
        &self,
        after: &OutputCursor,
        replay: &OutputReplay,
    ) -> Result<(), OutputReadError> {
        self.check_cursor(replay.head())?;
        if replay.after() != after {
            return Err(OutputReadError::invalid(
                "replay answers a different cursor than requested",
            ));
        }
        if replay
            .envelopes()
            .iter()
            .any(|envelope| envelope.changes().schema() != &self.schema)
        {
            return Err(OutputReadError::invalid(
                "replayed envelope schema differs from the declared output port schema",
            ));
        }
        Ok(())
    }

    fn request_cursor(&self, code: u32, input: &[u8]) -> Result<Option<OutputCursor>, Failure> {
        use abi::output_reads::operation::*;
        if code == QUERY_OBSERVATION && !self.binding.query_observations {
            return Err(Failure::new(
                abi::status::UNSUPPORTED,
                "query observations were not negotiated",
            ));
        }
        if recovery_request(code) && !self.binding.query_recovery_views {
            return Err(Failure::new(
                abi::status::UNSUPPORTED,
                "query recovery views were not negotiated",
            ));
        }
        match (&self.provider, code) {
            (Provider::Snapshot(_), SNAPSHOT)
            | (Provider::Replay(_), COMMITTED_HEAD)
            | (
                Provider::Query(_),
                QUERY_SNAPSHOT
                | QUERY_HEAD
                | QUERY_VIEW
                | QUERY_OBSERVATION
                | QUERY_RECOVERY_OBSERVATION
                | QUERY_RECOVERY_VIEW,
            ) => {
                if !input.is_empty() {
                    return Err(Failure::new(
                        abi::status::INVALID_ARGUMENT,
                        "this output read takes no input",
                    ));
                }
                Ok(None)
            }
            (Provider::Replay(_), REPLAY_AFTER) | (Provider::Query(_), QUERY_REPLAY) => {
                decode_cursor(input).map(Some)
            }
            _ => Err(Failure::new(
                abi::status::UNSUPPORTED,
                "operation is not supported by this output provider",
            )),
        }
    }

    async fn capture(
        &self,
        code: u32,
        cursor: Option<&OutputCursor>,
    ) -> Result<CapturedRead, OutputReadError> {
        use abi::output_reads::operation::*;
        let captured = match (&self.provider, code) {
            (Provider::Snapshot(provider), SNAPSHOT) => {
                CapturedRead::Snapshot(provider.snapshot().await?)
            }
            (Provider::Replay(provider), REPLAY_AFTER) => CapturedRead::Replay(
                provider
                    .replay_after(cursor.expect("validated cursor"))
                    .await?,
            ),
            (Provider::Query(provider), QUERY_SNAPSHOT) => {
                let (snapshot, generation) = provider.query_snapshot().await?;
                CapturedRead::QuerySnapshot(snapshot, generation)
            }
            (Provider::Query(provider), QUERY_REPLAY) => {
                let (replay, generation) = provider
                    .query_replay(cursor.expect("validated cursor"))
                    .await?;
                CapturedRead::QueryReplay(replay, generation)
            }
            (Provider::Query(provider), QUERY_VIEW | QUERY_RECOVERY_VIEW) => {
                let (cursor, view) = if code == QUERY_RECOVERY_VIEW {
                    provider.query_recovery_view().await?
                } else {
                    provider.query_view().await?
                };
                if cursor.sequence() != view.snapshot.as_of_sequence {
                    return Err(OutputReadError::invalid(
                        "query view cursor differs from its snapshot",
                    ));
                }
                if view.incarnation
                    != (!cursor.producer().persistent()).then_some(cursor.producer().incarnation())
                {
                    return Err(OutputReadError::invalid(
                        "query view incarnation differs from its producer epoch",
                    ));
                }
                let after = match view.retained.first() {
                    Some(envelope) => {
                        envelope.system().sequence().checked_sub(1).ok_or_else(|| {
                            OutputReadError::invalid("query retained output has a zero sequence")
                        })?
                    }
                    None => cursor.sequence(),
                };
                CapturedRead::QueryView {
                    retained: OutputReplay::try_new(
                        cursor.at_sequence(after),
                        cursor.clone(),
                        view.retained,
                    )?,
                    snapshot: OutputSnapshot::try_new(
                        cursor,
                        view.snapshot.projector.dataset(),
                        self.schema.clone(),
                        view.snapshot.rows,
                    )?,
                    generation: view.snapshot.generation,
                    config_hash: view.config_hash,
                }
            }
            _ => return Err(OutputReadError::invalid("this read cannot be transferred")),
        };
        match &captured {
            CapturedRead::Snapshot(snapshot) | CapturedRead::QuerySnapshot(snapshot, _) => {
                self.check_snapshot(snapshot)?;
            }
            CapturedRead::Replay(replay) | CapturedRead::QueryReplay(replay, _) => {
                self.check_replay(cursor.expect("validated cursor"), replay)?;
            }
            CapturedRead::QueryView {
                snapshot, retained, ..
            } => {
                self.check_snapshot(snapshot)?;
                self.check_replay(retained.after(), retained)?;
            }
        }
        Ok(captured)
    }

    async fn serve(&self, code: u32, cursor: Option<&OutputCursor>) -> Result<Vec<u8>, Failure> {
        use abi::output_reads::operation::*;
        match (&self.provider, code) {
            (Provider::Query(provider), QUERY_OBSERVATION | QUERY_RECOVERY_OBSERVATION) => {
                let description = self.binding.query_description(&self.port)?;
                let result = if code == QUERY_RECOVERY_OBSERVATION {
                    provider.query_recovery_observation().await
                } else {
                    provider.query_observation().await
                }
                .and_then(|observation| {
                    observation.validate(self.binding.descriptor.id(), &description)?;
                    crate::output_reads::WireQueryObservation::new(&observation)
                });
                finish(Outcome::from_result(result))
            }
            (Provider::Snapshot(provider), SNAPSHOT) => {
                let result = provider
                    .snapshot()
                    .await
                    .and_then(|snapshot| self.check_snapshot(&snapshot).map(|()| snapshot));
                snapshot_reply(result)
            }
            (Provider::Replay(provider), COMMITTED_HEAD) => {
                let result = provider
                    .committed_head()
                    .await
                    .and_then(|head| self.check_cursor(&head).map(|()| head));
                finish(Outcome::from_result(
                    result.map(|head| WireCursor::new(&head)),
                ))
            }
            (Provider::Replay(provider), REPLAY_AFTER) => {
                let after = cursor.expect("validated replay cursor");
                let result = provider
                    .replay_after(after)
                    .await
                    .and_then(|replay| self.check_replay(after, &replay).map(|()| replay));
                self.replay_reply(result)
            }
            (Provider::Query(provider), QUERY_SNAPSHOT) => {
                let result = provider
                    .query_snapshot()
                    .await
                    .and_then(|(snapshot, generation)| {
                        self.check_snapshot(&snapshot)?;
                        Ok((snapshot, generation))
                    });
                match result {
                    Ok((snapshot, generation)) => match snapshot_wire(&snapshot) {
                        Ok(snapshot) => finish(Outcome::Ok(WireQuerySnapshot {
                            snapshot,
                            generation,
                        })),
                        Err(message) => limit(message),
                    },
                    Err(error) => finish(Outcome::<WireQuerySnapshot<'_>>::from_result(Err(error))),
                }
            }
            (Provider::Query(provider), QUERY_HEAD) => {
                let result = provider.query_head().await.and_then(|(head, generation)| {
                    self.check_cursor(&head)?;
                    Ok(WireQueryHead {
                        cursor: WireCursor::new(&head),
                        generation,
                    })
                });
                finish(Outcome::from_result(result))
            }
            (Provider::Query(provider), QUERY_REPLAY) => {
                let after = cursor.expect("validated query cursor");
                let result = provider
                    .query_replay(after)
                    .await
                    .and_then(|(replay, generation)| {
                        self.check_replay(after, &replay)?;
                        Ok(WireQueryReplay {
                            replay: self.replay_wire(&replay)?,
                            generation,
                        })
                    });
                finish(Outcome::from_result(result))
            }
            (Provider::Query(_), QUERY_VIEW) => {
                self.query_view_reply(self.capture(code, cursor).await)
            }
            _ => Err(Failure::new(
                abi::status::UNSUPPORTED,
                "operation is not supported by this output provider",
            )),
        }
    }

    fn query_view_reply(
        &self,
        result: Result<CapturedRead, OutputReadError>,
    ) -> Result<Vec<u8>, Failure> {
        let checked = result.and_then(|captured| match captured {
            CapturedRead::QueryView {
                snapshot,
                retained,
                generation,
                config_hash,
            } => Ok((
                snapshot,
                self.replay_wire(&retained)?,
                generation,
                config_hash,
            )),
            _ => Err(OutputReadError::invalid("expected a query view")),
        });
        let (snapshot, retained, generation, config_hash) = match checked {
            Ok(view) => view,
            Err(error) => return finish(Outcome::<WireQueryView<'_>>::from_result(Err(error))),
        };
        let snapshot = match snapshot_wire(&snapshot) {
            Ok(snapshot) => snapshot,
            Err(message) => return limit(message),
        };
        finish(Outcome::Ok(WireQueryView {
            snapshot,
            retained,
            generation,
            config_hash,
        }))
    }

    fn replay_reply(
        &self,
        result: Result<OutputReplay, OutputReadError>,
    ) -> Result<Vec<u8>, Failure> {
        finish(Outcome::from_result(
            result.and_then(|replay| self.replay_wire(&replay)),
        ))
    }

    fn replay_wire(&self, replay: &OutputReplay) -> Result<WireReplay<'static>, OutputReadError> {
        if replay.envelopes().len() > abi::output_reads::MAX_REPLAY_ENVELOPES {
            return Err(OutputReadError::Failed(
                OutputReadLimit(format!(
                    "replay has {} envelopes; at most {} are supported",
                    replay.envelopes().len(),
                    abi::output_reads::MAX_REPLAY_ENVELOPES
                ))
                .into(),
            ));
        }
        let mut envelopes = Vec::with_capacity(replay.envelopes().len());
        for envelope in replay.envelopes() {
            match self.binding.codec.encode(envelope) {
                Ok(bytes) => envelopes.push(WireFrame {
                    envelope: Cow::Owned(bytes),
                }),
                Err(BinaryEnvelopeCodecError::SizeLimit { limit: bytes }) => {
                    return Err(OutputReadError::Failed(
                        OutputReadLimit(format!("a replayed envelope exceeds {bytes} bytes"))
                            .into(),
                    ));
                }
                Err(error) => {
                    return Err(OutputReadError::invalid(format!(
                        "replayed envelope cannot be encoded: {error}"
                    )));
                }
            }
        }
        Ok(WireReplay {
            after: WireCursor::new(replay.after()),
            head: WireCursor::new(replay.head()),
            envelopes,
        })
    }
}

pub(super) fn limit(message: String) -> Result<Vec<u8>, Failure> {
    encode_bounded_messagepack(
        &Outcome::<()>::Limit(message),
        abi::output_reads::MAX_READ_BYTES,
    )
    .map_err(|error| Failure::failed(error.to_string()))
}

fn decode_cursor(bytes: &[u8]) -> Result<OutputCursor, Failure> {
    let invalid = |message: String| Failure::new(abi::status::INVALID_ARGUMENT, message);
    let cursor: WireCursor = wire::decode(bytes).map_err(|error| invalid(format!("{error:#}")))?;
    cursor
        .into_cursor()
        .map_err(|error| invalid(error.to_string()))
}

/// Encode within the negotiated bound. An oversized answer is an explicit limit
/// outcome, never a truncated one.
pub(super) fn finish<T: Serialize>(outcome: Outcome<T>) -> Result<Vec<u8>, Failure> {
    match encode_bounded_messagepack(&outcome, abi::output_reads::MAX_READ_BYTES) {
        Ok(bytes) => Ok(bytes),
        Err(BinaryEnvelopeCodecError::SizeLimit { limit: bytes }) => {
            limit(format!("encoded output read exceeds {bytes} bytes"))
        }
        Err(error) => Err(Failure::failed(error.to_string())),
    }
}

unsafe extern "C" fn open(
    raw: *mut c_void,
    port: BorrowedBytes,
    capability: u32,
    out: *mut abi::output_reads::OutputReadHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let instance = unsafe { state_ref::<Arc<Instance>>(raw)? };
        if instance.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        let binding = instance.output_reads.as_ref().ok_or_else(|| {
            Failure::new(
                abi::status::UNSUPPORTED,
                "native component registered no output readers",
            )
        })?;
        let port = std::str::from_utf8(unsafe { transport::borrowed_bytes(port, 256)? })
            .map_err(|_| Failure::new(abi::status::INVALID_ARGUMENT, "output port is not UTF-8"))?;
        let port = PortId::try_new(port)
            .map_err(|error| Failure::new(abi::status::INVALID_ARGUMENT, error.to_string()))?;
        let handle = binding.open(&port, capability)?;
        unsafe { transport::write_out(out, handle) }
    })
}

unsafe extern "C" fn release_handle(state: *mut c_void) {
    transport::drop_boundary(|| unsafe { drop(Box::from_raw(state.cast::<Arc<HandleState>>())) });
}

unsafe extern "C" fn request(
    state: *mut c_void,
    code: u32,
    input: BorrowedBytes,
    out: *mut abi::OperationHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let handle = unsafe { state_ref::<Arc<HandleState>>(state)? }.clone();
        if handle.binding.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        let input =
            unsafe { transport::borrowed_bytes(input, abi::output_reads::MAX_REQUEST_BYTES)? }
                .to_vec();
        let cursor = handle.request_cursor(code, &input)?;
        let (guard, mut changed) = handle.binding.claim_read(code)?;
        let runtime = io_runtime()?;
        let damaged = handle.binding.damaged.clone();
        let operation = transport::export_component_operation(
            async move {
                guard.validate()?;
                let bytes = tokio::select! {
                    biased;
                    _ = changed.changed() => return Err(retired()),
                    result = tokio::time::timeout(
                        READ_TIMEOUT,
                        handle.serve(code, cursor.as_ref()),
                    ) => match result {
                        Ok(result) => result?,
                        Err(_) => return Err(Failure::failed("native output read timed out")),
                    },
                };
                guard.validate()?;
                Ok(bytes)
            },
            Some(runtime),
            damaged,
            false,
        );
        unsafe { transport::write_out(out, operation) }
    })
}

#[cfg(test)]
pub(super) fn test_binding() -> Arc<ReadBinding> {
    use drasi_computation_contracts::{
        ComponentId, ComponentRole, GraphChangeCodec, ImplementationIdentity, OutputFollowLimits,
        OutputSnapshotFollow, PipeRequirements, PortDescriptor, PortDirection,
        SnapshotFollowProvider,
    };

    struct Unused;
    #[async_trait::async_trait]
    impl SnapshotFollowProvider for Unused {
        async fn snapshot_and_follow(
            &self,
            _: OutputFollowLimits,
        ) -> Result<OutputSnapshotFollow, OutputReadError> {
            Err(OutputReadError::NotReady("unused test provider".into()))
        }
    }

    let out = PortId::try_new("out").unwrap();
    let ports = vec![PortDescriptor::new(
        out.clone(),
        PortDirection::Output,
        GraphChangeCodec::schema().descriptor().clone(),
        PipeRequirements::default(),
    )];
    let descriptor = ComponentDescriptor::try_new(
        ComponentId::try_new("49a5d640-ec80-4c62-9cdb-1baab036e8be").unwrap(),
        ports.clone(),
    )
    .unwrap();
    let metadata = FactoryMetadata {
        implementation: ImplementationIdentity::try_new("test/binding", "1").unwrap(),
        role: ComponentRole::Source,
        configuration_version: 1,
        configuration: Default::default(),
        ports,
        completion: None,
        capabilities: crate::metadata::Capabilities {
            output_follows: vec![out.clone()],
            ..Default::default()
        },
    };
    capture(
        &metadata,
        &descriptor,
        OutputReaders::new().with_snapshot_follow(out, Arc::new(Unused)),
        Arc::new(wire::codec(&[GraphChangeCodec::schema()]).unwrap()),
        Arc::new(transport::PanicState::default()),
    )
    .unwrap()
    .expect("a declared follow provider creates a binding")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_lifecycle_lock_still_fences_and_quarantines_the_component() {
        let binding = test_binding();
        binding.activate().unwrap();
        let (guard, _changed) = binding.claim().unwrap();
        guard.validate().unwrap();

        let poisoner = binding.clone();
        let panicked = std::thread::spawn(move || {
            let _guard = poisoner.lifecycle.lock().unwrap();
            panic!("poison the lifecycle lock on purpose");
        })
        .join();
        assert!(panicked.is_err() && binding.lifecycle.is_poisoned());
        assert!(!binding.damaged.is_damaged());

        // Fencing must still happen, and the poisoning must be observable.
        binding.deactivate().expect("deactivate output reads");
        assert!(
            binding.damaged.is_damaged(),
            "a poisoned lifecycle lock must quarantine the component"
        );
        assert!(binding.claim().is_err(), "new reads must be refused");
        assert!(guard.validate().is_err(), "in-flight reads must be fenced");
        assert!(
            binding.activate().is_err(),
            "a poisoned binding cannot reactivate"
        );
    }

    #[test]
    fn a_poisoned_lease_registry_quarantines_and_still_closes_leases() {
        let binding = test_binding();
        let poisoner = binding.clone();
        let panicked = std::thread::spawn(move || {
            let _guard = poisoner.leases.lock().unwrap();
            panic!("poison the lease registry on purpose");
        })
        .join();
        assert!(panicked.is_err());
        binding.retire();
        assert!(binding.damaged.is_damaged());
    }
}
