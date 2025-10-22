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

use anyhow::Result;
use async_trait::async_trait;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use log::{debug, error, info};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tokio::time::timeout;

use crate::bootstrap::{BootstrapContext, BootstrapProviderFactory, BootstrapRequest};
use crate::channels::*;
use crate::config::SourceConfig;
use crate::sources::Source;
use crate::utils::{AdaptiveBatchConfig, AdaptiveBatcher};

use super::{
    direct_format::{convert_direct_to_source_change, DirectSourceChange},
    EventResponse,
};

/// Adaptive HTTP source that batches incoming events
pub struct AdaptiveHttpSource {
    config: SourceConfig,
    status: Arc<RwLock<ComponentStatus>>,
    broadcast_tx: SourceBroadcastSender,
    event_tx: ComponentEventSender,
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    shutdown_tx: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
    // Adaptive batching configuration
    adaptive_config: AdaptiveBatchConfig,
}

/// Batch event request that can accept multiple events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchEventRequest {
    pub events: Vec<DirectSourceChange>,
}

/// Enhanced app state with batching channel
#[derive(Clone)]
struct AdaptiveAppState {
    source_id: String,
    // Channel for batching events
    batch_tx: mpsc::Sender<SourceChangeEvent>,
}

impl AdaptiveHttpSource {
    pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Self {
        // Configure adaptive batching
        let mut adaptive_config = AdaptiveBatchConfig::default();

        // Allow overriding adaptive parameters from config
        if let Some(max_batch) = config
            .properties
            .get("adaptive_max_batch_size")
            .and_then(|v| v.as_u64())
        {
            adaptive_config.max_batch_size = max_batch as usize;
        }
        if let Some(min_batch) = config
            .properties
            .get("adaptive_min_batch_size")
            .and_then(|v| v.as_u64())
        {
            adaptive_config.min_batch_size = min_batch as usize;
        }
        if let Some(max_wait_ms) = config
            .properties
            .get("adaptive_max_wait_ms")
            .and_then(|v| v.as_u64())
        {
            adaptive_config.max_wait_time = Duration::from_millis(max_wait_ms);
        }
        if let Some(min_wait_ms) = config
            .properties
            .get("adaptive_min_wait_ms")
            .and_then(|v| v.as_u64())
        {
            adaptive_config.min_wait_time = Duration::from_millis(min_wait_ms);
        }
        if let Some(window_secs) = config
            .properties
            .get("adaptive_window_secs")
            .and_then(|v| v.as_u64())
        {
            adaptive_config.throughput_window = Duration::from_secs(window_secs);
        }

        // Check if adaptive mode is explicitly disabled
        if let Some(enabled) = config
            .properties
            .get("adaptive_enabled")
            .and_then(|v| v.as_bool())
        {
            adaptive_config.adaptive_enabled = enabled;
        }

        let (broadcast_tx, _) = tokio::sync::broadcast::channel(1000);

        Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            broadcast_tx,
            event_tx,
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
            adaptive_config,
        }
    }

    async fn handle_single_event(
        Path(source_id): Path<String>,
        State(state): State<AdaptiveAppState>,
        Json(event): Json<DirectSourceChange>,
    ) -> Result<impl IntoResponse, (StatusCode, Json<EventResponse>)> {
        Self::process_events(&source_id, &state, vec![event]).await
    }

    async fn handle_batch_events(
        Path(source_id): Path<String>,
        State(state): State<AdaptiveAppState>,
        Json(batch): Json<BatchEventRequest>,
    ) -> Result<impl IntoResponse, (StatusCode, Json<EventResponse>)> {
        Self::process_events(&source_id, &state, batch.events).await
    }

    async fn process_events(
        source_id: &str,
        state: &AdaptiveAppState,
        events: Vec<DirectSourceChange>,
    ) -> Result<impl IntoResponse, (StatusCode, Json<EventResponse>)> {
        // Validate source name matches
        if source_id != state.source_id {
            error!(
                "[{}] Source name mismatch. Expected '{}', got '{}'",
                state.source_id, state.source_id, source_id
            );
            return Err((
                StatusCode::BAD_REQUEST,
                Json(EventResponse {
                    success: false,
                    message: "Source name mismatch".to_string(),
                    error: Some(format!(
                        "Expected source '{}', got '{}'",
                        state.source_id, source_id
                    )),
                }),
            ));
        }

        let mut success_count = 0;
        let mut error_count = 0;
        let mut last_error = None;

        // Process each event
        for event in events {
            match convert_direct_to_source_change(&event, source_id) {
                Ok(source_change) => {
                    let change_event = SourceChangeEvent {
                        source_id: source_id.to_string(),
                        change: source_change,
                        timestamp: chrono::Utc::now(),
                    };

                    // Send to batch channel
                    if let Err(e) = state.batch_tx.send(change_event).await {
                        error!(
                            "[{}] Failed to send to batch channel: {}",
                            state.source_id, e
                        );
                        error_count += 1;
                        last_error = Some("Internal channel error".to_string());
                    } else {
                        success_count += 1;
                    }
                }
                Err(e) => {
                    error!("[{}] Invalid event data: {}", state.source_id, e);
                    error_count += 1;
                    last_error = Some(e.to_string());
                }
            }
        }

        if error_count > 0 && success_count == 0 {
            // All events failed
            Err((
                StatusCode::BAD_REQUEST,
                Json(EventResponse {
                    success: false,
                    message: format!("All {} events failed", error_count),
                    error: last_error,
                }),
            ))
        } else if error_count > 0 {
            // Partial success
            Ok(Json(EventResponse {
                success: true,
                message: format!(
                    "Processed {} events successfully, {} failed",
                    success_count, error_count
                ),
                error: last_error,
            }))
        } else {
            // Complete success
            Ok(Json(EventResponse {
                success: true,
                message: format!("All {} events processed successfully", success_count),
                error: None,
            }))
        }
    }

    async fn health_check() -> impl IntoResponse {
        Json(serde_json::json!({
            "status": "healthy",
            "service": "adaptive-http-source",
            "features": ["adaptive-batching", "batch-endpoint"]
        }))
    }

    async fn run_adaptive_batcher(
        batch_rx: mpsc::Receiver<SourceChangeEvent>,
        broadcast_tx: SourceBroadcastSender,
        adaptive_config: AdaptiveBatchConfig,
        source_id: String,
    ) {
        let mut batcher = AdaptiveBatcher::new(batch_rx, adaptive_config);
        let mut total_events = 0u64;
        let mut total_batches = 0u64;

        info!("[{}] Adaptive HTTP batcher started", source_id);

        while let Some(batch) = batcher.next_batch().await {
            if batch.is_empty() {
                continue;
            }

            let batch_size = batch.len();
            total_events += batch_size as u64;
            total_batches += 1;

            debug!(
                "[{}] Processing adaptive batch of {} events",
                source_id, batch_size
            );

            // Send all events in the batch
            for event in batch {
                // Create profiling metadata with timestamps
                let mut profiling = crate::profiling::ProfilingMetadata::new();
                profiling.source_send_ns = Some(crate::profiling::timestamp_ns());

                let wrapper = SourceEventWrapper::with_profiling(
                    event.source_id.clone(),
                    SourceEvent::Change(event.change),
                    event.timestamp,
                    profiling,
                );

                // Broadcast to new architecture (Arc-wrapped for zero-copy)
                let arc_wrapper = Arc::new(wrapper.clone());
                if let Err(e) = broadcast_tx.send(arc_wrapper) {
                    debug!(
                        "[{}] Failed to broadcast (no subscribers): {}",
                        source_id, e
                    );
                }

                // Also send to legacy mpsc channel for backward compatibility
                if let Err(e) = broadcast_tx.send(Arc::new(wrapper)) {
                    error!("[{}] Failed to send batched event: {}", source_id, e);
                }
            }

            if total_batches % 100 == 0 {
                info!(
                    "[{}] Adaptive HTTP metrics - Batches: {}, Events: {}, Avg batch size: {:.1}",
                    source_id,
                    total_batches,
                    total_events,
                    total_events as f64 / total_batches as f64
                );
            }
        }

        info!(
            "[{}] Adaptive HTTP batcher stopped - Total batches: {}, Total events: {}",
            source_id, total_batches, total_events
        );
    }
}

