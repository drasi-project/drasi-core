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

//! Redis Stream publisher for CloudEvents

use super::types::{CloudEvent, ResultEvent};
use anyhow::{Context, Result};
use redis::aio::MultiplexedConnection;
use redis::{AsyncCommands, Client};
use std::time::Duration;
use tokio::time::sleep;

/// Configuration for the Redis Stream Publisher
#[derive(Debug, Clone, Default)]
pub struct PublisherConfig {
    /// Maximum stream length (optional, for MAXLEN policy)
    pub max_stream_length: Option<usize>,
}

/// Redis Stream Publisher for CloudEvents
pub struct RedisStreamPublisher {
    client: Client,
    config: PublisherConfig,
}

impl RedisStreamPublisher {
    /// Create a new Redis Stream Publisher
    pub fn new(redis_url: &str, config: PublisherConfig) -> Result<Self> {
        let client = Client::open(redis_url).context("Failed to create Redis client")?;

        Ok(Self { client, config })
    }

    /// Get a multiplexed connection with retry logic
    async fn get_connection(&self) -> Result<MultiplexedConnection> {
        let max_retries = 3;
        let mut retry_delay = Duration::from_millis(100);

        for attempt in 1..=max_retries {
            match self.client.get_multiplexed_async_connection().await {
                Ok(conn) => return Ok(conn),
                Err(e) if attempt < max_retries => {
                    log::warn!(
                        "Failed to connect to Redis (attempt {attempt}/{max_retries}): {e}. Retrying in {retry_delay:?}"
                    );
                    sleep(retry_delay).await;
                    retry_delay *= 2; // Exponential backoff
                }
                Err(e) => {
                    return Err(e).context("Failed to connect to Redis after retries");
                }
            }
        }

        Err(anyhow::anyhow!("Failed to connect to Redis after {max_retries} attempts"))
    }

    /// Publish a CloudEvent to a Redis Stream
    ///
    /// # Arguments
    /// * `event` - The CloudEvent to publish
    ///
    /// # Returns
    /// The message ID assigned by Redis
    pub async fn publish(&self, event: CloudEvent<ResultEvent>) -> Result<String> {
        let mut conn = self.get_connection().await?;

        // Serialize the CloudEvent to JSON
        let json_data = serde_json::to_string(&event).context("Failed to serialize CloudEvent")?;

        // Use the topic from the CloudEvent as the stream key
        let stream_key = &event.topic;

        // Publish to Redis Stream using XADD
        let message_id: String = if let Some(max_len) = self.config.max_stream_length {
            // Use MAXLEN with approximate trimming (~)
            conn.xadd_maxlen(
                stream_key,
                redis::streams::StreamMaxlen::Approx(max_len),
                "*",
                &[("data", json_data.as_str())],
            )
            .await
            .context("Failed to publish message to Redis Stream with MAXLEN")?
        } else {
            // No length limit
            conn.xadd(stream_key, "*", &[("data", json_data.as_str())])
                .await
                .context("Failed to publish message to Redis Stream")?
        };

        log::debug!("Published CloudEvent to stream '{stream_key}' with message ID: {message_id}");

        Ok(message_id)
    }

    /// Publish with custom retry logic
    pub async fn publish_with_retry(
        &self,
        event: CloudEvent<ResultEvent>,
        max_retries: usize,
    ) -> Result<String> {
        let mut retry_delay = Duration::from_millis(100);

        for attempt in 1..=max_retries {
            match self.publish(event.clone()).await {
                Ok(message_id) => return Ok(message_id),
                Err(e) if attempt < max_retries => {
                    log::warn!(
                        "Failed to publish CloudEvent (attempt {attempt}/{max_retries}): {e}. Retrying in {retry_delay:?}"
                    );
                    sleep(retry_delay).await;
                    retry_delay *= 2; // Exponential backoff
                }
                Err(e) => {
                    return Err(e).context("Failed to publish CloudEvent after retries");
                }
            }
        }

        Err(anyhow::anyhow!("Failed to publish CloudEvent after {max_retries} attempts"))
    }

