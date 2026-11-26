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

//! Generic discovery and reading of source, query and transformer outputs.
//!
//! The catalogue owns nothing. It resolves the providers that components
//! registered per output port (see [`ComputationComponent::output_readers`]),
//! as captured from the graph's coherent registry publication. Handles carry
//! the graph run, component generation and reader epoch they resolved; every read re-checks
//! them against the current publication, so a stopped, restarted or replaced
//! component yields [`OutputReadError::Retired`] rather than a replacement's
//! data or an empty default.
//!
//! Reading a snapshot and then replaying from its cursor detects epoch changes
//! and retention gaps ([`OutputHandle::catch_up`]). That is not a gap-free live
//! handover. [`OutputHandle::snapshot_and_follow`] requires an actual registered
//! provider that captures the snapshot and its bounded live lease together.
//! [`OutputHandle::current_state`] and [`OutputHandle::current_state_and_follow`]
//! deliver those views as bounded batches of ordinary added records.

use super::*;
use futures::{stream, stream::BoxStream, StreamExt};
use std::{future::Future, sync::Arc};
use tokio::time::Instant;

mod projection;
use projection::ProjectionRoute;

type ReadResult<T> = std::result::Result<T, OutputReadError>;

pub(super) fn transfer_schema(
    declared: &SchemaDescriptor,
    factory: Option<&dyn ComponentFactory>,
) -> Option<Arc<Schema>> {
    [
        GraphChangeCodec::schema(),
        QueryChangeCodec::schema(),
        SourceTransactionCodec::schema(),
    ]
    .into_iter()
    .chain(
        factory
            .into_iter()
            .flat_map(|factory| factory.record_schemas()),
    )
    .find(|schema| schema.descriptor() == declared)
}

/// Whether an output actually registered a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provision {
    Provided,
    /// The component is constructed and registered no such provider.
    Absent,
    /// The component has not been constructed in this publication, so no
    /// provider can be known yet.
    NotConstructed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputDescription {
    pub endpoint: OutputEndpoint,
    /// The output port's declared schema; snapshots must use it.
    pub schema: SchemaDescriptor,
    /// One-shot current state, including independent provider-backed reads.
    pub current_state: Provision,
    /// A materialized view with a committed producer cursor.
    pub snapshot: Provision,
    pub replay: Provision,
    pub snapshot_follow: Provision,
}

/// Graph-owned view of every output's registered read providers.
#[derive(Clone)]
pub struct OutputCatalogue {
    registry: tokio::sync::watch::Receiver<Arc<GraphRegistrySnapshot>>,
    state: tokio::sync::watch::Receiver<GraphState>,
}

#[derive(Clone)]
struct Resolved {
    description: OutputDescription,
    stream: Option<StreamId>,
    generation: ComponentGeneration,
    run_epoch: u64,
    read_epoch: OperationEpoch,
    readers: Option<WeakOutputPortReaders>,
    independent: Option<ProviderAttachment>,
    projection: Option<Arc<ProjectionRoute>>,
}

#[derive(Clone)]
struct ProviderAttachment {
    edge: EdgeDefinition,
    binding_generation: u64,
    generation: ComponentGeneration,
    read_epoch: OperationEpoch,
    reader: std::sync::Weak<dyn IndependentStateProvider>,
}

impl ProviderAttachment {
    fn resolve(
        graph: &GraphRegistrySnapshot,
        represented: &OutputEndpoint,
        input: &PortId,
    ) -> ReadResult<(Provision, Option<Self>)> {
        let target = Endpoint::new(represented.component.clone(), input.clone());
        let mut edges = graph
            .observed
            .relationships
            .iter()
            .filter(|(edge, _)| edge.to == target);
        let Some((edge, binding)) = edges.next() else {
            return Ok((Provision::Absent, None));
        };
        if edges.next().is_some() {
            return Err(OutputReadError::invalid(format!(
                "ambiguous provider attachment for {represented}"
            )));
        }
        if binding.binding != BindingState::Bound {
            return Ok((Provision::NotConstructed, None));
        }
        let Some(provider) = graph.observed.components.get(&edge.from.component) else {
            return Ok((Provision::NotConstructed, None));
        };
        let reader = graph
            .output_readers(&edge.from.component)
            .and_then(|ports| ports.port(&edge.from.port))
            .and_then(WeakOutputPortReaders::independent_provider);
        Ok(match reader {
            None => (Provision::Absent, None),
            Some(reader) => (
                Provision::Provided,
                Some(Self {
                    edge: edge.clone(),
                    binding_generation: binding.generation,
                    generation: provider.generation,
                    read_epoch: provider.read_epoch,
                    reader: reader.clone(),
                }),
            ),
        })
    }

