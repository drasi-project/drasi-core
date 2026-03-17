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

//! Runtime context types for plugin service injection.
//!
//! This module provides `SourceRuntimeContext` and `ReactionRuntimeContext` structs
//! that contain `Arc<T>` instances for drasi-lib provided services. These contexts
//! are provided to plugins when they are added to DrasiLib via the `initialize()` method.
//!
//! # Overview
//!
//! Instead of multiple `inject_*` methods, plugins now receive all services through
//! a single `initialize(context)` call. This provides:
//!
//! - **Single initialization point**: One call instead of multiple inject calls
//! - **Guaranteed complete initialization**: Context is complete or doesn't exist
//! - **Clearer API contract**: All available services visible at a glance
//! - **Extensibility**: New services can be added without changing traits
//!
//! # Example - Source Plugin
//!
//! ```ignore
//! use drasi_lib::context::SourceRuntimeContext;
//!
//! #[async_trait]
//! impl Source for MySource {
//!     async fn initialize(&self, context: SourceRuntimeContext) {
//!         // Store context for later use
//!         self.base.initialize(context).await;
//!     }
//!     // ...
//! }
//! ```
//!
//! # Example - Reaction Plugin
//!
//! ```ignore
//! use drasi_lib::context::ReactionRuntimeContext;
//!
//! #[async_trait]
//! impl Reaction for MyReaction {
//!     async fn initialize(&self, context: ReactionRuntimeContext) {
//!         // Store context for later use
//!         self.base.initialize(context).await;
//!     }
//!     // ...
//! }
//! ```

use std::sync::Arc;
use tokio::sync::RwLock;

use crate::component_graph::ComponentUpdateSender;
use crate::identity::IdentityProvider;
use crate::reactions::QueryProvider;
use crate::state_store::StateStoreProvider;

/// Context provided to Source plugins during initialization.
///
/// Contains `Arc<T>` instances for all drasi-lib provided services.
/// DrasiLib constructs this context when a source is added via `add_source()`.
///
/// # Available Services
///
/// - `instance_id`: The DrasiLib instance ID (for log routing isolation)
/// - `source_id`: The unique identifier for this source instance
/// - `state_store`: Optional persistent state storage (if configured)
/// - `update_tx`: mpsc sender for fire-and-forget status updates to the component graph
///
/// # Clone
///
/// This struct implements `Clone` and all fields use `Arc` internally,
/// making cloning cheap (just reference count increments).
#[derive(Clone)]
pub struct SourceRuntimeContext {
    /// DrasiLib instance ID (for log routing isolation)
    pub instance_id: String,

    /// Unique identifier for this source instance
    pub source_id: String,

    /// Optional persistent state storage.
    ///
    /// This is `Some` if a state store provider was configured on DrasiLib,
    /// otherwise `None`. Sources can use this to persist state across restarts.
    pub state_store: Option<Arc<dyn StateStoreProvider>>,

    /// mpsc sender for fire-and-forget component status updates.
    ///
    /// Status changes sent here are applied to the component graph by the
    /// graph update loop, which emits broadcast events to all subscribers.
    pub update_tx: ComponentUpdateSender,

    /// Optional identity provider for credential injection.
    ///
    /// This is `Some` if the host has configured an identity provider for this component.
    /// Sources can use this to obtain authentication credentials (passwords, tokens,
    /// certificates) for connecting to external systems.
    pub identity_provider: Option<Arc<dyn IdentityProvider>>,
}

impl SourceRuntimeContext {
    /// Create a new source runtime context.
    ///
    /// This is typically called by `SourceManager` when adding a source to DrasiLib.
    /// Plugin developers do not need to call this directly.
    ///
    /// # Arguments
    ///
    /// * `instance_id` - The DrasiLib instance ID
    /// * `source_id` - The unique identifier for this source
    /// * `state_store` - Optional persistent state storage
    /// * `update_tx` - mpsc sender for status updates to the component graph
    /// * `identity_provider` - Optional identity provider for credential injection
    pub fn new(
        instance_id: impl Into<String>,
        source_id: impl Into<String>,
        state_store: Option<Arc<dyn StateStoreProvider>>,
        update_tx: ComponentUpdateSender,
        identity_provider: Option<Arc<dyn IdentityProvider>>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            source_id: source_id.into(),
            state_store,
            update_tx,
            identity_provider,
        }
    }

    /// Get the DrasiLib instance ID.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Get the source's unique identifier.
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// Get a reference to the state store if configured.
    ///
    /// Returns `Some(&Arc<dyn StateStoreProvider>)` if a state store was configured,
    /// otherwise `None`.
    pub fn state_store(&self) -> Option<&Arc<dyn StateStoreProvider>> {
        self.state_store.as_ref()
    }
}

