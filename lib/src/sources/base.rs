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

//! Base implementation for common source functionality.
//!
//! This module provides `SourceBase` which encapsulates common patterns
//! used across all source implementations:
//! - Dispatcher setup and management
//! - Bootstrap subscription handling
//! - Event dispatching with profiling
//! - Component lifecycle management
//!
//! # Plugin Architecture
//!
//! SourceBase is designed to be used by source plugins. Each plugin:
//! 1. Defines its own typed configuration struct
//! 2. Creates a SourceBase with SourceBaseParams
//! 3. Optionally provides a bootstrap provider via `set_bootstrap_provider()`
//! 4. Implements the Source trait delegating to SourceBase methods

use anyhow::Result;
use log::{debug, error, info, warn};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{Notify, RwLock};
use tracing::Instrument;

use crate::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult};
use crate::channels::*;
use crate::component_graph::ComponentStatusHandle;
use crate::context::SourceRuntimeContext;
use crate::identity::IdentityProvider;
use crate::profiling;
use crate::sources::PositionComparator;
use crate::state_store::StateStoreProvider;
use bytes::Bytes;
use drasi_core::models::SourceChange;

/// Parameters for creating a SourceBase instance.
///
/// This struct contains only the information that SourceBase needs to function.
/// Plugin-specific configuration should remain in the plugin crate.
///
/// # Example
///
/// ```ignore
/// use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
///
/// let params = SourceBaseParams::new("my-source")
///     .with_dispatch_mode(DispatchMode::Channel)
///     .with_dispatch_buffer_capacity(2000)
///     .with_bootstrap_provider(my_provider);
///
/// let base = SourceBase::new(params)?;
/// ```
pub struct SourceBaseParams {
    /// Unique identifier for the source
    pub id: String,
    /// Dispatch mode (Broadcast or Channel) - defaults to Channel
    pub dispatch_mode: Option<DispatchMode>,
    /// Dispatch buffer capacity - defaults to 1000
    pub dispatch_buffer_capacity: Option<usize>,
    /// Optional state store provider to set during construction.
    ///
    /// When set, takes precedence over any state store provided via the runtime
    /// context during `initialize()`. This differs from `bootstrap_provider` which
    /// is never overwritten by context — only `state_store` and `identity_provider`
    /// have context-provided fallback behavior.
    pub state_store: Option<Arc<dyn StateStoreProvider>>,
    /// Optional bootstrap provider to set during construction
    pub bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>,
    /// Whether this source should auto-start - defaults to true
    pub auto_start: bool,
}

impl std::fmt::Debug for SourceBaseParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceBaseParams")
            .field("id", &self.id)
            .field("dispatch_mode", &self.dispatch_mode)
            .field("dispatch_buffer_capacity", &self.dispatch_buffer_capacity)
            .field(
                "state_store",
                &self.state_store.as_ref().map(|_| "<StateStoreProvider>"),
            )
            .field(
                "bootstrap_provider",
                &self.bootstrap_provider.as_ref().map(|_| "<provider>"),
            )
            .field("auto_start", &self.auto_start)
            .finish()
    }
}

impl SourceBaseParams {
    /// Create new params with just an ID, using defaults for everything else
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            state_store: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    /// Set the dispatch mode
    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    /// Set the dispatch buffer capacity
    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    /// Set the state store provider
    ///
    /// This is typically used when constructing sources outside of DrasiLib
    /// and you want to provide a persistent store for checkpointing.
    pub fn with_state_store(mut self, store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(store);
        self
    }

    /// Set the bootstrap provider
    ///
    /// This provider will be used during source subscription to deliver
    /// initial data to queries that request bootstrap.
    pub fn with_bootstrap_provider(mut self, provider: impl BootstrapProvider + 'static) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    /// Set whether this source should auto-start
    ///
    /// Default is `true`. Set to `false` if this source should only be
    /// started manually via `start_source()`.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }
}

/// Base implementation for common source functionality
pub struct SourceBase {
    /// Source identifier
    pub id: String,
    /// Dispatch mode setting
    dispatch_mode: DispatchMode,
    /// Dispatch buffer capacity
    dispatch_buffer_capacity: usize,
    /// Whether this source should auto-start
    pub auto_start: bool,
    /// Component status handle — always available, wired to graph during initialize().
    status_handle: ComponentStatusHandle,
    /// Dispatchers for sending source events to subscribers
    ///
    /// This is a vector of dispatchers that send source events to all registered
    /// subscribers (queries). When a source produces a change event, it broadcasts
    /// it to all dispatchers in this vector.
    pub dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    /// Runtime context (set by initialize())
    context: Arc<RwLock<Option<SourceRuntimeContext>>>,
    /// State store provider (extracted from context for convenience)
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    /// Handle to the source's main task
    pub task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Sender for shutdown signal
    pub shutdown_tx: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
    /// Optional bootstrap provider - plugins set this if they support bootstrap
    bootstrap_provider: Arc<RwLock<Option<Arc<dyn BootstrapProvider>>>>,
    /// Optional identity provider for credential management.
    /// Set either programmatically (via `set_identity_provider`) or automatically
    /// from the runtime context during `initialize()`.
    identity_provider: Arc<RwLock<Option<Arc<dyn IdentityProvider>>>>,
    /// Per-query position handles for replay-capable subscribers.
    ///
    /// Keyed by `query_id`. Values are the same `Arc<AtomicU64>` returned in
    /// `SubscriptionResponse::position_handle`. The query writes its last
    /// durably-processed sequence; the source reads `compute_confirmed_position()`
    /// to advance its upstream cursor. Initial value is `u64::MAX`
    /// ("no position confirmed yet"). Only populated when
    /// `request_position_handle == true`.
    position_handles: Arc<RwLock<HashMap<String, Arc<AtomicU64>>>>,
    /// Monotonically increasing counter for assigning event sequences.
    /// The framework stamps every dispatched event with this sequence.
    next_sequence: Arc<AtomicU64>,
    /// Original raw config JSON from the descriptor, preserving ConfigValue
    /// envelopes (secrets, env vars) for lossless persistence roundtrips.
    raw_config: Option<serde_json::Value>,
    /// Notified whenever a new subscriber registers via `create_streaming_receiver`.
    /// Sources can await `wait_for_subscribers()` before starting their polling
    /// loop to avoid dispatching events before any subscriber exists.
    subscriber_notify: Arc<Notify>,
    /// Per-subscriber resume positions for replay filtering.
    ///
    /// Keyed by dispatcher index in the `dispatchers` Vec. When an event's
    /// `source_position` has not yet passed the resume position, the event is
    /// not delivered to that subscriber's dispatcher. Once `position_reached()`
    /// returns true, the entry is removed and all subsequent events flow through.
    ///
    /// Only populated in Channel dispatch mode (Broadcast cannot filter per-subscriber).
    subscriber_resume_positions: Arc<RwLock<HashMap<usize, Bytes>>>,
    /// Optional position comparator for per-subscriber replay filtering.
    ///
    /// Set by sources that support replay. Without a comparator, position
    /// filtering is disabled (all events are delivered to all subscribers).
    position_comparator: Arc<RwLock<Option<Arc<dyn PositionComparator>>>>,
    /// Maps framework sequence numbers to source positions (`source_position`
    /// bytes from the dispatched event).
    ///
    /// Populated during `dispatch_event()` / `dispatch_events_batch()` for
    /// events that carry a `source_position`.  Used by
    /// `compute_confirmed_source_position()` to convert the confirmed
    /// sequence (from position handles) back to a source-native position
    /// (e.g. Postgres WAL LSN) for upstream cursor advancement.
    ///
    /// Pruned explicitly via `prune_position_map()` after the source has
    /// successfully acknowledged the confirmed position to its upstream.
    sequence_position_map: Arc<RwLock<BTreeMap<u64, Bytes>>>,
}

impl SourceBase {
    /// Create a new SourceBase with the given parameters
    ///
    /// The status channel is not required during construction - it will be
    /// provided via the `SourceRuntimeContext` when `initialize()` is called.
    ///
    /// If a bootstrap provider is specified in params, it will be set during
    /// construction (no async needed since nothing is shared yet).
    pub fn new(params: SourceBaseParams) -> Result<Self> {
        // Determine dispatch mode (default to Channel if not specified)
        let dispatch_mode = params.dispatch_mode.unwrap_or_default();
        let dispatch_buffer_capacity = params.dispatch_buffer_capacity.unwrap_or(1000);

        // Set up initial dispatchers based on dispatch mode
        let mut dispatchers: Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>> =
            Vec::new();

        if dispatch_mode == DispatchMode::Broadcast {
            // For broadcast mode, create a single broadcast dispatcher
            let dispatcher =
                BroadcastChangeDispatcher::<SourceEventWrapper>::new(dispatch_buffer_capacity);
            dispatchers.push(Box::new(dispatcher));
        }
        // For channel mode, dispatchers will be created on-demand when subscribing

        // Initialize providers if provided (no async needed at construction time)
        let bootstrap_provider = params
            .bootstrap_provider
            .map(|p| Arc::from(p) as Arc<dyn BootstrapProvider>);

        Ok(Self {
            id: params.id.clone(),
            dispatch_mode,
            dispatch_buffer_capacity,
            auto_start: params.auto_start,
            status_handle: ComponentStatusHandle::new(&params.id),
            dispatchers: Arc::new(RwLock::new(dispatchers)),
            context: Arc::new(RwLock::new(None)), // Set by initialize()
            state_store: Arc::new(RwLock::new(params.state_store)), // Extracted from context
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
            bootstrap_provider: Arc::new(RwLock::new(bootstrap_provider)),
            identity_provider: Arc::new(RwLock::new(None)),
            position_handles: Arc::new(RwLock::new(HashMap::new())),
            next_sequence: Arc::new(AtomicU64::new(1)),
            raw_config: None,
            subscriber_notify: Arc::new(Notify::new()),
            subscriber_resume_positions: Arc::new(RwLock::new(HashMap::new())),
            position_comparator: Arc::new(RwLock::new(None)),
            sequence_position_map: Arc::new(RwLock::new(BTreeMap::new())),
        })
    }

    /// Get whether this source should auto-start
    pub fn get_auto_start(&self) -> bool {
        self.auto_start
    }

    /// Get this source's dispatch mode.
    pub fn get_dispatch_mode(&self) -> DispatchMode {
        self.dispatch_mode
    }

    /// Set the original raw config JSON for lossless persistence roundtrips.
    ///
    /// Called by plugin descriptors to preserve the original config JSON
    /// (including `ConfigValue` envelopes for secrets and env vars) before
    /// resolution to plain values.
    pub fn set_raw_config(&mut self, config: serde_json::Value) {
        self.raw_config = Some(config);
    }

    /// Get the original raw config JSON, if set by a descriptor.
    ///
    /// Returns `None` for builder-created components that don't have
    /// a raw config JSON (they use DTO reconstruction in `properties()`).
    pub fn raw_config(&self) -> Option<&serde_json::Value> {
        self.raw_config.as_ref()
    }

