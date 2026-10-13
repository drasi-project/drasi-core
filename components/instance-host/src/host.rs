// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    io::Read,
    os::unix::{
        fs::{FileTypeExt, PermissionsExt},
        net::UnixListener,
    },
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
    thread::JoinHandle,
    time::Duration,
};

use anyhow::{ensure, Context, Result};
use drasi_computation_bridge::{BridgeEgressControl, BridgeIngressControl, IngressCut};
use drasi_host_sdk::{management::HostManagementResources, PluginRegistry};
use drasi_lib::{
    computation::v1::{
        ComponentHealth, ComponentLifecycle, GraphSelection, GraphState, RealizationState,
    },
    management::DesiredInstance,
    DrasiLib,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{sync::mpsc, task::JoinSet};
use uuid::Uuid;

use crate::{
    config::{HostConfig, IndexStorage, StopStage, MAX_MANIFEST_BYTES},
    platform,
    protocol::{
        self, ControlCommand, ControlReply, ControlRequest, ControlResponse, Credentials,
        OperationPhase, OperationStatus,
    },
    state::{self, StateDirectory},
    supervisor::{bounded, now_ms, ChildObservation, ChildSupervisor, Launch, SupervisorFactory},
    InstanceResources, CONTROL_VERSION,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostPhase {
    Booting,
    Starting,
    Running,
    Stopping,
    Stopped,
    Failed,
    CleanupRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentObservation {
    pub id: String,
    pub generation: u64,
    pub operation: u64,
    pub realization: String,
    pub lifecycle: String,
    pub health: String,
    pub started: bool,
    pub exhausted: bool,
    pub failure: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostObservation {
    pub version: u32,
    pub instance_id: String,
    pub generation: Uuid,
    pub pid: u32,
    pub manifest_fingerprint: String,
    pub state_dir: PathBuf,
    pub phase: HostPhase,
    pub ready: bool,
    pub healthy: bool,
    pub graph_state: Option<String>,
    pub graph_revision: Option<u64>,
    pub configuration_revision: Option<u64>,
    pub configuration_persistent: bool,
    pub observed_at_ms: u64,
    pub loaded_plugins: Vec<String>,
    pub components: Vec<ComponentObservation>,
    pub children: Vec<ChildObservation>,
    pub error: Option<String>,
    #[serde(default)]
    pub observation_truncated: bool,
    #[serde(default)]
    pub stop_cuts: BTreeMap<String, IngressCut>,
}

struct State {
    phase: HostPhase,
    error: Option<String>,
    plugins: Vec<String>,
    stop_cuts: BTreeMap<String, IngressCut>,
    prepared_shutdown: bool,
}

struct Operation {
    id: Uuid,
    command: ControlCommand,
}

struct RecordedOperation {
    fingerprint: String,
    status: OperationStatus,
}

struct Shared {
    config: HostConfig,
    credentials: Credentials,
    core: RwLock<Option<Arc<DrasiLib>>>,
    resources: RwLock<Option<Arc<InstanceResources>>>,
    state: Mutex<State>,
    supervisor: ChildSupervisor,
    operations: Mutex<BTreeMap<Uuid, RecordedOperation>>,
    requests: mpsc::Sender<Operation>,
    server_stop: AtomicBool,
    allow_force_exit: bool,
}

impl Shared {
    fn core(&self) -> Result<Arc<DrasiLib>> {
        self.core
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("normal DrasiLib has not initialized; inspect host error")
    }

    fn phase(&self, phase: HostPhase, error: Option<String>) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.phase = phase;
        if error.is_some() {
            state.error = error.map(|value| bounded(&value, self.config.runtime.diagnostic_bytes));
        }
    }

    fn status(&self) -> HostObservation {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let mut observation = HostObservation {
            version: CONTROL_VERSION,
            instance_id: self.config.instance_id.clone(),
            generation: self.credentials.generation,
            pid: std::process::id(),
            manifest_fingerprint: self.credentials.manifest_fingerprint.clone(),
            state_dir: self.config.state_dir.clone(),
            phase: state.phase,
            ready: false,
            healthy: false,
            graph_state: None,
            graph_revision: None,
            configuration_revision: None,
            configuration_persistent: false,
            observed_at_ms: now_ms(),
            loaded_plugins: state.plugins.clone(),
            components: Vec::new(),
            children: self.supervisor.observations(),
            error: state.error.clone(),
            observation_truncated: false,
            stop_cuts: state.stop_cuts.clone(),
        };
        drop(state);
        let Ok(core) = self.core() else {
            return observation;
        };
        observation.configuration_persistent = core.configuration_is_persistent();
        let required = match core.desired_configuration() {
            Ok(configuration) => {
                observation.configuration_revision = Some(configuration.revision);
                Some(
                    configuration
                        .desired
                        .topology
                        .components
                        .into_iter()
                        .map(|component| {
                            (
                                component.descriptor.id().clone(),
                                component.lifecycle.auto_start,
                            )
                        })
                        .collect::<BTreeMap<_, _>>(),
                )
            }
            Err(error) => {
                observation.error = Some(bounded(
                    &error.to_string(),
                    self.config.runtime.diagnostic_bytes,
                ));
                None
            }
        };
        let Ok(control) = core.computation_control() else {
            return observation;
        };
        let actual = control.observed();
        let desired = control.desired_snapshot();
        observation.graph_state = Some(format!("{:?}", control.state()));
        observation.graph_revision = Some(actual.revision.0);
        let mut healthy = !matches!(
            control.state(),
            GraphState::Failed | GraphState::CleanupRequired | GraphState::Cancelled
        );
        let mut ready = actual.components.len() == desired.nodes.len()
            && required.as_ref().is_some_and(|required| {
                required.keys().all(|id| actual.components.contains_key(id))
            });
        for (id, component) in &actual.components {
            let failure = component.failure.as_ref().map(|failure| {
                bounded(
                    &format!("{:?}: {}", failure.phase, failure.cause),
                    self.config.runtime.diagnostic_bytes,
                )
            });
            let requested = required
                .as_ref()
                .and_then(|required| required.get(id))
                .copied()
                .unwrap_or(false);
            if failure.is_some()
                || matches!(
                    component.health,
                    ComponentHealth::Degraded | ComponentHealth::Unavailable
                )
            {
                healthy = false;
            }
            if requested
                && !(component.realization == RealizationState::Created
                    && component.started
                    && !matches!(
                        component.health,
                        ComponentHealth::Degraded | ComponentHealth::Unavailable
                    )
                    && matches!(
                        component.lifecycle,
                        ComponentLifecycle::Running | ComponentLifecycle::Quiesced
                    ))
            {
                ready = false;
            }
            observation.components.push(ComponentObservation {
                id: id.to_string(),
                generation: component.generation.0,
                operation: component.operation.0,
                realization: format!("{:?}", component.realization),
                lifecycle: format!("{:?}", component.lifecycle),
                health: format!("{:?}", component.health),
                started: component.started,
                exhausted: component.exhausted,
                failure,
            });
        }
        for resource in actual.resources.values() {
            healthy &= resource.failure.is_none();
            ready &=
                resource.realization == drasi_lib::computation::v1::ResourceRealization::Created;
        }
        healthy &= actual
            .relationships
            .values()
            .all(|relationship| relationship.failure.is_none());
        observation.healthy =
            healthy && matches!(observation.phase, HostPhase::Running | HostPhase::Stopped);
        observation.ready = ready && observation.healthy && observation.phase == HostPhase::Running;
        if observation.phase == HostPhase::Running && !healthy {
            observation.phase = HostPhase::Failed;
            if observation.error.is_none() {
                observation.error = observation
                    .components
                    .iter()
                    .find_map(|component| component.failure.clone())
                    .or_else(|| {
                        Some("DrasiLib reports unhealthy component or resource ownership".into())
                    });
            }
        }
        Self::bound_observation(
            &mut observation,
            self.config.runtime.max_control_frame_bytes / 2,
        );
        observation
    }

    fn record(&self, id: Uuid, command: &ControlCommand) -> Result<bool> {
        let fingerprint = crate::config::digest(&serde_json::to_vec(command)?);
        let mut operations = self.operations.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(record) = operations.get(&id) {
            ensure!(
                record.fingerprint == fingerprint,
                "control request ID was reused with different content"
            );
            return Ok(false);
        }
        ensure!(
            operations.len() < self.config.runtime.max_control_operations,
            "control operation ledger is full; entries are not silently forgotten or replayed"
        );
        operations.insert(
            id,
            RecordedOperation {
                fingerprint,
                status: OperationStatus {
                    id,
                    phase: OperationPhase::Accepted,
                    result: None,
                    error: None,
                },
            },
        );
        Ok(true)
    }

    fn operation(
        &self,
        id: Uuid,
        phase: OperationPhase,
        result: Option<serde_json::Value>,
        error: Option<String>,
    ) {
        if let Some(operation) = self
            .operations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&id)
        {
            operation.status.phase = phase;
            operation.status.result = result.map(|value| {
                if serde_json::to_vec(&value).is_ok_and(|bytes| bytes.len() <= self.config.runtime.max_control_frame_bytes / 2) {
                    value
                } else {
                    serde_json::json!({"result_unavailable":"operation result exceeds the bounded control response budget"})
                }
            });
            operation.status.error =
                error.map(|value| bounded(&value, self.config.runtime.diagnostic_bytes));
        }
    }

    fn bound_observation(observation: &mut HostObservation, budget: usize) {
        let oversized = |value: &HostObservation| {
            serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > budget)
        };
        if !oversized(observation) {
            return;
        }
        observation.observation_truncated = true;
        for child in &mut observation.children {
            if let Some(previous) = &mut child.last_known {
                previous.components.clear();
                previous.children.clear();
                previous.observation_truncated = true;
            }
        }
        while oversized(observation) && !observation.components.is_empty() {
            observation
                .components
                .truncate(observation.components.len() / 2);
        }
        if oversized(observation) {
            for child in &mut observation.children {
                child.last_known = None;
            }
        }
    }

    async fn dispatch(self: &Arc<Self>, request: &ControlRequest) -> Result<ControlReply> {
        self.credentials.authorize(request)?;
        match &request.command {
            ControlCommand::Hello => Ok(ControlReply::Hello {
                runtime: "drasi-lib".into(),
                package_version: env!("CARGO_PKG_VERSION").into(),
                capabilities: vec![
                    "native-management-v1".into(),
                    "owned-local-children-v1".into(),
                    "ordered-composition-stop-v1".into(),
                ],
            }),
            ControlCommand::Status => Ok(ControlReply::Status {
                observation: Box::new(self.status()),
            }),
            ControlCommand::Configuration => Ok(ControlReply::Configuration {
                configuration: self.core()?.desired_configuration()?,
            }),
            ControlCommand::Operation { operation_id } => {
                let operation = self
                    .operations
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(operation_id)
                    .context("unknown control operation; no completion can be inferred")?
                    .status
                    .clone();
                Ok(ControlReply::Operation { operation })
            }
            ControlCommand::ForceExit => {
                ensure!(
                    self.allow_force_exit,
                    "force-exit is available only to the owning generic executable"
                );
                platform::kill_instance()
            }
            ControlCommand::ForceChild {
                component,
                generation,
            } => {
                if self.record(request.request_id, &request.command)? {
                    self.operation(request.request_id, OperationPhase::Running, None, None);
                    // This runs on the independent control executor, not a
                    // potentially blocked graph operation executor.
                    let result = self.supervisor.force(component, *generation).await;
                    match result {
                        Ok(observation) => self.operation(
                            request.request_id,
                            OperationPhase::Completed,
                            Some(serde_json::to_value(observation)?),
                            None,
                        ),
                        Err(error) => self.operation(
                            request.request_id,
                            OperationPhase::Incomplete,
                            None,
                            Some(format!("{error:#}")),
                        ),
                    }
                }
                Ok(ControlReply::Accepted {
                    operation_id: request.request_id,
                })
            }
            command => {
                if self.record(request.request_id, command)?
                    && self
                        .requests
                        .try_send(Operation {
                            id: request.request_id,
                            command: command.clone(),
                        })
                        .is_err()
                {
                    self.operations
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .remove(&request.request_id);
                    anyhow::bail!(
                        "host operation queue is full or closed; request was not accepted"
                    );
                }
                Ok(ControlReply::Accepted {
                    operation_id: request.request_id,
                })
            }
        }
    }
}

struct ControlThread {
    shared: Arc<Shared>,
    join: Option<JoinHandle<Result<()>>>,
}

impl ControlThread {
    fn start(shared: Arc<Shared>, listener: UnixListener) -> Result<Self> {
        listener.set_nonblocking(true)?;
        let server = shared.clone();
        let join = std::thread::Builder::new()
            .name("drasi-instance-control".into())
            .spawn(move || {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?
                    .block_on(serve(server, listener))
            })?;
        Ok(Self {
            shared,
            join: Some(join),
        })
    }

    fn finish(&mut self) -> Result<()> {
        self.shared.server_stop.store(true, Ordering::Release);
        if let Some(thread) = self.join.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("control thread panicked"))??;
        }
        Ok(())
    }
}

