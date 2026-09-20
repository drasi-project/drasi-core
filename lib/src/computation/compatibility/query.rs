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

use super::{component::RuntimeComponent, source::SourceInstance};
use crate::{
    channels::{ChangeReceiver, QueryResult, QuerySubscriptionResponse},
    computation::{scoped_graph::ScopedGraph, v1::*},
    config::QueryConfig,
    metrics::QueryOutputMetrics,
    queries::{FetchError, OutboxResponse, Query, SnapshotResponse},
    ComponentStatus,
};
use async_trait::async_trait;
use futures::future::BoxFuture;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock, Weak,
    },
    time::Duration,
};
use tokio::sync::{watch, Mutex};

struct QueryLife {
    scope: ScopedGraph,
    creation_attempted: bool,
    activation: Option<BoxFuture<'static, GraphResult<StartReport>>>,
    link: Option<CatalogLink>,
}

impl QueryLife {
    async fn ready(&mut self) -> anyhow::Result<GraphControl> {
        let retry = self.creation_attempted;
        self.creation_attempted = true;
        match self.scope.ready().await {
            Ok(control) => Ok(control),
            Err(error) if retry => {
                let control = self.scope.control()?;
                let failed = control
                    .observed()
                    .components
                    .iter()
                    .filter(|(_, node)| node.realization == RealizationState::CreationFailed)
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>();
                if failed.is_empty() {
                    return Err(error);
                }
                let request = control.clone();
                let report = self
                    .scope
                    .drive(async move {
                        let preview = request
                            .preview(
                                request.desired_snapshot().revision,
                                vec![DesiredMutation::Retry(GraphSelection::Exact(failed))],
                            )
                            .await?;
                        Ok(request
                            .reconcile(preview, TopologyBindings::default())
                            .await?)
                    })
                    .await?;
                if report.summary != OperationSummary::Completed {
                    anyhow::bail!("nested query creation retry failed: {report:?}");
                }
                Ok(control)
            }
            Err(error) => Err(error),
        }
    }
}

struct ParentBinding {
    control: GraphControl,
    node: ComponentId,
    token: u64,
    initial_status: ComponentStatus,
}

struct QueryExecution {
    catalog: QueryResultsCatalog,
    source_bindings: Vec<Arc<SourceInstance>>,
    subscriptions: Vec<Arc<LegacySourceSubscription>>,
    inspector: ComputationInspector,
    life: Mutex<QueryLife>,
}

pub(crate) struct QueryInstance {
    pub config: QueryConfig,
    pub metrics: Arc<QueryOutputMetrics>,
    execution: OnceLock<QueryExecution>,
    initializing: Mutex<()>,
    parent: OnceLock<ParentBinding>,
    global_catalog: QueryResultsCatalog,
    started: AtomicBool,
    requested: AtomicBool,
    closed: AtomicBool,
    changed: watch::Sender<u64>,
    owner: Weak<super::Runtime>,
    status: watch::Sender<ComponentStatus>,
    cleanup: drasi_core::computation::ComputationIoScope,
    deleted: Arc<AtomicBool>,
}

impl QueryInstance {
    pub(super) fn new(config: QueryConfig, owner: &Arc<super::Runtime>) -> Arc<Self> {
        Arc::new(Self {
            config,
            metrics: Arc::new(QueryOutputMetrics::new()),
            execution: OnceLock::new(),
            initializing: Mutex::new(()),
            parent: OnceLock::new(),
            global_catalog: owner.catalog.clone(),
            started: AtomicBool::new(false),
            requested: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            changed: owner.changed.clone(),
            owner: Arc::downgrade(owner),
            status: watch::channel(ComponentStatus::Added).0,
            cleanup: drasi_core::computation::ComputationIoScope::default(),
            deleted: Arc::new(AtomicBool::new(false)),
        })
    }

