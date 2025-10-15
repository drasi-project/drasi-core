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
use log::{error, info };
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// Import real Drasi Source SDK
use drasi_core::models::{ElementPropertyMap, ElementValue};
use ordered_float::OrderedFloat;
use serde_json::Value;
use std::collections::BTreeMap;

use super::{
    ApplicationSource, ApplicationSourceHandle, GrpcSource, HttpSource, MockSource, PlatformSource,
};
use crate::channels::*;
use crate::config::{SourceConfig, SourceRuntime};
use crate::utils::*;

#[async_trait]
pub trait Source: Send + Sync {
    async fn start(&self) -> Result<()>;
    async fn stop(&self) -> Result<()>;
    async fn status(&self) -> ComponentStatus;
    fn get_config(&self) -> &SourceConfig;
}

// Convert JSON value to ElementValue
pub fn convert_json_to_element_value(value: &Value) -> Result<ElementValue> {
    match value {
        Value::String(s) => Ok(ElementValue::String(Arc::from(s.as_str()))),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(ElementValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(ElementValue::Float(OrderedFloat(f)))
            } else {
                Ok(ElementValue::String(Arc::from(n.to_string())))
            }
        }
        Value::Bool(b) => Ok(ElementValue::Bool(*b)),
        Value::Null => Ok(ElementValue::Null),
        // For arrays and objects, convert to string representation
        Value::Array(_) | Value::Object(_) => {
            Ok(ElementValue::String(Arc::from(value.to_string())))
        }
    }
}

// Convert JSON properties to ElementPropertyMap
pub fn convert_json_to_element_properties(
    json_props: &serde_json::Map<String, Value>,
) -> Result<ElementPropertyMap> {
    let mut properties = BTreeMap::new();

    for (key, value) in json_props {
        let element_value = convert_json_to_element_value(value)?;

        properties.insert(Arc::from(key.as_str()), element_value);
    }

    let mut property_map = ElementPropertyMap::new();
    for (key, value) in properties {
        property_map.insert(&key, value);
    }
    Ok(property_map)
}

pub struct SourceManager {
    sources: Arc<RwLock<HashMap<String, Arc<dyn Source>>>>,
    source_event_tx: SourceEventSender,
    event_tx: ComponentEventSender,
    application_handles: Arc<RwLock<HashMap<String, ApplicationSourceHandle>>>,
}

