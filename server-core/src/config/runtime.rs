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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

use super::schema::{QueryConfig, ReactionConfig, SourceConfig};
use crate::channels::ComponentStatus;
use crate::indexes::IndexFactory;

/// Helper function to convert typed config to properties HashMap
fn serialize_to_properties<T: serde::Serialize>(config: &T) -> HashMap<String, serde_json::Value> {
    match serde_json::to_value(config) {
        Ok(serde_json::Value::Object(map)) => map
            .into_iter()
            .filter(|(k, _)| k != "source_type" && k != "reaction_type")
            .collect(),
        _ => HashMap::new(),
    }
}

/// Runtime representation of a source with execution status
///
/// `SourceRuntime` combines configuration with runtime state information like
/// current execution status and error messages. It's used for monitoring and
/// managing source lifecycle.
///
/// # Status Values
///
/// - `ComponentStatus::Stopped`: Source is configured but not running
/// - `ComponentStatus::Starting`: Source is initializing
/// - `ComponentStatus::Running`: Source is actively ingesting data
/// - `ComponentStatus::Error`: Source encountered an error (see `error_message`)
///
/// # Thread Safety
///
/// This struct is `Clone` and `Serialize` for sharing across threads and APIs.
///
/// # Examples
///
/// ```no_run
/// use drasi_server_core::{DrasiServerCore, ComponentStatus};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::from_config_file("config.yaml").await?;
/// core.start().await?;
///
/// // Get runtime information for a source
/// let source_info = core.get_source_info("orders_db").await?;
/// println!("Source {} is {:?}", source_info.id, source_info.status);
///
/// if let Some(error) = source_info.error_message {
///     eprintln!("Source error: {}", error);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceRuntime {
    /// Unique identifier for the source
    pub id: String,
    /// Type of source (e.g., "postgres", "http", "mock", "platform")
    pub source_type: String,
    /// Current status of the source
    pub status: ComponentStatus,
    /// Error message if status is Error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Source-specific configuration properties
    pub properties: HashMap<String, serde_json::Value>,
}

/// Runtime representation of a query with execution status
///
/// `QueryRuntime` combines query configuration with runtime state information.
/// Used for monitoring query execution, tracking which sources it subscribes to,
/// and inspecting any runtime errors.
///
/// # Status Values
///
/// - `ComponentStatus::Stopped`: Query is configured but not processing
/// - `ComponentStatus::Starting`: Query is initializing (bootstrap phase)
/// - `ComponentStatus::Running`: Query is actively processing events
/// - `ComponentStatus::Error`: Query encountered an error (see `error_message`)
///
/// # Examples
///
/// ```no_run
/// use drasi_server_core::{DrasiServerCore, ComponentStatus};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::from_config_file("config.yaml").await?;
/// core.start().await?;
///
/// // Get runtime information for a query
/// let query_info = core.get_query_info("active_orders").await?;
/// println!("Query: {}", query_info.query);
/// println!("Status: {:?}", query_info.status);
/// println!("Sources: {:?}", query_info.sources);
///
/// if let Some(joins) = query_info.joins {
///     println!("Synthetic joins configured: {}", joins.len());
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRuntime {
    /// Unique identifier for the query
    pub id: String,
    /// Cypher or GQL query string
    pub query: String,
    /// Current status of the query
    pub status: ComponentStatus,
    /// Error message if status is Error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Source subscriptions with middleware pipelines
    pub source_subscriptions: Vec<super::schema::SourceSubscriptionConfig>,
    /// Optional synthetic joins for the query
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joins: Option<Vec<super::schema::QueryJoinConfig>>,
}

/// Runtime representation of a reaction with execution status
///
/// `ReactionRuntime` combines reaction configuration with runtime state information.
/// Used for monitoring reaction execution, tracking which queries it subscribes to,
/// and inspecting delivery status.
///
/// # Status Values
///
/// - `ComponentStatus::Stopped`: Reaction is configured but not running
/// - `ComponentStatus::Starting`: Reaction is initializing connections
/// - `ComponentStatus::Running`: Reaction is actively delivering results
/// - `ComponentStatus::Error`: Reaction encountered an error (see `error_message`)
///
/// # Examples
///
/// ```no_run
/// use drasi_server_core::{DrasiServerCore, ComponentStatus};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiServerCore::from_config_file("config.yaml").await?;
/// core.start().await?;
///
/// // Get runtime information for a reaction
/// let reaction_info = core.get_reaction_info("order_webhook").await?;
/// println!("Reaction {} ({}) is {:?}",
///     reaction_info.id,
///     reaction_info.reaction_type,
///     reaction_info.status
/// );
/// println!("Subscribed to queries: {:?}", reaction_info.queries);
///
/// if let Some(error) = reaction_info.error_message {
///     eprintln!("Reaction error: {}", error);
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReactionRuntime {
    /// Unique identifier for the reaction
    pub id: String,
    /// Type of reaction (e.g., "log", "http", "grpc", "sse", "platform")
    pub reaction_type: String,
    /// Current status of the reaction
    pub status: ComponentStatus,
    /// Error message if status is Error
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// IDs of queries this reaction subscribes to
    pub queries: Vec<String>,
    /// Reaction-specific configuration properties
    pub properties: HashMap<String, serde_json::Value>,
}

