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

//! Test source for integration testing
//!
//! Provides a simple source that allows manual event injection for testing reactions.

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{Element, ElementMetadata, ElementPropertyMap, SourceChange};
use drasi_lib::channels::*;
use drasi_lib::channels::dispatcher::{ChangeDispatcher, ChannelChangeDispatcher};
use drasi_lib::plugin_core::Source;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A test source that allows manual event injection
pub struct TestSource {
    id: String,
    status: Arc<RwLock<ComponentStatus>>,
    event_tx: Arc<RwLock<Option<ComponentEventSender>>>,
    dispatchers: Arc<RwLock<Vec<Box<dyn ChangeDispatcher<SourceEventWrapper>>>>>,
}

impl TestSource {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            event_tx: Arc::new(RwLock::new(None)),
            dispatchers: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Send a node insert event
    pub async fn send_node_insert(
        &self,
        node_id: impl Into<String>,
        labels: Vec<&str>,
        properties: Value,
    ) -> Result<()> {
        let props = if let Value::Object(map) = properties {
            ElementPropertyMap::from_iter(
                map.into_iter()
                    .map(|(k, v)| (k, drasi_core::models::ElementValue::from_json(&v))),
            )
        } else {
            ElementPropertyMap::new()
        };

        let element = Element::Node {
            metadata: ElementMetadata {
                reference: drasi_core::models::ElementReference::new(self.id.clone(), node_id.into()),
                labels: labels.into_iter().map(|s| s.to_string()).collect(),
                effective_from: chrono::Utc::now().timestamp() as u64,
            },
            properties: props,
        };

        self.inject_change(SourceChange::Insert { element }).await
    }

    /// Send a node update event
    pub async fn send_node_update(
        &self,
        node_id: impl Into<String>,
        labels: Vec<&str>,
        old_properties: Value,
        new_properties: Value,
    ) -> Result<()> {
        let old_props = if let Value::Object(map) = old_properties {
            ElementPropertyMap::from_iter(
                map.into_iter()
                    .map(|(k, v)| (k, drasi_core::models::ElementValue::from_json(&v))),
            )
        } else {
            ElementPropertyMap::new()
        };

        let new_props = if let Value::Object(map) = new_properties {
            ElementPropertyMap::from_iter(
                map.into_iter()
                    .map(|(k, v)| (k, drasi_core::models::ElementValue::from_json(&v))),
            )
        } else {
            ElementPropertyMap::new()
        };

        let element_ref = drasi_core::models::ElementReference::new(self.id.clone(), node_id.into());

        self.inject_change(SourceChange::Update {
            element_ref,
            labels: labels.into_iter().map(|s| s.to_string()).collect(),
            old_properties: old_props,
            new_properties: new_props,
        })
        .await
    }

    /// Send a node delete event
    pub async fn send_node_delete(
        &self,
        node_id: impl Into<String>,
        labels: Vec<&str>,
        properties: Value,
    ) -> Result<()> {
        let props = if let Value::Object(map) = properties {
            ElementPropertyMap::from_iter(
                map.into_iter()
                    .map(|(k, v)| (k, drasi_core::models::ElementValue::from_json(&v))),
            )
        } else {
            ElementPropertyMap::new()
        };

        let element = Element::Node {
            metadata: ElementMetadata {
                reference: drasi_core::models::ElementReference::new(self.id.clone(), node_id.into()),
                labels: labels.into_iter().map(|s| s.to_string()).collect(),
                effective_from: chrono::Utc::now().timestamp() as u64,
            },
            properties: props,
        };

        self.inject_change(SourceChange::Delete { element }).await
    }

    async fn inject_change(&self, change: SourceChange) -> Result<()> {
        let dispatchers = self.dispatchers.read().await;
        let wrapper = SourceEventWrapper::new(
            self.id.clone(),
            SourceEvent::Change(change),
            chrono::Utc::now(),
        );
        let arc_wrapper = Arc::new(wrapper);
        for dispatcher in dispatchers.iter() {
            dispatcher.dispatch_change(arc_wrapper.clone()).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Source for TestSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn type_name(&self) -> &str {
        "test"
    }

    fn properties(&self) -> HashMap<String, Value> {
        HashMap::new()
    }

    fn auto_start(&self) -> bool {
        true
    }

    async fn inject_event_tx(&self, tx: ComponentEventSender) {
        *self.event_tx.write().await = Some(tx);
    }

    async fn start(&self) -> Result<()> {
        *self.status.write().await = ComponentStatus::Running;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        *self.status.write().await = ComponentStatus::Stopped;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        *self.status.read().await
    }

    async fn add_dispatcher(&self, dispatcher: Box<dyn ChangeDispatcher<SourceEventWrapper>>) {
        self.dispatchers.write().await.push(dispatcher);
    }
}
