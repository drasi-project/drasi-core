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
//! - **`ApplicationSourceHandle`**: A clonable handle for sending events from anywhere in your code
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
use log::{debug, info, warn};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_lib::channels::{ComponentEventSender, ComponentStatus, ComponentType, *};
use drasi_lib::managers::{with_component_context, ComponentContext};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::Source;

/// Handle for programmatic event injection into an Application Source
///
/// `ApplicationSourceHandle` provides a type-safe API for injecting graph data changes
/// (node inserts, updates, deletes, and relationship inserts) directly from your application
/// code into the Drasi continuous query processing pipeline.
#[derive(Clone)]
pub struct ApplicationSourceHandle {
    tx: mpsc::Sender<SourceChange>,
    source_id: String,
}

impl ApplicationSourceHandle {
    /// Send a raw source change event
    pub async fn send(&self, change: SourceChange) -> Result<()> {
        self.tx
            .send(change)
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
                element_id: end_node_id.into(),
            },
            out_node: ElementReference {
                source_id: Arc::from(self.source_id.as_str()),
                element_id: start_node_id.into(),
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
    app_rx: Arc<RwLock<Option<mpsc::Receiver<SourceChange>>>>,
    /// Sender for creating new handles
    app_tx: mpsc::Sender<SourceChange>,
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

        let handle = ApplicationSourceHandle {
            tx: app_tx.clone(),
            source_id: id.clone(),
        };

        let source = Self {
            base: SourceBase::new(params)?,
            config,
            app_rx: Arc::new(RwLock::new(Some(app_rx))),
            app_tx,
        };

        Ok((source, handle))
    }

    /// Get a new handle for this source
    pub fn get_handle(&self) -> ApplicationSourceHandle {
        ApplicationSourceHandle {
            tx: self.app_tx.clone(),
            source_id: self.base.id.clone(),
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
        let status_tx = self.base.status_tx();
        let status = self.base.status.clone();
        let source_id = self.base.id.clone();

        let ctx = ComponentContext::new(source_id, ComponentType::Source);
        let handle = tokio::spawn(with_component_context(ctx, async move {
            info!("ApplicationSource '{source_name}' event processor started");

            if let Some(ref tx) = *status_tx.read().await {
                let _ = tx
                    .send(ComponentEvent {
                        component_id: source_name.clone(),
                        component_type: ComponentType::Source,
                        status: ComponentStatus::Running,
                        timestamp: chrono::Utc::now(),
                        message: Some("Processing events".to_string()),
                    })
                    .await;
            }

            *status.write().await = ComponentStatus::Running;

            while let Some(change) = rx.recv().await {
                debug!("ApplicationSource '{source_name}' received event: {change:?}");

                let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                let wrapper = SourceEventWrapper::with_profiling(
                    source_name.clone(),
                    SourceEvent::Change(change),
                    chrono::Utc::now(),
                    profiling,
                );

                if let Err(e) =
                    SourceBase::dispatch_from_task(base_dispatchers.clone(), wrapper, &source_name)
                        .await
                {
                    debug!("Failed to dispatch change (no subscribers): {e}");
                }
            }

            info!("ApplicationSource '{source_name}' event processor stopped");
        }));

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
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting application source".to_string()),
            )
            .await?;

        self.process_events().await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping ApplicationSource '{}'", self.base.id);

        self.base
            .set_status_with_event(
                ComponentStatus::Stopping,
                Some("Stopping application source".to_string()),
            )
            .await?;

        if let Some(handle) = self.base.task_handle.write().await.take() {
            handle.abort();
        }

        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("Application source stopped".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.status.read().await.clone()
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "Application")
            .await
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
