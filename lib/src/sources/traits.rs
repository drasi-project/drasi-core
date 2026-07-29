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

//! Source trait module
//!
//! This module provides the core trait that all source plugins must implement.
//! It separates the plugin contract from the source manager and implementation details.
//!
//! # Plugin Architecture
//!
//! Each source plugin:
//! 1. Defines its own typed configuration struct with builder pattern
//! 2. Creates a `SourceBase` instance using `SourceBaseParams`
//! 3. Implements the `Source` trait
//! 4. Is passed to `DrasiLib` via `add_source()` which takes ownership
//!
//! drasi-lib has no knowledge of which plugins exist - it only knows about this trait.
//!
//! # Runtime Context Initialization
//!
//! Sources receive all drasi-lib services through a single `initialize()` call
//! when added to DrasiLib. The `SourceRuntimeContext` provides:
//! - `event_tx`: Channel for component lifecycle events
//! - `state_store`: Optional persistent state storage
//!
//! This replaces the previous `inject_*` methods with a cleaner single-call pattern.

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use thiserror::Error;

use crate::channels::*;
use crate::context::SourceRuntimeContext;

/// Structured error type for source operations.
///
/// Sources return these errors (via `anyhow::Error`) from trait methods like `subscribe()`.
/// The orchestration layer can downcast to check for specific error variants:
/// ```ignore
/// if let Some(source_err) = err.downcast_ref::<SourceError>() {
///     match source_err {
///         SourceError::PositionUnavailable { .. } => { /* handle */ }
///     }
/// }
/// ```
#[derive(Error, Debug)]
pub enum SourceError {
    /// The requested resume position is no longer available.
    /// The caller should consult its recovery_policy to decide next steps.
    #[error("Source '{source_id}' cannot resume from requested position: position unavailable (earliest available: {earliest_available:?})")]
    PositionUnavailable {
        source_id: String,
        /// The position that was requested (opaque bytes from a previous checkpoint)
        requested: Bytes,
        /// The earliest position the source can provide, if known
        earliest_available: Option<Bytes>,
    },
}

/// Trait for comparing source positions during per-subscriber replay filtering.
///
/// When a source rewinds to serve a late-joining subscriber, events that a
/// subscriber has already committed must be suppressed. Since `source_position`
/// is opaque bytes, the framework cannot compare them directly — the source
/// must provide the comparison logic.
///
/// # Contract
///
/// `position_reached(event_pos, resume_pos)` returns `true` when the event at
/// `event_pos` is **strictly after** `resume_pos` (i.e., it is new for a
/// subscriber that last committed at `resume_pos`). Events at or before the
/// resume position are filtered out.
///
/// # Default
///
/// [`ByteLexPositionComparator`] provides a byte-lexicographic comparison that
/// works for any position encoded in big-endian (e.g., Postgres u64 LSN,
/// MSSQL 20-byte LSN).
pub trait PositionComparator: Send + Sync {
    /// Returns `true` if `event_pos` is strictly after `resume_pos`.
    fn position_reached(&self, event_pos: &Bytes, resume_pos: &Bytes) -> bool;
}

/// Default byte-lexicographic position comparator.
///
/// Works correctly for any position encoded as big-endian bytes of equal length.
/// For positions of unequal length, shorter bytes compare as less than longer ones
/// with the same prefix.
#[derive(Debug, Clone, Default)]
pub struct ByteLexPositionComparator;

impl PositionComparator for ByteLexPositionComparator {
    fn position_reached(&self, event_pos: &Bytes, resume_pos: &Bytes) -> bool {
        event_pos.as_ref() > resume_pos.as_ref()
    }
}

/// Trait defining the interface for all source implementations.
///
/// This is the core abstraction that all source plugins must implement.
/// drasi-lib only interacts with sources through this trait - it has no
/// knowledge of specific plugin types or their configurations.
///
/// # Example Implementation
///
/// ```ignore
/// use drasi_lib::Source;
/// use drasi_lib::sources::{SourceBase, SourceBaseParams};
/// use drasi_lib::context::SourceRuntimeContext;
///
/// pub struct MySource {
///     base: SourceBase,
///     // Plugin-specific fields
/// }
///
/// impl MySource {
///     pub fn new(config: MySourceConfig) -> Result<Self> {
///         let params = SourceBaseParams::new(&config.id)
///             .with_dispatch_mode(config.dispatch_mode)
///             .with_dispatch_buffer_capacity(config.buffer_capacity);
///
///         Ok(Self {
///             base: SourceBase::new(params)?,
///         })
///     }
/// }
///
/// #[async_trait]
/// impl Source for MySource {
///     fn id(&self) -> &str {
///         &self.base.id
///     }
///
///     fn type_name(&self) -> &str {
///         "my-source"
///     }
///
///     fn properties(&self) -> HashMap<String, Value> {
///         // Return plugin-specific properties
///     }
///
///     async fn initialize(&self, context: SourceRuntimeContext) {
///         self.base.initialize(context).await;
///     }
///
///     // ... implement other methods
/// }
/// ```
#[async_trait]
pub trait Source: Send + Sync {
    /// Get the source's unique identifier
    fn id(&self) -> &str;

