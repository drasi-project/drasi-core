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
use log::{info, warn};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::api::{DrasiError, HandleRegistry};
use crate::channels::*;
use crate::component_ops::{map_component_error, map_state_error};
use crate::config::{DrasiServerCoreConfig, RuntimeConfig};
use crate::inspection::InspectionAPI;
use crate::lifecycle::{ComponentsRunningState, LifecycleManager};
use crate::queries::QueryManager;
use crate::reactions::ApplicationReactionHandle;
use crate::reactions::ReactionManager;
use crate::sources::{ApplicationSourceHandle, SourceManager};
use crate::state_guard::StateGuard;

/// Core Drasi Server for continuous query processing
///
/// `DrasiServerCore` is the main entry point for embedding Drasi functionality in your application.
/// It manages sources (data ingestion), queries (continuous Cypher/GQL queries), and reactions
/// (output destinations) with a reactive event-driven architecture.
///
/// # Architecture
///
/// - **Sources**: Data ingestion points (PostgreSQL, HTTP, gRPC, Application, Mock, Platform)
/// - **Queries**: Continuous Cypher or GQL queries that process data changes in real-time
/// - **Reactions**: Output destinations that receive query results (HTTP, gRPC, Application, Log)
///
/// # Lifecycle States
///
/// The server progresses through these states:
/// 1. **Created** (via `new()`, `builder()`, `from_config_file()`, or `from_config_str()`)
/// 2. **Initialized** (one-time setup, automatic with builder/config loaders)
/// 3. **Running** (after `start()`)
/// 4. **Stopped** (after `stop()`, can be restarted)
///
/// Components (sources, queries, reactions) have independent lifecycle states:
/// - `Stopped`: Component exists but is not processing
/// - `Starting`: Component is initializing
/// - `Running`: Component is actively processing
/// - `Stopping`: Component is shutting down
///
/// # Thread Safety
///
/// `DrasiServerCore` is `Clone` (all clones share the same underlying state) and all methods
/// are thread-safe. You can safely share clones across threads and call methods concurrently.
///
/// # Examples
///
/// ## Builder Pattern
///
/// ```no_run
/// use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .with_id("my-server")
///     .add_source(
///         Source::application("events")
///             .auto_start(true)
///             .build()
///     )
///     .add_query(
///         Query::cypher("my-query")
///             .query("MATCH (n:Person) RETURN n.name, n.age")
///             .from_source("events")
///             .auto_start(true)
///             .build()
///     )
///     .add_reaction(
///         Reaction::application("results")
///             .subscribe_to("my-query")
///             .auto_start(true)
///             .build()
///     )
///     .build()
///     .await?;
///
/// // Start all auto-start components
/// core.start().await?;
///
/// // Get handles for programmatic interaction
/// let source_handle = core.source_handle("events").await?;
/// let reaction_handle = core.reaction_handle("results").await?;
///
/// // Inject data into source
/// let properties = drasi_server_core::PropertyMapBuilder::new()
///     .with_string("name", "Alice")
///     .with_integer("age", 30)
///     .build();
///
/// source_handle.send_node_insert("1", vec!["Person"], properties).await?;
///
/// // Consume results from reaction
/// let mut sub = reaction_handle.subscribe_with_options(
///     drasi_server_core::SubscriptionOptions::default()
/// ).await?;
/// if let Some(result) = sub.recv().await {
///     println!("Result: {:?}", result);
/// }
///
/// // Stop server
/// core.stop().await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Configuration File
///
/// ```no_run
/// use drasi_server_core::DrasiServerCore;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Load from YAML configuration
/// let core = DrasiServerCore::from_config_file("config.yaml").await?;
/// core.start().await?;
///
/// // ... use the server ...
///
/// core.stop().await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Dynamic Runtime Configuration
///
/// ```no_run
/// use drasi_server_core::{DrasiServerCore, Source, Query};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .with_id("dynamic-server")
///     .build()
///     .await?;
///
/// core.start().await?;
///
/// // Add components at runtime
/// core.create_source(
///     Source::application("new-source")
///         .auto_start(true)
///         .build()
/// ).await?;
///
/// core.create_query(
///     Query::cypher("new-query")
///         .query("MATCH (n) RETURN n")
///         .from_source("new-source")
///         .auto_start(true)
///         .build()
/// ).await?;
///
/// // Start/stop individual components
/// core.stop_query("new-query").await?;
/// core.start_query("new-query").await?;
///
/// // Remove components
/// core.remove_query("new-query").await?;
/// core.remove_source("new-source").await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Restart Behavior
///
/// When you call `stop()` and then `start()` again, the server remembers which components
/// were running and will restart them automatically:
///
/// ```no_run
/// # use drasi_server_core::DrasiServerCore;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiServerCore::builder().build().await?;
/// core.start().await?;
/// // ... components are running ...
///
/// core.stop().await?;
/// // ... all components stopped ...
///
/// core.start().await?;
/// // ... previously running components automatically restarted ...
/// # Ok(())
/// # }
/// ```
pub struct DrasiServerCore {
    config: Arc<RuntimeConfig>,
    source_manager: Arc<SourceManager>,
    query_manager: Arc<QueryManager>,
    reaction_manager: Arc<ReactionManager>,
    running: Arc<RwLock<bool>>,
    state_guard: StateGuard,
    // Track components that were running before server stop
    components_running_before_stop: Arc<RwLock<ComponentsRunningState>>,
    // Handle registry for application sources and reactions
    handle_registry: HandleRegistry,
    // Inspection API for querying server state
    inspection: InspectionAPI,
    // Lifecycle manager for orchestrating component lifecycle
    lifecycle: Arc<RwLock<LifecycleManager>>,
}

