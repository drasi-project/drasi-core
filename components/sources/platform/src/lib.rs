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

//! Platform Source Plugin for Drasi
//!
//! This plugin consumes data change events from Redis Streams, which is the primary
//! integration point for the Drasi platform. It supports CloudEvent-wrapped messages
//! containing node and relation changes, as well as control events for query subscriptions.
//!
//! # Architecture
//!
//! The platform source connects to Redis as a consumer group member, enabling:
//! - **At-least-once delivery**: Messages are acknowledged after processing
//! - **Horizontal scaling**: Multiple consumers can share the workload
//! - **Fault tolerance**: Unacknowledged messages are redelivered
//!
//! # Configuration
//!
//! | Field | Type | Default | Description |
//! |-------|------|---------|-------------|
//! | `redis_url` | string | *required* | Redis connection URL (e.g., `redis://localhost:6379`) |
//! | `stream_key` | string | *required* | Redis stream key to consume from |
//! | `consumer_group` | string | `"drasi-core"` | Consumer group name |
//! | `consumer_name` | string | auto-generated | Unique consumer name within the group |
//! | `batch_size` | usize | `100` | Number of events to read per XREADGROUP call |
//! | `block_ms` | u64 | `5000` | Milliseconds to block waiting for events |
//!
//! # Data Format
//!
//! The platform source expects CloudEvent-wrapped messages with a `data` array
//! containing change events. Each event includes an operation type and payload.
//!
//! ## Node Insert
//!
//! ```json
//! {
//!     "data": [{
//!         "op": "i",
//!         "payload": {
//!             "after": {
//!                 "id": "user-123",
//!                 "labels": ["User"],
//!                 "properties": {
//!                     "name": "Alice",
//!                     "email": "alice@example.com"
//!                 }
//!             },
//!             "source": {
//!                 "db": "mydb",
//!                 "table": "node",
//!                 "ts_ns": 1699900000000000000
//!             }
//!         }
//!     }]
//! }
//! ```
//!
//! ## Node Update
//!
//! ```json
//! {
//!     "data": [{
//!         "op": "u",
//!         "payload": {
//!             "after": {
//!                 "id": "user-123",
//!                 "labels": ["User"],
//!                 "properties": { "name": "Alice Updated" }
//!             },
//!             "source": { "table": "node", "ts_ns": 1699900001000000000 }
//!         }
//!     }]
//! }
//! ```
//!
//! ## Node Delete
//!
//! ```json
//! {
//!     "data": [{
//!         "op": "d",
//!         "payload": {
//!             "before": {
//!                 "id": "user-123",
//!                 "labels": ["User"],
//!                 "properties": {}
//!             },
//!             "source": { "table": "node", "ts_ns": 1699900002000000000 }
//!         }
//!     }]
//! }
//! ```
//!
//! ## Relation Insert
//!
//! ```json
//! {
//!     "data": [{
//!         "op": "i",
//!         "payload": {
//!             "after": {
//!                 "id": "follows-1",
//!                 "labels": ["FOLLOWS"],
//!                 "startId": "user-123",
//!                 "endId": "user-456",
//!                 "properties": { "since": "2024-01-01" }
//!             },
//!             "source": { "table": "rel", "ts_ns": 1699900003000000000 }
//!         }
//!     }]
//! }
//! ```
//!
//! # Control Events
//!
//! Control events are identified by `payload.source.db = "Drasi"` (case-insensitive).
//! Currently supported control types:
//!
//! - **SourceSubscription**: Query subscription management
//!
//! # Example Configuration (YAML)
//!
//! ```yaml
//! source_type: platform
//! properties:
//!   redis_url: "redis://localhost:6379"
//!   stream_key: "my-app-changes"
//!   consumer_group: "drasi-consumers"
//!   batch_size: 50
//!   block_ms: 10000
//! ```
//!
//! # Usage Example
//!
//! ```rust,ignore
//! use drasi_source_platform::{PlatformSource, PlatformSourceBuilder};
//! use std::sync::Arc;
//!
//! let config = PlatformSourceBuilder::new()
//!     .with_redis_url("redis://localhost:6379")
//!     .with_stream_key("my-changes")
//!     .with_consumer_group("my-consumers")
//!     .build();
//!
//! let source = Arc::new(PlatformSource::new("platform-source", config)?);
//! drasi.add_source(source).await?;
//! ```

pub mod config;
pub use config::PlatformSourceConfig;

use anyhow::Result;
use log::{debug, error, info, warn};
use redis::streams::StreamReadReply;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_lib::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, ControlOperation,
    DispatchMode, SourceControl, SourceEvent, SourceEventWrapper, SubscriptionResponse,
};
use drasi_lib::plugin_core::Source;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::sources::manager::convert_json_to_element_properties;

#[cfg(test)]
mod tests;

