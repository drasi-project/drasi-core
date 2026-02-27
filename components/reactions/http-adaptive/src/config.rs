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

//! Configuration types for HTTP adaptive reactions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use drasi_lib::reactions::common::AdaptiveBatchConfig;

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

fn default_base_url() -> String {
    "http://localhost".to_string()
}

fn default_timeout_ms() -> u64 {
    5000
}

/// HTTP Adaptive reaction configuration with adaptive batching
///
/// This reaction extends the standard HTTP reaction with intelligent batching and
/// HTTP/2 connection pooling, automatically adjusting batch size and timing based
/// on throughput patterns.
///
/// # Key Features
///
/// - **Intelligent Batching**: Groups multiple results based on traffic patterns
/// - **Batch Endpoint**: Sends batches to `{base_url}/batch` endpoint
/// - **Adaptive Algorithm**: Dynamically adjusts batch size and wait time
/// - **HTTP/2 Pooling**: Maintains persistent connections for better performance
/// - **Individual Fallback**: Uses query-specific routes for single results
///
/// # Batch Endpoint Format
///
/// Batches are sent as POST requests to `{base_url}/batch` with an array of
/// `BatchResult` objects containing query_id, results array, timestamp, and count.
///
/// # Example
///
/// ```rust,ignore
/// use drasi_lib::config::{ReactionConfig, ReactionSpecificConfig};
/// use drasi_lib::reactions::http_adaptive::HttpAdaptiveReactionConfig;
/// use drasi_lib::reactions::common::AdaptiveBatchConfig;
/// use std::collections::HashMap;
///
/// let config = ReactionConfig {
///     id: "adaptive-webhook".to_string(),
///     queries: vec!["user-changes".to_string()],
///     auto_start: true,
///     config: ReactionSpecificConfig::HttpAdaptive(HttpAdaptiveReactionConfig {
///         base_url: "https://api.example.com".to_string(),
///         token: Some("your-api-token".to_string()),
///         timeout_ms: 10000,
///         routes: HashMap::new(),
///         adaptive: AdaptiveBatchConfig {
///             adaptive_min_batch_size: 20,
///             adaptive_max_batch_size: 500,
///             adaptive_window_size: 10,  // 1 second
///             adaptive_batch_timeout_ms: 1000,
///         },
///     }),
///     priority_queue_capacity: None,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpAdaptiveReactionConfig {
    /// Base URL for HTTP requests (e.g., "https://api.example.com")
    ///
    /// Batch requests are sent to `{base_url}/batch`
    #[serde(default = "default_base_url")]
    pub base_url: String,

    /// Optional bearer token for authentication
    ///
    /// If provided, adds `Authorization: Bearer {token}` header to all requests
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Query-specific route configurations for individual requests
    ///
    /// Used when only single results are available (fallback from batch endpoint).
    /// Maps query IDs to operation-specific call specifications.
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,

    /// Adaptive batching configuration (flattened into parent config)
    #[serde(flatten)]
    pub adaptive: AdaptiveBatchConfig,
}

impl Default for HttpAdaptiveReactionConfig {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            token: None,
            timeout_ms: default_timeout_ms(),
            routes: HashMap::new(),
            adaptive: AdaptiveBatchConfig::default(),
        }
    }
}
