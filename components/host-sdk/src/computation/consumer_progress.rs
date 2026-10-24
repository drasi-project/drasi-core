// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    ffi::c_void,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
};

use drasi_computation_plugin_sdk::{
    abi::{self, consumer_progress::*},
    consumer_progress::{self as wire, ProgressIdentity},
    transport::{self, Failure},
};
use drasi_lib::computation::v1::{
    ComponentId, ConsumerProgressSet, ConsumerProgressSnapshot, SourceProgressSnapshot,
};
use futures_util::{stream::FuturesUnordered, StreamExt};
use tokio::sync::watch;

#[derive(Clone, Copy, Default)]
struct Lifecycle {
    generation: u64,
    active: bool,
    cleanup_required: bool,
    revoked: bool,
}

struct State {
    owner: Weak<ConsumerProgressSet>,
    identity: ProgressIdentity,
    description: Vec<u8>,
    lifecycle: watch::Sender<Lifecycle>,
    subscriptions: AtomicUsize,
}

impl State {
    fn owner(&self) -> Result<Arc<ConsumerProgressSet>, Failure> {
        if self.lifecycle.borrow().revoked {
            return Err(Failure::closed());
        }
        self.owner.upgrade().ok_or_else(Failure::closed)
    }

    fn active(&self) -> Result<(Arc<ConsumerProgressSet>, u64), Failure> {
        let lifecycle = *self.lifecycle.borrow();
        if !lifecycle.active || lifecycle.revoked {
            return Err(Failure::closed());
        }
        Ok((self.owner()?, lifecycle.generation))
    }

    fn at_generation(&self, generation: u64) -> Result<Arc<ConsumerProgressSet>, Failure> {
        let (owner, current) = self.active()?;
        if current != generation {
            return Err(Failure::closed());
        }
        Ok(owner)
    }

    fn deactivate(&self) {
        self.lifecycle.send_modify(|state| {
            state.active = false;
            state.cleanup_required = true;
        });
    }
}

pub(super) struct ConsumerProgressBinding(Arc<State>);

impl ConsumerProgressBinding {
    pub(super) fn new(owner: &Arc<ConsumerProgressSet>) -> anyhow::Result<Self> {
        let identity = ProgressIdentity {
            version: VERSION,
            graph_id: owner.graph_id().into(),
            producer_id: owner.producer_id().clone(),
            consumers: owner.owners().keys().cloned().collect(),
        };
        identity.validate()?;
        let description = wire::encode(&identity)?;
        Ok(Self(Arc::new(State {
            owner: Arc::downgrade(owner),
            identity,
            description,
            lifecycle: watch::channel(Lifecycle::default()).0,
            subscriptions: AtomicUsize::new(0),
        })))
    }

    pub(super) fn activate(&self) -> anyhow::Result<Activation> {
        self.0.owner()?;
        let mut result = Ok(());
        self.0.lifecycle.send_if_modified(|state| {
            if state.revoked || state.active || state.cleanup_required {
                result = Err(anyhow::anyhow!(
                    "consumer progress activation requires successful prior cleanup"
                ));
                return false;
            }
            let Some(generation) = state.generation.checked_add(1) else {
                result = Err(anyhow::anyhow!("consumer progress generation exhausted"));
                return false;
            };
            state.generation = generation;
            state.active = true;
            state.cleanup_required = true;
            true
        });
        result?;
        Ok(Activation {
            state: self.0.clone(),
            completed: false,
        })
    }

    pub(super) fn deactivate(&self) {
        self.0.deactivate();
    }

    pub(super) fn stopped(&self) -> anyhow::Result<()> {
        let mut result = Ok(());
        self.0.lifecycle.send_if_modified(|state| {
            if state.active || state.revoked {
                result = Err(anyhow::anyhow!("consumer progress cleanup was not fenced"));
                return false;
            }
            state.cleanup_required = false;
            true
        });
        result
    }

    pub(super) fn table(&self) -> ConsumerProgressV1 {
        ConsumerProgressV1 {
            header: abi::Header::new::<ConsumerProgressV1>(),
            context: Arc::as_ptr(&self.0) as *mut c_void,
            retain: Some(retain),
            release: Some(release),
            describe: Some(describe),
            snapshot: Some(snapshot),
            subscribe: Some(subscribe),
        }
    }
}

impl Drop for ConsumerProgressBinding {
    fn drop(&mut self) {
        self.0.lifecycle.send_modify(|state| {
            state.revoked = true;
            state.active = false;
            state.cleanup_required = true;
        });
    }
}