impl Drop for ControlThread {
    fn drop(&mut self) {
        self.shared.server_stop.store(true, Ordering::Release);
        self.shared.supervisor.request_emergency_cleanup();
        if let Some(thread) = self.join.take() {
            let _ = thread.join();
        }
    }
}

async fn serve(shared: Arc<Shared>, listener: UnixListener) -> Result<()> {
    let listener = tokio::net::UnixListener::from_std(listener)?;
    let mut connections = JoinSet::new();
    loop {
        if shared.server_stop.load(Ordering::Acquire) {
            break;
        }
        tokio::select! {
            accepted = listener.accept(), if connections.len() < shared.config.runtime.max_control_connections => {
                let (mut socket, _) = accepted?;
                let shared = shared.clone();
                connections.spawn(async move {
                    let timeout = Duration::from_millis(shared.config.runtime.peer_timeout_ms);
                    let request: ControlRequest = tokio::time::timeout(timeout, protocol::read_async(&mut socket, shared.config.runtime.max_control_frame_bytes)).await??;
                    let result = shared.dispatch(&request).await;
                    let response = ControlResponse {
                        version: CONTROL_VERSION, instance_id: shared.config.instance_id.clone(),
                        generation: shared.credentials.generation, request_id: request.request_id,
                        reply: result.as_ref().ok().cloned(),
                        error: result.err().map(|error| bounded(&format!("{error:#}"), shared.config.runtime.diagnostic_bytes)),
                    };
                    tokio::time::timeout(timeout, protocol::write_async(&mut socket, &response, shared.config.runtime.max_control_frame_bytes)).await??;
                    Ok::<_, anyhow::Error>(())
                });
            }
            _ = connections.join_next(), if !connections.is_empty() => {}
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
        }
    }
    connections.abort_all();
    while connections.join_next().await.is_some() {}
    Ok(())
}

