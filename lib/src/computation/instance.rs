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

use std::{
    collections::BTreeMap,
    ops::{Deref, DerefMut},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, OnceLock,
    },
};
use tokio::{sync::watch, task::JoinHandle};

use super::v1::{
    ComponentBatch, ComputationGraph, DeploymentReport, DesiredMutation, GraphControl, GraphError,
    GraphRevision, GraphSelection, GraphState, ObservedGraph, OperationSummary, ResourceHandle,
    ResourceId, ResourceOwnership, StartReport, TopologyBindings,
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
    detail: String,
    owner: RejectedComputations,
}
impl ComputationCleanupError {
    pub fn cause(&self) -> &DrasiError {
        &self.cause
    }
    pub async fn cleanup(&self) -> Result<()> {
        let mut failures = Vec::new();
        if let DrasiError::Internal(cause) = &self.cause {
            if let Some(nested) = cause.downcast_ref::<Self>() {
                if let Err(error) = Box::pin(nested.cleanup()).await {
                    failures.push(error.to_string());
                }
            }
        }
        if let Err(error) = self.owner.shutdown().await {
            failures.push(error.to_string());
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(operation("registration", "cleanup", failures.join("; ")))
        }
    }
}
impl std::fmt::Debug for ComputationCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ComputationCleanupError")
            .field("cause", &self.cause)
            .field("cleanup_error", &self.detail)
            .finish_non_exhaustive()
    }
}
impl std::fmt::Display for ComputationCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}; cleanup remains owned and retryable: {}",
            self.cause, self.detail
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
                Err(error) => failures.push(error.to_string()),
            }
        }
        for batch in &self.batches {
            if let Err(error) = batch.cleanup().await {
                failures.push(error.to_string());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(operation("registration", "cleanup", failures.join("; ")))
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
    Finished(Option<Arc<str>>),
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

struct TaskLease<'a> {
    slot: &'a Mutex<Option<JoinHandle<()>>>,
    task: Option<JoinHandle<()>>,
}
impl Drop for TaskLease<'_> {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            let mut slot = self.slot.lock().unwrap_or_else(|error| error.into_inner());
            *slot = Some(task);
        }
    }
}

struct Entry {
    id: String,
    graph: Arc<GraphSlot>,
    control: Arc<OnceLock<GraphControl>>,
    cancel: watch::Sender<bool>,
    driver: watch::Sender<DriverState>,
    task: Mutex<Option<JoinHandle<()>>>,
    cleanup: tokio::sync::Mutex<()>,
    disposed: AtomicBool,
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
        let publish_state = driver.clone();
        let task = tokio::spawn(async move {
            let outcome = async {
                let mut graph = slot.take()?;
                let run = graph.run()?;
                let control = run.control();
                publish_control
                    .set(control.clone())
                    .map_err(|_| anyhow::anyhow!("driver control already published"))?;
                publish_state.send_replace(DriverState::Running);
                tokio::pin!(run);
                let result = tokio::select! {
                    biased;
                    _ = async { let _ = cancellation.wait_for(|cancelled| *cancelled).await; } => {
                        control.cancel();
                        run.await
                    }
                    result = &mut run => result,
                };
                match result {
                    Ok(()) | Err(GraphError::Cancelled) => Ok::<_, anyhow::Error>(()),
                    Err(error) => Err(error.into()),
                }
            }
            .await;
            publish_state.send_replace(DriverState::Finished(
                outcome.err().map(|error| Arc::from(format!("{error:#}"))),
            ));
        });
        Arc::new(Self {
            id,
            graph,
            control,
            cancel,
            driver,
            task: Mutex::new(Some(task)),
            cleanup: tokio::sync::Mutex::new(()),
            disposed: AtomicBool::new(false),
        })
    }

    async fn ready(self: &Arc<Self>) -> Result<ComputationHandle> {
        let mut state = self.driver.subscribe();
        state
            .wait_for(|state| !matches!(state, DriverState::Initializing))
            .await
            .map_err(|error| operation(&self.id, "deploy", error))?;
        if self.control.get().is_none() {
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
        if let Some(control) = self.control.get() {
            control.cancel();
        }
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
        let task = self
            .task
            .lock()
            .map_err(|_| operation(&self.id, "shutdown", "driver task ownership poisoned"))?
            .take();
        let mut task = TaskLease {
            slot: &self.task,
            task,
        };
        let joined = if let Some(handle) = task.task.as_mut() {
            let result = handle.await;
            task.task.take();
            result.map_err(|error| operation(&self.id, "join", error))
        } else {
            Ok(())
        };
        for batch in pending {
            batch.cleanup().await?;
        }
        let mut graph = self
            .graph
            .take()
            .map_err(|error| operation(&self.id, "shutdown", error))?;
        graph
            .dispose()
            .await
            .map_err(|error| operation(&self.id, "dispose", error))?;
        graph.graph.take();
        self.disposed.store(true, Ordering::Release);
        joined?;
        if let DriverState::Finished(Some(error)) = &*self.driver.borrow() {
            return Err(operation(&self.id, "shutdown", error));
        }
        Ok(())
    }
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.request_shutdown();
        if let Some(task) = self
            .task
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
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
            DriverState::Finished(error) => error.clone(),
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
            .map_err(|error| operation(self.id(), "deploy", error))
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
                .map_err(|error| operation(self.id(), "preview restart", error))?;
            let result = control
                .reconcile(preview, TopologyBindings::default())
                .await
                .map_err(|error| operation(self.id(), "restart", error))?;
            if result.summary != OperationSummary::Completed {
                return Err(operation(
                    self.id(),
                    "restart",
                    "one or more components failed; inspect graph observations",
                ));
            }
        }
        control
            .start_components(control.desired_snapshot().revision, GraphSelection::All)
            .await
            .map_err(|error| operation(self.id(), "start", error))
    }
    pub(crate) async fn shutdown(&self) -> Result<()> {
        self.entry.shutdown(&[]).await
    }
}

