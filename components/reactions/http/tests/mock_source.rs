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

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
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

#[derive(Clone)]
pub struct MockSourceHandle {
    tx: mpsc::Sender<SourceChange>,
    source_id: String,
}

impl MockSourceHandle {
    pub async fn send_node_insert(
        &self,
        element_id: impl Into<Arc<str>>,
        labels: Vec<impl Into<Arc<str>>>,
        properties: ElementPropertyMap,
    ) -> Result<()> {
        let effective_from = chrono::Utc::now().timestamp_millis() as u64;
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference {
                    source_id: Arc::from(self.source_id.as_str()),
                    element_id: element_id.into(),
                },
                labels: Arc::from(labels.into_iter().map(|l| l.into()).collect::<Vec<_>>()),
                effective_from,
            },
            properties,
        };

        self.tx
            .send(SourceChange::Insert { element })
            .await
            .map_err(|_| anyhow::anyhow!("failed to send mock source event"))
    }
}

pub struct MockSource {
    base: SourceBase,
    app_rx: Arc<RwLock<Option<mpsc::Receiver<SourceChange>>>>,
}

impl MockSource {
    pub fn new(id: impl Into<String>) -> Result<(Self, MockSourceHandle)> {
        let id = id.into();
        let (app_tx, app_rx) = mpsc::channel(1000);
        let handle = MockSourceHandle {
            tx: app_tx,
            source_id: id.clone(),
        };
        let source = Self {
            base: SourceBase::new(SourceBaseParams::new(id))?,
            app_rx: Arc::new(RwLock::new(Some(app_rx))),
        };
        Ok((source, handle))
    }

    async fn process_events(&self) -> Result<()> {
        let mut rx = self
            .app_rx
            .write()
            .await
            .take()
            .ok_or_else(|| anyhow::anyhow!("mock source receiver already taken"))?;

        let source_name = self.base.id.clone();
        let base_dispatchers = self.base.dispatchers.clone();
        let status_handle = self.base.status_handle();

        let handle = tokio::spawn(async move {
            status_handle
                .set_status(ComponentStatus::Running, Some("Mock source running".into()))
                .await;

            while let Some(change) = rx.recv().await {
                let mut profiling = ProfilingMetadata::new();
                profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                let wrapper = SourceEventWrapper::with_profiling(
                    source_name.clone(),
                    SourceEvent::Change(change),
                    chrono::Utc::now(),
                    profiling,
                );

                let _ =
                    SourceBase::dispatch_from_task(base_dispatchers.clone(), wrapper, &source_name)
                        .await;
            }
        });

        *self.base.task_handle.write().await = Some(handle);
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
                Some("Starting mock source".into()),
            )
            .await;
        self.process_events().await
    }

    async fn stop(&self) -> Result<()> {
        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping mock source".into()),
            )
            .await;

        if let Some(handle) = self.base.task_handle.write().await.take() {
            handle.abort();
        }

        self.base
            .set_status(ComponentStatus::Stopped, Some("Mock source stopped".into()))
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

pub struct PropertyMapBuilder {
    properties: ElementPropertyMap,
}

impl Default for PropertyMapBuilder {
    fn default() -> Self {
        Self::new()
    }
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

    pub fn build(self) -> ElementPropertyMap {
        self.properties
    }
}