    fn is_current(&self, graph: &GraphRegistrySnapshot) -> bool {
        graph
            .observed
            .relationships
            .get(&self.edge)
            .is_some_and(|binding| {
                binding.binding == BindingState::Bound
                    && binding.generation == self.binding_generation
            })
            && graph
                .observed
                .components
                .get(&self.edge.from.component)
                .is_some_and(|provider| {
                    provider.generation == self.generation && provider.read_epoch == self.read_epoch
                })
            && graph
                .output_readers(&self.edge.from.component)
                .and_then(|ports| ports.port(&self.edge.from.port))
                .and_then(WeakOutputPortReaders::independent_provider)
                .is_some_and(|reader| reader.ptr_eq(&self.reader))
    }
}

impl OutputCatalogue {
    pub fn new(control: GraphControl) -> Self {
        control.output_catalogue()
    }

    pub(crate) fn from_publications(
        registry: tokio::sync::watch::Receiver<Arc<GraphRegistrySnapshot>>,
        state: tokio::sync::watch::Receiver<GraphState>,
    ) -> Self {
        Self { registry, state }
    }

    pub fn graph_id(&self) -> Arc<str> {
        self.registry.borrow().desired.id.clone()
    }

    /// Whether `other` views the same running graph. Graphs in different
    /// runtimes may share instance, graph and component IDs; this tells them
    /// apart.
    pub fn same_graph(&self, other: &Self) -> bool {
        self.registry.same_channel(&other.registry)
    }

    /// The graph's current desired topology: descriptive data only, with no
    /// providers, resolved secrets or live endpoints.
    pub fn desired_graph(&self) -> Arc<crate::computation::v1::GraphSnapshot> {
        self.registry.borrow().desired.clone()
    }

    /// Resolve an explicit output, or the factory's primary/single output.
    /// An omitted port never means "the first port" or an assumed `out`.
    pub fn resolve_endpoint(
        &self,
        component: &ComponentId,
        port: Option<&PortId>,
    ) -> ReadResult<OutputEndpoint> {
        let publication = self.registry_snapshot();
        Self::endpoint_at(&publication, component, port)
    }

    fn endpoint_at(
        publication: &GraphRegistrySnapshot,
        component: &ComponentId,
        port: Option<&PortId>,
    ) -> ReadResult<OutputEndpoint> {
        if !publication
            .desired
            .nodes
            .iter()
            .any(|node| node.descriptor.id() == component)
        {
            return Err(OutputReadError::UnknownComponent(component.clone()));
        }
        let endpoint = publication
            .output_endpoint(component, port)
            .map_err(|error| OutputReadError::Failed(error.into()))?;
        Ok(OutputEndpoint::new(endpoint.component, endpoint.port))
    }

    /// Resolve primary selection and its provider generation from the same
    /// publication, so a read cannot cross a concurrent primary-port change.
    pub fn resolve_component(
        &self,
        component: &ComponentId,
        port: Option<&PortId>,
    ) -> ReadResult<OutputHandle> {
        let publication = self.registry_snapshot();
        let endpoint = Self::endpoint_at(&publication, component, port)?;
        let resolved = self.resolve_at(&publication, &endpoint)?;
        self.handle(endpoint, resolved)
    }

    fn registry_snapshot(&self) -> Arc<GraphRegistrySnapshot> {
        self.registry.borrow().clone()
    }

