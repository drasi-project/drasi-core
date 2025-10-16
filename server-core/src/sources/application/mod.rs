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

mod property_builder;

#[cfg(test)]
mod tests;

pub use property_builder::PropertyMapBuilder;

use anyhow::Result;
use async_trait::async_trait;
use log::{debug, error, info};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio::task::JoinHandle;

use crate::channels::{ComponentStatus, ComponentType, *};
use crate::config::SourceConfig;
use crate::sources::Source;
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};

/// Handle for applications to send events to the ApplicationSource
#[derive(Clone)]
pub struct ApplicationSourceHandle {
    tx: mpsc::Sender<SourceChange>,
    source_id: String,
}

impl ApplicationSourceHandle {
    /// Send a source change event
    pub async fn send(&self, change: SourceChange) -> Result<()> {
        self.tx
            .send(change)
            .await
            .map_err(|_| anyhow::anyhow!("Failed to send event: channel closed"))?;
        Ok(())
    }

    /// Send an insert event for a node
    pub async fn send_node_insert(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: drasi_core::models::ElementPropertyMap,
    ) -> Result<()> {
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference {
                    source_id: Arc::from(self.source_id.as_str()),
                    element_id: element_id.into(),
                },
                labels: Arc::from(labels.into_iter().map(|l| l.into()).collect::<Vec<_>>()),
                effective_from: chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64,
            },
            properties,
        };

        self.send(SourceChange::Insert { element }).await
    }

    /// Send an update event for a node
    pub async fn send_node_update(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: drasi_core::models::ElementPropertyMap,
    ) -> Result<()> {
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference {
                    source_id: Arc::from(self.source_id.as_str()),
                    element_id: element_id.into(),
                },
                labels: Arc::from(labels.into_iter().map(|l| l.into()).collect::<Vec<_>>()),
                effective_from: chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64,
            },
            properties,
        };

        self.send(SourceChange::Update { element }).await
    }

    /// Send a delete event for an element
    pub async fn send_delete(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
    ) -> Result<()> {
        let metadata = ElementMetadata {
            reference: ElementReference {
                source_id: Arc::from(self.source_id.as_str()),
                element_id: element_id.into(),
            },
            labels: Arc::from(labels.into_iter().map(|l| l.into()).collect::<Vec<_>>()),
            effective_from: chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64,
        };

        self.send(SourceChange::Delete { metadata }).await
    }

    /// Send an insert event for a relation
    pub async fn send_relation_insert(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: drasi_core::models::ElementPropertyMap,
        start_node_id: impl Into<Arc<str>>,
        end_node_id: impl Into<Arc<str>>,
    ) -> Result<()> {
        let element = Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference {
                    source_id: Arc::from(self.source_id.as_str()),
                    element_id: element_id.into(),
                },
                labels: Arc::from(labels.into_iter().map(|l| l.into()).collect::<Vec<_>>()),
                effective_from: chrono::Utc::now().timestamp_nanos_opt().unwrap() as u64,
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

    /// Send a batch of source changes
    pub async fn send_batch(&self, changes: Vec<SourceChange>) -> Result<()> {
        for change in changes {
            self.send(change).await?;
        }
        Ok(())
    }

    /// Get the source id for reference
    pub fn source_id(&self) -> &str {
        &self.source_id
    }
}

/// A source that allows applications to programmatically inject events
pub struct ApplicationSource {
    config: SourceConfig,
    status: Arc<RwLock<ComponentStatus>>,
    source_event_tx: SourceEventSender,
    event_tx: ComponentEventSender,
    app_rx: Arc<RwLock<Option<mpsc::Receiver<SourceChange>>>>,
    task_handle: Arc<RwLock<Option<JoinHandle<()>>>>,
    bootstrap_data: Arc<RwLock<Vec<SourceChange>>>,
}

impl ApplicationSource {
    pub fn new(
        config: SourceConfig,
        source_event_tx: SourceEventSender,
        event_tx: ComponentEventSender,
    ) -> (Self, ApplicationSourceHandle) {
        let (app_tx, app_rx) = mpsc::channel(1000);

        let handle = ApplicationSourceHandle {
            tx: app_tx.clone(),
            source_id: config.id.clone(),
        };

        let source = Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            source_event_tx,
            event_tx,
            app_rx: Arc::new(RwLock::new(Some(app_rx))),
            task_handle: Arc::new(RwLock::new(None)),
            bootstrap_data: Arc::new(RwLock::new(Vec::new())),
        };

        (source, handle)
    }

    async fn process_events(&self) -> Result<()> {
        let mut rx = self
            .app_rx
            .write()
            .await
            .take()
            .ok_or_else(|| anyhow::anyhow!("Receiver already taken"))?;

        let source_name = self.config.id.clone();
        let source_event_tx = self.source_event_tx.clone();
        let event_tx = self.event_tx.clone();
        let status = self.status.clone();
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

                let wrapper = SourceEventWrapper {
                    source_id: source_name.clone(),
                    event: SourceEvent::Change(change),
                    timestamp: chrono::Utc::now(),
                };

                if let Err(e) = source_event_tx.send(wrapper).await {
                    error!("Failed to send change event: {}", e);
                    break;
                }
            }

            info!(
                "ApplicationSource '{}' event processor stopped",
                source_name
            );
        });

        *self.task_handle.write().await = Some(handle);
        Ok(())
    }
}

#[async_trait]
impl Source for ApplicationSource {
    async fn start(&self) -> Result<()> {
        info!("Starting ApplicationSource '{}'", self.config.id);

        *self.status.write().await = ComponentStatus::Starting;

        let _ = self
            .event_tx
            .send(ComponentEvent {
                component_id: self.config.id.clone(),
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
        info!("Stopping ApplicationSource '{}'", self.config.id);

        *self.status.write().await = ComponentStatus::Stopping;

        let _ = self
            .event_tx
            .send(ComponentEvent {
                component_id: self.config.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Stopping,
                timestamp: chrono::Utc::now(),
                message: Some("Stopping application source".to_string()),
            })
            .await;

        // Cancel the processing task
        if let Some(handle) = self.task_handle.write().await.take() {
            handle.abort();
        }

        *self.status.write().await = ComponentStatus::Stopped;

        let _ = self
            .event_tx
            .send(ComponentEvent {
                component_id: self.config.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Stopped,
                timestamp: chrono::Utc::now(),
                message: Some("Application source stopped".to_string()),
            })
            .await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &SourceConfig {
        &self.config
    }
}
