// Copyright 2025 The Drasi Authors.
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

//! The state machine source.
//!
//! A single source component that:
//! * subscribes to the continuous queries referenced by its states (via the
//!   source query-subscription API — [`Source::subscribed_query_ids`] +
//!   [`Source::enqueue_query_result`]),
//! * evaluates state transitions with the pure [`StateMachine`] engine,
//! * durably persists each entity's state to the state store, and
//! * dispatches the resulting entity-state node changes to its own subscribers
//!   (downstream queries), bootstrapping late joiners from persisted state.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use log::{debug, error, info, warn};
use tokio::sync::Mutex;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::bootstrap::{
    BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
};
use drasi_lib::channels::{
    BootstrapEvent, BootstrapEventSender, ComponentStatus, QueryResult, SubscriptionResponse,
};
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::Source;

use crate::config::StateMachineSourceConfig;
use crate::engine::{EntityRecord, StateMachine};

/// A source that maps continuous-query results to per-entity state transitions
/// and exposes each entity's live state as a graph node.
pub struct StateMachineSource {
    base: SourceBase,
    config: StateMachineSourceConfig,
    engine: Arc<Mutex<StateMachine>>,
}

impl std::fmt::Debug for StateMachineSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StateMachineSource")
            .field("id", &self.base.id)
            .field("config", &self.config)
            .finish()
    }
}

impl StateMachineSource {
    /// Create a new state machine source registered under `id`.
    ///
    /// `id` is the source id downstream queries subscribe to.
    pub fn new(id: impl Into<String>, config: StateMachineSourceConfig) -> Result<Self> {
        Self::create(id.into(), config, None, true)
    }

    pub(crate) fn create(
        id: String,
        config: StateMachineSourceConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Result<Self> {
        config.validate()?;

        let mut params = SourceBaseParams::new(id).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        let engine = StateMachine::new(&config)?;

        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            engine: Arc::new(Mutex::new(engine)),
        })
    }

    /// Load persisted entity records into the engine so `previous` guards work
    /// across restarts, then return the loaded records for reference.
    async fn load_persisted_state(&self, store: &Arc<dyn StateStoreProvider>) -> Result<()> {
        let records = read_all_records(store.as_ref(), &self.base.id).await?;
        let loaded = records.len();
        self.engine.lock().await.load(records);
        info!(
            "[{}] loaded {} persisted entity states",
            self.base.id, loaded
        );
        Ok(())
    }

    /// Spawn the processing loop that drains the priority queue, applies
    /// transitions, persists them, and dispatches node changes to subscribers.
    fn spawn_processing_task(
        &self,
        store: Arc<dyn StateStoreProvider>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> tokio::task::JoinHandle<()> {
        let base = self.base.clone_shared();
        let engine = self.engine.clone();
        let source_id = self.base.id.clone();

        tokio::spawn(async move {
            info!("[{source_id}] state machine processing loop started");
            loop {
                let query_result_arc = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        info!("[{source_id}] state machine processing loop shutting down");
                        break;
                    }
                    result = base.priority_queue.dequeue() => result,
                };
                let query_result = (*query_result_arc).clone();
                debug!(
                    "[{source_id}] processing result from query '{}'",
                    query_result.query_id
                );

                let transitions = {
                    let mut sm = engine.lock().await;
                    sm.process(&query_result)
                };

                for record in transitions {
                    if let Err(e) = persist_record(store.as_ref(), &source_id, &record).await {
                        error!(
                            "[{source_id}] failed to persist entity '{}': {e}",
                            record.key
                        );
                        continue;
                    }

                    let element = record_to_node_element(&source_id, &record);
                    let change = if record.previous_state.is_none() {
                        SourceChange::Insert { element }
                    } else {
                        SourceChange::Update { element }
                    };

                    if let Err(e) = base.dispatch_source_change(change).await {
                        debug!(
                            "[{source_id}] could not dispatch state for '{}' (no subscribers): {e}",
                            record.key
                        );
                    } else {
                        info!(
                            "[{source_id}] entity '{}' -> {} (was {:?})",
                            record.key, record.state, record.previous_state
                        );
                    }
                }
            }
        })
    }
}

/// Read every persisted [`EntityRecord`] for a source partition.
async fn read_all_records(
    store: &dyn StateStoreProvider,
    partition: &str,
) -> Result<Vec<EntityRecord>> {
    let keys = store
        .list_keys(partition)
        .await
        .map_err(|e| anyhow::anyhow!("failed to list persisted entities: {e}"))?;

    let mut records = Vec::with_capacity(keys.len());
    for key in keys {
        if let Some(bytes) = store
            .get(partition, &key)
            .await
            .map_err(|e| anyhow::anyhow!("failed to read entity '{key}': {e}"))?
        {
            match serde_json::from_slice::<EntityRecord>(&bytes) {
                Ok(record) => records.push(record),
                Err(e) => error!("[{partition}] skipping corrupt entity record '{key}': {e}"),
            }
        }
    }
    Ok(records)
}

