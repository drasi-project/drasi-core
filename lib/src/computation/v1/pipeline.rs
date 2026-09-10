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
    config::{QueryConfig, QueryLanguage},
    indexes::{IndexFactory, StorageBackendRef},
    RecoveryPolicy,
};
use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
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
}
#[async_trait]
impl IndexBackendPlugin for FactoryProvider {
    async fn create_indexes(&self, query: &str) -> std::result::Result<CreatedIndexes, IndexError> {
        self.factory
            .build(&self.backend, query)
            .await
            .map_err(IndexError::other)
    }
    fn is_volatile(&self) -> bool {
        self.volatile
    }
}

/// Translates the existing query configuration and plugin contracts into a
/// native graph. Legacy managers never execute these queries or forward results.
pub struct ComputationPipelineBuilder {
    graph_id: String,
    services: LegacyPluginServices,
    catalog: QueryResultsCatalog,
    middleware: Arc<MiddlewareTypeRegistry>,
    indexes: Arc<IndexFactory>,
    default_recovery: RecoveryPolicy,
    input_capacity: usize,
    output_capacity: usize,
    sources: BTreeMap<String, (Arc<SourcePluginHost>, SourceSubscriptionOptions)>,
    queries: Vec<QueryConfig>,
    reactions: Vec<(Arc<ReactionPluginHost>, bool)>,
    overrides: BTreeMap<String, (Arc<dyn ComputationIndexProvider>, QueryPublicationMode)>,
}

