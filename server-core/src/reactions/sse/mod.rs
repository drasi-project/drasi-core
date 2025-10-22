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
use log::{debug, error, info, warn};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{broadcast, RwLock};
use tokio_stream::StreamExt;
use tower_http::cors::{Any, CorsLayer};

use crate::channels::priority_queue::PriorityQueue;
use crate::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, QueryResult,
};
use crate::config::ReactionConfig;
use crate::reactions::Reaction;
use crate::server_core::DrasiServerCore;
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
    subscription_tasks: Arc<RwLock<Vec<tokio::task::JoinHandle<()>>>>,
    priority_queue: PriorityQueue<QueryResult>,
    processing_task: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
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
            config: config.clone(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
            host,
            port,
            sse_path,
            heartbeat_interval_ms,
            broadcaster: tx,
            task_handles: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            subscription_tasks: Arc::new(RwLock::new(Vec::new())),
            priority_queue: PriorityQueue::new(config.priority_queue_capacity),
            processing_task: Arc::new(RwLock::new(None)),
        }
    }
}

#[async_trait]
impl Reaction for SseReaction {
    async fn start(&self, server_core: Arc<DrasiServerCore>) -> anyhow::Result<()> {
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

        // Get QueryManager from server_core
        let query_manager = server_core.query_manager();

        // Subscribe to each query and spawn forwarder tasks
        let mut subscription_tasks = self.subscription_tasks.write().await;
        for query_id in &self.config.queries {
            info!("[{}] Subscribing to query '{}'", self.config.id, query_id);

            // Get the query instance
            let query = query_manager
                .get_query_instance(query_id)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("Failed to get query instance '{}': {}", query_id, e)
                })?;

            // Subscribe to it
            let subscription_response =
                query.subscribe(self.config.id.clone()).await.map_err(|e| {
                    anyhow::anyhow!("Failed to subscribe to query '{}': {}", query_id, e)
                })?;

            let mut broadcast_receiver = subscription_response.broadcast_receiver;
            let priority_queue = self.priority_queue.clone();
            let reaction_id = self.config.id.clone();
            let query_id_clone = query_id.clone();

            // Spawn forwarder task
            let forwarder_handle = tokio::spawn(async move {
                info!(
                    "[{}] Forwarder task started for query '{}'",
                    reaction_id, query_id_clone
                );
                loop {
                    match broadcast_receiver.recv().await {
                        Ok(query_result) => {
                            debug!(
                                "[{}] Received result from query '{}', enqueuing to priority queue",
                                reaction_id, query_id_clone
                            );
                            if !priority_queue.enqueue(query_result).await {
                                warn!(
                                    "[{}] Priority queue full, dropping result from query '{}'",
                                    reaction_id, query_id_clone
                                );
                            }
                        }
                        Err(RecvError::Lagged(count)) => {
                            warn!(
                                "[{}] Forwarder lagged by {} messages for query '{}'",
                                reaction_id, count, query_id_clone
                            );
                            continue;
                        }
                        Err(RecvError::Closed) => {
                            info!(
                                "[{}] Broadcast channel closed for query '{}', exiting forwarder",
                                reaction_id, query_id_clone
                            );
                            break;
                        }
                    }
                }
            });

            subscription_tasks.push(forwarder_handle);
        }
        drop(subscription_tasks);

        // Spawn processing task
        let status = Arc::clone(&self.status);
        let broadcaster = self.broadcaster.clone();
        let reaction_id = self.config.id.clone();
        let priority_queue = self.priority_queue.clone();
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
        *self.processing_task.write().await = Some(processing_handle);

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

        // Abort all subscription forwarder tasks
        let mut subscription_tasks = self.subscription_tasks.write().await;
        for handle in subscription_tasks.drain(..) {
            handle.abort();
        }
        drop(subscription_tasks);

        // Abort the processing task
        let mut processing_task = self.processing_task.write().await;
        if let Some(handle) = processing_task.take() {
            handle.abort();
        }
        drop(processing_task);

        // Drain the priority queue
        let drained = self.priority_queue.drain().await;
        if !drained.is_empty() {
            info!(
                "[{}] Drained {} pending results from priority queue",
                self.config.id,
                drained.len()
            );
        }

        // Cancel all other tasks (heartbeat, HTTP server)
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
