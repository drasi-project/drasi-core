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

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, warn};
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

// Import drasi-core components
use drasi_core::{
    evaluation::context::{QueryPartEvaluationContext, QueryVariables},
    evaluation::functions::FunctionRegistry,
    evaluation::variable_value::VariableValue,
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_functions_gql::GQLFunctionSet;
use drasi_query_ast::api::{QueryConfiguration, QueryParser};
use drasi_query_cypher::CypherParser;
use drasi_query_gql::GQLParser;

use crate::channels::*;
use crate::config::{QueryConfig, QueryLanguage, QueryRuntime};
use crate::routers::DataRouter;
use crate::utils::*;

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

/// Convert QueryVariables (BTreeMap<Box<str>, VariableValue>) to JSON
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
        _ => serde_json::Value::String(format!("{:?}", value)),
    }
}

#[async_trait]
pub trait Query: Send + Sync {
    async fn start(&self, source_event_rx: SourceEventReceiver) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
    fn get_config(&self) -> &QueryConfig;
    fn as_any(&self) -> &dyn std::any::Any;
}

pub struct DrasiQuery {
    config: QueryConfig,
    status: Arc<RwLock<ComponentStatus>>,
    result_tx: QueryResultSender,
    event_tx: ComponentEventSender,
    bootstrap_request_tx: Option<BootstrapRequestSender>,
    bootstrap_response_rx: Option<Arc<tokio::sync::Mutex<BootstrapResponseReceiver>>>,
    #[allow(dead_code)]
    continuous_query: Option<ContinuousQuery>,
    current_results: Arc<RwLock<Vec<serde_json::Value>>>,
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl DrasiQuery {
    pub fn new(
        config: QueryConfig,
        result_tx: QueryResultSender,
        event_tx: ComponentEventSender,
    ) -> Self {
        Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            result_tx,
            event_tx,
            bootstrap_request_tx: None,
            bootstrap_response_rx: None,
            continuous_query: None,
            current_results: Arc::new(RwLock::new(Vec::new())),
            task_handle: Arc::new(RwLock::new(None)),
        }
    }

    pub fn with_bootstrap_channels(
        mut self,
        request_tx: BootstrapRequestSender,
        response_rx: BootstrapResponseReceiver,
    ) -> Self {
        self.bootstrap_request_tx = Some(request_tx);
        self.bootstrap_response_rx = Some(Arc::new(tokio::sync::Mutex::new(response_rx)));
        self
    }

    pub async fn get_current_results(&self) -> Vec<serde_json::Value> {
        self.current_results.read().await.clone()
    }
}

