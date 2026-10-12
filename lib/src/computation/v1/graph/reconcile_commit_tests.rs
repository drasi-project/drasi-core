// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;

struct Source(ComponentDescriptor);
#[async_trait]
impl crate::computation::v1::ComputationComponent for Source {
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
impl crate::computation::v1::EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<crate::computation::v1::OutputEnvelope>> {
        std::future::pending().await
    }
}
struct Sink(ComponentDescriptor);
#[async_trait]
impl crate::computation::v1::ComputationComponent for Sink {
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
impl crate::computation::v1::EnvelopeSink for Sink {
    fn completion(&self) -> crate::computation::v1::SinkCompletion {
        crate::computation::v1::SinkCompletion::Handled
    }
    async fn handle(&mut self, _: crate::computation::v1::InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}
fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).unwrap()
}
fn descriptor(value: &str, output: bool) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id(value),
        vec![crate::computation::v1::PortDescriptor::new(
            crate::computation::v1::PortId::try_new(if output { "out" } else { "in" }).unwrap(),
            if output {
                PortDirection::Output
            } else {
                PortDirection::Input
            },
            crate::computation::v1::GraphChangeCodec::schema()
                .descriptor()
                .clone(),
            PipeRequirements::default(),
        )],
    )
    .unwrap()
}

async fn fixture() -> (
    ComputationGraph,
    Operations,
    PipeGuard,
    ReconciliationPreview,
    Prepared,
) {
    let edge = EdgeDefinition::new(
        Endpoint::new(
            id("source"),
            crate::computation::v1::PortId::try_new("out").unwrap(),
        ),
        Endpoint::new(
            id("sink"),
            crate::computation::v1::PortId::try_new("in").unwrap(),
        ),
    );
    let mut graph = ComputationGraph::builder("commit-faults")
        .source(Box::new(Source(descriptor("source", true))))
        .sink(Box::new(Sink(descriptor("sink", false))))
        .bind_stream(
            edge.from.clone(),
            crate::computation::v1::StreamId::try_new("source/out").unwrap(),
        )
        .connect(
            edge.clone(),
            Box::new(crate::computation::v1::BoundedPipeConfig { capacity: 2 }),
        )
        .build()
        .unwrap();
    let (_cancel, mut cancel) = watch::channel(false);
    let controls = deploy(&mut graph, &mut cancel).await.unwrap();
    let mut operations = Operations::default();
    for &index in &graph.order {
        operations.attach_control(&graph, index).unwrap();
    }
    operations.refresh_connections(&graph).unwrap();
    let desired = graph
        .snapshot
        .select(GraphSelection::All)
        .unwrap()
        .components
        .into_iter()
        .find(|node| node.descriptor.id() == &id("sink"))
        .unwrap();
    let mut bindings = TopologyBindings::default();
    let ComponentConstruction::External { binding } = &desired.construction else {
        panic!("external fixture")
    };
    bindings.components.insert(
        binding.clone(),
        ConstructedComponent::sink(Box::new(Sink(descriptor("sink", false)))),
    );
    let plan = preview(
        &graph,
        vec![
            DesiredMutation::ReplaceComponent(desired),
            DesiredMutation::Bind(DesiredRelationship {
                definition: edge,
                policy: Default::default(),
                pipe: DesiredPipe::Bounded { capacity: 3 },
            }),
        ],
    )
    .unwrap();
    let prepared = prepare(&graph, &plan, bindings).unwrap();
    (graph, operations, controls, plan, prepared)
}

#[tokio::test]
async fn failed_commit_preflight_preserves_slots_bindings_counters_and_publications() {
    for fault in 0..6 {
        let (mut graph, mut operations, mut controls, mut plan, mut prepared) = fixture().await;
        match fault {
            0 => graph.next_generation = u64::MAX,
            1 => graph.next_edge = usize::MAX,
            2 => graph.next_binding_generation = u64::MAX,
            3 => {
                graph.next_resource_generation = u64::MAX;
                plan.resources
                    .insert(ResourceId::try_new("exhaustion-probe").unwrap());
            }
            _ => {}
        }
        let sink_index = graph.ids[&id("sink")];
        let held = (fault == 4).then(|| graph.components[sink_index].take().unwrap());
        let old_slot = graph.components[sink_index].clone();
        let peer = operations.peer_controls[&sink_index].clone();
        if fault == 5 {
            graph
                .peers
                .remove(&id("sink"), old_slot.generation)
                .unwrap();
        }
        let desired = graph.desired.borrow().clone();
        let observed = graph.observed();
        let inspection = graph.inspector.snapshot();
        let counters = (
            graph.next_generation,
            graph.next_edge,
            graph.next_binding_generation,
            graph.next_resource_generation,
        );
        let provider_ids = graph.providers.keys().copied().collect::<Vec<_>>();
        let edge_ids = graph.edges.keys().copied().collect::<Vec<_>>();
        let old_controls = controls.0.clone();
        let error = commit_desired(
            &mut graph,
            &mut operations,
            &mut controls,
            &plan,
            &mut prepared,
        )
        .unwrap_err();
        match fault {
            0..=3 => assert!(matches!(error, GraphError::Topology { .. })),
            4 => assert!(matches!(error, GraphError::OperationInProgress)),
            5 => assert!(matches!(
                error,
                GraphError::Control(crate::computation::v1::ControlError::StaleComponent { .. })
            )),
            _ => unreachable!(),
        }
        assert!(Arc::ptr_eq(&desired, &graph.desired.borrow()));
        assert!(Arc::ptr_eq(&observed, &graph.observed()));
        assert!(Arc::ptr_eq(&inspection, &graph.inspector.snapshot()));
        assert!(Arc::ptr_eq(&old_slot, &graph.components[sink_index]));
        assert_eq!(
            counters,
            (
                graph.next_generation,
                graph.next_edge,
                graph.next_binding_generation,
                graph.next_resource_generation
            )
        );
        assert_eq!(
            provider_ids,
            graph.providers.keys().copied().collect::<Vec<_>>()
        );
        assert_eq!(edge_ids, graph.edges.keys().copied().collect::<Vec<_>>());
        for (id, before) in old_controls {
            assert!(Arc::ptr_eq(&before, &controls.0[&id]));
        }
        drop(held);
        assert_eq!(graph.components[sink_index].take().unwrap().inputs.len(), 1);
        if fault != 5 {
            peer.not_ready().unwrap();
        }
    }
}

