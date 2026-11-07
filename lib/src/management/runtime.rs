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
use futures::future::BoxFuture;
use tokio::sync::{mpsc, oneshot, watch, RwLock};

use super::*;
use crate::computation::{instance::InstanceGraph, v1::*};
use crate::error::{OperationFailures, RetainedError};

#[cfg(test)]
#[path = "close_tests.rs"]
mod close_tests;

#[path = "prepare.rs"]
mod prepare;
#[path = "update_planning.rs"]
mod update_planning;

pub(crate) enum AuthoringOperation {
    Definitions(ConfigurationRequest),
    Native(Box<dyn FnOnce(&mut ComponentIds) -> GraphResult<DesiredTopology> + Send>),
    Scoped(Box<dyn FnOnce(&mut ComponentIds) -> GraphResult<ComponentBatch> + Send>),
    Provided(ComponentBatch),
    QueryUpdate {
        id: ComponentId,
        generation: ComponentGeneration,
        configuration: crate::config::AssignedQueryConfig,
    },
    Local {
        definitions: Vec<crate::computation::runtime::LocalDefinition>,
        queries: Vec<crate::config::QueryConfig>,
        native: Vec<Arc<crate::computation::instance::PendingComponents>>,
        native_factories: Vec<crate::computation::components::ComponentBatchDeclaration>,
    },
    Edit {
        generations: BTreeMap<ComponentId, ComponentGeneration>,
        edit: Box<dyn FnOnce(&mut DesiredInstance) -> Result<()> + Send>,
        delete_owned_resources: BTreeSet<ComponentId>,
    },
}

enum Command {
    NewRequestId(oneshot::Sender<Result<String>>),
    Accept {
        expected: u64,
        request: String,
        operation: AuthoringOperation,
        activation: crate::computation::instance::StartupCancellation,
        reply: oneshot::Sender<Result<AcceptanceReceipt>>,
    },
    AcceptAuto {
        operation: AuthoringOperation,
        activation: crate::computation::instance::StartupCancellation,
        reply: oneshot::Sender<Result<AcceptanceReceipt>>,
    },
    RestoreSnapshot {
        name: String,
        expected: u64,
        request: String,
        reply: oneshot::Sender<Result<AcceptanceReceipt>>,
    },
    Reconcile(
        crate::computation::instance::StartupCancellation,
        oneshot::Sender<Result<ManagementStatus>>,
    ),
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
    task: tokio::sync::Mutex<ManagementTask>,
    graph: Arc<InstanceGraph>,
    running: Arc<RwLock<bool>>,
    confirmed: Arc<AtomicBool>,
    guarantees: ConfigurationStoreGuarantees,
}

struct ManagementTask {
    driver: tokio::task::JoinHandle<()>,
    close: CloseProgress,
}

enum CloseProgress {
    Open,
    Requested(oneshot::Receiver<Result<()>>),
    Joining(Option<anyhow::Error>),
    Complete(Result<(), RetainedError>),
}

impl ManagementTask {
    fn new(driver: tokio::task::JoinHandle<()>) -> Self {
        Self {
            driver,
            close: CloseProgress::Open,
        }
    }

    async fn close(&mut self, commands: &mpsc::Sender<Command>) -> Result<()> {
        loop {
            match &mut self.close {
                CloseProgress::Open => match commands.reserve().await {
                    Ok(permit) => {
                        let (send, receive) = oneshot::channel();
                        self.close = CloseProgress::Requested(receive);
                        permit.send(Command::Close(send));
                    }
                    Err(_) => {
                        self.close = CloseProgress::Joining(Some(ManagementError::Closed.into()));
                    }
                },
                CloseProgress::Requested(receive) => match receive.await {
                    Ok(Ok(())) => self.close = CloseProgress::Joining(None),
                    Ok(Err(error)) => {
                        // The driver still owns the store and permits another close attempt.
                        self.close = CloseProgress::Open;
                        return Err(error);
                    }
                    Err(error) => {
                        self.close = CloseProgress::Joining(Some(
                            anyhow::Error::new(error)
                                .context("management close acknowledgement interrupted"),
                        ));
                    }
                },
                CloseProgress::Joining(prior) => {
                    let joined = (&mut self.driver).await.context("management driver failed");
                    let result = match (joined, prior.take()) {
                        (Ok(()), None) => Ok(()),
                        (Ok(()), Some(error)) | (Err(error), None) => Err(error),
                        (Err(error), Some(prior)) => Err(error.context(OperationFailures::new(
                            "management close acknowledgement also failed",
                            vec![prior],
                        ))),
                    };
                    self.close = CloseProgress::Complete(result.map_err(RetainedError::from));
                }
                CloseProgress::Complete(result) => {
                    return result.clone().map_err(anyhow::Error::new);
                }
            }
        }
    }
}

struct Driver {
    instance_id: String,
    graph: Arc<InstanceGraph>,
    running: Arc<RwLock<bool>>,
    shutdown: Arc<AtomicBool>,
    factories: FactoryRegistry,
    resolver: Arc<dyn ManagementResourceResolver>,
    ordinary_resolver: Arc<dyn OrdinaryComponentResolver>,
    runtime: Arc<crate::computation::runtime::Runtime>,
    store: Arc<dyn ConfigurationSession>,
    guarantees: ConfigurationStoreGuarantees,
    current: CommittedConfiguration,
    applied: DesiredTopology,
    pending_realization: Option<PendingRealization>,
    pending_bindings: Vec<Arc<crate::computation::instance::PendingComponents>>,
    pending_deprovision: BTreeSet<ComponentId>,
    pending_cleanup_target: Option<DesiredInstance>,
    pending_activation_guard: Option<crate::computation::instance::StartupCancellation>,
    desired: watch::Sender<CommittedConfiguration>,
    status: watch::Sender<ManagementStatus>,
    confirmed: Arc<AtomicBool>,
    transition: Option<super::transitions::PreparedRecoveryTransition>,
}

