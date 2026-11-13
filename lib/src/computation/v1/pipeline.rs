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

use super::*;
use crate::{
    config::{AssignedQueryConfig as QueryConfig, QueryLanguage},
    indexes::{IndexFactory, StorageBackendRef, StorageBackendSpec},
    RecoveryPolicy,
};
use async_trait::async_trait;
use drasi_core::{
    computation::{ComputationIndexProvider, InMemoryComputationProvider},
    interface::{CreatedIndexes, IndexBackendPlugin, IndexError},
    middleware::MiddlewareTypeRegistry,
};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

struct FactoryProvider {
    factory: Arc<IndexFactory>,
    backend: StorageBackendRef,
    volatile: bool,
    atomic_output: bool,
    durability: drasi_core::interface::StorageDurability,
}

struct MemoryQueryProvider;

#[async_trait]
impl ComputationIndexProvider for MemoryQueryProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        query: &ComponentId,
    ) -> std::result::Result<drasi_core::computation::ComputationIndexes, IndexError> {
        use drasi_core::{
            computation::{ComputationIndexes, ComputationResource},
            in_memory_index::in_memory_checkpoint_store::InMemoryCheckpointStore,
            interface::{IndexSet, StorageDurability},
        };
        let resources = InMemoryComputationProvider
            .create_indexes(graph, query)
            .await?;
        let indexes = resources.indexes();
        ComputationIndexes::try_new(
            IndexSet {
                element_index: indexes.element_index.clone(),
                archive_index: indexes.archive_index.clone(),
                result_index: indexes.result_index.clone(),
                future_queue: indexes.future_queue.clone(),
                session_control: indexes.session_control.clone(),
            },
            None,
            Some(ComputationResource::independent(Arc::new(
                InMemoryCheckpointStore::new(),
            ))),
            None,
            None,
        )
        .map(|resources| resources.with_durability(StorageDurability::VOLATILE))
        .map_err(IndexError::other)
    }

    fn is_volatile(&self) -> bool {
        true
    }
}

struct SubscriptionRecipe {
    source: ResourceId,
    progress: ResourceId,
    options: SourceSubscriptionOptions,
    stream: StreamId,
    query_id: ComponentId,
}

#[async_trait]
impl ResourceConstructor for SubscriptionRecipe {
    async fn construct(&self) -> anyhow::Result<ResourceHandle> {
        anyhow::bail!("source subscription construction requires its declared dependencies")
    }

    async fn construct_with_dependencies(
        &self,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<ResourceHandle> {
        let source = dependencies
            .get(&self.source)
            .ok_or_else(|| anyhow::anyhow!("source binding '{}' is unavailable", self.source))?
            .get::<SourcePluginHost>()?;
        let progress = dependencies
            .get(&self.progress)
            .ok_or_else(|| anyhow::anyhow!("query progress '{}' is unavailable", self.progress))?
            .get::<QuerySourceProgressResource>()?;
        let subscription = LegacySourceSubscription::new(
            source,
            self.options.clone(),
            self.stream.clone(),
            Some(progress.0.clone()),
            &self.query_id,
        )?;
        Ok(
            ResourceHandle::new(ResourceRole::SourceSubscription, subscription.clone())
                .with_cleanup(subscription),
        )
    }
}

struct BootstrapRecipe {
    subscriptions: Vec<ResourceId>,
    output: Option<Arc<dyn ComputationBootstrapProvider>>,
    scope: String,
    graph: String,
    query: ComponentId,
}

impl BootstrapRecipe {
    fn provider(
        &self,
        subscriptions: Vec<Arc<LegacySourceSubscription>>,
    ) -> anyhow::Result<Arc<dyn ComputationBootstrapProvider>> {
        let Some(output) = &self.output else {
            return Ok(LegacySourceBootstrap::new(subscriptions));
        };
        if subscriptions.is_empty() {
            return Ok(output.clone());
        }
        Ok(Arc::new(CompositeBootstrapProvider::new(
            self.scope.clone(),
            self.graph.clone(),
            self.query.clone(),
            vec![
                (
                    ResourceId::try_new("source-inputs")?,
                    LegacySourceBootstrap::new(subscriptions),
                ),
                (ResourceId::try_new("component-inputs")?, output.clone()),
            ],
        )?))
    }
}

#[async_trait]
impl ResourceConstructor for BootstrapRecipe {
    async fn construct(&self) -> anyhow::Result<ResourceHandle> {
        anyhow::bail!("query bootstrap construction requires its declared subscriptions")
    }

