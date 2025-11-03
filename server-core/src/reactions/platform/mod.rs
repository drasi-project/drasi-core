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

//! Platform Reaction for publishing query results to Redis Streams
//!
//! The Platform Reaction receives query results from drasi-server-core and publishes
//! them to a Drasi Platform Query Result Queue (Redis Stream) in Dapr CloudEvent format.
//!
//! # Configuration
//!
//! ```yaml
//! reactions:
//!   - id: platform-output
//!     reaction_type: platform
//!     queries: ["my-query"]
//!     auto_start: true
//!     properties:
//!       redis_url: "redis://localhost:6379"  # Required
//!       pubsub_name: "drasi-pubsub"          # Optional, default: "drasi-pubsub"
//!       source_name: "drasi-core"            # Optional, default: "drasi-core"
//!       max_stream_length: 10000             # Optional, default: unlimited
//!       emit_control_events: true            # Optional, default: true
//! ```
//!
//! # Stream Naming
//!
//! Results are published to streams named `{query-id}-results`, allowing consumers
//! to subscribe to specific query results.
//!
//! # CloudEvent Format
//!
//! Messages are wrapped in Dapr CloudEvent envelopes with the following structure:
//! - `data`: The ResultEvent (Change or Control)
//! - `datacontenttype`: "application/json"
//! - `id`: UUID v4
//! - `pubsubname`: From configuration
//! - `source`: From configuration
//! - `specversion`: "1.0"
//! - `time`: ISO 8601 timestamp
//! - `topic`: "{query-id}-results"
//! - `type`: "com.dapr.event.sent"

mod publisher;
mod transformer;
mod types;

pub use types::{
    CloudEvent, CloudEventConfig, ControlSignal, ResultChangeEvent, ResultControlEvent,
    ResultEvent, UpdatePayload,
};

use crate::channels::{ComponentEventSender, ComponentStatus};
use crate::config::ReactionConfig;
use crate::reactions::base::ReactionBase;
use crate::reactions::Reaction;
use crate::server_core::DrasiServerCore;
use crate::utils::log_component_start;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use publisher::{PublisherConfig, RedisStreamPublisher};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Platform Reaction for publishing to Redis Streams
pub struct PlatformReaction {
    base: ReactionBase,
    publisher: Arc<RedisStreamPublisher>,
    sequence_counter: Arc<RwLock<i64>>,
    cloud_event_config: CloudEventConfig,
    emit_control_events: bool,
    // Batch configuration
    batch_enabled: bool,
    batch_max_size: usize,
    batch_max_wait_ms: u64,
}

impl PlatformReaction {
    /// Create a new Platform Reaction
    pub fn new(config: ReactionConfig, event_tx: ComponentEventSender) -> Result<Self> {
        // Extract required configuration
        let redis_url = config
            .properties
            .get("redis_url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Missing required property: redis_url"))?
            .to_string();

        // Extract optional configuration with defaults
        let pubsub_name = config
            .properties
            .get("pubsub_name")
            .and_then(|v| v.as_str())
            .unwrap_or("drasi-pubsub")
            .to_string();

        let source_name = config
            .properties
            .get("source_name")
            .and_then(|v| v.as_str())
            .unwrap_or("drasi-core")
            .to_string();

