// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::{
    ChangeSet, ComponentDescriptor, ComputationComponent, ComputationGraph, EdgeDefinition,
    Endpoint, EnvelopeSink, EnvelopeSource, GraphChangeCodec, GraphProducerIdentity,
    GraphProducerProgress, LegacyIndexProviderAdapter, OutputEnvelope, PipeRequirements,
    PortDescriptor, PortDirection, QosChannel, QosChannelDefinition, QueryChangeCodec,
    ResourceCleanup, ResourceId, ResourceOwnership, ResourceRole, ResourceSpecification,
    RetentionPolicy, SinkCompletion, SubscriptionStart,
};
use drasi_core::{
    computation::{ComputationIndexProvider, ComputationResource, TransactionDomain},
    interface::{FailureMode, IndexError, IndexSet, SessionControl},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use std::{
    collections::HashSet,
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::sync::Notify;

mod transactional;

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("nonzero")
}

fn options() -> DeliveryOptions {
    DeliveryOptions {
        scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
        max_streams: size(2),
        receipts_per_stream: size(2),
        retry: DeliveryRetryPolicy::default(),
    }
}

fn scope() -> DeliveryScope {
    DeliveryScope::new(
        "scope".into(),
        "graph".into(),
        ComponentId::try_new("sink").expect("id"),
    )
    .expect("scope")
}

fn codec() -> EnvelopeCodec {
    let mut codec = EnvelopeCodec::new(size(1 << 20));
    codec
        .register_schema(GraphChangeCodec::schema())
        .expect("schema");
    codec
        .register_schema(QueryChangeCodec::schema())
        .expect("schema");
    codec
}

fn producer(stream: &str) -> GraphProducerIdentity {
    GraphProducerIdentity::new(
        "scope".into(),
        "graph".into(),
        ComponentId::try_new("producer").expect("id"),
        StreamId::try_new(stream).expect("stream"),
        true,
    )
    .expect("identity")
}

fn input(producer: &GraphProducerIdentity, sequence: u64, count: usize) -> InputEnvelope {
    let changes = (0..count)
        .map(|index| SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &index.to_string()),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::default(),
            },
        })
        .collect::<Vec<_>>();
    let seed = GraphChangeCodec::encode_change(
        SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", "seed"),
                labels: Arc::from([]),
                effective_from: 0,
            },
        },
        StreamId::try_new("seed").expect("stream"),
        1,
        None,
    )
    .expect("seed");
    let mut envelope =
        GraphChangeCodec::derive_changes(&seed, &changes, producer.stream().clone(), sequence)
            .expect("changes");
    GraphProducerProgress::annotate(&mut envelope, producer, sequence).expect("progress");
    InputEnvelope {
        port: PortId::try_new("in").expect("port"),
        envelope,
    }
}

async fn indexes(path: &Path) -> ComputationIndexes {
    LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(path, false, false)))
        .create_indexes("delivery", "consumer")
        .await
        .expect("indexes")
}

async fn runner(path: &Path) -> DeliveryRunner {
    DeliveryRunner::new(scope(), indexes(path).await, codec(), options()).expect("runner")
}

#[derive(Default)]
struct Handler {
    attempts: Vec<(u64, String)>,
    effects: HashSet<String>,
    fail: Option<u64>,
    transient: bool,
    fail_once: bool,
    hold: Option<Arc<Notify>>,
    arm_commit: Option<Arc<AtomicBool>>,
    record_effect: Option<std::path::PathBuf>,
}

#[async_trait]
impl DeliveryHandler for Handler {
    fn retryable(&self, _: &anyhow::Error) -> bool {
        self.transient
    }

