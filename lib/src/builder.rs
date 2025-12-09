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
//! and their components in a type-safe, ergonomic way.
//!
//! # Overview
//!
//! - [`DrasiLibBuilder`] - Main builder for creating a DrasiLib instance
//! - [`Query`] - Builder for query configurations
//!
//! # Plugin Architecture
//!
//! **Important**: drasi-lib has ZERO awareness of which plugins exist. Sources and
//! reactions are created externally as fully-configured instances implementing
//! `Source` and `Reaction` traits, then passed to DrasiLibBuilder via
//! `with_source()` and `with_reaction()`.
//!
//! # Examples
//!
//! ## Basic Usage with Pre-built Instances
//!
//! ```no_run
//! use drasi_lib::{DrasiLib, Query};
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! // Source and reaction instances are created externally by plugins
//! // Ownership is transferred to DrasiLib when added
//! // let my_source = my_source_plugin::create(...);
//! // let my_reaction = my_reaction_plugin::create(...);
//!
//! let core = DrasiLib::builder()
//!     .with_id("my-server")
//!     // .with_source(my_source)      // Ownership transferred
//!     // .with_reaction(my_reaction)  // Ownership transferred
//!     .with_query(
//!         Query::cypher("my-query")
//!             .query("MATCH (n:Person) RETURN n")
//!             .from_source("events")
//!             .build()
//!     )
//!     .build()
//!     .await?;
//!
//! core.start().await?;
//! # Ok(())
//! # }
//! ```

use std::sync::Arc;

use crate::config::{
    DrasiLibConfig, QueryConfig, QueryJoinConfig, QueryLanguage, SourceSubscriptionConfig,
};
use drasi_core::models::SourceMiddlewareConfig;
use crate::channels::DispatchMode;
use crate::error::{DrasiError, Result};
use crate::indexes::StorageBackendConfig;
use crate::plugin_core::Reaction as ReactionTrait;
use crate::plugin_core::Source as SourceTrait;
use crate::lib_core::DrasiLib;

// ============================================================================
// DrasiLibBuilder
// ============================================================================

/// Fluent builder for creating DrasiLib instances.
///
/// Use `DrasiLib::builder()` to get started.
///
/// # Plugin Architecture
///
/// **Important**: drasi-lib has ZERO awareness of which plugins exist. Sources and
/// reactions are created externally as fully-configured instances implementing
/// `Source` and `Reaction` traits, then passed via `with_source()` and `with_reaction()`.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::{DrasiLib, Query};
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // Source and reaction instances are created externally by plugins
/// // Ownership is transferred to DrasiLib when added
/// // let my_source = my_source_plugin::create(...);
/// // let my_reaction = my_reaction_plugin::create(...);
///
/// let core = DrasiLib::builder()
///     .with_id("my-server")
///     // .with_source(my_source)      // Ownership transferred
///     // .with_reaction(my_reaction)  // Ownership transferred
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
pub struct DrasiLibBuilder {
    server_id: Option<String>,
    priority_queue_capacity: Option<usize>,
    dispatch_buffer_capacity: Option<usize>,
    storage_backends: Vec<StorageBackendConfig>,
    query_configs: Vec<QueryConfig>,
    source_instances: Vec<Box<dyn SourceTrait>>,
    reaction_instances: Vec<Box<dyn ReactionTrait>>,
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
            query_configs: Vec::new(),
            source_instances: Vec::new(),
            reaction_instances: Vec::new(),
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

    /// Add a source instance, taking ownership.
    ///
    /// Source instances are created externally by plugins with their own typed configurations.
    /// drasi-lib only knows about the `Source` trait - it has no knowledge of which plugins exist.
    ///
    /// # Example
    /// ```ignore
    /// let source = MySource::new("my-source", config)?;
    /// let core = DrasiLib::builder()
    ///     .with_source(source)  // Ownership transferred
    ///     .build()
    ///     .await?;
    /// ```
    pub fn with_source(mut self, source: impl SourceTrait + 'static) -> Self {
        self.source_instances.push(Box::new(source));
        self
    }

    /// Add a query configuration.
    pub fn with_query(mut self, config: QueryConfig) -> Self {
        self.query_configs.push(config);
        self
    }

