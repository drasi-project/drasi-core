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

//! HTTP Source Plugin for Drasi
//!
//! This plugin exposes HTTP endpoints for receiving data change events. It includes
//! adaptive batching for optimized throughput and supports both single-event and
//! batch submission modes.
//!
//! # Endpoints
//!
//! The HTTP source exposes the following endpoints:
//!
//! - **`POST /sources/{source_id}/events`** - Submit a single event
//! - **`POST /sources/{source_id}/events/batch`** - Submit multiple events
//! - **`GET /health`** - Health check endpoint
//!
//! # Data Format
//!
//! Events are submitted as JSON using the `HttpSourceChange` format:
//!
//! ## Insert Operation
//!
//! ```json
//! {
//!     "operation": "insert",
//!     "element": {
//!         "type": "node",
//!         "id": "user-123",
//!         "labels": ["User"],
//!         "properties": {
//!             "name": "Alice",
//!             "email": "alice@example.com"
//!         }
//!     },
//!     "timestamp": 1699900000000000000
//! }
//! ```
//!
//! ## Update Operation
//!
//! ```json
//! {
//!     "operation": "update",
//!     "element": {
//!         "type": "node",
//!         "id": "user-123",
//!         "labels": ["User"],
//!         "properties": {
//!             "name": "Alice Updated"
//!         }
//!     }
//! }
//! ```
//!
//! ## Delete Operation
//!
//! ```json
//! {
//!     "operation": "delete",
//!     "id": "user-123",
//!     "labels": ["User"]
//! }
//! ```
//!
//! ## Relation Element
//!
//! ```json
//! {
//!     "operation": "insert",
//!     "element": {
//!         "type": "relation",
//!         "id": "follows-1",
//!         "labels": ["FOLLOWS"],
//!         "from": "user-123",
//!         "to": "user-456",
//!         "properties": {}
//!     }
//! }
//! ```
//!
//! # Batch Submission
//!
//! ```json
//! {
//!     "events": [
//!         { "operation": "insert", ... },
//!         { "operation": "update", ... }
//!     ]
//! }
//! ```
//!
//! # Adaptive Batching
//!
//! The HTTP source includes adaptive batching to optimize throughput. Events are
//! buffered and dispatched in batches, with batch size and timing adjusted based
//! on throughput patterns.
//!
//! | Parameter | Default | Description |
//! |-----------|---------|-------------|
//! | `adaptive_enabled` | `true` | Enable/disable adaptive batching |
//! | `adaptive_max_batch_size` | `1000` | Maximum events per batch |
//! | `adaptive_min_batch_size` | `1` | Minimum events per batch |
//! | `adaptive_max_wait_ms` | `100` | Maximum wait time before dispatching |
//! | `adaptive_min_wait_ms` | `10` | Minimum wait time between batches |
//!
//! # Configuration
//!
//! | Field | Type | Default | Description |
//! |-------|------|---------|-------------|
//! | `host` | string | *required* | Host address to bind to |
//! | `port` | u16 | `8080` | Port to listen on |
//! | `endpoint` | string | None | Optional custom path prefix |
//! | `timeout_ms` | u64 | `10000` | Request timeout in milliseconds |
//!
//! # Example Configuration (YAML)
//!
//! ```yaml
//! source_type: http
//! properties:
//!   host: "0.0.0.0"
//!   port: 8080
//!   adaptive_enabled: true
//!   adaptive_max_batch_size: 500
//! ```
//!
//! # Usage Examples
//!
//! ## Rust
//!
//! ```rust,ignore
//! use drasi_source_http::{HttpSource, HttpSourceBuilder};
//!
//! let config = HttpSourceBuilder::new()
//!     .with_host("0.0.0.0")
//!     .with_port(8080)
//!     .with_adaptive_enabled(true)
//!     .build();
//!
//! let source = Arc::new(HttpSource::new("http-source", config)?);
//! drasi.add_source(source).await?;
//! ```
//!
//! ## curl (Single Event)
//!
//! ```bash
//! curl -X POST http://localhost:8080/sources/my-source/events \
//!   -H "Content-Type: application/json" \
//!   -d '{"operation":"insert","element":{"type":"node","id":"1","labels":["Test"],"properties":{}}}'
//! ```
//!
//! ## curl (Batch)
//!
//! ```bash
//! curl -X POST http://localhost:8080/sources/my-source/events/batch \
//!   -H "Content-Type: application/json" \
//!   -d '{"events":[...]}'
//! ```