    fn construct(
        &self,
        sources: Vec<Arc<SourceInstance>>,
        owner: &Arc<super::Runtime>,
    ) -> anyhow::Result<QueryExecution> {
        let config = &self.config;
        if let Some(crate::indexes::StorageBackendRef::Inline(spec)) = &config.storage_backend {
            spec.validate().map_err(|error| {
                anyhow::anyhow!(
                    "Query '{}' has invalid inline storage backend configuration: {error}",
                    config.id
                )
            })?;
        }
        crate::queries::LabelExtractor::extract_labels(&config.query, &config.query_language)?;
        let runtime = &owner.config;
        let source_bindings = sources.clone();
        let services = owner.services.clone();
        let graph_id = format!(
            "lib-query/{}",
            config
                .id
                .bytes()
                .map(|value| format!("{value:02x}"))
                .collect::<String>()
        );
        let pipeline = ComputationPipelineBuilder::new(
            &graph_id,
            services.clone(),
            owner.middleware.clone(),
            runtime.index_factory.clone(),
            runtime.default_recovery_policy.unwrap_or_default(),
            runtime.global_priority_queue_capacity.unwrap_or(10_000),
            runtime.global_dispatch_buffer_capacity.unwrap_or(1_000),
        )?
        .for_runtime();
        let catalog = pipeline.catalog();
        catalog.configure_delivery(
            &config.id,
            config.dispatch_mode.unwrap_or_default(),
            config
                .dispatch_buffer_capacity
                .or(runtime.global_dispatch_buffer_capacity)
                .unwrap_or(1_000),
            crate::queries::compute_config_hash(config),
            self.metrics.clone(),
            self.status.subscribe(),
        )?;
        let mut pipeline = pipeline;
        let (backend, _) = runtime
            .index_factory
            .computation_backend(config.storage_backend.as_ref())?;
        if matches!(
            backend,
            crate::indexes::StorageBackendRef::Inline(crate::indexes::StorageBackendSpec::Memory {
                enable_archive: false
            })
        ) {
            pipeline = pipeline.query_provider(
                &config.id,
                Arc::new(drasi_core::computation::InMemoryComputationProvider),
                QueryPublicationMode::NonAtomic,
            )?;
        }
        for source in sources {
            pipeline = pipeline.source(
                source.borrowed.clone(),
                SourceSubscriptionOptions {
                    borrowed_recovery: true,
                    allow_broadcast_loss: true,
                    ..Default::default()
                },
            )?;
        }
        let mut graph_config = config.clone();
        // Subscriber capacity is enforced by the catalogue. The internal relay
        // retains committed notifications within the query's outbox bound.
        graph_config.dispatch_buffer_capacity = Some(config.outbox_capacity.clamp(1, 1_000_000));
        let (graph, subscriptions) = pipeline.query(graph_config).build_with_subscriptions()?;
        let inspector = graph.inspector();
        let scope = ScopedGraph::new(graph, services.scope);
        Ok(QueryExecution {
            catalog,
            source_bindings,
            subscriptions,
            inspector,
            life: Mutex::new(QueryLife {
                scope,
                creation_attempted: false,
                activation: None,
                link: None,
            }),
        })
    }

    fn execution(&self) -> anyhow::Result<&QueryExecution> {
        self.execution
            .get()
            .ok_or_else(|| anyhow::anyhow!("Query '{}' has not been created", self.config.id))
    }