impl Clone for DrasiServerCore {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            source_manager: Arc::clone(&self.source_manager),
            query_manager: Arc::clone(&self.query_manager),
            reaction_manager: Arc::clone(&self.reaction_manager),
            running: Arc::clone(&self.running),
            state_guard: self.state_guard.clone(),
            components_running_before_stop: Arc::clone(&self.components_running_before_stop),
            handle_registry: self.handle_registry.clone(),
            inspection: self.inspection.clone(),
            lifecycle: Arc::clone(&self.lifecycle),
        }
    }
}

impl DrasiServerCore {
    /// Create an Arc-wrapped reference to self
    ///
    /// Since DrasiServerCore contains all Arc-wrapped fields, cloning is cheap
    /// (just increments ref counts), but this helper makes the intent clearer
    /// and provides a single place to document this pattern.
    fn as_arc(&self) -> Arc<Self> {
        Arc::new(self.clone())
    }

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

        let state_guard = StateGuard::new();

        let inspection = InspectionAPI::new(
            source_manager.clone(),
            query_manager.clone(),
            reaction_manager.clone(),
            state_guard.clone(),
            config.clone(),
        );

        let components_running_before_stop =
            Arc::new(RwLock::new(ComponentsRunningState::default()));

        let lifecycle = Arc::new(RwLock::new(LifecycleManager::new(
            config.clone(),
            source_manager.clone(),
            query_manager.clone(),
            reaction_manager.clone(),
            components_running_before_stop.clone(),
            Some(receivers),
        )));