#[async_trait]
impl Source for AdaptiveHttpSource {
    async fn start(&self) -> Result<()> {
        info!("[{}] Starting adaptive HTTP source", self.config.id);

        *self.status.write().await = ComponentStatus::Starting;

        // Send start event
        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting adaptive HTTP source".to_string()),
        };
        self.event_tx.send(event).await?;

        // Extract configuration
        let host = self
            .config
            .properties
            .get("host")
            .and_then(|v| v.as_str())
            .unwrap_or("0.0.0.0")
            .to_string();
        let port = self
            .config
            .properties
            .get("port")
            .and_then(|v| v.as_u64())
            .unwrap_or(8080) as u16;

        // Create batch channel
        let (batch_tx, batch_rx) = mpsc::channel(1000);

        // Start adaptive batcher task        let broadcast_tx = self.broadcast_tx.clone();
        let adaptive_config = self.adaptive_config.clone();
        let source_id = self.config.id.clone();

        tokio::spawn(Self::run_adaptive_batcher(
            batch_rx,
            self.broadcast_tx.clone(),
            adaptive_config,
            source_id.clone(),
        ));

        // Create app state
        let state = AdaptiveAppState {
            source_id: self.config.id.clone(),
            batch_tx,
        };

        // Build router with both single and batch endpoints
        let app = Router::new()
            .route("/health", get(Self::health_check))
            .route(
                "/sources/:source_id/events",
                post(Self::handle_single_event),
            )
            .route(
                "/sources/:source_id/events/batch",
                post(Self::handle_batch_events),
            )
            .with_state(state);

