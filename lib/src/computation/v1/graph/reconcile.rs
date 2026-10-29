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
    recovery, resolve, validate_unbound_relationship, ComponentConstruction,
    ComponentSpecification, ConstructedComponent, DesiredComponent, DesiredPipe,
    DesiredRelationship, DesiredTopology, EdgeDefinition, EdgeSnapshot, Endpoint, FlowProgress,
    ImplementationIdentity, ResourceHandle, ResourceSpecification, TopologyBindings,
};
use super::*;
use crate::computation::v1::{
    validate_connection, validate_sink_completion, ComponentDescriptor, ComponentFactory,
    ComponentRole, ConfigurationValue, PipeCapabilities, PipeCapability, PipeProvider,
    PipeRequirements, PortDirection, RemovalPolicy, ResourceConstructor, ResourceId, ResourceRole,
};
use std::{future::Future, num::NonZeroUsize};

#[cfg(test)]
#[path = "reconcile_commit_tests.rs"]
mod commit_tests;

/// Desired mutations are descriptions, not constructed instances or resolved secrets.
/// Supply new external instances/providers separately when executing the preview.
#[derive(Debug, Clone)]
pub enum DesiredMutation {
    /// Apply a complete graph definition, preserving unchanged live instances.
    /// The graph ID must match. External bindings must be supplied explicitly.
    SetTopology(DesiredTopology),
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
    SetSubscriptions {
        consumer: ComponentId,
        producers: Vec<ComponentId>,
    },
    Unbind {
        edge: EdgeDefinition,
        policy: RemovalPolicy,
    },
    PutResource(ResourceSpecification),
    SetResourceDependencies {
        resource: ResourceId,
        dependencies: BTreeMap<ResourceId, ResourceRole>,
    },
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
    observation_resources: BTreeSet<ResourceId>,
    drain: BTreeSet<ComponentId>,
    epochs: BTreeMap<ComponentId, (ComponentGeneration, OperationEpoch)>,
    pub(super) deprovision: BTreeSet<ComponentId>,
}

