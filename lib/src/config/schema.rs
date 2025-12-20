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
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::channels::DispatchMode;
use crate::indexes::{StorageBackendConfig, StorageBackendRef};
use drasi_core::models::SourceMiddlewareConfig;

/// Query language for continuous queries
///
/// Drasi supports two query languages for continuous query processing:
///
/// # Query Languages
///
/// - **Cypher**: Default graph query language with pattern matching
/// - **GQL**: GraphQL-style queries compiled to Cypher
///
/// # Default Behavior
///
/// If not specified, queries default to `QueryLanguage::Cypher`.
///
/// # Examples
///
/// ## Using Cypher (Default)
///
/// ```yaml
/// queries:
///   - id: active_orders
///     query: "MATCH (o:Order) WHERE o.status = 'active' RETURN o"
///     queryLanguage: Cypher  # Optional, this is the default
///     sources: [orders_db]
/// ```
///
/// ## Using GQL
///
/// ```yaml
/// queries:
///   - id: user_data
///     query: |
///       {
///         users(status: "active") {
///           id
///           name
///           email
///         }
///       }
///     queryLanguage: GQL
///     sources: [users_db]
/// ```
///
/// # Important Limitations
///
/// **Unsupported Clauses**: ORDER BY, TOP, and LIMIT clauses are not supported in continuous
/// queries as they conflict with incremental result computation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub enum QueryLanguage {
    #[default]
    Cypher,
    GQL,
}

/// Source subscription configuration for queries
///
/// `SourceSubscriptionConfig` defines how a query subscribes to a specific source,
/// including any middleware pipeline to apply to changes from that source.
///
/// # Fields
///
/// - **source_id**: ID of the source to subscribe to
/// - **nodes**: Optional list of node labels to subscribe to from this source
/// - **relations**: Optional list of relation labels to subscribe to from this source
/// - **pipeline**: Optional list of middleware IDs to apply to changes from this source
///
/// # Examples
///
/// ## Simple Subscription (No Pipeline)
///
/// ```yaml
/// source_subscriptions:
///   - source_id: orders_db
///     pipeline: []
/// ```
///
/// ## Subscription with Middleware Pipeline
///
/// ```yaml
/// source_subscriptions:
///   - source_id: raw_events
///     pipeline: [decoder, mapper, validator]
/// ```
///
/// ## Subscription with Label Filtering
///
/// ```yaml
/// source_subscriptions:
///   - source_id: orders_db
///     nodes: [Order, Customer]
///     relations: [PLACED_BY]
///     pipeline: []
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSubscriptionConfig {
    pub source_id: String,
    #[serde(default)]
    pub nodes: Vec<String>,
    #[serde(default)]
    pub relations: Vec<String>,
    #[serde(default)]
    pub pipeline: Vec<String>,
}

/// Settings passed to a source when subscribing
///
/// `SourceSubscriptionSettings` contains all the information a source needs to
/// intelligently handle bootstrap and subscription for a query, including the
/// specific node and relation labels the query is interested in.
///
/// # Fields
///
/// - **source_id**: ID of the source
/// - **enable_bootstrap**: Whether to request initial data
/// - **query_id**: ID of the subscribing query
/// - **nodes**: Set of node labels the query is interested in from this source
/// - **relations**: Set of relation labels the query is interested in from this source
///
/// # Example
///
/// ```ignore
/// use drasi_lib::config::SourceSubscriptionSettings;
/// use std::collections::HashSet;
///
/// let settings = SourceSubscriptionSettings {
///     source_id: "orders_db".to_string(),
///     enable_bootstrap: true,
///     query_id: "my-query".to_string(),
///     nodes: ["Order", "Customer"].iter().map(|s| s.to_string()).collect(),
///     relations: ["PLACED_BY"].iter().map(|s| s.to_string()).collect(),
/// };
/// ```
#[derive(Debug, Clone)]
pub struct SourceSubscriptionSettings {
    pub source_id: String,
    pub enable_bootstrap: bool,
    pub query_id: String,
    pub nodes: HashSet<String>,
    pub relations: HashSet<String>,
}

