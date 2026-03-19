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

//! Reaction management operations for DrasiLib
//!
//! This module provides all reaction-related operations including adding, removing,
//! starting, and stopping reactions.

use futures::stream::Stream;
use std::collections::HashMap;

use crate::channels::{ComponentEvent, ComponentStatus};
use crate::component_ops::map_component_error;
use crate::config::ReactionRuntime;
use crate::error::{DrasiError, Result};
use crate::lib_core::DrasiLib;
use crate::reactions::Reaction;

impl DrasiLib {
    /// Add a reaction instance to a running server, taking ownership.
    ///
    /// The reaction instance is wrapped in an Arc internally - callers transfer
    /// ownership rather than pre-wrapping in Arc.
    ///
    /// If the server is running and the reaction has `auto_start=true`, the reaction
    /// will be started immediately after being added.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// // Create the reaction and transfer ownership
    /// // let reaction = MyReaction::new("my-reaction", vec!["query1".into()]);
    /// // core.add_reaction(reaction).await?;  // Ownership transferred, auto-started if server running
    /// # Ok(())
    /// # }
    /// ```
    pub async fn add_reaction(&self, reaction: impl Reaction + 'static) -> Result<()> {
        self.state_guard.require_initialized()?;

        // Capture auto_start and id before transferring ownership
        let should_auto_start = reaction.auto_start();
        let reaction_id = reaction.id().to_string();
        let reaction_type = reaction.type_name().to_string();
        let query_ids = reaction.query_ids();

        // Step 1: Register in the component graph (validates queries exist, creates node + edges)
        {
            let mut graph = self.component_graph.write().await;
            let mut metadata = HashMap::new();
            metadata.insert("kind".to_string(), reaction_type);
            graph
                .register_reaction(&reaction_id, metadata, &query_ids)
                .map_err(|e| {
                    DrasiError::operation_failed(
                        "reaction",
                        &reaction_id,
                        "add",
                        format!("Failed to register: {e}"),
                    )
                })?;
        }

        // Step 2: Provision runtime (initialize + store)
        if let Err(e) = self.reaction_manager.provision_reaction(reaction).await {
            // Compensating rollback: remove from graph on runtime failure
            let mut graph = self.component_graph.write().await;
            let _ = graph.deregister(&reaction_id);
            return Err(DrasiError::operation_failed(
                "reaction",
                &reaction_id,
                "add",
                format!("Failed to provision: {e}"),
            ));
        }

        // Step 3: Auto-start if needed
        if self.is_running().await && should_auto_start {
            self.reaction_manager
                .start_reaction(reaction_id.clone())
                .await
                .map_err(|e| {
                    DrasiError::operation_failed("reaction", &reaction_id, "start", format!("{e}"))
                })?;
        }

        Ok(())
    }

    /// Remove a reaction from a running server
    ///
    /// If the reaction is running, it will be stopped first before removal.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_reaction("old-reaction", false).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_reaction(&self, id: &str, cleanup: bool) -> Result<()> {
        self.state_guard.require_initialized()?;

        // Step 1: Validate no dependents
        {
            let graph = self.component_graph.read().await;
            if let Err(dependent_ids) = graph.can_remove(id) {
                return Err(DrasiError::operation_failed(
                    "reaction",
                    id,
                    "remove",
                    format!("Depended on by: {}", dependent_ids.join(", ")),
                ));
            }
        }

        // Step 2: Teardown runtime (stop, deprovision, remove from runtime map)
        self.reaction_manager
            .teardown_reaction(id.to_string(), cleanup)
            .await
            .map_err(|e| {
                DrasiError::operation_failed(
                    "reaction",
                    id,
                    "remove",
                    format!("Teardown failed: {e}"),
                )
            })?;

        // Step 3: Deregister from graph (remove node + edges, emit events)
        {
            let mut graph = self.component_graph.write().await;
            graph.deregister(id).map_err(|e| {
                DrasiError::operation_failed(
                    "reaction",
                    id,
                    "remove",
                    format!("Deregister failed: {e}"),
                )
            })?;
        }

