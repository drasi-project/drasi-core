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
use crate::identity::IdentityProvider;
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
/// - `status_tx`: Channel for reporting component status/lifecycle events
/// - `state_store`: Optional persistent state storage (if configured)
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
    /// * `status_tx` - Channel for reporting component status/lifecycle events
    /// * `state_store` - Optional persistent state storage
    pub fn new(
        instance_id: impl Into<String>,
        source_id: impl Into<String>,
        status_tx: ComponentEventSender,
        state_store: Option<Arc<dyn StateStoreProvider>>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            source_id: source_id.into(),
            status_tx,
            state_store,
            identity_provider: None,
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
            .field("instance_id", &self.instance_id)
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
/// - `instance_id`: The DrasiLib instance ID (for log routing isolation)
/// - `reaction_id`: The unique identifier for this reaction instance
/// - `status_tx`: Channel for reporting component status/lifecycle events
/// - `state_store`: Optional persistent state storage (if configured)
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
    /// * `status_tx` - Channel for reporting component status/lifecycle events
    /// * `state_store` - Optional persistent state storage
    /// * `query_provider` - Access to query instances for subscription
    pub fn new(
        instance_id: impl Into<String>,
        reaction_id: impl Into<String>,
        status_tx: ComponentEventSender,
        state_store: Option<Arc<dyn StateStoreProvider>>,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            reaction_id: reaction_id.into(),
            status_tx,
            state_store,
            identity_provider: None,
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

    /// Get a reference to the status channel.
    pub fn status_tx(&self) -> &ComponentEventSender {
        &self.status_tx
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
            .field("status_tx", &"<ComponentEventSender>")
            .field(
                "state_store",
                &self.state_store.as_ref().map(|_| "<StateStoreProvider>"),
            )
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_store::MemoryStateStoreProvider;
    use std::sync::Arc;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn test_source_runtime_context_creation() {
        let (status_tx, _rx) = mpsc::channel(100);
        let state_store = Arc::new(MemoryStateStoreProvider::new());

        let context =
            SourceRuntimeContext::new("test-instance", "test-source", status_tx, Some(state_store));

        assert_eq!(context.instance_id(), "test-instance");
        assert_eq!(context.source_id(), "test-source");
        assert!(context.state_store().is_some());
    }

    #[tokio::test]
    async fn test_source_runtime_context_without_state_store() {
        let (status_tx, _rx) = mpsc::channel(100);

        let context = SourceRuntimeContext::new("test-instance", "test-source", status_tx, None);

        assert_eq!(context.source_id(), "test-source");
        assert!(context.state_store().is_none());
    }

    #[tokio::test]
    async fn test_source_runtime_context_clone() {
        let (status_tx, _rx) = mpsc::channel(100);
        let state_store = Arc::new(MemoryStateStoreProvider::new());

        let context =
            SourceRuntimeContext::new("test-instance", "test-source", status_tx, Some(state_store));

        let cloned = context.clone();
        assert_eq!(cloned.source_id(), context.source_id());
    }

    #[tokio::test]
    async fn test_reaction_runtime_context_creation() {
        let (status_tx, _rx) = mpsc::channel(100);
        let state_store = Arc::new(MemoryStateStoreProvider::new());

        let context = ReactionRuntimeContext::new(
            "test-instance",
            "test-reaction",
            status_tx,
            Some(state_store),
        );

        assert_eq!(context.instance_id(), "test-instance");
        assert_eq!(context.reaction_id(), "test-reaction");
        assert!(context.state_store().is_some());
    }

    #[tokio::test]
    async fn test_reaction_runtime_context_without_state_store() {
        let (status_tx, _rx) = mpsc::channel(100);

        let context = ReactionRuntimeContext::new(
            "test-instance",
            "test-reaction",
            status_tx,
            None,
        );

        assert_eq!(context.reaction_id(), "test-reaction");
        assert!(context.state_store().is_none());
    }

    #[tokio::test]
    async fn test_reaction_runtime_context_clone() {
        let (status_tx, _rx) = mpsc::channel(100);
        let state_store = Arc::new(MemoryStateStoreProvider::new());

        let context = ReactionRuntimeContext::new(
            "test-instance",
            "test-reaction",
            status_tx,
            Some(state_store),
        );

        let cloned = context.clone();
        assert_eq!(cloned.reaction_id(), context.reaction_id());
    }

    #[test]
    fn test_source_runtime_context_debug() {
        let (status_tx, _rx) = mpsc::channel::<crate::channels::ComponentEvent>(100);
        let context = SourceRuntimeContext::new("test-instance", "test", status_tx, None);
        let debug_str = format!("{context:?}");
        assert!(debug_str.contains("SourceRuntimeContext"));
        assert!(debug_str.contains("test"));
    }

    #[test]
    fn test_reaction_runtime_context_debug() {
        let (status_tx, _rx) = mpsc::channel::<crate::channels::ComponentEvent>(100);
        let context =
            ReactionRuntimeContext::new("test-instance", "test", status_tx, None);
        let debug_str = format!("{context:?}");
        assert!(debug_str.contains("ReactionRuntimeContext"));
        assert!(debug_str.contains("test"));
    }
}