impl std::fmt::Debug for SourceRuntimeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceRuntimeContext")
            .field("instance_id", &self.instance_id)
            .field("source_id", &self.source_id)
            .field(
                "state_store",
                &self.state_store.as_ref().map(|_| "<StateStoreProvider>"),
            )
            .field("update_tx", &"<ComponentUpdateSender>")
            .field(
                "identity_provider",
                &self
                    .identity_provider
                    .as_ref()
                    .map(|_| "<IdentityProvider>"),
            )
            .finish()
    }
}

/// Context provided to Reaction plugins during initialization.
///
/// Contains `Arc<T>` instances for all drasi-lib provided services.
/// DrasiLib constructs this context when a reaction is added via `add_reaction()`.
///
/// # Available Services
///
/// - `instance_id`: The DrasiLib instance ID (for log routing isolation)
/// - `reaction_id`: The unique identifier for this reaction instance
/// - `state_store`: Optional persistent state storage (if configured)
/// - `query_provider`: Access to query instances for subscription
/// - `update_tx`: mpsc sender for fire-and-forget status updates to the component graph
/// - `identity_provider`: Optional identity provider for credential injection
///
/// # Clone
///
/// This struct implements `Clone` and all fields use `Arc` internally,
/// making cloning cheap (just reference count increments).
#[derive(Clone)]
pub struct ReactionRuntimeContext {
    /// DrasiLib instance ID (for log routing isolation)
    pub instance_id: String,

    /// Unique identifier for this reaction instance
    pub reaction_id: String,

    /// Optional persistent state storage.
    ///
    /// This is `Some` if a state store provider was configured on DrasiLib,
    /// otherwise `None`. Reactions can use this to persist state across restarts.
    pub state_store: Option<Arc<dyn StateStoreProvider>>,

    /// Access to query instances and subscription.
    ///
    /// Reactions use this to get query instances and subscribe to their results.
    /// This is always available (not optional) since reactions require queries.
    pub query_provider: Arc<dyn QueryProvider>,

    /// mpsc sender for fire-and-forget component status updates.
    ///
    /// Status changes sent here are applied to the component graph by the
    /// graph update loop, which emits broadcast events to all subscribers.
    pub update_tx: ComponentUpdateSender,

    /// Optional identity provider for credential injection.
    ///
    /// This is `Some` if the host has configured an identity provider for this component.
    /// Reactions can use this to obtain authentication credentials (passwords, tokens,
    /// certificates) for connecting to external systems.
    pub identity_provider: Option<Arc<dyn IdentityProvider>>,
}

impl ReactionRuntimeContext {
    /// Create a new reaction runtime context.
    ///
    /// This is typically called by `ReactionManager` when adding a reaction to DrasiLib.
    /// Plugin developers do not need to call this directly.
    ///
    /// # Arguments
    ///
    /// * `instance_id` - The DrasiLib instance ID
    /// * `reaction_id` - The unique identifier for this reaction
    /// * `state_store` - Optional persistent state storage
    /// * `query_provider` - Access to query instances for subscription
    /// * `update_tx` - mpsc sender for status updates to the component graph
    /// * `identity_provider` - Optional identity provider for credential injection
    pub fn new(
        instance_id: impl Into<String>,
        reaction_id: impl Into<String>,
        state_store: Option<Arc<dyn StateStoreProvider>>,
        query_provider: Arc<dyn QueryProvider>,
        update_tx: ComponentUpdateSender,
        identity_provider: Option<Arc<dyn IdentityProvider>>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            reaction_id: reaction_id.into(),
            state_store,
            query_provider,
            update_tx,
            identity_provider,
        }
    }

    /// Get the DrasiLib instance ID.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Get the reaction's unique identifier.
    pub fn reaction_id(&self) -> &str {
        &self.reaction_id
    }

    /// Get a reference to the state store if configured.
    pub fn state_store(&self) -> Option<&Arc<dyn StateStoreProvider>> {
        self.state_store.as_ref()
    }
}

