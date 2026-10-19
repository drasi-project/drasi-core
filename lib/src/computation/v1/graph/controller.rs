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
    collections::{BTreeMap, BTreeSet, VecDeque},
    ops::{Deref, DerefMut},
    sync::{Arc, Mutex},
    task::Poll,
};

use chrono::Utc;
use futures::{
    future::{AbortHandle, AbortRegistration, Abortable, Aborted, BoxFuture},
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
    EdgeSnapshot, GraphControl, GraphError, GraphResult, GraphSnapshot, GraphState, Incoming,
    NodeSnapshot, Outgoing, PipeGuard,
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
    pub(super) id: ComponentId,
    pub(super) generation: ComponentGeneration,
    value: Mutex<Option<Instance>>,
    configuration: Mutex<super::CapturedComponentConfiguration>,
    query_api: Mutex<Option<Arc<crate::computation::v1::QueryApi>>>,
    recovery: Mutex<crate::computation::v1::ComponentRecovery>,
    damaged: Mutex<Option<Arc<GraphError>>>,
    handler: Mutex<Option<Arc<dyn crate::computation::v1::ControlHandler>>>,
    requires_readiness: std::sync::atomic::AtomicBool,
}

struct Instance {
    component: Component,
    sequences: BTreeMap<PortId, u64>,
    inputs: Vec<Incoming>,
    outputs: Vec<Outgoing>,
    attempted: bool,
}

impl InstanceSlot {
    pub(super) fn take_unstarted(&self) -> Component {
        let mut value = self.value.lock().expect("unstarted component ownership");
        let instance = value.take().expect("unstarted component");
        assert!(!instance.attempted && instance.inputs.is_empty() && instance.outputs.is_empty());
        instance.component
    }

    pub(super) fn new(component: Component, generation: ComponentGeneration) -> Arc<Self> {
        let id = component.descriptor().id().clone();
        Self::declared(id, component, generation)
    }

    pub(super) fn declared(
        id: ComponentId,
        component: Component,
        generation: ComponentGeneration,
    ) -> Arc<Self> {
        let slot = Arc::new(Self {
            id,
            generation,
            configuration: Mutex::new(super::CapturedComponentConfiguration::Unavailable {
                reason: "component metadata has not been captured".into(),
            }),
            query_api: Mutex::new(None),
            recovery: Mutex::new(crate::computation::v1::ComponentRecovery::default()),
            damaged: Mutex::new(None),
            handler: Mutex::new(None),
            requires_readiness: std::sync::atomic::AtomicBool::new(false),
            value: Mutex::new(Some(Instance {
                component,
                sequences: BTreeMap::new(),
                inputs: Vec::new(),
                outputs: Vec::new(),
                attempted: false,
            })),
        });
        let identity = super::execution::Identity {
            slot: slot.clone(),
            epoch: OperationEpoch(0),
            phase: FailurePhase::Creation,
            action: "capture component metadata",
        };
        let _ = identity.call(|| slot.take()?.refresh_configuration());
        slot
    }

    pub(super) fn configuration(&self) -> super::CapturedComponentConfiguration {
        match self.configuration.lock() {
            Ok(configuration) => configuration.clone(),
            Err(error) => {
                log::error!(
                    "Component configuration publication is poisoned: {}",
                    self.id
                );
                super::CapturedComponentConfiguration::Unavailable {
                    reason: format!(
                        "configuration publication for {} is poisoned: {error}",
                        self.id
                    ),
                }
            }
        }
    }

    pub(super) fn recovery_contract(
        &self,
    ) -> GraphResult<crate::computation::v1::ComponentRecovery> {
        self.recovery
            .lock()
            .map(|contract| contract.clone())
            .map_err(|error| {
                topology(format!(
                    "recovery contract for {} is poisoned: {error}",
                    self.id
                ))
            })
    }

    pub(super) fn query_api(&self) -> Option<std::sync::Weak<crate::computation::v1::QueryApi>> {
        self.query_api
            .lock()
            .unwrap_or_else(|error| {
                log::error!("Query API publication is poisoned: {}", self.id);
                error.into_inner()
            })
            .as_ref()
            .map(Arc::downgrade)
    }

    pub(super) fn is_constructed(self: &Arc<Self>) -> GraphResult<bool> {
        Ok(!matches!(
            self.take()?.component,
            Component::Deferred { .. } | Component::Unresolved(_)
        ))
    }

    pub(super) fn quarantine(&self, error: GraphError) -> Arc<GraphError> {
        let mut damaged = self
            .damaged
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        damaged.get_or_insert_with(|| Arc::new(error)).clone()
    }

