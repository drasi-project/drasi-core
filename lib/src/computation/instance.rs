// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use futures::FutureExt;
use std::{
    collections::BTreeMap,
    ops::{Deref, DerefMut},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
};
use tokio::{sync::watch, task::JoinHandle};

use super::v1::{
    ComponentBatch, ComputationGraph, DeploymentReport, DesiredMutation, GraphControl, GraphError,
    GraphRevision, GraphRun, GraphSelection, GraphState, ObservedGraph, OperationSummary,
    RejectedResources, ResourceHandle, ResourceId, ResourceOwnership, StartReport,
    TopologyBindings,
};
use crate::error::{DrasiError, Result};

#[derive(Debug, Clone)]
pub struct ComputationInfo {
    pub id: String,
    pub state: GraphState,
    pub revision: GraphRevision,
    pub driver_error: Option<Arc<str>>,
}

/// A failed construction/registration rollback whose asynchronous cleanup is
/// still owned. Downcast the cause of `DrasiError::Internal` to this type and
/// keep it alive while retrying `cleanup()`.
pub struct ComputationCleanupError {
    cause: DrasiError,
    cleanup_error: Option<DrasiError>,
    owner: RejectedComputations,
}
impl ComputationCleanupError {
    pub fn cause(&self) -> &DrasiError {
        &self.cause
    }
    pub fn cleanup_error(&self) -> &DrasiError {
        self.cleanup_error.as_ref().unwrap_or(&self.cause)
    }
    pub async fn cleanup(&self) -> Result<()> {
        let mut failures = Vec::new();
        if let DrasiError::Internal(cause) = &self.cause {
            if let Some(nested) = cause.downcast_ref::<Self>() {
                if let Err(error) = Box::pin(nested.cleanup()).await {
                    failures.push(error.into());
                }
            }
        }
        if let Err(error) = self.owner.shutdown().await {
            failures.push(error.into());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(operation_cause(
                "registration",
                "cleanup",
                crate::error::OperationFailures::new("registration cleanup failed", failures),
            ))
        }
    }
}
impl std::fmt::Debug for ComputationCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputationCleanupError")
            .field("cause", &self.cause)
            .field("cleanup_error", self.cleanup_error())
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for ComputationCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; cleanup remains owned and retryable: {}",
            self.cause,
            self.cleanup_error()
        )
    }
}
impl std::error::Error for ComputationCleanupError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

struct RejectedComputations {
    core: Mutex<Option<crate::DrasiLib>>,
    batches: Vec<Arc<PendingComponents>>,
    cleanup: tokio::sync::Mutex<()>,
}
impl RejectedComputations {
    async fn shutdown(&self) -> Result<()> {
        let _cleanup = self.cleanup.lock().await;
        let mut failures = Vec::new();
        let core = self
            .core
            .lock()
            .map_err(|_| operation("registration", "cleanup", "instance ownership poisoned"))?
            .clone();
        if let Some(core) = core {
            match core.shutdown().await {
                Ok(()) => {
                    self.core
                        .lock()
                        .map_err(|_| {
                            operation("registration", "cleanup", "instance ownership poisoned")
                        })?
                        .take();
                }
                Err(error) => failures.push(error.into()),
            }
        }
        for batch in &self.batches {
            if let Err(error) = batch.cleanup().await {
                failures.push(error.into());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(operation_cause(
                "registration",
                "cleanup",
                crate::error::OperationFailures::new("registration cleanup failed", failures),
            ))
        }
    }
}
impl Drop for RejectedComputations {
    fn drop(&mut self) {
        let pending = self.core.get_mut().map_or(true, |core| core.is_some())
            || self.batches.iter().any(|batch| !batch.complete());
        if pending {
            log::warn!("Computation rollback owner dropped before cleanup completed; asynchronous resource cleanup is not guaranteed");
        }
    }
}

#[derive(Clone)]
enum DriverState {
    Initializing,
    Running,
    Finished(Option<crate::error::RetainedError>),
}

pub(super) struct GraphSlot(pub(super) Mutex<Option<ComputationGraph>>);
pub(super) struct GraphLease {
    slot: Arc<GraphSlot>,
    pub(super) graph: Option<ComputationGraph>,
}
impl GraphSlot {
    pub(super) fn take(self: &Arc<Self>) -> anyhow::Result<GraphLease> {
        let graph = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("graph ownership poisoned"))?
            .take()
            .ok_or_else(|| anyhow::anyhow!("graph is already leased to its driver"))?;
        Ok(GraphLease {
            slot: self.clone(),
            graph: Some(graph),
        })
    }
}
impl Deref for GraphLease {
    type Target = ComputationGraph;
    fn deref(&self) -> &Self::Target {
        self.graph.as_ref().expect("leased graph")
    }
}
impl DerefMut for GraphLease {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.graph.as_mut().expect("leased graph")
    }
}
impl Drop for GraphLease {
    fn drop(&mut self) {
        let mut slot = self.slot.0.lock().unwrap_or_else(|error| {
            log::error!("Returning graph to poisoned ownership slot: {error}");
            error.into_inner()
        });
        *slot = self.graph.take();
    }
}

