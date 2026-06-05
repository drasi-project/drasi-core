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

#![allow(unexpected_cfgs)]

//! Application Source Plugin for Drasi
//!
//! This plugin enables programmatic event injection into Drasi's continuous query
//! processing pipeline. Unlike other sources that connect to external data systems,
//! the application source allows your Rust code to directly send graph data changes.
//!
//! # Architecture
//!
//! The application source uses a handle-based pattern:
//! - **`ApplicationSource`**: The source component that processes events
//! - **`ApplicationSourceHandle`**: A cloneable handle for sending events from anywhere in your code
//!
//! # API Overview
//!
//! The `ApplicationSourceHandle` provides high-level methods for common operations:
//!
//! - [`send_node_insert`](ApplicationSourceHandle::send_node_insert) - Insert a new node
//! - [`send_node_update`](ApplicationSourceHandle::send_node_update) - Update an existing node
//! - [`send_delete`](ApplicationSourceHandle::send_delete) - Delete a node or relation
//! - [`send_relation_insert`](ApplicationSourceHandle::send_relation_insert) - Insert a relationship
//! - [`send_batch`](ApplicationSourceHandle::send_batch) - Send multiple changes efficiently
//! - [`send`](ApplicationSourceHandle::send) - Send a raw `SourceChange` event
//!
//! # Building Properties
//!
//! Use the [`PropertyMapBuilder`] to construct property maps fluently:
//!
//! ```rust,ignore
//! use drasi_source_application::PropertyMapBuilder;
//!
//! let props = PropertyMapBuilder::new()
//!     .string("name", "Alice")
//!     .integer("age", 30)
//!     .float("score", 95.5)
//!     .bool("active", true)
//!     .build();
//! ```
//!
//! # Configuration
//!
//! The application source has minimal configuration since it receives events programmatically
//! rather than connecting to an external system.
//!
//! | Field | Type | Default | Description |
//! |-------|------|---------|-------------|
//! | `properties` | object | `{}` | Custom properties (passed through to `properties()`) |
//!
//! # Bootstrap Support
//!
//! The application source supports pluggable bootstrap providers via the `BootstrapProvider`
//! trait. Configure a bootstrap provider using `set_bootstrap_provider()` or through the
//! builder pattern. Common options include `ApplicationBootstrapProvider` for replaying
//! stored events, or any other `BootstrapProvider` implementation.
//!
//! # Example Configuration (YAML)
//!
//! ```yaml
//! source_type: application
//! properties: {}
//! ```
//!
//! # Usage Example
//!
//! ```rust,ignore
//! use drasi_source_application::{
//!     ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle,
//!     PropertyMapBuilder
//! };
//! use std::sync::Arc;
//!
//! // Create the source and handle
//! let config = ApplicationSourceConfig::default();
//! let (source, handle) = ApplicationSource::new("my-app-source", config)?;
//!
//! // Add source to Drasi
//! let source = Arc::new(source);
//! drasi.add_source(source).await?;
//!
//! // Clone handle for use in different parts of your application
//! let handle_clone = handle.clone();
//!
//! // Insert a node
//! let props = PropertyMapBuilder::new()
//!     .string("name", "Alice")
//!     .integer("age", 30)
//!     .build();
//!
//! handle.send_node_insert("user-1", vec!["User"], props).await?;
//!
//! // Insert a relationship
//! let rel_props = PropertyMapBuilder::new()
//!     .string("since", "2024-01-01")
//!     .build();
//!
//! handle.send_relation_insert(
//!     "follows-1",
//!     vec!["FOLLOWS"],
//!     rel_props,
//!     "user-1",  // start node
//!     "user-2",  // end node
//! ).await?;
//!
//! // Update a node
//! let updated_props = PropertyMapBuilder::new()
//!     .integer("age", 31)
//!     .build();
//!
//! handle.send_node_update("user-1", vec!["User"], updated_props).await?;
//!
//! // Delete a node
//! handle.send_delete("user-1", vec!["User"]).await?;
//! ```
//!
//! # Use Cases
//!
//! - **Testing**: Inject test data directly without setting up external sources
//! - **Integration**: Bridge between your application logic and Drasi queries
//! - **Simulation**: Generate synthetic events for development and demos
//! - **Hybrid Sources**: Combine with other sources for complex data pipelines

