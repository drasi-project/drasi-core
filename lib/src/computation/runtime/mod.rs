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
mod construction;
mod events;
#[cfg(test)]
mod group_lifecycle_tests;
mod inspection;
mod local;
pub(crate) mod managed;
pub(crate) use local::{LocalConstructor, LocalDefinition};
pub(crate) mod native_query;
mod query_assembly;
mod reaction;
mod reaction_creation;
mod snapshot;
mod source;
mod source_creation;
pub(crate) use reaction_creation::ReactionConstructor;
#[cfg(test)]
mod tests;

// Ordinary API declarations own their plugin/configuration records immediately;
// the graph factory is the only path that validates and realizes those records.

use super::instance::ComputationHandle;
use super::v1::*;
use crate::{
    config::{AssignedQueryConfig as QueryConfig, RuntimeConfig},
    metrics::{LifecycleMetrics, ReactionMetricsSnapshot},
    queries::Query as QueryTrait,
    ComponentStatus, DrasiLib, Reaction, Source,
};
pub(crate) use component::RuntimeInstance as RuntimeDeclaration;
use component::{RuntimeComponent, RuntimeFactory, RuntimeInstance};
pub(crate) use native_query::NativeQuery;
use reaction::ReactionInstance;
use source::SourceInstance;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, OnceLock, Weak,
    },
};
use tokio::sync::{watch, Mutex};

type SourceRegistration = (String, Box<dyn Source>, HashMap<String, String>);
type ReactionRegistration = (String, Box<dyn Reaction>, HashMap<String, String>);
pub(crate) type NamedReactionRegistration = (
    String,
    Option<ComponentId>,
    Vec<crate::config::ComponentReference>,
    bool,
    reaction_creation::ReactionConstructor,
);
pub(crate) type AsyncSourceRegistration = (
    String,
    Option<ComponentId>,
    bool,
    source_creation::SourceConstructor,
);
type BootstrapRegistration = (String, String, HashMap<String, serde_json::Value>);

pub(crate) struct OrdinaryDeclarations {
    pub(crate) sources: Vec<SourceRegistration>,
    pub(crate) async_sources: Vec<AsyncSourceRegistration>,
    pub(crate) reactions: Vec<ReactionRegistration>,
    pub(crate) reaction_factories: Vec<NamedReactionRegistration>,
    pub(crate) bootstraps: Vec<BootstrapRegistration>,
    pub(crate) native: Vec<Arc<super::instance::PendingComponents>>,
    pub(crate) native_factories: Vec<super::components::ComponentBatchDeclaration>,
}

pub(crate) fn map_addition_error(kind: &str, id: &str, error: anyhow::Error) -> crate::DrasiError {
    let context = crate::DrasiError::operation_failed(kind, id, "add", format!("{error:#}"));
    context.with_cause(error)
}