#[tokio::test]
async fn commit_publishes_one_coherent_revision_and_fences_replaced_control() {
    let (mut graph, mut operations, mut controls, plan, mut prepared) = fixture().await;
    let old_slot = graph.components[graph.ids[&id("sink")]].clone();
    let old_control = operations.peer_controls[&graph.ids[&id("sink")]].clone();
    let previous = graph.inspector.snapshot();
    commit_desired(
        &mut graph,
        &mut operations,
        &mut controls,
        &plan,
        &mut prepared,
    )
    .unwrap();
    let next = graph.inspector.snapshot();
    assert_eq!(next.sequence, previous.sequence + 1);
    assert_eq!(next.desired.revision, next.observed.revision);
    assert_eq!(next.desired.revision, plan.desired.revision);
    assert!(next.observed.components[&id("sink")].generation > old_slot.generation);
    assert!(matches!(
        old_control.not_ready(),
        Err(crate::computation::v1::ControlError::StaleComponent { .. })
    ));
    assert!(old_slot.value.lock().unwrap().is_none());
    for index in graph.order.clone() {
        operations.attach_control(&graph, index).unwrap();
    }
    operations.refresh_connections(&graph).unwrap();
    operations.peer_controls[&graph.ids[&id("sink")]]
        .not_ready()
        .unwrap();
}

#[tokio::test]
async fn draining_stop_keeps_suspended_start_members_in_their_original_report() {
    let (graph, mut operations, controls, _, _) = fixture().await;
    let source = graph.ids[&id("source")];
    let sink = graph.ids[&id("sink")];
    operations.launch(&graph, sink, Operation::Process).unwrap();
    let (reply, mut started) = oneshot::channel();
    operations.begin_start(&graph, BTreeSet::from([source]), Some(reply), true);
    let (_cancel, mut cancel) = watch::channel(false);
    quiesce_before_stop(
        &graph,
        &mut operations,
        &controls,
        &mut cancel,
        &BTreeSet::from([source, sink]),
    )
    .await
    .unwrap();
    assert!(matches!(
        started.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(
        operations.starting.as_ref().unwrap().pending,
        BTreeSet::from([source])
    );
    assert!(!operations.active.contains_key(&source));
    let (reply, _) = oneshot::channel();
    operations
        .begin_stop(&graph, BTreeSet::from([source, sink]), reply)
        .unwrap();
    operations.advance_start(&graph).unwrap();
    let report = started.await.unwrap().unwrap();
    assert!(matches!(
        report.components[&id("source")],
        StartOutcome::NotRequested
    ));
    assert!(!graph.observed().components[&id("source")].started);
}

#[tokio::test]
async fn resume_collects_all_failures_and_still_restarts_later_members() {
    for failed_ids in [vec!["sink"], vec!["sink", "source"]] {
        let (graph, mut operations, _controls, _, _) = fixture().await;
        let selected = BTreeSet::from([id("sink"), id("source")]);
        for id in &selected {
            operations.paused.insert(graph.ids[id]);
        }
        update(&graph, |state| {
            for (id, node) in &mut state.components {
                node.lifecycle = ComponentLifecycle::Quiesced;
                if failed_ids.contains(&id.as_str()) {
                    node.operation = OperationEpoch(u64::MAX);
                }
            }
        });
        let error = resume(&graph, &mut operations, &selected).unwrap_err();
        let GraphError::Cleanup {
            primary: None,
            errors,
        } = error
        else {
            panic!("expected aggregate resume errors");
        };
        assert_eq!(errors.len(), failed_ids.len());
        let failures = errors
            .iter()
            .map(|error| {
                let GraphError::Component {
                    component,
                    operation: "resume processing",
                    ..
                } = error.underlying()
                else {
                    panic!("resume failure lost its component identity: {error}");
                };
                component.as_str()
            })
            .collect::<Vec<_>>();
        assert_eq!(failures, failed_ids);
        let observed = graph.observed();
        for id in selected {
            let index = graph.ids[&id];
            let node = &observed.components[&id];
            if failed_ids.contains(&id.as_str()) {
                assert_eq!(node.lifecycle, ComponentLifecycle::Quiesced);
                assert_eq!(node.health, ComponentHealth::Unavailable);
                assert!(operations.paused.contains(&index));
                assert!(!operations.active.contains_key(&index));
            } else {
                assert_eq!(node.lifecycle, ComponentLifecycle::Running);
                assert!(!operations.paused.contains(&index));
                assert!(operations.active.contains_key(&index));
            }
        }
    }
}
