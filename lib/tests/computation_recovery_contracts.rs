// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[allow(dead_code)]
mod computation_support;

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use computation_support::{component, descriptor, endpoint, stream};
use drasi_core::interface::{FailureMode, StorageDurability};
use drasi_lib::computation::v1::*;

struct Source {
    descriptor: ComponentDescriptor,
    contract: ComponentRecovery,
    calls: Arc<Calls>,
    activate_as: Option<ComponentRecovery>,
}

#[derive(Default)]
struct Calls {
    starts: AtomicUsize,
    reads: AtomicUsize,
}

#[async_trait]
impl ComputationComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        self.contract.clone()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.calls.starts.fetch_add(1, Ordering::SeqCst);
        if let Some(contract) = self.activate_as.take() {
            self.contract = contract;
        }
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        self.calls.reads.fetch_add(1, Ordering::SeqCst);
        std::future::pending().await
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    completion: SinkCompletion,
}

#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        self.completion
    }
    async fn handle(&mut self, _input: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

fn request(
    consumer: &str,
    scope: RecoveryScope,
    guarantee: RecoveryGuarantee,
) -> RecoveryRequirement {
    RecoveryRequirement {
        consumer: component(consumer),
        scope,
        guarantees: BTreeSet::from([guarantee]),
    }
}

fn source(id: &str, contract: ComponentRecovery) -> Box<dyn EnvelopeSource> {
    Box::new(Source {
        descriptor: descriptor(id, &[], &["out"]),
        contract,
        calls: Arc::new(Calls::default()),
        activate_as: None,
    })
}

fn sink(id: &str, inputs: &[&str]) -> Box<dyn EnvelopeSink> {
    Box::new(Sink {
        descriptor: descriptor(id, inputs, &[]),
        completion: SinkCompletion::Handled,
    })
}

fn memory_source() -> ComponentRecovery {
    ComponentRecovery::admitted(StorageDurability::VOLATILE).replay_to_durable_delivery()
}

fn retained(
    builder: ComputationGraphBuilder,
    producer: &str,
    consumer: &str,
    port: &str,
) -> GraphResult<ComputationGraphBuilder> {
    retained_policy(
        builder,
        producer,
        consumer,
        port,
        RetentionPolicy::Backpressure,
        ReplayGapPolicy::Strict,
    )
}

fn retained_policy(
    builder: ComputationGraphBuilder,
    producer: &str,
    consumer: &str,
    port: &str,
    policy: RetentionPolicy,
    gaps: ReplayGapPolicy,
) -> GraphResult<ComputationGraphBuilder> {
    let resource = ResourceId::try_new(format!("{producer}-{consumer}"))?;
    let capacity = NonZeroUsize::new(2).expect("capacity");
    let store = Arc::new(RetainedStoreResource(Arc::new(MemoryEnvelopeStore::new(
        capacity, policy,
    ))));
    Ok(builder
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: resource.to_string().into(),
        })?
        .provide_resource(
            resource.clone(),
            ResourceHandle::new(ResourceRole::StateStore, store),
        )?
        .connect(
            EdgeDefinition {
                from: endpoint(producer, "out"),
                to: endpoint(consumer, port),
            },
            Box::new(RetainedPipeConfig {
                resource,
                capacity,
                durable: false,
                retention: policy,
                gap_policy: gaps,
            }),
        ))
}

fn simple() -> GraphResult<ComputationGraphBuilder> {
    retained(
        ComputationGraph::builder("recovery-contracts")
            .source(source("source", memory_source()))
            .sink(sink("sink", &["in"]))
            .bind_stream(endpoint("source", "out"), stream("source")),
        "source",
        "sink",
        "in",
    )
}

