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

use anyhow::Result;
use drasi_core::models::{ElementPropertyMap, ElementValue};
use ordered_float::OrderedFloat;
use serde_json::Value;

use crate::{
    channels::{ComponentEvent, ComponentStatus, ComponentType},
    computation::runtime::Runtime,
    config::SourceRuntime,
    managers::LogMessage,
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

    pub async fn get_source_instance(&self, id: &str) -> Option<Arc<dyn Source>> {
        self.runtime.source(id).await.ok()
    }

    pub async fn provision_source(&self, source: impl Source + 'static) -> Result<()> {
        self.runtime
            .add_source(Box::new(source), HashMap::new(), false)
            .await?;
        Ok(())
    }

    pub async fn start_source(&self, id: String) -> Result<()> {
        self.runtime.start_component(&id, "source").await
    }

    pub async fn stop_source(&self, id: String) -> Result<()> {
        self.runtime.stop_component(&id, "source").await
    }

    pub async fn get_source_status(&self, id: String) -> Result<ComponentStatus> {
        self.runtime.component_status(&id, "source").await
    }

    pub async fn list_sources(&self) -> Vec<(String, ComponentStatus)> {
        self.runtime
            .list_components("source")
            .await
            .expect("initialized computation registry")
    }

    pub async fn get_source(&self, id: String) -> Result<SourceRuntime> {
        self.runtime.source_info(&id).await
    }

    pub async fn get_source_schema(&self, id: String) -> Result<Option<SourceSchema>> {
        Ok(self.runtime.source(&id).await?.describe_schema())
    }

    pub async fn teardown_source(&self, id: String, cleanup: bool) -> Result<()> {
        self.runtime.remove_component(&id, "source", cleanup).await
    }

    pub async fn update_source(&self, id: String, source: impl Source + 'static) -> Result<()> {
        self.runtime.update_source(&id, Box::new(source)).await
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

    pub async fn get_source_events(&self, id: &str) -> Vec<ComponentEvent> {
        self.runtime.component_events(id).await
    }

    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        self.runtime.events(Some(ComponentType::Source)).await
    }

    pub async fn subscribe_logs(
        &self,
        id: &str,
    ) -> Option<(
        Vec<LogMessage>,
        tokio::sync::broadcast::Receiver<LogMessage>,
    )> {
        self.runtime.subscribe_logs(id, "source").await.ok()
    }

    pub async fn subscribe_events(
        &self,
        id: &str,
    ) -> Option<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        self.runtime.subscribe_events(id, "source").await.ok()
    }
}
