// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[path = "support/mod.rs"]
mod fixture_support;
use fixture_support::FixtureGraphBuilderExt as _;

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

fn named_component(name: &str) -> ComponentId {
    fixture_support::component_id(match name {
        "source" => 77,
        "sink" => 101,
        "left-source" => 3,
        "left" => 743,
        "right-source" => 5,
        "right" => 465,
        "monitor" => 438,
        other => panic!("missing explicit fixture component ID for {other}"),
    })
}

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
        consumer: named_component(consumer),
        scope,
        guarantees: BTreeSet::from([guarantee]),
    }
}

fn source(
    id: &drasi_core_types::ComponentId,
    contract: ComponentRecovery,
) -> Box<dyn EnvelopeSource> {
    Box::new(Source {
        descriptor: descriptor(id, &[], &["out"]),
        contract,
        calls: Arc::new(Calls::default()),
        activate_as: None,
    })
}

fn sink(id: &drasi_core_types::ComponentId, inputs: &[&str]) -> Box<dyn EnvelopeSink> {
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
    producer: &drasi_core_types::ComponentId,
    consumer: &drasi_core_types::ComponentId,
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
    producer: &drasi_core_types::ComponentId,
    consumer: &drasi_core_types::ComponentId,
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
            .fixture_source(source(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                memory_source(),
            ))
            .fixture_sink(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                &["in"],
            ))
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("source"),
            ),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
        "in",
    )
}

