// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_lib::{computation::v1::*, management::ManagementResourceResolver};

/// The instance's resource resolver. It delegates every recipe and keeps
/// weak access to resolved QoS journals, so composition shutdown can read a
/// durable bridge ingress journal's subscriber progress. The graph owns the
/// resources.
pub struct InstanceResources {
    inner: Arc<dyn ManagementResourceResolver>,
    scope: Mutex<Option<(String, String)>>,
    journals: Mutex<BTreeMap<ResourceId, Weak<QosChannel>>>,
}

impl InstanceResources {
    pub fn new(inner: Arc<dyn ManagementResourceResolver>) -> Self {
        Self {
            inner,
            scope: Mutex::new(None),
            journals: Mutex::new(BTreeMap::new()),
        }
    }

    /// A live graph-owned QoS journal resolved by this instance.
    pub fn journal(&self, resource: &ResourceId) -> Result<Arc<QosChannel>> {
        self.journals
            .lock()
            .map_err(|_| anyhow::anyhow!("journal resource access poisoned"))?
            .get(resource)
            .and_then(Weak::upgrade)
            .context("bridge admission journal is not a live graph-owned resource")
    }
}

#[async_trait]
impl ManagementResourceResolver for InstanceResources {
    fn validate_transition(
        &self,
        previous: &DesiredTopology,
        desired: &DesiredTopology,
    ) -> Result<()> {
        self.inner.validate_transition(previous, desired)
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
        {
            let mut scope = self
                .scope
                .lock()
                .map_err(|_| anyhow::anyhow!("resource scope poisoned"))?;
            let identity = (instance.to_owned(), graph.to_owned());
            if let Some(current) = scope.as_ref() {
                ensure!(
                    current == &identity,
                    "an instance resource resolver cannot be shared across graph/instance owners"
                );
            } else {
                *scope = Some(identity);
            }
        }
        let handle = self
            .inner
            .resolve_with_dependencies(instance, graph, specification, configuration, dependencies)
            .await?;
        if let Ok(journal) = handle.get::<QosChannel>() {
            let mut journals = self
                .journals
                .lock()
                .map_err(|_| anyhow::anyhow!("journal resource access poisoned"))?;
            journals.retain(|_, value| value.strong_count() > 0);
            journals.insert(specification.id.clone(), Arc::downgrade(&journal));
        }
        Ok(handle)
    }
}

#[cfg(test)]
#[path = "resource_tests.rs"]
mod tests;
