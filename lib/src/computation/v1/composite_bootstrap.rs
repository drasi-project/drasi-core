// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::{BTreeSet, VecDeque},
    pin::Pin,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc, Mutex,
    },
    task::{Context as TaskContext, Poll},
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use futures::{task::AtomicWaker, Stream};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Mutex as AsyncMutex};

use super::{
    query_bootstrap::validate_bootstrap_state, BootstrapPreparation, BootstrapRetirement,
    BootstrapState, BootstrapWatermark, ChangeEnvelope, ComponentId, ComputationBootstrapProvider,
    ComputationBootstrapSnapshot, ResourceCleanup, ResourceId, SourceProgressReader,
};

pub const MAX_COMPOSITE_BOOTSTRAP_PROVIDERS: usize = 32;
pub const MAX_COMPOSITE_BOOTSTRAP_WATERMARKS: usize = 256;

#[cfg(test)]
mod tests;

const PREPARING: u8 = 0;
const PREPARED: u8 = 1;
const STREAMING: u8 = 2;
const ENDED: u8 = 3;
const COMPLETED: u8 = 4;
const FAILED: u8 = 5;
type SnapshotStream = Pin<Box<dyn Stream<Item = Result<ChangeEnvelope>> + Send>>;

struct Member {
    id: ResourceId,
    provider: Arc<dyn ComputationBootstrapProvider>,
    reader: Option<SourceProgressReader>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MemberState {
    provider: ResourceId,
    state: Option<Bytes>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StateRecord {
    version: u32,
    providers: Vec<MemberState>,
}

impl StateRecord {
    fn empty(members: &[Member]) -> Self {
        Self {
            version: 1,
            providers: members
                .iter()
                .map(|member| MemberState {
                    provider: member.id.clone(),
                    state: None,
                })
                .collect(),
        }
    }

    fn encode(&self) -> Result<Bytes> {
        let bytes = Bytes::from(serde_json::to_vec(self)?);
        validate_bootstrap_state(&bytes)?;
        Ok(bytes)
    }

    async fn read(parent: Option<&dyn BootstrapState>, members: &[Member]) -> Result<Self> {
        let bytes = match parent {
            Some(parent) => parent.read().await?,
            None => None,
        };
        let Some(bytes) = bytes else {
            return Ok(Self::empty(members));
        };
        validate_bootstrap_state(&bytes)?;
        let record: Self = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            record.version == 1
                && record
                    .providers
                    .iter()
                    .map(|state| &state.provider)
                    .eq(members.iter().map(|member| &member.id)),
            "composite bootstrap handover membership/version changed"
        );
        for member in &record.providers {
            if let Some(state) = &member.state {
                validate_bootstrap_state(state)?;
            }
        }
        Ok(record)
    }
}

/// The parent's borrowed transaction authority never escapes an operation.
struct NamespacedState<'a> {
    parent: &'a dyn BootstrapState,
    record: &'a AsyncMutex<StateRecord>,
    index: usize,
}

#[async_trait]
impl BootstrapState for NamespacedState<'_> {
    fn durability(&self) -> drasi_core::interface::StorageDurability {
        self.parent.durability()
    }
    async fn read(&self) -> Result<Option<Bytes>> {
        Ok(self.record.lock().await.providers[self.index].state.clone())
    }
    async fn write(&self, bytes: Bytes) -> Result<()> {
        validate_bootstrap_state(&bytes)?;
        let mut record = self.record.lock().await;
        let mut next = record.clone();
        next.providers[self.index].state = Some(bytes);
        self.parent.write(next.encode()?).await?;
        *record = next;
        Ok(())
    }
}

struct Run {
    phase: AtomicU8,
    stopped: watch::Sender<bool>,
    streams: Mutex<VecDeque<SnapshotStream>>,
    waker: AtomicWaker,
    record: AsyncMutex<StateRecord>,
    initial_keys: Mutex<BTreeSet<String>>,
    completion: Mutex<Option<Bytes>>,
    pending: Mutex<bool>,
}

impl Run {
    fn fail(&self) {
        self.phase.store(FAILED, Ordering::Release);
        self.stopped.send_replace(true);
        self.waker.wake();
    }
}

struct Operation {
    run: Arc<Run>,
    complete: bool,
}
impl Drop for Operation {
    fn drop(&mut self) {
        if !self.complete {
            self.run.fail();
        }
    }
}

