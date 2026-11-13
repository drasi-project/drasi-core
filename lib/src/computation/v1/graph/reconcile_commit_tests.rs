// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::{
    ComponentCreationError, ComponentDescriptor, ComponentFactory, ComponentSpecification,
    ComponentUpdateData, ComponentUpdateMode, ComponentUpdatePlan, ConfigurationSchema,
    ConstructedComponent, ConstructionContext, Endpoint, FactoryDescriptor, ImplementationIdentity,
    PipeRequirements,
};
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

struct SinkFactory(FactoryDescriptor);

impl SinkFactory {
    fn new() -> Self {
        Self(FactoryDescriptor {
            implementation: ImplementationIdentity::try_new("test/reconcile-sink", "1").unwrap(),
            role: ComponentRole::Sink,
            configuration_version: 1,
            configuration: ConfigurationSchema::default(),
            dependencies: BTreeMap::new(),
        })
    }

    fn specification(&self) -> ComponentSpecification {
        ComponentSpecification {
            descriptor: descriptor(2, false),
            role: ComponentRole::Sink,
            completion: Some(crate::computation::v1::SinkCompletion::Handled),
            implementation: self.0.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        }
    }
}

#[async_trait]
impl ComponentFactory for SinkFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.0
    }

    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }

    fn plan_update(
        &self,
        _: &ComponentSpecification,
        _: &ComponentSpecification,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        })
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        Ok(ConstructedComponent::sink(Box::new(Sink(
            context.specification.descriptor.clone(),
        ))))
    }
}

fn id(value: u128) -> ComponentId {
    ComponentId::from_uuid(uuid::Uuid::from_u128(value))
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
    for (number, name, hang) in [(3, "held", true), (4, "healthy", false)] {
        builder = builder.service(
            ComponentName::try_new(name).unwrap(),
            Box::new(StartingService {
                descriptor: ComponentDescriptor::try_new(id(number), Vec::new()).unwrap(),
                hang,
                stops: stops.clone(),
            }),
        );
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
        let StartOutcome::StartFailed(failure) = &report.components[&id(3)] else {
            panic!("hung activation must have a per-component failure");
        };
        assert!(failure.cause.to_string().contains("activation timeout"));
        assert_eq!(
            control.observed().components[&id(4)].lifecycle,
            ComponentLifecycle::Running
        );
        assert_eq!(started.elapsed(), std::time::Duration::from_secs(2));
        control.cancel();
    });
    assert!(result.is_err());
    assert_eq!(stops.load(std::sync::atomic::Ordering::SeqCst), 2);
}

