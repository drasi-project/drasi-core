// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::{GraphChangeCodec, QuerySourceProgress, StreamId};
use drasi_core::interface::StorageDurability;
use futures::{poll, StreamExt, TryStreamExt};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, AtomicUsize},
};
use tokio::sync::Notify;

fn component(name: &str) -> ComponentId {
    ComponentId::try_new(name).expect("component")
}
fn resource(name: &str) -> ResourceId {
    ResourceId::try_new(name).expect("resource")
}
fn owner() -> Arc<QuerySourceProgress> {
    Arc::new(QuerySourceProgress::new("graph", component("query")).expect("owner"))
}

#[derive(Default)]
struct Stored {
    value: Mutex<Option<Bytes>>,
    writes: AtomicUsize,
}
#[async_trait]
impl BootstrapState for Stored {
    fn durability(&self) -> StorageDurability {
        StorageDurability::LOCAL_PROCESS_RESTART
    }
    async fn read(&self) -> Result<Option<Bytes>> {
        Ok(self.value.lock().expect("state").clone())
    }
    async fn write(&self, value: Bytes) -> Result<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        *self.value.lock().expect("state") = Some(value);
        Ok(())
    }
}

#[derive(Default)]
struct LeafLifecycle {
    prepared: bool,
    stopped: bool,
    held: bool,
    retired: bool,
}
struct Leaf {
    name: String,
    reader: Option<SourceProgressReader>,
    outcomes: Mutex<VecDeque<BootstrapPreparation>>,
    lifecycle: Arc<Mutex<LeafLifecycle>>,
    seen: Mutex<Vec<Option<Bytes>>>,
    intent: Option<Bytes>,
    completion: Option<Bytes>,
    polls: Arc<AtomicUsize>,
    stops: AtomicUsize,
    snapshot_failure: AtomicBool,
    stop_failure: AtomicBool,
    freeze_failure: AtomicBool,
    block_open: AtomicBool,
    block_prepare: AtomicBool,
    block_complete: AtomicBool,
    complete_failure: AtomicBool,
    block_rows: AtomicBool,
    release: Arc<Notify>,
    opened: Notify,
}
impl Leaf {
    fn new(name: &str, reader: Option<SourceProgressReader>, metadata: bool) -> Arc<Self> {
        Arc::new(Self {
            name: name.into(),
            reader,
            outcomes: Mutex::new(VecDeque::new()),
            lifecycle: Arc::new(Mutex::new(LeafLifecycle::default())),
            seen: Mutex::new(Vec::new()),
            intent: metadata.then(|| Bytes::from(format!("{name}:intent"))),
            completion: metadata.then(|| Bytes::from(format!("{name}:complete"))),
            polls: Arc::new(AtomicUsize::new(0)),
            stops: AtomicUsize::new(0),
            snapshot_failure: AtomicBool::new(false),
            stop_failure: AtomicBool::new(false),
            freeze_failure: AtomicBool::new(false),
            block_open: AtomicBool::new(false),
            block_prepare: AtomicBool::new(false),
            block_complete: AtomicBool::new(false),
            complete_failure: AtomicBool::new(false),
            block_rows: AtomicBool::new(false),
            release: Arc::new(Notify::new()),
            opened: Notify::new(),
        })
    }
    async fn prepare_inner(
        &self,
        state: Option<&dyn BootstrapState>,
    ) -> Result<BootstrapPreparation> {
        let outcome = self
            .outcomes
            .lock()
            .expect("outcomes")
            .pop_front()
            .unwrap_or(BootstrapPreparation::Ready);
        {
            let mut lifecycle = self.lifecycle.lock().expect("lifecycle");
            anyhow::ensure!(
                !lifecycle.prepared && !lifecycle.held && !lifecycle.retired,
                "leaf requires cleanup"
            );
            lifecycle.prepared = outcome == BootstrapPreparation::Ready;
            lifecycle.stopped = false;
        }
        if let Some(state) = state {
            assert_eq!(state.durability(), StorageDurability::LOCAL_PROCESS_RESTART);
            let seen = state.read().await?;
            self.seen.lock().expect("seen").push(seen);
            if let Some(intent) = &self.intent {
                state.write(intent.clone()).await?;
            }
        }
        if self.block_prepare.load(Ordering::Acquire) {
            self.opened.notify_one();
            self.release.notified().await;
        }
        Ok(outcome)
    }
}
struct LeafLease {
    lifecycle: Arc<Mutex<LeafLifecycle>>,
    resumed: bool,
}
impl BootstrapRetirement for LeafLease {
    fn resume(mut self: Box<Self>) {
        self.lifecycle.lock().expect("lifecycle").held = false;
        self.resumed = true;
    }
    fn retire(self: Box<Self>) {
        drop(self);
    }
}
impl Drop for LeafLease {
    fn drop(&mut self) {
        if !self.resumed {
            self.lifecycle.lock().expect("lifecycle").retired = true;
        }
    }
}
#[async_trait]
impl ComputationBootstrapProvider for Leaf {
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        anyhow::ensure!(
            instance == "instance" && graph == "graph" && query == &component("query"),
            "wrong scope"
        );
        Ok(())
    }
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.reader.clone()
    }
    async fn prepare(&self) -> Result<BootstrapPreparation> {
        self.prepare_inner(None).await
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.prepare_inner(Some(state)).await
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.opened.notify_one();
        if self.block_open.load(Ordering::Acquire) {
            self.release.notified().await;
        }
        if self.snapshot_failure.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "typed snapshot failure",
            )
            .into());
        }
        let name = self.name.clone();
        let polls = self.polls.clone();
        let release = self.release.clone();
        let blocked = self.block_rows.load(Ordering::Acquire);
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::try_unfold(0, move |ordinal| {
                let name = name.clone();
                let polls = polls.clone();
                let release = release.clone();
                async move {
                    if blocked {
                        release.notified().await;
                    }
                    polls.fetch_add(1, Ordering::SeqCst);
                    if ordinal == 2 {
                        return Result::<_>::Ok(None);
                    }
                    let envelope = GraphChangeCodec::encode_changes(
                        &[],
                        StreamId::try_new(name)?,
                        ordinal + 1,
                        None,
                    )?;
                    Ok(Some((envelope, ordinal + 1)))
                }
            })),
            watermarks: Vec::new(),
        })
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        if self.block_complete.load(Ordering::Acquire) {
            self.opened.notify_one();
            self.release.notified().await;
        }
        if self.complete_failure.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "typed completion failure",
            )
            .into());
        }
        Ok(vec![BootstrapWatermark {
            stream: StreamId::try_new(self.name.as_str())?,
            source_id: Some(self.name.clone()),
            sequence: 2,
            position: Some(Bytes::from(self.name.clone())),
        }])
    }
    fn completion_state(&self) -> Result<Option<Bytes>> {
        Ok(self.completion.clone())
    }
    async fn stop(&self) -> Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.stop_failure.load(Ordering::Acquire) {
            return Err(
                std::io::Error::new(std::io::ErrorKind::TimedOut, "typed stop failure").into(),
            );
        }
        let mut lifecycle = self.lifecycle.lock().expect("lifecycle");
        lifecycle.prepared = false;
        lifecycle.stopped = true;
        Ok(())
    }
    fn freeze_for_retirement(&self) -> Result<Box<dyn BootstrapRetirement>> {
        anyhow::ensure!(
            !self.freeze_failure.load(Ordering::Acquire),
            "freeze rejected"
        );
        let mut lifecycle = self.lifecycle.lock().expect("lifecycle");
        anyhow::ensure!(
            lifecycle.stopped && !lifecycle.held && !lifecycle.retired,
            "leaf is not stopped"
        );
        lifecycle.held = true;
        Ok(Box::new(LeafLease {
            lifecycle: self.lifecycle.clone(),
            resumed: false,
        }))
    }
}

