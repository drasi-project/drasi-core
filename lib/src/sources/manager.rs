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
use log::{info, warn};
use std::sync::Arc;
use tokio::sync::RwLock;

// Import real Drasi Source SDK
use drasi_core::models::{ElementPropertyMap, ElementValue};
use ordered_float::OrderedFloat;
use serde_json::Value;
use std::collections::BTreeMap;

use crate::channels::*;
use crate::component_graph::{ComponentGraph, ComponentKind, ComponentUpdateSender};
use crate::config::SourceRuntime;
use crate::context::SourceRuntimeContext;
use crate::managers::{ComponentLogKey, ComponentLogRegistry};
use crate::sources::Source;
use crate::state_store::StateStoreProvider;

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
    instance_id: String,
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    log_registry: Arc<ComponentLogRegistry>,
    /// Shared component graph — the single source of truth for component metadata,
    /// state, relationships, runtime instances, AND event history.
    graph: Arc<RwLock<ComponentGraph>>,
    /// Channel sender for routing status updates through the graph update loop.
    /// Managers send transitional states (Starting, Stopping, Reconfiguring) here;
    /// the loop applies them to the graph and records events automatically.
    update_tx: ComponentUpdateSender,
}

impl SourceManager {
    /// Create a new SourceManager
    ///
    /// # Parameters
    /// - `instance_id`: The DrasiLib instance ID for log routing
    /// - `log_registry`: Shared log registry for component log streaming
    /// - `graph`: Shared component graph for tracking component relationships and emitting events
    pub fn new(
        instance_id: impl Into<String>,
        log_registry: Arc<ComponentLogRegistry>,
        graph: Arc<RwLock<ComponentGraph>>,
        update_tx: ComponentUpdateSender,
    ) -> Self {
        Self {
            instance_id: instance_id.into(),
            state_store: Arc::new(RwLock::new(None)),
            log_registry,
            graph,
            update_tx,
        }
    }

    /// Inject the state store provider (called after DrasiLib is fully constructed)
    ///
    /// This allows sources to access the state store when they are added.
    pub async fn inject_state_store(&self, state_store: Arc<dyn StateStoreProvider>) {
        *self.state_store.write().await = Some(state_store);
    }

    /// Get the runtime instance for a source by ID.
    ///
    /// Returns `None` if no runtime is registered for the given ID.
    pub async fn get_source_instance(&self, id: &str) -> Option<Arc<dyn Source>> {
        let graph = self.graph.read().await;
        graph.get_runtime::<Arc<dyn Source>>(id).cloned()
    }

    /// Provision a source instance for runtime — initialize and store it.
    ///
    /// This method handles runtime-only operations: creating the runtime context,
    /// initializing the source, and storing it in the runtime map. Graph registration
    /// (node creation, ownership edges) must be done by the caller beforehand via
    /// `ComponentGraph::register_source()`.
    ///
    /// # Parameters
    /// - `source`: The source instance to provision (ownership is transferred)
    ///
    /// # Note
    /// The source will NOT be auto-started. Call `start_source` separately
    /// if you need to start it after adding.
    pub async fn provision_source(&self, source: impl Source + 'static) -> Result<()> {
        let source: Arc<dyn Source> = Arc::new(source);
        let source_id = source.id().to_string();

        // Construct runtime context for this source (includes update channel for status reporting)
        let context = SourceRuntimeContext::new(
            &self.instance_id,
            &source_id,
            self.state_store.read().await.clone(),
            self.update_tx.clone(),
            None,
        );

        // Initialize the source with its runtime context
        source.initialize(context).await;

        // Store the runtime instance in the graph
        {
            let mut graph = self.graph.write().await;
            graph.set_runtime(&source_id, Box::new(source))?;
        }

        info!("Provisioned source: {source_id}");

        Ok(())
    }

    /// Start a source by ID, transitioning it to the Running state.
    ///
    /// # Errors
    /// Returns an error if the source is not found or the start operation fails.
    pub async fn start_source(&self, id: String) -> Result<()> {
        let source =
            crate::managers::lifecycle_helpers::get_runtime::<Arc<dyn Source>>(&self.graph, &id)
                .await
                .ok_or_else(|| {
                    anyhow::Error::new(crate::managers::ComponentNotFoundError::new("source", &id))
                })?;

        crate::managers::lifecycle_helpers::start_component(&self.graph, &id, "source", &source)
            .await
    }

