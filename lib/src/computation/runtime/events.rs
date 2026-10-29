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

use super::*;
use crate::{
    channels::{ComponentEvent, ComponentEventBroadcastSender, ComponentType},
    observability::ComponentEventHistory,
};
use std::sync::Mutex as SyncMutex;

/// An event journal, not a component registry. Membership and statuses are
/// derived from each pair of authoritative controller publications.
pub(super) struct Events {
    pub(super) broadcast: ComponentEventBroadcastSender,
    history: SyncMutex<ComponentEventHistory>,
    kinds: SyncMutex<Option<Arc<ComponentKinds>>>,
}

struct ComponentKinds {
    desired: Arc<GraphSnapshot>,
    values: BTreeMap<ComponentId, ComponentType>,
}

impl ComponentKinds {
    fn new(desired: Arc<GraphSnapshot>) -> Self {
        let values = desired
            .nodes
            .iter()
            .filter_map(|node| {
                let kind = match Runtime::public_component_kind(&desired, node) {
                    Some("source") => ComponentType::Source,
                    Some("query") => ComponentType::Query,
                    Some("reaction") => ComponentType::Reaction,
                    _ => return None,
                };
                Some((node.descriptor.id().clone(), kind))
            })
            .collect();
        Self { desired, values }
    }
}

impl Events {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            broadcast: tokio::sync::broadcast::channel(1000).0,
            history: SyncMutex::new(ComponentEventHistory::new()),
            kinds: SyncMutex::new(None),
        })
    }

    pub(super) fn component(&self, id: &str) -> Vec<ComponentEvent> {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_events(id)
    }

    pub(super) fn all(&self) -> Vec<ComponentEvent> {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_all_events()
    }

    pub(super) fn subscribe(
        &self,
        id: &str,
    ) -> (
        Vec<ComponentEvent>,
        tokio::sync::broadcast::Receiver<ComponentEvent>,
    ) {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscribe(id)
    }

    fn record(&self, event: ComponentEvent) {
        self.history
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .record_event(event.clone());
        let _ = self.broadcast.send(event);
    }

    fn publication_kinds(
        &self,
        previous: &ComputationInspection,
        current: &ComputationInspection,
    ) -> (Arc<ComponentKinds>, Arc<ComponentKinds>) {
        let mut cached = self.kinds.lock().unwrap_or_else(|error| {
            log::error!("Component event metadata cache is poisoned: {error}");
            error.into_inner()
        });
        let previous = cached
            .as_ref()
            .filter(|cached| Arc::ptr_eq(&cached.desired, &previous.desired))
            .cloned()
            .unwrap_or_else(|| Arc::new(ComponentKinds::new(previous.desired.clone())));
        let current = if Arc::ptr_eq(&previous.desired, &current.desired) {
            previous.clone()
        } else {
            Arc::new(ComponentKinds::new(current.desired.clone()))
        };
        *cached = Some(current.clone());
        (previous, current)
    }
}

fn changed_components<'a>(
    previous: &'a ComputationInspection,
    current: &'a ComputationInspection,
) -> (Vec<&'a ComponentId>, Vec<&'a ComponentId>) {
    // A topology edit may change kind or record identity without changing status.
    let topology_changed = !Arc::ptr_eq(&previous.desired, &current.desired);
    let mut changed = if topology_changed {
        current.observed.components.keys().collect()
    } else {
        Vec::new()
    };
    let mut removed = Vec::new();
    for change in previous
        .observed
        .components
        .diff(&current.observed.components)
    {
        match change {
            im::ordmap::DiffItem::Add(id, _)
            | im::ordmap::DiffItem::Update { new: (id, _), .. } => {
                if !topology_changed {
                    changed.push(id);
                }
            }
            im::ordmap::DiffItem::Remove(id, _) => removed.push(id),
        }
    }
    (changed, removed)
}

pub(super) fn observed_status(
    observed: &ObservedComponent,
    initial: ComponentStatus,
) -> ComponentStatus {
    if observed.failure.is_some() || observed.lifecycle == ComponentLifecycle::Failed {
        ComponentStatus::Error
    } else if observed.lifecycle == ComponentLifecycle::Starting {
        ComponentStatus::Starting
    } else if observed.lifecycle == ComponentLifecycle::Stopping {
        ComponentStatus::Stopping
    } else if !observed.lifecycle_requested {
        initial
    } else if observed.realization == RealizationState::CreationFailed {
        // An explicit stop acknowledged the creation failure.
        ComponentStatus::Stopped
    } else if observed.realization != RealizationState::Created {
        initial
    } else if observed.exhausted || observed.lifecycle == ComponentLifecycle::Stopped {
        ComponentStatus::Stopped
    } else {
        match observed.lifecycle {
            ComponentLifecycle::Stopped => ComponentStatus::Stopped,
            ComponentLifecycle::Starting => ComponentStatus::Starting,
            ComponentLifecycle::Running => ComponentStatus::Running,
            ComponentLifecycle::Quiescing | ComponentLifecycle::Quiesced => {
                ComponentStatus::Reconfiguring
            }
            ComponentLifecycle::Stopping => ComponentStatus::Stopping,
            ComponentLifecycle::Failed => ComponentStatus::Error,
        }
    }
}

