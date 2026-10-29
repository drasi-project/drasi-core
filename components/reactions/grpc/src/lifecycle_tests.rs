// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::sync::Arc;
use std::time::Duration;

use drasi_lib::{MemoryStateStoreProvider, Reaction};

use crate::{test_server, BatchingConfig, GrpcReaction, GrpcReactionConfig};

#[path = "../../tests/lifecycle_support.rs"]
mod support;

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
