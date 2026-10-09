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
    future::Future,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{Context, Poll},
    time::Duration,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use tokio::sync::{mpsc, watch};

mod addition;
mod configuration;
mod controller;
pub use addition::{ComponentAddition, ComponentHandle, RejectedAddition};
pub use configuration::{CapturedComponentConfiguration, GraphConfigurationSnapshot};
mod recovery;
mod registry;
mod resources;
mod retirement;
pub(crate) use registry::GraphRegistrySnapshot;
pub(crate) use retirement::RecoveryFreeze;
mod specification;
mod topology;
pub use controller::reconcile::{DesiredMutation, ReconciliationPreview, ReconciliationReport};
pub use specification::*;
pub use topology::*;

use super::{
    data::{validate_identifier, validate_schema},
    validate_connection, validate_sink_completion, ComponentDescriptor, ComponentGeneration,
    ComponentId, ContractError, Delivery, EnvelopeId, EnvelopeReceiver, EnvelopeSender,
    EnvelopeSink, EnvelopeSource, GraphRevision, InputEnvelope, InputMergePolicy, LifecyclePolicy,
    ObservedGraph, OutputEnvelope, PipeCapabilities, PipeCapability, PipeControl, PipeError,
    PipeProvider, PipeRequirements, PortDescriptor, PortDirection, PortId, RelationshipPolicy,
    ResourceId, SendFailure, SinkCompletion, StreamId, Transformer,
};

pub type GraphResult<T> = std::result::Result<T, GraphError>;