/// Root configuration for Drasi Server Core
///
/// `DrasiLibConfig` is the top-level configuration structure for DrasiLib.
/// It defines server settings, queries, and storage backends.
///
/// # Plugin Architecture
///
/// **Important**: drasi-lib has ZERO awareness of which plugins exist. Sources and
/// reactions are passed as owned instances via `with_source()` and `with_reaction()`
/// on the builder. Only queries can be configured via the builder.
///
/// # Configuration Structure
///
/// A typical configuration has these sections:
///
/// 1. **id**: Unique server identifier (optional, defaults to UUID)
/// 2. **priority_queue_capacity**: Default capacity for event queues (optional)
/// 3. **dispatch_buffer_capacity**: Default capacity for dispatch buffers (optional)
/// 4. **storage_backends**: Storage backend definitions (optional)
/// 5. **queries**: Continuous queries to process data
///
/// # Capacity Settings
///
/// - **priority_queue_capacity**: Default capacity for timestamp-ordered event queues in
///   queries and reactions. Higher values support more out-of-order events but consume
///   more memory. Default: 10000
///
/// - **dispatch_buffer_capacity**: Default capacity for event dispatch channels between
///   components (sources → queries, queries → reactions). Higher values improve throughput
///   under load but consume more memory. Default: 1000
///
/// Individual components can override these defaults by setting their own capacity values.
///
/// # Thread Safety
///
/// This struct is `Clone` and can be safely shared across threads.
///
/// # Usage
///
/// Use `DrasiLib::builder()` to create instances:
///
/// ```no_run
/// use drasi_lib::{DrasiLib, Query};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder()
///     .with_id("my-server")
///     .with_query(
///         Query::cypher("my-query")
///             .query("MATCH (n) RETURN n")
///             .from_source("my-source")
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
///
/// # Validation
///
/// Call [`validate()`](DrasiLibConfig::validate) to check:
/// - Unique query IDs
/// - Valid storage backend references
///
/// Note: Source and reaction validation happens at runtime when instances are added.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrasiLibConfig {
    /// Unique identifier for this DrasiLib instance (defaults to UUID)
    #[serde(default = "default_id")]
    pub id: String,
    /// Default priority queue capacity for queries and reactions (default: 10000 if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_queue_capacity: Option<usize>,
    /// Default dispatch buffer capacity for sources and queries (default: 1000 if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_buffer_capacity: Option<usize>,
    /// Global storage backend definitions that can be referenced by queries
    #[serde(default)]
    pub storage_backends: Vec<StorageBackendConfig>,
    /// Query configurations
    #[serde(default)]
    pub queries: Vec<QueryConfig>,
}

impl Default for DrasiLibConfig {
    fn default() -> Self {
        Self {
            id: default_id(),
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            storage_backends: Vec::new(),
            queries: Vec::new(),
        }
    }
}

