// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
};

use drasi_computation_plugin_sdk::{
    abi::{self, recovery::*},
    progress::{self, ProgressIdentity},
    transport::{self, Failure},
};
use drasi_lib::computation::v1::{QuerySourceProgress, SourceProgressSnapshot};
use tokio::sync::watch;

struct State {
    owner: Weak<QuerySourceProgress>,
    revoked: watch::Sender<bool>,
    subscriptions: AtomicUsize,
}

impl State {
    fn owner(&self) -> Result<Arc<QuerySourceProgress>, Failure> {
        if *self.revoked.borrow() {
            return Err(Failure::closed());
        }
        self.owner.upgrade().ok_or_else(Failure::closed)
    }
}

pub(super) struct ProgressBinding(Arc<State>);

impl ProgressBinding {
    pub(super) fn revoke(&self) {
        self.0.revoked.send_replace(true);
    }
    pub(super) fn new(owner: &Arc<QuerySourceProgress>) -> Self {
        Self(Arc::new(State {
            owner: Arc::downgrade(owner),
            revoked: watch::channel(false).0,
            subscriptions: AtomicUsize::new(0),
        }))
    }

    pub(super) fn table(&self) -> SourceProgressV1 {
        SourceProgressV1 {
            header: abi::Header::new::<SourceProgressV1>(),
            context: Arc::as_ptr(&self.0) as *mut c_void,
            retain: Some(retain),
            release: Some(release),
            describe: Some(describe),
            snapshot: Some(snapshot),
            subscribe: Some(subscribe),
        }
    }
}

impl Drop for ProgressBinding {
    fn drop(&mut self) {
        self.revoke();
    }
}

unsafe fn state<'a>(context: *mut c_void) -> Result<&'a State, Failure> {
    if context.is_null() {
        return Err(Failure::protocol("null source progress context"));
    }
    Ok(unsafe { &*context.cast::<State>() })
}

unsafe extern "C" fn retain(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::increment_strong_count(context.cast::<State>()) });
}
unsafe extern "C" fn release(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::decrement_strong_count(context.cast::<State>()) });
}
unsafe extern "C" fn describe(context: *mut c_void) -> abi::Reply {
    transport::reply_boundary(|| {
        let owner = unsafe { state(context)? }.owner()?;
        progress::encode(&ProgressIdentity {
            version: VERSION,
            graph_id: owner.graph_id().into(),
            component_id: owner.component_id().clone(),
        })
        .map_err(Failure::from)
    })
}
unsafe extern "C" fn snapshot(context: *mut c_void) -> abi::Reply {
    transport::reply_boundary(|| {
        let owner = unsafe { state(context)? }.owner()?;
        progress::encode_snapshot(&owner.snapshot()).map_err(Failure::from)
    })
}

struct Subscription {
    state: Arc<State>,
    receiver: Mutex<watch::Receiver<Arc<SourceProgressSnapshot>>>,
    waiting: AtomicBool,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.state.subscriptions.fetch_sub(1, Ordering::AcqRel);
    }
}

unsafe extern "C" fn subscribe(
    context: *mut c_void,
    out: *mut ProgressSubscriptionV1,
) -> abi::Status {
    transport::status_boundary(|| {
        if out.is_null() {
            return Err(Failure::protocol("null progress subscription output"));
        }
        let state = unsafe { state(context)? };
        let owner = state.owner()?;
        state
            .subscriptions
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_SUBSCRIPTIONS).then_some(count + 1)
            })
            .map_err(|_| Failure::new(abi::status::BUSY, "source progress subscription limit"))?;
        unsafe { Arc::increment_strong_count(context.cast::<State>()) };
        let subscription = Arc::new(Subscription {
            state: unsafe { Arc::from_raw(context.cast::<State>()) },
            receiver: Mutex::new(owner.subscribe()),
            waiting: AtomicBool::new(false),
        });
        unsafe {
            transport::write_out(
                out,
                ProgressSubscriptionV1 {
                    header: abi::Header::new::<ProgressSubscriptionV1>(),
                    context: Arc::into_raw(subscription) as *mut c_void,
                    release: Some(release_subscription),
                    snapshot: Some(subscription_snapshot),
                    wait: Some(wait),
                },
            )
        }
    })
}

unsafe fn subscription<'a>(context: *mut c_void) -> Result<&'a Subscription, Failure> {
    if context.is_null() {
        return Err(Failure::protocol("null progress subscription"));
    }
    Ok(unsafe { &*context.cast::<Subscription>() })
}

unsafe extern "C" fn release_subscription(context: *mut c_void) {
    transport::drop_boundary(|| unsafe {
        Arc::decrement_strong_count(context.cast::<Subscription>())
    });
}