        Ok(())
    }

    /// Update a reaction by replacing it with a new instance.
    ///
    /// Uses the `Reconfiguring` state transition to preserve the graph node, edges,
    /// and event history. The old reaction is stopped, the runtime is swapped, and the
    /// reaction is restarted if it was running.
    ///
    /// The new reaction must have the same ID as the existing one.
    ///
    /// # Errors
    ///
    /// Returns an error if the reaction doesn't exist, if the IDs don't match,
    /// if referenced queries don't exist, or if provisioning fails.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// // let new_reaction = MyReaction::new(updated_config);
    /// // core.update_reaction("my-reaction", new_reaction).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn update_reaction(
        &self,
        id: &str,
        new_reaction: impl crate::reactions::Reaction + 'static,
    ) -> Result<()> {
        self.state_guard.require_initialized()?;

        // Validate the new reaction has the same ID
        if new_reaction.id() != id {
            return Err(DrasiError::operation_failed(
                "reaction",
                id,
                "update",
                format!(
                    "New reaction ID '{}' does not match existing reaction ID '{}'",
                    new_reaction.id(),
                    id
                ),
            ));
        }

        // Delegate to ReactionManager which uses the Reconfiguring transition,
        // preserving the graph node, edges, and event history.
        self.reaction_manager
            .update_reaction(id.to_string(), new_reaction)
            .await
            .map_err(|e| DrasiError::operation_failed("reaction", id, "update", e.to_string()))
    }

    /// Start a stopped reaction
    ///
    /// This will create the necessary subscriptions to query result streams.
    /// The QueryProvider was already injected when the reaction was added via `add_reaction()`.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.start_reaction("my-reaction").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_reaction(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized()?;

        // Start the reaction (QueryProvider was injected when reaction was added)
        map_component_error(
            self.reaction_manager.start_reaction(id.to_string()).await,
            "reaction",
            id,
            "start",
        )
    }

    /// Stop a running reaction
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_reaction("my-reaction").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_reaction(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized()?;

        // Stop the reaction (subscriptions managed by reaction itself)
        map_component_error(
            self.reaction_manager.stop_reaction(id.to_string()).await,
            "reaction",
            id,
            "stop",
        )?;

        Ok(())
    }

    /// List all reactions with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let reactions = core.list_reactions().await?;
    /// for (id, status) in reactions {
    ///     println!("Reaction {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_reactions(&self) -> Result<Vec<(String, ComponentStatus)>> {
        self.inspection.list_reactions().await
    }

    /// Get detailed information about a specific reaction
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let reaction_info = core.get_reaction_info("my-reaction").await?;
    /// println!("Reaction type: {}", reaction_info.reaction_type);
    /// println!("Status: {:?}", reaction_info.status);
    /// println!("Queries: {:?}", reaction_info.queries);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_info(&self, id: &str) -> Result<ReactionRuntime> {
        self.inspection.get_reaction_info(id).await
    }

    /// Get the current status of a specific reaction
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_reaction_status("my-reaction").await?;
    /// println!("Reaction status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_status(&self, id: &str) -> Result<ComponentStatus> {
        self.inspection.get_reaction_status(id).await
    }

    /// Get lifecycle events for a specific reaction as an async stream.
    ///
    /// Returns events in chronological order (oldest first). Up to 100 most recent
    /// events are retained per component.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_reaction_events("my-reaction").await?;
    /// while let Some(event) = events.next().await {
    ///     println!("Event: {:?} - {:?}", event.status, event.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_events(
        &self,
        id: &str,
    ) -> Result<impl Stream<Item = ComponentEvent>> {
        self.inspection.get_reaction_events(id).await
    }

    /// Get all lifecycle events across all reactions as an async stream.
    ///
    /// Returns events sorted by timestamp (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_all_reaction_events().await?;
    /// while let Some(event) = events.next().await {
    ///     println!("{}: {:?}", event.component_id, event.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_all_reaction_events(&self) -> Result<impl Stream<Item = ComponentEvent>> {
        self.inspection.get_all_reaction_events().await
    }

    /// Get all lifecycle events across all components (sources, queries, reactions) as an async stream.
    ///
    /// Returns events sorted by timestamp (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_all_events().await?;
    /// while let Some(event) = events.next().await {
    ///     println!("{} ({:?}): {:?}", event.component_id, event.component_type, event.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_all_events(&self) -> Result<impl Stream<Item = ComponentEvent>> {
        self.inspection.get_all_events().await
    }

    /// Subscribe to live logs for a reaction.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// The receiver will receive new log messages as they are emitted by the reaction.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let (history, mut receiver) = core.subscribe_reaction_logs("my-reaction").await?;
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
    pub async fn subscribe_reaction_logs(
        &self,
        id: &str,
    ) -> Result<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        self.inspection.subscribe_reaction_logs(id).await
    }

    /// Subscribe to live events for a reaction.
    ///
    /// Returns the event history (oldest first) and a broadcast receiver for new events
    /// as they occur. Events include lifecycle status changes such as Starting, Running,
    /// Error, Stopped.
    pub async fn subscribe_reaction_events(
        &self,
        id: &str,
    ) -> Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        self.inspection.subscribe_reaction_events(id).await
    }
}

