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

use anyhow::Result;
use async_trait::async_trait;

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

    pub async fn start_query(&self, id: String) -> Result<()> {
        self.runtime.start_component(&id, "query").await
    }

    pub async fn stop_query(&self, id: String) -> Result<()> {
        self.runtime.stop_component(&id, "query").await
    }

    pub async fn get_query_status(&self, id: String) -> Result<ComponentStatus> {
        self.runtime.component_status(&id, "query").await
    }

    pub async fn get_query_instance(&self, id: &str) -> crate::error::Result<Arc<dyn Query>> {
        Ok(self.runtime.query_instance(id).await?)
    }

    pub async fn get_query(&self, id: String) -> Result<QueryRuntime> {
        self.runtime.query_info(&id).await
    }

    pub async fn update_query(&self, id: String, config: QueryConfig) -> Result<()> {
        self.runtime.update_query(&id, config).await
    }

    pub async fn teardown_query(&self, id: String) -> Result<()> {
        self.runtime.remove_component(&id, "query", true).await
    }

    pub async fn list_queries(&self) -> crate::error::Result<Vec<(String, ComponentStatus)>> {
        Ok(self.runtime.list_components("query").await?)
    }

    pub async fn get_query_config(&self, id: &str) -> crate::error::Result<QueryConfig> {
        Ok(self.runtime.query_configuration(id).await?)
    }

    pub async fn get_all_query_labels(&self) -> crate::error::Result<Vec<(String, QueryLabels)>> {
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

    pub async fn get_query_results(&self, id: &str) -> Result<Vec<serde_json::Value>> {
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

    pub async fn get_query_events(&self, id: &str) -> Vec<ComponentEvent> {
        self.runtime.component_events(id).await
    }

    pub async fn get_all_events(&self) -> Vec<ComponentEvent> {
        self.runtime.events(Some(ComponentType::Query)).await
    }

    pub async fn subscribe_logs(
        &self,
        id: &str,
    ) -> crate::error::Result<(
        Vec<LogMessage>,
        tokio::sync::broadcast::Receiver<LogMessage>,
    )> {
        Ok(self.runtime.subscribe_logs(id, "query").await?)
    }

    pub async fn subscribe_events(
        &self,
        id: &str,
    ) -> crate::error::Result<(
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    )> {
        Ok(self.runtime.subscribe_events(id, "query").await?)
    }
}

#[async_trait]
impl crate::reactions::QueryProvider for QueryManager {
    async fn get_query_instance(&self, id: &str) -> Result<Arc<dyn Query>> {
        Ok(self.runtime.query_instance(id).await?)
    }
}

#[cfg(test)]
mod pipeline_characterization_tests;

#[cfg(test)]
#[path = "temporal_tests.rs"]
mod temporal_tests;
