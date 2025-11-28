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

use crate::error::DrasiError;
use crate::channels::*;
use crate::component_ops::{map_component_error, map_state_error};
use crate::config::{DrasiLibConfig, ReactionConfig, RuntimeConfig, SourceConfig};
use crate::inspection::InspectionAPI;
use crate::lifecycle::{ComponentsRunningState, LifecycleManager};
use crate::plugin_core::{ReactionRegistry, SourceRegistry};
use crate::queries::QueryManager;
use crate::reactions::ReactionManager;
use crate::sources::SourceManager;
use crate::state_guard::StateGuard;
use drasi_core::middleware::MiddlewareTypeRegistry;

/// Core Drasi Server for continuous query processing
///
/// `DrasiLib` is the main entry point for embedding Drasi functionality in your application.
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
/// `DrasiLib` is `Clone` (all clones share the same underlying state) and all methods
/// are thread-safe. You can safely share clones across threads and call methods concurrently.
///
/// # Examples
///
/// ## Builder Pattern
///
/// ```ignore
/// use drasi_lib::{DrasiLib, Source, Query, Reaction};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder()
///     .with_id("my-server")
///     .add_source(
///         Source::mock("events")
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
///         Reaction::log("results")
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
/// // List and inspect components
/// let sources = core.list_sources().await?;
/// let queries = core.list_queries().await?;
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
/// use drasi_lib::DrasiLib;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Load from YAML configuration
/// let core = DrasiLib::from_config_file("config.yaml").await?;
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
/// ```ignore
/// use drasi_lib::{DrasiLib, Source, Query};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder()
///     .with_id("dynamic-server")
///     .build()
///     .await?;
///
/// core.start().await?;
///
/// // Add components at runtime
/// core.create_source(
///     Source::mock("new-source")
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
/// # use drasi_lib::DrasiLib;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiLib::builder().build().await?;
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
pub struct DrasiLib {
    pub(crate) config: Arc<RuntimeConfig>,
    pub(crate) source_manager: Arc<SourceManager>,
    pub(crate) query_manager: Arc<QueryManager>,
    pub(crate) reaction_manager: Arc<ReactionManager>,
    pub(crate) running: Arc<RwLock<bool>>,
    pub(crate) state_guard: StateGuard,
    // Track components that were running before server stop
    components_running_before_stop: Arc<RwLock<ComponentsRunningState>>,
    // Inspection API for querying server state
    inspection: InspectionAPI,
    // Lifecycle manager for orchestrating component lifecycle
    lifecycle: Arc<RwLock<LifecycleManager>>,
    // Middleware registry for source middleware
    middleware_registry: Arc<MiddlewareTypeRegistry>,
    // Plugin registries for factory-based component creation
    source_registry: Arc<RwLock<Option<SourceRegistry>>>,
    reaction_registry: Arc<RwLock<Option<ReactionRegistry>>>,
    // Store source and reaction configs for retrieval
    source_configs: Arc<RwLock<std::collections::HashMap<String, SourceConfig>>>,
    reaction_configs: Arc<RwLock<std::collections::HashMap<String, ReactionConfig>>>,
}

impl Clone for DrasiLib {
    fn clone(&self) -> Self {
        Self {
            config: Arc::clone(&self.config),
            source_manager: Arc::clone(&self.source_manager),
            query_manager: Arc::clone(&self.query_manager),
            reaction_manager: Arc::clone(&self.reaction_manager),
            running: Arc::clone(&self.running),
            state_guard: self.state_guard.clone(),
            components_running_before_stop: Arc::clone(&self.components_running_before_stop),
            inspection: self.inspection.clone(),
            lifecycle: Arc::clone(&self.lifecycle),
            middleware_registry: Arc::clone(&self.middleware_registry),
            source_registry: Arc::clone(&self.source_registry),
            reaction_registry: Arc::clone(&self.reaction_registry),
            source_configs: Arc::clone(&self.source_configs),
            reaction_configs: Arc::clone(&self.reaction_configs),
        }
    }
}

