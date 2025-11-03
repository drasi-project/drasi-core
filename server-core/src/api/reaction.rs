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

//! Reaction configuration builders

use crate::api::Properties;
use crate::config::{ReactionConfig, ReactionSpecificConfig};
use crate::config::typed::*;
use serde_json::Value;
use std::collections::HashMap;

/// Fluent builder for Reaction configuration
#[derive(Debug, Clone)]
pub struct ReactionBuilder {
    id: String,
    reaction_type: String,
    queries: Vec<String>,
    auto_start: bool,
    properties: HashMap<String, Value>,
    priority_queue_capacity: Option<usize>,
}

impl ReactionBuilder {
    /// Create a new reaction builder
    fn new(id: impl Into<String>, reaction_type: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            reaction_type: reaction_type.into(),
            queries: Vec::new(),
            auto_start: true,
            properties: HashMap::new(),
            priority_queue_capacity: None,
        }
    }

    /// Subscribe to a query
    pub fn subscribe_to(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Subscribe to multiple queries
    pub fn subscribe_to_queries(mut self, query_ids: Vec<String>) -> Self {
        self.queries.extend(query_ids);
        self
    }

    /// Set whether to auto-start this reaction (default: true)
    pub fn auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Add a property
    pub fn with_property(mut self, key: impl Into<String>, value: Value) -> Self {
        self.properties.insert(key.into(), value);
        self
    }

    /// Set properties using the Properties builder
    pub fn with_properties(mut self, properties: Properties) -> Self {
        self.properties = properties.build();
        self
    }

    /// Set the priority queue capacity for this reaction
    ///
    /// This overrides the global default priority queue capacity.
    /// Controls the internal event buffering capacity for timestamp-ordered processing.
    ///
    /// Default: Inherits from server global setting (or 10000 if not specified)
    ///
    /// Recommended values:
    /// - Critical reactions: 100000-1000000 (high reliability)
    /// - Normal reactions: 10000 (default)
    /// - Memory-constrained: 1000-5000
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Build the reaction configuration
    pub fn build(self) -> ReactionConfig {
        // Convert properties HashMap to typed config based on reaction_type
        let config = self.build_typed_config();

        ReactionConfig {
            id: self.id,
            queries: self.queries,
            auto_start: self.auto_start,
            config,
            priority_queue_capacity: self.priority_queue_capacity,
        }
    }

    /// Helper to build typed config from properties
    fn build_typed_config(&self) -> ReactionSpecificConfig {
        match self.reaction_type.as_str() {
            "log" => {
                let log_level = self.properties
                    .get("log_level")
                    .and_then(|v| v.as_str())
                    .unwrap_or("info")
                    .to_string();
                ReactionSpecificConfig::Log(LogReactionConfig { log_level })
            }
            "http" => {
                let base_url = self.properties
                    .get("base_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("http://localhost")
                    .to_string();
                let token = self.properties
                    .get("token")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let timeout_ms = self.properties
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(10000);

                ReactionSpecificConfig::Http(HttpReactionConfig {
                    base_url,
                    token,
                    timeout_ms,
                    queries: HashMap::new(),
                })
            }
            "grpc" => {
                let endpoint = self.properties
                    .get("endpoint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("grpc://localhost:50052")
                    .to_string();
                let token = self.properties
                    .get("token")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let timeout_ms = self.properties
                    .get("timeout_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(5000);
                let batch_size = self.properties
                    .get("batch_size")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                let batch_flush_timeout_ms = self.properties
                    .get("batch_flush_timeout_ms")
                    .and_then(|v| v.as_u64());
                let max_retries = self.properties
                    .get("max_retries")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let connection_retry_attempts = self.properties
                    .get("connection_retry_attempts")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as u32);
                let initial_connection_timeout_ms = self.properties
                    .get("initial_connection_timeout_ms")
                    .and_then(|v| v.as_u64());

                ReactionSpecificConfig::Grpc(GrpcReactionConfig {
                    endpoint,
                    token,
                    timeout_ms,
                    batch_size: batch_size.unwrap_or(10),
                    batch_flush_timeout_ms: batch_flush_timeout_ms.unwrap_or(1000),
                    max_retries: max_retries.unwrap_or(3),
                    connection_retry_attempts: connection_retry_attempts.unwrap_or(5),
                    initial_connection_timeout_ms: initial_connection_timeout_ms.unwrap_or(10000),
                    metadata: HashMap::new(),
                })
            }
            "sse" => {
                let host = self.properties
                    .get("host")
                    .and_then(|v| v.as_str())
                    .unwrap_or("localhost")
                    .to_string();
                let port = self.properties
                    .get("port")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(8080) as u16;
                let sse_path = self.properties
                    .get("sse_path")
                    .and_then(|v| v.as_str())
                    .unwrap_or("/sse")
                    .to_string();
                let heartbeat_interval_ms = self.properties
                    .get("heartbeat_interval_ms")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30000);

                ReactionSpecificConfig::Sse(SseReactionConfig {
                    host,
                    port,
                    sse_path,
                    heartbeat_interval_ms,
                })
            }
            "platform" => {
                let redis_url = self.properties
                    .get("redis_url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("redis://localhost:6379")
                    .to_string();
                let pubsub_name = self.properties
                    .get("pubsub_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("drasi")
                    .to_string();
                let source_name = self.properties
                    .get("source_name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("output")
                    .to_string();
                let max_stream_length = self.properties
                    .get("max_stream_length")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1000) as usize;
                let emit_control_events = self.properties
                    .get("emit_control_events")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let batch_enabled = self.properties
                    .get("batch_enabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let batch_max_size = self.properties
                    .get("batch_max_size")
                    .and_then(|v| v.as_u64())
                    .map(|v| v as usize);
                let batch_max_wait_ms = self.properties
                    .get("batch_max_wait_ms")
                    .and_then(|v| v.as_u64());

                ReactionSpecificConfig::Platform(PlatformReactionConfig {
                    redis_url,
                    pubsub_name: Some(pubsub_name),
                    source_name: Some(source_name),
                    max_stream_length: Some(max_stream_length),
                    emit_control_events,
                    batch_enabled,
                    batch_max_size: batch_max_size.unwrap_or(100),
                    batch_max_wait_ms: batch_max_wait_ms.unwrap_or(100),
                })
            }
            "profiler" => {
                let output_file = self.properties
                    .get("output_file")
                    .and_then(|v| v.as_str())
                    .map(String::from);
                let detailed = self.properties
                    .get("detailed")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let window_size = self.properties
                    .get("window_size")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(1000) as usize;
                let report_interval_secs = self.properties
                    .get("report_interval_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(60);

                ReactionSpecificConfig::Profiler(ProfilerReactionConfig {
                    output_file,
                    detailed,
                    window_size,
                    report_interval_secs,
                })
            }
            "application" => {
                ReactionSpecificConfig::Application(ApplicationReactionConfig {
                    properties: self.properties.clone(),
                })
            }
            _ => {
                // Default to application for unknown types
                ReactionSpecificConfig::Application(ApplicationReactionConfig {
                    properties: self.properties.clone(),
                })
            }
        }
    }
}

/// Reaction configuration factory
pub struct Reaction;

impl Reaction {
    /// Create an application reaction (for programmatic result consumption)
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::Reaction;
    ///
    /// let reaction = Reaction::application("my-reaction")
    ///     .subscribe_to("my-query")
    ///     .build();
    /// ```
    pub fn application(id: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, "application")
    }

    /// Create an HTTP reaction
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::{Reaction, Properties};
    ///
    /// let reaction = Reaction::http("http-reaction")
    ///     .subscribe_to("my-query")
    ///     .with_properties(
    ///         Properties::new()
    ///             .with_string("url", "http://localhost:8080/webhook")
    ///             .with_string("method", "POST")
    ///     )
    ///     .build();
    /// ```
    pub fn http(id: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, "http")
    }

    /// Create a gRPC reaction
    pub fn grpc(id: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, "grpc")
    }

    /// Create an SSE (Server-Sent Events) reaction
    pub fn sse(id: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, "sse")
    }

    /// Create a log reaction (for debugging)
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::Reaction;
    ///
    /// let reaction = Reaction::log("log-reaction")
    ///     .subscribe_to("my-query")
    ///     .build();
    /// ```
    pub fn log(id: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, "log")
    }

    /// Create a custom reaction with specified type
    pub fn custom(id: impl Into<String>, reaction_type: impl Into<String>) -> ReactionBuilder {
        ReactionBuilder::new(id, reaction_type)
    }
}