impl From<SourceConfig> for SourceRuntime {
    fn from(config: SourceConfig) -> Self {
        let id = config.id.clone();
        let source_type = config.source_type().to_string();
        let properties = serialize_to_properties(&config.config);
        Self {
            id,
            source_type,
            status: ComponentStatus::Stopped,
            error_message: None,
            properties,
        }
    }
}

impl From<QueryConfig> for QueryRuntime {
    fn from(config: QueryConfig) -> Self {
        Self {
            id: config.id,
            query: config.query,
            status: ComponentStatus::Stopped,
            error_message: None,
            source_subscriptions: config.source_subscriptions,
            joins: config.joins,
        }
    }
}

impl From<ReactionConfig> for ReactionRuntime {
    fn from(config: ReactionConfig) -> Self {
        let id = config.id.clone();
        let reaction_type = config.reaction_type().to_string();
        let queries = config.queries.clone();
        let properties = serialize_to_properties(&config.config);
        Self {
            id,
            reaction_type,
            status: ComponentStatus::Stopped,
            error_message: None,
            queries,
            properties,
        }
    }
}

/// Runtime configuration with applied defaults
///
/// `RuntimeConfig` represents a fully-resolved configuration with all global defaults
/// applied to individual components. It's created from [`DrasiServerCoreConfig`](super::schema::DrasiServerCoreConfig)
/// and used internally by [`DrasiServerCore`](crate::DrasiServerCore) for execution.
///
/// # Default Application
///
/// When converting from `DrasiServerCoreConfig` to `RuntimeConfig`, global capacity
/// settings are applied to components that don't specify their own values:
///
/// - **priority_queue_capacity**: Applied to queries and reactions (default: 10000)
/// - **dispatch_buffer_capacity**: Applied to sources and queries (default: 1000)
///
/// # Conversion
///
/// Automatically created via `From<DrasiServerCoreConfig>`:
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
///
/// # Examples
///
/// ## Creating RuntimeConfig
///
/// ```no_run
/// use drasi_server_core::{DrasiServerCore, DrasiServerCoreConfig, RuntimeConfig};
/// use std::sync::Arc;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Load and convert to runtime config
/// let config = DrasiServerCoreConfig::load_from_file("config.yaml")?;
/// let runtime_config: RuntimeConfig = config.into();
///
/// // Use with DrasiServerCore (via from_config_file helper)
/// let core = DrasiServerCore::from_config_file("config.yaml").await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Understanding Default Application
///
/// ```yaml
/// server_core:
///   priority_queue_capacity: 50000  # Global default
///
/// queries:
///   - id: q1
///     query: "MATCH (n) RETURN n"
///     sources: [s1]
///     # priority_queue_capacity will be 50000 (inherited)
///
///   - id: q2
///     query: "MATCH (m) RETURN m"
///     sources: [s1]
///     priority_queue_capacity: 100000  # Override global
/// ```
///
/// After conversion to `RuntimeConfig`:
/// - `q1` will have `priority_queue_capacity = Some(50000)`
/// - `q2` will have `priority_queue_capacity = Some(100000)`
#[derive(Debug, Clone)]
pub struct RuntimeConfig {
    pub server_core: super::schema::DrasiServerCoreSettings,
    /// Index factory for creating storage backend indexes for queries
    pub index_factory: Arc<IndexFactory>,
    pub sources: Vec<SourceConfig>,
    pub queries: Vec<QueryConfig>,
    pub reactions: Vec<ReactionConfig>,
}

impl From<super::schema::DrasiServerCoreConfig> for RuntimeConfig {
    fn from(config: super::schema::DrasiServerCoreConfig) -> Self {
        // Get the global defaults (or hardcoded fallbacks)
        let global_priority_queue = config.server_core.priority_queue_capacity.unwrap_or(10000);
        let global_dispatch_capacity = config.server_core.dispatch_buffer_capacity.unwrap_or(1000);

        // Create IndexFactory from storage backend configurations
        let index_factory = Arc::new(IndexFactory::new(config.storage_backends));

        // Apply global defaults to sources that don't specify their own dispatch capacity
        let sources = config
            .sources
            .into_iter()
            .map(|mut s| {
                if s.dispatch_buffer_capacity.is_none() {
                    s.dispatch_buffer_capacity = Some(global_dispatch_capacity);
                }
                s
            })
            .collect();

        // Apply global defaults to queries
        let queries = config
            .queries
            .into_iter()
            .map(|mut q| {
                if q.priority_queue_capacity.is_none() {
                    q.priority_queue_capacity = Some(global_priority_queue);
                }
                if q.dispatch_buffer_capacity.is_none() {
                    q.dispatch_buffer_capacity = Some(global_dispatch_capacity);
                }
                q
            })
            .collect();

        // Apply global default to reactions that don't specify their own capacity
        let reactions = config
            .reactions
            .into_iter()
            .map(|mut r| {
                if r.priority_queue_capacity.is_none() {
                    r.priority_queue_capacity = Some(global_priority_queue);
                }
                r
            })
            .collect();

        Self {
            server_core: config.server_core,
            index_factory,
            sources,
            queries,
            reactions,
        }
    }
}
