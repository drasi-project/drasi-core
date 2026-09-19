// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::*;
use crate::computation::v1::{ObservedResource, ResourceOwnership, ResourceRealization};

impl GraphControl {
    /// Queue a complete snapshot of the resources used by a component generation.
    ///
    /// Specifications must be borrowed; graph-owned additions belong in
    /// `add_component` or `reconcile`. Their IDs are attachment-local names,
    /// scoped under `attached/<hex-encoded component ID>/<generation>/`.
    /// An already registered instance keeps its existing ID and ownership.
    /// Empty reports detach all previously reported, non-declared dependencies.
    ///
    /// This never waits for queue capacity or publication. The controller checks
    /// the generation again when applying the report and discards retired reports.
    pub fn observe_resources(
        &self,
        component: &ComponentId,
        generation: ComponentGeneration,
        mut bindings: Vec<(ResourceSpecification, ResourceHandle)>,
    ) -> GraphResult<()> {
        self.validate_observation_generation(component, generation)?;
        validate_bindings(&bindings)?;
        let owner = component
            .as_str()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        for (specification, _) in &mut bindings {
            specification.id = ResourceId::try_new(format!(
                "attached/{owner}/{}/{}",
                generation.0, specification.id
            ))?;
        }
        self.queue_observation(controller::Command::ObserveResources {
            component: component.clone(),
            generation,
            bindings,
        })
    }

    /// Queue explicitly reported plugin provenance without mutating descriptors.
    ///
    /// Both admission and publication reject conflicts with declared provenance
    /// or a previously accepted origin. Queue admission does not await publication;
    /// conflicts discovered by the controller remain on the component observation.
    pub fn observe_plugin(
        &self,
        component: &ComponentId,
        generation: ComponentGeneration,
        plugin: PluginIdentity,
    ) -> GraphResult<()> {
        self.validate_observation_generation(component, generation)?;
        validate_plugin(&self.desired_snapshot(), component, &plugin)?;
        self.queue_observation(controller::Command::ObservePlugin {
            component: component.clone(),
            generation,
            plugin,
        })
    }

    fn validate_observation_generation(
        &self,
        component: &ComponentId,
        generation: ComponentGeneration,
    ) -> GraphResult<()> {
        if !self
            .observed()
            .components
            .get(component)
            .is_some_and(|node| node.generation == generation)
        {
            return Err(GraphError::StaleGeneration);
        }
        Ok(())
    }

    fn queue_observation(&self, command: controller::Command) -> GraphResult<()> {
        self.commands
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => GraphError::ObservationQueueFull,
                mpsc::error::TrySendError::Closed(_) => GraphError::ControllerClosed,
            })
    }
}

fn validate_bindings(bindings: &[(ResourceSpecification, ResourceHandle)]) -> GraphResult<()> {
    let mut unique = BTreeSet::new();
    for (specification, handle) in bindings {
        if specification.ownership != ResourceOwnership::Borrowed {
            return Err(topology(
                "resource observations cannot transfer graph cleanup ownership",
            ));
        }
        if !unique.insert(specification.id.clone()) || specification.role != handle.role() {
            return Err(topology("invalid attached provider bindings"));
        }
    }
    Ok(())
}