/// Errors retain the original component/provider cause. A send failure may follow
/// successful delivery to other branches; no rollback or retry is implied.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error(transparent)]
    Contract(#[from] ContractError),
    #[error(transparent)]
    Control(#[from] super::ControlError),
    #[error(transparent)]
    Recovery(#[from] super::RecoveryValidationError),
    #[error("component {component} validation failed: {source}")]
    Validation {
        component: ComponentId,
        #[source]
        source: anyhow::Error,
    },
    #[error("component addition rejected: {cause}")]
    AdditionRejected {
        #[source]
        cause: Box<GraphError>,
        addition: Arc<addition::RejectedAddition>,
    },
    #[error("invalid graph topology: {reason}")]
    Topology { reason: String },
    #[error("cannot start graph in state {state:?}")]
    InvalidState { state: GraphState },
    #[error("graph revision changed: expected {expected:?}, actual {actual:?}")]
    StaleRevision {
        expected: GraphRevision,
        actual: GraphRevision,
    },
    #[error("observation or command targets an obsolete component generation or operation")]
    StaleGeneration,
    #[error("the graph controller is closed")]
    ControllerClosed,
    #[error("the graph observation queue is full")]
    ObservationQueueFull,
    #[error("a conflicting component lifecycle operation is still in progress")]
    OperationInProgress,
    #[error("reconciliation did not reach its processing or cleanup boundary before the deadline")]
    ReconciliationTimeout,
    #[error("no selected component could be activated; inspect the per-component startup report")]
    StartupIncomplete,
    #[error("component {component} lost its explicit lifecycle dependency {dependency}")]
    DependencyUnavailable {
        component: ComponentId,
        dependency: ComponentId,
    },
    #[error("component {component} construction failed: {source}")]
    Creation {
        component: ComponentId,
        disposition: super::FailureDisposition,
        #[source]
        source: anyhow::Error,
    },
    #[error("resource {resource} cleanup failed: {source}")]
    ResourceCleanup {
        resource: ResourceId,
        #[source]
        source: anyhow::Error,
    },
    #[error("resource {resource} construction failed: {source}")]
    ResourceCreation {
        resource: ResourceId,
        #[source]
        source: anyhow::Error,
    },
    #[error("{cause}")]
    Reported {
        #[source]
        cause: Arc<GraphError>,
    },
    #[error("pipe for edge {edge} failed: {source}")]
    Pipe {
        edge: usize,
        #[source]
        source: PipeError,
    },
    #[error("component {component} failed during {operation}: {source}")]
    Component {
        component: ComponentId,
        operation: &'static str,
        #[source]
        source: anyhow::Error,
    },
    #[error("component {component} violated its emission/descriptor contract: {reason}")]
    Emission {
        component: ComponentId,
        reason: String,
    },
    #[error("edge {edge} could not confirm acceptance after {accepted_branches} accepted branches: {source}")]
    Forward {
        edge: usize,
        accepted_branches: usize,
        #[source]
        source: Box<SendFailure>,
    },
    #[error("graph cancelled; accepted/delivered events and effects are not rolled back")]
    Cancelled,
    #[error("component {component} stop exceeded the cleanup deadline")]
    StopTimeout { component: ComponentId },
    #[error("graph cleanup failed (original failure: {primary:?}): {errors:?}")]
    Cleanup {
        #[source]
        primary: Option<Box<GraphError>>,
        errors: Vec<GraphError>,
    },
}

fn validate_role(
    descriptor: &ComponentDescriptor,
    role: ComponentRole,
    completion: Option<SinkCompletion>,
) -> GraphResult<()> {
    let inputs = descriptor
        .ports()
        .iter()
        .filter(|port| port.direction() == PortDirection::Input)
        .count();
    let outputs = descriptor.ports().len() - inputs;
    let valid = match role {
        ComponentRole::Source => inputs == 0 && outputs > 0,
        ComponentRole::Transformer | ComponentRole::Query => inputs > 0 && outputs > 0,
        ComponentRole::Sink => inputs > 0 && outputs == 0,
        ComponentRole::Service => inputs == 0 && outputs == 0,
    };
    if !valid || (role == ComponentRole::Sink) != completion.is_some() {
        return Err(topology(format!(
            "invalid role/ports/completion for {}",
            descriptor.id()
        )));
    }
    Ok(())
}

fn validate_edge_contract(
    output: &PortDescriptor,
    input: &PortDescriptor,
    completion: Option<SinkCompletion>,
    capabilities: &PipeCapabilities,
    requirements: &PipeRequirements,
) -> GraphResult<()> {
    if let Some(completion) = completion {
        for requirements in [requirements, output.requirements(), input.requirements()] {
            validate_sink_completion(completion, requirements)?;
        }
        if capabilities
            .supported()
            .contains(&PipeCapability::ExplicitAcknowledgement)
        {
            validate_sink_completion(
                completion,
                &PipeRequirements::new([PipeCapability::ExplicitAcknowledgement]),
            )?;
        }
    }
    validate_connection(output, input, capabilities)?;
    capabilities.validate(requirements)?;
    for capability in capabilities.supported() {
        if !matches!(
            capability,
            PipeCapability::FifoPerStream
                | PipeCapability::RankedEventOrder
                | PipeCapability::Backpressure
                | PipeCapability::DurableAcceptance
                | PipeCapability::ExplicitAcknowledgement
                | PipeCapability::Replay
                | PipeCapability::RetainedHistory
        ) {
            return Err(ContractError::UnsupportedCapability {
                capability: *capability,
            }
            .into());
        }
    }
    if capabilities.capacity().is_none() {
        return Err(topology(
            "every edge must declare a finite nonzero capacity",
        ));
    }
    Ok(())
}

fn dependency_order(
    ids: &BTreeMap<ComponentId, usize>,
    edges: &[EdgeSnapshot],
) -> GraphResult<Vec<usize>> {
    let mut successors = vec![Vec::new(); ids.len()];
    let mut indegree = vec![0; ids.len()];
    for edge in edges {
        let from = *ids
            .get(&edge.definition.from.component)
            .ok_or_else(|| topology("unknown producer"))?;
        let to = *ids
            .get(&edge.definition.to.component)
            .ok_or_else(|| topology("unknown consumer"))?;
        successors[from].push(to);
        indegree[to] += 1;
    }
    let mut ready: VecDeque<_> = indegree
        .iter()
        .enumerate()
        .filter_map(|(index, degree)| (*degree == 0).then_some(index))
        .collect();
    let mut order = Vec::new();
    while let Some(index) = ready.pop_front() {
        order.push(index);
        for successor in &successors[index] {
            indegree[*successor] -= 1;
            if indegree[*successor] == 0 {
                ready.push_back(*successor);
            }
        }
    }
    if order.len() != ids.len() {
        return Err(topology("cycles (including self-loops) are not supported"));
    }
    Ok(order)
}

impl GraphError {
    pub fn underlying(&self) -> &GraphError {
        match self {
            Self::Reported { cause } => cause.underlying(),
            error => error,
        }
    }
}

/// Controller scope state, not a synthetic running graph-root component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphState {
    Ready,
    Starting,
    Running,
    Stopping,
    Completed,
    Cancelled,
    Failed,
    /// Run dropped or stop failed: call `ComputationGraph::shutdown` to await hooks.
    CleanupRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ComponentRole {
    Source,
    Transformer,
    Query,
    Sink,
    Service,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct Endpoint {
    pub component: ComponentId,
    pub port: PortId,
}

impl Endpoint {
    pub fn new(component: ComponentId, port: PortId) -> Self {
        Self { component, port }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct EdgeDefinition {
    pub from: Endpoint,
    pub to: Endpoint,
}

impl EdgeDefinition {
    pub fn new(from: Endpoint, to: Endpoint) -> Self {
        Self { from, to }
    }
}

/// Descriptive data only: no providers, resolved secrets, or live endpoints.
#[derive(Debug, Clone)]
pub struct NodeSnapshot {
    pub descriptor: ComponentDescriptor,
    pub role: ComponentRole,
    pub completion: Option<SinkCompletion>,
    pub output_streams: BTreeMap<PortId, StreamId>,
    pub input_merge: InputMergePolicy,
}

#[derive(Debug, Clone)]
pub struct EdgeSnapshot {
    pub definition: EdgeDefinition,
    pub capabilities: PipeCapabilities,
    pub policy: RelationshipPolicy,
    pub resources: BTreeMap<ResourceId, ResourceRole>,
    pub pipe: DesiredPipe,
}

#[derive(Debug, Clone)]
pub struct GraphSnapshot {
    pub id: Arc<str>,
    pub revision: GraphRevision,
    pub nodes: Arc<[NodeSnapshot]>,
    pub edges: Arc<[EdgeSnapshot]>,
    pub requirements: PipeRequirements,
    pub recovery_requirements: Vec<super::RecoveryRequirement>,
    pub lifecycle_policies: BTreeMap<ComponentId, LifecyclePolicy>,
    pub specifications: BTreeMap<ComponentId, ComponentSpecification>,
    pub external_bindings: BTreeMap<ComponentId, Arc<str>>,
    pub resources: BTreeMap<ResourceId, ResourceSpecification>,
    /// Host-supplied construction recipes. These are privileged configuration,
    /// not properties inferred from arbitrary provider objects.
    pub resource_configurations: BTreeMap<ResourceId, serde_json::Value>,
    pub resource_dependencies: BTreeMap<ResourceId, BTreeMap<ResourceId, ResourceRole>>,
    pub unbound_relationships: Arc<[DesiredRelationship]>,
    /// Host-declared control-only adjacency.
    pub control_connections: Arc<[(ComponentId, ComponentId)]>,
    /// Data subscriptions implemented by ordinary source/query/reaction hosts.
    pub subscriptions: Arc<[(ComponentId, ComponentId)]>,
    pub readiness_required: BTreeSet<ComponentId>,
    pub component_resources: BTreeMap<ComponentId, BTreeSet<ResourceId>>,
    pub component_plugins: BTreeMap<ComponentId, PluginIdentity>,
    pub allow_incomplete: bool,
}

impl GraphSnapshot {
    pub(crate) fn data_connections(&self) -> impl Iterator<Item = (&ComponentId, &ComponentId)> {
        self.edges
            .iter()
            .map(|edge| &edge.definition)
            .chain(
                self.unbound_relationships
                    .iter()
                    .map(|edge| &edge.definition),
            )
            .map(|edge| (&edge.from.component, &edge.to.component))
            .chain(self.subscriptions.iter().map(|(from, to)| (from, to)))
    }

    /// Immediate upstream components, including host-managed subscriptions.
    /// Unresolved producer identities remain visible until supplied or removed.
    pub fn data_dependencies(&self, component: &ComponentId) -> BTreeSet<ComponentId> {
        self.data_connections()
            .filter(|(_, to)| to == &component)
            .map(|(from, _)| from.clone())
            .collect()
    }

    pub fn data_dependents(&self, component: &ComponentId) -> BTreeSet<ComponentId> {
        self.data_connections()
            .filter(|(from, _)| from == &component)
            .map(|(_, to)| to.clone())
            .collect()
    }
}

enum Component {
    Source(Box<dyn EnvelopeSource>),
    Transformer(Box<dyn Transformer>),
    Query(Box<dyn Transformer>),
    Sink(Box<dyn EnvelopeSink>),
    Service(Box<dyn super::ComputationService>),
    Deferred {
        specification: Arc<ComponentSpecification>,
        factory: Arc<dyn ComponentFactory>,
    },
    Unresolved(Arc<DesiredComponent>),
}

impl Component {
    fn recovery_contract(&self) -> super::ComponentRecovery {
        match self {
            Self::Source(component) => {
                let mut contract = if let Some(admission) = component.admission() {
                    let mut contract =
                        super::ComponentRecovery::admitted(admission.channel().durability())
                            .replay_to_durable_delivery();
                    contract.source_admission = Some(admission);
                    contract
                } else {
                    component.recovery_contract()
                };
                contract.replay_progress = component.recovery_progress();
                contract
            }
            Self::Transformer(component) | Self::Query(component) => {
                let mut contract = component.recovery_contract();
                if matches!(
                    contract.processing,
                    super::recovery::ProcessingRecovery::Stateless
                ) && component.wakeup_source().is_some()
                {
                    contract.processing = super::recovery::ProcessingRecovery::StatelessWithWakeup;
                }
                contract
            }
            Self::Sink(component) => component.recovery_contract(),
            Self::Service(component) => component.recovery_contract(),
            Self::Deferred { .. } | Self::Unresolved(_) => super::ComponentRecovery::default(),
        }
    }

    fn query_api(&self) -> Option<Arc<super::QueryApi>> {
        match self {
            Self::Source(component) => component.query_api(),
            Self::Transformer(component) | Self::Query(component) => component.query_api(),
            Self::Sink(component) => component.query_api(),
            Self::Service(component) => component.query_api(),
            Self::Deferred { .. } | Self::Unresolved(_) => None,
        }
    }

    fn descriptor(&self) -> &ComponentDescriptor {
        match self {
            Self::Source(component) => component.descriptor(),
            Self::Transformer(component) => component.descriptor(),
            Self::Query(component) => component.descriptor(),
            Self::Sink(component) => component.descriptor(),
            Self::Service(component) => component.descriptor(),
            Self::Deferred { specification, .. } => &specification.descriptor,
            Self::Unresolved(definition) => &definition.descriptor,
        }
    }

    fn role(&self) -> ComponentRole {
        match self {
            Self::Source(_) => ComponentRole::Source,
            Self::Transformer(_) => ComponentRole::Transformer,
            Self::Query(_) => ComponentRole::Query,
            Self::Sink(_) => ComponentRole::Sink,
            Self::Service(_) => ComponentRole::Service,
            Self::Deferred { specification, .. } => specification.role,
            Self::Unresolved(definition) => definition.role,
        }
    }

    fn completion(&self) -> Option<SinkCompletion> {
        match self {
            Self::Sink(component) => Some(component.completion()),
            Self::Deferred { specification, .. } => specification.completion,
            Self::Unresolved(definition) => definition.completion,
            _ => None,
        }
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Source(component) => component.start().await,
            Self::Transformer(component) => component.start().await,
            Self::Query(component) => component.start().await,
            Self::Sink(component) => component.start().await,
            Self::Service(component) => component.start().await,
            Self::Deferred { .. } | Self::Unresolved(_) => {
                Err(anyhow::anyhow!("component has not been constructed"))
            }
        }
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Source(component) => component.stop().await,
            Self::Transformer(component) => component.stop().await,
            Self::Query(component) => component.stop().await,
            Self::Sink(component) => component.stop().await,
            Self::Service(component) => component.stop().await,
            Self::Deferred { .. } | Self::Unresolved(_) => {
                Err(anyhow::anyhow!("component has not been constructed"))
            }
        }
    }

    async fn deprovision(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Source(component) => component.deprovision().await,
            Self::Transformer(component) | Self::Query(component) => component.deprovision().await,
            Self::Sink(component) => component.deprovision().await,
            Self::Service(component) => component.deprovision().await,
            Self::Deferred { .. } | Self::Unresolved(_) => {
                anyhow::bail!("component has not been constructed")
            }
        }
    }

    async fn reconfigure(&mut self, context: ConstructionContext) -> anyhow::Result<()> {
        match self {
            Self::Source(component) => component.reconfigure(context).await,
            Self::Transformer(component) | Self::Query(component) => {
                component.reconfigure(context).await
            }
            Self::Sink(component) => component.reconfigure(context).await,
            Self::Service(component) => component.reconfigure(context).await,
            Self::Deferred { .. } | Self::Unresolved(_) => {
                Err(anyhow::anyhow!("component has not been constructed"))
            }
        }
    }

    fn bind_control(&mut self, control: super::ComponentControl) {
        match self {
            Self::Source(component) => component.bind_control(control),
            Self::Transformer(component) | Self::Query(component) => {
                component.bind_control(control)
            }
            Self::Sink(component) => component.bind_control(control),
            Self::Service(component) => component.bind_control(control),
            Self::Deferred { .. } | Self::Unresolved(_) => {}
        }
    }

    #[cfg(feature = "test-support")]
    fn bind_query_test_support(
        &mut self,
        inspector: &super::ComputationInspector,
        generation: super::ComponentGeneration,
    ) {
        if let Self::Transformer(component) | Self::Query(component) = self {
            component.bind_query_test_support(inspector, generation);
        }
    }

    fn control_handler(&self) -> Option<Arc<dyn super::ControlHandler>> {
        match self {
            Self::Source(component) => component.control_handler(),
            Self::Transformer(component) | Self::Query(component) => component.control_handler(),
            Self::Sink(component) => component.control_handler(),
            Self::Service(component) => component.control_handler(),
            Self::Deferred { .. } | Self::Unresolved(_) => None,
        }
    }

    fn requires_readiness_confirmation(&self) -> bool {
        match self {
            Self::Source(component) => component.requires_readiness_confirmation(),
            Self::Transformer(component) | Self::Query(component) => {
                component.requires_readiness_confirmation()
            }
            Self::Sink(component) => component.requires_readiness_confirmation(),
            Self::Service(component) => component.requires_readiness_confirmation(),
            Self::Deferred { .. } | Self::Unresolved(_) => false,
        }
    }
}

/// Entire topology is validated by `build` before any provider creation or start.
///
/// Policy: nonempty DAG; unique component IDs within this graph, port IDs within
/// each component, edges and output stream IDs within the graph. Every port must
/// be connected and every output port must bind exactly one stream. Sources have
/// only outputs, sinks only inputs, transformers both. Disconnected *complete*
/// pipelines are permitted; isolated nodes/unconnected ports are not. Graph IDs
/// are local descriptive IDs (there is no process-wide graph registry).
/// One producer output cannot route to multiple input ports of the same component:
/// independent queues would not preserve component-wide FIFO for that stream.
pub struct ComputationGraphBuilder {
    id: Arc<str>,
    components: Vec<Component>,
    edges: Vec<(EdgeDefinition, Box<dyn PipeProvider>)>,
    streams: Vec<(Endpoint, StreamId)>,
    requirements: PipeRequirements,
    recovery_requirements: Vec<super::RecoveryRequirement>,
    cleanup_timeout: Duration,
    lifecycle_policies: BTreeMap<ComponentId, LifecyclePolicy>,
    relationship_policies: BTreeMap<EdgeDefinition, RelationshipPolicy>,
    resources: BTreeMap<ResourceId, ResourceSpecification>,
    resource_configurations: BTreeMap<ResourceId, serde_json::Value>,
    resource_dependencies: BTreeMap<ResourceId, BTreeMap<ResourceId, ResourceRole>>,
    resource_handles: BTreeMap<ResourceId, ResourceHandle>,
    input_merge: BTreeMap<ComponentId, InputMergePolicy>,
    unbound_relationships: Vec<DesiredRelationship>,
    readiness_required: BTreeSet<ComponentId>,
    allow_empty: bool,
}

impl ComputationGraphBuilder {
    pub fn require_recovery(mut self, requirement: super::RecoveryRequirement) -> Self {
        self.recovery_requirements.push(requirement);
        self
    }

    /// Build component declarations and bindings without starting their execution.
    pub fn build_components(self) -> GraphResult<super::ComponentBatch> {
        self.build()?.into_component_batch()
    }

    pub(crate) fn for_additions(mut self) -> Self {
        self.allow_empty = true;
        self
    }

    pub fn require_downstream_ready(mut self, component: ComponentId) -> Self {
        self.readiness_required.insert(component);
        self
    }
    pub fn input_merge(mut self, component: ComponentId, policy: InputMergePolicy) -> Self {
        self.input_merge.insert(component, policy);
        self
    }

    pub fn component(
        mut self,
        specification: ComponentSpecification,
        factory: Arc<dyn ComponentFactory>,
    ) -> Self {
        self.components.push(Component::Deferred {
            specification: Arc::new(specification),
            factory,
        });
        self
    }

    pub fn declare_resource(mut self, specification: ResourceSpecification) -> GraphResult<Self> {
        validate_identifier("resource binding", &specification.binding)?;
        if self
            .resources
            .insert(specification.id.clone(), specification)
            .is_some()
        {
            return Err(topology("duplicate resource specification"));
        }
        Ok(self)
    }

    pub fn resource_configuration(
        mut self,
        resource: ResourceId,
        configuration: serde_json::Value,
    ) -> GraphResult<Self> {
        if !configuration.is_object() {
            return Err(topology("resource configuration must be an object"));
        }
        if self
            .resource_configurations
            .insert(resource, configuration)
            .is_some()
        {
            return Err(topology("duplicate resource configuration"));
        }
        Ok(self)
    }

    pub fn provide_resource(
        mut self,
        id: ResourceId,
        resource: ResourceHandle,
    ) -> GraphResult<Self> {
        if self.resource_handles.insert(id, resource).is_some() {
            return Err(topology("duplicate constructed resource binding"));
        }
        Ok(self)
    }

    /// Retain the dependency until the dependent resource has finished cleanup.
    pub fn resource_dependency(
        mut self,
        resource: ResourceId,
        dependency: ResourceId,
        role: ResourceRole,
    ) -> GraphResult<Self> {
        if self
            .resource_dependencies
            .entry(resource)
            .or_default()
            .insert(dependency, role)
            .is_some()
        {
            return Err(topology("duplicate resource dependency"));
        }
        Ok(self)
    }

    pub fn source(mut self, source: Box<dyn EnvelopeSource>) -> Self {
        self.components.push(Component::Source(source));
        self
    }

    pub fn transformer(mut self, transformer: Box<dyn Transformer>) -> Self {
        self.components.push(Component::Transformer(transformer));
        self
    }

    pub fn query(mut self, query: Box<dyn Transformer>) -> Self {
        self.components.push(Component::Query(query));
        self
    }

    pub fn sink(mut self, sink: Box<dyn EnvelopeSink>) -> Self {
        self.components.push(Component::Sink(sink));
        self
    }

    pub fn service(mut self, service: Box<dyn super::ComputationService>) -> Self {
        self.components.push(Component::Service(service));
        self
    }
    pub(crate) fn unbound(mut self, relationship: DesiredRelationship) -> Self {
        self.unbound_relationships.push(relationship);
        self
    }

    pub fn connect(mut self, edge: EdgeDefinition, provider: Box<dyn PipeProvider>) -> Self {
        self.edges.push((edge, provider));
        self
    }

    pub fn bind_stream(mut self, output: Endpoint, stream: StreamId) -> Self {
        self.streams.push((output, stream));
        self
    }

    pub fn lifecycle_policy(mut self, component: ComponentId, policy: LifecyclePolicy) -> Self {
        self.lifecycle_policies.insert(component, policy);
        self
    }

    pub fn relationship_policy(mut self, edge: EdgeDefinition, policy: RelationshipPolicy) -> Self {
        self.relationship_policies.insert(edge, policy);
        self
    }

    pub fn requirements(mut self, requirements: PipeRequirements) -> Self {
        self.requirements = requirements;
        self
    }

    /// Shared deadline for each cleanup pass. Every attempted component's stop
    /// is polled, even if an earlier hook exhausts the deadline. Timeouts/errors
    /// leave CleanupRequired; caller drop cannot await asynchronous cleanup.
    pub fn cleanup_timeout(mut self, timeout: Duration) -> Self {
        self.cleanup_timeout = timeout;
        self
    }

    pub fn build(self) -> GraphResult<ComputationGraph> {
        validate_identifier("graph", &self.id)?;
        resources::construction_order(self.resources.values(), &self.resource_dependencies)?;
        resources::validate_constructed_dependencies(
            &self.resource_dependencies,
            &self.resource_handles,
        )?;
        if self
            .resource_configurations
            .keys()
            .any(|id| !self.resources.contains_key(id))
        {
            return Err(topology(
                "resource configuration names an undeclared resource",
            ));
        }
        if self.components.is_empty() && !self.allow_empty {
            return Err(topology("graph must be nonempty"));
        }
        if self.cleanup_timeout.is_zero()
            || std::time::Instant::now()
                .checked_add(self.cleanup_timeout)
                .is_none()
        {
            return Err(topology(
                "cleanup timeout must be nonzero and representable",
            ));
        }
        let mut ids = BTreeMap::new();
        let mut nodes = Vec::new();
        let mut transitive_sources = Vec::new();
        for (id, handle) in &self.resource_handles {
            let declaration = self
                .resources
                .get(id)
                .ok_or_else(|| topology(format!("resource {id} was not declared")))?;
            if declaration.role != handle.role() {
                return Err(topology(format!("resource {id} has an incompatible role")));
            }
        }
        for (index, component) in self.components.iter().enumerate() {
            let downstream: Vec<_> = self
                .edges
                .iter()
                .filter(|(edge, _)| &edge.from.component == component.descriptor().id())
                .map(|(edge, _)| edge.to.component.clone())
                .collect();
            let transitive = !self.recovery_requirements.is_empty()
                && matches!(component, Component::Source(source)
                if source.recovery_progress().is_some_and(|progress| {
                    downstream.iter().any(|id| id != progress.component_id())
                        && self.recovery_requirements.iter().any(|requirement| {
                            requirement.guarantees.iter().any(|guarantee| {
                                *guarantee != super::RecoveryGuarantee::Acceptance
                            })
                        })
                }));
            validate_source_progress(component, &self.id, &downstream, transitive)?;
            if transitive {
                transitive_sources.push(component.descriptor().id().clone());
            }
            if let Component::Deferred {
                specification,
                factory,
            } = component
            {
                specification::validate_specification(
                    &self.id,
                    specification,
                    factory.as_ref(),
                    &self.resources,
                    &self.resource_handles,
                )?;
            }
            let descriptor = component.descriptor();
            if ids.insert(descriptor.id().clone(), index).is_some() {
                return Err(topology(format!("duplicate component {}", descriptor.id())));
            }
            validate_role(descriptor, component.role(), component.completion())?;
            nodes.push(NodeSnapshot {
                descriptor: descriptor.clone(),
                role: component.role(),
                completion: component.completion(),
                output_streams: BTreeMap::new(),
                input_merge: self
                    .input_merge
                    .get(descriptor.id())
                    .copied()
                    .unwrap_or_default(),
            });
        }
        let mut streams = BTreeSet::new();
        for (endpoint, stream) in self.streams {
            let (index, port) = resolve(&ids, &nodes, &endpoint)?;
            if port.direction() != PortDirection::Output {
                return Err(topology("stream binding must name an output port"));
            }
            if !streams.insert(stream.clone())
                || nodes[index]
                    .output_streams
                    .insert(endpoint.port, stream)
                    .is_some()
            {
                return Err(topology("duplicate stream identity or output binding"));
            }
        }
        let mut connected = BTreeSet::new();
        for relationship in &self.unbound_relationships {
            validate_orphan(relationship, &ids, &nodes)?;
            for (id, role) in relationship.pipe.resource_dependencies() {
                if self.resources.get(&id).map(|resource| resource.role) != Some(role) {
                    return Err(topology(
                        "unbound relationship requires an undeclared or incompatible resource",
                    ));
                }
            }
            if ids.contains_key(&relationship.definition.from.component) {
                connected.insert(relationship.definition.from.clone());
            }
            if ids.contains_key(&relationship.definition.to.component) {
                connected.insert(relationship.definition.to.clone());
            }
        }
        let mut unique_edges = BTreeSet::new();
        let mut stream_destinations = BTreeSet::new();
        let mut edges = Vec::new();
        let mut used_resources = BTreeMap::new();
        let mut multicast_outputs = BTreeMap::new();
        let mut multicast_subscribers = BTreeSet::new();
        for (edge_index, (edge, provider)) in self.edges.iter().enumerate() {
            if let Some((resource, subscriber)) = provider.multicast_subscription() {
                if multicast_outputs
                    .get(&resource)
                    .is_some_and(|from| from != &edge.from)
                    || !multicast_subscribers.insert((resource.clone(), subscriber))
                {
                    return Err(topology(
                        "multicast requires one producer output and distinct subscriber identities",
                    ));
                }
                multicast_outputs.insert(resource, edge.from.clone());
            }
            let resources = provider.resource_dependencies();
            let exclusive = provider.exclusive_resources();
            for (id, role) in &resources {
                let declaration = self
                    .resources
                    .get(id)
                    .ok_or_else(|| topology(format!("pipe requires undeclared resource {id}")))?;
                if declaration.role != *role {
                    return Err(topology(format!(
                        "pipe resource {id} has an incompatible role"
                    )));
                }
                let exclusive = exclusive.contains(id);
                if used_resources
                    .get(id)
                    .is_some_and(|prior| *prior || exclusive)
                {
                    return Err(topology(format!(
                        "pipe resource {id} requires exclusive binding ownership"
                    )));
                }
                used_resources.insert(id.clone(), exclusive);
            }
            provider
                .validate_resources(&self.resource_handles)
                .map_err(|source| GraphError::Pipe {
                    edge: edge_index,
                    source,
                })?;
            if !unique_edges.insert(edge.clone()) {
                return Err(topology("duplicate edge"));
            }
            let (from, output) = resolve(&ids, &nodes, &edge.from)?;
            if let Some(DesiredPipe::Qos(config)) = provider.specification() {
                if nodes[from].output_streams.get(&edge.from.port)
                    != Some(&config.definition.stream)
                {
                    return Err(topology(
                        "QoS channel stream differs from its producer output",
                    ));
                }
            }
            let (to, input) = resolve(&ids, &nodes, &edge.to)?;
            if !stream_destinations.insert((edge.from.clone(), edge.to.component.clone())) {
                return Err(topology(
                    "one producer output cannot feed multiple input ports on the same component",
                ));
            }
            let capabilities = provider.capabilities().map_err(|source| GraphError::Pipe {
                edge: edge_index,
                source,
            })?;
            validate_edge_contract(
                output,
                input,
                nodes[to].completion,
                &capabilities,
                &self.requirements,
            )?;
            connected.insert(edge.from.clone());
            connected.insert(edge.to.clone());
            let pipe = provider
                .specification()
                .unwrap_or_else(|| DesiredPipe::External {
                    binding: format!("pipe:{edge_index}"),
                    capabilities: capabilities.supported().iter().copied().collect(),
                    capacity: capabilities.capacity().map(std::num::NonZeroUsize::get),
                    resources: resources.clone(),
                    exclusive_resources: provider.exclusive_resources(),
                });
            if pipe.resource_dependencies() != resources {
                return Err(topology(
                    "pipe recipe omits its declared resource dependencies",
                ));
            }
            edges.push(EdgeSnapshot {
                definition: edge.clone(),
                pipe,
                capabilities,
                policy: self
                    .relationship_policies
                    .get(edge)
                    .cloned()
                    .unwrap_or_default(),
                resources,
            });
        }
        for node in &nodes {
            for port in node.descriptor.ports() {
                if !self.allow_empty
                    && !connected.contains(&Endpoint::new(
                        node.descriptor.id().clone(),
                        port.id().clone(),
                    ))
                {
                    return Err(topology("every declared port must be connected"));
                }
                if port.direction() == PortDirection::Output
                    && !node.output_streams.contains_key(port.id())
                    && !self.allow_empty
                {
                    return Err(topology("every output port must bind a stream"));
                }
            }
        }
        let order = dependency_order(&ids, &edges)?;
        if self
            .lifecycle_policies
            .keys()
            .any(|id| !ids.contains_key(id))
            || self
                .relationship_policies
                .keys()
                .any(|edge| !unique_edges.contains(edge))
            || self.input_merge.keys().any(|id| !ids.contains_key(id))
            || self
                .readiness_required
                .iter()
                .any(|id| !ids.contains_key(id))
        {
            return Err(topology(
                "lifecycle policy names an undeclared component or relationship",
            ));
        }
        let lifecycle_policies = nodes
            .iter()
            .map(|node| {
                let id = node.descriptor.id().clone();
                let policy = self
                    .lifecycle_policies
                    .get(&id)
                    .cloned()
                    .unwrap_or_default();
                (id, policy)
            })
            .collect();
        let snapshot = GraphSnapshot {
            id: self.id,
            revision: GraphRevision(1),
            nodes: nodes.clone().into(),
            edges: edges.into(),
            requirements: self.requirements,
            recovery_requirements: self.recovery_requirements,
            lifecycle_policies,
            specifications: self
                .components
                .iter()
                .filter_map(|component| {
                    if let Component::Deferred { specification, .. } = component {
                        Some((
                            specification.descriptor.id().clone(),
                            specification.as_ref().clone(),
                        ))
                    } else {
                        None
                    }
                })
                .collect(),
            external_bindings: self
                .components
                .iter()
                .filter_map(|component| {
                    if matches!(component, Component::Deferred { .. }) {
                        None
                    } else {
                        Some((
                            component.descriptor().id().clone(),
                            Arc::from(component.descriptor().id().as_str()),
                        ))
                    }
                })
                .collect(),
            resources: self.resources,
            resource_configurations: self.resource_configurations,
            resource_dependencies: self.resource_dependencies,
            unbound_relationships: self.unbound_relationships.into(),
            control_connections: Arc::from([]),
            subscriptions: Arc::from([]),
            readiness_required: self.readiness_required,
            component_resources: BTreeMap::new(),
            component_plugins: BTreeMap::new(),
            allow_incomplete: self.allow_empty,
        };
        for (component, node) in self.components.iter().zip(&nodes) {
            if let Some(admission) = component.recovery_contract().source_admission {
                validate_admission(&admission, node, &snapshot, &self.resource_handles, None)?;
            }
        }
        let observed = controller::initial_observations(&snapshot);
        let factories = self
            .components
            .iter()
            .filter_map(|component| {
                if let Component::Deferred {
                    specification,
                    factory,
                } = component
                {
                    Some((specification.implementation.clone(), factory.clone()))
                } else {
                    None
                }
            })
            .collect();
        let components: Vec<_> = self
            .components
            .into_iter()
            .enumerate()
            .map(|(index, component)| {
                controller::InstanceSlot::new(component, ComponentGeneration(index as u64 + 1))
            })
            .collect();
        let configurations = ids
            .iter()
            .map(|(id, index)| (id.clone(), components[*index].configuration()))
            .collect();
        let registry = watch::channel(Arc::new(GraphRegistrySnapshot::new(
            &snapshot,
            Arc::new(observed.clone()),
            &self.resource_handles,
            configurations,
            ids.iter()
                .filter_map(|(id, index)| {
                    components[*index].query_api().map(|api| (id.clone(), api))
                })
                .collect(),
        )))
        .0;
        let graph = ComputationGraph {
            execution_scope: snapshot.id.clone(),
            inspector: super::ComputationInspector::new(&snapshot, Arc::new(observed.clone())),
            next_edge: snapshot.edges.len(),
            next_binding_generation: 1,
            next_resource_generation: 2,
            edges: snapshot.edges.iter().cloned().enumerate().collect(),
            next_generation: nodes.len() as u64 + 1,
            nodes,
            factories,
            desired: watch::channel(Arc::new(snapshot.clone())).0,
            snapshot,
            observed: watch::channel(Arc::new(observed)).0,
            components,
            providers: self
                .edges
                .into_iter()
                .map(|(_, provider)| provider)
                .enumerate()
                .collect(),
            order,
            ids,
            reserved_components: BTreeSet::new(),
            state: watch::channel(GraphState::Ready).0,
            cleanup_timeout: self.cleanup_timeout,
            resource_handles: self.resource_handles,
            pending_resource_cleanup: BTreeMap::new(),
            registry,
            peers: super::ControlPlane::new(64)?,
            rejected_additions: Arc::new(std::sync::Mutex::new(Vec::new())),
            protected_components: BTreeSet::new(),
            protected_resources: BTreeSet::new(),
            deferred_activation: BTreeSet::new(),
            reported_resources: BTreeMap::new(),
            management_protected: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            retirement: None,
        };
        if !graph.snapshot.allow_incomplete {
            graph.validate_recovery(true)?;
        }
        for id in transitive_sources {
            if !graph.transitive_source_recovery(&id)? {
                return Err(topology(
                    "source progress requires a validated path to its owner",
                ));
            }
        }
        Ok(graph)
    }
}

fn topology(reason: impl Into<String>) -> GraphError {
    GraphError::Topology {
        reason: reason.into(),
    }
}

fn validate_orphan(
    relationship: &DesiredRelationship,
    ids: &BTreeMap<ComponentId, usize>,
    nodes: &[NodeSnapshot],
) -> GraphResult<()> {
    if !relationship.policy.orphan_permitted
        || relationship.policy.required_for_creation
        || relationship.policy.required_for_binding
        || relationship.policy.activation != super::ActivationCoupling::Independent
        || relationship.policy.propagate_failure
        || relationship.policy.fence_producer_on_failure
    {
        return Err(topology(
            "unbound relationship does not permit an unsatisfied dependency",
        ));
    }
    let mut present = false;
    for (endpoint, direction) in [
        (&relationship.definition.from, PortDirection::Output),
        (&relationship.definition.to, PortDirection::Input),
    ] {
        if ids.contains_key(&endpoint.component) {
            if resolve(ids, nodes, endpoint)?.1.direction() != direction {
                return Err(topology(
                    "unbound relationship has an incompatible endpoint role",
                ));
            }
            present = true;
        }
    }
    if !present {
        return Err(topology("unbound relationship has no desired endpoint"));
    }
    Ok(())
}

fn resolve<'a>(
    ids: &BTreeMap<ComponentId, usize>,
    nodes: &'a [NodeSnapshot],
    endpoint: &Endpoint,
) -> GraphResult<(usize, &'a PortDescriptor)> {
    let index = *ids
        .get(&endpoint.component)
        .ok_or_else(|| topology(format!("unknown component {}", endpoint.component)))?;
    let port = nodes[index]
        .descriptor
        .ports()
        .iter()
        .find(|port| port.id() == &endpoint.port)
        .ok_or_else(|| topology(format!("unknown port {endpoint:?}")))?;
    Ok((index, port))
}

/// Cloneable, generation-specific stop/status handle. It never owns data senders.
/// Cancellation is idempotent. Await the run to actually finish cleanup.
#[derive(Clone)]
pub struct GraphControl {
    cancel: watch::Sender<bool>,
    state: watch::Receiver<GraphState>,
    commands: controller::CommandSender,
    observed: watch::Receiver<Arc<ObservedGraph>>,
    desired: watch::Receiver<Arc<GraphSnapshot>>,
    inspector: super::ComputationInspector,
    peers: Arc<super::ControlPlane>,
    registry: watch::Receiver<Arc<GraphRegistrySnapshot>>,
    rejected_additions: Arc<std::sync::Mutex<Vec<Arc<addition::RejectedAddition>>>>,
    management_protected: Arc<std::sync::atomic::AtomicBool>,
    management_access: bool,
}

impl GraphControl {
    pub(crate) fn protect_configuration(&self) {
        self.management_protected
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(crate) fn management_control(&self) -> Self {
        let mut control = self.clone();
        control.management_access = true;
        control
    }

    pub(crate) fn require_configuration_write(&self) -> GraphResult<()> {
        if !self.management_access
            && self
                .management_protected
                .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(topology(
                "managed configuration must be changed through DrasiLib::apply_desired_state",
            ));
        }
        Ok(())
    }

    pub fn inspector(&self) -> super::ComputationInspector {
        self.inspector.clone()
    }
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
        if let Err(error) = self.peers.close() {
            log::error!("Could not close component control channels during cancellation: {error}");
        }
    }

    pub fn state(&self) -> GraphState {
        *self.state.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<GraphState> {
        self.state.clone()
    }
}

/// Authoritative DAG runtime, usable standalone or owned by [`crate::DrasiLib`].
///
/// `start` deploys and requests automatic startup in a caller-polled run; `run`
/// deploys without automatic activation and keeps its command controller open.
/// No graph worker is spawned. Drive the scope with `.await`, `join!`, or
/// `select!`; merely constructing it does not run components.
/// A failed start does not stop independently activated components or turn its
/// bound outgoing pipes into EOF. The control handle exposes per-item outcomes.
/// Instance leases serialize mutable calls without holding a mutex across await.
///
/// Only natural drain followed by successful stop permits a new run. Boxes and
/// stream high-watermarks are retained; a fresh generation creates fresh pipes.
/// Failed component activation can be explicitly stopped and retried through the
/// controller; no automatic retry or rollback is implied. Dropping a run
/// cancels its scoped futures/pipes immediately but cannot call async hooks;
/// `shutdown().await` must then be used to finish cleanup before dropping the
/// graph. Components owning their own external workers must stop them in hooks.
pub struct ComputationGraph {
    snapshot: GraphSnapshot,
    // Stable runtime indices survive compact desired snapshots and removals.
    nodes: Vec<NodeSnapshot>,
    edges: BTreeMap<usize, EdgeSnapshot>,
    components: Vec<Arc<controller::InstanceSlot>>,
    providers: BTreeMap<usize, Box<dyn PipeProvider>>,
    factories: BTreeMap<ImplementationIdentity, Arc<dyn ComponentFactory>>,
    next_generation: u64,
    next_edge: usize,
    next_binding_generation: u64,
    next_resource_generation: u64,
    order: Vec<usize>,
    ids: BTreeMap<ComponentId, usize>,
    reserved_components: BTreeSet<Arc<str>>,
    observed: watch::Sender<Arc<ObservedGraph>>,
    desired: watch::Sender<Arc<GraphSnapshot>>,
    state: watch::Sender<GraphState>,
    cleanup_timeout: Duration,
    resource_handles: BTreeMap<ResourceId, ResourceHandle>,
    pending_resource_cleanup: BTreeMap<ResourceId, ResourceHandle>,
    registry: watch::Sender<Arc<GraphRegistrySnapshot>>,
    inspector: super::ComputationInspector,
    execution_scope: Arc<str>,
    peers: Arc<super::ControlPlane>,
    rejected_additions: Arc<std::sync::Mutex<Vec<Arc<addition::RejectedAddition>>>>,
    deferred_activation: BTreeSet<ComponentId>,
    protected_components: BTreeSet<ComponentId>,
    protected_resources: BTreeSet<ResourceId>,
    reported_resources: BTreeMap<ComponentId, BTreeSet<ResourceId>>,
    management_protected: Arc<std::sync::atomic::AtomicBool>,
    retirement: Option<Arc<retirement::RetirementState>>,
}

impl ComputationGraph {
    pub(crate) fn into_component_batch(self) -> GraphResult<super::ComponentBatch> {
        assert_eq!(
            self.state(),
            GraphState::Ready,
            "component assembly must not be running"
        );
        let definition = self.snapshot.select(super::GraphSelection::All)?;
        let mut bindings = TopologyBindings {
            resources: self.resource_handles,
            factories: FactoryRegistry {
                factories: self.factories,
            },
            ..Default::default()
        };
        for slot in self.components {
            let component = slot.take_unstarted();
            if !matches!(component, Component::Deferred { .. }) {
                bindings.components.insert(
                    component.descriptor().id().as_str().to_owned(),
                    ConstructedComponent(component),
                );
            }
        }
        for (index, provider) in self.providers {
            if let DesiredPipe::External { binding, .. } = &self.edges[&index].pipe {
                bindings.pipes.insert(binding.clone(), provider);
            }
        }
        Ok(super::ComponentBatch {
            definition,
            bindings,
        })
    }
}

impl ComputationGraph {
    pub(crate) fn reserve_component_id(&mut self, id: &str) {
        self.reserved_components.insert(Arc::from(id));
    }

    pub(crate) fn set_execution_scope(&mut self, scope: Arc<str>) {
        self.execution_scope = scope;
    }
    pub fn inspector(&self) -> super::ComputationInspector {
        self.inspector.clone()
    }
    pub fn builder(id: impl Into<Arc<str>>) -> ComputationGraphBuilder {
        ComputationGraphBuilder {
            id: id.into(),
            components: Vec::new(),
            edges: Vec::new(),
            streams: Vec::new(),
            requirements: PipeRequirements::default(),
            recovery_requirements: Vec::new(),
            cleanup_timeout: Duration::from_secs(5),
            lifecycle_policies: BTreeMap::new(),
            relationship_policies: BTreeMap::new(),
            resources: BTreeMap::new(),
            resource_configurations: BTreeMap::new(),
            resource_dependencies: BTreeMap::new(),
            resource_handles: BTreeMap::new(),
            input_merge: BTreeMap::new(),
            unbound_relationships: Vec::new(),
            readiness_required: BTreeSet::new(),
            allow_empty: false,
        }
    }

    /// Create an empty graph whose controller can accept individual additions.
    /// Use `run()` to keep it open while adding nodes and connecting their ports.
    pub fn empty(id: impl Into<Arc<str>>) -> GraphResult<Self> {
        Self::builder(id).for_additions().build()
    }

    pub fn snapshot(&self) -> &GraphSnapshot {
        &self.snapshot
    }

    /// Assess the actual constructed participants, including ordinary data
    /// subscriptions. This does not enable recovery or change processing.
    pub fn recovery_report(
        &self,
        requirement: &super::RecoveryRequirement,
    ) -> GraphResult<super::RecoveryPathReport> {
        self.recovery_report_in(&self.snapshot, requirement)
    }

    fn recovery_report_in(
        &self,
        snapshot: &GraphSnapshot,
        requirement: &super::RecoveryRequirement,
    ) -> GraphResult<super::RecoveryPathReport> {
        let participants =
            recovery::participants(&requirement.consumer, snapshot.data_connections());
        let contracts = participants
            .iter()
            .filter_map(|id| self.ids.get(id).map(|index| (id, index)))
            .map(|(id, index)| {
                self.components[*index]
                    .recovery_contract()
                    .map(|contract| (id.clone(), contract))
            })
            .collect::<GraphResult<BTreeMap<_, _>>>()?;
        Ok(recovery::assess(
            snapshot,
            &self.resource_handles,
            &contracts,
            requirement,
        ))
    }

    fn validate_recovery(&self, allow_unconstructed: bool) -> GraphResult<()> {
        for requirement in &self.snapshot.recovery_requirements {
            if requirement.guarantees.is_empty() || !self.ids.contains_key(&requirement.consumer) {
                return Err(topology(
                    "recovery assertion requires a known consumer and at least one guarantee",
                ));
            }
            let report = self.recovery_report(requirement)?;
            let mut pending = false;
            if allow_unconstructed {
                for id in &report.participants {
                    if let Some(index) = self.ids.get(id) {
                        pending |= !self.components[*index].is_constructed()?;
                    }
                }
            }
            if pending {
                continue;
            }
            report.validate()?;
        }
        Ok(())
    }

    fn validate_recovery_activation(
        &self,
        component: &ComponentId,
    ) -> GraphResult<Option<super::RecoveryRequirement>> {
        let mut selected = None;
        for requirement in &self.snapshot.recovery_requirements {
            if !recovery::participants(&requirement.consumer, self.snapshot.data_connections())
                .contains(component)
            {
                continue;
            }
            self.recovery_report(requirement)?.validate()?;
            selected = Some(requirement.clone());
        }
        Ok(selected)
    }

    fn transitive_source_recovery(&self, component: &ComponentId) -> GraphResult<bool> {
        if self.snapshot.recovery_requirements.is_empty() {
            return Ok(false);
        }
        let contract = self.components[self.ids[component]].recovery_contract()?;
        let Some(progress) = &contract.replay_progress else {
            return Ok(false);
        };
        if contract.replay_until.as_ref() != Some(progress.component_id()) {
            return Ok(false);
        }
        for requirement in &self.snapshot.recovery_requirements {
            if requirement
                .guarantees
                .iter()
                .any(|guarantee| *guarantee != super::RecoveryGuarantee::Acceptance)
            {
                let report = self.recovery_report(requirement)?;
                if report.participants.contains(component)
                    && report.participants.contains(progress.component_id())
                {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    pub fn configuration_snapshot(&self) -> GraphResult<GraphConfigurationSnapshot> {
        self.registry.borrow().configuration_snapshot()
    }

    pub fn state(&self) -> GraphState {
        *self.state.borrow()
    }

    pub fn observed(&self) -> Arc<ObservedGraph> {
        self.observed.borrow().clone()
    }

    /// Exclusive borrowing prevents duplicate concurrent starts at compile time.
    /// Terminal states also reject starts at runtime without altering resources.
    ///
    /// ```compile_fail
    /// # use drasi_lib::computation::v1::ComputationGraph;
    /// fn duplicate(graph: &mut ComputationGraph) {
    ///     let first = graph.start().unwrap();
    ///     let second = graph.start(); // cannot borrow graph twice
    ///     drop(first);
    /// }
    /// ```
    pub fn start(&mut self) -> GraphResult<GraphRun<'_>> {
        self.open_run(true)
    }

    /// Drive deployment and live commands without automatically starting components.
    /// The controller remains available until explicitly cancelled.
    pub fn run(&mut self) -> GraphResult<GraphRun<'_>> {
        self.open_run(false)
    }

    fn open_run(&mut self, auto_start: bool) -> GraphResult<GraphRun<'_>> {
        let state = self.state();
        if !matches!(state, GraphState::Ready | GraphState::Completed) {
            return Err(GraphError::InvalidState { state });
        }
        self.state = watch::channel(GraphState::Starting).0;
        let mut observed = (*self.observed()).clone();
        observed.run_epoch = observed
            .run_epoch
            .checked_add(1)
            .ok_or_else(|| topology("graph run epoch exhausted"))?;
        observed.startup = None;
        observed.deployment = None;
        self.observed = watch::channel(Arc::new(observed)).0;
        self.inspector.publish(&self.snapshot, self.observed());
        self.desired = watch::channel(Arc::new(self.snapshot.clone())).0;
        registry::publish(self);
        self.peers = super::ControlPlane::new(64)?;
        let (cancel, cancellation) = watch::channel(false);
        let (commands, receiver) = controller::command_channel();
        let control = GraphControl {
            cancel,
            state: self.state.subscribe(),
            commands,
            observed: self.observed.subscribe(),
            desired: self.desired.subscribe(),
            inspector: self.inspector.clone(),
            peers: self.peers.clone(),
            registry: self.registry.subscribe(),
            rejected_additions: self.rejected_additions.clone(),
            management_protected: self.management_protected.clone(),
            management_access: false,
        };
        let state = self.state.clone();
        let observed = self.observed.clone();
        let inspector = self.inspector.clone();
        let registry = self.registry.clone();
        Ok(GraphRun {
            future: self.execute(cancellation, receiver, auto_start).boxed(),
            control,
            state,
            finished: false,
            observed,
            inspector,
            registry,
        })
    }

    /// Await remaining async stop hooks after a dropped run/failed cleanup.
    /// This does not roll back state or make the graph restartable. Repeated
    /// calls after successful cleanup do not call stop twice. Retrying a failed
    /// stop explicitly asks the implementor to finish its incomplete cleanup.
    pub async fn shutdown(&mut self) -> GraphResult<()> {
        if !controller::needs_cleanup(self)? {
            if self.state() == GraphState::CleanupRequired {
                self.state.send_replace(GraphState::Cancelled);
            }

            return Ok(());
        }
        self.state.send_replace(GraphState::CleanupRequired);
        let errors = controller::cleanup(self).await;
        if errors.is_empty() {
            self.state.send_replace(GraphState::Cancelled);
            Ok(())
        } else {
            Err(GraphError::Cleanup {
                primary: None,
                errors,
            })
        }
    }

    /// Release graph-owned external resources after scoped component cleanup.
    /// Borrowed resources are never shut down. Failed cleanup remains registered.
    pub async fn dispose(&mut self) -> GraphResult<()> {
        self.shutdown().await?;
        self.state.send_replace(GraphState::CleanupRequired);
        let rejected = self
            .rejected_additions
            .lock()
            .map_err(|_| topology("rejected addition cleanup registry is poisoned"))?
            .clone();
        let mut errors = Vec::new();
        for addition in rejected {
            if let Err(error) = addition
                .dispose(&self.resource_handles, self.cleanup_timeout)
                .await
            {
                errors.push(error);
            }
        }
        self.rejected_additions
            .lock()
            .map_err(|_| topology("rejected addition cleanup registry is poisoned"))?
            .retain(|addition| !addition.complete());
        if !errors.is_empty() {
            return Err(GraphError::Cleanup {
                primary: None,
                errors,
            });
        }
        controller::dispose_resources(self).await?;
        self.state.send_replace(GraphState::Cancelled);
        Ok(())
    }

    async fn execute(
        &mut self,
        mut cancel: watch::Receiver<bool>,
        commands: controller::CommandReceiver,
        auto_start: bool,
    ) -> GraphResult<()> {
        let result = controller::run(self, &mut cancel, commands, auto_start).await;
        self.state.send_replace(GraphState::Stopping);
        let mut errors = controller::cleanup(self).await;
        if let Err(error) = self.peers.close() {
            errors.push(GraphError::Control(error));
        }
        if !errors.is_empty() {
            self.state.send_replace(GraphState::CleanupRequired);
            return Err(GraphError::Cleanup {
                primary: result.err().map(Box::new),
                errors,
            });
        }
        self.state.send_replace(match &result {
            Ok(()) => GraphState::Completed,
            Err(GraphError::Cancelled) => GraphState::Cancelled,
            Err(_) => GraphState::Failed,
        });
        result
    }
}

/// Future owning every scoped operation for one generation. Drop cancels, but
/// never claims asynchronous stop hooks have run. See `ComputationGraph::shutdown`.
#[must_use = "a computation run must be polled; await it to drive and clean up the graph"]
pub struct GraphRun<'a> {
    future: BoxFuture<'a, GraphResult<()>>,
    control: GraphControl,
    state: watch::Sender<GraphState>,
    finished: bool,
    observed: watch::Sender<Arc<ObservedGraph>>,
    inspector: super::ComputationInspector,
    registry: watch::Sender<Arc<GraphRegistrySnapshot>>,
}

impl GraphRun<'_> {
    pub fn control(&self) -> GraphControl {
        self.control.clone()
    }
}

