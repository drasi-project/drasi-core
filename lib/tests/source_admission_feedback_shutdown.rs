// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Coordinated feedback shutdown and durable recovery, without timing sleeps.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::{collections::BTreeMap, num::NonZeroUsize, path::Path, sync::Arc, time::Duration};

use async_trait::async_trait;
use drasi_core::{
    computation::ComputationIndexProvider,
    interface::FailureMode,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{computation::v1::*, DrasiLib};
use tokio::sync::{mpsc, Semaphore};

const SOURCE: &str = "90f602e4-6310-47d2-b9d9-e774629624c1";
const SINK: &str = "2aa60a0d-8a91-494f-aa5f-dd44ee08d5fc";
const CLIENT: &str = "06ff2239-dcf0-4b6a-9790-a2bd716f1e2d";
const INSTANCE: &str = "feedback-shutdown-repro";
const GRAPH: &str = "__drasi_lib_runtime__";
const DEADLINE: Duration = Duration::from_secs(20);

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("component UUID")
}

fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("port")
}

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("nonzero size")
}

fn descriptor(value: &str, direction: PortDirection) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        id(value),
        vec![PortDescriptor::new(
            port(if direction == PortDirection::Output {
                "out"
            } else {
                "in"
            }),
            direction,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("descriptor")
}

fn input(sequence: u64) -> ChangeEnvelope {
    GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("fixture", &sequence.to_string()),
                    labels: vec!["Item".into()].into(),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::new(),
            },
        },
        StreamId::try_new("source/out").expect("stream"),
        sequence,
        None,
    )
    .expect("input envelope")
}

async fn open_channel(path: &Path, capacity: usize) -> Arc<QosChannel> {
    let provider = LegacyIndexProviderAdapter::scoped(
        Arc::new(
            RocksDbIndexProvider::new(path, false, false)
                .with_memory_budget_bytes(32 << 20)
                .expect("bounded storage"),
        ),
        INSTANCE,
    )
    .expect("scoped provider");
    let mut codec = EnvelopeCodec::new(size(65536));
    codec
        .register_schema(GraphChangeCodec::schema())
        .expect("schema");
    QosChannel::persistent_with_recovery(
        QosChannelDefinition {
            stream: StreamId::try_new("source/out").expect("stream"),
            capacity: size(capacity),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("sink".into(), SubscriptionStart::Earliest)]),
        },
        provider
            .create_indexes(GRAPH, &id(SOURCE))
            .await
            .expect("indexes"),
        codec,
        "journal",
        QosRecoveryOptions::Admission(AdmissionOptions {
            construction_scope: INSTANCE.into(),
            graph_id: GRAPH.into(),
            component_id: id(SOURCE),
            failure_scope: FailureMode::ProcessRestart,
            max_producers: size(1),
            receipts_per_producer: size(4),
        }),
    )
    .await
    .expect("persistent admission channel")
}

struct Source {
    descriptor: ComponentDescriptor,
    admission: Arc<SourceAdmission>,
}

#[async_trait]
impl ComputationComponent for Source {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSource for Source {
    fn admission(&self) -> Option<Arc<SourceAdmission>> {
        Some(self.admission.clone())
    }
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        anyhow::bail!("publication belongs to SourceAdmission")
    }
}

#[derive(Debug)]
enum Event {
    CallbackEntered,
    FeedbackAccepted,
    FeedbackClosed,
    FeedbackHandled,
    FeedbackAttempt,
}

struct Sink {
    descriptor: ComponentDescriptor,
    admission: Arc<SourceAdmission>,
    session: ProducerSession,
    release: Arc<Semaphore>,
    events: mpsc::UnboundedSender<Event>,
    coordinated: bool,
}

struct AcknowledgingSink {
    descriptor: ComponentDescriptor,
    entered: mpsc::UnboundedSender<u64>,
    release: Arc<Semaphore>,
    batched: bool,
}

struct PendingTimer {
    pending: Semaphore,
    handled: AtomicUsize,
}

