// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    loader::PluginOwner,
    progress::ProgressBinding,
    transaction::{RequestHandler, RequestScope},
};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_computation_plugin_abi::{self as abi, bootstrap::*};
use drasi_computation_plugin_sdk::{
    bootstrap::{self as wire, BootstrapFactoryMetadata, StateBytes},
    transport::{checked_table, take_status, OperationFuture, ReceivedBytes},
    CreateRequest, Scope,
};
use drasi_lib::computation::v1::{
    BinaryEnvelopeCodec, BootstrapPreparation, BootstrapRetirement, BootstrapState,
    BootstrapWatermark, ChangeEnvelope, ComponentId, ComputationBootstrapProvider,
    ComputationBootstrapSnapshot, QuerySourceProgress,
};
use serde::de::DeserializeOwned;
use std::{
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
};
use tokio_stream::Stream;

const IDLE: u8 = 0;
const CALLING: u8 = 1;
const HELD: u8 = 2;
const RETIRED: u8 = 3;

/// Immutable factory discovery from the existing loaded-plugin registry.
pub type NativeBootstrapFactories = std::collections::BTreeMap<
    drasi_lib::computation::v1::ImplementationIdentity,
    Arc<NativeBootstrapFactory>,
>;

pub struct NativeBootstrapFactory {
    metadata: BootstrapFactoryMetadata,
    index: u32,
    extension: PluginBootstrapV1,
    plugin: Arc<PluginOwner>,
    codec: Arc<BinaryEnvelopeCodec>,
}
impl NativeBootstrapFactory {
    pub(super) fn new(
        metadata: BootstrapFactoryMetadata,
        index: u32,
        extension: PluginBootstrapV1,
        plugin: Arc<PluginOwner>,
        codec: Arc<BinaryEnvelopeCodec>,
    ) -> Self {
        Self {
            metadata,
            index,
            extension,
            plugin,
            codec,
        }
    }
    pub fn metadata(&self) -> &BootstrapFactoryMetadata {
        &self.metadata
    }
    pub fn create_provider(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Option<Scope>,
        progress: Option<Arc<QuerySourceProgress>>,
    ) -> anyhow::Result<Arc<NativeBootstrapProxy>> {
        self.create_bound_provider(id, configuration, scope, progress, None)
    }
    pub(crate) fn create_query_provider(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Scope,
        progress: Option<Arc<QuerySourceProgress>>,
    ) -> anyhow::Result<Arc<NativeBootstrapProxy>> {
        let query_scope = Some((scope.clone(), id.clone()));
        self.create_bound_provider(id, configuration, Some(scope), progress, query_scope)
    }
    fn create_bound_provider(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
        scope: Option<Scope>,
        progress: Option<Arc<QuerySourceProgress>>,
        query_scope: Option<(Scope, ComponentId)>,
    ) -> anyhow::Result<Arc<NativeBootstrapProxy>> {
        let request = CreateRequest {
            id,
            configuration,
            scope,
            implementation: self.metadata.implementation.clone(),
            configuration_version: self.metadata.configuration_version,
        };
        self.metadata.validate_request(&request)?;
        anyhow::ensure!(
            self.metadata.source_progress == progress.is_some(),
            "native bootstrap source progress binding mismatch"
        );
        if let Some(progress) = &progress {
            let scope = request
                .scope
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("bootstrap progress requires graph scope"))?;
            anyhow::ensure!(
                progress.graph_id() == scope.graph_id,
                "bootstrap progress belongs to another graph"
            );
        }
        let binding = progress.as_ref().map(ProgressBinding::new);
        let table = binding.as_ref().map(ProgressBinding::table);
        let bytes = wire::encode(&request)?;
        let mut raw = BootstrapHandle::null();
        unsafe {
            take_status(self.extension.create.expect("validated")(
                self.plugin.state(),
                self.index,
                abi::BorrowedBytes::new(&bytes),
                table.as_ref().map_or(std::ptr::null(), |table| table),
                &mut raw,
            ))?;
        }
        let vtable = unsafe { checked_table(raw.vtable)? };
        anyhow::ensure!(
            !raw.state.is_null()
                && vtable.release.is_some()
                && vtable.begin.is_some()
                && vtable.cancel_snapshot.is_some(),
            "incomplete native bootstrap provider table"
        );
        Ok(Arc::new(NativeBootstrapProxy {
            inner: Arc::new(Remote {
                raw,
                _plugin: self.plugin.clone(),
                progress,
                _binding: binding,
                cleanup_required: AtomicBool::new(false),
                current_snapshot: AtomicU64::new(0),
                lifecycle: AtomicU8::new(IDLE),
                stopped: AtomicBool::new(false),
            }),
            codec: self.codec.clone(),
            pending: Mutex::new(None),
            completion: Mutex::new(None),
            query_scope,
        }))
    }
}

