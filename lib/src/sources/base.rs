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
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::sync::RwLock;
use tracing::Instrument;

use crate::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use crate::channels::*;
use crate::component_graph::ComponentStatusHandle;
use crate::context::SourceRuntimeContext;
use crate::identity::IdentityProvider;
use crate::position::SequencePosition;
use crate::profiling;
use crate::state_store::StateStoreProvider;
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
    /// Keyed by `query_id`. Values are the same `Arc<Mutex<Option<SequencePosition>>>`
    /// returned in `SubscriptionResponse::position_handle`. The query writes its last
    /// durably-processed sequence; the source reads `compute_confirmed_position()`
    /// to advance its upstream cursor. Initial value is `None`
    /// ("no position confirmed yet"). Only populated when
    /// `request_position_handle == true`.
    position_handles: Arc<RwLock<HashMap<String, Arc<Mutex<Option<SequencePosition>>>>>>,
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

        // Initialize bootstrap provider if provided (no async needed at construction time)
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
            state_store: Arc::new(RwLock::new(None)), // Extracted from context
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
            bootstrap_provider: Arc::new(RwLock::new(bootstrap_provider)),
            identity_provider: Arc::new(RwLock::new(None)),
            position_handles: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Get whether this source should auto-start
    pub fn get_auto_start(&self) -> bool {
        self.auto_start
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

        if let Some(state_store) = context.state_store.as_ref() {
            *self.state_store.write().await = Some(state_store.clone());
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

    /// Create and register a position handle for `query_id`, initialized to `None`.
    ///
    /// Returns the shared handle; the same `Arc` is placed in
    /// `SubscriptionResponse::position_handle` so the query and the source share
    /// one mutex-guarded position. If a handle already exists for `query_id`
    /// (re-subscribe after transient disconnect), the existing handle is returned
    /// to preserve any position the query had previously reported.
    pub async fn create_position_handle(
        &self,
        query_id: &str,
    ) -> Arc<Mutex<Option<SequencePosition>>> {
        let mut handles = self.position_handles.write().await;
        if let Some(existing) = handles.get(query_id) {
            return existing.clone();
        }
        let handle = Arc::new(Mutex::new(None));
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
    /// handle is still `None` (no subscriber has confirmed a position yet —
    /// typically because they are still bootstrapping). Otherwise returns the
    /// minimum confirmed position, suitable for advancing the source's
    /// upstream cursor (Postgres `flush_lsn`, Kafka commit, transient WAL prune
    /// threshold).
    ///
    /// Piggy-backs `cleanup_stale_handles()` so dropped subscribers do not pin
    /// the watermark indefinitely.
    pub async fn compute_confirmed_position(&self) -> Option<SequencePosition> {
        self.cleanup_stale_handles().await;
        let handles = self.position_handles.read().await;
        let mut min: Option<SequencePosition> = None;
        for handle in handles.values() {
            let guard = match handle.lock() {
                Ok(guard) => guard,
                Err(poisoned) => {
                    log::warn!(
                        "position handle mutex poisoned while computing confirmed position; recovering inner value"
                    );
                    poisoned.into_inner()
                }
            };
            if let Some(pos) = guard.as_ref() {
                min = Some(match min {
                    None => *pos,
                    Some(m) => {
                        if *pos < m {
                            *pos
                        } else {
                            m
                        }
                    }
                });
            }
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

    /// Returns a clonable [`ComponentStatusHandle`] for use in spawned tasks.
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

        Ok(receiver)
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

        let query_id_for_response = settings.query_id.clone();

        // resume_from overrides bootstrap: a resuming query already has base
        // state in its persistent index and just needs replay from the
        // requested sequence. Re-bootstrapping would corrupt that state.
        let bootstrap_receiver = if settings.resume_from.is_some() {
            info!(
                "Query '{}' resuming from sequence {:?}; skipping bootstrap on {} source '{}'",
                settings.query_id, settings.resume_from, source_type, self.id
            );
            None
        } else if settings.enable_bootstrap {
            self.handle_bootstrap_subscription(settings, source_type)
                .await?
        } else {
            None
        };

        // Only persistent (replay-capable) queries request a handle. Volatile
        // queries are deliberately excluded from the min-watermark so they
        // cannot pin upstream advancement.
        let position_handle = if settings.request_position_handle {
            Some(self.create_position_handle(&settings.query_id).await)
        } else {
            None
        };

        Ok(SubscriptionResponse {
            query_id: query_id_for_response,
            source_id: self.id.clone(),
            receiver,
            bootstrap_receiver,
            position_handle,
        })
    }

    /// Handle bootstrap subscription logic
    async fn handle_bootstrap_subscription(
        &self,
        settings: &crate::config::SourceSubscriptionSettings,
        source_type: &str,
    ) -> Result<Option<BootstrapEventReceiver>> {
        let provider_guard = self.bootstrap_provider.read().await;
        if let Some(provider) = provider_guard.clone() {
            drop(provider_guard); // Release lock before spawning task

            info!(
                "Creating bootstrap for query '{}' on {} source '{}'",
                settings.query_id, source_type, self.id
            );

            // Create bootstrap context
            let context = BootstrapContext::new_minimal(
                self.id.clone(), // server_id
                self.id.clone(), // source_id
            );

            // Create bootstrap channel
            let (bootstrap_tx, bootstrap_rx) = tokio::sync::mpsc::channel(1000);

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
                    match provider
                        .bootstrap(request, &context, bootstrap_tx, Some(&settings_clone))
                        .await
                    {
                        Ok(result) => {
                            info!(
                                "Bootstrap completed successfully for query '{}', sent {} events",
                                settings_clone.query_id, result.event_count
                            );
                            // `result.last_sequence` / `result.sequences_aligned`
                            // are intentionally unused at this call site — a
                            // future query-processor integration issue will
                            // plumb them through to the handover protocol.
                        }
                        Err(e) => {
                            error!(
                                "Bootstrap failed for query '{}': {e}",
                                settings_clone.query_id
                            );
                        }
                    }
                }
                .instrument(span),
            );

            Ok(Some(bootstrap_rx))
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

    /// Dispatch a SourceEventWrapper to all subscribers
    ///
    /// This is a generic method for dispatching any SourceEvent.
    /// It handles Arc-wrapping for zero-copy sharing and logs
    /// when there are no subscribers.
    pub async fn dispatch_event(&self, wrapper: SourceEventWrapper) -> Result<()> {
        debug!("[{}] Dispatching event: {:?}", self.id, &wrapper);

        // Arc-wrap for zero-copy sharing across dispatchers
        let arc_wrapper = Arc::new(wrapper);

        // Send to all dispatchers
        let dispatchers = self.dispatchers.read().await;
        for dispatcher in dispatchers.iter() {
            if let Err(e) = dispatcher.dispatch_change(arc_wrapper.clone()).await {
                debug!("[{}] Failed to dispatch event: {}", self.id, e);
            }
        }

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

    /// Create a test subscription to this source (synchronous, fallible)
    ///
    /// This method is intended for use in tests to receive events from the source.
    /// It properly handles both Broadcast and Channel dispatch modes by delegating
    /// to `create_streaming_receiver()`, making the dispatch mode transparent to tests.
    ///
    /// Note: This is a synchronous wrapper that uses `tokio::task::block_in_place` internally.
    /// For async contexts, prefer calling `create_streaming_receiver()` directly.
    ///
    /// # Returns
    /// A receiver that will receive all events dispatched by this source,
    /// or an error if the receiver cannot be created.
    pub fn try_test_subscribe(
        &self,
    ) -> anyhow::Result<Box<dyn ChangeReceiver<SourceEventWrapper>>> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.create_streaming_receiver())
        })
    }

    /// Create a test subscription to this source (synchronous wrapper)
    ///
    /// Convenience wrapper around [`try_test_subscribe`](Self::try_test_subscribe)
    /// that panics on failure. Prefer `try_test_subscribe()` in new code.
    ///
    /// # Panics
    /// Panics if the receiver cannot be created.
    pub fn test_subscribe(&self) -> Box<dyn ChangeReceiver<SourceEventWrapper>> {
        self.try_test_subscribe()
            .expect("Failed to create test subscription receiver")
    }

    /// Helper function to dispatch events from spawned tasks
    ///
    /// This is a static helper that can be used from spawned async tasks that don't
    /// have access to `self`. It manually iterates through dispatchers and sends the event.
    ///
    /// For code that has access to `&self`, prefer using `dispatch_event()` instead.
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

        self.set_status(
            ComponentStatus::Stopped,
            Some(format!("Source '{}' stopped", self.id)),
        )
        .await;
        info!("Source '{}' stopped", self.id);
        Ok(())
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
    use crate::SequencePosition;
    use async_trait::async_trait;

    fn make_settings(
        query_id: &str,
        enable_bootstrap: bool,
        resume_from: Option<SequencePosition>,
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
    async fn test_create_position_handle_initializes_to_none() {
        let base = SourceBase::new(SourceBaseParams::new("ph-init")).unwrap();
        let handle = base.create_position_handle("q1").await;
        assert!(handle.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn test_create_position_handle_idempotent_for_same_query() {
        let base = SourceBase::new(SourceBaseParams::new("ph-idem")).unwrap();
        let h1 = base.create_position_handle("q1").await;
        *h1.lock().unwrap() = Some(SequencePosition::from_u64(123));
        let h2 = base.create_position_handle("q1").await;
        // Same Arc — preserves the previously reported position.
        assert!(Arc::ptr_eq(&h1, &h2));
        assert_eq!(*h2.lock().unwrap(), Some(SequencePosition::from_u64(123)));
    }

    #[tokio::test]
    async fn test_remove_position_handle_drops_entry() {
        let base = SourceBase::new(SourceBaseParams::new("ph-rm")).unwrap();
        let handle = base.create_position_handle("q1").await;
        *handle.lock().unwrap() = Some(SequencePosition::from_u64(42));
        assert_eq!(
            base.compute_confirmed_position().await,
            Some(SequencePosition::from_u64(42))
        );
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
    async fn test_compute_confirmed_position_returns_none_when_all_unset() {
        let base = SourceBase::new(SourceBaseParams::new("ph-all-none")).unwrap();
        let _h1 = base.create_position_handle("q1").await;
        let _h2 = base.create_position_handle("q2").await;
        assert_eq!(base.compute_confirmed_position().await, None);
    }

    #[tokio::test]
    async fn test_compute_confirmed_position_filters_none_returns_min() {
        let base = SourceBase::new(SourceBaseParams::new("ph-min")).unwrap();
        let h1 = base.create_position_handle("q1").await;
        let _h2 = base.create_position_handle("q2").await; // stays None
        let h3 = base.create_position_handle("q3").await;
        *h1.lock().unwrap() = Some(SequencePosition::from_u64(100));
        *h3.lock().unwrap() = Some(SequencePosition::from_u64(50));
        assert_eq!(
            base.compute_confirmed_position().await,
            Some(SequencePosition::from_u64(50))
        );
    }

    #[tokio::test]
    async fn test_compute_confirmed_position_single_real_value() {
        let base = SourceBase::new(SourceBaseParams::new("ph-single")).unwrap();
        let h1 = base.create_position_handle("q1").await;
        let _h2 = base.create_position_handle("q2").await;
        *h1.lock().unwrap() = Some(SequencePosition::from_u64(7));
        assert_eq!(
            base.compute_confirmed_position().await,
            Some(SequencePosition::from_u64(7))
        );
    }

    #[tokio::test]
    async fn test_cleanup_stale_handles_drops_orphaned_arc() {
        let base = SourceBase::new(SourceBaseParams::new("ph-stale")).unwrap();
        {
            let handle = base.create_position_handle("q1").await;
            *handle.lock().unwrap() = Some(SequencePosition::from_u64(99));
            // handle (the external clone) drops here.
        }
        base.cleanup_stale_handles().await;
        assert_eq!(base.compute_confirmed_position().await, None);
    }

    #[tokio::test]
    async fn test_cleanup_stale_handles_keeps_held_arc() {
        let base = SourceBase::new(SourceBaseParams::new("ph-held")).unwrap();
        let handle = base.create_position_handle("q1").await;
        *handle.lock().unwrap() = Some(SequencePosition::from_u64(11));
        base.cleanup_stale_handles().await;
        // External clone still alive, entry retained.
        assert_eq!(
            base.compute_confirmed_position().await,
            Some(SequencePosition::from_u64(11))
        );
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
        assert!(handle.lock().unwrap().is_none());
        // Internal map should also have the entry (so min-watermark sees it).
        assert_eq!(base.compute_confirmed_position().await, None); // None → None
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
        *handle.lock().unwrap() = Some(SequencePosition::from_u64(42));
        assert_eq!(
            base.compute_confirmed_position().await,
            Some(SequencePosition::from_u64(42))
        );
    }

    #[tokio::test]
    async fn test_subscribe_with_resume_from_skips_bootstrap() {
        let base = make_base_with_bootstrap("sub-resume");
        let response = base
            .subscribe_with_bootstrap(
                &make_settings("q1", true, Some(SequencePosition::from_u64(100)), false),
                "test",
            )
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
        let response = base
            .subscribe_with_bootstrap(
                &make_settings("q1", false, Some(SequencePosition::from_u64(100)), false),
                "test",
            )
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
}
