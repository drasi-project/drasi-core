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

use async_trait::async_trait;
use axum::http::Method;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::IntoResponse;
use axum::{routing::get, Router};
use handlebars::Handlebars;
use log::{debug, error, info};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tower_http::cors::{Any, CorsLayer};

use drasi_lib::channels::{ComponentEventSender, ComponentStatus, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::{QueryProvider, Reaction};

pub use super::config::SseReactionConfig;
use super::SseReactionBuilder;

const BROADCAST_CHANNEL_CAPACITY: usize = 1024;

/// Helper function to pre-create broadcasters for static paths in a template spec
fn pre_create_broadcaster_for_template_spec(
    broadcasters: &mut HashMap<String, broadcast::Sender<String>>,
    template_spec: &super::config::TemplateSpec,
    base_sse_path: &str,
) {
    if let Some(custom_path) = &template_spec.extension.path {
        // Only pre-create broadcasters for static paths (no template variables)
        if !custom_path.contains("{{") {
            let resolved_path = if custom_path.starts_with('/') {
                custom_path.clone()
            } else {
                format!("{base_sse_path}/{custom_path}")
            };
            broadcasters.entry(resolved_path).or_insert_with(|| {
                let (tx, _rx) = broadcast::channel(BROADCAST_CHANNEL_CAPACITY);
                tx
            });
        }
    }
}

/// Helper function to pre-create broadcasters for all operation types in a QueryConfig
fn pre_create_broadcasters_for_query_config(
    broadcasters: &mut HashMap<String, broadcast::Sender<String>>,
    query_config: &super::config::QueryConfig,
    base_sse_path: &str,
) {
    if let Some(ref spec) = query_config.added {
        pre_create_broadcaster_for_template_spec(broadcasters, spec, base_sse_path);
    }
    if let Some(ref spec) = query_config.updated {
        pre_create_broadcaster_for_template_spec(broadcasters, spec, base_sse_path);
    }
    if let Some(ref spec) = query_config.deleted {
        pre_create_broadcaster_for_template_spec(broadcasters, spec, base_sse_path);
    }
}

/// SSE reaction exposes query results to browser clients via Server-Sent Events.
pub struct SseReaction {
    base: ReactionBase,
    config: SseReactionConfig,
    broadcasters: Arc<tokio::sync::RwLock<HashMap<String, broadcast::Sender<String>>>>,
    task_handles: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl std::fmt::Debug for SseReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseReaction")
            .field("id", &self.base.id)
            .field("config", &self.config)
            .field("broadcasters", &"<broadcasters>")
            .field("task_handles", &"<task_handles>")
            .finish()
    }
}

impl SseReaction {
    /// Create a builder for SseReaction
    pub fn builder(id: impl Into<String>) -> SseReactionBuilder {
        SseReactionBuilder::new(id)
    }

    /// Create a new SSE reaction
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(id: impl Into<String>, queries: Vec<String>, config: SseReactionConfig) -> Self {
        Self::create_internal(id.into(), queries, config, None, true)
    }

    /// Create a new SSE reaction with custom priority queue capacity
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn with_priority_queue_capacity(
        id: impl Into<String>,
        queries: Vec<String>,
        config: SseReactionConfig,
        priority_queue_capacity: usize,
    ) -> Self {
        Self::create_internal(
            id.into(),
            queries,
            config,
            Some(priority_queue_capacity),
            true,
        )
    }

