// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Host consumer of the snapshot-follow extension. A lease is a producer-owned
//! handle, released exactly once when its follow is dropped. Each `next` is one
//! independent cancellable operation carrying the last item this follow actually
//! returned, so the producer resends anything a cancelled or rejected reply
//! could have lost. Leases never outlive the proxy's stop, restart or release.

use super::output_reads::{failure_error, invalid, Context};
use super::proxy::{native_operation_cause, RemoteComponent};
use anyhow::Context as _;
use async_trait::async_trait;
use drasi_computation_plugin_abi::{self as abi, output_follow as ext};
use drasi_computation_plugin_sdk::{
    output_reads::{WireFollowItem, WireFollowLimits, WireNext},
    transport::{checked_table, take_status, Failure, OperationFuture},
};
use drasi_lib::computation::v1::*;
use std::{result::Result, sync::Arc};

struct ProviderHandle(ext::OutputFollowProviderHandle);
// The ABI permits concurrent `begin` calls and release on any thread.
unsafe impl Send for ProviderHandle {}
unsafe impl Sync for ProviderHandle {}

impl ProviderHandle {
    unsafe fn new(raw: ext::OutputFollowProviderHandle) -> Result<Self, Failure> {
        let table = unsafe { checked_table(raw.vtable)? };
        if raw.state.is_null() || table.release.is_none() {
            return Err(Failure::protocol("incomplete native output follow handle"));
        }
        // Dropping an incomplete handle releases it.
        let handle = Self(raw);
        if table.begin.is_none() {
            return Err(Failure::protocol("incomplete native output follow handle"));
        }
        Ok(handle)
    }

    /// The lease exists as soon as this returns; the operation opens it.
    fn begin(&self, limits: &[u8]) -> Result<(LeaseHandle, OperationFuture), Failure> {
        let table = unsafe { &*self.0.vtable };
        let mut lease = ext::OutputFollowLeaseHandle::null();
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(table.begin.expect("validated")(
                self.0.state,
                abi::BorrowedBytes::new(limits),
                &mut lease,
                &mut operation,
            ))?;
        }
        // Wrap both before failing so a malformed half is released exactly once.
        let lease = unsafe { LeaseHandle::new(lease) };
        let operation = unsafe { OperationFuture::new(operation) };
        Ok((lease?, operation?))
    }
}

impl Drop for ProviderHandle {
    fn drop(&mut self) {
        let table = unsafe { &*self.0.vtable };
        unsafe { table.release.expect("validated")(self.0.state) };
    }
}

struct LeaseHandle(ext::OutputFollowLeaseHandle);
// Operations on one lease are serialized by its single owner (`&mut` next).
unsafe impl Send for LeaseHandle {}
unsafe impl Sync for LeaseHandle {}

impl LeaseHandle {
    unsafe fn new(raw: ext::OutputFollowLeaseHandle) -> Result<Self, Failure> {
        let table = unsafe { checked_table(raw.vtable)? };
        if raw.state.is_null() || table.release.is_none() {
            return Err(Failure::protocol("incomplete native output follow lease"));
        }
        let handle = Self(raw);
        if table.next.is_none() {
            return Err(Failure::protocol("incomplete native output follow lease"));
        }
        Ok(handle)
    }

    fn next(&self, request: &[u8]) -> Result<OperationFuture, Failure> {
        let table = unsafe { &*self.0.vtable };
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(table.next.expect("validated")(
                self.0.state,
                abi::BorrowedBytes::new(request),
                &mut operation,
            ))?;
            OperationFuture::new(operation)
        }
    }
}

impl Drop for LeaseHandle {
    fn drop(&mut self) {
        let table = unsafe { &*self.0.vtable };
        unsafe { table.release.expect("validated")(self.0.state) };
    }
}

pub(super) struct NativeFollowProvider {
    handle: ProviderHandle,
    context: Arc<Context>,
}

impl NativeFollowProvider {
    pub(super) fn open(
        extension: ext::PluginOutputFollowV1,
        component: &Arc<RemoteComponent>,
        port: &PortId,
        context: Context,
    ) -> anyhow::Result<Self> {
        let mut raw = ext::OutputFollowProviderHandle::null();
        unsafe {
            take_status(extension.open.expect("validated")(
                component.state(),
                abi::BorrowedBytes::new(port.as_str().as_bytes()),
                &mut raw,
            ))
            .map_err(native_operation_cause)?;
        }
        let handle = unsafe { ProviderHandle::new(raw) }
            .map_err(native_operation_cause)
            .with_context(|| format!("native output {port} follow handle"))?;
        Ok(Self {
            handle,
            context: Arc::new(context),
        })
    }
}

