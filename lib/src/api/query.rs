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

//! Query configuration builders

use crate::channels::DispatchMode;
use crate::config::{QueryConfig, QueryJoinConfig, QueryLanguage};

/// Fluent builder for Query configuration
///
/// `QueryBuilder` provides a type-safe, ergonomic API for configuring continuous queries in Drasi.
/// Queries process data changes from sources using Cypher or GQL and emit results to reactions when
/// the query results change.
///
/// # Builder Pattern
///
/// The builder uses a fluent API where each method returns `self`, allowing you to chain
/// configuration calls. Call `build()` at the end to create the [`QueryConfig`].
///
/// # Query Languages
///
/// Drasi supports two query languages:
///
/// - **Cypher**: Graph query language for pattern matching and traversal
/// - **GQL**: GraphQL-based query language (experimental)
///
/// **Important**: Drasi Core does not support queries with ORDER BY, TOP, or LIMIT clauses.
///
/// # Data Flow
///
/// ```text
/// Sources → [Query Subscriptions] → Query Engine → [Priority Queue] → Reactions
/// ```
///
/// 1. Query subscribes to one or more sources via `from_source()` or `from_sources()`
/// 2. Source changes are processed by the query engine
/// 3. Results are ordered by timestamp in the priority queue
/// 4. Changed results are dispatched to subscribed reactions
///
/// # Performance Settings
///
/// - **priority_queue_capacity**: Buffer size for timestamp-ordered results (default: 10000)
/// - **dispatch_buffer_capacity**: Channel buffer for routing to reactions (default: 1000)
/// - **dispatch_mode**: Broadcast (shared) or Channel (per-reaction)
///
/// # Default Values
///
/// - **auto_start**: true (query starts when server starts)
/// - **dispatch_mode**: Channel (separate channel per reaction)
/// - **enable_bootstrap**: true (receive initial data from sources)
/// - **bootstrap_buffer_size**: 10000
///
/// # Thread Safety
///
/// `QueryBuilder` is `Clone` and can be used to create multiple query configurations.
/// The built configuration is immutable and thread-safe.
///
/// # Examples
///
/// ## Basic Cypher Query
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Query};
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
///
/// ## Query with Multiple Sources
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Query};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("orders").build())
///     .add_source(Source::application("customers").build())
///     .add_query(
///         Query::cypher("order_summary")
///             .query(r#"
///                 MATCH (o:Order)-[:PLACED_BY]->(c:Customer)
///                 RETURN o.id, o.total, c.name
///             "#)
///             .from_sources(vec!["orders".to_string(), "customers".to_string()])
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## High-Throughput Query with Performance Tuning
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Query};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::postgres("high_volume_db").build())
///     .add_query(
///         Query::cypher("hot_stocks")
///             .query("MATCH (s:Stock) WHERE s.change_percent > 5.0 RETURN s")
///             .from_source("high_volume_db")
///             .with_priority_queue_capacity(100000)   // Large buffer for bursts
///             .with_dispatch_buffer_capacity(5000)    // High-throughput dispatch
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## GQL Query
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Query};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("users").build())
///     .add_query(
///         Query::gql("user_list")
///             .query("{ users { id name email } }")
///             .from_source("users")
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct QueryBuilder {
    id: String,
    query: String,
    query_language: QueryLanguage,
    middleware: Vec<drasi_core::models::SourceMiddlewareConfig>,
    source_subscriptions: Vec<crate::config::SourceSubscriptionConfig>,
    auto_start: bool,
    joins: Option<Vec<QueryJoinConfig>>,
    priority_queue_capacity: Option<usize>,
    dispatch_buffer_capacity: Option<usize>,
    dispatch_mode: Option<DispatchMode>,
    storage_backend: Option<crate::indexes::StorageBackendRef>,
}

impl QueryBuilder {
    /// Create a new query builder
    fn new(id: impl Into<String>, query: impl Into<String>, language: QueryLanguage) -> Self {
        Self {
            id: id.into(),
            query: query.into(),
            query_language: language,
            middleware: Vec::new(),
            source_subscriptions: Vec::new(),
            auto_start: true,
            joins: None,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        }
    }

