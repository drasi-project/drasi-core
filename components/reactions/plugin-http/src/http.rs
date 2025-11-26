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
use drasi_lib::config::ReactionConfig;
use drasi_lib::reactions::common::base::ReactionBase;
use drasi_lib::reactions::Reaction;
use drasi_lib::utils::log_component_start;

pub struct HttpReaction {
    base: ReactionBase,
    base_url: String,
    token: Option<String>,
    timeout_ms: u64,
    query_configs: HashMap<String, QueryConfig>,
}

impl HttpReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        // Extract HTTP-specific configuration
        let (base_url, token, timeout_ms, query_configs) = match &config.config {
            drasi_lib::config::ReactionSpecificConfig::Http(http_config_map) => {
                // Deserialize HashMap into typed config
                let http_config: HttpReactionConfig = serde_json::from_value(
                    serde_json::to_value(http_config_map).unwrap_or_default()
                ).unwrap_or_default();
                (
                    http_config.base_url.clone(),
                    http_config.token.clone(),
                    http_config.timeout_ms,
                    http_config.routes.clone(),
                )
            }
            _ => ("http://localhost".to_string(), None, 10000, HashMap::new()),
        };

        Self {
            base: ReactionBase::new(config, event_tx),
            base_url,
            token,
            timeout_ms,
            query_configs,
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
    async fn start(
        &self,
        query_subscriber: Arc<dyn drasi_lib::reactions::common::base::QuerySubscriber>,
    ) -> Result<()> {
        log_component_start("HTTP Reaction", &self.base.config.id);

        info!(
            "[{}] HTTP reaction started - sending to base URL: {}",
            self.base.config.id, self.base_url
        );

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting HTTP reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        self.base.subscribe_to_queries(query_subscriber).await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("HTTP reaction started".to_string()),
            )
            .await?;

        // Spawn the main processing task
        let reaction_name = self.base.config.id.clone();
        let status = self.base.status.clone();
        let query_configs = self.query_configs.clone();
        let base_url = self.base_url.clone();
        let token = self.token.clone();
        let timeout_ms = self.timeout_ms;
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

    fn get_config(&self) -> &ReactionConfig {
        &self.base.config
    }
}
