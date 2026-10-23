// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

//! Explicit Temporal actions for query additions, updates, and removals.
//!
//! ```no_run
//! use drasi_reaction_temporal::{TemporalReaction, TemporalReactionConfig};
//! # fn main() -> anyhow::Result<()> {
//! let config: TemporalReactionConfig = serde_json::from_value(serde_json::json!({
//!     "outputTemplates": { "routes": { "alerts": {
//!         "added": { "action": "signal", "workflowId": "monitor",
//!                    "signalName": "alert", "template": "{{json after}}" }
//!     }}}
//! }))?;
//! let reaction = TemporalReaction::builder("temporal")
//!     .with_query("alerts").with_config(config).build()?;
//! # Ok(()) }
//! ```
//!
//! The host supplies persistent query output and a durable reaction state store.
//! Nothing starts a workflow unless an operation explicitly selects `start`.

#![allow(unexpected_cfgs)]

mod client;
pub mod config;
pub mod descriptor;
mod reaction;
mod render;

pub use config::{
    Action, ActionSpec, ClientIdentity, ConflictPolicy, OutputTemplates, QueryActions, ReusePolicy,
    TemporalReactionConfig, TlsConfig, UpdateWait,
};
pub use reaction::TemporalReaction;

/// Builds and validates a reaction without connecting to Temporal.
pub struct TemporalReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: TemporalReactionConfig,
    auto_start: bool,
}

impl TemporalReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: TemporalReactionConfig::default(),
            auto_start: true,
        }
    }

    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.queries.push(query.into());
        self
    }
    pub fn from_query(self, query: impl Into<String>) -> Self {
        self.with_query(query)
    }
    pub fn with_config(mut self, config: TemporalReactionConfig) -> Self {
        self.config = config;
        self
    }
    pub fn with_auto_start(mut self, enabled: bool) -> Self {
        self.auto_start = enabled;
        self
    }
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.config.priority_queue_capacity = capacity;
        self
    }
    pub fn build(self) -> anyhow::Result<TemporalReaction> {
        anyhow::ensure!(!self.id.trim().is_empty(), "reaction id must not be empty");
        anyhow::ensure!(!self.queries.is_empty(), "at least one query is required");
        let mut seen = std::collections::HashSet::new();
        for query in &self.queries {
            anyhow::ensure!(
                !query.trim().is_empty() && seen.insert(query),
                "query IDs must be nonempty and unique"
            );
        }
        self.config.validate(&self.queries)?;
        Ok(TemporalReaction::new(
            self.id,
            self.queries,
            self.config,
            self.auto_start,
        ))
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "temporal-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::TemporalReactionDescriptor],
    bootstrap_descriptors = [],
);

#[cfg(test)]
mod recovery_tests;
#[cfg(test)]
mod tests;