        let max_stream_length = config
            .properties
            .get("max_stream_length")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize);

        let emit_control_events = config
            .properties
            .get("emit_control_events")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        // Extract batch configuration
        let batch_enabled = config
            .properties
            .get("batch_enabled")
            .and_then(|v| v.as_bool())
            .unwrap_or(true); // Default: batching enabled

        let batch_max_size = config
            .properties
            .get("batch_max_size")
            .and_then(|v| v.as_u64())
            .map(|v| v as usize)
            .unwrap_or(100); // Default: 100 events per batch

        let batch_max_wait_ms = config
            .properties
            .get("batch_max_wait_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(10); // Default: 10ms timeout

        // Validate batch configuration
        if batch_max_size == 0 {
            return Err(anyhow!("batch_max_size must be greater than 0"));
        }
        if batch_max_size > 10000 {
            log::warn!(
                "batch_max_size {} is very large, consider using a smaller value (recommended: 100-1000)",
                batch_max_size
            );
        }
        if batch_max_wait_ms > 1000 {
            log::warn!(
                "batch_max_wait_ms {} is large and may increase latency (recommended: 1-100ms)",
                batch_max_wait_ms
            );
        }

        // Create publisher config
        let publisher_config = PublisherConfig { max_stream_length };

        // Create Redis publisher
        let publisher = RedisStreamPublisher::new(&redis_url, publisher_config)
            .context("Failed to create Redis Stream Publisher")?;

        // Create CloudEvent config
        let cloud_event_config = CloudEventConfig::with_values(pubsub_name, source_name);

        Ok(Self {
            base: ReactionBase::new(config, event_tx),
            publisher: Arc::new(publisher),
            sequence_counter: Arc::new(RwLock::new(0)),
            cloud_event_config,
            emit_control_events,
            batch_enabled,
            batch_max_size,
            batch_max_wait_ms,
        })
    }

    /// Emit a control event
    async fn emit_control_event(&self, signal: ControlSignal) -> Result<()> {
        if !self.emit_control_events {
            return Ok(());
        }

        // Use first query ID from config for control events
        let query_id = self
            .base
            .config
            .queries
            .first()
            .ok_or_else(|| anyhow!("No queries configured for reaction"))?;

        // Get next sequence
        let sequence = {
            let mut counter = self.sequence_counter.write().await;
            *counter += 1;
            *counter as u64
        };

        let result_event = ResultEvent::from_control_signal(
            query_id,
            sequence,
            chrono::Utc::now().timestamp_millis() as u64,
            signal,
        );

        let cloud_event = CloudEvent::new(result_event, query_id, &self.cloud_event_config);

        self.publisher.publish(cloud_event).await?;

        Ok(())
    }
}

#[async_trait]
impl Reaction for PlatformReaction {
    async fn start(&self, server_core: Arc<DrasiServerCore>) -> Result<()> {
        log_component_start("Reaction", &self.base.config.id);

        // Transition to Starting
        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting platform reaction".to_string()),
            )
            .await?;

        // Emit Running control event
        if let Err(e) = self.emit_control_event(ControlSignal::Running).await {
            log::warn!("Failed to emit Running control event: {}", e);
        }

        // Subscribe to all configured queries using ReactionBase
        self.base.subscribe_to_queries(server_core).await?;

