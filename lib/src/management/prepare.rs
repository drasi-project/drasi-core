// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use sha2::{Digest, Sha256};

fn digest(kind: &[u8], value: &impl serde::Serialize) -> Result<[u8; 32]> {
    let mut digest = Sha256::new();
    digest.update(b"drasi/configuration-authoring/1\0");
    digest.update(kind);
    digest.update(serde_json::to_vec(value)?);
    Ok(digest.finalize().into())
}

fn prior_creation(receipt: &AcceptanceReceipt) -> Result<&AcceptedComponentCreation> {
    receipt
        .creation
        .as_ref()
        .ok_or_else(|| ManagementError::RequestConflict.into())
}

impl Driver {
    pub(super) async fn accept(
        &mut self,
        expected: u64,
        request: String,
        operation: AuthoringOperation,
    ) -> Result<AcceptanceReceipt> {
        anyhow::ensure!(
            !self.shutdown.load(Ordering::Acquire),
            ManagementError::Closed
        );
        let prior = self.store.receipt(&request).await?;
        let control = self.graph.get().await?.control();
        let mut ids = ComponentIds::for_request(
            control.allows_external_component_ids(),
            prior
                .as_ref()
                .map(prior_creation)
                .transpose()?
                .map(|record| record.allocations.clone()),
        );
        let mut local_records = Vec::new();
        let mut local_binding_owners = Vec::new();
        let mut cleanup = BTreeSet::new();
        let mut updated_helpers = BTreeSet::new();
        let (input, native, request_digest) = match operation {
            AuthoringOperation::Definitions(input) => {
                let request_digest = digest(b"definitions\0", &input)?;
                if let Some(receipt) = &prior {
                    anyhow::ensure!(
                        prior_creation(receipt)?.request_digest == request_digest,
                        ManagementError::RequestConflict
                    );
                    return Ok(receipt.clone());
                }
                (input, None, request_digest)
            }
            AuthoringOperation::Native(construct) => {
                let mut native = construct(&mut ids).map_err(|error| {
                    if prior.is_some() {
                        anyhow::Error::new(ManagementError::RequestConflict).context(error)
                    } else {
                        error.into()
                    }
                })?;
                anyhow::ensure!(
                    native.graph_id == crate::computation::components::INSTANCE_GRAPH_ID,
                    "component definition does not belong to this instance graph"
                );
                native.validate_structure()?;
                canonical_graph(&mut native);
                native.revision = GraphRevision(0);
                let allocations = ids.finish().map_err(|error| {
                    if prior.is_some() {
                        anyhow::Error::new(ManagementError::RequestConflict).context(error)
                    } else {
                        error.into()
                    }
                })?;
                let request_digest = digest(b"native\0", &(&native, &allocations))?;
                if let Some(receipt) = &prior {
                    anyhow::ensure!(
                        prior_creation(receipt)?.request_digest == request_digest,
                        ManagementError::RequestConflict
                    );
                    return Ok(receipt.clone());
                }
                (
                    ConfigurationRequest::default(),
                    Some(native),
                    request_digest,
                )
            }
            AuthoringOperation::Scoped(construct) => {
                anyhow::ensure!(
                    prior.is_none(),
                    "process-local factory bindings cannot replay a consumed request"
                );
                let batch = construct(&mut ids)?;
                let (native, owner) = self
                    .prepare_bound_batch(batch, &mut ids, &control, true)
                    .await?;
                let request_digest = digest(b"scoped\0", &(&native, ids.finish()?))?;
                local_binding_owners.push(owner);
                (
                    ConfigurationRequest::default(),
                    Some(native),
                    request_digest,
                )
            }
            AuthoringOperation::Provided(batch) => {
                anyhow::ensure!(
                    prior.is_none(),
                    "preconstructed bindings cannot replay a consumed request"
                );
                let (native, owner) = self
                    .prepare_bound_batch(batch, &mut ids, &control, false)
                    .await?;
                let request_digest = digest(b"provided\0", &(&native, ids.finish()?))?;
                local_binding_owners.push(owner);
                (
                    ConfigurationRequest::default(),
                    Some(native),
                    request_digest,
                )
            }
            AuthoringOperation::QueryUpdate {
                id,
                generation,
                configuration,
            } => {
                anyhow::ensure!(
                    prior.is_none(),
                    "query replacement cannot replay a consumed request"
                );
                anyhow::ensure!(
                    control.component_handle(&id)?.generation() == generation,
                    GraphError::StaleGeneration
                );
                let existing = self
                    .current
                    .desired
                    .topology
                    .components
                    .iter()
                    .find(|node| node.descriptor.id() == &id)
                    .ok_or_else(|| {
                        anyhow::anyhow!("query {id} is not owned by this configuration")
                    })?;
                anyhow::ensure!(
                    existing.name.as_str() == configuration.name,
                    "use rename_component to change a component name"
                );
                let request_digest = digest(b"query-update\0", &configuration)?;
                let plan = self
                    .runtime
                    .native_query_update_plan(id.as_str(), configuration)
                    .await?;
                for (id, generation) in &plan.members {
                    anyhow::ensure!(
                        control.component_handle(id)?.generation() == *generation,
                        GraphError::StaleGeneration
                    );
                }
                let preview = control
                    .preview(control.desired_snapshot().revision, plan.changes)
                    .await?;
                let mut desired = self.current.desired.clone();
                desired.topology = self.native_projection(preview.desired().clone())?;
                updated_helpers.extend(
                    desired
                        .topology
                        .components
                        .iter()
                        .filter(|node| {
                            !self
                                .current
                                .desired
                                .topology
                                .components
                                .iter()
                                .any(|existing| existing.descriptor.id() == node.descriptor.id())
                        })
                        .map(|node| node.descriptor.id().clone()),
                );
                let owner = self.graph.retain(
                    ComponentBatch {
                        definition: desired.topology.clone(),
                        bindings: plan.bindings,
                    },
                    Some(control.clone()),
                );
                local_binding_owners.push(owner);
                (
                    ConfigurationRequest {
                        desired: Some(desired),
                        additions: Vec::new(),
                    },
                    None,
                    request_digest,
                )
            }
            AuthoringOperation::Local {
                definitions,
                queries,
                native,
                native_factories,
            } => {
                anyhow::ensure!(
                    prior.is_none(),
                    "process-local constructors cannot replay a consumed request"
                );
                anyhow::ensure!(
                    self.pending_bindings.is_empty(),
                    "previous accepted bindings are still pending"
                );
                let mut prepared = ComponentBatch {
                    definition: DesiredInstance::default().topology,
                    bindings: TopologyBindings::default(),
                };
                let mut rejection: Option<anyhow::Error> = self.guarantees.requires_reconstructible_definitions
                    .then(|| anyhow::anyhow!("process-local constructors and objects require memory-only configuration storage"));
                for definition in definitions {
                    let requested = definition.id.clone();
                    let allocated = ids.allocate_requested(requested.clone());
                    let id = match allocated {
                        Ok(id) => id,
                        Err(error) => {
                            rejection.get_or_insert_with(|| error.into());
                            requested
                                .clone()
                                .ok_or_else(|| anyhow::anyhow!("identity allocation failed"))?
                        }
                    };
                    if let Err(error) =
                        control.check_new_component(&id, &definition.name, requested.is_none())
                    {
                        rejection.get_or_insert_with(|| error.into());
                    }
                    let (record, batch) = self.runtime.prepare_local(id, definition)?;
                    if let Some(error) = &batch.bindings.validation_error {
                        rejection.get_or_insert_with(|| {
                            GraphError::Reported {
                                cause: error.clone(),
                            }
                            .into()
                        });
                    }
                    local_records.push(record);
                    crate::computation::runtime::managed::merge(&mut prepared, batch)?;
                }
                let owner = self.graph.retain(prepared, Some(control.clone()));
                if let Some(error) = rejection {
                    return Err(crate::computation::instance::reject_components(
                        owner,
                        error.into(),
                    )
                    .await
                    .into());
                }
                let additions = queries
                    .into_iter()
                    .map(|query| {
                        OrdinaryComponentDefinition::query(query)
                            .map(NewComponentDefinition::Ordinary)
                    })
                    .collect::<Result<Vec<_>>>()?;
                let input = ConfigurationRequest::adding(additions);
                local_binding_owners.push(owner);
                let mut native_definition = ComponentBatch {
                    definition: DesiredInstance::default().topology,
                    bindings: TopologyBindings::default(),
                };
                for owner in native {
                    let definition = self
                        .prepare_existing_batch(owner.clone(), &mut ids, &control, false)
                        .await?;
                    crate::computation::runtime::managed::merge(
                        &mut native_definition,
                        ComponentBatch {
                            definition,
                            bindings: TopologyBindings::default(),
                        },
                    )?;
                    local_binding_owners.push(owner);
                }
                for declare in native_factories {
                    let batch = declare(&mut ids)?;
                    let (definition, owner) = self
                        .prepare_bound_batch(batch, &mut ids, &control, true)
                        .await?;
                    crate::computation::runtime::managed::merge(
                        &mut native_definition,
                        ComponentBatch {
                            definition,
                            bindings: TopologyBindings::default(),
                        },
                    )?;
                    local_binding_owners.push(owner);
                }
                let request_digest = digest(
                    b"process-local\0",
                    &(
                        &local_records,
                        &input,
                        &native_definition.definition,
                        ids.finish()?,
                    ),
                )?;
                (input, Some(native_definition.definition), request_digest)
            }
            AuthoringOperation::Edit {
                generations,
                edit,
                delete_owned_resources,
            } => {
                anyhow::ensure!(
                    prior.is_none(),
                    "process-local mutations cannot replay a consumed request"
                );
                for (id, generation) in generations {
                    anyhow::ensure!(
                        control.component_handle(&id)?.generation() == generation,
                        GraphError::StaleGeneration
                    );
                }
                let mut desired = self.current.desired.clone();
                edit(&mut desired)?;
                anyhow::ensure!(
                    delete_owned_resources
                        .iter()
                        .all(|id| !super::super::ordinary::identities(&desired)
                            .iter()
                            .any(|node| &node.id == id)),
                    "state removal requires removing the component definition"
                );
                cleanup = delete_owned_resources;
                let input = ConfigurationRequest {
                    desired: Some(desired),
                    additions: Vec::new(),
                };
                let request_digest = digest(b"edit\0", &input)?;
                (input, None, request_digest)
            }
        };
        if !self.confirmed.load(Ordering::Acquire) {
            self.refresh_committed().await?;
        }
        anyhow::ensure!(
            expected == self.current.revision,
            ManagementError::RevisionConflict {
                expected,
                actual: self.current.revision,
            }
        );
        let existing: BTreeSet<_> = super::super::ordinary::identities(&self.current.desired)
            .into_iter()
            .map(|component| component.id)
            .collect();
        let mut desired = input
            .desired
            .unwrap_or_else(|| self.current.desired.clone());
        let mut components = Vec::new();
        for component in super::super::ordinary::identities(&desired) {
            if !existing.contains(&component.id) {
                let assigned = updated_helpers.contains(&component.id);
                control.check_new_component(&component.id, &component.name, assigned)?;
                if !assigned {
                    ids.allocate_requested(Some(component.id.clone()))?;
                }
                components.push(AcceptedComponentIdentity {
                    id: component.id,
                    name: component.name,
                });
            }
        }
        for record in local_records {
            components.push(AcceptedComponentIdentity {
                id: record.id.clone(),
                name: record.name.clone(),
            });
            desired.ordinary.push(record);
        }

        let mut additions = Vec::new();
        for definition in input.additions {
            let (name, requested) = match &definition {
                NewComponentDefinition::Native(definition) => (&definition.name, &definition.id),
                NewComponentDefinition::Ordinary(definition) => (&definition.name, &definition.id),
            };
            if let Some(id) = requested {
                control.check_new_component(id, name, false)?;
            }
            let id = ids.allocate_requested(requested.clone())?;
            control.check_new_component(&id, name, requested.is_none())?;
            components.push(AcceptedComponentIdentity {
                id: id.clone(),
                name: name.clone(),
            });
            additions.push((id, definition));
        }
        for (id, definition) in additions {
            match definition {
                NewComponentDefinition::Ordinary(definition) => {
                    anyhow::ensure!(!matches!(definition.component, OrdinaryComponent::ProcessLocal { .. }),
                        "process-local bindings can only be supplied through the instance construction API");
                    if matches!(definition.component, OrdinaryComponent::Query { .. }) {
                        anyhow::ensure!(
                            self.runtime.query_factory().is_some(),
                            QueryFactoryNotRegistered
                        );
                    }
                    desired.ordinary.push(definition.with_assigned_id(id));
                }
                NewComponentDefinition::Native(definition) => {
                    let factory = self
                        .factories
                        .get(&definition.implementation)
                        .ok_or_else(|| anyhow::anyhow!("creation requires a registered factory"))?;
                    let mut specification =
                        factory.specification_from_config(id.clone(), definition.configuration)?;
                    anyhow::ensure!(
                        specification.descriptor.id() == &id,
                        "factory substituted its assigned UUID"
                    );
                    specification.dependencies = definition.dependencies;
                    desired.topology.components.push(DesiredComponent {
                        name: definition.name,
                        descriptor: specification.descriptor.clone(),
                        role: specification.role,
                        completion: specification.completion,
                        streams: definition.streams,
                        lifecycle: definition.lifecycle,
                        input_merge: InputMergePolicy::default(),
                        construction: ComponentConstruction::Factory(specification),
                    });
                }
            }
        }
        if let Some(native) = native {
            for component in &native.components {
                let id = component.descriptor.id();
                ids.require_owned(id)?;
                control.check_new_component(id, &component.name, true)?;
                components.push(AcceptedComponentIdentity {
                    id: id.clone(),
                    name: component.name.clone(),
                });
            }
            let mut merged = ComponentBatch {
                definition: desired.topology,
                bindings: TopologyBindings::default(),
            };
            crate::computation::runtime::managed::merge(
                &mut merged,
                ComponentBatch {
                    definition: native,
                    bindings: TopologyBindings::default(),
                },
            )?;
            desired.topology = merged.definition;
        }
        let owners: Vec<_> = self
            .pending_bindings
            .iter()
            .chain(&local_binding_owners)
            .cloned()
            .collect();
        let checked: Result<()> = async move {
            for (index, left) in owners.iter().enumerate() {
                let left = left.lock().await?;
                let left = left
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("component batch was already submitted"))?;
                for right in &owners[..index] {
                    let right = right.lock().await?;
                    let right = right
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("component batch was already submitted"))?;
                    left.bindings.validate_merge(&right.bindings)?;
                }
            }
            Ok(())
        }
        .await;
        let prepared = checked.and_then(|()| self.normalize_desired(desired));
        let desired = match prepared {
            Ok(desired) => desired,
            Err(error) => {
                let mut cleanup = Vec::new();
                for owner in &local_binding_owners {
                    if let Err(failure) = owner.cleanup().await {
                        cleanup.push(failure.into());
                    }
                }
                if !cleanup.is_empty() {
                    return Err(error.context(OperationFailures::new(
                        "rejected batch cleanup failed",
                        cleanup,
                    )));
                }
                return Err(error);
            }
        };
        let creation = AcceptedComponentCreation {
            components,
            allocations: ids.finish()?,
            request_digest,
        };
        self.pending_bindings.extend(local_binding_owners);
        let proposed = desired.clone();
        let result = self
            .commit_prepared(expected, request, desired, Some(creation))
            .await;
        if result.is_ok()
            || !self.confirmed.load(Ordering::Acquire)
            || self.current.desired == proposed
        {
            self.pending_deprovision.extend(cleanup);
            if !self.confirmed.load(Ordering::Acquire) {
                self.pending_cleanup_target = Some(proposed.clone());
            }
        }
        if result.is_err()
            && self.confirmed.load(Ordering::Acquire)
            && self.current.desired != proposed
        {
            for owner in std::mem::take(&mut self.pending_bindings) {
                owner.cleanup().await?;
            }
        }
        result
    }

    pub(super) fn normalize_desired(&self, desired: DesiredInstance) -> Result<DesiredInstance> {
        let mut desired = normalize_for_store(desired, self.guarantees)?;
        let publication = self.runtime.control()?.registry_snapshot();
        let identities: BTreeSet<_> = super::super::ordinary::identities(&desired)
            .into_iter()
            .map(|node| node.id)
            .collect();
        for node in &desired.topology.components {
            let ComponentConstruction::Factory(spec) = &node.construction else {
                continue;
            };
            if !publication.has_query_authoring(spec) {
                continue;
            }
            if !spec.configuration.contains_key("query_config") {
                continue;
            }
            let Some(Ok(config)) = publication.authored_query_configuration(spec) else {
                continue;
            };
            for input in &config.inputs {
                let Some(id) = input.component.as_id() else {
                    continue;
                };
                if !identities.contains(id) {
                    continue;
                }
                let Some(producer) = publication.desired.component(id) else {
                    continue;
                };
                if !producer
                    .descriptor
                    .ports()
                    .iter()
                    .any(|port| port.direction() == PortDirection::Output)
                {
                    continue;
                }
                if desired
                    .topology
                    .components
                    .iter()
                    .find(|node| node.descriptor.id() == id)
                    .is_some_and(|node| node.descriptor != producer.descriptor)
                {
                    continue;
                }
                let pending = ComputationPipelineBuilder::pending_native_endpoint(
                    config.id.as_str(),
                    id.as_str(),
                )?;
                let explicit = input.port.as_deref().map(PortId::try_new).transpose()?;
                for edge in desired
                    .topology
                    .relationships
                    .iter_mut()
                    .chain(&mut desired.topology.boundary_relationships)
                {
                    if edge.definition.from == pending
                        && edge.definition.to.component == *node.descriptor.id()
                    {
                        edge.definition.from =
                            publication.output_endpoint(id, explicit.as_ref())?;
                    }
                }
            }
        }
        normalize_for_store(desired, self.guarantees)
    }

    fn native_projection(&self, mut target: DesiredTopology) -> Result<DesiredTopology> {
        let previous = &self.current.desired.topology;
        let ordinary: BTreeSet<_> = self
            .applied
            .components
            .iter()
            .filter(|node| {
                !previous
                    .components
                    .iter()
                    .any(|native| native.descriptor.id() == node.descriptor.id())
            })
            .map(|node| node.descriptor.id().clone())
            .collect();
        target
            .components
            .retain(|node| !ordinary.contains(node.descriptor.id()));
        let native: BTreeSet<_> = target
            .components
            .iter()
            .map(|node| node.descriptor.id().clone())
            .collect();
        for relationships in [&mut target.relationships, &mut target.boundary_relationships] {
            relationships.retain(|edge| {
                native.contains(&edge.definition.to.component)
                    || previous
                        .relationships
                        .iter()
                        .chain(&previous.boundary_relationships)
                        .any(|prior| prior.definition == edge.definition)
            });
        }
        target
            .subscriptions
            .retain(|(_, consumer)| native.contains(consumer));
        target
            .control_connections
            .retain(|(_, consumer)| native.contains(consumer));
        target
            .component_resources
            .retain(|id, _| native.contains(id));
        target.component_plugins.retain(|id, _| native.contains(id));
        target.readiness_required.retain(|id| native.contains(id));
        target
            .recovery_requirements
            .retain(|requirement| native.contains(&requirement.consumer));
        let mut resources: BTreeSet<_> = previous
            .resources
            .iter()
            .map(|resource| resource.id.clone())
            .collect();
        for node in &target.components {
            if let ComponentConstruction::Factory(spec) = &node.construction {
                resources.extend(spec.dependencies.values().flatten().cloned());
                resources.extend(spec.configuration.values().filter_map(|value| match value {
                    ConfigurationValue::Reference { resource, .. } => Some(resource.clone()),
                    _ => None,
                }));
            }
        }
        for edge in target
            .relationships
            .iter()
            .chain(&target.boundary_relationships)
        {
            resources.extend(edge.pipe.resource_dependencies().into_keys());
        }
        resources.extend(target.component_resources.values().flatten().cloned());
        target.include_resource_dependencies(&mut resources);
        let control = self.runtime.control()?;
        let infrastructure = self
            .runtime
            .infrastructure_resources(&control.registry_snapshot(), &BTreeSet::new())?;
        resources.retain(|id| !infrastructure.contains(id));
        target
            .resources
            .retain(|resource| resources.contains(&resource.id));
        target
            .resource_configurations
            .retain(|id, _| resources.contains(id));
        target
            .resource_dependencies
            .retain(|id, _| resources.contains(id));
        Ok(target)
    }

    async fn prepare_bound_batch(
        &mut self,
        batch: ComponentBatch,
        ids: &mut ComponentIds,
        control: &GraphControl,
        scoped: bool,
    ) -> Result<(
        DesiredTopology,
        Arc<crate::computation::instance::PendingComponents>,
    )> {
        let owner = self.graph.retain(batch, Some(control.clone()));
        let definition = self
            .prepare_existing_batch(owner.clone(), ids, control, scoped)
            .await?;
        Ok((definition, owner))
    }

    async fn prepare_existing_batch(
        &mut self,
        owner: Arc<crate::computation::instance::PendingComponents>,
        ids: &mut ComponentIds,
        control: &GraphControl,
        scoped: bool,
    ) -> Result<DesiredTopology> {
        owner.attach_control(control.clone());
        let mut value = owner.lock().await?;
        let batch = value
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("component batch was already submitted"))?;
        let validation: Result<()> = async {
            anyhow::ensure!(batch.definition.graph_id == crate::computation::components::INSTANCE_GRAPH_ID,
                "component definition does not belong to this instance graph");
            batch.definition.validate_structure()?;
            if scoped {
                anyhow::ensure!(batch.bindings.components.is_empty()
                    && batch.definition.components.iter().all(|node| matches!(node.construction, ComponentConstruction::Factory(_))),
                    "owned scoped creation requires upfront factory declarations; eager component objects must use the opted-in add API");
            } else {
                for node in &mut batch.definition.components {
                    if let ComponentConstruction::External { binding } = &mut node.construction {
                        let object = batch.bindings.components.remove(binding.as_str())
                            .ok_or_else(|| anyhow::anyhow!("opaque component {} has no supplied object", node.descriptor.id()))?;
                        anyhow::ensure!(object.descriptor() == &node.descriptor, "opaque object descriptor differs from its declaration");
                        let assigned = format!("process-local/{}", uuid::Uuid::new_v4());
                        batch.bindings.components.insert(assigned.clone(), object);
                        *binding = assigned;
                    }
                }
            }
            anyhow::ensure!(!self.guarantees.requires_reconstructible_definitions
                || batch.bindings.components.is_empty() && batch.bindings.resources.is_empty()
                    && batch.bindings.resource_constructors.is_empty() && batch.bindings.pipes.is_empty(),
                "process-local bindings are not persistently reconstructible");
            for node in &batch.definition.components {
                let id = node.descriptor.id();
                control.check_new_component(id, &node.name, scoped)?;
                if scoped { ids.require_owned(id)?; }
                else { ids.allocate_requested(Some(id.clone()))?; }
            }
            self.runtime.bind_batch_source_dependencies(batch).await?;
            let current = control.registry_snapshot();
            let existing = current.desired.resources.keys().filter_map(|id| {
                current.resource(id).ok().map(|handle| (id.clone(), handle))
            }).collect();
            crate::computation::components::append_definition(
                current.desired.select(GraphSelection::All)?,
                &batch.definition, &existing, &batch.bindings.resources,
            )?;
            let mut shared = BTreeSet::new();
            let infrastructure = self.runtime.infrastructure_resources(&current, &BTreeSet::new())?;
            for resource in &batch.definition.resources {
                if self.current.desired.topology.resources.iter().any(|existing| existing.id == resource.id) {
                    continue;
                }
                if let Some(existing) = current.desired.resources.get(&resource.id) {
                    let same = batch.bindings.resources.get(&resource.id)
                        .is_some_and(|supplied| current.resource(&resource.id).is_ok_and(|actual| actual.same_shared_instance(supplied)));
                    anyhow::ensure!(existing == resource && same, "resource {} conflicts with the instance's binding", resource.id);
                    if infrastructure.contains(&resource.id) {
                        shared.insert(resource.id.clone());
                    }
                }
            }
            batch.definition.resources.retain(|resource| !shared.contains(&resource.id));
            batch.definition.resource_dependencies.retain(|id, _| !shared.contains(id));
            batch.definition.resource_configurations.retain(|id, _| !shared.contains(id));
            batch.bindings.resources.retain(|id, _| !shared.contains(id));
            batch.bindings.resource_constructors.retain(|id, _| !shared.contains(id));
            batch.definition.subscriptions.extend(std::mem::take(&mut batch.bindings.subscriptions));
            batch.definition.readiness_required.extend(std::mem::take(&mut batch.bindings.readiness_required));
            canonical_graph(&mut batch.definition);
            Ok(())
        }.await;
        let definition = batch.definition.clone();
        drop(value);
        if let Err(error) = validation {
            return Err(
                crate::computation::instance::reject_components(owner, error.into())
                    .await
                    .into(),
            );
        }
        Ok(definition)
    }
}
