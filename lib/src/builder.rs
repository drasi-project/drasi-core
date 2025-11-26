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

//! Fluent builders for DrasiLib and its components.
//!
//! This module provides the builder pattern for constructing DrasiLib instances
//! and their components (sources, queries, reactions) in a type-safe, ergonomic way.
//!
//! # Overview
//!
//! - [`DrasiLibBuilder`] - Main builder for creating a DrasiLib instance
//! - [`Source`] - Builder for source configurations
//! - [`Query`] - Builder for query configurations
//! - [`Reaction`] - Builder for reaction configurations
//!
//! # Examples
//!
//! ## Basic Usage
//!
//! ```no_run
//! use drasi_lib::{DrasiLib, Source, Query, Reaction};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let core = DrasiLib::builder()
//!     .with_id("my-server")
//!     .add_source(
//!         Source::application("events")
//!             .auto_start(true)
//!             .build()
//!     )
//!     .add_query(
//!         Query::cypher("my-query")
//!             .query("MATCH (n:Person) RETURN n")
//!             .from_source("events")
//!             .build()
//!     )
//!     .add_reaction(
//!         Reaction::log("output")
//!             .subscribe_to("my-query")
//!             .build()
//!     )
//!     .build()
//!     .await?;
//!
//! core.start().await?;
//! # Ok(())
//! # }
//! ```
//!
//! ## Instance Injection (Advanced)
//!
//! For advanced use cases, you can inject pre-built source and reaction instances:
//!
//! ```no_run
//! use drasi_lib::{DrasiLib, Query};
//! use std::sync::Arc;
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Assume my_custom_source implements Source trait
//! // let my_custom_source: Arc<dyn SourceTrait> = ...;
//!
//! let core = DrasiLib::builder()
//!     .with_id("advanced-server")
//!     // .with_source(my_custom_source)  // Inject custom source instance
//!     .add_query(
//!         Query::cypher("query1")
//!             .query("MATCH (n) RETURN n")
//!             .from_source("custom-source")
//!             .build()
//!     )
//!     .build()
//!     .await?;
//! # Ok(())
//! # }
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use crate::bootstrap::BootstrapProviderConfig;
use crate::config::{
    DrasiLibConfig, DrasiLibSettings, QueryConfig, QueryJoinConfig, QueryLanguage,
    ReactionConfig, ReactionSpecificConfig, SourceConfig, SourceSpecificConfig,
    SourceSubscriptionConfig,
};
use drasi_core::models::SourceMiddlewareConfig;
use crate::channels::DispatchMode;
use crate::error::{DrasiError, Result};
use crate::indexes::StorageBackendConfig;
use crate::plugin_core::{ReactionRegistry, SourceRegistry};
use crate::reactions::Reaction as ReactionTrait;
use crate::server_core::DrasiLib;
use crate::sources::Source as SourceTrait;

// ============================================================================
// DrasiLibBuilder
// ============================================================================

/// Fluent builder for creating DrasiLib instances.
///
/// Use `DrasiLib::builder()` to get started.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::{DrasiLib, Source, Query, Reaction};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder()
///     .with_id("my-server")
///     .add_source(Source::application("my-source").build())
///     .add_query(
///         Query::cypher("my-query")
///             .query("MATCH (n) RETURN n")
///             .from_source("my-source")
///             .build()
///     )
///     .add_reaction(
///         Reaction::log("my-reaction")
///             .subscribe_to("my-query")
///             .build()
///     )
///     .build()
///     .await?;
/// # Ok(())
/// # }
/// ```
pub struct DrasiLibBuilder {
    server_id: Option<String>,
    priority_queue_capacity: Option<usize>,
    dispatch_buffer_capacity: Option<usize>,
    storage_backends: Vec<StorageBackendConfig>,
    source_configs: Vec<SourceConfig>,
    query_configs: Vec<QueryConfig>,
    reaction_configs: Vec<ReactionConfig>,
    source_instances: Vec<Arc<dyn SourceTrait>>,
    reaction_instances: Vec<Arc<dyn ReactionTrait>>,
    source_registry: Option<SourceRegistry>,
    reaction_registry: Option<ReactionRegistry>,
}

impl Default for DrasiLibBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl DrasiLibBuilder {
    /// Create a new builder with default values.
    pub fn new() -> Self {
        Self {
            server_id: None,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            storage_backends: Vec::new(),
            source_configs: Vec::new(),
            query_configs: Vec::new(),
            reaction_configs: Vec::new(),
            source_instances: Vec::new(),
            reaction_instances: Vec::new(),
            source_registry: None,
            reaction_registry: None,
        }
    }

