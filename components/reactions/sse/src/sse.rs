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

use anyhow::Context;
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
use tokio::sync::{broadcast, watch, Mutex};
use tokio::task::JoinSet;
use tokio_stream::StreamExt;
use tower_http::cors::{Any, CorsLayer};

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::{Reaction, SnapshotFetcher};

pub use super::config::SseReactionConfig;
use super::SseReactionBuilder;

const BROADCAST_CHANNEL_CAPACITY: usize = 1024;
const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct SseLifecycle {
    shutdown: Option<watch::Sender<bool>>,
    tasks: JoinSet<anyhow::Result<()>>,
}

impl SseLifecycle {
    async fn join_tasks(&mut self, reaction_id: &str, task_error: &mut Option<anyhow::Error>) {
        while let Some(result) = self.tasks.join_next().await {
            let error = match result {
                Ok(Ok(())) => continue,
                Ok(Err(error)) => error,
                Err(error) if error.is_cancelled() => continue,
                Err(error) => error.into(),
            };
            error!("[{reaction_id}] SSE background task failed: {error:#}");
            task_error.get_or_insert(error);
        }
    }
}

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
    pub(crate) base: ReactionBase,
    config: SseReactionConfig,
    broadcasters: Arc<tokio::sync::RwLock<HashMap<String, broadcast::Sender<String>>>>,
    lifecycle: Mutex<SseLifecycle>,
}

