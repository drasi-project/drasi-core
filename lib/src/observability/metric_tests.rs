// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{ComponentId, ComponentRole, ComponentSemanticKind};
use drasi_core::computation::InMemoryComputationProvider;
use std::{sync::Arc, time::Duration};
use tracing::instrument::WithSubscriber;

#[path = "../../tests/computation_support/metrics.rs"]
mod recording;
use recording::RecordingMetrics;

fn owner(
    metrics: &RecordingMetrics,
    profile: TelemetryProfile,
    role: ComponentRole,
    kind: ComponentSemanticKind,
) -> Arc<ComponentObservations> {
    let owner = TelemetryOptions::new(TelemetrySettings::new(profile).with_tracing(false))
        .with_instance_id("metrics-tests")
        .with_metric_context(
            MetricRegistrationContext::new(
                Arc::new(metrics.clone()),
                RegistrationLimits::default(),
            )
            .unwrap(),
        )
        .resolve("graph", None)
        .unwrap()
        .observations(ComponentId::try_new("component").unwrap(), role, kind, 1)
        .unwrap();
    if kind == ComponentSemanticKind::Query {
        owner.bind_query_metrics().unwrap();
    }
    owner
}

#[tokio::test]
async fn query_processing_phases_do_not_add_callback_metrics_or_errors() {
    let metrics = RecordingMetrics::default();
    let owner = TelemetryOptions::new(TelemetrySettings::new(TelemetryProfile::Debug))
        .with_instance_id("phase-metrics")
        .with_metric_context(
            MetricRegistrationContext::new(
                Arc::new(metrics.clone()),
                RegistrationLimits::default(),
            )
            .unwrap(),
        )
        .resolve("graph", None)
        .unwrap()
        .observations(
            ComponentId::try_new("query").unwrap(),
            ComponentRole::Query,
            ComponentSemanticKind::Query,
            1,
        )
        .unwrap();
    async {
        for phase in [
            QueryProcessingPhase::PrepareInput,
            QueryProcessingPhase::PrepareOutput,
            QueryProcessingPhase::Finalize,
        ] {
            let result = owner
                .clone()
                .query_phase(phase, async {
                    Err::<(), _>(drasi_core::interface::IndexError::NotSupported)
                })
                .await;
            assert!(result.is_err());
        }
    }
    .with_subscriber(tracing_subscriber::registry())
    .await;
    for operation in ["prepare_input", "prepare_output", "finalize_query"] {
        assert!(metrics
            .matching("drasi.component.calls", &[("operation", operation)])
            .is_empty());
        assert!(metrics
            .matching(
                "drasi.component.operation.duration",
                &[("operation", operation)]
            )
            .is_empty());
    }
    assert_eq!(metrics.count("drasi.errors", &[]), 0);
    assert_eq!(metrics.count("drasi.component.inputs_processed", &[]), 0);
}

#[tokio::test(start_paused = true)]
async fn evaluator_commit_and_cancelled_attempts_are_independent_of_tracing() {
    use drasi_core::computation::QueryOperation;
    let metrics = RecordingMetrics::default();
    let owner = owner(
        &metrics,
        TelemetryProfile::Persistence,
        ComponentRole::Query,
        ComponentSemanticKind::Query,
    );
    let observer = owner.query_observer_for(QueryPhase::Steady, true).unwrap();
    let mut evaluation = observer.start(QueryOperation::Evaluate, Some(1)).unwrap();
    tokio::time::advance(Duration::from_secs(2)).await;
    evaluation.finish(true, Some(1));
    drop(evaluation);
    tokio::time::advance(Duration::from_secs(3)).await;
    let mut commit = observer.start(QueryOperation::Commit, None).unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    commit.finish(true, None);
    drop(commit);
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[("phase", "steady")]),
        vec![2.0]
    );
    assert_eq!(
        metrics.histogram("drasi.query.commit.duration", &[("outcome", "success")]),
        vec![5.0]
    );
    let evaluation = observer.start(QueryOperation::Evaluate, Some(1)).unwrap();
    tokio::time::advance(Duration::from_secs(1)).await;
    drop(evaluation);
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]),
        vec![2.0, 1.0]
    );
    assert_eq!(metrics.count("drasi.query.events_processed", &[]), 0);
    owner.revoke();
    let mut evaluation = observer.start(QueryOperation::Evaluate, Some(1)).unwrap();
    evaluation.finish(true, Some(1));
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]),
        vec![2.0, 1.0]
    );
}

