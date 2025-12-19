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
    default_template: Option<QueryConfig>,
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
            default_template: None,
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

    /// Set the default template configuration used when no query-specific route is defined
    pub fn with_default_template(mut self, config: QueryConfig) -> Self {
        self.default_template = Some(config);
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: SseReactionConfig) -> Self {
        self.host = config.host;
        self.port = config.port;
        self.sse_path = config.sse_path;
        self.heartbeat_interval_ms = config.heartbeat_interval_ms;
        self.routes = config.routes;
        self.default_template = config.default_template;
        self
    }

    /// Validate a template by attempting to compile it with Handlebars
    fn validate_template(
        handlebars: &handlebars::Handlebars,
        template: &str,
    ) -> anyhow::Result<()> {
        if template.is_empty() {
            return Ok(());
        }
        // Validate the template by attempting to render it with empty data
        handlebars
            .render_template(template, &serde_json::json!({}))
            .map_err(|e| anyhow::anyhow!("Invalid template: {}", e))?;
        Ok(())
    }

    /// Validate all templates in a QueryConfig
    fn validate_query_config(
        handlebars: &handlebars::Handlebars,
        config: &QueryConfig,
    ) -> anyhow::Result<()> {
        if let Some(added) = &config.added {
            Self::validate_template(handlebars, &added.template)?;
        }
        if let Some(updated) = &config.updated {
            Self::validate_template(handlebars, &updated.template)?;
        }
        if let Some(deleted) = &config.deleted {
            Self::validate_template(handlebars, &deleted.template)?;
        }
        Ok(())
    }

    /// Build the SSE reaction
    pub fn build(self) -> anyhow::Result<SseReaction> {
        // Create a single Handlebars instance for all validation with json helper
        let mut handlebars = handlebars::Handlebars::new();

        // Register the json helper for template validation
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

        // Validate all templates in routes
        for (query_id, config) in &self.routes {
            Self::validate_query_config(&handlebars, config)
                .map_err(|e| anyhow::anyhow!("Invalid template in route '{}': {}", query_id, e))?;
        }

        // Validate default template if provided
        if let Some(default_template) = &self.default_template {
            Self::validate_query_config(&handlebars, default_template)
                .map_err(|e| anyhow::anyhow!("Invalid default template: {}", e))?;
        }

        // Validate that all routes correspond to subscribed queries
        if !self.routes.is_empty() && !self.queries.is_empty() {
            for route_query in self.routes.keys() {
                // Check exact match or if the query ends with the route (for dotted notation)
                let matches = self
                    .queries
                    .iter()
                    .any(|q| q == route_query || q.ends_with(&format!(".{}", route_query)));
                if !matches {
                    return Err(anyhow::anyhow!(
                        "Route '{}' does not match any subscribed query. Subscribed queries: {:?}",
                        route_query,
                        self.queries
                    ));
                }
            }
        }

        let config = SseReactionConfig {
            host: self.host,
            port: self.port,
            sse_path: self.sse_path,
            heartbeat_interval_ms: self.heartbeat_interval_ms,
            routes: self.routes,
            default_template: self.default_template,
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