pub(super) struct Activation {
    state: Arc<State>,
    completed: bool,
}
impl Activation {
    pub(super) fn complete(mut self) {
        self.completed = true;
    }
}
impl Drop for Activation {
    fn drop(&mut self) {
        if !self.completed {
            self.state.deactivate();
        }
    }
}

unsafe fn state<'a>(context: *mut c_void) -> Result<&'a State, Failure> {
    if context.is_null() {
        return Err(Failure::protocol("null consumer progress context"));
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
        let state = unsafe { state(context)? };
        state.owner()?;
        Ok(state.description.clone())
    })
}
unsafe extern "C" fn snapshot(context: *mut c_void) -> abi::Reply {
    transport::reply_boundary(|| {
        let state = unsafe { state(context)? };
        let (owner, generation) = state.active()?;
        let snapshot = owner.read_only().snapshot().map_err(Failure::from)?;
        let bytes = wire::encode_snapshot(&snapshot, &state.identity).map_err(Failure::from)?;
        state.at_generation(generation)?;
        Ok(bytes)
    })
}

type Receivers = BTreeMap<ComponentId, watch::Receiver<Arc<SourceProgressSnapshot>>>;

struct Subscription {
    state: Arc<State>,
    generation: u64,
    receivers: Mutex<Receivers>,
    waiting: AtomicBool,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        self.state.subscriptions.fetch_sub(1, Ordering::AcqRel);
    }
}

