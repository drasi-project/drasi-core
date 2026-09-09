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
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use futures::{future::BoxFuture, stream::FuturesUnordered, FutureExt, StreamExt};
use tokio::sync::{mpsc, watch};

mod controller;
mod specification;
pub use specification::*;

use super::{
    data::{validate_identifier, validate_schema},
    validate_connection, validate_sink_completion, ComponentDescriptor, ComponentGeneration,
    ComponentId, ContractError, Delivery, EnvelopeId, EnvelopeReceiver, EnvelopeSender,
    EnvelopeSink, EnvelopeSource, GraphRevision, InputEnvelope, LifecyclePolicy, ObservedGraph,
    OutputEnvelope, PipeCapabilities, PipeCapability, PipeControl, PipeError, PipeProvider,
    PipeRequirements, PortDescriptor, PortDirection, PortId, RelationshipPolicy, ResourceId,
    SendFailure, SinkCompletion, StreamId, Transformer,
};

pub type GraphResult<T> = std::result::Result<T, GraphError>;

/// Errors retain the original component/provider cause. A send failure may follow
/// successful delivery to other branches; no rollback or retry is implied.
#[derive(Debug, thiserror::Error)]
pub enum GraphError {
    #[error(transparent)]
    Contract(#[from] ContractError),
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
    #[error("a conflicting component lifecycle operation is still in progress")]
    OperationInProgress,
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
    #[error("edge {edge} rejected emission after {accepted_branches} accepted branches: {source}")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentRole {
    Source,
    Transformer,
    Query,
    Sink,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Endpoint {
    pub component: ComponentId,
    pub port: PortId,
}

impl Endpoint {
    pub fn new(component: ComponentId, port: PortId) -> Self {
        Self { component, port }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
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
}

#[derive(Debug, Clone)]
pub struct EdgeSnapshot {
    pub definition: EdgeDefinition,
    pub capabilities: PipeCapabilities,
    pub policy: RelationshipPolicy,
}

#[derive(Debug, Clone)]
pub struct GraphSnapshot {
    pub id: Arc<str>,
    pub revision: GraphRevision,
    pub nodes: Arc<[NodeSnapshot]>,
    pub edges: Arc<[EdgeSnapshot]>,
    pub requirements: PipeRequirements,
    pub lifecycle_policies: BTreeMap<ComponentId, LifecyclePolicy>,
    pub specifications: BTreeMap<ComponentId, ComponentSpecification>,
    pub external_bindings: BTreeMap<ComponentId, Arc<str>>,
    pub resources: BTreeMap<ResourceId, ResourceSpecification>,
}

enum Component {
    Source(Box<dyn EnvelopeSource>),
    Transformer(Box<dyn Transformer>),
    Query(Box<dyn Transformer>),
    Sink(Box<dyn EnvelopeSink>),
    Deferred {
        specification: Arc<ComponentSpecification>,
        factory: Arc<dyn ComponentFactory>,
    },
}

impl Component {
    fn descriptor(&self) -> &ComponentDescriptor {
        match self {
            Self::Source(component) => component.descriptor(),
            Self::Transformer(component) => component.descriptor(),
            Self::Query(component) => component.descriptor(),
            Self::Sink(component) => component.descriptor(),
            Self::Deferred { specification, .. } => &specification.descriptor,
        }
    }

    fn role(&self) -> ComponentRole {
        match self {
            Self::Source(_) => ComponentRole::Source,
            Self::Transformer(_) => ComponentRole::Transformer,
            Self::Query(_) => ComponentRole::Query,
            Self::Sink(_) => ComponentRole::Sink,
            Self::Deferred { specification, .. } => specification.role,
        }
    }

    fn completion(&self) -> Option<SinkCompletion> {
        match self {
            Self::Sink(component) => Some(component.completion()),
            Self::Deferred { specification, .. } => specification.completion,
            _ => None,
        }
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Source(component) => component.start().await,
            Self::Transformer(component) => component.start().await,
            Self::Query(component) => component.start().await,
            Self::Sink(component) => component.start().await,
            Self::Deferred { .. } => Err(anyhow::anyhow!("component has not been constructed")),
        }
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Source(component) => component.stop().await,
            Self::Transformer(component) => component.stop().await,
            Self::Query(component) => component.stop().await,
            Self::Sink(component) => component.stop().await,
            Self::Deferred { .. } => Err(anyhow::anyhow!("component has not been constructed")),
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
    cleanup_timeout: Duration,
    lifecycle_policies: BTreeMap<ComponentId, LifecyclePolicy>,
    relationship_policies: BTreeMap<EdgeDefinition, RelationshipPolicy>,
    resources: BTreeMap<ResourceId, ResourceSpecification>,
    resource_handles: BTreeMap<ResourceId, ResourceHandle>,
}

impl ComputationGraphBuilder {
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
        if self.components.is_empty() {
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
            if let Component::Deferred {
                specification,
                factory,
            } = component
            {
                specification::validate_specification(
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
            let inputs = descriptor
                .ports()
                .iter()
                .filter(|port| port.direction() == PortDirection::Input)
                .count();
            let outputs = descriptor.ports().len() - inputs;
            let valid = match component.role() {
                ComponentRole::Source => inputs == 0 && outputs > 0,
                ComponentRole::Transformer | ComponentRole::Query => inputs > 0 && outputs > 0,
                ComponentRole::Sink => inputs > 0 && outputs == 0,
            };
            if !valid {
                return Err(topology(format!(
                    "invalid role/ports for {}",
                    descriptor.id()
                )));
            }
            nodes.push(NodeSnapshot {
                descriptor: descriptor.clone(),
                role: component.role(),
                completion: component.completion(),
                output_streams: BTreeMap::new(),
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
        let mut unique_edges = BTreeSet::new();
        let mut stream_destinations = BTreeSet::new();
        let mut successors = vec![Vec::new(); nodes.len()];
        let mut indegree = vec![0; nodes.len()];
        let mut edges = Vec::new();
        for (edge_index, (edge, provider)) in self.edges.iter().enumerate() {
            if !unique_edges.insert(edge.clone()) {
                return Err(topology("duplicate edge"));
            }
            let (from, output) = resolve(&ids, &nodes, &edge.from)?;
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
            if let Some(completion) = nodes[to].completion {
                for requirements in
                    [&self.requirements, output.requirements(), input.requirements()]
                {
                    validate_sink_completion(completion, requirements)?;
                }
            }
            validate_connection(output, input, &capabilities)?;
            capabilities.validate(&self.requirements)?;
            // B2 has no acknowledgement/transaction/recovery host protocol.
            for capability in capabilities.supported() {
                if !matches!(
                    capability,
                    PipeCapability::FifoPerStream | PipeCapability::Backpressure
                ) {
                    return Err(ContractError::UnsupportedCapability {
                        capability: *capability,
                    }
                    .into());
                }
            }
            capabilities.validate(&PipeRequirements::new([PipeCapability::Backpressure]))?;
            if capabilities.capacity().is_none() {
                return Err(topology(
                    "every edge must declare a finite nonzero capacity",
                ));
            }
            connected.insert(edge.from.clone());
            connected.insert(edge.to.clone());
            successors[from].push(to);
            indegree[to] += 1;
            edges.push(EdgeSnapshot {
                definition: edge.clone(),
                capabilities,
                policy: self
                    .relationship_policies
                    .get(edge)
                    .cloned()
                    .unwrap_or_default(),
            });
        }
        for node in &nodes {
            for port in node.descriptor.ports() {
                if !connected.contains(&Endpoint::new(
                    node.descriptor.id().clone(),
                    port.id().clone(),
                )) {
                    return Err(topology("every declared port must be connected"));
                }
                if port.direction() == PortDirection::Output
                    && !node.output_streams.contains_key(port.id())
                {
                    return Err(topology("every output port must bind a stream"));
                }
            }
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
        if order.len() != nodes.len() {
            return Err(topology("cycles (including self-loops) are not supported"));
        }
        if self
            .lifecycle_policies
            .keys()
            .any(|id| !ids.contains_key(id))
            || self
                .relationship_policies
                .keys()
                .any(|edge| !unique_edges.contains(edge))
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
            nodes: nodes.into(),
            edges: edges.into(),
            requirements: self.requirements,
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
        };
        let observed = controller::initial_observations(&snapshot);
        Ok(ComputationGraph {
            desired: watch::channel(Arc::new(snapshot.clone())).0,
            snapshot,
            observed: watch::channel(Arc::new(observed)).0,
            components: self
                .components
                .into_iter()
                .enumerate()
                .map(|(index, component)| {
                    controller::InstanceSlot::new(component, ComponentGeneration(index as u64 + 1))
                })
                .collect(),
            providers: self
                .edges
                .into_iter()
                .map(|(_, provider)| provider)
                .collect(),
            order,
            ids,
            state: watch::channel(GraphState::Ready).0,
            cleanup_timeout: self.cleanup_timeout,
            resource_handles: self.resource_handles,
        })
    }
}

fn topology(reason: impl Into<String>) -> GraphError {
    GraphError::Topology {
        reason: reason.into(),
    }
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
    commands: mpsc::Sender<controller::Command>,
    observed: watch::Receiver<Arc<ObservedGraph>>,
    desired: watch::Receiver<Arc<GraphSnapshot>>,
}

impl GraphControl {
    pub fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    pub fn state(&self) -> GraphState {
        *self.state.borrow()
    }

    pub fn subscribe(&self) -> watch::Receiver<GraphState> {
        self.state.clone()
    }
}

/// Standalone opt-in DAG, unrelated to legacy `ComponentGraph`/`DrasiLib`.
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
    components: Vec<Arc<controller::InstanceSlot>>,
    providers: Vec<Box<dyn PipeProvider>>,
    order: Vec<usize>,
    ids: BTreeMap<ComponentId, usize>,
    observed: watch::Sender<Arc<ObservedGraph>>,
    desired: watch::Sender<Arc<GraphSnapshot>>,
    state: watch::Sender<GraphState>,
    cleanup_timeout: Duration,
    resource_handles: BTreeMap<ResourceId, ResourceHandle>,
}

impl ComputationGraph {
    pub fn builder(id: impl Into<Arc<str>>) -> ComputationGraphBuilder {
        ComputationGraphBuilder {
            id: id.into(),
            components: Vec::new(),
            edges: Vec::new(),
            streams: Vec::new(),
            requirements: PipeRequirements::default(),
            cleanup_timeout: Duration::from_secs(5),
            lifecycle_policies: BTreeMap::new(),
            relationship_policies: BTreeMap::new(),
            resources: BTreeMap::new(),
            resource_handles: BTreeMap::new(),
        }
    }

    pub fn snapshot(&self) -> &GraphSnapshot {
        &self.snapshot
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
        self.desired = watch::channel(Arc::new(self.snapshot.clone())).0;
        let (cancel, cancellation) = watch::channel(false);
        let (commands, receiver) = mpsc::channel(64);
        let control = GraphControl {
            cancel,
            state: self.state.subscribe(),
            commands,
            observed: self.observed.subscribe(),
            desired: self.desired.subscribe(),
        };
        let state = self.state.clone();
        let observed = self.observed.clone();
        Ok(GraphRun {
            future: self.execute(cancellation, receiver, auto_start).boxed(),
            control,
            state,
            finished: false,
            observed,
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
        controller::dispose_resources(self).await?;
        self.state.send_replace(GraphState::Cancelled);
        Ok(())
    }

    async fn execute(
        &mut self,
        mut cancel: watch::Receiver<bool>,
        commands: mpsc::Receiver<controller::Command>,
        auto_start: bool,
    ) -> GraphResult<()> {
        let result = controller::run(self, &mut cancel, commands, auto_start).await;
        self.state.send_replace(GraphState::Stopping);
        let errors = controller::cleanup(self).await;
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
}

struct Outgoing {
    edge: usize,
    port: PortId,
    sender: Arc<dyn EnvelopeSender>,
}

async fn receive(
    input: &mut Incoming,
) -> (
    &mut Incoming,
    std::result::Result<Option<Delivery>, PipeError>,
) {
    let result = input.receiver.receive().await;
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

async fn run_node(
    component: &mut Component,
    node: &NodeSnapshot,
    sequences: &mut BTreeMap<PortId, u64>,
    inputs: &mut [Incoming],
    outputs: &[Outgoing],
) -> GraphResult<()> {
    if inputs.is_empty() && !matches!(component, Component::Source(_)) {
        return std::future::pending().await;
    }
    let mut receivers: FuturesUnordered<_> = inputs.iter_mut().map(receive).collect();
    loop {
        let emissions = match component {
            Component::Source(source) => {
                match source
                    .next()
                    .await
                    .map_err(|source| component_error(node, "next", source))?
                {
                    Some(output) => vec![output],
                    None => {
                        check_descriptor(component, node)?;
                        return Ok(());
                    }
                }
            }
            _ => {
                let Some((incoming, delivery)) = receivers.next().await else {
                    return Ok(());
                };
                let delivery = delivery.map_err(|source| GraphError::Pipe {
                    edge: incoming.edge,
                    source,
                })?;
                let Some(delivery) = delivery else {
                    continue;
                };
                let (envelope, acknowledgement) = delivery.into_parts();
                if acknowledgement.is_some() {
                    return Err(topology(
                        "volatile graph received an unsupported acknowledgement",
                    ));
                }
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
                    Component::Source(_) => unreachable!(),
                    Component::Deferred { .. } => {
                        return Err(topology("cannot process an unconstructed component"))
                    }
                }
            }
        };
        check_descriptor(component, node)?;
        validate_emissions(node, sequences, &emissions)?;
        for emission in emissions {
            if !outputs.iter().any(|output| output.port == emission.port) {
                return Err(emission_error(
                    node,
                    "output port has no bound relationship",
                ));
            }
            for (accepted_branches, output) in outputs
                .iter()
                .filter(|output| output.port == emission.port)
                .enumerate()
            {
                output
                    .sender
                    .send(emission.envelope.clone())
                    .await
                    .map_err(|source| GraphError::Forward {
                        edge: output.edge,
                        accepted_branches,
                        source: Box::new(source),
                    })?;
            }
        }
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