impl Default for PendingTimer {
    fn default() -> Self {
        Self {
            pending: Semaphore::new(0),
            handled: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl WakeupSource for PendingTimer {
    async fn wait(&self) -> anyhow::Result<()> {
        self.pending.acquire().await?.forget();
        Ok(())
    }
    async fn has_pending(&self) -> anyhow::Result<bool> {
        Ok(true)
    }
}

struct ScheduledRelay {
    descriptor: ComponentDescriptor,
    timer: Arc<PendingTimer>,
}

#[async_trait]
impl ComputationComponent for ScheduledRelay {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl Transformer for ScheduledRelay {
    fn wakeup_source(&self) -> Option<Arc<dyn WakeupSource>> {
        Some(self.timer.clone())
    }
    async fn on_wakeup(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.timer.handled.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        Ok(vec![OutputEnvelope {
            port: port("out"),
            envelope: input.envelope.derive(
                input.envelope.id().clone(),
                input.envelope.changes().clone(),
                SystemMetadata::new(
                    StreamId::try_new("relay/out")?,
                    input.envelope.system().sequence(),
                ),
            ),
        }])
    }
}

#[async_trait]
impl ComputationComponent for AcknowledgingSink {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSink for AcknowledgingSink {
    fn batching(&self) -> anyhow::Result<Option<SinkBatching>> {
        Ok(self.batched.then_some(SinkBatching {
            max_inputs: 4,
            max_operations: 4,
            max_bytes: 65536,
            max_delay_ms: 60_000,
        }))
    }
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> anyhow::Result<()> {
        for input in inputs {
            self.handle(input).await?;
        }
        Ok(())
    }
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let sequence = input.envelope.system().sequence();
        self.entered.send(sequence)?;
        if sequence == 1 {
            self.release.acquire().await?.forget();
        }
        Ok(())
    }
}

#[async_trait]
impl ComputationComponent for Sink {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, received: InputEnvelope) -> anyhow::Result<()> {
        if received.envelope.system().sequence() == 1 {
            self.events.send(Event::CallbackEntered)?;
            assert_eq!(self.admission.in_feedback_context(), self.coordinated);
            let spawned = self.admission.clone();
            assert!(
                !tokio::spawn(async move { spawned.in_feedback_context() }).await?,
                "spawned workers are not processing callbacks"
            );
            self.release.acquire().await?.forget();
            self.events.send(Event::FeedbackAttempt)?;
            let result = async {
                if self.coordinated {
                    let feedback = self.admission.feedback();
                    feedback.wait_active().await?;
                    let session = feedback.register_producer(id(CLIENT)).await?;
                    assert_eq!(session, self.session);
                    assert_eq!(
                        feedback.producer_status(&session).await?.next_sequence,
                        Some(2)
                    );
                    assert!(feedback.admission_receipt(&session, 2).await?.is_none());
                    feedback.admit_wait(&session, 2, &input(2)).await
                } else {
                    self.admission.wait_active().await?;
                    self.admission.admit_wait(&self.session, 2, &input(2)).await
                }
            }
            .await;
            match result {
                Ok(_) => self.events.send(Event::FeedbackAccepted)?,
                Err(error) => {
                    if matches!(error, PipeError::Closed) {
                        self.events.send(Event::FeedbackClosed)?;
                    }
                    return Err(error.into());
                }
            }
        } else {
            assert_eq!(received.envelope.system().sequence(), 2);
            self.events.send(Event::FeedbackHandled)?;
        }
        Ok(())
    }
}

async fn scenario(
    stop_during_callback: bool,
    coordinated: bool,
    capacity: usize,
) -> Option<String> {
    let directory = tempfile::tempdir().expect("temporary storage");
    let channel = open_channel(directory.path(), capacity).await;
    let admission = SourceAdmission::new(channel.clone(), port("out"))
        .await
        .expect("admission service");
    let session = channel
        .register_producer(id(CLIENT))
        .await
        .expect("producer session");
    let release = Arc::new(Semaphore::new(0));
    let (events, mut receive) = mpsc::unbounded_channel();
    let resource = ResourceId::try_new("journal").expect("resource");
    let batch = ComponentBatch::builder()
        .source(
            ComponentName::try_new("source").expect("source name"),
            Box::new(Source {
                descriptor: descriptor(SOURCE, PortDirection::Output),
                admission: admission.clone(),
            }),
        )
        .sink(
            ComponentName::try_new("sink").expect("sink name"),
            Box::new(Sink {
                descriptor: descriptor(SINK, PortDirection::Input),
                admission: admission.clone(),
                session: session.clone(),
                release: release.clone(),
                events,
                coordinated,
            }),
        )
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "journal".into(),
        })
        .expect("resource declaration")
        .provide_resource(resource.clone(), channel.resource())
        .expect("resource binding")
        .bind_stream(
            Endpoint::new(id(SOURCE), port("out")),
            channel.definition().stream.clone(),
        )
        .connect(
            EdgeDefinition {
                from: Endpoint::new(id(SOURCE), port("out")),
                to: Endpoint::new(id(SINK), port("in")),
            },
            Box::new(channel.definition().pipe(resource, "sink")),
        )
        .build()
        .expect("component batch");
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_allow_external_component_ids(true)
        .with_feedback_shutdown(coordinated)
        .with_cleanup_timeout_secs(2)
        .with_shutdown_timeout_secs(10)
        .with_components(batch)
        .build()
        .await
        .expect("host");
    core.start().await.expect("host start");
    admission.wait_active().await.expect("active mailbox");
    assert!(
        !admission.in_feedback_context(),
        "external callers are not callbacks"
    );
    assert!(
        admission.feedback().wait_active().await.is_err(),
        "external callers cannot use feedback capability"
    );
    admission
        .admit_wait(&session, 1, &input(1))
        .await
        .expect("initial admission");
    assert!(matches!(receive.recv().await, Some(Event::CallbackEntered)));
    let control = core.computation_control().expect("graph control");

    let error = if stop_during_callback {
        let mut observed = control.subscribe_observed();
        let shutdown = core.shutdown();
        tokio::pin!(shutdown);
        // Poll shutdown concurrently, keeping its future alive through completion.
        loop {
            let state = observed.borrow_and_update().clone();
            let boundary = if coordinated {
                ComponentLifecycle::Quiescing
            } else {
                ComponentLifecycle::Quiesced
            };
            if state.components[&id(SOURCE)].lifecycle == boundary {
                break;
            }
            tokio::select! {
                result = &mut shutdown => panic!("shutdown finished before callback release: {result:?}"),
                changed = observed.changed() => changed.expect("lifecycle observer"),
            }
        }
        assert_eq!(admission.is_active().expect("mailbox state"), coordinated);
        assert!(matches!(
            admission.wait_active().await,
            Err(PipeError::Closed)
        ));
        assert!(matches!(
            admission.admit_wait(&session, 2, &input(2)).await,
            Err(PipeError::Closed)
        ));
        release.add_permits(1);
        let (event, result) = tokio::join!(receive.recv(), &mut shutdown);
        assert!(matches!(event, Some(Event::FeedbackAttempt)));
        if coordinated && capacity > 1 {
            result.expect("feedback drain shutdown");
            assert!(matches!(
                receive.recv().await,
                Some(Event::FeedbackAccepted)
            ));
            assert!(matches!(receive.recv().await, Some(Event::FeedbackHandled)));
            None
        } else {
            let error = result.expect_err("cannot acknowledge unfinished callback");
            let detail = format!("{error:#}");
            if coordinated {
                assert!(detail.contains("reconciliation did not reach"), "{detail}");
            } else {
                assert!(matches!(receive.recv().await, Some(Event::FeedbackClosed)));
                assert!(detail.contains("pipe is closed"), "{detail}");
                assert!(!detail.contains("reconciliation did not reach"), "{detail}");
            }
            Some(detail)
        }
    } else {
        release.add_permits(1);
        assert!(matches!(receive.recv().await, Some(Event::FeedbackAttempt)));
        assert!(matches!(
            receive.recv().await,
            Some(Event::FeedbackAccepted)
        ));
        assert!(matches!(receive.recv().await, Some(Event::FeedbackHandled)));
        // Await the callback's acknowledgement too, not only its last statement.
        control
            .quiesce_components(
                control.desired_snapshot().revision,
                GraphSelection::Exact(vec![id(SINK)]),
            )
            .await
            .expect("await handling and acknowledgement");
        if coordinated {
            core.stop().await.expect("coordinated stop");
            assert!(matches!(
                admission.wait_active().await,
                Err(PipeError::Closed)
            ));
            core.start().await.expect("restart initialized objects");
            admission.wait_active().await.expect("new activation");
            assert_eq!(
                admission
                    .admit_wait(&session, 2, &input(2))
                    .await
                    .expect("duplicate after restart")
                    .position,
                2
            );
        }
        core.shutdown().await.expect("normal shutdown");
        None
    };

    let expected_head = if error.is_some() { 1 } else { 2 };
    let expected_cursor = if error.is_some() { 0 } else { 2 };
    let progress = channel.progress().await.expect("journal progress");
    assert_eq!(progress.accepted, expected_head);
    assert_eq!(progress.processed["sink"], expected_cursor);
    channel.shutdown().await.expect("close borrowed journal");
    let reopened = open_channel(directory.path(), capacity).await;
    let progress = reopened.progress().await.expect("reopened progress");
    assert_eq!(progress.accepted, expected_head);
    assert_eq!(progress.processed["sink"], expected_cursor);
    if error.is_some() {
        assert_eq!(progress.earliest_available, Some(1));
        assert!(reopened
            .admission_receipt(&session, 2)
            .await
            .expect("feedback receipt lookup")
            .is_none());
        let resource = ResourceId::try_new("reopened").expect("resource");
        let pipe = reopened.definition().pipe(resource.clone(), "sink");
        let mut binding = pipe
            .create_with_resources(&BTreeMap::from([(resource, reopened.resource())]))
            .expect("replay binding");
        let mut receiver = binding.pipe.take_receiver().expect("replay receiver");
        let replay = receiver
            .receive()
            .await
            .expect("replay read")
            .expect("retained input");
        assert_eq!(replay.envelope().system().sequence(), 1);
        assert_eq!(replay.envelope().changes().operations().len(), 1);
        drop(replay);
        binding.control.cancel();
        assert_eq!(
            reopened
                .progress()
                .await
                .expect("unacknowledged replay")
                .processed["sink"],
            0
        );
        eprintln!("reopened journal: accepted=1, sink cursor=0, earliest=1; feedback sequence2 unaccepted");
    }
    reopened.shutdown().await.expect("close reopened journal");
    error
}

#[tokio::test]
async fn active_source_feedback_control() {
    assert!(tokio::time::timeout(DEADLINE, scenario(false, false, 4))
        .await
        .expect("bounded scenario")
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_source_feedback_control_multithread() {
    assert!(tokio::time::timeout(DEADLINE, scenario(false, false, 4))
        .await
        .expect("bounded scenario")
        .is_none());
}

#[tokio::test]
async fn coordinated_stop_restart_preserves_receipts() {
    assert!(tokio::time::timeout(DEADLINE, scenario(false, true, 4))
        .await
        .expect("restart")
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinated_stop_restart_preserves_receipts_multithread() {
    assert!(tokio::time::timeout(DEADLINE, scenario(false, true, 4))
        .await
        .expect("restart")
        .is_none());
}

#[tokio::test]
async fn coordinated_shutdown_finishes_feedback() {
    assert!(tokio::time::timeout(DEADLINE, scenario(true, true, 4))
        .await
        .expect("bounded scenario")
        .is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinated_shutdown_finishes_feedback_multithread() {
    assert!(tokio::time::timeout(DEADLINE, scenario(true, true, 4))
        .await
        .expect("bounded scenario")
        .is_none());
}

#[tokio::test]
async fn self_capacity_cycle_preserves_input() {
    assert!(tokio::time::timeout(DEADLINE, scenario(true, true, 1))
        .await
        .expect("bounded failure")
        .is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_capacity_cycle_preserves_input_multithread() {
    assert!(tokio::time::timeout(DEADLINE, scenario(true, true, 1))
        .await
        .expect("bounded failure")
        .is_some());
}

#[tokio::test]
async fn ordinary_drain_reports_failed_consumer_without_timeout() {
    assert!(tokio::time::timeout(DEADLINE, scenario(true, false, 4))
        .await
        .expect("bounded failure")
        .is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_drain_reports_failed_consumer_without_timeout_multithread() {
    assert!(tokio::time::timeout(DEADLINE, scenario(true, false, 4))
        .await
        .expect("bounded failure")
        .is_some());
}

async fn full_journal_with_independent_consumer(batched: bool) {
    const OTHER_SOURCE: &str = "c1bdd13a-ea50-473c-9787-4014e312f139";
    const OTHER_SINK: &str = "c8eea040-8be0-48df-9000-3286402977b1";
    const RELAY: &str = "4fc23f79-3c5a-4dde-a836-6a291a9e401b";
    let timer = Arc::new(PendingTimer::default());
    let directory = tempfile::tempdir().expect("journal directory");
    // The feedback target has capacity one and an independently progressing sink.
    let channel = open_channel(directory.path(), 1).await;
    let target = SourceAdmission::new(channel.clone(), port("out"))
        .await
        .expect("target");
    let target_session = channel
        .register_producer(id(CLIENT))
        .await
        .expect("target session");
    let other_directory = tempfile::tempdir().expect("other journal");
    let provider = LegacyIndexProviderAdapter::scoped(
        Arc::new(
            RocksDbIndexProvider::new(other_directory.path(), false, false)
                .with_memory_budget_bytes(32 << 20)
                .expect("memory bound"),
        ),
        INSTANCE,
    )
    .expect("provider");
    let mut codec = EnvelopeCodec::new(size(65536));
    codec
        .register_schema(GraphChangeCodec::schema())
        .expect("schema");
    let mut other_definition = channel.definition().clone();
    other_definition.stream = StreamId::try_new("other/out").expect("other stream");
    let other_channel = QosChannel::persistent_with_recovery(
        other_definition,
        provider
            .create_indexes(GRAPH, &id(OTHER_SOURCE))
            .await
            .expect("indexes"),
        codec,
        "journal",
        QosRecoveryOptions::Admission(AdmissionOptions {
            construction_scope: INSTANCE.into(),
            graph_id: GRAPH.into(),
            component_id: id(OTHER_SOURCE),
            failure_scope: FailureMode::ProcessRestart,
            max_producers: size(1),
            receipts_per_producer: size(4),
        }),
    )
    .await
    .expect("other journal");
    let other = SourceAdmission::new(other_channel.clone(), port("out"))
        .await
        .expect("other source");
    let other_session = other_channel
        .register_producer(id(CLIENT))
        .await
        .expect("other session");
    let (events, mut receive) = mpsc::unbounded_channel();
    let (handled, mut acknowledgements) = mpsc::unbounded_channel();
    let callback_release = Arc::new(Semaphore::new(0));
    let consumer_release = Arc::new(Semaphore::new(0));
    let mut batch = ComponentBatch::builder()
        .transformer(
            ComponentName::try_new("scheduled-relay").expect("name"),
            Box::new(ScheduledRelay {
                descriptor: ComponentDescriptor::try_new(
                    id(RELAY),
                    vec![
                        PortDescriptor::new(
                            port("in"),
                            PortDirection::Input,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        ),
                        PortDescriptor::new(
                            port("out"),
                            PortDirection::Output,
                            GraphChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        ),
                    ],
                )
                .expect("relay"),
                timer: timer.clone(),
            }),
        )
        .bind_stream(
            Endpoint::new(id(RELAY), port("out")),
            StreamId::try_new("relay/out").expect("stream"),
        )
        .connect(
            EdgeDefinition {
                from: Endpoint::new(id(RELAY), port("out")),
                to: Endpoint::new(id(OTHER_SINK), port("in")),
            },
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .source(
            ComponentName::try_new("target").expect("name"),
            Box::new(Source {
                descriptor: descriptor(SOURCE, PortDirection::Output),
                admission: target.clone(),
            }),
        )
        .source(
            ComponentName::try_new("other").expect("name"),
            Box::new(Source {
                descriptor: descriptor(OTHER_SOURCE, PortDirection::Output),
                admission: other.clone(),
            }),
        )
        .sink(
            ComponentName::try_new("independent").expect("name"),
            Box::new(AcknowledgingSink {
                descriptor: descriptor(SINK, PortDirection::Input),
                entered: handled,
                release: consumer_release.clone(),
                batched,
            }),
        )
        .sink(
            ComponentName::try_new("callback").expect("name"),
            Box::new(Sink {
                descriptor: descriptor(OTHER_SINK, PortDirection::Input),
                admission: target.clone(),
                session: target_session.clone(),
                release: callback_release.clone(),
                events,
                coordinated: true,
            }),
        );
    for (source, sink, journal, name) in [
        (SOURCE, SINK, channel.clone(), "target-journal"),
        (OTHER_SOURCE, RELAY, other_channel.clone(), "other-journal"),
    ] {
        let resource = ResourceId::try_new(name).expect("resource");
        batch = batch
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: name.into(),
            })
            .expect("declaration")
            .provide_resource(resource.clone(), journal.resource())
            .expect("resource")
            .bind_stream(
                Endpoint::new(id(source), port("out")),
                journal.definition().stream.clone(),
            )
            .connect(
                EdgeDefinition {
                    from: Endpoint::new(id(source), port("out")),
                    to: Endpoint::new(id(sink), port("in")),
                },
                Box::new(journal.definition().pipe(resource, "sink")),
            );
    }
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_allow_external_component_ids(true)
        .with_feedback_shutdown(true)
        .with_cleanup_timeout_secs(5)
        .with_components(batch.build().expect("batch"))
        .build()
        .await
        .expect("host");
    core.start().await.expect("start");
    target.wait_active().await.expect("target active");
    other.wait_active().await.expect("other active");
    target
        .admit_wait(&target_session, 1, &input(1))
        .await
        .expect("fill target");
    if !batched {
        assert_eq!(acknowledgements.recv().await, Some(1));
    }
    other
        .admit_wait(&other_session, 1, &input(1))
        .await
        .expect("start callback");
    assert!(matches!(receive.recv().await, Some(Event::CallbackEntered)));
    let control = core.computation_control().expect("control");
    let mut observed = control.subscribe_observed();
    let shutdown = core.shutdown();
    tokio::pin!(shutdown);
    loop {
        if observed.borrow_and_update().components[&id(SOURCE)].lifecycle
            == ComponentLifecycle::Quiescing
        {
            break;
        }
        tokio::select! {
            result = &mut shutdown => panic!("early shutdown {result:?}"),
            changed = observed.changed() => changed.expect("observation"),
        }
    }
    // The future wakeup becomes ready after the ingress fence. It must remain
    // unconsumed even while queued inputs and feedback continue to drain.
    timer.pending.add_permits(1);
    callback_release.add_permits(1);
    let event = tokio::select! {
        result = &mut shutdown => panic!("consumer still held {result:?}"),
        event = receive.recv() => event,
    };
    assert!(matches!(event, Some(Event::FeedbackAttempt)));
    assert_eq!(
        channel.progress().await.expect("full journal").processed["sink"],
        0
    );
    assert!(channel
        .admission_receipt(&target_session, 2)
        .await
        .expect("not accepted yet")
        .is_none());
    consumer_release.add_permits(1);
    shutdown.await.expect("independent consumer frees capacity");
    if batched {
        assert_eq!(acknowledgements.recv().await, Some(1));
    }
    assert_eq!(timer.handled.load(Ordering::SeqCst), 0);
    assert_eq!(timer.pending.available_permits(), 1);
    assert!(matches!(
        receive.recv().await,
        Some(Event::FeedbackAccepted)
    ));
    assert_eq!(acknowledgements.recv().await, Some(2));
    assert_eq!(
        channel.progress().await.expect("target handled").processed["sink"],
        2
    );
    assert_eq!(
        other_channel
            .progress()
            .await
            .expect("callback handled")
            .processed["sink"],
        1
    );
    channel.shutdown().await.expect("target cleanup");
    other_channel.shutdown().await.expect("other cleanup");
    let reopened = open_channel(directory.path(), 1).await;
    assert_eq!(
        reopened
            .progress()
            .await
            .expect("reopened target")
            .processed["sink"],
        2
    );
    reopened.shutdown().await.expect("reopened cleanup");
}

#[tokio::test]
async fn full_journal_drains_with_independent_consumer() {
    tokio::time::timeout(DEADLINE, full_journal_with_independent_consumer(false))
        .await
        .expect("bounded drain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_journal_drains_with_independent_consumer_multithread() {
    tokio::time::timeout(DEADLINE, full_journal_with_independent_consumer(false))
        .await
        .expect("bounded drain");
}

#[tokio::test]
async fn partial_batches_flush_at_feedback_fence_without_waiting_for_batch_timer() {
    tokio::time::timeout(DEADLINE, full_journal_with_independent_consumer(true))
        .await
        .expect("bounded batch drain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn partial_batches_flush_at_feedback_fence_without_waiting_for_batch_timer_multithread() {
    tokio::time::timeout(DEADLINE, full_journal_with_independent_consumer(true))
        .await
        .expect("bounded batch drain");
}