    /// Build the properties map for this source.
    ///
    /// If `raw_config` was set (descriptor path), returns its top-level keys.
    /// Otherwise, serializes `fallback_dto` (the DTO reconstructed from typed
    /// config) to produce camelCase output.
    ///
    /// This eliminates the duplicated if-let + serialize pattern from plugins.
    pub fn properties_or_serialize<D: serde::Serialize>(
        &self,
        fallback_dto: &D,
    ) -> HashMap<String, serde_json::Value> {
        if let Some(serde_json::Value::Object(map)) = self.raw_config.as_ref() {
            return map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        }

        match serde_json::to_value(fallback_dto) {
            Ok(serde_json::Value::Object(map)) => map.into_iter().collect(),
            _ => HashMap::new(),
        }
    }

    /// Initialize the source with runtime context.
    ///
    /// This method is called automatically by DrasiLib's `add_source()` method.
    /// Plugin developers do not need to call this directly.
    ///
    /// The context provides access to:
    /// - `source_id`: The source's unique identifier
    /// - `update_tx`: mpsc sender for fire-and-forget status updates to the component graph
    /// - `state_store`: Optional persistent state storage
    pub async fn initialize(&self, context: SourceRuntimeContext) {
        // Store context for later use
        *self.context.write().await = Some(context.clone());

        // Wire the status handle to the graph update channel
        self.status_handle.wire(context.update_tx.clone()).await;

        // Store state_store from context only if not already set programmatically
        if let Some(state_store) = context.state_store.as_ref() {
            let mut guard = self.state_store.write().await;
            if guard.is_none() {
                *guard = Some(state_store.clone());
            }
        }

        // Store identity provider from context if not already set programmatically
        if let Some(ip) = context.identity_provider.as_ref() {
            let mut guard = self.identity_provider.write().await;
            if guard.is_none() {
                *guard = Some(ip.clone());
            }
        }
    }

    /// Get the runtime context if initialized.
    ///
    /// Returns `None` if `initialize()` has not been called yet.
    pub async fn context(&self) -> Option<SourceRuntimeContext> {
        self.context.read().await.clone()
    }

    /// Get the state store if configured.
    ///
    /// Returns `None` if no state store was provided in the context.
    pub async fn state_store(&self) -> Option<Arc<dyn StateStoreProvider>> {
        self.state_store.read().await.clone()
    }

    /// Get the identity provider if set.
    ///
    /// Returns the identity provider set either programmatically via
    /// `set_identity_provider()` or from the runtime context during `initialize()`.
    /// Programmatically-set providers take precedence over context providers.
    pub async fn identity_provider(&self) -> Option<Arc<dyn IdentityProvider>> {
        self.identity_provider.read().await.clone()
    }

    /// Set the identity provider programmatically.
    ///
    /// This is typically called during source construction when the provider
    /// is available from configuration (e.g., `with_identity_provider()` builder).
    /// Providers set this way take precedence over context-injected providers.
    pub async fn set_identity_provider(&self, provider: Arc<dyn IdentityProvider>) {
        *self.identity_provider.write().await = Some(provider);
    }

    /// Create and register a position handle for `query_id`, initialized to `u64::MAX`.
    ///
    /// Returns the shared handle; the same `Arc` is placed in
    /// `SubscriptionResponse::position_handle` so the query and the source share
    /// one atomic. If a handle already exists for `query_id` (re-subscribe after
    /// transient disconnect), the existing handle is returned to preserve any
    /// position the query had previously reported.
    pub async fn create_position_handle(&self, query_id: &str) -> Arc<AtomicU64> {
        let mut handles = self.position_handles.write().await;
        if let Some(existing) = handles.get(query_id) {
            return existing.clone();
        }
        let handle = Arc::new(AtomicU64::new(u64::MAX));
        handles.insert(query_id.to_string(), handle.clone());
        handle
    }

    /// Remove the position handle for `query_id`. No-op if absent.
    ///
    /// Called from explicit cleanup paths (`stop_query`/`delete_query` will be
    /// wired in a follow-up issue). Until then, `cleanup_stale_handles()`
    /// (invoked inside `compute_confirmed_position`) catches dropped subscribers.
    pub async fn remove_position_handle(&self, query_id: &str) {
        let mut handles = self.position_handles.write().await;
        handles.remove(query_id);
    }

    /// Compute the minimum confirmed position across all live subscribers.
    ///
    /// Returns `None` if no handles are registered, or if every registered
    /// handle is still `u64::MAX` (no subscriber has confirmed a position yet —
    /// typically because they are still bootstrapping). Otherwise returns the
    /// minimum non-`u64::MAX` value, suitable for advancing the source's
    /// upstream cursor (Postgres `flush_lsn`, Kafka commit, transient WAL prune
    /// threshold).
    ///
    /// Piggy-backs `cleanup_stale_handles()` so dropped subscribers do not pin
    /// the watermark indefinitely.
    pub async fn compute_confirmed_position(&self) -> Option<u64> {
        self.cleanup_stale_handles().await;
        let handles = self.position_handles.read().await;
        let mut min: Option<u64> = None;
        for handle in handles.values() {
            let v = handle.load(Ordering::Relaxed);
            if v == u64::MAX {
                continue;
            }
            min = Some(min.map_or(v, |m| m.min(v)));
        }
        min
    }

    /// Drop entries whose `Arc::strong_count == 1` (only `SourceBase` holds a
    /// reference).
    ///
    /// This indicates the subscriber dropped its `SubscriptionResponse` without
    /// calling `remove_position_handle` — common during query teardown until
    /// explicit cleanup is wired by the query manager.
    ///
    /// Safety constraint: this relies on `SourceBase` being the only long-lived
    /// holder of the `Arc` besides the subscribing query. If a future periodic
    /// scan task (or any other component) clones the handle, this method must
    /// be revisited or replaced with explicit liveness tracking.
    pub async fn cleanup_stale_handles(&self) {
        let mut handles = self.position_handles.write().await;
        handles.retain(|_, handle| Arc::strong_count(handle) > 1);
    }

    /// Translate the confirmed framework sequence into the corresponding
    /// source-native position (e.g. Postgres WAL LSN, Kafka offset).
    ///
    /// Returns `None` when no confirmed position exists (no subscribers, all
    /// at `u64::MAX`, or the sequence map has been pruned past the confirmed
    /// point).
    ///
    /// This does **not** prune the internal map — call
    /// [`prune_position_map()`](Self::prune_position_map) after the source
    /// has successfully acknowledged the position to its upstream.
    pub async fn compute_confirmed_source_position(&self) -> Option<Bytes> {
        let confirmed_seq = self.compute_confirmed_position().await?;
        let map = self.sequence_position_map.read().await;
        // Find the entry with the largest sequence ≤ confirmed_seq.
        map.range(..=confirmed_seq)
            .next_back()
            .map(|(_, pos)| pos.clone())
    }

    /// Prune sequence→position entries that are no longer needed.
    ///
    /// Removes all entries with sequence ≤ `up_to_seq`. Call this after the
    /// source has successfully sent feedback/committed the confirmed position
    /// to its upstream, so re-send on failure is still possible.
    pub async fn prune_position_map(&self, up_to_seq: u64) {
        let mut map = self.sequence_position_map.write().await;
        // BTreeMap::split_off returns entries >= key; we keep those.
        let keep = map.split_off(&(up_to_seq.saturating_add(1)));
        *map = keep;
    }

    /// Clear the entire sequence→position map.
    ///
    /// Call this when restarting replication for replay so that stale
    /// sequence→position entries from the pre-replay stream cannot cause
    /// `compute_confirmed_source_position()` to return positions that no
    /// longer correspond to the current subscribers' checkpoints.  Without
    /// this, a keepalive feedback sent by the new replication task could
    /// advance `flush_lsn` (and thus the slot's `restart_lsn`) past the
    /// checkpoints of queries that have not yet subscribed, causing
    /// `PositionUnavailable` for subsequent subscribers.
    pub async fn clear_sequence_position_map(&self) {
        let mut map = self.sequence_position_map.write().await;
        map.clear();
    }

    /// Reset the sequence counter, typically after recovering from a checkpoint.
    /// The next dispatched event will receive `sequence + 1`.
    pub fn set_next_sequence(&self, sequence: u64) {
        self.next_sequence
            .store(sequence.saturating_add(1), Ordering::Relaxed);
    }

    /// Apply subscription settings that affect the source base.
    ///
    /// Should be called at the start of `Source::subscribe()` implementations.
    /// Handles:
    /// - Recovering the sequence counter from `last_sequence` to maintain monotonicity
    ///   across restarts.
    /// - Clearing stale sequence→position mappings when the counter advances, so
    ///   that `compute_confirmed_source_position()` cannot map a position handle to
    ///   a WAL position that predates the subscriber's actual checkpoint.
    pub fn apply_subscription_settings(
        &self,
        settings: &crate::config::SourceSubscriptionSettings,
    ) {
        if let Some(last_seq) = settings.last_sequence {
            // Atomically advance to last_seq+1, never go backwards.
            // fetch_max is safe under concurrent subscriptions.
            let next = last_seq.saturating_add(1);
            let prev = self.next_sequence.fetch_max(next, Ordering::Relaxed);
            if next > prev {
                info!(
                    "[{}] Sequence counter recovered to {} (from checkpoint last_sequence={})",
                    self.id, next, last_seq
                );
                // The counter jumped forward, meaning sequences currently in the
                // position map were assigned before this subscriber's checkpoint
                // was created. Those mappings are stale and would cause
                // compute_confirmed_source_position() to return incorrect
                // positions. Clear the map synchronously (try_write avoids
                // async in a non-async fn; contention here is near-zero since
                // dispatching is paused during subscribe).
                if let Ok(mut map) = self.sequence_position_map.try_write() {
                    map.clear();
                }
            }
        }
    }

    /// Returns a cloneable [`ComponentStatusHandle`] for use in spawned tasks.
    ///
    /// The handle can both read and write the component's status and automatically
    /// notifies the graph on every status change (after `initialize()`).
    pub fn status_handle(&self) -> ComponentStatusHandle {
        self.status_handle.clone()
    }

    /// Clone the SourceBase with shared Arc references
    ///
    /// This creates a new SourceBase that shares the same underlying
    /// data through Arc references. Useful for passing to spawned tasks.
    pub fn clone_shared(&self) -> Self {
        Self {
            id: self.id.clone(),
            dispatch_mode: self.dispatch_mode,
            dispatch_buffer_capacity: self.dispatch_buffer_capacity,
            auto_start: self.auto_start,
            status_handle: self.status_handle.clone(),
            dispatchers: self.dispatchers.clone(),
            context: self.context.clone(),
            state_store: self.state_store.clone(),
            task_handle: self.task_handle.clone(),
            shutdown_tx: self.shutdown_tx.clone(),
            bootstrap_provider: self.bootstrap_provider.clone(),
            identity_provider: self.identity_provider.clone(),
            position_handles: self.position_handles.clone(),
            next_sequence: self.next_sequence.clone(),
            raw_config: self.raw_config.clone(),
            subscriber_notify: self.subscriber_notify.clone(),
            subscriber_resume_positions: self.subscriber_resume_positions.clone(),
            position_comparator: self.position_comparator.clone(),
            sequence_position_map: self.sequence_position_map.clone(),
        }
    }