impl std::fmt::Debug for ReactionRuntimeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReactionRuntimeContext")
            .field("instance_id", &self.instance_id)
            .field("reaction_id", &self.reaction_id)
            .field(
                "state_store",
                &self.state_store.as_ref().map(|_| "<StateStoreProvider>"),
            )
            .field("query_provider", &"<QueryProvider>")
            .field("update_tx", &"<ComponentUpdateSender>")
            .field(
                "identity_provider",
                &self
                    .identity_provider
                    .as_ref()
                    .map(|_| "<IdentityProvider>"),
            )
            .finish()
    }
}

/// Context provided to Query components during initialization.
///
/// Contains the DrasiLib instance ID and update channel for status reporting.
/// Constructed by `QueryManager` when a query is added via `add_query()`.
///
/// Unlike sources and reactions, queries are internal to drasi-lib (not plugins),
/// but still follow the same context-based initialization pattern for consistency.
///
/// # Clone
///
/// This struct implements `Clone` and uses `Arc` internally for the update channel,
/// making cloning cheap (just reference count increments).
#[derive(Clone)]
pub struct QueryRuntimeContext {
    /// DrasiLib instance ID (for log routing isolation)
    pub instance_id: String,

    /// Unique identifier for this query instance
    pub query_id: String,

    /// mpsc sender for fire-and-forget component status updates.
    ///
    /// Status changes sent here are applied to the component graph by the
    /// graph update loop, which emits broadcast events to all subscribers.
    pub update_tx: ComponentUpdateSender,
}

impl QueryRuntimeContext {
    /// Create a new query runtime context.
    ///
    /// This is typically called by `QueryManager` when adding a query to DrasiLib.
    ///
    /// # Arguments
    ///
    /// * `instance_id` - The DrasiLib instance ID
    /// * `query_id` - The unique identifier for this query
    /// * `update_tx` - mpsc sender for status updates to the component graph
    pub fn new(
        instance_id: impl Into<String>,
        query_id: impl Into<String>,
        update_tx: ComponentUpdateSender,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            query_id: query_id.into(),
            update_tx,
        }
    }

    /// Get the DrasiLib instance ID.
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Get the query's unique identifier.
    pub fn query_id(&self) -> &str {
        &self.query_id
    }
}