    /// Get the source type name (e.g., "postgres", "http", "mock")
    fn type_name(&self) -> &str;

    /// Return **all** configuration properties for this source, including secrets.
    ///
    /// # Persistence contract
    ///
    /// This method is the **serialization hook** used by the host to persist
    /// configuration to disk. When the server saves its config file it calls
    /// `snapshot_configuration()`, which in turn calls `properties()` on every
    /// source. The returned map is written to the YAML config so the component
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

    /// Get the dispatch mode for this source (Channel or Broadcast)
    ///
    /// Default is Channel mode for backpressure support.
    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }

    /// Whether this source should auto-start when DrasiLib starts
    ///
    /// Default is `true`. Override to return `false` if this source
    /// should only be started manually via `start_source()`.
    fn auto_start(&self) -> bool {
        true
    }

    /// Whether this source supports positional replay via `resume_from`.
    ///
    /// Sources backed by a persistent log (e.g., Postgres WAL, Kafka) return
    /// `true` (the default). Volatile sources that cannot replay (e.g.,
    /// in-memory-only or purely push-based) should override this to return
    /// `false`. The orchestration layer uses this to validate compatibility
    /// with persistent queries and to request position handles.
    fn supports_replay(&self) -> bool {
        true
    }

    /// Describe the graph schema this source provides, if known.
    ///
    /// This is a best-effort introspection hook used by inspection APIs and
    /// future MCP adapters. Sources that cannot determine their schema should
    /// return `None`.
    fn describe_schema(&self) -> Option<crate::schema::SourceSchema> {
        None
    }

    /// Start the source
    ///
    /// This begins data ingestion and event generation.
    async fn start(&self) -> Result<()>;

    /// Stop the source
    ///
    /// This stops data ingestion and cleans up resources.
    async fn stop(&self) -> Result<()>;

    /// Get the current status of the source
    async fn status(&self) -> ComponentStatus;

    /// Subscribe to this source for change events.
    ///
    /// This is called by queries to receive data changes from this source.
    /// The source should return a receiver for streaming events and optionally
    /// a bootstrap receiver for initial data.
    ///
    /// # Important
    /// Implementations that maintain a sequence counter must recover its
    /// monotonicity after a restart from their durable position (e.g. the WAL
    /// head via [`SourceBase::set_next_sequence`](crate::sources::base::SourceBase::set_next_sequence),
    /// or the native `resume_from` position). Delegating to
    /// [`SourceBase::subscribe_with_replay()`](crate::sources::base::SourceBase::subscribe_with_replay)
    /// or [`SourceBase::subscribe_with_bootstrap()`](crate::sources::base::SourceBase::subscribe_with_bootstrap)
    /// handles the base-level bookkeeping automatically.
    ///
    /// # Arguments
    /// * `settings` - Subscription settings including query ID, text, and labels of interest
    ///
    /// # Returns
    /// A SubscriptionResponse containing:
    /// * A receiver for streaming source events
    /// * Optionally a bootstrap receiver for initial data
    async fn subscribe(
        &self,
        settings: crate::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse>;

    /// The IDs of continuous queries whose results feed this source.
    ///
    /// Most sources ingest data from an external system and return an empty
    /// list (the default). Sources that instead derive their graph from the
    /// results of one or more continuous queries — e.g. a state machine that
    /// maps query results to entity-state nodes — return those query IDs here.
    ///
    /// After the source starts and its subscribed queries are running, the host
    /// subscribes on the source's behalf and forwards each result via
    /// [`enqueue_query_result`](Source::enqueue_query_result). This mirrors the
    /// query-subscription model used by reactions.
    fn subscribed_query_ids(&self) -> Vec<String> {
        Vec::new()
    }

    /// Enqueue a query result for processing.
    ///
    /// The host calls this to forward query results to the source after
    /// subscribing on its behalf (see [`subscribed_query_ids`](Source::subscribed_query_ids)).
    /// The default implementation is a no-op. Sources that consume query results
    /// should delegate to
    /// [`SourceBase::enqueue_query_result`](crate::sources::base::SourceBase::enqueue_query_result).
    async fn enqueue_query_result(&self, _result: QueryResult) -> Result<()> {
        Ok(())
    }

    /// Downcast helper for testing - allows access to concrete types
    fn as_any(&self) -> &dyn std::any::Any;

    /// Permanently clean up internal state when the source is being removed.
    ///
    /// This is called when `remove_source(id, cleanup: true)` is used.
    /// Use this to release external resources that should not persist after
    /// the source is deleted (e.g., drop a replication slot, remove cursors).
    ///
    /// The default implementation is a no-op. Override only if your source
    /// manages external state that needs explicit teardown.
    ///
    /// Errors are logged but do not prevent the source from being removed.
    async fn deprovision(&self) -> Result<()> {
        Ok(())
    }

    /// Initialize the source with runtime context.
    ///
    /// This method is called automatically by DrasiLib when the source is added
    /// via `add_source()`. Plugin developers do not need to call this directly.
    ///
    /// The context provides access to:
    /// - `source_id`: The source's unique identifier
    /// - `event_tx`: Channel for reporting component lifecycle events
    /// - `state_store`: Optional persistent state storage
    ///
    /// Implementation should delegate to `self.base.initialize(context).await`.
    async fn initialize(&self, context: SourceRuntimeContext);

    /// Set the bootstrap provider for this source
    ///
    /// This method allows setting a bootstrap provider after source construction.
    /// It is optional - sources without a bootstrap provider will report that
    /// bootstrap is not available.
    ///
    /// Implementation should delegate to `self.base.set_bootstrap_provider(provider).await`.
    async fn set_bootstrap_provider(
        &self,
        _provider: Box<dyn crate::bootstrap::BootstrapProvider + 'static>,
    ) {
        // Default implementation does nothing - sources that support bootstrap
        // should override this to delegate to their SourceBase
    }

    /// Release the position handle that a query was holding on this source.
    ///
    /// Called during query stop to let the source advance its min-watermark.
    /// Sources that use position handles should delegate to
    /// `self.base.remove_position_handle(query_id).await`.
    ///
    /// The default is a no-op for sources that do not manage position handles.
    ///
    /// **Note:** This method has no FFI vtable entry yet. `SourceProxy`
    /// overrides it with an explicit no-op + log message, and overrides
    /// `supports_replay()` to return `false` so the orchestration layer
    /// does not expect plugin sources to support position handles. A future
    /// FFI SDK update (see issue #371) will add the vtable entry so plugin
    /// sources can participate in position-handle cleanup.
    async fn remove_position_handle(&self, _query_id: &str) {}

    /// Signal that the initial batch of query subscriptions is complete.
    ///
    /// Called by the lifecycle after all auto-start queries have subscribed
    /// during startup. Sources that hold back feedback during the subscription
    /// window (e.g., Postgres flush-fence) should release those guards here.
    ///
    /// The default is a no-op for sources that do not need startup fencing.
    async fn on_subscriptions_complete(&self) {}
    /// Set the identity provider for this source.
    ///
    /// This method allows attaching a per-source identity provider after
    /// construction (e.g. when wiring up a source from declarative config that
    /// references a named identity provider). It is optional — sources that do
    /// not authenticate to external systems can ignore it.
    ///
    /// Identity providers set via this method take precedence over any
    /// instance-wide provider injected through the runtime context during
    /// `initialize()`.
    ///
    /// Implementations backed by a [`SourceBase`](crate::sources::SourceBase)
    /// should delegate to `self.base.set_identity_provider(provider).await`;
    /// other implementors should store the provider and apply it during
    /// `initialize()`.
    async fn set_identity_provider(
        &self,
        _provider: std::sync::Arc<dyn crate::identity::IdentityProvider>,
    ) {
        // Default implementation does nothing - sources that consume an
        // identity provider should override this to delegate to their SourceBase.
    }
}