pub(super) fn observe(
    graph: &mut ComputationGraph,
    component: ComponentId,
    generation: ComponentGeneration,
    bindings: Vec<(ResourceSpecification, ResourceHandle)>,
) -> GraphResult<()> {
    graph
        .ids
        .get(&component)
        .ok_or(GraphError::StaleGeneration)?;
    if !graph
        .observed()
        .components
        .get(&component)
        .is_some_and(|node| node.generation == generation)
    {
        return Err(GraphError::StaleGeneration);
    }
    validate_bindings(&bindings)?;

    let revision = GraphRevision(
        graph
            .snapshot
            .revision
            .0
            .checked_add(1)
            .ok_or_else(|| topology("graph revision exhausted"))?,
    );
    let next = graph
        .next_resource_generation
        .checked_add(bindings.len() as u64)
        .ok_or_else(|| topology("resource generation exhausted"))?;
    let previous = graph
        .reported_resources
        .get(&component)
        .cloned()
        .unwrap_or_default();
    if let Some(attached) = graph.snapshot.component_resources.get_mut(&component) {
        attached.retain(|id| !previous.contains(id));
    }
    let mut reported = BTreeSet::new();
    let mut observed = Vec::new();
    for (offset, (mut specification, handle)) in bindings.into_iter().enumerate() {
        if let Some(existing) = graph
            .resource_handles
            .iter()
            .find(|(_, existing)| same_provider(existing, &handle))
            .map(|(id, _)| id.clone())
        {
            if !graph
                .snapshot
                .component_resources
                .get(&component)
                .is_some_and(|resources| resources.contains(&existing))
            {
                reported.insert(existing.clone());
            }
            graph
                .snapshot
                .component_resources
                .entry(component.clone())
                .or_default()
                .insert(existing);
            continue;
        }
        let mut id = specification.id.clone();
        if graph.snapshot.resources.contains_key(&id) {
            id = ResourceId::try_new(format!(
                "{id}/{}",
                graph.next_resource_generation + offset as u64
            ))?;
            while graph.snapshot.resources.contains_key(&id) {
                id = ResourceId::try_new(format!("{id}/next"))?;
            }
            specification.id = id.clone();
        }
        graph.snapshot.resources.insert(id.clone(), specification);
        graph.resource_handles.insert(id.clone(), handle);
        graph
            .snapshot
            .component_resources
            .entry(component.clone())
            .or_default()
            .insert(id.clone());
        reported.insert(id.clone());
        observed.push((
            id,
            ObservedResource {
                realization: ResourceRealization::Created,
                revision,
                generation: graph.next_resource_generation + offset as u64,
                transition_time: chrono::Utc::now(),
                failure: None,
            },
        ));
    }
    graph.next_resource_generation = next;
    graph.reported_resources.insert(component.clone(), reported);
    let mut released = Vec::new();
    for id in previous {
        if id.as_str().starts_with("attached/")
            && graph.snapshot.resources.get(&id).is_some_and(|resource| {
                resource.ownership == ResourceOwnership::Borrowed
            })
            && !graph.snapshot.component_resources.values().any(|resources| resources.contains(&id))
            && !graph.snapshot.specifications.values().any(|spec| {
                spec.dependencies.values().flatten().any(|resource| resource == &id)
                    || spec.configuration.values().any(|value| matches!(
                        value, super::ConfigurationValue::Reference { resource, .. } if resource == &id
                    ))
            })
            && !graph.snapshot.edges.iter().any(|edge| edge.resources.contains_key(&id))
        {
            graph.snapshot.resources.remove(&id);
            graph.resource_handles.remove(&id);
            released.push(id);
        }
    }
    graph.snapshot.revision = revision;
    controller::update(graph, |state| {
        state.revision = revision;
        state.resources.extend(observed);
        for id in released {
            state.resources.remove(&id);
        }
    });
    graph.desired.send_replace(Arc::new(graph.snapshot.clone()));
    Ok(())
}

pub(super) fn observe_plugin(
    graph: &mut ComputationGraph,
    component: &ComponentId,
    generation: ComponentGeneration,
    plugin: super::PluginIdentity,
) -> GraphResult<()> {
    let state = graph.observed();
    if !state
        .components
        .get(component)
        .is_some_and(|node| node.generation == generation)
    {
        return Err(GraphError::StaleGeneration);
    }
    validate_plugin(&graph.snapshot, component, &plugin)?;
    if graph.snapshot.component_plugins.get(component) == Some(&plugin) {
        return Ok(());
    }
    let revision = graph
        .snapshot
        .revision
        .0
        .checked_add(1)
        .ok_or_else(|| topology("graph revision exhausted"))?;
    graph.snapshot.revision = GraphRevision(revision);
    graph
        .snapshot
        .component_plugins
        .insert(component.clone(), plugin);
    graph.desired.send_replace(Arc::new(graph.snapshot.clone()));
    controller::update(graph, |state| state.revision = GraphRevision(revision));
    Ok(())
}

fn validate_plugin(
    snapshot: &GraphSnapshot,
    component: &ComponentId,
    plugin: &PluginIdentity,
) -> GraphResult<()> {
    validate_identifier("plugin", &plugin.id)?;
    validate_identifier("plugin version", &plugin.version)?;
    let declared = snapshot
        .nodes
        .iter()
        .find(|node| node.descriptor.id() == component)
        .and_then(|node| node.descriptor.plugin_identity())
        .or_else(|| {
            snapshot
                .specifications
                .get(component)
                .and_then(|spec| spec.implementation.plugin.as_ref())
        })
        .or_else(|| snapshot.component_plugins.get(component));
    if declared.is_some_and(|declared| declared != plugin) {
        return Err(GraphError::Validation {
            component: component.clone(),
            source: anyhow::anyhow!("reported plugin identity differs from declared provenance"),
        });
    }
    Ok(())
}

pub(super) fn same_provider(left: &ResourceHandle, right: &ResourceHandle) -> bool {
    left.same_shared_instance(right)
}
