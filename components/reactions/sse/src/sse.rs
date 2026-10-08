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
use log::{debug, error, info, warn};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, watch, Mutex, RwLock};
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tower_http::cors::{Any, CorsLayer};

use drasi_lib::channels::{ComponentStatus, ResultDiff};
use drasi_lib::context::workers::{
    cancel_owned_worker, join_owned_worker_gracefully, spawn_owned_worker, WorkerAlreadyOwned,
    WorkerCompletion,
};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::{Reaction, SnapshotFetcher};

pub use super::config::SseReactionConfig;
use super::SseReactionBuilder;

const BROADCAST_CHANNEL_CAPACITY: usize = 1024;

fn subscriber_events(
    receiver: broadcast::Receiver<String>,
    path: String,
    mut shutdown: watch::Receiver<bool>,
) -> impl tokio_stream::Stream<
    Item = Result<Event, tokio_stream::wrappers::errors::BroadcastStreamRecvError>,
> {
    let events = tokio_stream::wrappers::BroadcastStream::new(receiver)
        .take_while(move |result| match result {
            Ok(_) => true,
            Err(error) => {
                warn!("Closing lagged SSE subscriber on {path}: {error}; reconnect and fetch a fresh snapshot");
                false
            }
        })
        .map(|result| result.map(|message| Event::default().data(message)));
    futures::StreamExt::take_until(events, async move {
        let _ = shutdown.wait_for(|stopped| *stopped).await;
    })
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
    lifecycle: Mutex<()>,
    heartbeat_task: RwLock<Option<JoinHandle<()>>>,
    server_task: RwLock<Option<JoinHandle<std::io::Result<()>>>>,
    shutdown_tx: watch::Sender<bool>,
}

impl std::fmt::Debug for SseReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseReaction")
            .field("id", &self.base.id)
            .field("config", &self.config)
            .field("broadcasters", &"<broadcasters>")
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
            lifecycle: Mutex::new(()),
            heartbeat_task: RwLock::new(None),
            server_task: RwLock::new(None),
            shutdown_tx: watch::channel(false).0,
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
        let _lifecycle = self.lifecycle.lock().await;
        if self.server_task.read().await.is_some()
            || self.heartbeat_task.read().await.is_some()
            || self.base.processing_task.read().await.is_some()
            || self.shutdown_tx.receiver_count() != 0
        {
            return Err(WorkerAlreadyOwned.into());
        }
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
        let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
            Ok(listener) => listener,
            Err(error) => {
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!(
                            "Failed to bind SSE server on {host}:{port}: {error}"
                        )),
                    )
                    .await;
                return Err(error).context(format!("failed to bind SSE server on {host}:{port}"));
            }
        };
        self.shutdown_tx.send_replace(false);
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("SSE reaction started".to_string()),
            )
            .await;

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
        spawn_owned_worker(&self.base.processing_task, async move {
            info!("[{reaction_id}] SSE result processing task started");

            let mut handlebars = Handlebars::new();

            // Register the json helper to serialize values as JSON
            super::register_json_helper(&mut handlebars);

            loop {
                if !matches!(status_handle.get_status().await, ComponentStatus::Running) {
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
        })
        .await?;

        // Heartbeat task - sends to all paths
        let broadcasters_hb = self.broadcasters.clone();
        let interval = self.config.heartbeat_interval_ms;
        spawn_owned_worker(&self.heartbeat_task, async move {
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
        })
        .await?;

        // HTTP server task - dynamically creates routes for all paths
        let broadcasters_server = self.broadcasters.clone();
        let mut server_shutdown = self.shutdown_tx.subscribe();
        let subscriber_shutdown = self.shutdown_tx.subscribe();
        let server_status = self.base.status_handle();

        // Get snapshot_fetcher from the runtime context for the /snapshot/:query_id endpoint
        let snapshot_fetcher = self
            .base
            .context()
            .await
            .and_then(|ctx| ctx.snapshot_fetcher.clone());

        spawn_owned_worker(&self.server_task, async move {
            // Configure CORS to allow all origins
            let cors = CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::OPTIONS])
                .allow_headers(Any);

            // Create a handler that checks for matching paths dynamically
            let broadcasters_clone = broadcasters_server.clone();
            let handler = get(move |req: axum::http::Request<axum::body::Body>| {
                let broadcasters = broadcasters_clone.clone();
                let shutdown = subscriber_shutdown.clone();
                async move {
                    let path = req.uri().path().to_string();

                    // Try to find a broadcaster for this path
                    let broadcaster = {
                        let broadcasters_read = broadcasters.read().await;
                        broadcasters_read.get(&path).cloned()
                    };

                    if let Some(broadcaster) = broadcaster {
                        let rx = broadcaster.subscribe();
                        let stream = subscriber_events(rx, path, shutdown);
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
            let result = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = server_shutdown.wait_for(|stopped| *stopped).await;
                })
                .await;
            if let Err(error) = &result {
                error!("SSE server error: {error}");
                server_status
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("SSE server failed: {error}")),
                    )
                    .await;
            }
            result
        })
        .await?;

        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        let _lifecycle = self.lifecycle.lock().await;
        self.shutdown_tx.send_replace(true);
        cancel_owned_worker(
            &mut *self.heartbeat_task.write().await,
            Duration::from_secs(5),
        )
        .await
        .with_context(|| format!("SSE '{}' heartbeat cleanup", self.base.id))?;
        if let WorkerCompletion::Completed(result) = join_owned_worker_gracefully(
            &mut *self.server_task.write().await,
            Duration::from_secs(5),
        )
        .await
        .with_context(|| format!("SSE '{}' server cleanup", self.base.id))?
        {
            result.with_context(|| format!("SSE '{}' server failed", self.base.id))?;
        }
        tokio::time::timeout(Duration::from_secs(5), self.shutdown_tx.closed())
            .await
            .with_context(|| format!("SSE '{}' connection cleanup", self.base.id))?;
        self.base.stop_common().await
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
mod subscriber_stream_tests {
    use super::*;

