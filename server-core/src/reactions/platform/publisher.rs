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
#[derive(Debug, Clone)]
pub struct PublisherConfig {
    /// Maximum stream length (optional, for MAXLEN policy)
    pub max_stream_length: Option<usize>,
}

impl Default for PublisherConfig {
    fn default() -> Self {
        Self {
            max_stream_length: None,
        }
    }
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
                        "Failed to connect to Redis (attempt {}/{}): {}. Retrying in {:?}",
                        attempt,
                        max_retries,
                        e,
                        retry_delay
                    );
                    sleep(retry_delay).await;
                    retry_delay *= 2; // Exponential backoff
                }
                Err(e) => {
                    return Err(e).context("Failed to connect to Redis after retries");
                }
            }
        }

        unreachable!("Loop should have returned or errored");
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

        log::debug!(
            "Published CloudEvent to stream '{}' with message ID: {}",
            stream_key,
            message_id
        );

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
                        "Failed to publish CloudEvent (attempt {}/{}): {}. Retrying in {:?}",
                        attempt,
                        max_retries,
                        e,
                        retry_delay
                    );
                    sleep(retry_delay).await;
                    retry_delay *= 2; // Exponential backoff
                }
                Err(e) => {
                    return Err(e).context("Failed to publish CloudEvent after retries");
                }
            }
        }

        unreachable!("Loop should have returned or errored");
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
