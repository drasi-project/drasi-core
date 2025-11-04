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
use log::{debug, info};
use serde_json::Value;
use std::sync::Arc;

use crate::channels::*;
use crate::config::SourceConfig;
use crate::sources::{base::SourceBase, Source};
use crate::utils::*;

/// Mock source that runs as an internal tokio task
pub struct MockSource {
    base: SourceBase,
}

impl MockSource {
    pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
        Ok(Self {
            base: SourceBase::new(config, event_tx)?,
        })
    }
}

#[async_trait]
impl Source for MockSource {
    async fn start(&self) -> Result<()> {
        log_component_start("Mock Source", &self.base.config.id);

        self.base.set_status(ComponentStatus::Starting).await;
        self.base
            .send_component_event(
                ComponentStatus::Starting,
                Some("Starting mock source".to_string()),
            )
            .await?;

        // Get broadcast_tx for publishing
        let base_dispatchers = self.base.dispatchers.clone();
        let source_id = self.base.config.id.clone();

        // Get configuration
        let (data_type, interval_ms) = match &self.base.config.config {
            crate::config::SourceSpecificConfig::Mock(mock_config) => {
                (mock_config.data_type.clone(), mock_config.interval_ms)
            }
            _ => ("generic".to_string(), 5000),
        };

        // Start the data generation task
        let status = Arc::clone(&self.base.status);
        let source_name = self.base.config.id.clone();
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

                // Create profiling metadata with timestamps
                let mut profiling = crate::profiling::ProfilingMetadata::new();
                profiling.source_send_ns = Some(crate::profiling::timestamp_ns());

                let wrapper = SourceEventWrapper::with_profiling(
                    source_id.clone(),
                    SourceEvent::Change(source_change),
                    chrono::Utc::now(),
                    profiling,
                );

                // Dispatch to all subscribers via helper
                if let Err(e) =
                    SourceBase::dispatch_from_task(base_dispatchers.clone(), wrapper, &source_id)
                        .await
                {
                    debug!("Failed to dispatch change: {}", e);
                }
            }

            info!("Mock source task completed");
        });

        *self.base.task_handle.write().await = Some(task);
        self.base.set_status(ComponentStatus::Running).await;

        self.base
            .send_component_event(
                ComponentStatus::Running,
                Some("Mock source started successfully".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log_component_stop("Mock Source", &self.base.config.id);

        self.base.set_status(ComponentStatus::Stopping).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopping,
                Some("Stopping mock source".to_string()),
            )
            .await?;

        // Cancel the task
        if let Some(handle) = self.base.task_handle.write().await.take() {
            handle.abort();
            let _ = handle.await;
        }

        self.base.set_status(ComponentStatus::Stopped).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopped,
                Some("Mock source stopped successfully".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
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
        self.base
            .subscribe_with_bootstrap(
                query_id,
                enable_bootstrap,
                node_labels,
                relation_labels,
                "Mock",
            )
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

impl MockSource {
    /// Inject a test event into the mock source for testing purposes
    /// This allows tests to send specific events without relying on automatic generation
    pub async fn inject_event(&self, change: SourceChange) -> Result<()> {
        self.base.dispatch_source_change(change).await
    }

    /// Create a test subscription to this source
    ///
    /// This method delegates to SourceBase and is provided for convenience in tests.
    pub fn test_subscribe(
        &self,
    ) -> Box<dyn crate::channels::ChangeReceiver<crate::channels::SourceEventWrapper>> {
        self.base.test_subscribe()
    }
}
