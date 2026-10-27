// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Producer side of the snapshot-follow extension. A lease is its own handle and
//! shares the output-read activation fence: stop, restart, release or
//! replacement closes it and it can never serve a later activation.
//!
//! The lease itself owns the provider's open future, its tail and the future of
//! an in-progress `next`; operations only poll them. Releasing the lease therefore
//! drops the provider's retention synchronously, even while an operation handle is
//! still retained and unpolled; that operation later reports retirement. A
//! delivered-but-unacknowledged item is retained (at most one) and resent, and an
//! item produced by the provider is recorded in the same poll that obtains it, so
//! cancelling an operation can never lose it.

use super::output_reads::{
    check_snapshot, encode_snapshot, finish, limit, retired, ReadBinding, READ_TIMEOUT,
};
use super::*;
use crate::output_reads::{Outcome, WireFollowItem, WireFollowLimits, WireNext, WireSnapshot};
use drasi_computation_contracts::{
    BinaryEnvelopeCodecError, ChangeEnvelope, GraphProducerProgress, OutputFollow, OutputReadError,
    OutputSnapshotFollow, PortId, SchemaDescriptor, SnapshotFollowProvider,
};
use std::{
    borrow::Cow,
    future::Future,
    pin::Pin,
    sync::{atomic::AtomicBool, Mutex},
    task::{Context, Poll},
};

pub(super) static EXTENSION: abi::output_follow::PluginOutputFollowV1 =
    abi::output_follow::PluginOutputFollowV1 {
        header: Header::new::<abi::output_follow::PluginOutputFollowV1>(),
        version: abi::output_follow::VERSION,
        reserved: 0,
        open: Some(open),
    };

static PROVIDER_VTABLE: abi::output_follow::OutputFollowProviderVTable =
    abi::output_follow::OutputFollowProviderVTable {
        header: Header::new::<abi::output_follow::OutputFollowProviderVTable>(),
        release: Some(release_provider),
        begin: Some(begin),
    };

static LEASE_VTABLE: abi::output_follow::OutputFollowLeaseVTable =
    abi::output_follow::OutputFollowLeaseVTable {
        header: Header::new::<abi::output_follow::OutputFollowLeaseVTable>(),
        release: Some(release_lease),
        next: Some(next),
    };

type OpenFuture =
    Pin<Box<dyn Future<Output = Result<OutputSnapshotFollow, OutputReadError>> + Send>>;
type StepFuture = Pin<
    Box<
        dyn Future<
                Output = (
                    Box<dyn OutputFollow>,
                    Result<ChangeEnvelope, OutputReadError>,
                ),
            > + Send,
    >,
>;

/// Everything the provider owns for one lease lives in exactly one slot.
enum Slot {
    /// The provider's open future, until it completes.
    Opening(OpenFuture),
    Idle(Box<dyn OutputFollow>),
    /// The tail and the future of an in-progress provider `next`.
    Running(StepFuture),
    Closed,
}

struct LeaseState {
    slot: Slot,
    /// Items produced from the provider so far; indices start at 1.
    produced: u64,
    /// The last produced item until the holder acknowledges observing it.
    pending: Option<(u64, Vec<u8>)>,
    /// Encoded sticky non-item outcome (expiry, lag, epoch change, limit).
    terminal: Option<Vec<u8>>,
}

/// One live lease. Dropping the last reference returns its capacity.
pub(super) struct Lease {
    binding: Arc<ReadBinding>,
    schema: SchemaDescriptor,
    generation: u64,
    state: Mutex<LeaseState>,
    active_next: AtomicBool,
    /// Wakes operations of a closed lease so they report retirement promptly.
    cancel: tokio::sync::watch::Sender<()>,
    /// A poisoned lock fences the lease; it is never silently continued.
    poisoned: AtomicBool,
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.binding.lease_count.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Lease {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, LeaseState>, Failure> {
        self.state.lock().map_err(|_| {
            self.poisoned.store(true, Ordering::Release);
            Failure::failed("native output follow lease state is poisoned; the lease is fenced")
        })
    }

    /// Release the provider's resources even if a panic poisoned the lock. The
    /// lease stays fenced: every later operation reports the poisoned state.
    fn lock_for_cleanup(&self) -> std::sync::MutexGuard<'_, LeaseState> {
        self.state.lock().unwrap_or_else(|error| {
            self.poisoned.store(true, Ordering::Release);
            error.into_inner()
        })
    }