/// Host a normal DrasiLib on the caller's runtime. The caller must honor the
/// manifest's executor budget. Drop requests emergency owned-child cleanup;
/// only a successful stop operation proves graceful completion.
pub async fn run(config: HostConfig, credentials: Credentials, executable: PathBuf) -> Result<()> {
    run_inner(config, credentials, executable, false).await
}

async fn run_inner(
    config: HostConfig,
    credentials: Credentials,
    executable: PathBuf,
    allow_force_exit: bool,
) -> Result<()> {
    config.validate()?;
    credentials.validate(&config)?;
    let directory = Arc::new(StateDirectory::acquire(
        &config.state_dir,
        &config.instance_id,
    )?);
    if credentials.endpoint.exists() {
        ensure!(
            std::fs::symlink_metadata(&credentials.endpoint)?
                .file_type()
                .is_socket(),
            "control path is not a stale Unix socket"
        );
        std::fs::remove_file(&credentials.endpoint)?;
    }
    let listener = UnixListener::bind(&credentials.endpoint)?;
    std::fs::set_permissions(
        &credentials.endpoint,
        std::fs::Permissions::from_mode(0o600),
    )?;
    state::atomic_json(&directory.path().join("control.json"), &credentials)?;
    let supervisor = ChildSupervisor::new(
        executable,
        config.state_dir.clone(),
        config.instance_id.clone(),
        config.runtime.clone(),
    )?;
    supervisor.retain_parent_lease(directory.clone());
    let (send, mut receive) = mpsc::channel(32);
    let shared = Arc::new(Shared {
        config,
        credentials,
        core: RwLock::new(None),
        resources: RwLock::new(None),
        state: Mutex::new(State {
            phase: HostPhase::Booting,
            error: None,
            plugins: Vec::new(),
            stop_cuts: BTreeMap::new(),
            prepared_shutdown: false,
        }),
        supervisor,
        operations: Mutex::new(BTreeMap::new()),
        requests: send,
        server_stop: AtomicBool::new(false),
        allow_force_exit,
    });
    let mut control = ControlThread::start(shared.clone(), listener)?;
    let initialization = initialize(&shared);
    tokio::pin!(initialization);
    let initialized = match tokio::time::timeout(
        Duration::from_millis(shared.config.runtime.startup_timeout_ms),
        &mut initialization,
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            shared.phase(HostPhase::CleanupRequired, Some("initialization deadline elapsed; initialization future and process ownership remain retained".into()));
            initialization.await
        }
    };
    if let Err(error) = initialized {
        shared.phase(
            HostPhase::Failed,
            Some(format!("initialize normal DrasiLib: {error:#}")),
        );
    }
    state::atomic_json(
        &directory.path().join("last-observation.json"),
        &shared.status(),
    )?;
    while let Some(operation) = receive.recv().await {
        shared.operation(operation.id, OperationPhase::Running, None, None);
        let stopping = matches!(operation.command, ControlCommand::StopTree);
        let execution = execute(&shared, operation.command);
        tokio::pin!(execution);
        let result = match tokio::time::timeout(
            Duration::from_millis(shared.config.runtime.operation_timeout_ms),
            &mut execution,
        )
        .await
        {
            Ok(result) => result,
            Err(_) => {
                shared.operation(
                    operation.id,
                    OperationPhase::Incomplete,
                    None,
                    Some(
                        "deadline elapsed; the accepted operation and its cleanup remain owned"
                            .into(),
                    ),
                );
                shared.phase(HostPhase::CleanupRequired, Some("operation remains in progress after its deadline; no channels were forcibly closed".into()));
                execution.await
            }
        };
        let completed = result.is_ok();
        match result {
            Ok(value) => {
                shared.operation(operation.id, OperationPhase::Completed, Some(value), None)
            }
            Err(error) => {
                shared.operation(
                    operation.id,
                    OperationPhase::Failed,
                    None,
                    Some(format!("{error:#}")),
                );
                if stopping {
                    shared.phase(
                        HostPhase::CleanupRequired,
                        Some(format!("composition stop incomplete: {error:#}")),
                    );
                }
            }
        }
        state::atomic_json(
            &directory.path().join("last-observation.json"),
            &shared.status(),
        )?;
        if stopping && completed {
            shared.supervisor.close().await?;
            // An accepted operation is independently queryable before endpoint
            // retirement. The owner also verifies the actual successful exit.
            tokio::time::sleep(Duration::from_millis(100)).await;
            control.finish()?;
            std::fs::remove_file(&shared.credentials.endpoint)?;
            return Ok(());
        }
    }
    anyhow::bail!("host operation owner exited without successful shutdown")
}

