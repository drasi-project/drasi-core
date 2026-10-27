// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Producer side of the per-output read extension. Providers are captured once
//! at construction from `NativeComponent::output_readers`. A read never takes the
//! component's processing lock or data-operation slot, so it is independent of a
//! pending `next`/`transform`. Every read is fenced by the component's
//! activation generation: stop, restart, release or replacement ends it.

use super::*;
use crate::output_reads::{Outcome, WireCursor, WireFrame, WireRecord, WireReplay, WireSnapshot};
use drasi_computation_contracts::{
    encode_bounded_messagepack, BinaryEnvelopeCodecError, ComponentDescriptor, OutputCursor,
    OutputPortReaders, OutputReadError, OutputReaders, OutputReplay, OutputSnapshot, PortId,
    ReplayProvider, SchemaDescriptor, SnapshotProvider,
};
use serde::Serialize;
use std::{
    borrow::Cow,
    collections::BTreeSet,
    sync::{atomic::AtomicUsize, Mutex},
    time::Duration,
};

pub(super) static EXTENSION: abi::output_reads::PluginOutputReadsV1 =
    abi::output_reads::PluginOutputReadsV1 {
        header: Header::new::<abi::output_reads::PluginOutputReadsV1>(),
        version: abi::output_reads::VERSION,
        reserved: 0,
        open: Some(open),
    };

static HANDLE_VTABLE: abi::output_reads::OutputReadVTable = abi::output_reads::OutputReadVTable {
    header: Header::new::<abi::output_reads::OutputReadVTable>(),
    release: Some(release_handle),
    request: Some(request),
};

pub(super) const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Inactive,
    Active,
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
    lifecycle: Mutex<Lifecycle>,
    inflight: AtomicUsize,
    changed: tokio::sync::watch::Sender<()>,
    pub(super) damaged: Arc<transport::PanicState>,
    /// Weak registry so stop/release can end live leases promptly.
    pub(super) leases: Mutex<Vec<std::sync::Weak<super::output_follow::Lease>>>,
    pub(super) lease_count: AtomicUsize,
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
    anyhow::ensure!(
        declared(&capabilities.output_snapshots)
            == actual(&readers, |port| port.snapshot().is_some()),
        "native output snapshot providers do not match the declared output_snapshots"
    );
    anyhow::ensure!(
        declared(&capabilities.output_replays) == actual(&readers, |port| port.replay().is_some()),
        "native output replay providers do not match the declared output_replays"
    );
    anyhow::ensure!(
        declared(&capabilities.output_follows)
            == actual(&readers, |port| port.snapshot_follow().is_some()),
        "native output snapshot-follow providers do not match the declared output_follows"
    );
    if !capabilities.declares_output_reads() && !capabilities.declares_output_follows() {
        return Ok(None);
    }
    Ok(Some(Arc::new(ReadBinding {
        descriptor: descriptor.clone(),
        codec,
        readers,
        lifecycle: Mutex::new(Lifecycle {
            generation: 0,
            phase: Phase::Inactive,
        }),
        inflight: AtomicUsize::new(0),
        changed: tokio::sync::watch::channel(()).0,
        damaged,
        leases: Mutex::new(Vec::new()),
        lease_count: AtomicUsize::new(0),
    })))
}

