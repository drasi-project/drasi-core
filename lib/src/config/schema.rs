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
use std::collections::{BTreeMap, HashSet};

use crate::channels::DispatchMode;
use crate::indexes::{StorageBackendConfig, StorageBackendRef};
use crate::recovery::RecoveryPolicy;
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

/// Best-effort schema information reported by a source.
///
/// Sources may return this from `Source::describe_schema()` so higher layers
/// (such as inspection APIs or MCP adapters) can understand the graph shape
/// without reverse-engineering source-specific configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct SourceSchema {
    #[serde(default)]
    pub nodes: Vec<NodeSchema>,
    #[serde(default)]
    pub relations: Vec<RelationSchema>,
}

impl SourceSchema {
    /// Returns true when the schema contains no node or relation declarations.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.relations.is_empty()
    }
}

/// Schema for a single node label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct NodeSchema {
    pub label: String,
    #[serde(default)]
    pub properties: Vec<PropertySchema>,
}

impl NodeSchema {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            properties: Vec::new(),
        }
    }
}

/// Schema for a single relationship label.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct RelationSchema {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default)]
    pub properties: Vec<PropertySchema>,
}

impl RelationSchema {
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            from: None,
            to: None,
            properties: Vec::new(),
        }
    }
}

/// Schema for a single property.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct PropertySchema {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<PropertyType>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl PropertySchema {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            data_type: None,
            description: None,
        }
    }
}

/// Cross-source property type hints for schema discovery.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum PropertyType {
    String,
    Integer,
    Float,
    Boolean,
    Timestamp,
    Json,
}

/// Merged graph schema used by inspection APIs and future MCP tools.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct GraphSchema {
    #[serde(default)]
    pub nodes: BTreeMap<String, GraphNodeSchema>,
    #[serde(default)]
    pub relations: BTreeMap<String, GraphRelationSchema>,
    #[serde(default)]
    pub sources_without_schema: Vec<String>,
}

impl GraphSchema {
    /// Merge a source-provided schema into the aggregate graph view.
    pub fn merge_source_schema(&mut self, source_id: &str, schema: &SourceSchema) {
        for node in &schema.nodes {
            let entry = self.nodes.entry(node.label.clone()).or_default();
            push_unique(&mut entry.sources, source_id.to_string());
            merge_properties(&mut entry.properties, &node.properties);
        }

        for relation in &schema.relations {
            let entry = self.relations.entry(relation.label.clone()).or_default();
            push_unique(&mut entry.sources, source_id.to_string());

            if entry.from.is_none() {
                entry.from = relation.from.clone();
            }
            if entry.to.is_none() {
                entry.to = relation.to.clone();
            }

            merge_properties(&mut entry.properties, &relation.properties);
        }
    }

    /// Mark node labels as being referenced by a query.
    pub fn mark_queried_nodes<'a, I>(&mut self, labels: I, query_id: &str)
    where
        I: IntoIterator<Item = &'a str>,
    {
        for label in labels {
            let entry = self.nodes.entry(label.to_string()).or_default();
            push_unique(&mut entry.queried_by, query_id.to_string());
        }
    }

    /// Mark relationship labels as being referenced by a query.
    pub fn mark_queried_relations<'a, I>(&mut self, labels: I, query_id: &str)
    where
        I: IntoIterator<Item = &'a str>,
    {
        for label in labels {
            let entry = self.relations.entry(label.to_string()).or_default();
            push_unique(&mut entry.queried_by, query_id.to_string());
        }
    }

    /// Record a source that exists but could not describe its schema.
    pub fn record_source_without_schema(&mut self, source_id: &str) {
        push_unique(&mut self.sources_without_schema, source_id.to_string());
    }
}

/// Aggregated node schema across one or more sources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct GraphNodeSchema {
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub queried_by: Vec<String>,
    #[serde(default)]
    pub properties: Vec<PropertySchema>,
}

/// Aggregated relationship schema across one or more sources.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct GraphRelationSchema {
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub queried_by: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default)]
    pub properties: Vec<PropertySchema>,
}