unsafe extern "C" fn subscription_snapshot(context: *mut c_void) -> abi::Reply {
    transport::reply_boundary(|| {
        let subscription = unsafe { subscription(context)? };
        let _owner = subscription.state.owner()?;
        let mut receiver = subscription
            .receiver
            .lock()
            .map_err(|_| Failure::failed("progress subscription poisoned"))?;
        let mut observed = receiver.clone();
        let snapshot = observed.borrow_and_update().clone();
        let bytes = progress::encode_snapshot(&snapshot).map_err(Failure::from)?;
        // Preserve the exact observed watch version even if the owner publishes
        // during encoding. Failed encoding leaves the observation untouched.
        *receiver = observed;
        Ok(bytes)
    })
}

struct WaitGuard(Arc<Subscription>);
impl Drop for WaitGuard {
    fn drop(&mut self) {
        self.0.waiting.store(false, Ordering::Release);
    }
}

unsafe extern "C" fn wait(context: *mut c_void, out: *mut abi::OperationHandle) -> abi::Status {
    transport::status_boundary(|| {
        if out.is_null() {
            return Err(Failure::protocol("null progress wait output"));
        }

        let subscription = unsafe { subscription(context)? };
        subscription.state.owner()?;
        subscription
            .waiting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Failure::new(abi::status::BUSY, "progress wait already active"))?;
        unsafe { Arc::increment_strong_count(context.cast::<Subscription>()) };
        let guard = WaitGuard(unsafe { Arc::from_raw(context.cast::<Subscription>()) });
        let mut receiver = subscription
            .receiver
            .lock()
            .map_err(|_| Failure::failed("progress subscription poisoned"))?
            .clone();
        let mut revoked = subscription.state.revoked.subscribe();
        let operation = transport::export_operation(
            async move {
                guard.0.state.owner()?;
                // Watch-only polling is runtime-independent. No host storage
                // future or worker is polled under the plugin's Tokio runtime.
                tokio::select! {
                    biased;
                    _ = revoked.wait_for(|revoked| *revoked) => Err(Failure::closed()),
                    result = receiver.changed() => {
                        result.map_err(|_| Failure::closed())?;
                        guard.0.state.owner()?;
                        Ok(Vec::new())
                    }
                }
            },
            None,
        );
        unsafe { transport::write_out(out, operation) }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::{
        transport::{take_reply, take_status, OperationFuture},
        NativeSourceProgress,
    };
    use drasi_lib::computation::v1::ComponentId;
    use futures_util::poll;
    use std::time::Duration;

    fn owner() -> Arc<QuerySourceProgress> {
        Arc::new(QuerySourceProgress::new("graph", ComponentId::try_new("query").unwrap()).unwrap())
    }

    fn capability(binding: &ProgressBinding) -> NativeSourceProgress {
        unsafe { NativeSourceProgress::from_borrowed(&binding.table()).unwrap() }
    }

    fn raw_subscription(binding: &ProgressBinding) -> ProgressSubscriptionV1 {
        let table = binding.table();
        let mut subscription = ProgressSubscriptionV1::null();
        unsafe { take_status(table.subscribe.unwrap()(table.context, &mut subscription)).unwrap() };
        subscription
    }

    fn operation(raw: &ProgressSubscriptionV1) -> Result<OperationFuture, Failure> {
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(raw.wait.unwrap()(raw.context, &mut operation))?;
            OperationFuture::new(operation)
        }
    }

    fn closed(error: &anyhow::Error) {
        assert_eq!(
            error.downcast_ref::<Failure>().unwrap().code,
            abi::status::CLOSED
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn progress_revocation_wakes_waiters_and_fences_retained_readers() -> anyhow::Result<()> {
        let owner = owner();
        let binding = ProgressBinding::new(&owner);
        let handle = capability(&binding);
        let reader = handle.reader();
        assert!(reader.local_owner().is_none());
        assert!(reader.same_owner(&handle.clone().reader()));
        assert_eq!(reader.graph_id(), "graph");
        assert_eq!(reader.component_id(), owner.component_id());
        let mut updates = reader.subscribe()?;
        assert!(!updates.snapshot()?.recovered);
        let mut pending = Box::pin(updates.changed());
        assert!(poll!(pending.as_mut()).is_pending());
        drop(binding);
        let error = tokio::time::timeout(Duration::from_secs(1), pending)
            .await?
            .unwrap_err();
        closed(&error);
        closed(&updates.snapshot().unwrap_err());
        closed(&reader.snapshot().unwrap_err());
        assert!(reader.subscribe().is_err());
        let replacement = ProgressBinding::new(&owner);
        assert!(!capability(&replacement).reader().snapshot()?.recovered);
        closed(&reader.snapshot().unwrap_err());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn progress_wait_cancellation_preserves_observation_and_subscription_ownership(
    ) -> anyhow::Result<()> {
        let owner = owner();
        let binding = ProgressBinding::new(&owner);
        let raw = raw_subscription(&binding);
        let (sender, receiver) = watch::channel(Arc::new(SourceProgressSnapshot::default()));
        *unsafe { subscription(raw.context)? }
            .receiver
            .lock()
            .unwrap() = receiver;
        let mut pending = operation(&raw)?;
        assert!(poll!(&mut pending).is_pending());
        assert_eq!(operation(&raw).err().unwrap().code, abi::status::BUSY);
        drop(pending);
        sender.send_replace(Arc::new(SourceProgressSnapshot {
            ready: true,
            ..Default::default()
        }));
        let pending = operation(&raw)?;
        drop(pending);
        tokio::time::timeout(Duration::from_secs(1), operation(&raw)?).await??;
        // A completed wait also leaves the state unobserved until snapshot.
        tokio::time::timeout(Duration::from_secs(1), operation(&raw)?).await??;
        let bytes = unsafe { take_reply(raw.snapshot.unwrap()(raw.context))? };
        assert!(progress::decode_snapshot(&bytes)?.ready);
        let mut pending = operation(&raw)?;
        assert!(poll!(&mut pending).is_pending());
        unsafe { raw.release.unwrap()(raw.context) };
        assert_eq!(binding.0.subscriptions.load(Ordering::Acquire), 1);
        drop(pending);
        assert_eq!(binding.0.subscriptions.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[test]
    fn progress_subscription_capacity_and_owner_closure_fail_explicitly() -> anyhow::Result<()> {
        let owner = owner();
        let binding = ProgressBinding::new(&owner);
        let reader = capability(&binding).reader();
        let mut subscriptions = Vec::new();
        for _ in 0..MAX_SUBSCRIPTIONS {
            subscriptions.push(reader.subscribe()?);
        }
        assert_eq!(
            reader
                .subscribe()
                .err()
                .unwrap()
                .downcast_ref::<Failure>()
                .unwrap()
                .code,
            abi::status::BUSY
        );
        subscriptions.pop();
        subscriptions.push(reader.subscribe()?);
        drop(owner);
        closed(&reader.snapshot().unwrap_err());
        closed(&subscriptions[0].snapshot().unwrap_err());
        assert!(reader.subscribe().is_err());
        drop(subscriptions);
        assert_eq!(binding.0.subscriptions.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn progress_failed_snapshot_does_not_consume_a_publication() -> anyhow::Result<()> {
        let owner = owner();
        let binding = ProgressBinding::new(&owner);
        let raw = raw_subscription(&binding);
        let (sender, receiver) = watch::channel(Arc::new(SourceProgressSnapshot::default()));
        *unsafe { subscription(raw.context)? }
            .receiver
            .lock()
            .unwrap() = receiver;
        sender.send_replace(Arc::new(SourceProgressSnapshot {
            failure: Some("x".repeat(MAX_PROGRESS_BYTES + 1).into()),
            ..Default::default()
        }));
        assert!(unsafe { take_reply(raw.snapshot.unwrap()(raw.context)) }.is_err());
        tokio::time::timeout(Duration::from_secs(1), operation(&raw)?).await??;
        sender.send_replace(Arc::new(SourceProgressSnapshot {
            reset_generation: 7,
            ..Default::default()
        }));
        let bytes = unsafe { take_reply(raw.snapshot.unwrap()(raw.context))? };
        assert_eq!(progress::decode_snapshot(&bytes)?.reset_generation, 7);
        let mut pending = operation(&raw)?;
        assert!(poll!(&mut pending).is_pending());
        drop(pending);
        unsafe { raw.release.unwrap()(raw.context) };
        Ok(())
    }

    #[test]
    fn progress_observation_keeps_publications_after_the_copied_watch_version() {
        let (sender, mut receiver) = watch::channel(Arc::new(SourceProgressSnapshot::default()));
        let mut observed = receiver.clone();
        let snapshot = observed.borrow_and_update().clone();
        sender.send_replace(Arc::new(SourceProgressSnapshot {
            ready: true,
            ..Default::default()
        }));
        progress::encode_snapshot(&snapshot).unwrap();
        receiver = observed;
        assert!(receiver.has_changed().unwrap());
        assert!(!snapshot.ready);
        assert!(receiver.borrow_and_update().ready);
        assert!(!receiver.has_changed().unwrap());
    }
}
