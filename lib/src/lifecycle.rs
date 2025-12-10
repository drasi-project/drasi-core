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
use std::sync::Arc;

use crate::channels::{ComponentStatus, EventReceivers};
use crate::config::RuntimeConfig;
use crate::queries::QueryManager;
use crate::reactions::ReactionManager;
use crate::sources::SourceManager;

/// Manages the lifecycle orchestration for DrasiLib components
///
/// This module handles:
/// - Loading configuration and creating components
/// - Starting event processors for component events
/// - Starting components in dependency order (Sources → Queries → Reactions)
/// - Stopping components in reverse order (Reactions → Queries → Sources)
pub(crate) struct LifecycleManager {
    config: Arc<RuntimeConfig>,
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
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
        event_receivers: Option<EventReceivers>,
    ) -> Self {
        Self {
            config,
            source_manager,
            query_manager,
            reaction_manager,
            event_receivers,
        }
    }

    /// Load configuration from RuntimeConfig and create all components
    ///
    /// This method creates queries from configuration without starting them.
    /// Note: Sources and reactions must be added as instances via the builder or API.
    pub async fn load_configuration(&self) -> Result<()> {
        info!("Loading configuration");

        // Load queries (only queries use config-based creation)
        for query_config in &self.config.queries {
            let config = query_config.clone();
            self.query_manager.add_query_without_save(config).await?;
        }

        info!("Configuration loaded successfully");
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

    /// Start all components with auto_start enabled
    ///
    /// Components are started in dependency order: Sources → Queries → Reactions
    /// Only components with auto_start=true will be started.
    pub async fn start_components(&self) -> Result<()> {
        info!("Starting all auto-start components in sequence: Sources → Queries → Reactions");

        // Start sources first (SourceManager.start_all() checks auto_start internally)
        info!("Starting auto-start sources");
        self.source_manager.start_all().await?;
        info!("All auto-start sources started successfully");

        // Start queries after sources
        info!("Starting auto-start queries");
        for query_config in &self.config.queries {
            let id = &query_config.id;

            if query_config.auto_start {
                let status = self.query_manager.get_query_status(id.to_string()).await;
                if matches!(status, Ok(ComponentStatus::Stopped)) {
                    info!(
                        "Starting query '{}' (auto_start={})",
                        id, query_config.auto_start
                    );
                    self.query_manager.start_query(id.clone()).await?;
                    info!("Query '{id}' started successfully");
                } else {
                    info!("Query '{id}' already started or starting, status: {status:?}");
                }
            } else {
                let status = self.query_manager.get_query_status(id.to_string()).await;
                info!("Skipping query '{id}' (auto_start=false), status: {status:?}");
            }
        }
        info!("All auto-start queries started successfully");

        // Start reactions after queries
        // ReactionManager.start_all() handles auto_start logic internally
        info!("Starting auto-start reactions");
        self.reaction_manager.start_all().await?;
        info!("All auto-start reactions started successfully");

        info!("All auto-start components started in sequence: Sources → Queries → Reactions");
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
                    error!("Error stopping reaction {id}: {e}");
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
                    error!("Error stopping query {id}: {e}");
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
                    error!("Error stopping source {id}: {e}");
                }
            }
        }

        info!("All components stopped");
        Ok(())
    }
}
