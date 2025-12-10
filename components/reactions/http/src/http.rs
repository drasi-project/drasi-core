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

pub use super::config::{CallSpec, HttpReactionConfig, QueryConfig};

use anyhow::Result;
use async_trait::async_trait;
use handlebars::Handlebars;
use log::{debug, error, info, warn};
use reqwest::{
    header::{HeaderMap, HeaderName, HeaderValue},
    Client, Method,
};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;

use drasi_lib::channels::{ComponentEventSender, ComponentStatus};
use drasi_lib::managers::log_component_start;
use drasi_lib::plugin_core::{QuerySubscriber, Reaction};
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};

use super::HttpReactionBuilder;

pub struct HttpReaction {
    base: ReactionBase,
    config: HttpReactionConfig,
}

impl HttpReaction {
    /// Create a builder for HttpReaction
    pub fn builder(id: impl Into<String>) -> HttpReactionBuilder {
        HttpReactionBuilder::new(id)
    }

    /// Create a new HTTP reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: HttpReactionConfig) -> Self {
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    /// Create a new HTTP reaction with custom priority queue capacity
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: HttpReactionConfig,
        priority_queue_capacity: usize,
    ) -> Self {
        let id = id.into();
        let params = ReactionBaseParams::new(id, queries)
            .with_priority_queue_capacity(priority_queue_capacity);
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }

    /// Create from builder (internal method)
    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: HttpReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        Self {
            base: ReactionBase::new(params),
            config,
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
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "http"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "base_url".to_string(),
            serde_json::Value::String(self.config.base_url.clone()),
        );
        props.insert(
            "timeout_ms".to_string(),
            serde_json::Value::Number(self.config.timeout_ms.into()),
        );
        props
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn inject_query_subscriber(&self, query_subscriber: Arc<dyn QuerySubscriber>) {
        self.base.inject_query_subscriber(query_subscriber).await;
    }

    async fn start(&self) -> Result<()> {
        log_component_start("HTTP Reaction", &self.base.id);

        info!(
            "[{}] HTTP reaction started - sending to base URL: {}",
            self.base.id, self.config.base_url
        );

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting HTTP reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        // QuerySubscriber was injected via inject_query_subscriber() when reaction was added
        self.base.subscribe_to_queries().await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("HTTP reaction started".to_string()),
            )
            .await?;

        // Create shutdown channel for graceful termination
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // Spawn the main processing task
        let reaction_name = self.base.id.clone();
        let status = self.base.status.clone();
        let query_configs = self.config.routes.clone();
        let base_url = self.config.base_url.clone();
        let token = self.config.token.clone();
        let timeout_ms = self.config.timeout_ms;
        let priority_queue = self.base.priority_queue.clone();

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
                // Use select to wait for either a result OR shutdown signal
                let query_result_arc = tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        debug!("[{}] Received shutdown signal, exiting processing loop", reaction_name);
                        break;
                    }

                    result = priority_queue.dequeue() => result,
                };
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
                        .next_back()
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
        self.base.set_processing_task(processing_task_handle).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Use ReactionBase common stop functionality
        self.base.stop_common().await?;

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Transition to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("HTTP reaction stopped successfully".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}
