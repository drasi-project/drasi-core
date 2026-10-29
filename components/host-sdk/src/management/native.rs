// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::{NativeBootstrapFactories, NativeBootstrapFactory};

/// Ordered child bootstrap resources belonging to one real continuous query.
///
/// Canonical recipe: `{"kind":"compositeBootstrap","component":"query",
/// "providers":["postgres-bootstrap","http-bootstrap"]}`. Declare exactly those
/// child IDs as Bootstrap resource dependencies and bind the composite to the
/// query's single `bootstrap` slot. Recovery-bearing children must share the
/// same actual query progress owner; this recipe creates no aggregate reader.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CompositeBootstrapConfig {
    pub component: ComponentId,
    pub providers: Vec<ResourceId>,
}

impl CompositeBootstrapConfig {
    pub fn validate(
        &self,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceRole>,
    ) -> Result<()> {
        anyhow::ensure!(
            specification.role == ResourceRole::Bootstrap
                && specification.ownership == ResourceOwnership::Graph
                && !self.providers.is_empty()
                && self.providers.len() <= MAX_COMPOSITE_BOOTSTRAP_PROVIDERS,
            "composite bootstrap requires a graph-owned Bootstrap resource with bounded nonempty membership"
        );
        let expected: BTreeMap<_, _> = self
            .providers
            .iter()
            .map(|id| (id.clone(), ResourceRole::Bootstrap))
            .collect();
        anyhow::ensure!(
            expected.len() == self.providers.len()
                && !expected.contains_key(&specification.id) && &expected == dependencies,
            "composite bootstrap must declare exactly its distinct child Bootstrap resources, without self-dependency"
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
                            && spec.dependencies.get("bootstrap")
                                .is_some_and(|ids| ids.as_slice() == std::slice::from_ref(resource)),
                        "composite bootstrap may only be bound to its owning continuous query"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn resolve(
        &self,
        instance: &str,
        graph: &str,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<ResourceHandle> {
        self.validate(
            specification,
            &dependencies
                .iter()
                .map(|(id, handle)| (id.clone(), handle.role()))
                .collect(),
        )?;
        let providers = self
            .providers
            .iter()
            .map(|id| {
                let resource = dependencies
                    .get(id)
                    .context("composite bootstrap child is unavailable")?
                    .get::<QueryBootstrapResource>()?;
                Ok((id.clone(), resource.0.clone()))
            })
            .collect::<Result<Vec<_>>>()?;
        let provider = Arc::new(CompositeBootstrapProvider::new(
            instance,
            graph,
            self.component.clone(),
            providers,
        )?);
        Ok(ResourceHandle::new(
            ResourceRole::Bootstrap,
            Arc::new(QueryBootstrapResource(provider.clone())),
        )
        .with_cleanup(provider))
    }
}

/// Exact progress resources for one producer's required consumer boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ConsumerProgressConfig {
    pub producer: ComponentId,
    pub consumers: BTreeMap<ComponentId, ResourceId>,
}

impl ConsumerProgressConfig {
    pub fn validate(
        &self,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceRole>,
    ) -> Result<()> {
        anyhow::ensure!(
            specification.role == ResourceRole::Checkpoint
                && !self.consumers.is_empty()
                && self.consumers.len() <= MAX_CONSUMER_PROGRESS_MEMBERS,
            "consumer progress requires a Checkpoint resource with 1..=256 consumers"
        );
        anyhow::ensure!(
            !self.consumers.contains_key(&self.producer)
                && self
                    .consumers
                    .values()
                    .all(|resource| resource != &specification.id),
            "consumer progress cannot depend on itself or its producer's input progress"
        );
        let expected: BTreeMap<_, _> = self
            .consumers
            .values()
            .map(|resource| (resource.clone(), ResourceRole::Checkpoint))
            .collect();
        anyhow::ensure!(
            expected.len() == self.consumers.len() && dependencies == &expected,
            "consumer progress must declare one distinct checkpoint dependency per consumer"
        );
        Ok(())
    }

    pub fn resolve(
        &self,
        graph: &str,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<ResourceHandle> {
        self.validate(
            specification,
            &dependencies
                .iter()
                .map(|(id, handle)| (id.clone(), handle.role()))
                .collect(),
        )?;
        let owners = self
            .consumers
            .iter()
            .map(|(consumer, resource)| {
                let owner = dependencies
                    .get(resource)
                    .context("consumer progress dependency is unavailable")?
                    .get::<QuerySourceProgressResource>()?;
                anyhow::ensure!(
                    owner.0.component_id() == consumer && owner.0.graph_id() == graph,
                    "consumer progress dependency belongs to another graph or consumer"
                );
                Ok(owner.0.clone())
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(ResourceHandle::new(
            ResourceRole::Checkpoint,
            Arc::new(ConsumerProgressSet::new(
                graph,
                self.producer.clone(),
                owners,
            )?),
        ))
    }
}

fn snapshot_timeout_ms() -> u64 {
    300_000
}

fn snapshot_max_readers() -> usize {
    16
}

/// One allow-listed output: a component and, optionally, one of its output
/// ports. An omitted port selects the component's primary output through the
/// graph's shared port resolution when a read is opened.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeOutputSelector {
    pub component: ComponentId,
    #[serde(default)]
    pub port: Option<PortId>,
}

/// An immutable allow-list and limits policy for output-snapshot reads. It
/// declares no catalogue dependency: the native factory combines the resolved
/// policy with the constructing graph's actual output catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeOutputSnapshotsConfig {
    pub outputs: Vec<NativeOutputSelector>,
    #[serde(default = "snapshot_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default = "snapshot_max_readers")]
    pub max_readers: usize,
}

impl NativeOutputSnapshotsConfig {
    pub fn validate(
        &self,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceRole>,
    ) -> Result<()> {
        anyhow::ensure!(
            specification.role == ResourceRole::OutputCatalogue,
            "native output snapshots require an output-catalogue resource declaration"
        );
        anyhow::ensure!(
            dependencies.is_empty(),
            "native output snapshots are an immutable policy and declare no dependencies"
        );
        self.policy()?;
        Ok(())
    }

    fn policy(&self) -> Result<crate::computation::NativeOutputSnapshotsResource> {
        crate::computation::NativeOutputSnapshotsResource::new(
            self.outputs.iter().map(|output| {
                crate::computation::OutputSelector::new(
                    output.component.clone(),
                    output.port.clone(),
                )
            }),
            std::time::Duration::from_millis(self.timeout_ms),
            self.max_readers,
        )
    }

    pub fn resolve(
        &self,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<ResourceHandle> {
        self.validate(
            specification,
            &dependencies
                .iter()
                .map(|(id, handle)| (id.clone(), handle.role()))
                .collect(),
        )?;
        Ok(ResourceHandle::new(
            ResourceRole::OutputCatalogue,
            Arc::new(self.policy()?),
        ))
    }
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_bootstrap: Option<ResourceId>,
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
        if let Some(source) = &self.source_bootstrap {
            anyhow::ensure!(
                self.source_progress.is_some() && !expected.contains_key(source),
                "paired native bootstrap requires separate coordinator and query progress resources"
            );
            expected.insert(source.clone(), ResourceRole::Bootstrap);
        }
        anyhow::ensure!(
            !expected.contains_key(&specification.id) && *dependencies == expected,
            "native bootstrap must declare exactly its progress and configuration dependencies"
        );
        Ok(())
    }

    pub fn validate_owner(&self, resource: &ResourceId, topology: &DesiredTopology) -> Result<()> {
        if let Some(source) = &self.source_bootstrap {
            let recipe: Recipe = serde_json::from_value(
                topology
                    .resource_configurations
                    .get(source)
                    .context("paired source bootstrap recipe is unavailable")?
                    .clone(),
            )?;
            let Recipe::NativeSourceBootstrap(source) = recipe else {
                anyhow::bail!("paired native bootstrap requires a native source coordinator");
            };
            let progress = self
                .source_progress
                .as_ref()
                .context("missing paired query progress")?;
            if let Some(single) = source.source_progress {
                anyhow::ensure!(
                    &single == progress,
                    "paired bootstrap declares a different progress owner"
                );
            } else {
                let group = source
                    .consumer_progress
                    .context("missing source bootstrap group")?;
                let recipe: Recipe = serde_json::from_value(
                    topology
                        .resource_configurations
                        .get(&group)
                        .context("paired source consumer progress recipe is unavailable")?
                        .clone(),
                )?;
                let Recipe::ConsumerProgress(group) = recipe else {
                    anyhow::bail!("paired source bootstrap requires actual consumer progress");
                };
                anyhow::ensure!(
                    group.producer == source.component
                        && group.consumers.get(&self.component) == Some(progress),
                    "paired bootstrap query is not a declared source consumer"
                );
            }
        }
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
        anyhow::ensure!(
            factory.requires_source_bootstrap() == self.source_bootstrap.is_some(),
            "native bootstrap source coordinator binding mismatch"
        );
        crate::computation::runtime_configuration_schema(&metadata.configuration).validate_values(
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
        let configuration =
            crate::computation::runtime_configuration_schema(&factory.metadata().configuration)
                .resolve_values(&self.configuration, dependencies)
                .await?;
        let scope = drasi_computation_plugin_sdk::Scope {
            instance_id: instance.into(),
            graph_id: graph.into(),
            // Resource owners have no component generation. Bootstrap-v1
            // separately fences each live snapshot generation.
            generation: 0,
        };
        let configuration = serde_json::to_value(configuration)?;
        let provider = if let Some(source) = &self.source_bootstrap {
            factory.create_paired_query_provider(
                self.component.clone(),
                configuration,
                scope,
                progress.context("paired query progress is unavailable")?,
                dependencies
                    .get(source)
                    .context("source bootstrap coordinator is unavailable")?
                    .get::<crate::computation::NativeSourceBootstrapResource>()?,
            )?
        } else {
            factory.create_query_provider(self.component.clone(), configuration, scope, progress)?
        };
        Ok(ResourceHandle::new(
            ResourceRole::Bootstrap,
            Arc::new(QueryBootstrapResource(provider.clone())),
        )
        .with_cleanup(provider))
    }
}
