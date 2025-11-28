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

//! Query management operations for DrasiLib
//!
//! This module provides all query-related operations including creating, removing,
//! starting, and stopping queries.

use anyhow::Result as AnyhowResult;

use crate::channels::ComponentStatus;
use crate::component_ops::map_state_error;
use crate::config::{QueryConfig, QueryRuntime};
use crate::error::{DrasiError, Result};
use crate::lib_core::DrasiLib;

impl DrasiLib {
    /// Create a query in a running server
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::{DrasiLib, Query};
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.create_query(
    ///     Query::cypher("new-query")
    ///         .query("MATCH (n) RETURN n")
    ///         .from_source("source1")
    ///         .auto_start(true)
    ///         .build()
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn create_query(&self, query: QueryConfig) -> Result<()> {
        self.state_guard.require_initialized().await?;

        self.create_query_with_options(query, true)
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to add query: {}", e)))?;

        Ok(())
    }

    /// Remove a query from a running server
    ///
    /// If the query is running, it will be stopped first before removal.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_query("old-query").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_query(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized().await?;

        // Stop if running
        let status = self
            .query_manager
            .get_query_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("query", id))?;

        if matches!(status, ComponentStatus::Running) {
            self.query_manager
                .stop_query(id.to_string())
                .await
                .map_err(|e| DrasiError::provisioning(format!("Failed to stop query: {}", e)))?;
        }

        // Delete the query
        self.query_manager
            .delete_query(id.to_string())
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to delete query: {}", e)))?;

        Ok(())
    }

    /// Start a stopped query
    ///
    /// This will create the necessary subscriptions to source data streams.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.start_query("my-query").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_query(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized().await?;

        // Verify query exists
        let _config = self
            .query_manager
            .get_query_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("query", id))?;

        // Query will subscribe directly to sources when started
        map_state_error(
            self.query_manager.start_query(id.to_string()).await,
            "query",
            id,
        )
    }

    /// Stop a running query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_query("my-query").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_query(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized().await?;

        // Stop the query (it unsubscribes from sources automatically)
        map_state_error(
            self.query_manager.stop_query(id.to_string()).await,
            "query",
            id,
        )?;

        Ok(())
    }

    /// List all queries with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let queries = core.list_queries().await?;
    /// for (id, status) in queries {
    ///     println!("Query {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_queries(&self) -> Result<Vec<(String, ComponentStatus)>> {
        self.inspection.list_queries().await
    }

    /// Get detailed information about a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let query_info = core.get_query_info("my-query").await?;
    /// println!("Query: {}", query_info.query);
    /// println!("Status: {:?}", query_info.status);
    /// println!("Source subscriptions: {:?}", query_info.source_subscriptions);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_info(&self, id: &str) -> Result<QueryRuntime> {
        self.inspection.get_query_info(id).await
    }

    /// Get the current status of a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_query_status("my-query").await?;
    /// println!("Query status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_status(&self, id: &str) -> Result<ComponentStatus> {
        self.inspection.get_query_status(id).await
    }

    /// Get the current result set for a running query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let results = core.get_query_results("my-query").await?;
    /// println!("Current results: {} items", results.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_results(&self, id: &str) -> Result<Vec<serde_json::Value>> {
        self.inspection.get_query_results(id).await
    }

    /// Get the full configuration for a specific query
    ///
    /// This returns the complete query configuration including all fields like auto_start and joins,
    /// unlike `get_query_info()` which only returns runtime information.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_query_config("my-query").await?;
    /// println!("Auto-start: {}", config.auto_start);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_config(&self, id: &str) -> Result<QueryConfig> {
        self.inspection.get_query_config(id).await
    }

    /// Internal helper for creating queries with auto-start control
    pub(crate) async fn create_query_with_options(
        &self,
        config: QueryConfig,
        allow_auto_start: bool,
    ) -> AnyhowResult<()> {
        let query_id = config.id.clone();
        let should_auto_start = config.auto_start;

        // Add the query (without saving during initialization)
        self.query_manager.add_query_without_save(config).await?;

        // Start if auto-start is enabled and allowed
        if should_auto_start && allow_auto_start {
            self.query_manager.start_query(query_id).await?;
        }

        Ok(())
    }
}
