// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::*;

pub(super) type MetadataChanges = Arc<std::sync::Mutex<BTreeSet<ComponentId>>>;

/// A coherent current publication for in-process hosts. Weak bindings avoid a
/// cycle through components that themselves hold their graph's control handle.
/// Public inspection/history continues to contain no runtime handles.
#[derive(Clone)]
pub(crate) struct GraphRegistrySnapshot {
    pub desired: Arc<GraphSnapshot>,
    pub observed: Arc<ObservedGraph>,
    resources: im::OrdMap<ResourceId, specification::WeakResourceHandle>,
    configurations: im::OrdMap<ComponentId, CapturedComponentConfiguration>,
    query_apis: im::OrdMap<ComponentId, std::sync::Weak<crate::computation::v1::QueryApi>>,
    source_schema_readers:
        im::OrdMap<ComponentId, std::sync::Weak<dyn crate::computation::v1::SourceSchemaReader>>,
    output_readers: im::OrdMap<ComponentId, crate::computation::v1::WeakOutputReaders>,
    primary_ports: im::OrdMap<ComponentId, crate::computation::v1::PortPrimaries>,
    deferred_activation: im::OrdSet<ComponentId>,
}

impl GraphRegistrySnapshot {
    /// Status publications do not invalidate an otherwise coherent configuration.
    pub(crate) fn same_configuration_as(&self, other: &Self) -> bool {
        self.desired.id == other.desired.id
            && self.desired.revision == other.desired.revision
            && self.observed.run_epoch == other.observed.run_epoch
            && self.configurations == other.configurations
            && self.primary_ports == other.primary_ports
            && self.observed.components.len() == other.observed.components.len()
            && self.observed.components.iter().all(|(id, component)| {
                other
                    .observed
                    .components
                    .get(id)
                    .is_some_and(|next| component.generation == next.generation)
            })
            && self.observed.resources.len() == other.observed.resources.len()
            && self.observed.resources.iter().all(|(id, resource)| {
                other
                    .observed
                    .resources
                    .get(id)
                    .is_some_and(|next| resource.generation == next.generation)
            })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        desired: Arc<GraphSnapshot>,
        observed: Arc<ObservedGraph>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
        configurations: im::OrdMap<ComponentId, CapturedComponentConfiguration>,
        query_apis: im::OrdMap<ComponentId, std::sync::Weak<crate::computation::v1::QueryApi>>,
        source_schema_readers: im::OrdMap<
            ComponentId,
            std::sync::Weak<dyn crate::computation::v1::SourceSchemaReader>,
        >,
        output_readers: im::OrdMap<ComponentId, crate::computation::v1::WeakOutputReaders>,
        factories: &BTreeMap<ImplementationIdentity, Arc<dyn ComponentFactory>>,
        deferred_activation: im::OrdSet<ComponentId>,
    ) -> Self {
        Self {
            desired: desired.clone(),
            observed,
            configurations,
            query_apis,
            source_schema_readers,
            output_readers,
            primary_ports: desired
                .specifications
                .iter()
                .filter_map(|(id, specification)| {
                    factories
                        .get(&specification.implementation)
                        .map(|factory| (id.clone(), factory.primary_ports()))
                })
                .collect(),
            deferred_activation,
            resources: resources
                .iter()
                .map(|(id, handle)| (id.clone(), handle.downgrade()))
                .collect(),
        }
    }

    pub(crate) fn configuration_snapshot(&self) -> GraphResult<GraphConfigurationSnapshot> {
        Ok(GraphConfigurationSnapshot {
            topology: self.desired.select(super::super::GraphSelection::All)?,
            configurations: self
                .configurations
                .iter()
                .map(|(id, value)| (id.clone(), value.clone()))
                .collect(),
        })
    }

    pub(crate) fn resource(&self, id: &ResourceId) -> GraphResult<ResourceHandle> {
        self.resources
            .get(id)
            .ok_or_else(|| {
                topology(format!(
                    "resource {id} is not bound in this graph publication"
                ))
            })?
            .upgrade()
            .ok_or(GraphError::StaleGeneration)
    }

    pub(crate) fn component_configuration(
        &self,
        id: &ComponentId,
    ) -> Option<&CapturedComponentConfiguration> {
        self.configurations.get(id)
    }

    pub(crate) fn query_api(
        &self,
        id: &ComponentId,
    ) -> Option<Arc<crate::computation::v1::QueryApi>> {
        self.query_apis.get(id).and_then(std::sync::Weak::upgrade)
    }