async fn initialize(shared: &Arc<Shared>) -> Result<()> {
    let mut plugins = PluginRegistry::new();
    for plugin in &shared.config.plugins {
        if let Some(expected) = &plugin.sha256 {
            let mut file = std::fs::File::open(&plugin.path)?;
            let mut hash = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
            }
            ensure!(
                format!("{:x}", hash.finalize()).eq_ignore_ascii_case(expected),
                "allowlisted plugin content changed"
            );
        }
        let loaded = drasi_host_sdk::computation::load(&plugin.path)?;
        shared
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .plugins
            .push(loaded.metadata().plugin.id.to_string());
        plugins.register_computation_plugin(loaded)?;
    }
    let mut factories = plugins.computation_factory_registry()?;
    factories.register(Arc::new(SupervisorFactory::new(shared.supervisor.clone())))?;
    for factory in drasi_computation_bridge::factories() {
        factories.register(factory)?;
    }
    let mut middleware = drasi_core::middleware::MiddlewareTypeRegistry::new();
    middleware.register(Arc::new(drasi_middleware::map::MapFactory::new()));
    middleware.register(Arc::new(
        drasi_middleware::relabel::RelabelMiddlewareFactory::new(),
    ));
    let mut resources = HostManagementResources::new(&plugins, Arc::new(middleware), None)?;
    if !shared.config.indexes.is_empty() {
        let budget = drasi_index_rocksdb::RocksDbMemoryBudget::from_total_budget_bytes(
            shared.config.runtime.index_memory_bytes,
        )?;
        for (name, provider) in &shared.config.indexes {
            match provider {
                IndexStorage::Rocksdb => {
                    resources = resources.with_index_provider(
                        name,
                        Arc::new(
                            drasi_index_rocksdb::computation::RocksDbComputationProvider::new(
                                shared.config.state_dir.join("indexes").join(name),
                                drasi_index_rocksdb::RocksIndexOptions::new(
                                    false,
                                    false,
                                    budget.clone(),
                                ),
                            ),
                        ),
                    )?;
                }
            }
        }
    }
    let resources = Arc::new(InstanceResources::new(
        Arc::new(resources),
        shared.config.state_dir.clone(),
    ));
    *shared.resources.write().unwrap_or_else(|e| e.into_inner()) = Some(resources.clone());
    let mut builder = DrasiLib::builder()
        .with_id(&shared.config.instance_id)
        .with_component_factories(factories)
        .with_management_resources(resources);
    if let Some(storage) = &shared.config.configuration_store {
        let store = Arc::new(drasi_state_store_redb::RedbConfigurationStore::new(
            shared.config.state_dir.join("configuration.redb"),
            state::read_key(&storage.key_file)?,
        )?);
        store
            .initialize_if_absent_with(&shared.config.instance_id, "host-initial-manifest", || {
                Ok(shared.config.desired.clone())
            })
            .await?;
        builder = builder.with_configuration_store(store);
    }
    let core = Arc::new(builder.build().await?);
    *shared.core.write().unwrap_or_else(|e| e.into_inner()) = Some(core.clone());
    if shared.config.configuration_store.is_none() {
        core.apply_desired_state(0, "host-initial-manifest", shared.config.desired.clone())
            .await?;
    }
    core.reconcile_desired_state().await?;
    shared.phase(HostPhase::Starting, None);
    let mut observed = core.computation_control()?.subscribe_observed();
    core.start().await?;
    shared.phase(HostPhase::Running, None);
    loop {
        let status = shared.status();
        if status.ready {
            return Ok(());
        }
        ensure!(
            status.phase != HostPhase::Failed,
            "DrasiLib did not reach healthy native readiness: {}",
            status
                .error
                .as_deref()
                .unwrap_or("component readiness failed")
        );
        observed
            .changed()
            .await
            .context("DrasiLib readiness observer closed")?;
    }
}

