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
//! `ComponentName` selectors are exact current query names, including UUID-shaped names.
//! Use the paired `_by_id` methods for explicit [`crate::config::ComponentId`]
//! selection.

use futures::{stream::Stream, StreamExt};

use crate::channels::{ComponentEvent, ComponentStatus};
use crate::component_ops::map_component_error;
use crate::config::{ComponentId, QueryConfig, QueryRuntime};
use crate::error::Result;
use crate::lib_core::DrasiLib;
use crate::queries::QueryResultSubscription;
use crate::ComponentName;

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
    ///     Query::cypher(drasi_lib::ComponentName::try_new("new-query")?)
    ///         .query("MATCH (n) RETURN n")
    ///         .from_input_by_name(drasi_lib::ComponentName::try_new("source1")?)
    ///         .auto_start(true)
    ///         .build()?
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
    /// A missing source is a visible dependency wait. Supplying it resumes the
    /// existing declaration without re-adding the query.
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
        query
            .validate_declarations()
            .map_err(|error| crate::error::DrasiError::validation(error.to_string()))?;
        let name = query.name.clone();
        self.computation_runtime
            .add_query(query, self.is_running().await)
            .await
            .and_then(|handle| self.computation_runtime.public_handle(handle))
            .map_err(|error| {
                crate::DrasiError::operation_failed(
                    "query",
                    crate::error::OperationSubject::Name(name),
                    "add",
                    format!("{error:#}"),
                )
                .with_cause(error)
            })
    }

    /// Remove a query from a running server
    ///
    /// If the query is running, it will be stopped first before removal.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_query_by_name(&drasi_lib::ComponentName::try_new("old-query")?).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_query_by_name(&self, name: &ComponentName) -> Result<()> {
        let id = self.resolve_component_name(name, "query")?;
        self.remove_query_by_id(&id).await
    }

    /// Remove the query with the supplied immutable UUID.
    pub async fn remove_query_by_id(&self, id: &ComponentId) -> Result<()> {
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
    /// core.start_query_by_name(&drasi_lib::ComponentName::try_new("my-query")?).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_query_by_name(&self, name: &ComponentName) -> Result<()> {
        let id = self.resolve_component_name(name, "query")?;
        self.start_query_by_id(&id).await
    }

    /// Start the query with the supplied immutable UUID.
    pub async fn start_query_by_id(&self, id: &ComponentId) -> Result<()> {
        self.state_guard.require_initialized()?;
        map_component_error(
            self.computation_runtime.start_component(id, "query").await,
            "query",
            id,
            "start",
        )
    }

    /// Stop a running, starting, or failed query.
    ///
    /// Queries in `Error` may still own subscriptions and processing tasks. This
    /// performs their normal cleanup and acknowledges the failure before
    /// reporting `Stopped`; it does not remove the query or discard its persisted
    /// state. A terminal construction failure stays in `Error` until the query is
    /// updated or removed. Stopping an already stopped query returns an error,
    /// unlike calling the underlying query runtime's `stop()` directly, which is
    /// idempotent.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_query_by_name(&drasi_lib::ComponentName::try_new("my-query")?).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_query_by_name(&self, name: &ComponentName) -> Result<()> {
        let id = self.resolve_component_name(name, "query")?;
        self.stop_query_by_id(&id).await
    }

    /// Stop the query with the supplied immutable UUID.
    pub async fn stop_query_by_id(&self, id: &ComponentId) -> Result<()> {
        self.state_guard.require_initialized()?;
        map_component_error(
            self.computation_runtime.stop_component(id, "query").await,
            "query",
            id,
            "stop",
        )
    }

    /// Ask the registered query factory to assess and apply a configuration change.
    ///
    /// The UUID and name are preserved. No reset flag or reconstruction strategy
    /// is inferred from a changed query. Rebootstrap plans are rejected unless
    /// the owner can establish snapshot/live and downstream-reset guarantees.
    pub async fn update_query_by_name(
        &self,
        name: &ComponentName,
        config: QueryConfig,
    ) -> Result<()> {
        let id = self.resolve_component_name(name, "query")?;
        self.update_query_by_id(&id, config).await
    }

    /// Update the query with the supplied immutable UUID.
    pub async fn update_query_by_id(&self, id: &ComponentId, config: QueryConfig) -> Result<()> {
        self.state_guard.require_initialized()?;
        let assigned = id.clone();
        if config
            .id
            .as_ref()
            .is_some_and(|supplied| supplied != &assigned)
        {
            return Err(crate::DrasiError::invalid_config(
                "an update cannot change the component UUID",
            ));
        }
        map_component_error(
            self.computation_runtime
                .update_query(id, config.with_assigned_id(assigned))
                .await,
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
    /// for component in queries {
    ///     println!("Query {}: {:?}", component.id, component.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_queries(&self) -> Result<Vec<crate::config::ComponentListEntry>> {
        self.inspection.list_queries().await
    }

    /// Get detailed information about a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let query_info = core.get_query_info_by_name(&drasi_lib::ComponentName::try_new("my-query")?).await?;
    /// println!("Query: {}", query_info.query);
    /// println!("Status: {:?}", query_info.status);
    /// println!("Inputs: {:?}", query_info.inputs);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_info_by_name(&self, name: &ComponentName) -> Result<QueryRuntime> {
        let id = self.resolve_component_name(name, "query")?;
        self.get_query_info_by_id(&id).await
    }

    /// Get detailed query information by immutable UUID.
    pub async fn get_query_info_by_id(&self, id: &ComponentId) -> Result<QueryRuntime> {
        self.inspection.get_query_info_by_id(id).await
    }

    /// Get the current status of a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_query_status_by_name(&drasi_lib::ComponentName::try_new("my-query")?).await?;
    /// println!("Query status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_status_by_name(&self, name: &ComponentName) -> Result<ComponentStatus> {
        let id = self.resolve_component_name(name, "query")?;
        self.get_query_status_by_id(&id).await
    }

    /// Get query status by immutable UUID.
    pub async fn get_query_status_by_id(&self, id: &ComponentId) -> Result<ComponentStatus> {
        self.inspection.get_query_status_by_id(id).await
    }

    /// Get the current result set for a running query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let results = core.get_query_results_by_name(&drasi_lib::ComponentName::try_new("my-query")?).await?;
    /// println!("Current results: {} items", results.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_results_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<Vec<serde_json::Value>> {
        let id = self.resolve_component_name(name, "query")?;
        self.get_query_results_by_id(&id).await
    }

    /// Get the current query result set by immutable UUID.
    pub async fn get_query_results_by_id(
        &self,
        id: &ComponentId,
    ) -> Result<Vec<serde_json::Value>> {
        self.inspection.get_query_results_by_id(id).await
    }

    /// Read a consistent snapshot followed by changes, without a reaction or worker.
    ///
    /// Registers the live reader before capturing the snapshot, then delivers
    /// only changes after that snapshot's sequence. Apply `snapshot` first and
    /// then consume `stream`; reconnecting creates a fresh snapshot/stream pair.
    /// The query's configured delivery mode and capacity apply. Broadcast gaps
    /// replay from retained output, or return an error if history is gone.
    ///
    /// This is a connection-local reader, not a durable consumer checkpoint.
    /// Replacement, output reset and owner shutdown invalidate it. Dropping the
    /// stream unregisters it synchronously, even if it was never polled.
    pub async fn subscribe_query_results_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<QueryResultSubscription> {
        let id = self.resolve_component_name(name, "query")?;
        self.subscribe_query_results_by_id(&id).await
    }

    /// Subscribe to query results by immutable UUID.
    pub async fn subscribe_query_results_by_id(
        &self,
        id: &ComponentId,
    ) -> Result<QueryResultSubscription> {
        self.state_guard.require_initialized()?;
        let result = async {
            self.computation_runtime
                .query(id)
                .await?
                .subscribe_results()
                .await
        }
        .await;
        let (snapshot, stream) = map_component_error(result, "query", id, "subscribe")?;
        let id = id.clone();
        Ok(QueryResultSubscription {
            snapshot,
            stream: stream
                .map(move |result| map_component_error(result, "query", &id, "receive"))
                .boxed(),
        })
    }

    /// Get the full configuration for a specific query
    ///
    /// This returns the complete query configuration including all fields like auto_start and joins,
    /// unlike `get_query_info_by_name()` which only returns runtime information.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_query_config_by_name(&drasi_lib::ComponentName::try_new("my-query")?).await?;
    /// println!("Auto-start: {}", config.auto_start);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_config_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<crate::config::AssignedQueryConfig> {
        let id = self.resolve_component_name(name, "query")?;
        self.get_query_config_by_id(&id).await
    }

    /// Get query configuration by immutable UUID.
    pub async fn get_query_config_by_id(
        &self,
        id: &ComponentId,
    ) -> Result<crate::config::AssignedQueryConfig> {
        self.inspection.get_query_config_by_id(id).await
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
    /// let mut events = core.get_query_events_by_name(&drasi_lib::ComponentName::try_new("my-query")?).await?;
    /// while let Some(event) = events.next().await {
    ///     println!("Event: {:?} - {:?}", event.status, event.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_events_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<impl Stream<Item = ComponentEvent>> {
        let id = self.resolve_component_name(name, "query")?;
        self.get_query_events_by_id(&id).await
    }

    /// Get query lifecycle events by immutable UUID.
    pub async fn get_query_events_by_id(
        &self,
        id: &ComponentId,
    ) -> Result<impl Stream<Item = ComponentEvent>> {
        self.inspection.get_query_events_by_id(id).await
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
    /// let (history, mut receiver) = core.subscribe_query_logs_by_name(&drasi_lib::ComponentName::try_new("my-query")?).await?;
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
    pub async fn subscribe_query_logs_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<(
        Vec<crate::observability::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::observability::LogMessage>,
    )> {
        let id = self.resolve_component_name(name, "query")?;
        self.subscribe_query_logs_by_id(&id).await
    }

    /// Subscribe to query logs by immutable UUID.
    pub async fn subscribe_query_logs_by_id(
        &self,
        id: &ComponentId,
    ) -> Result<(
        Vec<crate::observability::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::observability::LogMessage>,
    )> {
        self.inspection.subscribe_query_logs_by_id(id).await
    }

    /// Subscribe to live events for a query.
    ///
    /// Returns the event history (oldest first) and a broadcast receiver for new events
    /// as they occur. Events include lifecycle status changes such as Starting, Running,
    /// Error, Stopped.
    pub async fn subscribe_query_events_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        let id = self.resolve_component_name(name, "query")?;
        self.subscribe_query_events_by_id(&id).await
    }

    /// Subscribe to query lifecycle events by immutable UUID.
    pub async fn subscribe_query_events_by_id(
        &self,
        id: &ComponentId,
    ) -> Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        self.inspection.subscribe_query_events_by_id(id).await
    }
}