    /// Create from builder (internal method)
    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: SseReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        Self::create_internal(id, queries, config, priority_queue_capacity, auto_start)
    }

    /// Internal constructor
    fn create_internal(
        id: String,
        queries: Vec<String>,
        config: SseReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }

        // Create default broadcaster for the main sse_path
        let mut broadcasters = HashMap::new();
        let (tx, _rx) = broadcast::channel(BROADCAST_CHANNEL_CAPACITY);
        broadcasters.insert(config.sse_path.clone(), tx);

        // Pre-create broadcasters for all configured static paths
        // This ensures paths are available immediately when clients connect
        // Note: Dynamic paths with template variables will still be created on-demand
        for query_config in config.routes.values() {
            pre_create_broadcasters_for_query_config(
                &mut broadcasters,
                query_config,
                &config.sse_path,
            );
        }

        // Also check default template for static paths
        if let Some(default_config) = &config.default_template {
            pre_create_broadcasters_for_query_config(
                &mut broadcasters,
                default_config,
                &config.sse_path,
            );
        }

        Self {
            base: ReactionBase::new(params),
            config,
            broadcasters: Arc::new(tokio::sync::RwLock::new(broadcasters)),
            task_handles: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }

    /// Resolve the SSE path for an event based on the template spec and base path
    fn resolve_sse_path(
        custom_path: Option<&String>,
        base_sse_path: &str,
        handlebars: &Handlebars,
        context: &Map<String, Value>,
        reaction_id: &str,
    ) -> String {
        if let Some(custom_path) = custom_path {
            // Render the path template if it contains variables
            let rendered_path = if custom_path.contains("{{") {
                handlebars
                    .render_template(custom_path, context)
                    .unwrap_or_else(|e| {
                        error!("[{reaction_id}] Failed to render path template '{custom_path}': {e}. Using template as-is.");
                        custom_path.clone()
                    })
            } else {
                custom_path.clone()
            };
            // Ensure path starts with /
            if rendered_path.starts_with('/') {
                rendered_path
            } else {
                format!("{base_sse_path}/{rendered_path}")
            }
        } else {
            base_sse_path.to_string()
        }
    }
}