struct PendingRealization {
    desired: DesiredTopology,
    request: BoxFuture<'static, GraphResult<ReconciliationReport>>,
    bindings: Vec<Arc<crate::computation::instance::PendingComponents>>,
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

async fn close_failed_restore(
    store: &dyn ConfigurationSession,
    error: anyhow::Error,
) -> anyhow::Error {
    match store.close().await {
        Ok(()) => error,
        Err(cleanup) => error.context(crate::error::OperationFailures::new(
            "configuration restore cleanup also failed",
            vec![cleanup],
        )),
    }
}

impl Management {
    pub(crate) fn persistent(&self) -> bool {
        self.guarantees.durable
    }

    pub(crate) async fn open(
        instance_id: String,
        graph: Arc<InstanceGraph>,
        running: Arc<RwLock<bool>>,
        shutdown: Arc<AtomicBool>,
        runtime: Arc<crate::computation::runtime::Runtime>,
        mut options: ManagementOptions,
    ) -> Result<Self> {
        if let Some(factory) = runtime.query_factory() {
            options.factories.select_query_factory(factory)?;
        }
        let provider = options
            .store
            .unwrap_or_else(|| Arc::new(super::memory::MemoryConfigStore::default()));
        let guarantees = provider.guarantees();
        guarantees.validate()?;
        let store = provider.open(&instance_id).await?;
        let current = match store.load().await {
            Ok(current) => current,
            Err(error) => return Err(close_failed_restore(store.as_ref(), error).await),
        };
        if let Err(error) = normalize_for_store(current.desired.clone(), guarantees) {
            let error = error.context("invalid persisted desired configuration");
            return Err(close_failed_restore(store.as_ref(), error).await);
        }
        let (desired, desired_rx) = watch::channel(current.clone());
        let (status, status_rx) = watch::channel(ManagementStatus {
            revision: current.revision,
            persistent: guarantees.durable,
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
            ordinary_resolver: options.ordinary,
            runtime,
            store,
            guarantees,
            applied: DesiredInstance::default().topology,
            pending_realization: None,
            pending_bindings: Vec::new(),
            pending_deprovision: BTreeSet::new(),
            pending_cleanup_target: None,
            pending_activation_guard: None,
            current,
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
            task: tokio::sync::Mutex::new(ManagementTask::new(task)),
            graph,
            running,
            confirmed,
            guarantees,
        };
        // Declarations and failures are visible on return, without requiring
        // successful component realization to open a persisted instance.
        if let Err(error) = management.reconcile().await {
            return Err(match management.close().await {
                Ok(()) => error,
                Err(cleanup) => error.context(crate::error::OperationFailures::new(
                    "configuration restore cleanup also failed",
                    vec![cleanup],
                )),
            });
        }
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
        self.apply_configuration(
            expected,
            request,
            ConfigurationRequest {
                desired: Some(desired),
                additions: Vec::new(),
            },
        )
        .await
    }

    pub(crate) async fn apply_configuration(
        &self,
        expected: u64,
        request: String,
        input: ConfigurationRequest,
    ) -> Result<AcceptanceReceipt> {
        self.accept(expected, request, AuthoringOperation::Definitions(input))
            .await
    }

    pub(crate) async fn accept(
        &self,
        expected: u64,
        request: String,
        operation: AuthoringOperation,
    ) -> Result<AcceptanceReceipt> {
        anyhow::ensure!(
            !request.is_empty() && request.len() <= 1024,
            "request ID must contain 1..=1024 bytes"
        );
        self.request(|reply| Command::Accept {
            expected,
            request,
            operation,
            activation: self.graph.startup_cancellation(),
            reply,
        })
        .await
    }

    pub(crate) async fn accept_auto(
        &self,
        operation: AuthoringOperation,
    ) -> Result<AcceptanceReceipt> {
        self.request(|reply| Command::AcceptAuto {
            operation,
            activation: self.graph.startup_cancellation(),
            reply,
        })
        .await
    }

    pub(crate) async fn wait_declared(&self, id: &ComponentId) -> Result<()> {
        let control = self.graph.get().await?.control();
        let mut observed = control.subscribe_observed();
        let mut status = self.status.clone();
        loop {
            if control.desired_snapshot().component(id).is_some() {
                return Ok(());
            }
            let current = status.borrow_and_update().clone();
            if !current.reconciling {
                if let Some(error) = current.error {
                    anyhow::bail!("accepted component {id} could not be declared: {error}");
                }
                anyhow::ensure!(
                    super::ordinary::identities(&self.desired.borrow().desired)
                        .iter()
                        .any(|entry| &entry.id == id),
                    "accepted component {id} was removed before declaration"
                );
            }
            tokio::select! {
                changed = observed.changed() => { changed?; },
                changed = status.changed() => { changed?; },
            }
        }
    }

    pub(crate) async fn wait_applied(&self, revision: u64) -> Result<ManagementStatus> {
        let mut status = self.status.clone();
        loop {
            let current = status.borrow_and_update().clone();
            if current.revision >= revision && !current.reconciling {
                anyhow::ensure!(
                    current.error.is_none() && current.applied,
                    "accepted configuration could not be realized: {:?}",
                    current.error
                );
                return Ok(current);
            }
            tokio::select! {
                changed = status.changed() => { changed?; },
                _ = self.commands.closed() => return Err(ManagementError::Closed.into()),
            }
        }
    }
    pub(crate) async fn restore_snapshot(
        &self,
        name: String,
        expected: u64,
        request: String,
    ) -> Result<AcceptanceReceipt> {
        self.request(|reply| Command::RestoreSnapshot {
            name,
            expected,
            request,
            reply,
        })
        .await
    }

    pub(crate) async fn create_component(
        &self,
        expected: u64,
        request: String,
        definition: ComponentDefinition,
    ) -> Result<AcceptanceReceipt> {
        self.apply_configuration(
            expected,
            request,
            ConfigurationRequest::adding([NewComponentDefinition::Native(definition)]),
        )
        .await
    }

    pub(crate) async fn create_ordinary(
        &self,
        expected: u64,
        request: String,
        definition: OrdinaryComponentDefinition,
    ) -> Result<AcceptanceReceipt> {
        self.apply_configuration(
            expected,
            request,
            ConfigurationRequest::adding([NewComponentDefinition::Ordinary(definition)]),
        )
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
            status.applied = false;
        }
        let running = *self.running.read().await;
        let handle = self.graph.get().await?;
        let actual = observed_definition(&handle.control(), &desired.desired)?;
        status.resource_errors = resource_errors(
            &handle.observed(),
            actual.as_ref().unwrap_or(&desired.desired.topology),
        );
        status.converged = status.applied
            && status.error.is_none()
            && desired.pending_updates.is_empty()
            && status.resource_errors.is_empty()
            && actual
                .as_ref()
                .is_some_and(|definition| graph_converged(definition, &handle.observed(), running));
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
        self.reconcile_for_startup(self.graph.startup_cancellation())
            .await
    }
    pub(crate) async fn reconcile_for_startup(
        &self,
        cancellation: crate::computation::instance::StartupCancellation,
    ) -> Result<ManagementStatus> {
        self.request(|reply| Command::Reconcile(cancellation, reply))
            .await
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
        self.task.lock().await.close(&self.commands).await
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

    fn validate_committed(&self, committed: &CommittedConfiguration) -> Result<()> {
        let validation: Result<()> =
            (|| {
                anyhow::ensure!(
                    committed.revision >= self.current.revision,
                    "configuration store regressed its committed revision"
                );
                anyhow::ensure!(
                    committed.revision != self.current.revision
                        || committed.desired == self.current.desired,
                    "configuration store changed desired state without advancing its revision"
                );
                normalize_for_store(committed.desired.clone(), self.guarantees)
                    .context("invalid persisted desired configuration")?;
                let mut updating = BTreeSet::new();
                let mut operations = BTreeSet::new();
                for pending in &committed.pending_updates {
                    pending.validate(&committed.desired)?;
                    anyhow::ensure!(
                        updating.insert(pending.component_id.clone())
                            && operations.insert(pending.operation_id.clone()),
                        "configuration store duplicated an accepted update"
                    );
                    if committed.revision == self.current.revision {
                        anyhow::ensure!(self.current.pending_updates.contains(pending),
                        "configuration store changed update intent without advancing its revision");
                    }
                }
                Ok(())
            })();
        if let Err(error) = validation {
            self.unconfirmed(&error);
            return Err(error);
        }
        Ok(())
    }

    fn adopt_committed(&mut self, committed: CommittedConfiguration) -> Result<()> {
        self.validate_committed(&committed)?;
        let changed = self.current.desired != committed.desired
            || self.current.revision != committed.revision;
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
        let committed = match self.store.load().await {
            Ok(committed) => committed,
            Err(error) => {
                self.unconfirmed(&error);
                return Err(error.context("configuration state could not be confirmed"));
            }
        };
        self.validate_committed(&committed)?;
        let completed: Vec<_> = self
            .current
            .pending_updates
            .iter()
            .filter(|old| {
                !committed.pending_updates.iter().any(|pending| {
                    pending.component_id == old.component_id
                        && pending.operation_id == old.operation_id
                })
            })
            .map(crate::management::AcceptedComponentUpdate::completion)
            .collect();
        if !completed.is_empty() {
            anyhow::ensure!(
                committed.revision >= self.current.revision,
                "configuration completion regressed its revision"
            );
            self.graph
                .get()
                .await?
                .control()
                .management_control()
                .confirm_updates(completed)
                .await?;
        }
        if let Some(target) = self.pending_cleanup_target.take() {
            if target != committed.desired {
                self.pending_deprovision.clear();
                for pending in std::mem::take(&mut self.pending_bindings) {
                    pending.cleanup().await?;
                }
            }
        }
        self.adopt_committed(committed)
    }

    fn pending(&self) {
        self.status.send_replace(ManagementStatus {
            revision: self.current.revision,
            persistent: self.guarantees.durable,
            reconciling: true,
            error: None,
            applied: false,
            converged: false,
            resource_errors: BTreeMap::new(),
        });
    }

    async fn commit_prepared(
        &mut self,
        expected: u64,
        request: String,
        desired: DesiredInstance,
        creation: Option<AcceptedComponentCreation>,
    ) -> Result<AcceptanceReceipt> {
        anyhow::ensure!(
            !request.is_empty() && request.len() <= 1024,
            "request ID must contain 1..=1024 bytes"
        );
        anyhow::ensure!(
            !self.shutdown.load(Ordering::Acquire),
            "instance is shut down"
        );
        self.settle_realization().await?;
        let graph = self.graph.clone();
        let _lifecycle = graph.lifecycle.lock().await;
        if !self.confirmed.load(Ordering::Acquire) {
            self.refresh_committed().await?;
        }
        let prior_receipt = self.store.receipt(&request).await?;
        if prior_receipt
            .as_ref()
            .is_some_and(|receipt| receipt.creation != creation)
        {
            return Err(ManagementError::RequestConflict.into());
        }
        let retry = prior_receipt.is_some();
        let mut provided = BTreeMap::new();
        let mut external = BTreeMap::new();
        for pending in &self.pending_bindings {
            provided.extend(pending.resources().await?);
            external.extend(pending.external_bindings().await?);
        }
        if !retry {
            self.runtime.validate_local_records(&desired, &provided)?;
            let publication = self.graph.get().await?.control().registry_snapshot();
            for node in &desired.topology.components {
                if let ComponentConstruction::External { binding } = &node.construction {
                    let current = self
                        .current
                        .desired
                        .topology
                        .components
                        .iter()
                        .find(|current| current.descriptor.id() == node.descriptor.id());
                    let live = current.is_some_and(|current| {
                        current.construction == node.construction
                            && current.descriptor == node.descriptor
                            && current.role == node.role
                    }) && publication
                        .desired
                        .component(node.descriptor.id())
                        .is_some();
                    anyhow::ensure!(
                        live || external
                            .get(binding)
                            .is_some_and(|descriptor| descriptor == &node.descriptor),
                        "opaque component {} is unavailable for reconstruction",
                        node.descriptor.id()
                    );
                }
            }
        }
        let updates = match &prior_receipt {
            Some(receipt) => receipt.updates.clone(),
            None => self.plan_updates(&desired, &request, &provided).await?,
        };
        let mut required_transition = None;
        if !retry {
            if expected != self.current.revision {
                return Err(ManagementError::RevisionConflict {
                    expected,
                    actual: self.current.revision,
                }
                .into());
            }
            super::ordinary::validate_transition(&self.current.desired, &desired)?;
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
                    self.guarantees.durable,
                    self.current.revision,
                    &self.current.desired.topology,
                    required_transition.as_ref(),
                )?;
                true
            }
            _ => false,
        };
        self.protect(&control, &desired).await?;
        let admission =
            check_instance_ownership(&control, &self.applied, &desired).and_then(|()| {
                anyhow::ensure!(
                    super::ordinary::identities(&desired)
                        .iter()
                        .all(|node| node.id.as_str() != self.instance_id),
                    "instance identity is reserved"
                );
                Ok(())
            });
        if let Err(error) = admission {
            self.protect(&control, &self.current.desired).await?;
            return Err(error);
        }
        if let Some(required) = required_transition {
            match required
                .prepare(&control, &self.current.desired.topology, allow_pending)
                .await
            {
                Ok(transition) => self.transition = Some(transition),
                Err(error) => {
                    self.protect(&control, &self.current.desired).await?;
                    return Err(error);
                }
            }
        }
        let result = self
            .store
            .commit(expected, &request, &desired, creation.as_ref(), &updates)
            .await;
        // Also resolves a backend error after a commit: never roll memory
        // backward or assume an uncertain commit was a rejection.
        let committed = match self.store.load().await {
            Ok(committed) => committed,
            Err(error) => {
                self.unconfirmed(&error);
                return Err(error.context("configuration state could not be confirmed"));
            }
        };
        if let Ok(receipt) = &result {
            if receipt.request_id != request
                || receipt.creation != creation
                || receipt.updates != updates
                || receipt.durable != self.guarantees.durable
                || receipt.revision > committed.revision
                || receipt.revision == committed.revision && committed.desired != desired
                || !retry
                    && updates
                        .iter()
                        .any(|update| !committed.pending_updates.contains(update))
            {
                let error = anyhow::anyhow!(
                    "configuration store receipt does not confirm the committed definition"
                );
                self.unconfirmed(&error);
                return Err(error);
            }
        }
        self.adopt_committed(committed)?;
        self.protect(&control, &self.current.desired).await?;
        if result.is_ok() {
            self.pending();
        }
        result
    }