fn independent(builder: ComputationGraphBuilder) -> ComputationGraphBuilder {
    builder
        .fixture_source(source(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b4")
                .expect("fixture UUID"),
            ComponentRecovery::default(),
        ))
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b5")
                .expect("fixture UUID"),
            &["in"],
        ))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b4")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("independent"),
        )
        .connect(
            EdgeDefinition::new(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b4")
                        .expect("fixture UUID"),
                    "out",
                ),
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b5")
                        .expect("fixture UUID"),
                    "in",
                ),
            ),
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
        issue.participant == RecoveryParticipant::Component(component(77))
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
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b6")
                .expect("fixture UUID"),
            &["in"],
        ))
        .connect(
            EdgeDefinition {
                from: endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                to: endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b6")
                        .expect("fixture UUID"),
                    "in",
                ),
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
    assert!(!reliable.participants.contains(&component(438)));
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
            .fixture_source(source(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b7")
                    .expect("fixture UUID"),
                memory_source(),
            ))
            .fixture_source(source(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b8")
                    .expect("fixture UUID"),
                ComponentRecovery::default(),
            ))
            .fixture_sink(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                &["a", "b"],
            ))
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b7")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("known"),
            )
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b8")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("unknown"),
            ),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b7")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
        "a",
    )?;
    let graph = retained(
        builder,
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b8")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
        "b",
    )?
    .build()?;
    let report = graph.recovery_report(&request(
        "sink",
        RecoveryScope::MemoryLifetime,
        RecoveryGuarantee::Replay,
    ))?;
    assert_eq!(
        report.participants,
        BTreeSet::from([component(439), component(440), component(101),])
    );
    assert!(report.issues.iter().any(|issue| {
        issue.participant == RecoveryParticipant::Component(component(440))
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
    desired.subscriptions.push((component(738), component(101)));
    desired.validate_structure()?;
    let restored = DesiredTopology::from_json(&desired.to_json()?)?;
    assert!(restored
        .subscriptions
        .contains(&(component(738), component(101))));
    let mut desired = graph.snapshot().select(GraphSelection::All)?;
    desired.subscriptions.push((component(77), component(101)));
    desired.recovery_requirements.push(request(
        "sink",
        RecoveryScope::MemoryLifetime,
        RecoveryGuarantee::Replay,
    ));
    let mut bindings = TopologyBindings::default();
    let binding_for = |name: &str| {
        let component = desired
            .components
            .iter()
            .find(|component| component.descriptor.id() == &named_component(name))
            .expect("fixture component");
        let ComponentConstruction::External { binding } = &component.construction else {
            panic!("fixture external binding")
        };
        binding.clone()
    };
    bindings.components.insert(
        binding_for("source"),
        ConstructedComponent::source(source(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            memory_source(),
        )),
    );
    bindings.components.insert(
        binding_for("sink"),
        ConstructedComponent::sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            &["in"],
        )),
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
                .fixture_source(source(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    memory_source(),
                ))
                .fixture_sink(sink(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                    &["in"],
                ))
                .bind_stream(
                    endpoint(
                        &drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-00000000004d",
                        )
                        .expect("fixture UUID"),
                        "out",
                    ),
                    stream("source"),
                ),
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
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
            .fixture_source(source(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                memory_source(),
            ))
            .fixture_sink(Box::new(Sink {
                descriptor: descriptor(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                    &["in"],
                    &[],
                ),
                completion: SinkCompletion::Accepted,
            }))
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("source"),
            ),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
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
            &BTreeSet::from([component(77), component(101)])
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
                .set_subscriptions(vec![(component(77), component(101))])
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
            control.observed().components[&component(101)].lifecycle,
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
                .fixture_source(source(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    memory_source(),
                ))
                .fixture_sink(sink(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002e7")
                        .expect("fixture UUID"),
                    &["in"],
                ))
                .fixture_sink(sink(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d1")
                        .expect("fixture UUID"),
                    &["in"],
                ))
                .bind_stream(
                    endpoint(
                        &drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-00000000004d",
                        )
                        .expect("fixture UUID"),
                        "out",
                    ),
                    stream("source"),
                ),
        );
        for consumer in order {
            let consumer_id = named_component(consumer);
            builder = retained(
                builder,
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &consumer_id,
                "in",
            )
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
                    vec![DesiredMutation::Restart(GraphSelection::Exact(vec![named_component(
                        "right",
                    )]))],
                )
                .await
                .expect("restart preview");
            assert_eq!(
                preview.paused(),
                &BTreeSet::from([
                    named_component("source"),
                    named_component("left"),
                    named_component("right")
                ]),
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
        let producer_id = named_component(producer);
        let consumer_id = named_component(consumer);
        builder = retained(
            builder
                .fixture_source(source(&producer_id, memory_source()))
                .fixture_sink(sink(&consumer_id, &["in"]))
                .bind_stream(endpoint(&producer_id, "out"), stream(producer)),
            &producer_id,
            &consumer_id,
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
            &BTreeSet::from([named_component("right-source"), named_component("right")]),
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
            .fixture_source(source(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                memory_source(),
            ))
            .fixture_transformer(Box::new(WakefulTransformer(descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b9")
                    .expect("fixture UUID"),
                &["in"],
                &["out"],
            ))))
            .fixture_sink(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                &["in"],
            ))
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("source"),
            )
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b9")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("timer"),
            ),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b9")
            .expect("fixture UUID"),
        "in",
    )?;
    let graph = retained(
        builder,
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b9")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
        "in",
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
            .fixture_source(source(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                memory_source(),
            ))
            .fixture_sink(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                &["in"],
            ))
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("source"),
            )
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
                EdgeDefinition::new(
                    endpoint(
                        &drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-00000000004d",
                        )
                        .expect("fixture UUID"),
                        "out",
                    ),
                    endpoint(
                        &drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-000000000065",
                        )
                        .expect("fixture UUID"),
                        "in",
                    ),
                ),
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
            (
                drasi_lib::DrasiLib::builder()
                    .with_allow_external_component_ids(true)
                    .with_query_factory(
                        drasi_lib::computation::v1::FactoryRegistry::standard()
                            .get(
                                &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                                    "drasi/continuous-query",
                                    "1",
                                )
                                .expect("reference query identity"),
                            )
                            .expect("reference query factory")
                            .clone(),
                    )
                    .with_components(batch),
                None,
            )
        } else {
            (
                drasi_lib::DrasiLib::builder()
                    .with_allow_external_component_ids(true)
                    .with_query_factory(
                        drasi_lib::computation::v1::FactoryRegistry::standard()
                            .get(
                                &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                                    "drasi/continuous-query",
                                    "1",
                                )
                                .expect("reference query identity"),
                            )
                            .expect("reference query factory")
                            .clone(),
                    ),
                Some(batch),
            )
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
    fn plan_update(
        &self,
        current: &ComponentSpecification,
        proposed: &ComponentSpecification,
    ) -> anyhow::Result<ComponentUpdatePlan> {
        anyhow::ensure!(
            current.descriptor == proposed.descriptor
                && current.implementation == proposed.implementation
                && current.dependencies == proposed.dependencies,
            "the stateless recovery probe cannot change identity, implementation or resources"
        );
        Ok(ComponentUpdatePlan {
            mode: ComponentUpdateMode::Recreate,
            data: ComponentUpdateData::Preserve,
        })
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let supported = context
            .configuration()
            .get("supported")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| {
                ComponentCreationError::terminal(anyhow::anyhow!("missing supported setting"))
            })?;
        Ok(ConstructedComponent::source(Box::new(Source {
            descriptor: context.specification.descriptor.clone(),
            contract: if supported {
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
            descriptor: descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &[],
                &["out"],
            ),
            role: ComponentRole::Source,
            completion: None,
            implementation: implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([(
                Arc::from("supported"),
                ConfigurationValue::Literal(supported.into()),
            )]),
            dependencies: BTreeMap::new(),
        },
        Arc::new(SourceFactory {
            descriptor: FactoryDescriptor {
                implementation,
                role: ComponentRole::Source,
                configuration_version: 1,
                configuration: ConfigurationSchema {
                    fields: BTreeMap::from([(
                        Arc::from("supported"),
                        ConfigurationField {
                            value_type: ConfigurationType::Boolean,
                            required: true,
                            secret: false,
                            constraint: None,
                        },
                    )]),
                    allow_additional: false,
                },
                dependencies: BTreeMap::new(),
            },
            calls,
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
                    .component(fixture_support::component_name(), specification, factory)
                    .fixture_sink(sink(
                        &drasi_core_types::ComponentId::try_new(
                            "00000000-0000-0000-0000-000000000065",
                        )
                        .expect("fixture UUID"),
                        &["in"],
                    ))
                    .bind_stream(
                        endpoint(
                            &drasi_core_types::ComponentId::try_new(
                                "00000000-0000-0000-0000-00000000004d",
                            )
                            .expect("fixture UUID"),
                            "out",
                        ),
                        stream("source"),
                    ),
            ),
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
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
                control.observed().components[&component(436)].lifecycle,
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
            .fixture_source(Box::new(Source {
                descriptor: descriptor(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &[],
                    &["out"],
                ),
                contract: memory_source(),
                calls: calls.clone(),
                activate_as: Some(ComponentRecovery::default()),
            }))
            .fixture_sink(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                &["in"],
            ))
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("source"),
            ),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
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
        let StartOutcome::StartFailed(failure) = &report.components[&component(77)] else {
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
    let calls = Arc::new(Calls::default());
    let (specification, factory) = deferred_source(true, calls.clone());
    let initial = ComputationGraph::builder("recovery-contracts")
        .component(
            ComponentName::try_new("source").unwrap(),
            specification,
            factory,
        )
        .fixture_sink(sink(&named_component("sink"), &["in"]))
        .bind_stream(
            endpoint(&named_component("source"), "out"),
            stream("source"),
        );
    let mut graph = independent(
        retained(
            initial,
            &named_component("source"),
            &named_component("sink"),
            "in",
        )
        .expect("builder"),
    )
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
        let (specification, _) = deferred_source(false, calls.clone());
        desired
            .components
            .iter_mut()
            .find(|node| node.descriptor.id() == &component(77))
            .expect("source")
            .construction = ComponentConstruction::Factory(specification);
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::SetTopology(desired)],
            )
            .await
            .expect("preview");
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .expect("committed failed definition");
        assert!(report.committed);
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        assert!(report
            .failures
            .iter()
            .any(|failure| matches!(failure.cause.as_ref(), GraphError::Recovery(_))));
        assert_eq!(calls.starts.load(Ordering::SeqCst), 1);
        assert!(calls.reads.load(Ordering::SeqCst) <= 1);
        assert!(!control.observed().components[&named_component("source")].started);
        assert!(!control
            .recovery_report(requirement)
            .await
            .expect("failure inspection")
            .satisfied());
        assert_eq!(
            control.observed().components[&component(436)].lifecycle,
            ComponentLifecycle::Running
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}