unsafe extern "C" fn subscribe(
    context: *mut c_void,
    out: *mut ConsumerProgressSubscriptionV1,
) -> abi::Status {
    transport::status_boundary(|| {
        if out.is_null() {
            return Err(Failure::protocol(
                "null consumer progress subscription output",
            ));
        }
        let state = unsafe { state(context)? };
        let (owner, generation) = state.active()?;
        state
            .subscriptions
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_SUBSCRIPTIONS).then_some(count + 1)
            })
            .map_err(|_| Failure::new(abi::status::BUSY, "consumer progress subscription limit"))?;
        unsafe { Arc::increment_strong_count(context.cast::<State>()) };
        let subscription = Arc::new(Subscription {
            state: unsafe { Arc::from_raw(context.cast::<State>()) },
            generation,
            receivers: Mutex::new(
                owner
                    .owners()
                    .iter()
                    .map(|(id, owner)| (id.clone(), owner.subscribe()))
                    .collect(),
            ),
            waiting: AtomicBool::new(false),
        });
        state.at_generation(generation)?;
        unsafe {
            transport::write_out(
                out,
                ConsumerProgressSubscriptionV1 {
                    header: abi::Header::new::<ConsumerProgressSubscriptionV1>(),
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
        return Err(Failure::protocol("null consumer progress subscription"));
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
        subscription.state.at_generation(subscription.generation)?;
        let mut receivers = subscription
            .receivers
            .lock()
            .map_err(|_| Failure::failed("consumer progress subscription poisoned"))?;
        let mut observed = receivers.clone();
        let snapshot = ConsumerProgressSnapshot {
            consumers: observed
                .iter_mut()
                .map(|(id, receiver)| (id.clone(), receiver.borrow_and_update().clone()))
                .collect(),
        };
        let bytes = wire::encode_snapshot(&snapshot, &subscription.state.identity)
            .map_err(Failure::from)?;
        subscription.state.at_generation(subscription.generation)?;
        // Commit only the versions successfully returned, not newer publications
        // or a partial observation when any member exceeds the wire bounds.
        *receivers = observed;
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
            return Err(Failure::protocol("null consumer progress wait output"));
        }
        let subscription = unsafe { subscription(context)? };
        subscription.state.at_generation(subscription.generation)?;
        subscription
            .waiting
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                Failure::new(abi::status::BUSY, "consumer progress wait already active")
            })?;
        unsafe { Arc::increment_strong_count(context.cast::<Subscription>()) };
        let guard = WaitGuard(unsafe { Arc::from_raw(context.cast::<Subscription>()) });
        let receivers = subscription
            .receivers
            .lock()
            .map_err(|_| Failure::failed("consumer progress subscription poisoned"))?
            .clone();
        let mut lifecycle = subscription.state.lifecycle.subscribe();
        let operation = transport::export_operation(
            async move {
                guard.0.state.at_generation(guard.0.generation)?;
                let mut waits: FuturesUnordered<_> = receivers
                    .into_values()
                    .map(|mut receiver| async move {
                        receiver.changed().await.map_err(|_| Failure::closed())
                    })
                    .collect();
                // Watch-only waits need no runtime entry, worker or forwarding task.
                tokio::select! {
                    biased;
                    _ = lifecycle.wait_for(|state| state.revoked || !state.active
                        || state.generation != guard.0.generation) => Err(Failure::closed()),
                    result = waits.next() => {
                        result.ok_or_else(|| Failure::protocol("empty consumer progress subscription"))??;
                        guard.0.state.at_generation(guard.0.generation)?;
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
        NativeConsumerProgress,
    };
    use drasi_lib::computation::v1::QuerySourceProgress;
    use futures_util::poll;
    use std::time::Duration;

    fn id(name: &str) -> ComponentId {
        ComponentId::try_new(name).unwrap()
    }
    fn owner() -> Arc<ConsumerProgressSet> {
        Arc::new(
            ConsumerProgressSet::new(
                "graph",
                id("producer"),
                ["first", "second"]
                    .map(|name| Arc::new(QuerySourceProgress::new("graph", id(name)).unwrap())),
            )
            .unwrap(),
        )
    }
    fn capability(binding: &ConsumerProgressBinding) -> NativeConsumerProgress {
        unsafe { NativeConsumerProgress::from_borrowed(&binding.table()).unwrap() }
    }
    fn raw_subscription(binding: &ConsumerProgressBinding) -> ConsumerProgressSubscriptionV1 {
        let table = binding.table();
        let mut raw = ConsumerProgressSubscriptionV1::null();
        unsafe { take_status(table.subscribe.unwrap()(table.context, &mut raw)).unwrap() };
        raw
    }
    fn operation(raw: &ConsumerProgressSubscriptionV1) -> Result<OperationFuture, Failure> {
        let mut operation = abi::OperationHandle::null();
        unsafe {
            take_status(raw.wait.unwrap()(raw.context, &mut operation))?;
            OperationFuture::new(operation)
        }
    }
    fn observed(
        raw: &ConsumerProgressSubscriptionV1,
        binding: &ConsumerProgressBinding,
    ) -> ConsumerProgressSnapshot {
        let bytes = unsafe { take_reply(raw.snapshot.unwrap()(raw.context)).unwrap() };
        wire::decode_snapshot(&bytes, &binding.0.identity).unwrap()
    }
    fn closed(error: &anyhow::Error) {
        assert_eq!(
            error.downcast_ref::<Failure>().unwrap().code,
            abi::status::CLOSED
        );
    }
    fn replace_receivers(raw: &ConsumerProgressSubscriptionV1, receivers: Receivers) {
        *unsafe { subscription(raw.context).unwrap() }
            .receivers
            .lock()
            .unwrap() = receivers;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consumer_progress_lifecycle_fences_old_subscriptions_and_retained_readers(
    ) -> anyhow::Result<()> {
        let owner = owner();
        let binding = ConsumerProgressBinding::new(&owner)?;
        let handle = capability(&binding);
        let reader = handle.reader();
        assert_eq!(handle.consumer_ids(), &[id("first"), id("second")]);
        assert!(reader.same_binding(&handle.clone().reader()));
        assert_eq!(reader.graph_id(), "graph");
        assert_eq!(reader.producer_id(), owner.producer_id());
        closed(&reader.snapshot().unwrap_err());
        assert!(reader.subscribe().is_err());
        binding.activate()?.complete();
        let mut old = reader.subscribe()?;
        let snapshot = old.snapshot()?;
        assert_eq!(
            snapshot.consumers.keys().collect::<Vec<_>>(),
            owner.owners().keys().collect::<Vec<_>>()
        );
        assert!(!snapshot.ready() && !snapshot.recovered());
        let mut pending = Box::pin(old.changed());
        assert!(poll!(pending.as_mut()).is_pending());
        binding.deactivate();
        assert!(
            binding.activate().is_err(),
            "cleanup must succeed before restart"
        );
        binding.stopped()?;
        binding.activate()?.complete();
        closed(
            &tokio::time::timeout(Duration::from_secs(1), pending)
                .await?
                .unwrap_err(),
        );
        closed(&old.snapshot().unwrap_err());
        closed(&old.changed().await.unwrap_err());
        let mut new = reader.subscribe()?;
        new.snapshot()?;
        let mut pending = Box::pin(new.changed());
        assert!(poll!(pending.as_mut()).is_pending());
        drop(binding);
        closed(
            &tokio::time::timeout(Duration::from_secs(1), pending)
                .await?
                .unwrap_err(),
        );
        assert_eq!(handle.consumer_ids(), &[id("first"), id("second")]);
        closed(&reader.snapshot().unwrap_err());
        assert!(reader.subscribe().is_err());
        let replacement = ConsumerProgressBinding::new(&owner)?;
        replacement.activate()?.complete();
        capability(&replacement).reader().snapshot()?;
        closed(&reader.snapshot().unwrap_err());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consumer_progress_cancelled_activation_requires_cleanup_and_wakes_waiters(
    ) -> anyhow::Result<()> {
        let owner = owner();
        let binding = ConsumerProgressBinding::new(&owner)?;
        let activation = binding.activate()?;
        let mut updates = capability(&binding).reader().subscribe()?;
        updates.snapshot()?;
        let mut wait = Box::pin(updates.changed());
        assert!(poll!(wait.as_mut()).is_pending());
        drop(activation);
        closed(
            &tokio::time::timeout(Duration::from_secs(1), wait)
                .await?
                .unwrap_err(),
        );
        assert!(binding.activate().is_err());
        binding.stopped()?;
        binding.activate()?.complete();
        closed(&updates.snapshot().unwrap_err());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consumer_progress_waits_are_independent_cancellable_and_retain_their_lease(
    ) -> anyhow::Result<()> {
        let owner = owner();
        let binding = ConsumerProgressBinding::new(&owner)?;
        binding.activate()?.complete();
        let a = raw_subscription(&binding);
        let b = raw_subscription(&binding);
        let (first, first_rx) = watch::channel(Arc::new(SourceProgressSnapshot::default()));
        let (second, second_rx) = watch::channel(Arc::new(SourceProgressSnapshot::default()));
        let receivers = BTreeMap::from([(id("first"), first_rx), (id("second"), second_rx)]);
        replace_receivers(&a, receivers.clone());
        replace_receivers(&b, receivers);
        let mut pending = operation(&a)?;
        assert!(poll!(&mut pending).is_pending());
        assert_eq!(operation(&a).err().unwrap().code, abi::status::BUSY);
        drop(pending);
        first.send_replace(Arc::new(SourceProgressSnapshot {
            ready: true,
            ..Default::default()
        }));
        drop(operation(&a)?);
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(1), operation(&a)?).await??;
        }
        assert!(observed(&a, &binding).consumers[&id("first")].ready);
        let mut pending = operation(&a)?;
        assert!(poll!(&mut pending).is_pending());
        tokio::time::timeout(Duration::from_secs(1), operation(&b)?).await??;
        assert!(!observed(&b, &binding).ready());
        second.send_replace(Arc::new(SourceProgressSnapshot {
            reset_generation: 3,
            ..Default::default()
        }));
        tokio::time::timeout(Duration::from_secs(1), pending).await??;
        tokio::time::timeout(Duration::from_secs(1), operation(&b)?).await??;
        assert_eq!(
            observed(&a, &binding).consumers[&id("second")].reset_generation,
            3
        );
        observed(&b, &binding);
        let mut pending = operation(&a)?;
        assert!(poll!(&mut pending).is_pending());
        unsafe {
            a.release.unwrap()(a.context);
            b.release.unwrap()(b.context);
        }
        assert_eq!(binding.0.subscriptions.load(Ordering::Acquire), 1);
        drop(pending);
        assert_eq!(binding.0.subscriptions.load(Ordering::Acquire), 0);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn consumer_progress_failed_whole_set_encoding_consumes_no_member() -> anyhow::Result<()>
    {
        let owner = owner();
        let binding = ConsumerProgressBinding::new(&owner)?;
        binding.activate()?.complete();
        let raw = raw_subscription(&binding);
        let (first, first_rx) = watch::channel(Arc::new(SourceProgressSnapshot::default()));
        let (second, second_rx) = watch::channel(Arc::new(SourceProgressSnapshot::default()));
        replace_receivers(
            &raw,
            BTreeMap::from([(id("first"), first_rx), (id("second"), second_rx)]),
        );
        let large = Arc::new(SourceProgressSnapshot {
            failure: Some("x".repeat(MAX_PROGRESS_BYTES / 2).into()),
            ..Default::default()
        });
        first.send_replace(large.clone());
        second.send_replace(large);
        assert!(unsafe { take_reply(raw.snapshot.unwrap()(raw.context)) }.is_err());
        let actual = unsafe { subscription(raw.context)? };
        assert!(actual
            .receivers
            .lock()
            .unwrap()
            .values()
            .all(|receiver| receiver.has_changed().unwrap()));
        tokio::time::timeout(Duration::from_secs(1), operation(&raw)?).await??;
        second.send_replace(Arc::new(SourceProgressSnapshot {
            reset_generation: 9,
            ..Default::default()
        }));
        let snapshot = observed(&raw, &binding);
        assert_eq!(snapshot.consumers[&id("second")].reset_generation, 9);
        assert!(snapshot.consumers[&id("first")].failure.is_some());
        let mut pending = operation(&raw)?;
        assert!(poll!(&mut pending).is_pending());
        drop(pending);
        unsafe { raw.release.unwrap()(raw.context) };
        Ok(())
    }

    #[test]
    fn consumer_progress_capacity_ownership_and_malformed_capabilities_fail_explicitly(
    ) -> anyhow::Result<()> {
        let owner = owner();
        let binding = ConsumerProgressBinding::new(&owner)?;
        for field in 0..7 {
            let mut table = binding.table();
            match field {
                0 => table.header.size += 1,
                1 => table.context = std::ptr::null_mut(),
                2 => table.retain = None,
                3 => table.release = None,
                4 => table.describe = None,
                5 => table.snapshot = None,
                _ => table.subscribe = None,
            }
            assert!(unsafe { NativeConsumerProgress::from_borrowed(&table) }.is_err());
            assert_eq!(Arc::strong_count(&binding.0), 1);
        }
        let capability = capability(&binding);
        let reader = capability.reader();
        drop(capability);
        assert_eq!(Arc::strong_count(&binding.0), 2);
        binding.activate()?.complete();
        let mut subscriptions = (0..MAX_SUBSCRIPTIONS)
            .map(|_| reader.subscribe())
            .collect::<anyhow::Result<Vec<_>>>()?;
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
        binding.deactivate();
        binding.stopped()?;
        binding.activate()?.complete();
        assert!(
            reader.subscribe().is_err(),
            "fenced but retained subscriptions still consume their bounded slots"
        );
        drop(subscriptions);
        reader.subscribe()?;
        drop(owner);
        closed(&reader.snapshot().unwrap_err());
        assert!(reader.subscribe().is_err());
        drop(reader);
        assert_eq!(binding.0.subscriptions.load(Ordering::Acquire), 0);
        assert_eq!(Arc::strong_count(&binding.0), 1);
        Ok(())
    }

    #[test]
    fn consumer_progress_invalid_descriptions_do_not_retain_and_partial_subscriptions_release_once(
    ) -> anyhow::Result<()> {
        unsafe extern "C" fn invalid_description(_: *mut c_void) -> abi::Reply {
            transport::reply_boundary(|| {
                wire::encode(&ProgressIdentity {
                    version: VERSION,
                    graph_id: "graph".into(),
                    producer_id: id("producer"),
                    consumers: vec![id("first"), id("first")],
                })
                .map_err(Failure::from)
            })
        }
        unsafe extern "C" fn partial_subscription(
            context: *mut c_void,
            out: *mut ConsumerProgressSubscriptionV1,
        ) -> abi::Status {
            let status = unsafe { subscribe(context, out) };
            if status.code == abi::status::OK {
                unsafe { (*out).snapshot = None };
            }
            status
        }
        let owner = owner();
        let binding = ConsumerProgressBinding::new(&owner)?;
        let mut raw = binding.table();
        raw.describe = Some(invalid_description);
        assert!(unsafe { NativeConsumerProgress::from_borrowed(&raw) }.is_err());
        assert_eq!(Arc::strong_count(&binding.0), 1);
        binding.activate()?.complete();
        let mut raw = binding.table();
        raw.subscribe = Some(partial_subscription);
        let capability = unsafe { NativeConsumerProgress::from_borrowed(&raw)? };
        assert!(capability.reader().subscribe().is_err());
        assert_eq!(binding.0.subscriptions.load(Ordering::Acquire), 0);
        assert_eq!(Arc::strong_count(&binding.0), 2);
        drop(capability);
        assert_eq!(Arc::strong_count(&binding.0), 1);
        Ok(())
    }
}
