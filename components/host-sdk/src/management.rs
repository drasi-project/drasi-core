// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Resource recipes for integrated DrasiLib management. Library loading and
//! factory registration use PluginRegistry; no second component registry exists.

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_lib::{
    computation::v1::*, management::ManagementResourceResolver, secret_store::SecretStoreProvider,
};
use serde::{Deserialize, Serialize};

mod native;
pub use native::NativeBootstrapConfig;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum Recipe {
    MemoryIndexes,
    QueryCatalog {},
    SourceProgress {
        component: ComponentId,
    },
    Indexes {
        provider: String,
    },
    SharedStorage {
        provider: String,
        component: ComponentId,
    },
    SharedQos(SharedQosConfig),
    NativeConsumer(NativeConsumerConfig),
    NativeBootstrap(NativeBootstrapConfig),
    Middleware,
    QueryMiddleware,
    TransactionalTransformers,
    Qos {
        definition: QosChannelDefinition,
        provider: Option<String>,
        recovery: Option<QosRecoveryConfig>,
    },
    Configuration {
        #[serde(default)]
        secrets: BTreeMap<String, String>,
    },
}

/// Host configuration cannot supply a different construction scope or graph.
/// These identities come from the actual owning instance during resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum QosRecoveryConfig {
    Admission {
        component: ComponentId,
        failure_scope: drasi_core::interface::FailureMode,
        max_producers: NonZeroUsize,
        receipts_per_producer: NonZeroUsize,
    },
    Replay {
        failure_scope: drasi_core::interface::FailureMode,
        receipt_capacity: NonZeroUsize,
    },
}

impl QosRecoveryConfig {
    pub fn options(&self, instance: &str, graph: &str) -> QosRecoveryOptions {
        match self {
            Self::Admission {
                component,
                failure_scope,
                max_producers,
                receipts_per_producer,
            } => QosRecoveryOptions::Admission(AdmissionOptions {
                construction_scope: instance.into(),
                graph_id: graph.into(),
                component_id: component.clone(),
                failure_scope: *failure_scope,
                max_producers: *max_producers,
                receipts_per_producer: *receipts_per_producer,
            }),
            Self::Replay {
                failure_scope,
                receipt_capacity,
            } => QosRecoveryOptions::Replay(ReplayOptions {
                failure_scope: *failure_scope,
                receipt_capacity: *receipt_capacity,
            }),
        }
    }
}

/// A journal constructed from the actual declared group, never another open of
/// its backing storage. The graph retains that group until journal cleanup ends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SharedQosConfig {
    pub definition: QosChannelDefinition,
    pub recovery: QosRecoveryConfig,
}

impl SharedQosConfig {
    pub fn validate(&self, specification: &ResourceSpecification) -> Result<ReplayOptions> {
        anyhow::ensure!(
            specification.role == ResourceRole::StateStore
                && specification.ownership == ResourceOwnership::Graph,
            "shared QoS requires a graph-owned state-store declaration"
        );
        let QosRecoveryConfig::Replay {
            failure_scope,
            receipt_capacity,
        } = &self.recovery
        else {
            anyhow::bail!("shared QoS requires output replay, not client admission");
        };
        let replay = ReplayOptions {
            failure_scope: *failure_scope,
            receipt_capacity: *receipt_capacity,
        };
        QosRecoveryOptions::Replay(replay.clone()).validate(&self.definition)?;
        Ok(replay)
    }

    pub async fn resolve(
        &self,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
        factories: &FactoryRegistry,
    ) -> Result<ResourceHandle> {
        let replay = self.validate(specification)?;
        anyhow::ensure!(
            dependencies.len() == 1,
            "shared QoS requires exactly its declared storage group"
        );
        let resource = dependencies
            .values()
            .next()
            .context("shared QoS storage dependency is unavailable")?;
        anyhow::ensure!(
            resource.role() == ResourceRole::IndexBackend,
            "shared QoS storage dependency has the wrong role"
        );
        let provider = resource.get::<QueryIndexProviderResource>()?;
        let group = provider
            .0
            .transaction_group()
            .context("shared QoS requires an actual shared transaction group")?;
        Ok(QosChannel::shared(
            self.definition.clone(),
            group,
            specification.id.as_str(),
            factories
                .envelope_codec(NonZeroUsize::new(64 * 1024 * 1024).expect("constant size"))?,
            replay,
        )
        .await?
        .resource())
    }
}