fn compose(leaves: &[Arc<Leaf>]) -> Result<CompositeBootstrapProvider> {
    CompositeBootstrapProvider::new(
        "instance",
        "graph",
        component("query"),
        leaves
            .iter()
            .map(|leaf| {
                (
                    resource(&leaf.name),
                    leaf.clone() as Arc<dyn ComputationBootstrapProvider>,
                )
            })
            .collect(),
    )
}

#[tokio::test]
async fn cancelled_prepare_and_completion_require_owned_cleanup_before_restart() -> Result<()> {
    for preparing in [true, false] {
        let first = Leaf::new("first", None, true);
        let second = Leaf::new("second", None, true);
        let provider = compose(&[first.clone(), second.clone()])?;
        let state = Stored::default();
        if preparing {
            second.block_prepare.store(true, Ordering::Release);
            let mut operation = Box::pin(provider.prepare_with_state(&state));
            assert!(poll!(operation.as_mut()).is_pending());
            drop(operation);
        } else {
            provider.prepare_with_state(&state).await?;
            provider
                .snapshot_with_state(&state)
                .await?
                .changes
                .try_collect::<Vec<_>>()
                .await?;
            second.block_complete.store(true, Ordering::Release);
            let mut operation = Box::pin(provider.complete_snapshot());
            assert!(poll!(operation.as_mut()).is_pending());
            drop(operation);
        }
        let intent = state.read().await?;
        assert!(provider.completion_state().is_err());
        assert!(provider.prepare_with_state(&state).await.is_err());
        assert!(provider.freeze_for_retirement().is_err());
        provider.stop().await?;
        assert_eq!(first.stops.load(Ordering::Acquire), 1);
        assert_eq!(second.stops.load(Ordering::Acquire), 1);
        assert_eq!(state.read().await?, intent);
        second.block_prepare.store(false, Ordering::Release);
        second.block_complete.store(false, Ordering::Release);
        provider.prepare_with_state(&state).await?;
        provider
            .snapshot_with_state(&state)
            .await?
            .changes
            .try_collect::<Vec<_>>()
            .await?;
        assert_eq!(provider.complete_snapshot().await?.len(), 2);
        assert!(provider.completion_state()?.is_some());
        assert_eq!(
            state.read().await?,
            intent,
            "completion must remain in the caller's transaction"
        );
        provider.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn partial_completion_keeps_all_metadata_uncommitted_and_preserves_the_typed_error(
) -> Result<()> {
    let first = Leaf::new("first", None, true);
    let second = Leaf::new("second", None, true);
    second.complete_failure.store(true, Ordering::Release);
    let provider = compose(&[first.clone(), second.clone()])?;
    let state = Stored::default();
    provider.prepare_with_state(&state).await?;
    let intent = state.read().await?;
    provider
        .snapshot_with_state(&state)
        .await?
        .changes
        .try_collect::<Vec<_>>()
        .await?;
    let error = provider
        .complete_snapshot()
        .await
        .err()
        .context("second completion must fail")?;
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .expect("typed cause")
            .kind(),
        std::io::ErrorKind::ConnectionReset
    );
    assert_eq!(state.read().await?, intent);
    assert!(provider.completion_state().is_err());
    assert_eq!(first.stops.load(Ordering::Acquire), 1);
    assert_eq!(second.stops.load(Ordering::Acquire), 1);
    Ok(())
}

#[test]
fn scope_membership_and_real_reader_identity_are_required() -> Result<()> {
    let actual = owner();
    let first = Leaf::new("first", Some(actual.clone().into()), false);
    let second = Leaf::new("second", Some(actual.clone().into()), false);
    let composite = compose(&[first.clone(), second])?;
    assert!(composite
        .recovery_reader()
        .expect("reader")
        .same_owner(&actual.clone().into()));
    assert!(composite
        .validate_query_scope("other", "graph", &component("query"))
        .is_err());
    assert!(compose(&[first.clone(), Leaf::new("impostor", Some(owner().into()), false)]).is_err());
    assert!(compose(&[first.clone(), first]).is_err());
    assert!(compose(&[]).is_err());
    let too_many: Vec<_> = (0..=MAX_COMPOSITE_BOOTSTRAP_PROVIDERS)
        .map(|index| Leaf::new(&format!("member-{index}"), None, false))
        .collect();
    assert!(compose(&too_many).is_err());
    Ok(())
}

async fn streamed_metadata() -> Result<()> {
    let leaves = [Leaf::new("first", None, true), Leaf::new("second", None, true)];
    let composite = compose(&leaves)?;
    let state = Stored::default();
    assert_eq!(
        composite.prepare_with_state(&state).await?,
        BootstrapPreparation::Ready
    );
    assert!(leaves
        .iter()
        .all(|leaf| leaf.seen.lock().expect("seen").as_slice() == [None]));
    let mut snapshot = composite.snapshot_with_state(&state).await?;
    assert!(leaves
        .iter()
        .all(|leaf| leaf.polls.load(Ordering::SeqCst) == 0));
    assert!(composite.complete_snapshot().await.is_err());
    let first = snapshot.changes.try_next().await?.expect("first row");
    assert_eq!(first.system().stream().as_str(), "first");
    assert_eq!(leaves[1].polls.load(Ordering::SeqCst), 0);
    let rest: Vec<_> = snapshot.changes.try_collect().await?;
    assert_eq!(
        rest.iter()
            .map(|envelope| envelope.system().stream().as_str())
            .collect::<Vec<_>>(),
        ["first", "second", "second"]
    );
    let watermarks = composite.complete_snapshot().await?;
    assert_eq!(watermarks.len(), 2);
    let completed = composite.completion_state()?.expect("completion");
    let record: StateRecord = serde_json::from_slice(&completed)?;
    assert_eq!(
        record.providers[0].state.as_deref(),
        Some(&b"first:complete"[..])
    );
    assert_eq!(
        record.providers[1].state.as_deref(),
        Some(&b"second:complete"[..])
    );
    let intent: StateRecord = serde_json::from_slice(&state.read().await?.expect("intent"))?;
    assert_eq!(
        intent.providers[0].state.as_deref(),
        Some(&b"first:intent"[..]),
        "completion remains query-transaction-owned"
    );
    state.write(completed).await?;
    composite.stop().await?;
    composite.prepare_with_state(&state).await?;
    assert_eq!(
        leaves[0].seen.lock().expect("seen")[1].as_deref(),
        Some(&b"first:complete"[..])
    );
    assert_eq!(
        leaves[1].seen.lock().expect("seen")[1].as_deref(),
        Some(&b"second:complete"[..])
    );
    composite.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn metadata_and_stream_order_current_thread() -> Result<()> {
    streamed_metadata().await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn metadata_and_stream_order_multi_thread() -> Result<()> {
    streamed_metadata().await
}

#[tokio::test]
async fn non_ready_priority_rearms_already_ready_siblings_and_retains_intents() -> Result<()> {
    for outcomes in [
        (
            BootstrapPreparation::RefreshVolatile,
            BootstrapPreparation::Ready,
            BootstrapPreparation::RefreshVolatile,
        ),
        (
            BootstrapPreparation::Ready,
            BootstrapPreparation::RefreshVolatile,
            BootstrapPreparation::RefreshVolatile,
        ),
        (
            BootstrapPreparation::RefreshVolatile,
            BootstrapPreparation::ResetRequired,
            BootstrapPreparation::ResetRequired,
        ),
    ] {
        let leaves = [Leaf::new("first", None, true), Leaf::new("second", None, true)];
        leaves[0]
            .outcomes
            .lock()
            .expect("outcomes")
            .push_back(outcomes.0);
        leaves[1]
            .outcomes
            .lock()
            .expect("outcomes")
            .push_back(outcomes.1);
        let composite = compose(&leaves)?;
        let state = Stored::default();
        assert_eq!(composite.prepare_with_state(&state).await?, outcomes.2);
        assert!(leaves
            .iter()
            .all(|leaf| leaf.stops.load(Ordering::SeqCst) == 1));
        assert_eq!(
            composite.prepare_with_state(&state).await?,
            BootstrapPreparation::Ready
        );
        assert!(composite.prepare_with_state(&state).await.is_err());
        assert_eq!(
            leaves[0].seen.lock().expect("seen")[1].as_deref(),
            Some(&b"first:intent"[..])
        );
        composite.stop().await?;
    }
    Ok(())
}

#[tokio::test]
async fn no_child_metadata_means_no_invented_persistent_completion() -> Result<()> {
    let composite = compose(&[Leaf::new("one", None, false), Leaf::new("two", None, false)])?;
    let state = Stored::default();
    composite.prepare_with_state(&state).await?;
    composite
        .snapshot_with_state(&state)
        .await?
        .changes
        .try_collect::<Vec<_>>()
        .await?;
    composite.complete_snapshot().await?;
    assert!(composite.completion_state()?.is_none());
    assert_eq!(state.writes.load(Ordering::SeqCst), 0);
    composite.stop().await
}

#[tokio::test]
async fn state_bound_and_changed_metadata_membership_never_overwrite_parent_state() -> Result<()> {
    let first = Leaf::new("first", None, false);
    let composite = compose(&[first])?;
    let state = Stored::default();
    composite.prepare_with_state(&state).await?;
    let run = composite.current()?.expect("run");
    let child = NamespacedState {
        parent: &state,
        record: &run.record,
        index: 0,
    };
    child.write(Bytes::from_static(b"safe")).await?;
    let before = state.read().await?;
    let oversized = Bytes::from(vec![255; super::super::MAX_BOOTSTRAP_STATE_BYTES]);
    assert!(
        child.write(oversized).await.is_err(),
        "record encoding has one shared bound"
    );
    assert_eq!(state.read().await?, before);
    composite.stop().await?;
    let changed = compose(&[Leaf::new("different", None, false)])?;
    assert!(changed.prepare_with_state(&state).await.is_err());
    assert_eq!(state.read().await?, before);
    changed.stop().await
}

#[tokio::test]
async fn partial_open_failure_stops_every_child_and_keeps_typed_causes() -> Result<()> {
    let leaves = [Leaf::new("first", None, false), Leaf::new("second", None, false)];
    leaves[1].snapshot_failure.store(true, Ordering::Release);
    leaves[0].stop_failure.store(true, Ordering::Release);
    let composite = compose(&leaves)?;
    composite.prepare().await?;
    let error = match composite.snapshot().await {
        Ok(_) => anyhow::bail!("snapshot unexpectedly succeeded"),
        Err(error) => error,
    };
    assert_eq!(
        error
            .downcast_ref::<std::io::Error>()
            .expect("typed snapshot error")
            .kind(),
        std::io::ErrorKind::PermissionDenied
    );
    assert!(error
        .downcast_ref::<crate::error::OperationFailures>()
        .is_some());
    assert!(leaves
        .iter()
        .all(|leaf| leaf.stops.load(Ordering::SeqCst) == 1));
    assert!(composite.prepare().await.is_err());
    leaves[0].stop_failure.store(false, Ordering::Release);
    composite.stop().await?;
    leaves[1].snapshot_failure.store(false, Ordering::Release);
    composite.prepare().await?;
    composite.stop().await
}

#[tokio::test]
async fn cancelled_snapshot_open_and_held_stream_require_owned_stop_before_restart() -> Result<()> {
    let leaves = [Leaf::new("first", None, false), Leaf::new("second", None, false)];
    leaves[1].block_open.store(true, Ordering::Release);
    let composite = compose(&leaves)?;
    composite.prepare().await?;
    let mut opening = Box::pin(composite.snapshot());
    assert!(poll!(opening.as_mut()).is_pending());
    drop(opening);
    assert!(composite.snapshot().await.is_err());
    composite.stop().await?;
    leaves[1].block_open.store(false, Ordering::Release);
    leaves[0].block_rows.store(true, Ordering::Release);
    composite.prepare().await?;
    let mut snapshot = composite.snapshot().await?;
    let mut next = Box::pin(snapshot.changes.next());
    assert!(poll!(next.as_mut()).is_pending());
    composite.stop().await?;
    assert!(next.await.expect("stopped stream").is_err());
    drop(snapshot);
    leaves[0].block_rows.store(false, Ordering::Release);
    composite.prepare().await?;
    composite.stop().await
}

#[tokio::test]
async fn early_stream_drop_and_duplicate_watermarks_cannot_complete() -> Result<()> {
    let leaves = [Leaf::new("first", None, false), Leaf::new("second", None, false)];
    let composite = compose(&leaves)?;
    composite.prepare().await?;
    let snapshot = composite.snapshot().await?;
    drop(snapshot);
    assert!(composite.complete_snapshot().await.is_err());
    assert!(composite.prepare().await.is_err());
    composite.stop().await?;
    let mut keys = BTreeSet::new();
    let mut output = Vec::new();
    append_watermarks(&mut output, leaves[0].complete_snapshot().await?, &mut keys)?;
    assert!(
        append_watermarks(&mut output, leaves[0].complete_snapshot().await?, &mut keys).is_err()
    );
    Ok(())
}

#[tokio::test]
async fn retirement_holds_actual_children_resumes_partial_rejection_and_retires_on_drop(
) -> Result<()> {
    let leaves = [Leaf::new("first", None, false), Leaf::new("second", None, false)];
    let composite = compose(&leaves)?;
    assert!(composite.freeze_for_retirement().is_err());
    composite.stop().await?;
    leaves[1].freeze_failure.store(true, Ordering::Release);
    assert!(composite.freeze_for_retirement().is_err());
    assert!(!leaves[0].lifecycle.lock().expect("lifecycle").held);
    leaves[1].freeze_failure.store(false, Ordering::Release);
    let held = composite.freeze_for_retirement()?;
    assert!(composite.prepare().await.is_err());
    held.resume();
    composite.prepare().await?;
    composite.stop().await?;
    let held = composite.freeze_for_retirement()?;
    drop(held);
    assert!(composite.prepare().await.is_err());
    assert!(leaves
        .iter()
        .all(|leaf| leaf.lifecycle.lock().expect("lifecycle").retired));
    Ok(())
}
