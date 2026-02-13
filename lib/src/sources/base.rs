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
use log::{debug, error, info};
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::Instrument;

use crate::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use crate::channels::*;
use crate::context::SourceRuntimeContext;
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
    /// Current component status
    pub status: Arc<RwLock<ComponentStatus>>,
    /// Dispatchers for sending source events to subscribers
    ///
    /// This is a vector of dispatchers that send source events to all registered
    /// subscribers (queries). When a source produces a change event, it broadcasts
    /// it to all dispatchers in this vector.
    pub dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    /// Runtime context (set by initialize())
    context: Arc<RwLock<Option<SourceRuntimeContext>>>,
    /// Channel for sending component status events (extracted from context for convenience)
    status_tx: Arc<RwLock<Option<ComponentEventSender>>>,
    /// State store provider (extracted from context for convenience)
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    /// Handle to the source's main task
    pub task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Sender for shutdown signal
    pub shutdown_tx: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
    /// Optional bootstrap provider - plugins set this if they support bootstrap
    bootstrap_provider: Arc<RwLock<Option<Arc<dyn BootstrapProvider>>>>,
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
            id: params.id,
            dispatch_mode,
            dispatch_buffer_capacity,
            auto_start: params.auto_start,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            dispatchers: Arc::new(RwLock::new(dispatchers)),
            context: Arc::new(RwLock::new(None)), // Set by initialize()
            status_tx: Arc::new(RwLock::new(None)), // Extracted from context
            state_store: Arc::new(RwLock::new(None)), // Extracted from context
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
            bootstrap_provider: Arc::new(RwLock::new(bootstrap_provider)),
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
    /// - `status_tx`: Channel for reporting component status events
    /// - `state_store`: Optional persistent state storage
    pub async fn initialize(&self, context: SourceRuntimeContext) {
        // Store context for later use
        *self.context.write().await = Some(context.clone());

        // Extract services for convenience
        *self.status_tx.write().await = Some(context.status_tx.clone());

        if let Some(state_store) = context.state_store.as_ref() {
            *self.state_store.write().await = Some(state_store.clone());
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

    /// Get the status channel Arc for internal use by spawned tasks
    ///
    /// This returns the internal status_tx wrapped in Arc<RwLock<Option<...>>>
    /// which allows background tasks to send component status events.
    ///
    /// Returns a clone of the Arc that can be moved into spawned tasks.
    pub fn status_tx(&self) -> Arc<RwLock<Option<ComponentEventSender>>> {
        self.status_tx.clone()
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
            status: self.status.clone(),
            dispatchers: self.dispatchers.clone(),
            context: self.context.clone(),
            status_tx: self.status_tx.clone(),
            state_store: self.state_store.clone(),
            task_handle: self.task_handle.clone(),
            shutdown_tx: self.shutdown_tx.clone(),
            bootstrap_provider: self.bootstrap_provider.clone(),
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
            "Query '{}' subscribing to {} source '{}' (bootstrap: {})",
            settings.query_id, source_type, self.id, settings.enable_bootstrap
        );

        // Create streaming receiver using helper method
        let receiver = self.create_streaming_receiver().await?;

        let query_id_for_response = settings.query_id.clone();

        // Handle bootstrap if requested and bootstrap provider is configured
        let bootstrap_receiver = if settings.enable_bootstrap {
            self.handle_bootstrap_subscription(settings, source_type)
                .await?
        } else {
            None
        };

        Ok(SubscriptionResponse {
            query_id: query_id_for_response,
            source_id: self.id.clone(),
            receiver,
            bootstrap_receiver,
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

            // Spawn bootstrap task with tracing span for log routing
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
                        Ok(count) => {
                            info!(
                                "Bootstrap completed successfully for query '{}', sent {count} events",
                                settings_clone.query_id
                            );
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
            
            // Yield to allow the spawned task to start
            tokio::task::yield_now().await;

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

    /// Create a test subscription to this source (synchronous wrapper)
    ///
    /// This method is intended for use in tests to receive events from the source.
    /// It properly handles both Broadcast and Channel dispatch modes by delegating
    /// to `create_streaming_receiver()`, making the dispatch mode transparent to tests.
    ///
    /// Note: This is a synchronous wrapper that uses `tokio::task::block_in_place` internally.
    /// For async contexts, prefer calling `create_streaming_receiver()` directly.
    ///
    /// # Returns
    /// A receiver that will receive all events dispatched by this source
    ///
    /// # Panics
    /// Panics if the receiver cannot be created (e.g., internal error)
    pub fn test_subscribe(&self) -> Box<dyn ChangeReceiver<SourceEventWrapper>> {
        // Use block_in_place to avoid nested executor issues in async tests
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.create_streaming_receiver())
        })
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
        if let Some(handle) = self.task_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    info!("Source '{}' task completed successfully", self.id);
                }
                Ok(Err(e)) => {
                    error!("Source '{}' task panicked: {}", self.id, e);
                }
                Err(_) => {
                    error!("Source '{}' task did not complete within timeout", self.id);
                }
            }
        }

        *self.status.write().await = ComponentStatus::Stopped;
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

    /// Get the current status
    pub async fn get_status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    /// Set the current status
    pub async fn set_status(&self, status: ComponentStatus) {
        *self.status.write().await = status;
    }

    /// Transition to a new status and send event
    pub async fn set_status_with_event(
        &self,
        status: ComponentStatus,
        message: Option<String>,
    ) -> Result<()> {
        *self.status.write().await = status.clone();
        self.send_component_event(status, message).await
    }

    /// Set the task handle
    pub async fn set_task_handle(&self, handle: tokio::task::JoinHandle<()>) {
        *self.task_handle.write().await = Some(handle);
    }

    /// Set the shutdown sender
    pub async fn set_shutdown_tx(&self, tx: tokio::sync::oneshot::Sender<()>) {
        *self.shutdown_tx.write().await = Some(tx);
    }

    /// Send a component event
    ///
    /// If the status channel has not been initialized yet, this method silently
    /// succeeds without sending anything. This allows sources to be used
    /// in a standalone fashion without DrasiLib if needed.
    pub async fn send_component_event(
        &self,
        status: ComponentStatus,
        message: Option<String>,
    ) -> Result<()> {
        let event = ComponentEvent {
            component_id: self.id.clone(),
            component_type: ComponentType::Source,
            status,
            timestamp: chrono::Utc::now(),
            message,
        };

        if let Some(ref tx) = *self.status_tx.read().await {
            if let Err(e) = tx.send(event).await {
                error!("Failed to send component event: {e}");
            }
        }
        // If status_tx is None, silently skip - initialization happens before start()
        Ok(())
    }
}