    pub(super) fn subscribe_results(&self) -> anyhow::Result<CatalogSubscription> {
        self.require_current()?;
        self.execution()?.catalog.subscribe_query(&self.config.id)
    }
    pub(super) fn notify(&self) {
        self.publish_status(self.read_status());
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }
    pub(super) fn publish_status(&self, status: ComponentStatus) {
        self.status.send_if_modified(|current| {
            if *current == status {
                false
            } else {
                *current = status;
                true
            }
        });
    }
    pub(super) fn bind_parent(
        &self,
        control: GraphControl,
        node: ComponentId,
        token: u64,
        initial_status: ComponentStatus,
    ) -> anyhow::Result<()> {
        self.parent
            .set(ParentBinding {
                control,
                node,
                token,
                initial_status,
            })
            .map_err(|_| anyhow::anyhow!("query already has a construction binding"))
    }
    pub(super) fn has_stale_sources(&self, sources: &[Arc<SourceInstance>]) -> bool {
        self.execution.get().is_some_and(|execution| {
            execution.source_bindings.len() != sources.len()
                || !execution
                    .source_bindings
                    .iter()
                    .zip(sources)
                    .all(|(left, right)| Arc::ptr_eq(left, right))
        })
    }
    fn require_current(&self) -> anyhow::Result<()> {
        let binding = self
            .parent
            .get()
            .ok_or_else(|| anyhow::anyhow!("query has no controller binding"))?;
        if super::record_token(&binding.control.desired_snapshot(), &binding.node)
            != Some(binding.token)
        {
            return Err(GraphError::StaleGeneration.into());
        }
        Ok(())
    }
    pub(crate) fn inspector(&self) -> ComputationInspector {
        match self.execution.get() {
            Some(execution) => execution.inspector.clone(),
            // An unrealized query is inspected through its declared parent node,
            // including its raw definition and any construction failure.
            None => self
                .parent
                .get()
                .expect("registered query has a parent binding")
                .control
                .inspector(),
        }
    }
    pub(super) fn execution_inspection(
        &self,
    ) -> Option<(Arc<ComputationInspection>, &[Arc<SourceInstance>])> {
        let execution = self.execution.get()?;
        Some((execution.inspector.snapshot(), &execution.source_bindings))
    }
    pub(crate) fn checkpoint_store(
        &self,
    ) -> anyhow::Result<Option<Arc<dyn drasi_core::interface::CheckpointStore>>> {
        self.execution()?.catalog.checkpoint_store(&self.config.id)
    }
    fn read_status(&self) -> ComponentStatus {
        if self.closed.load(Ordering::Acquire) {
            return ComponentStatus::Stopped;
        }
        if let Some(binding) = self.parent.get() {
            if super::record_token(&binding.control.desired_snapshot(), &binding.node)
                != Some(binding.token)
            {
                return ComponentStatus::Stopped;
            }
            if let Some(parent) = binding.control.observed().components.get(&binding.node) {
                if parent.failure.is_some() {
                    return ComponentStatus::Error;
                }
                if !parent.lifecycle_requested {
                    return binding.initial_status;
                }
                match parent.lifecycle {
                    ComponentLifecycle::Starting if !self.requested.load(Ordering::Acquire) => {
                        return ComponentStatus::Starting;
                    }
                    ComponentLifecycle::Stopping => return ComponentStatus::Stopping,
                    ComponentLifecycle::Stopped => {
                        return if self.started.load(Ordering::Acquire) {
                            ComponentStatus::Stopped
                        } else {
                            binding.initial_status
                        };
                    }
                    ComponentLifecycle::Failed => return ComponentStatus::Error,
                    _ => {}
                }
            }
        }
        if !self.started.load(Ordering::Acquire) {
            return ComponentStatus::Added;
        }
        if !self.requested.load(Ordering::Acquire) {
            return ComponentStatus::Stopped;
        }
        let Some(execution) = self.execution.get() else {
            return ComponentStatus::Added;
        };
        let snapshot = execution.inspector.snapshot();
        let Some(query) = snapshot
            .observed
            .components
            .get(&ComponentId::try_new(self.config.id.as_str()).expect("validated query ID"))
        else {
            return ComponentStatus::Starting;
        };
        if query.failure.is_some() || query.realization == RealizationState::CreationFailed {
            return ComponentStatus::Error;
        }
        match query.lifecycle {
            ComponentLifecycle::Running => ComponentStatus::Running,
            ComponentLifecycle::Failed => ComponentStatus::Error,
            ComponentLifecycle::Stopping => ComponentStatus::Stopping,
            _ => ComponentStatus::Starting,
        }
    }
    pub(super) fn last_error(&self) -> Option<String> {
        let snapshot = self.execution.get()?.inspector.snapshot();
        let id = ComponentId::try_new(self.config.id.as_str()).expect("validated query");
        snapshot
            .observed
            .components
            .get(&id)
            .and_then(|component| component.failure.as_ref())
            .or_else(|| {
                snapshot
                    .observed
                    .components
                    .values()
                    .find_map(|component| component.failure.as_ref())
            })
            .map(|failure| format!("{:#}", failure.cause))
    }
}

