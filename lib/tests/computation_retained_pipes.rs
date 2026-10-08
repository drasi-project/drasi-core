// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(test)]

#[allow(dead_code)]
mod computation_support;

use std::{num::NonZeroUsize, sync::Arc};

use computation_support::*;
use drasi_lib::computation::v1::*;

fn memory(capacity: usize, policy: RetentionPolicy) -> Arc<MemoryEnvelopeStore> {
    Arc::new(MemoryEnvelopeStore::new(
        NonZeroUsize::new(capacity).expect("capacity"),
        policy,
    ))
}

async fn handled(delivery: Delivery) {
    delivery
        .into_parts()
        .1
        .expect("explicit acknowledgement")
        .complete(HandlingOutcome::Handled)
        .await
        .expect("confirm local handling");
}

#[tokio::test]
async fn retained_delivery_drop_or_failure_does_not_acknowledge_and_capacity_backpressures() {
    let store = memory(1, RetentionPolicy::Backpressure);
    let mut pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("pipe");
    assert!(!pipe
        .capabilities()
        .supported()
        .contains(&PipeCapability::DurableAcceptance));
    assert!(pipe
        .capabilities()
        .supported()
        .contains(&PipeCapability::RetainedHistory));
    let sender = pipe.sender();
    let control = pipe.control();
    let mut receiver = pipe.take_receiver().expect("receiver");
    sender
        .send(root("source", 1, &[3]))
        .await
        .expect("first acceptance");
    let mut blocked = Box::pin(sender.send(root("source", 2, &[4])));
    assert!(futures::poll!(&mut blocked).is_pending());
    let first = receiver
        .receive()
        .await
        .expect("receive")
        .expect("delivery");
    drop(first);
    assert_eq!(
        store.progress(store.generation()).await.expect("progress"),
        0
    );
    let replayed = receiver.receive().await.expect("replay").expect("delivery");
    assert_eq!(replayed.envelope().system().sequence(), 1);
    replayed
        .into_parts()
        .1
        .expect("ack")
        .complete(HandlingOutcome::Failed {
            reason: "retry".into(),
        })
        .await
        .expect("record failure without progress");
    assert_eq!(
        store.progress(store.generation()).await.expect("progress"),
        0
    );
    handled(
        receiver
            .receive()
            .await
            .expect("replay again")
            .expect("delivery"),
    )
    .await;
    blocked.await.expect("capacity released by handling");
    control.close();
    let second = receiver.receive().await.expect("drain").expect("second");
    assert_eq!(second.envelope().system().sequence(), 2);
    handled(second).await;
    assert!(receiver
        .receive()
        .await
        .expect("drained after handling")
        .is_none());
}

#[tokio::test]
async fn new_binding_generation_closes_old_senders_and_acknowledgements_but_replays_unhandled_data()
{
    let store = memory(2, RetentionPolicy::Backpressure);
    let mut old = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("old pipe");
    let old_sender = old.sender();
    let mut old_receiver = old.take_receiver().expect("old receiver");
    old_sender
        .send(root("source", 1, &[3]))
        .await
        .expect("old acceptance");
    let old_delivery = old_receiver
        .receive()
        .await
        .expect("old receive")
        .expect("delivery");
    let mut new =
        RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("new generation");
    let mut new_receiver = new.take_receiver().expect("new receiver");
    assert!(matches!(
        old_sender
            .send(root("source", 2, &[4]))
            .await
            .expect_err("old sender closed")
            .error,
        PipeError::Closed
    ));
    assert!(matches!(
        old_delivery
            .into_parts()
            .1
            .expect("old ack")
            .complete(HandlingOutcome::Handled)
            .await,
        Err(PipeError::Closed)
    ));
    assert_eq!(
        store
            .progress(store.generation())
            .await
            .expect("unchanged progress"),
        0
    );
    let replayed = new_receiver
        .receive()
        .await
        .expect("replay")
        .expect("unhandled data");
    assert_eq!(replayed.envelope().system().sequence(), 1);
    handled(replayed).await;
    assert_eq!(
        store.progress(store.generation()).await.expect("progress"),
        1
    );
}

#[tokio::test]
async fn retained_history_gap_requires_explicit_policy_and_never_silent_skip() {
    let store = memory(2, RetentionPolicy::PruneOldest);
    let mut strict = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("pipe");
    let sender = strict.sender();
    for sequence in 1..=4 {
        sender
            .send(root("source", sequence, &[1]))
            .await
            .expect("retained acceptance");
    }
    let mut receiver = strict.take_receiver().expect("receiver");
    assert!(matches!(
        receiver.receive().await,
        Err(PipeError::PositionUnavailable {
            requested: 0,
            oldest: 3
        })
    ));
    let mut lossy = RetainedPipe::new(store.clone(), ReplayGapPolicy::SkipWithNotification)
        .expect("explicit gap policy");
    let mut receiver = lossy.take_receiver().expect("receiver");
    let delivery = receiver
        .receive()
        .await
        .expect("explicit skip")
        .expect("oldest retained");
    assert_eq!(delivery.envelope().system().sequence(), 3);
    handled(delivery).await;
    assert_eq!(
        store
            .progress(store.generation())
            .await
            .expect("explicit progress"),
        3
    );
}

#[tokio::test]
async fn retained_close_wakes_a_full_capacity_sender_without_claiming_acceptance() {
    let store = memory(1, RetentionPolicy::Backpressure);
    let pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("pipe");
    let sender = pipe.sender();
    sender.send(root("source", 1, &[1])).await.expect("first");
    let mut blocked = Box::pin(sender.send(root("source", 2, &[2])));
    assert!(futures::poll!(&mut blocked).is_pending());
    pipe.control().close();
    let failure = blocked.await.expect_err("capacity waiter closed");
    assert_eq!(failure.acceptance(), AcceptanceState::NotAccepted);
    assert_eq!(failure.envelope.system().sequence(), 2);
    assert_eq!(
        store
            .next(store.generation(), 0)
            .await
            .expect("retained")
            .expect("one")
            .envelope
            .system()
            .sequence(),
        1
    );
    assert!(store
        .next(store.generation(), 1)
        .await
        .expect("no second acceptance")
        .is_none());
}