/// Configuration for the platform source
#[derive(Debug, Clone)]
struct PlatformConfig {
    /// Redis connection URL
    redis_url: String,
    /// Redis stream key to read from
    stream_key: String,
    /// Consumer group name
    consumer_group: String,
    /// Consumer name (should be unique per instance)
    consumer_name: String,
    /// Number of events to read per XREADGROUP call
    batch_size: usize,
    /// Milliseconds to block waiting for events
    block_ms: u64,
    /// Stream position to start from (">" for new, "0" for all)
    start_id: String,
    /// Always recreate consumer group on startup (default: false)
    /// If true, deletes and recreates the consumer group using start_id
    /// If false, uses existing group position (ignores start_id if group exists)
    always_create_consumer_group: bool,
    /// Maximum connection retry attempts
    max_retries: usize,
    /// Delay between retries in milliseconds
    retry_delay_ms: u64,
}

impl Default for PlatformConfig {
    fn default() -> Self {
        Self {
            redis_url: String::new(),
            stream_key: String::new(),
            consumer_group: String::new(),
            consumer_name: String::new(),
            batch_size: 10,
            block_ms: 5000,
            start_id: ">".to_string(),
            always_create_consumer_group: false,
            max_retries: 3,
            retry_delay_ms: 1000,
        }
    }
}

/// Platform source that reads events from Redis Streams.
///
/// This source connects to a Redis instance and consumes CloudEvent-wrapped
/// messages from a stream using consumer groups. It supports both data events
/// (node/relation changes) and control events (query subscriptions).
///
/// # Fields
///
/// - `base`: Common source functionality (dispatchers, status, lifecycle)
/// - `config`: Platform-specific configuration (Redis connection, stream settings)
pub struct PlatformSource {
    /// Base source implementation providing common functionality
    base: SourceBase,
    /// Platform source configuration
    config: PlatformSourceConfig,
}

/// Builder for creating [`PlatformSource`] instances.
///
/// Provides a fluent API for constructing platform sources
/// with sensible defaults.
///
/// # Example
///
/// ```rust,ignore
/// use drasi_source_platform::PlatformSource;
///
/// let source = PlatformSource::builder("my-platform-source")
///     .with_redis_url("redis://localhost:6379")
///     .with_stream_key("my-app-changes")
///     .with_consumer_group("my-consumers")
///     .with_batch_size(50)
///     .build()?;
/// ```
pub struct PlatformSourceBuilder {
    id: String,
    redis_url: String,
    stream_key: String,
    consumer_group: Option<String>,
    consumer_name: Option<String>,
    batch_size: Option<usize>,
    block_ms: Option<u64>,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl PlatformSourceBuilder {
    /// Create a new builder with the given ID and default values.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            redis_url: String::new(),
            stream_key: String::new(),
            consumer_group: None,
            consumer_name: None,
            batch_size: None,
            block_ms: None,
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    /// Set the Redis connection URL.
    ///
    /// # Arguments
    ///
    /// * `url` - Redis connection URL (e.g., `redis://localhost:6379`)
    pub fn with_redis_url(mut self, url: impl Into<String>) -> Self {
        self.redis_url = url.into();
        self
    }

    /// Set the Redis stream key to consume from.
    ///
    /// # Arguments
    ///
    /// * `key` - Name of the Redis stream
    pub fn with_stream_key(mut self, key: impl Into<String>) -> Self {
        self.stream_key = key.into();
        self
    }

    /// Set the consumer group name.
    ///
    /// # Arguments
    ///
    /// * `group` - Consumer group name (default: `"drasi-core"`)
    pub fn with_consumer_group(mut self, group: impl Into<String>) -> Self {
        self.consumer_group = Some(group.into());
        self
    }

    /// Set a unique consumer name within the group.
    ///
    /// # Arguments
    ///
    /// * `name` - Unique consumer name (auto-generated if not specified)
    pub fn with_consumer_name(mut self, name: impl Into<String>) -> Self {
        self.consumer_name = Some(name.into());
        self
    }

    /// Set the batch size for reading events.
    ///
    /// # Arguments
    ///
    /// * `size` - Number of events to read per XREADGROUP call (default: 100)
    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = Some(size);
        self
    }

    /// Set the block timeout for waiting on events.
    ///
    /// # Arguments
    ///
    /// * `ms` - Milliseconds to block waiting for events (default: 5000)
    pub fn with_block_ms(mut self, ms: u64) -> Self {
        self.block_ms = Some(ms);
        self
    }

    /// Set the dispatch mode for this source
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Set the dispatch buffer capacity for this source
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the bootstrap provider for this source
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
    pub fn with_config(mut self, config: PlatformSourceConfig) -> Self {
        self.redis_url = config.redis_url;
        self.stream_key = config.stream_key;
        self.consumer_group = Some(config.consumer_group);
        self.consumer_name = config.consumer_name;
        self.batch_size = Some(config.batch_size);
        self.block_ms = Some(config.block_ms);
        self
    }