impl DrasiLib {
    /// Create an Arc-wrapped reference to self
    ///
    /// Since DrasiLib contains all Arc-wrapped fields, cloning is cheap
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

        // Initialize middleware registry and register all standard middleware factories
        let mut middleware_registry = MiddlewareTypeRegistry::new();
        middleware_registry.register(Arc::new(drasi_middleware::jq::JQFactory::new()));
        middleware_registry.register(Arc::new(drasi_middleware::map::MapFactory::new()));
        middleware_registry.register(Arc::new(drasi_middleware::unwind::UnwindFactory::new()));
        middleware_registry.register(Arc::new(
            drasi_middleware::relabel::RelabelMiddlewareFactory::new(),
        ));
        middleware_registry.register(Arc::new(drasi_middleware::decoder::DecoderFactory::new()));
        middleware_registry.register(Arc::new(
            drasi_middleware::parse_json::ParseJsonFactory::new(),
        ));
        middleware_registry.register(Arc::new(
            drasi_middleware::promote::PromoteMiddlewareFactory::new(),
        ));
        let middleware_registry = Arc::new(middleware_registry);

        let query_manager = Arc::new(QueryManager::new(
            channels.component_event_tx.clone(),
            source_manager.clone(),
            config.index_factory.clone(),
            middleware_registry.clone(),
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
            inspection,
            lifecycle,
            middleware_registry,
            source_registry: Arc::new(RwLock::new(None)),
            reaction_registry: Arc::new(RwLock::new(None)),
            source_configs: Arc::new(RwLock::new(std::collections::HashMap::new())),
            reaction_configs: Arc::new(RwLock::new(std::collections::HashMap::new())),
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
    /// # use drasi_lib::DrasiLib;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiLib::builder()
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
    /// # use drasi_lib::DrasiLib;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let core = DrasiLib::builder().build().await?;
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

    /// Get access to the middleware registry
    ///
    /// Returns a reference to the middleware type registry that contains all registered
    /// middleware factories. The registry is pre-populated with all standard middleware
    /// types (jq, map, unwind, relabel, decoder, parse_json, promote).
    ///
    /// # Thread Safety
    ///
    /// The returned Arc can be cloned and used across threads.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiLib::builder().build().await?;
    /// let registry = core.middleware_registry();
    /// // Use registry to create middleware instances
    /// # Ok(())
    /// # }
    /// ```
    pub fn middleware_registry(&self) -> Arc<MiddlewareTypeRegistry> {
        Arc::clone(&self.middleware_registry)
    }


    // ============================================================================
    // Dynamic Runtime Configuration
    // ============================================================================

    /// Add a source instance to a running server
    ///
    /// Sources are now instance-based. The caller must create the source instance
    /// and pass it as `Arc<dyn Source>`.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # use std::sync::Arc;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// // Sources must be created as instances by the caller
    /// // let source = Arc::new(MySource::new("new-source"));
    /// // core.add_source(source).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn add_source(
        &self,
        source: Arc<dyn crate::plugin_core::Source>,
    ) -> crate::error::Result<()> {
        self.state_guard.require_initialized().await?;

        // Add the source instance
        self.source_manager
            .add_source(source)
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to add source: {}", e)))?;

        Ok(())
    }

    /// Create a query in a running server
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::{DrasiLib, Query};
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
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
    pub async fn create_query(&self, query: crate::config::QueryConfig) -> crate::error::Result<()> {
        self.state_guard.require_initialized().await?;

        // Create query with auto-start enabled if requested
        self.create_query_with_options(query, true)
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to add query: {}", e)))?;

        Ok(())
    }

    /// Add a reaction instance to a running server
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// // Reactions must be created as instances by the caller
    /// // core.add_reaction(my_reaction_instance).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn add_reaction(
        &self,
        reaction: Arc<dyn crate::plugin_core::Reaction>,
    ) -> crate::error::Result<()> {
        self.state_guard.require_initialized().await?;

        // Add the reaction instance
        self.reaction_manager
            .add_reaction(reaction)
            .await
            .map_err(|e| DrasiError::provisioning(format!("Failed to add reaction: {}", e)))?;

        Ok(())
    }

