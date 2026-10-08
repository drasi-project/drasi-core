// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::{NativeBootstrapFactories, NativeBootstrapFactory};

pub(super) fn consumer_provider(
    instance: &str,
    graph: &str,
    provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
) -> Arc<dyn drasi_core::computation::ComputationIndexProvider> {
    Arc::new(ConsumerProvider {
        provider,
        graph: graph.into(),
        scope: format!("native-consumer/{}/{instance}/{graph}", instance.len()),
    })
}

struct ConsumerProvider {
    provider: Arc<dyn drasi_core::computation::ComputationIndexProvider>,
    scope: String,
    graph: String,
}

#[async_trait]
impl drasi_core::computation::ComputationIndexProvider for ConsumerProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &str,
    ) -> std::result::Result<
        drasi_core::computation::ComputationIndexes,
        drasi_core::interface::IndexError,
    > {
        if graph != self.graph {
            return Err(drasi_core::interface::IndexError::other(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "native consumer indexes belong to another graph",
                ),
            ));
        }
        self.provider.create_indexes(&self.scope, component).await
    }
    fn is_volatile(&self) -> bool {
        self.provider.is_volatile()
    }
    fn provider_dependency(
        &self,
    ) -> Option<&Arc<dyn drasi_core::computation::ComputationIndexProvider>> {
        Some(&self.provider)
    }
    fn durability(&self) -> drasi_core::interface::StorageDurability {
        self.provider.durability()
    }
    fn transaction_group(&self) -> Option<&drasi_core::computation::ComputationTransactionGroup> {
        self.provider.transaction_group()
    }
}

/// Reconstruct one query-owned native provider, retaining unresolved values.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeBootstrapConfig {
    pub component: ComponentId,
    pub implementation: ImplementationIdentity,
    pub configuration_version: u32,
    #[serde(default)]
    pub configuration: BTreeMap<Arc<str>, ConfigurationValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_progress: Option<ResourceId>,
}

impl NativeBootstrapConfig {
    pub fn validate(
        &self,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceRole>,
    ) -> Result<()> {
        anyhow::ensure!(
            specification.role == ResourceRole::Bootstrap
                && specification.ownership == ResourceOwnership::Graph,
            "native bootstrap requires a graph-owned bootstrap declaration"
        );
        anyhow::ensure!(
            self.configuration_version > 0,
            "invalid bootstrap configuration version"
        );
        let mut expected = BTreeMap::new();
        for value in self.configuration.values() {
            if let ConfigurationValue::Reference { resource, .. } = value {
                expected.insert(resource.clone(), ResourceRole::SecretStore);
            }
        }
        if let Some(progress) = &self.source_progress {
            anyhow::ensure!(
                !expected.contains_key(progress),
                "bootstrap progress cannot also resolve configuration"
            );
            expected.insert(progress.clone(), ResourceRole::Checkpoint);
        }
        anyhow::ensure!(
            *dependencies == expected,
            "native bootstrap must declare exactly its progress and configuration dependencies"
        );
        Ok(())
    }

    pub fn validate_owner(&self, resource: &ResourceId, topology: &DesiredTopology) -> Result<()> {
        let query = ContinuousQueryFactory::default();
        for component in &topology.components {
            if let ComponentConstruction::Factory(spec) = &component.construction {
                if spec
                    .dependencies
                    .values()
                    .flatten()
                    .any(|id| id == resource)
                {
                    anyhow::ensure!(
                        spec.descriptor.id() == &self.component
                            && spec.implementation == query.descriptor().implementation
                            && spec
                                .dependencies
                                .get("bootstrap")
                                .is_some_and(|ids| ids.as_slice() == std::slice::from_ref(resource)),
                        "native bootstrap may only be bound to its owning continuous query"
                    );
                    if let Some(progress) = &self.source_progress {
                        anyhow::ensure!(spec.dependencies.get("source_progress")
                            .is_some_and(|ids| ids.as_slice() == std::slice::from_ref(progress)),
                            "native bootstrap and query must declare the same progress owner");
                    }
                }
            }
        }
        Ok(())
    }

    pub fn validate_configuration(
        &self,
        factories: &NativeBootstrapFactories,
        declarations: &BTreeMap<ResourceId, ResourceRole>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<Arc<NativeBootstrapFactory>> {
        let factory = factories
            .get(&self.implementation)
            .context("native bootstrap factory is not registered")?;
        let metadata = factory.metadata();
        anyhow::ensure!(
            metadata.configuration_version == self.configuration_version,
            "native bootstrap configuration version mismatch"
        );
        anyhow::ensure!(
            metadata.source_progress == self.source_progress.is_some(),
            "native bootstrap source progress binding mismatch"
        );
        metadata.configuration.runtime().validate_values(
            &self.configuration,
            declarations,
            resources,
        )?;
        Ok(factory.clone())
    }

    pub async fn resolve(
        &self,
        instance: &str,
        graph: &str,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
        factories: &NativeBootstrapFactories,
    ) -> Result<ResourceHandle> {
        let roles = dependencies
            .iter()
            .map(|(id, handle)| (id.clone(), handle.role()))
            .collect();
        self.validate(specification, &roles)?;
        let factory = self.validate_configuration(factories, &roles, dependencies)?;
        let progress = self
            .source_progress
            .as_ref()
            .map(|id| -> Result<_> {
                let progress = dependencies
                    .get(id)
                    .context("bootstrap progress is unavailable")?
                    .get::<QuerySourceProgressResource>()?
                    .0
                    .clone();
                anyhow::ensure!(
                    progress.graph_id() == graph && progress.query_id() == &self.component,
                    "native bootstrap progress belongs to another graph/query"
                );
                Ok(progress)
            })
            .transpose()?;
        let configuration = factory
            .metadata()
            .configuration
            .runtime()
            .resolve_values(&self.configuration, dependencies)
            .await?;
        let provider = factory.create_query_provider(
            self.component.clone(),
            serde_json::to_value(configuration)?,
            drasi_computation_plugin_sdk::Scope {
                instance_id: instance.into(),
                graph_id: graph.into(),
                // Resource owners have no component generation. Bootstrap-v1
                // separately fences each live snapshot generation.
                generation: 0,
            },
            progress,
        )?;
        Ok(ResourceHandle::new(
            ResourceRole::Bootstrap,
            Arc::new(QueryBootstrapResource(provider.clone())),
        )
        .with_cleanup(provider))
    }
}
