// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::super::{
    resolve, validate_orphan, ComponentConstruction, ComponentSpecification, ConstructedComponent,
    DesiredComponent, DesiredPipe, DesiredRelationship, DesiredTopology, EdgeDefinition,
    EdgeSnapshot, Endpoint, FlowProgress, ImplementationIdentity, ResourceHandle,
    ResourceSpecification, TopologyBindings,
};
use super::*;
use crate::computation::v1::{
    validate_connection, validate_sink_completion, ComponentDescriptor, ComponentFactory,
    ComponentRole, ConfigurationValue, PipeCapabilities, PipeCapability, PipeProvider,
    PipeRequirements, PortDirection, RemovalPolicy, ResourceId,
};
use std::{future::Future, num::NonZeroUsize};

/// Desired mutations are descriptions, not constructed instances or resolved secrets.
/// Supply new external instances/providers separately when executing the preview.
#[derive(Debug, Clone)]
pub enum DesiredMutation {
    PutComponent(DesiredComponent),
    /// Replace even when the desired description is unchanged.
    ReplaceComponent(DesiredComponent),
    /// Explicit capability-backed update; unsupported updates are rejected.
    UpdateComponent(DesiredComponent),
    RemoveComponents {
        selection: GraphSelection,
        policy: RemovalPolicy,
    },
    Bind(DesiredRelationship),
    Unbind {
        edge: EdgeDefinition,
        policy: RemovalPolicy,
    },
    PutResource(ResourceSpecification),
    RebindResource(ResourceId),
    RemoveResource {
        resource: ResourceId,
        policy: RemovalPolicy,
    },
    Restart(GraphSelection),
    Retry(GraphSelection),
}

/// Immutable impact at one desired revision and set of construction/operation epochs.
/// Execution repeats preflight against actual bindings before changing anything.
#[derive(Debug, Clone)]
pub struct ReconciliationPreview {
    revision: GraphRevision,
    desired: DesiredTopology,
    changes: Vec<DesiredMutation>,
    create: BTreeSet<ComponentId>,
    replace: BTreeSet<ComponentId>,
    update: BTreeSet<ComponentId>,
    remove: BTreeSet<ComponentId>,
    restart: BTreeSet<ComponentId>,
    reactivate: BTreeSet<ComponentId>,
    pause: BTreeSet<ComponentId>,
    rebind: BTreeSet<EdgeDefinition>,
    unbind: BTreeSet<EdgeDefinition>,
    resources: BTreeSet<ResourceId>,
    remove_resources: BTreeSet<ResourceId>,
    drain: BTreeSet<ComponentId>,
    epochs: BTreeMap<ComponentId, (ComponentGeneration, OperationEpoch)>,
}

impl ReconciliationPreview {
    pub fn revision(&self) -> GraphRevision {
        self.revision
    }
    pub fn desired(&self) -> &DesiredTopology {
        &self.desired
    }
    pub fn created(&self) -> &BTreeSet<ComponentId> {
        &self.create
    }
    pub fn replaced(&self) -> &BTreeSet<ComponentId> {
        &self.replace
    }
    pub fn updated(&self) -> &BTreeSet<ComponentId> {
        &self.update
    }
    pub fn removed(&self) -> &BTreeSet<ComponentId> {
        &self.remove
    }
    pub fn restarted(&self) -> &BTreeSet<ComponentId> {
        &self.restart
    }
    pub fn resumed(&self) -> &BTreeSet<ComponentId> {
        &self.reactivate
    }
    /// Pausing does not call stop/start, discard admitted data, or change generation.
    pub fn paused(&self) -> &BTreeSet<ComponentId> {
        &self.pause
    }
    pub fn rebound(&self) -> &BTreeSet<EdgeDefinition> {
        &self.rebind
    }
    pub fn unbound(&self) -> &BTreeSet<EdgeDefinition> {
        &self.unbind
    }
}

#[derive(Debug, Clone)]
pub struct ReconciliationReport {
    pub revision: GraphRevision,
    pub summary: OperationSummary,
    /// Cleanup failure leaves the previous desired topology intact. Successfully
    /// stopped components remain stopped; this is not a rollback claim.
    pub committed: bool,
    pub creation: BTreeMap<ComponentId, CreationOutcome>,
    pub startup: Option<StartReport>,
    pub removed: BTreeSet<ComponentId>,
    pub failures: Vec<ComponentFailure>,
}