    /// Synchronously drop everything the provider holds for this lease and wake
    /// its operations. Called by release, stop, restart and component release.
    pub(super) fn close(&self) {
        let released = {
            let mut state = self.lock_for_cleanup();
            (
                std::mem::replace(&mut state.slot, Slot::Closed),
                state.pending.take(),
                state.terminal.take(),
            )
        };
        // Provider drops run outside the lease lock.
        drop(released);
        self.cancel.send_modify(|_| ());
    }

    fn is_closed(&self) -> bool {
        self.state
            .lock()
            .map_or(true, |state| matches!(state.slot, Slot::Closed))
    }

    fn fail(state: &mut LeaseState, outcome: Outcome<()>) -> Result<Vec<u8>, Failure> {
        let bytes = finish(outcome)?;
        state.terminal = Some(bytes.clone());
        Ok(bytes)
    }

    fn item_reply(index: u64, frame: &[u8]) -> Result<Vec<u8>, Failure> {
        finish(Outcome::Ok(WireFollowItem {
            index,
            envelope: Cow::Borrowed(frame),
        }))
    }

    /// Poll the provider's open future; on success the lease becomes idle and the
    /// encoded snapshot is the reply.
    fn poll_open(&self, cx: &mut Context<'_>) -> Poll<Result<Vec<u8>, Failure>> {
        let mut state = match self.lock() {
            Ok(state) => state,
            Err(failure) => return Poll::Ready(Err(failure)),
        };
        let opened = match &mut state.slot {
            Slot::Opening(future) => match future.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(opened) => opened,
            },
            Slot::Closed => return Poll::Ready(Err(retired())),
            _ => {
                return Poll::Ready(Err(Failure::protocol(
                    "native output follow lease was already opened",
                )))
            }
        };
        let (snapshot, follow) = match opened {
            Ok(opened) => opened.into_parts(),
            Err(error) => {
                state.slot = Slot::Closed;
                return Poll::Ready(finish(Outcome::<WireSnapshot<'_>>::from_result(Err(error))));
            }
        };
        // Any rejection closes the lease, dropping the tail and its retention.
        if let Err(error) = check_snapshot(&self.binding.descriptor, &self.schema, &snapshot) {
            state.slot = Slot::Closed;
            return Poll::Ready(finish(Outcome::<WireSnapshot<'_>>::from_result(Err(error))));
        }
        match encode_snapshot(&snapshot) {
            Ok(Ok(bytes)) => {
                state.slot = Slot::Idle(follow);
                Poll::Ready(Ok(bytes))
            }
            Ok(Err(message)) => {
                state.slot = Slot::Closed;
                Poll::Ready(limit(message))
            }
            Err(failure) => {
                state.slot = Slot::Closed;
                Poll::Ready(Err(failure))
            }
        }
    }

    /// Record a provider result as the pending item (or terminal outcome) in the
    /// same poll that obtained it, so it can be resent until acknowledged.
    fn deliver(
        &self,
        state: &mut LeaseState,
        result: Result<ChangeEnvelope, OutputReadError>,
    ) -> Result<Vec<u8>, Failure> {
        let envelope = match result {
            Ok(envelope) => envelope,
            Err(error) => return Self::fail(state, Outcome::<()>::from_result(Err(error))),
        };
        if envelope.changes().schema() != &self.schema {
            return Self::fail(
                state,
                Outcome::Invalid(
                    "followed envelope schema differs from the declared output port schema".into(),
                ),
            );
        }
        match GraphProducerProgress::from_envelope(&envelope) {
            Ok(Some(progress))
                if progress.identity().component_id() == self.binding.descriptor.id() => {}
            _ => {
                return Self::fail(
                    state,
                    Outcome::Invalid(
                        "followed envelope lacks this component's producer progress".into(),
                    ),
                )
            }
        }
        let frame = match self.binding.codec.encode(&envelope) {
            Ok(bytes) if bytes.len() <= abi::output_reads::MAX_READ_BYTES => bytes,
            Ok(_) | Err(BinaryEnvelopeCodecError::SizeLimit { .. }) => {
                return Self::fail(
                    state,
                    Outcome::Limit("a followed envelope exceeds the negotiated bound".into()),
                )
            }
            Err(error) => {
                return Self::fail(
                    state,
                    Outcome::Invalid(format!("followed envelope cannot be encoded: {error}")),
                )
            }
        };
        let Some(index) = state.produced.checked_add(1) else {
            return Self::fail(
                state,
                Outcome::Failed("native output follow item index is exhausted".into()),
            );
        };
        state.produced = index;
        let reply = Self::item_reply(index, &frame);
        state.pending = Some((index, frame));
        reply
    }

    /// One poll of `next`: resend an unacknowledged item, else drive the provider.
    fn poll_next(&self, cx: &mut Context<'_>, acknowledged: u64) -> Poll<Result<Vec<u8>, Failure>> {
        let mut state = match self.lock() {
            Ok(state) => state,
            Err(failure) => return Poll::Ready(Err(failure)),
        };
        if matches!(state.slot, Slot::Closed) {
            return Poll::Ready(Err(retired()));
        }
        if acknowledged > state.produced {
            return Poll::Ready(Err(Failure::new(
                abi::status::INVALID_ARGUMENT,
                "acknowledged an output follow item that was never delivered",
            )));
        }
        if let Some((index, frame)) = &state.pending {
            if *index > acknowledged {
                return Poll::Ready(Self::item_reply(*index, frame));
            }
            state.pending = None;
        }
        if let Some(terminal) = &state.terminal {
            return Poll::Ready(Ok(terminal.clone()));
        }
        if matches!(state.slot, Slot::Opening(_)) {
            return Poll::Ready(Err(Failure::new(
                abi::status::INVALID_ARGUMENT,
                "native output follow lease has not been opened",
            )));
        }
        if matches!(state.slot, Slot::Idle(_)) {
            let Slot::Idle(mut follow) = std::mem::replace(&mut state.slot, Slot::Closed) else {
                unreachable!("checked idle");
            };
            state.slot = Slot::Running(Box::pin(async move {
                let result = follow.next().await;
                (follow, result)
            }));
        }
        let stepped = match &mut state.slot {
            Slot::Running(future) => match future.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(stepped) => stepped,
            },
            _ => return Poll::Ready(Err(retired())),
        };
        let (follow, result) = stepped;
        state.slot = Slot::Idle(follow);
        Poll::Ready(self.deliver(&mut state, result))
    }
}

struct ProviderState {
    binding: Arc<ReadBinding>,
    provider: Arc<dyn SnapshotFollowProvider>,
    schema: SchemaDescriptor,
}

struct NextGuard(Arc<Lease>);
impl Drop for NextGuard {
    fn drop(&mut self) {
        self.0.active_next.store(false, Ordering::Release);
    }
}

unsafe extern "C" fn open(
    raw: *mut c_void,
    port: BorrowedBytes,
    out: *mut abi::output_follow::OutputFollowProviderHandle,
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
                "native component registered no output follow providers",
            )
        })?;
        let port = std::str::from_utf8(unsafe { transport::borrowed_bytes(port, 256)? })
            .map_err(|_| Failure::new(abi::status::INVALID_ARGUMENT, "output port is not UTF-8"))?;
        let port = PortId::try_new(port)
            .map_err(|error| Failure::new(abi::status::INVALID_ARGUMENT, error.to_string()))?;
        let schema = binding
            .descriptor
            .ports()
            .iter()
            .find(|declared| declared.id() == &port)
            .map(|declared| declared.schema().clone())
            .ok_or_else(|| {
                Failure::new(
                    abi::status::INVALID_ARGUMENT,
                    format!("unknown native output port {port}"),
                )
            })?;
        let provider = binding
            .readers
            .ports()
            .get(&port)
            .and_then(|registered| registered.snapshot_follow())
            .cloned()
            .ok_or_else(|| {
                Failure::new(
                    abi::status::UNSUPPORTED,
                    format!("native output {port} has no snapshot-follow provider"),
                )
            })?;
        unsafe {
            transport::write_out(
                out,
                abi::output_follow::OutputFollowProviderHandle {
                    state: Box::into_raw(Box::new(Arc::new(ProviderState {
                        binding: binding.clone(),
                        provider,
                        schema,
                    })))
                    .cast(),
                    vtable: &PROVIDER_VTABLE,
                },
            )
        }
    })
}

