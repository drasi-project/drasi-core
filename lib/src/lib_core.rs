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
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::*;
use crate::config::{DrasiLibConfig, RuntimeConfig};
use crate::inspection::InspectionAPI;
use crate::lifecycle::LifecycleManager;
use crate::managers::ComponentLogRegistry;
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
/// 1. **Created** (via `builder()`)
/// 2. **Initialized** (one-time setup, automatic with builder)
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
/// use drasi_lib::{DrasiLib, Query};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder()
///     .with_id("my-server")
///     .with_source(my_source)  // Pre-built source instance
///     .with_query(
///         Query::cypher("my-query")
///             .query("MATCH (n:Person) RETURN n.name, n.age")
///             .from_source("events")
///             .auto_start(true)
///             .build()
///     )
///     .with_reaction(my_reaction)  // Pre-built reaction instance
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
/// ## Dynamic Runtime Configuration
///
/// ```ignore
/// use drasi_lib::{DrasiLib, Query};
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
/// core.add_source(new_source_instance).await?;
///
/// core.add_query(
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
/// When you call `stop()` and then `start()` again, only components with `auto_start=true`
/// will be started. Components that were manually started (with `auto_start=false`) will
/// remain stopped:
///
/// ```no_run
/// # use drasi_lib::DrasiLib;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiLib::builder().build().await?;
/// core.start().await?;
/// // ... only auto_start=true components are running ...
///
/// core.stop().await?;
/// // ... all components stopped ...
///
/// core.start().await?;
/// // ... only auto_start=true components restarted ...
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
    // Inspection API for querying server state
    pub(crate) inspection: InspectionAPI,
    // Lifecycle manager for orchestrating component lifecycle
    pub(crate) lifecycle: Arc<RwLock<LifecycleManager>>,
    // Middleware registry for source middleware
    pub(crate) middleware_registry: Arc<MiddlewareTypeRegistry>,
    // Component log registry for live log streaming
    pub(crate) log_registry: Arc<ComponentLogRegistry>,
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
            inspection: self.inspection.clone(),
            lifecycle: Arc::clone(&self.lifecycle),
            middleware_registry: Arc::clone(&self.middleware_registry),
            log_registry: Arc::clone(&self.log_registry),
        }
    }
}

impl DrasiLib {
    // ============================================================================
    // Construction and Initialization
    // ============================================================================

    /// Create an Arc-wrapped reference to self
    ///
    /// Since DrasiLib contains all Arc-wrapped fields, cloning is cheap
    /// (just increments ref counts), but this helper makes the intent clearer
    /// and provides a single place to document this pattern.
    pub(crate) fn as_arc(&self) -> Arc<Self> {
        Arc::new(self.clone())
    }

