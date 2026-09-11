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

mod component;
mod query;
mod reaction;
mod source;
#[cfg(test)]
mod tests;

use super::v1::*;
use crate::{
    component_graph::{ComponentGraph, ComponentUpdate},
    config::{QueryConfig, RuntimeConfig},
    metrics::{LifecycleMetrics, ReactionMetricsSnapshot},
    queries::Query as QueryTrait,
    ComponentStatus, DrasiLib, Reaction, Source,
};
use async_trait::async_trait;
use component::{RuntimeComponent, RuntimeFactory, RuntimeInstance};
pub(crate) use query::QueryInstance;
use reaction::ReactionInstance;
use source::SourceInstance;
use std::{
    collections::{BTreeMap, HashMap},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock, Weak,
    },
};
use tokio::sync::{watch, Mutex, RwLock};

type SourceRegistration = (Box<dyn Source>, HashMap<String, String>);
type ReactionRegistration = (Box<dyn Reaction>, HashMap<String, String>);
type BootstrapRegistration = (String, String, HashMap<String, serde_json::Value>);

pub(crate) async fn build(
    config: Arc<RuntimeConfig>,
    sources: Vec<SourceRegistration>,
    reactions: Vec<ReactionRegistration>,
    bootstraps: Vec<BootstrapRegistration>,
    wal: Option<Arc<dyn crate::wal::WalProvider>>,
) -> crate::Result<DrasiLib> {
    let mut core = DrasiLib::new(config.clone());
    let setup = async {
        let runtime = Runtime::new(&core, wal).await?;
        core.computation_runtime = Some(runtime.clone());
        core.inspection.set_computation(runtime.clone());
        let source = crate::sources::component_graph_source::ComponentGraphSource::new(
            core.component_event_broadcast_tx.clone(),
            config.id.clone(),
            core.component_graph.clone(),
        )?;
        runtime
            .add_source(Box::new(source), HashMap::new(), false)
            .await?;
        for (source, metadata) in sources {
            runtime.add_source(source, metadata, false).await?;
        }
        for query in &config.queries {
            runtime.add_query(query.clone(), false).await?;
        }
        for (reaction, metadata) in reactions {
            runtime.add_reaction(reaction, metadata, false).await?;
        }
        for (source, kind, properties) in bootstraps {
            let mut metadata = HashMap::from([("kind".to_owned(), kind)]);
            for (name, value) in properties {
                metadata.insert(name, serde_json::to_string(&value)?);
            }
            core.component_graph
                .write()
                .await
                .register_bootstrap_provider(&format!("{source}-bootstrap"), metadata, &[source])?;
        }
        if config.identity_provider.is_some() {
            let ids = runtime
                .current_records()
                .await?
                .values()
                .filter_map(|record| match &record.value {
                    Value::Source(source) => Some(source.source.id().to_owned()),
                    Value::Reaction(reaction) => Some(reaction.reaction.id().to_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            core.component_graph
                .write()
                .await
                .register_identity_provider(
                    "identity-provider",
                    HashMap::from([("kind".into(), "identity_provider".into())]),
                    &ids,
                )?;
        }
        core.state_guard.mark_initialized();
        Ok::<_, anyhow::Error>(())
    }
    .await;
    if let Err(error) = setup {
        return Err(super::instance::cleanup_failed_instance(core, error.into()).await);
    }
    Ok(core)
}

#[derive(Clone)]
enum Value {
    Source(Arc<SourceInstance>),
    Query(Arc<QueryInstance>),
    Reaction(Arc<ReactionInstance>),
}
impl Value {
    fn instance(&self) -> Arc<dyn RuntimeComponent> {
        match self {
            Self::Source(value) => value.clone(),
            Self::Query(value) => value.clone(),
            Self::Reaction(value) => value.clone(),
        }
    }
    fn auto_start(&self) -> bool {
        match self {
            Self::Source(value) => value.source.auto_start(),
            Self::Query(value) => value.config.auto_start,
            Self::Reaction(value) => value.reaction.auto_start(),
        }
    }
}
#[derive(Clone)]
struct Record {
    token: u64,
    node: ComponentId,
    resource: ResourceId,
    value: Value,
    owner: Arc<RuntimeInstance>,
    metadata: HashMap<String, String>,
    initial_status: ComponentStatus,
}

fn record_token(snapshot: &GraphSnapshot, node: &ComponentId) -> Option<u64> {
    match snapshot
        .specifications
        .get(node)?
        .configuration
        .get("record_token")?
    {
        ConfigurationValue::Literal(value) => value.as_u64(),
        _ => None,
    }
}

pub(crate) struct Runtime {
    config: Arc<RuntimeConfig>,
    services: LegacyPluginServices,
    middleware: Arc<drasi_core::middleware::MiddlewareTypeRegistry>,
    projection: Arc<RwLock<ComponentGraph>>,
    parent: OnceLock<ComputationHandle>,
    records: RwLock<BTreeMap<u64, Record>>,
    mutations: Mutex<()>,
    projecting: Mutex<()>,
    next_instance: AtomicU64,
    changed: watch::Sender<u64>,
    catalog: QueryResultsCatalog,
    factory: Arc<RuntimeFactory>,
    logs: Arc<crate::managers::ComponentLogRegistry>,
    pub(crate) lifecycle_metrics: Arc<LifecycleMetrics>,
}

impl Runtime {
    pub(crate) async fn new(
        core: &DrasiLib,
        wal: Option<Arc<dyn crate::wal::WalProvider>>,
    ) -> anyhow::Result<Arc<Self>> {
        let runtime = Arc::new(Self {
            config: core.config.clone(),
            services: LegacyPluginServices {
                scope: Arc::from(core.config.id.as_str()),
                state_store: Some(core.config.state_store_provider.clone()),
                identity: core.config.identity_provider.clone(),
                wal,
                secrets: core.config.secret_store_provider.clone(),
            },
            middleware: core.middleware_registry.clone(),
            projection: core.component_graph.clone(),
            parent: OnceLock::new(),
            records: RwLock::new(BTreeMap::new()),
            mutations: Mutex::new(()),
            projecting: Mutex::new(()),
            next_instance: AtomicU64::new(1),
            changed: watch::channel(0).0,
            catalog: QueryResultsCatalog::new("__drasi_lib_queries__")?,
            factory: Arc::new(RuntimeFactory::default()),
            logs: core.log_registry.clone(),
            lifecycle_metrics: Arc::new(LifecycleMetrics::new()),
        });
        let projector = Projection {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("__inspection_projection__")?,
                vec![],
            )?,
            runtime: Arc::downgrade(&runtime),
        };
        let graph = ComputationGraph::builder("__drasi_lib_runtime__")
            .service(Box::new(projector))
            .build()?;
        let handle = core
            .computation_registry
            .add(graph, ComputationOptions { auto_start: false })
            .await?;
        runtime
            .parent
            .set(handle.clone())
            .map_err(|_| anyhow::anyhow!("native runtime already initialized"))?;
        handle.start().await?;
        Ok(runtime)
    }
    fn parent(&self) -> anyhow::Result<ComputationHandle> {
        self.parent
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("native runtime is not initialized"))
    }
    pub(crate) fn inspector(&self) -> anyhow::Result<ComputationInspector> {
        Ok(self.parent()?.inspector())
    }
    fn notify(&self) {
        self.changed
            .send_modify(|value| *value = value.wrapping_add(1));
    }

    fn next_token(&self) -> anyhow::Result<u64> {
        self.next_instance
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| anyhow::anyhow!("runtime generation exhausted"))
    }

    async fn records_at(
        &self,
        desired: &GraphSnapshot,
    ) -> anyhow::Result<BTreeMap<String, Record>> {
        let candidates = self.records.read().await;
        let mut records = BTreeMap::new();
        for node in desired.specifications.keys() {
            let Some(token) = record_token(desired, node) else {
                continue;
            };
            let record = candidates
                .get(&token)
                .filter(|record| &record.node == node)
                .ok_or_else(|| {
                    anyhow::anyhow!("native construction {token} has no owned runtime record")
                })?;
            record.owner.installed.store(true, Ordering::Release);
            records.insert(record.value.instance().id().to_owned(), record.clone());
        }
        Ok(records)
    }

    async fn current_records(&self) -> anyhow::Result<BTreeMap<String, Record>> {
        self.records_at(&self.parent()?.control().desired_snapshot())
            .await
    }

    async fn validate_registration(
        &self,
        id: &str,
        kind: crate::component_graph::ComponentKind,
        allow_placeholder: bool,
    ) -> anyhow::Result<()> {
        ComponentId::try_new(id)?;
        let projection = self.projection.read().await;
        if let Some(node) = projection.get_component(id) {
            if node.kind != kind
                || projection.has_runtime(id)
                || !(allow_placeholder
                    || node
                        .metadata
                        .get("unboundReference")
                        .is_some_and(|value| value == "true"))
            {
                anyhow::bail!("Component with id '{id}' already exists");
            }
        }
        if self.current_records().await?.contains_key(id) {
            anyhow::bail!("Component with id '{id}' already exists");
        }
        Ok(())
    }

    async fn discard_uninstalled(&self, record: &Record) -> anyhow::Result<()> {
        if record_token(&self.parent()?.control().desired_snapshot(), &record.node)
            != Some(record.token)
        {
            record.owner.shutdown().await?;
            self.records.write().await.remove(&record.token);
        }
        Ok(())
    }

    async fn validate_reference_roles(
        &self,
        ids: impl IntoIterator<Item = String>,
        kind: crate::component_graph::ComponentKind,
    ) -> anyhow::Result<()> {
        let projection = self.projection.read().await;
        for id in ids {
            if projection
                .get_component(&id)
                .is_some_and(|node| node.kind != kind)
            {
                anyhow::bail!("Component '{id}' is not a {kind}");
            }
        }
        Ok(())
    }

    async fn register(
        &self,
        value: Value,
        definition: serde_json::Value,
        metadata: HashMap<String, String>,
    ) -> anyhow::Result<Record> {
        let component = value.instance();
        let id = component.id().to_owned();
        let number = self.next_token()?;
        let node = ComponentId::try_new(format!("instance/{number}"))?;
        let resource = ResourceId::try_new(format!("instance/{number}"))?;
        let instance = Arc::new(RuntimeInstance::new(
            component.clone(),
            number,
            &self.config.id,
        ));
        let control = self.parent()?.control();
        if let Value::Query(query) = &value {
            query.bind_parent(
                control.clone(),
                node.clone(),
                number,
                ComponentStatus::Added,
            )?;
        }
        let factory = self.factory.clone();
        let spec = ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(node.clone(), vec![])?,
            role: ComponentRole::Service,
            completion: None,
            implementation: factory.descriptor().implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([
                (
                    Arc::from("id"),
                    ConfigurationValue::Literal(id.clone().into()),
                ),
                (
                    Arc::from("kind"),
                    ConfigurationValue::Literal(component.kind().into()),
                ),
                (
                    Arc::from("definition"),
                    ConfigurationValue::Literal(definition),
                ),
                (
                    Arc::from("record_token"),
                    ConfigurationValue::Literal(number.into()),
                ),
            ]),
            dependencies: BTreeMap::from([(Arc::from("instance"), vec![resource.clone()])]),
        };
        let desired = DesiredComponent {
            descriptor: spec.descriptor.clone(),
            role: ComponentRole::Service,
            completion: None,
            streams: BTreeMap::new(),
            lifecycle: LifecyclePolicy { auto_start: false },
            input_merge: InputMergePolicy::default(),
            construction: ComponentConstruction::Factory(spec),
        };
        let record = Record {
            token: number,
            node,
            resource: resource.clone(),
            value,
            owner: instance.clone(),
            metadata,
            initial_status: ComponentStatus::Added,
        };
        self.records.write().await.insert(number, record.clone());
        let changes = vec![
            DesiredMutation::PutResource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::Component,
                ownership: ResourceOwnership::Graph,
                binding: Arc::from(format!("{}:{id}", component.kind())),
            }),
            DesiredMutation::PutComponent(desired),
        ];
        let result = async {
            let preview = control
                .preview(control.desired_snapshot().revision, changes)
                .await?;
            let mut bindings = TopologyBindings::default();
            bindings.factories.register(factory)?;
            bindings.resources.insert(
                resource,
                ResourceHandle::new(ResourceRole::Component, instance.clone())
                    .with_cleanup(instance),
            );
            let report = control.reconcile(preview, bindings).await?;
            if report.summary != OperationSummary::Completed {
                anyhow::bail!("native component creation failed: {report:?}");
            }
            Ok::<_, anyhow::Error>(record.clone())
        }
        .await;
        self.project().await?;
        if result.is_err() {
            self.discard_uninstalled(&record).await?;
        }
        self.notify();
        result
    }

    pub(crate) async fn add_source(
        self: &Arc<Self>,
        source: Box<dyn Source>,
        metadata: HashMap<String, String>,
        start: bool,
    ) -> anyhow::Result<()> {
        let mutation = self.mutations.lock().await;
        let id = source.id().to_owned();
        let mut meta = metadata;
        meta.entry("kind".into())
            .or_insert_with(|| source.type_name().to_owned());
        meta.entry("autoStart".into())
            .or_insert_with(|| source.auto_start().to_string());
        self.validate_registration(&id, crate::component_graph::ComponentKind::Source, false)
            .await?;
        let source = SourceInstance::new(source, self.services.clone(), self.changed.clone());
        let record = match self
            .register(
                Value::Source(source.clone()),
                serde_json::json!({"auto_start":source.source.auto_start()}),
                meta,
            )
            .await
        {
            Ok(record) => record,
            Err(error) => {
                return Err(error);
            }
        };
        drop(mutation);
        if start && source.source.auto_start() {
            self.start_record(&record).await?;
        }
        self.project().await
    }

    pub(crate) async fn add_query(
        self: &Arc<Self>,
        config: QueryConfig,
        start: bool,
    ) -> anyhow::Result<()> {
        self.add_query_definition(config, start, false).await
    }
    pub(crate) async fn declare_query(self: &Arc<Self>, config: QueryConfig) -> anyhow::Result<()> {
        self.add_query_definition(config, false, true).await
    }
    async fn add_query_definition(
        self: &Arc<Self>,
        config: QueryConfig,
        start: bool,
        allow_unbound: bool,
    ) -> anyhow::Result<()> {
        let mutation = self.mutations.lock().await;
        self.validate_reference_roles(
            config.sources.iter().map(|source| source.source_id.clone()),
            crate::component_graph::ComponentKind::Source,
        )
        .await?;
        let sources = {
            let records = self.current_records().await?;
            config
                .sources
                .iter()
                .filter_map(|source| {
                    match records.get(&source.source_id).map(|record| &record.value) {
                        Some(Value::Source(source)) => Some(Ok(source.clone())),
                        None if allow_unbound => None,
                        _ => Some(Err(anyhow::anyhow!(
                            "Source '{}' not found",
                            source.source_id
                        ))),
                    }
                })
                .collect::<anyhow::Result<Vec<_>>>()?
        };
        crate::queries::LabelExtractor::extract_labels(&config.query, &config.query_language)?;
        let id = config.id.clone();
        self.validate_registration(
            &id,
            crate::component_graph::ComponentKind::Query,
            allow_unbound,
        )
        .await?;
        let query = QueryInstance::new(config.clone(), sources, self)?;
        let record = match self
            .register(
                Value::Query(query.clone()),
                serde_json::to_value(&config)?,
                HashMap::new(),
            )
            .await
        {
            Ok(record) => record,
            Err(error) => {
                return Err(error);
            }
        };
        drop(mutation);
        if start && config.auto_start {
            self.start_record(&record).await?;
        }
        self.project().await
    }

    pub(crate) async fn add_reaction(
        self: &Arc<Self>,
        reaction: Box<dyn Reaction>,
        metadata: HashMap<String, String>,
        start: bool,
    ) -> anyhow::Result<()> {
        self.add_reaction_definition(reaction, metadata, start, false)
            .await
    }
    pub(crate) async fn declare_reaction(
        self: &Arc<Self>,
        reaction: Box<dyn Reaction>,
    ) -> anyhow::Result<()> {
        self.add_reaction_definition(reaction, HashMap::new(), false, true)
            .await
    }
    async fn add_reaction_definition(
        self: &Arc<Self>,
        reaction: Box<dyn Reaction>,
        metadata: HashMap<String, String>,
        start: bool,
        allow_unbound: bool,
    ) -> anyhow::Result<()> {
        let mutation = self.mutations.lock().await;
        let id = reaction.id().to_owned();
        let ids = reaction.query_ids();
        self.validate_reference_roles(ids.clone(), crate::component_graph::ComponentKind::Query)
            .await?;
        let records = self.current_records().await?;
        for id in &ids {
            if !allow_unbound
                && !matches!(
                    records.get(id).map(|record| &record.value),
                    Some(Value::Query(_))
                )
            {
                anyhow::bail!("Query '{id}' not found");
            }
        }
        let mut metadata = metadata;
        metadata
            .entry("kind".into())
            .or_insert_with(|| reaction.type_name().to_owned());
        metadata
            .entry("autoStart".into())
            .or_insert_with(|| reaction.auto_start().to_string());
        self.validate_registration(&id, crate::component_graph::ComponentKind::Reaction, false)
            .await?;
        let reaction = ReactionInstance::new(reaction, self)?;
        let auto_start = reaction.reaction.auto_start();
        let record = match self
            .register(
                Value::Reaction(reaction.clone()),
                serde_json::json!({"queries":ids,"auto_start":auto_start}),
                metadata,
            )
            .await
        {
            Ok(record) => record,
            Err(error) => {
                return Err(error);
            }
        };
        drop(mutation);
        if start && auto_start {
            self.start_record(&record).await?;
        }
        self.project().await
    }

    async fn record(&self, id: &str, kind: &str) -> anyhow::Result<Record> {
        self.current_records()
            .await?
            .get(id)
            .filter(|record| record.value.instance().kind() == kind)
            .cloned()
            .ok_or_else(|| {
                crate::managers::ComponentNotFoundError::new(
                    match kind {
                        "source" => "source",
                        "query" => "query",
                        _ => "reaction",
                    },
                    id,
                )
                .into()
            })
    }
    fn control_for(
        &self,
        record: &Record,
    ) -> anyhow::Result<(GraphControl, GraphRevision, ObservedComponent)> {
        let control = self.parent()?.control();
        let desired = control.desired_snapshot();
        if record_token(&desired, &record.node) != Some(record.token) {
            return Err(GraphError::StaleGeneration.into());
        }
        let observed = control
            .observed()
            .components
            .get(&record.node)
            .cloned()
            .ok_or(GraphError::StaleGeneration)?;
        Ok((control, desired.revision, observed))
    }

    fn active(&self, record: &Record) -> anyhow::Result<bool> {
        let (_, _, observed) = self.control_for(record)?;
        Ok(matches!(
            observed.lifecycle,
            ComponentLifecycle::Starting | ComponentLifecycle::Running
        ) && !observed.exhausted)
    }

    async fn start_record(&self, record: &Record) -> anyhow::Result<()> {
        let (control, revision, observed) = self.control_for(record)?;
        if observed.realization == RealizationState::CreationFailed {
            let preview = control
                .preview(
                    revision,
                    vec![DesiredMutation::Retry(GraphSelection::Exact(vec![record.node.clone()]))],
                )
                .await?;
            let report = control
                .reconcile(preview, TopologyBindings::default())
                .await?;
            if report.summary != OperationSummary::Completed {
                self.project().await?;
                anyhow::bail!("native creation retry failed: {report:?}");
            }
        } else if observed.failure.is_some() || observed.lifecycle == ComponentLifecycle::Failed {
            self.stop_record(record).await?;
        }
        let (control, revision, _) = self.control_for(record)?;
        let report = control
            .start_requested(revision, GraphSelection::Exact(vec![record.node.clone()]))
            .await?;
        self.project().await?;
        if report.summary != OperationSummary::Completed
            || !matches!(
                report.components.get(&record.node),
                Some(StartOutcome::Started | StartOutcome::AlreadyRunning)
            )
        {
            anyhow::bail!("native activation failed: {:?}", report.components);
        }
        Ok(())
    }
    pub(crate) async fn start_component(
        self: &Arc<Self>,
        id: &str,
        kind: &str,
    ) -> anyhow::Result<()> {
        self.start_component_checked(id, kind, None).await
    }

    pub(super) async fn start_component_checked(
        self: &Arc<Self>,
        id: &str,
        kind: &str,
        expected: Option<u64>,
    ) -> anyhow::Result<()> {
        let mutation = self.mutations.lock().await;
        let mut record = self.record(id, kind).await?;
        if expected.is_some_and(|token| token != record.token) {
            return Err(GraphError::StaleGeneration.into());
        }
        let (_, _, observed) = self.control_for(&record)?;
        if matches!(
            observed.lifecycle,
            ComponentLifecycle::Starting
                | ComponentLifecycle::Running
                | ComponentLifecycle::Stopping
        ) && observed.failure.is_none()
            && !observed.exhausted
        {
            anyhow::bail!(
                "Component '{id}' cannot start while it is {:?}",
                observed.lifecycle
            );
        }
        let mut resume = Vec::new();
        if let Value::Query(query) = &record.value {
            let sources = {
                let records = self.current_records().await?;
                query
                    .config
                    .sources
                    .iter()
                    .map(|source| {
                        match records.get(&source.source_id).map(|record| &record.value) {
                            Some(Value::Source(source)) => Ok(source.clone()),
                            _ => Err(anyhow::anyhow!("Source '{}' not found", source.source_id)),
                        }
                    })
                    .collect::<anyhow::Result<Vec<_>>>()
            };
            if let Ok(sources) = sources {
                if !query.matches_sources(&sources) {
                    let configuration = query.config.clone();
                    let replacement = QueryInstance::new(configuration.clone(), sources, self)?;
                    resume = self.pause_consumers(&[id.to_owned()]).await?;
                    record = self
                        .replace_record(
                            record,
                            Value::Query(replacement),
                            serde_json::to_value(configuration)?,
                        )
                        .await?;
                }
            }
        }
        drop(mutation);
        self.start_record(&record).await?;
        self.resume_records(resume).await
    }
    async fn stop_record(&self, record: &Record) -> anyhow::Result<()> {
        let (control, revision, _) = self.control_for(record)?;
        let report = control
            .stop_components(revision, GraphSelection::Exact(vec![record.node.clone()]))
            .await?;
        self.project().await?;
        if report.summary != OperationSummary::Completed {
            anyhow::bail!("native stop failed: {:?}", report.components);
        }
        Ok(())
    }
    pub(crate) async fn stop_component(&self, id: &str, kind: &str) -> anyhow::Result<()> {
        self.stop_component_checked(id, kind, None).await
    }

    pub(super) async fn stop_component_checked(
        &self,
        id: &str,
        kind: &str,
        expected: Option<u64>,
    ) -> anyhow::Result<()> {
        let record = self.record(id, kind).await?;
        if expected.is_some_and(|token| token != record.token) {
            return Err(GraphError::StaleGeneration.into());
        }
        let (_, _, observed) = self.control_for(&record)?;
        if !matches!(
            observed.lifecycle,
            ComponentLifecycle::Running | ComponentLifecycle::Starting | ComponentLifecycle::Failed
        ) && observed.failure.is_none()
        {
            anyhow::bail!(
                "Component '{id}' cannot stop while it is {:?}",
                observed.lifecycle
            );
        }
        self.stop_record(&record).await
    }
    pub(crate) async fn remove_component(
        &self,
        id: &str,
        kind: &str,
        cleanup: bool,
    ) -> anyhow::Result<()> {
        let _mutation = self.mutations.lock().await;
        let record = self.record(id, kind).await?;
        let dependents: Vec<_> = self
            .current_records()
            .await?
            .iter()
            .filter_map(|(other_id, other)| {
                let depends = match &other.value {
                    Value::Query(query) => {
                        kind == "source"
                            && query
                                .config
                                .sources
                                .iter()
                                .any(|source| source.source_id == id)
                    }
                    Value::Reaction(reaction) => {
                        kind == "query"
                            && reaction
                                .reaction
                                .query_ids()
                                .iter()
                                .any(|query| query == id)
                    }
                    _ => false,
                };
                depends.then(|| other_id.clone())
            })
            .collect();
        if !dependents.is_empty() {
            anyhow::bail!("Depended on by: {}", dependents.join(", "));
        }
        let (_, _, observed) = self.control_for(&record)?;
        if observed.realization == RealizationState::Created
            && (self.active(&record)? || observed.failure.is_some())
        {
            self.stop_record(&record).await?;
        }
        let control = self.parent()?.control();
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![
                    DesiredMutation::RemoveComponents {
                        selection: GraphSelection::Exact(vec![record.node.clone()]),
                        policy: RemovalPolicy::Reject,
                    },
                    DesiredMutation::RemoveResource {
                        resource: record.resource.clone(),
                        policy: RemovalPolicy::Reject,
                    },
                ],
            )
            .await?;
        if cleanup || kind == "query" {
            record.owner.remove_data();
        }
        let result = control
            .reconcile(preview, TopologyBindings::default())
            .await;
        match result {
            Ok(report) if report.summary == OperationSummary::Completed => {}
            other => {
                record.owner.cancel_unstarted_removal();
                self.project().await?;
                anyhow::bail!("native removal failed: {other:?}");
            }
        }
        self.project().await?;
        self.records
            .write()
            .await
            .retain(|_, record| record.value.instance().id() != id);
        self.logs
            .remove_component_by_key(&crate::managers::ComponentLogKey::new(
                self.config.id.clone(),
                match kind {
                    "source" => crate::channels::ComponentType::Source,
                    "query" => crate::channels::ComponentType::Query,
                    _ => crate::channels::ComponentType::Reaction,
                },
                id,
            ))
            .await;
        self.notify();
        Ok(())
    }

    async fn replace_record(
        &self,
        old: Record,
        value: Value,
        definition: serde_json::Value,
    ) -> anyhow::Result<Record> {
        let component = value.instance();
        let id = component.id().to_owned();
        let (_, _, observed) = self.control_for(&old)?;
        if observed.realization == RealizationState::Created
            && (self.active(&old)? || observed.failure.is_some())
        {
            self.stop_record(&old).await?;
        }
        self.projection.write().await.validate_and_transition(
            &id,
            ComponentStatus::Reconfiguring,
            Some(format!("Reconfiguring {}", component.kind())),
        )?;
        let control = self.parent()?.control();
        let token = self.next_token()?;
        let owner = Arc::new(RuntimeInstance::new(
            component.clone(),
            token,
            &self.config.id,
        ));
        if let Value::Query(query) = &value {
            query.bind_parent(
                control.clone(),
                old.node.clone(),
                token,
                ComponentStatus::Stopped,
            )?;
        }
        let mut desired = control
            .desired_snapshot()
            .select(GraphSelection::Exact(vec![old.node.clone()]))?
            .components
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("native component disappeared"))?;
        let ComponentConstruction::Factory(spec) = &mut desired.construction else {
            anyhow::bail!("runtime component has no factory");
        };
        spec.configuration.insert(
            Arc::from("definition"),
            ConfigurationValue::Literal(definition),
        );
        spec.configuration.insert(
            Arc::from("record_token"),
            ConfigurationValue::Literal(token.into()),
        );
        let new = Record {
            token,
            node: old.node.clone(),
            resource: old.resource.clone(),
            value,
            owner,
            metadata: old.metadata.clone(),
            initial_status: ComponentStatus::Stopped,
        };
        // Retain both candidates before submission. The controller's committed
        // token selects the current record even if the caller drops this await.
        self.records.write().await.insert(token, new.clone());
        let result = async {
            let preview = control
                .preview(
                    control.desired_snapshot().revision,
                    vec![
                        DesiredMutation::ReplaceComponent(desired),
                        DesiredMutation::RebindResource(old.resource.clone()),
                    ],
                )
                .await?;
            let mut bindings = TopologyBindings::default();
            bindings.resources.insert(
                old.resource.clone(),
                ResourceHandle::new(ResourceRole::Component, new.owner.clone())
                    .with_cleanup(new.owner.clone()),
            );
            let report = control.reconcile(preview, bindings).await?;
            if report.summary != OperationSummary::Completed {
                anyhow::bail!("native reconfiguration failed: {report:?}");
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        self.project().await?;
        if let Err(error) = result {
            if let Err(cleanup) = self.discard_uninstalled(&new).await {
                return Err(error.context(format!("replacement cleanup also failed: {cleanup:#}")));
            }
            return Err(error);
        }
        self.records.write().await.remove(&old.token);
        Ok(new)
    }

    async fn pause_consumers(&self, query_ids: &[String]) -> anyhow::Result<Vec<Record>> {
        let records = self.current_records().await?;
        let mut paused = Vec::new();
        for kind in ["reaction", "query"] {
            for record in records.values() {
                let affected = match &record.value {
                    Value::Query(query) => kind == "query" && query_ids.contains(&query.config.id),
                    Value::Reaction(reaction) => {
                        kind == "reaction"
                            && reaction
                                .reaction
                                .query_ids()
                                .iter()
                                .any(|id| query_ids.contains(id))
                    }
                    _ => false,
                };
                if affected && self.active(record)? {
                    self.stop_record(record).await?;
                    paused.push(record.clone());
                }
            }
        }
        Ok(paused)
    }

    async fn resume_records(&self, records: Vec<Record>) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        for old in records.into_iter().rev() {
            let component = old.value.instance();
            let record = self.record(component.id(), component.kind()).await?;
            if let Err(error) = self.start_record(&record).await {
                failures.push(format!("{}: {error:#}", component.id()));
            }
        }
        if !failures.is_empty() {
            anyhow::bail!("native dependent restart failed: {}", failures.join("; "));
        }
        Ok(())
    }

    pub(crate) async fn update_source(
        self: &Arc<Self>,
        id: &str,
        source: Box<dyn Source>,
    ) -> anyhow::Result<()> {
        let mutation = self.mutations.lock().await;
        let old = self.record(id, "source").await?;
        if source.id() != id {
            anyhow::bail!(
                "New source ID '{}' does not match existing source ID '{id}'",
                source.id()
            );
        }
        let affected: Vec<_> = self
            .current_records()
            .await?
            .values()
            .filter_map(|record| match &record.value {
                Value::Query(query)
                    if query
                        .config
                        .sources
                        .iter()
                        .any(|source| source.source_id == id) =>
                {
                    Some(query.config.id.clone())
                }
                _ => None,
            })
            .collect();
        let mut resume = self.pause_consumers(&affected).await?;
        if self.active(&old)? {
            resume.push(old.clone());
        }
        let source = SourceInstance::new(source, self.services.clone(), self.changed.clone());
        self.replace_record(
            old,
            Value::Source(source.clone()),
            serde_json::json!({"auto_start":source.source.auto_start()}),
        )
        .await?;
        for id in affected {
            let record = self.record(&id, "query").await?;
            let Value::Query(query) = &record.value else {
                unreachable!()
            };
            let config = query.config.clone();
            let current = self.current_records().await?;
            let sources = config
                .sources
                .iter()
                .filter_map(|source| {
                    match current.get(&source.source_id).map(|record| &record.value) {
                        Some(Value::Source(source)) => Some(source.clone()),
                        _ => None,
                    }
                })
                .collect();
            let query = QueryInstance::new(config.clone(), sources, self)?;
            self.replace_record(record, Value::Query(query), serde_json::to_value(config)?)
                .await?;
        }
        drop(mutation);
        self.resume_records(resume).await
    }
    pub(crate) async fn update_query(
        self: &Arc<Self>,
        id: &str,
        config: QueryConfig,
    ) -> anyhow::Result<()> {
        let mutation = self.mutations.lock().await;
        let old = self.record(id, "query").await?;
        if config.id != id {
            anyhow::bail!(
                "New query ID '{}' does not match existing query ID '{id}'",
                config.id
            );
        }
        let sources = {
            let records = self.current_records().await?;
            config
                .sources
                .iter()
                .map(
                    |source| match records.get(&source.source_id).map(|record| &record.value) {
                        Some(Value::Source(source)) => Ok(source.clone()),
                        _ => Err(anyhow::anyhow!("Source '{}' not found", source.source_id)),
                    },
                )
                .collect::<anyhow::Result<Vec<_>>>()?
        };
        let query = QueryInstance::new(config.clone(), sources, self)?;
        let resume = self.pause_consumers(&[id.to_owned()]).await?;
        self.replace_record(old, Value::Query(query), serde_json::to_value(config)?)
            .await?;
        drop(mutation);
        self.resume_records(resume).await
    }
    pub(crate) async fn update_reaction(
        self: &Arc<Self>,
        id: &str,
        reaction: Box<dyn Reaction>,
    ) -> anyhow::Result<()> {
        let mutation = self.mutations.lock().await;
        let old = self.record(id, "reaction").await?;
        if reaction.id() != id {
            anyhow::bail!(
                "New reaction ID '{}' does not match existing reaction ID '{id}'",
                reaction.id()
            );
        }
        let ids = reaction.query_ids();
        let records = self.current_records().await?;
        for id in &ids {
            if !matches!(
                records.get(id).map(|record| &record.value),
                Some(Value::Query(_))
            ) {
                anyhow::bail!("Query '{id}' not found");
            }
        }
        let restart = self.active(&old)?;
        let reaction = ReactionInstance::new(reaction, self)?;
        let new = self
            .replace_record(
                old,
                Value::Reaction(reaction.clone()),
                serde_json::json!({"queries":ids,"auto_start":reaction.reaction.auto_start()}),
            )
            .await?;
        drop(mutation);
        if restart {
            self.start_record(&new).await?;
        }
        Ok(())
    }
    pub(crate) async fn start_kind(self: &Arc<Self>, kind: &str) -> anyhow::Result<()> {
        let records: Vec<_> = self
            .current_records()
            .await?
            .values()
            .filter(|record| record.value.instance().kind() == kind && record.value.auto_start())
            .cloned()
            .collect();
        let mut failures = Vec::new();
        for record in records {
            if self.active(&record)? {
                continue;
            }
            let result = self
                .start_component_checked(record.value.instance().id(), kind, Some(record.token))
                .await;
            if kind == "query" {
                if let Err(error) = result {
                    log::warn!("Query startup failed: {error:#}");
                }
            } else {
                if let Err(error) = result {
                    failures.push(format!("{error:#}"));
                }
            }
        }
        if !failures.is_empty() {
            anyhow::bail!("native {kind} startup failed: {}", failures.join("; "));
        }
        Ok(())
    }
    pub(crate) async fn subscriptions_complete(&self) -> anyhow::Result<()> {
        let sources: Vec<_> = self
            .current_records()
            .await?
            .values()
            .filter_map(|record| match &record.value {
                Value::Source(source) => Some(source.source.clone()),
                _ => None,
            })
            .collect();
        for source in sources {
            source.on_subscriptions_complete().await;
        }
        Ok(())
    }
    pub(crate) async fn stop_all(&self) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        for kind in ["reaction", "query", "source"] {
            let records: Vec<_> = self
                .current_records()
                .await?
                .values()
                .filter(|record| record.value.instance().kind() == kind)
                .cloned()
                .collect();
            for record in records {
                let (_, _, observed) = self.control_for(&record)?;
                if observed.realization == RealizationState::Created
                    && (self.active(&record)? || observed.failure.is_some())
                {
                    if let Err(error) = self.stop_record(&record).await {
                        failures.push(error.to_string());
                    }
                }
            }
        }
        if !failures.is_empty() {
            anyhow::bail!("native stop failed: {}", failures.join("; "));
        }
        Ok(())
    }
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.parent()?.shutdown().await?;
        let candidates: Vec<_> = self.records.read().await.values().cloned().collect();
        for record in candidates {
            record.owner.shutdown().await?;
        }
        self.projection
            .write()
            .await
            .take_runtime::<Arc<dyn Source>>(crate::sources::COMPONENT_GRAPH_SOURCE_ID);
        Ok(())
    }
    pub(crate) async fn source(&self, id: &str) -> anyhow::Result<Arc<dyn Source>> {
        match self.record(id, "source").await?.value {
            Value::Source(source) => Ok(source.source.clone()),
            _ => unreachable!(),
        }
    }
    pub(super) async fn query(&self, id: &str) -> anyhow::Result<Arc<QueryInstance>> {
        match self.record(id, "query").await?.value {
            Value::Query(query) => Ok(query),
            _ => unreachable!(),
        }
    }
    pub(crate) async fn reaction_metrics(
        &self,
        id: &str,
    ) -> anyhow::Result<HashMap<String, ReactionMetricsSnapshot>> {
        match self.record(id, "reaction").await?.value {
            Value::Reaction(reaction) => Ok(reaction
                .metrics
                .iter()
                .map(|(id, metrics)| (id.clone(), metrics.snapshot()))
                .collect()),
            _ => unreachable!(),
        }
    }
    pub(crate) async fn graph_schema(&self) -> anyhow::Result<crate::schema::GraphSchema> {
        let records: Vec<_> = self.current_records().await?.values().cloned().collect();
        let mut schema = crate::schema::GraphSchema::default();
        for record in &records {
            if let Value::Source(source) = &record.value {
                if source.source.id() == crate::sources::COMPONENT_GRAPH_SOURCE_ID {
                    continue;
                }
                if let Some(source_schema) = source
                    .source
                    .describe_schema()
                    .filter(|schema| !schema.is_empty())
                {
                    schema.merge_source_schema(source.source.id(), &source_schema);
                } else {
                    schema.record_source_without_schema(source.source.id());
                }
            }
        }
        for record in records {
            if let Value::Query(query) = record.value {
                let labels = crate::queries::LabelExtractor::extract_labels(
                    &query.config.query,
                    &query.config.query_language,
                )?;
                schema.mark_queried_nodes(
                    labels.node_labels.iter().map(String::as_str),
                    &query.config.id,
                );
                schema.mark_queried_relations(
                    labels.relation_labels.iter().map(String::as_str),
                    &query.config.id,
                );
            }
        }
        Ok(schema)
    }
    async fn project(&self) -> anyhow::Result<()> {
        let _projecting = self.projecting.lock().await;
        let publication = self.inspector()?.snapshot();
        let records = self.records_at(&publication.desired).await?;
        let retired: Vec<_> = self
            .records
            .read()
            .await
            .values()
            .filter(|record| record.owner.installed.load(Ordering::Acquire))
            .map(|record| record.value.instance().id().to_owned())
            .filter(|id| !records.contains_key(id))
            .collect();
        {
            use crate::component_graph::RelationshipKind;
            let mut projection = self.projection.write().await;
            for kind in ["source", "query", "reaction"] {
                for record in records
                    .values()
                    .filter(|record| record.value.instance().kind() == kind)
                {
                    let component = record.value.instance();
                    let id = component.id();
                    let dependencies = match &record.value {
                        Value::Source(_) => Vec::new(),
                        Value::Query(query) => query
                            .config
                            .sources
                            .iter()
                            .map(|source| source.source_id.clone())
                            .collect(),
                        Value::Reaction(reaction) => reaction.reaction.query_ids(),
                    };
                    for dependency in &dependencies {
                        if !projection.contains(dependency) {
                            let metadata =
                                HashMap::from([("unboundReference".into(), "true".into())]);
                            if kind == "query" {
                                projection.register_source(dependency, metadata)?;
                            } else {
                                projection.register_query(dependency, metadata, &[])?;
                            }
                        }
                    }
                    if !projection.contains(id) {
                        match kind {
                            "source" => projection.register_source(id, record.metadata.clone())?,
                            "query" => projection.register_query(
                                id,
                                record.metadata.clone(),
                                &dependencies,
                            )?,
                            _ => projection.register_reaction(
                                id,
                                record.metadata.clone(),
                                &dependencies,
                            )?,
                        }
                    }
                    let node = projection
                        .get_component_mut(id)
                        .expect("registered projection");
                    node.metadata = record.metadata.clone();
                    match &record.value {
                        Value::Source(source) => {
                            node.metadata
                                .insert("kind".into(), source.source.type_name().into());
                            node.metadata
                                .insert("autoStart".into(), source.source.auto_start().to_string());
                            projection.set_runtime(id, Box::new(source.source.clone()))?;
                        }
                        Value::Query(query) => {
                            projection
                                .set_runtime(id, Box::new(query.clone() as Arc<dyn QueryTrait>))?;
                        }
                        Value::Reaction(reaction) => {
                            node.metadata
                                .insert("kind".into(), reaction.reaction.type_name().into());
                            node.metadata.insert(
                                "autoStart".into(),
                                reaction.reaction.auto_start().to_string(),
                            );
                            projection.set_runtime(id, Box::new(reaction.reaction.clone()))?;
                        }
                    }
                    let obsolete: Vec<_> = projection
                        .snapshot()
                        .edges
                        .into_iter()
                        .filter(|edge| {
                            edge.to == id
                                && edge.relationship == RelationshipKind::Feeds
                                && !dependencies.contains(&edge.from)
                        })
                        .collect();
                    for edge in obsolete {
                        projection.remove_relationship(&edge.from, id, RelationshipKind::Feeds)?;
                    }
                    for dependency in dependencies {
                        projection.add_relationship(&dependency, id, RelationshipKind::Feeds)?;
                    }
                }
            }
            for id in retired {
                if projection.contains(&id) {
                    // Native dependency/removal policy has already decided this.
                    // A stale read-model edge must never veto committed cleanup.
                    projection.remove_component(&id)?;
                }
            }
        }
        for record in records.values() {
            let component = record.value.instance();
            let native = publication.observed.components.get(&record.node);
            let Some(native) = native else { continue };
            let mut status = if native.failure.is_some() {
                ComponentStatus::Error
            } else if native.lifecycle == ComponentLifecycle::Starting {
                ComponentStatus::Starting
            } else if native.lifecycle == ComponentLifecycle::Stopping {
                ComponentStatus::Stopping
            } else if native.operation.0 == 0 {
                record.initial_status
            } else if native.lifecycle == ComponentLifecycle::Stopped || native.exhausted {
                ComponentStatus::Stopped
            } else {
                component.status().await
            };
            let native_error = || {
                native
                    .failure
                    .as_ref()
                    .map(|failure| format!("{:#}", failure.cause))
            };
            let message = match &record.value {
                Value::Query(query) => {
                    query.publish_status(status);
                    query.last_error().or_else(native_error)
                }
                _ => native
                    .failure
                    .as_ref()
                    .map(|failure| format!("{:#}", failure.cause)),
            };
            let mut projection = self.projection.write().await;
            if projection.get_component(component.id()).is_some() {
                let current = projection
                    .get_component(component.id())
                    .expect("projected component")
                    .status;
                if current == ComponentStatus::Reconfiguring && status == ComponentStatus::Stopping
                {
                    continue;
                }
                if current == ComponentStatus::Reconfiguring && status == ComponentStatus::Added {
                    status = ComponentStatus::Stopped;
                }
                if status == ComponentStatus::Added && current != ComponentStatus::Added {
                    continue;
                }
                if (status == ComponentStatus::Running
                    && current != ComponentStatus::Running
                    && current != ComponentStatus::Starting)
                    || (status == ComponentStatus::Stopped && current == ComponentStatus::Added)
                    || (status == ComponentStatus::Error && current == ComponentStatus::Added)
                {
                    projection.apply_update(ComponentUpdate::Status {
                        component_id: component.id().to_owned(),
                        status: ComponentStatus::Starting,
                        message: None,
                    });
                }
                let current = projection
                    .get_component(component.id())
                    .expect("projected component")
                    .status;
                if status == ComponentStatus::Stopped
                    && current != ComponentStatus::Stopped
                    && current != ComponentStatus::Stopping
                    && current != ComponentStatus::Reconfiguring
                {
                    projection.apply_update(ComponentUpdate::Status {
                        component_id: component.id().to_owned(),
                        status: ComponentStatus::Stopping,
                        message: None,
                    });
                }
                projection.apply_update(ComponentUpdate::Status {
                    component_id: component.id().to_owned(),
                    status,
                    message,
                });
            }
        }
        Ok(())
    }
}

struct Projection {
    descriptor: ComponentDescriptor,
    runtime: Weak<Runtime>,
}
#[async_trait]
impl ComputationComponent for Projection {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl ComputationService for Projection {
    async fn run(&mut self) -> anyhow::Result<()> {
        let runtime = self
            .runtime
            .upgrade()
            .ok_or_else(|| anyhow::anyhow!("runtime was dropped"))?;
        let mut native = runtime.inspector()?.subscribe();
        let mut changed = runtime.changed.subscribe();
        drop(runtime);
        loop {
            if let Some(runtime) = self.runtime.upgrade() {
                runtime.project().await?;
            } else {
                return Ok(());
            }
            tokio::select! { result = native.changed() => result?, result = changed.changed() => result? }
        }
    }
}
