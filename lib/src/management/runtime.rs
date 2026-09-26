// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::{Context, Result};
use tokio::sync::{mpsc, oneshot, watch, RwLock};

use super::*;
use crate::computation::{instance::ComputationRegistry, v1::*};

enum Command {
    Apply {
        expected: u64,
        request: String,
        desired: DesiredInstance,
        reply: oneshot::Sender<Result<AcceptanceReceipt>>,
    },
    Reconcile(oneshot::Sender<Result<ManagementStatus>>),
    Register(Arc<dyn ComponentFactory>, oneshot::Sender<Result<()>>),
    Receipt(String, oneshot::Sender<Result<Option<AcceptanceReceipt>>>),
    Snapshot(String, oneshot::Sender<Result<CommittedConfiguration>>),
    LoadSnapshot(
        String,
        oneshot::Sender<Result<Option<CommittedConfiguration>>>,
    ),
    Close(oneshot::Sender<Result<()>>),
}

pub(crate) struct Management {
    commands: mpsc::Sender<Command>,
    desired: watch::Receiver<CommittedConfiguration>,
    status: watch::Receiver<ManagementStatus>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    registry: Arc<ComputationRegistry>,
    running: Arc<RwLock<bool>>,
}

struct Driver {
    instance_id: String,
    registry: Arc<ComputationRegistry>,
    running: Arc<RwLock<bool>>,
    shutdown: Arc<AtomicBool>,
    factories: FactoryRegistry,
    resolver: Arc<dyn ManagementResourceResolver>,
    store: Option<Arc<dyn ConfigurationSession>>,
    current: CommittedConfiguration,
    receipts: BTreeMap<String, (DesiredInstance, AcceptanceReceipt)>,
    snapshots: BTreeMap<String, CommittedConfiguration>,
    owned: BTreeSet<String>,
    pending_cleanup: Vec<(ResourceId, ResourceHandle)>,
    desired: watch::Sender<CommittedConfiguration>,
    status: watch::Sender<ManagementStatus>,
}

impl Management {
    pub(crate) async fn open(
        instance_id: String,
        registry: Arc<ComputationRegistry>,
        running: Arc<RwLock<bool>>,
        shutdown: Arc<AtomicBool>,
        options: ManagementOptions,
    ) -> Result<Self> {
        let store = match options.store {
            Some(store) => Some(store.open(&instance_id).await?),
            None => None,
        };
        let current = match &store {
            Some(store) => match store.load().await {
                Ok(current) => current,
                Err(error) => {
                    store
                        .close()
                        .await
                        .context("release failed configuration restore")?;
                    return Err(error);
                }
            },
            None => CommittedConfiguration::default(),
        };
        if let Err(error) = normalize(current.desired.clone()) {
            if let Some(store) = &store {
                store.close().await?;
            }
            return Err(error.context("invalid persisted desired configuration"));
        }
        let (desired, desired_rx) = watch::channel(current.clone());
        let (status, status_rx) = watch::channel(ManagementStatus {
            revision: current.revision,
            persistent: store.is_some(),
            reconciling: true,
            error: None,
            graphs: Vec::new(),
        });
        let mut driver = Driver {
            instance_id,
            registry: registry.clone(),
            running: running.clone(),
            shutdown,
            factories: options.factories,
            resolver: options.resources,
            store,
            owned: current
                .desired
                .graphs
                .iter()
                .map(|graph| graph.topology.graph_id.clone())
                .collect(),
            current,
            receipts: BTreeMap::new(),
            snapshots: BTreeMap::new(),
            pending_cleanup: Vec::new(),
            desired,
            status,
        };
        let (commands, receive) = mpsc::channel(32);
        let task = tokio::spawn(async move { driver.run(receive).await });
        let management = Self {
            commands,
            desired: desired_rx,
            status: status_rx,
            task: tokio::sync::Mutex::new(Some(task)),
            registry,
            running,
        };
        // Declarations and failures are visible on return, without requiring
        // successful component realization to open a persisted instance.
        management.reconcile().await?;
        Ok(management)
    }