    /// Publish multiple CloudEvents in a single Redis pipeline
    ///
    /// # Arguments
    /// * `events` - Vector of CloudEvents to publish
    ///
    /// # Returns
    /// Vector of message IDs assigned by Redis, in the same order as input events
    pub async fn publish_batch(&self, events: Vec<CloudEvent<ResultEvent>>) -> Result<Vec<String>> {
        if events.is_empty() {
            return Ok(Vec::new());
        }

        let mut conn = self.get_connection().await?;

        // Build a Redis pipeline for all events
        let mut pipe = redis::pipe();
        pipe.atomic();

        for event in &events {
            // Serialize the CloudEvent to JSON
            let json_data =
                serde_json::to_string(event).context("Failed to serialize CloudEvent in batch")?;

            // Use the topic from the CloudEvent as the stream key
            let stream_key = &event.topic;

            // Add XADD command to pipeline
            if let Some(max_len) = self.config.max_stream_length {
                // Use MAXLEN with approximate trimming (~)
                pipe.xadd_maxlen(
                    stream_key,
                    redis::streams::StreamMaxlen::Approx(max_len),
                    "*",
                    &[("data", json_data.as_str())],
                );
            } else {
                // No length limit
                pipe.xadd(stream_key, "*", &[("data", json_data.as_str())]);
            }
        }

        // Execute the pipeline
        let message_ids: Vec<String> = pipe
            .query_async(&mut conn)
            .await
            .context("Failed to execute batch publish pipeline")?;

        log::debug!(
            "Published batch of {} CloudEvents to Redis streams",
            events.len()
        );

        Ok(message_ids)
    }

    /// Publish multiple CloudEvents with retry logic
    ///
    /// # Arguments
    /// * `events` - Vector of CloudEvents to publish
    /// * `max_retries` - Maximum number of retry attempts
    ///
    /// # Returns
    /// Vector of message IDs assigned by Redis
    pub async fn publish_batch_with_retry(
        &self,
        events: Vec<CloudEvent<ResultEvent>>,
        max_retries: usize,
    ) -> Result<Vec<String>> {
        let mut retry_delay = Duration::from_millis(100);

        for attempt in 1..=max_retries {
            match self.publish_batch(events.clone()).await {
                Ok(message_ids) => return Ok(message_ids),
                Err(e) if attempt < max_retries => {
                    log::warn!(
                        "Failed to publish batch of {} CloudEvents (attempt {}/{}): {}. Retrying in {:?}",
                        events.len(),
                        attempt,
                        max_retries,
                        e,
                        retry_delay
                    );
                    sleep(retry_delay).await;
                    retry_delay *= 2; // Exponential backoff
                }
                Err(e) => {
                    log::error!(
                        "Failed to publish batch after {max_retries} retries, falling back to individual publishes"
                    );

                    // Fallback: Try publishing events individually
                    let mut message_ids = Vec::with_capacity(events.len());
                    for event in events {
                        match self.publish_with_retry(event, max_retries).await {
                            Ok(msg_id) => message_ids.push(msg_id),
                            Err(individual_err) => {
                                return Err(e).context(format!(
                                    "Batch publish failed after retries, and individual fallback also failed: {individual_err}"
                                ));
                            }
                        }
                    }
                    return Ok(message_ids);
                }
            }
        }

        Err(anyhow::anyhow!("Failed to publish batch after {max_retries} attempts"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_publisher_config_default() {
        let config = PublisherConfig::default();
        assert!(config.max_stream_length.is_none());
    }

    #[test]
    fn test_create_publisher() {
        let config = PublisherConfig::default();
        let result = RedisStreamPublisher::new("redis://localhost:6379", config);
        // This will succeed even if Redis is not running because we only create the client
        assert!(result.is_ok());
    }

    #[test]
    fn test_create_publisher_with_maxlen() {
        let config = PublisherConfig {
            max_stream_length: Some(10000),
        };
        let result = RedisStreamPublisher::new("redis://localhost:6379", config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_invalid_redis_url() {
        let config = PublisherConfig::default();
        let result = RedisStreamPublisher::new("invalid-url", config);
        assert!(result.is_err());
    }

    // Integration tests with real Redis would go in tests/platform_reaction_integration.rs
}