pub mod config;
pub use config::ApplicationSourceConfig;

mod property_builder;
mod time;

#[cfg(test)]
mod tests;

pub use property_builder::PropertyMapBuilder;

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info, trace, warn};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};

use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_lib::channels::{ComponentStatus, *};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::wal::{WalError, WalProvider};
use drasi_lib::Source;
use tracing::Instrument;

/// Internal event passed through the channel, carrying optional pre-assigned WAL sequence
struct InternalEvent {
    change: SourceChange,
    wal_seq: Option<u64>,
}

/// Handle for programmatic event injection into an Application Source
///
/// `ApplicationSourceHandle` provides a type-safe API for injecting graph data changes
/// (node inserts, updates, deletes, and relationship inserts) directly from your application
/// code into the Drasi continuous query processing pipeline.
#[derive(Clone)]
pub struct ApplicationSourceHandle {
    tx: mpsc::Sender<InternalEvent>,
    source_id: String,
    /// Shared WAL reference — populated when the source is started with durability enabled
    wal: Arc<tokio::sync::RwLock<Option<Arc<dyn WalProvider>>>>,
}

impl ApplicationSourceHandle {
    /// Send a raw source change event
    ///
    /// If WAL durability is enabled, the event is persisted to the WAL before
    /// being acknowledged (returned Ok). This ensures the WAL-before-ACK guarantee.
    pub async fn send(&self, change: SourceChange) -> Result<()> {
        // WAL append before ACK (if durability is enabled)
        let wal_seq = {
            let wal_guard = self.wal.read().await;
            if let Some(ref wal) = *wal_guard {
                match wal.append(&self.source_id, &change).await {
                    Ok(seq) => Some(seq),
                    Err(WalError::CapacityExhausted(msg)) => {
                        return Err(anyhow::anyhow!("WAL capacity exhausted: {msg}"));
                    }
                    Err(e) => {
                        return Err(anyhow::anyhow!("WAL append failed: {e}"));
                    }
                }
            } else {
                None
            }
        };

        self.tx
            .send(InternalEvent { change, wal_seq })
            .await
            .map_err(|_| anyhow::anyhow!("Failed to send event: channel closed"))?;
        Ok(())
    }