    async fn handle(&mut self, item: DeliveryItem<'_>) -> anyhow::Result<()> {
        let ordinal = item.operation.ordinal();
        self.attempts.push((ordinal, item.id.to_string()));
        if self.fail == Some(ordinal) {
            if self.fail_once {
                self.fail = None;
            }
            anyhow::bail!("injected destination failure");
        }
        self.effects.insert(item.id.to_string());
        if let Some(path) = &self.record_effect {
            use std::io::Write;
            let mut file = std::fs::File::create(path)?;
            file.write_all(&serde_json::to_vec(&self.effects)?)?;
            file.sync_all()?;
        }
        if let Some(arm) = &self.arm_commit {
            arm.store(true, Ordering::Release);
        }
        if let Some(entered) = &self.hold {
            entered.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn retirement_requires_loaded_completion_and_excludes_reopening() -> anyhow::Result<()> {
    for (initialized, operations, fail) in
        [(false, 0, false), (true, 0, false), (true, 3, false), (true, 3, true)]
    {
        let directory = tempfile::tempdir()?;
        let provider: Arc<dyn ComputationIndexProvider> = LegacyIndexProviderAdapter::new(
            Arc::new(RocksDbIndexProvider::new(directory.path(), false, false)),
        );
        let foreign: Arc<dyn ComputationIndexProvider> = LegacyIndexProviderAdapter::new(Arc::new(
            RocksDbIndexProvider::new(directory.path(), false, false),
        ));
        let mut delivery = DeliveryRunner::new(
            scope(),
            provider.create_indexes("delivery", "consumer").await?,
            codec(),
            options(),
        )?;
        let retirement = DeliveryRetirementState::new(&provider);
        assert!(retirement.freeze(&foreign, false)?.is_none());
        assert!(retirement.freeze(&provider, false).is_err());
        assert!(
            retirement.freeze(&provider, true).is_err(),
            "loss authorization still requires completed cleanup"
        );
        if initialized {
            delivery.progress().await?;
        }
        if operations > 0 {
            let batch = input(&producer("out"), 1, operations);
            let mut handler = Handler {
                fail: fail.then(|| batch.envelope.changes().operations()[1].ordinal()),
                ..Default::default()
            };
            assert_eq!(delivery.deliver(&batch, &mut handler).await.is_err(), fail);
        }
        assert!(
            retirement.freeze(&provider, false).is_err(),
            "a live owner is not retirement proof"
        );
        retirement.close(&mut delivery).await?;
        assert!(delivery.progress().await.is_err());
        if initialized && !fail {
            let held = retirement
                .freeze(&provider, false)?
                .expect("closed, drained owner");
            assert!(retirement.invalidate().is_err());
            assert!(retirement.freeze(&provider, false).is_err());
            held.resume();
            let held = retirement
                .freeze(&provider, false)?
                .expect("confirmed rejection resumes");
            drop(held);
            assert!(
                retirement.invalidate().is_err(),
                "abandonment fences reconstruction"
            );
        } else {
            assert!(retirement.freeze(&provider, false).is_err());
            retirement
                .freeze(&provider, true)?
                .expect("explicit loss after actual cleanup")
                .resume();
            retirement.invalidate()?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn partial_batches_resume_exact_operations_and_do_not_skip_other_streams(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let first = producer("first");
    let second = producer("second");
    let batch = input(&first, 1, 3);
    let failed = batch.envelope.changes().operations()[1].ordinal();
    let mut handler = Handler {
        fail: Some(failed),
        ..Default::default()
    };
    let mut delivery = runner(directory.path()).await;
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    assert_eq!(handler.effects.len(), 1);
    let progress = delivery.progress().await?;
    assert_eq!(
        (
            progress[0].handled_sequence,
            progress[0].completed_operations
        ),
        (0, 1)
    );
    assert!(matches!(
        delivery
            .deliver(&input(&first, 2, 1), &mut handler)
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(DeliveryError::Pending { sequence: 1 })
    ));
    handler.fail = None;
    delivery
        .deliver(&input(&second, 7, 1), &mut handler)
        .await?;
    assert_eq!(handler.effects.len(), 2);
    delivery.shutdown().await?;
    drop(delivery);

    let mut delivery = runner(directory.path()).await;
    let mut replay = batch.clone();
    replay.envelope = replay.envelope.reemit(99)?;
    delivery.deliver(&replay, &mut handler).await?;
    assert_eq!(handler.effects.len(), 4);
    assert_eq!(handler.attempts[1].1, handler.attempts[3].1);
    let attempts = handler.attempts.len();
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(handler.attempts.len(), attempts);
    assert!(delivery
        .progress()
        .await?
        .iter()
        .all(|progress| progress.completed_operations == progress.operations));
    delivery.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn receipts_reject_changed_expired_or_foreign_outputs_without_effects() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let identity = producer("out");
    let mut delivery = runner(directory.path()).await;
    let mut handler = Handler::default();
    for sequence in 1..=3 {
        delivery
            .deliver(&input(&identity, sequence, 1), &mut handler)
            .await?;
    }
    assert_eq!(handler.effects.len(), 3);
    for (batch, error) in [
        (input(&identity, 1, 1), ReplayRejection::ReceiptExpired(1)),
        (input(&identity, 3, 2), ReplayRejection::PayloadConflict(3)),
        (
            input(&producer("out"), 4, 1),
            ReplayRejection::ProducerChanged,
        ),
    ] {
        assert_eq!(
            delivery
                .deliver(&batch, &mut handler)
                .await
                .unwrap_err()
                .downcast_ref::<ReplayRejection>(),
            Some(&error)
        );
    }
    assert_eq!(handler.effects.len(), 3);
    delivery.shutdown().await?;
    drop(delivery);
    let mut delivery = runner(directory.path()).await;
    assert_eq!(delivery.progress().await?[0].handled_sequence, 3);
    delivery
        .deliver(&input(&identity, 3, 1), &mut handler)
        .await?;
    assert_eq!(handler.attempts.len(), 3);
    delivery.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn retry_is_explicit_bounded_and_cancellation_keeps_the_same_id() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let mut delivery = runner(directory.path()).await;
    delivery.options.retry = DeliveryRetryPolicy {
        max_attempts: size(3),
        delay: Duration::from_millis(1),
    };
    let batch = input(&producer("out"), 1, 1);
    let ordinal = batch.envelope.changes().operations()[0].ordinal();
    let mut handler = Handler {
        fail: Some(ordinal),
        transient: true,
        ..Default::default()
    };
    let error = delivery.deliver(&batch, &mut handler).await.unwrap_err();
    assert!(matches!(
        error.downcast_ref::<DeliveryError>(),
        Some(DeliveryError::Handling { attempts: 3, .. })
    ));
    assert_eq!(handler.attempts.len(), 3);
    assert!(handler
        .attempts
        .windows(2)
        .all(|pair| pair[0].1 == pair[1].1));
    handler.transient = false;
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    assert_eq!(handler.attempts.len(), 4);

    handler.fail = None;
    let entered = Arc::new(Notify::new());
    handler.hold = Some(entered.clone());
    {
        let pending = delivery.deliver(&batch, &mut handler);
        tokio::pin!(pending);
        tokio::select! {
            biased;
            result = &mut pending => panic!("destination should be held: {result:?}"),
            _ = entered.notified() => {}
        }
    }
    assert_eq!(delivery.progress().await?[0].handled_sequence, 0);
    delivery.shutdown().await?;
    drop(delivery);
    handler.hold = None;
    let mut delivery = runner(directory.path()).await;
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(
        handler.effects.len(),
        1,
        "idempotent destination must not repeat the cancelled effect"
    );
    assert_eq!(handler.attempts[0].1, handler.attempts[5].1);
    delivery.shutdown().await?;
    Ok(())
}

struct CommitFailure {
    inner: Arc<dyn SessionControl>,
    armed: Arc<AtomicBool>,
    after: bool,
    exit: bool,
}

#[async_trait]
impl SessionControl for CommitFailure {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> Result<(), IndexError> {
        if self.armed.swap(false, Ordering::AcqRel) {
            if self.after {
                self.inner.commit().await?;
            }
            if self.exit {
                std::process::exit(82);
            }
            return Err(IndexError::IOError);
        }
        self.inner.commit().await
    }
    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

fn with_control(
    original: ComputationIndexes,
    control: Arc<dyn SessionControl>,
) -> ComputationIndexes {
    let domain = TransactionDomain::new(control.clone());
    let set = original.indexes();
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
            original.checkpoint_store().expect("checkpoints").clone(),
            &domain,
        )),
        Some(ComputationResource::participating(
            original.outbox_writer().expect("outbox").clone(),
            &domain,
        )),
        Some(ComputationResource::participating(
            original
                .live_results_writer()
                .expect("live results")
                .clone(),
            &domain,
        )),
    )
    .expect("wrapped")
    .with_cleanup(original.cleanup().expect("cleanup").clone())
    .with_durability(original.durability())
}

#[tokio::test(flavor = "current_thread")]
async fn uncertain_progress_commits_reconstruct_without_skipping_unfinished_effects(
) -> anyhow::Result<()> {
    for after in [false, true] {
        let directory = tempfile::tempdir()?;
        let provider: Arc<dyn ComputationIndexProvider> = LegacyIndexProviderAdapter::new(
            Arc::new(RocksDbIndexProvider::new(directory.path(), false, false)),
        );
        let original = provider.create_indexes("delivery", "consumer").await?;
        let armed = Arc::new(AtomicBool::new(false));
        let control = Arc::new(CommitFailure {
            inner: original.indexes().session_control.clone(),
            armed: armed.clone(),
            after,
            exit: false,
        });
        let mut delivery =
            DeliveryRunner::new(scope(), with_control(original, control), codec(), options())?;
        let batch = input(&producer("out"), 1, 2);
        let mut handler = Handler {
            arm_commit: Some(armed),
            ..Default::default()
        };
        assert!(delivery.deliver(&batch, &mut handler).await.is_err());
        assert_eq!(handler.effects.len(), 1);
        assert!(
            delivery.progress().await.is_err(),
            "uncertain writes fence cached progress"
        );
        assert!(delivery.deliver(&batch, &mut handler).await.is_err());
        assert_eq!(handler.attempts.len(), 1);
        let retirement = DeliveryRetirementState::new(&provider);
        retirement.close(&mut delivery).await?;
        assert!(
            retirement.freeze(&provider, false).is_err(),
            "shutdown cannot certify an uncertain ledger"
        );
        retirement
            .freeze(&provider, true)?
            .expect("explicit loss after actual cleanup")
            .retire();
        drop(delivery);
        handler.arm_commit = None;
        let mut delivery = runner(directory.path()).await;
        assert_eq!(
            delivery.progress().await?[0].completed_operations,
            usize::from(after)
        );
        delivery.deliver(&batch, &mut handler).await?;
        assert_eq!(handler.effects.len(), 2);
        assert_eq!(handler.attempts.len(), if after { 2 } else { 3 });
        delivery.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn scope_and_damaged_progress_reject_before_destination_handling() -> anyhow::Result<()> {
    let batch = input(&producer("out"), 1, 1);
    for corruption in 0..6 {
        let directory = tempfile::tempdir()?;
        let mut delivery = runner(directory.path()).await;
        let mut handler = Handler::default();
        delivery.deliver(&batch, &mut handler).await?;
        delivery.shutdown().await?;
        drop(delivery);
        let indexes = indexes(directory.path()).await;
        let outbox = indexes.outbox_writer().expect("outbox").clone();
        let transaction = ComputationTransaction::try_new(indexes)?;
        transaction
            .run(async {
                let records = outbox.read_from(METADATA_KEY, 0).await?;
                let mut value: serde_json::Value =
                    serde_json::from_slice(&records[0].1).expect("metadata");
                match corruption {
                    0 => value["version"] = 2.into(),
                    1 => value["scope"]["consumer"] = "other".into(),
                    2 => value["streams"][0]["receipts"][0]["completed"] = 2.into(),
                    3 => value["streams"][0]["receipts"][0]["sequence"] = 0.into(),
                    4 => value["streams"][0]["handled"] = 42.into(),
                    _ => value["streams"][0]["receipts"] = serde_json::json!([]),
                }
                outbox
                    .append(
                        METADATA_KEY,
                        1,
                        &serde_json::to_vec(&value).expect("encode"),
                    )
                    .await?;
                Ok(())
            })
            .await?;
        transaction.shutdown().await?;
        drop(transaction);
        drop(outbox);
        let mut delivery = runner(directory.path()).await;
        assert!(
            delivery.deliver(&batch, &mut handler).await.is_err(),
            "corruption {corruption}"
        );
        assert_eq!(handler.attempts.len(), 1);
        delivery.shutdown().await?;
    }
    Ok(())
}

struct Source {
    descriptor: ComponentDescriptor,
    batch: Option<InputEnvelope>,
}

#[async_trait]
impl ComputationComponent for Source {
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
impl EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.batch.take().map(|input| OutputEnvelope {
            port: PortId::try_new("out").expect("port"),
            envelope: input.envelope,
        }))
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    delivery: DeliveryRunner,
    handler: Arc<tokio::sync::Mutex<Handler>>,
}

#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.delivery.progress().await?;
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.delivery.shutdown().await
    }
}

#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.delivery
            .deliver(&input, &mut *self.handler.lock().await)
            .await
    }
}

fn descriptor(
    id: &str,
    port: &str,
    direction: PortDirection,
) -> anyhow::Result<ComponentDescriptor> {
    Ok(ComponentDescriptor::try_new(
        ComponentId::try_new(id)?,
        vec![PortDescriptor::new(
            PortId::try_new(port)?,
            direction,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )?)
}

async fn external_sink(
    path: &Path,
    handler: Arc<tokio::sync::Mutex<Handler>>,
) -> anyhow::Result<Box<dyn EnvelopeSink>> {
    Ok(Box::new(Sink {
        descriptor: descriptor("sink", "in", PortDirection::Input)?,
        delivery: runner(path).await,
        handler,
    }))
}

async fn graph(
    path: &Path,
    batch: Option<InputEnvelope>,
    sink: Box<dyn EnvelopeSink>,
) -> anyhow::Result<(ComputationGraph, Arc<QosChannel>)> {
    let definition = QosChannelDefinition {
        stream: StreamId::try_new("out")?,
        capacity: size(1),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: std::collections::BTreeMap::from([(
            "target".into(),
            SubscriptionStart::Earliest,
        )]),
    };
    let pipe_indexes =
        LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(path, false, false)))
            .create_indexes("delivery", "pipe")
            .await?;
    let channel =
        QosChannel::persistent(definition.clone(), pipe_indexes, codec(), "journal").await?;
    let resource = ResourceId::try_new("pipe")?;
    let source = Endpoint::new(ComponentId::try_new("producer")?, PortId::try_new("out")?);
    let graph = ComputationGraph::builder("graph")
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "pipe".into(),
        })?
        .provide_resource(resource.clone(), channel.resource())?
        .source(Box::new(Source {
            descriptor: descriptor("producer", "out", PortDirection::Output)?,
            batch,
        }))
        .sink(sink)
        .bind_stream(source.clone(), definition.stream.clone())
        .connect(
            EdgeDefinition::new(
                source,
                Endpoint::new(ComponentId::try_new("sink")?, PortId::try_new("in")?),
            ),
            Box::new(definition.pipe(resource, "target")),
        )
        .build()?;
    Ok((graph, channel))
}