    /// Set the query string (Cypher or GQL)
    ///
    /// The query string is the continuous query expression that will process data changes.
    /// The language is determined by the factory method used (`Query::cypher()` or `Query::gql()`).
    ///
    /// **Important**: Drasi Core does not support ORDER BY, TOP, or LIMIT clauses.
    ///
    /// # Arguments
    ///
    /// * `query` - Query string in Cypher or GQL format
    ///
    /// # Examples
    ///
    /// ## Cypher Query
    ///
    /// ```no_run
    /// # use drasi_lib::Query;
    /// let query = Query::cypher("active_orders")
    ///     .query("MATCH (o:Order) WHERE o.status = 'active' RETURN o")
    ///     .from_source("orders")
    ///     .build();
    /// ```
    ///
    /// ## GQL Query
    ///
    /// ```no_run
    /// # use drasi_lib::Query;
    /// let query = Query::gql("user_list")
    ///     .query("{ users { id name email } }")
    ///     .from_source("users")
    ///     .build();
    /// ```
    ///
    /// ## Complex Cypher Pattern
    ///
    /// ```no_run
    /// # use drasi_lib::Query;
    /// let query = Query::cypher("order_details")
    ///     .query(r#"
    ///         MATCH (o:Order)-[:CONTAINS]->(i:Item)
    ///         MATCH (o)-[:PLACED_BY]->(c:Customer)
    ///         WHERE o.status = 'pending'
    ///         RETURN o.id, c.name, collect(i.name) as items
    ///     "#)
    ///     .from_sources(vec!["orders".to_string(), "customers".to_string()])
    ///     .build();
    /// ```
    pub fn query(mut self, query: impl Into<String>) -> Self {
        self.query = query.into();
        self
    }

    /// Subscribe this query to a single source
    ///
    /// The query will receive all change events from the specified source. Sources must be
    /// added to the server before queries that subscribe to them.
    ///
    /// # Arguments
    ///
    /// * `source_id` - ID of the source to subscribe to
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(
    ///         Query::cypher("my_query")
    ///             .query("MATCH (n) RETURN n")
    ///             .from_source("events")  // Subscribe to single source
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_source(mut self, source_id: impl Into<String>) -> Self {
        self.source_subscriptions
            .push(crate::config::SourceSubscriptionConfig {
                source_id: source_id.into(),
                pipeline: Vec::new(),
            });
        self
    }

    /// Subscribe this query to multiple sources
    ///
    /// The query will receive change events from all specified sources. This replaces any
    /// sources previously set via `from_source()`. Use this for queries that join data
    /// across multiple sources.
    ///
    /// # Arguments
    ///
    /// * `source_ids` - Vector of source IDs to subscribe to
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("orders").build())
    ///     .add_source(Source::application("customers").build())
    ///     .add_source(Source::application("products").build())
    ///     .add_query(
    ///         Query::cypher("order_summary")
    ///             .query(r#"
    ///                 MATCH (o:Order)-[:CONTAINS]->(p:Product)
    ///                 MATCH (o)-[:PLACED_BY]->(c:Customer)
    ///                 RETURN o.id, c.name, collect(p.name) as products
    ///             "#)
    ///             .from_sources(vec![
    ///                 "orders".to_string(),
    ///                 "customers".to_string(),
    ///                 "products".to_string()
    ///             ])
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn from_sources(mut self, source_ids: Vec<String>) -> Self {
        for source_id in source_ids {
            self.source_subscriptions
                .push(crate::config::SourceSubscriptionConfig {
                    source_id,
                    pipeline: Vec::new(),
                });
        }
        self
    }

