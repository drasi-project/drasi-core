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

use super::*;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio::sync::watch;

#[derive(Clone)]
pub(super) struct LocalQueryOutput {
    pub config_hash: u64,
    pub results: QueryResults,
    pub reader: Arc<super::query_output::QueryOutputProvider>,
    pub progress: Option<Arc<QuerySourceProgress>>,
    pub incarnation: Arc<std::sync::RwLock<Option<uuid::Uuid>>>,
    pub _checkpoint:
        Arc<std::sync::RwLock<Option<Arc<dyn drasi_core::interface::CheckpointStore>>>>,
    pub output_persistence: Arc<std::sync::RwLock<Option<bool>>>,
}

#[derive(Clone)]
pub(super) struct CatalogQuery {
    pub generation: u64,
    pub output: Arc<super::query_api::QueryApiOutput>,
    pub telemetry: Arc<crate::observability::QueryProgressObservations>,
}

#[cfg(test)]
#[derive(Clone)]
pub(crate) struct QueryOutputGenerationReader {
    query: ComponentId,
    results: QueryResults,
}

#[cfg(test)]
impl QueryOutputGenerationReader {
    pub(crate) fn generation(&self) -> u64 {
        self.results.observed_generation(self.query.as_str())
    }
}

struct CatalogState {
    entries: BTreeMap<ComponentId, CatalogQuery>,
    next_generation: u64,
    delivery: BTreeMap<ComponentId, DeliveryConfiguration>,
    subscribers: BTreeMap<(ComponentId, u64), Subscriber>,
    next_subscriber: u64,
    transport: BTreeMap<StreamId, Arc<std::sync::atomic::AtomicU64>>,
}
struct DeliveryConfiguration {
    mode: crate::DispatchMode,
    capacity: usize,
    metrics: Arc<crate::metrics::QueryOutputMetrics>,
}
#[derive(Clone)]
enum Subscriber {
    Channel {
        sender: tokio::sync::mpsc::Sender<super::EnvelopeMessage>,
        lag: Arc<ChannelLag>,
    },
    Broadcast(tokio::sync::broadcast::Sender<super::EnvelopeMessage>),
}
enum SubscriberReceiver {
    Channel {
        receiver: tokio::sync::mpsc::Receiver<super::EnvelopeMessage>,
        lag: Arc<ChannelLag>,
    },
    Broadcast(tokio::sync::broadcast::Receiver<super::EnvelopeMessage>),
}

/// Publications a bounded (Channel) subscriber missed while its query's
/// observers were released for a stop. Once a subscriber lags, it receives
/// nothing more until it has read its buffer and been told the gap, so the
/// reported gap is always in order.
#[derive(Default)]
struct ChannelLag {
    skipped: std::sync::atomic::AtomicU64,
    reported: tokio::sync::Notify,
}

impl ChannelLag {
    /// Records the publication as skipped when the subscriber already lags.
    fn skip_while_lagging(&self) -> bool {
        self.skipped
            .fetch_update(
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
                |skipped| (skipped > 0).then(|| skipped.saturating_add(1)),
            )
            .is_ok()
    }

    fn skip(&self) {
        self.skipped
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        self.reported.notify_one();
    }
}

/// Whether a query's live result observers are released for a stop. Released
/// observers never hold a publication: a subscriber without capacity lags.
pub(super) struct ObserverGate(watch::Sender<bool>);

impl ObserverGate {
    fn new() -> Self {
        Self(watch::channel(false).0)
    }

    async fn released(&self) {
        let mut released = self.0.subscribe();
        if released.wait_for(|released| *released).await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

impl ObserverDelivery for ObserverGate {
    fn release_observers(&self) {
        self.0.send_replace(true);
    }
    fn reclaim_observers(&self) {
        self.0.send_replace(false);
    }
}

enum Delivered {
    Sent,
    Lagged,
    Closed,
}
pub(crate) struct CatalogSubscription {
    receiver: SubscriberReceiver,
    catalog: Weak<CatalogInner>,
    key: (ComponentId, u64),
    head: Option<QuerySubscriptionHead>,
}

#[derive(Clone)]
pub(crate) struct QuerySubscriptionHead {
    pub(crate) sequence: u64,
    pub(crate) generation: u64,
    pub(super) config_hash: u64,
    pub(super) incarnation: Option<uuid::Uuid>,
    pub(super) output: Arc<super::query_api::QueryApiOutput>,
}

impl QuerySubscriptionHead {
    pub(super) fn belongs_to(&self, query: &CatalogQuery) -> bool {
        // Linked parent/child catalogs assign different registration generations.
        self.output.same_owner(&query.output)
    }
}
impl CatalogSubscription {
    pub(crate) fn head(&self) -> anyhow::Result<QuerySubscriptionHead> {
        self.head
            .clone()
            .ok_or_else(|| anyhow::anyhow!("subscription has no captured query head"))
    }

    /// Receives the next publication. A bounded subscriber that missed
    /// publications while its query's observers were released for a stop
    /// receives its buffered publications first and then the same typed
    /// `RecvError::Lagged` gap that a broadcast subscriber reports.
    #[cfg(test)]
    pub(crate) async fn receive(&mut self) -> anyhow::Result<ChangeEnvelope> {
        Ok(self.receive_message().await?.envelope)
    }

    pub(crate) async fn receive_message(&mut self) -> anyhow::Result<super::EnvelopeMessage> {
        match &mut self.receiver {
            SubscriberReceiver::Channel { receiver, lag } => loop {
                use tokio::sync::mpsc::error::TryRecvError;
                let disconnected = match receiver.try_recv() {
                    Ok(envelope) => return Ok(envelope),
                    Err(TryRecvError::Empty) => false,
                    Err(TryRecvError::Disconnected) => true,
                };
                let skipped = lag.skipped.swap(0, std::sync::atomic::Ordering::AcqRel);
                if skipped > 0 {
                    return Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped).into());
                }
                if disconnected {
                    anyhow::bail!("query output subscription closed");
                }
                tokio::select! {
                    biased;
                    envelope = receiver.recv() => {
                        if let Some(envelope) = envelope {
                            return Ok(envelope);
                        }
                    }
                    () = lag.reported.notified() => {}
                }
            },
            SubscriberReceiver::Broadcast(receiver) => receiver.recv().await.map_err(Into::into),
        }
    }

    #[cfg(test)]
    pub(crate) async fn receive_after_head(&mut self) -> anyhow::Result<ChangeEnvelope> {
        Ok(self.receive_message_after_head().await?.envelope)
    }