    /// Set the server ID.
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.server_id = Some(id.into());
        self
    }

    /// Set the default priority queue capacity for components.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set the default dispatch buffer capacity for components.
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Add a storage backend configuration.
    pub fn add_storage_backend(mut self, config: StorageBackendConfig) -> Self {
        self.storage_backends.push(config);
        self
    }

    /// Add a source configuration (will be created via registry).
    pub fn add_source(mut self, config: SourceConfig) -> Self {
        self.source_configs.push(config);
        self
    }

    /// Add a pre-built source instance directly.
    ///
    /// This bypasses the registry and allows you to inject custom source implementations.
    pub fn with_source(mut self, source: Arc<dyn SourceTrait>) -> Self {
        self.source_instances.push(source);
        self
    }

    /// Add a query configuration.
    pub fn add_query(mut self, config: QueryConfig) -> Self {
        self.query_configs.push(config);
        self
    }

    /// Add a reaction configuration (will be created via registry).
    pub fn add_reaction(mut self, config: ReactionConfig) -> Self {
        self.reaction_configs.push(config);
        self
    }

    /// Add a pre-built reaction instance directly.
    ///
    /// This bypasses the registry and allows you to inject custom reaction implementations.
    pub fn with_reaction(mut self, reaction: Arc<dyn ReactionTrait>) -> Self {
        self.reaction_instances.push(reaction);
        self
    }

    /// Set a custom source registry for plugin-based source creation.
    pub fn with_source_registry(mut self, registry: SourceRegistry) -> Self {
        self.source_registry = Some(registry);
        self
    }

    /// Set a custom reaction registry for plugin-based reaction creation.
    pub fn with_reaction_registry(mut self, registry: ReactionRegistry) -> Self {
        self.reaction_registry = Some(registry);
        self
    }

    /// Build the DrasiLib instance.
    ///
    /// This validates the configuration, creates all components, and initializes the server.
    /// After building, you can call `start()` to begin processing.
    pub async fn build(self) -> Result<DrasiLib> {
        // Build the configuration
        let config = DrasiLibConfig {
            server_core: DrasiLibSettings {
                id: self.server_id.unwrap_or_else(|| "drasi-lib".to_string()),
                priority_queue_capacity: self.priority_queue_capacity,
                dispatch_buffer_capacity: self.dispatch_buffer_capacity,
            },
            storage_backends: self.storage_backends,
            sources: self.source_configs.clone(),
            queries: self.query_configs.clone(),
            reactions: self.reaction_configs.clone(),
        };

        // Validate the configuration
        config
            .validate()
            .map_err(|e| DrasiError::startup_validation(e.to_string()))?;

        // Create runtime config and server with custom registries if provided
        let runtime_config = Arc::new(crate::config::RuntimeConfig::from(config));
        let mut core = DrasiLib::new_with_registries(
            runtime_config,
            self.source_registry,
            self.reaction_registry,
        );

        // Initialize the server
        core.initialize().await?;

        // Inject pre-built source instances
        for source in self.source_instances {
            let source_id = source.get_config().id.clone();
            let auto_start = source.get_config().auto_start;
            core.source_manager
                .add_source_instance(source, auto_start)
                .await
                .map_err(|e| {
                    DrasiError::provisioning(format!(
                        "Failed to add source instance '{}': {}",
                        source_id, e
                    ))
                })?;
        }

        // Inject pre-built reaction instances
        for reaction in self.reaction_instances {
            let reaction_id = reaction.get_config().id.clone();
            core.reaction_manager
                .add_reaction_instance(reaction)
                .await
                .map_err(|e| {
                    DrasiError::provisioning(format!(
                        "Failed to add reaction instance '{}': {}",
                        reaction_id, e
                    ))
                })?;
        }

        Ok(core)
    }
}

// ============================================================================
// Source Builder
// ============================================================================

/// Fluent builder for source configurations.
///
/// Use `Source::application()`, `Source::mock()`, etc. to get started.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::Source;
///
/// let source_config = Source::application("my-source")
///     .auto_start(true)
///     .with_property("key", "value")
///     .build();
/// ```
pub struct Source {
    id: String,
    source_type: String,
    properties: HashMap<String, serde_json::Value>,
    auto_start: bool,
    bootstrap_provider: Option<BootstrapProviderConfig>,
    dispatch_buffer_capacity: Option<usize>,
    dispatch_mode: Option<DispatchMode>,
}

