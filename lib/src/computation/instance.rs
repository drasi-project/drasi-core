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

use futures::{stream::FuturesUnordered, StreamExt};
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
    ComputationGraph, DeploymentReport, DesiredMutation, GraphControl, GraphError, GraphRevision,
    GraphSelection, GraphSnapshot, GraphState, ObservedGraph, OperationSummary, StartReport,
    StopReport, TopologyBindings,
};
use crate::error::{DrasiError, Result};

#[derive(Debug, Clone, Copy)]
pub struct ComputationOptions {
    pub auto_start: bool,
}
impl Default for ComputationOptions {
    fn default() -> Self {
        Self { auto_start: true }
    }
}

#[derive(Debug, Clone)]
pub struct ComputationInfo {
    pub id: String,
    pub auto_start: bool,
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
    graphs: Vec<Arc<GraphSlot>>,
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
        for slot in &self.graphs {
            if slot
                .0
                .lock()
                .map_err(|_| operation("registration", "cleanup", "graph ownership poisoned"))?
                .is_none()
            {
                continue;
            }
            let mut graph = slot
                .take()
                .map_err(|error| operation("registration", "cleanup", error))?;
            match graph.dispose().await {
                Ok(()) => {
                    graph.graph.take();
                }
                Err(error) => failures.push(format!("{}: {error}", graph.snapshot().id)),
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
            || self
                .graphs
                .iter()
                .any(|slot| slot.0.lock().map_or(true, |graph| graph.is_some()));
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
    options: ComputationOptions,
    graph: Arc<GraphSlot>,
    control: Arc<OnceLock<GraphControl>>,
    cancel: watch::Sender<bool>,
    driver: watch::Sender<DriverState>,
    task: Mutex<Option<JoinHandle<()>>>,
    cleanup: tokio::sync::Mutex<()>,
    disposed: AtomicBool,
}

impl Entry {
    fn new(mut graph: ComputationGraph, options: ComputationOptions, instance: &str) -> Arc<Self> {
        let id = graph.snapshot().id.to_string();
        graph.set_execution_scope(Arc::from(format!("{instance}::computation::{id}")));
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
            options,
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

    async fn shutdown(&self) -> Result<()> {
        self.request_shutdown();
        let _cleanup = self.cleanup.lock().await;
        if self.disposed.load(Ordering::Acquire) {
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
pub struct ComputationHandle {
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
    pub fn desired(&self) -> Arc<GraphSnapshot> {
        self.control().desired_snapshot()
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
            auto_start: self.entry.options.auto_start,
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
    /// Soft stop: keep the controller, instances and resources for restart.
    /// A failed safe-boundary wait is surfaced even when forced stop succeeds.
    pub async fn stop(&self) -> Result<StopReport> {
        self.deployment().await?;
        let control = self.control();
        let revision = control.desired_snapshot().revision;
        let quiesced = control
            .quiesce_components(revision, GraphSelection::All)
            .await;
        let result = control
            .stop_components(revision, GraphSelection::All)
            .await
            .map_err(|error| operation(self.id(), "stop", error))?;
        quiesced.map_err(|error| operation(self.id(), "quiesce before stop", error))?;
        Ok(result)
    }
    /// Permanent cancellation and awaited cleanup. Failed cleanup remains owned
    /// and can be retried through this handle or DrasiLib shutdown/removal.
    pub async fn shutdown(&self) -> Result<()> {
        self.entry.shutdown().await
    }
}

pub(crate) struct ComputationRegistry {
    pub(crate) lifecycle: tokio::sync::Mutex<()>,
    instance_id: String,
    pub(crate) wal: Mutex<Option<Arc<dyn crate::wal::WalProvider>>>,
    pub(crate) services: Mutex<BTreeMap<String, super::v1::LegacyPluginServices>>,
    entries: Mutex<BTreeMap<String, Arc<Entry>>>,
    closed: AtomicBool,
}

impl ComputationRegistry {
    pub(crate) fn new(instance_id: String) -> Self {
        Self {
            lifecycle: tokio::sync::Mutex::new(()),
            instance_id,
            wal: Mutex::new(None),
            services: Mutex::new(BTreeMap::new()),
            entries: Mutex::new(BTreeMap::new()),
            closed: AtomicBool::new(false),
        }
    }
}

pub(crate) async fn build_instance(
    build: impl std::future::Future<Output = Result<crate::DrasiLib>>,
    graphs: Vec<(ComputationGraph, ComputationOptions)>,
) -> Result<crate::DrasiLib> {
    let mut ids = std::collections::BTreeSet::new();
    let invalid = graphs.iter().find_map(|(graph, _)| {
        if graph.state() != GraphState::Ready {
            Some(DrasiError::invalid_state(
                "only fresh computation graphs can be registered",
            ))
        } else if !ids.insert(graph.snapshot().id.clone()) {
            Some(DrasiError::already_exists(
                "computation",
                graph.snapshot().id.to_string(),
            ))
        } else {
            None
        }
    });
    let built = if let Some(error) = invalid {
        Err(error)
    } else {
        build.await
    };
    let core = match built {
        Ok(core) => core,
        Err(error) => return Err(dispose_rejected(graphs, None, error).await),
    };
    let mut graphs = graphs.into_iter();
    while let Some((graph, options)) = graphs.next() {
        if let Err(error) = core.add_computation_graph(graph, options).await {
            return Err(dispose_rejected(graphs.collect(), Some(core), error).await);
        }
    }
    Ok(core)
}

async fn dispose_rejected(
    graphs: Vec<(ComputationGraph, ComputationOptions)>,
    core: Option<crate::DrasiLib>,
    error: DrasiError,
) -> DrasiError {
    let owner = RejectedComputations {
        core: Mutex::new(core),
        graphs: graphs
            .into_iter()
            .map(|(graph, _)| Arc::new(GraphSlot(Mutex::new(Some(graph)))))
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

pub(crate) async fn reject_graph(
    graph: ComputationGraph,
    options: ComputationOptions,
    error: DrasiError,
) -> DrasiError {
    dispose_rejected(vec![(graph, options)], None, error).await
}

pub(crate) async fn cleanup_failed_instance(
    core: crate::DrasiLib,
    error: DrasiError,
) -> DrasiError {
    dispose_rejected(Vec::new(), Some(core), error).await
}

impl ComputationRegistry {
    pub(crate) fn is_empty(&self) -> Result<bool> {
        Ok(self.entries()?.is_empty())
    }
    pub(crate) fn request_shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        let entries = self.entries.lock().unwrap_or_else(|error| {
            log::error!("Cancelling computations through poisoned registry: {error}");
            error.into_inner()
        });
        for entry in entries.values() {
            entry.request_shutdown();
        }
    }
    fn entries(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Arc<Entry>>>> {
        self.entries
            .lock()
            .map_err(|_| DrasiError::invalid_state("computation registry ownership poisoned"))
    }
    pub(crate) async fn add(
        &self,
        graph: ComputationGraph,
        options: ComputationOptions,
    ) -> Result<ComputationHandle> {
        let id = graph.snapshot().id.to_string();
        if graph.state() != GraphState::Ready {
            return Err(reject_graph(
                graph,
                options,
                DrasiError::invalid_state("only a fresh graph can be registered"),
            )
            .await);
        }
        let mut candidate = Some(graph);
        let result = (|| {
            let mut entries = self.entries()?;
            if self.closed.load(Ordering::Acquire) {
                return Err(DrasiError::invalid_state(
                    "computation registry is shut down",
                ));
            }
            if entries.contains_key(&id) {
                return Err(DrasiError::already_exists("computation", id));
            }
            let entry = Entry::new(
                candidate.take().expect("unregistered graph"),
                options,
                &self.instance_id,
            );
            entries.insert(id, entry.clone());
            Ok(entry)
        })();
        let entry = match result {
            Ok(entry) => entry,
            Err(error) => {
                return Err(
                    reject_graph(candidate.take().expect("rejected graph"), options, error).await,
                )
            }
        };
        entry.ready().await
    }
    pub(crate) async fn get(&self, id: &str) -> Result<ComputationHandle> {
        let entry = self
            .entries()?
            .get(id)
            .cloned()
            .ok_or_else(|| DrasiError::component_not_found("computation", id))?;
        entry.ready().await
    }
    pub(crate) async fn list(&self) -> Result<Vec<ComputationHandle>> {
        let entries: Vec<_> = self.entries()?.values().cloned().collect();
        let mut result = Vec::new();
        for entry in entries {
            result.push(entry.ready().await?);
        }
        Ok(result)
    }
    pub(crate) async fn remove(&self, id: &str) -> Result<()> {
        let handle = self.get(id).await?;
        handle.shutdown().await?;
        let mut entries = self.entries()?;
        if entries
            .get(id)
            .is_some_and(|entry| Arc::ptr_eq(entry, &handle.entry))
        {
            entries.remove(id);
        }
        Ok(())
    }
    pub(crate) async fn start_auto(&self) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        let mut pending: FuturesUnordered<_> = self
            .list()
            .await?
            .into_iter()
            .filter(|handle| handle.entry.options.auto_start)
            .map(|handle| async move {
                let result = handle.start().await;
                (handle, result)
            })
            .collect();
        while let Some((handle, result)) = pending.next().await {
            match result {
                Ok(report) if report.summary == OperationSummary::Completed => {}
                Ok(_) => failures.push(format!("{}: incomplete startup", handle.id())),
                Err(error) => failures.push(format!("{}: {error}", handle.id())),
            }
        }
        if !failures.is_empty() {
            anyhow::bail!("computation startup failed: {}", failures.join("; "));
        }
        Ok(())
    }
    pub(crate) async fn stop_all(&self) -> anyhow::Result<()> {
        self.stop_all_except(None).await
    }
    pub(crate) async fn stop_all_except(&self, excluded: Option<&str>) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        let mut pending: FuturesUnordered<_> = self
            .list()
            .await?
            .into_iter()
            .filter(|handle| !*handle.entry.cancel.borrow())
            .filter(|handle| Some(handle.id()) != excluded)
            .map(|handle| async move {
                let result = handle.stop().await;
                (handle, result)
            })
            .collect();
        while let Some((handle, result)) = pending.next().await {
            match result {
                Ok(report) if report.summary == OperationSummary::Completed => {}
                Ok(_) => failures.push(format!("{}: incomplete stop", handle.id())),
                Err(error) => failures.push(format!("{}: {error}", handle.id())),
            }
        }
        if !failures.is_empty() {
            anyhow::bail!("computation stop failed: {}", failures.join("; "));
        }
        Ok(())
    }
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.request_shutdown();
        let entries: Vec<_> = self.entries()?.values().cloned().collect();
        for entry in &entries {
            entry.request_shutdown();
        }
        let mut failures = Vec::new();
        for entry in entries {
            if let Err(error) = entry.shutdown().await {
                failures.push(format!("{}: {error}", entry.id));
            }
        }
        if !failures.is_empty() {
            anyhow::bail!("computation shutdown failed: {}", failures.join("; "));
        }
        Ok(())
    }
}

impl Drop for ComputationRegistry {
    fn drop(&mut self) {
        for entry in self
            .entries
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .values()
        {
            entry.request_shutdown();
        }
    }
}