pub(crate) struct InstanceGraph {
    pub(crate) lifecycle: tokio::sync::Mutex<()>,
    instance_id: String,
    entry: Mutex<Option<Arc<Entry>>>,
    pending: Mutex<Vec<Arc<PendingComponents>>>,
    closed: AtomicBool,
}

impl InstanceGraph {
    pub(crate) fn new(instance_id: String) -> Self {
        Self {
            lifecycle: tokio::sync::Mutex::new(()),
            instance_id,
            entry: Mutex::new(None),
            pending: Mutex::new(Vec::new()),
            closed: AtomicBool::new(false),
        }
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
        if let Err(error) = core.add_components(batch).await {
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
            detail: cleanup.to_string(),
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
        Err(cleanup) => components_cleanup_error(batch, error, cleanup.to_string()),
    }
}

pub(crate) fn components_cleanup_error(
    batch: Arc<PendingComponents>,
    cause: DrasiError,
    detail: String,
) -> DrasiError {
    DrasiError::Internal(anyhow::Error::new(ComputationCleanupError {
        cause,
        detail,
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
    resources: tokio::sync::Mutex<BTreeMap<ResourceId, ResourceHandle>>,
    control: Option<GraphControl>,
    done: AtomicBool,
}

impl PendingComponents {
    fn new(batch: ComponentBatch, control: Option<GraphControl>) -> Arc<Self> {
        let resources = batch
            .definition
            .resources
            .iter()
            .filter(|resource| resource.ownership == ResourceOwnership::Graph)
            .filter_map(|resource| {
                batch
                    .bindings
                    .resources
                    .get(&resource.id)
                    .map(|handle| (resource.id.clone(), handle.clone()))
            })
            .collect();
        Arc::new(Self {
            value: tokio::sync::Mutex::new(Some(batch)),
            resources: tokio::sync::Mutex::new(resources),
            control,
            done: AtomicBool::new(false),
        })
    }

    pub(crate) async fn take(&self) -> Result<ComponentBatch> {
        self.value
            .lock()
            .await
            .take()
            .ok_or_else(|| DrasiError::invalid_state("component batch was already submitted"))
    }

    fn complete(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }

    pub(crate) async fn cleanup(&self) -> Result<()> {
        let mut resources = self.resources.lock().await;
        let existing = self
            .control
            .as_ref()
            .map(|control| control.registry_snapshot());
        let ids: Vec<_> = resources.keys().cloned().collect();
        let mut failures = Vec::new();
        for id in ids {
            let resource = resources[&id].clone();
            let adopted = existing.as_ref().is_some_and(|snapshot| {
                snapshot
                    .desired
                    .resources
                    .keys()
                    .filter_map(|id| snapshot.resource(id).ok())
                    .any(|current| current.same_shared_instance(&resource))
            });
            if !adopted {
                match tokio::time::timeout(std::time::Duration::from_secs(30), resource.shutdown())
                    .await
                {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        failures.push(format!("{id}: {error:#}"));
                        continue;
                    }
                    Err(error) => {
                        failures.push(format!("{id}: {error}"));
                        continue;
                    }
                }
            }
            resources.remove(&id);
        }
        if !failures.is_empty() {
            return Err(operation("components", "cleanup", failures.join("; ")));
        }
        self.value.lock().await.take();
        self.done.store(true, Ordering::Release);
        Ok(())
    }
}

impl Drop for PendingComponents {
    fn drop(&mut self) {
        if !self.resources.get_mut().is_empty() {
            log::warn!("Component batch dropped before awaited resource cleanup completed");
        }
    }
}
