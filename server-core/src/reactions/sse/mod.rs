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
use tokio::sync::{broadcast, RwLock};
use tokio_stream::StreamExt;
use tower_http::cors::{Any, CorsLayer};

use crate::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, QueryResultReceiver,
};
use crate::config::ReactionConfig;
use crate::reactions::Reaction;
use crate::utils::log_component_start;

/// SSE reaction exposes query results to browser clients via Server-Sent Events.
pub struct SseReaction {
    config: ReactionConfig,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: ComponentEventSender,
    host: String,
    port: u16,
    sse_path: String,
    heartbeat_interval_ms: u64,
    broadcaster: broadcast::Sender<String>,
    task_handles: Arc<tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>>,
}

impl SseReaction {
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Self {
        let host = config
            .properties
            .get("host")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0")
            .to_string();
        let port = config
            .properties
            .get("port")
            .and_then(|v| v.as_u64())
            .unwrap_or(50051) as u16;
        let sse_path = config
            .properties
            .get("sse_path")
            .and_then(|v| v.as_str())
            .unwrap_or("/events")
            .to_string();
        let heartbeat_interval_ms = config
            .properties
            .get("heartbeat_interval_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(15_000);
        let (tx, _rx) = broadcast::channel(1024);
        Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
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
    async fn start(&self, mut result_rx: QueryResultReceiver) -> anyhow::Result<()> {
        log_component_start("SSE Reaction", &self.config.id);
        *self.status.write().await = ComponentStatus::Starting;
        let _ = self
            .event_tx
            .send(ComponentEvent {
                component_id: self.config.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Starting,
                timestamp: chrono::Utc::now(),
                message: Some("Starting SSE reaction".into()),
            })
            .await;
        *self.status.write().await = ComponentStatus::Running;
        let _ = self
            .event_tx
            .send(ComponentEvent {
                component_id: self.config.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Running,
                timestamp: chrono::Utc::now(),
                message: Some("SSE reaction started".into()),
            })
            .await;

        let status = Arc::clone(&self.status);
        let broadcaster = self.broadcaster.clone();
        let reaction_id = self.config.id.clone();
        let queries = self.config.queries.clone();
        let handle = tokio::spawn(async move {
            info!("[{}] SSE result processing loop started", reaction_id);
            while let Some(query_result) = result_rx.recv().await {
                if !matches!(*status.read().await, ComponentStatus::Running) {
                    info!("[{}] SSE reaction not running, breaking loop", reaction_id);
                    break;
                }
                if !queries.contains(&query_result.query_id) {
                    debug!(
                        "[{}] Ignoring result from non-subscribed query '{}'",
                        reaction_id, query_result.query_id
                    );
                    continue;
                }
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
            info!("[{}] SSE result loop ended - channel closed", reaction_id);
        });

        // Store the task handle to keep it alive
        self.task_handles.lock().await.push(handle);

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
        *self.status.write().await = ComponentStatus::Stopping;
        let _ = self
            .event_tx
            .send(ComponentEvent {
                component_id: self.config.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Stopping,
                timestamp: chrono::Utc::now(),
                message: Some("Stopping SSE reaction".into()),
            })
            .await;

        // Cancel all tasks
        let mut handles = self.task_handles.lock().await;
        for handle in handles.drain(..) {
            handle.abort();
        }

        *self.status.write().await = ComponentStatus::Stopped;
        let _ = self
            .event_tx
            .send(ComponentEvent {
                component_id: self.config.id.clone(),
                component_type: ComponentType::Reaction,
                status: ComponentStatus::Stopped,
                timestamp: chrono::Utc::now(),
                message: Some("SSE reaction stopped".into()),
            })
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }
    fn get_config(&self) -> &ReactionConfig {
        &self.config
    }
}
