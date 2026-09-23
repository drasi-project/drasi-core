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
    /// Access the instance controller for explicit port connections and inspection.
    pub fn computation_control(&self) -> Result<super::v1::GraphControl> {
        self.state_guard.require_initialized()?;
        self.computation_runtime.control().map_err(DrasiError::from)
    }

    pub fn computation_component(&self, id: &str) -> Result<super::v1::ComponentHandle> {
        let id = super::v1::ComponentId::try_new(id)
            .map_err(|error| DrasiError::invalid_config(error.to_string()))?;
        self.computation_control()?
            .component_handle(&id)
            .map_err(|error| {
                DrasiError::operation_failed("component", id.as_str(), "get", error.to_string())
            })
    }

    /// Inspect every host-visible native scope, including ordinary query
    /// execution graphs and independently registered graphs. Scope ownership and
    /// host-known shared dependencies are explicit; private plugin internals
    /// are not inferred. Individual scopes are coherent, not globally atomic.
    pub async fn inspect_computation_inventory(&self) -> Result<super::v1::ComputationInventory> {
        self.state_guard.require_initialized()?;
        let mut inventory = self.computation_runtime.inventory().await?;
        for graph in self.computation_registry.list().await? {
            let scope = super::v1::ComputationScope::root(graph.id());
            if !inventory.scopes.contains_key(&scope) {
                inventory
                    .insert(scope, None, graph.inspector().topology())
                    .map_err(anyhow::Error::from)?;
            }
        }
        Ok(inventory)
    }

    /// Inspect a query's internal native graph, including its concrete index,
    /// bootstrap and pipe resources. An unrealized query exposes its parent
    /// declaration so its construction failure remains inspectable.
    pub async fn inspect_query_computation(
        &self,
        id: &str,
    ) -> Result<super::v1::ComputationInspector> {
        self.state_guard.require_initialized()?;
        self.computation_runtime
            .query(id)
            .await
            .map(|query| query.inspector())
            .map_err(|error| {
                DrasiError::operation_failed("query", id, "inspect_computation", error.to_string())
            })
    }

    /// Add a preconstructed native component using the same node-first
    /// controller used by ordinary source, query and reaction additions.
    pub async fn add_computation_component(
        &self,
        mut addition: super::v1::ComponentAddition,
    ) -> Result<super::v1::ComponentHandle> {
        let control = self.computation_control()?;
        let id = addition.definition.descriptor.id().clone();
        if !self.is_running().await {
            addition.bindings.defer_activation = true;
        }
        control
            .add_component(addition)
            .await
            .map_err(|error| match error {
                rejected @ super::v1::GraphError::AdditionRejected { .. } => {
                    DrasiError::from(anyhow::Error::new(rejected))
                }
                error => {
                    DrasiError::operation_failed("component", id.as_str(), "add", error.to_string())
                }
            })
    }

    pub async fn add_transformer_with_handle(
        &self,
        transformer: impl super::v1::Transformer + 'static,
    ) -> Result<super::v1::ComponentHandle> {
        self.add_computation_component(super::v1::ComponentAddition::new(
            super::v1::ConstructedComponent::transformer(Box::new(transformer)),
        ))
        .await
    }

    pub async fn add_transformer(
        &self,
        transformer: impl super::v1::Transformer + 'static,
    ) -> Result<()> {
        self.add_transformer_with_handle(transformer).await?;
        Ok(())
    }

    pub async fn borrow_computation_source(
        &self,
        id: &str,
    ) -> Result<std::sync::Arc<super::v1::SourcePluginHost>> {
        self.state_guard.require_initialized()?;
        let source = self.source_instance(id).await.map_err(|error| {
            if error
                .downcast_ref::<crate::managers::ComponentNotFoundError>()
                .is_some()
            {
                DrasiError::component_not_found("source", id)
            } else {
                DrasiError::from(error)
            }
        })?;
        Ok(super::v1::SourcePluginHost::borrowed(source))
    }
    pub async fn borrow_computation_reaction(
        &self,
        id: &str,
        catalog: super::v1::QueryResultsCatalog,
    ) -> Result<std::sync::Arc<super::v1::ReactionPluginHost>> {
        self.state_guard.require_initialized()?;
        let reaction = self
            .computation_runtime
            .reaction(id)
            .await
            .map_err(|error| {
                if error
                    .downcast_ref::<crate::managers::ComponentNotFoundError>()
                    .is_some()
                {
                    DrasiError::component_not_found("reaction", id)
                } else {
                    DrasiError::from(error)
                }
            })?;
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
        let runtime = &self.computation_runtime;
        let mut failures = Vec::new();
        if let Err(error) = runtime.start_kind("source").await {
            failures.push(format!("sources: {error:#}"));
        }
        // Keep stop available if independent components start before a failure.
        *self.running.write().await = true;
        if self.is_shutdown.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("instance shutdown interrupted startup");
        }
        runtime.start_kind("query").await?;
        if let Err(error) = runtime.start_native_components().await {
            failures.push(format!("native components: {error:#}"));
        }
        if let Err(error) = self.computation_registry.start_auto().await {
            failures.push(format!("additional graphs: {error:#}"));
        }
        if self.is_shutdown.load(std::sync::atomic::Ordering::Acquire) {
            anyhow::bail!("instance shutdown interrupted startup");
        }
        runtime.subscriptions_complete().await?;
        if let Err(error) = runtime.start_kind("reaction").await {
            failures.push(format!("reactions: {error:#}"));
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
    /// Register an additional graph under instance-owned execution.
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
            if let Err(error) = handle.control().request_auto_start().await {
                log::error!(
                    "Computation {} was added but its driver cannot accept activation: {error}",
                    handle.id()
                );
            }
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