#[tokio::test]
async fn graph_negotiates_retained_pipe_resources_and_acknowledges_actual_sink_handling() {
    let store = memory(1, RetentionPolicy::Backpressure);
    let resource = ResourceId::try_new("journal").expect("resource");
    let provided = Arc::new(RetainedStoreResource(store.clone()));
    let received = Received::default();
    let mut graph = ComputationGraph::builder("retained-graph")
        .source(Box::new(FiniteSource::new(
            "source",
            vec![output(root("source", 1, &[7])), output(root("source", 2, &[8]))],
        )))
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            received.clone(),
        )))
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Graph,
            binding: Arc::from("journal"),
        })
        .expect("declaration")
        .provide_resource(
            resource.clone(),
            ResourceHandle::new(ResourceRole::StateStore, provided.clone()).with_cleanup(provided),
        )
        .expect("resource instance")
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(RetainedPipeConfig {
                resource: resource.clone(),
                capacity: NonZeroUsize::new(1).expect("capacity"),
                durable: false,
                retention: RetentionPolicy::Backpressure,
                gap_policy: ReplayGapPolicy::Strict,
            }),
        )
        .build()
        .expect("negotiated graph");
    assert_eq!(
        graph.snapshot().edges[0].resources[&resource],
        ResourceRole::StateStore
    );
    graph
        .start()
        .expect("scope")
        .await
        .expect("drain handled delivery");
    assert_eq!(received.lock().expect("received").len(), 2);
    assert_eq!(
        store
            .progress(
                store
                    .acquire_generation()
                    .expect("inspect completed journal")
            )
            .await
            .expect("handled progress"),
        2
    );
    graph.dispose().await.expect("owned journal cleanup");
}

#[tokio::test]
async fn graph_broadcast_profile_does_not_hide_its_lag_or_claim_backpressure() {
    let received = Received::default();
    let mut graph = ComputationGraph::builder("broadcast-graph")
        .source(Box::new(FiniteSource::new(
            "source",
            vec![output(root("source", 1, &[7]))],
        )))
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            received.clone(),
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BroadcastPipeConfig {
                capacity: 2,
                lag_policy: BroadcastLagPolicy::Report,
            }),
        )
        .build()
        .expect("explicit broadcast profile");
    assert!(!graph.snapshot().edges[0]
        .capabilities
        .supported()
        .contains(&PipeCapability::Backpressure));
    graph
        .start()
        .expect("scope")
        .await
        .expect("finite broadcast");
    assert_eq!(received.lock().expect("received").len(), 1);
}

struct AcceptanceOnlySink(ComponentDescriptor);

#[async_trait::async_trait]
impl ComputationComponent for AcceptanceOnlySink {
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
impl EnvelopeSink for AcceptanceOnlySink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Accepted
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        panic!("must be rejected in preflight")
    }
}

#[test]
fn retained_graph_preflight_rejects_hidden_resources_shared_exclusive_logs_and_accepted_sinks() {
    for case in ["undeclared", "shared", "accepted"] {
        let store = memory(1, RetentionPolicy::Backpressure);
        let resource = ResourceId::try_new("journal").expect("id");
        let profile = RetainedPipeConfig {
            resource: resource.clone(),
            capacity: NonZeroUsize::new(1).expect("capacity"),
            durable: false,
            retention: RetentionPolicy::Backpressure,
            gap_policy: ReplayGapPolicy::Strict,
        };
        let mut builder = ComputationGraph::builder("retained-preflight")
            .source(Box::new(FiniteSource::new("source", vec![])))
            .bind_stream(endpoint("source", "out"), stream("source"));
        builder = if case == "accepted" {
            builder.sink(Box::new(AcceptanceOnlySink(descriptor(
                "sink",
                &["in"],
                &[],
            ))))
        } else {
            builder.sink(Box::new(CollectSink::new(
                "sink",
                &["in"],
                Received::default(),
            )))
        };
        if case != "undeclared" {
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: resource.clone(),
                    role: ResourceRole::StateStore,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from("journal"),
                })
                .expect("declaration")
                .provide_resource(
                    resource,
                    ResourceHandle::new(
                        ResourceRole::StateStore,
                        Arc::new(RetainedStoreResource(store.clone())),
                    ),
                )
                .expect("resource");
        }
        builder = builder.connect(edge("source", "sink"), Box::new(profile.clone()));
        if case == "shared" {
            builder = builder
                .sink(Box::new(CollectSink::new(
                    "other",
                    &["in"],
                    Received::default(),
                )))
                .connect(edge("source", "other"), Box::new(profile));
        }
        assert!(builder.build().is_err(), "{case}");
        assert_eq!(
            store.generation(),
            0,
            "{case}: no pipe creation during preflight"
        );
    }
}

struct PausedStore {
    inner: Arc<MemoryEnvelopeStore>,
    pause_append: std::sync::atomic::AtomicBool,
    pause_ack: std::sync::atomic::AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl ResourceCleanup for PausedStore {
    async fn shutdown(&self) -> anyhow::Result<()> {
        self.inner.shutdown().await
    }
}

#[async_trait::async_trait]
impl RetainedEnvelopeStore for PausedStore {
    fn capacity(&self) -> NonZeroUsize {
        self.inner.capacity()
    }
    fn durable(&self) -> bool {
        false
    }
    fn retention_policy(&self) -> RetentionPolicy {
        self.inner.retention_policy()
    }
    fn acquire_generation(&self) -> std::result::Result<u64, PipeError> {
        self.inner.acquire_generation()
    }
    fn revoke_generation(&self, generation: u64) {
        self.inner.revoke_generation(generation);
    }
    fn generation(&self) -> u64 {
        self.inner.generation()
    }
    fn notify(&self) -> &tokio::sync::Notify {
        self.inner.notify()
    }
    async fn append(
        &self,
        generation: u64,
        envelope: &ChangeEnvelope,
    ) -> std::result::Result<u64, PipeError> {
        if self
            .pause_append
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.append(generation, envelope).await
    }
    async fn next(
        &self,
        generation: u64,
        after: u64,
    ) -> std::result::Result<Option<StoredEnvelope>, PipeError> {
        self.inner.next(generation, after).await
    }
    async fn progress(&self, generation: u64) -> std::result::Result<u64, PipeError> {
        self.inner.progress(generation).await
    }
    async fn acknowledge(
        &self,
        generation: u64,
        position: u64,
    ) -> std::result::Result<(), PipeError> {
        if self
            .pause_ack
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.entered.notify_one();
            self.release.notified().await;
        }
        self.inner.acknowledge(generation, position).await
    }
}