#[tokio::test(flavor = "current_thread")]
async fn connected_qos_waits_for_every_operation_and_replays_only_unfinished_delivery(
) -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let batch = input(&producer("out"), 1, 3);
    let failed = batch.envelope.changes().operations()[1].ordinal();
    let handler = Arc::new(tokio::sync::Mutex::new(Handler {
        fail: Some(failed),
        ..Default::default()
    }));
    let (mut running, channel) = graph(
        directory.path(),
        Some(batch),
        external_sink(directory.path(), handler.clone()).await?,
    )
    .await?;
    assert!(
        tokio::time::timeout(Duration::from_secs(5), running.start()?)
            .await?
            .is_err()
    );
    assert_eq!(channel.progress().await?.processed["target"], 0);
    assert_eq!(handler.lock().await.effects.len(), 1);
    running.shutdown().await?;
    channel.shutdown().await?;
    drop(running);
    drop(channel);
    handler.lock().await.fail = None;

    let (mut running, channel) = graph(
        directory.path(),
        None,
        external_sink(directory.path(), handler.clone()).await?,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(5), running.start()?).await??;
    assert_eq!(handler.lock().await.effects.len(), 3);
    assert_eq!(handler.lock().await.attempts.len(), 4);
    assert_eq!(channel.progress().await?.accepted, 1);
    assert_eq!(channel.progress().await?.processed["target"], 1);
    running.shutdown().await?;
    channel.shutdown().await?;
    Ok(())
}