    /// Set the bootstrap provider for this source, taking ownership.
    ///
    /// Call this after creating the SourceBase if the source plugin supports bootstrapping.
    /// The bootstrap provider is created by the plugin using its own configuration.
    ///
    /// # Example
    /// ```ignore
    /// let provider = MyBootstrapProvider::new(config);
    /// source_base.set_bootstrap_provider(provider).await;  // Ownership transferred
    /// ```
    pub async fn set_bootstrap_provider(&self, provider: impl BootstrapProvider + 'static) {
        *self.bootstrap_provider.write().await = Some(Arc::new(provider));
    }

    /// Set the position comparator for per-subscriber replay filtering.
    ///
    /// Sources that support replay should call this during construction to
    /// enable per-subscriber position gating. Without a comparator, all events
    /// are delivered to all subscribers regardless of their `resume_from` position.
    pub async fn set_position_comparator(&self, comparator: impl PositionComparator + 'static) {
        *self.position_comparator.write().await = Some(Arc::new(comparator));
    }

    /// Get the source ID
    pub fn get_id(&self) -> &str {
        &self.id
    }

    /// Create a streaming receiver for a query subscription
    ///
    /// This creates the appropriate receiver based on the configured dispatch mode:
    /// - Broadcast mode: Returns a receiver from the shared broadcast dispatcher
    /// - Channel mode: Creates a new dedicated dispatcher and returns its receiver
    ///
    /// This is a helper method that can be used by sources with custom subscribe logic.
    pub async fn create_streaming_receiver(
        &self,
    ) -> Result<Box<dyn ChangeReceiver<SourceEventWrapper>>> {
        let receiver: Box<dyn ChangeReceiver<SourceEventWrapper>> = match self.dispatch_mode {
            DispatchMode::Broadcast => {
                // For broadcast mode, use the single dispatcher
                let dispatchers = self.dispatchers.read().await;
                if let Some(dispatcher) = dispatchers.first() {
                    dispatcher.create_receiver().await?
                } else {
                    return Err(anyhow::anyhow!("No broadcast dispatcher available"));
                }
            }
            DispatchMode::Channel => {
                // For channel mode, create a new dispatcher for this subscription
                let dispatcher = ChannelChangeDispatcher::<SourceEventWrapper>::new(
                    self.dispatch_buffer_capacity,
                );
                let receiver = dispatcher.create_receiver().await?;

                // Add the new dispatcher to our list
                let mut dispatchers = self.dispatchers.write().await;
                dispatchers.push(Box::new(dispatcher));

                receiver
            }
        };

        // Wake any task blocked in wait_for_subscribers().
        // Use notify_one() which stores a permit even if no one is waiting yet,
        // avoiding a race between the dispatchers check and the await.
        self.subscriber_notify.notify_one();

        Ok(receiver)
    }

    /// Wait until at least one subscriber has registered.
    ///
    /// Sources that start a background polling loop (e.g. CDC) should call
    /// this before entering their poll loop on a restart cycle. Without this,
    /// events dispatched before `subscribe()` creates a new dispatcher would
    /// be silently dropped, advancing the checkpoint past changes that no
    /// subscriber ever received.
    ///
    /// Returns immediately if at least one dispatcher already exists (fresh
    /// start with bootstrap, or broadcast mode which always has one).
    pub async fn wait_for_subscribers(&self) {
        loop {
            let dispatchers = self.dispatchers.read().await;
            if !dispatchers.is_empty() {
                return;
            }
            drop(dispatchers);
            self.subscriber_notify.notified().await;
        }
    }

    /// Subscribe to this source with optional bootstrap
    ///
    /// This is the standard subscribe implementation that all sources can use.
    /// It handles:
    /// - Creating a receiver for streaming events (based on dispatch mode)
    /// - Setting up bootstrap if requested and a provider has been set
    /// - Returning the appropriate SubscriptionResponse
    pub async fn subscribe_with_bootstrap(
        &self,
        settings: &crate::config::SourceSubscriptionSettings,
        source_type: &str,
    ) -> Result<SubscriptionResponse> {
        // Recover sequence counter from checkpoint before anything else
        self.apply_subscription_settings(settings);

        self.subscribe_with_bootstrap_context(settings, source_type, HashMap::new())
            .await
    }

    /// Subscribe to this source with optional bootstrap context properties.
    pub async fn subscribe_with_bootstrap_context(
        &self,
        settings: &crate::config::SourceSubscriptionSettings,
        source_type: &str,
        bootstrap_properties: HashMap<String, serde_json::Value>,
    ) -> Result<SubscriptionResponse> {
        info!(
            "Query '{}' subscribing to {} source '{}' (bootstrap: {}, resume_from: {:?}, request_handle: {})",
            settings.query_id,
            source_type,
            self.id,
            settings.enable_bootstrap,
            settings.resume_from,
            settings.request_position_handle
        );

        // Create streaming receiver using helper method
        let receiver = self.create_streaming_receiver().await?;

        // Register per-subscriber position filter for replay dedup.
        // In Channel mode, the new dispatcher is the last entry in the vec.
        // In Broadcast mode, per-subscriber filtering is not supported.
        if self.dispatch_mode == DispatchMode::Channel {
            if let Some(ref resume_pos) = settings.resume_from {
                let dispatchers = self.dispatchers.read().await;
                let dispatcher_idx = dispatchers.len().saturating_sub(1);
                drop(dispatchers);
                self.subscriber_resume_positions
                    .write()
                    .await
                    .insert(dispatcher_idx, resume_pos.clone());
                debug!(
                    "[{}] Registered resume position filter for subscriber '{}' at dispatcher index {}",
                    self.id, settings.query_id, dispatcher_idx
                );
            }
        }

        let query_id_for_response = settings.query_id.clone();

        // resume_from overrides bootstrap: a resuming query already has base
        // state in its persistent index and just needs replay from the
        // requested sequence. Re-bootstrapping would corrupt that state.
        let (bootstrap_receiver, bootstrap_result_receiver) = if settings.resume_from.is_some() {
            info!(
                "Query '{}' resuming from sequence {:?}; skipping bootstrap on {} source '{}'",
                settings.query_id, settings.resume_from, source_type, self.id
            );
            (None, None)
        } else if settings.enable_bootstrap {
            match self
                .handle_bootstrap_subscription(settings, source_type, bootstrap_properties)
                .await?
            {
                Some((event_rx, result_rx)) => (Some(event_rx), Some(result_rx)),
                None => (None, None),
            }
        } else {
            (None, None)
        };

        // Only persistent (replay-capable) queries request a handle. Volatile
        // queries are deliberately excluded from the min-watermark so they
        // cannot pin upstream advancement.
        let position_handle = if settings.request_position_handle {
            let handle = self.create_position_handle(&settings.query_id).await;
            // Initialize the handle to the query's checkpoint sequence so that
            // compute_confirmed_position() includes this subscriber from the
            // start. Without this, a resuming query whose handle stays at
            // u64::MAX would be invisible to the min-watermark, letting
            // flush_lsn advance past its checkpoint.
            if let Some(last_seq) = settings.last_sequence {
                handle.store(last_seq, Ordering::Release);
            }
            Some(handle)
        } else {
            None
        };

        Ok(SubscriptionResponse {
            query_id: query_id_for_response,
            source_id: self.id.clone(),
            receiver,
            bootstrap_receiver,
            position_handle,
            bootstrap_result_receiver,
        })
    }

    /// Create only the bootstrap receiver for a subscription.
    pub async fn create_bootstrap_receiver(
        &self,
        settings: &crate::config::SourceSubscriptionSettings,
        source_type: &str,
        bootstrap_properties: HashMap<String, serde_json::Value>,
    ) -> Result<Option<BootstrapEventReceiver>> {
        Ok(self
            .handle_bootstrap_subscription(settings, source_type, bootstrap_properties)
            .await?
            .map(|(bootstrap_receiver, _)| bootstrap_receiver))
    }

    /// Subscribe with WAL replay support.
    ///
    /// Creates a streaming channel, captures the WAL head under the dispatchers
    /// write lock, registers the new dispatcher, then reads WAL events and
    /// returns a composite receiver that yields replay events first followed by
    /// live events. This guarantees correct ordering without deadlock.
    ///
    /// # Arguments
    ///
    /// * `settings` - Subscription settings (query_id, resume_from, etc.)
    /// * `wal` - The WAL provider to read replay events from
    /// * `source_id` - The source ID registered with the WAL
    /// * `resume_seq` - The last confirmed sequence; replay starts from resume_seq+1
    /// * `source_type` - Label for logging (e.g., "HTTP", "gRPC")
    pub async fn subscribe_with_replay(
        &self,
        settings: &crate::config::SourceSubscriptionSettings,
        wal: &dyn crate::wal::WalProvider,
        resume_seq: u64,
        source_type: &str,
    ) -> Result<SubscriptionResponse> {
        use crate::channels::dispatcher::ReplayThenLiveReceiver;

        // Recover sequence counter from checkpoint
        self.apply_subscription_settings(settings);

        info!(
            "Query '{}' subscribing to {} source '{}' with WAL replay from seq {}",
            settings.query_id, source_type, self.id, resume_seq
        );

        // Hold dispatchers write lock to block live dispatch during setup.
        // This ensures no live events reach the new subscriber before replay.
        let mut dispatchers = self.dispatchers.write().await;

        // Capture WAL head while batcher is blocked (fast in-memory read)
        let head = wal.head_sequence(&self.id).await.map_err(|e| {
            anyhow::anyhow!("Failed to read WAL head for source '{}': {e}", self.id)
        })?;

        // Create dedicated channel for this subscriber
        let dispatcher = crate::channels::dispatcher::ChannelChangeDispatcher::<
            crate::channels::events::SourceEventWrapper,
        >::new(self.dispatch_buffer_capacity);
        let live_receiver = dispatcher.create_receiver().await?;

        // Add dispatcher to live list — after lock release, live events flow to it
        dispatchers.push(Box::new(dispatcher));
        drop(dispatchers);

        self.subscriber_notify.notify_one();

        // Read WAL events (after releasing lock — I/O can be slow).
        // Filter to only include events up to the captured head to avoid
        // duplicates with live events that were appended during setup.
        let replay_events = if resume_seq < head {
            match wal.read_from(&self.id, resume_seq.saturating_add(1)).await {
                Ok(events) => events
                    .into_iter()
                    .filter(|(seq, _)| *seq <= head)
                    .collect::<Vec<_>>(),
                Err(e @ crate::wal::WalError::PositionUnavailable { .. }) => {
                    return Err(e.into());
                }
                Err(e) => return Err(e.into()),
            }
        } else {
            Vec::new()
        };

        // Build replay wrappers
        let replay_wrappers: std::collections::VecDeque<
            std::sync::Arc<crate::channels::events::SourceEventWrapper>,
        > = replay_events
            .into_iter()
            .map(|(seq, change)| {
                std::sync::Arc::new(crate::channels::events::SourceEventWrapper {
                    source_id: self.id.clone(),
                    event: crate::channels::events::SourceEvent::Change(change),
                    timestamp: chrono::Utc::now(),
                    sequence: Some(seq),
                    source_position: Some(bytes::Bytes::from(seq.to_be_bytes().to_vec())),
                    profiling: None,
                })
            })
            .collect();

        let replay_count = replay_wrappers.len();
        info!(
            "[{}] WAL replay: {} events (seq {}..={})",
            self.id,
            replay_count,
            resume_seq + 1,
            head
        );

        // Composite receiver: replay first, then live
        let composite: Box<
            dyn crate::channels::ChangeReceiver<crate::channels::events::SourceEventWrapper>,
        > = Box::new(ReplayThenLiveReceiver::new(replay_wrappers, live_receiver));

        // Position handle
        let position_handle = if settings.request_position_handle {
            let handle = self.create_position_handle(&settings.query_id).await;
            if let Some(last_seq) = settings.last_sequence {
                handle.store(last_seq, Ordering::Release);
            }
            Some(handle)
        } else {
            None
        };

        Ok(SubscriptionResponse {
            query_id: settings.query_id.clone(),
            source_id: self.id.clone(),
            receiver: composite,
            bootstrap_receiver: None,
            position_handle,
            bootstrap_result_receiver: None,
        })
    }