#[async_trait]
impl Reaction for SseReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "sse"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "host".to_string(),
            serde_json::Value::String(self.config.host.clone()),
        );
        props.insert(
            "port".to_string(),
            serde_json::Value::Number(self.config.port.into()),
        );
        props.insert(
            "sse_path".to_string(),
            serde_json::Value::String(self.config.sse_path.clone()),
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

    async fn start(&self) -> anyhow::Result<()> {
        log_component_start("SSE Reaction", &self.base.id);

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting SSE reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        // QueryProvider is available from initialize() context
        self.base.subscribe_to_queries().await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("SSE reaction started".to_string()),
            )
            .await?;

        // Create shutdown channel for graceful termination
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // Spawn processing task
        let status = self.base.status.clone();
        let broadcasters = self.broadcasters.clone();
        let reaction_id = self.base.id.clone();
        let priority_queue = self.base.priority_queue.clone();
        let query_configs = self.config.routes.clone();
        let default_template = self.config.default_template.clone();
        let base_sse_path = self.config.sse_path.clone();
        let processing_handle = tokio::spawn(async move {
            info!("[{reaction_id}] SSE result processing task started");

            let mut handlebars = Handlebars::new();

            // Register the json helper to serialize values as JSON
            super::register_json_helper(&mut handlebars);

            loop {
                if !matches!(*status.read().await, ComponentStatus::Running) {
                    info!("[{reaction_id}] SSE reaction not running, breaking loop");
                    break;
                }

                // Use select to wait for either a result OR shutdown signal
                let query_result = tokio::select! {
                    biased;

                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_id}] Received shutdown signal, exiting processing loop");
                        break;
                    }

                    result = priority_queue.dequeue() => result,
                };

                info!(
                    "[{}] Processing result from query '{}' with {} items",
                    reaction_id,
                    query_result.query_id,
                    query_result.results.len()
                );

                let query_name = &query_result.query_id;
                let timestamp = chrono::Utc::now().timestamp_millis();

                // Check if we have configuration for this query
                // First, try exact match on full query ID
                // If the query ID is in dotted format (e.g., "source.query" or "namespace.source.query"),
                // also try matching just the last segment for flexibility with source-prefixed queries
                // Note: If multiple queries share the same final segment, configure using full IDs to avoid ambiguity
                let query_config = query_configs
                    .get(query_name)
                    .or_else(|| {
                        if query_name.contains('.') {
                            query_name
                                .rsplit('.')
                                .next()
                                .and_then(|name| query_configs.get(name))
                        } else {
                            None
                        }
                    })
                    .or(default_template.as_ref());

                // Process results based on query-specific configuration or default template
                if let Some(config) = query_config {
                    // Per-query custom templates
                    for result in &query_result.results {
                        let (template_spec, operation) = match result {
                            ResultDiff::Add { .. } => (config.added.as_ref(), "ADD"),
                            ResultDiff::Update { .. } => (config.updated.as_ref(), "UPDATE"),
                            ResultDiff::Delete { .. } => (config.deleted.as_ref(), "DELETE"),
                            ResultDiff::Aggregation { .. } | ResultDiff::Noop => (None, "NOOP"),
                        };

                        if let Some(spec) = template_spec {
                            // Prepare context for template
                            let mut context = Map::new();

                            match result {
                                ResultDiff::Add { data } => {
                                    let data_json = serde_json::to_value(data)
                                        .expect("QueryVariables serialization should succeed");
                                    context.insert("after".to_string(), data_json);
                                }
                                ResultDiff::Update { before, after, .. } => {
                                    let before_json = serde_json::to_value(before)
                                        .expect("QueryVariables serialization should succeed");
                                    let after_json = serde_json::to_value(after)
                                        .expect("QueryVariables serialization should succeed");
                                    context.insert("before".to_string(), before_json);
                                    context.insert("after".to_string(), after_json);
                                }
                                ResultDiff::Delete { data } => {
                                    let data_json = serde_json::to_value(data)
                                        .expect("QueryVariables serialization should succeed");
                                    context.insert("before".to_string(), data_json);
                                }
                                ResultDiff::Aggregation { .. } | ResultDiff::Noop => {}
                            }

                            context.insert(
                                "query_name".to_string(),
                                Value::String(query_name.to_string()),
                            );
                            context.insert(
                                "operation".to_string(),
                                Value::String(operation.to_string()),
                            );
                            context
                                .insert("timestamp".to_string(), Value::Number(timestamp.into()));

                            // Determine the SSE path for this event
                            let sse_path = SseReaction::resolve_sse_path(
                                spec.extension.path.as_ref(),
                                &base_sse_path,
                                &handlebars,
                                &context,
                                &reaction_id,
                            );

                            // Render template if provided
                            let payload = if !spec.template.is_empty() {
                                match handlebars.render_template(&spec.template, &context) {
                                    Ok(rendered) => rendered,
                                    Err(e) => {
                                        error!(
                                            "[{reaction_id}] Failed to render template for query '{query_name}': {e}. Falling back to default format."
                                        );
                                        json!({
                                            "queryId": query_name,
                                            "result": result,
                                            "timestamp": timestamp
                                        })
                                        .to_string()
                                    }
                                }
                            } else {
                                json!({
                                    "queryId": query_name,
                                    "result": result,
                                    "timestamp": timestamp
                                })
                                .to_string()
                            };

                            // Get or create broadcaster for this path (double-checked locking)
                            let broadcaster = {
                                // Try read lock first (common case)
                                {
                                    let broadcasters_read = broadcasters.read().await;
                                    if let Some(broadcaster) = broadcasters_read.get(&sse_path) {
                                        broadcaster.clone()
                                    } else {
                                        drop(broadcasters_read);
                                        // Need to create broadcaster, acquire write lock
                                        let mut broadcasters_write = broadcasters.write().await;
                                        // Re-check if another thread created it while we waited for write lock
                                        if let Some(broadcaster) = broadcasters_write.get(&sse_path)
                                        {
                                            broadcaster.clone()
                                        } else {
                                            let (tx, _rx) =
                                                broadcast::channel(BROADCAST_CHANNEL_CAPACITY);
                                            debug!(
                                                "[{reaction_id}] Created broadcaster for path: {sse_path}"
                                            );
                                            broadcasters_write.insert(sse_path.clone(), tx.clone());
                                            tx
                                        }
                                    }
                                }
                            };

                            match broadcaster.send(payload.clone()) {
                                Ok(count) => {
                                    debug!(
                                        "[{reaction_id}] Broadcast to {count} SSE listeners on path {sse_path}"
                                    )
                                }
                                Err(e) => debug!(
                                    "[{reaction_id}] no SSE listeners on path {sse_path}: {e}"
                                ),
                            }
                        }
                    }
                } else {
                    // Default behavior - send all results together to the base path
                    let payload = json!({
                        "queryId": query_result.query_id,
                        "results": query_result.results,
                        "timestamp": timestamp
                    })
                    .to_string();

                    // Get broadcaster for the base path
                    let broadcaster = {
                        let broadcasters_read = broadcasters.read().await;
                        broadcasters_read.get(&base_sse_path).cloned()
                    };

                    if let Some(broadcaster) = broadcaster {
                        match broadcaster.send(payload.clone()) {
                            Ok(count) => {
                                info!("[{reaction_id}] Broadcast query result to {count} SSE listeners on {base_sse_path}")
                            }
                            Err(e) => {
                                debug!("[{reaction_id}] no SSE listeners on {base_sse_path}: {e}")
                            }
                        }
                    } else {
                        // This should not happen because the base_sse_path broadcaster is pre-created,
                        // but log an error defensively so dropped events are visible during debugging.
                        error!(
                            "[{reaction_id}] Missing broadcaster for base SSE path {base_sse_path}; \
                             dropping event payload for query '{}'",
                            query_result.query_id
                        );
                    }
                }
            }
            info!("[{reaction_id}] SSE result processing task ended");
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_handle).await;

        // Heartbeat task - sends to all paths
        let broadcasters_hb = self.broadcasters.clone();
        let interval = self.config.heartbeat_interval_ms;
        let hb_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(interval));
            loop {
                ticker.tick().await;
                let beat = json!({"type":"heartbeat","ts": chrono::Utc::now().timestamp_millis()})
                    .to_string();
                // Send heartbeat to all broadcasters
                let broadcasters_read = broadcasters_hb.read().await;
                for broadcaster in broadcasters_read.values() {
                    let _ = broadcaster.send(beat.clone());
                }
            }
        });
        self.task_handles.lock().await.push(hb_handle);

        // HTTP server task - dynamically creates routes for all paths
        let host = self.config.host.clone();
        let port = self.config.port;
        let broadcasters_server = self.broadcasters.clone();
        let server_handle = tokio::spawn(async move {
            // Configure CORS to allow all origins
            let cors = CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::OPTIONS])
                .allow_headers(Any);

            // Create a handler that checks for matching paths dynamically
            let broadcasters_clone = broadcasters_server.clone();
            let handler = get(move |req: axum::http::Request<axum::body::Body>| {
                let broadcasters = broadcasters_clone.clone();
                async move {
                    let path = req.uri().path().to_string();

                    // Try to find a broadcaster for this path
                    let broadcaster = {
                        let broadcasters_read = broadcasters.read().await;
                        broadcasters_read.get(&path).cloned()
                    };

                    if let Some(broadcaster) = broadcaster {
                        let rx = broadcaster.subscribe();
                        let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
                            .filter_map(|res| res.ok())
                            .map(|msg| {
                                Ok::<Event, std::convert::Infallible>(Event::default().data(msg))
                            });
                        Sse::new(stream)
                            .keep_alive(
                                KeepAlive::new()
                                    .interval(Duration::from_secs(30))
                                    .text("keep-alive"),
                            )
                            .into_response()
                    } else {
                        // Return 404 for unknown paths
                        axum::http::StatusCode::NOT_FOUND.into_response()
                    }
                }
            });

            let app = Router::new().fallback(handler).layer(cors);

            info!("Starting SSE server on {host}:{port} with CORS enabled");
            let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
                Ok(l) => l,
                Err(e) => {
                    error!("Failed to bind SSE server: {e}");
                    return;
                }
            };
            if let Err(e) = axum::serve(listener, app).await {
                error!("SSE server error: {e}");
            }
        });
        self.task_handles.lock().await.push(server_handle);

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        // Use ReactionBase common stop functionality
        self.base.stop_common().await?;

        // Cancel all other tasks (heartbeat, HTTP server)
        let mut handles = self.task_handles.lock().await;
        for handle in handles.drain(..) {
            handle.abort();
        }

        // Transition to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("SSE reaction stopped".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
}
