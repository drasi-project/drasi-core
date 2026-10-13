// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::{Arc, Mutex, Weak},
};

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use drasi_computation_bridge::{
    BridgeEgressControl, BridgeIngressControl, TlsResourceRecipe, TlsRole,
};
use drasi_lib::{computation::v1::*, management::ManagementResourceResolver};
use serde::Deserialize;

/// Normal managed resource recipes for the native bridge. The graph owns the
/// resources; the host retains only weak access to their real control handles.
pub struct InstanceResources {
    inner: Arc<dyn ManagementResourceResolver>,
    base: PathBuf,
    scope: Mutex<Option<(String, String)>>,
    ingress: Mutex<BTreeMap<ResourceId, Weak<BridgeIngressControl>>>,
    egress: Mutex<BTreeMap<ResourceId, Weak<BridgeEgressControl>>>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum ControlRecipe {
    #[serde(rename = "bridgeIngressControl")]
    Ingress,
    #[serde(rename = "bridgeEgressControl")]
    Egress,
}

enum Recipe {
    Ingress,
    Egress,
    Tls(TlsResourceRecipe),
}

fn recipe(value: &serde_json::Value) -> Result<Option<Recipe>> {
    match value.get("kind").and_then(serde_json::Value::as_str) {
        Some("bridgeIngressControl" | "bridgeEgressControl") => {
            ensure!(
                value.as_object().is_some_and(|object| object.len() == 1),
                "bridge control recipes contain only their kind"
            );
            Ok(Some(
                match serde_json::from_value::<ControlRecipe>(value.clone())? {
                    ControlRecipe::Ingress => Recipe::Ingress,
                    ControlRecipe::Egress => Recipe::Egress,
                },
            ))
        }
        Some("bridgeTls") => Ok(Some(Recipe::Tls(serde_json::from_value(value.clone())?))),
        _ => Ok(None),
    }
}

impl InstanceResources {
    pub fn new(inner: Arc<dyn ManagementResourceResolver>, base: impl Into<PathBuf>) -> Self {
        Self {
            inner,
            base: base.into(),
            scope: Mutex::new(None),
            ingress: Mutex::new(BTreeMap::new()),
            egress: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn ingress(&self, resource: &ResourceId) -> Result<Arc<BridgeIngressControl>> {
        self.ingress
            .lock()
            .map_err(|_| anyhow::anyhow!("ingress resource access poisoned"))?
            .get(resource)
            .and_then(Weak::upgrade)
            .context("native ingress control is not a live graph-owned resource")
    }

    pub fn egress(&self, resource: &ResourceId) -> Result<Arc<BridgeEgressControl>> {
        self.egress
            .lock()
            .map_err(|_| anyhow::anyhow!("egress resource access poisoned"))?
            .get(resource)
            .and_then(Weak::upgrade)
            .context("native egress control is not a live graph-owned resource")
    }

    fn validate_recipe(
        specification: &ResourceSpecification,
        recipe: &Recipe,
        dependencies: &BTreeMap<ResourceId, ResourceRole>,
    ) -> Result<()> {
        ensure!(
            specification.ownership == ResourceOwnership::Graph,
            "bridge resources must have ordinary graph cleanup ownership"
        );
        ensure!(
            dependencies.is_empty(),
            "bridge control/TLS recipes do not acquire other resources"
        );
        ensure!(
            specification.binding.as_ref() == specification.id.as_str(),
            "bridge resource binding must equal its stable resource ID"
        );
        let role = match recipe {
            Recipe::Ingress | Recipe::Egress => ResourceRole::Component,
            Recipe::Tls(config) => {
                config.validate()?;
                ResourceRole::Identity
            }
        };
        ensure!(
            specification.role == role,
            "bridge resource has the wrong role"
        );
        Ok(())
    }

    fn delegated_topology(
        topology: &DesiredTopology,
    ) -> Result<(DesiredTopology, BTreeSet<ResourceId>)> {
        let mut ids = BTreeSet::new();
        for resource in &topology.resources {
            let Some(configuration) = topology.resource_configurations.get(&resource.id) else {
                continue;
            };
            let Some(kind) = recipe(configuration)? else {
                continue;
            };
            let dependencies = topology
                .resource_dependencies
                .get(&resource.id)
                .cloned()
                .unwrap_or_default();
            Self::validate_recipe(resource, &kind, &dependencies)?;
            let mut users = 0;
            for component in &topology.components {
                let ComponentConstruction::Factory(spec) = &component.construction else {
                    continue;
                };
                for (slot, resources) in &spec.dependencies {
                    if !resources.contains(&resource.id) {
                        continue;
                    }
                    users += 1;
                    match &kind {
                        Recipe::Ingress => ensure!(
                            slot.as_ref() == "control"
                                && spec.implementation.name.as_ref() == "drasi.bridge/source",
                            "ingress control must belong to its native bridge source"
                        ),
                        Recipe::Egress => ensure!(
                            slot.as_ref() == "control"
                                && spec.implementation.name.as_ref() == "drasi.bridge/sink",
                            "egress control must belong to its native bridge sink"
                        ),
                        Recipe::Tls(config) => {
                            let expected = match spec.implementation.name.as_ref() {
                                "drasi.bridge/source" => TlsRole::Server,
                                "drasi.bridge/sink" => TlsRole::Client,
                                _ => anyhow::bail!(
                                    "bridge TLS must belong to a native bridge endpoint"
                                ),
                            };
                            ensure!(
                                slot.as_ref() == "tls" && config.role == expected,
                                "bridge TLS client/server role does not match its endpoint"
                            );
                        }
                    }
                }
            }
            ensure!(
                matches!(kind, Recipe::Tls(_)) || users <= 1,
                "a control resource cannot represent multiple bridge lifetimes"
            );
            ids.insert(resource.id.clone());
        }
        for (resource, dependencies) in &topology.resource_dependencies {
            ensure!(
                ids.contains(resource) || dependencies.keys().all(|id| !ids.contains(id)),
                "ordinary resource recipes cannot depend on bridge control/TLS capabilities"
            );
        }
        let mut delegated = topology.clone();
        delegated
            .resources
            .retain(|resource| !ids.contains(&resource.id));
        delegated
            .resource_configurations
            .retain(|id, _| !ids.contains(id));
        delegated
            .resource_dependencies
            .retain(|id, _| !ids.contains(id));
        Ok((delegated, ids))
    }
}

#[async_trait]
impl ManagementResourceResolver for InstanceResources {
    fn validate_transition(
        &self,
        previous: &DesiredTopology,
        desired: &DesiredTopology,
    ) -> Result<()> {
        let (old, old_ids) = Self::delegated_topology(previous)?;
        let (next, _) = Self::delegated_topology(desired)?;
        // Do not mask an in-place control/TLS change from the underlying recovery
        // guard merely because that resolver does not parse these new recipes.
        for id in old_ids {
            if let Some(resource) = desired.resources.iter().find(|resource| resource.id == id) {
                ensure!(previous.resources.iter().find(|resource| resource.id == id) == Some(resource)
                    && previous.resource_configurations.get(&id) == desired.resource_configurations.get(&id)
                    && previous.resource_dependencies.get(&id) == desired.resource_dependencies.get(&id),
                    "bridge control/TLS resource recipes are immutable; use explicit retirement/reconstruction");
            } else {
                ensure!(!desired.components.iter().any(|component| {
                    matches!(&component.construction, ComponentConstruction::Factory(spec)
                        if spec.dependencies.values().flatten().any(|resource| resource == &id)
                        || spec.configuration.values().any(|value| matches!(value, ConfigurationValue::Reference { resource, .. } if resource == &id)))
                }), "cannot remove a bridge resource that remains referenced");
            }
        }

        // Component definitions, dependencies, edges and all ordinary recipes
        // stay intact, including the SDK's actual persistent-domain protections.
        self.inner.validate_transition(&old, &next)
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
        let Some(kind) = recipe(configuration)? else {
            return self
                .inner
                .resolve_with_dependencies(
                    instance,
                    graph,
                    specification,
                    configuration,
                    dependencies,
                )
                .await;
        };
        Self::validate_recipe(
            specification,
            &kind,
            &dependencies
                .iter()
                .map(|(id, value)| (id.clone(), value.role()))
                .collect(),
        )?;
        Ok(match kind {
            Recipe::Ingress => {
                let control = Arc::new(BridgeIngressControl::new());
                let mut controls = self
                    .ingress
                    .lock()
                    .map_err(|_| anyhow::anyhow!("ingress resource access poisoned"))?;
                controls.retain(|_, value| value.strong_count() > 0);
                controls.insert(specification.id.clone(), Arc::downgrade(&control));
                control.resource()
            }
            Recipe::Egress => {
                let control = Arc::new(BridgeEgressControl::new());
                let mut controls = self
                    .egress
                    .lock()
                    .map_err(|_| anyhow::anyhow!("egress resource access poisoned"))?;
                controls.retain(|_, value| value.strong_count() > 0);
                controls.insert(specification.id.clone(), Arc::downgrade(&control));
                control.resource()
            }
            Recipe::Tls(config) => {
                ResourceHandle::new(ResourceRole::Identity, config.resolve(&self.base)?)
            }
        })
    }
}

#[cfg(test)]
#[path = "resource_tests.rs"]
mod tests;
