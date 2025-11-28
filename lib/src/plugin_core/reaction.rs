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
//! 4. Is passed to `DrasiLib` as `Arc<dyn Reaction>`
//!
//! drasi-lib has no knowledge of which plugins exist - it only knows about this trait.
//!
//! # Event Channel Injection
//!
//! Reactions do not need to receive an event channel during construction.
//! DrasiLib automatically injects the event channel when the reaction is added
//! via `add_reaction()`. This simplifies the plugin constructor API.

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
/// # Subscription Model
///
/// Reactions manage their own subscriptions to queries using the broadcast channel pattern:
/// - Each reaction receives a reference to a QuerySubscriber on startup
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
///     // No event_tx needed - it's injected automatically by DrasiLib
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

    /// Start the reaction with access to query subscriptions
    ///
    /// The reaction should:
    /// 1. Subscribe to all configured queries via query_subscriber
    /// 2. Start its processing loop
    /// 3. Update its status to Running
    async fn start(&self, query_subscriber: Arc<dyn QuerySubscriber>) -> Result<()>;

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
}
