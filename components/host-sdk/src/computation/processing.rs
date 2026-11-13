// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Host-owned index sessions for opted-in native transformers.

use anyhow::Context;
use drasi_core_types::{CheckpointKey, ComponentId, OutputPartition};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{
    collections::HashMap,
    ffi::c_void,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use drasi_computation_plugin_abi::{self as abi, processing::*};
use drasi_computation_plugin_sdk::{
    processing::{index_error_from_failure, index_error_to_failure, AtomicStateInfo},
    transport::{self, checked_table, take_status, Failure, OperationFuture},
    wire,
};
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationQueryError, ComputationTransaction,
    },
    evaluation::functions::aggregation::ValueAccumulator,
    interface::{
        ElementStream, IndexError, IndexJoin, OutboxPageLimits, PushType, ResultKey, ResultOwner,
        RowMutation,
    },
    models::{Element, ElementReference, ElementTimestamp, TimestampBound, TimestampRange},
};
use futures_util::StreamExt;
use ordered_float::OrderedFloat;
use tokio::{runtime::Handle, sync::Notify, task::JoinHandle};
use tokio_util::sync::CancellationToken;

use super::transaction::{RequestHandler, RequestScope};

#[path = "processing_bootstrap_state.rs"]
mod bootstrap_state;
#[path = "processing_output.rs"]
mod output;
#[path = "processing_progress.rs"]
mod progress;
pub(super) use progress::Call as ProcessingCall;
#[path = "processing_reset.rs"]
mod reset;
#[path = "processing_scheduling.rs"]
mod scheduling;

const MAX_OPERATIONS: usize = 16;
type Opening = Pin<Box<dyn Future<Output = Result<ComputationIndexes, IndexError>> + Send>>;

struct CleanupAttempt {
    component: ComponentId,
    finished: bool,
}
impl Drop for CleanupAttempt {
    fn drop(&mut self) {
        if !self.finished {
            log::error!(
                "native processing cleanup for {} was interrupted before completion",
                self.component
            );
        }
    }
}

/// A provider owned by the graph, never a provider or storage pointer supplied
/// by the native library.
pub struct NativeProcessingResource {
    provider: Arc<dyn ComputationIndexProvider>,
    publication: drasi_lib::computation::v1::QueryPublicationMode,
    scheduling_clock: Option<Arc<dyn drasi_computation_contracts::PhysicalClock>>,
}

impl NativeProcessingResource {
    pub fn new(provider: Arc<dyn ComputationIndexProvider>) -> Self {
        Self {
            provider,
            publication: drasi_lib::computation::v1::QueryPublicationMode::Atomic,
            scheduling_clock: None,
        }
    }

    pub fn with_publication(
        mut self,
        publication: drasi_lib::computation::v1::QueryPublicationMode,
    ) -> Self {
        self.publication = publication;
        self
    }

    pub fn provider(&self) -> &Arc<dyn ComputationIndexProvider> {
        &self.provider
    }

    pub fn publication(&self) -> drasi_lib::computation::v1::QueryPublicationMode {
        self.publication
    }

    /// Supply the physical clock for negotiated non-query scheduled processing.
    /// It does not change logical input timestamps or the query test-clock binding.
    pub fn with_scheduling_clock(
        mut self,
        clock: Arc<dyn drasi_computation_contracts::PhysicalClock>,
    ) -> Self {
        self.scheduling_clock = Some(clock);
        self
    }

    pub(super) fn scheduling_clock(
        &self,
    ) -> Option<Arc<dyn drasi_computation_contracts::PhysicalClock>> {
        self.scheduling_clock.clone()
    }
}

struct Live {
    owner: Option<Arc<ComputationTransaction>>,
    active: bool,
    prepared: bool,
    cancel: CancellationToken,
    runtime: Option<Handle>,
    outstanding: usize,
    quiesced: bool,
}
struct State {
    component: ComponentId,
    progress: Option<Arc<progress::Progress>>,
    output: Option<Arc<output::Output>>,
    scheduling: Option<Arc<scheduling::Scheduling>>,
    live: Mutex<Live>,
    settled: Notify,
}

impl State {
    fn checkpoint_store(
        &self,
        resources: &ComputationIndexes,
    ) -> anyhow::Result<Arc<dyn drasi_core::interface::CheckpointStore>> {
        match &self.progress {
            Some(progress) => progress.checkpoint_store(resources),
            None => resources
                .checkpoint_store()
                .cloned()
                .context("missing processing checkpoint store"),
        }
    }

    fn progress_call(&self) -> anyhow::Result<Option<progress::Call>> {
        self.progress
            .as_ref()
            .map(|progress| {
                let call = progress.call()?;
                Ok(match &self.output {
                    Some(output) => call.with_failure(output.failure.clone()),
                    None => call,
                })
            })
            .transpose()
    }
}

pub(super) struct ProcessingBinding {
    state: Arc<State>,
    provider: Arc<dyn ComputationIndexProvider>,
    graph: String,
    component: ComponentId,
    opening: Mutex<Option<Opening>>,
    runtime: Option<Handle>,
    volatile_processing: bool,
    non_atomic_publication: bool,
    replacement: bool,
}

impl ProcessingBinding {
    pub(super) fn new(
        provider: Arc<dyn ComputationIndexProvider>,
        graph: String,
        component: ComponentId,
    ) -> Self {
        Self {
            state: Arc::new(State {
                component: component.clone(),
                progress: None,
                output: None,
                scheduling: None,
                live: Mutex::new(Live {
                    owner: None,
                    active: false,
                    prepared: false,
                    cancel: CancellationToken::new(),
                    runtime: None,
                    outstanding: 0,
                    quiesced: false,
                }),
                settled: Notify::new(),
            }),
            provider,
            graph,
            component,
            opening: Mutex::new(None),
            runtime: None,
            volatile_processing: false,
            non_atomic_publication: false,
            replacement: false,
        }
    }

    pub(super) fn with_volatile_processing(mut self, supported: bool) -> Self {
        self.volatile_processing = supported && self.provider.is_volatile();
        self
    }

    pub(super) fn with_replacement(mut self, replacement: bool) -> Self {
        self.replacement = replacement;
        self
    }