fn independent(builder: ComputationGraphBuilder) -> ComputationGraphBuilder {
    builder
        .source(source("independent", ComponentRecovery::default()))
        .sink(sink("observer", &["in"]))
        .bind_stream(endpoint("independent", "out"), stream("independent"))
        .connect(
            EdgeDefinition::new(endpoint("independent", "out"), endpoint("observer", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
}

#[test]
fn memory_replay_and_process_restart_are_different_requirements() -> GraphResult<()> {
    let requirement = request(
        "sink",
        RecoveryScope::MemoryLifetime,
        RecoveryGuarantee::Replay,
    );
    let graph = simple()?.require_recovery(requirement.clone()).build()?;
    assert!(graph.recovery_report(&requirement)?.satisfied());
    let durable = request(
        "sink",
        RecoveryScope::Failure(FailureMode::ProcessRestart),
        RecoveryGuarantee::Replay,
    );
    let report = graph.recovery_report(&durable)?;
    assert!(!report.satisfied());
    assert!(report.issues.iter().any(|issue| {
        issue.participant == RecoveryParticipant::Component(component("source"))
            && matches!(
                issue.reason,
                RecoveryIncompatibility::StorageSurvival { .. }
            )
    }));
    assert!(matches!(
        simple()?.require_recovery(durable).build(),
        Err(GraphError::Recovery(_))
    ));
    Ok(())
}

#[test]
fn an_unrelated_lossy_monitor_does_not_weaken_the_required_consumer() -> GraphResult<()> {
    let graph = simple()?
        .sink(sink("monitor", &["in"]))
        .connect(
            EdgeDefinition {
                from: endpoint("source", "out"),
                to: endpoint("monitor", "in"),
            },
            Box::new(BroadcastPipeConfig {
                capacity: 2,
                lag_policy: BroadcastLagPolicy::Report,
            }),
        )
        .build()?;
    let reliable = graph.recovery_report(&request(
        "sink",
        RecoveryScope::MemoryLifetime,
        RecoveryGuarantee::Replay,
    ))?;
    assert!(reliable.satisfied());
    assert!(!reliable.participants.contains(&component("monitor")));
    let monitor = graph.recovery_report(&request(
        "monitor",
        RecoveryScope::MemoryLifetime,
        RecoveryGuarantee::Replay,
    ))?;
    assert!(monitor
        .issues
        .iter()
        .any(|issue| issue.reason == RecoveryIncompatibility::LossyDelivery));
    Ok(())
}

#[test]
fn fan_in_includes_every_contributing_producer() -> GraphResult<()> {
    let builder = retained(
        ComputationGraph::builder("fan-in")
            .source(source("known", memory_source()))
            .source(source("unknown", ComponentRecovery::default()))
            .sink(sink("sink", &["a", "b"]))
            .bind_stream(endpoint("known", "out"), stream("known"))
            .bind_stream(endpoint("unknown", "out"), stream("unknown")),
        "known",
        "sink",
        "a",
    )?;
    let graph = retained(builder, "unknown", "sink", "b")?.build()?;
    let report = graph.recovery_report(&request(
        "sink",
        RecoveryScope::MemoryLifetime,
        RecoveryGuarantee::Replay,
    ))?;
    assert_eq!(
        report.participants,
        BTreeSet::from([component("known"), component("unknown"), component("sink"),])
    );
    assert!(report.issues.iter().any(|issue| {
        issue.participant == RecoveryParticipant::Component(component("unknown"))
            && issue.reason == RecoveryIncompatibility::UnknownAdmission
    }));
    Ok(())
}

#[test]
fn source_acceptance_is_not_processing_or_external_effect_completion() -> GraphResult<()> {
    let graph = simple()?.build()?;
    for guarantee in [
        RecoveryGuarantee::Replay,
        RecoveryGuarantee::CommittedProcessing,
        RecoveryGuarantee::Publication,
        RecoveryGuarantee::ExternalEffects,
    ] {
        assert!(!graph
            .recovery_report(&request("source", RecoveryScope::MemoryLifetime, guarantee,))?
            .satisfied());
    }
    assert!(graph
        .recovery_report(&request(
            "source",
            RecoveryScope::MemoryLifetime,
            RecoveryGuarantee::Acceptance,
        ))?
        .satisfied());
    assert!(!graph
        .recovery_report(&request(
            "sink",
            RecoveryScope::MemoryLifetime,
            RecoveryGuarantee::ExternalEffects,
        ))?
        .satisfied());
    Ok(())
}

#[test]
fn host_subscriptions_and_missing_dependencies_are_not_omitted() -> GraphResult<()> {
    let graph = simple()?.build()?;
    let mut desired = graph.snapshot().select(GraphSelection::All)?;
    desired
        .subscriptions
        .push((component("missing"), component("sink")));
    assert!(desired.validate_structure().is_err());
    let mut desired = graph.snapshot().select(GraphSelection::All)?;
    desired
        .subscriptions
        .push((component("source"), component("sink")));
    desired.recovery_requirements.push(request(
        "sink",
        RecoveryScope::MemoryLifetime,
        RecoveryGuarantee::Replay,
    ));
    let mut bindings = TopologyBindings::default();
    bindings.components.insert(
        "source".into(),
        ConstructedComponent::source(source("source", memory_source())),
    );
    bindings.components.insert(
        "sink".into(),
        ConstructedComponent::sink(sink("sink", &["in"])),
    );
    let resource = desired.resources[0].id.clone();
    bindings.resources.insert(
        resource,
        ResourceHandle::new(
            ResourceRole::StateStore,
            Arc::new(RetainedStoreResource(Arc::new(MemoryEnvelopeStore::new(
                NonZeroUsize::new(2).expect("capacity"),
                RetentionPolicy::Backpressure,
            )))),
        ),
    );
    let Err(GraphError::Recovery(error)) = desired.build(bindings) else {
        panic!("ordinary subscription without a recovery contract must fail");
    };
    assert!(error
        .issues
        .iter()
        .any(|issue| issue.reason == RecoveryIncompatibility::UnknownSubscription));
    Ok(())
}

#[test]
fn requirements_round_trip_without_changing_fast_topology_json() -> GraphResult<()> {
    let graph = simple()?.build()?;
    let mut desired = graph.snapshot().select(GraphSelection::All)?;
    let fast = desired.to_json()?;
    assert!(!fast.contains("recovery_requirements"));
    desired.recovery_requirements.push(request(
        "sink",
        RecoveryScope::MemoryLifetime,
        RecoveryGuarantee::Replay,
    ));
    assert_eq!(DesiredTopology::from_json(&desired.to_json()?)?, desired);
    desired.recovery_requirements[0].guarantees.clear();
    assert!(desired.validate_structure().is_err());
    Ok(())
}

#[test]
fn lossy_retention_and_gap_skipping_cannot_satisfy_replay() -> GraphResult<()> {
    for gaps in [ReplayGapPolicy::Strict, ReplayGapPolicy::SkipWithNotification] {
        let graph = retained_policy(
            ComputationGraph::builder("lossy-recovery")
                .source(source("source", memory_source()))
                .sink(sink("sink", &["in"]))
                .bind_stream(endpoint("source", "out"), stream("source")),
            "source",
            "sink",
            "in",
            RetentionPolicy::PruneOldest,
            gaps,
        )?
        .build()?;
        let report = graph.recovery_report(&request(
            "sink",
            RecoveryScope::MemoryLifetime,
            RecoveryGuarantee::Replay,
        ))?;
        assert!(report
            .issues
            .iter()
            .any(|issue| issue.reason == RecoveryIncompatibility::LossyDelivery));
    }
    Ok(())
}

#[test]
fn a_retained_pipe_does_not_upgrade_an_acceptance_only_sink() -> GraphResult<()> {
    let result = retained(
        ComputationGraph::builder("acceptance-only")
            .source(source("source", memory_source()))
            .sink(Box::new(Sink {
                descriptor: descriptor("sink", &["in"], &[]),
                completion: SinkCompletion::Accepted,
            }))
            .bind_stream(endpoint("source", "out"), stream("source")),
        "source",
        "sink",
        "in",
    )?
    .build();
    assert!(matches!(
        result,
        Err(GraphError::Contract(
            ContractError::InsufficientSinkCompletion {
                actual: SinkCompletion::Accepted,
                required: PipeCapability::ExplicitAcknowledgement,
            }
        ))
    ));
    Ok(())
}

async fn live_changes() {
    let mut graph = simple().expect("builder").build().expect("graph");
    let run = graph.start().expect("controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        assert_eq!(
            control.startup_report().await.expect("startup").summary,
            OperationSummary::Completed,
        );
        let requirement = request(
            "sink",
            RecoveryScope::MemoryLifetime,
            RecoveryGuarantee::Replay,
        );
        let mut desired = control
            .desired_snapshot()
            .select(GraphSelection::All)
            .expect("desired");
        desired.recovery_requirements.push(requirement.clone());
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::SetTopology(desired.clone())],
            )
            .await
            .expect("preview assertion-only change");
        assert_eq!(
            preview.paused(),
            &BTreeSet::from([component("source"), component("sink")])
        );
        let reconciled = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("reconcile");
        assert_eq!(reconciled.summary, OperationSummary::Completed);
        let report = control
            .recovery_report(requirement.clone())
            .await
            .expect("live report");
        assert!(report.satisfied());
        assert_eq!(report.revision, reconciled.revision);
        assert_eq!(report.graph_id.as_ref(), "recovery-contracts");

        assert!(matches!(
            control
                .set_subscriptions(vec![(component("source"), component("sink"))])
                .await,
            Err(GraphError::Recovery(_))
        ));
        assert!(control.desired_snapshot().subscriptions.is_empty());
        assert_eq!(control.desired_snapshot().revision, reconciled.revision);

        desired.recovery_requirements[0]
            .guarantees
            .insert(RecoveryGuarantee::CommittedProcessing);
        let stronger = control
            .preview(
                reconciled.revision,
                vec![DesiredMutation::SetTopology(desired.clone())],
            )
            .await
            .expect("strengthening is not a downgrade");
        assert!(matches!(
            control
                .reconcile(stronger, TopologyBindings::default())
                .await,
            Err(GraphError::Recovery(_))
        ));
        assert!(control
            .recovery_report(requirement)
            .await
            .expect("original contract")
            .satisfied());
        assert_eq!(
            control.observed().components[&component("sink")].lifecycle,
            ComponentLifecycle::Running
        );
        desired.recovery_requirements.clear();
        assert!(
            control
                .preview(
                    reconciled.revision,
                    vec![DesiredMutation::SetTopology(desired)],
                )
                .await
                .is_err(),
            "assertions cannot silently disappear"
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test]
async fn overlapping_assertions_pause_their_whole_closure_regardless_of_order() {
    for order in [["left", "right"], ["right", "left"]] {
        let mut builder = independent(
            ComputationGraph::builder("overlapping-recovery")
                .source(source("source", memory_source()))
                .sink(sink("left", &["in"]))
                .sink(sink("right", &["in"]))
                .bind_stream(endpoint("source", "out"), stream("source")),
        );
        for consumer in order {
            builder = retained(builder, "source", consumer, "in")
                .expect("retained branch")
                .require_recovery(request(
                    consumer,
                    RecoveryScope::MemoryLifetime,
                    RecoveryGuarantee::Replay,
                ));
        }
        let mut graph = builder.build().expect("graph");
        let run = graph.start().expect("controller");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            assert_eq!(
                control.startup_report().await.expect("startup").summary,
                OperationSummary::Completed,
            );
            let preview = control
                .preview(
                    control.desired_snapshot().revision,
                    vec![DesiredMutation::Restart(GraphSelection::Exact(vec![component("right")]))],
                )
                .await
                .expect("restart preview");
            assert_eq!(
                preview.paused(),
                &BTreeSet::from([component("source"), component("left"), component("right")]),
            );
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
    }
}

#[tokio::test]
async fn adding_an_assertion_does_not_pause_an_independent_asserted_path() {
    let mut builder = ComputationGraph::builder("independent-assertions");
    for (producer, consumer) in [("left-source", "left"), ("right-source", "right")] {
        builder = retained(
            builder
                .source(source(producer, memory_source()))
                .sink(sink(consumer, &["in"]))
                .bind_stream(endpoint(producer, "out"), stream(producer)),
            producer,
            consumer,
            "in",
        )
        .expect("retained branch");
    }
    let mut graph = builder
        .require_recovery(request(
            "left",
            RecoveryScope::MemoryLifetime,
            RecoveryGuarantee::Replay,
        ))
        .build()
        .expect("graph");
    let run = graph.start().expect("controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.startup_report().await.expect("startup");
        let mut desired = control
            .desired_snapshot()
            .select(GraphSelection::All)
            .expect("topology");
        desired.recovery_requirements.push(request(
            "right",
            RecoveryScope::MemoryLifetime,
            RecoveryGuarantee::Replay,
        ));
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::SetTopology(desired)],
            )
            .await
            .expect("assertion preview");
        assert_eq!(
            preview.paused(),
            &BTreeSet::from([component("right-source"), component("right")]),
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test(flavor = "current_thread")]
async fn live_assertion_changes_current_thread() {
    tokio::time::timeout(std::time::Duration::from_secs(10), live_changes())
        .await
        .expect("recovery reconciliation deadlocked");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_assertion_changes_multi_thread() {
    tokio::time::timeout(std::time::Duration::from_secs(10), live_changes())
        .await
        .expect("recovery reconciliation deadlocked");
}

struct SourceFactory {
    descriptor: FactoryDescriptor,
    calls: Arc<Calls>,
    supported: bool,
}

struct Timer;

#[async_trait]
impl WakeupSource for Timer {
    async fn wait(&self) -> anyhow::Result<()> {
        std::future::pending().await
    }
    async fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(false)
    }
}

struct WakefulTransformer(ComponentDescriptor);

#[async_trait]
impl ComputationComponent for WakefulTransformer {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        ComponentRecovery::stateless()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl Transformer for WakefulTransformer {
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        Some(Arc::new(Timer))
    }
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(vec![])
    }
}

#[test]
fn a_timer_cannot_hide_behind_a_stateless_declaration() -> GraphResult<()> {
    let builder = retained(
        ComputationGraph::builder("wakeful")
            .source(source("source", memory_source()))
            .transformer(Box::new(WakefulTransformer(descriptor(
                "timer",
                &["in"],
                &["out"],
            ))))
            .sink(sink("sink", &["in"]))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .bind_stream(endpoint("timer", "out"), stream("timer")),
        "source",
        "timer",
        "in",
    )?;
    let graph = retained(builder, "timer", "sink", "in")?.build()?;
    let report = graph.recovery_report(&request(
        "sink",
        RecoveryScope::MemoryLifetime,
        RecoveryGuarantee::Replay,
    ))?;
    assert!(report
        .issues
        .iter()
        .any(|issue| issue.reason == RecoveryIncompatibility::UntrackedWakeupState));
    Ok(())
}

async fn instance_batches() {
    for initial in [false, true] {
        let requirement = request(
            "sink",
            RecoveryScope::MemoryLifetime,
            RecoveryGuarantee::Replay,
        );
        let capacity = NonZeroUsize::new(2).expect("capacity");
        let resource = ResourceId::try_new("journal").expect("resource");
        let store = Arc::new(RetainedStoreResource(Arc::new(MemoryEnvelopeStore::new(
            capacity,
            RetentionPolicy::Backpressure,
        ))));
        let batch = ComponentBatch::builder()
            .source(source("source", memory_source()))
            .sink(sink("sink", &["in"]))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "journal".into(),
            })
            .expect("resource declaration")
            .provide_resource(
                resource.clone(),
                ResourceHandle::new(ResourceRole::StateStore, store),
            )
            .expect("resource")
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("sink", "in")),
                Box::new(RetainedPipeConfig {
                    resource,
                    capacity,
                    durable: false,
                    retention: RetentionPolicy::Backpressure,
                    gap_policy: ReplayGapPolicy::Strict,
                }),
            )
            .require_recovery(requirement.clone())
            .build()
            .expect("batch");
        let (builder, later) = if initial {
            (drasi_lib::DrasiLib::builder().with_components(batch), None)
        } else {
            (drasi_lib::DrasiLib::builder(), Some(batch))
        };
        let drasi = builder.build().await.expect("instance");
        if let Some(batch) = later {
            drasi
                .add_components(batch)
                .await
                .expect("ordinary addition");
        }
        drasi.start().await.expect("start");
        let control = drasi.computation_control().expect("single controller");
        assert!(control
            .recovery_report(requirement.clone())
            .await
            .expect("instance report")
            .satisfied());
        assert!(control
            .desired_snapshot()
            .recovery_requirements
            .contains(&requirement));
        drasi.shutdown().await.expect("shutdown");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn assertions_survive_instance_batches_current_thread() {
    tokio::time::timeout(std::time::Duration::from_secs(10), instance_batches())
        .await
        .expect("instance recovery path stalled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assertions_survive_instance_batches_multi_thread() {
    tokio::time::timeout(std::time::Duration::from_secs(10), instance_batches())
        .await
        .expect("instance recovery path stalled");
}

#[async_trait]
impl ComponentFactory for SourceFactory {
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
        Ok(ConstructedComponent::source(Box::new(Source {
            descriptor: context.specification.descriptor.clone(),
            contract: if self.supported {
                memory_source()
            } else {
                ComponentRecovery::default()
            },
            calls: self.calls.clone(),
            activate_as: None,
        })))
    }
}