impl Source {
    /// Create a new source builder with the given ID and type.
    fn new(id: impl Into<String>, source_type: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            source_type: source_type.into(),
            properties: HashMap::new(),
            auto_start: true,
            bootstrap_provider: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
        }
    }

    /// Create an application source builder.
    ///
    /// Application sources allow programmatic data injection via handles.
    pub fn application(id: impl Into<String>) -> Self {
        Self::new(id, "application")
    }

    /// Create a mock source builder.
    ///
    /// Mock sources are useful for testing and development.
    pub fn mock(id: impl Into<String>) -> Self {
        Self::new(id, "mock")
    }

    /// Create a PostgreSQL source builder.
    pub fn postgres(id: impl Into<String>) -> Self {
        Self::new(id, "postgres")
    }

    /// Create an HTTP source builder.
    pub fn http(id: impl Into<String>) -> Self {
        Self::new(id, "http")
    }

    /// Create a gRPC source builder.
    pub fn grpc(id: impl Into<String>) -> Self {
        Self::new(id, "grpc")
    }

    /// Create a platform source builder.
    pub fn platform(id: impl Into<String>) -> Self {
        Self::new(id, "platform")
    }

    /// Set whether the source should auto-start.
    pub fn auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set a property value.
    pub fn with_property(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.properties.insert(key.into(), value.into());
        self
    }

    /// Set the bootstrap provider configuration.
    pub fn with_bootstrap_provider(mut self, provider: BootstrapProviderConfig) -> Self {
        self.bootstrap_provider = Some(provider);
        self
    }

    /// Set the dispatch buffer capacity.
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the dispatch mode.
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Build the source configuration.
    pub fn build(self) -> SourceConfig {
        let config = match self.source_type.as_str() {
            "application" => SourceSpecificConfig::Application(self.properties),
            "mock" => SourceSpecificConfig::Mock(self.properties),
            "postgres" => SourceSpecificConfig::Postgres(self.properties),
            "http" => SourceSpecificConfig::Http(self.properties),
            "grpc" => SourceSpecificConfig::Grpc(self.properties),
            "platform" => SourceSpecificConfig::Platform(self.properties),
            _ => SourceSpecificConfig::Application(self.properties),
        };

        SourceConfig {
            id: self.id,
            auto_start: self.auto_start,
            config,
            bootstrap_provider: self.bootstrap_provider,
            dispatch_buffer_capacity: self.dispatch_buffer_capacity,
            dispatch_mode: self.dispatch_mode,
        }
    }
}

// ============================================================================
// Query Builder
// ============================================================================

/// Fluent builder for query configurations.
///
/// Use `Query::cypher()` or `Query::gql()` to get started.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::Query;
///
/// let query_config = Query::cypher("my-query")
///     .query("MATCH (n:Person) RETURN n.name, n.age")
///     .from_source("my-source")
///     .auto_start(true)
///     .build();
/// ```
pub struct Query {
    id: String,
    query: String,
    query_language: QueryLanguage,
    source_subscriptions: Vec<SourceSubscriptionConfig>,
    middleware: Vec<SourceMiddlewareConfig>,
    auto_start: bool,
    joins: Option<Vec<QueryJoinConfig>>,
    enable_bootstrap: bool,
    bootstrap_buffer_size: usize,
    priority_queue_capacity: Option<usize>,
    dispatch_buffer_capacity: Option<usize>,
    dispatch_mode: Option<DispatchMode>,
    storage_backend: Option<crate::indexes::StorageBackendRef>,
}

