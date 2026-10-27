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

/// A coherent current publication for in-process hosts. Weak bindings avoid a
/// cycle through components that themselves hold their graph's control handle.
/// Public inspection/history continues to contain no runtime handles.
#[derive(Clone)]
pub(crate) struct GraphRegistrySnapshot {
    pub desired: Arc<GraphSnapshot>,
    pub observed: Arc<ObservedGraph>,
    resources: BTreeMap<ResourceId, specification::WeakResourceHandle>,
    configurations: BTreeMap<ComponentId, CapturedComponentConfiguration>,
    query_apis: BTreeMap<ComponentId, std::sync::Weak<crate::computation::v1::QueryApi>>,
    source_schema_readers:
        BTreeMap<ComponentId, std::sync::Weak<dyn crate::computation::v1::SourceSchemaReader>>,
    output_readers: BTreeMap<ComponentId, crate::computation::v1::WeakOutputReaders>,
    primary_ports: BTreeMap<ComponentId, crate::computation::v1::PortPrimaries>,
    deferred_activation: BTreeSet<ComponentId>,
}

impl GraphRegistrySnapshot {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        desired: &GraphSnapshot,
        observed: Arc<ObservedGraph>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
        configurations: BTreeMap<ComponentId, CapturedComponentConfiguration>,
        query_apis: BTreeMap<ComponentId, std::sync::Weak<crate::computation::v1::QueryApi>>,
        source_schema_readers: BTreeMap<
            ComponentId,
            std::sync::Weak<dyn crate::computation::v1::SourceSchemaReader>,
        >,
        output_readers: BTreeMap<ComponentId, crate::computation::v1::WeakOutputReaders>,
        factories: &BTreeMap<ImplementationIdentity, Arc<dyn ComponentFactory>>,
        deferred_activation: BTreeSet<ComponentId>,
    ) -> Self {
        Self {
            desired: Arc::new(desired.clone()),
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
            configurations: self.configurations.clone(),
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
    graph
        .registry
        .send_replace(Arc::new(GraphRegistrySnapshot::new(
            &graph.snapshot,
            graph.observed(),
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
        )));
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
        let mut graph = graph();
        let id = ResourceId::try_new("resource").unwrap();
        let instance = Arc::new(123usize);
        let weak = Arc::downgrade(&instance);
        graph.resource_handles.insert(
            id.clone(),
            ResourceHandle::new(ResourceRole::Component, instance),
        );
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
