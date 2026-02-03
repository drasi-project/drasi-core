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

// ============================================================================
// Core Public Modules
// ============================================================================

/// Fluent builders for DrasiLib and components
pub mod builder;

/// Runtime context types for plugin service injection
pub mod context;

/// State store provider for persistent plugin state
pub mod state_store;

/// Error types for drasi-lib
pub mod error;

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

/// Component logger for emitting logs from components
pub use managers::ComponentLogger;

/// Logging initialization functions - call before any other logger setup
pub use managers::{
    init_logging, init_logging_with_level, init_logging_with_logger, try_init_logging,
    try_init_logging_with_level, try_init_logging_with_logger,
};

// ============================================================================
// Configuration Types
// ============================================================================

/// Configuration types
pub use config::{
    DrasiLibConfig, QueryConfig, QueryLanguage, QueryRuntime, ReactionRuntime, RuntimeConfig,
    SourceRuntime, SourceSubscriptionSettings,
};

/// Storage backend configuration types
pub use indexes::{StorageBackendConfig, StorageBackendRef, StorageBackendSpec};

// ============================================================================
// Plugin Traits (for plugin development)
// ============================================================================

/// Source trait for implementing source plugins
pub use sources::Source;

/// Reaction traits for implementing reaction plugins
pub use reactions::{QueryProvider, Reaction};

/// Bootstrap provider trait for implementing bootstrap plugins
pub use bootstrap::BootstrapProvider;

/// Index backend plugin trait for implementing storage backends
pub use indexes::IndexBackendPlugin;

/// State store provider traits and default implementation
pub use state_store::{
    MemoryStateStoreProvider, StateStoreError, StateStoreProvider, StateStoreResult,
};

/// Runtime context types for plugin initialization
pub use context::{ReactionRuntimeContext, SourceRuntimeContext};

pub use reactions::{ReactionBase, ReactionBaseParams};
/// Base implementations for source and reaction plugins
/// These are used by plugin developers, not by drasi-lib itself
pub use sources::{SourceBase, SourceBaseParams};

// ============================================================================
// Builder Types (for fluent configuration)
// ============================================================================

/// Fluent builder for DrasiLib instances
pub use builder::DrasiLibBuilder;

/// Fluent builder for query configurations
pub use builder::Query;

// ============================================================================
// API Module (backward compatibility alias)
// ============================================================================

/// Re-export builders as `api` module for backward compatibility with tests.
/// This allows `use crate::api::{Query};` to work.
pub mod api {
    pub use crate::builder::Query;
}