/// Blanket implementation of Source for `Box<dyn Source>`
///
/// This allows boxed trait objects to be used with methods expecting `impl Source`.
#[async_trait]
impl Source for Box<dyn Source + 'static> {
    fn id(&self) -> &str {
        (**self).id()
    }

    fn type_name(&self) -> &str {
        (**self).type_name()
    }

    fn properties(&self) -> std::collections::HashMap<String, serde_json::Value> {
        (**self).properties()
    }

    fn dispatch_mode(&self) -> DispatchMode {
        (**self).dispatch_mode()
    }

    fn auto_start(&self) -> bool {
        (**self).auto_start()
    }

    fn describe_schema(&self) -> Option<crate::schema::SourceSchema> {
        (**self).describe_schema()
    }

    fn supports_replay(&self) -> bool {
        (**self).supports_replay()
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

    async fn subscribe(
        &self,
        settings: crate::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        (**self).subscribe(settings).await
    }

    fn subscribed_query_ids(&self) -> Vec<String> {
        (**self).subscribed_query_ids()
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        (**self).enqueue_query_result(result).await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        (**self).as_any()
    }

    async fn deprovision(&self) -> Result<()> {
        (**self).deprovision().await
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        (**self).initialize(context).await
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn crate::bootstrap::BootstrapProvider + 'static>,
    ) {
        (**self).set_bootstrap_provider(provider).await
    }

    async fn remove_position_handle(&self, query_id: &str) {
        (**self).remove_position_handle(query_id).await
    }

    async fn on_subscriptions_complete(&self) {
        (**self).on_subscriptions_complete().await
    }

    async fn set_identity_provider(
        &self,
        provider: std::sync::Arc<dyn crate::identity::IdentityProvider>,
    ) {
        (**self).set_identity_provider(provider).await
    }
}
