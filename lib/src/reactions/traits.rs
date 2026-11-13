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
//! - `snapshot_fetcher`: Scoped access to graph-owned query results
//!
//! This replaces the previous `inject_*` methods with a cleaner single-call pattern.

use anyhow::Result;
use async_trait::async_trait;

use crate::channels::{ComponentStatus, QueryResult};
use crate::context::ReactionRuntimeContext;
use crate::reactions::bootstrap_context::BootstrapContext;
use crate::recovery::ReactionRecoveryPolicy;

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
/// Query subscriptions are managed by the host (DrasiLib/ReactionManager).
/// After `start()` succeeds, the host subscribes to the reaction's configured queries
/// and forwards results via `enqueue_query_result()`. Reactions should NOT
/// subscribe to queries themselves.
///
/// # Example Implementation
///
/// This compile-checked skeleton leaves connector-specific work to the implementer;
/// it does not supply a processing worker or claim completed destination effects.
///
/// ```no_run
/// use drasi_lib::{ComponentId, Reaction};
/// use drasi_lib::config::ComponentReference;
/// use drasi_lib::reactions::{ReactionBase, ReactionBaseParams};
/// use drasi_lib::context::ReactionRuntimeContext;
/// # use anyhow::Result;
/// # use async_trait::async_trait;
/// # use drasi_lib::channels::{ComponentStatus, QueryResult};
/// # pub struct MyReactionConfig {
/// #     id: ComponentId,
/// #     queries: Vec<ComponentReference>,
/// #     queue_capacity: usize,
/// # }
///
/// pub struct MyReaction {
///     base: ReactionBase,
///     // Plugin-specific fields
/// }
///
/// impl MyReaction {
///     pub fn new(config: MyReactionConfig) -> Self {
///         let params = ReactionBaseParams::new(config.id, config.queries.clone())
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
///     fn id(&self) -> &ComponentId {
///         &self.base.id
///     }
///
///     fn type_name(&self) -> &str {
///         "my-reaction"
///     }
///
///     fn query_inputs(&self) -> Vec<ComponentReference> {
///         self.base.queries.clone()
///     }
///
///     async fn bind_runtime_context(&self, context: ReactionRuntimeContext) {
///         self.base.bind_runtime_context(context).await;
///     }
///
///     async fn start(&self) -> Result<()> {
///         // Start processing loop — host handles query subscriptions
///         // after start() returns successfully
///         todo!("Start and retain the reaction's processing worker")
///     }
///
///     async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
///         self.base.enqueue_query_result(result).await
///     }
///
///     // ... implement other methods
/// #     fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
/// #         todo!("Return every declared configuration property")
/// #     }
/// #     async fn stop(&self) -> Result<()> { todo!("Await connector cleanup") }
/// #     async fn status(&self) -> ComponentStatus { self.base.get_status().await }
/// }
/// ```
#[async_trait]
pub trait Reaction: Send + Sync {
    /// Get the reaction's unique identifier
    fn id(&self) -> &crate::ComponentId;

    /// Get the reaction type name (e.g., "http", "log", "sse")
    fn type_name(&self) -> &str;

    /// Return **all** configuration properties for this reaction, including secrets.
    ///
    /// # Persistence contract
    ///
    /// This method is the **serialization hook** used by the host to persist
    /// configuration to disk. When the server saves its config file it calls
    /// `snapshot_configuration()`, which in turn calls `properties()` on every
    /// reaction. The returned map is written to the YAML config so the component
    /// can be recreated on the next startup.
    ///
    /// Because there is no separate config cache — the live component is the
    /// single source of truth — any key/value omitted here will be **lost** on
    /// the next save and the component will fail to start after a restart.
    ///
    /// # ⚠ Do not filter secrets
    ///
    /// Implementations **must** include sensitive values (passwords, tokens,
    /// connection strings, etc.). Removing them makes the persistence round-trip
    /// lossy and breaks restart. The host is responsible for protecting the
    /// config file on disk; this method is not an external-facing API.
    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value>;

