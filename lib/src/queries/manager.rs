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

use std::sync::Arc;

use crate::{ComponentId, ComponentName};
use anyhow::Result;

use crate::{
    channels::{ComponentEvent, ComponentStatus, ComponentType},
    computation::runtime::Runtime,
    config::{QueryConfig, QueryRuntime},
    observability::LogMessage,
    queries::{LabelExtractor, QueryLabels},
};

// Keep the plugin-facing trait import path stable.
pub use crate::queries::traits::Query;

/// A lightweight facade over graph-owned queries.
///
/// There is no separate query execution engine or registry behind this handle.
#[derive(Clone)]
pub struct QueryManager {
    runtime: Arc<Runtime>,
}

impl QueryManager {
    pub(crate) fn new(runtime: Arc<Runtime>) -> Self {
        Self { runtime }
    }

    pub async fn add_query(&self, config: QueryConfig) -> Result<()> {
        self.runtime.add_query(config, true).await?;
        Ok(())
    }

    pub async fn add_query_without_save(&self, config: QueryConfig) -> Result<()> {
        self.add_query(config).await
    }

    pub async fn provision_query(&self, config: QueryConfig) -> Result<()> {
        self.runtime.declare_query(config).await
    }

    pub async fn start_query_by_id(&self, id: &ComponentId) -> Result<()> {
        self.runtime.start_component(id, "query").await
    }

    pub async fn stop_query_by_id(&self, id: &ComponentId) -> Result<()> {
        self.runtime.stop_component(id, "query").await
    }

    pub async fn get_query_status_by_id(&self, id: &ComponentId) -> Result<ComponentStatus> {
        self.runtime.component_status(id, "query").await
    }

    pub async fn get_query_instance_by_id(
        &self,
        id: &ComponentId,
    ) -> crate::error::Result<Arc<dyn Query>> {
        Ok(self.runtime.query_instance(id).await?)
    }

    pub async fn get_query_by_id(&self, id: &ComponentId) -> Result<QueryRuntime> {
        self.runtime.query_info(id).await
    }

    pub async fn update_query_by_id(&self, id: &ComponentId, config: QueryConfig) -> Result<()> {
        let assigned = id.clone();
        anyhow::ensure!(
            config
                .id
                .as_ref()
                .is_none_or(|supplied| supplied == &assigned),
            "an update cannot change the component UUID"
        );
        self.runtime
            .update_query(id, config.with_assigned_id(assigned))
            .await
    }

    pub async fn teardown_query_by_id(&self, id: &ComponentId) -> Result<()> {
        self.runtime.remove_component(id, "query", true).await
    }

    pub async fn list_queries(
        &self,
    ) -> crate::error::Result<Vec<crate::config::ComponentListEntry>> {
        Ok(self.runtime.list_components("query").await?)
    }

    pub async fn get_query_config_by_id(
        &self,
        id: &ComponentId,
    ) -> crate::error::Result<crate::config::AssignedQueryConfig> {
        Ok(self.runtime.query_configuration(id).await?)
    }

    pub async fn get_all_query_labels(
        &self,
    ) -> crate::error::Result<Vec<(ComponentId, QueryLabels)>> {
        self.runtime
            .query_configurations()
            .await?
            .into_iter()
            .map(|config| {
                Ok((
                    config.id,
                    LabelExtractor::extract_labels(&config.query, &config.query_language)?,
                ))
            })
            .collect()
    }

    pub async fn get_query_results_by_id(
        &self,
        id: &ComponentId,
    ) -> Result<Vec<serde_json::Value>> {
        Ok(
            Query::fetch_snapshot(self.runtime.query_instance(id).await?.as_ref())
                .await?
                .to_vec(),
        )
    }

    pub async fn start_all(&self) -> Result<()> {
        self.runtime.start_kind("query").await
    }

    pub async fn stop_all(&self) -> Result<()> {
        self.runtime.stop_kind("query").await
    }

    pub async fn get_query_events_by_id(&self, id: &ComponentId) -> Result<Vec<ComponentEvent>> {
        Ok(self.runtime.component_events(id).await)
    }

    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        self.runtime.events(Some(ComponentType::Query)).await
    }

    pub async fn subscribe_logs_by_id(
        &self,
        id: &ComponentId,
    ) -> crate::error::Result<(
        Vec<LogMessage>,
        tokio::sync::broadcast::Receiver<LogMessage>,
    )> {
        Ok(self.runtime.subscribe_logs(id, "query").await?)
    }

    pub async fn subscribe_events_by_id(
        &self,
        id: &ComponentId,
    ) -> crate::error::Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        Ok(self.runtime.subscribe_events(id, "query").await?)
    }

    pub async fn start_query_by_name(&self, name: &ComponentName) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.start_query_by_id(&id).await
    }

    pub async fn stop_query_by_name(&self, name: &ComponentName) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.stop_query_by_id(&id).await
    }

    pub async fn get_query_status_by_name(&self, name: &ComponentName) -> Result<ComponentStatus> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.get_query_status_by_id(&id).await
    }

    pub async fn get_query_instance_by_name(
        &self,
        name: &ComponentName,
    ) -> crate::error::Result<Arc<dyn Query>> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.get_query_instance_by_id(&id).await
    }

    pub async fn get_query_by_name(&self, name: &ComponentName) -> Result<QueryRuntime> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.get_query_by_id(&id).await
    }

    pub async fn update_query_by_name(
        &self,
        name: &ComponentName,
        config: QueryConfig,
    ) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.update_query_by_id(&id, config).await
    }

    pub async fn teardown_query_by_name(&self, name: &ComponentName) -> Result<()> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.teardown_query_by_id(&id).await
    }

    pub async fn get_query_config_by_name(
        &self,
        name: &ComponentName,
    ) -> crate::error::Result<crate::config::AssignedQueryConfig> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.get_query_config_by_id(&id).await
    }

    pub async fn get_query_results_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<Vec<serde_json::Value>> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.get_query_results_by_id(&id).await
    }

    pub async fn get_query_events_by_name(
        &self,
        name: &ComponentName,
    ) -> Result<Vec<ComponentEvent>> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.get_query_events_by_id(&id).await
    }

    pub async fn subscribe_logs_by_name(
        &self,
        name: &ComponentName,
    ) -> crate::error::Result<(
        Vec<LogMessage>,
        tokio::sync::broadcast::Receiver<LogMessage>,
    )> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.subscribe_logs_by_id(&id).await
    }

    pub async fn subscribe_events_by_name(
        &self,
        name: &ComponentName,
    ) -> crate::error::Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        let id = self.runtime.component_id_by_name(name, Some("query"))?;
        self.subscribe_events_by_id(&id).await
    }
}

#[cfg(test)]
mod pipeline_characterization_tests;

#[cfg(test)]
#[path = "temporal_tests.rs"]
mod temporal_tests;
