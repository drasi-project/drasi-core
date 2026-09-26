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
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum Recipe {
    MemoryIndexes,
    Indexes {
        provider: String,
    },
    Middleware,
    QueryMiddleware,
    TransactionalTransformers,
    Qos {
        definition: QosChannelDefinition,
        provider: Option<String>,
    },
    Configuration {
        #[serde(default)]
        secrets: BTreeMap<String, String>,
    },
}

/// Standard host-owned recipes. Supply a custom ManagementResourceResolver for
/// other providers, such as RocksDB or retained journals; their implementation
/// dependencies do not belong in drasi-lib.
pub struct HostManagementResources {
    middleware: Arc<drasi_core::middleware::MiddlewareTypeRegistry>,
    transactional: Arc<TransactionalTransformerRegistry>,
    secrets: Option<Arc<dyn SecretStoreProvider>>,
    factories: FactoryRegistry,
    indexes: BTreeMap<String, Arc<dyn drasi_core::computation::ComputationIndexProvider>>,
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
    async fn resolve(
        &self,
        instance: &str,
        graph: &str,
        specification: &ResourceSpecification,
        configuration: &serde_json::Value,
    ) -> Result<ResourceHandle> {
        let recipe: Recipe = serde_json::from_value(configuration.clone())
            .context("unsupported host resource recipe")?;
        let resource = match recipe {
            Recipe::MemoryIndexes => ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(QueryIndexProviderResource(Arc::new(
                    drasi_core::computation::InMemoryComputationProvider,
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
            } => {
                anyhow::ensure!(
                    specification.ownership == ResourceOwnership::Graph,
                    "host-created QoS resources require graph ownership"
                );
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
                    QosChannel::persistent(
                        definition,
                        indexes,
                        self.factories.envelope_codec(
                            NonZeroUsize::new(64 * 1024 * 1024).expect("constant size"),
                        )?,
                        specification.id.as_str(),
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