#[tokio::test]
async fn cancellation_revokes_admitted_sends_and_acks_before_the_store_mutation() {
    let store = Arc::new(PausedStore {
        inner: memory(2, RetentionPolicy::Backpressure),
        pause_append: std::sync::atomic::AtomicBool::new(true),
        pause_ack: std::sync::atomic::AtomicBool::new(false),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("pipe");
    let sender = pipe.sender();
    let mut send = Box::pin(sender.send(root("source", 1, &[1])));
    tokio::select! { result = &mut send => panic!("paused: {result:?}"), _ = store.entered.notified() => {} }
    pipe.control().cancel();
    store.release.notify_one();
    assert!(matches!(
        send.await.expect_err("revoked before write").error,
        PipeError::Closed
    ));

    let mut pipe =
        RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("new generation");
    assert!(store
        .next(store.generation(), 0)
        .await
        .expect("no late write")
        .is_none());
    pipe.sender()
        .send(root("source", 2, &[2]))
        .await
        .expect("accept");
    let mut receiver = pipe.take_receiver().expect("receiver");
    let ack = receiver
        .receive()
        .await
        .expect("delivery")
        .expect("one")
        .into_parts()
        .1
        .expect("ack");
    store
        .pause_ack
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut completion = Box::pin(ack.complete(HandlingOutcome::Handled));
    tokio::select! { result = &mut completion => panic!("paused: {result:?}"), _ = store.entered.notified() => {} }
    pipe.control().cancel();
    store.release.notify_one();
    assert!(matches!(completion.await, Err(PipeError::Closed)));
    let generation = store
        .acquire_generation()
        .expect("new inspection generation");
    assert_eq!(
        store
            .progress(generation)
            .await
            .expect("no late acknowledgement"),
        0
    );
}

#[cfg(feature = "computation-rocksdb-tests")]
mod durable {
    use super::*;
    use drasi_core::{
        computation::{
            ComputationIndexProvider, ComputationIndexes, ComputationResource, TransactionDomain,
        },
        interface::{IndexError, IndexSet, SessionControl},
    };
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    use std::result::Result;

    async fn open(path: &std::path::Path) -> Arc<IndexedEnvelopeStore> {
        open_capacity(path, 2).await
    }

    fn codec() -> Arc<EnvelopeCodec> {
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(16_384).expect("limit"));
        codec
            .register_schema(Arc::new(Schema::new(
                schema_descriptor(),
                Arc::new(ReadingValidator(schema_descriptor())),
            )))
            .expect("schema");
        Arc::new(codec)
    }

    async fn resources(path: &std::path::Path) -> ComputationIndexes {
        let provider = RocksDbComputationProvider::new(
            path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
            ),
        );
        provider
            .create_indexes("retained-pipe", "journal")
            .await
            .expect("durable graph resources")
    }

    async fn open_capacity(path: &std::path::Path, capacity: usize) -> Arc<IndexedEnvelopeStore> {
        let store = Arc::new(
            IndexedEnvelopeStore::try_new(
                resources(path).await,
                codec(),
                "edge",
                NonZeroUsize::new(capacity).expect("capacity"),
                RetentionPolicy::Backpressure,
            )
            .expect("durable journal"),
        );
        assert_eq!(
            store.durability(),
            drasi_core::interface::StorageDurability::LOCAL_PROCESS_RESTART
        );
        store
    }

    #[tokio::test]
    async fn durable_acceptance_reopens_and_only_handled_deliveries_advance_progress() {
        let temp = tempfile::tempdir().expect("temp");
        {
            let store = open(temp.path()).await;
            let mut pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("pipe");
            assert!(pipe
                .capabilities()
                .supported()
                .contains(&PipeCapability::DurableAcceptance));
            assert!(pipe
                .capabilities()
                .supported()
                .contains(&PipeCapability::Replay));
            pipe.sender()
                .send(root("source", 1, &[7]))
                .await
                .expect("durable acceptance");
            pipe.sender()
                .send(root("source", 2, &[8]))
                .await
                .expect("second acceptance");
            let mut receiver = pipe.take_receiver().expect("receiver");
            handled(receiver.receive().await.expect("receive").expect("first")).await;
            drop(
                receiver
                    .receive()
                    .await
                    .expect("receive")
                    .expect("unhandled second"),
            );
            pipe.control().cancel();
            drop(receiver);
            drop(pipe);
            store.shutdown().await.expect("await store cleanup");
        }
        {
            let store = open(temp.path()).await;
            let mut pipe =
                RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("reopened pipe");
            let mut receiver = pipe.take_receiver().expect("receiver");
            assert_eq!(
                store
                    .progress(store.generation())
                    .await
                    .expect("recovered progress"),
                1
            );
            let replayed = receiver
                .receive()
                .await
                .expect("retained replay")
                .expect("unhandled second");
            assert_eq!(replayed.envelope().system().sequence(), 2);
            assert_eq!(values(replayed.envelope()), [8]);
            handled(replayed).await;
            pipe.control().close();
            assert!(receiver.receive().await.expect("drained").is_none());
            drop(receiver);
            drop(pipe);
            store.shutdown().await.expect("await cleanup");
        }
    }

    #[tokio::test]
    async fn reducing_retention_capacity_cannot_prune_unacknowledged_entries_under_backpressure() {
        let temp = tempfile::tempdir().expect("temp");
        {
            let store = open_capacity(temp.path(), 2).await;
            let pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("pipe");
            pipe.sender()
                .send(root("source", 1, &[1]))
                .await
                .expect("first");
            pipe.sender()
                .send(root("source", 2, &[2]))
                .await
                .expect("second");
            drop(pipe);
            store.shutdown().await.expect("cleanup");
        }
        let store = open_capacity(temp.path(), 1).await;
        let mut pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict)
            .expect("smaller retained pipe");
        let sender = pipe.sender();
        let mut receiver = pipe.take_receiver().expect("receiver");
        let third = root("source", 3, &[3]);
        assert!(matches!(
            store.append(store.generation(), &third).await,
            Err(PipeError::CapacityExhausted)
        ));
        handled(
            receiver
                .receive()
                .await
                .expect("first")
                .expect("retained first"),
        )
        .await;
        assert!(
            matches!(
                store.append(store.generation(), &third).await,
                Err(PipeError::CapacityExhausted)
            ),
            "second remains unacknowledged"
        );
        handled(
            receiver
                .receive()
                .await
                .expect("second")
                .expect("retained second"),
        )
        .await;
        sender
            .send(third)
            .await
            .expect("all evicted entries have been handled");
        handled(
            receiver
                .receive()
                .await
                .expect("third")
                .expect("third event"),
        )
        .await;
        pipe.control().close();
        assert!(receiver.receive().await.expect("drain").is_none());
        drop(receiver);
        drop(pipe);
        store.shutdown().await.expect("cleanup");
    }

    struct AmbiguousCommit(
        Arc<dyn SessionControl>,
        std::sync::atomic::AtomicUsize,
        usize,
    );

    #[async_trait::async_trait]
    impl SessionControl for AmbiguousCommit {
        async fn begin(&self) -> std::result::Result<(), IndexError> {
            self.0.begin().await
        }
        async fn commit(&self) -> std::result::Result<(), IndexError> {
            self.0.commit().await?;
            if self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == self.2 {
                Err(IndexError::IOError)
            } else {
                Ok(())
            }
        }
        fn rollback(&self) -> std::result::Result<(), IndexError> {
            self.0.rollback()
        }
    }

    async fn ambiguous_resources(path: &std::path::Path, fail_on: usize) -> ComputationIndexes {
        let original = resources(path).await;
        let set = original.indexes();
        let control: Arc<dyn SessionControl> = Arc::new(AmbiguousCommit(
            set.session_control.clone(),
            std::sync::atomic::AtomicUsize::new(0),
            fail_on,
        ));
        let domain = TransactionDomain::new(control.clone());
        ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: control,
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
                original.live_results_writer().expect("live").clone(),
                &domain,
            )),
        )
        .expect("explicit provider bundle")
        .with_cleanup(original.cleanup().expect("cleanup owner").clone())
    }

    #[tokio::test]
    async fn durable_commit_ambiguity_never_claims_definite_nonacceptance() {
        let temp = tempfile::tempdir().expect("temp");
        {
            let indexes = ambiguous_resources(temp.path(), 1).await;
            let store = Arc::new(
                IndexedEnvelopeStore::try_new(
                    indexes,
                    codec(),
                    "edge",
                    NonZeroUsize::new(2).expect("capacity"),
                    RetentionPolicy::Backpressure,
                )
                .expect("store"),
            );
            let pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("pipe");
            let failure = pipe
                .sender()
                .send(root("source", 1, &[9]))
                .await
                .expect_err("commit receipt uncertain");
            assert_eq!(failure.acceptance(), AcceptanceState::Unknown);
            assert!(matches!(failure.error, PipeError::AcceptanceUnknown { .. }));
            drop(pipe);
            store
                .shutdown()
                .await
                .expect("recover ownership after uncertain commit");
        }
        let store = open(temp.path()).await;
        let mut pipe =
            RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("recovered pipe");
        let mut receiver = pipe.take_receiver().expect("receiver");
        let retained = receiver
            .receive()
            .await
            .expect("recovered durable record")
            .expect("event did commit");
        assert_eq!(values(retained.envelope()), [9]);
        handled(retained).await;
        drop(receiver);
        drop(pipe);
        store.shutdown().await.expect("cleanup");
    }

    #[tokio::test]
    async fn durable_acknowledgement_commit_ambiguity_is_explicit_and_recoverable() {
        let temp = tempfile::tempdir().expect("temp");
        {
            let store = Arc::new(
                IndexedEnvelopeStore::try_new(
                    ambiguous_resources(temp.path(), 2).await,
                    codec(),
                    "edge",
                    NonZeroUsize::new(2).expect("capacity"),
                    RetentionPolicy::Backpressure,
                )
                .expect("store"),
            );
            let mut pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("pipe");
            pipe.sender()
                .send(root("source", 1, &[9]))
                .await
                .expect("first commit accepted");
            let mut receiver = pipe.take_receiver().expect("receiver");
            let ack = receiver
                .receive()
                .await
                .expect("receive")
                .expect("delivery")
                .into_parts()
                .1
                .expect("ack");
            assert!(matches!(
                ack.complete(HandlingOutcome::Handled).await,
                Err(PipeError::AcknowledgementUnknown { .. })
            ));
            drop(receiver);
            drop(pipe);
            store
                .shutdown()
                .await
                .expect("cleanup uncertain acknowledgement");
        }
        let store = open(temp.path()).await;
        let pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("new scope");
        assert_eq!(
            store
                .progress(store.generation())
                .await
                .expect("actual committed progress"),
            1
        );
        drop(pipe);
        store.shutdown().await.expect("cleanup");
    }

    #[derive(Clone, Copy, Debug)]
    enum Fault {
        ReadCheckpoint = 1,
        ReadSequence,
        ReadOutbox,
        Begin,
        Append,
        StageSequence,
        StageCheckpoint,
        BeforeCommit,
        AfterCommit,
        PauseReadCheckpoint,
        PauseReadSequence,
        PauseReadOutbox,
    }

    struct FaultBackend {
        original: ComputationIndexes,
        fault: std::sync::atomic::AtomicUsize,
        entered: tokio::sync::Notify,
        resume: tokio::sync::Notify,
        persistent: bool,
        reads: [std::sync::atomic::AtomicUsize; 3],
    }

    impl FaultBackend {
        fn take(&self, fault: Fault) -> bool {
            self.fault
                .compare_exchange(
                    fault as usize,
                    0,
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
        }

        fn fail(&self, fault: Fault) -> Result<(), IndexError> {
            if self.take(fault) {
                Err(IndexError::IOError)
            } else {
                Ok(())
            }
        }

        fn arm(&self, fault: Fault) {
            self.fault
                .store(fault as usize, std::sync::atomic::Ordering::Release);
        }

        async fn reading(
            &self,
            fault: Fault,
            pause: Fault,
            counter: usize,
        ) -> Result<(), IndexError> {
            self.reads[counter].fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            self.fail(fault)?;
            if self.take(pause) {
                self.entered.notify_one();
                self.resume.notified().await;
            }
            Ok(())
        }

        fn read_counts(&self) -> [usize; 3] {
            [0, 1, 2].map(|index| self.reads[index].load(std::sync::atomic::Ordering::Acquire))
        }

        fn checkpoints(&self) -> &Arc<dyn drasi_core::interface::CheckpointStore> {
            self.original.checkpoint_store().expect("checkpoint")
        }

        fn outbox(&self) -> &Arc<dyn drasi_core::interface::OutboxWriter> {
            self.original.outbox_writer().expect("outbox")
        }

        fn bundle(self: &Arc<Self>, cleanup: bool) -> ComputationIndexes {
            let original = self.original.indexes();
            let control: Arc<dyn SessionControl> = self.clone();
            let domain = TransactionDomain::new(control.clone());
            let indexes = ComputationIndexes::try_new(
                IndexSet {
                    element_index: original.element_index.clone(),
                    archive_index: original.archive_index.clone(),
                    result_index: original.result_index.clone(),
                    future_queue: original.future_queue.clone(),
                    session_control: control,
                },
                Some(domain.clone()),
                Some(ComputationResource::participating(
                    self.clone() as Arc<dyn drasi_core::interface::CheckpointStore>,
                    &domain,
                )),
                Some(ComputationResource::participating(
                    self.clone() as Arc<dyn drasi_core::interface::OutboxWriter>,
                    &domain,
                )),
                Some(ComputationResource::participating(
                    self.original.live_results_writer().expect("live").clone(),
                    &domain,
                )),
            )
            .expect("one explicit transaction domain");
            if cleanup {
                indexes.with_cleanup(self.original.cleanup().expect("cleanup").clone())
            } else {
                indexes
            }
        }
    }

    async fn fault_backend(path: &std::path::Path, persistent: bool) -> Arc<FaultBackend> {
        Arc::new(FaultBackend {
            original: resources(path).await,
            fault: std::sync::atomic::AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
            persistent,
            reads: [0, 0, 0].map(std::sync::atomic::AtomicUsize::new),
        })
    }

    #[async_trait::async_trait]
    impl SessionControl for FaultBackend {
        async fn begin(&self) -> Result<(), IndexError> {
            self.fail(Fault::Begin)?;
            self.original.indexes().session_control.begin().await
        }
        async fn commit(&self) -> Result<(), IndexError> {
            if self.take(Fault::BeforeCommit) {
                self.entered.notify_one();
                self.resume.notified().await;
            }
            self.original.indexes().session_control.commit().await?;
            if self.take(Fault::AfterCommit) {
                self.entered.notify_one();
                self.resume.notified().await;
            }
            Ok(())
        }
        fn rollback(&self) -> Result<(), IndexError> {
            self.original.indexes().session_control.rollback()
        }
    }

    #[async_trait::async_trait]
    impl drasi_core::interface::CheckpointStore for FaultBackend {
        fn is_persistent(&self) -> bool {
            self.persistent && self.checkpoints().is_persistent()
        }
        async fn stage_checkpoint(
            &self,
            key: &str,
            sequence: u64,
            position: Option<&bytes::Bytes>,
        ) -> Result<(), IndexError> {
            self.fail(Fault::StageCheckpoint)?;
            self.checkpoints()
                .stage_checkpoint(key, sequence, position)
                .await
        }
        async fn read_checkpoint(
            &self,
            key: &str,
        ) -> Result<Option<drasi_core::interface::SourceCheckpoint>, IndexError> {
            self.reading(Fault::ReadCheckpoint, Fault::PauseReadCheckpoint, 0)
                .await?;
            self.checkpoints().read_checkpoint(key).await
        }
        async fn read_all_checkpoints(
            &self,
        ) -> Result<
            std::collections::HashMap<String, drasi_core::interface::SourceCheckpoint>,
            IndexError,
        > {
            self.checkpoints().read_all_checkpoints().await
        }
        async fn clear_checkpoints(&self) -> Result<(), IndexError> {
            self.checkpoints().clear_checkpoints().await
        }
        async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
            self.checkpoints().write_config_hash(hash).await
        }
        async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
            self.checkpoints().read_config_hash().await
        }
        async fn stage_result_sequence(&self, key: &str, sequence: u64) -> Result<(), IndexError> {
            self.fail(Fault::StageSequence)?;
            self.checkpoints()
                .stage_result_sequence(key, sequence)
                .await
        }
        async fn read_result_sequence(&self, key: &str) -> Result<Option<u64>, IndexError> {
            self.reading(Fault::ReadSequence, Fault::PauseReadSequence, 1)
                .await?;
            self.checkpoints().read_result_sequence(key).await
        }
    }

    #[async_trait::async_trait]
    impl drasi_core::interface::OutboxWriter for FaultBackend {
        async fn append(&self, key: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
            self.outbox().append(key, sequence, data).await
        }
        async fn read_from(
            &self,
            key: &str,
            after: u64,
        ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
            self.reading(Fault::ReadOutbox, Fault::PauseReadOutbox, 2)
                .await?;
            self.outbox().read_from(key, after).await
        }
        async fn read_latest_sequence(&self, key: &str) -> Result<Option<u64>, IndexError> {
            self.outbox().read_latest_sequence(key).await
        }
        async fn clear(&self, key: &str) -> Result<(), IndexError> {
            self.outbox().clear(key).await
        }
        async fn append_and_trim(
            &self,
            key: &str,
            sequence: u64,
            data: &[u8],
            retain_from: u64,
        ) -> Result<usize, IndexError> {
            self.fail(Fault::Append)?;
            self.outbox()
                .append_and_trim(key, sequence, data, retain_from)
                .await
        }
        async fn trim_before(&self, key: &str, retain_from: u64) -> Result<usize, IndexError> {
            self.outbox().trim_before(key, retain_from).await
        }
        async fn trim_to_capacity(&self, key: &str, capacity: usize) -> Result<usize, IndexError> {
            self.outbox().trim_to_capacity(key, capacity).await
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn durable_journal_reads_once_per_owner_and_keeps_only_its_retained_window() {
        let directory = tempfile::tempdir().expect("directory");
        for reopened in [false, true] {
            let backend = fault_backend(directory.path(), true).await;
            let store = IndexedEnvelopeStore::try_new(
                backend.bundle(true),
                codec(),
                "edge",
                NonZeroUsize::new(32).expect("capacity"),
                RetentionPolicy::Backpressure,
            )
            .expect("store");
            assert_eq!(backend.read_counts(), [0, 0, 0], "construction does no I/O");
            let generation = store.acquire_generation().expect("generation");
            let start = if reopened { 257 } else { 1 };
            for position in start..start + 256 {
                let value = u16::try_from(position).expect("bounded test sequence");
                assert_eq!(
                    store
                        .append(generation, &root("source", position, &[value]))
                        .await
                        .expect("commit"),
                    position
                );
                let next = store
                    .next(generation, position - 1)
                    .await
                    .expect("read")
                    .expect("committed envelope");
                assert_eq!(next.position, position);
                assert_eq!(values(&next.envelope), [value]);
                store.acknowledge(generation, position).await.expect("ack");
                assert_eq!(
                    store.progress(generation).await.expect("progress"),
                    position
                );
            }
            let head = start + 255;
            assert!(matches!(
                store.next(generation, 0).await,
                Err(PipeError::PositionUnavailable { oldest, .. }) if oldest == head - 31
            ));
            assert!(store.next(generation, head).await.expect("end").is_none());
            let replacement = store.acquire_generation().expect("new binding");
            assert_eq!(
                store.progress(replacement).await.expect("cached progress"),
                head
            );
            assert!(matches!(
                store.progress(generation).await,
                Err(PipeError::Closed)
            ));
            assert_eq!(
                backend.read_counts(),
                [1, 1, 1],
                "append, lookup, progress, handling and rebinding never reread the window"
            );
            let persisted = backend
                .outbox()
                .read_from("edge", 0)
                .await
                .expect("disk window");
            assert_eq!(persisted.len(), 32);
            assert_eq!(persisted.first().expect("oldest").0, head - 31);
            assert_eq!(persisted.last().expect("latest").0, head);
            store.shutdown().await.expect("cleanup");
            drop((store, backend));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn durable_cached_pruning_preserves_strict_gaps_across_reconstruction() {
        let directory = tempfile::tempdir().expect("directory");
        for reopened in [false, true] {
            let store = Arc::new(
                IndexedEnvelopeStore::try_new(
                    resources(directory.path()).await,
                    codec(),
                    "edge",
                    NonZeroUsize::new(2).expect("capacity"),
                    RetentionPolicy::PruneOldest,
                )
                .expect("store"),
            );
            let mut pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict).expect("pipe");
            if !reopened {
                for sequence in 1..=5 {
                    pipe.sender()
                        .send(root("source", sequence, &[7]))
                        .await
                        .expect("append");
                }
            }
            let mut receiver = pipe.take_receiver().expect("receiver");
            assert!(matches!(
                receiver.receive().await,
                Err(PipeError::PositionUnavailable {
                    requested: 0,
                    oldest: 4
                })
            ));
            assert_eq!(
                store
                    .progress(store.generation())
                    .await
                    .expect("strict progress"),
                0
            );
            drop((receiver, pipe));
            if reopened {
                let mut pipe =
                    RetainedPipe::new(store.clone(), ReplayGapPolicy::SkipWithNotification)
                        .expect("explicit skip policy");
                let mut receiver = pipe.take_receiver().expect("receiver");
                for sequence in 4..=5 {
                    let delivery = receiver
                        .receive()
                        .await
                        .expect("explicit gap handling")
                        .expect("retained");
                    assert_eq!(delivery.envelope().system().sequence(), sequence);
                    handled(delivery).await;
                }
                assert_eq!(
                    store
                        .progress(store.generation())
                        .await
                        .expect("handled progress"),
                    5
                );
                drop((receiver, pipe));
            }
            store.shutdown().await.expect("cleanup");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn interrupted_journal_load_never_installs_partial_state_or_accepts_input() {
        for (fault, cancelled_counts) in [
            (Fault::PauseReadCheckpoint, [2, 1, 1]),
            (Fault::PauseReadSequence, [2, 2, 1]),
            (Fault::PauseReadOutbox, [2, 2, 2]),
        ] {
            for revoke in [false, true] {
                let directory = tempfile::tempdir().expect("directory");
                let backend = fault_backend(directory.path(), true).await;
                let store = IndexedEnvelopeStore::try_new(
                    backend.bundle(true),
                    codec(),
                    "edge",
                    NonZeroUsize::new(2).expect("capacity"),
                    RetentionPolicy::Backpressure,
                )
                .expect("store");
                let generation = store.acquire_generation().expect("generation");
                backend.arm(fault);
                let input = root("source", 1, &[7]);
                let mut operation = Box::pin(store.append(generation, &input));
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    tokio::select! {
                        _ = backend.entered.notified() => {}
                        result = &mut operation => panic!("read barrier not reached: {result:?}"),
                    }
                })
                .await
                .expect("read entered");
                if revoke {
                    store.revoke_generation(generation);
                    backend.resume.notify_one();
                    assert!(matches!(operation.await, Err(PipeError::Closed)));
                } else {
                    drop(operation);
                }
                assert!(backend
                    .outbox()
                    .read_from("edge", 0)
                    .await
                    .expect("no input")
                    .is_empty());
                let replacement = store
                    .acquire_generation()
                    .expect("read did not fence writes");
                assert_eq!(
                    store.progress(replacement).await.expect("complete state"),
                    0
                );
                assert_eq!(
                    backend.read_counts(),
                    if revoke { [1, 1, 1] } else { cancelled_counts },
                    "{fault:?}: a cancelled partial read must be reconstructed"
                );
                assert_eq!(
                    store
                        .append(replacement, &input)
                        .await
                        .expect("accept once"),
                    1
                );
                store.shutdown().await.expect("cleanup");
            }
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn corrupt_journal_reconstruction_rejects_reads_and_writes_without_changing_storage() {
        for case in [
            "missing-head",
            "head-behind",
            "head-ahead",
            "progress-ahead",
            "missing-tail",
            "missing-middle",
            "empty-window",
            "invalid-envelope",
        ] {
            let directory = tempfile::tempdir().expect("directory");
            let backend = fault_backend(directory.path(), true).await;
            backend
                .original
                .indexes()
                .session_control
                .begin()
                .await
                .expect("begin");
            for position in 1..=3 {
                if case == "empty-window"
                    || case == "missing-tail" && position == 3
                    || case == "missing-middle" && position == 2
                {
                    continue;
                }
                let bytes = if case == "invalid-envelope" && position == 1 {
                    bytes::Bytes::from_static(b"invalid-envelope")
                } else {
                    codec()
                        .encode(&root("source", position, &[7]))
                        .expect("encode")
                };
                backend
                    .outbox()
                    .append("edge", position, &bytes)
                    .await
                    .expect("stage record");
            }
            let head = match case {
                "missing-head" => 0,
                "head-behind" => 2,
                "head-ahead" => 4,
                _ => 3,
            };
            let progress = match case {
                "progress-ahead" => 4,
                "invalid-envelope" => 1,
                _ => 0,
            };
            backend
                .checkpoints()
                .stage_result_sequence("edge", head)
                .await
                .expect("head");
            backend
                .checkpoints()
                .stage_checkpoint("computation-pipe-consumer:edge", progress, None)
                .await
                .expect("progress");
            backend
                .original
                .indexes()
                .session_control
                .commit()
                .await
                .expect("commit");
            let before = backend
                .outbox()
                .read_from("edge", 0)
                .await
                .expect("persisted records");
            let store = IndexedEnvelopeStore::try_new(
                backend.bundle(true),
                codec(),
                "edge",
                NonZeroUsize::new(3).expect("capacity"),
                RetentionPolicy::Backpressure,
            )
            .expect("store");
            let generation = store.acquire_generation().expect("generation");
            assert!(
                matches!(store.progress(generation).await, Err(PipeError::Backend(_))),
                "{case}"
            );
            assert!(
                matches!(store.next(generation, 0).await, Err(PipeError::Backend(_))),
                "{case}"
            );
            assert!(
                matches!(
                    store.append(generation, &root("source", 4, &[8])).await,
                    Err(PipeError::Backend(_))
                ),
                "{case}"
            );
            assert!(
                matches!(
                    store.acknowledge(generation, 3).await,
                    Err(PipeError::Backend(_))
                ),
                "{case}"
            );
            assert_eq!(
                backend
                    .outbox()
                    .read_from("edge", 0)
                    .await
                    .expect("unchanged"),
                before,
                "{case}"
            );
            assert_eq!(
                backend
                    .checkpoints()
                    .read_result_sequence("edge")
                    .await
                    .expect("head"),
                Some(head)
            );
            assert_eq!(
                backend
                    .checkpoints()
                    .read_checkpoint("computation-pipe-consumer:edge")
                    .await
                    .expect("progress")
                    .expect("saved")
                    .sequence,
                progress
            );
            store.shutdown().await.expect("cleanup");
        }
    }

    #[tokio::test]
    async fn durable_storage_failures_preserve_committed_data_and_fence_interrupted_writes() {
        for (operation, fault, fenced) in [
            ("append", Fault::ReadOutbox, false),
            ("append", Fault::ReadSequence, false),
            ("append", Fault::Begin, true),
            ("append", Fault::Append, true),
            ("append", Fault::StageSequence, true),
            ("receive", Fault::ReadOutbox, false),
            ("acknowledge", Fault::ReadCheckpoint, false),
            ("acknowledge", Fault::ReadSequence, false),
            ("acknowledge", Fault::Begin, true),
            ("acknowledge", Fault::StageCheckpoint, true),
        ] {
            let directory = tempfile::tempdir().expect("directory");
            let seed = open(directory.path()).await;
            let generation = seed.acquire_generation().expect("seed generation");
            seed.append(generation, &root("source", 1, &[7]))
                .await
                .expect("committed seed");
            seed.shutdown().await.expect("seed cleanup");
            drop(seed);
            let backend = fault_backend(directory.path(), true).await;
            let store = IndexedEnvelopeStore::try_new(
                backend.bundle(true),
                codec(),
                "edge",
                NonZeroUsize::new(2).expect("capacity"),
                RetentionPolicy::Backpressure,
            )
            .expect("store");
            let generation = store.acquire_generation().expect("generation");
            backend.arm(fault);
            let result = match operation {
                "append" => store
                    .append(generation, &root("source", 2, &[8]))
                    .await
                    .map(|_| ()),
                "receive" => store.next(generation, 0).await.map(|_| ()),
                "acknowledge" => store.acknowledge(generation, 1).await,
                _ => unreachable!("declared operations"),
            };
            assert!(
                matches!(result, Err(PipeError::Backend(_))),
                "{operation}/{fault:?}: {result:?}"
            );
            assert_eq!(
                backend.fault.load(std::sync::atomic::Ordering::Acquire),
                0,
                "fault was exercised"
            );
            if fenced {
                assert!(store
                    .progress(generation)
                    .await
                    .expect_err("fenced owner")
                    .to_string()
                    .contains("cleanup and recovery"));
            } else {
                assert_eq!(
                    store
                        .progress(generation)
                        .await
                        .expect("read failure did not mutate"),
                    0
                );
            }
            store.shutdown().await.expect("rollback and cleanup");
            drop((store, backend));
            let reopened = open(directory.path()).await;
            let generation = reopened.acquire_generation().expect("new generation");
            assert_eq!(
                reopened.progress(generation).await.expect("unhandled seed"),
                0
            );
            let first = reopened
                .next(generation, 0)
                .await
                .expect("read")
                .expect("seed");
            assert_eq!(first.position, 1);
            assert_eq!(values(&first.envelope), [7]);
            assert!(reopened
                .next(generation, 1)
                .await
                .expect("no uncommitted output")
                .is_none());
            reopened
                .acknowledge(generation, 1)
                .await
                .expect("retry handling");
            reopened
                .append(generation, &root("source", 2, &[8]))
                .await
                .expect("retry publish");
            reopened.shutdown().await.expect("cleanup");
        }
    }

    #[tokio::test]
    async fn durable_generation_revocation_during_commit_never_reports_definite_success() {
        for acknowledgement in [false, true] {
            for fault in [Fault::BeforeCommit, Fault::AfterCommit] {
                let directory = tempfile::tempdir().expect("directory");
                let backend = fault_backend(directory.path(), true).await;
                let store = IndexedEnvelopeStore::try_new(
                    backend.bundle(true),
                    codec(),
                    "edge",
                    NonZeroUsize::new(2).expect("capacity"),
                    RetentionPolicy::Backpressure,
                )
                .expect("store");
                let generation = store.acquire_generation().expect("generation");
                let first = root("source", 1, &[7]);
                if acknowledgement {
                    store.append(generation, &first).await.expect("seed");
                }
                backend.arm(fault);
                let mut operation = Box::pin(async {
                    if acknowledgement {
                        store.acknowledge(generation, 1).await
                    } else {
                        store.append(generation, &first).await.map(|_| ())
                    }
                });
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    tokio::select! {
                        _ = backend.entered.notified() => {}
                        result = &mut operation => panic!("commit barrier not reached: {result:?}"),
                    }
                })
                .await
                .expect("commit entered");
                assert!(store
                    .acquire_generation()
                    .expect_err("exclusive operation")
                    .to_string()
                    .contains("in use"));
                assert!(store
                    .shutdown()
                    .await
                    .expect_err("must not race cleanup")
                    .to_string()
                    .contains("before shutdown"));
                store.revoke_generation(generation);
                backend.resume.notify_one();
                let error = tokio::time::timeout(std::time::Duration::from_secs(5), operation)
                    .await
                    .expect("commit returned")
                    .expect_err("revoked owner");
                if acknowledgement {
                    assert!(matches!(error, PipeError::AcknowledgementUnknown { .. }));
                } else {
                    assert!(matches!(error, PipeError::AcceptanceUnknown { .. }));
                }
                assert!(store
                    .acquire_generation()
                    .expect_err("fenced")
                    .to_string()
                    .contains("cleanup and recovery"));
                store.shutdown().await.expect("cleanup");
                drop((store, backend));
                let reopened = open(directory.path()).await;
                let generation = reopened.acquire_generation().expect("recovered generation");
                assert_eq!(
                    reopened
                        .progress(generation)
                        .await
                        .expect("durable progress"),
                    u64::from(acknowledgement)
                );
                assert_eq!(
                    values(
                        &reopened
                            .next(generation, 0)
                            .await
                            .expect("read")
                            .expect("committed")
                            .envelope
                    ),
                    [7]
                );
                assert!(reopened
                    .next(generation, 1)
                    .await
                    .expect("one committed record")
                    .is_none());
                reopened.shutdown().await.expect("cleanup");
            }
        }
    }

    #[tokio::test]
    async fn durable_store_rejects_invalid_providers_payloads_positions_and_exhausted_sequences() {
        for (cleanup, persistent, expected) in
            [(false, true, "cleanup owner"), (true, false, "persistent storage")]
        {
            let directory = tempfile::tempdir().expect("directory");
            let backend = fault_backend(directory.path(), persistent).await;
            let result = IndexedEnvelopeStore::try_new(
                backend.bundle(cleanup),
                codec(),
                "edge",
                NonZeroUsize::new(2).expect("capacity"),
                RetentionPolicy::Backpressure,
            );
            assert!(matches!(result, Err(error) if error.to_string().contains(expected)));
            backend
                .original
                .cleanup()
                .expect("owner")
                .shutdown()
                .await
                .expect("cleanup rejected bundle");
        }
        let directory = tempfile::tempdir().expect("directory");
        let backend = fault_backend(directory.path(), true).await;
        let store = IndexedEnvelopeStore::try_new(
            backend.bundle(true),
            codec(),
            "edge",
            NonZeroUsize::new(2).expect("capacity"),
            RetentionPolicy::Backpressure,
        )
        .expect("store");
        let generation = store.acquire_generation().expect("generation");
        store
            .append(generation, &root("source", 1, &[7]))
            .await
            .expect("seed");
        assert!(store
            .acknowledge(generation, 2)
            .await
            .expect_err("future progress")
            .to_string()
            .contains("acknowledgement position"));
        store.acknowledge(generation, 1).await.expect("handled");
        assert!(store
            .acknowledge(generation, 0)
            .await
            .expect_err("regression")
            .to_string()
            .contains("acknowledgement position"));
        store
            .shutdown()
            .await
            .expect("cleanup before storage mutation");
        drop((store, backend));
        let backend = fault_backend(directory.path(), true).await;
        backend
            .original
            .indexes()
            .session_control
            .begin()
            .await
            .expect("inject persisted boundary");
        backend
            .checkpoints()
            .stage_result_sequence("edge", u64::MAX)
            .await
            .expect("max sequence");
        backend
            .outbox()
            .append_and_trim(
                "edge",
                u64::MAX,
                &codec()
                    .encode(&root("source", 1, &[7]))
                    .expect("valid envelope"),
                u64::MAX,
            )
            .await
            .expect("last possible position");
        backend
            .original
            .indexes()
            .session_control
            .commit()
            .await
            .expect("commit boundary");
        let store = IndexedEnvelopeStore::try_new(
            backend.bundle(true),
            codec(),
            "edge",
            NonZeroUsize::new(2).expect("capacity"),
            RetentionPolicy::Backpressure,
        )
        .expect("reconstruct boundary");
        let generation = store.acquire_generation().expect("generation");
        assert!(store
            .next(generation, u64::MAX)
            .await
            .expect("end")
            .is_none());
        assert!(store
            .append(generation, &root("source", 2, &[8]))
            .await
            .expect_err("no wrap")
            .to_string()
            .contains("position exhausted"));
        assert_eq!(
            store
                .progress(generation)
                .await
                .expect("unchanged progress"),
            1
        );
        store.shutdown().await.expect("cleanup");

        let directory = tempfile::tempdir().expect("directory");
        let mut tiny_codec = EnvelopeCodec::new(NonZeroUsize::new(1).expect("one byte"));
        tiny_codec
            .register_schema(Arc::new(Schema::new(
                schema_descriptor(),
                Arc::new(ReadingValidator(schema_descriptor())),
            )))
            .expect("schema");
        let store = IndexedEnvelopeStore::try_new(
            resources(directory.path()).await,
            Arc::new(tiny_codec),
            "edge",
            NonZeroUsize::new(2).expect("capacity"),
            RetentionPolicy::Backpressure,
        )
        .expect("store");
        let generation = store.acquire_generation().expect("generation");
        assert!(matches!(
            store.append(generation, &root("source", 1, &[7])).await,
            Err(PipeError::Backend(_))
        ));
        assert_eq!(store.progress(generation).await.expect("not accepted"), 0);
        assert!(store.next(generation, 0).await.expect("empty").is_none());
        store.shutdown().await.expect("cleanup");
    }
}