const CRASH_ROOT: &str = "DRASI_DELIVERY_CRASH_ROOT";
const CRASH_AFTER: &str = "DRASI_DELIVERY_CRASH_AFTER";

#[tokio::test(flavor = "current_thread")]
#[ignore = "invoked by operation_identity_and_partial_progress_survive_process_exit"]
async fn delivery_crash_child() -> anyhow::Result<()> {
    let path = std::path::PathBuf::from(std::env::var(CRASH_ROOT)?);
    let original = indexes(&path).await;
    let armed = Arc::new(AtomicBool::new(false));
    let control = Arc::new(CommitFailure {
        inner: original.indexes().session_control.clone(),
        armed: armed.clone(),
        after: std::env::var(CRASH_AFTER)? == "true",
        exit: true,
    });
    let mut delivery =
        DeliveryRunner::new(scope(), with_control(original, control), codec(), options())?;
    let batch = InputEnvelope {
        port: PortId::try_new("in")?,
        envelope: codec().decode(&std::fs::read(path.join("input.json"))?)?,
    };
    let mut handler = Handler {
        arm_commit: Some(armed),
        record_effect: Some(path.join("effect.json")),
        ..Default::default()
    };
    delivery.deliver(&batch, &mut handler).await?;
    anyhow::bail!("delivery did not reach the required progress commit");
}