/// Standard host-owned recipes with registered index providers. Supply a custom
/// ManagementResourceResolver for other recipe kinds; backend implementation
/// dependencies do not belong in drasi-lib.
pub struct HostManagementResources {
    middleware: Arc<drasi_core::middleware::MiddlewareTypeRegistry>,
    transactional: Arc<TransactionalTransformerRegistry>,
    secrets: Option<Arc<dyn SecretStoreProvider>>,
    factories: FactoryRegistry,
    bootstraps: crate::computation::NativeBootstrapFactories,
    indexes: BTreeMap<String, Arc<dyn drasi_core::computation::ComputationIndexProvider>>,
}

/// Durable consumer settings; the actual provider is a declared graph dependency.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeConsumerConfig {
    pub failure_scope: drasi_core::interface::FailureMode,
    pub max_streams: NonZeroUsize,
    pub receipts_per_stream: NonZeroUsize,
    #[serde(default)]
    pub retry: DeliveryRetryPolicy,
}
impl NativeConsumerConfig {
    pub fn options(&self) -> DeliveryOptions {
        DeliveryOptions {
            scope: RecoveryScope::Failure(self.failure_scope),
            max_streams: self.max_streams,
            receipts_per_stream: self.receipts_per_stream,
            retry: self.retry.clone(),
        }
    }
    pub fn validate(&self, specification: &ResourceSpecification) -> Result<()> {
        anyhow::ensure!(
            specification.role == ResourceRole::IndexBackend
                && specification.ownership == ResourceOwnership::Graph,
            "native consumer requires a graph-owned index-backend declaration"
        );
        self.options().validate()
    }
    pub fn resolve(
        &self,
        instance: &str,
        graph: &str,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<ResourceHandle> {
        self.validate_dependencies(
            specification,
            &dependencies
                .iter()
                .map(|(id, handle)| (id.clone(), handle.role()))
                .collect(),
        )?;
        let dependency = dependencies
            .values()
            .next()
            .context("missing native consumer indexes")?;
        let provider = dependency.get::<QueryIndexProviderResource>()?.0.clone();
        anyhow::ensure!(
            provider.transaction_group().is_none(),
            "native consumer requires its own progress transaction, not a producer's shared group"
        );
        Ok(ResourceHandle::new(
            ResourceRole::IndexBackend,
            Arc::new(crate::computation::NativeConsumerResource {
                provider: native::consumer_provider(instance, graph, provider),
                options: self.options(),
            }),
        ))
    }
}

impl NativeConsumerConfig {
    pub fn validate_dependencies(
        &self,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceRole>,
    ) -> Result<()> {
        self.validate(specification)?;
        anyhow::ensure!(
            dependencies.len() == 1
                && dependencies
                    .values()
                    .all(|role| *role == ResourceRole::IndexBackend),
            "native consumer requires exactly one index-provider dependency"
        );
        Ok(())
    }
}

impl HostManagementResources {
    pub fn new(
        registry: &crate::PluginRegistry,
        middleware: Arc<drasi_core::middleware::MiddlewareTypeRegistry>,
        secrets: Option<Arc<dyn SecretStoreProvider>>,
    ) -> Result<Self> {
        Ok(Self {
            transactional: registry.transactional_transformer_registry(middleware.clone())?,
            middleware,
            secrets,
            factories: registry.computation_factory_registry()?,
            bootstraps: registry.computation_bootstrap_factories()?,
            indexes: BTreeMap::new(),
        })
    }

