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

use crate::channels::ComponentEventSender;
use crate::reactions::QueryProvider;
use crate::state_store::StateStoreProvider;

/// Context provided to Source plugins during initialization.
///
/// Contains `Arc<T>` instances for all drasi-lib provided services.
/// DrasiLib constructs this context when a source is added via `add_source()`.
///
/// # Available Services
///
/// - `source_id`: The unique identifier for this source instance
/// - `status_tx`: Channel for reporting component status/lifecycle events
/// - `state_store`: Optional persistent state storage (if configured)
///
/// # Clone
///
/// This struct implements `Clone` and all fields use `Arc` internally,
/// making cloning cheap (just reference count increments).
#[derive(Clone)]
pub struct SourceRuntimeContext {
    /// Unique identifier for this source instance
    pub source_id: String,

    /// Channel for reporting component status/lifecycle events.
    ///
    /// Use this to send status updates (Starting, Running, Stopped, Error)
    /// back to DrasiLib for monitoring and lifecycle management.
    pub status_tx: ComponentEventSender,

    /// Optional persistent state storage.
    ///
    /// This is `Some` if a state store provider was configured on DrasiLib,
    /// otherwise `None`. Sources can use this to persist state across restarts.
    pub state_store: Option<Arc<dyn StateStoreProvider>>,
}

impl SourceRuntimeContext {
    /// Create a new source runtime context.
    ///
    /// This is typically called by `SourceManager` when adding a source to DrasiLib.
    /// Plugin developers do not need to call this directly.
    ///
    /// # Arguments
    ///
    /// * `source_id` - The unique identifier for this source
    /// * `status_tx` - Channel for reporting component status/lifecycle events
    /// * `state_store` - Optional persistent state storage
    pub fn new(
        source_id: impl Into<String>,
        status_tx: ComponentEventSender,
        state_store: Option<Arc<dyn StateStoreProvider>>,
    ) -> Self {
        Self {
            source_id: source_id.into(),
            status_tx,
            state_store,
        }
    }