fn push_unique(values: &mut Vec<String>, value: String) {
    match values.binary_search(&value) {
        Ok(_) => {}
        Err(pos) => values.insert(pos, value),
    }
}

fn merge_properties(target: &mut Vec<PropertySchema>, incoming: &[PropertySchema]) {
    for property in incoming {
        if let Some(existing) = target.iter_mut().find(|p| p.name == property.name) {
            if existing.data_type.is_none() {
                existing.data_type = property.data_type;
            }
            if existing.description.is_none() {
                existing.description = property.description.clone();
            }
        } else {
            target.push(property.clone());
        }
    }

    target.sort_by(|a, b| a.name.cmp(&b.name));
}

/// Strip an optional schema prefix from a qualified table name to derive a node label.
///
/// For example, `"public.users"` becomes `"users"` and `"orders"` stays `"orders"`.
/// This is used by database sources (Postgres, MSSQL) when converting configured
/// table names into graph node labels.
pub fn normalize_table_label(table: &str) -> String {
    table.rsplit('.').next().unwrap_or(table).to_string()
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
///     resume_from: None,
///     request_position_handle: false,
/// };
/// ```
#[derive(Debug, Clone)]
pub struct SourceSubscriptionSettings {
    pub source_id: String,
    pub enable_bootstrap: bool,
    pub query_id: String,
    pub nodes: HashSet<String>,
    pub relations: HashSet<String>,
    /// If set, the subscribing query requests events replayed from this sequence position.
    /// Only meaningful when the source returns `supports_replay() == true`.
    pub resume_from: Option<u64>,
    /// If true, the query requests a shared `Arc<AtomicU64>` position handle in the
    /// `SubscriptionResponse` for reporting its durably-processed position back to the source.
    pub request_position_handle: bool,
}

/// Root configuration for drasi-lib
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
    /// Recovery policy when a source cannot honor a requested resume position.
    /// `None` inherits the global default (itself defaulting to `Strict`).
    /// See [`RecoveryPolicy`](crate::RecoveryPolicy).
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "recoveryPolicy"
    )]
    pub recovery_policy: Option<RecoveryPolicy>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_unique_maintains_sorted_order_and_deduplication() {
        let mut values = Vec::new();
        push_unique(&mut values, "charlie".to_string());
        push_unique(&mut values, "alpha".to_string());
        push_unique(&mut values, "bravo".to_string());
        push_unique(&mut values, "alpha".to_string()); // duplicate

        assert_eq!(values, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn normalize_table_label_strips_schema_prefix() {
        assert_eq!(normalize_table_label("public.users"), "users");
        assert_eq!(normalize_table_label("dbo.orders"), "orders");
        assert_eq!(normalize_table_label("customers"), "customers");
    }

    #[test]
    fn merge_source_schema_combines_properties() {
        let mut graph = GraphSchema::default();

        let schema_a = SourceSchema {
            nodes: vec![NodeSchema {
                label: "Sensor".to_string(),
                properties: vec![PropertySchema::new("temperature")],
            }],
            relations: Vec::new(),
        };
        let schema_b = SourceSchema {
            nodes: vec![NodeSchema {
                label: "Sensor".to_string(),
                properties: vec![
                    PropertySchema {
                        name: "temperature".to_string(),
                        data_type: Some(PropertyType::Float),
                        description: None,
                    },
                    PropertySchema::new("humidity"),
                ],
            }],
            relations: Vec::new(),
        };

        graph.merge_source_schema("src-a", &schema_a);
        graph.merge_source_schema("src-b", &schema_b);

        let sensor = graph.nodes.get("Sensor").unwrap();
        assert_eq!(sensor.sources, vec!["src-a", "src-b"]);
        assert_eq!(sensor.properties.len(), 2);

        // temperature should have gotten the type from src-b
        let temp = sensor.properties.iter().find(|p| p.name == "temperature").unwrap();
        assert_eq!(temp.data_type, Some(PropertyType::Float));
    }
}