    /// Get explicit authoring references for the queries this reaction subscribes to.
    ///
    /// DrasiLib resolves these references once when the reaction declaration is
    /// admitted. Subsequent rename or name reuse cannot retarget the running
    /// reaction.
    fn query_inputs(&self) -> Vec<crate::config::ComponentReference>;

    /// Whether this reaction should auto-start when DrasiLib starts
    ///
    /// Default is `true` to match query behavior. Override to return `false`
    /// if this reaction should only be started manually via `start_reaction()`.
    fn auto_start(&self) -> bool {
        true
    }

    /// Bind mandatory framework services before optional initialization.
    ///
    /// This method is called automatically by DrasiLib when the reaction is added
    /// via `add_reaction()`. Plugin developers do not need to call this directly.
    ///
    /// The context provides access to:
    /// - `reaction_id`: The reaction's unique identifier
    /// - `event_tx`: Channel for reporting component lifecycle events
    /// - `state_store`: Optional persistent state storage
    ///
    /// Implementations using ReactionBase delegate to `bind_runtime_context`.
    /// This required hook is separate from the optional no-op initializer.
    async fn bind_runtime_context(&self, context: ReactionRuntimeContext);

    /// Optional instance-lifetime initialization after mandatory context binding.
    async fn initialize(&self) -> Result<()> {
        Ok(())
    }

    /// Final instance-lifetime release, including after failed initialization.
    /// This does not delete persistent state or external artifacts.
    async fn dispose(&self) -> Result<()> {
        Ok(())
    }

    /// Pause independently owned processing at a host-established data boundary.
    async fn pause_processing(&self) -> Result<()> {
        Ok(())
    }

    /// Resume a temporary pause without starting a new activation.
    async fn resume_processing(&self) -> Result<()> {
        Ok(())
    }

    /// Execute a registered factory's accepted retained-object update.
    async fn apply_update(
        &self,
        _context: crate::management::OrdinaryComponentUpdateContext,
    ) -> Result<()> {
        anyhow::bail!("this reaction does not support retained-object updates")
    }

    /// Start the reaction
    ///
    /// The reaction should:
    /// 1. Start its processing loop
    /// 2. Update its status to Running
    ///
    /// Query subscriptions are managed by the host after start() returns.
    /// Results are forwarded via `enqueue_query_result()`.
    async fn start(&self) -> Result<()> {
        Ok(())
    }

    /// Stop the reaction, cleaning up all subscriptions and tasks
    async fn stop(&self) -> Result<()> {
        Ok(())
    }

    /// Get the current status of the reaction
    async fn status(&self) -> ComponentStatus;

    /// Enqueue a query result for processing.
    ///
    /// The host calls this to forward query results to the reaction after
    /// subscribing on its behalf. The default implementation is a no-op.
    /// Most reactions should delegate to `self.base.enqueue_query_result(result)`.
    async fn enqueue_query_result(&self, _result: QueryResult) -> Result<()> {
        Ok(())
    }

    /// Permanently clean up internal state when the reaction is being removed.
    ///
    /// This is called when `remove_reaction(id, cleanup: true)` is used.
    /// Use this to release external resources that should not persist after
    /// the reaction is deleted (e.g., delete output topics, remove external subscriptions).
    ///
    /// Support is explicit. Failure retains the cleanup owner and prevents
    /// final release; ordinary disposal never invokes this destructive hook.
    async fn delete_owned_resources(&self) -> Result<()> {
        anyhow::bail!("this reaction does not support deleting owned resources")
    }