    /// Get the source's unique identifier.
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// Get a reference to the status channel.
    ///
    /// Use this to send component status updates (Starting, Running, Stopped, Error)
    /// back to DrasiLib.
    pub fn status_tx(&self) -> &ComponentEventSender {
        &self.status_tx
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
            .field("source_id", &self.source_id)
            .field("status_tx", &"<ComponentEventSender>")
            .field(
                "state_store",
                &self.state_store.as_ref().map(|_| "<StateStoreProvider>"),
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
/// - `reaction_id`: The unique identifier for this reaction instance
/// - `status_tx`: Channel for reporting component status/lifecycle events
/// - `state_store`: Optional persistent state storage (if configured)
/// - `query_provider`: Access to query instances for subscription
///
/// # Clone
///
/// This struct implements `Clone` and all fields use `Arc` internally,
/// making cloning cheap (just reference count increments).
#[derive(Clone)]
pub struct ReactionRuntimeContext {
    /// Unique identifier for this reaction instance
    pub reaction_id: String,

    /// Channel for reporting component status/lifecycle events.
    ///
    /// Use this to send status updates (Starting, Running, Stopped, Error)
    /// back to DrasiLib for monitoring and lifecycle management.
    pub status_tx: ComponentEventSender,

    /// Optional persistent state storage.
    ///
    /// This is `Some` if a state store provider was configured on DrasiLib,
    /// otherwise `None`. Reactions can use this to persist state across restarts.
    pub state_store: Option<Arc<dyn StateStoreProvider>>,

    /// Access to query instances for subscription.
    ///
    /// Reactions use this to get query instances and subscribe to their results.
    /// This is always available (not optional) since reactions require queries.
    pub query_provider: Arc<dyn QueryProvider>,
}

impl ReactionRuntimeContext {
    /// Create a new reaction runtime context.
    ///
    /// This is typically called by `ReactionManager` when adding a reaction to DrasiLib.
    /// Plugin developers do not need to call this directly.
    ///
    /// # Arguments
    ///
    /// * `reaction_id` - The unique identifier for this reaction
    /// * `status_tx` - Channel for reporting component status/lifecycle events
    /// * `state_store` - Optional persistent state storage
    /// * `query_provider` - Access to query instances for subscription
    pub fn new(
        reaction_id: impl Into<String>,
        status_tx: ComponentEventSender,
        state_store: Option<Arc<dyn StateStoreProvider>>,
        query_provider: Arc<dyn QueryProvider>,
    ) -> Self {
        Self {
            reaction_id: reaction_id.into(),
            status_tx,
            state_store,
            query_provider,
        }
    }

    /// Get the reaction's unique identifier.
    pub fn reaction_id(&self) -> &str {
        &self.reaction_id
    }

    /// Get a reference to the status channel.
    ///
    /// Use this to send component status updates (Starting, Running, Stopped, Error)
    /// back to DrasiLib.
    pub fn status_tx(&self) -> &ComponentEventSender {
        &self.status_tx
    }

    /// Get a reference to the state store if configured.
    ///
    /// Returns `Some(&Arc<dyn StateStoreProvider>)` if a state store was configured,
    /// otherwise `None`.
    pub fn state_store(&self) -> Option<&Arc<dyn StateStoreProvider>> {
        self.state_store.as_ref()
    }

    /// Get a reference to the query provider.
    ///
    /// Use this to get query instances and subscribe to their results.
    pub fn query_provider(&self) -> &Arc<dyn QueryProvider> {
        &self.query_provider
    }
}

impl std::fmt::Debug for ReactionRuntimeContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ReactionRuntimeContext")
            .field("reaction_id", &self.reaction_id)
            .field("status_tx", &"<ComponentEventSender>")
            .field(
                "state_store",
                &self.state_store.as_ref().map(|_| "<StateStoreProvider>"),
            )
            .field("query_provider", &"<QueryProvider>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queries::Query;
    use crate::state_store::MemoryStateStoreProvider;
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    // Mock QueryProvider for testing
    struct MockQueryProvider;

    #[async_trait]
    impl QueryProvider for MockQueryProvider {
        async fn get_query_instance(&self, _id: &str) -> Result<Arc<dyn Query>> {
            Err(anyhow::anyhow!("MockQueryProvider: query not found"))
        }
    }

    #[tokio::test]
    async fn test_source_runtime_context_creation() {
        let (status_tx, _rx) = mpsc::channel(100);
        let state_store = Arc::new(MemoryStateStoreProvider::new());

        let context = SourceRuntimeContext::new("test-source", status_tx, Some(state_store));

        assert_eq!(context.source_id(), "test-source");
        assert!(context.state_store().is_some());
    }

    #[tokio::test]
    async fn test_source_runtime_context_without_state_store() {
        let (status_tx, _rx) = mpsc::channel(100);

        let context = SourceRuntimeContext::new("test-source", status_tx, None);

        assert_eq!(context.source_id(), "test-source");
        assert!(context.state_store().is_none());
    }

    #[tokio::test]
    async fn test_source_runtime_context_clone() {
        let (status_tx, _rx) = mpsc::channel(100);
        let state_store = Arc::new(MemoryStateStoreProvider::new());

        let context = SourceRuntimeContext::new("test-source", status_tx, Some(state_store));

        let cloned = context.clone();
        assert_eq!(cloned.source_id(), context.source_id());
    }

    #[tokio::test]
    async fn test_reaction_runtime_context_creation() {
        let (status_tx, _rx) = mpsc::channel(100);
        let state_store = Arc::new(MemoryStateStoreProvider::new());
        let query_provider = Arc::new(MockQueryProvider);

        let context = ReactionRuntimeContext::new(
            "test-reaction",
            status_tx,
            Some(state_store),
            query_provider,
        );

        assert_eq!(context.reaction_id(), "test-reaction");
        assert!(context.state_store().is_some());
    }

    #[tokio::test]
    async fn test_reaction_runtime_context_without_state_store() {
        let (status_tx, _rx) = mpsc::channel(100);
        let query_provider = Arc::new(MockQueryProvider);

        let context = ReactionRuntimeContext::new("test-reaction", status_tx, None, query_provider);

        assert_eq!(context.reaction_id(), "test-reaction");
        assert!(context.state_store().is_none());
    }

    #[tokio::test]
    async fn test_reaction_runtime_context_clone() {
        let (status_tx, _rx) = mpsc::channel(100);
        let state_store = Arc::new(MemoryStateStoreProvider::new());
        let query_provider = Arc::new(MockQueryProvider);

        let context = ReactionRuntimeContext::new(
            "test-reaction",
            status_tx,
            Some(state_store),
            query_provider,
        );

        let cloned = context.clone();
        assert_eq!(cloned.reaction_id(), context.reaction_id());
    }

    #[test]
    fn test_source_runtime_context_debug() {
        let (status_tx, _rx) = mpsc::channel::<crate::channels::ComponentEvent>(100);
        let context = SourceRuntimeContext::new("test", status_tx, None);
        let debug_str = format!("{context:?}");
        assert!(debug_str.contains("SourceRuntimeContext"));
        assert!(debug_str.contains("test"));
    }

    #[test]
    fn test_reaction_runtime_context_debug() {
        let (status_tx, _rx) = mpsc::channel::<crate::channels::ComponentEvent>(100);
        let query_provider = Arc::new(MockQueryProvider);
        let context = ReactionRuntimeContext::new("test", status_tx, None, query_provider);
        let debug_str = format!("{context:?}");
        assert!(debug_str.contains("ReactionRuntimeContext"));
        assert!(debug_str.contains("test"));
    }
}