    /// Handle bootstrap subscription logic.
    ///
    /// Returns the bootstrap event receiver and a oneshot receiver for the
    /// `BootstrapResult` handover metadata.
    async fn handle_bootstrap_subscription(
        &self,
        settings: &crate::config::SourceSubscriptionSettings,
        source_type: &str,
        bootstrap_properties: HashMap<String, serde_json::Value>,
    ) -> Result<
        Option<(
            BootstrapEventReceiver,
            tokio::sync::oneshot::Receiver<anyhow::Result<BootstrapResult>>,
        )>,
    > {
        let provider_guard = self.bootstrap_provider.read().await;
        if let Some(provider) = provider_guard.clone() {
            drop(provider_guard); // Release lock before spawning task

            info!(
                "Creating bootstrap for query '{}' on {} source '{}'",
                settings.query_id, source_type, self.id
            );

            // Create bootstrap context
            let context = if bootstrap_properties.is_empty() {
                BootstrapContext::new_minimal(
                    self.id.clone(), // server_id
                    self.id.clone(), // source_id
                )
            } else {
                BootstrapContext::with_properties(
                    self.id.clone(), // server_id
                    self.id.clone(), // source_id
                    bootstrap_properties,
                )
            };

            // Create bootstrap channel
            let (bootstrap_tx, bootstrap_rx) = tokio::sync::mpsc::channel(1000);

            // Create oneshot for BootstrapResult handover metadata
            let (result_tx, result_rx) = tokio::sync::oneshot::channel();

            // Convert HashSet to Vec for backward compatibility with BootstrapRequest
            let node_labels: Vec<String> = settings.nodes.iter().cloned().collect();
            let relation_labels: Vec<String> = settings.relations.iter().cloned().collect();

            // Create bootstrap request with request_id
            let request = BootstrapRequest {
                query_id: settings.query_id.clone(),
                node_labels,
                relation_labels,
                request_id: format!("{}-{}", settings.query_id, uuid::Uuid::new_v4()),
            };

            // Clone settings for the async task
            let settings_clone = settings.clone();
            let source_id = self.id.clone();

            // Get instance_id from context for log routing isolation
            let instance_id = self
                .context()
                .await
                .map(|c| c.instance_id.clone())
                .unwrap_or_default();

            // Spawn bootstrap task with tracing span for proper log routing
            let span = tracing::info_span!(
                "source_bootstrap",
                instance_id = %instance_id,
                component_id = %source_id,
                component_type = "source"
            );
            tokio::spawn(
                async move {
                    let outcome = provider
                        .bootstrap(request, &context, bootstrap_tx, Some(&settings_clone))
                        .await;

                    match &outcome {
                        Ok(result) => {
                            info!(
                                "Bootstrap completed successfully for query '{}', sent {} events \
                                 (last_sequence={:?}, sequences_aligned={})",
                                settings_clone.query_id,
                                result.event_count,
                                result.last_sequence,
                                result.sequences_aligned
                            );
                        }
                        Err(e) => {
                            error!(
                                "Bootstrap failed for query '{}': {e}",
                                settings_clone.query_id
                            );
                        }
                    }

                    // Send the result (or error) to the query manager for handover.
                    // If the receiver was dropped (query stopped), this is a no-op.
                    let _ = result_tx.send(outcome);
                }
                .instrument(span),
            );

            Ok(Some((bootstrap_rx, result_rx)))
        } else {
            info!(
                "Bootstrap requested for query '{}' but no bootstrap provider configured for {} source '{}'",
                settings.query_id, source_type, self.id
            );
            Ok(None)
        }
    }

