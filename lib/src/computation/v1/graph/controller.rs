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

use super::specification::{ConstructionContext, ResourceOwnership};
use super::{
    cancelled, check_descriptor, component_error, run_node, topology, Component, ComputationGraph,
    GraphControl, GraphError, GraphResult, GraphSnapshot, GraphState, Incoming, NodeSnapshot,
    Outgoing, PipeGuard,
};
use crate::computation::v1::{
    ActivationCoupling, BindingState, ComponentFailure, ComponentGeneration, ComponentHealth,
    ComponentId, ComponentLifecycle, CreationOutcome, DataAvailability, DeploymentReport,
    FailureDisposition, FailurePhase, GraphRevision, GraphSelection, HealthObservation,
    LifecyclePolicy, ObservedComponent, ObservedGraph, ObservedRelationship, ObservedResource,
    OperationEpoch, OperationSummary, PortId, RealizationState, ResourceRealization, StartOutcome,
    StartReport, StopOutcome, StopReport,
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
        Arc::new(Self {
            id: component.descriptor().id().clone(),
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
    Start {
        revision: GraphRevision,
        selection: GraphSelection,
        reply: oneshot::Sender<GraphResult<StartReport>>,
    },
    Stop {
        revision: GraphRevision,
        selection: GraphSelection,
        reply: oneshot::Sender<GraphResult<StopReport>>,
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
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Start {
                revision,
                selection,
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
                        transition_time: now,
                    },
                )
            })
            .collect(),
        relationships: snapshot
            .edges
            .iter()
            .map(|edge| {
                (
                    edge.definition.clone(),
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

fn update(graph: &ComputationGraph, change: impl FnOnce(&mut ObservedGraph)) {
    graph
        .observed
        .send_modify(|snapshot| change(Arc::make_mut(snapshot)));
}

pub(super) fn mark_cleanup_required(observed: &watch::Sender<Arc<ObservedGraph>>) {
    observed.send_modify(|snapshot| {
        let snapshot = Arc::make_mut(snapshot);
        for node in snapshot.components.values_mut() {
            if matches!(
                node.lifecycle,
                ComponentLifecycle::Starting | ComponentLifecycle::Running
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
        disposition: if let GraphError::Creation { disposition, .. } = &error {
            *disposition
        } else if matches!(
            &error,
            GraphError::Contract(_) | GraphError::Emission { .. } | GraphError::Topology { .. }
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
    let (ids, direction) = match selection {
        GraphSelection::All => return Ok((0..graph.components.len()).collect()),
        GraphSelection::Exact(ids) => (ids, 0),
        GraphSelection::Dependencies(ids) => (ids, -1),
        GraphSelection::Dependents(ids) => (ids, 1),
    };
    let mut selected = BTreeSet::new();
    for id in ids {
        selected.insert(
            *graph
                .ids
                .get(id)
                .ok_or_else(|| topology(format!("unknown component {id}")))?,
        );
    }
    if direction != 0 {
        loop {
            let mut added = Vec::new();
            for edge in graph.snapshot.edges.iter() {
                let from = graph.ids[&edge.definition.from.component];
                let to = graph.ids[&edge.definition.to.component];
                let (origin, target) = if direction < 0 {
                    (to, from)
                } else {
                    (from, to)
                };
                if selected.contains(&origin) && !selected.contains(&target) {
                    added.push(target);
                }
            }
            if added.is_empty() {
                break;
            }
            selected.extend(added);
        }
    }
    Ok(selected)
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
        let node = graph.snapshot.nodes[index].clone();
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
            .filter(|failure| failure.phase == FailurePhase::Creation)
        {
            unavailable.insert(index);
            creation_failures.insert(index, failure.clone());
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
    for (edge_index, (provider, edge)) in graph
        .providers
        .iter()
        .zip(graph.snapshot.edges.iter())
        .enumerate()
    {
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
        update(graph, |state| {
            let observed = state
                .relationships
                .get_mut(&edge.definition)
                .expect("declared relationship");
            observed.binding = BindingState::Binding;
            observed.generation = state.run_epoch;
            observed.failure = None;
            observed.transition_time = Utc::now();
        });
        let provided = (|| {
            let mut provided = provider.create().map_err(|source| GraphError::Pipe {
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
            graph.components[from].take()?.outputs.push(Outgoing {
                edge: edge_index,
                port: edge.definition.from.port.clone(),
                sender: provided.pipe.sender(),
            });
            graph.components[to].take()?.inputs.push(Incoming {
                edge: edge_index,
                port: edge.definition.to.port.clone(),
                receiver,
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
    for (index, node) in graph.snapshot.nodes.iter().enumerate() {
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
    Start,
    Process,
    Stop,
}

struct Active {
    epoch: OperationEpoch,
    operation: Operation,
    abort: AbortHandle,
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
}

struct Stopping {
    revision: GraphRevision,
    pending: BTreeSet<usize>,
    outcomes: BTreeMap<ComponentId, StopOutcome>,
    reply: oneshot::Sender<GraphResult<StopReport>>,
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
}

impl Operations {
    fn launch(
        &mut self,
        graph: &ComputationGraph,
        index: usize,
        operation: Operation,
    ) -> GraphResult<()> {
        let slot = &graph.components[index];
        let mut lease = slot.take()?;
        let node = graph.snapshot.nodes[index].clone();
        let generation = slot.generation;
        let epoch = OperationEpoch(
            graph.observed().components[&slot.id]
                .operation
                .0
                .checked_add(1)
                .ok_or_else(|| topology("component operation epoch exhausted"))?,
        );
        let timeout = graph.cleanup_timeout;
        update(graph, |state| {
            let observed = state.components.get_mut(&slot.id).expect("component");
            observed.operation = epoch;
            observed.revision = state.revision;
            observed.transition_time = Utc::now();
            if operation != Operation::Process {
                observed.lifecycle = if operation == Operation::Start {
                    ComponentLifecycle::Starting
                } else {
                    ComponentLifecycle::Stopping
                };
            }
        });
        let future = async move {
            match operation {
                Operation::Start => {
                    check_descriptor(&lease.component, &node)?;
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
                    run_node(component, &node, sequences, inputs, outputs).await
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
                    result: Abortable::new(future, registration).await,
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
            },
        );
        Ok(())
    }

    fn begin_start(
        &mut self,
        graph: &ComputationGraph,
        selected: BTreeSet<usize>,
        reply: Option<oneshot::Sender<GraphResult<StartReport>>>,
    ) {
        self.starting = Some(Starting {
            revision: graph.snapshot.revision,
            pending: selected,
            outcomes: BTreeMap::new(),
            reply,
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
            let node = &graph.snapshot.nodes[index];
            let id = node.descriptor.id();
            let observed = graph.observed();
            let current = &observed.components[id];
            let outcome = if current.realization != RealizationState::Created {
                Some(StartOutcome::NotCreated)
            } else if !graph.snapshot.lifecycle_policies[id].auto_start {
                Some(StartOutcome::NotRequested)
            } else if current.lifecycle == ComponentLifecycle::Running {
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
                        || self
                            .active
                            .get(&index)
                            .is_some_and(|active| active.operation == Operation::Start)
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
            self.propagated_stop.remove(&index);
            let id = graph.snapshot.nodes[index].descriptor.id();
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
            }
            if affected.len() == before {
                break;
            }
        }
        affected.remove(&origin);
        for index in affected {
            let id = graph.snapshot.nodes[index].descriptor.id().clone();
            let failure = failure(
                GraphError::DependencyUnavailable {
                    component: id.clone(),
                    dependency: graph.snapshot.nodes[origin].descriptor.id().clone(),
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
                            dependencies: vec![graph.snapshot.nodes[origin]
                                .descriptor
                                .id()
                                .clone()],
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
            return Ok(());
        }
        let result = completion.result.map_err(|_| GraphError::Cancelled)?;
        let id = &slot.id;
        match (completion.operation, result) {
            (Operation::Start, Ok(())) => {
                update(graph, |state| {
                    let node = state.components.get_mut(id).expect("component");
                    node.lifecycle = ComponentLifecycle::Running;
                    node.health = ComponentHealth::Unknown;
                    node.failure = None;
                    node.transition_time = Utc::now();
                });
                if let Some(group) = &mut self.starting {
                    group.outcomes.insert(id.clone(), StartOutcome::Started);
                }
                self.launch(graph, completion.index, Operation::Process)?;
            }
            (Operation::Process, Ok(())) => {
                self.exhausted.insert(completion.index);
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
                let fatal = matches!(
                    error,
                    GraphError::Contract(_)
                        | GraphError::Emission { .. }
                        | GraphError::Forward { .. }
                        | GraphError::Topology { .. }
                );
                let phase = match operation {
                    Operation::Start => FailurePhase::Activation,
                    Operation::Process => FailurePhase::Processing,
                    Operation::Stop => FailurePhase::Stop,
                };
                let failure = failure(error, phase);
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
    let controls = deploy(graph, cancel).await?;
    let mut operations = Operations::default();
    if auto_start {
        operations.begin_start(graph, select(graph, &GraphSelection::All)?, None);
    } else {
        graph.state.send_replace(GraphState::Ready);
    }
    loop {
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
            completion = operations.futures.next(), if !operations.futures.is_empty() => {
                if let Some(completion) = completion {
                    operations.complete(graph, completion, &controls)?;
                }
            }
            command = commands.recv() => match command {
                Some(Command::Start { revision, selection, reply }) => {
                    let valid = check_revision(graph, revision).and_then(|_| {
                        if operations.starting.is_some() || operations.stopping.is_some() {
                            Err(GraphError::OperationInProgress)
                        } else { select(graph, &selection) }
                    });
                    match valid {
                        Ok(selected) => operations.begin_start(graph, selected, Some(reply)),
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
        let node: &NodeSnapshot = &graph.snapshot.nodes[index];
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
    for slot in &graph.components {
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
