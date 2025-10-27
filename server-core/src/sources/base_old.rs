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
//! - Broadcast channel setup and management
//! - Bootstrap subscription handling
//! - Event dispatching with profiling
//! - Component lifecycle management
//!
//! ## Usage
//!
//! Source implementations should create a `SourceBase` instance and use
//! its methods to handle common operations:
//!
//! ```ignore
//! use drasi_server_core::sources::base::SourceBase;
//! use drasi_server_core::config::SourceConfig;
//! use drasi_server_core::channels::ComponentEventSender;
//! use anyhow::Result;
//!
//! pub struct MySource {
//!     base: SourceBase,
//!     // ... source-specific fields
//! }
//!
//! impl MySource {
//!     pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
//!         let base = SourceBase::new(config, event_tx)?;
//!         Ok(Self { base /* ... */ })
//!     }
//! }
//! ```

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
    /// Broadcast channel for sending source events to subscribers
    pub broadcast_tx: SourceBroadcastSender,
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
        // Set up broadcast channel with configured capacity
        let capacity = config.dispatch_buffer_capacity.unwrap_or(1000);
        let (broadcast_tx, _) = tokio::sync::broadcast::channel(capacity);

        Ok(Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            broadcast_tx,
            event_tx,
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(RwLock::new(None)),
        })
    }

    /// Get a broadcast receiver for subscribing to source events
    pub fn get_broadcast_receiver(&self) -> Result<SourceBroadcastReceiver> {
        Ok(self.broadcast_tx.subscribe())
    }

    /// Subscribe to this source with optional bootstrap
    ///
    /// This is the standard subscribe implementation that all sources can use.
    /// It handles:
    /// - Creating a broadcast receiver for streaming events
    /// - Setting up bootstrap if requested and a provider is configured
    /// - Returning the appropriate SubscriptionResponse
    ///
    /// # Arguments
    /// * `query_id` - ID of the subscribing query
    /// * `enable_bootstrap` - Whether to request bootstrap data
    /// * `node_labels` - Node labels to bootstrap
    /// * `relation_labels` - Relation labels to bootstrap
    /// * `source_type` - Name of the source type for logging (e.g., "gRPC", "HTTP")
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

        let broadcast_receiver = self.broadcast_tx.subscribe();
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
            broadcast_receiver,
            bootstrap_receiver,
        })
    }

    /// Handle bootstrap subscription logic
    ///
    /// This method encapsulates the ~80 lines of identical bootstrap logic
    /// that was duplicated across all source implementations.
    async fn handle_bootstrap_subscription(
        &self,
        query_id: String,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
        source_type: &str,
    ) -> Result<Option<BootstrapEventReceiver>> {
        if let Some(provider_config) = &self.config.bootstrap_provider {
            info!(
                "Bootstrap enabled for query '{}' with {} node labels and {} relation labels, delegating to bootstrap provider",
                query_id,
                node_labels.len(),
                relation_labels.len()
            );

            // Create channel for bootstrap events
            let (tx, rx) = tokio::sync::mpsc::channel(1000);

            // Create bootstrap provider
            let provider = BootstrapProviderFactory::create_provider(provider_config)?;

            // Create bootstrap context
            let context = BootstrapContext::new(
                self.config.id.clone(),
                Arc::new(self.config.clone()),
                self.config.id.clone(),
            );

            // Create bootstrap request
            let request = BootstrapRequest {
                query_id: query_id.clone(),
                node_labels,
                relation_labels,
                request_id: format!("{}-{}", query_id, uuid::Uuid::new_v4()),
            };

            // Spawn bootstrap task
            tokio::spawn(async move {
                match provider.bootstrap(request, &context, tx).await {
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

            Ok(Some(rx))
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
    /// - Broadcasting to all subscribers
    /// - Handling the no-subscriber case gracefully
    pub fn dispatch_source_change(&self, change: SourceChange) -> Result<()> {
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

        // Broadcast event
        self.broadcast_event(wrapper)
    }

    /// Broadcast a SourceEventWrapper to all subscribers
    ///
    /// This is a generic method for broadcasting any SourceEvent.
    /// It handles Arc-wrapping for zero-copy broadcast and logs
    /// when there are no subscribers.
    pub fn broadcast_event(&self, wrapper: SourceEventWrapper) -> Result<()> {
        debug!("[{}] Broadcasting event: {:?}", self.config.id, &wrapper);

        // Arc-wrap for zero-copy broadcast
        let arc_wrapper = Arc::new(wrapper);

        if let Err(e) = self.broadcast_tx.send(arc_wrapper) {
            debug!(
                "[{}] Failed to broadcast (no subscribers): {}",
                self.config.id, e
            );
        }

        Ok(())
    }

    /// Send a component lifecycle event
    ///
    /// Helper method for sending component status updates
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
            error!("[{}] Failed to send component event: {}", self.config.id, e);
        }

        Ok(())
    }

    /// Update the component status
    pub async fn set_status(&self, status: ComponentStatus) {
        *self.status.write().await = status;
    }

    /// Get the current component status
    pub async fn get_status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    /// Common stop implementation for sources
    ///
    /// This handles the standard shutdown sequence:
    /// - Update status to Stopping
    /// - Send stopping event
    /// - Trigger shutdown via oneshot channel
    /// - Wait for task to complete
    /// - Update status to Stopped
    /// - Send stopped event
    pub async fn stop_common(&self, source_type: &str) -> Result<()> {
        info!("Stopping {} source '{}'", source_type, self.config.id);

        // Update status to Stopping
        self.set_status(ComponentStatus::Stopping).await;
        self.send_component_event(
            ComponentStatus::Stopping,
            Some(format!("Stopping {} source", source_type)),
        )
        .await?;

        // Send shutdown signal
        if let Some(tx) = self.shutdown_tx.write().await.take() {
            let _ = tx.send(());
        }

        // Wait for task to complete
        if let Some(handle) = self.task_handle.write().await.take() {
            let _ = handle.await;
        }

        // Update status to Stopped
        self.set_status(ComponentStatus::Stopped).await;
        self.send_component_event(
            ComponentStatus::Stopped,
            Some(format!("{} source stopped", source_type)),
        )
        .await?;

        info!("{} source '{}' stopped", source_type, self.config.id);
        Ok(())
    }
}