#[tokio::test(flavor = "current_thread")]
async fn operation_identity_and_partial_progress_survive_process_exit() -> anyhow::Result<()> {
    for after in [false, true] {
        let directory = tempfile::tempdir()?;
        let batch = input(&producer("out"), 1, 2);
        std::fs::write(
            directory.path().join("input.json"),
            codec().encode(&batch.envelope)?,
        )?;
        crash_at_commit(
            "computation::v1::delivery::tests::delivery_crash_child",
            directory.path(),
            after,
        )
        .await?;
        let effects: HashSet<String> =
            serde_json::from_slice(&std::fs::read(directory.path().join("effect.json"))?)?;
        assert_eq!(effects.len(), 1);
        let mut handler = Handler {
            effects,
            ..Default::default()
        };
        let mut delivery = runner(directory.path()).await;
        let progress = delivery.progress().await?;
        assert_eq!(progress[0].handled_sequence, 0);
        assert_eq!(progress[0].completed_operations, usize::from(after));
        delivery.deliver(&batch, &mut handler).await?;
        assert_eq!(
            handler.effects.len(),
            2,
            "replay must preserve the already recorded operation ID"
        );
        assert_eq!(handler.attempts.len(), if after { 1 } else { 2 });
        assert_eq!(delivery.progress().await?[0].handled_sequence, 1);
        delivery.shutdown().await?;
    }
    Ok(())
}

