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

//! Configuration types for the platform source plugin.
//!
//! This module defines the configuration structure for connecting to Redis Streams
//! and consuming CloudEvent-wrapped change events.

use serde::{Deserialize, Serialize};

fn default_consumer_group() -> String {
    "drasi-core".to_string()
}

fn default_batch_size() -> usize {
    100
}

fn default_block_ms() -> u64 {
    5000
}

/// Platform source configuration for Redis Streams consumption.
///
/// This configuration defines how the platform source connects to Redis
/// and consumes events from a stream using consumer groups.
///
/// # Example
///
/// ```rust
/// use drasi_plugin_platform::PlatformSourceConfig;
///
/// let config = PlatformSourceConfig {
///     redis_url: "redis://localhost:6379".to_string(),
///     stream_key: "my-app-changes".to_string(),
///     consumer_group: "my-consumers".to_string(),
///     consumer_name: Some("consumer-1".to_string()),
///     batch_size: 50,
///     block_ms: 10000,
/// };
/// ```
///
/// # YAML Configuration
///
/// ```yaml
/// source_type: platform
/// properties:
///   redis_url: "redis://localhost:6379"
///   stream_key: "my-app-changes"
///   consumer_group: "my-consumers"
///   batch_size: 50
///   block_ms: 10000
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlatformSourceConfig {
    /// Redis connection URL.
    ///
    /// Standard Redis connection string format.
    ///
    /// # Examples
    ///
    /// - `redis://localhost:6379` - Local Redis without auth
    /// - `redis://:password@host:6379` - Redis with password
    /// - `redis://user:password@host:6379` - Redis with username and password
    /// - `rediss://host:6379` - Redis with TLS
    pub redis_url: String,

    /// Redis stream key to consume from.
    ///
    /// This is the name of the Redis stream that contains the CloudEvent-wrapped
    /// change events. The stream must exist or will be created automatically
    /// when the consumer group is created with MKSTREAM.
    pub stream_key: String,

    /// Consumer group name.
    ///
    /// All source instances with the same consumer group share the workload.
    /// Each message is delivered to only one consumer in the group.
    ///
    /// **Default**: `"drasi-core"`
    #[serde(default = "default_consumer_group")]
    pub consumer_group: String,

    /// Consumer name (unique within group).
    ///
    /// Identifies this specific consumer instance within the consumer group.
    /// If not specified, a unique name is auto-generated based on the source ID.
    ///
    /// **Default**: Auto-generated from source ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub consumer_name: Option<String>,

    /// Number of events to read per XREADGROUP call.
    ///
    /// Higher values improve throughput but increase memory usage and
    /// may delay processing of individual events.
    ///
    /// **Default**: `100`
    ///
    /// **Valid range**: 1-10000 (recommended)
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// Milliseconds to block waiting for new events.
    ///
    /// When no events are available, the consumer blocks for this duration
    /// before returning an empty result. Higher values reduce CPU usage
    /// but increase latency for detecting source shutdown.
    ///
    /// **Default**: `5000` (5 seconds)
    ///
    /// **Valid range**: 100-60000 (recommended)
    #[serde(default = "default_block_ms")]
    pub block_ms: u64,
}

impl PlatformSourceConfig {
    /// Validate the configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - `redis_url` is empty
    /// - `stream_key` is empty
    /// - `consumer_group` is empty
    /// - `batch_size` is 0
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.redis_url.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: redis_url cannot be empty. \
                 Please provide a valid Redis connection URL (e.g., redis://localhost:6379)"
            ));
        }

        if self.stream_key.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: stream_key cannot be empty. \
                 Please specify the Redis stream key to consume from"
            ));
        }

        if self.consumer_group.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: consumer_group cannot be empty. \
                 Please specify a consumer group name"
            ));
        }

        if self.batch_size == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: batch_size cannot be 0. \
                 Please specify a positive batch size"
            ));
        }

        Ok(())
    }
}
