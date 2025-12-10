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

//! gRPC reaction plugin for Drasi
//!
//! This plugin implements gRPC reactions for Drasi.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_grpc::GrpcReaction;
//!
//! let reaction = GrpcReaction::builder("my-grpc-reaction")
//!     .with_queries(vec!["query1".to_string()])
//!     .with_endpoint("grpc://localhost:50052")
//!     .with_batch_size(200)
//!     .with_timeout_ms(10000)
//!     .build()?;
//! ```

pub mod config;
pub mod connection;
pub mod grpc;
pub mod helpers;
pub mod proto;

pub use config::GrpcReactionConfig;
pub use grpc::GrpcReaction;

// Re-export types for plugin-grpc-adaptive
pub use helpers::convert_json_to_proto_struct;
pub use proto::{
    ProcessResultsRequest, ProtoQueryResult, ProtoQueryResultItem, ReactionServiceClient,
};

use std::collections::HashMap;

/// Builder for gRPC reaction
///
/// Creates a GrpcReaction instance with a fluent API.
pub struct GrpcReactionBuilder {
    id: String,
    queries: Vec<String>,
    endpoint: String,
    timeout_ms: u64,
    batch_size: usize,
    batch_flush_timeout_ms: u64,
    max_retries: u32,
    connection_retry_attempts: u32,
    initial_connection_timeout_ms: u64,
    metadata: HashMap<String, String>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl GrpcReactionBuilder {
    /// Create a new gRPC reaction builder with the given ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            endpoint: "grpc://localhost:50052".to_string(),
            timeout_ms: 5000,
            batch_size: 100,
            batch_flush_timeout_ms: 1000,
            max_retries: 3,
            connection_retry_attempts: 5,
            initial_connection_timeout_ms: 10000,
            metadata: HashMap::new(),
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

    /// Set the batch size
    pub fn with_batch_size(mut self, batch_size: usize) -> Self {
        self.batch_size = batch_size;
        self
    }

    /// Set the batch flush timeout in milliseconds
    pub fn with_batch_flush_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.batch_flush_timeout_ms = timeout_ms;
        self
    }

    /// Set the maximum number of retries for failed requests
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Set the number of connection retry attempts
    pub fn with_connection_retry_attempts(mut self, attempts: u32) -> Self {
        self.connection_retry_attempts = attempts;
        self
    }

    /// Set the initial connection timeout in milliseconds
    pub fn with_initial_connection_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.initial_connection_timeout_ms = timeout_ms;
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
    pub fn with_config(mut self, config: GrpcReactionConfig) -> Self {
        self.endpoint = config.endpoint;
        self.timeout_ms = config.timeout_ms;
        self.batch_size = config.batch_size;
        self.batch_flush_timeout_ms = config.batch_flush_timeout_ms;
        self.max_retries = config.max_retries;
        self.connection_retry_attempts = config.connection_retry_attempts;
        self.initial_connection_timeout_ms = config.initial_connection_timeout_ms;
        self.metadata = config.metadata;
        self
    }

    /// Build the gRPC reaction
    pub fn build(self) -> anyhow::Result<GrpcReaction> {
        let config = GrpcReactionConfig {
            endpoint: self.endpoint,
            timeout_ms: self.timeout_ms,
            batch_size: self.batch_size,
            batch_flush_timeout_ms: self.batch_flush_timeout_ms,
            max_retries: self.max_retries,
            connection_retry_attempts: self.connection_retry_attempts,
            initial_connection_timeout_ms: self.initial_connection_timeout_ms,
            metadata: self.metadata,
        };

        Ok(GrpcReaction::from_builder(
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
    use drasi_lib::plugin_core::Reaction;

    #[test]
    fn test_grpc_builder_defaults() {
        let reaction = GrpcReactionBuilder::new("test-reaction").build().unwrap();
        assert_eq!(reaction.id(), "test-reaction");
        let props = reaction.properties();
        assert_eq!(
            props.get("endpoint"),
            Some(&serde_json::Value::String(
                "grpc://localhost:50052".to_string()
            ))
        );
        assert_eq!(
            props.get("batch_size"),
            Some(&serde_json::Value::Number(100.into()))
        );
    }

    #[test]
    fn test_grpc_builder_custom_values() {
        let reaction = GrpcReaction::builder("test-reaction")
            .with_endpoint("grpc://api.example.com:50052")
            .with_timeout_ms(10000)
            .with_batch_size(200)
            .with_queries(vec!["query1".to_string()])
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    }

    #[test]
    fn test_grpc_new_constructor() {
        let config = GrpcReactionConfig::default();

        let reaction = GrpcReaction::new("test-reaction", vec!["query1".to_string()], config);

        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    }
}
