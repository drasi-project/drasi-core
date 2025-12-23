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
use log::info;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

// Import real Drasi Source SDK
use drasi_core::models::{ElementPropertyMap, ElementValue};
use ordered_float::OrderedFloat;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::channels::*;
use crate::config::SourceRuntime;
use crate::managers::{is_operation_valid, log_component_error, Operation};
use crate::plugin_core::{Source, StateStoreProvider};

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
    event_tx: ComponentEventSender,
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
}

impl SourceManager {
    /// Create a new SourceManager
    pub fn new(event_tx: ComponentEventSender) -> Self {
        Self {
            sources: Arc::new(RwLock::new(HashMap::new())),
            event_tx,
            state_store: Arc::new(RwLock::new(None)),
        }
    }

    /// Inject the state store provider (called after DrasiLib is fully constructed)
    ///
    /// This allows sources to access the state store when they are added.
    pub async fn inject_state_store(&self, state_store: Arc<dyn StateStoreProvider>) {
        *self.state_store.write().await = Some(state_store);
    }

    pub async fn get_source_instance(&self, id: &str) -> Option<Arc<dyn Source>> {
        self.sources.read().await.get(id).cloned()
    }

    /// Add a source instance, taking ownership and wrapping it in an Arc internally.
    ///
    /// # Parameters
    /// - `source`: The source instance to add (ownership is transferred)
    ///
    /// # Returns
    /// - `Ok(())` if the source was added successfully
    /// - `Err` if a source with the same ID already exists
    ///
    /// # Note
    /// The source will NOT be auto-started. Call `start_source` separately
    /// if you need to start it after adding.
    ///
    /// The event channel and state store provider are automatically injected
    /// into the source before it is stored.
    ///
    /// # Example
    /// ```ignore
    /// let source = MySource::new("my-source", config)?;
    /// manager.add_source(source).await?;  // Ownership transferred
    /// ```
    pub async fn add_source(&self, source: impl Source + 'static) -> Result<()> {
        let source: Arc<dyn Source> = Arc::new(source);
        let source_id = source.id().to_string();

        // Inject the event channel before storing
        source.inject_event_tx(self.event_tx.clone()).await;

        // Inject the state store if available
        if let Some(state_store) = self.state_store.read().await.as_ref() {
            source.inject_state_store(state_store.clone()).await;
        }

        // Use a single write lock to atomically check and insert
        // This eliminates the TOCTOU race condition from separate read-then-write
        {
            let mut sources = self.sources.write().await;
            if sources.contains_key(&source_id) {
                return Err(anyhow::anyhow!(
                    "Source with id '{source_id}' already exists"
                ));
            }
            sources.insert(source_id.clone(), source);
        }

        info!("Added source: {source_id}");
        Ok(())
    }

    pub async fn start_source(&self, id: String) -> Result<()> {
        let source = {
            let sources = self.sources.read().await;
            sources.get(&id).cloned()
        };

        if let Some(source) = source {
            let status = source.status().await;
            is_operation_valid(&status, &Operation::Start).map_err(|e| anyhow::anyhow!(e))?;
            source.start().await?;
        } else {
            return Err(anyhow::anyhow!("Source not found: {id}"));
        }

        Ok(())
    }

    pub async fn stop_source(&self, id: String) -> Result<()> {
        let source = {
            let sources = self.sources.read().await;
            sources.get(&id).cloned()
        };

        if let Some(source) = source {
            let status = source.status().await;
            is_operation_valid(&status, &Operation::Stop).map_err(|e| anyhow::anyhow!(e))?;
            source.stop().await?;
        } else {
            return Err(anyhow::anyhow!("Source not found: {id}"));
        }

        Ok(())
    }

    pub async fn get_source_status(&self, id: String) -> Result<ComponentStatus> {
        let source = {
            let sources = self.sources.read().await;
            sources.get(&id).cloned()
        };

        if let Some(source) = source {
            Ok(source.status().await)
        } else {
            Err(anyhow::anyhow!("Source not found: {id}"))
        }
    }

    pub async fn list_sources(&self) -> Vec<(String, ComponentStatus)> {
        let sources: Vec<(String, Arc<dyn Source>)> = {
            let sources = self.sources.read().await;
            sources
                .iter()
                .map(|(id, source)| (id.clone(), source.clone()))
                .collect()
        };

        let mut result = Vec::new();
        for (id, source) in sources {
            let status = source.status().await;
            result.push((id, status));
        }

        result
    }

    pub async fn get_source(&self, id: String) -> Result<SourceRuntime> {
        let source = {
            let sources = self.sources.read().await;
            sources.get(&id).cloned()
        };

        if let Some(source) = source {
            let status = source.status().await;
            let runtime = SourceRuntime {
                id: source.id().to_string(),
                source_type: source.type_name().to_string(),
                status: status.clone(),
                error_message: match &status {
                    ComponentStatus::Error => Some("Source error occurred".to_string()),
                    _ => None,
                },
                properties: source.properties(),
            };
            Ok(runtime)
        } else {
            Err(anyhow::anyhow!("Source not found: {id}"))
        }
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
                info!("Stopping source '{id}' before deletion");
                source.stop().await?;

                // Wait a bit to ensure the source has fully stopped
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

                // Verify it's stopped
                let new_status = source.status().await;
                if !matches!(new_status, ComponentStatus::Stopped) {
                    return Err(anyhow::anyhow!(
                        "Failed to stop source '{id}' before deletion"
                    ));
                }
            } else {
                // Still validate the operation for non-running states
                is_operation_valid(&status, &Operation::Delete).map_err(|e| anyhow::anyhow!(e))?;
            }

            // Now remove the source
            self.sources.write().await.remove(&id);
            info!("Deleted source: {id}");

            Ok(())
        } else {
            Err(anyhow::anyhow!("Source not found: {id}"))
        }
    }

    /// Start all sources that have `auto_start` enabled.
    ///
    /// Sources must have been added via `add_source()` first, which injects
    /// the necessary event channel.
    ///
    /// Only sources with `auto_start() == true` will be started.
    pub async fn start_all(&self) -> Result<()> {
        let sources: Vec<Arc<dyn Source>> = {
            let sources = self.sources.read().await;
            sources.values().cloned().collect()
        };

        let mut failed_sources = Vec::new();

        for source in sources {
            // Only start sources with auto_start enabled
            if !source.auto_start() {
                info!("Skipping source '{}' (auto_start=false)", source.id());
                continue;
            }

            let source_id = source.id().to_string();
            info!("Starting source: {source_id}");
            if let Err(e) = source.start().await {
                log_component_error("Source", &source_id, &e.to_string());
                failed_sources.push((source_id, e.to_string()));
                // Continue starting other sources instead of returning early
            }
        }

        // Return error only if any sources failed to start
        if !failed_sources.is_empty() {
            let error_msg = failed_sources
                .iter()
                .map(|(id, err)| format!("{id}: {err}"))
                .collect::<Vec<_>>()
                .join(", ");
            Err(anyhow::anyhow!("Failed to start some sources: {error_msg}"))
        } else {
            Ok(())
        }
    }

    pub async fn stop_all(&self) -> Result<()> {
        let sources: Vec<Arc<dyn Source>> = {
            let sources = self.sources.read().await;
            sources.values().cloned().collect()
        };

        for source in sources {
            if let Err(e) = source.stop().await {
                log_component_error("Source", source.id(), &e.to_string());
            }
        }
        Ok(())
    }
}
