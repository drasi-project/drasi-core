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

//! HTTP Adaptive reaction plugin for Drasi
//!
//! This plugin implements HTTP Adaptive reactions for Drasi and provides extension traits
//! for configuring HTTP Adaptive reactions in the Drasi plugin architecture.
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_reaction_http_adaptive::AdaptiveHttpReaction;
//!
//! let reaction = AdaptiveHttpReaction::builder("my-adaptive-reaction")
//!     .with_queries(vec!["query1".to_string()])
//!     .with_base_url("http://api.example.com")
//!     .with_token("secret-token")
//!     .with_timeout_ms(10000)
//!     .with_max_batch_size(500)
//!     .build()?;
//! ```

mod adaptive_batcher;
pub mod config;
pub mod http_adaptive;

pub use config::HttpAdaptiveReactionConfig;
pub use http_adaptive::AdaptiveHttpReaction;

use drasi_lib::reactions::common::AdaptiveBatchConfig;
use drasi_reaction_http::QueryConfig;
use std::collections::HashMap;

/// Builder for HTTP Adaptive reaction
///
/// Creates an AdaptiveHttpReaction instance with a fluent API.
pub struct HttpAdaptiveReactionBuilder {
    id: String,
    queries: Vec<String>,
    base_url: String,
    token: Option<String>,
    timeout_ms: u64,
    routes: HashMap<String, QueryConfig>,
    adaptive: AdaptiveBatchConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl HttpAdaptiveReactionBuilder {
    /// Create a new HTTP Adaptive reaction builder with the given ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            base_url: "http://localhost".to_string(),
            token: None,
            timeout_ms: 5000,
            routes: HashMap::new(),
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

    /// Set the base URL for HTTP requests
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Set the authentication token
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Set the request timeout in milliseconds
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = timeout_ms;
        self
    }

    /// Add a route configuration for a specific query
    pub fn with_route(mut self, query_id: impl Into<String>, config: QueryConfig) -> Self {
        self.routes.insert(query_id.into(), config);
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

    /// Set the adaptive window size (in 100ms intervals)
    pub fn with_window_size(mut self, size: usize) -> Self {
        self.adaptive.adaptive_window_size = size;
        self
    }

    /// Set the batch timeout in milliseconds
    pub fn with_batch_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.adaptive.adaptive_batch_timeout_ms = timeout_ms;
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
    pub fn with_config(mut self, config: HttpAdaptiveReactionConfig) -> Self {
        self.base_url = config.base_url;
        self.token = config.token;
        self.timeout_ms = config.timeout_ms;
        self.routes = config.routes;
        self.adaptive = config.adaptive;
        self
    }

    /// Build the HTTP Adaptive reaction
    pub fn build(self) -> anyhow::Result<AdaptiveHttpReaction> {
        let config = HttpAdaptiveReactionConfig {
            base_url: self.base_url,
            token: self.token,
            timeout_ms: self.timeout_ms,
            routes: self.routes,
            adaptive: self.adaptive,
        };

        Ok(AdaptiveHttpReaction::from_builder(
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
    fn test_adaptive_http_builder_defaults() {
        // devskim: ignore DS137138: Unit test only – no actual network request is made
        let reaction = HttpAdaptiveReactionBuilder::new("test-reaction")
            .build()
            .unwrap();
        assert_eq!(reaction.id(), "test-reaction");
        let props = reaction.properties();
        assert_eq!(
            props.get("base_url"),
            Some(&serde_json::Value::String("http://localhost".to_string()))
        );
    }

    #[test]
    fn test_adaptive_http_builder_custom_values() {
        // devskim: ignore DS137138: Unit test only – no actual network request is made
        let reaction = AdaptiveHttpReaction::builder("test-reaction")
            .with_base_url("http://api.example.com")
            .with_token("secret-token")
            .with_timeout_ms(10000)
            .with_queries(vec!["query1".to_string()])
            .with_max_batch_size(500)
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    }

    #[test]
    fn test_adaptive_http_new_constructor() {
        let config = HttpAdaptiveReactionConfig::default();

        let reaction =
            AdaptiveHttpReaction::new("test-reaction", vec!["query1".to_string()], config);

        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    }
}
