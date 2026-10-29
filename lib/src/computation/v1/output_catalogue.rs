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
//! the component generation and operation they resolved; every read re-checks
//! them against the current publication, so a stopped, restarted or replaced
//! component yields [`OutputReadError::Retired`] rather than a replacement's
//! data or an empty default.
//!
//! Reading a snapshot and then replaying from its cursor detects epoch changes
//! and retention gaps ([`OutputHandle::catch_up`]). That is not a gap-free live
//! handover. [`OutputHandle::snapshot_and_follow`] requires an actual registered
//! provider that captures the snapshot and its bounded live lease together.

use super::*;
use std::{future::Future, sync::Arc};

type ReadResult<T> = std::result::Result<T, OutputReadError>;

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

struct Resolved {
    description: OutputDescription,
    stream: Option<StreamId>,
    generation: ComponentGeneration,
    operation: OperationEpoch,
    readers: Option<WeakOutputPortReaders>,
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
        let resolved = Self::resolve_at(&publication, &endpoint)?;
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
            for port in node
                .descriptor
                .ports()
                .iter()
                .filter(|port| port.direction() == PortDirection::Output)
            {
                let endpoint = OutputEndpoint::new(node.descriptor.id().clone(), port.id().clone());
                descriptions.push(Self::resolve_at(&publication, &endpoint)?.description);
            }
        }
        Ok(descriptions)
    }

    pub fn describe(&self, endpoint: &OutputEndpoint) -> ReadResult<OutputDescription> {
        Self::resolve_at(&self.registry_snapshot(), endpoint).map(|resolved| resolved.description)
    }

    /// Capture the endpoint's current providers. Fails with
    /// [`OutputReadError::NotReady`] while its component is not constructed.
    pub fn resolve(&self, endpoint: &OutputEndpoint) -> ReadResult<OutputHandle> {
        let resolved = Self::resolve_at(&self.registry_snapshot(), endpoint)?;
        self.handle(endpoint.clone(), resolved)
    }

    fn handle(&self, endpoint: OutputEndpoint, resolved: Resolved) -> ReadResult<OutputHandle> {
        if resolved.description.snapshot == Provision::NotConstructed
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
            operation: resolved.operation,
            readers: resolved.readers,
        })
    }

    fn resolve_at(
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
            .find(|port| port.id() == &endpoint.port && port.direction() == PortDirection::Output)
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
        Ok(Resolved {
            description: OutputDescription {
                endpoint: endpoint.clone(),
                schema: port.schema().clone(),
                snapshot,
                replay,
                snapshot_follow,
            },
            stream: node.output_streams.get(&endpoint.port).cloned(),
            generation: observed.generation,
            operation: observed.operation,
            readers,
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
    operation: OperationEpoch,
    readers: Option<WeakOutputPortReaders>,
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
        if observed.generation == self.generation
            && observed.operation == self.operation
            && same_providers
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
