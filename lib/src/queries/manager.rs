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

use anyhow::{Context, Result};
use async_trait::async_trait;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::{Notify, RwLock};

// Import drasi-core components
use drasi_core::{
    evaluation::context::{QueryPartEvaluationContext, QueryVariables},
    evaluation::functions::FunctionRegistry,
    evaluation::variable_value::VariableValue,
    middleware::MiddlewareTypeRegistry,
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_functions_gql::GQLFunctionSet;
use drasi_query_ast::api::{QueryConfiguration, QueryParser};
use drasi_query_cypher::CypherParser;
use drasi_query_gql::GQLParser;

use crate::channels::*;
use crate::config::SourceSubscriptionSettings;
use crate::config::{QueryConfig, QueryLanguage, QueryRuntime};
use crate::managers::{
    is_operation_valid, log_component_error, log_component_start, log_component_stop,
    ComponentEventHistory, ComponentLogKey, ComponentLogRegistry, Operation,
};
use crate::queries::PriorityQueue;
use crate::queries::QueryBase;
use crate::sources::Source;
use crate::sources::{FutureQueueSource, SourceManager, FUTURE_QUEUE_SOURCE_ID};
use tracing::Instrument;

/// Default query configuration
struct DefaultQueryConfig;

impl QueryConfiguration for DefaultQueryConfig {
    fn get_aggregating_function_names(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        set.insert("count".into());
        set.insert("sum".into());
        set.insert("min".into());
        set.insert("max".into());
        set.insert("avg".into());
        set.insert("collect".into());
        set.insert("stdev".into());
        set.insert("stdevp".into());
        set
    }
}

/// Convert QueryVariables (`BTreeMap<Box<str>, VariableValue>`) to JSON
fn convert_query_variables_to_json(vars: &QueryVariables) -> serde_json::Value {
    let mut result = serde_json::Map::new();
    for (key, value) in vars.iter() {
        result.insert(key.to_string(), convert_variable_value_to_json(value));
    }
    serde_json::Value::Object(result)
}

/// Convert a single VariableValue to JSON
fn convert_variable_value_to_json(value: &VariableValue) -> serde_json::Value {
    match value {
        VariableValue::Null => serde_json::Value::Null,
        VariableValue::Bool(b) => serde_json::Value::Bool(*b),
        VariableValue::Float(f) => {
            // Float might be NaN or Infinity, handle gracefully
            serde_json::Value::String(f.to_string())
        }
        VariableValue::Integer(i) => {
            // Integer might be too large for JSON number
            serde_json::Value::String(i.to_string())
        }
        VariableValue::String(s) => serde_json::Value::String(s.clone()),
        VariableValue::List(list) => {
            serde_json::Value::Array(list.iter().map(convert_variable_value_to_json).collect())
        }
        VariableValue::Object(map) => {
            let mut result = serde_json::Map::new();
            for (k, v) in map.iter() {
                result.insert(k.clone(), convert_variable_value_to_json(v));
            }
            serde_json::Value::Object(result)
        }
        // For complex types, convert to string representation
        _ => serde_json::Value::String(format!("{value:?}")),
    }
}

#[async_trait]
pub trait Query: Send + Sync {
    /// Start the query - subscribes to sources and begins processing events
    async fn start(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
    fn get_config(&self) -> &QueryConfig;
    fn as_any(&self) -> &dyn std::any::Any;

    /// Subscribe to query results for reactions
    /// Returns a broadcast receiver for Arc-wrapped QueryResults
    async fn subscribe(&self, reaction_id: String) -> Result<QuerySubscriptionResponse, String>;
}

/// Bootstrap phase tracking for each source
#[derive(Debug, Clone, PartialEq)]
enum BootstrapPhase {
    NotStarted,
    InProgress,
    Completed,
}

pub struct DrasiQuery {
    // DrasiLib instance ID for log routing isolation
    instance_id: String,
    // Use QueryBase for common functionality
    base: QueryBase,
    #[allow(dead_code)]
    continuous_query: Option<ContinuousQuery>,
    current_results: Arc<RwLock<Vec<serde_json::Value>>>,
    // Priority queue for ordered event processing
    priority_queue: PriorityQueue,
    // Reference to SourceManager for direct subscription
    source_manager: Arc<SourceManager>,
    // Track subscription tasks for cleanup
    subscription_tasks: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
    // Abort handles for bootstrap + supervisor tasks (for cleanup on stop)
    bootstrap_abort_handles: Arc<RwLock<Vec<tokio::task::AbortHandle>>>,
    // Track bootstrap state per source
    bootstrap_state: Arc<RwLock<HashMap<String, BootstrapPhase>>>,
    // IndexFactory for creating storage backend indexes
    index_factory: Arc<crate::indexes::IndexFactory>,
    // Middleware registry for query middleware
    middleware_registry: Arc<MiddlewareTypeRegistry>,
    // Future queue source (if using temporal queries)
    future_queue_source: Arc<RwLock<Option<Arc<FutureQueueSource>>>>,
}

impl DrasiQuery {
    pub fn new(
        instance_id: impl Into<String>,
        config: QueryConfig,
        event_tx: ComponentEventSender,
        source_manager: Arc<SourceManager>,
        index_factory: Arc<crate::indexes::IndexFactory>,
        middleware_registry: Arc<MiddlewareTypeRegistry>,
    ) -> Result<Self> {
        // Create priority queue with configured capacity (fallback to 10000 if not set)
        let priority_capacity = config.priority_queue_capacity.unwrap_or(10000);
        let priority_queue = PriorityQueue::new(priority_capacity);

        // Create QueryBase for common functionality
        let base = QueryBase::new(config, event_tx).context("Failed to create QueryBase")?;

        Ok(Self {
            instance_id: instance_id.into(),
            base,
            continuous_query: None,
            current_results: Arc::new(RwLock::new(Vec::new())),
            priority_queue,
            source_manager,
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            bootstrap_abort_handles: Arc::new(RwLock::new(Vec::new())),
            bootstrap_state: Arc::new(RwLock::new(HashMap::new())),
            index_factory,
            middleware_registry,
            future_queue_source: Arc::new(RwLock::new(None)),
        })
    }

    pub async fn get_current_results(&self) -> Vec<serde_json::Value> {
        self.current_results.read().await.clone()
    }
}

#[cfg(test)]
impl DrasiQuery {
    /// Count active subscription forwarder tasks (testing helper)
    pub async fn subscription_task_count(&self) -> usize {
        self.subscription_tasks.read().await.len()
    }
}

#[async_trait]
impl Query for DrasiQuery {
    async fn start(&self) -> Result<()> {
        log_component_start("Query", &self.base.config.id);

        *self.base.status.write().await = ComponentStatus::Starting;
        self.bootstrap_state.write().await.clear();

        let event = ComponentEvent {
            component_id: self.base.config.id.clone(),
            component_type: ComponentType::Query,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting query".to_string()),
        };

        if let Err(e) = self.base.event_tx.send(event).await {
            error!("Failed to send component event: {e}");
        }

        // Build and initialize the actual Drasi ContinuousQuery
        let query_str = self.base.config.query.clone();

        // Create a parser and function registry based on the query language
        let config = Arc::new(DefaultQueryConfig);
        let (parser, function_registry): (Arc<dyn QueryParser>, Arc<FunctionRegistry>) =
            match self.base.config.query_language {
                QueryLanguage::Cypher => {
                    debug!(
                        "Query '{}' using Cypher parser and function set",
                        self.base.config.id
                    );
                    (
                        Arc::new(CypherParser::new(config)),
                        Arc::new(FunctionRegistry::new()).with_cypher_function_set(),
                    )
                }
                QueryLanguage::GQL => {
                    debug!(
                        "Query '{}' using GQL parser and function set",
                        self.base.config.id
                    );
                    (
                        Arc::new(GQLParser::new(config)),
                        Arc::new(FunctionRegistry::new()).with_gql_function_set(),
                    )
                }
            };

        let mut builder =
            QueryBuilder::new(&query_str, parser).with_function_registry(function_registry);

        // Configure middleware registry and middleware
        builder = builder.with_middleware_registry(self.middleware_registry.clone());

        // Add all middleware configurations from config
        for mw in &self.base.config.middleware {
            builder = builder.with_source_middleware(Arc::new(mw.clone()));
        }

        // Configure source pipelines for all subscriptions
        for sub in &self.base.config.sources {
            builder = builder.with_source_pipeline(&sub.source_id, &sub.pipeline);
        }

        // Add joins if configured
        if let Some(joins) = &self.base.config.joins {
            debug!(
                "Query '{}' has {} configured joins",
                self.base.config.id,
                joins.len()
            );
            let drasi_joins: Vec<drasi_core::models::QueryJoin> =
                joins.iter().cloned().map(|j| j.into()).collect();
            builder = builder.with_joins(drasi_joins);
        }

        // Build indexes - either from configured backend or default in-memory
        let future_queue = if let Some(backend_ref) = &self.base.config.storage_backend {
            debug!(
                "Query '{}' using storage backend: {:?}",
                self.base.config.id, backend_ref
            );
            let index_factory = self.index_factory.clone();

            let index_set = index_factory
                .build(backend_ref, &self.base.config.id)
                .await
                .context("Failed to build index set")?;

            let future_queue = index_set.future_queue.clone();

            builder = builder
                .with_element_index(index_set.element_index)
                .with_archive_index(index_set.archive_index)
                .with_result_index(index_set.result_index)
                .with_future_queue(index_set.future_queue);

            future_queue
        } else {
            debug!(
                "Query '{}' using default in-memory indexes",
                self.base.config.id
            );
            // Create explicit in-memory future queue so we can access it for FutureQueueSource
            let future_queue = Arc::new(
                drasi_core::in_memory_index::in_memory_future_queue::InMemoryFutureQueue::new(),
            );
            builder = builder.with_future_queue(future_queue.clone());
            future_queue
        };

        let continuous_query = match builder.try_build().await {
            Ok(query) => query,
            Err(e) => {
                error!("Failed to build query '{}': {}", self.base.config.id, e);
                *self.base.status.write().await = ComponentStatus::Error;

                let event = ComponentEvent {
                    component_id: self.base.config.id.clone(),
                    component_type: ComponentType::Query,
                    status: ComponentStatus::Error,
                    timestamp: chrono::Utc::now(),
                    message: Some(format!("Failed to build query: {e}")),
                };

                if let Err(e) = self.base.event_tx.send(event).await {
                    error!("Failed to send component event: {e}");
                }

                return Err(anyhow::anyhow!("Failed to build query: {e}"));
            }
        };

        // Extract labels from the query for bootstrap
        let labels = match crate::queries::LabelExtractor::extract_labels(
            &query_str,
            &self.base.config.query_language,
        ) {
            Ok(labels) => labels,
            Err(e) => {
                warn!("Failed to extract labels from query '{}': {}. Bootstrap will request all data.",
                    self.base.config.id, e);
                crate::queries::QueryLabels {
                    node_labels: vec![],
                    relation_labels: vec![],
                }
            }
        };

        // Build subscription settings for each source
        let subscription_settings =
            match crate::queries::SubscriptionSettingsBuilder::build_subscription_settings(
                &self.base.config,
                &labels,
            ) {
                Ok(settings) => settings,
                Err(e) => {
                    error!(
                        "Failed to build subscription settings for query '{}': {}",
                        self.base.config.id, e
                    );
                    *self.base.status.write().await = ComponentStatus::Error;

                    let event = ComponentEvent {
                        component_id: self.base.config.id.clone(),
                        component_type: ComponentType::Query,
                        status: ComponentStatus::Error,
                        timestamp: chrono::Utc::now(),
                        message: Some(format!("Failed to build subscription settings: {e}")),
                    };

                    if let Err(e) = self.base.event_tx.send(event).await {
                        error!("Failed to send component event: {e}");
                    }

                    return Err(anyhow::anyhow!(
                        "Failed to build subscription settings: {e}"
                    ));
                }
            };

        // Subscribe to each source sequentially
        // Also includes FutureQueueSource for temporal query support

        // Set up FutureQueueSource for temporal query support
        // This creates a virtual source that polls the future queue and emits due elements
        // as source events, integrating temporal queries into the standard source subscription
        // mechanism. Events preserve their original source_id from the FutureElementRef.
        debug!(
            "Query '{}' setting up FutureQueueSource for temporal queries",
            self.base.config.id
        );

        let future_queue_source = Arc::new(FutureQueueSource::new(
            future_queue,
            self.base.config.id.clone(),
        ));

        // Start the FutureQueueSource
        if let Err(e) = future_queue_source.start().await {
            error!(
                "Query '{}' failed to start FutureQueueSource: {}",
                self.base.config.id, e
            );
            *self.base.status.write().await = ComponentStatus::Error;
            return Err(anyhow::anyhow!("Failed to start FutureQueueSource: {e}"));
        }

        info!(
            "Query '{}' FutureQueueSource started successfully",
            self.base.config.id
        );

        // Store the FutureQueueSource for cleanup
        *self.future_queue_source.write().await = Some(future_queue_source.clone());

        info!(
            "Query '{}' subscribing to {} sources: {:?}",
            self.base.config.id,
            self.base.config.sources.len(),
            self.base
                .config
                .sources
                .iter()
                .map(|s| &s.source_id)
                .collect::<Vec<_>>()
        );

        let mut bootstrap_channels = Vec::new();
        let mut subscription_tasks: Vec<tokio::task::JoinHandle<()>> = Vec::new();

        // Build list of sources to subscribe to (regular sources + FutureQueueSource)
        let mut sources_to_subscribe: Vec<(String, Arc<dyn Source>, SourceSubscriptionSettings)> =
            Vec::new();

        // Add regular sources from SourceManager
        for (idx, subscription) in self.base.config.sources.iter().enumerate() {
            let source_id = &subscription.source_id;
            match self.source_manager.get_source_instance(source_id).await {
                Some(src) => {
                    sources_to_subscribe.push((
                        source_id.clone(),
                        src,
                        subscription_settings[idx].clone(),
                    ));
                }
                None => {
                    error!(
                        "Query '{}' failed to find source '{}' in SourceManager",
                        self.base.config.id, source_id
                    );
                    // Cleanup already-spawned tasks before returning error
                    for handle in subscription_tasks.drain(..) {
                        handle.abort();
                        let _ = handle.await;
                    }
                    *self.base.status.write().await = ComponentStatus::Error;
                    return Err(anyhow::anyhow!("Source '{source_id}' not found"));
                }
            }
        }

        // Add FutureQueueSource
        sources_to_subscribe.push((
            FUTURE_QUEUE_SOURCE_ID.to_string(),
            future_queue_source as Arc<dyn Source>,
            SourceSubscriptionSettings {
                source_id: FUTURE_QUEUE_SOURCE_ID.to_string(),
                enable_bootstrap: false,
                query_id: self.base.config.id.clone(),
                nodes: HashSet::new(),
                relations: HashSet::new(),
            },
        ));

        for (source_id, source, settings) in sources_to_subscribe {
            let subscription_response = match source.subscribe(settings.clone()).await {
                Ok(response) => response,
                Err(e) => {
                    error!(
                        "Query '{}' failed to subscribe to source '{}': {}",
                        self.base.config.id, source_id, e
                    );
                    // Cleanup already-spawned tasks before returning error
                    for handle in subscription_tasks.drain(..) {
                        handle.abort();
                        let _ = handle.await;
                    }
                    *self.base.status.write().await = ComponentStatus::Error;
                    return Err(anyhow::anyhow!(
                        "Failed to subscribe to source '{source_id}': {e}"
                    ));
                }
            };

            info!(
                "Query '{}' successfully subscribed to source '{}'",
                self.base.config.id, source_id
            );

            // Store bootstrap channel if provided
            // Also initialize bootstrap state only for sources that support bootstrap
            if let Some(bootstrap_rx) = subscription_response.bootstrap_receiver {
                bootstrap_channels.push((source_id.clone(), bootstrap_rx));
                self.bootstrap_state
                    .write()
                    .await
                    .insert(source_id.to_string(), BootstrapPhase::NotStarted);
            }

            // Spawn task to forward events from receiver to priority queue
            let mut receiver = subscription_response.receiver;
            let priority_queue = self.priority_queue.clone();
            let query_id = self.base.config.id.clone();
            let source_id_clone = source_id.clone();
            let instance_id = self.instance_id.clone();

            // Get source dispatch mode to determine enqueue strategy
            let dispatch_mode = source.dispatch_mode();
            let use_blocking_enqueue =
                matches!(dispatch_mode, crate::channels::DispatchMode::Channel);

            let span = tracing::info_span!(
                "query_source_forwarder",
                instance_id = %instance_id,
                component_id = %query_id,
                component_type = "query"
            );
            let task = tokio::spawn(
                async move {
                    debug!(
                        "Query '{query_id}' started event forwarder for source '{source_id_clone}' (dispatch_mode: {dispatch_mode:?}, blocking_enqueue: {use_blocking_enqueue})"
                    );

                    loop {
                        match receiver.recv().await {
                            Ok(arc_event) => {
                                // Use appropriate enqueue method based on dispatch mode
                                if use_blocking_enqueue {
                                    // Channel mode: Use blocking enqueue to prevent message loss
                                    // This creates backpressure when the priority queue is full
                                    priority_queue.enqueue_wait(arc_event).await;
                                } else {
                                    // Broadcast mode: Use non-blocking enqueue to prevent deadlock
                                    // Messages may be dropped when priority queue is full
                                    if !priority_queue.enqueue(arc_event).await {
                                        warn!(
                                            "Query '{query_id}' priority queue at capacity, dropping event from source '{source_id_clone}' (broadcast mode)"
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                error!(
                                    "Query '{query_id}' receiver error for source '{source_id_clone}': {e}"
                                );
                                info!(
                                    "Query '{query_id}' channel closed for source '{source_id_clone}'"
                                );
                                break;
                            }
                        }
                    }

                    debug!("Query '{query_id}' event forwarder exited for source '{source_id_clone}'");
                }
                .instrument(span),
            );

            subscription_tasks.push(task);
        }

        // Store subscription tasks
        *self.subscription_tasks.write().await = subscription_tasks;

        // Wrap continuous_query in Arc for sharing across tasks
        let continuous_query = Arc::new(continuous_query);

        // Gate that blocks the streaming event processor until bootstrap completes.
        // Events buffer safely in the priority queue during bootstrap.
        let bootstrap_gate = Arc::new(Notify::new());

        // NEW: Handle bootstrap channels
        if !bootstrap_channels.is_empty() {
            info!(
                "Query '{}' starting bootstrap from {} sources",
                self.base.config.id,
                bootstrap_channels.len()
            );

            // Emit bootstrapStarted control signal
            let mut metadata = HashMap::new();
            metadata.insert(
                "control_signal".to_string(),
                serde_json::json!("bootstrapStarted"),
            );
            metadata.insert(
                "source_count".to_string(),
                serde_json::json!(bootstrap_channels.len()),
            );

            let control_result = QueryResult::new(
                self.base.config.id.clone(),
                chrono::Utc::now(),
                vec![],
                metadata,
            );

            // Dispatch the control signal to all subscribed reactions
            self.base.dispatch_query_result(control_result).await.ok();
            info!(
                "[BOOTSTRAP] Emitted bootstrapStarted signal for query '{}'",
                self.base.config.id
            );

            // Process bootstrap events from each source
            let continuous_query_clone = continuous_query.clone();
            let base_dispatchers = self.base.dispatchers.clone();
            let query_id = self.base.config.id.clone();
            let bootstrap_state = self.bootstrap_state.clone();
            let instance_id = self.instance_id.clone();
            let bootstrap_current_results = self.current_results.clone();

            let mut bootstrap_handles = Vec::new();
            let mut abort_handles = Vec::new();

            for (source_id, mut bootstrap_rx) in bootstrap_channels {
                // Mark source bootstrap as in progress
                bootstrap_state
                    .write()
                    .await
                    .insert(source_id.to_string(), BootstrapPhase::InProgress);

                info!(
                    "[BOOTSTRAP] Query '{query_id}' processing bootstrap from source '{source_id}'"
                );

                let continuous_query_ref = continuous_query_clone.clone();
                let query_id_clone = query_id.clone();
                let source_id_clone = source_id.clone();
                let bootstrap_state_clone = bootstrap_state.clone();
                let base_dispatchers_clone = base_dispatchers.clone();
                let instance_id_clone = instance_id.clone();
                let current_results_clone = bootstrap_current_results.clone();
                let bootstrap_gate_clone = bootstrap_gate.clone();

                let span = tracing::info_span!(
                    "query_bootstrap",
                    instance_id = %instance_id_clone,
                    component_id = %query_id,
                    component_type = "query"
                );
                let handle = tokio::spawn(
                    async move {
                        let mut count = 0u64;

                        while let Some(bootstrap_event) = bootstrap_rx.recv().await {
                            count += 1;

                            // Process bootstrap change through ContinuousQuery
                            match continuous_query_ref
                                .process_source_change(bootstrap_event.change)
                                .await
                            {
                                Ok(results) => {
                                    if !results.is_empty() {
                                        debug!(
                                            "[BOOTSTRAP] Query '{}' received {} results from bootstrap event {}",
                                            query_id_clone, results.len(), count
                                        );

                                        // Apply bootstrap results to current_results so they
                                        // are visible via the query results API.
                                        let mut result_set = current_results_clone.write().await;
                                        for ctx in &results {
                                            match ctx {
                                                QueryPartEvaluationContext::Adding { after, .. } => {
                                                    result_set.push(convert_query_variables_to_json(after));
                                                }
                                                QueryPartEvaluationContext::Removing { before, .. } => {
                                                    let data = convert_query_variables_to_json(before);
                                                    result_set.retain(|item| item != &data);
                                                }
                                                QueryPartEvaluationContext::Updating { before, after, .. } => {
                                                    let before_json = convert_query_variables_to_json(before);
                                                    let after_json = convert_query_variables_to_json(after);
                                                    if let Some(pos) = result_set.iter().position(|item| item == &before_json) {
                                                        result_set[pos] = after_json;
                                                    } else {
                                                        result_set.retain(|item| item != &before_json);
                                                        result_set.push(after_json);
                                                    }
                                                }
                                                QueryPartEvaluationContext::Aggregation { .. }
                                                | QueryPartEvaluationContext::Noop => {}
                                            }
                                        }
                                        drop(result_set);
                                    }
                                }
                                Err(e) => {
                                    error!(
                                        "[BOOTSTRAP] Query '{query_id_clone}' failed to process bootstrap event from source '{source_id_clone}': {e}"
                                    );
                                }
                            }
                        }

                        info!(
                            "[BOOTSTRAP] Query '{query_id_clone}' completed bootstrap from source '{source_id_clone}' ({count} events)"
                        );

                        // Mark source bootstrap as completed and check if all sources are done
                        // under a single write lock to prevent duplicate completion signals
                        let all_completed = {
                            let mut state = bootstrap_state_clone.write().await;
                            state.insert(source_id_clone.to_string(), BootstrapPhase::Completed);
                            state
                                .values()
                                .all(|phase| *phase == BootstrapPhase::Completed)
                        };

                        if all_completed {
                            info!(
                                "[BOOTSTRAP] Query '{query_id_clone}' all sources completed bootstrap"
                            );

                            // Emit bootstrapCompleted control signal
                            let mut metadata = HashMap::new();
                            metadata.insert(
                                "control_signal".to_string(),
                                serde_json::json!("bootstrapCompleted"),
                            );

                            let control_result = QueryResult::new(
                                query_id_clone.clone(),
                                chrono::Utc::now(),
                                vec![],
                                metadata,
                            );

                            let arc_result = Arc::new(control_result);

                            // Dispatch bootstrapCompleted signal to all reactions
                            let dispatchers = base_dispatchers_clone.read().await;
                            let mut dispatched = false;
                            for dispatcher in dispatchers.iter() {
                                if dispatcher.dispatch_change(arc_result.clone()).await.is_ok() {
                                    dispatched = true;
                                }
                            }

                            if !dispatched {
                                debug!(
                                    "No reactions subscribed to query '{query_id_clone}' for bootstrapCompleted signal"
                                );
                            } else {
                                info!(
                                    "[BOOTSTRAP] Emitted bootstrapCompleted signal for query '{query_id_clone}'"
                                );
                            }

                            // Open the bootstrap gate so the event processor can start
                            bootstrap_gate_clone.notify_one();
                            info!("[BOOTSTRAP] Query '{query_id_clone}' bootstrap gate opened");
                        }
                    }
                    .instrument(span),
                );
                abort_handles.push(handle.abort_handle());
                bootstrap_handles.push(handle);
            }

            // Supervisor task: monitors all bootstrap tasks and opens the gate
            // with an Error status if any task panics. Without this, a panicked
            // bootstrap task would leave the gate closed forever.
            {
                let bootstrap_gate_clone = bootstrap_gate.clone();
                let status_clone = self.base.status.clone();
                let event_tx_clone = self.base.event_tx.clone();
                let query_id_clone = self.base.config.id.clone();
                let instance_id_clone = self.instance_id.clone();

                let span = tracing::info_span!(
                    "bootstrap_supervisor",
                    instance_id = %instance_id_clone,
                    component_id = %query_id_clone,
                    component_type = "query"
                );
                let supervisor_handle = tokio::spawn(
                    async move {
                        let results = futures::future::join_all(bootstrap_handles).await;
                        let panic_count = results.iter().filter(|r| matches!(r, Err(e) if e.is_panic())).count();

                        if panic_count > 0 {
                            error!(
                                "[BOOTSTRAP] Query '{query_id_clone}' {panic_count} bootstrap task(s) panicked, \
                                 transitioning to Error and opening gate"
                            );

                            *status_clone.write().await = ComponentStatus::Error;

                            let error_event = ComponentEvent {
                                component_id: query_id_clone.clone(),
                                component_type: ComponentType::Query,
                                status: ComponentStatus::Error,
                                timestamp: chrono::Utc::now(),
                                message: Some(format!(
                                    "Bootstrap failed: {panic_count} task(s) panicked"
                                )),
                            };
                            let _ = event_tx_clone.send(error_event).await;

                            bootstrap_gate_clone.notify_one();
                        }
                        // If all tasks succeeded, the last one already opened the gate.
                    }
                    .instrument(span),
                );
                abort_handles.push(supervisor_handle.abort_handle());
            }

            // Store abort handles for cleanup on stop()
            *self.bootstrap_abort_handles.write().await = abort_handles;
        } else {
            info!(
                "Query '{}' no bootstrap channels, skipping bootstrap",
                self.base.config.id
            );
            // No bootstrap needed — open the gate immediately
            bootstrap_gate.notify_one();
        }

        // NEW: Spawn event processor task that reads from priority queue
        let continuous_query_for_processor = continuous_query.clone();
        let base_dispatchers = self.base.dispatchers.clone();
        let query_id = self.base.config.id.clone();
        let current_results = self.current_results.clone();
        let task_handle_clone = self.base.task_handle.clone();
        let priority_queue = self.priority_queue.clone();
        let status = self.base.status.clone();
        let instance_id = self.instance_id.clone();
        let event_tx_for_processor = self.base.event_tx.clone();
        let config_id_for_processor = self.base.config.id.clone();

        // Create shutdown channel for graceful termination
        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        self.base.set_shutdown_tx(shutdown_tx).await;

        let span = tracing::info_span!(
            "query_processor",
            instance_id = %instance_id,
            component_id = %query_id,
            component_type = "query"
        );
        let handle = tokio::spawn(
            async move {
                info!("Query '{query_id}' waiting for bootstrap gate before processing events");

                // Wait for bootstrap to complete (or immediate signal if no bootstrap).
                // If shutdown arrives while waiting, exit cleanly.
                tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        info!(
                            "Query '{query_id}' received shutdown during bootstrap wait, exiting"
                        );
                        return;
                    }

                    _ = bootstrap_gate.notified() => {
                        info!("Query '{query_id}' bootstrap gate opened, starting event processing");
                    }
                }

                // Bootstrap complete — transition to Running only if still Starting.
                // If stop() was called during bootstrap, status may already be
                // Stopping and we must not overwrite it.
                let should_run = {
                    let mut guard = status.write().await;
                    if matches!(*guard, ComponentStatus::Starting) {
                        *guard = ComponentStatus::Running;
                        true
                    } else {
                        warn!(
                            "Query '{query_id}' bootstrap completed but status is {:?}, \
                             skipping transition to Running",
                            *guard
                        );
                        false
                    }
                };

                if should_run {
                    let running_event = ComponentEvent {
                        component_id: config_id_for_processor,
                        component_type: ComponentType::Query,
                        status: ComponentStatus::Running,
                        timestamp: chrono::Utc::now(),
                        message: Some("Query started successfully".to_string()),
                    };

                    if let Err(e) = event_tx_for_processor.send(running_event).await {
                        error!("Failed to send component event: {e}");
                    }
                }

                info!("Query '{query_id}' starting priority queue event processor");

                loop {
                    // Check if query is still running
                    if !matches!(*status.read().await, ComponentStatus::Running) {
                        info!(
                            "Query '{query_id}' status changed to non-running, exiting processing loop"
                        );
                        break;
                    }

                    // Use select to wait for either an event OR shutdown signal
                    let arc_event = tokio::select! {
                        // Check for shutdown signal first (biased)
                        biased;

                        _ = &mut shutdown_rx => {
                            info!(
                                "Query '{query_id}' received shutdown signal, exiting processing loop"
                            );
                            break;
                        }

                        // Dequeue events from priority queue (blocks until available)
                        event = priority_queue.dequeue() => event,
                    };

                    // Try to extract without cloning if we have sole ownership (zero-copy path).
                    // This succeeds in Channel dispatch mode where each query has its own event copy.
                    // Falls back to cloning in Broadcast mode where events are shared.
                    let (source_id, event, _timestamp, profiling_opt) =
                        match SourceEventWrapper::try_unwrap_arc(arc_event) {
                            Ok(parts) => parts,
                            Err(arc) => {
                                // Shared reference - must clone the data we need
                                (
                                    arc.source_id.clone(),
                                    arc.event.clone(),
                                    arc.timestamp,
                                    arc.profiling.clone(),
                                )
                            }
                        };

                    debug!("Query '{query_id}' processing event from source '{source_id}'");

                    // Extract the SourceChange from the SourceEvent (now owned, no clone needed)
                    let source_change = match event {
                        SourceEvent::Change(change) => change,
                        SourceEvent::Control(_) => {
                            debug!(
                                "Query '{query_id}' ignoring control event from source '{source_id}'"
                            );
                            continue;
                        }
                    };

                    // Use profiling metadata from source event or create new
                    let mut profiling =
                        profiling_opt.unwrap_or_else(crate::profiling::ProfilingMetadata::new);

                    // Capture query_receive_ns timestamp
                    profiling.query_receive_ns = Some(crate::profiling::timestamp_ns());

                    // Process the change through the actual Drasi continuous query
                    profiling.query_core_call_ns = Some(crate::profiling::timestamp_ns());

                    match continuous_query_for_processor
                        .process_source_change(source_change)
                        .await
                    {
                        Ok(results) => {
                            profiling.query_core_return_ns = Some(crate::profiling::timestamp_ns());
                            if !results.is_empty() {
                                debug!(
                                    "Query '{}' received {} results from drasi-core",
                                    query_id,
                                    results.len()
                                );

                                // Convert Drasi results to our QueryResult format
                                let converted_results: Vec<ResultDiff> = results
                                    .iter()
                                    .map(|ctx| match ctx {
                                        QueryPartEvaluationContext::Adding { after, .. } => {
                                            debug!("Query '{query_id}' got Adding context");
                                            ResultDiff::Add {
                                                data: convert_query_variables_to_json(after),
                                            }
                                        }
                                    QueryPartEvaluationContext::Removing { before, .. } => {
                                        debug!(
                                            "Query '{query_id}' got Removing context"
                                        );
                                        ResultDiff::Delete {
                                            data: convert_query_variables_to_json(before),
                                        }
                                    }
                                    QueryPartEvaluationContext::Updating { before, after, .. } => {
                                        debug!("Query '{query_id}' got Updating context");
                                        ResultDiff::Update {
                                            data: convert_query_variables_to_json(after),
                                            before: convert_query_variables_to_json(before),
                                            after: convert_query_variables_to_json(after),
                                            grouping_keys: None,
                                        }
                                    }
                                    QueryPartEvaluationContext::Aggregation {
                                        before, after, ..
                                    } => {
                                        ResultDiff::Aggregation {
                                            before: before.as_ref().map(convert_query_variables_to_json),
                                            after: convert_query_variables_to_json(after),
                                        }
                                    }
                                    QueryPartEvaluationContext::Noop => {
                                        ResultDiff::Noop
                                    }
                                })
                                .collect();

                            // Update the current result set based on the changes
                            let mut result_set = current_results.write().await;
                            for result in &converted_results {
                                match result {
                                    ResultDiff::Add { data } => {
                                        result_set.push(data.clone());
                                    }
                                    ResultDiff::Delete { data } => {
                                        result_set.retain(|item| item != data);
                                    }
                                    ResultDiff::Update { before, after, .. } => {
                                        if let Some(pos) =
                                            result_set.iter().position(|item| item == before)
                                        {
                                            result_set[pos] = after.clone();
                                        } else {
                                            warn!("UPDATE: Could not find exact match for before state, treating as remove+add");
                                            result_set.retain(|item| item != before);
                                            result_set.push(after.clone());
                                        }
                                    }
                                    ResultDiff::Aggregation { .. } | ResultDiff::Noop => {}
                                }
                            }
                            drop(result_set);

                            profiling.query_send_ns = Some(crate::profiling::timestamp_ns());

                            let query_result = QueryResult::with_profiling(
                                query_id.clone(),
                                chrono::Utc::now(),
                                converted_results,
                                {
                                    let mut meta = HashMap::new();
                                    meta.insert(
                                        "source_id".to_string(),
                                        serde_json::Value::String(source_id.clone()),
                                    );
                                    meta.insert(
                                        "processed_by".to_string(),
                                        serde_json::Value::String("drasi-core".to_string()),
                                    );
                                    meta.insert(
                                        "result_count".to_string(),
                                        serde_json::Value::Number(results.len().into()),
                                    );
                                    meta
                                },
                                profiling,
                            );

                            debug!(
                                "Query '{}' sending {} results to reactions",
                                query_id,
                                results.len()
                            );

                            // Dispatch query result to all subscribed reactions
                            let arc_result = Arc::new(query_result);
                            let dispatchers = base_dispatchers.read().await;
                            for dispatcher in dispatchers.iter() {
                                if let Err(e) = dispatcher.dispatch_change(arc_result.clone()).await
                                {
                                    debug!("Failed to dispatch result for query '{query_id}': {e}");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!("Query '{query_id}' failed to process source change: {e}");
                    }
                }
            }

            info!("Query '{query_id}' processing task exited");
        }
        .instrument(span),
    );

        // Store the task handle
        *task_handle_clone.write().await = Some(handle);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log_component_stop("Query", &self.base.config.id);

        self.base
            .emit_status_event(ComponentStatus::Stopping, Some("Stopping query"))
            .await;

        // Stop the FutureQueueSource if it exists
        if let Some(future_queue_source) = self.future_queue_source.write().await.take() {
            debug!("Query '{}' stopping FutureQueueSource", self.base.config.id);
            if let Err(e) = future_queue_source.stop().await {
                error!(
                    "Query '{}' failed to stop FutureQueueSource: {}",
                    self.base.config.id, e
                );
            }
        }

        // Abort bootstrap tasks and supervisor
        let bootstrap_aborts: Vec<_> = {
            let mut handles = self.bootstrap_abort_handles.write().await;
            handles.drain(..).collect()
        };
        for handle in bootstrap_aborts {
            handle.abort();
        }

        // Drain and abort source subscription forwarders so they don't leak across restarts
        let subscription_handles: Vec<_> = {
            let mut tasks = self.subscription_tasks.write().await;
            tasks.drain(..).collect()
        };

        for handle in subscription_handles {
            handle.abort();
            let _ = handle.await;
        }

        // Use QueryBase common stop behavior to finish shutting down the processor task
        self.base.stop_common().await?;

        self.base
            .emit_status_event(ComponentStatus::Stopped, Some("Query stopped successfully"))
            .await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.status.read().await.clone()
    }

    fn get_config(&self) -> &QueryConfig {
        &self.base.config
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn subscribe(&self, reaction_id: String) -> Result<QuerySubscriptionResponse, String> {
        debug!(
            "Reaction '{}' subscribing to query '{}'",
            reaction_id, self.base.config.id
        );

        // Use QueryBase's subscribe method which returns QuerySubscriptionResponse
        self.base
            .subscribe(&reaction_id)
            .await
            .map_err(|e| format!("Failed to subscribe: {e}"))
    }
}

pub struct QueryManager {
    instance_id: String,
    queries: Arc<RwLock<HashMap<String, Arc<dyn Query>>>>,
    event_tx: ComponentEventSender,
    source_manager: Arc<SourceManager>,
    index_factory: Arc<crate::indexes::IndexFactory>,
    middleware_registry: Arc<MiddlewareTypeRegistry>,
    event_history: Arc<RwLock<ComponentEventHistory>>,
    log_registry: Arc<ComponentLogRegistry>,
}

impl QueryManager {
    pub fn new(
        instance_id: impl Into<String>,
        event_tx: ComponentEventSender,
        source_manager: Arc<SourceManager>,
        index_factory: Arc<crate::indexes::IndexFactory>,
        middleware_registry: Arc<MiddlewareTypeRegistry>,
        log_registry: Arc<ComponentLogRegistry>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            queries: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            source_manager,
            index_factory,
            middleware_registry,
            event_history: Arc::new(RwLock::new(ComponentEventHistory::new())),
            log_registry,
        }
    }

    pub async fn add_query(&self, config: QueryConfig) -> Result<()> {
        self.add_query_internal(config).await
    }

    pub async fn add_query_without_save(&self, config: QueryConfig) -> Result<()> {
        self.add_query_internal(config).await
    }

    /// Add a pre-created query instance (for testing)
    pub async fn add_query_instance_for_test(&self, query: Arc<dyn Query>) -> Result<()> {
        let query_id = query.get_config().id.clone();

        // Use a single write lock to atomically check and insert
        // This eliminates the TOCTOU race condition from separate read-then-write
        let mut queries = self.queries.write().await;
        if queries.contains_key(&query_id) {
            return Err(anyhow::anyhow!("Query with id '{query_id}' already exists"));
        }
        queries.insert(query_id, query);
        Ok(())
    }

    async fn add_query_internal(&self, config: QueryConfig) -> Result<()> {
        // Create the query instance first (before acquiring lock)
        let query = DrasiQuery::new(
            &self.instance_id,
            config.clone(),
            self.event_tx.clone(),
            self.source_manager.clone(),
            self.index_factory.clone(),
            self.middleware_registry.clone(),
        )?;

        let query: Arc<dyn Query> = Arc::new(query);

        let query_id = config.id.clone();
        let should_auto_start = config.auto_start;

        // Use a single write lock to atomically check and insert
        // This eliminates the TOCTOU race condition from separate read-then-write
        {
            let mut queries = self.queries.write().await;
            if queries.contains_key(&config.id) {
                return Err(anyhow::anyhow!(
                    "Query with id '{}' already exists",
                    config.id
                ));
            }
            queries.insert(config.id.clone(), query);
        }

        info!("Added query: {} with bootstrap support", config.id);

        // Note: Auto-start is handled by the caller (server.add_query)
        // which has access to the data router for subscriptions
        if should_auto_start {
            info!("Query '{query_id}' is configured for auto-start (will be started by caller)");
        }

        Ok(())
    }

    pub async fn start_query(&self, id: String) -> Result<()> {
        let query = {
            let queries = self.queries.read().await;
            queries.get(&id).cloned()
        };

        if let Some(query) = query {
            let status = query.status().await;
            is_operation_valid(&status, &Operation::Start).map_err(|e| anyhow::anyhow!(e))?;
            query.start().await?;
        } else {
            return Err(anyhow::anyhow!("Query not found: {id}"));
        }

        Ok(())
    }

    pub async fn stop_query(&self, id: String) -> Result<()> {
        let query = {
            let queries = self.queries.read().await;
            queries.get(&id).cloned()
        };

        if let Some(query) = query {
            let status = query.status().await;
            is_operation_valid(&status, &Operation::Stop).map_err(|e| anyhow::anyhow!(e))?;
            query.stop().await?;
        } else {
            return Err(anyhow::anyhow!("Query not found: {id}"));
        }

        Ok(())
    }

    pub async fn get_query_status(&self, id: String) -> Result<ComponentStatus> {
        let query = {
            let queries = self.queries.read().await;
            queries.get(&id).cloned()
        };

        if let Some(query) = query {
            Ok(query.status().await)
        } else {
            Err(anyhow::anyhow!("Query not found: {id}"))
        }
    }

    /// Get a query instance for subscription by reactions
    /// Returns Arc<dyn Query> which reactions can use to subscribe to query results
    pub async fn get_query_instance(&self, query_id: &str) -> Result<Arc<dyn Query>, String> {
        let queries = self.queries.read().await;
        if let Some(query) = queries.get(query_id) {
            Ok(Arc::clone(query))
        } else {
            Err(format!(
                "Query '{query_id}' not found. Available queries can be listed using list_queries()."
            ))
        }
    }

    pub async fn get_query(&self, id: String) -> Result<QueryRuntime> {
        let query = {
            let queries = self.queries.read().await;
            queries.get(&id).cloned()
        };

        if let Some(query) = query {
            let status = query.status().await;
            let config = query.get_config();
            let error_message = match &status {
                ComponentStatus::Error => self.event_history.read().await.get_last_error(&id),
                _ => None,
            };
            let runtime = QueryRuntime {
                id: config.id.clone(),
                query: config.query.clone(),
                status: status.clone(),
                error_message,
                source_subscriptions: config.sources.clone(),
                joins: config.joins.clone(),
            };
            Ok(runtime)
        } else {
            Err(anyhow::anyhow!("Query not found: {id}"))
        }
    }

    pub async fn update_query(&self, id: String, config: QueryConfig) -> Result<()> {
        let (query, was_running) = {
            let queries = self.queries.read().await;
            let query = queries.get(&id).cloned();
            if let Some(ref q) = query {
                let status = q.status().await;
                let was_running =
                    matches!(status, ComponentStatus::Running | ComponentStatus::Starting);
                (query, was_running)
            } else {
                return Err(anyhow::anyhow!("Query not found: {id}"));
            }
        };

        if let Some(query) = query {
            // If running, we need to stop it first
            if was_running {
                self.stop_query(id.clone()).await?;
                // Validate the operation after stop
                let status = query.status().await;
                is_operation_valid(&status, &Operation::Update).map_err(|e| anyhow::anyhow!(e))?;
            } else {
                let status = query.status().await;
                is_operation_valid(&status, &Operation::Update).map_err(|e| anyhow::anyhow!(e))?;
            }

            // For now, update means remove and re-add
            self.delete_query(id.clone()).await?;
            self.add_query(config).await?;

            // If it was running, restart it
            if was_running {
                // Query will subscribe directly to sources when started
                self.start_query(id).await?;
            }
        }

        Ok(())
    }

    pub async fn delete_query(&self, id: String) -> Result<()> {
        // First check if the query exists
        let query = {
            let queries = self.queries.read().await;
            queries.get(&id).cloned()
        };

        if let Some(query) = query {
            let status = query.status().await;

            // If the query is running or starting, stop it first
            if matches!(status, ComponentStatus::Running | ComponentStatus::Starting) {
                info!("Stopping query '{id}' before deletion");
                if let Err(e) = query.stop().await {
                    warn!(
                        "Failed to stop query '{id}' during deletion (may already be stopped): {e}"
                    );
                }

                // Wait a bit to ensure the query has fully stopped
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Verify it's stopped - accept Stopped or Error
                let new_status = query.status().await;
                if !matches!(
                    new_status,
                    ComponentStatus::Stopped | ComponentStatus::Error
                ) {
                    warn!("Query '{id}' in unexpected state {new_status:?} after stop, proceeding with deletion");
                }
            } else {
                // Still validate the operation for non-running states
                is_operation_valid(&status, &Operation::Delete).map_err(|e| anyhow::anyhow!(e))?;
            }

            // Now remove the query
            self.queries.write().await.remove(&id);
            // Clean up event history for this query
            self.event_history.write().await.remove_component(&id);
            // Clean up log resources for this query
            let log_key = ComponentLogKey::new(&self.instance_id, ComponentType::Query, &id);
            self.log_registry.remove_component_by_key(&log_key).await;
            info!("Deleted query: {id}");

            Ok(())
        } else {
            Err(anyhow::anyhow!("Query not found: {id}"))
        }
    }

    pub async fn list_queries(&self) -> Vec<(String, ComponentStatus)> {
        let queries_snapshot = {
            let queries = self.queries.read().await;
            queries
                .iter()
                .map(|(id, query)| (id.clone(), Arc::clone(query)))
                .collect::<Vec<_>>()
        };

        let mut result = Vec::new();
        for (id, query) in queries_snapshot {
            let status = query.status().await;
            result.push((id, status));
        }

        result
    }

    pub async fn get_query_config(&self, id: &str) -> Option<QueryConfig> {
        let queries = self.queries.read().await;
        queries.get(id).map(|q| q.get_config().clone())
    }

    pub async fn get_query_results(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        let query = {
            let queries = self.queries.read().await;
            queries.get(id).cloned()
        };

        if let Some(query) = query {
            // Check if the query is running
            let status = query.status().await;
            if status != ComponentStatus::Running {
                return Err(anyhow::anyhow!("Query '{id}' is not running"));
            }

            // Downcast to DrasiQuery to access get_current_results
            // Since all queries are DrasiQuery instances, this is safe
            let drasi_query = query
                .as_any()
                .downcast_ref::<DrasiQuery>()
                .ok_or_else(|| anyhow::anyhow!("Internal error: invalid query type"))?;

            Ok(drasi_query.get_current_results().await)
        } else {
            Err(anyhow::anyhow!("Query not found: {id}"))
        }
    }

    pub async fn start_all(&self) -> Result<()> {
        let queries_snapshot = {
            let queries = self.queries.read().await;
            queries
                .iter()
                .map(|(id, query)| (id.clone(), Arc::clone(query)))
                .collect::<Vec<_>>()
        };

        let mut failed_queries = Vec::new();

        for (id, query) in queries_snapshot {
            let config = query.get_config();
            if config.auto_start {
                info!("Auto-starting query: {id}");
                if let Err(e) = query.start().await {
                    error!("Failed to start query {id}: {e}");
                    failed_queries.push((id.clone(), e.to_string()));
                    // Continue starting other queries instead of returning early
                }
            } else {
                info!("Query '{id}' has auto_start=false, skipping automatic startup");
            }
        }

        // Return error only if any queries failed to start
        if !failed_queries.is_empty() {
            let error_msg = failed_queries
                .iter()
                .map(|(id, err)| format!("{id}: {err}"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!("Failed to start some queries: {error_msg}"))
        } else {
            Ok(())
        }
    }

    pub async fn stop_all(&self) -> Result<()> {
        let queries = self.queries.read().await;
        for query in queries.values() {
            if let Err(e) = query.stop().await {
                log_component_error("Query", &query.get_config().id, &e.to_string());
            }
        }
        Ok(())
    }

    /// Record a component event in the history.
    ///
    /// This should be called by the event processing loop to track component
    /// lifecycle events for later querying.
    pub async fn record_event(&self, event: ComponentEvent) {
        self.event_history.write().await.record_event(event);
    }

    /// Get events for a specific query.
    ///
    /// Returns events in chronological order (oldest first).
    pub async fn get_query_events(&self, id: &str) -> Vec<ComponentEvent> {
        self.event_history.read().await.get_events(id)
    }

    /// Get all events across all queries.
    ///
    /// Returns events sorted by timestamp (oldest first).
    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        self.event_history.read().await.get_all_events()
    }

    /// Subscribe to live logs for a query.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// Returns None if the query doesn't exist.
    pub async fn subscribe_logs(
        &self,
        id: &str,
    ) -> Option<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        // Verify the query exists
        {
            let queries = self.queries.read().await;
            if !queries.contains_key(id) {
                return None;
            }
        }

        let log_key = ComponentLogKey::new(&self.instance_id, ComponentType::Query, id);
        Some(self.log_registry.subscribe_by_key(&log_key).await)
    }

    /// Subscribe to live events for a query.
    ///
    /// Returns the event history and a broadcast receiver for new events.
    /// Returns None if the query doesn't exist.
    pub async fn subscribe_events(
        &self,
        id: &str,
    ) -> Option<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        // Verify the query exists
        {
            let queries = self.queries.read().await;
            if !queries.contains_key(id) {
                return None;
            }
        }

        Some(self.event_history.write().await.subscribe(id))
    }
}
