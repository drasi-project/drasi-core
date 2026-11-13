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
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use super::QueryInputConfig;
use crate::channels::DispatchMode;
use crate::indexes::{StorageBackendConfig, StorageBackendRef};
use crate::recovery::RecoveryPolicy;
pub use drasi_computation_contracts::{
    ComponentId, ComponentName, ComponentReference, QueryJoinConfig, QueryJoinKeyConfig,
    QueryResultConfig,
};
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
///     inputs: [orders_db]
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
///     inputs: [users_db]
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
/// - **nodes**: Set of node labels the query is interested in
/// - **relations**: Set of physical relation labels the query is interested in
///
/// # Example
///
/// ```rust
/// use drasi_lib::config::SourceSubscriptionSettings;
///
/// let settings = SourceSubscriptionSettings {
///     source_id: drasi_lib::ComponentId::generate(),
///     enable_bootstrap: true,
///     query_id: drasi_lib::ComponentId::generate(),
///     nodes: ["Order", "Customer"].iter().map(|s| s.to_string()).collect(),
///     relations: ["PLACED_BY"].iter().map(|s| s.to_string()).collect(),
///     resume_from: None,
///     resume_sequence: None,
///     request_position_handle: false,
/// };
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSubscriptionSettings {
    pub source_id: ComponentId,
    pub enable_bootstrap: bool,
    pub query_id: ComponentId,
    /// Node labels the query is interested in. Explicit mappings are exclusive to their
    /// configured source; unmapped labels are sent to every source, which must filter out
    /// labels it does not own.
    pub nodes: HashSet<String>,
    /// Physical relation labels the query is interested in. Explicit mappings are exclusive
    /// to their configured source; unmapped labels are sent to every source, which must filter
    /// out labels it does not own.
    pub relations: HashSet<String>,
    /// If set, the subscribing query requests events replayed from this source position.
    /// Contains the opaque position bytes that the source interprets to seek its change stream.
    /// Only meaningful when the source returns `supports_replay() == true`.
    pub resume_from: Option<Bytes>,
    /// If set, the last source-local monotonic `sequence` the subscribing query
    /// durably checkpointed for this source. On resubscribe the source raises its
    /// per-source sequence counter to at least `resume_sequence + 1` (via `fetch_max`,
    /// never lowering it), so filled-in sequences after a restart stay strictly above
    /// the query's dedup high-water and are not wrongly dropped.
    ///
    /// This is independent of `resume_from` (the store-native cursor position): native-
    /// cursor sources (postgres LSN, kafka offset, mysql binlog, …) restore their stream
    /// position from `resume_from` but do not otherwise recover the framework sequence,
    /// which is what this field carries back. Sequence-as-position sources (e.g. http,
    /// grpc, application) are unaffected: their WAL-head restore (`set_next_sequence`)
    /// already advances the counter directly, and whichever floor — WAL-head restore or
    /// this field — is higher wins.
    ///
    /// For WAL-backed sources, `Some(0)` also requests replay from the beginning
    /// when a recovering query has not checkpointed this source yet. It must
    /// remain distinct from `None`, including across plugin FFI.
    pub resume_sequence: Option<u64>,
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
///         Query::cypher(drasi_lib::ComponentName::try_new("my-query")?)
///             .query("MATCH (n) RETURN n")
///             .from_input_by_name(drasi_lib::ComponentName::try_new("my-source")?)
///             .build()?
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
    /// Permit caller-assigned UUIDs and preconstructed objects. Immutable after construction.
    #[serde(default, rename = "allowExternalComponentIds")]
    pub allow_external_component_ids: bool,
    /// Default priority queue capacity for queries and reactions (default: 10000 if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_queue_capacity: Option<usize>,
    /// Default dispatch buffer capacity for sources and queries (default: 1000 if not specified)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch_buffer_capacity: Option<usize>,
    /// Per-component activation deadline in seconds (default: 300).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation_timeout_secs: Option<u64>,
    /// Overall startup deadline, including lifecycle admission and managed reconciliation (default: 600).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub startup_timeout_secs: Option<u64>,
    /// Overall stop/shutdown deadline; unfinished work remains owned (default: 60).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shutdown_timeout_secs: Option<u64>,
    /// Per-component construction/configuration deadline (default: 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub construction_timeout_secs: Option<u64>,
    /// Per-resource construction deadline (default: 30).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_construction_timeout_secs: Option<u64>,
    /// Per-owner cleanup deadline, independent of the overall shutdown budget (default: 5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleanup_timeout_secs: Option<u64>,
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
            allow_external_component_ids: false,
            priority_queue_capacity: None,
            dispatch_buffer_capacity: None,
            activation_timeout_secs: None,
            startup_timeout_secs: None,
            shutdown_timeout_secs: None,
            construction_timeout_secs: None,
            resource_construction_timeout_secs: None,
            cleanup_timeout_secs: None,
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
/// This is authoring input: `name` is required and `id` is optional. Only instances
/// constructed with `allowExternalComponentIds` accept a supplied UUID. Accepted
/// configurations use [`AssignedQueryConfig`]. Input strings select names, even
/// when UUID-shaped; `{"id":"UUID"}` selects an identity. Resolution replaces a
/// name with the selected UUID once, so rename cannot retarget a connection.
///
/// # Query Languages
///
/// Queries can be written in either:
/// - **Cypher**: Default graph pattern matching language
/// - **GQL**: ISO/IEC 39075 graph query language, not GraphQL
///
/// **Important**: ORDER BY, LIMIT, and SKIP clauses are not supported in continuous
/// queries as they conflict with incremental result computation.
///
/// # Bootstrap Processing
///
/// - **enableBootstrap**: Controls whether the query processes initial data (default: true)
/// - **priority_queue_capacity**: Capacity of the live-event inbox during bootstrap
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
/// - **inputs**: Ordered upstream components, each a name or an object with `component`, optional `port`, and optional label/pipeline mappings
/// - **result**: Optional result options; `result.label` names published result nodes
/// - **auto_start**: Start automatically (default: true)
/// - **joins**: Optional synthetic join definitions
/// - **enableBootstrap**: Process initial data (default: true)
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
///     inputs:
///       - orders_db
///     auto_start: true
///     enableBootstrap: true
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
///     inputs:
///       - orders_db
///       - customers_db
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
///     inputs:
///       - orders_db
///       - customers_db
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
///     inputs:
///       - event_stream
///     priority_queue_capacity: 100000  # Large queue for many out-of-order events
///     dispatch_buffer_capacity: 10000  # Large output buffer
/// ```
///
/// ## GQL Query
///
/// ```yaml
/// queries:
///   - id: gql_users
///     query: |
///       MATCH (u:User)
///       WHERE u.status = 'active'
///       RETURN u.id, u.name, u.email
///     queryLanguage: GQL
///     inputs:
///       - users_db
/// ```
macro_rules! query_configuration {
    ($name:ident, $input:ty { $($identity:tt)* }) => {
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct $name {
    $($identity)*
    /// Query string (Cypher or GQL depending on query_language)
    pub query: String,
    /// Query language to use (default: Cypher)
    #[serde(default, rename = "queryLanguage")]
    pub query_language: QueryLanguage,
    /// Middleware configurations for this query
    #[serde(default)]
    pub middleware: Vec<SourceMiddlewareConfig>,
    /// Upstream component inputs with optional ports, label interests and middleware pipelines
    #[serde(default)]
    pub inputs: Vec<$input>,
    /// Result publication options
    #[serde(default, skip_serializing_if = "QueryResultConfig::is_default")]
    pub result: QueryResultConfig,
    /// Optional synthetic joins for the query
    #[serde(skip_serializing_if = "Option::is_none")]
    pub joins: Option<Vec<QueryJoinConfig>>,
    /// Whether to enable bootstrap (default: true)
    #[serde(default = "default_enable_bootstrap", rename = "enableBootstrap")]
    pub enable_bootstrap: bool,
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
    /// See [`RecoveryPolicy`].
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        rename = "recoveryPolicy"
    )]
    pub recovery_policy: Option<RecoveryPolicy>,
    /// Maximum number of recent QueryResult emissions retained in the outbox ring buffer.
    /// Reactions use this buffer for gap recovery via `fetch_outbox`.
    /// Default: 1000. Minimum: 1 (values below 1 are clamped to 1).
    #[serde(default = "default_outbox_capacity", rename = "outboxCapacity")]
    pub outbox_capacity: usize,
    /// Maximum time in seconds that `fetch_snapshot` / `fetch_outbox` will wait for
    /// the query to finish bootstrapping before returning `FetchError::TimedOut`.
    /// Default: 300 (5 minutes).
    #[serde(
        default = "default_bootstrap_timeout_secs",
        rename = "bootstrapTimeoutSecs"
    )]
    pub bootstrap_timeout_secs: u64,
}
    };
}

