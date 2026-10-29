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

struct StartingService {
    descriptor: ComponentDescriptor,
    hang: bool,
    stops: Arc<std::sync::atomic::AtomicUsize>,
}

#[async_trait]
impl crate::computation::v1::ComputationComponent for StartingService {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        if self.hang {
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl crate::computation::v1::ComputationService for StartingService {
    async fn run(&mut self) -> anyhow::Result<()> {
        std::future::pending().await
    }
}

#[tokio::test(start_paused = true)]
async fn activation_deadline_is_local_and_every_attempted_owner_is_cleaned_up() {
    let stops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut builder = ComputationGraph::builder("bounded-activation")
        .activation_timeout(std::time::Duration::from_secs(2));
    for (name, hang) in [("held", true), ("healthy", false)] {
        builder = builder.service(Box::new(StartingService {
            descriptor: ComponentDescriptor::try_new(id(name), Vec::new()).unwrap(),
            hang,
            stops: stops.clone(),
        }));
    }
    let mut graph = builder.build().unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let started = tokio::time::Instant::now();
        let report = control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .unwrap();
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        let StartOutcome::StartFailed(failure) = &report.components[&id("held")] else {
            panic!("hung activation must have a per-component failure");
        };
        assert!(failure.cause.to_string().contains("activation timeout"));
        assert_eq!(
            control.observed().components[&id("healthy")].lifecycle,
            ComponentLifecycle::Running
        );
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(2));
        control.cancel();
    });
    assert!(result.is_err());
    assert_eq!(stops.load(std::sync::atomic::Ordering::SeqCst), 2);
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