#[async_trait]
impl RuntimeComponent for QueryInstance {
    async fn wait_ready(&self) -> anyhow::Result<()> {
        let execution = self.execution()?;
        let mut changes = execution.inspector.subscribe();
        let id = ComponentId::try_new(self.config.id.as_str())?;
        loop {
            self.require_current()?;
            // Measure readiness in the execution graph, independently of the
            // containing service's transitional lifecycle state.
            let snapshot = changes.borrow_and_update().clone();
            let node =
                snapshot.observed.components.get(&id).ok_or_else(|| {
                    anyhow::anyhow!("query component is not in its execution graph")
                })?;
            if node.started && node.lifecycle == ComponentLifecycle::Running {
                return Ok(());
            }
            if let Some(failure) = &node.failure {
                return Err(GraphError::Reported {
                    cause: failure.cause.clone(),
                }
                .into());
            }
            changes.changed().await?;
        }
    }
    fn id(&self) -> &str {
        &self.config.id
    }
    fn kind(&self) -> &'static str {
        "query"
    }
    async fn initialize(&self, _: ComponentGeneration) -> anyhow::Result<()> {
        let _initializing = self.initializing.lock().await;
        self.require_current()?;
        if self.closed.load(Ordering::Acquire) {
            anyhow::bail!("query is permanently shut down");
        }
        if self.execution.get().is_none() {
            let owner = self
                .owner
                .upgrade()
                .ok_or_else(|| anyhow::anyhow!("native runtime is gone"))?;
            let sources = owner.query_sources(&self.config).await?;
            let execution = self.construct(sources, &owner)?;
            self.execution
                .set(execution)
                .map_err(|_| anyhow::anyhow!("query execution was already constructed"))?;
        }
        let execution = self.execution()?;
        let mut life = execution.life.lock().await;
        life.ready().await?;
        if life.link.is_none() {
            life.link = Some(
                self.global_catalog
                    .link_query(&self.config.id, &execution.catalog)?,
            );
        }
        self.notify();
        Ok(())
    }
    async fn start(&self) -> anyhow::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            anyhow::bail!("query is permanently shut down");
        }
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("native runtime is gone"))?;
        let (_, volatile) = owner
            .config
            .index_factory
            .computation_backend(self.config.storage_backend.as_ref())?;
        for source in &self.config.sources {
            let source_instance = owner.source(&source.source_id).await?;
            if !volatile && !source_instance.supports_replay() {
                anyhow::bail!("IncompatibleSource: sources that do not support replay cannot feed persistent query '{}' (source '{}', supports_replay=false)", self.config.id, source.source_id);
            }
        }
        let execution = self.execution()?;
        for subscription in &execution.subscriptions {
            subscription.prepare_start();
        }
        self.started.store(true, Ordering::Release);
        self.requested.store(true, Ordering::Release);
        self.notify();
        let mut life = execution.life.lock().await;
        let control = life.scope.control()?;
        let revision = control.desired_snapshot().revision;
        let mut activation: BoxFuture<'static, GraphResult<StartReport>> =
            Box::pin(async move { control.start_requested(revision, GraphSelection::All).await });
        let subscriptions = execution.subscriptions.clone();
        let ready = async move {
            for subscription in subscriptions {
                subscription.wait_subscribed().await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let report = life
            .scope
            .drive(async {
                tokio::select! {
                    biased;
                    result = ready => { result?; Ok(None) },
                    report = &mut activation => Ok(Some(report?)),
                }
            })
            .await
            .map_err(|error| {
                error.context(format!(
                    "Query '{}' under {:?} recovery: {}",
                    self.config.id,
                    self.config
                        .recovery_policy
                        .or(owner.config.default_recovery_policy)
                        .unwrap_or_default(),
                    self.last_error()
                        .unwrap_or_else(|| "input subscription failed".into()),
                ))
            })?;
        life.activation = Some(match report {
            Some(report) => Box::pin(async move { Ok(report) }),
            None => activation,
        });
        self.notify();
        Ok(())
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.started.store(true, Ordering::Release);
        self.requested.store(false, Ordering::Release);
        let Some(execution) = self.execution.get() else {
            return Ok(());
        };
        let mut life = execution.life.lock().await;
        life.activation = None;
        let result = life.scope.stop().await;
        self.notify();
        result
    }
    async fn run(&self) -> anyhow::Result<()> {
        let execution = self.execution()?;
        let mut life = execution.life.lock().await;
        if let Some(activation) = life.activation.take() {
            let report = life
                .scope
                .drive(async move { Ok(activation.await?) })
                .await?;
            self.notify();
            if report.summary != OperationSummary::Completed {
                anyhow::bail!("query activation failed: {:?}", report.components);
            }
        }
        let mut changes = execution.inspector.subscribe();
        loop {
            tokio::select! {
                result = life.scope.run() => return result,
                result = changes.changed() => { result?; self.notify(); }
            }
        }
    }
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.closed.store(true, Ordering::Release);
        self.requested.store(false, Ordering::Release);
        let _initializing = self.initializing.lock().await;
        if let Some(execution) = self.execution.get() {
            let mut life = execution.life.lock().await;
            life.activation = None;
            life.scope.shutdown().await?;
            life.link = None;
        }
        self.notify();
        Ok(())
    }
    async fn deprovision(&self) -> anyhow::Result<()> {
        self.cleanup.quiesce().await?;
        if self.deleted.load(Ordering::Acquire) {
            return Ok(());
        }
        let Some(execution) = self.execution.get() else {
            // Declaration and validation alone never open query indexes.
            self.deleted.store(true, Ordering::Release);
            return Ok(());
        };
        let owner = self
            .owner
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("native runtime is gone"))?;
        let factory = owner.config.index_factory.clone();
        let (backend, volatile) =
            factory.computation_backend(self.config.storage_backend.as_ref())?;
        if volatile {
            self.deleted.store(true, Ordering::Release);
            return Ok(());
        }
        let query = self.config.id.clone();
        let key = LegacyIndexProviderAdapter::storage_key(
            Some(owner.services.scope.as_ref()),
            execution.inspector.snapshot().desired.id.as_ref(),
            &query,
        );
        let deleted = self.deleted.clone();
        self.cleanup
            .run_async(async move {
                use drasi_core::interface::IndexError;
                let indexes = factory
                    .build(&backend, &key)
                    .await
                    .map_err(IndexError::other)?;
                indexes.set.session_control.begin().await?;
                let clear = async {
                    indexes.set.element_index.clear().await?;
                    indexes.set.archive_index.clear().await?;
                    indexes.set.result_index.clear().await?;
                    indexes.set.result_index.apply_sequence(0, "").await?;
                    indexes.set.future_queue.clear().await?;
                    if let Some(store) = &indexes.checkpoint_store {
                        store.clear_checkpoints().await?;
                        store.write_result_sequence(&query, 0).await?;
                    }
                    if let Some(store) = &indexes.outbox_writer {
                        store.clear(&query).await?;
                        store.clear(&query_reset_key(&query)).await?;
                    }
                    if let Some(store) = &indexes.live_results_writer {
                        store.clear(&query).await?;
                    }
                    Ok::<_, IndexError>(())
                }
                .await;
                if let Err(error) = clear {
                    if let Err(rollback) = indexes.set.session_control.rollback() {
                        return Err(IndexError::other(std::io::Error::new(
                            std::io::ErrorKind::Other,
                            format!("index cleanup failed: {error}; rollback failed: {rollback}"),
                        )));
                    }
                    return Err(error);
                }
                indexes.set.session_control.commit().await?;
                deleted.store(true, Ordering::Release);
                Ok(())
            })
            .await?;
        self.cleanup.quiesce().await?;
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.read_status()
    }
}

