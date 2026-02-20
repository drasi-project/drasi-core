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

use super::schema::QueryConfig;
use crate::channels::ComponentStatus;
use crate::indexes::IndexBackendPlugin;
use crate::indexes::IndexFactory;
use crate::state_store::{MemoryStateStoreProvider, StateStoreProvider};

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
/// use drasi_lib::{DrasiLib, ComponentStatus};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder().with_id("my-server").build().await?;
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
/// use drasi_lib::{DrasiLib, ComponentStatus};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder().with_id("my-server").build().await?;
/// core.start().await?;
///
/// // Get runtime information for a query
/// let query_info = core.get_query_info("active_orders").await?;
/// println!("Query: {}", query_info.query);
/// println!("Status: {:?}", query_info.status);
/// println!("Source subscriptions: {:?}", query_info.source_subscriptions);
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
/// use drasi_lib::{DrasiLib, ComponentStatus};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder().with_id("my-server").build().await?;
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

impl From<QueryConfig> for QueryRuntime {
    fn from(config: QueryConfig) -> Self {
        Self {
            id: config.id,
            query: config.query,
            status: ComponentStatus::Stopped,
            error_message: None,
            source_subscriptions: config.sources,
            joins: config.joins,
        }
    }
}

/// Runtime configuration with applied defaults
///
/// `RuntimeConfig` represents a fully-resolved configuration with all global defaults
/// applied to individual components. It's created from [`DrasiLibConfig`](super::schema::DrasiLibConfig)
/// and used internally by [`DrasiLib`](crate::DrasiLib) for execution.
///
/// # Plugin Architecture
///
/// **Important**: drasi-lib has ZERO awareness of which plugins exist. Sources and
/// reactions are passed as owned instances via `add_source()` and `add_reaction()`.
/// Only queries are stored in RuntimeConfig.
///
/// # Default Application
///
/// When converting from `DrasiLibConfig` to `RuntimeConfig`, global capacity
/// settings are applied to queries that don't specify their own values:
///
/// - **dispatch_buffer_capacity**: Applied to queries (default: 1000)
///
/// # Examples
///
/// ```yaml
/// id: my-server
/// dispatch_buffer_capacity: 5000  # Global default
///
/// queries:
///   - id: q1
///     query: "MATCH (n) RETURN n"
///     source_subscriptions:
///       - source_id: s1
///     # dispatch_buffer_capacity will be 5000 (inherited)
/// ```
#[derive(Clone)]
pub struct RuntimeConfig {
    /// Unique identifier for this DrasiLib instance
    pub id: String,
    /// Index factory for creating storage backend indexes for queries
    pub index_factory: Arc<IndexFactory>,
    /// State store provider for plugin state persistence
    pub state_store_provider: Arc<dyn StateStoreProvider>,
    /// Query configurations (sources/reactions are now instance-only)
    pub queries: Vec<QueryConfig>,
}

impl std::fmt::Debug for RuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeConfig")
            .field("id", &self.id)
            .field("index_factory", &self.index_factory)
            .field("state_store_provider", &"<dyn StateStoreProvider>")
            .field("queries", &self.queries)
            .finish()
    }
}

impl RuntimeConfig {
    /// Create a new RuntimeConfig with optional index backend and state store providers.
    ///
    /// When an index provider is supplied, RocksDB and Redis/Garnet storage backends
    /// will delegate to the provider for index creation. Without a provider, only
    /// in-memory storage backends can be used.
    ///
    /// When a state store provider is supplied, it will be used for plugin state
    /// persistence. Without a provider, the default in-memory state store is used.
    ///
    /// # Arguments
    ///
    /// * `config` - The DrasiLib configuration
    /// * `index_provider` - Optional index backend plugin for persistent storage
    /// * `state_store_provider` - Optional state store provider for plugin state
    pub fn new(
        config: super::schema::DrasiLibConfig,
        index_provider: Option<Arc<dyn IndexBackendPlugin>>,
        state_store_provider: Option<Arc<dyn StateStoreProvider>>,
    ) -> Self {
        // Get the global defaults (or hardcoded fallbacks)
        let global_dispatch_capacity = config.dispatch_buffer_capacity.unwrap_or(1000);

        // Create IndexFactory from storage backend configurations with optional plugin
        let index_factory = Arc::new(IndexFactory::new(config.storage_backends, index_provider));

        // Use provided state store or default to in-memory
        let state_store_provider: Arc<dyn StateStoreProvider> =
            state_store_provider.unwrap_or_else(|| Arc::new(MemoryStateStoreProvider::new()));

        // Apply global defaults to queries
        let queries = config
            .queries
            .into_iter()
            .map(|mut q| {
                if q.dispatch_buffer_capacity.is_none() {
                    q.dispatch_buffer_capacity = Some(global_dispatch_capacity);
                }
                q
            })
            .collect();

        Self {
            id: config.id,
            index_factory,
            state_store_provider,
            queries,
        }
    }
}

impl From<super::schema::DrasiLibConfig> for RuntimeConfig {
    fn from(config: super::schema::DrasiLibConfig) -> Self {
        // Default to no index provider and no state store provider
        Self::new(config, None, None)
    }
}