struct DriverPublication {
    state: watch::Sender<DriverState>,
    graph: String,
    finished: bool,
}
impl DriverPublication {
    fn finish(&mut self, outcome: anyhow::Result<()>) {
        self.state.send_replace(DriverState::Finished(
            outcome.err().map(crate::error::RetainedError::from),
        ));
        self.finished = true;
    }
}
impl Drop for DriverPublication {
    fn drop(&mut self) {
        if !self.finished {
            self.finish(Err(GraphError::Driver {
                graph: Arc::from(self.graph.as_str()),
                run_epoch: 0,
                source: anyhow::anyhow!(
                    "driver exited without a completion report; cleanup remains owned"
                ),
            }
            .into()));
        }
    }
}

struct Entry {
    id: String,
    graph: Arc<GraphSlot>,
    control: Arc<OnceLock<GraphControl>>,
    cancel: watch::Sender<bool>,
    driver: watch::Sender<DriverState>,
    task: tokio::sync::RwLock<Option<JoinHandle<()>>>,
    cleanup: tokio::sync::Mutex<()>,
    disposed: AtomicBool,
}

fn driver_result(result: std::result::Result<(), GraphError>) -> anyhow::Result<()> {
    match result {
        Ok(()) | Err(GraphError::Cancelled) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

async fn finish_driver(
    control: &GraphControl,
    mut run: Pin<&mut GraphRun<'_>>,
) -> anyhow::Result<()> {
    let mut failures = Vec::new();
    if control.observed().deployment.is_some() {
        // The existing driver owns and polls this drain even if a shutdown
        // waiter disappears. Keep real input owners live until producers stop.
        loop {
            let stop = control.stop_for_shutdown(control.desired_snapshot().revision);
            tokio::pin!(stop);
            let report = tokio::select! {
                result = &mut stop => result,
                result = &mut run => return driver_result(result),
            };
            match report {
                // A previously accepted edit may advance the revision before
                // Stop is admitted. This rejection occurs before stop effects.
                Err(GraphError::StaleRevision { expected, actual }) => {
                    log::debug!("Refreshing rejected shutdown revision {expected:?} to {actual:?}");
                    continue;
                }
                Err(GraphError::ControllerClosed) => return driver_result(run.await),
                Ok(report) if report.summary == OperationSummary::Completed => {}
                Ok(report) => failures.push(super::v1::LifecycleReportError::Stop(report).into()),
                Err(error) => failures.push(error.into()),
            }
            break;
        }
    }
    // Deployment may still own an unfinished constructor. Preserve the
    // cancellation path and await all cleanup rather than waiting for readiness.
    control.cancel();
    if let Err(error) = driver_result(run.await) {
        failures.push(error);
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(crate::error::OperationFailures::new("computation shutdown failed", failures).into())
    }
}

impl Entry {
    fn new(mut graph: ComputationGraph, instance: &str) -> Arc<Self> {
        let id = graph.snapshot().id.to_string();
        graph.set_execution_scope(Arc::from(instance));
        let graph = Arc::new(GraphSlot(Mutex::new(Some(graph))));
        let control = Arc::new(OnceLock::new());
        let (cancel, mut cancellation) = watch::channel(false);
        let (driver, _) = watch::channel(DriverState::Initializing);
        let slot = graph.clone();
        let publish_control = control.clone();
        let mut publication = DriverPublication {
            state: driver.clone(),
            graph: id.clone(),
            finished: false,
        };
        let entry = Arc::new(Self {
            id,
            graph,
            control,
            cancel,
            driver,
            task: tokio::sync::RwLock::new(None),
            cleanup: tokio::sync::Mutex::new(()),
            disposed: AtomicBool::new(false),
        });
        let work =
            async move {
                let outcome = std::panic::AssertUnwindSafe(async {
                let mut graph = slot.take()?;
                let run = graph.run()?;
                let control = run.control();
                publish_control
                    .set(control.clone())
                    .map_err(|_| anyhow::anyhow!("driver control already published"))?;
                publication.state.send_replace(DriverState::Running);
                tokio::pin!(run);
                tokio::select! {
                    biased;
                    _ = async { let _ = cancellation.wait_for(|cancelled| *cancelled).await; } => {
                        finish_driver(&control, run.as_mut()).await
                    }
                    result = &mut run => driver_result(result),
                }
            }).catch_unwind().await.unwrap_or_else(|payload| {
                Err(GraphError::Driver {
                    graph: Arc::from(publication.graph.as_str()),
                    run_epoch: 0,
                    source: super::v1::ComponentPanic::new(anyhow::anyhow!(
                        "{}",
                        super::v1::bounded_panic_message(payload)
                    )).into(),
                }.into())
            });
                publication.finish(outcome);
            };
        crate::context::workers::try_spawn_owned_worker(&entry.task, work)
            .expect("new instance owns a vacant driver slot");
        entry
    }

    async fn ready(self: &Arc<Self>) -> Result<ComputationHandle> {
        let mut state = self.driver.subscribe();
        state
            .wait_for(|state| !matches!(state, DriverState::Initializing))
            .await
            .map_err(|error| operation_cause(&self.id, "deploy", error))?;
        if self.control.get().is_none() {
            if let DriverState::Finished(Some(error)) = &*self.driver.borrow() {
                return Err(operation_cause(&self.id, "deploy", error.clone()));
            }
            return Err(operation(
                &self.id,
                "deploy",
                "driver failed before publishing its control",
            ));
        }
        Ok(ComputationHandle {
            entry: self.clone(),
        })
    }

    fn request_shutdown(&self) {
        self.cancel.send_replace(true);
    }

    async fn shutdown(&self, pending: &[Arc<PendingComponents>]) -> Result<()> {
        self.request_shutdown();
        let _cleanup = self.cleanup.lock().await;
        if self.disposed.load(Ordering::Acquire) {
            for batch in pending {
                batch.cleanup().await?;
            }
            return Ok(());
        }
        let mut task = self.task.write().await;
        let joined = if let Some(handle) = task.as_mut() {
            let result = handle.await;
            *task = None;
            result.map_err(|error| operation_cause(&self.id, "join", error))
        } else {
            Ok(())
        };
        drop(task);
        for batch in pending {
            batch.cleanup().await?;
        }
        let mut graph = self
            .graph
            .take()
            .map_err(|error| operation_cause(&self.id, "shutdown", error))?;
        graph
            .dispose()
            .await
            .map_err(|error| operation_cause(&self.id, "dispose", error))?;
        graph.graph.take();
        self.disposed.store(true, Ordering::Release);
        joined?;
        if let DriverState::Finished(Some(error)) = &*self.driver.borrow() {
            return Err(operation_cause(&self.id, "shutdown", error.clone()));
        }
        Ok(())
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.request_shutdown();
        if let Some(task) = self.task.get_mut().take() {
            task.abort();
            if !self.disposed.load(Ordering::Acquire) {
                log::warn!("Computation {} dropped without awaited shutdown; asynchronous plugin cleanup is not guaranteed", self.id);
            }
        }
    }
}

fn operation(id: &str, action: &str, error: impl std::fmt::Display) -> DrasiError {
    DrasiError::operation_failed("computation", id, action, error.to_string())
}

fn operation_cause(id: &str, action: &str, error: impl Into<anyhow::Error>) -> DrasiError {
    let error = error.into();
    operation(id, action, &error).with_cause(error)
}

/// An instance-owned graph handle. The underlying controller remains the only
/// graph state writer; this handle does not acquire a graph lock while it runs.
#[derive(Clone)]
pub(crate) struct ComputationHandle {
    entry: Arc<Entry>,
}

impl ComputationHandle {
    pub fn id(&self) -> &str {
        &self.entry.id
    }
    pub fn control(&self) -> GraphControl {
        self.entry
            .control
            .get()
            .expect("published graph control")
            .clone()
    }
    pub fn observed(&self) -> Arc<ObservedGraph> {
        self.control().observed()
    }
    pub fn inspector(&self) -> super::v1::ComputationInspector {
        self.control().inspector()
    }
    pub fn info(&self) -> ComputationInfo {
        let control = self.control();
        let driver_error = match &*self.entry.driver.borrow() {
            DriverState::Finished(error) => {
                error.as_ref().map(|error| Arc::from(error.to_string()))
            }
            _ => None,
        };
        ComputationInfo {
            id: self.id().to_owned(),
            state: control.state(),
            revision: control.desired_snapshot().revision,
            driver_error,
        }
    }
    pub async fn deployment(&self) -> Result<DeploymentReport> {
        self.control()
            .deployment_report()
            .await
            .map_err(|error| operation_cause(self.id(), "deploy", error))
    }
    pub async fn start(&self) -> Result<StartReport> {
        if *self.entry.cancel.borrow() {
            return Err(DrasiError::invalid_state(
                "computation driver has been shut down",
            ));
        }
        self.deployment().await?;
        let control = self.control();
        let exhausted: Vec<_> = control
            .observed()
            .components
            .iter()
            .filter(|(_, state)| state.exhausted)
            .map(|(id, _)| id.clone())
            .collect();
        if !exhausted.is_empty() {
            let preview = control
                .preview(
                    control.desired_snapshot().revision,
                    vec![DesiredMutation::Restart(GraphSelection::Exact(exhausted))],
                )
                .await
                .map_err(|error| operation_cause(self.id(), "preview restart", error))?;
            let result = control
                .reconcile(preview, TopologyBindings::default())
                .await
                .map_err(|error| operation_cause(self.id(), "restart", error))?;
            if result.summary != OperationSummary::Completed {
                return Err(operation_cause(
                    self.id(),
                    "restart",
                    super::v1::LifecycleReportError::Reconciliation(result),
                ));
            }
        }
        control
            .start_components(control.desired_snapshot().revision, GraphSelection::All)
            .await
            .map_err(|error| operation_cause(self.id(), "start", error))
    }
    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.entry.shutdown(&[]).await
    }
}

/// Instance lifecycle state that is read and changed only while
/// [`InstanceGraph::lifecycle`] is held.
#[derive(Debug, Default)]
pub(crate) struct InstanceLifecycle {
    /// The last stop failed, timed out, or was cancelled. Completed attempts
    /// report Stopped; interrupted waits retain the prior running flag. Either
    /// way, retry cleanup before permitting restart.
    pub(crate) stop_incomplete: bool,
}

pub(crate) struct InstanceGraph {
    /// Serializes instance-wide lifecycle operations.
    pub(crate) lifecycle: Arc<tokio::sync::Mutex<InstanceLifecycle>>,
    startup_task: tokio::sync::RwLock<Option<JoinHandle<()>>>,
    stop_epoch: watch::Sender<u64>,
    instance_id: String,
    entry: Mutex<Option<Arc<Entry>>>,
    pending: Mutex<Vec<Arc<PendingComponents>>>,
    closed: AtomicBool,
}

impl InstanceGraph {
    pub(crate) fn new(instance_id: String) -> Self {
        Self {
            lifecycle: Arc::default(),
            startup_task: tokio::sync::RwLock::new(None),
            stop_epoch: watch::channel(0).0,
            instance_id,
            entry: Mutex::new(None),
            pending: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }
    }

    pub(crate) fn startup_cancellation(&self) -> StartupCancellation {
        let changes = self.stop_epoch.subscribe();
        let epoch = *changes.borrow();
        StartupCancellation {
            changes,
            epoch,
            owner: Arc::from(self.instance_id.as_str()),
        }
    }

    pub(crate) fn interrupt_startup(&self) {
        self.stop_epoch.send_modify(|epoch| {
            *epoch = epoch.checked_add(1).expect("instance stop epoch exhausted");
        });
    }

    pub(crate) fn spawn_startup(
        &self,
        work: impl std::future::Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        crate::context::workers::try_spawn_owned_worker(&self.startup_task, work)?;
        Ok(())
    }

    pub(crate) async fn join_startup(&self) -> anyhow::Result<()> {
        let mut slot = self.startup_task.write().await;
        if let Some(task) = slot.as_mut() {
            let result = task.await;
            *slot = None;
            result?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(crate) struct StartupCancellation {
    changes: watch::Receiver<u64>,
    epoch: u64,
    owner: Arc<str>,
}

impl StartupCancellation {
    pub(crate) async fn cancelled(&mut self) {
        let _ = self.changes.wait_for(|epoch| *epoch != self.epoch).await;
    }

    pub(crate) fn error(&self) -> anyhow::Error {
        super::v1::StartupSupersededError {
            owner: self.owner.clone(),
        }
        .into()
    }
}

pub(crate) async fn build_instance(
    build: impl std::future::Future<Output = Result<crate::DrasiLib>>,
    batches: Vec<ComponentBatch>,
) -> Result<crate::DrasiLib> {
    let mut ids = std::collections::BTreeSet::new();
    let invalid = batches
        .iter()
        .flat_map(|batch| &batch.definition.components)
        .find_map(|component| {
            let id = component.descriptor.id();
            (!ids.insert(id.clone()))
                .then(|| DrasiError::already_exists("component", id.to_string()))
        });
    let built = if let Some(error) = invalid {
        Err(error)
    } else {
        build.await
    };
    let core = match built {
        Ok(core) => core,
        Err(error) => return Err(dispose_rejected(batches, None, error).await),
    };
    let mut batches = batches.into_iter();
    while let Some(batch) = batches.next() {
        let accepted = core.add_components(batch).await.and_then(|report| {
            report
                .ensure_committed()
                .map_err(|error| DrasiError::Internal((*error).into()))
        });
        if let Err(error) = accepted {
            return Err(dispose_rejected(batches.collect(), Some(core), error).await);
        }
    }
    Ok(core)
}

async fn dispose_rejected(
    batches: Vec<ComponentBatch>,
    core: Option<crate::DrasiLib>,
    error: DrasiError,
) -> DrasiError {
    let owner = RejectedComputations {
        core: Mutex::new(core),
        batches: batches
            .into_iter()
            .map(|batch| PendingComponents::new(batch, None))
            .collect(),
        cleanup: tokio::sync::Mutex::new(()),
    };
    match owner.shutdown().await {
        Ok(()) => error,
        Err(cleanup) => DrasiError::Internal(anyhow::Error::new(ComputationCleanupError {
            cause: error,
            cleanup_error: Some(cleanup),
            owner,
        })),
    }
}

pub(crate) async fn reject_components(
    batch: Arc<PendingComponents>,
    error: DrasiError,
) -> DrasiError {
    match batch.cleanup().await {
        Ok(()) => error,
        Err(cleanup) => components_cleanup_error(batch, error, Some(cleanup)),
    }
}

pub(crate) fn components_cleanup_error(
    batch: Arc<PendingComponents>,
    cause: DrasiError,
    cleanup_error: Option<DrasiError>,
) -> DrasiError {
    DrasiError::Internal(anyhow::Error::new(ComputationCleanupError {
        cause,
        cleanup_error,
        owner: RejectedComputations {
            core: Mutex::new(None),
            batches: vec![batch],
            cleanup: tokio::sync::Mutex::new(()),
        },
    }))
}

pub(crate) async fn cleanup_failed_instance(
    core: crate::DrasiLib,
    error: DrasiError,
) -> DrasiError {
    dispose_rejected(Vec::new(), Some(core), error).await
}

impl InstanceGraph {
    pub(crate) fn request_shutdown(&self) {
        self.interrupt_startup();
        self.closed.store(true, Ordering::Release);
        let entry = self.entry.lock().unwrap_or_else(|error| {
            log::error!("Cancelling computation through poisoned ownership: {error}");
            error.into_inner()
        });
        if let Some(entry) = entry.as_ref() {
            entry.request_shutdown();
        }
    }
    pub(crate) async fn initialize(&self, graph: ComputationGraph) -> Result<ComputationHandle> {
        let entry = {
            let mut current = self
                .entry
                .lock()
                .map_err(|_| DrasiError::invalid_state("instance graph ownership poisoned"))?;
            if current.is_some() || self.closed.load(Ordering::Acquire) {
                return Err(DrasiError::invalid_state(
                    "instance graph is already initialized or closed",
                ));
            }
            let entry = Entry::new(graph, &self.instance_id);
            *current = Some(entry.clone());
            entry
        };
        entry.ready().await
    }
    pub(crate) async fn get(&self) -> Result<ComputationHandle> {
        let entry = self
            .entry
            .lock()
            .map_err(|_| DrasiError::invalid_state("instance graph ownership poisoned"))?
            .clone()
            .ok_or_else(|| DrasiError::invalid_state("instance graph is not initialized"))?;
        entry.ready().await
    }
    pub(crate) fn retain(
        &self,
        batch: ComponentBatch,
        control: Option<GraphControl>,
    ) -> Arc<PendingComponents> {
        let batch = PendingComponents::new(batch, control);
        let mut pending = self.pending.lock().unwrap_or_else(|error| {
            log::error!("Retaining component ownership through poisoned cleanup state: {error}");
            error.into_inner()
        });
        pending.retain(|batch| !batch.complete());
        pending.push(batch.clone());
        batch
    }
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.request_shutdown();
        self.join_startup().await?;
        let entry = self
            .entry
            .lock()
            .map_err(|_| DrasiError::invalid_state("instance graph ownership poisoned"))?
            .clone();
        let pending = self
            .pending
            .lock()
            .map_err(|_| DrasiError::invalid_state("component cleanup ownership poisoned"))?
            .clone();
        if let Some(entry) = entry {
            entry.shutdown(&pending).await?;
        } else {
            for batch in pending {
                batch.cleanup().await?;
            }
        }
        Ok(())
    }
}

impl Drop for InstanceGraph {
    fn drop(&mut self) {
        if let Some(entry) = self
            .entry
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
        {
            entry.request_shutdown();
        }
    }
}

pub(crate) struct PendingComponents {
    value: tokio::sync::Mutex<Option<ComponentBatch>>,
    resources: Option<Arc<RejectedResources>>,
    control: Option<GraphControl>,
    done: AtomicBool,
    cleanup_started: AtomicBool,
}

impl PendingComponents {
    fn new(batch: ComponentBatch, control: Option<GraphControl>) -> Arc<Self> {
        let resources = RejectedResources::new(
            batch.definition.resources.iter().cloned(),
            &batch.bindings.resources,
        );
        Arc::new(Self {
            value: tokio::sync::Mutex::new(Some(batch)),
            resources,
            control,
            done: AtomicBool::new(false),
            cleanup_started: AtomicBool::new(false),
        })
    }

    pub(crate) async fn take(&self) -> Result<ComponentBatch> {
        let mut value = self.value.lock().await;
        if self.cleanup_started.load(Ordering::Acquire) {
            return Err(DrasiError::invalid_state(
                "component batch cleanup has begun",
            ));
        }
        value
            .take()
            .ok_or_else(|| DrasiError::invalid_state("component batch was already submitted"))
    }

    fn complete(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    pub(crate) async fn cleanup(&self) -> Result<()> {
        {
            let mut value = self.value.lock().await;
            self.cleanup_started.store(true, Ordering::Release);
            if let Some(batch) = value.as_mut() {
                for resource in &batch.definition.resources {
                    if resource.ownership == ResourceOwnership::Graph {
                        batch.bindings.resources.remove(&resource.id);
                    }
                }
            }
        }
        let existing = self
            .control
            .as_ref()
            .map(|control| control.registry_snapshot());
        let handles = existing
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .desired
                    .resources
                    .keys()
                    .filter_map(|id| {
                        snapshot
                            .resource(id)
                            .ok()
                            .map(|handle| (id.clone(), handle))
                    })
                    .collect()
            })
            .unwrap_or_default();
        if let Some(resources) = &self.resources {
            resources
                .dispose(&handles, std::time::Duration::from_secs(30))
                .await
                .map_err(|error| operation_cause("components", "cleanup", error))?;
        }
        let value = self.value.lock().await.take();
        let dropped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value)));
        self.done.store(true, Ordering::Release);
        dropped.map_err(|payload| {
            operation_cause(
                "components",
                "cleanup",
                super::v1::ComponentPanic::new(anyhow::anyhow!(
                    "rejected component batch destruction: {}",
                    super::v1::bounded_panic_message(payload)
                )),
            )
        })
    }
}