    pub(super) fn with_scheduling(
        mut self,
        enabled: bool,
        clock: Option<Arc<dyn drasi_computation_contracts::PhysicalClock>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            enabled || clock.is_none(),
            "a scheduling clock requires negotiated scheduled processing"
        );
        if enabled {
            anyhow::ensure!(
                !self.provider.is_volatile()
                    && !self.volatile_processing
                    && !self.non_atomic_publication
                    && self.state.progress.is_some(),
                "scheduled processing requires persistent atomic input progress"
            );
            Arc::get_mut(&mut self.state)
                .context("scheduling must bind before export")?
                .scheduling = Some(Arc::new(scheduling::Scheduling::new(clock)));
        }
        Ok(self)
    }

    pub(super) fn with_publication(
        mut self,
        mode: drasi_lib::computation::v1::QueryPublicationMode,
        supported: bool,
    ) -> anyhow::Result<Self> {
        self.non_atomic_publication = !self.provider.is_volatile()
            && mode == drasi_lib::computation::v1::QueryPublicationMode::NonAtomic;
        anyhow::ensure!(
            !self.non_atomic_publication || supported,
            "native factory does not support persistent non-atomic publication"
        );
        Ok(self)
    }

    pub(super) fn with_committed_input_progress(
        self,
        port: Option<drasi_computation_contracts::PortId>,
    ) -> anyhow::Result<Self> {
        self.with_committed_input_progress_owner(port, None)
    }

    pub(super) fn with_committed_input_progress_owner(
        mut self,
        port: Option<drasi_computation_contracts::PortId>,
        owner: Option<Arc<drasi_lib::computation::v1::QuerySourceProgress>>,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            port.is_some() || owner.is_none(),
            "processing does not expose committed input progress"
        );
        if let Some(port) = port {
            Arc::get_mut(&mut self.state)
                .ok_or_else(|| anyhow::anyhow!("processing progress must bind before export"))?
                .progress = Some(Arc::new(progress::Progress::with_reader(
                self.graph.clone(),
                self.component.clone(),
                port,
                owner,
                self.volatile_processing,
                self.non_atomic_publication,
                self.replacement,
            )?));
        }
        Ok(self)
    }

    pub(super) fn with_durable_output(
        mut self,
        enabled: bool,
        codec: Arc<drasi_computation_contracts::BinaryEnvelopeCodec>,
    ) -> anyhow::Result<Self> {
        if enabled && !self.volatile_processing {
            anyhow::ensure!(
                self.state.progress.is_some(),
                "durable output requires committed input progress"
            );
            Arc::get_mut(&mut self.state)
                .ok_or_else(|| anyhow::anyhow!("durable output must bind before export"))?
                .output = Some(Arc::new(output::Output::new(
                self.component.clone(),
                self.graph.clone(),
                codec,
                !self.non_atomic_publication,
            )));
        }
        Ok(self)
    }

    pub(super) fn committed_input_progress(
        &self,
    ) -> anyhow::Result<Option<drasi_computation_contracts::SourceProgressView>> {
        self.state
            .progress
            .as_ref()
            .map(|progress| progress.view())
            .transpose()
    }

    pub(super) fn bootstrap_state(&self) -> Arc<dyn drasi_computation_contracts::BootstrapState> {
        Arc::new(bootstrap_state::ProcessingBootstrapState::new(
            Arc::downgrade(&self.state),
        ))
    }

    pub(super) fn progress_owner(
        &self,
    ) -> Option<Arc<drasi_lib::computation::v1::QuerySourceProgress>> {
        self.state
            .progress
            .as_ref()
            .map(|progress| progress.owner_reader())
    }

    pub(super) fn bind_scheduling(
        &self,
        scheduling: &drasi_lib::computation::v1::QuerySchedulingResource,
    ) -> anyhow::Result<()> {
        let live = self
            .state
            .live
            .lock()
            .map_err(|_| anyhow::anyhow!("native processing state poisoned"))?;
        let owner = live
            .owner
            .as_ref()
            .context("query scheduling requires active processing resources")?;
        scheduling.bind_processing(owner);
        Ok(())
    }

    pub(super) fn bind_transport(
        &self,
        counter: Arc<std::sync::atomic::AtomicU64>,
    ) -> anyhow::Result<()> {
        self.state
            .progress
            .as_ref()
            .context("query transport requires its processing owner")?
            .bind_transport(counter)
    }

    pub(super) fn bind_clock(
        &self,
        clock: &Arc<drasi_lib::computation::v1::QuerySchedulingResource>,
    ) -> anyhow::Result<()> {
        self.state
            .progress
            .as_ref()
            .context("query clock requires its processing owner")?
            .bind_clock(clock)
    }

    pub(super) fn recovery_contract(&self) -> drasi_lib::computation::v1::ComponentRecovery {
        let mut recovery = drasi_lib::computation::v1::ComponentRecovery::default();
        match self.state.live.lock() {
            Ok(live) => {
                if let Some(owner) = &live.owner {
                    if self.volatile_processing {
                        return recovery.with_committed_progress(
                            self.progress_owner().expect("volatile processing progress"),
                        );
                    }
                    if !self.non_atomic_publication {
                        if let Ok(actual) =
                            drasi_lib::computation::v1::ComponentRecovery::transactional(
                                owner.resources(),
                            )
                        {
                            recovery = actual;
                        }
                    }
                }
            }
            Err(error) => log::error!("native processing recovery metadata poisoned: {error}"),
        }
        if let Some(progress) = self.progress_owner() {
            recovery = recovery.with_committed_progress(progress);
        }
        if let Some(output) = &self.state.output {
            recovery = recovery.with_output_bindings(output.publication.clone());
        }
        recovery
    }

    pub(super) fn progress_call(&self) -> anyhow::Result<Option<progress::Call>> {
        self.state.progress_call()
    }

    pub(super) fn cleanup_call(&self) -> anyhow::Result<Option<progress::Call>> {
        self.state
            .progress
            .as_ref()
            .map(|progress| progress.cleanup_call())
            .transpose()
    }

    pub(super) async fn bind_outputs(
        &mut self,
        bindings: &drasi_lib::computation::v1::OutputBindings,
    ) -> anyhow::Result<()> {
        let Some(output) = &self.state.output else {
            anyhow::ensure!(
                bindings.destinations().is_empty(),
                "native component does not track durable output destinations"
            );
            return Ok(());
        };
        let (owner, cancel) = {
            let mut live = self
                .state
                .live
                .lock()
                .map_err(|_| anyhow::anyhow!("native processing state poisoned"))?;
            anyhow::ensure!(
                live.active && live.outstanding < MAX_OPERATIONS,
                "native output binding requires an active processing owner"
            );
            let owner = live
                .owner
                .clone()
                .ok_or_else(|| anyhow::anyhow!("native output binding owner is missing"))?;
            live.outstanding += 1;
            (owner, live.cancel.clone())
        };
        let _owned = OperationGuard(self.state.clone());
        let _serial = self
            .state
            .progress
            .as_ref()
            .expect("durable output progress")
            .serial
            .lock()
            .await;
        output.validate_bindings(&owner, bindings)?;
        let call = self.progress_call()?;
        let draft = Arc::new(output::Draft::new(output, false)?);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => anyhow::bail!("native output binding was cancelled"),
            result = async {
                if owner.is_transaction_group_member() {
                    owner.run_with_mutations(async {
                        draft.bind(owner.resources(), &self.component, bindings).await.map_err(operation_error)?;
                        Ok(((), vec![Box::new(Publication {
                            owner: Arc::downgrade(&owner), progress: None, output: Some(draft.clone()),
                            scheduling: None,
                        }) as Box<dyn drasi_core::computation::TransactionGroupMutation>]))
                    }).await
                } else {
                    owner.run(async {
                        draft.bind(owner.resources(), &self.component, bindings).await.map_err(operation_error)?;
                        Ok(())
                    }).await
                }
            } => result?,
        }
        if !owner.is_transaction_group_member() {
            draft.publish()?;
        }
        output.bound();
        if let Some(call) = call {
            call.complete();
        }
        Ok(())
    }

    pub(super) fn output_ready(&self) -> anyhow::Result<()> {
        if let Some(output) = &self.state.output {
            output.ensure_bound()?;
        }
        Ok(())
    }

    pub(super) fn needs_cleanup(&self) -> anyhow::Result<bool> {
        let owned = self
            .state
            .live
            .lock()
            .map_err(|_| anyhow::anyhow!("native processing state poisoned"))?
            .owner
            .is_some();
        Ok(owned
            || self
                .opening
                .lock()
                .map_err(|_| anyhow::anyhow!("native processing opening poisoned"))?
                .is_some())
    }

    pub(super) fn progress_ready(&self) -> anyhow::Result<()> {
        if let Some(progress) = &self.state.progress {
            progress.ready()?;
        }
        Ok(())
    }

    pub(super) fn suspend_progress(&self) -> anyhow::Result<()> {
        if let Some(scheduling) = &self.state.scheduling {
            scheduling.suspend()?;
        }
        if let Some(progress) = &self.state.progress {
            progress.suspend()?;
        }
        Ok(())
    }

    fn fence_progress(&self) -> anyhow::Result<()> {
        if let Some(progress) = &self.state.progress {
            progress.fail()?;
        }
        self.suspend_progress()
    }

    pub(super) fn table(&self) -> ProcessingV1 {
        ProcessingV1 {
            header: abi::Header::new::<ProcessingV1>(),
            context: Arc::as_ptr(&self.state).cast_mut().cast(),
            retain: Some(retain),
            release: Some(release),
            run: Some(run),
        }
    }

    pub(super) fn with_table<T>(&self, use_table: impl FnOnce(&ProcessingV1) -> T) -> T {
        if self.state.scheduling.is_some() {
            let table = ScheduledProcessingV1 {
                processing: ProcessingV1 {
                    header: abi::Header::new::<ScheduledProcessingV1>(),
                    ..self.table()
                },
                version: SCHEDULING_VERSION,
                reserved: 0,
                observe: Some(scheduling::observe),
            };
            use_table(&table.processing)
        } else {
            use_table(&self.table())
        }
    }

    pub(super) async fn open(&mut self) -> anyhow::Result<()> {
        self.acquire(true).await?;
        if let Some(scheduling) = &self.state.scheduling {
            let progress = self
                .state
                .progress
                .as_ref()
                .context("missing scheduling progress")?;
            let _serial = progress.serial.lock().await;
            let owner = self
                .state
                .live
                .lock()
                .map_err(|_| anyhow::anyhow!("native processing state poisoned"))?
                .owner
                .clone()
                .context("missing scheduling transaction owner")?;
            scheduling.recover(&owner).await?;
        }
        Ok(())
    }

    pub(super) async fn prepare(&mut self) -> anyhow::Result<()> {
        self.acquire(false).await
    }

    async fn acquire(&mut self, activate: bool) -> anyhow::Result<()> {
        let progress_call = self.progress_call()?;
        {
            let mut live = self
                .state
                .live
                .lock()
                .map_err(|_| anyhow::anyhow!("native processing state poisoned"))?;
            if live.active || live.prepared {
                anyhow::ensure!(
                    activate || !live.active,
                    "cannot prepare active processing resources"
                );
                if activate {
                    self.runtime = Some(Handle::try_current()?);
                    live.runtime = self.runtime.clone();
                    live.active = true;
                    live.prepared = false;
                    live.quiesced = false;
                    if live.cancel.is_cancelled() {
                        live.cancel = CancellationToken::new();
                        live.quiesced = false;
                    }
                }
                if let Some(call) = progress_call {
                    call.complete();
                }
                return Ok(());
            }
            anyhow::ensure!(
                live.owner.is_none(),
                "native processing must finish cleanup before restarting"
            );
        }
        let opening = self
            .opening
            .get_mut()
            .map_err(|_| anyhow::anyhow!("native processing open state poisoned"))?;
        if opening.is_none() {
            self.runtime = Some(Handle::try_current()?);
            let provider = self.provider.clone();
            let graph = self.graph.clone();
            let component = self.component.clone();
            *opening = Some(Box::pin(async move {
                provider.create_indexes(&graph, &component).await
            }));
        }
        // Keep the accepted provider operation in the binding if this caller is
        // cancelled; stop or Drop still owns its eventual resource cleanup.
        let result = opening.as_mut().expect("opening retained").await;
        *opening = None;
        let owner = Arc::new(ComputationTransaction::for_processing(result?));
        let mut live = self
            .state
            .live
            .lock()
            .map_err(|_| anyhow::anyhow!("native processing state poisoned"))?;
        live.cancel = CancellationToken::new();
        live.runtime = self.runtime.clone();
        live.owner = Some(owner.clone());
        if let Some(scheduling) = &self.state.scheduling {
            scheduling.validate(&owner)?;
        }
        if let Some(progress) = &self.state.progress {
            progress.bind_owner(&owner)?;
        }
        if let Some(output) = &self.state.output {
            output.bind_owner(&self.provider, &owner)?;
        }
        live.active = activate;
        live.prepared = !activate;
        if let Some(call) = progress_call {
            call.complete();
        }
        Ok(())
    }

    pub(super) async fn close(&mut self) -> anyhow::Result<()> {
        let cleanup_call = self.cleanup_call()?;
        self.suspend_progress()?;
        let owner = {
            let mut live = self
                .state
                .live
                .lock()
                .map_err(|_| anyhow::anyhow!("native processing state poisoned"))?;
            live.active = false;
            live.prepared = false;
            live.cancel.cancel();
            live.owner.clone()
        };
        loop {
            let settled = self.state.settled.notified();
            if self
                .state
                .live
                .lock()
                .map_err(|_| anyhow::anyhow!("native processing state poisoned"))?
                .outstanding
                == 0
            {
                break;
            }
            settled.await;
        }
        let opening = self
            .opening
            .get_mut()
            .map_err(|_| anyhow::anyhow!("native processing open state poisoned"))?;
        let owner = match (owner, opening.as_mut()) {
            (Some(owner), _) => Some(owner),
            (None, Some(pending)) => {
                let result = pending.await;
                *opening = None;
                let owner = Arc::new(ComputationTransaction::for_processing(result?));
                self.state
                    .live
                    .lock()
                    .map_err(|_| anyhow::anyhow!("native processing state poisoned"))?
                    .owner = Some(owner.clone());
                Some(owner)
            }
            (None, None) => None,
        };
        if let Some(owner) = owner {
            if self.volatile_processing && !owner.recovery_required() {
                owner.quiesce().await?;
                {
                    let mut live = self
                        .state
                        .live
                        .lock()
                        .map_err(|_| anyhow::anyhow!("native processing state poisoned"))?;
                    live.prepared = true;
                    live.quiesced = true;
                }
                self.state
                    .progress
                    .as_ref()
                    .expect("volatile processing progress")
                    .reset_after_quiesce(&owner)?;
                if let Some(call) = cleanup_call {
                    call.complete();
                }
                return Ok(());
            }
            owner.shutdown().await?;
            self.state
                .live
                .lock()
                .map_err(|_| anyhow::anyhow!("native processing state poisoned"))?
                .owner = None;
        }
        if let Some(progress) = &self.state.progress {
            progress.discard_volatile_state().await?;
            progress.reset()?;
        }
        if let Some(call) = cleanup_call {
            call.complete();
        }
        Ok(())
    }
}

