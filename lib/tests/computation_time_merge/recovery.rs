// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource, ComputationTransaction,
        InMemoryComputationProvider, TransactionDomain,
    },
    interface::{FailureMode, IndexError, IndexSet, SessionControl},
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use std::{
    path::Path,
    sync::atomic::{AtomicU8, Ordering},
};

fn provider(path: &Path) -> Arc<dyn ComputationIndexProvider> {
    Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("RocksDB budget"),
        ),
    ))
}

async fn create(path: &Path, config: SourceTimeMergeDefinition) -> SourceTimeMergeTransformer {
    SourceTimeMergeTransformer::new_durable(config, schema(), provider(path))
        .await
        .expect("construct durable merger")
}

async fn durable(path: &Path, config: SourceTimeMergeDefinition) -> SourceTimeMergeTransformer {
    let mut merger = create(path, config).await;
    merger.start().await.expect("start durable merger");
    merger
}

#[tokio::test(start_paused = true)]
async fn buffered_inputs_and_admission_receipts_survive_reconstruction() {
    let directory = tempfile::tempdir().unwrap();
    let mut merger = durable(directory.path(), definition()).await;
    merger.transform(input(event("a", 1, 90))).await.unwrap();
    merger.transform(input(event("b", 1, 10))).await.unwrap();
    merger.stop().await.unwrap();
    drop(merger);
    let mut merger = durable(directory.path(), definition()).await;
    assert_eq!(merger.subscribe().borrow().buffered_events, 2);
    assert!(
        !merger.has_pending_emissions(),
        "reopen conservatively restarts residence waits"
    );
    merger.transform(input(event("b", 1, 10))).await.unwrap();
    assert_eq!(merger.subscribe().borrow().replayed_inputs, 1);
    tokio::time::advance(Duration::from_millis(1000)).await;
    let batch = merger.on_wakeup().await.unwrap();
    assert_eq!(
        collect(&mut merger, batch)
            .await
            .iter()
            .map(timestamp)
            .collect::<Vec<_>>(),
        [10, 90]
    );
    merger.stop().await.unwrap();
    drop(merger);
    let mut merger = durable(directory.path(), definition()).await;
    assert_eq!(merger.subscribe().borrow().emitted, 2);
    assert_eq!(merger.subscribe().borrow().buffered_events, 0);
    merger.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn main_and_late_outboxes_replay_with_stable_producer_progress() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = definition();
    config.late_policy = LateEventPolicy::Route;
    config.late_output_stream = Some(stream("late"));
    let mut merger = durable(directory.path(), config.clone()).await;
    merger.transform(input(event("a", 1, 100))).await.unwrap();
    tokio::time::advance(Duration::from_millis(1000)).await;
    let mut batch = merger.on_wakeup().await.unwrap();
    for late in [false, true] {
        if late {
            batch = merger.transform(input(event("b", 1, 50))).await.unwrap();
        }
        let original = GraphProducerProgress::from_envelope(&batch[0].envelope)
            .unwrap()
            .unwrap();
        assert!(original.identity().persistent());
        merger.stop().await.unwrap();
        drop(merger);
        merger = durable(directory.path(), config.clone()).await;
        let replay = merger.continue_transform().await.unwrap();
        assert_eq!(
            GraphProducerProgress::from_envelope(&replay[0].envelope)
                .unwrap()
                .unwrap(),
            original
        );
        assert_eq!(replay[0].port, batch[0].port);
        assert!(replay[0].envelope.system().sequence() > batch[0].envelope.system().sequence());
        merger.delivery_completed(&replay).await.unwrap();
    }
    merger.transform(input(event("a", 2, 200))).await.unwrap();
    tokio::time::advance(Duration::from_millis(1000)).await;
    let batch = merger.on_wakeup().await.unwrap();
    assert_eq!(
        logical(&batch[0]),
        2,
        "late stream must not leave a main logical gap"
    );
    merger.delivery_completed(&batch).await.unwrap();
    merger.stop().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn held_input_survives_restart_and_requires_explicit_loss_acknowledgement() {
    let directory = tempfile::tempdir().unwrap();
    let mut merger = durable(directory.path(), definition()).await;
    merger.transform(input(event("a", 1, 100))).await.unwrap();
    tokio::time::advance(Duration::from_millis(1000)).await;
    let batch = merger.on_wakeup().await.unwrap();
    collect(&mut merger, batch).await;
    assert!(merger.transform(input(event("b", 1, 90))).await.is_err());
    merger.stop().await.unwrap();
    drop(merger);
    let mut merger = create(directory.path(), definition()).await;
    assert!(matches!(
        merger.start().await.unwrap_err().downcast_ref(),
        Some(SourceTimeMergeError::Late { .. })
    ));
    assert_eq!(
        merger
            .subscribe()
            .borrow()
            .held_event
            .as_ref()
            .unwrap()
            .system()
            .timestamp(),
        Some(time(90))
    );
    merger.discard_held_event().await.unwrap();
    merger.stop().await.unwrap();
    drop(merger);
    let mut merger = durable(directory.path(), definition()).await;
    assert_eq!(merger.subscribe().borrow().discarded, 1);
    assert_eq!(merger.subscribe().borrow().buffered_events, 0);
    merger.stop().await.unwrap();
}

fn bindings(journal: uuid::Uuid) -> OutputBindings {
    OutputBindings::try_new([OutputDestination {
        output: port("out"),
        consumer: component("sink"),
        input: port("in"),
        journal,
        subscriber: "sink".into(),
    }])
    .expect("destination binding")
}

#[tokio::test]
async fn buffered_work_pins_destination_membership_across_restart() {
    let directory = tempfile::tempdir().unwrap();
    let original = bindings(uuid::Uuid::new_v4());
    let replacement = bindings(uuid::Uuid::new_v4());
    let mut merger = durable(directory.path(), definition()).await;
    merger.bind_output_destinations(&original).await.unwrap();
    merger.transform(input(event("a", 1, 10))).await.unwrap();
    merger.stop().await.unwrap();
    drop(merger);
    let mut merger = durable(directory.path(), definition()).await;
    assert!(merger.bind_output_destinations(&replacement).await.is_err());
    assert!(merger
        .bind_output_destinations(&OutputBindings::default())
        .await
        .is_err());
    merger.bind_output_destinations(&original).await.unwrap();
    merger.stop().await.unwrap();
}

#[tokio::test]
async fn configuration_changes_and_volatile_producers_are_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let mut merger = durable(directory.path(), definition()).await;
    let identity =
        GraphProducerIdentity::volatile("test".into(), "test".into(), component("a"), stream("a"))
            .unwrap();
    let mut original = event("a", 1, 10);
    GraphProducerProgress::annotate(&mut original, &identity, 1).unwrap();
    assert!(merger.transform(input(original.clone())).await.is_err());
    let derived = original.derive(
        emission_id(&stream("b"), 1).unwrap(),
        original.changes().clone(),
        SystemMetadata::new(stream("b"), 1).with_timestamp(time(10)),
    );
    assert!(
        merger.transform(input(derived)).await.is_err(),
        "derivation must not launder volatile ancestry"
    );
    merger.stop().await.unwrap();
    drop(merger);
    let mut changed = definition();
    changed.max_wait_ms += 1;
    let mut merger = create(directory.path(), changed).await;
    assert!(merger.start().await.is_err());
    merger.stop().await.unwrap();
    assert!(SourceTimeMergeTransformer::new_durable(
        definition(),
        schema(),
        Arc::new(InMemoryComputationProvider)
    )
    .await
    .is_err());
}