        // Transition to Running
        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("Platform reaction started".to_string()),
            )
            .await?;

        // Clone what we need for the processing task
        let publisher = self.publisher.clone();
        let sequence_counter = self.sequence_counter.clone();
        let cloud_event_config = self.cloud_event_config.clone();
        let reaction_id = self.base.config.id.clone();
        let emit_control_events = self.emit_control_events;
        let priority_queue = self.base.priority_queue.clone();
        let batch_enabled = self.batch_enabled;
        let batch_max_size = self.batch_max_size;
        let batch_max_wait_ms = self.batch_max_wait_ms;

        // Spawn main processing task
        let processing_task_handle = tokio::spawn(async move {
            log::debug!(
                "Platform Reaction '{}' processing task started",
                reaction_id
            );

            // Buffer for batching CloudEvents before publishing
            let mut event_buffer: Vec<CloudEvent<ResultEvent>> = Vec::new();
            let mut last_flush_time = std::time::Instant::now();

            loop {
                // Dequeue from priority queue (blocking until available)
                let query_result = priority_queue.dequeue().await;

                // Check if this is a control signal
                if let Some(control_signal) = query_result.metadata.get("control_signal") {
                    // Flush any buffered events before processing control signal
                    if !event_buffer.is_empty() {
                        log::debug!(
                            "Flushing {} buffered events before control signal",
                            event_buffer.len()
                        );
                        if let Err(e) = publisher
                            .publish_batch_with_retry(event_buffer.clone(), 3)
                            .await
                        {
                            log::error!(
                                "Failed to publish buffered events before control signal: {}",
                                e
                            );
                        }
                        event_buffer.clear();
                        last_flush_time = std::time::Instant::now();
                    }

                    if emit_control_events {
                        // This is a control signal, emit it as a control event
                        if let Some(signal_type) = control_signal.as_str() {
                            let control_signal = match signal_type {
                                "bootstrapStarted" => ControlSignal::BootstrapStarted,
                                "bootstrapCompleted" => ControlSignal::BootstrapCompleted,
                                _ => {
                                    log::debug!("Unknown control signal type: {}", signal_type);
                                    continue;
                                }
                            };

                            // Get sequence for control event
                            let control_sequence = {
                                let mut counter = sequence_counter.write().await;
                                *counter += 1;
                                *counter
                            };

                            // Emit control event
                            let control_event = ResultControlEvent {
                                query_id: query_result.query_id.clone(),
                                sequence: control_sequence as u64,
                                source_time_ms: query_result.timestamp.timestamp_millis() as u64,
                                metadata: None,
                                control_signal: control_signal.clone(),
                            };

                            let cloud_event = CloudEvent::new(
                                ResultEvent::Control(control_event),
                                &query_result.query_id,
                                &cloud_event_config,
                            );

                            if let Err(e) = publisher.publish_with_retry(cloud_event, 3).await {
                                log::warn!("Failed to emit control event {}: {}", signal_type, e);
                            } else {
                                log::info!("Emitted control event: {}", signal_type);
                            }
                        }
                    }
                    // Skip regular processing for control signals
                    continue;
                }

                // Create a mutable clone for profiling updates
                let mut query_result_mut = (*query_result).clone();

                // Capture reaction receive timestamp for regular results
                if let Some(ref mut profiling) = query_result_mut.profiling {
                    profiling.reaction_receive_ns = Some(crate::profiling::timestamp_ns());
                }

                // Increment sequence counter
                let sequence = {
                    let mut counter = sequence_counter.write().await;
                    *counter += 1;
                    *counter
                };

                // Transform query result to platform event
                match transformer::transform_query_result(
                    query_result_mut.clone(),
                    sequence,
                    sequence as u64,
                ) {
                    Ok(result_event) => {
                        // Wrap in CloudEvent
                        let cloud_event = CloudEvent::new(
                            result_event,
                            &query_result_mut.query_id,
                            &cloud_event_config,
                        );

                        if batch_enabled {
                            // Add to buffer
                            event_buffer.push(cloud_event);

                            // Check if we should flush the buffer
                            let should_flush = event_buffer.len() >= batch_max_size
                                || last_flush_time.elapsed().as_millis()
                                    >= batch_max_wait_ms as u128;

                            if should_flush {
                                log::debug!(
                                    "Flushing batch of {} events (size: {}, time_elapsed: {}ms)",
                                    event_buffer.len(),
                                    event_buffer.len() >= batch_max_size,
                                    last_flush_time.elapsed().as_millis()
                                );

                                match publisher
                                    .publish_batch_with_retry(event_buffer.clone(), 3)
                                    .await
                                {
                                    Ok(message_ids) => {
                                        log::debug!(
                                            "Published batch of {} query results",
                                            message_ids.len()
                                        );
                                    }
                                    Err(e) => {
                                        log::error!(
                                            "Failed to publish batch of {} query results: {}",
                                            event_buffer.len(),
                                            e
                                        );
                                    }
                                }

                                event_buffer.clear();
                                last_flush_time = std::time::Instant::now();
                            }
                        } else {
                            // Batching disabled - publish immediately
                            match publisher.publish_with_retry(cloud_event, 3).await {
                                Ok(message_id) => {
                                    log::debug!(
                                        "Published query result for '{}' (sequence: {}, message_id: {})",
                                        query_result_mut.query_id,
                                        sequence,
                                        message_id
                                    );
                                }
                                Err(e) => {
                                    log::error!(
                                        "Failed to publish query result for '{}': {}",
                                        query_result_mut.query_id,
                                        e
                                    );
                                }
                            }
                        }
                    }
                    Err(e) => {
                        log::error!(
                            "Failed to transform query result for '{}': {}",
                            query_result_mut.query_id,
                            e
                        );
                    }
                }
            }
        });

        // Store the processing task handle
        self.base.set_processing_task(processing_task_handle).await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        // Use ReactionBase common stop functionality
        self.base.stop_common().await?;

        // Emit Stopped control event
        if let Err(e) = self.emit_control_event(ControlSignal::Stopped).await {
            log::warn!("Failed to emit Stopped control event: {}", e);
        }

        // Transition to Stopped
        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Platform reaction stopped".to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;
    use tokio::sync::mpsc;

    fn create_test_config() -> ReactionConfig {
        let mut properties = HashMap::new();
        properties.insert("redis_url".to_string(), json!("redis://localhost:6379"));

        ReactionConfig {
            id: "test-reaction".to_string(),
            reaction_type: "platform".to_string(),
            queries: vec!["test-query".to_string()],
            auto_start: true,
            properties,
            priority_queue_capacity: None,
        }
    }

    #[tokio::test]
    async fn test_create_platform_reaction() {
        let config = create_test_config();
        let (event_tx, _event_rx) = mpsc::channel(100);

        let reaction = PlatformReaction::new(config, event_tx);
        assert!(reaction.is_ok());

        let reaction = reaction.unwrap();
        assert_eq!(reaction.base.config.id, "test-reaction");
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_create_platform_reaction_missing_redis_url() {
        let mut properties = HashMap::new();
        properties.insert("pubsub_name".to_string(), json!("test-pubsub"));

        let config = ReactionConfig {
            id: "test-reaction".to_string(),
            reaction_type: "platform".to_string(),
            queries: vec!["test-query".to_string()],
            auto_start: true,
            properties,
            priority_queue_capacity: None,
        };

        let (event_tx, _event_rx) = mpsc::channel(100);

        let result = PlatformReaction::new(config, event_tx);
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e
                .to_string()
                .contains("Missing required property: redis_url"));
        }
    }

    #[tokio::test]
    async fn test_config_with_custom_values() {
        let mut properties = HashMap::new();
        properties.insert("redis_url".to_string(), json!("redis://localhost:6379"));
        properties.insert("pubsub_name".to_string(), json!("custom-pubsub"));
        properties.insert("source_name".to_string(), json!("custom-source"));
        properties.insert("max_stream_length".to_string(), json!(5000));
        properties.insert("emit_control_events".to_string(), json!(false));

        let config = ReactionConfig {
            id: "test-reaction".to_string(),
            reaction_type: "platform".to_string(),
            queries: vec!["test-query".to_string()],
            auto_start: true,
            properties,
            priority_queue_capacity: None,
        };

        let (event_tx, _event_rx) = mpsc::channel(100);

        let reaction = PlatformReaction::new(config, event_tx).unwrap();
        assert_eq!(reaction.cloud_event_config.pubsub_name, "custom-pubsub");
        assert_eq!(reaction.cloud_event_config.source, "custom-source");
        assert_eq!(reaction.emit_control_events, false);
    }

    #[tokio::test]
    async fn test_get_config() {
        let config = create_test_config();
        let (event_tx, _event_rx) = mpsc::channel(100);

        let reaction = PlatformReaction::new(config.clone(), event_tx).unwrap();
        assert_eq!(reaction.get_config().id, config.id);
    }

    #[tokio::test]
    async fn test_status_lifecycle() {
        let config = create_test_config();
        let (event_tx, _event_rx) = mpsc::channel(100);

        let reaction = PlatformReaction::new(config, event_tx).unwrap();
        assert_eq!(reaction.status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_profiling_metadata_end_to_end() {
        use crate::channels::QueryResult;
        use crate::profiling::ProfilingMetadata;

        // Create profiling metadata with all fields populated
        let profiling = ProfilingMetadata {
            source_ns: Some(1744055144490466971),
            reactivator_start_ns: Some(1744055140000000000),
            reactivator_end_ns: Some(1744055142000000000),
            source_receive_ns: Some(1744055159124143047),
            source_send_ns: Some(1744055173551481387),
            query_receive_ns: Some(1744055178510629042),
            query_core_call_ns: Some(1744055178510650750),
            query_core_return_ns: Some(1744055178510848750),
            query_send_ns: Some(1744055178510900000),
            reaction_receive_ns: Some(1744055178510950000),
            reaction_complete_ns: None, // Not included in output
        };

        // Create QueryResult with profiling data
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1744055173551).unwrap(),
            results: vec![
                json!({"type": "add", "data": {"id": "1", "value": "test"}}),
                json!({"type": "delete", "data": {"id": "2"}}),
            ],
            metadata: HashMap::new(),
            profiling: Some(profiling),
        };

        // Transform through the transformer
        let result_event =
            transformer::transform_query_result(query_result.clone(), 24851, 24851).unwrap();

        // Create CloudEvent config
        let cloud_event_config = CloudEventConfig {
            pubsub_name: "test-pubsub".to_string(),
            source: "test-source".to_string(),
        };

        // Wrap in CloudEvent
        let cloud_event =
            CloudEvent::new(result_event, &query_result.query_id, &cloud_event_config);

        // Serialize to JSON
        let json_str = serde_json::to_string(&cloud_event).unwrap();

        // Deserialize and validate structure
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        // Validate CloudEvent structure
        assert_eq!(parsed["source"], "test-source");
        assert_eq!(parsed["topic"], "test-query-results");
        assert_eq!(parsed["type"], "com.dapr.event.sent");

        // Validate data structure (camelCase field names)
        let data = &parsed["data"];
        assert_eq!(data["kind"], "change");
        assert_eq!(data["queryId"], "test-query");
        assert_eq!(data["sequence"], 24851);
        assert_eq!(data["addedResults"].as_array().unwrap().len(), 1);
        assert_eq!(data["deletedResults"].as_array().unwrap().len(), 1);

        // Validate tracking metadata structure
        assert!(data["metadata"].is_object());
        let metadata = &data["metadata"];
        assert!(metadata["tracking"].is_object());

        let tracking = &metadata["tracking"];

        // Validate source tracking metadata
        assert!(tracking["source"].is_object());
        let source = &tracking["source"];
        assert_eq!(source["seq"], 24851);
        assert_eq!(source["source_ns"], 1744055144490466971u64);
        assert_eq!(source["changeDispatcherEnd_ns"], 1744055173551481387u64);
        assert_eq!(source["changeDispatcherStart_ns"], 1744055173551481387u64);
        assert_eq!(source["changeRouterEnd_ns"], 1744055173551481387u64);
        assert_eq!(source["changeRouterStart_ns"], 1744055159124143047u64);
        assert_eq!(source["reactivatorEnd_ns"], 1744055142000000000u64);
        assert_eq!(source["reactivatorStart_ns"], 1744055140000000000u64);

        // Validate query tracking metadata
        assert!(tracking["query"].is_object());
        let query = &tracking["query"];
        assert_eq!(query["enqueue_ns"], 1744055173551481387u64);
        assert_eq!(query["dequeue_ns"], 1744055178510629042u64);
        assert_eq!(query["queryStart_ns"], 1744055178510650750u64);
        assert_eq!(query["queryEnd_ns"], 1744055178510848750u64);
    }

    #[tokio::test]
    async fn test_profiling_metadata_without_profiling_data() {
        use crate::channels::QueryResult;

        // Create QueryResult WITHOUT profiling data
        let query_result = QueryResult {
            query_id: "test-query".to_string(),
            timestamp: chrono::DateTime::from_timestamp_millis(1744055173551).unwrap(),
            results: vec![json!({"type": "add", "data": {"id": "1"}})],
            metadata: HashMap::new(),
            profiling: None, // No profiling data
        };

        // Transform through the transformer
        let result_event = transformer::transform_query_result(query_result.clone(), 1, 1).unwrap();

        // Create CloudEvent config
        let cloud_event_config = CloudEventConfig {
            pubsub_name: "test-pubsub".to_string(),
            source: "test-source".to_string(),
        };

        // Wrap in CloudEvent
        let cloud_event =
            CloudEvent::new(result_event, &query_result.query_id, &cloud_event_config);

        // Serialize to JSON
        let json_str = serde_json::to_string(&cloud_event).unwrap();

        // Deserialize and validate structure
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        // Validate data structure
        let data = &parsed["data"];

        // Metadata should be None when there's no profiling data and no other metadata
        assert!(data["metadata"].is_null() || !data.get("metadata").is_some());
    }
}