#[async_trait]
impl SnapshotFollowProvider for NativeFollowProvider {
    async fn snapshot_and_follow(
        &self,
        limits: OutputFollowLimits,
    ) -> Result<OutputSnapshotFollow, OutputReadError> {
        limits.validate()?;
        let request =
            encode_bounded_messagepack(&WireFollowLimits::new(&limits)?, ext::MAX_REQUEST_BYTES)
                .map_err(invalid)?;
        let context = &self.context;
        let (generation, mut changed) = context.fence.enter()?;
        // The lease handle is owned from here: every early return releases it.
        let (lease, operation) = self.handle.begin(&request).map_err(failure_error)?;
        let bytes = tokio::select! {
            biased;
            _ = changed.changed() => return Err(OutputReadError::Retired),
            result = operation => result.map_err(failure_error)?,
        };
        if !context.fence.is_current(generation) {
            return Err(OutputReadError::Retired);
        }
        if bytes.len() > abi::output_reads::MAX_READ_BYTES {
            return Err(invalid("native output follow open reply exceeds its bound"));
        }
        let snapshot = context.snapshot_from(&bytes)?;
        let follow = NativeFollow {
            lease,
            context: context.clone(),
            generation,
            cursor: snapshot.cursor().clone(),
            observed: 0,
        };
        Ok(OutputSnapshotFollow::new(snapshot, Box::new(follow)))
    }
}

struct NativeFollow {
    lease: LeaseHandle,
    context: Arc<Context>,
    /// The activation this lease belongs to.
    generation: u64,
    /// The last position this follow returned (initially the snapshot's).
    cursor: OutputCursor,
    /// The last item index this follow returned; sent as the acknowledgement.
    observed: u64,
}

#[async_trait]
impl OutputFollow for NativeFollow {
    async fn next(&mut self) -> Result<ChangeEnvelope, OutputReadError> {
        let context = self.context.clone();
        let (_, mut changed) = context.fence.enter()?;
        if !context.fence.is_current(self.generation) {
            return Err(OutputReadError::Retired);
        }
        let request = encode_bounded_messagepack(
            &WireNext {
                acknowledged: self.observed,
            },
            ext::MAX_REQUEST_BYTES,
        )
        .map_err(invalid)?;
        let operation = self.lease.next(&request).map_err(failure_error)?;
        let bytes = tokio::select! {
            biased;
            _ = changed.changed() => return Err(OutputReadError::Retired),
            result = operation => result.map_err(failure_error)?,
        };
        if !context.fence.is_current(self.generation) {
            return Err(OutputReadError::Retired);
        }
        if bytes.len() > abi::output_reads::MAX_READ_BYTES {
            return Err(invalid("native output follow item exceeds its bound"));
        }
        let item: WireFollowItem<'_> = context.outcome(&bytes)?;
        let expected = self
            .observed
            .checked_add(1)
            .ok_or_else(|| invalid("native output follow item index is exhausted"))?;
        if item.index != expected {
            return Err(invalid("native output follow skipped or repeated an item"));
        }
        let envelope = context.codec.decode(&item.envelope).map_err(invalid)?;
        if envelope.changes().schema() != context.schema.descriptor() {
            return Err(invalid(
                "followed envelope has another schema than the output",
            ));
        }
        if envelope.system().stream() != self.cursor.producer().stream() {
            return Err(invalid(
                "followed envelope belongs to another stream than its producer's",
            ));
        }
        let progress = GraphProducerProgress::from_envelope(&envelope)
            .map_err(invalid)?
            .ok_or_else(|| invalid("followed envelope lacks its producer's progress"))?;
        if progress.identity() != self.cursor.producer() {
            return Err(OutputReadError::EpochChanged {
                requested: Box::new(self.cursor.producer().clone()),
                current: Box::new(progress.identity().clone()),
            });
        }
        if progress.sequence() <= self.cursor.sequence() {
            return Err(invalid("followed output is out of logical order"));
        }
        // Acknowledge only what is being returned to the caller.
        self.cursor = self.cursor.at_sequence(progress.sequence());
        self.observed = item.index;
        Ok(envelope)
    }
}