    /// Every declared output port in one coherent publication. A declared
    /// output that cannot be described (a publication that omits its
    /// component's observation) fails the whole listing rather than yielding an
    /// incomplete inventory.
    pub fn list(&self) -> ReadResult<Vec<OutputDescription>> {
        let publication = self.registry_snapshot();
        let mut descriptions = Vec::new();
        for node in publication.desired.nodes.iter() {
            for port in node.descriptor.ports().iter().filter(|port| {
                port.direction() == PortDirection::Output && port.kind() == PortKind::Data
            }) {
                let endpoint = OutputEndpoint::new(node.descriptor.id().clone(), port.id().clone());
                descriptions.push(self.resolve_at(&publication, &endpoint)?.description);
            }
        }
        Ok(descriptions)
    }

    pub fn describe(&self, endpoint: &OutputEndpoint) -> ReadResult<OutputDescription> {
        self.resolve_at(&self.registry_snapshot(), endpoint)
            .map(|resolved| resolved.description)
    }

    /// Capture the endpoint's current providers. Fails with
    /// [`OutputReadError::NotReady`] while its component is not constructed.
    pub fn resolve(&self, endpoint: &OutputEndpoint) -> ReadResult<OutputHandle> {
        let resolved = self.resolve_at(&self.registry_snapshot(), endpoint)?;
        self.handle(endpoint.clone(), resolved)
    }

    pub(crate) async fn read_when_ready<T, F: Future<Output = ReadResult<T>>>(
        &self,
        endpoint: &OutputEndpoint,
        read: impl Fn(OutputHandle) -> F,
    ) -> ReadResult<T> {
        let mut publications = self.registry.clone();
        let generation = self
            .registry_snapshot()
            .observed
            .components
            .get(&endpoint.component)
            .map(|component| component.generation);
        loop {
            publications.borrow_and_update();
            let publication = self.registry_snapshot();
            let result = match self
                .resolve_at(&publication, endpoint)
                .and_then(|resolved| self.handle(endpoint.clone(), resolved))
            {
                Ok(handle) => read(handle).await,
                Err(error) => Err(error),
            };
            match result {
                Err(OutputReadError::Retired)
                    if !matches!(
                        *self.state.borrow(),
                        GraphState::Stopping
                            | GraphState::Completed
                            | GraphState::Cancelled
                            | GraphState::Failed
                            | GraphState::CleanupRequired
                    ) && self
                        .registry_snapshot()
                        .observed
                        .components
                        .get(&endpoint.component)
                        .is_some_and(|component| {
                            Some(component.generation) == generation
                                && !matches!(
                                    component.lifecycle,
                                    ComponentLifecycle::Failed | ComponentLifecycle::Stopping
                                )
                        }) =>
                {
                    // Activation changes the operation epoch before publishing readiness.
                    // No snapshot/replay has escaped this wait, and replacement stays fenced.
                    publications
                        .changed()
                        .await
                        .map_err(|_| OutputReadError::Retired)?;
                    continue;
                }
                Err(error @ OutputReadError::NotReady(_)) => {
                    if self
                        .registry_snapshot()
                        .observed
                        .components
                        .get(&endpoint.component)
                        .is_some_and(|component| component.lifecycle == ComponentLifecycle::Failed)
                    {
                        return Err(error);
                    }
                    publications
                        .changed()
                        .await
                        .map_err(|_| OutputReadError::Retired)?;
                }
                result => return result,
            }
        }
    }

    fn handle(&self, endpoint: OutputEndpoint, resolved: Resolved) -> ReadResult<OutputHandle> {
        if resolved.readers.is_none()
            && resolved.description.snapshot == Provision::NotConstructed
            && resolved.description.replay == Provision::NotConstructed
            && resolved.description.snapshot_follow == Provision::NotConstructed
        {
            return Err(OutputReadError::NotReady(format!(
                "component {} is not constructed",
                endpoint.component
            )));
        }
        Ok(OutputHandle {
            catalogue: self.clone(),
            endpoint,
            schema: resolved.description.schema,
            stream: resolved.stream,
            generation: resolved.generation,
            run_epoch: resolved.run_epoch,
            read_epoch: resolved.read_epoch,
            readers: resolved.readers,
            independent: resolved.independent,
            projection: resolved.projection,
            current_state: resolved.description.current_state,
        })
    }