        Self {
            config,
            source_manager,
            query_manager,
            reaction_manager,
            running: Arc::new(RwLock::new(false)),
            state_guard,
            components_running_before_stop,
            handle_registry: HandleRegistry::new(),
            inspection,
            lifecycle,
        }
    }

    /// Internal initialization - performs one-time setup
    /// This is called internally by builder and config loaders
    pub(crate) async fn initialize(&mut self) -> Result<()> {
        let already_initialized = self.state_guard.is_initialized().await;
        if already_initialized {
            info!("Server already initialized, skipping initialization");
            return Ok(());
        }

        info!("Initializing Drasi Server Core");

        // Load configuration
        let lifecycle = self.lifecycle.read().await;
        lifecycle.load_configuration().await?;
        drop(lifecycle);

        // Start event processors (one-time)
        let mut lifecycle = self.lifecycle.write().await;
        lifecycle.start_event_processors().await;
        drop(lifecycle);

        self.state_guard.mark_initialized().await;
        info!("Drasi Server Core initialized successfully");
        Ok(())
    }

    /// Start the server and all auto-start components
    ///
    /// This starts all components (sources, queries, reactions) that have `auto_start` set to `true`,
    /// as well as any components that were running when `stop()` was last called.
    ///
    /// Components are started in dependency order: Sources → Queries → Reactions
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The server is not initialized (`DrasiError::InvalidState`)
    /// * The server is already running (`anyhow::Error`)
    /// * Any component fails to start (propagated from component)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .with_id("my-server")
    ///     .build()
    ///     .await?;
    ///
    /// // Start server and all auto-start components
    /// core.start().await?;
    ///
    /// assert!(core.is_running().await);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start(&self) -> Result<()> {
        let mut running = self.running.write().await;
        if *running {
            warn!("Server is already running");
            return Err(anyhow!("Server is already running"));
        }

        info!("Starting Drasi Server Core");

        // Ensure initialized
        if !self.state_guard.is_initialized().await {
            return Err(anyhow!("Server must be initialized before starting"));
        }

        // Start all configured components
        let lifecycle = self.lifecycle.read().await;
        lifecycle.start_components(self.as_arc()).await?;

        *running = true;
        info!("Drasi Server Core started successfully");

        Ok(())
    }

    /// Stop the server and all running components
    ///
    /// This stops all currently running components (sources, queries, reactions) and saves their
    /// state so they can be automatically restarted on the next `start()` call.
    ///
    /// Components are stopped in reverse dependency order: Reactions → Queries → Sources
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The server is not running (`anyhow::Error`)
    /// * Any component fails to stop (logged as error, but doesn't prevent other components from stopping)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let core = DrasiServerCore::builder().build().await?;
    /// # core.start().await?;
    /// // Stop server and all running components
    /// core.stop().await?;
    ///
    /// assert!(!core.is_running().await);
    ///
    /// // Components will be automatically restarted on next start()
    /// core.start().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop(&self) -> Result<()> {
        let mut running = self.running.write().await;
        if !*running {
            warn!("Server is already stopped");
            return Err(anyhow!("Server is already stopped"));
        }

        info!("Stopping Drasi Server Core");

        // Save running component state before stopping
        let lifecycle = self.lifecycle.read().await;
        lifecycle.save_running_components_state().await?;

        // Stop all components
        lifecycle.stop_all_components().await?;

        *running = false;
        info!("Drasi Server Core stopped successfully");

        Ok(())
    }

    // ============================================================================
    // New Public API - Handle Access
    // ============================================================================

    /// Get a handle to an application source for programmatic event injection
    ///
    /// Returns an [`ApplicationSourceHandle`] that allows you to programmatically inject
    /// graph data changes (node inserts, updates, deletes, relation inserts) into the source.
    ///
    /// The handle is cached after first retrieval for efficiency.
    ///
    /// # Arguments
    ///
    /// * `id` - The unique identifier of the application source
    ///
    /// # Returns
    ///
    /// Returns `Ok(ApplicationSourceHandle)` if the source exists and is an application source.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The source doesn't exist (`DrasiError::ComponentNotFound`)
    /// * The source is not an application source type
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently. The returned handle can
    /// also be cloned and used across multiple threads.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .build()
    ///     .await?;
    ///
    /// // Get handle to inject events
    /// let handle = core.source_handle("events").await?;
    ///
    /// // Inject a node insert
    /// let properties = drasi_server_core::PropertyMapBuilder::new()
    ///     .with_string("name", "Alice")
    ///     .with_integer("age", 30)
    ///     .build();
    ///
    /// handle.send_node_insert("user-1", vec!["User"], properties).await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// See also: [`ApplicationSourceHandle`] for available operations
    pub async fn source_handle(&self, id: &str) -> crate::api::Result<ApplicationSourceHandle> {
        // Try to get from handle registry first
        if let Ok(handle) = self.handle_registry.get_source_handle(id).await {
            return Ok(handle);
        }

        // If not in registry, try to get from source manager
        // This handles the case where the source exists but handle isn't registered yet
        if let Some(handle) = self.source_manager.get_application_handle(id).await {
            // Register it for future use
            self.handle_registry
                .register_source_handle(id.to_string(), handle.clone())
                .await;
            Ok(handle)
        } else {
            Err(DrasiError::component_not_found("source", id))
        }
    }

    /// Get direct access to the query manager (advanced usage)
    ///
    /// This provides low-level access to the query manager for advanced scenarios.
    /// Most users should use the higher-level methods like `get_query_info()`,
    /// `start_query()`, etc. instead.
    ///
    /// # Thread Safety
    ///
    /// The returned reference is thread-safe and can be used across threads.
    pub fn query_manager(&self) -> &QueryManager {
        &self.query_manager
    }

    /// Get a handle to an application reaction for programmatic result consumption
    ///
    /// Returns an [`ApplicationReactionHandle`] that allows you to subscribe to and
    /// consume query results programmatically in your application code.
    ///
    /// The handle is cached after first retrieval for efficiency.
    ///
    /// # Arguments
    ///
    /// * `id` - The unique identifier of the application reaction
    ///
    /// # Returns
    ///
    /// Returns `Ok(ApplicationReactionHandle)` if the reaction exists and is an application reaction.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The reaction doesn't exist (`DrasiError::ComponentNotFound`)
    /// * The reaction is not an application reaction type
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently. The returned handle can
    /// also be cloned and used across multiple threads.
    ///
    /// # Examples
    ///
    /// ## Subscribe and Receive Results
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(
    ///         Query::cypher("users")
    ///             .query("MATCH (n:User) RETURN n")
    ///             .from_source("events")
    ///             .build()
    ///     )
    ///     .add_reaction(
    ///         Reaction::application("results")
    ///             .subscribe_to("users")
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    ///
    /// // Get handle to consume results
    /// let handle = core.reaction_handle("results").await?;
    ///
    /// // Subscribe to results
    /// let mut subscription = handle.subscribe_with_options(
    ///     drasi_server_core::SubscriptionOptions::default()
    /// ).await?;
    ///
    /// // Receive results as they arrive
    /// while let Some(result) = subscription.recv().await {
    ///     println!("Received result: {:?}", result);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// See also: [`ApplicationReactionHandle`] for available operations
    pub async fn reaction_handle(&self, id: &str) -> crate::api::Result<ApplicationReactionHandle> {
        // Try to get from handle registry first
        if let Ok(handle) = self.handle_registry.get_reaction_handle(id).await {
            return Ok(handle);
        }

        // If not in registry, try to get from reaction manager
        if let Some(handle) = self.reaction_manager.get_application_handle(id).await {
            // Register it for future use
            self.handle_registry
                .register_reaction_handle(id.to_string(), handle.clone())
                .await;
            Ok(handle)
        } else {
            Err(DrasiError::component_not_found("reaction", id))
        }
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
        self.state_guard.require_initialized().await?;

        // Create source with auto-start enabled if requested
        self.create_source_with_options(source, true)
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to add source: {}", e)))?;

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
        self.state_guard.require_initialized().await?;

        // Create query with auto-start enabled if requested
        self.create_query_with_options(query, true)
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to add query: {}", e)))?;

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
        self.state_guard.require_initialized().await?;

        // Create reaction with auto-start enabled if requested
        self.create_reaction_with_options(reaction, true)
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to add reaction: {}", e)))?;

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
        self.state_guard.require_initialized().await?;

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
                .map_err(|e| DrasiError::provisioning(format!("Failed to stop source: {}", e)))?;
        }

        // Delete the source
        self.source_manager
            .delete_source(id.to_string())
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to delete source: {}", e)))?;

        self.handle_registry.remove_source_handle(id).await;

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
        self.state_guard.require_initialized().await?;

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
                .map_err(|e| DrasiError::provisioning(format!("Failed to stop query: {}", e)))?;
        }

        // Delete the query
        self.query_manager
            .delete_query(id.to_string())
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to delete query: {}", e)))?;

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
        self.state_guard.require_initialized().await?;

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
                .map_err(|e| DrasiError::provisioning(format!("Failed to stop reaction: {}", e)))?;
        }

        // Delete the reaction
        self.reaction_manager
            .delete_reaction(id.to_string())
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to delete reaction: {}", e)))?;

        self.handle_registry.remove_reaction_handle(id).await;

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
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example(core: &DrasiServerCore) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_source("my-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_source(&self, id: &str) -> crate::api::Result<()> {
        self.state_guard.require_initialized().await?;

        map_component_error(
            self.source_manager.stop_source(id.to_string()).await,
            "source",
            id,
            "stop",
        )
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
        self.state_guard.require_initialized().await?;

        // Get the query config to determine which sources it needs
        // Verify query exists
        let _config = self
            .query_manager
            .get_query_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("query", id))?;

        // Query will subscribe directly to sources when started
        map_state_error(
            self.query_manager.start_query(id.to_string()).await,
            "query",
            id,
        )
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
        self.state_guard.require_initialized().await?;

        // Stop the query first
        map_state_error(
            self.query_manager.stop_query(id.to_string()).await,
            "query",
            id,
        )?;

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
        self.state_guard.require_initialized().await?;

        // Get the reaction config to determine which queries it subscribes to
        let _config = self
            .reaction_manager
            .get_reaction_config(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("reaction", id))?;

        // Start the reaction with QuerySubscriber for query subscriptions
        let subscriber: Arc<dyn crate::reactions::common::base::QuerySubscriber> = self.as_arc();
        map_state_error(
            self.reaction_manager
                .start_reaction(id.to_string(), subscriber)
                .await,
            "reaction",
            id,
        )
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
        self.state_guard.require_initialized().await?;

        // Stop the reaction - subscriptions are managed by the reaction itself
        map_state_error(
            self.reaction_manager.stop_reaction(id.to_string()).await,
            "reaction",
            id,
        )?;

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
        self.inspection.list_sources().await
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
        self.inspection.get_source_info(id).await
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
        self.inspection.get_source_status(id).await
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
        self.inspection.get_source_config(id).await
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
        self.inspection.list_queries().await
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
        self.inspection.get_query_info(id).await
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
        self.inspection.get_query_status(id).await
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
        self.inspection.get_query_results(id).await
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
        self.inspection.get_query_config(id).await
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
        self.inspection.list_reactions().await
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
        self.inspection.get_reaction_info(id).await
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
        self.inspection.get_reaction_status(id).await
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
        self.inspection.get_reaction_config(id).await
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
        self.inspection.get_current_config().await
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
        config
            .validate()
            .map_err(|e| DrasiError::startup_validation(e.to_string()))?;

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
        config
            .validate()
            .map_err(|e| DrasiError::startup_validation(e.to_string()))?;

        let runtime_config = Arc::new(RuntimeConfig::from(config));
        let mut core = Self::new(runtime_config);
        core.initialize().await?;

        Ok(core)
    }

    // ============================================================================
    // Server Status
    // ============================================================================

    /// Check if the server is currently running
    ///
    /// Returns `true` if `start()` has been called and the server is actively processing,
    /// `false` if the server is stopped or has not been started yet.
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder().build().await?;
    ///
    /// assert!(!core.is_running().await); // Not started yet
    ///
    /// core.start().await?;
    /// assert!(core.is_running().await); // Now running
    ///
    /// core.stop().await?;
    /// assert!(!core.is_running().await); // Stopped again
    /// # Ok(())
    /// # }
    /// ```
    pub async fn is_running(&self) -> bool {
        *self.running.read().await
    }

    /// Get the runtime configuration
    ///
    /// Returns a reference to the immutable runtime configuration containing server settings
    /// and all component configurations. This is the configuration that was provided during
    /// initialization (via builder, config file, or config string).
    ///
    /// For a current snapshot of the configuration including runtime additions, use
    /// [`get_current_config()`](Self::get_current_config) instead.
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .with_id("my-server")
    ///     .build()
    ///     .await?;
    ///
    /// let config = core.get_config();
    /// println!("Server ID: {}", config.server_core.id);
    /// println!("Number of sources: {}", config.sources.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn get_config(&self) -> &RuntimeConfig {
        &self.config
    }

    // ============================================================================
    // Internal Methods
    // ============================================================================
    // Note: Direct manager/router access methods were removed as they are not used
    // internally. Managers are accessed directly via self.source_manager, etc.

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
            // Pass QuerySubscriber to reaction for query subscriptions
            let subscriber: Arc<dyn crate::reactions::common::base::QuerySubscriber> =
                self.as_arc();
            self.reaction_manager
                .start_reaction(reaction_id.clone(), subscriber)
                .await?;
        }

        Ok(())
    }
}

// Implement QuerySubscriber trait for DrasiServerCore
// This breaks the circular dependency by providing a minimal interface for reactions
#[async_trait::async_trait]
impl crate::reactions::common::base::QuerySubscriber for DrasiServerCore {
    async fn get_query_instance(&self, id: &str) -> Result<Arc<dyn crate::queries::Query>> {
        self.query_manager
            .get_query_instance(id)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }
}