        // Create shutdown channel
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();

        // Clone for the message
        let host_clone = host.clone();

        // Start server
        let server_handle = tokio::spawn(async move {
            let addr = format!("{}:{}", host, port);
            info!("[{}] Adaptive HTTP source listening on {}", source_id, addr);

            let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
            axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        *self.task_handle.write().await = Some(server_handle);
        *self.shutdown_tx.write().await = Some(shutdown_tx);
        *self.status.write().await = ComponentStatus::Running;

        // Send running event
        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: Some(format!(
                "Adaptive HTTP source running on {}:{} with batch support",
                host_clone, port
            )),
        };
        self.event_tx.send(event).await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping adaptive HTTP source", self.config.id);

        *self.status.write().await = ComponentStatus::Stopping;

        // Send stop event
        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Stopping,
            timestamp: chrono::Utc::now(),
            message: Some("Stopping adaptive HTTP source".to_string()),
        };
        self.event_tx.send(event).await?;

        // Send shutdown signal
        if let Some(shutdown_tx) = self.shutdown_tx.write().await.take() {
            let _ = shutdown_tx.send(());
        }

        // Wait for server to stop
        if let Some(handle) = self.task_handle.write().await.take() {
            let _ = timeout(Duration::from_secs(5), handle).await;
        }

        *self.status.write().await = ComponentStatus::Stopped;

        // Send stopped event
        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: Some("Adaptive HTTP source stopped".to_string()),
        };
        self.event_tx.send(event).await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &SourceConfig {
        &self.config
    }

    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse> {
        info!(
            "Query '{}' subscribing to HTTP source '{}' (bootstrap: {})",
            query_id, self.config.id, enable_bootstrap
        );

        let broadcast_receiver = self.broadcast_tx.subscribe();

        // Clone query_id for later use since it will be moved into async block
        let query_id_for_response = query_id.clone();

        // Handle bootstrap if requested and bootstrap provider is configured
        let bootstrap_receiver = if enable_bootstrap {
            if let Some(provider_config) = &self.config.bootstrap_provider {
                info!(
                    "Bootstrap enabled for query '{}' with {} node labels and {} relation labels, delegating to bootstrap provider",
                    query_id,
                    node_labels.len(),
                    relation_labels.len()
                );

                let (tx, rx) = tokio::sync::mpsc::channel(1000);

                // Create bootstrap provider
                let provider = BootstrapProviderFactory::create_provider(provider_config)?;

                // Create bootstrap context
                let context = BootstrapContext::new(
                    self.config.id.clone(), // server_id (using source_id as placeholder)
                    Arc::new(self.config.clone()),
                    self.config.id.clone(),
                );

                // Create bootstrap request
                let request = BootstrapRequest {
                    query_id: query_id.clone(),
                    node_labels,
                    relation_labels,
                    request_id: format!("{}-{}", query_id, uuid::Uuid::new_v4()),
                };

                // Spawn bootstrap task
                tokio::spawn(async move {
                    match provider.bootstrap(request, &context, tx).await {
                        Ok(count) => {
                            info!(
                                "Bootstrap completed successfully for query '{}', sent {} events",
                                query_id, count
                            );
                        }
                        Err(e) => {
                            error!("Bootstrap failed for query '{}': {}", query_id, e);
                        }
                    }
                });

                Some(rx)
            } else {
                info!(
                    "Bootstrap requested for query '{}' but no bootstrap provider configured for source '{}'",
                    query_id, self.config.id
                );
                None
            }
        } else {
            None
        };

        Ok(SubscriptionResponse {
            query_id: query_id_for_response,
            source_id: self.config.id.clone(),
            broadcast_receiver,
            bootstrap_receiver,
        })
    }

    fn get_broadcast_receiver(&self) -> Result<SourceBroadcastReceiver> {
        Ok(self.broadcast_tx.subscribe())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
