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

use anyhow::{anyhow, Result};
use log::{error, info, warn};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::{ComponentStatus, *};
use crate::config::RuntimeConfig;
use crate::queries::QueryManager;
use crate::reactions::ReactionManager;
use crate::routers::{BootstrapRouter, DataRouter, SubscriptionRouter};
use crate::sources::SourceManager;

/// Core Drasi server functionality without web API
pub struct DrasiServerCore {
    config: Arc<RuntimeConfig>,
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
    subscription_router: Arc<SubscriptionRouter>,
    data_router: Arc<DataRouter>,
    bootstrap_router: Arc<BootstrapRouter>,
    event_receivers: Option<EventReceivers>,
    running: Arc<RwLock<bool>>,
    initialized: Arc<RwLock<bool>>,
    // Track components that were running before server stop
    components_running_before_stop: Arc<RwLock<ComponentsRunningState>>,
}

#[derive(Default, Clone)]
struct ComponentsRunningState {
    sources: HashSet<String>,
    queries: HashSet<String>,
    reactions: HashSet<String>,
}

impl DrasiServerCore {
    /// Create a new DrasiServerCore with the given configuration
    pub fn new(config: Arc<RuntimeConfig>) -> Self {
        let (channels, receivers) = EventChannels::new();

        let source_manager = Arc::new(SourceManager::new(
            channels.source_event_tx.clone(),
            channels.component_event_tx.clone(),
        ));

        let query_manager = Arc::new(QueryManager::new(
            channels.query_result_tx.clone(),
            channels.component_event_tx.clone(),
            channels.bootstrap_request_tx.clone(),
        ));

        let reaction_manager = Arc::new(ReactionManager::new(channels.component_event_tx.clone()));

        let subscription_router = Arc::new(SubscriptionRouter::new());
        let data_router = Arc::new(DataRouter::new());
        let bootstrap_router = Arc::new(BootstrapRouter::new());

        Self {
            config,
            source_manager,
            query_manager,
            reaction_manager,
            subscription_router,
            data_router,
            bootstrap_router,
            event_receivers: Some(receivers),
            running: Arc::new(RwLock::new(false)),
            initialized: Arc::new(RwLock::new(false)),
            components_running_before_stop: Arc::new(
                RwLock::new(ComponentsRunningState::default()),
            ),
        }
    }

    /// Initialize the server components (one-time setup)
    pub async fn initialize(&mut self) -> Result<()> {
        let already_initialized = *self.initialized.read().await;
        if already_initialized {
            info!("Server already initialized, skipping initialization");
            return Ok(());
        }

        info!("Initializing Drasi Server Core");

        // Load configuration
        self.load_configuration().await?;

        // Start event processors (one-time)
        self.start_event_processors().await;

        *self.initialized.write().await = true;
        info!("Drasi Server Core initialized successfully");
        Ok(())
    }

    /// Start the server and all auto-start components
    pub async fn start(&self) -> Result<()> {
        let mut running = self.running.write().await;
        if *running {
            warn!("Server is already running");
            return Err(anyhow!("Server is already running"));
        }

        info!("Starting Drasi Server Core");

        // Ensure initialized
        if !*self.initialized.read().await {
            return Err(anyhow!("Server must be initialized before starting"));
        }

        // Start all configured components
        self.start_components().await?;

        *running = true;
        info!("Drasi Server Core started successfully");

        Ok(())
    }

    /// Stop the server and all running components
    pub async fn stop(&self) -> Result<()> {
        let mut running = self.running.write().await;
        if !*running {
            warn!("Server is already stopped");
            return Err(anyhow!("Server is already stopped"));
        }

        info!("Stopping Drasi Server Core");

        // Save running component state before stopping
        self.save_running_components_state().await?;

        // Stop all components
        self.stop_all_components().await?;

        *running = false;
        info!("Drasi Server Core stopped successfully");

        Ok(())
    }

    /// Get references to managers for programmatic access
    pub fn source_manager(&self) -> &Arc<SourceManager> {
        &self.source_manager
    }

    pub fn query_manager(&self) -> &Arc<QueryManager> {
        &self.query_manager
    }

    pub fn reaction_manager(&self) -> &Arc<ReactionManager> {
        &self.reaction_manager
    }

    pub fn data_router(&self) -> &Arc<DataRouter> {
        &self.data_router
    }

    pub fn subscription_router(&self) -> &Arc<SubscriptionRouter> {
        &self.subscription_router
    }

    pub fn bootstrap_router(&self) -> &Arc<BootstrapRouter> {
        &self.bootstrap_router
    }

    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    async fn load_configuration(&self) -> Result<()> {
        info!("Loading configuration");

        // Load sources
        for source_config in &self.config.sources {
            let config = source_config.clone();
            self.create_source_with_options(config, false).await?;
        }

        // Load queries
        for query_config in &self.config.queries {
            let config = query_config.clone();
            self.create_query_with_options(config, false).await?;
        }

        // Load reactions
        for reaction_config in &self.config.reactions {
            let config = reaction_config.clone();
            self.create_reaction_with_options(config, false).await?;
        }

        info!("Configuration loaded successfully");
        Ok(())
    }

