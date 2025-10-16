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

// Public API module - clean interface for users
pub mod api;

// Internal modules - public for internal use but hidden from docs
#[doc(hidden)]
pub mod application;
#[doc(hidden)]
pub mod bootstrap;
#[doc(hidden)]
pub mod channels;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod error;
#[doc(hidden)]
pub mod queries;
#[doc(hidden)]
pub mod reactions;
#[doc(hidden)]
pub mod routers;
#[doc(hidden)]
pub mod server_core;
#[doc(hidden)]
pub mod sources;
#[doc(hidden)]
pub mod utils;

#[cfg(test)]
mod test_support;

// ============================================================================
// New Clean Public API
// ============================================================================

/// Main server type - use `DrasiServerCore::builder()` or `DrasiServerCore::from_config_file()`
pub use server_core::DrasiServerCore;

/// Builder types for fluent API
pub use api::{
    DrasiServerCoreBuilder, Properties, Query, QueryBuilder, Reaction, ReactionBuilder, Source,
    SourceBuilder,
};

/// Error types
pub use api::{DrasiError, Result};

/// Application integration handles
pub use sources::ApplicationSourceHandle;
pub use reactions::ApplicationReactionHandle;

/// Subscription options for reactions
pub use reactions::application::SubscriptionOptions;

/// Property map builder for sources
pub use sources::application::PropertyMapBuilder;

// ============================================================================
// Re-exported for backward compatibility (will be deprecated)
// ============================================================================

#[doc(hidden)]
pub use application::ApplicationHandle;
#[doc(hidden)]
pub use channels::{ComponentEvent, ComponentStatus, QueryResult};
#[doc(hidden)]
pub use config::{DrasiServerCoreConfig, QueryConfig, ReactionConfig, RuntimeConfig, SourceConfig};

// Manager types - these are internal and should not be used directly
// They are temporarily exported for backward compatibility
#[doc(hidden)]
pub use queries::{Query as QueryTrait, QueryManager};
#[doc(hidden)]
pub use reactions::{Reaction as ReactionTrait, ReactionManager};
#[doc(hidden)]
pub use sources::{Source as SourceTrait, SourceManager};