impl PublicationObserver for Events {
    fn publish(&self, previous: &ComputationInspection, current: &ComputationInspection) {
        let (previous_kinds, current_kinds) = self.publication_kinds(previous, current);
        let (changed, removed) = changed_components(previous, current);
        for id in changed {
            let observed = &current.observed.components[id];
            let Some(kind) = current_kinds.values.get(id) else {
                continue;
            };
            let old = previous.observed.components.get(id);
            let replaced = old.is_some_and(|old| old.generation != observed.generation)
                || old.is_some()
                    && record_token(&previous.desired, id) != record_token(&current.desired, id);
            let status = observed_status(observed, ComponentStatus::Added);
            let emit = |status, message| {
                self.record(ComponentEvent {
                    component_id: id.to_string(),
                    component_type: kind.clone(),
                    status,
                    timestamp: current.timestamp,
                    message,
                });
            };
            if old.is_none() {
                emit(ComponentStatus::Added, Some(format!("{kind:?} added")));
            } else if replaced {
                emit(
                    ComponentStatus::Reconfiguring,
                    Some(format!("Reconfiguring {kind:?}")),
                );
            }
            let old_status = old.map(|old| observed_status(old, ComponentStatus::Added));
            // Requesting a first start changes intent before the lifecycle
            // transitions. It is not a stop of a previously running component.
            if !replaced
                && status == ComponentStatus::Stopped
                && !observed.started
                && old
                    .is_some_and(|old| !old.started && old.lifecycle == ComponentLifecycle::Stopped)
            {
                continue;
            }
            if replaced || old_status != Some(status) && status != ComponentStatus::Added {
                emit(
                    if replaced && status == ComponentStatus::Added {
                        ComponentStatus::Stopped
                    } else {
                        status
                    },
                    observed
                        .failure
                        .as_ref()
                        .map(|failure| format!("{:#}", failure.cause)),
                );
            }
        }

        for id in removed {
            if let Some(kind) = previous_kinds.values.get(id) {
                self.record(ComponentEvent {
                    component_id: id.to_string(),
                    component_type: kind.clone(),
                    status: ComponentStatus::Removed,
                    timestamp: current.timestamp,
                    message: Some("Component removed".into()),
                });
                self.history
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .remove_component(id.as_str());
            }
        }
    }
}

#[cfg(test)]
mod publication_tests {
    use super::*;

    struct Service(ComponentDescriptor);
    #[async_trait::async_trait]
    impl ComputationComponent for Service {
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
    #[async_trait::async_trait]
    impl ComputationService for Service {
        async fn run(&mut self) -> anyhow::Result<()> {
            std::future::pending().await
        }
    }

    fn id(index: usize) -> ComponentId {
        ComponentId::try_new(format!("component-{index:05}")).unwrap()
    }

    fn graph(count: usize) -> ComputationGraph {
        let mut builder = ComputationGraph::builder("event-publication");
        for index in 0..count {
            builder = builder.service(Box::new(Service(
                ComponentDescriptor::try_new(id(index), vec![])
                    .unwrap()
                    .with_semantic_kind(ComponentSemanticKind::Source),
            )));
        }
        builder.build().unwrap()
    }

    fn publication(count: usize) -> ComputationInspection {
        let graph = graph(count);
        let mut snapshot = (*graph.inspector().snapshot()).clone();
        let observed = Arc::make_mut(&mut snapshot.observed);
        for index in 0..count {
            let node = observed.components.get_mut(&id(index)).unwrap();
            node.realization = RealizationState::Created;
            node.lifecycle_requested = true;
            node.started = true;
            node.lifecycle = ComponentLifecycle::Running;
        }
        snapshot
    }