pub mod config;
pub use config::HttpSourceConfig;

mod adaptive_batcher;
mod models;
mod time;

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

use crate::adaptive_batcher::{AdaptiveBatchConfig, AdaptiveBatcher};

/// Response for event submission
#[derive(Debug, Serialize, Deserialize)]
pub struct EventResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// HTTP source with configurable adaptive batching.
///
/// This source exposes HTTP endpoints for receiving data change events.
/// It supports both single-event and batch submission modes, with adaptive
/// batching for optimized throughput.
///
/// # Fields
///
/// - `base`: Common source functionality (dispatchers, status, lifecycle)
/// - `config`: HTTP-specific configuration (host, port, timeout)
/// - `adaptive_config`: Adaptive batching settings for throughput optimization
pub struct HttpSource {
    /// Base source implementation providing common functionality
    base: SourceBase,
    /// HTTP source configuration
    config: HttpSourceConfig,
    /// Adaptive batching configuration for throughput optimization
    adaptive_config: AdaptiveBatchConfig,
}

/// Batch event request that can accept multiple events
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchEventRequest {
    pub events: Vec<HttpSourceChange>,
}

/// HTTP source app state with batching channel.
///
/// Shared state passed to Axum route handlers.
#[derive(Clone)]
struct HttpAppState {
    /// The source ID for validation against incoming requests
    source_id: String,
    /// Channel for sending events to the adaptive batcher
    batch_tx: mpsc::Sender<SourceChangeEvent>,
}

impl HttpSource {
    /// Create a new HTTP source.
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance
    /// * `config` - HTTP source configuration
    ///
    /// # Returns
    ///
    /// A new `HttpSource` instance, or an error if construction fails.
    ///
    /// # Errors
    ///
    /// Returns an error if the base source cannot be initialized.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_http::{HttpSource, HttpSourceBuilder};
    ///
    /// let config = HttpSourceBuilder::new()
    ///     .with_host("0.0.0.0")
    ///     .with_port(8080)
    ///     .build();
    ///
    /// let source = HttpSource::new("my-http-source", config)?;
    /// ```
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

    /// Create a new HTTP source with custom dispatch settings.
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance
    /// * `config` - HTTP source configuration
    /// * `dispatch_mode` - Optional dispatch mode (Channel, Direct, etc.)
    /// * `dispatch_buffer_capacity` - Optional buffer capacity for channel dispatch
    ///
    /// # Returns
    ///
    /// A new `HttpSource` instance with custom dispatch settings.
    ///
    /// # Errors
    ///
    /// Returns an error if the base source cannot be initialized.
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

    /// Handle a single event submission from `POST /sources/{source_id}/events`.
    ///
    /// Validates the source ID matches this source and converts the HTTP event
    /// to a source change before sending to the adaptive batcher.
    async fn handle_single_event(
        Path(source_id): Path<String>,
        State(state): State<HttpAppState>,
        Json(event): Json<HttpSourceChange>,
    ) -> Result<impl IntoResponse, (StatusCode, Json<EventResponse>)> {
        debug!("[{source_id}] HTTP endpoint received single event: {event:?}");
        Self::process_events(&source_id, &state, vec![event]).await
    }

    /// Handle a batch event submission from `POST /sources/{source_id}/events/batch`.
    ///
    /// Validates the source ID and processes all events in the batch,
    /// returning partial success if some events fail.
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

