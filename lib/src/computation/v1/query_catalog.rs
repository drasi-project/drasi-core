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
pub(super) struct CatalogQuery {
    pub generation: u64,
    pub config_hash: u64,
    pub results: QueryResults,
    pub progress: Option<Arc<QuerySourceProgress>>,
    pub incarnation: Arc<std::sync::RwLock<Option<uuid::Uuid>>>,
    pub status: Option<watch::Receiver<crate::ComponentStatus>>,
    pub checkpoint: Arc<std::sync::RwLock<Option<Arc<dyn drasi_core::interface::CheckpointStore>>>>,
    pub output_persistence: Arc<std::sync::RwLock<Option<bool>>>,
}

#[derive(Clone)]
pub(crate) struct QueryOutputGenerationReader {
    query: ComponentId,
    results: QueryResults,
}

impl QueryOutputGenerationReader {
    pub(crate) fn generation(&self) -> u64 {
        self.results
            .state
            .read()
            .unwrap_or_else(|poisoned| {
                log::error!(
                    "Reading stored output generation for '{}' from poisoned query state",
                    self.query
                );
                poisoned.into_inner()
            })
            .generation
    }
}

struct CatalogState {
    entries: BTreeMap<ComponentId, CatalogQuery>,
    next_generation: u64,
    delivery: BTreeMap<ComponentId, DeliveryConfiguration>,
    subscribers: BTreeMap<(ComponentId, u64), Subscriber>,
    next_subscriber: u64,
}
struct DeliveryConfiguration {
    mode: crate::DispatchMode,
    capacity: usize,
    hash: u64,
    metrics: Arc<crate::metrics::QueryOutputMetrics>,
    status: watch::Receiver<crate::ComponentStatus>,
}
#[derive(Clone)]
enum Subscriber {
    Channel(tokio::sync::mpsc::Sender<ChangeEnvelope>),
    Broadcast(tokio::sync::broadcast::Sender<ChangeEnvelope>),
}
enum SubscriberReceiver {
    Channel(tokio::sync::mpsc::Receiver<ChangeEnvelope>),
    Broadcast(tokio::sync::broadcast::Receiver<ChangeEnvelope>),
}
pub(crate) struct CatalogSubscription {
    receiver: SubscriberReceiver,
    catalog: Weak<CatalogInner>,
    key: (ComponentId, u64),
    head: Option<QuerySubscriptionHead>,
}

#[derive(Clone)]
pub(crate) struct QuerySubscriptionHead {
    pub(super) sequence: u64,
    pub(super) generation: u64,
    pub(super) config_hash: u64,
    pub(super) incarnation: Option<uuid::Uuid>,
    results: QueryResults,
}

impl QuerySubscriptionHead {
    pub(super) fn belongs_to(&self, query: &CatalogQuery) -> bool {
        // Linked parent/child catalogs assign different registration generations.
        Arc::ptr_eq(&self.results.state, &query.results.state)
    }
}
impl CatalogSubscription {
    pub(crate) fn head(&self) -> anyhow::Result<QuerySubscriptionHead> {
        self.head
            .clone()
            .ok_or_else(|| anyhow::anyhow!("subscription has no captured query head"))
    }