    /// Set whether to auto-start this query when the server starts
    ///
    /// When `auto_start` is `true` (the default), the query will automatically begin processing
    /// when [`DrasiServerCore::start()`](crate::DrasiServerCore::start) is called. When `false`,
    /// the query must be started manually via the control API.
    ///
    /// # Arguments
    ///
    /// * `auto_start` - Whether to automatically start this query (default: true)
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Query that requires manual start (e.g., after validation)
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(
    ///         Query::cypher("admin_query")
    ///             .query("MATCH (n) RETURN n")
    ///             .from_source("events")
    ///             .auto_start(false)  // Don't start automatically
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    ///
    /// // Start manually when ready
    /// // control_api.start_query("admin_query").await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Add a join configuration for cross-source relationships
    ///
    /// Join configurations define how data from multiple sources should be connected in the
    /// query. This is a low-level API for advanced use cases. Refer to `QueryJoinConfig`
    /// documentation for structure details.
    ///
    /// # Arguments
    ///
    /// * `join` - Join configuration specifying source relationship
    pub fn with_join(mut self, join: QueryJoinConfig) -> Self {
        self.joins.get_or_insert_with(Vec::new).push(join);
        self
    }

    /// Set multiple join configurations at once
    ///
    /// This replaces any joins previously configured via `with_join()`.
    /// Refer to `QueryJoinConfig` documentation for structure details.
    ///
    /// # Arguments
    ///
    /// * `joins` - Vector of join configurations
    pub fn with_joins(mut self, joins: Vec<QueryJoinConfig>) -> Self {
        self.joins = Some(joins);
        self
    }

    /// Set the priority queue capacity for this query (performance tuning)
    ///
    /// Priority queues buffer query results and order them by timestamp before delivering to
    /// reactions. This ensures reactions receive events in the correct temporal order even if
    /// sources emit events out of order.
    ///
    /// This setting overrides the global default for this specific query.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Maximum number of results to buffer (default: inherits from server, typically 10000)
    ///
    /// # Performance Implications
    ///
    /// - **Larger capacity**: Handles more out-of-order events, uses more memory
    /// - **Smaller capacity**: Lower memory usage, may block if events arrive very out of order
    ///
    /// # Recommended Values
    ///
    /// - **High-volume queries**: 50000-1000000 (for sources with high throughput and latency variance)
    /// - **Normal queries**: 10000 (default, suitable for most use cases)
    /// - **Memory-constrained**: 1000-5000 (when memory is limited)
    ///
    /// # Examples
    ///
    /// ## High-Volume Query
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::postgres("trading_db").build())
    ///     .add_query(
    ///         Query::cypher("hot_stocks")
    ///             .query("MATCH (s:Stock) WHERE s.volume > 1000000 RETURN s")
    ///             .from_source("trading_db")
    ///             .with_priority_queue_capacity(500000)  // Large buffer for high-frequency trading
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Memory-Constrained Query
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(
    ///         Query::cypher("simple_filter")
    ///             .query("MATCH (n:Event) WHERE n.priority = 'high' RETURN n")
    ///             .from_source("events")
    ///             .with_priority_queue_capacity(2000)  // Small buffer for low memory usage
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set the dispatch buffer capacity for this query (performance tuning)
    ///
    /// Dispatch buffers are async channels used to route query results from this query to
    /// subscribed reactions. This setting overrides the global default for this specific query.
    ///
    /// # Arguments
    ///
    /// * `capacity` - Maximum number of results to buffer (default: inherits from server, typically 1000)
    ///
    /// # Performance Implications
    ///
    /// - **Larger capacity**: Better burst handling, higher memory usage per reaction
    /// - **Smaller capacity**: Lower memory usage, backpressure during bursts
    ///
    /// # Examples
    ///
    /// ## High-Throughput Query
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::postgres("sensor_db").build())
    ///     .add_query(
    ///         Query::cypher("sensor_alerts")
    ///             .query("MATCH (s:Sensor) WHERE s.temp > 100 RETURN s")
    ///             .from_source("sensor_db")
    ///             .with_dispatch_buffer_capacity(10000)  // Large buffer for many reactions
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the dispatch mode for this query (performance tuning)
    ///
    /// Dispatch mode controls how query results are distributed to subscribed reactions:
    ///
    /// - **`DispatchMode::Channel`** (default): Separate channel per reaction
    ///   - Better isolation between reactions
    ///   - Higher memory usage (N × buffer_capacity)
    ///   - Recommended for most use cases
    ///
    /// - **`DispatchMode::Broadcast`**: Single shared channel for all reactions
    ///   - Memory efficient (1 × buffer_capacity)
    ///   - All reactions must keep up or slow ones block fast ones
    ///   - Best when all reactions have similar processing speeds
    ///
    /// # Arguments
    ///
    /// * `mode` - Dispatch mode (Channel or Broadcast)
    ///
    /// # Examples
    ///
    /// ## Broadcast Mode (Memory Efficient)
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query, DispatchMode};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(
    ///         Query::cypher("shared_events")
    ///             .query("MATCH (e:Event) RETURN e")
    ///             .from_source("events")
    ///             .with_dispatch_mode(DispatchMode::Broadcast)  // Share channel
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Channel Mode (Better Isolation)
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query, DispatchMode};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(
    ///         Query::cypher("isolated_events")
    ///             .query("MATCH (e:Event) RETURN e")
    ///             .from_source("events")
    ///             .with_dispatch_mode(DispatchMode::Channel)  // Explicit, but default
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Configure a storage backend for this query by referencing a named backend
    ///
    /// Storage backends define where and how query indexes are persisted:
    ///
    /// - **Memory**: Volatile in-memory storage (default if not configured)
    /// - **RocksDB**: Persistent local storage for single-node deployments
    /// - **Redis/Garnet**: Distributed storage for multi-node deployments
    ///
    /// Named backends must be defined in the server configuration's `storage_backends` list.
    ///
    /// # Arguments
    ///
    /// * `backend_id` - ID of a storage backend defined in server configuration
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::Query;
    /// let query = Query::cypher("my_query")
    ///     .query("MATCH (n) RETURN n")
    ///     .from_source("events")
    ///     .with_storage_backend("rocks_persistent")  // Reference named backend
    ///     .build();
    /// ```
    pub fn with_storage_backend(mut self, backend_id: impl Into<String>) -> Self {
        self.storage_backend = Some(crate::indexes::StorageBackendRef::Named(backend_id.into()));
        self
    }

    /// Configure an inline storage backend for this query
    ///
    /// Use this to configure storage directly on the query without defining it in
    /// server configuration. Useful for testing or simple deployments.
    ///
    /// # Arguments
    ///
    /// * `spec` - Storage backend specification
    ///
    /// # Examples
    ///
    /// ## Inline Memory Backend
    ///
    /// ```no_run
    /// # use drasi_lib::{Query, StorageBackendSpec};
    /// let query = Query::cypher("my_query")
    ///     .query("MATCH (n) RETURN n")
    ///     .from_source("events")
    ///     .with_inline_storage(StorageBackendSpec::Memory {
    ///         enable_archive: true
    ///     })
    ///     .build();
    /// ```
    ///
    /// ## Inline RocksDB Backend
    ///
    /// ```no_run
    /// # use drasi_lib::{Query, StorageBackendSpec};
    /// let query = Query::cypher("my_query")
    ///     .query("MATCH (n) RETURN n")
    ///     .from_source("events")
    ///     .with_inline_storage(StorageBackendSpec::RocksDb {
    ///         path: "/data/query-indexes".to_string(),
    ///         enable_archive: true,
    ///         direct_io: false,
    ///     })
    ///     .build();
    /// ```
    pub fn with_inline_storage(mut self, spec: crate::indexes::StorageBackendSpec) -> Self {
        self.storage_backend = Some(crate::indexes::StorageBackendRef::Inline(spec));
        self
    }

    /// Add a middleware configuration to the query
    ///
    /// Middleware transforms or filters data as it flows from sources to the query engine.
    /// Multiple middleware can be added and will be available for use in source pipelines.
    ///
    /// # Arguments
    ///
    /// * `middleware` - The middleware configuration to add
    ///
    /// # Examples
    ///
    /// ## Add JQ Middleware
    ///
    /// ```no_run
    /// # use drasi_lib::Query;
    /// # use drasi_core::models::SourceMiddlewareConfig;
    /// # use std::sync::Arc;
    /// let query = Query::cypher("query1")
    ///     .query("MATCH (n) RETURN n")
    ///     .with_middleware(SourceMiddlewareConfig {
    ///         kind: Arc::from("jq"),
    ///         name: Arc::from("transform1"),
    ///         config: serde_json::json!({"filter": ".value | tonumber"}).as_object().unwrap().clone(),
    ///     })
    ///     .from_source("source1")
    ///     .build();
    /// ```
    ///
    /// ## Multiple Middleware
    ///
    /// ```no_run
    /// # use drasi_lib::Query;
    /// # use drasi_core::models::SourceMiddlewareConfig;
    /// # use std::sync::Arc;
    /// let query = Query::cypher("query1")
    ///     .query("MATCH (n) RETURN n")
    ///     .with_middleware(SourceMiddlewareConfig {
    ///         kind: Arc::from("jq"),
    ///         name: Arc::from("decoder"),
    ///         config: serde_json::json!({"filter": ".data | fromjson"}).as_object().unwrap().clone(),
    ///     })
    ///     .with_middleware(SourceMiddlewareConfig {
    ///         kind: Arc::from("jq"),
    ///         name: Arc::from("validator"),
    ///         config: serde_json::json!({"filter": "select(.value > 0)"}).as_object().unwrap().clone(),
    ///     })
    ///     .from_source("source1")
    ///     .build();
    /// ```
    pub fn with_middleware(
        mut self,
        middleware: drasi_core::models::SourceMiddlewareConfig,
    ) -> Self {
        self.middleware.push(middleware);
        self
    }

    /// Configure a source subscription with a middleware pipeline
    ///
    /// This method allows you to specify both the source to subscribe to and an ordered
    /// list of middleware to apply to changes from that source. Middleware must be defined
    /// using `with_middleware()` before being referenced in a pipeline.
    ///
    /// # Arguments
    ///
    /// * `source_id` - The ID of the source to subscribe to
    /// * `pipeline` - The ordered list of middleware names to apply
    ///
    /// # Examples
    ///
    /// ## Source with Pipeline
    ///
    /// ```no_run
    /// # use drasi_lib::Query;
    /// # use drasi_core::models::SourceMiddlewareConfig;
    /// # use std::sync::Arc;
    /// let query = Query::cypher("query1")
    ///     .query("MATCH (n) RETURN n")
    ///     .with_middleware(SourceMiddlewareConfig {
    ///         kind: Arc::from("jq"),
    ///         name: Arc::from("transform1"),
    ///         config: serde_json::json!({"filter": ".value | tonumber"}).as_object().unwrap().clone(),
    ///     })
    ///     .with_source_pipeline("source1", vec!["transform1".to_string()])
    ///     .build();
    /// ```
    ///
    /// ## Multiple Sources with Different Pipelines
    ///
    /// ```no_run
    /// # use drasi_lib::Query;
    /// # use drasi_core::models::SourceMiddlewareConfig;
    /// # use std::sync::Arc;
    /// let query = Query::cypher("query1")
    ///     .query("MATCH (n) RETURN n")
    ///     .with_middleware(SourceMiddlewareConfig {
    ///         kind: Arc::from("jq"),
    ///         name: Arc::from("decoder"),
    ///         config: serde_json::json!({"filter": ".data | fromjson"}).as_object().unwrap().clone(),
    ///     })
    ///     .with_middleware(SourceMiddlewareConfig {
    ///         kind: Arc::from("jq"),
    ///         name: Arc::from("validator"),
    ///         config: serde_json::json!({"filter": "select(.value > 0)"}).as_object().unwrap().clone(),
    ///     })
    ///     .with_source_pipeline("raw_source", vec!["decoder".to_string(), "validator".to_string()])
    ///     .with_source_pipeline("clean_source", vec!["validator".to_string()])
    ///     .build();
    /// ```
    pub fn with_source_pipeline(
        mut self,
        source_id: impl Into<String>,
        pipeline: Vec<String>,
    ) -> Self {
        let subscription = crate::config::SourceSubscriptionConfig {
            source_id: source_id.into(),
            pipeline,
        };
        self.source_subscriptions.push(subscription);
        self
    }

    /// Build the query configuration
    ///
    /// Consumes the builder and creates the final [`QueryConfig`] that can be passed to
    /// [`DrasiServerCoreBuilder::add_query()`](crate::DrasiServerCoreBuilder::add_query).
    ///
    /// # Returns
    ///
    /// Returns the configured [`QueryConfig`] ready for use.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let query_config = Query::cypher("my_query")
    ///     .query("MATCH (n) RETURN n")
    ///     .from_source("events")
    ///     .build();  // Build returns QueryConfig
    ///
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(query_config)
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn build(self) -> QueryConfig {
        QueryConfig {
            id: self.id,
            query: self.query,
            query_language: self.query_language,
            middleware: self.middleware,
            source_subscriptions: self.source_subscriptions,
            auto_start: self.auto_start,
            joins: self.joins,
            enable_bootstrap: true,       // Default: bootstrap enabled
            bootstrap_buffer_size: 10000, // Default buffer size
            priority_queue_capacity: self.priority_queue_capacity,
            dispatch_buffer_capacity: self.dispatch_buffer_capacity,
            dispatch_mode: self.dispatch_mode,
            storage_backend: self.storage_backend,
        }
    }
}

