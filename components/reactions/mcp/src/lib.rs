#![allow(unexpected_cfgs)]
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

//! MCP reaction plugin for Drasi.
//!
//! This plugin implements an MCP (Model Context Protocol) server with HTTP + SSE
//! transport. MCP clients can subscribe to query resources and receive real-time
//! notifications as query results change.

use std::collections::HashMap;

pub mod config;
pub mod descriptor;
pub mod mcp;

pub use config::{McpReactionConfig, NotificationTemplate, QueryConfig};
pub use mcp::McpReaction;

/// Helper function to register the json helper in a Handlebars instance.
fn register_json_helper(handlebars: &mut handlebars::Handlebars) {
    handlebars.register_helper(
        "json",
        Box::new(
            |h: &handlebars::Helper,
             _: &handlebars::Handlebars,
             _: &handlebars::Context,
             _: &mut handlebars::RenderContext,
             out: &mut dyn handlebars::Output|
             -> handlebars::HelperResult {
                if let Some(value) = h.param(0) {
                    match serde_json::to_string(&value.value()) {
                        Ok(json_str) => out.write(&json_str)?,
                        Err(_) => out.write("null")?,
                    }
                } else {
                    out.write("null")?;
                }
                Ok(())
            },
        ),
    );
}

/// Builder for MCP reaction.
pub struct McpReactionBuilder {
    id: String,
    queries: Vec<String>,
    host: String,
    port: u16,
    bearer_token: Option<String>,
    routes: HashMap<String, QueryConfig>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    max_sessions: usize,
    session_channel_capacity: usize,
}

impl McpReactionBuilder {
    /// Create a new MCP reaction builder with the given ID.
    pub fn new(id: impl Into<String>) -> Self {
        let defaults = McpReactionConfig::default();
        Self {
            id: id.into(),
            queries: Vec::new(),
            host: defaults.host,
            port: defaults.port,
            bearer_token: None,
            routes: HashMap::new(),
            priority_queue_capacity: None,
            auto_start: true,
            max_sessions: defaults.max_sessions,
            session_channel_capacity: defaults.session_channel_capacity,
        }
    }

    /// Set the query IDs to subscribe to.
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Add a query ID to subscribe to.
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Set the bind address for the MCP server.
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Set the port to bind MCP server.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set the optional bearer token for authentication.
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Self {
        self.bearer_token = Some(token.into());
        self
    }

    /// Add a route configuration for a specific query.
    pub fn with_route(mut self, query_id: impl Into<String>, config: QueryConfig) -> Self {
        self.routes.insert(query_id.into(), config);
        self
    }

    /// Set the priority queue capacity.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether the reaction should auto-start.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the maximum number of concurrent sessions.
    pub fn with_max_sessions(mut self, max: usize) -> Self {
        self.max_sessions = max;
        self
    }

    /// Set the per-session notification channel capacity.
    pub fn with_session_channel_capacity(mut self, capacity: usize) -> Self {
        self.session_channel_capacity = capacity;
        self
    }

    /// Set the full configuration at once.
    pub fn with_config(mut self, config: McpReactionConfig) -> Self {
        self.host = config.host;
        self.port = config.port;
        self.bearer_token = config.bearer_token;
        self.max_sessions = config.max_sessions;
        self.session_channel_capacity = config.session_channel_capacity;
        self.routes = config.routes;
        self
    }

    /// Build the MCP reaction.
    pub fn build(self) -> anyhow::Result<McpReaction> {
        Ok(McpReaction::from_builder(
            self.id,
            self.queries,
            McpReactionConfig {
                host: self.host,
                port: self.port,
                bearer_token: self.bearer_token,
                max_sessions: self.max_sessions,
                session_channel_capacity: self.session_channel_capacity,
                routes: self.routes,
            },
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
    fn test_builder_sets_config() {
        let config = McpReactionConfig {
            host: "127.0.0.1".to_string(),
            port: 4242,
            bearer_token: Some("token".to_string()),
            max_sessions: 50,
            session_channel_capacity: 512,
            routes: HashMap::new(),
        };

        let reaction = McpReaction::builder("test-reaction")
            .with_queries(vec!["query1".to_string()])
            .with_config(config)
            .build()
            .unwrap();

        assert_eq!(reaction.query_ids(), vec!["query1".to_string()]);
        assert_eq!(reaction.config().port, 4242);
        assert_eq!(reaction.config().host, "127.0.0.1");
        assert_eq!(reaction.config().bearer_token.as_deref(), Some("token"));
        assert_eq!(reaction.config().max_sessions, 50);
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "mcp-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::McpReactionDescriptor],
    bootstrap_descriptors = [],
);
