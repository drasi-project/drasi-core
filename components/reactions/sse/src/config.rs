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

//! Configuration types for SSE (Server-Sent Events) reactions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_sse_path() -> String {
    "/events".to_string()
}

fn default_heartbeat_interval_ms() -> u64 {
    30000
}

fn default_sse_port() -> u16 {
    8080
}

fn default_sse_host() -> String {
    "0.0.0.0".to_string()
}

/// Specification for SSE output template and path.
///
/// This type is used to configure SSE templates for different operation types (added, updated, deleted).
/// All template fields support Handlebars template syntax for dynamic content generation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TemplateSpec {
    /// Optional custom path for this specific template.
    /// If provided, events will be sent to this path. Absolute paths (starting with '/')
    /// are used as-is, while relative paths are appended to the base SSE path.
    /// For example, if the base sse_path is "/events" and path is "sensors",
    /// events will be sent to "/events/sensors".
    /// Supports Handlebars templates for dynamic paths.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Event data template as a Handlebars template.
    /// If empty, sends the default JSON format with queryId, results, and timestamp.
    #[serde(default)]
    pub template: String,
}

/// Configuration for query-specific SSE output.
///
/// Defines different template specifications for each operation type (added, updated, deleted).
/// Each operation type can have its own template and optionally its own endpoint.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct QueryConfig {
    /// Template specification for ADD operations (new rows in query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added: Option<TemplateSpec>,

    /// Template specification for UPDATE operations (modified rows in query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated: Option<TemplateSpec>,

    /// Template specification for DELETE operations (removed rows from query results).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<TemplateSpec>,
}

/// SSE (Server-Sent Events) reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SseReactionConfig {
    /// Host to bind SSE server
    #[serde(default = "default_sse_host")]
    pub host: String,

    /// Port to bind SSE server
    #[serde(default = "default_sse_port")]
    pub port: u16,

    /// SSE path
    #[serde(default = "default_sse_path")]
    pub sse_path: String,

    /// Heartbeat interval in milliseconds
    #[serde(default = "default_heartbeat_interval_ms")]
    pub heartbeat_interval_ms: u64,

    /// Query-specific template configurations
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,

    /// Default template configuration used when no query-specific route is defined.
    /// If not set, falls back to the built-in default format.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
}

impl Default for SseReactionConfig {
    fn default() -> Self {
        Self {
            host: default_sse_host(),
            port: default_sse_port(),
            sse_path: default_sse_path(),
            heartbeat_interval_ms: default_heartbeat_interval_ms(),
            routes: HashMap::new(),
            default_template: None,
        }
    }
}