impl Drop for ProcessingBinding {
    fn drop(&mut self) {
        let retirement = match self.needs_cleanup() {
            Ok(false) => self
                .state
                .progress
                .as_ref()
                .map_or(Ok(()), |progress| progress.retire_closed()),
            Ok(true) => self.fence_progress(),
            Err(error) => {
                log::error!("native processing cleanup state unavailable: {error:#}");
                self.fence_progress()
            }
        };
        if let Err(error) = retirement {
            log::error!("native committed progress retirement failed: {error:#}");
        }
        let (owner, quiesced) = {
            let mut live = self
                .state
                .live
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            live.active = false;
            live.cancel.cancel();
            (live.owner.take(), live.quiesced && live.outstanding == 0)
        };
        if let Some(owner) = &owner {
            if let Some(cleanup) = owner.resources().cleanup() {
                cleanup.cancel();
            }
        }
        let opening = self
            .opening
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if owner.is_none() && opening.is_none() {
            return;
        }
        if self.volatile_processing && quiesced && opening.is_none() {
            return;
        }
        let state = self.state.clone();
        if let Some(runtime) = self.runtime.clone().or_else(|| Handle::try_current().ok()) {
            let mut attempt = CleanupAttempt {
                component: self.component.clone(),
                finished: false,
            };
            runtime.spawn(async move {
                let owner = match (owner, opening) {
                    (Some(owner), _) => Some(owner),
                    (None, Some(opening)) => match opening.await {
                        Ok(indexes) => {
                            Some(Arc::new(ComputationTransaction::for_processing(indexes)))
                        }
                        Err(error) => {
                            log::error!("native processing open failed during cleanup: {error}");
                            None
                        }
                    },
                    (None, None) => None,
                };
                if let Some(owner) = owner {
                    loop {
                        let settled = state.settled.notified();
                        if state
                            .live
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .outstanding
                            == 0
                        {
                            break;
                        }
                        settled.await;
                    }
                    if let Err(error) = owner.shutdown().await {
                        log::error!("native processing cleanup failed: {error}");
                    }
                }
                attempt.finished = true;
            });
        } else {
            log::error!("native processing was dropped without a runtime to finish cleanup");
        }
    }
}

