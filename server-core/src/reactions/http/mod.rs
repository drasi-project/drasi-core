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
use handlebars::Handlebars;
use log::{debug, error, info, warn};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, Method,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::RwLock;

use crate::channels::priority_queue::PriorityQueue;
use crate::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, QueryResult,
};
use crate::config::ReactionConfig;
use crate::server_core::DrasiServerCore;
use crate::utils::log_component_start;

use crate::reactions::Reaction;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSpec {
    pub url: String,
    pub method: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<CallSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<CallSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<CallSpec>,
}

pub struct HttpReaction {
    config: ReactionConfig,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: ComponentEventSender,
    base_url: String,
    token: Option<String>,
    timeout_ms: u64,
    query_configs: HashMap<String, QueryConfig>,
    subscription_tasks: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
    priority_queue: PriorityQueue<QueryResult>,
    processing_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl HttpReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        // Extract HTTP-specific configuration
        let base_url = config
            .properties
            .get("base_url")
            .and_then(|v| v.as_str())
            .unwrap_or("http://localhost")
            .to_string();

        let token = config
            .properties
            .get("token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let timeout_ms = config
            .properties
            .get("timeout_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(10000);

        // Parse query configurations
        let mut query_configs = HashMap::new();
        if let Some(queries_value) = config.properties.get("queries") {
            if let Some(queries_obj) = queries_value.as_object() {
                for (query_name, query_value) in queries_obj {
                    if let Ok(query_config) =
                        serde_json::from_value::<QueryConfig>(query_value.clone())
                    {
                        query_configs.insert(query_name.clone(), query_config);
                    } else {
                        warn!("Failed to parse query config for query: {}", query_name);
                    }
                }
            }
        }

        Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
            base_url,
            token,
            timeout_ms,
            query_configs,
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            priority_queue: PriorityQueue::new(10000),
            processing_task: Arc::new(RwLock::new(None)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn process_result(
        client: &Client,
        handlebars: &Handlebars<'static>,
        base_url: &str,
        token: &Option<String>,
        call_spec: &CallSpec,
        result_type: &str,
        data: &Value,
        query_name: &str,
        reaction_name: &str,
    ) -> Result<()> {
        // Prepare context for Handlebars templates
        let mut context = Map::new();

        match result_type {
            "ADD" => {
                context.insert("after".to_string(), data.clone());
            }
            "UPDATE" => {
                // For updates, we receive the full result object with before/after/data fields
                if let Some(obj) = data.as_object() {
                    if let Some(before) = obj.get("before") {
                        context.insert("before".to_string(), before.clone());
                    }
                    if let Some(after) = obj.get("after") {
                        context.insert("after".to_string(), after.clone());
                    }
                    if let Some(data_field) = obj.get("data") {
                        context.insert("data".to_string(), data_field.clone());
                    }
                } else {
                    context.insert("after".to_string(), data.clone());
                }
            }
            "DELETE" => {
                context.insert("before".to_string(), data.clone());
            }
            _ => {
                context.insert("data".to_string(), data.clone());
            }
        }

        // Add query metadata
        context.insert(
            "query_name".to_string(),
            Value::String(query_name.to_string()),
        );
        context.insert(
            "operation".to_string(),
            Value::String(result_type.to_string()),
        );

        // Render URL
        let url = handlebars.render_template(&call_spec.url, &context)?;
        let full_url = if url.starts_with("http://") || url.starts_with("https://") {
            url
        } else {
            format!("{}{}", base_url, url)
        };

        // Render body
        let body = if !call_spec.body.is_empty() {
            debug!(
                "[{}] Rendering template: {} with context: {:?}",
                reaction_name, call_spec.body, context
            );
            let rendered = handlebars.render_template(&call_spec.body, &context)?;
            debug!("[{}] Rendered body: {}", reaction_name, rendered);
            rendered
        } else {
            serde_json::to_string(&data)?
        };

        // Build headers
        let mut headers = HeaderMap::new();
        headers.insert("Content-Type", HeaderValue::from_static("application/json"));

        if let Some(token) = token {
            headers.insert(
                "Authorization",
                HeaderValue::from_str(&format!("Bearer {}", token))?,
            );
        }

        for (key, value) in &call_spec.headers {
            let header_name = HeaderName::from_bytes(key.as_bytes())?;
            let header_value =
                HeaderValue::from_str(&handlebars.render_template(value, &context)?)?;
            headers.insert(header_name, header_value);
        }

        // Parse method
        let method = match call_spec.method.to_uppercase().as_str() {
            "GET" => Method::GET,
            "POST" => Method::POST,
            "PUT" => Method::PUT,
            "DELETE" => Method::DELETE,
            "PATCH" => Method::PATCH,
            _ => Method::POST,
        };

        // Make HTTP request
        debug!(
            "[{}] Sending {} request to {} with body: {}",
            reaction_name, method, full_url, body
        );

        let response = client
            .request(method.clone(), &full_url)
            .headers(headers)
            .body(body.clone())
            .send()
            .await?;

        let status = response.status();
        debug!(
            "[{}] HTTP {} {} - Status: {}",
            reaction_name,
            call_spec.method.to_uppercase(),
            full_url,
            status.as_u16()
        );

        if !status.is_success() {
            let error_body = response
                .text()
                .await
                .unwrap_or_else(|_| "Unable to read response body".to_string());
            warn!(
                "[{}] HTTP request failed with status {}: {}",
                reaction_name,
                status.as_u16(),
                error_body
            );
        }

        Ok(())
    }
}

#[async_trait]
impl Reaction for HttpReaction {
    async fn start(&self, server_core: Arc<DrasiServerCore>) -> Result<()> {
        log_component_start("HTTP Reaction", &self.config.id);

        info!(
            "[{}] HTTP reaction started - sending to base URL: {}",
            self.config.id, self.base_url
        );

        *self.status.write().await = ComponentStatus::Starting;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting HTTP reaction".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Get QueryManager from server_core
        let query_manager = server_core.query_manager();

        // Subscribe to each query and spawn forwarder tasks
        let mut subscription_tasks = self.subscription_tasks.write().await;
        for query_id in &self.config.queries {
            info!("[{}] Subscribing to query: {}", self.config.id, query_id);

            // Get the query instance
            let query = match query_manager.get_query_instance(query_id).await {
                Ok(q) => q,
                Err(e) => {
                    error!(
                        "[{}] Failed to get query instance for '{}': {}",
                        self.config.id, query_id, e
                    );
                    continue;
                }
            };

            // Subscribe to the query
            let subscription_response = match query.subscribe(self.config.id.clone()).await {
                Ok(response) => response,
                Err(e) => {
                    error!(
                        "[{}] Failed to subscribe to query '{}': {}",
                        self.config.id, query_id, e
                    );
                    continue;
                }
            };

            let mut broadcast_receiver = subscription_response.broadcast_receiver;
            let priority_queue = self.priority_queue.clone();
            let reaction_id = self.config.id.clone();
            let query_id_clone = query_id.clone();

            // Spawn forwarder task
            let forwarder_task = tokio::spawn(async move {
                info!(
                    "[{}] Forwarder task started for query '{}'",
                    reaction_id, query_id_clone
                );

                loop {
                    match broadcast_receiver.recv().await {
                        Ok(query_result) => {
                            // Enqueue to priority queue
                            if !priority_queue.enqueue(query_result).await {
                                warn!(
                                    "[{}] Priority queue full, dropped result from query '{}'",
                                    reaction_id, query_id_clone
                                );
                            }
                        }
                        Err(RecvError::Lagged(count)) => {
                            warn!(
                                "[{}] Broadcast receiver lagged by {} messages for query '{}'",
                                reaction_id, count, query_id_clone
                            );
                            // Continue processing
                        }
                        Err(RecvError::Closed) => {
                            info!(
                                "[{}] Broadcast channel closed for query '{}'",
                                reaction_id, query_id_clone
                            );
                            break;
                        }
                    }
                }

                info!(
                    "[{}] Forwarder task stopped for query '{}'",
                    reaction_id, query_id_clone
                );
            });

            subscription_tasks.push(forwarder_task);
        }
        drop(subscription_tasks);

        *self.status.write().await = ComponentStatus::Running;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: Some("HTTP reaction started".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Spawn the main processing task
        let reaction_name = self.config.id.clone();
        let status = Arc::clone(&self.status);
        let query_configs = self.query_configs.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        let timeout_ms = self.timeout_ms;
        let priority_queue = self.priority_queue.clone();

        let processing_task_handle = tokio::spawn(async move {
            let client = match Client::builder()
                .timeout(std::time::Duration::from_millis(timeout_ms))
                .build()
            {
                Ok(c) => c,
                Err(e) => {
                    error!("[{}] Failed to create HTTP client: {}", reaction_name, e);
                    return;
                }
            };

            let mut handlebars = Handlebars::new();

            // Register the json helper to serialize values as JSON
            handlebars.register_helper(
                "json",
                Box::new(
                    |h: &handlebars::Helper,
                     _: &Handlebars,
                     _: &handlebars::Context,
                     _: &mut handlebars::RenderContext,
                     out: &mut dyn handlebars::Output|
                     -> handlebars::HelperResult {
                        if let Some(value) = h.param(0) {
                            let json_str = serde_json::to_string(&value.value())
                                .unwrap_or_else(|_| "null".to_string());
                            out.write(&json_str)?;
                        }
                        Ok(())
                    },
                ),
            );

            loop {
                // Dequeue results from priority queue in timestamp order
                let query_result_arc = priority_queue.dequeue().await;
                let query_result = query_result_arc.as_ref();

                if !matches!(*status.read().await, ComponentStatus::Running) {
                    break;
                }

                if query_result.results.is_empty() {
                    debug!("[{}] Received empty result set from query", reaction_name);
                    continue;
                }

                let query_name = &query_result.query_id;

                // Check if we have configuration for this query
                let query_config = query_configs.get(query_name).or_else(|| {
                    // Try without source prefix if format is "source.query"
                    query_name
                        .split('.')
                        .last()
                        .and_then(|name| query_configs.get(name))
                });

                let default_config;
                let query_config = match query_config {
                    Some(config) => config,
                    None => {
                        debug!(
                            "[{}] No configuration for query '{}', using default",
                            reaction_name, query_name
                        );
                        // Create a default configuration that POSTs all changes
                        default_config = QueryConfig {
                            added: Some(CallSpec {
                                url: format!("/changes/{}", query_name),
                                method: "POST".to_string(),
                                body: String::new(),
                                headers: HashMap::new(),
                            }),
                            updated: Some(CallSpec {
                                url: format!("/changes/{}", query_name),
                                method: "POST".to_string(),
                                body: String::new(),
                                headers: HashMap::new(),
                            }),
                            deleted: Some(CallSpec {
                                url: format!("/changes/{}", query_name),
                                method: "POST".to_string(),
                                body: String::new(),
                                headers: HashMap::new(),
                            }),
                        };
                        &default_config
                    }
                };

                debug!(
                    "[{}] Processing {} results from query '{}'",
                    reaction_name,
                    query_result.results.len(),
                    query_name
                );

                // Process each result
                for result in &query_result.results {
                    if let Some(result_type) = result.get("type").and_then(|v| v.as_str()) {
                        // For UPDATE, we need the whole result object to access before/after
                        // For ADD/DELETE, we just need the data field
                        let data_to_process = if result_type == "UPDATE" {
                            result
                        } else {
                            result.get("data").unwrap_or(result)
                        };

                        let call_spec = match result_type {
                            "ADD" => query_config.added.as_ref(),
                            "UPDATE" => query_config.updated.as_ref(),
                            "DELETE" => query_config.deleted.as_ref(),
                            _ => None,
                        };

                        if let Some(spec) = call_spec {
                            if let Err(e) = Self::process_result(
                                &client,
                                &handlebars,
                                &base_url,
                                &token,
                                spec,
                                result_type,
                                data_to_process,
                                query_name,
                                &reaction_name,
                            )
                            .await
                            {
                                error!("[{}] Failed to process result: {}", reaction_name, e);
                            }
                        }
                    }
                }
            }

            info!("[{}] HTTP reaction stopped", reaction_name);
            *status.write().await = ComponentStatus::Stopped;
        });

        // Store the processing task handle
        *self.processing_task.write().await = Some(processing_task_handle);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping HTTP reaction", self.config.id);

        *self.status.write().await = ComponentStatus::Stopping;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Stopping,
            timestamp: chrono::Utc::now(),
            message: Some("Stopping HTTP reaction".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Abort all subscription forwarder tasks
        let mut subscription_tasks = self.subscription_tasks.write().await;
        for task in subscription_tasks.drain(..) {
            task.abort();
        }
        drop(subscription_tasks);

        // Abort the processing task
        let mut processing_task = self.processing_task.write().await;
        if let Some(task) = processing_task.take() {
            task.abort();
        }
        drop(processing_task);

        // Drain the priority queue
        let drained = self.priority_queue.drain().await;
        if !drained.is_empty() {
            info!(
                "[{}] Drained {} pending results from priority queue",
                self.config.id,
                drained.len()
            );
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        *self.status.write().await = ComponentStatus::Stopped;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Reaction,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: Some("HTTP reaction stopped successfully".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &ReactionConfig {
        &self.config
    }
}
