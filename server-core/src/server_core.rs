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
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::api::{DrasiError, HandleRegistry};
use crate::channels::{ComponentStatus, *};
use crate::config::{DrasiServerCoreConfig, RuntimeConfig};
use crate::queries::QueryManager;
use crate::reactions::ApplicationReactionHandle;
use crate::reactions::ReactionManager;
use crate::sources::{ApplicationSourceHandle, SourceManager};

/// Core Drasi Server functionality without web API
pub struct DrasiServerCore {
    config: Arc<RuntimeConfig>,
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
    #[allow(dead_code)]
    event_receivers: Option<EventReceivers>,
    running: Arc<RwLock<bool>>,
    initialized: Arc<RwLock<bool>>,
    // Track components that were running before server stop
    components_running_before_stop: Arc<RwLock<ComponentsRunningState>>,
    // Handle registry for application sources and reactions
    handle_registry: HandleRegistry,
}

impl Clone for DrasiServerCore {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            source_manager: Arc::clone(&self.source_manager),
            query_manager: Arc::clone(&self.query_manager),
            reaction_manager: Arc::clone(&self.reaction_manager),
            event_receivers: None, // Don't clone receivers - they're not used after initialization
            running: Arc::clone(&self.running),
            initialized: Arc::clone(&self.initialized),
            components_running_before_stop: Arc::clone(&self.components_running_before_stop),
            handle_registry: self.handle_registry.clone(),
        }
    }
}

#[derive(Default, Clone)]
struct ComponentsRunningState {
    sources: HashSet<String>,
    queries: HashSet<String>,
    reactions: HashSet<String>,
}

impl DrasiServerCore {
    /// Internal constructor - creates uninitialized server
    /// Use `builder()`, `from_config_file()`, or `from_config_str()` instead
    pub(crate) fn new(config: Arc<RuntimeConfig>) -> Self {
        let (channels, receivers) = EventChannels::new();

        let source_manager = Arc::new(SourceManager::new(channels.component_event_tx.clone()));

        let query_manager = Arc::new(QueryManager::new(
            channels.component_event_tx.clone(),
            source_manager.clone(),
        ));

        let reaction_manager = Arc::new(ReactionManager::new(channels.component_event_tx.clone()));

        Self {
            config,
            source_manager,
            query_manager,
            reaction_manager,
            event_receivers: Some(receivers),
            running: Arc::new(RwLock::new(false)),
            initialized: Arc::new(RwLock::new(false)),
            components_running_before_stop: Arc::new(
                RwLock::new(ComponentsRunningState::default()),
            ),
            handle_registry: HandleRegistry::new(),
        }
    }

