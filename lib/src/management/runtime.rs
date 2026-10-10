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
use crate::computation::{instance::InstanceGraph, v1::*};

enum Command {
    NewRequestId(oneshot::Sender<Result<String>>),
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
    ListSnapshots(
        Option<String>,
        std::num::NonZeroUsize,
        oneshot::Sender<Result<Vec<ConfigurationSnapshotSummary>>>,
    ),
    DeleteSnapshot(String, u64, oneshot::Sender<Result<bool>>),
    Close(oneshot::Sender<Result<()>>),
}

pub(crate) struct Management {
    commands: mpsc::Sender<Command>,
    desired: watch::Receiver<CommittedConfiguration>,
    status: watch::Receiver<ManagementStatus>,
    task: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    graph: Arc<InstanceGraph>,
    running: Arc<RwLock<bool>>,
    confirmed: Arc<AtomicBool>,
}

struct Driver {
    instance_id: String,
    graph: Arc<InstanceGraph>,
    running: Arc<RwLock<bool>>,
    shutdown: Arc<AtomicBool>,
    factories: FactoryRegistry,
    resolver: Arc<dyn ManagementResourceResolver>,
    store: Option<Arc<dyn ConfigurationSession>>,
    current: CommittedConfiguration,
    receipts: BTreeMap<String, (DesiredInstance, AcceptanceReceipt)>,
    snapshots: BTreeMap<String, CommittedConfiguration>,
    applied: DesiredTopology,
    desired: watch::Sender<CommittedConfiguration>,
    status: watch::Sender<ManagementStatus>,
    confirmed: Arc<AtomicBool>,
    transition: Option<super::transitions::PreparedRecoveryTransition>,
}

pub(super) struct ManagedResource {
    pub(super) resolver: Arc<dyn ManagementResourceResolver>,
    pub(super) instance_id: String,
    pub(super) graph_id: String,
    pub(super) specification: ResourceSpecification,
    pub(super) configuration: serde_json::Value,
}

#[async_trait::async_trait]
impl ResourceConstructor for ManagedResource {
    async fn construct(&self) -> Result<ResourceHandle> {
        self.construct_with_dependencies(&BTreeMap::new()).await
    }

    async fn construct_with_dependencies(
        &self,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<ResourceHandle> {
        self.resolver
            .resolve_with_dependencies(
                &self.instance_id,
                &self.graph_id,
                &self.specification,
                &self.configuration,
                dependencies,
            )
            .await
    }
}

impl Management {
    pub(crate) fn persistent(&self) -> bool {
        self.status.borrow().persistent
    }