/// Configuration for a continuous query
///
/// `QueryConfig` defines a continuous query that processes data changes from sources
/// and emits incremental result updates. Queries subscribe to one or more sources and
/// maintain materialized views that update automatically as data changes.
///
/// # Query Languages
///
/// Queries can be written in either:
/// - **Cypher**: Default graph pattern matching language
/// - **GQL**: GraphQL-style queries (compiled to Cypher)
///
/// **Important**: ORDER BY, TOP, and LIMIT clauses are not supported in continuous
/// queries as they conflict with incremental result computation.
///
/// # Bootstrap Processing
///
/// - **enableBootstrap**: Controls whether the query processes initial data (default: true)
/// - **bootstrapBufferSize**: Event buffer size during bootstrap phase (default: 10000)
///
/// During bootstrap, events are buffered to maintain ordering while initial data loads.
/// After bootstrap completes, queries switch to incremental processing mode.
///
/// # Synthetic Joins
///
/// Queries can define synthetic relationships between node types from different sources
/// via the `joins` field. This creates virtual edges based on property equality without
/// requiring physical relationships in the source data.
///
/// # Configuration Fields
///
/// - **id**: Unique identifier (referenced by reactions)
/// - **query**: Query string in specified language
/// - **queryLanguage**: Cypher or GQL (default: Cypher)
/// - **sources**: Source IDs to subscribe to
/// - **auto_start**: Start automatically (default: true)
/// - **joins**: Optional synthetic join definitions
/// - **enableBootstrap**: Process initial data (default: true)
/// - **bootstrapBufferSize**: Buffer size during bootstrap (default: 10000)
/// - **priority_queue_capacity**: Out-of-order event queue size (overrides global)
/// - **dispatch_buffer_capacity**: Output buffer size (overrides global)
/// - **dispatch_mode**: Broadcast or Channel routing
///
/// # Examples
///
/// ## Basic Cypher Query
///
/// ```yaml
/// queries:
///   - id: active_orders
///     query: "MATCH (o:Order) WHERE o.status = 'active' RETURN o"
///     queryLanguage: Cypher  # Optional, this is default
///     sources: [orders_db]
///     auto_start: true
///     enableBootstrap: true
///     bootstrapBufferSize: 10000
/// ```
///
/// ## Query with Multiple Sources
///
/// ```yaml
/// queries:
///   - id: order_customer_join
///     query: |
///       MATCH (o:Order)-[:BELONGS_TO]->(c:Customer)
///       WHERE o.status = 'active'
///       RETURN o, c
///     sources: [orders_db, customers_db]
/// ```
///
/// ## Query with Synthetic Joins
///
/// ```yaml
/// queries:
///   - id: synthetic_join_query
///     query: |
///       MATCH (o:Order)-[:CUSTOMER]->(c:Customer)
///       RETURN o.id, c.name
///     sources: [orders_db, customers_db]
///     joins:
///       - id: CUSTOMER              # Relationship type in query
///         keys:
///           - label: Order
///             property: customer_id
///           - label: Customer
///             property: id
/// ```
///
/// ## High-Throughput Query
///
/// ```yaml
/// queries:
///   - id: high_volume_processing
///     query: "MATCH (n:Event) WHERE n.timestamp > timestamp() - 60000 RETURN n"
///     sources: [event_stream]
///     priority_queue_capacity: 100000  # Large queue for many out-of-order events
///     dispatch_buffer_capacity: 10000  # Large output buffer
///     bootstrapBufferSize: 50000       # Large bootstrap buffer
/// ```
///
/// ## GQL Query
///
/// ```yaml
/// queries:
///   - id: gql_users
///     query: |
///       {
///         users(status: "active") {
///           id
///           name
///           email
///         }
///       }
///     queryLanguage: GQL
///     sources: [users_db]
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryConfig {
    /// Unique identifier for the query
    pub id: String,
    /// Query string (Cypher or GQL depending on query_language)
    pub query: String,
    /// Query language to use (default: Cypher)
    #[serde(default, rename = "queryLanguage")]
    pub query_language: QueryLanguage,
    /// Middleware configurations for this query
    #[serde(default)]
    pub middleware: Vec<SourceMiddlewareConfig>,
    /// Source subscriptions with optional middleware pipelines
    #[serde(default)]
    pub sources: Vec<SourceSubscriptionConfig>,
    /// Whether to automatically start this query (default: true)
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    /// Optional synthetic joins for the query
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joins: Option<Vec<QueryJoinConfig>>,
    /// Whether to enable bootstrap (default: true)
    #[serde(default = "default_enable_bootstrap", rename = "enableBootstrap")]
    pub enable_bootstrap: bool,
    /// Maximum number of events to buffer during bootstrap (default: 10000)
    #[serde(
        default = "default_bootstrap_buffer_size",
        rename = "bootstrapBufferSize"
    )]
    pub bootstrap_buffer_size: usize,
    /// Priority queue capacity for this query (default: server global, or 10000 if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_queue_capacity: Option<usize>,
    /// Dispatch buffer capacity for this query (default: server global, or 1000 if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_buffer_capacity: Option<usize>,
    /// Dispatch mode for this query (default: Channel)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_mode: Option<DispatchMode>,
    /// Storage backend for this query (default: in-memory)
    /// Can reference a named backend or provide inline configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage_backend: Option<StorageBackendRef>,
}