    /// Internal initialization - performs one-time setup
    /// This is called internally by builder and config loaders
    pub(crate) async fn initialize(&mut self) -> Result<()> {
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

    // ============================================================================
    // New Public API - Handle Access
    // ============================================================================

    /// Get a handle to an application source by ID
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let handle = core.source_handle("my-source")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn source_handle(&self, id: &str) -> crate::api::Result<ApplicationSourceHandle> {
        // Try to get from handle registry first
        if let Ok(handle) = futures::executor::block_on(self.handle_registry.get_source_handle(id))
        {
            return Ok(handle);
        }

        // If not in registry, try to get from source manager
        // This handles the case where the source exists but handle isn't registered yet
        futures::executor::block_on(async {
            if let Some(handle) = self.source_manager.get_application_handle(id).await {
                // Register it for future use
                self.handle_registry
                    .register_source_handle(id.to_string(), handle.clone())
                    .await;
                Ok(handle)
            } else {
                Err(DrasiError::component_not_found("source", id))
            }
        })
    }

    /// Get a handle to an application reaction by ID
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let handle = core.reaction_handle("my-reaction")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn query_manager(&self) -> &QueryManager {
        &self.query_manager
    }

    /// Get a handle to an application reaction for programmatic interaction
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let handle = core.reaction_handle("my-app-reaction")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn reaction_handle(&self, id: &str) -> crate::api::Result<ApplicationReactionHandle> {
        // Try to get from handle registry first
        if let Ok(handle) =
            futures::executor::block_on(self.handle_registry.get_reaction_handle(id))
        {
            return Ok(handle);
        }

        // If not in registry, try to get from reaction manager
        futures::executor::block_on(async {
            if let Some(handle) = self.reaction_manager.get_application_handle(id).await {
                // Register it for future use
                self.handle_registry
                    .register_reaction_handle(id.to_string(), handle.clone())
                    .await;
                Ok(handle)
            } else {
                Err(DrasiError::component_not_found("reaction", id))
            }
        })
    }

    // ============================================================================
    // Dynamic Runtime Configuration
    // ============================================================================

    /// Create a source in a running server
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source};
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.create_source(
    ///     Source::mock("new-source")
    ///         .auto_start(true)
    ///         .build()
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn create_source(
        &self,
        source: crate::config::SourceConfig,
    ) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot add source to uninitialized server",
            ));
        }

        // Create source with auto-start enabled if requested
        self.create_source_with_options(source, true)
            .await
            .map_err(|e| DrasiError::initialization(format!("Failed to add source: {}", e)))?;

        Ok(())
    }

    /// Create a query in a running server
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Query};
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.create_query(
    ///     Query::cypher("new-query")
    ///         .query("MATCH (n) RETURN n")
    ///         .from_source("source1")
    ///         .auto_start(true)
    ///         .build()
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn create_query(&self, query: crate::config::QueryConfig) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot add query to uninitialized server",
            ));
        }

        // Create query with auto-start enabled if requested
        self.create_query_with_options(query, true)
            .await
            .map_err(|e| DrasiError::initialization(format!("Failed to add query: {}", e)))?;

        Ok(())
    }

    /// Create a reaction in a running server
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Reaction};
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.create_reaction(
    ///     Reaction::log("new-reaction")
    ///         .subscribe_to("query1")
    ///         .auto_start(true)
    ///         .build()
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn create_reaction(
        &self,
        reaction: crate::config::ReactionConfig,
    ) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot add reaction to uninitialized server",
            ));
        }

        // Create reaction with auto-start enabled if requested
        self.create_reaction_with_options(reaction, true)
            .await
            .map_err(|e| DrasiError::initialization(format!("Failed to add reaction: {}", e)))?;

        Ok(())
    }

    /// Remove a source from a running server
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_source("old-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_source(&self, id: &str) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot remove source from uninitialized server",
            ));
        }

        // Stop if running
        let status = self
            .source_manager
            .get_source_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("source", id))?;

        if matches!(status, crate::channels::ComponentStatus::Running) {
            self.source_manager
                .stop_source(id.to_string())
                .await
                .map_err(|e| DrasiError::initialization(format!("Failed to stop source: {}", e)))?;
        }

        // Delete the source
        self.source_manager
            .delete_source(id.to_string())
            .await
            .map_err(|e| DrasiError::initialization(format!("Failed to delete source: {}", e)))?;

        Ok(())
    }

    /// Remove a query from a running server
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_query("old-query").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_query(&self, id: &str) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot remove query from uninitialized server",
            ));
        }

        // Stop if running
        let status = self
            .query_manager
            .get_query_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("query", id))?;

        if matches!(status, crate::channels::ComponentStatus::Running) {
            self.query_manager
                .stop_query(id.to_string())
                .await
                .map_err(|e| DrasiError::initialization(format!("Failed to stop query: {}", e)))?;
        }

        // Delete the query
        self.query_manager
            .delete_query(id.to_string())
            .await
            .map_err(|e| DrasiError::initialization(format!("Failed to delete query: {}", e)))?;

        Ok(())
    }

    /// Remove a reaction from a running server
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_reaction("old-reaction").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_reaction(&self, id: &str) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot remove reaction from uninitialized server",
            ));
        }

        // Stop if running
        let status = self
            .reaction_manager
            .get_reaction_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("reaction", id))?;

        if matches!(status, crate::channels::ComponentStatus::Running) {
            self.reaction_manager
                .stop_reaction(id.to_string())
                .await
                .map_err(|e| {
                    DrasiError::initialization(format!("Failed to stop reaction: {}", e))
                })?;
        }

        // Delete the reaction
        self.reaction_manager
            .delete_reaction(id.to_string())
            .await
            .map_err(|e| DrasiError::initialization(format!("Failed to delete reaction: {}", e)))?;

        Ok(())
    }

    // ============================================================================
    // Component Listing and Inspection
    // ============================================================================

    /// Start a stopped source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.start_source("my-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_source(&self, id: &str) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot start source on uninitialized server",
            ));
        }

        self.source_manager
            .start_source(id.to_string())
            .await
            .map_err(|e| {
                if e.to_string().contains("not found") {
                    DrasiError::component_not_found("source", id)
                } else {
                    DrasiError::invalid_state(e.to_string())
                }
            })
    }

    /// Stop a running source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_source("my-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_source(&self, id: &str) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot stop source on uninitialized server",
            ));
        }

        self.source_manager
            .stop_source(id.to_string())
            .await
            .map_err(|e| {
                if e.to_string().contains("not found") {
                    DrasiError::component_not_found("source", id)
                } else {
                    DrasiError::invalid_state(e.to_string())
                }
            })
    }

    /// Start a stopped query
    ///
    /// This will create the necessary subscriptions to source data streams.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.start_query("my-query").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_query(&self, id: &str) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot start query on uninitialized server",
            ));
        }

        // Get the query config to determine which sources it needs
        // Verify query exists
        let _config = self
            .query_manager
            .get_query_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("query", id))?;

        // Query will subscribe directly to sources when started
        self.query_manager
            .start_query(id.to_string())
            .await
            .map_err(|e| {
                if e.to_string().contains("not found") {
                    DrasiError::component_not_found("query", id)
                } else {
                    DrasiError::invalid_state(e.to_string())
                }
            })
    }

    /// Stop a running query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_query("my-query").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_query(&self, id: &str) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot stop query on uninitialized server",
            ));
        }

        // Stop the query first
        self.query_manager
            .stop_query(id.to_string())
            .await
            .map_err(|e| {
                if e.to_string().contains("not found") {
                    DrasiError::component_not_found("query", id)
                } else {
                    DrasiError::invalid_state(e.to_string())
                }
            })?;

        // Query unsubscribes from sources automatically when stopped

        Ok(())
    }

    /// Start a stopped reaction
    ///
    /// This will create the necessary subscriptions to query result streams.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.start_reaction("my-reaction").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_reaction(&self, id: &str) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot start reaction on uninitialized server",
            ));
        }

        // Get the reaction config to determine which queries it subscribes to
        let _config = self
            .reaction_manager
            .get_reaction_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("reaction", id))?;

        // Start the reaction with server core reference for query subscriptions
        self.reaction_manager
            .start_reaction(id.to_string(), Arc::new(self.clone()))
            .await
            .map_err(|e| {
                if e.to_string().contains("not found") {
                    DrasiError::component_not_found("reaction", id)
                } else {
                    DrasiError::invalid_state(e.to_string())
                }
            })
    }

    /// Stop a running reaction
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_reaction("my-reaction").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_reaction(&self, id: &str) -> crate::api::Result<()> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot stop reaction on uninitialized server",
            ));
        }

        // Stop the reaction - subscriptions are managed by the reaction itself
        self.reaction_manager
            .stop_reaction(id.to_string())
            .await
            .map_err(|e| {
                if e.to_string().contains("not found") {
                    DrasiError::component_not_found("reaction", id)
                } else {
                    DrasiError::invalid_state(e.to_string())
                }
            })?;

        Ok(())
    }

    /// List all sources with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let sources = core.list_sources().await?;
    /// for (id, status) in sources {
    ///     println!("Source {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_sources(
        &self,
    ) -> crate::api::Result<Vec<(String, crate::channels::ComponentStatus)>> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot list sources from uninitialized server",
            ));
        }
        Ok(self.source_manager.list_sources().await)
    }

    /// Get detailed information about a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let source_info = core.get_source_info("my-source").await?;
    /// println!("Source type: {}", source_info.source_type);
    /// println!("Status: {:?}", source_info.status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_info(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::SourceRuntime> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot get source info from uninitialized server",
            ));
        }
        self.source_manager
            .get_source(id.to_string())
            .await
            .map_err(|_e| DrasiError::component_not_found("source", id))
    }

    /// Get the current status of a specific source
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_source_status("my-source").await?;
    /// println!("Source status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_status(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::channels::ComponentStatus> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot get source status from uninitialized server",
            ));
        }
        self.source_manager
            .get_source_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("source", id))
    }

    /// Get the full configuration for a specific source
    ///
    /// This returns the complete source configuration including auto_start and bootstrap_provider,
    /// unlike `get_source_info()` which only returns runtime information.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_source_config("my-source").await?;
    /// println!("Auto-start: {}", config.auto_start);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_source_config(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::SourceConfig> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot get source config from uninitialized server",
            ));
        }
        self.source_manager
            .get_source_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("source", id))
    }

    /// List all queries with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let queries = core.list_queries().await?;
    /// for (id, status) in queries {
    ///     println!("Query {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_queries(
        &self,
    ) -> crate::api::Result<Vec<(String, crate::channels::ComponentStatus)>> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot list queries from uninitialized server",
            ));
        }
        Ok(self.query_manager.list_queries().await)
    }

    /// Get detailed information about a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let query_info = core.get_query_info("my-query").await?;
    /// println!("Query: {}", query_info.query);
    /// println!("Status: {:?}", query_info.status);
    /// println!("Sources: {:?}", query_info.sources);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_info(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::QueryRuntime> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot get query info from uninitialized server",
            ));
        }
        self.query_manager
            .get_query(id.to_string())
            .await
            .map_err(|_e| DrasiError::component_not_found("query", id))
    }

    /// Get the current status of a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_query_status("my-query").await?;
    /// println!("Query status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_status(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::channels::ComponentStatus> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot get query status from uninitialized server",
            ));
        }
        self.query_manager
            .get_query_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("query", id))
    }

    /// Get the current result set for a running query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let results = core.get_query_results("my-query").await?;
    /// println!("Current results: {} items", results.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_results(&self, id: &str) -> crate::api::Result<Vec<serde_json::Value>> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot get query results from uninitialized server",
            ));
        }
        self.query_manager.get_query_results(id).await.map_err(|e| {
            if e.to_string().contains("not found") {
                DrasiError::component_not_found("query", id)
            } else if e.to_string().contains("not running") {
                DrasiError::invalid_state(format!("Query '{}' is not running", id))
            } else {
                DrasiError::initialization(e.to_string())
            }
        })
    }

    /// Get the full configuration for a specific query
    ///
    /// This returns the complete query configuration including all fields like auto_start and joins,
    /// unlike `get_query_info()` which only returns runtime information.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_query_config("my-query").await?;
    /// println!("Auto-start: {}", config.auto_start);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_config(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::QueryConfig> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot get query config from uninitialized server",
            ));
        }
        self.query_manager
            .get_query_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("query", id))
    }

    /// List all reactions with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let reactions = core.list_reactions().await?;
    /// for (id, status) in reactions {
    ///     println!("Reaction {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_reactions(
        &self,
    ) -> crate::api::Result<Vec<(String, crate::channels::ComponentStatus)>> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot list reactions from uninitialized server",
            ));
        }
        Ok(self.reaction_manager.list_reactions().await)
    }

    /// Get detailed information about a specific reaction
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let reaction_info = core.get_reaction_info("my-reaction").await?;
    /// println!("Reaction type: {}", reaction_info.reaction_type);
    /// println!("Status: {:?}", reaction_info.status);
    /// println!("Queries: {:?}", reaction_info.queries);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_info(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::ReactionRuntime> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot get reaction info from uninitialized server",
            ));
        }
        self.reaction_manager
            .get_reaction(id.to_string())
            .await
            .map_err(|_e| DrasiError::component_not_found("reaction", id))
    }

    /// Get the current status of a specific reaction
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_reaction_status("my-reaction").await?;
    /// println!("Reaction status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_status(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::channels::ComponentStatus> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot get reaction status from uninitialized server",
            ));
        }
        self.reaction_manager
            .get_reaction_status(id.to_string())
            .await
            .map_err(|_| DrasiError::component_not_found("reaction", id))
    }

    /// Get the full configuration for a specific reaction
    ///
    /// This returns the complete reaction configuration including all fields,
    /// unlike `get_reaction_info()` which only returns runtime information.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_reaction_config("my-reaction").await?;
    /// println!("Auto-start: {}", config.auto_start);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_reaction_config(
        &self,
        id: &str,
    ) -> crate::api::Result<crate::config::ReactionConfig> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot get reaction config from uninitialized server",
            ));
        }
        self.reaction_manager
            .get_reaction_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("reaction", id))
    }

    // ============================================================================
    // Full Configuration Snapshot
    // ============================================================================

    /// Get a complete configuration snapshot of all components
    ///
    /// Returns the full server configuration including all sources, queries, and reactions
    /// with their complete configurations. This is useful for persistence, backups, or introspection.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_current_config().await?;
    /// println!("Server has {} sources, {} queries, {} reactions",
    ///          config.sources.len(), config.queries.len(), config.reactions.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_current_config(&self) -> crate::api::Result<DrasiServerCoreConfig> {
        if !*self.initialized.read().await {
            return Err(DrasiError::invalid_state(
                "Cannot get config from uninitialized server",
            ));
        }

        // Collect all source configs
        let source_ids: Vec<String> = self
            .source_manager
            .list_sources()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        let mut sources = Vec::new();
        for id in source_ids {
            if let Some(config) = self.source_manager.get_source_config(&id).await {
                sources.push(config);
            }
        }

        // Collect all query configs
        let query_ids: Vec<String> = self
            .query_manager
            .list_queries()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        let mut queries = Vec::new();
        for id in query_ids {
            if let Some(config) = self.query_manager.get_query_config(&id).await {
                queries.push(config);
            }
        }

        // Collect all reaction configs
        let reaction_ids: Vec<String> = self
            .reaction_manager
            .list_reactions()
            .await
            .into_iter()
            .map(|(id, _)| id)
            .collect();

        let mut reactions = Vec::new();
        for id in reaction_ids {
            if let Some(config) = self.reaction_manager.get_reaction_config(&id).await {
                reactions.push(config);
            }
        }

        Ok(DrasiServerCoreConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: self.config.server_core.id.clone(),
                priority_queue_capacity: self.config.server_core.priority_queue_capacity,
                broadcast_channel_capacity: self.config.server_core.broadcast_channel_capacity,
            },
            sources,
            queries,
            reactions,
        })
    }

    // ============================================================================
    // Builder and Config File Loading
    // ============================================================================

    /// Create a builder for configuring DrasiServerCore programmatically
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .with_id("my-server")
    ///     .add_source(Source::application("source1").build())
    ///     .add_query(Query::cypher("query1")
    ///         .query("MATCH (n) RETURN n")
    ///         .from_source("source1")
    ///         .build())
    ///     .add_reaction(Reaction::application("reaction1")
    ///         .subscribe_to("query1")
    ///         .build())
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn builder() -> crate::api::DrasiServerCoreBuilder {
        crate::api::DrasiServerCoreBuilder::new()
    }

    /// Load configuration from a YAML or JSON file and create a ready-to-start server
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::DrasiServerCore;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::from_config_file("config.yaml").await?;
    /// core.start().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn from_config_file(path: impl AsRef<Path>) -> crate::api::Result<Self> {
        let config = DrasiServerCoreConfig::load_from_file(path)?;
        config.validate()?;

        let runtime_config = Arc::new(RuntimeConfig::from(config));
        let mut core = Self::new(runtime_config);
        core.initialize().await?;

        Ok(core)
    }

    /// Load configuration from a YAML or JSON string and create a ready-to-start server
    ///
    /// # Example
    /// ```no_run
    /// use drasi_server_core::DrasiServerCore;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let yaml = r#"
    /// server_core:
    ///   id: my-server
    /// sources: []
    /// queries: []
    /// reactions: []
    /// "#;
    /// let core = DrasiServerCore::from_config_str(yaml).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn from_config_str(yaml: &str) -> crate::api::Result<Self> {
        let config: DrasiServerCoreConfig = serde_yaml::from_str(yaml)?;
        config.validate()?;

        let runtime_config = Arc::new(RuntimeConfig::from(config));
        let mut core = Self::new(runtime_config);
        core.initialize().await?;

        Ok(core)
    }

    // ============================================================================
    // Server Status
    // ============================================================================

    /// Check if the server is currently running
    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    // ============================================================================
    // Internal Methods
    // ============================================================================
    // Note: Direct manager/router access methods were removed as they are not used
    // internally. Managers are accessed directly via self.source_manager, etc.

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

            // DataRouter no longer needed - queries subscribe directly to sources
            // control_signal_rx is no longer used
            drop(receivers.control_signal_rx);

            // SubscriptionRouter no longer needed - reactions subscribe directly to queries
        }
    }

    async fn create_source_with_options(
        &self,
        config: crate::config::SourceConfig,
        allow_auto_start: bool,
    ) -> Result<()> {
        // Add the source (without saving during initialization)
        self.source_manager
            .add_source_without_save(config, allow_auto_start)
            .await?;

        Ok(())
    }

    async fn create_query_with_options(
        &self,
        config: crate::config::QueryConfig,
        allow_auto_start: bool,
    ) -> Result<()> {
        let query_id = config.id.clone();
        let should_auto_start = config.auto_start;

        // Add the query (without saving during initialization)
        self.query_manager.add_query_without_save(config).await?;

        // Start if auto-start is enabled and allowed
        if should_auto_start && allow_auto_start {
            // Query will subscribe directly to sources when started
            self.query_manager.start_query(query_id.clone()).await?;
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

        // Add the reaction (without saving during initialization)
        self.reaction_manager
            .add_reaction_without_save(config)
            .await?;

        // Start if auto-start is enabled and allowed
        if should_auto_start && allow_auto_start {
            // Pass server core to reaction for direct query subscriptions
            self.reaction_manager
                .start_reaction(reaction_id.clone(), Arc::new(self.clone()))
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
                        .start_reaction(id.clone(), Arc::new(self.clone()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Query, Reaction, Source};

    #[tokio::test]
    async fn test_server_initialization() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let mut core = DrasiServerCore::new(config);
        let result = core.initialize().await;

        assert!(result.is_ok(), "Server initialization should succeed");
        assert!(
            *core.initialized.read().await,
            "Server should be marked as initialized"
        );
    }

    #[tokio::test]
    async fn test_server_initialization_idempotent() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let mut core = DrasiServerCore::new(config);

        // First initialization
        let result1 = core.initialize().await;
        assert!(result1.is_ok(), "First initialization should succeed");

        // Second initialization should also succeed (idempotent)
        let result2 = core.initialize().await;
        assert!(
            result2.is_ok(),
            "Second initialization should succeed (idempotent)"
        );
    }

    #[tokio::test]
    async fn test_start_without_initialization_fails() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let core = DrasiServerCore::new(config);
        let result = core.start().await;

        assert!(result.is_err(), "Start should fail without initialization");
        assert!(result.unwrap_err().to_string().contains("initialized"));
    }

    #[tokio::test]
    async fn test_start_already_running_fails() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let mut core = DrasiServerCore::new(config);
        core.initialize().await.unwrap();

        // First start should succeed
        core.start().await.unwrap();

        // Second start should fail
        let result = core.start().await;
        assert!(result.is_err(), "Start should fail when already running");
        assert!(result.unwrap_err().to_string().contains("already running"));
    }

    #[tokio::test]
    async fn test_stop_not_running_fails() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let mut core = DrasiServerCore::new(config);
        core.initialize().await.unwrap();

        let result = core.stop().await;
        assert!(result.is_err(), "Stop should fail when not running");
        assert!(result.unwrap_err().to_string().contains("already stopped"));
    }

    #[tokio::test]
    async fn test_start_and_stop_lifecycle() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let mut core = DrasiServerCore::new(config);
        core.initialize().await.unwrap();

        assert!(
            !core.is_running().await,
            "Server should not be running initially"
        );

        core.start().await.unwrap();
        assert!(
            core.is_running().await,
            "Server should be running after start"
        );

        core.stop().await.unwrap();
        assert!(
            !core.is_running().await,
            "Server should not be running after stop"
        );
    }

    #[tokio::test]
    async fn test_builder_creates_initialized_server() {
        let core = DrasiServerCore::builder()
            .with_id("builder-test")
            .build()
            .await;

        assert!(core.is_ok(), "Builder should create initialized server");
        let core = core.unwrap();
        assert!(
            *core.initialized.read().await,
            "Server should be initialized"
        );
    }

    #[tokio::test]
    async fn test_from_config_str_creates_server() {
        let yaml = r#"
server_core:
  id: yaml-test
sources: []
queries: []
reactions: []
"#;
        let core = DrasiServerCore::from_config_str(yaml).await;
        assert!(core.is_ok(), "from_config_str should create server");
        assert!(*core.unwrap().initialized.read().await);
    }

    #[tokio::test]
    async fn test_source_handle_for_nonexistent_source() {
        let core = DrasiServerCore::builder().build().await.unwrap();

        let result = core.source_handle("nonexistent");
        assert!(
            result.is_err(),
            "Should return error for nonexistent source"
        );

        match result {
            Err(DrasiError::ComponentNotFound { kind, id }) => {
                assert_eq!(kind, "source");
                assert_eq!(id, "nonexistent");
            }
            _ => panic!("Expected ComponentNotFound error"),
        }
    }

    #[tokio::test]
    async fn test_reaction_handle_for_nonexistent_reaction() {
        let core = DrasiServerCore::builder().build().await.unwrap();

        let result = core.reaction_handle("nonexistent");
        assert!(
            result.is_err(),
            "Should return error for nonexistent reaction"
        );

        match result {
            Err(DrasiError::ComponentNotFound { kind, id }) => {
                assert_eq!(kind, "reaction");
                assert_eq!(id, "nonexistent");
            }
            _ => panic!("Expected ComponentNotFound error"),
        }
    }

    #[tokio::test]
    async fn test_create_source_without_initialization() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let core = DrasiServerCore::new(config);
        let source = Source::application("runtime-source").build();

        let result = core.create_source(source).await;
        assert!(
            result.is_err(),
            "create_source should fail without initialization"
        );
        assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
    }

    #[tokio::test]
    async fn test_create_query_without_initialization() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let core = DrasiServerCore::new(config);
        let query = Query::cypher("runtime-query")
            .query("MATCH (n) RETURN n")
            .from_source("source1")
            .build();

        let result = core.create_query(query).await;
        assert!(
            result.is_err(),
            "create_query should fail without initialization"
        );
        assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
    }

    #[tokio::test]
    async fn test_create_reaction_without_initialization() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let core = DrasiServerCore::new(config);
        let reaction = Reaction::log("runtime-reaction")
            .subscribe_to("query1")
            .build();

        let result = core.create_reaction(reaction).await;
        assert!(
            result.is_err(),
            "create_reaction should fail without initialization"
        );
        assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
    }

    #[tokio::test]
    async fn test_remove_source_without_initialization() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let core = DrasiServerCore::new(config);

        let result = core.remove_source("any-source").await;
        assert!(
            result.is_err(),
            "remove_source should fail without initialization"
        );
        assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
    }

    #[tokio::test]
    async fn test_remove_query_without_initialization() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let core = DrasiServerCore::new(config);

        let result = core.remove_query("any-query").await;
        assert!(
            result.is_err(),
            "remove_query should fail without initialization"
        );
        assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
    }

    #[tokio::test]
    async fn test_remove_reaction_without_initialization() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let core = DrasiServerCore::new(config);

        let result = core.remove_reaction("any-reaction").await;
        assert!(
            result.is_err(),
            "remove_reaction should fail without initialization"
        );
        assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
    }

    #[tokio::test]
    async fn test_remove_nonexistent_source() {
        let mut core = DrasiServerCore::builder().build().await.unwrap();
        core.initialize().await.unwrap();

        let result = core.remove_source("nonexistent").await;
        assert!(result.is_err(), "Should fail to remove nonexistent source");
        assert!(matches!(
            result.unwrap_err(),
            DrasiError::ComponentNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn test_remove_nonexistent_query() {
        let mut core = DrasiServerCore::builder().build().await.unwrap();
        core.initialize().await.unwrap();

        let result = core.remove_query("nonexistent").await;
        assert!(result.is_err(), "Should fail to remove nonexistent query");
        assert!(matches!(
            result.unwrap_err(),
            DrasiError::ComponentNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn test_remove_nonexistent_reaction() {
        let mut core = DrasiServerCore::builder().build().await.unwrap();
        core.initialize().await.unwrap();

        let result = core.remove_reaction("nonexistent").await;
        assert!(
            result.is_err(),
            "Should fail to remove nonexistent reaction"
        );
        assert!(matches!(
            result.unwrap_err(),
            DrasiError::ComponentNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn test_is_running_initial_state() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let core = DrasiServerCore::new(config);
        assert!(
            !core.is_running().await,
            "Server should not be running initially"
        );
    }

    #[tokio::test]
    async fn test_builder_with_components() {
        let core = DrasiServerCore::builder()
            .with_id("complex-server")
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("query1")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .add_reaction(Reaction::log("reaction1").subscribe_to("query1").build())
            .build()
            .await;

        assert!(core.is_ok(), "Builder with components should succeed");
        let core = core.unwrap();
        assert!(*core.initialized.read().await);
        assert_eq!(core.config.sources.len(), 1);
        assert_eq!(core.config.queries.len(), 1);
        assert_eq!(core.config.reactions.len(), 1);
    }

    // ========================================================================
    // Tests for Component Listing and Inspection APIs
    // ========================================================================

    #[tokio::test]
    async fn test_list_sources() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_source(Source::mock("source2").build())
            .build()
            .await
            .unwrap();

        let sources = core.list_sources().await.unwrap();
        assert_eq!(sources.len(), 2);

        let source_ids: Vec<String> = sources.iter().map(|(id, _)| id.clone()).collect();
        assert!(source_ids.contains(&"source1".to_string()));
        assert!(source_ids.contains(&"source2".to_string()));
    }

    #[tokio::test]
    async fn test_list_sources_without_initialization() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let core = DrasiServerCore::new(config);
        let result = core.list_sources().await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
    }

    #[tokio::test]
    async fn test_get_source_info() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("test-source").build())
            .build()
            .await
            .unwrap();

        let source_info = core.get_source_info("test-source").await.unwrap();
        assert_eq!(source_info.id, "test-source");
        assert_eq!(source_info.source_type, "application");
    }

    #[tokio::test]
    async fn test_get_source_info_not_found() {
        let core = DrasiServerCore::builder().build().await.unwrap();

        let result = core.get_source_info("nonexistent").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DrasiError::ComponentNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn test_get_source_status() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("test-source").build())
            .build()
            .await
            .unwrap();

        let status = core.get_source_status("test-source").await.unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));
    }

    #[tokio::test]
    async fn test_list_queries() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("query1")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .add_query(
                Query::cypher("query2")
                    .query("MATCH (n) RETURN n.name")
                    .from_source("source1")
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let queries = core.list_queries().await.unwrap();
        assert_eq!(queries.len(), 2);

        let query_ids: Vec<String> = queries.iter().map(|(id, _)| id.clone()).collect();
        assert!(query_ids.contains(&"query1".to_string()));
        assert!(query_ids.contains(&"query2".to_string()));
    }

    #[tokio::test]
    async fn test_get_query_info() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("test-query")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let query_info = core.get_query_info("test-query").await.unwrap();
        assert_eq!(query_info.id, "test-query");
        assert_eq!(query_info.query, "MATCH (n) RETURN n");
        assert!(query_info.sources.contains(&"source1".to_string()));
    }

    #[tokio::test]
    async fn test_get_query_info_not_found() {
        let core = DrasiServerCore::builder().build().await.unwrap();

        let result = core.get_query_info("nonexistent").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DrasiError::ComponentNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn test_get_query_status() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("test-query")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let status = core.get_query_status("test-query").await.unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));
    }

    #[tokio::test]
    async fn test_get_query_results_not_running() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("test-query")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let result = core.get_query_results("test-query").await;
        assert!(result.is_err());
        // Query must be running to get results
        let err = result.unwrap_err();
        assert!(matches!(err, DrasiError::InvalidState(_)));
    }

    #[tokio::test]
    async fn test_list_reactions() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("query1")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .add_reaction(Reaction::log("reaction1").subscribe_to("query1").build())
            .add_reaction(
                Reaction::application("reaction2")
                    .subscribe_to("query1")
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let reactions = core.list_reactions().await.unwrap();
        assert_eq!(reactions.len(), 2);

        let reaction_ids: Vec<String> = reactions.iter().map(|(id, _)| id.clone()).collect();
        assert!(reaction_ids.contains(&"reaction1".to_string()));
        assert!(reaction_ids.contains(&"reaction2".to_string()));
    }

    #[tokio::test]
    async fn test_get_reaction_info() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("query1")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .add_reaction(
                Reaction::log("test-reaction")
                    .subscribe_to("query1")
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let reaction_info = core.get_reaction_info("test-reaction").await.unwrap();
        assert_eq!(reaction_info.id, "test-reaction");
        assert_eq!(reaction_info.reaction_type, "log");
        assert!(reaction_info.queries.contains(&"query1".to_string()));
    }

    #[tokio::test]
    async fn test_get_reaction_info_not_found() {
        let core = DrasiServerCore::builder().build().await.unwrap();

        let result = core.get_reaction_info("nonexistent").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DrasiError::ComponentNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn test_get_reaction_status() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("query1")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .add_reaction(
                Reaction::log("test-reaction")
                    .subscribe_to("query1")
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let status = core.get_reaction_status("test-reaction").await.unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));
    }

    #[tokio::test]
    async fn test_listing_empty_components() {
        let core = DrasiServerCore::builder().build().await.unwrap();

        let sources = core.list_sources().await.unwrap();
        assert_eq!(sources.len(), 0);

        let queries = core.list_queries().await.unwrap();
        assert_eq!(queries.len(), 0);

        let reactions = core.list_reactions().await.unwrap();
        assert_eq!(reactions.len(), 0);
    }

    #[tokio::test]
    async fn test_list_components_after_runtime_add() {
        let mut core = DrasiServerCore::builder().build().await.unwrap();
        core.initialize().await.unwrap();

        // Initially empty
        let sources = core.list_sources().await.unwrap();
        assert_eq!(sources.len(), 0);

        // Add a source at runtime
        core.create_source(Source::application("runtime-source").build())
            .await
            .unwrap();

        // Should now show in list
        let sources = core.list_sources().await.unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "runtime-source");
    }

    #[tokio::test]
    async fn test_list_components_after_removal() {
        let mut core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_source(Source::application("source2").build())
            .build()
            .await
            .unwrap();

        core.initialize().await.unwrap();

        let sources = core.list_sources().await.unwrap();
        assert_eq!(sources.len(), 2);

        // Remove one source
        core.remove_source("source1").await.unwrap();

        // Should only show remaining source
        let sources = core.list_sources().await.unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0].0, "source2");
    }

    // ========================================================================
    // Tests for Component Start/Stop APIs
    // ========================================================================

    #[tokio::test]
    async fn test_start_source() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("test-source").auto_start(false).build())
            .build()
            .await
            .unwrap();

        // Initially stopped
        let status = core.get_source_status("test-source").await.unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));

        // Start it
        core.start_source("test-source").await.unwrap();

        // Should now be starting or running
        let status = core.get_source_status("test-source").await.unwrap();
        assert!(matches!(
            status,
            ComponentStatus::Starting | ComponentStatus::Running
        ));
    }

    #[tokio::test]
    async fn test_stop_source() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("test-source").auto_start(true).build())
            .build()
            .await
            .unwrap();

        // Start the server to start auto-start components
        core.start().await.unwrap();

        // Should be running
        let status = core.get_source_status("test-source").await.unwrap();
        assert!(matches!(
            status,
            ComponentStatus::Running | ComponentStatus::Starting
        ));

        // Stop it
        core.stop_source("test-source").await.unwrap();

        // Should now be stopped
        let status = core.get_source_status("test-source").await.unwrap();
        assert!(matches!(
            status,
            ComponentStatus::Stopped | ComponentStatus::Stopping
        ));
    }

    #[tokio::test]
    async fn test_start_source_not_found() {
        let core = DrasiServerCore::builder().build().await.unwrap();

        let result = core.start_source("nonexistent").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DrasiError::ComponentNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn test_start_query() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("test-query")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        // Initially stopped
        let status = core.get_query_status("test-query").await.unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));

        // Start it
        core.start_query("test-query").await.unwrap();

        // Should now be starting or running
        let status = core.get_query_status("test-query").await.unwrap();
        assert!(matches!(
            status,
            ComponentStatus::Starting | ComponentStatus::Running
        ));
    }

    #[tokio::test]
    async fn test_stop_query() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("test-query")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .auto_start(true)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        // Start the server
        core.start().await.unwrap();

        // Should be running
        let status = core.get_query_status("test-query").await.unwrap();
        assert!(matches!(
            status,
            ComponentStatus::Running | ComponentStatus::Starting
        ));

        // Stop it
        core.stop_query("test-query").await.unwrap();

        // Should now be stopped
        let status = core.get_query_status("test-query").await.unwrap();
        assert!(matches!(
            status,
            ComponentStatus::Stopped | ComponentStatus::Stopping
        ));
    }

    #[tokio::test]
    async fn test_start_query_not_found() {
        let core = DrasiServerCore::builder().build().await.unwrap();

        let result = core.start_query("nonexistent").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DrasiError::ComponentNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn test_start_reaction() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("query1")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .add_reaction(
                Reaction::log("test-reaction")
                    .subscribe_to("query1")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        // Initially stopped
        let status = core.get_reaction_status("test-reaction").await.unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));

        // Start it
        core.start_reaction("test-reaction").await.unwrap();

        // Should now be starting or running
        let status = core.get_reaction_status("test-reaction").await.unwrap();
        assert!(matches!(
            status,
            ComponentStatus::Starting | ComponentStatus::Running
        ));
    }

    #[tokio::test]
    async fn test_stop_reaction() {
        let core = DrasiServerCore::builder()
            .add_source(Source::application("source1").build())
            .add_query(
                Query::cypher("query1")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .add_reaction(
                Reaction::log("test-reaction")
                    .subscribe_to("query1")
                    .auto_start(true)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        // Start the server
        core.start().await.unwrap();

        // Should be running
        let status = core.get_reaction_status("test-reaction").await.unwrap();
        assert!(matches!(
            status,
            ComponentStatus::Running | ComponentStatus::Starting
        ));

        // Stop it
        core.stop_reaction("test-reaction").await.unwrap();

        // Should now be stopped
        let status = core.get_reaction_status("test-reaction").await.unwrap();
        assert!(matches!(
            status,
            ComponentStatus::Stopped | ComponentStatus::Stopping
        ));
    }

    #[tokio::test]
    async fn test_start_reaction_not_found() {
        let core = DrasiServerCore::builder().build().await.unwrap();

        let result = core.start_reaction("nonexistent").await;
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            DrasiError::ComponentNotFound { .. }
        ));
    }

    #[tokio::test]
    async fn test_start_source_without_initialization() {
        let config = Arc::new(RuntimeConfig {
            server_core: crate::config::DrasiServerCoreSettings {
                id: "test-server".to_string(),
                priority_queue_capacity: None,
                broadcast_channel_capacity: None,
            },
            sources: vec![],
            queries: vec![],
            reactions: vec![],
        });

        let core = DrasiServerCore::new(config);
        let result = core.start_source("any-source").await;

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), DrasiError::InvalidState(_)));
    }

    #[tokio::test]
    async fn test_lifecycle_start_then_stop() {
        let core = DrasiServerCore::builder()
            .add_source(Source::mock("test-source").auto_start(false).build())
            .build()
            .await
            .unwrap();

        // Initially stopped
        let status = core.get_source_status("test-source").await.unwrap();
        assert!(matches!(status, ComponentStatus::Stopped));

        // Start
        core.start_source("test-source").await.unwrap();
        let status = core.get_source_status("test-source").await.unwrap();
        assert!(matches!(
            status,
            ComponentStatus::Starting | ComponentStatus::Running
        ));

        // Stop
        core.stop_source("test-source").await.unwrap();
        let status = core.get_source_status("test-source").await.unwrap();
        assert!(matches!(
            status,
            ComponentStatus::Stopped | ComponentStatus::Stopping
        ));
    }

    #[tokio::test]
    async fn test_start_stop_all_component_types() {
        let core = DrasiServerCore::builder()
            .add_source(Source::mock("source1").auto_start(false).build())
            .add_query(
                Query::cypher("query1")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .auto_start(false)
                    .build(),
            )
            .add_reaction(
                Reaction::log("reaction1")
                    .subscribe_to("query1")
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        // All should be stopped initially
        assert!(matches!(
            core.get_source_status("source1").await.unwrap(),
            ComponentStatus::Stopped
        ));
        assert!(matches!(
            core.get_query_status("query1").await.unwrap(),
            ComponentStatus::Stopped
        ));
        assert!(matches!(
            core.get_reaction_status("reaction1").await.unwrap(),
            ComponentStatus::Stopped
        ));

        // Start all
        core.start_source("source1").await.unwrap();
        core.start_query("query1").await.unwrap();
        core.start_reaction("reaction1").await.unwrap();

        // All should be starting or running
        let source_status = core.get_source_status("source1").await.unwrap();
        assert!(matches!(
            source_status,
            ComponentStatus::Starting | ComponentStatus::Running
        ));

        let query_status = core.get_query_status("query1").await.unwrap();
        assert!(matches!(
            query_status,
            ComponentStatus::Starting | ComponentStatus::Running
        ));

        let reaction_status = core.get_reaction_status("reaction1").await.unwrap();
        assert!(matches!(
            reaction_status,
            ComponentStatus::Starting | ComponentStatus::Running
        ));
    }
}