#[tokio::test(start_paused = true)]
async fn worker_effect_success_precedes_checkpointing_and_stale_updates_are_fenced() {
    let metrics = RecordingMetrics::default();
    let owner = owner(
        &metrics,
        TelemetryProfile::Basic,
        ComponentRole::Service,
        ComponentSemanticKind::Reaction,
    );
    owner
        .bind_reaction_queries(&["one".into(), "two".into()])
        .unwrap();
    let worker = owner.scope(async { WorkerTelemetry::capture() }).await;
    let mut work = worker.reaction_batch_work();
    tokio::time::advance(Duration::from_secs(2)).await;
    work.handled("one", 3, 1);
    work.handled("two", 2, 0);
    work.finish(true);
    tokio::time::advance(Duration::from_secs(5)).await;
    drop(work);
    assert_eq!(
        metrics.count("drasi.reaction.results_processed", &[("query_id", "one")]),
        3
    );
    assert_eq!(
        metrics.count("drasi.reaction.results_processed", &[("query_id", "two")]),
        2
    );
    assert_eq!(
        metrics.count(
            "drasi.component.inputs_processed",
            &[("completion", "handled")]
        ),
        1
    );
    assert_eq!(
        metrics.histogram(
            "drasi.component.operation.duration",
            &[("operation", "reaction_batch"), ("outcome", "success")]
        ),
        vec![2.0]
    );
    let mut failed = worker.reaction_work();
    failed.handled("one", 1, 0);
    failed.finish(false);
    assert_eq!(metrics.count("drasi.errors", &[]), 1);
    owner.revoke();
    failed.handled("one", 10, 1);
    assert_eq!(
        metrics.count("drasi.reaction.results_processed", &[("query_id", "one")]),
        4
    );
}

#[test]
fn checkpoint_lag_requires_known_compatible_progress_and_tracks_both_owners() {
    let metrics = RecordingMetrics::default();
    let owner = owner(
        &metrics,
        TelemetryProfile::Persistence,
        ComponentRole::Service,
        ComponentSemanticKind::Reaction,
    );
    owner.bind_reaction_queries(&["query".into()]).unwrap();
    let subscription = owner.reaction_subscription("query").unwrap();
    let progress = QueryProgressObservations::default();
    let epoch = QueryEpoch {
        generation: 1,
        config_hash: 9,
        incarnation: Some(uuid::Uuid::new_v4()),
    };
    progress.bind(&subscription, epoch).unwrap();
    progress.publish(Some(QueryHead {
        epoch,
        sequence: 10,
    }));
    assert!(metrics
        .matching("drasi.reaction.checkpoint_lag_events", &[])
        .is_empty());
    subscription.checkpoint(7, 8).unwrap();
    assert!(metrics
        .matching("drasi.reaction.checkpoint_lag_events", &[])
        .is_empty());
    subscription.checkpoint(7, 9).unwrap();
    progress.publish(Some(QueryHead {
        epoch,
        sequence: 12,
    }));
    subscription.checkpoint(12, 9).unwrap();
    assert_eq!(
        metrics.gauge_samples("drasi.reaction.checkpoint_lag_events", &[]),
        vec![3.0, 5.0, 0.0]
    );
    progress.publish(None);
    subscription.checkpoint(12, 9).unwrap();
    progress.publish(Some(QueryHead {
        epoch: QueryEpoch {
            generation: 2,
            ..epoch
        },
        sequence: 20,
    }));
    assert_eq!(
        metrics.gauge_samples("drasi.reaction.checkpoint_lag_events", &[]),
        vec![3.0, 5.0, 0.0]
    );
    owner.revoke();
    progress.publish(Some(QueryHead {
        epoch,
        sequence: 20,
    }));
    assert_eq!(
        metrics.gauge_samples("drasi.reaction.checkpoint_lag_events", &[]),
        vec![3.0, 5.0, 0.0]
    );
}