/// Synthetic join configuration for queries
///
/// `QueryJoinConfig` defines a virtual relationship between node types from different
/// sources. This allows queries to join data without requiring physical relationships
/// in the source systems.
///
/// # Join Semantics
///
/// Joins create synthetic edges by matching property values across nodes. The `id`
/// field specifies the relationship type used in the query's MATCH pattern, and `keys`
/// define which properties to match.
///
/// # Examples
///
/// ## Simple Join on Single Property
///
/// ```yaml
/// joins:
///   - id: CUSTOMER              # Use in query: MATCH (o:Order)-[:CUSTOMER]->(c:Customer)
///     keys:
///       - label: Order
///         property: customer_id  # Order.customer_id
///       - label: Customer
///         property: id           # Customer.id
///       # Creates edge when Order.customer_id == Customer.id
/// ```
///
/// ## Multi-Source Join
///
/// ```yaml
/// joins:
///   - id: ASSIGNED_TO
///     keys:
///       - label: Task
///         property: assignee_id
///       - label: User
///         property: user_id
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryJoinConfig {
    /// Unique identifier for the join (should match relationship type in query)
    pub id: String,
    /// Keys defining the join relationship
    pub keys: Vec<QueryJoinKeyConfig>,
}

/// Join key specification for synthetic joins
///
/// `QueryJoinKeyConfig` specifies one side of a join condition by identifying
/// a node label and the property to use for matching.
///
/// # Example
///
/// For joining orders to customers:
///
/// ```yaml
/// keys:
///   - label: Order
///     property: customer_id
///   - label: Customer
///     property: id
/// ```
///
/// This creates an edge when `Order.customer_id == Customer.id`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryJoinKeyConfig {
    /// Node label to match
    pub label: String,
    /// Property to use for joining
    pub property: String,
}

impl DrasiLibConfig {
    /// Validate configuration consistency and references
    ///
    /// Performs comprehensive validation checks:
    /// - Ensures all query IDs are unique
    /// - Validates storage backend configurations
    ///
    /// Note: Source and reaction validation happens at runtime when instances are added,
    /// since drasi-lib has no knowledge of specific plugin configurations.
    ///
    /// # Errors
    ///
    /// Returns error if validation fails with a description of the problem.
    pub fn validate(&self) -> Result<()> {
        // Validate unique query ids
        let mut query_ids = std::collections::HashSet::new();
        for query in &self.queries {
            if !query_ids.insert(&query.id) {
                return Err(anyhow::anyhow!("Duplicate query id: '{}'", query.id));
            }
        }

        // Validate unique storage backend ids
        let mut storage_backend_ids = std::collections::HashSet::new();
        for backend in &self.storage_backends {
            if !storage_backend_ids.insert(&backend.id) {
                return Err(anyhow::anyhow!(
                    "Duplicate storage backend id: '{}'",
                    backend.id
                ));
            }
            // Validate backend configuration
            backend.spec.validate().map_err(|e| {
                anyhow::anyhow!(
                    "Storage backend '{}' has invalid configuration: {}",
                    backend.id,
                    e
                )
            })?;
        }

        // Validate storage backend references in queries
        for query in &self.queries {
            if let Some(backend_ref) = &query.storage_backend {
                match backend_ref {
                    StorageBackendRef::Named(backend_id) => {
                        if !storage_backend_ids.contains(backend_id) {
                            return Err(anyhow::anyhow!(
                                "Query '{}' references unknown storage backend: '{}'",
                                query.id,
                                backend_id
                            ));
                        }
                    }
                    StorageBackendRef::Inline(spec) => {
                        // Validate inline backend configuration
                        spec.validate().map_err(|e| {
                            anyhow::anyhow!(
                                "Query '{}' has invalid inline storage backend configuration: {}",
                                query.id,
                                e
                            )
                        })?;
                    }
                }
            }
        }

        Ok(())
    }
}

fn default_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn default_auto_start() -> bool {
    true
}

fn default_enable_bootstrap() -> bool {
    true
}

fn default_bootstrap_buffer_size() -> usize {
    10000
}

// Conversion implementations for QueryJoin types
impl From<QueryJoinKeyConfig> for drasi_core::models::QueryJoinKey {
    fn from(config: QueryJoinKeyConfig) -> Self {
        drasi_core::models::QueryJoinKey {
            label: config.label,
            property: config.property,
        }
    }
}

impl From<QueryJoinConfig> for drasi_core::models::QueryJoin {
    fn from(config: QueryJoinConfig) -> Self {
        drasi_core::models::QueryJoin {
            id: config.id,
            keys: config.keys.into_iter().map(|k| k.into()).collect(),
        }
    }
}
