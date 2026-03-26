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

//! Source management operations for DrasiLib
//!
//! This module provides all source-related operations including adding, removing,
//! starting, and stopping sources.

use futures::stream::Stream;
use std::collections::HashMap;

use crate::channels::{ComponentEvent, ComponentStatus};
use crate::component_ops::map_component_error;
use crate::config::SourceRuntime;
use crate::error::{DrasiError, Result};
use crate::lib_core::DrasiLib;
use crate::sources::Source;

impl DrasiLib {
    /// Add a source instance to a running server, taking ownership.
    ///
    /// The source instance is wrapped in an Arc internally - callers transfer
    /// ownership rather than pre-wrapping in Arc.
    ///
    /// If the server is running and the source has `auto_start=true`, the source
    /// will be started immediately after being added.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// // Create the source and transfer ownership
    /// // let source = MySource::new("new-source", config)?;
    /// // core.add_source(source).await?;  // Ownership transferred, auto-started if server running
    /// # Ok(())
    /// # }
    /// ```
    pub async fn add_source(&self, source: impl Source + 'static) -> Result<()> {
        self.add_source_with_metadata(source, HashMap::new()).await
    }

    /// Add a source to a running server with additional metadata.
    ///
    /// Same as [`add_source`](Self::add_source) but merges `extra_metadata`
    /// (e.g. `pluginId`, `pluginGeneration`) into the component graph node.
    pub async fn add_source_with_metadata(
        &self,
        source: impl Source + 'static,
        extra_metadata: HashMap<String, String>,
    ) -> Result<()> {
        self.state_guard.require_initialized()?;

        // Capture auto_start and id before transferring ownership
        let should_auto_start = source.auto_start();
        let source_id = source.id().to_string();
        let source_type = source.type_name().to_string();

        // Step 1: Register in the component graph (validates uniqueness, creates node + edges)
        {
            let mut graph = self.component_graph.write().await;
            let mut metadata = HashMap::new();
            metadata.insert("kind".to_string(), source_type);
            metadata.insert("autoStart".to_string(), should_auto_start.to_string());
            metadata.extend(extra_metadata);
            graph.register_source(&source_id, metadata).map_err(|e| {
                DrasiError::operation_failed(
                    "source",
                    &source_id,
                    "add",
                    format!("Failed to register: {e}"),
                )
            })?;
        }

        // Step 2: Provision runtime (initialize + store)
        if let Err(e) = self.source_manager.provision_source(source).await {
            // Compensating rollback: remove from graph on runtime failure
            let mut graph = self.component_graph.write().await;
            let _ = graph.deregister(&source_id);
            return Err(DrasiError::operation_failed(
                "source",
                &source_id,
                "add",
                format!("Failed to provision: {e}"),
            ));
        }

        // Step 3: Auto-start if needed
        if self.is_running().await && should_auto_start {
            self.source_manager
                .start_source(source_id.clone())
                .await
                .map_err(|e| {
                    DrasiError::operation_failed("source", &source_id, "start", format!("{e}"))
                })?;
        }

        Ok(())
    }

    /// Remove a source from a running server
    ///
    /// If the source is running, it will be stopped first before removal.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_source("old-source", false).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_source(&self, id: &str, cleanup: bool) -> Result<()> {
        self.state_guard.require_initialized()?;

        // Step 1: Validate no dependents
        {
            let graph = self.component_graph.read().await;
            if let Err(dependent_ids) = graph.can_remove(id) {
                return Err(DrasiError::operation_failed(
                    "source",
                    id,
                    "remove",
                    format!("Depended on by: {}", dependent_ids.join(", ")),
                ));
            }
        }

