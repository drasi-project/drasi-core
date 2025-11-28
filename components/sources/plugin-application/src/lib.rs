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
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use drasi_lib::bootstrap::{BootstrapContext, BootstrapProviderConfig, BootstrapProviderFactory, BootstrapRequest};
use drasi_lib::channels::{ComponentEventSender, ComponentStatus, ComponentType, *};
use drasi_lib::plugin_core::Source;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};

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
        let effective_from =
            drasi_lib::utils::time::get_current_timestamp_nanos().unwrap_or_else(|e| {
                warn!(
                    "Failed to get timestamp for node insert: {}, using fallback",
                    e
                );
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
    pub async fn send_delete(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
    ) -> Result<()> {
        let effective_from =
            drasi_lib::utils::time::get_current_timestamp_nanos().unwrap_or_else(|e| {
                warn!("Failed to get timestamp for delete: {}, using fallback", e);
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

/// A source that allows applications to programmatically inject events
pub struct ApplicationSource {
    base: SourceBase,
    config: ApplicationSourceConfig,
    app_rx: Arc<RwLock<Option<mpsc::Receiver<SourceChange>>>>,
    app_tx: mpsc::Sender<SourceChange>,
    bootstrap_data: Arc<RwLock<Vec<SourceChange>>>,
    bootstrap_provider_config: Option<BootstrapProviderConfig>,
}

impl ApplicationSource {
    /// Create a new application source and its handle
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
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
            bootstrap_data: Arc::new(RwLock::new(Vec::new())),
            bootstrap_provider_config: None,
        };

        Ok((source, handle))
    }

    /// Create a new application source with bootstrap provider
    ///
    /// The event channel is automatically injected when the source is added
    /// to DrasiLib via `add_source()`.
    pub fn with_bootstrap_provider(
        id: impl Into<String>,
        config: ApplicationSourceConfig,
        bootstrap_provider_config: BootstrapProviderConfig,
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
            bootstrap_data: Arc::new(RwLock::new(Vec::new())),
            bootstrap_provider_config: Some(bootstrap_provider_config),
        };

        Ok((source, handle))
    }

    /// Get a clone of the bootstrap data Arc for sharing with ApplicationBootstrapProvider
    pub fn get_bootstrap_data(&self) -> Arc<RwLock<Vec<SourceChange>>> {
        Arc::clone(&self.bootstrap_data)
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
        let event_tx = self.base.event_tx();
        let status = self.base.status.clone();
        let bootstrap_data = self.bootstrap_data.clone();

        let handle = tokio::spawn(async move {
            info!(
                "ApplicationSource '{}' event processor started",
                source_name
            );

            if let Some(ref tx) = *event_tx.read().await {
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
                debug!(
                    "ApplicationSource '{}' received event: {:?}",
                    source_name, change
                );

                if matches!(change, SourceChange::Insert { .. }) {
                    bootstrap_data.write().await.push(change.clone());
                }

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
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "application"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        self.config.properties.clone()
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
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse> {
        info!(
            "Query '{}' subscribing to ApplicationSource '{}' (bootstrap: {})",
            query_id, self.base.id, enable_bootstrap
        );

        let receiver = self.base.create_streaming_receiver().await?;
        let query_id_for_response = query_id.clone();

        let bootstrap_receiver = if enable_bootstrap {
            if let Some(ref provider_config) = self.bootstrap_provider_config {
                info!(
                    "Bootstrap enabled for query '{}' with {} node labels and {} relation labels, delegating to bootstrap provider",
                    query_id,
                    node_labels.len(),
                    relation_labels.len()
                );

                let (tx, rx) = tokio::sync::mpsc::channel(1000);

                let provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider> = if matches!(
                    provider_config,
                    BootstrapProviderConfig::Application { .. }
                ) {
                    Box::new(
                        drasi_plugin_application_bootstrap::ApplicationBootstrapProvider::with_shared_data(
                            self.get_bootstrap_data()
                        )
                    )
                } else {
                    BootstrapProviderFactory::create_provider(provider_config)?
                };

                let context = BootstrapContext::new_minimal(
                    self.base.id.clone(),
                    self.base.id.clone(),
                );

                let request = BootstrapRequest {
                    query_id: query_id.clone(),
                    node_labels,
                    relation_labels,
                    request_id: format!("{}-{}", query_id, uuid::Uuid::new_v4()),
                };

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
                info!(
                    "Bootstrap requested for query '{}' but no bootstrap provider configured, using internal bootstrap",
                    query_id
                );

                let (tx, rx) = tokio::sync::mpsc::channel(1000);
                let bootstrap_data = self.bootstrap_data.read().await;

                info!(
                    "Sending {} bootstrap events for ApplicationSource '{}' (internal)",
                    bootstrap_data.len(),
                    self.base.id
                );

                for (seq, change) in bootstrap_data.iter().enumerate() {
                    let event = BootstrapEvent {
                        source_id: self.base.id.clone(),
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
            source_id: self.base.id.clone(),
            receiver,
            bootstrap_receiver,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        self.base.inject_event_tx(tx).await;
    }
}