struct LegacyReceiver(CatalogSubscription);
#[async_trait]
impl ChangeReceiver<QueryResult> for LegacyReceiver {
    async fn recv(&mut self) -> anyhow::Result<Arc<QueryResult>> {
        Ok(Arc::new(QueryChangeCodec::to_legacy_result(
            &self.0.receive().await?,
        )?))
    }
}

#[async_trait]
impl Query for QueryInstance {
    async fn start(&self) -> anyhow::Result<()> {
        self.require_current()?;
        self.owner
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("native runtime is gone"))?
            .start_component_checked(
                &self.config.id,
                "query",
                self.parent.get().map(|binding| binding.token),
            )
            .await
    }
    async fn stop(&self) -> anyhow::Result<()> {
        self.require_current()?;
        self.owner
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("native runtime is gone"))?
            .stop_component_checked(
                &self.config.id,
                "query",
                self.parent.get().map(|binding| binding.token),
            )
            .await
    }
    async fn status(&self) -> ComponentStatus {
        self.read_status()
    }
    fn get_config(&self) -> &QueryConfig {
        &self.config
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn subscription_count(&self) -> usize {
        if self.requested.load(Ordering::Acquire) {
            self.execution
                .get()
                .map_or(0, |execution| execution.subscriptions.len())
        } else {
            0
        }
    }
    async fn subscribe(&self, _: String) -> anyhow::Result<QuerySubscriptionResponse> {
        self.require_current()?;
        self.fetch_snapshot().await?;
        let (receiver, as_of_sequence) = self
            .execution()?
            .catalog
            .subscribe_query_at_head(&self.config.id)?;
        Ok(QuerySubscriptionResponse {
            query_id: self.config.id.clone(),
            receiver: Box::new(LegacyReceiver(receiver)),
            as_of_sequence,
        })
    }
    async fn fetch_snapshot(&self) -> std::result::Result<SnapshotResponse, FetchError> {
        let status = self.read_status();
        if !matches!(status, ComponentStatus::Starting | ComponentStatus::Running) {
            return Err(FetchError::NotRunning { status });
        }
        let Some(execution) = self.execution.get() else {
            return Err(FetchError::NotRunning { status });
        };
        execution
            .catalog
            .legacy_snapshot(
                &self.config.id,
                Duration::from_secs(self.config.bootstrap_timeout_secs),
            )
            .await
            .map_err(|error| {
                log::warn!("Native snapshot unavailable: {error:#}");
                if error
                    .downcast_ref::<tokio::time::error::Elapsed>()
                    .is_some()
                {
                    FetchError::TimedOut
                } else {
                    FetchError::NotRunning {
                        status: self.read_status(),
                    }
                }
            })
    }
    async fn fetch_outbox(&self, after: u64) -> std::result::Result<OutboxResponse, FetchError> {
        let status = self.read_status();
        if !matches!(status, ComponentStatus::Starting | ComponentStatus::Running) {
            return Err(FetchError::NotRunning { status });
        }
        let Some(execution) = self.execution.get() else {
            return Err(FetchError::NotRunning { status });
        };
        execution
            .catalog
            .legacy_outbox(
                &self.config.id,
                after,
                Duration::from_secs(self.config.bootstrap_timeout_secs),
            )
            .await
    }
    fn output_metrics(&self) -> Option<Arc<QueryOutputMetrics>> {
        Some(self.metrics.clone())
    }
    async fn release_persistent_handles(&self) {
        if let Err(error) = RuntimeComponent::shutdown(self).await {
            log::error!("Native query cleanup failed: {error:#}");
        }
    }
}
