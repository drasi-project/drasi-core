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
