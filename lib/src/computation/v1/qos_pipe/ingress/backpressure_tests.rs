// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationResource, TransactionDomain},
    interface::{FailureMode, IndexError, IndexSet, SessionControl},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use std::sync::atomic::AtomicU8;
use tokio::sync::watch;

const SOURCE_ID: &str = "04f61123-59aa-4055-a1c3-6514e8eab61e";

#[derive(Default)]
struct CommitGate {
    mode: AtomicU8,
    entered: Notify,
    release: Notify,
}

struct GatedSession {
    inner: Arc<dyn SessionControl>,
    gate: Arc<CommitGate>,
}

#[async_trait]
impl SessionControl for GatedSession {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }

    async fn commit(&self) -> Result<(), IndexError> {
        let mode = self.gate.mode.swap(0, Ordering::SeqCst);
        if mode == 1 {
            self.gate.entered.notify_one();
            self.gate.release.notified().await;
        }
        self.inner.commit().await?;
        if mode == 2 {
            return Err(IndexError::IOError);
        }
        Ok(())
    }

    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

struct Fixture {
    service: Arc<SourceAdmission>,
    session: ProducerSession,
    gate: Arc<CommitGate>,
    directory: tempfile::TempDir,
}

impl Fixture {
    async fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let gate = Arc::new(CommitGate::default());
        let channel = Self::open(directory.path(), gate.clone()).await;
        let session = channel
            .register_producer(ComponentId::generate())
            .await
            .unwrap();
        let service = SourceAdmission::new(channel, PortId::try_new("out").unwrap())
            .await
            .unwrap();
        Self {
            service,
            session,
            gate,
            directory,
        }
    }

    async fn open(path: &std::path::Path, gate: Arc<CommitGate>) -> Arc<QosChannel> {
        let original = LegacyIndexProviderAdapter::new(Arc::new(
            RocksDbIndexProvider::new(path, false, false)
                .with_memory_budget_bytes(32 << 20)
                .unwrap(),
        ))
        .create_indexes("graph", SOURCE_ID)
        .await
        .unwrap();
        let set = original.indexes();
        let control: Arc<dyn SessionControl> = Arc::new(GatedSession {
            inner: set.session_control.clone(),
            gate,
        });
        let domain = TransactionDomain::new(control.clone());
        let indexes = ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: control,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                original.checkpoint_store().unwrap().clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.outbox_writer().unwrap().clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.live_results_writer().unwrap().clone(),
                &domain,
            )),
        )
        .unwrap()
        .with_cleanup(original.cleanup().unwrap().clone())
        .with_durability(original.durability());
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(65536).unwrap());
        codec.register_schema(GraphChangeCodec::schema()).unwrap();
        QosChannel::persistent_with_recovery(
            QosChannelDefinition {
                stream: StreamId::try_new("source/out").unwrap(),
                capacity: NonZeroUsize::new(1).unwrap(),
                durable: true,
                retention: RetentionPolicy::Backpressure,
                subscribers: BTreeMap::from([("consumer".into(), SubscriptionStart::Earliest)]),
            },
            indexes,
            codec,
            "journal",
            QosRecoveryOptions::Admission(AdmissionOptions {
                construction_scope: "scope".into(),
                graph_id: "graph".into(),
                component_id: ComponentId::try_new(SOURCE_ID).unwrap(),
                failure_scope: FailureMode::ProcessRestart,
                max_producers: NonZeroUsize::new(2).unwrap(),
                receipts_per_producer: NonZeroUsize::new(4).unwrap(),
            }),
        )
        .await
        .unwrap()
    }

    async fn acknowledge(&self) {
        let mut pipe = self
            .service
            .channel
            .bind("consumer", ReplayGapPolicy::Strict)
            .unwrap();
        pipe.pipe
            .take_receiver()
            .unwrap()
            .receive()
            .await
            .unwrap()
            .unwrap()
            .into_parts()
            .1
            .unwrap()
            .complete(HandlingOutcome::Handled)
            .await
            .unwrap();
        pipe.control.cancel();
    }

    async fn fill(&self) {
        self.service
            .channel
            .admit(&self.session, 1, &input(1))
            .await
            .unwrap();
    }
}

fn input(sequence: u64) -> ChangeEnvelope {
    GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &sequence.to_string()),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::default(),
            },
        },
        StreamId::try_new("source/out").unwrap(),
        sequence,
        None,
    )
    .unwrap()
}