/// Query configuration factory for creating queries with type-safe builders
///
/// `Query` is a zero-sized type that provides factory methods for creating continuous queries
/// in different query languages. Each factory method returns a [`QueryBuilder`] configured for
/// that language.
///
/// # Query Languages
///
/// - **Cypher**: Graph query language for pattern matching and relationship traversal
/// - **GQL**: GraphQL-based query language (experimental)
///
/// **Important Limitation**: Drasi Core does not support queries with ORDER BY, TOP, or LIMIT clauses.
///
/// # Continuous Query Model
///
/// Unlike traditional databases where queries run once and return results, Drasi queries are
/// **continuous** - they run continuously and emit result **changes** when the underlying data changes:
///
/// - Initial results are emitted during bootstrap
/// - When data changes, only the **diff** (added/removed/updated rows) is emitted
/// - Reactions receive incremental updates, not full result sets
///
/// # Examples
///
/// ## Basic Cypher Query
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Query};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("events").build())
///     .add_query(
///         Query::cypher("active_users")
///             .query("MATCH (u:User) WHERE u.active = true RETURN u")
///             .from_source("events")
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## GQL Query
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Query};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("users").build())
///     .add_query(
///         Query::gql("user_list")
///             .query("{ users { id name email } }")
///             .from_source("users")
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Multiple Queries on Same Source
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, Query};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::builder()
///     .add_source(Source::application("events").build())
///     .add_query(
///         Query::cypher("high_priority")
///             .query("MATCH (e:Event) WHERE e.priority = 'high' RETURN e")
///             .from_source("events")
///             .build()
///     )
///     .add_query(
///         Query::cypher("critical")
///             .query("MATCH (e:Event) WHERE e.priority = 'critical' RETURN e")
///             .from_source("events")
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct Query;

