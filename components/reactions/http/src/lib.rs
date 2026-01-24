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

//! HTTP reaction plugin for Drasi
//!
//! This plugin implements HTTP reactions for Drasi and provides extension traits
//! for configuring HTTP reactions in the Drasi plugin architecture.
//!
//! ## Features
//!
//! - **Standard Mode**: Send individual HTTP requests for each query result
//! - **Adaptive Batching Mode**: Intelligently batch results based on throughput patterns
//! - **Template Support**: Handlebars templates for dynamic URLs, bodies, and headers
//! - **Multiple HTTP Methods**: GET, POST, PUT, DELETE, PATCH
//! - **Authentication**: Built-in Bearer token support
//!
//! ## Standard Usage
//!
//! ```rust,ignore
//! use drasi_reaction_http::HttpReaction;
//!
//! let reaction = HttpReaction::builder("my-http-reaction")
//!     .with_base_url("http://api.example.com")
//!     .with_token("secret-token")
//!     .with_query("my-query")
//!     .build()?;
//! ```
//!
//! ## Adaptive Batching Usage
//!
//! Enable intelligent batching for high-throughput scenarios:
//!
//! ```rust,ignore
//! use drasi_reaction_http::HttpReaction;
//!
//! let reaction = HttpReaction::builder("my-http-reaction")
//!     .with_base_url("http://api.example.com")
//!     .with_query("my-query")
//!     .with_adaptive_enabled(true)
//!     .with_min_batch_size(10)
//!     .with_max_batch_size(500)
//!     .with_window_size(50)  // 5 seconds
//!     .with_batch_timeout_ms(1000)
//!     .build()?;
//! ```
//!
//! When adaptive batching is enabled, results are sent to `{base_url}/batch` as a JSON
//! array of `BatchResult` objects.

pub mod adaptive_batcher;
pub mod config;
pub mod http;

pub use config::{CallSpec, HttpReactionConfig, QueryConfig};
pub use http::{BatchResult, HttpReaction};

use drasi_lib::reactions::common::AdaptiveBatchConfig;
use std::collections::HashMap;

/// Builder for HTTP reaction
///
/// Creates an HttpReaction instance with a fluent API. Supports both standard
/// (individual request) mode and adaptive batching mode.
///
/// # Standard Mode Example
/// ```rust,ignore
/// use drasi_reaction_http::HttpReaction;
///
/// let reaction = HttpReaction::builder("my-http-reaction")
///     .with_queries(vec!["query1".to_string()])
///     .with_base_url("http://api.example.com")
///     .with_token("secret-token")
///     .with_timeout_ms(10000)
///     .build()?;
/// ```
///
/// # Adaptive Batching Example
/// ```rust,ignore
/// use drasi_reaction_http::HttpReaction;
///
/// let reaction = HttpReaction::builder("my-http-reaction")
///     .with_base_url("http://api.example.com")
///     .with_query("my-query")
///     .with_adaptive_enabled(true)
///     .with_min_batch_size(10)
///     .with_max_batch_size(500)
///     .with_window_size(50)  // 5 seconds (50 × 100ms)
///     .with_batch_timeout_ms(1000)
///     .build()?;
/// ```
pub struct HttpReactionBuilder {
    id: String,
    queries: Vec<String>,
    base_url: String,
    token: Option<String>,
    timeout_ms: u64,
    routes: HashMap<String, QueryConfig>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    // Adaptive batching configuration
    adaptive_enabled: bool,
    adaptive_min_batch_size: usize,
    adaptive_max_batch_size: usize,
    adaptive_window_size: usize,
    adaptive_batch_timeout_ms: u64,
}