    /// Process a list of events, converting and sending them to the batcher.
    ///
    /// # Arguments
    ///
    /// * `source_id` - Source ID from the request path
    /// * `state` - Shared app state containing the batch channel
    /// * `events` - List of HTTP source changes to process
    ///
    /// # Returns
    ///
    /// Success response with count of processed events, or error if all fail.
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
            "[{source_id}] Event processing complete: {success_count} succeeded, {error_count} failed"
        );

        if error_count > 0 && success_count == 0 {
            Err((
                StatusCode::BAD_REQUEST,
                Json(EventResponse {
                    success: false,
                    message: format!("All {error_count} events failed"),
                    error: last_error,
                }),
            ))
        } else if error_count > 0 {
            Ok(Json(EventResponse {
                success: true,
                message: format!(
                    "Processed {success_count} events successfully, {error_count} failed"
                ),
                error: last_error,
            }))
        } else {
            Ok(Json(EventResponse {
                success: true,
                message: format!("All {success_count} events processed successfully"),
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
                Vec<
                    Box<
                        dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync,
                    >,
                >,
            >,
        >,
        adaptive_config: AdaptiveBatchConfig,
        source_id: String,
    ) {
        let mut batcher = AdaptiveBatcher::new(batch_rx, adaptive_config.clone());
        let mut total_events = 0u64;
        let mut total_batches = 0u64;

        info!("[{source_id}] Adaptive HTTP batcher started with config: {adaptive_config:?}");

        while let Some(batch) = batcher.next_batch().await {
            if batch.is_empty() {
                debug!("[{source_id}] Batcher received empty batch, skipping");
                continue;
            }

            let batch_size = batch.len();
            total_events += batch_size as u64;
            total_batches += 1;

            debug!(
                "[{source_id}] Batcher forwarding batch #{total_batches} with {batch_size} events to dispatchers"
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
                "[{source_id}] Batch #{total_batches} complete: {sent_count} dispatched, {failed_count} failed"
            );

            if total_batches.is_multiple_of(100) {
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
            "[{source_id}] Adaptive HTTP batcher stopped - Total batches: {total_batches}, Total events: {total_events}"
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

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
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

        info!("[{source_id}] Starting adaptive batcher task");
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
            let addr = format!("{host}:{port}");
            info!("[{source_id}] Adaptive HTTP source attempting to bind to {addr}");

            let listener = match tokio::net::TcpListener::bind(&addr).await {
                Ok(listener) => {
                    info!("[{source_id}] Adaptive HTTP source successfully listening on {addr}");
                    listener
                }
                Err(e) => {
                    error!("[{source_id}] Failed to bind HTTP server to {addr}: {e}");
                    let _ = error_tx.send(format!(
                        "Failed to bind HTTP server to {addr}: {e}. Common causes: port already in use, insufficient permissions"
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
                error!("[{source_id}] HTTP server error: {e}");
            }
        });

        *self.base.task_handle.write().await = Some(server_handle);
        *self.base.shutdown_tx.write().await = Some(shutdown_tx);

        // Check for startup errors with a short timeout
        match timeout(Duration::from_millis(500), error_rx).await {
            Ok(Ok(error_msg)) => {
                self.base.set_status(ComponentStatus::Error).await;
                return Err(anyhow::anyhow!("{error_msg}"));
            }
            _ => {
                self.base.set_status(ComponentStatus::Running).await;
            }
        }

        self.base
            .send_component_event(
                ComponentStatus::Running,
                Some(format!(
                    "Adaptive HTTP source running on {host_clone}:{port} with batch support"
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
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(
                &settings,
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

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

/// Builder for HttpSource instances.
///
/// Provides a fluent API for constructing HTTP sources with sensible defaults
/// and adaptive batching settings. The builder takes the source ID at construction
/// and returns a fully constructed `HttpSource` from `build()`.
///
/// # Example
///
/// ```rust,ignore
/// use drasi_source_http::HttpSource;
///
/// let source = HttpSource::builder("my-source")
///     .with_host("0.0.0.0")
///     .with_port(8080)
///     .with_adaptive_enabled(true)
///     .with_bootstrap_provider(my_provider)
///     .build()?;
/// ```
pub struct HttpSourceBuilder {
    id: String,
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
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl HttpSourceBuilder {
    /// Create a new HTTP source builder with the given source ID.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the source instance
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
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
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
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

    /// Set the dispatch mode for event routing.
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Set the dispatch buffer capacity.
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the bootstrap provider for initial data delivery.
    pub fn with_bootstrap_provider(
        mut self,
        provider: impl drasi_lib::bootstrap::BootstrapProvider + 'static,
    ) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    /// Set whether this source should auto-start when DrasiLib starts.
    ///
    /// Default is `true`. Set to `false` if this source should only be
    /// started manually via `start_source()`.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: HttpSourceConfig) -> Self {
        self.host = config.host;
        self.port = config.port;
        self.endpoint = config.endpoint;
        self.timeout_ms = config.timeout_ms;
        self.adaptive_max_batch_size = config.adaptive_max_batch_size;
        self.adaptive_min_batch_size = config.adaptive_min_batch_size;
        self.adaptive_max_wait_ms = config.adaptive_max_wait_ms;
        self.adaptive_min_wait_ms = config.adaptive_min_wait_ms;
        self.adaptive_window_secs = config.adaptive_window_secs;
        self.adaptive_enabled = config.adaptive_enabled;
        self
    }

    /// Build the HttpSource instance.
    ///
    /// # Returns
    ///
    /// A fully constructed `HttpSource`, or an error if construction fails.
    pub fn build(self) -> Result<HttpSource> {
        let config = HttpSourceConfig {
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
        };

        // Build SourceBaseParams with all settings
        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        // Configure adaptive batching
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

        Ok(HttpSource {
            base: SourceBase::new(params)?,
            config,
            adaptive_config,
        })
    }
}

impl HttpSource {
    /// Create a builder for HttpSource with the given ID.
    ///
    /// This is the recommended way to construct an HttpSource.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for the source instance
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// let source = HttpSource::builder("my-source")
    ///     .with_host("0.0.0.0")
    ///     .with_port(8080)
    ///     .with_bootstrap_provider(my_provider)
    ///     .build()?;
    /// ```
    pub fn builder(id: impl Into<String>) -> HttpSourceBuilder {
        HttpSourceBuilder::new(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod construction {
        use super::*;

        #[test]
        fn test_builder_with_valid_config() {
            let source = HttpSourceBuilder::new("test-source")
                .with_host("localhost")
                .with_port(8080)
                .build();
            assert!(source.is_ok());
        }

        #[test]
        fn test_builder_with_custom_config() {
            let source = HttpSourceBuilder::new("http-source")
                .with_host("0.0.0.0")
                .with_port(9000)
                .with_endpoint("/events")
                .build()
                .unwrap();
            assert_eq!(source.id(), "http-source");
        }

        #[test]
        fn test_with_dispatch_creates_source() {
            let config = HttpSourceConfig {
                host: "localhost".to_string(),
                port: 8080,
                endpoint: None,
                timeout_ms: 10000,
                adaptive_max_batch_size: None,
                adaptive_min_batch_size: None,
                adaptive_max_wait_ms: None,
                adaptive_min_wait_ms: None,
                adaptive_window_secs: None,
                adaptive_enabled: None,
            };
            let source = HttpSource::with_dispatch(
                "dispatch-source",
                config,
                Some(DispatchMode::Channel),
                Some(1000),
            );
            assert!(source.is_ok());
            assert_eq!(source.unwrap().id(), "dispatch-source");
        }
    }

    mod properties {
        use super::*;

        #[test]
        fn test_id_returns_correct_value() {
            let source = HttpSourceBuilder::new("my-http-source")
                .with_host("localhost")
                .build()
                .unwrap();
            assert_eq!(source.id(), "my-http-source");
        }

        #[test]
        fn test_type_name_returns_http() {
            let source = HttpSourceBuilder::new("test")
                .with_host("localhost")
                .build()
                .unwrap();
            assert_eq!(source.type_name(), "http");
        }

        #[test]
        fn test_properties_contains_host_and_port() {
            let source = HttpSourceBuilder::new("test")
                .with_host("192.168.1.1")
                .with_port(9000)
                .build()
                .unwrap();
            let props = source.properties();

            assert_eq!(
                props.get("host"),
                Some(&serde_json::Value::String("192.168.1.1".to_string()))
            );
            assert_eq!(
                props.get("port"),
                Some(&serde_json::Value::Number(9000.into()))
            );
        }

        #[test]
        fn test_properties_includes_endpoint_when_set() {
            let source = HttpSourceBuilder::new("test")
                .with_host("localhost")
                .with_endpoint("/api/v1")
                .build()
                .unwrap();
            let props = source.properties();

            assert_eq!(
                props.get("endpoint"),
                Some(&serde_json::Value::String("/api/v1".to_string()))
            );
        }

        #[test]
        fn test_properties_excludes_endpoint_when_none() {
            let source = HttpSourceBuilder::new("test")
                .with_host("localhost")
                .build()
                .unwrap();
            let props = source.properties();

            assert!(!props.contains_key("endpoint"));
        }
    }

    mod lifecycle {
        use super::*;

        #[tokio::test]
        async fn test_initial_status_is_stopped() {
            let source = HttpSourceBuilder::new("test")
                .with_host("localhost")
                .build()
                .unwrap();
            assert_eq!(source.status().await, ComponentStatus::Stopped);
        }
    }

    mod builder {
        use super::*;

        #[test]
        fn test_http_builder_defaults() {
            let source = HttpSourceBuilder::new("test").build().unwrap();
            assert_eq!(source.config.port, 8080);
            assert_eq!(source.config.timeout_ms, 10000);
            assert_eq!(source.config.endpoint, None);
        }

        #[test]
        fn test_http_builder_custom_values() {
            let source = HttpSourceBuilder::new("test")
                .with_host("api.example.com")
                .with_port(9000)
                .with_endpoint("/webhook")
                .with_timeout_ms(5000)
                .build()
                .unwrap();

            assert_eq!(source.config.host, "api.example.com");
            assert_eq!(source.config.port, 9000);
            assert_eq!(source.config.endpoint, Some("/webhook".to_string()));
            assert_eq!(source.config.timeout_ms, 5000);
        }

        #[test]
        fn test_http_builder_adaptive_batching() {
            let source = HttpSourceBuilder::new("test")
                .with_host("localhost")
                .with_adaptive_max_batch_size(1000)
                .with_adaptive_min_batch_size(10)
                .with_adaptive_max_wait_ms(500)
                .with_adaptive_min_wait_ms(50)
                .with_adaptive_window_secs(60)
                .with_adaptive_enabled(true)
                .build()
                .unwrap();

            assert_eq!(source.config.adaptive_max_batch_size, Some(1000));
            assert_eq!(source.config.adaptive_min_batch_size, Some(10));
            assert_eq!(source.config.adaptive_max_wait_ms, Some(500));
            assert_eq!(source.config.adaptive_min_wait_ms, Some(50));
            assert_eq!(source.config.adaptive_window_secs, Some(60));
            assert_eq!(source.config.adaptive_enabled, Some(true));
        }

        #[test]
        fn test_builder_id() {
            let source = HttpSource::builder("my-http-source")
                .with_host("localhost")
                .build()
                .unwrap();

            assert_eq!(source.base.id, "my-http-source");
        }
    }

    mod event_conversion {
        use super::*;

        #[test]
        fn test_convert_node_insert() {
            let mut props = serde_json::Map::new();
            props.insert(
                "name".to_string(),
                serde_json::Value::String("Alice".to_string()),
            );
            props.insert("age".to_string(), serde_json::Value::Number(30.into()));

            let http_change = HttpSourceChange::Insert {
                element: HttpElement::Node {
                    id: "user-1".to_string(),
                    labels: vec!["User".to_string()],
                    properties: props,
                },
                timestamp: Some(1234567890000000000),
            };

            let result = convert_http_to_source_change(&http_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Insert { element } => match element {
                    drasi_core::models::Element::Node {
                        metadata,
                        properties,
                    } => {
                        assert_eq!(metadata.reference.element_id.as_ref(), "user-1");
                        assert_eq!(metadata.labels.len(), 1);
                        assert_eq!(metadata.effective_from, 1234567890000000000);
                        assert!(properties.get("name").is_some());
                        assert!(properties.get("age").is_some());
                    }
                    _ => panic!("Expected Node element"),
                },
                _ => panic!("Expected Insert operation"),
            }
        }

        #[test]
        fn test_convert_relation_insert() {
            let http_change = HttpSourceChange::Insert {
                element: HttpElement::Relation {
                    id: "follows-1".to_string(),
                    labels: vec!["FOLLOWS".to_string()],
                    from: "user-1".to_string(),
                    to: "user-2".to_string(),
                    properties: serde_json::Map::new(),
                },
                timestamp: None,
            };

            let result = convert_http_to_source_change(&http_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Insert { element } => match element {
                    drasi_core::models::Element::Relation {
                        metadata,
                        out_node,
                        in_node,
                        ..
                    } => {
                        assert_eq!(metadata.reference.element_id.as_ref(), "follows-1");
                        assert_eq!(out_node.element_id.as_ref(), "user-1");
                        assert_eq!(in_node.element_id.as_ref(), "user-2");
                    }
                    _ => panic!("Expected Relation element"),
                },
                _ => panic!("Expected Insert operation"),
            }
        }

        #[test]
        fn test_convert_delete() {
            let http_change = HttpSourceChange::Delete {
                id: "user-1".to_string(),
                labels: Some(vec!["User".to_string()]),
                timestamp: Some(9999999999),
            };

            let result = convert_http_to_source_change(&http_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Delete { metadata } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "user-1");
                    assert_eq!(metadata.labels.len(), 1);
                }
                _ => panic!("Expected Delete operation"),
            }
        }

        #[test]
        fn test_convert_update() {
            let http_change = HttpSourceChange::Update {
                element: HttpElement::Node {
                    id: "user-1".to_string(),
                    labels: vec!["User".to_string()],
                    properties: serde_json::Map::new(),
                },
                timestamp: None,
            };

            let result = convert_http_to_source_change(&http_change, "test-source");
            assert!(result.is_ok());

            match result.unwrap() {
                drasi_core::models::SourceChange::Update { .. } => {
                    // Success
                }
                _ => panic!("Expected Update operation"),
            }
        }
    }

    mod adaptive_config {
        use super::*;

        #[test]
        fn test_adaptive_config_from_http_config() {
            let source = HttpSourceBuilder::new("test")
                .with_host("localhost")
                .with_adaptive_max_batch_size(500)
                .with_adaptive_enabled(true)
                .build()
                .unwrap();

            // The adaptive config should be initialized from the http config
            assert_eq!(source.adaptive_config.max_batch_size, 500);
            assert!(source.adaptive_config.adaptive_enabled);
        }

        #[test]
        fn test_adaptive_config_uses_defaults_when_not_specified() {
            let source = HttpSourceBuilder::new("test")
                .with_host("localhost")
                .build()
                .unwrap();

            // Should use AdaptiveBatchConfig defaults
            let default_config = AdaptiveBatchConfig::default();
            assert_eq!(
                source.adaptive_config.max_batch_size,
                default_config.max_batch_size
            );
            assert_eq!(
                source.adaptive_config.min_batch_size,
                default_config.min_batch_size
            );
        }
    }
}
