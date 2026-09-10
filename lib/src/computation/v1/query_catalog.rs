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
    pub incarnation: Option<uuid::Uuid>,
}
struct CatalogState {
    entries: BTreeMap<ComponentId, CatalogQuery>,
    next_generation: u64,
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
    pub(super) fn publish(&self, envelope: ChangeEnvelope) -> anyhow::Result<()> {
        let id = ComponentId::try_new(QueryChangeCodec::metadata(&envelope)?.query_id)?;
        if self.get(&id)?.is_none() {
            anyhow::bail!("query publication is not registered in this catalogue");
        }
        if self.0.events.send(envelope).is_err() {
            log::trace!("Native query publication has no live subscribers; retained results remain available");
        }
        Ok(())
    }
    pub(super) fn same_registry(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
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
        self.catalog.publish(input.envelope)
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
    pub(super) fn register(
        &self,
        id: ComponentId,
        results: QueryResults,
        progress: Option<Arc<QuerySourceProgress>>,
        config_hash: u64,
        volatile: bool,
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
        state.entries.insert(
            id.clone(),
            CatalogQuery {
                generation,
                config_hash,
                results,
                progress,
                incarnation: volatile.then(uuid::Uuid::new_v4),
            },
        );
        self.0.changed.send_modify(|version| *version += 1);
        Ok(QueryRegistration {
            catalog: Arc::downgrade(&self.0),
            id,
            generation,
        })
    }
    fn get(&self, id: &ComponentId) -> anyhow::Result<Option<CatalogQuery>> {
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
