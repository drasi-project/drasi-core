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
use std::time::Duration;
// RecvError no longer needed with trait-based receivers
use tokio::sync::mpsc;

use crate::channels::{ComponentEventSender, ComponentStatus};
use crate::config::ReactionConfig;
use crate::reactions::base::ReactionBase;
use crate::reactions::Reaction;
use crate::utils::{AdaptiveBatchConfig, AdaptiveBatcher};

use super::http::{CallSpec, QueryConfig};

/// Batch result for sending multiple results in one HTTP call
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResult {
    pub query_id: String,
    pub results: Vec<Value>,
    pub timestamp: String,
    pub count: usize,
}

/// Adaptive HTTP reaction that batches webhook calls
pub struct AdaptiveHttpReaction {
    base: ReactionBase,
    base_url: String,
    token: Option<String>,
    timeout_ms: u64,
    query_configs: HashMap<String, QueryConfig>,
    // Adaptive batching configuration
    adaptive_config: AdaptiveBatchConfig,
    // HTTP client with connection pooling
    client: Client,
    // Support batch endpoints
    batch_endpoints_enabled: bool,
}

impl AdaptiveHttpReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        // Extract HTTP-specific configuration from typed config
        let (base_url, token, timeout_ms, query_configs) = match &config.config {
            crate::config::ReactionSpecificConfig::Http(http_config) => (
                http_config.base_url.clone(),
                http_config.token.clone(),
                http_config.timeout_ms,
                http_config.routes.clone(),
            ),
            _ => ("http://localhost".to_string(), None, 10000, HashMap::new()),
        };

        // Configure adaptive batching
        let mut adaptive_config = AdaptiveBatchConfig::default();

        // Allow overriding adaptive parameters from config if using custom variant
        if let crate::config::ReactionSpecificConfig::Custom { properties } = &config.config {
            if let Some(max_batch) = properties
                .get("adaptive_max_batch_size")
                .and_then(|v| v.as_u64())
            {
                adaptive_config.max_batch_size = max_batch as usize;
            }
            if let Some(min_batch) = properties
                .get("adaptive_min_batch_size")
                .and_then(|v| v.as_u64())
            {
                adaptive_config.min_batch_size = min_batch as usize;
            }
            if let Some(max_wait_ms) = properties
                .get("adaptive_max_wait_ms")
                .and_then(|v| v.as_u64())
            {
                adaptive_config.max_wait_time = Duration::from_millis(max_wait_ms);
            }
            if let Some(min_wait_ms) = properties
                .get("adaptive_min_wait_ms")
                .and_then(|v| v.as_u64())
            {
                adaptive_config.min_wait_time = Duration::from_millis(min_wait_ms);
            }
            if let Some(window_secs) = properties
                .get("adaptive_window_secs")
                .and_then(|v| v.as_u64())
            {
                adaptive_config.throughput_window = Duration::from_secs(window_secs);
            }

            // Check if adaptive mode is explicitly disabled
            if let Some(enabled) = properties.get("adaptive_enabled").and_then(|v| v.as_bool()) {
                adaptive_config.adaptive_enabled = enabled;
            }
        }

        // Check if batch endpoints are enabled
        let batch_endpoints_enabled = true; // Default to true for adaptive HTTP

        // Convert typed QueryCallConfig to local QueryConfig
        let query_configs: HashMap<String, QueryConfig> = query_configs
            .into_iter()
            .map(|(key, call_config)| {
                (
                    key,
                    QueryConfig {
                        added: call_config.added.map(|spec| CallSpec {
                            url: spec.url,
                            method: spec.method,
                            body: spec.body,
                            headers: spec.headers,
                        }),
                        updated: call_config.updated.map(|spec| CallSpec {
                            url: spec.url,
                            method: spec.method,
                            body: spec.body,
                            headers: spec.headers,
                        }),
                        deleted: call_config.deleted.map(|spec| CallSpec {
                            url: spec.url,
                            method: spec.method,
                            body: spec.body,
                            headers: spec.headers,
                        }),
                    },
                )
            })
            .collect();

        // Create HTTP client with connection pooling
        let client = Client::builder()
            .timeout(Duration::from_millis(timeout_ms))
            .pool_idle_timeout(Duration::from_secs(90))
            .pool_max_idle_per_host(10)
            .http2_prior_knowledge() // Use HTTP/2 when available
            .build()
            .unwrap_or_else(|_| Client::new());

        Self {
            base: ReactionBase::new(config, event_tx),
            base_url,
            token,
            timeout_ms,
            query_configs,
            adaptive_config,
            client,
            batch_endpoints_enabled,
        }
    }

    async fn send_batch(
        &self,
        batch: Vec<(String, Vec<Value>)>,
        reaction_name: &str,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        // Group by query_id for batch sending
        let mut batches_by_query: HashMap<String, Vec<Value>> = HashMap::new();
        for (query_id, results) in batch {
            batches_by_query
                .entry(query_id)
                .or_default()
                .extend(results);
        }

        // If batch endpoints are enabled and we have multiple results, use batch endpoint
        if self.batch_endpoints_enabled && batches_by_query.values().any(|v| v.len() > 1) {
            // Send as batch
            let batch_results: Vec<BatchResult> = batches_by_query
                .into_iter()
                .map(|(query_id, results)| BatchResult {
                    query_id: query_id.clone(),
                    count: results.len(),
                    results,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                })
                .collect();

            let batch_url = format!("{}/batch", self.base_url);
            let body = serde_json::to_string(&batch_results)?;

            // Build headers
            let mut headers = HeaderMap::new();
            headers.insert("Content-Type", HeaderValue::from_static("application/json"));
            if let Some(ref token) = self.token {
                headers.insert(
                    "Authorization",
                    HeaderValue::from_str(&format!("Bearer {}", token))?,
                );
            }

            debug!(
                "[{}] Sending batch of {} results to {}",
                reaction_name,
                batch_results.iter().map(|b| b.count).sum::<usize>(),
                batch_url
            );

            let response = self
                .client
                .post(&batch_url)
                .headers(headers)
                .body(body)
                .send()
                .await?;

            let status = response.status();
            if !status.is_success() {
                let error_body = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unable to read response body".to_string());
                warn!(
                    "[{}] Batch HTTP request failed with status {}: {}",
                    reaction_name,
                    status.as_u16(),
                    error_body
                );
            } else {
                debug!("[{}] Batch sent successfully", reaction_name);
            }
        } else {
            // Send individual requests for each result
            for (query_id, results) in batches_by_query {
                for result in results {
                    if let Err(e) = self
                        .send_single_result(&query_id, &result, reaction_name)
                        .await
                    {
                        error!("[{}] Failed to send result: {}", reaction_name, e);
                    }
                }
            }
        }

        Ok(())
    }

    async fn send_single_result(
        &self,
        query_id: &str,
        data: &Value,
        reaction_name: &str,
    ) -> Result<()> {
        // Determine operation type
        let operation = if data.get("before").is_some() && data.get("after").is_some() {
            "updated"
        } else if data.get("after").is_some() {
            "added"
        } else {
            "deleted"
        };

        // Get call spec for this query and operation
        let call_spec = match self.query_configs.get(query_id) {
            Some(config) => match operation {
                "added" => config.added.as_ref(),
                "updated" => config.updated.as_ref(),
                "deleted" => config.deleted.as_ref(),
                _ => None,
            },
            None => None,
        };

        if let Some(call_spec) = call_spec {
            // Prepare context for template rendering
            let mut context = Map::new();
            context.insert("data".to_string(), data.clone());
            context.insert("query_id".to_string(), Value::String(query_id.to_string()));
            context.insert(
                "operation".to_string(),
                Value::String(operation.to_string()),
            );

            // Render URL
            let handlebars = Handlebars::new();
            let full_url = handlebars
                .render_template(&format!("{}{}", self.base_url, call_spec.url), &context)?;

            // Render body
            let body = if !call_spec.body.is_empty() {
                handlebars.render_template(&call_spec.body, &context)?
            } else {
                serde_json::to_string(&data)?
            };

            // Build headers
            let mut headers = HeaderMap::new();
            headers.insert("Content-Type", HeaderValue::from_static("application/json"));

            if let Some(ref token) = self.token {
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
            let response = self
                .client
                .request(method, &full_url)
                .headers(headers)
                .body(body)
                .send()
                .await?;

            let status = response.status();
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
        }

        Ok(())
    }

    async fn run_internal(
        reaction_name: String,
        base: ReactionBase,
        adaptive_config: AdaptiveBatchConfig,
        sender: Arc<Self>,
    ) {
        // Create channel for batching
        let (batch_tx, batch_rx) = mpsc::channel(1000);

        // Spawn adaptive batcher task
        let batcher_handle = tokio::spawn({
            let reaction_name = reaction_name.clone();
            let sender = sender.clone();
            async move {
                let mut batcher = AdaptiveBatcher::new(batch_rx, adaptive_config);
                let mut total_batches = 0u64;
                let mut total_results = 0u64;

                info!("[{}] Adaptive HTTP batcher started", reaction_name);

                while let Some(batch) = batcher.next_batch().await {
                    if batch.is_empty() {
                        continue;
                    }

                    let batch_size = batch
                        .iter()
                        .map(|(_, v): &(String, Vec<Value>)| v.len())
                        .sum::<usize>();
                    total_results += batch_size as u64;
                    total_batches += 1;

                    debug!(
                        "[{}] Processing adaptive batch of {} results",
                        reaction_name, batch_size
                    );

                    if let Err(e) = sender.send_batch(batch, &reaction_name).await {
                        error!("[{}] Failed to send batch: {}", reaction_name, e);
                    }

                    if total_batches % 100 == 0 {
                        info!(
                            "[{}] Adaptive HTTP metrics - Batches: {}, Results: {}, Avg batch size: {:.1}",
                            reaction_name,
                            total_batches,
                            total_results,
                            total_results as f64 / total_batches as f64
                        );
                    }
                }

                info!(
                    "[{}] Adaptive HTTP batcher stopped - Total batches: {}, Total results: {}",
                    reaction_name, total_batches, total_results
                );
            }
        });

        // Main task: receive query results from priority queue and forward to batcher
        loop {
            let query_result_arc = base.priority_queue.dequeue().await;
            let query_result = query_result_arc.as_ref();

            if !matches!(*base.status.read().await, ComponentStatus::Running) {
                info!(
                    "[{}] Reaction status changed to non-running, exiting",
                    reaction_name
                );
                break;
            }

            if query_result.results.is_empty() {
                continue;
            }

            // Send to batcher
            if batch_tx
                .send((query_result.query_id.clone(), query_result.results.clone()))
                .await
                .is_err()
            {
                error!("[{}] Failed to send to batch channel", reaction_name);
                break;
            }
        }

        // Close the batch channel
        drop(batch_tx);

        // Wait for batcher to complete
        let _ = tokio::time::timeout(Duration::from_secs(5), batcher_handle).await;

        info!("[{}] Adaptive HTTP reaction completed", reaction_name);
    }
}