impl Future for GraphRun<'_> {
    type Output = GraphResult<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = self.future.as_mut().poll(cx);
        if result.is_ready() {
            self.finished = true;
        }
        result
    }
}

impl Drop for GraphRun<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.control.cancel();
            self.state.send_replace(GraphState::CleanupRequired);
            controller::mark_cleanup_required(&self.observed);
            self.inspector.publish(
                &self.control.desired_snapshot(),
                self.observed.borrow().clone(),
            );
            registry::publish_observed(&self.registry, self.observed.borrow().clone());
        }
    }
}

struct PipeGuard(BTreeMap<usize, Arc<dyn PipeControl>>);

impl Drop for PipeGuard {
    fn drop(&mut self) {
        for control in self.0.values() {
            control.cancel();
        }
    }
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    let _ = cancel.wait_for(|value| *value).await;
}

struct Incoming {
    edge: usize,
    port: PortId,
    receiver: Box<dyn EnvelopeReceiver>,
    acknowledgement_required: bool,
    pending: Option<PendingInput>,
    exhausted: bool,
    progress: Arc<FlowProgress>,
}

struct Outgoing {
    edge: usize,
    port: PortId,
    sender: Arc<dyn EnvelopeSender>,
    progress: Arc<FlowProgress>,
    multicast: Option<ResourceId>,
    admission_channel: Option<Arc<super::QosChannel>>,
    destination: Option<Arc<super::OutputDestination>>,
}

