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

//! Plugin core module for reaction abstractions
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
//! # Dependency Injection
//!
//! Reactions receive dependencies through injection methods rather than constructor parameters:
//! - `inject_event_tx()` - Event channel for component lifecycle events
//! - `inject_query_subscriber()` - Query subscriber for accessing queries
//!
//! Both are automatically called by DrasiLib when the reaction is added via `add_reaction()`.
//! This simplifies the plugin constructor API and enables auto-start functionality.

use anyhow::Result;
use async_trait::async_trait;
use std::sync::Arc;

use crate::channels::{ComponentEventSender, ComponentStatus};
use crate::queries::Query;

/// Trait for subscribing to queries without requiring full DrasiLib dependency.
///
/// This trait provides a way for reactions to access query instances for subscription
/// without needing a direct dependency on the server core.
#[async_trait]
pub trait QuerySubscriber: Send + Sync {
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
/// - QuerySubscriber is injected via `inject_query_subscriber()` at add time
/// - Reactions access queries via `query_subscriber.get_query_instance()`
/// - For each query, reactions call `query.subscribe(reaction_id)`
/// - Each subscription provides a broadcast receiver for that query's results
/// - Reactions use a priority queue to process results from multiple queries in timestamp order
///
/// # Example Implementation
///
/// ```ignore
/// use drasi_lib::plugin_core::{Reaction, QuerySubscriber};
/// use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
///
/// pub struct MyReaction {
///     base: ReactionBase,
///     // Plugin-specific fields
/// }
///
/// impl MyReaction {
///     // No event_tx or query_subscriber needed - injected automatically by DrasiLib
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
///     async fn inject_event_tx(&self, tx: ComponentEventSender) {
///         self.base.inject_event_tx(tx).await;
///     }
///
///     async fn inject_query_subscriber(&self, qs: Arc<dyn QuerySubscriber>) {
///         self.base.inject_query_subscriber(qs).await;
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

    /// Inject the query subscriber for accessing queries
    ///
    /// This method is called automatically by DrasiLib when the reaction is added
    /// via `add_reaction()`. Plugin developers do not need to call this directly.
    ///
    /// Implementation should delegate to `self.base.inject_query_subscriber(qs).await`.
    async fn inject_query_subscriber(&self, query_subscriber: Arc<dyn QuerySubscriber>);

    /// Start the reaction
    ///
    /// The reaction should:
    /// 1. Subscribe to all configured queries (using injected QuerySubscriber)
    /// 2. Start its processing loop
    /// 3. Update its status to Running
    ///
    /// Note: QuerySubscriber is already available via `inject_query_subscriber()` which
    /// is called when the reaction is added to DrasiLib.
    async fn start(&self) -> Result<()>;

    /// Stop the reaction, cleaning up all subscriptions and tasks
    async fn stop(&self) -> Result<()>;

    /// Get the current status of the reaction
    async fn status(&self) -> ComponentStatus;

    /// Inject the event channel for component lifecycle events
    ///
    /// This method is called automatically by DrasiLib when the reaction is added
    /// via `add_reaction()`. Plugin developers do not need to call this directly.
    ///
    /// Implementation should delegate to `self.base.inject_event_tx(tx).await`.
    async fn inject_event_tx(&self, tx: ComponentEventSender);

    /// Inject the state store provider for persistent state storage
    ///
    /// This method is called automatically by DrasiLib when the reaction is added
    /// via `add_reaction()`. Plugin developers do not need to call this directly.
    ///
    /// Reactions that need to persist state should store this reference and use it
    /// to read/write state data. The store_id used should typically be the reaction's ID.
    async fn inject_state_store(
        &self,
        _state_store: std::sync::Arc<dyn crate::plugin_core::StateStoreProvider>,
    ) {
        // Default implementation does nothing - reactions that need state storage
        // should override this to store the reference
    }
}

/// Blanket implementation of Reaction for Box<dyn Reaction>
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

    async fn inject_query_subscriber(&self, query_subscriber: Arc<dyn QuerySubscriber>) {
        (**self).inject_query_subscriber(query_subscriber).await
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

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        (**self).inject_event_tx(tx).await
    }

    async fn inject_state_store(
        &self,
        state_store: std::sync::Arc<dyn crate::plugin_core::StateStoreProvider>,
    ) {
        (**self).inject_state_store(state_store).await
    }
}