    /// Set the identity provider for this reaction.
    ///
    /// This method allows attaching a per-reaction identity provider after
    /// construction (e.g. when wiring up a reaction from declarative config
    /// that references a named identity provider). It is optional — reactions
    /// that do not authenticate to external systems can ignore it.
    ///
    /// Identity providers set via this method take precedence over any
    /// instance-wide provider injected through the runtime context during
    /// `initialize()`.
    ///
    /// Implementations backed by a [`ReactionBase`](crate::reactions::ReactionBase)
    /// should delegate to `self.base.set_identity_provider(provider).await`;
    /// other implementors should store the provider and apply it during
    /// `initialize()`.
    async fn set_identity_provider(
        &self,
        _provider: std::sync::Arc<dyn crate::identity::IdentityProvider>,
    ) {
        // Default implementation does nothing - reactions that consume an
        // identity provider should override this to delegate to their
        // ReactionBase.
    }

    /// Whether this reaction requires a durable (persistent) state store.
    ///
    /// Reactions that checkpoint their outbox position should return `true`.
    /// The host validates at startup that a durable `StateStoreProvider` is
    /// configured when this returns `true`.
    ///
    /// Default: `false` (volatile state store is acceptable).
    fn is_durable(&self) -> bool {
        false
    }

    /// Whether this reaction needs a full snapshot on first start (no prior checkpoint).
    ///
    /// If `true`, the host calls `bootstrap()` with a `BootstrapContext` providing
    /// `fetch_snapshot()` the first time the reaction starts with no existing checkpoint.
    ///
    /// Default: `false`.
    fn needs_snapshot_on_fresh_start(&self) -> bool {
        false
    }

    /// The default recovery policy for this reaction.
    ///
    /// Can be overridden per-reaction instance via `ReactionBaseParams::recovery_policy`.
    ///
    /// Default: `ReactionRecoveryPolicy::Strict`.
    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::Strict
    }

    /// Called by the host during startup when bootstrap or recovery is needed.
    ///
    /// The `BootstrapContext` provides access to the query's `fetch_snapshot()`
    /// and `fetch_outbox()` APIs, as well as checkpoint read/write helpers.
    ///
    /// Default: no-op.
    async fn bootstrap(&self, _ctx: BootstrapContext) -> Result<()> {
        Ok(())
    }
}

/// Blanket implementation of Reaction for `Box<dyn Reaction>`
///
/// This allows boxed trait objects to be used with methods expecting `impl Reaction`.
#[async_trait]
impl Reaction for Box<dyn Reaction + 'static> {
    fn id(&self) -> &crate::ComponentId {
        (**self).id()
    }

    fn type_name(&self) -> &str {
        (**self).type_name()
    }

    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        (**self).properties()
    }

    fn query_inputs(&self) -> Vec<crate::config::ComponentReference> {
        (**self).query_inputs()
    }

    fn auto_start(&self) -> bool {
        (**self).auto_start()
    }

    async fn bind_runtime_context(&self, context: ReactionRuntimeContext) {
        (**self).bind_runtime_context(context).await
    }

    async fn initialize(&self) -> Result<()> {
        (**self).initialize().await
    }

    async fn dispose(&self) -> Result<()> {
        (**self).dispose().await
    }

    async fn pause_processing(&self) -> Result<()> {
        (**self).pause_processing().await
    }

    async fn resume_processing(&self) -> Result<()> {
        (**self).resume_processing().await
    }

    async fn apply_update(
        &self,
        context: crate::management::OrdinaryComponentUpdateContext,
    ) -> Result<()> {
        (**self).apply_update(context).await
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

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        (**self).enqueue_query_result(result).await
    }

    async fn delete_owned_resources(&self) -> Result<()> {
        (**self).delete_owned_resources().await
    }

    async fn set_identity_provider(
        &self,
        provider: std::sync::Arc<dyn crate::identity::IdentityProvider>,
    ) {
        (**self).set_identity_provider(provider).await
    }

    fn is_durable(&self) -> bool {
        (**self).is_durable()
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        (**self).needs_snapshot_on_fresh_start()
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        (**self).default_recovery_policy()
    }

    async fn bootstrap(&self, ctx: BootstrapContext) -> Result<()> {
        (**self).bootstrap(ctx).await
    }
}
