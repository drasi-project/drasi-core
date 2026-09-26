// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Resource recipes for integrated DrasiLib management. Library loading and
//! factory registration use PluginRegistry; no second component registry exists.

use std::{collections::BTreeMap, sync::Arc};

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
    Middleware,
    QueryMiddleware,
    TransactionalTransformers,
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
        })
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
        _: &str,
        _: &str,
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