pub(crate) async fn build(
    config: Arc<RuntimeConfig>,
    declarations: OrdinaryDeclarations,
    wal: Option<Arc<dyn crate::wal::WalProvider>>,
    query_factory: Option<Arc<dyn ComponentFactory>>,
    management: crate::management::ManagementOptions,
    #[cfg(feature = "test-support")] test_controls: BTreeMap<
        String,
        crate::test_support::QueryTestControl,
    >,
) -> crate::Result<DrasiLib> {
    let OrdinaryDeclarations {
        sources,
        async_sources,
        reactions,
        reaction_factories,
        bootstraps,
        native,
        native_factories,
    } = declarations;
    let mut core = DrasiLib::new_with_wal(config.clone(), wal);
    for batch in &native {
        core.instance_graph.retain_pending(batch.clone());
    }
    if let Some(factory) = query_factory {
        core.register_query_factory(factory)?;
    }
    #[cfg(feature = "test-support")]
    core.computation_runtime
        .test_controls
        .set(test_controls)
        .map_err(|_| crate::DrasiError::invalid_state("test controls already configured"))?;
    let setup = async {
        let mut recipes = BTreeMap::new();
        for (source, kind, properties) in bootstraps {
            anyhow::ensure!(
                sources.iter().any(|(name, _, _)| name == &source)
                    || async_sources.iter().any(|(name, _, _, _)| name == &source),
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
        core.initialize_with_management(management).await?;
        for batch in &native {
            batch.attach_control(core.instance_graph.get().await?.control());
        }
        let mut definitions = Vec::new();
        for (name, source, metadata) in sources {
            let recipe = recipes.remove(&name);
            definitions.push(LocalDefinition {
                name: ComponentName::try_new(name)?,
                id: Some(ComponentId::try_new(source.id())?),
                auto_start: source.auto_start(),
                inputs: Vec::new(),
                constructor: LocalConstructor::SourceObject(source),
                metadata,
                bootstrap: recipe,
            });
        }
        for (name, requested_id, auto_start, constructor) in async_sources {
            let bootstrap = recipes.remove(&name);
            definitions.push(LocalDefinition {
                name: ComponentName::try_new(name)?,
                id: requested_id,
                auto_start,
                inputs: Vec::new(),
                constructor: LocalConstructor::Source(constructor),
                metadata: HashMap::new(),
                bootstrap,
            });
        }
        for (name, reaction, metadata) in reactions {
            let inputs = reaction
                .query_ids()
                .into_iter()
                .map(|id| {
                    Ok(crate::config::ComponentReference::by_id(
                        ComponentId::try_new(id)?,
                    ))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            definitions.push(LocalDefinition {
                name: ComponentName::try_new(name)?,
                id: Some(ComponentId::try_new(reaction.id())?),
                auto_start: reaction.auto_start(),
                inputs,
                constructor: LocalConstructor::ReactionObject(reaction),
                metadata,
                bootstrap: None,
            });
        }
        for (name, requested_id, inputs, auto_start, factory) in reaction_factories {
            definitions.push(LocalDefinition {
                name: ComponentName::try_new(name)?,
                id: requested_id,
                auto_start,
                inputs,
                constructor: LocalConstructor::Reaction(factory),
                metadata: HashMap::new(),
                bootstrap: None,
            });
        }
        if !definitions.is_empty()
            || !config.queries.is_empty()
            || !native.is_empty()
            || !native_factories.is_empty()
        {
            let owner = core
                .management
                .get()
                .ok_or_else(|| anyhow::anyhow!("configuration owner is unavailable"))?;
            let receipt = owner
                .accept_auto(crate::management::AuthoringOperation::Local {
                    definitions,
                    queries: config.queries.clone(),
                    native,
                    native_factories,
                })
                .await?;
            if let Some(creation) = receipt.creation {
                for component in creation.components {
                    owner.wait_declared(&component.id).await?;
                }
            }
            for record in &owner.configuration()?.desired.ordinary {
                if matches!(
                    record.component,
                    crate::management::OrdinaryComponent::Query { .. }
                ) {
                    for input in record.references()? {
                        if let Some(id) = input.as_id() {
                            if let Ok(source) =
                                core.computation_runtime.source_host(id.as_str()).await
                            {
                                if let Err(error) = source.wait_prepared().await {
                                    log::warn!(
                                        "Initial source {id} construction failed: {error:#}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
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
    Reaction(Arc<ReactionInstance>),
}
impl Value {
    fn instance(&self) -> Arc<dyn RuntimeComponent> {
        match self {
            Self::Source(value) => value.clone(),
            Self::Reaction(value) => value.clone(),
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

/// Whether a component may still hold resources or process data, so stopping
/// the instance, or removing or replacing its record, must stop it first.
fn needs_stop(observed: &ObservedComponent) -> bool {
    match observed.realization {
        RealizationState::Pending | RealizationState::Creating => true,
        RealizationState::Created => is_active(observed) || observed.failure.is_some(),
        _ => false,
    }
}

/// Rejects an explicit stop of a component that has nothing to stop.
///
/// A failed component is stoppable, so the stop can release what it retains
/// and acknowledge the failure.
fn ensure_stoppable(kind: &str, id: &str, observed: &ObservedComponent) -> anyhow::Result<()> {
    let stoppable = matches!(
        observed.lifecycle,
        ComponentLifecycle::Running | ComponentLifecycle::Starting | ComponentLifecycle::Failed
    ) || observed.failure.is_some()
        || !observed.waiting_for.is_empty()
        || matches!(
            observed.realization,
            RealizationState::Pending | RealizationState::Creating
        );
    match observed.lifecycle {
        _ if stoppable => Ok(()),
        ComponentLifecycle::Stopped => {
            anyhow::bail!("Cannot stop {kind} '{id}': it is already stopped")
        }
        ComponentLifecycle::Stopping => anyhow::bail!("The {kind} '{id}' is already stopping"),
        lifecycle => anyhow::bail!("Cannot stop {kind} '{id}' while it is {lifecycle:?}"),
    }
}

fn is_active(observed: &ObservedComponent) -> bool {
    matches!(
        observed.lifecycle,
        ComponentLifecycle::Starting | ComponentLifecycle::Running
    ) && !observed.exhausted
}

fn accept_waiting_start(report: StartReport, observed: &ObservedGraph) -> anyhow::Result<()> {
    if report.summary == OperationSummary::Completed {
        return Ok(());
    }
    let waiting = report.summary == OperationSummary::CompletedWithFailures
        && !report.components.is_empty()
        && report.components.iter().all(|(id, outcome)| match outcome {
            StartOutcome::StartFailed(_) => false,
            StartOutcome::NotCreated | StartOutcome::Blocked { .. } => observed
                .components
                .get(id)
                .is_some_and(|node| node.failure.is_none() && !node.waiting_for.is_empty()),
            _ => true,
        });
    if waiting {
        Ok(())
    } else {
        Err(LifecycleReportError::Start(report).into())
    }
}

/// Whether `error` reports that a concurrent configuration change replaced or
/// removed the record an operation was given, so the change, not the caller,
/// now owns that component's lifecycle.
fn superseded(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<GraphError>(),
        Some(GraphError::StaleGeneration)
    )
}

fn index_resource_id(name: &str) -> GraphResult<ResourceId> {
    let encoded: String = name.bytes().map(|byte| format!("{byte:02x}")).collect();
    Ok(ResourceId::try_new(format!("instance.index/{encoded}"))?)
}

pub(crate) struct Runtime {
    #[cfg(feature = "test-support")]
    test_controls: OnceLock<BTreeMap<String, crate::test_support::QueryTestControl>>,
    config: Arc<RuntimeConfig>,
    services: LegacyPluginServices,
    middleware: Arc<drasi_core::middleware::MiddlewareTypeRegistry>,
    query_factory: OnceLock<Arc<dyn ComponentFactory>>,
    events: Arc<events::Events>,
    parent: OnceLock<ComputationHandle>,
    instance_owner: OnceLock<Weak<super::instance::InstanceGraph>>,
    configuration_owner: OnceLock<Weak<OnceLock<crate::management::Management>>>,
    mutations: Mutex<()>,
    next_instance: AtomicU64,
    changed: watch::Sender<u64>,
    catalog: QueryResultsCatalog,
    factory: Arc<RuntimeFactory>,
    logs: Arc<crate::observability::ComponentLogRegistry>,
    pub(crate) lifecycle_metrics: Arc<LifecycleMetrics>,
}

impl Runtime {
    pub(crate) fn public_control(self: &Arc<Self>) -> anyhow::Result<GraphControl> {
        let owner = Arc::downgrade(self);
        let rename_owner = owner.clone();
        Ok(self
            .control()?
            .with_dependency_refresh(move || {
                let owner = owner.clone();
                Box::pin(async move {
                    let owner = owner.upgrade().ok_or(GraphError::ControllerClosed)?;
                    owner
                        .refresh_pending_query_inputs()
                        .await
                        .map_err(|source| GraphError::ReferenceResolution { source })
                })
            })
            .with_name_editor(move |id, generation, name| {
                let owner = rename_owner.clone();
                Box::pin(async move {
                    owner
                        .upgrade()
                        .ok_or(GraphError::ControllerClosed)?
                        .rename_owned_at(id, generation, name)
                        .await
                        .map_err(|source| GraphError::ReferenceResolution { source })
                })
            }))
    }

    pub(crate) fn public_handle(
        self: &Arc<Self>,
        handle: ComponentHandle,
    ) -> anyhow::Result<ComponentHandle> {
        Ok(handle.with_control(self.public_control()?))
    }

    pub(crate) fn new(
        config: Arc<RuntimeConfig>,
        middleware: Arc<drasi_core::middleware::MiddlewareTypeRegistry>,
        logs: Arc<crate::observability::ComponentLogRegistry>,
        wal: Option<Arc<dyn crate::wal::WalProvider>>,
    ) -> Arc<Self> {
        let services = LegacyPluginServices {
            scope: Arc::from(config.id.as_str()),
            state_store: Some(config.state_store_provider.clone()),
            identity: config.identity_provider.clone(),
            wal,
            secrets: config.secret_store_provider.clone(),
        };
        let factory = Arc::new(RuntimeFactory::new(services.clone()));
        Arc::new(Self {
            #[cfg(feature = "test-support")]
            test_controls: OnceLock::new(),
            config,
            services,
            middleware,
            query_factory: OnceLock::new(),
            events: events::Events::new(),
            parent: OnceLock::new(),
            instance_owner: OnceLock::new(),
            configuration_owner: OnceLock::new(),
            mutations: Mutex::new(()),
            next_instance: AtomicU64::new(1),
            changed: watch::channel(0).0,
            catalog: QueryResultsCatalog::new("__drasi_lib_queries__").expect("runtime catalog ID"),
            factory,
            logs,
            lifecycle_metrics: Arc::new(LifecycleMetrics::new()),
        })
    }

    pub(crate) fn event_sender(&self) -> crate::channels::ComponentEventBroadcastSender {
        self.events.broadcast.clone()
    }

    pub(crate) async fn initialize(
        self: &Arc<Self>,
        graph_owner: &Arc<super::instance::InstanceGraph>,
    ) -> anyhow::Result<()> {
        let mut builder = self.services.declare(
            ComputationGraph::builder("__drasi_lib_runtime__")
                .activation_timeout(std::time::Duration::from_secs(
                    self.config
                        .activation_timeout_secs
                        .unwrap_or(DEFAULT_ACTIVATION_TIMEOUT.as_secs()),
                ))
                .startup_timeout(std::time::Duration::from_secs(
                    self.config
                        .startup_timeout_secs
                        .unwrap_or(DEFAULT_STARTUP_TIMEOUT.as_secs()),
                ))
                .shutdown_timeout(std::time::Duration::from_secs(
                    self.config
                        .shutdown_timeout_secs
                        .unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT.as_secs()),
                ))
                .construction_timeout(std::time::Duration::from_secs(
                    self.config
                        .construction_timeout_secs
                        .unwrap_or(DEFAULT_CONSTRUCTION_TIMEOUT.as_secs()),
                ))
                .resource_construction_timeout(std::time::Duration::from_secs(
                    self.config
                        .resource_construction_timeout_secs
                        .unwrap_or(DEFAULT_RESOURCE_CONSTRUCTION_TIMEOUT.as_secs()),
                ))
                .cleanup_timeout(std::time::Duration::from_secs(
                    self.config
                        .cleanup_timeout_secs
                        .unwrap_or(DEFAULT_CLEANUP_TIMEOUT.as_secs()),
                )),
        )?;
        for (name, provider) in self.config.index_factory.configured_providers() {
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
        let mut graph = builder.build()?;
        graph.set_component_identity_policy(self.config.allow_external_component_ids);
        let observer: Arc<dyn PublicationObserver> = self.events.clone();
        graph.inspector().observe(&observer);
        graph.reserve_component_id(&self.config.id);
        let handle = graph_owner.initialize(graph).await?;
        self.parent
            .set(handle.clone())
            .map_err(|_| anyhow::anyhow!("native runtime already initialized"))?;
        self.instance_owner
            .set(Arc::downgrade(graph_owner))
            .map_err(|_| anyhow::anyhow!("instance ownership was already attached"))?;
        handle.start().await?;
        Ok(())
    }
    fn parent(&self) -> anyhow::Result<ComputationHandle> {
        self.parent
            .get()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("native runtime is not initialized"))
    }
    pub(crate) fn plugin_services(&self) -> LegacyPluginServices {
        self.services.clone()
    }

    pub(crate) fn query_factory(&self) -> Option<Arc<dyn ComponentFactory>> {
        self.query_factory.get().cloned()
    }

    pub(crate) fn register_query_factory(
        &self,
        factory: Arc<dyn ComponentFactory>,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            factory.descriptor().role == ComponentRole::Transformer
                && factory.query_authoring().is_some(),
            "selected factory does not support ordinary query authoring"
        );
        if let Err(requested) = self.query_factory.set(factory) {
            anyhow::ensure!(self.query_factory.get().is_some_and(|current| Arc::ptr_eq(current, &requested)),
                "query factory is already registered; create a new instance to select another implementation");
        }
        Ok(())
    }

    pub(crate) async fn bind_batch_source_dependencies(
        &self,
        batch: &mut ComponentBatch,
    ) -> anyhow::Result<()> {
        let mut subscriptions = BTreeSet::new();
        let mut records = None;
        let publication = self.control()?.registry_snapshot();
        for node in &batch.definition.components {
            let ComponentConstruction::Factory(spec) = &node.construction else {
                continue;
            };
            let factory = batch
                .bindings
                .factories
                .get(&spec.implementation)
                .cloned()
                .or_else(|| publication.factory(&spec.implementation));
            if factory.is_none_or(|factory| factory.query_authoring().is_none()) {
                continue;
            }
            let config = match spec.configuration.get("query_config") {
                Some(ConfigurationValue::Literal(value)) => {
                    match serde_json::from_value::<QueryConfig>(value.clone()) {
                        Ok(config) => config,
                        Err(error) => {
                            log::debug!(
                                "Query '{}' configuration is pending component validation: {error}",
                                node.descriptor.id()
                            );
                            continue;
                        }
                    }
                }
                _ => continue,
            };
            for source in config.inputs {
                let Some(producer) = source.component.as_id() else {
                    continue;
                };
                if batch
                    .definition
                    .relationships
                    .iter()
                    .chain(batch.definition.boundary_relationships.iter())
                    .any(|relationship| {
                        &relationship.definition.from.component == producer
                            && relationship.definition.to.component == *node.descriptor.id()
                    })
                {
                    continue;
                }
                if !spec.dependencies.contains_key("declaration") {
                    let resource =
                        ComputationPipelineBuilder::source_resource_id(producer.as_str())?;
                    let Some(binding) = batch.bindings.resources.get(&resource) else {
                        continue;
                    };
                    let host = binding.get::<SourcePluginHost>()?;
                    if host.is_owned() {
                        continue;
                    }
                    if records.is_none() {
                        records = Some(self.current_records().await?);
                    }
                    let Some(record) = records
                        .as_ref()
                        .expect("source records were loaded")
                        .get(producer.as_str())
                    else {
                        continue;
                    };
                    let Value::Source(owner) = &record.value else {
                        continue;
                    };
                    // A resource-owned or external host has no independent source node.
                    if !Arc::ptr_eq(&host, &owner.borrowed)
                        && !Arc::ptr_eq(&host.source()?, &owner.borrowed.source()?)
                    {
                        continue;
                    }
                }
                let producer = producer.clone();
                subscriptions.insert((producer.clone(), node.descriptor.id().clone()));
                let reader = ComputationPipelineBuilder::source_component_id(
                    node.descriptor.id().as_str(),
                    producer.as_str(),
                )?;
                if batch
                    .definition
                    .components
                    .iter()
                    .any(|node| node.descriptor.id() == &reader)
                {
                    subscriptions.insert((producer, reader));
                }
            }
        }
        for subscription in subscriptions {
            if !batch.definition.subscriptions.contains(&subscription) {
                batch.definition.subscriptions.push(subscription);
            }
        }
        Ok(())
    }

    pub(crate) fn infrastructure_resources(
        &self,
        publication: &GraphRegistrySnapshot,
        ordinary: &BTreeSet<&str>,
    ) -> anyhow::Result<BTreeSet<ResourceId>> {
        let mut resources: BTreeSet<_> = self
            .services
            .dependencies()
            .into_values()
            .flatten()
            .collect();
        for (name, _) in self.config.index_factory.configured_providers() {
            resources.insert(index_resource_id(&name)?);
        }
        for (id, specification) in &publication.desired.specifications {
            if ordinary.contains(id.as_str()) {
                resources.extend(specification.dependencies.values().flatten().cloned());
                resources.extend(
                    publication
                        .desired
                        .component_resources
                        .get(id)
                        .into_iter()
                        .flatten()
                        .cloned(),
                );
            }
        }
        resources.extend(
            publication
                .desired
                .select(GraphSelection::Exact(
                    ordinary
                        .iter()
                        .map(|id| ComponentId::try_new(*id))
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                ))?
                .resources
                .into_iter()
                .map(|resource| resource.id),
        );
        Ok(resources)
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
            let mut record = owner.record()?;
            if let Some(metadata) = record.value.instance().metadata() {
                record.metadata.extend(metadata);
            }
            anyhow::ensure!(
                &record.node == node
                    && &record.resource == resource
                    && record_token(&publication.desired, node) == Some(record.token),
                "native component {node} does not match its graph-owned instance"
            );
            records.insert(record.value.instance().id().to_owned(), record);
        }
        Ok(records)
    }

    async fn current_records(&self) -> anyhow::Result<BTreeMap<String, Record>> {
        self.records_at(&self.parent()?.control().registry_snapshot())
            .await
    }

    async fn reaction_queries(&self, ids: &[String]) -> anyhow::Result<()> {
        let control = self.control()?;
        let publication = control.registry_snapshot();
        let mut dependencies = Vec::new();
        for id in ids {
            let node = publication
                .desired
                .nodes
                .iter()
                .find(|node| node.descriptor.id().as_str() == id)
                .ok_or_else(|| anyhow::anyhow!("Query '{id}' not found"))?;
            anyhow::ensure!(Self::is_query_node(node), "Component '{id}' is not a Query");
            dependencies.push(control.component_handle(&ComponentId::try_new(id.as_str())?)?);
        }
        for handle in dependencies {
            handle.wait_created().await?;
        }
        Ok(())
    }

    async fn query_subscription(&self, id: &str) -> anyhow::Result<CatalogSubscription> {
        let publication = self.control()?.registry_snapshot();
        self.query_observation_at(&publication, id)?;
        let api = publication
            .query_api(&ComponentId::try_new(id)?)
            .ok_or_else(|| anyhow::anyhow!("query '{id}' has no result subscription capability"))?;
        Self::native_result_catalog(&publication, id, &api)?
            .subscribe_query(id)
            .await
    }

    async fn link_query_output(&self, id: &str) -> anyhow::Result<()> {
        let publication = self.control()?.registry_snapshot();
        self.query_observation_at(&publication, id)?;
        let api = publication
            .query_api(&ComponentId::try_new(id)?)
            .ok_or_else(|| anyhow::anyhow!("query '{id}' has no result access capability"))?;
        Self::native_result_catalog(&publication, id, &api)?;
        api.link_catalog(&self.catalog)
    }

    pub(super) fn native_result_catalog(
        publication: &GraphRegistrySnapshot,
        id: &str,
        api: &QueryApi,
    ) -> anyhow::Result<QueryResultsCatalog> {
        anyhow::ensure!(
            api.config.id == id,
            "query read capability belongs to another component"
        );
        let catalog = api
            .catalog
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Query '{id}' has no configured result outlet"))?;
        let outlet_factory = QueryResultsOutletFactory::default()
            .descriptor()
            .implementation
            .clone();
        let connected = publication
            .desired
            .edges
            .iter()
            .filter(|edge| edge.definition.from.component.as_str() == id)
            .filter_map(|edge| {
                publication
                    .desired
                    .specifications
                    .get(&edge.definition.to.component)
            })
            .filter(|spec| spec.implementation == outlet_factory)
            .flat_map(|spec| spec.dependencies.get("catalog").into_iter().flatten());
        for resource in connected {
            let supplied = publication
                .resource(resource)?
                .get::<QueryResultsCatalog>()?;
            if supplied.same_registry(catalog) {
                return Ok(catalog.clone());
            }
        }
        anyhow::bail!("Query '{id}' has no configured result subscription outlet");
    }

    pub(crate) async fn add_source(
        self: &Arc<Self>,
        name: ComponentName,
        source: Box<dyn Source>,
        metadata: HashMap<String, String>,
        start: bool,
    ) -> anyhow::Result<ComponentHandle> {
        self.add_source_with_recipe(name, source, metadata, None, start, false)
            .await
    }

    pub(crate) async fn create_source(
        self: &Arc<Self>,
        name: ComponentName,
        auto_start: bool,
        factory: impl FnOnce(ComponentId) -> anyhow::Result<Box<dyn Source>> + Send + 'static,
    ) -> anyhow::Result<ComponentHandle> {
        self.create_source_async(
            name,
            None,
            auto_start,
            Box::new(move |id| Box::pin(async move { Ok((factory(id)?, HashMap::new())) })),
            false,
        )
        .await
    }

    pub(crate) async fn create_source_async(
        self: &Arc<Self>,
        name: ComponentName,
        requested_id: Option<ComponentId>,
        auto_start: bool,
        constructor: source_creation::SourceConstructor,
        _start: bool,
    ) -> anyhow::Result<ComponentHandle> {
        self.accept_local(LocalDefinition {
            name,
            id: requested_id,
            auto_start,
            inputs: Vec::new(),
            constructor: LocalConstructor::Source(constructor),
            metadata: HashMap::new(),
            bootstrap: None,
        })
        .await
    }

    async fn add_source_with_recipe(
        self: &Arc<Self>,
        name: ComponentName,
        source: Box<dyn Source>,
        metadata: HashMap<String, String>,
        bootstrap_recipe: Option<crate::config::BootstrapSnapshot>,
        _start: bool,
        created: bool,
    ) -> anyhow::Result<ComponentHandle> {
        anyhow::ensure!(
            !created,
            "eager library-owned source construction is not supported"
        );
        let id = ComponentId::try_new(source.id())?;
        let mut meta = metadata;
        meta.entry("kind".into())
            .or_insert_with(|| source.type_name().to_owned());
        meta.entry("autoStart".into())
            .or_insert_with(|| source.auto_start().to_string());
        let auto_start = source.auto_start();
        self.accept_local(LocalDefinition {
            name,
            id: Some(id),
            auto_start,
            inputs: Vec::new(),
            constructor: LocalConstructor::SourceObject(source),
            metadata: meta,
            bootstrap: bootstrap_recipe,
        })
        .await
    }

    pub(crate) async fn add_query(
        self: &Arc<Self>,
        config: crate::config::QueryConfig,
        _start: bool,
    ) -> anyhow::Result<ComponentHandle> {
        let owner = self.configuration_owner()?;
        let owner = owner
            .get()
            .ok_or_else(|| anyhow::anyhow!("configuration owner is not initialized"))?;
        let definition = crate::management::OrdinaryComponentDefinition::query(config)?;
        let receipt = owner
            .accept_auto(crate::management::AuthoringOperation::Definitions(
                crate::management::ConfigurationRequest::adding([
                    crate::management::NewComponentDefinition::Ordinary(definition),
                ]),
            ))
            .await?;
        let id = receipt
            .creation
            .as_ref()
            .and_then(|creation| creation.components.first())
            .ok_or_else(|| anyhow::anyhow!("accepted query has no identity assignment"))?
            .id
            .clone();
        owner.wait_declared(&id).await?;
        self.component_handle(&id)
    }
    pub(crate) async fn declare_query(
        self: &Arc<Self>,
        config: crate::config::QueryConfig,
    ) -> anyhow::Result<()> {
        self.add_query(config, false).await.map(|_| ())
    }
    pub(crate) async fn add_reaction(
        self: &Arc<Self>,
        name: ComponentName,
        reaction: Box<dyn Reaction>,
        metadata: HashMap<String, String>,
        start: bool,
    ) -> anyhow::Result<ComponentHandle> {
        self.add_reaction_definition(name, reaction, metadata, start, false)
            .await
    }

    pub(crate) async fn create_reaction(
        self: &Arc<Self>,
        name: ComponentName,
        references: Vec<crate::config::ComponentReference>,
        auto_start: bool,
        factory: impl FnOnce(ComponentId, Vec<ComponentId>) -> anyhow::Result<Box<dyn Reaction>>
            + Send
            + 'static,
    ) -> anyhow::Result<ComponentHandle> {
        self.create_reaction_with_inputs(
            name,
            None,
            references,
            auto_start,
            ReactionConstructor::Sync(Box::new(factory)),
            false,
        )
        .await
    }

    pub(crate) async fn create_reaction_with_inputs(
        self: &Arc<Self>,
        name: ComponentName,
        requested_id: Option<ComponentId>,
        references: Vec<crate::config::ComponentReference>,
        auto_start: bool,
        constructor: reaction_creation::ReactionConstructor,
        _start: bool,
    ) -> anyhow::Result<ComponentHandle> {
        self.accept_local(LocalDefinition {
            name,
            id: requested_id,
            auto_start,
            inputs: references,
            constructor: LocalConstructor::Reaction(constructor),
            metadata: HashMap::new(),
            bootstrap: None,
        })
        .await
    }
    pub(crate) async fn declare_reaction(
        self: &Arc<Self>,
        name: ComponentName,
        reaction: Box<dyn Reaction>,
    ) -> anyhow::Result<()> {
        self.add_reaction_definition(name, reaction, HashMap::new(), false, false)
            .await
            .map(|_| ())
    }
    async fn add_reaction_definition(
        self: &Arc<Self>,
        name: ComponentName,
        reaction: Box<dyn Reaction>,
        metadata: HashMap<String, String>,
        _start: bool,
        created: bool,
    ) -> anyhow::Result<ComponentHandle> {
        anyhow::ensure!(
            !created,
            "eager library-owned reaction construction is not supported"
        );
        let id = ComponentId::try_new(reaction.id())?;
        let ids = reaction.query_ids();
        let mut metadata = metadata;
        metadata
            .entry("kind".into())
            .or_insert_with(|| reaction.type_name().to_owned());
        metadata
            .entry("autoStart".into())
            .or_insert_with(|| reaction.auto_start().to_string());
        let auto_start = reaction.auto_start();
        let inputs = ids
            .into_iter()
            .map(|id| {
                Ok(crate::config::ComponentReference::by_id(
                    ComponentId::try_new(id)?,
                ))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        self.accept_local(LocalDefinition {
            name,
            id: Some(id),
            auto_start,
            inputs,
            constructor: LocalConstructor::ReactionObject(reaction),
            metadata,
            bootstrap: None,
        })
        .await
    }

    async fn record(&self, id: &str, kind: &str) -> anyhow::Result<Record> {
        self.current_records()
            .await?
            .get(id)
            .filter(|record| record.value.instance().kind() == kind)
            .cloned()
            .ok_or_else(|| {
                crate::error::ComponentNotFoundError::new(
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
        let publication = control.registry_snapshot();
        if record_token(&publication.desired, &record.node) != Some(record.token) {
            return Err(GraphError::StaleGeneration.into());
        }
        let observed = publication
            .observed
            .components
            .get(&record.node)
            .cloned()
            .ok_or(GraphError::StaleGeneration)?;
        Ok((control, publication.desired.revision, observed))
    }

    fn active(&self, record: &Record) -> anyhow::Result<bool> {
        let (_, _, observed) = self.control_for(record)?;
        Ok(is_active(&observed))
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
            self.retry_creation(&control, revision, &record.node)
                .await?;
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
        let result = control
            .start_members(BTreeMap::from([(record.node.clone(), handle.generation())]))
            .await;
        let report = result?;
        handle.observed()?;
        if report.summary != OperationSummary::Completed
            || !matches!(
                report.components.get(&record.node),
                Some(StartOutcome::Started | StartOutcome::AlreadyRunning)
            )
        {
            return Err(super::v1::LifecycleReportError::Start(report).into());
        }
        Ok(())
    }

    async fn retry_creation(
        &self,
        control: &GraphControl,
        revision: GraphRevision,
        node: &ComponentId,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            control.desired_snapshot().revision == revision,
            GraphError::StaleGeneration
        );
        let generation = control.component_handle(node)?.generation();
        let stopped = control
            .stop_members(BTreeMap::from([(node.clone(), generation)]))
            .await?;
        anyhow::ensure!(
            stopped.summary == OperationSummary::Completed,
            LifecycleReportError::Stop(stopped)
        );
        let owner = self.configuration_owner()?;
        let owner = owner
            .get()
            .ok_or_else(|| anyhow::anyhow!("configuration owner is unavailable"))?;
        let status = owner.reconcile().await?;
        anyhow::ensure!(
            status.error.is_none(),
            "native creation retry failed: {:?}",
            status.error
        );
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
        if !self.current_records().await?.contains_key(id) {
            if expected.is_some() {
                return Err(GraphError::StaleGeneration.into());
            }
            let control = self.control()?;
            let publication = control.registry_snapshot();
            let kind = match kind {
                "source" => "source",
                "query" => "query",
                "reaction" => "reaction",
                _ => anyhow::bail!("unsupported component kind {kind}"),
            };
            let observed = self.component_observation_at(&publication, id, kind)?;
            if kind == "query" {
                let query = ComponentId::try_new(id)?;
                if self.ordinary_queries_at(&publication)?.contains_key(&query) {
                    let handle = control.component_handle(&query)?;
                    return self.start_query_handle(&handle).await;
                }
                return native_query::start_query(&control, &ComponentId::try_new(id)?, None).await;
            }
            anyhow::ensure!(
                !matches!(
                    observed.lifecycle,
                    ComponentLifecycle::Starting
                        | ComponentLifecycle::Running
                        | ComponentLifecycle::Stopping
                ) || observed.failure.is_some()
                    || observed.exhausted,
                "Component '{id}' cannot start while it is {:?}",
                observed.lifecycle
            );
            let component = ComponentId::try_new(id)?;
            let handle = control.component_handle(&component)?;
            anyhow::ensure!(
                handle.generation() == observed.generation,
                GraphError::StaleGeneration
            );
            if observed.realization == RealizationState::CreationFailed {
                self.retry_creation(&control, publication.desired.revision, &component)
                    .await?;
                return control
                    .component_handle(&component)?
                    .start()
                    .await
                    .map_err(Into::into);
            }
            if observed.failure.is_some() || observed.lifecycle == ComponentLifecycle::Failed {
                handle.stop().await?;
            }
            return handle.start().await.map_err(Into::into);
        }
        let record = self.record(id, kind).await?;
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
        self.start_record(&record).await
    }
    async fn stop_record(&self, record: &Record) -> anyhow::Result<()> {
        let result = self.handle_for(record)?.stop().await;
        result?;
        Ok(())
    }

    /// Stops every listed record whose component may still be active and
    /// returns all the failures, so one component cannot keep the others
    /// running.
    ///
    /// The records are a listing taken before the sweep, and lifecycle
    /// sweeps are not serialized with configuration changes. A record that a
    /// concurrent change has since replaced or removed is skipped: the
    /// change owns that component's lifecycle.
    async fn stop_records<'a>(
        &self,
        records: impl IntoIterator<Item = &'a Record>,
    ) -> Vec<anyhow::Error> {
        let mut failures = Vec::new();
        for record in records {
            let result = async {
                let (_, _, observed) = self.control_for(record)?;
                if needs_stop(&observed) {
                    self.stop_record(record).await?;
                }
                anyhow::Ok(())
            }
            .await;
            match result {
                Ok(()) => {}
                Err(error) if superseded(&error) => log::debug!(
                    "{} was replaced or removed concurrently; its lifecycle is no longer this sweep's",
                    record.node
                ),
                Err(error) => failures.push(error.context(record.node.to_string())),
            }
        }
        failures
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
        if !self.current_records().await?.contains_key(id) {
            if expected.is_some() {
                return Err(GraphError::StaleGeneration.into());
            }
            let control = self.control()?;
            let kind = match kind {
                "source" => "source",
                "query" => "query",
                "reaction" => "reaction",
                _ => anyhow::bail!("unsupported component kind {kind}"),
            };
            let publication = control.registry_snapshot();
            let observed = self.component_observation_at(&publication, id, kind)?;
            if kind == "query" {
                return native_query::stop_query(&control, &ComponentId::try_new(id)?, None).await;
            }
            ensure_stoppable(kind, id, observed)?;
            let handle = control.component_handle(&ComponentId::try_new(id)?)?;
            anyhow::ensure!(
                handle.generation() == observed.generation,
                GraphError::StaleGeneration
            );
            return handle.stop().await.map_err(Into::into);
        }
        let record = self.record(id, kind).await?;
        if expected.is_some_and(|token| token != record.token) {
            return Err(GraphError::StaleGeneration.into());
        }
        let (_, _, observed) = self.control_for(&record)?;
        ensure_stoppable(kind, id, &observed)?;
        self.stop_record(&record).await
    }
    pub(crate) async fn remove_component(
        &self,
        id: &str,
        kind: &str,
        cleanup: bool,
    ) -> anyhow::Result<()> {
        self.remove_owned(id, kind, cleanup || kind == "query")
            .await
    }

    pub(crate) async fn update_ordinary_definition(
        self: &Arc<Self>,
        id: &str,
        kind: &'static str,
        definition: crate::management::OrdinaryComponentRecord,
    ) -> anyhow::Result<()> {
        anyhow::ensure!(
            definition.id.as_str() == id,
            "an update cannot change the component UUID"
        );
        let publication = self.control()?.registry_snapshot();
        let current = self.component_observation_at(&publication, id, kind)?;
        let owner = self.configuration_owner()?;
        let owner = owner
            .get()
            .ok_or_else(|| anyhow::anyhow!("configuration owner is unavailable"))?;
        let accepted = owner.configuration()?;
        let previous = accepted
            .desired
            .ordinary
            .iter()
            .find(|record| record.id.as_str() == id)
            .ok_or_else(|| {
                anyhow::anyhow!("native components update through apply_configuration")
            })?;
        anyhow::ensure!(
            previous.name == definition.name,
            "use rename_component to change a component name"
        );
        anyhow::ensure!(
            matches!(
                (&definition.component, kind),
                (
                    crate::management::OrdinaryComponent::Source { .. },
                    "source"
                ) | (
                    crate::management::OrdinaryComponent::Reaction { .. },
                    "reaction"
                )
            ),
            "update definition does not match the component role"
        );
        let id = definition.id.clone();
        let receipt = owner
            .accept_auto(crate::management::AuthoringOperation::Edit {
                generations: BTreeMap::from([(id.clone(), current.generation)]),
                delete_owned_resources: BTreeSet::new(),
                edit: Box::new(move |desired| {
                    let current = desired
                        .ordinary
                        .iter_mut()
                        .find(|current| current.id == id)
                        .ok_or_else(|| anyhow::anyhow!("component {id} was removed"))?;
                    *current = definition;
                    Ok(())
                }),
            })
            .await?;
        let status = owner.wait_applied(receipt.revision).await?;
        anyhow::ensure!(
            status.error.is_none(),
            "accepted component update failed: {:?}",
            status.error
        );
        Ok(())
    }
    pub(crate) async fn update_query(
        self: &Arc<Self>,
        id: &str,
        config: QueryConfig,
    ) -> anyhow::Result<()> {
        self.update_owned_query(id, config).await
    }
    pub(crate) async fn start_kind(self: &Arc<Self>, kind: &str) -> anyhow::Result<()> {
        let publication = self.control()?.registry_snapshot();
        let mut failures = Vec::new();
        if kind == "query" {
            for id in self.ordinary_queries_at(&publication)?.keys() {
                let observed = &publication.observed.components[id];
                if publication.desired.lifecycle_policies[id].auto_start && !is_active(observed) {
                    if let Err(error) = self.start_component(id.as_str(), "query").await {
                        let observed = self.control()?.observed();
                        let waiting = error
                            .chain()
                            .find_map(|cause| match cause.downcast_ref::<LifecycleReportError>() {
                                Some(LifecycleReportError::Start(report))
                                    if report.components.contains_key(id) =>
                                {
                                    Some(report)
                                }
                                _ => None,
                            })
                            .is_some_and(|report| {
                                report.components.keys().all(|member| {
                                    observed
                                        .components
                                        .get(member)
                                        .is_some_and(|node| node.failure.is_none())
                                }) && accept_waiting_start(report.clone(), &observed).is_ok()
                            });
                        if !waiting {
                            failures.push(error.context(format!("query {id}")));
                        }
                    }
                }
            }
        }
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
        for record in records {
            let result = async {
                if self.active(&record)? {
                    return Ok(());
                }
                let (control, _, observed) = self.control_for(&record)?;
                if observed.failure.is_none() && !observed.waiting_for.is_empty() {
                    let report = control
                        .start_members(BTreeMap::from([(record.node.clone(), observed.generation)]))
                        .await?;
                    return accept_waiting_start(report, &control.observed());
                }
                self.start_component_checked(record.value.instance().id(), kind, Some(record.token))
                    .await
            }
            .await;
            match result {
                Ok(()) => {}
                Err(error) if superseded(&error) => log::debug!(
                    "{} was replaced or removed concurrently; its lifecycle is no longer this sweep's",
                    record.node
                ),
                Err(error) => failures.push(error),
            }
        }
        if !failures.is_empty() {
            return Err(crate::error::OperationFailures::new(
                format!("native {kind} startup failed"),
                failures,
            )
            .into());
        }
        Ok(())
    }
    async fn native_component_ids(
        &self,
        publication: &GraphRegistrySnapshot,
    ) -> anyhow::Result<Vec<ComponentId>> {
        let mut ordinary: BTreeSet<_> = self
            .records_at(publication)
            .await?
            .into_values()
            .map(|record| record.node)
            .collect();
        for id in self.ordinary_queries_at(publication)?.keys() {
            ordinary.extend(native_query::query_members(publication, id).into_keys());
        }
        Ok(publication
            .desired
            .nodes
            .iter()
            .map(|node| node.descriptor.id())
            .filter(|id| !ordinary.contains(*id))
            .cloned()
            .collect())
    }
    pub(crate) async fn start_native_components(&self, managed: bool) -> anyhow::Result<()> {
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
            let mut handle = control.component_handle(&id)?;
            let initial = handle.observed()?;
            if initial.realization == RealizationState::CreationFailed && initial.failure.is_none()
            {
                if managed {
                    continue;
                }
                let preview = control
                    .preview(
                        control.desired_snapshot().revision,
                        vec![DesiredMutation::Retry(GraphSelection::Exact(vec![id.clone()]))],
                    )
                    .await?;
                let report = control
                    .reconcile(preview, TopologyBindings::default())
                    .await?;
                report.ensure_committed()?;
                handle = control.component_handle(&id)?;
            }
            let mut changes = control.subscribe_observed();
            let settled = tokio::select! {
                result = changes.wait_for(|state| {
                    state.components.get(&id).is_none_or(|node| {
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
                failures.push(error.context(id.to_string()));
                continue;
            }
            let observed = match handle.observed() {
                Ok(observed) => observed,
                Err(error) => {
                    failures.push(anyhow::Error::new(error).context(id.to_string()));
                    continue;
                }
            };
            if let Some(failure) = observed.failure {
                failures.push(
                    anyhow::Error::new(GraphError::Reported {
                        cause: failure.cause,
                    })
                    .context(id.to_string()),
                );
            } else if observed.realization == RealizationState::Created
                || !observed.waiting_for.is_empty()
            {
                ready.push(handle);
            } else {
                failures.push(anyhow::anyhow!(
                    "{id}: creation is {:?}",
                    observed.realization
                ));
            }
        }
        if !ready.is_empty() {
            for handle in &ready {
                handle.observed()?;
            }
            let report = control
                .start_members(
                    ready
                        .into_iter()
                        .map(|handle| (handle.id().clone(), handle.generation()))
                        .collect(),
                )
                .await?;
            if let Err(error) = accept_waiting_start(report, &control.observed()) {
                failures.push(error);
            }
        }
        if !failures.is_empty() {
            return Err(crate::error::OperationFailures::new(
                "native component startup failed",
                failures,
            )
            .into());
        }
        Ok(())
    }
    pub(crate) async fn subscriptions_complete(self: &Arc<Self>) -> anyhow::Result<()> {
        let publication = self.control()?.registry_snapshot();
        let ordinary = self.ordinary_queries_at(&publication)?;
        let mut queries = ordinary
            .keys()
            .map(|id| self.native_query_configuration_at(&publication, id.as_str()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        for node in publication.desired.nodes.iter() {
            let id = node.descriptor.id();
            if !ordinary.contains_key(id) {
                if let Some(api) = publication.query_api(id) {
                    queries.push(api.config.clone());
                }
            }
        }
        let sources: Vec<_> = self
            .current_records()
            .await?
            .values()
            .filter_map(|record| match &record.value {
                Value::Source(source) => Some(source.clone()),
                _ => None,
            })
            .collect();
        for source in sources {
            let mut complete = true;
            for query in queries.iter().filter(|query| {
                query.inputs.iter().any(|input| {
                    input
                        .component
                        .as_id()
                        .is_some_and(|id| id.as_str() == source.source.id())
                })
            }) {
                let id = ComponentId::try_new(query.id.as_str())?;
                let requested = query.auto_start
                    || publication
                        .observed
                        .components
                        .get(&id)
                        .is_some_and(|state| state.started || state.lifecycle_requested);
                if requested {
                    let expected = query
                        .inputs
                        .iter()
                        .filter_map(|input| input.component.as_id())
                        .collect::<BTreeSet<_>>()
                        .len();
                    if self.query(&query.id).await?.subscription_count().await != expected {
                        complete = false;
                        break;
                    }
                }
            }
            if complete {
                source.notify_subscriptions_complete().await;
            }
        }
        Ok(())
    }
    pub(crate) async fn stop_all(&self) -> anyhow::Result<()> {
        let mut failures = Vec::new();
        let control = self.control()?;
        let publication = control.registry_snapshot();
        match self.native_component_ids(&publication).await {
            Ok(native) if native.is_empty() => {}
            Ok(native) => match control
                .stop_members(
                    native
                        .into_iter()
                        .map(|id| {
                            let generation = publication.observed.components[&id].generation;
                            (id, generation)
                        })
                        .collect(),
                )
                .await
            {
                Ok(report) if report.summary == OperationSummary::Completed => {}
                Ok(report) => failures.push(super::v1::LifecycleReportError::Stop(report).into()),
                Err(error) => failures.push(anyhow::Error::new(error)),
            },
            Err(error) => failures.push(error.context("listing native components")),
        }
        // Consumers stop before their producers, and each kind is listed
        // afresh so the sweep sees the configuration as it is when it starts
        // that kind.
        for kind in ["reaction", "query", "source"] {
            if kind == "query" {
                let current = control.registry_snapshot();
                for id in self.ordinary_queries_at(&current)?.keys() {
                    if native_query::query_needs_stop(&current, id) {
                        if let Err(error) = native_query::stop_query(&control, id, None).await {
                            failures.push(error);
                        }
                    }
                }
            }
            match self.current_records().await {
                Ok(records) => failures.extend(
                    self.stop_records(
                        records
                            .values()
                            .filter(|record| record.value.instance().kind() == kind),
                    )
                    .await,
                ),
                Err(error) => failures.push(error.context(format!("listing {kind} components"))),
            }
        }
        if !failures.is_empty() {
            return Err(
                crate::error::OperationFailures::new("native stop failed", failures).into(),
            );
        }
        Ok(())
    }
    pub(crate) async fn stop_kind(&self, kind: &str) -> anyhow::Result<()> {
        let records = self.current_records().await?;
        let mut failures = self
            .stop_records(
                records
                    .values()
                    .filter(|record| record.value.instance().kind() == kind),
            )
            .await;
        if kind == "query" {
            let control = self.control()?;
            let publication = control.registry_snapshot();
            for id in self.ordinary_queries_at(&publication)?.keys() {
                if native_query::query_needs_stop(&publication, id) {
                    if let Err(error) = native_query::stop_query(&control, id, None).await {
                        failures.push(error);
                    }
                }
            }
        }
        if !failures.is_empty() {
            return Err(crate::error::OperationFailures::new(
                format!("stopping {kind} components failed"),
                failures,
            )
            .into());
        }
        Ok(())
    }
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.parent.get() {
            parent.shutdown().await?;
        }
        Ok(())
    }
    pub(crate) async fn source(&self, id: &str) -> anyhow::Result<Arc<dyn Source>> {
        if !self.current_records().await?.contains_key(id) {
            self.component_observation_at(&self.control()?.registry_snapshot(), id, "source")?;
            anyhow::bail!("Source '{id}' exists but does not expose the Source plugin interface");
        }
        match self.record(id, "source").await?.value {
            Value::Source(source) => Ok(source.source.clone()),
            _ => unreachable!(),
        }
    }
    pub(crate) async fn source_host(&self, id: &str) -> anyhow::Result<Arc<SourcePluginHost>> {
        match self.record(id, "source").await?.value {
            Value::Source(source) => Ok(source.borrowed.clone()),
            _ => unreachable!(),
        }
    }
    pub(crate) async fn reaction(&self, id: &str) -> anyhow::Result<Arc<dyn Reaction>> {
        if !self.current_records().await?.contains_key(id) {
            self.component_observation_at(&self.control()?.registry_snapshot(), id, "reaction")?;
            anyhow::bail!(
                "Reaction '{id}' exists but does not expose the Reaction plugin interface"
            );
        }
        match self.record(id, "reaction").await?.value {
            Value::Reaction(reaction) => Ok(reaction.reaction.clone()),
            _ => unreachable!(),
        }
    }
    pub(crate) async fn query(self: &Arc<Self>, id: &str) -> anyhow::Result<Arc<NativeQuery>> {
        let control = self.control()?;
        let publication = control.registry_snapshot();
        let config = self.native_query_configuration_at(&publication, id)?;
        let handle = control.component_handle(&ComponentId::try_new(id)?)?;
        anyhow::ensure!(
            publication.observed.components[handle.id()].generation == handle.generation(),
            GraphError::StaleGeneration
        );
        Ok(Arc::new(NativeQuery {
            config,
            handle,
            control,
            owner: Arc::downgrade(self),
        }))
    }
    pub(crate) async fn query_instance(
        self: &Arc<Self>,
        id: &str,
    ) -> anyhow::Result<Arc<dyn QueryTrait>> {
        Ok(self.query(id).await?)
    }
    pub(crate) async fn reaction_metrics(
        &self,
        id: &str,
    ) -> anyhow::Result<HashMap<String, ReactionMetricsSnapshot>> {
        match self.record(id, "reaction").await?.value {
            Value::Reaction(reaction) => Ok(reaction
                .metrics()?
                .iter()
                .map(|(id, metrics)| (id.clone(), metrics.snapshot()))
                .collect()),
            _ => unreachable!(),
        }
    }
    pub(crate) fn query_progress_metrics(
        &self,
    ) -> anyhow::Result<Vec<crate::metrics::QueryProgressMetricsSnapshot>> {
        let control = self.control()?;
        anyhow::ensure!(
            !matches!(
                control.state(),
                GraphState::Cancelled
                    | GraphState::Completed
                    | GraphState::Failed
                    | GraphState::CleanupRequired
            ),
            "query progress is unavailable while the graph is {:?}",
            control.state()
        );
        let publication = control.registry_snapshot();
        let mut snapshots = Vec::new();
        for (id, component) in &publication.observed.components {
            if component.failure.is_some()
                || !component.started
                || !matches!(
                    component.lifecycle,
                    ComponentLifecycle::Running | ComponentLifecycle::Quiesced
                )
            {
                continue;
            }
            if let Some(api) = publication.query_api(id) {
                if let Some(snapshot) = api.progress_metrics(component)? {
                    snapshots.push((id, snapshot));
                }
            }
        }
        let current = control.registry_snapshot();
        Ok(snapshots
            .into_iter()
            .filter(|(id, snapshot)| {
                current
                    .observed
                    .components
                    .get(*id)
                    .is_some_and(|component| {
                        component.generation.0 == snapshot.component_generation
                            && component.operation.0 == snapshot.component_operation
                            && component.failure.is_none()
                            && component.started
                            && matches!(
                                component.lifecycle,
                                ComponentLifecycle::Running | ComponentLifecycle::Quiesced
                            )
                    })
            })
            .map(|(_, snapshot)| snapshot)
            .collect())
    }
    pub(crate) async fn graph_schema(&self) -> anyhow::Result<crate::schema::GraphSchema> {
        let publication = self.control()?.registry_snapshot();
        let sources: Vec<_> = publication
            .desired
            .nodes
            .iter()
            .filter(|node| {
                Self::public_component_kind(&publication.desired, node) == Some("source")
            })
            .map(|node| node.descriptor.id().as_str())
            .collect();
        let mut schema = crate::schema::GraphSchema::default();
        for id in &sources {
            if let Some(source_schema) = self
                .source_schema_at(&publication, id)
                .await?
                .filter(|schema| !schema.is_empty())
            {
                schema.merge_source_schema(id, &source_schema);
            } else {
                schema.record_source_without_schema(id);
            }
        }
        for query in self.query_configurations().await? {
            let labels = crate::queries::LabelExtractor::extract_labels(
                &query.query,
                &query.query_language,
            )?;
            schema.mark_queried_nodes(labels.node_labels.iter().map(String::as_str), &query.id);
            schema.mark_queried_relations(
                labels.relation_labels.iter().map(String::as_str),
                &query.id,
            );
        }
        for id in sources {
            self.ensure_source_schema_current(&publication, id)?;
        }
        Ok(schema)
    }
}