    pub(super) fn damage(&self) -> Option<Arc<GraphError>> {
        self.damaged
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(super) fn preserve_failure(&self, error: GraphError) -> GraphError {
        if let Some(cause) = self.damage() {
            let retained = |error: &GraphError| matches!(error, GraphError::Reported { cause: current } if Arc::ptr_eq(current, &cause));
            if retained(&error)
                || matches!(&error, GraphError::Cleanup { primary: Some(primary), .. } if retained(primary))
            {
                error
            } else {
                GraphError::Cleanup {
                    primary: Some(Box::new(GraphError::Reported { cause })),
                    errors: vec![error],
                }
            }
        } else if error.damages_generation() {
            GraphError::Reported {
                cause: self.quarantine(error),
            }
        } else {
            error
        }
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

impl InstanceLease {
    fn refresh_recovery(&self) -> GraphResult<crate::computation::v1::ComponentRecovery> {
        let contract = self.component.recovery_contract();
        *self.slot.recovery.lock().map_err(|error| {
            topology(format!(
                "recovery contract for {} is poisoned: {error}",
                self.slot.id
            ))
        })? = contract.clone();
        Ok(contract)
    }

    fn refresh_configuration(&self) -> GraphResult<()> {
        self.refresh_recovery()?;
        *self.slot.query_api.lock().unwrap_or_else(|error| {
            log::error!("Query API publication is poisoned: {}", self.slot.id);
            error.into_inner()
        }) = self.component.query_api();
        let captured =
            self.component
                .capture_configuration()
                .map_err(|source| GraphError::Component {
                    component: self.slot.id.clone(),
                    operation: "capture component configuration",
                    source,
                })?;
        self.refresh_control();
        match self.slot.configuration.lock() {
            Ok(mut configuration) => *configuration = captured,
            Err(error) => {
                log::error!(
                    "Component configuration publication is poisoned: {}",
                    self.slot.id
                );
                *error.into_inner() = super::CapturedComponentConfiguration::Unavailable {
                    reason: format!("configuration publication for {} is poisoned", self.slot.id),
                };
            }
        }
        Ok(())
    }

    fn refresh_control(&self) {
        *self
            .slot
            .handler
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = self.component.control_handler();
        self.slot.requires_readiness.store(
            self.component.requires_readiness_confirmation(),
            std::sync::atomic::Ordering::Release,
        );
    }
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
        if self.value.is_none() {
            self.slot
                .query_api
                .lock()
                .unwrap_or_else(|error| {
                    log::error!("Retiring poisoned query API: {}", self.slot.id);
                    error.into_inner()
                })
                .take();
        }
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
    RetiredResources(oneshot::Sender<BTreeSet<super::ResourceId>>),
    FreezeRecovery {
        resources: BTreeSet<super::ResourceId>,
        components: BTreeSet<ComponentId>,
        allow_processing_failure: bool,
        reply: oneshot::Sender<GraphResult<super::RecoveryFreeze>>,
    },
    AutoStart,
    Add {
        addition: super::ComponentAddition,
        management_access: bool,
        reply: oneshot::Sender<GraphResult<(ComponentId, ComponentGeneration)>>,
    },
    AddComponents {
        batch: crate::computation::v1::ComponentBatch,
        management_access: bool,
        reply: oneshot::Sender<GraphResult<reconcile::ReconciliationReport>>,
    },
    ReconcileComponents {
        previous: super::DesiredTopology,
        desired: super::DesiredTopology,
        bindings: super::TopologyBindings,
        reply: oneshot::Sender<GraphResult<reconcile::ReconciliationReport>>,
    },
    ProtectComponents {
        components: BTreeSet<ComponentId>,
        resources: BTreeSet<super::ResourceId>,
        reply: oneshot::Sender<GraphResult<()>>,
    },
    ControlConnections {
        connections: Vec<(ComponentId, ComponentId)>,
        subscriptions: bool,
        management_access: bool,
        reply: oneshot::Sender<GraphResult<()>>,
    },
    ControlHandler {
        component: ComponentId,
        generation: ComponentGeneration,
        handler: Arc<dyn crate::computation::v1::ControlHandler>,
        management_access: bool,
        reply: oneshot::Sender<GraphResult<()>>,
    },
    RegisterFactory {
        factory: Arc<dyn crate::computation::v1::ComponentFactory>,
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
    StartMembers {
        members: BTreeMap<ComponentId, ComponentGeneration>,
        reply: oneshot::Sender<GraphResult<StartReport>>,
    },
    StopMembers {
        members: BTreeMap<ComponentId, ComponentGeneration>,
        reply: oneshot::Sender<GraphResult<StopReport>>,
    },
    MutateMembers {
        members: BTreeMap<ComponentId, ComponentGeneration>,
        changes: Vec<reconcile::DesiredMutation>,
        bindings: super::TopologyBindings,
        deprovision: BTreeSet<ComponentId>,
        management_access: bool,
        reply: oneshot::Sender<GraphResult<reconcile::ReconciliationReport>>,
    },
    ReadinessPolicy {
        component: ComponentId,
        generation: ComponentGeneration,
        required: bool,
        management_access: bool,
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
        management_access: bool,
        reply: oneshot::Sender<GraphResult<()>>,
    },
    Preview {
        revision: GraphRevision,
        changes: Vec<reconcile::DesiredMutation>,
        reply: oneshot::Sender<GraphResult<reconcile::ReconciliationPreview>>,
    },
    RecoveryReport {
        requirement: crate::computation::v1::RecoveryRequirement,
        reply: oneshot::Sender<GraphResult<crate::computation::v1::RecoveryPathReport>>,
    },
    Reconcile {
        preview: reconcile::ReconciliationPreview,
        bindings: super::TopologyBindings,
        management_access: bool,
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
    ForceCancel {
        revision: GraphRevision,
        selection: GraphSelection,
        reply: oneshot::Sender<GraphResult<crate::computation::v1::ForceCancellationReport>>,
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
        management_access: bool,
        reply: oneshot::Sender<GraphResult<GraphRevision>>,
    },
    Health {
        revision: GraphRevision,
        observation: HealthObservation,
        reply: oneshot::Sender<GraphResult<()>>,
    },
}

impl Command {
    fn rejected_resources(
        &self,
        graph: &ComputationGraph,
    ) -> Option<Arc<super::addition::RejectedResources>> {
        let (specifications, bindings) = match self {
            Self::AddComponents { batch, .. } => {
                (batch.definition.resources.clone(), &batch.bindings)
            }
            Self::ReconcileComponents {
                desired, bindings, ..
            } => (desired.resources.clone(), bindings),
            Self::Reconcile {
                preview, bindings, ..
            } => (preview.desired().resources.clone(), bindings),
            Self::MutateMembers {
                changes, bindings, ..
            } => {
                let mut specifications = graph.snapshot.resources.clone();
                for change in changes {
                    match change {
                        reconcile::DesiredMutation::PutResource(resource) => {
                            specifications.insert(resource.id.clone(), resource.clone());
                        }
                        reconcile::DesiredMutation::SetTopology(desired) => {
                            specifications.extend(
                                desired
                                    .resources
                                    .iter()
                                    .map(|resource| (resource.id.clone(), resource.clone())),
                            );
                        }
                        _ => {}
                    }
                }
                (specifications.into_values().collect(), bindings)
            }
            _ => return None,
        };
        super::addition::RejectedResources::new(specifications, &bindings.resources)
    }

    fn reject(self, error: GraphError) {
        match self {
            Self::Add { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::AddComponents { reply, .. }
            | Self::ReconcileComponents { reply, .. }
            | Self::MutateMembers { reply, .. }
            | Self::Reconcile { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::FreezeRecovery { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::HandleStart { reply, .. }
            | Self::StartMembers { reply, .. }
            | Self::Start { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::HandleStop { reply, .. }
            | Self::StopMembers { reply, .. }
            | Self::Stop { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::ProtectComponents { reply, .. }
            | Self::ControlConnections { reply, .. }
            | Self::ControlHandler { reply, .. }
            | Self::RegisterFactory { reply, .. }
            | Self::ReadinessPolicy { reply, .. }
            | Self::BindStream { reply, .. }
            | Self::Health { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Preview { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::RecoveryReport { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Quiesce { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::ForceCancel { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Policy { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::RetiredResources(_)
            | Self::AutoStart
            | Self::ObserveResources { .. }
            | Self::ObservePlugin { .. } => {
                log::error!("Cannot service graph notification: {error}");
            }
        }
    }
}

const COMMAND_CAPACITY: usize = 64;

#[derive(Clone)]
pub(super) struct CommandSender {
    regular: mpsc::Sender<Command>,
    construction: mpsc::Sender<Command>,
}

pub(super) struct CommandReceiver {
    regular: mpsc::Receiver<Command>,
    construction: mpsc::Receiver<Command>,
    construction_first: bool,
}

pub(super) fn command_channel() -> (CommandSender, CommandReceiver) {
    let (regular_tx, regular) = mpsc::channel(COMMAND_CAPACITY);
    let (construction_tx, construction) = mpsc::channel(COMMAND_CAPACITY);
    (
        CommandSender {
            regular: regular_tx,
            construction: construction_tx,
        },
        CommandReceiver {
            regular,
            construction,
            construction_first: false,
        },
    )
}

impl CommandSender {
    fn channel(&self, command: &Command) -> &mpsc::Sender<Command> {
        match command {
            Command::Add { .. } | Command::HandleStop { .. } => &self.construction,
            _ => &self.regular,
        }
    }

    pub(super) async fn send(
        &self,
        command: Command,
    ) -> Result<(), mpsc::error::SendError<Command>> {
        self.channel(&command).send(command).await
    }

    pub(super) fn try_send(&self, command: Command) -> Result<(), mpsc::error::TrySendError<()>> {
        self.channel(&command)
            .try_send(command)
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => mpsc::error::TrySendError::Full(()),
                mpsc::error::TrySendError::Closed(_) => mpsc::error::TrySendError::Closed(()),
            })
    }

    pub(super) async fn closed(&self) {
        self.regular.closed().await;
    }
}

impl CommandReceiver {
    fn recv(&mut self) -> impl std::future::Future<Output = Option<Command>> + '_ {
        // Alternate ready queues without allowing a busy lane to starve another.
        futures::future::poll_fn(move |cx| {
            let (first, second) = if self.construction_first {
                (&mut self.construction, &mut self.regular)
            } else {
                (&mut self.regular, &mut self.construction)
            };
            let first_closed = match first.poll_recv(cx) {
                Poll::Ready(Some(command)) => {
                    self.construction_first = !self.construction_first;
                    return Poll::Ready(Some(command));
                }
                Poll::Ready(None) => true,
                Poll::Pending => false,
            };
            match second.poll_recv(cx) {
                Poll::Ready(Some(command)) => Poll::Ready(Some(command)),
                Poll::Ready(None) if first_closed => Poll::Ready(None),
                _ => Poll::Pending,
            }
        })
    }

    // Mutations wait in their bounded channel, not an unbounded side queue.
    // Independent additions and generation-fenced stops remain serviceable.
    async fn recv_during_construction(&mut self) -> Option<Command> {
        self.construction.recv().await
    }
}

fn configuration_access(graph: &ComputationGraph, management_access: bool) -> GraphResult<()> {
    if graph.retirement_active() {
        return Err(GraphError::OperationInProgress);
    }
    if !management_access
        && graph
            .management_protected
            .load(std::sync::atomic::Ordering::Acquire)
    {
        return Err(topology(
            "managed configuration must be changed through DrasiLib::apply_desired_state",
        ));
    }
    Ok(())
}

fn component_configuration_access(
    graph: &ComputationGraph,
    management_access: bool,
    id: &ComponentId,
) -> GraphResult<()> {
    configuration_access(graph, management_access)?;
    if !management_access && graph.protected_components.contains(id) {
        return Err(topology(format!("component {id} is managed by DrasiLib")));
    }
    Ok(())
}

fn addition_id(pending: &Option<super::ComponentAddition>) -> &ComponentId {
    pending
        .as_ref()
        .expect("pending component addition")
        .definition
        .descriptor
        .id()
}

impl GraphControl {
    /// Explicitly interrupt selected work without a drain/rollback promise.
    /// The selection must include every registered user of the same actual
    /// resources. Interrupted generations remain quarantined after cleanup.
    /// This cannot preempt nonyielding synchronous code or a native fault.
    pub async fn force_cancel_components(
        &self,
        revision: GraphRevision,
        selection: GraphSelection,
    ) -> GraphResult<crate::computation::v1::ForceCancellationReport> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::ForceCancel {
                revision,
                selection,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub(crate) async fn retired_resources(&self) -> GraphResult<BTreeSet<super::ResourceId>> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::RetiredResources(reply))
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)
    }

    pub(crate) async fn freeze_recovery(
        &self,
        resources: BTreeSet<super::ResourceId>,
        components: BTreeSet<ComponentId>,
        allow_processing_failure: bool,
    ) -> GraphResult<super::RecoveryFreeze> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::FreezeRecovery {
                resources,
                components,
                allow_processing_failure,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub async fn recovery_report(
        &self,
        requirement: crate::computation::v1::RecoveryRequirement,
    ) -> GraphResult<crate::computation::v1::RecoveryPathReport> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::RecoveryReport { requirement, reply })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        response.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub(crate) async fn start_members(
        &self,
        members: BTreeMap<ComponentId, ComponentGeneration>,
    ) -> GraphResult<StartReport> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::StartMembers { members, reply })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        receive.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub(crate) async fn stop_members(
        &self,
        members: BTreeMap<ComponentId, ComponentGeneration>,
    ) -> GraphResult<StopReport> {
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::StopMembers { members, reply })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        receive.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub(crate) async fn mutate_members(
        &self,
        members: BTreeMap<ComponentId, ComponentGeneration>,
        changes: Vec<reconcile::DesiredMutation>,
        bindings: super::TopologyBindings,
        deprovision: BTreeSet<ComponentId>,
    ) -> GraphResult<reconcile::ReconciliationReport> {
        self.require_configuration_write()?;
        let (reply, receive) = oneshot::channel();
        self.commands
            .send(Command::MutateMembers {
                members,
                changes,
                bindings,
                deprovision,
                management_access: self.management_access,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        receive.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub(crate) async fn reconcile_components(
        &self,
        previous: super::DesiredTopology,
        desired: super::DesiredTopology,
        bindings: super::TopologyBindings,
    ) -> GraphResult<reconcile::ReconciliationReport> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::ReconcileComponents {
                previous,
                desired,
                bindings,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub(crate) async fn add_components(
        &self,
        batch: crate::computation::v1::ComponentBatch,
    ) -> GraphResult<reconcile::ReconciliationReport> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::AddComponents {
                batch,
                management_access: self.management_access,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub(crate) async fn protect_components(
        &self,
        components: BTreeSet<ComponentId>,
        resources: BTreeSet<super::ResourceId>,
    ) -> GraphResult<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::ProtectComponents {
                components,
                resources,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

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

    /// Make a construction implementation available to waiting declarations.
    /// An existing identity may only be registered again with the same factory.
    pub async fn register_factory(
        &self,
        factory: Arc<dyn crate::computation::v1::ComponentFactory>,
    ) -> GraphResult<()> {
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::RegisterFactory { factory, reply })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
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

    /// Start selected components explicitly, independently of auto-start defaults.
    pub async fn start_requested(
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
        self.require_configuration_write()?;
        let (reply, result) = oneshot::channel();
        self.commands
            .send(Command::Policy {
                revision,
                component,
                policy,
                management_access: self.management_access,
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
                        waiting_for: Vec::new(),
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

pub(super) fn failure(error: GraphError, phase: FailurePhase) -> ComponentFailure {
    ComponentFailure {
        phase,
        disposition: error.failure_disposition(),
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

fn creation_dependencies(
    graph: &ComputationGraph,
    index: usize,
) -> Vec<super::super::DependencyWait> {
    use super::super::DependencyWait;
    let id = graph.nodes[index].descriptor.id();
    let resources: BTreeSet<_> = graph
        .snapshot
        .specifications
        .get(id)
        .into_iter()
        .flat_map(|specification| {
            specification.dependencies.values().flatten().chain(
                specification
                    .configuration
                    .values()
                    .filter_map(|value| match value {
                        super::ConfigurationValue::Reference { resource, .. } => Some(resource),
                        _ => None,
                    }),
            )
        })
        .filter(|resource| !graph.resource_handles.contains_key(*resource))
        .cloned()
        .collect();
    let observed = graph.observed();
    let dependencies: BTreeSet<_> = graph
        .snapshot
        .edges
        .iter()
        .filter(|edge| edge.policy.required_for_creation && &edge.definition.to.component == id)
        .map(|edge| &edge.definition.from.component)
        .chain(
            graph
                .snapshot
                .subscriptions
                .iter()
                .filter(|(_, to)| to == id)
                .map(|(from, _)| from),
        )
        .filter(|dependency| {
            !observed.components.get(*dependency).is_some_and(|node| {
                node.realization == RealizationState::Created && node.failure.is_none()
            })
        })
        .cloned()
        .collect();
    let factory = graph
        .snapshot
        .specifications
        .get(id)
        .filter(|specification| !graph.factories.contains_key(&specification.implementation));
    dependencies
        .into_iter()
        .map(|id| DependencyWait::Component { id })
        .chain(
            resources
                .into_iter()
                .map(|id| DependencyWait::Resource { id }),
        )
        .chain(factory.map(|specification| DependencyWait::Factory {
            implementation: specification.implementation.clone(),
        }))
        .collect()
}

fn binding_waits(graph: &ComputationGraph, id: &ComponentId) -> Vec<super::super::DependencyWait> {
    use super::super::{DependencyWait, Endpoint, PortDirection};
    let node = &graph.nodes[graph.ids[id]];
    let observed = graph.observed();
    let mut waiting = Vec::new();
    for port in node.descriptor.ports() {
        let endpoint = Endpoint::new(id.clone(), port.id().clone());
        let connected = graph.snapshot.edges.iter().any(|edge| {
            (edge.definition.from == endpoint || edge.definition.to == endpoint)
                && observed.relationships[&edge.definition].binding == BindingState::Bound
        });
        let optional_input =
            port.direction() == PortDirection::Input
                && graph.snapshot.unbound_relationships.iter().any(|edge| {
                    edge.definition.to == endpoint && !edge.policy.required_for_binding
                });
        if !connected && !optional_input {
            waiting.push(DependencyWait::Connection {
                endpoint: endpoint.clone(),
            });
        }
        if port.direction() == PortDirection::Output && !node.output_streams.contains_key(port.id())
        {
            waiting.push(DependencyWait::Stream { endpoint });
        }
    }
    waiting
}

async fn deploy(
    graph: &mut ComputationGraph,
    cancel: &mut watch::Receiver<bool>,
    operations: &mut Operations,
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
    let mut creation_blocks = BTreeMap::new();
    let mut pending: BTreeSet<_> = graph.order.iter().copied().collect();
    let empty_controls = PipeGuard(BTreeMap::new());
    while !pending.is_empty() || !operations.active.is_empty() {
        for &index in &graph.order {
            if !pending.contains(&index) {
                continue;
            }
            let slot = &graph.components[index];
            let required: Vec<_> = graph
                .snapshot
                .edges
                .iter()
                .filter(|edge| {
                    edge.policy.required_for_creation && edge.definition.to.component == slot.id
                })
                .map(|edge| graph.ids[&edge.definition.from.component])
                .collect();
            if required.iter().any(|dependency| {
                pending.contains(dependency) || operations.active.contains_key(dependency)
            }) {
                continue;
            }
            pending.remove(&index);
            if required
                .iter()
                .any(|dependency| unavailable.contains(dependency))
            {
                unavailable.insert(index);
                let waiting = creation_dependencies(graph, index);
                let blocked = operations.defer_creation(graph, index, Operation::Create, waiting);
                creation_blocks.insert(index, blocked);
                continue;
            }
            if let Some(cause) = slot.damage() {
                unavailable.insert(index);
                creation_failures.insert(
                    index,
                    failure(GraphError::Reported { cause }, FailurePhase::Creation),
                );
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
            let waiting = creation_dependencies(graph, index);
            if !waiting.is_empty() {
                unavailable.insert(index);
                let blocked = operations.defer_creation(graph, index, Operation::Create, waiting);
                creation_blocks.insert(index, blocked);
                continue;
            }
            if let Err(error) = operations.launch(graph, index, Operation::Create) {
                unavailable.insert(index);
                creation_failures.insert(index, failure(error, FailurePhase::Creation));
            }
        }
        if operations.futures.is_empty() {
            if pending.is_empty() {
                break;
            }
            return Err(topology(
                "component construction dependencies could not advance",
            ));
        }
        let completion = tokio::select! {
            biased;
            _ = cancelled(cancel) => return Err(GraphError::Cancelled),
            completion = operations.futures.next() => completion.expect("owned creation task"),
        };
        let index = completion.index;
        operations.complete(graph, completion, &empty_controls)?;
        if let Some(failure) = graph.observed().components[graph.nodes[index].descriptor.id()]
            .failure
            .as_ref()
        {
            unavailable.insert(index);
            creation_failures.insert(index, failure.clone());
        }
    }
    let mut controls = PipeGuard(BTreeMap::new());
    let unconstructed = unavailable.clone();
    for (&edge_index, edge) in &graph.edges {
        let from = graph.ids[&edge.definition.from.component];
        let to = graph.ids[&edge.definition.to.component];
        if unconstructed.contains(&from)
            || unconstructed.contains(&to)
            || edge
                .resources
                .keys()
                .any(|id| !graph.resource_handles.contains_key(id))
        {
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
        let constructed = construct_pipe(graph, edge_index, edge)?;
        let provided = reconcile::drive(graph, operations, &controls, cancel, constructed)
            .await
            .and_then(|provided| install_pipe(graph, &mut controls, edge_index, edge, provided));
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
        let waiting = binding_waits(graph, id);
        let blocked = unavailable.contains(&index) || !waiting.is_empty();
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
            if !creation_blocks.contains_key(&index) {
                observed.waiting_for = waiting;
            }
            observed.exhausted = false;
            observed.transition_time = Utc::now();
        });
        outcomes.insert(
            id.clone(),
            if let Some(failure) = creation_failure {
                CreationOutcome::CreationFailed(failure.clone())
            } else if let Some(blocked) = creation_blocks.get(&index) {
                blocked.clone()
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
        summary: if outcomes
            .values()
            .all(|outcome| matches!(outcome, CreationOutcome::Created))
        {
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
    Reconfigure,
    Start,
    Process,
    Stop,
}

impl Operation {
    fn phase(self) -> FailurePhase {
        match self {
            Self::Create | Self::Reconfigure => FailurePhase::Creation,
            Self::Start => FailurePhase::Activation,
            Self::Process => FailurePhase::Processing,
            Self::Stop => FailurePhase::Stop,
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Reconfigure => "reconfigure",
            Self::Start => "start",
            Self::Process => "process",
            Self::Stop => "stop",
        }
    }
}

struct Active {
    epoch: OperationEpoch,
    operation: Operation,
    abort: AbortHandle,
    quiesce: watch::Sender<bool>,
    outputs: Vec<(usize, Arc<super::FlowProgress>)>,
    reply: Option<oneshot::Sender<GraphResult<()>>>,
}

pub(super) struct Completion {
    index: usize,
    generation: ComponentGeneration,
    epoch: OperationEpoch,
    operation: Operation,
    result: Result<GraphResult<()>, Aborted>,
}

struct Starting {
    revision: GraphRevision,
    selected: BTreeSet<usize>,
    pending: BTreeSet<usize>,
    outcomes: BTreeMap<ComponentId, StartOutcome>,
    reply: Option<oneshot::Sender<GraphResult<StartReport>>>,
    force: bool,
}

struct Stopping {
    revision: GraphRevision,
    selected: BTreeSet<usize>,
    pending: BTreeSet<usize>,
    outcomes: BTreeMap<ComponentId, StopOutcome>,
    reply: StopReply,
}

enum StopReply {
    Graceful(oneshot::Sender<GraphResult<StopReport>>),
    Forced {
        interrupted: BTreeMap<ComponentId, ComponentFailure>,
        reply: oneshot::Sender<GraphResult<crate::computation::v1::ForceCancellationReport>>,
    },
}

impl StopReply {
    fn send(self, cleanup: StopReport) {
        match self {
            Self::Graceful(reply) => {
                let _ = reply.send(Ok(cleanup));
            }
            Self::Forced { interrupted, reply } => {
                let _ = reply.send(Ok(crate::computation::v1::ForceCancellationReport {
                    revision: cleanup.revision,
                    summary: if interrupted.is_empty() {
                        cleanup.summary
                    } else {
                        OperationSummary::CompletedWithFailures
                    },
                    interrupted,
                    cleanup,
                }));
            }
        }
    }
}

enum DrainReply {
    Stop(oneshot::Sender<GraphResult<StopReport>>),
    Quiesce(oneshot::Sender<GraphResult<Vec<ComponentId>>>),
}

impl DrainReply {
    fn reject(self, error: GraphError) {
        match self {
            Self::Stop(reply) => {
                let _ = reply.send(Err(error));
            }
            Self::Quiesce(reply) => {
                let _ = reply.send(Err(error));
            }
        }
    }
}

struct Draining {
    id: u64,
    selected: BTreeSet<usize>,
    pending: BTreeSet<usize>,
    waiting: BTreeMap<usize, AbortHandle>,
    deadline: tokio::time::Instant,
    reply: DrainReply,
}

pub(super) struct DrainCompletion {
    group: u64,
    index: usize,
    result: GraphResult<()>,
}

/// A single-component start or stop waiting behind a conflicting lifecycle
/// operation. Requests for the same component begin in arrival order.
enum HandleRequest {
    Start {
        component: ComponentId,
        generation: ComponentGeneration,
        reply: oneshot::Sender<GraphResult<StartReport>>,
    },
    Stop {
        component: ComponentId,
        generation: ComponentGeneration,
        reply: oneshot::Sender<GraphResult<StopReport>>,
    },
    StopMembers {
        members: BTreeMap<ComponentId, ComponentGeneration>,
        reply: oneshot::Sender<GraphResult<StopReport>>,
    },
}

impl HandleRequest {
    fn components(&self) -> BTreeSet<ComponentId> {
        match self {
            Self::Start { component, .. } | Self::Stop { component, .. } => {
                BTreeSet::from([component.clone()])
            }
            Self::StopMembers { members, .. } => members.keys().cloned().collect(),
        }
    }

    fn abandoned(&self) -> bool {
        match self {
            Self::Start { reply, .. } => reply.is_closed(),
            Self::Stop { reply, .. } | Self::StopMembers { reply, .. } => reply.is_closed(),
        }
    }

    /// A stop may interrupt an in-flight start; a start needs the controller
    /// free of both.
    fn ready(&self, graph: &ComputationGraph, operations: &Operations) -> bool {
        let selected = self
            .components()
            .iter()
            .filter_map(|id| graph.ids.get(id).copied())
            .collect();
        match self {
            Self::Start { .. } => !operations.conflicts(&selected, true),
            Self::Stop { .. } | Self::StopMembers { .. } => !operations.conflicts(&selected, false),
        }
    }

    fn validate(&self, graph: &ComputationGraph) -> GraphResult<BTreeSet<usize>> {
        if let Self::StopMembers { members, .. } = self {
            return member_indices(graph, members);
        }
        let (component, generation) = match self {
            Self::Start {
                component,
                generation,
                ..
            }
            | Self::Stop {
                component,
                generation,
                ..
            } => (component, *generation),
            Self::StopMembers { .. } => unreachable!("members validated above"),
        };
        let index = graph
            .ids
            .get(component)
            .filter(|index| graph.components[**index].generation == generation)
            .copied()
            .ok_or(GraphError::StaleGeneration)?;
        if matches!(self, Self::Start { .. }) && graph.retirement_active() {
            return Err(GraphError::OperationInProgress);
        }
        Ok(BTreeSet::from([index]))
    }

    fn reject(self, error: GraphError) {
        match self {
            Self::Start { reply, .. } => {
                let _ = reply.send(Err(error));
            }
            Self::Stop { reply, .. } | Self::StopMembers { reply, .. } => {
                let _ = reply.send(Err(error));
            }
        }
    }

    async fn begin(
        self,
        graph: &mut ComputationGraph,
        operations: &mut Operations,
        _controls: &PipeGuard,
        _cancel: &mut watch::Receiver<bool>,
    ) -> GraphResult<()> {
        let selected = match self.validate(graph) {
            Ok(selected) => selected,
            Err(error) => {
                self.reject(error);
                return Ok(());
            }
        };
        match self {
            Self::Start {
                component, reply, ..
            } => {
                graph.deferred_activation.remove(&component);
                operations.begin_start(graph, selected, Some(reply), true);
                Ok(())
            }
            Self::Stop {
                component, reply, ..
            } => {
                graph.deferred_activation.insert(component);
                operations.begin_drain(graph, selected, DrainReply::Stop(reply));
                Ok(())
            }
            Self::StopMembers { members, reply } => {
                graph.deferred_activation.extend(members.into_keys());
                operations.begin_drain(graph, selected, DrainReply::Stop(reply));
                Ok(())
            }
        }
    }
}

pub(super) struct ControlCompletion {
    component: ComponentId,
    generation: ComponentGeneration,
    result: Result<anyhow::Result<()>, Aborted>,
    next: Option<PeerDriver>,
}

#[derive(Default)]
pub(super) struct ExecutionTasks {
    nodes: Arc<super::execution::Tasks<Completion>>,
    controls: Arc<super::execution::Tasks<ControlCompletion>>,
    drains: Arc<super::execution::Tasks<DrainCompletion>>,
    pub(super) resources: super::resource_tasks::ResourceTasks,
    preparations: Arc<super::execution::Tasks<GraphResult<reconcile::Prepared>>>,
    pipes: Arc<super::execution::Tasks<GraphResult<ProvidedBinding>>>,
    retirements: Arc<super::execution::Tasks<GraphResult<()>>>,
}

impl ExecutionTasks {
    pub(super) fn cancel(&self) {
        self.nodes.cancel_processing();
        self.controls.cancel();
        self.drains.cancel();
        self.resources.cancel_construction();
        self.preparations.cancel();
        self.pipes.cancel();
    }

    pub(super) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.controls.is_empty()
            && self.drains.is_empty()
            && self.resources.is_empty()
            && self.preparations.is_empty()
            && self.pipes.is_empty()
            && self.retirements.is_empty()
    }
}

struct ProvidedBinding {
    control: Option<Arc<dyn crate::computation::v1::PipeControl>>,
    outgoing: Option<Outgoing>,
    incoming: Option<Incoming>,
}

impl Drop for ProvidedBinding {
    fn drop(&mut self) {
        if let Some(control) = &self.control {
            control.cancel();
        }
    }
}

fn construct_pipe(
    graph: &ComputationGraph,
    index: usize,
    edge: &EdgeSnapshot,
) -> GraphResult<BoxFuture<'static, GraphResult<ProvidedBinding>>> {
    let provider = graph.providers[&index].clone();
    let resources = graph.resource_handles.clone();
    let edge = edge.clone();
    let identity = super::execution::GraphIdentity {
        graph: graph.snapshot.id.clone(),
        run_epoch: graph.observed().run_epoch,
        phase: FailurePhase::Binding,
    };
    let (abort, registration) = AbortHandle::new_pair();
    let joined =
        graph
            .tasks
            .pipes
            .spawn(identity.clone(), abort, async move {
                identity
                    .run(async move {
                        Abortable::new(
                            async move {
                                let mut provided = provider
                                    .create_with_resources(&resources)
                                    .map_err(|source| GraphError::Pipe {
                                        edge: index,
                                        source,
                                    })?;
                                let mut controls =
                                    PipeGuard(BTreeMap::from([(index, provided.control.clone())]));
                                if provided.pipe.capabilities() != &edge.capabilities {
                                    return Err(topology(
                                        "created pipe differs from preflight capabilities",
                                    ));
                                }
                                let receiver = provided.pipe.take_receiver().map_err(|source| {
                                    GraphError::Pipe {
                                        edge: index,
                                        source,
                                    }
                                })?;
                                let progress = Arc::new(super::FlowProgress::default());
                                let outgoing = Outgoing {
                                    edge: index,
                                    port: edge.definition.from.port.clone(),
                                    sender: provided.pipe.sender(),
                                    progress: progress.clone(),
                                    multicast: provider
                                        .multicast_subscription()
                                        .map(|(group, _)| group),
                                    admission_channel: super::admission_channel(
                                        provider.as_ref(),
                                        &resources,
                                    )?,
                                    destination: super::output_destination(
                                        &edge.definition,
                                        &edge.pipe,
                                        &resources,
                                    )
                                    .map_err(|error| GraphError::Pipe {
                                        edge: index,
                                        source: crate::computation::v1::PipeError::Backend(error),
                                    })?,
                                };
                                let incoming = Incoming {
                    edge: index, port: edge.definition.to.port.clone(), receiver,
                    acknowledgement_required: edge.capabilities.supported()
                        .contains(&crate::computation::v1::PipeCapability::ExplicitAcknowledgement),
                    pending: None, exhausted: false, progress,
                };
                                drop(provided);
                                Ok(ProvidedBinding {
                                    control: controls.0.remove(&index),
                                    outgoing: Some(outgoing),
                                    incoming: Some(incoming),
                                })
                            },
                            registration,
                        )
                        .await
                        .unwrap_or(Err(GraphError::Cancelled))
                    })
                    .await
            })?;
    Ok(async move { joined.await? }.boxed())
}

fn install_pipe(
    graph: &ComputationGraph,
    controls: &mut PipeGuard,
    index: usize,
    edge: &EdgeSnapshot,
    mut provided: ProvidedBinding,
) -> GraphResult<()> {
    let from = &graph.components[graph.ids[&edge.definition.from.component]];
    let to = &graph.components[graph.ids[&edge.definition.to.component]];
    if let Some(cause) = from.damage().or_else(|| to.damage()) {
        return Err(GraphError::Reported { cause });
    }
    let mut from = from.take()?;
    let mut to = to.take()?;
    controls
        .0
        .insert(index, provided.control.take().expect("owned pipe control"));
    from.outputs
        .push(provided.outgoing.take().expect("owned pipe output"));
    to.inputs
        .push(provided.incoming.take().expect("owned pipe input"));
    Ok(())
}

struct RetirementCompletion {
    slot: Option<Arc<InstanceSlot>>,
    result: GraphResult<()>,
}

fn retire_callback(
    graph: &ComputationGraph,
    operations: &mut Operations,
    slot: Option<Arc<InstanceSlot>>,
    call: impl FnOnce(&super::execution::TaskIdentity) -> GraphResult<()> + Send + 'static,
) -> GraphResult<()> {
    let identity: super::execution::TaskIdentity = match &slot {
        Some(slot) => super::execution::Identity {
            slot: slot.clone(),
            epoch: graph.observed().components[&slot.id].operation,
            phase: FailurePhase::Removal,
            action: "retire resource metadata",
        }
        .into(),
        None => super::execution::GraphIdentity {
            graph: graph.snapshot.id.clone(),
            run_epoch: graph.observed().run_epoch,
            phase: FailurePhase::Removal,
        }
        .into(),
    };
    let (abort, _registration) = AbortHandle::new_pair();
    let joined = graph
        .tasks
        .retirements
        .spawn(identity.clone(), abort, async move {
            let callback_identity = identity.clone();
            identity.run(async move { call(&callback_identity) }).await
        })?;
    operations.retirements.push(
        async move {
            RetirementCompletion {
                slot,
                result: joined.await.and_then(|result| result),
            }
        }
        .boxed(),
    );
    Ok(())
}

fn drop_resource_handles(
    identity: &super::execution::TaskIdentity,
    handles: impl IntoIterator<Item = super::ResourceHandle>,
) -> GraphResult<()> {
    let errors: Vec<_> = handles
        .into_iter()
        .filter_map(|handle| {
            identity
                .call(|| {
                    drop(handle);
                    Ok(())
                })
                .err()
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(GraphError::Cleanup {
            primary: None,
            errors,
        })
    }
}

fn retire_metadata(
    graph: &ComputationGraph,
    operations: &mut Operations,
    command: Command,
) -> GraphResult<()> {
    let slot = match &command {
        Command::ObserveResources {
            component,
            generation,
            ..
        }
        | Command::ObservePlugin {
            component,
            generation,
            ..
        } => graph
            .ids
            .get(component)
            .map(|index| graph.components[*index].clone())
            .filter(|slot| slot.generation == *generation),
        _ => None,
    };
    retire_callback(graph, operations, slot, move |identity| match command {
        Command::ObserveResources { bindings, .. } => {
            drop_resource_handles(identity, bindings.into_iter().map(|(_, handle)| handle))
        }
        command => identity.call(|| {
            drop(command);
            Ok(())
        }),
    })
}

async fn flush_metadata(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    commands: &mut CommandReceiver,
) -> GraphResult<()> {
    for (_, command) in std::mem::take(&mut graph.pending_metadata) {
        handle_command(graph, operations, controls, cancel, command, commands).await?;
    }
    Ok(())
}

struct PeerDriver {
    plane: Arc<crate::computation::v1::control::ControlPlane>,
    control: crate::computation::v1::ComponentControl,
    inbox: mpsc::Receiver<crate::computation::v1::PeerMessage>,
    handler: watch::Receiver<Option<Arc<dyn crate::computation::v1::ControlHandler>>>,
}

impl PeerDriver {
    async fn handle_next(&mut self) -> anyhow::Result<bool> {
        loop {
            let mut status_changes = self.plane.subscribe();
            let message = loop {
                if let Some(message) = self
                    .plane
                    .take_status(self.control.id(), self.control.generation())?
                {
                    break message;
                }
                tokio::select! {
                    message = self.inbox.recv() => match message {
                        Some(message) => break message,
                        None => return Ok(false),
                    },
                    changed = status_changes.changed() => { changed?; }
                }
            };
            let mut changes = self.plane.subscribe();
            if let Err(error) = self.plane.validate_delivery(&message) {
                log::debug!(
                    "Discarding obsolete peer notification for {}: {error}",
                    self.control.id()
                );
                continue;
            }
            let callback = self.handler.borrow().clone();
            if let Some(callback) = callback {
                let pending = callback.on_message(message.clone(), self.control.clone());
                tokio::pin!(pending);
                loop {
                    tokio::select! {
                        biased;
                        changed = changes.changed() => {
                            if changed.is_err() || self.plane.validate_delivery(&message).is_err() {
                                return Ok(true);
                            }
                        }
                        result = &mut pending => {
                            result?;
                            return Ok(true);
                        }
                    }
                }
            } else if matches!(
                message.notification,
                crate::computation::v1::ControlNotification::Custom { .. }
            ) {
                anyhow::bail!(
                    "component {} has no handler for a custom control message",
                    self.control.id()
                );
            }
            return Ok(true);
        }
    }

    async fn advance(
        mut self,
        registration: AbortRegistration,
        identity: super::execution::Identity,
    ) -> ControlCompletion {
        let component = self.control.id().clone();
        let generation = self.control.generation();
        let result = identity
            .run(async {
                Abortable::new(
                    async {
                        while self
                            .handle_next()
                            .await
                            .map_err(|error| identity.error(error))?
                        {}
                        Ok(())
                    },
                    registration,
                )
                .await
                .unwrap_or(Err(GraphError::Cancelled))
            })
            .await;
        let next = result
            .as_ref()
            .is_err_and(|error| !error.damages_generation())
            && identity.slot.damage().is_none();
        ControlCompletion {
            component,
            generation,
            result: Ok(result.map_err(anyhow::Error::new)),
            next: next.then_some(self),
        }
    }
}

#[derive(Default)]
struct Operations {
    futures: FuturesUnordered<BoxFuture<'static, Completion>>,
    active: BTreeMap<usize, Active>,
    starting: Vec<Starting>,
    suspended_starts: BTreeSet<usize>,
    stopping: Vec<Stopping>,
    draining: Vec<Draining>,
    next_drain: u64,
    drain_futures: FuturesUnordered<BoxFuture<'static, DrainCompletion>>,
    /// Handle starts and stops waiting for a conflicting operation, in arrival order.
    handle_requests: VecDeque<HandleRequest>,
    stop_after_abort: BTreeSet<usize>,
    propagated_stop: BTreeSet<usize>,
    exhausted: BTreeSet<usize>,
    quiescing: BTreeSet<usize>,
    paused: BTreeSet<usize>,
    resume_after_quiesce: BTreeSet<usize>,
    invalidated_bindings: BTreeSet<usize>,
    control_futures: FuturesUnordered<BoxFuture<'static, ControlCompletion>>,
    control_handlers:
        BTreeMap<usize, watch::Sender<Option<Arc<dyn crate::computation::v1::ControlHandler>>>>,
    peer_controls: BTreeMap<usize, crate::computation::v1::ComponentControl>,
    control_aborts: BTreeMap<usize, AbortHandle>,
    control_quiescing: BTreeSet<usize>,
    parked_controls: BTreeMap<usize, PeerDriver>,
    pending_creation: BTreeMap<usize, Operation>,
    creation_revision: Option<GraphRevision>,
    creation_dirty: bool,
    resource_revision: Option<GraphRevision>,
    resources_dirty: bool,
    binding_revision: Option<GraphRevision>,
    bindings_dirty: bool,
    pending_auto_start: BTreeMap<usize, bool>,
    connection_generations:
        BTreeMap<(ComponentId, ComponentId), BTreeMap<super::EdgeDefinition, u64>>,
    reconciliation: Option<ReconciliationReservation>,
    retirements: FuturesUnordered<BoxFuture<'static, RetirementCompletion>>,
    retiring: BTreeSet<usize>,
}

struct ReconciliationReservation {
    components: BTreeSet<ComponentId>,
    resources: BTreeSet<super::ResourceId>,
    resource_instances: Vec<super::specification::ResourceScopeIdentity>,
    committed: bool,
}

impl ReconciliationReservation {
    fn permits(&self, graph: &ComputationGraph, command: &Command) -> bool {
        let disjoint = |selection: &GraphSelection| {
            select(graph, selection).map_or(true, |selected| {
                selected.iter().all(|index| {
                    !self
                        .components
                        .contains(graph.nodes[*index].descriptor.id())
                })
            })
        };
        match command {
            Command::RetiredResources(_)
            | Command::RecoveryReport { .. }
            | Command::Preview { .. }
            | Command::Health { .. }
            | Command::ObserveResources { .. }
            | Command::ObservePlugin { .. }
            | Command::AutoStart => true,
            Command::ForceCancel { .. } => true,
            Command::Start { selection, .. }
            | Command::Stop { selection, .. }
            | Command::Quiesce { selection, .. } => disjoint(selection),
            Command::HandleStart { component, .. } | Command::ControlHandler { component, .. } => {
                !self.components.contains(component)
            }
            Command::HandleStop { component, .. } => {
                !self.components.contains(component)
                    || self.committed
                        && graph
                            .observed()
                            .components
                            .get(component)
                            .is_some_and(|node| node.realization == RealizationState::Creating)
            }
            Command::StartMembers { members, .. } | Command::StopMembers { members, .. } => {
                members.keys().all(|id| !self.components.contains(id))
            }
            Command::Add { addition, .. } => {
                let reserved = |id: &super::ResourceId| {
                    self.resources.contains(id)
                        || graph.resource_handles.get(id).is_some_and(|handle| {
                            self.resource_instances
                                .iter()
                                .any(|identity| identity.matches(handle))
                        })
                };
                let dependencies_clear = match &addition.definition.construction {
                    super::ComponentConstruction::Factory(specification) =>
                        specification.dependencies.values().flatten().all(|id| !reserved(id))
                            && specification.configuration.values().all(|value|
                                !matches!(value, super::ConfigurationValue::Reference { resource, .. } if reserved(resource))),
                    super::ComponentConstruction::External { .. } => true,
                };
                self.committed
                    && !self
                        .components
                        .contains(addition.definition.descriptor.id())
                    && addition
                        .resources
                        .iter()
                        .all(|resource| !self.resources.contains(&resource.id))
                    && addition.bindings.resources.keys().all(|id| !reserved(id))
                    && dependencies_clear
                    && (!self
                        .resources
                        .iter()
                        .any(|id| graph.tasks.resources.contains(id))
                        || addition.bindings.resources.is_empty())
                    && addition.bindings.resources.values().all(|handle| {
                        !self
                            .resource_instances
                            .iter()
                            .any(|identity| identity.matches(handle))
                            && !graph.resource_handles.iter().any(|(id, current)| {
                                self.resources.contains(id)
                                    && (current.same_instance(handle)
                                        || super::resources::same_provider(current, handle))
                            })
                    })
            }
            _ => false,
        }
    }
}

impl Operations {
    fn dependencies_changed(&mut self) {
        self.creation_dirty = true;
        self.resources_dirty = true;
        self.bindings_dirty = true;
    }

    fn complete_retirement(
        &mut self,
        graph: &ComputationGraph,
        completion: RetirementCompletion,
    ) -> GraphResult<()> {
        let RetirementCompletion { slot, result } = completion;
        if let Err(error) = &result {
            log::error!("Retained retirement failed: {error}");
            let mut affected = self.retiring.clone();
            if let Some(slot) = slot {
                if let Some(index) = graph
                    .ids
                    .get(&slot.id)
                    .filter(|index| Arc::ptr_eq(&graph.components[**index], &slot))
                {
                    affected.insert(*index);
                }
            }
            for index in &affected {
                if graph.components[*index].damage().is_some() {
                    if let Some(active) = self.active.get(index) {
                        if active.operation != Operation::Stop {
                            active.abort.abort();
                            self.stop_after_abort.insert(*index);
                        }
                    }
                    if let Some(abort) = self.control_aborts.get(index) {
                        abort.abort();
                    }
                    self.propagated_stop.insert(*index);
                }
            }
            update(graph, |state| {
                for index in &affected {
                    let slot = &graph.components[*index];
                    if let Some(cause) = slot.damage() {
                        if let Some(node) = state.components.get_mut(&slot.id) {
                            node.failure = Some(failure(
                                GraphError::Reported { cause },
                                FailurePhase::Removal,
                            ));
                            node.lifecycle = ComponentLifecycle::Failed;
                            node.health = ComponentHealth::Unavailable;
                        }
                    }
                }
            });
            for index in affected {
                if graph.components[index].damage().is_some() {
                    if let Err(error) = self.propagate(graph, index) {
                        log::error!("Cannot propagate retirement quarantine: {error}");
                    }
                }
            }
        }
        result
    }

    fn conflicts(&self, selected: &BTreeSet<usize>, starts: bool) -> bool {
        !selected.is_disjoint(&self.retiring)
            || (starts
                && self
                    .starting
                    .iter()
                    .any(|group| !group.selected.is_disjoint(selected)))
            || self
                .stopping
                .iter()
                .any(|group| !group.selected.is_disjoint(selected))
            || self
                .draining
                .iter()
                .any(|group| !group.selected.is_disjoint(selected))
    }

    fn begin_drain(
        &mut self,
        graph: &ComputationGraph,
        selected: BTreeSet<usize>,
        reply: DrainReply,
    ) {
        if self.conflicts(&selected, false) {
            reply.reject(GraphError::OperationInProgress);
            return;
        }
        let Some(next) = self.next_drain.checked_add(1) else {
            reply.reject(topology("drain operation identity exhausted"));
            return;
        };
        let Some(deadline) = tokio::time::Instant::now().checked_add(graph.cleanup_timeout) else {
            reply.reject(topology("drain deadline is no longer representable"));
            return;
        };
        self.next_drain = next;
        self.suspended_starts.extend(&selected);
        if matches!(reply, DrainReply::Stop(_)) {
            for index in &selected {
                self.pending_auto_start.remove(index);
                self.supersede_queued_starts(graph.nodes[*index].descriptor.id());
            }
        }
        self.draining.push(Draining {
            id: next,
            pending: selected.clone(),
            selected,
            waiting: BTreeMap::new(),
            deadline,
            reply,
        });
    }

    fn next_drain_deadline(&self) -> Option<tokio::time::Instant> {
        self.draining.iter().map(|group| group.deadline).min()
    }

    fn pause_for_drain(&mut self, graph: &ComputationGraph, index: usize) {
        if let Some(active) = self
            .active
            .get(&index)
            .filter(|active| active.operation == Operation::Process)
        {
            self.quiescing.insert(index);
            // A new drain supersedes the previous drain's withdrawn pause request.
            self.resume_after_quiesce.remove(&index);
            active.quiesce.send_replace(true);
            update(graph, |state| {
                state
                    .components
                    .get_mut(graph.nodes[index].descriptor.id())
                    .expect("component")
                    .lifecycle = ComponentLifecycle::Quiescing;
            });
        }
    }

    fn resume_quiescing(&mut self, graph: &ComputationGraph, selected: &BTreeSet<usize>) {
        for index in selected {
            if self.quiescing.contains(index) {
                if let Some(active) = self.active.get(index) {
                    active.quiesce.send_replace(false);
                    self.resume_after_quiesce.insert(*index);
                    update(graph, |state| {
                        let node = state
                            .components
                            .get_mut(graph.nodes[*index].descriptor.id())
                            .expect("component");
                        if node.failure.is_none() {
                            node.lifecycle = ComponentLifecycle::Running;
                        }
                    });
                }
            }
        }
    }

    fn fail_drain(&mut self, graph: &ComputationGraph, group: Draining, error: GraphError) {
        for abort in group.waiting.values() {
            abort.abort();
        }
        for index in &group.selected {
            self.suspended_starts.remove(index);
        }
        self.resume_quiescing(graph, &group.selected);
        let ids = group
            .selected
            .iter()
            .map(|index| graph.nodes[*index].descriptor.id().clone())
            .collect();
        let error = match reconcile::resume(graph, self, &ids) {
            Ok(()) => error,
            Err(cleanup) => GraphError::Cleanup {
                primary: Some(Box::new(error)),
                errors: vec![cleanup],
            },
        };
        group.reply.reject(error);
    }

    fn complete_drain(&mut self, graph: &ComputationGraph, completion: DrainCompletion) {
        let Some(position) = self
            .draining
            .iter()
            .position(|group| group.id == completion.group)
        else {
            return;
        };
        self.draining[position].waiting.remove(&completion.index);
        match completion.result {
            Ok(()) => self.pause_for_drain(graph, completion.index),
            Err(error) => {
                let group = self.draining.remove(position);
                self.fail_drain(graph, group, error);
            }
        }
    }

    fn advance_drains(
        &mut self,
        graph: &ComputationGraph,
        controls: &PipeGuard,
    ) -> GraphResult<()> {
        for mut group in std::mem::take(&mut self.draining) {
            if tokio::time::Instant::now() >= group.deadline {
                self.fail_drain(graph, group, GraphError::ReconciliationTimeout);
                continue;
            }
            let result = (|| {
                // Stable registration indices need not follow dependencies.
                // Removing an upstream must unblock its consumers in this pass.
                for index in graph.order.iter().copied() {
                    if !group.pending.contains(&index) {
                        continue;
                    }
                    if group.waiting.contains_key(&index) {
                        continue;
                    }
                    let processing = self
                        .active
                        .get(&index)
                        .is_some_and(|active| active.operation == Operation::Process);
                    if !processing {
                        group.pending.remove(&index);
                        continue;
                    }
                    if self.quiescing.contains(&index)
                        && !self.resume_after_quiesce.contains(&index)
                    {
                        continue;
                    }
                    let incoming: Vec<_> = graph
                        .edges
                        .iter()
                        .filter(|(edge_index, edge)| {
                            graph.ids[&edge.definition.to.component] == index
                                && group
                                    .selected
                                    .contains(&graph.ids[&edge.definition.from.component])
                                && !self.invalidated_bindings.contains(edge_index)
                        })
                        .collect();
                    if incoming.iter().any(|(_, edge)| {
                        group
                            .pending
                            .contains(&graph.ids[&edge.definition.from.component])
                    }) {
                        continue;
                    }
                    let mut drains = Vec::new();
                    for (&edge_index, edge) in incoming {
                        let from = graph.ids[&edge.definition.from.component];
                        let progress = if let Some(active) = self.active.get(&from) {
                            active
                                .outputs
                                .iter()
                                .find(|(edge, _)| *edge == edge_index)
                                .map(|(_, progress)| progress.clone())
                        } else {
                            graph.components[from]
                                .take()?
                                .outputs
                                .iter()
                                .find(|output| output.edge == edge_index)
                                .map(|output| output.progress.clone())
                        };
                        if let Some(progress) = progress {
                            let control = controls
                                .0
                                .get(&edge_index)
                                .cloned()
                                .ok_or_else(|| topology("bound edge has no drain control"))?;
                            drains.push((progress, control));
                        }
                    }
                    if drains.is_empty() {
                        self.pause_for_drain(graph, index);
                        continue;
                    }
                    let id = group.id;
                    let deadline = group.deadline;
                    let slot = graph.components[index].clone();
                    let identity = super::execution::Identity {
                        epoch: graph.observed().components[&slot.id].operation,
                        slot,
                        phase: FailurePhase::Stop,
                        action: "drain inputs",
                    };
                    let (abort, registration) = AbortHandle::new_pair();
                    let joined =
                        graph
                            .tasks
                            .drains
                            .spawn(identity.clone(), abort.clone(), async move {
                                let result = identity
                                    .run(async {
                                        Abortable::new(
                                            async {
                                                tokio::time::timeout_at(deadline, async {
                                                    for (progress, control) in drains {
                                                        progress.drained(control.as_ref()).await?;
                                                    }
                                                    Ok(())
                                                })
                                                .await
                                                .map_err(|_| GraphError::ReconciliationTimeout)?
                                            },
                                            registration,
                                        )
                                        .await
                                        .unwrap_or(Err(GraphError::Cancelled))
                                    })
                                    .await;
                                DrainCompletion {
                                    group: id,
                                    index,
                                    result,
                                }
                            })?;
                    group.waiting.insert(index, abort);
                    self.drain_futures.push(
                        async move {
                            joined.await.unwrap_or_else(|error| DrainCompletion {
                                group: id,
                                index,
                                result: Err(error),
                            })
                        }
                        .boxed(),
                    );
                }
                Ok(())
            })();
            if let Err(error) = result {
                self.fail_drain(graph, group, error);
            } else if group.pending.is_empty() {
                for index in &group.selected {
                    self.suspended_starts.remove(index);
                }
                match group.reply {
                    DrainReply::Stop(reply) => self.begin_stop(graph, group.selected, reply)?,
                    DrainReply::Quiesce(reply) => {
                        let _ = reply.send(Ok(group
                            .selected
                            .iter()
                            .map(|index| graph.nodes[*index].descriptor.id().clone())
                            .collect()));
                    }
                }
            } else {
                self.draining.push(group);
            }
        }
        Ok(())
    }

    fn attach_control(&mut self, graph: &ComputationGraph, index: usize) -> GraphResult<()> {
        let slot = &graph.components[index];
        if slot.damage().is_some() {
            return Ok(());
        }
        let handler = slot
            .handler
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        if let Some(control) = self.peer_controls.get(&index) {
            if control.generation() == slot.generation {
                if let Some(handler) = handler {
                    if let Some(handlers) = self.control_handlers.get(&index) {
                        handlers.send_replace(Some(handler));
                    }
                }
                return Ok(());
            }
        }
        let (control, inbox) = graph.peers.attach(slot.id.clone(), slot.generation)?;
        if let Some(previous) = self.control_aborts.remove(&index) {
            previous.abort();
        }
        let (handlers, handler) = watch::channel(handler);
        self.control_handlers.insert(index, handlers);
        self.peer_controls.insert(index, control.clone());
        self.launch_control(
            graph,
            index,
            PeerDriver {
                plane: graph.peers.clone(),
                control,
                inbox,
                handler,
            },
        )?;
        Ok(())
    }

    fn launch_control(
        &mut self,
        graph: &ComputationGraph,
        index: usize,
        driver: PeerDriver,
    ) -> GraphResult<()> {
        let slot = graph.components[index].clone();
        let component = slot.id.clone();
        let generation = slot.generation;
        let identity = super::execution::Identity {
            slot,
            epoch: graph.observed().components[&component].operation,
            phase: FailurePhase::Control,
            action: "control notification",
        };
        let (abort, registration) = AbortHandle::new_pair();
        self.control_aborts.insert(index, abort.clone());
        let joined = graph.tasks.controls.spawn(
            identity.clone(),
            abort,
            driver.advance(registration, identity),
        )?;
        self.control_futures.push(
            async move {
                joined.await.unwrap_or_else(|error| ControlCompletion {
                    component,
                    generation,
                    result: Ok(Err(error.into())),
                    next: None,
                })
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
        self.control_aborts.remove(&index);
        if let Some(next) = completed.next {
            if self.control_quiescing.contains(&index) {
                self.parked_controls.insert(index, next);
            } else if graph.components[index].damage().is_none() {
                if let Err(error) = self.launch_control(graph, index, next) {
                    log::error!("Cannot resume component control: {error}");
                }
            }
        }
        if let Ok(Err(source)) = completed.result {
            let error = match source.downcast::<GraphError>() {
                Ok(error) => error,
                Err(source) => GraphError::Component {
                    component: completed.component.clone(),
                    operation: "control notification",
                    source,
                },
            };
            if matches!(error.underlying(), GraphError::Cancelled) {
                return;
            }
            let damaged = error.damages_generation();
            let error = if damaged {
                graph.components[index].preserve_failure(error)
            } else {
                error
            };
            if damaged {
                if let Some(active) = self.active.get(&index) {
                    if active.operation != Operation::Stop {
                        active.abort.abort();
                        self.stop_after_abort.insert(index);
                    }
                    self.propagated_stop.insert(index);
                }
                if let Some(abort) = self.control_aborts.get(&index) {
                    abort.abort();
                }
            }
            let failure = failure(error, FailurePhase::Control);
            update(graph, |state| {
                let node = state
                    .components
                    .get_mut(&completed.component)
                    .expect("component");
                node.health = if damaged {
                    node.lifecycle = ComponentLifecycle::Failed;
                    ComponentHealth::Unavailable
                } else {
                    ComponentHealth::Degraded
                };
                node.failure = Some(failure);
                node.transition_time = Utc::now();
            });
            if damaged {
                if let Err(error) = self.propagate(graph, index) {
                    log::error!("Cannot propagate quarantined control failure: {error}");
                }
            }
        }
    }

    fn retire_control(&mut self, graph: &ComputationGraph, index: usize) -> GraphResult<()> {
        let slot = &graph.components[index];
        graph.peers.remove(&slot.id, slot.generation)?;
        self.forget_control(index);
        Ok(())
    }

    fn forget_control(&mut self, index: usize) {
        if let Some(abort) = self.control_aborts.remove(&index) {
            abort.abort();
        }
        self.peer_controls.remove(&index);
        self.control_handlers.remove(&index);
        self.control_quiescing.remove(&index);
        self.parked_controls.remove(&index);
        self.pending_creation.remove(&index);
        self.pending_auto_start.remove(&index);
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

    fn defer_creation(
        &mut self,
        graph: &ComputationGraph,
        index: usize,
        operation: Operation,
        waiting: Vec<super::super::DependencyWait>,
    ) -> CreationOutcome {
        self.pending_creation.insert(index, operation);
        let id = graph.nodes[index].descriptor.id();
        let current = graph.observed();
        let unchanged = current.components[id].realization == RealizationState::Blocked
            && current.components[id].waiting_for == waiting;
        let outcome = CreationOutcome::Blocked {
            dependencies: waiting
                .iter()
                .filter_map(|dependency| match dependency {
                    super::super::DependencyWait::Component { id } => Some(id.clone()),
                    _ => None,
                })
                .collect(),
            resources: waiting
                .iter()
                .filter_map(|dependency| match dependency {
                    super::super::DependencyWait::Resource { id } => Some(id.clone()),
                    _ => None,
                })
                .collect(),
        };
        if !unchanged {
            update(graph, |state| {
                let node = state.components.get_mut(id).expect("declared component");
                node.realization = RealizationState::Blocked;
                node.waiting_for = waiting;
                node.transition_time = Utc::now();
                if let Some(report) = &mut state.deployment {
                    report.components.insert(id.clone(), outcome.clone());
                    report.summary = OperationSummary::CompletedWithFailures;
                }
            });
        }
        outcome
    }

    fn advance_additions(&mut self, graph: &ComputationGraph) -> GraphResult<()> {
        for (index, driver) in std::mem::take(&mut self.parked_controls) {
            self.control_quiescing.remove(&index);
            if graph.ids.get(driver.control.id()) == Some(&index)
                && graph.components[index].generation == driver.control.generation()
                && graph.components[index].damage().is_none()
            {
                self.launch_control(graph, index, driver)?;
            }
        }
        if graph.retirement_active() {
            return Ok(());
        }
        let pending =
            if self.creation_dirty || self.creation_revision != Some(graph.snapshot.revision) {
                self.creation_dirty = false;
                self.creation_revision = Some(graph.snapshot.revision);
                std::mem::take(&mut self.pending_creation)
            } else {
                BTreeMap::new()
            };
        for (index, operation) in pending {
            let id = graph.nodes[index].descriptor.id();
            if graph.ids.get(id) != Some(&index)
                || graph.observed().components[id].failure.is_some()
            {
                continue;
            }
            if self.active.contains_key(&index)
                || self.conflicts(&BTreeSet::from([index]), true)
                || self
                    .reconciliation
                    .as_ref()
                    .is_some_and(|reservation| reservation.components.contains(id))
            {
                self.pending_creation.insert(index, operation);
                continue;
            }
            let waiting = creation_dependencies(graph, index);
            if !waiting.is_empty() {
                self.defer_creation(graph, index, operation, waiting);
                continue;
            }
            if let Err(error) = self
                .attach_control(graph, index)
                .and_then(|_| self.launch(graph, index, operation))
            {
                let failure = failure(error, FailurePhase::Creation);
                update(graph, |state| {
                    let node = state.components.get_mut(id).expect("declared component");
                    node.realization = RealizationState::CreationFailed;
                    node.failure = Some(failure);
                });
            }
        }
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
        let eligible = |index: &usize| {
            let id = graph.nodes[*index].descriptor.id();
            let observed = graph.observed();
            graph.ids.get(id) == Some(index)
                && observed.components[id].realization == RealizationState::Created
                && observed.components[id].failure.is_none()
                && !graph.deferred_activation.contains(id)
                && !self.conflicts(&BTreeSet::from([*index]), true)
                && binding_dependencies(graph, id).is_empty()
                && !graph.snapshot.edges.iter().any(|edge| {
                    edge.definition.to.component == *id
                        && edge.policy.activation == ActivationCoupling::RequiresRunning
                        && observed.components[&edge.definition.from.component].lifecycle
                            != ComponentLifecycle::Running
                })
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
            if let Err(error) = control.publish_status(
                crate::computation::v1::ControlDirection::Upstream,
                notification.clone(),
            ) {
                log::warn!(
                    "Upstream availability notification failed for {}: {error}",
                    control.id()
                );
            }
            if let Err(error) = control.publish_status(
                crate::computation::v1::ControlDirection::Downstream,
                notification,
            ) {
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
        if matches!(operation, Operation::Create | Operation::Reconfigure) {
            self.pending_creation.remove(&index);
        }
        self.launch_cleanup(graph, index, operation, false)
    }

    fn launch_cleanup(
        &mut self,
        graph: &ComputationGraph,
        index: usize,
        operation: Operation,
        deprovision: bool,
    ) -> GraphResult<()> {
        let slot = &graph.components[index];
        if operation != Operation::Stop {
            if let Some(cause) = slot.damage() {
                return Err(GraphError::Reported { cause });
            }
        }
        let recovery = if matches!(operation, Operation::Start | Operation::Process) {
            graph.validate_recovery_activation(&slot.id)
        } else {
            Ok(None)
        };
        let transitive_recovery = if matches!(operation, Operation::Start | Operation::Process) {
            graph.transitive_source_recovery(&slot.id)?
        } else {
            false
        };
        if operation == Operation::Start {
            graph.peers.reset_ready(&slot.id, slot.generation)?;
        }
        let mut lease = slot.take()?;
        let outputs = lease
            .outputs
            .iter()
            .map(|output| (output.edge, output.progress.clone()))
            .collect();
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
        let peer_control = self.peer_controls.get(&index).cloned();
        let admission_snapshot = graph.snapshot.clone();
        let reconfiguration = if operation == Operation::Reconfigure {
            let specification = graph
                .snapshot
                .specifications
                .get(&slot.id)
                .cloned()
                .ok_or_else(|| topology("reconfiguration requires a factory specification"))?;
            let factory = graph
                .factories
                .get(&specification.implementation)
                .cloned()
                .ok_or_else(|| topology("reconfiguration factory is not registered"))?;
            Some((Arc::new(specification), factory))
        } else {
            None
        };
        let resolved_factory = graph
            .snapshot
            .specifications
            .get(&slot.id)
            .and_then(|specification| graph.factories.get(&specification.implementation))
            .cloned();
        let downstream: Vec<_> = graph
            .snapshot
            .edges
            .iter()
            .filter(|edge| edge.definition.from.component == slot.id)
            .map(|edge| edge.definition.to.component.clone())
            .collect();
        let (quiesce, mut quiescence) = watch::channel(false);
        let identity = super::execution::Identity {
            slot: slot.clone(),
            epoch,
            phase: operation.phase(),
            action: operation.action(),
        };
        let work_identity = identity.clone();
        let span = tracing::info_span!("computation_component",
            instance_id = %graph.execution_scope,
            component_id = %node.descriptor.id(),
            component_type = crate::observability::ComponentLogType::for_component(
                node.role, node.descriptor.semantic_kind()).as_str(),
            generation = generation.0);
        update(graph, |state| {
            let observed = state.components.get_mut(&slot.id).expect("component");
            let cleanup_only = operation == Operation::Stop
                && deprovision
                && observed.lifecycle == ComponentLifecycle::Stopped;
            observed.operation = epoch;
            observed.revision = state.revision;
            observed.transition_time = Utc::now();
            if matches!(operation, Operation::Create | Operation::Reconfigure) {
                observed.realization = RealizationState::Creating;
                observed.waiting_for.clear();
            } else if operation != Operation::Process && !cleanup_only {
                observed.lifecycle = if operation == Operation::Start {
                    ComponentLifecycle::Starting
                } else {
                    ComponentLifecycle::Stopping
                };
            }
        });
        let future = async move {
            if operation != Operation::Stop {
                if let Some(cause) = lease.slot.damage() {
                    return Err(GraphError::Reported { cause });
                }
            }
            match operation {
                Operation::Create | Operation::Reconfigure => {
                    super::validate_role(&node.descriptor, node.role, node.completion).map_err(
                        |error| super::addition::validation_failure(node.descriptor.id(), error),
                    )?;
                    let definition = if let Some(definition) = reconfiguration {
                        Some(definition)
                    } else if let Component::Deferred {
                        specification,
                        factory,
                    } = &lease.component
                    {
                        Some((specification.clone(), factory.clone()))
                    } else if let Component::Unresolved(definition) = &lease.component {
                        match &definition.construction {
                            super::ComponentConstruction::Factory(specification) => {
                                resolved_factory
                                    .map(|factory| (Arc::new(specification.clone()), factory))
                            }
                            _ => None,
                        }
                    } else {
                        None
                    };
                    if let Some((specification, factory)) = definition {
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
                        if operation == Operation::Reconfigure {
                            tokio::time::timeout(timeout, lease.component.reconfigure(context))
                                .await
                                .map_err(|_| work_identity.interrupted("reconfiguration timeout"))?
                                .map_err(|source| component_error(&node, "reconfigure", source))?;
                        } else {
                            let constructed =
                                tokio::time::timeout(timeout, factory.create(context))
                                    .await
                                    .map_err(|_| GraphError::ReconciliationTimeout)?
                                    .map_err(|error| GraphError::Creation {
                                        component: node.descriptor.id().clone(),
                                        disposition: error.disposition,
                                        source: error.source,
                                    })?;
                            lease.component = constructed.0;
                        }
                        lease.refresh_configuration()?;
                    } else if let Component::Unresolved(definition) = &lease.component {
                        if matches!(
                            definition.construction,
                            super::ComponentConstruction::Factory(_)
                        ) {
                            return Err(GraphError::Creation {
                                component: node.descriptor.id().clone(),
                                disposition: FailureDisposition::Retryable,
                                source: anyhow::anyhow!("component factory is not registered"),
                            });
                        }
                        return Err(super::addition::validation_failure(
                            node.descriptor.id(),
                            topology("component instance binding is missing"),
                        ));
                    }
                    if lease.component.role() != node.role {
                        return Err(super::addition::validation_failure(
                            node.descriptor.id(),
                            topology("constructed role differs from its declaration"),
                        ));
                    }
                    if let Some(control) = peer_control {
                        let attempted = lease.attempted;
                        lease.attempted = true;
                        lease.component.bind_control(control);
                        lease.attempted = attempted;
                    }
                    lease.refresh_control();
                    check_descriptor(&lease.component, &node).map_err(|error| {
                        super::addition::validation_failure(node.descriptor.id(), error)
                    })
                }
                Operation::Start => {
                    let requirement = recovery?;
                    let admission_before = lease.component.recovery_contract();
                    let expected = if requirement.is_some() {
                        Some(lease.slot.recovery_contract()?)
                    } else {
                        None
                    };
                    check_descriptor(&lease.component, &node)?;
                    super::validate_source_progress(
                        &lease.component,
                        &graph_id,
                        &downstream,
                        transitive_recovery,
                    )?;
                    if let Component::Source(source) = &lease.component {
                        if let Some(admission) = source.admission() {
                            super::validate_admission(
                                &admission,
                                &node,
                                &admission_snapshot,
                                &resources,
                                Some(&scope),
                            )
                            .map_err(|error| {
                                component_error(&node, "validate admission", error.into())
                            })?;
                            if lease.outputs.is_empty()
                                || lease.outputs.iter().any(|output| {
                                    output.port != *admission.port()
                                        || !output.admission_channel.as_ref().is_some_and(
                                            |channel| Arc::ptr_eq(channel, admission.channel()),
                                        )
                                })
                            {
                                return Err(component_error(
                                    &node,
                                    "validate admission",
                                    super::emission_error(
                                        &node,
                                        "source admission output is not bound",
                                    )
                                    .into(),
                                ));
                            }
                        }
                    }
                    lease.attempted = true;
                    if let Some(control) = peer_control {
                        lease.component.bind_control(control);
                    }
                    lease
                        .component
                        .start()
                        .await
                        .map_err(|source| component_error(&node, "start", source))?;
                    {
                        let Instance {
                            component, outputs, ..
                        } = &mut *lease;
                        super::bind_output_destinations(
                            component,
                            &node,
                            outputs,
                            &admission_snapshot,
                            &resources,
                        )
                        .await?;
                    }
                    let actual = lease.refresh_recovery()?;
                    if (admission_before.source_admission.is_some()
                        || actual.source_admission.is_some())
                        && admission_before != actual
                    {
                        return Err(component_error(
                            &node,
                            "validate admission",
                            super::emission_error(
                                &node,
                                "source admission changed during activation",
                            )
                            .into(),
                        ));
                    }
                    if let Some(admission) = &actual.source_admission {
                        super::validate_admission(
                            admission,
                            &node,
                            &admission_snapshot,
                            &resources,
                            Some(&scope),
                        )
                        .map_err(|error| {
                            component_error(&node, "validate admission", error.into())
                        })?;
                    }
                    if let Some(requirement) = requirement {
                        if expected.as_ref() != Some(&actual) {
                            return Err(GraphError::Recovery(crate::computation::v1::RecoveryValidationError {
                                consumer: requirement.consumer,
                                issues: vec![crate::computation::v1::RecoveryIssue {
                                    participant: crate::computation::v1::RecoveryParticipant::Component(
                                        node.descriptor.id().clone(),
                                    ),
                                    guarantee: *requirement.guarantees.iter().next().expect("validated requirement"),
                                    reason: crate::computation::v1::RecoveryIncompatibility::ContractChangedDuringActivation,
                                }],
                            }));
                        }
                    }
                    lease.refresh_control();
                    check_descriptor(&lease.component, &node)
                }
                Operation::Process => {
                    recovery?;
                    let Instance {
                        component,
                        sequences,
                        inputs,
                        outputs,
                        ..
                    } = &mut *lease;
                    super::bind_output_destinations(
                        component,
                        &node,
                        outputs,
                        &admission_snapshot,
                        &resources,
                    )
                    .await?;
                    let budget = super::NodeWorkBudget::default();
                    budget
                        .run(run_node(
                            component,
                            &node,
                            sequences,
                            inputs,
                            outputs,
                            &mut quiescence,
                            super::SourceRouting {
                                graph_id: &graph_id,
                                downstream: &downstream,
                                transitive_recovery,
                            },
                            &budget,
                        ))
                        .await
                }
                Operation::Stop => tokio::time::timeout(timeout, async {
                    if lease.attempted {
                        lease
                            .component
                            .stop()
                            .await
                            .map_err(|source| component_error(&node, "stop", source))?;
                        lease.attempted = false;
                    }
                    if deprovision
                        && !matches!(
                            lease.component,
                            Component::Deferred { .. } | Component::Unresolved(_)
                        )
                    {
                        lease
                            .component
                            .deprovision()
                            .await
                            .map_err(|source| component_error(&node, "deprovision", source))?;
                    }
                    Ok(())
                })
                .await
                .map_err(|_| GraphError::StopTimeout {
                    component: node.descriptor.id().clone(),
                })?,
            }
        };
        let (abort, registration) = AbortHandle::new_pair();
        let joined = graph
            .tasks
            .nodes
            .spawn(identity.clone(), abort.clone(), async move {
                let result = identity
                    .run(async {
                        Abortable::new(future.instrument(span), registration)
                            .await
                            .unwrap_or_else(|_| {
                                Err(if operation == Operation::Reconfigure {
                                    identity.interrupted("reconfiguration cancellation")
                                } else {
                                    GraphError::Cancelled
                                })
                            })
                    })
                    .await;
                Completion {
                    index,
                    generation,
                    epoch,
                    operation,
                    result: Ok(result),
                }
            })?;
        self.futures.push(
            async move {
                joined.await.unwrap_or_else(|error| Completion {
                    index,
                    generation,
                    epoch,
                    operation,
                    result: Ok(Err(error)),
                })
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
                outputs,
                reply: None,
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
                    node.waiting_for.clear();
                    node.lifecycle_requested = true;
                }
            }
        });
        self.starting.push(Starting {
            revision: graph.snapshot.revision,
            selected: selected.clone(),
            pending: selected,
            outcomes: BTreeMap::new(),
            reply,
            force,
        });
    }

    fn advance_start(&mut self, graph: &ComputationGraph) -> GraphResult<()> {
        if graph
            .retirement
            .as_ref()
            .is_some_and(|state| state.active() && !state.retired())
        {
            return Ok(());
        }
        for group in std::mem::take(&mut self.starting) {
            self.advance_start_group(graph, group)?;
        }
        Ok(())
    }

    fn advance_start_group(
        &mut self,
        graph: &ComputationGraph,
        mut group: Starting,
    ) -> GraphResult<()> {
        for index in graph.order.iter().rev().copied() {
            if !group.pending.contains(&index)
                || self.suspended_starts.contains(&index)
                || self.retiring.contains(&index)
            {
                continue;
            }
            let node = &graph.nodes[index];
            let id = node.descriptor.id();
            let observed = graph.observed();
            let current = &observed.components[id];
            let outcome = if graph.component_retired(id) {
                Some(StartOutcome::NotRequested)
            } else if let Some(cause) = graph.components[index].damage() {
                Some(StartOutcome::StartFailed(failure(
                    GraphError::Reported { cause },
                    FailurePhase::Activation,
                )))
            } else if current.realization != RealizationState::Created {
                if current.failure.is_none()
                    && !current.waiting_for.is_empty()
                    && (group.force || graph.snapshot.lifecycle_policies[id].auto_start)
                    && !graph.deferred_activation.contains(id)
                {
                    self.pending_auto_start.insert(index, group.force);
                }
                Some(StartOutcome::NotCreated)
            } else if !group.force
                && (graph.deferred_activation.contains(id)
                    || !graph.snapshot.lifecycle_policies[id].auto_start)
            {
                Some(StartOutcome::NotRequested)
            } else if !binding_dependencies(graph, id).is_empty() {
                self.pending_auto_start.insert(index, group.force);
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
                match self.launch(graph, index, Operation::Process) {
                    Ok(()) => Some(StartOutcome::AlreadyRunning),
                    Err(error) => Some(StartOutcome::StartFailed(self.fail_operation(
                        graph,
                        index,
                        Operation::Start,
                        error,
                    )?)),
                }
            } else if self.active.get(&index).is_some_and(|active| {
                active.operation == Operation::Start
                    || (active.operation == Operation::Process
                        && current.lifecycle == ComponentLifecycle::Starting)
            }) {
                // Join an existing activation rather than treating it as a blocker.
                // The report still waits for Start to finish; readiness is separate.
                Some(StartOutcome::Started)
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
                    self.pending_auto_start.insert(index, group.force);
                    Some(StartOutcome::Blocked { dependencies })
                }
            };
            group.pending.remove(&index);
            if let Some(outcome) = outcome {
                if let StartOutcome::Blocked { dependencies } = &outcome {
                    if current.failure.is_none() {
                        let mut waiting = binding_waits(graph, id);
                        if waiting.is_empty() {
                            waiting.extend(
                                dependencies
                                    .iter()
                                    .filter(|dependency| *dependency != id)
                                    .map(|dependency| super::super::DependencyWait::Component {
                                        id: dependency.clone(),
                                    }),
                            );
                        }
                        if current.waiting_for != waiting {
                            update(graph, |state| {
                                state.components.get_mut(id).expect("component").waiting_for =
                                    waiting;
                            });
                        }
                    }
                }
                group.outcomes.insert(id.clone(), outcome);
            } else if let Err(error) = self.launch(graph, index, Operation::Start) {
                let failure = self.fail_operation(graph, index, Operation::Start, error)?;
                group
                    .outcomes
                    .insert(id.clone(), StartOutcome::StartFailed(failure));
            }
        }
        if group.pending.is_empty()
            && !self.active.iter().any(|(index, active)| {
                group.selected.contains(index) && active.operation == Operation::Start
            })
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
            self.starting.push(group);
        }
        Ok(())
    }

    /// Rejects queued handle starts for `component`, which a later stop
    /// supersedes; otherwise they would restart the component afterwards.
    fn supersede_queued_starts(&mut self, component: &ComponentId) {
        let is_superseded = |queued: &HandleRequest| matches!(queued, HandleRequest::Start { component: id, .. } if id == component);
        if !self.handle_requests.iter().any(is_superseded) {
            return;
        }
        let (superseded, retained): (VecDeque<_>, VecDeque<_>) =
            std::mem::take(&mut self.handle_requests)
                .into_iter()
                .partition(is_superseded);
        self.handle_requests = retained;
        for start in superseded {
            start.reject(GraphError::StartSuperseded {
                component: component.clone(),
            });
        }
    }

    fn begin_stop(
        &mut self,
        graph: &ComputationGraph,
        selected: BTreeSet<usize>,
        reply: oneshot::Sender<GraphResult<StopReport>>,
    ) -> GraphResult<()> {
        self.begin_stop_reply(graph, selected, StopReply::Graceful(reply))
    }

    fn force_scope(graph: &ComputationGraph, selected: &BTreeSet<usize>) -> GraphResult<()> {
        let desired = graph.snapshot.select(GraphSelection::All)?;
        let requested: BTreeSet<_> = selected
            .iter()
            .map(|index| graph.nodes[*index].descriptor.id().clone())
            .collect();
        let mut required = requested.clone();
        for id in desired.resources.iter().map(|resource| &resource.id) {
            let users = desired.component_resource_users(id);
            if !users.is_disjoint(&requested)
                && (graph.tasks.resources.contains(id)
                    || graph.pending_resource_cleanup.contains_key(id)
                    || graph.observed().resources.get(id).is_some_and(|resource| {
                        resource.realization
                            == crate::computation::v1::ResourceRealization::CleanupRequired
                    }))
            {
                return Err(GraphError::Validation {
                    component: requested
                        .iter()
                        .next()
                        .cloned()
                        .expect("selected resource user"),
                    source: crate::computation::v1::ForceCancellationResourceUnavailable {
                        resource: id.clone(),
                    }
                    .into(),
                });
            }
        }
        loop {
            let before = required.len();
            for (id, handle) in &graph.resource_handles {
                let mut users = desired.component_resource_users(id);
                let mut resources = BTreeSet::from([id.clone()]);
                for (other, candidate) in &graph.resource_handles {
                    if handle.same_instance(candidate)
                        || super::resources::same_provider(handle, candidate)
                    {
                        users.extend(desired.component_resource_users(other));
                        resources.insert(other.clone());
                    }
                }
                for edge in graph.snapshot.edges.iter() {
                    if edge.resources.keys().any(|id| resources.contains(id)) {
                        users.insert(edge.definition.from.component.clone());
                        users.insert(edge.definition.to.component.clone());
                    }
                }
                if !users.is_disjoint(&required) {
                    required.extend(users);
                }
            }
            for edge in graph.snapshot.edges.iter() {
                if edge.policy.propagate_failure
                    && required.contains(&edge.definition.from.component)
                {
                    required.insert(edge.definition.to.component.clone());
                }
                if edge.policy.fence_producer_on_failure
                    && required.contains(&edge.definition.to.component)
                {
                    required.insert(edge.definition.from.component.clone());
                }
            }
            if required.len() == before {
                break;
            }
        }
        if required != requested {
            return Err(GraphError::Validation {
                component: requested
                    .iter()
                    .next()
                    .cloned()
                    .expect("nonempty conflicting scope"),
                source: crate::computation::v1::ForceCancellationScopeError {
                    requested,
                    required,
                }
                .into(),
            });
        }
        Ok(())
    }

    fn begin_force_cancel(
        &mut self,
        graph: &mut ComputationGraph,
        selected: BTreeSet<usize>,
        reply: oneshot::Sender<GraphResult<crate::computation::v1::ForceCancellationReport>>,
    ) -> GraphResult<()> {
        let mut interrupted = BTreeMap::new();
        for index in &selected {
            let slot = &graph.components[*index];
            graph.deferred_activation.insert(slot.id.clone());
            let phase = self
                .active
                .get(index)
                .filter(|active| active.operation != Operation::Stop)
                .map(|active| active.operation.phase());
            let control_active = self.control_aborts.contains_key(index)
                && self
                    .control_handlers
                    .get(index)
                    .is_some_and(|handler| handler.borrow().is_some());
            if let Some(phase) = phase.or(control_active.then_some(FailurePhase::Control)) {
                let identity = super::execution::Identity {
                    slot: slot.clone(),
                    epoch: graph.observed().components[&slot.id].operation,
                    phase,
                    action: "force cancellation",
                };
                let cause = identity.interrupted("explicit scoped force cancellation");
                let failure = failure(cause, phase);
                interrupted.insert(slot.id.clone(), failure.clone());
                update(graph, |state| {
                    let node = state
                        .components
                        .get_mut(&slot.id)
                        .expect("selected generation");
                    node.failure = Some(failure);
                    node.health = ComponentHealth::Unavailable;
                });
            } else if let Some(cause) = slot.damage() {
                interrupted.insert(
                    slot.id.clone(),
                    failure(GraphError::Reported { cause }, FailurePhase::Processing),
                );
            }
            if interrupted.contains_key(&slot.id) {
                if let Some(abort) = self.control_aborts.get(index) {
                    abort.abort();
                }
                if self.peer_controls.remove(index).is_some() {
                    graph.peers.remove(&slot.id, slot.generation)?;
                }
            }
        }
        let (superseded, retained): (Vec<_>, Vec<_>) = std::mem::take(&mut self.draining)
            .into_iter()
            .partition(|drain| !drain.selected.is_disjoint(&selected));
        self.draining = retained;
        for drain in superseded {
            for abort in drain.waiting.values() {
                abort.abort();
            }
            for index in &drain.selected {
                self.suspended_starts.remove(index);
            }
            self.resume_quiescing(
                graph,
                &drain.selected.difference(&selected).copied().collect(),
            );
            let cause = interrupted
                .values()
                .next()
                .map(|failure| GraphError::Reported {
                    cause: failure.cause.clone(),
                })
                .unwrap_or(GraphError::Cancelled);
            drain.reply.reject(cause);
        }
        self.begin_stop_reply(graph, selected, StopReply::Forced { interrupted, reply })
    }

    fn begin_stop_reply(
        &mut self,
        graph: &ComputationGraph,
        selected: BTreeSet<usize>,
        reply: StopReply,
    ) -> GraphResult<()> {
        let mut group = Stopping {
            revision: graph.snapshot.revision,
            selected: selected.clone(),
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
                if let Err(error) = control.publish_not_ready() {
                    log::warn!("Cannot revoke readiness for {}: {error}", control.id());
                }
            }
            self.propagated_stop.remove(&index);
            let id = graph.nodes[index].descriptor.id();
            self.supersede_queued_starts(id);
            for starting in &mut self.starting {
                if starting.pending.remove(&index) {
                    starting
                        .outcomes
                        .insert(id.clone(), StartOutcome::NotRequested);
                }
            }
            if let Some(active) = self.active.get(&index) {
                if active.operation == Operation::Start {
                    for starting in self
                        .starting
                        .iter_mut()
                        .filter(|group| group.selected.contains(&index))
                    {
                        starting.outcomes.insert(
                            id.clone(),
                            StartOutcome::StartFailed(failure(
                                GraphError::Cancelled,
                                FailurePhase::Activation,
                            )),
                        );
                    }
                }
                if active.operation != Operation::Stop {
                    active.abort.abort();
                    self.stop_after_abort.insert(index);
                }
                group.pending.insert(index);
            } else if graph.components[index].take()?.attempted {
                group.pending.insert(index);
            } else {
                let mut acknowledged = false;
                update(graph, |state| {
                    acknowledged = state
                        .components
                        .get_mut(id)
                        .expect("selected component")
                        .acknowledge_stopped();
                });
                let outcome = if acknowledged {
                    StopOutcome::Stopped
                } else if graph.observed().components[id].realization != RealizationState::Created {
                    StopOutcome::NotCreated
                } else {
                    StopOutcome::AlreadyStopped
                };
                group.outcomes.insert(id.clone(), outcome);
            }
        }
        self.stopping.push(group);
        self.finish_stop(graph);
        Ok(())
    }

    fn advance_stop(&mut self, graph: &ComputationGraph) -> GraphResult<()> {
        let pending: BTreeSet<_> = self
            .stopping
            .iter()
            .flat_map(|group| group.pending.iter().copied())
            .chain(self.propagated_stop.iter().copied())
            .collect();
        for index in graph.order.iter().copied() {
            if !pending.contains(&index) {
                continue;
            }
            if !self.active.contains_key(&index) && !self.stop_after_abort.contains(&index) {
                if graph.snapshot.edges.iter().any(|edge| {
                    graph.ids[&edge.definition.to.component] == index
                        && pending.contains(&graph.ids[&edge.definition.from.component])
                }) {
                    continue;
                }
                self.launch(graph, index, Operation::Stop)?;
            }
        }
        self.finish_stop(graph);
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
            for starting in &mut self.starting {
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
                if active.operation != Operation::Stop {
                    active.abort.abort();
                    self.stop_after_abort.insert(index);
                }
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
        for mut group in std::mem::take(&mut self.stopping) {
            let waiting_for_controls = match &group.reply {
                StopReply::Forced { interrupted, .. } => interrupted
                    .keys()
                    .any(|id| self.control_aborts.contains_key(&graph.ids[id])),
                StopReply::Graceful(_) => false,
            };
            if !group.pending.is_empty() || waiting_for_controls {
                self.stopping.push(group);
                continue;
            }
            let failed = group
                .outcomes
                .values()
                .any(|outcome| matches!(outcome, StopOutcome::StopFailed(_)));
            if let StopReply::Forced { interrupted, .. } = &mut group.reply {
                let observed = graph.observed();
                for (id, cause) in interrupted {
                    if let Some(current) = observed.components[id].failure.as_ref() {
                        *cause = current.clone();
                    }
                }
            }
            group.reply.send(StopReport {
                revision: group.revision,
                summary: if failed {
                    OperationSummary::CompletedWithFailures
                } else {
                    OperationSummary::Completed
                },
                components: group.outcomes,
            });
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
        let active = self
            .active
            .remove(&completion.index)
            .expect("validated active operation");
        if self.stop_after_abort.remove(&completion.index) {
            let result = completion
                .result
                .map_err(|_| GraphError::Cancelled)
                .and_then(|result| result);
            let interrupted_failure = result
                .err()
                .filter(|error| !matches!(error.underlying(), GraphError::Cancelled))
                .map(|error| failure(slot.preserve_failure(error), completion.operation.phase()));
            if let Some(reply) = active.reply {
                let _ = reply.send(Err(GraphError::Cancelled));
            }
            self.quiescing.remove(&completion.index);
            if let Some(failure) = interrupted_failure {
                update(graph, |state| {
                    state
                        .components
                        .get_mut(&slot.id)
                        .expect("component")
                        .failure = Some(failure)
                });
            }
            if let Some(cause) = slot.damage() {
                update(graph, |state| {
                    let node = state.components.get_mut(&slot.id).expect("component");
                    if !node
                        .failure
                        .as_ref()
                        .is_some_and(|failure| failure.cause.damages_generation())
                    {
                        node.failure = Some(failure(
                            GraphError::Reported { cause },
                            completion.operation.phase(),
                        ));
                    }
                    node.lifecycle = ComponentLifecycle::Failed;
                    node.health = ComponentHealth::Unavailable;
                });
            }
            if matches!(
                completion.operation,
                Operation::Create | Operation::Reconfigure
            ) {
                update(graph, |state| {
                    let node = state.components.get_mut(&slot.id).expect("component");
                    node.realization = RealizationState::CreationFailed;
                    if slot.damage().is_none() {
                        node.failure = Some(failure(GraphError::Cancelled, FailurePhase::Creation));
                        node.lifecycle = ComponentLifecycle::Stopped;
                    }
                });
            }
            return Ok(());
        }
        let mut result = completion.result.map_err(|_| GraphError::Cancelled)?;
        if completion.operation != Operation::Process {
            self.dependencies_changed();
        }
        if completion.operation != Operation::Stop && result.is_ok() {
            if let Some(cause) = slot.damage() {
                result = Err(GraphError::Reported { cause });
            }
        }
        let id = &slot.id;
        if matches!(
            completion.operation,
            Operation::Create | Operation::Reconfigure
        ) && result.is_ok()
        {
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
        if matches!(
            completion.operation,
            Operation::Create | Operation::Reconfigure
        ) {
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
                    let waiting = if connected {
                        Vec::new()
                    } else {
                        binding_waits(graph, id)
                    };
                    update(graph, |state| {
                        let node = state.components.get_mut(id).expect("added component");
                        node.realization = if connected {
                            RealizationState::Created
                        } else {
                            RealizationState::Blocked
                        };
                        node.failure = None;
                        node.waiting_for = waiting;
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
                        if completion.operation == Operation::Reconfigure {
                            node.lifecycle = ComponentLifecycle::Failed;
                        }
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
        let mut succeeded = result.is_ok();
        match (completion.operation, result) {
            (Operation::Start, Ok(())) => {
                let automatically_ready = !slot
                    .requires_readiness
                    .load(std::sync::atomic::Ordering::Acquire);
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
                for group in self
                    .starting
                    .iter_mut()
                    .filter(|group| group.selected.contains(&completion.index))
                {
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
                if self
                    .draining
                    .iter()
                    .any(|group| group.selected.contains(&completion.index))
                {
                    self.paused.insert(completion.index);
                    update(graph, |state| {
                        state.components.get_mut(id).expect("component").lifecycle =
                            ComponentLifecycle::Quiesced;
                    });
                } else {
                    if let Err(error) = self.launch(graph, completion.index, Operation::Process) {
                        succeeded = false;
                        self.fail_operation(graph, completion.index, Operation::Start, error)?;
                    }
                }
            }
            (Operation::Process, Ok(())) => {
                if self.quiescing.remove(&completion.index) {
                    self.paused.insert(completion.index);
                    update(graph, |state| {
                        let observed = state.components.get_mut(id).expect("component");
                        observed.lifecycle = ComponentLifecycle::Quiesced;
                        observed.transition_time = Utc::now();
                    });
                    if self.resume_after_quiesce.remove(&completion.index) {
                        self.paused.remove(&completion.index);
                        update(graph, |state| {
                            state.components.get_mut(id).expect("component").lifecycle =
                                ComponentLifecycle::Running;
                        });
                        if let Err(error) = self.launch(graph, completion.index, Operation::Process)
                        {
                            self.fail_operation(
                                graph,
                                completion.index,
                                Operation::Process,
                                error,
                            )?;
                        }
                    }
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
                    if let Err(error) = control.publish_not_ready() {
                        log::warn!("Readiness notification failed for {id}: {error}");
                    }
                }
                let propagated = self.propagated_stop.remove(&completion.index);
                update(graph, |state| {
                    let node = state.components.get_mut(id).expect("component");
                    node.record_stopped(if propagated {
                        ComponentLifecycle::Failed
                    } else {
                        ComponentLifecycle::Stopped
                    });
                    node.health = if propagated || slot.damage().is_some() {
                        ComponentHealth::Unavailable
                    } else {
                        ComponentHealth::Unknown
                    };
                    for (edge, observed) in &mut state.relationships {
                        if edge.from.component == *id
                            && observed.binding == BindingState::Bound
                            && observed.availability != DataAvailability::Exhausted
                        {
                            observed.availability = DataAvailability::Idle;
                        }
                    }
                });
                for group in self
                    .stopping
                    .iter_mut()
                    .filter(|group| group.selected.contains(&completion.index))
                {
                    group.pending.remove(&completion.index);
                    group.outcomes.insert(id.clone(), StopOutcome::Stopped);
                }
            }
            (operation, Err(error)) => {
                self.fail_operation(graph, completion.index, operation, error)?;
            }
            (Operation::Create | Operation::Reconfigure, Ok(())) => {
                unreachable!("creation handled above")
            }
        }
        self.finish_stop(graph);
        if let Some(reply) = active.reply {
            let result = if succeeded {
                Ok(())
            } else if let Some(failure) = &graph.observed().components[id].failure {
                Err(GraphError::Reported {
                    cause: failure.cause.clone(),
                })
            } else {
                Err(GraphError::Cancelled)
            };
            let _ = reply.send(result);
        }
        Ok(())
    }

    fn fail_operation(
        &mut self,
        graph: &ComputationGraph,
        index: usize,
        operation: Operation,
        error: GraphError,
    ) -> GraphResult<ComponentFailure> {
        self.quiescing.remove(&index);
        self.pending_auto_start.remove(&index);
        let slot = &graph.components[index];
        let id = &slot.id;
        let failure = failure(slot.preserve_failure(error), operation.phase());
        if failure.cause.damages_generation() {
            if let Some(abort) = self.control_aborts.get(&index) {
                abort.abort();
            }
        }
        self.notify_availability(
            index,
            crate::computation::v1::ControlNotification::Unavailable {
                reason: failure.cause.to_string(),
            },
        );
        if let Some(control) = self.peer_controls.get(&index) {
            if let Err(error) = control.publish_not_ready() {
                log::warn!("Readiness notification failed for {id}: {error}");
            }
        }
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
            for group in self
                .starting
                .iter_mut()
                .filter(|group| group.selected.contains(&index))
            {
                group
                    .outcomes
                    .insert(id.clone(), StartOutcome::StartFailed(failure.clone()));
            }
        }
        if operation == Operation::Stop {
            self.propagated_stop.remove(&index);
            for group in self
                .stopping
                .iter_mut()
                .filter(|group| group.selected.contains(&index))
            {
                group.pending.remove(&index);
                group
                    .outcomes
                    .insert(id.clone(), StopOutcome::StopFailed(failure.clone()));
            }
        } else {
            self.propagate(graph, index)?;
        }
        Ok(failure)
    }
}

fn accept_addition(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    addition: super::ComponentAddition,
    management_access: bool,
    reply: oneshot::Sender<GraphResult<(ComponentId, ComponentGeneration)>>,
) {
    let validation_error = addition.bindings.validation_error.clone();
    let deferred = addition.bindings.defer_activation;
    let activate = addition.definition.lifecycle.auto_start && !deferred;
    let mut pending = Some(addition);
    let permission = configuration_access(graph, management_access).and_then(|_| {
        if !management_access
            && (graph.protected_components.contains(addition_id(&pending))
                || pending
                    .as_ref()
                    .expect("pending addition")
                    .resources
                    .iter()
                    .any(|resource| graph.protected_resources.contains(&resource.id))
                || pending
                    .as_ref()
                    .expect("pending addition")
                    .bindings
                    .resources
                    .keys()
                    .any(|id| graph.protected_resources.contains(id)))
        {
            Err(topology("component configuration is managed by DrasiLib"))
        } else {
            Ok(())
        }
    });
    match permission.and_then(|_| super::addition::insert(graph, &mut pending)) {
        Ok((index, id, generation)) => {
            if deferred {
                graph.deferred_activation.insert(id.clone());
            }
            if activate {
                operations.pending_auto_start.insert(index, false);
            }
            let setup = operations
                .attach_control(graph, index)
                .and_then(|_| operations.refresh_connections(graph))
                .and_then(|_| match validation_error {
                    Some(cause) => Err(GraphError::Reported { cause }),
                    None => {
                        let waiting = creation_dependencies(graph, index);
                        if waiting.is_empty() {
                            operations.launch(graph, index, Operation::Create)
                        } else {
                            operations.defer_creation(graph, index, Operation::Create, waiting);
                            Ok(())
                        }
                    }
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
                pending.take().expect("rejected addition retains ownership"),
            );
            let mut rejected = graph.rejected_additions.lock().unwrap_or_else(|error| {
                log::error!("Retaining rejected resources through poisoned registry: {error}");
                error.into_inner()
            });
            rejected.retain(|addition| !addition.complete());
            rejected.push(addition.clone());
            let _ = reply.send(Err(GraphError::AdditionRejected {
                cause: Box::new(error),
                addition,
            }));
        }
    }
}

/// Accepts a single-component start or stop. A request that conflicts only
/// with the operation in flight, or follows a queued request for the same
/// component, is queued for the controller loop instead of failing. A stop
/// supersedes the same component's queued starts.
async fn submit_handle_request(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    request: HandleRequest,
) -> GraphResult<()> {
    if let Err(error) = request.validate(graph) {
        request.reject(error);
        return Ok(());
    }
    if !matches!(&request, HandleRequest::Start { .. }) {
        for component in request.components() {
            operations.supersede_queued_starts(&component);
        }
    }
    let components = request.components();
    let follows_queued = operations
        .handle_requests
        .iter()
        .any(|queued| !queued.components().is_disjoint(&components));
    if follows_queued || !request.ready(graph, operations) {
        if operations.handle_requests.len() >= COMMAND_CAPACITY {
            request.reject(GraphError::OperationInProgress);
        } else {
            operations.handle_requests.push_back(request);
        }
        Ok(())
    } else {
        request.begin(graph, operations, controls, cancel).await
    }
}

/// Begins every queued handle request that no longer conflicts, keeping each
/// component's requests in arrival order. Requests are revalidated when they
/// begin, and those whose caller has gone are dropped. Returns whether any
/// request began.
async fn begin_handle_requests(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &PipeGuard,
    cancel: &mut watch::Receiver<bool>,
) -> GraphResult<bool> {
    let mut began = false;
    let mut blocked = BTreeSet::new();
    let mut position = 0;
    while let Some(request) = operations.handle_requests.get(position) {
        if request.abandoned() {
            operations.handle_requests.remove(position);
        } else if !blocked.is_disjoint(&request.components()) || !request.ready(graph, operations) {
            blocked.extend(request.components());
            position += 1;
        } else if let Some(request) = operations.handle_requests.remove(position) {
            request.begin(graph, operations, controls, cancel).await?;
            began = true;
        }
    }
    Ok(began)
}

async fn quiesce_before_stop(
    graph: &ComputationGraph,
    operations: &mut Operations,
    controls: &PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    selected: &BTreeSet<usize>,
) -> GraphResult<()> {
    // An incomplete start/constructor has no processing boundary to drain.
    // Already-running members still quiesce even while other members start.
    let processing: BTreeSet<_> = selected
        .iter()
        .copied()
        .filter(|index| {
            operations
                .active
                .get(index)
                .map_or(true, |active| active.operation == Operation::Process)
        })
        .collect();
    // Keep the group's pending members so a drain cannot finish its start report early.
    let previous = std::mem::replace(&mut operations.suspended_starts, selected.clone());
    let result = reconcile::quiesce(graph, operations, controls, cancel, processing).await;
    operations.suspended_starts = previous;
    result.map(|_| ())
}

async fn drain_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn bind_ready_dependencies(
    graph: &mut ComputationGraph,
    operations: &mut Operations,
    controls: &mut PipeGuard,
    cancel: &mut watch::Receiver<bool>,
    commands: &mut CommandReceiver,
) -> GraphResult<()> {
    if graph.retirement_active() || operations.reconciliation.is_some() {
        return Ok(());
    }
    let observed = graph.observed();
    let mut changes = Vec::new();
    let mut bindings = super::TopologyBindings::default();
    let mut selected_edges = Vec::new();
    for (index, edge) in &graph.edges {
        let state = &observed.relationships[&edge.definition];
        if state.binding != BindingState::Declared
            || state.failure.is_some()
            || edge
                .resources
                .keys()
                .any(|id| !graph.resource_handles.contains_key(id))
        {
            continue;
        }
        let endpoints = [
            graph.ids[&edge.definition.from.component],
            graph.ids[&edge.definition.to.component],
        ];
        if endpoints.iter().any(|index| {
            observed.components[graph.nodes[*index].descriptor.id()]
                .failure
                .is_some()
                || operations.pending_creation.contains_key(index)
                || operations
                    .active
                    .get(index)
                    .is_some_and(|active| active.operation != Operation::Process)
        }) || operations.conflicts(&endpoints.into_iter().collect(), true)
        {
            continue;
        }
        let mut constructed = true;
        for index in endpoints {
            if !operations.active.contains_key(&index)
                && !graph.components[index].is_constructed()?
            {
                constructed = false;
            }
        }
        if !constructed {
            continue;
        }
        if let super::DesiredPipe::External { binding, .. } = &edge.pipe {
            bindings
                .pipes
                .insert(binding.clone(), Box::new(graph.providers[index].clone()));
        }
        selected_edges.push(edge.definition.clone());
        changes.push(reconcile::DesiredMutation::Bind(
            super::DesiredRelationship {
                definition: edge.definition.clone(),
                policy: edge.policy.clone(),
                pipe: edge.pipe.clone(),
            },
        ));
    }
    if changes.is_empty() {
        return Ok(());
    }
    let outcome = match reconcile::preview(graph, changes) {
        Ok(preview) => {
            reconcile::execute(
                graph, operations, controls, cancel, preview, bindings, commands,
            )
            .await
        }
        Err(error) => Err(error),
    };
    let outcome = outcome.and_then(|report| {
        if report.committed {
            Ok(())
        } else {
            Err(GraphError::Component {
                component: selected_edges[0].from.component.clone(),
                operation: "bind deferred connections",
                source: super::super::LifecycleReportError::Reconciliation(report).into(),
            })
        }
    });
    match outcome {
        Ok(()) => Ok(()),
        Err(GraphError::Cancelled) => Err(GraphError::Cancelled),
        Err(error) => {
            let failed = failure(error, FailurePhase::Binding);
            log::error!("Deferred connection failed: {}", failed.cause);
            update(graph, |state| {
                for edge in selected_edges {
                    if let Some(binding) = state.relationships.get_mut(&edge) {
                        binding.binding = BindingState::Failed;
                        binding.failure = Some(failed.clone());
                        binding.availability = DataAvailability::Unavailable;
                    }
                }
            });
            Ok(())
        }
    }
}

fn member_indices(
    graph: &ComputationGraph,
    members: &BTreeMap<ComponentId, ComponentGeneration>,
) -> GraphResult<BTreeSet<usize>> {
    members
        .iter()
        .map(|(id, generation)| {
            graph
                .ids
                .get(id)
                .filter(|index| graph.components[**index].generation == *generation)
                .copied()
                .ok_or(GraphError::StaleGeneration)
        })
        .collect()
}

pub(super) async fn run(
    graph: &mut ComputationGraph,
    cancel: &mut watch::Receiver<bool>,
    mut commands: CommandReceiver,
    auto_start: bool,
) -> GraphResult<()> {
    if *cancel.borrow() {
        return Err(GraphError::Cancelled);
    }
    let mut operations = Operations::default();
    let mut controls = deploy(graph, cancel, &mut operations).await?;
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
        tokio::task::consume_budget().await;
        if *cancel.borrow() {
            return Err(GraphError::Cancelled);
        }
        if operations.resources_dirty
            || operations.resource_revision != Some(graph.snapshot.revision)
        {
            operations.resources_dirty = false;
            operations.resource_revision = Some(graph.snapshot.revision);
            super::resource_tasks::advance_construction(graph)?;
        }
        operations.advance_additions(graph)?;
        if operations.bindings_dirty || operations.binding_revision != Some(graph.snapshot.revision)
        {
            operations.bindings_dirty = false;
            operations.binding_revision = Some(graph.snapshot.revision);
            bind_ready_dependencies(graph, &mut operations, &mut controls, cancel, &mut commands)
                .await?;
        }
        // Finished groups free the controller for queued requests, and a
        // request that begins may finish at once, so repeat until none begins.
        loop {
            operations.advance_drains(graph, &controls)?;
            operations.advance_start(graph)?;
            operations.advance_stop(graph)?;
            if !begin_handle_requests(graph, &mut operations, &controls, cancel).await? {
                break;
            }
        }
        if auto_start
            && operations.active.is_empty()
            && operations.starting.is_empty()
            && operations.stopping.is_empty()
            && operations.draining.is_empty()
            && operations.retirements.is_empty()
            && graph.tasks.resources.is_empty()
            && operations.pending_creation.is_empty()
            && operations.pending_auto_start.is_empty()
            && operations.propagated_stop.is_empty()
        {
            let observed = graph.observed();
            let failed = observed
                .components
                .values()
                .any(|node| node.failure.is_some())
                || observed
                    .relationships
                    .values()
                    .any(|edge| edge.failure.is_some())
                || observed
                    .resources
                    .values()
                    .any(|resource| resource.failure.is_some())
                || observed.startup.as_ref().is_some_and(|report| {
                    report.summary == OperationSummary::CompletedWithFailures
                });
            if !failed {
                return Ok(());
            }
        }
        let deadline = operations.next_drain_deadline();
        tokio::select! {
            _ = cancelled(cancel) => return Err(GraphError::Cancelled),
            job = graph.tasks.resources.next(), if !graph.tasks.resources.is_empty() => {
                if let Err(error) = super::resource_tasks::complete(graph, &job) {
                    log::debug!("Resource callback completed with a retained failure: {error}");
                }
                operations.dependencies_changed();
            }
            _ = drain_deadline(deadline) => {},
            completion = operations.drain_futures.next(), if !operations.drain_futures.is_empty() => {
                if let Some(completion) = completion { operations.complete_drain(graph, completion); }
            }
            completion = operations.control_futures.next(), if !operations.control_futures.is_empty() && !graph.retirement_active() => {
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
            result = operations.retirements.next(), if !operations.retirements.is_empty() => {
                if let Some(result) = result {
                    let _ = operations.complete_retirement(graph, result);
                }
                if operations.retirements.is_empty() { operations.retiring.clear(); }
            }
            command = commands.recv() => {
                let command = command.ok_or(GraphError::ControllerClosed)?;
                handle_command(graph, &mut operations, &mut controls, cancel, command, &mut commands).await?;
            }
        }
    }
}

fn handle_command<'a>(
    graph: &'a mut ComputationGraph,
    mut operations: &'a mut Operations,
    mut controls: &'a mut PipeGuard,
    cancel: &'a mut watch::Receiver<bool>,
    command: Command,
    mut commands: &'a mut CommandReceiver,
) -> BoxFuture<'a, GraphResult<()>> {
    async move {
        if operations.reconciliation.is_some() {
            let key = match &command {
                Command::ObserveResources { component, generation, .. } =>
                    Some((component.clone(), *generation, false)),
                Command::ObservePlugin { component, generation, .. } =>
                    Some((component.clone(), *generation, true)),
                _ => None,
            };
            if let Some((component, generation, plugin)) = key {
                if graph.observed().components.get(&component).is_some_and(|node| node.generation == generation) {
                    if let Some(previous) = graph.pending_metadata.insert((component, plugin), command) {
                        retire_metadata(graph, operations, previous)?;
                    }
                } else {
                    retire_metadata(graph, operations, command)?;
                }
                return Ok(());
            }
        }
        if operations
            .reconciliation
            .as_ref()
            .is_some_and(|reservation| !reservation.permits(graph, &command))
        {
            if let Command::Add { addition, reply, .. } = command {
                let rejected = super::addition::RejectedAddition::new(addition);
                graph.rejected_additions.lock().unwrap_or_else(|error| error.into_inner())
                    .push(rejected.clone());
                let _ = reply.send(Err(GraphError::AdditionRejected {
                    cause: Box::new(GraphError::OperationInProgress),
                    addition: rejected,
                }));
            } else {
                if let Some(resources) = command.rejected_resources(graph) {
                    graph.rejected_resources.push(resources);
                }
                retire_callback(graph, operations, None, move |_| {
                    command.reject(GraphError::OperationInProgress); Ok(())
                })?;
            }
            return Ok(());
        }
        match Some(command) {
                Some(Command::RetiredResources(reply)) => {
                    let resources = graph.retirement.as_ref().filter(|state| state.retired())
                        .map(|state| state.resources.keys().filter(|id| graph.resource_retired(id)).cloned().collect()).unwrap_or_default();
                    let _ = reply.send(resources);
                }
                Some(Command::FreezeRecovery { resources, components, allow_processing_failure, reply }) => {
                    let result = if graph.retirement_active()
                        || !operations.active.is_empty()
                        || !operations.futures.is_empty()
                        || !operations.starting.is_empty()
                        || !operations.stopping.is_empty()
                        || !operations.draining.is_empty()
                        || !operations.propagated_stop.is_empty()
                        || !operations.pending_auto_start.is_empty()
                        || !graph.tasks.resources.is_empty()
                        || !graph.tasks.preparations.is_empty()
                        || !graph.tasks.pipes.is_empty()
                        || !graph.tasks.retirements.is_empty()
                    {
                        Err(GraphError::OperationInProgress)
                    } else {
                        super::RecoveryFreeze::capture(graph, &resources, &components, allow_processing_failure)
                    };
                    let _ = reply.send(result);
                }
                Some(Command::ReadinessPolicy { component, generation, required, management_access, reply }) => {
                    let result = (|| {
                        component_configuration_access(graph, management_access, &component)?;
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
                        if operations.reconciliation.as_ref().is_some_and(|reservation| {
                            reservation.components.contains(graph.nodes[index].descriptor.id())
                        }) {
                            continue;
                        }
                        if graph.snapshot.lifecycle_policies[graph.nodes[index].descriptor.id()].auto_start {
                            graph.deferred_activation.remove(graph.nodes[index].descriptor.id());
                            operations.pending_auto_start.insert(index, false);
                        }
                    }
                }
                Some(Command::BindStream { component, generation, port, stream, management_access, reply }) => {
                    let result = (|| {
                        component_configuration_access(graph, management_access, &component)?;
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
                    let slot = graph.ids.get(&component).map(|index| graph.components[*index].clone())
                        .filter(|slot| slot.generation == generation);
                    let mut retired: Vec<_> = bindings.iter().map(|(_, handle)| handle.clone()).collect();
                    let result = super::resources::observe(graph, component.clone(), generation, bindings, &mut retired);
                    retire_callback(graph, operations, slot, move |identity|
                        drop_resource_handles(identity, retired)
                    )?;
                    if let Err(error) = result {
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
                Some(Command::ReconcileComponents { previous, desired, bindings, reply }) => {
                    let prepared = (|| {
                        if graph.retirement.as_ref().is_some_and(|state| state.active() && !state.retired()) {
                            return Err(GraphError::OperationInProgress);
                        }
                        let current = graph.snapshot.select(GraphSelection::All)?;
                        let target = crate::management::compose_definition(current.clone(), &previous, &desired, &graph.observation_resources)
                            .map_err(|error| topology(format!("{error:#}")))?;
                        let mut changes = Vec::new();
                        if !crate::management::equivalent(&current, &target).map_err(|error| topology(error.to_string()))?
                            || !bindings.resource_constructors.is_empty()
                            || bindings.factories.factories.iter().any(|(id, factory)| {
                                graph.factories.get(id).map_or(true, |current| !Arc::ptr_eq(current, factory))
                            })
                        {
                            changes.push(reconcile::DesiredMutation::SetTopology(target));
                        }
                        if graph.retirement.as_ref().is_some_and(|state| state.retired()) {
                            for node in &desired.components {
                                if graph.component_retired(node.descriptor.id()) {
                                    changes.push(reconcile::DesiredMutation::ReplaceComponent(node.clone()));
                                }
                            }
                        }
                        let observed = graph.observed();
                        for node in &desired.components {
                            if observed.components.get(node.descriptor.id()).is_some_and(|node| {
                                node.failure.as_ref().is_some_and(|failure| failure.disposition == FailureDisposition::Retryable)
                            }) {
                                changes.push(reconcile::DesiredMutation::Retry(GraphSelection::Exact(vec![node.descriptor.id().clone()])));
                            }
                        }
                        if changes.is_empty() { Ok(None) }
                        else { reconcile::preview(graph, changes).map(Some) }
                    })();
                    let result = match prepared {
                        Ok(Some(preview)) => reconcile::execute(graph, operations, controls, cancel, preview, bindings, commands).await,
                        Ok(None) => Ok(reconcile::ReconciliationReport {
                            revision: graph.snapshot.revision,
                            summary: OperationSummary::Completed,
                            committed: true,
                            creation: BTreeMap::new(),
                            startup: None,
                            removed: BTreeSet::new(),
                            failures: Vec::new(),
                        }),
                        Err(error) => Err(error),
                    };
                    if result.as_ref().is_ok_and(|report| report.committed) {
                        graph.complete_retirement();
                    }
                    let cancelled = matches!(&result, Err(GraphError::Cancelled));
                    let _ = reply.send(result);
                    if cancelled { return Err(GraphError::Cancelled); }
                }
                Some(Command::AddComponents { mut batch, management_access, reply }) => {
                    let prepared = (|| {
                        configuration_access(graph, management_access)?;
                        if batch.definition.graph_id != graph.snapshot.id.as_ref() {
                            return Err(topology("component definition does not belong to this graph"));
                        }
                        let target = crate::computation::components::append_definition(
                            graph.snapshot.select(GraphSelection::All)?,
                            &batch.definition,
                            &graph.resource_handles,
                            &batch.bindings.resources,
                        ).map_err(|error| topology(format!("{error:#}")))?;
                        batch.bindings.resources.retain(|id, supplied| {
                            !graph.resource_handles.get(id).is_some_and(|existing| existing.same_shared_instance(supplied))
                        });
                        let preview = reconcile::preview(graph, vec![reconcile::DesiredMutation::SetTopology(target)])?;
                        if !management_access {
                            reconcile::check_protected(graph, &preview)?;
                        }
                        Ok(preview)
                    })();
                    let result = match prepared {
                        Ok(preview) => reconcile::execute(graph, operations, controls, cancel, preview, batch.bindings, commands).await,
                        Err(error) => Err(error),
                    };
                    let cancelled = matches!(&result, Err(GraphError::Cancelled));
                    let _ = reply.send(result);
                    if cancelled { return Err(GraphError::Cancelled); }
                }
                Some(Command::ProtectComponents { components, resources, reply }) => {
                    graph.protected_components = components;
                    graph.protected_resources = resources;
                    let _ = reply.send(Ok(()));
                }
                Some(Command::Add { addition, management_access, reply }) => {
                    accept_addition(graph, operations, addition, management_access, reply);
                }
                Some(Command::RegisterFactory { factory, reply }) => {
                    let identity = factory.descriptor().implementation.clone();
                    let result = if let Some(current) = graph.factories.get(&identity) {
                        if Arc::ptr_eq(current, &factory) {
                            Ok(())
                        } else {
                            Err(topology("an implementation identity cannot be rebound to a different factory"))
                        }
                    } else {
                        graph.factories.insert(identity, factory);
                        operations.dependencies_changed();
                        Ok(())
                    };
                    let _ = reply.send(result);
                }
                Some(Command::ControlConnections { connections, subscriptions, management_access, reply }) => {
                    let protected = |connections: &[(ComponentId, ComponentId)]| {
                        connections.iter().filter(|(from, to)| {
                            graph.protected_components.contains(from) || graph.protected_components.contains(to)
                        }).cloned().collect::<BTreeSet<_>>()
                    };
                    let old = if subscriptions { graph.snapshot.subscriptions.as_ref() } else { graph.snapshot.control_connections.as_ref() };
                    let permission = configuration_access(graph, management_access).and_then(|_| {
                        if !management_access && protected(old) != protected(&connections) {
                            Err(topology("connection configuration is managed by DrasiLib"))
                        } else { Ok(()) }
                    }).and_then(|_| {
                        if subscriptions {
                            for (from, _) in &connections {
                                if let Some(index) = graph.ids.get(from) {
                                    if graph.components[*index].recovery_contract()?.source_admission.is_some() {
                                        return Err(topology("source admission cannot publish through an untracked subscription"));
                                    }
                                }
                            }
                        }
                        if subscriptions && !graph.snapshot.recovery_requirements.is_empty() {
                            let old: BTreeSet<_> = old.iter().cloned().collect();
                            let new: BTreeSet<_> = connections.iter().cloned().collect();
                            let changed: Vec<_> = old.symmetric_difference(&new).collect();
                            if !changed.is_empty() {
                                let mut candidate = graph.snapshot.clone();
                                candidate.subscriptions = connections.clone().into();
                                for requirement in &candidate.recovery_requirements {
                                    let report = graph.recovery_report_in(&candidate, requirement)?;
                                    if changed.iter().any(|(_, to)| report.participants.contains(to)) {
                                        report.validate()?;
                                    }
                                }
                            }
                        }
                        Ok(())
                    });
                    let result = if let Err(error) = permission {
                        Err(error)
                    } else if connections.iter().any(|(from, to)| {
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
                Some(Command::ControlHandler { component, generation, handler, management_access, reply }) => {
                    let result = component_configuration_access(graph, management_access, &component).and_then(|_| graph.ids.get(&component)
                        .filter(|index| graph.components[**index].generation == generation)
                        .and_then(|index| operations.control_handlers.get(index))
                        .ok_or(GraphError::StaleGeneration)
                        .map(|handlers| { handlers.send_replace(Some(handler)); }));
                    let _ = reply.send(result);
                }
                Some(Command::HandleStart { component, generation, reply }) => {
                    submit_handle_request(graph, operations, controls, cancel, HandleRequest::Start { component, generation, reply }).await?;
                }
                Some(Command::StartMembers { members, reply }) => {
                    let selected = member_indices(graph, &members).and_then(|selected| {
                        if graph.retirement_active() || operations.conflicts(&selected, true) { Err(GraphError::OperationInProgress) }
                        else { Ok(selected) }
                    });
                    match selected {
                        Ok(selected) => {
                            for id in members.keys() { graph.deferred_activation.remove(id); }
                            operations.begin_start(graph, selected, Some(reply), true);
                        }
                        Err(error) => { let _ = reply.send(Err(error)); }
                    }
                }
                Some(Command::StopMembers { members, reply }) => {
                    submit_handle_request(graph, operations, controls, cancel, HandleRequest::StopMembers { members, reply }).await?;
                }
                Some(Command::HandleStop { component, generation, reply }) => {
                    submit_handle_request(graph, operations, controls, cancel, HandleRequest::Stop { component, generation, reply }).await?;
                }
                Some(Command::MutateMembers { members, changes, bindings, deprovision, management_access, reply }) => {
                    let result = async {
                        configuration_access(graph, management_access)?;
                        member_indices(graph, &members)?;
                        let mut preview = reconcile::preview(graph, changes)?;
                        if !management_access {
                            reconcile::check_protected(graph, &preview)?;
                        }
                        if deprovision.iter().any(|id| !members.contains_key(id) || !preview.removed().contains(id)) {
                            return Err(topology("state removal requires an explicitly removed member"));
                        }
                        preview.deprovision = deprovision;
                        reconcile::execute(graph, operations, controls, cancel, preview, bindings, commands).await
                    }.await;
                    let cancelled = matches!(&result, Err(GraphError::Cancelled));
                    let _ = reply.send(result);
                    if cancelled { return Err(GraphError::Cancelled); }
                }
                Some(Command::Preview { revision, changes, reply }) => {
                    let result = check_revision(graph, revision).and_then(|_| reconcile::preview(graph, changes));
                    let _ = reply.send(result);
                }
                Some(Command::RecoveryReport { requirement, reply }) => {
                    let _ = reply.send(graph.recovery_report(&requirement));
                }
                Some(Command::Reconcile { preview, bindings, management_access, reply }) => {
                    let permission = configuration_access(graph, management_access).and_then(|_| {
                        if !management_access {
                            reconcile::check_protected(graph, &preview)?;
                        }
                        Ok(())
                    });
                    let result = match permission {
                        Ok(()) => reconcile::execute(graph, operations, controls, cancel, preview, bindings, commands).await,
                        Err(error) => Err(error),
                    };
                    let cancelled = matches!(&result, Err(GraphError::Cancelled));
                    let _ = reply.send(result);
                    if cancelled { return Err(GraphError::Cancelled); }
                }
                Some(Command::Start { revision, selection, force, reply }) => {
                    let valid = check_revision(graph, revision).and_then(|_| {
                        let selected = select(graph, &selection)?;
                        if graph.retirement_active() || operations.conflicts(&selected, true) {
                            Err(GraphError::OperationInProgress)
                        } else { Ok(selected) }
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
                        let selected = select(graph, &selection)?;
                        if operations.conflicts(&selected, false) { Err(GraphError::OperationInProgress) }
                        else { Ok(selected) }
                    });
                    match valid {
                        Ok(selected) => {
                            graph.deferred_activation.extend(selected.iter().map(|index| graph.nodes[*index].descriptor.id().clone()));
                            operations.begin_drain(graph, selected, DrainReply::Stop(reply));
                        }
                        Err(error) => { let _ = reply.send(Err(error)); }
                    }
                }
                Some(Command::ForceCancel { revision, selection, reply }) => {
                    let selected = check_revision(graph, revision).and_then(|_| {
                        if graph.retirement_active() { return Err(GraphError::OperationInProgress); }
                        if graph.pending_metadata.keys().any(|(_, plugin)| !plugin) {
                            return Err(GraphError::OperationInProgress);
                        }
                        let selected = select(graph, &selection)?;
                        if !selected.is_disjoint(&operations.retiring) { return Err(GraphError::OperationInProgress); }
                        Operations::force_scope(graph, &selected)?;
                        Ok(selected)
                    });
                    match selected {
                        Ok(selected) => operations.begin_force_cancel(graph, selected, reply)?,
                        Err(error) => { let _ = reply.send(Err(error)); }
                    }
                }
                Some(Command::Quiesce { revision, selection, reply }) => {
                    match check_revision(graph, revision).and_then(|_| select(graph, &selection)) {
                        Ok(selected) if selected.iter().any(|index| operations.active.get(index)
                            .is_some_and(|active| active.operation != Operation::Process)) => {
                            let _ = reply.send(Err(GraphError::OperationInProgress));
                        }
                        Ok(selected) => operations.begin_drain(graph, selected, DrainReply::Quiesce(reply)),
                        Err(error) => { let _ = reply.send(Err(error)); }
                    }
                }
                Some(Command::Policy { revision, component, policy, management_access, reply }) => {
                    let result = check_revision(graph, revision).and_then(|_| {
                        component_configuration_access(graph, management_access, &component)?;
                        let index = graph.ids.get(&component)
                            .ok_or_else(|| topology(format!("unknown component {component}")))?;
                        if operations.conflicts(&BTreeSet::from([*index]), true) {
                            return Err(GraphError::OperationInProgress);
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
                None => unreachable!("a received command is present"),
        }
        Ok(())
    }
    .boxed()
}

pub(super) async fn cleanup(graph: &mut ComputationGraph) -> Vec<GraphError> {
    mark_cleanup_required(&graph.observed);
    graph.tasks.cancel();
    let mut errors = Vec::new();
    let mut joined_stops = BTreeSet::new();
    let mut disposal = Operations::default();
    for (_, command) in std::mem::take(&mut graph.pending_metadata) {
        if let Err(error) = retire_metadata(graph, &mut disposal, command) {
            errors.push(error);
        }
    }
    for result in graph.tasks.preparations.settle(graph.cleanup_timeout).await {
        match result {
            Ok(Ok(prepared)) => {
                if let Err(error) = retire_callback(graph, &mut disposal, None, move |_| {
                    drop(prepared);
                    Ok(())
                }) {
                    errors.push(error);
                }
            }
            Ok(Err(error)) | Err(error)
                if !matches!(error.underlying(), GraphError::Cancelled)
                    || error.damages_generation() =>
            {
                errors.push(error)
            }
            Ok(Err(_)) | Err(_) => {}
        }
    }
    for result in graph.tasks.pipes.settle(graph.cleanup_timeout).await {
        match result {
            Ok(Ok(provided)) => {
                if let Err(error) = retire_callback(graph, &mut disposal, None, move |_| {
                    drop(provided);
                    Ok(())
                }) {
                    errors.push(error);
                }
            }
            Ok(Err(error)) | Err(error)
                if !matches!(error.underlying(), GraphError::Cancelled)
                    || error.damages_generation() =>
            {
                errors.push(error)
            }
            Ok(Err(_)) | Err(_) => {}
        }
    }
    for result in graph.tasks.retirements.settle(graph.cleanup_timeout).await {
        if let Err(error) = result.and_then(|result| result) {
            errors.push(error);
        }
    }
    for result in graph.tasks.nodes.settle(graph.cleanup_timeout).await {
        match result {
            Ok(completion) => {
                if completion.operation == Operation::Stop {
                    joined_stops.insert(completion.index);
                }
                if let Ok(Err(error)) = completion.result {
                    if !matches!(error, GraphError::Cancelled) {
                        let error = graph.components[completion.index].preserve_failure(error);
                        let failure = failure(error, completion.operation.phase());
                        update(graph, |state| {
                            if let Some(node) = state
                                .components
                                .get_mut(graph.nodes[completion.index].descriptor.id())
                            {
                                if node.generation == completion.generation {
                                    node.lifecycle = ComponentLifecycle::Failed;
                                    node.failure = Some(failure.clone());
                                }
                            }
                        });
                        if completion.operation == Operation::Stop
                            || failure.cause.damages_generation()
                        {
                            errors.push(GraphError::Reported {
                                cause: failure.cause,
                            });
                        }
                    }
                }
            }
            Err(error) => errors.push(error),
        }
    }
    for result in graph.tasks.controls.settle(graph.cleanup_timeout).await {
        if let Err(error) = result {
            errors.push(error);
        }
    }
    for result in graph.tasks.drains.settle(graph.cleanup_timeout).await {
        if let Err(error) = result {
            errors.push(error);
        }
    }
    let callbacks = graph.tasks.resources.settle(graph.cleanup_timeout).await;
    for (job, result) in callbacks {
        let result = if job.joined() {
            super::resource_tasks::complete(graph, &job)
        } else {
            result
        };
        if let Err(error) = result {
            if !matches!(error.underlying(), GraphError::Cancelled) {
                errors.push(error);
            }
        }
    }

    let mut operations = Operations::default();
    let controls = PipeGuard(BTreeMap::new());
    let mut pending = BTreeSet::new();
    for index in graph.order.iter().copied() {
        let slot = &graph.components[index];
        if let Some(cause) = slot.damage() {
            update(graph, |state| {
                let node = state.components.get_mut(&slot.id).expect("component");
                if !node
                    .failure
                    .as_ref()
                    .is_some_and(|failure| failure.cause.damages_generation())
                {
                    node.failure = Some(failure(
                        GraphError::Reported { cause },
                        FailurePhase::Processing,
                    ));
                }
                node.lifecycle = ComponentLifecycle::Failed;
                node.health = ComponentHealth::Unavailable;
            });
        }
        let lease = match slot.take() {
            Ok(lease) => lease,
            Err(error) => {
                errors.push(error);
                continue;
            }
        };
        if lease.attempted && !joined_stops.contains(&index) {
            pending.insert(index);
        }
    }
    // Data work is joined before these hooks. Shared graph resources remain
    // owned until every user is cleaned up; data edges are not resource leases.
    for &index in &graph.order {
        if pending.remove(&index) {
            if let Err(error) = operations.launch(graph, index, Operation::Stop) {
                errors.push(error);
            }
        }
    }
    let join_budget = graph
        .cleanup_timeout
        .saturating_add(std::time::Duration::from_millis(10));
    let deadline = tokio::time::Instant::now() + join_budget;
    while !operations.futures.is_empty() {
        let completion = match tokio::time::timeout_at(deadline, operations.futures.next()).await {
            Ok(completion) => completion,
            Err(_) => {
                graph.tasks.cancel();
                errors.extend(
                    operations
                        .active
                        .keys()
                        .map(|index| GraphError::StopTimeout {
                            component: graph.nodes[*index].descriptor.id().clone(),
                        }),
                );
                break;
            }
        };
        let Some(completion) = completion else {
            break;
        };
        let index = completion.index;
        if let Err(error) = operations.complete(graph, completion, &controls) {
            errors.push(error);
        }
        if let Some(failure) = graph.observed().components[graph.nodes[index].descriptor.id()]
            .failure
            .as_ref()
            .filter(|failure| failure.phase == FailurePhase::Stop)
        {
            errors.push(GraphError::Reported {
                cause: failure.cause.clone(),
            });
        }
    }
    for &index in &graph.order {
        let slot = &graph.components[index];
        let Ok(mut lease) = slot.take() else {
            continue;
        };
        if !lease.attempted {
            let identity = super::execution::Identity {
                slot: slot.clone(),
                epoch: graph.observed().components[&slot.id].operation,
                phase: FailurePhase::Stop,
                action: "release pipe endpoints",
            };
            if let Err(error) = identity.call(|| {
                lease.inputs.clear();
                lease.outputs.clear();
                Ok(())
            }) {
                errors.push(error);
            }
            update(graph, |state| {
                if let Some(observed) = state.components.get_mut(&slot.id) {
                    observed.record_stopped(ComponentLifecycle::Stopped);
                }
            });
        }
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
    if errors.is_empty() && !graph.tasks.is_empty() {
        errors.push(GraphError::OperationInProgress);
    }
    errors
}

pub(super) fn needs_cleanup(graph: &ComputationGraph) -> GraphResult<bool> {
    if !graph.tasks.is_empty() {
        return Ok(true);
    }
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
    let order = super::resources::cleanup_order(
        graph.snapshot.resources.values(),
        &graph.snapshot.resource_dependencies,
    )?;
    for id in &order {
        let specification = &graph.snapshot.resources[id];
        if specification.ownership == ResourceOwnership::Borrowed {
            continue;
        }
        if !graph.resource_handles.contains_key(id)
            && !graph.pending_resource_cleanup.contains_key(id)
            && !graph.tasks.resources.contains(id)
        {
            continue;
        }
        update(graph, |state| {
            state.resources.get_mut(id).expect("resource").realization =
                ResourceRealization::CleanupRequired;
        });
        let result = match super::resources::ensure_released_dependents(graph, id) {
            Ok(()) => match super::resource_tasks::shutdown(graph, id) {
                Ok(job) => {
                    let result = tokio::time::timeout(
                        graph
                            .cleanup_timeout
                            .saturating_add(std::time::Duration::from_millis(10)),
                        job.wait(),
                    )
                    .await
                    .map_err(|error| GraphError::ResourceCleanup {
                        resource: id.clone(),
                        source: error.into(),
                    })
                    .and_then(|result| result);
                    if job.joined() {
                        super::resource_tasks::complete(graph, &job)
                    } else {
                        result
                    }
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {}
            Err(error) => {
                let failure = failure(error, FailurePhase::Removal);
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

#[cfg(test)]
#[path = "execution_regression_tests.rs"]
mod execution_regression_tests;

#[cfg(test)]
mod command_tests {
    use super::*;
    use crate::computation::v1::{
        ComponentDescriptor, ComponentRecovery, ComputationComponent, EnvelopeSource,
        GraphChangeCodec, OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection,
        RecoveryGuarantee, RecoveryRequirement, RecoveryScope,
    };

    #[tokio::test]
    async fn ready_command_queues_alternate_without_starvation() {
        let (sender, mut receiver) = command_channel();
        for _ in 0..COMMAND_CAPACITY {
            sender.try_send(Command::AutoStart).unwrap();
            let (reply, _response) = oneshot::channel();
            sender
                .send(Command::HandleStop {
                    component: ComponentId::try_new("pending").unwrap(),
                    generation: ComponentGeneration(1),
                    reply,
                })
                .await
                .unwrap();
        }
        for _ in 0..COMMAND_CAPACITY {
            assert!(matches!(receiver.recv().await, Some(Command::AutoStart)));
            assert!(matches!(
                receiver.recv().await,
                Some(Command::HandleStop { .. })
            ));
        }
        assert!(receiver.recv().now_or_never().is_none());
    }

    #[tokio::test]
    async fn closing_one_command_queue_keeps_the_other_receivable() {
        for close_regular in [false, true] {
            let (sender, mut receiver) = command_channel();
            let (remaining, closed) = if close_regular {
                (sender.construction, sender.regular)
            } else {
                (sender.regular, sender.construction)
            };
            drop(closed);
            assert!(receiver.recv().now_or_never().is_none());
            remaining.send(Command::AutoStart).await.unwrap();
            assert!(matches!(receiver.recv().await, Some(Command::AutoStart)));
            drop(remaining);
            assert!(receiver.recv().await.is_none());
        }
    }

    struct AdmittedSource(ComponentDescriptor);

    #[async_trait::async_trait]
    impl ComputationComponent for AdmittedSource {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.0
        }
        fn recovery_contract(&self) -> ComponentRecovery {
            ComponentRecovery::admitted(drasi_core::interface::StorageDurability::VOLATILE)
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    #[async_trait::async_trait]
    impl EnvelopeSource for AdmittedSource {
        async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
            std::future::pending().await
        }
    }

    #[test]
    fn recovery_inspection_and_activation_ignore_unrelated_poisoned_contracts() {
        let source = |name| {
            Box::new(AdmittedSource(
                ComponentDescriptor::try_new(
                    ComponentId::try_new(name).unwrap(),
                    vec![PortDescriptor::new(
                        crate::computation::v1::PortId::try_new("out").unwrap(),
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )
                .unwrap(),
            ))
        };
        let selected = ComponentId::try_new("selected").unwrap();
        let unrelated = ComponentId::try_new("unrelated").unwrap();
        let request = |consumer| RecoveryRequirement {
            consumer,
            scope: RecoveryScope::MemoryLifetime,
            guarantees: [RecoveryGuarantee::Acceptance].into(),
        };
        let mut graph = ComputationGraph::builder("inspection-isolation")
            .source(source("selected"))
            .source(source("unrelated"))
            .require_recovery(request(selected.clone()))
            .build()
            .unwrap();
        let slot = graph.components[graph.ids[&unrelated]].clone();
        assert!(std::thread::spawn(move || {
            let _guard = slot.recovery.lock().unwrap();
            panic!("injected unrelated metadata poison");
        })
        .join()
        .is_err());
        assert!(graph
            .recovery_report(&request(selected.clone()))
            .unwrap()
            .satisfied());
        assert!(graph.recovery_report(&request(unrelated.clone())).is_err());
        graph.snapshot.recovery_requirements = vec![request(unrelated)];
        assert!(graph
            .validate_recovery_activation(&selected)
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn construction_does_not_remove_backpressure_or_block_handle_stop() {
        let (sender, mut receiver) = command_channel();
        for _ in 0..COMMAND_CAPACITY {
            sender.try_send(Command::AutoStart).unwrap();
        }
        assert!(matches!(
            sender.try_send(Command::AutoStart),
            Err(mpsc::error::TrySendError::Full(_))
        ));
        let send = sender.send(Command::AutoStart);
        tokio::pin!(send);
        assert!(futures::poll!(&mut send).is_pending());
        assert!(receiver.recv_during_construction().now_or_never().is_none());
        let (reply, _response) = oneshot::channel();
        sender
            .send(Command::HandleStop {
                component: ComponentId::try_new("pending").unwrap(),
                generation: ComponentGeneration(1),
                reply,
            })
            .await
            .unwrap();
        assert!(matches!(
            receiver.recv_during_construction().await,
            Some(Command::HandleStop { .. })
        ));
        assert!(futures::poll!(&mut send).is_pending());
        assert!(matches!(receiver.recv().await, Some(Command::AutoStart)));
        send.await.unwrap();
        for _ in 0..COMMAND_CAPACITY {
            assert!(matches!(receiver.recv().await, Some(Command::AutoStart)));
        }
        assert!(receiver.recv().now_or_never().is_none());
    }
}