async fn crash_at_commit(test: &str, path: &Path, after: bool) -> anyhow::Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new(std::env::current_exe()?)
        .args(["--exact", test, "--ignored", "--nocapture"])
        .env(CRASH_ROOT, path)
        .env(CRASH_AFTER, after.to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while child.try_wait()?.is_none() {
        if std::time::Instant::now() > deadline {
            child.kill()?;
            let output = child.wait_with_output()?;
            anyhow::bail!("delivery child timed out: {output:?}");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let output = child.wait_with_output()?;
    assert_eq!(output.status.code(), Some(82), "{output:?}");
    Ok(())
}

#[test]
fn operation_keys_cover_scope_port_producer_sequence_and_ordinal_not_transport(
) -> anyhow::Result<()> {
    let identity = producer("out");
    let input = input(&identity, 9, 2);
    let candidate = Candidate::new(&input.envelope, &codec())?;
    let key = operation_id(&scope(), &input, &candidate, 0)?;
    assert_eq!(key.as_str().len(), "drasi-delivery-v1-".len() + 64);
    let mut replay = input.clone();
    replay.envelope = replay.envelope.reemit(100)?;
    assert_eq!(
        operation_id(
            &scope(),
            &replay,
            &Candidate::new(&replay.envelope, &codec())?,
            0
        )?,
        key
    );
    assert_ne!(operation_id(&scope(), &input, &candidate, 1)?, key);
    replay.port = PortId::try_new("different")?;
    assert_ne!(operation_id(&scope(), &replay, &candidate, 0)?, key);
    for changed in [
        DeliveryScope::new(
            "different".into(),
            "graph".into(),
            ComponentId::try_new("sink")?,
        )?,
        DeliveryScope::new(
            "scope".into(),
            "different".into(),
            ComponentId::try_new("sink")?,
        )?,
        DeliveryScope::new(
            "scope".into(),
            "graph".into(),
            ComponentId::try_new("different")?,
        )?,
    ] {
        assert_ne!(operation_id(&changed, &input, &candidate, 0)?, key);
    }
    let later = Candidate {
        sequence: 10,
        ..candidate
    };
    assert_ne!(operation_id(&scope(), &input, &later, 0)?, key);
    let replaced = Candidate {
        producer: Producer::Graph(producer("out")),
        sequence: 9,
        ..later
    };
    assert_ne!(operation_id(&scope(), &input, &replaced, 0)?, key);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn empty_batches_limits_and_retry_delay_cancellation_preserve_progress() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let mut delivery = runner(directory.path()).await;
    let mut handler = Handler::default();
    let identity = producer("out");
    delivery
        .deliver(&input(&identity, 1, 0), &mut handler)
        .await?;
    assert_eq!(delivery.progress().await?[0].handled_sequence, 1);
    assert!(handler.attempts.is_empty());
    delivery
        .deliver(&input(&producer("second"), 1, 0), &mut handler)
        .await?;
    assert!(matches!(
        delivery
            .deliver(&input(&producer("third"), 1, 0), &mut handler)
            .await
            .unwrap_err()
            .downcast_ref(),
        Some(DeliveryError::StreamCapacity)
    ));
    delivery.options.retry = DeliveryRetryPolicy {
        max_attempts: size(3),
        delay: Duration::from_secs(60),
    };
    let batch = input(&identity, 2, 1);
    handler.fail = Some(0);
    handler.transient = true;
    assert!(tokio::time::timeout(
        Duration::from_millis(20),
        delivery.deliver(&batch, &mut handler)
    )
    .await
    .is_err());
    assert_eq!(handler.attempts.len(), 1);
    assert_eq!(delivery.progress().await?[0].handled_sequence, 1);
    assert_eq!(delivery.progress().await?[0].completed_operations, 0);
    delivery.shutdown().await?;
    Ok(())
}

#[test]
fn delivery_settings_enforce_exact_retention_retry_and_identifier_limits() -> anyhow::Result<()> {
    let mut config = options();
    config.max_streams = size(256);
    config.receipts_per_stream = size(16);
    config.retry.max_attempts = size(32);
    config.retry.delay = Duration::from_secs(60);
    config.validate()?;
    config.receipts_per_stream = size(17);
    assert!(config.validate().is_err());
    config.receipts_per_stream = size(1);
    config.max_streams = size(257);
    assert!(config.validate().is_err());
    config.max_streams = size(1);
    config.receipts_per_stream = size(1024);
    config.validate()?;
    config.receipts_per_stream = size(1025);
    assert!(config.validate().is_err());
    config.receipts_per_stream = size(1);
    config.retry.max_attempts = size(33);
    assert!(config.validate().is_err());
    config.retry.max_attempts = size(1);
    config.retry.delay += Duration::from_nanos(1);
    assert!(config.validate().is_err());
    bounded_identifier(&"x".repeat(256))?;
    assert!(bounded_identifier(&"x".repeat(257)).is_err());
    for value in ["", "a b", "a\0b"] {
        assert!(bounded_identifier(value).is_err());
    }
    Ok(())
}

#[derive(Default)]
struct Positions {
    seen: Vec<(DeliveryPosition, u64)>,
    batches: Vec<DeliveryBatchIdentity>,
    fail_once: bool,
}

#[async_trait]
impl DeliveryHandler for Positions {
    async fn handle(&mut self, item: DeliveryItem<'_>) -> anyhow::Result<()> {
        item.position.validate()?;
        self.batches.push(item.batch_identity()?);
        self.seen.push((item.position, item.operation.ordinal()));
        if self.fail_once {
            self.fail_once = false;
            anyhow::bail!("unconfirmed destination operation");
        }
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn delivery_positions_use_vector_indexes_and_stay_stable_across_transport_and_batches(
) -> anyhow::Result<()> {
    let identity = producer("out");
    let mut batch = input(&identity, 9, 2);
    let mut operations = batch.envelope.changes().operations().to_vec();
    for (index, operation) in operations.iter_mut().enumerate() {
        match operation {
            ChangeOperation::Added { ordinal, .. }
            | ChangeOperation::Updated { ordinal, .. }
            | ChangeOperation::Deleted { ordinal, .. } => *ordinal = 10 + index as u64 * 4,
        }
    }
    batch.envelope = ChangeEnvelope::new(
        batch.envelope.id().clone(),
        ChangeSet::try_new(
            batch.envelope.changes().id().clone(),
            batch.envelope.changes().schema().clone(),
            operations,
        )?,
        (**batch.envelope.system()).clone(),
    );
    GraphProducerProgress::annotate(&mut batch.envelope, &identity, 9)?;
    let directory = tempfile::tempdir()?;
    let mut delivery = runner(directory.path()).await;
    let batch_identity = delivery.batch_identity(&batch)?;
    assert!(
        delivery.ledger.is_none(),
        "describing a batch must not load or mutate progress"
    );
    let mut handler = Positions {
        fail_once: true,
        ..Default::default()
    };
    assert!(delivery.deliver(&batch, &mut handler).await.is_err());
    batch.envelope = batch.envelope.reemit(100)?;
    delivery.deliver(&batch, &mut handler).await?;
    assert_eq!(handler.seen[0], handler.seen[1]);
    assert!(handler
        .batches
        .iter()
        .all(|identity| *identity == batch_identity));
    assert_eq!(delivery.batch_identity(&batch)?, batch_identity);
    assert_eq!((handler.seen[1].0.operation(), handler.seen[1].1), (0, 10));
    assert_eq!((handler.seen[2].0.operation(), handler.seen[2].1), (1, 14));
    assert_eq!(handler.seen[1].0.sequence(), 9);
    assert_eq!(handler.seen[1].0.operations(), 2);
    delivery
        .deliver(&input(&identity, 10, 1), &mut handler)
        .await?;
    assert_eq!(handler.seen[1].0.stream(), handler.seen[3].0.stream());
    assert_eq!(handler.seen[3].0.sequence(), 10);
    delivery.shutdown().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn delivery_stream_keys_separate_consumer_scope_port_and_persistent_producer(
) -> anyhow::Result<()> {
    let identity = producer("out");
    let original = input(&identity, 9, 1);
    let mut other_port = original.clone();
    other_port.port = PortId::try_new("different")?;
    let mut keys = HashSet::new();
    for (scope, batch) in [
        (scope(), original.clone()),
        (
            DeliveryScope::new(
                "different".into(),
                "graph".into(),
                ComponentId::try_new("sink")?,
            )?,
            original.clone(),
        ),
        (
            DeliveryScope::new(
                "scope".into(),
                "different".into(),
                ComponentId::try_new("sink")?,
            )?,
            original.clone(),
        ),
        (
            DeliveryScope::new(
                "scope".into(),
                "graph".into(),
                ComponentId::try_new("different")?,
            )?,
            original,
        ),
        (scope(), other_port),
        (scope(), input(&producer("out"), 9, 1)),
        (scope(), input(&producer("different"), 9, 1)),
    ] {
        let directory = tempfile::tempdir()?;
        let mut delivery =
            DeliveryRunner::new(scope, indexes(directory.path()).await, codec(), options())?;
        let mut handler = Positions::default();
        delivery.deliver(&batch, &mut handler).await?;
        assert!(keys.insert(handler.seen[0].0.stream().to_owned()));
        delivery.shutdown().await?;
    }
    assert_eq!(keys.len(), 7);
    Ok(())
}

#[test]
fn delivery_position_validation_rejects_malformed_versions_keys_and_ranges() -> anyhow::Result<()> {
    let position = DeliveryPosition {
        version: 1,
        stream: format!("drasi-delivery-stream-v1-{}", "a".repeat(64)),
        sequence: u64::MAX,
        operation: 1,
        operations: 2,
    };
    position.validate()?;
    let encoded = serde_json::to_value(&position)?;
    assert_eq!(
        serde_json::from_value::<DeliveryPosition>(encoded.clone())?,
        position
    );
    for (field, invalid) in [
        ("version", serde_json::json!(0)),
        ("version", serde_json::json!(2)),
        ("sequence", serde_json::json!(0)),
        ("operations", serde_json::json!(0)),
        ("operations", serde_json::json!(1)),
        ("operation", serde_json::json!(2)),
        ("stream", serde_json::json!("")),
        (
            "stream",
            serde_json::json!(format!("drasi-delivery-stream-v1-{}", "a".repeat(63))),
        ),
        (
            "stream",
            serde_json::json!(format!("drasi-delivery-stream-v1-{}", "a".repeat(65))),
        ),
        (
            "stream",
            serde_json::json!(format!("drasi-delivery-stream-v1-{}", "A".repeat(64))),
        ),
        (
            "stream",
            serde_json::json!(format!("drasi-delivery-stream-v1-{}", "g".repeat(64))),
        ),
    ] {
        let mut changed = encoded.clone();
        changed[field] = invalid;
        let decoded: DeliveryPosition = serde_json::from_value(changed)?;
        assert!(decoded.validate().is_err(), "{field}");
    }
    let mut changed = encoded;
    changed["unexpected"] = serde_json::json!(true);
    assert!(serde_json::from_value::<DeliveryPosition>(changed).is_err());
    Ok(())
}