async fn execute(shared: &Arc<Shared>, command: ControlCommand) -> Result<serde_json::Value> {
    if matches!(command, ControlCommand::StopTree) {
        return stop_tree(shared)
            .await
            .map(|_| serde_json::json!({"stopped":true}));
    }
    ensure!(!shared.state.lock().unwrap_or_else(|e| e.into_inner()).prepared_shutdown,
        "composition shutdown owns this instance; complete its cleanup before issuing new mutations");
    let core = shared.core()?;
    match command {
        ControlCommand::Apply {
            expected_revision,
            management_request_id,
            desired,
        } => {
            let mut candidate = shared.config.clone();
            candidate.desired = *desired.clone();
            candidate.validate()?;
            Ok(serde_json::to_value(
                core.apply_desired_state(expected_revision, management_request_id, *desired)
                    .await?,
            )?)
        }
        ControlCommand::Reconcile => {
            Ok(serde_json::to_value(core.reconcile_desired_state().await?)?)
        }
        ControlCommand::Start => {
            core.start().await?;
            shared.phase(HostPhase::Running, None);
            Ok(serde_json::json!({"started":true}))
        }
        ControlCommand::StartComponents { components } => {
            ensure!(components.len() <= 1024, "start selection exceeds budget");
            let control = core.computation_control()?;
            let report = control
                .start_requested(
                    control.desired_snapshot().revision,
                    GraphSelection::Exact(components),
                )
                .await?;
            Ok(serde_json::json!({
                "summary": format!("{:?}", report.summary),
                "components": report.components.iter().map(|(id, result)| (id.to_string(), format!("{result:?}"))).collect::<BTreeMap<_,_>>()
            }))
        }
        ControlCommand::Quiesce { components } => {
            ensure!(components.len() <= 1024, "quiesce selection exceeds budget");
            let control = core.computation_control()?;
            let stopped = control
                .quiesce_components(
                    control.desired_snapshot().revision,
                    GraphSelection::Exact(components),
                )
                .await?;
            Ok(serde_json::to_value(stopped)?)
        }
        ControlCommand::StopChild {
            component,
            generation,
        } => Ok(serde_json::to_value(
            shared.supervisor.stop(&component, generation).await?,
        )?),
        ControlCommand::RepairChild {
            component,
            generation,
        } => {
            let children = shared.supervisor.observations();
            let current = children
                .iter()
                .find(|child| child.component == component)
                .context("child is not locally owned")?;
            ensure!(
                current.generation == Some(generation)
                    && (current.reaped || current.ownership_lost),
                "repair requires this exact reaped process generation"
            );
            if current.ownership_lost {
                shared
                    .supervisor
                    .confirm_previous_exit(&component, generation)
                    .await?;
            }
            core.computation_component(&component)?.stop().await?;
            let observation = shared.supervisor.repair(&component, generation).await?;
            core.computation_component(&component)?.start().await?;
            shared.phase(HostPhase::Running, None);
            Ok(serde_json::to_value(observation)?)
        }
        _ => anyhow::bail!("command is not an ordinary host operation"),
    }
}

