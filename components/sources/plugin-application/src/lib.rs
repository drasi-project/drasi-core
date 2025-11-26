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

pub mod config;
pub use config::ApplicationSourceConfig;

mod property_builder;

#[cfg(test)]
mod tests;

pub use property_builder::PropertyMapBuilder;

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, info, warn};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use drasi_lib::bootstrap::{BootstrapContext, BootstrapProviderFactory, BootstrapRequest};
use drasi_lib::channels::{ComponentStatus, ComponentType, *};
use drasi_lib::config::SourceConfig;
use drasi_lib::sources::base::SourceBase;
use drasi_lib::sources::Source;
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};

/// Handle for programmatic event injection into an Application Source
///
/// `ApplicationSourceHandle` provides a type-safe API for injecting graph data changes
/// (node inserts, updates, deletes, and relationship inserts) directly from your application
/// code into the Drasi continuous query processing pipeline.
///
/// # Usage Pattern
///
/// 1. Get the handle from `DrasiServerCore::source_handle()`
/// 2. Use the fluent builder methods (`send_node_insert()`, `send_relation_insert()`, etc.)
/// 3. Build properties using `.with_*()` methods from [`PropertyMapBuilder`]
/// 4. Call `.send()` to inject the event
///
/// # Thread Safety
///
/// `ApplicationSourceHandle` is `Clone` and all methods are thread-safe. You can safely
/// clone the handle and use it from multiple threads concurrently. The underlying channel
/// has a buffer of 1000 events.
///
/// # Performance
///
/// - Each method creates a single event with a timestamp
/// - Events are sent through an async channel (bounded to 1000)
/// - For high-throughput scenarios, use `send_batch()` to reduce overhead
/// - The `send()` method is async and may block if the channel buffer is full
///
/// # Examples
///
/// ## Inserting Nodes
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, PropertyMapBuilder};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiServerCore::builder()
/// #     .add_source(Source::application("events").build())
/// #     .build().await?;
/// let handle = core.source_handle("events").await?;
///
/// // Insert a Person node with properties
/// let properties = PropertyMapBuilder::new()
///     .with_string("name", "Alice")
///     .with_integer("age", 30)
///     .with_bool("active", true)
///     .build();
///
/// handle.send_node_insert("person-1", vec!["Person"], properties).await?;
///
/// // Insert with multiple labels
/// let properties = PropertyMapBuilder::new()
///     .with_string("username", "alice")
///     .with_string("email", "alice@example.com")
///     .build();
///
/// handle.send_node_insert("user-1", vec!["User", "Admin"], properties).await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Updating Nodes
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, PropertyMapBuilder};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiServerCore::builder()
/// #     .add_source(Source::application("events").build())
/// #     .build().await?;
/// let handle = core.source_handle("events").await?;
///
/// // Update a node's properties
/// let properties = PropertyMapBuilder::new()
///     .with_integer("age", 31)  // Update age
///     .with_string("city", "Portland")  // Add new property
///     .build();
///
/// handle.send_node_update("person-1", vec!["Person"], properties).await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Inserting Relationships
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, PropertyMapBuilder};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiServerCore::builder()
/// #     .add_source(Source::application("events").build())
/// #     .build().await?;
/// let handle = core.source_handle("events").await?;
///
/// // Insert a KNOWS relationship
/// let properties = PropertyMapBuilder::new()
///     .with_string("since", "2020")
///     .with_integer("strength", 5)
///     .build();
///
/// handle.send_relation_insert(
///     "rel-1",
///     vec!["KNOWS"],
///     properties,
///     "person-1",  // start node
///     "person-2"   // end node
/// ).await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Deleting Elements
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiServerCore::builder()
/// #     .add_source(Source::application("events").build())
/// #     .build().await?;
/// let handle = core.source_handle("events").await?;
///
/// // Delete a node or relationship
/// handle.send_delete("person-1", vec!["Person"]).await?;
/// handle.send_delete("rel-1", vec!["KNOWS"]).await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Batch Operations
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source};
/// # use drasi_core::models::SourceChange;
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiServerCore::builder()
/// #     .add_source(Source::application("events").build())
/// #     .build().await?;
/// let handle = core.source_handle("events").await?;
///
/// // Prepare a batch of changes
/// let changes: Vec<SourceChange> = vec![
///     // ... create SourceChange instances ...
/// ];
///
/// // Send all at once (more efficient for large batches)
/// handle.send_batch(changes).await?;
/// # Ok(())
/// # }
/// ```
///
/// ## Error Handling
///
/// ```no_run
/// # use drasi_lib::{DrasiServerCore, Source, PropertyMapBuilder};
/// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
/// # let core = DrasiServerCore::builder()
/// #     .add_source(Source::application("events").build())
/// #     .build().await?;
/// let handle = core.source_handle("events").await?;
///
/// let properties = PropertyMapBuilder::new()
///     .with_string("name", "Bob")
///     .build();
///
/// match handle.send_node_insert("person-1", vec!["Person"], properties).await {
///     Ok(_) => println!("Event sent successfully"),
///     Err(e) => {
///         // Channel closed - source may have been stopped or deleted
///         eprintln!("Failed to send event: {}", e);
///     }
/// }
/// # Ok(())
/// # }
/// ```
#[derive(Clone)]
pub struct ApplicationSourceHandle {
    tx: mpsc::Sender<SourceChange>,
    source_id: String,
}

