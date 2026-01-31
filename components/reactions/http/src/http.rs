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
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use drasi_lib::channels::{ComponentEventSender, ComponentStatus, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::{QueryProvider, Reaction};

use super::adaptive_batcher::{AdaptiveBatchConfig as InternalBatchConfig, AdaptiveBatcher};
use super::HttpReactionBuilder;

/// Batch result for sending multiple results in one HTTP call.
///
/// When adaptive batching is enabled, batches are sent to `{base_url}{batch_endpoint_path}`
/// as a JSON array of `BatchResult` objects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchResult {
    /// The ID of the query that produced these results
    pub query_id: String,
    /// Array of result diffs from the query
    pub results: Vec<ResultDiff>,
    /// ISO 8601 timestamp when the batch was created
    pub timestamp: String,
    /// Number of results in this batch (matches results.len())
    pub count: usize,
}

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

    /// Check if adaptive batching is enabled
    pub fn is_adaptive_enabled(&self) -> bool {
        self.config.adaptive.is_some()
    }

    /// Create a Handlebars instance with the json helper registered
    fn create_handlebars() -> Handlebars<'static> {
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

        handlebars
    }

    /// Create an HTTP client with appropriate configuration
    fn create_client(timeout_ms: u64, http2_enabled: bool) -> Result<Client> {
        let mut builder = Client::builder().timeout(Duration::from_millis(timeout_ms));

        // Enable HTTP/2 connection pooling when requested
        if http2_enabled {
            builder = builder
                .pool_idle_timeout(Duration::from_secs(90))
                .pool_max_idle_per_host(10)
                .http2_prior_knowledge();
        }

        builder.build().map_err(Into::into)
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
            format!("{base_url}{url}")
        };

        // Render body
        let body = if !call_spec.body.is_empty() {
            debug!(
                "[{}] Rendering template: {} with context: {:?}",
                reaction_name, call_spec.body, context
            );
            let rendered = handlebars.render_template(&call_spec.body, &context)?;
            debug!("[{reaction_name}] Rendered body: {rendered}");
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
                HeaderValue::from_str(&format!("Bearer {token}"))?,
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
        debug!("[{reaction_name}] Sending {method} request to {full_url} with body: {body}");

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

    /// Send a batch of results to the batch endpoint
    async fn send_batch(
        client: &Client,
        base_url: &str,
        batch_endpoint_path: &str,
        token: &Option<String>,
        query_configs: &HashMap<String, QueryConfig>,
        batch: Vec<(String, Vec<ResultDiff>)>,
        reaction_name: &str,
    ) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        // Group by query_id for batch sending
        let mut batches_by_query: HashMap<String, Vec<ResultDiff>> = HashMap::new();
        for (query_id, results) in batch {
            batches_by_query
                .entry(query_id)
                .or_default()
                .extend(results);
        }

        // If we have multiple results, use batch endpoint
        if batches_by_query.values().any(|v| v.len() > 1) {
            // Send as batch
            let batch_results: Vec<BatchResult> = batches_by_query
                .into_iter()
                .map(|(query_id, results)| BatchResult {
                    query_id,
                    count: results.len(),
                    results,
                    timestamp: chrono::Utc::now().to_rfc3339(),
                })
                .collect();

            let batch_url = format!("{}{}", base_url, batch_endpoint_path);
            let body = serde_json::to_string(&batch_results)?;

            // Build headers
            let mut headers = HeaderMap::new();
            headers.insert("Content-Type", HeaderValue::from_static("application/json"));
            if let Some(token) = token {
                headers.insert(
                    "Authorization",
                    HeaderValue::from_str(&format!("Bearer {token}"))?,
                );
            }

            debug!(
                "[{}] Sending batch of {} results to {}",
                reaction_name,
                batch_results.iter().map(|b| b.count).sum::<usize>(),
                batch_url
            );

            let response = client
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
                debug!("[{reaction_name}] Batch sent successfully");
            }
        } else {
            // Send individual requests for each result
            let handlebars = Self::create_handlebars();
            for (query_id, results) in batches_by_query {
                for result in results {
                    if let Err(e) = Self::send_single_result(
                        client,
                        &handlebars,
                        base_url,
                        token,
                        query_configs,
                        &query_id,
                        &result,
                        reaction_name,
                    )
                    .await
                    {
                        error!("[{reaction_name}] Failed to send result: {e}");
                    }
                }
            }
        }

        Ok(())
    }

    /// Send a single result using query-specific routes
    #[allow(clippy::too_many_arguments)]
    async fn send_single_result(
        client: &Client,
        handlebars: &Handlebars<'static>,
        base_url: &str,
        token: &Option<String>,
        query_configs: &HashMap<String, QueryConfig>,
        query_id: &str,
        result: &ResultDiff,
        reaction_name: &str,
    ) -> Result<()> {
        let (operation, call_spec) = match result {
            ResultDiff::Add { .. } => ("added", query_configs.get(query_id).and_then(|c| c.added.as_ref())),
            ResultDiff::Update { .. } | ResultDiff::Aggregation { .. } => {
                ("updated", query_configs.get(query_id).and_then(|c| c.updated.as_ref()))
            }
            ResultDiff::Delete { .. } => ("deleted", query_configs.get(query_id).and_then(|c| c.deleted.as_ref())),
            ResultDiff::Noop => return Ok(()),
        };

        if let Some(call_spec) = call_spec {
            // Prepare context for template rendering
            let mut context = Map::new();
            let data = serde_json::to_value(result).expect("ResultDiff serialization should succeed");
            context.insert("data".to_string(), data.clone());
            context.insert("query_id".to_string(), Value::String(query_id.to_string()));
            context.insert("operation".to_string(), Value::String(operation.to_string()));

            // Render URL
            let full_url = handlebars.render_template(&format!("{}{}", base_url, call_spec.url), &context)?;

            // Render body
            let body = if !call_spec.body.is_empty() {
                handlebars.render_template(&call_spec.body, &context)?
            } else {
                serde_json::to_string(&data)?
            };

            // Build headers
            let mut headers = HeaderMap::new();
            headers.insert("Content-Type", HeaderValue::from_static("application/json"));

            if let Some(token) = token {
                headers.insert(
                    "Authorization",
                    HeaderValue::from_str(&format!("Bearer {token}"))?,
                );
            }

            for (key, value) in &call_spec.headers {
                let header_name = HeaderName::from_bytes(key.as_bytes())?;
                let header_value = HeaderValue::from_str(&handlebars.render_template(value, &context)?)?;
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
            let response = client
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

    /// Run the standard (non-adaptive) processing loop
    async fn run_standard_mode(
        reaction_name: String,
        status: Arc<tokio::sync::RwLock<ComponentStatus>>,
        query_configs: HashMap<String, QueryConfig>,
        base_url: String,
        token: Option<String>,
        timeout_ms: u64,
        priority_queue: drasi_lib::channels::priority_queue::PriorityQueue<drasi_lib::channels::events::QueryResult>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        let client = match Self::create_client(timeout_ms, false) {
            Ok(c) => c,
            Err(e) => {
                error!("[{reaction_name}] Failed to create HTTP client: {e}");
                return;
            }
        };

        let handlebars = Self::create_handlebars();

        loop {
            // Use select to wait for either a result OR shutdown signal
            let query_result_arc = tokio::select! {
                biased;

                _ = &mut shutdown_rx => {
                    debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                    break;
                }

                result = priority_queue.dequeue() => result,
            };
            let query_result = query_result_arc.as_ref();

            if !matches!(*status.read().await, ComponentStatus::Running) {
                break;
            }

            if query_result.results.is_empty() {
                debug!("[{reaction_name}] Received empty result set from query");
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
                        "[{reaction_name}] No configuration for query '{query_name}', using default"
                    );
                    // Create a default configuration that POSTs all changes
                    default_config = QueryConfig {
                        added: Some(CallSpec {
                            url: format!("/changes/{query_name}"),
                            method: "POST".to_string(),
                            body: String::new(),
                            headers: HashMap::new(),
                        }),
                        updated: Some(CallSpec {
                            url: format!("/changes/{query_name}"),
                            method: "POST".to_string(),
                            body: String::new(),
                            headers: HashMap::new(),
                        }),
                        deleted: Some(CallSpec {
                            url: format!("/changes/{query_name}"),
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
                match result {
                    ResultDiff::Add { data } => {
                        if let Some(spec) = query_config.added.as_ref() {
                            if let Err(e) = Self::process_result(
                                &client,
                                &handlebars,
                                &base_url,
                                &token,
                                spec,
                                "ADD",
                                data,
                                query_name,
                                &reaction_name,
                            )
                            .await
                            {
                                error!("[{reaction_name}] Failed to process result: {e}");
                            }
                        }
                    }
                    ResultDiff::Delete { data } => {
                        if let Some(spec) = query_config.deleted.as_ref() {
                            if let Err(e) = Self::process_result(
                                &client,
                                &handlebars,
                                &base_url,
                                &token,
                                spec,
                                "DELETE",
                                data,
                                query_name,
                                &reaction_name,
                            )
                            .await
                            {
                                error!("[{reaction_name}] Failed to process result: {e}");
                            }
                        }
                    }
                    ResultDiff::Update { .. } => {
                        if let Some(spec) = query_config.updated.as_ref() {
                            let data_to_process = serde_json::to_value(result)
                                .expect("ResultDiff serialization should succeed");
                            if let Err(e) = Self::process_result(
                                &client,
                                &handlebars,
                                &base_url,
                                &token,
                                spec,
                                "UPDATE",
                                &data_to_process,
                                query_name,
                                &reaction_name,
                            )
                            .await
                            {
                                error!("[{reaction_name}] Failed to process result: {e}");
                            }
                        }
                    }
                    ResultDiff::Aggregation { .. } | ResultDiff::Noop => {}
                }
            }
        }

        info!("[{reaction_name}] HTTP reaction stopped");
        *status.write().await = ComponentStatus::Stopped;
    }

    /// Run the adaptive batching processing loop
    async fn run_adaptive_mode(
        reaction_name: String,
        status: Arc<tokio::sync::RwLock<ComponentStatus>>,
        query_configs: HashMap<String, QueryConfig>,
        base_url: String,
        batch_endpoint_path: String,
        token: Option<String>,
        timeout_ms: u64,
        http2_enabled: bool,
        adaptive_config: drasi_lib::reactions::common::AdaptiveBatchConfig,
        priority_queue: drasi_lib::channels::priority_queue::PriorityQueue<drasi_lib::channels::events::QueryResult>,
        mut shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) {
        let client = match Self::create_client(timeout_ms, http2_enabled) {
            Ok(c) => c,
            Err(e) => {
                error!("[{reaction_name}] Failed to create HTTP client: {e}");
                return;
            }
        };

        // Convert from lib config to internal batcher config
        let internal_config = InternalBatchConfig {
            min_batch_size: adaptive_config.adaptive_min_batch_size,
            max_batch_size: adaptive_config.adaptive_max_batch_size,
            throughput_window: Duration::from_millis(adaptive_config.adaptive_window_size as u64 * 100),
            max_wait_time: Duration::from_millis(adaptive_config.adaptive_batch_timeout_ms),
            min_wait_time: Duration::from_millis(100),
            adaptive_enabled: true,
        };

        // Create channel for batching with capacity based on batch configuration
        let batch_channel_capacity = internal_config.recommended_channel_capacity();
        let (batch_tx, batch_rx) = mpsc::channel(batch_channel_capacity);

        debug!(
            "[{}] HTTP reaction using adaptive batching - channel capacity: {} (max_batch_size: {} × 5)",
            reaction_name, batch_channel_capacity, internal_config.max_batch_size
        );

        // Clone values needed by batcher task
        let batcher_reaction_name = reaction_name.clone();
        let batcher_client = client.clone();
        let batcher_base_url = base_url.clone();
        let batcher_batch_endpoint_path = batch_endpoint_path.clone();
        let batcher_token = token.clone();
        let batcher_query_configs = query_configs.clone();

        // Spawn adaptive batcher task
        let batcher_handle = tokio::spawn(async move {
            let mut batcher = AdaptiveBatcher::new(batch_rx, internal_config);
            let mut total_batches = 0u64;
            let mut total_results = 0u64;

            info!("[{batcher_reaction_name}] Adaptive HTTP batcher started");

            while let Some(batch) = batcher.next_batch().await {
                if batch.is_empty() {
                    continue;
                }

                let batch_size = batch
                    .iter()
                    .map(|(_, v): &(String, Vec<ResultDiff>)| v.len())
                    .sum::<usize>();
                total_results += batch_size as u64;
                total_batches += 1;

                debug!("[{batcher_reaction_name}] Processing adaptive batch of {batch_size} results");

                if let Err(e) = Self::send_batch(
                    &batcher_client,
                    &batcher_base_url,
                    &batcher_batch_endpoint_path,
                    &batcher_token,
                    &batcher_query_configs,
                    batch,
                    &batcher_reaction_name,
                )
                .await
                {
                    error!("[{batcher_reaction_name}] Failed to send batch: {e}");
                }

                if total_batches.is_multiple_of(100) {
                    info!(
                        "[{}] Adaptive HTTP metrics - Batches: {}, Results: {}, Avg batch size: {:.1}",
                        batcher_reaction_name,
                        total_batches,
                        total_results,
                        total_results as f64 / total_batches as f64
                    );
                }
            }

            info!(
                "[{batcher_reaction_name}] Adaptive HTTP batcher stopped - Total batches: {total_batches}, Total results: {total_results}"
            );
        });

        // Main task: receive query results from priority queue and forward to batcher
        loop {
            // Use select to wait for either a result OR shutdown signal
            let query_result_arc = tokio::select! {
                biased;

                _ = &mut shutdown_rx => {
                    debug!("[{reaction_name}] Received shutdown signal, exiting processing loop");
                    break;
                }

                result = priority_queue.dequeue() => result,
            };
            let query_result = query_result_arc.as_ref();

            if !matches!(*status.read().await, ComponentStatus::Running) {
                info!("[{reaction_name}] Reaction status changed to non-running, exiting");
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
                error!("[{reaction_name}] Failed to send to batch channel");
                break;
            }
        }

        // Close the batch channel
        drop(batch_tx);

        // Wait for batcher to complete
        let _ = tokio::time::timeout(Duration::from_secs(5), batcher_handle).await;

        info!("[{reaction_name}] Adaptive HTTP reaction completed");
        *status.write().await = ComponentStatus::Stopped;
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
        props.insert(
            "adaptive_enabled".to_string(),
            serde_json::Value::Bool(self.is_adaptive_enabled()),
        );
        props.insert(
            "batch_endpoint_path".to_string(),
            serde_json::Value::String(self.config.batch_endpoint_path.clone()),
        );
        props.insert(
            "http2_enabled".to_string(),
            serde_json::Value::Bool(self.config.http2_enabled),
        );
        props
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        let adaptive_enabled = self.is_adaptive_enabled();
        let mode_str = if adaptive_enabled { "adaptive batching" } else { "standard" };

        log_component_start("HTTP Reaction", &self.base.id);

        info!(
            "[{}] HTTP reaction starting in {} mode - sending to base URL: {}",
            self.base.id, mode_str, self.config.base_url
        );

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some(format!("Starting HTTP reaction in {} mode", mode_str)),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        self.base.subscribe_to_queries().await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some(format!("HTTP reaction started in {} mode", mode_str)),
            )
            .await?;

        // Create shutdown channel for graceful termination
        let shutdown_rx = self.base.create_shutdown_channel().await;

        // Clone values for the spawned task
        let reaction_name = self.base.id.clone();
        let status = self.base.status.clone();
        let query_configs = self.config.routes.clone();
        let base_url = self.config.base_url.clone();
        let batch_endpoint_path = self.config.batch_endpoint_path.clone();
        let token = self.config.token.clone();
        let timeout_ms = self.config.timeout_ms;
        let http2_enabled = self.config.http2_enabled;
        let priority_queue = self.base.priority_queue.clone();

        // Spawn the appropriate processing task based on mode
        let processing_task_handle = if let Some(adaptive_config) = self.config.adaptive.clone() {
            tokio::spawn(Self::run_adaptive_mode(
                reaction_name,
                status,
                query_configs,
                base_url,
                batch_endpoint_path,
                token,
                timeout_ms,
                http2_enabled,
                adaptive_config,
                priority_queue,
                shutdown_rx,
            ))
        } else {
            tokio::spawn(Self::run_standard_mode(
                reaction_name,
                status,
                query_configs,
                base_url,
                token,
                timeout_ms,
                priority_queue,
                shutdown_rx,
            ))
        };

        // Store the processing task handle
        self.base.set_processing_task(processing_task_handle).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Use ReactionBase common stop functionality
        self.base.stop_common().await?;

        tokio::time::sleep(Duration::from_millis(100)).await;

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
}