    /// The providers published for one component generation, if it registered any.
    pub(crate) fn output_readers(
        &self,
        id: &ComponentId,
    ) -> Option<&crate::computation::v1::WeakOutputReaders> {
        self.output_readers.get(id)
    }

    pub(crate) fn activation_is_deferred(&self, id: &ComponentId) -> bool {
        self.deferred_activation.contains(id)
    }

    pub(crate) fn output_endpoint(
        &self,
        component: &ComponentId,
        explicit: Option<&PortId>,
    ) -> GraphResult<Endpoint> {
        let node = self
            .desired
            .nodes
            .iter()
            .find(|node| node.descriptor.id() == component)
            .ok_or_else(|| topology(format!("unknown input component {component}")))?;
        let primaries = self
            .primary_ports
            .get(component)
            .cloned()
            .unwrap_or_default();
        let port = primaries.resolve(node.descriptor.ports(), PortDirection::Output, explicit)?;
        Ok(Endpoint::new(component.clone(), port))
    }

    pub(crate) fn source_schema_reader(
        &self,
        id: &ComponentId,
    ) -> GraphResult<Option<Arc<dyn crate::computation::v1::SourceSchemaReader>>> {
        self.source_schema_readers
            .get(id)
            .map(|reader| reader.upgrade().ok_or(GraphError::StaleGeneration))
            .transpose()
    }
}

pub(super) fn publish(graph: &ComputationGraph) {
    let changed = std::mem::take(&mut *graph.metadata_changes.lock().unwrap_or_else(|error| {
        log::error!("Publishing through poisoned component metadata ownership: {error}");
        error.into_inner()
    }));
    let desired = graph.desired.borrow().clone();
    let observed = graph.observed();
    let previous = graph.registry.borrow().clone();
    let next = if !Arc::ptr_eq(&previous.desired, &desired) {
        GraphRegistrySnapshot::new(
            desired,
            observed,
            &graph.resource_handles,
            graph
                .ids
                .iter()
                .map(|(id, index)| (id.clone(), graph.components[*index].configuration()))
                .collect(),
            graph
                .ids
                .iter()
                .filter_map(|(id, index)| {
                    graph.components[*index]
                        .query_api()
                        .map(|api| (id.clone(), api))
                })
                .collect(),
            graph
                .ids
                .iter()
                .filter_map(|(id, index)| {
                    graph.components[*index]
                        .source_schema_reader()
                        .map(|reader| (id.clone(), reader))
                })
                .collect(),
            graph
                .ids
                .iter()
                .filter_map(|(id, index)| {
                    graph.components[*index]
                        .output_readers()
                        .map(|readers| (id.clone(), readers))
                })
                .collect(),
            &graph.factories,
            graph.deferred_activation.clone(),
        )
    } else {
        let mut next = (*previous).clone();
        next.observed = observed.clone();
        next.deferred_activation = graph.deferred_activation.clone();
        for id in changed {
            if let Some(index) = graph.ids.get(&id) {
                let slot = &graph.components[*index];
                next.configurations.insert(id.clone(), slot.configuration());
                replace_optional(&mut next.query_apis, &id, slot.query_api());
                replace_optional(
                    &mut next.source_schema_readers,
                    &id,
                    slot.source_schema_reader(),
                );
                replace_optional(&mut next.output_readers, &id, slot.output_readers());
            } else {
                next.configurations.remove(&id);
                next.query_apis.remove(&id);
                next.source_schema_readers.remove(&id);
                next.output_readers.remove(&id);
            }
        }
        for change in previous.observed.resources.diff(&observed.resources) {
            let id = match change {
                im::ordmap::DiffItem::Add(id, _) | im::ordmap::DiffItem::Remove(id, _) => id,
                im::ordmap::DiffItem::Update { new: (id, _), .. } => id,
            };
            match graph.resource_handles.get(id) {
                Some(handle) => {
                    next.resources.insert(id.clone(), handle.downgrade());
                }
                None => {
                    next.resources.remove(id);
                }
            }
        }
        next
    };
    graph.registry.send_replace(Arc::new(next));
}

fn replace_optional<T: Clone>(
    map: &mut im::OrdMap<ComponentId, T>,
    id: &ComponentId,
    value: Option<T>,
) {
    match value {
        Some(value) => {
            map.insert(id.clone(), value);
        }
        None => {
            map.remove(id);
        }
    }
}

pub(super) fn publish_observed(
    registry: &watch::Sender<Arc<GraphRegistrySnapshot>>,
    observed: Arc<ObservedGraph>,
) {
    registry.send_modify(|current| {
        let mut next = (**current).clone();
        next.observed = observed;
        *current = Arc::new(next);
    });
}