impl ReconciliationPreview {
    fn same_impact(&self, other: &Self) -> bool {
        self.desired == other.desired
            && self.create == other.create
            && self.replace == other.replace
            && self.update == other.update
            && self.remove == other.remove
            && self.restart == other.restart
            && self.reactivate == other.reactivate
            && self.pause == other.pause
            && self.rebind == other.rebind
            && self.unbind == other.unbind
            && self.resources == other.resources
            && self.remove_resources == other.remove_resources
            && self.observation_resources == other.observation_resources
            && self.drain == other.drain
            && self.epochs == other.epochs
    }

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

/// Outcome of [`GraphControl::reconcile`].
///
/// Once a reconciliation commits, its report is always returned: post-commit
/// failures (creation, recovery validation, activation, deadlines) are recorded
/// in `failures` with `summary == CompletedWithFailures`, never surfaced as an
/// `Err`. Only controller cancellation aborts a committed reconciliation.
#[derive(Debug, Clone)]
#[must_use = "inspect committed, summary and failures before treating reconciliation as successful"]
pub struct ReconciliationReport {
    pub revision: GraphRevision,
    pub summary: OperationSummary,
    /// Whether `revision` is now the graph's desired topology.
    ///
    /// When false, cleanup failure left the previous desired topology intact.
    /// Successfully stopped components remain stopped; this is not a rollback
    /// claim.
    pub committed: bool,
    pub creation: BTreeMap<ComponentId, CreationOutcome>,
    /// Absent when nothing was started, or when startup did not finish within
    /// the deadline. If the graph had not settled, eligible components are
    /// queued and start automatically once in-flight lifecycle work completes.
    pub startup: Option<StartReport>,
    pub removed: BTreeSet<ComponentId>,
    pub failures: Vec<ComponentFailure>,
}

impl ReconciliationReport {
    /// Require desired-state acceptance, not successful construction or readiness.
    /// The error retains the complete uncommitted report and its failure causes.
    pub fn ensure_committed(
        &self,
    ) -> std::result::Result<(), Box<crate::computation::v1::LifecycleReportError>> {
        if self.committed {
            Ok(())
        } else {
            Err(Box::new(
                crate::computation::v1::LifecycleReportError::Reconciliation(self.clone()),
            ))
        }
    }
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
        self.require_configuration_write()?;
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Reconcile {
                preview,
                bindings,
                management_access: self.management_access,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }
}

pub(super) fn check_protected(
    graph: &ComputationGraph,
    preview: &ReconciliationPreview,
) -> GraphResult<()> {
    let old = graph.snapshot.select(GraphSelection::All)?;
    let target = &preview.desired;
    for id in &graph.protected_components {
        if old
            .components
            .iter()
            .find(|node| node.descriptor.id() == id)
            != target
                .components
                .iter()
                .find(|node| node.descriptor.id() == id)
            || preview.replace.contains(id)
            || preview.update.contains(id)
            || preview.restart.contains(id)
            || old.component_resources.get(id) != target.component_resources.get(id)
            || old.component_plugins.get(id) != target.component_plugins.get(id)
            || old.readiness_required.contains(id) != target.readiness_required.contains(id)
        {
            return Err(topology(format!("component {id} is managed by DrasiLib")));
        }
    }
    for id in &graph.protected_resources {
        if old.resources.iter().find(|resource| &resource.id == id)
            != target.resources.iter().find(|resource| &resource.id == id)
            || old.resource_configurations.get(id) != target.resource_configurations.get(id)
            || old.resource_dependencies.get(id) != target.resource_dependencies.get(id)
            || preview.resources.contains(id)
            || preview.remove_resources.contains(id)
        {
            return Err(topology(format!("resource {id} is managed by DrasiLib")));
        }
    }
    let protected_edges = |topology: &DesiredTopology| {
        topology
            .relationships
            .iter()
            .chain(&topology.boundary_relationships)
            .filter(|edge| {
                graph
                    .protected_components
                    .contains(&edge.definition.from.component)
                    || graph
                        .protected_components
                        .contains(&edge.definition.to.component)
            })
            .map(|edge| (edge.definition.clone(), edge.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    let protected_links = |links: &[(ComponentId, ComponentId)]| {
        links
            .iter()
            .filter(|(from, to)| {
                graph.protected_components.contains(from) || graph.protected_components.contains(to)
            })
            .cloned()
            .collect::<BTreeSet<_>>()
    };
    let protected_recovery = |desired: &DesiredTopology| {
        desired
            .recovery_requirements
            .iter()
            .filter(|requirement| {
                !recovery_participants(desired, &requirement.consumer)
                    .is_disjoint(&graph.protected_components)
            })
            .cloned()
            .collect::<Vec<_>>()
    };
    if protected_edges(&old) != protected_edges(target)
        || protected_links(&old.subscriptions) != protected_links(&target.subscriptions)
        || protected_links(&old.control_connections) != protected_links(&target.control_connections)
        || protected_recovery(&old) != protected_recovery(target)
        || (!graph.protected_components.is_empty() && old.requirements != target.requirements)
    {
        return Err(topology("connection configuration is managed by DrasiLib"));
    }
    Ok(())
}

fn recovery_participants(
    desired: &DesiredTopology,
    consumer: &ComponentId,
) -> BTreeSet<ComponentId> {
    recovery::participants(
        consumer,
        desired
            .relationships
            .iter()
            .chain(&desired.boundary_relationships)
            .map(|edge| {
                (
                    &edge.definition.from.component,
                    &edge.definition.to.component,
                )
            })
            .chain(desired.subscriptions.iter().map(|(from, to)| (from, to))),
    )
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
        for (from, to) in &desired.subscriptions {
            if selected.contains(from) {
                selected.insert(to.clone());
            }
        }
        if before == selected.len() {
            break;
        }
    }
}

fn resource_users(desired: &DesiredTopology, resource: &ResourceId) -> BTreeSet<ComponentId> {
    desired.component_resource_users(resource)
}

fn remove_components(
    desired: &mut DesiredTopology,
    mut selected: BTreeSet<ComponentId>,
    policy: RemovalPolicy,
    observation_resources: &BTreeSet<ResourceId>,
) -> GraphResult<BTreeSet<ComponentId>> {
    if matches!(policy, RemovalPolicy::Cascade | RemovalPolicy::Drain) {
        dependent_closure(desired, &mut selected);
    }
    for (from, to) in &desired.subscriptions {
        if selected.contains(from) && !selected.contains(to) && policy == RemovalPolicy::Reject {
            return Err(topology(format!("component {from} is depended on by {to}")));
        }
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
    let detached: BTreeSet<_> = desired
        .component_resources
        .iter()
        .filter(|(id, _)| selected.contains(*id))
        .flat_map(|(_, resources)| resources.iter().cloned())
        .collect();
    desired
        .component_resources
        .retain(|id, _| !selected.contains(id));
    desired
        .control_connections
        .retain(|(from, to)| !selected.contains(from) && !selected.contains(to));
    desired.subscriptions.retain(|(from, to)| {
        !selected.contains(to) && (policy == RemovalPolicy::Orphan || !selected.contains(from))
    });
    desired
        .readiness_required
        .retain(|id| !selected.contains(id));
    desired
        .component_plugins
        .retain(|id, _| !selected.contains(id));
    let unused: BTreeSet<_> = detached
        .into_iter()
        .filter(|id| {
            observation_resources.contains(id)
                && resource_users(desired, id).is_empty()
                && !desired
                    .resource_dependencies
                    .values()
                    .any(|dependencies| dependencies.contains_key(id))
        })
        .collect();
    desired
        .resources
        .retain(|resource| !unused.contains(&resource.id));
    desired
        .resource_configurations
        .retain(|id, _| !unused.contains(id));
    desired
        .resource_dependencies
        .retain(|id, _| !unused.contains(id));
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
    let mut observation_resources = graph.observation_resources.clone();
    let mut drain = BTreeSet::new();
    let mut explicit_binds = BTreeSet::new();
    for change in &changes {
        match change {
            DesiredMutation::SetTopology(target) => {
                if target.version != 1 || target.graph_id != desired.graph_id {
                    return Err(topology("desired topology identity or version mismatch"));
                }
                if old.components.iter().any(|node| {
                    graph
                        .reserved_components
                        .contains(node.descriptor.id().as_str())
                        && !target.components.iter().any(|new| new == node)
                }) {
                    return Err(topology(
                        "full desired topology cannot change host-reserved components",
                    ));
                }
                for resource in &target.resources {
                    if old.resources.iter().find(|prior| prior.id == resource.id) != Some(resource)
                        || old.resource_configurations.get(&resource.id)
                            != target.resource_configurations.get(&resource.id)
                        || old.resource_dependencies.get(&resource.id)
                            != target.resource_dependencies.get(&resource.id)
                        || !graph.resource_handles.contains_key(&resource.id)
                        || graph.resource_retired(&resource.id)
                        || graph
                            .observed()
                            .resources
                            .get(&resource.id)
                            .is_some_and(|state| {
                                state.realization != ResourceRealization::Created
                                    || state.failure.is_some()
                            })
                    {
                        resources.insert(resource.id.clone());
                        observation_resources.remove(&resource.id);
                    }
                }
                desired = target.clone();
            }
            DesiredMutation::SetSubscriptions {
                consumer,
                producers,
            } => {
                if !desired
                    .components
                    .iter()
                    .any(|node| node.descriptor.id() == consumer)
                    || producers.iter().any(|producer| producer == consumer)
                {
                    return Err(topology("subscription declaration requires an existing consumer and no self-reference"));
                }
                desired.subscriptions.retain(|(_, to)| to != consumer);
                desired.subscriptions.extend(
                    producers
                        .iter()
                        .cloned()
                        .map(|from| (from, consumer.clone())),
                );
                desired.subscriptions.sort();
                desired.subscriptions.dedup();
            }
            DesiredMutation::PutComponent(node)
            | DesiredMutation::ReplaceComponent(node)
            | DesiredMutation::UpdateComponent(node) => {
                let id = node.descriptor.id();
                if graph.reserved_components.contains(id.as_str()) {
                    return Err(topology(format!(
                        "component ID {id} is reserved by the host"
                    )));
                }
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
                let removed = remove_components(
                    &mut desired,
                    selected_ids(graph, selection)?,
                    *policy,
                    &observation_resources,
                )?;
                if *policy == RemovalPolicy::Drain {
                    drain.extend(removed);
                }
            }
            DesiredMutation::Bind(edge) => {
                explicit_binds.insert(edge.definition.clone());
                desired
                    .boundary_relationships
                    .retain(|prior| prior.definition != edge.definition);
                let endpoints_exist =
                    [&edge.definition.from.component, &edge.definition.to.component]
                        .iter()
                        .all(|id| {
                            desired
                                .components
                                .iter()
                                .any(|node| node.descriptor.id() == *id)
                        });
                if !endpoints_exist {
                    if matches!(&edge.pipe, DesiredPipe::External { .. }) {
                        return Err(topology("opaque pipe bindings require declared endpoints"));
                    }
                    desired
                        .relationships
                        .retain(|prior| prior.definition != edge.definition);
                    desired.boundary_relationships.push(edge.clone());
                    continue;
                }
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
                let removed = if let Some(index) = desired
                    .relationships
                    .iter()
                    .position(|prior| &prior.definition == edge)
                {
                    desired.relationships.remove(index)
                } else if let Some(index) = desired
                    .boundary_relationships
                    .iter()
                    .position(|prior| &prior.definition == edge)
                {
                    desired.boundary_relationships.remove(index)
                } else {
                    return Err(topology("cannot unbind an undeclared relationship"));
                };
                let consumer_declared = desired
                    .components
                    .iter()
                    .any(|node| node.descriptor.id() == &edge.to.component);
                if *policy == RemovalPolicy::Orphan {
                    desired.boundary_relationships.push(removed);
                } else if *policy == RemovalPolicy::Cascade && consumer_declared {
                    remove_components(
                        &mut desired,
                        BTreeSet::from([edge.to.component.clone()]),
                        *policy,
                        &observation_resources,
                    )?;
                } else if *policy == RemovalPolicy::Drain && consumer_declared {
                    drain.insert(edge.to.component.clone());
                }
            }
            DesiredMutation::PutResource(spec) => {
                super::super::validate_identifier("resource binding", &spec.binding)?;
                observation_resources.remove(&spec.id);
                if let Some(prior) = desired
                    .resources
                    .iter_mut()
                    .find(|prior| prior.id == spec.id)
                {
                    if prior != spec {
                        resources.insert(spec.id.clone());
                        desired.resource_configurations.remove(&spec.id);
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
                observation_resources.remove(id);
                desired.resource_configurations.remove(id);
            }
            DesiredMutation::SetResourceDependencies {
                resource,
                dependencies,
            } => {
                if !desired
                    .resources
                    .iter()
                    .any(|specification| &specification.id == resource)
                {
                    return Err(topology(
                        "resource dependencies require a declared resource",
                    ));
                }
                let previous = desired
                    .resource_dependencies
                    .get(resource)
                    .cloned()
                    .unwrap_or_default();
                if previous != *dependencies {
                    resources.insert(resource.clone());
                    if dependencies.is_empty() {
                        desired.resource_dependencies.remove(resource);
                    } else {
                        desired
                            .resource_dependencies
                            .insert(resource.clone(), dependencies.clone());
                    }
                }
            }
            DesiredMutation::RemoveResource { resource, policy } => {
                if !desired.resources.iter().any(|spec| &spec.id == resource) {
                    return Err(topology("unknown resource"));
                }
                let mut affected = BTreeSet::from([resource.clone()]);
                desired.include_resource_dependents(&mut affected);
                if affected.len() > 1
                    && !matches!(policy, RemovalPolicy::Cascade | RemovalPolicy::Drain)
                {
                    return Err(topology("resource is still required by another resource"));
                }
                let users = resource_users(&desired, resource);
                let mut users = users;
                for edge in desired
                    .relationships
                    .iter()
                    .chain(&desired.boundary_relationships)
                    .filter(|edge| {
                        edge.pipe
                            .resource_dependencies()
                            .keys()
                            .any(|id| affected.contains(id))
                    })
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
                        let removed = remove_components(
                            &mut desired,
                            users,
                            *policy,
                            &observation_resources,
                        )?;
                        if *policy == RemovalPolicy::Drain {
                            drain.extend(removed);
                        }
                    } else {
                        return Err(topology("resource is still referenced; remove optional references explicitly before orphaning"));
                    }
                }
                desired
                    .resources
                    .retain(|spec| !affected.contains(&spec.id));
                desired
                    .resource_configurations
                    .retain(|id, _| !affected.contains(id));
                desired
                    .resource_dependencies
                    .retain(|id, _| !affected.contains(id));
                remove_resources.extend(affected);
            }
            DesiredMutation::Restart(selection) => restart.extend(selected_ids(graph, selection)?),
            DesiredMutation::Retry(selection) => {
                for id in selected_ids(graph, selection)? {
                    let observed = &graph.observed().components[&id];
                    let Some(error) = observed.failure.as_ref() else {
                        // An explicit stop acknowledged a retryable creation
                        // failure; the component still needs constructing.
                        if observed.realization == RealizationState::CreationFailed {
                            forced.insert(id);
                            continue;
                        }
                        return Err(topology(format!(
                            "component {id} has no visible failure to retry"
                        )));
                    };
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
    desired.resolve_required_relationships();
    if old.recovery_requirements.iter().any(|requirement| {
        !desired.recovery_requirements.iter().any(|replacement| {
            replacement.consumer == requirement.consumer
                && replacement.scope == requirement.scope
                && requirement.guarantees.is_subset(&replacement.guarantees)
        })
    }) {
        return Err(topology(
            "live reconciliation cannot remove or weaken a recovery assertion",
        ));
    }
    let old_nodes: BTreeMap<_, _> = old
        .components
        .iter()
        .map(|node| (node.descriptor.id().clone(), node))
        .collect();
    remove_resources.extend(
        old.resources
            .iter()
            .filter(|resource| !desired.resources.iter().any(|new| new.id == resource.id))
            .map(|resource| resource.id.clone()),
    );
    let mut affected_resources: BTreeSet<_> = resources.union(&remove_resources).cloned().collect();
    old.include_resource_dependents(&mut affected_resources);
    desired.include_resource_dependents(&mut affected_resources);
    resources.extend(
        affected_resources
            .into_iter()
            .filter(|id| desired.resources.iter().any(|spec| &spec.id == id)),
    );
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
    for (id, node) in &new_nodes {
        let binds_inputs = match &node.construction {
            ComponentConstruction::Factory(specification) => graph
                .factories
                .get(&specification.implementation)
                .is_some_and(|factory| factory.binds_inputs()),
            _ => false,
        };
        if !binds_inputs || !old_nodes.contains_key(id) || replace.contains(id) {
            continue;
        }
        let bindings = |relationships: &[DesiredRelationship],
                        nodes: &BTreeMap<ComponentId, &DesiredComponent>| {
            super::super::specification::collect_input_bindings(
                id,
                relationships.iter().map(|edge| &edge.definition),
                |endpoint| {
                    nodes
                        .get(&endpoint.component)
                        .and_then(|node| node.streams.get(&endpoint.port))
                        .cloned()
                },
            )
        };
        if bindings(&old.relationships, &old_nodes) != bindings(&desired.relationships, &new_nodes)
        {
            if updating.contains(id) {
                return Err(topology(
                    "in-place update cannot change the inputs of a component that binds them",
                ));
            }
            replace.insert(id.clone());
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
            || (description.policy.rebind_on_consumer_replace
                && replace.contains(&edge.to.component))
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
    let consumer_bindings = graph
        .ids
        .iter()
        .filter_map(|(id, index)| {
            graph.components[*index]
                .consumer_progress()
                .transpose()
                .map(|progress| progress.map(|progress| (id.clone(), progress)))
        })
        .collect::<GraphResult<Vec<_>>>()?;
    let mut feedback_restarts = BTreeSet::new();
    loop {
        let before = (rebind.len(), restart.len(), reactivate.len(), pause.len());
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
            if reactivate.contains(&edge.from.component)
                || [&edge.from.component, &edge.to.component].iter().any(|id| {
                    feedback_restarts.contains(*id)
                        && graph
                            .observed()
                            .components
                            .get(*id)
                            .is_some_and(|state| state.exhausted)
                })
            {
                rebind.insert(edge.clone());
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
        for (producer, progress) in &consumer_bindings {
            let participants =
                super::super::consumer_progress::participants(&graph.snapshot, progress);
            let changed_path = rebind.iter().chain(&unbind).any(|edge| {
                participants.contains(&edge.from.component)
                    && !progress.owners().contains_key(&edge.from.component)
            });
            let changed_subscription = old.subscriptions != desired.subscriptions
                && old
                    .subscriptions
                    .iter()
                    .chain(&desired.subscriptions)
                    .any(|(from, to)| {
                        participants.contains(from)
                            && !progress.owners().contains_key(from)
                            && (old.subscriptions.contains(&(from.clone(), to.clone()))
                                != desired.subscriptions.contains(&(from.clone(), to.clone())))
                    });
            if changed_path
                || changed_subscription
                || participants.iter().any(|id| {
                    replace.contains(id)
                        || updating.contains(id)
                        || remove.contains(id)
                        || restart.contains(id)
                })
            {
                // Parking processing does not stop an owned I/O worker from
                // acknowledging upstream. Await stop before changing this cut.
                feedback_restarts.insert(producer.clone());
                restart.insert(producer.clone());
                pause.extend(participants);
            }
        }
        if before == (rebind.len(), restart.len(), reactivate.len(), pause.len()) {
            break;
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
    loop {
        let before = pause.len();
        for requirement in &desired.recovery_requirements {
            let participants = recovery_participants(&desired, &requirement.consumer);
            if !old.recovery_requirements.contains(requirement)
                || !participants.is_disjoint(&pause)
                || !participants.is_disjoint(&create)
            {
                pause.extend(participants);
            }
        }
        if pause.len() == before {
            break;
        }
    }
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
    let mut retired_reports = BTreeSet::new();
    for id in &replace {
        if let Some(reported) = graph.reported_resources.get(id) {
            retired_reports.extend(reported.iter().cloned());
            if let Some(resources) = desired.component_resources.get_mut(id) {
                resources.retain(|resource| !reported.contains(resource));
            }
        }
    }
    let retired_reports: BTreeSet<_> = retired_reports
        .into_iter()
        .filter(|id| {
            observation_resources.contains(id)
                && resource_users(&desired, id).is_empty()
                && !desired
                    .resource_dependencies
                    .values()
                    .any(|dependencies| dependencies.contains_key(id))
        })
        .collect();
    desired
        .resources
        .retain(|resource| !retired_reports.contains(&resource.id));
    desired
        .resource_configurations
        .retain(|id, _| !retired_reports.contains(id));
    desired
        .resource_dependencies
        .retain(|id, _| !retired_reports.contains(id));
    remove_resources.extend(retired_reports);
    let retained_resources: BTreeSet<_> = desired
        .resources
        .iter()
        .map(|resource| &resource.id)
        .collect();
    observation_resources.retain(|id| retained_resources.contains(id));
    validate_definition(&desired)?;
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
        observation_resources,
        drain,
        epochs,
        deprovision: BTreeSet::new(),
    })
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
        for port in node.descriptor.ports() {
            if port.direction() == PortDirection::Output {
                let Some(stream) = node.streams.get(port.id()) else {
                    continue;
                };
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
    let mut unique = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    let mut edges = Vec::new();
    let mut multicast = BTreeMap::new();
    let mut multicast_subscribers = BTreeSet::new();
    for (index, edge) in desired.relationships.iter().enumerate() {
        if let DesiredPipe::Qos(config) = &edge.pipe {
            if multicast
                .get(&config.resource)
                .is_some_and(|(from, definition)| {
                    *from != &edge.definition.from || *definition != &config.definition
                })
                || !multicast_subscribers.insert((&config.resource, &config.subscriber))
            {
                return Err(topology("QoS multicast requires one producer, consistent declarations and distinct subscribers"));
            }
            multicast.insert(
                &config.resource,
                (&edge.definition.from, &config.definition),
            );
        }
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
        if let DesiredPipe::Qos(config) = &edge.pipe {
            if nodes[ids[&edge.definition.from.component]]
                .output_streams
                .get(&edge.definition.from.port)
                != Some(&config.definition.stream)
            {
                return Err(topology(
                    "QoS channel stream differs from its producer output",
                ));
            }
        }
        let (to, input) = resolve(&ids, &nodes, &edge.definition.to)?;
        for component in [&edge.definition.from.component, &edge.definition.to.component] {
            let node = &nodes[ids[component]];
            super::super::validate_role(&node.descriptor, node.role, node.completion)?;
        }
        if !nodes[ids[&edge.definition.from.component]]
            .output_streams
            .contains_key(&edge.definition.from.port)
        {
            return Err(topology(
                "connect an output only after binding its producer stream",
            ));
        }
        let capabilities = edge.pipe.capabilities()?;
        super::super::validate_edge_contract(
            output,
            input,
            nodes[to].completion,
            &capabilities,
            &desired.requirements,
        )?;
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
        if edge.policy.required_for_binding
            && !destinations.insert((
                edge.definition.from.clone(),
                edge.definition.to.component.clone(),
            ))
        {
            return Err(topology(
                "one producer output cannot feed multiple ports on the same component",
            ));
        }
        validate_unbound_relationship(edge, &ids, &nodes)?;
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
    }
    let order = super::super::dependency_order(&ids, &edges, &desired.subscriptions)?;
    let mut resource_ids = BTreeSet::new();
    for resource in &desired.resources {
        if !resource_ids.insert(resource.id.clone()) {
            return Err(topology("duplicate resource"));
        }
        super::super::validate_identifier("resource binding", &resource.binding)?;
    }
    super::super::resources::construction_order(
        desired.resources.iter(),
        &desired.resource_dependencies,
    )?;
    Ok((nodes, edges, order))
}

pub(crate) fn validate_definition(desired: &DesiredTopology) -> GraphResult<()> {
    if desired.version != 1 {
        return Err(topology("unsupported desired topology version"));
    }
    super::super::validate_identifier("graph", &desired.graph_id)?;
    describe(desired)?;
    let ids: BTreeSet<_> = desired
        .components
        .iter()
        .map(|node| node.descriptor.id())
        .collect();
    let resources: BTreeSet<_> = desired
        .resources
        .iter()
        .map(|resource| &resource.id)
        .collect();
    if desired.recovery_requirements.iter().any(|requirement| {
        requirement.guarantees.is_empty() || !ids.contains(&requirement.consumer)
    }) {
        return Err(topology(
            "recovery assertion requires a known consumer and at least one guarantee",
        ));
    }
    if desired
        .control_connections
        .iter()
        .any(|(from, to)| from == to || !ids.contains(from) || !ids.contains(to))
        || desired
            .subscriptions
            .iter()
            .any(|(from, to)| from == to || !ids.contains(to))
        || desired
            .readiness_required
            .iter()
            .any(|id| !ids.contains(id))
        || desired.component_resources.iter().any(|(id, values)| {
            !ids.contains(id) || values.iter().any(|value| !resources.contains(value))
        })
        || desired.component_plugins.keys().any(|id| !ids.contains(id))
        || desired
            .resource_configurations
            .keys()
            .any(|id| !resources.contains(id))
    {
        return Err(topology(
            "desired topology references undeclared nodes or resources",
        ));
    }
    for node in &desired.components {
        if let ComponentConstruction::Factory(spec) = &node.construction {
            if spec.descriptor != node.descriptor
                || spec.role != node.role
                || spec.completion != node.completion
            {
                return Err(topology(
                    "factory specification and desired interface disagree",
                ));
            }
        }
    }
    for plugin in desired.component_plugins.values() {
        super::super::validate_identifier("plugin", &plugin.id)?;
        super::super::validate_identifier("plugin version", &plugin.version)?;
    }
    Ok(())
}

pub(super) struct Prepared {
    nodes: Vec<NodeSnapshot>,
    edges: Vec<EdgeSnapshot>,
    order: Vec<usize>,
    components: BTreeMap<ComponentId, Component>,
    pipes: BTreeMap<EdgeDefinition, Box<dyn PipeProvider>>,
    resources: BTreeMap<ResourceId, ResourceHandle>,
    resource_constructors: BTreeMap<ResourceId, Arc<dyn ResourceConstructor>>,
    factories: BTreeMap<ImplementationIdentity, Arc<dyn ComponentFactory>>,
}

struct PreparationSnapshot {
    snapshot: GraphSnapshot,
    observed: Arc<ObservedGraph>,
    components: Vec<Arc<InstanceSlot>>,
    ids: BTreeMap<ComponentId, usize>,
    edges: BTreeMap<usize, EdgeSnapshot>,
    providers: BTreeMap<usize, Arc<dyn PipeProvider>>,
    factories: BTreeMap<ImplementationIdentity, Arc<dyn ComponentFactory>>,
    resource_handles: BTreeMap<ResourceId, ResourceHandle>,
    resource_constructors: BTreeMap<ResourceId, Arc<dyn ResourceConstructor>>,
    execution_scope: Arc<str>,
    next_generation: u64,
    next_edge: usize,
    next_binding_generation: u64,
    next_resource_generation: u64,
}

impl PreparationSnapshot {
    fn capture(graph: &ComputationGraph) -> Self {
        Self {
            snapshot: graph.snapshot.clone(),
            observed: graph.observed(),
            components: graph.components.clone(),
            ids: graph.ids.clone(),
            edges: graph.edges.clone(),
            providers: graph.providers.clone(),
            factories: graph.factories.clone(),
            resource_handles: graph.resource_handles.clone(),
            resource_constructors: graph.resource_constructors.clone(),
            execution_scope: graph.execution_scope.clone(),
            next_generation: graph.next_generation,
            next_edge: graph.next_edge,
            next_binding_generation: graph.next_binding_generation,
            next_resource_generation: graph.next_resource_generation,
        }
    }
    fn observed(&self) -> &ObservedGraph {
        &self.observed
    }
}

async fn prepare_owned(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    commands: &mut CommandReceiver,
    plan: &ReconciliationPreview,
    bindings: TopologyBindings,
) -> GraphResult<(
    Prepared,
    Option<Arc<super::super::addition::RejectedResources>>,
)> {
    if !graph.tasks.preparations.is_empty() || !graph.tasks.pipes.is_empty() {
        return Err(GraphError::OperationInProgress);
    }
    let mut failures: Vec<_> = settle_retirements(graph, operations, controls, cancel, commands)
        .await
        .into_iter()
        .filter_map(Result::err)
        .collect();
    match failures.len() {
        0 => {}
        1 => return Err(failures.pop().expect("one retirement failure")),
        _ => {
            return Err(GraphError::Cleanup {
                primary: None,
                errors: failures,
            })
        }
    }
    let resources = super::super::addition::RejectedResources::new(
        plan.desired.resources.iter().cloned(),
        &bindings.resources,
    );
    if let Some(resources) = &resources {
        graph.rejected_resources.push(resources.clone());
    }
    let snapshot = PreparationSnapshot::capture(graph);
    let plan = plan.clone();
    let revision = graph.snapshot.revision;
    let identity = super::super::execution::GraphIdentity {
        graph: graph.snapshot.id.clone(),
        run_epoch: graph.observed().run_epoch,
        phase: FailurePhase::Validation,
    };
    let (abort, registration) = AbortHandle::new_pair();
    let joined = graph
        .tasks
        .preparations
        .spawn(identity.clone(), abort, async move {
            identity
                .run(async move {
                    Abortable::new(
                        async move { prepare(&snapshot, &plan, bindings) },
                        registration,
                    )
                    .await
                    .unwrap_or(Err(GraphError::Cancelled))
                })
                .await
        })?;
    let prepared = drive_reconciliation(
        graph,
        operations,
        controls,
        cancel,
        commands,
        graph.cleanup_timeout,
        async { joined.await? },
    )
    .await?;
    if graph.snapshot.revision != revision {
        return Err(GraphError::StaleRevision {
            expected: revision,
            actual: graph.snapshot.revision,
        });
    }
    Ok((prepared, resources))
}

fn validate_output_bindings(graph: &ComputationGraph, prepared: &Prepared) -> GraphResult<()> {
    for node in &prepared.nodes {
        if graph.component_retired(node.descriptor.id()) {
            continue;
        }
        if let Some(index) = graph.ids.get(node.descriptor.id()) {
            if let Some(bindings) = graph.components[*index]
                .recovery_contract()?
                .output_bindings
            {
                super::super::validate_output_bindings(
                    &bindings,
                    node,
                    &prepared.edges,
                    &prepared.resources,
                )?;
            }
        }
    }
    Ok(())
}

fn prepare(
    graph: &PreparationSnapshot,
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
    for id in &plan.resources {
        if !bindings.resource_constructors.contains_key(id)
            && !bindings.resources.contains_key(id)
            && graph.snapshot.resources.get(id) == declarations.get(id)
            && graph.snapshot.resource_configurations.get(id)
                == plan.desired.resource_configurations.get(id)
        {
            if let Some(constructor) = graph.resource_constructors.get(id) {
                bindings
                    .resource_constructors
                    .insert(id.clone(), constructor.clone());
            }
        }
    }
    for id in bindings.resource_constructors.keys() {
        if !plan.resources.contains(id) || !declarations.contains_key(id) {
            return Err(topology(
                "resource constructor was not included in the preview",
            ));
        }
        if bindings.resources.contains_key(id) {
            return Err(topology(
                "resource has both a constructor and a constructed binding",
            ));
        }
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
    super::super::resources::validate_constructed_dependencies(
        &plan.desired.resource_dependencies,
        &resources,
    )?;
    let mut components = BTreeMap::new();
    for node in &plan.desired.components {
        let id = node.descriptor.id();
        let construct = plan.create.contains(id) || plan.replace.contains(id);
        match &node.construction {
            ComponentConstruction::Factory(spec) => {
                if !construct
                    && !plan.update.contains(id)
                    && graph
                        .observed()
                        .components
                        .get(id)
                        .is_some_and(|node| node.failure.is_some())
                {
                    continue;
                }
                if spec.descriptor != node.descriptor
                    || spec.role != node.role
                    || spec.completion != node.completion
                {
                    return Err(topology(
                        "factory specification and desired interface disagree",
                    ));
                }
                let Some(factory) = factories.get(&spec.implementation) else {
                    if construct {
                        components
                            .insert(id.clone(), Component::Unresolved(Arc::new(node.clone())));
                        continue;
                    }
                    continue;
                };
                if !bindings.deferred_management_validation {
                    super::super::specification::validate_specification(
                        &plan.desired.graph_id,
                        spec,
                        factory.as_ref(),
                        &declarations,
                        &resources,
                    )?;
                }
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
                component
                    .0
                    .output_readers()
                    .validate(&node.descriptor)
                    .map_err(|error| topology(error.to_string()))?;
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
    let mut admission_snapshot = graph.snapshot.clone();
    admission_snapshot.nodes = nodes.clone().into();
    admission_snapshot.edges = edges.clone().into();
    admission_snapshot.subscriptions = plan.desired.subscriptions.clone().into();
    admission_snapshot.unbound_relationships = plan.desired.boundary_relationships.clone().into();
    for node in nodes
        .iter()
        .filter(|node| node.role == ComponentRole::Source)
    {
        let id = node.descriptor.id();
        let contract = if let Some(component) = components.get(id) {
            component.recovery_contract()
        } else if let Some(index) = graph.ids.get(id) {
            graph.components[*index].recovery_contract()?
        } else {
            continue;
        };
        if let Some(admission) = contract.source_admission {
            let scope = graph
                .observed()
                .components
                .get(id)
                .filter(|node| node.started)
                .map(|_| graph.execution_scope.as_ref());
            super::super::validate_admission(
                &admission,
                node,
                &admission_snapshot,
                &resources,
                scope,
            )?;
        }
    }
    for node in &nodes {
        let id = node.descriptor.id();
        let contract = if let Some(component) = components.get(id) {
            component.recovery_contract()
        } else if let Some(index) = graph.ids.get(id) {
            if graph.components[*index].consumer_progress()?.is_none() {
                continue;
            }
            graph.components[*index].recovery_contract()?
        } else {
            continue;
        };
        let Some(progress) = contract.consumer_progress else {
            continue;
        };
        if progress.producer_id() != id {
            return Err(topology("consumer progress belongs to another producer"));
        }
        let selected =
            super::super::consumer_progress::participants(&admission_snapshot, &progress);
        let mut contracts = BTreeMap::new();
        let mut pending = BTreeSet::new();
        for participant in selected {
            let contract = if let Some(component) = components.get(&participant) {
                if matches!(
                    component,
                    Component::Deferred { .. } | Component::Unresolved(_)
                ) {
                    pending.insert(participant.clone());
                }
                component.recovery_contract()
            } else if let Some(index) = graph.ids.get(&participant) {
                if !graph
                    .observed()
                    .components
                    .get(&participant)
                    .is_some_and(|node| node.realization == RealizationState::Created)
                {
                    pending.insert(participant.clone());
                }
                graph.components[*index].recovery_contract()?
            } else {
                pending.insert(participant.clone());
                crate::computation::v1::ComponentRecovery::default()
            };
            contracts.insert(participant, contract);
        }
        super::super::consumer_progress::analyze(
            &admission_snapshot,
            &progress,
            &contracts,
            &pending,
        )
        .validate(true)?;
    }
    if !plan.desired.recovery_requirements.is_empty() {
        let mut snapshot = graph.snapshot.clone();
        snapshot.nodes = nodes.clone().into();
        snapshot.edges = edges.clone().into();
        snapshot.subscriptions = plan.desired.subscriptions.clone().into();
        snapshot.unbound_relationships = plan.desired.boundary_relationships.clone().into();
        let selected: BTreeSet<_> = plan
            .desired
            .recovery_requirements
            .iter()
            .flat_map(|requirement| recovery_participants(&plan.desired, &requirement.consumer))
            .collect();
        let mut contracts = BTreeMap::new();
        let mut pending = BTreeSet::new();
        for node in &nodes {
            let id = node.descriptor.id();
            if !selected.contains(id) {
                continue;
            }
            let contract = if let Some(component) = components.get(id) {
                if matches!(
                    component,
                    Component::Deferred { .. } | Component::Unresolved(_)
                ) {
                    pending.insert(id.clone());
                }
                component.recovery_contract()
            } else if let Some(index) = graph.ids.get(id) {
                graph.components[*index].recovery_contract()?
            } else {
                pending.insert(id.clone());
                crate::computation::v1::ComponentRecovery::default()
            };
            contracts.insert(id.clone(), contract);
        }
        for requirement in &plan.desired.recovery_requirements {
            let report =
                super::super::recovery::assess(&snapshot, &resources, &contracts, requirement);
            if report.participants.is_disjoint(&pending)
                && bindings.resource_constructors.is_empty()
            {
                report.validate()?;
            }
        }
    }
    Ok(Prepared {
        nodes,
        edges,
        order,
        components,
        pipes,
        resources,
        resource_constructors: bindings.resource_constructors,
        factories,
    })
}

pub(super) async fn drive<T>(
    graph: &ComputationGraph,
    operations: &mut Operations,
    controls: &PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    operation: impl Future<Output = GraphResult<T>>,
) -> GraphResult<T> {
    drive_with_deadline(
        graph,
        operations,
        controls,
        cancel,
        graph.cleanup_timeout,
        operation,
    )
    .await
}

async fn drive_with_deadline<T>(
    graph: &ComputationGraph,
    operations: &mut Operations,
    controls: &PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    deadline: std::time::Duration,
    operation: impl Future<Output = GraphResult<T>>,
) -> GraphResult<T> {
    let operation = tokio::time::timeout(deadline, operation);
    tokio::pin!(operation);
    loop {
        operations.advance_drains(graph, controls)?;
        operations.advance_start(graph)?;
        operations.advance_stop(graph)?;
        let drain_timeout = operations.next_drain_deadline();
        tokio::select! {
            biased;
            _ = cancelled(cancel) => return Err(GraphError::Cancelled),
            _ = drain_deadline(drain_timeout) => {},
            completion = operations.drain_futures.next(), if !operations.drain_futures.is_empty() => {
                if let Some(completion) = completion { operations.complete_drain(graph, completion); }
            }
            completion = operations.control_futures.next(), if !operations.control_futures.is_empty() => {
                if let Some(completion) = completion { operations.complete_control(graph, completion); }
            }
            result = &mut operation => return result.map_err(|_| GraphError::ReconciliationTimeout)?,
            completion = operations.futures.next(), if !operations.futures.is_empty() => {
                if let Some(completion) = completion { operations.complete(graph, completion, controls)?; }
            }
        }
    }
}

async fn drive_reconciliation<T>(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    commands: &mut CommandReceiver,
    deadline: std::time::Duration,
    operation: impl Future<Output = GraphResult<T>>,
) -> GraphResult<T> {
    let operation = tokio::time::timeout(deadline, operation);
    tokio::pin!(operation);
    let mut peers = graph.peers.subscribe();
    loop {
        tokio::task::consume_budget().await;
        operations.advance_additions(graph)?;
        operations.advance_drains(graph, controls)?;
        operations.advance_start(graph)?;
        operations.advance_stop(graph)?;
        begin_handle_requests(graph, operations, controls, cancel).await?;
        let drain_timeout = operations.next_drain_deadline();
        tokio::select! {
            _ = cancelled(cancel) => return Err(GraphError::Cancelled),
            job = graph.tasks.resources.next(), if !graph.tasks.resources.is_empty() => {
                if let Err(error) = super::super::resource_tasks::complete(graph, &job) {
                    log::debug!("Resource callback completed with a retained failure: {error}");
                }
                operations.dependencies_changed();
            }
            changed = peers.changed() => {
                changed.map_err(|_| GraphError::ControllerClosed)?;
            }
            _ = drain_deadline(drain_timeout) => {},
            completion = operations.drain_futures.next(), if !operations.drain_futures.is_empty() => {
                if let Some(completion) = completion { operations.complete_drain(graph, completion); }
            }
            completion = operations.control_futures.next(), if !operations.control_futures.is_empty() => {
                if let Some(completion) = completion { operations.complete_control(graph, completion); }
            }
            result = &mut operation => return result.map_err(|_| GraphError::ReconciliationTimeout)?,
            completion = operations.futures.next(), if !operations.futures.is_empty() => {
                if let Some(completion) = completion { operations.complete(graph, completion, controls)?; }
            }
            result = operations.retirements.next(), if !operations.retirements.is_empty() => {
                if let Some(result) = result {
                    let completed = operations.complete_retirement(graph, result);
                    if operations.retirements.is_empty() { operations.retiring.clear(); }
                    completed?;
                }
            }
            command = commands.recv() => {
                handle_command(graph, operations, controls, cancel,
                    command.ok_or(GraphError::ControllerClosed)?, commands).await?;
            }
        }
    }
}

async fn settle_retirements(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    commands: &mut CommandReceiver,
) -> Vec<GraphResult<()>> {
    let deadline = tokio::time::Instant::now() + graph.cleanup_timeout;
    let mut pending = std::mem::take(&mut operations.retirements);
    let mut outcomes = Vec::new();
    while !pending.is_empty() {
        let result = drive_reconciliation(
            graph,
            operations,
            controls,
            cancel,
            commands,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
            async { pending.next().await.ok_or(GraphError::ControllerClosed) },
        )
        .await;
        let interrupted = result.is_err();
        outcomes.push(match result {
            Ok(completion) => operations.complete_retirement(graph, completion),
            Err(error) => Err(error),
        });
        pending.extend(std::mem::take(&mut operations.retirements));
        if interrupted {
            break;
        }
    }
    operations.retirements.extend(pending);
    if operations.retirements.is_empty() {
        if graph.tasks.retirements.is_empty() {
            operations.retiring.clear();
        } else {
            outcomes.push(Err(GraphError::OperationInProgress));
        }
    }
    outcomes
}

async fn pause_reconciliation(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    commands: &mut CommandReceiver,
    index: usize,
) -> GraphResult<()> {
    let Some(active) = operations.active.get(&index) else {
        return Ok(());
    };
    if active.operation != Operation::Process {
        return Err(GraphError::OperationInProgress);
    }
    operations.pause_for_drain(graph, index);
    let id = graph.nodes[index].descriptor.id().clone();
    let mut observed = graph.observed.subscribe();
    let result = drive_reconciliation(
        graph,
        operations,
        controls,
        cancel,
        commands,
        graph.cleanup_timeout,
        async move {
            let state = observed
                .wait_for(|state| state.components[&id].lifecycle != ComponentLifecycle::Quiescing)
                .await
                .map_err(|_| GraphError::ControllerClosed)?;
            match &state.components[&id].failure {
                Some(failure) => Err(GraphError::Reported {
                    cause: failure.cause.clone(),
                }),
                None => Ok(()),
            }
        },
    )
    .await;
    if result.is_err() {
        operations.resume_quiescing(graph, &BTreeSet::from([index]));
    }
    result
}

pub(super) async fn quiesce(
    graph: &ComputationGraph,
    operations: &mut Operations,
    controls: &PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    selected: BTreeSet<usize>,
) -> GraphResult<Vec<ComponentId>> {
    if operations.conflicts(&selected, false)
        || selected.iter().any(|index| {
            operations
                .active
                .get(index)
                .is_some_and(|active| active.operation != Operation::Process)
        })
    {
        return Err(GraphError::OperationInProgress);
    }
    let ids: BTreeSet<_> = selected
        .iter()
        .map(|index| graph.nodes[*index].descriptor.id().clone())
        .collect();
    let result = async {
        for &index in &graph.order {
            if !selected.contains(&index) {
                continue;
            }
            for (&edge_index, edge) in &graph.edges {
                let from = graph.ids[&edge.definition.from.component];
                if &edge.definition.to.component != graph.nodes[index].descriptor.id()
                    || !selected.contains(&from)
                    || operations.invalidated_bindings.contains(&edge_index)
                {
                    continue;
                }
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
            pause(graph, operations, controls, cancel, index).await?;
        }
        Ok::<_, GraphError>(())
    }
    .await;
    if let Err(error) = result {
        if !matches!(error, GraphError::Cancelled) {
            if let Err(resume_error) = resume(graph, operations, &ids) {
                return Err(GraphError::Cleanup {
                    primary: Some(Box::new(error)),
                    errors: vec![resume_error],
                });
            }
        }
        return Err(error);
    }
    Ok(ids.into_iter().collect())
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
                completion = operations.control_futures.next(), if !operations.control_futures.is_empty() => {
                    if let Some(completion) = completion { operations.complete_control(graph, completion); }
                }
                _ = &mut deadline => {
                    operations.resume_quiescing(graph, &BTreeSet::from([index]));
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

pub(super) fn resume(
    graph: &ComputationGraph,
    operations: &mut Operations,
    ids: &BTreeSet<ComponentId>,
) -> GraphResult<()> {
    let mut errors = Vec::new();
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
            if let Err(error) = operations.launch(graph, index, Operation::Process) {
                operations.paused.insert(index);
                let failed = failure(
                    GraphError::Component {
                        component: id.clone(),
                        operation: "resume processing",
                        source: error.into(),
                    },
                    FailurePhase::Activation,
                );
                update(graph, |state| {
                    let node = state.components.get_mut(id).expect("component");
                    node.lifecycle = ComponentLifecycle::Quiesced;
                    node.health = ComponentHealth::Unavailable;
                    node.failure = Some(failed.clone());
                });
                errors.push(GraphError::Reported {
                    cause: failed.cause,
                });
            }
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(GraphError::Cleanup {
            primary: None,
            errors,
        })
    }
}

async fn stop_instance(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    index: usize,
    deprovision: bool,
    commands: &mut CommandReceiver,
) -> GraphResult<()> {
    let slot = graph.components[index].clone();
    if operations
        .peer_controls
        .get(&index)
        .is_some_and(|control| control.generation() == slot.generation)
    {
        graph.peers.reset_ready(&slot.id, slot.generation)?;
    }
    if !slot.take()?.attempted && !deprovision {
        return Ok(());
    }
    if deprovision {
        if let Some(cause) = slot.damage() {
            return Err(GraphError::Reported { cause });
        }
    }
    operations.launch_cleanup(graph, index, Operation::Stop, deprovision)?;
    let (reply, response) = oneshot::channel();
    operations
        .active
        .get_mut(&index)
        .expect("owned stop task")
        .reply = Some(reply);
    drive_reconciliation(
        graph,
        operations,
        controls,
        cancel,
        commands,
        graph.cleanup_timeout,
        async { response.await.map_err(|_| GraphError::ControllerClosed)? },
    )
    .await
}

async fn pause_control(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    index: usize,
    commands: &mut CommandReceiver,
) -> GraphResult<()> {
    operations.control_quiescing.insert(index);
    if let Some(abort) = operations.control_aborts.get(&index) {
        abort.abort();
    }
    let deadline = tokio::time::sleep(graph.cleanup_timeout);
    tokio::pin!(deadline);
    while operations.control_aborts.contains_key(&index) {
        operations.advance_drains(graph, controls)?;
        operations.advance_start(graph)?;
        operations.advance_stop(graph)?;
        begin_handle_requests(graph, operations, controls, cancel).await?;
        tokio::select! {
            biased;
            _ = cancelled(cancel) => return Err(GraphError::Cancelled),
            _ = &mut deadline => return Err(GraphError::ReconciliationTimeout),
            completion = operations.control_futures.next(), if !operations.control_futures.is_empty() => {
                if let Some(completion) = completion { operations.complete_control(graph, completion); }
            }
            completion = operations.futures.next(), if !operations.futures.is_empty() => {
                if let Some(completion) = completion { operations.complete(graph, completion, controls)?; }
            }
            command = commands.recv() => {
                handle_command(graph, operations, controls, cancel,
                    command.ok_or(GraphError::ControllerClosed)?, commands).await?;
            }
        }
    }
    Ok(())
}

/// Applies a previewed topology change.
///
/// Before `commit_desired`, an error leaves the previous desired topology in
/// place. After it, the change is observable, so the only error is
/// [`GraphError::Cancelled`]; everything else is recorded on the returned
/// report (see [`ReconciliationReport`]).
pub(super) async fn execute(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    supplied: ReconciliationPreview,
    bindings: TopologyBindings,
    commands: &mut CommandReceiver,
) -> GraphResult<ReconciliationReport> {
    if operations.reconciliation.is_some() {
        return Err(GraphError::OperationInProgress);
    }
    operations.reconciliation = Some(ReconciliationReservation {
        components: supplied
            .pause
            .iter()
            .chain(&supplied.create)
            .chain(&supplied.replace)
            .chain(&supplied.update)
            .chain(&supplied.remove)
            .chain(&supplied.restart)
            .cloned()
            .collect(),
        resources: supplied
            .resources
            .union(&supplied.remove_resources)
            .cloned()
            .collect(),
        resource_instances: graph
            .resource_handles
            .iter()
            .chain(graph.pending_resource_cleanup.iter())
            .filter(|(id, _)| {
                supplied.resources.contains(*id) || supplied.remove_resources.contains(*id)
            })
            .map(|(_, handle)| handle.scope_identity())
            .collect(),
        committed: false,
    });
    let result = execute_reserved(
        graph, operations, controls, cancel, supplied, bindings, commands,
    )
    .await;
    operations.reconciliation = None;
    operations.dependencies_changed();
    let metadata = flush_metadata(graph, operations, controls, cancel, commands).await;
    match result {
        Ok(mut report) => {
            record_committed(&mut report, FailurePhase::Control, metadata)?;
            if !report.failures.is_empty() {
                report.summary = OperationSummary::CompletedWithFailures;
            }
            Ok(report)
        }
        Err(primary) => match metadata {
            Ok(()) => Err(primary),
            Err(error) => Err(GraphError::Cleanup {
                primary: Some(Box::new(primary)),
                errors: vec![error],
            }),
        },
    }
}

async fn execute_reserved(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    supplied: ReconciliationPreview,
    bindings: TopologyBindings,
    commands: &mut CommandReceiver,
) -> GraphResult<ReconciliationReport> {
    check_revision(graph, supplied.revision)?;
    if supplied.desired.graph_id != graph.snapshot.id.as_ref() {
        return Err(topology("preview belongs to another graph"));
    }
    let affected: BTreeSet<_> = supplied
        .pause
        .iter()
        .filter_map(|id| graph.ids.get(id).copied())
        .collect();
    if operations.conflicts(&affected, true) || !operations.propagated_stop.is_disjoint(&affected) {
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
    let mut plan = preview(graph, supplied.changes.clone())?;
    if !supplied.same_impact(&plan) {
        return Err(GraphError::StalePreview {
            revision: supplied.revision,
        });
    }
    if !supplied.deprovision.is_subset(&plan.remove) {
        return Err(topology(
            "state cleanup must only target removed components",
        ));
    }
    plan.deprovision = supplied.deprovision;
    if plan
        .resources
        .iter()
        .chain(&plan.remove_resources)
        .any(|id| graph.tasks.resources.contains(id))
    {
        return Err(GraphError::OperationInProgress);
    }
    let defer_activation = bindings.defer_activation;
    let (mut prepared, supplied_resources) = prepare_owned(
        graph, operations, controls, cancel, commands, &plan, bindings,
    )
    .await?;
    validate_output_bindings(graph, &prepared)?;
    for id in plan.remove.iter().chain(&plan.replace) {
        let Some(&index) = graph.ids.get(id) else {
            continue;
        };
        if let Some(active) = operations
            .active
            .get(&index)
            .filter(|active| active.operation == Operation::Create)
        {
            active.abort.abort();
            operations.stop_after_abort.insert(index);
            let deadline = tokio::time::sleep(graph.cleanup_timeout);
            tokio::pin!(deadline);
            while operations.active.contains_key(&index) {
                operations.advance_drains(graph, controls)?;
                operations.advance_start(graph)?;
                operations.advance_stop(graph)?;
                begin_handle_requests(graph, operations, controls, cancel).await?;
                tokio::select! {
                    biased;
                    _ = cancelled(cancel) => return Err(GraphError::Cancelled),
                    _ = &mut deadline => return Err(GraphError::ReconciliationTimeout),
                    completion = operations.control_futures.next(), if !operations.control_futures.is_empty() => {
                        if let Some(completion) = completion { operations.complete_control(graph, completion); }
                    }
                    completion = operations.futures.next() => {
                        if let Some(completion) = completion { operations.complete(graph, completion, controls)?; }
                    }
                    command = commands.recv() => {
                        handle_command(graph, operations, controls, cancel,
                            command.ok_or(GraphError::ControllerClosed)?, commands).await?;
                    }
                }
            }
        }
    }
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
            let consumer = graph.nodes[index].descriptor.id().clone();
            // Drain incoming work while this consumer is still active, then
            // park it before advancing to its downstream consumers.
            for edge in changing_edges
                .iter()
                .filter(|edge| edge.to.component == consumer)
            {
                // Retirement already proved drain and fenced these exact owners.
                // Asking their now-closed journal to drain would block cleanup.
                if graph.component_retired(&edge.from.component)
                    && graph.component_retired(&edge.to.component)
                {
                    continue;
                }
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
                    drive_reconciliation(
                        graph,
                        operations,
                        controls,
                        cancel,
                        commands,
                        graph.cleanup_timeout,
                        async { progress.drained(control.as_ref()).await },
                    )
                    .await?;
                }
            }
            if plan.pause.contains(graph.nodes[index].descriptor.id()) {
                pause_reconciliation(graph, operations, controls, cancel, commands, index).await?;
            }
        }
        // A producer can commit between preflight and quiescence. Recheck before
        // stopping/replacing anything or releasing the original pipe bindings.
        validate_output_bindings(graph, &prepared)?;
        Ok::<_, GraphError>(())
    }
    .await;
    if let Err(error) = boundary {
        if matches!(error, GraphError::Cancelled) {
            return Err(error);
        }
        report.failures.push(failure(error, FailurePhase::Removal));
        report.summary = OperationSummary::CompletedWithFailures;
        let resumed = resume(graph, operations, &plan.pause);
        record_committed(&mut report, FailurePhase::Activation, resumed)?;
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
    for id in plan.remove.iter().chain(&plan.replace) {
        if let Some(&index) = graph.ids.get(id) {
            if let Err(error) =
                pause_control(graph, operations, controls, cancel, index, commands).await
            {
                if matches!(error, GraphError::Cancelled) {
                    return Err(error);
                }
                report.failures.push(failure(error, FailurePhase::Control));
                report.summary = OperationSummary::CompletedWithFailures;
                let resumed = resume(graph, operations, &plan.pause);
                record_committed(&mut report, FailurePhase::Activation, resumed)?;
                return Ok(report);
            }
        }
    }
    for index in graph.order.clone() {
        let id = graph.nodes[index].descriptor.id().clone();
        if stop.contains(&id) {
            if let Err(error) = stop_instance(
                graph,
                operations,
                controls,
                cancel,
                index,
                plan.deprovision.contains(&id),
                commands,
            )
            .await
            {
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
        let cleanup_order = super::super::resources::cleanup_order(
            graph.snapshot.resources.values(),
            &graph.snapshot.resource_dependencies,
        )?;
        for id in cleanup_order
            .iter()
            .filter(|id| plan.resources.contains(*id) || plan.remove_resources.contains(*id))
        {
            let Some(spec) = graph.snapshot.resources.get(id) else {
                continue;
            };
            if !graph.resource_handles.contains_key(id)
                && !graph.pending_resource_cleanup.contains_key(id)
                && !graph.tasks.resources.contains(id)
            {
                continue;
            }
            if spec.ownership == ResourceOwnership::Graph {
                cleanup_touched.insert(id.clone());
                update(graph, |state| {
                    state.resources.get_mut(id).expect("resource").realization =
                        ResourceRealization::CleanupRequired
                });
                let result = match super::super::resources::ensure_released_dependents(graph, id) {
                    Err(error) => Err(error),
                    Ok(()) => {
                        let job = super::super::resource_tasks::shutdown(graph, id)?;
                        let result = drive_reconciliation(
                            graph,
                            operations,
                            controls,
                            cancel,
                            commands,
                            graph.cleanup_timeout,
                            job.wait(),
                        )
                        .await;
                        if job.joined() {
                            super::super::resource_tasks::complete(graph, &job)
                        } else {
                            result
                        }
                    }
                };
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
                    debug_assert!(!graph.tasks.resources.contains(id));
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
        let resumed = resume(
            graph,
            operations,
            &plan.pause.difference(&stop).cloned().collect(),
        );
        record_committed(&mut report, FailurePhase::Activation, resumed)?;
        return Ok(report);
    }
    let retirement_failures =
        match commit_desired(graph, operations, controls, &plan, &mut prepared) {
            Ok(failures) => failures,
            Err(error) => {
                report.failures.push(failure(error, FailurePhase::Binding));
                report.summary = OperationSummary::CompletedWithFailures;
                let resumed = resume(
                    graph,
                    operations,
                    &plan.pause.difference(&stop).cloned().collect(),
                );
                record_committed(&mut report, FailurePhase::Activation, resumed)?;
                return Ok(report);
            }
        };
    if defer_activation {
        graph.deferred_activation.extend(
            plan.create
                .iter()
                .chain(&plan.replace)
                .chain(&plan.restart)
                .cloned(),
        );
    }
    report.revision = graph.snapshot.revision;
    report.committed = true;
    if let Some(resources) = supplied_resources {
        resources.adopted(&graph.resource_handles).await;
        graph
            .rejected_resources
            .retain(|resources| !resources.complete());
    }
    operations
        .reconciliation
        .as_mut()
        .expect("reserved reconciliation")
        .committed = true;
    let mut retirement_failed = !retirement_failures.is_empty();
    report.failures.extend(retirement_failures);
    report.removed = plan.remove.clone();
    for result in settle_retirements(graph, operations, controls, cancel, commands).await {
        retirement_failed |= result.is_err();
        record_committed(&mut report, FailurePhase::Removal, result)?;
    }
    if retirement_failed {
        report.summary = OperationSummary::CompletedWithFailures;
        return Ok(report);
    }
    for index in graph.order.clone() {
        if !operations.active.contains_key(&index) {
            if let Err(error) = operations.attach_control(graph, index) {
                let failed = failure(error, FailurePhase::Binding);
                update(graph, |state| {
                    let node = state
                        .components
                        .get_mut(graph.nodes[index].descriptor.id())
                        .expect("committed node");
                    node.failure = Some(failed.clone());
                    node.health = ComponentHealth::Unavailable;
                });
                report.failures.push(failed);
            }
        }
    }
    let connections = operations.refresh_connections(graph);
    record_committed(&mut report, FailurePhase::Binding, connections)?;
    // The desired topology is now published and observable, so the caller must
    // receive this report: from here on only controller cancellation aborts, and
    // every other failure is recorded against the committed revision.
    let resources = realize_resources(
        graph,
        operations,
        controls,
        cancel,
        &mut prepared,
        &mut report,
        commands,
    )
    .await;
    let mut realized = record_committed(&mut report, FailurePhase::Creation, resources)?.is_some();
    if realized {
        let components = realize(
            graph,
            operations,
            controls,
            cancel,
            &plan,
            &mut report,
            commands,
        )
        .await;
        realized = record_committed(&mut report, FailurePhase::Creation, components)?.is_some();
    }
    verify_recovery(graph, &plan, &mut report);
    let affected: BTreeSet<_> = plan
        .pause
        .iter()
        .chain(&plan.create)
        .chain(&plan.replace)
        .chain(&plan.restart)
        .chain(&activate_blocked)
        .filter_map(|id| graph.ids.get(id).copied())
        .collect();
    let settled = settle(graph, operations, controls, cancel, &affected, commands).await;
    let settled = record_committed(&mut report, FailurePhase::Activation, settled)?.is_some();
    // Producers paused for the commit are resumed even when the graph did not
    // settle; leaving them paused would silently stall every downstream reader.
    let resumed = resume(graph, operations, &plan.pause);
    record_committed(&mut report, FailurePhase::Activation, resumed)?;
    let start: BTreeSet<_> = plan
        .create
        .iter()
        .chain(&plan.replace)
        .chain(&plan.restart)
        .chain(&activate_blocked)
        .filter_map(|id| graph.ids.get(id).copied())
        .collect();
    if !start.is_empty() && realized {
        if settled {
            let (reply, receiver) = oneshot::channel();
            operations.begin_start(graph, start, Some(reply), false);
            let startup = drive_reconciliation(
                graph,
                operations,
                controls,
                cancel,
                commands,
                graph.cleanup_timeout,
                async { receiver.await.map_err(|_| GraphError::ControllerClosed)? },
            )
            .await;
            if let Some(startup) = record_committed(&mut report, FailurePhase::Activation, startup)?
            {
                report.startup = Some(startup);
            }
        } else {
            // A conflicting lifecycle group is still active. Preserve its
            // report and start these members when their own domain is free.
            defer_startup(graph, operations, start);
        }
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

/// Records a post-commit step's failure on the committed report.
///
/// Returns the step's value, or `None` once its failure is recorded. Only
/// [`GraphError::Cancelled`] propagates: it means the controller is shutting
/// down, and the caller replies with it before tearing the graph down.
fn record_committed<T>(
    report: &mut ReconciliationReport,
    phase: FailurePhase,
    result: GraphResult<T>,
) -> GraphResult<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(GraphError::Cancelled) => Err(GraphError::Cancelled),
        Err(error) => {
            report.failures.push(failure(error, phase));
            Ok(None)
        }
    }
}

/// Records every recovery requirement that this reconciliation touched and that
/// the committed topology no longer satisfies.
fn verify_recovery(
    graph: &ComputationGraph,
    plan: &ReconciliationPreview,
    report: &mut ReconciliationReport,
) {
    for requirement in &graph.snapshot.recovery_requirements {
        let participants =
            recovery::participants(&requirement.consumer, graph.snapshot.data_connections());
        if participants.is_disjoint(&plan.pause)
            && participants.is_disjoint(&plan.create)
            && participants.is_disjoint(&plan.replace)
        {
            continue;
        }
        let verified = graph
            .recovery_report(requirement)
            .and_then(|assessment| assessment.validate().map_err(GraphError::Recovery));
        if let Err(error) = verified {
            report
                .failures
                .push(failure(error, FailurePhase::Validation));
        }
    }
}

/// Waits, within the cleanup deadline, for lifecycle groups that began while
/// this reconciliation was realizing components.
async fn settle(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    selected: &BTreeSet<usize>,
    commands: &mut CommandReceiver,
) -> GraphResult<()> {
    let deadline = tokio::time::sleep(graph.cleanup_timeout);
    tokio::pin!(deadline);
    while operations.conflicts(selected, true) {
        operations.advance_drains(graph, controls)?;
        operations.advance_start(graph)?;
        operations.advance_stop(graph)?;
        begin_handle_requests(graph, operations, controls, cancel).await?;
        if !operations.conflicts(selected, true) {
            break;
        }
        tokio::select! {
            biased;
            _ = cancelled(cancel) => return Err(GraphError::Cancelled),
            _ = &mut deadline => return Err(GraphError::ReconciliationTimeout),
            completion = operations.drain_futures.next(), if !operations.drain_futures.is_empty() => {
                if let Some(completion) = completion { operations.complete_drain(graph, completion); }
            }
            completion = operations.control_futures.next(), if !operations.control_futures.is_empty() => {
                if let Some(completion) = completion { operations.complete_control(graph, completion); }
            }
            completion = operations.futures.next(), if !operations.futures.is_empty() => {
                if let Some(completion) = completion { operations.complete(graph, completion, controls)?; }
            }
            command = commands.recv() => {
                handle_command(graph, operations, controls, cancel,
                    command.ok_or(GraphError::ControllerClosed)?, commands).await?;
            }
        }
    }
    Ok(())
}

/// Queues the components that a reconciliation would have started, applying
/// the same eligibility as an immediate startup, so they start automatically
/// once the in-flight lifecycle group completes.
fn defer_startup(graph: &ComputationGraph, operations: &mut Operations, start: BTreeSet<usize>) {
    let observed = graph.observed();
    for index in start {
        let id = graph.nodes[index].descriptor.id();
        if graph.snapshot.lifecycle_policies[id].auto_start
            && !graph.deferred_activation.contains(id)
            && observed.components[id].realization == RealizationState::Created
        {
            operations.pending_auto_start.insert(index, false);
        }
    }
}

async fn realize_resources(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    prepared: &mut Prepared,
    report: &mut ReconciliationReport,
    commands: &mut CommandReceiver,
) -> GraphResult<()> {
    let mut constructors = std::mem::take(&mut prepared.resource_constructors);
    let order = super::super::resources::construction_order(
        graph.snapshot.resources.values(),
        &graph.snapshot.resource_dependencies,
    )?;
    for id in order {
        let Some(constructor) = constructors.remove(&id) else {
            continue;
        };
        graph
            .resource_constructors
            .insert(id.clone(), constructor.clone());
        let specification = graph.snapshot.resources[&id].clone();
        if graph
            .snapshot
            .resource_dependencies
            .get(&id)
            .into_iter()
            .flat_map(|dependencies| dependencies.keys())
            .any(|dependency| !graph.resource_handles.contains_key(dependency))
        {
            continue;
        }
        let dependencies: GraphResult<BTreeMap<_, _>> = graph
            .snapshot
            .resource_dependencies
            .get(&id)
            .into_iter()
            .flat_map(|required| required.iter())
            .map(|(dependency, role)| {
                graph
                    .resource_handles
                    .get(dependency)
                    .filter(|handle| handle.role() == *role)
                    .cloned()
                    .map(|handle| (dependency.clone(), handle))
                    .ok_or_else(|| GraphError::ResourceCreation {
                        resource: id.clone(),
                        source: anyhow::anyhow!("required resource {dependency} is unavailable"),
                    })
            })
            .collect();
        let result = match dependencies {
            Err(error) => Err(error),
            Ok(dependencies) => {
                let job = super::super::resource_tasks::construct(
                    graph,
                    &id,
                    constructor,
                    dependencies,
                    specification.role,
                )?;
                let result = drive_reconciliation(
                    graph,
                    operations,
                    controls,
                    cancel,
                    commands,
                    std::time::Duration::from_secs(30),
                    job.wait(),
                )
                .await;
                if job.joined() {
                    super::super::resource_tasks::complete(graph, &job)
                } else {
                    result
                }
            }
        };
        let failed = match result {
            Ok(()) => None,
            Err(GraphError::Cancelled) => return Err(GraphError::Cancelled),
            Err(error) => {
                let failure = failure(error, FailurePhase::Creation);
                report.failures.push(failure.clone());
                Some(failure)
            }
        };
        update(graph, |state| {
            let resource = state.resources.get_mut(&id).expect("declared resource");
            resource.realization = if graph.pending_resource_cleanup.contains_key(&id) {
                ResourceRealization::CleanupRequired
            } else if failed.is_some() {
                ResourceRealization::Pending
            } else {
                ResourceRealization::Created
            };
            resource.failure = failed;
            resource.transition_time = Utc::now();
        });
    }
    Ok(())
}

fn commit_desired(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    plan: &ReconciliationPreview,
    prepared: &mut Prepared,
) -> GraphResult<Vec<ComponentFailure>> {
    let next_generation = graph
        .next_generation
        .checked_add(
            u64::try_from(prepared.components.len())
                .map_err(|_| topology("too many component generations"))?,
        )
        .ok_or_else(|| topology("construction generation exhausted"))?;
    let next_edge = graph
        .next_edge
        .checked_add(prepared.pipes.len())
        .ok_or_else(|| topology("relationship generation exhausted"))?;
    let next_binding = graph
        .next_binding_generation
        .checked_add(
            u64::try_from(plan.rebind.len())
                .map_err(|_| topology("too many binding generations"))?,
        )
        .ok_or_else(|| topology("binding generation exhausted"))?;
    let next_resource = graph
        .next_resource_generation
        .checked_add(
            u64::try_from(plan.resources.len())
                .map_err(|_| topology("too many resource generations"))?,
        )
        .ok_or_else(|| topology("resource generation exhausted"))?;
    let old_edges: Vec<_> = graph
        .edges
        .iter()
        .filter(|(_, edge)| {
            plan.rebind.contains(&edge.definition) || plan.unbind.contains(&edge.definition)
        })
        .map(|(index, _)| *index)
        .collect();
    let mut required = BTreeSet::new();
    for index in &old_edges {
        let edge = &graph.edges[index];
        for endpoint in [&edge.definition.from, &edge.definition.to] {
            required.insert(
                *graph
                    .ids
                    .get(&endpoint.component)
                    .ok_or_else(|| topology("edge endpoint is missing"))?,
            );
        }
    }
    let mut retired_controls = Vec::new();
    for id in plan.remove.iter().chain(prepared.components.keys()) {
        if let Some(&index) = graph.ids.get(id) {
            required.insert(index);
            if operations.peer_controls.contains_key(&index) {
                retired_controls.push((id.clone(), graph.components[index].generation));
            }
        } else if plan.remove.contains(id) {
            return Err(GraphError::StaleGeneration);
        }
    }
    let mut leases = BTreeMap::new();
    for index in required.iter().copied() {
        leases.insert(index, graph.components[index].take()?);
    }
    let mut future_ids = graph.ids.clone();
    for id in &plan.remove {
        future_ids.remove(id);
    }
    let mut next_index = graph.components.len();
    for node in &prepared.nodes {
        let id = node.descriptor.id();
        if !future_ids.contains_key(id) {
            if !prepared.components.contains_key(id) {
                return Err(topology("new component has no prepared instance"));
            }
            future_ids.insert(id.clone(), next_index);
            next_index = next_index
                .checked_add(1)
                .ok_or_else(|| topology("component index exhausted"))?;
        }
    }
    let order = prepared
        .order
        .iter()
        .map(|index| {
            prepared
                .nodes
                .get(*index)
                .and_then(|node| future_ids.get(node.descriptor.id()))
                .copied()
                .ok_or_else(|| topology("prepared ordering references a missing component"))
        })
        .collect::<GraphResult<Vec<_>>>()?;
    let pairs = prepared
        .edges
        .iter()
        .map(|edge| {
            (
                edge.definition.from.component.clone(),
                edge.definition.to.component.clone(),
            )
        })
        .chain(plan.desired.control_connections.iter().cloned())
        .chain(plan.desired.subscriptions.iter().cloned())
        .collect();
    let rebound = plan
        .rebind
        .iter()
        .map(|edge| (edge.from.component.clone(), edge.to.component.clone()))
        .collect();
    let mut replacements = BTreeMap::new();
    for ((id, component), generation) in std::mem::take(&mut prepared.components)
        .into_iter()
        .zip(graph.next_generation..next_generation)
    {
        let slot = InstanceSlot::declared(
            id.clone(),
            component,
            ComponentGeneration(generation),
            graph.metadata_changes.clone(),
        );
        let lease = slot.take()?;
        replacements.insert(id, (slot, lease));
    }
    let peers = graph.peers.clone();
    let peer_commit = peers.prepare_topology_commit(&retired_controls, pairs, &rebound)?;
    let binding_generations: BTreeMap<_, _> = plan
        .rebind
        .iter()
        .cloned()
        .zip(graph.next_binding_generation..next_binding)
        .collect();
    let resource_generations: BTreeMap<_, _> = plan
        .resources
        .iter()
        .cloned()
        .zip(graph.next_resource_generation..next_resource)
        .collect();
    // Every fallible ownership/counter check precedes the first mutation.
    let mut cancelled_controls = Vec::new();
    let mut retired_instances = Vec::new();
    let mut retired_providers = Vec::new();
    let mut retired_outputs = Vec::new();
    let mut retired_inputs = Vec::new();
    let mut retired_handlers = Vec::new();
    for index in old_edges {
        operations.invalidated_bindings.remove(&index);
        let edge = graph.edges.remove(&index).expect("edge");
        if let Some(control) = controls.0.remove(&index) {
            cancelled_controls.push(control);
        }
        if let Some(provider) = graph.providers.remove(&index) {
            retired_providers.push(provider);
        }
        let from = graph.ids[&edge.definition.from.component];
        let to = graph.ids[&edge.definition.to.component];
        let outputs = &mut leases
            .get_mut(&from)
            .expect("prepared source lease")
            .outputs;
        for output in std::mem::take(outputs) {
            if output.edge == index {
                retired_outputs.push(output);
            } else {
                outputs.push(output);
            }
        }
        let inputs = &mut leases.get_mut(&to).expect("prepared receiver lease").inputs;
        for input in std::mem::take(inputs) {
            if input.edge == index {
                retired_inputs.push(input);
            } else {
                inputs.push(input);
            }
        }
    }
    for id in &plan.remove {
        let index = graph.ids.remove(id).expect("removed component");
        graph.deferred_activation.remove(id);
        graph.reported_resources.remove(id);
        retired_handlers.push(index);
        let lease = leases.get_mut(&index).expect("prepared removal lease");
        if let Some(instance) = lease.value.take() {
            retired_instances.push((
                lease.slot.clone(),
                graph.observed().components[id].operation,
                instance,
            ));
        }
        operations.paused.remove(&index);
        operations.exhausted.remove(&index);
    }
    for node in &prepared.nodes {
        let id = node.descriptor.id().clone();
        if let Some((slot, mut new)) = replacements.remove(&id) {
            if let Some(&index) = graph.ids.get(&id) {
                graph.reported_resources.remove(&id);
                retired_handlers.push(index);
                let old = leases.get_mut(&index).expect("prepared replacement lease");
                new.inputs = std::mem::take(&mut old.inputs);
                new.sink_batch = old.sink_batch.take();
                new.outputs = std::mem::take(&mut old.outputs);
                new.sequences = std::mem::take(&mut old.sequences)
                    .into_iter()
                    .filter(|(port, _)| {
                        node.output_streams.contains_key(port)
                            && graph.nodes[index].output_streams.get(port)
                                == node.output_streams.get(port)
                    })
                    .collect();
                if let Some(instance) = old.value.take() {
                    retired_instances.push((
                        old.slot.clone(),
                        graph.observed().components[&id].operation,
                        instance,
                    ));
                }
                drop(new);
                graph.components[index] = slot;
                operations.exhausted.remove(&index);
                operations.paused.remove(&index);
                graph.nodes[index] = node.clone();
            } else {
                drop(new);
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
            graph.providers.insert(graph.next_edge, Arc::from(provider));
            graph.next_edge += 1;
        } else if let Some((_, existing)) = graph
            .edges
            .iter_mut()
            .find(|(_, old)| old.definition == edge.definition)
        {
            *existing = edge.clone();
        }
    }
    graph.order = order;
    graph.next_generation = next_generation;
    graph.next_edge = next_edge;
    graph.next_binding_generation = next_binding;
    graph.next_resource_generation = next_resource;
    let retired_resources = std::mem::replace(
        &mut graph.resource_handles,
        std::mem::take(&mut prepared.resources),
    );
    let (retired_constructors, retained_constructors): (BTreeMap<_, _>, BTreeMap<_, _>) =
        std::mem::take(&mut graph.resource_constructors)
            .into_iter()
            .partition(|(id, _)| plan.remove_resources.contains(id) || plan.resources.contains(id));
    graph.resource_constructors = retained_constructors;
    graph.snapshot.resource_dependencies = plan.desired.resource_dependencies.clone();
    let retired_factories = std::mem::replace(
        &mut graph.factories,
        std::mem::take(&mut prepared.factories),
    );
    graph.snapshot.revision = plan.desired.revision;
    graph.snapshot.requirements = plan.desired.requirements.clone();
    graph.snapshot.recovery_requirements = plan.desired.recovery_requirements.clone();
    graph.snapshot.nodes = prepared.nodes.clone().into();
    graph.snapshot.edges = prepared.edges.clone().into();
    graph.snapshot.unbound_relationships = plan.desired.boundary_relationships.clone().into();
    graph.snapshot.control_connections = plan
        .desired
        .control_connections
        .iter()
        .filter(|(from, to)| graph.ids.contains_key(from) && graph.ids.contains_key(to))
        .cloned()
        .collect::<Vec<_>>()
        .into();
    graph.snapshot.subscriptions = plan
        .desired
        .subscriptions
        .iter()
        .filter(|(_, to)| graph.ids.contains_key(to))
        .cloned()
        .collect::<Vec<_>>()
        .into();
    graph.snapshot.readiness_required = plan
        .desired
        .readiness_required
        .iter()
        .filter(|id| graph.ids.contains_key(*id))
        .cloned()
        .collect();
    graph.snapshot.component_resources = plan
        .desired
        .component_resources
        .iter()
        .filter(|(id, _)| graph.ids.contains_key(*id))
        .map(|(id, resources)| (id.clone(), resources.clone()))
        .collect();
    graph.snapshot.component_plugins = plan
        .desired
        .component_plugins
        .iter()
        .filter(|(id, _)| graph.ids.contains_key(*id) && !plan.replace.contains(*id))
        .map(|(id, plugin)| (id.clone(), plugin.clone()))
        .collect();
    graph.snapshot.resources = plan
        .desired
        .resources
        .iter()
        .map(|spec| (spec.id.clone(), spec.clone()))
        .collect();
    graph.observation_resources = plan.observation_resources.clone();
    graph.snapshot.resource_configurations = plan
        .desired
        .resource_configurations
        .iter()
        .filter(|(id, _)| {
            graph.snapshot.resources.contains_key(*id)
                && (!plan.resources.contains(*id)
                    || plan
                        .changes
                        .iter()
                        .any(|change| matches!(change, DesiredMutation::SetTopology(_))))
        })
        .map(|(id, configuration)| (id.clone(), configuration.clone()))
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
    peer_commit.commit();
    for index in retired_handlers {
        operations.forget_control(index);
    }
    drop(leases);
    graph.desired.send_replace(Arc::new(graph.snapshot.clone()));
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
                    waiting_for: Vec::new(),
                    exhausted: false,
                    started: false,
                    lifecycle_requested: false,
                    transition_time: Utc::now(),
                },
            );
        }
        for id in state.components.keys().cloned().collect::<Vec<_>>() {
            let component = state.components.get_mut(&id).expect("observed component");
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
        let retained: BTreeSet<_> = plan
            .desired
            .relationships
            .iter()
            .chain(&plan.desired.boundary_relationships)
            .map(|relationship| &relationship.definition)
            .collect();
        let removed: Vec<_> = state
            .relationships
            .keys()
            .filter(|edge| !retained.contains(edge))
            .cloned()
            .collect();
        for edge in removed {
            state.relationships.remove(&edge);
        }
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
        for id in &plan.remove_resources {
            state.resources.remove(id);
        }
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
    let mut failures = Vec::new();
    let identity = super::super::execution::GraphIdentity {
        graph: graph.snapshot.id.clone(),
        run_epoch: graph.observed().run_epoch,
        phase: FailurePhase::Removal,
    };
    operations.retiring.extend(required);
    let retirement_targets: BTreeMap<_, _> = operations
        .retiring
        .iter()
        .filter_map(|index| {
            let slot = &graph.components[*index];
            graph
                .ids
                .get(&slot.id)
                .filter(|current| *current == index)?;
            Some((slot.id.clone(), slot.clone()))
        })
        .collect();
    let (abort, _registration) = AbortHandle::new_pair();
    let joined = graph
        .tasks
        .retirements
        .spawn(identity.clone(), abort, async move {
            identity
                .run(async {
                    let mut errors = Vec::new();
                    for (slot, epoch, instance) in retired_instances {
                        let target = retirement_targets.get(&slot.id);
                        let component = super::super::execution::Identity {
                            slot,
                            epoch,
                            phase: FailurePhase::Removal,
                            action: "destroy retired component",
                        };
                        if let Err(error) = component.call(|| {
                            drop(instance);
                            Ok(())
                        }) {
                            let cause = Arc::new(error);
                            if let Some(target) = target {
                                target.quarantine(GraphError::Reported {
                                    cause: cause.clone(),
                                });
                            }
                            errors.push(GraphError::Reported { cause });
                        }
                    }
                    for control in cancelled_controls {
                        if let Err(error) = identity.call(|| {
                            control.cancel();
                            drop(control);
                            Ok(())
                        }) {
                            let cause = Arc::new(error);
                            for target in retirement_targets.values() {
                                target.quarantine(GraphError::Reported {
                                    cause: cause.clone(),
                                });
                            }
                            errors.push(GraphError::Reported { cause });
                        }
                    }
                    macro_rules! retire {
                        ($values:expr) => {
                            for value in $values {
                                if let Err(error) = identity.call(|| {
                                    drop(value);
                                    Ok(())
                                }) {
                                    let cause = Arc::new(error);
                                    for target in retirement_targets.values() {
                                        target.quarantine(GraphError::Reported {
                                            cause: cause.clone(),
                                        });
                                    }
                                    errors.push(GraphError::Reported { cause });
                                }
                            }
                        };
                    }
                    retire!(retired_providers);
                    retire!(retired_resources);
                    retire!(retired_factories);
                    retire!(retired_constructors);
                    retire!(retired_outputs);
                    retire!(retired_inputs);
                    if errors.is_empty() {
                        Ok(())
                    } else {
                        Err(GraphError::Cleanup {
                            primary: None,
                            errors,
                        })
                    }
                })
                .await
        });
    match joined {
        Ok(joined) => operations.retirements.push(
            async move {
                RetirementCompletion {
                    slot: None,
                    result: joined.await.and_then(|result| result),
                }
            }
            .boxed(),
        ),
        Err(error) => {
            operations.retiring.clear();
            failures.push(failure(error, FailurePhase::Removal));
        }
    }
    Ok(failures)
}

async fn realize_component(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    index: usize,
    operation: Operation,
    commands: &mut CommandReceiver,
) -> GraphResult<CreationOutcome> {
    let id = graph.nodes[index].descriptor.id().clone();
    let waiting = creation_dependencies(graph, index);
    if !waiting.is_empty() {
        return Ok(operations.defer_creation(graph, index, operation, waiting));
    }
    operations.attach_control(graph, index)?;
    operations.launch(graph, index, operation)?;
    let mut peers = graph.peers.subscribe();
    while operations.active.contains_key(&index)
        || operations.stop_after_abort.contains(&index)
        || operations
            .stopping
            .iter()
            .any(|group| group.pending.contains(&index))
        || operations
            .draining
            .iter()
            .any(|group| group.selected.contains(&index))
    {
        tokio::task::consume_budget().await;
        operations.advance_drains(graph, controls)?;
        operations.advance_additions(graph)?;
        operations.advance_start(graph)?;
        operations.advance_stop(graph)?;
        begin_handle_requests(graph, operations, controls, cancel).await?;
        tokio::select! {
            biased;
            _ = cancelled(cancel) => return Err(GraphError::Cancelled),
            completion = operations.drain_futures.next(), if !operations.drain_futures.is_empty() => {
                if let Some(completion) = completion { operations.complete_drain(graph, completion); }
            }
            completion = operations.control_futures.next(), if !operations.control_futures.is_empty() => {
                if let Some(completion) = completion { operations.complete_control(graph, completion); }
            }
            changed = peers.changed() => { changed.map_err(|_| GraphError::ControllerClosed)?; }
            completion = operations.futures.next(), if !operations.futures.is_empty() => {
                if let Some(completion) = completion { operations.complete(graph, completion, controls)?; }
            }
            command = commands.recv() => {
                handle_command(graph, operations, controls, cancel,
                    command.ok_or(GraphError::ControllerClosed)?, commands).await?;
            }
        }
    }
    let observed = graph.observed();
    let node = &observed.components[&id];
    Ok(if let Some(failure) = &node.failure {
        CreationOutcome::CreationFailed(failure.clone())
    } else if !matches!(
        graph.components[index].take()?.component,
        Component::Deferred { .. } | Component::Unresolved(_)
    ) {
        update(graph, |state| {
            state
                .components
                .get_mut(&id)
                .expect("constructed component")
                .realization = RealizationState::Created;
        });
        CreationOutcome::Created
    } else {
        CreationOutcome::Blocked {
            dependencies: Vec::new(),
            resources: Vec::new(),
        }
    })
}

async fn realize(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    plan: &ReconciliationPreview,
    report: &mut ReconciliationReport,
    commands: &mut CommandReceiver,
) -> GraphResult<()> {
    for index in graph.order.clone() {
        let id = graph.nodes[index].descriptor.id();
        if !plan.create.contains(id) && !plan.replace.contains(id) && !plan.update.contains(id) {
            continue;
        }
        let operation = if plan.update.contains(id) {
            Operation::Reconfigure
        } else {
            Operation::Create
        };
        let id = id.clone();
        let outcome = realize_component(
            graph, operations, controls, cancel, index, operation, commands,
        )
        .await?;
        report.creation.insert(id, outcome);
    }
    for (index, edge) in graph.edges.clone() {
        if !plan.rebind.contains(&edge.definition) {
            continue;
        }
        if edge
            .resources
            .keys()
            .any(|id| !graph.resource_handles.contains_key(id))
        {
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
            Component::Deferred { .. } | Component::Unresolved(_)
        ) || matches!(
            graph.components[to].take()?.component,
            Component::Deferred { .. } | Component::Unresolved(_)
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
        let constructed = construct_pipe(graph, index, &edge)?;
        let result = drive_reconciliation(
            graph,
            operations,
            controls,
            cancel,
            commands,
            graph.cleanup_timeout,
            constructed,
        )
        .await
        .and_then(|provided| install_pipe(graph, controls, index, &edge, provided));
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
        if observed.failure.as_ref().is_some_and(|error| {
            matches!(
                error.phase,
                FailurePhase::Creation | FailurePhase::Validation
            )
        }) {
            continue;
        }
        if operations.active.contains_key(&index) {
            continue;
        }
        let lease = graph.components[index].take()?;
        if !matches!(
            lease.component,
            Component::Deferred { .. } | Component::Unresolved(_)
        ) && binding_dependencies(graph, id).is_empty()
        {
            update(graph, |state| {
                let component = state.components.get_mut(id).expect("component");
                component.realization = RealizationState::Created;
                component.waiting_for.clear();
            });
        }
    }
    Ok(())
}