    pub(crate) async fn receive_message_after_head(
        &mut self,
    ) -> anyhow::Result<super::EnvelopeMessage> {
        loop {
            let message = self.receive_message().await?;
            if let Some(head) = &self.head {
                if QueryChangeCodec::query_generation(&message.envelope)? == head.generation
                    && QueryRecoveryIdentity::from_query_output(&message.envelope)?.incarnation()
                        == head.incarnation
                    && QueryChangeCodec::query_sequence(&message.envelope)? <= head.sequence
                {
                    continue;
                }
            }
            return Ok(message);
        }
    }
}
impl Drop for CatalogSubscription {
    fn drop(&mut self) {
        if let Some(catalog) = self.catalog.upgrade() {
            catalog
                .state
                .lock()
                .unwrap_or_else(|error| {
                    log::error!("Removing subscription from poisoned query catalogue: {error}");
                    error.into_inner()
                })
                .subscribers
                .remove(&self.key);
        }
    }
}
struct CatalogInner {
    graph: Arc<str>,
    state: Mutex<CatalogState>,
    changed: watch::Sender<u64>,
    events: tokio::sync::broadcast::Sender<ChangeEnvelope>,
}

/// A graph-owned catalogue of actual native query readers. It never delegates
/// execution or subscriptions to the legacy QueryManager.
#[derive(Clone)]
pub struct QueryResultsCatalog(Arc<CatalogInner>);

pub(super) struct QueryRegistration {
    catalog: Weak<CatalogInner>,
    id: ComponentId,
    generation: u64,
}
pub(crate) struct CatalogLink {
    _registration: QueryRegistration,
}
impl Drop for QueryRegistration {
    fn drop(&mut self) {
        if let Some(catalog) = self.catalog.upgrade() {
            let mut state = catalog.state.lock().unwrap_or_else(|error| {
                log::error!("Retiring query reader in a poisoned catalogue: {error}");
                error.into_inner()
            });
            if state
                .entries
                .get(&self.id)
                .is_some_and(|entry| entry.generation == self.generation)
            {
                if let Some(entry) = state.entries.remove(&self.id) {
                    entry.telemetry.publish(None);
                }
                catalog.changed.send_modify(|version| *version += 1);
            }
        }
    }
}

impl QueryResultsCatalog {
    pub fn new(graph: impl Into<Arc<str>>) -> Result<Self> {
        let graph = graph.into();
        super::data::validate_identifier("graph", &graph)?;
        Ok(Self(Arc::new(CatalogInner {
            graph,
            state: Mutex::new(CatalogState {
                entries: BTreeMap::new(),
                next_generation: 1,
                delivery: BTreeMap::new(),
                subscribers: BTreeMap::new(),
                next_subscriber: 1,
                transport: BTreeMap::new(),
            }),
            changed: watch::channel(0).0,
            events: tokio::sync::broadcast::channel(256).0,
        })))
    }
    pub fn graph_id(&self) -> &str {
        &self.0.graph
    }
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<ChangeEnvelope> {
        self.0.events.subscribe()
    }
    #[cfg(test)]
    pub(super) async fn publish(&self, envelope: ChangeEnvelope) -> anyhow::Result<()> {
        self.publish_observed(envelope, &ObserverGate::new()).await
    }

    /// Publishes to every current subscriber. A bounded subscriber without
    /// capacity backpressures publication until `observers` is released for a
    /// stop; it then lags instead (see [`CatalogSubscription::receive`]).
    #[cfg(test)]
    pub(super) async fn publish_observed(
        &self,
        envelope: ChangeEnvelope,
        observers: &ObserverGate,
    ) -> anyhow::Result<()> {
        self.publish_message(
            super::EnvelopeMessage::new(
                envelope,
                crate::observability::ExecutionContext::current(),
            ),
            observers,
        )
        .await
    }