    fn resolve_at(
        &self,
        publication: &GraphRegistrySnapshot,
        endpoint: &OutputEndpoint,
    ) -> ReadResult<Resolved> {
        let mut output = Self::resolve_direct(publication, endpoint)?;
        if output
            .readers
            .as_ref()
            .and_then(WeakOutputPortReaders::projection)
            .is_some()
        {
            (output.description.current_state, output.projection) =
                ProjectionRoute::resolve(self, publication, output.clone())?;
        }
        Ok(output)
    }

    fn resolve_direct(
        publication: &GraphRegistrySnapshot,
        endpoint: &OutputEndpoint,
    ) -> ReadResult<Resolved> {
        let unknown = || OutputReadError::UnknownOutput(endpoint.clone());
        let node = publication
            .desired
            .nodes
            .iter()
            .find(|node| node.descriptor.id() == &endpoint.component)
            .ok_or_else(unknown)?;
        let port = node
            .descriptor
            .ports()
            .iter()
            .find(|port| {
                port.id() == &endpoint.port
                    && port.direction() == PortDirection::Output
                    && port.kind() == PortKind::Data
            })
            .ok_or_else(unknown)?;
        let observed = publication
            .observed
            .components
            .get(&endpoint.component)
            .ok_or_else(unknown)?;
        let readers = publication
            .output_readers(&endpoint.component)
            .and_then(|readers| readers.port(&endpoint.port))
            .cloned();
        // A preconstructed component publishes its providers when declared, so
        // registered providers are authoritative. Only an unregistered output
        // of a component that is not yet realized is unknown rather than absent.
        let provision = |provided: bool| {
            if provided {
                Provision::Provided
            } else if observed.realization == RealizationState::Created {
                Provision::Absent
            } else {
                Provision::NotConstructed
            }
        };
        let snapshot = provision(
            readers
                .as_ref()
                .is_some_and(WeakOutputPortReaders::snapshot_is_provided),
        );
        let replay = provision(
            readers
                .as_ref()
                .is_some_and(WeakOutputPortReaders::replay_is_provided),
        );
        let snapshot_follow = provision(
            readers
                .as_ref()
                .is_some_and(WeakOutputPortReaders::snapshot_follow_is_provided),
        );
        let (current_state, independent) = match readers
            .as_ref()
            .and_then(WeakOutputPortReaders::provider_input)
        {
            Some(input) if snapshot != Provision::Provided => {
                ProviderAttachment::resolve(publication, endpoint, input)?
            }
            _ => (snapshot, None),
        };
        Ok(Resolved {
            description: OutputDescription {
                endpoint: endpoint.clone(),
                schema: port.schema().clone(),
                current_state,
                snapshot,
                replay,
                snapshot_follow,
            },
            stream: node.output_streams.get(&endpoint.port).cloned(),
            generation: observed.generation,
            run_epoch: publication.observed.run_epoch,
            read_epoch: observed.read_epoch,
            readers,
            independent,
            projection: None,
        })
    }
}

/// A snapshot and the replay from its own cursor. Epoch change and retention
/// loss are detected; live changes after the replay head are not captured.
#[derive(Debug, Clone)]
pub struct OutputCatchUp {
    pub snapshot: OutputSnapshot,
    pub replay: OutputReplay,
}

/// Providers resolved for one output generation. Holds no strong provider
/// reference; each read upgrades the published weak handle and is fenced
/// against the current graph publication.
#[derive(Clone)]
pub struct OutputHandle {
    catalogue: OutputCatalogue,
    endpoint: OutputEndpoint,
    schema: SchemaDescriptor,
    stream: Option<StreamId>,
    generation: ComponentGeneration,
    run_epoch: u64,
    read_epoch: OperationEpoch,
    readers: Option<WeakOutputPortReaders>,
    current_state: Provision,
    independent: Option<ProviderAttachment>,
    projection: Option<Arc<ProjectionRoute>>,
}

impl OutputHandle {
    pub fn endpoint(&self) -> &OutputEndpoint {
        &self.endpoint
    }

    pub fn provides_snapshot(&self) -> bool {
        self.readers
            .as_ref()
            .is_some_and(WeakOutputPortReaders::snapshot_is_provided)
    }

    pub fn provides_replay(&self) -> bool {
        self.readers
            .as_ref()
            .is_some_and(WeakOutputPortReaders::replay_is_provided)
    }