    /// Register an external index implementation for durable QoS journal recipes.
    pub fn with_index_provider(
        mut self,
        name: impl Into<String>,
        provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
    ) -> Result<Self> {
        let name = name.into();
        ResourceId::try_new(name.clone())?;
        anyhow::ensure!(
            !self.indexes.contains_key(&name),
            "duplicate managed index provider"
        );
        self.indexes.insert(name, provider);
        Ok(self)
    }
}

pub struct HostConfigurationResolver {
    secrets: Option<Arc<dyn SecretStoreProvider>>,
    local: BTreeMap<String, String>,
}

impl HostConfigurationResolver {
    pub fn new(secrets: Option<Arc<dyn SecretStoreProvider>>) -> Self {
        Self {
            secrets,
            local: BTreeMap::new(),
        }
    }
}

fn reference(key: &str) -> Result<(&str, &str)> {
    let (kind, name) = key
        .split_once(':')
        .context("expected env:NAME, env-json:NAME, secret:NAME or secret-json:NAME")?;
    anyhow::ensure!(
        matches!(kind, "env" | "env-json" | "secret" | "secret-json")
            && !name.is_empty()
            && !name.chars().any(char::is_control),
        "invalid configuration reference"
    );
    Ok((kind, name))
}

#[async_trait]
impl ConfigurationResolver for HostConfigurationResolver {
    fn validate_reference(&self, key: &str) -> Result<()> {
        reference(key).map(|_| ())
    }
    async fn resolve(&self, key: &str) -> Result<serde_json::Value> {
        let (kind, name) = reference(key)?;
        let value = if kind.starts_with("secret") {
            match self.local.get(name) {
                Some(value) => value.clone(),
                None => {
                    self.secrets
                        .as_ref()
                        .context("no secret provider supplied")?
                        .get_secret(name)
                        .await?
                }
            }
        } else {
            std::env::var(name).context("environment reference unavailable")?
        };
        if kind.ends_with("-json") {
            serde_json::from_str(&value).context("configuration reference is not valid JSON")
        } else {
            Ok(value.into())
        }
    }
}

#[async_trait]
impl ManagementResourceResolver for HostManagementResources {
    fn validate_transition(
        &self,
        previous: &DesiredTopology,
        desired: &DesiredTopology,
    ) -> Result<()> {
        for resource in &desired.resources {
            let configuration = desired
                .resource_configurations
                .get(&resource.id)
                .context("managed resource recipe is unavailable")?;
            let dependencies = desired
                .resource_dependencies
                .get(&resource.id)
                .cloned()
                .unwrap_or_default();
            match serde_json::from_value(configuration.clone())? {
                Recipe::NativeBootstrap(config) => {
                    config.validate(resource, &dependencies)?;
                    config.validate_owner(&resource.id, desired)?;
                    config.validate_configuration(
                        &self.bootstraps,
                        &dependencies,
                        &BTreeMap::new(),
                    )?;
                }
                Recipe::NativeConsumer(config) => {
                    config.validate_dependencies(resource, &dependencies)?
                }
                _ => {}
            }
        }
        let mut protected = Vec::new();
        for resource in &previous.resources {
            let configuration = previous
                .resource_configurations
                .get(&resource.id)
                .context("managed resource recipe is unavailable")?;
            let recipe: Recipe = serde_json::from_value(configuration.clone())?;
            let persistent = match recipe {
                Recipe::Indexes { provider } => self
                    .indexes
                    .get(&provider)
                    .is_none_or(|provider| !provider.is_volatile()),
                Recipe::SharedStorage { .. } | Recipe::SharedQos(_) | Recipe::NativeConsumer(_) => {
                    true
                }
                Recipe::Qos {
                    provider, recovery, ..
                } => provider.is_some() || recovery.is_some(),
                _ => false,
            };
            if persistent {
                protected.push(resource.id.clone());
            }
        }
        drasi_lib::management::validate_recovery_resource_changes(previous, desired, protected)
    }

    async fn resolve(
        &self,
        instance: &str,
        graph: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
    ) -> Result<ResourceHandle> {
        self.resolve_with_dependencies(
            instance,
            graph,
            specification,
            configuration,
            &BTreeMap::new(),
        )
        .await
    }