impl GraphControl {
    pub async fn preview(
        &self,
        revision: GraphRevision,
        changes: Vec<DesiredMutation>,
    ) -> GraphResult<ReconciliationPreview> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Preview {
                revision,
                changes,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub async fn reconcile(
        &self,
        preview: ReconciliationPreview,
        bindings: TopologyBindings,
    ) -> GraphResult<ReconciliationReport> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Reconcile {
                preview,
                bindings,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }
}

fn selected_ids(
    graph: &ComputationGraph,
    selection: &GraphSelection,
) -> GraphResult<BTreeSet<ComponentId>> {
    Ok(select(graph, selection)?
        .into_iter()
        .map(|index| graph.nodes[index].descriptor.id().clone())
        .collect())
}

fn dependent_closure(desired: &DesiredTopology, selected: &mut BTreeSet<ComponentId>) {
    loop {
        let before = selected.len();
        for edge in &desired.relationships {
            if selected.contains(&edge.definition.from.component) {
                selected.insert(edge.definition.to.component.clone());
            }
        }
        if before == selected.len() {
            break;
        }
    }
}

fn resource_users(desired: &DesiredTopology, resource: &ResourceId) -> BTreeSet<ComponentId> {
    desired.components.iter().filter_map(|node| {
        let ComponentConstruction::Factory(spec) = &node.construction else { return None; };
        (spec.dependencies.values().flatten().any(|id| id == resource)
            || spec.configuration.values().any(|value| matches!(value, ConfigurationValue::Reference { resource: id, .. } if id == resource)))
            .then(|| node.descriptor.id().clone())
    }).collect()
}

fn remove_components(
    desired: &mut DesiredTopology,
    mut selected: BTreeSet<ComponentId>,
    policy: RemovalPolicy,
) -> GraphResult<BTreeSet<ComponentId>> {
    if matches!(policy, RemovalPolicy::Cascade | RemovalPolicy::Drain) {
        dependent_closure(desired, &mut selected);
    }
    let mut retained = Vec::new();
    for relationship in &desired.relationships {
        let from = selected.contains(&relationship.definition.from.component);
        let to = selected.contains(&relationship.definition.to.component);
        if !from && !to {
            retained.push(relationship.clone());
        } else if from != to {
            match policy {
                RemovalPolicy::Reject => {
                    return Err(topology("removal would leave a relationship dependency"))
                }
                RemovalPolicy::Orphan => {
                    if !relationship.policy.orphan_permitted
                        || relationship.policy.required_for_binding
                        || relationship.policy.required_for_creation
                    {
                        return Err(topology("relationship does not permit orphaning"));
                    }
                    desired.boundary_relationships.push(relationship.clone());
                }
                RemovalPolicy::Cascade | RemovalPolicy::Drain => {}
            }
        }
    }
    desired.relationships = retained;
    desired
        .components
        .retain(|node| !selected.contains(node.descriptor.id()));
    desired.boundary_relationships.retain(|edge| {
        desired.components.iter().any(|node| {
            node.descriptor.id() == &edge.definition.from.component
                || node.descriptor.id() == &edge.definition.to.component
        })
    });
    Ok(selected)
}

pub(super) fn preview(
    graph: &ComputationGraph,
    changes: Vec<DesiredMutation>,
) -> GraphResult<ReconciliationPreview> {
    let mut desired = graph.snapshot.select(GraphSelection::All)?;
    let old = desired.clone();
    let mut forced = BTreeSet::new();
    let mut updating = BTreeSet::new();
    let mut restart = BTreeSet::new();
    let mut resources = BTreeSet::new();
    let mut remove_resources = BTreeSet::new();
    let mut drain = BTreeSet::new();
    let mut explicit_binds = BTreeSet::new();
    for change in &changes {
        match change {
            DesiredMutation::PutComponent(node)
            | DesiredMutation::ReplaceComponent(node)
            | DesiredMutation::UpdateComponent(node) => {
                let id = node.descriptor.id();
                if matches!(change, DesiredMutation::ReplaceComponent(_)) {
                    forced.insert(id.clone());
                }
                if matches!(change, DesiredMutation::UpdateComponent(_)) {
                    updating.insert(id.clone());
                }
                if let Some(existing) = desired
                    .components
                    .iter_mut()
                    .find(|existing| existing.descriptor.id() == id)
                {
                    *existing = node.clone();
                } else {
                    desired.components.push(node.clone());
                }
            }
            DesiredMutation::RemoveComponents { selection, policy } => {
                let removed =
                    remove_components(&mut desired, selected_ids(graph, selection)?, *policy)?;
                if *policy == RemovalPolicy::Drain {
                    drain.extend(removed);
                }
            }
            DesiredMutation::Bind(edge) => {
                explicit_binds.insert(edge.definition.clone());
                desired
                    .boundary_relationships
                    .retain(|prior| prior.definition != edge.definition);
                if let Some(prior) = desired
                    .relationships
                    .iter_mut()
                    .find(|prior| prior.definition == edge.definition)
                {
                    *prior = edge.clone();
                } else {
                    desired.relationships.push(edge.clone());
                }
            }
            DesiredMutation::Unbind { edge, policy } => {
                let index = desired
                    .relationships
                    .iter()
                    .position(|prior| &prior.definition == edge)
                    .ok_or_else(|| topology("cannot unbind an undeclared relationship"))?;
                let removed = desired.relationships.remove(index);
                if *policy == RemovalPolicy::Orphan {
                    desired.boundary_relationships.push(removed);
                } else if *policy == RemovalPolicy::Cascade {
                    remove_components(
                        &mut desired,
                        BTreeSet::from([edge.to.component.clone()]),
                        *policy,
                    )?;
                } else if *policy == RemovalPolicy::Drain {
                    drain.insert(edge.to.component.clone());
                }
            }
            DesiredMutation::PutResource(spec) => {
                super::super::validate_identifier("resource binding", &spec.binding)?;
                if let Some(prior) = desired
                    .resources
                    .iter_mut()
                    .find(|prior| prior.id == spec.id)
                {
                    if prior != spec {
                        resources.insert(spec.id.clone());
                    }
                    *prior = spec.clone();
                } else {
                    resources.insert(spec.id.clone());
                    desired.resources.push(spec.clone());
                }
            }
            DesiredMutation::RebindResource(id) => {
                if !desired.resources.iter().any(|resource| &resource.id == id) {
                    return Err(topology(format!("resource {id} was not declared")));
                }
                resources.insert(id.clone());
            }
            DesiredMutation::RemoveResource { resource, policy } => {
                let users = resource_users(&desired, resource);
                let mut users = users;
                for edge in desired
                    .relationships
                    .iter()
                    .chain(&desired.boundary_relationships)
                    .filter(|edge| edge.pipe.resource_dependencies().contains_key(resource))
                {
                    for id in [&edge.definition.from.component, &edge.definition.to.component] {
                        if desired
                            .components
                            .iter()
                            .any(|node| node.descriptor.id() == id)
                        {
                            users.insert(id.clone());
                        }
                    }
                }
                if !users.is_empty() {
                    if matches!(policy, RemovalPolicy::Cascade | RemovalPolicy::Drain) {
                        let removed = remove_components(&mut desired, users, *policy)?;
                        if *policy == RemovalPolicy::Drain {
                            drain.extend(removed);
                        }
                    } else {
                        return Err(topology("resource is still referenced; remove optional references explicitly before orphaning"));
                    }
                }
                let before = desired.resources.len();
                desired.resources.retain(|spec| &spec.id != resource);
                if before == desired.resources.len() {
                    return Err(topology("unknown resource"));
                }
                remove_resources.insert(resource.clone());
            }
            DesiredMutation::Restart(selection) => restart.extend(selected_ids(graph, selection)?),
            DesiredMutation::Retry(selection) => {
                for id in selected_ids(graph, selection)? {
                    let observed = &graph.observed().components[&id];
                    let error = observed.failure.as_ref().ok_or_else(|| {
                        topology(format!("component {id} has no visible failure to retry"))
                    })?;
                    if error.disposition != FailureDisposition::Retryable {
                        return Err(topology(
                            "terminal failure requires a specification change or removal",
                        ));
                    }
                    if error.phase == FailurePhase::Activation || error.phase == FailurePhase::Stop
                    {
                        restart.insert(id);
                    } else {
                        forced.insert(id);
                    }
                }
            }
        }
    }
    let old_nodes: BTreeMap<_, _> = old
        .components
        .iter()
        .map(|node| (node.descriptor.id().clone(), node))
        .collect();
    let new_nodes: BTreeMap<_, _> = desired
        .components
        .iter()
        .map(|node| (node.descriptor.id().clone(), node))
        .collect();
    let remove: BTreeSet<_> = old_nodes
        .keys()
        .filter(|id| !new_nodes.contains_key(*id))
        .cloned()
        .collect();
    let create: BTreeSet<_> = new_nodes
        .keys()
        .filter(|id| !old_nodes.contains_key(*id))
        .cloned()
        .collect();
    let mut replace = BTreeSet::new();
    let mut pause = BTreeSet::new();
    for (id, node) in &new_nodes {
        let Some(old) = old_nodes.get(id) else {
            if updating.contains(id) {
                return Err(topology("cannot update an unconstructed component"));
            }
            continue;
        };
        let structural = node.descriptor != old.descriptor
            || node.role != old.role
            || node.completion != old.completion
            || node.streams != old.streams;
        let definition_changed = structural || node.construction != old.construction;
        if forced.contains(id)
            && !definition_changed
            && graph.observed().components[id]
                .failure
                .as_ref()
                .is_some_and(|error| {
                    error.phase == FailurePhase::Creation
                        && error.disposition == FailureDisposition::Terminal
                })
        {
            return Err(topology(
                "terminal creation failure requires a changed specification",
            ));
        }
        if updating.contains(id) {
            let compatible = match (&old.construction, &node.construction) {
                (ComponentConstruction::Factory(old), ComponentConstruction::Factory(new)) => {
                    old.implementation == new.implementation && old.dependencies == new.dependencies
                }
                _ => false,
            };
            if structural || !compatible {
                return Err(topology(
                    "in-place update cannot change interfaces, resources or implementation",
                ));
            }
        } else if definition_changed || forced.contains(id) {
            replace.insert(id.clone());
        }
        if node.input_merge != old.input_merge || definition_changed {
            pause.insert(id.clone());
        }
    }
    for id in &resources {
        replace.extend(
            resource_users(&desired, id)
                .into_iter()
                .filter(|id| old_nodes.contains_key(id)),
        );
    }
    if updating
        .iter()
        .any(|id| replace.contains(id) || remove.contains(id))
    {
        return Err(topology("conflicting update and replacement/removal"));
    }
    restart.retain(|id| !remove.contains(id) && !replace.contains(id));
    let old_edges: BTreeMap<_, _> = old
        .relationships
        .iter()
        .map(|edge| (edge.definition.clone(), edge))
        .collect();
    let new_edges: BTreeMap<_, _> = desired
        .relationships
        .iter()
        .map(|edge| (edge.definition.clone(), edge))
        .collect();
    let unbind: BTreeSet<_> = old_edges
        .keys()
        .filter(|edge| !new_edges.contains_key(*edge))
        .cloned()
        .collect();
    let mut rebind = BTreeSet::new();
    let mut reactivate = BTreeSet::new();
    for (edge, description) in &new_edges {
        let rebound = old_edges
            .get(edge)
            .map_or(true, |old| old.pipe != description.pipe);
        let resource_changed = graph
            .snapshot
            .edges
            .iter()
            .find(|prior| &prior.definition == edge)
            .is_some_and(|prior| prior.resources.keys().any(|id| resources.contains(id)));
        let missing_binding = graph
            .observed()
            .relationships
            .get(edge)
            .is_some_and(|observed| observed.binding != BindingState::Bound);
        if rebound
            || explicit_binds.contains(edge)
            || replace.contains(&edge.from.component)
            || resource_changed
            || (missing_binding && replace.contains(&edge.to.component))
            || ([&edge.from.component, &edge.to.component].iter().any(|id| {
                restart.contains(*id)
                    && graph
                        .observed()
                        .components
                        .get(*id)
                        .is_some_and(|state| state.exhausted)
            }))
        {
            rebind.insert(edge.clone());
        }
    }
    loop {
        let before = rebind.len();
        for edge in &rebind {
            if graph
                .observed()
                .components
                .get(&edge.to.component)
                .is_some_and(|state| state.exhausted)
            {
                reactivate.insert(edge.to.component.clone());
            }
        }
        for edge in new_edges.keys() {
            if reactivate.contains(&edge.from.component) {
                rebind.insert(edge.clone());
            }
        }
        if before == rebind.len() {
            break;
        }
    }
    for edge in rebind.iter().chain(&unbind) {
        pause.insert(edge.from.component.clone());
        pause.insert(edge.to.component.clone());
        if let Some(prior) = old_edges.get(edge) {
            if !prior.policy.dynamically_replaceable {
                restart.insert(edge.from.component.clone());
                restart.insert(edge.to.component.clone());
            }
        }
    }
    pause.extend(
        replace
            .iter()
            .chain(&updating)
            .chain(&remove)
            .chain(&restart)
            .cloned(),
    );
    pause.retain(|id| old_nodes.contains_key(id));
    restart.retain(|id| !remove.contains(id) && !replace.contains(id));
    if !drain.is_empty() {
        let mut boundary = drain.clone();
        dependent_closure(&old, &mut boundary);
        if boundary.iter().any(|id| {
            graph.observed().components.get(id).is_some_and(|state| {
                state
                    .failure
                    .as_ref()
                    .is_some_and(|error| error.phase == FailurePhase::Processing)
            })
        }) {
            return Err(topology(
                "cannot promise handled drain while a selected processing boundary has failed",
            ));
        }
        if old.components.iter().any(|node| {
            boundary.contains(node.descriptor.id())
                && node.completion == Some(crate::computation::v1::SinkCompletion::Accepted)
        }) {
            return Err(topology(
                "cannot promise handled drain through an acceptance-only sink",
            ));
        }
    }
    desired.revision = GraphRevision(
        graph
            .snapshot
            .revision
            .0
            .checked_add(1)
            .ok_or_else(|| topology("graph revision exhausted"))?,
    );
    describe(&desired)?;
    let epochs = pause
        .iter()
        .map(|id| {
            let observed = &graph.observed().components[id];
            (id.clone(), (observed.generation, observed.operation))
        })
        .collect();
    Ok(ReconciliationPreview {
        revision: graph.snapshot.revision,
        desired,
        changes,
        create,
        replace,
        update: updating,
        remove,
        restart,
        reactivate,
        pause,
        rebind,
        unbind,
        resources,
        remove_resources,
        drain,
        epochs,
    })
}

fn desired_capabilities(pipe: &DesiredPipe) -> GraphResult<PipeCapabilities> {
    match pipe {
        DesiredPipe::External {
            capabilities,
            capacity,
            ..
        } => {
            let capacity = capacity
                .map(|capacity| {
                    NonZeroUsize::new(capacity)
                        .ok_or_else(|| topology("invalid external pipe capacity"))
                })
                .transpose()?;
            Ok(PipeCapabilities::try_new(
                capabilities.iter().copied(),
                capacity,
            )?)
        }
        _ => {
            let provider = pipe.resolve(&mut TopologyBindings::default())?;
            provider
                .capabilities()
                .map_err(|source| GraphError::Pipe { edge: 0, source })
        }
    }
}

fn describe(
    desired: &DesiredTopology,
) -> GraphResult<(Vec<NodeSnapshot>, Vec<EdgeSnapshot>, Vec<usize>)> {
    let mut ids = BTreeMap::new();
    let mut streams = BTreeSet::new();
    let mut nodes = Vec::new();
    for (index, node) in desired.components.iter().enumerate() {
        let id = node.descriptor.id();
        if ids.insert(id.clone(), index).is_some() {
            return Err(topology("duplicate desired component"));
        }
        super::super::validate_role(&node.descriptor, node.role, node.completion)?;
        for port in node.descriptor.ports() {
            if port.direction() == PortDirection::Output {
                let stream = node
                    .streams
                    .get(port.id())
                    .ok_or_else(|| topology("output port must bind a stream"))?;
                if !streams.insert(stream.clone()) {
                    return Err(topology("duplicate output stream"));
                }
            } else if node.streams.contains_key(port.id()) {
                return Err(topology("input cannot bind a producer stream"));
            }
        }
        if node.streams.keys().any(|port| {
            !node
                .descriptor
                .ports()
                .iter()
                .any(|descriptor| descriptor.id() == port)
        }) {
            return Err(topology("stream binding names an undeclared port"));
        }
        nodes.push(NodeSnapshot {
            descriptor: node.descriptor.clone(),
            role: node.role,
            completion: node.completion,
            output_streams: node.streams.clone(),
            input_merge: node.input_merge,
        });
    }
    let mut connected = BTreeSet::new();
    let mut unique = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    let mut edges = Vec::new();
    for (index, edge) in desired.relationships.iter().enumerate() {
        if !unique.insert(edge.definition.clone()) {
            return Err(topology("duplicate relationship"));
        }
        if !destinations.insert((
            edge.definition.from.clone(),
            edge.definition.to.component.clone(),
        )) {
            return Err(topology(
                "one producer output cannot feed multiple ports on the same component",
            ));
        }
        let (_, output) = resolve(&ids, &nodes, &edge.definition.from)?;
        let (to, input) = resolve(&ids, &nodes, &edge.definition.to)?;
        let capabilities = desired_capabilities(&edge.pipe)?;
        super::super::validate_edge_contract(
            output,
            input,
            nodes[to].completion,
            &capabilities,
            &desired.requirements,
        )?;
        connected.insert(edge.definition.from.clone());
        connected.insert(edge.definition.to.clone());
        let resources = edge.pipe.resource_dependencies();
        for (id, role) in &resources {
            if !desired
                .resources
                .iter()
                .any(|resource| &resource.id == id && &resource.role == role)
            {
                return Err(topology(format!(
                    "edge {index} requires an undeclared or incompatible resource"
                )));
            }
        }
        edges.push(EdgeSnapshot {
            definition: edge.definition.clone(),
            capabilities,
            policy: edge.policy.clone(),
            resources,
            pipe: edge.pipe.clone(),
        });
    }
    for edge in &desired.boundary_relationships {
        if !unique.insert(edge.definition.clone()) {
            return Err(topology("duplicate bound/unbound relationship"));
        }
        validate_orphan(edge, &ids, &nodes)?;
        for (id, role) in edge.pipe.resource_dependencies() {
            if !desired
                .resources
                .iter()
                .any(|resource| resource.id == id && resource.role == role)
            {
                return Err(topology(
                    "unbound relationship requires an undeclared or incompatible resource",
                ));
            }
        }
        connected.insert(edge.definition.from.clone());
        connected.insert(edge.definition.to.clone());
    }
    for node in &nodes {
        for port in node.descriptor.ports() {
            if !connected.contains(&Endpoint::new(
                node.descriptor.id().clone(),
                port.id().clone(),
            )) {
                return Err(topology("mutation would leave a port unbound without an explicit permissible orphan relationship"));
            }
        }
    }
    let order = super::super::dependency_order(&ids, &edges)?;
    let mut resource_ids = BTreeSet::new();
    for resource in &desired.resources {
        if !resource_ids.insert(resource.id.clone()) {
            return Err(topology("duplicate resource"));
        }
        super::super::validate_identifier("resource binding", &resource.binding)?;
    }
    Ok((nodes, edges, order))
}

struct Prepared {
    nodes: Vec<NodeSnapshot>,
    edges: Vec<EdgeSnapshot>,
    order: Vec<usize>,
    components: BTreeMap<ComponentId, Component>,
    pipes: BTreeMap<EdgeDefinition, Box<dyn PipeProvider>>,
    resources: BTreeMap<ResourceId, ResourceHandle>,
    factories: BTreeMap<ImplementationIdentity, Arc<dyn ComponentFactory>>,
}

fn prepare(
    graph: &ComputationGraph,
    plan: &ReconciliationPreview,
    mut bindings: TopologyBindings,
) -> GraphResult<Prepared> {
    graph
        .next_generation
        .checked_add((plan.create.len() + plan.replace.len()) as u64)
        .ok_or_else(|| topology("construction generation exhausted"))?;
    graph
        .next_edge
        .checked_add(plan.rebind.len())
        .ok_or_else(|| topology("relationship index exhausted"))?;
    graph
        .next_binding_generation
        .checked_add(plan.rebind.len() as u64)
        .ok_or_else(|| topology("binding generation exhausted"))?;
    graph
        .next_resource_generation
        .checked_add(plan.resources.len() as u64)
        .ok_or_else(|| topology("resource generation exhausted"))?;
    for id in &plan.pause {
        graph.observed().components[id]
            .operation
            .0
            .checked_add(3)
            .ok_or_else(|| topology("operation epoch exhausted"))?;
    }
    let (nodes, mut edges, order) = describe(&plan.desired)?;
    let mut factories = graph.factories.clone();
    for (identity, factory) in std::mem::take(&mut bindings.factories.factories) {
        if factories
            .get(&identity)
            .is_some_and(|prior| !Arc::ptr_eq(prior, &factory))
        {
            return Err(topology(
                "an implementation identity cannot be rebound to a different factory",
            ));
        }
        factories.insert(identity, factory);
    }
    let declarations: BTreeMap<_, _> = plan
        .desired
        .resources
        .iter()
        .map(|spec| (spec.id.clone(), spec.clone()))
        .collect();
    let mut resources = graph.resource_handles.clone();
    for id in &plan.remove_resources {
        resources.remove(id);
    }
    for (id, handle) in std::mem::take(&mut bindings.resources) {
        if !plan.resources.contains(&id) {
            return Err(topology("resource binding was not included in the preview"));
        }
        let declaration = declarations
            .get(&id)
            .ok_or_else(|| topology("resource binding is undeclared"))?;
        if declaration.role != handle.role() {
            return Err(topology("resource binding role mismatch"));
        }
        resources.insert(id, handle);
    }
    for id in &plan.resources {
        if graph.resource_handles.contains_key(id)
            && !plan.remove_resources.contains(id)
            && resources
                .get(id)
                .is_some_and(|new| graph.resource_handles[id].same_instance(new))
        {
            return Err(topology(
                "resource replacement requires a new explicit constructed binding",
            ));
        }
    }
    let mut components = BTreeMap::new();
    for node in &plan.desired.components {
        let id = node.descriptor.id();
        let construct = plan.create.contains(id) || plan.replace.contains(id);
        match &node.construction {
            ComponentConstruction::Factory(spec) => {
                if spec.descriptor != node.descriptor
                    || spec.role != node.role
                    || spec.completion != node.completion
                {
                    return Err(topology(
                        "factory specification and desired interface disagree",
                    ));
                }
                let factory = factories
                    .get(&spec.implementation)
                    .ok_or_else(|| topology("implementation is not registered"))?;
                super::super::specification::validate_specification(
                    &plan.desired.graph_id,
                    spec,
                    factory.as_ref(),
                    &declarations,
                    &resources,
                )?;
                if plan.update.contains(id) && !factory.supports_reconfiguration() {
                    return Err(topology(
                        "factory does not support in-place reconfiguration",
                    ));
                }
                if construct {
                    components.insert(
                        id.clone(),
                        Component::Deferred {
                            specification: Arc::new(spec.clone()),
                            factory: factory.clone(),
                        },
                    );
                }
            }
            ComponentConstruction::External { binding } if construct => {
                let component = bindings.components.remove(binding).ok_or_else(|| {
                    topology(format!("external component binding {binding} is required"))
                })?;
                if component.0.descriptor() != &node.descriptor
                    || component.0.role() != node.role
                    || component.0.completion() != node.completion
                {
                    return Err(topology("external component interface mismatch"));
                }
                components.insert(id.clone(), component.0);
            }
            ComponentConstruction::External { .. } => {}
        }
    }
    let mut pipes = BTreeMap::new();
    let mut used_resources = BTreeMap::new();
    for edge in &mut edges {
        if plan.rebind.contains(&edge.definition) {
            pipes.insert(edge.definition.clone(), edge.pipe.resolve(&mut bindings)?);
        }
        let provider: &dyn PipeProvider = if let Some(provider) = pipes.get(&edge.definition) {
            provider.as_ref()
        } else {
            let index = graph
                .edges
                .iter()
                .find(|(_, old)| old.definition == edge.definition)
                .map(|(index, _)| index)
                .ok_or_else(|| topology("missing retained provider"))?;
            graph.providers[index].as_ref()
        };
        provider
            .validate_resources(&resources)
            .map_err(|source| GraphError::Pipe { edge: 0, source })?;
        if provider
            .capabilities()
            .map_err(|source| GraphError::Pipe { edge: 0, source })?
            != edge.capabilities
        {
            return Err(topology(
                "actual provider capabilities differ from the preview",
            ));
        }
        edge.resources = provider.resource_dependencies();
        let exclusive = provider.exclusive_resources();
        for (id, role) in &edge.resources {
            if declarations.get(id).map(|spec| spec.role) != Some(*role) {
                return Err(topology("pipe resource dependency mismatch"));
            }
            let exclusive = exclusive.contains(id);
            if used_resources
                .get(id)
                .is_some_and(|prior| *prior || exclusive)
            {
                return Err(topology(
                    "pipe resource requires exclusive binding ownership",
                ));
            }
            used_resources.insert(id.clone(), exclusive);
        }
    }
    if !bindings.components.is_empty() || !bindings.pipes.is_empty() {
        return Err(topology("unused external component or pipe binding"));
    }
    Ok(Prepared {
        nodes,
        edges,
        order,
        components,
        pipes,
        resources,
        factories,
    })
}

async fn drive<T>(
    graph: &ComputationGraph,
    operations: &mut Operations,
    controls: &PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    operation: impl Future<Output = GraphResult<T>>,
) -> GraphResult<T> {
    let operation = tokio::time::timeout(graph.cleanup_timeout, operation);
    tokio::pin!(operation);
    loop {
        operations.advance_start(graph)?;
        operations.advance_stop(graph)?;
        tokio::select! {
            biased;
            _ = cancelled(cancel) => return Err(GraphError::Cancelled),
            result = &mut operation => return result.map_err(|_| GraphError::ReconciliationTimeout)?,
            completion = operations.futures.next(), if !operations.futures.is_empty() => {
                if let Some(completion) = completion { operations.complete(graph, completion, controls)?; }
            }
        }
    }
}

async fn pause(
    graph: &ComputationGraph,
    operations: &mut Operations,
    controls: &PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    index: usize,
) -> GraphResult<()> {
    if let Some(active) = operations.active.get(&index) {
        if active.operation != Operation::Process {
            return Err(GraphError::OperationInProgress);
        }
        operations.quiescing.insert(index);
        active.quiesce.send_replace(true);
        update(graph, |state| {
            state
                .components
                .get_mut(graph.nodes[index].descriptor.id())
                .expect("component")
                .lifecycle = ComponentLifecycle::Quiescing
        });
        let deadline = tokio::time::sleep(graph.cleanup_timeout);
        tokio::pin!(deadline);
        while operations.active.contains_key(&index) {
            tokio::select! {
                biased;
                _ = cancelled(cancel) => return Err(GraphError::Cancelled),
                _ = &mut deadline => {
                    if let Some(active) = operations.active.get(&index) { active.quiesce.send_replace(false); }
                    operations.quiescing.remove(&index);
                    update(graph, |state| state.components.get_mut(graph.nodes[index].descriptor.id()).expect("component").lifecycle = ComponentLifecycle::Running);
                    return Err(GraphError::ReconciliationTimeout);
                }
                completion = operations.futures.next() => {
                    if let Some(completion) = completion { operations.complete(graph, completion, controls)?; }
                }
            }
        }
    }
    Ok(())
}

fn resume(
    graph: &ComputationGraph,
    operations: &mut Operations,
    ids: &BTreeSet<ComponentId>,
) -> GraphResult<()> {
    for id in ids {
        let Some(&index) = graph.ids.get(id) else {
            continue;
        };
        if operations.paused.contains(&index)
            && matches!(
                graph.observed().components[id].lifecycle,
                ComponentLifecycle::Running | ComponentLifecycle::Quiesced
            )
            && binding_dependencies(graph, id).is_empty()
            && !operations.invalidated_bindings.iter().any(|edge| {
                graph.edges.get(edge).is_some_and(|edge| {
                    &edge.definition.from.component == id || &edge.definition.to.component == id
                })
            })
            && !operations.exhausted.contains(&index)
        {
            operations.paused.remove(&index);
            update(graph, |state| {
                state.components.get_mut(id).expect("component").lifecycle =
                    ComponentLifecycle::Running
            });
            operations.launch(graph, index, Operation::Process)?;
        }
    }
    Ok(())
}

async fn stop_instance(
    graph: &ComputationGraph,
    operations: &mut Operations,
    controls: &PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    index: usize,
) -> GraphResult<()> {
    let slot = graph.components[index].clone();
    let node = graph.nodes[index].clone();
    let mut lease = slot.take()?;
    if !lease.attempted {
        return Ok(());
    }
    update(graph, |state| {
        let observed = state.components.get_mut(&slot.id).expect("component");
        observed.lifecycle = ComponentLifecycle::Stopping;
        observed.operation = OperationEpoch(observed.operation.0 + 1);
        observed.transition_time = Utc::now();
    });
    let result = drive(graph, operations, controls, cancel, async {
        lease
            .component
            .stop()
            .await
            .map_err(|source| component_error(&node, "stop", source))
    })
    .await;
    if result.is_ok() {
        lease.attempted = false;
    }
    update(graph, |state| {
        let observed = state.components.get_mut(&slot.id).expect("component");
        observed.lifecycle = if result.is_ok() {
            ComponentLifecycle::Stopped
        } else {
            ComponentLifecycle::Failed
        };
        observed.transition_time = Utc::now();
    });
    result
}

pub(super) async fn execute(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    supplied: ReconciliationPreview,
    bindings: TopologyBindings,
) -> GraphResult<ReconciliationReport> {
    check_revision(graph, supplied.revision)?;
    if supplied.desired.graph_id != graph.snapshot.id.as_ref() {
        return Err(topology("preview belongs to another graph"));
    }
    if operations.starting.is_some()
        || operations.stopping.is_some()
        || !operations.propagated_stop.is_empty()
    {
        return Err(GraphError::OperationInProgress);
    }
    for (id, (generation, epoch)) in &supplied.epochs {
        let observed = graph.observed();
        let current = observed
            .components
            .get(id)
            .ok_or(GraphError::StaleGeneration)?;
        if &current.generation != generation || &current.operation != epoch {
            return Err(GraphError::StaleGeneration);
        }
    }
    let plan = preview(graph, supplied.changes)?;
    let mut prepared = prepare(graph, &plan, bindings)?;
    let activate_blocked: BTreeSet<_> = plan
        .pause
        .iter()
        .filter(|id| graph.observed().components[*id].realization != RealizationState::Created)
        .cloned()
        .collect();
    let mut report = ReconciliationReport {
        revision: plan.revision,
        summary: OperationSummary::Completed,
        committed: false,
        creation: BTreeMap::new(),
        startup: None,
        removed: BTreeSet::new(),
        failures: Vec::new(),
    };
    let changing_edges: BTreeSet<_> = plan.rebind.union(&plan.unbind).cloned().collect();
    let boundary = async {
        for index in graph.order.clone() {
            // Drain incoming work while this consumer is still active, then
            // park it before advancing to its downstream consumers.
            for edge in changing_edges
                .iter()
                .filter(|edge| &edge.to.component == graph.nodes[index].descriptor.id())
            {
                let Some((&edge_index, _)) =
                    graph.edges.iter().find(|(_, old)| &old.definition == edge)
                else {
                    continue;
                };
                if operations.invalidated_bindings.contains(&edge_index) {
                    continue;
                }
                let from = graph.ids[&edge.from.component];
                let progress = graph.components[from]
                    .take()?
                    .outputs
                    .iter()
                    .find(|output| output.edge == edge_index)
                    .map(|output| output.progress.clone());
                if let Some(progress) = progress {
                    let control = controls
                        .0
                        .get(&edge_index)
                        .cloned()
                        .ok_or_else(|| topology("bound edge has no drain control"))?;
                    drive(graph, operations, controls, cancel, async {
                        progress.drained(control.as_ref()).await
                    })
                    .await?;
                }
            }
            if plan.pause.contains(graph.nodes[index].descriptor.id()) {
                pause(graph, operations, controls, cancel, index).await?;
            }
        }
        Ok::<_, GraphError>(())
    }
    .await;
    if let Err(error) = boundary {
        if matches!(error, GraphError::Cancelled) {
            return Err(error);
        }
        report.failures.push(failure(error, FailurePhase::Removal));
        report.summary = OperationSummary::CompletedWithFailures;
        resume(graph, operations, &plan.pause)?;
        return Ok(report);
    }
    let stop: BTreeSet<_> = plan
        .replace
        .iter()
        .chain(&plan.remove)
        .chain(&plan.restart)
        .chain(&activate_blocked)
        .cloned()
        .collect();
    for index in graph.order.clone() {
        let id = graph.nodes[index].descriptor.id().clone();
        if stop.contains(&id) {
            if let Err(error) = stop_instance(graph, operations, controls, cancel, index).await {
                if matches!(error, GraphError::Cancelled) {
                    return Err(error);
                }
                let failure = failure(error, FailurePhase::Stop);
                update(graph, |state| {
                    state.components.get_mut(&id).expect("component").failure =
                        Some(failure.clone())
                });
                report.failures.push(failure);
            }
        }
    }
    let mut cleanup_touched = BTreeSet::new();
    if report.failures.is_empty() {
        for id in plan.resources.iter().chain(&plan.remove_resources) {
            let Some(spec) = graph.snapshot.resources.get(id) else {
                continue;
            };
            let Some(handle) = graph.resource_handles.get(id).cloned() else {
                continue;
            };
            if spec.ownership == ResourceOwnership::Graph {
                cleanup_touched.insert(id.clone());
                update(graph, |state| {
                    state.resources.get_mut(id).expect("resource").realization =
                        ResourceRealization::CleanupRequired
                });
                let result = drive(graph, operations, controls, cancel, async {
                    handle
                        .shutdown()
                        .await
                        .map_err(|source| GraphError::ResourceCleanup {
                            resource: id.clone(),
                            source,
                        })
                })
                .await;
                if let Err(error) = result {
                    if matches!(error, GraphError::Cancelled) {
                        return Err(error);
                    }
                    let failure = failure(error, FailurePhase::Removal);
                    update(graph, |state| {
                        state.resources.get_mut(id).expect("resource").failure =
                            Some(failure.clone())
                    });
                    report.failures.push(failure);
                } else {
                    graph.resource_handles.remove(id);
                    update(graph, |state| {
                        state.resources.get_mut(id).expect("resource").realization =
                            ResourceRealization::Released
                    });
                }
            }
        }
    }
    if !report.failures.is_empty() {
        for (&index, edge) in &graph.edges {
            if edge.resources.keys().any(|id| cleanup_touched.contains(id)) {
                if let Some(control) = controls.0.get(&index) {
                    control.cancel();
                }
                operations.invalidated_bindings.insert(index);
                update(graph, |state| {
                    let observed = state
                        .relationships
                        .get_mut(&edge.definition)
                        .expect("relationship");
                    observed.binding = BindingState::Failed;
                    observed.availability = DataAvailability::Unavailable;
                    observed.failure = Some(report.failures[0].clone());
                    observed.transition_time = Utc::now();
                });
            }
        }
        report.summary = OperationSummary::CompletedWithFailures;
        resume(
            graph,
            operations,
            &plan.pause.difference(&stop).cloned().collect(),
        )?;
        return Ok(report);
    }
    commit_desired(graph, operations, controls, &plan, &mut prepared)?;
    report.revision = graph.snapshot.revision;
    report.committed = true;
    report.removed = plan.remove.clone();
    realize(graph, operations, controls, cancel, &plan, &mut report).await?;
    resume(graph, operations, &plan.pause)?;
    let start: BTreeSet<_> = plan
        .create
        .iter()
        .chain(&plan.replace)
        .chain(&plan.restart)
        .chain(&activate_blocked)
        .filter_map(|id| graph.ids.get(id).copied())
        .collect();
    if !start.is_empty() {
        let (reply, receiver) = oneshot::channel();
        operations.begin_start(graph, start, Some(reply));
        report.startup = Some(
            drive(graph, operations, controls, cancel, async {
                receiver.await.map_err(|_| GraphError::ControllerClosed)?
            })
            .await?,
        );
    }
    if !report.failures.is_empty()
        || report
            .creation
            .values()
            .any(|outcome| !matches!(outcome, CreationOutcome::Created))
        || report
            .startup
            .as_ref()
            .is_some_and(|report| report.summary != OperationSummary::Completed)
    {
        report.summary = OperationSummary::CompletedWithFailures;
    }
    Ok(report)
}

fn commit_desired(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    plan: &ReconciliationPreview,
    prepared: &mut Prepared,
) -> GraphResult<()> {
    let old_edges: Vec<_> = graph
        .edges
        .iter()
        .filter(|(_, edge)| {
            plan.rebind.contains(&edge.definition) || plan.unbind.contains(&edge.definition)
        })
        .map(|(index, _)| *index)
        .collect();
    for index in old_edges {
        operations.invalidated_bindings.remove(&index);
        let edge = graph.edges.remove(&index).expect("edge");
        if let Some(control) = controls.0.remove(&index) {
            control.cancel();
        }
        graph.providers.remove(&index);
        let from = graph.ids[&edge.definition.from.component];
        let to = graph.ids[&edge.definition.to.component];
        graph.components[from]
            .take()?
            .outputs
            .retain(|output| output.edge != index);
        graph.components[to]
            .take()?
            .inputs
            .retain(|input| input.edge != index);
    }
    for id in &plan.remove {
        let index = graph.ids.remove(id).expect("removed component");
        let mut lease = graph.components[index].take()?;
        lease.value.take();
        operations.paused.remove(&index);
        operations.exhausted.remove(&index);
    }
    for node in &prepared.nodes {
        let id = node.descriptor.id().clone();
        if let Some(component) = prepared.components.remove(&id) {
            let generation = ComponentGeneration(graph.next_generation);
            graph.next_generation = graph
                .next_generation
                .checked_add(1)
                .ok_or_else(|| topology("construction generation exhausted"))?;
            let slot = InstanceSlot::new(component, generation);
            if let Some(&index) = graph.ids.get(&id) {
                let mut old = graph.components[index].take()?;
                let mut new = slot.take()?;
                new.inputs = std::mem::take(&mut old.inputs);
                new.outputs = std::mem::take(&mut old.outputs);
                new.sequences = std::mem::take(&mut old.sequences)
                    .into_iter()
                    .filter(|(port, _)| {
                        node.output_streams.contains_key(port)
                            && graph.nodes[index].output_streams.get(port)
                                == node.output_streams.get(port)
                    })
                    .collect();
                old.value.take();
                drop(new);
                graph.components[index] = slot;
                operations.exhausted.remove(&index);
                operations.paused.remove(&index);
                graph.nodes[index] = node.clone();
            } else {
                let index = graph.components.len();
                graph.ids.insert(id.clone(), index);
                graph.components.push(slot);
                graph.nodes.push(node.clone());
            }
        } else if let Some(&index) = graph.ids.get(&id) {
            graph.nodes[index] = node.clone();
        }
    }
    for edge in &prepared.edges {
        if let Some(provider) = prepared.pipes.remove(&edge.definition) {
            graph.edges.insert(graph.next_edge, edge.clone());
            graph.providers.insert(graph.next_edge, provider);
            graph.next_edge = graph
                .next_edge
                .checked_add(1)
                .ok_or_else(|| topology("relationship generation exhausted"))?;
        } else if let Some((_, existing)) = graph
            .edges
            .iter_mut()
            .find(|(_, old)| old.definition == edge.definition)
        {
            *existing = edge.clone();
        }
    }
    graph.order = prepared
        .order
        .iter()
        .map(|index| graph.ids[prepared.nodes[*index].descriptor.id()])
        .collect();
    graph.resource_handles = std::mem::take(&mut prepared.resources);
    graph.factories = std::mem::take(&mut prepared.factories);
    graph.snapshot.revision = plan.desired.revision;
    graph.snapshot.nodes = prepared.nodes.clone().into();
    graph.snapshot.edges = prepared.edges.clone().into();
    graph.snapshot.unbound_relationships = plan.desired.boundary_relationships.clone().into();
    graph.snapshot.resources = plan
        .desired
        .resources
        .iter()
        .map(|spec| (spec.id.clone(), spec.clone()))
        .collect();
    graph.snapshot.lifecycle_policies = plan
        .desired
        .components
        .iter()
        .map(|node| (node.descriptor.id().clone(), node.lifecycle.clone()))
        .collect();
    graph.snapshot.specifications.clear();
    graph.snapshot.external_bindings.clear();
    for node in &plan.desired.components {
        match &node.construction {
            ComponentConstruction::Factory(spec) => {
                graph
                    .snapshot
                    .specifications
                    .insert(node.descriptor.id().clone(), spec.clone());
            }
            ComponentConstruction::External { binding } => {
                graph
                    .snapshot
                    .external_bindings
                    .insert(node.descriptor.id().clone(), Arc::from(binding.as_str()));
            }
        }
    }
    let binding_generations: BTreeMap<_, _> = plan
        .rebind
        .iter()
        .map(|edge| {
            let generation = graph.next_binding_generation;
            graph.next_binding_generation += 1;
            (edge.clone(), generation)
        })
        .collect();
    let resource_generations: BTreeMap<_, _> = plan
        .resources
        .iter()
        .map(|id| {
            let generation = graph.next_resource_generation;
            graph.next_resource_generation += 1;
            (id.clone(), generation)
        })
        .collect();
    update(graph, |state| {
        state.revision = plan.desired.revision;
        for id in &plan.remove {
            state.components.remove(id);
        }
        for id in plan.create.iter().chain(&plan.replace) {
            state.components.insert(
                id.clone(),
                ObservedComponent {
                    generation: graph.components[graph.ids[id]].generation,
                    operation: OperationEpoch(0),
                    revision: state.revision,
                    realization: RealizationState::Pending,
                    lifecycle: ComponentLifecycle::Stopped,
                    health: ComponentHealth::Unknown,
                    failure: None,
                    exhausted: false,
                    transition_time: Utc::now(),
                },
            );
        }
        for component in state.components.values_mut() {
            component.revision = state.revision;
        }
        for id in plan.restart.iter().chain(&plan.reactivate) {
            if let Some(component) = state.components.get_mut(id) {
                component.exhausted = false;
                if plan.reactivate.contains(id)
                    && !plan.restart.contains(id)
                    && component.lifecycle == ComponentLifecycle::Running
                {
                    component.lifecycle = ComponentLifecycle::Quiesced;
                }
            }
        }
        state.relationships.retain(|edge, _| {
            plan.desired
                .relationships
                .iter()
                .chain(&plan.desired.boundary_relationships)
                .any(|relationship| &relationship.definition == edge)
        });
        for edge in plan.rebind.iter().chain(
            plan.desired
                .boundary_relationships
                .iter()
                .map(|edge| &edge.definition),
        ) {
            let generation = binding_generations.get(edge).copied().unwrap_or(0);
            state.relationships.insert(
                edge.clone(),
                ObservedRelationship {
                    binding: BindingState::Declared,
                    availability: DataAvailability::Unavailable,
                    generation,
                    revision: state.revision,
                    failure: None,
                    transition_time: Utc::now(),
                },
            );
        }
        state
            .resources
            .retain(|id, _| !plan.remove_resources.contains(id));
        for id in &plan.resources {
            let generation = resource_generations[id];
            state.resources.insert(
                id.clone(),
                ObservedResource {
                    realization: if graph.resource_handles.contains_key(id) {
                        ResourceRealization::Created
                    } else {
                        ResourceRealization::Pending
                    },
                    generation,
                    revision: state.revision,
                    transition_time: Utc::now(),
                    failure: None,
                },
            );
        }
    });
    for id in plan.restart.iter().chain(&plan.reactivate) {
        if let Some(&index) = graph.ids.get(id) {
            operations.exhausted.remove(&index);
            if plan.reactivate.contains(id) && !plan.restart.contains(id) {
                operations.paused.insert(index);
            }
        }
    }
    graph.desired.send_replace(Arc::new(graph.snapshot.clone()));
    Ok(())
}

async fn realize(
    graph: &ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    plan: &ReconciliationPreview,
    report: &mut ReconciliationReport,
) -> GraphResult<()> {
    for &index in &graph.order {
        let id = graph.nodes[index].descriptor.id();
        if !plan.create.contains(id) && !plan.replace.contains(id) && !plan.update.contains(id) {
            continue;
        }
        let slot = graph.components[index].clone();
        let node = graph.nodes[index].clone();
        let mut lease = slot.take()?;
        let update_in_place = plan.update.contains(id);
        let definition = if update_in_place {
            let spec = Arc::new(graph.snapshot.specifications[id].clone());
            let factory = graph.factories[&spec.implementation].clone();
            Some((spec, factory))
        } else if let Component::Deferred {
            specification,
            factory,
        } = &lease.component
        {
            Some((specification.clone(), factory.clone()))
        } else {
            None
        };
        let mut created = Ok(());
        if let Some((spec, factory)) = definition {
            let missing: Vec<_> = spec
                .dependencies
                .values()
                .flatten()
                .chain(spec.configuration.values().filter_map(|value| {
                    if let ConfigurationValue::Reference { resource, .. } = value {
                        Some(resource)
                    } else {
                        None
                    }
                }))
                .filter(|id| !graph.resource_handles.contains_key(*id))
                .cloned()
                .collect();
            let dependencies: Vec<_> = graph
                .snapshot
                .edges
                .iter()
                .filter(|edge| {
                    edge.policy.required_for_creation
                        && &edge.definition.to.component == id
                        && graph.observed().components[&edge.definition.from.component].realization
                            != RealizationState::Created
                })
                .map(|edge| edge.definition.from.component.clone())
                .collect();
            if !missing.is_empty() || !dependencies.is_empty() {
                update(graph, |state| {
                    state.components.get_mut(id).expect("component").realization =
                        RealizationState::Blocked
                });
                report.creation.insert(
                    id.clone(),
                    CreationOutcome::Blocked {
                        dependencies,
                        resources: missing,
                    },
                );
                continue;
            }
            update(graph, |state| {
                state.components.get_mut(id).expect("component").realization =
                    RealizationState::Creating
            });
            let resources = graph.resource_handles.clone();
            let graph_id = graph.snapshot.id.clone();
            created = drive(graph, operations, controls, cancel, async {
                let context = ConstructionContext::resolve(
                    graph_id,
                    slot.generation,
                    spec,
                    resources,
                    &factory.descriptor().configuration,
                )
                .await
                .map_err(|error| GraphError::Creation {
                    component: id.clone(),
                    disposition: error.disposition,
                    source: error.source,
                })?;
                if update_in_place {
                    lease
                        .component
                        .reconfigure(context)
                        .await
                        .map_err(|source| component_error(&node, "reconfigure", source))?;
                } else {
                    let ConstructedComponent(component) =
                        factory
                            .create(context)
                            .await
                            .map_err(|error| GraphError::Creation {
                                component: id.clone(),
                                disposition: error.disposition,
                                source: error.source,
                            })?;
                    lease.component = component;
                }
                if lease.component.role() != node.role {
                    lease.attempted = true;
                    return Err(topology("constructed role differs from desired role"));
                }
                check_descriptor(&lease.component, &node)
            })
            .await;
        }
        match created {
            Ok(()) => {
                update(graph, |state| {
                    let observed = state.components.get_mut(id).expect("component");
                    observed.realization = RealizationState::Created;
                    observed.failure = None;
                    observed.transition_time = Utc::now();
                });
                report.creation.insert(id.clone(), CreationOutcome::Created);
            }
            Err(error) => {
                if matches!(error, GraphError::Cancelled) {
                    return Err(error);
                }
                let failure = failure(error, FailurePhase::Creation);
                update(graph, |state| {
                    let observed = state.components.get_mut(id).expect("component");
                    observed.realization = RealizationState::CreationFailed;
                    observed.failure = Some(failure.clone());
                    if update_in_place {
                        observed.lifecycle = ComponentLifecycle::Failed;
                    }
                    observed.transition_time = Utc::now();
                });
                report
                    .creation
                    .insert(id.clone(), CreationOutcome::CreationFailed(failure));
            }
        }
    }
    for (&index, edge) in &graph.edges {
        if !plan.rebind.contains(&edge.definition) {
            continue;
        }
        let from = graph.ids[&edge.definition.from.component];
        let to = graph.ids[&edge.definition.to.component];
        if [from, to].iter().any(|index| {
            graph.observed().components[graph.nodes[*index].descriptor.id()]
                .failure
                .as_ref()
                .is_some_and(|failure| failure.phase == FailurePhase::Creation)
        }) {
            continue;
        }
        if matches!(
            graph.components[from].take()?.component,
            Component::Deferred { .. }
        ) || matches!(
            graph.components[to].take()?.component,
            Component::Deferred { .. }
        ) {
            continue;
        }
        update(graph, |state| {
            state
                .relationships
                .get_mut(&edge.definition)
                .expect("relationship")
                .binding = BindingState::Binding
        });
        let result = (|| {
            let mut pipe = graph.providers[&index]
                .create_with_resources(&graph.resource_handles)
                .map_err(|source| GraphError::Pipe {
                    edge: index,
                    source,
                })?;
            controls.0.insert(index, pipe.control.clone());
            if pipe.pipe.capabilities() != &edge.capabilities {
                return Err(topology("created pipe differs from preflight capabilities"));
            }
            let receiver = pipe
                .pipe
                .take_receiver()
                .map_err(|source| GraphError::Pipe {
                    edge: index,
                    source,
                })?;
            let progress = Arc::new(FlowProgress::default());
            graph.components[from].take()?.outputs.push(Outgoing {
                edge: index,
                port: edge.definition.from.port.clone(),
                sender: pipe.pipe.sender(),
                progress: progress.clone(),
            });
            graph.components[to].take()?.inputs.push(Incoming {
                edge: index,
                port: edge.definition.to.port.clone(),
                receiver,
                acknowledgement_required: edge
                    .capabilities
                    .supported()
                    .contains(&PipeCapability::ExplicitAcknowledgement),
                pending: None,
                exhausted: false,
                progress,
            });
            Ok::<_, GraphError>(())
        })();
        match result {
            Ok(()) => update(graph, |state| {
                let observed = state
                    .relationships
                    .get_mut(&edge.definition)
                    .expect("relationship");
                observed.binding = BindingState::Bound;
                observed.availability = DataAvailability::Idle;
                observed.failure = None;
            }),
            Err(error) => {
                if let Some(control) = controls.0.get(&index) {
                    control.cancel();
                }
                let failure = failure(error, FailurePhase::Binding);
                update(graph, |state| {
                    let observed = state
                        .relationships
                        .get_mut(&edge.definition)
                        .expect("relationship");
                    observed.binding = BindingState::Failed;
                    observed.availability = DataAvailability::Unavailable;
                    observed.failure = Some(failure.clone());
                });
                report.failures.push(failure);
            }
        }
    }
    for id in plan.pause.iter().chain(&plan.create).chain(&plan.replace) {
        let Some(&index) = graph.ids.get(id) else {
            continue;
        };
        let observed = &graph.observed().components[id];
        if observed
            .failure
            .as_ref()
            .is_some_and(|error| error.phase == FailurePhase::Creation)
        {
            continue;
        }
        if operations.active.contains_key(&index) {
            continue;
        }
        let lease = graph.components[index].take()?;
        if !matches!(lease.component, Component::Deferred { .. })
            && binding_dependencies(graph, id).is_empty()
        {
            update(graph, |state| {
                state.components.get_mut(id).expect("component").realization =
                    RealizationState::Created
            });
        }
    }
    Ok(())
}