struct Lifecycle {
    run: Option<Arc<Run>>,
    reusable: bool,
    stopped: bool,
    held: bool,
    retired: bool,
}
struct Control {
    operation: AsyncMutex<()>,
    lifecycle: Mutex<Lifecycle>,
}

/// Ordered, streaming initialization under one query's actual progress/storage
/// authority. Child resource IDs are stable handover metadata namespaces.
///
/// Membership is limited to 32 distinct providers. The version-1 handover record
/// retains their ordered resource IDs and optional opaque state, within the
/// existing 65536-byte query-owned bound; changing that order or membership
/// cannot reinterpret a retained record. Completion returns the combined state
/// and at most 256 distinct watermarks for the caller's existing transaction.
///
/// Non-Ready preparation stops and rearms every child before a retry, including
/// siblings which already returned Ready. No rows or progress are synthesized.
/// Dropping a stream or operation fences reuse until owned cleanup succeeds.
pub struct CompositeBootstrapProvider {
    instance: String,
    graph: String,
    component: ComponentId,
    members: Vec<Member>,
    reader: Option<SourceProgressReader>,
    control: Arc<Control>,
}

impl CompositeBootstrapProvider {
    pub fn new(
        instance: impl Into<String>,
        graph: impl Into<String>,
        component: ComponentId,
        providers: Vec<(ResourceId, Arc<dyn ComputationBootstrapProvider>)>,
    ) -> Result<Self> {
        let instance = instance.into();
        let graph = graph.into();
        super::data::validate_identifier("instance", &instance)?;
        super::data::validate_identifier("graph", &graph)?;
        anyhow::ensure!(
            !providers.is_empty() && providers.len() <= MAX_COMPOSITE_BOOTSTRAP_PROVIDERS,
            "composite bootstrap requires 1..={MAX_COMPOSITE_BOOTSTRAP_PROVIDERS} providers"
        );
        let mut names = BTreeSet::new();
        let mut members: Vec<Member> = Vec::new();
        let mut reader: Option<SourceProgressReader> = None;
        for (id, provider) in providers {
            anyhow::ensure!(
                names.insert(id.clone())
                    && members
                        .iter()
                        .all(|member| !Arc::ptr_eq(&member.provider, &provider)),
                "composite bootstrap providers must have distinct IDs and actual handles"
            );
            provider.validate_query_scope(&instance, &graph, &component)?;
            let actual = provider.recovery_reader();
            if let Some(actual) = &actual {
                anyhow::ensure!(
                    actual.graph_id() == graph
                        && actual.component_id() == &component
                        && actual.local_owner().is_some(),
                    "composite bootstrap requires an actual local query progress owner"
                );
                anyhow::ensure!(
                    reader
                        .as_ref()
                        .map_or(true, |reader| reader.same_owner(actual)),
                    "composite bootstrap children have different actual progress owners"
                );
                reader = Some(actual.clone());
            }
            members.push(Member {
                id,
                provider,
                reader: actual,
            });
        }
        Ok(Self {
            instance,
            graph,
            component,
            members,
            reader,
            control: Arc::new(Control {
                operation: AsyncMutex::new(()),
                lifecycle: Mutex::new(Lifecycle {
                    run: None,
                    reusable: true,
                    stopped: false,
                    held: false,
                    retired: false,
                }),
            }),
        })
    }

    fn current(&self) -> Result<Option<Arc<Run>>> {
        Ok(self
            .control
            .lifecycle
            .lock()
            .map_err(|_| anyhow::anyhow!("composite bootstrap lifecycle poisoned"))?
            .run
            .clone())
    }

    fn available(&self) -> Result<()> {
        let state = self
            .control
            .lifecycle
            .lock()
            .map_err(|_| anyhow::anyhow!("composite bootstrap lifecycle poisoned"))?;
        anyhow::ensure!(
            !state.held && !state.retired,
            "composite bootstrap lifecycle is held or retired"
        );
        Ok(())
    }

    fn validate_children(&self) -> Result<()> {
        for member in &self.members {
            member
                .provider
                .validate_query_scope(&self.instance, &self.graph, &self.component)?;
            anyhow::ensure!(
                match (&member.reader, member.provider.recovery_reader()) {
                    (None, None) => true,
                    (Some(expected), Some(actual)) => expected.same_owner(&actual),
                    _ => false,
                },
                "composite bootstrap child {} changed its actual progress binding",
                member.id
            );
        }
        Ok(())
    }