    async fn resolve_with_dependencies(
        &self,
        instance: &str,
        graph: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<ResourceHandle> {
        let recipe: Recipe = serde_json::from_value(configuration.clone())
            .context("unsupported host resource recipe")?;
        if let Recipe::SharedQos(config) = recipe {
            return config
                .resolve(specification, dependencies, &self.factories)
                .await;
        }
        if let Recipe::NativeConsumer(config) = recipe {
            return config.resolve(instance, graph, specification, dependencies);
        }
        if let Recipe::NativeBootstrap(config) = recipe {
            return config
                .resolve(
                    instance,
                    graph,
                    specification,
                    dependencies,
                    &self.bootstraps,
                )
                .await;
        }
        anyhow::ensure!(
            dependencies.is_empty(),
            "this host recipe does not accept resource dependencies"
        );
        let resource = match recipe {
            Recipe::MemoryIndexes => ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(QueryIndexProviderResource(Arc::new(
                    drasi_core::computation::InMemoryComputationProvider,
                ))),
            ),
            Recipe::QueryCatalog {} => ResourceHandle::new(
                ResourceRole::QueryCatalog,
                Arc::new(QueryResultsCatalog::new(graph)?),
            ),
            Recipe::SourceProgress { component } => ResourceHandle::new(
                ResourceRole::Checkpoint,
                Arc::new(QuerySourceProgressResource(Arc::new(
                    QuerySourceProgress::new(graph, component)?,
                ))),
            ),
            Recipe::Indexes { provider } => ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(QueryIndexProviderResource(
                    self.indexes
                        .get(&provider)
                        .context("managed index provider is not registered")?
                        .clone(),
                )),
            ),
            Recipe::SharedStorage {
                provider,
                component,
            } => {
                anyhow::ensure!(
                    specification.role == ResourceRole::IndexBackend
                        && specification.ownership == ResourceOwnership::Graph,
                    "shared storage requires a graph-owned index-backend declaration"
                );
                let provider = self
                    .indexes
                    .get(&provider)
                    .context("shared storage index provider is not registered")?;
                let scope = format!("shared/{}/{instance}/{graph}", instance.len());
                let indexes = provider
                    .create_indexes(&scope, specification.id.as_str())
                    .await?;
                SharedStorageGroup::new(graph, component, indexes)?.resource()
            }
            Recipe::SharedQos(_) => unreachable!("shared QoS resolved with dependencies"),
            Recipe::NativeConsumer(_) => unreachable!("native consumer resolved with dependencies"),
            Recipe::NativeBootstrap(_) => {
                unreachable!("native bootstrap resolved with dependencies")
            }
            Recipe::Middleware => ResourceHandle::new(
                ResourceRole::Middleware,
                Arc::new(MiddlewareRegistryResource(self.middleware.clone())),
            ),
            Recipe::QueryMiddleware => ResourceHandle::new(
                ResourceRole::Middleware,
                Arc::new(QueryMiddlewareResource(self.middleware.clone())),
            ),
            Recipe::TransactionalTransformers => ResourceHandle::new(
                ResourceRole::Component,
                Arc::new(TransactionalTransformerRegistryResource(
                    self.transactional.clone(),
                )),
            ),
            Recipe::Qos {
                definition,
                provider,
                recovery,
            } => {
                anyhow::ensure!(
                    specification.role == ResourceRole::StateStore,
                    "QoS recipes require the state-store resource role"
                );
                anyhow::ensure!(
                    specification.ownership == ResourceOwnership::Graph,
                    "host-created QoS resources require graph ownership"
                );
                let recovery = recovery
                    .map(|recovery| recovery.options(instance, graph))
                    .unwrap_or_default();
                recovery.validate(&definition)?;
                let channel = if definition.durable {
                    let provider = provider
                        .as_ref()
                        .and_then(|name| self.indexes.get(name))
                        .context("durable QoS requires a registered index provider")?;
                    anyhow::ensure!(
                        !provider.is_volatile(),
                        "durable QoS cannot use volatile indexes"
                    );
                    let scope = format!("qos/{}/{instance}/{graph}", instance.len());
                    let indexes = provider
                        .create_indexes(&scope, specification.id.as_str())
                        .await?;
                    QosChannel::persistent_with_recovery(
                        definition,
                        indexes,
                        self.factories.envelope_codec(
                            NonZeroUsize::new(64 * 1024 * 1024).expect("constant size"),
                        )?,
                        specification.id.as_str(),
                        recovery,
                    )
                    .await?
                } else {
                    anyhow::ensure!(
                        provider.is_none(),
                        "volatile QoS must not declare durable storage"
                    );
                    QosChannel::volatile(definition)?
                };
                channel.resource()
            }
            Recipe::Configuration { secrets } => ResourceHandle::new(
                ResourceRole::SecretStore,
                Arc::new(ConfigurationResolverResource(Arc::new(
                    HostConfigurationResolver {
                        secrets: self.secrets.clone(),
                        local: secrets,
                    },
                ))),
            ),
        };
        anyhow::ensure!(
            resource.role() == specification.role,
            "resource recipe role differs from declaration"
        );
        Ok(resource)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn persistent_recipes_stay_protected_without_live_handles_and_fast_recipes_remain_editable(
    ) -> Result<()> {
        let resolver = HostManagementResources::new(
            &crate::PluginRegistry::new(),
            Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
            None,
        )?;
        let definition = QosChannelDefinition {
            stream: StreamId::try_new("source/out")?,
            capacity: NonZeroUsize::new(4).expect("capacity"),
            durable: false,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("sink".into(), SubscriptionStart::Earliest)]),
        };
        for (recipe, role, protected) in [
            (
                json!({"kind":"memoryIndexes"}),
                ResourceRole::IndexBackend,
                false,
            ),
            (
                json!({"kind":"indexes","provider":"unavailable"}),
                ResourceRole::IndexBackend,
                true,
            ),
            (
                json!({"kind":"sharedStorage","provider":"unavailable","component":"query"}),
                ResourceRole::IndexBackend,
                true,
            ),
            (
                json!({"kind":"qos","definition":definition}),
                ResourceRole::StateStore,
                false,
            ),
            (
                json!({"kind":"qos","definition":definition,"provider":"unavailable"}),
                ResourceRole::StateStore,
                true,
            ),
        ] {
            let mut previous = drasi_lib::management::DesiredInstance::default().topology;
            let id = ResourceId::try_new("resource")?;
            previous.resources.push(ResourceSpecification {
                id: id.clone(),
                role,
                ownership: ResourceOwnership::Graph,
                binding: "resource".into(),
            });
            previous.resource_configurations.insert(id, recipe);
            resolver.validate_transition(&previous, &previous)?;
            let result = resolver.validate_transition(
                &previous,
                &drasi_lib::management::DesiredInstance::default().topology,
            );
            if protected {
                assert!(result
                    .expect_err("retirement required")
                    .is::<drasi_lib::management::RecoveryTransitionRequired>());
            } else {
                result?;
            }
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shared_recipes_require_actual_group_and_preserve_isolated_journal_identity(
    ) -> Result<()> {
        let directory = tempfile::tempdir()?;
        let resolver = HostManagementResources::new(
            &crate::PluginRegistry::new(),
            Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
            None,
        )?
        .with_index_provider(
            "disk",
            LegacyIndexProviderAdapter::new(Arc::new(
                drasi_index_rocksdb::RocksDbIndexProvider::new(directory.path(), false, false),
            )),
        )?;
        let group = ResourceSpecification {
            id: ResourceId::try_new("group")?,
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Graph,
            binding: "group".into(),
        };
        let journal = ResourceSpecification {
            id: ResourceId::try_new("journal")?,
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: "journal".into(),
        };
        let group_recipe = json!({"kind":"sharedStorage","provider":"disk","component":"query"});
        let definition = QosChannelDefinition {
            stream: StreamId::try_new("query/out")?,
            capacity: NonZeroUsize::new(4).expect("capacity"),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
        };
        let recipe = json!({"kind":"sharedQos","definition":definition,
            "recovery":{"kind":"replay","failureScope":drasi_core::interface::FailureMode::ProcessRestart,"receiptCapacity":8}});
        assert!(resolver
            .resolve("first", "graph", &journal, &recipe)
            .await
            .is_err());
        let first = resolver
            .resolve("first", "graph", &group, &group_recipe)
            .await?;
        let second = resolver
            .resolve("second", "graph", &group, &group_recipe)
            .await?;
        let handle = resolver
            .resolve_with_dependencies(
                "first",
                "graph",
                &journal,
                &recipe,
                &BTreeMap::from([(group.id.clone(), first.clone())]),
            )
            .await?;
        let channel = handle.get::<QosChannel>()?;
        let identity = channel.output_journal_identity();
        assert!(identity.is_some());
        let pipe = definition
            .pipe(journal.id.clone(), "consumer")
            .with_shared_storage(group.id.clone());
        pipe.validate_resources(&BTreeMap::from([
            (group.id.clone(), first.clone()),
            (journal.id.clone(), handle.clone()),
        ]))?;
        assert!(pipe
            .validate_resources(&BTreeMap::from([
                (group.id.clone(), second.clone()),
                (journal.id.clone(), handle)
            ]))
            .is_err());
        let other = resolver
            .resolve_with_dependencies(
                "second",
                "graph",
                &journal,
                &recipe,
                &BTreeMap::from([(group.id.clone(), second.clone())]),
            )
            .await?
            .get::<QosChannel>()?;
        assert_ne!(other.output_journal_identity(), identity);
        channel.shutdown().await?;
        other.shutdown().await?;
        for handle in [first, second] {
            handle
                .get::<QueryIndexProviderResource>()?
                .0
                .transaction_group()
                .context("shared group")?
                .shutdown()
                .await?;
        }
        let reopened = resolver
            .resolve("first", "graph", &group, &group_recipe)
            .await?;
        let restored = resolver
            .resolve_with_dependencies(
                "first",
                "graph",
                &journal,
                &recipe,
                &BTreeMap::from([(group.id.clone(), reopened.clone())]),
            )
            .await?
            .get::<QosChannel>()?;
        assert_eq!(restored.output_journal_identity(), identity);
        restored.shutdown().await?;
        reopened
            .get::<QueryIndexProviderResource>()?
            .0
            .transaction_group()
            .context("shared group")?
            .shutdown()
            .await?;
        Ok(())
    }

    #[test]
    fn recovery_recipes_bind_actual_scope_and_reject_identity_overrides() -> Result<()> {
        let recipe = json!({
            "kind":"admission", "component":"source",
            "failureScope":drasi_core::interface::FailureMode::ProcessRestart,
            "maxProducers":2, "receiptsPerProducer":4
        });
        let config: QosRecoveryConfig = serde_json::from_value(recipe.clone())?;
        assert_eq!(serde_json::to_value(&config)?, recipe);
        let QosRecoveryOptions::Admission(options) =
            config.options("actual-instance", "actual-graph")
        else {
            anyhow::bail!("admission options");
        };
        assert_eq!(options.construction_scope, "actual-instance");
        assert_eq!(options.graph_id, "actual-graph");
        for field in ["constructionScope", "graphId", "persistent", "identity"] {
            let mut forged = recipe.clone();
            forged[field] = json!("forged");
            assert!(serde_json::from_value::<QosRecoveryConfig>(forged).is_err());
        }
        let mut oversized = options;
        oversized.max_producers = NonZeroUsize::new(1025).expect("invalid bound");
        let definition = QosChannelDefinition {
            stream: StreamId::try_new("source/out")?,
            capacity: NonZeroUsize::new(4).expect("capacity"),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
        };
        assert!(QosRecoveryOptions::Admission(oversized)
            .validate(&definition)
            .is_err());
        let mut volatile = definition;
        volatile.durable = false;
        assert!(config
            .options("actual-instance", "actual-graph")
            .validate(&volatile)
            .is_err());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn managed_admission_recipes_restore_receipts_and_isolate_instance_identity() -> Result<()>
    {
        use drasi_core::models::{ElementMetadata, ElementReference, SourceChange};
        let directory = tempfile::tempdir()?;
        let resolver = HostManagementResources::new(
            &crate::PluginRegistry::new(),
            Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
            None,
        )?
        .with_index_provider(
            "disk",
            LegacyIndexProviderAdapter::new(Arc::new(
                drasi_index_rocksdb::RocksDbIndexProvider::new(directory.path(), false, false),
            )),
        )?;
        let specification = ResourceSpecification {
            id: ResourceId::try_new("channel")?,
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: "channel".into(),
        };
        let definition = QosChannelDefinition {
            stream: StreamId::try_new("source/out")?,
            capacity: NonZeroUsize::new(4).expect("capacity"),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
        };
        let mut recipe = json!({
            "kind":"qos", "definition":definition, "provider":"disk",
            "recovery":{"kind":"admission","component":"source",
                "failureScope":drasi_core::interface::FailureMode::ProcessRestart,
                "maxProducers":2,"receiptsPerProducer":4}
        });
        let event = GraphChangeCodec::encode_change(
            SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "one"),
                    labels: Arc::from([Arc::from("Node")]),
                    effective_from: 1,
                },
            },
            definition.stream.clone(),
            1,
            None,
        )?;
        let mut sessions = Vec::new();
        for instance in ["first", "second"] {
            let handle = resolver
                .resolve(instance, "graph", &specification, &recipe)
                .await?;
            let channel = handle.get::<QosChannel>()?;
            let session = channel
                .register_producer(ComponentId::try_new("client")?)
                .await?;
            channel.admit(&session, 1, &event).await?;
            let mut connection = definition
                .pipe(specification.id.clone(), "consumer")
                .create_with_resources(&BTreeMap::from([(specification.id.clone(), handle)]))?;
            let mut receiver = connection.pipe.take_receiver()?;
            let delivery = receiver.receive().await?.expect("admitted data");
            let progress = GraphProducerProgress::from_envelope(delivery.envelope())?
                .expect("producer identity");
            assert_eq!(progress.identity().construction_scope(), instance);
            assert_eq!(progress.identity().graph_id(), "graph");
            sessions.push(session);
            connection.control.cancel();
            channel.shutdown().await?;
        }
        assert_ne!(sessions[0].incarnation, sessions[1].incarnation);
        let handle = resolver
            .resolve("first", "graph", &specification, &recipe)
            .await?;
        let channel = handle.get::<QosChannel>()?;
        assert!(channel.admission_receipt(&sessions[0], 1).await?.is_some());
        assert!(channel.admission_receipt(&sessions[1], 1).await.is_err());
        assert_eq!(channel.progress().await?.accepted, 1);
        assert_eq!(channel.progress().await?.processed["consumer"], 0);
        channel.shutdown().await?;
        recipe.as_object_mut().expect("recipe").remove("recovery");
        assert!(resolver
            .resolve("first", "graph", &specification, &recipe)
            .await
            .is_err());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn progress_recipes_create_distinct_actual_owners_and_reject_forged_state() -> Result<()>
    {
        let resolver = HostManagementResources::new(
            &crate::PluginRegistry::new(),
            Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
            None,
        )?;
        let specification = ResourceSpecification {
            id: ResourceId::try_new("progress")?,
            role: ResourceRole::Checkpoint,
            ownership: ResourceOwnership::Graph,
            binding: "progress".into(),
        };
        let recipe = json!({"kind":"sourceProgress","component":"query"});
        let first = resolver
            .resolve("first", "instance", &specification, &recipe)
            .await?;
        let second = resolver
            .resolve("second", "instance", &specification, &recipe)
            .await?;
        let first = first.get::<QuerySourceProgressResource>()?;
        let second = second.get::<QuerySourceProgressResource>()?;
        assert_eq!(first.0.graph_id(), "instance");
        assert_eq!(first.0.query_id().as_str(), "query");
        assert!(!Arc::ptr_eq(&first.0, &second.0));
        for recipe in [
            json!({"kind":"sourceProgress","component":""}),
            json!({"kind":"sourceProgress","component":"query","persistent":true}),
            json!({"kind":"sourceProgress","component":"query","ready":true}),
            json!({"kind":"sourceProgress","component":"query","graph":"other"}),
            json!({"kind":"sourceProgress","component":"query","checkpoints":{}}),
        ] {
            assert!(resolver
                .resolve("first", "instance", &specification, &recipe)
                .await
                .is_err());
        }
        let wrong_role = ResourceSpecification {
            role: ResourceRole::StateStore,
            ..specification
        };
        assert!(resolver
            .resolve("first", "instance", &wrong_role, &recipe)
            .await
            .is_err());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn query_catalog_recipe_reconstructs_a_typed_catalog() -> Result<()> {
        let resolver = HostManagementResources::new(
            &crate::PluginRegistry::new(),
            Arc::new(drasi_core::middleware::MiddlewareTypeRegistry::new()),
            None,
        )?;
        let specification = ResourceSpecification {
            id: ResourceId::try_new("results")?,
            role: ResourceRole::QueryCatalog,
            ownership: ResourceOwnership::Graph,
            binding: "results".into(),
        };
        let handle = resolver
            .resolve(
                "first",
                "instance",
                &specification,
                &json!({"kind":"queryCatalog"}),
            )
            .await?;
        handle.get::<QueryResultsCatalog>()?;
        assert!(resolver
            .resolve(
                "first",
                "instance",
                &specification,
                &json!({"kind":"queryCatalog","graph":"other"}),
            )
            .await
            .is_err());
        Ok(())
    }
}
