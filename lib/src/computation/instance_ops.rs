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

use super::v1::{ComponentBatch, ComputationInfo, GraphSelection, ReconciliationReport};
use crate::{DrasiError, DrasiLib, Result};

/// Complete user configuration, excluding generated query-internal graphs.
/// The existing source/query/reaction representation is retained under `instance`.
/// Configurations may contain secrets and must be stored as privileged data.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstanceConfigurationSnapshot {
    pub version: u32,
    pub instance: crate::ConfigurationSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_components: Option<super::v1::GraphConfigurationSnapshot>,
}

impl DrasiLib {
    /// Snapshot ordinary and native component configuration without pausing
    /// processing. Concurrent reconfiguration is detected and retried; a busy
    /// instance reports an error rather than returning a mixed configuration.
    pub async fn snapshot_computation_configuration(
        &self,
    ) -> Result<InstanceConfigurationSnapshot> {
        use super::v1::GraphSelection;
        use std::{collections::BTreeSet, sync::Arc};

        self.state_guard.require_initialized()?;
        let control = self.computation_control()?;
        for _ in 0..3 {
            let root = control.registry_snapshot();
            let instance = match self
                .computation_runtime
                .configuration_snapshot_at(&root)
                .await
            {
                Ok(instance) => instance,
                Err(error) if !Arc::ptr_eq(&root, &control.registry_snapshot()) => {
                    log::debug!(
                        "Retrying instance configuration after concurrent change: {error:#}"
                    );
                    continue;
                }
                Err(error) => return Err(DrasiError::from(error)),
            };
            let ordinary: BTreeSet<_> = instance
                .sources
                .iter()
                .map(|source| source.id.as_str())
                .chain(instance.queries.iter().map(|query| query.id.as_str()))
                .chain(
                    instance
                        .reactions
                        .iter()
                        .map(|reaction| reaction.id.as_str()),
                )
                .collect();
            let native_ids: BTreeSet<_> = root
                .desired
                .nodes
                .iter()
                .map(|node| node.descriptor.id())
                .filter(|id| !ordinary.contains(id.as_str()))
                .cloned()
                .collect();
            let infrastructure = self
                .computation_runtime
                .infrastructure_resources(&root, &ordinary)?;
            let native_resources: Vec<_> = root
                .desired
                .resources
                .keys()
                .filter(|id| !infrastructure.contains(*id))
                .cloned()
                .collect();
            let native_components = if native_ids.is_empty() && native_resources.is_empty() {
                None
            } else {
                let mut snapshot = root.configuration_snapshot().map_err(anyhow::Error::from)?;
                snapshot.topology = root
                    .desired
                    .select(GraphSelection::Exact(native_ids.iter().cloned().collect()))
                    .map_err(anyhow::Error::from)?;
                for id in native_resources {
                    if !snapshot
                        .topology
                        .resources
                        .iter()
                        .any(|resource| resource.id == id)
                    {
                        snapshot
                            .topology
                            .resources
                            .push(root.desired.resources[&id].clone());
                    }
                    if let Some(recipe) = root.desired.resource_configurations.get(&id) {
                        snapshot
                            .topology
                            .resource_configurations
                            .insert(id, recipe.clone());
                    }
                }
                snapshot
                    .configurations
                    .retain(|id, _| native_ids.contains(id));
                Some(snapshot)
            };
            if !Arc::ptr_eq(&root, &control.registry_snapshot()) {
                continue;
            }
            return Ok(InstanceConfigurationSnapshot {
                version: 1,
                instance,
                native_components,
            });
        }
        Err(DrasiError::invalid_state(
            "instance configuration changed during snapshot; retry after reconfiguration completes",
        ))
    }

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

