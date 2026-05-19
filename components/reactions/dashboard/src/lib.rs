#![allow(unexpected_cfgs)]
// Copyright 2026 The Drasi Authors.
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

//! Dashboard reaction plugin for Drasi.

pub mod api;
pub mod config;
pub mod dashboard;
pub mod descriptor;
pub mod storage;
pub mod websocket;

pub use config::DashboardReactionConfig;
pub use dashboard::DashboardReaction;
pub use storage::{DashboardConfig, DashboardWidget, GridOptions, WidgetGrid};

/// Builder for dashboard reactions.
pub struct DashboardReactionBuilder {
    id: String,
    queries: Vec<String>,
    host: String,
    port: u16,
    heartbeat_interval_ms: u64,
    results_api_url: Option<String>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    predefined_dashboards: Vec<DashboardConfig>,
}

impl DashboardReactionBuilder {
    /// Create a new dashboard builder with defaults.
    pub fn new(id: impl Into<String>) -> Self {
        let config = DashboardReactionConfig::default();
        Self {
            id: id.into(),
            queries: Vec::new(),
            host: config.host,
            port: config.port,
            heartbeat_interval_ms: config.heartbeat_interval_ms,
            results_api_url: None,
            priority_queue_capacity: None,
            auto_start: true,
            predefined_dashboards: Vec::new(),
        }
    }

    /// Replace subscribed query IDs.
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Add one subscribed query ID.
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Set bind host.
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Set bind port.
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set websocket heartbeat interval.
    pub fn with_heartbeat_interval_ms(mut self, heartbeat_interval_ms: u64) -> Self {
        self.heartbeat_interval_ms = heartbeat_interval_ms;
        self
    }

    /// Set priority queue capacity.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Configure auto-start behavior.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the results API base URL for proxying initial query data.
    pub fn with_results_api_url(mut self, url: impl Into<String>) -> Self {
        self.results_api_url = Some(url.into());
        self
    }

    /// Add a predefined dashboard that will be seeded on startup.
    /// Predefined dashboards are only seeded if they don't already exist in the state store,
    /// so user edits made via the UI are preserved across restarts.
    pub fn with_dashboard(mut self, dashboard: DashboardConfig) -> Self {
        self.predefined_dashboards.push(dashboard);
        self
    }

    /// Set full config.
    pub fn with_config(mut self, config: DashboardReactionConfig) -> Self {
        self.host = config.host;
        self.port = config.port;
        self.heartbeat_interval_ms = config.heartbeat_interval_ms;
        self.results_api_url = config.results_api_url;
        self
    }

    /// Build the dashboard reaction.
    pub fn build(self) -> anyhow::Result<DashboardReaction> {
        let config = DashboardReactionConfig {
            host: self.host,
            port: self.port,
            heartbeat_interval_ms: self.heartbeat_interval_ms,
            results_api_url: self.results_api_url,
        };

        Ok(DashboardReaction::from_builder(
            self.id,
            self.queries,
            config,
            self.priority_queue_capacity,
            self.auto_start,
            self.predefined_dashboards,
        ))
    }
}

#[cfg(test)]
mod tests;

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "dashboard-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::DashboardReactionDescriptor],
    bootstrap_descriptors = [],
);