impl HttpReactionBuilder {
    /// Create a new HTTP reaction builder with the given ID
    pub fn new(id: impl Into<String>) -> Self {
        let default_adaptive = AdaptiveBatchConfig::default();
        Self {
            id: id.into(),
            queries: Vec::new(),
            base_url: "http://localhost".to_string(),
            token: None,
            timeout_ms: 5000,
            routes: HashMap::new(),
            priority_queue_capacity: None,
            auto_start: true,
            // Adaptive batching defaults (disabled by default)
            adaptive_enabled: false,
            adaptive_min_batch_size: default_adaptive.adaptive_min_batch_size,
            adaptive_max_batch_size: default_adaptive.adaptive_max_batch_size,
            adaptive_window_size: default_adaptive.adaptive_window_size,
            adaptive_batch_timeout_ms: default_adaptive.adaptive_batch_timeout_ms,
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

    /// Enable or disable adaptive batching.
    ///
    /// When enabled, results are intelligently batched based on throughput patterns
    /// and sent to `{base_url}/batch`. When disabled (default), each result is sent
    /// as an individual HTTP request.
    pub fn with_adaptive_enabled(mut self, enabled: bool) -> Self {
        self.adaptive_enabled = enabled;
        self
    }

    /// Set the minimum batch size
    pub fn with_min_batch_size(mut self, size: usize) -> Self {
        self.adaptive_min_batch_size = size;
        self
    }

    /// Set the maximum batch size
    pub fn with_max_batch_size(mut self, size: usize) -> Self {
        self.adaptive_max_batch_size = size;
        self
    }

    /// Set the adaptive window size
    pub fn with_window_size(mut self, size: usize) -> Self {
        self.adaptive_window_size = size;
        self
    }

    /// Set the batch timeout in milliseconds
    pub fn with_batch_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.adaptive_batch_timeout_ms = timeout_ms;
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: HttpReactionConfig) -> Self {
        self.base_url = config.base_url;
        self.token = config.token;
        self.timeout_ms = config.timeout_ms;
        self.routes = config.routes;
        if let Some(adaptive) = config.adaptive {
            self.adaptive_enabled = true;
            self.adaptive_min_batch_size = adaptive.adaptive_min_batch_size;
            self.adaptive_max_batch_size = adaptive.adaptive_max_batch_size;
            self.adaptive_window_size = adaptive.adaptive_window_size;
            self.adaptive_batch_timeout_ms = adaptive.adaptive_batch_timeout_ms;
        } else {
            self.adaptive_enabled = false;
        }
        self
    }

    /// Build the HTTP reaction
    pub fn build(self) -> anyhow::Result<HttpReaction> {
        let adaptive = if self.adaptive_enabled {
            Some(AdaptiveBatchConfig {
                adaptive_min_batch_size: self.adaptive_min_batch_size,
                adaptive_max_batch_size: self.adaptive_max_batch_size,
                adaptive_window_size: self.adaptive_window_size,
                adaptive_batch_timeout_ms: self.adaptive_batch_timeout_ms,
            })
        } else {
            None
        };

        let config = HttpReactionConfig {
            base_url: self.base_url,
            token: self.token,
            timeout_ms: self.timeout_ms,
            routes: self.routes,
            adaptive,
        };

        Ok(HttpReaction::from_builder(
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
    fn test_http_builder_defaults() {
        let reaction = HttpReactionBuilder::new("test-reaction").build().unwrap();
        assert_eq!(reaction.id(), "test-reaction");
        let props = reaction.properties();
        assert_eq!(
            props.get("base_url"),
            Some(&serde_json::Value::String("http://localhost".to_string()))
        );
        assert_eq!(
            props.get("timeout_ms"),
            Some(&serde_json::Value::Number(5000.into()))
        );
    }

    #[test]
    fn test_http_builder_custom_values() {
        let reaction = HttpReaction::builder("test-reaction")
            .with_base_url("http://api.example.com") // DevSkim: ignore DS137138
            .with_token("secret-token")
            .with_timeout_ms(10000)
            .with_queries(vec!["query1".to_string()])
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "test-reaction");
        let props = reaction.properties();
        assert_eq!(
            props.get("base_url"),
            Some(&serde_json::Value::String(
                "http://api.example.com".to_string() // DevSkim: ignore DS137138
            ))
        );
        assert_eq!(
            props.get("timeout_ms"),
            Some(&serde_json::Value::Number(10000.into()))
        );
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    }

    #[test]
    fn test_http_builder_with_query() {
        let reaction = HttpReaction::builder("test-reaction")
            .with_query("query1")
            .with_query("query2")
            .build()
            .unwrap();

        assert_eq!(reaction.query_ids(), vec!["query1", "query2"]);
    }

    #[test]
    fn test_http_new_constructor() {
        let config = HttpReactionConfig {
            base_url: "http://test.example.com".to_string(), // DevSkim: ignore DS137138
            token: Some("test-token".to_string()),
            timeout_ms: 3000,
            routes: Default::default(),
            adaptive: None,
        };

        let reaction = HttpReaction::new("test-reaction", vec!["query1".to_string()], config);

        assert_eq!(reaction.id(), "test-reaction");
        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
    }

    #[test]
    fn test_http_builder_with_adaptive() {
        let reaction = HttpReaction::builder("adaptive-reaction")
            .with_base_url("http://api.example.com") // DevSkim: ignore DS137138
            .with_queries(vec!["query1".to_string()])
            .with_adaptive_enabled(true)
            .with_min_batch_size(10)
            .with_max_batch_size(500)
            .with_window_size(50)
            .with_batch_timeout_ms(2000)
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "adaptive-reaction");
        assert!(reaction.is_adaptive_enabled());
    }

    #[test]
    fn test_http_builder_without_adaptive() {
        let reaction = HttpReaction::builder("standard-reaction")
            .with_base_url("http://api.example.com") // DevSkim: ignore DS137138
            .with_queries(vec!["query1".to_string()])
            .build()
            .unwrap();

        assert_eq!(reaction.id(), "standard-reaction");
        assert!(!reaction.is_adaptive_enabled());
    }
}