fn deferred_source(
    supported: bool,
    calls: Arc<Calls>,
) -> (ComponentSpecification, Arc<SourceFactory>) {
    let implementation =
        ImplementationIdentity::try_new("test/recovery-source", "1").expect("implementation");
    (
        ComponentSpecification {
            descriptor: descriptor("source", &[], &["out"]),
            role: ComponentRole::Source,
            completion: None,
            implementation: implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::new(),
            dependencies: BTreeMap::new(),
        },
        Arc::new(SourceFactory {
            descriptor: FactoryDescriptor {
                implementation,
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema::default(),
                dependencies: BTreeMap::new(),
            },
            calls,
            supported,
        }),
    )
}

async fn deferred_activation() {
    for supported in [false, true] {
        let calls = Arc::new(Calls::default());
        let (specification, factory) = deferred_source(supported, calls.clone());
        let requirement = request(
            "sink",
            RecoveryScope::MemoryLifetime,
            RecoveryGuarantee::Replay,
        );
        let mut graph = retained(
            independent(
                ComputationGraph::builder("deferred-recovery")
                    .component(specification, factory)
                    .sink(sink("sink", &["in"]))
                    .bind_stream(endpoint("source", "out"), stream("source")),
            ),
            "source",
            "sink",
            "in",
        )
        .expect("builder")
        .require_recovery(requirement.clone())
        .build()
        .expect("deferred graph");
        let run = graph.start().expect("controller");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            let report = control.startup_report().await.expect("activation report");
            assert_eq!(
                report.summary,
                if supported {
                    OperationSummary::Completed
                } else {
                    OperationSummary::CompletedWithFailures
                }
            );
            assert_eq!(calls.starts.load(Ordering::SeqCst), usize::from(supported));
            if !supported {
                assert_eq!(calls.reads.load(Ordering::SeqCst), 0);
            }
            assert_eq!(
                control
                    .recovery_report(requirement)
                    .await
                    .expect("constructed report")
                    .satisfied(),
                supported
            );
            assert_eq!(
                control.observed().components[&component("independent")].lifecycle,
                ComponentLifecycle::Running
            );
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
    }
}