    async fn save_running_components_state(&self) -> Result<()> {
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

    async fn start_event_processors(&mut self) {
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

            // Start data router
            let source_event_rx = receivers.source_event_rx;
            let data_router = self.data_router.clone();
            tokio::spawn(async move {
                data_router.start(source_event_rx).await;
            });

            // Start subscription router
            let query_rx = receivers.query_result_rx;
            let router = self.subscription_router.clone();
            tokio::spawn(async move {
                router.start(query_rx).await;
            });

            // Start bootstrap router
            let bootstrap_request_rx = receivers.bootstrap_request_rx;
            let bootstrap_router = self.bootstrap_router.clone();
            tokio::spawn(async move {
                info!("Starting bootstrap router");
                bootstrap_router.start(bootstrap_request_rx).await;
            });
        }
    }

    async fn create_source_with_options(
        &self,
        config: crate::config::SourceConfig,
        allow_auto_start: bool,
    ) -> Result<()> {
        let source_id = config.id.clone();
        let bootstrap_provider_config = config.bootstrap_provider.clone();
        let source_config_arc = std::sync::Arc::new(config.clone());

        // Add the source (without saving during initialization)
        self.source_manager
            .add_source_without_save(config, allow_auto_start)
            .await?;

        // Register bootstrap provider with bootstrap router
        let source_event_tx = self.source_manager.get_source_event_sender();
        if let Err(e) = self
            .bootstrap_router
            .register_provider(
                self.config.server.id.clone(),
                source_config_arc,
                bootstrap_provider_config,
                source_event_tx,
            )
            .await
        {
            error!(
                "Failed to register bootstrap provider for source '{}': {}",
                source_id, e
            );
        } else {
            info!("Registered bootstrap provider for source '{}'", source_id);
        }

        Ok(())
    }

    async fn create_query_with_options(
        &self,
        config: crate::config::QueryConfig,
        allow_auto_start: bool,
    ) -> Result<()> {
        let query_id = config.id.clone();
        let should_auto_start = config.auto_start;
        let sources = config.sources.clone();

        // Add the query (without saving during initialization)
        self.query_manager.add_query_without_save(config).await?;

        // Register with bootstrap router
        let bootstrap_senders = self.query_manager.get_bootstrap_response_senders().await;
        if let Some(sender) = bootstrap_senders.get(&query_id) {
            self.bootstrap_router
                .register_query(query_id.clone(), sender.clone())
                .await;
            info!("Registered query '{}' with bootstrap router", query_id);
        }

        // Start if auto-start is enabled and allowed
        if should_auto_start && allow_auto_start {
            // Add query subscription and get receiver
            let rx = self
                .data_router
                .add_query_subscription(query_id.clone(), sources)
                .await;

            self.query_manager.start_query(query_id.clone(), rx).await?;
        }

        Ok(())
    }

    async fn create_reaction_with_options(
        &self,
        config: crate::config::ReactionConfig,
        allow_auto_start: bool,
    ) -> Result<()> {
        let reaction_id = config.id.clone();
        let should_auto_start = config.auto_start;
        let queries = config.queries.clone();

        // Add the reaction (without saving during initialization)
        self.reaction_manager
            .add_reaction_without_save(config)
            .await?;

        // Start if auto-start is enabled and allowed
        if should_auto_start && allow_auto_start {
            // Add reaction subscription and get receiver
            let rx = self
                .subscription_router
                .add_reaction_subscription(reaction_id.clone(), queries)
                .await;

            self.reaction_manager
                .start_reaction(reaction_id.clone(), rx)
                .await?;
        }

        Ok(())
    }

    async fn start_components(&self) -> Result<()> {
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
                    // Get sources for this query
                    let sources = query_config.sources.clone();
                    info!(
                        "Creating subscription for query '{}' to sources: {:?}",
                        id, sources
                    );
                    let rx = self
                        .data_router
                        .add_query_subscription(id.clone(), sources)
                        .await;
                    info!("Subscription created, starting query '{}'", id);
                    self.query_manager.start_query(id.clone(), rx).await?;
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
                    // Get queries for this reaction
                    let queries = reaction_config.queries.clone();
                    let rx = self
                        .subscription_router
                        .add_reaction_subscription(id.clone(), queries)
                        .await;
                    self.reaction_manager.start_reaction(id.clone(), rx).await?;
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
        info!("[STARTUP-COMPLETE] DrasiServerCore.start() is now returning - all components and subscriptions should be active");
        Ok(())
    }

    async fn stop_all_components(&self) -> Result<()> {
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