    async fn request<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T>>) -> Command,
    ) -> Result<T> {
        let (send, receive) = oneshot::channel();
        self.commands
            .send(command(send))
            .await
            .map_err(|_| ManagementError::Closed)?;
        receive
            .await
            .context("management operation interrupted; resolve request ID before retrying")?
    }

    pub(crate) async fn apply(
        &self,
        expected: u64,
        request: String,
        desired: DesiredInstance,
    ) -> Result<AcceptanceReceipt> {
        let desired = normalize(desired)?;
        anyhow::ensure!(
            !request.is_empty() && request.len() <= 1024,
            "request ID must contain 1..=1024 bytes"
        );
        self.request(|reply| Command::Apply {
            expected,
            request,
            desired,
            reply,
        })
        .await
    }

    pub(crate) fn configuration(&self) -> CommittedConfiguration {
        self.desired.borrow().clone()
    }
    pub(crate) async fn status(&self) -> Result<ManagementStatus> {
        let mut status = self.status.borrow().clone();
        let desired = self.desired.borrow().clone();
        if status.revision != desired.revision {
            status.revision = desired.revision;
            status.reconciling = true;
            status.error = None;
        }
        let running = *self.running.read().await;
        let handles = self.registry.list().await?;
        for report in &mut status.graphs {
            let target = desired
                .desired
                .graphs
                .iter()
                .find(|graph| graph.topology.graph_id == report.graph_id);
            let handle = handles.iter().find(|graph| graph.id() == report.graph_id);
            report.converged = match (target, handle) {
                (Some(target), Some(handle)) => {
                    report.applied
                        && report.error.is_none()
                        && report.resource_errors.is_empty()
                        && graph_converged(target, &handle.observed(), running)
                }
                _ => false,
            };
        }
        let latest = self.desired.borrow().revision;
        if latest != desired.revision {
            status.revision = latest;
            status.reconciling = true;
            for graph in &mut status.graphs {
                graph.converged = false;
            }
        }
        Ok(status)
    }
    pub(crate) async fn reconcile(&self) -> Result<ManagementStatus> {
        self.request(Command::Reconcile).await
    }
    pub(crate) async fn register(&self, factory: Arc<dyn ComponentFactory>) -> Result<()> {
        self.request(|reply| Command::Register(factory, reply))
            .await
    }
    pub(crate) async fn receipt(&self, id: String) -> Result<Option<AcceptanceReceipt>> {
        valid_name(&id)?;
        self.request(|reply| Command::Receipt(id, reply)).await
    }
    pub(crate) async fn snapshot(&self, name: String) -> Result<CommittedConfiguration> {
        valid_name(&name)?;
        self.request(|reply| Command::Snapshot(name, reply)).await
    }
    pub(crate) async fn load_snapshot(
        &self,
        name: String,
    ) -> Result<Option<CommittedConfiguration>> {
        valid_name(&name)?;
        self.request(|reply| Command::LoadSnapshot(name, reply))
            .await
    }
    pub(crate) async fn close(&self) -> Result<()> {
        let mut task = self.task.lock().await;
        if task.is_none() {
            return Ok(());
        }
        self.request(Command::Close).await?;
        if let Some(task) = task.take() {
            task.await.context("management driver failed")?;
        }
        Ok(())
    }
}

impl Driver {
    fn pending(&self) {
        self.status.send_replace(ManagementStatus {
            revision: self.current.revision,
            persistent: self.store.is_some(),
            reconciling: true,
            error: None,
            graphs: self
                .current
                .desired
                .graphs
                .iter()
                .map(|graph| GraphManagementStatus {
                    graph_id: graph.topology.graph_id.clone(),
                    applied: false,
                    converged: false,
                    error: None,
                    resource_errors: BTreeMap::new(),
                })
                .collect(),
        });
    }