        // Step 2: Teardown runtime (stop, deprovision, remove from runtime map)
        self.source_manager
            .teardown_source(id.to_string(), cleanup)
            .await
            .map_err(|e| {
                DrasiError::operation_failed(
                    "source",
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
                    "source",
                    id,
                    "remove",
                    format!("Deregister failed: {e}"),
                )
            })?;
        }

        Ok(())
    }

    /// Update a source by replacing it with a new instance.
    ///
    /// Uses the `Reconfiguring` state transition to preserve the graph node, edges,
    /// and event history. The old source is stopped, the runtime is swapped, and the
    /// source is restarted if it was running.
    ///
    /// The new source must have the same ID as the existing one.
    ///
    /// # Errors
    ///
    /// Returns an error if the source doesn't exist, if the IDs don't match,
    /// or if the new source cannot be started.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// // let new_source = MySource::new(updated_config)?;
    /// // core.update_source("my-source", new_source).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn update_source(
        &self,
        id: &str,
        new_source: impl crate::sources::Source + 'static,
    ) -> Result<()> {
        self.state_guard.require_initialized()?;

        // Validate the new source has the same ID
        if new_source.id() != id {
            return Err(DrasiError::operation_failed(
                "source",
                id,
                "update",
                format!(
                    "New source ID '{}' does not match existing source ID '{}'",
                    new_source.id(),
                    id
                ),
            ));
        }

        // Delegate to SourceManager which uses the Reconfiguring transition,
        // preserving the graph node, edges, and event history.
        self.source_manager
            .update_source(id.to_string(), new_source)
            .await
            .map_err(|e| DrasiError::operation_failed("source", id, "update", e.to_string()))
    }

    /// Start a stopped source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.start_source("my-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_source(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized()?;

        map_component_error(
            self.source_manager.start_source(id.to_string()).await,
            "source",
            id,
            "start",
        )
    }

    /// Stop a running source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_source("my-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_source(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized()?;

        map_component_error(
            self.source_manager.stop_source(id.to_string()).await,
            "source",
            id,
            "stop",
        )
    }

    /// List all sources with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let sources = core.list_sources().await?;
    /// for (id, status) in sources {
    ///     println!("Source {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_sources(&self) -> Result<Vec<(String, ComponentStatus)>> {
        self.inspection.list_sources().await
    }

    /// Get detailed information about a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let source_info = core.get_source_info("my-source").await?;
    /// println!("Source type: {}", source_info.source_type);
    /// println!("Status: {:?}", source_info.status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_info(&self, id: &str) -> Result<SourceRuntime> {
        self.inspection.get_source_info(id).await
    }

    /// Get the current status of a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_source_status("my-source").await?;
    /// println!("Source status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_status(&self, id: &str) -> Result<ComponentStatus> {
        self.inspection.get_source_status(id).await
    }

    /// Get lifecycle events for a specific source as an async stream.
    ///
    /// Returns events in chronological order (oldest first). Up to 100 most recent
    /// events are retained per component.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_source_events("my-source").await?;
    /// while let Some(event) = events.next().await {
    ///     println!("Event: {:?} - {:?}", event.status, event.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_events(&self, id: &str) -> Result<impl Stream<Item = ComponentEvent>> {
        self.inspection.get_source_events(id).await
    }

    /// Get all lifecycle events across all sources as an async stream.
    ///
    /// Returns events sorted by timestamp (oldest first).
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use futures::StreamExt;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let mut events = core.get_all_source_events().await?;
    /// while let Some(event) = events.next().await {
    ///     println!("{}: {:?}", event.component_id, event.status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_all_source_events(&self) -> Result<impl Stream<Item = ComponentEvent>> {
        self.inspection.get_all_source_events().await
    }

    /// Subscribe to live logs for a source.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// The receiver will receive new log messages as they are emitted by the source.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let (history, mut receiver) = core.subscribe_source_logs("my-source").await?;
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
    pub async fn subscribe_source_logs(
        &self,
        id: &str,
    ) -> Result<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        self.inspection.subscribe_source_logs(id).await
    }

    /// Subscribe to live events for a source.
    ///
    /// Returns the event history (oldest first) and a broadcast receiver for new events
    /// as they occur. Events include lifecycle status changes such as Starting, Running,
    /// Error, Stopped.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// # async fn example(core: &DrasiLib) -> anyhow::Result<()> {
    /// let (history, mut receiver) = core.subscribe_source_events("my-source").await?;
    ///
    /// // Print historical events
    /// for event in history {
    ///     println!("[{:?}] {:?}: {:?}", event.timestamp, event.status, event.message);
    /// }
    ///
    /// // Listen for new events
    /// while let Ok(event) = receiver.recv().await {
    ///     println!("[{:?}] {:?}: {:?}", event.timestamp, event.status, event.message);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn subscribe_source_events(
        &self,
        id: &str,
    ) -> Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        self.inspection.subscribe_source_events(id).await
    }
}

#[cfg(test)]
mod tests {
    use crate::channels::ComponentStatus;
    use crate::error::DrasiError;
    use crate::lib_core::DrasiLib;
    use crate::sources::tests::{create_test_mock_source, TestMockSource};
    use crate::sources::COMPONENT_GRAPH_SOURCE_ID;
    use crate::test_helpers::wait_for_component_status;
    use std::time::Duration;

    /// Helper: build a DrasiLib, start it, and return it.
    async fn build_and_start() -> DrasiLib {
        let core = DrasiLib::builder().with_id("test").build().await.unwrap();
        core.start().await.unwrap();
        core
    }

    // ========================================================================
    // add_source
    // ========================================================================

    #[tokio::test]
    async fn add_source_happy_path() {
        let core = build_and_start().await;
        let source = create_test_mock_source("src-1".to_string());

        core.add_source(source).await.unwrap();

        let sources = core.list_sources().await.unwrap();
        assert!(
            sources.iter().any(|(id, _)| id == "src-1"),
            "Added source should appear in list"
        );
    }

    #[tokio::test]
    async fn add_source_duplicate_returns_error() {
        let core = build_and_start().await;
        let s1 = create_test_mock_source("dup-src".to_string());
        let s2 = create_test_mock_source("dup-src".to_string());

        core.add_source(s1).await.unwrap();
        let err = core.add_source(s2).await.unwrap_err();

        assert!(
            matches!(err, DrasiError::OperationFailed { .. }),
            "Duplicate add should return OperationFailed, got: {err:?}"
        );
    }