    /// Insert a new node into the graph
    pub async fn send_node_insert(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: drasi_core::models::ElementPropertyMap,
    ) -> Result<()> {
        let effective_from = crate::time::get_current_timestamp_millis().unwrap_or_else(|e| {
            warn!("Failed to get timestamp for node insert: {e}, using fallback");
            chrono::Utc::now().timestamp_millis() as u64
        });

        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference {
                    source_id: Arc::from(self.source_id.as_str()),
                    element_id: element_id.into(),
                },
                labels: Arc::from(labels.into_iter().map(|l| l.into()).collect::<Vec<_>>()),
                effective_from,
            },
            properties,
        };

        self.send(SourceChange::Insert { element }).await
    }

    /// Update an existing node in the graph
    pub async fn send_node_update(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: drasi_core::models::ElementPropertyMap,
    ) -> Result<()> {
        let effective_from = crate::time::get_current_timestamp_millis().unwrap_or_else(|e| {
            warn!("Failed to get timestamp for node update: {e}, using fallback");
            chrono::Utc::now().timestamp_millis() as u64
        });

        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference {
                    source_id: Arc::from(self.source_id.as_str()),
                    element_id: element_id.into(),
                },
                labels: Arc::from(labels.into_iter().map(|l| l.into()).collect::<Vec<_>>()),
                effective_from,
            },
            properties,
        };

        self.send(SourceChange::Update { element }).await
    }

    /// Delete a node or relationship from the graph
    pub async fn send_delete(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
    ) -> Result<()> {
        let effective_from = crate::time::get_current_timestamp_millis().unwrap_or_else(|e| {
            warn!("Failed to get timestamp for delete: {e}, using fallback");
            chrono::Utc::now().timestamp_millis() as u64
        });

        let metadata = ElementMetadata {
            reference: ElementReference {
                source_id: Arc::from(self.source_id.as_str()),
                element_id: element_id.into(),
            },
            labels: Arc::from(labels.into_iter().map(|l| l.into()).collect::<Vec<_>>()),
            effective_from,
        };

        self.send(SourceChange::Delete { metadata }).await
    }

    /// Insert a new relationship into the graph
    pub async fn send_relation_insert(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: drasi_core::models::ElementPropertyMap,
        start_node_id: impl Into<Arc<str>>,
        end_node_id: impl Into<Arc<str>>,
    ) -> Result<()> {
        let effective_from = crate::time::get_current_timestamp_millis().unwrap_or_else(|e| {
            warn!("Failed to get timestamp for relation insert: {e}, using fallback");
            chrono::Utc::now().timestamp_millis() as u64
        });

        let element = Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference {
                    source_id: Arc::from(self.source_id.as_str()),
                    element_id: element_id.into(),
                },
                labels: Arc::from(labels.into_iter().map(|l| l.into()).collect::<Vec<_>>()),
                effective_from,
            },
            properties,
            in_node: ElementReference {
                source_id: Arc::from(self.source_id.as_str()),
                element_id: start_node_id.into(),
            },
            out_node: ElementReference {
                source_id: Arc::from(self.source_id.as_str()),
                element_id: end_node_id.into(),
            },
        };

        self.send(SourceChange::Insert { element }).await
    }

    /// Send a batch of source changes efficiently
    pub async fn send_batch(&self, changes: Vec<SourceChange>) -> Result<()> {
        for change in changes {
            self.send(change).await?;
        }
        Ok(())
    }

    /// Get the source ID that this handle is connected to
    pub fn source_id(&self) -> &str {
        &self.source_id
    }
}

/// A source that allows applications to programmatically inject events.
///
/// This source receives events from an [`ApplicationSourceHandle`] and forwards
/// them to the Drasi query processing pipeline.
///
/// # Fields
///
/// - `base`: Common source functionality (dispatchers, status, lifecycle, bootstrap)
/// - `config`: Application source configuration
/// - `app_rx`: Receiver for events from the handle
/// - `app_tx`: Sender for creating additional handles
pub struct ApplicationSource {
    /// Base source implementation providing common functionality
    base: SourceBase,
    /// Application source configuration
    config: ApplicationSourceConfig,
    /// Receiver for events from handles (taken when processing starts)
    app_rx: Arc<RwLock<Option<mpsc::Receiver<InternalEvent>>>>,
    /// Sender for creating new handles
    app_tx: mpsc::Sender<InternalEvent>,
    /// WAL provider for durable event persistence (shared with handles for WAL-before-ACK)
    wal: Arc<tokio::sync::RwLock<Option<Arc<dyn WalProvider>>>>,
    /// Handle to the WAL pruning background task (if running)
    prune_task: tokio::sync::RwLock<Option<tokio::task::JoinHandle<()>>>,
}