    fn protect<'a>(
        &self,
        control: &'a GraphControl,
        desired: &DesiredInstance,
    ) -> impl std::future::Future<Output = Result<()>> + Send + 'a {
        let components = self
            .applied
            .components
            .iter()
            .chain(&desired.topology.components)
            .map(|node| node.descriptor.id().clone())
            .chain(desired.ordinary.iter().map(|record| record.id.clone()))
            .collect();
        let resources = self
            .applied
            .resources
            .iter()
            .chain(&desired.topology.resources)
            .map(|resource| resource.id.clone())
            .collect();
        async move {
            control.protect_components(components, resources).await?;
            Ok(())
        }
    }

    async fn settle_realization(&mut self) -> Result<Option<ReconciliationReport>> {
        let Some(pending) = self.pending_realization.as_mut() else {
            return Ok(None);
        };
        let result = pending.request.as_mut().await;
        let pending = self
            .pending_realization
            .take()
            .expect("owned reconciliation");
        if result.as_ref().is_ok_and(|report| report.committed) {
            self.applied = pending.desired;
        }
        let mut cleanup = Vec::new();
        for binding in pending.bindings {
            if let Err(error) = binding.cleanup().await {
                cleanup.push(error.into());
            }
        }
        if cleanup.is_empty() {
            result.map(Some).map_err(Into::into)
        } else {
            if let Err(error) = result {
                cleanup.insert(0, error.into());
            }
            Err(OperationFailures::new("accepted component cleanup failed", cleanup).into())
        }
    }

    async fn realize(
        &mut self,
        configuration: &DesiredInstance,
        lifecycle: tokio::sync::OwnedMutexGuard<crate::computation::instance::InstanceLifecycle>,
    ) -> ManagementStatus {
        let mut status = ManagementStatus {
            revision: self.current.revision,
            persistent: self.guarantees.durable,
            reconciling: false,
            applied: false,
            converged: false,
            error: None,
            resource_errors: BTreeMap::new(),
        };
        let result: Result<()> = async {
            let handle = self.graph.get().await?;
            let public_control = handle.control();
            self.protect(&public_control, configuration).await?;
            check_instance_ownership(&public_control, &self.applied, configuration)?;
            let control = public_control.management_control();
            let actual = control.desired_snapshot();
            let old = actual.select(GraphSelection::All)?;
            let observed = control.observed();
            let mut pending_resources = BTreeMap::new();
            for pending in &self.pending_bindings {
                pending_resources.extend(pending.resources().await?);
            }
            let mut compiled = ComponentBatch {
                definition: configuration.topology.clone(),
                bindings: TopologyBindings {
                    factories: self.factories.clone(),
                    deferred_management_validation: true,
                    defer_activation: true,
                    ..Default::default()
                },
            };
            let ordinary = self
                .runtime
                .managed_components(
                    &configuration.ordinary,
                    &configuration.topology,
                    &self.applied,
                    &self.factories,
                    self.ordinary_resolver.clone(),
                    &pending_resources,
                    &self.current.pending_updates,
                )
                .await?;
            crate::computation::runtime::managed::merge(&mut compiled, ordinary)?;
            compiled.definition.resolve_required_relationships();
            let owners = std::mem::take(&mut self.pending_bindings);
            for pending in &owners {
                let pending = pending.take().await?;
                compiled.bindings.merge(pending.bindings)?;
            }
            let desired = &compiled.definition;
            let updates = self
                .update_bindings(configuration, desired, &pending_resources)
                .await?;
            let id = &desired.graph_id;
            let mut bindings = compiled.bindings;
            let mut changed_resources = control.retired_resources().await?;
            for resource in &desired.resources {
                let current_resource = observed.resources.get(&resource.id);
                let needs_resource = old.resources.iter().find(|prior| prior.id == resource.id)
                    != Some(resource)
                    || old.resource_configurations.get(&resource.id)
                        != desired.resource_configurations.get(&resource.id)
                    || old.resource_dependencies.get(&resource.id)
                        != desired.resource_dependencies.get(&resource.id)
                    || current_resource.is_none_or(|state| {
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
                if bindings.resources.contains_key(&resource.id)
                    || bindings.resource_constructors.contains_key(&resource.id)
                {
                    continue;
                }
                let Some(configuration) = desired.resource_configurations.get(&resource.id) else {
                    // Generated ordinary resources carry their constructors/bindings;
                    // unavailable source bindings remain explicit graph dependencies.
                    continue;
                };
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
            let request_control = control.clone();
            let current_bindings = control.registry_snapshot();
            bindings.resources.retain(|id, supplied| {
                !current_bindings
                    .resource(id)
                    .is_ok_and(|current| current.same_shared_instance(supplied))
            });
            let previous = self.applied.clone();
            let next = desired.clone();
            let running = *self.running.read().await;
            let cancellation = self
                .pending_activation_guard
                .take()
                .unwrap_or_else(|| self.graph.startup_cancellation());
            let activation = desired
                .components
                .iter()
                .filter(|node| {
                    let id = node.descriptor.id();
                    observed.components.get(id).map_or(
                        running && node.lifecycle.auto_start,
                        |state| {
                            matches!(
                                state.lifecycle,
                                ComponentLifecycle::Running | ComponentLifecycle::Starting
                            ) || state.failure.is_none()
                                && (running
                                    && node.lifecycle.auto_start
                                    && !state.lifecycle_requested
                                    || state.lifecycle_requested
                                        && !current_bindings.activation_is_deferred(id)
                                        && matches!(
                                            state.realization,
                                            RealizationState::Pending | RealizationState::Creating
                                        ))
                        },
                    )
                })
                .map(|node| (node.descriptor.id().clone(), cancellation.clone()))
                .collect();
            let delete_owned_resources = std::mem::take(&mut self.pending_deprovision);
            self.pending_realization = Some(PendingRealization {
                desired: next.clone(),
                bindings: owners,
                request: Box::pin(async move {
                    request_control
                        .reconcile_components(
                            previous,
                            next,
                            bindings,
                            activation,
                            delete_owned_resources,
                            updates,
                        )
                        .await
                }),
            });
            // Accepted graph work remains owned even if stop interrupts this
            // wait. Do not keep the instance gate across constructors/startup.
            drop(lifecycle);
            let report = self
                .settle_realization()
                .await?
                .expect("submitted reconciliation");
            status.resource_errors = resource_errors(&control.observed(), desired);
            if report.committed {
                status.applied = true;
            } else {
                anyhow::bail!("component transition did not commit: {:?}", report.failures);
            }
            self.finish_ready_updates().await?;
            self.protect(&public_control, configuration).await?;
            let observed = control.observed();
            status.resource_errors = resource_errors(&observed, desired);
            status.converged = status.resource_errors.is_empty()
                && self.current.pending_updates.is_empty()
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
        self.reconcile_cancellable(self.graph.startup_cancellation())
            .await
    }

    async fn reconcile_cancellable(
        &mut self,
        mut cancellation: crate::computation::instance::StartupCancellation,
    ) -> Result<ManagementStatus> {
        let result = tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(cancellation.error()),
            result = self.reconcile_active() => result,
        };
        if let Err(error) = &result {
            self.status.send_modify(|status| {
                status.reconciling = false;
                status.converged = false;
                status.error = Some(format!("{error:#}"));
            });
        }
        result
    }

    async fn reconcile_active(&mut self) -> Result<ManagementStatus> {
        loop {
            anyhow::ensure!(
                !self.shutdown.load(Ordering::Acquire),
                "instance is shut down"
            );
            self.settle_realization().await?;
            self.refresh_committed().await?;
            let normalized = self.normalize_desired(self.current.desired.clone())?;
            if normalized != self.current.desired {
                let request = self.store.new_request_id().await?;
                // Binding names is a configuration write, not an in-memory restore
                // shortcut. Confirm this write before compiling any constructors.
                if let Err(error) = self
                    .commit_prepared(self.current.revision, request, normalized.clone(), None)
                    .await
                {
                    if !self.confirmed.load(Ordering::Acquire) || self.current.desired != normalized
                    {
                        return Err(error);
                    }
                }
            }
            let graph = self.graph.clone();
            let lifecycle = graph.lifecycle.clone().lock_owned().await;
            self.refresh_committed().await?;
            let desired = self.current.desired.clone();
            let status = self.realize(&desired, lifecycle).await;
            self.status.send_replace(status.clone());
            if status.error.is_some()
                || self.normalize_desired(self.current.desired.clone())? == self.current.desired
            {
                return Ok(status);
            }
        }
    }

    async fn run(&mut self, mut receive: mpsc::Receiver<Command>) {
        let mut observations = self
            .graph
            .get()
            .await
            .ok()
            .map(|handle| handle.control().update_observations());
        loop {
            let command = tokio::select! {
                command = receive.recv() => command,
                changed = async {
                    match &mut observations {
                        Some(observations) => observations.changed().await,
                        None => std::future::pending().await,
                    }
                }, if !self.current.pending_updates.is_empty() => {
                    if changed.is_err() { observations = None; }
                    if let Err(error) = self.finish_ready_updates().await {
                        self.status.send_modify(|status| {
                            status.error = Some(format!("accepted update completion is pending: {error:#}"));
                            status.converged = false;
                        });
                    }
                    continue;
                }
            };
            let Some(command) = command else { break };
            match command {
                Command::NewRequestId(reply) => {
                    let result = if self.shutdown.load(Ordering::Acquire) {
                        Err(ManagementError::Closed.into())
                    } else if !self.confirmed.load(Ordering::Acquire) {
                        Err(ManagementError::ConfigurationUnconfirmed.into())
                    } else {
                        self.store.new_request_id().await
                    };
                    let _ = reply.send(result);
                }
                Command::Accept {
                    expected,
                    request,
                    operation,
                    activation,
                    reply,
                } => {
                    let previous = self.current.revision;
                    let result = self.accept(expected, request, operation).await;
                    let accepted = result.is_ok() || self.current.revision != previous;
                    let _ = reply.send(result);
                    if accepted {
                        self.pending_activation_guard = Some(activation);
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
                Command::AcceptAuto {
                    operation,
                    activation,
                    reply,
                } => {
                    let previous = self.current.revision;
                    let result = async {
                        self.refresh_committed().await?;
                        let request = self.store.new_request_id().await?;
                        self.accept(self.current.revision, request, operation).await
                    }
                    .await;
                    let accepted = result.is_ok() || self.current.revision != previous;
                    let _ = reply.send(result);
                    if accepted {
                        self.pending_activation_guard = Some(activation);
                        if let Err(error) = self.reconcile().await {
                            self.status.send_modify(|status| {
                                status.reconciling = false;
                                status.error = Some(format!("{error:#}"));
                            });
                        }
                    }
                }
                Command::RestoreSnapshot {
                    name,
                    expected,
                    request,
                    reply,
                } => {
                    let previous = self.current.revision;
                    let snapshot = self.store.load_snapshot(&name).await;
                    let result = match snapshot {
                        Ok(Some(snapshot)) => {
                            match normalize_for_store(snapshot.desired, self.guarantees) {
                                Ok(desired) => {
                                    self.commit_prepared(expected, request, desired, None).await
                                }
                                Err(error) => Err(error),
                            }
                        }
                        Ok(None) => Err(anyhow::anyhow!("configuration snapshot does not exist")),
                        Err(error) => Err(error),
                    };
                    let accepted = result.is_ok() || self.current.revision != previous;
                    let _ = reply.send(result);
                    if accepted {
                        if let Err(error) = self.reconcile().await {
                            self.status.send_modify(|status| {
                                status.reconciling = false;
                                status.error = Some(format!("{error:#}"));
                            });
                        }
                    }
                }
                Command::Reconcile(cancellation, reply) => {
                    let _ = reply.send(self.reconcile_cancellable(cancellation).await);
                }
                Command::Register(factory, reply) => {
                    let result = if self.shutdown.load(Ordering::Acquire) {
                        Err(ManagementError::Closed.into())
                    } else if self
                        .factories
                        .get(&factory.descriptor().implementation)
                        .is_some_and(|registered| Arc::ptr_eq(registered, &factory))
                    {
                        Ok(())
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
                    let result = self.store.receipt(&id).await;
                    let _ = reply.send(result);
                }
                Command::Snapshot(name, reply) => {
                    let result = self.store.snapshot(&name).await;
                    let _ = reply.send(result);
                }
                Command::LoadSnapshot(name, reply) => {
                    let result = self.store.load_snapshot(&name).await;
                    let _ = reply.send(result);
                }
                Command::ListSnapshots(after, limit, reply) => {
                    let result = self.store.list_snapshots(after.as_deref(), limit).await;
                    let _ = reply.send(result);
                }
                Command::DeleteSnapshot(name, expected, reply) => {
                    let result = self.store.delete_snapshot(&name, expected).await;
                    let _ = reply.send(result);
                }
                Command::Close(reply) => {
                    let result = self.store.close().await;
                    let closed = result.is_ok();
                    let _ = reply.send(result);
                    if closed {
                        return;
                    }
                }
            }
        }
        let mut retry_delay = std::time::Duration::from_secs(1);
        loop {
            let stopped = self.graph.shutdown().await;
            if stopped.is_ok() {
                if let Err(error) = self.store.close().await {
                    log::error!("Closing dropped configuration session failed; ownership retained, retry in {retry_delay:?}: {error}");
                } else {
                    break;
                }
            } else {
                log::error!("Dropped managed instance retains store ownership until cleanup succeeds; retry in {retry_delay:?}: {stopped:?}");
            }
            tokio::time::sleep(retry_delay).await;
            retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(30));
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
                    && match actual.lifecycle {
                        ComponentLifecycle::Running => true,
                        ComponentLifecycle::Stopped => {
                            actual.lifecycle_requested || !running || !node.lifecycle.auto_start
                        }
                        _ => false,
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
            let reason = resource
                .failure
                .as_ref()
                .map(|failure| failure.cause.to_string())
                .or_else(|| {
                    let missing: Vec<_> = desired
                        .resource_dependencies
                        .get(id)
                        .into_iter()
                        .flat_map(|dependencies| dependencies.keys())
                        .filter(|dependency| {
                            !observed.resources.get(*dependency).is_some_and(|state| {
                                state.realization == ResourceRealization::Created
                                    && state.failure.is_none()
                            })
                        })
                        .map(|dependency| format!("resource '{dependency}'"))
                        .collect();
                    (!missing.is_empty()).then(|| format!("Waiting for {}", missing.join(", ")))
                });
            reason.map(|reason| (id.clone(), reason))
        })
        .collect()
}

fn observed_definition(
    control: &GraphControl,
    desired: &DesiredInstance,
) -> Result<Option<DesiredTopology>> {
    let publication = control.registry_snapshot();
    let mut ids: BTreeSet<_> = super::ordinary::identities(desired)
        .into_iter()
        .map(|identity| identity.id)
        .collect();
    if ids
        .iter()
        .any(|id| publication.desired.component(id).is_none())
    {
        return Ok(None);
    }
    for record in &desired.ordinary {
        if matches!(record.component, OrdinaryComponent::Query { .. }) {
            ids.extend(
                crate::computation::runtime::native_query::query_members(&publication, &record.id)
                    .into_keys(),
            );
        }
    }
    let mut definition = publication
        .desired
        .select(GraphSelection::Exact(ids.into_iter().collect()))?;
    let all = publication.desired.select(GraphSelection::All)?;
    let mut resources: BTreeSet<_> = definition
        .resources
        .iter()
        .chain(&desired.topology.resources)
        .map(|resource| resource.id.clone())
        .collect();
    all.include_resource_dependencies(&mut resources);
    if resources
        .iter()
        .any(|id| !publication.desired.resources.contains_key(id))
    {
        return Ok(None);
    }
    definition.resources = all
        .resources
        .into_iter()
        .filter(|resource| resources.contains(&resource.id))
        .collect();
    definition.resource_configurations = all
        .resource_configurations
        .into_iter()
        .filter(|(id, _)| resources.contains(id))
        .collect();
    definition.resource_dependencies = all
        .resource_dependencies
        .into_iter()
        .filter(|(id, _)| resources.contains(id))
        .collect();
    Ok(Some(definition))
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
    graph
        .boundary_relationships
        .sort_by(|left, right| left.definition.cmp(&right.definition));
    graph.control_connections.sort();
    graph.subscriptions.sort();
}

pub(super) fn normalize(desired: DesiredInstance) -> Result<DesiredInstance> {
    normalize_for_store(desired, ConfigurationStoreGuarantees::DURABLE)
}

pub(super) fn normalize_for_store(
    mut desired: DesiredInstance,
    guarantees: ConfigurationStoreGuarantees,
) -> Result<DesiredInstance> {
    anyhow::ensure!(desired.version == 2, "unsupported desired instance version; use fresh UUID configuration and state, without migrating or deleting the old data");
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
        topology.resolve_required_relationships();
        anyhow::ensure!(
            !guarantees.requires_reconstructible_definitions
                || topology
                    .components
                    .iter()
                    .all(|node| matches!(node.construction, ComponentConstruction::Factory(_))),
            "managed components require reconstruction factories, not opaque object bindings"
        );
        anyhow::ensure!(
            !guarantees.requires_reconstructible_definitions
                || topology
                    .relationships
                    .iter()
                    .chain(&topology.boundary_relationships)
                    .all(|edge| !matches!(edge.pipe, DesiredPipe::External { .. })),
            "managed pipes require reconstructible definitions"
        );
        anyhow::ensure!(
            !guarantees.requires_reconstructible_definitions
                || topology
                    .resources
                    .iter()
                    .all(|resource| topology.resource_configurations.contains_key(&resource.id)),
            "managed resources require construction recipes"
        );
        canonical_graph(topology);
    }
    super::ordinary::normalize(&mut desired)?;
    anyhow::ensure!(
        !guarantees.requires_reconstructible_definitions
            || desired
                .ordinary
                .iter()
                .all(|record| !matches!(record.component, OrdinaryComponent::ProcessLocal { .. })),
        "process-local definitions are not reconstructible"
    );
    Ok(desired)
}

fn check_instance_ownership(
    control: &GraphControl,
    previous: &DesiredTopology,
    desired: &DesiredInstance,
) -> Result<()> {
    check_ownership(control, previous, &desired.topology)?;
    let current = control.registry_snapshot();
    let owned: BTreeSet<_> = previous
        .components
        .iter()
        .map(|node| node.descriptor.id())
        .collect();
    for identity in super::ordinary::identities(desired) {
        if current.desired.component(&identity.id).is_some() && !owned.contains(&identity.id) {
            control.check_new_component(&identity.id, &identity.name, true)?;
        }
        if let Some(existing) = current.desired.component_by_name(&identity.name) {
            if existing.descriptor.id() != &identity.id && !owned.contains(existing.descriptor.id())
            {
                return Err(GraphError::DuplicateComponentName {
                    existing: Box::new(current.desired.conflict(existing, &current.observed)),
                }
                .into());
            }
        }
    }
    Ok(())
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
    observation_resources: &BTreeSet<ResourceId>,
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
            match desired
                .components
                .iter()
                .find(|next| next.descriptor.id() == node.descriptor.id())
            {
                Some(next) => {
                    let mut previous = (**node).clone();
                    previous.name = next.name.clone();
                    previous != *next
                }
                None => true,
            }
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
    // A previously unbound owned edge may now be bound in the live graph.
    for relationships in [&mut current.relationships, &mut current.boundary_relationships] {
        relationships.retain(|edge| {
            !previous.relationships.contains(edge)
                && !previous.boundary_relationships.contains(edge)
        });
    }
    current
        .relationships
        .extend(desired.relationships.iter().cloned());
    current
        .boundary_relationships
        .extend(desired.boundary_relationships.iter().cloned());
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
        .filter(|id| observation_resources.contains(id) && !referenced.contains(id))
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
    current.resolve_required_relationships();
    current.validate_structure()?;
    Ok(current)
}

#[cfg(test)]
mod recovery_tests {
    use super::*;

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

    fn component(number: u128) -> ComponentId {
        ComponentId::from_uuid(uuid::Uuid::from_u128(number))
    }

    fn definition(ids: &[(u128, &str)]) -> DesiredTopology {
        let mut builder = ComputationGraph::builder("managed-recovery");
        for (number, name) in ids {
            builder = builder.service(
                ComponentName::try_new(*name).unwrap(),
                Box::new(Service(
                    ComponentDescriptor::try_new(component(*number), vec![]).unwrap(),
                )),
            );
        }
        builder
            .build()
            .unwrap()
            .snapshot()
            .select(GraphSelection::All)
            .unwrap()
    }

    fn requirement(number: u128) -> RecoveryRequirement {
        RecoveryRequirement {
            consumer: component(number),
            scope: RecoveryScope::MemoryLifetime,
            guarantees: [RecoveryGuarantee::Acceptance].into(),
        }
    }

    fn connected_definition(ids: &[(u128, &str)]) -> DesiredTopology {
        let mut topology = definition(ids);
        for node in &mut topology.components {
            node.descriptor = ComponentDescriptor::try_new(
                node.descriptor.id().clone(),
                [("in", PortDirection::Input), ("out", PortDirection::Output)]
                    .into_iter()
                    .map(|(port, direction)| {
                        PortDescriptor::new(
                            PortId::try_new(port).unwrap(),
                            direction,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )
                    })
                    .collect(),
            )
            .unwrap();
            node.role = ComponentRole::Transformer;
            node.streams.insert(
                PortId::try_new("out").unwrap(),
                StreamId::try_new(format!("{}/out", node.descriptor.id())).unwrap(),
            );
        }
        topology
    }

    fn relationship(from: u128, to: u128) -> DesiredRelationship {
        DesiredRelationship {
            definition: EdgeDefinition::new(
                Endpoint::new(component(from), PortId::try_new("out").unwrap()),
                Endpoint::new(component(to), PortId::try_new("in").unwrap()),
            ),
            policy: RelationshipPolicy::default(),
            pipe: DesiredPipe::Bounded { capacity: 8 },
        }
    }

    #[test]
    fn managed_composition_tracks_edge_ownership_across_binding() {
        let owned = relationship(1, 2);
        let unmanaged = relationship(3, 4);
        let mut previous = connected_definition(&[(1, "producer"), (2, "consumer")]);
        previous.boundary_relationships.push(owned.clone());
        let mut current = connected_definition(&[
            (1, "producer"),
            (2, "consumer"),
            (3, "manual-producer"),
            (4, "manual-consumer"),
        ]);
        current.relationships = vec![owned, unmanaged.clone()];
        let mut desired = previous.clone();
        let repeated = compose_definition(current, &previous, &desired, &BTreeSet::new()).unwrap();
        assert_eq!(repeated.relationships.len(), 2);
        assert!(repeated.boundary_relationships.is_empty());
        assert!(repeated.relationships.contains(&unmanaged));
        assert!(repeated
            .relationships
            .contains(&desired.boundary_relationships[0]));

        desired.boundary_relationships[0].pipe = DesiredPipe::Bounded { capacity: 16 };
        let updated = compose_definition(repeated, &previous, &desired, &BTreeSet::new()).unwrap();
        assert_eq!(updated.relationships.len(), 2);
        assert!(updated.boundary_relationships.is_empty());
        assert!(updated.relationships.contains(&unmanaged));
        assert!(updated
            .relationships
            .contains(&desired.boundary_relationships[0]));

        previous = desired.clone();
        desired.boundary_relationships.clear();
        let removed = compose_definition(updated, &previous, &desired, &BTreeSet::new()).unwrap();
        assert_eq!(removed.relationships, vec![unmanaged]);
        assert!(removed.boundary_relationships.is_empty());
    }

    #[test]
    fn managed_composition_retains_pending_edges_and_unmanaged_optional_boundaries() {
        let owned = relationship(1, 2);
        let mut unmanaged = relationship(3, 4);
        unmanaged.policy.required_for_binding = false;
        unmanaged.policy.orphan_permitted = true;
        let mut current = connected_definition(&[(3, "manual-producer")]);
        current.boundary_relationships.push(unmanaged.clone());
        let previous = definition(&[]);
        let mut desired = connected_definition(&[(1, "producer")]);
        desired.relationships.push(owned.clone());
        let pending = compose_definition(current, &previous, &desired, &BTreeSet::new()).unwrap();
        assert!(pending.relationships.is_empty());
        assert_eq!(
            pending.boundary_relationships,
            vec![unmanaged.clone(), owned.clone()]
        );

        let previous = desired.clone();
        desired
            .components
            .extend(connected_definition(&[(2, "consumer")]).components);
        let bound = compose_definition(pending, &previous, &desired, &BTreeSet::new()).unwrap();
        assert_eq!(bound.relationships, vec![owned]);
        assert_eq!(bound.boundary_relationships, vec![unmanaged]);
    }

    #[test]
    fn managed_composition_preserves_unmanaged_assertions_and_applies_managed_assertions() {
        let mut previous = definition(&[(1, "managed")]);
        previous.recovery_requirements.push(requirement(1));
        let mut current = definition(&[(1, "managed"), (2, "manual")]);
        current.recovery_requirements = vec![requirement(1), requirement(2)];
        let mut desired = previous.clone();
        desired.recovery_requirements[0]
            .guarantees
            .insert(RecoveryGuarantee::Replay);
        let composed = compose_definition(current, &previous, &desired, &BTreeSet::new()).unwrap();
        assert_eq!(composed.recovery_requirements.len(), 2);
        assert!(composed.recovery_requirements.contains(&requirement(2)));
        assert!(composed
            .recovery_requirements
            .contains(&desired.recovery_requirements[0]));
        assert!(!composed.recovery_requirements.contains(&requirement(1)));
    }

    #[tokio::test]
    async fn detached_dependent_resources_leave_no_dangling_recipes_or_dependencies() {
        let previous = definition(&[(1, "managed")]);
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
        current
            .component_resources
            .insert(component(1), BTreeSet::from([child.clone()]));
        current.validate_structure().unwrap();
        let mut target = previous.clone();
        target.components.clear();
        let declared =
            compose_definition(current.clone(), &previous, &target, &BTreeSet::new()).unwrap();
        assert_eq!(declared.resources.len(), 2);
        assert_eq!(
            declared.resource_dependencies,
            current.resource_dependencies
        );
        assert_eq!(
            declared.resource_configurations,
            current.resource_configurations
        );
        let composed = compose_definition(
            current.clone(),
            &previous,
            &target,
            &BTreeSet::from([child.clone()]),
        )
        .unwrap();
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
            let retained = control
                .preview(
                    control.desired_snapshot().revision,
                    vec![DesiredMutation::RemoveComponents {
                        selection: GraphSelection::All,
                        policy: RemovalPolicy::Cascade,
                    }],
                )
                .await
                .unwrap();
            assert_eq!(retained.desired().resources.len(), 2);
            assert_eq!(
                retained.desired().resource_dependencies,
                current.resource_dependencies
            );
            let preview = control
                .preview(
                    control.desired_snapshot().revision,
                    vec![DesiredMutation::RemoveResource {
                        resource: child,
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
