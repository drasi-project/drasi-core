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

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use log::{error, info};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::*;
use crate::config::SourceConfig;
use crate::sources::Source;
use crate::utils::*;

/// Mock source that runs as an internal tokio task
pub struct MockSource {
    config: SourceConfig,
    status: Arc<RwLock<ComponentStatus>>,
    source_event_tx: SourceEventSender,
    event_tx: ComponentEventSender,
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl MockSource {
    pub fn new(
        config: SourceConfig,
        source_event_tx: SourceEventSender,
        event_tx: ComponentEventSender,
    ) -> Self {
        Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            source_event_tx,
            event_tx,
            task_handle: Arc::new(RwLock::new(None)),
        }
    }
}

#[async_trait]
impl Source for MockSource {
    async fn start(&self) -> Result<()> {
        log_component_start("Mock Source", &self.config.id);

        *self.status.write().await = ComponentStatus::Starting;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Starting,
            timestamp: chrono::Utc::now(),
            message: Some("Starting mock source".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Get source_event_tx for publishing
        let source_event_tx = self.source_event_tx.clone();
        let source_id = self.config.id.clone();

        // Get configuration
        let data_type = self
            .config
            .properties
            .get("data_type")
            .and_then(|v| v.as_str())
            .unwrap_or("generic")
            .to_string();

        let interval_ms = self
            .config
            .properties
            .get("interval_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(5000);

        // Start the data generation task
        let status = Arc::clone(&self.status);
        let source_name = self.config.id.clone();
        let task = tokio::spawn(async move {
            let mut interval =
                tokio::time::interval(tokio::time::Duration::from_millis(interval_ms));
            let mut seq = 0u64;

            loop {
                interval.tick().await;

                // Check if we should stop
                if !matches!(*status.read().await, ComponentStatus::Running) {
                    break;
                }

                seq += 1;

                // Generate data based on type
                let source_change = match data_type.as_str() {
                    "counter" => {
                        let element_id = format!("counter_{}", seq);
                        let reference = ElementReference::new(&source_name, &element_id);

                        let mut property_map = ElementPropertyMap::new();
                        property_map.insert(
                            "value",
                            crate::sources::convert_json_to_element_value(&Value::Number(
                                seq.into(),
                            ))
                            .unwrap(),
                        );
                        property_map.insert(
                            "timestamp",
                            crate::sources::convert_json_to_element_value(&Value::String(
                                chrono::Utc::now().to_rfc3339(),
                            ))
                            .unwrap(),
                        );

                        let metadata = ElementMetadata {
                            reference,
                            labels: Arc::from(vec![Arc::from("Counter")]),
                            effective_from: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_nanos() as u64,
                        };

                        let element = Element::Node {
                            metadata,
                            properties: property_map,
                        };

                        SourceChange::Insert { element }
                    }
                    "sensor" => {
                        let sensor_id = rand::random::<u32>() % 5;
                        let element_id = format!("reading_{}_{}", sensor_id, seq);
                        let reference = ElementReference::new(&source_name, &element_id);

                        let mut property_map = ElementPropertyMap::new();
                        property_map.insert(
                            "sensor_id",
                            crate::sources::convert_json_to_element_value(&Value::String(format!(
                                "sensor_{}",
                                sensor_id
                            )))
                            .unwrap(),
                        );
                        property_map.insert(
                            "temperature",
                            crate::sources::convert_json_to_element_value(&Value::Number(
                                serde_json::Number::from_f64(20.0 + rand::random::<f64>() * 10.0)
                                    .unwrap(),
                            ))
                            .unwrap(),
                        );
                        property_map.insert(
                            "humidity",
                            crate::sources::convert_json_to_element_value(&Value::Number(
                                serde_json::Number::from_f64(40.0 + rand::random::<f64>() * 20.0)
                                    .unwrap(),
                            ))
                            .unwrap(),
                        );
                        property_map.insert(
                            "timestamp",
                            crate::sources::convert_json_to_element_value(&Value::String(
                                chrono::Utc::now().to_rfc3339(),
                            ))
                            .unwrap(),
                        );

                        let metadata = ElementMetadata {
                            reference,
                            labels: Arc::from(vec![Arc::from("SensorReading")]),
                            effective_from: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_nanos() as u64,
                        };

                        let element = Element::Node {
                            metadata,
                            properties: property_map,
                        };

                        SourceChange::Insert { element }
                    }
                    _ => {
                        // Generic data
                        let element_id = format!("generic_{}", seq);
                        let reference = ElementReference::new(&source_name, &element_id);

                        let mut property_map = ElementPropertyMap::new();
                        property_map.insert(
                            "value",
                            crate::sources::convert_json_to_element_value(&Value::Number(
                                rand::random::<i32>().into(),
                            ))
                            .unwrap(),
                        );
                        property_map.insert(
                            "message",
                            crate::sources::convert_json_to_element_value(&Value::String(
                                "Generic mock data".to_string(),
                            ))
                            .unwrap(),
                        );
                        property_map.insert(
                            "timestamp",
                            crate::sources::convert_json_to_element_value(&Value::String(
                                chrono::Utc::now().to_rfc3339(),
                            ))
                            .unwrap(),
                        );

                        let metadata = ElementMetadata {
                            reference,
                            labels: Arc::from(vec![Arc::from("Generic")]),
                            effective_from: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap()
                                .as_nanos() as u64,
                        };

                        let element = Element::Node {
                            metadata,
                            properties: property_map,
                        };

                        SourceChange::Insert { element }
                    }
                };

                let wrapper = SourceEventWrapper {
                    source_id: source_id.clone(),
                    event: SourceEvent::Change(source_change),
                    timestamp: chrono::Utc::now(),
                };

                if let Err(e) = source_event_tx.send(wrapper).await {
                    error!("Failed to publish change: {}", e);
                    break;
                }
            }

            info!("Mock source task completed");
        });

        *self.task_handle.write().await = Some(task);
        *self.status.write().await = ComponentStatus::Running;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Running,
            timestamp: chrono::Utc::now(),
            message: Some("Mock source started successfully".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log_component_stop("Mock Source", &self.config.id);

        *self.status.write().await = ComponentStatus::Stopping;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Stopping,
            timestamp: chrono::Utc::now(),
            message: Some("Stopping mock source".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        // Cancel the task
        if let Some(handle) = self.task_handle.write().await.take() {
            handle.abort();
            let _ = handle.await;
        }

        *self.status.write().await = ComponentStatus::Stopped;

        let event = ComponentEvent {
            component_id: self.config.id.clone(),
            component_type: ComponentType::Source,
            status: ComponentStatus::Stopped,
            timestamp: chrono::Utc::now(),
            message: Some("Mock source stopped successfully".to_string()),
        };

        if let Err(e) = self.event_tx.send(event).await {
            error!("Failed to send component event: {}", e);
        }

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &SourceConfig {
        &self.config
    }
}