#[tokio::test(flavor = "current_thread")]
async fn deferred_contract_checked_before_activation_current_thread() {
    tokio::time::timeout(std::time::Duration::from_secs(10), deferred_activation())
        .await
        .expect("deferred validation stalled");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deferred_contract_checked_before_activation_multi_thread() {
    tokio::time::timeout(std::time::Duration::from_secs(10), deferred_activation())
        .await
        .expect("deferred validation stalled");
}

#[tokio::test]
async fn activation_cannot_change_a_validated_contract_before_processing() {
    let calls = Arc::new(Calls::default());
    let requirement = request(
        "sink",
        RecoveryScope::MemoryLifetime,
        RecoveryGuarantee::Replay,
    );
    let mut graph = retained(
        ComputationGraph::builder("changing-contract")
            .source(Box::new(Source {
                descriptor: descriptor("source", &[], &["out"]),
                contract: memory_source(),
                calls: calls.clone(),
                activate_as: Some(ComponentRecovery::default()),
            }))
            .sink(sink("sink", &["in"]))
            .bind_stream(endpoint("source", "out"), stream("source")),
        "source",
        "sink",
        "in",
    )
    .expect("builder")
    .require_recovery(requirement)
    .build()
    .expect("valid initial contract");
    let run = graph.start().expect("controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let report = control.startup_report().await.expect("startup");
        let StartOutcome::StartFailed(failure) = &report.components[&component("source")] else {
            panic!("changed contract must fail activation");
        };
        let GraphError::Recovery(error) = failure.cause.as_ref() else {
            panic!("typed recovery failure");
        };
        assert_eq!(
            error.issues[0].reason,
            RecoveryIncompatibility::ContractChangedDuringActivation
        );
        assert_eq!(calls.starts.load(Ordering::SeqCst), 1);
        assert_eq!(calls.reads.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test]
async fn replacement_checks_actual_contract_before_resuming_existing_consumers() {
    let requirement = request(
        "sink",
        RecoveryScope::MemoryLifetime,
        RecoveryGuarantee::Replay,
    );
    let mut graph = independent(simple().expect("builder"))
        .require_recovery(requirement.clone())
        .build()
        .expect("graph");
    let run = graph.start().expect("controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.startup_report().await.expect("startup");
        let mut desired = control
            .desired_snapshot()
            .select(GraphSelection::All)
            .expect("desired");
        let calls = Arc::new(Calls::default());
        let (specification, factory) = deferred_source(false, calls.clone());
        desired
            .components
            .iter_mut()
            .find(|node| node.descriptor.id() == &component("source"))
            .expect("source")
            .construction = ComponentConstruction::Factory(specification);
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::SetTopology(desired)],
            )
            .await
            .expect("preview");
        let mut bindings = TopologyBindings::default();
        bindings.factories.register(factory).expect("factory");
        let report = control
            .reconcile(preview, bindings)
            .await
            .expect("committed failed definition");
        assert!(report.committed);
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        assert!(report
            .failures
            .iter()
            .any(|failure| matches!(failure.cause.as_ref(), GraphError::Recovery(_))));
        assert_eq!(calls.starts.load(Ordering::SeqCst), 0);
        assert_eq!(calls.reads.load(Ordering::SeqCst), 0);
        assert!(!control
            .recovery_report(requirement)
            .await
            .expect("failure inspection")
            .satisfied());
        assert_eq!(
            control.observed().components[&component("independent")].lifecycle,
            ComponentLifecycle::Running
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}