    pub(crate) async fn receive(&mut self) -> anyhow::Result<ChangeEnvelope> {
        match &mut self.receiver {
            SubscriberReceiver::Channel(receiver) => receiver
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("query output subscription closed")),
            SubscriberReceiver::Broadcast(receiver) => receiver.recv().await.map_err(Into::into),
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
                state.entries.remove(&self.id);
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
    pub(super) async fn publish(&self, envelope: ChangeEnvelope) -> anyhow::Result<()> {
        let id = ComponentId::try_new(QueryChangeCodec::metadata(&envelope)?.query_id)?;
        if self.get(&id)?.is_none() {
            anyhow::bail!("query publication is not registered in this catalogue");
        }
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
            let envelope = envelope.clone();
            async move {
                let closed = match subscriber {
                    Subscriber::Channel(sender) => sender.send(envelope).await.is_err(),
                    Subscriber::Broadcast(sender) => sender.send(envelope).is_err(),
                };
                (key, closed)
            }
        });
        for (key, closed) in futures::future::join_all(deliveries).await {
            if closed {
                self.0
                    .state
                    .lock()
                    .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?
                    .subscribers
                    .remove(&key);
                log::trace!("Retired a closed native query output subscription");
            }
        }
        if self.0.events.send(envelope).is_err() {
            log::trace!("Native query publication has no live subscribers; retained results remain available");
        }
        Ok(())
    }
    pub(super) fn same_registry(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn configure_delivery(
        &self,
        id: &str,
        mode: crate::DispatchMode,
        capacity: usize,
        hash: u64,
        metrics: Arc<crate::metrics::QueryOutputMetrics>,
        status: watch::Receiver<crate::ComponentStatus>,
    ) -> anyhow::Result<()> {
        if capacity == 0 {
            anyhow::bail!("query delivery capacity must be nonzero");
        }
        self.0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?
            .delivery
            .insert(
                ComponentId::try_new(id)?,
                DeliveryConfiguration {
                    mode,
                    capacity,
                    hash,
                    metrics,
                    status,
                },
            );
        Ok(())
    }
    pub(crate) fn subscribe_query(&self, id: &str) -> anyhow::Result<CatalogSubscription> {
        self.subscribe_query_at_head(id)
            .map(|(subscription, _)| subscription)
    }
    pub(crate) fn subscribe_query_at_head(
        &self,
        id: &str,
    ) -> anyhow::Result<(CatalogSubscription, u64)> {
        let id = ComponentId::try_new(id)?;
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
        let output = query
            .results
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        let head = Self::read_head(&query, output.sequence, output.generation, output.ready)?;
        let mut receiver = self.subscribe_query_locked(&mut state, id)?;
        receiver.head = Some(head);
        Ok((receiver, output.sequence))
    }

    pub(super) fn query_head(&self, id: &str) -> anyhow::Result<QuerySubscriptionHead> {
        let state = self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?;
        let query = state
            .entries
            .get(&ComponentId::try_new(id)?)
            .ok_or_else(|| anyhow::anyhow!("query is not registered"))?;
        let output = query
            .results
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        Self::read_head(query, output.sequence, output.generation, output.ready)
    }

    fn read_head(
        query: &CatalogQuery,
        sequence: u64,
        generation: u64,
        ready: bool,
    ) -> anyhow::Result<QuerySubscriptionHead> {
        anyhow::ensure!(ready, "query output is not ready for subscriptions");
        if let Some(status) = &query.status {
            anyhow::ensure!(
                matches!(
                    *status.borrow(),
                    crate::ComponentStatus::Starting | crate::ComponentStatus::Running
                ),
                "query is not running"
            );
        }
        Ok(QuerySubscriptionHead {
            sequence,
            generation,
            config_hash: query.config_hash,
            incarnation: *query
                .incarnation
                .read()
                .map_err(|_| anyhow::anyhow!("query output identity poisoned"))?,
            results: query.results.clone(),
        })
    }

    pub(crate) fn output_is_persistent(&self, id: &str) -> anyhow::Result<bool> {
        let query = self
            .get(&ComponentId::try_new(id)?)?
            .ok_or_else(|| anyhow::anyhow!("query is not registered"))?;
        let persistence = *query
            .output_persistence
            .read()
            .map_err(|_| anyhow::anyhow!("query output persistence observation poisoned"))?;
        persistence.ok_or_else(|| anyhow::anyhow!("query output persistence is not yet known"))
    }

    /// Capture while the query is registered. The reader retains only output
    /// state, not index handles, and remains usable after permanent shutdown.
    pub(crate) fn output_generation_reader(
        &self,
        id: &str,
    ) -> anyhow::Result<QueryOutputGenerationReader> {
        let id = ComponentId::try_new(id)?;
        let query = self
            .get(&id)?
            .ok_or_else(|| anyhow::anyhow!("query is not registered"))?;
        Ok(QueryOutputGenerationReader {
            query: id,
            results: query.results,
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
                (
                    Subscriber::Channel(sender),
                    SubscriberReceiver::Channel(receiver),
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
    pub(super) fn configured_hash(&self, id: &ComponentId) -> anyhow::Result<Option<u64>> {
        Ok(self
            .0
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("query catalogue ownership poisoned"))?
            .delivery
            .get(id)
            .map(|config| config.hash))
    }
    pub(crate) fn checkpoint_store(
        &self,
        id: &str,
    ) -> anyhow::Result<Option<Arc<dyn drasi_core::interface::CheckpointStore>>> {
        let Some(entry) = self.get(&ComponentId::try_new(id)?)? else {
            return Ok(None);
        };
        let checkpoint = entry
            .checkpoint
            .read()
            .map_err(|_| anyhow::anyhow!("query checkpoint view poisoned"))?
            .clone();
        Ok(checkpoint)
    }
    pub(crate) fn current_sequence(&self, id: &str) -> anyhow::Result<u64> {
        let query = self
            .get(&ComponentId::try_new(id)?)?
            .ok_or_else(|| anyhow::anyhow!("query is not registered"))?;
        let state = query
            .results
            .state
            .read()
            .map_err(|_| anyhow::anyhow!("query output state poisoned"))?;
        Ok(state.sequence)
    }
}

pub struct QueryResultsOutlet {
    descriptor: ComponentDescriptor,
    catalog: QueryResultsCatalog,
}
impl QueryResultsOutlet {
    pub fn new(id: ComponentId, catalog: QueryResultsCatalog) -> Self {
        Self {
            descriptor: ComponentDescriptor::try_new(
                id,
                vec![PortDescriptor::new(
                    PortId::try_new("in").expect("port"),
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("outlet"),
            catalog,
        }
    }
}
#[async_trait::async_trait]
impl ComputationComponent for QueryResultsOutlet {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl EnvelopeSink for QueryResultsOutlet {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.catalog.publish(input.envelope).await
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
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }
    fn validate(&self, spec: &ComponentSpecification) -> anyhow::Result<()> {
        let expected = ComponentDescriptor::try_new(
            spec.descriptor.id().clone(),
            vec![PortDescriptor::new(
                PortId::try_new("in")?,
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?;
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
        results: QueryResults,
        progress: Option<Arc<QuerySourceProgress>>,
        config_hash: u64,
        incarnation: Arc<std::sync::RwLock<Option<uuid::Uuid>>>,
        checkpoint: Arc<std::sync::RwLock<Option<Arc<dyn drasi_core::interface::CheckpointStore>>>>,
        output_persistence: Arc<std::sync::RwLock<Option<bool>>>,
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
        let config_hash = state
            .delivery
            .get(&id)
            .map_or(config_hash, |config| config.hash);
        let status = state.delivery.get(&id).map(|config| config.status.clone());
        state.entries.insert(
            id.clone(),
            CatalogQuery {
                generation,
                config_hash,
                results,
                progress,
                incarnation,
                status,
                checkpoint,
                output_persistence,
            },
        );
        self.0.changed.send_modify(|version| *version += 1);
        Ok(QueryRegistration {
            catalog: Arc::downgrade(&self.0),
            id,
            generation,
        })
    }
    pub(crate) fn link_query(&self, id: &str, source: &Self) -> anyhow::Result<CatalogLink> {
        let id = ComponentId::try_new(id)?;
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
    pub(crate) async fn legacy_snapshot(
        &self,
        id: &str,
        timeout: Duration,
    ) -> anyhow::Result<crate::queries::SnapshotResponse> {
        let entry = self.ready(id, timeout).await?;
        if let Some(metrics) = self.configured_metrics(&ComponentId::try_new(id)?)? {
            metrics.record_snapshot_fetch();
        }
        super::plugin_reaction::snapshot_response(id, &super::plugin_reaction::output_view(&entry)?)
    }
    pub(crate) async fn legacy_outbox(
        &self,
        id: &str,
        after: u64,
        timeout: Duration,
    ) -> std::result::Result<crate::queries::OutboxResponse, crate::queries::FetchError> {
        let entry = self.ready(id, timeout).await.map_err(|error| {
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
        let view = super::plugin_reaction::output_view(&entry).map_err(|error| {
            log::error!("Cannot read native output state: {error:#}");
            crate::queries::FetchError::NotRunning {
                status: crate::ComponentStatus::Error,
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
    pub(super) async fn ready(&self, id: &str, timeout: Duration) -> anyhow::Result<CatalogQuery> {
        let id = ComponentId::try_new(id)?;
        let mut changed = self.0.changed.subscribe();
        tokio::time::timeout(timeout, async {
            loop {
                let entry = self.get(&id)?;
                if let Some(entry) = entry {
                    if entry.status.as_ref().is_some_and(|status| !matches!(*status.borrow(), crate::ComponentStatus::Starting | crate::ComponentStatus::Running)) {
                        anyhow::bail!("query is not running");
                    }
                    let wait = async {
                        if let Some(progress) = &entry.progress { progress.wait_ready().await?; }
                        entry.results.wait_ready().await?;
                        Ok::<_, anyhow::Error>(())
                    };
                    tokio::select! {
                        result = wait => {
                            result?;
                            if self.get(&id)?.is_some_and(|current| current.generation == entry.generation) { return Ok(entry); }
                        }
                        result = changed.changed() => { result?; }
                        _ = async {
                            if let Some(mut status) = entry.status.clone() {
                                let _ = status.wait_for(|status| !matches!(status, crate::ComponentStatus::Starting | crate::ComponentStatus::Running)).await;
                            } else { std::future::pending::<()>().await; }
                        } => anyhow::bail!("query stopped while waiting for output"),
                    }
                } else { changed.changed().await?; }
            }
        }).await?
    }
    pub async fn snapshot(&self, id: &str, timeout: Duration) -> anyhow::Result<QuerySnapshot> {
        self.ready(id, timeout)
            .await?
            .results
            .snapshot()
            .map_err(Into::into)
    }
    pub async fn replay(
        &self,
        id: &str,
        after: u64,
        timeout: Duration,
    ) -> anyhow::Result<Vec<ChangeEnvelope>> {
        self.ready(id, timeout)
            .await?
            .results
            .replay(after)
            .map_err(Into::into)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroUsize;

    #[tokio::test]
    async fn output_persistence_requires_an_explicit_observation_and_is_shared_by_links() {
        let catalog = QueryResultsCatalog::new("durability").unwrap();
        let id = ComponentId::try_new("query").unwrap();
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
        let link = linked.link_query("query", &catalog).unwrap();
        let observation = catalog.get(&id).unwrap().unwrap().output_persistence;

        assert!(!catalog.output_is_persistent("query").unwrap());
        assert!(!linked.output_is_persistent("query").unwrap());
        *observation.write().unwrap() = None;
        assert!(catalog.output_is_persistent("query").is_err());
        assert!(linked.output_is_persistent("query").is_err());
        *observation.write().unwrap() = Some(true);
        assert!(catalog.output_is_persistent("query").unwrap());
        assert!(linked.output_is_persistent("query").unwrap());
        *observation.write().unwrap() = Some(false);
        assert!(!linked.output_is_persistent("query").unwrap());

        drop(link);
        assert!(linked.output_is_persistent("query").is_err());
        query.stop().await.unwrap();
        drop(query);
        assert!(catalog.output_is_persistent("query").is_err());
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
                id: ComponentId::try_new("query").unwrap(),
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
        query.start().await.unwrap();
        for sequence in 1..=2 {
            query
                .transform(InputEnvelope {
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
            .legacy_snapshot("query", Duration::from_secs(1))
            .await
            .unwrap();
        let head = catalog
            .legacy_outbox("query", u64::MAX, Duration::from_secs(1))
            .await
            .unwrap();
        assert!(head.results.is_empty());
        assert_eq!(head.latest_sequence, 2);
        assert_eq!(head.latest_sequence, snapshot.as_of_sequence);
        assert_eq!(head.output_generation, snapshot.output_generation);
        assert_eq!(head.config_hash, snapshot.config_hash);
        assert!(matches!(
            catalog
                .legacy_outbox("query", 0, Duration::from_secs(1))
                .await,
            Err(crate::queries::FetchError::OutboxGap(_))
        ));
        assert_eq!(
            catalog
                .legacy_outbox("query", 1, Duration::from_secs(1))
                .await
                .unwrap()
                .results
                .len(),
            1
        );
        query.stop().await.unwrap();
    }

    #[tokio::test]
    async fn output_generation_reader_survives_retirement_without_defaulting_to_zero() {
        let catalog = QueryResultsCatalog::new("generation-reader").unwrap();
        let id = ComponentId::try_new("query").unwrap();
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
        let reader = catalog.output_generation_reader("query").unwrap();
        assert_eq!(reader.generation(), 0);
        catalog
            .get(&id)
            .unwrap()
            .unwrap()
            .results
            .state
            .write()
            .unwrap()
            .generation = 7;
        assert_eq!(reader.generation(), 7);
        query.stop().await.unwrap();
        drop(query);
        assert!(catalog.output_generation_reader("query").is_err());
        assert_eq!(reader.generation(), 7);

        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _state = reader.results.state.write().unwrap();
            panic!("injected metadata lock poisoning");
        }));
        assert!(poisoned.is_err());
        assert_eq!(reader.generation(), 7);
    }
}
