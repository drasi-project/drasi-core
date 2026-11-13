// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::{ComponentUpdateData, ComponentUpdateMode, ComponentUpdatePlan};
use crate::management::{AcceptedComponentUpdate, ComponentUpdateDefinition};
use sha2::{Digest, Sha256};

fn validate_plan(plan: ComponentUpdatePlan) -> Result<()> {
    anyhow::ensure!(
        plan.data == ComponentUpdateData::Preserve || plan.mode == ComponentUpdateMode::Recreate,
        "data reconstruction requires Recreate"
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

fn changed_resource_recipes(
    previous: &DesiredTopology,
    proposed: &DesiredTopology,
) -> BTreeSet<ResourceId> {
    previous
        .resources
        .iter()
        .filter_map(|current| {
            let target = proposed
                .resources
                .iter()
                .find(|target| target.id == current.id)?;
            (current != target
                || previous.resource_configurations.get(&current.id)
                    != proposed.resource_configurations.get(&current.id)
                || previous.resource_dependencies.get(&current.id)
                    != proposed.resource_dependencies.get(&current.id))
            .then(|| current.id.clone())
        })
        .collect()
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
            let factories = self.effective_factories().await?;
            Some(
                self.runtime
                    .managed_components(
                        &old_records,
                        &desired.topology,
                        &self.applied,
                        &factories,
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
            let factory = self
                .effective_factories()
                .await?
                .get(&proposed.implementation)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("accepted update factory is not registered"))?;
            factory.validate_update_plan(&current, proposed, update.plan)?;
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
        let public_control = self.graph.get().await?.control();
        for update in &self.current.pending_updates {
            if update.plan.data != ComponentUpdateData::Preserve
                && public_control
                    .observed()
                    .components
                    .get(&update.component_id)
                    .is_some_and(|node| {
                        node.realization == RealizationState::Created
                            && node.lifecycle == ComponentLifecycle::Stopped
                            && node.failure.is_none()
                    })
            {
                let report = public_control
                    .prepare_query_update(update.component_id.clone())
                    .await?;
                if report.summary != OperationSummary::Completed {
                    return Err(crate::computation::v1::LifecycleReportError::Start(report).into());
                }
            }
        }
        let control = public_control.management_control();
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

    fn assess_resource_updates(
        &self,
        desired: &DesiredInstance,
        control: &GraphControl,
        publication: &GraphRegistrySnapshot,
    ) -> Result<BTreeSet<ComponentId>> {
        let previous = &self.current.desired.topology;
        let changed_resources = changed_resource_recipes(previous, &desired.topology);
        if changed_resources.is_empty() {
            return Ok(BTreeSet::new());
        }
        let actual = publication
            .desired
            .select(crate::computation::v1::GraphSelection::All)?;
        let mut resource_users = BTreeSet::new();
        for resource in &changed_resources {
            let current = previous
                .resources
                .iter()
                .find(|spec| &spec.id == resource)
                .ok_or_else(|| {
                    anyhow::anyhow!("updated resource {resource} has no current recipe")
                })?;
            let proposed = desired
                .topology
                .resources
                .iter()
                .find(|spec| &spec.id == resource)
                .ok_or_else(|| {
                    anyhow::anyhow!("updated resource {resource} has no target recipe")
                })?;
            anyhow::ensure!(
                current.ownership == proposed.ownership,
                "provider ownership changes require explicit removal and addition"
            );
            let mut affected = BTreeSet::from([resource.clone()]);
            previous.include_resource_dependents(&mut affected);
            actual.include_resource_dependents(&mut affected);
            desired.topology.include_resource_dependents(&mut affected);
            anyhow::ensure!(!affected.iter().any(|id| publication.observed.resources.get(id)
                .is_some_and(|resource| resource.realization == crate::computation::v1::ResourceRealization::CleanupRequired)),
                "resource recipe change overlaps unfinished provider cleanup; retry the accepted target first");
            let users: BTreeSet<_> = affected
                .iter()
                .flat_map(|id| {
                    previous
                        .component_resource_users(id)
                        .into_iter()
                        .chain(actual.component_resource_users(id))
                        .chain(desired.topology.component_resource_users(id))
                })
                .filter(|id| {
                    publication
                        .observed
                        .components
                        .get(id)
                        .is_some_and(|node| node.realization == RealizationState::Created)
                })
                .collect();
            if users.is_empty() {
                continue;
            }
            for id in &affected {
                for topology in [previous, &desired.topology] {
                    if let Some(specification) =
                        topology.resources.iter().find(|spec| &spec.id == id)
                    {
                        anyhow::ensure!(
                            specification.ownership
                                == crate::computation::v1::ResourceOwnership::Graph,
                            "live recipe updates cannot take authority over borrowed resource {id}"
                        );
                    }
                }
            }
            anyhow::ensure!(current == proposed,
                "live resource updates must retain their identity, role, ownership and binding interface");
            let user = users
                .first()
                .ok_or_else(|| anyhow::anyhow!("resource update has no live user"))?;
            let plan = control.assess_component_update(user, || {
                self.resolver
                    .plan_update(resource, previous, &desired.topology)
            })?;
            anyhow::ensure!(
                plan.mode == ComponentUpdateMode::Recreate && plan.data == ComponentUpdateData::Preserve,
                "resource owner updates require Recreate with Preserve; replacement does not authorize data loss"
            );
            resource_users.extend(users);
        }
        for id in &resource_users {
            anyhow::ensure!(
                !self
                    .current
                    .pending_updates
                    .iter()
                    .any(|pending| &pending.component_id == id),
                "component {id} already has an incomplete accepted update"
            );
            let node = previous
                .components
                .iter()
                .find(|node| node.descriptor.id() == id)
                .ok_or_else(|| anyhow::anyhow!("live resource user {id} has no managed recipe"))?;
            let ComponentConstruction::Factory(specification) = &node.construction else {
                anyhow::bail!("external resource users require explicit removal and addition");
            };
            let factory = self
                .factories
                .get(&specification.implementation)
                .ok_or_else(|| {
                    anyhow::anyhow!("resource user {id} requires a registered factory")
                })?;
            factory.validate_update_plan(
                specification,
                specification,
                ComponentUpdatePlan {
                    mode: ComponentUpdateMode::Recreate,
                    data: ComponentUpdateData::Preserve,
                },
            )?;
        }
        Ok(resource_users)
    }

    pub(super) async fn plan_updates(
        &mut self,
        desired: &DesiredInstance,
        request: &str,
        pending: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<Vec<AcceptedComponentUpdate>> {
        let control = self.graph.get().await?.control();
        let publication = control.registry_snapshot();
        let resource_users = self.assess_resource_updates(desired, &control, &publication)?;
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
                    if publication.observed.components.get(&proposed.id).is_some_and(|node|
                        matches!(node.realization, RealizationState::Pending | RealizationState::Blocked))
                    {
                        continue;
                    }
                    query_changes.push((current, proposed));
                    continue;
                }
                _ => anyhow::bail!(
                    "unregistered/process-local components do not support managed updates; disconnect, remove and add a new component"
                ),
            };
            validate_plan(plan)?;
            anyhow::ensure!(
                plan.data == ComponentUpdateData::Preserve,
                "ordinary source/reaction updates cannot reset query-owned state"
            );
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
            let factories = self.effective_factories().await?;
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
                    &factories,
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
                    &factories,
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
                factory.validate_update_plan(&old, &new, plan)?;
                if plan.data != ComponentUpdateData::Preserve {
                    proposed_batch
                        .definition
                        .validate_query_reset_consumers(&proposed.id)?;
                    anyhow::ensure!(
                        current.references()? == proposed.references()?,
                        "initialized query reconstruction cannot change live inputs"
                    );
                }
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
            if publication.observed.components.get(id).is_some_and(|node| {
                matches!(
                    node.realization,
                    RealizationState::Pending | RealizationState::Blocked
                )
            }) {
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
            factory.validate_update_plan(current, proposed, plan)?;
            if resource_users.contains(id) {
                anyhow::ensure!(
                    plan.mode == ComponentUpdateMode::Recreate
                        && plan.data == ComponentUpdateData::Preserve,
                    "combined component/provider updates require Recreate with Preserve"
                );
            }
            if plan.data != ComponentUpdateData::Preserve {
                desired.topology.validate_query_reset_consumers(id)?;
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_owner_requires_explicit_live_update_support() -> Result<()> {
        let topology = crate::computation::v1::ComputationGraph::empty("owner-assessment")?
            .snapshot()
            .select(crate::computation::v1::GraphSelection::All)?;
        let error = NoManagementResources
            .plan_update(&ResourceId::try_new("provider")?, &topology, &topology)
            .err()
            .ok_or_else(|| anyhow::anyhow!("default resolver unexpectedly approved an update"))?;
        assert!(error
            .to_string()
            .contains("owner does not support live recipe updates"));
        Ok(())
    }
}