impl SourceManager {
    /// Create a new SourceManager with unified event sender
    pub fn new(
        source_event_tx: SourceEventSender,
        event_tx: ComponentEventSender,
    ) -> Self {
        Self {
            sources: Arc::new(RwLock::new(HashMap::new())),
            source_event_tx,
            event_tx,
            application_handles: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn get_source_instance(&self, id: &str) -> Option<Arc<dyn Source>> {
        self.sources.read().await.get(id).cloned()
    }

    pub async fn get_application_handle(&self, id: &str) -> Option<ApplicationSourceHandle> {
        self.application_handles.read().await.get(id).cloned()
    }

    /// Get the source event sender for bootstrap provider registration
    pub fn get_source_event_sender(&self) -> SourceEventSender {
        self.source_event_tx.clone()
    }

    pub async fn add_source(&self, config: SourceConfig) -> Result<()> {
        self.add_source_internal(config, true ).await
    }

    pub async fn add_source_with_options(
        &self,
        config: SourceConfig,
        allow_auto_start: bool,
    ) -> Result<()> {
        self.add_source_internal(config, allow_auto_start)
            .await
    }

    pub async fn add_source_without_save(
        &self,
        config: SourceConfig,
        allow_auto_start: bool,
    ) -> Result<()> {
        self.add_source_internal(config, allow_auto_start )
            .await
    }

    async fn add_source_internal(
        &self,
        config: SourceConfig,
        allow_auto_start: bool
    ) -> Result<()> {
        // Check if source with this id already exists
        if self.sources.read().await.contains_key(&config.id) {
            return Err(anyhow::anyhow!(
                "Source with id '{}' already exists",
                config.id
            ));
        }

        let source: Arc<dyn Source> = match config.source_type.as_str() {
            // Internal Rust sources running as tokio tasks
            "mock" => Arc::new(MockSource::new(
                config.clone(),
                self.source_event_tx.clone(),
                self.event_tx.clone(),
            )),
            "postgres" => Arc::new(super::PostgresReplicationSource::new(
                config.clone(),
                self.source_event_tx.clone(),
                self.event_tx.clone(),
            )),
            "http" => Arc::new(HttpSource::new(
                config.clone(),
                self.source_event_tx.clone(),
                self.event_tx.clone(),
            )),
            "grpc" => Arc::new(GrpcSource::new(
                config.clone(),
                self.source_event_tx.clone(),
                self.event_tx.clone(),
            )),
            "platform" => Arc::new(PlatformSource::new(
                config.clone(),
                self.source_event_tx.clone(),
                self.event_tx.clone(),
            )),
            "application" => {
                let (app_source, handle) = ApplicationSource::new(
                    config.clone(),
                    self.source_event_tx.clone(),
                    self.event_tx.clone(),
                );
                // Store the handle for the application to use
                self.application_handles
                    .write()
                    .await
                    .insert(config.id.clone(), handle);
                Arc::new(app_source)
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "Unknown source type: {}",
                    config.source_type
                ));
            }
        };

        let source_id = config.id.clone();
        let should_auto_start = config.auto_start;

        self.sources.write().await.insert(config.id.clone(), source);
        info!("Added source: {}", config.id);

        // Auto-start the source if configured and allowed
        if should_auto_start && allow_auto_start {
            info!("Auto-starting source: {}", source_id);
            if let Err(e) = self.start_source(source_id.clone()).await {
                error!("Failed to auto-start source {}: {}", source_id, e);
                // Don't fail the add operation, just log the error
                // The source was successfully added, just not started
            }
        }

        Ok(())
    }

    pub async fn start_source(&self, id: String) -> Result<()> {
        let sources = self.sources.read().await;
        if let Some(source) = sources.get(&id) {
            let status = source.status().await;
            is_operation_valid(&status, &Operation::Start).map_err(|e| anyhow::anyhow!(e))?;
            source.start().await?;
        } else {
            return Err(anyhow::anyhow!("Source not found: {}", id));
        }

        Ok(())
    }

    pub async fn stop_source(&self, id: String) -> Result<()> {
        let sources = self.sources.read().await;
        if let Some(source) = sources.get(&id) {
            let status = source.status().await;
            is_operation_valid(&status, &Operation::Stop).map_err(|e| anyhow::anyhow!(e))?;
            source.stop().await?;
        } else {
            return Err(anyhow::anyhow!("Source not found: {}", id));
        }

        Ok(())
    }

    pub async fn get_source_status(&self, id: String) -> Result<ComponentStatus> {
        let sources = self.sources.read().await;
        if let Some(source) = sources.get(&id) {
            Ok(source.status().await)
        } else {
            Err(anyhow::anyhow!("Source not found: {}", id))
        }
    }

    pub async fn list_sources(&self) -> Vec<(String, ComponentStatus)> {
        let sources = self.sources.read().await;
        let mut result = Vec::new();

        for (id, source) in sources.iter() {
            let status = source.status().await;
            result.push((id.clone(), status));
        }

        result
    }

    pub async fn get_source_config(&self, id: &str) -> Option<SourceConfig> {
        let sources = self.sources.read().await;
        sources.get(id).map(|s| s.get_config().clone())
    }

