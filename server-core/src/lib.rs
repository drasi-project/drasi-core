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
// Public API Module
// ============================================================================

/// Public API for configuring and running Drasi Server Core
pub mod api;

// ============================================================================
// Internal Modules (crate-private, but visible to integration tests)
// ============================================================================

// These modules are internal but need to be accessible to integration tests
// that test platform-specific components
#[cfg_attr(not(test), doc(hidden))]
pub mod application;
#[cfg_attr(not(test), doc(hidden))]
pub mod bootstrap;
#[cfg_attr(not(test), doc(hidden))]
pub mod channels;
#[cfg_attr(not(test), doc(hidden))]
pub mod queries;
#[cfg_attr(not(test), doc(hidden))]
pub mod reactions;
#[cfg_attr(not(test), doc(hidden))]
pub mod routers;
#[cfg_attr(not(test), doc(hidden))]
pub mod server_core;
#[cfg_attr(not(test), doc(hidden))]
pub mod sources;
#[cfg_attr(not(test), doc(hidden))]
pub mod utils;

// Config module needs to be public for config file loading
pub mod config;

// Profiling module for performance monitoring
#[cfg_attr(not(test), doc(hidden))]
pub mod profiling;

#[cfg(test)]
mod test_support;

// ============================================================================
// Clean Public API - Everything Users Need
// ============================================================================

/// Main server type - use `DrasiServerCore::builder()` or `DrasiServerCore::from_config_file()`
///
/// # Examples
///
/// ```no_run
/// use drasi_server_core::DrasiServerCore;
///
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// // From config file
/// let core = DrasiServerCore::from_config_file("config.yaml").await?;
/// core.start().await?;
/// # Ok(())
/// # }
/// ```
pub use server_core::DrasiServerCore;

/// Builder types for fluent API
pub use api::{
    DrasiServerCoreBuilder, Properties, Query, QueryBuilder, Reaction, ReactionBuilder, Source,
    SourceBuilder,
};

/// Error types for the public API
pub use api::{DrasiError, Result};

/// Application integration handles for direct source/reaction access
pub use reactions::ApplicationReactionHandle;
pub use sources::ApplicationSourceHandle;

/// Subscription options for configuring reactions
pub use reactions::application::SubscriptionOptions;

/// Property map builder for creating source data
pub use sources::application::PropertyMapBuilder;

/// Component status type for monitoring component states
pub use channels::ComponentStatus;

// ============================================================================
// Configuration Types (for file-based config)
// ============================================================================

/// Configuration types for YAML/JSON config files
pub use config::{
    DrasiServerCoreConfig, DrasiServerCoreSettings, QueryConfig, QueryLanguage, QueryRuntime,
    ReactionConfig, ReactionRuntime, RuntimeConfig, SourceConfig, SourceRuntime,
};