    /// Build the platform source.
    ///
    /// # Errors
    ///
    /// Returns an error if the source cannot be constructed.
    pub fn build(self) -> Result<PlatformSource> {
        let config = PlatformSourceConfig {
            redis_url: self.redis_url,
            stream_key: self.stream_key,
            consumer_group: self
                .consumer_group
                .unwrap_or_else(|| "drasi-core".to_string()),
            consumer_name: self.consumer_name,
            batch_size: self.batch_size.unwrap_or(100),
            block_ms: self.block_ms.unwrap_or(5000),
        };

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

        Ok(PlatformSource {
            base: SourceBase::new(params)?,
            config,
        })
    }
}

impl PlatformSource {
    /// Create a builder for PlatformSource
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_platform::PlatformSource;
    ///
    /// let source = PlatformSource::builder("my-platform-source")
    ///     .with_redis_url("redis://localhost:6379")
    ///     .with_stream_key("my-changes")
    ///     .with_bootstrap_provider(my_provider)
    ///     .build()?;
    /// ```
    pub fn builder(id: impl Into<String>) -> PlatformSourceBuilder {
        PlatformSourceBuilder::new(id)
    }

    /// Create a new platform source.
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance
    /// * `config` - Platform source configuration
    ///
    /// # Returns
    ///
    /// A new `PlatformSource` instance, or an error if construction fails.
    ///
    /// # Errors
    ///
    /// Returns an error if the base source cannot be initialized.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_platform::{PlatformSource, PlatformSourceBuilder};
    ///
    /// let config = PlatformSourceBuilder::new("my-platform-source")
    ///     .with_redis_url("redis://localhost:6379")
    ///     .with_stream_key("my-changes")
    ///     .build()?;
    /// ```
    pub fn new(id: impl Into<String>, config: PlatformSourceConfig) -> Result<Self> {
        let id = id.into();
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }

    /// Subscribe to source events (for testing)
    ///
    /// This method is intended for use in tests to receive events broadcast by the source.
    /// In production, queries subscribe to sources through the SourceManager.
    /// Parse configuration from properties
    #[allow(dead_code)]
    fn parse_config(properties: &HashMap<String, Value>) -> Result<PlatformConfig> {
        // Extract required fields first
        let redis_url = properties
            .get("redis_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Configuration error: Missing required field 'redis_url'. \
                 Platform source requires a Redis connection URL"
                )
            })?
            .to_string();

        let stream_key = properties
            .get("stream_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Configuration error: Missing required field 'stream_key'. \
                 Platform source requires a Redis Stream key to read from"
                )
            })?
            .to_string();

        let consumer_group = properties
            .get("consumer_group")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Configuration error: Missing required field 'consumer_group'. \
                 Platform source requires a consumer group name"
                )
            })?
            .to_string();

        let consumer_name = properties
            .get("consumer_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Configuration error: Missing required field 'consumer_name'. \
                 Platform source requires a unique consumer name"
                )
            })?
            .to_string();

        // Get defaults for optional field handling
        let defaults = PlatformConfig::default();

        // Build config with optional fields
        let config = PlatformConfig {
            redis_url,
            stream_key,
            consumer_group,
            consumer_name,
            batch_size: properties
                .get("batch_size")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(defaults.batch_size),
            block_ms: properties
                .get("block_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(defaults.block_ms),
            start_id: properties
                .get("start_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or(defaults.start_id),
            always_create_consumer_group: properties
                .get("always_create_consumer_group")
                .and_then(|v| v.as_bool())
                .unwrap_or(defaults.always_create_consumer_group),
            max_retries: properties
                .get("max_retries")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(defaults.max_retries),
            retry_delay_ms: properties
                .get("retry_delay_ms")
                .and_then(|v| v.as_u64())
                .unwrap_or(defaults.retry_delay_ms),
        };

        // Validate
        if config.redis_url.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: redis_url cannot be empty. \
                 Please provide a valid Redis connection URL (e.g., redis://localhost:6379)"
            ));
        }
        if config.stream_key.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: stream_key cannot be empty. \
                 Please specify the Redis Stream key to read from"
            ));
        }
        if config.consumer_group.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: consumer_group cannot be empty. \
                 Please specify a consumer group name for this source"
            ));
        }
        if config.consumer_name.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: consumer_name cannot be empty. \
                 Please specify a unique consumer name within the consumer group"
            ));
        }

        Ok(config)
    }

    /// Connect to Redis with retry logic
    async fn connect_with_retry(
        redis_url: &str,
        max_retries: usize,
        retry_delay_ms: u64,
    ) -> Result<redis::aio::MultiplexedConnection> {
        let client = redis::Client::open(redis_url)?;
        let mut delay = retry_delay_ms;

        for attempt in 0..max_retries {
            match client.get_multiplexed_async_connection().await {
                Ok(conn) => {
                    info!("Successfully connected to Redis");
                    return Ok(conn);
                }
                Err(e) if attempt < max_retries - 1 => {
                    warn!(
                        "Redis connection failed (attempt {}/{}): {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                    delay *= 2; // Exponential backoff
                }
                Err(e) => {
                    return Err(anyhow::anyhow!(
                        "Failed to connect to Redis after {max_retries} attempts: {e}"
                    ));
                }
            }
        }

        unreachable!()
    }

    /// Create or recreate consumer group based on configuration
    async fn create_consumer_group(
        conn: &mut redis::aio::MultiplexedConnection,
        stream_key: &str,
        consumer_group: &str,
        start_id: &str,
        always_create: bool,
    ) -> Result<()> {
        // Determine the initial position for the consumer group
        let group_start_id = if start_id == ">" {
            "$" // ">" means only new messages, so create group at end
        } else {
            start_id // "0" or specific ID
        };

        // If always_create is true, delete the existing group first
        if always_create {
            info!(
                "always_create_consumer_group=true, deleting consumer group '{consumer_group}' if it exists"
            );

            let destroy_result: Result<i64, redis::RedisError> = redis::cmd("XGROUP")
                .arg("DESTROY")
                .arg(stream_key)
                .arg(consumer_group)
                .query_async(conn)
                .await;

            match destroy_result {
                Ok(1) => info!("Successfully deleted consumer group '{consumer_group}'"),
                Ok(0) => debug!("Consumer group '{consumer_group}' did not exist"),
                Ok(n) => warn!("Unexpected result from XGROUP DESTROY: {n}"),
                Err(e) => warn!("Error deleting consumer group (will continue): {e}"),
            }
        }

        // Try to create the consumer group
        let result: Result<String, redis::RedisError> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(stream_key)
            .arg(consumer_group)
            .arg(group_start_id)
            .arg("MKSTREAM")
            .query_async(conn)
            .await;

        match result {
            Ok(_) => {
                info!(
                    "Created consumer group '{consumer_group}' for stream '{stream_key}' at position '{group_start_id}'"
                );
                Ok(())
            }
            Err(e) => {
                // BUSYGROUP error means the group already exists
                if e.to_string().contains("BUSYGROUP") {
                    info!(
                        "Consumer group '{consumer_group}' already exists for stream '{stream_key}', will resume from last position"
                    );
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("Failed to create consumer group: {e}"))
                }
            }
        }
    }

    /// Start the stream consumer task
    async fn start_consumer_task(
        source_id: String,
        platform_config: PlatformConfig,
        dispatchers: Arc<
            RwLock<
                Vec<
                    Box<
                        dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync,
                    >,
                >,
            >,
        >,
        event_tx: Arc<RwLock<Option<ComponentEventSender>>>,
        status: Arc<RwLock<ComponentStatus>>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            info!(
                "Starting platform source consumer for source '{}' on stream '{}'",
                source_id, platform_config.stream_key
            );

            // Connect to Redis
            let mut conn = match Self::connect_with_retry(
                &platform_config.redis_url,
                platform_config.max_retries,
                platform_config.retry_delay_ms,
            )
            .await
            {
                Ok(conn) => conn,
                Err(e) => {
                    error!("Failed to connect to Redis: {e}");
                    if let Some(ref tx) = *event_tx.read().await {
                        let _ = tx
                            .send(ComponentEvent {
                                component_id: source_id.clone(),
                                component_type: ComponentType::Source,
                                status: ComponentStatus::Stopped,
                                timestamp: chrono::Utc::now(),
                                message: Some(format!("Failed to connect to Redis: {e}")),
                            })
                            .await;
                    }
                    *status.write().await = ComponentStatus::Stopped;
                    return;
                }
            };

            // Create consumer group
            if let Err(e) = Self::create_consumer_group(
                &mut conn,
                &platform_config.stream_key,
                &platform_config.consumer_group,
                &platform_config.start_id,
                platform_config.always_create_consumer_group,
            )
            .await
            {
                error!("Failed to create consumer group: {e}");
                if let Some(ref tx) = *event_tx.read().await {
                    let _ = tx
                        .send(ComponentEvent {
                            component_id: source_id.clone(),
                            component_type: ComponentType::Source,
                            status: ComponentStatus::Stopped,
                            timestamp: chrono::Utc::now(),
                            message: Some(format!("Failed to create consumer group: {e}")),
                        })
                        .await;
                }
                *status.write().await = ComponentStatus::Stopped;
                return;
            }

            // Main consumer loop
            loop {
                // Read from stream using ">" to get next undelivered messages for this consumer group
                let read_result: Result<StreamReadReply, redis::RedisError> =
                    redis::cmd("XREADGROUP")
                        .arg("GROUP")
                        .arg(&platform_config.consumer_group)
                        .arg(&platform_config.consumer_name)
                        .arg("COUNT")
                        .arg(platform_config.batch_size)
                        .arg("BLOCK")
                        .arg(platform_config.block_ms)
                        .arg("STREAMS")
                        .arg(&platform_config.stream_key)
                        .arg(">") // Always use ">" for consumer group reads
                        .query_async(&mut conn)
                        .await;

                match read_result {
                    Ok(reply) => {
                        // Collect all stream IDs for batch acknowledgment
                        let mut all_stream_ids = Vec::new();

                        // Process each stream entry
                        for stream_key in reply.keys {
                            for stream_id in stream_key.ids {
                                debug!("Received event from stream: {}", stream_id.id);

                                // Store stream ID for batch acknowledgment
                                all_stream_ids.push(stream_id.id.clone());

                                // Extract event data
                                match extract_event_data(&stream_id.map) {
                                    Ok(event_json) => {
                                        // Parse JSON
                                        match serde_json::from_str::<Value>(&event_json) {
                                            Ok(cloud_event) => {
                                                // Detect message type
                                                let message_type =
                                                    detect_message_type(&cloud_event);

                                                match message_type {
                                                    MessageType::Control(control_type) => {
                                                        // Handle control message
                                                        debug!(
                                                            "Detected control message of type: {control_type}"
                                                        );

                                                        match transform_control_event(
                                                            cloud_event,
                                                            &control_type,
                                                        ) {
                                                            Ok(control_events) => {
                                                                // Publish control events
                                                                for control_event in control_events
                                                                {
                                                                    // Create profiling metadata with timestamps
                                                                    let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                                                                    profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                                                                    let wrapper = SourceEventWrapper::with_profiling(
                                                                        source_id.clone(),
                                                                        SourceEvent::Control(control_event),
                                                                        chrono::Utc::now(),
                                                                        profiling,
                                                                    );

                                                                    // Dispatch via helper
                                                                    if let Err(e) = SourceBase::dispatch_from_task(
                                                                        dispatchers.clone(),
                                                                        wrapper,
                                                                        &source_id,
                                                                    )
                                                                    .await
                                                                    {
                                                                        debug!("[{source_id}] Failed to dispatch control event (no subscribers): {e}");
                                                                    } else {
                                                                        debug!(
                                                                            "Published control event for stream {}",
                                                                            stream_id.id
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                            Err(e) => {
                                                                warn!(
                                                                    "Failed to transform control event {}: {}",
                                                                    stream_id.id, e
                                                                );
                                                            }
                                                        }
                                                    }
                                                    MessageType::Data => {
                                                        // Handle data message
                                                        match transform_platform_event(
                                                            cloud_event,
                                                            &source_id,
                                                        ) {
                                                            Ok(source_changes_with_timestamps) => {
                                                                // Publish source changes
                                                                for item in
                                                                    source_changes_with_timestamps
                                                                {
                                                                    // Create profiling metadata with timestamps
                                                                    let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                                                                    profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                                                                    // Extract source_ns from SourceChange transaction time
                                                                    profiling.source_ns = Some(
                                                                        item.source_change
                                                                            .get_transaction_time(),
                                                                    );

                                                                    // Set reactivator timestamps from event
                                                                    profiling
                                                                        .reactivator_start_ns =
                                                                        item.reactivator_start_ns;
                                                                    profiling.reactivator_end_ns =
                                                                        item.reactivator_end_ns;

                                                                    let wrapper = SourceEventWrapper::with_profiling(
                                                                        source_id.clone(),
                                                                        SourceEvent::Change(item.source_change),
                                                                        chrono::Utc::now(),
                                                                        profiling,
                                                                    );

                                                                    // Dispatch via helper
                                                                    if let Err(e) = SourceBase::dispatch_from_task(
                                                                        dispatchers.clone(),
                                                                        wrapper,
                                                                        &source_id,
                                                                    )
                                                                    .await
                                                                    {
                                                                        debug!("[{source_id}] Failed to dispatch change (no subscribers): {e}");
                                                                    } else {
                                                                        debug!(
                                                                            "Published source change for event {}",
                                                                            stream_id.id
                                                                        );
                                                                    }
                                                                }
                                                            }
                                                            Err(e) => {
                                                                warn!(
                                                                    "Failed to transform event {}: {}",
                                                                    stream_id.id, e
                                                                );
                                                                if let Some(ref tx) =
                                                                    *event_tx.read().await
                                                                {
                                                                    let _ = tx
                                                                        .send(ComponentEvent {
                                                                            component_id: source_id.clone(),
                                                                            component_type:
                                                                                ComponentType::Source,
                                                                            status: ComponentStatus::Running,
                                                                            timestamp: chrono::Utc::now(),
                                                                            message: Some(format!(
                                                                                "Transformation error: {e}"
                                                                            )),
                                                                        })
                                                                        .await;
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                warn!(
                                                    "Failed to parse JSON for event {}: {}",
                                                    stream_id.id, e
                                                );
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Failed to extract event data from {}: {}",
                                            stream_id.id, e
                                        );
                                    }
                                }
                            }
                        }

                        // Batch acknowledge all messages at once
                        if !all_stream_ids.is_empty() {
                            debug!("Acknowledging batch of {} messages", all_stream_ids.len());

                            let mut cmd = redis::cmd("XACK");
                            cmd.arg(&platform_config.stream_key)
                                .arg(&platform_config.consumer_group);

                            // Add all stream IDs to the command
                            for stream_id in &all_stream_ids {
                                cmd.arg(stream_id);
                            }

                            match cmd.query_async::<_, i64>(&mut conn).await {
                                Ok(ack_count) => {
                                    debug!("Successfully acknowledged {ack_count} messages");
                                    if ack_count as usize != all_stream_ids.len() {
                                        warn!(
                                            "Acknowledged {} messages but expected {}",
                                            ack_count,
                                            all_stream_ids.len()
                                        );
                                    }
                                }
                                Err(e) => {
                                    error!("Failed to acknowledge message batch: {e}");

                                    // Fallback: Try acknowledging messages individually
                                    warn!("Falling back to individual acknowledgments");
                                    for stream_id in &all_stream_ids {
                                        match redis::cmd("XACK")
                                            .arg(&platform_config.stream_key)
                                            .arg(&platform_config.consumer_group)
                                            .arg(stream_id)
                                            .query_async::<_, i64>(&mut conn)
                                            .await
                                        {
                                            Ok(_) => {
                                                debug!(
                                                    "Individually acknowledged message {stream_id}"
                                                );
                                            }
                                            Err(e) => {
                                                error!(
                                                    "Failed to individually acknowledge message {stream_id}: {e}"
                                                );
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        // Check if it's a connection error
                        if is_connection_error(&e) {
                            error!("Redis connection lost: {e}");
                            if let Some(ref tx) = *event_tx.read().await {
                                let _ = tx
                                    .send(ComponentEvent {
                                        component_id: source_id.clone(),
                                        component_type: ComponentType::Source,
                                        status: ComponentStatus::Running,
                                        timestamp: chrono::Utc::now(),
                                        message: Some(format!("Redis connection lost: {e}")),
                                    })
                                    .await;
                            }

                            // Try to reconnect
                            match Self::connect_with_retry(
                                &platform_config.redis_url,
                                platform_config.max_retries,
                                platform_config.retry_delay_ms,
                            )
                            .await
                            {
                                Ok(new_conn) => {
                                    conn = new_conn;
                                    info!("Reconnected to Redis");
                                }
                                Err(e) => {
                                    error!("Failed to reconnect to Redis: {e}");
                                    *status.write().await = ComponentStatus::Stopped;
                                    return;
                                }
                            }
                        } else if !e.to_string().contains("timeout") {
                            // Log non-timeout errors
                            error!("Error reading from stream: {e}");
                        }
                    }
                }
            }
        })
    }
}

#[async_trait::async_trait]
impl Source for PlatformSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "platform"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "redis_url".to_string(),
            serde_json::Value::String(self.config.redis_url.clone()),
        );
        props.insert(
            "stream_key".to_string(),
            serde_json::Value::String(self.config.stream_key.clone()),
        );
        props.insert(
            "consumer_group".to_string(),
            serde_json::Value::String(self.config.consumer_group.clone()),
        );
        if let Some(ref consumer_name) = self.config.consumer_name {
            props.insert(
                "consumer_name".to_string(),
                serde_json::Value::String(consumer_name.clone()),
            );
        }
        props.insert(
            "batch_size".to_string(),
            serde_json::Value::Number(self.config.batch_size.into()),
        );
        props.insert(
            "block_ms".to_string(),
            serde_json::Value::Number(self.config.block_ms.into()),
        );
        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        info!("Starting platform source: {}", self.base.id);

        // Extract configuration from typed config
        let platform_config = PlatformConfig {
            redis_url: self.config.redis_url.clone(),
            stream_key: self.config.stream_key.clone(),
            consumer_group: self.config.consumer_group.clone(),
            consumer_name: self
                .config
                .consumer_name
                .clone()
                .unwrap_or_else(|| format!("drasi-consumer-{}", self.base.id)),
            batch_size: self.config.batch_size,
            block_ms: self.config.block_ms,
            start_id: ">".to_string(),
            always_create_consumer_group: false,
            max_retries: 5,
            retry_delay_ms: 1000,
        };

        // Update status
        *self.base.status.write().await = ComponentStatus::Running;

        // Start consumer task
        let task = Self::start_consumer_task(
            self.base.id.clone(),
            platform_config,
            self.base.dispatchers.clone(),
            self.base.event_tx(),
            self.base.status.clone(),
        )
        .await;

        *self.base.task_handle.write().await = Some(task);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping platform source: {}", self.base.id);

        // Cancel the task
        if let Some(handle) = self.base.task_handle.write().await.take() {
            handle.abort();
        }

        // Update status
        *self.base.status.write().await = ComponentStatus::Stopped;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.status.read().await.clone()
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
                "Platform",
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

impl PlatformSource {
    /// Create a test subscription to this source (synchronous)
    ///
    /// This method delegates to SourceBase and is provided for convenience in tests.
    /// Note: Use test_subscribe_async() in async contexts to avoid runtime issues.
    pub fn test_subscribe(
        &self,
    ) -> Box<dyn drasi_lib::channels::ChangeReceiver<drasi_lib::channels::SourceEventWrapper>> {
        self.base.test_subscribe()
    }

    /// Create a test subscription to this source (async)
    ///
    /// This method delegates to SourceBase and is provided for convenience in async tests.
    /// Prefer this method over test_subscribe() in async contexts.
    pub async fn test_subscribe_async(
        &self,
    ) -> Box<dyn drasi_lib::channels::ChangeReceiver<drasi_lib::channels::SourceEventWrapper>> {
        self.base
            .create_streaming_receiver()
            .await
            .expect("Failed to create test subscription")
    }
}

/// Extract event data from Redis stream entry
fn extract_event_data(entry_map: &HashMap<String, redis::Value>) -> Result<String> {
    // Look for common field names
    for key in &["data", "event", "payload", "message"] {
        if let Some(redis::Value::Data(data)) = entry_map.get(*key) {
            return String::from_utf8(data.clone())
                .map_err(|e| anyhow::anyhow!("Invalid UTF-8 in event data: {e}"));
        }
    }

    Err(anyhow::anyhow!(
        "No event data found in stream entry. Available keys: {:?}",
        entry_map.keys().collect::<Vec<_>>()
    ))
}

/// Check if error is a connection error
fn is_connection_error(e: &redis::RedisError) -> bool {
    e.is_connection_dropped()
        || e.is_io_error()
        || e.to_string().contains("connection")
        || e.to_string().contains("EOF")
}

/// Message type discriminator
#[derive(Debug, Clone, PartialEq)]
enum MessageType {
    /// Control message with control type from source.table
    Control(String),
    /// Data message (node/relation change)
    Data,
}

/// Detect message type based on payload.source.db field
///
/// Returns MessageType::Control with table name if source.db = "Drasi" (case-insensitive)
/// Returns MessageType::Data for all other cases
fn detect_message_type(cloud_event: &Value) -> MessageType {
    // Extract data array and get first event to check message type
    let data_array = match cloud_event["data"].as_array() {
        Some(arr) if !arr.is_empty() => arr,
        _ => return MessageType::Data, // Default to data if no data array
    };

    // Check the first event's source.db field
    let first_event = &data_array[0];
    let source_db = first_event["payload"]["source"]["db"]
        .as_str()
        .unwrap_or("");

    // Case-insensitive comparison with "Drasi"
    if source_db.eq_ignore_ascii_case("drasi") {
        // Extract source.table to determine control type
        let control_type = first_event["payload"]["source"]["table"]
            .as_str()
            .unwrap_or("Unknown")
            .to_string();
        MessageType::Control(control_type)
    } else {
        MessageType::Data
    }
}

/// Helper struct to hold SourceChange along with reactivator timestamps
#[derive(Debug)]
struct SourceChangeWithTimestamps {
    source_change: SourceChange,
    reactivator_start_ns: Option<u64>,
    reactivator_end_ns: Option<u64>,
}

/// Transform CloudEvent-wrapped platform event to drasi-core SourceChange(s)
///
/// Handles events in CloudEvent format with a data array containing change events.
/// Each event in the data array has: op, payload.after/before, payload.source
fn transform_platform_event(
    cloud_event: Value,
    source_id: &str,
) -> Result<Vec<SourceChangeWithTimestamps>> {
    let mut source_changes = Vec::new();

    // Extract the data array from CloudEvent wrapper
    let data_array = cloud_event["data"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'data' array in CloudEvent"))?;

    // Process each event in the data array
    for event in data_array {
        // Extract reactivator timestamps from top-level event fields
        let reactivator_start_ns = event["reactivatorStart_ns"].as_u64();
        let reactivator_end_ns = event["reactivatorEnd_ns"].as_u64();

        // Extract operation type (op field: "i", "u", "d")
        let op = event["op"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'op' field"))?;

        // Extract payload
        let payload = &event["payload"];
        if payload.is_null() {
            return Err(anyhow::anyhow!("Missing 'payload' field"));
        }

        // Extract element type from payload.source.table
        let element_type = payload["source"]["table"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'payload.source.table' field"))?;

        // Get element data based on operation
        let element_data = match op {
            "i" | "u" => &payload["after"],
            "d" => &payload["before"],
            _ => return Err(anyhow::anyhow!("Unknown operation type: {op}")),
        };

        if element_data.is_null() {
            return Err(anyhow::anyhow!(
                "Missing element data (after/before) for operation {op}"
            ));
        }

        // Extract element ID
        let element_id = element_data["id"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid element 'id' field"))?;

        // Extract labels
        let labels_array = element_data["labels"]
            .as_array()
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'labels' field"))?;

        let labels: Vec<Arc<str>> = labels_array
            .iter()
            .filter_map(|v| v.as_str().map(Arc::from))
            .collect();

        if labels.is_empty() {
            return Err(anyhow::anyhow!("Labels array is empty or invalid"));
        }

        // Extract timestamp from payload.source.ts_ns (already in nanoseconds)
        let effective_from = payload["source"]["ts_ns"]
            .as_u64()
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'payload.source.ts_ns' field"))?;

        // Build ElementMetadata
        let reference = ElementReference::new(source_id, element_id);
        let metadata = ElementMetadata {
            reference,
            labels: labels.into(),
            effective_from,
        };

        // Handle delete operation (no properties needed)
        if op == "d" {
            source_changes.push(SourceChangeWithTimestamps {
                source_change: SourceChange::Delete { metadata },
                reactivator_start_ns,
                reactivator_end_ns,
            });
            continue;
        }

        // Convert properties
        let properties_obj = element_data["properties"]
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'properties' field"))?;

        let properties = convert_json_to_element_properties(properties_obj)?;

        // Build element based on type
        let element = match element_type {
            "node" => Element::Node {
                metadata,
                properties,
            },
            "rel" | "relation" => {
                // Extract startId and endId
                let start_id = element_data["startId"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'startId' for relation"))?;
                let end_id = element_data["endId"]
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'endId' for relation"))?;

                Element::Relation {
                    metadata,
                    properties,
                    out_node: ElementReference::new(source_id, start_id),
                    in_node: ElementReference::new(source_id, end_id),
                }
            }
            _ => return Err(anyhow::anyhow!("Unknown element type: {element_type}")),
        };

        // Build SourceChange
        let source_change = match op {
            "i" => SourceChange::Insert { element },
            "u" => SourceChange::Update { element },
            _ => unreachable!(),
        };

        source_changes.push(SourceChangeWithTimestamps {
            source_change,
            reactivator_start_ns,
            reactivator_end_ns,
        });
    }

    Ok(source_changes)
}

/// Transform CloudEvent-wrapped control event to SourceControl(s)
///
/// Handles control messages from Query API service with source.db = "Drasi"
/// Currently supports "SourceSubscription" control type
fn transform_control_event(cloud_event: Value, control_type: &str) -> Result<Vec<SourceControl>> {
    let mut control_events = Vec::new();

    // Extract the data array from CloudEvent wrapper
    let data_array = cloud_event["data"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'data' array in CloudEvent"))?;

    // Check if control type is supported
    if control_type != "SourceSubscription" {
        info!(
            "Skipping unknown control type '{control_type}' (only 'SourceSubscription' is supported)"
        );
        return Ok(control_events); // Return empty vec for unknown types
    }

    // Process each event in the data array
    for event in data_array {
        // Extract operation type (op field: "i", "u", "d")
        let op = event["op"]
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'op' field in control event"))?;

        // Extract payload
        let payload = &event["payload"];
        if payload.is_null() {
            warn!("Missing 'payload' field in control event, skipping");
            continue;
        }

        // Get data based on operation
        let control_data = match op {
            "i" | "u" => &payload["after"],
            "d" => &payload["before"],
            _ => {
                warn!("Unknown operation type in control event: {op}, skipping");
                continue;
            }
        };

        if control_data.is_null() {
            warn!("Missing control data (after/before) for operation {op}, skipping");
            continue;
        }

        // Extract required fields for SourceSubscription
        let query_id = match control_data["queryId"].as_str() {
            Some(id) => id.to_string(),
            None => {
                warn!("Missing required 'queryId' field in control event, skipping");
                continue;
            }
        };

        let query_node_id = match control_data["queryNodeId"].as_str() {
            Some(id) => id.to_string(),
            None => {
                warn!("Missing required 'queryNodeId' field in control event, skipping");
                continue;
            }
        };

        // Extract optional label arrays (default to empty if missing)
        let node_labels = control_data["nodeLabels"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let rel_labels = control_data["relLabels"]
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Map operation to ControlOperation
        let operation = match op {
            "i" => ControlOperation::Insert,
            "u" => ControlOperation::Update,
            "d" => ControlOperation::Delete,
            _ => unreachable!(), // Already filtered above
        };

        // Build SourceControl::Subscription
        let control_event = SourceControl::Subscription {
            query_id,
            query_node_id,
            node_labels,
            rel_labels,
            operation,
        };

        control_events.push(control_event);
    }

    Ok(control_events)
}