query_configuration!(QueryConfig, QueryInputConfig {
    /// Omit for DrasiLib-owned creation. Explicit UUIDs require instance opt-in.
    #[serde(default)]
    pub id: Option<ComponentId>,
    /// Graph-owned, case-sensitive name.
    pub name: ComponentName,
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
});

query_configuration!(AssignedQueryConfig, QueryInputConfig {
    /// Required accepted identity. Missing and null IDs are invalid.
    pub id: ComponentId,
    pub name: ComponentName,
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
});

query_configuration!(QuerySettings, QueryInputConfig {});

query_configuration!(ResolvedQueryConfig, drasi_computation_contracts::QueryInputConfig {
    pub id: ComponentId,
    #[serde(default)]
    pub source_transactions: Option<drasi_computation_contracts::SourceTransactionLimits>,
});

macro_rules! query_configuration_methods {
    ($name:ident) => {
        impl $name {
            /// Query settings without the enclosing declaration's identity or lifecycle.
            pub fn into_settings(self) -> QuerySettings {
                QuerySettings {
                    query: self.query,
                    query_language: self.query_language,
                    middleware: self.middleware,
                    inputs: self.inputs,
                    result: self.result,
                    joins: self.joins,
                    enable_bootstrap: self.enable_bootstrap,
                    priority_queue_capacity: self.priority_queue_capacity,
                    dispatch_buffer_capacity: self.dispatch_buffer_capacity,
                    dispatch_mode: self.dispatch_mode,
                    storage_backend: self.storage_backend,
                    recovery_policy: self.recovery_policy,
                    outbox_capacity: self.outbox_capacity,
                    bootstrap_timeout_secs: self.bootstrap_timeout_secs,
                }
            }

            /// Reject malformed input and result declarations before any component is created.
            pub fn validate_declarations(&self) -> Result<()> {
                let mut components = HashSet::new();
                for input in &self.inputs {
                    input
                        .validate()
                        .map_err(|error| anyhow::anyhow!("Query '{}': {error}", self.name))?;
                    if input
                        .component
                        .as_name()
                        .is_some_and(|name| name == &self.name)
                    {
                        anyhow::bail!(
                            "Query '{}' cannot consume its own output as input '{}'",
                            self.name,
                            input.component
                        );
                    }
                    if !components.insert(serde_json::to_string(&input.component)?) {
                        anyhow::bail!(
                            "Query '{}' declares input '{}' more than once",
                            self.name,
                            input.component
                        );
                    }
                }
                self.result
                    .validate()
                    .map_err(|error| anyhow::anyhow!("Query '{}': {error}", self.name))
            }
        }
    };
}