    pub fn provides_snapshot_follow(&self) -> bool {
        self.readers
            .as_ref()
            .is_some_and(WeakOutputPortReaders::snapshot_follow_is_provided)
    }

    fn absent(&self, capability: OutputCapability) -> OutputReadError {
        OutputReadError::Absent {
            endpoint: self.endpoint.clone(),
            capability,
        }
    }

    fn ensure_current(&self) -> ReadResult<()> {
        if self.catalogue.registry.has_changed().is_err()
            || self.catalogue.state.has_changed().is_err()
        {
            return Err(OutputReadError::Retired);
        }
        let current = self.catalogue.registry_snapshot();
        self.ensure_local_current(&current)?;
        if let Some(route) = &self.projection {
            route.ensure_current(&current)?;
        }
        Ok(())
    }

    fn ensure_local_current(&self, current: &GraphRegistrySnapshot) -> ReadResult<()> {
        let observed = current
            .observed
            .components
            .get(&self.endpoint.component)
            .ok_or(OutputReadError::Retired)?;
        let published = current
            .output_readers(&self.endpoint.component)
            .and_then(|readers| readers.port(&self.endpoint.port));
        let same_providers = match (&self.readers, published) {
            (None, None) => true,
            (Some(captured), Some(published)) => captured.same_providers(published),
            _ => false,
        };
        let live = observed.lifecycle != ComponentLifecycle::Stopping
            && !matches!(
                *self.catalogue.state.borrow(),
                GraphState::Stopping
                    | GraphState::Completed
                    | GraphState::Cancelled
                    | GraphState::Failed
                    | GraphState::CleanupRequired
            );
        if current.observed.run_epoch == self.run_epoch
            && observed.generation == self.generation
            && observed.read_epoch == self.read_epoch
            && same_providers
            && self
                .independent
                .as_ref()
                .is_none_or(|provider| provider.is_current(current))
            && live
        {
            Ok(())
        } else {
            Err(OutputReadError::Retired)
        }
    }

    async fn fenced<T>(&self, read: impl Future<Output = ReadResult<T>>) -> ReadResult<T> {
        let mut publications = self.catalogue.registry.clone();
        let mut state = self.catalogue.state.clone();
        tokio::pin!(read);
        loop {
            self.ensure_current()?;
            tokio::select! {
                result = &mut read => {
                    self.ensure_current()?;
                    return result;
                }
                changed = publications.changed() => {
                    changed.map_err(|_| OutputReadError::Retired)?;
                }
                changed = state.changed() => {
                    changed.map_err(|_| OutputReadError::Retired)?;
                }
            }
        }
    }

    fn check_cursor(&self, cursor: &OutputCursor) -> ReadResult<()> {
        let producer = cursor.producer();
        if producer.component_id() != &self.endpoint.component
            || self
                .stream
                .as_ref()
                .is_some_and(|stream| producer.stream() != stream)
        {
            return Err(OutputReadError::invalid(format!(
                "provider for {} reported another component's or stream's position",
                self.endpoint
            )));
        }
        Ok(())
    }

    pub async fn snapshot(&self) -> ReadResult<OutputSnapshot> {
        // A stopped or replaced generation is retired even if it never had
        // this capability; absence is only reported for the current one.
        self.ensure_current()?;
        let provider = self
            .readers
            .as_ref()
            .map(WeakOutputPortReaders::upgrade_snapshot)
            .transpose()?
            .flatten()
            .ok_or_else(|| self.absent(OutputCapability::Snapshot))?;
        let snapshot = self.fenced(async { provider.snapshot().await }).await?;
        self.check_snapshot(&snapshot)?;
        Ok(snapshot)
    }

    fn check_snapshot(&self, snapshot: &OutputSnapshot) -> ReadResult<()> {
        if snapshot.schema() != &self.schema {
            return Err(OutputReadError::invalid(format!(
                "snapshot for {} does not use the port's declared schema",
                self.endpoint
            )));
        }
        self.check_cursor(snapshot.cursor())
    }