struct OperationGuard(Arc<State>);
impl Drop for OperationGuard {
    fn drop(&mut self) {
        let mut live = self
            .0
            .live
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        live.outstanding -= 1;
        drop(live);
        self.0.settled.notify_one();
    }
}

struct OperationTask(JoinHandle<Result<Vec<u8>, Failure>>);
impl OperationTask {
    async fn result(mut self) -> Result<Vec<u8>, Failure> {
        (&mut self.0).await.map_err(|error| {
            if error.is_cancelled() {
                Failure::cancelled()
            } else {
                Failure::panicked()
            }
        })?
    }
}
impl Drop for OperationTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkKind {
    Ordinary,
    Output,
    DeferredProgress,
}

struct Work(ProcessingWorkV1, WorkKind);
unsafe impl Send for Work {}
unsafe impl Sync for Work {}
impl Drop for Work {
    fn drop(&mut self) {
        unsafe { self.0.release.expect("validated")(self.0.context) };
    }
}

unsafe extern "C" fn retain(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::increment_strong_count(context.cast::<State>()) });
}
unsafe extern "C" fn release(context: *mut c_void) {
    transport::drop_boundary(|| unsafe { Arc::decrement_strong_count(context.cast::<State>()) });
}
unsafe extern "C" fn run(
    context: *mut c_void,
    work: *const ProcessingWorkV1,
    out: *mut abi::OperationHandle,
) -> abi::Status {
    transport::status_boundary(|| {
        if context.is_null() || out.is_null() {
            return Err(Failure::protocol(
                "null native processing context or output",
            ));
        }
        if work.is_null() || !work.is_aligned() {
            return Err(Failure::protocol(
                "null or unaligned native processing work",
            ));
        }
        let header = unsafe { std::ptr::read(work.cast::<abi::Header>()) };
        let (work, kind) = if header.size == std::mem::size_of::<OutputProcessingWorkV1>() as u32 {
            let extended = *unsafe { checked_table(work.cast::<OutputProcessingWorkV1>())? };
            if extended.reserved != 0 {
                return Err(Failure::protocol("invalid native output processing mode"));
            }
            let kind = match extended.mode {
                OUTPUT_WORK => WorkKind::Output,
                DEFERRED_PROGRESS_WORK => WorkKind::DeferredProgress,
                _ => return Err(Failure::protocol("invalid native output processing mode")),
            };
            (extended.work, kind)
        } else {
            (*unsafe { checked_table(work)? }, WorkKind::Ordinary)
        };
        if work.context.is_null()
            || work.retain.is_none()
            || work.release.is_none()
            || work.begin.is_none()
        {
            return Err(Failure::protocol("incomplete native processing work"));
        }
        let state = unsafe { &*context.cast::<State>() };
        if kind == WorkKind::DeferredProgress
            && !state
                .progress
                .as_ref()
                .is_some_and(|progress| progress.is_non_atomic())
        {
            return Err(Failure::protocol(
                "deferred progress requires negotiated non-atomic publication",
            ));
        }
        if kind == WorkKind::Output
            && state.output.is_none()
            && !state
                .progress
                .as_ref()
                .is_some_and(|progress| progress.is_volatile())
        {
            return Err(Failure::protocol(
                "native output work requires negotiated durable output",
            ));
        }
        let (owner, cancel, runtime) = {
            let mut live = state
                .live
                .lock()
                .map_err(|_| Failure::failed("native processing state poisoned"))?;
            if !live.active {
                return Err(Failure::closed());
            }
            if live.outstanding >= MAX_OPERATIONS {
                return Err(Failure::new(
                    abi::status::BUSY,
                    "native processing operation capacity exceeded",
                ));
            }
            let owner = live.owner.clone().ok_or_else(Failure::closed)?;
            let runtime = live.runtime.clone().ok_or_else(Failure::closed)?;
            live.outstanding += 1;
            (owner, live.cancel.clone(), runtime)
        };
        unsafe { Arc::increment_strong_count(context.cast::<State>()) };
        let guard = OperationGuard(unsafe { Arc::from_raw(context.cast::<State>()) });
        let progress_call = state.progress_call().map_err(Failure::from)?;
        unsafe { work.retain.expect("validated")(work.context) };
        let work = Work(work, kind);
        let (start, started) = tokio::sync::oneshot::channel();
        let task = OperationTask(runtime.spawn(async move {
            // Release this task's owner reference before announcing settled work.
            let (_guard, owner) = (guard, owner);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Failure::cancelled()),
                result = started => {
                    if result.is_err() {
                        return Err(Failure::cancelled());
                    }
                },
            }
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(Failure::cancelled()),
                result = drive(&owner, &work, &_guard.0.component, _guard.0.progress.as_ref(), _guard.0.output.as_ref(), _guard.0.scheduling.as_ref()) => result.map_err(processing_failure),
            };
            if result.is_ok() {
                if let Some(call) = progress_call { call.complete(); }
            }
            result
        }));
        let operation = transport::export_operation(
            async move {
                let _ = start.send(());
                task.result().await
            },
            None,
        );
        unsafe { transport::write_out(out, operation) }
    })
}

struct Publication {
    owner: std::sync::Weak<ComputationTransaction>,
    progress: Option<Arc<progress::Draft>>,
    output: Option<Arc<output::Draft>>,
    scheduling: Option<Arc<scheduling::Draft>>,
}

#[async_trait::async_trait]
impl drasi_core::computation::TransactionGroupMutation for Publication {
    async fn stage(
        &mut self,
        context: &drasi_core::computation::TransactionGroupContext<'_>,
    ) -> Result<(), IndexError> {
        let owner = self.owner.upgrade().ok_or_else(|| {
            IndexError::other(std::io::Error::other(
                "processing publication owner is closed",
            ))
        })?;
        context.require_member(&owner).map_err(IndexError::other)
    }

    async fn committed(&mut self) -> Result<(), IndexError> {
        if let Some(progress) = &self.progress {
            progress
                .publish()
                .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
        }
        if let Some(output) = &self.output {
            output
                .publish()
                .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
        }
        if let Some(scheduling) = &self.scheduling {
            scheduling
                .publish()
                .map_err(|error| IndexError::Other(error.into_boxed_dyn_error()))?;
        }
        Ok(())
    }
}