fn descriptor(value: u128, output: bool) -> ComponentDescriptor {
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
    let source = graph.ids[&id(1)];
    let sink = graph.ids[&id(2)];
    update(&graph, |state| {
        state.components.get_mut(&id(1)).unwrap().lifecycle = ComponentLifecycle::Failed;
        state.components.get_mut(&id(2)).unwrap().lifecycle = ComponentLifecycle::Running;
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
async fn individual_handle_start_waits_for_the_reconciliation_reservation() {
    for committed in [false, true] {
        let (mut graph, mut operations, mut controls, _, _) = fixture().await;
        let component = id(1);
        let index = graph.ids[&component];
        let generation = graph.components[index].generation;
        operations.reconciliation = Some(ReconciliationReservation {
            components: BTreeSet::from([component.clone()]),
            resources: BTreeSet::new(),
            resource_instances: Vec::new(),
            committed,
        });
        let (reply, mut result) = oneshot::channel();
        let (_cancel, mut cancel) = watch::channel(false);
        let (_sender, mut commands) = command_channel();
        handle_command(
            &mut graph,
            &mut operations,
            &mut controls,
            &mut cancel,
            Command::HandleStart {
                component: component.clone(),
                generation,
                reply,
            },
            &mut commands,
        )
        .await
        .unwrap();
        assert!(matches!(
            result.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        assert_eq!(operations.handle_requests.len(), 1);
        assert!(
            !begin_handle_requests(&mut graph, &mut operations, &controls, &mut cancel)
                .await
                .unwrap()
        );
        assert!(!graph.observed().components[&component].lifecycle_requested);

        operations.reconciliation = None;
        assert!(
            begin_handle_requests(&mut graph, &mut operations, &controls, &mut cancel)
                .await
                .unwrap()
        );
        assert!(operations.handle_requests.is_empty());
        assert!(graph.observed().components[&component].lifecycle_requested);
        assert!(operations
            .starting
            .iter()
            .any(|group| group.pending.contains(&index)));
        assert_eq!(graph.components[index].generation, generation);
    }
}

#[tokio::test]
async fn deferred_binding_waits_for_a_busy_endpoint_without_failing_the_controller() {
    let (mut graph, mut operations, mut controls, _, _) = fixture().await;
    let edge = graph.snapshot.edges[0].definition.clone();
    update(&graph, |state| {
        state.relationships.get_mut(&edge).unwrap().binding = BindingState::Declared;
    });
    let held = graph.components[graph.ids[&id(1)]].take().unwrap();
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

async fn created_group_start_waits_for_the_committed_binding_reservation() {
    let (mut graph, mut operations, mut controls, _, _) = fixture().await;
    let members = BTreeMap::from([id(1), id(2)].map(|id| {
        let observed = graph.observed();
        assert_eq!(
            observed.components[&id].realization,
            RealizationState::Created
        );
        let generation = observed.components[&id].generation;
        (id, generation)
    }));
    operations.reconciliation = Some(ReconciliationReservation {
        components: BTreeSet::from([id(2)]),
        resources: BTreeSet::new(),
        resource_instances: Vec::new(),
        committed: true,
    });
    let (_cancel, mut cancel) = watch::channel(false);
    let (_sender, mut commands) = command_channel();
    let (reply, result) = oneshot::channel();
    let revision = graph.snapshot.revision;
    handle_command(
        &mut graph,
        &mut operations,
        &mut controls,
        &mut cancel,
        Command::Start {
            revision,
            selection: GraphSelection::Exact(members.keys().cloned().collect()),
            force: true,
            reply,
        },
        &mut commands,
    )
    .await
    .unwrap();
    assert!(matches!(
        result.await.unwrap(),
        Err(GraphError::OperationInProgress)
    ));

    let (reply, mut result) = oneshot::channel();
    handle_command(
        &mut graph,
        &mut operations,
        &mut controls,
        &mut cancel,
        Command::StartMembers {
            members: members.clone(),
            reply,
        },
        &mut commands,
    )
    .await
    .unwrap();
    assert!(matches!(
        result.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    assert_eq!(operations.handle_requests.len(), 1);
    assert!(
        !begin_handle_requests(&mut graph, &mut operations, &controls, &mut cancel)
            .await
            .unwrap()
    );
    for id in members.keys() {
        assert!(!graph.observed().components[id].lifecycle_requested);
    }

    operations.reconciliation = None;
    assert!(
        begin_handle_requests(&mut graph, &mut operations, &controls, &mut cancel)
            .await
            .unwrap()
    );
    let report = reconcile::drive(&graph, &mut operations, &controls, &mut cancel, async {
        result.await.map_err(|_| GraphError::ControllerClosed)?
    })
    .await
    .unwrap();
    assert_eq!(report.summary, OperationSummary::Completed);
    for (id, generation) in members {
        assert!(matches!(report.components[&id], StartOutcome::Started));
        let observed = graph.observed();
        assert_eq!(observed.components[&id].generation, generation);
        assert_eq!(
            observed.components[&id].lifecycle,
            ComponentLifecycle::Running
        );
    }
}

#[tokio::test(flavor = "current_thread")]
async fn created_group_start_waits_for_the_committed_binding_reservation_current_thread() {
    created_group_start_waits_for_the_committed_binding_reservation().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn created_group_start_waits_for_the_committed_binding_reservation_multi_thread() {
    created_group_start_waits_for_the_committed_binding_reservation().await;
}

struct ProgressSource {
    descriptor: ComponentDescriptor,
    progress: Arc<crate::computation::v1::ConsumerProgressSet>,
}

#[async_trait]
impl crate::computation::v1::ComputationComponent for ProgressSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> crate::computation::v1::ComponentRecovery {
        crate::computation::v1::ComponentRecovery::default()
            .with_consumer_progress(self.progress.clone())
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl crate::computation::v1::EnvelopeSource for ProgressSource {
    async fn next(&mut self) -> anyhow::Result<Option<crate::computation::v1::OutputEnvelope>> {
        std::future::pending().await
    }
}

struct ProgressSink {
    descriptor: ComponentDescriptor,
    progress: Arc<crate::computation::v1::QuerySourceProgress>,
}

#[async_trait]
impl crate::computation::v1::ComputationComponent for ProgressSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> crate::computation::v1::ComponentRecovery {
        crate::computation::v1::ComponentRecovery::default()
            .with_volatile_input_progress(self.progress.clone())
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl crate::computation::v1::EnvelopeSink for ProgressSink {
    fn completion(&self) -> crate::computation::v1::SinkCompletion {
        crate::computation::v1::SinkCompletion::Handled
    }
    async fn handle(&mut self, _: crate::computation::v1::InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

async fn deferred_binding_waits_for_the_actual_consumer_cut() {
    use crate::computation::v1::{ConsumerProgressSet, PortId, QuerySourceProgress};
    for blocker in [Some(Operation::Create), Some(Operation::Start), Some(Operation::Stop), None] {
        let graph_id = "deferred-consumer-cut";
        let owners =
            [2, 3].map(|number| Arc::new(QuerySourceProgress::new(graph_id, id(number)).unwrap()));
        let mut builder = ComputationGraph::builder(graph_id).source(
            ComponentName::try_new("source").unwrap(),
            Box::new(ProgressSource {
                descriptor: descriptor(1, true),
                progress: Arc::new(
                    ConsumerProgressSet::new(graph_id, id(1), owners.iter().cloned()).unwrap(),
                ),
            }),
        );
        for owner in &owners {
            builder = builder
                .sink(
                    ComponentName::try_new(format!("sink-{}", owner.component_id())).unwrap(),
                    Box::new(ProgressSink {
                        descriptor: ComponentDescriptor::try_new(
                            owner.component_id().clone(),
                            descriptor(2, false).ports().to_vec(),
                        )
                        .unwrap(),
                        progress: owner.clone(),
                    }),
                )
                .connect(
                    EdgeDefinition::new(
                        Endpoint::new(id(1), PortId::try_new("out").unwrap()),
                        Endpoint::new(owner.component_id().clone(), PortId::try_new("in").unwrap()),
                    ),
                    Box::new(crate::computation::v1::BoundedPipeConfig { capacity: 2 }),
                );
        }
        let mut graph = builder
            .bind_stream(
                Endpoint::new(id(1), PortId::try_new("out").unwrap()),
                crate::computation::v1::StreamId::try_new("source/out").unwrap(),
            )
            .build()
            .unwrap();
        let mut operations = Operations::default();
        let (_cancel, mut cancel) = watch::channel(false);
        let mut controls = deploy(&mut graph, &mut cancel, &mut operations)
            .await
            .unwrap();
        for &index in &graph.order {
            operations.attach_control(&graph, index).unwrap();
        }
        operations.refresh_connections(&graph).unwrap();
        let edges: Vec<_> = graph
            .snapshot
            .edges
            .iter()
            .map(|edge| edge.definition.clone())
            .collect();
        update(&graph, |state| {
            for edge in &edges {
                state.relationships.get_mut(edge).unwrap().binding = BindingState::Declared;
            }
        });
        let sibling = graph.ids[&id(3)];
        let held = graph.components[sibling].take().unwrap();
        let (abort, _) = AbortHandle::new_pair();
        if let Some(operation) = blocker {
            operations.active.insert(
                sibling,
                Active {
                    _admission_activation: None,
                    epoch: OperationEpoch(1),
                    operation,
                    abort,
                    quiesce: watch::channel(false).0,
                    pause_component: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    outputs: Vec::new(),
                    reply: None,
                },
            );
        } else {
            operations
                .pending_creation
                .insert(sibling, Operation::Create);
        }
        let revision = graph.snapshot.revision;
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
        assert_eq!(graph.snapshot.revision, revision);
        assert!(operations.bindings_dirty);
        for edge in &edges {
            let observed = graph.observed();
            assert_eq!(observed.relationships[edge].binding, BindingState::Declared);
            assert!(observed.relationships[edge].failure.is_none());
        }
        operations.active.remove(&sibling);
        operations.pending_creation.remove(&sibling);
        drop(held);
        operations.dependencies_changed();
        bind_ready_dependencies(
            &mut graph,
            &mut operations,
            &mut controls,
            &mut cancel,
            &mut commands,
        )
        .await
        .unwrap();
        for edge in &edges {
            assert_eq!(
                graph.observed().relationships[edge].binding,
                BindingState::Bound
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn deferred_binding_waits_for_the_actual_consumer_cut_current_thread() {
    deferred_binding_waits_for_the_actual_consumer_cut().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_binding_waits_for_the_actual_consumer_cut_multi_thread() {
    deferred_binding_waits_for_the_actual_consumer_cut().await;
}

#[tokio::test]
async fn stale_completion_keeps_the_current_operation_and_controller_alive() {
    let (graph, mut operations, controls, _, _) = fixture().await;
    let index = graph.ids[&id(1)];
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
            pause_component: Arc::new(std::sync::atomic::AtomicBool::new(true)),
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
    let changes = vec![DesiredMutation::Restart(GraphSelection::Exact(vec![id(1)]))];
    let supplied = preview(&graph, changes.clone()).unwrap();
    assert!(supplied.rebind.is_empty());
    update(&graph, |state| {
        state.components.get_mut(&id(1)).unwrap().exhausted = true;
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
            ComponentName::try_new("duplicate").unwrap(),
            ConstructedComponent::source(Box::new(Source(descriptor(5, true)))),
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
    desired.subscriptions.push((id(2), id(1)));
    assert!(desired.validate_structure().is_err());
    assert!(preview(&graph, vec![DesiredMutation::SetTopology(desired)]).is_err());
    for pair in [(id(1), id(1)), (id(6), id(2))] {
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
            connections: vec![(id(2), id(1))],
            subscriptions: false,
            consumer: None,
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
    let source = graph.ids[&id(1)];
    operations.paused.insert(source);
    update(&graph, |state| {
        let source = state.components.get_mut(&id(1)).unwrap();
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

#[derive(Default)]
struct StartupBindingControl {
    initializing: tokio::sync::Notify,
    release: tokio::sync::Notify,
    initializations: std::sync::atomic::AtomicUsize,
    disposals: std::sync::atomic::AtomicUsize,
}

struct StartupBindingSource {
    descriptor: ComponentDescriptor,
    control: Arc<StartupBindingControl>,
}

#[async_trait]
impl crate::computation::v1::ComputationComponent for StartupBindingSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn initialize(&mut self) -> anyhow::Result<()> {
        self.control
            .initializations
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.control.initializing.notify_one();
        self.control.release.notified().await;
        Ok(())
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn dispose(&mut self) -> anyhow::Result<()> {
        self.control
            .disposals
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl crate::computation::v1::EnvelopeSource for StartupBindingSource {
    async fn next(&mut self) -> anyhow::Result<Option<crate::computation::v1::OutputEnvelope>> {
        std::future::pending().await
    }
}

struct StartupBindingSourceFactory {
    descriptor: FactoryDescriptor,
    control: Arc<StartupBindingControl>,
}

impl StartupBindingSourceFactory {
    fn new(control: Arc<StartupBindingControl>) -> Self {
        Self {
            descriptor: FactoryDescriptor {
                implementation: ImplementationIdentity::try_new("test/startup-binding-source", "1")
                    .unwrap(),
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema::default(),
                dependencies: BTreeMap::new(),
            },
            control,
        }
    }

    fn specification(&self) -> ComponentSpecification {
        ComponentSpecification {
            descriptor: descriptor(1, true),
            role: ComponentRole::Source,
            completion: None,
            implementation: self.descriptor.implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        }
    }
}

#[async_trait]
impl ComponentFactory for StartupBindingSourceFactory {
    fn descriptor(&self) -> &FactoryDescriptor {
        &self.descriptor
    }

    fn validate(&self, _: &ComponentSpecification) -> anyhow::Result<()> {
        Ok(())
    }

    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        Ok(ConstructedComponent::source(Box::new(
            StartupBindingSource {
                descriptor: context.specification.descriptor.clone(),
                control: self.control.clone(),
            },
        )))
    }
}

fn startup_binding_batch(
    source: Option<Arc<StartupBindingSourceFactory>>,
) -> crate::computation::v1::ComponentBatch {
    let sink = Arc::new(SinkFactory::new());
    let edge = EdgeDefinition::new(
        Endpoint::new(
            id(1),
            crate::computation::v1::PortId::try_new("out").unwrap(),
        ),
        Endpoint::new(
            id(2),
            crate::computation::v1::PortId::try_new("in").unwrap(),
        ),
    );
    let mut builder = ComputationGraph::builder("startup-binding")
        .component(
            ComponentName::try_new("sink").unwrap(),
            sink.specification(),
            sink,
        )
        .lifecycle_policy(id(2), LifecyclePolicy { auto_start: false });
    let missing_source = source.is_none();
    if let Some(source) = source {
        builder = builder
            .component(
                ComponentName::try_new("source").unwrap(),
                source.specification(),
                source,
            )
            .lifecycle_policy(id(1), LifecyclePolicy { auto_start: false })
            .bind_stream(
                edge.from.clone(),
                crate::computation::v1::StreamId::try_new("source/out").unwrap(),
            )
            .connect(
                edge.clone(),
                Box::new(crate::computation::v1::BoundedPipeConfig { capacity: 2 }),
            );
    }
    let mut batch = builder.build_components().unwrap();
    if missing_source {
        batch
            .definition
            .boundary_relationships
            .push(DesiredRelationship {
                definition: edge,
                policy: Default::default(),
                pipe: DesiredPipe::Bounded { capacity: 2 },
            });
    }
    batch.bindings.deferred_management_validation = true;
    batch.bindings.defer_activation = true;
    batch
}

async fn start_members_joins_committed_construction_and_binding() {
    let state = Arc::new(StartupBindingControl::default());
    let batch = startup_binding_batch(Some(Arc::new(StartupBindingSourceFactory::new(
        state.clone(),
    ))));
    let edge = batch.definition.relationships[0].definition.clone();
    let mut graph = ComputationGraph::builder("startup-binding")
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let scenario = async {
        control.deployment_report().await.unwrap();
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::SetTopology(batch.definition)],
            )
            .await
            .unwrap();
        control
            .reconcile(preview, batch.bindings)
            .await
            .unwrap()
            .ensure_committed()
            .unwrap();
        state.initializing.notified().await;
        let mut observations = control.subscribe_observed();
        observations
            .wait_for(|observed| {
                observed.components[&id(2)].realization == RealizationState::Blocked
            })
            .await
            .unwrap();
        let generation = control.component_handle_by_id(&id(2)).unwrap().generation();
        let start = control.start_members(BTreeMap::from([(id(2), generation)]));
        tokio::pin!(start);
        let mut premature = match futures::poll!(&mut start) {
            std::task::Poll::Ready(result) => Some(result),
            std::task::Poll::Pending => None,
        };
        if premature.is_none() {
            // A later read-only command fences admission without releasing the initializer.
            tokio::select! {
                biased;
                result = &mut start => premature = Some(result),
                result = control.preview(control.desired_snapshot().revision, Vec::new()) => {
                    result.unwrap();
                }
            }
            if premature.is_none() {
                if let std::task::Poll::Ready(result) = futures::poll!(&mut start) {
                    premature = Some(result);
                }
            }
        }
        state.release.notify_one();
        let replied_before_binding = premature.is_some();
        let report = match premature {
            Some(result) => result,
            None => start.await,
        };
        observations
            .wait_for(|observed| {
                !matches!(
                    observed.components[&id(1)].realization,
                    RealizationState::Pending | RealizationState::Creating
                )
            })
            .await
            .unwrap();
        let publication = control.registry_snapshot();
        control.cancel();
        (replied_before_binding, report, publication)
    };
    let (result, (premature, report, publication)) =
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(run, scenario)
        })
        .await
        .expect("startup must settle after releasing the committed initializer");
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(
        state
            .initializations
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(state.disposals.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        !premature,
        "start replied before its declared input could bind: {report:?}"
    );
    assert_eq!(report.unwrap().summary, OperationSummary::Completed);
    assert_eq!(
        publication.observed.relationships[&edge].binding,
        BindingState::Bound
    );
    assert_eq!(
        publication.observed.components[&id(2)].realization,
        RealizationState::Created
    );
    assert!(publication.observed.components[&id(2)].started);
}

#[tokio::test(flavor = "current_thread")]
async fn start_members_joins_committed_construction_and_binding_current_thread() {
    start_members_joins_committed_construction_and_binding().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_members_joins_committed_construction_and_binding_multi_thread() {
    start_members_joins_committed_construction_and_binding().await;
}

async fn start_members_preserves_missing_declaration_waits() {
    let batch = startup_binding_batch(None);
    let mut graph = ComputationGraph::builder("startup-binding")
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let scenario = async {
        control.deployment_report().await.unwrap();
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::SetTopology(batch.definition)],
            )
            .await
            .unwrap();
        control
            .reconcile(preview, batch.bindings)
            .await
            .unwrap()
            .ensure_committed()
            .unwrap();
        let generation = control.component_handle_by_id(&id(2)).unwrap().generation();
        let report = control
            .start_members(BTreeMap::from([(id(2), generation)]))
            .await;
        let publication = control.registry_snapshot();
        control.cancel();
        (report, publication)
    };
    let (result, (report, publication)) =
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            tokio::join!(run, scenario)
        })
        .await
        .expect("a missing declaration must remain an observable wait, not hang startup");
    assert!(matches!(result, Err(GraphError::Cancelled)));
    let report = report.unwrap();
    assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
    assert!(matches!(
        report.components[&id(2)],
        StartOutcome::NotCreated
    ));
    let sink = &publication.observed.components[&id(2)];
    assert_eq!(sink.realization, RealizationState::Blocked);
    assert_eq!(sink.lifecycle, ComponentLifecycle::Stopped);
    assert!(!sink.started);
    assert!(sink.failure.is_none());
    assert!(sink
        .waiting_for
        .contains(&crate::computation::v1::DependencyWait::Component { id: id(1) }));
    assert!(sink
        .waiting_for
        .contains(&crate::computation::v1::DependencyWait::Connection {
            endpoint: Endpoint::new(
                id(2),
                crate::computation::v1::PortId::try_new("in").unwrap()
            ),
        }));
    assert!(!publication.observed.components.contains_key(&id(1)));
}

#[tokio::test(flavor = "current_thread")]
async fn start_members_preserves_missing_declaration_waits_current_thread() {
    start_members_preserves_missing_declaration_waits().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn start_members_preserves_missing_declaration_waits_multi_thread() {
    start_members_preserves_missing_declaration_waits().await;
}

#[derive(Default)]
struct StopReservationControl {
    hold_start: std::sync::atomic::AtomicBool,
    hold_stop: std::sync::atomic::AtomicBool,
    starts: std::sync::atomic::AtomicUsize,
    stops: std::sync::atomic::AtomicUsize,
    completed_stops: std::sync::atomic::AtomicUsize,
    runs: std::sync::atomic::AtomicUsize,
    joined_runs: std::sync::atomic::AtomicUsize,
    start_entered: tokio::sync::Notify,
    stop_entered: tokio::sync::Notify,
    release_start: tokio::sync::Notify,
    release_stop: tokio::sync::Notify,
}

struct StopReservationService {
    descriptor: ComponentDescriptor,
    control: Arc<StopReservationControl>,
}

struct StopReservationRun(Arc<StopReservationControl>);

impl Drop for StopReservationRun {
    fn drop(&mut self) {
        self.0
            .joined_runs
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

#[async_trait]
impl crate::computation::v1::ComputationComponent for StopReservationService {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.control
            .starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .control
            .hold_start
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.control.start_entered.notify_one();
            self.control.release_start.notified().await;
        }
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.control
            .stops
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .control
            .hold_stop
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.control.stop_entered.notify_one();
            self.control.release_stop.notified().await;
        }
        self.control
            .completed_stops
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl crate::computation::v1::ComputationService for StopReservationService {
    async fn run(&mut self) -> anyhow::Result<()> {
        self.control
            .runs
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _owned = StopReservationRun(self.control.clone());
        std::future::pending().await
    }
}

async fn stop_members_joins_real_reconciliation(precommit: bool) {
    use std::sync::atomic::Ordering::SeqCst;

    let target = Arc::new(StopReservationControl::default());
    let sibling = Arc::new(StopReservationControl::default());
    let unrelated = Arc::new(StopReservationControl::default());
    let mut builder = ComputationGraph::builder("stop-reservation-witness");
    for (number, name, state) in [
        (3, "target", target.clone()),
        (4, "sibling", sibling.clone()),
        (5, "unrelated", unrelated.clone()),
    ] {
        builder = builder.service(
            ComponentName::try_new(name).unwrap(),
            Box::new(StopReservationService {
                descriptor: ComponentDescriptor::try_new(id(number), Vec::new()).unwrap(),
                control: state,
            }),
        );
    }
    let mut graph = builder.build().unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let scenario = async {
        control.deployment_report().await.unwrap();
        let members: BTreeMap<_, _> = [3, 4, 5]
            .into_iter()
            .map(|number| {
                let handle = control.component_handle_by_id(&id(number)).unwrap();
                (id(number), handle.generation())
            })
            .collect();
        assert_eq!(
            control
                .start_members(members.clone())
                .await
                .unwrap()
                .summary,
            OperationSummary::Completed
        );
        let target_handle = control.component_handle_by_id(&id(3)).unwrap();
        let unrelated_handle = control.component_handle_by_id(&id(5)).unwrap();
        target_handle.wait_started().await.unwrap();
        unrelated_handle.wait_started().await.unwrap();
        let initial_revision = control.desired_snapshot().revision;
        let restart = if precommit {
            target.hold_stop.store(true, SeqCst);
            vec![id(3)]
        } else {
            sibling.hold_start.store(true, SeqCst);
            vec![id(3), id(4)]
        };
        let preview = control
            .preview(
                initial_revision,
                vec![DesiredMutation::Restart(GraphSelection::Exact(restart))],
            )
            .await
            .unwrap();
        let reconciliation = control.reconcile(preview, TopologyBindings::default());
        tokio::pin!(reconciliation);
        if precommit {
            tokio::select! {
                result = &mut reconciliation => panic!("reconciliation bypassed held stop: {result:?}"),
                _ = target.stop_entered.notified() => {}
            }
        } else {
            tokio::select! {
                result = &mut reconciliation => panic!("reconciliation bypassed held activation: {result:?}"),
                _ = sibling.start_entered.notified() => {}
            }
            let mut registry = control.subscribe_registry();
            loop {
                let running = registry
                    .borrow_and_update()
                    .observed
                    .components
                    .get(&id(3))
                    .is_some_and(|observed| {
                        observed.generation == target_handle.generation()
                            && observed.lifecycle == ComponentLifecycle::Running
                    });
                if running {
                    break;
                }
                registry.changed().await.unwrap();
            }
            target.hold_stop.store(true, SeqCst);
        }
        let held_revision = control.desired_snapshot().revision;
        let held_target = target_handle.observed().unwrap();
        let completed_before_stop = target.completed_stops.load(SeqCst);

        let queued_start = control.start_members(BTreeMap::from([(id(3), members[&id(3)])]));
        tokio::pin!(queued_start);
        let mut early_start = match futures::poll!(&mut queued_start) {
            std::task::Poll::Ready(result) => Some(result),
            std::task::Poll::Pending => None,
        };
        if early_start.is_none() {
            tokio::select! {
                biased;
                result = &mut queued_start => early_start = Some(result),
                result = control.preview(held_revision, Vec::new()) => { result.unwrap(); }
            }
        }

        let stop = control.stop_members(BTreeMap::from([(id(3), members[&id(3)])]));
        tokio::pin!(stop);
        let mut early_stop = match futures::poll!(&mut stop) {
            std::task::Poll::Ready(result) => Some(result),
            std::task::Poll::Pending => None,
        };
        if early_stop.is_none() {
            tokio::select! {
                biased;
                result = &mut stop => early_stop = Some(result),
                result = control.preview(held_revision, Vec::new()) => { result.unwrap(); }
            }
        }
        if early_start.is_none() {
            if let std::task::Poll::Ready(result) = futures::poll!(&mut queued_start) {
                early_start = Some(result);
            }
        }
        let unrelated_stop = control
            .stop_members(BTreeMap::from([(id(5), members[&id(5)])]))
            .await
            .unwrap();
        let unrelated_start = control
            .start_members(BTreeMap::from([(id(5), members[&id(5)])]))
            .await
            .unwrap();
        unrelated_handle.wait_started().await.unwrap();
        if early_stop.is_none() {
            if let std::task::Poll::Ready(result) = futures::poll!(&mut stop) {
                early_stop = Some(result);
            }
        }
        if early_start.is_none() {
            if let std::task::Poll::Ready(result) = futures::poll!(&mut queued_start) {
                early_start = Some(result);
            }
        }
        let stop_was_early = early_stop.is_some();
        let start_superseded_while_reserved = early_start.as_ref().is_some_and(|result| {
            matches!(result, Err(GraphError::StartSuperseded { component }) if component == &id(3))
        });
        let completed_while_reserved = target.completed_stops.load(SeqCst);

        if precommit {
            target.release_stop.notify_one();
        } else {
            sibling.release_start.notify_one();
        }
        let reconciled = reconciliation.await.unwrap();
        let mut completed_before_callback_release = None;
        if !precommit && !stop_was_early {
            tokio::select! {
                result = &mut stop => completed_before_callback_release = Some(result),
                _ = target.stop_entered.notified() => {}
            }
            if completed_before_callback_release.is_none() {
                tokio::select! {
                    biased;
                    result = &mut stop => completed_before_callback_release = Some(result),
                    result = control.preview(control.desired_snapshot().revision, Vec::new()) => {
                        result.unwrap();
                    }
                }
            }
        }
        let stop_completed_before_cleanup = completed_before_callback_release.is_some();
        target.hold_stop.store(false, SeqCst);
        target.release_stop.notify_one();
        let stopped = match early_stop.or(completed_before_callback_release) {
            Some(result) => result,
            None => stop.await,
        };
        let restarted = match early_start {
            Some(result) => result,
            None => queued_start.await,
        };
        let final_target = target_handle.observed().unwrap();
        let starts_before_cancel = target.starts.load(SeqCst);
        let runs_before_cancel = target.runs.load(SeqCst);
        let joined_before_cancel = target.joined_runs.load(SeqCst);
        let completed_after_stop = target.completed_stops.load(SeqCst);
        control.cancel();
        (
            initial_revision,
            held_revision,
            held_target,
            completed_before_stop,
            completed_while_reserved,
            stop_was_early,
            start_superseded_while_reserved,
            unrelated_stop,
            unrelated_start,
            reconciled,
            stop_completed_before_cleanup,
            stopped,
            restarted,
            final_target,
            starts_before_cancel,
            runs_before_cancel,
            joined_before_cancel,
            completed_after_stop,
        )
    };
    let (run_result, evidence) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::join!(run, scenario)
    })
    .await
    .expect("held reconciliation must finish after explicit callback releases");
    let (
        initial_revision,
        held_revision,
        held_target,
        completed_before_stop,
        completed_while_reserved,
        stop_was_early,
        start_superseded_while_reserved,
        unrelated_stop,
        unrelated_start,
        reconciled,
        stop_completed_before_cleanup,
        stopped,
        restarted,
        final_target,
        starts_before_cancel,
        runs_before_cancel,
        joined_before_cancel,
        completed_after_stop,
    ) = evidence;
    eprintln!(
        "E02 stop reservation evidence AFTER callback releases and joined graph shutdown: \
         precommit={precommit}; run_result={run_result:#?}; \
         initial_revision={initial_revision:?}; held_revision={held_revision:?}; \
         held_target={held_target:#?}; final_target={final_target:#?}; \
         completed_before_stop={completed_before_stop}; \
         completed_while_reserved={completed_while_reserved}; \
         completed_after_stop={completed_after_stop}; \
         stop_was_early={stop_was_early}; \
         start_superseded_while_reserved={start_superseded_while_reserved}; \
         stop_completed_before_cleanup={stop_completed_before_cleanup}; \
         starts_before_cancel={starts_before_cancel}; \
         runs_before_cancel={runs_before_cancel}; joined_before_cancel={joined_before_cancel}; \
         unrelated_stop={unrelated_stop:#?}; unrelated_start={unrelated_start:#?}; \
         reconciliation_report={reconciled:#?}; \
         explicit_stop_result={stopped:#?}; superseded_start_result={restarted:#?}"
    );
    assert!(matches!(run_result, Err(GraphError::Cancelled)));
    if precommit {
        assert_eq!(held_revision, initial_revision);
        assert_eq!(completed_before_stop, 0);
    } else {
        assert!(held_revision > initial_revision);
        assert_eq!(
            held_target.lifecycle,
            ComponentLifecycle::Running,
            "target must be running within the committed reservation"
        );
        assert_eq!(completed_before_stop, 1);
    }
    assert_eq!(completed_while_reserved, completed_before_stop);
    assert!(
        !stop_was_early,
        "a current-generation stop must join the real reserved owner, not return Busy or success: {stopped:?}"
    );
    assert!(
        start_superseded_while_reserved,
        "stop admission must supersede the older queued start before the reservation releases: {restarted:?}"
    );
    assert_eq!(unrelated_stop.summary, OperationSummary::Completed);
    assert_eq!(unrelated_start.summary, OperationSummary::Completed);
    assert!(reconciled.committed);
    if precommit {
        assert!(reconciled.creation.is_empty(), "{reconciled:#?}");
        assert!(reconciled.removed.is_empty(), "{reconciled:#?}");
        assert!(reconciled.failures.is_empty(), "{reconciled:#?}");
        let startup = reconciled
            .startup
            .as_ref()
            .expect("the reserved target activation must retain its startup report");
        assert_eq!(startup.revision, reconciled.revision);
        assert_eq!(startup.components.len(), 1, "{startup:#?}");
        match (
            reconciled.summary,
            startup.summary,
            startup.components.get(&id(3)),
        ) {
            (
                OperationSummary::CompletedWithFailures,
                OperationSummary::CompletedWithFailures,
                Some(StartOutcome::StartFailed(failure)),
            ) => {
                assert_eq!(failure.phase, FailurePhase::Activation);
                assert_eq!(failure.disposition, FailureDisposition::Retryable);
                assert!(matches!(failure.cause.as_ref(), GraphError::Cancelled));
            }
            (
                OperationSummary::Completed,
                OperationSummary::Completed,
                Some(StartOutcome::Started),
            ) => {
                assert_eq!(
                    (starts_before_cancel, completed_after_stop),
                    (2, 2),
                    "a completed activation must have its matching awaited stop"
                );
                assert!(
                    final_target.started,
                    "a completed activation must retain its successful-start history"
                );
            }
            _ => panic!("unexpected reserved target activation: {reconciled:#?}"),
        }
    } else {
        assert_eq!(reconciled.summary, OperationSummary::Completed);
    }
    assert!(
        !stop_completed_before_cleanup,
        "stop replied before its actual stop callback completed"
    );
    assert_eq!(stopped.unwrap().summary, OperationSummary::Completed);
    assert!(matches!(
        restarted,
        Err(GraphError::StartSuperseded { component }) if component == id(3)
    ));
    assert_eq!(final_target.lifecycle, ComponentLifecycle::Stopped);
    if !precommit {
        assert!(
            final_target.started,
            "stopping retains the successful-start history of the committed target"
        );
    }
    assert_eq!(
        final_target.generation, held_target.generation,
        "a restart and its queued stop must keep the same actual generation"
    );
    assert_eq!(
        runs_before_cancel, joined_before_cancel,
        "processing ownership must be joined"
    );
    if precommit {
        assert!(
            matches!(
                (starts_before_cancel, completed_after_stop),
                (1, 1) | (2, 2)
            ),
            "stop must join exactly the starts that entered: \
             starts={starts_before_cancel}; completed_stops={completed_after_stop}"
        );
        assert_eq!(
            (runs_before_cancel, joined_before_cancel),
            (1, 1),
            "the superseded activation must not launch a second processing owner"
        );
    } else {
        assert_eq!(
            starts_before_cancel, 2,
            "the superseded queued start must not reactivate"
        );
        assert_eq!(
            completed_after_stop, 2,
            "restart cleanup and explicit stop must both complete"
        );
    }
}

#[tokio::test]
async fn stop_members_joins_precommit_reconciliation_current_thread() {
    stop_members_joins_real_reconciliation(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_members_joins_precommit_reconciliation_multi_thread() {
    stop_members_joins_real_reconciliation(true).await;
}

#[tokio::test]
async fn stop_members_joins_committed_activation_reconciliation_current_thread() {
    stop_members_joins_real_reconciliation(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_members_joins_committed_activation_reconciliation_multi_thread() {
    stop_members_joins_real_reconciliation(false).await;
}

async fn fixture() -> (
    ComputationGraph,
    Operations,
    PipeGuard,
    ReconciliationPreview,
    Prepared,
) {
    let sink_factory = Arc::new(SinkFactory::new());
    let edge = EdgeDefinition::new(
        Endpoint::new(
            id(1),
            crate::computation::v1::PortId::try_new("out").unwrap(),
        ),
        Endpoint::new(
            id(2),
            crate::computation::v1::PortId::try_new("in").unwrap(),
        ),
    );
    let mut graph = ComputationGraph::builder("commit-faults")
        .source(
            ComponentName::try_new("source").unwrap(),
            Box::new(Source(descriptor(1, true))),
        )
        .component(
            ComponentName::try_new("sink").unwrap(),
            sink_factory.specification(),
            sink_factory,
        )
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
        .find(|node| node.descriptor.id() == &id(2))
        .unwrap();
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
    let prepared = prepare(
        &PreparationSnapshot::capture(&graph),
        &plan,
        TopologyBindings::default(),
    )
    .unwrap();
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
        let sink_index = graph.ids[&id(2)];
        let held = (fault == 4).then(|| graph.components[sink_index].take().unwrap());
        let old_slot = graph.components[sink_index].clone();
        let peer = operations.peer_controls[&sink_index].clone();
        if fault == 5 {
            graph.peers.remove(&id(2), old_slot.generation).unwrap();
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
    let old_slot = graph.components[graph.ids[&id(2)]].clone();
    let old_control = operations.peer_controls[&graph.ids[&id(2)]].clone();
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
    assert!(next.observed.components[&id(2)].generation > old_slot.generation);
    assert!(matches!(
        old_control.not_ready(),
        Err(crate::computation::v1::ControlError::StaleComponent { .. })
    ));
    assert!(old_slot.value.lock().unwrap().is_none());
    for index in graph.order.clone() {
        operations.attach_control(&graph, index).unwrap();
    }
    operations.refresh_connections(&graph).unwrap();
    operations.peer_controls[&graph.ids[&id(2)]]
        .not_ready()
        .unwrap();
}

#[tokio::test]
async fn draining_stop_keeps_suspended_start_members_in_their_original_report() {
    let (graph, mut operations, controls, _, _) = fixture().await;
    let source = graph.ids[&id(1)];
    let sink = graph.ids[&id(2)];
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
        report.components[&id(1)],
        StartOutcome::NotRequested
    ));
    assert!(!graph.observed().components[&id(1)].started);
}

#[tokio::test]
async fn resume_collects_all_failures_and_still_restarts_later_members() {
    for failed_ids in [vec![id(2)], vec![id(2), id(1)]] {
        let (graph, mut operations, _controls, _, _) = fixture().await;
        let selected = BTreeSet::from([id(2), id(1)]);
        for id in &selected {
            operations.paused.insert(graph.ids[id]);
        }
        update(&graph, |state| {
            for id in &state.components.keys().cloned().collect::<Vec<_>>() {
                let node = state.components.get_mut(id).expect("observed component");
                node.lifecycle = ComponentLifecycle::Quiesced;
                if failed_ids.contains(id) {
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
                component.clone()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            failures.into_iter().collect::<BTreeSet<_>>(),
            failed_ids.iter().cloned().collect()
        );
        let observed = graph.observed();
        for id in selected {
            let index = graph.ids[&id];
            let node = &observed.components[&id];
            if failed_ids.contains(&id) {
                assert_eq!(node.lifecycle, ComponentLifecycle::Quiesced);
                assert_eq!(node.health, ComponentHealth::Unavailable);
                assert!(operations.paused.contains(&index));
                assert!(!operations.active.contains_key(&index));
            } else {
                assert_eq!(node.lifecycle, ComponentLifecycle::Starting);
                assert!(!operations.paused.contains(&index));
                assert!(operations.active.contains_key(&index));
            }
        }
    }
}

async fn reconciliation_wires_retained_inflight_constructor() {
    let state = Arc::new(StartupBindingControl::default());
    let mut batch = startup_binding_batch(Some(Arc::new(StartupBindingSourceFactory::new(
        state.clone(),
    ))));
    let desired = batch.definition.clone();
    let edge = desired.relationships[0].definition.clone();
    batch.definition.relationships.clear();
    let initial = batch.definition;
    let mut graph = ComputationGraph::builder("startup-binding")
        .build()
        .unwrap();
    let previous = graph.snapshot.select(GraphSelection::All).unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (run_result, result) = tokio::join!(run, async {
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            control.deployment_report().await?;
            control
                .reconcile_components(
                    previous,
                    initial.clone(),
                    batch.bindings,
                    BTreeMap::new(),
                    BTreeSet::new(),
                    BTreeMap::new(),
                )
                .await?
                .ensure_committed()?;
            state.initializing.notified().await;
            let source = control.component_handle_by_id(&id(1))?;
            let sink = control.component_handle_by_id(&id(2))?;
            let mut wiring = Box::pin(control.reconcile_components(
                initial,
                desired,
                TopologyBindings {
                    deferred_management_validation: true,
                    defer_activation: true,
                    ..Default::default()
                },
                BTreeMap::new(),
                BTreeSet::new(),
                BTreeMap::new(),
            ));
            let premature = match futures::poll!(wiring.as_mut()) {
                std::task::Poll::Ready(result) => Some(result),
                std::task::Poll::Pending => None,
            };
            control.retired_resources().await?;
            let mut observed = control.subscribe_observed();
            let premature = match premature {
                Some(result) => Some(result),
                None => tokio::select! {
                    result = &mut wiring => Some(result),
                    declared = observed.wait_for(|state| state.relationships.contains_key(&edge)) => {
                        declared?;
                        None
                    }
                },
            };
            if premature.as_ref().is_some_and(|result| {
                result
                    .as_ref()
                    .is_ok_and(|report| report.committed && report.failures.is_empty())
            }) {
                observed
                    .wait_for(|state| state.relationships.contains_key(&edge))
                    .await?;
            }
            let source_before = source.observed()?;
            let sink_before = sink.observed()?;
            let held_binding = control.observed().relationships.get(&edge).cloned();
            state.release.notify_one();
            let report = match premature {
                Some(result) => result?,
                None => wiring.await?,
            };
            if report.committed && report.failures.is_empty() {
                let mut observed = control.subscribe_observed();
                observed
                    .wait_for(|state| {
                        state.components.values().any(|node| node.failure.is_some())
                            || state.relationships.get(&edge).is_some_and(|relationship| {
                                relationship.failure.is_some()
                                    || relationship.binding == BindingState::Bound
                                        && [&id(1), &id(2)].iter().all(|id| {
                                            state.components.get(*id).is_some_and(|node| {
                                                node.realization == RealizationState::Created
                                            })
                                        })
                            })
                    })
                    .await?;
            }
            let source_after = source.observed()?;
            let sink_after = sink.observed()?;
            let binding = control.observed().relationships.get(&edge).cloned();
            let early_disposals = state.disposals.load(std::sync::atomic::Ordering::SeqCst);
            Ok::<_, anyhow::Error>((
                source_before,
                sink_before,
                report,
                source_after,
                sink_after,
                held_binding,
                binding,
                early_disposals,
            ))
        })
        .await;
        state.release.notify_one();
        control.cancel();
        result
    });
    let (
        source_before,
        sink_before,
        report,
        source_after,
        sink_after,
        held_binding,
        binding,
        early_disposals,
    ) = result
        .expect("retained construction wiring exceeded the existing three-second budget")
        .expect("managed initial wiring witness failed");
    assert!(matches!(run_result, Err(GraphError::Cancelled)));
    assert!(
        report.committed && report.failures.is_empty(),
        "initial managed wiring rejected its retained Creating owner: {report:?}"
    );
    assert!(report.removed.is_empty());
    assert!(report.creation.is_empty());
    assert_eq!(source_before.realization, RealizationState::Creating);
    assert_eq!(source_before.generation, source_after.generation);
    assert_eq!(sink_before.generation, sink_after.generation);
    assert_eq!(source_after.realization, RealizationState::Created);
    assert_eq!(sink_after.realization, RealizationState::Created);
    assert!(source_after.failure.is_none());
    assert!(sink_after.failure.is_none());
    assert!(!source_after.started);
    assert!(!sink_after.started);
    let held_binding = held_binding.expect("initial wiring waited for retained initialization");
    assert_eq!(held_binding.binding, BindingState::Declared);
    assert!(held_binding.failure.is_none());
    let binding = binding.expect("managed wiring did not retain the actual relationship");
    assert_eq!(binding.binding, BindingState::Bound);
    assert!(binding.failure.is_none());
    assert_eq!(early_disposals, 0);
    assert_eq!(
        state
            .initializations
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    assert_eq!(state.disposals.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "current_thread")]
async fn reconciliation_wires_retained_inflight_constructor_current_thread() {
    reconciliation_wires_retained_inflight_constructor().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconciliation_wires_retained_inflight_constructor_multi_thread() {
    reconciliation_wires_retained_inflight_constructor().await;
}