    async fn commit(
        &mut self,
        expected: u64,
        request: String,
        desired: DesiredInstance,
    ) -> Result<AcceptanceReceipt> {
        anyhow::ensure!(
            !self.shutdown.load(Ordering::Acquire),
            "instance is shut down"
        );
        let registry = self.registry.clone();
        let _lifecycle = registry.lifecycle.lock().await;
        for graph in self.registry.list().await? {
            anyhow::ensure!(
                self.owned.contains(graph.id())
                    || !desired
                        .graphs
                        .iter()
                        .any(|target| target.topology.graph_id == graph.id()),
                "desired configuration cannot adopt an unmanaged graph implicitly",
            );
        }
        if let Some(store) = &self.store {
            let previous = self.current.clone();
            let result = store.commit(expected, &request, &desired).await;
            // Also resolves a backend error after a commit: never roll memory
            // backward or assume an uncertain commit was a rejection.
            let committed = store
                .load()
                .await
                .context("configuration state could not be confirmed")?;
            anyhow::ensure!(
                committed.revision >= previous.revision,
                "configuration store regressed its committed revision"
            );
            if let Ok(receipt) = &result {
                anyhow::ensure!(
                    receipt.durable && receipt.revision <= committed.revision,
                    "configuration store did not confirm durable acceptance"
                );
                if receipt.revision == committed.revision {
                    anyhow::ensure!(
                        committed.desired == desired,
                        "configuration store receipt does not match its committed definition"
                    );
                }
                self.current = committed;
            }
            self.desired.send_replace(self.current.clone());
            if result.is_ok() || previous != self.current {
                self.pending();
            }
            return result;
        }
        if let Some((prior, receipt)) = self.receipts.get(&request) {
            if prior != &desired {
                return Err(ManagementError::RequestConflict.into());
            }
            return Ok(receipt.clone());
        }
        if expected != self.current.revision {
            return Err(ManagementError::RevisionConflict {
                expected,
                actual: self.current.revision,
            }
            .into());
        }
        if desired != self.current.desired {
            self.current.revision = self
                .current
                .revision
                .checked_add(1)
                .context("configuration revision exhausted")?;
            self.current.desired = desired.clone();
        }
        let receipt = AcceptanceReceipt {
            request_id: request.clone(),
            revision: self.current.revision,
            durable: false,
        };
        self.receipts.insert(request, (desired, receipt.clone()));
        self.desired.send_replace(self.current.clone());
        self.pending();
        Ok(receipt)
    }

    async fn cleanup_pending(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        let pending = std::mem::take(&mut self.pending_cleanup);
        for (id, handle) in pending {
            match handle.shutdown().await {
                Ok(()) => {}
                Err(error) => {
                    failures.push(error.to_string());
                    self.pending_cleanup.push((id, handle));
                }
            }
        }
        anyhow::ensure!(
            failures.is_empty(),
            "pending provider cleanup failed: {}",
            failures.join("; ")
        );
        Ok(())
    }

