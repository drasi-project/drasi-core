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

//! gRPC Adaptive reaction plugin for Drasi
//!
//! This plugin implements gRPC Adaptive reactions for Drasi.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_grpc_adaptive::AdaptiveGrpcReaction;
//!
//! let reaction = AdaptiveGrpcReaction::builder("my-grpc-adaptive")
//!     .with_queries(vec!["query1".to_string()])
//!     .with_endpoint("grpc://localhost:50052")
//!     .with_max_batch_size(500)
//!     .build()?;
//! ```

mod adaptive_batcher;
pub mod config;
pub mod descriptor;
pub mod grpc_adaptive;
pub mod helpers;
pub mod proto;

pub use config::GrpcAdaptiveReactionConfig;
pub use grpc_adaptive::AdaptiveGrpcReaction;

use drasi_lib::reactions::common::AdaptiveBatchConfig;
use std::collections::HashMap;

/// Builder for gRPC Adaptive reaction
pub struct GrpcAdaptiveReactionBuilder {
    id: String,
    queries: Vec<String>,
    endpoint: String,
    timeout_ms: u64,
    max_retries: u32,
    connection_retry_attempts: u32,
    initial_connection_timeout_ms: u64,
    metadata: HashMap<String, String>,
    adaptive: AdaptiveBatchConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl GrpcAdaptiveReactionBuilder {
    /// Create a new gRPC Adaptive reaction builder with the given ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            endpoint: "grpc://localhost:50052".to_string(),
            timeout_ms: 5000,
            max_retries: 3,
            connection_retry_attempts: 5,
            initial_connection_timeout_ms: 10000,
            metadata: HashMap::new(),
            adaptive: AdaptiveBatchConfig::default(),
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

    /// Set the gRPC endpoint
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Set the request timeout in milliseconds
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Set the maximum number of retries
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Set the minimum batch size
    pub fn with_min_batch_size(mut self, size: usize) -> Self {
        self.adaptive.adaptive_min_batch_size = size;
        self
    }

    /// Set the maximum batch size
    pub fn with_max_batch_size(mut self, size: usize) -> Self {
        self.adaptive.adaptive_max_batch_size = size;
        self
    }

    /// Add metadata header
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
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

    /// Set the full configuration at once
    pub fn with_config(mut self, config: GrpcAdaptiveReactionConfig) -> Self {
        self.endpoint = config.endpoint;
        self.timeout_ms = config.timeout_ms;
        self.max_retries = config.max_retries;
        self.connection_retry_attempts = config.connection_retry_attempts;
        self.initial_connection_timeout_ms = config.initial_connection_timeout_ms;
        self.metadata = config.metadata;
        self.adaptive = config.adaptive;
        self
    }

    /// Build the gRPC Adaptive reaction
    pub fn build(self) -> anyhow::Result<AdaptiveGrpcReaction> {
        let config = GrpcAdaptiveReactionConfig {
            endpoint: self.endpoint,
            timeout_ms: self.timeout_ms,
            max_retries: self.max_retries,
            connection_retry_attempts: self.connection_retry_attempts,
            initial_connection_timeout_ms: self.initial_connection_timeout_ms,
            metadata: self.metadata,
            adaptive: self.adaptive,
        };

        Ok(AdaptiveGrpcReaction::from_builder(
            self.id,
            self.queries,
            config,
            self.priority_queue_capacity,
            self.auto_start,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_lib::Reaction;

    #[test]
    fn test_grpc_adaptive_builder_defaults() {
        let reaction = GrpcAdaptiveReactionBuilder::new("test-reaction")
            .build()
            .unwrap();
        assert_eq!(reaction.id(), "test-reaction");
    }

    #[test]
    fn test_grpc_adaptive_builder_custom() {
        let reaction = AdaptiveGrpcReaction::builder("test-reaction")
            .with_endpoint("grpc://api.example.com:50052")
            .with_queries(vec!["query1".to_string()])
            .with_max_batch_size(500)
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    }

    #[test]
    fn test_grpc_adaptive_new_constructor() {
        let config = GrpcAdaptiveReactionConfig::default();
        let reaction =
            AdaptiveGrpcReaction::new("test-reaction", vec!["query1".to_string()], config);
        assert_eq!(reaction.id(), "test-reaction");
    }
}

/// Dynamic plugin entry point.
///
/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "grpc-adaptive-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::GrpcAdaptiveReactionDescriptor],
    bootstrap_descriptors = [],
);
