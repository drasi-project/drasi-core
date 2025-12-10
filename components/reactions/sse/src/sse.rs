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
use axum::{routing::get, Router};
use log::{debug, error, info};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tower_http::cors::{Any, CorsLayer};

use drasi_lib::channels::{ComponentEventSender, ComponentStatus};
use drasi_lib::managers::log_component_start;
use drasi_lib::plugin_core::{QuerySubscriber, Reaction};
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};

pub use super::config::SseReactionConfig;
use super::SseReactionBuilder;

/// SSE reaction exposes query results to browser clients via Server-Sent Events.
pub struct SseReaction {
    base: ReactionBase,
    config: SseReactionConfig,
    broadcaster: broadcast::Sender<String>,
    task_handles: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
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
        let (tx, _rx) = broadcast::channel(1024);
        Self {
            base: ReactionBase::new(params),
            config,
            broadcaster: tx,
            task_handles: Arc::new(tokio::sync::Mutex::new(Vec::new())),
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

    async fn inject_query_subscriber(&self, query_subscriber: Arc<dyn QuerySubscriber>) {
        self.base.inject_query_subscriber(query_subscriber).await;
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
        // QuerySubscriber was injected via inject_query_subscriber() when reaction was added
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
        let broadcaster = self.broadcaster.clone();
        let reaction_id = self.base.id.clone();
        let priority_queue = self.base.priority_queue.clone();
        let processing_handle = tokio::spawn(async move {
            info!("[{reaction_id}] SSE result processing task started");
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

                let payload = json!({
                    "queryId": query_result.query_id,
                    "results": query_result.results,
                    "timestamp": chrono::Utc::now().timestamp_millis()
                })
                .to_string();

                match broadcaster.send(payload.clone()) {
                    Ok(count) => {
                        info!("[{reaction_id}] Broadcast query result to {count} SSE listeners")
                    }
                    Err(e) => debug!("[{reaction_id}] no SSE listeners: {e}"),
                }
            }
            info!("[{reaction_id}] SSE result processing task ended");
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_handle).await;

        // Heartbeat task
        let hb_tx = self.broadcaster.clone();
        let interval = self.config.heartbeat_interval_ms;
        let hb_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(interval));
            loop {
                ticker.tick().await;
                let beat = json!({"type":"heartbeat","ts": chrono::Utc::now().timestamp_millis()})
                    .to_string();
                let _ = hb_tx.send(beat);
            }
        });
        self.task_handles.lock().await.push(hb_handle);

        // HTTP server task
        let host = self.config.host.clone();
        let port = self.config.port;
        let path = self.config.sse_path.clone();
        let rx_factory = self.broadcaster.clone();
        let server_handle = tokio::spawn(async move {
            // Configure CORS to allow all origins
            let cors = CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::OPTIONS])
                .allow_headers(Any);

            let app = Router::new()
                .route(
                    &path,
                    get(move || async move {
                        let rx = rx_factory.subscribe();
                        let stream = tokio_stream::wrappers::BroadcastStream::new(rx)
                            .filter_map(|res| res.ok())
                            .map(|msg| {
                                Ok::<Event, std::convert::Infallible>(Event::default().data(msg))
                            });
                        Sse::new(stream).keep_alive(
                            KeepAlive::new()
                                .interval(Duration::from_secs(30))
                                .text("keep-alive"),
                        )
                    }),
                )
                .layer(cors);

            info!("Starting SSE server on {host}:{port} path {path} with CORS enabled");
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

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}