async fn drive(
    owner: &Arc<ComputationTransaction>,
    work: &Work,
    component: &ComponentId,
    progress: Option<&Arc<progress::Progress>>,
    output: Option<&Arc<output::Output>>,
    scheduling: Option<&Arc<scheduling::Scheduling>>,
) -> drasi_core::computation::Result<Vec<u8>> {
    if let Some(progress) = progress {
        return Box::pin(async {
            let _serial = progress.serial.lock().await;
            if work.1 == WorkKind::DeferredProgress && owner.is_transaction_group_member() {
                return Err(operation_error(anyhow::anyhow!(
                    "deferred non-atomic publication cannot join a shared transaction group"
                )));
            }
            if progress.has_deferred().map_err(operation_error)? && work.1 != WorkKind::Output {
                return Err(operation_error(anyhow::anyhow!(
                    "finish pending non-atomic output before more work"
                )));
            }
            let reservations = if work.1 == WorkKind::Output && !progress.is_volatile() {
                output
                    .ok_or_else(|| operation_error(anyhow::anyhow!("output work has no owner")))?
                    .reserve()
                    .await
                    .map_err(operation_error)?
            } else {
                None
            };
            let draft = Arc::new(progress::Draft::new(progress).map_err(operation_error)?);
            let output = output
                .map(|output| output::Draft::new(output, work.1 == WorkKind::Output).map(Arc::new))
                .transpose()
                .map_err(operation_error)?;
            let scheduling =
                scheduling.map(|scheduling| Arc::new(scheduling::Draft::new(scheduling.clone())));
            let publication = owner.is_transaction_group_member().then(|| Publication {
                owner: Arc::downgrade(owner),
                progress: Some(draft.clone()),
                output: output.clone(),
                scheduling: scheduling.clone(),
            });
            let result = drive_inner(
                owner,
                work,
                component,
                Some(&draft),
                output.as_deref(),
                scheduling.as_deref(),
                reservations,
                publication,
            )
            .await?;
            if !owner.is_transaction_group_member() {
                if work.1 == WorkKind::DeferredProgress {
                    draft.defer().map_err(operation_error)?;
                } else {
                    draft.publish().map_err(operation_error)?;
                }
                if let Some(output) = output {
                    output.publish().map_err(operation_error)?;
                }
                if let Some(scheduling) = scheduling {
                    if let Err(error) = scheduling.publish() {
                        owner.fence();
                        return Err(operation_error(error));
                    }
                }
            }
            Ok(result)
        })
        .await;
    }
    drive_inner(owner, work, component, None, None, None, None, None).await
}

#[allow(clippy::too_many_arguments)]
async fn drive_inner(
    owner: &ComputationTransaction,
    work: &Work,
    component: &ComponentId,
    progress: Option<&progress::Draft>,
    output: Option<&output::Draft>,
    scheduling: Option<&scheduling::Draft>,
    reservations: Option<drasi_lib::computation::v1::OutputReservations>,
    publication: Option<Publication>,
) -> drasi_core::computation::Result<Vec<u8>> {
    let operation = async {
        let mut scope = RequestScope::new(
            request::GET_ELEMENT..=request::SCHEDULE_COMPLETE,
            abi::MAX_MESSAGE_BYTES,
        );
        let operation = {
            let mut raw = abi::OperationHandle::null();
            let table = scope.table();
            unsafe {
                take_status(work.0.begin.expect("validated")(
                    work.0.context,
                    &table,
                    &mut raw,
                ))
                .map_err(operation_error)?;
                OperationFuture::new(raw).map_err(operation_error)?
            }
        };
        let handler = IndexRequests {
            component,
            resources: owner.resources(),
            configured_joins: AtomicBool::new(false),
            streams: tokio::sync::Mutex::new(SlotStreams::default()),
            progress,
            output,
            scheduling,
        };
        tokio::pin!(operation);
        let result = tokio::select! {
            biased;
            result = &mut operation => result.map(|bytes| bytes.to_vec()).map_err(operation_error),
            result = scope.serve(&handler) => {
                match result {
                    Ok(()) => unreachable!("request service never completes successfully"),
                    Err(error) => Err(operation_error(error)),
                }
            },
        };
        scope.revoke();
        let result = async {
            let bytes = result?;
            scope.finish().map_err(operation_error)?;
            if let Some(progress) = progress {
                progress.validate_completion().map_err(operation_error)?;
            }
            if let Some(scheduling) = scheduling {
                scheduling
                    .stage(owner.resources())
                    .await
                    .map_err(operation_error)?;
            }
            Ok(bytes)
        }
        .await;
        if result.is_err() && handler.configured_joins.load(Ordering::Acquire) {
            owner.fence();
        }
        result
    };
    if let Some(publication) = publication {
        owner
            .run_with_mutations(async {
                let result = operation.await?;
                let envelope = output
                    .map(output::Draft::emission)
                    .transpose()
                    .map_err(operation_error)?
                    .flatten();
                let mut mutations = match reservations {
                    Some(reservations) => reservations
                        .mutations(envelope.as_ref())
                        .map_err(operation_error)?,
                    None => Vec::new(),
                };
                mutations.push(Box::new(publication));
                Ok((result, mutations))
            })
            .await
    } else {
        owner.run(operation).await
    }
}

fn operation_error(error: impl Into<anyhow::Error>) -> ComputationQueryError {
    let error = error.into();
    if let Some(index) = error
        .downcast_ref::<Failure>()
        .and_then(index_error_from_failure)
    {
        return ComputationQueryError::Index(index);
    }
    ComputationQueryError::Index(IndexError::Other(error.into_boxed_dyn_error()))
}

fn processing_failure(error: ComputationQueryError) -> Failure {
    if let ComputationQueryError::Index(index) = &error {
        if let IndexError::Other(source) = index {
            if let Some(failure) = source.downcast_ref::<Failure>() {
                return failure.clone();
            }
        }
        return index_error_to_failure(index);
    }
    Failure::from(anyhow::Error::new(error))
}

struct IndexRequests<'a> {
    component: &'a ComponentId,
    resources: &'a ComputationIndexes,
    configured_joins: AtomicBool,
    streams: tokio::sync::Mutex<SlotStreams>,
    progress: Option<&'a progress::Draft>,
    output: Option<&'a output::Draft>,
    scheduling: Option<&'a scheduling::Draft>,
}

#[derive(Default)]
struct SlotStreams {
    next_id: u64,
    active: HashMap<u64, ElementStream>,
}

