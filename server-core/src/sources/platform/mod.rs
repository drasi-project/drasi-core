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
use log::{debug, error, info, warn};
use redis::streams::StreamReadReply;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::channels::{
    ComponentEvent, ComponentEventSender, ComponentStatus, ComponentType, SourceChangeSender,
};
use crate::config::SourceConfig;
use crate::sources::manager::convert_json_to_element_properties;
use crate::sources::{Publisher, Source};
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};

#[cfg(test)]
mod tests;

/// Publisher for sending source changes to internal channels
struct ChannelPublisher {
    source_change_tx: SourceChangeSender,
    source_id: String,
}

#[async_trait::async_trait]
impl Publisher for ChannelPublisher {
    async fn publish(
        &self,
        change: SourceChange,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use crate::channels::SourceChangeEvent;

        let event = SourceChangeEvent {
            source_id: self.source_id.clone(),
            change,
            timestamp: chrono::Utc::now(),
        };

        self.source_change_tx
            .send(event)
            .await
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
    }
}

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
            max_retries: 3,
            retry_delay_ms: 1000,
        }
    }
}

/// Platform source that reads events from Redis Streams
pub struct PlatformSource {
    config: SourceConfig,
    status: Arc<RwLock<ComponentStatus>>,
    source_change_tx: SourceChangeSender,
    event_tx: ComponentEventSender,
    task_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
}