async fn stop_tree(shared: &Arc<Shared>) -> Result<()> {
    let core = shared.core().ok();
    if shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .prepared_shutdown
    {
        if let Some(core) = core {
            core.shutdown().await?;
        }
        shared.phase(HostPhase::Stopped, None);
        return Ok(());
    }
    let mut ingress: BTreeMap<String, Arc<BridgeIngressControl>> = BTreeMap::new();
    let mut egress: BTreeMap<String, Arc<BridgeEgressControl>> = BTreeMap::new();
    let mut revision = None;
    if let Some(core) = &core {
        let desired: DesiredInstance = core.desired_configuration()?.desired;
        shared.config.validate_stop_plan(&desired)?;
        let resources = shared
            .resources
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .context("native resource resolver is unavailable")?;
        revision = Some(core.computation_control()?.desired_snapshot().revision);
        for component in &desired.topology.components {
            let drasi_lib::computation::v1::ComponentConstruction::Factory(spec) =
                &component.construction
            else {
                continue;
            };
            let implementation = spec.implementation.name.as_ref();
            if !matches!(implementation, "drasi.bridge/source" | "drasi.bridge/sink") {
                continue;
            }
            let controls = spec.dependencies.get("control")
                .context("bridge composition requires a declared native control resource; endpoints remain open")?;
            ensure!(
                controls.len() == 1,
                "bridge requires exactly one native control resource"
            );
            let id = component.descriptor.id().to_string();
            if implementation == "drasi.bridge/source" {
                ingress.insert(id, resources.ingress(&controls[0])?);
            } else {
                egress.insert(id, resources.egress(&controls[0])?);
            }
        }
        if !ingress.is_empty() || !egress.is_empty() {
            let management = core.management_status().await?;
            ensure!(
                !management.reconciling && management.applied,
                "finish current configuration reconciliation before composition shutdown"
            );
        }
    }
    shared.phase(HostPhase::Stopping, None);
    let timeout = Duration::from_millis(shared.config.runtime.operation_timeout_ms);
    let mut inlet_cuts = BTreeMap::new();
    for stage in &shared.config.stop_stages {
        match stage {
            StopStage::Quiesce { components } => {
                let control = core
                    .as_ref()
                    .context("DrasiLib is unavailable for the configured stop cut")?
                    .computation_control()?;
                for id in components {
                    if let Some(input) = ingress.get(id.as_str()) {
                        let cut = input.sealed_cut().await?
                            .context("bridge ingress has no peer-sealed cut; its sender must finish first, and endpoints remain open")?;
                        let drained = input.wait_drained(&cut, timeout).await?;
                        ensure!(
                            drained.drained,
                            "native ingress has not drained its sealed cut"
                        );
                        inlet_cuts.insert(id.to_string(), cut.clone());
                        shared
                            .state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .stop_cuts
                            .insert(format!("ingress:{id}"), cut);
                    }
                }
                control
                    .quiesce_components(
                        revision.context("graph revision is unavailable")?,
                        GraphSelection::Exact(components.clone()),
                    )
                    .await?;
                let observed = control.observed();
                for id in components {
                    let component = observed
                        .components
                        .get(id)
                        .context("quiesced component is unavailable")?;
                    ensure!(
                        component.failure.is_none(),
                        "component {id} failed before its drain could be confirmed"
                    );
                    if let Some(output) = egress.get(id.as_str()) {
                        let cut = output.finish(timeout).await?;
                        shared
                            .state
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .stop_cuts
                            .insert(format!("egress:{id}"), cut);
                    }
                }
            }
            StopStage::StopChild { component } => {
                let children = shared.supervisor.observations();
                let child = children
                    .iter()
                    .find(|child| child.component == component.as_str())
                    .context("stop cut child was not realized")?;
                if let Some(generation) = child.generation {
                    let stopped = shared.supervisor.stop(&child.component, generation).await?;
                    if !ingress.is_empty() || !egress.is_empty() {
                        ensure!(
                            stopped.reaped
                                && stopped
                                    .last_known
                                    .as_ref()
                                    .is_some_and(|host| host.phase == HostPhase::Stopped),
                            "child termination is not evidence of completed normal DrasiLib drain"
                        );
                    }
                }
            }
        }
    }
    for (id, input) in &ingress {
        let cut = inlet_cuts
            .get(id)
            .context("bridge ingress has no validated drain stage")?;
        ensure!(
            input.drain_status(cut).await?.drained,
            "bridge ingress cut changed before shutdown"
        );
    }
    for output in egress.values() {
        let status = output.snapshot()?;
        ensure!(
            status.sealed && !status.pending && !status.in_flight,
            "bridge egress still owns an unresolved input"
        );
    }
    shared.supervisor.stop_all().await?;
    if let Some(core) = &core {
        if !ingress.is_empty() || !egress.is_empty() {
            let control = core.computation_control()?;
            control
                .quiesce_components(
                    revision.context("graph revision is unavailable")?,
                    GraphSelection::All,
                )
                .await?;
            ensure!(
                control
                    .observed()
                    .components
                    .values()
                    .all(|component| component.failure.is_none()),
                "graph failures prevent a positive composition drain"
            );
        }
    }
    shared
        .state
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .prepared_shutdown = true;
    if let Some(core) = core {
        core.shutdown().await?;
    }
    shared.phase(HostPhase::Stopped, None);
    Ok(())
}

