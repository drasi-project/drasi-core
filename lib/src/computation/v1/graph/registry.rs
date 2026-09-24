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
}

impl GraphRegistrySnapshot {
    pub(super) fn new(
        desired: &GraphSnapshot,
        observed: Arc<ObservedGraph>,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
        configurations: BTreeMap<ComponentId, CapturedComponentConfiguration>,
    ) -> Self {
        Self {
            desired: Arc::new(desired.clone()),
            observed,
            configurations,
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