impl Query {
    /// Create a new Cypher query builder.
    pub fn cypher(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            query: String::new(),
            query_language: QueryLanguage::Cypher,
            source_subscriptions: Vec::new(),
            middleware: Vec::new(),
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        }
    }

    /// Create a new GQL query builder.
    pub fn gql(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            query: String::new(),
            query_language: QueryLanguage::GQL,
            source_subscriptions: Vec::new(),
            middleware: Vec::new(),
            auto_start: true,
            joins: None,
            enable_bootstrap: true,
            bootstrap_buffer_size: 10000,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            dispatch_mode: None,
            storage_backend: None,
        }
    }

    /// Set the query string.
    pub fn query(mut self, query: impl Into<String>) -> Self {
        self.query = query.into();
        self
    }

    /// Subscribe to a source.
    pub fn from_source(mut self, source_id: impl Into<String>) -> Self {
        self.source_subscriptions.push(SourceSubscriptionConfig {
            source_id: source_id.into(),
            pipeline: Vec::new(),
        });
        self
    }

    /// Subscribe to a source with a middleware pipeline.
    ///
    /// The pipeline is a list of middleware names (strings) that will be applied to
    /// data from this source before it reaches the query.
    pub fn from_source_with_pipeline(
        mut self,
        source_id: impl Into<String>,
        pipeline: Vec<String>,
    ) -> Self {
        self.source_subscriptions.push(SourceSubscriptionConfig {
            source_id: source_id.into(),
            pipeline,
        });
        self
    }

    /// Add middleware to the query.
    pub fn with_middleware(mut self, middleware: SourceMiddlewareConfig) -> Self {
        self.middleware.push(middleware);
        self
    }

    /// Set whether the query should auto-start.
    pub fn auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the join configuration.
    pub fn with_joins(mut self, joins: Vec<QueryJoinConfig>) -> Self {
        self.joins = Some(joins);
        self
    }

    /// Enable or disable bootstrap.
    pub fn enable_bootstrap(mut self, enable: bool) -> Self {
        self.enable_bootstrap = enable;
        self
    }

    /// Set the bootstrap buffer size.
    pub fn with_bootstrap_buffer_size(mut self, size: usize) -> Self {
        self.bootstrap_buffer_size = size;
        self
    }

    /// Set the priority queue capacity.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set the dispatch buffer capacity.
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the dispatch mode.
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Set the storage backend reference.
    pub fn with_storage_backend(mut self, backend: crate::indexes::StorageBackendRef) -> Self {
        self.storage_backend = Some(backend);
        self
    }

    /// Build the query configuration.
    pub fn build(self) -> QueryConfig {
        QueryConfig {
            id: self.id,
            query: self.query,
            query_language: self.query_language,
            source_subscriptions: self.source_subscriptions,
            middleware: self.middleware,
            auto_start: self.auto_start,
            joins: self.joins,
            enable_bootstrap: self.enable_bootstrap,
            bootstrap_buffer_size: self.bootstrap_buffer_size,
            priority_queue_capacity: self.priority_queue_capacity,
            dispatch_buffer_capacity: self.dispatch_buffer_capacity,
            dispatch_mode: self.dispatch_mode,
            storage_backend: self.storage_backend,
        }
    }
}

// ============================================================================
// Reaction Builder
// ============================================================================

/// Fluent builder for reaction configurations.
///
/// Use `Reaction::log()`, `Reaction::application()`, etc. to get started.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::Reaction;
///
/// let reaction_config = Reaction::log("my-reaction")
///     .subscribe_to("my-query")
///     .auto_start(true)
///     .build();
/// ```
pub struct Reaction {
    id: String,
    reaction_type: String,
    queries: Vec<String>,
    properties: HashMap<String, serde_json::Value>,
    auto_start: bool,
    priority_queue_capacity: Option<usize>,
}

