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
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::bootstrap::BootstrapProviderConfig;
use crate::channels::DispatchMode;
use crate::config::typed::{ReactionSpecificConfig, SourceSpecificConfig};

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum QueryLanguage {
    Cypher,
    GQL,
}

impl Default for QueryLanguage {
    fn default() -> Self {
        QueryLanguage::Cypher
    }
}

/// Root configuration for Drasi Server Core
///
/// `DrasiServerCoreConfig` is the top-level configuration structure for loading Drasi
/// configurations from YAML or JSON files. It defines all sources, queries, reactions,
/// and global server settings.
///
/// # Configuration File Structure
///
/// A typical configuration file has four main sections:
///
/// 1. **server_core**: Global server settings (optional)
/// 2. **sources**: Data ingestion sources
/// 3. **queries**: Continuous queries to process data
/// 4. **reactions**: Output destinations for query results
///
/// # Thread Safety
///
/// This struct is `Clone` and can be safely shared across threads.
///
/// # Loading Configuration
///
/// Use [`DrasiServerCoreConfig::load_from_file()`] to load from YAML or JSON:
///
/// ```no_run
/// use drasi_server_core::DrasiServerCoreConfig;
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;
/// config.validate()?;  // Validate references between components
/// # Ok(())
/// # }
/// ```
///
/// # Complete Configuration Example
///
/// ```yaml
/// server_core:
///   id: production-server
///   priority_queue_capacity: 50000
///   dispatch_buffer_capacity: 5000
///
/// sources:
///   - id: orders_db
///     source_type: postgres
///     auto_start: true
///     host: localhost
///     port: 5432
///     database: orders
///     user: postgres
///     password: secret
///     tables: [orders, customers]
///     table_keys:
///       - table: orders
///         key_columns: [order_id]
///       - table: customers
///         key_columns: [customer_id]
///
/// queries:
///   - id: active_orders
///     query: "MATCH (o:Order) WHERE o.status = 'active' RETURN o"
///     queryLanguage: Cypher
///     sources: [orders_db]
///     auto_start: true
///     enableBootstrap: true
///     bootstrapBufferSize: 10000
///
/// reactions:
///   - id: order_webhook
///     reaction_type: http
///     queries: [active_orders]
///     auto_start: true
///     base_url: "https://api.example.com/orders"
///     timeout_ms: 10000
/// ```
///
/// # Validation
///
/// Call [`validate()`](DrasiServerCoreConfig::validate) to check:
/// - Unique component IDs
/// - Valid source references in queries
/// - Valid query references in reactions
///
/// # Conversion to Runtime Config
///
/// Convert to [`RuntimeConfig`](crate::RuntimeConfig) for server execution:
///
/// ```no_run
/// use drasi_server_core::{DrasiServerCoreConfig, RuntimeConfig};
///
/// # fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;
/// let runtime_config: RuntimeConfig = config.into();
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DrasiServerCoreConfig {
    #[serde(default)]
    pub server_core: DrasiServerCoreSettings,
    pub sources: Vec<SourceConfig>,
    pub queries: Vec<QueryConfig>,
    pub reactions: Vec<ReactionConfig>,
}