fn output_destination(
    edge: &EdgeDefinition,
    pipe: &DesiredPipe,
    resources: &BTreeMap<ResourceId, ResourceHandle>,
) -> anyhow::Result<Option<Arc<super::OutputDestination>>> {
    let DesiredPipe::Qos(config) = pipe else {
        return Ok(None);
    };
    let channel = resources
        .get(&config.resource)
        .ok_or_else(|| anyhow::anyhow!("output channel resource is missing"))?
        .get::<super::QosChannel>()?;
    let Some(journal) = channel.output_journal_identity() else {
        return Ok(None);
    };
    Ok(Some(Arc::new(super::OutputDestination {
        output: edge.from.port.clone(),
        consumer: edge.to.component.clone(),
        input: edge.to.port.clone(),
        journal,
        subscriber: config.subscriber.clone(),
    })))
}

fn validate_output_bindings(
    state: &super::output_bindings::OutputBindingState,
    node: &NodeSnapshot,
    edges: &[EdgeSnapshot],
    resources: &BTreeMap<ResourceId, ResourceHandle>,
) -> GraphResult<()> {
    if edges.iter().any(|edge| {
        &edge.definition.from.component == node.descriptor.id()
            && matches!(&edge.pipe, DesiredPipe::Qos(config) if !resources.contains_key(&config.resource))
    }) {
        return state.validate_unresolved()
            .map_err(|error| component_error(node, "validate unresolved output destinations", error.into()));
    }
    let bindings = output_bindings(node, edges, resources)?;
    state
        .validate(&bindings)
        .map_err(|error| component_error(node, "validate output destinations", error.into()))
}