    /// Add a reaction instance, taking ownership.
    ///
    /// Reaction instances are created externally by plugins with their own typed configurations.
    /// drasi-lib only knows about the `Reaction` trait - it has no knowledge of which plugins exist.
    ///
    /// # Example
    /// ```ignore
    /// let reaction = MyReaction::new("my-reaction", vec!["query1".into()]);
    /// let core = DrasiLib::builder()
    ///     .with_reaction(reaction)  // Ownership transferred
    ///     .build()
    ///     .await?;
    /// ```
    pub fn with_reaction(mut self, reaction: impl ReactionTrait + 'static) -> Self {
        self.reaction_instances.push(Box::new(reaction));
        self
    }

    /// Build the DrasiLib instance.
    ///
    /// This validates the configuration, creates all components, and initializes the server.
    /// After building, you can call `start()` to begin processing.
    pub async fn build(self) -> Result<DrasiLib> {
        // Build the configuration
        let config = DrasiLibConfig {
            id: self.server_id.unwrap_or_else(|| "drasi-lib".to_string()),
            priority_queue_capacity: self.priority_queue_capacity,
            dispatch_buffer_capacity: self.dispatch_buffer_capacity,
            storage_backends: self.storage_backends,
            queries: self.query_configs.clone(),
        };

        // Validate the configuration
        config
            .validate()
            .map_err(|e| DrasiError::startup_validation(e.to_string()))?;

        // Create runtime config and server
        let runtime_config = Arc::new(crate::config::RuntimeConfig::from(config));
        let mut core = DrasiLib::new(runtime_config);

        // Initialize the server
        core.initialize().await?;

        // Inject pre-built source instances
        for source in self.source_instances {
            let source_id = source.id().to_string();
            core.source_manager
                .add_source(source)
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
            let reaction_id = reaction.id().to_string();
            core.reaction_manager
                .add_reaction(reaction)
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
    sources: Vec<SourceSubscriptionConfig>,
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
            sources: Vec::new(),
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
            sources: Vec::new(),
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
        self.sources.push(SourceSubscriptionConfig {
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
        self.sources.push(SourceSubscriptionConfig {
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
            sources: self.sources,
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
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DrasiLib;

    // ==========================================================================
    // Query Builder Tests
    // ==========================================================================

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
        assert_eq!(config.sources.len(), 1);
        assert_eq!(config.sources[0].source_id, "source1");
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

        assert_eq!(config.sources.len(), 2);
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

        assert_eq!(core.get_config().id, "test-server");
    }

    #[tokio::test]
    async fn test_drasi_lib_builder_with_query_no_source() {
        // Test builder with query configuration that has no source subscriptions
        // In the instance-based approach, sources are added after build()
        let core = DrasiLibBuilder::new()
            .with_id("test-server")
            .with_query(
                Query::cypher("query1")
                    .query("MATCH (n) RETURN n")
                    // No from_source() call - query has no source subscriptions
                    .auto_start(false)
                    .build(),
            )
            .build()
            .await
            .unwrap();

        let queries = core.list_queries().await.unwrap();
        assert_eq!(queries.len(), 1);
    }

    // ==========================================================================
    // DrasiLib Builder Integration Tests (from builder_tests.rs)
    // ==========================================================================

    #[tokio::test]
    async fn test_builder_creates_initialized_server() {
        let core = DrasiLib::builder()
            .with_id("builder-test")
            .build()
            .await;

        assert!(core.is_ok(), "Builder should create initialized server");
        let core = core.unwrap();
        assert!(
            core.state_guard.is_initialized().await,
            "Server should be initialized"
        );
    }

    #[tokio::test]
    async fn test_builder_with_query() {
        // In the instance-based approach, sources and reactions are added as instances
        // after the builder creates the core. Here we just test query config addition.
        let core = DrasiLib::builder()
            .with_id("complex-server")
            .with_query(
                Query::cypher("query1")
                    .query("MATCH (n) RETURN n")
                    .from_source("source1")
                    .build(),
            )
            .build()
            .await;

        assert!(core.is_ok(), "Builder with query should succeed");
        let core = core.unwrap();
        assert!(core.state_guard.is_initialized().await);
        assert_eq!(core.config.queries.len(), 1);
    }
}
