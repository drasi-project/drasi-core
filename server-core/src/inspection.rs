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

use std::sync::Arc;

use crate::api::DrasiError;
use crate::config::{DrasiServerCoreConfig, DrasiServerCoreSettings, RuntimeConfig};
use crate::queries::QueryManager;
use crate::reactions::ReactionManager;
use crate::sources::SourceManager;
use crate::state_guard::StateGuard;

/// Inspection API for querying server state and component information
///
/// This module provides all inspection/listing methods for sources, queries, and reactions,
/// separated from the main server core lifecycle management.
#[derive(Clone)]
pub struct InspectionAPI {
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
    state_guard: StateGuard,
    config: Arc<RuntimeConfig>,
}

impl InspectionAPI {
    pub(crate) fn new(
        source_manager: Arc<SourceManager>,
        query_manager: Arc<QueryManager>,
        reaction_manager: Arc<ReactionManager>,
        state_guard: StateGuard,
        config: Arc<RuntimeConfig>,
    ) -> Self {
        Self {
            source_manager,
            query_manager,
            reaction_manager,
            state_guard,
            config,
        }
    }

    // ============================================================================
    // Source Inspection Methods
    // ============================================================================

    /// List all sources with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let sources = core.list_sources().await?;
    /// for (id, status) in sources {
    ///     println!("Source {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_sources(
        &self,
    ) -> crate::api::Result<Vec<(String, crate::channels::ComponentStatus)>> {
        self.state_guard.require_initialized().await?;
        Ok(self.source_manager.list_sources().await)
    }

    /// Get detailed information about a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let source_info = core.get_source_info("my-source").await?;
    /// println!("Source type: {}", source_info.source_type);
    /// println!("Status: {:?}", source_info.status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_info(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::SourceRuntime> {
        self.state_guard.require_initialized().await?;
        self.source_manager
            .get_source(id.to_string())
            .await
            .map_err(|_e| DrasiError::component_not_found("source", id))
    }

    /// Get the current status of a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_source_status("my-source").await?;
    /// println!("Source status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_status(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::channels::ComponentStatus> {
        self.state_guard.require_initialized().await?;
        self.source_manager
            .get_source_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("source", id))
    }

    /// Get the full configuration for a specific source
    ///
    /// This returns the complete source configuration including auto_start and bootstrap_provider,
    /// unlike `get_source_info()` which only returns runtime information.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_source_config("my-source").await?;
    /// println!("Auto-start: {}", config.auto_start);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_config(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::SourceConfig> {
        self.state_guard.require_initialized().await?;
        self.source_manager
            .get_source_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("source", id))
    }

    // ============================================================================
    // Query Inspection Methods
    // ============================================================================

    /// List all queries with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let queries = core.list_queries().await?;
    /// for (id, status) in queries {
    ///     println!("Query {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_queries(
        &self,
    ) -> crate::api::Result<Vec<(String, crate::channels::ComponentStatus)>> {
        self.state_guard.require_initialized().await?;
        Ok(self.query_manager.list_queries().await)
    }

    /// Get detailed information about a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let query_info = core.get_query_info("my-query").await?;
    /// println!("Query: {}", query_info.query);
    /// println!("Status: {:?}", query_info.status);
    /// println!("Sources: {:?}", query_info.sources);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_info(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::QueryRuntime> {
        self.state_guard.require_initialized().await?;
        self.query_manager
            .get_query(id.to_string())
            .await
            .map_err(|_e| DrasiError::component_not_found("query", id))
    }

    /// Get the current status of a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_query_status("my-query").await?;
    /// println!("Query status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_status(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::channels::ComponentStatus> {
        self.state_guard.require_initialized().await?;
        self.query_manager
            .get_query_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("query", id))
    }

    /// Get the current result set for a running query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let results = core.get_query_results("my-query").await?;
    /// println!("Current results: {} items", results.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_results(&self, id: &str) -> crate::api::Result<Vec<serde_json::Value>> {
        self.state_guard.require_initialized().await?;
        self.query_manager.get_query_results(id).await.map_err(|e| {
            if e.to_string().contains("not found") {
                DrasiError::component_not_found("query", id)
            } else if e.to_string().contains("not running") {
                DrasiError::invalid_state(format!("Query '{}' is not running", id))
            } else {
                DrasiError::provisioning(e.to_string())
            }
        })
    }

    /// Get the full configuration for a specific query
    ///
    /// This returns the complete query configuration including all fields like auto_start and joins,
    /// unlike `get_query_info()` which only returns runtime information.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_query_config("my-query").await?;
    /// println!("Auto-start: {}", config.auto_start);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_config(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::QueryConfig> {
        self.state_guard.require_initialized().await?;
        self.query_manager
            .get_query_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("query", id))
    }

    // ============================================================================
    // Reaction Inspection Methods
    // ============================================================================

    /// List all reactions with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let reactions = core.list_reactions().await?;
    /// for (id, status) in reactions {
    ///     println!("Reaction {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_reactions(
        &self,
    ) -> crate::api::Result<Vec<(String, crate::channels::ComponentStatus)>> {
        self.state_guard.require_initialized().await?;
        Ok(self.reaction_manager.list_reactions().await)
    }

    /// Get detailed information about a specific reaction
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let reaction_info = core.get_reaction_info("my-reaction").await?;
    /// println!("Reaction type: {}", reaction_info.reaction_type);
    /// println!("Status: {:?}", reaction_info.status);
    /// println!("Queries: {:?}", reaction_info.queries);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_info(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::ReactionRuntime> {
        self.state_guard.require_initialized().await?;
        self.reaction_manager
            .get_reaction(id.to_string())
            .await
            .map_err(|_e| DrasiError::component_not_found("reaction", id))
    }

    /// Get the current status of a specific reaction
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_reaction_status("my-reaction").await?;
    /// println!("Reaction status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_status(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::channels::ComponentStatus> {
        self.state_guard.require_initialized().await?;
        self.reaction_manager
            .get_reaction_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("reaction", id))
    }

    /// Get the full configuration for a specific reaction
    ///
    /// This returns the complete reaction configuration including all fields,
    /// unlike `get_reaction_info()` which only returns runtime information.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_reaction_config("my-reaction").await?;
    /// println!("Auto-start: {}", config.auto_start);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_config(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::ReactionConfig> {
        self.state_guard.require_initialized().await?;
        self.reaction_manager
            .get_reaction_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("reaction", id))
    }

    // ============================================================================
    // Full Configuration Snapshot
    // ============================================================================

    /// Get a complete configuration snapshot of all components
    ///
    /// Returns the full server configuration including all sources, queries, and reactions
    /// with their complete configurations. This is useful for persistence, backups, or introspection.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_current_config().await?;
    /// println!("Server has {} sources, {} queries, {} reactions",
    ///          config.sources.len(), config.queries.len(), config.reactions.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_current_config(&self) -> crate::api::Result<DrasiServerCoreConfig> {
        self.state_guard.require_initialized().await?;

        // Collect all source configs
        let source_ids: Vec<String> = self
            .source_manager
            .list_sources()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        let mut sources = Vec::new();
        for id in source_ids {
            if let Some(config) = self.source_manager.get_source_config(&id).await {
                sources.push(config);
            }
        }

        // Collect all query configs
        let query_ids: Vec<String> = self
            .query_manager
            .list_queries()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        let mut queries = Vec::new();
        for id in query_ids {
            if let Some(config) = self.query_manager.get_query_config(&id).await {
                queries.push(config);
            }
        }

        // Collect all reaction configs
        let reaction_ids: Vec<String> = self
            .reaction_manager
            .list_reactions()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        let mut reactions = Vec::new();
        for id in reaction_ids {
            if let Some(config) = self.reaction_manager.get_reaction_config(&id).await {
                reactions.push(config);
            }
        }

        Ok(DrasiServerCoreConfig {
            server_core: DrasiServerCoreSettings {
                id: self.config.server_core.id.clone(),
                priority_queue_capacity: self.config.server_core.priority_queue_capacity,
                dispatch_buffer_capacity: self.config.server_core.dispatch_buffer_capacity,
            },
            storage_backends: vec![],
            sources,
            queries,
            reactions,
        })
    }
}
