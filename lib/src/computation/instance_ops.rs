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

use super::v1::{
    ComputationGraph, ComputationHandle, ComputationInfo, ComputationOptions, StartReport,
    StopReport,
};
use crate::{DrasiError, DrasiLib, Result};

impl DrasiLib {
    pub async fn borrow_computation_source(
        &self,
        id: &str,
    ) -> Result<std::sync::Arc<super::v1::SourcePluginHost>> {
        self.state_guard.require_initialized()?;
        if let Some(runtime) = &self.computation_runtime {
            return Ok(super::v1::SourcePluginHost::borrowed(
                runtime.source(id).await?,
            ));
        }
        let source = self
            .source_manager
            .get_source_instance(id)
            .await
            .ok_or_else(|| DrasiError::component_not_found("source", id))?;
        Ok(super::v1::SourcePluginHost::borrowed(source))
    }
    pub async fn borrow_computation_reaction(
        &self,
        id: &str,
        catalog: super::v1::QueryResultsCatalog,
    ) -> Result<std::sync::Arc<super::v1::ReactionPluginHost>> {
        self.state_guard.require_initialized()?;
        let reaction = self
            .component_graph
            .read()
            .await
            .get_runtime::<std::sync::Arc<dyn crate::Reaction>>(id)
            .cloned()
            .ok_or_else(|| DrasiError::component_not_found("reaction", id))?;
        super::v1::ReactionPluginHost::borrowed(reaction, catalog)
            .map_err(|error| DrasiError::invalid_config(error.to_string()))
    }
    pub fn computation_pipeline(
        &self,
        graph_id: &str,
    ) -> Result<super::v1::ComputationPipelineBuilder> {
        let services = self.computation_plugin_services(graph_id)?;
        super::v1::ComputationPipelineBuilder::new(
            graph_id,
            services,
            self.middleware_registry.clone(),
            self.config.index_factory.clone(),
            self.config.default_recovery_policy.unwrap_or_default(),
            self.config.global_priority_queue_capacity.unwrap_or(10_000),
            self.config.global_dispatch_buffer_capacity.unwrap_or(1_000),
        )
        .map_err(|error| DrasiError::invalid_config(error.to_string()))
    }
    pub(crate) async fn start_parallel_components(&self) -> anyhow::Result<()> {
        if let Some(runtime) = &self.computation_runtime {
            let mut failures = Vec::new();
            if let Err(error) = runtime.start_kind("source").await {
                failures.push(format!("native sources: {error:#}"));
            }
            // A failed start can leave independent components running. Keep the
            // ordinary stop path available for that partially active instance.
            *self.running.write().await = true;
            runtime.start_kind("query").await?;
            if let Err(error) = self.computation_registry.start_auto().await {
                failures.push(format!("additional native graphs: {error:#}"));
            }
            runtime.subscriptions_complete().await?;
            if let Err(error) = runtime.start_kind("reaction").await {
                failures.push(format!("native reactions: {error:#}"));
            }
            if !failures.is_empty() {
                anyhow::bail!(
                    "native startup completed with failures: {}",
                    failures.join("; ")
                );
            }
            return Ok(());
        }
        if self.computation_registry.is_empty()? {
            return self.lifecycle.start_components().await;
        }
        let mut failures = Vec::new();
        if let Err(error) = self.source_manager.start_all().await {
            failures.push(format!("legacy sources: {error:#}"));
        }
        *self.running.write().await = true;
        if self.is_shutdown.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("instance shutdown interrupted startup");
        }
        // Native subscriptions must register their position handles before the
        // legacy instance releases the common source subscription fence.
        if let Err(error) = self.computation_registry.start_auto().await {
            failures.push(format!("{error:#}"));
        }
        if self.is_shutdown.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("instance shutdown interrupted startup");
        }
        if let Err(error) = self.query_manager.start_all().await {
            log::warn!("Some legacy queries failed to start (retaining legacy best-effort behavior): {error}");
        }
        self.source_manager.subscriptions_complete().await;
        if let Err(error) = self.reaction_manager.start_all().await {
            failures.push(format!("legacy reactions: {error:#}"));
        }
        if !failures.is_empty() {
            anyhow::bail!("instance startup had failures: {}", failures.join("; "));
        }
        Ok(())
    }
    /// Borrow instance services through graph-specific state and WAL namespaces.
    pub fn computation_plugin_services(
        &self,
        graph_id: &str,
    ) -> Result<super::v1::LegacyPluginServices> {
        self.state_guard.require_initialized()?;
        let mut services = self
            .computation_registry
            .services
            .lock()
            .map_err(|_| DrasiError::invalid_state("computation services binding poisoned"))?;
        if let Some(services) = services.get(graph_id) {
            return Ok(services.clone());
        }
        let wal = self
            .computation_registry
            .wal
            .lock()
            .map_err(|_| DrasiError::invalid_state("computation WAL binding poisoned"))?
            .clone();
        let scoped = super::v1::LegacyPluginServices::scoped(
            &self.config.id,
            graph_id,
            Some(self.config.state_store_provider.clone()),
            self.config.identity_provider.clone(),
            wal,
        )
        .map_err(|error| DrasiError::invalid_config(error.to_string()))?
        .with_secret_store(self.config.secret_store_provider.clone());
        services.insert(graph_id.to_owned(), scoped.clone());
        Ok(scoped)
    }
    pub async fn inspect_computation_graph(
        &self,
        id: &str,
    ) -> Result<super::v1::ComputationInspector> {
        Ok(self.get_computation_graph(id).await?.inspector())
    }
    /// Read and subscribe to the existing log registry under this graph's
    /// execution scope. Use the plugin's own ID for logs from wrapped plugins.
    /// Native transformers use Query and sinks use Reaction as log categories.
    pub async fn subscribe_computation_logs(
        &self,
        graph_id: &str,
        component_type: crate::channels::ComponentType,
        component_id: &str,
    ) -> Result<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        self.get_computation_graph(graph_id).await?;
        super::v1::ComponentId::try_new(component_id)
            .map_err(|error| DrasiError::invalid_config(error.to_string()))?;
        let key = crate::managers::ComponentLogKey::new(
            format!("{}::computation::{graph_id}", self.config.id),
            component_type,
            component_id,
        );
        Ok(self.log_registry.subscribe_by_key(&key).await)
    }
    /// Register a parallel graph without adding its nodes to ComponentGraph.
    /// The instance owns its driver; failures stay inspectable on the handle.
    pub async fn add_computation_graph(
        &self,
        graph: ComputationGraph,
        options: ComputationOptions,
    ) -> Result<ComputationHandle> {
        let _lifecycle = self.computation_registry.lifecycle.lock().await;
        if let Err(error) = self.state_guard.require_initialized() {
            return Err(super::instance::reject_graph(graph, options, error).await);
        }
        if self.is_shutdown.load(std::sync::atomic::Ordering::Acquire) {
            return Err(super::instance::reject_graph(
                graph,
                options,
                DrasiError::invalid_state("instance has been shut down"),
            )
            .await);
        }
        let handle = self.computation_registry.add(graph, options).await?;
        if options.auto_start && self.is_running().await {
            handle.start().await?;
        }
        Ok(handle)
    }

    pub async fn get_computation_graph(&self, id: &str) -> Result<ComputationHandle> {
        self.state_guard.require_initialized()?;
        self.computation_registry.get(id).await
    }

    pub async fn list_computation_graphs(&self) -> Result<Vec<ComputationInfo>> {
        self.state_guard.require_initialized()?;
        Ok(self
            .computation_registry
            .list()
            .await?
            .iter()
            .map(ComputationHandle::info)
            .collect())
    }

    pub async fn start_computation_graph(&self, id: &str) -> Result<StartReport> {
        self.get_computation_graph(id).await?.start().await
    }

    pub async fn stop_computation_graph(&self, id: &str) -> Result<StopReport> {
        self.get_computation_graph(id).await?.stop().await
    }

    pub async fn remove_computation_graph(&self, id: &str) -> Result<()> {
        self.state_guard.require_initialized()?;
        self.computation_registry.remove(id).await
    }
}