/// Persist a single entity record to the state store, partitioned by source id.
async fn persist_record(
    store: &dyn StateStoreProvider,
    source_id: &str,
    record: &EntityRecord,
) -> Result<()> {
    let bytes = serde_json::to_vec(record).context("serialize entity record")?;
    store
        .set(source_id, &record.key, bytes)
        .await
        .map_err(|e| anyhow::anyhow!("state store set failed: {e}"))
}

#[async_trait]
impl Source for StateMachineSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "state-machine"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.base.properties_or_serialize(&self.config)
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    fn supports_replay(&self) -> bool {
        false
    }

    fn subscribed_query_ids(&self) -> Vec<String> {
        self.config.referenced_queries()
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }

    async fn start(&self) -> Result<()> {
        info!("Starting StateMachineSource '{}'", self.base.id);
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting state machine source".to_string()),
            )
            .await;

        let store = self.base.state_store().await.ok_or_else(|| {
            anyhow::anyhow!(
                "state machine source '{}' requires a durable state store, but none is configured",
                self.base.id
            )
        })?;
        if !store.is_durable() {
            warn!(
                "[{}] state store is not durable; entity state will not survive restarts",
                self.base.id
            );
        }

        // Reload persisted state so `previous` guards work across restarts.
        self.load_persisted_state(&store).await?;

        // Install the bootstrap provider that replays persisted entity records to
        // late/downstream subscribers.
        let provider = StateMachineBootstrapProvider {
            partition: self.base.id.clone(),
            state_store: store.clone(),
        };
        self.base.set_bootstrap_provider(provider).await;

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        self.base.set_shutdown_tx(shutdown_tx).await;

        let task = self.spawn_processing_task(store, shutdown_rx);
        self.base.set_task_handle(task).await;

        self.base
            .set_status(ComponentStatus::Running, Some("Running".to_string()))
            .await;
        info!("[{}] state machine source started", self.base.id);
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping StateMachineSource '{}'", self.base.id);
        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping state machine source".to_string()),
            )
            .await;

        self.base.stop_common().await?;

        self.base
            .set_status(ComponentStatus::Stopped, Some("Stopped".to_string()))
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "StateMachine")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }
}

/// Bootstrap provider that replays persisted entity records as node inserts.
struct StateMachineBootstrapProvider {
    partition: String,
    state_store: Arc<dyn StateStoreProvider>,
}

#[async_trait]
impl BootstrapProvider for StateMachineBootstrapProvider {
    async fn bootstrap(
        &self,
        request: BootstrapRequest,
        context: &BootstrapContext,
        event_tx: BootstrapEventSender,
        _settings: Option<&drasi_lib::config::SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        let records = read_all_records(self.state_store.as_ref(), &self.partition).await?;

        let mut count = 0usize;
        for record in records {
            // Filter by requested node labels (all our nodes share one label).
            if !request.node_labels.is_empty() && !request.node_labels.contains(&record.label) {
                continue;
            }

            let element = record_to_node_element(&self.partition, &record);
            let sequence = context.next_sequence();
            event_tx
                .send(BootstrapEvent {
                    source_id: context.source_id.clone(),
                    change: SourceChange::Insert { element },
                    timestamp: chrono::Utc::now(),
                    sequence,
                })
                .await
                .map_err(|e| anyhow::anyhow!("failed to send bootstrap node: {e}"))?;
            count += 1;
        }

        info!(
            "StateMachineSource '{}' bootstrapped {} entity nodes for query '{}'",
            self.partition, count, request.query_id
        );

        Ok(BootstrapResult {
            event_count: count,
            source_position: None,
        })
    }
}

/// Build a graph node [`Element`] from a persisted entity record.
pub(crate) fn record_to_node_element(source_id: &str, record: &EntityRecord) -> Element {
    let properties = ElementPropertyMap::from(&record.node_properties());
    let effective_from = if record.entered_at > 0 {
        record.entered_at as u64
    } else {
        chrono::Utc::now().timestamp_millis().max(0) as u64
    };
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source_id, &record.key),
            labels: Arc::from(vec![Arc::from(record.label.as_str())]),
            effective_from,
        },
        properties,
    }
}