impl ApplicationSource {
    /// Create a new application source and its handle.
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    ///
    /// # Arguments
    ///
    /// * `id` - Unique identifier for this source instance
    /// * `config` - Application source configuration
    ///
    /// # Returns
    ///
    /// A tuple of `(ApplicationSource, ApplicationSourceHandle)` where the handle
    /// can be used to send events to the source.
    ///
    /// # Errors
    ///
    /// Returns an error if the base source cannot be initialized.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// use drasi_source_application::{ApplicationSource, ApplicationSourceConfig};
    ///
    /// let config = ApplicationSourceConfig::default();
    /// let (source, handle) = ApplicationSource::new("my-source", config)?;
    /// ```
    pub fn new(
        id: impl Into<String>,
        config: ApplicationSourceConfig,
    ) -> Result<(Self, ApplicationSourceHandle)> {
        let id = id.into();
        let params = SourceBaseParams::new(id.clone());
        let (app_tx, app_rx) = mpsc::channel(1000);

        // Shared WAL reference — populated later in start() when durability is enabled
        let shared_wal = Arc::new(tokio::sync::RwLock::new(None));

        let handle = ApplicationSourceHandle {
            tx: app_tx.clone(),
            source_id: id.clone(),
            wal: shared_wal.clone(),
        };

        let source = Self {
            base: SourceBase::new(params)?,
            config,
            app_rx: Arc::new(RwLock::new(Some(app_rx))),
            app_tx,
            wal: shared_wal,
            prune_task: tokio::sync::RwLock::new(None),
        };

        Ok((source, handle))
    }

    /// Get a new handle for this source
    ///
    /// The handle shares the WAL reference with the source, so handles obtained
    /// before or after `start()` will automatically use WAL when durability is enabled.
    pub fn get_handle(&self) -> ApplicationSourceHandle {
        ApplicationSourceHandle {
            tx: self.app_tx.clone(),
            source_id: self.base.id.clone(),
            wal: self.wal.clone(),
        }
    }

    async fn process_events(&self) -> Result<()> {
        let mut rx = self
            .app_rx
            .write()
            .await
            .take()
            .ok_or_else(|| anyhow::anyhow!("Receiver already taken"))?;

        let source_name = self.base.id.clone();
        let base_dispatchers = self.base.dispatchers.clone();
        let reporter = self.base.status_handle();
        let source_id = self.base.id.clone();

        // Get instance_id from context for log route isolation
        let instance_id = self
            .base
            .context()
            .await
            .map(|c| c.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "application_source_processor",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );
        let handle = tokio::spawn(
            async move {
                info!("ApplicationSource '{source_name}' event processor started");

                reporter
                    .set_status(
                        ComponentStatus::Running,
                        Some("Processing events".to_string()),
                    )
                    .await;

                while let Some(event) = rx.recv().await {
                    debug!(
                        "ApplicationSource '{source_name}' received event: {:?}",
                        event.change
                    );

                    let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                    profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                    let mut wrapper = SourceEventWrapper::with_profiling(
                        source_name.clone(),
                        SourceEvent::Change(event.change),
                        chrono::Utc::now(),
                        profiling,
                    );

                    // Use pre-assigned WAL sequence from handle (WAL-before-ACK)
                    if let Some(seq) = event.wal_seq {
                        wrapper.sequence = Some(seq);
                        wrapper.source_position =
                            Some(bytes::Bytes::from(seq.to_be_bytes().to_vec()));
                    }

                    if let Err(e) = SourceBase::dispatch_from_task(
                        base_dispatchers.clone(),
                        wrapper,
                        &source_name,
                    )
                    .await
                    {
                        debug!("Failed to dispatch change (no subscribers): {e}");
                    }
                }

                info!("ApplicationSource '{source_name}' event processor stopped");
            }
            .instrument(span),
        );

        *self.base.task_handle.write().await = Some(handle);
        Ok(())
    }
}