unsafe extern "C" fn release_provider(state: *mut c_void) {
    transport::drop_boundary(|| unsafe { drop(Box::from_raw(state.cast::<Arc<ProviderState>>())) });
}

unsafe extern "C" fn begin(
    state: *mut c_void,
    input: BorrowedBytes,
    out_lease: *mut abi::output_follow::OutputFollowLeaseHandle,
    out_operation: *mut abi::OperationHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out_lease)?;
        require_output(out_operation)?;
        let provider = unsafe { state_ref::<Arc<ProviderState>>(state)? }.clone();
        let binding = provider.binding.clone();
        if binding.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        let input =
            unsafe { transport::borrowed_bytes(input, abi::output_follow::MAX_REQUEST_BYTES)? }
                .to_vec();
        let invalid = |message: String| Failure::new(abi::status::INVALID_ARGUMENT, message);
        let limits = wire::decode::<WireFollowLimits>(&input)
            .map_err(|error| invalid(format!("{error:#}")))?
            .into_limits()
            .map_err(|error| invalid(error.to_string()))?;
        let (guard, mut changed) = binding.claim()?;
        if binding.lease_count.fetch_add(1, Ordering::AcqRel) >= abi::output_follow::MAX_LEASES {
            binding.lease_count.fetch_sub(1, Ordering::AcqRel);
            return Err(Failure::new(
                abi::status::BUSY,
                "too many native output follow leases are active",
            ));
        }
        let opening: OpenFuture = {
            let follow_provider = provider.provider.clone();
            Box::pin(async move { follow_provider.snapshot_and_follow(limits).await })
        };
        let lease = Arc::new(Lease {
            binding: binding.clone(),
            schema: provider.schema.clone(),
            generation: guard.generation(),
            state: Mutex::new(LeaseState {
                slot: Slot::Opening(opening),
                produced: 0,
                pending: None,
                terminal: None,
            }),
            active_next: AtomicBool::new(false),
            cancel: tokio::sync::watch::channel(()).0,
            poisoned: AtomicBool::new(false),
        });
        {
            let mut leases = binding.leases.lock().map_err(|_| {
                binding.damaged.quarantine();
                Failure::failed("native output follow lease registry is poisoned")
            })?;
            leases.retain(|lease| lease.strong_count() > 0);
            leases.push(Arc::downgrade(&lease));
        }
        let runtime = io_runtime()?;
        let damaged = binding.damaged.clone();
        let opening = lease.clone();
        let mut cancelled = opening.cancel.subscribe();
        let operation = transport::export_component_operation(
            async move {
                guard.validate()?;
                if opening.is_closed() {
                    return Err(retired());
                }
                let bytes = tokio::select! {
                    biased;
                    _ = changed.changed() => return Err(retired()),
                    _ = cancelled.changed() => return Err(retired()),
                    result = tokio::time::timeout(
                        READ_TIMEOUT,
                        std::future::poll_fn(|cx| opening.poll_open(cx)),
                    ) => match result {
                        Ok(result) => result?,
                        Err(_) => {
                            opening.close();
                            return Err(Failure::failed("native output follow open timed out"));
                        }
                    },
                };
                guard.validate()?;
                Ok(bytes)
            },
            Some(runtime),
            damaged,
            false,
        );
        unsafe {
            transport::write_out(
                out_lease,
                abi::output_follow::OutputFollowLeaseHandle {
                    state: Box::into_raw(Box::new(lease)).cast(),
                    vtable: &LEASE_VTABLE,
                },
            )?;
            transport::write_out(out_operation, operation)
        }
    })
}