    /// Stop a running source by ID, transitioning it to the Stopped state.
    ///
    /// # Errors
    /// Returns an error if the source is not found or the stop operation fails.
    pub async fn stop_source(&self, id: String) -> Result<()> {
        let source =
            crate::managers::lifecycle_helpers::get_runtime::<Arc<dyn Source>>(&self.graph, &id)
                .await
                .ok_or_else(|| {
                    anyhow::Error::new(crate::managers::ComponentNotFoundError::new("source", &id))
                })?;

        crate::managers::lifecycle_helpers::stop_component(&self.graph, &id, "source", &source)
            .await
    }

    /// Get the current status of a source by ID.
    ///
    /// # Errors
    /// Returns an error if the source is not found in the component graph.
    pub async fn get_source_status(&self, id: String) -> Result<ComponentStatus> {
        crate::managers::lifecycle_helpers::get_component_status(&self.graph, &id, "Source").await
    }

    /// List all registered sources with their current statuses.
    pub async fn list_sources(&self) -> Vec<(String, ComponentStatus)> {
        crate::managers::lifecycle_helpers::list_components(&self.graph, &ComponentKind::Source)
            .await
    }

    /// Get the full runtime descriptor for a source, including its status and properties.
    ///
    /// # Errors
    /// Returns an error if the source is not found.
    pub async fn get_source(&self, id: String) -> Result<SourceRuntime> {
        let graph = self.graph.read().await;
        let source = graph.get_runtime::<Arc<dyn Source>>(&id).cloned();

        if let Some(source) = source {
            let status = graph
                .get_component(&id)
                .map(|n| n.status)
                .unwrap_or(ComponentStatus::Stopped);
            let error_message = match &status {
                ComponentStatus::Error => graph.get_last_error(&id),
                _ => None,
            };
            drop(graph);
            let runtime = SourceRuntime {
                id: source.id().to_string(),
                source_type: source.type_name().to_string(),
                status,
                error_message,
                properties: source.properties(),
            };
            Ok(runtime)
        } else {
            Err(crate::managers::ComponentNotFoundError::new("source", &id).into())
        }
    }

    /// Teardown a source's runtime state — stop, deprovision, and remove from runtime map.
    ///
    /// This method handles runtime-only operations. Graph deregistration
    /// (node removal, edge cleanup) must be done by the caller afterwards via
    /// `ComponentGraph::deregister()`.
    ///
    /// The caller should validate dependencies via `graph.can_remove()` before calling this.
    pub async fn teardown_source(&self, id: String, cleanup: bool) -> Result<()> {
        crate::managers::lifecycle_helpers::teardown_component::<Arc<dyn Source>, _, _>(
            &self.graph,
            &id,
            "source",
            ComponentType::Source,
            &self.instance_id,
            &self.log_registry,
            cleanup,
            || async {},
        )
        .await
    }

    /// Update a source by replacing it with a new instance.
    ///
    /// Flow: validate exists → validate status → set Reconfiguring via graph →
    /// stop if running/starting → wait for stopped → initialize new →
    /// replace (if still exists) → restart if was running.
    /// Log and event history are preserved.
    pub async fn update_source(&self, id: String, new_source: impl Source + 'static) -> Result<()> {
        let old_source = {
            let graph = self.graph.read().await;
            graph.get_runtime::<Arc<dyn Source>>(&id).cloned()
        };

        if let Some(old_source) = old_source {
            // Verify the new source has the same ID
            if new_source.id() != id {
                return Err(anyhow::anyhow!(
                    "New source ID '{}' does not match existing source ID '{}'",
                    new_source.id(),
                    id
                ));
            }

            let graph = &self.graph;
            let instance_id = &self.instance_id;
            let state_store = &self.state_store;
            let update_tx = &self.update_tx;

            crate::managers::lifecycle_helpers::reconfigure_component::<Arc<dyn Source>, _, _, _>(
                graph,
                &id,
                "source",
                &old_source,
                || async {},
                || async {
                    let new_source: Arc<dyn Source> = Arc::new(new_source);
                    let context = SourceRuntimeContext::new(
                        instance_id,
                        &id,
                        state_store.read().await.clone(),
                        update_tx.clone(),
                        None,
                    );
                    new_source.initialize(context).await;

                    let mut g = graph.write().await;
                    if !g.has_runtime(&id) {
                        return Err(anyhow::anyhow!(
                            "Source '{id}' was concurrently deleted during reconfiguration"
                        ));
                    }
                    g.set_runtime(&id, Box::new(new_source))?;
                    Ok(())
                },
                || self.start_source(id.clone()),
            )
            .await
        } else {
            Err(crate::managers::ComponentNotFoundError::new("source", &id).into())
        }
    }