/// Global server settings for Drasi Server Core
///
/// `DrasiServerCoreSettings` configures global behavior and default capacity settings
/// that apply to all components unless overridden at the component level.
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
/// # Component Overrides
///
/// Individual components can override these defaults by setting their own capacity values.
/// This allows fine-tuning performance for specific high-throughput components while
/// keeping reasonable defaults for others.
///
/// # Examples
///
/// ## Default Settings
///
/// ```yaml
/// server_core:
///   id: my-server
/// # priority_queue_capacity defaults to 10000
/// # dispatch_buffer_capacity defaults to 1000
/// ```
///
/// ## High-Throughput Configuration
///
/// ```yaml
/// server_core:
///   id: production-server
///   priority_queue_capacity: 50000   # Large queues for high event volumes
///   dispatch_buffer_capacity: 5000   # Large buffers for throughput
/// ```
///
/// ## Component-Specific Override
///
/// ```yaml
/// server_core:
///   priority_queue_capacity: 10000  # Default for most components
///
/// queries:
///   - id: high_volume_query
///     priority_queue_capacity: 100000  # Override for this specific query
///     query: "MATCH (n) RETURN n"
///     sources: [data_source]
/// ```
///
/// # Performance Considerations
///
/// **Priority Queue Capacity**:
/// - Too small: Out-of-order events may be dropped
/// - Too large: Higher memory usage
/// - Recommended: 10000-50000 for most workloads
///
/// **Dispatch Buffer Capacity**:
/// - Too small: Backpressure may slow down upstream components
/// - Too large: Higher memory usage, longer shutdown times
/// - Recommended: 1000-10000 for most workloads
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrasiServerCoreSettings {
    #[serde(default = "default_id")]
    pub id: String,
    /// Default priority queue capacity for queries and reactions (default: 10000 if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_queue_capacity: Option<usize>,
    /// Default dispatch buffer capacity for sources and queries (default: 1000 if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_buffer_capacity: Option<usize>,
}

/// Configuration for a data ingestion source
///
/// `SourceConfig` defines how Drasi ingests data from external systems. Each source
/// continuously monitors data changes and sends them to subscribed queries.
///
/// # Source Types
///
/// Sources are configured via the `source_type` discriminator field which determines
/// the type-specific properties available:
///
/// - **postgres**: PostgreSQL replication via logical decoding
/// - **http**: HTTP endpoint for change events (adaptive batching)
/// - **grpc**: gRPC streaming endpoint
/// - **mock**: Generated test data
/// - **platform**: Redis Streams integration with Drasi Platform
/// - **application**: Direct application integration via handles
///
/// # Bootstrap Providers
///
/// All sources support pluggable bootstrap providers for initial data delivery,
/// independent from streaming configuration. This enables powerful patterns like
/// "bootstrap from database, stream from HTTP" or "bootstrap from file for testing".
///
/// # Configuration Fields
///
/// - **id**: Unique identifier (referenced by queries)
/// - **source_type**: Discriminator determining source implementation
/// - **auto_start**: Whether to start automatically (default: true)
/// - **bootstrap_provider**: Optional bootstrap configuration
/// - **dispatch_buffer_capacity**: Event buffer size (overrides global default)
/// - **dispatch_mode**: Broadcast or Channel routing mode
///
/// # Examples
///
/// ## PostgreSQL Source with Standard Bootstrap
///
/// ```yaml
/// sources:
///   - id: orders_db
///     source_type: postgres
///     auto_start: true
///     host: localhost
///     port: 5432
///     database: orders
///     user: postgres
///     password: secret
///     tables: [orders, customers]
///     table_keys:
///       - table: orders
///         key_columns: [order_id]
///     bootstrap_provider:
///       type: postgres  # Use PostgreSQL snapshot bootstrap
/// ```
///
/// ## HTTP Source with PostgreSQL Bootstrap (Mix-and-Match)
///
/// ```yaml
/// sources:
///   - id: http_with_db_bootstrap
///     source_type: http
///     host: localhost
///     port: 8080
///     database: mydb           # Used by postgres bootstrap provider
///     user: dbuser
///     password: dbpass
///     tables: [stocks]
///     table_keys:
///       - table: stocks
///         key_columns: [symbol]
///     bootstrap_provider:
///       type: postgres          # Bootstrap from PostgreSQL
///     # HTTP source will stream changes after bootstrap completes
/// ```
///
/// ## Mock Source with ScriptFile Bootstrap
///
/// ```yaml
/// sources:
///   - id: test_source
///     source_type: mock
///     data_type: sensor
///     interval_ms: 1000
///     bootstrap_provider:
///       type: scriptfile
///       file_paths:
///         - "/path/to/test_data.jsonl"
/// ```
///
/// ## Platform Source (Redis Streams)
///
/// ```yaml
/// sources:
///   - id: platform_events
///     source_type: platform
///     redis_url: "redis://localhost:6379"
///     stream_key: "sensor-data:changes"
///     consumer_group: "drasi-core"
///     batch_size: 10
/// ```
///
/// ## High-Throughput Configuration
///
/// ```yaml
/// sources:
///   - id: high_volume_source
///     source_type: http
///     host: localhost
///     port: 9000
///     dispatch_buffer_capacity: 10000  # Override global default
///     dispatch_mode: Channel           # Or Broadcast for fanout
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    /// Unique identifier for the source
    pub id: String,
    /// Whether to automatically start this source (default: true)
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    /// Typed source-specific configuration (contains source_type as discriminator)
    #[serde(flatten)]
    pub config: SourceSpecificConfig,
    /// Optional bootstrap provider configuration
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bootstrap_provider: Option<BootstrapProviderConfig>,
    /// Dispatch buffer capacity for this source (default: server global, or 1000 if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_buffer_capacity: Option<usize>,
    /// Dispatch mode for this source (default: Channel)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_mode: Option<DispatchMode>,
}