    /// Read this output once. An empty view completes without any data
    /// envelopes; a missing snapshot provider is `Absent`, not an empty view.
    pub async fn current_state(
        &self,
        options: CurrentStateOptions,
    ) -> ReadResult<CurrentStateRead> {
        self.ensure_current()?;
        let view = match &self.projection {
            Some(route) => self.fenced(route.capture(options.batches)).await?,
            None => self.capture_direct(options.batches).await?,
        };
        self.deliver_state(view, options, None, None).await
    }

    async fn capture_direct(&self, limits: SnapshotBatchLimits) -> ReadResult<CapturedView> {
        if let Some(binding) = &self.independent {
            let reader = binding.reader.upgrade().ok_or(OutputReadError::Retired)?;
            let scan = self
                .fenced(reader.open_state(
                    &self.endpoint,
                    &binding.edge.to.port,
                    &binding.edge.from.port,
                ))
                .await?;
            CapturedView::new(self.endpoint.to_string(), None, |stream| {
                state_transfer::scan_batches(scan, stream, self.schema.clone(), limits)
            })
        } else if self.current_state == Provision::NotConstructed {
            Err(OutputReadError::NotReady(format!(
                "waiting for the state provider of {}",
                self.endpoint
            )))
        } else {
            CapturedView::materialized(self.snapshot().await?, limits)
        }
    }

    /// Capture the baseline and protect its suffix in the provider's *one*
    /// snapshot-and-follow operation, before scanning or spooling any records.
    /// The caller can take the lease only after the baseline has reached EOF.
    pub async fn current_state_and_follow(
        &self,
        options: CurrentStateOptions,
        limits: OutputFollowLimits,
    ) -> ReadResult<CurrentStateRead> {
        let start = Instant::now();
        let (snapshot, follow) = self.snapshot_and_follow(limits).await?.into_parts();
        // snapshot_and_follow validated this lifetime and started the provider lease.
        self.deliver_state(
            CapturedView::materialized(snapshot, options.batches)?,
            options,
            Some(follow),
            Some(start + limits.lifetime),
        )
        .await
    }

    fn file_schema(&self) -> ReadResult<Arc<Schema>> {
        let publication = self.catalogue.registry_snapshot();
        let factory = publication
            .desired
            .specifications
            .get(&self.endpoint.component)
            .and_then(|spec| publication.factory(&spec.implementation));
        transfer_schema(&self.schema, factory.as_deref()).ok_or_else(|| {
            OutputReadError::invalid(format!(
                "no executable schema registered for temporary-file transfer of {}",
                self.endpoint
            ))
        })
    }

    async fn deliver_state(
        &self,
        view: CapturedView,
        options: CurrentStateOptions,
        follow: Option<Box<dyn OutputFollow>>,
        deadline: Option<Instant>,
    ) -> ReadResult<CurrentStateRead> {
        let CapturedView {
            session,
            dataset,
            cursor,
            batches,
        } = view;
        let baseline = self
            .fenced(with_lease(deadline, async {
                Ok(match options.transfer {
                    StateTransfer::Stream => batches,
                    StateTransfer::TemporaryFile { max_file_bytes } => {
                        let scope = state_transfer::file_scope(
                            session,
                            &self.endpoint,
                            self.generation,
                            self.read_epoch,
                            &dataset,
                            cursor.as_ref(),
                            &self.schema,
                        )?;
                        state_transfer::FileTransfer::seal_stream(
                            batches,
                            scope,
                            self.file_schema()?,
                            options.batches,
                            max_file_bytes,
                        )
                        .await?
                        .import(scope)
                    }
                })
            }))
            .await?;
        Ok(CurrentStateRead {
            handle: self.clone(),
            cursor,
            dataset,
            baseline: Baseline::Reading(baseline),
            follow,
            deadline,
        })
    }