#[cfg(test)]
mod tests {
    use super::super::output_reads::Fence;
    use super::*;
    use drasi_computation_plugin_sdk::{output_reads::Outcome, transport::export_operation, wire};
    use std::{
        borrow::Cow,
        collections::VecDeque,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Mutex,
        },
    };

    struct Scenario {
        replies: Mutex<VecDeque<Vec<u8>>>,
        acknowledged: Arc<Mutex<Vec<u64>>>,
        released: Arc<AtomicUsize>,
    }

    unsafe extern "C" fn fake_release(state: *mut std::ffi::c_void) {
        let scenario = unsafe { Box::from_raw(state.cast::<Scenario>()) };
        scenario.released.fetch_add(1, Ordering::SeqCst);
    }

    unsafe extern "C" fn fake_next(
        state: *mut std::ffi::c_void,
        input: abi::BorrowedBytes,
        out: *mut abi::OperationHandle,
    ) -> abi::Status {
        let scenario = unsafe { &*state.cast::<Scenario>() };
        let request: WireNext =
            wire::decode(unsafe { std::slice::from_raw_parts(input.data, input.len) }).unwrap();
        scenario
            .acknowledged
            .lock()
            .unwrap()
            .push(request.acknowledged);
        let reply = scenario.replies.lock().unwrap().pop_front();
        let operation = match reply {
            Some(reply) => export_operation(async move { Ok(reply) }, None),
            None => export_operation(std::future::pending::<Result<Vec<u8>, Failure>>(), None),
        };
        unsafe { std::ptr::write(out, operation) };
        abi::Status::ok()
    }

    static VTABLE: ext::OutputFollowLeaseVTable = ext::OutputFollowLeaseVTable {
        header: abi::Header::new::<ext::OutputFollowLeaseVTable>(),
        release: Some(fake_release),
        next: Some(fake_next),
    };

    struct Fixture {
        follow: NativeFollow,
        owner: GraphProducerIdentity,
        acknowledged: Arc<Mutex<Vec<u64>>>,
        released: Arc<AtomicUsize>,
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
            900 + logical,
            None,
        )
        .unwrap();
        GraphProducerProgress::annotate(&mut envelope, identity, logical).unwrap();
        envelope
    }

    fn fixture(replies: Vec<Vec<u8>>) -> Fixture {
        let owner = producer("component");
        let acknowledged = Arc::new(Mutex::new(Vec::new()));
        let released = Arc::new(AtomicUsize::new(0));
        let state = Box::into_raw(Box::new(Scenario {
            replies: Mutex::new(replies.into()),
            acknowledged: acknowledged.clone(),
            released: released.clone(),
        }));
        let lease = unsafe {
            LeaseHandle::new(ext::OutputFollowLeaseHandle {
                state: state.cast(),
                vtable: &VTABLE,
            })
        }
        .unwrap();
        let schema = GraphChangeCodec::schema();
        let codec = Arc::new(wire::codec(std::slice::from_ref(&schema)).unwrap());
        let context = Arc::new(Context {
            endpoint: OutputEndpoint::new(
                ComponentId::try_new("component").unwrap(),
                PortId::try_new("out").unwrap(),
            ),
            capability: OutputCapability::SnapshotFollow,
            schema,
            codec,
            fence: Arc::new(Fence::new()),
        });
        Fixture {
            follow: NativeFollow {
                lease,
                context,
                generation: 0,
                cursor: OutputCursor::try_new(owner.clone(), 0).unwrap(),
                observed: 0,
            },
            owner,
            acknowledged,
            released,
        }
    }

    fn item(fixture: &Fixture, index: u64, envelope: &ChangeEnvelope) -> Vec<u8> {
        wire::encode(&Outcome::Ok(WireFollowItem {
            index,
            envelope: Cow::Owned(fixture.follow.context.codec.encode(envelope).unwrap()),
        }))
        .unwrap()
    }

    fn fixture_with_items(items: &[(u64, u64)]) -> Fixture {
        let fixture = fixture(Vec::new());
        let replies = items
            .iter()
            .map(|(index, logical)| item(&fixture, *index, &envelope(&fixture.owner, *logical)))
            .collect::<VecDeque<_>>();
        let state = unsafe { &*fixture.follow.lease.0.state.cast::<Scenario>() };
        *state.replies.lock().unwrap() = replies;
        fixture
    }

    #[tokio::test(flavor = "current_thread")]
    async fn items_are_acknowledged_only_after_they_are_returned() {
        let mut fixture = fixture_with_items(&[(1, 1), (2, 3)]);
        let owner = fixture.owner.clone();
        assert_eq!(
            GraphProducerProgress::from_envelope(&fixture.follow.next().await.unwrap())
                .unwrap()
                .unwrap()
                .identity(),
            &owner
        );
        let second = fixture.follow.next().await.unwrap();
        assert_eq!(
            GraphProducerProgress::from_envelope(&second)
                .unwrap()
                .unwrap()
                .sequence(),
            3
        );
        assert_eq!(*fixture.acknowledged.lock().unwrap(), [0, 1]);
        assert_eq!(fixture.released.load(Ordering::SeqCst), 0);
        let released = fixture.released.clone();
        drop(fixture.follow);
        assert_eq!(released.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn a_rejected_reply_is_not_acknowledged_so_it_can_be_resent() {
        let mut fixture = fixture(Vec::new());
        let good = item(&fixture, 1, &envelope(&fixture.owner, 1));
        let state = unsafe { &*fixture.follow.lease.0.state.cast::<Scenario>() };
        // First reply is garbage, then the same item arrives again.
        *state.replies.lock().unwrap() = VecDeque::from([vec![0xc1], good]);
        assert!(matches!(
            fixture.follow.next().await,
            Err(OutputReadError::Invalid(_))
        ));
        assert!(fixture.follow.next().await.is_ok());
        assert_eq!(*fixture.acknowledged.lock().unwrap(), [0, 0]);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn skipped_repeated_foreign_and_unordered_items_are_rejected() {
        for (name, items) in [
            ("skipped index", vec![(2u64, 1u64)]),
            ("repeated index", vec![(1, 1), (1, 2)]),
            ("unordered sequence", vec![(1, 2), (2, 2)]),
        ] {
            let mut fixture = fixture_with_items(&items);
            let mut failed = false;
            for _ in 0..items.len() {
                failed = fixture.follow.next().await.is_err();
            }
            assert!(failed, "{name} was accepted");
        }
        let mut fixture = fixture_with_items(&[]);
        let foreign = producer("component");
        let reply = item(&fixture, 1, &envelope(&foreign, 1));
        let state = unsafe { &*fixture.follow.lease.0.state.cast::<Scenario>() };
        state.replies.lock().unwrap().push_back(reply);
        let Err(OutputReadError::EpochChanged { requested, current }) = fixture.follow.next().await
        else {
            panic!("another epoch's output must be a typed epoch change")
        };
        assert_eq!((*requested, *current), (fixture.owner.clone(), foreign));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn typed_lease_errors_and_limits_are_preserved() {
        let mut fixture = fixture(Vec::new());
        let encode = |outcome: Outcome<()>| wire::encode(&outcome).unwrap();
        let state = unsafe { &*fixture.follow.lease.0.state.cast::<Scenario>() };
        for outcome in [
            Outcome::LeaseExpired,
            Outcome::Gap {
                requested: 4,
                oldest: None,
                latest: 9,
            },
            Outcome::Limit("too large".into()),
            Outcome::NotReady("busy".into()),
        ] {
            state.replies.lock().unwrap().push_back(encode(outcome));
        }
        assert!(matches!(
            fixture.follow.next().await,
            Err(OutputReadError::LeaseExpired)
        ));
        assert!(matches!(
            fixture.follow.next().await,
            Err(OutputReadError::Gap {
                requested: 4,
                oldest: None,
                latest: 9
            })
        ));
        let Err(OutputReadError::Failed(error)) = fixture.follow.next().await else {
            panic!("a limit must stay an explicit failure")
        };
        assert!(error
            .downcast_ref::<drasi_computation_plugin_sdk::output_reads::OutputReadLimit>()
            .is_some());
        assert!(matches!(
            fixture.follow.next().await,
            Err(OutputReadError::NotReady(_))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn retirement_ends_a_pending_next_and_every_later_one() {
        let mut fixture = fixture(Vec::new());
        let fence = fixture.follow.context.fence.clone();
        {
            let pending = fixture.follow.next();
            tokio::pin!(pending);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(20), &mut pending)
                    .await
                    .is_err()
            );
            fence.advance();
            assert!(matches!(pending.await, Err(OutputReadError::Retired)));
        }
        assert!(matches!(
            fixture.follow.next().await,
            Err(OutputReadError::Retired)
        ));
        fence.retire();
        assert!(matches!(
            fixture.follow.next().await,
            Err(OutputReadError::Retired)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn another_stream_and_an_exhausted_index_are_errors_not_panics() {
        let mut fixture = fixture(Vec::new());
        let other_stream = GraphProducerIdentity::for_epoch(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new("component").unwrap(),
            StreamId::try_new("component/other").unwrap(),
            fixture.owner.incarnation(),
            true,
        )
        .unwrap();
        let wrong = item(&fixture, 1, &envelope(&other_stream, 1));
        let right = item(&fixture, 1, &envelope(&fixture.owner, 1));
        let state = unsafe { &*fixture.follow.lease.0.state.cast::<Scenario>() };
        state.replies.lock().unwrap().push_back(wrong);
        assert!(matches!(
            fixture.follow.next().await,
            Err(OutputReadError::Invalid(_))
        ));
        assert_eq!(fixture.follow.observed, 0);

        fixture.follow.observed = u64::MAX;
        state.replies.lock().unwrap().push_back(right);
        assert!(matches!(
            fixture.follow.next().await,
            Err(OutputReadError::Invalid(_))
        ));
        assert_eq!(fixture.follow.observed, u64::MAX);
    }
}
