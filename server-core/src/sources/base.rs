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

use anyhow::Result;
use log::{debug, error, info};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::bootstrap::{BootstrapContext, BootstrapProviderFactory, BootstrapRequest};
use crate::channels::*;
use crate::config::SourceConfig;
use crate::profiling;
use drasi_core::models::SourceChange;

/// Base implementation for common source functionality
pub struct SourceBase {
    /// Source configuration
    pub config: SourceConfig,
    /// Current component status
    pub status: Arc<RwLock<ComponentStatus>>,
    /// Dispatchers for sending source events to subscribers
    pub dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    /// Channel for sending component lifecycle events
    pub event_tx: ComponentEventSender,
    /// Handle to the source's main task
    pub task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    /// Sender for shutdown signal
    pub shutdown_tx: Arc<RwLock<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl SourceBase {
    /// Create a new SourceBase with the given configuration
    pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
        // Determine dispatch mode (default to Channel if not specified)
        let dispatch_mode = config.dispatch_mode.unwrap_or_default();

        // Set up initial dispatchers based on dispatch mode
        let mut dispatchers: Vec<Box<dyn ChangeDispatcher<SourceEventWrapper> + Send + Sync>> = Vec::new();

        if dispatch_mode == DispatchMode::Broadcast {
            // For broadcast mode, create a single broadcast dispatcher
            let capacity = config.dispatch_buffer_capacity.unwrap_or(1000);
            let dispatcher = BroadcastChangeDispatcher::<SourceEventWrapper>::new(capacity);
            dispatchers.push(Box::new(dispatcher));
        }
        // For channel mode, dispatchers will be created on-demand when subscribing

        Ok(Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            dispatchers: Arc::new(RwLock::new(dispatchers)),
            event_tx,
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
        })
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
        let dispatch_mode = self.config.dispatch_mode.unwrap_or_default();

        let receiver: Box<dyn ChangeReceiver<SourceEventWrapper>> = match dispatch_mode {
            DispatchMode::Broadcast => {
                // For broadcast mode, use the single dispatcher
                let dispatchers = self.dispatchers.read().await;
                if let Some(dispatcher) = dispatchers.first() {
                    dispatcher.create_receiver()?
                } else {
                    return Err(anyhow::anyhow!("No broadcast dispatcher available"));
                }
            }
            DispatchMode::Channel => {
                // For channel mode, create a new dispatcher for this subscription
                let capacity = self.config.dispatch_buffer_capacity.unwrap_or(1000);
                let dispatcher = ChannelChangeDispatcher::<SourceEventWrapper>::new(capacity);
                let receiver = dispatcher.create_receiver()?;

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
    /// - Setting up bootstrap if requested and a provider is configured
    /// - Returning the appropriate SubscriptionResponse
    pub async fn subscribe_with_bootstrap(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
        source_type: &str,
    ) -> Result<SubscriptionResponse> {
        info!(
            "Query '{}' subscribing to {} source '{}' (bootstrap: {})",
            query_id, source_type, self.config.id, enable_bootstrap
        );

        // Create streaming receiver using helper method
        let receiver = self.create_streaming_receiver().await?;

        let query_id_for_response = query_id.clone();

        // Handle bootstrap if requested and bootstrap provider is configured
        let bootstrap_receiver = if enable_bootstrap {
            self.handle_bootstrap_subscription(query_id, node_labels, relation_labels, source_type)
                .await?
        } else {
            None
        };

        Ok(SubscriptionResponse {
            query_id: query_id_for_response,
            source_id: self.config.id.clone(),
            receiver,
            bootstrap_receiver,
        })
    }

    /// Handle bootstrap subscription logic
    async fn handle_bootstrap_subscription(
        &self,
        query_id: String,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
        source_type: &str,
    ) -> Result<Option<BootstrapEventReceiver>> {
        if let Some(provider_config) = &self.config.bootstrap_provider {
            info!(
                "Creating bootstrap provider for query '{}' on {} source '{}'",
                query_id, source_type, self.config.id
            );

            // Create bootstrap context with Arc-wrapped source config
            let context = BootstrapContext::new(
                self.config.id.clone(), // server_id
                Arc::new(self.config.clone()),
                self.config.id.clone(),
            );

            // Create the bootstrap provider using the factory
            let provider = BootstrapProviderFactory::create_provider(provider_config)?;

            // Create bootstrap channel
            let (bootstrap_tx, bootstrap_rx) = tokio::sync::mpsc::channel(1000);

            // Create bootstrap request with request_id
            let request = BootstrapRequest {
                query_id: query_id.clone(),
                node_labels,
                relation_labels,
                request_id: format!("{}-{}", query_id, uuid::Uuid::new_v4()),
            };

            // Spawn bootstrap task
            tokio::spawn(async move {
                match provider.bootstrap(request, &context, bootstrap_tx).await {
                    Ok(count) => {
                        info!(
                            "Bootstrap completed successfully for query '{}', sent {} events",
                            query_id, count
                        );
                    }
                    Err(e) => {
                        error!("Bootstrap failed for query '{}': {}", query_id, e);
                    }
                }
            });

            Ok(Some(bootstrap_rx))
        } else {
            info!(
                "Bootstrap requested for query '{}' but no bootstrap provider configured for {} source '{}'",
                query_id, source_type, self.config.id
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
            self.config.id.clone(),
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
        debug!("[{}] Dispatching event: {:?}", self.config.id, &wrapper);

        // Arc-wrap for zero-copy sharing across dispatchers
        let arc_wrapper = Arc::new(wrapper);

        // Send to all dispatchers
        let dispatchers = self.dispatchers.read().await;
        for dispatcher in dispatchers.iter() {
            if let Err(e) = dispatcher.dispatch_change(arc_wrapper.clone()).await {
                debug!(
                    "[{}] Failed to dispatch event: {}",
                    self.config.id, e
                );
            }
        }

        Ok(())
    }

    /// Broadcast SourceControl events
    pub async fn broadcast_control(&self, control: SourceControl) -> Result<()> {
        let wrapper = SourceEventWrapper::new(
            self.config.id.clone(),
            SourceEvent::Control(control),
            chrono::Utc::now(),
        );
        self.dispatch_event(wrapper).await
    }

    /// Handle common stop functionality
    pub async fn stop_common(&self) -> Result<()> {
        info!("Stopping source '{}'", self.config.id);

        // Send shutdown signal if we have one
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }

        // Wait for task to complete
        if let Some(handle) = self.task_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {
                    info!("Source '{}' task completed successfully", self.config.id);
                }
                Ok(Err(e)) => {
                    error!("Source '{}' task panicked: {}", self.config.id, e);
                }
                Err(_) => {
                    error!(
                        "Source '{}' task did not complete within timeout",
                        self.config.id
                    );
                }
            }
        }

        *self.status.write().await = ComponentStatus::Stopped;
        info!("Source '{}' stopped", self.config.id);
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

    /// Set the task handle
    pub async fn set_task_handle(&self, handle: tokio::task::JoinHandle<()>) {
        *self.task_handle.write().await = Some(handle);
    }

    /// Set the shutdown sender
    pub async fn set_shutdown_tx(&self, tx: tokio::sync::oneshot::Sender<()>) {
        *self.shutdown_tx.write().await = Some(tx);
    }

    /// Send a component event
    pub async fn send_component_event(
        &self,
        status: ComponentStatus,
        message: Option<String>,
    ) -> Result<()> {
        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Source,
            status,
            timestamp: chrono::Utc::now(),
            message,
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }
        Ok(())
    }
}