    /// Capture current state and a bounded live tail with no window between
    /// snapshot capture and retention. The lease starts at this request, is
    /// non-renewing, and is released by dropping the returned follow handle.
    /// Install snapshot state and its cursor together before consuming the tail.
    pub async fn snapshot_and_follow(
        &self,
        limits: OutputFollowLimits,
    ) -> ReadResult<OutputSnapshotFollow> {
        self.ensure_current()?;
        limits.validate()?;
        let deadline = tokio::time::Instant::now() + limits.lifetime;
        let provider = self
            .readers
            .as_ref()
            .map(WeakOutputPortReaders::upgrade_snapshot_follow)
            .transpose()?
            .flatten()
            .ok_or_else(|| self.absent(OutputCapability::SnapshotFollow))?;
        let opened = self
            .fenced(async {
                tokio::time::timeout_at(deadline, provider.snapshot_and_follow(limits))
                    .await
                    .map_err(|_| OutputReadError::LeaseExpired)?
            })
            .await?;
        let (snapshot, follow) = opened.into_parts();
        self.check_snapshot(&snapshot)?;
        let cursor = snapshot.cursor().clone();
        Ok(OutputSnapshotFollow::new(
            snapshot,
            Box::new(FencedOutputFollow {
                handle: self.clone(),
                cursor,
                follow: Some(follow),
                deadline,
            }),
        ))
    }
}

async fn with_lease<T>(
    deadline: Option<Instant>,
    read: impl Future<Output = ReadResult<T>>,
) -> ReadResult<T> {
    let Some(deadline) = deadline else {
        return read.await;
    };
    if Instant::now() >= deadline {
        return Err(OutputReadError::LeaseExpired);
    }
    let result = tokio::time::timeout_at(deadline, read)
        .await
        .map_err(|_| OutputReadError::LeaseExpired)?;
    if Instant::now() >= deadline {
        return Err(OutputReadError::LeaseExpired);
    }
    result
}

enum Baseline {
    Reading(BoxStream<'static, ReadResult<ChangeEnvelope>>),
    Complete,
    Failed,
}

struct CapturedView {
    session: BootstrapSessionId,
    dataset: String,
    cursor: Option<OutputCursor>,
    batches: StateScan,
}

impl CapturedView {
    fn new(
        dataset: String,
        cursor: Option<OutputCursor>,
        batches: impl FnOnce(StreamId) -> StateScan,
    ) -> ReadResult<Self> {
        let session = BootstrapSessionId::new();
        let stream = StreamId::try_new(format!("snapshot/{session}"))
            .map_err(|e| OutputReadError::Failed(e.into()))?;
        Ok(Self {
            session,
            dataset,
            cursor,
            batches: batches(stream),
        })
    }

    fn materialized(snapshot: OutputSnapshot, limits: SnapshotBatchLimits) -> ReadResult<Self> {
        Self::new(
            snapshot.dataset().to_owned(),
            Some(snapshot.cursor().clone()),
            |stream| stream::iter(SnapshotBatches::new(&snapshot, stream, limits)).boxed(),
        )
    }
}

/// A generation-fenced delivery session. Drop it to cancel the baseline and
/// release its file and follow lease. No worker enumerates rows in the background.
pub struct CurrentStateRead {
    handle: OutputHandle,
    cursor: Option<OutputCursor>,
    dataset: String,
    baseline: Baseline,
    follow: Option<Box<dyn OutputFollow>>,
    deadline: Option<Instant>,
}

impl CurrentStateRead {
    /// The actual logical output cut, when one exists. A read-only datastore
    /// snapshot does not invent a replay cursor for the source's live stream.
    pub fn cursor(&self) -> Option<&OutputCursor> {
        self.cursor.as_ref()
    }

    /// The dataset scope, including for empty views.
    pub fn dataset(&self) -> &str {
        &self.dataset
    }

    /// The output port's complete declared schema.
    pub fn schema(&self) -> &SchemaDescriptor {
        &self.handle.schema
    }

    /// `Ok(None)` completes the baseline. Treat batches as provisional until
    /// then: an error is terminal, never successful EOF. Cancelling this
    /// future without dropping the session does not consume a batch.
    pub async fn next_batch(&mut self) -> ReadResult<Option<ChangeEnvelope>> {
        let stream = match &mut self.baseline {
            Baseline::Complete => return Ok(None),
            Baseline::Failed => {
                return Err(OutputReadError::invalid(
                    "current-state read already failed",
                ))
            }
            Baseline::Reading(stream) => stream,
        };
        let result = self
            .handle
            .fenced(with_lease(self.deadline, async {
                tokio::task::yield_now().await;
                stream.next().await.transpose()
            }))
            .await;
        match &result {
            Ok(None) => self.baseline = Baseline::Complete,
            Err(_) => {
                self.baseline = Baseline::Failed;
                self.follow = None;
            }
            Ok(Some(_)) => (),
        }
        result
    }