    /// Inspect the ComputationGraph and its QueryGraphs. Scope ownership and
    /// host-known shared dependencies are explicit; private plugin internals
    /// are not inferred. Individual scopes are coherent, not globally atomic.
    pub async fn inspect_computation_inventory(&self) -> Result<super::v1::ComputationInventory> {
        self.state_guard.require_initialized()?;
        self.computation_runtime
            .inventory()
            .await
            .map_err(DrasiError::from)
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
        let source = self
            .computation_runtime
            .source_host(id)
            .await
            .map_err(|error| {
                if error
                    .downcast_ref::<crate::managers::ComponentNotFoundError>()
                    .is_some()
                {
                    DrasiError::component_not_found("source", id)
                } else {
                    DrasiError::from(error)
                }
            })?;
        Ok(source)
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
    pub fn computation_pipeline(&self) -> Result<super::v1::ComputationPipelineBuilder> {
        let services = self.computation_plugin_services()?;
        super::v1::ComputationPipelineBuilder::new(
            super::components::INSTANCE_GRAPH_ID,
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
    /// Borrow the instance's injected services.
    pub fn computation_plugin_services(&self) -> Result<super::v1::LegacyPluginServices> {
        self.state_guard.require_initialized()?;
        Ok(self.computation_runtime.plugin_services())
    }
    pub fn inspect_computation_graph(&self) -> Result<super::v1::ComputationInspector> {
        Ok(self.computation_control()?.inspector())
    }
    /// Read and subscribe to the instance's component log registry.
    /// Native transformers use Query and sinks use Reaction as log categories.
    pub async fn subscribe_computation_logs(
        &self,
        component_type: crate::channels::ComponentType,
        component_id: &str,
    ) -> Result<(
        Vec<crate::managers::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::managers::LogMessage>,
    )> {
        self.computation_component(component_id)?;
        super::v1::ComponentId::try_new(component_id)
            .map_err(|error| DrasiError::invalid_config(error.to_string()))?;
        let key = crate::managers::ComponentLogKey::new(
            self.config.id.clone(),
            component_type,
            component_id,
        );
        Ok(self.log_registry.subscribe_by_key(&key).await)
    }
    pub async fn computation_info(&self) -> Result<ComputationInfo> {
        self.state_guard.require_initialized()?;
        Ok(self.instance_graph.get().await?.info())
    }

    /// Add a batch to the instance graph, retaining failed declarations and
    /// exposing construction and activation outcomes in the returned report.
    pub async fn add_components(&self, batch: ComponentBatch) -> Result<ReconciliationReport> {
        let control = self.computation_control();
        let pending = self
            .instance_graph
            .retain(batch, control.as_ref().ok().cloned());
        let result: Result<ReconciliationReport> = async {
            let control = control?;
            control
                .require_configuration_write()
                .map_err(anyhow::Error::from)?;
            let _lifecycle = self.instance_graph.lifecycle.lock().await;
            if self.is_shutdown.load(std::sync::atomic::Ordering::Acquire) {
                return Err(DrasiError::invalid_state("instance has been shut down"));
            }
            let mut batch = pending.take().await?;
            let current = control.registry_snapshot();
            let mut ids: std::collections::BTreeSet<_> = current
                .desired
                .nodes
                .iter()
                .map(|node| node.descriptor.id().clone())
                .collect();
            for component in &batch.definition.components {
                if !ids.insert(component.descriptor.id().clone()) {
                    return Err(DrasiError::already_exists(
                        "component",
                        component.descriptor.id().to_string(),
                    ));
                }
            }
            if batch.definition.graph_id != current.desired.id.as_ref() {
                return Err(DrasiError::invalid_config(
                    "component definition does not belong to the instance ComputationGraph",
                ));
            }
            batch.bindings.defer_activation |= !self.is_running().await;
            batch.bindings.deferred_management_validation = true;
            control
                .add_components(batch)
                .await
                .map_err(|error| DrasiError::from(anyhow::Error::from(error)))
        }
        .await;
        match result {
            Ok(report) => {
                if let Err(error) = pending.cleanup().await {
                    let detail = error.to_string();
                    return Err(super::instance::components_cleanup_error(
                        pending, error, detail,
                    ));
                }
                Ok(report)
            }
            Err(error) => Err(super::instance::reject_components(pending, error).await),
        }
    }
}