    async fn stop_members(&self) -> Result<()> {
        self.available()?;
        {
            let mut state = self
                .control
                .lifecycle
                .lock()
                .map_err(|_| anyhow::anyhow!("composite bootstrap lifecycle poisoned"))?;
            state.reusable = false;
            state.stopped = false;
        }
        if let Some(run) = self.current()? {
            run.fail();
            let streams = std::mem::take(
                &mut *run
                    .streams
                    .lock()
                    .map_err(|_| anyhow::anyhow!("composite bootstrap streams poisoned"))?,
            );
            drop(streams);
        }
        let mut failures = Vec::new();
        for member in self.members.iter().rev() {
            if let Err(error) = member.provider.stop().await {
                failures
                    .push(error.context(format!("stop composite bootstrap child {}", member.id)));
            }
        }
        if !failures.is_empty() {
            return Err(crate::error::OperationFailures::new(
                "composite bootstrap cleanup failed",
                failures,
            )
            .into());
        }
        let mut state = self
            .control
            .lifecycle
            .lock()
            .map_err(|_| anyhow::anyhow!("composite bootstrap lifecycle poisoned"))?;
        state.run = None;
        state.reusable = true;
        state.stopped = true;
        Ok(())
    }

    async fn fail_operation(&self, run: &Run, error: anyhow::Error) -> anyhow::Error {
        run.fail();
        match self.stop_members().await {
            Ok(()) => error,
            Err(cleanup) => error.context(crate::error::OperationFailures::new(
                "composite bootstrap operation cleanup also failed",
                vec![cleanup],
            )),
        }
    }

    async fn prepare_operation(
        &self,
        parent: Option<&dyn BootstrapState>,
    ) -> Result<BootstrapPreparation> {
        let _operation = self.control.operation.lock().await;
        self.available()?;
        self.validate_children()?;
        let run = Arc::new(Run {
            phase: AtomicU8::new(PREPARING),
            stopped: watch::channel(false).0,
            streams: Mutex::new(VecDeque::new()),
            waker: AtomicWaker::new(),
            record: AsyncMutex::new(StateRecord::empty(&self.members)),
            initial_keys: Mutex::new(BTreeSet::new()),
            completion: Mutex::new(None),
            pending: Mutex::new(false),
        });
        {
            let mut state = self
                .control
                .lifecycle
                .lock()
                .map_err(|_| anyhow::anyhow!("composite bootstrap lifecycle poisoned"))?;
            anyhow::ensure!(
                state.reusable && state.run.is_none(),
                "composite bootstrap requires awaited cleanup before preparation"
            );
            state.run = Some(run.clone());
            state.reusable = false;
            state.stopped = false;
        }
        let mut operation = Operation {
            run: run.clone(),
            complete: false,
        };
        let mut stopped = run.stopped.subscribe();
        let result = tokio::select! {
            biased;
            _ = stopped.wait_for(|stopped| *stopped) => Err(anyhow::anyhow!("composite bootstrap preparation cancelled")),
            result = async {
                *run.record.lock().await = StateRecord::read(parent, &self.members).await?;
                let mut preparation = BootstrapPreparation::Ready;
                let mut pending = false;
                for (index, member) in self.members.iter().enumerate() {
                    let result = match parent {
                        Some(parent) => member.provider.prepare_with_state(&NamespacedState { parent, record: &run.record, index }).await,
                        None => member.provider.prepare().await,
                    }.with_context(|| format!("prepare composite bootstrap child {}", member.id))?;
                    preparation = combine(preparation, result);
                    pending |= member.provider.has_pending_snapshot()?;
                }
                self.validate_children()?;
                *run.pending.lock().map_err(|_| anyhow::anyhow!("composite bootstrap pending state poisoned"))? = pending;
                Result::<_>::Ok(preparation)
            } => result,
        };
        let preparation = match result {
            Ok(value) => value,
            Err(error) => return Err(self.fail_operation(&run, error).await),
        };
        if preparation == BootstrapPreparation::Ready {
            run.phase.store(PREPARED, Ordering::Release);
        } else {
            // A Ready native sibling is already sealed. Reset/reprepare must
            // rearm every child, without discarding its persisted namespace.
            self.stop_members().await?;
        }
        operation.complete = true;
        Ok(preparation)
    }