impl ApplicationSourceHandle {
    /// Send a raw source change event
    ///
    /// This is a low-level method for sending pre-constructed [`SourceChange`] events.
    /// Most users should use the higher-level methods like [`send_node_insert()`](Self::send_node_insert),
    /// [`send_node_update()`](Self::send_node_update), etc. instead.
    ///
    /// # Arguments
    ///
    /// * `change` - The source change event to send
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The underlying channel is closed (source has been stopped or deleted)
    /// * The channel buffer is full (blocks until space is available or channel closes)
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently from multiple threads.
    pub async fn send(&self, change: SourceChange) -> Result<()> {
        self.tx
            .send(change)
            .await
            .map_err(|_| anyhow::anyhow!("Failed to send event: channel closed"))?;
        Ok(())
    }

    /// Insert a new node into the graph
    ///
    /// Creates and sends a node insert event with the specified ID, labels, and properties.
    /// The node will be timestamped with the current time.
    ///
    /// # Arguments
    ///
    /// * `element_id` - Unique identifier for the node (must be unique within this source)
    /// * `labels` - One or more labels for the node (e.g., "Person", "User")
    /// * `properties` - Property map containing node attributes
    ///
    /// # Returns
    ///
    /// Returns `Ok(())` if the event was successfully sent to the processing pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// * The underlying channel is closed (`anyhow::Error`)
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently from multiple threads.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, PropertyMapBuilder};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let core = DrasiServerCore::builder()
    /// #     .add_source(Source::application("events").build())
    /// #     .build().await?;
    /// let handle = core.source_handle("events").await?;
    ///
    /// let properties = PropertyMapBuilder::new()
    ///     .with_string("name", "Alice")
    ///     .with_integer("age", 30)
    ///     .build();
    ///
    /// handle.send_node_insert("user-1", vec!["User", "Person"], properties).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn send_node_insert(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: drasi_core::models::ElementPropertyMap,
    ) -> Result<()> {
        let effective_from =
            drasi_lib::utils::time::get_current_timestamp_nanos().unwrap_or_else(|e| {
                warn!(
                    "Failed to get timestamp for node insert: {}, using fallback",
                    e
                );
                // Use current milliseconds * 1M as fallback
                (chrono::Utc::now().timestamp_millis() as u64) * 1_000_000
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
    ///
    /// Creates and sends a node update event. The properties provided will update or add
    /// to the node's existing properties. To remove properties, use a full replacement
    /// or send a delete followed by an insert.
    ///
    /// # Arguments
    ///
    /// * `element_id` - Unique identifier for the node to update
    /// * `labels` - Current labels for the node
    /// * `properties` - Properties to update or add
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying channel is closed.
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently.
    pub async fn send_node_update(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: drasi_core::models::ElementPropertyMap,
    ) -> Result<()> {
        let effective_from =
            drasi_lib::utils::time::get_current_timestamp_nanos().unwrap_or_else(|e| {
                warn!(
                    "Failed to get timestamp for node update: {}, using fallback",
                    e
                );
                // Use current milliseconds * 1M as fallback
                (chrono::Utc::now().timestamp_millis() as u64) * 1_000_000
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
    ///
    /// Creates and sends a delete event for the specified element. This removes the element
    /// from the graph and triggers any queries watching for deletions.
    ///
    /// # Arguments
    ///
    /// * `element_id` - Unique identifier for the element to delete
    /// * `labels` - Labels of the element being deleted
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying channel is closed.
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently.
    pub async fn send_delete(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
    ) -> Result<()> {
        let effective_from =
            drasi_lib::utils::time::get_current_timestamp_nanos().unwrap_or_else(|e| {
                warn!("Failed to get timestamp for delete: {}, using fallback", e);
                // Use current milliseconds * 1M as fallback
                (chrono::Utc::now().timestamp_millis() as u64) * 1_000_000
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
    ///
    /// Creates and sends a relationship insert event connecting two nodes. The relationship
    /// is directed from `start_node_id` to `end_node_id`.
    ///
    /// # Arguments
    ///
    /// * `element_id` - Unique identifier for the relationship
    /// * `labels` - One or more labels for the relationship (e.g., "KNOWS", "FOLLOWS")
    /// * `properties` - Property map containing relationship attributes
    /// * `start_node_id` - ID of the source/outgoing node
    /// * `end_node_id` - ID of the target/incoming node
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying channel is closed.
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, PropertyMapBuilder};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let core = DrasiServerCore::builder()
    /// #     .add_source(Source::application("events").build())
    /// #     .build().await?;
    /// let handle = core.source_handle("events").await?;
    ///
    /// // Create FOLLOWS relationship from user-1 to user-2
    /// let properties = PropertyMapBuilder::new()
    ///     .with_string("since", "2024-01-01")
    ///     .build();
    ///
    /// handle.send_relation_insert(
    ///     "follow-1",
    ///     vec!["FOLLOWS"],
    ///     properties,
    ///     "user-1",  // follower
    ///     "user-2"   // followee
    /// ).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn send_relation_insert(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: drasi_core::models::ElementPropertyMap,
        start_node_id: impl Into<Arc<str>>,
        end_node_id: impl Into<Arc<str>>,
    ) -> Result<()> {
        let effective_from =
            drasi_lib::utils::time::get_current_timestamp_nanos().unwrap_or_else(|e| {
                warn!(
                    "Failed to get timestamp for relation insert: {}, using fallback",
                    e
                );
                // Use current milliseconds * 1M as fallback
                (chrono::Utc::now().timestamp_millis() as u64) * 1_000_000
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
    ///
    /// Sends multiple source changes in sequence. This is more efficient than calling
    /// individual send methods in a loop as it reduces overhead.
    ///
    /// # Arguments
    ///
    /// * `changes` - Vector of pre-constructed source change events
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying channel is closed or if any individual send fails.
    /// If an error occurs, subsequent changes in the batch will not be sent.
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source, PropertyMapBuilder};
    /// # use drasi_core::models::{SourceChange, Element, ElementMetadata, ElementReference};
    /// # use std::sync::Arc;
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let core = DrasiServerCore::builder()
    /// #     .add_source(Source::application("events").build())
    /// #     .build().await?;
    /// let handle = core.source_handle("events").await?;
    ///
    /// // Create multiple changes
    /// let changes = vec![
    ///     // ... construct SourceChange instances ...
    /// ];
    ///
    /// // Send all at once
    /// handle.send_batch(changes).await?;
    /// # Ok(())
    /// # }
    /// ```
    pub async fn send_batch(&self, changes: Vec<SourceChange>) -> Result<()> {
        for change in changes {
            self.send(change).await?;
        }
        Ok(())
    }

    /// Get the source ID that this handle is connected to
    ///
    /// Returns the unique identifier of the application source that this handle sends events to.
    ///
    /// # Thread Safety
    ///
    /// This method is thread-safe and can be called concurrently.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # use drasi_lib::{DrasiServerCore, Source};
    /// # async fn example() -> Result<(), Box<dyn std::error::Error>> {
    /// # let core = DrasiServerCore::builder()
    /// #     .add_source(Source::application("events").build())
    /// #     .build().await?;
    /// let handle = core.source_handle("events").await?;
    /// assert_eq!(handle.source_id(), "events");
    /// # Ok(())
    /// # }
    /// ```
    pub fn source_id(&self) -> &str {
        &self.source_id
    }
}

/// A source that allows applications to programmatically inject events
pub struct ApplicationSource {
    base: SourceBase,
    app_rx: Arc<RwLock<Option<mpsc::Receiver<SourceChange>>>>,
    bootstrap_data: Arc<RwLock<Vec<SourceChange>>>,
}

impl ApplicationSource {
    pub fn new(
        config: SourceConfig,
        event_tx: ComponentEventSender,
    ) -> Result<(Self, ApplicationSourceHandle)> {
        let (app_tx, app_rx) = mpsc::channel(1000);

        let handle = ApplicationSourceHandle {
            tx: app_tx.clone(),
            source_id: config.id.clone(),
        };

        let source = Self {
            base: SourceBase::new(config, event_tx)?,
            app_rx: Arc::new(RwLock::new(Some(app_rx))),
            bootstrap_data: Arc::new(RwLock::new(Vec::new())),
        };

        Ok((source, handle))
    }

    /// Get a clone of the bootstrap data Arc for sharing with ApplicationBootstrapProvider
    /// This allows the provider to access the actual bootstrap data stored by this source
    pub fn get_bootstrap_data(&self) -> Arc<RwLock<Vec<SourceChange>>> {
        Arc::clone(&self.bootstrap_data)
    }

    async fn process_events(&self) -> Result<()> {
        let mut rx = self
            .app_rx
            .write()
            .await
            .take()
            .ok_or_else(|| anyhow::anyhow!("Receiver already taken"))?;

        let source_name = self.base.config.id.clone();
        let base_dispatchers = self.base.dispatchers.clone();
        let event_tx = self.base.event_tx.clone();
        let status = self.base.status.clone();
        let bootstrap_data = self.bootstrap_data.clone();

        let handle = tokio::spawn(async move {
            info!(
                "ApplicationSource '{}' event processor started",
                source_name
            );

            // Send running status
            let _ = event_tx
                .send(ComponentEvent {
                    component_id: source_name.clone(),
                    component_type: ComponentType::Source,
                    status: ComponentStatus::Running,
                    timestamp: chrono::Utc::now(),
                    message: Some("Processing events".to_string()),
                })
                .await;

            *status.write().await = ComponentStatus::Running;

            while let Some(change) = rx.recv().await {
                debug!(
                    "ApplicationSource '{}' received event: {:?}",
                    source_name, change
                );

                // Store for bootstrap if it's an insert
                if matches!(change, SourceChange::Insert { .. }) {
                    bootstrap_data.write().await.push(change.clone());
                }

                // Create profiling metadata with timestamps
                let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                let wrapper = SourceEventWrapper::with_profiling(
                    source_name.clone(),
                    SourceEvent::Change(change),
                    chrono::Utc::now(),
                    profiling,
                );

                // Dispatch to all subscribers via helper
                if let Err(e) =
                    SourceBase::dispatch_from_task(base_dispatchers.clone(), wrapper, &source_name)
                        .await
                {
                    debug!("Failed to dispatch change (no subscribers): {}", e);
                }
            }

            info!(
                "ApplicationSource '{}' event processor stopped",
                source_name
            );
        });

        *self.base.task_handle.write().await = Some(handle);
        Ok(())
    }
}

#[async_trait]
impl Source for ApplicationSource {
    async fn start(&self) -> Result<()> {
        info!("Starting ApplicationSource '{}'", self.base.config.id);

        *self.base.status.write().await = ComponentStatus::Starting;

        let _ = self
            .base
            .event_tx
            .send(ComponentEvent {
                component_id: self.base.config.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Starting,
                timestamp: chrono::Utc::now(),
                message: Some("Starting application source".to_string()),
            })
            .await;

        self.process_events().await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        info!("Stopping ApplicationSource '{}'", self.base.config.id);

        *self.base.status.write().await = ComponentStatus::Stopping;

        let _ = self
            .base
            .event_tx
            .send(ComponentEvent {
                component_id: self.base.config.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Stopping,
                timestamp: chrono::Utc::now(),
                message: Some("Stopping application source".to_string()),
            })
            .await;

        // Cancel the processing task
        if let Some(handle) = self.base.task_handle.write().await.take() {
            handle.abort();
        }

        *self.base.status.write().await = ComponentStatus::Stopped;

        let _ = self
            .base
            .event_tx
            .send(ComponentEvent {
                component_id: self.base.config.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Stopped,
                timestamp: chrono::Utc::now(),
                message: Some("Application source stopped".to_string()),
            })
            .await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.status.read().await.clone()
    }

    fn get_config(&self) -> &SourceConfig {
        &self.base.config
    }

    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse> {
        info!(
            "Query '{}' subscribing to ApplicationSource '{}' (bootstrap: {})",
            query_id, self.base.config.id, enable_bootstrap
        );

        // Create streaming receiver using SourceBase helper method
        let receiver = self.base.create_streaming_receiver().await?;

        // Clone query_id for later use since it will be moved into async block
        let query_id_for_response = query_id.clone();

        // Handle bootstrap if requested
        let bootstrap_receiver = if enable_bootstrap {
            // Check if bootstrap provider is configured
            if let Some(provider_config) = &self.base.config.bootstrap_provider {
                info!(
                    "Bootstrap enabled for query '{}' with {} node labels and {} relation labels, delegating to bootstrap provider",
                    query_id,
                    node_labels.len(),
                    relation_labels.len()
                );

                let (tx, rx) = tokio::sync::mpsc::channel(1000);

                // Create bootstrap provider - if it's Application type, use shared data
                let provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider> = if matches!(
                    provider_config,
                    drasi_lib::bootstrap::BootstrapProviderConfig::Application { .. }
                ) {
                    // Create ApplicationBootstrapProvider with shared reference to our bootstrap_data
                    Box::new(
                            drasi_plugin_application_bootstrap::ApplicationBootstrapProvider::with_shared_data(
                                self.get_bootstrap_data()
                            )
                        )
                } else {
                    // For other provider types, use the factory
                    BootstrapProviderFactory::create_provider(provider_config)?
                };

                // Create bootstrap context
                let context = BootstrapContext::new(
                    self.base.config.id.clone(), // server_id (using source_id as placeholder)
                    Arc::new(self.base.config.clone()),
                    self.base.config.id.clone(),
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
                            log::error!("Bootstrap failed for query '{}': {}", query_id, e);
                        }
                    }
                });

                Some(rx)
            } else {
                // No bootstrap provider configured, fall back to internal bootstrap logic
                info!(
                    "Bootstrap requested for query '{}' but no bootstrap provider configured, using internal bootstrap",
                    query_id
                );

                let (tx, rx) = tokio::sync::mpsc::channel(1000);
                let bootstrap_data = self.bootstrap_data.read().await;

                info!(
                    "Sending {} bootstrap events for ApplicationSource '{}' (internal)",
                    bootstrap_data.len(),
                    self.base.config.id
                );

                // Send bootstrap events
                for (seq, change) in bootstrap_data.iter().enumerate() {
                    let event = BootstrapEvent {
                        source_id: self.base.config.id.clone(),
                        change: change.clone(),
                        timestamp: chrono::Utc::now(),
                        sequence: seq as u64,
                    };
                    let _ = tx.send(event).await;
                }

                Some(rx)
            }
        } else {
            None
        };

        Ok(SubscriptionResponse {
            query_id: query_id_for_response,
            source_id: self.base.config.id.clone(),
            receiver,
            bootstrap_receiver,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}