pub(crate) fn main_entry() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mut config_path = None;
    let mut owned = false;
    while let Some(argument) = args.next() {
        if argument == "--config" {
            config_path = Some(PathBuf::from(
                args.next().context("--config requires a manifest path")?,
            ));
        } else if argument == "--owned" {
            ensure!(!owned, "duplicate --owned");
            owned = true;
        } else {
            anyhow::bail!("usage: drasi-instance-host --config MANIFEST [--owned]");
        }
    }
    let config: HostConfig = state::read_json(
        &config_path.context("--config is required")?,
        MAX_MANIFEST_BYTES,
    )?;
    config.validate()?;
    platform::isolate_process_group()?;
    let credentials = if owned {
        let launch: Launch = protocol::read_frame(&mut std::io::stdin().lock(), 8192)?;
        launch.credentials.validate(&config)?;
        crate::config::validate_name(&launch.owner_instance)?;
        // No Tokio task or graph heartbeat participates in parent-death
        // containment. Only this read endpoint is inherited by the host.
        std::thread::Builder::new()
            .name("drasi-owner-watchdog".into())
            .spawn(|| {
                let mut input = std::io::stdin().lock();
                let mut unexpected = [0_u8; 1];
                loop {
                    match input.read(&mut unexpected) {
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                        _ => platform::kill_instance(),
                    }
                }
            })?;
        launch.credentials
    } else {
        Credentials::new(&config)?
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.runtime.worker_threads)
        .max_blocking_threads(config.runtime.blocking_threads)
        .enable_all()
        .build()?;
    let executable = std::env::current_exe()?;
    let result = runtime.block_on(run_inner(config, credentials, executable, true));
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}
