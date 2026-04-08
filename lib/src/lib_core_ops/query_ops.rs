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
use futures::stream::Stream;
use std::collections::HashMap;

use crate::channels::{ComponentEvent, ComponentStatus};
use crate::component_ops::map_component_error;
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
    /// core.add_query(
    ///     Query::cypher("new-query")
    ///         .query("MATCH (n) RETURN n")
    ///         .from_source("source1")
    ///         .auto_start(true)
    ///         .build()
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn add_query(&self, query: QueryConfig) -> Result<()> {
        self.state_guard.require_initialized()?;

        let query_id = query.id.clone();
        self.add_query_with_options(query, true)
            .await
            .map_err(|e| DrasiError::operation_failed("query", &query_id, "add", format!("{e}")))?;

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
        self.state_guard.require_initialized()?;

        // Step 1: Validate no dependents
        {
            let graph = self.component_graph.read().await;
            if let Err(dependent_ids) = graph.can_remove(id) {
                return Err(DrasiError::operation_failed(
                    "query",
                    id,
                    "remove",
                    format!("Depended on by: {}", dependent_ids.join(", ")),
                ));
            }
        }

        // Step 2: Teardown runtime (stop, remove from runtime map)
        self.query_manager
            .teardown_query(id.to_string())
            .await
            .map_err(|e| {
                DrasiError::operation_failed("query", id, "remove", format!("Teardown failed: {e}"))
            })?;

        // Step 3: Deregister from graph (remove node + edges, emit events)
        // If this fails after teardown, the runtime is already gone. Rather than
        // returning an error (which would leave an orphaned graph node with no
        // runtime backing), set the component to Error state and log the failure.
        {
            let mut graph = self.component_graph.write().await;
            if let Err(e) = graph.deregister(id) {
                log::error!(
                    "Query '{id}' runtime was torn down but graph deregister failed: {e}. \
                     Setting component to Error state."
                );
                let _ = graph.validate_and_transition(
                    id,
                    ComponentStatus::Error,
                    Some(format!("Orphaned: deregister failed after teardown: {e}")),
                );
            }
        }

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
        self.state_guard.require_initialized()?;

        // Verify query exists
        let _config = self
            .query_manager
            .get_query_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("query", id))?;

        // Query will subscribe directly to sources when started
        map_component_error(
            self.query_manager.start_query(id.to_string()).await,
            "query",
            id,
            "start",
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
        self.state_guard.require_initialized()?;

        // Stop the query (it unsubscribes from sources automatically)
        map_component_error(
            self.query_manager.stop_query(id.to_string()).await,
            "query",
            id,
            "stop",
        )?;

        Ok(())
    }

    /// Update a query by replacing it with a new configuration.
    ///
    /// Uses the `Reconfiguring` state transition to preserve the graph node, edges,
    /// and event history. The old query is stopped, the runtime is swapped, and the
    /// query is restarted if it was running.
    ///
    /// # Errors
    ///
    /// Returns an error if the query doesn't exist, if the new configuration
    /// references non-existent sources, or if provisioning fails.
    pub async fn update_query(&self, id: &str, config: QueryConfig) -> Result<()> {
        self.state_guard.require_initialized()?;

        // Delegate to QueryManager which uses the Reconfiguring transition,
        // preserving the graph node, edges, and event history.
        self.query_manager
            .update_query(id.to_string(), config)
            .await
            .map_err(|e| DrasiError::operation_failed("query", id, "update", e.to_string()))
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

    /// Get lifecycle events for a specific query as an async stream.
    ///
    /// Returns events in chronological order (oldest first). Up to 100 most recent
    /// events are retained per component.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_query_events("my-query").await?;
    /// while let Some(event) = events.next().await {
    ///     println!("Event: {:?} - {:?}", event.status, event.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_events(&self, id: &str) -> Result<impl Stream<Item = ComponentEvent>> {
        self.inspection.get_query_events(id).await
    }

    /// Get all lifecycle events across all queries as an async stream.
    ///
    /// Returns events sorted by timestamp (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_all_query_events().await?;
    /// while let Some(event) = events.next().await {
    ///     println!("{}: {:?}", event.component_id, event.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_all_query_events(&self) -> Result<impl Stream<Item = ComponentEvent>> {
        self.inspection.get_all_query_events().await
    }

    /// Subscribe to live logs for a query.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// The receiver will receive new log messages as they are emitted by the query.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let (history, mut receiver) = core.subscribe_query_logs("my-query").await?;
    ///
    /// // Print historical logs
    /// for log in history {
    ///     println!("[{:?}] {}", log.level, log.message);
    /// }
    ///
    /// // Listen for new logs
    /// while let Ok(log) = receiver.recv().await {
    ///     println!("[{:?}] {}", log.level, log.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn subscribe_query_logs(
        &self,
        id: &str,
    ) -> Result<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        self.inspection.subscribe_query_logs(id).await
    }

    /// Subscribe to live events for a query.
    ///
    /// Returns the event history (oldest first) and a broadcast receiver for new events
    /// as they occur. Events include lifecycle status changes such as Starting, Running,
    /// Error, Stopped.
    pub async fn subscribe_query_events(
        &self,
        id: &str,
    ) -> Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        self.inspection.subscribe_query_events(id).await
    }

    /// Internal helper for creating queries with auto-start control
    pub(crate) async fn add_query_with_options(
        &self,
        config: QueryConfig,
        allow_auto_start: bool,
    ) -> AnyhowResult<()> {
        let query_id = config.id.clone();
        let should_auto_start = config.auto_start;

        // Step 1: Register in the component graph (validates sources exist, creates node + edges)
        {
            let mut graph = self.component_graph.write().await;
            let mut metadata = HashMap::new();
            metadata.insert("query".to_string(), config.query.clone());
            let source_ids: Vec<String> =
                config.sources.iter().map(|s| s.source_id.clone()).collect();
            graph.register_query(&config.id, metadata, &source_ids)?;
        }

        // Step 2: Provision runtime (create DrasiQuery, initialize, store)
        if let Err(e) = self.query_manager.provision_query(config).await {
            // Compensating rollback: remove from graph on runtime failure
            let mut graph = self.component_graph.write().await;
            let _ = graph.deregister(&query_id);
            return Err(e);
        }

        // Step 3: Start if auto-start is enabled and allowed
        if should_auto_start && allow_auto_start {
            self.query_manager.start_query(query_id).await?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::channels::ComponentStatus;
    use crate::error::DrasiError;
    use crate::sources::tests::TestMockSource;
    use crate::{DrasiLib, Query};

    /// Build a DrasiLib with a single mock source, started and ready for queries.
    async fn build_core_with_source() -> DrasiLib {
        let source = TestMockSource::new("test-source".to_string()).unwrap();
        let core = DrasiLib::builder()
            .with_id("test")
            .with_source(source)
            .build()
            .await
            .unwrap();
        core.start().await.unwrap();
        core
    }

    // ========================================================================
    // add_query
    // ========================================================================

    #[tokio::test]
    async fn add_query_happy_path() {
        let core = build_core_with_source().await;

        let config = Query::cypher("q1")
            .query("MATCH (n:Test) RETURN n")
            .from_source("test-source")
            .auto_start(false)
            .build();

        core.add_query(config).await.unwrap();

        // Query should appear in list
        let queries = core.list_queries().await.unwrap();
        assert!(queries.iter().any(|(id, _)| id == "q1"));
    }

    #[tokio::test]
    async fn add_query_missing_source_returns_error() {
        let core = build_core_with_source().await;

        let config = Query::cypher("q-bad")
            .query("MATCH (n) RETURN n")
            .from_source("nonexistent-source")
            .auto_start(false)
            .build();

        let result = core.add_query(config).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, DrasiError::OperationFailed { .. }),
            "expected OperationFailed, got: {err:?}"
        );
    }

    // ========================================================================
    // remove_query
    // ========================================================================

    #[tokio::test]
    async fn remove_query_happy_path() {
        let core = build_core_with_source().await;

        let config = Query::cypher("q-remove")
            .query("MATCH (n:Test) RETURN n")
            .from_source("test-source")
            .auto_start(false)
            .build();
        core.add_query(config).await.unwrap();

        // Remove should succeed
        core.remove_query("q-remove").await.unwrap();

        // Query should no longer appear in list
        let queries = core.list_queries().await.unwrap();
        assert!(!queries.iter().any(|(id, _)| id == "q-remove"));
    }

    #[tokio::test]
    async fn remove_query_nonexistent_returns_error() {
        let core = build_core_with_source().await;

        let result = core.remove_query("does-not-exist").await;
        assert!(result.is_err());
    }

    // ========================================================================
    // list_queries
    // ========================================================================

    #[tokio::test]
    async fn list_queries_empty_then_populated() {
        let core = build_core_with_source().await;

        // Initially no queries
        let queries = core.list_queries().await.unwrap();
        assert!(queries.is_empty(), "expected no queries initially");

        // Add two queries
        let config1 = Query::cypher("q-list-1")
            .query("MATCH (n) RETURN n")
            .from_source("test-source")
            .auto_start(false)
            .build();
        let config2 = Query::cypher("q-list-2")
            .query("MATCH (n) RETURN n")
            .from_source("test-source")
            .auto_start(false)
            .build();
        core.add_query(config1).await.unwrap();
        core.add_query(config2).await.unwrap();

        let queries = core.list_queries().await.unwrap();
        let ids: Vec<&str> = queries.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"q-list-1"));
        assert!(ids.contains(&"q-list-2"));
        assert_eq!(queries.len(), 2);
    }

    // ========================================================================
    // get_query_status
    // ========================================================================

    #[tokio::test]
    async fn get_query_status_returns_correct_status() {
        let core = build_core_with_source().await;

        let config = Query::cypher("q-status")
            .query("MATCH (n:Test) RETURN n")
            .from_source("test-source")
            .auto_start(false)
            .build();
        core.add_query(config).await.unwrap();

        // Query added without auto-start should be Stopped
        let status = core.get_query_status("q-status").await.unwrap();
        assert_eq!(status, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn get_query_status_nonexistent_returns_error() {
        let core = build_core_with_source().await;

        let result = core.get_query_status("ghost-query").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, DrasiError::ComponentNotFound { .. }),
            "expected ComponentNotFound, got: {err:?}"
        );
    }

    // ========================================================================
    // start_query / stop_query lifecycle
    // ========================================================================

    #[tokio::test]
    async fn start_and_stop_query_lifecycle() {
        let core = build_core_with_source().await;

        let config = Query::cypher("q-lifecycle")
            .query("MATCH (n:Test) RETURN n")
            .from_source("test-source")
            .auto_start(false)
            .build();
        core.add_query(config).await.unwrap();

        // Start the query
        core.start_query("q-lifecycle").await.unwrap();

        // Allow async status propagation
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let status = core.get_query_status("q-lifecycle").await.unwrap();
        assert_eq!(status, ComponentStatus::Running);

        // Stop the query
        core.stop_query("q-lifecycle").await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let status = core.get_query_status("q-lifecycle").await.unwrap();
        assert_eq!(status, ComponentStatus::Stopped);
    }

    // ========================================================================
    // get_query_config
    // ========================================================================

    #[tokio::test]
    async fn get_query_config_returns_correct_fields() {
        let core = build_core_with_source().await;

        let config = Query::cypher("q-config")
            .query("MATCH (n:Person) RETURN n.name")
            .from_source("test-source")
            .auto_start(false)
            .build();
        core.add_query(config).await.unwrap();

        let retrieved = core.get_query_config("q-config").await.unwrap();
        assert_eq!(retrieved.id, "q-config");
        assert_eq!(retrieved.query, "MATCH (n:Person) RETURN n.name");
        assert!(!retrieved.auto_start);
        assert_eq!(retrieved.sources.len(), 1);
        assert_eq!(retrieved.sources[0].source_id, "test-source");
    }
}
