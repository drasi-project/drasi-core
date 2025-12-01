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

pub use super::config::PlatformReactionConfig;

// Use modules declared in lib.rs
use crate::publisher;
use crate::transformer;
use crate::types::{
    CloudEvent, CloudEventConfig, ControlSignal, ResultControlEvent,
    ResultEvent,
};

use drasi_lib::channels::{ComponentEventSender, ComponentStatus};
use drasi_lib::plugin_core::{QuerySubscriber, Reaction};
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::managers::log_component_start;
use std::collections::HashMap;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use publisher::{PublisherConfig, RedisStreamPublisher};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Platform Reaction for publishing to Redis Streams
pub struct PlatformReaction {
    base: ReactionBase,
    config: PlatformReactionConfig,
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
    ///
    /// The event channel is automatically injected when the reaction is added
    /// to DrasiLib via `add_reaction()`.
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: PlatformReactionConfig,
    ) -> Result<Self> {
        let id = id.into();

        // Extract configuration values
        let redis_url = config.redis_url.clone();
        let pubsub_name = config
            .pubsub_name
            .clone()
            .unwrap_or_else(|| "drasi-pubsub".to_string());
        let source_name = config
            .source_name
            .clone()
            .unwrap_or_else(|| "drasi-core".to_string());
        let max_stream_length = config.max_stream_length;
        let emit_control_events = config.emit_control_events;
        let batch_enabled = config.batch_enabled;
        let batch_max_size = config.batch_max_size;
        let batch_max_wait_ms = config.batch_max_wait_ms;

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

        let params = ReactionBaseParams::new(id, queries);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
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
        if let Some(ref pubsub_name) = self.config.pubsub_name {
            props.insert(
                "pubsub_name".to_string(),
                serde_json::Value::String(pubsub_name.clone()),
            );
        }
        if let Some(ref source_name) = self.config.source_name {
            props.insert(
                "source_name".to_string(),
                serde_json::Value::String(source_name.clone()),
            );
        }
        props.insert(
            "emit_control_events".to_string(),
            serde_json::Value::Bool(self.config.emit_control_events),
        );
        props.insert(
            "batch_enabled".to_string(),
            serde_json::Value::Bool(self.config.batch_enabled),
        );
        props
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    async fn start(
        &self,
        query_subscriber: Arc<dyn QuerySubscriber>,
    ) -> Result<()> {
        log_component_start("Reaction", &self.base.id);

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
        self.base.subscribe_to_queries(query_subscriber).await?;

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
        let reaction_id = self.base.id.clone();
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
                    profiling.reaction_receive_ns = Some(drasi_lib::profiling::timestamp_ns());
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

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}
