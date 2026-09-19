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
mod inspection;
mod projection;
mod query;
mod reaction;
mod source;
#[cfg(test)]
mod tests;

// Ordinary API declarations own their plugin/configuration records immediately;
// the graph factory is the only path that validates and realizes those records.

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
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock, Weak,
    },
};
use tokio::sync::{watch, Mutex, RwLock};

type SourceRegistration = (Box<dyn Source>, HashMap<String, String>);
type ReactionRegistration = (Box<dyn Reaction>, HashMap<String, String>);
type BootstrapRegistration = (String, String, HashMap<String, serde_json::Value>);

pub(crate) fn map_addition_error(kind: &str, id: &str, error: anyhow::Error) -> crate::DrasiError {
    let context = crate::DrasiError::operation_failed(kind, id, "add", format!("{error:#}"));
    if matches!(
        error.downcast_ref::<GraphError>(),
        Some(GraphError::AdditionRejected { .. })
    ) {
        crate::DrasiError::Internal(error.context(context))
    } else {
        context
    }
}

pub(crate) async fn build(
    config: Arc<RuntimeConfig>,
    sources: Vec<SourceRegistration>,
    reactions: Vec<ReactionRegistration>,
    bootstraps: Vec<BootstrapRegistration>,
    wal: Option<Arc<dyn crate::wal::WalProvider>>,
) -> crate::Result<DrasiLib> {
    let mut core = DrasiLib::new(config.clone());
    let setup = async {
        let mut recipes = BTreeMap::new();
        for (source, kind, properties) in bootstraps {
            anyhow::ensure!(
                source == crate::sources::COMPONENT_GRAPH_SOURCE_ID
                    || sources.iter().any(|(instance, _)| instance.id() == source),
                "bootstrap recipe references unknown source '{source}'"
            );
            anyhow::ensure!(
                recipes
                    .insert(
                        source.clone(),
                        crate::config::BootstrapSnapshot { kind, properties }
                    )
                    .is_none(),
                "duplicate bootstrap recipe for source '{source}'"
            );
        }
        *core
            .computation_registry
            .wal
            .lock()
            .map_err(|_| anyhow::anyhow!("computation WAL binding poisoned"))? = wal.clone();
        let runtime = Runtime::new(&core, wal).await?;
        core.computation_runtime = Some(runtime.clone());
        core.inspection.set_computation(runtime.clone());
        let source = crate::sources::component_graph_source::ComponentGraphSource::new(
            core.component_event_broadcast_tx.clone(),
            config.id.clone(),
            core.component_graph.clone(),
        )?;
        runtime
            .add_source_with_recipe(
                Box::new(source),
                HashMap::new(),
                recipes.remove(crate::sources::COMPONENT_GRAPH_SOURCE_ID),
                false,
            )
            .await?;
        for (source, metadata) in sources {
            let recipe = recipes.remove(source.id());
            runtime
                .add_source_with_recipe(source, metadata, recipe, false)
                .await?;
        }
        for query in &config.queries {
            runtime.add_query(query.clone(), false).await?;
        }
        for (reaction, metadata) in reactions {
            runtime.add_reaction(reaction, metadata, false).await?;
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

    fn subscription_inputs(&self) -> anyhow::Result<Vec<ComponentId>> {
        let inputs = match self {
            Self::Source(_) => Vec::new(),
            Self::Query(query) => query
                .config
                .sources
                .iter()
                .map(|source| source.source_id.clone())
                .collect(),
            Self::Reaction(reaction) => reaction.query_ids.clone(),
        };
        let own_id = self.instance().id().to_owned();
        inputs
            .into_iter()
            .map(|input| {
                anyhow::ensure!(
                    input != own_id,
                    "component '{own_id}' cannot subscribe to itself"
                );
                ComponentId::try_new(input).map_err(anyhow::Error::from)
            })
            .collect()
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
    bootstrap_recipe: Option<crate::config::BootstrapSnapshot>,
    initial_status: ComponentStatus,
    activation_requested: bool,
}

#[derive(Clone)]
struct RecordData {
    token: u64,
    node: ComponentId,
    resource: ResourceId,
    value: Value,
    metadata: HashMap<String, String>,
    bootstrap_recipe: Option<crate::config::BootstrapSnapshot>,
    initial_status: ComponentStatus,
    activation_requested: bool,
}

impl Record {
    fn data(&self) -> RecordData {
        RecordData {
            token: self.token,
            node: self.node.clone(),
            resource: self.resource.clone(),
            value: self.value.clone(),
            metadata: self.metadata.clone(),
            bootstrap_recipe: self.bootstrap_recipe.clone(),
            initial_status: self.initial_status,
            activation_requested: self.activation_requested,
        }
    }
}

impl RecordData {
    fn record(&self, owner: Arc<RuntimeInstance>) -> Record {
        Record {
            token: self.token,
            node: self.node.clone(),
            resource: self.resource.clone(),
            value: self.value.clone(),
            owner,
            metadata: self.metadata.clone(),
            bootstrap_recipe: self.bootstrap_recipe.clone(),
            initial_status: self.initial_status,
            activation_requested: self.activation_requested,
        }
    }
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

fn index_resource_id(name: &str) -> GraphResult<ResourceId> {
    let encoded: String = name.bytes().map(|byte| format!("{byte:02x}")).collect();
    Ok(ResourceId::try_new(format!("instance.index/{encoded}"))?)
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
        let services = LegacyPluginServices {
            scope: Arc::from(core.config.id.as_str()),
            state_store: Some(core.config.state_store_provider.clone()),
            identity: core.config.identity_provider.clone(),
            wal,
            secrets: core.config.secret_store_provider.clone(),
        };
        let factory = Arc::new(RuntimeFactory::new(
            services.clone(),
            core.config.index_factory.clone(),
        ));
        let runtime = Arc::new(Self {
            config: core.config.clone(),
            services,
            middleware: core.middleware_registry.clone(),
            projection: core.component_graph.clone(),
            parent: OnceLock::new(),
            records: RwLock::new(BTreeMap::new()),
            mutations: Mutex::new(()),
            projecting: Mutex::new(()),
            next_instance: AtomicU64::new(1),
            changed: watch::channel(0).0,
            catalog: QueryResultsCatalog::new("__drasi_lib_queries__")?,
            factory,
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
        let mut builder = runtime
            .services
            .declare(ComputationGraph::builder("__drasi_lib_runtime__"))?;
        for (name, provider) in runtime.config.index_factory.configured_providers() {
            // Provider names are configuration keys, not validated graph identifiers.
            let id = index_resource_id(&name)?;
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: id.clone(),
                    role: ResourceRole::IndexBackend,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from(id.as_str()),
                })?
                .provide_resource(
                    id,
                    ResourceHandle::new(ResourceRole::IndexBackend, Arc::new(provider)),
                )?;
        }
        let mut graph = builder.service(Box::new(projector)).build()?;
        // Preserve the ordinary API's instance-root namespace without letting
        // the compatibility projection decide whether an addition is accepted.
        graph.reserve_component_id(&core.config.id);
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
    pub(crate) fn control(&self) -> anyhow::Result<GraphControl> {
        Ok(self.parent()?.control())
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
        publication: &GraphRegistrySnapshot,
    ) -> anyhow::Result<BTreeMap<String, Record>> {
        let mut records = BTreeMap::new();
        for (node, specification) in &publication.desired.specifications {
            if specification.implementation != self.factory.descriptor().implementation {
                continue;
            }
            let resource = specification
                .dependencies
                .get("instance")
                .filter(|resources| resources.len() == 1)
                .and_then(|resources| resources.first())
                .ok_or_else(|| {
                    anyhow::anyhow!("native component {node} has no unique instance binding")
                })?;
            let owner = publication.resource(resource)?.get::<RuntimeInstance>()?;
            let record = owner.record()?;
            anyhow::ensure!(
                &record.node == node
                    && &record.resource == resource
                    && record_token(&publication.desired, node) == Some(record.token),
                "native component {node} does not match its graph-owned instance"
            );
            record.owner.installed.store(true, Ordering::Release);
            records.insert(record.value.instance().id().to_owned(), record);
        }
        Ok(records)
    }

    async fn current_records(&self) -> anyhow::Result<BTreeMap<String, Record>> {
        self.records_at(&self.parent()?.control().registry_snapshot())
            .await
    }

    async fn validate_registration(
        &self,
        id: &str,
        _kind: crate::component_graph::ComponentKind,
        _allow_placeholder: bool,
    ) -> anyhow::Result<()> {
        ComponentId::try_new(id)?;
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

    async fn query_sources(
        &self,
        config: &QueryConfig,
    ) -> anyhow::Result<Vec<Arc<SourceInstance>>> {
        let records = self.current_records().await?;
        let mut dependencies = Vec::new();
        for setting in &config.sources {
            let record = records
                .get(&setting.source_id)
                .ok_or_else(|| anyhow::anyhow!("Source '{}' not found", setting.source_id))?;
            let Value::Source(source) = &record.value else {
                anyhow::bail!("Component '{}' is not a Source", setting.source_id);
            };
            dependencies.push((source.clone(), self.handle_for(record)?));
        }
        for (_, handle) in &dependencies {
            handle.wait_created().await?;
        }
        Ok(dependencies.into_iter().map(|(source, _)| source).collect())
    }

    async fn reaction_queries(&self, ids: &[String]) -> anyhow::Result<()> {
        let records = self.current_records().await?;
        let mut dependencies = Vec::new();
        for id in ids {
            let record = records
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("Query '{id}' not found"))?;
            if !matches!(&record.value, Value::Query(_)) {
                anyhow::bail!("Component '{id}' is not a Query");
            }
            dependencies.push(self.handle_for(record)?);
        }
        for handle in dependencies {
            handle.wait_created().await?;
        }
        Ok(())
    }

    async fn register(
        &self,
        value: Value,
        definition: serde_json::Value,
        metadata: HashMap<String, String>,
        bootstrap_recipe: Option<crate::config::BootstrapSnapshot>,
        activate: bool,
    ) -> anyhow::Result<ComponentHandle> {
        let component = value.instance();
        let id = component.id().to_owned();
        let number = self.next_token()?;
        let node = ComponentId::try_new(id.as_str())?;
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
        let mut descriptor = ComponentDescriptor::try_new(node.clone(), vec![])?
            .with_semantic_kind(match &value {
                Value::Source(_) => ComponentSemanticKind::Source,
                Value::Query(_) => ComponentSemanticKind::Query,
                Value::Reaction(_) => ComponentSemanticKind::Reaction,
            });
        let mut validation_error = None;
        if matches!(&value, Value::Source(_) | Value::Reaction(_)) {
            if let (Some(id), Some(version)) =
                (metadata.get("pluginId"), metadata.get("pluginVersion"))
            {
                match descriptor.clone().with_plugin_identity(PluginIdentity {
                    id: Arc::from(id.as_str()),
                    version: Arc::from(version.as_str()),
                }) {
                    Ok(declaration) => descriptor = declaration,
                    Err(error) => {
                        validation_error = Some(Arc::new(GraphError::Validation {
                            component: node.clone(),
                            source: error.into(),
                        }));
                    }
                }
            }
        }
        let mut dependencies: BTreeMap<_, _> = [(Arc::from("instance"), vec![resource.clone()])]
            .into_iter()
            .chain(self.services.dependencies())
            .collect();
        self.bind_index_dependency(&value, &mut dependencies)?;
        let spec = ComponentSpecification {
            descriptor,
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
            dependencies,
        };
        let desired = DesiredComponent {
            descriptor: spec.descriptor.clone(),
            role: ComponentRole::Service,
            completion: None,
            streams: BTreeMap::new(),
            lifecycle: LifecyclePolicy {
                auto_start: value.auto_start(),
            },
            input_merge: InputMergePolicy::default(),
            construction: ComponentConstruction::Factory(spec),
        };
        let activation_requested = activate && value.auto_start();
        let record = Record {
            token: number,
            node,
            resource: resource.clone(),
            value,
            owner: instance.clone(),
            metadata,
            bootstrap_recipe,
            initial_status: ComponentStatus::Added,
            activation_requested,
        };
        record.owner.bind_record(record.data())?;
        self.records.write().await.insert(number, record.clone());
        let result = async {
            let mut bindings = TopologyBindings {
                defer_activation: !activate,
                validation_error,
                ..Default::default()
            };
            match record.value.subscription_inputs() {
                Ok(inputs) => {
                    bindings.subscriptions = inputs
                        .into_iter()
                        .map(|from| (from, record.node.clone()))
                        .collect();
                }
                Err(error) => {
                    bindings.validation_error = Some(Arc::new(GraphError::Validation {
                        component: record.node.clone(),
                        source: error,
                    }));
                }
            }
            bindings.factories.register(factory)?;
            bindings.resources.insert(
                resource.clone(),
                ResourceHandle::new(ResourceRole::Component, instance.clone())
                    .with_cleanup(instance),
            );
            let handle = control
                .add_component(ComponentAddition {
                    definition: desired,
                    resources: vec![ResourceSpecification {
                        id: resource.clone(),
                        role: ResourceRole::Component,
                        ownership: ResourceOwnership::Graph,
                        binding: Arc::from(format!("{}:{id}", component.kind())),
                    }],
                    bindings,
                })
                .await?;
            record.owner.installed.store(true, Ordering::Release);
            Ok::<_, anyhow::Error>(handle)
        }
        .await;
        self.notify();
        match result {
            Ok(handle) => Ok(handle),
            Err(error) => {
                if matches!(
                    error.downcast_ref::<GraphError>(),
                    Some(GraphError::AdditionRejected { .. })
                ) {
                    record.owner.rejection_owned.store(true, Ordering::Release);
                } else if let Err(cleanup) = self.discard_uninstalled(&record).await {
                    return Err(error.context(format!("addition cleanup also failed: {cleanup:#}")));
                }
                Err(error)
            }
        }
    }

    fn bind_index_dependency(
        &self,
        value: &Value,
        dependencies: &mut BTreeMap<Arc<str>, Vec<ResourceId>>,
    ) -> anyhow::Result<()> {
        dependencies.remove("indexes");
        if let Value::Query(query) = value {
            if let Some((name, _)) = self
                .config
                .index_factory
                .configured_provider(query.config.storage_backend.as_ref())
            {
                dependencies.insert(Arc::from("indexes"), vec![index_resource_id(&name)?]);
            }
        }
        Ok(())
    }

    async fn project_addition(&self) {
        if let Err(error) = self.project_declarations().await {
            log::error!("Could not project declared native components: {error:#}");
        }
        self.notify();
    }

    pub(crate) async fn add_source(
        self: &Arc<Self>,
        source: Box<dyn Source>,
        metadata: HashMap<String, String>,
        start: bool,
    ) -> anyhow::Result<ComponentHandle> {
        self.add_source_with_recipe(source, metadata, None, start)
            .await
    }

    async fn add_source_with_recipe(
        self: &Arc<Self>,
        source: Box<dyn Source>,
        metadata: HashMap<String, String>,
        bootstrap_recipe: Option<crate::config::BootstrapSnapshot>,
        start: bool,
    ) -> anyhow::Result<ComponentHandle> {
        let mutation = self.mutations.lock().await;
        let id = source.id().to_owned();
        let mut meta = metadata;
        meta.entry("kind".into())
            .or_insert_with(|| source.type_name().to_owned());
        meta.entry("autoStart".into())
            .or_insert_with(|| source.auto_start().to_string());
        self.validate_registration(&id, crate::component_graph::ComponentKind::Source, false)
            .await?;
        let source = SourceInstance::new(
            source,
            self.services.clone(),
            self.changed.clone(),
            self.control()?,
        );
        let handle = self
            .register(
                Value::Source(source.clone()),
                serde_json::json!({"auto_start":source.source.auto_start()}),
                meta,
                bootstrap_recipe,
                start,
            )
            .await?;
        drop(mutation);
        self.project_addition().await;
        Ok(handle)
    }

    pub(crate) async fn add_query(
        self: &Arc<Self>,
        config: QueryConfig,
        start: bool,
    ) -> anyhow::Result<ComponentHandle> {
        self.add_query_definition(config, start, false).await
    }
    pub(crate) async fn declare_query(self: &Arc<Self>, config: QueryConfig) -> anyhow::Result<()> {
        self.add_query_definition(config, false, true)
            .await
            .map(|_| ())
    }
    async fn add_query_definition(
        self: &Arc<Self>,
        config: QueryConfig,
        start: bool,
        allow_unbound: bool,
    ) -> anyhow::Result<ComponentHandle> {
        let mutation = self.mutations.lock().await;
        let id = config.id.clone();
        self.validate_registration(
            &id,
            crate::component_graph::ComponentKind::Query,
            allow_unbound,
        )
        .await?;
        let query = QueryInstance::new(config.clone(), self);
        let handle = self
            .register(
                Value::Query(query.clone()),
                serde_json::to_value(&config)?,
                HashMap::new(),
                None,
                start,
            )
            .await?;
        drop(mutation);
        self.project_addition().await;
        Ok(handle)
    }

    pub(crate) async fn add_reaction(
        self: &Arc<Self>,
        reaction: Box<dyn Reaction>,
        metadata: HashMap<String, String>,
        start: bool,
    ) -> anyhow::Result<ComponentHandle> {
        self.add_reaction_definition(reaction, metadata, start)
            .await
    }
    pub(crate) async fn declare_reaction(
        self: &Arc<Self>,
        reaction: Box<dyn Reaction>,
    ) -> anyhow::Result<()> {
        self.add_reaction_definition(reaction, HashMap::new(), false)
            .await
            .map(|_| ())
    }
    async fn add_reaction_definition(
        self: &Arc<Self>,
        reaction: Box<dyn Reaction>,
        metadata: HashMap<String, String>,
        start: bool,
    ) -> anyhow::Result<ComponentHandle> {
        let mutation = self.mutations.lock().await;
        let id = reaction.id().to_owned();
        let ids = reaction.query_ids();
        let mut metadata = metadata;
        metadata
            .entry("kind".into())
            .or_insert_with(|| reaction.type_name().to_owned());
        metadata
            .entry("autoStart".into())
            .or_insert_with(|| reaction.auto_start().to_string());
        self.validate_registration(&id, crate::component_graph::ComponentKind::Reaction, false)
            .await?;
        let reaction = ReactionInstance::new(reaction, self);
        let auto_start = reaction.reaction.auto_start();
        let handle = self
            .register(
                Value::Reaction(reaction.clone()),
                serde_json::json!({"queries":ids,"auto_start":auto_start}),
                metadata,
                None,
                start,
            )
            .await?;
        drop(mutation);
        self.project_addition().await;
        Ok(handle)
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

    fn needs_resume(&self, record: &Record) -> anyhow::Result<bool> {
        let (control, _, observed) = self.control_for(record)?;
        Ok(self.active(record)?
            || matches!(
                observed.realization,
                RealizationState::Pending | RealizationState::Creating
            ) && record.activation_requested
                && control
                    .desired_snapshot()
                    .lifecycle_policies
                    .get(&record.node)
                    .is_some_and(|policy| policy.auto_start))
    }

    fn handle_for(&self, record: &Record) -> anyhow::Result<ComponentHandle> {
        let (control, _, observed) = self.control_for(record)?;
        let handle = control.component_handle(&record.node)?;
        if handle.generation() != observed.generation {
            return Err(GraphError::StaleGeneration.into());
        }
        Ok(handle)
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
        let handle = self.handle_for(record)?;
        handle.wait_created().await?;
        let (_, _, observed) = self.control_for(record)?;
        if observed.generation != handle.generation() {
            return Err(GraphError::StaleGeneration.into());
        }
        // Instance startup must reach the source subscription fence before a
        // query's bootstrap can confirm readiness. Handles wait for that later.
        let result = handle.start_requested().await;
        self.project().await?;
        let report = result?;
        handle.observed()?;
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
                if query.has_stale_sources(&sources) {
                    let configuration = query.config.clone();
                    let replacement = QueryInstance::new(configuration.clone(), self);
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
        self.start_record(&record).await?;
        self.resume_records(resume).await
    }
    async fn stop_record(&self, record: &Record) -> anyhow::Result<()> {
        let result = self.handle_for(record)?.stop().await;
        self.project().await?;
        result?;
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
            && !matches!(
                observed.realization,
                RealizationState::Pending | RealizationState::Creating
            )
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
        let record = self.record(id, kind).await?;
        let dependents: Vec<_> = self
            .control()?
            .desired_snapshot()
            .data_dependents(&record.node)
            .into_iter()
            .map(|id| id.to_string())
            .collect();
        if !dependents.is_empty() {
            anyhow::bail!("Depended on by: {}", dependents.join(", "));
        }
        let (_, _, observed) = self.control_for(&record)?;
        if matches!(
            observed.realization,
            RealizationState::Pending | RealizationState::Creating
        ) || observed.realization == RealizationState::Created
            && (self.active(&record)? || observed.failure.is_some())
        {
            self.stop_record(&record).await?;
        }
        let (control, revision, _) = self.control_for(&record)?;
        let preview = control
            .preview(
                revision,
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
        self.records.write().await.remove(&record.token);
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
        let subscription_inputs = value.subscription_inputs()?;
        let component = value.instance();
        let id = component.id().to_owned();
        let (_, _, observed) = self.control_for(&old)?;
        if matches!(
            observed.realization,
            RealizationState::Pending | RealizationState::Creating
        ) || observed.realization == RealizationState::Created
            && (self.active(&old)? || observed.failure.is_some())
        {
            self.stop_record(&old).await?;
        }
        {
            let mut projection = self.projection.write().await;
            if projection.get_component(&id).is_some() {
                if let Err(error) = projection.project_status(
                    &id,
                    ComponentStatus::Reconfiguring,
                    Some(format!("Reconfiguring {}", component.kind())),
                ) {
                    log::warn!("Could not project reconfiguration of {id}: {error:#}");
                }
            }
        }
        let (control, revision, _) = self.control_for(&old)?;
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
        desired.lifecycle.auto_start = value.auto_start();
        spec.configuration.insert(
            Arc::from("definition"),
            ConfigurationValue::Literal(definition),
        );
        spec.configuration.insert(
            Arc::from("record_token"),
            ConfigurationValue::Literal(token.into()),
        );
        self.bind_index_dependency(&value, &mut spec.dependencies)?;
        let new = Record {
            token,
            node: old.node.clone(),
            resource: old.resource.clone(),
            value,
            owner,
            metadata: old.metadata.clone(),
            bootstrap_recipe: old.bootstrap_recipe.clone(),
            initial_status: ComponentStatus::Stopped,
            activation_requested: false,
        };
        new.owner.bind_record(new.data())?;
        // Retain both candidates before submission. The controller's committed
        // token selects the current record even if the caller drops this await.
        self.records.write().await.insert(token, new.clone());
        let result = async {
            let preview = control
                .preview(
                    revision,
                    vec![
                        DesiredMutation::ReplaceComponent(desired),
                        DesiredMutation::RebindResource(old.resource.clone()),
                        DesiredMutation::SetSubscriptions {
                            consumer: old.node.clone(),
                            producers: subscription_inputs,
                        },
                    ],
                )
                .await?;
            let mut bindings = TopologyBindings {
                defer_activation: true,
                ..Default::default()
            };
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
        let desired = self.control()?.desired_snapshot();
        let queries: BTreeSet<_> = query_ids
            .iter()
            .map(|id| ComponentId::try_new(id.as_str()))
            .collect::<std::result::Result<_, _>>()?;
        let dependents: BTreeSet<_> = queries
            .iter()
            .flat_map(|id| desired.data_dependents(id))
            .collect();
        let mut paused = Vec::new();
        for kind in ["reaction", "query"] {
            for record in records.values() {
                let affected = record.value.instance().kind() == kind
                    && if kind == "query" {
                        queries.contains(&record.node)
                    } else {
                        dependents.contains(&record.node)
                    };
                if affected && self.needs_resume(record)? {
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
        let old = self.record(id, "source").await?;
        if source.id() != id {
            anyhow::bail!(
                "New source ID '{}' does not match existing source ID '{id}'",
                source.id()
            );
        }
        let records = self.current_records().await?;
        let affected: Vec<_> = self
            .control()?
            .desired_snapshot()
            .data_dependents(&old.node)
            .into_iter()
            .filter(|id| {
                records
                    .get(id.as_str())
                    .is_some_and(|record| matches!(&record.value, Value::Query(_)))
            })
            .map(|id| id.to_string())
            .collect();
        let mut resume = self.pause_consumers(&affected).await?;
        if self.needs_resume(&old)? {
            resume.push(old.clone());
        }
        let source = SourceInstance::new(
            source,
            self.services.clone(),
            self.changed.clone(),
            self.control()?,
        );
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
            let query = QueryInstance::new(config.clone(), self);
            self.replace_record(record, Value::Query(query), serde_json::to_value(config)?)
                .await?;
        }
        self.resume_records(resume).await
    }
    pub(crate) async fn update_query(
        self: &Arc<Self>,
        id: &str,
        config: QueryConfig,
    ) -> anyhow::Result<()> {
        let old = self.record(id, "query").await?;
        if config.id != id {
            anyhow::bail!(
                "New query ID '{}' does not match existing query ID '{id}'",
                config.id
            );
        }
        let query = QueryInstance::new(config.clone(), self);
        let resume = self.pause_consumers(&[id.to_owned()]).await?;
        self.replace_record(old, Value::Query(query), serde_json::to_value(config)?)
            .await?;
        self.resume_records(resume).await
    }
    pub(crate) async fn update_reaction(
        self: &Arc<Self>,
        id: &str,
        reaction: Box<dyn Reaction>,
    ) -> anyhow::Result<()> {
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
        let restart = self.needs_resume(&old)?;
        let reaction = ReactionInstance::new(reaction, self);
        let new = self
            .replace_record(
                old,
                Value::Reaction(reaction.clone()),
                serde_json::json!({"queries":ids,"auto_start":reaction.reaction.auto_start()}),
            )
            .await?;
        if restart {
            self.start_record(&new).await?;
        }
        Ok(())
    }
    pub(crate) async fn start_kind(self: &Arc<Self>, kind: &str) -> anyhow::Result<()> {
        let publication = self.control()?.registry_snapshot();
        let records: Vec<_> = self
            .records_at(&publication)
            .await?
            .values()
            .filter(|record| {
                record.value.instance().kind() == kind
                    && publication.desired.lifecycle_policies[&record.node].auto_start
            })
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
    async fn native_component_ids(
        &self,
        publication: &GraphRegistrySnapshot,
    ) -> anyhow::Result<Vec<ComponentId>> {
        let ordinary: BTreeSet<_> = self
            .records_at(publication)
            .await?
            .into_values()
            .map(|record| record.node)
            .collect();
        Ok(publication
            .desired
            .nodes
            .iter()
            .map(|node| node.descriptor.id())
            .filter(|id| id.as_str() != "__inspection_projection__" && !ordinary.contains(*id))
            .cloned()
            .collect())
    }
    pub(crate) async fn start_native_components(&self) -> anyhow::Result<()> {
        let control = self.control()?;
        let publication = control.registry_snapshot();
        let desired = &publication.desired;
        let selected: Vec<_> = self
            .native_component_ids(&publication)
            .await?
            .into_iter()
            .filter(|id| {
                desired
                    .lifecycle_policies
                    .get(id)
                    .is_some_and(|policy| policy.auto_start)
            })
            .collect();
        let mut ready = Vec::new();
        let mut failures = Vec::new();
        for id in selected {
            let handle = control.component_handle(&id)?;
            let mut changes = control.subscribe_observed();
            let settled = tokio::select! {
                result = changes.wait_for(|state| {
                    state.components.get(&id).map_or(true, |node| {
                        node.generation != handle.generation()
                            || !matches!(
                                node.realization,
                                RealizationState::Pending | RealizationState::Creating
                            )
                    })
                }) => result.map(|_| ()).map_err(anyhow::Error::from),
                result = handle.wait_created() => result.map_err(anyhow::Error::from),
            };
            if let Err(error) = settled {
                failures.push(format!("{id}: {error:#}"));
                continue;
            }
            let observed = match handle.observed() {
                Ok(observed) => observed,
                Err(error) => {
                    failures.push(format!("{id}: {error}"));
                    continue;
                }
            };
            if let Some(failure) = observed.failure {
                failures.push(format!("{id}: {:#}", failure.cause));
            } else if observed.realization == RealizationState::Created {
                ready.push(handle);
            } else {
                failures.push(format!("{id}: creation is {:?}", observed.realization));
            }
        }
        if !ready.is_empty() {
            let revision = control.desired_snapshot().revision;
            for handle in &ready {
                handle.observed()?;
            }
            let report = control
                .start_requested(
                    revision,
                    GraphSelection::Exact(
                        ready
                            .into_iter()
                            .map(|handle| handle.id().clone())
                            .collect(),
                    ),
                )
                .await?;
            if report.summary != OperationSummary::Completed {
                failures.push(format!("native activation failed: {:?}", report.components));
            }
        }
        if !failures.is_empty() {
            anyhow::bail!("native component startup failed: {}", failures.join("; "));
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
        let control = self.control()?;
        let publication = control.registry_snapshot();
        let desired = &publication.desired;
        let native = self.native_component_ids(&publication).await?;
        if !native.is_empty() {
            match control
                .stop_components(desired.revision, GraphSelection::Exact(native))
                .await
            {
                Ok(report) if report.summary == OperationSummary::Completed => {}
                Ok(report) => failures.push(format!("native stop failed: {:?}", report.components)),
                Err(error) => failures.push(error.to_string()),
            }
        }
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
                if matches!(
                    observed.realization,
                    RealizationState::Pending | RealizationState::Creating
                ) || observed.realization == RealizationState::Created
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
            // Rejected additions belong to the graph's rejection owner, or to
            // the caller after take(). Resubmission makes the record installed.
            if record.owner.installed.load(Ordering::Acquire)
                || !record.owner.rejection_owned.load(Ordering::Acquire)
            {
                record.owner.shutdown().await?;
            }
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
    pub(crate) async fn query(&self, id: &str) -> anyhow::Result<Arc<QueryInstance>> {
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
    async fn project_declarations(&self) -> anyhow::Result<()> {
        let _projecting = self.projecting.lock().await;
        let publication = self.control()?.registry_snapshot();
        let records = self.records_at(&publication).await?;
        self.project_records(&records, &publication.desired).await
    }

    async fn project_records(
        &self,
        records: &BTreeMap<String, Record>,
        desired: &GraphSnapshot,
    ) -> anyhow::Result<()> {
        {
            use crate::component_graph::RelationshipKind;
            let mut projection = self.projection.write().await;
            let retired: Vec<_> = projection
                .snapshot()
                .nodes
                .into_iter()
                .filter(|node| {
                    matches!(
                        node.kind,
                        crate::component_graph::ComponentKind::Source
                            | crate::component_graph::ComponentKind::Query
                            | crate::component_graph::ComponentKind::Reaction
                    ) && !records.contains_key(&node.id)
                        && !node
                            .metadata
                            .get("unboundReference")
                            .is_some_and(|value| value == "true")
                })
                .map(|node| node.id)
                .collect();
            for kind in ["source", "query", "reaction"] {
                for record in records
                    .values()
                    .filter(|record| record.value.instance().kind() == kind)
                {
                    let component = record.value.instance();
                    let id = component.id();
                    let dependencies: Vec<_> = desired
                        .subscriptions
                        .iter()
                        .filter(|(_, to)| to == &record.node)
                        .map(|(from, _)| from.as_str().to_owned())
                        .collect();
                    let expected_kind = match kind {
                        "source" => crate::component_graph::ComponentKind::Source,
                        "query" => crate::component_graph::ComponentKind::Query,
                        _ => crate::component_graph::ComponentKind::Reaction,
                    };
                    if projection.get_component(id).is_some_and(|node| {
                        node.kind != expected_kind
                            || node
                                .metadata
                                .get("unboundReference")
                                .is_some_and(|value| value == "true")
                    }) {
                        projection.remove_component(id)?;
                    }
                    if !projection.contains(id) {
                        match kind {
                            "source" => projection.register_source(id, record.metadata.clone())?,
                            "query" => {
                                projection.register_query(id, record.metadata.clone(), &[])?
                            }
                            _ => projection.register_reaction(id, record.metadata.clone(), &[])?,
                        }
                    }
                    let node = projection
                        .get_component_mut(id)
                        .expect("registered projection");
                    node.metadata = record.metadata.clone();
                    node.metadata.insert(
                        "autoStart".into(),
                        desired.lifecycle_policies[&record.node]
                            .auto_start
                            .to_string(),
                    );
                    match &record.value {
                        Value::Source(source) => {
                            node.metadata
                                .insert("kind".into(), source.source.type_name().into());
                            projection.set_runtime(id, Box::new(source.source.clone()))?;
                        }
                        Value::Query(query) => {
                            projection
                                .set_runtime(id, Box::new(query.clone() as Arc<dyn QueryTrait>))?;
                        }
                        Value::Reaction(reaction) => {
                            node.metadata
                                .insert("kind".into(), reaction.reaction.type_name().into());
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
                        let expected = if kind == "query" {
                            crate::component_graph::ComponentKind::Source
                        } else {
                            crate::component_graph::ComponentKind::Query
                        };
                        if projection
                            .get_component(&dependency)
                            .is_some_and(|node| node.kind == expected)
                        {
                            projection.add_relationship(
                                &dependency,
                                id,
                                RelationshipKind::Feeds,
                            )?;
                        }
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
            if let Err(error) = self.project_provider_metadata(&mut projection, records) {
                log::warn!("Could not project provider configuration metadata: {error:#}");
            }
        }
        Ok(())
    }

    async fn project(&self) -> anyhow::Result<()> {
        let (publication, records) = {
            let _projecting = self.projecting.lock().await;
            let publication = self.control()?.registry_snapshot();
            let records = self.records_at(&publication).await?;
            self.project_records(&records, &publication.desired).await?;
            (publication, records)
        };
        for record in records.values() {
            let component = record.value.instance();
            let native = publication.observed.components.get(&record.node);
            let Some(native) = native else { continue };
            let mut status = inspection::status(record, native);
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
            let _projecting = self.projecting.lock().await;
            let current = self.control()?.registry_snapshot();
            if record_token(&current.desired, &record.node) != Some(record.token)
                || current
                    .observed
                    .components
                    .get(&record.node)
                    .map_or(true, |value| {
                        value.generation != native.generation || value.operation != native.operation
                    })
            {
                continue;
            }
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
                if let Err(error) = runtime.project().await {
                    if !matches!(
                        error.downcast_ref::<GraphError>(),
                        Some(GraphError::StaleGeneration)
                    ) {
                        return Err(error);
                    }
                    log::debug!(
                        "Computation projection publication was superseded; awaiting current state"
                    );
                }
            } else {
                return Ok(());
            }
            tokio::select! { result = native.changed() => result?, result = changed.changed() => result? }
        }
    }
}