    /// Internal constructor - creates uninitialized server
    /// Use `builder()` instead
    pub(crate) fn new(config: Arc<RuntimeConfig>) -> Self {
        let (channels, receivers) = EventChannels::new();

        // Use the shared global log registry.
        // Since tracing uses a single global subscriber, all DrasiLib instances
        // share the same log registry. This ensures logs are properly routed
        // regardless of how many DrasiLib instances are created.
        let log_registry = crate::managers::get_or_init_global_registry();

        // Get the instance ID from config for log routing
        let instance_id = config.id.clone();

        let source_manager = Arc::new(SourceManager::new(
            &instance_id,
            channels.component_event_tx.clone(),
            log_registry.clone(),
        ));

        // Initialize middleware registry and register all standard middleware factories
        let mut middleware_registry = MiddlewareTypeRegistry::new();

        #[cfg(feature = "middleware-jq")]
        middleware_registry.register(Arc::new(drasi_middleware::jq::JQFactory::new()));

        #[cfg(feature = "middleware-map")]
        middleware_registry.register(Arc::new(drasi_middleware::map::MapFactory::new()));

        #[cfg(feature = "middleware-unwind")]
        middleware_registry.register(Arc::new(drasi_middleware::unwind::UnwindFactory::new()));

        #[cfg(feature = "middleware-relabel")]
        middleware_registry.register(Arc::new(
            drasi_middleware::relabel::RelabelMiddlewareFactory::new(),
        ));

        #[cfg(feature = "middleware-decoder")]
        middleware_registry.register(Arc::new(drasi_middleware::decoder::DecoderFactory::new()));

        #[cfg(feature = "middleware-parse-json")]
        middleware_registry.register(Arc::new(
            drasi_middleware::parse_json::ParseJsonFactory::new(),
        ));

        #[cfg(feature = "middleware-promote")]
        middleware_registry.register(Arc::new(
            drasi_middleware::promote::PromoteMiddlewareFactory::new(),
        ));

        let middleware_registry = Arc::new(middleware_registry);

        let query_manager = Arc::new(QueryManager::new(
            &instance_id,
            channels.component_event_tx.clone(),
            source_manager.clone(),
            config.index_factory.clone(),
            middleware_registry.clone(),
            log_registry.clone(),
        ));

        let reaction_manager = Arc::new(ReactionManager::new(
            &instance_id,
            channels.component_event_tx.clone(),
            log_registry.clone(),
        ));

        let state_guard = StateGuard::new();

        let inspection = InspectionAPI::new(
            source_manager.clone(),
            query_manager.clone(),
            reaction_manager.clone(),
            state_guard.clone(),
            config.clone(),
        );

        let lifecycle = Arc::new(RwLock::new(LifecycleManager::new(
            config.clone(),
            source_manager.clone(),
            query_manager.clone(),
            reaction_manager.clone(),
            Some(receivers),
        )));

        Self {
            config,
            source_manager,
            query_manager,
            reaction_manager,
            running: Arc::new(RwLock::new(false)),
            state_guard,
            inspection,
            lifecycle,
            middleware_registry,
            log_registry,
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

        // Inject QueryProvider into ReactionManager
        // This allows reactions to access queries when they start
        let query_provider: Arc<dyn crate::reactions::QueryProvider> = self.as_arc();
        self.reaction_manager
            .inject_query_provider(query_provider)
            .await;

        // Inject StateStoreProvider into SourceManager and ReactionManager
        // This allows sources and reactions to persist state
        let state_store = self.config.state_store_provider.clone();
        self.source_manager
            .inject_state_store(state_store.clone())
            .await;
        self.reaction_manager.inject_state_store(state_store).await;

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

    // ============================================================================
    // Lifecycle Operations (start/stop)
    // ============================================================================

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
        lifecycle.start_components().await?;

        *running = true;
        info!("Drasi Server Core started successfully");

        Ok(())
    }

    /// Stop the server and all running components
    ///
    /// This stops all currently running components (sources, queries, reactions).
    /// Components are stopped in reverse dependency order: Reactions → Queries → Sources
    ///
    /// On the next `start()`, only components with `auto_start=true` will be restarted.
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
    /// // Only auto_start=true components will be restarted
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

        // Stop all components
        let lifecycle = self.lifecycle.read().await;
        lifecycle.stop_all_components().await?;

        *running = false;
        info!("Drasi Server Core stopped successfully");

        Ok(())
    }

    // ============================================================================
    // Handle Access (for advanced usage)
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
    // Configuration Snapshot
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
    ///     .with_query(
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
    /// println!("Server ID: {}", config.id);
    /// println!("Number of queries: {}", config.queries.len());
    /// # Ok(())
    /// # }
    /// ```
    pub fn get_config(&self) -> &RuntimeConfig {
        &self.config
    }
}

// ============================================================================
// QueryProvider Trait Implementation
// ============================================================================