impl SourceConfig {
    /// Get the source type as a string from the enum variant
    pub fn source_type(&self) -> &str {
        match &self.config {
            SourceSpecificConfig::Mock(_) => "mock",
            SourceSpecificConfig::Postgres(_) => "postgres",
            SourceSpecificConfig::Http(_) => "http",
            SourceSpecificConfig::Grpc(_) => "grpc",
            SourceSpecificConfig::Platform(_) => "platform",
            SourceSpecificConfig::Application(_) => "application",
            SourceSpecificConfig::Custom { .. } => "custom",
        }
    }

    /// Get typed configuration properties as HashMap for backward compatibility
    pub fn get_properties(&self) -> HashMap<String, serde_json::Value> {
        match serde_json::to_value(&self.config) {
            Ok(serde_json::Value::Object(map)) => map
                .into_iter()
                .filter(|(k, _)| k != "source_type")
                .collect(),
            _ => HashMap::new(),
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
    /// IDs of sources this query subscribes to
    pub sources: Vec<String>,
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

/// Configuration for a reaction (output destination)
///
/// `ReactionConfig` defines how query results are delivered to external systems. Reactions
/// subscribe to one or more queries and receive incremental result updates as data changes.
///
/// # Reaction Types
///
/// Reactions are configured via the `reaction_type` discriminator field which determines
/// the type-specific properties available:
///
/// - **log**: Write results to application logs (debug/info/warn/error levels)
/// - **http**: Send results to HTTP webhooks with configurable retry
/// - **grpc**: Stream results via gRPC with batching support
/// - **sse**: Expose Server-Sent Events endpoint for client subscriptions
/// - **platform**: Publish to Redis Streams for Drasi Platform integration
/// - **profiler**: Performance monitoring and profiling output
/// - **application**: Direct application integration via handles
///
/// # Result Delivery
///
/// Reactions receive **three types of events** from queries:
/// - **added**: New results matching the query
/// - **updated**: Existing results that changed
/// - **deleted**: Results that no longer match
///
/// Each reaction type handles these events according to its specific protocol.
///
/// # Configuration Fields
///
/// - **id**: Unique identifier
/// - **reaction_type**: Discriminator determining reaction implementation
/// - **queries**: Query IDs to subscribe to
/// - **auto_start**: Start automatically (default: true)
/// - **priority_queue_capacity**: Event queue size for out-of-order handling (overrides global)
///
/// # Examples
///
/// ## Log Reaction
///
/// ```yaml
/// reactions:
///   - id: debug_logger
///     reaction_type: log
///     queries: [active_orders]
///     auto_start: true
///     log_level: info  # trace, debug, info, warn, error
/// ```
///
/// ## HTTP Webhook Reaction
///
/// ```yaml
/// reactions:
///   - id: order_webhook
///     reaction_type: http
///     queries: [active_orders, pending_orders]
///     auto_start: true
///     base_url: "https://api.example.com/webhooks/orders"
///     token: "secret-token"
///     timeout_ms: 10000
///     queries:
///       active_orders:
///         added:
///           url: "/new-order"
///           method: POST
///           body: "{{payload}}"
///         updated:
///           url: "/update-order"
///           method: PUT
///         deleted:
///           url: "/cancel-order"
///           method: DELETE
/// ```
///
/// ## gRPC Streaming Reaction
///
/// ```yaml
/// reactions:
///   - id: grpc_stream
///     reaction_type: grpc
///     queries: [events]
///     endpoint: "grpc://service.example.com:50051"
///     token: "bearer-token"
///     batch_size: 100
///     batch_flush_timeout_ms: 1000
///     max_retries: 3
/// ```
///
/// ## Server-Sent Events Reaction
///
/// ```yaml
/// reactions:
///   - id: sse_endpoint
///     reaction_type: sse
///     queries: [live_updates]
///     host: "0.0.0.0"
///     port: 8080
///     sse_path: "/events"
///     heartbeat_interval_ms: 30000
/// ```
///
/// ## Platform Reaction (Redis Streams)
///
/// ```yaml
/// reactions:
///   - id: platform_publisher
///     reaction_type: platform
///     queries: [processed_data]
///     redis_url: "redis://localhost:6379"
///     pubsub_name: "drasi-pubsub"
///     source_name: "processed-results"
///     emit_control_events: true
///     batch_enabled: true
///     batch_max_size: 100
/// ```
///
/// ## High-Throughput Reaction
///
/// ```yaml
/// reactions:
///   - id: high_volume_reaction
///     reaction_type: http
///     queries: [event_stream]
///     base_url: "https://api.example.com/events"
///     priority_queue_capacity: 100000  # Large queue for out-of-order events
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionConfig {
    /// Unique identifier for the reaction
    pub id: String,
    /// IDs of queries this reaction subscribes to
    pub queries: Vec<String>,
    /// Whether to automatically start this reaction (default: true)
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    /// Typed reaction-specific configuration (contains reaction_type as discriminator)
    #[serde(flatten)]
    pub config: ReactionSpecificConfig,
    /// Priority queue capacity for this reaction (default: server global, or 10000 if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_queue_capacity: Option<usize>,
}

impl ReactionConfig {
    /// Get the reaction type as a string from the enum variant
    pub fn reaction_type(&self) -> &str {
        match &self.config {
            ReactionSpecificConfig::Log(_) => "log",
            ReactionSpecificConfig::Http(_) => "http",
            ReactionSpecificConfig::Grpc(_) => "grpc",
            ReactionSpecificConfig::Sse(_) => "sse",
            ReactionSpecificConfig::Platform(_) => "platform",
            ReactionSpecificConfig::Profiler(_) => "profiler",
            ReactionSpecificConfig::Application(_) => "application",
            ReactionSpecificConfig::Custom { .. } => "custom",
        }
    }

    /// Get typed configuration properties as HashMap for backward compatibility
    pub fn get_properties(&self) -> HashMap<String, serde_json::Value> {
        match serde_json::to_value(&self.config) {
            Ok(serde_json::Value::Object(map)) => map
                .into_iter()
                .filter(|(k, _)| k != "reaction_type")
                .collect(),
            _ => HashMap::new(),
        }
    }
}

impl DrasiServerCoreConfig {
    /// Load configuration from a YAML or JSON file
    ///
    /// Attempts to parse the file as YAML first, then falls back to JSON if YAML parsing fails.
    /// Returns a detailed error if both formats fail.
    ///
    /// # Arguments
    ///
    /// * `path` - Path to configuration file (.yaml, .yml, or .json)
    ///
    /// # Returns
    ///
    /// Parsed configuration on success, or error describing parse failures.
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - File cannot be read
    /// - Content is not valid YAML or JSON
    /// - Configuration structure doesn't match expected schema
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use drasi_server_core::DrasiServerCoreConfig;
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// // Load from YAML
    /// let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;
    ///
    /// // Load from JSON
    /// let config_json = DrasiServerCoreConfig::load_from_file("config.json")?;
    ///
    /// // Validate the configuration
    /// config.validate()?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let content = fs::read_to_string(path_ref).map_err(|e| {
            anyhow::anyhow!("Failed to read config file {}: {}", path_ref.display(), e)
        })?;

        // Try YAML first, then JSON
        match serde_yaml::from_str::<DrasiServerCoreConfig>(&content) {
            Ok(config) => Ok(config),
            Err(yaml_err) => {
                // If YAML fails, try JSON
                match serde_json::from_str::<DrasiServerCoreConfig>(&content) {
                    Ok(config) => Ok(config),
                    Err(json_err) => {
                        // Both failed, return detailed error
                        Err(anyhow::anyhow!(
                            "Failed to parse config file '{}':\n  YAML error: {}\n  JSON error: {}",
                            path_ref.display(),
                            yaml_err,
                            json_err
                        ))
                    }
                }
            }
        }
    }

