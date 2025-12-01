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
//! This module contains configuration types for HTTP reaction and shared types
//! used by HTTP Adaptive reaction implementations.

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpReactionConfig {
    /// Base URL for HTTP requests
    #[serde(default = "default_base_url")]
    pub base_url: String,

    /// Optional authentication token
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Query-specific call configurations
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,
}

impl Default for HttpReactionConfig {
    fn default() -> Self {
        Self {
            base_url: default_base_url(),
            token: None,
            timeout_ms: default_timeout_ms(),
            routes: HashMap::new(),
        }
    }
}