struct Remote {
    raw: BootstrapHandle,
    _plugin: Arc<PluginOwner>,
    progress: Option<Arc<QuerySourceProgress>>,
    _binding: Option<ProgressBinding>,
    cleanup_required: AtomicBool,
    current_snapshot: AtomicU64,
    lifecycle: AtomicU8,
    stopped: AtomicBool,
}
// Only the producer dereferences its handle; independent cancellation is
// thread-safe, and begin rejects overlapping data/lifecycle operations.
unsafe impl Send for Remote {}
unsafe impl Sync for Remote {}
impl Remote {
    fn table(&self) -> &BootstrapVTable {
        unsafe { &*self.raw.vtable }
    }
    fn begin(
        &self,
        code: u32,
        generation: u64,
        state: Option<&BootstrapStateV1>,
    ) -> anyhow::Result<OperationFuture> {
        anyhow::ensure!(
            self.lifecycle.load(Ordering::Acquire) != RETIRED,
            "native bootstrap resource is retired"
        );
        anyhow::ensure!(
            code == STOP || !self.cleanup_required.load(Ordering::Acquire),
            "native bootstrap requires cleanup before reuse"
        );
        self.stopped.store(false, Ordering::Release);
        let mut raw = abi::OperationHandle::null();
        unsafe {
            take_status(self.table().begin.expect("validated")(
                self.raw.state,
                code,
                generation,
                state.map_or(std::ptr::null(), |state| state),
                &mut raw,
            ))?;
            Ok(OperationFuture::new(raw)?)
        }
    }
    fn cancel(&self) {
        self.cancel_snapshot(0);
    }
    fn cancel_snapshot(&self, generation: u64) {
        if generation != 0 && generation != self.current_snapshot.load(Ordering::Acquire) {
            return;
        }
        self.cleanup_required.store(true, Ordering::Release);
        self.current_snapshot.store(0, Ordering::Release);
        if let Err(error) = unsafe {
            take_status(self.table().cancel_snapshot.expect("validated")(
                self.raw.state,
                generation,
            ))
        } {
            tracing::error!(%error, "native bootstrap cancellation request failed; cleanup remains required");
        }
    }
    fn guard(self: &Arc<Self>) -> anyhow::Result<CallGuard> {
        self.lifecycle
            .compare_exchange(IDLE, CALLING, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|state| {
                if state == RETIRED {
                    drasi_computation_plugin_sdk::transport::Failure::closed()
                } else {
                    drasi_computation_plugin_sdk::transport::Failure::new(
                        abi::status::BUSY,
                        "native bootstrap host operation already active",
                    )
                }
            })?;
        Ok(CallGuard {
            remote: self.clone(),
            completed: false,
        })
    }
    async fn call<T: DeserializeOwned>(
        self: &Arc<Self>,
        code: u32,
        state: Option<&dyn BootstrapState>,
    ) -> anyhow::Result<(T, CallGuard)> {
        let guard = self.guard()?;
        if code == STOP {
            self.current_snapshot.store(0, Ordering::Release);
        }
        let bytes = if let Some(state) = state {
            let mut requests = RequestScope::new(READ_STATE..=WRITE_STATE, MAX_CONTROL_BYTES);
            let operation = {
                let durability = wire::encode(&state.durability())?;
                let table = BootstrapStateV1 {
                    header: abi::Header::new::<BootstrapStateV1>(),
                    durability: abi::BorrowedBytes::new(&durability),
                    reserved: 0,
                    requests: requests.table(),
                };
                self.begin(code, 0, Some(&table))?
            };
            let handler = StateRequests(state);
            tokio::pin!(operation);
            let result = tokio::select! {
                biased;
                result = &mut operation => result.map_err(anyhow::Error::from),
                result = requests.serve(&handler) => {
                    result?;
                    unreachable!("scoped request service does not complete successfully")
                }
            };
            requests.revoke();
            let bytes = result?;
            requests.finish()?;
            bytes
        } else {
            self.begin(code, 0, None)?.await?
        };
        let decoded = wire::decode(&bytes)?;
        Ok((decoded, guard))
    }
}
impl Drop for Remote {
    fn drop(&mut self) {
        unsafe { self.table().release.expect("validated")(self.raw.state) };
    }
}
struct CallGuard {
    remote: Arc<Remote>,
    completed: bool,
}
impl Drop for CallGuard {
    fn drop(&mut self) {
        if !self.completed {
            self.remote.cancel();
        }
        let _ = self.remote.lifecycle.compare_exchange(
            CALLING,
            IDLE,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}

struct Retirement {
    remote: Arc<Remote>,
    resumed: bool,
}
impl BootstrapRetirement for Retirement {
    fn resume(mut self: Box<Self>) {
        self.resumed = true;
    }
    fn retire(self: Box<Self>) {
        drop(self);
    }
}
impl Drop for Retirement {
    fn drop(&mut self) {
        let _ = self.remote.lifecycle.compare_exchange(
            HELD,
            if self.resumed { IDLE } else { RETIRED },
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        if self.remote.lifecycle.load(Ordering::Acquire) == RETIRED {
            if let Some(binding) = &self.remote._binding {
                binding.revoke();
            }
        }
    }
}
struct StateRequests<'a>(&'a dyn BootstrapState);
#[async_trait]
impl RequestHandler for StateRequests<'_> {
    async fn apply(&self, code: u32, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        match code {
            READ_STATE => {
                wire::decode::<()>(input)?;
                let value = self.0.read().await?;
                if let Some(value) = &value {
                    wire::validate_state(value)?;
                }
                wire::encode(&value.map(|bytes| StateBytes::from(bytes.to_vec())))
            }
            WRITE_STATE => {
                let value: StateBytes = wire::decode(input)?;
                wire::validate_state(&value)?;
                self.0.write(value.into_vec().into()).await?;
                wire::encode(&())
            }
            _ => anyhow::bail!("unsupported native bootstrap state operation"),
        }
    }
}

pub struct NativeBootstrapProxy {
    inner: Arc<Remote>,
    codec: Arc<BinaryEnvelopeCodec>,
    pending: Mutex<Option<bool>>,
    completion: Mutex<Option<Option<Bytes>>>,
    query_scope: Option<(Scope, ComponentId)>,
}
impl Drop for NativeBootstrapProxy {
    fn drop(&mut self) {
        self.inner.cancel();
        if let Some(binding) = &self.inner._binding {
            binding.revoke();
        }
    }
}
impl NativeBootstrapProxy {
    pub fn source_progress(&self) -> Option<&Arc<QuerySourceProgress>> {
        self.inner.progress.as_ref()
    }
    async fn stop_inner(&self, retire: bool) -> anyhow::Result<()> {
        let ((), mut guard) = self.inner.call::<()>(STOP, None).await?;
        *self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("bootstrap preparation poisoned"))? = None;
        *self
            .completion
            .lock()
            .map_err(|_| anyhow::anyhow!("bootstrap completion poisoned"))? = None;
        self.inner.cleanup_required.store(false, Ordering::Release);
        self.inner.stopped.store(true, Ordering::Release);
        if retire {
            self.inner.lifecycle.store(RETIRED, Ordering::Release);
            if let Some(binding) = &self.inner._binding {
                binding.revoke();
            }
        }
        guard.completed = true;
        Ok(())
    }
    async fn prepare_inner(
        &self,
        state: Option<&dyn BootstrapState>,
    ) -> anyhow::Result<BootstrapPreparation> {
        let (prepared, mut guard) = self.inner.call::<wire::Prepared>(PREPARE, state).await?;
        *self
            .pending
            .lock()
            .map_err(|_| anyhow::anyhow!("bootstrap preparation poisoned"))? =
            Some(prepared.pending);
        *self
            .completion
            .lock()
            .map_err(|_| anyhow::anyhow!("bootstrap completion poisoned"))? = None;
        guard.completed = true;
        Ok(prepared.preparation.into())
    }
    async fn snapshot_inner(
        &self,
        state: Option<&dyn BootstrapState>,
    ) -> anyhow::Result<ComputationBootstrapSnapshot> {
        let (started, mut guard) = self
            .inner
            .call::<wire::SnapshotStarted>(SNAPSHOT, state)
            .await?;
        if started.generation == 0 {
            self.inner.cancel();
            anyhow::bail!("native bootstrap returned an invalid stream generation");
        }
        self.inner
            .current_snapshot
            .store(started.generation, Ordering::Release);
        let stream = SnapshotStream {
            remote: self.inner.clone(),
            codec: self.codec.clone(),
            operation: None,
            ended: false,
            generation: started.generation,
            guard: None,
        };
        wire::validate_watermarks(&started.watermarks)?;
        guard.completed = true;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(stream),
            watermarks: started.watermarks.into_iter().map(Into::into).collect(),
        })
    }
}
#[async_trait]
impl ComputationBootstrapProvider for NativeBootstrapProxy {
    fn freeze_for_retirement(&self) -> anyhow::Result<Box<dyn BootstrapRetirement>> {
        let mut guard = self.inner.guard()?;
        guard.completed = true;
        anyhow::ensure!(
            self.inner.stopped.load(Ordering::Acquire)
                && !self.inner.cleanup_required.load(Ordering::Acquire)
                && self.inner.current_snapshot.load(Ordering::Acquire) == 0,
            "native bootstrap must complete stop before retirement"
        );
        self.inner.lifecycle.store(HELD, Ordering::Release);
        Ok(Box::new(Retirement {
            remote: self.inner.clone(),
            resumed: false,
        }))
    }
    fn validate_query_scope(
        &self,
        instance: &str,
        graph: &str,
        component: &ComponentId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.inner.lifecycle.load(Ordering::Acquire) != RETIRED,
            "native bootstrap resource is retired"
        );
        if let Some((scope, owner)) = &self.query_scope {
            anyhow::ensure!(
                scope.instance_id == instance && scope.graph_id == graph && owner == component,
                "native bootstrap belongs to another instance/graph/query"
            );
        }
        Ok(())
    }
    fn recovery_reader(&self) -> Option<drasi_lib::computation::v1::SourceProgressReader> {
        self.inner
            .progress
            .clone()
            .map(drasi_lib::computation::v1::SourceProgressReader::Local)
    }

    async fn stop(&self) -> anyhow::Result<()> {
        match self.inner.lifecycle.load(Ordering::Acquire) {
            HELD | RETIRED => Ok(()),
            _ => self.stop_inner(false).await,
        }
    }
    async fn prepare(&self) -> anyhow::Result<BootstrapPreparation> {
        self.prepare_inner(None).await
    }
    async fn prepare_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<BootstrapPreparation> {
        self.prepare_inner(Some(state)).await
    }
    fn has_pending_snapshot(&self) -> anyhow::Result<bool> {
        self.pending
            .lock()
            .map_err(|_| anyhow::anyhow!("bootstrap preparation poisoned"))?
            .ok_or_else(|| anyhow::anyhow!("native bootstrap has not been prepared"))
    }
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        self.snapshot_inner(None).await
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> anyhow::Result<ComputationBootstrapSnapshot> {
        self.snapshot_inner(Some(state)).await
    }
    async fn complete_snapshot(&self) -> anyhow::Result<Vec<BootstrapWatermark>> {
        let (completed, mut guard) = self.inner.call::<wire::Completed>(COMPLETE, None).await?;
        if let Err(error) = wire::validate_watermarks(&completed.watermarks).and_then(|_| {
            completed
                .state
                .as_ref()
                .map_or(Ok(()), |state| wire::validate_state(state))
        }) {
            self.inner.cancel();
            return Err(error);
        }
        *self
            .completion
            .lock()
            .map_err(|_| anyhow::anyhow!("bootstrap completion poisoned"))? =
            Some(completed.state.map(|bytes| bytes.into_vec().into()));
        guard.completed = true;
        Ok(completed.watermarks.into_iter().map(Into::into).collect())
    }
    fn completion_state(&self) -> anyhow::Result<Option<Bytes>> {
        self.completion
            .lock()
            .map_err(|_| anyhow::anyhow!("bootstrap completion poisoned"))?
            .clone()
            .ok_or_else(|| anyhow::anyhow!("native bootstrap completion has not finished"))
    }
}

