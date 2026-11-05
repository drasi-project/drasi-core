// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;

/// Reconstruct one merger's independent persistent storage owner.
///
/// Declare `indexes` as this resource's sole `IndexBackend` dependency, and bind
/// this graph-owned resource to the merger's `storage` slot. The provider is the
/// actual declared handle, not a second open inferred from a path or name.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceTimeMergeStorageConfig {
    pub implementation: ImplementationIdentity,
    pub configuration_version: u32,
    pub component: ComponentId,
    pub indexes: ResourceId,
}

impl SourceTimeMergeStorageConfig {
    /// Pin the implemented storage recipe and its configuration format.
    pub fn new(component: ComponentId, indexes: ResourceId) -> Result<Self> {
        Ok(Self {
            implementation: ImplementationIdentity::try_new(
                "drasi/source-time-merge-storage",
                "1",
            )?,
            configuration_version: 1,
            component,
            indexes,
        })
    }

    pub fn validate(
        &self,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceRole>,
    ) -> Result<()> {
        anyhow::ensure!(
            specification.role == ResourceRole::IndexBackend
                && specification.ownership == ResourceOwnership::Graph
                && self.configuration_version > 0
                && self.indexes != specification.id,
            "time merge storage requires a graph-owned index-backend declaration"
        );
        anyhow::ensure!(
            dependencies == &BTreeMap::from([(self.indexes.clone(), ResourceRole::IndexBackend)]),
            "time merge storage must declare exactly its actual index-provider dependency"
        );
        Ok(())
    }

    pub fn validate_owner(&self, resource: &ResourceId, topology: &DesiredTopology) -> Result<()> {
        for component in &topology.components {
            if let ComponentConstruction::Factory(specification) = &component.construction {
                if specification.descriptor.id() == &self.component
                    || specification
                        .dependencies
                        .values()
                        .flatten()
                        .any(|id| id == resource)
                {
                    anyhow::ensure!(
                        specification.descriptor.id() == &self.component
                            && specification.role == ComponentRole::Transformer
                            && specification.dependencies.get("storage")
                                .is_some_and(|ids| ids.as_slice() == std::slice::from_ref(resource)),
                        "time merge storage may only be bound to its owning merger"
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
        let supported = Self::new(self.component.clone(), self.indexes.clone())?;
        anyhow::ensure!(
            self.implementation == supported.implementation
                && self.configuration_version == supported.configuration_version,
            "unsupported time merge storage implementation or configuration version"
        );
        let provider = dependencies
            .get(&self.indexes)
            .context("time merge index provider is unavailable")?
            .get::<QueryIndexProviderResource>()?;
        Ok(SourceTimeMergeStorage::new(
            instance,
            graph,
            self.component.clone(),
            provider.0.clone(),
        )?
        .resource())
    }
}