#[async_trait]
impl Query for DrasiQuery {
    async fn start(&self, mut source_event_rx: SourceEventReceiver) -> Result<()> {
        log_component_start("Query", &self.config.id);

        *self.status.write().await = ComponentStatus::Starting;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Query,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting query".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        *self.status.write().await = ComponentStatus::Running;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Query,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: Some("Query started successfully".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Build and initialize the actual Drasi ContinuousQuery
        let query_str = self.config.query.clone();

        // Create a parser and function registry based on the query language
        let config = Arc::new(DefaultQueryConfig);
        let (parser, function_registry): (Arc<dyn QueryParser>, Arc<FunctionRegistry>) =
            match self.config.query_language {
                QueryLanguage::Cypher => {
                    debug!(
                        "Query '{}' using Cypher parser and function set",
                        self.config.id
                    );
                    (
                        Arc::new(CypherParser::new(config)),
                        Arc::new(FunctionRegistry::new()).with_cypher_function_set(),
                    )
                }
                QueryLanguage::GQL => {
                    debug!(
                        "Query '{}' using GQL parser and function set",
                        self.config.id
                    );
                    (
                        Arc::new(GQLParser::new(config)),
                        Arc::new(FunctionRegistry::new()).with_gql_function_set(),
                    )
                }
            };

        let mut builder =
            QueryBuilder::new(&query_str, parser).with_function_registry(function_registry);

        // Add joins if configured
        if let Some(joins) = &self.config.joins {
            debug!(
                "Query '{}' has {} configured joins",
                self.config.id,
                joins.len()
            );
            let drasi_joins: Vec<drasi_core::models::QueryJoin> =
                joins.iter().cloned().map(|j| j.into()).collect();
            builder = builder.with_joins(drasi_joins);
        }

        let continuous_query = match builder.try_build().await {
            Ok(query) => query,
            Err(e) => {
                error!("Failed to build query '{}': {}", self.config.id, e);
                *self.status.write().await = ComponentStatus::Error;

                let event = ComponentEvent {
                    component_id: self.config.id.clone(),
                    component_type: ComponentType::Query,
                    status: ComponentStatus::Error,
                    timestamp: chrono::Utc::now(),
                    message: Some(format!("Failed to build query: {}", e)),
                };

                if let Err(e) = self.event_tx.send(event).await {
                    error!("Failed to send component event: {}", e);
                }

                return Err(anyhow::anyhow!("Failed to build query: {}", e));
            }
        };

        // Extract labels from the query for bootstrap
        let labels = match crate::queries::LabelExtractor::extract_labels(
            &query_str,
            &self.config.query_language,
        ) {
            Ok(labels) => labels,
            Err(e) => {
                warn!("Failed to extract labels from query '{}': {}. Bootstrap will request all data.", 
                    self.config.id, e);
                crate::queries::QueryLabels {
                    node_labels: vec![],
                    relation_labels: vec![],
                }
            }
        };

        // Send bootstrap requests if bootstrap channels are configured
        if let (Some(request_tx), Some(response_rx)) =
            (&self.bootstrap_request_tx, &self.bootstrap_response_rx)
        {
            info!(
                "Sending bootstrap request for query '{}' with labels: nodes={:?}, relations={:?}",
                self.config.id, labels.node_labels, labels.relation_labels
            );

            let bootstrap_request = BootstrapRequest {
                query_id: self.config.id.clone(),
                node_labels: labels.node_labels,
                relation_labels: labels.relation_labels,
                request_id: uuid::Uuid::new_v4().to_string(),
            };

            if let Err(e) = request_tx.send(bootstrap_request).await {
                error!(
                    "Failed to send bootstrap request for query '{}': {}",
                    self.config.id, e
                );
            } else {
                // Wait for bootstrap completion
                let response_rx = response_rx.clone();
                tokio::spawn(async move {
                    let mut rx = response_rx.lock().await;
                    while let Some(response) = rx.recv().await {
                        match &response.status {
                            BootstrapStatus::Started => {
                                info!(
                                    "Bootstrap started for query: {}",
                                    response.message.as_ref().unwrap_or(&"".to_string())
                                );
                            }
                            BootstrapStatus::InProgress { count } => {
                                debug!("Bootstrap in progress: {} elements processed", count);
                            }
                            BootstrapStatus::Completed { total_count } => {
                                info!("Bootstrap completed: {} total elements", total_count);
                                break;
                            }
                            BootstrapStatus::Failed { error } => {
                                error!("Bootstrap failed: {}", error);
                                break;
                            }
                        }
                    }
                });
            }
        }

        // Store the continuous query for processing
        let continuous_query = Arc::new(continuous_query);
        let result_tx = self.result_tx.clone();
        let query_id = self.config.id.clone();
        let source_filter = self.config.sources.clone();
        let current_results = self.current_results.clone();
        let task_handle_clone = self.task_handle.clone();

        let handle = tokio::spawn(async move {
            info!("Starting Drasi continuous query processing: {}", query_str);

            while let Some(source_event_wrapper) = source_event_rx.recv().await {
                // Filter events based on configured sources
                if !source_filter.contains(&source_event_wrapper.source_id) {
                    debug!(
                        "Query '{}' ignoring event from source '{}' (not in filter: {:?})",
                        query_id, source_event_wrapper.source_id, source_filter
                    );
                    continue;
                }

                // Extract the SourceChange from the SourceEvent
                // Skip control messages and bootstrap markers - queries only process data changes
                let source_change = match &source_event_wrapper.event {
                    SourceEvent::Change(change) => change.clone(),
                    SourceEvent::Control(_) => {
                        debug!(
                            "Query '{}' ignoring control event from source '{}'",
                            query_id, source_event_wrapper.source_id
                        );
                        continue;
                    }
                    SourceEvent::BootstrapStart { query_id: marker_query_id } => {
                        debug!(
                            "Query '{}' received bootstrap start marker for query '{}'",
                            query_id, marker_query_id
                        );
                        continue;
                    }
                    SourceEvent::BootstrapEnd { query_id: marker_query_id } => {
                        debug!(
                            "Query '{}' received bootstrap end marker for query '{}'",
                            query_id, marker_query_id
                        );
                        continue;
                    }
                };

                debug!(
                    "Query '{}' processing change from source '{}'",
                    query_id, source_event_wrapper.source_id
                );

                // Extract profiling metadata from source event or create new
                let mut profiling = source_event_wrapper
                    .profiling
                    .unwrap_or_else(|| crate::profiling::ProfilingMetadata::new());

                // Capture query_receive_ns timestamp
                profiling.query_receive_ns = Some(crate::profiling::timestamp_ns());

                // Process the change through the actual Drasi continuous query
                // Capture query_core_call_ns before calling drasi-core
                profiling.query_core_call_ns = Some(crate::profiling::timestamp_ns());

                match continuous_query.process_source_change(source_change).await {
                    Ok(results) => {
                        // Capture query_core_return_ns after drasi-core returns
                        profiling.query_core_return_ns = Some(crate::profiling::timestamp_ns());
                        if !results.is_empty() {
                            debug!(
                                "Query '{}' received {} results from drasi-core",
                                query_id,
                                results.len()
                            );
                            for (i, r) in results.iter().enumerate() {
                                debug!("  Result {}: {:?}", i, r);
                            }

                            // Convert Drasi results to our QueryResult format
                            let converted_results: Vec<serde_json::Value> = results
                                .iter()
                                .map(|ctx| match ctx {
                                    QueryPartEvaluationContext::Adding { after } => {
                                        debug!("Query '{}' got Adding context", query_id);
                                        serde_json::json!({
                                            "type": "ADD",
                                            "data": convert_query_variables_to_json(after)
                                        })
                                    },
                                    QueryPartEvaluationContext::Removing { before } => {
                                        warn!("Query '{}' got Removing context for UPDATE source event", query_id);
                                        serde_json::json!({
                                            "type": "DELETE", 
                                            "data": convert_query_variables_to_json(before)
                                        })
                                    },
                                    QueryPartEvaluationContext::Updating { before, after } => {
                                        debug!("Query '{}' got Updating context", query_id);
                                        serde_json::json!({
                                            "type": "UPDATE",
                                            "data": convert_query_variables_to_json(after),
                                            "before": convert_query_variables_to_json(before),
                                            "after": convert_query_variables_to_json(after)
                                        })
                                    },
                                    QueryPartEvaluationContext::Aggregation { before, after, .. } => {
                                        serde_json::json!({
                                            "type": "aggregation",
                                            "before": before.as_ref().map(convert_query_variables_to_json),
                                            "after": convert_query_variables_to_json(after)
                                        })
                                    },
                                    QueryPartEvaluationContext::Noop => {
                                        serde_json::json!({
                                            "type": "noop"
                                        })
                                    }
                                })
                                .collect();

                            // Update the current result set based on the changes
                            let mut result_set = current_results.write().await;
                            for result in &converted_results {
                                if let Some(change_type) =
                                    result.get("type").and_then(|t| t.as_str())
                                {
                                    match change_type {
                                        "ADD" => {
                                            if let Some(data) = result.get("data") {
                                                result_set.push(data.clone());
                                            }
                                        }
                                        "DELETE" => {
                                            if let Some(data) = result.get("data") {
                                                result_set.retain(|item| item != data);
                                            }
                                        }
                                        "UPDATE" => {
                                            if let (Some(before), Some(after)) =
                                                (result.get("before"), result.get("after"))
                                            {
                                                // Try to match by exact equality first
                                                if let Some(pos) = result_set
                                                    .iter()
                                                    .position(|item| item == before)
                                                {
                                                    result_set[pos] = after.clone();
                                                } else {
                                                    // If exact match fails, this might be a projection query
                                                    // Just add the new value and remove any old one with same key
                                                    // For now, treat it as remove old + add new
                                                    warn!("UPDATE: Could not find exact match for before state, treating as remove+add");
                                                    result_set.retain(|item| item != before);
                                                    result_set.push(after.clone());
                                                }
                                            } else if let Some(data) = result.get("data") {
                                                // Simplified UPDATE without before/after - just update in place
                                                // This shouldn't happen but handle it gracefully
                                                warn!(
                                                    "UPDATE without before/after, adding new data"
                                                );
                                                result_set.push(data.clone());
                                            }
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            drop(result_set);

                            // Capture query_send_ns before sending to reactions
                            profiling.query_send_ns = Some(crate::profiling::timestamp_ns());

                            let query_result = QueryResult::with_profiling(
                                query_id.clone(),
                                chrono::Utc::now(),
                                converted_results,
                                {
                                    let mut meta = HashMap::new();
                                    meta.insert(
                                        "source_id".to_string(),
                                        serde_json::Value::String(
                                            source_event_wrapper.source_id.clone(),
                                        ),
                                    );
                                    meta.insert(
                                        "query".to_string(),
                                        serde_json::Value::String(query_str.clone()),
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

                            if let Err(e) = result_tx.send(query_result).await {
                                error!("Failed to send query result: {}", e);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        error!(
                            "Query '{}' failed to process source change: {}",
                            query_id, e
                        );
                    }
                }
            }

            info!("Query '{}' processing task exited", query_id);
        });

        // Store the task handle
        *task_handle_clone.write().await = Some(handle);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log_component_stop("Query", &self.config.id);

        *self.status.write().await = ComponentStatus::Stopping;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Query,
            status: ComponentStatus::Stopping,
            timestamp: chrono::Utc::now(),
            message: Some("Stopping query".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Abort the processing task if it exists
        if let Some(handle) = self.task_handle.write().await.take() {
            handle.abort();
            // Wait for the task to finish (it will be cancelled)
            let _ = handle.await;
        }

        *self.status.write().await = ComponentStatus::Stopped;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Query,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: Some("Query stopped successfully".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &QueryConfig {
        &self.config
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

pub struct QueryManager {
    queries: Arc<RwLock<HashMap<String, Arc<dyn Query>>>>,
    result_tx: QueryResultSender,
    event_tx: ComponentEventSender,
    bootstrap_request_tx: BootstrapRequestSender,
    bootstrap_response_senders: Arc<RwLock<HashMap<String, BootstrapResponseSender>>>,
}

impl QueryManager {
    pub fn new(
        result_tx: QueryResultSender,
        event_tx: ComponentEventSender,
        bootstrap_request_tx: BootstrapRequestSender,
    ) -> Self {
        Self {
            queries: Arc::new(RwLock::new(HashMap::new())),
            result_tx,
            event_tx,
            bootstrap_request_tx,
            bootstrap_response_senders: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn get_bootstrap_response_senders(&self) -> HashMap<String, BootstrapResponseSender> {
        self.bootstrap_response_senders.read().await.clone()
    }

    pub async fn add_query(&self, config: QueryConfig) -> Result<()> {
        self.add_query_internal(config).await
    }

    pub async fn add_query_without_save(&self, config: QueryConfig) -> Result<()> {
        self.add_query_internal(config).await
    }

    async fn add_query_internal(&self, config: QueryConfig) -> Result<()> {
        // Check if query with this id already exists
        if self.queries.read().await.contains_key(&config.id) {
            return Err(anyhow::anyhow!(
                "Query with id '{}' already exists",
                config.id
            ));
        }

        // Create a dedicated bootstrap response channel for this query
        let (bootstrap_resp_tx, bootstrap_resp_rx) = tokio::sync::mpsc::channel(100);

        // Store the response sender so bootstrap router can send responses
        self.bootstrap_response_senders
            .write()
            .await
            .insert(config.id.clone(), bootstrap_resp_tx);

        let query = DrasiQuery::new(
            config.clone(),
            self.result_tx.clone(),
            self.event_tx.clone(),
        )
        .with_bootstrap_channels(self.bootstrap_request_tx.clone(), bootstrap_resp_rx);

        let query: Arc<dyn Query> = Arc::new(query);

        let query_id = config.id.clone();
        let should_auto_start = config.auto_start;

        self.queries.write().await.insert(config.id.clone(), query);
        info!("Added query: {} with bootstrap support", config.id);

        // Note: Auto-start is handled by the caller (server.create_query)
        // which has access to the data router for subscriptions
        if should_auto_start {
            info!(
                "Query '{}' is configured for auto-start (will be started by caller)",
                query_id
            );
        }

        Ok(())
    }

    pub async fn start_query(
        &self,
        id: String,
        source_event_rx: SourceEventReceiver,
    ) -> Result<()> {
        let queries = self.queries.read().await;
        if let Some(query) = queries.get(&id) {
            let status = query.status().await;
            is_operation_valid(&status, &Operation::Start).map_err(|e| anyhow::anyhow!(e))?;
            query.start(source_event_rx).await?;
        } else {
            return Err(anyhow::anyhow!("Query not found: {}", id));
        }

        Ok(())
    }

    pub async fn stop_query(&self, id: String) -> Result<()> {
        let queries = self.queries.read().await;
        if let Some(query) = queries.get(&id) {
            let status = query.status().await;
            is_operation_valid(&status, &Operation::Stop).map_err(|e| anyhow::anyhow!(e))?;
            query.stop().await?;
        } else {
            return Err(anyhow::anyhow!("Query not found: {}", id));
        }

        Ok(())
    }

    pub async fn get_query_status(&self, id: String) -> Result<ComponentStatus> {
        let queries = self.queries.read().await;
        if let Some(query) = queries.get(&id) {
            Ok(query.status().await)
        } else {
            Err(anyhow::anyhow!("Query not found: {}", id))
        }
    }

    pub async fn get_query(&self, id: String) -> Result<QueryRuntime> {
        let queries = self.queries.read().await;
        if let Some(query) = queries.get(&id) {
            let status = query.status().await;
            let config = query.get_config();
            let runtime = QueryRuntime {
                id: config.id.clone(),
                query: config.query.clone(),
                status: status.clone(),
                error_message: match &status {
                    ComponentStatus::Error => Some("Query error occurred".to_string()),
                    _ => None,
                },
                sources: config.sources.clone(),
                properties: config.properties.clone(),
                joins: config.joins.clone(),
            };
            Ok(runtime)
        } else {
            Err(anyhow::anyhow!("Query not found: {}", id))
        }
    }

    pub async fn update_query(&self, id: String, config: QueryConfig) -> Result<()> {
        let queries = self.queries.read().await;
        if let Some(query) = queries.get(&id) {
            let status = query.status().await;
            let was_running =
                matches!(status, ComponentStatus::Running | ComponentStatus::Starting);

            // If running, we need to stop it first
            if was_running {
                drop(queries);
                self.stop_query(id.clone()).await?;
                // Re-acquire lock after stop
                let queries = self.queries.read().await;
                if let Some(query) = queries.get(&id) {
                    let status = query.status().await;
                    is_operation_valid(&status, &Operation::Update)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                drop(queries);
            } else {
                is_operation_valid(&status, &Operation::Update).map_err(|e| anyhow::anyhow!(e))?;
                drop(queries);
            }

            // For now, update means remove and re-add
            self.delete_query(id.clone()).await?;
            self.add_query(config).await?;

            // If it was running, restart it
            if was_running {
                // Need to get a receiver for the query
                let (_tx, rx) = tokio::sync::mpsc::channel(100);
                self.start_query(id, rx).await?;
            }
        } else {
            return Err(anyhow::anyhow!("Query not found: {}", id));
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

            // If the query is running, stop it first
            if matches!(status, ComponentStatus::Running) {
                info!("Stopping query '{}' before deletion", id);
                query.stop().await?;

                // Wait a bit to ensure the query has fully stopped
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Verify it's stopped
                let new_status = query.status().await;
                if !matches!(new_status, ComponentStatus::Stopped) {
                    return Err(anyhow::anyhow!(
                        "Failed to stop query '{}' before deletion",
                        id
                    ));
                }
            } else {
                // Still validate the operation for non-running states
                is_operation_valid(&status, &Operation::Delete).map_err(|e| anyhow::anyhow!(e))?;
            }

            // Now remove the query
            self.queries.write().await.remove(&id);
            info!("Deleted query: {}", id);

            Ok(())
        } else {
            Err(anyhow::anyhow!("Query not found: {}", id))
        }
    }

    pub async fn list_queries(&self) -> Vec<(String, ComponentStatus)> {
        let queries = self.queries.read().await;
        let mut result = Vec::new();

        for (id, query) in queries.iter() {
            let status = query.status().await;
            result.push((id.clone(), status));
        }

        result
    }

    pub async fn get_query_config(&self, id: &str) -> Option<QueryConfig> {
        let queries = self.queries.read().await;
        queries.get(id).map(|q| q.get_config().clone())
    }

    pub async fn get_query_results(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        let queries = self.queries.read().await;
        if let Some(query) = queries.get(id) {
            // Check if the query is running
            let status = query.status().await;
            if status != ComponentStatus::Running {
                return Err(anyhow::anyhow!("Query '{}' is not running", id));
            }

            // Downcast to DrasiQuery to access get_current_results
            // Since all queries are DrasiQuery instances, this is safe
            let drasi_query = query
                .as_any()
                .downcast_ref::<DrasiQuery>()
                .ok_or_else(|| anyhow::anyhow!("Internal error: invalid query type"))?;

            Ok(drasi_query.get_current_results().await)
        } else {
            Err(anyhow::anyhow!("Query not found: {}", id))
        }
    }

    pub async fn start_all(&self, data_router: &DataRouter) -> Result<()> {
        let queries = self.queries.read().await;
        let mut failed_queries = Vec::new();

        for (id, query) in queries.iter() {
            let config = query.get_config();
            if config.auto_start {
                // Get a receiver connected to the data router
                let enable_bootstrap = config.enable_bootstrap;
                let rx = data_router
                    .add_query_subscription(id.clone(), config.sources.clone(), enable_bootstrap)
                    .await
                    .map_err(|e| anyhow::anyhow!("Failed to add query subscription: {}", e))?;
                info!("Auto-starting query: {} (bootstrap: {})", id, enable_bootstrap);
                if let Err(e) = query.start(rx).await {
                    error!("Failed to start query {}: {}", id, e);
                    failed_queries.push((id.clone(), e.to_string()));
                    // Continue starting other queries instead of returning early
                }
            } else {
                info!(
                    "Query '{}' has auto_start=false, skipping automatic startup",
                    id
                );
            }
        }

        // Return error only if any queries failed to start
        if !failed_queries.is_empty() {
            let error_msg = failed_queries
                .iter()
                .map(|(id, err)| format!("{}: {}", id, err))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "Failed to start some queries: {}",
                error_msg
            ))
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
}