impl std::fmt::Debug for SseReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseReaction")
            .field("id", &self.base.id)
            .field("config", &self.config)
            .field("broadcasters", &"<broadcasters>")
            .field("lifecycle", &"<lifecycle>")
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
            lifecycle: Mutex::new(SseLifecycle::default()),
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
        use crate::descriptor::SseReactionConfigDto;

        self.base
            .properties_or_serialize(&SseReactionConfigDto::from(&self.config))
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
        // Serialize lifecycle transitions, including partially completed starts and stops.
        let mut lifecycle = self.lifecycle.lock().await;
        anyhow::ensure!(
            lifecycle.shutdown.is_none()
                && lifecycle.tasks.is_empty()
                && self.base.processing_task.read().await.is_none(),
            "SSE reaction '{}' is already started; stop it before starting again",
            self.base.id
        );

        log_component_start("SSE Reaction", &self.base.id);

        // Transition to Starting
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting SSE reaction".to_string()),
            )
            .await;

        let host = self.config.host.clone();
        let port = self.config.port;
        let listener = match tokio::time::timeout(
            LIFECYCLE_TIMEOUT,
            tokio::net::TcpListener::bind((host.as_str(), port)),
        )
        .await
        .context("SSE listener bind timed out")
        .and_then(|result| result.map_err(anyhow::Error::from))
        .with_context(|| format!("Failed to bind SSE server on {host}:{port}"))
        {
            Ok(listener) => listener,
            Err(error) => {
                error!("[{}] {error:#}", self.base.id);
                self.base
                    .set_status(ComponentStatus::Error, Some(format!("{error:#}")))
                    .await;
                return Err(error);
            }
        };

        let (shutdown_tx, mut server_shutdown_rx) = watch::channel(false);
        let mut heartbeat_shutdown_rx = shutdown_tx.subscribe();
        lifecycle.shutdown = Some(shutdown_tx);

        // Create shutdown channel for graceful termination
        let mut shutdown_rx = self.base.create_shutdown_channel().await;

        // Spawn processing task
        let status_handle = self.base.status_handle();
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
                if !matches!(
                    status_handle.get_status().await,
                    ComponentStatus::Starting | ComponentStatus::Running
                ) {
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
                            ResultDiff::Aggregation { .. } => {
                                (config.updated.as_ref(), "AGGREGATION")
                            }
                            ResultDiff::Noop => (None, "NOOP"),
                        };

                        if let Some(spec) = template_spec {
                            // Prepare context for template
                            let mut context = Map::new();

                            match result {
                                ResultDiff::Add { data, .. } => {
                                    context.insert("after".to_string(), data.clone());
                                }
                                ResultDiff::Update { before, after, .. } => {
                                    context.insert("before".to_string(), before.clone());
                                    context.insert("after".to_string(), after.clone());
                                }
                                ResultDiff::Delete { data, .. } => {
                                    context.insert("before".to_string(), data.clone());
                                }
                                ResultDiff::Aggregation { before, after, .. } => {
                                    if let Some(before) = before {
                                        context.insert("before".to_string(), before.clone());
                                    }
                                    context.insert("after".to_string(), after.clone());
                                }
                                ResultDiff::Noop => {}
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
        lifecycle.tasks.spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(interval));
            loop {
                tokio::select! {
                    biased;
                    _ = heartbeat_shutdown_rx.wait_for(|shutdown| *shutdown) => break,
                    _ = ticker.tick() => {}
                }
                let beat = json!({"type":"heartbeat","ts": chrono::Utc::now().timestamp_millis()})
                    .to_string();
                // Send heartbeat to all broadcasters
                let broadcasters_read = broadcasters_hb.read().await;
                for broadcaster in broadcasters_read.values() {
                    let _ = broadcaster.send(beat.clone());
                }
            }
            Ok(())
        });

        // HTTP server task - dynamically creates routes for all paths
        let broadcasters_server = self.broadcasters.clone();

        // Get snapshot_fetcher from the runtime context for the /snapshot/:query_id endpoint
        let snapshot_fetcher = self
            .base
            .context()
            .await
            .and_then(|ctx| ctx.snapshot_fetcher.clone());

        lifecycle.tasks.spawn(async move {
            // Configure CORS to allow all origins
            let cors = CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::OPTIONS])
                .allow_headers(Any);

            // Create a handler that checks for matching paths dynamically
            let broadcasters_clone = broadcasters_server.clone();
            let stream_shutdown_rx = server_shutdown_rx.clone();
            let handler = get(move |req: axum::http::Request<axum::body::Body>| {
                let broadcasters = broadcasters_clone.clone();
                let mut shutdown_rx = stream_shutdown_rx.clone();
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
                        let stream = futures::StreamExt::take_until(stream, async move {
                            let _ = shutdown_rx.wait_for(|shutdown| *shutdown).await;
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

            let app = Router::new()
                .route(
                    "/snapshot/:query_id",
                    get({
                        let sf = snapshot_fetcher.clone();
                        move |axum::extract::Path(query_id): axum::extract::Path<String>| {
                            let sf = sf.clone();
                            async move {
                                let fetcher = match sf.as_ref() {
                                    Some(f) => f,
                                    None => {
                                        return (
                                            axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                            "Snapshot fetcher not available",
                                        )
                                            .into_response();
                                    }
                                };
                                match fetcher.fetch_snapshot(&query_id).await {
                                    Ok(snapshot) => {
                                        // Stream the snapshot as a JSON array using chunked
                                        // transfer encoding. This keeps memory proportional to
                                        // a single row rather than the full result set.
                                        let is_first = std::sync::Arc::new(
                                            std::sync::atomic::AtomicBool::new(true),
                                        );
                                        let row_stream = snapshot.map(move |row| {
                                            let json = serde_json::to_string(&row)
                                                .unwrap_or_else(|_| "null".into());
                                            if is_first
                                                .swap(false, std::sync::atomic::Ordering::Relaxed)
                                            {
                                                json
                                            } else {
                                                format!(",{json}")
                                            }
                                        });

                                        let full_stream = tokio_stream::once("[".to_string())
                                            .chain(row_stream)
                                            .chain(tokio_stream::once("]".to_string()))
                                            .map(Ok::<_, std::convert::Infallible>);

                                        let body = axum::body::Body::from_stream(full_stream);
                                        match axum::response::Response::builder()
                                            .header("content-type", "application/json")
                                            .body(body)
                                        {
                                            Ok(resp) => resp.into_response(),
                                            Err(_) => (
                                                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                                                "Failed to build response",
                                            )
                                                .into_response(),
                                        }
                                    }
                                    Err(e) => {
                                        let msg = format!("Failed to fetch snapshot: {e}");
                                        (axum::http::StatusCode::NOT_FOUND, msg).into_response()
                                    }
                                }
                            }
                        }
                    }),
                )
                .fallback(handler)
                .layer(cors);

            info!("Starting SSE server on {host}:{port} with CORS enabled");
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = server_shutdown_rx.wait_for(|shutdown| *shutdown).await;
                })
                .await
                .context("SSE HTTP server failed")
        });

        // The listener is bound and background tasks are spawned, but may not yet be scheduled.
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("SSE reaction started".to_string()),
            )
            .await;

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let mut lifecycle = self.lifecycle.lock().await;
        if let Some(shutdown) = &lifecycle.shutdown {
            shutdown.send_replace(true);
        }

        let mut task_error = None;
        if let Err(error) = tokio::time::timeout(
            LIFECYCLE_TIMEOUT,
            lifecycle.join_tasks(&self.base.id, &mut task_error),
        )
        .await
        {
            error!(
                "[{}] SSE graceful shutdown timed out: {error}",
                self.base.id
            );
            task_error.get_or_insert(
                anyhow::Error::new(error).context("SSE graceful shutdown timed out"),
            );
            lifecycle.tasks.abort_all();
            if let Err(error) = tokio::time::timeout(
                LIFECYCLE_TIMEOUT,
                lifecycle.join_tasks(&self.base.id, &mut task_error),
            )
            .await
            {
                error!(
                    "[{}] Aborted SSE tasks did not terminate: {error}",
                    self.base.id
                );
            }
        }

        self.base.stop_common().await?;
        // Retain unfinished tasks and the start guard after a timeout so stop() can be retried.
        if lifecycle.tasks.is_empty() {
            lifecycle.shutdown = None;
        }

        if let Some(error) = task_error {
            let error = error.context(format!("Failed to stop SSE reaction '{}'", self.base.id));
            self.base
                .set_status(ComponentStatus::Error, Some(format!("{error:#}")))
                .await;
            return Err(error);
        }

        // Transition to Stopped
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("SSE reaction stopped".to_string()),
            )
            .await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(
        &self,
        result: drasi_lib::channels::QueryResult,
    ) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }

    fn is_durable(&self) -> bool {
        false
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    fn default_recovery_policy(&self) -> drasi_lib::recovery::ReactionRecoveryPolicy {
        drasi_lib::recovery::ReactionRecoveryPolicy::AutoSkipGap
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use crate::tests::sse_reaction_with_reserved_port;
    use drasi_lib::channels::QueryResult;
    use drasi_lib::component_graph::ComponentUpdate;
    use drasi_lib::context::ReactionRuntimeContext;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn test_sse_lifecycle_stop_reports_task_failure_after_releasing_listener() {
        let (mut reaction, reserved_listener) =
            sse_reaction_with_reserved_port("failed-task").await;
        let addr = reserved_listener.local_addr().unwrap();
        // Tokio's interval panics on zero: exercise the actual heartbeat task.
        reaction.config.heartbeat_interval_ms = 0;
        let (update_tx, mut updates) = tokio::sync::mpsc::channel(16);
        reaction
            .initialize(ReactionRuntimeContext::new(
                "test-instance",
                reaction.id(),
                None,
                update_tx,
                None,
            ))
            .await;
        drop(reserved_listener);
        reaction.start().await.unwrap();

        let error = reaction
            .stop()
            .await
            .expect_err("unexpected task failures must be reported");
        assert!(error
            .downcast_ref::<tokio::task::JoinError>()
            .unwrap()
            .is_panic());
        assert_eq!(reaction.status().await, ComponentStatus::Error);
        let mut last_update = None;
        while let Ok(update) = updates.try_recv() {
            last_update = Some(update);
        }
        assert!(matches!(
            last_update,
            Some(ComponentUpdate::Status {
                status: ComponentStatus::Error,
                message: Some(message),
                ..
            }) if message.contains("panicked")
        ));
        assert!(reaction.lifecycle.lock().await.tasks.is_empty());
        assert!(reaction.base.processing_task.read().await.is_none());
        let _probe = TcpListener::bind(addr)
            .await
            .expect("all listeners must be released even when a task failed");
        reaction.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_sse_lifecycle_stop_preserves_returned_task_errors() {
        let (reaction, reserved_listener) = sse_reaction_with_reserved_port("server-error").await;
        let addr = reserved_listener.local_addr().unwrap();
        drop(reserved_listener);
        reaction.start().await.unwrap();
        reaction.lifecycle.lock().await.tasks.spawn(async {
            Err(std::io::Error::new(std::io::ErrorKind::ConnectionAborted, "serve failed").into())
        });

        let error = reaction.stop().await.unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::ConnectionAborted
        );
        assert_eq!(reaction.status().await, ComponentStatus::Error);
        assert!(reaction.lifecycle.lock().await.tasks.is_empty());
        assert!(reaction.base.processing_task.read().await.is_none());
        let _probe = TcpListener::bind(addr).await.unwrap();
    }

    #[tokio::test]
    async fn test_sse_lifecycle_processing_survives_starting_status() {
        let (reaction, reserved_listener) =
            sse_reaction_with_reserved_port("starting-result").await;
        drop(reserved_listener);
        let mut results = reaction.broadcasters.read().await["/events"].subscribe();
        let shutdown_gate = reaction.base.shutdown_tx.write().await;
        let start = reaction.start();
        tokio::pin!(start);
        assert!(futures::poll!(&mut start).is_pending());
        let processing_gate = reaction.base.processing_task.write().await;
        drop(shutdown_gate);
        assert!(futures::poll!(&mut start).is_pending());
        assert_eq!(reaction.status().await, ComponentStatus::Starting);

        reaction
            .enqueue_query_result(QueryResult::new(
                "starting-query".to_string(),
                1,
                chrono::Utc::now(),
                vec![ResultDiff::Add {
                    data: json!({"value": 42}),
                    row_signature: 0,
                }],
                Default::default(),
            ))
            .await
            .unwrap();
        let payload = tokio::time::timeout(Duration::from_secs(5), results.recv())
            .await
            .expect("the processor must not exit while status is Starting")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&payload).unwrap()["queryId"],
            "starting-query"
        );
        drop(processing_gate);
        start.await.unwrap();
        reaction.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_sse_lifecycle_partial_start_requires_cleanup() {
        let (reaction, reserved_listener) = sse_reaction_with_reserved_port("partial-start").await;
        drop(reserved_listener);
        let shutdown_gate = reaction.base.shutdown_tx.write().await;
        let mut start = Box::pin(reaction.start());
        assert!(futures::poll!(&mut start).is_pending());
        drop(start);
        drop(shutdown_gate);

        assert!(reaction.lifecycle.lock().await.tasks.is_empty());
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .to_string()
            .contains("already started"));
        reaction.stop().await.unwrap();
        reaction.start().await.unwrap();
        reaction.stop().await.unwrap();
    }

    #[tokio::test]
    async fn test_sse_lifecycle_shutdown_timeout_retains_unfinished_tasks() {
        let (reaction, reserved_listener) = sse_reaction_with_reserved_port("stuck-task").await;
        let addr = reserved_listener.local_addr().unwrap();
        drop(reserved_listener);
        reaction.start().await.unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        reaction
            .lifecycle
            .lock()
            .await
            .tasks
            .spawn_blocking(move || {
                started_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            });
        started_rx.await.unwrap();

        let error = tokio::time::timeout(Duration::from_secs(15), reaction.stop())
            .await
            .expect("shutdown must remain bounded even if a task cannot be aborted")
            .unwrap_err();
        assert!(format!("{error:#}").contains("shutdown timed out"));
        assert_eq!(reaction.status().await, ComponentStatus::Error);
        assert_eq!(reaction.lifecycle.lock().await.tasks.len(), 1);
        assert!(reaction.start().await.is_err());
        let probe = TcpListener::bind(addr).await.unwrap();
        drop(probe);

        release_tx.send(()).unwrap();
        reaction.stop().await.unwrap();
        assert!(reaction.lifecycle.lock().await.tasks.is_empty());
        reaction.start().await.unwrap();
        reaction.stop().await.unwrap();
    }
}