impl Query {
    /// Create a Cypher continuous query
    ///
    /// Cypher is Drasi's primary query language for graph pattern matching and traversal.
    /// The query will continuously evaluate as data changes and emit result diffs to reactions.
    ///
    /// **Important**: Drasi Core does not support ORDER BY, TOP, or LIMIT clauses in Cypher queries.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this query
    ///
    /// # Cypher Patterns Supported
    ///
    /// - **MATCH**: Pattern matching for nodes and relationships
    /// - **WHERE**: Filtering conditions
    /// - **RETURN**: Result projection
    /// - **WITH**: Pipeline stage separator
    /// - **Aggregations**: count(), sum(), avg(), collect(), etc.
    /// - **Functions**: String, math, temporal, and custom Drasi functions
    ///
    /// # Examples
    ///
    /// ## Simple Filter Query
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("events").build())
    ///     .add_query(
    ///         Query::cypher("active_orders")
    ///             .query("MATCH (o:Order) WHERE o.status = 'active' RETURN o")
    ///             .from_source("events")
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Relationship Traversal
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("graph").build())
    ///     .add_query(
    ///         Query::cypher("customer_orders")
    ///             .query(r#"
    ///                 MATCH (c:Customer)-[:PLACED]->(o:Order)
    ///                 RETURN c.name, o.id, o.total
    ///             "#)
    ///             .from_source("graph")
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Aggregation Query
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("sales").build())
    ///     .add_query(
    ///         Query::cypher("order_totals")
    ///             .query(r#"
    ///                 MATCH (c:Customer)-[:PLACED]->(o:Order)
    ///                 RETURN c.id, c.name, count(o) as order_count, sum(o.total) as total_spent
    ///             "#)
    ///             .from_source("sales")
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## Complex Pattern with Multiple Sources
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("orders").build())
    ///     .add_source(Source::application("inventory").build())
    ///     .add_query(
    ///         Query::cypher("low_stock_orders")
    ///             .query(r#"
    ///                 MATCH (o:Order)-[:CONTAINS]->(i:Item)
    ///                 MATCH (p:Product {id: i.product_id})
    ///                 WHERE p.stock < 10
    ///                 RETURN o.id, p.name, p.stock
    ///             "#)
    ///             .from_sources(vec!["orders".to_string(), "inventory".to_string()])
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn cypher(id: impl Into<String>) -> QueryBuilder {
        QueryBuilder::new(id, "", QueryLanguage::Cypher)
    }

    /// Create a GQL (GraphQL) continuous query (experimental)
    ///
    /// GQL provides a GraphQL-style query interface that is compiled to Cypher internally.
    /// This is an experimental feature and may have limitations compared to native Cypher.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this query
    ///
    /// # Examples
    ///
    /// ## Basic GQL Query
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("users").build())
    ///     .add_query(
    ///         Query::gql("user_list")
    ///             .query("{ users { id name email } }")
    ///             .from_source("users")
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// ## GQL with Filtering
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, Query};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let core = DrasiServerCore::builder()
    ///     .add_source(Source::application("users").build())
    ///     .add_query(
    ///         Query::gql("active_users")
    ///             .query(r#"
    ///                 {
    ///                     users(where: {active: true}) {
    ///                         id
    ///                         name
    ///                         email
    ///                     }
    ///                 }
    ///             "#)
    ///             .from_source("users")
    ///             .build()
    ///     )
    ///     .build()
    ///     .await?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn gql(id: impl Into<String>) -> QueryBuilder {
        QueryBuilder::new(id, "", QueryLanguage::GQL)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn test_with_middleware() {
        // Create a query with middleware
        let middleware = drasi_core::models::SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("transform1"),
            config: serde_json::json!({"filter": ".value | tonumber"})
                .as_object()
                .unwrap()
                .clone(),
        };

        let config = Query::cypher("test_query")
            .query("MATCH (n) RETURN n")
            .with_middleware(middleware.clone())
            .from_source("source1")
            .build();

        // Verify middleware was added
        assert_eq!(config.middleware.len(), 1);
        assert_eq!(config.middleware[0].kind.as_ref(), "jq");
        assert_eq!(config.middleware[0].name.as_ref(), "transform1");
    }

    #[test]
    fn test_with_multiple_middleware() {
        // Create a query with multiple middleware
        let middleware1 = drasi_core::models::SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("decoder"),
            config: serde_json::json!({"filter": ".data | fromjson"})
                .as_object()
                .unwrap()
                .clone(),
        };

        let middleware2 = drasi_core::models::SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("validator"),
            config: serde_json::json!({"filter": "select(.value > 0)"})
                .as_object()
                .unwrap()
                .clone(),
        };

        let config = Query::cypher("test_query")
            .query("MATCH (n) RETURN n")
            .with_middleware(middleware1)
            .with_middleware(middleware2)
            .from_source("source1")
            .build();

        // Verify both middleware were added
        assert_eq!(config.middleware.len(), 2);
        assert_eq!(config.middleware[0].name.as_ref(), "decoder");
        assert_eq!(config.middleware[1].name.as_ref(), "validator");
    }

    #[test]
    fn test_with_source_pipeline() {
        // Create a query with a source that has a pipeline
        let config = Query::cypher("test_query")
            .query("MATCH (n) RETURN n")
            .with_source_pipeline("source1", vec!["transform1".to_string()])
            .build();

        // Verify source subscription was created with pipeline
        assert_eq!(config.source_subscriptions.len(), 1);
        assert_eq!(config.source_subscriptions[0].source_id, "source1");
        assert_eq!(config.source_subscriptions[0].pipeline.len(), 1);
        assert_eq!(config.source_subscriptions[0].pipeline[0], "transform1");
    }

    #[test]
    fn test_with_source_pipeline_multiple_middleware() {
        // Create a query with a source that has multiple middleware in pipeline
        let config = Query::cypher("test_query")
            .query("MATCH (n) RETURN n")
            .with_source_pipeline(
                "source1",
                vec!["decoder".to_string(), "validator".to_string()],
            )
            .build();

        // Verify source subscription was created with correct pipeline
        assert_eq!(config.source_subscriptions.len(), 1);
        assert_eq!(config.source_subscriptions[0].source_id, "source1");
        assert_eq!(config.source_subscriptions[0].pipeline.len(), 2);
        assert_eq!(config.source_subscriptions[0].pipeline[0], "decoder");
        assert_eq!(config.source_subscriptions[0].pipeline[1], "validator");
    }

    #[test]
    fn test_multiple_sources_with_different_pipelines() {
        // Create a query with multiple sources that have different pipelines
        let config = Query::cypher("test_query")
            .query("MATCH (n) RETURN n")
            .with_source_pipeline(
                "raw_source",
                vec!["decoder".to_string(), "validator".to_string()],
            )
            .with_source_pipeline("clean_source", vec!["validator".to_string()])
            .build();

        // Verify both source subscriptions were created
        assert_eq!(config.source_subscriptions.len(), 2);
        assert_eq!(config.source_subscriptions[0].source_id, "raw_source");
        assert_eq!(config.source_subscriptions[0].pipeline.len(), 2);
        assert_eq!(config.source_subscriptions[1].source_id, "clean_source");
        assert_eq!(config.source_subscriptions[1].pipeline.len(), 1);
    }

    #[test]
    fn test_full_middleware_configuration() {
        // Test a complete middleware configuration with both middleware definitions and pipelines
        let middleware1 = drasi_core::models::SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("decoder"),
            config: serde_json::json!({"filter": ".data | fromjson"})
                .as_object()
                .unwrap()
                .clone(),
        };

        let middleware2 = drasi_core::models::SourceMiddlewareConfig {
            kind: Arc::from("jq"),
            name: Arc::from("validator"),
            config: serde_json::json!({"filter": "select(.value > 0)"})
                .as_object()
                .unwrap()
                .clone(),
        };

        let config = Query::cypher("test_query")
            .query("MATCH (n) RETURN n")
            .with_middleware(middleware1)
            .with_middleware(middleware2)
            .with_source_pipeline(
                "raw_source",
                vec!["decoder".to_string(), "validator".to_string()],
            )
            .with_source_pipeline("clean_source", vec!["validator".to_string()])
            .build();

        // Verify everything was configured correctly
        assert_eq!(config.middleware.len(), 2);
        assert_eq!(config.source_subscriptions.len(), 2);
        assert_eq!(config.source_subscriptions[0].pipeline.len(), 2);
        assert_eq!(config.source_subscriptions[1].pipeline.len(), 1);
    }

    #[test]
    fn test_method_chaining() {
        // Test that methods can be chained with other builder methods
        let config = Query::cypher("test_query")
            .query("MATCH (n) RETURN n")
            .auto_start(false)
            .with_middleware(drasi_core::models::SourceMiddlewareConfig {
                kind: Arc::from("jq"),
                name: Arc::from("transform1"),
                config: serde_json::json!({"filter": ".value"})
                    .as_object()
                    .unwrap()
                    .clone(),
            })
            .with_source_pipeline("source1", vec!["transform1".to_string()])
            .with_priority_queue_capacity(5000)
            .build();

        // Verify all settings were applied
        assert!(!config.auto_start);
        assert_eq!(config.middleware.len(), 1);
        assert_eq!(config.source_subscriptions.len(), 1);
        assert_eq!(config.priority_queue_capacity, Some(5000));
    }

    #[test]
    fn test_middleware_without_pipeline() {
        // Test that middleware can be added without using it in a pipeline
        let config = Query::cypher("test_query")
            .query("MATCH (n) RETURN n")
            .with_middleware(drasi_core::models::SourceMiddlewareConfig {
                kind: Arc::from("jq"),
                name: Arc::from("transform1"),
                config: serde_json::json!({"filter": ".value"})
                    .as_object()
                    .unwrap()
                    .clone(),
            })
            .from_source("source1")
            .build();

        // Verify middleware was added but source has no pipeline
        assert_eq!(config.middleware.len(), 1);
        assert_eq!(config.source_subscriptions.len(), 1);
        assert_eq!(config.source_subscriptions[0].pipeline.len(), 0);
    }
}