    /// Remove a source from a running server
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_source("old-source").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_source(&self, id: &str) -> crate::error::Result<()> {
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


        Ok(())
    }

    /// Remove a query from a running server
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_query("old-query").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_query(&self, id: &str) -> crate::error::Result<()> {
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
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.remove_reaction("old-reaction").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn remove_reaction(&self, id: &str) -> crate::error::Result<()> {
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


        Ok(())
    }

    // ============================================================================
    // Component Listing and Inspection
    // ============================================================================

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
    pub async fn start_source(&self, id: &str) -> crate::error::Result<()> {
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
    pub async fn stop_source(&self, id: &str) -> crate::error::Result<()> {
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
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.start_query("my-query").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_query(&self, id: &str) -> crate::error::Result<()> {
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
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_query("my-query").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_query(&self, id: &str) -> crate::error::Result<()> {
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
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.start_reaction("my-reaction").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn start_reaction(&self, id: &str) -> crate::error::Result<()> {
        self.state_guard.require_initialized().await?;

        // Start the reaction with QuerySubscriber for query subscriptions
        let subscriber: Arc<dyn crate::plugin_core::QuerySubscriber> = self.as_arc();
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
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// core.stop_reaction("my-reaction").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn stop_reaction(&self, id: &str) -> crate::error::Result<()> {
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
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let sources = core.list_sources().await?;
    /// for (id, status) in sources {
    ///     println!("Source {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_sources(
        &self,
    ) -> crate::error::Result<Vec<(String, crate::channels::ComponentStatus)>> {
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
    pub async fn get_source_info(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::config::SourceRuntime> {
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
    pub async fn get_source_status(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::channels::ComponentStatus> {
        self.inspection.get_source_status(id).await
    }

    /// List all queries with their current status
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let queries = core.list_queries().await?;
    /// for (id, status) in queries {
    ///     println!("Query {}: {:?}", id, status);
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub async fn list_queries(
        &self,
    ) -> crate::error::Result<Vec<(String, crate::channels::ComponentStatus)>> {
        self.inspection.list_queries().await
    }

    /// Get detailed information about a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let query_info = core.get_query_info("my-query").await?;
    /// println!("Query: {}", query_info.query);
    /// println!("Status: {:?}", query_info.status);
    /// println!("Source subscriptions: {:?}", query_info.source_subscriptions);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_info(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::config::QueryRuntime> {
        self.inspection.get_query_info(id).await
    }

    /// Get the current status of a specific query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let status = core.get_query_status("my-query").await?;
    /// println!("Query status: {:?}", status);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_status(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::channels::ComponentStatus> {
        self.inspection.get_query_status(id).await
    }

    /// Get the current result set for a running query
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let results = core.get_query_results("my-query").await?;
    /// println!("Current results: {} items", results.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_results(&self, id: &str) -> crate::error::Result<Vec<serde_json::Value>> {
        self.inspection.get_query_results(id).await
    }

    /// Get the full configuration for a specific query
    ///
    /// This returns the complete query configuration including all fields like auto_start and joins,
    /// unlike `get_query_info()` which only returns runtime information.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_query_config("my-query").await?;
    /// println!("Auto-start: {}", config.auto_start);
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_query_config(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::config::QueryConfig> {
        self.inspection.get_query_config(id).await
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
    pub async fn list_reactions(
        &self,
    ) -> crate::error::Result<Vec<(String, crate::channels::ComponentStatus)>> {
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
    pub async fn get_reaction_info(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::config::ReactionRuntime> {
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
    pub async fn get_reaction_status(
        &self,
        id: &str,
    ) -> crate::error::Result<crate::channels::ComponentStatus> {
        self.inspection.get_reaction_status(id).await
    }

    // ============================================================================
    // Full Configuration Snapshot
    // ============================================================================

    /// Get a complete configuration snapshot of all components
    ///
    /// Returns the full server configuration including all queries with their complete configurations.
    /// Note: Sources and reactions are now instance-based and not stored in config.
    /// Use `list_sources()` and `list_reactions()` for runtime information about these components.
    ///
    /// # Example
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let config = core.get_current_config().await?;
    /// println!("Server has {} queries", config.queries.len());
    /// # Ok(())
    /// # }
    /// ```
    pub async fn get_current_config(&self) -> crate::error::Result<DrasiLibConfig> {
        self.inspection.get_current_config().await
    }

    // ============================================================================
    // Builder and Config File Loading
    // ============================================================================


    /// Create a builder for configuring a new DrasiLib instance.
    ///
    /// The builder provides a fluent API for adding queries and source/reaction instances.
    /// Note: Sources and reactions are now instance-based. Use `with_source()` and `with_reaction()`
    /// to add pre-built instances.
    ///
    /// # Example
    /// ```no_run
    /// use drasi_lib::{DrasiLib, Query};
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiLib::builder()
    ///     .with_id("my-server")
    ///     .add_query(
    ///         Query::cypher("my-query")
    ///             .query("MATCH (n) RETURN n")
    ///             .from_source("events")
    ///             .build()
    ///     )
    ///     // Use .with_source(source_instance) and .with_reaction(reaction_instance)
    ///     // for pre-built source and reaction instances
    ///     .build()
    ///     .await?;
    ///
    /// core.start().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn builder() -> crate::builder::DrasiLibBuilder {
        crate::builder::DrasiLibBuilder::new()
    }

    /// Load configuration from a YAML or JSON file and create a ready-to-start server
    ///
    /// # Example
    /// ```no_run
    /// use drasi_lib::DrasiLib;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiLib::from_config_file("config.yaml").await?;
    /// core.start().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn from_config_file(path: impl AsRef<Path>) -> crate::error::Result<Self> {
        let config = DrasiLibConfig::load_from_file(path)?;
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
    /// use drasi_lib::DrasiLib;
    ///
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let yaml = r#"
    /// server_core:
    ///   id: my-server
    /// sources: []
    /// queries: []
    /// reactions: []
    /// "#;
    /// let core = DrasiLib::from_config_str(yaml).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn from_config_str(yaml: &str) -> crate::error::Result<Self> {
        let config: DrasiLibConfig = serde_yaml::from_str(yaml)
            .map_err(|e| DrasiError::InvalidConfig(format!("Failed to parse YAML: {}", e)))?;
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
    /// # use drasi_lib::DrasiLib;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiLib::builder().build().await?;
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
    /// # use drasi_lib::DrasiLib;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiLib::builder()
    ///     .with_id("my-server")
    ///     .build()
    ///     .await?;
    ///
    /// let config = core.get_config();
    /// println!("Server ID: {}", config.server_core.id);
    /// println!("Number of queries: {}", config.queries.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn get_config(&self) -> &RuntimeConfig {
        &self.config
    }

    // ============================================================================
    // Registry Management
    // ============================================================================

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
    pub async fn create_source(&self, config: SourceConfig) -> crate::error::Result<()> {
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
        self.source_configs.write().await.insert(source_id.clone(), config);

        drop(registry_guard);

        // Add the source to the manager
        self.source_manager.add_source(source).await.map_err(|e| {
            DrasiError::provisioning(format!("Failed to add source '{}': {}", source_id, e))
        })?;

        // Auto-start if configured and server is running
        if auto_start && *self.running.read().await {
            self.source_manager.start_source(source_id.clone()).await.map_err(|e| {
                DrasiError::component_error(format!("Failed to start source '{}': {}", source_id, e))
            })?;
        }

        Ok(())
    }

    /// Get the configuration for a source.
    ///
    /// Returns the `SourceConfig` that was used to create the source.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The source does not exist
    /// * The source was created without a config (e.g., via `with_source()`)
    pub async fn get_source_config(&self, id: &str) -> crate::error::Result<SourceConfig> {
        // First check if the source exists using get_source_instance
        if self.source_manager.get_source_instance(id).await.is_none() {
            return Err(DrasiError::component_not_found("source", id));
        }

        // Get the stored config
        let configs = self.source_configs.read().await;
        configs.get(id).cloned().ok_or_else(|| {
            DrasiError::component_not_found(
                "source config",
                &format!("{} (was it created via with_source()?)", id),
            )
        })
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
    pub async fn create_reaction(&self, config: ReactionConfig) -> crate::error::Result<()> {
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
                .map_err(|e| DrasiError::component_error(format!("Failed to start reaction '{}': {}", reaction_id, e)))?;
        }

        Ok(())
    }

    /// Get the configuration for a reaction.
    ///
    /// Returns the `ReactionConfig` that was used to create the reaction.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The reaction does not exist
    /// * The reaction was created without a config (e.g., via `with_reaction()`)
    pub async fn get_reaction_config(&self, id: &str) -> crate::error::Result<ReactionConfig> {
        // First check if the reaction exists by trying to get its status
        if self.reaction_manager.get_reaction_status(id.to_string()).await.is_err() {
            return Err(DrasiError::component_not_found("reaction", id));
        }

        // Get the stored config
        let configs = self.reaction_configs.read().await;
        configs.get(id).cloned().ok_or_else(|| {
            DrasiError::component_not_found(
                "reaction config",
                &format!("{} (was it created via with_reaction()?)", id),
            )
        })
    }

    // ============================================================================
    // Internal Methods
    // ============================================================================
    // Note: Direct manager/router access methods were removed as they are not used
    // internally. Managers are accessed directly via self.source_manager, etc.

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
}

// Implement QuerySubscriber trait for DrasiLib
// This breaks the circular dependency by providing a minimal interface for reactions
#[async_trait::async_trait]
impl crate::plugin_core::QuerySubscriber for DrasiLib {
    async fn get_query_instance(&self, id: &str) -> Result<Arc<dyn crate::queries::Query>> {
        self.query_manager
            .get_query_instance(id)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_test_server() -> DrasiLib {
        let config_yaml = r#"
server_core:
  id: test-server

sources: []
queries: []
reactions: []
"#;
        DrasiLib::from_config_str(config_yaml)
            .await
            .expect("Failed to build server")
    }

    #[tokio::test]
    async fn test_middleware_registry_is_initialized() {
        let core = create_test_server().await;

        let registry = core.middleware_registry();

        // Verify that all 7 standard middleware factories are registered
        assert!(
            registry.get("jq").is_some(),
            "JQ factory should be registered"
        );
        assert!(
            registry.get("map").is_some(),
            "Map factory should be registered"
        );
        assert!(
            registry.get("unwind").is_some(),
            "Unwind factory should be registered"
        );
        assert!(
            registry.get("relabel").is_some(),
            "Relabel factory should be registered"
        );
        assert!(
            registry.get("decoder").is_some(),
            "Decoder factory should be registered"
        );
        assert!(
            registry.get("parse_json").is_some(),
            "ParseJson factory should be registered"
        );
        assert!(
            registry.get("promote").is_some(),
            "Promote factory should be registered"
        );
    }

    #[tokio::test]
    async fn test_middleware_registry_arc_sharing() {
        let core = create_test_server().await;

        let registry1 = core.middleware_registry();
        let registry2 = core.middleware_registry();

        // Both Arc instances should point to the same underlying registry
        // We can't directly test Arc equality, but we can verify both work
        assert!(registry1.get("jq").is_some());
        assert!(registry2.get("jq").is_some());
    }

    #[tokio::test]
    async fn test_middleware_registry_accessible_before_start() {
        let core = create_test_server().await;

        // Should be accessible even before server is started
        assert!(!core.is_running().await);
        let registry = core.middleware_registry();
        assert!(registry.get("jq").is_some());
    }

    #[tokio::test]
    async fn test_middleware_registry_accessible_after_start() {
        let core = create_test_server().await;

        core.start().await.expect("Failed to start server");

        // Should be accessible after server is started
        assert!(core.is_running().await);
        let registry = core.middleware_registry();
        assert!(registry.get("jq").is_some());

        core.stop().await.expect("Failed to stop server");
    }
}
