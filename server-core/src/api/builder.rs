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

//! Builder for DrasiServerCore

use crate::api::{DrasiError, Result};
use crate::config::{
    DrasiServerCoreConfig, DrasiServerCoreSettings, QueryConfig, ReactionConfig, RuntimeConfig,
    SourceConfig,
};
use crate::server_core::DrasiServerCore;
use std::sync::Arc;

/// Fluent builder for constructing a DrasiServerCore instance
///
/// `DrasiServerCoreBuilder` provides a type-safe, ergonomic API for configuring and building
/// a [`DrasiServerCore`] instance. It allows you to configure the server ID, performance settings,
/// and add sources, queries, and reactions before initialization.
///
/// # Builder Pattern
///
/// The builder uses a fluent API where each method returns `self`, allowing you to chain
/// configuration calls. Call `build()` at the end to create and initialize the server.
///
/// # Default Values
///
/// - **server_id**: Auto-generated UUID if not specified
/// - **priority_queue_capacity**: 10000 (can be overridden per component)
/// - **dispatch_buffer_capacity**: 1000 (can be overridden per component)
///
/// # Thread Safety
///
/// The builder is `Clone` and can be used to create multiple server instances with the
/// same configuration.
///
/// # Examples
///
/// ## Minimal Configuration
///
/// ```no_run
/// # use drasi_server_core::DrasiServerCore;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Create a server with defaults
/// let core = DrasiServerCore::builder()
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Full Configuration
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
/// # use serde_json::json;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .with_id("production-server")
///     .with_priority_queue_capacity(50000)   // Large queue for high throughput
///     .with_dispatch_buffer_capacity(5000)   // Large buffers
///     .add_source(
///         Source::postgres("orders_db")
///             .with_property("host", json!("localhost"))
///             .with_property("database", json!("orders"))
///             .auto_start(true)
///             .build()
///     )
///     .add_query(
///         Query::cypher("active_orders")
///             .query("MATCH (o:Order) WHERE o.status = 'active' RETURN o")
///             .from_source("orders_db")
///             .auto_start(true)
///             .build()
///     )
///     .add_reaction(
///         Reaction::http("order_webhook")
///             .subscribe_to("active_orders")
///             .with_property("base_url", json!("https://api.example.com/orders"))
///             .auto_start(true)
///             .build()
///     )
///     .build()
///     .await?;
///
/// core.start().await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Adding Multiple Components
///
/// ```no_run
/// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let sources = vec![
///     Source::application("source1").build(),
///     Source::application("source2").build(),
/// ];
///
/// let queries = vec![
///     Query::cypher("query1").query("MATCH (n) RETURN n").from_source("source1").build(),
///     Query::cypher("query2").query("MATCH (n) RETURN n").from_source("source2").build(),
/// ];
///
/// let core = DrasiServerCore::builder()
///     .add_sources(sources)
///     .add_queries(queries)
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct DrasiServerCoreBuilder {
    server_id: Option<String>,
    priority_queue_capacity: Option<usize>,
    dispatch_buffer_capacity: Option<usize>,
    storage_backends: Vec<crate::indexes::StorageBackendConfig>,
    sources: Vec<SourceConfig>,
    queries: Vec<QueryConfig>,
    reactions: Vec<ReactionConfig>,
}

impl DrasiServerCoreBuilder {
    /// Create a new builder with default settings
    ///
    /// # Examples
    ///
    /// ```
    /// # use drasi_server_core::DrasiServerCoreBuilder;
    /// let builder = DrasiServerCoreBuilder::new();
    /// ```
    pub fn new() -> Self {
        Self {
            server_id: None,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            storage_backends: Vec::new(),
            sources: Vec::new(),
            queries: Vec::new(),
            reactions: Vec::new(),
        }
    }

