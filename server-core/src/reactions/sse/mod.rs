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
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tower_http::cors::{Any, CorsLayer};

use crate::channels::{ComponentEventSender, ComponentStatus};
use crate::config::ReactionConfig;
use crate::reactions::base::ReactionBase;
use crate::reactions::Reaction;
use crate::server_core::DrasiServerCore;
use crate::utils::log_component_start;

/// SSE reaction exposes query results to browser clients via Server-Sent Events.
pub struct SseReaction {
    base: ReactionBase,
    host: String,
    port: u16,
    sse_path: String,
    heartbeat_interval_ms: u64,
    broadcaster: broadcast::Sender<String>,
    task_handles: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl SseReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        let (host, port, sse_path, heartbeat_interval_ms) = match &config.config {
            crate::config::ReactionSpecificConfig::Sse(sse_config) => (
                sse_config.host.clone(),
                sse_config.port,
                sse_config.sse_path.clone(),
                sse_config.heartbeat_interval_ms,
            ),
            _ => ("0.0.0.0".to_string(), 50051, "/events".to_string(), 15_000),
        };
        let (tx, _rx) = broadcast::channel(1024);
        Self {
            base: ReactionBase::new(config, event_tx),
            host,
            port,
            sse_path,
            heartbeat_interval_ms,
            broadcaster: tx,
            task_handles: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        }
    }
}

#[async_trait]
impl Reaction for SseReaction {
    async fn start(&self, server_core: Arc<DrasiServerCore>) -> anyhow::Result<()> {
        log_component_start("SSE Reaction", &self.base.config.id);

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting SSE reaction".to_string()),
            )
            .await?;

        // Subscribe to all configured queries using ReactionBase
        self.base.subscribe_to_queries(server_core).await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("SSE reaction started".to_string()),
            )
            .await?;

        // Spawn processing task
        let status = self.base.status.clone();
        let broadcaster = self.broadcaster.clone();
        let reaction_id = self.base.config.id.clone();
        let priority_queue = self.base.priority_queue.clone();
        let processing_handle = tokio::spawn(async move {
            info!("[{}] SSE result processing task started", reaction_id);
            loop {
                if !matches!(*status.read().await, ComponentStatus::Running) {
                    info!("[{}] SSE reaction not running, breaking loop", reaction_id);
                    break;
                }

                // Dequeue the next result in timestamp order (blocking)
                let query_result = priority_queue.dequeue().await;

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
                    Ok(count) => info!(
                        "[{}] Broadcast query result to {} SSE listeners",
                        reaction_id, count
                    ),
                    Err(e) => debug!("[{}] no SSE listeners: {}", reaction_id, e),
                }
            }
            info!("[{}] SSE result processing task ended", reaction_id);
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_handle).await;

        // Heartbeat task
        let hb_tx = self.broadcaster.clone();
        let interval = self.heartbeat_interval_ms;
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
        let host = self.host.clone();
        let port = self.port;
        let path = self.sse_path.clone();
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

            info!(
                "Starting SSE server on {}:{} path {} with CORS enabled",
                host, port, path
            );
            let listener = match tokio::net::TcpListener::bind((host.as_str(), port)).await {
                Ok(l) => l,
                Err(e) => {
                    error!("Failed to bind SSE server: {}", e);
                    return;
                }
            };
            if let Err(e) = axum::serve(listener, app).await {
                error!("SSE server error: {}", e);
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
    fn get_config(&self) -> &ReactionConfig {
        &self.base.config
    }
}