    pub async fn get_source(&self, id: String) -> Result<SourceRuntime> {
        let sources = self.sources.read().await;
        if let Some(source) = sources.get(&id) {
            let status = source.status().await;
            let config = source.get_config();
            let runtime = SourceRuntime {
                id: config.id.clone(),
                source_type: config.source_type.clone(),
                status: status.clone(),
                error_message: match &status {
                    ComponentStatus::Error => Some("Source error occurred".to_string()),
                    _ => None,
                },
                properties: config.properties.clone(),
            };
            Ok(runtime)
        } else {
            Err(anyhow::anyhow!("Source not found: {}", id))
        }
    }

    pub async fn update_source(&self, id: String, config: SourceConfig) -> Result<()> {
        let sources = self.sources.read().await;
        if let Some(source) = sources.get(&id) {
            let status = source.status().await;
            let was_running =
                matches!(status, ComponentStatus::Running | ComponentStatus::Starting);

            // If running, we need to stop it first
            if was_running {
                drop(sources);
                self.stop_source(id.clone()).await?;
                // Re-acquire lock after stop
                let sources = self.sources.read().await;
                if let Some(source) = sources.get(&id) {
                    let status = source.status().await;
                    is_operation_valid(&status, &Operation::Update)
                        .map_err(|e| anyhow::anyhow!(e))?;
                }
                drop(sources);
            } else {
                is_operation_valid(&status, &Operation::Update).map_err(|e| anyhow::anyhow!(e))?;
                drop(sources);
            }

            // For now, update means remove and re-add
            self.delete_source(id.clone()).await?;
            // Add the new configuration
            self.add_source_internal(config, false ).await?;

            // If it was running, restart it
            if was_running {
                self.start_source(id).await?;
            }
        } else {
            return Err(anyhow::anyhow!("Source not found: {}", id));
        }

        Ok(())
    }

    pub async fn delete_source(&self, id: String) -> Result<()> {
        // First check if the source exists
        let source = {
            let sources = self.sources.read().await;
            sources.get(&id).cloned()
        };

        if let Some(source) = source {
            let status = source.status().await;

            // If the source is running, stop it first
            if matches!(status, ComponentStatus::Running) {
                info!("Stopping source '{}' before deletion", id);
                source.stop().await?;

                // Wait a bit to ensure the source has fully stopped
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Verify it's stopped
                let new_status = source.status().await;
                if !matches!(new_status, ComponentStatus::Stopped) {
                    return Err(anyhow::anyhow!(
                        "Failed to stop source '{}' before deletion",
                        id
                    ));
                }
            } else {
                // Still validate the operation for non-running states
                is_operation_valid(&status, &Operation::Delete).map_err(|e| anyhow::anyhow!(e))?;
            }

            // Now remove the source
            self.sources.write().await.remove(&id);
            info!("Deleted source: {}", id);

            Ok(())
        } else {
            Err(anyhow::anyhow!("Source not found: {}", id))
        }
    }

    pub async fn start_all(&self) -> Result<()> {
        let sources = self.sources.read().await;
        let mut failed_sources = Vec::new();

        for source in sources.values() {
            let config = source.get_config();
            if config.auto_start {
                info!("Auto-starting source: {}", config.id);
                if let Err(e) = source.start().await {
                    log_component_error("Source", &config.id, &e.to_string());
                    failed_sources.push((config.id.clone(), e.to_string()));
                    // Continue starting other sources instead of returning early
                }
            } else {
                info!(
                    "Source '{}' has auto_start=false, skipping automatic startup",
                    config.id
                );
            }
        }

        // Return error only if any sources failed to start
        if !failed_sources.is_empty() {
            let error_msg = failed_sources
                .iter()
                .map(|(id, err)| format!("{}: {}", id, err))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!(
                "Failed to start some sources: {}",
                error_msg
            ))
        } else {
            Ok(())
        }
    }

    pub async fn stop_all(&self) -> Result<()> {
        let sources = self.sources.read().await;
        for source in sources.values() {
            if let Err(e) = source.stop().await {
                log_component_error("Source", &source.get_config().id, &e.to_string());
            }
        }
        Ok(())
    }
}