    /// Save configuration to a YAML file
    ///
    /// Serializes the configuration to YAML format and writes to the specified file path.
    ///
    /// # Arguments
    ///
    /// * `path` - Destination file path
    ///
    /// # Errors
    ///
    /// Returns error if:
    /// - Configuration cannot be serialized to YAML
    /// - File cannot be written
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use drasi_server_core::DrasiServerCoreConfig;
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;
    /// config.save_to_file("config_backup.yaml")?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn save_to_file<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let content = serde_yaml::to_string(self)?;
        fs::write(path, content)?;
        Ok(())
    }

    /// Validate configuration consistency and references
    ///
    /// Performs comprehensive validation checks:
    /// - Ensures all component IDs (sources, queries, reactions) are unique
    /// - Validates that queries reference existing sources
    /// - Validates that reactions reference existing queries
    ///
    /// # Errors
    ///
    /// Returns error if validation fails with a description of the problem.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use drasi_server_core::DrasiServerCoreConfig;
    ///
    /// # fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;
    ///
    /// // Validate before using
    /// match config.validate() {
    ///     Ok(()) => println!("Configuration is valid"),
    ///     Err(e) => eprintln!("Invalid configuration: {}", e),
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn validate(&self) -> Result<()> {
        // Validate unique source ids
        let mut source_ids = std::collections::HashSet::new();
        for source in &self.sources {
            if !source_ids.insert(&source.id) {
                return Err(anyhow::anyhow!("Duplicate source id: '{}'", source.id));
            }
        }

        // Validate unique query ids
        let mut query_ids = std::collections::HashSet::new();
        for query in &self.queries {
            if !query_ids.insert(&query.id) {
                return Err(anyhow::anyhow!("Duplicate query id: '{}'", query.id));
            }
        }

        // Validate unique reaction ids
        let mut reaction_ids = std::collections::HashSet::new();
        for reaction in &self.reactions {
            if !reaction_ids.insert(&reaction.id) {
                return Err(anyhow::anyhow!("Duplicate reaction id: '{}'", reaction.id));
            }
        }

        // Validate source references in queries
        for query in &self.queries {
            for source_id in &query.sources {
                if !source_ids.contains(source_id) {
                    return Err(anyhow::anyhow!(
                        "Query '{}' references unknown source: '{}'",
                        query.id,
                        source_id
                    ));
                }
            }
        }

        // Validate query references in reactions
        for reaction in &self.reactions {
            for query_id in &reaction.queries {
                if !query_ids.contains(query_id) {
                    return Err(anyhow::anyhow!(
                        "Reaction '{}' references unknown query: '{}'",
                        reaction.id,
                        query_id
                    ));
                }
            }
        }

        Ok(())
    }
}

impl Default for DrasiServerCoreSettings {
    fn default() -> Self {
        Self {
            // Default server ID to a random UUID
            id: uuid::Uuid::new_v4().to_string(),
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
        }
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