    #[tokio::test]
    async fn lag_closes_the_subscriber_instead_of_sending_a_truncated_tail() {
        let (sender, receiver) = broadcast::channel(2);
        let (_shutdown, shutdown_rx) = watch::channel(false);
        let stream = subscriber_events(receiver, "/events/test".into(), shutdown_rx);
        tokio::pin!(stream);
        sender.send("first".into()).unwrap();
        assert!(stream.next().await.unwrap().is_ok());
        for event in ["second", "third", "fourth"] {
            sender.send(event.into()).unwrap();
        }
        assert!(
            stream.next().await.is_none(),
            "a lagged subscriber must reconnect and resnapshot"
        );
        let _ = sender.send("later".into());
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn healthy_subscribers_receive_ordered_events_and_close_normally() {
        let (sender, receiver) = broadcast::channel(2);
        sender.send("first".into()).unwrap();
        sender.send("second".into()).unwrap();
        drop(sender);
        let (_shutdown, shutdown_rx) = watch::channel(false);
        let response = Sse::new(subscriber_events(
            receiver,
            "/events/test".into(),
            shutdown_rx,
        ))
        .into_response();
        let body = axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap();
        assert_eq!(
            std::str::from_utf8(&body).unwrap(),
            "data: first\n\ndata: second\n\n"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn shutdown_closes_idle_and_buffered_subscribers_before_more_events() {
        for buffered in [false, true] {
            let (sender, receiver) = broadcast::channel(2);
            let (shutdown, shutdown_rx) = watch::channel(false);
            let stream = subscriber_events(receiver, "/events/test".into(), shutdown_rx);
            tokio::pin!(stream);
            if buffered {
                sender.send("not delivered after shutdown".into()).unwrap();
            }
            shutdown.send_replace(true);
            assert!(stream.next().await.is_none());
            assert!(stream.next().await.is_none());
        }
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use drasi_lib::context::workers::WorkerCleanupError;
    use tokio::sync::Notify;

    #[tokio::test(flavor = "current_thread")]
    async fn bind_failure_is_typed_and_never_starts_workers() -> anyhow::Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let reaction = SseReaction::builder("occupied")
            .with_host("127.0.0.1")
            .with_port(listener.local_addr()?.port())
            .build()?;
        let error = reaction.start().await.unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<std::io::Error>()
                .expect("I/O cause")
                .kind(),
            std::io::ErrorKind::AddrInUse
        );
        assert_eq!(reaction.status().await, ComponentStatus::Error);
        assert!(reaction.base.processing_task.read().await.is_none());
        assert!(reaction.heartbeat_task.read().await.is_none());
        assert!(reaction.server_task.read().await.is_none());
        drop(listener);
        reaction.start().await?;
        reaction.stop().await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_cleanup_retains_worker_and_connection_owners() -> anyhow::Result<()> {
        for connection_only in [false, true] {
            let reaction = SseReaction::builder("cancelled").with_port(0).build()?;
            let release = Arc::new(Notify::new());
            let connection = if connection_only {
                Some(reaction.shutdown_tx.subscribe())
            } else {
                let drain = release.clone();
                spawn_owned_worker(&reaction.server_task, async move {
                    drain.notified().await;
                    Ok(())
                })
                .await?;
                None
            };
            {
                let stop = reaction.stop();
                tokio::pin!(stop);
                tokio::select! {
                    result = &mut stop => panic!("cleanup is still incomplete: {result:?}"),
                    _ = tokio::time::sleep(Duration::from_millis(10)) => {}
                }
            }
            assert_eq!(
                reaction.server_task.read().await.is_some(),
                !connection_only
            );
            assert!(reaction
                .start()
                .await
                .unwrap_err()
                .downcast_ref::<WorkerAlreadyOwned>()
                .is_some());
            release.notify_one();
            drop(connection);
            reaction.stop().await?;
            reaction.start().await?;
            reaction.stop().await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_failures_preserve_io_and_panic_causes() -> anyhow::Result<()> {
        for panic in [false, true] {
            let reaction = SseReaction::builder("failed").with_port(0).build()?;
            spawn_owned_worker(&reaction.server_task, async move {
                assert!(!panic, "injected SSE server panic");
                Err(std::io::Error::new(
                    std::io::ErrorKind::ConnectionReset,
                    "injected server failure",
                ))
            })
            .await?;
            let error = reaction.stop().await.unwrap_err();
            if panic {
                assert!(
                    matches!(error.downcast_ref(), Some(WorkerCleanupError::Join(error)) if error.is_panic())
                );
            } else {
                assert_eq!(
                    error
                        .downcast_ref::<std::io::Error>()
                        .expect("I/O cause")
                        .kind(),
                    std::io::ErrorKind::ConnectionReset
                );
            }
            assert!(reaction.server_task.read().await.is_none());
            reaction.stop().await?;
        }
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn server_timeout_keeps_the_actual_http_request_owned_until_it_finishes(
    ) -> anyhow::Result<()> {
        let reaction = SseReaction::builder("slow-request").with_port(0).build()?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mut shutdown = reaction.shutdown_tx.subscribe();
        let app = Router::new().route(
            "/",
            get({
                let entered = entered.clone();
                let release = release.clone();
                move || {
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        entered.notify_one();
                        release.notified().await;
                        "finished"
                    }
                }
            }),
        );
        spawn_owned_worker(&reaction.server_task, async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown.wait_for(|stopped| *stopped).await;
                })
                .await
        })
        .await?;
        reaction
            .base
            .set_status(ComponentStatus::Running, None)
            .await;
        let request = tokio::spawn(async move {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(12))
                .build()?
                .get(format!("http://{address}/"))
                .send()
                .await?
                .text()
                .await
        });
        tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
        let error = reaction.stop().await.unwrap_err();
        assert!(
            matches!(error.downcast_ref(), Some(WorkerCleanupError::TimedOut { timeout }) if *timeout == Duration::from_secs(5))
        );
        assert_eq!(reaction.status().await, ComponentStatus::Running);
        assert!(reaction.server_task.read().await.is_some());
        assert!(reaction
            .start()
            .await
            .unwrap_err()
            .downcast_ref::<WorkerAlreadyOwned>()
            .is_some());
        assert!(!request.is_finished());
        release.notify_one();
        assert_eq!(request.await??, "finished");
        reaction.stop().await?;
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
        let rebound = tokio::net::TcpListener::bind(address).await?;
        drop(rebound);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn restarts_close_live_streams_and_release_the_same_listener() -> anyhow::Result<()> {
        let reservation = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let port = reservation.local_addr()?.port();
        drop(reservation);
        let reaction = SseReaction::builder("restart")
            .with_host("127.0.0.1")
            .with_port(port)
            .with_heartbeat_interval_ms(5)
            .build()?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(2))
            .build()?;
        reaction.stop().await?;
        for _ in 0..3 {
            reaction.start().await?;
            assert_eq!(
                client
                    .get(format!("http://127.0.0.1:{port}/snapshot/query"))
                    .send()
                    .await?
                    .status(),
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            );
            let response = client
                .get(format!("http://127.0.0.1:{port}/events"))
                .send()
                .await?;
            assert_eq!(response.status(), axum::http::StatusCode::OK);
            let mut events = response.bytes_stream();
            let first = tokio::time::timeout(Duration::from_secs(2), events.next())
                .await?
                .expect("live stream")?;
            assert!(std::str::from_utf8(&first)?.contains("heartbeat"));
            reaction.stop().await?;
            assert!(reaction.base.processing_task.read().await.is_none());
            assert!(reaction.heartbeat_task.read().await.is_none());
            assert!(reaction.server_task.read().await.is_none());
            assert_eq!(reaction.shutdown_tx.receiver_count(), 0);
            tokio::time::timeout(Duration::from_secs(2), async {
                while let Some(event) = events.next().await {
                    event?;
                }
                Ok::<_, anyhow::Error>(())
            })
            .await??;
            let rebound = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
            drop(rebound);
        }
        reaction.stop().await?;
        Ok(())
    }
}
