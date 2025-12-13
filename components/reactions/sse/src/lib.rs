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

//! Server-Sent Events (SSE) reaction plugin for Drasi
//!
//! This plugin implements SSE reactions for Drasi.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_sse::SseReaction;
//!
//! let reaction = SseReaction::builder("my-sse-reaction")
//!     .with_queries(vec!["query1".to_string()])
//!     .with_host("0.0.0.0")
//!     .with_port(8080)
//!     .build()?;
//! ```

use std::collections::HashMap;

pub mod config;
pub mod sse;

pub use config::{QueryConfig, SseReactionConfig, TemplateSpec};
pub use sse::SseReaction;

/// Builder for SSE reaction
pub struct SseReactionBuilder {
    id: String,
    queries: Vec<String>,
    host: String,
    port: u16,
    sse_path: String,
    heartbeat_interval_ms: u64,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    routes: HashMap<String, QueryConfig>,
}

impl SseReactionBuilder {
    /// Create a new SSE reaction builder with the given ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            host: "0.0.0.0".to_string(),
            port: 8080,
            sse_path: "/events".to_string(),
            heartbeat_interval_ms: 30000,
            priority_queue_capacity: None,
            auto_start: true,
            routes: HashMap::new(),
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

    /// Set the host to bind to
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Set the port to bind to
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set the SSE path
    pub fn with_sse_path(mut self, path: impl Into<String>) -> Self {
        self.sse_path = path.into();
        self
    }

    /// Set the heartbeat interval in milliseconds
    pub fn with_heartbeat_interval_ms(mut self, interval_ms: u64) -> Self {
        self.heartbeat_interval_ms = interval_ms;
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

    /// Add a route configuration for a specific query
    pub fn with_route(mut self, query_id: impl Into<String>, config: QueryConfig) -> Self {
        self.routes.insert(query_id.into(), config);
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: SseReactionConfig) -> Self {
        self.host = config.host;
        self.port = config.port;
        self.sse_path = config.sse_path;
        self.heartbeat_interval_ms = config.heartbeat_interval_ms;
        self.routes = config.routes;
        self
    }

    /// Build the SSE reaction
    pub fn build(self) -> anyhow::Result<SseReaction> {
        let config = SseReactionConfig {
            host: self.host,
            port: self.port,
            sse_path: self.sse_path,
            heartbeat_interval_ms: self.heartbeat_interval_ms,
            routes: self.routes,
        };

        Ok(SseReaction::from_builder(
            self.id,
            self.queries,
            config,
            self.priority_queue_capacity,
            self.auto_start,
        ))
    }
}

#[cfg(test)]
mod tests;
