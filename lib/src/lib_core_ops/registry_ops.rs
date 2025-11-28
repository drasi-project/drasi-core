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

//! Registry operations for DrasiLib
//!
//! This module provides factory registry operations for creating sources and reactions
//! from configuration using registered factories.

use crate::config::{ReactionConfig, SourceConfig};
use crate::error::{DrasiError, Result};
use crate::plugin_core::{ReactionRegistry, SourceRegistry};
use crate::lib_core::DrasiLib;

impl DrasiLib {
    /// Set the source registry for factory-based source creation.
    ///
    /// This is called by the builder when a registry is provided.
    /// After setting, you can use `create_source()` to create sources from config.
    pub async fn set_source_registry(&self, registry: SourceRegistry) {
        *self.source_registry.write().await = Some(registry);
    }

    /// Set the reaction registry for factory-based reaction creation.
    ///
    /// This is called by the builder when a registry is provided.
    /// After setting, you can use `create_reaction()` to create reactions from config.
    pub async fn set_reaction_registry(&self, registry: ReactionRegistry) {
        *self.reaction_registry.write().await = Some(registry);
    }

    /// Create a source from generic configuration.
    ///
    /// Uses the registered source factory to create a source instance from `SourceConfig`.
    /// The source is added to the server and optionally auto-started.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * No source registry has been set
    /// * No factory is registered for the source type
    /// * The factory fails to create the source
    /// * Adding the source to the server fails
    pub async fn create_source(&self, config: SourceConfig) -> Result<()> {
        // Get the registry
        let registry_guard = self.source_registry.read().await;
        let registry = registry_guard
            .as_ref()
            .ok_or_else(|| DrasiError::provisioning("No source registry configured".to_string()))?;

        // Create the source instance using the factory
        let source = registry.create(&config).map_err(|e| {
            DrasiError::provisioning(format!(
                "Failed to create source '{}' of type '{}': {}",
                config.id, config.source_type, e
            ))
        })?;

        let source_id = config.id.clone();
        let auto_start = config.auto_start;

        // Store the config for later retrieval
        self.source_configs
            .write()
            .await
            .insert(source_id.clone(), config);

        drop(registry_guard);

        // Add the source to the manager
        self.source_manager.add_source(source).await.map_err(|e| {
            DrasiError::provisioning(format!("Failed to add source '{}': {}", source_id, e))
        })?;

        // Auto-start if configured and server is running
        if auto_start && *self.running.read().await {
            self.source_manager
                .start_source(source_id.clone())
                .await
                .map_err(|e| {
                    DrasiError::component_error(format!(
                        "Failed to start source '{}': {}",
                        source_id, e
                    ))
                })?;
        }

        Ok(())
    }

    /// Create a reaction from generic configuration.
    ///
    /// Uses the registered reaction factory to create a reaction instance from `ReactionConfig`.
    /// The reaction is added to the server and optionally auto-started.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * No reaction registry has been set
    /// * No factory is registered for the reaction type
    /// * The factory fails to create the reaction
    /// * Adding the reaction to the server fails
    pub async fn create_reaction(&self, config: ReactionConfig) -> Result<()> {
        // Get the registry
        let registry_guard = self.reaction_registry.read().await;
        let registry = registry_guard.as_ref().ok_or_else(|| {
            DrasiError::provisioning("No reaction registry configured".to_string())
        })?;

        // Create the reaction instance using the factory
        let reaction = registry.create(&config).map_err(|e| {
            DrasiError::provisioning(format!(
                "Failed to create reaction '{}' of type '{}': {}",
                config.id, config.reaction_type, e
            ))
        })?;

        let reaction_id = config.id.clone();
        let auto_start = config.auto_start;

        // Store the config for later retrieval
        self.reaction_configs
            .write()
            .await
            .insert(reaction_id.clone(), config);

        drop(registry_guard);

        // Add the reaction to the manager
        self.reaction_manager
            .add_reaction(reaction)
            .await
            .map_err(|e| {
                DrasiError::provisioning(format!("Failed to add reaction '{}': {}", reaction_id, e))
            })?;

        // Auto-start if configured and server is running
        if auto_start && *self.running.read().await {
            self.reaction_manager
                .start_reaction(reaction_id.clone(), self.as_arc())
                .await
                .map_err(|e| {
                    DrasiError::component_error(format!(
                        "Failed to start reaction '{}': {}",
                        reaction_id, e
                    ))
                })?;
        }

        Ok(())
    }
}
