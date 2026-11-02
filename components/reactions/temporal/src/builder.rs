// Copyright 2026 The Drasi Authors.
// SPDX-License-Identifier: Apache-2.0

use crate::{TemporalReaction, TemporalReactionConfig};

/// Builds and validates a reaction without connecting to Temporal.
pub struct TemporalReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: TemporalReactionConfig,
    auto_start: bool,
}

impl TemporalReactionBuilder {
    /// Creates a builder with no queries, default configuration, and auto-start enabled.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: TemporalReactionConfig::default(),
            auto_start: true,
        }
    }

    /// Replaces the entire query subscription list.
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Appends one query subscription. Duplicate IDs are rejected by [`Self::build`].
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.queries.push(query.into());
        self
    }

    /// Replaces all connection, action, and queue settings, including earlier capacity overrides.
    pub fn with_config(mut self, config: TemporalReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Controls whether the host starts this reaction automatically; defaults to `true`.
    pub fn with_auto_start(mut self, enabled: bool) -> Self {
        self.auto_start = enabled;
        self
    }

    /// Overrides the priority queue capacity in the current configuration.
    ///
    /// A later [`Self::with_config`] replaces this value.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.config.priority_queue_capacity = capacity;
        self
    }

    /// Validates the configuration and creates a stopped reaction without network access.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty reaction ID, an empty subscription list, blank or
    /// duplicate query IDs, or invalid connection settings, action mappings, or templates.
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
