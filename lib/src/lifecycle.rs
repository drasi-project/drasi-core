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

use anyhow::Result;
use log::info;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::{ComponentStatus, EventReceivers};
use crate::config::RuntimeConfig;
use crate::queries::QueryManager;
use crate::reactions::ReactionManager;
use crate::sources::SourceManager;

/// Tracks which components were running before the server was stopped
#[derive(Default, Clone)]
pub(crate) struct ComponentsRunningState {
    pub sources: HashSet<String>,
    pub queries: HashSet<String>,
    pub reactions: HashSet<String>,
}

/// Manages the lifecycle orchestration for DrasiLib components
///
/// This module handles:
/// - Loading configuration and creating components
/// - Starting event processors for component events
/// - Starting components in dependency order (Sources → Queries → Reactions)
/// - Stopping components in reverse order (Reactions → Queries → Sources)
/// - Saving and restoring running component state across restarts
pub(crate) struct LifecycleManager {
    config: Arc<RuntimeConfig>,
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
    components_running_before_stop: Arc<RwLock<ComponentsRunningState>>,
    // Event receivers - taken and consumed during initialization
    event_receivers: Option<EventReceivers>,
}

impl LifecycleManager {
    /// Create a new lifecycle manager
    pub fn new(
        config: Arc<RuntimeConfig>,
        source_manager: Arc<SourceManager>,
        query_manager: Arc<QueryManager>,
        reaction_manager: Arc<ReactionManager>,
        components_running_before_stop: Arc<RwLock<ComponentsRunningState>>,
        event_receivers: Option<EventReceivers>,
    ) -> Self {
        Self {
            config,
            source_manager,
            query_manager,
            reaction_manager,
            components_running_before_stop,
            event_receivers,
        }
    }

    /// Load configuration from RuntimeConfig and create all components
    ///
    /// This method creates sources, queries, and reactions from the configuration
    /// without starting them. Components are not saved during this initialization phase.
    pub async fn load_configuration(&self) -> Result<()> {
        info!("Loading configuration");

        // Load sources
        for source_config in &self.config.sources {
            let config = source_config.clone();
            self.source_manager
                .add_source_without_save(config, false)
                .await?;
        }

        // Load queries
        for query_config in &self.config.queries {
            let config = query_config.clone();
            self.query_manager.add_query_without_save(config).await?;
        }

        // Load reactions
        for reaction_config in &self.config.reactions {
            let config = reaction_config.clone();
            self.reaction_manager
                .add_reaction_without_save(config)
                .await?;
        }

        info!("Configuration loaded successfully");
        Ok(())
    }

    /// Save the current state of running components
    ///
    /// This is called before stopping the server to track which components
    /// were running, so they can be automatically restarted on the next start().
    pub async fn save_running_components_state(&self) -> Result<()> {
        let mut state = self.components_running_before_stop.write().await;
        state.sources.clear();
        state.queries.clear();
        state.reactions.clear();

        // Save running sources
        for (name, _) in self.source_manager.list_sources().await {
            let status = self.source_manager.get_source_status(name.clone()).await?;
            if matches!(status, ComponentStatus::Running) {
                state.sources.insert(name);
            }
        }

        // Save running queries
        for (name, _) in self.query_manager.list_queries().await {
            let status = self.query_manager.get_query_status(name.clone()).await?;
            if matches!(status, ComponentStatus::Running) {
                state.queries.insert(name);
            }
        }

        // Save running reactions
        for (name, _) in self.reaction_manager.list_reactions().await {
            let status = self
                .reaction_manager
                .get_reaction_status(name.clone())
                .await?;
            if matches!(status, ComponentStatus::Running) {
                state.reactions.insert(name);
            }
        }

        info!(
            "Saved running components state: {} sources, {} queries, {} reactions",
            state.sources.len(),
            state.queries.len(),
            state.reactions.len()
        );
        Ok(())
    }

    /// Start event processors (one-time operation during initialization)
    ///
    /// This takes ownership of the event receivers and spawns background tasks
    /// to process component events. Can only be called once.
    pub async fn start_event_processors(&mut self) {
        if let Some(receivers) = self.event_receivers.take() {
            // Start component event processor
            let component_rx = receivers.component_event_rx;
            tokio::spawn(async move {
                let mut rx = component_rx;
                while let Some(event) = rx.recv().await {
                    info!(
                        "Component Event - {:?} {}: {:?} - {}",
                        event.component_type,
                        event.component_id,
                        event.status,
                        event.message.unwrap_or_default()
                    );
                }
            });

            // DataRouter no longer needed - queries subscribe directly to sources
            // control_signal_rx is no longer used
            drop(receivers.control_signal_rx);

            // SubscriptionRouter no longer needed - reactions subscribe directly to queries
        }
    }