    pub(crate) async fn open(
        instance_id: String,
        graph: Arc<InstanceGraph>,
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
            applied: false,
            converged: false,
            resource_errors: BTreeMap::new(),
        });
        let confirmed = Arc::new(AtomicBool::new(true));
        let mut driver = Driver {
            instance_id,
            graph: graph.clone(),
            running: running.clone(),
            shutdown,
            factories: options.factories,
            resolver: options.resources,
            store,
            applied: DesiredInstance::default().topology,
            current,
            receipts: BTreeMap::new(),
            snapshots: BTreeMap::new(),
            desired,
            status,
            confirmed: confirmed.clone(),
            transition: None,
        };
        let (commands, receive) = mpsc::channel(32);
        let task = tokio::spawn(async move { driver.run(receive).await });
        let management = Self {
            commands,
            desired: desired_rx,
            status: status_rx,
            task: tokio::sync::Mutex::new(Some(task)),
            graph,
            running,
            confirmed,
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

    pub(crate) fn configuration(&self) -> Result<CommittedConfiguration> {
        anyhow::ensure!(
            self.confirmed.load(Ordering::Acquire),
            ManagementError::ConfigurationUnconfirmed
        );
        Ok(self.desired.borrow().clone())
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
        let handle = self.graph.get().await?;
        status.resource_errors = resource_errors(&handle.observed(), &desired.desired.topology);
        status.converged = status.applied
            && status.error.is_none()
            && status.resource_errors.is_empty()
            && graph_converged(&desired.desired.topology, &handle.observed(), running);
        let latest = self.desired.borrow().revision;
        if latest != desired.revision {
            status.revision = latest;
            status.reconciling = true;
            status.converged = false;
        }
        if !self.confirmed.load(Ordering::Acquire) {
            status
                .error
                .get_or_insert_with(|| ManagementError::ConfigurationUnconfirmed.to_string());
            status.converged = false;
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
    pub(crate) async fn new_request_id(&self) -> Result<String> {
        self.request(Command::NewRequestId).await
    }
    pub(crate) async fn list_snapshots(
        &self,
        after: Option<String>,
        limit: std::num::NonZeroUsize,
    ) -> Result<Vec<ConfigurationSnapshotSummary>> {
        if let Some(after) = &after {
            valid_name(after)?;
        }
        self.request(|reply| Command::ListSnapshots(after, limit, reply))
            .await
    }
    pub(crate) async fn delete_snapshot(&self, name: String, expected: u64) -> Result<bool> {
        valid_name(&name)?;
        self.request(|reply| Command::DeleteSnapshot(name, expected, reply))
            .await
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
    fn unconfirmed(&self, error: &anyhow::Error) {
        self.confirmed.store(false, Ordering::Release);
        self.status.send_modify(|status| {
            status.reconciling = false;
            status.error = Some(format!(
                "configuration state could not be confirmed: {error:#}"
            ));
            status.converged = false;
        });
    }

    fn adopt_committed(&mut self, committed: CommittedConfiguration) -> Result<()> {
        let validation: Result<()> = (|| {
            anyhow::ensure!(
                committed.revision >= self.current.revision,
                "configuration store regressed its committed revision"
            );
            anyhow::ensure!(
                committed.revision != self.current.revision
                    || committed.desired == self.current.desired,
                "configuration store changed desired state without advancing its revision"
            );
            normalize(committed.desired.clone())
                .context("invalid persisted desired configuration")?;
            Ok(())
        })();
        if let Err(error) = validation {
            self.unconfirmed(&error);
            return Err(error);
        }
        let changed = self.current != committed;
        if let Some(transition) = self.transition.take() {
            if changed {
                transition.retire();
            } else {
                transition.resume();
            }
        }
        self.current = committed;
        self.desired.send_replace(self.current.clone());
        let was_confirmed = self.confirmed.swap(true, Ordering::AcqRel);
        if changed || !was_confirmed {
            self.pending();
        }
        Ok(())
    }

    async fn refresh_committed(&mut self) -> Result<()> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let committed = match store.load().await {
            Ok(committed) => committed,
            Err(error) => {
                self.unconfirmed(&error);
                return Err(error.context("configuration state could not be confirmed"));
            }
        };
        self.adopt_committed(committed)
    }

    fn pending(&self) {
        self.status.send_replace(ManagementStatus {
            revision: self.current.revision,
            persistent: self.store.is_some(),
            reconciling: true,
            error: None,
            applied: false,
            converged: false,
            resource_errors: BTreeMap::new(),
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
        let graph = self.graph.clone();
        let _lifecycle = graph.lifecycle.lock().await;
        if !self.confirmed.load(Ordering::Acquire) {
            self.refresh_committed().await?;
        }
        let retry = if let Some(store) = &self.store {
            store.receipt(&request).await?.is_some()
        } else {
            self.receipts.contains_key(&request)
        };
        if self.store.is_none() {
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
        }
        let mut required_transition = None;
        if !retry {
            if expected != self.current.revision {
                return Err(ManagementError::RevisionConflict {
                    expected,
                    actual: self.current.revision,
                }
                .into());
            }
            if let Err(error) = self
                .resolver
                .validate_transition(&self.current.desired.topology, &desired.topology)
            {
                let Some(required) = error.downcast_ref::<RecoveryTransitionRequired>() else {
                    return Err(error);
                };
                if *self.running.read().await
                    || !equivalent(&self.applied, &self.current.desired.topology)?
                {
                    return Err(error);
                }
                required_transition = Some(required.clone());
            }
        }
        let control = graph.get().await?.control();
        let allow_pending = match &desired.retirement {
            Some(authorization)
                if !retry && desired.retirement != self.current.desired.retirement =>
            {
                authorization.validate_transition(
                    self.store.is_some(),
                    self.current.revision,
                    &self.current.desired.topology,
                    required_transition.as_ref(),
                )?;
                true
            }
            _ => false,
        };
        self.protect(&control, &desired.topology).await?;
        let admission =
            check_ownership(&control, &self.applied, &desired.topology).and_then(|()| {
                anyhow::ensure!(
                    desired
                        .topology
                        .components
                        .iter()
                        .all(|node| node.descriptor.id().as_str() != self.instance_id),
                    "instance identity is reserved"
                );
                Ok(())
            });
        if let Err(error) = admission {
            self.protect(&control, &self.current.desired.topology)
                .await?;
            return Err(error);
        }
        if self.store.is_none() && desired != self.current.desired {
            self.current
                .revision
                .checked_add(1)
                .context("configuration revision exhausted")?;
        }
        if let Some(required) = required_transition {
            match required
                .prepare(&control, &self.current.desired.topology, allow_pending)
                .await
            {
                Ok(transition) => self.transition = Some(transition),
                Err(error) => {
                    self.protect(&control, &self.current.desired.topology)
                        .await?;
                    return Err(error);
                }
            }
        }
        if let Some(store) = &self.store {
            let result = store.commit(expected, &request, &desired).await;
            // Also resolves a backend error after a commit: never roll memory
            // backward or assume an uncertain commit was a rejection.
            let committed = match store.load().await {
                Ok(committed) => committed,
                Err(error) => {
                    self.unconfirmed(&error);
                    return Err(error.context("configuration state could not be confirmed"));
                }
            };
            if let Ok(receipt) = &result {
                if receipt.request_id != request
                    || !receipt.durable
                    || receipt.revision > committed.revision
                    || receipt.revision == committed.revision && committed.desired != desired
                {
                    let error = anyhow::anyhow!(
                        "configuration store receipt does not confirm the committed definition"
                    );
                    self.unconfirmed(&error);
                    return Err(error);
                }
            }
            self.adopt_committed(committed)?;
            self.protect(&control, &self.current.desired.topology)
                .await?;
            if result.is_ok() {
                self.pending();
            }
            return result;
        }
        if desired != self.current.desired {
            self.current.revision = self
                .current
                .revision
                .checked_add(1)
                .context("configuration revision exhausted")?;
            self.current.desired = desired.clone();
        }
        if let Some(transition) = self.transition.take() {
            transition.retire();
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

    async fn protect(&self, control: &GraphControl, desired: &DesiredTopology) -> Result<()> {
        control
            .protect_components(
                self.applied
                    .components
                    .iter()
                    .chain(&desired.components)
                    .map(|node| node.descriptor.id().clone())
                    .collect(),
                self.applied
                    .resources
                    .iter()
                    .chain(&desired.resources)
                    .map(|resource| resource.id.clone())
                    .collect(),
            )
            .await?;
        Ok(())
    }

    async fn realize(&mut self, desired: &DesiredTopology) -> ManagementStatus {
        let id = &desired.graph_id;
        let mut status = ManagementStatus {
            revision: self.current.revision,
            persistent: self.store.is_some(),
            reconciling: false,
            applied: false,
            converged: false,
            error: None,
            resource_errors: BTreeMap::new(),
        };
        let result: Result<()> = async {
            let handle = self.graph.get().await?;
            let public_control = handle.control();
            self.protect(&public_control, desired).await?;
            check_ownership(&public_control, &self.applied, desired)?;
            let control = public_control.management_control();
            let actual = control.desired_snapshot();
            let old = actual.select(GraphSelection::All)?;
            let observed = control.observed();
            let mut bindings = TopologyBindings {
                factories: self.factories.clone(),
                deferred_management_validation: true,
                defer_activation: !*self.running.read().await,
                ..Default::default()
            };
            let mut changed_resources = control.retired_resources().await?;
            for resource in &desired.resources {
                let current_resource = observed.resources.get(&resource.id);
                let needs_resource = old.resources.iter().find(|prior| prior.id == resource.id)
                    != Some(resource)
                    || old.resource_configurations.get(&resource.id)
                        != desired.resource_configurations.get(&resource.id)
                    || old.resource_dependencies.get(&resource.id)
                        != desired.resource_dependencies.get(&resource.id)
                    || current_resource.map_or(true, |state| {
                        state.realization != ResourceRealization::Created || state.failure.is_some()
                    });
                if needs_resource {
                    changed_resources.insert(resource.id.clone());
                }
            }
            old.include_resource_dependents(&mut changed_resources);
            desired.include_resource_dependents(&mut changed_resources);
            for resource in &desired.resources {
                if !changed_resources.contains(&resource.id) {
                    continue;
                }
                let configuration = desired
                    .resource_configurations
                    .get(&resource.id)
                    .context("missing provider construction recipe")?;
                bindings.resource_constructors.insert(
                    resource.id.clone(),
                    resource_constructor(
                        self.resolver.clone(),
                        &self.instance_id,
                        id,
                        resource.clone(),
                        configuration.clone(),
                    ),
                );
            }
            let report = control
                .reconcile_components(self.applied.clone(), desired.clone(), bindings)
                .await?;
            status.resource_errors = resource_errors(&control.observed(), desired);
            if report.committed {
                status.applied = true;
                self.applied = desired.clone();
            } else {
                anyhow::bail!("component transition did not commit: {:?}", report.failures);
            }
            self.protect(&public_control, desired).await?;
            if *self.running.read().await {
                let state = control.observed();
                let start: Vec<_> = desired
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
                        desired.components.iter().any(|target| {
                            target.descriptor.id() == *id && !target.lifecycle.auto_start
                        }) && matches!(
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
            status.resource_errors = resource_errors(&observed, desired);
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
        let graph = self.graph.clone();
        let _lifecycle = graph.lifecycle.lock().await;
        self.refresh_committed().await?;
        let desired = self.current.desired.clone();
        let status = self.realize(&desired.topology).await;
        self.status.send_replace(status.clone());
        Ok(status)
    }

    async fn run(&mut self, mut receive: mpsc::Receiver<Command>) {
        while let Some(command) = receive.recv().await {
            match command {
                Command::NewRequestId(reply) => {
                    let result = if self.shutdown.load(Ordering::Acquire) {
                        Err(ManagementError::Closed.into())
                    } else if !self.confirmed.load(Ordering::Acquire) {
                        Err(ManagementError::ConfigurationUnconfirmed.into())
                    } else {
                        match &self.store {
                            Some(store) => store.new_request_id().await,
                            None => Ok(uuid::Uuid::new_v4().to_string()),
                        }
                    };
                    let _ = reply.send(result);
                }
                Command::Apply {
                    expected,
                    request,
                    desired,
                    reply,
                } => {
                    let previous = self.current.revision;
                    let result = self.commit(expected, request, desired).await;
                    let accepted = result.is_ok() || self.current.revision != previous;
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
                Command::ListSnapshots(after, limit, reply) => {
                    let result = match &self.store {
                        Some(store) => store.list_snapshots(after.as_deref(), limit).await,
                        None => Ok(self
                            .snapshots
                            .range::<str, _>((
                                after
                                    .as_deref()
                                    .map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded),
                                std::ops::Bound::Unbounded,
                            ))
                            .take(limit.get())
                            .map(|(name, snapshot)| ConfigurationSnapshotSummary {
                                name: name.clone(),
                                revision: snapshot.revision,
                            })
                            .collect()),
                    };
                    let _ = reply.send(result);
                }
                Command::DeleteSnapshot(name, expected, reply) => {
                    let result = match &self.store {
                        Some(store) => store.delete_snapshot(&name, expected).await,
                        None => match self.snapshots.get(&name) {
                            Some(snapshot) if snapshot.revision != expected => {
                                Err(ManagementError::RevisionConflict {
                                    expected,
                                    actual: snapshot.revision,
                                }
                                .into())
                            }
                            Some(_) => {
                                self.snapshots.remove(&name);
                                Ok(true)
                            }
                            None => Ok(false),
                        },
                    };
                    let _ = reply.send(result);
                }
                Command::Close(reply) => {
                    let result = match &self.store {
                        Some(store) => store.close().await,
                        None => Ok(()),
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
            let stopped = self.graph.shutdown().await;
            if stopped.is_ok() {
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
                log::error!("Dropped managed instance retains store ownership until cleanup succeeds: {stopped:?}");
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    }
}

fn graph_converged(desired: &DesiredTopology, observed: &ObservedGraph, running: bool) -> bool {
    desired.resources.iter().all(|resource| {
        observed.resources.get(&resource.id).is_some_and(|state| {
            state.realization == ResourceRealization::Created && state.failure.is_none()
        })
    }) && desired.components.iter().all(|node| {
        node.descriptor.ports().iter().all(|port| {
            let endpoint = Endpoint::new(node.descriptor.id().clone(), port.id().clone());
            desired
                .relationships
                .iter()
                .any(|edge| edge.definition.from == endpoint || edge.definition.to == endpoint)
        }) && observed
            .components
            .get(node.descriptor.id())
            .is_some_and(|actual| {
                actual.realization == RealizationState::Created
                    && actual.failure.is_none()
                    && if running && node.lifecycle.auto_start {
                        actual.lifecycle == ComponentLifecycle::Running
                    } else {
                        actual.lifecycle == ComponentLifecycle::Stopped
                    }
            })
    }) && desired.relationships.iter().all(|edge| {
        observed
            .relationships
            .get(&edge.definition)
            .is_some_and(|state| state.binding == BindingState::Bound)
    })
}

fn resource_errors(
    observed: &ObservedGraph,
    desired: &DesiredTopology,
) -> BTreeMap<ResourceId, String> {
    observed
        .resources
        .iter()
        .filter(|(id, _)| desired.resources.iter().any(|resource| &resource.id == *id))
        .filter_map(|(id, resource)| {
            resource
                .failure
                .as_ref()
                .map(|failure| (id.clone(), failure.cause.to_string()))
        })
        .collect()
}

fn valid_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty() && name.len() <= 1024,
        "configuration record name must contain 1..=1024 bytes"
    );
    Ok(())
}

pub(crate) fn equivalent(left: &DesiredTopology, right: &DesiredTopology) -> Result<bool> {
    let mut left = left.clone();
    let mut right = right.clone();
    canonical_graph(&mut left);
    canonical_graph(&mut right);
    Ok(left == right)
}

fn canonical_graph(graph: &mut DesiredTopology) {
    graph.revision = GraphRevision(0);
    graph
        .resource_dependencies
        .retain(|_, dependencies| !dependencies.is_empty());
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
    if let Some(authorization) = &desired.retirement {
        authorization.validate_removed(&desired.topology)?;
    }
    {
        let topology = &mut desired.topology;
        anyhow::ensure!(
            topology.graph_id == crate::computation::components::INSTANCE_GRAPH_ID,
            "definition does not belong to the instance ComputationGraph"
        );
        topology.validate_structure()?;
        anyhow::ensure!(
            topology.boundary_relationships.is_empty(),
            "managed components require complete boundary definitions"
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
    Ok(desired)
}

fn check_ownership(
    control: &GraphControl,
    previous: &DesiredTopology,
    desired: &DesiredTopology,
) -> Result<()> {
    let current = control.desired_snapshot();
    for node in &desired.components {
        let id = node.descriptor.id();
        anyhow::ensure!(
            !current.nodes.iter().any(|node| node.descriptor.id() == id)
                || previous
                    .components
                    .iter()
                    .any(|node| node.descriptor.id() == id),
            "desired configuration cannot adopt unmanaged component {id}"
        );
    }
    for resource in &desired.resources {
        anyhow::ensure!(
            !current.resources.contains_key(&resource.id)
                || previous.resources.iter().any(|old| old.id == resource.id),
            "desired configuration cannot adopt unmanaged resource {}",
            resource.id
        );
    }
    Ok(())
}

pub(crate) fn compose_definition(
    mut current: DesiredTopology,
    previous: &DesiredTopology,
    desired: &DesiredTopology,
) -> Result<DesiredTopology> {
    let old_nodes: BTreeSet<_> = previous
        .components
        .iter()
        .map(|node| node.descriptor.id().clone())
        .collect();
    let old_resources: BTreeSet<_> = previous
        .resources
        .iter()
        .map(|resource| resource.id.clone())
        .collect();
    let changed: BTreeSet<_> = previous
        .components
        .iter()
        .filter(|node| {
            desired
                .components
                .iter()
                .find(|next| next.descriptor.id() == node.descriptor.id())
                != Some(*node)
        })
        .map(|node| node.descriptor.id().clone())
        .collect();
    let detached: BTreeSet<_> = changed
        .iter()
        .flat_map(|id| {
            current
                .component_resources
                .get(id)
                .into_iter()
                .flatten()
                .cloned()
        })
        .collect();
    current
        .components
        .retain(|node| !old_nodes.contains(node.descriptor.id()));
    current
        .components
        .extend(desired.components.iter().cloned());
    current
        .resources
        .retain(|resource| !old_resources.contains(&resource.id));
    current.resources.extend(desired.resources.iter().cloned());
    current
        .resource_configurations
        .retain(|id, _| !old_resources.contains(id));
    current
        .resource_configurations
        .extend(desired.resource_configurations.clone());
    current
        .resource_dependencies
        .retain(|id, _| !old_resources.contains(id));
    current
        .resource_dependencies
        .extend(desired.resource_dependencies.clone());
    current
        .relationships
        .retain(|edge| !previous.relationships.contains(edge));
    current
        .relationships
        .extend(desired.relationships.iter().cloned());
    current
        .control_connections
        .retain(|edge| !previous.control_connections.contains(edge));
    current
        .control_connections
        .extend(desired.control_connections.iter().cloned());
    current
        .subscriptions
        .retain(|edge| !previous.subscriptions.contains(edge));
    current
        .subscriptions
        .extend(desired.subscriptions.iter().cloned());
    current
        .readiness_required
        .retain(|id| !old_nodes.contains(id));
    current
        .readiness_required
        .extend(desired.readiness_required.iter().cloned());
    current
        .component_resources
        .retain(|id, _| !changed.contains(id));
    current
        .component_resources
        .extend(desired.component_resources.clone());
    current
        .component_plugins
        .retain(|id, _| !changed.contains(id));
    current
        .component_plugins
        .extend(desired.component_plugins.clone());
    current
        .recovery_requirements
        .retain(|requirement| !previous.recovery_requirements.contains(requirement));
    current
        .recovery_requirements
        .extend(desired.recovery_requirements.iter().cloned());
    current.requirements = PipeRequirements::new(
        current
            .requirements
            .required()
            .iter()
            .chain(desired.requirements.required())
            .copied(),
    );
    let referenced: BTreeSet<_> = current
        .component_resources
        .values()
        .flatten()
        .cloned()
        .chain(current.components.iter().flat_map(|node| {
            match &node.construction {
                ComponentConstruction::Factory(spec) => spec
                    .dependencies
                    .values()
                    .flatten()
                    .cloned()
                    .chain(spec.configuration.values().filter_map(|value| match value {
                        ConfigurationValue::Reference { resource, .. } => Some(resource.clone()),
                        _ => None,
                    }))
                    .collect::<Vec<_>>(),
                ComponentConstruction::External { .. } => Vec::new(),
            }
        }))
        .chain(
            current
                .relationships
                .iter()
                .chain(&current.boundary_relationships)
                .flat_map(|edge| edge.pipe.resource_dependencies().into_keys()),
        )
        .chain(
            current
                .resource_dependencies
                .values()
                .flat_map(|dependencies| dependencies.keys().cloned()),
        )
        .collect();
    let unused: BTreeSet<_> = detached
        .into_iter()
        .filter(|id| id.as_str().starts_with("attached/") && !referenced.contains(id))
        .collect();
    current
        .resources
        .retain(|resource| !unused.contains(&resource.id));
    current
        .resource_configurations
        .retain(|id, _| !unused.contains(id));
    current
        .resource_dependencies
        .retain(|id, _| !unused.contains(id));
    current.validate_structure()?;
    Ok(current)
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::computation::v1::*;

    struct Service(ComponentDescriptor);

    #[async_trait]
    impl ComputationComponent for Service {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.0
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_trait]
    impl ComputationService for Service {
        async fn run(&mut self) -> anyhow::Result<()> {
            std::future::pending().await
        }
    }

    fn definition(ids: &[&str]) -> DesiredTopology {
        let mut builder = ComputationGraph::builder("managed-recovery");
        for id in ids {
            builder = builder.service(Box::new(Service(
                ComponentDescriptor::try_new(ComponentId::try_new(*id).unwrap(), vec![]).unwrap(),
            )));
        }
        builder
            .build()
            .unwrap()
            .snapshot()
            .select(GraphSelection::All)
            .unwrap()
    }

    fn requirement(id: &str) -> RecoveryRequirement {
        RecoveryRequirement {
            consumer: ComponentId::try_new(id).unwrap(),
            scope: RecoveryScope::MemoryLifetime,
            guarantees: [RecoveryGuarantee::Acceptance].into(),
        }
    }

    #[test]
    fn managed_composition_preserves_unmanaged_assertions_and_applies_managed_assertions() {
        let mut previous = definition(&["managed"]);
        previous.recovery_requirements.push(requirement("managed"));
        let mut current = definition(&["managed", "manual"]);
        current.recovery_requirements = vec![requirement("managed"), requirement("manual")];
        let mut desired = previous.clone();
        desired.recovery_requirements[0]
            .guarantees
            .insert(RecoveryGuarantee::Replay);
        let composed = compose_definition(current, &previous, &desired).unwrap();
        assert_eq!(composed.recovery_requirements.len(), 2);
        assert!(composed
            .recovery_requirements
            .contains(&requirement("manual")));
        assert!(composed
            .recovery_requirements
            .contains(&desired.recovery_requirements[0]));
        assert!(!composed
            .recovery_requirements
            .contains(&requirement("managed")));
    }

    #[tokio::test]
    async fn detached_dependent_resources_leave_no_dangling_recipes_or_dependencies() {
        let previous = definition(&["managed"]);
        let mut current = previous.clone();
        let parent = ResourceId::try_new("parent").unwrap();
        let child = ResourceId::try_new("attached/child").unwrap();
        for id in [&parent, &child] {
            current.resources.push(ResourceSpecification {
                id: id.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: id.as_str().into(),
            });
        }
        current
            .resource_configurations
            .insert(child.clone(), serde_json::json!({}));
        current.resource_dependencies.insert(
            child.clone(),
            BTreeMap::from([(parent.clone(), ResourceRole::StateStore)]),
        );
        current.component_resources.insert(
            ComponentId::try_new("managed").unwrap(),
            BTreeSet::from([child.clone()]),
        );
        current.validate_structure().unwrap();
        let mut target = previous.clone();
        target.components.clear();
        target.allow_incomplete = true;
        let composed = compose_definition(current.clone(), &previous, &target).unwrap();
        assert!(composed.resource_dependencies.is_empty());
        assert!(composed.resource_configurations.is_empty());
        assert_eq!(composed.resources.len(), 1);
        assert_eq!(composed.resources[0].id, parent);

        let ComponentConstruction::External { binding } = &current.components[0].construction
        else {
            panic!("external test component");
        };
        let mut bindings = TopologyBindings::default();
        bindings.components.insert(
            binding.clone(),
            ConstructedComponent::service(Box::new(Service(
                current.components[0].descriptor.clone(),
            ))),
        );
        let mut graph = current.build(bindings).unwrap();
        let run = graph.run().unwrap();
        let control = run.control();
        let (outcome, preview) = tokio::join!(run, async {
            let preview = control
                .preview(
                    control.desired_snapshot().revision,
                    vec![DesiredMutation::RemoveComponents {
                        selection: GraphSelection::All,
                        policy: RemovalPolicy::Cascade,
                    }],
                )
                .await;
            control.cancel();
            preview
        });
        assert!(matches!(outcome, Err(GraphError::Cancelled)));
        let preview = preview.unwrap();
        assert!(preview.desired().resource_dependencies.is_empty());
        assert!(preview.desired().resource_configurations.is_empty());
        assert_eq!(preview.desired().resources.len(), 1);
        assert_eq!(preview.desired().resources[0].id, parent);
        graph.dispose().await.unwrap();
    }
}