    async fn snapshot_operation(
        &self,
        parent: Option<&dyn BootstrapState>,
    ) -> Result<ComputationBootstrapSnapshot> {
        let _operation = self.control.operation.lock().await;
        self.available()?;
        self.validate_children()?;
        let run = self
            .current()?
            .context("composite bootstrap is not prepared")?;
        anyhow::ensure!(
            run.phase.load(Ordering::Acquire) == PREPARED,
            "composite bootstrap snapshot must follow Ready preparation once"
        );
        let mut operation = Operation {
            run: run.clone(),
            complete: false,
        };
        let mut stopped = run.stopped.subscribe();
        let result = tokio::select! {
            biased;
            _ = stopped.wait_for(|stopped| *stopped) => Err(anyhow::anyhow!("composite bootstrap snapshot cancelled")),
            result = async {
                *run.record.lock().await = StateRecord::read(parent, &self.members).await?;
                let mut watermarks = Vec::new();
                let mut keys = BTreeSet::new();
                for (index, member) in self.members.iter().enumerate() {
                    let snapshot = match parent {
                        Some(parent) => member.provider.snapshot_with_state(&NamespacedState { parent, record: &run.record, index }).await,
                        None => member.provider.snapshot().await,
                    }.with_context(|| format!("open composite bootstrap child {}", member.id))?;
                    run.streams.lock().map_err(|_| anyhow::anyhow!("composite bootstrap streams poisoned"))?.push_back(snapshot.changes);
                    append_watermarks(&mut watermarks, snapshot.watermarks, &mut keys)?;
                }
                *run.initial_keys.lock().map_err(|_| anyhow::anyhow!("composite bootstrap watermark state poisoned"))? = keys;
                self.validate_children()?;
                Result::<_>::Ok(watermarks)
            } => result,
        };
        let watermarks = match result {
            Ok(value) => value,
            Err(error) => return Err(self.fail_operation(&run, error).await),
        };
        run.phase.store(STREAMING, Ordering::Release);
        operation.complete = true;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(Cursor { run }),
            watermarks,
        })
    }
}

fn combine(left: BootstrapPreparation, right: BootstrapPreparation) -> BootstrapPreparation {
    match (left, right) {
        (BootstrapPreparation::ResetRequired, _) | (_, BootstrapPreparation::ResetRequired) => {
            BootstrapPreparation::ResetRequired
        }
        (BootstrapPreparation::RefreshVolatile, _) | (_, BootstrapPreparation::RefreshVolatile) => {
            BootstrapPreparation::RefreshVolatile
        }
        _ => BootstrapPreparation::Ready,
    }
}

fn append_watermarks(
    output: &mut Vec<BootstrapWatermark>,
    added: Vec<BootstrapWatermark>,
    keys: &mut BTreeSet<String>,
) -> Result<()> {
    anyhow::ensure!(
        keys.len().saturating_add(added.len()) <= MAX_COMPOSITE_BOOTSTRAP_WATERMARKS,
        "composite bootstrap watermark bound exceeded"
    );
    for watermark in added {
        watermark.validate()?;
        if let Some(source) = &watermark.source_id {
            ComponentId::try_new(source.as_str())?;
        }
        anyhow::ensure!(
            watermark
                .position
                .as_ref()
                .map_or(true, |position| position.len()
                    <= crate::sources::SourceBase::MAX_SOURCE_POSITION_BYTES),
            "composite bootstrap source position exceeds the checkpoint bound"
        );
        anyhow::ensure!(
            keys.insert(super::query::progress_key(
                watermark.stream.as_str(),
                watermark.source_id.as_deref()
            )),
            "composite bootstrap children supplied duplicate source watermarks"
        );
        output.push(watermark);
    }
    Ok(())
}