    /// Start all auto-start components and previously running components
    ///
    /// Components are started in dependency order: Sources → Queries → Reactions
    /// After successful start, the saved running state is cleared.
    pub async fn start_components(
        &self,
        server_core: Arc<crate::server_core::DrasiLib>,
    ) -> Result<()> {
        info!("Starting all auto-start components in sequence: Sources → Queries → Reactions");

        let running_before = self.components_running_before_stop.read().await.clone();

        // Start sources first
        info!("Starting auto-start sources");
        for source_config in &self.config.sources {
            let id = &source_config.id;
            let should_start = source_config.auto_start || running_before.sources.contains(id);

            if should_start {
                let status = self.source_manager.get_source_status(id.to_string()).await;
                if matches!(status, Ok(ComponentStatus::Stopped)) {
                    info!(
                        "Starting source '{}' (auto_start={}, was_running={})",
                        id,
                        source_config.auto_start,
                        running_before.sources.contains(id)
                    );
                    self.source_manager.start_source(id.to_string()).await?;
                } else {
                    info!(
                        "Source '{}' already started or starting, status: {:?}",
                        id, status
                    );
                }
            } else {
                let status = self.source_manager.get_source_status(id.to_string()).await;
                info!(
                    "Source '{}' will not start (auto_start=false, was not running), status: {:?}",
                    id, status
                );
            }
        }
        info!("All required sources started successfully");

        // Start queries after sources
        info!("Starting auto-start queries");
        for query_config in &self.config.queries {
            let id = &query_config.id;
            let should_start = query_config.auto_start || running_before.queries.contains(id);

            if should_start {
                let status = self.query_manager.get_query_status(id.to_string()).await;
                if matches!(status, Ok(ComponentStatus::Stopped)) {
                    info!(
                        "Starting query '{}' (auto_start={}, was_running={})",
                        id,
                        query_config.auto_start,
                        running_before.queries.contains(id)
                    );
                    // Query will subscribe directly to sources when started
                    info!("Starting query '{}'", id);
                    self.query_manager.start_query(id.clone()).await?;
                    info!("Query '{}' started successfully", id);
                } else {
                    info!(
                        "Query '{}' already started or starting, status: {:?}",
                        id, status
                    );
                }
            } else {
                let status = self.query_manager.get_query_status(id.to_string()).await;
                info!(
                    "Query '{}' will not start (auto_start=false, was not running), status: {:?}",
                    id, status
                );
            }
        }
        info!("All required queries started successfully");

        // Start reactions after queries
        info!("Starting auto-start reactions");
        for reaction_config in &self.config.reactions {
            let id = &reaction_config.id;
            let should_start = reaction_config.auto_start || running_before.reactions.contains(id);

            if should_start {
                let status = self
                    .reaction_manager
                    .get_reaction_status(id.to_string())
                    .await;
                if matches!(status, Ok(ComponentStatus::Stopped)) {
                    info!(
                        "Starting reaction '{}' (auto_start={}, was_running={})",
                        id,
                        reaction_config.auto_start,
                        running_before.reactions.contains(id)
                    );
                    // Pass server core to reaction for direct query subscriptions
                    self.reaction_manager
                        .start_reaction(id.clone(), server_core.clone())
                        .await?;
                } else {
                    info!(
                        "Reaction '{}' already started or starting, status: {:?}",
                        id, status
                    );
                }
            } else {
                let status = self
                    .reaction_manager
                    .get_reaction_status(id.to_string())
                    .await;
                info!("Reaction '{}' will not start (auto_start=false, was not running), status: {:?}", id, status);
            }
        }
        info!("All required reactions started successfully");

        // Clear the saved state after successful start
        self.components_running_before_stop
            .write()
            .await
            .sources
            .clear();
        self.components_running_before_stop
            .write()
            .await
            .queries
            .clear();
        self.components_running_before_stop
            .write()
            .await
            .reactions
            .clear();

        info!("All required components started in sequence: Sources → Queries → Reactions");
        info!("[STARTUP-COMPLETE] DrasiLib.start() is now returning - all components and subscriptions should be active");
        Ok(())
    }

    /// Stop all running components
    ///
    /// Components are stopped in reverse dependency order: Reactions → Queries → Sources
    /// Errors during shutdown are logged but don't prevent other components from stopping.
    pub async fn stop_all_components(&self) -> Result<()> {
        use log::error;

        // Stop all reactions first (reverse order)
        info!("Stopping all reactions");
        let reaction_ids: Vec<String> = self
            .reaction_manager
            .list_reactions()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        for id in reaction_ids {
            let status = self.reaction_manager.get_reaction_status(id.clone()).await;
            if matches!(status, Ok(ComponentStatus::Running)) {
                if let Err(e) = self.reaction_manager.stop_reaction(id.clone()).await {
                    error!("Error stopping reaction {}: {}", id, e);
                }
                // Subscriptions are managed by the reaction itself - no router cleanup needed
            }
        }

        // Stop all queries
        info!("Stopping all queries");
        let query_ids: Vec<String> = self
            .query_manager
            .list_queries()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        for id in query_ids {
            let status = self.query_manager.get_query_status(id.clone()).await;
            if matches!(status, Ok(ComponentStatus::Running)) {
                if let Err(e) = self.query_manager.stop_query(id.clone()).await {
                    error!("Error stopping query {}: {}", id, e);
                }
                // Query unsubscribes from sources automatically when stopped
            }
        }

        // Stop all sources
        info!("Stopping all sources");
        let source_ids: Vec<String> = self
            .source_manager
            .list_sources()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        for id in source_ids {
            let status = self.source_manager.get_source_status(id.clone()).await;
            if matches!(status, Ok(ComponentStatus::Running)) {
                if let Err(e) = self.source_manager.stop_source(id.clone()).await {
                    error!("Error stopping source {}: {}", id, e);
                }
            }
        }

        info!("All components stopped");
        Ok(())
    }
}