impl Drop for PendingComponents {
    fn drop(&mut self) {
        if self
            .resources
            .as_ref()
            .is_some_and(|resources| !resources.complete())
        {
            log::warn!("Component batch dropped before awaited resource cleanup completed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computation::v1::{
        ComponentDescriptor, ComponentId, ComputationComponent, ComputationService, LifecyclePolicy,
    };
    use std::sync::atomic::AtomicUsize;

    struct IdleService {
        descriptor: ComponentDescriptor,
        stops: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl ComputationComponent for IdleService {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            self.stops.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl ComputationService for IdleService {
        async fn run(&mut self) -> anyhow::Result<()> {
            std::future::pending().await
        }
    }

    async fn shutdown_after_accepted_revision_change() -> anyhow::Result<()> {
        let id = ComponentId::try_new("idle")?;
        let stops = Arc::new(AtomicUsize::new(0));
        let mut graph = ComputationGraph::builder("shutdown-revision")
            .service(Box::new(IdleService {
                descriptor: ComponentDescriptor::try_new(id.clone(), vec![])?,
                stops: stops.clone(),
            }))
            .build()?;
        let run = graph.start()?;
        let control = run.control();
        tokio::pin!(run);
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::select! {
                report = control.startup_report() => {
                    assert_eq!(report?.summary, OperationSummary::Completed);
                    Ok(())
                }
                result = &mut run => anyhow::bail!("driver ended during startup: {result:?}"),
            }
        })
        .await??;
        let revision = control.desired_snapshot().revision;
        let change =
            control.set_lifecycle_policy(revision, id, LifecyclePolicy { auto_start: false });
        tokio::pin!(change);
        assert!(futures::poll!(&mut change).is_pending());
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            finish_driver(&control, run.as_mut()),
        )
        .await??;
        assert!(change.await?.0 > revision.0);
        assert_eq!(stops.load(Ordering::SeqCst), 1);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_retries_only_rejected_revision_on_current_thread() -> anyhow::Result<()> {
        shutdown_after_accepted_revision_change().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_retries_only_rejected_revision_on_multiple_threads() -> anyhow::Result<()> {
        shutdown_after_accepted_revision_change().await
    }

    async fn shutdown_after_external_cancellation() -> anyhow::Result<()> {
        for _ in 0..8 {
            let stops = Arc::new(AtomicUsize::new(0));
            let graph = ComputationGraph::builder("external-cancellation")
                .service(Box::new(IdleService {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new("idle")?,
                        vec![],
                    )?,
                    stops: stops.clone(),
                }))
                .build()?;
            let entry = Entry::new(graph, "external-cancellation");
            let handle = entry.ready().await?;
            let control = handle.control();
            assert_eq!(
                control
                    .start_components(control.desired_snapshot().revision, GraphSelection::All)
                    .await?
                    .summary,
                OperationSummary::Completed
            );
            control.cancel();
            entry.shutdown(&[]).await?;
            assert_eq!(stops.load(Ordering::SeqCst), 1);
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_after_cancel_awaits_terminal_result_on_current_thread() -> anyhow::Result<()>
    {
        shutdown_after_external_cancellation().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_after_cancel_awaits_terminal_result_on_multiple_threads() -> anyhow::Result<()>
    {
        shutdown_after_external_cancellation().await
    }

    #[test]
    fn driver_watch_retains_graph_and_provider_causes_across_clones() {
        let error = GraphError::ResourceCleanup {
            resource: ResourceId::try_new("state").unwrap(),
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied").into(),
        };
        let state = DriverState::Finished(Some(crate::error::RetainedError::from(
            anyhow::Error::new(error),
        )));
        let retained = state.clone();
        drop(state);
        let DriverState::Finished(Some(cause)) = retained else {
            panic!("driver failure was lost");
        };
        let error = operation_cause("instance", "shutdown", cause);
        assert!(matches!(
            error.downcast_ref::<GraphError>(),
            Some(GraphError::ResourceCleanup { .. })
        ));
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
