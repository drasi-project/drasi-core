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
//! use drasi_lib::reactions::common::AdaptiveBatchConfig;
//!
//! let reaction = HttpReaction::builder("my-http-reaction")
//!     .with_base_url("http://api.example.com")
//!     .with_query("my-query")
//!     .with_adaptive_batching(AdaptiveBatchConfig {
//!         adaptive_min_batch_size: 10,
//!         adaptive_max_batch_size: 500,
//!         ..Default::default()
//!     })
//!     .build()?;
//! ```
//!
//! When adaptive batching is enabled, results are sent to `{base_url}{batch_endpoint_path}`
//! as a JSON array of `BatchResult` objects.

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
/// use drasi_lib::reactions::common::AdaptiveBatchConfig;
///
/// let reaction = HttpReaction::builder("my-http-reaction")
///     .with_base_url("http://api.example.com")
///     .with_query("my-query")
///     .with_adaptive_batching(AdaptiveBatchConfig {
///         adaptive_min_batch_size: 10,
///         adaptive_max_batch_size: 500,
///         ..Default::default()
///     })
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
    /// Adaptive batching configuration (None = disabled, Some = enabled)
    adaptive: Option<AdaptiveBatchConfig>,
    /// Custom batch endpoint path (default: "/batch")
    batch_endpoint_path: String,
    /// Enable HTTP/2 for connection pooling
    http2_enabled: bool,
}

impl HttpReactionBuilder {
    /// Create a new HTTP reaction builder with the given ID
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            base_url: "http://localhost".to_string(),
            token: None,
            timeout_ms: 5000,
            routes: HashMap::new(),
            priority_queue_capacity: None,
            auto_start: true,
            adaptive: None,
            batch_endpoint_path: "/batch".to_string(),
            http2_enabled: false,
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

    /// Enable adaptive batching with the given configuration.
    ///
    /// When enabled, results are intelligently batched based on throughput patterns
    /// and sent to `{base_url}{batch_endpoint_path}`. When not called (default),
    /// each result is sent as an individual HTTP request.
    ///
    /// # Example
    /// ```rust,ignore
    /// use drasi_lib::reactions::common::AdaptiveBatchConfig;
    ///
    /// builder.with_adaptive_batching(AdaptiveBatchConfig {
    ///     adaptive_min_batch_size: 10,
    ///     adaptive_max_batch_size: 500,
    ///     ..Default::default()
    /// })
    /// ```
    pub fn with_adaptive_batching(mut self, config: AdaptiveBatchConfig) -> Self {
        self.adaptive = Some(config);
        self
    }

    /// Set a custom batch endpoint path (default: "/batch").
    ///
    /// Only used when adaptive batching is enabled. Batch requests are sent
    /// to `{base_url}{batch_endpoint_path}`.
    pub fn with_batch_endpoint_path(mut self, path: impl Into<String>) -> Self {
        self.batch_endpoint_path = path.into();
        self
    }

    /// Enable HTTP/2 for connection pooling.
    ///
    /// When true, the HTTP client uses HTTP/2 with connection pooling,
    /// which can improve throughput for adaptive batching scenarios.
    /// Default: false (automatically set to true when adaptive batching is enabled
    /// unless explicitly overridden).
    pub fn with_http2_enabled(mut self, enabled: bool) -> Self {
        self.http2_enabled = enabled;
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: HttpReactionConfig) -> Self {
        self.base_url = config.base_url;
        self.token = config.token;
        self.timeout_ms = config.timeout_ms;
        self.routes = config.routes;
        self.adaptive = config.adaptive;
        self.batch_endpoint_path = config.batch_endpoint_path;
        self.http2_enabled = config.http2_enabled;
        self
    }

    /// Build the HTTP reaction
    pub fn build(self) -> anyhow::Result<HttpReaction> {
        let http2_enabled = if self.adaptive.is_some() && !self.http2_enabled {
            // Default to HTTP/2 when adaptive batching is enabled
            true
        } else {
            self.http2_enabled
        };

        let config = HttpReactionConfig {
            base_url: self.base_url,
            token: self.token,
            timeout_ms: self.timeout_ms,
            routes: self.routes,
            adaptive: self.adaptive,
            batch_endpoint_path: self.batch_endpoint_path,
            http2_enabled,
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
            ..Default::default()
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
            .with_adaptive_batching(AdaptiveBatchConfig {
                adaptive_min_batch_size: 10,
                adaptive_max_batch_size: 500,
                adaptive_window_size: 50,
                adaptive_batch_timeout_ms: 2000,
            })
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

    #[test]
    fn test_http_builder_adaptive_defaults_http2() {
        // When adaptive batching is enabled, http2 should default to true
        let reaction = HttpReaction::builder("adaptive-reaction")
            .with_adaptive_batching(AdaptiveBatchConfig::default())
            .build()
            .unwrap();

        assert!(reaction.is_adaptive_enabled());
        let props = reaction.properties();
        assert_eq!(
            props.get("http2_enabled"),
            Some(&serde_json::Value::Bool(true))
        );
    }

    #[test]
    fn test_http_builder_custom_batch_endpoint_path() {
        let reaction = HttpReaction::builder("test-reaction")
            .with_adaptive_batching(AdaptiveBatchConfig::default())
            .with_batch_endpoint_path("/webhooks/batch")
            .build()
            .unwrap();

        let props = reaction.properties();
        assert_eq!(
            props.get("batch_endpoint_path"),
            Some(&serde_json::Value::String("/webhooks/batch".to_string()))
        );
    }

    #[tokio::test]
    async fn test_adaptive_batching_forms_batches() {
        use crate::adaptive_batcher::{
            AdaptiveBatchConfig as InternalBatchConfig, AdaptiveBatcher,
        };
        use tokio::sync::mpsc;

        let config = InternalBatchConfig {
            max_batch_size: 50,
            min_batch_size: 5,
            max_wait_time: std::time::Duration::from_millis(500),
            min_wait_time: std::time::Duration::from_millis(1),
            throughput_window: std::time::Duration::from_secs(5),
            adaptive_enabled: true,
        };

        let (tx, rx) = mpsc::channel(config.recommended_channel_capacity());
        let mut batcher = AdaptiveBatcher::new(rx, config);

        // Send 20 messages rapidly
        for i in 0..20 {
            tx.send(i).await.unwrap();
        }

        // The batcher should collect them into a batch
        let batch = batcher.next_batch().await.unwrap();
        assert!(!batch.is_empty(), "Batch should not be empty");
        assert!(batch.len() > 1, "Rapid sends should result in batching");
    }

    #[tokio::test]
    async fn test_adaptive_batching_respects_timeout() {
        use crate::adaptive_batcher::{
            AdaptiveBatchConfig as InternalBatchConfig, AdaptiveBatcher,
        };
        use tokio::sync::mpsc;

        let config = InternalBatchConfig {
            max_batch_size: 100,
            min_batch_size: 10,
            max_wait_time: std::time::Duration::from_millis(100),
            min_wait_time: std::time::Duration::from_millis(10),
            throughput_window: std::time::Duration::from_secs(5),
            adaptive_enabled: true,
        };

        let (tx, rx) = mpsc::channel(config.recommended_channel_capacity());
        let mut batcher = AdaptiveBatcher::new(rx, config);

        // Send only 2 messages (well below min_batch_size of 10)
        tx.send(1).await.unwrap();
        tx.send(2).await.unwrap();

        // The batcher should flush the partial batch after the timeout
        let batch = batcher.next_batch().await.unwrap();
        assert_eq!(batch.len(), 2, "Partial batch should flush on timeout");
    }
}
