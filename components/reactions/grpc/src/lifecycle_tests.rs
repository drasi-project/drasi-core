// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;
use std::time::Duration;

use drasi_lib::{MemoryStateStoreProvider, Reaction};

use crate::{test_server, BatchingConfig, GrpcReaction, GrpcReactionConfig};

#[path = "../../tests/lifecycle_support.rs"]
mod support;

#[path = "../../tests/worker_telemetry.rs"]
mod worker_telemetry;

fn reaction(endpoint: &str, adaptive: bool) -> GrpcReaction {
    GrpcReaction::new(
        "grpc-lifecycle",
        vec!["q1".into()],
        GrpcReactionConfig {
            endpoint: endpoint.into(),
            timeout_ms: 30_000,
            max_retries: 0,
            initial_connection_timeout_ms: 3_000,
            batching: if adaptive {
                BatchingConfig::Adaptive {
                    adaptive_min_batch_size: 1,
                    adaptive_max_batch_size: 1,
                    adaptive_window_size: 10,
                    adaptive_batch_timeout_ms: 10,
                }
            } else {
                BatchingConfig::Fixed {
                    batch_size: 1,
                    batch_flush_timeout_ms: 10,
                }
            },
            ..Default::default()
        },
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_tracing_preserves_batch_origins_in_fixed_and_adaptive_modes() {
    use drasi_lib::observability::ExecutionContext;
    for adaptive in [false, true] {
        for same_origin in [false, true] {
            let server = test_server::start_held().await;
            let reaction = Arc::new(
                GrpcReaction::new(
                    "grpc-trace",
                    vec!["q1".into()],
                    GrpcReactionConfig {
                        endpoint: server.endpoint.clone(),
                        max_retries: 0,
                        batching: if adaptive {
                            BatchingConfig::Adaptive {
                                adaptive_min_batch_size: 2,
                                adaptive_max_batch_size: 2,
                                adaptive_window_size: 10,
                                adaptive_batch_timeout_ms: 10_000,
                            }
                        } else {
                            BatchingConfig::Fixed {
                                batch_size: 2,
                                batch_flush_timeout_ms: 10_000,
                            }
                        },
                        ..Default::default()
                    },
                )
                .unwrap(),
            );
            let recording = worker_telemetry::Recording::default();
            let origins = [
                worker_telemetry::context(1),
                worker_telemetry::context(if same_origin { 1 } else { 2 }),
                ExecutionContext::default(),
                ExecutionContext::default(),
            ];
            recording
                .run(reaction.clone(), async {
                    for (index, origin) in origins.iter().enumerate() {
                        let mut result = support::result(index as u64 + 1);
                        if index == 1 {
                            result.sequence = 1;
                        }
                        worker_telemetry::enqueue(reaction.as_ref(), result, origin.clone()).await;
                    }
                    assert_eq!(
                        server
                            .recorder
                            .wait_for_items(2, Duration::from_secs(3))
                            .await,
                        2
                    );
                    assert!(
                        recording.finished().is_empty(),
                        "remote acknowledgement is pending"
                    );
                    server.recorder.release_responses(2);
                    recording.wait_for_finished(2).await;
                    assert_eq!(server.recorder.total_items().await, 4);
                })
                .await;
            let spans = recording.finished();
            assert_eq!(spans.len(), 2);
            assert!(spans
                .iter()
                .all(|span| span.name == "reaction.process_batch"
                    && span.fields["component_id"] == "grpc-trace"
                    && span.fields["outcome"] == "success"
                    && span.fields["completion"] == "handled"
                    && span.fields["input_count"] == "2"
                    && span.fields["change_count"] == "2"));
            let parents = recording.parents();
            if same_origin {
                assert_eq!(parents[0].context, origins[0]);
                assert!(parents[0].links.is_empty());
            } else {
                assert!(parents[0].context.is_empty());
                assert_eq!(parents[0].links.len(), 2);
                assert!(parents[0].links.contains(origins[0].trace().unwrap()));
                assert!(parents[0].links.contains(origins[1].trace().unwrap()));
            }
            assert!(parents[1].context.is_empty());
            assert!(parents[1].links.is_empty());
            server.shutdown().await;
        }
    }
}

#[tokio::test]
async fn worker_tracing_counts_inputs_not_diffs_when_a_result_splits_across_batches() {
    for adaptive in [false, true] {
        let server = test_server::start().await;
        let reaction = Arc::new(
            GrpcReaction::new(
                "grpc-trace-split",
                vec!["q1".into()],
                GrpcReactionConfig {
                    endpoint: server.endpoint.clone(),
                    batching: if adaptive {
                        BatchingConfig::Adaptive {
                            adaptive_min_batch_size: 1,
                            adaptive_max_batch_size: 2,
                            adaptive_window_size: 10,
                            adaptive_batch_timeout_ms: 20,
                        }
                    } else {
                        BatchingConfig::Fixed {
                            batch_size: 2,
                            batch_flush_timeout_ms: 20,
                        }
                    },
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let recording = worker_telemetry::Recording::default();
        recording
            .run(reaction.clone(), async {
                let mut result = support::result(7);
                result.results = (1..=3)
                    .map(|row_signature| drasi_lib::channels::ResultDiff::Add {
                        data: serde_json::json!({"id": row_signature}),
                        row_signature,
                    })
                    .collect();
                worker_telemetry::enqueue(reaction.as_ref(), result, worker_telemetry::context(1))
                    .await;
                recording.wait_for_finished(2).await;
                assert_eq!(server.recorder.total_items().await, 3);
                assert_eq!(
                    reaction
                        .base
                        .read_checkpoint("q1")
                        .await
                        .unwrap()
                        .unwrap()
                        .sequence,
                    7
                );
            })
            .await;
        let spans = recording.finished();
        assert_eq!(spans.len(), 2);
        assert!(spans
            .iter()
            .all(|span| span.fields["input_count"] == "1" && span.fields["outcome"] == "success"));
        assert_eq!(
            spans
                .iter()
                .map(|span| span.fields["change_count"].parse::<usize>().unwrap())
                .sum::<usize>(),
            3
        );
        assert!(recording.parents().iter().all(|parent| parent.context
            == worker_telemetry::context(1)
            && parent.links.is_empty()));
        server.shutdown().await;
    }
}

#[tokio::test]
async fn worker_tracing_records_grpc_failure_before_skip_or_stop() {
    use drasi_lib::{channels::ComponentStatus, ReactionRecoveryPolicy};
    for adaptive in [false, true] {
        for policy in [
            ReactionRecoveryPolicy::Strict,
            ReactionRecoveryPolicy::AutoSkipGap,
        ] {
            let server = test_server::start_with_failures(1).await;
            let reaction = Arc::new(
                GrpcReaction::builder("grpc-trace-error")
                    .with_query("q1")
                    .with_config(GrpcReactionConfig {
                        endpoint: server.endpoint.clone(),
                        max_retries: 0,
                        batching: if adaptive {
                            BatchingConfig::Adaptive {
                                adaptive_min_batch_size: 1,
                                adaptive_max_batch_size: 1,
                                adaptive_window_size: 10,
                                adaptive_batch_timeout_ms: 10,
                            }
                        } else {
                            BatchingConfig::Fixed {
                                batch_size: 1,
                                batch_flush_timeout_ms: 10,
                            }
                        },
                        ..Default::default()
                    })
                    .with_recovery_policy(policy)
                    .build()
                    .unwrap(),
            );
            let recording = worker_telemetry::Recording::default();
            recording
                .run(reaction.clone(), async {
                    worker_telemetry::enqueue(
                        reaction.as_ref(),
                        support::result(7),
                        worker_telemetry::context(1),
                    )
                    .await;
                    recording.wait_for_finished(1).await;
                    assert_eq!(recording.finished()[0].fields["outcome"], "error");
                    let checkpoint = reaction.base.read_checkpoint("q1").await.unwrap();
                    if policy == ReactionRecoveryPolicy::Strict {
                        assert!(checkpoint.is_none());
                        assert!(matches!(reaction.status().await, ComponentStatus::Error));
                    } else {
                        assert_eq!(checkpoint.unwrap().sequence, 7);
                        assert!(matches!(reaction.status().await, ComponentStatus::Running));
                    }
                })
                .await;
            server.shutdown().await;
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn requests_survive_stop_and_three_restarts_in_both_modes() {
    for adaptive in [false, true] {
        let server = test_server::start_held().await;
        let reaction = reaction(&server.endpoint, adaptive);
        support::initialize(&reaction, Arc::new(MemoryStateStoreProvider::new())).await;
        for sequence in 1..=3 {
            reaction.start().await.unwrap();
            assert!(reaction
                .start()
                .await
                .unwrap_err()
                .is::<drasi_lib::context::workers::WorkerAlreadyOwned>());
            reaction
                .enqueue_query_result(support::result(sequence))
                .await
                .unwrap();
            assert_eq!(
                server
                    .recorder
                    .wait_for_items(usize::try_from(sequence).unwrap(), Duration::from_secs(3))
                    .await,
                usize::try_from(sequence).unwrap()
            );
            if sequence == 1 {
                support::assert_incomplete_stop(&reaction, &reaction.base).await;
                assert!(reaction.base.read_checkpoint("q1").await.unwrap().is_none());
            }
            server.recorder.release_responses(1);
            reaction.stop().await.unwrap();
            assert!(reaction.base.processing_task.read().await.is_none());
            assert!(reaction.base.shutdown_tx.read().await.is_none());
            assert_eq!(
                reaction
                    .base
                    .read_checkpoint("q1")
                    .await
                    .unwrap()
                    .unwrap()
                    .sequence,
                sequence
            );
        }
        let batches = server.recorder.batches().await;
        assert_eq!(batches.len(), 3);
        assert_eq!(
            batches
                .iter()
                .flat_map(|batch| batch.sequences.iter().copied())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        server.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn checkpoint_writes_survive_stop_in_both_modes() {
    for adaptive in [false, true] {
        let server = test_server::start().await;
        let reaction = reaction(&server.endpoint, adaptive);
        let store = Arc::new(support::ControlledStore::new(false));
        support::initialize(&reaction, store.clone()).await;
        reaction.start().await.unwrap();
        reaction
            .enqueue_query_result(support::result(7))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), store.entered.notified())
            .await
            .unwrap();
        assert_eq!(server.recorder.total_items().await, 1);
        support::assert_incomplete_stop(&reaction, &reaction.base).await;
        assert!(reaction.base.read_checkpoint("q1").await.unwrap().is_none());
        store.release.add_permits(1);
        reaction.stop().await.unwrap();
        assert_eq!(
            reaction
                .base
                .read_checkpoint("q1")
                .await
                .unwrap()
                .unwrap()
                .sequence,
            7
        );
        server.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_start_and_checkpoint_panic_require_cleanup_in_both_modes() {
    for adaptive in [false, true] {
        let server = test_server::start().await;
        let reaction = reaction(&server.endpoint, adaptive);
        let store = Arc::new(support::ControlledStore::new(true));
        support::initialize(&reaction, store.clone()).await;
        support::assert_cancelled_start(&reaction, &reaction.base).await;
        reaction.start().await.unwrap();
        reaction
            .enqueue_query_result(support::result(1))
            .await
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), store.entered.notified())
            .await
            .unwrap();
        store.release.add_permits(1);
        support::assert_panic_cleanup(&reaction, &reaction.base).await;
        assert!(reaction.base.read_checkpoint("q1").await.unwrap().is_none());
        assert_eq!(server.recorder.total_items().await, 1);
        server.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn admitted_result_is_not_discarded_by_stopping_status() {
    for adaptive in [false, true] {
        let server = test_server::start_held().await;
        let reaction = reaction(&server.endpoint, adaptive);
        support::initialize(&reaction, Arc::new(MemoryStateStoreProvider::new())).await;
        reaction.start().await.unwrap();
        let signal_lock = reaction.base.shutdown_tx.write().await;
        reaction
            .enqueue_query_result(support::result(1))
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), reaction.stop())
                .await
                .is_err()
        );
        assert_eq!(
            server
                .recorder
                .wait_for_items(1, Duration::from_secs(3))
                .await,
            1
        );
        drop(signal_lock);
        server.recorder.release_responses(1);
        reaction.stop().await.unwrap();
        assert_eq!(
            reaction
                .base
                .read_checkpoint("q1")
                .await
                .unwrap()
                .unwrap()
                .sequence,
            1
        );
        server.shutdown().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn final_fixed_batch_remains_owned_until_acknowledged() {
    let server = test_server::start_held().await;
    let reaction = GrpcReaction::builder("grpc-final-flush")
        .with_query("q1")
        .with_endpoint(&server.endpoint)
        .with_timeout_ms(30_000)
        .with_batching(BatchingConfig::Fixed {
            batch_size: 10,
            batch_flush_timeout_ms: 30_000,
        })
        .build()
        .unwrap();
    support::initialize(&reaction, Arc::new(MemoryStateStoreProvider::new())).await;
    reaction.start().await.unwrap();
    // Let the interval's initial empty tick run before submitting a partial batch.
    tokio::task::yield_now().await;
    reaction
        .enqueue_query_result(support::result(1))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while !reaction.base.priority_queue.is_empty().await {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(server.recorder.total_items().await, 0);
    support::assert_incomplete_stop(&reaction, &reaction.base).await;
    assert_eq!(server.recorder.total_items().await, 1);
    server.recorder.release_responses(1);
    reaction.stop().await.unwrap();
    assert_eq!(
        reaction
            .base
            .read_checkpoint("q1")
            .await
            .unwrap()
            .unwrap()
            .sequence,
        1
    );
    server.shutdown().await;
}