    async fn publish_message(
        &self,
        message: super::EnvelopeMessage,
        observers: &ObserverGate,
    ) -> anyhow::Result<()> {
        let envelope = &message.envelope;
        let id = QueryChangeCodec::metadata(envelope)?.query_id;
        let query = self.get(&id)?.ok_or_else(|| {
            anyhow::anyhow!("query publication is not registered in this catalogue")
        })?;
        let identity = QueryRecoveryIdentity::from_query_output(envelope)?;
        query
            .telemetry
            .publish(Some(crate::observability::QueryHead {
                sequence: QueryChangeCodec::query_sequence(envelope)?,
                epoch: crate::observability::QueryEpoch {
                    generation: QueryChangeCodec::query_generation(envelope)?,
                    config_hash: identity.configuration_hash(),
                    incarnation: identity.incarnation(),
                },
            }));
        let subscribers: Vec<_> = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?
            .subscribers
            .iter()
            .filter(|((query, _), _)| query == &id)
            .map(|(key, subscriber)| (key.clone(), subscriber.clone()))
            .collect();
        let deliveries = subscribers.into_iter().map(|(key, subscriber)| {
            let message = message.clone();
            async move {
                let delivered = match subscriber {
                    Subscriber::Channel { sender, lag } => {
                        use tokio::sync::mpsc::error::TrySendError;
                        if lag.skip_while_lagging() {
                            Delivered::Lagged
                        } else {
                            // Reserving is cancellation-safe: a release drops
                            // the wait without consuming capacity.
                            let permit = tokio::select! {
                                biased;
                                permit = sender.reserve() => permit.map(Some).map_err(|_| ()),
                                () = observers.released() => match sender.try_reserve() {
                                    Ok(permit) => Ok(Some(permit)),
                                    Err(TrySendError::Full(())) => Ok(None),
                                    Err(TrySendError::Closed(())) => Err(()),
                                },
                            };
                            match permit {
                                Ok(Some(permit)) => {
                                    permit.send(message);
                                    Delivered::Sent
                                }
                                Ok(None) => {
                                    lag.skip();
                                    Delivered::Lagged
                                }
                                Err(()) => Delivered::Closed,
                            }
                        }
                    }
                    Subscriber::Broadcast(sender) => match sender.send(message) {
                        Ok(_) => Delivered::Sent,
                        Err(_) => Delivered::Closed,
                    },
                };
                (key, delivered)
            }
        });
        for (key, delivered) in futures::future::join_all(deliveries).await {
            if matches!(delivered, Delivered::Lagged) {
                log::debug!("A released query result subscriber without capacity lagged");
            }
            if matches!(delivered, Delivered::Closed) {
                self.0
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?
                    .subscribers
                    .remove(&key);
                log::trace!("Retired a closed native query output subscription");
            }
        }
        if self.0.events.send(message.envelope).is_err() {
            log::trace!("Native query publication has no live subscribers; retained results remain available");
        }
        Ok(())
    }
    pub(crate) fn same_registry(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(super) fn configure_delivery(
        &self,
        config: &crate::config::ResolvedQueryConfig,
        metrics: Arc<crate::metrics::QueryOutputMetrics>,
        default_capacity: usize,
    ) -> anyhow::Result<()> {
        let capacity = config.dispatch_buffer_capacity.unwrap_or(default_capacity);
        anyhow::ensure!(capacity > 0, "query delivery capacity must be nonzero");
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?;
        let id = config.id.clone();
        state.delivery.insert(
            id,
            DeliveryConfiguration {
                mode: config.dispatch_mode.unwrap_or_default(),
                capacity,
                metrics,
            },
        );
        Ok(())
    }
    pub(super) fn transport_sequence(
        &self,
        stream: &StreamId,
    ) -> anyhow::Result<Arc<std::sync::atomic::AtomicU64>> {
        Ok(self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?
            .transport
            .entry(stream.clone())
            .or_default()
            .clone())
    }
    pub(crate) async fn subscribe_query(
        &self,
        id: &ComponentId,
    ) -> anyhow::Result<CatalogSubscription> {
        self.subscribe_query_at_head(id)
            .await
            .map(|(subscription, _)| subscription)
    }
    pub(crate) async fn subscribe_query_at_head(
        &self,
        id: &ComponentId,
    ) -> anyhow::Result<(CatalogSubscription, u64)> {
        let id = id.clone();
        let (query, mut receiver) = {
            let mut state = self
                .0
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?;
            let query = state
                .entries
                .get(&id)
                .ok_or_else(|| anyhow::anyhow!("query is not registered"))?
                .clone();
            // Attach before awaiting a remote head, so intervening output cannot be lost.
            let receiver = self.subscribe_query_locked(&mut state, id.clone())?;
            (query, receiver)
        };
        let head = Self::read_head(&query).await?;
        anyhow::ensure!(
            self.get(&id)?
                .is_some_and(|current| current.generation == query.generation),
            "query changed while attaching its result subscription"
        );
        let sequence = head.sequence;
        receiver.head = Some(head);
        Ok((receiver, sequence))
    }

    pub(super) async fn query_head(
        &self,
        id: &ComponentId,
    ) -> anyhow::Result<QuerySubscriptionHead> {
        let query = self
            .get(&id.clone())?
            .ok_or_else(|| anyhow::anyhow!("query is not registered"))?;
        Self::read_head(&query).await
    }

    async fn read_head(query: &CatalogQuery) -> anyhow::Result<QuerySubscriptionHead> {
        let (sequence, generation, incarnation) = query.output.head().await?;
        query
            .telemetry
            .publish(Some(crate::observability::QueryHead {
                sequence,
                epoch: crate::observability::QueryEpoch {
                    generation,
                    config_hash: query.output.configuration_hash(),
                    incarnation,
                },
            }));
        Ok(QuerySubscriptionHead {
            sequence,
            generation,
            config_hash: query.output.configuration_hash(),
            incarnation,
            output: query.output.clone(),
        })
    }

    pub(crate) fn output_is_persistent(&self, id: &ComponentId) -> anyhow::Result<bool> {
        let query = self
            .get(&id.clone())?
            .ok_or_else(|| anyhow::anyhow!("query is not registered"))?;
        query.output.is_persistent()
    }

    /// Capture while the query is registered. The reader retains only output
    /// state, not index handles, and remains usable after permanent shutdown.
    #[cfg(test)]
    pub(crate) fn output_generation_reader(
        &self,
        id: &ComponentId,
    ) -> anyhow::Result<QueryOutputGenerationReader> {
        let id = id.clone();
        let query = self
            .get(&id)?
            .ok_or_else(|| anyhow::anyhow!("query is not registered"))?;
        let super::query_api::QueryApiOutput::Local(output) = query.output.as_ref() else {
            anyhow::bail!("native query generation reads require an active component");
        };
        Ok(QueryOutputGenerationReader {
            query: id,
            results: output.results.clone(),
        })
    }
    fn subscribe_query_locked(
        &self,
        state: &mut CatalogState,
        id: ComponentId,
    ) -> anyhow::Result<CatalogSubscription> {
        let configuration = state
            .delivery
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("query delivery is not configured"))?;
        let (subscriber, receiver) = match configuration.mode {
            crate::DispatchMode::Channel => {
                let (sender, receiver) = tokio::sync::mpsc::channel(configuration.capacity);
                let lag = Arc::new(ChannelLag::default());
                (
                    Subscriber::Channel {
                        sender,
                        lag: lag.clone(),
                    },
                    SubscriberReceiver::Channel { receiver, lag },
                )
            }
            crate::DispatchMode::Broadcast => {
                let (sender, receiver) = tokio::sync::broadcast::channel(configuration.capacity);
                (
                    Subscriber::Broadcast(sender),
                    SubscriberReceiver::Broadcast(receiver),
                )
            }
        };
        let sequence = state.next_subscriber;
        state.next_subscriber = sequence
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("subscriber generation exhausted"))?;
        let key = (id, sequence);
        state.subscribers.insert(key.clone(), subscriber);
        Ok(CatalogSubscription {
            receiver,
            catalog: Arc::downgrade(&self.0),
            key,
            head: None,
        })
    }
    pub(super) fn configured_metrics(
        &self,
        id: &ComponentId,
    ) -> anyhow::Result<Option<Arc<crate::metrics::QueryOutputMetrics>>> {
        Ok(self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?
            .delivery
            .get(id)
            .map(|config| config.metrics.clone()))
    }
    #[cfg(test)]
    pub(crate) fn checkpoint_store(
        &self,
        id: &ComponentId,
    ) -> anyhow::Result<Option<Arc<dyn drasi_core::interface::CheckpointStore>>> {
        let Some(entry) = self.get(&id.clone())? else {
            return Ok(None);
        };
        let super::query_api::QueryApiOutput::Local(output) = entry.output.as_ref() else {
            anyhow::bail!("native query checkpoints belong to its processing service");
        };
        let checkpoint = output
            ._checkpoint
            .read()
            .map_err(|_| anyhow::anyhow!("query checkpoint view poisoned"))?
            .clone();
        Ok(checkpoint)
    }
    pub(crate) async fn current_sequence(&self, id: &ComponentId) -> anyhow::Result<u64> {
        let query = self
            .get(&id.clone())?
            .ok_or_else(|| anyhow::anyhow!("query is not registered"))?;
        Ok(query.output.head().await?.0)
    }
}

pub struct QueryResultsOutlet {
    descriptor: ComponentDescriptor,
    catalog: QueryResultsCatalog,
    observers: Arc<ObserverGate>,
}