impl GraphControl {
    pub(crate) fn registry_snapshot(&self) -> Arc<GraphRegistrySnapshot> {
        self.registry.borrow().clone()
    }

    pub(crate) fn subscribe_registry(&self) -> watch::Receiver<Arc<GraphRegistrySnapshot>> {
        self.registry.clone()
    }

    /// Capture topology and component configuration from one graph publication.
    /// Does not quiesce components, poll their getters, or serialize queued data.
    pub fn configuration_snapshot(&self) -> GraphResult<GraphConfigurationSnapshot> {
        self.registry_snapshot().configuration_snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computation::v1::*;
    use async_trait::async_trait;

    struct Idle(ComponentDescriptor);
    #[async_trait]
    impl ComputationComponent for Idle {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.0
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }
    #[async_trait]
    impl ComputationService for Idle {
        async fn run(&mut self) -> anyhow::Result<()> {
            std::future::pending().await
        }
    }

    fn graph() -> ComputationGraph {
        ComputationGraph::builder("registry")
            .service(Box::new(Idle(
                ComponentDescriptor::try_new(ComponentId::try_new("component").unwrap(), vec![])
                    .unwrap(),
            )))
            .build()
            .unwrap()
    }

    #[test]
    #[ignore = "manual publication scaling measurement"]
    #[allow(clippy::print_stdout)]
    fn publication_status_scaling() {
        for count in [16, 1024, 4096] {
            let mut builder = ComputationGraph::builder("publication-scaling");
            for index in 0..count {
                builder = builder.service(Box::new(Idle(
                    ComponentDescriptor::try_new(
                        ComponentId::try_new(format!("component-{index:05}")).unwrap(),
                        vec![],
                    )
                    .unwrap(),
                )));
            }
            let graph = builder.build().unwrap();
            let id = ComponentId::try_new("component-00000").unwrap();
            let began = std::time::Instant::now();
            for index in 0..128 {
                super::super::controller::update(&graph, |state| {
                    let component = state.components.get_mut(&id).unwrap();
                    component.operation = OperationEpoch(index);
                    component.lifecycle = if index % 2 == 0 {
                        ComponentLifecycle::Running
                    } else {
                        ComponentLifecycle::Stopped
                    };
                });
            }
            println!(
                "publication nodes={count}, updates=128, elapsed_us={}",
                began.elapsed().as_micros()
            );
        }
    }

    #[test]
    fn configuration_comparison_ignores_status_but_fences_changed_owners_and_values() {
        let graph = graph();
        let original = graph.registry.borrow().clone();
        let id = ComponentId::try_new("component").unwrap();
        let mut status = (*original).clone();
        let observed = Arc::make_mut(&mut status.observed);
        let component = observed.components.get_mut(&id).unwrap();
        component.health = ComponentHealth::Healthy;
        component.started = true;
        component.operation = OperationEpoch(component.operation.0 + 1);
        assert!(original.same_configuration_as(&status));

        let mut definition = status.clone();
        Arc::make_mut(&mut definition.desired).revision =
            GraphRevision(original.desired.revision.0 + 1);
        assert!(!original.same_configuration_as(&definition));

        let mut values = status.clone();
        values.configurations.insert(
            id.clone(),
            CapturedComponentConfiguration::Available {
                values: serde_json::json!({"changed": true}),
            },
        );
        assert!(!original.same_configuration_as(&values));

        let mut replacement = status.clone();
        let component = Arc::make_mut(&mut replacement.observed)
            .components
            .get_mut(&id)
            .unwrap();
        component.generation = ComponentGeneration(component.generation.0 + 1);
        assert!(!original.same_configuration_as(&replacement));

        let mut restarted = status;
        Arc::make_mut(&mut restarted.observed).run_epoch += 1;
        assert!(!original.same_configuration_as(&restarted));
    }

    #[test]
    fn status_publication_shares_metadata_reports_and_unchanged_observations() {
        let graph = graph();
        super::super::controller::update(&graph, |state| {
            state.deployment = Some(Arc::new(DeploymentReport {
                revision: state.revision,
                summary: OperationSummary::Completed,
                components: im::OrdMap::new(),
                resources: im::OrdMap::new(),
            }));
            state.startup = Some(Arc::new(StartReport {
                revision: state.revision,
                summary: OperationSummary::Completed,
                components: BTreeMap::new(),
            }));
        });
        let before = graph.registry.borrow().clone();
        let inspection = graph.inspector.snapshot();
        let id = ComponentId::try_new("component").unwrap();
        super::super::controller::update(&graph, |state| {
            state.components.get_mut(&id).unwrap().health = ComponentHealth::Healthy;
        });
        let after = graph.registry.borrow().clone();
        assert_eq!(
            before.observed.components[&id].health,
            ComponentHealth::Unknown
        );
        assert_eq!(
            after.observed.components[&id].health,
            ComponentHealth::Healthy
        );
        assert!(Arc::ptr_eq(&before.desired, &after.desired));
        assert!(Arc::ptr_eq(
            &inspection.desired,
            &graph.inspector.snapshot().desired
        ));
        assert!(before.configurations.ptr_eq(&after.configurations));
        assert!(before.query_apis.ptr_eq(&after.query_apis));
        assert!(before
            .source_schema_readers
            .ptr_eq(&after.source_schema_readers));
        assert!(before.output_readers.ptr_eq(&after.output_readers));
        assert!(before.primary_ports.ptr_eq(&after.primary_ports));
        assert!(before.resources.ptr_eq(&after.resources));
        assert!(before
            .observed
            .relationships
            .ptr_eq(&after.observed.relationships));
        assert!(before.observed.resources.ptr_eq(&after.observed.resources));
        assert!(Arc::ptr_eq(
            before.observed.deployment.as_ref().unwrap(),
            after.observed.deployment.as_ref().unwrap(),
        ));
        assert!(Arc::ptr_eq(
            before.observed.startup.as_ref().unwrap(),
            after.observed.startup.as_ref().unwrap(),
        ));
    }

    #[tokio::test]
    async fn component_waits_require_the_coherent_registry_publication() {
        for (started, unpublished_component) in [(false, true), (true, true), (true, false)] {
            let mut graph = graph();
            let observed = graph.observed.clone();
            let registry = graph.registry.clone();
            let run = graph.start().unwrap();
            let control = run.control();
            let id = ComponentId::try_new("component").unwrap();
            let handle = control.component_handle(&id).unwrap();
            if unpublished_component {
                let mut prior = (*control.registry_snapshot().observed).clone();
                prior.components.remove(&id);
                publish_observed(&registry, Arc::new(prior));
            }
            observed.send_modify(|snapshot| {
                let component = Arc::make_mut(snapshot).components.get_mut(&id).unwrap();
                component.realization = RealizationState::Created;
                component.started = started;
            });
            let mut waiting = Box::pin(async {
                if started {
                    handle.wait_started().await
                } else {
                    handle.wait_created().await
                }
            });
            assert!(
                futures::poll!(&mut waiting).is_pending(),
                "a component observation must not precede its public read/configuration facets"
            );
            publish_observed(&registry, observed.borrow().clone());
            tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
                .await
                .unwrap()
                .unwrap();
            drop(run);
            graph.shutdown().await.unwrap();
        }
    }

    #[tokio::test]
    async fn dropping_a_run_publishes_cleanup_observations_to_the_host_registry() {
        let mut graph = graph();
        let mut run = Box::pin(graph.start().unwrap());
        let control = run.control();
        tokio::select! {
            result = &mut run => panic!("idle graph ended: {result:?}"),
            report = control.startup_report() => assert_eq!(report.unwrap().summary, OperationSummary::Completed),
        }
        drop(run);
        assert_eq!(graph.state(), GraphState::CleanupRequired);
        assert!(Arc::ptr_eq(
            &control.registry_snapshot().observed,
            &control.observed()
        ));
        graph.shutdown().await.unwrap();
    }

    #[test]
    fn registry_and_inspection_publications_do_not_keep_resource_instances_alive() {
        let id = ResourceId::try_new("resource").unwrap();
        let instance = Arc::new(123usize);
        let weak = Arc::downgrade(&instance);
        let mut graph = ComputationGraph::builder("registry")
            .declare_resource(ResourceSpecification {
                id: id.clone(),
                role: ResourceRole::Component,
                ownership: ResourceOwnership::Borrowed,
                binding: "resource".into(),
            })
            .unwrap()
            .provide_resource(
                id.clone(),
                ResourceHandle::new(ResourceRole::Component, instance),
            )
            .unwrap()
            .build()
            .unwrap();
        publish(&graph);
        let publication = graph.registry.borrow().clone();
        let inspection = graph.inspector();
        assert_eq!(
            *publication.resource(&id).unwrap().get::<usize>().unwrap(),
            123
        );
        graph.resource_handles.remove(&id);
        assert!(weak.upgrade().is_none());
        assert!(
            matches!(publication.resource(&id), Err(GraphError::StaleGeneration)),
            "never substitute a newer binding"
        );
        drop(graph);
        assert!(weak.upgrade().is_none());
        assert_eq!(inspection.snapshot().desired.id.as_ref(), "registry");
    }
}
