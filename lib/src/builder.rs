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

use std::collections::HashMap;
use std::sync::Arc;

use crate::channels::DispatchMode;
use crate::config::{
    DrasiLibConfig, QueryConfig, QueryJoinConfig, QueryLanguage, SourceSubscriptionConfig,
};
use crate::error::{DrasiError, Result};
use crate::identity::IdentityProvider;
use crate::indexes::IndexBackendPlugin;
use crate::indexes::StorageBackendConfig;
use crate::lib_core::DrasiLib;
use crate::reactions::Reaction as ReactionTrait;
use crate::secret_store::SecretStoreProvider;
use crate::sources::Source as SourceTrait;
use crate::state_store::StateStoreProvider;
use crate::wal::WalProvider;
use drasi_core::models::SourceMiddlewareConfig;

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
    source_instances: Vec<(
        Box<dyn SourceTrait>,
        std::collections::HashMap<String, String>,
    )>,
    reaction_instances: Vec<(
        Box<dyn ReactionTrait>,
        std::collections::HashMap<String, String>,
    )>,
    /// Bootstrap provider metadata to register in the component graph.
    /// Each entry: (source_id, kind, properties).
    bootstrap_metadata: Vec<(
        String,
        String,
        std::collections::HashMap<String, serde_json::Value>,
    )>,
    index_providers: HashMap<String, Arc<dyn IndexBackendPlugin>>,
    state_store_provider: Option<Arc<dyn StateStoreProvider>>,
    identity_provider: Option<Arc<dyn IdentityProvider>>,
    wal_provider: Option<Arc<dyn WalProvider>>,
    secret_store_provider: Option<Arc<dyn SecretStoreProvider>>,
    default_recovery_policy: Option<crate::recovery::RecoveryPolicy>,
    default_index_backend: Option<crate::indexes::StorageBackendRef>,
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
            bootstrap_metadata: Vec::new(),
            index_providers: HashMap::new(),
            state_store_provider: None,
            identity_provider: None,
            wal_provider: None,
            secret_store_provider: None,
            default_recovery_policy: None,
            default_index_backend: None,
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

    /// Register a named index backend provider for persistent storage.
    ///
    /// Persistent storage backends (e.g. RocksDB or Redis/Garnet) are supplied as
    /// pre-built providers implementing [`IndexBackendPlugin`] and registered under a
    /// name. A query selects a provider by referencing that same name via its
    /// `storage_backend` (a [`crate::indexes::StorageBackendRef::Named`]).
    ///
    /// The `name` must match the name used by queries (and, if you also declare the
    /// backend in `storage_backends`, the backend id). Multiple providers may be
    /// registered under different names. If no provider is registered, only in-memory
    /// storage backends can be used.
    ///
    /// # Example
    /// ```ignore
    /// use drasi_index_rocksdb::RocksDbIndexProvider;
    /// use std::sync::Arc;
    ///
    /// let provider = RocksDbIndexProvider::new("/data/drasi", true, false);
    /// let core = DrasiLib::builder()
    ///     .with_index_provider("rocks", Arc::new(provider))
    ///     .build()
    ///     .await?;
    /// ```
    pub fn with_index_provider(
        mut self,
        name: impl Into<String>,
        provider: Arc<dyn IndexBackendPlugin>,
    ) -> Self {
        self.index_providers.insert(name.into(), provider);
        self
    }

    /// Register an index provider and make it the default backend for queries
    /// that do not specify a `storage_backend`.
    ///
    /// This is equivalent to [`with_index_provider`](Self::with_index_provider)
    /// followed by marking that named backend as the default: every query whose
    /// `storage_backend` is `None` will use this provider instead of native
    /// in-memory indexes. Queries may still opt out per-query by setting an
    /// explicit `storage_backend` (e.g. an inline `Memory` backend).
    ///
    /// Calling this more than once replaces the default with the most recent name.
    ///
    /// # Example
    /// ```ignore
    /// use drasi_index_rocksdb::RocksDbIndexProvider;
    /// use std::sync::Arc;
    ///
    /// let provider = RocksDbIndexProvider::new("/data/drasi", true, false);
    /// let core = DrasiLib::builder()
    ///     .with_default_index_provider("rocks", Arc::new(provider))
    ///     .build()
    ///     .await?;
    /// // All queries without an explicit storage_backend now persist to RocksDB.
    /// ```
    pub fn with_default_index_provider(
        mut self,
        name: impl Into<String>,
        provider: Arc<dyn IndexBackendPlugin>,
    ) -> Self {
        let name = name.into();
        self.index_providers.insert(name.clone(), provider);
        self.default_index_backend = Some(crate::indexes::StorageBackendRef::Named(name));
        self
    }

    /// Set the state store provider for plugin state persistence.
    ///
    /// State store providers allow plugins (Sources, BootstrapProviders, and Reactions)
    /// to store and retrieve runtime state that can persist across runs of DrasiLib.
    ///
    /// If no state store provider is set, the default in-memory provider will be used.
    /// The in-memory provider does not persist state across restarts.
    ///
    /// # Example
    /// ```ignore
    /// use drasi_state_store_json::JsonStateStoreProvider;
    /// use std::sync::Arc;
    ///
    /// let state_store = JsonStateStoreProvider::new("/data/state");
    /// let core = DrasiLib::builder()
    ///     .with_state_store_provider(Arc::new(state_store))
    ///     .build()
    ///     .await?;
    /// ```
    pub fn with_state_store_provider(mut self, provider: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store_provider = Some(provider);
        self
    }

    /// Set the identity provider for credential injection.
    ///
    /// Identity providers supply authentication credentials (passwords, tokens,
    /// certificates) to sources and reactions that need them for connecting to
    /// external systems.
    ///
    /// If no identity provider is set, sources and reactions will receive `None`
    /// for `context.identity_provider`.
    ///
    /// # Example
    /// ```ignore
    /// use drasi_identity_azure::AzureIdentityProvider;
    /// use std::sync::Arc;
    ///
    /// let provider = AzureIdentityProvider::with_default_credentials("user@tenant.onmicrosoft.com")?;
    /// let core = DrasiLib::builder()
    ///     .with_identity_provider(Arc::new(provider))
    ///     .build()
    ///     .await?;
    /// ```
    pub fn with_identity_provider(mut self, provider: Arc<dyn IdentityProvider>) -> Self {
        self.identity_provider = Some(provider);
        self
    }

    /// Set the Write-Ahead Log provider for transient source durability.
    ///
    /// WAL providers enable transient sources (HTTP, gRPC, Application) to persist
    /// incoming events before acknowledging the caller. This enables crash recovery
    /// and replay for persistent queries subscribing to those sources.
    ///
    /// If no WAL provider is set, transient sources cannot enable durability and
    /// will operate in their default fire-and-forget mode.
    ///
    /// # Example
    /// ```ignore
    /// use drasi_wal_redb::RedbWalProvider;
    /// use std::sync::Arc;
    ///
    /// let wal = RedbWalProvider::new("/data/wal");
    /// let core = DrasiLib::builder()
    ///     .with_wal_provider(Arc::new(wal))
    ///     .build()
    ///     .await?;
    /// ```
    pub fn with_wal_provider(mut self, provider: Arc<dyn WalProvider>) -> Self {
        self.wal_provider = Some(provider);
        self
    }

    /// Set the secret store provider for resolving `ConfigValue::Secret` references.
    ///
    /// Secret store providers resolve named secret references (e.g., database passwords,
    /// API keys) from external secret management systems like Azure Key Vault, OS keyrings,
    /// or local secret files.
    ///
    /// The secret store provider is initialized **before** any source/reaction/bootstrap
    /// plugins, and its resolved values are injected into `DtoMapper` via the global
    /// secret resolver registry.
    ///
    /// If no secret store provider is set, `ConfigValue::Secret` references will fail
    /// with a "not implemented" error.
    ///
    /// # Example
    /// ```ignore
    /// use drasi_secret_store_file::FileSecretStoreProvider;
    /// use std::sync::Arc;
    ///
    /// let secrets = FileSecretStoreProvider::new("/etc/drasi/secrets.json").await?;
    /// let core = DrasiLib::builder()
    ///     .with_secret_store_provider(Arc::new(secrets))
    ///     .build()
    ///     .await?;
    /// ```
    pub fn with_secret_store_provider(mut self, provider: Arc<dyn SecretStoreProvider>) -> Self {
        self.secret_store_provider = Some(provider);
        self
    }

    /// Set the global default recovery policy for all queries.
    ///
    /// Per-query `QueryConfig::recovery_policy` overrides this.
    /// If neither is set, defaults to [`RecoveryPolicy::Strict`](crate::RecoveryPolicy::Strict).
    pub fn with_default_recovery_policy(mut self, policy: crate::recovery::RecoveryPolicy) -> Self {
        self.default_recovery_policy = Some(policy);
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
        self.source_instances
            .push((Box::new(source), std::collections::HashMap::new()));
        self
    }

    /// Add a source instance with additional component metadata.
    ///
    /// Like [`with_source`](Self::with_source) but merges `extra_metadata`
    /// (e.g. `pluginId`) into the component graph node.
    pub fn with_source_metadata(
        mut self,
        source: impl SourceTrait + 'static,
        extra_metadata: std::collections::HashMap<String, String>,
    ) -> Self {
        self.source_instances
            .push((Box::new(source), extra_metadata));
        self
    }

    /// Add a query configuration.
    pub fn with_query(mut self, config: QueryConfig) -> Self {
        self.query_configs.push(config);
        self
    }

    /// Register bootstrap provider metadata for a source.
    ///
    /// This records the bootstrap provider's kind and configuration properties
    /// so they can be persisted in the component graph and included in
    /// `snapshot_configuration()` output.
    ///
    /// # Arguments
    /// * `source_id` - The ID of the source this bootstrap provider is attached to
    /// * `kind` - Bootstrap provider kind (e.g., "http", "postgres", "scriptfile")
    /// * `properties` - Configuration properties for persistence (raw config with
    ///   ConfigValue envelopes intact for lossless roundtripping)
    ///
    /// # Example
    /// ```ignore
    /// let builder = DrasiLib::builder()
    ///     .with_source(my_source)
    ///     .with_bootstrap_for_source("my-source", "http", config_properties);
    /// ```
    pub fn with_bootstrap_for_source(
        mut self,
        source_id: impl Into<String>,
        kind: impl Into<String>,
        properties: std::collections::HashMap<String, serde_json::Value>,
    ) -> Self {
        self.bootstrap_metadata
            .push((source_id.into(), kind.into(), properties));
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
        self.reaction_instances
            .push((Box::new(reaction), std::collections::HashMap::new()));
        self
    }

    /// Add a reaction instance with additional component metadata.
    ///
    /// Like [`with_reaction`](Self::with_reaction) but merges `extra_metadata`
    /// (e.g. `pluginId`) into the component graph node.
    pub fn with_reaction_metadata(
        mut self,
        reaction: impl ReactionTrait + 'static,
        extra_metadata: std::collections::HashMap<String, String>,
    ) -> Self {
        self.reaction_instances
            .push((Box::new(reaction), extra_metadata));
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
            .map_err(|e| DrasiError::validation(e.to_string()))?;

        // Strict referential validation for query storage backends. Config-level
        // validation (DrasiLibConfig::validate) is intentionally lenient about named
        // references because injected providers are only known here. We perform the
        // authoritative check now, where both declared backends and injected provider
        // names are available, so any misconfiguration fails at build time rather than
        // being deferred to (possibly manual) query start.
        {
            use crate::indexes::config::{StorageBackendRef, StorageBackendSpec};

            // Declared backends, classified as in-memory or plugin (by kind).
            let mut declared_memory: std::collections::HashSet<&str> =
                std::collections::HashSet::new();
            let mut declared_plugin: std::collections::HashMap<&str, &str> =
                std::collections::HashMap::new();
            for b in &config.storage_backends {
                match &b.spec {
                    StorageBackendSpec::Memory { .. } => {
                        declared_memory.insert(b.id.as_str());
                    }
                    StorageBackendSpec::Plugin { kind, .. } => {
                        declared_plugin.insert(b.id.as_str(), kind.as_str());
                    }
                }
            }

            // An injected provider must not shadow a declared in-memory backend of the
            // same name (it would silently change a volatile backend into a persistent
            // one). Sharing a name with a declared plugin backend is expected.
            for name in self.index_providers.keys() {
                if declared_memory.contains(name.as_str()) {
                    return Err(DrasiError::validation(format!(
                        "Injected index provider '{name}' collides with a storage backend \
                         declared as 'memory'. Rename the provider or the storage backend."
                    )));
                }
            }

            for query in &config.queries {
                match &query.storage_backend {
                    Some(StorageBackendRef::Named(name)) => {
                        if self.index_providers.contains_key(name)
                            || declared_memory.contains(name.as_str())
                        {
                            // Resolvable: injected provider or in-memory backend.
                        } else if let Some(kind) = declared_plugin.get(name.as_str()) {
                            return Err(DrasiError::validation(format!(
                                "Query '{}' references storage backend '{}' (kind '{}') which is \
                                 declared but has no injected provider. Inject one via \
                                 with_index_provider(\"{}\", ...).",
                                query.id, name, kind, name
                            )));
                        } else {
                            return Err(DrasiError::validation(format!(
                                "Query '{}' references unknown storage backend '{}'. Declare it in \
                                 storage_backends or inject a provider via \
                                 with_index_provider(\"{}\", ...).",
                                query.id, name, name
                            )));
                        }
                    }
                    Some(StorageBackendRef::Inline(StorageBackendSpec::Plugin {
                        kind, ..
                    })) => {
                        return Err(DrasiError::validation(format!(
                            "Query '{}' uses an inline '{}' storage backend, which is not \
                             supported in embedded mode. Declare a named storage backend and \
                             inject a provider via with_index_provider(name, ...).",
                            query.id, kind
                        )));
                    }
                    // Inline Memory or no backend: nothing to validate here.
                    Some(StorageBackendRef::Inline(StorageBackendSpec::Memory { .. })) | None => {}
                }
            }

            // Validate the default backend reference (used by queries with no
            // explicit storage_backend) against injected providers and declared
            // in-memory backends, so a misconfigured default fails at build time.
            if let Some(StorageBackendRef::Named(name)) = &self.default_index_backend {
                if !self.index_providers.contains_key(name)
                    && !declared_memory.contains(name.as_str())
                {
                    return Err(DrasiError::validation(format!(
                        "Default index backend '{name}' is not registered. Provide it via \
                         with_default_index_provider(\"{name}\", ...) or declare it in \
                         storage_backends."
                    )));
                }
            }
        }

        // Create runtime config and server with optional index and state store providers
        let runtime_config = Arc::new(crate::config::RuntimeConfig::new(
            config,
            self.index_providers,
            self.state_store_provider,
            self.identity_provider,
            self.secret_store_provider,
            self.default_recovery_policy,
            self.default_index_backend,
        ));
        let mut core = DrasiLib::new(runtime_config);

        // Inject state store before provisioning sources (they need it for initialization)
        let state_store = core.config.state_store_provider.clone();
        core.source_manager
            .inject_state_store(state_store.clone())
            .await;
        core.reaction_manager.inject_state_store(state_store).await;

        // Inject WAL provider into SourceManager (if configured)
        // This allows transient sources to persist events for crash recovery
        if let Some(wal_provider) = self.wal_provider {
            core.source_manager.inject_wal_provider(wal_provider).await;
        }

        // Register the component graph source BEFORE initialize (which loads query config).
        // Queries reference sources, so sources must exist in the graph first.
        {
            use crate::sources::component_graph_source::ComponentGraphSource;
            let graph_source = ComponentGraphSource::new(
                core.component_event_broadcast_tx.clone(),
                core.config.id.clone(),
                core.component_graph.clone(),
            )
            .map_err(|e| {
                DrasiError::operation_failed(
                    "source",
                    "component-graph",
                    "add",
                    format!("Failed to create: {e}"),
                )
            })?;

            let source_id = graph_source.id().to_string();
            let source_type = graph_source.type_name().to_string();
            {
                let mut graph = core.component_graph.write().await;
                let mut metadata = std::collections::HashMap::new();
                metadata.insert("kind".to_string(), source_type);
                metadata.insert(
                    "autoStart".to_string(),
                    graph_source.auto_start().to_string(),
                );
                graph.register_source(&source_id, metadata).map_err(|e| {
                    DrasiError::operation_failed(
                        "source",
                        &source_id,
                        "add",
                        format!("Failed to register: {e}"),
                    )
                })?;
            }
            if let Err(e) = core.source_manager.provision_source(graph_source).await {
                let mut graph = core.component_graph.write().await;
                let _ = graph.deregister(&source_id);
                return Err(DrasiError::operation_failed(
                    "source",
                    &source_id,
                    "add",
                    format!("Failed to provision: {e}"),
                ));
            }
        }

        // Inject pre-built source instances BEFORE initialize.
        // Queries reference sources by ID, so sources must be in the graph first.
        for (source, extra_metadata) in self.source_instances {
            let source_id = source.id().to_string();
            let source_type = source.type_name().to_string();
            let auto_start = source.auto_start();

            {
                let mut graph = core.component_graph.write().await;
                let mut metadata = std::collections::HashMap::new();
                metadata.insert("kind".to_string(), source_type);
                metadata.insert("autoStart".to_string(), auto_start.to_string());
                metadata.extend(extra_metadata);
                graph.register_source(&source_id, metadata).map_err(|e| {
                    DrasiError::operation_failed(
                        "source",
                        &source_id,
                        "add",
                        format!("Failed to register: {e}"),
                    )
                })?;
            }
            if let Err(e) = core.source_manager.provision_source(source).await {
                let mut graph = core.component_graph.write().await;
                let _ = graph.deregister(&source_id);
                return Err(DrasiError::operation_failed(
                    "source",
                    &source_id,
                    "add",
                    format!("Failed to provision: {e}"),
                ));
            }
        }

        // Register bootstrap provider metadata in the component graph.
        // This enables snapshot_configuration() to persist and reconstruct
        // bootstrap provider configurations.
        for (source_id, kind, properties) in self.bootstrap_metadata {
            let bp_id = format!("{source_id}-bootstrap");
            let mut metadata = std::collections::HashMap::new();
            metadata.insert("kind".to_string(), kind);
            for (key, value) in properties {
                metadata.insert(key, serde_json::to_string(&value).unwrap_or_default());
            }
            let mut graph = core.component_graph.write().await;
            if let Err(e) = graph.register_bootstrap_provider(
                &bp_id,
                metadata,
                std::slice::from_ref(&source_id),
            ) {
                log::warn!(
                    "Failed to register bootstrap provider metadata for source '{source_id}': {e}"
                );
            }
        }

        // Initialize the server (loads query configurations — sources must already be registered)
        core.initialize().await?;

        // Inject pre-built reaction instances
        for (reaction, extra_metadata) in self.reaction_instances {
            let reaction_id = reaction.id().to_string();
            let reaction_type = reaction.type_name().to_string();
            let query_ids = reaction.query_ids();

            // Register in graph first, then provision
            {
                let mut graph = core.component_graph.write().await;
                let mut metadata = std::collections::HashMap::new();
                metadata.insert("kind".to_string(), reaction_type);
                metadata.extend(extra_metadata);
                graph
                    .register_reaction(&reaction_id, metadata, &query_ids)
                    .map_err(|e| {
                        DrasiError::operation_failed(
                            "reaction",
                            &reaction_id,
                            "add",
                            format!("Failed to register: {e}"),
                        )
                    })?;
            }
            if let Err(e) = core.reaction_manager.provision_reaction(reaction).await {
                let mut graph = core.component_graph.write().await;
                let _ = graph.deregister(&reaction_id);
                return Err(DrasiError::operation_failed(
                    "reaction",
                    &reaction_id,
                    "add",
                    format!("Failed to provision: {e}"),
                ));
            }
        }

        // Register the identity provider in the component graph (if configured).
        // This creates an IdentityProvider node with Authenticates edges to all
        // sources and reactions that receive credentials from it.
        if core.config.identity_provider.is_some() {
            let mut graph = core.component_graph.write().await;
            let component_ids: Vec<String> = graph
                .list_by_kind(&crate::component_graph::ComponentKind::Source)
                .into_iter()
                .chain(graph.list_by_kind(&crate::component_graph::ComponentKind::Reaction))
                .map(|(id, _)| id)
                .collect();

            let mut metadata = std::collections::HashMap::new();
            metadata.insert("kind".to_string(), "identity_provider".to_string());
            graph
                .register_identity_provider("identity-provider", metadata, &component_ids)
                .map_err(|e| {
                    DrasiError::operation_failed(
                        "identity_provider",
                        "identity-provider",
                        "add",
                        format!("Failed to register: {e}"),
                    )
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
    recovery_policy: Option<crate::recovery::RecoveryPolicy>,
    outbox_capacity: usize,
    bootstrap_timeout_secs: u64,
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
            recovery_policy: None,
            outbox_capacity: crate::queries::output_state::DEFAULT_OUTBOX_CAPACITY,
            bootstrap_timeout_secs: 300,
        }
    }

    /// Create a new GQL (ISO 9074:2024) query builder.
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
            recovery_policy: None,
            outbox_capacity: crate::queries::output_state::DEFAULT_OUTBOX_CAPACITY,
            bootstrap_timeout_secs: 300,
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
            nodes: Vec::new(),
            relations: Vec::new(),
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
            nodes: Vec::new(),
            relations: Vec::new(),
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

    /// Set the recovery policy. Applies only to queries with a persistent
    /// storage backend. See [`RecoveryPolicy`](crate::RecoveryPolicy).
    pub fn with_recovery_policy(mut self, policy: crate::recovery::RecoveryPolicy) -> Self {
        self.recovery_policy = Some(policy);
        self
    }

    /// Set the outbox capacity (number of recent QueryResult emissions retained).
    /// Default: 1000.
    pub fn with_outbox_capacity(mut self, capacity: usize) -> Self {
        self.outbox_capacity = capacity;
        self
    }

    /// Set the bootstrap timeout in seconds.
    /// This controls how long `fetch_snapshot` / `fetch_outbox` will wait for
    /// the query to finish bootstrapping before returning `FetchError::TimedOut`.
    /// Default: 300 (5 minutes).
    pub fn with_bootstrap_timeout_secs(mut self, secs: u64) -> Self {
        self.bootstrap_timeout_secs = secs;
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
            recovery_policy: self.recovery_policy,
            outbox_capacity: self.outbox_capacity,
            bootstrap_timeout_secs: self.bootstrap_timeout_secs,
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
        let core = DrasiLib::builder().with_id("builder-test").build().await;

        assert!(core.is_ok(), "Builder should create initialized server");
        let core = core.unwrap();
        assert!(
            core.state_guard.is_initialized(),
            "Server should be initialized"
        );
    }

    #[tokio::test]
    async fn test_builder_with_query() {
        // In the instance-based approach, sources and reactions are added as instances
        // after the builder creates the core. Here we just test query config addition.
        // Source must be registered before a query can reference it
        let source = crate::sources::tests::TestMockSource::new("source1".to_string()).unwrap();
        let core = DrasiLib::builder()
            .with_id("complex-server")
            .with_source(source)
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
        assert!(core.state_guard.is_initialized());
        assert_eq!(core.config.queries.len(), 1);
    }

    // ==========================================================================
    // DrasiLibBuilder Unit Tests
    // ==========================================================================

    #[test]
    fn test_builder_with_id_sets_id() {
        let builder = DrasiLibBuilder::new().with_id("my-server");
        assert_eq!(builder.server_id, Some("my-server".to_string()));
    }

    #[test]
    fn test_builder_with_id_accepts_string() {
        let builder = DrasiLibBuilder::new().with_id(String::from("owned-id"));
        assert_eq!(builder.server_id, Some("owned-id".to_string()));
    }

    #[test]
    fn test_builder_with_priority_queue_capacity() {
        let builder = DrasiLibBuilder::new().with_priority_queue_capacity(50000);
        assert_eq!(builder.priority_queue_capacity, Some(50000));
    }

    #[test]
    fn test_builder_with_dispatch_buffer_capacity() {
        let builder = DrasiLibBuilder::new().with_dispatch_buffer_capacity(2000);
        assert_eq!(builder.dispatch_buffer_capacity, Some(2000));
    }

    #[test]
    fn test_builder_with_query_adds_to_list() {
        let q = Query::cypher("q1").query("MATCH (n) RETURN n").build();
        let builder = DrasiLibBuilder::new().with_query(q);
        assert_eq!(builder.query_configs.len(), 1);
        assert_eq!(builder.query_configs[0].id, "q1");
    }

    #[test]
    fn test_builder_with_multiple_queries() {
        let q1 = Query::cypher("q1").query("MATCH (a) RETURN a").build();
        let q2 = Query::gql("q2").query("MATCH (b) RETURN b").build();
        let builder = DrasiLibBuilder::new().with_query(q1).with_query(q2);
        assert_eq!(builder.query_configs.len(), 2);
        assert_eq!(builder.query_configs[0].id, "q1");
        assert_eq!(builder.query_configs[1].id, "q2");
    }

    #[test]
    fn test_builder_add_storage_backend() {
        use crate::indexes::config::{StorageBackendConfig, StorageBackendSpec};

        let backend = StorageBackendConfig {
            id: "mem1".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        };
        let builder = DrasiLibBuilder::new().add_storage_backend(backend);
        assert_eq!(builder.storage_backends.len(), 1);
        assert_eq!(builder.storage_backends[0].id, "mem1");
    }

    #[test]
    fn test_builder_add_multiple_storage_backends() {
        use crate::indexes::config::{StorageBackendConfig, StorageBackendSpec};

        let b1 = StorageBackendConfig {
            id: "mem1".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        };
        let b2 = StorageBackendConfig {
            id: "mem2".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: true,
            },
        };
        let builder = DrasiLibBuilder::new()
            .add_storage_backend(b1)
            .add_storage_backend(b2);
        assert_eq!(builder.storage_backends.len(), 2);
        assert_eq!(builder.storage_backends[0].id, "mem1");
        assert_eq!(builder.storage_backends[1].id, "mem2");
    }

    #[test]
    fn test_builder_default_values() {
        let builder = DrasiLibBuilder::new();
        assert_eq!(builder.server_id, None);
        assert_eq!(builder.priority_queue_capacity, None);
        assert_eq!(builder.dispatch_buffer_capacity, None);
        assert!(builder.storage_backends.is_empty());
        assert!(builder.query_configs.is_empty());
        assert!(builder.source_instances.is_empty());
        assert!(builder.reaction_instances.is_empty());
        assert!(builder.index_providers.is_empty());
        assert!(builder.state_store_provider.is_none());
    }

    #[test]
    fn test_builder_fluent_chaining() {
        use crate::indexes::config::{StorageBackendConfig, StorageBackendSpec};

        let backend = StorageBackendConfig {
            id: "mem".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        };
        let q = Query::cypher("q1").query("MATCH (n) RETURN n").build();

        let builder = DrasiLibBuilder::new()
            .with_id("chained")
            .with_priority_queue_capacity(20000)
            .with_dispatch_buffer_capacity(3000)
            .add_storage_backend(backend)
            .with_query(q);

        assert_eq!(builder.server_id, Some("chained".to_string()));
        assert_eq!(builder.priority_queue_capacity, Some(20000));
        assert_eq!(builder.dispatch_buffer_capacity, Some(3000));
        assert_eq!(builder.storage_backends.len(), 1);
        assert_eq!(builder.query_configs.len(), 1);
    }

    #[tokio::test]
    async fn test_builder_default_id_when_none_set() {
        let core = DrasiLibBuilder::new().build().await.unwrap();
        assert_eq!(core.get_config().id, "drasi-lib");
    }

    #[tokio::test]
    async fn test_builder_with_storage_backend_builds_ok() {
        use crate::indexes::config::{StorageBackendConfig, StorageBackendSpec};

        let backend = StorageBackendConfig {
            id: "test-mem".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        };
        let core = DrasiLibBuilder::new()
            .add_storage_backend(backend)
            .build()
            .await;
        assert!(core.is_ok(), "Builder with storage backend should succeed");
    }

    #[tokio::test]
    async fn test_builder_query_named_unknown_backend_errors() {
        use crate::indexes::config::StorageBackendRef;

        let query = Query::cypher("q")
            .query("MATCH (n) RETURN n")
            .with_storage_backend(StorageBackendRef::Named("nope".to_string()))
            .build();
        let err = DrasiLibBuilder::new()
            .with_query(query)
            .build()
            .await
            .map(|_| ())
            .expect_err("unknown named backend should fail");
        assert!(err.to_string().contains("unknown storage backend"));
    }

    #[tokio::test]
    async fn test_builder_query_named_memory_backend_ok() {
        use crate::indexes::config::{StorageBackendConfig, StorageBackendRef, StorageBackendSpec};

        let backend = StorageBackendConfig {
            id: "mem".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        };
        let query = Query::cypher("q")
            .query("MATCH (n) RETURN n")
            .with_storage_backend(StorageBackendRef::Named("mem".to_string()))
            .build();
        let core = DrasiLibBuilder::new()
            .add_storage_backend(backend)
            .with_query(query)
            .build()
            .await;
        assert!(core.is_ok(), "named memory backend should resolve");
    }

    #[tokio::test]
    async fn test_builder_query_declared_plugin_without_provider_errors() {
        use crate::indexes::config::{StorageBackendConfig, StorageBackendRef, StorageBackendSpec};

        let backend = StorageBackendConfig {
            id: "rocks".to_string(),
            spec: StorageBackendSpec::Plugin {
                kind: "rocksdb".to_string(),
                config: serde_json::json!({ "path": "/data/drasi" }),
            },
        };
        let query = Query::cypher("q")
            .query("MATCH (n) RETURN n")
            .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
            .build();
        let err = DrasiLibBuilder::new()
            .add_storage_backend(backend)
            .with_query(query)
            .build()
            .await
            .map(|_| ())
            .expect_err("declared plugin without provider should fail");
        assert!(err.to_string().contains("no injected provider"));
    }

    #[tokio::test]
    async fn test_builder_inline_plugin_backend_errors() {
        use crate::indexes::config::{StorageBackendRef, StorageBackendSpec};

        let query = Query::cypher("q")
            .query("MATCH (n) RETURN n")
            .with_storage_backend(StorageBackendRef::Inline(StorageBackendSpec::Plugin {
                kind: "rocksdb".to_string(),
                config: serde_json::json!({ "path": "/data/drasi" }),
            }))
            .build();
        let err = DrasiLibBuilder::new()
            .with_query(query)
            .build()
            .await
            .map(|_| ())
            .expect_err("inline plugin backend should fail in embedded mode");
        assert!(err.to_string().contains("not supported in embedded mode"));
    }

    #[tokio::test]
    async fn test_builder_provider_shadows_memory_backend_errors() {
        use crate::indexes::config::{StorageBackendConfig, StorageBackendSpec};
        use async_trait::async_trait;
        use drasi_core::in_memory_index::in_memory_element_index::InMemoryElementIndex;
        use drasi_core::in_memory_index::in_memory_future_queue::InMemoryFutureQueue;
        use drasi_core::in_memory_index::in_memory_result_index::InMemoryResultIndex;
        use drasi_core::interface::{CreatedIndexes, IndexSet, NoOpSessionControl};

        // A persistent (non-volatile) provider injected under a name that is also
        // declared as an in-memory backend must be rejected — silently turning a
        // volatile backend into a persistent one is a misconfiguration.
        struct MockPersistentPlugin;

        #[async_trait]
        impl IndexBackendPlugin for MockPersistentPlugin {
            async fn create_indexes(
                &self,
                _query_id: &str,
            ) -> std::result::Result<CreatedIndexes, drasi_core::interface::IndexError>
            {
                let element_index = Arc::new(InMemoryElementIndex::new());
                Ok(CreatedIndexes {
                    set: IndexSet {
                        element_index: element_index.clone(),
                        archive_index: element_index,
                        result_index: Arc::new(InMemoryResultIndex::new()),
                        future_queue: Arc::new(InMemoryFutureQueue::new()),
                        session_control: Arc::new(NoOpSessionControl),
                    },
                    checkpoint_store: None,
                    outbox_writer: None,
                    live_results_writer: None,
                })
            }

            fn is_volatile(&self) -> bool {
                false
            }
        }

        let backend = StorageBackendConfig {
            id: "mem".to_string(),
            spec: StorageBackendSpec::Memory {
                enable_archive: false,
            },
        };
        let err = DrasiLibBuilder::new()
            .add_storage_backend(backend)
            .with_index_provider("mem", Arc::new(MockPersistentPlugin))
            .build()
            .await
            .map(|_| ())
            .expect_err("provider shadowing a declared memory backend should fail");
        assert!(
            err.to_string().contains("collides"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn test_with_default_index_provider_sets_default_backend() {
        use async_trait::async_trait;
        use drasi_core::in_memory_index::in_memory_element_index::InMemoryElementIndex;
        use drasi_core::in_memory_index::in_memory_future_queue::InMemoryFutureQueue;
        use drasi_core::in_memory_index::in_memory_result_index::InMemoryResultIndex;
        use drasi_core::interface::{CreatedIndexes, IndexSet, NoOpSessionControl};

        struct MockPersistentPlugin;

        #[async_trait]
        impl IndexBackendPlugin for MockPersistentPlugin {
            async fn create_indexes(
                &self,
                _query_id: &str,
            ) -> std::result::Result<CreatedIndexes, drasi_core::interface::IndexError>
            {
                let element_index = Arc::new(InMemoryElementIndex::new());
                Ok(CreatedIndexes {
                    set: IndexSet {
                        element_index: element_index.clone(),
                        archive_index: element_index,
                        result_index: Arc::new(InMemoryResultIndex::new()),
                        future_queue: Arc::new(InMemoryFutureQueue::new()),
                        session_control: Arc::new(NoOpSessionControl),
                    },
                    checkpoint_store: None,
                    outbox_writer: None,
                    live_results_writer: None,
                })
            }

            fn is_volatile(&self) -> bool {
                false
            }
        }

        // A query with no explicit storage_backend should resolve to the default
        // provider injected via with_default_index_provider.
        let core = DrasiLibBuilder::new()
            .with_default_index_provider("rocks", Arc::new(MockPersistentPlugin))
            .with_query(Query::cypher("q1").query("MATCH (n) RETURN n").build())
            .build()
            .await
            .expect("build with default index provider should succeed");

        match core.config.index_factory.default_backend() {
            Some(crate::indexes::StorageBackendRef::Named(name)) => assert_eq!(name, "rocks"),
            other => panic!("expected default Named(\"rocks\"), got {other:?}"),
        }
    }

    #[test]
    fn test_query_cypher_sets_id_and_language() {
        let q = Query::cypher("cypher-q");
        assert_eq!(q.id, "cypher-q");
        assert_eq!(q.query_language, QueryLanguage::Cypher);
    }

    #[test]
    fn test_query_gql_sets_id_and_language() {
        let q = Query::gql("gql-q");
        assert_eq!(q.id, "gql-q");
        assert_eq!(q.query_language, QueryLanguage::GQL);
    }

    #[test]
    fn test_query_from_source_adds_source() {
        let q = Query::cypher("q").from_source("src1");
        assert_eq!(q.sources.len(), 1);
        assert_eq!(q.sources[0].source_id, "src1");
    }

    #[test]
    fn test_query_from_source_chaining() {
        let q = Query::cypher("q")
            .from_source("src1")
            .from_source("src2")
            .from_source("src3");
        assert_eq!(q.sources.len(), 3);
        assert_eq!(q.sources[0].source_id, "src1");
        assert_eq!(q.sources[1].source_id, "src2");
        assert_eq!(q.sources[2].source_id, "src3");
    }

    #[test]
    fn test_query_auto_start_default_true() {
        let q = Query::cypher("q");
        assert!(q.auto_start);
    }

    #[test]
    fn test_query_auto_start_false() {
        let q = Query::cypher("q").auto_start(false);
        assert!(!q.auto_start);
    }

    #[test]
    fn test_query_enable_bootstrap_default_true() {
        let q = Query::cypher("q");
        assert!(q.enable_bootstrap);
    }

    #[test]
    fn test_query_enable_bootstrap_false() {
        let q = Query::cypher("q").enable_bootstrap(false);
        assert!(!q.enable_bootstrap);
    }

    #[test]
    fn test_query_bootstrap_buffer_size_default() {
        let q = Query::cypher("q");
        assert_eq!(q.bootstrap_buffer_size, 10000);
    }

    #[test]
    fn test_query_with_bootstrap_buffer_size() {
        let q = Query::cypher("q").with_bootstrap_buffer_size(5000);
        assert_eq!(q.bootstrap_buffer_size, 5000);
    }

    #[test]
    fn test_query_with_dispatch_mode_broadcast() {
        let q = Query::cypher("q").with_dispatch_mode(DispatchMode::Broadcast);
        assert_eq!(q.dispatch_mode, Some(DispatchMode::Broadcast));
    }

    #[test]
    fn test_query_with_dispatch_mode_channel() {
        let q = Query::cypher("q").with_dispatch_mode(DispatchMode::Channel);
        assert_eq!(q.dispatch_mode, Some(DispatchMode::Channel));
    }

    #[test]
    fn test_query_dispatch_mode_default_none() {
        let q = Query::cypher("q");
        assert_eq!(q.dispatch_mode, None);
    }

    #[test]
    fn test_query_with_priority_queue_capacity() {
        let q = Query::cypher("q").with_priority_queue_capacity(50000);
        assert_eq!(q.priority_queue_capacity, Some(50000));
    }

    #[test]
    fn test_query_priority_queue_capacity_default_none() {
        let q = Query::cypher("q");
        assert_eq!(q.priority_queue_capacity, None);
    }

    #[test]
    fn test_query_with_dispatch_buffer_capacity() {
        let q = Query::cypher("q").with_dispatch_buffer_capacity(5000);
        assert_eq!(q.dispatch_buffer_capacity, Some(5000));
    }

    #[test]
    fn test_query_dispatch_buffer_capacity_default_none() {
        let q = Query::cypher("q");
        assert_eq!(q.dispatch_buffer_capacity, None);
    }

    #[test]
    fn test_query_build_propagates_all_fields() {
        let config = Query::cypher("full-query")
            .query("MATCH (n:Person) RETURN n.name")
            .from_source("source-a")
            .from_source("source-b")
            .auto_start(false)
            .enable_bootstrap(false)
            .with_bootstrap_buffer_size(5000)
            .with_priority_queue_capacity(50000)
            .with_dispatch_buffer_capacity(2500)
            .with_dispatch_mode(DispatchMode::Broadcast)
            .build();

        assert_eq!(config.id, "full-query");
        assert_eq!(config.query, "MATCH (n:Person) RETURN n.name");
        assert_eq!(config.query_language, QueryLanguage::Cypher);
        assert_eq!(config.sources.len(), 2);
        assert_eq!(config.sources[0].source_id, "source-a");
        assert_eq!(config.sources[1].source_id, "source-b");
        assert!(!config.auto_start);
        assert!(!config.enable_bootstrap);
        assert_eq!(config.bootstrap_buffer_size, 5000);
        assert_eq!(config.priority_queue_capacity, Some(50000));
        assert_eq!(config.dispatch_buffer_capacity, Some(2500));
        assert_eq!(config.dispatch_mode, Some(DispatchMode::Broadcast));
        assert!(config.joins.is_none());
        assert!(config.middleware.is_empty());
        assert!(config.storage_backend.is_none());
    }

    #[test]
    fn test_query_build_gql_propagates_language() {
        let config = Query::gql("gql-full")
            .query("MATCH (n) RETURN n")
            .from_source("src")
            .build();

        assert_eq!(config.id, "gql-full");
        assert_eq!(config.query_language, QueryLanguage::GQL);
        assert_eq!(config.query, "MATCH (n) RETURN n");
        assert_eq!(config.sources.len(), 1);
        // Verify defaults are preserved through build
        assert!(config.auto_start);
        assert!(config.enable_bootstrap);
        assert_eq!(config.bootstrap_buffer_size, 10000);
    }

    #[test]
    fn test_query_build_defaults() {
        let config = Query::cypher("defaults-only").build();

        assert_eq!(config.id, "defaults-only");
        assert_eq!(config.query, "");
        assert_eq!(config.query_language, QueryLanguage::Cypher);
        assert!(config.sources.is_empty());
        assert!(config.middleware.is_empty());
        assert!(config.auto_start);
        assert!(config.joins.is_none());
        assert!(config.enable_bootstrap);
        assert_eq!(config.bootstrap_buffer_size, 10000);
        assert_eq!(config.priority_queue_capacity, None);
        assert_eq!(config.dispatch_buffer_capacity, None);
        assert_eq!(config.dispatch_mode, None);
        assert!(config.storage_backend.is_none());
    }
}