#[async_trait]
impl Reaction for AdaptiveHttpReaction {
    async fn start(
        &self,
        query_subscriber: Arc<dyn crate::reactions::base::QuerySubscriber>,
    ) -> Result<()> {
        info!("[{}] Starting adaptive HTTP reaction", self.base.config.id);

        // Set status to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting adaptive HTTP reaction".to_string()),
            )
            .await?;

        // Subscribe to queries
        self.base.subscribe_to_queries(query_subscriber).await?;

        // Set status to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some(format!(
                    "Adaptive HTTP reaction running - Base URL: {}, Batch endpoints: {}",
                    self.base_url,
                    if self.batch_endpoints_enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                )),
            )
            .await?;

        // Create Arc for sharing self with the internal task
        let self_arc = Arc::new(Self {
            base: ReactionBase {
                config: self.base.config.clone(),
                status: self.base.status.clone(),
                event_tx: self.base.event_tx.clone(),
                priority_queue: self.base.priority_queue.clone(),
                subscription_tasks: self.base.subscription_tasks.clone(),
                processing_task: self.base.processing_task.clone(),
            },
            base_url: self.base_url.clone(),
            token: self.token.clone(),
            timeout_ms: self.timeout_ms,
            query_configs: self.query_configs.clone(),
            adaptive_config: self.adaptive_config.clone(),
            client: self.client.clone(),
            batch_endpoints_enabled: self.batch_endpoints_enabled,
        });

        let reaction_name = self.base.config.id.clone();
        let base = ReactionBase {
            config: self.base.config.clone(),
            status: self.base.status.clone(),
            event_tx: self.base.event_tx.clone(),
            priority_queue: self.base.priority_queue.clone(),
            subscription_tasks: self.base.subscription_tasks.clone(),
            processing_task: self.base.processing_task.clone(),
        };
        let adaptive_config = self.adaptive_config.clone();

        let processing_task_handle = tokio::spawn(Self::run_internal(
            reaction_name,
            base,
            adaptive_config,
            self_arc,
        ));

        // Store the processing task handle
        self.base.set_processing_task(processing_task_handle).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping adaptive HTTP reaction", self.base.config.id);

        // Set status to Stopping
        self.base
            .set_status_with_event(
                ComponentStatus::Stopping,
                Some("Stopping adaptive HTTP reaction".to_string()),
            )
            .await?;

        // Perform common cleanup
        self.base.stop_common().await?;

        // Wait a moment for cleanup
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Set status to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Adaptive HTTP reaction stopped successfully".to_string()),
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
