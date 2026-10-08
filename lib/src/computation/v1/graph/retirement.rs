// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU8, Ordering},
        Arc,
    },
};

use super::{specification::WeakResourceHandle, *};
use crate::computation::v1::{ComponentLifecycle, RealizationState, ResourceRealization};

const HELD: u8 = 0;
const RELEASED: u8 = 1;
const RETIRED: u8 = 2;
const ABANDONED: u8 = 3;

pub(super) struct RetirementState {
    state: AtomicU8,
    pub(super) resources: BTreeMap<ResourceId, WeakResourceHandle>,
    pub(super) components: BTreeMap<ComponentId, ComponentGeneration>,
}

impl RetirementState {
    pub(super) fn active(&self) -> bool {
        self.state.load(Ordering::Acquire) != RELEASED
    }

    pub(super) fn retired(&self) -> bool {
        self.state.load(Ordering::Acquire) == RETIRED
    }
}

/// Only the graph controller can capture this lease after awaited component stop.
/// Unresolved destruction requires reconstruction, never an automatic restart.
pub(crate) struct RecoveryFreeze {
    state: Arc<RetirementState>,
    pub(crate) resources: BTreeMap<ResourceId, ResourceHandle>,
    pub(crate) contracts: Vec<super::super::ComponentRecovery>,
    resolved: bool,
}

impl RecoveryFreeze {
    pub(super) fn capture(
        graph: &mut ComputationGraph,
        resources: &BTreeSet<ResourceId>,
        components: &BTreeSet<ComponentId>,
        allow_processing_failure: bool,
    ) -> GraphResult<Self> {
        let observed = graph.observed();
        let mut handles = BTreeMap::new();
        for id in resources {
            let state = observed
                .resources
                .get(id)
                .ok_or_else(|| topology("recovery resource is unavailable"))?;
            if state.realization != ResourceRealization::Created || state.failure.is_some() {
                return Err(topology(
                    "recovery resource requires successful reconstruction",
                ));
            }
            handles.insert(
                id.clone(),
                graph
                    .resource_handles
                    .get(id)
                    .ok_or_else(|| topology("recovery resource has no owner"))?
                    .clone(),
            );
        }
        let mut contracts = Vec::new();
        let mut generations = BTreeMap::new();
        for id in components {
            let state = observed
                .components
                .get(id)
                .ok_or_else(|| topology("recovery component is unavailable"))?;
            if state.realization != RealizationState::Created
                || state.lifecycle != ComponentLifecycle::Stopped
                || state.failure.as_ref().is_some_and(|failure| {
                    !allow_processing_failure
                        || failure.phase != crate::computation::v1::FailurePhase::Processing
                })
            {
                return Err(topology(
                    "recovery component must complete stop; only authorized processing failures may remain",
                ));
            }
            let slot = &graph.components[graph.ids[id]];
            if !slot.is_constructed()? {
                return Err(topology("recovery component is not constructed"));
            }
            contracts.push(slot.recovery_contract()?);
            generations.insert(id.clone(), state.generation);
        }
        let state = Arc::new(RetirementState {
            state: AtomicU8::new(HELD),
            resources: handles
                .iter()
                .map(|(id, handle)| (id.clone(), handle.downgrade()))
                .collect(),
            components: generations,
        });
        graph.retirement = Some(state.clone());
        Ok(Self {
            state,
            resources: handles,
            contracts,
            resolved: false,
        })
    }

    pub(crate) fn resume(mut self) {
        self.state.state.store(RELEASED, Ordering::Release);
        self.resolved = true;
    }

    pub(crate) fn retire(mut self) {
        self.state.state.store(RETIRED, Ordering::Release);
        self.resolved = true;
    }
}

impl Drop for RecoveryFreeze {
    fn drop(&mut self) {
        if !self.resolved {
            self.state.state.store(ABANDONED, Ordering::Release);
        }
    }
}

impl ComputationGraph {
    pub(super) fn resource_retired(&self, id: &ResourceId) -> bool {
        self.retirement.as_ref().is_some_and(|state| {
            state.retired()
                && state.resources.get(id).is_some_and(|old| {
                    self.resource_handles.get(id).is_some_and(|current| {
                        old.upgrade().is_some_and(|old| current.same_instance(&old))
                    })
                })
        })
    }

