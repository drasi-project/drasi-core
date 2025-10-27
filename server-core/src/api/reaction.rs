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
use crate::config::ReactionConfig;
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
        ReactionConfig {
            id: self.id,
            reaction_type: self.reaction_type,
            queries: self.queries,
            auto_start: self.auto_start,
            properties: self.properties,
            priority_queue_capacity: self.priority_queue_capacity,
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
