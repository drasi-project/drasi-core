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

use std::{
    collections::{BTreeMap, BTreeSet},
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
};

use chrono::Utc;
use futures::{
    future::{AbortHandle, Abortable, Aborted, BoxFuture},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use tokio::sync::{mpsc, oneshot, watch};
use tracing::Instrument;

#[path = "reconcile.rs"]
pub(super) mod reconcile;

use super::specification::{ConstructionContext, ResourceOwnership};
use super::{
    cancelled, check_descriptor, component_error, run_node, topology, Component, ComputationGraph,
    GraphControl, GraphError, GraphResult, GraphSnapshot, GraphState, Incoming, NodeSnapshot,
    Outgoing, PipeGuard,
};
use crate::computation::v1::{
    ActivationCoupling, BindingState, ComponentFailure, ComponentGeneration, ComponentHealth,
    ComponentId, ComponentLifecycle, ComponentRole, CreationOutcome, DataAvailability,
    DeploymentReport, FailureDisposition, FailurePhase, GraphRevision, GraphSelection,
    HealthObservation, LifecyclePolicy, ObservedComponent, ObservedGraph, ObservedRelationship,
    ObservedResource, OperationEpoch, OperationSummary, PortId, RealizationState,
    ResourceRealization, StartOutcome, StartReport, StopOutcome, StopReport,
};

pub(super) struct InstanceSlot {
    id: ComponentId,
    generation: ComponentGeneration,
    value: Mutex<Option<Instance>>,
}

struct Instance {
    component: Component,
    sequences: BTreeMap<PortId, u64>,
    inputs: Vec<Incoming>,
    outputs: Vec<Outgoing>,
    attempted: bool,
}

impl InstanceSlot {
    pub(super) fn new(component: Component, generation: ComponentGeneration) -> Arc<Self> {
        let id = component.descriptor().id().clone();
        Self::declared(id, component, generation)
    }

    pub(super) fn declared(
        id: ComponentId,
        component: Component,
        generation: ComponentGeneration,
    ) -> Arc<Self> {
        Arc::new(Self {
            id,
            generation,
            value: Mutex::new(Some(Instance {
                component,
                sequences: BTreeMap::new(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                attempted: false,
            })),
        })
    }

    fn take(self: &Arc<Self>) -> GraphResult<InstanceLease> {
        let value = self
            .value
            .lock()
            .map_err(|error| topology(format!("instance {} ownership poisoned: {error}", self.id)))?
            .take()
            .ok_or(GraphError::OperationInProgress)?;
        Ok(InstanceLease {
            slot: self.clone(),
            value: Some(value),
        })
    }
}

struct InstanceLease {
    slot: Arc<InstanceSlot>,
    value: Option<Instance>,
}

impl Deref for InstanceLease {
    type Target = Instance;

    fn deref(&self) -> &Self::Target {
        self.value
            .as_ref()
            .expect("an active instance lease owns its value")
    }
}

impl DerefMut for InstanceLease {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value
            .as_mut()
            .expect("an active instance lease owns its value")
    }
}

impl Drop for InstanceLease {
    fn drop(&mut self) {
        let mut slot = match self.slot.value.lock() {
            Ok(slot) => slot,
            Err(error) => {
                log::error!("Retaining instance in a poisoned ownership slot: {error}");
                error.into_inner()
            }
        };
        *slot = self.value.take();
    }
}

pub(super) enum Command {
    AutoStart,
    Add {
        addition: super::ComponentAddition,
        reply: oneshot::Sender<GraphResult<(ComponentId, ComponentGeneration)>>,
    },
    ControlConnections {
        connections: Vec<(ComponentId, ComponentId)>,
        subscriptions: bool,
        reply: oneshot::Sender<GraphResult<()>>,
    },
    ControlHandler {
        component: ComponentId,
        generation: ComponentGeneration,
        handler: Arc<dyn crate::computation::v1::ControlHandler>,
        reply: oneshot::Sender<GraphResult<()>>,
    },
    HandleStart {
        component: ComponentId,
        generation: ComponentGeneration,
        reply: oneshot::Sender<GraphResult<StartReport>>,
    },
    HandleStop {
        component: ComponentId,
        generation: ComponentGeneration,
        reply: oneshot::Sender<GraphResult<StopReport>>,
    },
    ReadinessPolicy {
        component: ComponentId,
        generation: ComponentGeneration,
        required: bool,
        reply: oneshot::Sender<GraphResult<()>>,
    },
    ObserveResources {
        component: ComponentId,
        generation: ComponentGeneration,
        bindings: Vec<(super::ResourceSpecification, super::ResourceHandle)>,
    },
    ObservePlugin {
        component: ComponentId,
        generation: ComponentGeneration,
        plugin: super::PluginIdentity,
    },
    BindStream {
        component: ComponentId,
        generation: ComponentGeneration,
        port: PortId,
        stream: crate::computation::v1::StreamId,
        reply: oneshot::Sender<GraphResult<()>>,
    },
    Preview {
        revision: GraphRevision,
        changes: Vec<reconcile::DesiredMutation>,
        reply: oneshot::Sender<GraphResult<reconcile::ReconciliationPreview>>,
    },
    Reconcile {
        preview: reconcile::ReconciliationPreview,
        bindings: super::TopologyBindings,
        reply: oneshot::Sender<GraphResult<reconcile::ReconciliationReport>>,
    },
    Start {
        revision: GraphRevision,
        selection: GraphSelection,
        force: bool,
        reply: oneshot::Sender<GraphResult<StartReport>>,
    },
    Stop {
        revision: GraphRevision,
        selection: GraphSelection,
        reply: oneshot::Sender<GraphResult<StopReport>>,
    },
    Quiesce {
        revision: GraphRevision,
        selection: GraphSelection,
        reply: oneshot::Sender<GraphResult<Vec<ComponentId>>>,
    },
    Policy {
        revision: GraphRevision,
        component: ComponentId,
        policy: LifecyclePolicy,
        reply: oneshot::Sender<GraphResult<GraphRevision>>,
    },
    Health {
        revision: GraphRevision,
        observation: HealthObservation,
        reply: oneshot::Sender<GraphResult<()>>,
    },
}

impl GraphControl {
    /// Park selected processing at safe boundaries without stopping instances.
    /// Incoming work from selected producers drains first; other input queues
    /// remain owned by the parked component. No external-effect drain is implied.
    pub async fn quiesce_components(
        &self,
        revision: GraphRevision,
        selection: GraphSelection,
    ) -> GraphResult<Vec<ComponentId>> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Quiesce {
                revision,
                selection,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub fn observed(&self) -> Arc<ObservedGraph> {
        self.observed.borrow().clone()
    }

    pub fn subscribe_observed(&self) -> watch::Receiver<Arc<ObservedGraph>> {
        self.observed.clone()
    }

    pub fn desired_snapshot(&self) -> Arc<GraphSnapshot> {
        self.desired.borrow().clone()
    }

    pub async fn deployment_report(&self) -> GraphResult<DeploymentReport> {
        let mut observed = self.observed.clone();
        let snapshot = tokio::select! {
            biased;
            result = observed.wait_for(|value| value.deployment.is_some()) => result.map_err(|_| GraphError::ControllerClosed)?,
            _ = self.commands.closed() => return Err(GraphError::ControllerClosed),
        };
        snapshot
            .deployment
            .clone()
            .ok_or(GraphError::ControllerClosed)
    }

    pub async fn startup_report(&self) -> GraphResult<StartReport> {
        let mut observed = self.observed.clone();
        let snapshot = tokio::select! {
            biased;
            result = observed.wait_for(|value| value.startup.is_some()) => result.map_err(|_| GraphError::ControllerClosed)?,
            _ = self.commands.closed() => return Err(GraphError::ControllerClosed),
        };
        snapshot.startup.clone().ok_or(GraphError::ControllerClosed)
    }

    pub async fn start_components(
        &self,
        revision: GraphRevision,
        selection: GraphSelection,
    ) -> GraphResult<StartReport> {
        self.request_start(revision, selection, false).await
    }

    pub(crate) async fn start_requested(
        &self,
        revision: GraphRevision,
        selection: GraphSelection,
    ) -> GraphResult<StartReport> {
        self.request_start(revision, selection, true).await
    }

    async fn request_start(
        &self,
        revision: GraphRevision,
        selection: GraphSelection,
        force: bool,
    ) -> GraphResult<StartReport> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Start {
                revision,
                selection,
                force,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub async fn stop_components(
        &self,
        revision: GraphRevision,
        selection: GraphSelection,
    ) -> GraphResult<StopReport> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Stop {
                revision,
                selection,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub async fn set_lifecycle_policy(
        &self,
        revision: GraphRevision,
        component: ComponentId,
        policy: LifecyclePolicy,
    ) -> GraphResult<GraphRevision> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Policy {
                revision,
                component,
                policy,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub async fn report_health(
        &self,
        revision: GraphRevision,
        observation: HealthObservation,
    ) -> GraphResult<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Health {
                revision,
                observation,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }
}

pub(super) fn initial_observations(snapshot: &GraphSnapshot) -> ObservedGraph {
    let now = Utc::now();
    ObservedGraph {
        revision: snapshot.revision,
        run_epoch: 0,
        components: snapshot
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| {
                (
                    node.descriptor.id().clone(),
                    ObservedComponent {
                        generation: ComponentGeneration(index as u64 + 1),
                        operation: OperationEpoch(0),
                        revision: snapshot.revision,
                        realization: RealizationState::Pending,
                        lifecycle: ComponentLifecycle::Stopped,
                        health: ComponentHealth::Unknown,
                        failure: None,
                        exhausted: false,
                        started: false,
                        lifecycle_requested: false,
                        transition_time: now,
                    },
                )
            })
            .collect(),
        relationships: snapshot
            .edges
            .iter()
            .map(|edge| &edge.definition)
            .chain(
                snapshot
                    .unbound_relationships
                    .iter()
                    .map(|edge| &edge.definition),
            )
            .map(|edge| {
                (
                    edge.clone(),
                    ObservedRelationship {
                        binding: BindingState::Declared,
                        availability: DataAvailability::Unknown,
                        generation: 0,
                        revision: snapshot.revision,
                        failure: None,
                        transition_time: now,
                    },
                )
            })
            .collect(),
        deployment: None,
        startup: None,
        resources: snapshot
            .resources
            .keys()
            .map(|id| {
                (
                    id.clone(),
                    ObservedResource {
                        realization: ResourceRealization::Pending,
                        revision: snapshot.revision,
                        generation: 1,
                        transition_time: now,
                        failure: None,
                    },
                )
            })
            .collect(),
    }
}

pub(super) fn update(graph: &ComputationGraph, change: impl FnOnce(&mut ObservedGraph)) {
    graph
        .observed
        .send_modify(|snapshot| change(Arc::make_mut(snapshot)));
    super::registry::publish(graph);
    graph.inspector.publish(&graph.snapshot, graph.observed());
}

pub(super) fn mark_cleanup_required(observed: &watch::Sender<Arc<ObservedGraph>>) {
    observed.send_modify(|snapshot| {
        let snapshot = Arc::make_mut(snapshot);
        for node in snapshot.components.values_mut() {
            if matches!(
                node.lifecycle,
                ComponentLifecycle::Starting
                    | ComponentLifecycle::Running
                    | ComponentLifecycle::Quiescing
                    | ComponentLifecycle::Quiesced
            ) {
                node.lifecycle = ComponentLifecycle::Stopping;
                node.transition_time = Utc::now();
            }
        }
        for edge in snapshot.relationships.values_mut() {
            if edge.binding == BindingState::Bound {
                edge.binding = BindingState::Draining;
                edge.availability = DataAvailability::Unavailable;
                edge.transition_time = Utc::now();
            }
        }
    });
}

fn failure(error: GraphError, phase: FailurePhase) -> ComponentFailure {
    ComponentFailure {
        phase,
        disposition: if let GraphError::Creation { disposition, .. } = error.underlying() {
            *disposition
        } else if matches!(
            error.underlying(),
            GraphError::Contract(_)
                | GraphError::Emission { .. }
                | GraphError::Topology { .. }
                | GraphError::Validation { .. }
        ) {
            FailureDisposition::Terminal
        } else {
            FailureDisposition::Retryable
        },
        cause: Arc::new(error),
        timestamp: Utc::now(),
    }
}

fn check_revision(graph: &ComputationGraph, revision: GraphRevision) -> GraphResult<()> {
    if revision == graph.snapshot.revision {
        Ok(())
    } else {
        Err(GraphError::StaleRevision {
            expected: revision,
            actual: graph.snapshot.revision,
        })
    }
}

fn select(graph: &ComputationGraph, selection: &GraphSelection) -> GraphResult<BTreeSet<usize>> {
    super::topology::selected(&graph.snapshot, selection)?
        .into_iter()
        .map(|id| {
            graph
                .ids
                .get(&id)
                .copied()
                .ok_or_else(|| topology(format!("unknown component {id}")))
        })
        .collect()
}

fn binding_dependencies(graph: &ComputationGraph, id: &ComponentId) -> Vec<ComponentId> {
    let state = graph.observed();
    let node = &graph.nodes[graph.ids[id]];
    let mut missing = BTreeSet::new();
    for port in node.descriptor.ports() {
        let endpoint = super::Endpoint::new(id.clone(), port.id().clone());
        let bound = graph.snapshot.edges.iter().any(|edge| {
            (edge.definition.from == endpoint || edge.definition.to == endpoint)
                && state.relationships[&edge.definition].binding == BindingState::Bound
        });
        let optional_input =
            port.direction() == super::super::PortDirection::Input
                && graph.snapshot.unbound_relationships.iter().any(|edge| {
                    edge.definition.to == endpoint && !edge.policy.required_for_binding
                });
        if !bound && !optional_input {
            missing.insert(id.clone());
        }
        if port.direction() == super::super::PortDirection::Output
            && !node.output_streams.contains_key(port.id())
        {
            missing.insert(id.clone());
        }
    }
    for edge in graph.snapshot.edges.iter() {
        if edge.policy.required_for_binding
            && state.relationships[&edge.definition].binding != BindingState::Bound
        {
            if &edge.definition.from.component == id {
                missing.insert(edge.definition.to.component.clone());
            }
            if &edge.definition.to.component == id {
                missing.insert(edge.definition.from.component.clone());
            }
        }
    }
    for edge in graph
        .snapshot
        .unbound_relationships
        .iter()
        .filter(|edge| edge.policy.required_for_binding)
    {
        if &edge.definition.from.component == id {
            missing.insert(edge.definition.to.component.clone());
        }
        if &edge.definition.to.component == id {
            missing.insert(edge.definition.from.component.clone());
        }
    }
    missing.into_iter().collect()
}

async fn deploy(
    graph: &mut ComputationGraph,
    cancel: &mut watch::Receiver<bool>,
) -> GraphResult<PipeGuard> {
    update(graph, |state| {
        for (id, resource) in &mut state.resources {
            resource.realization = if graph.resource_handles.contains_key(id) {
                ResourceRealization::Created
            } else {
                ResourceRealization::Pending
            };
            resource.transition_time = Utc::now();
        }
    });
    let mut unavailable = BTreeSet::new();
    let mut creation_failures = BTreeMap::new();
    let mut resource_blocks = BTreeMap::new();
    for index in graph.order.iter().copied() {
        let slot = graph.components[index].clone();
        let node = graph.nodes[index].clone();
        let mut lease = slot.take()?;
        check_descriptor(&lease.component, &node)?;
        lease.inputs.clear();
        lease.outputs.clear();
        let missing_creation: Vec<_> = graph
            .snapshot
            .edges
            .iter()
            .filter(|edge| {
                edge.policy.required_for_creation
                    && edge.definition.to.component == slot.id
                    && unavailable.contains(&graph.ids[&edge.definition.from.component])
            })
            .map(|edge| edge.definition.from.component.clone())
            .collect();
        if !missing_creation.is_empty() {
            unavailable.insert(index);
            continue;
        }
        if let Some(failure) = graph.observed().components[&slot.id]
            .failure
            .as_ref()
            .filter(|failure| {
                matches!(
                    failure.phase,
                    FailurePhase::Creation | FailurePhase::Validation
                )
            })
        {
            unavailable.insert(index);
            creation_failures.insert(index, failure.clone());
            continue;
        }
        if matches!(lease.component, Component::Unresolved(_)) {
            unavailable.insert(index);
            creation_failures.insert(
                index,
                failure(
                    super::addition::validation_failure(
                        &slot.id,
                        topology("component instance is unresolved"),
                    ),
                    FailurePhase::Validation,
                ),
            );
            continue;
        }
        if let Component::Deferred {
            specification,
            factory,
        } = &lease.component
        {
            let specification = specification.clone();
            let factory = factory.clone();
            let missing_resources: BTreeSet<_> = specification
                .dependencies
                .values()
                .flatten()
                .chain(specification.configuration.values().filter_map(|value| {
                    if let super::specification::ConfigurationValue::Reference {
                        resource, ..
                    } = value
                    {
                        Some(resource)
                    } else {
                        None
                    }
                }))
                .filter(|id| !graph.resource_handles.contains_key(*id))
                .cloned()
                .collect();
            if !missing_resources.is_empty() {
                unavailable.insert(index);
                resource_blocks.insert(index, missing_resources.into_iter().collect::<Vec<_>>());
                continue;
            }
            update(graph, |state| {
                let observed = state.components.get_mut(&slot.id).expect("component");
                observed.realization = RealizationState::Creating;
                observed.transition_time = Utc::now();
            });
            let construction = async {
                let context = ConstructionContext::resolve(
                    graph.execution_scope.clone(),
                    graph.snapshot.id.clone(),
                    slot.generation,
                    specification,
                    graph.resource_handles.clone(),
                    &factory.descriptor().configuration,
                )
                .await?;
                factory.create(context).await
            };
            let result = tokio::select! {
                biased;
                _ = cancelled(cancel) => return Err(GraphError::Cancelled),
                result = construction => result,
            };
            match result {
                Ok(constructed) => {
                    lease.component = constructed.0;
                    let invalid = if lease.component.role() != node.role {
                        Err(topology(
                            "constructed component role differs from its factory specification",
                        ))
                    } else {
                        check_descriptor(&lease.component, &node)
                    };
                    if let Err(error) = invalid {
                        lease.attempted = true;
                        let failure = failure(error, FailurePhase::Creation);
                        unavailable.insert(index);
                        creation_failures.insert(index, failure);
                    }
                }
                Err(error) => {
                    let failure = failure(
                        GraphError::Creation {
                            component: slot.id.clone(),
                            disposition: error.disposition,
                            source: error.source,
                        },
                        FailurePhase::Creation,
                    );
                    unavailable.insert(index);
                    creation_failures.insert(index, failure);
                }
            }
        }
    }
    let mut controls = PipeGuard(BTreeMap::new());
    let unconstructed = unavailable.clone();
    for (&edge_index, edge) in &graph.edges {
        let provider = &graph.providers[&edge_index];
        let from = graph.ids[&edge.definition.from.component];
        let to = graph.ids[&edge.definition.to.component];
        if unconstructed.contains(&from) || unconstructed.contains(&to) {
            update(graph, |state| {
                let observed = state
                    .relationships
                    .get_mut(&edge.definition)
                    .expect("relationship");
                observed.binding = BindingState::Declared;
                observed.availability = DataAvailability::Unavailable;
            });
            if edge.policy.required_for_binding {
                unavailable.insert(to);
                unavailable.insert(from);
            }
            continue;
        }
        let binding_generation = graph.next_binding_generation;
        graph.next_binding_generation = binding_generation
            .checked_add(1)
            .ok_or_else(|| topology("binding generation exhausted"))?;
        update(graph, |state| {
            let observed = state
                .relationships
                .get_mut(&edge.definition)
                .expect("declared relationship");
            observed.binding = BindingState::Binding;
            observed.generation = binding_generation;
            observed.failure = None;
            observed.transition_time = Utc::now();
        });
        let provided = (|| {
            let mut provided = provider
                .create_with_resources(&graph.resource_handles)
                .map_err(|source| GraphError::Pipe {
                    edge: edge_index,
                    source,
                })?;
            controls.0.insert(edge_index, provided.control.clone());
            if provided.pipe.capabilities() != &edge.capabilities {
                return Err(topology(
                    "created pipe differs from preflight capability declaration",
                ));
            }
            let receiver = provided
                .pipe
                .take_receiver()
                .map_err(|source| GraphError::Pipe {
                    edge: edge_index,
                    source,
                })?;
            let from = graph.ids[&edge.definition.from.component];
            let to = graph.ids[&edge.definition.to.component];
            let progress = Arc::new(super::FlowProgress::default());
            graph.components[from].take()?.outputs.push(Outgoing {
                edge: edge_index,
                port: edge.definition.from.port.clone(),
                sender: provided.pipe.sender(),
                progress: progress.clone(),
            });
            graph.components[to].take()?.inputs.push(Incoming {
                edge: edge_index,
                port: edge.definition.to.port.clone(),
                receiver,
                acknowledgement_required: edge
                    .capabilities
                    .supported()
                    .contains(&crate::computation::v1::PipeCapability::ExplicitAcknowledgement),
                pending: None,
                exhausted: false,
                progress,
            });
            Ok(())
        })();
        match provided {
            Ok(()) => update(graph, |state| {
                let observed = state
                    .relationships
                    .get_mut(&edge.definition)
                    .expect("relationship");
                observed.binding = BindingState::Bound;
                observed.availability = DataAvailability::Idle;
            }),
            Err(error) => {
                if let Some(control) = controls.0.get(&edge_index) {
                    control.cancel();
                }
                if edge.policy.required_for_binding {
                    unavailable.insert(graph.ids[&edge.definition.from.component]);
                    unavailable.insert(graph.ids[&edge.definition.to.component]);
                }
                let failure = failure(error, FailurePhase::Binding);
                update(graph, |state| {
                    let observed = state
                        .relationships
                        .get_mut(&edge.definition)
                        .expect("relationship");
                    observed.binding = BindingState::Failed;
                    observed.availability = DataAvailability::Unavailable;
                    observed.failure = Some(failure);
                });
            }
        }
    }
    loop {
        let before = unavailable.len();
        for edge in graph.snapshot.edges.iter() {
            if edge.policy.required_for_creation
                && unavailable.contains(&graph.ids[&edge.definition.from.component])
            {
                unavailable.insert(graph.ids[&edge.definition.to.component]);
            }
        }
        if unavailable.len() == before {
            break;
        }
    }
    let mut outcomes = BTreeMap::new();
    for &index in &graph.order {
        let node = &graph.nodes[index];
        let id = node.descriptor.id();
        let blocked = unavailable.contains(&index);
        let creation_failure = creation_failures.get(&index);
        update(graph, |state| {
            let observed = state.components.get_mut(id).expect("component");
            observed.realization = if creation_failure.is_some() {
                RealizationState::CreationFailed
            } else if blocked {
                RealizationState::Blocked
            } else {
                RealizationState::Created
            };
            observed.lifecycle = ComponentLifecycle::Stopped;
            observed.health = ComponentHealth::Unknown;
            observed.failure = creation_failure.cloned();
            observed.exhausted = false;
            observed.transition_time = Utc::now();
        });
        outcomes.insert(
            id.clone(),
            if let Some(failure) = creation_failure {
                CreationOutcome::CreationFailed(failure.clone())
            } else if let Some(resources) = resource_blocks.get(&index) {
                CreationOutcome::Blocked {
                    dependencies: Vec::new(),
                    resources: resources.clone(),
                }
            } else if blocked {
                CreationOutcome::Blocked {
                    dependencies: graph
                        .snapshot
                        .edges
                        .iter()
                        .filter_map(|edge| {
                            if edge.policy.required_for_creation
                                && edge.definition.to.component == *id
                                && unavailable.contains(&graph.ids[&edge.definition.from.component])
                            {
                                return Some(edge.definition.from.component.clone());
                            }
                            if !edge.policy.required_for_binding
                                || graph.observed().relationships[&edge.definition].binding
                                    == BindingState::Bound
                            {
                                return None;
                            }
                            if edge.definition.from.component == *id {
                                Some(edge.definition.to.component.clone())
                            } else if edge.definition.to.component == *id {
                                Some(edge.definition.from.component.clone())
                            } else {
                                None
                            }
                        })
                        .collect(),
                    resources: Vec::new(),
                }
            } else {
                CreationOutcome::Created
            },
        );
    }
    let report = DeploymentReport {
        revision: graph.snapshot.revision,
        summary: if unavailable.is_empty() {
            OperationSummary::Completed
        } else {
            OperationSummary::CompletedWithFailures
        },
        components: outcomes,
        resources: graph
            .observed()
            .resources
            .iter()
            .map(|(id, state)| (id.clone(), state.realization))
            .collect(),
    };
    update(graph, |state| state.deployment = Some(report));
    Ok(controls)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Operation {
    Create,
    Start,
    Process,
    Stop,
}

struct Active {
    epoch: OperationEpoch,
    operation: Operation,
    abort: AbortHandle,
    quiesce: watch::Sender<bool>,
}

struct Completion {
    index: usize,
    generation: ComponentGeneration,
    epoch: OperationEpoch,
    operation: Operation,
    result: Result<GraphResult<()>, Aborted>,
}

struct Starting {
    revision: GraphRevision,
    pending: BTreeSet<usize>,
    outcomes: BTreeMap<ComponentId, StartOutcome>,
    reply: Option<oneshot::Sender<GraphResult<StartReport>>>,
    force: bool,
}

struct Stopping {
    revision: GraphRevision,
    pending: BTreeSet<usize>,
    outcomes: BTreeMap<ComponentId, StopOutcome>,
    reply: oneshot::Sender<GraphResult<StopReport>>,
}

struct ControlCompletion {
    component: ComponentId,
    generation: ComponentGeneration,
    result: Result<anyhow::Result<()>, Aborted>,
}

#[derive(Default)]
struct Operations {
    futures: FuturesUnordered<BoxFuture<'static, Completion>>,
    active: BTreeMap<usize, Active>,
    starting: Option<Starting>,
    stopping: Option<Stopping>,
    stop_after_abort: BTreeSet<usize>,
    propagated_stop: BTreeSet<usize>,
    exhausted: BTreeSet<usize>,
    last_failure: Option<Arc<GraphError>>,
    quiescing: BTreeSet<usize>,
    paused: BTreeSet<usize>,
    invalidated_bindings: BTreeSet<usize>,
    control_futures: FuturesUnordered<BoxFuture<'static, ControlCompletion>>,
    control_handlers:
        BTreeMap<usize, watch::Sender<Option<Arc<dyn crate::computation::v1::ControlHandler>>>>,
    peer_controls: BTreeMap<usize, crate::computation::v1::ComponentControl>,
    control_aborts: BTreeMap<usize, AbortHandle>,
    pending_auto_start: BTreeMap<usize, bool>,
    connection_generations:
        BTreeMap<(ComponentId, ComponentId), BTreeMap<super::EdgeDefinition, u64>>,
}

impl Operations {
    fn attach_control(&mut self, graph: &ComputationGraph, index: usize) -> GraphResult<()> {
        let slot = &graph.components[index];
        let mut instance = slot.take()?;
        let valid = instance.component.descriptor() == &graph.nodes[index].descriptor
            && instance.component.role() == graph.nodes[index].role;
        if let Some(control) = self.peer_controls.get(&index) {
            if control.generation() == slot.generation {
                if valid {
                    instance.component.bind_control(control.clone());
                }
                if let Some(handler) = valid
                    .then(|| instance.component.control_handler())
                    .flatten()
                {
                    if let Some(handlers) = self.control_handlers.get(&index) {
                        if handlers.borrow().is_none() {
                            handlers.send_replace(Some(handler));
                        }
                    }
                }
                return Ok(());
            }
        }
        let (control, mut inbox) = graph.peers.attach(slot.id.clone(), slot.generation)?;
        if let Some(previous) = self.control_aborts.remove(&index) {
            previous.abort();
        }
        if valid {
            instance.component.bind_control(control.clone());
        }
        let (handlers, handler) = watch::channel(
            valid
                .then(|| instance.component.control_handler())
                .flatten(),
        );
        self.control_handlers.insert(index, handlers);
        self.peer_controls.insert(index, control.clone());
        let plane = graph.peers.clone();
        let id = slot.id.clone();
        let generation = slot.generation;
        let (abort, registration) = AbortHandle::new_pair();
        self.control_aborts.insert(index, abort);
        self.control_futures.push(
            async move {
                let result = Abortable::new(
                    async {
                        while let Some(message) = inbox.recv().await {
                            let mut changes = plane.subscribe();
                            if let Err(error) = plane.validate_delivery(&message) {
                                log::debug!(
                                    "Discarding obsolete peer notification for {id}: {error}"
                                );
                                continue;
                            }
                            let callback = handler.borrow().clone();
                            if let Some(callback) = callback {
                                let pending = callback.on_message(message.clone(), control.clone());
                                tokio::pin!(pending);
                                loop {
                                    tokio::select! {
                                        biased;
                                        changed = changes.changed() => {
                                            if changed.is_err() || plane.validate_delivery(&message).is_err() {
                                                log::debug!("Cancelling a retired peer callback for {id}");
                                                break;
                                            }
                                        }
                                        result = &mut pending => { result?; break; }
                                    }
                                }
                            } else if matches!(
                                message.notification,
                                crate::computation::v1::ControlNotification::Custom { .. }
                            ) {
                                anyhow::bail!(
                                    "component {id} has no handler for a custom control message"
                                );
                            }
                        }
                        Ok(())
                    },
                    registration,
                )
                .await;
                ControlCompletion {
                    component: id,
                    generation,
                    result,
                }
            }
            .boxed(),
        );
        Ok(())
    }

    fn complete_control(&mut self, graph: &ComputationGraph, completed: ControlCompletion) {
        let Some(&index) = graph.ids.get(&completed.component) else {
            return;
        };
        if graph.components[index].generation != completed.generation {
            return;
        }
        if let Ok(Err(source)) = completed.result {
            let failure = failure(
                GraphError::Component {
                    component: completed.component.clone(),
                    operation: "control notification",
                    source,
                },
                FailurePhase::Control,
            );
            update(graph, |state| {
                let node = state
                    .components
                    .get_mut(&completed.component)
                    .expect("component");
                node.health = ComponentHealth::Degraded;
                node.failure = Some(failure);
                node.transition_time = Utc::now();
            });
        }
    }

    fn retire_control(&mut self, graph: &ComputationGraph, index: usize) -> GraphResult<()> {
        let slot = &graph.components[index];
        if let Some(abort) = self.control_aborts.remove(&index) {
            abort.abort();
        }
        graph.peers.remove(&slot.id, slot.generation)?;
        self.peer_controls.remove(&index);
        self.control_handlers.remove(&index);
        self.pending_auto_start.remove(&index);
        Ok(())
    }

    fn refresh_connections(&mut self, graph: &ComputationGraph) -> GraphResult<()> {
        let observed = graph.observed();
        let mut current: BTreeMap<_, BTreeMap<_, _>> = BTreeMap::new();
        for edge in graph.snapshot.edges.iter() {
            current
                .entry((
                    edge.definition.from.component.clone(),
                    edge.definition.to.component.clone(),
                ))
                .or_default()
                .insert(
                    edge.definition.clone(),
                    observed
                        .relationships
                        .get(&edge.definition)
                        .map_or(0, |state| state.generation),
                );
        }
        for pair in graph
            .snapshot
            .control_connections
            .iter()
            .chain(graph.snapshot.subscriptions.iter())
        {
            current.entry(pair.clone()).or_default();
        }
        let rebound: Vec<_> = current
            .iter()
            .filter(|(pair, bindings)| {
                self.connection_generations
                    .get(*pair)
                    .is_some_and(|previous| previous != *bindings)
            })
            .map(|(pair, _)| pair.clone())
            .collect();
        graph
            .peers
            .sync_connections(current.keys().cloned(), rebound)?;
        self.connection_generations = current;
        Ok(())
    }

    fn advance_additions(&mut self, graph: &ComputationGraph) -> GraphResult<()> {
        let newly_ready: Vec<_> = graph
            .observed()
            .components
            .iter()
            .filter(|(id, node)| {
                !node.started
                    && node.failure.is_none()
                    && matches!(
                        node.lifecycle,
                        ComponentLifecycle::Starting | ComponentLifecycle::Running
                    )
                    && graph.ids.get(*id).is_some_and(|index| {
                        self.active
                            .get(index)
                            .is_some_and(|active| active.operation == Operation::Process)
                    })
                    && graph.peers.is_ready(id, node.generation)
            })
            .map(|(id, _)| id.clone())
            .collect();
        if !newly_ready.is_empty() {
            update(graph, |state| {
                for id in &newly_ready {
                    let node = state.components.get_mut(id).expect("ready component");
                    node.started = true;
                    node.lifecycle = ComponentLifecycle::Running;
                }
            });
            for id in newly_ready {
                self.notify_availability(
                    graph.ids[&id],
                    crate::computation::v1::ControlNotification::Available,
                );
            }
        }
        if self.starting.is_some() || self.stopping.is_some() {
            return Ok(());
        }
        let eligible = |index: &usize| {
            let id = graph.nodes[*index].descriptor.id();
            graph.ids.get(id) == Some(index)
                && graph.observed().components[id].realization == RealizationState::Created
                && !graph.deferred_activation.contains(id)
                && self.readiness_satisfied(graph, id)
        };
        let force = self
            .pending_auto_start
            .iter()
            .any(|(index, forced)| *forced && eligible(index));
        let ready: BTreeSet<_> = self
            .pending_auto_start
            .iter()
            .filter(|(index, forced)| **forced == force && eligible(index))
            .map(|(index, _)| *index)
            .collect();
        self.pending_auto_start.retain(|index, _| {
            let id = graph.nodes[*index].descriptor.id();
            graph.ids.get(id) == Some(index) && !ready.contains(index)
        });
        if !ready.is_empty() {
            self.begin_start(graph, ready, None, force);
        }
        Ok(())
    }

    fn readiness_satisfied(&self, graph: &ComputationGraph, id: &ComponentId) -> bool {
        !graph.snapshot.readiness_required.contains(id)
            || ((graph
                .snapshot
                .edges
                .iter()
                .any(|edge| edge.definition.from.component == *id)
                || graph
                    .snapshot
                    .control_connections
                    .iter()
                    .chain(graph.snapshot.subscriptions.iter())
                    .any(|(from, _)| from == id))
                && graph.peers.downstream_ready(id))
    }

    fn notify_availability(
        &self,
        index: usize,
        notification: crate::computation::v1::ControlNotification,
    ) {
        if let Some(control) = self.peer_controls.get(&index) {
            if let Err(error) = control.notify_upstream(notification.clone()) {
                log::warn!(
                    "Upstream availability notification failed for {}: {error}",
                    control.id()
                );
            }
            if let Err(error) = control.notify_downstream(notification) {
                log::warn!(
                    "Peer availability notification failed for {}: {error}",
                    control.id()
                );
            }
        }
    }

    fn launch(
        &mut self,
        graph: &ComputationGraph,
        index: usize,
        operation: Operation,
    ) -> GraphResult<()> {
        let slot = &graph.components[index];
        if operation == Operation::Start {
            graph.peers.reset_ready(&slot.id, slot.generation)?;
        }
        let mut lease = slot.take()?;
        let node = graph.nodes[index].clone();
        let generation = slot.generation;
        let epoch = OperationEpoch(
            graph.observed().components[&slot.id]
                .operation
                .0
                .checked_add(1)
                .ok_or_else(|| topology("component operation epoch exhausted"))?,
        );
        let timeout = graph.cleanup_timeout;
        let declarations = graph.snapshot.resources.clone();
        let resources = graph.resource_handles.clone();
        let scope = graph.execution_scope.clone();
        let graph_id = graph.snapshot.id.clone();
        let downstream: Vec<_> = graph
            .snapshot
            .edges
            .iter()
            .filter(|edge| edge.definition.from.component == slot.id)
            .map(|edge| edge.definition.to.component.clone())
            .collect();
        let (quiesce, mut quiescence) = watch::channel(false);
        let span = tracing::info_span!("computation_component",
            instance_id = %graph.execution_scope,
            component_id = %node.descriptor.id(),
            component_type = match node.role {
                ComponentRole::Source => "source",
                ComponentRole::Transformer | ComponentRole::Query => "query",
                ComponentRole::Sink => "reaction",
                ComponentRole::Service => "query",
            },
            generation = generation.0);
        update(graph, |state| {
            let observed = state.components.get_mut(&slot.id).expect("component");
            observed.operation = epoch;
            observed.revision = state.revision;
            observed.transition_time = Utc::now();
            if operation == Operation::Create {
                observed.realization = RealizationState::Creating;
            } else if operation != Operation::Process {
                observed.lifecycle = if operation == Operation::Start {
                    ComponentLifecycle::Starting
                } else {
                    ComponentLifecycle::Stopping
                };
            }
        });
        let future = async move {
            match operation {
                Operation::Create => {
                    super::validate_role(&node.descriptor, node.role, node.completion).map_err(
                        |error| super::addition::validation_failure(node.descriptor.id(), error),
                    )?;
                    if let Component::Deferred {
                        specification,
                        factory,
                    } = &lease.component
                    {
                        let specification = specification.clone();
                        let factory = factory.clone();
                        super::specification::validate_specification(
                            &graph_id,
                            &specification,
                            factory.as_ref(),
                            &declarations,
                            &resources,
                        )
                        .map_err(|error| {
                            super::addition::validation_failure(node.descriptor.id(), error)
                        })?;
                        let context = ConstructionContext::resolve(
                            scope,
                            graph_id,
                            generation,
                            specification,
                            resources,
                            &factory.descriptor().configuration,
                        )
                        .await
                        .map_err(|error| GraphError::Creation {
                            component: node.descriptor.id().clone(),
                            disposition: error.disposition,
                            source: error.source,
                        })?;
                        let constructed = tokio::time::timeout(timeout, factory.create(context))
                            .await
                            .map_err(|_| GraphError::ReconciliationTimeout)?
                            .map_err(|error| GraphError::Creation {
                                component: node.descriptor.id().clone(),
                                disposition: error.disposition,
                                source: error.source,
                            })?;
                        lease.component = constructed.0;
                    } else if matches!(lease.component, Component::Unresolved(_)) {
                        return Err(super::addition::validation_failure(
                            node.descriptor.id(),
                            topology("component factory or instance binding is missing"),
                        ));
                    }
                    if lease.component.role() != node.role {
                        return Err(super::addition::validation_failure(
                            node.descriptor.id(),
                            topology("constructed role differs from its declaration"),
                        ));
                    }
                    check_descriptor(&lease.component, &node).map_err(|error| {
                        super::addition::validation_failure(node.descriptor.id(), error)
                    })
                }
                Operation::Start => {
                    check_descriptor(&lease.component, &node)?;
                    super::validate_source_progress(&lease.component, &graph_id, &downstream)?;
                    lease.attempted = true;
                    lease
                        .component
                        .start()
                        .await
                        .map_err(|source| component_error(&node, "start", source))?;
                    check_descriptor(&lease.component, &node)
                }
                Operation::Process => {
                    let Instance {
                        component,
                        sequences,
                        inputs,
                        outputs,
                        ..
                    } = &mut *lease;
                    run_node(
                        component,
                        &node,
                        sequences,
                        inputs,
                        outputs,
                        &mut quiescence,
                        super::SourceRouting {
                            graph_id: &graph_id,
                            downstream: &downstream,
                        },
                    )
                    .await
                }
                Operation::Stop => {
                    if !lease.attempted {
                        return Ok(());
                    }
                    tokio::time::timeout(timeout, lease.component.stop())
                        .await
                        .map_err(|_| GraphError::StopTimeout {
                            component: node.descriptor.id().clone(),
                        })?
                        .map_err(|source| component_error(&node, "stop", source))?;
                    lease.attempted = false;
                    Ok(())
                }
            }
        };
        let (abort, registration) = AbortHandle::new_pair();
        self.futures.push(
            async move {
                Completion {
                    index,
                    generation,
                    epoch,
                    operation,
                    result: Abortable::new(future.instrument(span), registration).await,
                }
            }
            .boxed(),
        );
        self.active.insert(
            index,
            Active {
                epoch,
                operation,
                abort,
                quiesce,
            },
        );
        Ok(())
    }

    fn begin_start(
        &mut self,
        graph: &ComputationGraph,
        selected: BTreeSet<usize>,
        reply: Option<oneshot::Sender<GraphResult<StartReport>>>,
        force: bool,
    ) {
        update(graph, |state| {
            for index in &selected {
                let id = graph.nodes[*index].descriptor.id();
                let node = state.components.get_mut(id).expect("selected component");
                if (force
                    || graph.snapshot.lifecycle_policies[id].auto_start
                        && !graph.deferred_activation.contains(id))
                    && node.realization == RealizationState::Created
                    && node.lifecycle == ComponentLifecycle::Stopped
                {
                    node.started = false;
                    node.failure = None;
                    node.lifecycle_requested = true;
                }
            }
        });
        self.starting = Some(Starting {
            revision: graph.snapshot.revision,
            pending: selected,
            outcomes: BTreeMap::new(),
            reply,
            force,
        });
    }

    fn advance_start(&mut self, graph: &ComputationGraph) -> GraphResult<()> {
        let Some(mut group) = self.starting.take() else {
            return Ok(());
        };
        for index in graph.order.iter().rev().copied() {
            if !group.pending.contains(&index) {
                continue;
            }
            let node = &graph.nodes[index];
            let id = node.descriptor.id();
            let observed = graph.observed();
            let current = &observed.components[id];
            let outcome = if current.realization != RealizationState::Created {
                Some(StartOutcome::NotCreated)
            } else if !group.force
                && (graph.deferred_activation.contains(id)
                    || !graph.snapshot.lifecycle_policies[id].auto_start)
            {
                Some(StartOutcome::NotRequested)
            } else if !binding_dependencies(graph, id).is_empty() {
                Some(StartOutcome::Blocked {
                    dependencies: binding_dependencies(graph, id),
                })
            } else if !self.readiness_satisfied(graph, id) {
                self.pending_auto_start.insert(index, group.force);
                Some(StartOutcome::Blocked {
                    dependencies: graph
                        .snapshot
                        .edges
                        .iter()
                        .filter(|edge| edge.definition.from.component == *id)
                        .map(|edge| edge.definition.to.component.clone())
                        .chain(
                            graph
                                .snapshot
                                .control_connections
                                .iter()
                                .chain(graph.snapshot.subscriptions.iter())
                                .filter(|(from, _)| from == id)
                                .map(|(_, to)| to.clone()),
                        )
                        .collect(),
                })
            } else if current.lifecycle == ComponentLifecycle::Running {
                Some(StartOutcome::AlreadyRunning)
            } else if current.lifecycle == ComponentLifecycle::Quiesced {
                self.paused.remove(&index);
                update(graph, |state| {
                    state.components.get_mut(id).expect("component").lifecycle =
                        ComponentLifecycle::Running
                });
                self.launch(graph, index, Operation::Process)?;
                Some(StartOutcome::AlreadyRunning)
            } else if self.active.contains_key(&index)
                || self.exhausted.contains(&index)
                || (current.lifecycle == ComponentLifecycle::Failed
                    && graph.components[index].take()?.attempted)
            {
                Some(StartOutcome::Blocked {
                    dependencies: vec![id.clone()],
                })
            } else {
                let dependencies: Vec<_> = graph
                    .snapshot
                    .edges
                    .iter()
                    .filter(|edge| {
                        edge.definition.to.component == *id
                            && edge.policy.activation == ActivationCoupling::RequiresRunning
                            && observed.components[&edge.definition.from.component].lifecycle
                                != ComponentLifecycle::Running
                    })
                    .map(|edge| edge.definition.from.component.clone())
                    .collect();
                if dependencies.iter().any(|dependency| {
                    let index = graph.ids[dependency];
                    group.pending.contains(&index)
                        || self.active.get(&index).is_some_and(|active| {
                            active.operation == Operation::Start
                                || observed.components[dependency].lifecycle
                                    == ComponentLifecycle::Starting
                        })
                }) {
                    continue;
                }
                if dependencies.is_empty() {
                    None
                } else {
                    Some(StartOutcome::Blocked { dependencies })
                }
            };
            group.pending.remove(&index);
            if let Some(outcome) = outcome {
                group.outcomes.insert(id.clone(), outcome);
            } else {
                self.launch(graph, index, Operation::Start)?;
            }
        }
        if group.pending.is_empty()
            && !self
                .active
                .values()
                .any(|active| active.operation == Operation::Start)
        {
            let failed = group.outcomes.values().any(|outcome| {
                matches!(
                    outcome,
                    StartOutcome::StartFailed(_)
                        | StartOutcome::NotCreated
                        | StartOutcome::Blocked { .. }
                )
            });
            let report = StartReport {
                revision: group.revision,
                summary: if failed {
                    OperationSummary::CompletedWithFailures
                } else {
                    OperationSummary::Completed
                },
                components: group.outcomes,
            };
            update(graph, |state| state.startup = Some(report.clone()));
            graph.state.send_replace(GraphState::Running);
            if let Some(reply) = group.reply {
                let _ = reply.send(Ok(report));
            }
        } else {
            self.starting = Some(group);
        }
        Ok(())
    }

    fn begin_stop(
        &mut self,
        graph: &ComputationGraph,
        selected: BTreeSet<usize>,
        reply: oneshot::Sender<GraphResult<StopReport>>,
    ) -> GraphResult<()> {
        let mut group = Stopping {
            revision: graph.snapshot.revision,
            pending: BTreeSet::new(),
            outcomes: BTreeMap::new(),
            reply,
        };
        for index in selected {
            self.pending_auto_start.remove(&index);
            update(graph, |state| {
                state
                    .components
                    .get_mut(graph.nodes[index].descriptor.id())
                    .expect("selected component")
                    .lifecycle_requested = true;
            });
            if let Some(control) = self.peer_controls.get(&index) {
                if let Err(error) = control.not_ready() {
                    log::warn!("Cannot revoke readiness for {}: {error}", control.id());
                }
            }
            self.propagated_stop.remove(&index);
            let id = graph.nodes[index].descriptor.id();
            if let Some(starting) = &mut self.starting {
                if starting.pending.remove(&index) {
                    starting
                        .outcomes
                        .insert(id.clone(), StartOutcome::NotRequested);
                }
            }
            if let Some(active) = self.active.get(&index) {
                if active.operation == Operation::Start {
                    if let Some(starting) = &mut self.starting {
                        starting.outcomes.insert(
                            id.clone(),
                            StartOutcome::StartFailed(failure(
                                GraphError::Cancelled,
                                FailurePhase::Activation,
                            )),
                        );
                    }
                }
                active.abort.abort();
                self.stop_after_abort.insert(index);
                group.pending.insert(index);
            } else if graph.components[index].take()?.attempted {
                group.pending.insert(index);
            } else if graph.observed().components[id].realization != RealizationState::Created {
                group.outcomes.insert(id.clone(), StopOutcome::NotCreated);
            } else {
                group
                    .outcomes
                    .insert(id.clone(), StopOutcome::AlreadyStopped);
            }
        }
        self.stopping = Some(group);
        self.finish_stop(graph);
        Ok(())
    }

    fn advance_stop(&mut self, graph: &ComputationGraph) -> GraphResult<()> {
        let pending = self
            .stopping
            .as_ref()
            .map(|group| &group.pending)
            .unwrap_or(&self.propagated_stop);
        for index in graph.order.iter().copied() {
            if !pending.contains(&index) {
                continue;
            }
            if !self.active.contains_key(&index) && !self.stop_after_abort.contains(&index) {
                self.launch(graph, index, Operation::Stop)?;
            }
            break;
        }
        Ok(())
    }

    fn propagate(&mut self, graph: &ComputationGraph, origin: usize) -> GraphResult<()> {
        let mut affected = BTreeSet::from([origin]);
        loop {
            let before = affected.len();
            for edge in graph.snapshot.edges.iter() {
                if edge.policy.propagate_failure
                    && affected.contains(&graph.ids[&edge.definition.from.component])
                {
                    affected.insert(graph.ids[&edge.definition.to.component]);
                }
                if edge.policy.fence_producer_on_failure
                    && affected.contains(&graph.ids[&edge.definition.to.component])
                {
                    affected.insert(graph.ids[&edge.definition.from.component]);
                }
            }
            if affected.len() == before {
                break;
            }
        }
        affected.remove(&origin);
        for index in affected {
            let id = graph.nodes[index].descriptor.id().clone();
            let failure = failure(
                GraphError::DependencyUnavailable {
                    component: id.clone(),
                    dependency: graph.nodes[origin].descriptor.id().clone(),
                },
                FailurePhase::Processing,
            );
            if let Some(starting) = &mut self.starting {
                if starting.pending.remove(&index)
                    || self
                        .active
                        .get(&index)
                        .is_some_and(|active| active.operation == Operation::Start)
                {
                    starting.outcomes.insert(
                        id.clone(),
                        StartOutcome::Blocked {
                            dependencies: vec![graph.nodes[origin].descriptor.id().clone()],
                        },
                    );
                }
            }
            let attempted = if let Some(active) = self.active.get(&index) {
                active.abort.abort();
                self.stop_after_abort.insert(index);
                true
            } else {
                graph.components[index].take()?.attempted
            };
            if attempted {
                self.propagated_stop.insert(index);
            }
            update(graph, |state| {
                let node = state.components.get_mut(&id).expect("dependent");
                node.lifecycle = if attempted {
                    ComponentLifecycle::Stopping
                } else {
                    ComponentLifecycle::Failed
                };
                node.health = ComponentHealth::Unavailable;
                node.failure = Some(failure);
                node.transition_time = Utc::now();
            });
        }
        Ok(())
    }

    fn finish_stop(&mut self, graph: &ComputationGraph) {
        if self
            .stopping
            .as_ref()
            .is_some_and(|group| group.pending.is_empty())
        {
            let group = self.stopping.take().expect("completed stop group");
            let failed = group
                .outcomes
                .values()
                .any(|outcome| matches!(outcome, StopOutcome::StopFailed(_)));
            let _ = group.reply.send(Ok(StopReport {
                revision: group.revision,
                summary: if failed {
                    OperationSummary::CompletedWithFailures
                } else {
                    OperationSummary::Completed
                },
                components: group.outcomes,
            }));
            graph.state.send_replace(GraphState::Running);
        }
    }

    fn complete(
        &mut self,
        graph: &ComputationGraph,
        completion: Completion,
        controls: &PipeGuard,
    ) -> GraphResult<()> {
        let Some(active) = self.active.get(&completion.index) else {
            return Err(GraphError::StaleGeneration);
        };
        let slot = &graph.components[completion.index];
        if active.epoch != completion.epoch || slot.generation != completion.generation {
            return Err(GraphError::StaleGeneration);
        }
        self.active.remove(&completion.index);
        if self.stop_after_abort.remove(&completion.index) {
            self.quiescing.remove(&completion.index);
            if completion.operation == Operation::Create {
                update(graph, |state| {
                    let node = state.components.get_mut(&slot.id).expect("component");
                    node.realization = RealizationState::CreationFailed;
                    node.failure = Some(failure(GraphError::Cancelled, FailurePhase::Creation));
                    node.lifecycle = ComponentLifecycle::Stopped;
                });
            }
            return Ok(());
        }
        let mut result = completion.result.map_err(|_| GraphError::Cancelled)?;
        let id = &slot.id;
        if completion.operation == Operation::Create && result.is_ok() {
            if let Some(failure) = &graph.observed().components[id].failure {
                result = Err(GraphError::Reported {
                    cause: failure.cause.clone(),
                });
            }
        }
        if completion.operation == Operation::Process
            && graph.observed().components[id].lifecycle == ComponentLifecycle::Starting
        {
            if graph.peers.is_ready(id, slot.generation) {
                update(graph, |state| {
                    let node = state.components.get_mut(id).expect("component");
                    node.started = true;
                    node.lifecycle = ComponentLifecycle::Running;
                });
            } else if result.is_ok() {
                result = Err(GraphError::StartupIncomplete);
            }
        }
        if completion.operation == Operation::Create {
            let outcome = match result {
                Ok(()) => {
                    self.attach_control(graph, completion.index)?;
                    let lease = slot.take()?;
                    let connected =
                        graph.nodes[completion.index]
                            .descriptor
                            .ports()
                            .iter()
                            .all(|port| match port.direction() {
                                crate::computation::v1::PortDirection::Input => {
                                    lease.inputs.iter().any(|input| &input.port == port.id())
                                }
                                crate::computation::v1::PortDirection::Output => {
                                    lease.outputs.iter().any(|output| &output.port == port.id())
                                        && graph.nodes[completion.index]
                                            .output_streams
                                            .contains_key(port.id())
                                }
                            });
                    update(graph, |state| {
                        let node = state.components.get_mut(id).expect("added component");
                        node.realization = if connected {
                            RealizationState::Created
                        } else {
                            RealizationState::Blocked
                        };
                        node.failure = None;
                        node.transition_time = Utc::now();
                    });
                    if connected {
                        CreationOutcome::Created
                    } else {
                        CreationOutcome::Blocked {
                            dependencies: Vec::new(),
                            resources: Vec::new(),
                        }
                    }
                }
                Err(error) => {
                    let phase = super::addition::creation_phase(&error);
                    let failure = failure(error, phase);
                    self.notify_availability(
                        completion.index,
                        crate::computation::v1::ControlNotification::Unavailable {
                            reason: failure.cause.to_string(),
                        },
                    );
                    update(graph, |state| {
                        let node = state.components.get_mut(id).expect("added component");
                        node.realization = RealizationState::CreationFailed;
                        node.health = ComponentHealth::Unavailable;
                        node.failure = Some(failure.clone());
                        node.transition_time = Utc::now();
                    });
                    CreationOutcome::CreationFailed(failure)
                }
            };
            update(graph, |state| {
                if let Some(report) = &mut state.deployment {
                    report.components.insert(id.clone(), outcome);
                    report.summary = if report
                        .components
                        .values()
                        .all(|outcome| matches!(outcome, CreationOutcome::Created))
                    {
                        OperationSummary::Completed
                    } else {
                        OperationSummary::CompletedWithFailures
                    };
                }
            });
            return Ok(());
        }
        match (completion.operation, result) {
            (Operation::Start, Ok(())) => {
                let automatically_ready = !slot.take()?.component.requires_readiness_confirmation();
                update(graph, |state| {
                    let node = state.components.get_mut(id).expect("component");
                    node.lifecycle = if automatically_ready {
                        ComponentLifecycle::Running
                    } else {
                        ComponentLifecycle::Starting
                    };
                    node.health = ComponentHealth::Unknown;
                    node.failure = None;
                    node.started |= automatically_ready;
                    node.transition_time = Utc::now();
                });
                if let Some(group) = &mut self.starting {
                    group.outcomes.insert(id.clone(), StartOutcome::Started);
                }
                if automatically_ready {
                    if let Some(control) = self.peer_controls.get(&completion.index) {
                        if let Err(error) = control.ready() {
                            log::warn!("Readiness notification failed for {id}: {error}");
                        }
                    }
                    self.notify_availability(
                        completion.index,
                        crate::computation::v1::ControlNotification::Available,
                    );
                }
                self.launch(graph, completion.index, Operation::Process)?;
            }
            (Operation::Process, Ok(())) => {
                if self.quiescing.remove(&completion.index) {
                    self.paused.insert(completion.index);
                    update(graph, |state| {
                        let observed = state.components.get_mut(id).expect("component");
                        observed.lifecycle = ComponentLifecycle::Quiesced;
                        observed.transition_time = Utc::now();
                    });
                    return Ok(());
                }
                self.exhausted.insert(completion.index);
                update(graph, |state| {
                    state.components.get_mut(id).expect("component").exhausted = true
                });
                let lease = slot.take()?;
                for output in &lease.outputs {
                    if let Some(control) = controls.0.get(&output.edge) {
                        control.close();
                    }
                }
                update(graph, |state| {
                    for (edge, observed) in &mut state.relationships {
                        if edge.from.component == *id && observed.binding == BindingState::Bound {
                            observed.availability = DataAvailability::Exhausted;
                        }
                    }
                });
            }
            (Operation::Stop, Ok(())) => {
                if let Some(control) = self.peer_controls.get(&completion.index) {
                    if let Err(error) = control.not_ready() {
                        log::warn!("Readiness notification failed for {id}: {error}");
                    }
                }
                let propagated = self.propagated_stop.remove(&completion.index);
                update(graph, |state| {
                    let node = state.components.get_mut(id).expect("component");
                    node.lifecycle = if propagated {
                        ComponentLifecycle::Failed
                    } else {
                        ComponentLifecycle::Stopped
                    };
                    node.health = if propagated {
                        ComponentHealth::Unavailable
                    } else {
                        ComponentHealth::Unknown
                    };
                    node.transition_time = Utc::now();
                    for (edge, observed) in &mut state.relationships {
                        if edge.from.component == *id
                            && observed.binding == BindingState::Bound
                            && observed.availability != DataAvailability::Exhausted
                        {
                            observed.availability = DataAvailability::Idle;
                        }
                    }
                });
                if let Some(group) = &mut self.stopping {
                    group.pending.remove(&completion.index);
                    group.outcomes.insert(id.clone(), StopOutcome::Stopped);
                }
            }
            (operation, Err(error)) => {
                self.quiescing.remove(&completion.index);
                let fatal = matches!(
                    error,
                    GraphError::Contract(_)
                        | GraphError::Emission { .. }
                        | GraphError::Forward { .. }
                        | GraphError::Topology { .. }
                );
                let phase = match operation {
                    Operation::Create => FailurePhase::Creation,
                    Operation::Start => FailurePhase::Activation,
                    Operation::Process => FailurePhase::Processing,
                    Operation::Stop => FailurePhase::Stop,
                };
                let failure = failure(error, phase);
                self.notify_availability(
                    completion.index,
                    crate::computation::v1::ControlNotification::Unavailable {
                        reason: failure.cause.to_string(),
                    },
                );
                if let Some(control) = self.peer_controls.get(&completion.index) {
                    if let Err(error) = control.not_ready() {
                        log::warn!("Readiness notification failed for {id}: {error}");
                    }
                }
                self.last_failure = Some(failure.cause.clone());
                update(graph, |state| {
                    let node = state.components.get_mut(id).expect("component");
                    node.lifecycle = ComponentLifecycle::Failed;
                    node.health = ComponentHealth::Unavailable;
                    node.failure = Some(failure.clone());
                    node.transition_time = Utc::now();
                    for (edge, observed) in &mut state.relationships {
                        if edge.from.component == *id {
                            observed.availability = DataAvailability::Unavailable;
                        }
                    }
                });
                if operation == Operation::Start {
                    if let Some(group) = &mut self.starting {
                        group
                            .outcomes
                            .insert(id.clone(), StartOutcome::StartFailed(failure.clone()));
                    }
                }
                if operation == Operation::Stop {
                    self.propagated_stop.remove(&completion.index);
                    if let Some(group) = &mut self.stopping {
                        group.pending.remove(&completion.index);
                        group
                            .outcomes
                            .insert(id.clone(), StopOutcome::StopFailed(failure.clone()));
                    }
                }
                if fatal {
                    return Err(GraphError::Reported {
                        cause: failure.cause,
                    });
                }
                if operation != Operation::Stop {
                    self.propagate(graph, completion.index)?;
                }
            }
            (Operation::Create, Ok(())) => unreachable!("creation handled above"),
        }
        self.finish_stop(graph);
        Ok(())
    }
}

pub(super) async fn run(
    graph: &mut ComputationGraph,
    cancel: &mut watch::Receiver<bool>,
    mut commands: mpsc::Receiver<Command>,
    auto_start: bool,
) -> GraphResult<()> {
    if *cancel.borrow() {
        return Err(GraphError::Cancelled);
    }
    let mut controls = deploy(graph, cancel).await?;
    let mut operations = Operations::default();
    for index in graph.order.clone() {
        operations.attach_control(graph, index)?;
    }
    operations.refresh_connections(graph)?;
    let mut peer_changes = graph.peers.subscribe();
    if auto_start {
        operations.begin_start(graph, select(graph, &GraphSelection::All)?, None, false);
    } else {
        graph.state.send_replace(GraphState::Ready);
    }
    loop {
        operations.advance_additions(graph)?;
        operations.advance_start(graph)?;
        operations.advance_stop(graph)?;
        if auto_start
            && operations.active.is_empty()
            && operations.starting.is_none()
            && operations.stopping.is_none()
            && operations.propagated_stop.is_empty()
        {
            if let Some(cause) = operations.last_failure {
                return Err(GraphError::Reported { cause });
            }
            if graph
                .observed()
                .startup
                .as_ref()
                .is_some_and(|report| report.summary == OperationSummary::CompletedWithFailures)
            {
                return Err(GraphError::StartupIncomplete);
            }
            return Ok(());
        }
        tokio::select! {
            biased;
            _ = cancelled(cancel) => return Err(GraphError::Cancelled),
            completion = operations.control_futures.next(), if !operations.control_futures.is_empty() => {
                if let Some(completion) = completion {
                    operations.complete_control(graph, completion);
                }
            }
            changed = peer_changes.changed() => {
                changed.map_err(|_| GraphError::ControllerClosed)?;
            }
            completion = operations.futures.next(), if !operations.futures.is_empty() => {
                if let Some(completion) = completion {
                    operations.complete(graph, completion, &controls)?;
                }
            }
            command = commands.recv() => match command {
                Some(Command::ReadinessPolicy { component, generation, required, reply }) => {
                    let result = (|| {
                        let state = graph.observed();
                        let node = state.components.get(&component)
                            .filter(|node| node.generation == generation)
                            .ok_or(GraphError::StaleGeneration)?;
                        if matches!(node.lifecycle, ComponentLifecycle::Starting | ComponentLifecycle::Running) {
                            return Err(GraphError::OperationInProgress);
                        }
                        if graph.snapshot.readiness_required.contains(&component) != required {
                            let revision = graph.snapshot.revision.0.checked_add(1)
                                .ok_or_else(|| topology("graph revision exhausted"))?;
                            if required {
                                graph.snapshot.readiness_required.insert(component.clone());
                            } else {
                                graph.snapshot.readiness_required.remove(&component);
                            }
                            graph.snapshot.revision = GraphRevision(revision);
                            graph.desired.send_replace(Arc::new(graph.snapshot.clone()));
                            update(graph, |state| state.revision = GraphRevision(revision));
                        }
                        Ok(())
                    })();
                    let _ = reply.send(result);
                }
                Some(Command::AutoStart) => {
                    for &index in graph.ids.values() {
                        if graph.snapshot.lifecycle_policies[graph.nodes[index].descriptor.id()].auto_start {
                            graph.deferred_activation.remove(graph.nodes[index].descriptor.id());
                            operations.pending_auto_start.insert(index, false);
                        }
                    }
                }
                Some(Command::BindStream { component, generation, port, stream, reply }) => {
                    let result = (|| {
                        let index = *graph.ids.get(&component).ok_or(GraphError::StaleGeneration)?;
                        let state = graph.observed();
                        if state.components[&component].generation != generation {
                            return Err(GraphError::StaleGeneration);
                        }
                        if state.components[&component].started {
                            return Err(topology("replace or reconcile a previously started output stream"));
                        }
                        if !graph.nodes[index].descriptor.ports().iter().any(|candidate| {
                            candidate.id() == &port
                                && candidate.direction() == crate::computation::v1::PortDirection::Output
                        }) {
                            return Err(topology("stream binding does not name an output port"));
                        }
                        if graph.snapshot.nodes.iter().any(|node| {
                            node.output_streams.iter().any(|(other, value)| {
                                value == &stream && (node.descriptor.id() != &component || other != &port)
                            })
                        }) {
                            return Err(topology("duplicate output stream identity"));
                        }
                        let revision = GraphRevision(graph.snapshot.revision.0.checked_add(1)
                            .ok_or_else(|| topology("graph revision exhausted"))?);
                        graph.nodes[index].output_streams.insert(port.clone(), stream.clone());
                        let mut nodes = graph.snapshot.nodes.to_vec();
                        nodes.iter_mut().find(|node| node.descriptor.id() == &component)
                            .expect("component").output_streams.insert(port, stream);
                        graph.snapshot.nodes = nodes.into();
                        graph.snapshot.revision = revision;
                        graph.desired.send_replace(Arc::new(graph.snapshot.clone()));
                        update(graph, |state| state.revision = revision);
                        Ok(())
                    })();
                    let _ = reply.send(result);
                }
                Some(Command::ObserveResources { component, generation, bindings }) => {
                    if let Err(error) = super::resources::observe(graph, component.clone(), generation, bindings) {
                        if matches!(error, GraphError::StaleGeneration) {
                            log::debug!("Ignoring retired provider observation for {component}");
                        } else {
                            log::error!("Provider observation failed for {component}: {error}");
                            let failure = failure(error, FailurePhase::Control);
                            update(graph, |state| {
                                if let Some(node) = state.components.get_mut(&component) {
                                    node.failure = Some(failure);
                                    node.health = ComponentHealth::Degraded;
                                }
                            });
                        }
                    }
                }
                Some(Command::ObservePlugin { component, generation, plugin }) => {
                    if let Err(error) = super::resources::observe_plugin(graph, &component, generation, plugin) {
                        if matches!(error, GraphError::StaleGeneration) {
                            log::debug!("Ignoring plugin observation from retired component {component}");
                        } else {
                            log::error!("Plugin observation failed for {component}: {error}");
                            let failure = failure(error, FailurePhase::Validation);
                            update(graph, |state| {
                                if let Some(node) = state.components.get_mut(&component) {
                                    node.failure = Some(failure);
                                    node.health = ComponentHealth::Degraded;
                                }
                            });
                        }
                    }
                }
                Some(Command::Add { addition, reply }) => {
                    let validation_error = addition.bindings.validation_error.clone();
                    let deferred = addition.bindings.defer_activation;
                    let activate = addition.definition.lifecycle.auto_start
                        && !addition.bindings.defer_activation;
                    let mut pending = Some(addition);
                    match super::addition::insert(graph, &mut pending) {
                        Ok((index, id, generation)) => {
                            if deferred {
                                graph.deferred_activation.insert(id.clone());
                            }
                            if activate {
                                operations.pending_auto_start.insert(index, false);
                            }
                            let setup = operations.attach_control(graph, index)
                                .and_then(|_| operations.refresh_connections(graph))
                                .and_then(|_| match validation_error {
                                    Some(cause) => Err(GraphError::Reported { cause }),
                                    None => operations.launch(graph, index, Operation::Create),
                                });
                            if let Err(error) = setup {
                                let phase = super::addition::creation_phase(&error);
                                let failure = failure(error, phase);
                                update(graph, |state| {
                                    let node = state.components.get_mut(&id).expect("added component");
                                    node.realization = RealizationState::CreationFailed;
                                    node.failure = Some(failure);
                                });
                            }
                            let _ = reply.send(Ok((id, generation)));
                        }
                        Err(error) => {
                            let addition = super::addition::RejectedAddition::new(
                                pending.take().expect("rejected addition retains its ownership")
                            );
                            let mut rejected = graph.rejected_additions.lock().unwrap_or_else(|error| {
                                log::error!("Retaining rejected resources through poisoned registry: {error}");
                                error.into_inner()
                            });
                            rejected.retain(|addition| !addition.complete());
                            rejected.push(addition.clone());
                            let _ = reply.send(Err(GraphError::AdditionRejected {
                                cause: Box::new(error), addition,
                            }));
                        }
                    }
                }
                Some(Command::ControlConnections { connections, subscriptions, reply }) => {
                    let result = if connections.iter().any(|(from, to)| {
                        from == to || !graph.ids.contains_key(to)
                            || (!subscriptions && !graph.ids.contains_key(from))
                    }) {
                        Err(topology("control connection requires distinct existing components"))
                    } else {
                        match graph.snapshot.revision.0.checked_add(1) {
                            Some(revision) => {
                                if subscriptions {
                                    graph.snapshot.subscriptions = connections.into();
                                } else {
                                    graph.snapshot.control_connections = connections.into();
                                }
                                operations.refresh_connections(graph).map(|_| {
                                    graph.snapshot.revision = GraphRevision(revision);
                                    graph.desired.send_replace(Arc::new(graph.snapshot.clone()));
                                    update(graph, |state| state.revision = GraphRevision(revision));
                                })
                            }
                            None => Err(topology("graph revision exhausted")),
                        }
                    };
                    let _ = reply.send(result);
                }
                Some(Command::ControlHandler { component, generation, handler, reply }) => {
                    let result = graph.ids.get(&component)
                        .filter(|index| graph.components[**index].generation == generation)
                        .and_then(|index| operations.control_handlers.get(index))
                        .ok_or(GraphError::StaleGeneration)
                        .map(|handlers| { handlers.send_replace(Some(handler)); });
                    let _ = reply.send(result);
                }
                Some(Command::HandleStart { component, generation, reply }) => {
                    let valid = graph.ids.get(&component)
                        .filter(|index| graph.components[**index].generation == generation)
                        .copied().ok_or(GraphError::StaleGeneration)
                        .and_then(|index| {
                            if operations.starting.is_some() || operations.stopping.is_some() {
                                Err(GraphError::OperationInProgress)
                            } else {
                                Ok(index)
                            }
                        });
                    match valid {
                        Ok(index) => {
                            graph.deferred_activation.remove(&component);
                            operations.begin_start(graph, BTreeSet::from([index]), Some(reply), true);
                        }
                        Err(error) => { let _ = reply.send(Err(error)); }
                    }
                }
                Some(Command::HandleStop { component, generation, reply }) => {
                    let valid = graph.ids.get(&component)
                        .filter(|index| graph.components[**index].generation == generation)
                        .copied().ok_or(GraphError::StaleGeneration)
                        .and_then(|index| {
                            if operations.stopping.is_some() {
                                Err(GraphError::OperationInProgress)
                            } else {
                                Ok(index)
                            }
                        });
                    match valid {
                        Ok(index) => operations.begin_stop(graph, BTreeSet::from([index]), reply)?,
                        Err(error) => { let _ = reply.send(Err(error)); }
                    }
                }
                Some(Command::Preview { revision, changes, reply }) => {
                    let result = check_revision(graph, revision).and_then(|_| reconcile::preview(graph, changes));
                    let _ = reply.send(result);
                }
                Some(Command::Reconcile { preview, bindings, reply }) => {
                    let result = reconcile::execute(graph, &mut operations, &mut controls, cancel, preview, bindings).await;
                    let cancelled = matches!(&result, Err(GraphError::Cancelled));
                    let _ = reply.send(result);
                    if cancelled { return Err(GraphError::Cancelled); }
                }
                Some(Command::Start { revision, selection, force, reply }) => {
                    let valid = check_revision(graph, revision).and_then(|_| {
                        if operations.starting.is_some() || operations.stopping.is_some() {
                            Err(GraphError::OperationInProgress)
                        } else { select(graph, &selection) }
                    });
                    match valid {
                        Ok(selected) => {
                            for index in &selected {
                                graph.deferred_activation.remove(graph.nodes[*index].descriptor.id());
                            }
                            operations.begin_start(graph, selected, Some(reply), force);
                        }
                        Err(error) => { let _ = reply.send(Err(error)); }
                    }
                }
                Some(Command::Stop { revision, selection, reply }) => {
                    let valid = check_revision(graph, revision).and_then(|_| {
                        if operations.stopping.is_some() { Err(GraphError::OperationInProgress) }
                        else { select(graph, &selection) }
                    });
                    match valid {
                        Ok(selected) => operations.begin_stop(graph, selected, reply)?,
                        Err(error) => { let _ = reply.send(Err(error)); }
                    }
                }
                Some(Command::Quiesce { revision, selection, reply }) => {
                    let result = match check_revision(graph, revision).and_then(|_| select(graph, &selection)) {
                        Ok(selected) => reconcile::quiesce(graph, &mut operations, &controls, cancel, selected).await,
                        Err(error) => Err(error),
                    };
                    let was_cancelled = matches!(&result, Err(GraphError::Cancelled));
                    let _ = reply.send(result);
                    if was_cancelled { return Err(GraphError::Cancelled); }
                }
                Some(Command::Policy { revision, component, policy, reply }) => {
                    let result = check_revision(graph, revision).and_then(|_| {
                        if operations.starting.is_some() || operations.stopping.is_some() {
                            return Err(GraphError::OperationInProgress);
                        }
                        if !graph.ids.contains_key(&component) {
                            return Err(topology(format!("unknown component {component}")));
                        }
                        if graph.snapshot.lifecycle_policies.get(&component) != Some(&policy) {
                            graph.snapshot.revision = GraphRevision(graph.snapshot.revision.0.checked_add(1)
                                .ok_or_else(|| topology("graph revision exhausted"))?);
                            graph.snapshot.lifecycle_policies.insert(component, policy);
                            graph.desired.send_replace(Arc::new(graph.snapshot.clone()));
                            update(graph, |state| state.revision = graph.snapshot.revision);
                        }
                        Ok(graph.snapshot.revision)
                    });
                    let _ = reply.send(result);
                }
                Some(Command::Health { revision, observation, reply }) => {
                    let result = check_revision(graph, revision).and_then(|_| {
                        let state = graph.observed();
                        let node = state.components.get(&observation.component).ok_or(GraphError::StaleGeneration)?;
                        if node.generation != observation.generation || node.operation != observation.operation {
                            return Err(GraphError::StaleGeneration);
                        }
                        update(graph, |state| {
                            let node = state.components.get_mut(&observation.component).expect("validated component");
                            node.health = observation.health;
                            node.revision = state.revision;
                            node.transition_time = Utc::now();
                            for (edge, observed) in &mut state.relationships {
                                if edge.from.component == observation.component
                                    && observed.binding == BindingState::Bound
                                    && observed.availability != DataAvailability::Exhausted
                                {
                                    observed.availability = match observation.health {
                                        ComponentHealth::Unknown => DataAvailability::Unknown,
                                        ComponentHealth::Healthy => DataAvailability::Available,
                                        ComponentHealth::Degraded | ComponentHealth::Unavailable => DataAvailability::Unavailable,
                                    };
                                }
                            }
                        });
                        Ok(())
                    });
                    if result.is_ok() {
                        let notification = match observation.health {
                            ComponentHealth::Healthy => Some(crate::computation::v1::ControlNotification::Available),
                            ComponentHealth::Degraded | ComponentHealth::Unavailable =>
                                Some(crate::computation::v1::ControlNotification::Unavailable {
                                    reason: format!("component health is {:?}", observation.health),
                                }),
                            ComponentHealth::Unknown => None,
                        };
                        if let Some(notification) = notification {
                            operations.notify_availability(graph.ids[&observation.component], notification);
                        }
                    }
                    let _ = reply.send(result);
                }
                None => return Err(GraphError::ControllerClosed),
            }
        }
    }
}

pub(super) async fn cleanup(graph: &mut ComputationGraph) -> Vec<GraphError> {
    mark_cleanup_required(&graph.observed);
    let mut errors = Vec::new();
    let now = tokio::time::Instant::now();
    let deadline = match now.checked_add(graph.cleanup_timeout) {
        Some(deadline) => deadline,
        None => {
            errors.push(topology("cleanup deadline is no longer representable"));
            now
        }
    };
    for index in graph.order.iter().copied() {
        let mut lease = match graph.components[index].take() {
            Ok(lease) => lease,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        let node: &NodeSnapshot = &graph.nodes[index];
        if lease.attempted {
            let result = tokio::time::timeout_at(deadline, lease.component.stop()).await;
            match result {
                Ok(Ok(())) => {
                    lease.attempted = false;
                    update(graph, |state| {
                        let observed = state
                            .components
                            .get_mut(node.descriptor.id())
                            .expect("component");
                        observed.lifecycle = ComponentLifecycle::Stopped;
                        observed.transition_time = Utc::now();
                    });
                }
                other => {
                    let error = match other {
                        Ok(Err(source)) => component_error(node, "stop", source),
                        Err(_) => GraphError::StopTimeout {
                            component: node.descriptor.id().clone(),
                        },
                        Ok(Ok(())) => unreachable!(),
                    };
                    let failure = failure(error, FailurePhase::Stop);
                    update(graph, |state| {
                        let observed = state
                            .components
                            .get_mut(node.descriptor.id())
                            .expect("component");
                        observed.lifecycle = ComponentLifecycle::Failed;
                        observed.failure = Some(failure.clone());
                    });
                    errors.push(GraphError::Reported {
                        cause: failure.cause,
                    });
                }
            }
        }
        if !lease.attempted {
            update(graph, |state| {
                let observed = state
                    .components
                    .get_mut(node.descriptor.id())
                    .expect("component");
                observed.lifecycle = ComponentLifecycle::Stopped;
            });
        }
        lease.inputs.clear();
        lease.outputs.clear();
    }
    update(graph, |state| {
        for edge in state.relationships.values_mut() {
            if matches!(edge.binding, BindingState::Bound | BindingState::Draining) {
                edge.binding = BindingState::Declared;
                edge.availability = DataAvailability::Unavailable;
                edge.transition_time = Utc::now();
            }
        }
        for node in state.components.values_mut() {
            if matches!(
                node.realization,
                RealizationState::Created | RealizationState::Creating
            ) {
                node.realization = RealizationState::Pending;
            }
        }
    });
    errors
}

pub(super) fn needs_cleanup(graph: &ComputationGraph) -> GraphResult<bool> {
    for &index in graph.ids.values() {
        let slot = &graph.components[index];
        let lease = slot.take()?;
        if lease.attempted || !lease.inputs.is_empty() || !lease.outputs.is_empty() {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) async fn dispose_resources(graph: &mut ComputationGraph) -> GraphResult<()> {
    let mut failures = Vec::new();
    for (id, specification) in &graph.snapshot.resources {
        if specification.ownership == ResourceOwnership::Borrowed {
            continue;
        }
        let Some(resource) = graph.resource_handles.get(id) else {
            continue;
        };
        update(graph, |state| {
            state.resources.get_mut(id).expect("resource").realization =
                ResourceRealization::CleanupRequired;
        });
        let result = tokio::time::timeout(graph.cleanup_timeout, resource.shutdown()).await;
        match result {
            Ok(Ok(())) => {
                graph.resource_handles.remove(id);
                update(graph, |state| {
                    let observed = state.resources.get_mut(id).expect("resource");
                    observed.realization = ResourceRealization::Released;
                    observed.failure = None;
                    observed.transition_time = Utc::now();
                });
            }
            other => {
                let source = match other {
                    Ok(Err(error)) => error,
                    Err(error) => anyhow::Error::new(error),
                    Ok(Ok(())) => unreachable!(),
                };
                let failure = failure(
                    GraphError::ResourceCleanup {
                        resource: id.clone(),
                        source,
                    },
                    FailurePhase::Removal,
                );
                update(graph, |state| {
                    state.resources.get_mut(id).expect("resource").failure = Some(failure.clone());
                });
                failures.push(GraphError::Reported {
                    cause: failure.cause,
                });
            }
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(GraphError::Cleanup {
            primary: None,
            errors: failures,
        })
    }
}