#[cfg(test)]
mod tests {
    use crate::channels::ComponentStatus;
    use crate::error::DrasiError;
    use crate::sources::tests::TestMockSource;
    use crate::test_helpers::creation::BuilderFixtures;
    use crate::{DrasiLib, Query};

    /// Build a DrasiLib with a single mock source, started and ready for queries.
    async fn build_core_with_source() -> DrasiLib {
        let source = TestMockSource::fixture("test-source", true);
        let core = DrasiLib::builder()
            .with_query_factory(
                crate::computation::v1::FactoryRegistry::standard()
                    .get(
                        &crate::computation::v1::ImplementationIdentity::try_new(
                            "drasi/continuous-query",
                            "1",
                        )
                        .expect("reference query identity"),
                    )
                    .expect("reference query factory")
                    .clone(),
            )
            .with_id("test")
            .with_source_fixture(source)
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

        let config = Query::cypher(
            drasi_core_types::ComponentName::try_new("q1").expect("fixture component name"),
        )
        .query("MATCH (n:Test) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();

        core.add_query(config).await.unwrap();

        // Query should appear in list
        let queries = core.list_queries().await.unwrap();
        assert!(queries
            .iter()
            .any(|entry| entry.name == crate::ComponentName::try_new("q1").unwrap()));
    }

    #[tokio::test]
    async fn add_query_missing_source_is_retained_as_waiting() {
        let core = build_core_with_source().await;

        let config = Query::cypher(
            drasi_core_types::ComponentName::try_new("q-bad").expect("fixture component name"),
        )
        .query("MATCH (n) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("nonexistent-source")
                .expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();

        let handle = core.add_query_with_handle(config).await.unwrap();
        let observed = handle.observed().unwrap();
        assert_eq!(
            observed.realization,
            crate::computation::v1::RealizationState::Blocked
        );
        assert!(observed.failure.is_none());
        assert!(observed
            .waiting_reason()
            .unwrap()
            .contains("nonexistent-source"));
        core.remove_query_by_name(
            &drasi_core_types::ComponentName::try_new("q-bad").expect("fixture component name"),
        )
        .await
        .unwrap();
        core.shutdown().await.unwrap();
    }

    // ========================================================================
    // remove_query
    // ========================================================================

    #[tokio::test]
    async fn remove_query_happy_path() {
        let core = build_core_with_source().await;

        let config = Query::cypher(
            drasi_core_types::ComponentName::try_new("q-remove").expect("fixture component name"),
        )
        .query("MATCH (n:Test) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();
        core.add_query(config).await.unwrap();

        // Remove should succeed
        core.remove_query_by_name(
            &drasi_core_types::ComponentName::try_new("q-remove").expect("fixture component name"),
        )
        .await
        .unwrap();

        // Query should no longer appear in list
        let queries = core.list_queries().await.unwrap();
        assert!(!queries
            .iter()
            .any(|entry| entry.name == crate::ComponentName::try_new("q-remove").unwrap()));
    }

    #[tokio::test]
    async fn remove_query_nonexistent_returns_error() {
        let core = build_core_with_source().await;

        let result = core
            .remove_query_by_name(
                &drasi_core_types::ComponentName::try_new("does-not-exist")
                    .expect("fixture component name"),
            )
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn remove_query_does_not_deregister_a_source() {
        let core = build_core_with_source().await;

        let result = core
            .remove_query_by_name(
                &drasi_core_types::ComponentName::try_new("test-source")
                    .expect("fixture component name"),
            )
            .await;
        assert!(result.is_err());

        let sources = core.list_sources().await.unwrap();
        assert!(
            sources
                .iter()
                .any(|entry| entry.name == crate::ComponentName::try_new("test-source").unwrap()),
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
        let config1 = Query::cypher(
            drasi_core_types::ComponentName::try_new("q-list-1").expect("fixture component name"),
        )
        .query("MATCH (n) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();
        let config2 = Query::cypher(
            drasi_core_types::ComponentName::try_new("q-list-2").expect("fixture component name"),
        )
        .query("MATCH (n) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();
        core.add_query(config1).await.unwrap();
        core.add_query(config2).await.unwrap();

        let queries = core.list_queries().await.unwrap();
        let names: Vec<_> = queries.iter().map(|entry| &entry.name).collect();
        assert!(names.contains(&&crate::ComponentName::try_new("q-list-1").unwrap()));
        assert!(names.contains(&&crate::ComponentName::try_new("q-list-2").unwrap()));
        assert_eq!(queries.len(), 2);
    }

    // ========================================================================
    // get_query_status
    // ========================================================================

    #[tokio::test]
    async fn get_query_status_returns_correct_status() {
        let core = build_core_with_source().await;

        let config = Query::cypher(
            drasi_core_types::ComponentName::try_new("q-status").expect("fixture component name"),
        )
        .query("MATCH (n:Test) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();
        core.add_query(config).await.unwrap();

        // Query added without auto-start should be Added
        let status = core
            .get_query_status_by_name(
                &drasi_core_types::ComponentName::try_new("q-status")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        assert_eq!(status, ComponentStatus::Added);
    }

    #[tokio::test]
    async fn get_query_status_nonexistent_returns_error() {
        let core = build_core_with_source().await;

        let result = core
            .get_query_status_by_name(
                &drasi_core_types::ComponentName::try_new("ghost-query")
                    .expect("fixture component name"),
            )
            .await;
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

        let config = Query::cypher(
            drasi_core_types::ComponentName::try_new("q-lifecycle")
                .expect("fixture component name"),
        )
        .query("MATCH (n:Test) RETURN n")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();
        core.add_query(config).await.unwrap();

        // Subscribe to events before triggering actions
        let mut event_rx = core.subscribe_all_component_events();

        // Start the query
        core.start_query_by_name(
            &drasi_core_types::ComponentName::try_new("q-lifecycle")
                .expect("fixture component name"),
        )
        .await
        .unwrap();

        crate::test_helpers::wait_for_component_status_by_name(
            &mut event_rx,
            &core.component_graph(),
            &crate::ComponentName::try_new("q-lifecycle").expect("fixture component name"),
            ComponentStatus::Running,
            std::time::Duration::from_secs(5),
        )
        .await;
        let status = core
            .get_query_status_by_name(
                &drasi_core_types::ComponentName::try_new("q-lifecycle")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        assert_eq!(status, ComponentStatus::Running);

        // Stop the query
        core.stop_query_by_name(
            &drasi_core_types::ComponentName::try_new("q-lifecycle")
                .expect("fixture component name"),
        )
        .await
        .unwrap();

        crate::test_helpers::wait_for_component_status_by_name(
            &mut event_rx,
            &core.component_graph(),
            &crate::ComponentName::try_new("q-lifecycle").expect("fixture component name"),
            ComponentStatus::Stopped,
            std::time::Duration::from_secs(5),
        )
        .await;
        let status = core
            .get_query_status_by_name(
                &drasi_core_types::ComponentName::try_new("q-lifecycle")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        assert_eq!(status, ComponentStatus::Stopped);
    }

    // ========================================================================
    // get_query_config
    // ========================================================================

    #[tokio::test]
    async fn get_query_config_returns_correct_fields() {
        let core = build_core_with_source().await;

        let config = Query::cypher(
            drasi_core_types::ComponentName::try_new("q-config").expect("fixture component name"),
        )
        .query("MATCH (n:Person) RETURN n.name")
        .from_input_by_name(
            drasi_core_types::ComponentName::try_new("test-source")
                .expect("fixture component name"),
        )
        .auto_start(false)
        .build()
        .unwrap();
        core.add_query(config).await.unwrap();

        let retrieved = core
            .get_query_config_by_name(
                &drasi_core_types::ComponentName::try_new("q-config")
                    .expect("fixture component name"),
            )
            .await
            .unwrap();
        assert_eq!(
            &retrieved.id,
            core.component_by_name(&retrieved.name).unwrap().id()
        );
        assert_eq!(retrieved.name.as_str(), "q-config");
        assert_eq!(retrieved.query, "MATCH (n:Person) RETURN n.name");
        assert!(!retrieved.auto_start);
        assert_eq!(retrieved.inputs.len(), 1);
        assert_eq!(
            retrieved.inputs[0].component.as_id(),
            Some(
                core.component_by_name(
                    &drasi_core_types::ComponentName::try_new("test-source")
                        .expect("fixture component name")
                )
                .unwrap()
                .id()
            )
        );
    }
}
