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

//! Mock source for dashboard integration tests.

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::channels::{
    ComponentStatus, DispatchMode, SourceEvent, SourceEventWrapper, SubscriptionResponse,
};
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::profiling::ProfilingMetadata;
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::Source;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use ordered_float::OrderedFloat;

/// Handle for injecting source changes in tests.
#[derive(Clone)]
pub struct MockSourceHandle {
    tx: mpsc::Sender<SourceChange>,
    source_id: String,
}

impl MockSourceHandle {
    pub async fn send(&self, change: SourceChange) -> Result<()> {
        self.tx
            .send(change)
            .await
            .map_err(|_| anyhow::anyhow!("failed sending mock source event: channel closed"))?;
        Ok(())
    }

    pub async fn send_node_insert(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: ElementPropertyMap,
    ) -> Result<()> {
        let element = Element::Node {
            metadata: self.make_metadata(element_id, labels),
            properties,
        };
        self.send(SourceChange::Insert { element }).await
    }

    pub async fn send_node_update(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: ElementPropertyMap,
    ) -> Result<()> {
        let element = Element::Node {
            metadata: self.make_metadata(element_id, labels),
            properties,
        };
        self.send(SourceChange::Update { element }).await
    }

    pub async fn send_node_delete(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
    ) -> Result<()> {
        let metadata = self.make_metadata(element_id, labels);
        self.send(SourceChange::Delete { metadata }).await
    }

    fn make_metadata(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
    ) -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference {
                source_id: Arc::from(self.source_id.as_str()),
                element_id: element_id.into(),
            },
            labels: Arc::from(
                labels
                    .into_iter()
                    .map(|label| label.into())
                    .collect::<Vec<_>>(),
            ),
            effective_from: chrono::Utc::now().timestamp_millis() as u64,
        }
    }
}

pub struct MockSource {
    base: SourceBase,
    app_rx: Arc<RwLock<Option<mpsc::Receiver<SourceChange>>>>,
}

impl MockSource {
    pub fn new(id: impl Into<String>) -> Result<(Self, MockSourceHandle)> {
        let source_id = id.into();
        let params = SourceBaseParams::new(source_id.clone());
        let (tx, rx) = mpsc::channel(1000);

        let handle = MockSourceHandle {
            tx,
            source_id: source_id.clone(),
        };

        let source = Self {
            base: SourceBase::new(params)?,
            app_rx: Arc::new(RwLock::new(Some(rx))),
        };

        Ok((source, handle))
    }

    async fn process_events(&self) -> Result<()> {
        let mut receiver = self
            .app_rx
            .write()
            .await
            .take()
            .ok_or_else(|| anyhow::anyhow!("mock source receiver already taken"))?;

        let source_name = self.base.id.clone();
        let dispatchers = self.base.dispatchers.clone();
        let status_handle = self.base.status_handle();

        let task_handle = tokio::spawn(async move {
            status_handle
                .set_status(ComponentStatus::Running, Some("Mock source running".into()))
                .await;

            while let Some(change) = receiver.recv().await {
                let mut profiling = ProfilingMetadata::new();
                profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                let event = SourceEventWrapper::with_profiling(
                    source_name.clone(),
                    SourceEvent::Change(change),
                    chrono::Utc::now(),
                    profiling,
                );

                let _ =
                    SourceBase::dispatch_from_task(dispatchers.clone(), event, &source_name).await;
            }
        });

        *self.base.task_handle.write().await = Some(task_handle);
        Ok(())
    }
}

#[async_trait]
impl Source for MockSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "mock"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }

    fn auto_start(&self) -> bool {
        true
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> Result<()> {
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting mock source".to_string()),
            )
            .await;
        self.process_events().await
    }

    async fn stop(&self) -> Result<()> {
        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping mock source".to_string()),
            )
            .await;

        if let Some(task_handle) = self.base.task_handle.write().await.take() {
            task_handle.abort();
        }

        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Mock source stopped".to_string()),
            )
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base.subscribe_with_bootstrap(&settings, "Mock").await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

/// Helper for building property maps in integration tests.
pub struct PropertyMapBuilder {
    properties: ElementPropertyMap,
}

impl PropertyMapBuilder {
    pub fn new() -> Self {
        Self {
            properties: ElementPropertyMap::new(),
        }
    }

    pub fn with_string(mut self, key: &str, value: &str) -> Self {
        self.properties
            .insert(key, ElementValue::String(Arc::from(value)));
        self
    }

    pub fn with_integer(mut self, key: &str, value: i64) -> Self {
        self.properties.insert(key, ElementValue::Integer(value));
        self
    }

    pub fn with_float(mut self, key: &str, value: f64) -> Self {
        self.properties
            .insert(key, ElementValue::Float(OrderedFloat(value)));
        self
    }

    pub fn with_bool(mut self, key: &str, value: bool) -> Self {
        self.properties.insert(key, ElementValue::Bool(value));
        self
    }

    pub fn build(self) -> ElementPropertyMap {
        self.properties
    }
}

impl Default for PropertyMapBuilder {
    fn default() -> Self {
        Self::new()
    }
}