    /// Set a custom server ID
    ///
    /// If not set, a random UUID will be generated automatically.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this server instance
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .with_id("production-server-1")
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.server_id = Some(id.into());
        self
    }

    /// Set global default priority queue capacity for queries and reactions
    ///
    /// Priority queues are used to order query results by timestamp before delivering them
    /// to reactions. A larger capacity allows for more out-of-order events but uses more memory.
    ///
    /// Individual queries and reactions can override this value in their configuration.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Maximum number of results to buffer (default: 10000)
    ///
    /// # Performance Implications
    ///
    /// - **Larger capacity**: Handles more out-of-order events, uses more memory
    /// - **Smaller capacity**: Lower memory usage, may block if events arrive out of order
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // High-throughput configuration
    /// let core = DrasiServerCore::builder()
    ///     .with_priority_queue_capacity(50000)
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set global default dispatch buffer capacity for sources and queries
    ///
    /// Dispatch buffers are async channels used to send events from sources to queries and
    /// from queries to reactions. A larger capacity allows for more buffering during bursts
    /// but uses more memory.
    ///
    /// Individual sources and queries can override this value in their configuration.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Maximum number of events to buffer (default: 1000)
    ///
    /// # Performance Implications
    ///
    /// - **Larger capacity**: Better burst handling, higher memory usage
    /// - **Smaller capacity**: Lower memory usage, backpressure during bursts
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::DrasiServerCore;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Low-latency configuration with small buffers
    /// let core = DrasiServerCore::builder()
    ///     .with_dispatch_buffer_capacity(100)
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Add a storage backend to the server configuration
    ///
    /// Storage backends define where and how query indexes are persisted. Queries can
    /// reference these backends by ID using `.with_storage_backend()`.
    ///
    /// # Storage Backend Types
    ///
    /// - **Memory**: Volatile in-memory storage (fast, no persistence)
    /// - **RocksDB**: Persistent local storage for single-node deployments
    /// - **Redis/Garnet**: Distributed storage for multi-node deployments
    ///
    /// # Arguments
    ///
    /// * `backend` - Storage backend configuration with ID and specification
    ///
    /// # Examples
    ///
    /// ## RocksDB Backend
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Query, StorageBackendConfig, StorageBackendSpec};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_storage_backend(StorageBackendConfig {
    ///         id: "rocks_persistent".to_string(),
    ///         spec: StorageBackendSpec::RocksDb {
    ///             path: "/data/drasi/indexes".to_string(),
    ///             enable_archive: true,
    ///             direct_io: false,
    ///         },
    ///     })
    ///     .add_query(
    ///         Query::cypher("my_query")
    ///             .query("MATCH (n) RETURN n")
    ///             .from_source("events")
    ///             .with_storage_backend("rocks_persistent")  // Reference the backend
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Redis/Garnet Backend
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Query, StorageBackendConfig, StorageBackendSpec};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_storage_backend(StorageBackendConfig {
    ///         id: "redis_distributed".to_string(),
    ///         spec: StorageBackendSpec::Redis {
    ///             connection_string: "redis://localhost:6379".to_string(),
    ///             cache_size: 10000,
    ///         },
    ///     })
    ///     .add_query(
    ///         Query::cypher("my_query")
    ///             .query("MATCH (n) RETURN n")
    ///             .from_source("events")
    ///             .with_storage_backend("redis_distributed")
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn add_storage_backend(mut self, backend: crate::indexes::StorageBackendConfig) -> Self {
        self.storage_backends.push(backend);
        self
    }

    /// Add multiple storage backends at once
    ///
    /// Convenient method for adding multiple storage backend configurations in one call.
    ///
    /// # Arguments
    ///
    /// * `backends` - Vector of storage backend configurations
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, StorageBackendConfig, StorageBackendSpec};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let backends = vec![
    ///     StorageBackendConfig {
    ///         id: "rocks_primary".to_string(),
    ///         spec: StorageBackendSpec::RocksDb {
    ///             path: "/data/primary".to_string(),
    ///             enable_archive: true,
    ///             direct_io: false,
    ///         },
    ///     },
    ///     StorageBackendConfig {
    ///         id: "redis_cache".to_string(),
    ///         spec: StorageBackendSpec::Redis {
    ///             connection_string: "redis://cache:6379".to_string(),
    ///             cache_size: 5000,
    ///         },
    ///     },
    /// ];
    ///
    /// let core = DrasiServerCore::builder()
    ///     .add_storage_backends(backends)
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn add_storage_backends(
        mut self,
        backends: Vec<crate::indexes::StorageBackendConfig>,
    ) -> Self {
        self.storage_backends.extend(backends);
        self
    }

    /// Add a source to the server configuration
    ///
    /// Sources ingest data changes from external systems (databases, APIs, applications, etc.).
    /// Use the [`Source`](crate::Source) builder to create source configurations.
    ///
    /// # Arguments
    ///
    /// * `source` - Source configuration created via `Source` builder
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(
    ///         Source::application("events")
    ///             .auto_start(true)
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn add_source(mut self, source: SourceConfig) -> Self {
        self.sources.push(source);
        self
    }

    /// Add multiple sources at once
    ///
    /// Convenient method for adding multiple source configurations in one call.
    ///
    /// # Arguments
    ///
    /// * `sources` - Vector of source configurations
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let sources = vec![
    ///     Source::application("source1").build(),
    ///     Source::application("source2").build(),
    ///     Source::application("source3").build(),
    /// ];
    ///
    /// let core = DrasiServerCore::builder()
    ///     .add_sources(sources)
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn add_sources(mut self, sources: Vec<SourceConfig>) -> Self {
        self.sources.extend(sources);
        self
    }

    /// Add a continuous query to the server configuration
    ///
    /// Queries process data changes using Cypher or GQL and emit results when the query
    /// results change. Use the [`Query`](crate::Query) builder to create query configurations.
    ///
    /// # Arguments
    ///
    /// * `query` - Query configuration created via `Query` builder
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(
    ///         Query::cypher("active_users")
    ///             .query("MATCH (u:User) WHERE u.active = true RETURN u")
    ///             .from_source("events")
    ///             .auto_start(true)
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn add_query(mut self, query: QueryConfig) -> Self {
        self.queries.push(query);
        self
    }

    /// Add multiple queries at once
    ///
    /// Convenient method for adding multiple query configurations in one call.
    ///
    /// # Arguments
    ///
    /// * `queries` - Vector of query configurations
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let queries = vec![
    ///     Query::cypher("query1").query("MATCH (n) RETURN n").from_source("events").build(),
    ///     Query::cypher("query2").query("MATCH (n:User) RETURN n").from_source("events").build(),
    /// ];
    ///
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_queries(queries)
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn add_queries(mut self, queries: Vec<QueryConfig>) -> Self {
        self.queries.extend(queries);
        self
    }

    /// Add a reaction to the server configuration
    ///
    /// Reactions subscribe to query results and deliver them to external systems (webhooks,
    /// applications, logs, etc.). Use the [`Reaction`](crate::Reaction) builder to create
    /// reaction configurations.
    ///
    /// # Arguments
    ///
    /// * `reaction` - Reaction configuration created via `Reaction` builder
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
    /// # use serde_json::json;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(Query::cypher("users").query("MATCH (n) RETURN n").from_source("events").build())
    ///     .add_reaction(
    ///         Reaction::http("webhook")
    ///             .subscribe_to("users")
    ///             .with_property("base_url", json!("https://api.example.com/webhook"))
    ///             .auto_start(true)
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn add_reaction(mut self, reaction: ReactionConfig) -> Self {
        self.reactions.push(reaction);
        self
    }

    /// Add multiple reactions at once
    ///
    /// Convenient method for adding multiple reaction configurations in one call.
    ///
    /// # Arguments
    ///
    /// * `reactions` - Vector of reaction configurations
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source, Query, Reaction};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let reactions = vec![
    ///     Reaction::application("app_reaction").subscribe_to("query1").build(),
    ///     Reaction::log("log_reaction").subscribe_to("query1").build(),
    /// ];
    ///
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(Query::cypher("query1").query("MATCH (n) RETURN n").from_source("events").build())
    ///     .add_reactions(reactions)
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn add_reactions(mut self, reactions: Vec<ReactionConfig>) -> Self {
        self.reactions.extend(reactions);
        self
    }

    /// Build and initialize the DrasiServerCore instance
    ///
    /// Creates a new [`DrasiServerCore`] with the configured sources, queries, and reactions,
    /// then performs all initialization. The returned server is ready to be started with
    /// [`start()`](crate::DrasiServerCore::start).
    ///
    /// # Returns
    ///
    /// Returns `Ok(DrasiServerCore)` if initialization succeeds.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * Component initialization fails (invalid configuration, resource allocation, etc.)
    /// * Bootstrap providers fail to initialize
    /// * Query compilation fails
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_server_core::{DrasiServerCore, Source};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .with_id("my-server")
    ///     .add_source(Source::application("events").build())
    ///     .build()  // Initialize everything
    ///     .await?;
    ///
    /// // Server is initialized but not started
    /// core.start().await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn build(self) -> Result<DrasiServerCore> {
        let server_settings = DrasiServerCoreSettings {
            id: self
                .server_id
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            priority_queue_capacity: self.priority_queue_capacity,
            dispatch_buffer_capacity: self.dispatch_buffer_capacity,
        };

        // Create DrasiServerCoreConfig first, then convert to RuntimeConfig
        // This ensures the From implementation applies the priority queue hierarchy
        let core_config = DrasiServerCoreConfig {
            server_core: server_settings,
            storage_backends: self.storage_backends,
            sources: self.sources,
            queries: self.queries,
            reactions: self.reactions,
        };

        core_config
            .validate()
            .map_err(|e| DrasiError::startup_validation(e.to_string()))?;

        let config = Arc::new(RuntimeConfig::from(core_config));

        // Create and initialize the server
        let mut core = DrasiServerCore::new(config);
        core.initialize().await?;

        Ok(core)
    }
}

impl Default for DrasiServerCoreBuilder {
    fn default() -> Self {
        Self::new()
    }
}
