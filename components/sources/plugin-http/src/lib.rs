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

//! HTTP source plugin for Drasi
//!
//! This plugin provides the HTTP source implementation
//! in the Drasi plugin architecture.

pub mod config;
pub use config::HttpSourceConfig;

mod models;

// Export HTTP source models and conversion
pub use models::{convert_http_to_source_change, HttpElement, HttpSourceChange};

use anyhow::Result;
use async_trait::async_trait;
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use log::{debug, error, info, trace};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::timeout;

use drasi_lib::channels::*;
use drasi_lib::plugin_core::Source;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::utils::{AdaptiveBatchConfig, AdaptiveBatcher};

/// Response for event submission
#[derive(Debug, Serialize, Deserialize)]
pub struct EventResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// HTTP source with configurable adaptive batching
pub struct HttpSource {
    base: SourceBase,
    config: HttpSourceConfig,
    adaptive_config: AdaptiveBatchConfig,
}

/// Batch event request that can accept multiple events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchEventRequest {
    pub events: Vec<HttpSourceChange>,
}

/// HTTP source app state with batching channel
#[derive(Clone)]
struct HttpAppState {
    source_id: String,
    batch_tx: mpsc::Sender<SourceChangeEvent>,
}

impl HttpSource {
    /// Create a new HTTP source
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    pub fn new(id: impl Into<String>, config: HttpSourceConfig) -> Result<Self> {
        let id = id.into();
        let params = SourceBaseParams::new(id);

        // Configure adaptive batching
        let mut adaptive_config = AdaptiveBatchConfig::default();

        // Allow overriding adaptive parameters from config
        if let Some(max_batch) = config.adaptive_max_batch_size {
            adaptive_config.max_batch_size = max_batch;
        }
        if let Some(min_batch) = config.adaptive_min_batch_size {
            adaptive_config.min_batch_size = min_batch;
        }
        if let Some(max_wait_ms) = config.adaptive_max_wait_ms {
            adaptive_config.max_wait_time = Duration::from_millis(max_wait_ms);
        }
        if let Some(min_wait_ms) = config.adaptive_min_wait_ms {
            adaptive_config.min_wait_time = Duration::from_millis(min_wait_ms);
        }
        if let Some(window_secs) = config.adaptive_window_secs {
            adaptive_config.throughput_window = Duration::from_secs(window_secs);
        }
        if let Some(enabled) = config.adaptive_enabled {
            adaptive_config.adaptive_enabled = enabled;
        }

        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            adaptive_config,
        })
    }

    /// Create a new HTTP source with custom dispatch settings
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    pub fn with_dispatch(
        id: impl Into<String>,
        config: HttpSourceConfig,
        dispatch_mode: Option<DispatchMode>,
        dispatch_buffer_capacity: Option<usize>,
    ) -> Result<Self> {
        let id = id.into();
        let mut params = SourceBaseParams::new(id);
        if let Some(mode) = dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }

        let mut adaptive_config = AdaptiveBatchConfig::default();

        if let Some(max_batch) = config.adaptive_max_batch_size {
            adaptive_config.max_batch_size = max_batch;
        }
        if let Some(min_batch) = config.adaptive_min_batch_size {
            adaptive_config.min_batch_size = min_batch;
        }
        if let Some(max_wait_ms) = config.adaptive_max_wait_ms {
            adaptive_config.max_wait_time = Duration::from_millis(max_wait_ms);
        }
        if let Some(min_wait_ms) = config.adaptive_min_wait_ms {
            adaptive_config.min_wait_time = Duration::from_millis(min_wait_ms);
        }
        if let Some(window_secs) = config.adaptive_window_secs {
            adaptive_config.throughput_window = Duration::from_secs(window_secs);
        }
        if let Some(enabled) = config.adaptive_enabled {
            adaptive_config.adaptive_enabled = enabled;
        }

        Ok(Self {
            base: SourceBase::new(params)?,
            config,
            adaptive_config,
        })
    }

    async fn handle_single_event(
        Path(source_id): Path<String>,
        State(state): State<HttpAppState>,
        Json(event): Json<HttpSourceChange>,
    ) -> Result<impl IntoResponse, (StatusCode, Json<EventResponse>)> {
        debug!(
            "[{}] HTTP endpoint received single event: {:?}",
            source_id, event
        );
        Self::process_events(&source_id, &state, vec![event]).await
    }

    async fn handle_batch_events(
        Path(source_id): Path<String>,
        State(state): State<HttpAppState>,
        Json(batch): Json<BatchEventRequest>,
    ) -> Result<impl IntoResponse, (StatusCode, Json<EventResponse>)> {
        debug!(
            "[{}] HTTP endpoint received batch of {} events",
            source_id,
            batch.events.len()
        );
        Self::process_events(&source_id, &state, batch.events).await
    }

    async fn process_events(
        source_id: &str,
        state: &HttpAppState,
        events: Vec<HttpSourceChange>,
    ) -> Result<impl IntoResponse, (StatusCode, Json<EventResponse>)> {
        trace!("[{}] Processing {} events", source_id, events.len());

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

        for (idx, event) in events.iter().enumerate() {
            match convert_http_to_source_change(event, source_id) {
                Ok(source_change) => {
                    let change_event = SourceChangeEvent {
                        source_id: source_id.to_string(),
                        change: source_change,
                        timestamp: chrono::Utc::now(),
                    };

                    if let Err(e) = state.batch_tx.send(change_event).await {
                        error!(
                            "[{}] Failed to send event {} to batch channel: {}",
                            state.source_id,
                            idx + 1,
                            e
                        );
                        error_count += 1;
                        last_error = Some("Internal channel error".to_string());
                    } else {
                        success_count += 1;
                    }
                }
                Err(e) => {
                    error!(
                        "[{}] Failed to convert event {}: {}",
                        state.source_id,
                        idx + 1,
                        e
                    );
                    error_count += 1;
                    last_error = Some(e.to_string());
                }
            }
        }

        debug!(
            "[{}] Event processing complete: {} succeeded, {} failed",
            source_id, success_count, error_count
        );

        if error_count > 0 && success_count == 0 {
            Err((
                StatusCode::BAD_REQUEST,
                Json(EventResponse {
                    success: false,
                    message: format!("All {} events failed", error_count),
                    error: last_error,
                }),
            ))
        } else if error_count > 0 {
            Ok(Json(EventResponse {
                success: true,
                message: format!(
                    "Processed {} events successfully, {} failed",
                    success_count, error_count
                ),
                error: last_error,
            }))
        } else {
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
            "service": "http-source",
            "features": ["adaptive-batching", "batch-endpoint"]
        }))
    }

    async fn run_adaptive_batcher(
        batch_rx: mpsc::Receiver<SourceChangeEvent>,
        dispatchers: Arc<
            tokio::sync::RwLock<
                Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>,
            >,
        >,
        adaptive_config: AdaptiveBatchConfig,
        source_id: String,
    ) {
        let mut batcher = AdaptiveBatcher::new(batch_rx, adaptive_config.clone());
        let mut total_events = 0u64;
        let mut total_batches = 0u64;

        info!(
            "[{}] Adaptive HTTP batcher started with config: {:?}",
            source_id, adaptive_config
        );

        while let Some(batch) = batcher.next_batch().await {
            if batch.is_empty() {
                debug!("[{}] Batcher received empty batch, skipping", source_id);
                continue;
            }

            let batch_size = batch.len();
            total_events += batch_size as u64;
            total_batches += 1;

            debug!(
                "[{}] Batcher forwarding batch #{} with {} events to dispatchers",
                source_id, total_batches, batch_size
            );

            let mut sent_count = 0;
            let mut failed_count = 0;
            for (idx, event) in batch.into_iter().enumerate() {
                debug!(
                    "[{}] Batch #{}, dispatching event {}/{}",
                    source_id,
                    total_batches,
                    idx + 1,
                    batch_size
                );

                let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                let wrapper = SourceEventWrapper::with_profiling(
                    event.source_id.clone(),
                    SourceEvent::Change(event.change),
                    event.timestamp,
                    profiling,
                );

                if let Err(e) =
                    SourceBase::dispatch_from_task(dispatchers.clone(), wrapper.clone(), &source_id)
                        .await
                {
                    error!(
                        "[{}] Batch #{}, failed to dispatch event {}/{} (no subscribers): {}",
                        source_id,
                        total_batches,
                        idx + 1,
                        batch_size,
                        e
                    );
                    failed_count += 1;
                } else {
                    debug!(
                        "[{}] Batch #{}, successfully dispatched event {}/{}",
                        source_id,
                        total_batches,
                        idx + 1,
                        batch_size
                    );
                    sent_count += 1;
                }
            }

            debug!(
                "[{}] Batch #{} complete: {} dispatched, {} failed",
                source_id, total_batches, sent_count, failed_count
            );

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
impl Source for HttpSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "http"
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
        if let Some(ref endpoint) = self.config.endpoint {
            props.insert(
                "endpoint".to_string(),
                serde_json::Value::String(endpoint.clone()),
            );
        }
        props
    }

    async fn start(&self) -> Result<()> {
        info!("[{}] Starting adaptive HTTP source", self.base.id);

        self.base.set_status(ComponentStatus::Starting).await;
        self.base
            .send_component_event(
                ComponentStatus::Starting,
                Some("Starting adaptive HTTP source".to_string()),
            )
            .await?;

        let host = self.config.host.clone();
        let port = self.config.port;

        // Create batch channel with capacity based on batch configuration
        let batch_channel_capacity = self.adaptive_config.recommended_channel_capacity();
        let (batch_tx, batch_rx) = mpsc::channel(batch_channel_capacity);
        info!(
            "[{}] HttpSource using batch channel capacity: {} (max_batch_size: {} x 5)",
            self.base.id, batch_channel_capacity, self.adaptive_config.max_batch_size
        );

        // Start adaptive batcher task
        let adaptive_config = self.adaptive_config.clone();
        let source_id = self.base.id.clone();

        info!("[{}] Starting adaptive batcher task", source_id);
        tokio::spawn(Self::run_adaptive_batcher(
            batch_rx,
            self.base.dispatchers.clone(),
            adaptive_config,
            source_id.clone(),
        ));

        // Create app state
        let state = HttpAppState {
            source_id: self.base.id.clone(),
            batch_tx,
        };

        // Build router
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

        let host_clone = host.clone();

        // Start server
        let (error_tx, error_rx) = tokio::sync::oneshot::channel();
        let server_handle = tokio::spawn(async move {
            let addr = format!("{}:{}", host, port);
            info!(
                "[{}] Adaptive HTTP source attempting to bind to {}",
                source_id, addr
            );

            let listener = match tokio::net::TcpListener::bind(&addr).await {
                Ok(listener) => {
                    info!(
                        "[{}] Adaptive HTTP source successfully listening on {}",
                        source_id, addr
                    );
                    listener
                }
                Err(e) => {
                    error!(
                        "[{}] Failed to bind HTTP server to {}: {}",
                        source_id, addr, e
                    );
                    let _ = error_tx.send(format!(
                        "Failed to bind HTTP server to {}: {}. Common causes: port already in use, insufficient permissions",
                        addr, e
                    ));
                    return;
                }
            };

            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(async move {
                    let _ = shutdown_rx.await;
                })
                .await
            {
                error!("[{}] HTTP server error: {}", source_id, e);
            }
        });

        *self.base.task_handle.write().await = Some(server_handle);
        *self.base.shutdown_tx.write().await = Some(shutdown_tx);

        // Check for startup errors with a short timeout
        match timeout(Duration::from_millis(500), error_rx).await {
            Ok(Ok(error_msg)) => {
                self.base.set_status(ComponentStatus::Error).await;
                return Err(anyhow::anyhow!("{}", error_msg));
            }
            _ => {
                self.base.set_status(ComponentStatus::Running).await;
            }
        }

        self.base
            .send_component_event(
                ComponentStatus::Running,
                Some(format!(
                    "Adaptive HTTP source running on {}:{} with batch support",
                    host_clone, port
                )),
            )
            .await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("[{}] Stopping adaptive HTTP source", self.base.id);

        self.base.set_status(ComponentStatus::Stopping).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopping,
                Some("Stopping adaptive HTTP source".to_string()),
            )
            .await?;

        if let Some(tx) = self.base.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }

        if let Some(handle) = self.base.task_handle.write().await.take() {
            let _ = timeout(Duration::from_secs(5), handle).await;
        }

        self.base.set_status(ComponentStatus::Stopped).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopped,
                Some("Adaptive HTTP source stopped".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(
                query_id,
                enable_bootstrap,
                node_labels,
                relation_labels,
                "HTTP",
            )
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}