fn output_bindings(
    node: &NodeSnapshot,
    edges: &[EdgeSnapshot],
    resources: &BTreeMap<ResourceId, ResourceHandle>,
) -> GraphResult<super::OutputBindings> {
    let result = (|| {
        let destinations = edges
            .iter()
            .filter(|edge| &edge.definition.from.component == node.descriptor.id())
            .filter_map(|edge| {
                output_destination(&edge.definition, &edge.pipe, resources).transpose()
            })
            .take(super::output_bindings::MAX_DESTINATIONS + 1)
            .map(|destination| destination.map(|destination| (*destination).clone()))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut bindings = super::OutputBindings::try_new(destinations)?;
        for edge in edges
            .iter()
            .filter(|edge| &edge.definition.from.component == node.descriptor.id())
        {
            if let DesiredPipe::Qos(config) = &edge.pipe {
                let channel = resources
                    .get(&config.resource)
                    .ok_or_else(|| anyhow::anyhow!("output channel resource is missing"))?
                    .get::<super::QosChannel>()?;
                bindings.attach_shared(&edge.definition.from.port, &channel)?;
            }
        }
        Ok(bindings)
    })();
    result.map_err(|error| component_error(node, "validate output destinations", error))
}

async fn bind_output_destinations(
    component: &mut Component,
    node: &NodeSnapshot,
    outputs: &[Outgoing],
    snapshot: &GraphSnapshot,
    resources: &BTreeMap<ResourceId, ResourceHandle>,
) -> GraphResult<()> {
    if component.recovery_contract().output_bindings.is_none() {
        if outputs.iter().any(|output| {
            output
                .admission_channel
                .as_ref()
                .is_some_and(|channel| channel.is_shared())
        }) {
            return Err(topology(
                "shared QoS output requires a producer with transactional destination tracking",
            ));
        }
        return Ok(());
    }
    let mut bindings = super::OutputBindings::try_new(
        outputs
            .iter()
            .filter_map(|output| output.destination.as_deref().cloned()),
    )
    .map_err(|error| component_error(node, "bind output destinations", error.into()))?;
    for output in outputs {
        if let Some(channel) = &output.admission_channel {
            bindings
                .attach_shared(&output.port, channel)
                .map_err(|error| component_error(node, "bind shared output", error.into()))?;
        }
    }
    if bindings != output_bindings(node, &snapshot.edges, resources)? {
        return Err(component_error(
            node,
            "bind output destinations",
            super::OutputBindingError::Invalid("a required output destination is not bound".into())
                .into(),
        ));
    }
    match component {
        Component::Transformer(transformer) | Component::Query(transformer) => transformer
            .bind_output_destinations(&bindings)
            .await
            .map_err(|error| component_error(node, "bind output destinations", error)),
        _ => Err(topology(
            "output destination tracking requires a transformer",
        )),
    }
}