    #[allow(clippy::print_stdout)]
    async fn control_latency() {
        for count in [16, 1024, 4096] {
            let mut graph = graph(count);
            let events = Events::new();
            let observer: Arc<dyn PublicationObserver> = events;
            graph.inspector().observe(&observer);
            {
                let mut run = Box::pin(graph.start().unwrap());
                let control = run.control();
                tokio::select! {
                    result = run.as_mut() => panic!("graph ended before startup: {result:?}"),
                    result = control.startup_report() => {
                        assert_eq!(result.unwrap().summary, OperationSummary::Completed);
                    }
                }
                let mut notifications = control.inspector().subscribe();
                let component = id(count / 2);
                let mut samples = Vec::new();
                for index in 0..128 {
                    notifications.borrow_and_update();
                    let state = control.observed().components[&component].clone();
                    let health = if index % 2 == 0 {
                        ComponentHealth::Healthy
                    } else {
                        ComponentHealth::Degraded
                    };
                    let began = std::time::Instant::now();
                    let request = control.report_health(
                        control.desired_snapshot().revision,
                        HealthObservation {
                            component: component.clone(),
                            generation: state.generation,
                            operation: state.operation,
                            health,
                        },
                    );
                    tokio::select! {
                        result = run.as_mut() => panic!("graph ended during observation: {result:?}"),
                        result = request => result.unwrap(),
                    }
                    notifications.changed().await.unwrap();
                    assert_eq!(
                        notifications.borrow().observed.components[&component].health,
                        health
                    );
                    samples.push(began.elapsed().as_nanos());
                }
                samples.sort_unstable();
                println!(
                            "publication control flavor={:?}, nodes={count}, samples=128, median_ns={}, p95_ns={}, max_ns={}",
                            tokio::runtime::Handle::current().runtime_flavor(),
                            samples[64], samples[121], samples[127],
                        );
                control.cancel();
                assert!(matches!(run.await, Ok(()) | Err(GraphError::Cancelled)));
            }
            graph.dispose().await.unwrap();
        }
    }

    #[tokio::test]
    #[ignore = "manual control and notification latency measurement"]
    async fn publication_control_latency_current_thread() {
        control_latency().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "manual control and notification latency measurement"]
    async fn publication_control_latency_multi_thread() {
        control_latency().await;
    }

    #[test]
    fn one_changed_component_reuses_kind_metadata_and_visits_one_candidate() {
        let previous = publication(4096);
        let mut current = previous.clone();
        Arc::make_mut(&mut current.observed)
            .components
            .get_mut(&id(2048))
            .unwrap()
            .lifecycle = ComponentLifecycle::Stopped;
        let (changed, removed) = changed_components(&previous, &current);
        assert_eq!(changed, vec![&id(2048)]);
        assert!(removed.is_empty());
        let events = Events::new();
        events.publish(&previous, &current);
        let (old_kinds, next_kinds) = events.publication_kinds(&previous, &current);
        assert!(Arc::ptr_eq(&old_kinds, &next_kinds));
        assert_eq!(events.component(id(2048).as_str()).len(), 1);
        assert!(events.component(id(0).as_str()).is_empty());
        assert!(changed_components(&current, &current).0.is_empty());

        let mut edited = current.clone();
        Arc::make_mut(&mut edited.desired).revision.0 += 1;
        assert_eq!(changed_components(&current, &edited).0.len(), 4096);
    }

    #[test]
    fn changed_and_added_events_precede_removal_and_removed_history_is_cleared() {
        let previous = publication(3);
        let mut current = previous.clone();
        let mut nodes = previous.desired.nodes.to_vec();
        nodes.remove(0);
        let mut addition = nodes[0].clone();
        addition.descriptor = ComponentDescriptor::try_new(id(3), vec![])
            .unwrap()
            .with_semantic_kind(ComponentSemanticKind::Source);
        nodes.push(addition);
        Arc::make_mut(&mut current.desired).nodes = nodes.into();
        let observed = Arc::make_mut(&mut current.observed);
        let addition = observed.components.remove(&id(0)).unwrap();
        observed.components.insert(id(3), addition);
        observed.components.get_mut(&id(1)).unwrap().lifecycle = ComponentLifecycle::Stopped;
        let events = Events::new();
        let mut stream = events.broadcast.subscribe();
        events.publish(&previous, &current);
        let expected = [
            (id(1), ComponentStatus::Stopped),
            (id(3), ComponentStatus::Added),
            (id(3), ComponentStatus::Running),
            (id(0), ComponentStatus::Removed),
        ];
        for (id, status) in expected {
            let event = stream.try_recv().unwrap();
            assert_eq!(event.component_id, id.as_str());
            assert_eq!(event.status, status);
        }
        assert!(stream.try_recv().is_err());
        assert!(events.component(id(0).as_str()).is_empty());
    }
}