    async fn construct_with_dependencies(
        &self,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<ResourceHandle> {
        let subscriptions = self
            .subscriptions
            .iter()
            .map(|id| {
                dependencies
                    .get(id)
                    .ok_or_else(|| anyhow::anyhow!("query subscription '{id}' is unavailable"))?
                    .get::<LegacySourceSubscription>()
                    .map_err(anyhow::Error::from)
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        Ok(ResourceHandle::new(
            ResourceRole::Bootstrap,
            Arc::new(QueryBootstrapResource(self.provider(subscriptions)?)),
        ))
    }
}

#[async_trait]
impl IndexBackendPlugin for FactoryProvider {
    async fn create_indexes(
        &self,
        query: &ComponentId,
    ) -> std::result::Result<CreatedIndexes, IndexError> {
        self.factory
            .build(&self.backend, query)
            .await
            .map_err(IndexError::other)
    }
    async fn create_scoped_indexes(
        &self,
        storage_scope: &drasi_core::interface::StorageScope,
        query_id: &ComponentId,
    ) -> std::result::Result<CreatedIndexes, IndexError> {
        self.factory
            .build_scoped(&self.backend, storage_scope, query_id)
            .await
            .map_err(IndexError::other)
    }
    fn is_volatile(&self) -> bool {
        self.volatile
    }
    fn durability(&self) -> drasi_core::interface::StorageDurability {
        self.durability
    }
    fn supports_atomic_query_output(&self) -> bool {
        self.atomic_output
    }
}

/// Assemble source subscriptions, queries and reactions for a DrasiLib instance.
pub struct ComputationPipelineBuilder {
    graph_id: String,
    services: LegacyPluginServices,
    catalog: QueryResultsCatalog,
    middleware: Arc<MiddlewareTypeRegistry>,
    query_factory: Option<Arc<dyn ComponentFactory>>,
    indexes: Arc<IndexFactory>,
    default_recovery: RecoveryPolicy,
    input_capacity: usize,
    output_capacity: usize,
    sources: BTreeMap<ComponentId, (Option<Arc<SourcePluginHost>>, SourceSubscriptionOptions)>,
    native_sources: BTreeMap<ComponentId, Option<Endpoint>>,
    output_catalogue: Option<(String, OutputCatalogue)>,
    bootstraps: BTreeMap<ComponentId, Arc<dyn ComputationBootstrapProvider>>,
    queries: Vec<QueryConfig>,
    reactions: Vec<(ComponentName, Arc<ReactionPluginHost>, bool)>,
    overrides: BTreeMap<ComponentId, (Arc<dyn ComputationIndexProvider>, QueryPublicationMode)>,
    #[cfg(feature = "test-support")]
    test_control: Option<crate::test_support::QueryTestControl>,
}

fn encoded(value: &str) -> String {
    value.bytes().map(|value| format!("{value:02x}")).collect()
}
fn resource(kind: &str, id: &ComponentId) -> GraphResult<ResourceId> {
    Ok(ResourceId::try_new(format!(
        "{kind}/{}",
        encoded(id.as_str())
    ))?)
}
fn endpoint(component: ComponentId, port: &str) -> Endpoint {
    Endpoint::new(component, PortId::try_new(port).expect("constant port"))
}

fn builtin_factory<T: ComponentFactory + Default>() -> Arc<dyn ComponentFactory> {
    FactoryRegistry::standard()
        .get(&T::default().descriptor().implementation)
        .expect("pipeline factory is registered")
        .clone()
}

fn authorize_helper(
    ids: &mut Option<&mut ComponentIds>,
    owner: &ComponentId,
    helper: ComponentId,
) -> GraphResult<ComponentId> {
    if let Some(ids) = ids.as_deref_mut() {
        ids.authorize_derived(owner, helper)
    } else {
        Ok(helper)
    }
}

impl ComputationPipelineBuilder {
    pub(crate) fn new(
        graph_id: &str,
        services: LegacyPluginServices,
        middleware: Arc<MiddlewareTypeRegistry>,
        indexes: Arc<IndexFactory>,
        default_recovery: RecoveryPolicy,
        input_capacity: usize,
        output_capacity: usize,
    ) -> GraphResult<Self> {
        Ok(Self {
            graph_id: graph_id.to_owned(),
            services,
            catalog: QueryResultsCatalog::new(graph_id)?,
            middleware,
            query_factory: None,
            indexes,
            default_recovery,
            input_capacity,
            output_capacity,
            sources: BTreeMap::new(),
            native_sources: BTreeMap::new(),
            output_catalogue: None,
            bootstraps: BTreeMap::new(),
            queries: Vec::new(),
            reactions: Vec::new(),
            overrides: BTreeMap::new(),
            #[cfg(feature = "test-support")]
            test_control: None,
        })
    }
    #[cfg(feature = "test-support")]
    pub(crate) fn with_test_control(
        mut self,
        control: crate::test_support::QueryTestControl,
    ) -> Self {
        self.test_control = Some(control);
        self
    }
    pub fn catalog(&self) -> QueryResultsCatalog {
        self.catalog.clone()
    }
    pub(crate) fn with_catalog(mut self, catalog: QueryResultsCatalog) -> Self {
        self.catalog = catalog;
        self
    }
    pub fn services(&self) -> LegacyPluginServices {
        self.services.clone()
    }
    pub fn source_component_id(query: &ComponentId, source: &ComponentId) -> Result<ComponentId> {
        drasi_computation_contracts::query_source_component_id(query, source).map_err(Into::into)
    }
    pub(crate) fn source_resource_id(source: &ComponentId) -> GraphResult<ResourceId> {
        resource("source-host", source)
    }
    pub(crate) fn index_resource_id(query: &ComponentId) -> GraphResult<ResourceId> {
        resource("query-indexes", query)
    }
    pub fn source(
        self,
        host: Arc<SourcePluginHost>,
        options: SourceSubscriptionOptions,
    ) -> GraphResult<Self> {
        let id = host.id().to_owned();
        self.source_binding(&id, Some(host), options)
    }

    /// Connect a native graph-change output using bounded, ranked query input.
    ///
    /// The source remains owned by the surrounding graph, not by this query.
    /// Source-plugin label selectors are not used on this path. Supply initial
    /// data explicitly with `query_bootstrap`. Admitted durable sources require
    /// their actual QoS connections instead of these bounded input pipes.
    pub fn native_source(self, output: Endpoint) -> GraphResult<Self> {
        let id = output.component.clone();
        self.native_source_binding(id, Some(output))
    }

    pub(crate) fn pending_native_source(self, component: &ComponentId) -> GraphResult<Self> {
        self.native_source_binding(component.to_owned(), None)
    }

    fn native_source_binding(
        mut self,
        id: ComponentId,
        output: Option<Endpoint>,
    ) -> GraphResult<Self> {
        if self.sources.contains_key(&id) || self.native_sources.contains_key(&id) {
            return Err(GraphError::Topology {
                reason: "duplicate source".into(),
            });
        }
        self.native_sources.insert(id, output);
        Ok(self)
    }

    pub(crate) fn pending_native_endpoint(
        query: &ComponentId,
        input: &ComponentId,
    ) -> GraphResult<Endpoint> {
        // An internal construction dependency, never a claimed producer port.
        Ok(endpoint(
            ComponentId::derive_owned(
                &query.clone(),
                &format!("pending-native-input/{}", input.clone()),
            )?,
            "unresolved-primary",
        ))
    }

    pub(crate) fn with_output_catalogue(
        mut self,
        scope: String,
        catalogue: OutputCatalogue,
    ) -> Self {
        self.output_catalogue = Some((scope, catalogue));
        self
    }

    /// Supply an application-owned snapshot for a query with native inputs.
    pub fn query_bootstrap(
        mut self,
        query: &ComponentId,
        bootstrap: Arc<dyn ComputationBootstrapProvider>,
    ) -> GraphResult<Self> {
        if self.bootstraps.insert(query.clone(), bootstrap).is_some() {
            return Err(GraphError::Topology {
                reason: "duplicate query bootstrap".into(),
            });
        }
        Ok(self)
    }

    pub(crate) fn source_binding(
        mut self,
        id: &ComponentId,
        host: Option<Arc<SourcePluginHost>>,
        options: SourceSubscriptionOptions,
    ) -> GraphResult<Self> {
        if self.native_sources.contains_key(id) {
            return Err(GraphError::Topology {
                reason: "duplicate source".into(),
            });
        }
        if host.as_ref().is_some_and(|host| host.id() != id) {
            return Err(GraphError::Topology {
                reason: "source binding has a different identity".into(),
            });
        }
        if self
            .sources
            .insert(id.to_owned(), (host, options))
            .is_some()
        {
            return Err(GraphError::Topology {
                reason: "duplicate source".into(),
            });
        }
        Ok(self)
    }
    pub fn query(mut self, config: QueryConfig) -> Self {
        self.queries.push(config);
        self
    }
    pub fn reaction(
        mut self,
        name: ComponentName,
        host: Arc<ReactionPluginHost>,
        auto_start: bool,
    ) -> Self {
        self.reactions.push((name, host, auto_start));
        self
    }
    pub fn middleware_registry(mut self, registry: Arc<MiddlewareTypeRegistry>) -> Self {
        self.middleware = registry;
        self
    }

    /// Select query execution without changing ordinary query authoring or source assembly.
    pub fn query_factory(mut self, factory: Arc<dyn ComponentFactory>) -> Self {
        self.query_factory = Some(factory);
        self
    }
    pub fn query_provider(
        mut self,
        query: &ComponentId,
        provider: Arc<dyn ComputationIndexProvider>,
        publication: QueryPublicationMode,
    ) -> GraphResult<Self> {
        if self
            .overrides
            .insert(query.clone(), (provider, publication))
            .is_some()
        {
            return Err(GraphError::Topology {
                reason: "duplicate query provider override".into(),
            });
        }
        Ok(self)
    }
    pub fn build(self) -> GraphResult<ComponentBatch> {
        self.build_inner(None)
    }

    /// Assemble inside `DrasiLib::create_components`. Primary component UUIDs
    /// must come from this allocation scope. Only this pipeline's actual
    /// owner-derived helpers gain admission alongside those fresh owners.
    pub fn build_owned(self, ids: &mut ComponentIds) -> GraphResult<ComponentBatch> {
        for query in &self.queries {
            ids.require_owned(&query.id.clone())?;
        }
        for (_, host, _) in &self.reactions {
            ids.require_owned(&host.id().clone())?;
        }
        self.build_inner(Some(ids))
    }

    fn build_inner(self, mut ids: Option<&mut ComponentIds>) -> GraphResult<ComponentBatch> {
        let invalid = |reason: String| GraphError::Topology { reason };
        let query_authoring = self
            .query_factory
            .as_ref()
            .map(|factory| {
                factory.query_authoring().ok_or_else(|| {
                    invalid("selected factory does not support ordinary query authoring".into())
                })
            })
            .transpose()?;
        if !self.queries.is_empty() && query_authoring.is_none() {
            return Err(invalid(QueryFactoryNotRegistered.to_string()));
        }
        let mut native_inputs = Vec::new();
        let mut query_ids = BTreeSet::new();
        let mut prepared = Vec::new();
        let mut declared_sources = BTreeSet::new();
        let mut subscribers: BTreeMap<ComponentId, usize> = BTreeMap::new();
        for host in self.sources.values().filter_map(|(host, _)| host.as_ref()) {
            if host.is_owned() && !host.services().same_services(&self.services) {
                return Err(invalid(
                    "owned source must use this pipeline's scoped services".into(),
                ));
            }
        }
        for query in &self.queries {
            for input in &query.inputs {
                input.id().map_err(|error| invalid(error.to_string()))?;
            }
            let id = query.id.clone();
            if !query_ids.insert(query.id.clone()) {
                return Err(invalid("duplicate query".into()));
            }
            let execution = QueryExecutionSettings::from_configuration(query)
                .map_err(|source| GraphError::ReferenceResolution { source })?;
            query_authoring
                .expect("query authoring was required")
                .validate_execution(&execution, Some(&self.middleware))
                .map_err(|error| invalid(format!("{error:#}")))?;
            let settings = if query.inputs.is_empty() {
                Vec::new()
            } else {
                QueryExecutionSettings::subscriptions(query)
                    .map_err(|error| invalid(format!("{error:#}")))?
            };
            for setting in &settings {
                if self.native_sources.contains_key(&setting.source_id) {
                    let configured = query
                        .inputs
                        .iter()
                        .find(|input| {
                            input
                                .component
                                .as_id()
                                .is_some_and(|id| id == &setting.source_id)
                        })
                        .expect("subscription belongs to this query");
                    if !configured.nodes.is_empty() || !configured.relations.is_empty() {
                        return Err(invalid(
                            "native input label selection belongs in the graph, not Source-plugin subscription settings".into(),
                        ));
                    }
                }
                declared_sources.insert(setting.source_id.clone());
                if query.auto_start {
                    *subscribers.entry(setting.source_id.clone()).or_default() += 1;
                }
            }
            if !self.overrides.contains_key(&query.id) {
                self.indexes
                    .computation_backend(query.storage_backend.as_ref())
                    .map_err(|error| invalid(error.to_string()))?;
            }
            prepared.push((id, execution, settings));
        }
        for id in self.overrides.keys() {
            if !query_ids.contains(id) {
                return Err(invalid(format!(
                    "provider override names unknown query {id}"
                )));
            }
        }
        for id in self.bootstraps.keys() {
            let query = self
                .queries
                .iter()
                .find(|query| &query.id == id)
                .ok_or_else(|| invalid(format!("bootstrap names unknown query {id}")))?;
            if !query.enable_bootstrap
                || query.inputs.iter().any(|input| {
                    input
                        .component
                        .as_id()
                        .is_none_or(|id| !self.native_sources.contains_key(id))
                })
            {
                return Err(invalid(
                    "explicit query bootstrap requires enabled bootstrap and native input bindings"
                        .into(),
                ));
            }
        }
        let mut reaction_ids = BTreeSet::new();
        for (_, host, _) in &self.reactions {
            if !reaction_ids.insert(host.id()) {
                return Err(invalid("duplicate reaction".into()));
            }
            if !host.catalog().same_registry(&self.catalog)
                || host.is_owned() && !host.services().same_services(&self.services)
            {
                return Err(invalid(
                    "reaction must use this pipeline's catalogue and scoped services".into(),
                ));
            }
            if host.query_ids().is_empty()
                || host.query_ids().iter().any(|id| !query_ids.contains(id))
            {
                return Err(invalid(format!(
                    "reaction {} has unresolved query bindings",
                    host.id()
                )));
            }
        }
        let mut builder = self
            .services
            .declare(ComputationGraph::builder(self.graph_id.as_str()))?;
        let assembly = self.queries.first().map(|query| &query.id);
        let catalog_id = match assembly {
            Some(query) => resource("query-catalog", query)?,
            None => ResourceId::try_new("query-catalog")?,
        };
        builder = builder
            .declare_resource(ResourceSpecification {
                id: catalog_id.clone(),
                role: ResourceRole::QueryCatalog,
                ownership: ResourceOwnership::Borrowed,
                binding: Arc::from("query-catalog"),
            })?
            .provide_resource(
                catalog_id.clone(),
                ResourceHandle::new(ResourceRole::QueryCatalog, Arc::new(self.catalog.clone())),
            )?;
        let middleware_id = match assembly {
            Some(query) => resource("query-middleware", query)?,
            None => ResourceId::try_new("query-middleware")?,
        };
        builder = builder
            .declare_resource(ResourceSpecification {
                id: middleware_id.clone(),
                role: ResourceRole::Middleware,
                ownership: ResourceOwnership::Borrowed,
                binding: Arc::from("instance.middleware"),
            })?
            .provide_resource(
                middleware_id.clone(),
                ResourceHandle::new(
                    ResourceRole::Middleware,
                    Arc::new(QueryMiddlewareResource(self.middleware.clone())),
                ),
            )?;
        for (name, (host, _)) in &self.sources {
            let id = Self::source_resource_id(name)?;
            builder = builder.declare_resource(ResourceSpecification {
                id: id.clone(),
                role: ResourceRole::LegacySource,
                ownership: if host.as_ref().is_some_and(|host| host.is_owned()) {
                    ResourceOwnership::Graph
                } else {
                    ResourceOwnership::Borrowed
                },
                binding: Arc::from(name.as_str()),
            })?;
            let Some(host) = host else { continue };
            builder = builder.provide_resource(id, host.resource())?;
            if let Some(bootstrap) = host
                .bootstrap_resource()
                .map_err(|error| invalid(format!("{error:#}")))?
            {
                let id = resource("source-bootstrap", name)?;
                builder = builder
                    .declare_resource(ResourceSpecification {
                        id: id.clone(),
                        role: ResourceRole::Bootstrap,
                        ownership: ResourceOwnership::Borrowed,
                        binding: Arc::from(format!("source-bootstrap:{name}")),
                    })?
                    .provide_resource(id, bootstrap)?;
            }
        }
        for name in declared_sources.iter().filter(|name| {
            !self.sources.contains_key(*name) && !self.native_sources.contains_key(*name)
        }) {
            builder = builder.declare_resource(ResourceSpecification {
                id: Self::source_resource_id(name)?,
                role: ResourceRole::LegacySource,
                ownership: ResourceOwnership::Borrowed,
                binding: Arc::from(name.as_str()),
            })?;
        }
        let source_factory = builtin_factory::<SourcePluginAdapterFactory>();
        let query_factory = self.query_factory.clone();
        let outlet_owner = self
            .queries
            .first()
            .map(|query| ComponentId::try_new(query.id.as_str()))
            .transpose()?;
        let outlet_id = if let Some(owner) = &outlet_owner {
            authorize_helper(
                &mut ids,
                owner,
                ComponentId::derive_owned(owner, "query-results")?,
            )?
        } else {
            match ids.as_deref_mut() {
                Some(ids) => ids.allocate()?,
                None => ComponentId::generate(),
            }
        };
        let outlet_owner = outlet_owner.as_ref().unwrap_or(&outlet_id);
        let outlet_factory = builtin_factory::<QueryResultsOutletFactory>();
        builder = builder
            .component(
                ComponentName::try_new(format!("__internal/{outlet_owner}/results"))?,
                ComponentSpecification {
                    descriptor: QueryResultsOutlet::new(outlet_id.clone(), self.catalog.clone())
                        .descriptor()
                        .clone(),
                    role: ComponentRole::Sink,
                    completion: Some(SinkCompletion::Handled),
                    implementation: outlet_factory.descriptor().implementation.clone(),
                    configuration_version: 1,
                    configuration: BTreeMap::new(),
                    dependencies: BTreeMap::from([(
                        Arc::from("catalog"),
                        vec![catalog_id.clone()],
                    )]),
                },
                outlet_factory,
            )
            .lifecycle_policy(outlet_id.clone(), LifecyclePolicy { auto_start: true });
        for (config, (query_id, execution, settings)) in self.queries.iter().zip(prepared) {
            let input_capacity = config
                .priority_queue_capacity
                .unwrap_or(self.input_capacity);
            let input_queue_id = resource("query-inputs", &config.id)?;
            let input_queue = RankedInputQueue::new(input_capacity)
                .map_err(|error| invalid(error.to_string()))?;
            let scheduling_id = resource("query-scheduling", &config.id)?;
            let scheduling = QuerySchedulingResource::default();
            #[cfg(feature = "test-support")]
            let scheduling = scheduling.with_test_control(self.test_control.clone());
            let scheduling = Arc::new(scheduling);
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: input_queue_id.clone(),
                    role: ResourceRole::Pipe,
                    ownership: ResourceOwnership::Graph,
                    binding: Arc::from(format!("query-inputs:{}", config.id)),
                })?
                .provide_resource(input_queue_id.clone(), input_queue.resource())?
                .declare_resource(ResourceSpecification {
                    id: scheduling_id.clone(),
                    role: ResourceRole::FutureQueue,
                    ownership: ResourceOwnership::Graph,
                    binding: Arc::from(format!("query-scheduling:{}", config.id)),
                })?
                .provide_resource(scheduling_id.clone(), scheduling.resource())?;
            let scheduled_rank = settings.len();
            if settings.is_empty() {
                builder = builder.unbound(DesiredRelationship {
                    definition: EdgeDefinition::new(
                        endpoint(
                            ComponentId::derive_owned(&query_id, "unbound-input")?,
                            "out",
                        ),
                        endpoint(query_id.clone(), "in"),
                    ),
                    policy: RelationshipPolicy {
                        required_for_creation: false,
                        required_for_binding: false,
                        orphan_permitted: true,
                        ..Default::default()
                    },
                    pipe: DesiredPipe::Bounded {
                        capacity: self.input_capacity,
                    },
                });
            }
            let progress = Arc::new(QuerySourceProgress::new(
                self.graph_id.as_str(),
                query_id.clone(),
            )?);
            let progress_id = resource("query-progress", &config.id)?;
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: progress_id.clone(),
                    role: ResourceRole::Checkpoint,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from(format!("query-progress:{}", config.id)),
                })?
                .provide_resource(
                    progress_id.clone(),
                    ResourceHandle::new(
                        ResourceRole::Checkpoint,
                        Arc::new(QuerySourceProgressResource(progress.clone())),
                    ),
                )?;
            let mut subscription_ids = Vec::new();
            for (source_rank, setting) in settings.into_iter().enumerate() {
                if let Some(output) = self.native_sources.get(&setting.source_id) {
                    let output = match output {
                        Some(output) => output.clone(),
                        None => Self::pending_native_endpoint(&config.id, &setting.source_id)?,
                    };
                    native_inputs.push(DesiredRelationship {
                        definition: EdgeDefinition::new(output, endpoint(query_id.clone(), "in")),
                        policy: RelationshipPolicy {
                            activation: if config.enable_bootstrap
                                && self.output_catalogue.is_some()
                            {
                                ActivationCoupling::RequiresRunning
                            } else {
                                ActivationCoupling::Independent
                            },
                            ..Default::default()
                        },
                        pipe: DesiredPipe::Ranked(RankedInputPipeConfig {
                            queue: input_queue_id.clone(),
                            capacity: input_capacity,
                            source_rank,
                            source_id: None,
                            source_dispatch: None,
                            drop_when_full: false,
                        }),
                    });
                    continue;
                }
                let declaration = self.sources.get(&setting.source_id);
                let supplied = declaration.and_then(|(host, _)| host.as_ref());
                let options = declaration
                    .map(|(_, options)| options.clone())
                    .unwrap_or_default();
                let source_id = authorize_helper(
                    &mut ids,
                    &query_id,
                    Self::source_component_id(&config.id, &setting.source_id)?,
                )?;
                let stream = StreamId::try_new(format!("{source_id}/out"))?;
                let options = SourceSubscriptionOptions {
                    enable_bootstrap: setting.enable_bootstrap,
                    nodes: setting.nodes,
                    relations: setting.relations,
                    bootstrap_timeout_secs: config.bootstrap_timeout_secs,
                    ..options
                };
                let subscription_id = ResourceId::try_new(format!(
                    "subscription/{}/{}",
                    encoded(config.id.as_str()),
                    encoded(setting.source_id.as_str())
                ))?;
                builder = builder
                    .declare_resource(ResourceSpecification {
                        id: subscription_id.clone(),
                        role: ResourceRole::SourceSubscription,
                        ownership: ResourceOwnership::Graph,
                        binding: Arc::from(format!("subscription:{source_id}")),
                    })?
                    .resource_dependency(
                        subscription_id.clone(),
                        Self::source_resource_id(&setting.source_id)?,
                        ResourceRole::LegacySource,
                    )?
                    .resource_dependency(
                        subscription_id.clone(),
                        progress_id.clone(),
                        ResourceRole::Checkpoint,
                    )?;
                builder = builder.construct_resource(
                    subscription_id.clone(),
                    Arc::new(SubscriptionRecipe {
                        source: Self::source_resource_id(&setting.source_id)?,
                        progress: progress_id.clone(),
                        options,
                        stream: stream.clone(),
                        query_id: config.id.clone(),
                    }),
                )?;
                subscription_ids.push(subscription_id.clone());
                let descriptor = ComponentDescriptor::try_new(
                    source_id.clone(),
                    vec![PortDescriptor::new(
                        PortId::try_new("out")?,
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?;
                let mut dependencies: BTreeMap<_, _> = [
                    (
                        Arc::from("source"),
                        vec![Self::source_resource_id(&setting.source_id)?],
                    ),
                    (Arc::from("subscription"), vec![subscription_id]),
                    (Arc::from("progress"), vec![progress_id.clone()]),
                    (Arc::from("catalog"), vec![catalog_id.clone()]),
                ]
                .into_iter()
                .chain(
                    supplied
                        .into_iter()
                        .flat_map(|host| host.services().dependencies()),
                )
                .collect();
                if supplied
                    .map(|host| host.bootstrap_resource())
                    .transpose()
                    .map_err(|error| invalid(format!("{error:#}")))?
                    .flatten()
                    .is_some()
                {
                    dependencies.insert(
                        Arc::from("bootstrap"),
                        vec![resource("source-bootstrap", &setting.source_id)?],
                    );
                }
                builder = builder
                    .component(
                        ComponentName::try_new(format!(
                            "__internal/{query_id}/input/{source_rank}"
                        ))?,
                        ComponentSpecification {
                            descriptor,
                            role: ComponentRole::Source,
                            completion: None,
                            implementation: source_factory.descriptor().implementation.clone(),
                            configuration_version: 1,
                            configuration: BTreeMap::from([
                                (
                                    Arc::from("stream"),
                                    ConfigurationValue::Literal(stream.as_str().into()),
                                ),
                                (
                                    Arc::from("buffer_before_ready"),
                                    ConfigurationValue::Literal(true.into()),
                                ),
                            ]),
                            dependencies,
                        },
                        source_factory.clone(),
                    )
                    .lifecycle_policy(
                        source_id.clone(),
                        LifecyclePolicy {
                            auto_start: config.auto_start,
                        },
                    )
                    .bind_stream(endpoint(source_id.clone(), "out"), stream);
                let edge = EdgeDefinition::new(
                    endpoint(source_id, "out"),
                    endpoint(query_id.clone(), "in"),
                );
                builder = builder
                    .connect(
                        edge.clone(),
                        Box::new(RankedInputPipeConfig {
                            queue: input_queue_id.clone(),
                            capacity: input_capacity,
                            source_rank,
                            source_id: Some(setting.source_id.clone()),
                            source_dispatch: Some(Self::source_resource_id(&setting.source_id)?),
                            drop_when_full: false,
                        }),
                    )
                    .relationship_policy(
                        edge,
                        RelationshipPolicy {
                            activation: ActivationCoupling::RequiresRunning,
                            rebind_on_consumer_replace: true,
                            propagate_failure: true,
                            fence_producer_on_failure: true,
                            ..Default::default()
                        },
                    );
            }
            let scheduled = authorize_helper(
                &mut ids,
                &query_id,
                ComponentId::derive_owned(&query_id, "query-scheduled")?,
            )?;
            let scheduled_stream = StreamId::try_new(format!("{scheduled}/out"))?;
            let scheduled_factory = builtin_factory::<QueryScheduledSourceFactory>();
            let scheduled_edge = EdgeDefinition::new(
                endpoint(scheduled.clone(), "out"),
                endpoint(query_id.clone(), "in"),
            );
            builder = builder
                .component(
                    ComponentName::try_new(format!("__internal/{query_id}/scheduled"))?,
                    ComponentSpecification {
                        descriptor: ComponentDescriptor::try_new(
                            scheduled.clone(),
                            vec![PortDescriptor::new(
                                PortId::try_new("out")?,
                                PortDirection::Output,
                                GraphChangeCodec::schema().descriptor().clone(),
                                PipeRequirements::default(),
                            )],
                        )?,
                        role: ComponentRole::Source,
                        completion: None,
                        implementation: scheduled_factory.descriptor().implementation.clone(),
                        configuration_version: 1,
                        configuration: BTreeMap::from([(
                            Arc::from("stream"),
                            ConfigurationValue::Literal(scheduled_stream.as_str().into()),
                        )]),
                        dependencies: BTreeMap::from([
                            (Arc::from("scheduling"), vec![scheduling_id.clone()]),
                            (Arc::from("catalog"), vec![catalog_id.clone()]),
                        ]),
                    },
                    scheduled_factory,
                )
                .lifecycle_policy(
                    scheduled.clone(),
                    LifecyclePolicy {
                        auto_start: config.auto_start,
                    },
                )
                .bind_stream(endpoint(scheduled, "out"), scheduled_stream)
                .connect(
                    scheduled_edge.clone(),
                    Box::new(RankedInputPipeConfig {
                        queue: input_queue_id,
                        capacity: input_capacity,
                        source_rank: scheduled_rank,
                        source_id: None,
                        source_dispatch: None,
                        drop_when_full: false,
                    }),
                )
                .relationship_policy(
                    scheduled_edge,
                    RelationshipPolicy {
                        activation: ActivationCoupling::RequiresRunning,
                        rebind_on_consumer_replace: true,
                        propagate_failure: true,
                        fence_producer_on_failure: true,
                        ..Default::default()
                    },
                );
            let bootstrap = self.bootstraps.get(&config.id);
            let generic_outputs: Vec<_> = config
                .inputs
                .iter()
                .filter_map(|input| {
                    self.native_sources
                        .get(input.component.as_id()?)
                        .and_then(Option::as_ref)
                })
                .map(|output| OutputEndpoint::new(output.component.clone(), output.port.clone()))
                .collect();
            let generic =
                if bootstrap.is_none() && config.enable_bootstrap && !generic_outputs.is_empty() {
                    self.output_catalogue
                        .as_ref()
                        .map(|(_, catalogue)| {
                            super::output_bootstrap::OutputBootstrapProvider::new(
                                catalogue.clone(),
                                generic_outputs,
                                progress.clone(),
                                std::time::Duration::from_secs(config.bootstrap_timeout_secs),
                            )
                            .map(|provider| provider as Arc<dyn ComputationBootstrapProvider>)
                        })
                        .transpose()
                        .map_err(|error| invalid(format!("{error:#}")))?
                } else {
                    None
                };
            let bootstrap_id =
                if bootstrap.is_some() || generic.is_some() || !subscription_ids.is_empty() {
                    let id = resource("query-bootstrap", &config.id)?;
                    builder = builder.declare_resource(ResourceSpecification {
                        id: id.clone(),
                        role: ResourceRole::Bootstrap,
                        ownership: ResourceOwnership::Graph,
                        binding: Arc::from(format!("query-bootstrap:{}", config.id)),
                    })?;
                    Some(id)
                } else {
                    None
                };
            for subscription in &subscription_ids {
                builder = builder.resource_dependency(
                    bootstrap_id
                        .clone()
                        .expect("subscription bootstrap resource"),
                    subscription.clone(),
                    ResourceRole::SourceSubscription,
                )?;
            }
            if let Some(bootstrap) = bootstrap {
                builder = builder.provide_resource(
                    bootstrap_id.clone().expect("explicit bootstrap resource"),
                    ResourceHandle::new(
                        ResourceRole::Bootstrap,
                        Arc::new(QueryBootstrapResource(bootstrap.clone())),
                    ),
                )?;
            } else if let Some(bootstrap_id) = &bootstrap_id {
                let recipe = BootstrapRecipe {
                    subscriptions: subscription_ids,
                    output: generic,
                    scope: self
                        .output_catalogue
                        .as_ref()
                        .map_or_else(|| self.graph_id.clone(), |(scope, _)| scope.clone()),
                    graph: self.graph_id.clone(),
                    query: query_id.clone(),
                };
                builder = builder.construct_resource(bootstrap_id.clone(), Arc::new(recipe))?;
            }
            let indexes_id = Self::index_resource_id(&config.id)?;
            let (handle, publication, ownership) =
                if let Some((provider, publication)) = self.overrides.get(&config.id) {
                    (
                        ResourceHandle::new(
                            ResourceRole::IndexBackend,
                            Arc::new(QueryIndexProviderResource(provider.clone())),
                        ),
                        *publication,
                        ResourceOwnership::Borrowed,
                    )
                } else {
                    let (backend, volatile, durability) = self
                        .indexes
                        .computation_backend(config.storage_backend.as_ref())
                        .map_err(|error| invalid(error.to_string()))?;
                    if matches!(
                        backend,
                        StorageBackendRef::Inline(StorageBackendSpec::Memory {
                            enable_archive: false
                        })
                    ) {
                        (
                            ResourceHandle::new(
                                ResourceRole::IndexBackend,
                                Arc::new(QueryIndexProviderResource(Arc::new(MemoryQueryProvider))),
                            ),
                            QueryPublicationMode::NonAtomic,
                            ResourceOwnership::Borrowed,
                        )
                    } else {
                        let atomic_output = self
                            .indexes
                            .configured_provider(Some(&backend))
                            .is_some_and(|(_, provider)| provider.supports_atomic_query_output());
                        let provider = LegacyIndexProviderAdapter::scoped(
                            Arc::new(FactoryProvider {
                                factory: self.indexes.clone(),
                                backend,
                                volatile,
                                atomic_output,
                                durability,
                            }),
                            self.services.scope.clone(),
                        )?;
                        (
                            provider.resource(),
                            if atomic_output {
                                QueryPublicationMode::Atomic
                            } else {
                                QueryPublicationMode::NonAtomic
                            },
                            ResourceOwnership::Graph,
                        )
                    }
                };
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: indexes_id.clone(),
                    role: ResourceRole::IndexBackend,
                    ownership,
                    binding: Arc::from(format!("query-indexes:{}", config.id)),
                })?
                .provide_resource(indexes_id.clone(), handle)?;
            let stream = StreamId::try_new(format!("{}/out", config.id))?;
            let definition = ContinuousQueryDefinition {
                graph_id: self.graph_id.clone(),
                id: query_id.clone(),
                query: config.query.clone(),
                language: match config.query_language {
                    QueryLanguage::Cypher => ComputationQueryLanguage::Cypher,
                    QueryLanguage::GQL => ComputationQueryLanguage::Gql,
                },
                output_stream: stream.clone(),
                outbox_capacity: std::num::NonZeroUsize::new(
                    config.outbox_capacity.clamp(1, 1_000_000),
                )
                .expect("bounded capacity"),
            };
            let recovery = config.recovery_policy.unwrap_or(self.default_recovery);
            let descriptor = query_authoring
                .expect("query authoring was required")
                .query_descriptor(query_id.clone())
                .map_err(|error| invalid(format!("{error:#}")))?;
            if descriptor.id() != &query_id {
                return Err(invalid(
                    "query factory substituted its assigned UUID".into(),
                ));
            }
            builder = builder
                .component(
                    config.name.clone(),
                    ComponentSpecification {
                        descriptor,
                        role: ComponentRole::Transformer,
                        completion: None,
                        implementation: query_factory
                            .as_ref()
                            .expect("query factory was required")
                            .descriptor()
                            .implementation
                            .clone(),
                        configuration_version: 1,
                        configuration: BTreeMap::from([
                            (
                                Arc::from("defer_build"),
                                ConfigurationValue::Literal(true.into()),
                            ),
                            (
                                Arc::from("query"),
                                ConfigurationValue::Literal(config.query.clone().into()),
                            ),
                            (
                                Arc::from("stream"),
                                ConfigurationValue::Literal(stream.as_str().into()),
                            ),
                            (
                                Arc::from("language"),
                                ConfigurationValue::Literal(
                                    if definition.language == ComputationQueryLanguage::Gql {
                                        "gql"
                                    } else {
                                        "cypher"
                                    }
                                    .into(),
                                ),
                            ),
                            (
                                Arc::from("outbox_capacity"),
                                ConfigurationValue::Literal(
                                    definition.outbox_capacity.get().into(),
                                ),
                            ),
                            (
                                Arc::from("recovery"),
                                ConfigurationValue::Literal(
                                    if recovery == RecoveryPolicy::AutoReset {
                                        "auto_reset"
                                    } else {
                                        "strict"
                                    }
                                    .into(),
                                ),
                            ),
                            (
                                Arc::from("publication"),
                                ConfigurationValue::Literal(
                                    if publication == QueryPublicationMode::Atomic {
                                        "atomic"
                                    } else {
                                        "non_atomic"
                                    }
                                    .into(),
                                ),
                            ),
                            (
                                Arc::from("execution"),
                                ConfigurationValue::Literal(
                                    serde_json::to_value(execution)
                                        .map_err(|error| invalid(error.to_string()))?,
                                ),
                            ),
                            (
                                Arc::from("query_config"),
                                ConfigurationValue::Literal(
                                    serde_json::to_value(config)
                                        .map_err(|error| invalid(error.to_string()))?,
                                ),
                            ),
                        ]),
                        dependencies: [
                            (Arc::from("indexes"), vec![indexes_id]),
                            (Arc::from("scheduling"), vec![scheduling_id]),
                            (Arc::from("source_progress"), vec![progress_id]),
                            (Arc::from("middleware"), vec![middleware_id.clone()]),
                            (Arc::from("catalog"), vec![catalog_id.clone()]),
                        ]
                        .into_iter()
                        .chain(bootstrap_id.map(|id| (Arc::from("bootstrap"), vec![id])))
                        .collect(),
                    },
                    query_factory
                        .as_ref()
                        .expect("query factory was required")
                        .clone(),
                )
                .lifecycle_policy(
                    query_id.clone(),
                    LifecyclePolicy {
                        auto_start: config.auto_start,
                    },
                )
                .bind_stream(endpoint(query_id.clone(), "out"), stream)
                .connect(
                    EdgeDefinition::new(
                        endpoint(query_id, "out"),
                        endpoint(outlet_id.clone(), "in"),
                    ),
                    Box::new(BoundedPipeConfig {
                        capacity: config
                            .dispatch_buffer_capacity
                            .unwrap_or(self.output_capacity),
                    }),
                );
        }
        let reaction_factory = builtin_factory::<ReactionPluginAdapterFactory>();
        for (name, host, auto_start) in self.reactions {
            let id = host.id().clone();
            let host_id = resource("reaction-host", host.id())?;
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: host_id.clone(),
                    role: ResourceRole::LegacyReaction,
                    ownership: if host.is_owned() {
                        ResourceOwnership::Graph
                    } else {
                        ResourceOwnership::Borrowed
                    },
                    binding: Arc::from(host_id.as_str()),
                })?
                .provide_resource(host_id.clone(), host.resource())?;
            builder = builder
                .component(
                    name,
                    ComponentSpecification {
                        descriptor: ComponentDescriptor::try_new(
                            id.clone(),
                            vec![PortDescriptor::new(
                                PortId::try_new("in")?,
                                PortDirection::Input,
                                QueryChangeCodec::schema().descriptor().clone(),
                                PipeRequirements::default(),
                            )],
                        )?,
                        role: ComponentRole::Sink,
                        completion: Some(SinkCompletion::Accepted),
                        implementation: reaction_factory.descriptor().implementation.clone(),
                        configuration_version: 1,
                        configuration: BTreeMap::new(),
                        dependencies: [
                            (Arc::from("reaction"), vec![host_id]),
                            (Arc::from("catalog"), vec![catalog_id.clone()]),
                        ]
                        .into_iter()
                        .chain(host.services().dependencies())
                        .collect(),
                    },
                    reaction_factory.clone(),
                )
                .lifecycle_policy(id.clone(), LifecyclePolicy { auto_start });
            for query in host.query_ids() {
                let config = self
                    .queries
                    .iter()
                    .find(|config| &config.id == query)
                    .expect("validated query");
                let edge = EdgeDefinition::new(
                    endpoint(ComponentId::try_new(query.as_str())?, "out"),
                    endpoint(id.clone(), "in"),
                );
                let capacity = config
                    .dispatch_buffer_capacity
                    .unwrap_or(self.output_capacity);
                builder = if config.dispatch_mode == Some(crate::DispatchMode::Broadcast) {
                    builder.connect(
                        edge.clone(),
                        Box::new(BroadcastPipeConfig {
                            capacity,
                            lag_policy: BroadcastLagPolicy::SkipWithNotification,
                        }),
                    )
                } else {
                    builder.connect(edge.clone(), Box::new(BoundedPipeConfig { capacity }))
                };
                if host.is_owned() {
                    builder = builder.relationship_policy(
                        edge,
                        RelationshipPolicy {
                            activation: ActivationCoupling::RequiresRunning,
                            ..Default::default()
                        },
                    );
                }
            }
        }
        let graph = builder.build()?;
        for (name, count) in subscribers {
            if let Some((Some(host), _)) = self.sources.get(&name) {
                host.expected_subscriptions(count.max(1));
            }
        }
        let mut batch = graph.into_component_batch()?;
        batch
            .definition
            .boundary_relationships
            .extend(native_inputs);
        Ok(batch)
    }
}