fn outlet_descriptor(id: ComponentId) -> GraphResult<ComponentDescriptor> {
    Ok(ComponentDescriptor::try_new(
        id,
        vec![PortDescriptor::new(
            PortId::try_new("in")?,
            PortDirection::Input,
            QueryChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )?)
}

impl QueryResultsOutlet {
    pub fn new(id: ComponentId, catalog: QueryResultsCatalog) -> Self {
        Self {
            descriptor: outlet_descriptor(id).expect("outlet"),
            catalog,
            observers: Arc::new(ObserverGate::new()),
        }
    }
}
#[async_trait::async_trait]
impl ComputationComponent for QueryResultsOutlet {
    fn description(&self) -> crate::computation::v1::ComponentDescription {
        crate::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    crate::passive_component_lifecycle!();
}
#[async_trait::async_trait]
impl EnvelopeSink for QueryResultsOutlet {
    fn observes_handled_branch_latency(&self) -> bool {
        false
    }
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn observer_delivery(&self) -> Option<Arc<dyn ObserverDelivery>> {
        // Live result subscribers are observers, not delivery obligations:
        // durable consumers recover gaps from the query's outbox.
        Some(self.observers.clone())
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.catalog
            .publish_message(
                super::EnvelopeMessage::new(input.envelope, input.execution_context),
                &self.observers,
            )
            .await
    }
}

pub struct QueryResultsOutletFactory {
    descriptor: FactoryDescriptor,
}
impl Default for QueryResultsOutletFactory {
    fn default() -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("drasi/query-results-outlet", "1")
                    .expect("implementation"),
                role: ComponentRole::Sink,
                configuration_version: 1,
                configuration: ConfigurationSchema::default(),
                dependencies: BTreeMap::from([(
                    Arc::from("catalog"),
                    ResourceRequirement::exactly_one::<QueryResultsCatalog>(
                        ResourceRole::QueryCatalog,
                    ),
                )]),
            },
        }
    }
}
#[async_trait::async_trait]
impl ComponentFactory for QueryResultsOutletFactory {
    fn describe(
        &self,
        _context: &crate::computation::v1::ConstructionContext,
    ) -> anyhow::Result<crate::computation::v1::ComponentDescription> {
        Ok(crate::computation::v1::ComponentDescription::undeclared())
    }
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn specification_from_config(
        &self,
        id: ComponentId,
        configuration: serde_json::Value,
    ) -> anyhow::Result<ComponentSpecification> {
        let configuration: BTreeMap<Arc<str>, serde_json::Value> =
            serde_json::from_value(configuration)?;
        Ok(ComponentSpecification {
            descriptor: outlet_descriptor(id)?,
            role: self.descriptor.role,
            completion: Some(SinkCompletion::Handled),
            implementation: self.descriptor.implementation.clone(),
            configuration_version: self.descriptor.configuration_version,
            configuration: configuration
                .into_iter()
                .map(|(key, value)| (key, ConfigurationValue::Literal(value)))
                .collect(),
            dependencies: BTreeMap::new(),
        })
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        let expected = outlet_descriptor(spec.descriptor.id().clone())?;
        if spec.descriptor != expected || spec.completion != Some(SinkCompletion::Handled) {
            anyhow::bail!("query result outlet requires a typed, locally handled input");
        }
        Ok(())
    }
    fn validate_scope(
        &self,
        graph: &str,
        spec: &ComponentSpecification,
        _: &BTreeMap<ResourceId, ResourceSpecification>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> anyhow::Result<()> {
        self.validate(spec)?;
        if let Some(catalog) = spec
            .dependencies
            .get("catalog")
            .and_then(|ids| ids.first())
            .and_then(|id| resources.get(id))
        {
            if catalog.get::<QueryResultsCatalog>()?.graph_id() != graph {
                anyhow::bail!("query outlet catalogue belongs to another graph");
            }
        }
        Ok(())
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let catalog = context
            .resources::<QueryResultsCatalog>("catalog")
            .map_err(ComponentCreationError::terminal)?
            .pop()
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing query catalogue"))
            })?;
        Ok(ConstructedComponent::sink(Box::new(
            QueryResultsOutlet::new(context.component_id, catalog.as_ref().clone()),
        )))
    }
}

impl QueryResultsCatalog {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn register(
        &self,
        id: ComponentId,
        reader: Arc<super::query_output::QueryOutputProvider>,
        progress: Option<Arc<QuerySourceProgress>>,
        config_hash: u64,
        incarnation: Arc<std::sync::RwLock<Option<uuid::Uuid>>>,
        checkpoint: Arc<std::sync::RwLock<Option<Arc<dyn drasi_core::interface::CheckpointStore>>>>,
        output_persistence: Arc<std::sync::RwLock<Option<bool>>>,
    ) -> anyhow::Result<QueryRegistration> {
        self.register_output(
            id,
            Arc::new(super::query_api::QueryApiOutput::Local(LocalQueryOutput {
                config_hash,
                results: reader.results(),
                reader,
                progress,
                incarnation,
                _checkpoint: checkpoint,
                output_persistence,
            })),
        )
    }