    /// After reading to EOF and installing the baseline and cut together,
    /// take the already-armed live lease. Never opens a new subscription.
    /// The lease still reports retention loss or retirement through `next()`.
    pub fn into_follow(self) -> ReadResult<Box<dyn OutputFollow>> {
        self.handle.ensure_current()?;
        if !matches!(self.baseline, Baseline::Complete) {
            return Err(OutputReadError::invalid("baseline did not complete"));
        }
        if self.deadline.is_some_and(|end| Instant::now() >= end) {
            return Err(OutputReadError::LeaseExpired);
        }
        self.follow
            .ok_or_else(|| OutputReadError::invalid("one-shot read has no follow lease"))
    }
}

struct FencedOutputFollow {
    handle: OutputHandle,
    cursor: OutputCursor,
    follow: Option<Box<dyn OutputFollow>>,
    deadline: tokio::time::Instant,
}

#[async_trait::async_trait]
impl OutputFollow for FencedOutputFollow {
    async fn next(&mut self) -> ReadResult<ChangeEnvelope> {
        let follow = self.follow.as_mut().ok_or(OutputReadError::Retired)?;
        let result = self
            .handle
            .fenced(async {
                tokio::time::timeout_at(self.deadline, follow.next())
                    .await
                    .map_err(|_| OutputReadError::LeaseExpired)?
            })
            .await
            .and_then(|envelope| {
                let progress = GraphProducerProgress::from_envelope(&envelope)
                    .map_err(OutputReadError::Failed)?
                    .ok_or_else(|| {
                        OutputReadError::invalid("follow output lacks producer progress")
                    })?;
                let current =
                    OutputCursor::try_new(progress.identity().clone(), progress.sequence())?;
                self.cursor.ensure_same_epoch(&current)?;
                if current.sequence() <= self.cursor.sequence()
                    || envelope.changes().schema() != &self.handle.schema
                    || envelope.system().stream() != current.producer().stream()
                {
                    return Err(OutputReadError::invalid(
                        "follow output has an invalid schema, stream or revision",
                    ));
                }
                self.cursor = current;
                Ok(envelope)
            });
        if result.is_err() {
            self.follow = None;
        }
        result
    }
}

impl OutputHandle {
    fn replay_provider(&self) -> ReadResult<Arc<dyn ReplayProvider>> {
        self.ensure_current()?;
        self.readers
            .as_ref()
            .map(WeakOutputPortReaders::upgrade_replay)
            .transpose()?
            .flatten()
            .ok_or_else(|| self.absent(OutputCapability::Replay))
    }

    pub async fn committed_head(&self) -> ReadResult<OutputCursor> {
        let provider = self.replay_provider()?;
        let head = self
            .fenced(async { provider.committed_head().await })
            .await?;
        self.check_cursor(&head)?;
        Ok(head)
    }

    pub async fn replay_after(&self, after: &OutputCursor) -> ReadResult<OutputReplay> {
        let provider = self.replay_provider()?;
        let replay = self
            .fenced(async { provider.replay_after(after).await })
            .await?;
        if replay.after() != after {
            return Err(OutputReadError::invalid(
                "replay answered a different cursor than requested",
            ));
        }
        self.check_cursor(replay.head())?;
        let stream = replay.head().producer().stream();
        for envelope in replay.envelopes() {
            if envelope.changes().schema() != &self.schema || envelope.system().stream() != stream {
                return Err(OutputReadError::invalid(format!(
                    "replay for {} contains output outside the port's declared schema or stream",
                    self.endpoint
                )));
            }
        }
        Ok(replay)
    }

    /// Requires both providers. Either failing is returned as is; there is no
    /// fallback to an empty or partial result.
    pub async fn catch_up(&self) -> ReadResult<OutputCatchUp> {
        let snapshot = self.snapshot().await?;
        let replay = self.replay_after(snapshot.cursor()).await?;
        Ok(OutputCatchUp { snapshot, replay })
    }
}
