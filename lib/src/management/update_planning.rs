// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::{ComponentUpdateData, ComponentUpdateMode, ComponentUpdatePlan};
use crate::management::{AcceptedComponentUpdate, ComponentUpdateDefinition};
use sha2::{Digest, Sha256};

fn validate_plan(plan: ComponentUpdatePlan) -> Result<()> {
    anyhow::ensure!(
        plan.data == ComponentUpdateData::Preserve,
        "Rebootstrap requires an authoritative snapshot/live handover, superseded-work proof, and downstream reset acknowledgement; this configuration path cannot establish those guarantees"
    );
    Ok(())
}

fn operation_id(request: &str, component: &ComponentId) -> String {
    let mut digest = Sha256::new();
    digest.update(b"drasi/component-update/1\0");
    digest.update(request.as_bytes());
    digest.update(b"\0");
    digest.update(component.as_str().as_bytes());
    format!("{:x}", digest.finalize())
}

fn retained_interface(
    current: &ComponentSpecification,
    proposed: &ComponentSpecification,
    plan: ComponentUpdatePlan,
) -> Result<()> {
    anyhow::ensure!(
        current.implementation == proposed.implementation,
        "changing a component implementation requires explicit removal and addition"
    );
    if plan.mode != ComponentUpdateMode::Recreate {
        anyhow::ensure!(
            current.descriptor == proposed.descriptor
                && current.role == proposed.role
                && current.completion == proposed.completion
                && current.dependencies == proposed.dependencies,
            "retained-object updates cannot change immutable interfaces or resource bindings"
        );
    }
    Ok(())
}

fn dependency_recipes_changed(
    previous: &DesiredTopology,
    proposed: &DesiredTopology,
    specification: &ComponentSpecification,
) -> bool {
    let mut dependencies: BTreeSet<_> = specification
        .dependencies
        .values()
        .flatten()
        .cloned()
        .collect();
    previous.include_resource_dependencies(&mut dependencies);
    proposed.include_resource_dependencies(&mut dependencies);
    dependencies.iter().any(|id| {
        previous
            .resources
            .iter()
            .find(|resource| &resource.id == id)
            != proposed
                .resources
                .iter()
                .find(|resource| &resource.id == id)
            || previous.resource_configurations.get(id) != proposed.resource_configurations.get(id)
            || previous.resource_dependencies.get(id) != proposed.resource_dependencies.get(id)
    })
}

fn input_descriptions(
    topology: &DesiredTopology,
    component: &ComponentId,
) -> BTreeSet<(EdgeDefinition, Option<StreamId>)> {
    topology
        .relationships
        .iter()
        .chain(&topology.boundary_relationships)
        .filter(|edge| &edge.definition.to.component == component)
        .map(|edge| {
            let stream = topology
                .components
                .iter()
                .find(|node| node.descriptor.id() == &edge.definition.from.component)
                .and_then(|node| node.streams.get(&edge.definition.from.port))
                .cloned();
            (edge.definition.clone(), stream)
        })
        .collect()
}

