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
        self.state_guard.require_initialized().await?;

        // Capture auto_start and id before transferring ownership
        let should_auto_start = source.auto_start();
        let source_id = source.id().to_string();

        self.source_manager
            .add_source(source)
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to add source: {e}")))?;

        // If server is running and source wants auto-start, start it
        if self.is_running().await && should_auto_start {
            self.source_manager
                .start_source(source_id)
                .await
                .map_err(|e| DrasiError::provisioning(format!("Failed to start source: {e}")))?;
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
        self.state_guard.require_initialized().await?;

        // Stop if running
        let status = self
            .source_manager
            .get_source_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("source", id))?;

        if matches!(status, ComponentStatus::Running) {
            self.source_manager
                .stop_source(id.to_string())
                .await
                .map_err(|e| DrasiError::provisioning(format!("Failed to stop source: {e}")))?;
        }

        // Delete the source
        self.source_manager
            .delete_source(id.to_string(), cleanup)
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to delete source: {e}")))?;

        Ok(())
    }

    /// Update a source by replacing it with a new instance.
    ///
    /// This stops the old source, replaces it with the new one, and restarts
    /// if it was running before. Log and event history are preserved.
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
        self.state_guard.require_initialized().await?;

        self.source_manager
            .update_source(id.to_string(), new_source)
            .await
            .map_err(|e| {
                match e.downcast::<DrasiError>() {
                    Ok(drasi_err) => drasi_err,
                    Err(e) => DrasiError::provisioning(format!("Failed to update source: {e}")),
                }
            })?;

        Ok(())
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
        self.state_guard.require_initialized().await?;

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
        self.state_guard.require_initialized().await?;

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