fn encoded(value: &str) -> String {
    value.bytes().map(|value| format!("{value:02x}")).collect()
}
fn resource(kind: &str, id: &str) -> GraphResult<ResourceId> {
    Ok(ResourceId::try_new(format!("{kind}/{}", encoded(id)))?)
}
fn endpoint(component: ComponentId, port: &str) -> Endpoint {
    Endpoint::new(component, PortId::try_new(port).expect("constant port"))
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
            indexes,
            default_recovery,
            input_capacity,
            output_capacity,
            sources: BTreeMap::new(),
            queries: Vec::new(),
            reactions: Vec::new(),
            overrides: BTreeMap::new(),
        })
    }
    pub fn catalog(&self) -> QueryResultsCatalog {
        self.catalog.clone()
    }
    pub fn services(&self) -> LegacyPluginServices {
        self.services.clone()
    }
    pub fn source_component_id(query: &str, source: &str) -> Result<ComponentId> {
        ComponentId::try_new(format!("source/{}/{}", encoded(query), encoded(source)))
    }
    pub fn reaction_component_id(reaction: &str) -> Result<ComponentId> {
        ComponentId::try_new(format!("reaction/{}", encoded(reaction)))
    }
    pub fn source(
        mut self,
        host: Arc<SourcePluginHost>,
        options: SourceSubscriptionOptions,
    ) -> GraphResult<Self> {
        if self
            .sources
            .insert(host.id().to_owned(), (host, options))
            .is_some()
        {
            return Err(GraphError::Topology {
                reason: "duplicate compatibility source".into(),
            });
        }
        Ok(self)
    }
    pub fn query(mut self, config: QueryConfig) -> Self {
        self.queries.push(config);
        self
    }
    pub fn reaction(mut self, host: Arc<ReactionPluginHost>, auto_start: bool) -> Self {
        self.reactions.push((host, auto_start));
        self
    }
    pub fn middleware_registry(mut self, registry: Arc<MiddlewareTypeRegistry>) -> Self {
        self.middleware = registry;
        self
    }
    pub fn query_provider(
        mut self,
        query: &str,
        provider: Arc<dyn ComputationIndexProvider>,
        publication: QueryPublicationMode,
    ) -> GraphResult<Self> {
        if self
            .overrides
            .insert(query.to_owned(), (provider, publication))
            .is_some()
        {
            return Err(GraphError::Topology {
                reason: "duplicate query provider override".into(),
            });
        }
        Ok(self)
    }
    pub fn build(self) -> GraphResult<ComputationGraph> {
        let invalid = |reason: String| GraphError::Topology { reason };
        let mut query_ids = BTreeSet::new();
        let mut prepared = Vec::new();
        let mut subscribers: BTreeMap<String, usize> = BTreeMap::new();
        for (host, _) in self.sources.values() {
            if host.is_owned() && !host.services().same_services(&self.services) {
                return Err(invalid(
                    "owned source must use this pipeline's scoped services".into(),
                ));
            }
        }
        for query in &self.queries {
            let id = ComponentId::try_new(query.id.as_str())?;
            if !query_ids.insert(query.id.clone()) {
                return Err(invalid("duplicate compatibility query".into()));
            }
            let execution = QueryExecutionSettings::from_legacy_config(query);
            execution
                .validate(Some(&self.middleware))
                .map_err(|error| invalid(format!("{error:#}")))?;
            let settings = QueryExecutionSettings::legacy_subscriptions(query)
                .map_err(|error| invalid(format!("{error:#}")))?;
            for setting in &settings {
                if !self.sources.contains_key(&setting.source_id) {
                    return Err(invalid(format!("unknown source {}", setting.source_id)));
                }
                if query.auto_start {
                    *subscribers.entry(setting.source_id.clone()).or_default() += 1;
                }
            }
            if settings.is_empty() {
                return Err(invalid("compatibility query requires sources".into()));
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
        let mut reaction_ids = BTreeSet::new();
        for (host, _) in &self.reactions {
            if !reaction_ids.insert(host.id()) {
                return Err(invalid("duplicate compatibility reaction".into()));
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
        let catalog_id = ResourceId::try_new("query-catalog")?;
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
        let middleware_id = ResourceId::try_new("query-middleware")?;
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
            let id = resource("source-host", name)?;
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: id.clone(),
                    role: ResourceRole::LegacySource,
                    ownership: if host.is_owned() {
                        ResourceOwnership::Graph
                    } else {
                        ResourceOwnership::Borrowed
                    },
                    binding: Arc::from(name.as_str()),
                })?
                .provide_resource(id, host.resource())?;
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
        let source_factory = Arc::new(SourcePluginAdapterFactory::default());
        let query_factory = Arc::new(ContinuousQueryFactory::default());
        let outlet_id = ComponentId::try_new("__computation_results__")?;
        let outlet_factory = Arc::new(QueryResultsOutletFactory::default());
        builder = builder.component(
            ComponentSpecification {
                descriptor: QueryResultsOutlet::new(outlet_id.clone(), self.catalog.clone())
                    .descriptor()
                    .clone(),
                role: ComponentRole::Sink,
                completion: Some(SinkCompletion::Handled),
                implementation: outlet_factory.descriptor().implementation.clone(),
                configuration_version: 1,
                configuration: BTreeMap::new(),
                dependencies: BTreeMap::from([(Arc::from("catalog"), vec![catalog_id.clone()])]),
            },
            outlet_factory,
        );
        for (config, (query_id, execution, settings)) in self.queries.iter().zip(prepared) {
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
            let mut subscriptions = Vec::new();
            for setting in settings {
                let (host, options) = &self.sources[&setting.source_id];
                let source_id = Self::source_component_id(&config.id, &setting.source_id)?;
                let stream = StreamId::try_new(format!("{source_id}/out"))?;
                let options = SourceSubscriptionOptions {
                    enable_bootstrap: setting.enable_bootstrap,
                    nodes: setting.nodes,
                    relations: setting.relations,
                    bootstrap_timeout_secs: config.bootstrap_timeout_secs,
                    ..options.clone()
                };
                let subscription = LegacySourceSubscription::new(
                    host.clone(),
                    options,
                    stream.clone(),
                    Some(progress.clone()),
                )
                .map_err(|error| invalid(format!("{error:#}")))?;
                let subscription_id = ResourceId::try_new(format!(
                    "subscription/{}/{}",
                    encoded(&config.id),
                    encoded(&setting.source_id)
                ))?;
                builder = builder
                    .declare_resource(ResourceSpecification {
                        id: subscription_id.clone(),
                        role: ResourceRole::SourceSubscription,
                        ownership: ResourceOwnership::Graph,
                        binding: Arc::from(format!("subscription:{source_id}")),
                    })?
                    .provide_resource(
                        subscription_id.clone(),
                        ResourceHandle::new(ResourceRole::SourceSubscription, subscription.clone())
                            .with_cleanup(subscription.clone()),
                    )?;
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
                        vec![resource("source-host", &setting.source_id)?],
                    ),
                    (Arc::from("subscription"), vec![subscription_id]),
                    (Arc::from("progress"), vec![progress_id.clone()]),
                ]
                .into_iter()
                .chain(host.services().dependencies())
                .collect();
                if host
                    .bootstrap_resource()
                    .map_err(|error| invalid(format!("{error:#}")))?
                    .is_some()
                {
                    dependencies.insert(
                        Arc::from("bootstrap"),
                        vec![resource("source-bootstrap", &setting.source_id)?],
                    );
                }
                builder = builder
                    .component(
                        ComponentSpecification {
                            descriptor,
                            role: ComponentRole::Source,
                            completion: None,
                            implementation: source_factory.descriptor().implementation.clone(),
                            configuration_version: 1,
                            configuration: BTreeMap::from([(
                                Arc::from("stream"),
                                ConfigurationValue::Literal(stream.as_str().into()),
                            )]),
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
                        Box::new(BoundedPipeConfig {
                            capacity: config
                                .priority_queue_capacity
                                .unwrap_or(self.input_capacity),
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
                subscriptions.push(subscription);
            }
            let bootstrap_id = resource("query-bootstrap", &config.id)?;
            let bootstrap = LegacySourceBootstrap::new(subscriptions);
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: bootstrap_id.clone(),
                    role: ResourceRole::Bootstrap,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from(format!("query-bootstrap:{}", config.id)),
                })?
                .provide_resource(
                    bootstrap_id.clone(),
                    ResourceHandle::new(
                        ResourceRole::Bootstrap,
                        Arc::new(QueryBootstrapResource(bootstrap)),
                    ),
                )?;
            let indexes_id = resource("query-indexes", &config.id)?;
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
                    let (backend, volatile) = self
                        .indexes
                        .computation_backend(config.storage_backend.as_ref())
                        .map_err(|error| invalid(error.to_string()))?;
                    let provider = LegacyIndexProviderAdapter::scoped(
                        Arc::new(FactoryProvider {
                            factory: self.indexes.clone(),
                            backend,
                            volatile,
                        }),
                        self.services.scope.clone(),
                    )?;
                    (
                        provider.resource(),
                        QueryPublicationMode::NonAtomic,
                        ResourceOwnership::Graph,
                    )
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
            builder = builder
                .component(
                    ComponentSpecification {
                        descriptor: definition.descriptor(),
                        role: ComponentRole::Query,
                        completion: None,
                        implementation: query_factory.descriptor().implementation.clone(),
                        configuration_version: 1,
                        configuration: BTreeMap::from([
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
                        ]),
                        dependencies: BTreeMap::from([
                            (Arc::from("indexes"), vec![indexes_id]),
                            (Arc::from("bootstrap"), vec![bootstrap_id]),
                            (Arc::from("source_progress"), vec![progress_id]),
                            (Arc::from("middleware"), vec![middleware_id.clone()]),
                            (Arc::from("catalog"), vec![catalog_id.clone()]),
                        ]),
                    },
                    query_factory.clone(),
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
        let reaction_factory = Arc::new(ReactionPluginAdapterFactory::default());
        for (host, auto_start) in self.reactions {
            let id = Self::reaction_component_id(host.id())?;
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
                    binding: Arc::from(host.id()),
                })?
                .provide_resource(host_id.clone(), host.resource())?;
            builder = builder
                .component(
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
            self.sources[&name].0.expected_subscriptions(count.max(1));
        }
        Ok(graph)
    }
}