// Implement QueryProvider trait for DrasiLib
// This breaks the circular dependency by providing a minimal interface for reactions
#[async_trait::async_trait]
impl crate::reactions::QueryProvider for DrasiLib {
    async fn get_query_instance(&self, id: &str) -> Result<Arc<dyn crate::queries::Query>> {
        self.query_manager
            .get_query_instance(id)
            .await
            .map_err(|e| anyhow::anyhow!(e))
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    async fn create_test_server() -> DrasiLib {
        DrasiLib::builder()
            .with_id("test-server")
            .build()
            .await
            .expect("Failed to build server")
    }

    #[tokio::test]
    async fn test_middleware_registry_is_initialized() {
        let core = create_test_server().await;

        let registry = core.middleware_registry();

        // Verify that middleware factories are registered when their features are enabled
        #[cfg(feature = "middleware-jq")]
        assert!(
            registry.get("jq").is_some(),
            "JQ factory should be registered"
        );
        #[cfg(feature = "middleware-map")]
        assert!(
            registry.get("map").is_some(),
            "Map factory should be registered"
        );
        #[cfg(feature = "middleware-unwind")]
        assert!(
            registry.get("unwind").is_some(),
            "Unwind factory should be registered"
        );
        #[cfg(feature = "middleware-relabel")]
        assert!(
            registry.get("relabel").is_some(),
            "Relabel factory should be registered"
        );
        #[cfg(feature = "middleware-decoder")]
        assert!(
            registry.get("decoder").is_some(),
            "Decoder factory should be registered"
        );
        #[cfg(feature = "middleware-parse-json")]
        assert!(
            registry.get("parse_json").is_some(),
            "ParseJson factory should be registered"
        );
        #[cfg(feature = "middleware-promote")]
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
        // Test with any available middleware feature
        #[cfg(feature = "middleware-jq")]
        {
            assert!(registry1.get("jq").is_some());
            assert!(registry2.get("jq").is_some());
        }
        #[cfg(all(feature = "middleware-map", not(feature = "middleware-jq")))]
        {
            assert!(registry1.get("map").is_some());
            assert!(registry2.get("map").is_some());
        }
        #[cfg(all(
            feature = "middleware-decoder",
            not(feature = "middleware-jq"),
            not(feature = "middleware-map")
        ))]
        {
            assert!(registry1.get("decoder").is_some());
            assert!(registry2.get("decoder").is_some());
        }
    }

    #[tokio::test]
    async fn test_middleware_registry_accessible_before_start() {
        let core = create_test_server().await;

        // Should be accessible even before server is started
        assert!(!core.is_running().await);
        let registry = core.middleware_registry();

        // Verify registry is accessible (test with any available middleware)
        #[cfg(feature = "middleware-jq")]
        assert!(registry.get("jq").is_some());
        #[cfg(all(feature = "middleware-map", not(feature = "middleware-jq")))]
        assert!(registry.get("map").is_some());

        // If no middleware features are enabled, just verify the registry exists
        #[cfg(not(any(
            feature = "middleware-jq",
            feature = "middleware-map",
            feature = "middleware-decoder",
            feature = "middleware-parse-json",
            feature = "middleware-promote",
            feature = "middleware-relabel",
            feature = "middleware-unwind"
        )))]
        {
            // Registry should exist even with no middleware
            let _ = registry;
        }
    }

    #[tokio::test]
    async fn test_middleware_registry_accessible_after_start() {
        let core = create_test_server().await;

        core.start().await.expect("Failed to start server");

        // Should be accessible after server is started
        assert!(core.is_running().await);
        let registry = core.middleware_registry();

        // Verify registry is accessible (test with any available middleware)
        #[cfg(feature = "middleware-jq")]
        assert!(registry.get("jq").is_some());
        #[cfg(all(feature = "middleware-map", not(feature = "middleware-jq")))]
        assert!(registry.get("map").is_some());

        // If no middleware features are enabled, just verify the registry exists
        #[cfg(not(any(
            feature = "middleware-jq",
            feature = "middleware-map",
            feature = "middleware-decoder",
            feature = "middleware-parse-json",
            feature = "middleware-promote",
            feature = "middleware-relabel",
            feature = "middleware-unwind"
        )))]
        {
            // Registry should exist even with no middleware
            let _ = registry;
        }

        core.stop().await.expect("Failed to stop server");
    }
}