    /// Start all sources that have `auto_start` enabled.
    ///
    /// Sources must have been added via `add_source()` first, which injects
    /// the necessary event channel.
    ///
    /// Only sources with `auto_start() == true` will be started.
    pub async fn start_all(&self) -> Result<()> {
        crate::managers::lifecycle_helpers::start_all_components::<Arc<dyn Source>, _, _>(
            &self.graph,
            &ComponentKind::Source,
            "source",
            |s| s.auto_start(),
            |id, source| async move {
                // Validate and apply Starting transition atomically through the graph
                {
                    let mut graph = self.graph.write().await;
                    graph.validate_and_transition(
                        &id,
                        ComponentStatus::Starting,
                        Some("Starting source".to_string()),
                    )?;
                }

                if let Err(e) = source.start().await {
                    let mut graph = self.graph.write().await;
                    let _ = graph.validate_and_transition(
                        &id,
                        ComponentStatus::Error,
                        Some(format!("Start failed: {e}")),
                    );
                    return Err(e);
                }
                Ok(())
            },
        )
        .await
    }

    /// Stop all running sources.
    ///
    /// # Errors
    /// Returns an error if any source fails to stop.
    pub async fn stop_all(&self) -> Result<()> {
        crate::managers::lifecycle_helpers::stop_all_components(
            &self.graph,
            &ComponentKind::Source,
            "Source",
            |id| self.stop_source(id),
        )
        .await
    }

    /// Record a component event in the history.
    ///
    /// This should be called by the event processing loop to track component
    /// Record a component event — delegates to the graph's centralized event history.
    ///
    /// Note: In most cases, events are recorded automatically by `apply_update()`.
    /// This method is retained for backward compatibility and edge cases.
    pub async fn record_event(&self, event: ComponentEvent) {
        let mut graph = self.graph.write().await;
        graph.record_event(event);
    }

    /// Get events for a specific source.
    ///
    /// Returns events in chronological order (oldest first).
    pub async fn get_source_events(&self, id: &str) -> Vec<ComponentEvent> {
        self.graph.read().await.get_events(id)
    }

    /// Get all events across all sources.
    ///
    /// Returns events sorted by timestamp (oldest first).
    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        let graph = self.graph.read().await;
        graph
            .get_all_events()
            .into_iter()
            .filter(|e| e.component_type == ComponentType::Source)
            .collect()
    }

    /// Subscribe to live logs for a source.
    ///
    /// Returns the log history and a broadcast receiver for new logs.
    /// Returns None if the source doesn't exist.
    pub async fn subscribe_logs(
        &self,
        id: &str,
    ) -> Option<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        // Verify the source exists in the graph
        {
            let graph = self.graph.read().await;
            if !graph.has_runtime(id) {
                return None;
            }
        }

        let log_key = ComponentLogKey::new(&self.instance_id, ComponentType::Source, id);
        Some(self.log_registry.subscribe_by_key(&log_key).await)
    }

    /// Subscribe to live events for a source.
    ///
    /// Returns the event history and a broadcast receiver for new events.
    /// Returns None if the source doesn't exist.
    pub async fn subscribe_events(
        &self,
        id: &str,
    ) -> Option<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        let mut graph = self.graph.write().await;
        if !graph.has_runtime(id) {
            return None;
        }
        Some(graph.subscribe_events(id))
    }
}
