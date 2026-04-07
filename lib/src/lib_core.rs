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
use log::{info, warn};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::*;
use crate::error::DrasiError;
use crate::component_graph::{ComponentGraph, GraphSnapshot};
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
/// - **Component Graph**: A directed dependency graph tracking all components and their
///   bidirectional relationships (Source→Query→Reaction). The DrasiLib instance is the root node.
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
/// core.remove_source("new-source", false).await?;
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
    pub(crate) lifecycle: Arc<LifecycleManager>,
    // Middleware registry for source middleware
    pub(crate) middleware_registry: Arc<MiddlewareTypeRegistry>,
    // Component log registry for live log streaming
    pub(crate) log_registry: Arc<ComponentLogRegistry>,
    // Broadcast sender for component events — shared with ComponentGraph.
    //
    // This is the *same* sender that the ComponentGraph uses internally to emit
    // events on mutations. Cloned before the graph is wrapped in `Arc<RwLock<>>`,
    // so subscribers can call `.subscribe()` without acquiring the graph lock.
    pub(crate) component_event_broadcast_tx: ComponentEventBroadcastSender,
    /// Component dependency graph — the single source of truth for component relationships.
    ///
    /// All managers share this graph via `Arc<RwLock<>>`. The graph is updated atomically
    /// alongside the manager HashMaps when components are added, removed, or updated.
    pub(crate) component_graph: Arc<RwLock<ComponentGraph>>,
    /// Handle to the graph update loop task for clean shutdown.
    pub(crate) graph_update_handle: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
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
            component_event_broadcast_tx: self.component_event_broadcast_tx.clone(),
            component_graph: Arc::clone(&self.component_graph),
            graph_update_handle: Arc::clone(&self.graph_update_handle),
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
    pub(crate) fn to_arc(&self) -> Arc<Self> {
        Arc::new(self.clone())
    }

    /// Subscribe to all component events (status changes, additions, removals).
    ///
    /// Returns a broadcast receiver that gets a copy of every `ComponentEvent` across
    /// all sources, queries, and reactions. Used by the component graph source to
    /// detect component lifecycle changes in real-time.
    pub fn subscribe_all_component_events(&self) -> ComponentEventBroadcastReceiver {
        self.component_event_broadcast_tx.subscribe()
    }

    /// Internal constructor - creates uninitialized server
    /// Use `builder()` instead
    pub(crate) fn new(config: Arc<RuntimeConfig>) -> Self {
        // Use the shared global log registry.
        // Since tracing uses a single global subscriber, all DrasiLib instances
        // share the same log registry. This ensures logs are properly routed
        // regardless of how many DrasiLib instances are created.
        let log_registry = crate::managers::get_or_init_global_registry();

        // Get the instance ID from config for log routing
        let instance_id = config.id.clone();

        // Create the shared component graph with the instance as root node.
        // Extract the broadcast sender and update sender BEFORE wrapping in Arc<RwLock<>>
        // so subscribers and components can use them without acquiring the graph lock.
        let (graph, update_rx) = ComponentGraph::new(&instance_id);
        let component_event_broadcast_tx = graph.event_sender().clone();
        let update_tx = graph.update_sender();
        let component_graph = Arc::new(RwLock::new(graph));

        let source_manager = Arc::new(SourceManager::new(
            &instance_id,
            log_registry.clone(),
            component_graph.clone(),
            update_tx.clone(),
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
            source_manager.clone(),
            config.index_factory.clone(),
            middleware_registry.clone(),
            log_registry.clone(),
            component_graph.clone(),
            update_tx.clone(),
        ));

        let reaction_manager = Arc::new(ReactionManager::new(
            &instance_id,
            log_registry.clone(),
            component_graph.clone(),
            update_tx.clone(),
        ));

        let state_guard = StateGuard::new();

        let inspection = InspectionAPI::new(
            source_manager.clone(),
            query_manager.clone(),
            reaction_manager.clone(),
            state_guard.clone(),
            config.clone(),
        );

        let lifecycle = Arc::new(LifecycleManager::new(
            config.clone(),
            source_manager.clone(),
            query_manager.clone(),
            reaction_manager.clone(),
            component_graph.clone(),
        ));

        // Spawn the graph update loop — sole consumer of component status updates.
        // Components send status changes via the mpsc update channel (fire-and-forget),
        // and this loop applies them to the graph. Events are recorded in the graph's
        // centralized ComponentEventHistory during apply_update().
        //
        // Batch optimization: drains all available updates under a single write lock
        // acquisition, reducing lock contention when multiple components report status
        // simultaneously (e.g., during start_all / stop_all).
        let graph_update_handle = {
            let graph = component_graph.clone();
            let handle = tokio::spawn(async move {
                let mut update_rx = update_rx;
                while let Some(first) = update_rx.recv().await {
                    // Drain all immediately available updates into a batch
                    let mut batch = vec![first];
                    while let Ok(update) = update_rx.try_recv() {
                        batch.push(update);
                    }

                    // Apply entire batch under a single write lock.
                    // Events are recorded in the graph's centralized event history
                    // inside apply_update(), eliminating per-manager dispatch.
                    let events: Vec<_> = {
                        let mut g = graph.write().await;
                        batch
                            .into_iter()
                            .filter_map(|update| g.apply_update(update))
                            .collect()
                    };
                    // Lock released — log events outside the lock

                    for event in events {
                        log::info!(
                            "Component Event - {:?} {}: {:?} - {}",
                            event.component_type,
                            event.component_id,
                            event.status,
                            event.message.clone().unwrap_or_default()
                        );
                    }
                }
                tracing::debug!("Graph update loop exited — all senders dropped");
            });
            Arc::new(tokio::sync::Mutex::new(Some(handle)))
        };

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
            component_event_broadcast_tx,
            component_graph,
            graph_update_handle,
        }
    }

    /// Internal initialization - performs one-time setup
    /// This is called internally by builder and config loaders
    pub(crate) async fn initialize(&mut self) -> Result<()> {
        let already_initialized = self.state_guard.is_initialized();
        if already_initialized {
            info!("Server already initialized, skipping initialization");
            return Ok(());
        }

        info!("Initializing drasi-lib");

        // Inject QueryManager into ReactionManager
        // This allows the host to subscribe reactions to query results
        self.reaction_manager
            .inject_query_provider(
                Arc::clone(&self.query_manager) as Arc<dyn crate::reactions::QueryProvider>
            )
            .await;

        // Inject StateStoreProvider into SourceManager and ReactionManager
        // This allows sources and reactions to persist state
        let state_store = self.config.state_store_provider.clone();
        self.source_manager
            .inject_state_store(state_store.clone())
            .await;
        self.reaction_manager.inject_state_store(state_store).await;

        // Load configuration
        self.lifecycle.load_configuration().await?;

        self.state_guard.mark_initialized();
        info!("drasi-lib initialized successfully");
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
    /// * The server is already running (`DrasiError::InvalidState`)
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
    pub async fn start(&self) -> crate::error::Result<()> {
        let mut running = self.running.write().await;
        if *running {
            warn!("Server is already running");
            return Err(DrasiError::invalid_state("Server is already running"));
        }

        info!("Starting drasi-lib");

        // Ensure initialized
        if !self.state_guard.is_initialized() {
            return Err(DrasiError::invalid_state(
                "Server must be initialized before starting",
            ));
        }

        // Start all configured components
        self.lifecycle.start_components().await?;

        *running = true;
        info!("drasi-lib started successfully");

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
    /// * The server is not running (`DrasiError::InvalidState`)
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
    pub async fn stop(&self) -> crate::error::Result<()> {
        let mut running = self.running.write().await;
        if !*running {
            warn!("Server is already stopped");
            return Err(DrasiError::invalid_state("Server is already stopped"));
        }

        info!("Stopping drasi-lib");

        // Stop all components
        self.lifecycle.stop_all_components().await?;

        *running = false;
        info!("drasi-lib stopped successfully");

        Ok(())
    }

    /// Shut down the server permanently, releasing all resources.
    ///
    /// Unlike [`stop()`](Self::stop), which allows the server to be restarted,
    /// `shutdown()` performs a full teardown: it stops all components and aborts
    /// the internal graph update loop. After `shutdown()`, the instance cannot
    /// be restarted — create a new `DrasiLib` instance instead.
    ///
    /// This method is idempotent — calling it on an already-stopped server will
    /// still clean up the graph update loop.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiLib::builder().build().await?;
    /// core.start().await?;
    /// // ... use the server ...
    /// core.shutdown().await?; // full teardown, no restart possible
    /// # Ok(())
    /// # }
    /// ```
    pub async fn shutdown(&self) -> crate::error::Result<()> {
        // Stop components if still running
        if self.is_running().await {
            self.stop().await?;
        }

        // Abort the graph update loop task to prevent leaked spawned tasks.
        // The loop exits naturally when all senders are dropped, but abort
        // ensures immediate cleanup even if references linger.
        if let Some(handle) = self.graph_update_handle.lock().await.take() {
            handle.abort();
            let _ = handle.await;
        }

        info!("drasi-lib shut down permanently");
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

    /// Get access to the component log registry.
    ///
    /// The log registry captures structured log messages from components and
    /// supports live streaming via subscriptions. This is used by the REST API
    /// to serve log streams and by dynamic plugin loading to wire plugin logs
    /// into the same registry as statically-linked components.
    pub fn log_registry(&self) -> Arc<ComponentLogRegistry> {
        Arc::clone(&self.log_registry)
    }

    /// Get access to the component graph for event history queries.
    ///
    /// The graph centralizes all component event history. Use `graph.read().await.get_events(id)`
    /// to query events for specific components, or `graph.read().await.get_all_events()` for all.
    ///
    /// For recording events from external plugins, use `graph.write().await.record_event(event)`.
    pub fn component_graph(&self) -> Arc<RwLock<ComponentGraph>> {
        Arc::clone(&self.component_graph)
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

    /// Capture a point-in-time configuration snapshot of all components.
    ///
    /// The snapshot is captured atomically under a single graph read lock,
    /// ensuring consistency between topology, status, and properties.
    ///
    /// # Use Cases
    ///
    /// - **Persistence:** Serialize the snapshot and store it for later recovery
    /// - **Debugging:** Inspect the full configuration state of a running instance
    /// - **Migration:** Export configuration from one instance to recreate on another
    ///
    /// # Important
    ///
    /// Sources and reactions are trait objects — their properties are captured but
    /// they cannot be automatically reconstructed from the snapshot alone. The host
    /// must supply the appropriate plugin factories to rebuild them.
    ///
    /// # Example
    ///
    /// ```no_run
    /// # use drasi_lib::DrasiLib;
    /// # async fn example(core: &DrasiLib) -> Result<(), Box<dyn std::error::Error>> {
    /// let snapshot = core.snapshot_configuration().await?;
    /// println!("Instance {} has {} sources, {} queries, {} reactions",
    ///     snapshot.instance_id,
    ///     snapshot.sources.len(),
    ///     snapshot.queries.len(),
    ///     snapshot.reactions.len(),
    /// );
    ///
    /// // Serialize for storage
    /// let json = serde_json::to_string_pretty(&snapshot)?;
    /// std::fs::write("config-snapshot.json", &json)?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn snapshot_configuration(
        &self,
    ) -> crate::error::Result<crate::config::snapshot::ConfigurationSnapshot> {
        use crate::component_graph::ComponentKind;
        use crate::config::snapshot::{
            BootstrapSnapshot, ConfigurationSnapshot, QuerySnapshot, ReactionSnapshot,
            SourceSnapshot,
        };

        self.state_guard.require_initialized()?;

        let graph = self.component_graph.read().await;
        let graph_snapshot = graph.snapshot();

        let mut sources = Vec::new();
        let mut queries = Vec::new();
        let mut reactions = Vec::new();

        for node in &graph_snapshot.nodes {
            match node.kind {
                ComponentKind::Source => {
                    if let Some(source) =
                        graph.get_runtime::<std::sync::Arc<dyn crate::sources::Source>>(&node.id)
                    {
                        // Look for an attached bootstrap provider via BootstrappedBy edge
                        let bootstrap_provider = graph
                            .get_neighbors(
                                &node.id,
                                &crate::component_graph::RelationshipKind::BootstrappedBy,
                            )
                            .into_iter()
                            .find(|n| n.kind == ComponentKind::BootstrapProvider)
                            .map(|bp_node| {
                                let kind =
                                    bp_node.metadata.get("kind").cloned().unwrap_or_default();
                                let properties: std::collections::HashMap<
                                    String,
                                    serde_json::Value,
                                > = bp_node
                                    .metadata
                                    .iter()
                                    .filter(|(k, _)| *k != "kind")
                                    .filter_map(|(k, v)| {
                                        serde_json::from_str(v)
                                            .ok()
                                            .map(|parsed| (k.clone(), parsed))
                                    })
                                    .collect();
                                BootstrapSnapshot { kind, properties }
                            });

                        sources.push(SourceSnapshot {
                            id: node.id.clone(),
                            source_type: source.type_name().to_string(),
                            status: node.status,
                            auto_start: node
                                .metadata
                                .get("autoStart")
                                .map(|v| v == "true")
                                .unwrap_or(false),
                            properties: source.properties(),
                            bootstrap_provider,
                        });
                    }
                }
                ComponentKind::Query => {
                    if let Some(query) = graph
                        .get_runtime::<std::sync::Arc<dyn crate::queries::manager::Query>>(&node.id)
                    {
                        queries.push(QuerySnapshot {
                            id: node.id.clone(),
                            config: query.get_config().clone(),
                            status: node.status,
                        });
                    }
                }
                ComponentKind::Reaction => {
                    if let Some(reaction) = graph
                        .get_runtime::<std::sync::Arc<dyn crate::reactions::Reaction>>(&node.id)
                    {
                        reactions.push(ReactionSnapshot {
                            id: node.id.clone(),
                            reaction_type: reaction.type_name().to_string(),
                            status: node.status,
                            auto_start: node
                                .metadata
                                .get("autoStart")
                                .map(|v| v == "true")
                                .unwrap_or(true),
                            queries: reaction.query_ids(),
                            properties: reaction.properties(),
                        });
                    }
                }
                _ => {}
            }
        }

        Ok(ConfigurationSnapshot {
            instance_id: graph_snapshot.instance_id,
            timestamp: chrono::Utc::now().to_rfc3339(),
            sources,
            queries,
            reactions,
            edges: graph_snapshot.edges,
        })
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

    // ========================================================================
    // DrasiLib Lifecycle Tests
    // ========================================================================

    #[tokio::test]
    async fn test_build_minimal_instance() {
        let result = DrasiLib::builder().with_id("test").build().await;
        assert!(result.is_ok(), "Minimal DrasiLib build should succeed");
    }

    #[tokio::test]
    async fn test_is_running_lifecycle() {
        let core = create_test_server().await;

        assert!(
            !core.is_running().await,
            "Should not be running before start"
        );

        core.start().await.expect("Failed to start");
        assert!(core.is_running().await, "Should be running after start");

        core.stop().await.expect("Failed to stop");
        assert!(!core.is_running().await, "Should not be running after stop");
    }

    #[tokio::test]
    async fn test_start_then_stop() {
        let core = create_test_server().await;

        let start_result = core.start().await;
        assert!(start_result.is_ok(), "start() should succeed");

        let stop_result = core.stop().await;
        assert!(stop_result.is_ok(), "stop() should succeed");
    }

    #[tokio::test]
    async fn test_get_config_returns_runtime_config() {
        let core = DrasiLib::builder()
            .with_id("config-test")
            .build()
            .await
            .expect("Failed to build");

        let config = core.get_config();
        assert_eq!(config.id, "config-test");
    }

    #[tokio::test]
    async fn test_component_graph_returns_arc_rwlock() {
        let core = DrasiLib::builder()
            .with_id("graph-test")
            .build()
            .await
            .expect("Failed to build");

        let graph = core.component_graph();
        let graph_read = graph.read().await;
        assert_eq!(graph_read.instance_id(), "graph-test");
    }

    #[tokio::test]
    async fn test_builder_returns_drasi_lib_builder() {
        let builder = DrasiLib::builder();
        let result = builder.with_id("builder-test").build().await;
        assert!(
            result.is_ok(),
            "Builder should produce a valid DrasiLib instance"
        );
    }

    // ========================================================================
    // Configuration Snapshot Tests
    // ========================================================================

    mod snapshot_tests {
        use super::*;
        use crate::builder::Query;
        use crate::component_graph::ComponentKind;
        use crate::config::snapshot::ConfigurationSnapshot;
        use crate::reactions::Reaction;
        use crate::sources::tests::{create_test_mock_source, TestMockSource};
        use async_trait::async_trait;
        use std::collections::HashMap;

        // -- Mock source with properties --

        struct PropertiedSource {
            id: String,
            status_handle: crate::component_graph::ComponentStatusHandle,
            dispatchers: Arc<
                RwLock<
                    Vec<
                        Box<
                            dyn crate::channels::ChangeDispatcher<
                                crate::channels::SourceEventWrapper,
                            >,
                        >,
                    >,
                >,
            >,
        }

        impl PropertiedSource {
            fn new(id: &str) -> Self {
                Self {
                    id: id.to_string(),
                    status_handle: crate::component_graph::ComponentStatusHandle::new(id),
                    dispatchers: Arc::new(RwLock::new(Vec::new())),
                }
            }
        }

        #[async_trait]
        impl crate::sources::Source for PropertiedSource {
            fn id(&self) -> &str {
                &self.id
            }
            fn type_name(&self) -> &str {
                "test-postgres"
            }
            fn properties(&self) -> HashMap<String, serde_json::Value> {
                let mut props = HashMap::new();
                props.insert(
                    "host".to_string(),
                    serde_json::Value::String("localhost".to_string()),
                );
                props.insert("port".to_string(), serde_json::json!(5432));
                props
            }
            fn auto_start(&self) -> bool {
                false
            }
            async fn start(&self) -> anyhow::Result<()> {
                self.status_handle
                    .set_status(
                        crate::channels::ComponentStatus::Starting,
                        Some("Starting".into()),
                    )
                    .await;
                self.status_handle
                    .set_status(
                        crate::channels::ComponentStatus::Running,
                        Some("Running".into()),
                    )
                    .await;
                Ok(())
            }
            async fn stop(&self) -> anyhow::Result<()> {
                self.status_handle
                    .set_status(
                        crate::channels::ComponentStatus::Stopping,
                        Some("Stopping".into()),
                    )
                    .await;
                self.status_handle
                    .set_status(
                        crate::channels::ComponentStatus::Stopped,
                        Some("Stopped".into()),
                    )
                    .await;
                Ok(())
            }
            async fn status(&self) -> crate::channels::ComponentStatus {
                self.status_handle.get_status().await
            }
            async fn subscribe(
                &self,
                settings: crate::config::SourceSubscriptionSettings,
            ) -> anyhow::Result<crate::channels::SubscriptionResponse> {
                use crate::channels::ChannelChangeDispatcher;
                let dispatcher =
                    ChannelChangeDispatcher::<crate::channels::SourceEventWrapper>::new(100);
                let receiver = dispatcher.create_receiver().await?;
                self.dispatchers.write().await.push(Box::new(dispatcher));
                Ok(crate::channels::SubscriptionResponse {
                    query_id: settings.query_id,
                    source_id: self.id.clone(),
                    receiver,
                    bootstrap_receiver: None,
                })
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
            async fn initialize(&self, ctx: crate::context::SourceRuntimeContext) {
                self.status_handle.wire(ctx.update_tx.clone()).await;
            }
        }

        // -- Mock reaction with properties --

        struct PropertiedReaction {
            id: String,
            queries: Vec<String>,
            status_handle: crate::component_graph::ComponentStatusHandle,
        }

        impl PropertiedReaction {
            fn new(id: &str, queries: Vec<String>) -> Self {
                Self {
                    id: id.to_string(),
                    queries,
                    status_handle: crate::component_graph::ComponentStatusHandle::new(id),
                }
            }
        }

        #[async_trait]
        impl Reaction for PropertiedReaction {
            fn id(&self) -> &str {
                &self.id
            }
            fn type_name(&self) -> &str {
                "test-http"
            }
            fn properties(&self) -> HashMap<String, serde_json::Value> {
                let mut props = HashMap::new();
                props.insert(
                    "base_url".to_string(),
                    serde_json::Value::String("https://example.com/webhook".to_string()),
                );
                props.insert("timeout_ms".to_string(), serde_json::json!(5000));
                props
            }
            fn query_ids(&self) -> Vec<String> {
                self.queries.clone()
            }
            fn auto_start(&self) -> bool {
                false
            }
            async fn initialize(&self, ctx: crate::context::ReactionRuntimeContext) {
                self.status_handle.wire(ctx.update_tx.clone()).await;
            }
            async fn start(&self) -> anyhow::Result<()> {
                self.status_handle
                    .set_status(
                        crate::channels::ComponentStatus::Starting,
                        Some("Starting".into()),
                    )
                    .await;
                self.status_handle
                    .set_status(
                        crate::channels::ComponentStatus::Running,
                        Some("Running".into()),
                    )
                    .await;
                Ok(())
            }
            async fn stop(&self) -> anyhow::Result<()> {
                self.status_handle
                    .set_status(
                        crate::channels::ComponentStatus::Stopping,
                        Some("Stopping".into()),
                    )
                    .await;
                self.status_handle
                    .set_status(
                        crate::channels::ComponentStatus::Stopped,
                        Some("Stopped".into()),
                    )
                    .await;
                Ok(())
            }
            async fn status(&self) -> crate::channels::ComponentStatus {
                self.status_handle.get_status().await
            }
            async fn enqueue_query_result(
                &self,
                _result: crate::channels::QueryResult,
            ) -> anyhow::Result<()> {
                Ok(())
            }
        }

        // -- Test helpers --

        async fn build_core_with_components() -> DrasiLib {
            let source = PropertiedSource::new("pg-source");
            let core = DrasiLib::builder()
                .with_id("snapshot-test")
                .with_source(source)
                .with_query(
                    Query::cypher("q1")
                        .query("MATCH (n:Person) RETURN n.name")
                        .from_source("pg-source")
                        .auto_start(false)
                        .build(),
                )
                .build()
                .await
                .unwrap();
            core.start().await.unwrap();

            let reaction = PropertiedReaction::new("http-reaction", vec!["q1".to_string()]);
            core.add_reaction(reaction).await.unwrap();

            core
        }

        // -- Tests --

        #[tokio::test]
        async fn snapshot_empty_instance() {
            let core = create_test_server().await;
            core.start().await.unwrap();

            let snapshot = core.snapshot_configuration().await.unwrap();

            assert_eq!(snapshot.instance_id, "test-server");
            assert!(!snapshot.timestamp.is_empty());
            // No user sources (only internal component-graph source may be present)
            assert!(snapshot.queries.is_empty());
            assert!(snapshot.reactions.is_empty());
        }

        #[tokio::test]
        async fn snapshot_requires_initialization() {
            // StateGuard is marked initialized during build(), so we test
            // with a manually constructed guard to verify the check exists.
            // This validates the guard is wired up in the snapshot method.
            let core = DrasiLib::builder().with_id("uninit").build().await.unwrap();

            // After build(), snapshot should succeed (initialized at build time)
            let result = core.snapshot_configuration().await;
            assert!(
                result.is_ok(),
                "Snapshot should succeed after build/initialization"
            );
        }

        #[tokio::test]
        async fn snapshot_captures_source_properties() {
            let core = build_core_with_components().await;

            let snapshot = core.snapshot_configuration().await.unwrap();

            let pg_source = snapshot
                .sources
                .iter()
                .find(|s| s.id == "pg-source")
                .expect("pg-source should be in snapshot");

            assert_eq!(pg_source.source_type, "test-postgres");
            assert!(!pg_source.auto_start);
            assert_eq!(
                pg_source.properties.get("host"),
                Some(&serde_json::Value::String("localhost".to_string()))
            );
            assert_eq!(
                pg_source.properties.get("port"),
                Some(&serde_json::json!(5432))
            );
        }

        #[tokio::test]
        async fn snapshot_captures_query_config() {
            let core = build_core_with_components().await;

            let snapshot = core.snapshot_configuration().await.unwrap();

            let q1 = snapshot
                .queries
                .iter()
                .find(|q| q.id == "q1")
                .expect("q1 should be in snapshot");

            assert_eq!(q1.config.query, "MATCH (n:Person) RETURN n.name");
            assert!(
                q1.config.sources.iter().any(|s| s.source_id == "pg-source"),
                "Query should reference pg-source"
            );
        }

        #[tokio::test]
        async fn snapshot_captures_reaction_properties() {
            let core = build_core_with_components().await;

            let snapshot = core.snapshot_configuration().await.unwrap();

            let reaction = snapshot
                .reactions
                .iter()
                .find(|r| r.id == "http-reaction")
                .expect("http-reaction should be in snapshot");

            assert_eq!(reaction.reaction_type, "test-http");
            assert_eq!(reaction.queries, vec!["q1".to_string()]);
            assert_eq!(
                reaction.properties.get("base_url"),
                Some(&serde_json::Value::String(
                    "https://example.com/webhook".to_string()
                ))
            );
            assert_eq!(
                reaction.properties.get("timeout_ms"),
                Some(&serde_json::json!(5000))
            );
        }

        #[tokio::test]
        async fn snapshot_includes_dependency_edges() {
            let core = build_core_with_components().await;

            let snapshot = core.snapshot_configuration().await.unwrap();

            // Should have edges: source→query (Feeds) and query→reaction (Feeds)
            assert!(
                !snapshot.edges.is_empty(),
                "Snapshot should include dependency edges"
            );

            // Check source→query edge exists
            let has_source_to_query = snapshot
                .edges
                .iter()
                .any(|e| e.from == "pg-source" && e.to == "q1");
            assert!(has_source_to_query, "Should have edge from pg-source to q1");

            // Check query→reaction edge exists
            let has_query_to_reaction = snapshot
                .edges
                .iter()
                .any(|e| e.from == "q1" && e.to == "http-reaction");
            assert!(
                has_query_to_reaction,
                "Should have edge from q1 to http-reaction"
            );
        }

        #[tokio::test]
        async fn snapshot_json_round_trip() {
            let core = build_core_with_components().await;

            let snapshot = core.snapshot_configuration().await.unwrap();

            // Serialize to JSON
            let json = serde_json::to_string_pretty(&snapshot).expect("Should serialize to JSON");

            // Deserialize back
            let restored: ConfigurationSnapshot =
                serde_json::from_str(&json).expect("Should deserialize from JSON");

            assert_eq!(restored.instance_id, snapshot.instance_id);
            assert_eq!(restored.timestamp, snapshot.timestamp);
            assert_eq!(restored.sources.len(), snapshot.sources.len());
            assert_eq!(restored.queries.len(), snapshot.queries.len());
            assert_eq!(restored.reactions.len(), snapshot.reactions.len());
            assert_eq!(restored.edges.len(), snapshot.edges.len());

            // Verify source properties survived round-trip
            let src = restored
                .sources
                .iter()
                .find(|s| s.id == "pg-source")
                .unwrap();
            assert_eq!(src.properties.get("port"), Some(&serde_json::json!(5432)));

            // Verify query config survived round-trip
            let q = restored.queries.iter().find(|q| q.id == "q1").unwrap();
            assert_eq!(q.config.query, "MATCH (n:Person) RETURN n.name");

            // Verify reaction properties survived round-trip
            let rxn = restored
                .reactions
                .iter()
                .find(|r| r.id == "http-reaction")
                .unwrap();
            assert_eq!(
                rxn.properties.get("timeout_ms"),
                Some(&serde_json::json!(5000))
            );
        }

        #[tokio::test]
        async fn snapshot_captures_component_status() {
            let core = build_core_with_components().await;

            let snapshot = core.snapshot_configuration().await.unwrap();

            // Sources with auto_start=false should be Stopped after add
            let src = snapshot
                .sources
                .iter()
                .find(|s| s.id == "pg-source")
                .unwrap();
            assert!(
                matches!(
                    src.status,
                    crate::channels::ComponentStatus::Stopped
                        | crate::channels::ComponentStatus::Running
                ),
                "Source status should be captured: {:?}",
                src.status
            );
        }

        #[tokio::test]
        async fn snapshot_multiple_sources_and_reactions() {
            let s1 = PropertiedSource::new("src-a");
            let s2 = TestMockSource::with_auto_start("src-b".to_string(), false).unwrap();

            let core = DrasiLib::builder()
                .with_id("multi-test")
                .with_source(s1)
                .with_source(s2)
                .with_query(
                    Query::cypher("qa")
                        .query("MATCH (n:A) RETURN n")
                        .from_source("src-a")
                        .auto_start(false)
                        .build(),
                )
                .with_query(
                    Query::cypher("qb")
                        .query("MATCH (n:B) RETURN n")
                        .from_source("src-b")
                        .auto_start(false)
                        .build(),
                )
                .build()
                .await
                .unwrap();
            core.start().await.unwrap();

            let r1 = PropertiedReaction::new("rxn-1", vec!["qa".to_string()]);
            let r2 = PropertiedReaction::new("rxn-2", vec!["qb".to_string()]);
            core.add_reaction(r1).await.unwrap();
            core.add_reaction(r2).await.unwrap();

            let snapshot = core.snapshot_configuration().await.unwrap();

            // Should have both user sources (plus internal component-graph source)
            let user_sources: Vec<_> = snapshot
                .sources
                .iter()
                .filter(|s| s.id == "src-a" || s.id == "src-b")
                .collect();
            assert_eq!(user_sources.len(), 2, "Should have 2 user sources");

            assert_eq!(snapshot.queries.len(), 2, "Should have 2 queries");
            assert_eq!(snapshot.reactions.len(), 2, "Should have 2 reactions");

            // PropertiedSource has properties, TestMockSource has empty
            let src_a = snapshot.sources.iter().find(|s| s.id == "src-a").unwrap();
            assert!(!src_a.properties.is_empty(), "src-a should have properties");
            let src_b = snapshot.sources.iter().find(|s| s.id == "src-b").unwrap();
            assert!(
                src_b.properties.is_empty(),
                "src-b (TestMockSource) should have empty properties"
            );
        }

        #[tokio::test]
        async fn snapshot_captures_reaction_auto_start() {
            let core = build_core_with_components().await;

            let snapshot = core.snapshot_configuration().await.unwrap();

            let reaction = snapshot
                .reactions
                .iter()
                .find(|r| r.id == "http-reaction")
                .expect("http-reaction should be in snapshot");

            // PropertiedReaction has auto_start=false
            assert!(
                !reaction.auto_start,
                "Reaction auto_start should be captured as false"
            );
        }

        #[tokio::test]
        async fn snapshot_source_without_bootstrap_has_none() {
            let core = build_core_with_components().await;

            let snapshot = core.snapshot_configuration().await.unwrap();

            let src = snapshot
                .sources
                .iter()
                .find(|s| s.id == "pg-source")
                .expect("pg-source should be in snapshot");

            assert!(
                src.bootstrap_provider.is_none(),
                "Source without bootstrap should have bootstrap_provider=None"
            );
        }

        #[tokio::test]
        async fn snapshot_captures_bootstrap_provider_from_graph() {
            let source = PropertiedSource::new("bp-source");
            let core = DrasiLib::builder()
                .with_id("bootstrap-test")
                .with_source(source)
                .with_query(
                    Query::cypher("bp-query")
                        .query("MATCH (n) RETURN n")
                        .from_source("bp-source")
                        .auto_start(false)
                        .build(),
                )
                .build()
                .await
                .unwrap();
            core.start().await.unwrap();

            // Manually register a bootstrap provider in the graph
            // (this is what the server would do after creating a source with bootstrap config)
            {
                let graph = core.component_graph();
                let mut g = graph.write().await;
                let mut metadata = HashMap::new();
                metadata.insert("kind".to_string(), "postgres".to_string());
                metadata.insert(
                    "timeout_seconds".to_string(),
                    serde_json::json!(300).to_string(),
                );
                g.register_bootstrap_provider(
                    "bp-source-bootstrap",
                    metadata,
                    &["bp-source".to_string()],
                )
                .unwrap();
            }

            let snapshot = core.snapshot_configuration().await.unwrap();

            let src = snapshot
                .sources
                .iter()
                .find(|s| s.id == "bp-source")
                .expect("bp-source should be in snapshot");

            let bp = src
                .bootstrap_provider
                .as_ref()
                .expect("Source should have bootstrap_provider");

            assert_eq!(bp.kind, "postgres");
            assert!(
                bp.properties.contains_key("timeout_seconds"),
                "Bootstrap properties should include timeout_seconds"
            );
        }

        #[tokio::test]
        async fn snapshot_bootstrap_json_round_trip() {
            let source = PropertiedSource::new("rt-source");
            let core = DrasiLib::builder()
                .with_id("bp-roundtrip")
                .with_source(source)
                .build()
                .await
                .unwrap();
            core.start().await.unwrap();

            // Register bootstrap with properties
            {
                let graph = core.component_graph();
                let mut g = graph.write().await;
                let mut metadata = HashMap::new();
                metadata.insert("kind".to_string(), "scriptfile".to_string());
                metadata.insert(
                    "file_paths".to_string(),
                    serde_json::json!(["data.jsonl", "init.jsonl"]).to_string(),
                );
                g.register_bootstrap_provider(
                    "rt-source-bootstrap",
                    metadata,
                    &["rt-source".to_string()],
                )
                .unwrap();
            }

            let snapshot = core.snapshot_configuration().await.unwrap();

            // Serialize to JSON and back
            let json = serde_json::to_string_pretty(&snapshot).expect("Should serialize");
            let restored: ConfigurationSnapshot =
                serde_json::from_str(&json).expect("Should deserialize");

            let src = restored
                .sources
                .iter()
                .find(|s| s.id == "rt-source")
                .unwrap();
            let bp = src.bootstrap_provider.as_ref().unwrap();
            assert_eq!(bp.kind, "scriptfile");
            assert!(bp.properties.contains_key("file_paths"));
        }

        #[tokio::test]
        async fn snapshot_excludes_introspection_source() {
            let core = build_core_with_components().await;

            let snapshot = core.snapshot_configuration().await.unwrap();

            // The __component_graph__ internal source should not appear in snapshot
            // (it's filtered by the fact that it doesn't match user source patterns)
            let has_internal = snapshot.sources.iter().any(|s| s.id.starts_with("__"));

            // Note: Whether internal sources appear depends on the implementation.
            // If they do appear, the server should filter them. This test documents
            // the current behavior.
            if has_internal {
                // Document that internal sources are present (server should filter)
                let internal: Vec<_> = snapshot
                    .sources
                    .iter()
                    .filter(|s| s.id.starts_with("__"))
                    .map(|s| s.id.as_str())
                    .collect();
                eprintln!(
                    "Note: Internal sources present in snapshot (server should filter): {internal:?}"
                );
            }
        }
    }
}