impl Driver {
    pub(super) async fn update_bindings(
        &mut self,
        desired: &DesiredInstance,
        compiled: &DesiredTopology,
        pending: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<BTreeMap<ComponentId, crate::computation::v1::ComponentUpdateExecution>> {
        let mut old_records = desired.ordinary.clone();
        let ordinary = self
            .current
            .pending_updates
            .iter()
            .any(|update| matches!(update.current, ComponentUpdateDefinition::Ordinary(_)));
        for update in &self.current.pending_updates {
            if let ComponentUpdateDefinition::Ordinary(current) = &update.current {
                if let Some(record) = old_records
                    .iter_mut()
                    .find(|record| record.id == current.id)
                {
                    *record = current.clone();
                }
            }
        }
        let old_batch = if ordinary {
            Some(
                self.runtime
                    .managed_components(
                        &old_records,
                        &desired.topology,
                        &self.applied,
                        &self.factories,
                        self.ordinary_resolver.clone(),
                        pending,
                        &[],
                    )
                    .await?,
            )
        } else {
            None
        };
        let mut bindings = BTreeMap::new();
        for update in &self.current.pending_updates {
            validate_plan(update.plan)?;
            let Some(target) = compiled
                .components
                .iter()
                .find(|node| node.descriptor.id() == &update.component_id)
            else {
                continue;
            };
            let ComponentConstruction::Factory(proposed) = &target.construction else {
                anyhow::bail!("accepted update no longer has a registered target");
            };
            let mut current = match &update.current {
                ComponentUpdateDefinition::Native(current) => current.clone(),
                ComponentUpdateDefinition::Ordinary(_) => {
                    let node = old_batch
                        .as_ref()
                        .and_then(|batch| {
                            batch
                                .definition
                                .components
                                .iter()
                                .find(|node| node.descriptor.id() == &update.component_id)
                        })
                        .ok_or_else(|| {
                            anyhow::anyhow!("accepted ordinary update has no current recipe")
                        })?;
                    let ComponentConstruction::Factory(current) = &node.construction else {
                        anyhow::bail!("accepted ordinary update has an opaque current recipe");
                    };
                    current.clone()
                }
            };
            if matches!(update.current, ComponentUpdateDefinition::Ordinary(_)) {
                current.dependencies = proposed.dependencies.clone();
            }
            bindings.insert(
                update.component_id.clone(),
                crate::computation::v1::ComponentUpdateExecution {
                    operation_id: update.operation_id.clone().into(),
                    plan: update.plan,
                    current: Arc::new(current),
                    proposed: Arc::new(proposed.clone()),
                    require_confirmation: true,
                },
            );
        }
        Ok(bindings)
    }

    pub(super) async fn finish_ready_updates(&mut self) -> Result<()> {
        if self.current.pending_updates.is_empty() {
            return Ok(());
        }
        let control = self.graph.get().await?.control().management_control();
        let wanted: BTreeSet<_> = super::super::ordinary::identities(&self.current.desired)
            .into_iter()
            .map(|identity| identity.id)
            .collect();
        let applied: BTreeSet<_> = self
            .applied
            .components
            .iter()
            .map(|component| component.descriptor.id().clone())
            .collect();
        // A missing node during initial restoration is not a completed removal.
        // The owner must first have applied the corresponding declaration/removal.
        let completed = control
            .completed_updates(
                self.current
                    .pending_updates
                    .iter()
                    .filter(|update| {
                        wanted.contains(&update.component_id)
                            == applied.contains(&update.component_id)
                    })
                    .map(AcceptedComponentUpdate::completion)
                    .collect(),
            )
            .await?;
        if completed.is_empty() {
            return Ok(());
        }
        let result = self
            .store
            .complete_updates(self.current.revision, &completed)
            .await;
        // A readback resolves an acknowledgement lost after durable completion.
        self.refresh_committed().await?;
        if completed.iter().any(|completed| {
            self.current.pending_updates.iter().any(|pending| {
                pending.component_id == completed.component_id
                    && pending.operation_id == completed.operation_id
            })
        }) {
            result?;
            anyhow::bail!("configuration store did not confirm completed update intent");
        }
        Ok(())
    }

    pub(super) async fn plan_updates(
        &mut self,
        desired: &DesiredInstance,
        request: &str,
        pending: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<Vec<AcceptedComponentUpdate>> {
        let control = self.graph.get().await?.control();
        let publication = control.registry_snapshot();
        let names = super::super::ordinary::identities(desired)
            .into_iter()
            .map(|identity| (identity.name, identity.id))
            .collect();
        let mut updates = Vec::new();
        let mut query_changes = Vec::new();
        for proposed in &desired.ordinary {
            let Some(current) = self
                .current
                .desired
                .ordinary
                .iter()
                .find(|current| current.id == proposed.id)
            else {
                continue;
            };
            let mut bound = current.clone();
            bound.bind_names(&names)?;
            if bound.same_construction(proposed) {
                continue;
            }
            anyhow::ensure!(
                !self
                    .current
                    .pending_updates
                    .iter()
                    .any(|pending| pending.component_id == proposed.id),
                "component {} already has an incomplete accepted update",
                proposed.id
            );
            let plan = match (&current.component, &proposed.component) {
                (OrdinaryComponent::Source { kind: old, .. }, OrdinaryComponent::Source { kind, .. })
                | (OrdinaryComponent::Reaction { kind: old, .. }, OrdinaryComponent::Reaction { kind, .. }) => {
                    anyhow::ensure!(old == kind,
                        "changing an ordinary implementation requires explicit removal and addition");
                    control.assess_component_update(&proposed.id, ||
                        self.ordinary_resolver.plan_update(&self.instance_id, current, proposed))?
                }
                (OrdinaryComponent::Query { .. }, OrdinaryComponent::Query { .. }) => {
                    query_changes.push((current, proposed));
                    continue;
                }
                _ => anyhow::bail!(
                    "unregistered/process-local components do not support managed updates; disconnect, remove and add a new component"
                ),
            };
            validate_plan(plan)?;
            if plan.mode != ComponentUpdateMode::Recreate {
                anyhow::ensure!(
                    current.references()? == proposed.references()?,
                    "retained ordinary updates cannot change bound inputs"
                );
            }
            self.validate_live_update(&publication, &proposed.id, plan)?;
            updates.push(AcceptedComponentUpdate {
                component_id: proposed.id.clone(),
                operation_id: operation_id(request, &proposed.id),
                plan,
                current: ComponentUpdateDefinition::Ordinary(current.clone()),
                proposed: ComponentUpdateDefinition::Ordinary(proposed.clone()),
            });
        }
        if !query_changes.is_empty() {
            let factory = self
                .runtime
                .query_factory()
                .ok_or(crate::computation::v1::QueryFactoryNotRegistered)?;
            let proposed_batch = self
                .runtime
                .managed_components(
                    &desired.ordinary,
                    &desired.topology,
                    &self.applied,
                    &self.factories,
                    self.ordinary_resolver.clone(),
                    pending,
                    &[],
                )
                .await?;
            let current_batch = self
                .runtime
                .managed_components(
                    &self.current.desired.ordinary,
                    &self.current.desired.topology,
                    &self.applied,
                    &self.factories,
                    self.ordinary_resolver.clone(),
                    pending,
                    &[],
                )
                .await?;
            for (current, proposed) in query_changes {
                let specification = |batch: &ComponentBatch,
                                     id: &ComponentId|
                 -> Result<ComponentSpecification> {
                    let node = batch
                        .definition
                        .components
                        .iter()
                        .find(|node| node.descriptor.id() == id)
                        .ok_or_else(|| {
                            anyhow::anyhow!("query definition has no compiled component")
                        })?;
                    let ComponentConstruction::Factory(specification) = &node.construction else {
                        anyhow::bail!("query update requires a registered factory");
                    };
                    Ok(specification.clone())
                };
                let old = specification(&current_batch, &current.id)?;
                let new = specification(&proposed_batch, &proposed.id)?;
                let plan = control
                    .assess_component_update(&proposed.id, || factory.plan_update(&old, &new))?;
                validate_plan(plan)?;
                retained_interface(&old, &new, plan)?;
                self.validate_live_update(&publication, &proposed.id, plan)?;
                updates.push(AcceptedComponentUpdate {
                    component_id: proposed.id.clone(),
                    operation_id: operation_id(request, &proposed.id),
                    plan,
                    current: ComponentUpdateDefinition::Ordinary(current.clone()),
                    proposed: ComponentUpdateDefinition::Ordinary(proposed.clone()),
                });
            }
        }
        for proposed in &desired.topology.components {
            let id = proposed.descriptor.id();
            let Some(current) = self
                .current
                .desired
                .topology
                .components
                .iter()
                .find(|current| current.descriptor.id() == id)
            else {
                continue;
            };
            if publication
                .observed
                .components
                .get(id)
                .is_some_and(|node| node.realization == RealizationState::Created)
            {
                if let ComponentConstruction::Factory(specification) = &current.construction {
                    anyhow::ensure!(!dependency_recipes_changed(&self.current.desired.topology, &desired.topology, specification),
                        "live resource-recipe changes require an assessment of actual resource bindings; the specification-only update planner cannot establish it");
                    if self
                        .factories
                        .get(&specification.implementation)
                        .is_some_and(|factory| factory.binds_inputs())
                    {
                        anyhow::ensure!(input_descriptions(&self.current.desired.topology, id) == input_descriptions(&desired.topology, id),
                            "live input-binding changes require current/proposed graph bindings; the specification-only update planner cannot establish them");
                    }
                }
            }
            if current.construction == proposed.construction
                && current.descriptor == proposed.descriptor
                && current.role == proposed.role
                && current.streams == proposed.streams
            {
                continue;
            }
            anyhow::ensure!(
                !self
                    .current
                    .pending_updates
                    .iter()
                    .any(|pending| &pending.component_id == id),
                "component {id} already has an incomplete accepted update"
            );
            let (ComponentConstruction::Factory(current), ComponentConstruction::Factory(proposed)) =
                (&current.construction, &proposed.construction)
            else {
                anyhow::bail!("external objects require explicit removal and addition, not managed replacement");
            };
            let factory = self
                .factories
                .get(&current.implementation)
                .ok_or_else(|| anyhow::anyhow!("update factory is not registered"))?;
            let plan =
                control.assess_component_update(id, || factory.plan_update(current, proposed))?;
            validate_plan(plan)?;
            retained_interface(current, proposed, plan)?;
            self.validate_live_update(&publication, id, plan)?;
            updates.push(AcceptedComponentUpdate {
                component_id: id.clone(),
                operation_id: operation_id(request, id),
                plan,
                current: ComponentUpdateDefinition::Native(current.clone()),
                proposed: ComponentUpdateDefinition::Native(proposed.clone()),
            });
        }
        updates.sort_by(|left, right| left.component_id.cmp(&right.component_id));
        Ok(updates)
    }

    fn validate_live_update(
        &self,
        publication: &GraphRegistrySnapshot,
        id: &ComponentId,
        plan: ComponentUpdatePlan,
    ) -> Result<()> {
        if plan.mode == ComponentUpdateMode::InPlace {
            anyhow::ensure!(publication.observed.components.get(id).is_some_and(|node|
                matches!(node.lifecycle, ComponentLifecycle::Running | ComponentLifecycle::Quiesced)),
                "InPlace requires an activated component; the factory must assess a stopped-instance strategy");
        }
        Ok(())
    }
}