impl IndexRequests<'_> {
    fn checkpoint_store(&self) -> anyhow::Result<Arc<dyn drasi_core::interface::CheckpointStore>> {
        match self.progress {
            Some(progress) => progress.checkpoint_store(self.resources),
            None => self
                .resources
                .checkpoint_store()
                .cloned()
                .context("missing processing checkpoint store"),
        }
    }

    async fn store_stream(&self, stream: ElementStream) -> anyhow::Result<Vec<u8>> {
        let mut streams = self.streams.lock().await;
        let id = streams.next_id;
        streams.next_id = id
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("native slot stream identifiers exhausted"))?;
        streams.active.insert(id, stream);
        wire::encode(&id)
    }
}
#[async_trait::async_trait]
impl RequestHandler for IndexRequests<'_> {
    async fn apply(&self, code: u32, input: &[u8]) -> anyhow::Result<Vec<u8>> {
        if (request::SCHEDULE_REQUEST..=request::SCHEDULE_COMPLETE).contains(&code) {
            return self
                .scheduling
                .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?
                .apply(self.resources, code, input)
                .await;
        }
        anyhow::ensure!(
            self.scheduling.is_none() || !matches!(code,
                request::FUTURE_PUSH..=request::FUTURE_POP
                | request::FUTURE_CLEAR
                | request::ATOMIC_STATE_RESET
            ),
            "scheduled processing owns its queue and checkpoint; use the typed scheduling operations"
        );
        let volatile = self.progress.is_some_and(progress::Draft::is_volatile);
        let non_atomic = self.progress.is_some_and(progress::Draft::is_non_atomic);
        let volatile_operation = matches!(code,
            request::CHECKPOINT_GET..=request::RESULT_SEQUENCE_STAGE
            | request::INPUT_CHECKPOINT_GET..=request::INPUT_TRANSPORT_STAGE
            | request::RESET_GENERATION_GET
            | request::INPUT_PROGRESS_PREPARE..=request::INPUT_PRODUCER_BIND
            | request::INPUT_PROGRESS_VALIDATE
        );
        if matches!(code,
            request::ATOMIC_STATE_INFO..=request::DELIVERY_PROGRESS_STAGE
            | request::INPUT_PROGRESS_PREPARE..=request::INPUT_PRODUCER_BIND
            | request::INPUT_PROGRESS_VALIDATE
            | request::RESET_OUTPUT_BASELINE
        ) && !(volatile && volatile_operation
            || non_atomic
                && matches!(code,
                    request::CHECKPOINT_GET..=request::INPUT_TRANSPORT_STAGE
                    | request::RESET_GENERATION_GET..=request::DELIVERY_PROGRESS_STAGE
                    | request::INPUT_PROGRESS_PREPARE..=request::INPUT_PRODUCER_BIND
                    | request::INPUT_PROGRESS_VALIDATE
                    | request::RESET_OUTPUT_BASELINE
                ))
        {
            match self.resources.atomic_result_transaction() {
                Ok(_) => {}
                Err(
                    ComputationQueryError::AtomicOutputUnsupported
                    | ComputationQueryError::TransactionMismatch,
                ) => {
                    return Err(index_error_to_failure(&IndexError::NotSupported).into());
                }
                Err(error) => return Err(error.into()),
            }
        }
        let index = &self.resources.indexes().element_index;
        match code {
            request::NON_ATOMIC_RESET_BEGIN | request::NON_ATOMIC_STATE_RESET => {
                anyhow::ensure!(
                    non_atomic,
                    "non-atomic reset requires its negotiated profile"
                );
                let preserve: bool = wire::decode(input)?;
                let progress = self.progress.context("missing non-atomic progress")?;
                let output = self.output.context("missing non-atomic output owner")?;
                output
                    .validate_reset(self.resources, self.component, preserve)
                    .await?;
                let generation = if code == request::NON_ATOMIC_RESET_BEGIN {
                    reset::begin_non_atomic(self.resources, self.component, progress, preserve)
                        .await?
                } else {
                    let generation =
                        reset::reset_non_atomic(self.resources, self.component, progress, preserve)
                            .await?;
                    output.reset(preserve)?;
                    generation
                };
                wire::encode(&generation)
            }
            request::NON_ATOMIC_RESET_FINISH => {
                wire::decode::<()>(input)?;
                anyhow::ensure!(
                    non_atomic,
                    "non-atomic reset requires its negotiated profile"
                );
                reset::finish_non_atomic(
                    self.resources,
                    self.component,
                    self.progress.context("missing reset progress")?,
                )
                .await?;
                wire::encode(&())
            }
            request::INPUT_PROGRESS_RESUME => {
                wire::decode::<()>(input)?;
                self.progress
                    .context("input progress is unavailable")?
                    .resume()?;
                wire::encode(&())
            }
            request::NEXT_EMISSION_SEQUENCE => {
                let stored: u64 = wire::decode(input)?;
                let sequence = match self.progress {
                    Some(progress) => progress.next_emission(stored)?,
                    None => stored
                        .checked_add(1)
                        .context("emission sequence exhausted")?,
                };
                wire::encode(&sequence)
            }
            request::STATE_INFO => {
                wire::decode::<()>(input)?;
                wire::encode(
                    &drasi_computation_plugin_sdk::processing::ProcessingStateInfo {
                        atomic: self.resources.atomic_result_transaction().is_ok(),
                        volatile,
                        non_atomic_publication: self
                            .progress
                            .is_some_and(progress::Draft::is_non_atomic),
                        non_atomic_reset: if non_atomic {
                            reset::non_atomic_info(self.resources, self.component).await?
                        } else {
                            None
                        },
                        durability: self.resources.durability(),
                        persistent_checkpoints: self
                            .resources
                            .checkpoint_store()
                            .is_some_and(|store| store.is_persistent()),
                    },
                )
            }
            request::VOLATILE_STATE_RESET => {
                let preserve = wire::decode(input)?;
                anyhow::ensure!(
                    volatile,
                    "volatile reset requires an explicitly volatile processing profile"
                );
                let generation = reset::reset_volatile(
                    self.resources,
                    self.component,
                    self.progress.expect("volatile progress"),
                    preserve,
                )
                .await?;
                wire::encode(&generation)
            }
            request::PHYSICAL_TIME => {
                wire::decode::<()>(input)?;
                let now = match (self.scheduling, self.progress) {
                    (Some(scheduling), _) => scheduling.physical_time()?,
                    (None, Some(progress)) => progress.physical_time()?,
                    (None, None) => u64::try_from(chrono::Utc::now().timestamp_millis())?,
                };
                wire::encode(&now)
            }
            request::INPUT_PROGRESS_PREPARE | request::INPUT_PROGRESS_VALIDATE => {
                let input: wire::Envelope = wire::decode(input)?;
                let codec =
                    wire::codec(&[drasi_computation_contracts::GraphChangeCodec::schema()])?;
                let input = input.into_input(&codec)?;
                let progress = self
                    .progress
                    .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?;
                let decision = if code == request::INPUT_PROGRESS_VALIDATE {
                    progress.validate_input(self.resources, input).await?
                } else {
                    progress.prepare_input(self.resources, input).await?
                };
                wire::encode(&decision)
            }
            request::INPUT_PROGRESS_STAGE => {
                wire::decode::<()>(input)?;
                self.progress
                    .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?
                    .stage_prepared_input(self.resources)
                    .await?;
                wire::encode(&())
            }
            request::INPUT_PRODUCER_BIND => {
                let (producer, key) = wire::decode(input)?;
                self.progress
                    .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?
                    .bind_producer(self.resources, producer, key)
                    .await?;
                wire::encode(&())
            }
            request::DELIVERY_PROGRESS_GET => {
                wire::decode::<()>(input)?;
                wire::encode(
                    &self
                        .output
                        .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?
                        .read(self.resources, self.component)
                        .await?,
                )
            }
            request::DELIVERY_PROGRESS_STAGE => {
                let sequence = wire::decode(input)?;
                self.output
                    .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?
                    .stage_delivery(self.resources, self.component, sequence)
                    .await?;
                wire::encode(&())
            }
            request::RESET_GENERATION_GET => {
                wire::decode::<()>(input)?;
                let value = self
                    .checkpoint_store()?
                    .read_checkpoint(&CheckpointKey::Metadata(progress::RESET_GENERATION.into()))
                    .await?;
                if let Some(value) = &value {
                    anyhow::ensure!(
                        value.sequence > 0 && value.source_position.is_none(),
                        "invalid processing reset generation"
                    );
                }
                let mut generation = value.map(|value| value.sequence);
                if non_atomic {
                    if let Some(marker) =
                        reset::non_atomic_info(self.resources, self.component).await?
                    {
                        generation = Some(
                            generation
                                .map_or(marker.generation, |value| value.max(marker.generation)),
                        );
                    }
                }
                wire::encode(&generation)
            }
            request::ATOMIC_STATE_RESET => {
                let preserve_bootstrap = wire::decode(input)?;
                if let Some(output) = self.output {
                    output
                        .validate_reset(self.resources, self.component, preserve_bootstrap)
                        .await?;
                }
                let progress = self
                    .progress
                    .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?;
                let generation =
                    reset::reset(self.resources, self.component, progress, preserve_bootstrap)
                        .await?;
                if let Some(output) = self.output {
                    output.reset(preserve_bootstrap)?;
                }
                wire::encode(&generation)
            }
            request::RESET_OUTPUT_BASELINE => {
                let sequence = wire::decode(input)?;
                self.progress
                    .context("missing reset progress")?
                    .ensure_resettable()?;
                self.output
                    .context("missing reset output owner")?
                    .initialize_reset_output(self.resources, self.component, sequence)
                    .await?;
                wire::encode(&())
            }
            request::INPUT_PROGRESS_RECOVER => {
                wire::decode::<()>(input)?;
                self.progress
                    .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?
                    .recover(self.resources)
                    .await?;
                wire::encode(&())
            }
            request::INPUT_CHECKPOINT_GET => {
                let key: drasi_computation_contracts::SourceProgressKey = wire::decode(input)?;
                let progress = self
                    .progress
                    .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?;
                let checkpoint = self
                    .checkpoint_store()?
                    .read_checkpoint(&progress.key(&key)?)
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&checkpoint.map(|value| {
                    (
                        value.sequence,
                        value.source_position.map(|position| position.to_vec()),
                    )
                }))
            }
            request::INPUT_CHECKPOINT_STAGE => {
                let (key, sequence, position, transport): (
                    drasi_computation_contracts::SourceProgressKey,
                    u64,
                    Option<Vec<u8>>,
                    Option<(drasi_computation_contracts::StreamId, u64)>,
                ) = wire::decode(input)?;
                self.progress
                    .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?
                    .stage(
                        self.resources,
                        key,
                        drasi_core::interface::SourceCheckpoint::new(
                            sequence,
                            position.map(Into::into),
                        ),
                        transport,
                    )
                    .await?;
                wire::encode(&())
            }
            request::INPUT_TRANSPORT_STAGE => {
                let (stream, sequence) = wire::decode(input)?;
                self.progress
                    .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?
                    .stage_transport(self.resources, stream, sequence)
                    .await?;
                wire::encode(&())
            }
            request::BOOTSTRAP_COMPLETE_STAGE => {
                let complete = wire::decode(input)?;
                self.progress
                    .ok_or_else(|| index_error_to_failure(&IndexError::NotSupported))?
                    .stage_bootstrap(self.resources, complete)
                    .await?;
                wire::encode(&())
            }
            request::LIVE_ROWS_READ_PAGE => {
                let (after, limits): (Option<u64>, OutboxPageLimits) = wire::decode(input)?;
                anyhow::ensure!(
                    limits.max_records.get() <= MAX_LIVE_RESULT_PAGE_RECORDS
                        && limits.max_bytes.get() <= abi::MAX_MESSAGE_BYTES,
                    "native live-result page exceeds the record or frame budget"
                );
                let page = self
                    .resources
                    .live_results_writer()
                    .expect("validated atomic live results")
                    .read_page(self.component, after, limits)
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                limits
                    .validate_page(&page)
                    .map_err(|error| index_error_to_failure(&error))?;
                let mut previous = after;
                for (signature, _) in &page {
                    if previous.is_some_and(|previous| *signature <= previous) {
                        return Err(index_error_to_failure(&IndexError::CorruptedData).into());
                    }
                    previous = Some(*signature);
                }
                wire::encode(&page)
            }
            request::LIVE_ROWS_APPLY => {
                let owned: Vec<(u64, Option<Vec<u8>>)> = wire::decode(input)?;
                let mutations: Vec<_> = owned
                    .iter()
                    .map(|(signature, data)| RowMutation {
                        row_signature: *signature,
                        data: data.as_deref(),
                    })
                    .collect();
                self.resources
                    .live_results_writer()
                    .expect("validated atomic live results")
                    .apply_mutations(self.component, &mutations)
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            request::ATOMIC_STATE_INFO => {
                wire::decode::<()>(input)?;
                wire::encode(&AtomicStateInfo {
                    durability: self.resources.durability(),
                    persistent_checkpoints: self
                        .resources
                        .checkpoint_store()
                        .expect("validated atomic checkpoint")
                        .is_persistent(),
                })
            }
            request::CHECKPOINT_GET => {
                let key: String = wire::decode(input)?;
                let checkpoint = self
                    .checkpoint_store()?
                    .read_checkpoint(&CheckpointKey::Metadata(key))
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&checkpoint.map(|value| {
                    (
                        value.sequence,
                        value.source_position.map(|position| position.to_vec()),
                    )
                }))
            }
            request::CHECKPOINT_STAGE => {
                let (key, sequence, position): (String, u64, Option<Vec<u8>>) =
                    wire::decode(input)?;
                anyhow::ensure!(
                    !progress::reserved(&key)
                        && !output::reserved(&key)
                        && !scheduling::reserved(&key),
                    "owned checkpoints require the typed progress operation"
                );
                let position = position.map(bytes::Bytes::from);
                self.checkpoint_store()?
                    .stage_checkpoint(&CheckpointKey::Metadata(key), sequence, position.as_ref())
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            request::RESULT_SEQUENCE_GET => {
                wire::decode::<()>(input)?;
                wire::encode(
                    &self
                        .checkpoint_store()?
                        .read_result_sequence(&OutputPartition::Component(self.component.clone()))
                        .await
                        .map_err(|error| index_error_to_failure(&error))?,
                )
            }
            request::RESULT_SEQUENCE_STAGE => {
                let sequence: u64 = wire::decode(input)?;
                if let Some(output) = self.output {
                    output
                        .stage_head(self.resources, self.component, sequence)
                        .await?;
                }
                self.checkpoint_store()?
                    .stage_result_sequence(
                        &OutputPartition::Component(self.component.clone()),
                        sequence,
                    )
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            request::OUTBOX_READ_PAGE => {
                let (after, limits): (u64, OutboxPageLimits) = wire::decode(input)?;
                anyhow::ensure!(
                    limits.max_records.get() <= MAX_OUTBOX_PAGE_RECORDS
                        && limits.max_bytes.get() <= abi::MAX_MESSAGE_BYTES,
                    "native outbox page exceeds the record or frame budget"
                );
                let page = self
                    .resources
                    .outbox_writer()
                    .expect("validated atomic outbox")
                    .read_page(
                        &OutputPartition::Component(self.component.clone()),
                        after,
                        limits,
                    )
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                limits
                    .validate_page(&page)
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&page)
            }
            request::OUTBOX_APPEND_AND_TRIM => {
                let (sequence, data, retain_from): (u64, Vec<u8>, u64) = wire::decode(input)?;
                let envelope = if let Some(output) = self.output {
                    output
                        .validate_append(self.resources, self.component)
                        .await?;
                    Some(output.prepare_emission(sequence, &data)?)
                } else {
                    None
                };
                let removed = self
                    .resources
                    .outbox_writer()
                    .expect("validated atomic outbox")
                    .append_and_trim(
                        &OutputPartition::Component(self.component.clone()),
                        sequence,
                        &data,
                        retain_from,
                    )
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                if let (Some(output), Some(envelope)) = (self.output, envelope) {
                    output.record_emission(envelope)?;
                }
                wire::encode(&removed)
            }
            request::GET_ELEMENT => {
                let reference: ElementReference = wire::decode(input)?;
                wire::encode(
                    &index
                        .get_element(&reference)
                        .await
                        .map_err(|error| index_error_to_failure(&error))?
                        .as_deref(),
                )
            }
            request::SET_ELEMENT => {
                let (element, slots): (Element, Vec<usize>) = wire::decode(input)?;
                index
                    .set_element(&element, &slots)
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            request::DELETE_ELEMENT => {
                let reference: ElementReference = wire::decode(input)?;
                index
                    .delete_element(&reference)
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            request::GET_SLOT_ELEMENT => {
                let (slot, reference): (usize, ElementReference) = wire::decode(input)?;
                wire::encode(
                    &index
                        .get_slot_element_by_ref(slot, &reference)
                        .await
                        .map_err(|error| index_error_to_failure(&error))?
                        .as_deref(),
                )
            }
            request::SET_JOINS => {
                let joins: Vec<IndexJoin> = wire::decode(input)?;
                self.configured_joins.store(true, Ordering::Release);
                index
                    .set_joins(&joins)
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            request::CLEAR_ELEMENTS => {
                wire::decode::<()>(input)?;
                index
                    .clear()
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            request::GET_ARCHIVE_ELEMENT => {
                let (reference, time): (ElementReference, ElementTimestamp) = wire::decode(input)?;
                wire::encode(
                    &self
                        .resources
                        .indexes()
                        .archive_index
                        .get_element_as_at(&reference, time)
                        .await
                        .map_err(|error| index_error_to_failure(&error))?
                        .as_deref(),
                )
            }
            request::CLEAR_ARCHIVE => {
                wire::decode::<()>(input)?;
                self.resources
                    .indexes()
                    .archive_index
                    .clear()
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            request::ARCHIVE_VERSIONS_OPEN => {
                let (reference, from, to, previous): (
                    ElementReference,
                    ElementTimestamp,
                    ElementTimestamp,
                    bool,
                ) = wire::decode(input)?;
                let stream = self
                    .resources
                    .indexes()
                    .archive_index
                    .get_element_versions(
                        &reference,
                        TimestampRange {
                            from: if previous {
                                TimestampBound::StartFromPrevious(from)
                            } else {
                                TimestampBound::Included(from)
                            },
                            to,
                        },
                    )
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                self.store_stream(stream).await
            }
            request::FUTURE_PUSH => {
                let (push_type, position, signature, reference, original_time, due_time): (
                    PushType,
                    usize,
                    u64,
                    ElementReference,
                    ElementTimestamp,
                    ElementTimestamp,
                ) = wire::decode(input)?;
                wire::encode(
                    &self
                        .resources
                        .indexes()
                        .future_queue
                        .push(
                            push_type,
                            position,
                            signature,
                            &reference,
                            original_time,
                            due_time,
                        )
                        .await
                        .map_err(|error| index_error_to_failure(&error))?,
                )
            }
            request::FUTURE_REMOVE => {
                let (position, signature): (usize, u64) = wire::decode(input)?;
                self.resources
                    .indexes()
                    .future_queue
                    .remove(position, signature)
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            request::FUTURE_POP => {
                wire::decode::<()>(input)?;
                wire::encode(
                    &self
                        .resources
                        .indexes()
                        .future_queue
                        .pop()
                        .await
                        .map_err(|error| index_error_to_failure(&error))?,
                )
            }
            request::FUTURE_PEEK => {
                wire::decode::<()>(input)?;
                wire::encode(
                    &self
                        .resources
                        .indexes()
                        .future_queue
                        .peek_due_time()
                        .await
                        .map_err(|error| index_error_to_failure(&error))?,
                )
            }
            request::FUTURE_CLEAR => {
                wire::decode::<()>(input)?;
                self.resources
                    .indexes()
                    .future_queue
                    .clear()
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            request::RESULT_GET => {
                let (key, owner): (ResultKey, ResultOwner) = wire::decode(input)?;
                wire::encode(
                    &self
                        .resources
                        .indexes()
                        .result_index
                        .get(&key, &owner)
                        .await
                        .map_err(|error| index_error_to_failure(&error))?,
                )
            }
            request::RESULT_SET => {
                let (key, owner, value): (ResultKey, ResultOwner, Option<ValueAccumulator>) =
                    wire::decode(input)?;
                self.resources
                    .indexes()
                    .result_index
                    .set(key, owner, value)
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            request::SLOT_OPEN => {
                let slot: usize = wire::decode(input)?;
                let stream = index
                    .get_slot_elements(slot)
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                self.store_stream(stream).await
            }
            request::SLOT_INBOUND_OPEN | request::SLOT_OUTBOUND_OPEN => {
                let (slot, reference): (usize, ElementReference) = wire::decode(input)?;
                let stream = if code == request::SLOT_INBOUND_OPEN {
                    index.get_slot_elements_by_inbound(slot, &reference).await
                } else {
                    index.get_slot_elements_by_outbound(slot, &reference).await
                }
                .map_err(|error| index_error_to_failure(&error))?;
                self.store_stream(stream).await
            }
            request::SLOT_NEXT => {
                let id: u64 = wire::decode(input)?;
                let mut streams = self.streams.lock().await;
                let stream = streams
                    .active
                    .get_mut(&id)
                    .ok_or_else(|| anyhow::anyhow!("unknown native slot stream"))?;
                match stream.next().await {
                    Some(Ok(element)) => wire::encode(&Some(element.as_ref())),
                    Some(Err(error)) => Err(index_error_to_failure(&error).into()),
                    None => {
                        streams.active.remove(&id);
                        wire::encode(&Option::<&Element>::None)
                    }
                }
            }
            request::SORTED_GET_NEXT => {
                let (set, bits): (u64, Option<u64>) = wire::decode(input)?;
                let value = bits.map(|bits| OrderedFloat(f64::from_bits(bits)));
                let next = self
                    .resources
                    .indexes()
                    .result_index
                    .get_next(set, value)
                    .await
                    .map_err(|error| index_error_to_failure(&error))?
                    .map(|(value, count)| (value.0.to_bits(), count));
                wire::encode(&next)
            }
            request::SORTED_GET_COUNT => {
                let (set, bits): (u64, u64) = wire::decode(input)?;
                let count = self
                    .resources
                    .indexes()
                    .result_index
                    .get_value_count(set, OrderedFloat(f64::from_bits(bits)))
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&count)
            }
            request::SORTED_INCREMENT => {
                let (set, bits, delta): (u64, u64, isize) = wire::decode(input)?;
                self.resources
                    .indexes()
                    .result_index
                    .increment_value_count(set, OrderedFloat(f64::from_bits(bits)), delta)
                    .await
                    .map_err(|error| index_error_to_failure(&error))?;
                wire::encode(&())
            }
            _ => anyhow::bail!("unsupported native indexed processing request"),
        }
    }
}