/// Builder for HTTP source configuration
pub struct HttpSourceBuilder {
    host: String,
    port: u16,
    endpoint: Option<String>,
    timeout_ms: u64,
    adaptive_max_batch_size: Option<usize>,
    adaptive_min_batch_size: Option<usize>,
    adaptive_max_wait_ms: Option<u64>,
    adaptive_min_wait_ms: Option<u64>,
    adaptive_window_secs: Option<u64>,
    adaptive_enabled: Option<bool>,
}

impl Default for HttpSourceBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl HttpSourceBuilder {
    /// Create a new HTTP source builder with default values
    pub fn new() -> Self {
        Self {
            host: String::new(),
            port: 8080,
            endpoint: None,
            timeout_ms: 10000,
            adaptive_max_batch_size: None,
            adaptive_min_batch_size: None,
            adaptive_max_wait_ms: None,
            adaptive_min_wait_ms: None,
            adaptive_window_secs: None,
            adaptive_enabled: None,
        }
    }

    /// Set the HTTP host
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Set the HTTP port
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set the endpoint path
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Set the request timeout in milliseconds
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Set the adaptive batching maximum batch size
    pub fn with_adaptive_max_batch_size(mut self, size: usize) -> Self {
        self.adaptive_max_batch_size = Some(size);
        self
    }