#[test]
fn exclusions_leave_no_detail_observer_or_recorder_registrations() {
    let metrics = RecordingMetrics::default();
    let graph = TelemetryOptions::new(
        TelemetrySettings::new(TelemetryProfile::Basic)
            .with_tracing(false)
            .exclude_metrics(""),
    )
    .with_instance_id("excluded")
    .with_metric_context(
        MetricRegistrationContext::new(Arc::new(metrics.clone()), RegistrationLimits::default())
            .unwrap(),
    )
    .resolve("graph", None)
    .unwrap();
    let owner = graph
        .observations(
            ComponentId::try_new("query").unwrap(),
            ComponentRole::Query,
            ComponentSemanticKind::Query,
            1,
        )
        .unwrap();
    assert!(owner.query_observer_for(QueryPhase::Steady, true).is_none());
    owner.query_input(QueryPhase::Steady);
    owner.query_state(2, 3);
    assert!(metrics.is_empty());
}

fn query_definition(text: &str) -> crate::computation::v1::ContinuousQueryDefinition {
    use crate::computation::v1::*;
    ContinuousQueryDefinition {
        graph_id: "metric-query".into(),
        id: ComponentId::try_new("component").unwrap(),
        query: text.into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("query/out").unwrap(),
        outbox_capacity: std::num::NonZeroUsize::new(8).unwrap(),
    }
}

fn query_input(sequence: u64) -> crate::computation::v1::InputEnvelope {
    use crate::computation::v1::*;
    use drasi_core::models::*;
    InputEnvelope {
        execution_context: Default::default(),
        port: PortId::try_new("in").unwrap(),
        envelope: GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", &sequence.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: 1_000,
                    },
                    properties: ElementPropertyMap::from(
                        serde_json::json!({"name": "Name", "ready": true}),
                    ),
                },
            },
            StreamId::try_new("source/out").unwrap(),
            sequence,
            Some(chrono::DateTime::from_timestamp_millis(1_000).unwrap()),
        )
        .unwrap(),
    }
}

#[tokio::test]
async fn real_query_distinguishes_inputs_due_work_and_idle_notifications() {
    use crate::computation::v1::*;
    let metrics = RecordingMetrics::default();
    let owner = owner(
        &metrics,
        TelemetryProfile::Persistence,
        ComponentRole::Query,
        ComponentSemanticKind::Query,
    );
    owner.scope(async {
        let mut query = ContinuousQueryTransformer::new(
            query_definition("MATCH (n:Item) WHERE drasi.trueFor(n.ready, duration({ seconds: 5 })) RETURN n.name AS name"),
            Arc::new(InMemoryComputationProvider),
        ).await.unwrap();
        query.start().await.unwrap();
        assert!(query.on_wakeup().await.unwrap().is_empty());
        assert!(metrics.histogram("drasi.query.engine.duration", &[]).is_empty());
        assert!(query.transform(query_input(1)).await.unwrap().is_empty());
        assert!(query.transform(query_input(1)).await.unwrap().is_empty());
        assert_eq!(metrics.count("drasi.query.events_processed", &[]), 1);
        assert_eq!(metrics.histogram("drasi.query.engine.duration", &[]).len(), 1);
        let outputs = query.on_wakeup().await.unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(metrics.count("drasi.query.events_processed", &[]), 1);
        assert_eq!(metrics.count("drasi.query.results_emitted", &[]), 1);
        assert_eq!(metrics.histogram("drasi.query.engine.duration", &[]).len(), 2);
        assert!(query.on_wakeup().await.unwrap().is_empty());
        let notification = InputEnvelope {
            envelope: GraphChangeCodec::encode_futures_due(
                StreamId::try_new("timer/out").unwrap(), 1, chrono::Utc::now(),
            ).unwrap(),
            ..query_input(2)
        };
        assert!(query.transform(notification).await.unwrap().is_empty());
        assert_eq!(metrics.count("drasi.query.events_processed", &[]), 1);
        assert_eq!(metrics.histogram("drasi.query.engine.duration", &[]).len(), 2);
        query.stop().await.unwrap();
    }).await;
}

