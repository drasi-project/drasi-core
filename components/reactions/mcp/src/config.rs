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

//! Configuration types for MCP reactions.

use serde::Deserialize;
use std::collections::HashMap;

fn default_port() -> u16 {
    3000
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

const DEFAULT_MAX_SESSIONS: usize = 100;
const DEFAULT_SESSION_CHANNEL_CAPACITY: usize = 1024;

fn default_max_sessions() -> usize {
    DEFAULT_MAX_SESSIONS
}

fn default_session_channel_capacity() -> usize {
    DEFAULT_SESSION_CHANNEL_CAPACITY
}

/// Template definition for MCP notifications.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct NotificationTemplate {
    /// Handlebars template string.
    pub template: String,
}

/// Per-query MCP configuration.
#[derive(Debug, Clone, Deserialize, PartialEq, Default)]
pub struct QueryConfig {
    /// Human-readable title for the query resource.
    pub title: Option<String>,

    /// Description of what the resource represents.
    pub description: Option<String>,

    /// Template for ADD operations.
    pub added: Option<NotificationTemplate>,

    /// Template for UPDATE operations.
    pub updated: Option<NotificationTemplate>,

    /// Template for DELETE operations.
    pub deleted: Option<NotificationTemplate>,
}

/// MCP reaction configuration.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct McpReactionConfig {
    /// HTTP server bind address.
    #[serde(default = "default_host")]
    pub host: String,

    /// HTTP server port.
    #[serde(default = "default_port")]
    pub port: u16,

    /// Optional bearer token for authentication.
    pub bearer_token: Option<String>,

    /// Maximum number of concurrent sessions.
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,

    /// Per-session notification channel capacity.
    #[serde(default = "default_session_channel_capacity")]
    pub session_channel_capacity: usize,

    /// Query-specific template configurations.
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,
}

impl Default for McpReactionConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            bearer_token: None,
            max_sessions: default_max_sessions(),
            session_channel_capacity: default_session_channel_capacity(),
            routes: HashMap::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = McpReactionConfig::default();
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 3000);
        assert!(config.bearer_token.is_none());
        assert_eq!(config.max_sessions, 100);
        assert_eq!(config.session_channel_capacity, 1024);
    }

    #[test]
    fn test_deserialize_config() {
        let raw = serde_json::json!({
            "port": 4000,
            "bearer_token": "secret",
            "routes": {
                "query1": {
                    "title": "Query 1",
                    "added": { "template": "{\"type\":\"added\"}" }
                }
            }
        });
        let config: McpReactionConfig = serde_json::from_value(raw).expect("valid json");
        assert_eq!(config.port, 4000);
        assert_eq!(config.bearer_token.as_deref(), Some("secret"));
        assert!(config.routes.contains_key("query1"));
    }
}