    /// Set the adaptive batching minimum batch size
    pub fn with_adaptive_min_batch_size(mut self, size: usize) -> Self {
        self.adaptive_min_batch_size = Some(size);
        self
    }

    /// Set the adaptive batching maximum wait time in milliseconds
    pub fn with_adaptive_max_wait_ms(mut self, wait_ms: u64) -> Self {
        self.adaptive_max_wait_ms = Some(wait_ms);
        self
    }

    /// Set the adaptive batching minimum wait time in milliseconds
    pub fn with_adaptive_min_wait_ms(mut self, wait_ms: u64) -> Self {
        self.adaptive_min_wait_ms = Some(wait_ms);
        self
    }

    /// Set the adaptive batching throughput window in seconds
    pub fn with_adaptive_window_secs(mut self, secs: u64) -> Self {
        self.adaptive_window_secs = Some(secs);
        self
    }

    /// Enable or disable adaptive batching
    pub fn with_adaptive_enabled(mut self, enabled: bool) -> Self {
        self.adaptive_enabled = Some(enabled);
        self
    }

    /// Build the HTTP source configuration
    pub fn build(self) -> HttpSourceConfig {
        HttpSourceConfig {
            host: self.host,
            port: self.port,
            endpoint: self.endpoint,
            timeout_ms: self.timeout_ms,
            adaptive_max_batch_size: self.adaptive_max_batch_size,
            adaptive_min_batch_size: self.adaptive_min_batch_size,
            adaptive_max_wait_ms: self.adaptive_max_wait_ms,
            adaptive_min_wait_ms: self.adaptive_min_wait_ms,
            adaptive_window_secs: self.adaptive_window_secs,
            adaptive_enabled: self.adaptive_enabled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_builder_defaults() {
        let config = HttpSourceBuilder::new().build();
        assert_eq!(config.port, 8080);
        assert_eq!(config.timeout_ms, 10000);
        assert_eq!(config.endpoint, None);
    }

    #[test]
    fn test_http_builder_custom_values() {
        let config = HttpSourceBuilder::new()
            .with_host("api.example.com")
            .with_port(9000)
            .with_endpoint("/webhook")
            .with_timeout_ms(5000)
            .build();

        assert_eq!(config.host, "api.example.com");
        assert_eq!(config.port, 9000);
        assert_eq!(config.endpoint, Some("/webhook".to_string()));
        assert_eq!(config.timeout_ms, 5000);
    }

    #[test]
    fn test_http_builder_adaptive_batching() {
        let config = HttpSourceBuilder::new()
            .with_host("localhost")
            .with_adaptive_max_batch_size(1000)
            .with_adaptive_min_batch_size(10)
            .with_adaptive_max_wait_ms(500)
            .with_adaptive_min_wait_ms(50)
            .with_adaptive_window_secs(60)
            .with_adaptive_enabled(true)
            .build();

        assert_eq!(config.adaptive_max_batch_size, Some(1000));
        assert_eq!(config.adaptive_min_batch_size, Some(10));
        assert_eq!(config.adaptive_max_wait_ms, Some(500));
        assert_eq!(config.adaptive_min_wait_ms, Some(50));
        assert_eq!(config.adaptive_window_secs, Some(60));
        assert_eq!(config.adaptive_enabled, Some(true));
    }
}