#[cfg(test)]
mod tests {
    use crate::channels::{ComponentStatus, QueryResult};
    use crate::error::DrasiError;
    use crate::sources::tests::TestMockSource;
    use crate::{DrasiLib, Query};
    use anyhow::Result as AnyhowResult;
    use async_trait::async_trait;
    use std::collections::HashMap;

    // ========================================================================
    // Mock reaction for testing
    // ========================================================================

    struct TestMockReaction {
        id: String,
        queries: Vec<String>,
        auto_start: bool,
        status_handle: crate::component_graph::ComponentStatusHandle,
    }

    impl TestMockReaction {
        fn new(id: String, queries: Vec<String>) -> Self {
            let status_handle = crate::component_graph::ComponentStatusHandle::new(&id);
            Self {
                id,
                queries,
                auto_start: true,
                status_handle,
            }
        }

        fn with_auto_start(id: String, queries: Vec<String>, auto_start: bool) -> Self {
            let status_handle = crate::component_graph::ComponentStatusHandle::new(&id);
            Self {
                id,
                queries,
                auto_start,
                status_handle,
            }
        }
    }

    #[async_trait]
    impl crate::reactions::Reaction for TestMockReaction {
        fn id(&self) -> &str {
            &self.id
        }

        fn type_name(&self) -> &str {
            "test-log"
        }

        fn properties(&self) -> HashMap<String, serde_json::Value> {
            HashMap::new()
        }

        fn query_ids(&self) -> Vec<String> {
            self.queries.clone()
        }

        fn auto_start(&self) -> bool {
            self.auto_start
        }

        async fn initialize(&self, context: crate::context::ReactionRuntimeContext) {
            self.status_handle.wire(context.update_tx.clone()).await;
        }

        async fn start(&self) -> AnyhowResult<()> {
            self.status_handle
                .set_status(
                    ComponentStatus::Starting,
                    Some("Starting reaction".to_string()),
                )
                .await;
            self.status_handle
                .set_status(
                    ComponentStatus::Running,
                    Some("Reaction started".to_string()),
                )
                .await;
            Ok(())
        }

        async fn stop(&self) -> AnyhowResult<()> {
            self.status_handle
                .set_status(
                    ComponentStatus::Stopping,
                    Some("Stopping reaction".to_string()),
                )
                .await;
            self.status_handle
                .set_status(
                    ComponentStatus::Stopped,
                    Some("Reaction stopped".to_string()),
                )
                .await;
            Ok(())
        }

        async fn status(&self) -> ComponentStatus {
            self.status_handle.get_status().await
        }

        async fn enqueue_query_result(&self, _result: QueryResult) -> AnyhowResult<()> {
            Ok(())
        }
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    /// Build a started DrasiLib with a mock source and one query.
    async fn build_core_with_query() -> DrasiLib {
        let source = TestMockSource::new("test-source".to_string()).unwrap();
        let core = DrasiLib::builder()
            .with_id("test")
            .with_source(source)
            .with_query(
                Query::cypher("q1")
                    .query("MATCH (n:Test) RETURN n")
                    .from_source("test-source")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();
        core.start().await.unwrap();
        core
    }

    // ========================================================================
    // add_reaction
    // ========================================================================

    #[tokio::test]
    async fn add_reaction_happy_path() {
        let core = build_core_with_query().await;

        let reaction = TestMockReaction::with_auto_start("r1".into(), vec!["q1".into()], false);
        core.add_reaction(reaction).await.unwrap();

        let reactions = core.list_reactions().await.unwrap();
        assert!(reactions.iter().any(|(id, _)| id == "r1"));
    }

    #[tokio::test]
    async fn add_reaction_duplicate_returns_error() {
        let core = build_core_with_query().await;

        let r1 = TestMockReaction::with_auto_start("r-dup".into(), vec!["q1".into()], false);
        core.add_reaction(r1).await.unwrap();

        let r2 = TestMockReaction::with_auto_start("r-dup".into(), vec!["q1".into()], false);
        let result = core.add_reaction(r2).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, DrasiError::OperationFailed { .. }),
            "expected OperationFailed, got: {err:?}"
        );
    }