struct Cursor {
    run: Arc<Run>,
}
impl Stream for Cursor {
    type Item = Result<ChangeEnvelope>;
    fn poll_next(self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        self.run.waker.register(context.waker());
        loop {
            if *self.run.stopped.borrow() {
                return Poll::Ready(Some(Err(anyhow::anyhow!(
                    "composite bootstrap stream is stopped"
                ))));
            }
            let mut streams = match self.run.streams.lock() {
                Ok(streams) => streams,
                Err(_) => {
                    return Poll::Ready(Some(Err(anyhow::anyhow!(
                        "composite bootstrap streams poisoned"
                    ))))
                }
            };
            let Some(stream) = streams.front_mut() else {
                self.run.phase.store(ENDED, Ordering::Release);
                return Poll::Ready(None);
            };
            match stream.as_mut().poll_next(context) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(item))) => return Poll::Ready(Some(Ok(item))),
                Poll::Ready(Some(Err(error))) => {
                    drop(streams);
                    self.run.fail();
                    return Poll::Ready(Some(Err(error)));
                }
                Poll::Ready(None) => {
                    let finished = streams.pop_front();
                    drop(streams);
                    drop(finished);
                }
            }
        }
    }
}
impl Drop for Cursor {
    fn drop(&mut self) {
        if self.run.phase.load(Ordering::Acquire) == STREAMING {
            self.run.fail();
        }
        let streams = self
            .run
            .streams
            .lock()
            .map(|mut streams| std::mem::take(&mut *streams));
        if let Ok(streams) = streams {
            drop(streams);
        }
    }
}

#[async_trait]
impl ComputationBootstrapProvider for CompositeBootstrapProvider {
    fn validate_query_scope(
        &self,
        instance: &str,
        graph: &str,
        component: &ComponentId,
    ) -> Result<()> {
        self.available()?;
        anyhow::ensure!(
            self.instance == instance && self.graph == graph && &self.component == component,
            "composite bootstrap belongs to another instance/graph/query"
        );
        self.validate_children()
    }
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.reader.clone()
    }
    async fn prepare(&self) -> Result<BootstrapPreparation> {
        self.prepare_operation(None).await
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.prepare_operation(Some(state)).await
    }
    fn has_pending_snapshot(&self) -> Result<bool> {
        self.available()?;
        match self.current()? {
            None => Ok(false),
            Some(run) => {
                anyhow::ensure!(
                    run.phase.load(Ordering::Acquire) != FAILED,
                    "composite bootstrap requires cleanup"
                );
                let pending = *run
                    .pending
                    .lock()
                    .map_err(|_| anyhow::anyhow!("composite bootstrap pending state poisoned"))?;
                Ok(pending)
            }
        }
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.snapshot_operation(None).await
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        self.snapshot_operation(Some(state)).await
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        let _operation = self.control.operation.lock().await;
        self.available()?;
        self.validate_children()?;
        let run = self
            .current()?
            .context("composite bootstrap is not prepared")?;
        anyhow::ensure!(
            run.phase.load(Ordering::Acquire) == ENDED,
            "composite bootstrap did not exhaust every child exactly once"
        );
        let mut operation = Operation {
            run: run.clone(),
            complete: false,
        };
        let mut stopped = run.stopped.subscribe();
        let result = tokio::select! {
            biased;
            _ = stopped.wait_for(|stopped| *stopped) => Err(anyhow::anyhow!("composite bootstrap completion cancelled")),
            result = async {
                let mut watermarks = Vec::new();
                let mut keys = run.initial_keys.lock().map_err(|_| anyhow::anyhow!("composite bootstrap watermark state poisoned"))?.clone();
                let mut record = run.record.lock().await.clone();
                for (index, member) in self.members.iter().enumerate() {
                    append_watermarks(&mut watermarks, member.provider.complete_snapshot().await
                        .with_context(|| format!("complete composite bootstrap child {}", member.id))?, &mut keys)?;
                    if let Some(state) = member.provider.completion_state()? {
                        validate_bootstrap_state(&state)?;
                        record.providers[index].state = Some(state);
                    }
                }
                self.validate_children()?;
                let completion = record.providers.iter().any(|member| member.state.is_some()).then(|| record.encode()).transpose()?;
                *run.completion.lock().map_err(|_| anyhow::anyhow!("composite bootstrap completion state poisoned"))? = completion;
                Result::<_>::Ok(watermarks)
            } => result,
        };
        let watermarks = match result {
            Ok(value) => value,
            Err(error) => return Err(self.fail_operation(&run, error).await),
        };
        run.phase.store(COMPLETED, Ordering::Release);
        operation.complete = true;
        Ok(watermarks)
    }
    fn completion_state(&self) -> Result<Option<Bytes>> {
        let run = self
            .current()?
            .context("composite bootstrap is not prepared")?;
        anyhow::ensure!(
            run.phase.load(Ordering::Acquire) == COMPLETED,
            "composite bootstrap completion is unavailable"
        );
        let state = run
            .completion
            .lock()
            .map_err(|_| anyhow::anyhow!("composite bootstrap completion state poisoned"))?
            .clone();
        Ok(state)
    }
    fn take_input_replay(&self) -> Result<Vec<ChangeEnvelope>> {
        self.available()?;
        let mut replay = Vec::new();
        for member in &self.members {
            replay.extend(member.provider.take_input_replay()?);
        }
        Ok(replay)
    }

    fn input_origin(&self, stream: &super::StreamId) -> Result<Option<super::OutputEndpoint>> {
        self.available()?;
        let mut origin = None;
        for member in &self.members {
            if let Some(candidate) = member.provider.input_origin(stream)? {
                anyhow::ensure!(
                    origin.is_none(),
                    "multiple bootstrap providers claim an input stream"
                );
                origin = Some(candidate);
            }
        }
        Ok(origin)
    }

    async fn stop(&self) -> Result<()> {
        if let Some(run) = self.current()? {
            run.stopped.send_replace(true);
            run.waker.wake();
        }
        let _operation = self.control.operation.lock().await;
        if self
            .control
            .lifecycle
            .lock()
            .map_err(|_| anyhow::anyhow!("composite bootstrap lifecycle poisoned"))?
            .stopped
        {
            return Ok(());
        }
        self.stop_members().await
    }
    fn freeze_for_retirement(&self) -> Result<Box<dyn BootstrapRetirement>> {
        let _operation = self
            .control
            .operation
            .try_lock()
            .context("composite bootstrap operation is active")?;
        {
            let mut state = self
                .control
                .lifecycle
                .lock()
                .map_err(|_| anyhow::anyhow!("composite bootstrap lifecycle poisoned"))?;
            anyhow::ensure!(
                state.stopped && state.reusable && !state.held && !state.retired,
                "composite bootstrap requires successful stop before retirement"
            );
            state.held = true;
        }
        let mut leases: Vec<Box<dyn BootstrapRetirement>> = Vec::new();
        for member in &self.members {
            match member.provider.freeze_for_retirement() {
                Ok(lease) => leases.push(lease),
                Err(error) => {
                    for lease in leases.into_iter().rev() {
                        lease.resume();
                    }
                    self.control
                        .lifecycle
                        .lock()
                        .map_err(|_| anyhow::anyhow!("composite bootstrap lifecycle poisoned"))?
                        .held = false;
                    return Err(
                        error.context(format!("freeze composite bootstrap child {}", member.id))
                    );
                }
            }
        }
        Ok(Box::new(Retirement {
            control: self.control.clone(),
            leases: Some(leases),
        }))
    }
}