    async fn realize_graph(&mut self, desired: &DesiredGraph) -> GraphManagementStatus {
        let id = &desired.topology.graph_id;
        let mut status = GraphManagementStatus {
            graph_id: id.clone(),
            applied: false,
            converged: false,
            error: None,
            resource_errors: BTreeMap::new(),
        };
        let result: Result<()> = async {
            self.cleanup_pending().await?;
            let current = self
                .registry
                .list()
                .await?
                .into_iter()
                .find(|graph| graph.id() == id);
            let handle = match current {
                Some(handle)
                    if matches!(
                        handle.info().state,
                        GraphState::Failed
                            | GraphState::Cancelled
                            | GraphState::CleanupRequired
                            | GraphState::Completed
                    ) =>
                {
                    self.registry.remove(id).await?;
                    self.registry
                        .add(
                            ComputationGraph::empty(id.as_str())?,
                            ComputationOptions { auto_start: false },
                        )
                        .await?
                }
                Some(handle) => handle,
                None => {
                    let graph = ComputationGraph::empty(id.as_str())?;
                    self.registry
                        .add(graph, ComputationOptions { auto_start: false })
                        .await?
                }
            };
            let public_control = handle.control();
            public_control.protect_configuration();
            let control = public_control.management_control();
            let actual = control.desired_snapshot();
            let mut target = desired.topology.clone();
            target.revision = actual.revision;
            let old = actual.select(GraphSelection::All)?;
            let mut changes = Vec::new();
            if !equivalent(&old, &target)? {
                changes.push(DesiredMutation::SetTopology(target.clone()));
            }
            let observed = control.observed();
            let mut bindings = TopologyBindings {
                factories: self.factories.clone(),
                deferred_management_validation: true,
                defer_activation: !*self.running.read().await || !desired.auto_start,
                ..Default::default()
            };
            let mut resources_changed = false;
            for resource in &target.resources {
                let current_resource = observed.resources.get(&resource.id);
                let needs_resource = old.resources.iter().find(|prior| prior.id == resource.id)
                    != Some(resource)
                    || old.resource_configurations.get(&resource.id)
                        != target.resource_configurations.get(&resource.id)
                    || current_resource.map_or(true, |state| {
                        state.realization != ResourceRealization::Created
                    });
                if !needs_resource {
                    continue;
                }
                resources_changed = true;
                let configuration = target
                    .resource_configurations
                    .get(&resource.id)
                    .context("missing provider construction recipe")?;
                let resolved = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    self.resolver
                        .resolve(&self.instance_id, id, resource, configuration),
                )
                .await
                .map_err(|_| anyhow::anyhow!("resource construction timed out"))
                .and_then(|result| result);
                match resolved {
                    Ok(handle) if handle.role() == resource.role => {
                        if resource.ownership == ResourceOwnership::Graph {
                            self.pending_cleanup
                                .push((resource.id.clone(), handle.clone()));
                        }
                        bindings.resources.insert(resource.id.clone(), handle);
                    }
                    Ok(handle) => {
                        if resource.ownership == ResourceOwnership::Graph {
                            if let Err(error) = handle.shutdown().await {
                                self.pending_cleanup.push((resource.id.clone(), handle));
                                return Err(error.context("cleanup incorrectly typed resource"));
                            }
                        }
                        status.resource_errors.insert(
                            resource.id.clone(),
                            "provider role differs from declaration".into(),
                        );
                    }
                    Err(error) => {
                        status
                            .resource_errors
                            .insert(resource.id.clone(), format!("{error:#}"));
                    }
                }
            }
            if resources_changed && changes.is_empty() {
                changes.push(DesiredMutation::SetTopology(target.clone()));
            }
            for node in &target.components {
                if observed
                    .components
                    .get(node.descriptor.id())
                    .is_some_and(|node| {
                        node.failure.as_ref().is_some_and(|failure| {
                            failure.disposition == FailureDisposition::Retryable
                        })
                    })
                {
                    changes.push(DesiredMutation::Retry(GraphSelection::Exact(vec![node
                        .descriptor
                        .id()
                        .clone()])));
                }
            }
            if !changes.is_empty() {
                let preview = control.preview(actual.revision, changes).await?;
                let result = control.reconcile(preview, bindings).await;
                let publication = control.registry_snapshot();
                self.pending_cleanup.retain(|(id, handle)| {
                    publication
                        .resource(id)
                        .map_or(true, |bound| !bound.same_instance(handle))
                });
                let report = result?;
                if report.committed {
                    self.cleanup_pending().await?;
                    status.applied = true;
                } else {
                    self.cleanup_pending().await?;
                    anyhow::bail!("graph transition did not commit: {:?}", report.failures);
                }
            } else {
                status.applied = true;
            }
            if *self.running.read().await && desired.auto_start {
                let state = control.observed();
                let start: Vec<_> = target
                    .components
                    .iter()
                    .filter(|node| node.lifecycle.auto_start)
                    .filter(|node| {
                        state
                            .components
                            .get(node.descriptor.id())
                            .is_some_and(|state| {
                                state.realization == RealizationState::Created
                                    && state.lifecycle == ComponentLifecycle::Stopped
                            })
                    })
                    .map(|node| node.descriptor.id().clone())
                    .collect();
                if !start.is_empty() {
                    control
                        .start_components(
                            control.desired_snapshot().revision,
                            GraphSelection::Exact(start),
                        )
                        .await?;
                }
            }
            {
                let state = control.observed();
                let running: Vec<_> = state
                    .components
                    .iter()
                    .filter(|(id, node)| {
                        (!desired.auto_start
                            || target.components.iter().any(|target| {
                                target.descriptor.id() == *id && !target.lifecycle.auto_start
                            }))
                            && matches!(
                                node.lifecycle,
                                ComponentLifecycle::Running | ComponentLifecycle::Starting
                            )
                    })
                    .map(|(id, _)| id.clone())
                    .collect();
                if !running.is_empty() {
                    control
                        .stop_components(
                            control.desired_snapshot().revision,
                            GraphSelection::Exact(running),
                        )
                        .await?;
                }
            }
            let observed = control.observed();
            status.converged = status.resource_errors.is_empty()
                && graph_converged(desired, &observed, *self.running.read().await);
            Ok(())
        }
        .await;
        if let Err(error) = result {
            status.error = Some(format!("{error:#}"));
            status.converged = false;
        }
        status
    }

    async fn reconcile(&mut self) -> Result<ManagementStatus> {
        anyhow::ensure!(
            !self.shutdown.load(Ordering::Acquire),
            "instance is shut down"
        );
        let registry = self.registry.clone();
        let _lifecycle = registry.lifecycle.lock().await;
        let desired = self.current.desired.clone();
        let ids: BTreeSet<_> = desired
            .graphs
            .iter()
            .map(|graph| graph.topology.graph_id.clone())
            .collect();
        let mut reports = Vec::new();
        for id in self.owned.difference(&ids).cloned().collect::<Vec<_>>() {
            let present = registry.list().await?.iter().any(|graph| graph.id() == id);
            if present {
                let removal: Result<()> = async {
                    let handle = registry.get(&id).await?;
                    if !matches!(
                        handle.info().state,
                        GraphState::Failed | GraphState::Cancelled | GraphState::CleanupRequired
                    ) {
                        let empty = ComputationGraph::empty(id.as_str())?
                            .snapshot()
                            .select(GraphSelection::All)?;
                        let control = handle.control().management_control();
                        let preview = control
                            .preview(
                                control.desired_snapshot().revision,
                                vec![DesiredMutation::SetTopology(empty)],
                            )
                            .await?;
                        let result = control
                            .reconcile(preview, TopologyBindings::default())
                            .await?;
                        anyhow::ensure!(result.committed, "graph removal cleanup is incomplete");
                    }
                    registry.remove(&id).await?;
                    Ok(())
                }
                .await;
                if let Err(error) = removal {
                    reports.push(GraphManagementStatus {
                        graph_id: id,
                        applied: false,
                        converged: false,
                        error: Some(format!("graph removal remains pending: {error}")),
                        resource_errors: BTreeMap::new(),
                    });
                    continue;
                }
            }
            self.owned.remove(&id);
        }
        self.owned.extend(ids);
        for graph in &desired.graphs {
            reports.push(self.realize_graph(graph).await);
        }
        let status = ManagementStatus {
            revision: self.current.revision,
            persistent: self.store.is_some(),
            reconciling: false,
            error: None,
            graphs: reports,
        };
        self.status.send_replace(status.clone());
        Ok(status)
    }

    async fn run(&mut self, mut receive: mpsc::Receiver<Command>) {
        while let Some(command) = receive.recv().await {
            match command {
                Command::Apply {
                    expected,
                    request,
                    desired,
                    reply,
                } => {
                    let result = self.commit(expected, request, desired).await;
                    let accepted = result.is_ok();
                    let _ = reply.send(result);
                    if accepted {
                        if let Err(error) = self.reconcile().await {
                            log::error!(
                                "Accepted configuration is pending reconciliation: {error}"
                            );
                            self.status.send_modify(|status| {
                                status.reconciling = false;
                                status.error = Some(format!("{error:#}"));
                            });
                        }
                    }
                }
                Command::Reconcile(reply) => {
                    let _ = reply.send(self.reconcile().await);
                }
                Command::Register(factory, reply) => {
                    let result = if self.shutdown.load(Ordering::Acquire) {
                        Err(ManagementError::Closed.into())
                    } else {
                        self.factories
                            .register(factory)
                            .map_err(anyhow::Error::from)
                    };
                    let registered = result.is_ok();
                    let _ = reply.send(result);
                    if registered {
                        if let Err(error) = self.reconcile().await {
                            log::error!("New factory registered, but desired-state reconciliation failed: {error}");
                            self.status.send_modify(|status| {
                                status.reconciling = false;
                                status.error = Some(format!("{error:#}"));
                            });
                        }
                    }
                }
                Command::Receipt(id, reply) => {
                    let result = match &self.store {
                        Some(store) => store.receipt(&id).await,
                        None => Ok(self.receipts.get(&id).map(|(_, receipt)| receipt.clone())),
                    };
                    let _ = reply.send(result);
                }
                Command::Snapshot(name, reply) => {
                    let result = match &self.store {
                        Some(store) => store.snapshot(&name).await,
                        None if self.snapshots.contains_key(&name) => {
                            Err(ManagementError::SnapshotExists(name).into())
                        }
                        None => {
                            self.snapshots.insert(name, self.current.clone());
                            Ok(self.current.clone())
                        }
                    };
                    let _ = reply.send(result);
                }
                Command::LoadSnapshot(name, reply) => {
                    let result = match &self.store {
                        Some(store) => store.load_snapshot(&name).await,
                        None => Ok(self.snapshots.get(&name).cloned()),
                    };
                    let _ = reply.send(result);
                }
                Command::Close(reply) => {
                    let result = self.cleanup_pending().await;
                    let result = match (result, &self.store) {
                        (Ok(()), Some(store)) => store.close().await,
                        (result, _) => result,
                    };
                    let closed = result.is_ok();
                    let _ = reply.send(result);
                    if closed {
                        return;
                    }
                }
            }
        }
        loop {
            let graphs = self.registry.shutdown().await;
            let resources = self.cleanup_pending().await;
            if graphs.is_ok() && resources.is_ok() {
                if let Some(store) = &self.store {
                    if let Err(error) = store.close().await {
                        log::error!("Closing dropped configuration session failed: {error}");
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            } else {
                log::error!("Dropped managed instance retains store ownership until cleanup succeeds: graphs={graphs:?}, resources={resources:?}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}

fn graph_converged(desired: &DesiredGraph, observed: &ObservedGraph, running: bool) -> bool {
    desired.topology.components.iter().all(|node| {
        node.descriptor.ports().iter().all(|port| {
            let endpoint = Endpoint::new(node.descriptor.id().clone(), port.id().clone());
            desired
                .topology
                .relationships
                .iter()
                .any(|edge| edge.definition.from == endpoint || edge.definition.to == endpoint)
        }) && observed
            .components
            .get(node.descriptor.id())
            .is_some_and(|actual| {
                actual.realization == RealizationState::Created
                    && actual.failure.is_none()
                    && if running && desired.auto_start && node.lifecycle.auto_start {
                        actual.lifecycle == ComponentLifecycle::Running
                    } else {
                        actual.lifecycle == ComponentLifecycle::Stopped
                    }
            })
    }) && observed
        .relationships
        .values()
        .all(|edge| edge.binding == BindingState::Bound)
}

fn valid_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 1024,
        "configuration record name must contain 1..=1024 bytes"
    );
    Ok(())
}

fn equivalent(left: &DesiredTopology, right: &DesiredTopology) -> Result<bool> {
    let mut left = left.clone();
    let mut right = right.clone();
    canonical_graph(&mut left);
    canonical_graph(&mut right);
    Ok(left == right)
}

fn canonical_graph(graph: &mut DesiredTopology) {
    graph.revision = GraphRevision(0);
    graph
        .components
        .sort_by(|left, right| left.descriptor.id().cmp(right.descriptor.id()));
    graph
        .resources
        .sort_by(|left, right| left.id.cmp(&right.id));
    graph
        .relationships
        .sort_by(|left, right| left.definition.cmp(&right.definition));
    graph.control_connections.sort();
    graph.subscriptions.sort();
}

pub(super) fn normalize(mut desired: DesiredInstance) -> Result<DesiredInstance> {
    anyhow::ensure!(desired.version == 1, "unsupported desired instance version");
    let mut ids = BTreeSet::new();
    for graph in &mut desired.graphs {
        let topology = &mut graph.topology;
        anyhow::ensure!(
            !topology.graph_id.starts_with("__"),
            "internal graph IDs are reserved"
        );
        anyhow::ensure!(
            ids.insert(topology.graph_id.clone()),
            "duplicate desired graph"
        );
        topology.validate_structure()?;
        anyhow::ensure!(
            topology.boundary_relationships.is_empty(),
            "managed graphs require complete boundary definitions"
        );
        anyhow::ensure!(
            topology
                .components
                .iter()
                .all(|node| matches!(node.construction, ComponentConstruction::Factory(_))),
            "managed components require reconstruction factories, not opaque object bindings"
        );
        anyhow::ensure!(
            topology
                .relationships
                .iter()
                .all(|edge| !matches!(edge.pipe, DesiredPipe::External { .. })),
            "managed pipes require reconstructible definitions"
        );
        anyhow::ensure!(
            topology
                .resources
                .iter()
                .all(|resource| topology.resource_configurations.contains_key(&resource.id)),
            "managed resources require construction recipes"
        );
        canonical_graph(topology);
    }
    desired
        .graphs
        .sort_by(|left, right| left.topology.graph_id.cmp(&right.topology.graph_id));
    Ok(desired)
}