#[tokio::test]
async fn prepared_input_replay_evaluates_only_fresh_inputs() {
    use crate::computation::v1::*;
    struct Replay(std::sync::Mutex<Vec<ChangeEnvelope>>);
    #[async_trait::async_trait]
    impl ComputationBootstrapProvider for Replay {
        async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
            Ok(ComputationBootstrapSnapshot {
                changes: Box::pin(futures::stream::empty()),
                watermarks: Vec::new(),
            })
        }
        fn take_input_replay(&self) -> anyhow::Result<Vec<ChangeEnvelope>> {
            Ok(std::mem::take(&mut *self.0.lock().unwrap()))
        }
    }
    let metrics = RecordingMetrics::default();
    let owner = owner(
        &metrics,
        TelemetryProfile::Basic,
        ComponentRole::Query,
        ComponentSemanticKind::Query,
    );
    owner
        .scope(async {
            let replay = Arc::new(Replay(std::sync::Mutex::new(vec![
                query_input(1).envelope,
                query_input(1).envelope,
            ])));
            let mut query = ContinuousQueryTransformer::new(
                query_definition("MATCH (n:Item) RETURN n.name AS name"),
                Arc::new(InMemoryComputationProvider),
            )
            .await
            .unwrap()
            .with_bootstrap(replay);
            query.start().await.unwrap();
            assert_eq!(query.on_wakeup().await.unwrap().len(), 1);
            assert!(query.on_wakeup().await.unwrap().is_empty());
            assert_eq!(
                metrics.count("drasi.query.events_processed", &[("phase", "steady")]),
                1
            );
            assert_eq!(metrics.count("drasi.query.results_emitted", &[]), 1);
            assert_eq!(
                metrics.histogram("drasi.query.engine.duration", &[]).len(),
                1
            );
            query.stop().await.unwrap();
        })
        .await;
}

#[cfg(feature = "computation-rocksdb-tests")]
#[tokio::test]
async fn durable_output_replay_only_observes_real_bookkeeping_commits() {
    use crate::computation::v1::*;
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    let directory = tempfile::tempdir().unwrap();
    let provider = || -> Arc<dyn drasi_core::computation::ComputationIndexProvider> {
        Arc::new(RocksDbComputationProvider::new(
            directory.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).unwrap(),
            ),
        ))
    };
    let metrics = RecordingMetrics::default();
    let owner = owner(
        &metrics,
        TelemetryProfile::Persistence,
        ComponentRole::Query,
        ComponentSemanticKind::Query,
    );
    owner
        .scope(async {
            let definition = || query_definition("MATCH (n:Item) RETURN n.name AS name");
            let mut query = ContinuousQueryTransformer::new(definition(), provider())
                .await
                .unwrap()
                .with_delivery_tracking();
            query.start().await.unwrap();
            let output = query.transform(query_input(1)).await.unwrap();
            let identity = QueryRecoveryIdentity::from_envelope(&output[0].envelope).unwrap();
            query.stop().await.unwrap();
            drop(query);
            let mut query = ContinuousQueryTransformer::new(definition(), provider())
                .await
                .unwrap()
                .with_delivery_tracking();
            query.start().await.unwrap();
            let commits = metrics.histogram("drasi.query.commit.duration", &[]).len();
            let replay = query.on_wakeup().await.unwrap();
            assert_eq!(replay.len(), 1);
            assert_eq!(
                QueryRecoveryIdentity::from_envelope(&replay[0].envelope).unwrap(),
                identity
            );
            assert_eq!(metrics.count("drasi.query.events_processed", &[]), 1);
            assert_eq!(metrics.count("drasi.query.results_emitted", &[]), 1);
            assert_eq!(
                metrics.histogram("drasi.query.engine.duration", &[]).len(),
                1
            );
            assert_eq!(
                metrics.histogram("drasi.query.commit.duration", &[]).len(),
                commits + 1
            );
            query.delivery_completed(&replay).await.unwrap();
            query.stop().await.unwrap();
        })
        .await;
}

#[tokio::test]
async fn split_input_completion_is_per_query_and_does_not_count_dropped_tails() {
    let metrics = RecordingMetrics::default();
    let owner = owner(
        &metrics,
        TelemetryProfile::Basic,
        ComponentRole::Service,
        ComponentSemanticKind::Reaction,
    );
    owner
        .bind_reaction_queries(&["one".into(), "two".into()])
        .unwrap();
    let worker = owner.scope(async { WorkerTelemetry::capture() }).await;
    let one = worker.reaction_input("one", 3);
    let two = worker.reaction_input("two", 2);
    one.clone().handled(2);
    two.clone().handled(1);
    assert_eq!(
        metrics.count(
            "drasi.component.inputs_processed",
            &[("completion", "handled")]
        ),
        0
    );
    one.handled(1);
    drop(two);
    assert_eq!(
        metrics.count(
            "drasi.component.inputs_processed",
            &[("completion", "handled")]
        ),
        1
    );
    assert_eq!(
        metrics.count("drasi.reaction.results_processed", &[("query_id", "one")]),
        3
    );
    assert_eq!(
        metrics.count("drasi.reaction.results_processed", &[("query_id", "two")]),
        1
    );
}