fn admission_channel(
    provider: &dyn PipeProvider,
    resources: &BTreeMap<ResourceId, super::ResourceHandle>,
) -> GraphResult<Option<Arc<super::QosChannel>>> {
    let Some(DesiredPipe::Qos(config)) = provider.specification() else {
        return Ok(None);
    };
    let resource = resources
        .get(&config.resource)
        .ok_or_else(|| topology("QoS admission resource is missing"))?;
    resource
        .get::<super::QosChannel>()
        .map(Some)
        .map_err(|source| GraphError::ResourceCreation {
            resource: config.resource,
            source: source.into(),
        })
}

#[derive(Default)]
struct FlowProgress {
    in_flight: AtomicUsize,
    notify: tokio::sync::Notify,
}

impl FlowProgress {
    async fn drained(&self, control: &dyn PipeControl) -> GraphResult<()> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.in_flight.load(Ordering::Acquire) == 0
                && control
                    .is_idle()
                    .await
                    .map_err(|source| GraphError::Pipe { edge: 0, source })?
            {
                return Ok(());
            }
            notified.await;
        }
    }
}

struct DeliveryWork(Arc<FlowProgress>);

impl DeliveryWork {
    fn new(progress: Arc<FlowProgress>) -> GraphResult<Self> {
        progress
            .in_flight
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| topology("in-flight delivery counter exhausted"))?;
        progress.notify.notify_one();
        Ok(Self(progress))
    }
}

