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

//! Configuration types for HTTP reactions.
//!
//! This module contains configuration types for HTTP reaction, including
//! adaptive batching configuration.

use drasi_lib::reactions::common::AdaptiveBatchConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_base_url() -> String {
    "http://localhost".to_string()
}

fn default_timeout_ms() -> u64 {
    5000
}

/// Specification for an HTTP call, including URL, method, headers, and body template.
///
/// This type is used to configure HTTP requests for different operation types (added, updated, deleted).
/// All fields support Handlebars template syntax for dynamic content generation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CallSpec {
    /// URL path (appended to base_url) or absolute URL.
    /// Supports Handlebars templates for dynamic URLs.
    pub url: String,

    /// HTTP method: GET, POST, PUT, DELETE, or PATCH (case-insensitive).
    pub method: String,

    /// Request body as a Handlebars template.
    /// If empty, sends the raw JSON data.
    #[serde(default)]
    pub body: String,

    /// Additional HTTP headers as key-value pairs.
    /// Header values support Handlebars templates.
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

/// Configuration for query-specific HTTP calls.
///
/// Defines different HTTP call specifications for each operation type (added, updated, deleted).
/// Each operation type can have its own URL, method, body template, and headers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryConfig {
    /// HTTP call specification for ADD operations (new rows in query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<CallSpec>,

    /// HTTP call specification for UPDATE operations (modified rows in query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<CallSpec>,

    /// HTTP call specification for DELETE operations (removed rows from query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<CallSpec>,
}

/// HTTP reaction configuration
///
/// This configuration supports both standard (non-batching) and adaptive batching modes:
///
/// - **Standard mode** (default): Each query result is sent as an individual HTTP request.
///   This is the default behavior when `adaptive` is `None`.
///
/// - **Adaptive batching mode**: Results are intelligently batched based on throughput patterns.
///   Enable by setting `adaptive` to `Some(AdaptiveBatchConfig { ... })`.
///   Batches are sent to `{base_url}/batch` endpoint.
///
/// # Example
///
/// ```rust,ignore
/// use drasi_reaction_http::HttpReactionConfig;
/// use drasi_lib::reactions::common::AdaptiveBatchConfig;
///
/// // Standard mode (no batching)
/// let config = HttpReactionConfig {
///     base_url: "https://api.example.com".to_string(),
///     adaptive: None,  // or just omit this field
///     ..Default::default()
/// };
///
/// // Adaptive batching mode
/// let config = HttpReactionConfig {
///     base_url: "https://api.example.com".to_string(),
///     adaptive: Some(AdaptiveBatchConfig {
///         adaptive_min_batch_size: 10,
///         adaptive_max_batch_size: 500,
///         adaptive_window_size: 50,  // 5 seconds
///         adaptive_batch_timeout_ms: 1000,
///     }),
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpReactionConfig {
    /// Base URL for HTTP requests.
    ///
    /// In adaptive batching mode, batch requests are sent to `{base_url}/batch`.
    #[serde(default = "default_base_url")]
    pub base_url: String,

    /// Optional bearer token for authentication.
    ///
    /// If provided, adds `Authorization: Bearer {token}` header to all requests.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Query-specific call configurations
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,

    /// Adaptive batching configuration.
    ///
    /// When `Some(...)`, enables intelligent batching based on throughput patterns:
    /// - Low traffic: Results sent immediately for minimal latency
    /// - Medium traffic: Moderate batching for balanced performance
    /// - High traffic: Large batches for network efficiency
    /// - Burst traffic: Maximum batches to handle spikes
    ///
    /// Batches are sent to `{base_url}/batch` as a JSON array of `BatchResult` objects.
    ///
    /// When `None` (default), each result is sent as an individual HTTP request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adaptive: Option<AdaptiveBatchConfig>,
}

impl Default for HttpReactionConfig {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            token: None,
            timeout_ms: default_timeout_ms(),
            routes: HashMap::new(),
            adaptive: None,
        }
    }
}
