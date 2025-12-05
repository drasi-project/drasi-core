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

//! Platform reaction plugin for Drasi
//!
//! This plugin implements Platform reactions for Drasi.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_platform::PlatformReaction;
//!
//! let reaction = PlatformReaction::builder("my-platform-reaction")
//!     .with_queries(vec!["query1".to_string()])
//!     .with_redis_url("redis://localhost:6379")
//!     .with_batch_enabled(true)
//!     .build()?;
//! ```

pub mod config;
pub mod platform;
pub mod publisher;
pub mod transformer;
pub mod types;

pub use config::PlatformReactionConfig;
pub use platform::PlatformReaction;

/// Builder for Platform reaction
pub struct PlatformReactionBuilder {
    id: String,
    queries: Vec<String>,
    redis_url: String,
    pubsub_name: Option<String>,
    source_name: Option<String>,
    max_stream_length: Option<usize>,
    emit_control_events: bool,
    batch_enabled: bool,
    batch_max_size: usize,
    batch_max_wait_ms: u64,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl PlatformReactionBuilder {
    /// Create a new Platform reaction builder with the given ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            redis_url: "redis://localhost:6379".to_string(),
            pubsub_name: None,
            source_name: None,
            max_stream_length: None,
            emit_control_events: false,
            batch_enabled: false,
            batch_max_size: 100,
            batch_max_wait_ms: 100,
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Set the query IDs to subscribe to
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Add a query ID to subscribe to
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Set the Redis URL
    pub fn with_redis_url(mut self, url: impl Into<String>) -> Self {
        self.redis_url = url.into();
        self
    }

    /// Set the PubSub name
    pub fn with_pubsub_name(mut self, name: impl Into<String>) -> Self {
        self.pubsub_name = Some(name.into());
        self
    }

    /// Set the source name
    pub fn with_source_name(mut self, name: impl Into<String>) -> Self {
        self.source_name = Some(name.into());
        self
    }

    /// Set the maximum stream length
    pub fn with_max_stream_length(mut self, length: usize) -> Self {
        self.max_stream_length = Some(length);
        self
    }

    /// Enable control events
    pub fn with_emit_control_events(mut self, emit: bool) -> Self {
        self.emit_control_events = emit;
        self
    }

    /// Enable batching
    pub fn with_batch_enabled(mut self, enabled: bool) -> Self {
        self.batch_enabled = enabled;
        self
    }

    /// Set the maximum batch size
    pub fn with_batch_max_size(mut self, size: usize) -> Self {
        self.batch_max_size = size;
        self
    }

    /// Set the maximum batch wait time in milliseconds
    pub fn with_batch_max_wait_ms(mut self, ms: u64) -> Self {
        self.batch_max_wait_ms = ms;
        self
    }

    /// Set the priority queue capacity
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the reaction should auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Build the Platform reaction
    pub fn build(self) -> anyhow::Result<PlatformReaction> {
        let config = PlatformReactionConfig {
            redis_url: self.redis_url,
            pubsub_name: self.pubsub_name,
            source_name: self.source_name,
            max_stream_length: self.max_stream_length,
            emit_control_events: self.emit_control_events,
            batch_enabled: self.batch_enabled,
            batch_max_size: self.batch_max_size,
            batch_max_wait_ms: self.batch_max_wait_ms,
        };

        PlatformReaction::from_builder(
            self.id,
            self.queries,
            config,
            self.priority_queue_capacity,
            self.auto_start,
        )
    }
}