    /// Dispatch a SourceChange event with profiling metadata
    ///
    /// This method handles the common pattern of:
    /// - Creating profiling metadata with timestamp
    /// - Wrapping the change in a SourceEventWrapper
    /// - Dispatching to all subscribers
    /// - Handling the no-subscriber case gracefully
    pub async fn dispatch_source_change(&self, change: SourceChange) -> Result<()> {
        // Create profiling metadata
        let mut profiling = profiling::ProfilingMetadata::new();
        profiling.source_send_ns = Some(profiling::timestamp_ns());

        // Create event wrapper
        let wrapper = SourceEventWrapper::with_profiling(
            self.id.clone(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
            profiling,
        );

        // Dispatch event
        self.dispatch_event(wrapper).await
    }

    /// Maximum allowed size for source position bytes (64KB).
    /// Positions exceeding this limit are skipped at checkpoint time
    /// to prevent memory issues, preserving the last good position.
    pub const MAX_SOURCE_POSITION_BYTES: usize = 65_536;

    /// Dispatch a SourceEventWrapper to all subscribers.
    ///
    /// This is a generic method for dispatching any SourceEvent.
    /// It handles Arc-wrapping for zero-copy sharing and logs
    /// when there are no subscribers.
    /// The framework stamps every event with a monotonic sequence number.
    pub async fn dispatch_event(&self, mut wrapper: SourceEventWrapper) -> Result<()> {
        // Warn about oversized source positions; the checkpoint layer will
        // enforce the limit and preserve the last good position.
        if let Some(ref pos) = wrapper.source_position {
            if pos.len() > Self::MAX_SOURCE_POSITION_BYTES {
                warn!(
                    "[{}] Source position is large ({} bytes > {} limit); \
                     checkpoint staging will preserve the previous good position",
                    self.id,
                    pos.len(),
                    Self::MAX_SOURCE_POSITION_BYTES
                );
            }
        }

        // Framework assigns the monotonic sequence (skip if pre-set by WAL)
        if let Some(seq) = wrapper.sequence {
            // Pre-set by WAL — advance counter to maintain monotonicity
            self.next_sequence
                .fetch_max(seq.saturating_add(1), Ordering::Relaxed);
        } else {
            wrapper.sequence = Some(self.next_sequence.fetch_add(1, Ordering::Relaxed));
        }

        // Record sequence→source_position mapping for confirmed-position lookups.
        if let (Some(seq), Some(ref pos)) = (wrapper.sequence, &wrapper.source_position) {
            self.sequence_position_map
                .write()
                .await
                .insert(seq, pos.clone());
        }

        debug!("[{}] Dispatching event: {:?}", self.id, &wrapper);

        // Arc-wrap for zero-copy sharing across dispatchers
        let arc_wrapper = Arc::new(wrapper);

        // Send to all dispatchers, filtering by per-subscriber resume position
        let dispatchers = self.dispatchers.read().await;
        let comparator = self.position_comparator.read().await;
        let mut cleared_indices: Vec<usize> = Vec::new();
        // Collect (dispatcher_index, new_high_water) updates for after dispatch.
        let mut hwm_updates: Vec<(usize, Bytes)> = Vec::new();

        for (idx, dispatcher) in dispatchers.iter().enumerate() {
            // Check per-subscriber position high-water mark.
            // Events at or before the subscriber's high-water mark are
            // suppressed.  When the event passes the mark, we deliver it
            // and advance the mark (instead of removing it) so that future
            // rewinds (caused by later subscribers triggering a stream
            // restart) are still caught.
            if let Some(ref cmp) = *comparator {
                let resume_positions = self.subscriber_resume_positions.read().await;
                if let Some(resume_pos) = resume_positions.get(&idx) {
                    if let Some(ref event_pos) = arc_wrapper.source_position {
                        if !cmp.position_reached(event_pos, resume_pos) {
                            // Event hasn't passed high-water mark — suppress
                            continue;
                        }
                        // Position reached — will update high-water after dispatch
                        cleared_indices.push(idx);
                    }
                    // No source_position on event — cannot filter, deliver it
                }
            }

            if let Err(e) = dispatcher.dispatch_change(arc_wrapper.clone()).await {
                debug!("[{}] Failed to dispatch event: {}", self.id, e);
            } else if let Some(ref event_pos) = arc_wrapper.source_position {
                // Track high-water mark so future rewinds are caught
                hwm_updates.push((idx, event_pos.clone()));
            }
        }
        drop(comparator);
        drop(dispatchers);

        // Advance high-water marks for dispatchers that received this event.
        // For dispatchers that already had an entry (cleared_indices), this
        // updates their mark.  For dispatchers that had no entry yet, this
        // establishes one — protecting them from future rewinds.
        if !hwm_updates.is_empty() {
            let mut resume_positions = self.subscriber_resume_positions.write().await;
            for (idx, pos) in hwm_updates {
                resume_positions.insert(idx, pos);
            }
        }

        Ok(())
    }

    /// Dispatch a batch of events, acquiring the dispatchers lock once for
    /// the entire batch. This is more efficient than calling
    /// [`dispatch_event()`](Self::dispatch_event) per-event when the source
    /// processes multiple rows per poll cycle.
    pub async fn dispatch_events_batch(&self, events: Vec<SourceEventWrapper>) -> Result<()> {
        if events.is_empty() {
            return Ok(());
        }

        let dispatchers = self.dispatchers.read().await;
        let comparator = self.position_comparator.read().await;

        for mut wrapper in events {
            if let Some(ref pos) = wrapper.source_position {
                if pos.len() > Self::MAX_SOURCE_POSITION_BYTES {
                    warn!(
                        "[{}] Source position is large ({} bytes > {} limit); \
                         checkpoint staging will preserve the previous good position",
                        self.id,
                        pos.len(),
                        Self::MAX_SOURCE_POSITION_BYTES
                    );
                }
            }

            // Framework assigns the monotonic sequence (skip if pre-set by WAL)
            if let Some(seq) = wrapper.sequence {
                self.next_sequence
                    .fetch_max(seq.saturating_add(1), Ordering::Relaxed);
            } else {
                wrapper.sequence = Some(self.next_sequence.fetch_add(1, Ordering::Relaxed));
            }

            // Record sequence→source_position mapping for confirmed-position lookups.
            if let (Some(seq), Some(ref pos)) = (wrapper.sequence, &wrapper.source_position) {
                self.sequence_position_map
                    .write()
                    .await
                    .insert(seq, pos.clone());
            }

            debug!("[{}] Dispatching event (batch): {:?}", self.id, &wrapper);

            let arc_wrapper = Arc::new(wrapper);
            let mut cleared_indices: Vec<usize> = Vec::new();
            let mut hwm_updates: Vec<(usize, Bytes)> = Vec::new();

            for (idx, dispatcher) in dispatchers.iter().enumerate() {
                // Check per-subscriber position high-water mark
                if let Some(ref cmp) = *comparator {
                    let resume_positions = self.subscriber_resume_positions.read().await;
                    if let Some(resume_pos) = resume_positions.get(&idx) {
                        if let Some(ref event_pos) = arc_wrapper.source_position {
                            if !cmp.position_reached(event_pos, resume_pos) {
                                debug!(
                                    "[{}] Position filter: SKIPPING event for dispatcher {} \
                                     (event_pos={:?} <= resume_pos={:?})",
                                    self.id,
                                    idx,
                                    event_pos.as_ref(),
                                    resume_pos.as_ref()
                                );
                                continue;
                            }
                            debug!(
                                "[{}] Position filter: PASSING event for dispatcher {} \
                                 (event_pos={:?} > resume_pos={:?})",
                                self.id,
                                idx,
                                event_pos.as_ref(),
                                resume_pos.as_ref()
                            );
                            cleared_indices.push(idx);
                        }
                    } else {
                        debug!(
                            "[{}] Position filter: NO resume position for dispatcher {}, passing through",
                            self.id, idx
                        );
                    }
                }

                if let Err(e) = dispatcher.dispatch_change(arc_wrapper.clone()).await {
                    debug!("[{}] Failed to dispatch event: {}", self.id, e);
                } else if let Some(ref event_pos) = arc_wrapper.source_position {
                    hwm_updates.push((idx, event_pos.clone()));
                }
            }

            // Advance high-water marks for dispatchers that received this event
            if !hwm_updates.is_empty() {
                let mut resume_positions = self.subscriber_resume_positions.write().await;
                for (idx, pos) in hwm_updates {
                    resume_positions.insert(idx, pos);
                }
            }
        }

        drop(comparator);
        drop(dispatchers);

        Ok(())
    }

    /// Broadcast SourceControl events
    pub async fn broadcast_control(&self, control: SourceControl) -> Result<()> {
        let wrapper = SourceEventWrapper::new(
            self.id.clone(),
            SourceEvent::Control(control),
            chrono::Utc::now(),
        );
        self.dispatch_event(wrapper).await
    }

    /// Create a test subscription to this source (async, fallible)
    ///
    /// This method is intended for use in tests to receive events from the source.
    /// It properly handles both Broadcast and Channel dispatch modes by delegating
    /// to `create_streaming_receiver()`, making the dispatch mode transparent to tests.
    ///
    /// This method is designed for async test contexts and is compatible with
    /// both `current_thread` and `multi_thread` Tokio runtimes.
    ///
    /// # Returns
    /// A receiver that will receive all events dispatched by this source,
    /// or an error if the receiver cannot be created.
    pub async fn try_test_subscribe(
        &self,
    ) -> anyhow::Result<Box<dyn ChangeReceiver<SourceEventWrapper>>> {
        self.create_streaming_receiver().await
    }

    /// Create a test subscription to this source
    ///
    /// Convenience wrapper around [`try_test_subscribe`](Self::try_test_subscribe)
    /// that panics on failure. Prefer `try_test_subscribe()` in new code.
    ///
    /// # Panics
    /// Panics if the receiver cannot be created.
    pub async fn test_subscribe(&self) -> Box<dyn ChangeReceiver<SourceEventWrapper>> {
        self.try_test_subscribe()
            .await
            .expect("Failed to create test subscription receiver")
    }

    /// Helper function to dispatch events from spawned tasks (unstamped).
    ///
    /// This is a static helper that can be used from spawned async tasks that don't
    /// have access to `self`. It manually iterates through dispatchers and sends the event.
    ///
    /// **Important**: This method does NOT stamp a monotonic sequence number and
    /// does NOT validate `source_position` size. Events dispatched through this
    /// method will not be checkpoint-tracked. This is acceptable for sources that
    /// do not support replay (`supports_replay() == false`).
    ///
    /// # For recoverable/checkpointed sources
    ///
    /// Use [`clone_shared()`](Self::clone_shared) to obtain a `SourceBase` that
    /// can be moved into spawned tasks, then call [`dispatch_event()`](Self::dispatch_event)
    /// which stamps sequences and validates positions:
    ///
    /// ```ignore
    /// let base = self.base.clone_shared();
    /// tokio::spawn(async move {
    ///     base.dispatch_event(wrapper).await.ok();
    /// });
    /// ```
    ///
    /// # Arguments
    /// * `dispatchers` - Arc to the dispatchers list (from `self.base.dispatchers.clone()`)
    /// * `wrapper` - The event wrapper to dispatch
    /// * `source_id` - Source ID for logging
    pub async fn dispatch_from_task(
        dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
        wrapper: SourceEventWrapper,
        source_id: &str,
    ) -> Result<()> {
        debug!(
            "[{}] Dispatching event from task: {:?}",
            source_id, &wrapper
        );

        // Arc-wrap for zero-copy sharing across dispatchers
        let arc_wrapper = Arc::new(wrapper);

        // Send to all dispatchers
        let dispatchers_guard = dispatchers.read().await;
        for dispatcher in dispatchers_guard.iter() {
            if let Err(e) = dispatcher.dispatch_change(arc_wrapper.clone()).await {
                debug!("[{source_id}] Failed to dispatch event from task: {e}");
            }
        }

        Ok(())
    }

    /// Handle common stop functionality
    pub async fn stop_common(&self) -> Result<()> {
        info!("Stopping source '{}'", self.id);

        // Send shutdown signal if we have one
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }

        // Wait for task to complete
        if let Some(mut handle) = self.task_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), &mut handle).await {
                Ok(Ok(())) => {
                    info!("Source '{}' task completed successfully", self.id);
                }
                Ok(Err(e)) => {
                    error!("Source '{}' task panicked: {}", self.id, e);
                }
                Err(_) => {
                    warn!(
                        "Source '{}' task did not complete within timeout, aborting",
                        self.id
                    );
                    handle.abort();
                }
            }
        }

        // Clear stale dispatchers so that a subsequent start()+subscribe()
        // cycle does not race: without this, the CDC polling loop could
        // dispatch events to the old (dead) channel receivers before the
        // new subscribe() call creates fresh dispatchers, silently dropping
        // events while still advancing the checkpoint LSN.
        //
        // Broadcast mode keeps a single persistent dispatcher that hands
        // out receivers; channel mode creates one dispatcher per subscriber.
        if self.dispatch_mode == DispatchMode::Channel {
            let mut dispatchers = self.dispatchers.write().await;
            dispatchers.clear();
        }

        self.set_status(
            ComponentStatus::Stopped,
            Some(format!("Source '{}' stopped", self.id)),
        )
        .await;
        info!("Source '{}' stopped", self.id);
        Ok(())
    }

    /// Clear stale dispatchers from a prior lifecycle.
    ///
    /// Sources that manage their own stop/start lifecycle (instead of using
    /// `stop_common()`) **must** call this at the end of their `stop()`
    /// implementation. Without this, a subsequent `start()` + `subscribe()`
    /// cycle can race: the polling loop dispatches events to the old (dead)
    /// channel receivers before `subscribe()` creates fresh dispatchers,
    /// silently dropping events while still advancing the checkpoint.
    ///
    /// Only channel-mode dispatchers are cleared — broadcast mode keeps a
    /// single persistent dispatcher.
    pub async fn clear_dispatchers(&self) {
        if self.dispatch_mode == DispatchMode::Channel {
            let mut dispatchers = self.dispatchers.write().await;
            dispatchers.clear();
        }
    }

    /// Clear the source's state store partition.
    ///
    /// This is called during deprovision to remove all persisted state
    /// associated with this source. Sources that override `deprovision()`
    /// can call this to clean up their state store.
    pub async fn deprovision_common(&self) -> Result<()> {
        info!("Deprovisioning source '{}'", self.id);
        if let Some(store) = self.state_store().await {
            let count = store.clear_store(&self.id).await.map_err(|e| {
                anyhow::anyhow!(
                    "Failed to clear state store for source '{}': {}",
                    self.id,
                    e
                )
            })?;
            info!(
                "Cleared {} keys from state store for source '{}'",
                count, self.id
            );
        }
        Ok(())
    }

    /// Get the current status.
    pub async fn get_status(&self) -> ComponentStatus {
        self.status_handle.get_status().await
    }

    /// Set the component's status — updates local state AND notifies the graph.
    ///
    /// This is the single canonical way to change a source's status.
    pub async fn set_status(&self, status: ComponentStatus, message: Option<String>) {
        self.status_handle.set_status(status, message).await;
    }

    /// Set the task handle
    pub async fn set_task_handle(&self, handle: tokio::task::JoinHandle<()>) {
        *self.task_handle.write().await = Some(handle);
    }

    /// Set the shutdown sender
    pub async fn set_shutdown_tx(&self, tx: tokio::sync::oneshot::Sender<()>) {
        *self.shutdown_tx.write().await = Some(tx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sources::ByteLexPositionComparator;

    // =========================================================================
    // SourceBaseParams tests
    // =========================================================================

    #[test]
    fn test_params_new_defaults() {
        let params = SourceBaseParams::new("test-source");
        assert_eq!(params.id, "test-source");
        assert!(params.dispatch_mode.is_none());
        assert!(params.dispatch_buffer_capacity.is_none());
        assert!(params.bootstrap_provider.is_none());
        assert!(params.auto_start);
    }

    #[test]
    fn test_params_with_dispatch_mode() {
        let params = SourceBaseParams::new("s1").with_dispatch_mode(DispatchMode::Broadcast);
        assert_eq!(params.dispatch_mode, Some(DispatchMode::Broadcast));
    }

    #[test]
    fn test_params_with_dispatch_buffer_capacity() {
        let params = SourceBaseParams::new("s1").with_dispatch_buffer_capacity(50000);
        assert_eq!(params.dispatch_buffer_capacity, Some(50000));
    }

    #[test]
    fn test_params_with_auto_start_false() {
        let params = SourceBaseParams::new("s1").with_auto_start(false);
        assert!(!params.auto_start);
    }

    #[test]
    fn test_params_builder_chaining() {
        let params = SourceBaseParams::new("chained")
            .with_dispatch_mode(DispatchMode::Broadcast)
            .with_dispatch_buffer_capacity(2000)
            .with_auto_start(false);

        assert_eq!(params.id, "chained");
        assert_eq!(params.dispatch_mode, Some(DispatchMode::Broadcast));
        assert_eq!(params.dispatch_buffer_capacity, Some(2000));
        assert!(!params.auto_start);
    }

    // =========================================================================
    // SourceBase tests
    // =========================================================================

    #[tokio::test]
    async fn test_new_defaults() {
        let params = SourceBaseParams::new("my-source");
        let base = SourceBase::new(params).unwrap();

        assert_eq!(base.id, "my-source");
        assert!(base.auto_start);
        assert_eq!(base.get_status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_get_id() {
        let base = SourceBase::new(SourceBaseParams::new("id-check")).unwrap();
        assert_eq!(base.get_id(), "id-check");
    }

    #[tokio::test]
    async fn test_get_auto_start() {
        let base_default = SourceBase::new(SourceBaseParams::new("a")).unwrap();
        assert!(base_default.get_auto_start());

        let base_false =
            SourceBase::new(SourceBaseParams::new("b").with_auto_start(false)).unwrap();
        assert!(!base_false.get_auto_start());
    }

    #[tokio::test]
    async fn test_get_status_initial() {
        let base = SourceBase::new(SourceBaseParams::new("s")).unwrap();
        assert_eq!(base.get_status().await, ComponentStatus::Stopped);
    }

    #[tokio::test]
    async fn test_set_status() {
        let base = SourceBase::new(SourceBaseParams::new("s")).unwrap();

        base.set_status(ComponentStatus::Running, None).await;
        assert_eq!(base.get_status().await, ComponentStatus::Running);

        base.set_status(ComponentStatus::Error, Some("oops".into()))
            .await;
        assert_eq!(base.get_status().await, ComponentStatus::Error);
    }

    #[tokio::test]
    async fn test_status_handle_returns_handle() {
        let base = SourceBase::new(SourceBaseParams::new("s")).unwrap();
        let handle = base.status_handle();

        // The handle should reflect the same status as the base
        assert_eq!(handle.get_status().await, ComponentStatus::Stopped);

        // Mutating through the handle should be visible via SourceBase
        handle.set_status(ComponentStatus::Starting, None).await;
        assert_eq!(base.get_status().await, ComponentStatus::Starting);
    }

    // =========================================================================
    // Position handle and resume_from tests (issue #366)
    // =========================================================================

    use crate::bootstrap::{
        BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult,
    };
    use crate::channels::BootstrapEventSender;
    use async_trait::async_trait;

    fn make_settings(
        query_id: &str,
        enable_bootstrap: bool,
        resume_from: Option<bytes::Bytes>,
        request_position_handle: bool,
    ) -> crate::config::SourceSubscriptionSettings {
        use std::collections::HashSet;
        crate::config::SourceSubscriptionSettings {
            source_id: "test-src".to_string(),
            enable_bootstrap,
            query_id: query_id.to_string(),
            nodes: HashSet::new(),
            relations: HashSet::new(),
            resume_from,
            request_position_handle,
            last_sequence: None,
        }
    }

    /// Minimal bootstrap provider that closes its sender immediately.
    /// Enough to verify a `bootstrap_receiver` was created without sending data.
    struct NoopProvider;

    #[async_trait]
    impl BootstrapProvider for NoopProvider {
        async fn bootstrap(
            &self,
            _request: BootstrapRequest,
            _context: &BootstrapContext,
            _event_tx: BootstrapEventSender,
            _settings: Option<&crate::config::SourceSubscriptionSettings>,
        ) -> Result<BootstrapResult> {
            Ok(BootstrapResult::default())
        }
    }

    fn make_base_with_bootstrap(id: &str) -> SourceBase {
        let mut params = SourceBaseParams::new(id);
        params.bootstrap_provider = Some(Box::new(NoopProvider));
        SourceBase::new(params).unwrap()
    }

    #[tokio::test]
    async fn test_create_position_handle_initializes_to_u64_max() {
        let base = SourceBase::new(SourceBaseParams::new("ph-init")).unwrap();
        let handle = base.create_position_handle("q1").await;
        assert_eq!(handle.load(Ordering::Relaxed), u64::MAX);
    }

    #[tokio::test]
    async fn test_create_position_handle_idempotent_for_same_query() {
        let base = SourceBase::new(SourceBaseParams::new("ph-idem")).unwrap();
        let h1 = base.create_position_handle("q1").await;
        h1.store(123, Ordering::Relaxed);
        let h2 = base.create_position_handle("q1").await;
        // Same Arc — preserves the previously reported position.
        assert!(Arc::ptr_eq(&h1, &h2));
        assert_eq!(h2.load(Ordering::Relaxed), 123);
    }

    #[tokio::test]
    async fn test_remove_position_handle_drops_entry() {
        let base = SourceBase::new(SourceBaseParams::new("ph-rm")).unwrap();
        let handle = base.create_position_handle("q1").await;
        handle.store(42, Ordering::Relaxed);
        assert_eq!(base.compute_confirmed_position().await, Some(42));
        base.remove_position_handle("q1").await;
        assert_eq!(base.compute_confirmed_position().await, None);
    }

    #[tokio::test]
    async fn test_remove_position_handle_noop_when_absent() {
        let base = SourceBase::new(SourceBaseParams::new("ph-rm-absent")).unwrap();
        // Must not panic.
        base.remove_position_handle("never-registered").await;
        assert_eq!(base.compute_confirmed_position().await, None);
    }

    #[tokio::test]
    async fn test_compute_confirmed_position_returns_none_when_empty() {
        let base = SourceBase::new(SourceBaseParams::new("ph-empty")).unwrap();
        assert_eq!(base.compute_confirmed_position().await, None);
    }

    #[tokio::test]
    async fn test_compute_confirmed_position_returns_none_when_all_max() {
        let base = SourceBase::new(SourceBaseParams::new("ph-all-max")).unwrap();
        let _h1 = base.create_position_handle("q1").await;
        let _h2 = base.create_position_handle("q2").await;
        assert_eq!(base.compute_confirmed_position().await, None);
    }

    #[tokio::test]
    async fn test_compute_confirmed_position_filters_max_returns_min() {
        let base = SourceBase::new(SourceBaseParams::new("ph-min")).unwrap();
        let h1 = base.create_position_handle("q1").await;
        let _h2 = base.create_position_handle("q2").await; // stays u64::MAX
        let h3 = base.create_position_handle("q3").await;
        h1.store(100, Ordering::Relaxed);
        h3.store(50, Ordering::Relaxed);
        assert_eq!(base.compute_confirmed_position().await, Some(50));
    }

    #[tokio::test]
    async fn test_compute_confirmed_position_single_real_value() {
        let base = SourceBase::new(SourceBaseParams::new("ph-single")).unwrap();
        let h1 = base.create_position_handle("q1").await;
        let _h2 = base.create_position_handle("q2").await;
        h1.store(7, Ordering::Relaxed);
        assert_eq!(base.compute_confirmed_position().await, Some(7));
    }

    #[tokio::test]
    async fn test_cleanup_stale_handles_drops_orphaned_arc() {
        let base = SourceBase::new(SourceBaseParams::new("ph-stale")).unwrap();
        {
            let handle = base.create_position_handle("q1").await;
            handle.store(99, Ordering::Relaxed);
            // handle (the external clone) drops here.
        }
        base.cleanup_stale_handles().await;
        assert_eq!(base.compute_confirmed_position().await, None);
    }

    #[tokio::test]
    async fn test_cleanup_stale_handles_keeps_held_arc() {
        let base = SourceBase::new(SourceBaseParams::new("ph-held")).unwrap();
        let handle = base.create_position_handle("q1").await;
        handle.store(11, Ordering::Relaxed);
        base.cleanup_stale_handles().await;
        // External clone still alive, entry retained.
        assert_eq!(base.compute_confirmed_position().await, Some(11));
        // Keep `handle` alive past the assertion.
        drop(handle);
    }

    #[tokio::test]
    async fn test_subscribe_with_request_position_handle_returns_handle() {
        let base = SourceBase::new(SourceBaseParams::new("sub-handle")).unwrap();
        let response = base
            .subscribe_with_bootstrap(&make_settings("q1", false, None, true), "test")
            .await
            .unwrap();
        let handle = response.position_handle.expect("expected handle");
        assert_eq!(handle.load(Ordering::Relaxed), u64::MAX);
        // Internal map should also have the entry (so min-watermark sees it).
        assert_eq!(base.compute_confirmed_position().await, None); // u64::MAX → None
    }

    #[tokio::test]
    async fn test_subscribe_without_request_position_handle_returns_none() {
        let base = SourceBase::new(SourceBaseParams::new("sub-no-handle")).unwrap();
        let response = base
            .subscribe_with_bootstrap(&make_settings("q1", false, None, false), "test")
            .await
            .unwrap();
        assert!(response.position_handle.is_none());
        // Internal map must be empty so this volatile query is excluded from
        // the min-watermark.
        let handles = base.position_handles.read().await;
        assert!(handles.is_empty());
    }

    #[tokio::test]
    async fn test_subscribe_returned_handle_shared_with_internal() {
        let base = SourceBase::new(SourceBaseParams::new("sub-shared")).unwrap();
        let response = base
            .subscribe_with_bootstrap(&make_settings("q1", false, None, true), "test")
            .await
            .unwrap();
        let handle = response.position_handle.unwrap();
        handle.store(42, Ordering::Relaxed);
        assert_eq!(base.compute_confirmed_position().await, Some(42));
    }

    #[tokio::test]
    async fn test_subscribe_with_resume_from_skips_bootstrap() {
        let base = make_base_with_bootstrap("sub-resume");
        let position = bytes::Bytes::copy_from_slice(&100u64.to_le_bytes());
        let response = base
            .subscribe_with_bootstrap(&make_settings("q1", true, Some(position), false), "test")
            .await
            .unwrap();
        assert!(
            response.bootstrap_receiver.is_none(),
            "resume_from must override enable_bootstrap"
        );
    }

    #[tokio::test]
    async fn test_subscribe_resume_without_bootstrap_still_none() {
        let base = make_base_with_bootstrap("sub-resume-no-bs");
        let position = bytes::Bytes::copy_from_slice(&100u64.to_le_bytes());
        let response = base
            .subscribe_with_bootstrap(&make_settings("q1", false, Some(position), false), "test")
            .await
            .unwrap();
        assert!(response.bootstrap_receiver.is_none());
    }

    #[tokio::test]
    async fn test_subscribe_no_resume_with_bootstrap_returns_receiver() {
        let base = make_base_with_bootstrap("sub-bs");
        let response = base
            .subscribe_with_bootstrap(&make_settings("q1", true, None, false), "test")
            .await
            .unwrap();
        assert!(
            response.bootstrap_receiver.is_some(),
            "regression guard: bootstrap path must still produce a receiver"
        );
    }

    #[tokio::test]
    async fn test_subscribe_no_resume_no_bootstrap_returns_none() {
        let base = make_base_with_bootstrap("sub-neither");
        let response = base
            .subscribe_with_bootstrap(&make_settings("q1", false, None, false), "test")
            .await
            .unwrap();
        assert!(response.bootstrap_receiver.is_none());
        assert!(response.position_handle.is_none());
    }

    // =========================================================================
    // dispatch_events_batch tests
    // =========================================================================

    fn make_event(source_id: &str, position: Option<&[u8]>) -> SourceEventWrapper {
        let change = drasi_core::models::SourceChange::Insert {
            element: drasi_core::models::Element::Node {
                metadata: drasi_core::models::ElementMetadata {
                    reference: drasi_core::models::ElementReference::new(source_id, "n1"),
                    labels: Arc::from([Arc::from("Label")]),
                    effective_from: 0,
                },
                properties: drasi_core::models::ElementPropertyMap::new(),
            },
        };
        let mut wrapper = SourceEventWrapper::new(
            source_id.to_string(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
        );
        wrapper.source_position = position.map(|p| bytes::Bytes::from(p.to_vec()));
        wrapper
    }

    #[tokio::test]
    async fn test_dispatch_events_batch_empty_returns_ok() {
        let base = SourceBase::new(SourceBaseParams::new("batch-empty")).unwrap();
        let result = base.dispatch_events_batch(Vec::new()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_dispatch_events_batch_stamps_monotonic_sequences() {
        let params = SourceBaseParams::new("batch-seq").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();

        // Create a receiver so events are actually captured
        let mut receiver = base.create_streaming_receiver().await.unwrap();

        let events = vec![
            make_event("batch-seq", Some(b"\x01")),
            make_event("batch-seq", Some(b"\x02")),
            make_event("batch-seq", Some(b"\x03")),
        ];

        base.dispatch_events_batch(events).await.unwrap();

        let e1 = receiver.recv().await.unwrap();
        let e2 = receiver.recv().await.unwrap();
        let e3 = receiver.recv().await.unwrap();

        let s1 = e1.sequence.expect("event 1 must have sequence");
        let s2 = e2.sequence.expect("event 2 must have sequence");
        let s3 = e3.sequence.expect("event 3 must have sequence");

        assert_eq!(s2, s1 + 1, "sequences must be monotonically increasing");
        assert_eq!(s3, s2 + 1, "sequences must be monotonically increasing");
    }

    #[tokio::test]
    async fn test_dispatch_events_batch_multi_dispatcher_fanout() {
        let params =
            SourceBaseParams::new("batch-fanout").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();

        // Create two receivers (two dispatchers in channel mode)
        let mut rx1 = base.create_streaming_receiver().await.unwrap();
        let mut rx2 = base.create_streaming_receiver().await.unwrap();

        let events = vec![
            make_event("batch-fanout", Some(b"\x01")),
            make_event("batch-fanout", Some(b"\x02")),
        ];

        base.dispatch_events_batch(events).await.unwrap();

        // Both receivers should get both events
        let r1_e1 = rx1.recv().await.unwrap();
        let r1_e2 = rx1.recv().await.unwrap();
        let r2_e1 = rx2.recv().await.unwrap();
        let r2_e2 = rx2.recv().await.unwrap();

        // Same sequences across both receivers
        assert_eq!(r1_e1.sequence, r2_e1.sequence);
        assert_eq!(r1_e2.sequence, r2_e2.sequence);
    }

    #[tokio::test]
    async fn test_dispatch_events_batch_oversized_position_still_dispatches() {
        let params =
            SourceBaseParams::new("batch-oversize").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        let mut rx = base.create_streaming_receiver().await.unwrap();

        // Create an event with a position larger than MAX_SOURCE_POSITION_BYTES
        let big_pos = vec![0xAA; SourceBase::MAX_SOURCE_POSITION_BYTES + 1];
        let events = vec![make_event("batch-oversize", Some(&big_pos))];

        // Should succeed (warn but not error)
        base.dispatch_events_batch(events).await.unwrap();

        let received = rx.recv().await.unwrap();
        assert!(received.sequence.is_some(), "event must still be stamped");
        assert_eq!(
            received.source_position.as_ref().map(|p| p.len()),
            Some(SourceBase::MAX_SOURCE_POSITION_BYTES + 1),
            "oversized position must still be delivered (checkpoint layer enforces the limit)"
        );
    }

    // =========================================================================
    // Per-subscriber position filtering tests
    // =========================================================================

    #[tokio::test]
    async fn test_position_filter_suppresses_events_before_resume() {
        let params = SourceBaseParams::new("pos-filter").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        base.set_position_comparator(ByteLexPositionComparator)
            .await;

        // Create two subscribers
        let mut rx1 = base.create_streaming_receiver().await.unwrap();
        let mut rx2 = base.create_streaming_receiver().await.unwrap();

        // rx1 (dispatcher 0): resume_from = position [0x00, 0x05]
        // rx2 (dispatcher 1): no resume position (gets everything)
        base.subscriber_resume_positions
            .write()
            .await
            .insert(0, Bytes::from_static(&[0x00, 0x05]));

        // Dispatch event at position [0x00, 0x03] — before rx1's resume
        let event = make_event("pos-filter", Some(&[0x00, 0x03]));
        base.dispatch_event(event).await.unwrap();

        // rx2 should receive it (no filter)
        let r2 = tokio::time::timeout(std::time::Duration::from_millis(100), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(r2.source_position.as_ref().unwrap().as_ref(), &[0x00, 0x03]);

        // rx1 should NOT receive it (position not reached)
        let r1 = tokio::time::timeout(std::time::Duration::from_millis(50), rx1.recv()).await;
        assert!(r1.is_err(), "rx1 should timeout — event was suppressed");
    }

    #[tokio::test]
    async fn test_position_filter_delivers_events_past_resume() {
        let params = SourceBaseParams::new("pos-filter2").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        base.set_position_comparator(ByteLexPositionComparator)
            .await;

        let mut rx1 = base.create_streaming_receiver().await.unwrap();

        // resume_from = [0x00, 0x05]
        base.subscriber_resume_positions
            .write()
            .await
            .insert(0, Bytes::from_static(&[0x00, 0x05]));

        // Event at [0x00, 0x06] — past resume
        let event = make_event("pos-filter2", Some(&[0x00, 0x06]));
        base.dispatch_event(event).await.unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            received.source_position.as_ref().unwrap().as_ref(),
            &[0x00, 0x06]
        );
    }

    #[tokio::test]
    async fn test_position_filter_advances_high_water_mark() {
        let params = SourceBaseParams::new("pos-hwm").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        base.set_position_comparator(ByteLexPositionComparator)
            .await;

        let mut rx = base.create_streaming_receiver().await.unwrap();

        // resume_from = [0x00, 0x03]
        base.subscriber_resume_positions
            .write()
            .await
            .insert(0, Bytes::from_static(&[0x00, 0x03]));

        // First event at [0x00, 0x04] — past resume, advances high-water mark
        base.dispatch_event(make_event("pos-hwm", Some(&[0x00, 0x04])))
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap();

        // High-water mark should be updated (not cleared)
        {
            let positions = base.subscriber_resume_positions.read().await;
            assert_eq!(
                positions.get(&0).map(|b| b.as_ref()),
                Some([0x00, 0x04].as_slice()),
                "high-water mark should be advanced to dispatched position"
            );
        }

        // Subsequent event at LOWER position should be suppressed (rewind protection)
        base.dispatch_event(make_event("pos-hwm", Some(&[0x00, 0x01])))
            .await
            .unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            r.is_err(),
            "event below high-water mark should be suppressed after rewind"
        );

        // Event at HIGHER position should flow through
        base.dispatch_event(make_event("pos-hwm", Some(&[0x00, 0x06])))
            .await
            .unwrap();
        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            received.source_position.as_ref().unwrap().as_ref(),
            &[0x00, 0x06]
        );
    }

    #[tokio::test]
    async fn test_position_filter_equal_position_is_suppressed() {
        // resume_from is the LAST committed position, so an event at exactly
        // that position has already been processed — it should be suppressed.
        let params = SourceBaseParams::new("pos-equal").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        base.set_position_comparator(ByteLexPositionComparator)
            .await;

        let mut rx = base.create_streaming_receiver().await.unwrap();

        base.subscriber_resume_positions
            .write()
            .await
            .insert(0, Bytes::from_static(&[0x00, 0x05]));

        // Event at exactly [0x00, 0x05] — should be suppressed
        base.dispatch_event(make_event("pos-equal", Some(&[0x00, 0x05])))
            .await
            .unwrap();

        let r = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            r.is_err(),
            "event at exactly resume position should be suppressed"
        );
    }

    #[tokio::test]
    async fn test_position_filter_no_comparator_delivers_all() {
        // Without a position comparator set, all events should be delivered
        // even if there's a resume position entry.
        let params = SourceBaseParams::new("no-cmp").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        // Deliberately NOT setting a position comparator

        let mut rx = base.create_streaming_receiver().await.unwrap();

        base.subscriber_resume_positions
            .write()
            .await
            .insert(0, Bytes::from_static(&[0x00, 0x05]));

        // Event at [0x00, 0x03] — normally suppressed, but no comparator
        base.dispatch_event(make_event("no-cmp", Some(&[0x00, 0x03])))
            .await
            .unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            received.source_position.as_ref().unwrap().as_ref(),
            &[0x00, 0x03]
        );
    }

    #[tokio::test]
    async fn test_position_filter_batch_mode() {
        let params = SourceBaseParams::new("pos-batch").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        base.set_position_comparator(ByteLexPositionComparator)
            .await;

        let mut rx1 = base.create_streaming_receiver().await.unwrap();
        let mut rx2 = base.create_streaming_receiver().await.unwrap();

        // rx1 (idx 0): resume_from = [0x00, 0x05]
        // rx2 (idx 1): resume_from = [0x00, 0x02]
        {
            let mut positions = base.subscriber_resume_positions.write().await;
            positions.insert(0, Bytes::from_static(&[0x00, 0x05]));
            positions.insert(1, Bytes::from_static(&[0x00, 0x02]));
        }

        let events = vec![
            make_event("pos-batch", Some(&[0x00, 0x01])), // before both
            make_event("pos-batch", Some(&[0x00, 0x03])), // past rx2, before rx1
            make_event("pos-batch", Some(&[0x00, 0x06])), // past both
        ];
        base.dispatch_events_batch(events).await.unwrap();

        // rx2 should receive events at [0x03] and [0x06] (skipping [0x01])
        let r2_1 = tokio::time::timeout(std::time::Duration::from_millis(100), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            r2_1.source_position.as_ref().unwrap().as_ref(),
            &[0x00, 0x03]
        );

        let r2_2 = tokio::time::timeout(std::time::Duration::from_millis(100), rx2.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            r2_2.source_position.as_ref().unwrap().as_ref(),
            &[0x00, 0x06]
        );

        // rx1 should only receive event at [0x06] (skipping [0x01] and [0x03])
        let r1_1 = tokio::time::timeout(std::time::Duration::from_millis(100), rx1.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            r1_1.source_position.as_ref().unwrap().as_ref(),
            &[0x00, 0x06]
        );

        // No more events for rx1
        let r1_extra = tokio::time::timeout(std::time::Duration::from_millis(50), rx1.recv()).await;
        assert!(r1_extra.is_err(), "rx1 should have no more events");
    }

    #[tokio::test]
    async fn test_position_filter_events_without_position_delivered() {
        // Events with no source_position cannot be filtered — they pass through.
        let params = SourceBaseParams::new("pos-none").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        base.set_position_comparator(ByteLexPositionComparator)
            .await;

        let mut rx = base.create_streaming_receiver().await.unwrap();

        base.subscriber_resume_positions
            .write()
            .await
            .insert(0, Bytes::from_static(&[0x00, 0x05]));

        // Event with no source_position
        base.dispatch_event(make_event("pos-none", None))
            .await
            .unwrap();

        let received = tokio::time::timeout(std::time::Duration::from_millis(100), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(received.source_position.is_none());
    }

    // =========================================================================
    // Sequence → source position mapping tests
    // =========================================================================

    #[tokio::test]
    async fn test_sequence_position_map_populated_on_dispatch() {
        let params = SourceBaseParams::new("spm-1").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        let _rx = base.create_streaming_receiver().await.unwrap();

        let lsn: u64 = 0x1234;
        base.dispatch_event(make_event("spm-1", Some(&lsn.to_be_bytes())))
            .await
            .unwrap();

        let map = base.sequence_position_map.read().await;
        assert_eq!(map.len(), 1);
        let (seq, pos) = map.iter().next().unwrap();
        assert_eq!(*seq, 1); // first sequence
        assert_eq!(pos.as_ref(), &lsn.to_be_bytes());
    }

    #[tokio::test]
    async fn test_sequence_position_map_not_populated_without_position() {
        let params = SourceBaseParams::new("spm-none").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        let _rx = base.create_streaming_receiver().await.unwrap();

        base.dispatch_event(make_event("spm-none", None))
            .await
            .unwrap();

        let map = base.sequence_position_map.read().await;
        assert!(map.is_empty());
    }

    #[tokio::test]
    async fn test_compute_confirmed_source_position_basic() {
        let params = SourceBaseParams::new("cssp-1").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        let _rx = base.create_streaming_receiver().await.unwrap();

        // Create position handle and set confirmed sequence
        let handle = base.create_position_handle("q1").await;

        // Dispatch 3 events with known LSNs
        for lsn in [100u64, 200, 300] {
            base.dispatch_event(make_event("cssp-1", Some(&lsn.to_be_bytes())))
                .await
                .unwrap();
        }

        // Confirm up to sequence 2 (second event, LSN=200)
        handle.store(2, Ordering::Relaxed);

        let confirmed = base.compute_confirmed_source_position().await;
        assert!(confirmed.is_some());
        let lsn_bytes = confirmed.unwrap();
        assert_eq!(u64::from_be_bytes(lsn_bytes[..8].try_into().unwrap()), 200);
    }

    #[tokio::test]
    async fn test_compute_confirmed_source_position_returns_none_when_no_handles() {
        let base = SourceBase::new(SourceBaseParams::new("cssp-none")).unwrap();
        assert!(base.compute_confirmed_source_position().await.is_none());
    }

    #[tokio::test]
    async fn test_compute_confirmed_source_position_returns_none_when_all_max() {
        let base = SourceBase::new(SourceBaseParams::new("cssp-max")).unwrap();
        let _h = base.create_position_handle("q1").await;
        // h stays at u64::MAX
        assert!(base.compute_confirmed_source_position().await.is_none());
    }

    #[tokio::test]
    async fn test_compute_confirmed_source_position_min_of_two_queries() {
        let params = SourceBaseParams::new("cssp-2q").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        let _rx = base.create_streaming_receiver().await.unwrap();

        let h1 = base.create_position_handle("q1").await;
        let h2 = base.create_position_handle("q2").await;

        // Dispatch 3 events
        for lsn in [100u64, 200, 300] {
            base.dispatch_event(make_event("cssp-2q", Some(&lsn.to_be_bytes())))
                .await
                .unwrap();
        }

        // q1 confirmed seq 3 (LSN=300), q2 confirmed seq 1 (LSN=100)
        h1.store(3, Ordering::Relaxed);
        h2.store(1, Ordering::Relaxed);

        let confirmed = base.compute_confirmed_source_position().await;
        assert!(confirmed.is_some());
        let lsn_bytes = confirmed.unwrap();
        // min is seq 1 → LSN 100
        assert_eq!(u64::from_be_bytes(lsn_bytes[..8].try_into().unwrap()), 100);
    }

    #[tokio::test]
    async fn test_prune_position_map() {
        let params = SourceBaseParams::new("prune-1").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        let _rx = base.create_streaming_receiver().await.unwrap();

        for lsn in [10u64, 20, 30, 40, 50] {
            base.dispatch_event(make_event("prune-1", Some(&lsn.to_be_bytes())))
                .await
                .unwrap();
        }
        // Sequences are 1..=5
        assert_eq!(base.sequence_position_map.read().await.len(), 5);

        base.prune_position_map(3).await;
        let map = base.sequence_position_map.read().await;
        assert_eq!(map.len(), 2); // sequences 4 and 5 remain
        assert!(map.contains_key(&4));
        assert!(map.contains_key(&5));
    }

    #[tokio::test]
    async fn test_prune_position_map_all() {
        let params = SourceBaseParams::new("prune-all").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        let _rx = base.create_streaming_receiver().await.unwrap();

        for lsn in [10u64, 20] {
            base.dispatch_event(make_event("prune-all", Some(&lsn.to_be_bytes())))
                .await
                .unwrap();
        }
        base.prune_position_map(100).await; // prune beyond last seq
        assert!(base.sequence_position_map.read().await.is_empty());
    }

    #[tokio::test]
    async fn test_position_handle_initialized_to_last_sequence() {
        use crate::config::SourceSubscriptionSettings;

        let params = SourceBaseParams::new("ph-init").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();

        let settings = SourceSubscriptionSettings {
            query_id: "q1".to_string(),
            source_id: "ph-init".to_string(),
            enable_bootstrap: false,
            resume_from: Some(Bytes::from_static(&[0x00, 0x01])),
            last_sequence: Some(42),
            request_position_handle: true,
            nodes: Default::default(),
            relations: Default::default(),
        };

        let response = base
            .subscribe_with_bootstrap(&settings, "test")
            .await
            .unwrap();
        let handle = response.position_handle.expect("should have handle");
        // Handle should be initialized to last_sequence, not u64::MAX
        assert_eq!(handle.load(Ordering::Relaxed), 42);
    }

    #[tokio::test]
    async fn test_position_handle_stays_max_without_last_sequence() {
        use crate::config::SourceSubscriptionSettings;

        let params = SourceBaseParams::new("ph-no-ls").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();

        let settings = SourceSubscriptionSettings {
            query_id: "q1".to_string(),
            source_id: "ph-no-ls".to_string(),
            enable_bootstrap: true,
            resume_from: None,
            last_sequence: None,
            request_position_handle: true,
            nodes: Default::default(),
            relations: Default::default(),
        };

        let response = base
            .subscribe_with_bootstrap(&settings, "test")
            .await
            .unwrap();
        let handle = response.position_handle.expect("should have handle");
        // No last_sequence → handle stays at u64::MAX
        assert_eq!(handle.load(Ordering::Relaxed), u64::MAX);
    }

    #[tokio::test]
    async fn test_batch_dispatch_populates_sequence_position_map() {
        let params = SourceBaseParams::new("spm-batch").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        let _rx = base.create_streaming_receiver().await.unwrap();

        let events = vec![
            make_event("spm-batch", Some(&100u64.to_be_bytes())),
            make_event("spm-batch", Some(&200u64.to_be_bytes())),
            make_event("spm-batch", None), // no position
        ];

        base.dispatch_events_batch(events).await.unwrap();

        let map = base.sequence_position_map.read().await;
        // Only 2 entries (the one without position is skipped)
        assert_eq!(map.len(), 2);
        assert!(map.contains_key(&1));
        assert!(map.contains_key(&2));
    }

    #[tokio::test]
    async fn test_position_filter_rewind_protection_multi_subscriber() {
        // Simulates the Postgres scenario: subscriber A joins, processes events,
        // then subscriber B joins and the source rewinds the stream.
        // Subscriber A should NOT see replayed events thanks to the
        // persistent high-water mark.
        let params = SourceBaseParams::new("rewind").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        base.set_position_comparator(ByteLexPositionComparator)
            .await;

        // Subscriber A joins with resume_from at [0x10]
        let mut rx_a = base.create_streaming_receiver().await.unwrap();
        base.subscriber_resume_positions
            .write()
            .await
            .insert(0, Bytes::from_static(&[0x10]));

        // Source dispatches events at [0x20] and [0x30] — both past A's resume
        base.dispatch_event(make_event("rewind", Some(&[0x20])))
            .await
            .unwrap();
        let ev = rx_a.recv().await.unwrap();
        assert_eq!(ev.source_position.as_ref().unwrap().as_ref(), &[0x20]);

        base.dispatch_event(make_event("rewind", Some(&[0x30])))
            .await
            .unwrap();
        let ev = rx_a.recv().await.unwrap();
        assert_eq!(ev.source_position.as_ref().unwrap().as_ref(), &[0x30]);

        // Subscriber B joins with resume_from at [0x10]
        let mut rx_b = base.create_streaming_receiver().await.unwrap();
        base.subscriber_resume_positions
            .write()
            .await
            .insert(1, Bytes::from_static(&[0x10]));

        // Source REWINDS — replays from [0x20] again
        // A should NOT see these (high-water is at [0x30])
        // B should see [0x20] (past its resume_from [0x10])
        base.dispatch_event(make_event("rewind", Some(&[0x20])))
            .await
            .unwrap();

        // A should NOT receive the replayed event
        let r = tokio::time::timeout(std::time::Duration::from_millis(50), rx_a.recv()).await;
        assert!(
            r.is_err(),
            "subscriber A should not see replayed event at [0x20]"
        );

        // B SHOULD receive it (it's past B's resume_from)
        let ev = tokio::time::timeout(std::time::Duration::from_millis(100), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.source_position.as_ref().unwrap().as_ref(), &[0x20]);

        // Replay [0x30] — A should NOT see it, B should
        base.dispatch_event(make_event("rewind", Some(&[0x30])))
            .await
            .unwrap();

        let r = tokio::time::timeout(std::time::Duration::from_millis(50), rx_a.recv()).await;
        assert!(
            r.is_err(),
            "subscriber A should not see replayed event at [0x30]"
        );

        let ev = tokio::time::timeout(std::time::Duration::from_millis(100), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev.source_position.as_ref().unwrap().as_ref(), &[0x30]);

        // New event [0x40] — BOTH should see it
        base.dispatch_event(make_event("rewind", Some(&[0x40])))
            .await
            .unwrap();

        let ev_a = tokio::time::timeout(std::time::Duration::from_millis(100), rx_a.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev_a.source_position.as_ref().unwrap().as_ref(), &[0x40]);

        let ev_b = tokio::time::timeout(std::time::Duration::from_millis(100), rx_b.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ev_b.source_position.as_ref().unwrap().as_ref(), &[0x40]);
    }

    #[tokio::test]
    async fn test_high_water_mark_set_for_new_subscriber_without_resume() {
        // A subscriber without initial resume_from should get a high-water
        // mark after its first event, protecting it from future rewinds.
        let params = SourceBaseParams::new("hwm-new").with_dispatch_mode(DispatchMode::Channel);
        let base = SourceBase::new(params).unwrap();
        base.set_position_comparator(ByteLexPositionComparator)
            .await;

        let mut rx = base.create_streaming_receiver().await.unwrap();
        // No resume_from set

        // Dispatch event — should be delivered (no filter)
        base.dispatch_event(make_event("hwm-new", Some(&[0x10])))
            .await
            .unwrap();
        let _ = rx.recv().await.unwrap();

        // Now the high-water mark should be set at [0x10]
        {
            let positions = base.subscriber_resume_positions.read().await;
            assert_eq!(
                positions.get(&0).map(|b| b.as_ref()),
                Some([0x10].as_slice()),
                "high-water mark should be set after first dispatch"
            );
        }

        // Rewind: event at [0x05] should be suppressed
        base.dispatch_event(make_event("hwm-new", Some(&[0x05])))
            .await
            .unwrap();
        let r = tokio::time::timeout(std::time::Duration::from_millis(50), rx.recv()).await;
        assert!(
            r.is_err(),
            "event below high-water mark should be suppressed"
        );
    }
}