#[async_trait]
impl drasi_lib::computation::v1::ResourceCleanup for NativeBootstrapProxy {
    async fn shutdown(&self) -> anyhow::Result<()> {
        // A held provider already completed stop. Atomically revoke that lease
        // so terminal cleanup cannot be blocked by unknown commit confirmation.
        match self.inner.lifecycle.compare_exchange(
            HELD,
            RETIRED,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) | Err(RETIRED) => {
                if let Some(binding) = &self.inner._binding {
                    binding.revoke();
                }
                Ok(())
            }
            Err(_) => self.stop_inner(true).await,
        }
    }
}

struct SnapshotStream {
    remote: Arc<Remote>,
    codec: Arc<BinaryEnvelopeCodec>,
    operation: Option<OperationFuture>,
    ended: bool,
    generation: u64,
    guard: Option<CallGuard>,
}
impl SnapshotStream {
    fn decode(&self, bytes: ReceivedBytes) -> anyhow::Result<Option<ChangeEnvelope>> {
        let bytes: Option<StateBytes> = drasi_computation_plugin_sdk::wire::decode(&bytes)?;
        bytes
            .map(|bytes| self.codec.decode(&bytes).map_err(anyhow::Error::from))
            .transpose()
    }
}
impl Stream for SnapshotStream {
    type Item = anyhow::Result<ChangeEnvelope>;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.ended {
            return Poll::Ready(None);
        }
        if self.generation != self.remote.current_snapshot.load(Ordering::Acquire) {
            self.ended = true;
            return Poll::Ready(Some(Err(
                drasi_computation_plugin_sdk::transport::Failure::closed().into(),
            )));
        }
        if self.operation.is_none() {
            let guard = match self.remote.guard() {
                Ok(guard) => guard,
                Err(error) => {
                    self.ended = true;
                    return Poll::Ready(Some(Err(error)));
                }
            };
            self.guard = Some(guard);
            match self.remote.begin(NEXT, self.generation, None) {
                Ok(operation) => self.operation = Some(operation),
                Err(error) => {
                    self.guard.take();
                    self.ended = true;
                    return Poll::Ready(Some(Err(error)));
                }
            }
        }
        let result = match Pin::new(self.operation.as_mut().expect("initialized")).poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(result) => result,
        };
        self.operation = None;
        let result = result
            .map_err(anyhow::Error::from)
            .and_then(|bytes| self.decode(bytes));
        if let Some(mut guard) = self.guard.take() {
            guard.completed = result.is_ok();
        }
        match result {
            Ok(Some(envelope)) => Poll::Ready(Some(Ok(envelope))),
            Ok(None) => {
                self.ended = true;
                Poll::Ready(None)
            }
            Err(error) => {
                self.ended = true;
                Poll::Ready(Some(Err(error)))
            }
        }
    }
}
impl Drop for SnapshotStream {
    fn drop(&mut self) {
        self.operation.take();
        self.guard.take();
        if !self.ended {
            self.remote.cancel_snapshot(self.generation);
        }
    }
}