impl ReadBinding {
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
    pub(super) fn deactivate(&self) {
        {
            let mut lifecycle = self.lock_to_fence(&self.lifecycle);
            if lifecycle.phase == Phase::Active {
                lifecycle.phase = Phase::Inactive;
            }
            self.changed.send_modify(|_| ());
        }
        self.close_leases();
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
        let lifecycle = self
            .lifecycle
            .lock()
            .map_err(|_| Failure::protocol("native output read lifecycle lock is poisoned"))?;
        match lifecycle.phase {
            Phase::Retired => return Err(retired()),
            Phase::Inactive if lease_generation.is_some() => return Err(retired()),
            Phase::Inactive => {
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
        if lifecycle.phase == Phase::Active && lifecycle.generation == self.generation {
            Ok(())
        } else {
            Err(retired())
        }
    }
}

impl Drop for ReadGuard {
    fn drop(&mut self) {
        self.binding.inflight.fetch_sub(1, Ordering::AcqRel);
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
    if snapshot.records().len() > abi::output_reads::MAX_SNAPSHOT_RECORDS {
        return Ok(Err(format!(
            "snapshot has {} records; at most {} are supported",
            snapshot.records().len(),
            abi::output_reads::MAX_SNAPSHOT_RECORDS
        )));
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
    let reply = Outcome::Ok(WireSnapshot {
        cursor: WireCursor::new(snapshot.cursor()),
        dataset: snapshot.dataset().to_owned(),
        schema: snapshot.schema().clone(),
        records,
    });
    match encode_bounded_messagepack(&reply, abi::output_reads::MAX_READ_BYTES) {
        Ok(bytes) => Ok(Ok(bytes)),
        Err(BinaryEnvelopeCodecError::SizeLimit { limit: bytes }) => {
            Ok(Err(format!("encoded output read exceeds {bytes} bytes")))
        }
        Err(error) => Err(Failure::failed(error.to_string())),
    }
}

enum Provider {
    Snapshot(Arc<dyn SnapshotProvider>),
    Replay(Arc<dyn ReplayProvider>),
}

/// One provider handle. It owns its provider reference and the binding, but not
/// the component, which it never keeps alive.
struct HandleState {
    binding: Arc<ReadBinding>,
    provider: Provider,
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

    async fn serve(&self, code: u32, cursor: Option<&OutputCursor>) -> Result<Vec<u8>, Failure> {
        use abi::output_reads::operation::*;
        match (&self.provider, code) {
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
            _ => Err(Failure::new(
                abi::status::UNSUPPORTED,
                "operation is not supported by this output provider",
            )),
        }
    }

    fn replay_reply(
        &self,
        result: Result<OutputReplay, OutputReadError>,
    ) -> Result<Vec<u8>, Failure> {
        let replay = match result {
            Ok(replay) => replay,
            Err(error) => return finish(Outcome::<WireReplay<'_>>::from_result(Err(error))),
        };
        if replay.envelopes().len() > abi::output_reads::MAX_REPLAY_ENVELOPES {
            return limit(format!(
                "replay has {} envelopes; at most {} are supported",
                replay.envelopes().len(),
                abi::output_reads::MAX_REPLAY_ENVELOPES
            ));
        }
        let mut envelopes = Vec::with_capacity(replay.envelopes().len());
        for envelope in replay.envelopes() {
            match self.binding.codec.encode(envelope) {
                Ok(bytes) => envelopes.push(WireFrame {
                    envelope: Cow::Owned(bytes),
                }),
                Err(BinaryEnvelopeCodecError::SizeLimit { limit: bytes }) => {
                    return limit(format!("a replayed envelope exceeds {bytes} bytes"))
                }
                Err(error) => {
                    return finish(Outcome::<WireReplay<'_>>::Invalid(format!(
                        "replayed envelope cannot be encoded: {error}"
                    )))
                }
            }
        }
        finish(Outcome::Ok(WireReplay {
            after: WireCursor::new(replay.after()),
            head: WireCursor::new(replay.head()),
            envelopes,
        }))
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
        use abi::output_reads::operation::*;
        let cursor = match (&handle.provider, code) {
            (Provider::Snapshot(_), SNAPSHOT) | (Provider::Replay(_), COMMITTED_HEAD) => {
                if !input.is_empty() {
                    return Err(Failure::new(
                        abi::status::INVALID_ARGUMENT,
                        "this output read takes no input",
                    ));
                }
                None
            }
            (Provider::Replay(_), REPLAY_AFTER) => Some(decode_cursor(&input)?),
            _ => {
                return Err(Failure::new(
                    abi::status::UNSUPPORTED,
                    "operation is not supported by this output provider",
                ))
            }
        };
        let (guard, mut changed) = handle.binding.claim()?;
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
    let descriptor =
        ComponentDescriptor::try_new(ComponentId::try_new("component").unwrap(), ports.clone())
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
        binding.deactivate();
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