    pub(super) fn register_output(
        &self,
        id: ComponentId,
        output: Arc<super::query_api::QueryApiOutput>,
    ) -> anyhow::Result<QueryRegistration> {
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?;
        let generation = state.next_generation;
        state.next_generation = generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("query reader generation exhausted"))?;
        if let Some(previous) = state.entries.insert(
            id.clone(),
            CatalogQuery {
                generation,
                output,
                telemetry: Arc::new(Default::default()),
            },
        ) {
            previous.telemetry.publish(None);
        }
        self.0.changed.send_modify(|version| *version += 1);
        Ok(QueryRegistration {
            catalog: Arc::downgrade(&self.0),
            id,
            generation,
        })
    }
    pub(crate) fn link_query(
        &self,
        id: &ComponentId,
        source: &Self,
    ) -> anyhow::Result<CatalogLink> {
        let id = id.clone();
        let mut entry = source
            .get(&id)?
            .ok_or_else(|| anyhow::anyhow!("native query reader has not been created"))?;
        let mut state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?;
        let generation = state.next_generation;
        state.next_generation = generation
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("query generation exhausted"))?;
        entry.generation = generation;
        state.entries.insert(id.clone(), entry);
        self.0.changed.send_modify(|version| *version += 1);
        Ok(CatalogLink {
            _registration: QueryRegistration {
                catalog: Arc::downgrade(&self.0),
                id,
                generation,
            },
        })
    }
    #[cfg(test)]
    pub(crate) async fn legacy_snapshot(
        &self,
        id: &ComponentId,
        timeout: Duration,
    ) -> anyhow::Result<crate::queries::SnapshotResponse> {
        let (snapshot, hash) = self.query_snapshot(id, timeout).await?;
        if let Some(metrics) = self.configured_metrics(&id.clone())? {
            metrics.record_snapshot_fetch();
        }
        super::plugin_reaction::snapshot_response_parts(id, &snapshot, hash)
    }
    #[cfg(test)]
    pub(crate) async fn legacy_outbox(
        &self,
        id: &ComponentId,
        after: u64,
        timeout: Duration,
    ) -> std::result::Result<crate::queries::OutboxResponse, crate::queries::FetchError> {
        let view = self.query_view(id, timeout).await.map_err(|error| {
            log::warn!("Native outbox is not available: {error:#}");
            if error
                .downcast_ref::<tokio::time::error::Elapsed>()
                .is_some()
            {
                crate::queries::FetchError::TimedOut
            } else {
                crate::queries::FetchError::NotRunning {
                    status: crate::ComponentStatus::Error,
                }
            }
        })?;
        super::plugin_reaction::outbox_response(&view, after)
    }
    pub(super) fn get(&self, id: &ComponentId) -> anyhow::Result<Option<CatalogQuery>> {
        Ok(self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?
            .entries
            .get(id)
            .cloned())
    }
    pub(super) async fn ready(
        &self,
        id: &ComponentId,
        timeout: Duration,
    ) -> anyhow::Result<CatalogQuery> {
        let id = id.clone();
        let mut changed = self.0.changed.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                let entry = self.get(&id)?;
                if let Some(entry) = entry {
                    let wait = entry.output.wait_ready();
                    tokio::select! {
                        result = wait => {
                            if self.get(&id)?.is_some_and(|current| current.generation == entry.generation) {
                                result?;
                                return Ok(entry);
                            }
                        }
                        result = changed.changed() => { result?; }
                    }
                } else { changed.changed().await?; }
            }
        }).await?
    }

    fn require_registration(&self, id: &ComponentId, entry: &CatalogQuery) -> anyhow::Result<()> {
        anyhow::ensure!(
            self.get(&id.clone())?
                .is_some_and(|current| current.generation == entry.generation),
            GraphError::StaleGeneration
        );
        Ok(())
    }

    #[cfg(test)]
    pub(super) async fn query_view(
        &self,
        id: &ComponentId,
        timeout: Duration,
    ) -> anyhow::Result<super::plugin_reaction::OutputView> {
        tokio::time::timeout(timeout, async {
            let entry = self.ready(id, timeout).await?;
            let view = entry.output.view().await?;
            self.require_registration(id, &entry)?;
            Ok(view)
        })
        .await?
    }

    pub(super) async fn query_snapshot(
        &self,
        id: &ComponentId,
        timeout: Duration,
    ) -> anyhow::Result<(QueryResultSnapshot, u64)> {
        tokio::time::timeout(timeout, async {
            let entry = self.ready(id, timeout).await?;
            let snapshot = entry.output.snapshot().await?;
            self.require_registration(id, &entry)?;
            Ok((snapshot, entry.output.configuration_hash()))
        })
        .await?
    }

    pub async fn snapshot(
        &self,
        id: &ComponentId,
        timeout: Duration,
    ) -> anyhow::Result<QueryResultSnapshot> {
        Ok(self.query_snapshot(id, timeout).await?.0)
    }
    pub async fn replay(
        &self,
        id: &ComponentId,
        after: u64,
        timeout: Duration,
    ) -> anyhow::Result<Vec<ChangeEnvelope>> {
        tokio::time::timeout(timeout, async {
            let entry = self.ready(id, timeout).await?;
            let replay = entry.output.replay(after).await?;
            self.require_registration(id, &entry)?;
            Ok(replay)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_contracts::{QueryDescription, QueryOutputView, QueryViewProvider};
    use std::num::NonZeroUsize;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct PausedQueryReader {
        description: QueryDescription,
        reader: Arc<super::super::query_output::QueryOutputProvider>,
        pause_head: AtomicBool,
        pause_reads: AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }

    #[async_trait::async_trait]
    impl QueryViewProvider for PausedQueryReader {
        fn description(&self) -> QueryDescription {
            self.description.clone()
        }

        async fn query_head(&self) -> std::result::Result<(OutputCursor, u64), OutputReadError> {
            let head = self.reader.query_head();
            if self.pause_head.swap(false, Ordering::AcqRel) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            head
        }

        async fn query_snapshot(
            &self,
        ) -> std::result::Result<(OutputSnapshot, u64), OutputReadError> {
            if self.pause_reads.load(Ordering::Acquire) {
                std::future::pending::<()>().await;
            }
            self.reader.query_snapshot()
        }

        async fn query_view(
            &self,
        ) -> std::result::Result<(OutputCursor, QueryOutputView), OutputReadError> {
            if self.pause_reads.load(Ordering::Acquire) {
                std::future::pending::<()>().await;
            }
            self.reader.query_view_with_cursor()
        }

        async fn query_replay(
            &self,
            after: &OutputCursor,
        ) -> std::result::Result<(OutputReplay, u64), OutputReadError> {
            if self.pause_reads.load(Ordering::Acquire) {
                std::future::pending::<()>().await;
            }
            self.reader.query_replay(after)
        }
    }

    async fn asynchronous_catalog() -> anyhow::Result<(
        ContinuousQueryTransformer,
        QueryResultsCatalog,
        Arc<QueryApi>,
        Arc<PausedQueryReader>,
    )> {
        let definition = ContinuousQueryDefinition {
            graph_id: "asynchronous-reader".into(),
            id: crate::test_helpers::component_id(46),
            query: "MATCH (n:Item) RETURN n.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out")?,
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        };
        let execution = QueryExecutionSettings::default();
        let description = QueryDescription {
            graph_id: definition.graph_id.clone(),
            query: definition.query.clone(),
            language: definition.language,
            output_stream: definition.output_stream.clone(),
            outbox_capacity: definition.outbox_capacity,
            bootstrap: false,
            auto_reset: false,
            volatile: true,
            configuration_hash: definition.configuration_hash(&execution)?,
            execution,
        };
        let catalog = QueryResultsCatalog::new(definition.graph_id.as_str())?;
        let mut query = ContinuousQueryTransformer::new(
            definition,
            Arc::new(drasi_core::computation::InMemoryComputationProvider),
        )
        .await?;
        query.start(&lifecycle_contexts::start()).await?;
        let provider = Arc::new(PausedQueryReader {
            reader: super::super::query_output::QueryOutputProvider::new(
                &query.results(),
                &description.output_stream,
            ),
            description,
            pause_head: AtomicBool::new(true),
            pause_reads: AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let api = QueryApi::from_native_reader(
            crate::test_helpers::component_id(46),
            provider.clone(),
            None,
        )?
        .with_native_result_catalog(catalog.clone(), crate::DispatchMode::Channel, 1)?;
        Ok((query, catalog, api, provider))
    }

    async fn first_output(
        query: &mut ContinuousQueryTransformer,
    ) -> anyhow::Result<ChangeEnvelope> {
        use drasi_core::models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        };
        Ok(query
            .transform(InputEnvelope {
                port: PortId::try_new("in")?,
                execution_context: Default::default(),
                envelope: GraphChangeCodec::encode_change(
                    SourceChange::Insert {
                        element: Element::Node {
                            metadata: ElementMetadata {
                                reference: ElementReference::new("source", "one"),
                                labels: Arc::from([Arc::from("Item")]),
                                effective_from: 1,
                            },
                            properties: ElementPropertyMap::from(serde_json::json!({"value": 1})),
                        },
                    },
                    StreamId::try_new("source/out")?,
                    1,
                    None,
                )?,
            })
            .await?
            .remove(0)
            .envelope)
    }

    #[tokio::test]
    async fn asynchronous_subscription_attaches_before_sampling_its_head() -> anyhow::Result<()> {
        let (mut query, catalog, _api, provider) = asynchronous_catalog().await?;
        let query_id =
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                .expect("fixture UUID");
        let mut attaching = Box::pin(catalog.subscribe_query_at_head(&query_id));
        tokio::select! {
            result = &mut attaching => anyhow::bail!("head did not pause: {}", result.is_ok()),
            _ = provider.entered.notified() => {}
        }
        assert_eq!(catalog.0.state.lock().unwrap().subscribers.len(), 1);
        catalog.publish(first_output(&mut query).await?).await?;
        provider.release.notify_one();
        let (mut subscription, head) = attaching.await?;
        assert_eq!(head, 0);
        let delivered =
            tokio::time::timeout(Duration::from_secs(1), subscription.receive_after_head())
                .await??;
        assert_eq!(QueryChangeCodec::query_sequence(&delivered)?, 1);
        query.stop(&lifecycle_contexts::stop()).await?;
        Ok(())
    }

    /// Released observers never hold a publication: a full bounded subscriber
    /// lags instead, reads its buffer, then the typed gap, and receives nothing
    /// after the gap until it has observed it; a reading subscriber loses nothing.
    #[tokio::test]
    async fn released_observers_lag_a_full_subscriber_in_order() -> anyhow::Result<()> {
        let (mut query, catalog, _api, provider) = asynchronous_catalog().await?;
        provider.pause_head.store(false, Ordering::Release);
        let query_id = crate::test_helpers::component_id(46);
        let (mut full, _) = catalog.subscribe_query_at_head(&query_id).await?;
        let (mut reading, _) = catalog.subscribe_query_at_head(&query_id).await?;
        let observers = ObserverGate::new();
        let first = first_output(&mut query).await?;
        let second = first.clone();
        catalog.publish_observed(first.clone(), &observers).await?;
        reading.receive().await?;

        // Unreleased, the full subscriber backpressures publication; a release
        // wakes that same publication without waiting for the reader.
        let mut blocked = Box::pin(catalog.publish_observed(second.clone(), &observers));
        tokio::select! {
            result = &mut blocked => anyhow::bail!("publication did not backpressure: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        observers.release_observers();
        tokio::time::timeout(Duration::from_secs(1), blocked).await??;
        reading.receive().await?;
        tokio::time::timeout(
            Duration::from_secs(1),
            catalog.publish_observed(second.clone(), &observers),
        )
        .await??;
        reading.receive().await?;
        observers.reclaim_observers();
        // Still lagging: skipped (not delivered after the gap) without waiting.
        tokio::time::timeout(
            Duration::from_secs(1),
            catalog.publish_observed(second.clone(), &observers),
        )
        .await??;
        reading.receive().await?;

        full.receive().await?;
        let gap = full
            .receive()
            .await
            .expect_err("the gap is reported after the buffer");
        assert!(matches!(
            gap.downcast_ref::<tokio::sync::broadcast::error::RecvError>(),
            Some(tokio::sync::broadcast::error::RecvError::Lagged(3))
        ));
        catalog.publish_observed(second, &observers).await?;
        full.receive().await?;
        reading.receive().await?;
        query.stop(&lifecycle_contexts::stop()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn cancelled_subscription_head_releases_its_channel() -> anyhow::Result<()> {
        let (mut query, catalog, _api, provider) = asynchronous_catalog().await?;
        let query_id =
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                .expect("fixture UUID");
        let mut attaching = Box::pin(catalog.subscribe_query_at_head(&query_id));
        tokio::select! {
            result = &mut attaching => anyhow::bail!("head did not pause: {}", result.is_ok()),
            _ = provider.entered.notified() => {}
        }
        drop(attaching);
        assert!(catalog.0.state.lock().unwrap().subscribers.is_empty());
        tokio::time::timeout(
            Duration::from_secs(1),
            catalog.publish(first_output(&mut query).await?),
        )
        .await??;
        query.stop(&lifecycle_contexts::stop()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn subscription_rejects_a_new_volatile_epoch_even_with_the_same_generation(
    ) -> anyhow::Result<()> {
        let (mut query, catalog, api, provider) = asynchronous_catalog().await?;
        provider.pause_head.store(false, Ordering::Release);
        let (subscription, _) = catalog
            .subscribe_query_at_head(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID"),
            )
            .await?;
        let head = subscription.head()?;
        api.validate_subscription(&head).await?;
        let (cursor, generation) = provider.reader.query_head()?;
        let producer = cursor.producer();
        query.output_owner().state.write().unwrap().producer =
            Some(GraphProducerIdentity::volatile(
                producer.construction_scope().to_owned(),
                producer.graph_id().to_owned(),
                producer.component_id().clone(),
                producer.stream().clone(),
            )?);
        assert_eq!(provider.reader.query_head()?.1, generation);
        assert!(api.validate_subscription(&head).await.is_err());
        query.stop(&lifecycle_contexts::stop()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn catalogue_read_deadlines_include_remote_read_time() -> anyhow::Result<()> {
        let (mut query, catalog, _api, provider) = asynchronous_catalog().await?;
        provider.pause_head.store(false, Ordering::Release);
        provider.pause_reads.store(true, Ordering::Release);
        let deadline = Duration::from_millis(10);
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            catalog.snapshot(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID"),
                deadline,
            ),
        )
        .await?
        .err()
        .expect("snapshot ignored its deadline");
        assert!(error.is::<tokio::time::error::Elapsed>());
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            catalog.replay(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID"),
                0,
                deadline,
            ),
        )
        .await?
        .expect_err("replay ignored its deadline");
        assert!(error.is::<tokio::time::error::Elapsed>());
        assert!(matches!(
            catalog
                .legacy_outbox(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                        .expect("fixture UUID"),
                    0,
                    deadline
                )
                .await,
            Err(crate::queries::FetchError::TimedOut)
        ));
        query.stop(&lifecycle_contexts::stop()).await?;
        Ok(())
    }

    #[tokio::test]
    async fn fresh_query_subscription_filters_replayed_output_by_logical_sequence(
    ) -> anyhow::Result<()> {
        use drasi_core::models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        };
        for mode in [crate::DispatchMode::Channel, crate::DispatchMode::Broadcast] {
            let catalog = QueryResultsCatalog::new("subscription-head")?;
            let stream = StreamId::try_new("query/out")?;
            let query_id = ComponentId::generate();
            let config = crate::Query::cypher(
                drasi_core_types::ComponentName::try_new("query").expect("fixture component name"),
            )
            .query("MATCH (n:Person) RETURN n.name AS name")
            .with_outbox_capacity(8)
            .enable_bootstrap(false)
            .build()?
            .with_assigned_id(query_id.clone());
            let mut config = config;
            config.dispatch_mode = Some(mode);
            let mut query = ContinuousQueryTransformer::new(
                ContinuousQueryDefinition {
                    graph_id: "subscription-head".into(),
                    id: query_id.clone(),
                    query: config.query.clone(),
                    language: ComputationQueryLanguage::Cypher,
                    output_stream: stream.clone(),
                    outbox_capacity: NonZeroUsize::new(8).unwrap(),
                },
                Arc::new(drasi_core::computation::InMemoryComputationProvider),
            )
            .await?
            .with_query_configuration(config)?
            .with_result_catalog(&catalog)?;
            query.start(&lifecycle_contexts::start()).await?;
            let input = |sequence, name: &str| -> anyhow::Result<InputEnvelope> {
                Ok(InputEnvelope {
                    execution_context: Default::default(),
                    port: PortId::try_new("in")?,
                    envelope: GraphChangeCodec::encode_change(
                        SourceChange::Insert {
                            element: Element::Node {
                                metadata: ElementMetadata {
                                    reference: ElementReference::new("source", name),
                                    labels: vec!["Person".into()].into(),
                                    effective_from: sequence,
                                },
                                properties: ElementPropertyMap::from(
                                    serde_json::json!({ "name": name }),
                                ),
                            },
                        },
                        StreamId::try_new("source/out")?,
                        sequence,
                        None,
                    )?,
                })
            };
            let first = query
                .transform(input(1, "Alice")?)
                .await?
                .remove(0)
                .envelope;
            let (mut subscription, head) = catalog.subscribe_query_at_head(&query_id).await?;
            assert_eq!(head, 1);
            let second = query.transform(input(2, "Bob")?).await?.remove(0).envelope;
            for (envelope, transport) in [(&first, 99), (&second, 100)] {
                catalog
                    .publish(envelope.derive(
                        emission_id(&stream, transport)?,
                        envelope.changes().clone(),
                        SystemMetadata::new(stream.clone(), transport),
                    ))
                    .await?;
            }
            let received =
                tokio::time::timeout(Duration::from_secs(1), subscription.receive_after_head())
                    .await??;
            assert_eq!(received.system().sequence(), 100);
            assert_eq!(QueryChangeCodec::query_sequence(&received)?, 2);
            query.stop(&lifecycle_contexts::stop()).await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn query_replacement_refreshes_identity_and_delivery_without_resetting_transport() {
        let catalog = QueryResultsCatalog::new("delivery").unwrap();
        let id = ComponentId::generate();
        let stream = StreamId::try_new("query/out").unwrap();
        let open = |text: &'static str, capacity| {
            let catalog = catalog.clone();
            let id = id.clone();
            let stream = stream.clone();
            async move {
                let mut config = crate::Query::cypher(
                    drasi_core_types::ComponentName::try_new("query")
                        .expect("fixture component name"),
                )
                .query(text)
                .with_outbox_capacity(8)
                .build()
                .unwrap()
                .with_assigned_id(id.clone());
                config.dispatch_buffer_capacity = Some(capacity);
                ContinuousQueryTransformer::new(
                    ContinuousQueryDefinition {
                        graph_id: "delivery".into(),
                        id,
                        query: text.into(),
                        language: ComputationQueryLanguage::Cypher,
                        output_stream: stream,
                        outbox_capacity: std::num::NonZeroUsize::new(8).unwrap(),
                    },
                    Arc::new(drasi_core::computation::InMemoryComputationProvider),
                )
                .await
                .unwrap()
                .with_query_configuration(config)
                .unwrap()
                .with_result_catalog(&catalog)
                .unwrap()
            }
        };
        let mut first = open("MATCH (n) RETURN n", 1).await;
        let before = catalog
            .get(&id)
            .unwrap()
            .unwrap()
            .output
            .configuration_hash();
        let sequence = catalog.transport_sequence(&stream).unwrap();
        sequence.store(41, std::sync::atomic::Ordering::Release);
        let mut replacement = open("MATCH (n) RETURN n.name", 8).await;
        assert_eq!(
            catalog
                .transport_sequence(&stream)
                .unwrap()
                .load(std::sync::atomic::Ordering::Acquire),
            41
        );
        assert_ne!(
            catalog
                .get(&id)
                .unwrap()
                .unwrap()
                .output
                .configuration_hash(),
            before
        );
        assert_eq!(catalog.0.state.lock().unwrap().delivery[&id].capacity, 8);
        first.stop(&lifecycle_contexts::stop()).await.unwrap();
        replacement.stop(&lifecycle_contexts::stop()).await.unwrap();
    }

    #[tokio::test]
    async fn output_persistence_requires_an_explicit_observation_and_is_shared_by_links() {
        let catalog = QueryResultsCatalog::new("durability").unwrap();
        let id = crate::test_helpers::component_id(46);
        let mut query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: "durability".into(),
                id: id.clone(),
                query: "MATCH (n) RETURN n".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new("query/out").unwrap(),
                outbox_capacity: NonZeroUsize::new(8).unwrap(),
            },
            Arc::new(drasi_core::computation::InMemoryComputationProvider),
        )
        .await
        .unwrap()
        .with_result_catalog(&catalog)
        .unwrap();
        let linked = QueryResultsCatalog::new("ordinary").unwrap();
        let link = linked
            .link_query(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID"),
                &catalog,
            )
            .unwrap();
        let entry = catalog.get(&id).unwrap().unwrap();
        let super::super::query_api::QueryApiOutput::Local(output) = entry.output.as_ref() else {
            panic!("expected a linked query");
        };
        let observation = output.output_persistence.clone();

        assert!(!catalog
            .output_is_persistent(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID")
            )
            .unwrap());
        assert!(!linked
            .output_is_persistent(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID")
            )
            .unwrap());
        *observation.write().unwrap() = None;
        assert!(catalog
            .output_is_persistent(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID")
            )
            .is_err());
        assert!(linked
            .output_is_persistent(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID")
            )
            .is_err());
        *observation.write().unwrap() = Some(true);
        assert!(catalog
            .output_is_persistent(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID")
            )
            .unwrap());
        assert!(linked
            .output_is_persistent(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID")
            )
            .unwrap());
        *observation.write().unwrap() = Some(false);
        assert!(!linked
            .output_is_persistent(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID")
            )
            .unwrap());

        drop(link);
        assert!(linked
            .output_is_persistent(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID")
            )
            .is_err());
        query.stop(&lifecycle_contexts::stop()).await.unwrap();
        drop(query);
        assert!(catalog
            .output_is_persistent(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID")
            )
            .is_err());
    }

    #[tokio::test]
    async fn legacy_outbox_head_lookup_preserves_catalog_head_and_gap_checks() {
        use drasi_core::models::{
            Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        };

        let catalog = QueryResultsCatalog::new("outbox-head").unwrap();
        let mut query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: "outbox-head".into(),
                id: crate::test_helpers::component_id(46),
                query: "MATCH (n:Item) RETURN n.value AS value".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new("query/out").unwrap(),
                outbox_capacity: NonZeroUsize::new(1).unwrap(),
            },
            Arc::new(drasi_core::computation::InMemoryComputationProvider),
        )
        .await
        .unwrap()
        .with_result_catalog(&catalog)
        .unwrap();
        query.start(&lifecycle_contexts::start()).await.unwrap();
        for sequence in 1..=2 {
            query
                .transform(InputEnvelope {
                    execution_context: Default::default(),
                    port: PortId::try_new("in").unwrap(),
                    envelope: GraphChangeCodec::encode_change(
                        SourceChange::Insert {
                            element: Element::Node {
                                metadata: ElementMetadata {
                                    reference: ElementReference::new(
                                        "source",
                                        &format!("item-{sequence}"),
                                    ),
                                    labels: Arc::from([Arc::from("Item")]),
                                    effective_from: sequence,
                                },
                                properties: ElementPropertyMap::from(
                                    serde_json::json!({"value": sequence}),
                                ),
                            },
                        },
                        StreamId::try_new("source/out").unwrap(),
                        sequence,
                        None,
                    )
                    .unwrap(),
                })
                .await
                .unwrap();
        }
        let snapshot = catalog
            .legacy_snapshot(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID"),
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        let head = catalog
            .legacy_outbox(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID"),
                u64::MAX,
                Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(head.results.is_empty());
        assert_eq!(head.latest_sequence, 2);
        assert_eq!(head.latest_sequence, snapshot.as_of_sequence);
        assert_eq!(head.output_generation, snapshot.output_generation);
        assert_eq!(head.config_hash, snapshot.config_hash);
        assert!(matches!(
            catalog
                .legacy_outbox(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                        .expect("fixture UUID"),
                    0,
                    Duration::from_secs(1)
                )
                .await,
            Err(crate::queries::FetchError::OutboxGap(_))
        ));
        assert_eq!(
            catalog
                .legacy_outbox(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                        .expect("fixture UUID"),
                    1,
                    Duration::from_secs(1)
                )
                .await
                .unwrap()
                .results
                .len(),
            1
        );
        query.stop(&lifecycle_contexts::stop()).await.unwrap();
    }

    #[tokio::test]
    async fn output_generation_reader_survives_retirement_without_defaulting_to_zero() {
        let catalog = QueryResultsCatalog::new("generation-reader").unwrap();
        let id = crate::test_helpers::component_id(46);
        let mut query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: "generation-reader".into(),
                id: id.clone(),
                query: "MATCH (n) RETURN n".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new("query/out").unwrap(),
                outbox_capacity: NonZeroUsize::new(8).unwrap(),
            },
            Arc::new(drasi_core::computation::InMemoryComputationProvider),
        )
        .await
        .unwrap()
        .with_result_catalog(&catalog)
        .unwrap();
        let reader = catalog
            .output_generation_reader(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID"),
            )
            .unwrap();
        assert_eq!(reader.generation(), 0);
        let owner = query.output_owner();
        owner.state.write().unwrap().generation = 7;
        assert_eq!(reader.generation(), 7);
        query.stop(&lifecycle_contexts::stop()).await.unwrap();
        drop(query);
        assert!(catalog
            .output_generation_reader(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000002e")
                    .expect("fixture UUID")
            )
            .is_err());
        assert_eq!(reader.generation(), 7);

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _state = owner.state.write().unwrap();
            panic!("injected metadata lock poisoning");
        }));
        assert!(poisoned.is_err());
        assert_eq!(reader.generation(), 7);
    }

    #[allow(dead_code)]
    mod lifecycle_contexts {
        use crate::computation::v1::{
            CancellationToken, DisposeContext, DisposeReason, InitializeContext, OperationScope,
            PauseContext, ResumeContext, StartContext, StateIntent, StopContext, StopMode,
        };

        fn scope() -> OperationScope {
            OperationScope::new("test", "component", 1, None, CancellationToken::new())
        }

        pub fn initialize() -> InitializeContext {
            InitializeContext::new(scope())
        }

        pub fn start() -> StartContext {
            StartContext::new(scope())
        }

        pub fn pause() -> PauseContext {
            PauseContext::new(scope())
        }

        pub fn resume() -> ResumeContext {
            ResumeContext::new(scope())
        }

        pub fn stop() -> StopContext {
            StopContext::new(scope(), StopMode::Drain)
        }

        pub fn dispose() -> DisposeContext {
            DisposeContext::new(scope(), DisposeReason::Shutdown, StateIntent::Preserve, 1)
                .expect("shutdown preserves state")
        }

        pub fn erase() -> DisposeContext {
            DisposeContext::new(scope(), DisposeReason::Removal, StateIntent::Erase, 1)
                .expect("removal may erase state")
        }
    }
}