    pub(super) fn component_retired(&self, id: &ComponentId) -> bool {
        self.retirement.as_ref().is_some_and(|state| {
            state.retired()
                && state.components.get(id).is_some_and(|generation| {
                    self.observed()
                        .components
                        .get(id)
                        .is_some_and(|component| &component.generation == generation)
                })
        })
    }

    pub(super) fn retirement_active(&self) -> bool {
        self.retirement.as_ref().is_some_and(|state| state.active())
    }

    pub(super) fn complete_retirement(&mut self) {
        let Some(retirement) = &self.retirement else {
            return;
        };
        let observed = self.observed();
        if retirement.retired()
            && retirement.components.iter().all(|(id, old)| {
                observed
                    .components
                    .get(id)
                    .map_or(true, |component| component.generation != *old)
            })
            && retirement.resources.iter().all(|(id, old)| {
                self.resource_handles.get(id).map_or(true, |current| {
                    old.upgrade()
                        .map_or(true, |old| !current.same_instance(&old))
                })
            })
        {
            self.retirement = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::computation::v1::{
        ComponentDescriptor, ComponentFailure, ComputationComponent, ComputationService,
        FailureDisposition, FailurePhase, GraphSelection,
    };

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
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn loss_authorized_retirement_only_allows_processing_failure_after_completed_stop() {
        let id = ComponentId::try_new("service").unwrap();
        let mut graph = ComputationGraph::builder("freeze")
            .service(Box::new(Service(
                ComponentDescriptor::try_new(id.clone(), vec![]).unwrap(),
            )))
            .build()
            .unwrap();
        for phase in [
            FailurePhase::Validation,
            FailurePhase::Creation,
            FailurePhase::Binding,
            FailurePhase::Activation,
            FailurePhase::Processing,
            FailurePhase::Stop,
            FailurePhase::Removal,
            FailurePhase::Control,
        ] {
            for lifecycle in [ComponentLifecycle::Stopped, ComponentLifecycle::Failed] {
                controller::update(&graph, |observed| {
                    let node = observed.components.get_mut(&id).unwrap();
                    node.realization = RealizationState::Created;
                    node.lifecycle = lifecycle;
                    node.failure = Some(ComponentFailure {
                        phase,
                        disposition: FailureDisposition::Terminal,
                        cause: Arc::new(topology("injected failure")),
                        timestamp: chrono::Utc::now(),
                    });
                });
                for authorized in [false, true] {
                    let result = RecoveryFreeze::capture(
                        &mut graph,
                        &BTreeSet::new(),
                        &BTreeSet::from([id.clone()]),
                        authorized,
                    );
                    assert_eq!(
                        result.is_ok(),
                        authorized
                            && phase == FailurePhase::Processing
                            && lifecycle == ComponentLifecycle::Stopped,
                        "{phase:?}/{lifecycle:?}: {:?}",
                        result.as_ref().err()
                    );
                    if let Ok(frozen) = result {
                        frozen.resume();
                    }
                }
            }
        }
        graph.dispose().await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn recovery_freeze_requires_resolution_and_abandonment_cannot_authorize_replacement() {
        for resolution in 0..3 {
            let mut graph = ComputationGraph::empty("freeze").unwrap();
            let run = graph.run().unwrap();
            let control = run.control();
            let (result, ()) = tokio::join!(run, async {
                let frozen = control
                    .freeze_recovery(BTreeSet::new(), BTreeSet::new(), false)
                    .await
                    .unwrap();
                assert!(matches!(
                    control
                        .start_components(control.desired_snapshot().revision, GraphSelection::All)
                        .await,
                    Err(GraphError::OperationInProgress)
                ));
                match resolution {
                    0 => frozen.resume(),
                    1 => frozen.retire(),
                    _ => drop(frozen),
                }
                let desired = control
                    .desired_snapshot()
                    .select(GraphSelection::All)
                    .unwrap();
                let result = control
                    .management_control()
                    .reconcile_components(desired.clone(), desired, TopologyBindings::default())
                    .await;
                assert_eq!(result.is_ok(), resolution != 2);
                let started = control
                    .start_components(control.desired_snapshot().revision, GraphSelection::All)
                    .await;
                assert_eq!(started.is_ok(), resolution != 2);
                control.cancel();
            });
            assert!(matches!(result, Err(GraphError::Cancelled)));
            graph.dispose().await.unwrap();
        }
    }
}
