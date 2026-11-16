// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

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
        component: &drasi_lib::ComponentId,
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