impl Drop for DeliveryWork {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.0.notify.notify_one();
    }
}

struct PendingInput {
    delivery: Delivery,
    work: DeliveryWork,
}

async fn receive(input: &mut Incoming) -> (&mut Incoming, std::result::Result<(), PipeError>) {
    let result = if input.pending.is_some() || input.exhausted {
        Ok(())
    } else {
        match input.receiver.receive().await {
            Ok(delivery) => {
                input.exhausted = delivery.is_none();
                if let Some(delivery) = delivery {
                    match DeliveryWork::new(input.progress.clone()) {
                        Ok(work) => input.pending = Some(PendingInput { delivery, work }),
                        Err(error) => return (input, Err(PipeError::Backend(error.into()))),
                    }
                }
                Ok(())
            }
            Err(error) => Err(error),
        }
    };
    (input, result)
}

fn component_error(
    node: &NodeSnapshot,
    operation: &'static str,
    source: anyhow::Error,
) -> GraphError {
    GraphError::Component {
        component: node.descriptor.id().clone(),
        operation,
        source,
    }
}

fn emission_error(node: &NodeSnapshot, reason: impl Into<String>) -> GraphError {
    GraphError::Emission {
        component: node.descriptor.id().clone(),
        reason: reason.into(),
    }
}

fn check_descriptor(component: &Component, node: &NodeSnapshot) -> GraphResult<()> {
    if component.descriptor() != &node.descriptor || component.completion() != node.completion {
        return Err(emission_error(
            node,
            "descriptor or sink completion changed",
        ));
    }
    Ok(())
}

fn validate_source_progress(
    component: &Component,
    graph_id: &str,
    downstream: &[ComponentId],
    transitive: bool,
) -> GraphResult<()> {
    let Component::Source(source) = component else {
        return Ok(());
    };
    if source.admission().is_some() && source.recovery_progress().is_some() {
        return Err(topology(
            "source admission cannot also own a separate replay boundary",
        ));
    }
    if let Some(progress) = source.recovery_progress() {
        if progress.graph_id() != graph_id
            || !transitive && downstream.iter().any(|id| id != progress.component_id())
        {
            return Err(topology(
                "source recovery progress must belong to its immediate consumer on every branch",
            ));
        }
    }
    Ok(())
}

fn validate_admission(
    admission: &super::SourceAdmission,
    node: &NodeSnapshot,
    snapshot: &GraphSnapshot,
    resources: &BTreeMap<ResourceId, ResourceHandle>,
    scope: Option<&str>,
) -> GraphResult<()> {
    if admission.component_id() != node.descriptor.id()
        || admission.identity().graph_id() != snapshot.id.as_ref()
        || scope.is_some_and(|scope| scope != admission.identity().construction_scope())
        || node
            .descriptor
            .ports()
            .iter()
            .any(|port| port.direction() == PortDirection::Output && port.id() != admission.port())
        || snapshot
            .subscriptions
            .iter()
            .any(|(from, _)| from == node.descriptor.id())
        || snapshot
            .unbound_relationships
            .iter()
            .any(|edge| &edge.definition.from.component == node.descriptor.id())
    {
        return Err(emission_error(
            node,
            "source admission identity or publication boundary mismatch",
        ));
    }
    let mut outputs = snapshot
        .edges
        .iter()
        .filter(|edge| &edge.definition.from.component == node.descriptor.id())
        .peekable();
    if outputs.peek().is_none() && snapshot.allow_incomplete && scope.is_none() {
        return Ok(());
    }
    if outputs.peek().is_none()
        || node.output_streams.get(admission.port()) != Some(admission.identity().stream())
        || outputs.any(|edge| {
            let DesiredPipe::Qos(config) = &edge.pipe else {
                return true;
            };
            edge.definition.from.port != *admission.port()
                || config.gap_policy != super::ReplayGapPolicy::Strict
                || !admission.channel().matches_definition(config)
                || !resources
                    .get(&config.resource)
                    .and_then(|handle| handle.get::<super::QosChannel>().ok())
                    .is_some_and(|channel| Arc::ptr_eq(&channel, admission.channel()))
        })
    {
        return Err(emission_error(
            node,
            "source admission requires its actual outgoing QoS channel on every branch",
        ));
    }
    Ok(())
}

fn event_time_selection(
    times: impl Iterator<Item = Option<chrono::DateTime<chrono::Utc>>>,
) -> usize {
    times
        .enumerate()
        .take_while(|(_, time)| time.is_some())
        .min_by_key(|(index, time)| (*time, *index))
        .map(|(index, _)| index)
        .unwrap_or(0)
}

struct SourceRouting<'a> {
    graph_id: &'a str,
    downstream: &'a [ComponentId],
    transitive_recovery: bool,
}

// Bound synchronous work per poll without adding periodic yields to nodes that
// already suspended for I/O. Nodes share their controller's Tokio task budget.
#[derive(Default)]
struct NodeWorkBudget(std::sync::atomic::AtomicU8);