#[tokio::test]
async fn busy_start_group_does_not_drop_unrelated_start_requests() {
    let (graph, mut operations, _, _, _) = fixture().await;
    let source = graph.ids[&id("source")];
    let sink = graph.ids[&id("sink")];
    update(&graph, |state| {
        state.components.get_mut(&id("source")).unwrap().lifecycle = ComponentLifecycle::Failed;
        state.components.get_mut(&id("sink")).unwrap().lifecycle = ComponentLifecycle::Running;
    });
    let mut held = graph.components[source].take().unwrap();
    held.attempted = true;
    let (reply, mut blocked) = oneshot::channel();
    operations.begin_start(&graph, BTreeSet::from([source]), Some(reply), true);
    let (reply, independent) = oneshot::channel();
    operations.begin_start(&graph, BTreeSet::from([sink]), Some(reply), true);
    operations.advance_start(&graph).unwrap();
    assert!(matches!(
        blocked.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(
        independent.await.unwrap().unwrap().summary,
        OperationSummary::Completed
    );
    assert_eq!(operations.starting.len(), 1);
    drop(held);
    operations.advance_start(&graph).unwrap();
    assert!(blocked.await.unwrap().is_ok());
}

#[tokio::test]
async fn deferred_binding_waits_for_a_busy_endpoint_without_failing_the_controller() {
    let (mut graph, mut operations, mut controls, _, _) = fixture().await;
    let edge = graph.snapshot.edges[0].definition.clone();
    update(&graph, |state| {
        state.relationships.get_mut(&edge).unwrap().binding = BindingState::Declared;
    });
    let held = graph.components[graph.ids[&id("source")]].take().unwrap();
    let (_cancel, mut cancel) = watch::channel(false);
    let (_sender, mut commands) = command_channel();
    bind_ready_dependencies(
        &mut graph,
        &mut operations,
        &mut controls,
        &mut cancel,
        &mut commands,
    )
    .await
    .unwrap();
    assert!(operations.bindings_dirty);
    assert!(graph.observed().relationships[&edge].failure.is_none());
    drop(held);
    bind_ready_dependencies(
        &mut graph,
        &mut operations,
        &mut controls,
        &mut cancel,
        &mut commands,
    )
    .await
    .unwrap();
    assert_eq!(
        graph.observed().relationships[&edge].binding,
        BindingState::Bound
    );
}

#[tokio::test]
async fn stale_completion_keeps_the_current_operation_and_controller_alive() {
    let (graph, mut operations, controls, _, _) = fixture().await;
    let index = graph.ids[&id("source")];
    let generation = graph.components[index].generation;
    operations
        .complete(
            &graph,
            Completion {
                index,
                generation,
                epoch: OperationEpoch(99),
                operation: Operation::Start,
                result: Ok(Ok(())),
            },
            &controls,
        )
        .unwrap();
    let (abort, _) = AbortHandle::new_pair();
    operations.active.insert(
        index,
        Active {
            _admission_activation: None,
            epoch: OperationEpoch(2),
            operation: Operation::Start,
            abort,
            quiesce: watch::channel(false).0,
            outputs: Vec::new(),
            reply: None,
        },
    );
    operations
        .complete(
            &graph,
            Completion {
                index,
                generation,
                epoch: OperationEpoch(1),
                operation: Operation::Start,
                result: Ok(Ok(())),
            },
            &controls,
        )
        .unwrap();
    assert_eq!(operations.active[&index].epoch, OperationEpoch(2));
}

#[tokio::test]
async fn reconciliation_rejects_changed_impact_without_changing_revision() {
    let (mut graph, mut operations, mut controls, _, _) = fixture().await;
    let changes = vec![DesiredMutation::Restart(GraphSelection::Exact(vec![id("source")]))];
    let supplied = preview(&graph, changes.clone()).unwrap();
    assert!(supplied.rebind.is_empty());
    update(&graph, |state| {
        state.components.get_mut(&id("source")).unwrap().exhausted = true;
    });
    assert!(!preview(&graph, changes).unwrap().rebind.is_empty());
    let previous = graph.desired.borrow().clone();
    let (_cancel, mut cancel) = watch::channel(false);
    let (_sender, mut commands) = command_channel();
    let error = execute(
        &mut graph,
        &mut operations,
        &mut controls,
        &mut cancel,
        supplied,
        TopologyBindings::default(),
        &mut commands,
    )
    .await
    .unwrap_err();
    assert!(matches!(error, GraphError::StalePreview { .. }));
    assert!(Arc::ptr_eq(&previous, &graph.desired.borrow()));
    assert!(operations.reconciliation.is_none());
}

#[tokio::test]
async fn all_declaration_paths_reject_invalid_streams_and_subscription_cycles() {
    let (mut graph, _, _, _, _) = fixture().await;
    let original = graph.desired.borrow().clone();
    for port in ["out", "undeclared"] {
        let mut addition = super::super::super::addition::ComponentAddition::new(
            ConstructedComponent::source(Box::new(Source(descriptor("duplicate", true)))),
        );
        addition.definition.streams.insert(
            crate::computation::v1::PortId::try_new(port).unwrap(),
            crate::computation::v1::StreamId::try_new("source/out").unwrap(),
        );
        let mut pending = Some(addition);
        assert!(super::super::super::addition::insert(&mut graph, &mut pending).is_err());
        assert!(
            pending.is_some(),
            "rejection must retain supplied ownership"
        );
        assert!(Arc::ptr_eq(&original, &graph.desired.borrow()));
    }
    let mut desired = graph.snapshot.select(GraphSelection::All).unwrap();
    desired.subscriptions.push((id("sink"), id("source")));
    assert!(desired.validate_structure().is_err());
    assert!(preview(&graph, vec![DesiredMutation::SetTopology(desired)]).is_err());
    for pair in [(id("source"), id("source")), (id("missing"), id("sink"))] {
        let mut desired = graph.snapshot.select(GraphSelection::All).unwrap();
        desired.control_connections.push(pair);
        assert!(preview(&graph, vec![DesiredMutation::SetTopology(desired)]).is_err());
    }
}

#[tokio::test]
async fn rejected_control_refresh_preserves_the_live_configuration() {
    let (mut graph, mut operations, mut controls, _, _) = fixture().await;
    let previous = graph.desired.borrow().clone();
    let revision = graph.snapshot.revision;
    graph.peers.close().unwrap();
    let (reply, result) = oneshot::channel();
    let (_cancel, mut cancel) = watch::channel(false);
    let (_sender, mut commands) = command_channel();
    handle_command(
        &mut graph,
        &mut operations,
        &mut controls,
        &mut cancel,
        Command::ControlConnections {
            connections: vec![(id("sink"), id("source"))],
            subscriptions: false,
            management_access: false,
            reply,
        },
        &mut commands,
    )
    .await
    .unwrap();
    assert!(result.await.unwrap().is_err());
    assert!(graph.snapshot.control_connections.is_empty());
    assert_eq!(graph.snapshot.revision, revision);
    assert!(Arc::ptr_eq(&previous, &graph.desired.borrow()));
}

#[tokio::test]
async fn failed_reconciliation_retains_boundary_and_resume_failures() {
    let (mut graph, mut operations, mut controls, _, _) = fixture().await;
    let source = graph.ids[&id("source")];
    operations.paused.insert(source);
    update(&graph, |state| {
        let source = state.components.get_mut(&id("source")).unwrap();
        source.lifecycle = ComponentLifecycle::Quiesced;
    });
    let edge = graph.snapshot.edges[0].definition.clone();
    let plan = preview(
        &graph,
        vec![DesiredMutation::Bind(DesiredRelationship {
            definition: edge,
            policy: Default::default(),
            pipe: DesiredPipe::Bounded { capacity: 3 },
        })],
    )
    .unwrap();
    let held = graph.components[source].take().unwrap();
    let (_cancel, mut cancel) = watch::channel(false);
    let (_sender, mut commands) = command_channel();
    let report = execute(
        &mut graph,
        &mut operations,
        &mut controls,
        &mut cancel,
        plan,
        TopologyBindings::default(),
        &mut commands,
    )
    .await
    .unwrap();
    assert!(!report.committed);
    assert!(report.ensure_committed().is_err());
    assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
    assert_eq!(report.failures.len(), 2);
    assert_eq!(report.failures[0].phase, FailurePhase::Removal);
    assert_eq!(report.failures[1].phase, FailurePhase::Activation);
    drop(held);
}

async fn preflight_waits_for_retained_metadata_retirement() {
    let (mut graph, mut operations, mut controls, _, prepared) = fixture().await;
    drop(prepared);
    let plan = preview(&graph, vec![]).unwrap();
    let (release, released) = tokio::sync::oneshot::channel();
    let identity = super::super::super::execution::GraphIdentity {
        graph: graph.snapshot.id.clone(),
        run_epoch: graph.observed().run_epoch,
        phase: FailurePhase::Removal,
    };
    let (abort, _registration) = AbortHandle::new_pair();
    let joined = graph
        .tasks
        .retirements
        .spawn(identity, abort, async move {
            released.await.map_err(|_| GraphError::ControllerClosed)?;
            Ok(())
        })
        .unwrap();
    operations.retirements.push(
        async move {
            RetirementCompletion {
                slot: None,
                result: joined.await.and_then(|result| result),
            }
        }
        .boxed(),
    );
    let (_cancel, mut cancel) = watch::channel(false);
    let (_sender, mut commands) = command_channel();
    let prepared = {
        let preparation = prepare_owned(
            &mut graph,
            &mut operations,
            &mut controls,
            &mut cancel,
            &mut commands,
            &plan,
            TopologyBindings::default(),
        );
        tokio::pin!(preparation);
        tokio::select! {
            _ = &mut preparation => panic!("preflight must join the retained retirement"),
            _ = tokio::time::sleep(std::time::Duration::from_millis(20)) => {}
        }
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), preparation)
            .await
            .unwrap()
            .unwrap()
    };
    assert!(graph.tasks.retirements.is_empty());
    assert!(graph.tasks.preparations.is_empty());
    drop(prepared);
    drop(operations);
    drop(controls);
    graph.shutdown().await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn preflight_waits_for_retained_metadata_retirement_current_thread() {
    preflight_waits_for_retained_metadata_retirement().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preflight_waits_for_retained_metadata_retirement_multi_thread() {
    preflight_waits_for_retained_metadata_retirement().await;
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
    let mut operations = Operations::default();
    let controls = deploy(&mut graph, &mut cancel, &mut operations)
        .await
        .unwrap();
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
    let prepared = prepare(&PreparationSnapshot::capture(&graph), &plan, bindings).unwrap();
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
    assert_eq!(operations.starting[0].pending, BTreeSet::from([source]));
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
            for id in &state.components.keys().cloned().collect::<Vec<_>>() {
                let node = state.components.get_mut(id).expect("observed component");
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