struct Retirement {
    control: Arc<Control>,
    leases: Option<Vec<Box<dyn BootstrapRetirement>>>,
}
impl BootstrapRetirement for Retirement {
    fn resume(mut self: Box<Self>) {
        if let Some(leases) = self.leases.take() {
            for lease in leases.into_iter().rev() {
                lease.resume();
            }
        }
        if let Ok(mut state) = self.control.lifecycle.lock() {
            state.held = false;
        }
    }
    fn retire(self: Box<Self>) {
        drop(self);
    }
}
impl Drop for Retirement {
    fn drop(&mut self) {
        if let Some(leases) = self.leases.take() {
            for lease in leases {
                lease.retire();
            }
            if let Ok(mut state) = self.control.lifecycle.lock() {
                state.retired = true;
            }
        }
    }
}

#[async_trait]
impl ResourceCleanup for CompositeBootstrapProvider {
    async fn shutdown(&self) -> Result<()> {
        <Self as ComputationBootstrapProvider>::stop(self).await
    }
}

impl Drop for CompositeBootstrapProvider {
    fn drop(&mut self) {
        if let Ok(Some(run)) = self.current() {
            run.fail();
            let streams = run
                .streams
                .lock()
                .map(|mut streams| std::mem::take(&mut *streams));
            if let Ok(streams) = streams {
                drop(streams);
            }
            log::warn!("Composite bootstrap dropped without awaited cleanup");
        }
    }
}
