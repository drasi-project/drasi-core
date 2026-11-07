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

use super::v1::{ComponentBatch, ComputationInfo};
use crate::{DrasiError, DrasiLib, Result};

/// Descriptive user configuration, excluding generated ordinary-query infrastructure.
/// The existing source/query/reaction representation is retained under `instance`.
/// Captured object properties do not imply reconstructibility. Native factory
/// definitions and resource recipes can be checked and exported separately with
/// [`super::v1::GraphConfigurationSnapshot::reconstructible_topology`].
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
    /// Resolve a current name without interpreting UUID-shaped names as IDs.
    pub fn component_by_name(&self, name: &str) -> Result<super::v1::ComponentHandle> {
        let name = super::v1::ComponentName::try_new(name).map_err(anyhow::Error::from)?;
        let handle = self
            .computation_control()?
            .component_handle_by_name(&name)
            .map_err(anyhow::Error::from)?;
        let current = self.computation_runtime.component_handle(handle.id())?;
        if current.generation() != handle.generation() {
            return Err(DrasiError::from(anyhow::Error::from(
                super::v1::GraphError::StaleGeneration,
            )));
        }
        self.computation_runtime
            .public_handle(current)
            .map_err(Into::into)
    }

    /// Rename graph-owned metadata through the configuration owner without changing identity.
    pub async fn rename_component(&self, id: &str, name: &str) -> Result<()> {
        let name = super::v1::ComponentName::try_new(name).map_err(anyhow::Error::from)?;
        let id = super::v1::ComponentId::try_new(id).map_err(anyhow::Error::from)?;
        self.computation_runtime
            .rename_owned(id, name)
            .await
            .map_err(Into::into)
    }

    /// Accept one native factory definition, then construct it with its assigned UUID.
    pub async fn create_computation_component(
        &self,
        definition: crate::management::ComponentDefinition,
    ) -> Result<super::v1::ComponentHandle> {
        self.state_guard.require_initialized()?;
        let owner = self
            .management
            .get()
            .ok_or_else(|| DrasiError::invalid_state("configuration owner is unavailable"))?;
        let receipt = owner
            .accept_auto(crate::management::AuthoringOperation::Definitions(
                crate::management::ConfigurationRequest::adding([
                    crate::management::NewComponentDefinition::Native(definition),
                ]),
            ))
            .await?;
        let id = receipt
            .creation
            .as_ref()
            .and_then(|creation| creation.components.first())
            .ok_or_else(|| DrasiError::invalid_state("accepted component has no assignment"))?
            .id
            .clone();
        owner.wait_declared(&id).await?;
        self.computation_component(id.as_str())
    }

    /// Accept factory declarations with scoped UUID allocation. The callback
    /// prepares metadata only; eager component objects are rejected.
    pub async fn create_components(
        &self,
        construct: impl FnOnce(&mut super::v1::ComponentIds) -> super::v1::GraphResult<ComponentBatch>
            + Send
            + 'static,
    ) -> Result<crate::management::AcceptanceReceipt> {
        self.state_guard.require_initialized()?;
        self.management
            .get()
            .ok_or_else(|| DrasiError::invalid_state("configuration owner is unavailable"))?
            .accept_auto(crate::management::AuthoringOperation::Scoped(Box::new(
                construct,
            )))
            .await
            .map_err(Into::into)
    }

    /// Snapshot ordinary and native component configuration without pausing
    /// processing. Concurrent reconfiguration is detected and retried; ordinary
    /// status updates do not invalidate the captured configuration. Unresolved
    /// configuration changes report an error rather than returning mixed state.
    pub async fn snapshot_computation_configuration(
        &self,
    ) -> Result<InstanceConfigurationSnapshot> {
        use super::v1::GraphSelection;
        use std::collections::BTreeSet;

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
                Err(error) if !root.same_configuration_as(&control.registry_snapshot()) => {
                    log::debug!(
                        "Retrying instance configuration after concurrent change: {error:#}"
                    );
                    continue;
                }
                Err(error) => return Err(DrasiError::from(error)),
            };
            let mut query_members = BTreeSet::new();
            for id in self.computation_runtime.ordinary_queries_at(&root)?.keys() {
                query_members
                    .extend(super::runtime::native_query::query_members(&root, id).into_keys());
            }
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
                .chain(query_members.iter().map(|id| id.as_str()))
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
                let mut native_resources: BTreeSet<_> = native_resources.into_iter().collect();
                snapshot
                    .topology
                    .include_resource_dependencies(&mut native_resources);
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
                            .insert(id.clone(), recipe.clone());
                    }
                    if let Some(dependencies) = root.desired.resource_dependencies.get(&id) {
                        snapshot
                            .topology
                            .resource_dependencies
                            .insert(id, dependencies.clone());
                    }
                }
                snapshot
                    .configurations
                    .retain(|id, _| native_ids.contains(id));
                Some(snapshot)
            };
            if !root.same_configuration_as(&control.registry_snapshot()) {
                continue;
            }
            return Ok(InstanceConfigurationSnapshot {
                version: 2,
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
        self.computation_runtime
            .public_control()
            .map_err(DrasiError::from)
    }

    /// Generic discovery and reading of the optional snapshot/replay providers
    /// that source, query and transformer outputs actually registered.
    pub fn output_catalogue(&self) -> Result<super::v1::OutputCatalogue> {
        Ok(super::v1::OutputCatalogue::new(self.computation_control()?))
    }

    pub fn computation_component(&self, id: &str) -> Result<super::v1::ComponentHandle> {
        let id = super::v1::ComponentId::try_new(id)
            .map_err(|error| DrasiError::invalid_config(error.to_string()).with_cause(error))?;
        self.state_guard.require_initialized()?;
        self.computation_runtime
            .component_handle(&id)
            .and_then(|handle| self.computation_runtime.public_handle(handle))
            .map_err(|error| {
                DrasiError::operation_failed("component", id.as_str(), "get", error.to_string())
                    .with_cause(error)
            })
    }

    /// Inspect the instance's single ComputationGraph and host-known shared
    /// dependencies from one coherent publication. Private plugin internals
    /// are not inferred.
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
            .query_inspector(id)
            .await
            .map_err(|error| {
                DrasiError::operation_failed("query", id, "inspect_computation", error.to_string())
                    .with_cause(error)
            })
    }

    /// Add an owned component or factory declaration using the same node-first
    /// controller used by ordinary source, query and reaction additions.
    pub async fn add_computation_component(
        &self,
        addition: super::v1::ComponentAddition,
    ) -> Result<super::v1::ComponentHandle> {
        let id = addition.definition.descriptor.id().clone();
        let mut definition = crate::management::DesiredInstance::default().topology;
        definition.components.push(addition.definition);
        definition.resources = addition.resources;
        self.add_components(ComponentBatch {
            definition,
            bindings: addition.bindings,
        })
        .await?;
        let owner = self
            .management
            .get()
            .ok_or_else(|| DrasiError::invalid_state("configuration owner is unavailable"))?;
        owner.wait_declared(&id).await?;
        self.computation_component(id.as_str())
    }

    pub(crate) async fn add_factory_component(
        &self,
        addition: super::v1::ComponentAddition,
        kind: super::v1::ComponentSemanticKind,
    ) -> Result<super::v1::ComponentHandle> {
        use super::v1::{ComponentConstruction, ComponentRole, ComponentSemanticKind};

        let definition = &addition.definition;
        let actual = definition
            .descriptor
            .semantic_kind()
            .unwrap_or_else(|| definition.role.into());
        let role_matches = matches!(
            (kind, definition.role),
            (
                ComponentSemanticKind::Source,
                ComponentRole::Source | ComponentRole::Service
            ) | (
                ComponentSemanticKind::Reaction,
                ComponentRole::Sink | ComponentRole::Service
            )
        );
        if actual != kind
            || !role_matches
            || !matches!(definition.construction, ComponentConstruction::Factory(_))
        {
            let error = DrasiError::invalid_config(format!(
                "expected a {kind:?} factory declaration with a compatible execution role"
            ));
            let mut definition = crate::management::DesiredInstance::default().topology;
            definition.components.push(addition.definition);
            definition.resources = addition.resources;
            let pending = self.instance_graph.retain(
                ComponentBatch {
                    definition,
                    bindings: addition.bindings,
                },
                self.computation_control().ok(),
            );
            return Err(super::instance::reject_components(pending, error).await);
        }
        self.add_computation_component(addition).await
    }

    pub async fn add_transformer_with_handle(
        &self,
        name: super::v1::ComponentName,
        transformer: impl super::v1::Transformer + 'static,
    ) -> Result<super::v1::ComponentHandle> {
        self.add_computation_component(super::v1::ComponentAddition::new(
            name,
            super::v1::ConstructedComponent::transformer(Box::new(transformer)),
        ))
        .await
    }

    pub async fn add_transformer(
        &self,
        name: super::v1::ComponentName,
        transformer: impl super::v1::Transformer + 'static,
    ) -> Result<()> {
        self.add_transformer_with_handle(name, transformer).await?;
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
                    .downcast_ref::<crate::error::ComponentNotFoundError>()
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
                    .downcast_ref::<crate::error::ComponentNotFoundError>()
                    .is_some()
                {
                    DrasiError::component_not_found("reaction", id)
                } else {
                    DrasiError::from(error)
                }
            })?;
        super::v1::ReactionPluginHost::borrowed(reaction, catalog)
            .map_err(|error| DrasiError::invalid_config(error.to_string()).with_cause(error))
    }
    /// Register the query plugin used by subsequent ordinary query declarations.
    /// The selected implementation cannot be replaced underneath existing queries.
    pub fn register_query_factory(
        &self,
        factory: std::sync::Arc<dyn super::v1::ComponentFactory>,
    ) -> Result<()> {
        if self.is_shutdown.load(std::sync::atomic::Ordering::Acquire) {
            return Err(DrasiError::invalid_state(
                "cannot register a query factory after shutdown",
            ));
        }
        self.computation_runtime
            .register_query_factory(factory)
            .map_err(|error| DrasiError::invalid_config(error.to_string()).with_cause(error))
    }

    pub fn query_factory_implementation(&self) -> Option<super::v1::ImplementationIdentity> {
        self.computation_runtime
            .query_factory()
            .map(|factory| factory.descriptor().implementation.clone())
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
        .map(|builder| match self.computation_runtime.query_factory() {
            Some(factory) => builder.query_factory(factory),
            None => builder,
        })
        .map_err(|error| DrasiError::invalid_config(error.to_string()).with_cause(error))
    }
    pub(crate) async fn start_parallel_components(&self) -> anyhow::Result<()> {
        let runtime = &self.computation_runtime;
        let mut failures = Vec::new();
        if let Err(error) = runtime.start_kind("source").await {
            failures.push(error.context("sources"));
        }
        if self.is_shutdown.load(std::sync::atomic::Ordering::Acquire) {
            failures.push(
                anyhow::Error::new(super::v1::GraphError::Cancelled)
                    .context("instance shutdown interrupted startup"),
            );
            return Err(crate::error::OperationFailures::new(
                "instance startup had failures",
                failures,
            )
            .into());
        }
        if let Err(error) = runtime.start_kind("query").await {
            failures.push(error.context("queries"));
            return Err(crate::error::OperationFailures::new(
                "instance startup had failures",
                failures,
            )
            .into());
        }
        if let Err(error) = runtime
            .start_native_components(self.management.get().is_some())
            .await
        {
            failures.push(error.context("native components"));
        }
        if self.is_shutdown.load(std::sync::atomic::Ordering::Acquire) {
            failures.push(
                anyhow::Error::new(super::v1::GraphError::Cancelled)
                    .context("instance shutdown interrupted startup"),
            );
            return Err(crate::error::OperationFailures::new(
                "instance startup had failures",
                failures,
            )
            .into());
        }
        if let Err(error) = runtime.subscriptions_complete().await {
            failures.push(error.context("completing subscriptions"));
            return Err(crate::error::OperationFailures::new(
                "instance startup had failures",
                failures,
            )
            .into());
        }
        if let Err(error) = runtime.start_kind("reaction").await {
            failures.push(error.context("reactions"));
        }
        if !failures.is_empty() {
            return Err(crate::error::OperationFailures::new(
                "instance startup had failures",
                failures,
            )
            .into());
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
    ///
    /// Pass a native [`super::v1::ComponentRole`] or an explicit
    /// [`crate::observability::ComponentLogType`]. Older `ComponentType` arguments
    /// remain accepted, including Query for transformers/services and Reaction
    /// for sinks, but returned messages report the actual native role.
    /// A category unrelated to the component is rejected.
    pub async fn subscribe_computation_logs(
        &self,
        component_type: impl Into<crate::observability::ComponentLogType>,
        component_id: &str,
    ) -> Result<(
        Vec<crate::observability::LogMessage>,
        tokio::sync::broadcast::Receiver<crate::observability::LogMessage>,
    )> {
        self.computation_component(component_id)?;
        super::v1::ComponentId::try_new(component_id)
            .map_err(|error| DrasiError::invalid_config(error.to_string()).with_cause(error))?;
        let publication = self.computation_control()?.registry_snapshot();
        let key = self
            .computation_runtime
            .component_log_key(&publication, component_id)?;
        let requested = component_type.into();
        if requested != key.component_type && requested != key.component_type.legacy_category() {
            return Err(DrasiError::invalid_config(format!(
                "component '{component_id}' logs use '{}', not '{}'",
                key.component_type.as_str(),
                requested.as_str()
            )));
        }
        Ok(self.log_registry.subscribe_by_key(&key).await)
    }
    pub async fn computation_info(&self) -> Result<ComputationInfo> {
        self.state_guard.require_initialized()?;
        Ok(self.instance_graph.get().await?.info())
    }

    /// Add a batch to the instance graph, retaining failed declarations and
    /// exposing construction and activation outcomes in the returned report.
    pub async fn add_components(
        &self,
        batch: ComponentBatch,
    ) -> Result<crate::management::AcceptanceReceipt> {
        self.state_guard.require_initialized()?;
        self.management
            .get()
            .ok_or_else(|| DrasiError::invalid_state("configuration owner is unavailable"))?
            .accept_auto(crate::management::AuthoringOperation::Provided(batch))
            .await
            .map_err(Into::into)
    }
}