    // ========================================================================
    // remove_source
    // ========================================================================

    #[tokio::test]
    async fn remove_source_happy_path() {
        let core = build_and_start().await;
        let source = TestMockSource::with_auto_start("rm-src".to_string(), false).unwrap();

        core.add_source(source).await.unwrap();
        assert!(core
            .list_sources()
            .await
            .unwrap()
            .iter()
            .any(|(id, _)| id == "rm-src"));

        core.remove_source("rm-src", false).await.unwrap();
        assert!(
            !core
                .list_sources()
                .await
                .unwrap()
                .iter()
                .any(|(id, _)| id == "rm-src"),
            "Removed source should not appear in list"
        );
    }

    #[tokio::test]
    async fn remove_source_nonexistent_returns_error() {
        let core = build_and_start().await;
        let err = core
            .remove_source("no-such-source", false)
            .await
            .unwrap_err();

        assert!(
            matches!(err, DrasiError::OperationFailed { .. }),
            "Removing nonexistent source should fail, got: {err:?}"
        );
    }

    // ========================================================================
    // list_sources
    // ========================================================================

    #[tokio::test]
    async fn list_sources_empty_then_populated() {
        let core = build_and_start().await;

        // Fresh server has at least the internal component-graph source
        let initial = core.list_sources().await.unwrap();
        let user_sources: Vec<_> = initial
            .iter()
            .filter(|(id, _)| id != COMPONENT_GRAPH_SOURCE_ID)
            .collect();
        assert!(user_sources.is_empty(), "No user sources initially");

        // Add two sources
        let s1 = TestMockSource::with_auto_start("list-s1".to_string(), false).unwrap();
        let s2 = TestMockSource::with_auto_start("list-s2".to_string(), false).unwrap();
        core.add_source(s1).await.unwrap();
        core.add_source(s2).await.unwrap();

        let after = core.list_sources().await.unwrap();
        let user_sources: Vec<_> = after
            .iter()
            .filter(|(id, _)| id != COMPONENT_GRAPH_SOURCE_ID)
            .collect();
        assert_eq!(user_sources.len(), 2, "Should have 2 user sources");
    }

    // ========================================================================
    // get_source_status
    // ========================================================================

    #[tokio::test]
    async fn get_source_status_returns_correct_status() {
        let core = build_and_start().await;
        let source = TestMockSource::with_auto_start("status-src".to_string(), false).unwrap();

        core.add_source(source).await.unwrap();

        let status = core.get_source_status("status-src").await.unwrap();
        assert_eq!(
            status,
            ComponentStatus::Stopped,
            "Newly added non-autostart source should be Stopped"
        );
    }

    #[tokio::test]
    async fn get_source_status_nonexistent_returns_error() {
        let core = build_and_start().await;
        let err = core.get_source_status("ghost").await.unwrap_err();

        assert!(
            matches!(err, DrasiError::ComponentNotFound { .. }),
            "Status of nonexistent source should return ComponentNotFound, got: {err:?}"
        );
    }

    // ========================================================================
    // start_source / stop_source lifecycle
    // ========================================================================

    #[tokio::test]
    async fn start_and_stop_source_lifecycle() {
        let core = build_and_start().await;

        // Subscribe BEFORE adding so we catch all events
        let mut event_rx = core.component_graph.read().await.subscribe();

        let source = TestMockSource::with_auto_start("lifecycle-src".to_string(), false).unwrap();
        core.add_source(source).await.unwrap();

        // Initially stopped
        let status = core.get_source_status("lifecycle-src").await.unwrap();
        assert_eq!(status, ComponentStatus::Stopped);

        // Start
        core.start_source("lifecycle-src").await.unwrap();
        wait_for_component_status(
            &mut event_rx,
            "lifecycle-src",
            ComponentStatus::Running,
            Duration::from_secs(5),
        )
        .await;

        let status = core.get_source_status("lifecycle-src").await.unwrap();
        assert_eq!(status, ComponentStatus::Running);

        // Stop
        core.stop_source("lifecycle-src").await.unwrap();
        wait_for_component_status(
            &mut event_rx,
            "lifecycle-src",
            ComponentStatus::Stopped,
            Duration::from_secs(5),
        )
        .await;

        let status = core.get_source_status("lifecycle-src").await.unwrap();
        assert_eq!(status, ComponentStatus::Stopped);
    }

    // ========================================================================
    // get_source_info
    // ========================================================================

    #[tokio::test]
    async fn get_source_info_returns_correct_fields() {
        let core = build_and_start().await;
        let source = TestMockSource::with_auto_start("info-src".to_string(), false).unwrap();

        core.add_source(source).await.unwrap();

        let info = core.get_source_info("info-src").await.unwrap();
        assert_eq!(info.id, "info-src");
        assert_eq!(info.source_type, "mock");
        assert_eq!(info.status, ComponentStatus::Stopped);
        assert!(info.error_message.is_none());
    }
}
