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

use crate::channels::ComponentStatus;
use crate::component_ops::map_state_error;
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
        self.state_guard.require_initialized().await?;

        // Capture auto_start and id before transferring ownership
        let should_auto_start = reaction.auto_start();
        let reaction_id = reaction.id().to_string();

        self.reaction_manager
            .add_reaction(reaction)
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to add reaction: {e}")))?;

        // If server is running and reaction wants auto-start, start it
        if self.is_running().await && should_auto_start {
            self.reaction_manager
                .start_reaction(reaction_id)
                .await
                .map_err(|e| DrasiError::provisioning(format!("Failed to start reaction: {e}")))?;
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
    /// core.remove_reaction("old-reaction").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_reaction(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized().await?;

        // Stop if running
        let status = self
            .reaction_manager
            .get_reaction_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("reaction", id))?;

        if matches!(status, ComponentStatus::Running) {
            self.reaction_manager
                .stop_reaction(id.to_string())
                .await
                .map_err(|e| DrasiError::provisioning(format!("Failed to stop reaction: {e}")))?;
        }

        // Delete the reaction
        self.reaction_manager
            .delete_reaction(id.to_string())
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to delete reaction: {e}")))?;

        Ok(())
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
        self.state_guard.require_initialized().await?;

        // Start the reaction (QueryProvider was injected when reaction was added)
        map_state_error(
            self.reaction_manager.start_reaction(id.to_string()).await,
            "reaction",
            id,
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
        self.state_guard.require_initialized().await?;

        // Stop the reaction (subscriptions managed by reaction itself)
        map_state_error(
            self.reaction_manager.stop_reaction(id.to_string()).await,
            "reaction",
            id,
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
}