query_configuration_methods!(QueryConfig);
query_configuration_methods!(AssignedQueryConfig);

impl QueryConfig {
    /// Assign configuration data; admission authority remains with DrasiLib.
    pub fn with_assigned_id(self, id: ComponentId) -> AssignedQueryConfig {
        let name = self.name.clone();
        let auto_start = self.auto_start;
        self.into_settings().with_identity(id, name, auto_start)
    }
}

impl QuerySettings {
    pub fn with_identity(
        self,
        id: ComponentId,
        name: ComponentName,
        auto_start: bool,
    ) -> AssignedQueryConfig {
        AssignedQueryConfig {
            id,
            name,
            auto_start,
            query: self.query,
            query_language: self.query_language,
            middleware: self.middleware,
            inputs: self.inputs,
            result: self.result,
            joins: self.joins,
            enable_bootstrap: self.enable_bootstrap,
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

impl AssignedQueryConfig {
    /// Resolve the execution representation only after every input is ID-bound.
    /// Names and lifecycle policy remain with the graph-owned declaration.
    pub fn into_resolved(self) -> Result<ResolvedQueryConfig> {
        Ok(ResolvedQueryConfig {
            id: self.id,
            source_transactions: None,
            query: self.query,
            query_language: self.query_language,
            middleware: self.middleware,
            inputs: self
                .inputs
                .iter()
                .map(QueryInputConfig::to_execution_input)
                .collect::<Result<_>>()?,
            result: self.result,
            joins: self.joins,
            enable_bootstrap: self.enable_bootstrap,
            priority_queue_capacity: self.priority_queue_capacity,
            dispatch_buffer_capacity: self.dispatch_buffer_capacity,
            dispatch_mode: self.dispatch_mode,
            storage_backend: self.storage_backend,
            recovery_policy: self.recovery_policy,
            outbox_capacity: self.outbox_capacity,
            bootstrap_timeout_secs: self.bootstrap_timeout_secs,
        })
    }

    /// Retain the current UUID when preparing an explicit update request.
    pub fn into_authoring(self) -> QueryConfig {
        QueryConfig {
            id: Some(self.id),
            name: self.name,
            auto_start: self.auto_start,
            query: self.query,
            query_language: self.query_language,
            middleware: self.middleware,
            inputs: self.inputs,
            result: self.result,
            joins: self.joins,
            enable_bootstrap: self.enable_bootstrap,
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

impl ResolvedQueryConfig {
    pub fn execution_settings(&self) -> drasi_computation_contracts::QueryExecutionSettings {
        drasi_computation_contracts::QueryExecutionSettings {
            joins: self.joins.clone().unwrap_or_default(),
            middleware: self.middleware.clone(),
            inputs: self.inputs.clone(),
            result: self.result.clone(),
            source_transactions: self.source_transactions,
        }
    }

    /// Attach metadata obtained from the same authoritative graph publication.
    pub fn into_assigned(self, name: ComponentName, auto_start: bool) -> AssignedQueryConfig {
        AssignedQueryConfig {
            id: self.id,
            name,
            auto_start,
            query: self.query,
            query_language: self.query_language,
            middleware: self.middleware,
            inputs: self
                .inputs
                .into_iter()
                .map(QueryInputConfig::from_execution)
                .collect(),
            result: self.result,
            joins: self.joins,
            enable_bootstrap: self.enable_bootstrap,
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
        for (name, value) in [
            ("activation_timeout_secs", self.activation_timeout_secs),
            ("startup_timeout_secs", self.startup_timeout_secs),
            ("shutdown_timeout_secs", self.shutdown_timeout_secs),
            ("construction_timeout_secs", self.construction_timeout_secs),
            (
                "resource_construction_timeout_secs",
                self.resource_construction_timeout_secs,
            ),
            ("cleanup_timeout_secs", self.cleanup_timeout_secs),
        ] {
            if let Some(seconds) = value {
                anyhow::ensure!(
                    seconds > 0
                        && std::time::Instant::now()
                            .checked_add(std::time::Duration::from_secs(seconds))
                            .is_some(),
                    "{name} must be positive and representable"
                );
            }
        }
        // Validate unique query ids
        let mut query_ids = std::collections::HashSet::new();
        let mut query_names = std::collections::HashSet::new();
        for query in &self.queries {
            if let Some(id) = &query.id {
                if !query_ids.insert(id) {
                    return Err(anyhow::anyhow!("Duplicate query id: '{id}'"));
                }
            }
            if !query_names.insert(&query.name) {
                return Err(anyhow::anyhow!("Duplicate query name: '{}'", query.name));
            }
        }

        for query in &self.queries {
            query.validate_declarations()?;
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
                    StorageBackendRef::Named(_backend_id) => {
                        // Named references are validated leniently here: a name absent
                        // from `storage_backends` may still be satisfied by a provider
                        // injected at build time via `with_index_provider`. Strict
                        // referential validation (against the union of declared backends
                        // and injected providers) is performed in the builder.
                    }
                    StorageBackendRef::Inline(spec) => {
                        // Validate inline backend configuration
                        spec.validate().map_err(|e| {
                            anyhow::anyhow!(
                                "Query '{}' has invalid inline storage backend configuration: {}",
                                query.name,
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

fn default_outbox_capacity() -> usize {
    crate::queries::output_state::DEFAULT_OUTBOX_CAPACITY
}

fn default_bootstrap_timeout_secs() -> u64 {
    300
}

#[cfg(test)]
mod tests {
    use crate::schema::{
        normalize_table_label, GraphSchema, NodeSchema, PropertySchema, PropertyType, SourceSchema,
    };
    use std::collections::BTreeSet;

    #[test]
    fn btreeset_sources_maintains_sorted_order_and_deduplication() {
        let mut graph = GraphSchema::default();
        let schema = SourceSchema {
            nodes: vec![NodeSchema {
                label: "Item".to_string(),
                properties: Vec::new(),
            }],
            relations: Vec::new(),
        };

        graph.merge_source_schema(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000027f")
                .expect("fixture UUID"),
            &schema,
        );
        graph.merge_source_schema(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000280")
                .expect("fixture UUID"),
            &schema,
        );
        graph.merge_source_schema(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000281")
                .expect("fixture UUID"),
            &schema,
        );
        graph.merge_source_schema(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000280")
                .expect("fixture UUID"),
            &schema,
        ); // duplicate

        let item = graph.nodes.get("Item").unwrap();
        let sources: Vec<_> = item.sources.iter().cloned().collect();
        assert_eq!(
            sources,
            vec![
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000027f")
                    .expect("fixture UUID"),
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000280")
                    .expect("fixture UUID"),
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000281")
                    .expect("fixture UUID"),
            ]
        );
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

        graph.merge_source_schema(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001e3")
                .expect("fixture UUID"),
            &schema_a,
        );
        graph.merge_source_schema(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001e4")
                .expect("fixture UUID"),
            &schema_b,
        );

        let sensor = graph.nodes.get("Sensor").unwrap();
        assert_eq!(
            sensor.sources,
            BTreeSet::from([
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001e3")
                    .expect("fixture UUID"),
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001e4")
                    .expect("fixture UUID"),
            ])
        );
        assert_eq!(sensor.properties.len(), 2);

        // temperature should have gotten the type from src-b
        let temp = sensor
            .properties
            .iter()
            .find(|p| p.name == "temperature")
            .unwrap();
        assert_eq!(temp.data_type, Some(PropertyType::Float));
    }
}
