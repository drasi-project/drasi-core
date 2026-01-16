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

//! Reaction trait module
//!
//! This module provides the core traits that all reaction plugins must implement.
//! It separates the plugin contract from the reaction manager and implementation details.
//!
//! # Plugin Architecture
//!
//! Each reaction plugin:
//! 1. Defines its own typed configuration struct with builder pattern
//! 2. Creates a `ReactionBase` instance using `ReactionBaseParams`
//! 3. Implements the `Reaction` trait
//! 4. Is passed to `DrasiLib` via `add_reaction()` which takes ownership
//!
//! drasi-lib has no knowledge of which plugins exist - it only knows about this trait.
//!
//! # Runtime Context Initialization
//!
//! Reactions receive all drasi-lib services through a single `initialize()` call
//! when added to DrasiLib. The `ReactionRuntimeContext` provides:
//! - `event_tx`: Channel for component lifecycle events
//! - `state_store`: Optional persistent state storage
//! - `query_provider`: Access to query instances for subscription
//!
//! This replaces the previous `inject_*` methods with a cleaner single-call pattern.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::channels::ComponentStatus;
use crate::context::ReactionRuntimeContext;
use crate::queries::Query;

/// Trait for providing access to queries without requiring full DrasiLib dependency.
///
/// This trait provides a way for reactions to access query instances for subscription
/// without needing a direct dependency on the server core.
#[async_trait]
pub trait QueryProvider: Send + Sync {
    /// Get a query instance by ID
    async fn get_query_instance(&self, id: &str) -> Result<Arc<dyn Query>>;
}

/// Trait defining the interface for all reaction implementations.
///
/// This is the core abstraction that all reaction plugins must implement.
/// drasi-lib only interacts with reactions through this trait - it has no
/// knowledge of specific plugin types or their configurations.
///
/// # Lifecycle
///
/// Reactions follow this lifecycle:
/// 1. Created by plugin code with configuration
/// 2. Added to DrasiLib via `add_reaction()` - dependencies injected automatically
/// 3. Started via `start()` (auto-start or manual based on `auto_start()`)
/// 4. Stopped via `stop()` when no longer needed
///
/// # Subscription Model
///
/// Reactions manage their own subscriptions to queries using the broadcast channel pattern:
/// - QueryProvider is injected via `inject_query_provider()` at add time
/// - Reactions access queries via `query_provider.get_query_instance()`
/// - For each query, reactions call `query.subscribe(reaction_id)`
/// - Each subscription provides a broadcast receiver for that query's results
/// - Reactions use a priority queue to process results from multiple queries in timestamp order
///
/// # Example Implementation
///
/// ```ignore
/// use drasi_lib::{Reaction, QueryProvider};
/// use drasi_lib::reactions::{ReactionBase, ReactionBaseParams};
/// use drasi_lib::context::ReactionRuntimeContext;
///
/// pub struct MyReaction {
///     base: ReactionBase,
///     // Plugin-specific fields
/// }
///
/// impl MyReaction {
///     pub fn new(config: MyReactionConfig) -> Self {
///         let params = ReactionBaseParams::new(&config.id, config.queries.clone())
///             .with_priority_queue_capacity(config.queue_capacity);
///
///         Self {
///             base: ReactionBase::new(params),
///         }
///     }
/// }
///
/// #[async_trait]
/// impl Reaction for MyReaction {
///     fn id(&self) -> &str {
///         &self.base.id
///     }
///
///     fn type_name(&self) -> &str {
///         "my-reaction"
///     }
///
///     fn query_ids(&self) -> Vec<String> {
///         self.base.queries.clone()
///     }
///
///     async fn initialize(&self, context: ReactionRuntimeContext) {
///         self.base.initialize(context).await;
///     }
///
///     async fn start(&self) -> Result<()> {
///         self.base.subscribe_to_queries().await?;
///         // ... start processing
///         Ok(())
///     }
///
///     // ... implement other methods
/// }
/// ```
#[async_trait]
pub trait Reaction: Send + Sync {
    /// Get the reaction's unique identifier
    fn id(&self) -> &str;

    /// Get the reaction type name (e.g., "http", "log", "sse")
    fn type_name(&self) -> &str;

    /// Get the reaction's configuration properties for inspection
    ///
    /// This returns a HashMap representation of the reaction's configuration
    /// for use in APIs and inspection. The actual typed configuration is
    /// owned by the plugin - this is just for external visibility.
    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value>;

    /// Get the list of query IDs this reaction subscribes to
    fn query_ids(&self) -> Vec<String>;

    /// Whether this reaction should auto-start when DrasiLib starts
    ///
    /// Default is `true` to match query behavior. Override to return `false`
    /// if this reaction should only be started manually via `start_reaction()`.
    fn auto_start(&self) -> bool {
        true
    }

    /// Initialize the reaction with runtime context.
    ///
    /// This method is called automatically by DrasiLib when the reaction is added
    /// via `add_reaction()`. Plugin developers do not need to call this directly.
    ///
    /// The context provides access to:
    /// - `reaction_id`: The reaction's unique identifier
    /// - `event_tx`: Channel for reporting component lifecycle events
    /// - `state_store`: Optional persistent state storage
    /// - `query_provider`: Access to query instances for subscription
    ///
    /// Implementation should delegate to `self.base.initialize(context).await`.
    async fn initialize(&self, context: ReactionRuntimeContext);

    /// Start the reaction
    ///
    /// The reaction should:
    /// 1. Subscribe to all configured queries (using injected QueryProvider)
    /// 2. Start its processing loop
    /// 3. Update its status to Running
    ///
    /// Note: QueryProvider is already available via `inject_query_provider()` which
    /// is called when the reaction is added to DrasiLib.
    async fn start(&self) -> Result<()>;

    /// Stop the reaction, cleaning up all subscriptions and tasks
    async fn stop(&self) -> Result<()>;

    /// Get the current status of the reaction
    async fn status(&self) -> ComponentStatus;
}

/// Blanket implementation of Reaction for `Box<dyn Reaction>`
///
/// This allows boxed trait objects to be used with methods expecting `impl Reaction`.
#[async_trait]
impl Reaction for Box<dyn Reaction + 'static> {
    fn id(&self) -> &str {
        (**self).id()
    }

    fn type_name(&self) -> &str {
        (**self).type_name()
    }

    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        (**self).properties()
    }

    fn query_ids(&self) -> Vec<String> {
        (**self).query_ids()
    }

    fn auto_start(&self) -> bool {
        (**self).auto_start()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        (**self).initialize(context).await
    }

    async fn start(&self) -> Result<()> {
        (**self).start().await
    }

    async fn stop(&self) -> Result<()> {
        (**self).stop().await
    }

    async fn status(&self) -> ComponentStatus {
        (**self).status().await
    }
}
