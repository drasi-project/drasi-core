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

//! # drasi-lib
//!
//! Embedded library for running Drasi continuous queries with sources, queries, and reactions.
//!
//! ## Choosing an interface
//!
//! | Task | Start here |
//! |---|---|
//! | Embed Drasi and configure ordinary queries | [`DrasiLib`], [`DrasiLibBuilder`], [`Query`], [`config`] |
//! | Assemble in-process native components | [`computation::v1`]: component traits, factories, ports, resources and graph control |
//! | Read component state, logs or events | [`component_graph::ComponentGraphView`], [`observability`], [`metrics`] |
//! | Supply host-owned storage or configuration services | [`indexes`], [`management`], [`state_store`], [`wal`] |
//! | Maintain an ordinary Source/Reaction plugin | [`sources`], [`reactions`], [`bootstrap`], [`channels`], [`context`] |
//!
//! A dynamically loaded **native plugin** implements the traits in the
//! [native plugin SDK](https://docs.rs/drasi-computation-plugin-sdk), not this
//! crate's graph-owning traits. Its shared messages and codecs come from
//! [`drasi_computation_contracts`]; values come from `drasi-core-types`.
//! The Host SDK connects those plugin operations to this runtime. Do not pass
//! Rust trait objects, resource owners or graph handles across a library boundary.
//!
//! The graph and shared-contract imports refer to the same types:
//!
//! ```
//! use drasi_computation_contracts::ComponentId;
//! use drasi_lib::computation::v1::ComponentId as GraphComponentId;
//!
//! let id: GraphComponentId = ComponentId::try_new("processor")?;
//! assert_eq!(id.as_str(), "processor");
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ### Existing ordinary plugin interfaces
//!
//! Ordinary [`Source`], [`Reaction`] and [`BootstrapProvider`] contracts remain
//! available while native migration proceeds. Their base helpers and contexts
//! are documented in the corresponding modules; they are not the native
//! dynamic-plugin interface. The old import paths and root exports are identical:
//!
//! ```
//! use drasi_lib::{BootstrapProvider, Reaction, Source};
//!
//! fn source_api<T: Source + ?Sized>(value: &T) {
//!     fn same_trait<T: drasi_lib::sources::Source + ?Sized>(_: &T) {}
//!     same_trait(value);
//! }
//! fn reaction_api<T: Reaction + ?Sized>(value: &T) {
//!     fn same_trait<T: drasi_lib::reactions::Reaction + ?Sized>(_: &T) {}
//!     same_trait(value);
//! }
//! fn bootstrap_api<T: BootstrapProvider + ?Sized>(value: &T) {
//!     fn same_trait<T: drasi_lib::bootstrap::BootstrapProvider + ?Sized>(_: &T) {}
//!     same_trait(value);
//! }
//! ```
//!
//! Use [`QueryConfigurationSnapshot`] for saved settings and
//! [`computation::v1::QueryResultSnapshot`] for result rows. Logging and event
//! history use [`observability`]. Retired query helpers and old import aliases
//! are not part of this breaking release.
//!
//! Some public implementation modules remain hidden from the main documentation
//! index. They are **not crate-private**: existing callers may depend on their
//! imports, so hiding them is not permission to remove them. Prefer the supported
//! entry points above rather than adding dependencies on internal manager or
//! state-guard implementations.
//!
//! ### Ownership and lifetime
//!
//! The graph owns component lifecycle and declared resources. A borrowed service
//! does not grant permission to commit unrelated state or retain it after
//! replacement. Components must retain and join their own background work;
//! cancelling a wait is not completed cleanup. See [`context::workers`],
//! [`management::ConfigurationSession`] and [`computation::v1::GraphRun`].
//! Query readers observe an existing component; they do not own another evaluator.
//!
//! ## Error Handling
//!
//! Ordinary [`DrasiLib`] operations use [`error::Result`], which carries
//! [`DrasiError`]. Use [`DrasiError::classification`] for the public category and
//! [`DrasiError::downcast_ref`] for retained typed causes. Graph APIs expose
//! [`computation::v1::GraphError`] and plugin/provider traits use their documented
//! result types.
//!
//! Internal modules and plugin trait implementations use `anyhow::Result` for flexibility.
//! The [`DrasiError::Internal`] variant (with `#[from] anyhow::Error`) auto-converts
//! internal errors at the public API boundary via `?`.
//!
//! See the [`error`] module documentation for the full error handling architecture.

// ============================================================================
// Core Public Modules
// ============================================================================

/// Read-only component inspection.
pub mod component_graph;

/// Fluent builders for DrasiLib and components
pub mod builder;

/// Runtime context types for plugin service injection
pub mod context;

/// State store provider for persistent plugin state
pub mod state_store;

/// Secret store provider for resolving named secrets at runtime
pub mod secret_store;

/// Write-Ahead Log plugin contract for transient source crash recovery
pub mod wal;

/// Error types for drasi-lib
pub mod error;

/// Computation contracts and the authoritative DAG runtime.
pub mod computation;
/// Optional desired-configuration management and host-supplied storage contracts.
pub mod management;

/// Identity providers for authentication credentials
pub mod identity;

/// Recovery policy and error types for checkpoint-based recovery
pub mod recovery;

// ============================================================================
// Public ordinary integration modules
// ============================================================================