impl Reaction {
    /// Create a new reaction builder with the given ID and type.
    fn new(id: impl Into<String>, reaction_type: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            reaction_type: reaction_type.into(),
            queries: Vec::new(),
            properties: HashMap::new(),
            auto_start: true,
            priority_queue_capacity: None,
        }
    }

    /// Create an application reaction builder.
    ///
    /// Application reactions allow programmatic result consumption via handles.
    pub fn application(id: impl Into<String>) -> Self {
        Self::new(id, "application")
    }

    /// Create a log reaction builder.
    ///
    /// Log reactions output results to the console.
    pub fn log(id: impl Into<String>) -> Self {
        Self::new(id, "log")
    }

    /// Create an HTTP reaction builder.
    pub fn http(id: impl Into<String>) -> Self {
        Self::new(id, "http")
    }

    /// Create a gRPC reaction builder.
    pub fn grpc(id: impl Into<String>) -> Self {
        Self::new(id, "grpc")
    }

    /// Create an SSE (Server-Sent Events) reaction builder.
    pub fn sse(id: impl Into<String>) -> Self {
        Self::new(id, "sse")
    }

    /// Create a platform reaction builder.
    pub fn platform(id: impl Into<String>) -> Self {
        Self::new(id, "platform")
    }

    /// Subscribe to a query.
    pub fn subscribe_to(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Set whether the reaction should auto-start.
    pub fn auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set a property value.
    pub fn with_property(mut self, key: impl Into<String>, value: impl Into<serde_json::Value>) -> Self {
        self.properties.insert(key.into(), value.into());
        self
    }

    /// Set the priority queue capacity.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Build the reaction configuration.
    pub fn build(self) -> ReactionConfig {
        let config = match self.reaction_type.as_str() {
            "application" => ReactionSpecificConfig::Application(self.properties),
            "log" => ReactionSpecificConfig::Log(self.properties),
            "http" => ReactionSpecificConfig::Http(self.properties),
            "grpc" => ReactionSpecificConfig::Grpc(self.properties),
            "sse" => ReactionSpecificConfig::Sse(self.properties),
            "platform" => ReactionSpecificConfig::Platform(self.properties),
            _ => ReactionSpecificConfig::Application(self.properties),
        };

        ReactionConfig {
            id: self.id,
            queries: self.queries,
            auto_start: self.auto_start,
            config,
            priority_queue_capacity: self.priority_queue_capacity,
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_source_builder_application() {
        let config = Source::application("test-source")
            .auto_start(false)
            .with_property("key", "value")
            .build();

        assert_eq!(config.id, "test-source");
        assert!(!config.auto_start);
        assert_eq!(config.source_type(), "application");
    }

    #[test]
    fn test_source_builder_mock() {
        let config = Source::mock("mock-source").build();

        assert_eq!(config.id, "mock-source");
        assert!(config.auto_start);
        assert_eq!(config.source_type(), "mock");
    }

    #[test]
    fn test_query_builder_cypher() {
        let config = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .from_source("source1")
            .auto_start(false)
            .build();

        assert_eq!(config.id, "test-query");
        assert_eq!(config.query, "MATCH (n) RETURN n");
        assert_eq!(config.query_language, QueryLanguage::Cypher);
        assert!(!config.auto_start);
        assert_eq!(config.source_subscriptions.len(), 1);
        assert_eq!(config.source_subscriptions[0].source_id, "source1");
    }

    #[test]
    fn test_query_builder_gql() {
        let config = Query::gql("test-query")
            .query("MATCH (n:Person) RETURN n.name")
            .from_source("source1")
            .build();

        assert_eq!(config.query_language, QueryLanguage::GQL);
    }

    #[test]
    fn test_query_builder_multiple_sources() {
        let config = Query::cypher("test-query")
            .query("MATCH (n) RETURN n")
            .from_source("source1")
            .from_source("source2")
            .build();

        assert_eq!(config.source_subscriptions.len(), 2);
    }

    #[test]
    fn test_reaction_builder_log() {
        let config = Reaction::log("test-reaction")
            .subscribe_to("query1")
            .auto_start(false)
            .build();

        assert_eq!(config.id, "test-reaction");
        assert_eq!(config.reaction_type(), "log");
        assert!(!config.auto_start);
        assert_eq!(config.queries.len(), 1);
        assert_eq!(config.queries[0], "query1");
    }

    #[test]
    fn test_reaction_builder_multiple_queries() {
        let config = Reaction::application("test-reaction")
            .subscribe_to("query1")
            .subscribe_to("query2")
            .build();

        assert_eq!(config.queries.len(), 2);
    }

    #[tokio::test]
    async fn test_drasi_lib_builder_empty() {
        let core = DrasiLibBuilder::new().build().await.unwrap();

        assert!(!core.is_running().await);
    }

    #[tokio::test]
    async fn test_drasi_lib_builder_with_id() {
        let core = DrasiLibBuilder::new()
            .with_id("test-server")
            .build()
            .await
            .unwrap();

        assert_eq!(core.get_config().server_core.id, "test-server");
    }

    #[tokio::test]
    async fn test_drasi_lib_builder_with_components() {
        // Use the test mock registries that have "mock" and "log" types registered
        use crate::test_support::helpers::test_mocks::{create_test_reaction_registry, create_test_source_registry};

        let core = DrasiLibBuilder::new()
            .with_id("test-server")
            .with_source_registry(create_test_source_registry())
            .with_reaction_registry(create_test_reaction_registry())
            .add_source(Source::mock("source1").build())
            .add_query(
                Query::cypher("query1")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .add_reaction(Reaction::log("reaction1").subscribe_to("query1").build())
            .build()
            .await
            .unwrap();

        let sources = core.list_sources().await.unwrap();
        let queries = core.list_queries().await.unwrap();
        let reactions = core.list_reactions().await.unwrap();

        assert_eq!(sources.len(), 1);
        assert_eq!(queries.len(), 1);
        assert_eq!(reactions.len(), 1);
    }
}