impl NodeWorkBudget {
    fn checkpoint(&self) -> Option<impl Future<Output = ()>> {
        let work = self.0.load(Ordering::Relaxed) + 1;
        if work < 64 {
            self.0.store(work, Ordering::Relaxed);
            return None;
        }
        self.0.store(0, Ordering::Relaxed);
        let mut yielded = false;
        Some(futures::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                // Requeue this node, not the whole controller's Tokio task.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }))
    }

    async fn run<T>(&self, work: impl Future<Output = T>) -> T {
        futures::pin_mut!(work);
        futures::future::poll_fn(|cx| {
            let result = work.as_mut().poll(cx);
            if result.is_pending() {
                self.0.store(0, Ordering::Relaxed);
            }
            result
        })
        .await
    }
}

async fn run_admitted_source(
    component: &Component,
    node: &NodeSnapshot,
    sequences: &mut BTreeMap<PortId, u64>,
    outputs: &[Outgoing],
    quiesce: &mut watch::Receiver<bool>,
    budget: &NodeWorkBudget,
    admission: Arc<super::SourceAdmission>,
) -> GraphResult<()> {
    if admission.component_id() != node.descriptor.id()
        || node.output_streams.get(admission.port())
            != Some(&admission.channel().definition().stream)
        || outputs.is_empty()
        || outputs.iter().any(|output| {
            output.port != *admission.port()
                || !output
                    .admission_channel
                    .as_ref()
                    .is_some_and(|channel| Arc::ptr_eq(channel, admission.channel()))
        })
    {
        return Err(emission_error(
            node,
            "source admission requires its actual outgoing QoS channel on every branch",
        ));
    }
    let mut lease = admission
        .bind()
        .map_err(|error| component_error(node, "bind admission", error.into()))?;
    loop {
        if let Some(yield_node) = budget.checkpoint() {
            yield_node.await;
        }
        check_descriptor(component, node)?;
        let mut pending = sequences.clone();
        let accepted = tokio::select! {
            biased;
            _ = cancelled(quiesce) => return Ok(()),
            result = lease.process(|envelope| {
                validate_emissions(node, &mut pending, &[OutputEnvelope {
                    port: admission.port().clone(), envelope: envelope.clone(),
                }]).map_err(Into::into)
            }) => result.map_err(|error| component_error(node, "admit", error.into()))?,
        };
        if accepted {
            *sequences = pending;
        }
    }
}

async fn run_node(
    component: &mut Component,
    node: &NodeSnapshot,
    sequences: &mut BTreeMap<PortId, u64>,
    inputs: &mut [Incoming],
    outputs: &[Outgoing],
    quiesce: &mut watch::Receiver<bool>,
    routing: SourceRouting<'_>,
    budget: &NodeWorkBudget,
) -> GraphResult<()> {
    validate_source_progress(
        component,
        routing.graph_id,
        routing.downstream,
        routing.transitive_recovery,
    )?;
    if let Component::Service(service) = component {
        tokio::select! {
            biased;
            _ = cancelled(quiesce) => {}
            result = service.run() => return result.map_err(|error| component_error(node, "run service", error)),
        }
        return service
            .quiesce()
            .await
            .map_err(|error| component_error(node, "quiesce service", error));
    }
    let wakeup = match &*component {
        Component::Transformer(transformer) | Component::Query(transformer) => {
            transformer.wakeup_source()
        }
        _ => None,
    };
    if inputs.is_empty() && !matches!(component, Component::Source(_)) && wakeup.is_none() {
        cancelled(quiesce).await;
        return Ok(());
    }
    let mut receivers: FuturesUnordered<_> = inputs
        .iter_mut()
        .filter(|input| !input.exhausted)
        .map(receive)
        .collect();
    let mut ready: Vec<&mut Incoming> = Vec::new();
    if let Component::Source(source) = component {
        if let Some(admission) = source.admission() {
            // Keep the opt-in journal future out of every fast node's state.
            return Box::pin(run_admitted_source(
                component, node, sequences, outputs, quiesce, budget, admission,
            ))
            .await;
        }
    }
    'processing: loop {
        if let Some(yield_node) = budget.checkpoint() {
            yield_node.await;
        }
        if *quiesce.borrow() {
            return Ok(());
        }
        let mut acknowledgement = None;
        let mut consumed = None;
        let mut emissions = match component {
            Component::Source(source) => {
                match tokio::select! {
                    biased;
                    _ = cancelled(quiesce) => return Ok(()),
                    result = source.next() => result.map_err(|source| component_error(node, "next", source))?,
                } {
                    Some(output) => vec![output],
                    None => {
                        check_descriptor(component, node)?;
                        return Ok(());
                    }
                }
            }
            _ => 'input_work: {
                if let Component::Transformer(transformer) | Component::Query(transformer) =
                    component
                {
                    if transformer.has_pending_emissions() {
                        break 'input_work transformer.continue_transform().await.map_err(
                            |source| component_error(node, "recovered processing", source),
                        )?;
                    }
                }
                if ready.is_empty() {
                    let (scheduled, incoming) = tokio::select! {
                        biased;
                        _ = cancelled(quiesce) => return Ok(()),
                        result = async {
                        if let Some(wakeup) = &wakeup {
                        if receivers.is_empty() {
                            if !wakeup
                                .has_pending()
                                .await
                                .map_err(|source| component_error(node, "scheduled work", source))?
                            {
                                return Ok::<_, GraphError>((false, None));
                            }
                            wakeup.wait().await.map_err(|source| {
                                component_error(node, "scheduled wait", source)
                            })?;
                            Ok((true, None))
                        } else {
                            tokio::select! {
                                result = wakeup.wait() => {
                                    result.map_err(|source| component_error(node, "scheduled wait", source))?;
                                    Ok((true, None))
                                }
                                incoming = receivers.next() => Ok((false, incoming)),
                            }
                        }
                    } else {
                        Ok((false, receivers.next().await))
                    }
                    } => result?,
                    };
                    if scheduled {
                        match component {
                            Component::Transformer(transformer) | Component::Query(transformer) => {
                                break 'input_work transformer.on_wakeup().await.map_err(
                                    |source| component_error(node, "scheduled processing", source),
                                )?;
                            }
                            _ => return Err(topology("non-transformer received scheduled work")),
                        }
                    }
                    let Some((incoming, delivery)) = incoming else {
                        return Ok(());
                    };
                    delivery.map_err(|source| GraphError::Pipe {
                        edge: incoming.edge,
                        source,
                    })?;
                    if incoming.pending.is_none() {
                        continue 'processing;
                    }
                    ready.push(incoming);
                }
                if node.input_merge == InputMergePolicy::EventTimeAcrossStreams {
                    while let Some(Some((incoming, delivery))) = receivers.next().now_or_never() {
                        delivery.map_err(|source| GraphError::Pipe {
                            edge: incoming.edge,
                            source,
                        })?;
                        if incoming.pending.is_some() {
                            ready.push(incoming);
                        }
                    }
                }
                let chosen = if node.input_merge == InputMergePolicy::EventTimeAcrossStreams {
                    event_time_selection(ready.iter().map(|incoming| {
                        incoming
                            .pending
                            .as_ref()
                            .expect("ready delivery")
                            .delivery
                            .envelope()
                            .system()
                            .timestamp()
                    }))
                } else {
                    0
                };
                let incoming = ready.remove(chosen);
                let PendingInput { delivery, work } =
                    incoming.pending.take().expect("ready delivery");
                let (envelope, completion) = delivery.into_parts();
                if completion.is_some() != incoming.acknowledgement_required {
                    return Err(topology(
                        "pipe delivery acknowledgement differs from its negotiated capability",
                    ));
                }
                let port = node
                    .descriptor
                    .ports()
                    .iter()
                    .find(|port| port.id() == &incoming.port)
                    .ok_or_else(|| topology("delivery targets an undeclared input port"))?;
                validate_schema(port.schema(), envelope.changes().schema())?;
                if completion.is_some() && node.completion == Some(SinkCompletion::Accepted) {
                    return Err(topology(
                        "acceptance-only sink cannot acknowledge completed handling",
                    ));
                }
                acknowledgement = completion.map(|completion| (incoming.edge, completion));
                consumed = Some(work);
                let input = InputEnvelope {
                    port: incoming.port.clone(),
                    envelope,
                };
                // At most one pending receive per edge, no forwarding tasks or hidden queues.
                receivers.push(receive(incoming));
                match component {
                    Component::Transformer(transformer) | Component::Query(transformer) => {
                        transformer
                            .transform(input)
                            .await
                            .map_err(|source| component_error(node, "transform", source))?
                    }
                    Component::Sink(sink) => {
                        sink.handle(input)
                            .await
                            .map_err(|source| component_error(node, "handle", source))?;
                        Vec::new()
                    }
                    Component::Source(_) | Component::Service(_) => unreachable!(),
                    Component::Deferred { .. } | Component::Unresolved(_) => {
                        return Err(topology("cannot process an unconstructed component"))
                    }
                }
            }
        };
        loop {
            check_descriptor(component, node)?;
            validate_emissions(node, sequences, &emissions)?;
            // The input/continuation already paid for its first forward.
            let mut first_branch = true;
            for emission in &emissions {
                if !outputs.iter().any(|output| output.port == emission.port) {
                    return Err(emission_error(
                        node,
                        "output port has no bound relationship",
                    ));
                }
                let mut multicast = BTreeSet::new();
                let mut accepted_branches = 0;
                for output in outputs.iter().filter(|output| output.port == emission.port) {
                    if first_branch {
                        first_branch = false;
                    } else if let Some(yield_node) = budget.checkpoint() {
                        yield_node.await;
                    }
                    if output
                        .multicast
                        .as_ref()
                        .is_some_and(|group| !multicast.insert(group.clone()))
                    {
                        continue;
                    }
                    output
                        .sender
                        .send(emission.envelope.clone())
                        .await
                        .map_err(|source| GraphError::Forward {
                            edge: output.edge,
                            accepted_branches,
                            source: Box::new(source),
                        })?;
                    accepted_branches += match &output.multicast {
                        Some(group) => outputs
                            .iter()
                            .filter(|branch| {
                                branch.port == emission.port
                                    && branch.multicast.as_ref() == Some(group)
                            })
                            .count(),
                        None => 1,
                    };
                }
            }
            match component {
                Component::Transformer(transformer) | Component::Query(transformer) => {
                    transformer
                        .delivery_completed(&emissions)
                        .await
                        .map_err(|source| component_error(node, "complete delivery", source))?;
                }
                _ => {}
            }
            let continuation = match component {
                Component::Transformer(transformer) | Component::Query(transformer)
                    if transformer.has_pending_emissions() =>
                {
                    if let Some(yield_node) = budget.checkpoint() {
                        yield_node.await;
                    }
                    Some(tokio::select! {
                        biased;
                        _ = cancelled(quiesce) => return Ok(()),
                        result = transformer.continue_transform() =>
                            result.map_err(|source| component_error(node, "continue transform", source))?,
                    })
                }
                _ => None,
            };
            match continuation {
                Some(next) => emissions = next,
                None => break,
            }
        }
        if let Some((edge, acknowledgement)) = acknowledgement {
            acknowledgement
                .complete(super::HandlingOutcome::Handled)
                .await
                .map_err(|source| GraphError::Pipe { edge, source })?;
        }
        drop(consumed);
    }
}

fn validate_emissions(
    node: &NodeSnapshot,
    sequences: &mut BTreeMap<PortId, u64>,
    emissions: &[OutputEnvelope],
) -> GraphResult<()> {
    let mut pending = sequences.clone();
    let mut identities = std::collections::HashSet::new();
    for emission in emissions {
        let port = node
            .descriptor
            .ports()
            .iter()
            .find(|port| port.id() == &emission.port)
            .ok_or_else(|| emission_error(node, "unknown output port"))?;
        if port.direction() != PortDirection::Output {
            return Err(emission_error(node, "emission targets an input port"));
        }
        validate_schema(port.schema(), emission.envelope.changes().schema())?;
        if node.output_streams.get(&emission.port) != Some(emission.envelope.system().stream()) {
            return Err(emission_error(
                node,
                "stream is not owned by this producer/output port",
            ));
        }
        let sequence = emission.envelope.system().sequence();
        if pending
            .get(&emission.port)
            .is_some_and(|previous| sequence <= *previous)
        {
            return Err(emission_error(
                node,
                "output sequence must strictly increase",
            ));
        }
        if !identities.insert(emission.envelope.id()) {
            return Err(emission_error(
                node,
                "duplicate logical envelope ID in transform result",
            ));
        }
        pending.insert(emission.port.clone(), sequence);
    }
    *sequences = pending;
    Ok(())
}

/// Optional collision-free ID convention for producers: stream namespace plus
/// big-endian sequence bytes. Opaque caller IDs are also accepted; callers retain
/// their uniqueness obligation. The runtime checks authoritative (stream, sequence)
/// with bounded per-port state, not an unbounded global logical-ID deduplication set.
pub fn emission_id(stream: &StreamId, sequence: u64) -> super::Result<EnvelopeId> {
    EnvelopeId::try_new(
        stream.as_str(),
        bytes::Bytes::copy_from_slice(&sequence.to_be_bytes()),
    )
}

#[cfg(test)]
mod work_budget_tests {
    use super::NodeWorkBudget;
    use std::{
        future::Future,
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        task::{Context, Wake, Waker},
    };

    #[derive(Default)]
    struct NodeWake(AtomicUsize);

    impl Wake for NodeWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn unexhausted_work_does_not_construct_a_yield_future() {
        let budget = NodeWorkBudget::default();
        for _ in 0..63 {
            assert!(budget.checkpoint().is_none());
        }
        assert!(budget.checkpoint().is_some());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn exhausted_work_requeues_the_node_without_a_runtime_turn() {
        let budget = NodeWorkBudget::default();
        let work = budget.run(async {
            for _ in 0..64 {
                if let Some(yield_node) = budget.checkpoint() {
                    yield_node.await;
                }
            }
        });
        tokio::pin!(work);
        let wake = Arc::new(NodeWake::default());
        let waker = Waker::from(wake.clone());
        let mut context = Context::from_waker(&waker);
        assert!(work.as_mut().poll(&mut context).is_pending());
        assert_eq!(wake.0.load(Ordering::Relaxed), 1);
        assert!(work.as_mut().poll(&mut context).is_ready());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn ready_work_still_yields_at_the_per_poll_limit() {
        let budget = NodeWorkBudget::default();
        let work = budget.run(async {
            for _ in 0..128 {
                if let Some(yield_node) = budget.checkpoint() {
                    yield_node.await;
                }
            }
        });
        tokio::pin!(work);
        assert!(futures::poll!(&mut work).is_pending());
        assert!(futures::poll!(&mut work).is_pending());
        assert!(futures::poll!(&mut work).is_ready());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn suspended_work_does_not_accumulate_artificial_yields() {
        let budget = NodeWorkBudget::default();
        let work = budget.run(async {
            for _ in 0..128 {
                tokio::task::yield_now().await;
                if let Some(yield_node) = budget.checkpoint() {
                    yield_node.await;
                }
            }
        });
        tokio::pin!(work);
        for _ in 0..128 {
            assert!(futures::poll!(&mut work).is_pending());
        }
        assert!(futures::poll!(&mut work).is_ready());
    }
}

#[cfg(test)]
mod merge_tests {
    use super::event_time_selection;

    #[test]
    fn untimed_heads_keep_their_arrival_position() {
        let early = chrono::DateTime::from_timestamp(10, 0);
        let late = chrono::DateTime::from_timestamp(50, 0);
        assert_eq!(event_time_selection([late, None, early].into_iter()), 0);
        assert_eq!(event_time_selection([None, early, late].into_iter()), 0);
        assert_eq!(event_time_selection([late, early, None].into_iter()), 1);
    }
}
