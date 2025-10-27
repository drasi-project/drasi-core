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

//! Builder for DrasiServerCore

use crate::api::Result;
use crate::config::{
    DrasiServerCoreConfig, DrasiServerCoreSettings, QueryConfig, ReactionConfig, RuntimeConfig,
    SourceConfig,
};
use crate::server_core::DrasiServerCore;
use std::sync::Arc;

/// Fluent builder for DrasiServerCore
#[derive(Debug, Clone)]
pub struct DrasiServerCoreBuilder {
    server_id: Option<String>,
    priority_queue_capacity: Option<usize>,
    dispatch_buffer_capacity: Option<usize>,
    sources: Vec<SourceConfig>,
    queries: Vec<QueryConfig>,
    reactions: Vec<ReactionConfig>,
}

impl DrasiServerCoreBuilder {
    /// Create a new builder
    pub fn new() -> Self {
        Self {
            server_id: None,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            sources: Vec::new(),
            queries: Vec::new(),
            reactions: Vec::new(),
        }
    }

    /// Set the server ID (default: auto-generated UUID)
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.server_id = Some(id.into());
        self
    }

    /// Set global default priority queue capacity for queries and reactions
    ///
    /// This sets the default capacity for priority queues used by queries and reactions.
    /// Individual components can override this value in their configuration.
    ///
    /// Default: 10000 if not specified
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set global default dispatch buffer capacity for sources and queries
    ///
    /// This sets the default capacity for dispatch channels (both broadcast and mpsc)
    /// used by sources and queries. Individual components can override this value.
    ///
    /// Default: 1000 if not specified
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Add a source configuration
    pub fn add_source(mut self, source: SourceConfig) -> Self {
        self.sources.push(source);
        self
    }

    /// Add multiple source configurations
    pub fn add_sources(mut self, sources: Vec<SourceConfig>) -> Self {
        self.sources.extend(sources);
        self
    }

    /// Add a query configuration
    pub fn add_query(mut self, query: QueryConfig) -> Self {
        self.queries.push(query);
        self
    }

    /// Add multiple query configurations
    pub fn add_queries(mut self, queries: Vec<QueryConfig>) -> Self {
        self.queries.extend(queries);
        self
    }

    /// Add a reaction configuration
    pub fn add_reaction(mut self, reaction: ReactionConfig) -> Self {
        self.reactions.push(reaction);
        self
    }

    /// Add multiple reaction configurations
    pub fn add_reactions(mut self, reactions: Vec<ReactionConfig>) -> Self {
        self.reactions.extend(reactions);
        self
    }

    /// Build and initialize the DrasiServerCore instance
    ///
    /// This performs all initialization and returns a ready-to-start server.
    pub async fn build(self) -> Result<DrasiServerCore> {
        let server_settings = DrasiServerCoreSettings {
            id: self.server_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            priority_queue_capacity: self.priority_queue_capacity,
            dispatch_buffer_capacity: self.dispatch_buffer_capacity,
        };

        // Create DrasiServerCoreConfig first, then convert to RuntimeConfig
        // This ensures the From implementation applies the priority queue hierarchy
        let core_config = DrasiServerCoreConfig {
            server_core: server_settings,
            sources: self.sources,
            queries: self.queries,
            reactions: self.reactions,
        };

        let config = Arc::new(RuntimeConfig::from(core_config));

        // Create and initialize the server
        let mut core = DrasiServerCore::new(config);
        core.initialize().await?;

        Ok(core)
    }
}

impl Default for DrasiServerCoreBuilder {
    fn default() -> Self {
        Self::new()
    }
}
