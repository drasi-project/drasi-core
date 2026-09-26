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
use crate::computation::v1::{
    ComponentConstruction, ComponentFactory, ComponentLifecycle, ConstructedComponent,
    ControlHandler, DesiredComponent, FailurePhase, ObservedComponent, OperationEpoch,
    RealizationState, ResourceRealization, ResourceSpecification, TopologyBindings,
};
use tokio::sync::oneshot;

/// Ownership returned with a rejected addition. The graph also retains this
/// owner until disposal, so discarding an error cannot lose asynchronous cleanup.
pub struct RejectedAddition {
    value: tokio::sync::Mutex<Option<ComponentAddition>>,
    complete: std::sync::atomic::AtomicBool,
}

impl std::fmt::Debug for RejectedAddition {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RejectedAddition")
            .field(
                "complete",
                &self.complete.load(std::sync::atomic::Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

impl RejectedAddition {
    pub(super) fn new(addition: ComponentAddition) -> Arc<Self> {
        Arc::new(Self {
            value: tokio::sync::Mutex::new(Some(addition)),
            complete: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Recover the supplied instances/resources instead of letting graph
    /// disposal reclaim them. Only one caller can take the rejected value.
    pub async fn take(&self) -> Option<ComponentAddition> {
        let result = self.value.lock().await.take();
        self.complete
            .store(true, std::sync::atomic::Ordering::Release);
        result
    }

    pub(super) fn complete(&self) -> bool {
        self.complete.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(super) async fn dispose(
        &self,
        existing: &BTreeMap<ResourceId, ResourceHandle>,
        timeout: Duration,
    ) -> GraphResult<()> {
        let mut value = self.value.lock().await;
        let Some(addition) = value.as_mut() else {
            return Ok(());
        };
        let owned: Vec<_> = addition
            .resources
            .iter()
            .filter(|resource| resource.ownership == ResourceOwnership::Graph)
            .map(|resource| resource.id.clone())
            .collect();
        let mut errors = Vec::new();
        for id in owned {
            let Some(handle) = addition.bindings.resources.get(&id).cloned() else {
                continue;
            };
            if existing.values().any(|current| {
                current.same_instance(&handle) || super::resources::same_provider(current, &handle)
            }) {
                addition.bindings.resources.remove(&id);
                continue;
            }
            match tokio::time::timeout(timeout, handle.shutdown()).await {
                Ok(Ok(())) => {
                    addition.bindings.resources.remove(&id);
                }
                result => {
                    let source = match result {
                        Ok(Err(error)) => error,
                        Err(error) => error.into(),
                        Ok(Ok(())) => unreachable!(),
                    };
                    errors.push(GraphError::ResourceCleanup {
                        resource: id,
                        source,
                    });
                }
            }
        }
        if errors.is_empty() {
            value.take();
            self.complete
                .store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        } else {
            Err(GraphError::Cleanup {
                primary: None,
                errors,
            })
        }
    }
}

impl Drop for RejectedAddition {
    fn drop(&mut self) {
        if self.value.get_mut().as_ref().is_some_and(|addition| {
            addition.resources.iter().any(|resource| {
                resource.ownership == ResourceOwnership::Graph
                    && addition.bindings.resources.contains_key(&resource.id)
            })
        }) {
            log::warn!("Rejected addition dropped without returning its resources or awaiting graph disposal");
        }
    }
}

/// A declaration and its already supplied instances. Accepting this value adds
/// the node; validation, construction and activation are observable node outcomes.
pub struct ComponentAddition {
    pub definition: DesiredComponent,
    pub resources: Vec<ResourceSpecification>,
    pub bindings: TopologyBindings,
}

impl ComponentAddition {
    pub fn new(instance: ConstructedComponent) -> Self {
        let descriptor = instance.0.descriptor().clone();
        let binding = descriptor.id().as_str().to_owned();
        let definition = DesiredComponent {
            descriptor,
            role: instance.0.role(),
            completion: instance.0.completion(),
            streams: BTreeMap::new(),
            lifecycle: LifecyclePolicy::default(),
            input_merge: InputMergePolicy::default(),
            construction: ComponentConstruction::External {
                binding: binding.clone(),
            },
        };
        let mut bindings = TopologyBindings::default();
        bindings.components.insert(binding, instance);
        Self {
            definition,
            resources: Vec::new(),
            bindings,
        }
    }

    pub fn from_specification(
        specification: ComponentSpecification,
        factory: Arc<dyn ComponentFactory>,
    ) -> Self {
        let definition = DesiredComponent {
            descriptor: specification.descriptor.clone(),
            role: specification.role,
            completion: specification.completion,
            streams: BTreeMap::new(),
            lifecycle: LifecyclePolicy::default(),
            input_merge: InputMergePolicy::default(),
            construction: ComponentConstruction::Factory(specification),
        };
        let mut bindings = TopologyBindings::default();
        bindings
            .factories
            .register(factory)
            .expect("a new registry has no duplicate factory");
        Self {
            definition,
            resources: Vec::new(),
            bindings,
        }
    }

    pub fn auto_start(mut self, enabled: bool) -> Self {
        self.definition.lifecycle.auto_start = enabled;
        self
    }

    pub fn bind_stream(mut self, port: PortId, stream: StreamId) -> Self {
        self.definition.streams.insert(port, stream);
        self
    }

    pub fn require_downstream_ready(mut self) -> Self {
        self.bindings
            .readiness_required
            .insert(self.definition.descriptor.id().clone());
        self
    }

    /// Retain auto-start policy but let the containing instance start it later.
    pub fn defer_activation(mut self) -> Self {
        self.bindings.defer_activation = true;
        self
    }

    pub fn with_resource(
        mut self,
        specification: ResourceSpecification,
        handle: ResourceHandle,
    ) -> Self {
        self.bindings
            .resources
            .insert(specification.id.clone(), handle);
        self.resources.push(specification);
        self
    }
}

/// A handle to one addition, never to a later component reusing the same ID.
///
/// `wait_started().await` waits for successful readiness in this construction
/// generation's most recently requested activation. Completion is retained after
/// stop until another start is requested. Use `tokio::time::timeout` for a deadline.
#[derive(Clone)]
pub struct ComponentHandle {
    control: GraphControl,
    id: ComponentId,
    generation: ComponentGeneration,
}

impl ComponentHandle {
    pub fn id(&self) -> &ComponentId {
        &self.id
    }

    pub fn generation(&self) -> ComponentGeneration {
        self.generation
    }

    /// Set readiness-gated activation before starting this component.
    pub async fn require_downstream_ready(&self, required: bool) -> GraphResult<()> {
        self.control.require_configuration_write()?;
        let (reply, result) = oneshot::channel();
        self.control
            .commands
            .send(controller::Command::ReadinessPolicy {
                component: self.id.clone(),
                generation: self.generation,
                required,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    /// Configure a native output stream before its first activation.
    pub async fn bind_stream(&self, port: PortId, stream: StreamId) -> GraphResult<()> {
        self.control.require_configuration_write()?;
        let (reply, result) = oneshot::channel();
        self.control
            .commands
            .send(controller::Command::BindStream {
                component: self.id.clone(),
                generation: self.generation,
                port,
                stream,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub fn observed(&self) -> GraphResult<ObservedComponent> {
        self.control
            .observed()
            .components
            .get(&self.id)
            .filter(|node| node.generation == self.generation)
            .cloned()
            .ok_or(GraphError::StaleGeneration)
    }

    pub fn control(&self) -> GraphResult<super::super::ComponentControl> {
        self.observed()?;
        self.control
            .peers
            .sender(&self.id, self.generation)
            .map_err(GraphError::from)
    }

    pub async fn set_control_handler(&self, handler: Arc<dyn ControlHandler>) -> GraphResult<()> {
        self.control.require_configuration_write()?;
        let (reply, result) = oneshot::channel();
        self.control
            .commands
            .send(controller::Command::ControlHandler {
                component: self.id.clone(),
                generation: self.generation,
                handler,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    pub async fn wait_created(&self) -> GraphResult<()> {
        self.wait(false).await
    }

    pub async fn wait_started(&self) -> GraphResult<()> {
        self.wait(true).await
    }

    async fn wait(&self, started: bool) -> GraphResult<()> {
        let mut changes = self.control.subscribe_observed();
        loop {
            let snapshot = changes.borrow_and_update().clone();
            let node = snapshot
                .components
                .get(&self.id)
                .filter(|node| node.generation == self.generation)
                .ok_or(GraphError::StaleGeneration)?;
            if started && node.started || !started && node.realization == RealizationState::Created
            {
                return Ok(());
            }
            if let Some(failure) = &node.failure {
                return Err(GraphError::Reported {
                    cause: failure.cause.clone(),
                });
            }
            tokio::select! {
                biased;
                changed = changes.changed() => changed.map_err(|_| GraphError::ControllerClosed)?,
                _ = self.control.commands.closed() => return Err(GraphError::ControllerClosed),
            }
        }
    }

    pub(crate) async fn start_requested(&self) -> GraphResult<super::super::StartReport> {
        self.wait_created().await?;
        let (reply, result) = oneshot::channel();
        self.control
            .commands
            .send(controller::Command::HandleStart {
                component: self.id.clone(),
                generation: self.generation,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        let report = result.await.map_err(|_| GraphError::ControllerClosed)??;
        self.observed()?;
        Ok(report)
    }

    pub async fn start(&self) -> GraphResult<()> {
        let report = match self.start_requested().await {
            Err(GraphError::OperationInProgress)
                if matches!(
                    self.observed()?.lifecycle,
                    ComponentLifecycle::Starting | ComponentLifecycle::Running
                ) =>
            {
                return self.wait_started().await;
            }
            report => report?,
        };
        match report.components.get(&self.id) {
            Some(
                super::super::StartOutcome::Started | super::super::StartOutcome::AlreadyRunning,
            ) => self.wait_started().await,
            Some(super::super::StartOutcome::StartFailed(failure)) => Err(GraphError::Reported {
                cause: failure.cause.clone(),
            }),
            Some(super::super::StartOutcome::Blocked { .. })
                if self
                    .control
                    .desired_snapshot()
                    .readiness_required
                    .contains(&self.id) =>
            {
                self.wait_started().await
            }
            _ => Err(GraphError::StartupIncomplete),
        }
    }

    pub async fn stop(&self) -> GraphResult<()> {
        let (reply, result) = oneshot::channel();
        self.control
            .commands
            .send(controller::Command::HandleStop {
                component: self.id.clone(),
                generation: self.generation,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        let report = result.await.map_err(|_| GraphError::ControllerClosed)??;
        self.observed()?;
        match report.components.get(&self.id) {
            Some(super::super::StopOutcome::StopFailed(failure)) => Err(GraphError::Reported {
                cause: failure.cause.clone(),
            }),
            Some(_) => Ok(()),
            None => Err(GraphError::StaleGeneration),
        }
    }
}

impl std::fmt::Debug for ComponentHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ComponentHandle")
            .field("id", &self.id)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl GraphControl {
    /// Connect declared ports. Pipe creation failures remain on the connection
    /// in the returned reconciliation report.
    pub async fn connect(
        &self,
        edge: EdgeDefinition,
        provider: Box<dyn PipeProvider>,
        policy: RelationshipPolicy,
    ) -> GraphResult<super::ReconciliationReport> {
        let desired = self.desired_snapshot();
        if desired
            .edges
            .iter()
            .any(|existing| existing.definition == edge)
        {
            return Err(topology("duplicate connection"));
        }
        let capabilities = provider.capabilities().map_err(|source| GraphError::Pipe {
            edge: desired.edges.len(),
            source,
        })?;
        let binding = format!(
            "{}:{}:{}:{}",
            edge.from.component.as_str().len(),
            edge.from.component,
            edge.from.port.as_str().len(),
            edge.from.port,
        );
        let binding = format!(
            "{binding}:{}:{}:{}:{}",
            edge.to.component.as_str().len(),
            edge.to.component,
            edge.to.port.as_str().len(),
            edge.to.port
        );
        let pipe = super::DesiredPipe::External {
            binding: binding.clone(),
            capabilities: capabilities.supported().iter().copied().collect(),
            capacity: capabilities.capacity().map(std::num::NonZeroUsize::get),
            resources: provider.resource_dependencies(),
            exclusive_resources: provider.exclusive_resources(),
        };
        let preview = self
            .preview(
                desired.revision,
                vec![super::DesiredMutation::Bind(super::DesiredRelationship {
                    definition: edge,
                    pipe,
                    policy,
                })],
            )
            .await?;
        let mut bindings = TopologyBindings::default();
        bindings.pipes.insert(binding, provider);
        self.reconcile(preview, bindings).await
    }

    pub(crate) async fn request_auto_start(&self) -> GraphResult<()> {
        self.commands
            .send(controller::Command::AutoStart)
            .await
            .map_err(|_| GraphError::ControllerClosed)
    }

    /// Add a node and return without waiting for its realization or activation.
    /// Only rejection of the graph addition is a method error. Later failures
    /// belong to the returned generation-specific handle.
    pub async fn add_component(&self, addition: ComponentAddition) -> GraphResult<ComponentHandle> {
        if let Err(cause) = self.require_configuration_write() {
            let addition = RejectedAddition::new(addition);
            self.rejected_additions
                .lock()
                .map_err(|_| topology("rejected addition ownership poisoned"))?
                .push(addition.clone());
            return Err(GraphError::AdditionRejected {
                cause: Box::new(cause),
                addition,
            });
        }
        let (reply, result) = oneshot::channel();
        if let Err(error) = self
            .commands
            .send(controller::Command::Add { addition, reply })
            .await
        {
            let controller::Command::Add { addition, .. } = error.0 else {
                unreachable!("only an addition was sent");
            };
            let rejected = RejectedAddition::new(addition);
            let mut pending = self.rejected_additions.lock().unwrap_or_else(|error| {
                log::error!(
                    "Retaining rejected resources through poisoned cleanup registry: {error}"
                );
                error.into_inner()
            });
            pending.retain(|addition| !addition.complete());
            pending.push(rejected.clone());
            return Err(GraphError::AdditionRejected {
                cause: Box::new(GraphError::ControllerClosed),
                addition: rejected,
            });
        }
        let (id, generation) = result.await.map_err(|_| GraphError::ControllerClosed)??;
        Ok(ComponentHandle {
            control: self.clone(),
            id,
            generation,
        })
    }

    pub fn component_handle(&self, id: &ComponentId) -> GraphResult<ComponentHandle> {
        let generation = self
            .observed()
            .components
            .get(id)
            .ok_or_else(|| topology(format!("unknown component {id}")))?
            .generation;
        Ok(ComponentHandle {
            control: self.clone(),
            id: id.clone(),
            generation,
        })
    }

    /// Host wiring for control-only component connections.
    /// Component code receives only a restricted `ComponentControl`, never this
    /// authority to change the connection graph.
    pub async fn set_control_connections(
        &self,
        connections: Vec<(ComponentId, ComponentId)>,
    ) -> GraphResult<()> {
        self.require_configuration_write()?;
        let (reply, result) = oneshot::channel();
        self.commands
            .send(controller::Command::ControlConnections {
                connections,
                subscriptions: false,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }

    /// Data subscription topology implemented by ordinary component hosts.
    pub async fn set_subscriptions(
        &self,
        connections: Vec<(ComponentId, ComponentId)>,
    ) -> GraphResult<()> {
        self.require_configuration_write()?;
        let (reply, result) = oneshot::channel();
        self.commands
            .send(controller::Command::ControlConnections {
                connections,
                subscriptions: true,
                reply,
            })
            .await
            .map_err(|_| GraphError::ControllerClosed)?;
        result.await.map_err(|_| GraphError::ControllerClosed)?
    }
}

pub(super) fn insert(
    graph: &mut ComputationGraph,
    pending: &mut Option<ComponentAddition>,
) -> GraphResult<(usize, ComponentId, ComponentGeneration)> {
    let addition = pending.as_ref().expect("unprocessed addition");
    let id = addition.definition.descriptor.id().clone();
    validate_identifier("component", id.as_str())?;
    if graph.reserved_components.contains(id.as_str()) {
        return Err(topology(format!(
            "component ID {id} is reserved by the host"
        )));
    }
    if graph.ids.contains_key(&id) {
        return Err(topology(format!("duplicate component {id}")));
    }
    let revision = GraphRevision(
        graph
            .snapshot
            .revision
            .0
            .checked_add(1)
            .ok_or_else(|| topology("graph revision exhausted"))?,
    );
    let next_generation = graph
        .next_generation
        .checked_add(1)
        .ok_or_else(|| topology("component generation exhausted"))?;
    let mut resource_ids = BTreeSet::new();
    for resource in &addition.resources {
        validate_identifier("resource binding", &resource.binding)?;
        if !resource_ids.insert(resource.id.clone())
            || graph.snapshot.resources.contains_key(&resource.id)
        {
            return Err(topology(format!("duplicate resource {}", resource.id)));
        }
    }
    let next_resource_generation = graph
        .next_resource_generation
        .checked_add(resource_ids.len() as u64)
        .ok_or_else(|| topology("resource generation exhausted"))?;
    let resource_generations: BTreeMap<_, _> = resource_ids
        .iter()
        .enumerate()
        .map(|(offset, id)| (id.clone(), graph.next_resource_generation + offset as u64))
        .collect();
    if addition
        .bindings
        .resources
        .keys()
        .any(|id| !resource_ids.contains(id))
    {
        return Err(topology("an addition cannot replace an existing resource"));
    }
    if !addition.bindings.pipes.is_empty() {
        return Err(topology("add pipes using a connection mutation"));
    }
    if addition
        .bindings
        .subscriptions
        .iter()
        .any(|(from, to)| to != &id || from == to)
    {
        return Err(topology(
            "an addition can declare only its own non-self input subscriptions",
        ));
    }
    let expected = match &addition.definition.construction {
        ComponentConstruction::External { binding } => Some(binding),
        ComponentConstruction::Factory(_) => None,
    };
    if addition
        .bindings
        .components
        .keys()
        .any(|binding| Some(binding) != expected)
    {
        return Err(topology(
            "an addition contains unrelated component instances",
        ));
    }
    if addition
        .bindings
        .readiness_required
        .iter()
        .any(|target| target != &id)
    {
        return Err(topology(
            "an addition cannot change another component's readiness policy",
        ));
    }
    for (identity, factory) in &addition.bindings.factories.factories {
        if graph
            .factories
            .get(identity)
            .is_some_and(|prior| !Arc::ptr_eq(prior, factory))
        {
            return Err(topology(
                "an implementation identity cannot change its factory",
            ));
        }
    }
    let mut addition = pending.take().expect("validated addition");
    graph
        .factories
        .extend(addition.bindings.factories.factories);
    graph
        .snapshot
        .readiness_required
        .extend(addition.bindings.readiness_required);
    let component = match &addition.definition.construction {
        ComponentConstruction::Factory(specification) => graph
            .factories
            .get(&specification.implementation)
            .cloned()
            .map(|factory| Component::Deferred {
                specification: Arc::new(specification.clone()),
                factory,
            }),
        ComponentConstruction::External { binding } => addition
            .bindings
            .components
            .remove(binding)
            .map(|component| component.0),
    }
    .unwrap_or_else(|| Component::Unresolved(Arc::new(addition.definition.clone())));
    let node = NodeSnapshot {
        descriptor: addition.definition.descriptor.clone(),
        role: addition.definition.role,
        completion: addition.definition.completion,
        output_streams: addition.definition.streams.clone(),
        input_merge: addition.definition.input_merge,
    };
    let index = graph.components.len();
    let generation = ComponentGeneration(graph.next_generation);
    graph.next_generation = next_generation;
    graph.ids.insert(id.clone(), index);
    graph.nodes.push(node.clone());
    graph.components.push(controller::InstanceSlot::declared(
        id.clone(),
        component,
        generation,
    ));
    graph.order.push(index);
    let mut nodes = graph.snapshot.nodes.to_vec();
    nodes.push(node);
    graph.snapshot.nodes = nodes.into();
    graph.snapshot.revision = revision;
    graph.snapshot.allow_incomplete = true;
    let mut subscriptions: BTreeSet<_> = graph.snapshot.subscriptions.iter().cloned().collect();
    subscriptions.extend(addition.bindings.subscriptions);
    graph.snapshot.subscriptions = subscriptions.into_iter().collect::<Vec<_>>().into();
    graph
        .snapshot
        .lifecycle_policies
        .insert(id.clone(), addition.definition.lifecycle);
    match addition.definition.construction {
        ComponentConstruction::Factory(specification) => {
            graph
                .snapshot
                .specifications
                .insert(id.clone(), specification);
        }
        ComponentConstruction::External { binding } => {
            graph
                .snapshot
                .external_bindings
                .insert(id.clone(), Arc::from(binding));
        }
    }
    for specification in addition.resources {
        graph
            .snapshot
            .resources
            .insert(specification.id.clone(), specification);
    }
    graph
        .snapshot
        .component_resources
        .insert(id.clone(), resource_ids.clone());
    graph.resource_handles.extend(addition.bindings.resources);
    graph.next_resource_generation = next_resource_generation;
    controller::update(graph, |state| {
        state.revision = revision;
        state.components.insert(
            id.clone(),
            ObservedComponent {
                generation,
                operation: OperationEpoch(0),
                revision,
                realization: RealizationState::Pending,
                lifecycle: ComponentLifecycle::Stopped,
                health: super::super::ComponentHealth::Unknown,
                failure: None,
                exhausted: false,
                started: false,
                lifecycle_requested: false,
                transition_time: chrono::Utc::now(),
            },
        );
        for node in state.components.values_mut() {
            node.revision = revision;
        }
        for resource in resource_ids {
            state.resources.insert(
                resource.clone(),
                super::super::ObservedResource {
                    realization: if graph.resource_handles.contains_key(&resource) {
                        ResourceRealization::Created
                    } else {
                        ResourceRealization::Pending
                    },
                    generation: resource_generations[&resource],
                    revision,
                    transition_time: chrono::Utc::now(),
                    failure: None,
                },
            );
        }
        if let Some(deployment) = &mut state.deployment {
            deployment.revision = revision;
            deployment
                .components
                .insert(id.clone(), super::super::CreationOutcome::NotAttempted);
        }
    });
    graph.desired.send_replace(Arc::new(graph.snapshot.clone()));
    Ok((index, id, generation))
}

pub(super) fn validation_failure(id: &ComponentId, error: GraphError) -> GraphError {
    GraphError::Validation {
        component: id.clone(),
        source: anyhow::Error::new(error),
    }
}

pub(super) fn creation_phase(error: &GraphError) -> FailurePhase {
    match error.underlying() {
        GraphError::Validation { .. } => FailurePhase::Validation,
        _ => FailurePhase::Creation,
    }
}