impl PlatformSource {
    /// Create a new platform source
    pub fn new(
        config: SourceConfig,
        source_change_tx: SourceChangeSender,
        event_tx: ComponentEventSender,
    ) -> Self {
        Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            source_change_tx,
            event_tx,
            task_handle: Arc::new(RwLock::new(None)),
        }
    }

    /// Parse configuration from properties
    fn parse_config(properties: &HashMap<String, Value>) -> Result<PlatformConfig> {
        let mut config = PlatformConfig::default();

        // Required fields
        config.redis_url = properties
            .get("redis_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required field: redis_url"))?
            .to_string();

        config.stream_key = properties
            .get("stream_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required field: stream_key"))?
            .to_string();

        config.consumer_group = properties
            .get("consumer_group")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required field: consumer_group"))?
            .to_string();

        config.consumer_name = properties
            .get("consumer_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Missing required field: consumer_name"))?
            .to_string();

        // Optional fields
        if let Some(batch_size) = properties.get("batch_size").and_then(|v| v.as_u64()) {
            config.batch_size = batch_size as usize;
        }

        if let Some(block_ms) = properties.get("block_ms").and_then(|v| v.as_u64()) {
            config.block_ms = block_ms;
        }

        if let Some(start_id) = properties.get("start_id").and_then(|v| v.as_str()) {
            config.start_id = start_id.to_string();
        }

        if let Some(max_retries) = properties.get("max_retries").and_then(|v| v.as_u64()) {
            config.max_retries = max_retries as usize;
        }

        if let Some(retry_delay_ms) = properties.get("retry_delay_ms").and_then(|v| v.as_u64()) {
            config.retry_delay_ms = retry_delay_ms;
        }

        // Validate
        if config.redis_url.is_empty() {
            return Err(anyhow::anyhow!("redis_url cannot be empty"));
        }
        if config.stream_key.is_empty() {
            return Err(anyhow::anyhow!("stream_key cannot be empty"));
        }
        if config.consumer_group.is_empty() {
            return Err(anyhow::anyhow!("consumer_group cannot be empty"));
        }
        if config.consumer_name.is_empty() {
            return Err(anyhow::anyhow!("consumer_name cannot be empty"));
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
                        "Failed to connect to Redis after {} attempts: {}",
                        max_retries,
                        e
                    ));
                }
            }
        }

        unreachable!()
    }

    /// Create consumer group if it doesn't exist
    async fn create_consumer_group(
        conn: &mut redis::aio::MultiplexedConnection,
        stream_key: &str,
        consumer_group: &str,
    ) -> Result<()> {
        // Try to create the consumer group
        let result: Result<String, redis::RedisError> = redis::cmd("XGROUP")
            .arg("CREATE")
            .arg(stream_key)
            .arg(consumer_group)
            .arg("0")
            .arg("MKSTREAM")
            .query_async(conn)
            .await;

        match result {
            Ok(_) => {
                info!(
                    "Created consumer group '{}' for stream '{}'",
                    consumer_group, stream_key
                );
                Ok(())
            }
            Err(e) => {
                // BUSYGROUP error means the group already exists, which is fine
                if e.to_string().contains("BUSYGROUP") {
                    debug!(
                        "Consumer group '{}' already exists for stream '{}'",
                        consumer_group, stream_key
                    );
                    Ok(())
                } else {
                    Err(anyhow::anyhow!("Failed to create consumer group: {}", e))
                }
            }
        }
    }

    /// Start the stream consumer task
    async fn start_consumer_task(
        source_id: String,
        platform_config: PlatformConfig,
        source_change_tx: SourceChangeSender,
        event_tx: ComponentEventSender,
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
                    error!("Failed to connect to Redis: {}", e);
                    let _ = event_tx
                        .send(ComponentEvent {
                            component_id: source_id.clone(),
                            component_type: ComponentType::Source,
                            status: ComponentStatus::Stopped,
                            timestamp: chrono::Utc::now(),
                            message: Some(format!("Failed to connect to Redis: {}", e)),
                        })
                        .await;
                    *status.write().await = ComponentStatus::Stopped;
                    return;
                }
            };

            // Create consumer group
            if let Err(e) = Self::create_consumer_group(
                &mut conn,
                &platform_config.stream_key,
                &platform_config.consumer_group,
            )
            .await
            {
                error!("Failed to create consumer group: {}", e);
                let _ = event_tx
                    .send(ComponentEvent {
                        component_id: source_id.clone(),
                        component_type: ComponentType::Source,
                        status: ComponentStatus::Stopped,
                        timestamp: chrono::Utc::now(),
                        message: Some(format!("Failed to create consumer group: {}", e)),
                    })
                    .await;
                *status.write().await = ComponentStatus::Stopped;
                return;
            }

            // Create publisher
            let publisher = Arc::new(ChannelPublisher {
                source_change_tx,
                source_id: source_id.clone(),
            });

            // Main consumer loop
            let current_id = platform_config.start_id.clone();
            loop {
                // Read from stream
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
                        .arg(&current_id)
                        .query_async(&mut conn)
                        .await;

                match read_result {
                    Ok(reply) => {
                        // Process each stream entry
                        for stream_key in reply.keys {
                            for stream_id in stream_key.ids {
                                debug!("Received event from stream: {}", stream_id.id);

                                // Extract event data
                                match extract_event_data(&stream_id.map) {
                                    Ok(event_json) => {
                                        // Parse JSON
                                        match serde_json::from_str::<Value>(&event_json) {
                                            Ok(cloud_event) => {
                                                // Transform to SourceChange(s)
                                                match transform_platform_event(
                                                    cloud_event,
                                                    &source_id,
                                                ) {
                                                    Ok(source_changes) => {
                                                        // Publish each source change
                                                        for source_change in source_changes {
                                                            if let Err(e) = publisher
                                                                .publish(source_change)
                                                                .await
                                                            {
                                                                error!(
                                                                    "Failed to publish source change: {}",
                                                                    e
                                                                );
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
                                                        let _ = event_tx
                                                            .send(ComponentEvent {
                                                                component_id: source_id.clone(),
                                                                component_type:
                                                                    ComponentType::Source,
                                                                status: ComponentStatus::Running,
                                                                timestamp: chrono::Utc::now(),
                                                                message: Some(format!(
                                                                    "Transformation error: {}",
                                                                    e
                                                                )),
                                                            })
                                                            .await;
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

                                // Acknowledge the message
                                let _: Result<i64, redis::RedisError> = redis::cmd("XACK")
                                    .arg(&platform_config.stream_key)
                                    .arg(&platform_config.consumer_group)
                                    .arg(&stream_id.id)
                                    .query_async(&mut conn)
                                    .await;
                            }
                        }
                    }
                    Err(e) => {
                        // Check if it's a connection error
                        if is_connection_error(&e) {
                            error!("Redis connection lost: {}", e);
                            let _ = event_tx
                                .send(ComponentEvent {
                                    component_id: source_id.clone(),
                                    component_type: ComponentType::Source,
                                    status: ComponentStatus::Running,
                                    timestamp: chrono::Utc::now(),
                                    message: Some(format!("Redis connection lost: {}", e)),
                                })
                                .await;

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
                                    error!("Failed to reconnect to Redis: {}", e);
                                    *status.write().await = ComponentStatus::Stopped;
                                    return;
                                }
                            }
                        } else if !e.to_string().contains("timeout") {
                            // Log non-timeout errors
                            error!("Error reading from stream: {}", e);
                        }
                    }
                }
            }
        })
    }
}

#[async_trait::async_trait]
impl Source for PlatformSource {
    async fn start(&self) -> Result<()> {
        info!("Starting platform source: {}", self.config.id);

        // Parse configuration
        let platform_config = Self::parse_config(&self.config.properties)?;

        // Update status
        *self.status.write().await = ComponentStatus::Running;

        // Start consumer task
        let task = Self::start_consumer_task(
            self.config.id.clone(),
            platform_config,
            self.source_change_tx.clone(),
            self.event_tx.clone(),
            self.status.clone(),
        )
        .await;

        *self.task_handle.write().await = Some(task);

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping platform source: {}", self.config.id);

        // Cancel the task
        if let Some(handle) = self.task_handle.write().await.take() {
            handle.abort();
        }

        // Update status
        *self.status.write().await = ComponentStatus::Stopped;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &SourceConfig {
        &self.config
    }
}

/// Extract event data from Redis stream entry
fn extract_event_data(entry_map: &HashMap<String, redis::Value>) -> Result<String> {
    // Look for common field names
    for key in &["data", "event", "payload", "message"] {
        if let Some(redis::Value::Data(data)) = entry_map.get(*key) {
            return String::from_utf8(data.clone())
                .map_err(|e| anyhow::anyhow!("Invalid UTF-8 in event data: {}", e));
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

/// Transform CloudEvent-wrapped platform event to drasi-core SourceChange(s)
///
/// Handles events in CloudEvent format with a data array containing change events.
/// Each event in the data array has: op, payload.after/before, payload.source
fn transform_platform_event(cloud_event: Value, source_id: &str) -> Result<Vec<SourceChange>> {
    let mut source_changes = Vec::new();

    // Extract the data array from CloudEvent wrapper
    let data_array = cloud_event["data"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing or invalid 'data' array in CloudEvent"))?;

    // Process each event in the data array
    for event in data_array {
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
            _ => return Err(anyhow::anyhow!("Unknown operation type: {}", op)),
        };

        if element_data.is_null() {
            return Err(anyhow::anyhow!(
                "Missing element data (after/before) for operation {}",
                op
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
            source_changes.push(SourceChange::Delete { metadata });
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
            _ => return Err(anyhow::anyhow!("Unknown element type: {}", element_type)),
        };

        // Build SourceChange
        let source_change = match op {
            "i" => SourceChange::Insert { element },
            "u" => SourceChange::Update { element },
            _ => unreachable!(),
        };

        source_changes.push(source_change);
    }

    Ok(source_changes)
}
