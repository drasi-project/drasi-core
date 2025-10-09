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
    ChangeEvent, CloudEvent, CloudEventConfig, ControlEvent, ControlSignal, ResultEvent,
    UpdatePayload,
};

use crate::channels::{ComponentEvent, ComponentEventSender, ComponentStatus, QueryResultReceiver};
use crate::config::ReactionConfig;
use crate::reactions::Reaction;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use publisher::{PublisherConfig, RedisStreamPublisher};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

/// Platform Reaction for publishing to Redis Streams
pub struct PlatformReaction {
    config: ReactionConfig,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: ComponentEventSender,
    publisher: Arc<RedisStreamPublisher>,
    sequence_counter: Arc<RwLock<i64>>,
    shutdown_tx: Arc<RwLock<Option<mpsc::Sender<()>>>>,
    cloud_event_config: CloudEventConfig,
    emit_control_events: bool,
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

        // Create publisher config
        let publisher_config = PublisherConfig { max_stream_length };

        // Create Redis publisher
        let publisher = RedisStreamPublisher::new(&redis_url, publisher_config)
            .context("Failed to create Redis Stream Publisher")?;

        // Create CloudEvent config
        let cloud_event_config = CloudEventConfig::with_values(pubsub_name, source_name);

        Ok(Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx,
            publisher: Arc::new(publisher),
            sequence_counter: Arc::new(RwLock::new(0)),
            shutdown_tx: Arc::new(RwLock::new(None)),
            cloud_event_config,
            emit_control_events,
        })
    }

    /// Emit a control event
    async fn emit_control_event(&self, signal: ControlSignal) -> Result<()> {
        if !self.emit_control_events {
            return Ok(());
        }

        let control_event = ControlEvent {
            control_signal: signal,
        };

        let result_event = ResultEvent::Control(control_event);

        // Use first query ID from config for control events
        let query_id = self
            .config
            .queries
            .first()
            .ok_or_else(|| anyhow!("No queries configured for reaction"))?;

        let cloud_event = CloudEvent::new(result_event, query_id, &self.cloud_event_config);

        self.publisher.publish(cloud_event).await?;

        Ok(())
    }
}

#[async_trait]
impl Reaction for PlatformReaction {
    async fn start(&self, mut result_rx: QueryResultReceiver) -> Result<()> {
        log::info!("Starting Platform Reaction: {}", self.config.id);

        // Update status to Starting
        *self.status.write().await = ComponentStatus::Starting;

        // Send started event
        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: crate::channels::ComponentType::Reaction,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting platform reaction".to_string()),
        };
        if let Err(e) = self.event_tx.send(event).await {
            log::error!("Failed to send reaction started event: {}", e);
        }

        // Emit Running control event
        if let Err(e) = self.emit_control_event(ControlSignal::Running).await {
            log::warn!("Failed to emit Running control event: {}", e);
        }

        // Update status to Running
        *self.status.write().await = ComponentStatus::Running;

        // Send running event
        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: crate::channels::ComponentType::Reaction,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: Some("Platform reaction started".to_string()),
        };
        if let Err(e) = self.event_tx.send(event).await {
            log::error!("Failed to send reaction running event: {}", e);
        }

        // Create shutdown channel
        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        *self.shutdown_tx.write().await = Some(shutdown_tx);

        // Clone what we need for the async task
        let publisher = self.publisher.clone();
        let sequence_counter = self.sequence_counter.clone();
        let cloud_event_config = self.cloud_event_config.clone();
        let reaction_id = self.config.id.clone();
        let status = self.status.clone();

        // Spawn task to process query results
        tokio::spawn(async move {
            log::debug!("Platform Reaction '{}' processing task started", reaction_id);

            loop {
                tokio::select! {
                    // Handle query results
                    Some(query_result) = result_rx.recv() => {
                        // Increment sequence counter
                        let sequence = {
                            let mut counter = sequence_counter.write().await;
                            *counter += 1;
                            *counter
                        };

                        // Transform query result to platform event
                        match transformer::transform_query_result(query_result.clone(), sequence) {
                            Ok(result_event) => {
                                // Wrap in CloudEvent
                                let cloud_event = CloudEvent::new(
                                    result_event,
                                    &query_result.query_id,
                                    &cloud_event_config,
                                );

                                // Publish to Redis
                                match publisher.publish_with_retry(cloud_event, 3).await {
                                    Ok(message_id) => {
                                        log::debug!(
                                            "Published query result for '{}' (sequence: {}, message_id: {})",
                                            query_result.query_id,
                                            sequence,
                                            message_id
                                        );
                                    }
                                    Err(e) => {
                                        log::error!(
                                            "Failed to publish query result for '{}': {}",
                                            query_result.query_id,
                                            e
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                log::error!(
                                    "Failed to transform query result for '{}': {}",
                                    query_result.query_id,
                                    e
                                );
                            }
                        }
                    }

                    // Handle shutdown signal
                    _ = shutdown_rx.recv() => {
                        log::info!("Platform Reaction '{}' received shutdown signal", reaction_id);
                        break;
                    }

                    // Handle channel closure
                    else => {
                        log::warn!("Platform Reaction '{}' result channel closed", reaction_id);
                        break;
                    }
                }
            }

            // Update status to Stopped
            *status.write().await = ComponentStatus::Stopped;
            log::info!("Platform Reaction '{}' processing task stopped", reaction_id);
        });

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log::info!("Stopping Platform Reaction: {}", self.config.id);

        // Send shutdown signal
        if let Some(shutdown_tx) = self.shutdown_tx.write().await.take() {
            let _ = shutdown_tx.send(()).await;
        }

        // Emit Stopped control event
        if let Err(e) = self.emit_control_event(ControlSignal::Stopped).await {
            log::warn!("Failed to emit Stopped control event: {}", e);
        }

        // Update status
        *self.status.write().await = ComponentStatus::Stopped;

        // Send stopped event
        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: crate::channels::ComponentType::Reaction,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: Some("Platform reaction stopped".to_string()),
        };
        if let Err(e) = self.event_tx.send(event).await {
            log::error!("Failed to send reaction stopped event: {}", e);
        }

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &ReactionConfig {
        &self.config
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
        }
    }

    #[tokio::test]
    async fn test_create_platform_reaction() {
        let config = create_test_config();
        let (event_tx, _event_rx) = mpsc::channel(100);

        let reaction = PlatformReaction::new(config, event_tx);
        assert!(reaction.is_ok());

        let reaction = reaction.unwrap();
        assert_eq!(reaction.config.id, "test-reaction");
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
        };

        let (event_tx, _event_rx) = mpsc::channel(100);

        let result = PlatformReaction::new(config, event_tx);
        assert!(result.is_err());
        if let Err(e) = result {
            assert!(e.to_string().contains("Missing required property: redis_url"));
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
}