#[tokio::test]
async fn durable_output_cannot_connect_to_a_volatile_pipe() {
    let directory = tempfile::tempdir().unwrap();
    let merger = create(directory.path(), definition()).await;
    let graph = ComputationGraph::builder("merge-test")
        .source(Box::new(FiniteSource::new("a", vec![])))
        .source(Box::new(FiniteSource::new("b", vec![])))
        .transformer(Box::new(merger))
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            Received::default(),
        )))
        .bind_stream(endpoint("a", "out"), stream("a"))
        .bind_stream(endpoint("b", "out"), stream("b"))
        .bind_stream(endpoint("merge", "out"), stream("merge"))
        .connect(
            edge("a", "merge"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("b", "merge"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("merge", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build();
    assert!(graph.is_err());
}

#[derive(Default)]
struct Fault {
    // 1: fail before commit; 2: lose commit acknowledgement; 3: cancel after commit.
    mode: AtomicU8,
    committed: tokio::sync::Notify,
}

struct FaultSession {
    inner: Arc<dyn SessionControl>,
    fault: Arc<Fault>,
}

#[async_trait]
impl SessionControl for FaultSession {
    async fn begin(&self) -> std::result::Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> std::result::Result<(), IndexError> {
        let mode = self.fault.mode.swap(0, Ordering::SeqCst);
        if mode == 1 {
            return Err(IndexError::IOError);
        }
        self.inner.commit().await?;
        if mode == 2 {
            return Err(IndexError::IOError);
        }
        if mode == 3 {
            self.fault.committed.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    fn rollback(&self) -> std::result::Result<(), IndexError> {
        self.inner.rollback()
    }
}

struct FaultProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    fault: Arc<Fault>,
}

#[async_trait]
impl ComputationIndexProvider for FaultProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        id: &str,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, id).await?;
        let set = original.indexes();
        let session: Arc<dyn SessionControl> = Arc::new(FaultSession {
            inner: set.session_control.clone(),
            fault: self.fault.clone(),
        });
        let domain = TransactionDomain::new(session.clone());
        let indexes = ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: session,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                original.checkpoint_store().expect("checkpoint").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.outbox_writer().expect("outbox").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.live_results_writer().expect("projection").clone(),
                &domain,
            )),
        )
        .map_err(IndexError::other)?;
        Ok(indexes.with_cleanup(original.cleanup().expect("cleanup owner").clone()))
    }
    fn is_volatile(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn commit_errors_and_cancellation_fence_owner_and_recover_exact_admission() {
    for mode in [1, 2, 3] {
        let directory = tempfile::tempdir().unwrap();
        let fault = Arc::new(Fault::default());
        let injected = Arc::new(FaultProvider {
            inner: provider(directory.path()),
            fault: fault.clone(),
        });
        let mut merger = SourceTimeMergeTransformer::new_durable(definition(), schema(), injected)
            .await
            .unwrap();
        merger.start().await.unwrap();
        fault.mode.store(mode, Ordering::SeqCst);
        if mode == 3 {
            tokio::select! {
                _ = fault.committed.notified() => {}
                result = merger.transform(input(event("a", 1, 10))) => panic!("expected cancellation: {result:?}"),
            }
        } else {
            assert!(merger.transform(input(event("a", 1, 10))).await.is_err());
        }
        assert!(matches!(
            merger
                .transform(input(event("a", 2, 20)))
                .await
                .unwrap_err()
                .downcast_ref(),
            Some(SourceTimeMergeError::RecoveryRequired)
        ));
        merger.stop().await.unwrap();
        drop(merger);
        let mut merger = durable(directory.path(), definition()).await;
        assert_eq!(
            merger.subscribe().borrow().buffered_events,
            usize::from(mode != 1)
        );
        merger.transform(input(event("a", 1, 10))).await.unwrap();
        assert_eq!(merger.subscribe().borrow().buffered_events, 1);
        assert_eq!(
            merger.subscribe().borrow().replayed_inputs,
            u64::from(mode != 1)
        );
        merger.stop().await.unwrap();
    }
}

#[tokio::test]
async fn corrupted_retained_snapshots_fail_closed() {
    for field in ["version", "logical_sequence", "late_events", "sources", "buffered"] {
        let directory = tempfile::tempdir().unwrap();
        let mut merger = durable(directory.path(), definition()).await;
        merger.transform(input(event("a", 1, 10))).await.unwrap();
        merger.stop().await.unwrap();
        drop(merger);
        {
            let indexes = provider(directory.path())
                .create_indexes("merge-test", "merge")
                .await
                .unwrap();
            let owner = ComputationTransaction::try_new(indexes).unwrap();
            let store = owner.resources().checkpoint_store().unwrap();
            let key = "\0computation:source-time-merge:v1";
            let record = store.read_checkpoint(key).await.unwrap().unwrap();
            let mut bytes = record.source_position.unwrap().to_vec();
            let encoded_key = rmp_serde::to_vec(field).unwrap();
            let index = bytes
                .windows(encoded_key.len())
                .position(|part| part == encoded_key)
                .unwrap()
                + encoded_key.len();
            bytes[index] = match field {
                "sources" | "buffered" => 0xc1, // Reserved, invalid MessagePack marker.
                _ => 99,
            };
            let bytes = Bytes::from(bytes);
            owner
                .run(async {
                    store.stage_checkpoint(key, 1, Some(&bytes)).await?;
                    Ok(())
                })
                .await
                .unwrap();
            owner.shutdown().await.unwrap();
        }

        let mut merger = create(directory.path(), definition()).await;
        assert!(merger.start().await.is_err(), "corrupt {field} accepted");
        merger.stop().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn output_commit_and_confirmation_failures_preserve_the_exact_obligation() {
    for confirming in [false, true] {
        for mode in [1, 2] {
            let directory = tempfile::tempdir().unwrap();
            let fault = Arc::new(Fault::default());
            let injected = Arc::new(FaultProvider {
                inner: provider(directory.path()),
                fault: fault.clone(),
            });
            let mut merger =
                SourceTimeMergeTransformer::new_durable(definition(), schema(), injected)
                    .await
                    .unwrap();
            merger.start().await.unwrap();
            merger.transform(input(event("a", 1, 10))).await.unwrap();
            tokio::time::advance(Duration::from_millis(1000)).await;
            if confirming {
                let batch = merger.on_wakeup().await.unwrap();
                fault.mode.store(mode, Ordering::SeqCst);
                assert!(merger.delivery_completed(&batch).await.is_err());
            } else {
                fault.mode.store(mode, Ordering::SeqCst);
                assert!(merger.on_wakeup().await.is_err());
            }
            merger.stop().await.unwrap();
            drop(merger);
            let mut merger = durable(directory.path(), definition()).await;
            let pending = !(confirming && mode == 2);
            assert_eq!(
                merger.subscribe().borrow().buffered_events,
                usize::from(pending)
            );
            if pending {
                tokio::time::advance(Duration::from_millis(1000)).await;
                let batch = merger.on_wakeup().await.unwrap();
                assert_eq!(logical(&batch[0]), 1);
                assert_eq!(timestamp(&batch[0]), 10);
                collect(&mut merger, batch).await;
            }
            merger.stop().await.unwrap();
        }
    }
}

#[tokio::test(start_paused = true)]
async fn persistent_qos_receipt_deduplicates_unconfirmed_merger_output_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let mut merger = durable(directory.path(), definition()).await;
    let channel_definition = QosChannelDefinition {
        stream: stream("merge"),
        capacity: size(8),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: std::collections::BTreeMap::from([(
            "sink".into(),
            SubscriptionStart::Earliest,
        )]),
    };
    let mut codec = EnvelopeCodec::new(size(1 << 20));
    codec.register_schema(schema()).unwrap();
    let channel = QosChannel::persistent_with_recovery(
        channel_definition,
        provider(directory.path())
            .create_indexes("merge-test", "journal")
            .await
            .unwrap(),
        codec,
        "merge-journal",
        QosRecoveryOptions::Replay(ReplayOptions {
            failure_scope: FailureMode::ProcessRestart,
            receipt_capacity: size(8),
        }),
    )
    .await
    .unwrap();
    merger.transform(input(event("a", 1, 10))).await.unwrap();
    tokio::time::advance(Duration::from_millis(1000)).await;
    let batch = merger.on_wakeup().await.unwrap();
    channel.publish(&batch[0].envelope).await.unwrap();
    assert_eq!(channel.progress().await.unwrap().accepted, 1);
    merger.stop().await.unwrap();
    drop(merger);
    let mut merger = durable(directory.path(), definition()).await;
    let replay = merger.continue_transform().await.unwrap();
    channel.publish(&replay[0].envelope).await.unwrap();
    merger.delivery_completed(&replay).await.unwrap();
    assert_eq!(
        channel.progress().await.unwrap().accepted,
        1,
        "replay must not append twice"
    );
    merger.stop().await.unwrap();
    channel.shutdown().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn chained_persistent_producers_reject_gaps_but_accept_consecutive_logical_progress() {
    let directory = tempfile::tempdir().unwrap();
    let mut producer = durable(directory.path(), definition()).await;
    producer.transform(input(event("a", 1, 90))).await.unwrap();
    producer.transform(input(event("a", 2, 10))).await.unwrap();
    tokio::time::advance(Duration::from_millis(1000)).await;
    let first = producer.on_wakeup().await.unwrap();
    let outputs = collect(&mut producer, first).await;
    let mut config = definition();
    config.id = component("follower");
    config.sources = vec![stream("merge")];
    config.output_stream = stream("follower");
    config.reorder_window_ms = 0;
    let mut follower = durable(directory.path(), config).await;
    let error = follower
        .transform(input(outputs[1].envelope.clone()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("logical progress has a gap"));
    assert_eq!(follower.subscribe().borrow().buffered_events, 0);
    for output in outputs {
        let batch = follower
            .transform(input(output.envelope.clone()))
            .await
            .unwrap();
        assert_eq!(batch.len(), 1);
        assert_eq!(logical(&batch[0]), logical(&output));
        follower.delivery_completed(&batch).await.unwrap();
        assert!(follower
            .transform(input(output.envelope))
            .await
            .unwrap()
            .is_empty());
    }
    follower.stop().await.unwrap();
    producer.stop().await.unwrap();
}