unsafe extern "C" fn release_lease(state: *mut c_void) {
    transport::drop_boundary(|| {
        let lease = unsafe { Box::from_raw(state.cast::<Arc<Lease>>()) };
        lease.close();
    });
}

unsafe extern "C" fn next(
    state: *mut c_void,
    input: BorrowedBytes,
    out: *mut abi::OperationHandle,
) -> Status {
    transport::status_boundary(|| {
        require_output(out)?;
        let lease = unsafe { state_ref::<Arc<Lease>>(state)? }.clone();
        if lease.binding.damaged.is_damaged() {
            return Err(Failure::panicked());
        }
        if lease.poisoned.load(Ordering::Acquire) {
            return Err(Failure::failed(
                "native output follow lease state is poisoned; the lease is fenced",
            ));
        }
        let input =
            unsafe { transport::borrowed_bytes(input, abi::output_follow::MAX_REQUEST_BYTES)? }
                .to_vec();
        let request: WireNext = wire::decode(&input)
            .map_err(|error| Failure::new(abi::status::INVALID_ARGUMENT, format!("{error:#}")))?;
        if lease.active_next.swap(true, Ordering::AcqRel) {
            return Err(Failure::new(
                abi::status::BUSY,
                "native output follow already has an active next",
            ));
        }
        let active = NextGuard(lease.clone());
        let (guard, mut changed) = lease.binding.claim_generation(Some(lease.generation))?;
        let mut cancelled = lease.cancel.subscribe();
        let runtime = io_runtime()?;
        let damaged = lease.binding.damaged.clone();
        let operation = transport::export_component_operation(
            async move {
                let _active = active;
                guard.validate()?;
                if lease.is_closed() {
                    return Err(retired());
                }
                let bytes = tokio::select! {
                    biased;
                    _ = changed.changed() => return Err(retired()),
                    _ = cancelled.changed() => return Err(retired()),
                    result = std::future::poll_fn(|cx| lease.poll_next(cx, request.acknowledged)) => result?,
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
mod tests {
    use super::super::output_reads::test_binding;
    use super::*;
    use drasi_computation_contracts::{
        ComponentId, GraphChangeCodec, GraphProducerIdentity, StreamId,
    };
    use drasi_core_types::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use std::sync::atomic::AtomicUsize;

    /// A tail that counts when the provider's retention is dropped.
    struct Counted(Arc<AtomicUsize>);
    impl Drop for Counted {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    #[async_trait::async_trait]
    impl OutputFollow for Counted {
        async fn next(&mut self) -> Result<ChangeEnvelope, OutputReadError> {
            std::future::pending().await
        }
    }

    fn lease(binding: &Arc<ReadBinding>, slot: Slot, produced: u64) -> Arc<Lease> {
        binding.lease_count.fetch_add(1, Ordering::SeqCst);
        Arc::new(Lease {
            binding: binding.clone(),
            schema: GraphChangeCodec::schema().descriptor().clone(),
            generation: 1,
            state: Mutex::new(LeaseState {
                slot,
                produced,
                pending: None,
                terminal: None,
            }),
            active_next: AtomicBool::new(false),
            cancel: tokio::sync::watch::channel(()).0,
            poisoned: AtomicBool::new(false),
        })
    }

    fn envelope(logical: u64) -> ChangeEnvelope {
        let identity = GraphProducerIdentity::new(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new("component").unwrap(),
            StreamId::try_new("component/out").unwrap(),
            true,
        )
        .unwrap();
        let mut envelope = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("s", "a"),
                        labels: Arc::from(Vec::<Arc<str>>::new()),
                        effective_from: 1,
                    },
                    properties: ElementPropertyMap::from(serde_json::json!({})),
                },
            },
            identity.stream().clone(),
            1,
            None,
        )
        .unwrap();
        GraphProducerProgress::annotate(&mut envelope, &identity, logical).unwrap();
        envelope
    }

    #[test]
    fn an_exhausted_item_index_is_an_explicit_terminal_error_not_a_wrap_or_panic() {
        let binding = test_binding();
        let normal = lease(&binding, Slot::Closed, 0);
        {
            let mut state = normal.lock().unwrap();
            normal.deliver(&mut state, Ok(envelope(1))).unwrap();
            assert_eq!(state.produced, 1);
            assert_eq!(state.pending.as_ref().map(|(index, _)| *index), Some(1));
        }

        let exhausted = lease(&binding, Slot::Closed, u64::MAX);
        let mut state = exhausted.lock().unwrap();
        let reply = exhausted.deliver(&mut state, Ok(envelope(1))).unwrap();
        assert_eq!(state.produced, u64::MAX, "the index must not wrap");
        assert!(state.pending.is_none());
        assert_eq!(state.terminal.as_ref(), Some(&reply));
        assert!(matches!(
            wire::decode::<Outcome<()>>(&reply).unwrap(),
            Outcome::Failed(message) if message.contains("exhausted")
        ));
    }

    #[test]
    fn a_poisoned_lease_still_releases_provider_retention_and_stays_fenced() {
        let binding = test_binding();
        let dropped = Arc::new(AtomicUsize::new(0));
        let lease = lease(&binding, Slot::Idle(Box::new(Counted(dropped.clone()))), 0);
        let poisoner = lease.clone();
        let panicked = std::thread::spawn(move || {
            let _guard = poisoner.state.lock().unwrap();
            panic!("poison the lease lock on purpose");
        })
        .join();
        assert!(panicked.is_err() && lease.state.is_poisoned());

        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        let Poll::Ready(Err(failure)) = lease.poll_next(&mut cx, 0) else {
            panic!("a poisoned lease must report the failure, not continue")
        };
        assert_eq!(failure.code, abi::status::FAILED);
        assert!(failure.message.contains("poisoned"));
        assert!(lease.poisoned.load(Ordering::Acquire));
        assert_eq!(dropped.load(Ordering::SeqCst), 0);

        lease.close();
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "cleanup must release the provider even when the lock is poisoned"
        );
        assert!(lease.is_closed());
        assert!(matches!(
            lease.poll_next(&mut cx, 0),
            Poll::Ready(Err(failure)) if failure.message.contains("poisoned")
        ));
    }
}