    // ========================================================================
    // remove_reaction
    // ========================================================================

    #[tokio::test]
    async fn remove_reaction_happy_path() {
        let core = build_core_with_query().await;

        let reaction =
            TestMockReaction::with_auto_start("r-remove".into(), vec!["q1".into()], false);
        core.add_reaction(reaction).await.unwrap();

        core.remove_reaction("r-remove", false).await.unwrap();

        let reactions = core.list_reactions().await.unwrap();
        assert!(!reactions.iter().any(|(id, _)| id == "r-remove"));
    }

    #[tokio::test]
    async fn remove_reaction_nonexistent_returns_error() {
        let core = build_core_with_query().await;

        let result = core.remove_reaction("does-not-exist", false).await;
        assert!(result.is_err());
    }

    // ========================================================================
    // list_reactions
    // ========================================================================

    #[tokio::test]
    async fn list_reactions_empty_then_populated() {
        let core = build_core_with_query().await;

        // Initially no reactions
        let reactions = core.list_reactions().await.unwrap();
        assert!(reactions.is_empty(), "expected no reactions initially");

        // Add two reactions
        let r1 = TestMockReaction::with_auto_start("r-list-1".into(), vec!["q1".into()], false);
        let r2 = TestMockReaction::with_auto_start("r-list-2".into(), vec!["q1".into()], false);
        core.add_reaction(r1).await.unwrap();
        core.add_reaction(r2).await.unwrap();

        let reactions = core.list_reactions().await.unwrap();
        let ids: Vec<&str> = reactions.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"r-list-1"));
        assert!(ids.contains(&"r-list-2"));
        assert_eq!(reactions.len(), 2);
    }

    // ========================================================================
    // get_reaction_status
    // ========================================================================

    #[tokio::test]
    async fn get_reaction_status_returns_correct_status() {
        let core = build_core_with_query().await;

        let reaction =
            TestMockReaction::with_auto_start("r-status".into(), vec!["q1".into()], false);
        core.add_reaction(reaction).await.unwrap();

        let status = core.get_reaction_status("r-status").await.unwrap();
        assert_eq!(status, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn get_reaction_status_nonexistent_returns_error() {
        let core = build_core_with_query().await;

        let result = core.get_reaction_status("ghost-reaction").await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, DrasiError::ComponentNotFound { .. }),
            "expected ComponentNotFound, got: {err:?}"
        );
    }

    // ========================================================================
    // start_reaction / stop_reaction lifecycle
    // ========================================================================

    #[tokio::test]
    async fn start_and_stop_reaction_lifecycle() {
        let core = build_core_with_query().await;

        let reaction =
            TestMockReaction::with_auto_start("r-lifecycle".into(), vec!["q1".into()], false);
        core.add_reaction(reaction).await.unwrap();

        // Start the reaction
        core.start_reaction("r-lifecycle").await.unwrap();

        // Allow async status propagation
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let status = core.get_reaction_status("r-lifecycle").await.unwrap();
        assert_eq!(status, ComponentStatus::Running);

        // Stop the reaction
        core.stop_reaction("r-lifecycle").await.unwrap();

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let status = core.get_reaction_status("r-lifecycle").await.unwrap();
        assert_eq!(status, ComponentStatus::Stopped);
    }

    // ========================================================================
    // get_reaction_info
    // ========================================================================

    #[tokio::test]
    async fn get_reaction_info_returns_correct_fields() {
        let core = build_core_with_query().await;

        let reaction = TestMockReaction::with_auto_start("r-info".into(), vec!["q1".into()], false);
        core.add_reaction(reaction).await.unwrap();

        let info = core.get_reaction_info("r-info").await.unwrap();
        assert_eq!(info.id, "r-info");
        assert_eq!(info.reaction_type, "test-log");
        assert_eq!(info.status, ComponentStatus::Stopped);
        assert_eq!(info.queries, vec!["q1".to_string()]);
    }
}
