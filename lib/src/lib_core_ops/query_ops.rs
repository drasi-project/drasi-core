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

use futures::stream::Stream;

use crate::channels::{ComponentEvent, ComponentStatus};
use crate::component_ops::map_component_error;
use crate::config::{QueryConfig, QueryRuntime};
use crate::error::Result;
use crate::lib_core::DrasiLib;

impl DrasiLib {
    /// Create a query in a running server
    ///
    /// Success confirms node declaration. Parsing, dependency
    /// validation, construction and activation run afterward; failures remain on
    /// the node. Use `add_query_with_handle` to await those outcomes explicitly.
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
        self.add_query_with_handle(query).await.map(|_| ())
    }

    /// Declare a query and return its generation-bound computation handle.
    ///
    /// The original configuration is retained even when parsing or construction fails.
    /// Await `wait_created()` for validation and construction, or `wait_started()`
    /// for activation. Auto-start is requested only when the instance is running.
    /// `wait_started()` includes query bootstrap and actual `Running` readiness;
    /// ordinary `start_query()` retains subscription-ready completion.
    /// The handle also exposes restricted `control()` notifications and
    /// `set_control_handler()` for receiving them independently of query data.
    /// Ownership-bearing native rejections retain their `GraphError` through
    /// `DrasiError::Internal`, including the rejected addition's recovery handle.
    pub async fn add_query_with_handle(
        &self,
        query: QueryConfig,
    ) -> Result<crate::computation::v1::ComponentHandle> {
        self.state_guard.require_initialized()?;
        let id = query.id.clone();
        self.computation_runtime
            .add_query(query, self.is_running().await)
            .await
            .map_err(|error| crate::computation::runtime::map_addition_error("query", &id, error))
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
        map_component_error(
            self.computation_runtime
                .remove_component(id, "query", true)
                .await,
            "query",
            id,
            "remove",
        )
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
        map_component_error(
            self.computation_runtime.start_component(id, "query").await,
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
        map_component_error(
            self.computation_runtime.stop_component(id, "query").await,
            "query",
            id,
            "stop",
        )
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
        map_component_error(
            self.computation_runtime.update_query(id, config).await,
            "query",
            id,
            "update",
        )
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

        let handle = core.add_query_with_handle(config).await.unwrap();
        assert!(handle.wait_created().await.is_err());
        let observed = handle.observed().unwrap();
        assert_eq!(
            observed.realization,
            crate::computation::v1::RealizationState::CreationFailed
        );
        assert!(format!("{:#}", observed.failure.as_ref().unwrap().cause)
            .contains("nonexistent-source"));
        core.remove_query("q-bad").await.unwrap();
        core.shutdown().await.unwrap();
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

    #[tokio::test]
    async fn remove_query_does_not_deregister_a_source() {
        let core = build_core_with_source().await;

        let result = core.remove_query("test-source").await;
        assert!(result.is_err());

        let sources = core.list_sources().await.unwrap();
        assert!(
            sources.iter().any(|(id, _)| id == "test-source"),
            "remove_query must not deregister a source id, got {sources:?}"
        );
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

        // Query added without auto-start should be Added
        let status = core.get_query_status("q-status").await.unwrap();
        assert_eq!(status, ComponentStatus::Added);
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

        // Subscribe to events before triggering actions
        let mut event_rx = core.subscribe_all_component_events();

        // Start the query
        core.start_query("q-lifecycle").await.unwrap();

        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "q-lifecycle",
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        let status = core.get_query_status("q-lifecycle").await.unwrap();
        assert_eq!(status, ComponentStatus::Running);

        // Stop the query
        core.stop_query("q-lifecycle").await.unwrap();

        crate::test_helpers::wait_for_component_status(
            &mut event_rx,
            "q-lifecycle",
            ComponentStatus::Stopped,
            std::time::Duration::from_secs(5),
        )
        .await;
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
