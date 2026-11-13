// Copyright 2026 The Drasi Authors.
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

use std::{collections::HashMap, sync::Arc};

use crate::{ComponentId, ComponentName};
use anyhow::Result;
use drasi_core::models::{ElementPropertyMap, ElementValue};
use ordered_float::OrderedFloat;
use serde_json::Value;

use crate::{
    channels::{ComponentEvent, ComponentStatus, ComponentType},
    computation::runtime::Runtime,
    config::SourceRuntime,
    observability::LogMessage,
    schema::SourceSchema,
    sources::Source,
};

/// Convert JSON values using the source plugin compatibility representation.
pub fn convert_json_to_element_value(value: &Value) -> ElementValue {
    match value {
        Value::String(s) => ElementValue::String(Arc::from(s.as_str())),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ElementValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                ElementValue::Float(OrderedFloat(f))
            } else {
                ElementValue::String(Arc::from(n.to_string()))
            }
        }
        Value::Bool(b) => ElementValue::Bool(*b),
        Value::Null => ElementValue::Null,
        Value::Array(_) | Value::Object(_) => ElementValue::String(Arc::from(value.to_string())),
    }
}

pub fn convert_json_to_element_properties(
    json_props: &serde_json::Map<String, Value>,
) -> ElementPropertyMap {
    let mut properties = ElementPropertyMap::new();
    for (key, value) in json_props {
        properties.insert(key, convert_json_to_element_value(value));
    }
    properties
}

/// Source operations on the instance's sole ComputationGraph.
///
/// This facade owns no component map, execution tasks, or independent lifecycle.
/// Configure shared services through the DrasiLib builder.
#[derive(Clone)]
pub struct SourceManager {
    runtime: Arc<Runtime>,
}

impl SourceManager {
    pub(crate) fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    pub async fn get_source_instance_by_id(&self, id: &ComponentId) -> Result<Arc<dyn Source>> {
        self.runtime.source(id).await
    }

    pub async fn provision_source(
        &self,
        name: crate::config::ComponentName,
        source: impl Source + 'static,
    ) -> Result<()> {
        self.runtime
            .add_source(name, Box::new(source), HashMap::new(), false)
            .await?;
        Ok(())
    }

    pub async fn start_source_by_id(&self, id: &ComponentId) -> Result<()> {
        self.runtime.start_component(id, "source").await
    }

    /// Stop a running, starting, or failed source by ID, transitioning it to Stopped.
    ///
    /// # Errors
    /// Returns an error if the source is not found or the stop operation fails.
    pub async fn stop_source_by_id(&self, id: &ComponentId) -> Result<()> {
        self.runtime.stop_component(id, "source").await
    }

    pub async fn get_source_status_by_id(&self, id: &ComponentId) -> Result<ComponentStatus> {
        self.runtime.component_status(id, "source").await
    }

    pub async fn list_sources(
        &self,
    ) -> crate::error::Result<Vec<crate::config::ComponentListEntry>> {
        Ok(self.runtime.list_components("source").await?)
    }

    pub async fn get_source_by_id(&self, id: &ComponentId) -> Result<SourceRuntime> {
        self.runtime.source_info(id).await
    }

    /// Inspect native or legacy source-data schemas. Unsupported discovery returns
    /// `None`; discovery and lifecycle errors remain errors.
    pub async fn get_source_schema_by_id(&self, id: &ComponentId) -> Result<Option<SourceSchema>> {
        self.runtime.source_schema(id).await
    }

    pub async fn teardown_source_by_id(&self, id: &ComponentId, cleanup: bool) -> Result<()> {
        self.runtime.remove_component(id, "source", cleanup).await
    }

    pub async fn update_source_by_id(
        &self,
        id: &ComponentId,
        definition: crate::management::OrdinaryComponentRecord,
    ) -> Result<()> {
        self.runtime
            .update_ordinary_definition(id, "source", definition)
            .await
    }

    pub async fn start_all(&self) -> Result<()> {
        self.runtime.start_kind("source").await
    }

    pub async fn stop_all(&self) -> Result<()> {
        self.runtime.stop_kind("source").await
    }

    pub async fn subscriptions_complete(&self) -> Result<()> {
        self.runtime.subscriptions_complete().await
    }

    pub async fn get_source_events_by_id(&self, id: &ComponentId) -> Result<Vec<ComponentEvent>> {
        Ok(self.runtime.component_events(id).await)
    }

    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        self.runtime.events(Some(ComponentType::Source)).await
    }

    pub async fn subscribe_logs_by_id(
        &self,
        id: &ComponentId,
    ) -> crate::error::Result<(
        Vec<LogMessage>,
        tokio::sync::broadcast::Receiver<LogMessage>,
    )> {
        Ok(self.runtime.subscribe_logs(id, "source").await?)
    }

    pub async fn subscribe_events_by_id(
        &self,
        id: &ComponentId,
    ) -> crate::error::Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        Ok(self.runtime.subscribe_events(id, "source").await?)
    }

    pub async fn get_source_instance_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<Arc<dyn Source>> {
        let id = self.runtime.component_id_by_name(name, Some("source"))?;
        self.get_source_instance_by_id(&id).await
    }

    pub async fn start_source_by_name(&self, name: &ComponentName) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("source"))?;
        self.start_source_by_id(&id).await
    }

    pub async fn stop_source_by_name(&self, name: &ComponentName) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("source"))?;
        self.stop_source_by_id(&id).await
    }

    pub async fn get_source_status_by_name(&self, name: &ComponentName) -> Result<ComponentStatus> {
        let id = self.runtime.component_id_by_name(name, Some("source"))?;
        self.get_source_status_by_id(&id).await
    }

    pub async fn get_source_by_name(&self, name: &ComponentName) -> Result<SourceRuntime> {
        let id = self.runtime.component_id_by_name(name, Some("source"))?;
        self.get_source_by_id(&id).await
    }

    pub async fn get_source_schema_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<Option<SourceSchema>> {
        let id = self.runtime.component_id_by_name(name, Some("source"))?;
        self.get_source_schema_by_id(&id).await
    }

    pub async fn teardown_source_by_name(&self, name: &ComponentName, cleanup: bool) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("source"))?;
        self.teardown_source_by_id(&id, cleanup).await
    }

    pub async fn update_source_by_name(
        &self,
        name: &ComponentName,
        definition: crate::management::OrdinaryComponentRecord,
    ) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("source"))?;
        self.update_source_by_id(&id, definition).await
    }

    pub async fn get_source_events_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<Vec<ComponentEvent>> {
        let id = self.runtime.component_id_by_name(name, Some("source"))?;
        self.get_source_events_by_id(&id).await
    }

    pub async fn subscribe_logs_by_name(
        &self,
        name: &ComponentName,
    ) -> crate::error::Result<(
        Vec<LogMessage>,
        tokio::sync::broadcast::Receiver<LogMessage>,
    )> {
        let id = self.runtime.component_id_by_name(name, Some("source"))?;
        self.subscribe_logs_by_id(&id).await
    }

    pub async fn subscribe_events_by_name(
        &self,
        name: &ComponentName,
    ) -> crate::error::Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        let id = self.runtime.component_id_by_name(name, Some("source"))?;
        self.subscribe_events_by_id(&id).await
    }
}