impl std::fmt::Debug for QueryRuntimeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("QueryRuntimeContext")
            .field("instance_id", &self.instance_id)
            .field("query_id", &self.query_id)
            .field("update_tx", &"<ComponentUpdateSender>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::component_graph::ComponentGraph;
    use crate::queries::Query;
    use crate::state_store::MemoryStateStoreProvider;
    use async_trait::async_trait;
    use std::sync::Arc;

    // Mock QueryProvider for testing
    struct MockQueryProvider;

    #[async_trait]
    impl QueryProvider for MockQueryProvider {
        async fn get_query_instance(&self, _id: &str) -> anyhow::Result<Arc<dyn Query>> {
            Err(anyhow::anyhow!("MockQueryProvider: query not found"))
        }
    }

    fn test_update_tx() -> ComponentUpdateSender {
        let (graph, _rx) = ComponentGraph::new("test-instance");
        graph.update_sender()
    }

    #[tokio::test]
    async fn test_source_runtime_context_creation() {
        let state_store = Arc::new(MemoryStateStoreProvider::new());
        let update_tx = test_update_tx();

        let context = SourceRuntimeContext::new(
            "test-instance",
            "test-source",
            Some(state_store),
            update_tx,
            None,
        );

        assert_eq!(context.instance_id(), "test-instance");
        assert_eq!(context.source_id(), "test-source");
        assert!(context.state_store().is_some());
    }

    #[tokio::test]
    async fn test_source_runtime_context_without_state_store() {
        let update_tx = test_update_tx();

        let context =
            SourceRuntimeContext::new("test-instance", "test-source", None, update_tx, None);

        assert_eq!(context.source_id(), "test-source");
        assert!(context.state_store().is_none());
    }

    #[tokio::test]
    async fn test_source_runtime_context_clone() {
        let state_store = Arc::new(MemoryStateStoreProvider::new());
        let update_tx = test_update_tx();

        let context = SourceRuntimeContext::new(
            "test-instance",
            "test-source",
            Some(state_store),
            update_tx,
            None,
        );

        let cloned = context.clone();
        assert_eq!(cloned.source_id(), context.source_id());
    }

    #[tokio::test]
    async fn test_reaction_runtime_context_creation() {
        let state_store = Arc::new(MemoryStateStoreProvider::new());
        let query_provider = Arc::new(MockQueryProvider);
        let update_tx = test_update_tx();

        let context = ReactionRuntimeContext::new(
            "test-instance",
            "test-reaction",
            Some(state_store),
            query_provider,
            update_tx,
            None,
        );

        assert_eq!(context.instance_id(), "test-instance");
        assert_eq!(context.reaction_id(), "test-reaction");
        assert!(context.state_store().is_some());
    }

    #[tokio::test]
    async fn test_reaction_runtime_context_without_state_store() {
        let query_provider = Arc::new(MockQueryProvider);
        let update_tx = test_update_tx();

        let context = ReactionRuntimeContext::new(
            "test-instance",
            "test-reaction",
            None,
            query_provider,
            update_tx,
            None,
        );

        assert_eq!(context.reaction_id(), "test-reaction");
        assert!(context.state_store().is_none());
    }

    #[tokio::test]
    async fn test_reaction_runtime_context_clone() {
        let state_store = Arc::new(MemoryStateStoreProvider::new());
        let query_provider = Arc::new(MockQueryProvider);
        let update_tx = test_update_tx();

        let context = ReactionRuntimeContext::new(
            "test-instance",
            "test-reaction",
            Some(state_store),
            query_provider,
            update_tx,
            None,
        );

        let cloned = context.clone();
        assert_eq!(cloned.reaction_id(), context.reaction_id());
    }

    #[test]
    fn test_source_runtime_context_debug() {
        let update_tx = test_update_tx();
        let context = SourceRuntimeContext::new("test-instance", "test", None, update_tx, None);
        let debug_str = format!("{context:?}");
        assert!(debug_str.contains("SourceRuntimeContext"));
        assert!(debug_str.contains("test"));
    }

    #[test]
    fn test_reaction_runtime_context_debug() {
        let query_provider = Arc::new(MockQueryProvider);
        let update_tx = test_update_tx();
        let context = ReactionRuntimeContext::new(
            "test-instance",
            "test",
            None,
            query_provider,
            update_tx,
            None,
        );
        let debug_str = format!("{context:?}");
        assert!(debug_str.contains("ReactionRuntimeContext"));
        assert!(debug_str.contains("test"));
    }

    #[tokio::test]
    async fn test_query_runtime_context_creation() {
        let update_tx = test_update_tx();
        let context = QueryRuntimeContext::new("test-instance", "test-query", update_tx);

        assert_eq!(context.instance_id(), "test-instance");
        assert_eq!(context.query_id(), "test-query");
    }

    #[tokio::test]
    async fn test_query_runtime_context_clone() {
        let update_tx = test_update_tx();
        let context = QueryRuntimeContext::new("test-instance", "test-query", update_tx);

        let cloned = context.clone();
        assert_eq!(cloned.query_id(), context.query_id());
        assert_eq!(cloned.instance_id(), context.instance_id());
    }

    #[test]
    fn test_query_runtime_context_debug() {
        let update_tx = test_update_tx();
        let context = QueryRuntimeContext::new("test-instance", "test-query", update_tx);
        let debug_str = format!("{context:?}");
        assert!(debug_str.contains("QueryRuntimeContext"));
        assert!(debug_str.contains("test-query"));
    }
}
