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
//! ## Error Handling
//!
//! All public API methods return [`error::Result<T>`], which uses [`DrasiError`] —
//! a structured enum that supports pattern matching on failure modes.
//!
//! Internal modules and plugin trait implementations use `anyhow::Result` for flexibility.
//! The [`DrasiError::Internal`] variant (with `#[from] anyhow::Error`) auto-converts
//! internal errors at the public API boundary via `?`.
//!
//! See the [`error`] module documentation for the full error handling architecture.

// ============================================================================
// Core Public Modules
// ============================================================================

/// Component dependency graph — the single source of truth for configuration
pub mod component_graph;

/// Opaque position type for source replay and resume
pub mod position;

/// Fluent builders for DrasiLib and components
pub mod builder;

/// Runtime context types for plugin service injection
pub mod context;

/// State store provider for persistent plugin state
pub mod state_store;

/// Error types for drasi-lib
pub mod error;

/// Identity providers for authentication credentials
pub mod identity;

/// Recovery policy and error types for checkpoint-based recovery
pub mod recovery;

// ============================================================================
// Internal Modules (crate-private, but visible to integration tests)
// ============================================================================

// These modules are internal but need to be accessible to integration tests
// that test platform-specific components
#[cfg_attr(not(test), doc(hidden))]
pub mod bootstrap;
#[cfg_attr(not(test), doc(hidden))]
pub mod channels;
#[cfg_attr(not(test), doc(hidden))]
pub mod component_ops;
#[cfg_attr(not(test), doc(hidden))]
pub mod inspection;
#[cfg_attr(not(test), doc(hidden))]
pub mod lib_core;
#[cfg_attr(not(test), doc(hidden))]
pub mod lifecycle;
#[cfg_attr(not(test), doc(hidden))]
pub mod queries;
#[cfg_attr(not(test), doc(hidden))]
pub mod reactions;
#[cfg_attr(not(test), doc(hidden))]
pub mod sources;

// Sub-modules for lib_core operations (split for maintainability)
mod lib_core_ops;
#[cfg_attr(not(test), doc(hidden))]
pub mod managers;
#[cfg_attr(not(test), doc(hidden))]
pub mod state_guard;

// Config module needs to be public for configuration types
pub mod config;

// Indexes module for storage backend configuration
pub mod indexes;

// Profiling module for performance monitoring
#[cfg_attr(not(test), doc(hidden))]
pub mod profiling;

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
pub use recovery::{RecoveryError, RecoveryPolicy};

/// Component status type for monitoring component states
pub use channels::ComponentStatus;

/// Component event for tracking lifecycle changes
pub use channels::{ComponentEvent, ComponentType};

/// Subscription response for source subscriptions
pub use channels::SubscriptionResponse;

/// Dispatch mode for configuring event routing (Broadcast or Channel)
pub use channels::DispatchMode;

/// Log level and log message types for component log streaming
pub use managers::{LogLevel, LogMessage};

/// Tracing initialization function - call to set up component log routing
pub use managers::get_or_init_global_registry;

/// Deprecated tracing initialization functions — use `get_or_init_global_registry()` instead.
#[allow(deprecated)]
pub use managers::{init_tracing, try_init_tracing};

// ============================================================================
// Configuration Types
// ============================================================================

pub use config::snapshot::QuerySnapshot;
/// Configuration types
pub use config::{
    BootstrapSnapshot, ConfigurationSnapshot, DrasiLibConfig, QueryConfig, QueryLanguage,
    QueryRuntime, ReactionRuntime, ReactionSnapshot, RuntimeConfig, SourceRuntime, SourceSnapshot,
    SourceSubscriptionSettings,
};

/// Storage backend configuration types
pub use indexes::{StorageBackendConfig, StorageBackendRef, StorageBackendSpec};

// ============================================================================
// Plugin Traits (for plugin development)
// ============================================================================

/// Source trait for implementing source plugins
pub use sources::Source;

/// Structured error type for source operations (e.g., replay position unavailable)
pub use sources::SourceError;

/// Opaque position type for source replay and resume
pub use position::SequencePosition;

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

/// Runtime context types for plugin initialization
pub use context::{QueryRuntimeContext, ReactionRuntimeContext, SourceRuntimeContext};

/// Base implementations for reaction plugins
pub use reactions::{ReactionBase, ReactionBaseParams};
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
    ComponentGraph, ComponentKind, ComponentNode, GraphEdge, GraphSnapshot, RelationshipKind,
};

// ============================================================================
// API Module (backward compatibility alias)
// ============================================================================

/// Re-export builders as `api` module for backward compatibility with tests.
/// This allows `use crate::api::{Query};` to work.
pub mod api {
    pub use crate::builder::Query;
}
