// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::{
    NativeFactory, NativeSourceBootstrapFactories, NativeSourceBootstrapProgress,
};

/// Immutable source construction recipe for one graph-owned bootstrap engine.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeSourceBootstrapConfig {
    pub component: ComponentId,
    pub implementation: ImplementationIdentity,
    pub configuration_version: u32,
    #[serde(default)]
    pub configuration: BTreeMap<Arc<str>, ConfigurationValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_progress: Option<ResourceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_progress: Option<ResourceId>,
}

impl NativeSourceBootstrapConfig {
    pub fn validate(
        &self,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceRole>,
    ) -> Result<()> {
        anyhow::ensure!(
            specification.role == ResourceRole::Bootstrap
                && specification.ownership == ResourceOwnership::Graph
                && self.configuration_version > 0,
            "native source bootstrap requires a graph-owned Bootstrap resource"
        );
        anyhow::ensure!(
            self.source_progress.is_some() ^ self.consumer_progress.is_some(),
            "native source bootstrap requires exactly one progress mode"
        );
        let mut expected = BTreeMap::new();
        for value in self.configuration.values() {
            if let ConfigurationValue::Reference { resource, .. } = value {
                expected.insert(resource.clone(), ResourceRole::SecretStore);
            }
        }
        let progress = self
            .source_progress
            .as_ref()
            .or(self.consumer_progress.as_ref())
            .expect("validated progress");
        anyhow::ensure!(
            !expected.contains_key(progress),
            "source bootstrap progress cannot also resolve configuration"
        );
        expected.insert(progress.clone(), ResourceRole::Checkpoint);
        let identities: Vec<_> = dependencies
            .iter()
            .filter(|(_, role)| **role == ResourceRole::Identity)
            .collect();
        anyhow::ensure!(
            identities.len() <= 1,
            "source bootstrap accepts at most one identity"
        );
        if let Some((identity, _)) = identities.first() {
            anyhow::ensure!(
                !expected.contains_key(*identity),
                "source bootstrap identity conflicts with another dependency"
            );
            expected.insert((*identity).clone(), ResourceRole::Identity);
        }
        anyhow::ensure!(
            !expected.contains_key(&specification.id) && &expected == dependencies,
            "native source bootstrap must declare exactly its progress, optional identity and configuration dependencies"
        );
        Ok(())
    }

    pub fn validate_owner(&self, resource: &ResourceId, topology: &DesiredTopology) -> Result<()> {
        for component in &topology.components {
            if let ComponentConstruction::Factory(spec) = &component.construction {
                if spec.descriptor.id() == &self.component
                    || spec
                        .dependencies
                        .values()
                        .flatten()
                        .any(|id| id == resource)
                {
                    let identities: Vec<_> = topology
                        .resource_dependencies
                        .get(resource)
                        .into_iter()
                        .flat_map(|dependencies| dependencies.iter())
                        .filter(|(_, role)| **role == ResourceRole::Identity)
                        .map(|(id, _)| id.clone())
                        .collect();
                    anyhow::ensure!(
                        spec.dependencies
                            .get("identity")
                            .map(Vec::as_slice)
                            .unwrap_or_default()
                            == identities.as_slice(),
                        "source bootstrap and source must declare the same identity resource"
                    );
                    let (slot, progress) = if let Some(progress) = &self.source_progress {
                        ("source_progress", progress)
                    } else {
                        (
                            "consumer_progress",
                            self.consumer_progress
                                .as_ref()
                                .context("missing progress")?,
                        )
                    };
                    anyhow::ensure!(
                        spec.descriptor.id() == &self.component
                            && spec.role == ComponentRole::Source
                            && spec.implementation == self.implementation
                            && spec.configuration_version == self.configuration_version
                            && spec.configuration == self.configuration
                            && spec.dependencies.get("source_bootstrap")
                                .is_some_and(|ids| ids.as_slice() == std::slice::from_ref(resource))
                            && spec.dependencies.get(slot)
                                .is_some_and(|ids| ids.as_slice() == std::slice::from_ref(progress)),
                        "source bootstrap and source must declare the same source configuration and progress owner"
                    );
                }
            }
        }
        Ok(())
    }

    pub fn validate_configuration(
        &self,
        factories: &NativeSourceBootstrapFactories,
        declarations: &BTreeMap<ResourceId, ResourceRole>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<Arc<NativeFactory>> {
        let factory = factories
            .get(&self.implementation)
            .context("native source bootstrap factory is not registered")?;
        anyhow::ensure!(
            factory.supports_source_bootstrap()
                && (factory.supports_activation_credentials()
                    || !declarations
                        .values()
                        .any(|role| *role == ResourceRole::Identity))
                && factory.metadata().configuration_version == self.configuration_version
                && factory.source_bootstrap_uses_consumer_progress()
                    == self.consumer_progress.is_some(),
            "native source bootstrap factory version or progress mode mismatch"
        );
        crate::computation::runtime_configuration_schema(&factory.metadata().configuration)
            .validate_values(&self.configuration, declarations, resources)?;
        Ok(factory.clone())
    }

    pub async fn resolve(
        &self,
        instance: &str,
        graph: &str,
        specification: &ResourceSpecification,
        dependencies: &BTreeMap<ResourceId, ResourceHandle>,
        factories: &NativeSourceBootstrapFactories,
    ) -> Result<ResourceHandle> {
        let roles = dependencies
            .iter()
            .map(|(id, handle)| (id.clone(), handle.role()))
            .collect();
        self.validate(specification, &roles)?;
        let factory = self.validate_configuration(factories, &roles, dependencies)?;
        let progress = if let Some(id) = &self.source_progress {
            NativeSourceBootstrapProgress::Single(
                dependencies
                    .get(id)
                    .context("source bootstrap progress is unavailable")?
                    .get::<QuerySourceProgressResource>()?
                    .0
                    .clone(),
            )
        } else {
            NativeSourceBootstrapProgress::Shared(
                dependencies
                    .get(self.consumer_progress.as_ref().expect("validated mode"))
                    .context("source bootstrap consumer progress is unavailable")?
                    .get::<ConsumerProgressSet>()?,
            )
        };
        let configuration =
            crate::computation::runtime_configuration_schema(&factory.metadata().configuration)
                .resolve_values(&self.configuration, dependencies)
                .await?;
        let identity = dependencies
            .values()
            .find(|resource| resource.role() == ResourceRole::Identity)
            .map(|resource| resource.get::<crate::computation::NativeIdentityResource>())
            .transpose()?;
        let coordinator = factory.create_source_bootstrap_with_identity(
            self.component.clone(),
            serde_json::to_value(configuration)?,
            drasi_computation_plugin_sdk::Scope {
                instance_id: instance.into(),
                graph_id: graph.into(),
                generation: 0,
            },
            progress,
            identity,
        )?;
        Ok(
            ResourceHandle::new(ResourceRole::Bootstrap, coordinator.clone())
                .with_cleanup(coordinator),
        )
    }
}