/// Ordinary bootstrap providers and their snapshot request/context types.
pub mod bootstrap;
/// Ordinary source/reaction messages, status and dispatch contracts.
pub mod channels;
/// Public implementation helpers retained for existing callers; prefer `DrasiLib`.
#[cfg_attr(not(test), doc(hidden))]
pub mod component_ops;
/// Compatibility inspection implementation; prefer `DrasiLib` and `ComponentGraphView`.
#[cfg_attr(not(test), doc(hidden))]
pub mod inspection;
/// Implementation of the root-exported `DrasiLib`; prefer the root import.
#[cfg_attr(not(test), doc(hidden))]
pub mod lib_core;
/// Query read contracts, snapshot/outbox exchange types and ordinary API facades.
pub mod queries;
/// Ordinary reaction traits, base helpers and result-snapshot interfaces.
pub mod reactions;
/// Ordinary source traits, base helpers and subscription interfaces.
pub mod sources;

// Sub-modules for lib_core operations (split for maintainability)
mod lib_core_ops;
/// Public initialization helper retained for existing callers, not a native service.
#[cfg_attr(not(test), doc(hidden))]
pub mod state_guard;

// Config module needs to be public for configuration types
pub mod config;

// Schema discovery types (separate from configuration)
pub mod schema;

// Indexes module for storage backend configuration
pub mod indexes;

/// Optional processing-time metadata and profiling interfaces.
pub mod profiling;

// Observability metrics for the Resumable Reactions pipeline
pub mod metrics;

/// Component logs, tracing integration and lifecycle-event history.
pub mod observability;

#[cfg(test)]
pub(crate) mod test_helpers;

#[cfg(test)]
mod lifecycle_events_tests;

// ============================================================================
// Clean Public API - Everything Users Need
// ============================================================================

/// Main server type - use `DrasiLib::builder()` to create instances
///
/// # Examples
///
/// ```no_run
/// use drasi_lib::DrasiLib;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// let core = DrasiLib::builder()
///     .with_id("my-server")
///     .build()
///     .await?;
/// core.start().await?;
/// # Ok(())
/// # }
/// ```
pub use lib_core::DrasiLib;

/// Error types for drasi-lib
pub use error::{DrasiError, Result};

/// Recovery policy and error types for checkpoint-based recovery
pub use recovery::{ReactionRecoveryPolicy, RecoveryError, RecoveryPolicy};

/// Component status type for monitoring component states
pub use channels::ComponentStatus;

/// Component event for tracking lifecycle changes
pub use channels::{ComponentEvent, ComponentType};

/// Subscription response for source subscriptions
pub use channels::SubscriptionResponse;

/// Dispatch mode for configuring event routing (Broadcast or Channel)
pub use channels::DispatchMode;

/// Log level and log message types for component log streaming
pub use observability::{ComponentLogType, LogLevel, LogMessage};

/// Tracing initialization function - call to set up component log routing
pub use observability::get_or_init_global_registry;

// ============================================================================
// Configuration Types
// ============================================================================

pub use config::snapshot::QueryConfigurationSnapshot;
/// Configuration types
pub use config::{
    BootstrapSnapshot, ConfigurationSnapshot, DrasiLibConfig, QueryConfig, QueryInputConfig,
    QueryLanguage, QueryResultConfig, QueryRuntime, ReactionRuntime, ReactionSnapshot,
    RuntimeConfig, SourceRuntime, SourceSnapshot, SourceSubscriptionSettings,
};

/// Schema discovery types.
pub use schema::{
    normalize_table_label, GraphNodeSchema, GraphRelationSchema, GraphSchema, NodeSchema,
    PropertySchema, PropertyType, RelationSchema, SourceSchema,
};

/// Storage backend configuration types
pub use indexes::{StorageBackendConfig, StorageBackendRef, StorageBackendSpec};

// ============================================================================
// Ordinary plugin and host-provider traits (not the native dynamic SDK)
// ============================================================================

/// Source trait for implementing source plugins
pub use sources::Source;

/// Structured error type for source operations (e.g., replay position unavailable)
pub use sources::SourceError;

/// Reaction traits for implementing reaction plugins
pub use reactions::Reaction;

/// Bootstrap provider trait for implementing bootstrap plugins
pub use bootstrap::BootstrapProvider;
/// Bootstrap provider that generates data from the component graph
pub use bootstrap::ComponentGraphBootstrapProvider;

/// Index backend plugin trait for implementing storage backends
pub use indexes::IndexBackendPlugin;

/// State store provider traits and default implementation
pub use state_store::{
    MemoryStateStoreProvider, StateStoreError, StateStoreProvider, StateStoreResult,
};

/// Secret store provider traits and default implementation
pub use secret_store::{MemorySecretStoreProvider, SecretStoreProvider};

/// Write-Ahead Log plugin contract and configuration types
pub use wal::{
    CapacityPolicy, DurabilityConfig, WalError, WalProvider, WriteAheadLogConfig, MIN_MAX_EVENTS,
};

/// Runtime context types for plugin initialization
pub use context::{QueryRuntimeContext, ReactionRuntimeContext, SourceRuntimeContext};

/// Checkpoint type for durable reaction progress tracking
pub use reactions::ReactionCheckpoint;
/// Runtime snapshot fetcher trait for on-demand query access
pub use reactions::SnapshotFetcher;
/// Base implementations for reaction plugins
pub use reactions::{ReactionBase, ReactionBaseParams};
/// Position comparison trait for per-subscriber replay filtering
pub use sources::{ByteLexPositionComparator, PositionComparator};
/// Base implementations for source plugins
pub use sources::{SourceBase, SourceBaseParams};

// ============================================================================
// Builder Types (for fluent configuration)
// ============================================================================

/// Fluent builder for DrasiLib instances
pub use builder::DrasiLibBuilder;

/// Fluent builder for query configurations
pub use builder::Query;

/// Async helper to wait for a component to reach a target status without polling.
pub use component_graph::wait_for_status;
/// Component graph types for dependency tracking and configuration queries
pub use component_graph::{
    ComponentGraphView, ComponentKind, ComponentNode, GraphEdge, GraphSnapshot, RelationshipKind,
};