#[async_trait]
impl Source for ApplicationSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "application"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.config.properties.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        info!("Starting ApplicationSource '{}'", self.base.id);

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting application source".to_string()),
            )
            .await;

        // Initialize WAL if durability is enabled
        let wal_ref: Option<Arc<dyn WalProvider>> =
            if self.config.durability.as_ref().is_some_and(|d| d.enabled) {
                let ctx = self
                    .base
                    .context()
                    .await
                    .ok_or_else(|| anyhow::anyhow!("Context not initialized"))?;
                let wal = ctx.wal_provider.clone().ok_or_else(|| {
                    anyhow::anyhow!("Durability enabled but no WAL provider configured on DrasiLib")
                })?;
                let wal_config = self
                    .config
                    .durability
                    .as_ref()
                    .expect("durability checked above")
                    .to_wal_config();
                wal.register(&self.base.id, wal_config.clone())
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "Failed to register WAL for source '{}': {}",
                            self.base.id,
                            e
                        )
                    })?;

                info!(
                    "[{}] WAL registered: max_events={}, policy={:?}",
                    self.base.id, wal_config.max_events, wal_config.capacity_policy
                );

                // Resume sequence counter from WAL head
                let head = wal.head_sequence(&self.base.id).await.unwrap_or(0);
                if head > 0 {
                    self.base.set_next_sequence(head);
                    info!(
                        "[{}] WAL resumed from persisted state: head={}, next_sequence={}",
                        self.base.id,
                        head,
                        head + 1
                    );
                }

                *self.wal.write().await = Some(wal.clone());
                Some(wal)
            } else {
                None
            };

        self.process_events().await?;

        // Spawn WAL pruning task if durability is enabled
        if let Some(wal) = wal_ref {
            let base = self.base.clone_shared();
            let source_id = self.base.id.clone();
            let prune_handle = tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    if let Some(confirmed) = base.compute_confirmed_position().await {
                        if confirmed > 0 {
                            match wal.prune_up_to(&source_id, confirmed).await {
                                Ok(pruned) => {
                                    if pruned > 0 {
                                        let remaining =
                                            wal.event_count(&source_id).await.unwrap_or(0);
                                        debug!(
                                            "[{source_id}] WAL pruned: count={pruned}, confirmed_seq={confirmed}, remaining={remaining}"
                                        );
                                    }
                                }
                                Err(e) => {
                                    warn!("[{source_id}] WAL prune failed: {e}");
                                }
                            }
                        }
                    }
                }
            });
            *self.prune_task.write().await = Some(prune_handle);
        }

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping ApplicationSource '{}'", self.base.id);

        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping application source".to_string()),
            )
            .await;

        // Cancel WAL pruning task
        if let Some(handle) = self.prune_task.write().await.take() {
            handle.abort();
        }

        if let Some(handle) = self.base.task_handle.write().await.take() {
            handle.abort();
        }

        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Application source stopped".to_string()),
            )
            .await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        // If WAL is enabled and subscriber is resuming, use WAL replay
        let wal_guard = self.wal.read().await;
        if let (Some(wal), Some(ref resume_from)) = (wal_guard.as_ref(), &settings.resume_from) {
            // Decode resume_from as big-endian u64 sequence
            if resume_from.len() >= 8 {
                let resume_seq =
                    u64::from_be_bytes(resume_from[..8].try_into().unwrap_or_default());
                let wal_clone = wal.clone();
                drop(wal_guard);
                return self
                    .base
                    .subscribe_with_replay(&settings, wal_clone.as_ref(), resume_seq, "Application")
                    .await;
            } else {
                drop(wal_guard);
                return Err(anyhow::anyhow!(
                    "Invalid resume_from position: expected at least 8 bytes, got {}",
                    resume_from.len()
                ));
            }
        }
        drop(wal_guard);
        self.base
            .subscribe_with_bootstrap(&settings, "Application")
            .await
    }

    fn supports_replay(&self) -> bool {
        self.config.durability.as_ref().is_some_and(|d| d.enabled)
    }

    async fn deprovision(&self) -> Result<()> {
        // Delete WAL data if durability was enabled
        let wal_guard = self.wal.read().await;
        if let Some(ref wal) = *wal_guard {
            info!("[{}] Deprovisioning: deleting WAL data", self.base.id);
            if let Err(e) = wal.delete_wal(&self.base.id).await {
                warn!(
                    "[{}] Failed to delete WAL during deprovision: {}",
                    self.base.id, e
                );
            }
        }
        drop(wal_guard);
        Ok(())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}