fn rejected(error: PipeError, expected: AdmissionRejection) {
    let PipeError::Backend(error) = error else {
        panic!("unexpected error: {error}");
    };
    assert_eq!(error.downcast_ref::<AdmissionRejection>(), Some(&expected));
}

#[tokio::test]
async fn waits_for_cursor_commit_contention_while_admit_stays_fail_fast() {
    let fixture = Fixture::new().await;
    fixture.fill().await;
    let service = &fixture.service;
    let _activation = service.begin_activation().unwrap();
    let mut lease = service.bind().unwrap();
    let (_pause, mut quiesce) = watch::channel(false);
    fixture.gate.mode.store(1, Ordering::SeqCst);
    let acknowledgement = fixture.acknowledge();
    tokio::pin!(acknowledgement);
    tokio::select! {
        biased;
        _ = &mut acknowledgement => panic!("cursor commit must be gated"),
        _ = fixture.gate.entered.notified() => {},
    }
    let input = input(2);
    let (result, processed) = tokio::join!(
        service.admit(&fixture.session, 2, &input),
        lease.process(&mut quiesce, |_| Ok(())),
    );
    rejected(result.unwrap_err(), AdmissionRejection::Busy);
    assert_eq!(processed.unwrap(), Some(false));
    let waiting = service.admit_wait(&fixture.session, 2, &input);
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    let mut process = Box::pin(lease.process(&mut quiesce, |_| Ok(())));
    assert!(futures::poll!(&mut process).is_pending());
    assert!(futures::poll!(&mut waiting).is_pending());
    fixture.gate.release.notify_one();
    acknowledgement.await;
    assert_eq!(process.await.unwrap(), Some(true));
    assert_eq!(waiting.await.unwrap().position, 2);
    assert_eq!(
        service
            .channel
            .producer_status(&fixture.session)
            .await
            .unwrap()
            .next_sequence,
        Some(3)
    );
    drop(lease);
    service.channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn capacity_change_before_rejection_is_observed_does_not_lose_wakeup() {
    let fixture = Fixture::new().await;
    fixture.fill().await;
    let service = &fixture.service;
    let _activation = service.begin_activation().unwrap();
    let mut lease = service.bind().unwrap();
    let (_pause, mut quiesce) = watch::channel(false);
    let input = input(2);
    let waiting = service.admit_wait(&fixture.session, 2, &input);
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    assert_eq!(
        lease.process(&mut quiesce, |_| Ok(())).await.unwrap(),
        Some(false)
    );
    // The rejection is in the reply slot, but the caller has not observed it.
    fixture.acknowledge().await;
    assert!(futures::poll!(&mut waiting).is_pending());
    assert_eq!(
        lease.process(&mut quiesce, |_| Ok(())).await.unwrap(),
        Some(true)
    );
    assert_eq!(waiting.await.unwrap().position, 2);
    drop(lease);
    service.channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn capacity_notification_wakes_all_waiters_without_duplicate_admission() {
    let fixture = Fixture::new().await;
    fixture.fill().await;
    let service = &fixture.service;
    let _activation = service.begin_activation().unwrap();
    let mut lease = service.bind().unwrap();
    let (_pause, mut quiesce) = watch::channel(false);
    let input = input(2);
    let first = service.admit_wait(&fixture.session, 2, &input);
    let second = service.admit_wait(&fixture.session, 2, &input);
    tokio::pin!(first, second);
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    for _ in 0..2 {
        assert_eq!(
            lease.process(&mut quiesce, |_| Ok(())).await.unwrap(),
            Some(false)
        );
    }
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    fixture.acknowledge().await;
    assert!(futures::poll!(&mut first).is_pending());
    assert!(futures::poll!(&mut second).is_pending());
    for _ in 0..2 {
        assert_eq!(
            lease.process(&mut quiesce, |_| Ok(())).await.unwrap(),
            Some(true)
        );
    }
    assert_eq!(first.await.unwrap(), second.await.unwrap());
    assert_eq!(service.channel.progress().await.unwrap().accepted, 2);
    drop(lease);
    service.channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn capacity_wait_is_quiet_and_cancelled_wait_does_not_consume_sequence() {
    let fixture = Fixture::new().await;
    fixture.fill().await;
    let service = &fixture.service;
    let _activation = service.begin_activation().unwrap();
    let mut lease = service.bind().unwrap();
    let (_pause, mut quiesce) = watch::channel(false);
    let input = input(2);
    {
        let waiting = service.admit_wait(&fixture.session, 2, &input);
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        assert_eq!(
            lease.process(&mut quiesce, |_| Ok(())).await.unwrap(),
            Some(false)
        );
        assert!(futures::poll!(&mut waiting).is_pending());
        assert!(matches!(
            lease.requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }
    fixture.acknowledge().await;
    assert!(matches!(
        lease.requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert!(service
        .channel
        .admission_receipt(&fixture.session, 2)
        .await
        .unwrap()
        .is_none());
    let (result, processed) = tokio::join!(
        service.admit_wait(&fixture.session, 2, &input),
        lease.process(&mut quiesce, |_| Ok(())),
    );
    assert_eq!(processed.unwrap(), Some(true));
    assert_eq!(result.unwrap().position, 2);
    drop(lease);
    service.channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn mailbox_backpressure_and_cancellation_remain_bounded() {
    let fixture = Fixture::new().await;
    let service = &fixture.service;
    let _activation = service.begin_activation().unwrap();
    let mut lease = service.bind().unwrap();
    let sender = service.binding.lock().unwrap().requests.clone().unwrap();
    let permits = (0..PENDING_REQUESTS)
        .map(|_| sender.clone().try_reserve_owned().unwrap())
        .collect::<Vec<_>>();
    let input = input(1);
    rejected(
        service
            .admit(&fixture.session, 1, &input)
            .await
            .unwrap_err(),
        AdmissionRejection::Busy,
    );
    {
        let cancelled = service.admit_wait(&fixture.session, 1, &input);
        tokio::pin!(cancelled);
        assert!(futures::poll!(&mut cancelled).is_pending());
    }
    let waiting = service.admit_wait(&fixture.session, 1, &input);
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    drop(permits);
    assert!(futures::poll!(&mut waiting).is_pending());
    let (_pause, mut quiesce) = watch::channel(false);
    assert_eq!(
        lease.process(&mut quiesce, |_| Ok(())).await.unwrap(),
        Some(true)
    );
    assert_eq!(waiting.await.unwrap().position, 1);
    assert!(matches!(
        lease.requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    drop(lease);
    service.channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelling_queued_or_mutex_waiting_request_never_admits_it() {
    for executing in [false, true] {
        let fixture = Fixture::new().await;
        let service = &fixture.service;
        let _activation = service.begin_activation().unwrap();
        let mut lease = service.bind().unwrap();
        let (_pause, mut quiesce) = watch::channel(false);
        let input = input(1);
        let lock = service.channel.state.lock().await;
        let mut waiting = Box::pin(service.admit_wait(&fixture.session, 1, &input));
        assert!(futures::poll!(&mut waiting).is_pending());
        let mut processing =
            Box::pin(lease.process(&mut quiesce, |_| panic!("cancelled input validated")));
        if executing {
            assert!(futures::poll!(&mut processing).is_pending());
        }
        drop(waiting);
        assert_eq!(processing.await.unwrap(), Some(false));
        drop(lock);
        assert!(service
            .channel
            .admission_receipt(&fixture.session, 1)
            .await
            .unwrap()
            .is_none());
        drop(lease);
        service.channel.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn capacity_wait_is_fenced_by_stop_even_if_immediately_restarted() {
    let fixture = Fixture::new().await;
    fixture.fill().await;
    let service = &fixture.service;
    let activation = service.begin_activation().unwrap();
    let mut lease = service.bind().unwrap();
    let (_pause, mut quiesce) = watch::channel(false);
    let input = input(2);
    let waiting = service.admit_wait(&fixture.session, 2, &input);
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    lease.process(&mut quiesce, |_| Ok(())).await.unwrap();
    assert!(futures::poll!(&mut waiting).is_pending());
    drop(lease);
    drop(activation);
    let _replacement = service.begin_activation().unwrap();
    let replacement = service.bind().unwrap();
    fixture.acknowledge().await;
    assert!(matches!(waiting.await, Err(PipeError::Closed)));
    assert!(service
        .channel
        .admission_receipt(&fixture.session, 2)
        .await
        .unwrap()
        .is_none());
    drop(replacement);
    service.channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn closed_channel_wakes_mailbox_and_capacity_waiters() {
    for full_mailbox in [false, true] {
        let fixture = Fixture::new().await;
        fixture.fill().await;
        let service = &fixture.service;
        let _activation = service.begin_activation().unwrap();
        let mut lease = service.bind().unwrap();
        let sender = service.binding.lock().unwrap().requests.clone().unwrap();
        let permits = if full_mailbox {
            (0..PENDING_REQUESTS)
                .map(|_| sender.clone().try_reserve_owned().unwrap())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let (_pause, mut quiesce) = watch::channel(false);
        let input = input(2);
        let waiting = service.admit_wait(&fixture.session, 2, &input);
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        if !full_mailbox {
            lease.process(&mut quiesce, |_| Ok(())).await.unwrap();
            assert!(futures::poll!(&mut waiting).is_pending());
        }
        service.channel.shutdown().await.unwrap();
        assert!(matches!(waiting.await, Err(PipeError::Closed)));
        assert!(matches!(
            lease.requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        drop(permits);
    }
}

#[tokio::test]
async fn channel_close_wakes_mutex_waiter_before_mutex_is_released() {
    let fixture = Fixture::new().await;
    let service = &fixture.service;
    let _activation = service.begin_activation().unwrap();
    let mut lease = service.bind().unwrap();
    let (_pause, mut quiesce) = watch::channel(false);
    let input = input(1);
    let lock = service.channel.state.lock().await;
    let waiting = service.admit_wait(&fixture.session, 1, &input);
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    let processing = lease.process(&mut quiesce, |_| panic!("closed input validated"));
    tokio::pin!(processing);
    assert!(futures::poll!(&mut processing).is_pending());
    let shutdown = service.channel.shutdown();
    tokio::pin!(shutdown);
    assert!(futures::poll!(&mut shutdown).is_pending());
    assert!(matches!(processing.await, Err(PipeError::Closed)));
    assert!(matches!(waiting.await, Err(PipeError::Closed)));
    drop(lock);
    shutdown.await.unwrap();
}

#[tokio::test]
async fn full_journal_still_returns_duplicate_receipt_and_payload_conflict() {
    let fixture = Fixture::new().await;
    fixture.fill().await;
    let service = &fixture.service;
    let _activation = service.begin_activation().unwrap();
    let mut lease = service.bind().unwrap();
    let (_pause, mut quiesce) = watch::channel(false);
    for sequence in [1, 2] {
        let input = input(sequence);
        let (result, processed) = tokio::join!(
            service.admit_wait(&fixture.session, 1, &input),
            lease.process(&mut quiesce, |_| panic!(
                "retry must not revalidate or append"
            )),
        );
        if sequence == 1 {
            assert_eq!(result.unwrap().position, 1);
            assert_eq!(processed.unwrap(), Some(true));
        } else {
            rejected(result.unwrap_err(), AdmissionRejection::PayloadConflict(1));
            assert_eq!(processed.unwrap(), Some(false));
        }
    }
    assert_eq!(service.channel.progress().await.unwrap().accepted, 1);
    drop(lease);
    service.channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn uncertain_commit_is_not_retried_and_receipt_survives_reconstruction() {
    let fixture = Fixture::new().await;
    let service = &fixture.service;
    let _activation = service.begin_activation().unwrap();
    let mut lease = service.bind().unwrap();
    let (_pause, mut quiesce) = watch::channel(false);
    fixture.gate.mode.store(2, Ordering::SeqCst);
    let input = input(1);
    let (result, processed) = tokio::join!(
        service.admit_wait(&fixture.session, 1, &input),
        lease.process(&mut quiesce, |_| Ok(())),
    );
    assert!(matches!(result, Err(PipeError::AcceptanceUnknown { .. })));
    assert!(processed.is_err());
    assert!(service
        .admit_wait(&fixture.session, 1, &input)
        .await
        .is_err());
    assert!(matches!(
        lease.requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    drop(lease);
    service.channel.shutdown().await.unwrap();
    let reopened = Fixture::open(fixture.directory.path(), fixture.gate.clone()).await;
    assert_eq!(
        reopened
            .admission_receipt(&fixture.session, 1)
            .await
            .unwrap()
            .unwrap()
            .position,
        1
    );
    assert_eq!(
        reopened
            .admit(&fixture.session, 1, &input)
            .await
            .unwrap()
            .position,
        1
    );
    assert_eq!(reopened.progress().await.unwrap().accepted, 1);
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn capacity_wait_wakes_on_producer_retirement_and_worker_failure() {
    for retire in [true, false] {
        let fixture = Fixture::new().await;
        let service = &fixture.service;
        let other = service
            .channel
            .register_producer(ComponentId::generate())
            .await
            .unwrap();
        service.channel.admit(&other, 1, &input(1)).await.unwrap();
        let _activation = service.begin_activation().unwrap();
        let mut lease = service.bind().unwrap();
        let (_pause, mut quiesce) = watch::channel(false);
        let input = input(2);
        let waiting = service.admit_wait(&fixture.session, 1, &input);
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        assert_eq!(
            lease.process(&mut quiesce, |_| Ok(())).await.unwrap(),
            Some(false)
        );
        assert!(futures::poll!(&mut waiting).is_pending());
        if retire {
            service
                .channel
                .retire_producer(&fixture.session)
                .await
                .unwrap();
            assert!(futures::poll!(&mut waiting).is_pending());
            lease
                .process(&mut quiesce, |_| panic!("retired producer validated"))
                .await
                .unwrap();
            rejected(
                waiting.await.unwrap_err(),
                AdmissionRejection::SessionExpired,
            );
        } else {
            service.report_failure("producer worker failed").unwrap();
            assert!(waiting
                .await
                .unwrap_err()
                .to_string()
                .contains("producer worker failed"));
        }
        drop(lease);
        service.channel.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn capacity_wait_wakes_on_transaction_fence_without_retrying() {
    for cancel in [false, true] {
        let fixture = Fixture::new().await;
        fixture.fill().await;
        let service = &fixture.service;
        let _activation = service.begin_activation().unwrap();
        let mut lease = service.bind().unwrap();
        let (_pause, mut quiesce) = watch::channel(false);
        let input = input(2);
        let waiting = service.admit_wait(&fixture.session, 2, &input);
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        lease.process(&mut quiesce, |_| Ok(())).await.unwrap();
        assert!(futures::poll!(&mut waiting).is_pending());
        // Producer metadata uses the same transaction and can fence a full journal.
        fixture
            .gate
            .mode
            .store(if cancel { 1 } else { 2 }, Ordering::SeqCst);
        let mut registration = Box::pin(service.channel.register_producer(ComponentId::generate()));
        if cancel {
            tokio::select! {
                biased;
                result = &mut registration => panic!("registration must be gated: {result:?}"),
                _ = fixture.gate.entered.notified() => {},
            }
            drop(registration);
        } else {
            assert!(matches!(
                registration.await,
                Err(PipeError::AcknowledgementUnknown { .. })
            ));
        }
        assert!(matches!(
            futures::poll!(&mut waiting),
            std::task::Poll::Ready(Err(_))
        ));
        assert!(matches!(
            lease.requests.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        drop(lease);
        service.channel.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn successful_registration_does_not_wake_capacity_waiters() {
    let fixture = Fixture::new().await;
    fixture.fill().await;
    let service = &fixture.service;
    let _activation = service.begin_activation().unwrap();
    let mut lease = service.bind().unwrap();
    let (_pause, mut quiesce) = watch::channel(false);
    let input = input(2);
    let waiting = service.admit_wait(&fixture.session, 2, &input);
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    lease.process(&mut quiesce, |_| Ok(())).await.unwrap();
    assert!(futures::poll!(&mut waiting).is_pending());
    service
        .channel
        .register_producer(ComponentId::generate())
        .await
        .unwrap();
    assert!(futures::poll!(&mut waiting).is_pending());
    assert!(matches!(
        lease.requests.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    drop(lease);
    assert!(matches!(waiting.await, Err(PipeError::Closed)));
    service.channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn quiescing_mutex_waiter_returns_closed_without_consuming_input() {
    let fixture = Fixture::new().await;
    let service = &fixture.service;
    let _activation = service.begin_activation().unwrap();
    let mut lease = service.bind().unwrap();
    let (pause, mut quiesce) = watch::channel(false);
    let input = input(1);
    let lock = service.channel.state.lock().await;
    let waiting = service.admit_wait(&fixture.session, 1, &input);
    tokio::pin!(waiting);
    assert!(futures::poll!(&mut waiting).is_pending());
    let processing = lease.process(&mut quiesce, |_| panic!("quiesced input validated"));
    tokio::pin!(processing);
    assert!(futures::poll!(&mut processing).is_pending());
    pause.send(true).unwrap();
    assert_eq!(processing.await.unwrap(), None);
    assert!(matches!(waiting.await, Err(PipeError::Closed)));
    drop(lock);
    assert!(service
        .channel
        .admission_receipt(&fixture.session, 1)
        .await
        .unwrap()
        .is_none());
    service.channel.shutdown().await.unwrap();
}
