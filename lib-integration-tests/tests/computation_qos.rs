// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::BTreeMap, num::NonZeroUsize, path::Path, sync::Arc, time::Duration};

use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    computation::ComputationIndexProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::computation::v1::*;

#[path = "computation_qos/configured.rs"]
mod configured;
#[path = "computation_qos/reliability.rs"]
mod reliability;
#[path = "computation_qos/replay.rs"]
mod replay;

fn definition(durable: bool, retention: RetentionPolicy, capacity: usize) -> QosChannelDefinition {
    QosChannelDefinition {
        stream: StreamId::try_new("source/out").expect("stream"),
        capacity: NonZeroUsize::new(capacity).expect("capacity"),
        durable,
        retention,
        subscribers: BTreeMap::from([
            ("fast".into(), SubscriptionStart::Earliest),
            ("slow".into(), SubscriptionStart::Earliest),
        ]),
    }
}

fn event(sequence: u64) -> Result<ChangeEnvelope> {
    let change = SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", &sequence.to_string()),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: sequence,
            },
            properties: ElementPropertyMap::default(),
        },
    };
    let envelope =
        GraphChangeCodec::encode_change(change, StreamId::try_new("source/out")?, sequence, None)?;
    Ok(envelope.derive(
        envelope.id().clone(),
        envelope.changes().clone(),
        SystemMetadata::new(StreamId::try_new("source/out")?, sequence)
            .with_source_position(Bytes::copy_from_slice(&sequence.to_be_bytes())),
    ))
}

fn endpoint(
    channel: &Arc<QosChannel>,
    definition: &QosChannelDefinition,
    subscriber: &str,
    skip: bool,
) -> Result<ProvidedPipe> {
    let resource = ResourceId::try_new("channel")?;
    let mut config = definition.pipe(resource.clone(), subscriber);
    if skip {
        config.gap_policy = ReplayGapPolicy::SkipWithNotification;
    }
    Ok(config.create_with_resources(&BTreeMap::from([(resource, channel.resource())]))?)
}

async fn received(
    receiver: &mut dyn EnvelopeReceiver,
    sequence: u64,
) -> Result<Box<dyn Acknowledgement>> {
    let delivery = tokio::time::timeout(Duration::from_secs(3), receiver.receive())
        .await??
        .expect("expected a delivery");
    let (envelope, ack) = delivery.into_parts();
    assert_eq!(envelope.system().sequence(), sequence);
    assert_eq!(
        envelope.system().source_position(),
        Some(&Bytes::copy_from_slice(&sequence.to_be_bytes()))
    );
    Ok(ack.expect("QoS deliveries are explicitly acknowledged"))
}

async fn persistent(root: &Path, definition: QosChannelDefinition) -> Result<Arc<QosChannel>> {
    let provider =
        LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(root, false, false)));
    let indexes = provider.create_indexes("qos", "channel").await?;
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1024 * 1024).expect("codec limit"));
    codec.register_schema(GraphChangeCodec::schema())?;
    let channel = QosChannel::persistent(definition, indexes, codec, "journal").await?;
    assert_eq!(
        channel.durability(),
        drasi_core::interface::StorageDurability::LOCAL_PROCESS_RESTART
    );
    Ok(channel)
}

#[tokio::test]
async fn persisted_qos_corruption_is_rejected_before_delivery_or_new_acceptance() -> Result<()> {
    for fault in [
        "version",
        "stream",
        "durability",
        "head",
        "missing-cursor",
        "unknown-cursor",
        "future-cursor",
        "capacity",
        "producer-sequence",
        "missing-metadata",
        "record-gap",
        "foreign-record",
        "pruned-required-record",
        "orphan-records",
    ] {
        let directory = tempfile::tempdir()?;
        let desired = definition(true, RetentionPolicy::Backpressure, 2);
        let channel = persistent(directory.path(), desired.clone()).await?;
        channel.publish(&event(1)?).await?;
        channel.publish(&event(2)?).await?;
        channel.shutdown().await?;
        drop(channel);

        let provider = LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(
            directory.path(),
            false,
            false,
        )));
        let indexes = provider.create_indexes("qos", "channel").await?;
        let checkpoint = indexes
            .checkpoint_store()
            .expect("checkpoint store")
            .clone();
        let outbox = indexes.outbox_writer().expect("outbox").clone();
        let saved = checkpoint
            .read_checkpoint("journal")
            .await?
            .expect("committed journal");
        let mut metadata: serde_json::Value =
            serde_json::from_slice(saved.source_position.as_ref().expect("metadata"))?;
        match fault {
            "version" => metadata["version"] = serde_json::json!(2),
            "stream" => metadata["definition"]["stream"] = serde_json::json!("other/out"),
            "durability" => metadata["definition"]["durable"] = serde_json::json!(false),
            "head" => metadata["head"] = serde_json::json!(3),
            "missing-cursor" => {
                metadata["cursors"]
                    .as_object_mut()
                    .expect("cursors")
                    .remove("fast");
            }
            "unknown-cursor" => {
                metadata["cursors"]["unknown"] =
                    serde_json::json!({"position": 0, "retired": false})
            }
            "future-cursor" => metadata["cursors"]["fast"]["position"] = serde_json::json!(3),
            "capacity" => metadata["definition"]["capacity"] = serde_json::json!(1),
            "producer-sequence" => metadata["producer_sequence"] = serde_json::json!(1),
            "missing-metadata" => {}
            "record-gap" => {
                metadata["head"] = serde_json::json!(4);
                metadata["producer_sequence"] = serde_json::json!(4);
            }
            "foreign-record" | "pruned-required-record" | "orphan-records" => {}
            _ => unreachable!("declared corruption cases"),
        }
        let bytes = (fault != "missing-metadata")
            .then(|| serde_json::to_vec(&metadata).map(Bytes::from))
            .transpose()?;
        let invalid_record = if matches!(fault, "record-gap" | "foreign-record") {
            let codec = FactoryRegistry::standard()
                .envelope_codec(NonZeroUsize::new(1024 * 1024).expect("limit"))?;
            let envelope = if fault == "record-gap" {
                event(4)?
            } else {
                let original = event(2)?;
                original.derive(
                    original.id().clone(),
                    original.changes().clone(),
                    SystemMetadata::new(StreamId::try_new("other/out")?, 2),
                )
            };
            Some((
                if fault == "record-gap" { 4 } else { 2 },
                codec.encode(&envelope)?,
            ))
        } else {
            None
        };
        let transaction = drasi_core::computation::ComputationTransaction::try_new(indexes)?;
        transaction
            .run(async {
                if let Some((position, record)) = &invalid_record {
                    outbox
                        .append_and_trim("journal", *position, record, 1)
                        .await?;
                }
                if fault == "pruned-required-record" {
                    outbox.trim_before("journal", 2).await?;
                }
                checkpoint
                    .stage_checkpoint(
                        "journal",
                        if fault == "record-gap" {
                            4
                        } else {
                            saved.sequence
                        },
                        bytes.as_ref(),
                    )
                    .await?;
                Ok(())
            })
            .await?;
        if fault == "orphan-records" {
            checkpoint.clear_checkpoints().await?;
        }
        drop(checkpoint);
        drop(outbox);
        transaction.shutdown().await?;
        drop(transaction);
        drop(provider);
        let failure = match persistent(directory.path(), desired).await {
            Ok(channel) => {
                channel.shutdown().await?;
                anyhow::bail!("{fault}: corrupted journal was accepted");
            }
            Err(error) => error,
        };
        let expected = match fault {
            "capacity" | "producer-sequence" => "head and retained data disagree",
            "missing-metadata" => "metadata is missing",
            "record-gap" => "sequence gap",
            "foreign-record" => "another producer stream",
            "pruned-required-record" => "pruned required history",
            "orphan-records" => "no committed metadata",
            _ => "configuration or progress is inconsistent",
        };
        assert!(
            failure.to_string().contains(expected),
            "{fault}: {failure:#}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn rejected_qos_reconfiguration_preserves_every_existing_subscriber_obligation() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let desired = definition(true, RetentionPolicy::Backpressure, 2);
    let channel = persistent(directory.path(), desired.clone()).await?;
    channel.publish(&event(1)?).await?;
    channel.publish(&event(2)?).await?;
    channel.shutdown().await?;
    drop(channel);
    for unavailable_start in [None, Some(SubscriptionStart::After(3))] {
        let mut changed = desired.clone();
        if let Some(start) = unavailable_start {
            changed.subscribers.insert("new".into(), start);
        } else {
            changed.capacity = NonZeroUsize::new(1).expect("capacity");
        }
        let error = persistent(directory.path(), changed)
            .await
            .err()
            .expect("must not discard required or invent future history");
        let expected = if unavailable_start.is_some() {
            "unavailable history"
        } else {
            "discard required history"
        };
        assert!(error.to_string().contains(expected), "{error:#}");
    }
    let restored = persistent(directory.path(), desired.clone()).await?;
    for subscriber in ["fast", "slow"] {
        let mut pipe = endpoint(&restored, &desired, subscriber, false)?;
        let mut receiver = pipe.pipe.take_receiver()?;
        for sequence in 1..=2 {
            received(receiver.as_mut(), sequence)
                .await?
                .complete(HandlingOutcome::Handled)
                .await?;
        }
    }
    assert_eq!(
        restored.progress().await?.processed,
        BTreeMap::from([("fast".into(), 2), ("slow".into(), 2)])
    );
    restored.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn persistent_qos_rejects_volatile_declarations_and_retired_identity_reuse() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let error = persistent(
        directory.path(),
        definition(false, RetentionPolicy::Backpressure, 2),
    )
    .await
    .err()
    .expect("volatile declaration cannot provide durability");
    assert!(error
        .to_string()
        .contains("durable checkpoints and a durable declaration"));
    let original = definition(true, RetentionPolicy::Backpressure, 2);
    let channel = persistent(directory.path(), original.clone()).await?;
    channel.publish(&event(1)?).await?;
    channel.shutdown().await?;
    drop(channel);
    let mut reduced = original.clone();
    reduced.subscribers.remove("fast");
    let channel = persistent(directory.path(), reduced.clone()).await?;
    channel.shutdown().await?;
    drop(channel);
    let error = persistent(directory.path(), original)
        .await
        .err()
        .expect("retired identity stays retired");
    assert!(
        error.to_string().contains("requires a new identity"),
        "{error:#}"
    );

    reduced
        .subscribers
        .insert("new-fast".into(), SubscriptionStart::Earliest);
    reduced
        .subscribers
        .insert("latest".into(), SubscriptionStart::Latest);
    let channel = persistent(directory.path(), reduced.clone()).await?;
    let progress = channel.progress().await?;
    assert_eq!(progress.processed["new-fast"], 0);
    assert_eq!(progress.processed["latest"], 1);
    assert_eq!(progress.processed["slow"], 0);
    let mut pipe = endpoint(&channel, &reduced, "new-fast", false)?;
    received(pipe.pipe.take_receiver()?.as_mut(), 1)
        .await?
        .complete(HandlingOutcome::Handled)
        .await?;
    channel.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn persistent_pruning_preserves_strict_gaps_until_the_subscriber_explicitly_allows_loss(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let desired = definition(true, RetentionPolicy::PruneOldest, 2);
    let channel = persistent(directory.path(), desired.clone()).await?;
    let mut fast = endpoint(&channel, &desired, "fast", false)?;
    let mut receiver = fast.pipe.take_receiver()?;
    for sequence in 1..=4 {
        channel.publish(&event(sequence)?).await?;
        received(receiver.as_mut(), sequence)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
    }
    channel.shutdown().await?;
    drop((receiver, fast, channel));
    let channel = persistent(directory.path(), desired.clone()).await?;
    assert_eq!(channel.progress().await?.processed["fast"], 4);
    assert_eq!(channel.progress().await?.processed["slow"], 0);
    let mut strict = endpoint(&channel, &desired, "slow", false)?;
    let mut receiver = strict.pipe.take_receiver()?;
    assert!(matches!(
        receiver.receive().await,
        Err(PipeError::PositionUnavailable {
            requested: 0,
            oldest: 3
        })
    ));
    assert_eq!(
        channel.progress().await?.processed["slow"],
        0,
        "strict gaps do not silently skip"
    );
    drop((receiver, strict));
    let mut skip = endpoint(&channel, &desired, "slow", true)?;
    let mut receiver = skip.pipe.take_receiver()?;
    for sequence in 3..=4 {
        received(receiver.as_mut(), sequence)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
    }
    channel.shutdown().await?;
    drop((receiver, skip, channel));
    let channel = persistent(directory.path(), desired).await?;
    let progress = channel.progress().await?;
    assert_eq!(progress.accepted, 4);
    assert_eq!(
        progress.processed,
        BTreeMap::from([("fast".into(), 4), ("slow".into(), 4)])
    );
    channel.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn blocking_multicast_retires_only_after_every_subscriber_handles() -> Result<()> {
    let definition = definition(false, RetentionPolicy::Backpressure, 1);
    let channel = QosChannel::volatile(definition.clone())?;
    let mut fast = endpoint(&channel, &definition, "fast", false)?;
    let mut slow = endpoint(&channel, &definition, "slow", false)?;
    let mut fast_rx = fast.pipe.take_receiver()?;
    let mut slow_rx = slow.pipe.take_receiver()?;
    let first = event(1)?;
    assert_eq!(channel.publish(&first).await?.position(), Some(1));
    assert_eq!(channel.publish(&first).await?.position(), Some(1));
    received(fast_rx.as_mut(), 1)
        .await?
        .complete(HandlingOutcome::Handled)
        .await?;
    let held = received(slow_rx.as_mut(), 1).await?;
    let second = event(2)?;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), channel.publish(&second))
            .await
            .is_err()
    );
    drop(held);
    let retried = received(slow_rx.as_mut(), 1).await?;
    retried.complete(HandlingOutcome::Handled).await?;
    assert_eq!(channel.publish(&second).await?.position(), Some(2));
    received(fast_rx.as_mut(), 2)
        .await?
        .complete(HandlingOutcome::Handled)
        .await?;
    received(slow_rx.as_mut(), 2)
        .await?
        .complete(HandlingOutcome::Handled)
        .await?;
    let progress = channel.progress().await?;
    assert_eq!(progress.accepted, 2);
    assert_eq!(
        progress.processed,
        BTreeMap::from([("fast".into(), 2), ("slow".into(), 2)])
    );
    channel.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn lossy_multicast_reports_gaps_and_requires_explicit_skip() -> Result<()> {
    let definition = definition(false, RetentionPolicy::PruneOldest, 1);
    let channel = QosChannel::volatile(definition.clone())?;
    let mut strict = endpoint(&channel, &definition, "fast", false)?;
    let mut skipping = endpoint(&channel, &definition, "slow", true)?;
    let mut strict_rx = strict.pipe.take_receiver()?;
    let mut skipping_rx = skipping.pipe.take_receiver()?;
    channel.publish(&event(1)?).await?;
    channel.publish(&event(2)?).await?;
    assert!(matches!(
        strict_rx.receive().await,
        Err(PipeError::PositionUnavailable {
            requested: 0,
            oldest: 2
        })
    ));
    received(skipping_rx.as_mut(), 2)
        .await?
        .complete(HandlingOutcome::Handled)
        .await?;
    assert_eq!(channel.progress().await?.processed["slow"], 2);
    assert_eq!(channel.progress().await?.processed["fast"], 0);
    channel.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn persistent_multicast_reopens_independent_progress_and_accepted_source_position(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let definition = definition(true, RetentionPolicy::Backpressure, 3);
    {
        let channel = persistent(directory.path(), definition.clone()).await?;
        let mut fast = endpoint(&channel, &definition, "fast", false)?;
        let mut slow = endpoint(&channel, &definition, "slow", false)?;
        let mut fast_rx = fast.pipe.take_receiver()?;
        let mut slow_rx = slow.pipe.take_receiver()?;
        channel.publish(&event(1)?).await?;
        channel.publish(&event(2)?).await?;
        received(fast_rx.as_mut(), 1)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        received(fast_rx.as_mut(), 2)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        drop(received(slow_rx.as_mut(), 1).await?);
        assert_eq!(channel.progress().await?.processed["slow"], 0);
        channel.shutdown().await?;
    }
    {
        let channel = persistent(directory.path(), definition.clone()).await?;
        let progress = channel.progress().await?;
        assert_eq!(progress.producer_sequence, Some(2));
        assert_eq!(
            progress.source_position,
            Some(Bytes::copy_from_slice(&2u64.to_be_bytes()))
        );
        let mut fast = endpoint(&channel, &definition, "fast", false)?;
        let mut slow = endpoint(&channel, &definition, "slow", false)?;
        let mut fast_rx = fast.pipe.take_receiver()?;
        let mut slow_rx = slow.pipe.take_receiver()?;
        assert!(
            tokio::time::timeout(Duration::from_millis(30), fast_rx.receive())
                .await
                .is_err()
        );
        received(slow_rx.as_mut(), 1)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        received(slow_rx.as_mut(), 2)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        let third = event(3)?;
        assert_eq!(channel.publish(&third).await?.position(), Some(3));
        assert_eq!(channel.publish(&third).await?.position(), Some(3));
        received(fast_rx.as_mut(), 3)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        received(slow_rx.as_mut(), 3)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        channel.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn cancelled_subscription_keeps_history_and_stale_ack_cannot_advance_a_replacement(
) -> Result<()> {
    let definition = definition(false, RetentionPolicy::Backpressure, 1);
    let channel = QosChannel::volatile(definition.clone())?;
    let mut old = endpoint(&channel, &definition, "slow", false)?;
    let mut old_rx = old.pipe.take_receiver()?;
    channel.publish(&event(1)?).await?;
    let stale = received(old_rx.as_mut(), 1).await?;
    old.control.cancel();
    drop(old_rx);
    let mut replacement = endpoint(&channel, &definition, "slow", false)?;
    let mut replacement_rx = replacement.pipe.take_receiver()?;
    assert!(stale.complete(HandlingOutcome::Handled).await.is_err());
    assert_eq!(channel.progress().await?.processed["slow"], 0);
    received(replacement_rx.as_mut(), 1)
        .await?
        .complete(HandlingOutcome::Handled)
        .await?;
    assert!(channel.retire("slow").await.is_err());
    replacement.control.cancel();
    channel.retire("slow").await?;
    assert_eq!(channel.progress().await?.retired, ["slow"]);
    channel.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn membership_change_preserves_existing_progress_and_captures_new_subscription_cut(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let original = definition(true, RetentionPolicy::Backpressure, 3);
    {
        let channel = persistent(directory.path(), original.clone()).await?;
        let mut fast = endpoint(&channel, &original, "fast", false)?;
        let mut receiver = fast.pipe.take_receiver()?;
        for sequence in 1..=3 {
            channel.publish(&event(sequence)?).await?;
        }
        received(receiver.as_mut(), 1)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        received(receiver.as_mut(), 2)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        channel.shutdown().await?;
    }
    let mut updated = original;
    updated.subscribers.remove("slow");
    updated
        .subscribers
        .insert("new".into(), SubscriptionStart::Latest);
    {
        let channel = persistent(directory.path(), updated.clone()).await?;
        let progress = channel.progress().await?;
        assert_eq!(progress.processed["fast"], 2);
        assert_eq!(progress.processed["new"], 3);
        assert_eq!(progress.retired, ["slow"]);
        let mut fast = endpoint(&channel, &updated, "fast", false)?;
        let mut new = endpoint(&channel, &updated, "new", false)?;
        let mut fast_rx = fast.pipe.take_receiver()?;
        let mut new_rx = new.pipe.take_receiver()?;
        received(fast_rx.as_mut(), 3)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        channel.publish(&event(4)?).await?;
        received(fast_rx.as_mut(), 4)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        received(new_rx.as_mut(), 4)
            .await?
            .complete(HandlingOutcome::Handled)
            .await?;
        channel.shutdown().await?;
    }
    let channel = persistent(directory.path(), updated).await?;
    assert_eq!(channel.progress().await?.processed["new"], 4);
    channel.shutdown().await?;
    Ok(())
}

struct FiniteSource {
    descriptor: ComponentDescriptor,
    sequence: u64,
}
#[async_trait]
impl ComputationComponent for FiniteSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for FiniteSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        if self.sequence == 5 {
            return Ok(None);
        }
        self.sequence += 1;
        Ok(Some(OutputEnvelope {
            port: PortId::try_new("out")?,
            envelope: event(self.sequence)?,
        }))
    }
}

struct RecordingSink {
    descriptor: ComponentDescriptor,
    seen: Arc<std::sync::Mutex<Vec<u64>>>,
}
#[async_trait]
impl ComputationComponent for RecordingSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for RecordingSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.seen
            .lock()
            .expect("recorded events")
            .push(input.envelope.system().sequence());
        Ok(())
    }
}

fn descriptor(id: &str, port: &str, direction: PortDirection) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        ComponentId::try_new(id).expect("component"),
        vec![PortDescriptor::new(
            PortId::try_new(port).expect("port"),
            direction,
            GraphChangeCodec::schema().descriptor().clone(),
            PipeRequirements::default(),
        )],
    )
    .expect("descriptor")
}

#[tokio::test]
async fn graph_fanout_appends_once_and_both_consumers_complete_every_event() -> Result<()> {
    let definition = definition(false, RetentionPolicy::Backpressure, 1);
    let channel = QosChannel::volatile(definition.clone())?;
    let resource = ResourceId::try_new("journal")?;
    let fast = Arc::new(std::sync::Mutex::new(Vec::new()));
    let slow = Arc::new(std::sync::Mutex::new(Vec::new()));
    let source = Endpoint::new(ComponentId::try_new("source")?, PortId::try_new("out")?);
    let mut graph = ComputationGraph::builder("multicast")
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "journal".into(),
        })?
        .provide_resource(resource.clone(), channel.resource())?
        .source(Box::new(FiniteSource {
            descriptor: descriptor("source", "out", PortDirection::Output),
            sequence: 0,
        }))
        .sink(Box::new(RecordingSink {
            descriptor: descriptor("fast", "in", PortDirection::Input),
            seen: fast.clone(),
        }))
        .sink(Box::new(RecordingSink {
            descriptor: descriptor("slow", "in", PortDirection::Input),
            seen: slow.clone(),
        }))
        .bind_stream(source.clone(), definition.stream.clone())
        .connect(
            EdgeDefinition::new(
                source.clone(),
                Endpoint::new(ComponentId::try_new("fast")?, PortId::try_new("in")?),
            ),
            Box::new(definition.pipe(resource.clone(), "fast")),
        )
        .connect(
            EdgeDefinition::new(
                source,
                Endpoint::new(ComponentId::try_new("slow")?, PortId::try_new("in")?),
            ),
            Box::new(definition.pipe(resource, "slow")),
        )
        .build()?;
    tokio::time::timeout(Duration::from_secs(5), graph.start()?).await??;
    assert_eq!(*fast.lock().unwrap(), [1, 2, 3, 4, 5]);
    assert_eq!(*slow.lock().unwrap(), [1, 2, 3, 4, 5]);
    let progress = channel.progress().await?;
    assert_eq!(progress.accepted, 5);
    assert_eq!(progress.processed["fast"], 5);
    assert_eq!(progress.processed["slow"], 5);
    graph.shutdown().await?;
    channel.shutdown().await?;
    Ok(())
}

struct ModelConsumer {
    id: String,
    connection: ProvidedPipe,
    receiver: Box<dyn EnvelopeReceiver>,
    pending: Option<(u64, Box<dyn Acknowledgement>)>,
    cursor: u64,
    retired: bool,
    skip: bool,
}

#[tokio::test]
async fn seeded_qos_schedules_match_an_independent_acceptance_and_completion_ledger() -> Result<()>
{
    let mut exercised = [0; 8];
    for seed in 1..=128_u64 {
        let retention = if seed % 2 == 0 {
            RetentionPolicy::Backpressure
        } else {
            RetentionPolicy::PruneOldest
        };
        let mut definition = definition(false, retention, (seed % 4 + 1) as usize);
        definition.subscribers = (0..4)
            .map(|index| (format!("consumer-{index}"), SubscriptionStart::Earliest))
            .collect();
        let channel = QosChannel::volatile(definition.clone())?;
        let mut consumers = Vec::new();
        for (index, id) in definition.subscribers.keys().enumerate() {
            let skip = retention == RetentionPolicy::PruneOldest && index % 2 == 0;
            let mut connection = endpoint(&channel, &definition, id, skip)?;
            let receiver = connection.pipe.take_receiver()?;
            consumers.push(ModelConsumer {
                id: id.clone(),
                connection,
                receiver,
                pending: None,
                cursor: 0,
                retired: false,
                skip,
            });
        }
        let mut accepted: Vec<ChangeEnvelope> = Vec::new();
        let mut schedule = seed;
        tokio::time::timeout(Duration::from_secs(5), async {
            for step in 0..1024 {
                schedule = schedule
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let index = ((schedule >> 32) % consumers.len() as u64) as usize;
                let action = ((schedule >> 40) % 8) as usize;
                let head = accepted.len() as u64;
                let oldest = head
                    .saturating_sub(definition.capacity.get() as u64)
                    .saturating_add(1);
                if action == 0 || action == 1 || action == 6 && step < 768 {
                    let next = event(head * 3)?;
                    let blocked = retention == RetentionPolicy::Backpressure
                        && head >= definition.capacity.get() as u64
                        && consumers
                            .iter()
                            .any(|consumer| !consumer.retired && consumer.cursor < oldest);
                    if blocked {
                        assert!(
                            futures::poll!(Box::pin(channel.publish(&next))).is_pending(),
                            "seed={seed}, step={step}"
                        );
                        exercised[0] += 1;
                    } else {
                        assert_eq!(channel.publish(&next).await?.position(), Some(head + 1));
                        accepted.push(next);
                        exercised[1] += 1;
                    }
                } else if action == 7 && head != 0 {
                    let offset = ((schedule >> 48) % head) as usize;
                    let result = channel.publish(&accepted[offset]).await;
                    if offset as u64 + 1 < oldest {
                        assert!(result.is_err(), "expired producer retry must not append");
                    } else {
                        assert_eq!(result?.position(), Some(offset as u64 + 1));
                    }
                    exercised[7] += 1;
                } else {
                    let consumer = &mut consumers[index];
                    if consumer.retired {
                        continue;
                    }
                    match action {
                        2 => {
                            if consumer.pending.is_some() || consumer.cursor == head {
                                assert!(futures::poll!(consumer.receiver.receive()).is_pending());
                            } else if consumer.cursor + 1 < oldest && !consumer.skip {
                                assert!(matches!(consumer.receiver.receive().await,
                                    Err(PipeError::PositionUnavailable { requested, oldest: found })
                                        if requested == consumer.cursor && found == oldest));
                            } else {
                                if consumer.cursor + 1 < oldest {
                                    consumer.cursor = oldest - 1;
                                }
                                let position = consumer.cursor + 1;
                                let delivery = consumer
                                    .receiver
                                    .receive()
                                    .await?
                                    .expect("ledger has pending work");
                                let (envelope, acknowledgement) = delivery.into_parts();
                                assert_eq!(envelope.id(), accepted[position as usize - 1].id());
                                assert_eq!(
                                    envelope.system().source_position(),
                                    accepted[position as usize - 1].system().source_position()
                                );
                                consumer.pending =
                                    Some((position, acknowledgement.expect("handled completion")));
                                exercised[2] += 1;
                            }
                        }
                        3 => {
                            if let Some((position, acknowledgement)) = consumer.pending.take() {
                                acknowledgement.complete(HandlingOutcome::Handled).await?;
                                consumer.cursor = position;
                                exercised[3] += 1;
                            }
                        }
                        4 => {
                            exercised[4] += usize::from(consumer.pending.take().is_some());
                        }
                        5 => {
                            consumer.connection.control.cancel();
                            if let Some((_, acknowledgement)) = consumer.pending.take() {
                                assert!(matches!(
                                    acknowledgement.complete(HandlingOutcome::Handled).await,
                                    Err(PipeError::Closed)
                                ));
                            }
                            let mut connection =
                                endpoint(&channel, &definition, &consumer.id, consumer.skip)?;
                            consumer.receiver = connection.pipe.take_receiver()?;
                            consumer.connection = connection;
                            exercised[5] += 1;
                        }
                        6 => {
                            consumer.connection.control.cancel();
                            consumer.pending.take();
                            channel.retire(&consumer.id).await?;
                            consumer.retired = true;
                            exercised[6] += 1;
                        }
                        _ => {}
                    }
                }
                let progress = channel.progress().await?;
                assert_eq!(
                    progress.accepted,
                    accepted.len() as u64,
                    "seed={seed}, step={step}"
                );
                assert_eq!(
                    progress.earliest_available,
                    if accepted.is_empty() {
                        None
                    } else {
                        Some(
                            (accepted.len() as u64)
                                .saturating_sub(definition.capacity.get() as u64)
                                + 1,
                        )
                    }
                );
                for consumer in &consumers {
                    assert_eq!(
                        progress.processed[&consumer.id], consumer.cursor,
                        "seed={seed}, step={step}, consumer={}",
                        consumer.id
                    );
                    assert_eq!(progress.retired.contains(&consumer.id), consumer.retired);
                }
                assert_eq!(
                    progress.producer_sequence,
                    accepted.last().map(|event| event.system().sequence())
                );
            }
            Ok::<(), anyhow::Error>(())
        })
        .await??;
        channel.shutdown().await?;
    }
    assert!(
        exercised.iter().all(|count| *count > 0),
        "schedule did not exercise every operation: {exercised:?}"
    );
    Ok(())
}

struct CommitGate {
    before_commit: bool,
    entered: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[tokio::test]
async fn cancelled_append_and_completion_resolve_exactly_before_and_after_real_commit() -> Result<()>
{
    use std::sync::atomic::{AtomicBool, Ordering};
    for acknowledgement in [false, true] {
        for before_commit in [false, true] {
            let directory = tempfile::tempdir()?;
            let definition = definition(true, RetentionPolicy::Backpressure, 2);
            let fail = Arc::new(AtomicBool::new(false));
            let gate = Arc::new(CommitGate {
                before_commit,
                entered: tokio::sync::Notify::new(),
                resume: tokio::sync::Notify::new(),
            });
            let channel = uncertain_channel(
                directory.path(),
                definition.clone(),
                fail.clone(),
                Some(gate.clone()),
            )
            .await?;
            let first = event(0)?;
            let completion = if acknowledgement {
                channel.publish(&first).await?;
                let mut connection = endpoint(&channel, &definition, "fast", false)?;
                let mut receiver = connection.pipe.take_receiver()?;
                let acknowledgement = received(receiver.as_mut(), 0).await?;
                // Keep the subscription owner alive until its completion future is dropped.
                Some((connection, receiver, acknowledgement))
            } else {
                None
            };
            fail.store(true, Ordering::Release);
            let mut operation = Box::pin(async {
                if let Some((connection, receiver, acknowledgement)) = completion {
                    let result = acknowledgement.complete(HandlingOutcome::Handled).await;
                    drop((connection, receiver));
                    result
                } else {
                    channel.publish(&first).await.map(|_| ())
                }
            });
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::select! {
                    _ = gate.entered.notified() => {},
                    result = &mut operation => panic!("commit reply deliberately withheld: {result:?}"),
                }
            }).await?;
            drop(operation);
            assert!(
                channel.progress().await.is_err(),
                "cancelled storage work fences the cached owner"
            );
            channel.shutdown().await?;
            let reopened = persistent(directory.path(), definition).await?;
            let expected_accepted = u64::from(acknowledgement || !before_commit);
            let progress = reopened.progress().await?;
            assert_eq!(progress.accepted, expected_accepted);
            assert_eq!(
                progress.producer_sequence,
                (expected_accepted == 1).then_some(0)
            );
            assert_eq!(
                progress.processed["fast"],
                u64::from(acknowledgement && !before_commit)
            );
            assert_eq!(progress.processed["slow"], 0);
            assert_eq!(reopened.publish(&first).await?.position(), Some(1));
            assert_eq!(reopened.progress().await?.accepted, 1);
            reopened.shutdown().await?;
        }
    }
    Ok(())
}

struct LoseCommitResponse {
    inner: Arc<dyn drasi_core::interface::SessionControl>,
    fail: Arc<std::sync::atomic::AtomicBool>,
    gate: Option<Arc<CommitGate>>,
    skip: std::sync::atomic::AtomicUsize,
}
#[async_trait]
impl drasi_core::interface::SessionControl for LoseCommitResponse {
    async fn begin(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        if self.fail.load(std::sync::atomic::Ordering::Acquire)
            && self
                .skip
                .fetch_update(
                    std::sync::atomic::Ordering::AcqRel,
                    std::sync::atomic::Ordering::Acquire,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
        {
            return self.inner.commit().await;
        }
        let fail = self.fail.swap(false, std::sync::atomic::Ordering::AcqRel);
        if let Some(gate) = self.gate.as_ref().filter(|gate| fail && gate.before_commit) {
            gate.entered.notify_one();
            gate.resume.notified().await;
        }
        self.inner.commit().await?;
        if fail {
            if let Some(gate) = self.gate.as_ref().filter(|gate| !gate.before_commit) {
                gate.entered.notify_one();
                gate.resume.notified().await;
            }
            if self.gate.is_none() {
                return Err(drasi_core::interface::IndexError::IOError);
            }
        }
        Ok(())
    }
    fn rollback(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        self.inner.rollback()
    }
}

#[tokio::test]
async fn endpoint_revocation_during_commit_reports_unknown_and_recovers_exact_durable_progress(
) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    for acknowledgement in [false, true] {
        for before_commit in [false, true] {
            let directory = tempfile::tempdir()?;
            let definition = definition(true, RetentionPolicy::Backpressure, 2);
            let fail = Arc::new(AtomicBool::new(false));
            let gate = Arc::new(CommitGate {
                before_commit,
                entered: tokio::sync::Notify::new(),
                resume: tokio::sync::Notify::new(),
            });
            let channel = uncertain_channel(
                directory.path(),
                definition.clone(),
                fail.clone(),
                Some(gate.clone()),
            )
            .await?;
            let mut pipe = endpoint(&channel, &definition, "fast", false)?;
            let sender = pipe.pipe.sender();
            let mut receiver = pipe.pipe.take_receiver()?;
            let first = event(1)?;
            let ack = if acknowledgement {
                channel.publish(&first).await?;
                Some(received(receiver.as_mut(), 1).await?)
            } else {
                None
            };
            fail.store(true, Ordering::Release);
            let mut operation = Box::pin(async {
                if let Some(ack) = ack {
                    ack.complete(HandlingOutcome::Handled).await
                } else {
                    sender
                        .send(first.clone())
                        .await
                        .map(|_| ())
                        .map_err(|failure| failure.error)
                }
            });
            tokio::time::timeout(Duration::from_secs(3), async {
                tokio::select! {
                    _ = gate.entered.notified() => {}
                    result = &mut operation => panic!("commit barrier was not reached: {result:?}"),
                }
            })
            .await?;
            pipe.control.cancel();
            gate.resume.notify_one();
            let failure = tokio::time::timeout(Duration::from_secs(3), operation)
                .await?
                .expect_err("revoked owner cannot return success");
            if acknowledgement {
                assert!(matches!(failure, PipeError::AcknowledgementUnknown { .. }));
            } else {
                assert!(matches!(failure, PipeError::AcceptanceUnknown { .. }));
            }
            channel.shutdown().await?;
            drop((receiver, sender, pipe, channel));
            let reopened = persistent(directory.path(), definition).await?;
            let progress = reopened.progress().await?;
            assert_eq!(progress.accepted, 1);
            assert_eq!(progress.producer_sequence, Some(1));
            assert_eq!(progress.processed["fast"], u64::from(acknowledgement));
            assert_eq!(progress.processed["slow"], 0);
            assert_eq!(reopened.publish(&first).await?.position(), Some(1));
            reopened.shutdown().await?;
        }
    }
    Ok(())
}

async fn uncertain_channel(
    root: &Path,
    definition: QosChannelDefinition,
    fail: Arc<std::sync::atomic::AtomicBool>,
    gate: Option<Arc<CommitGate>>,
) -> Result<Arc<QosChannel>> {
    let indexes = uncertain_indexes(root, fail, gate).await?;
    Ok(QosChannel::persistent(
        definition,
        indexes,
        FactoryRegistry::standard()
            .envelope_codec(NonZeroUsize::new(1024 * 1024).expect("codec limit"))?,
        "journal",
    )
    .await?)
}

async fn uncertain_indexes(
    root: &Path,
    fail: Arc<std::sync::atomic::AtomicBool>,
    gate: Option<Arc<CommitGate>>,
) -> Result<drasi_core::computation::ComputationIndexes> {
    uncertain_indexes_after(root, fail, gate, 0).await
}

async fn uncertain_indexes_after(
    root: &Path,
    fail: Arc<std::sync::atomic::AtomicBool>,
    gate: Option<Arc<CommitGate>>,
    skip: usize,
) -> Result<drasi_core::computation::ComputationIndexes> {
    let provider =
        LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(root, false, false)));
    let original = provider.create_indexes("qos", "channel").await?;
    let control: Arc<dyn drasi_core::interface::SessionControl> = Arc::new(LoseCommitResponse {
        inner: original.indexes().session_control.clone(),
        fail,
        gate,
        skip: std::sync::atomic::AtomicUsize::new(skip),
    });
    let outbox = original.outbox_writer().expect("outbox").clone();
    wrapped_indexes(original, control, outbox)
}

fn wrapped_indexes(
    original: drasi_core::computation::ComputationIndexes,
    control: Arc<dyn drasi_core::interface::SessionControl>,
    outbox: Arc<dyn drasi_core::interface::OutboxWriter>,
) -> Result<drasi_core::computation::ComputationIndexes> {
    use drasi_core::{
        computation::{ComputationIndexes, ComputationResource, TransactionDomain},
        interface::IndexSet,
    };
    let domain = TransactionDomain::new(control.clone());
    let set = original.indexes();
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
            original
                .checkpoint_store()
                .expect("checkpoint store")
                .clone(),
            &domain,
        )),
        Some(ComputationResource::participating(outbox, &domain)),
        Some(ComputationResource::participating(
            original
                .live_results_writer()
                .expect("live results")
                .clone(),
            &domain,
        )),
    )?
    .with_cleanup(original.cleanup().expect("storage owner").clone())
    .with_durability(original.durability());
    Ok(indexes)
}

#[tokio::test]
async fn ambiguous_acceptance_and_acknowledgement_recover_the_committed_state() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    for acknowledge in [false, true] {
        let directory = tempfile::tempdir()?;
        let definition = definition(true, RetentionPolicy::Backpressure, 2);
        let first = event(1)?;
        {
            let fail = Arc::new(AtomicBool::new(false));
            let channel =
                uncertain_channel(directory.path(), definition.clone(), fail.clone(), None).await?;
            if acknowledge {
                channel.publish(&first).await?;
                let mut endpoint = endpoint(&channel, &definition, "fast", false)?;
                let mut receiver = endpoint.pipe.take_receiver()?;
                let acknowledgement = received(receiver.as_mut(), 1).await?;
                fail.store(true, Ordering::Release);
                assert!(matches!(
                    acknowledgement.complete(HandlingOutcome::Handled).await,
                    Err(PipeError::AcknowledgementUnknown { .. })
                ));
            } else {
                fail.store(true, Ordering::Release);
                assert!(matches!(
                    channel.publish(&first).await,
                    Err(PipeError::AcceptanceUnknown { .. })
                ));
            }
            assert!(
                channel.progress().await.is_err(),
                "ambiguous writes fence the cached owner"
            );
            channel.shutdown().await?;
        }
        let channel = persistent(directory.path(), definition).await?;
        let progress = channel.progress().await?;
        assert_eq!(progress.accepted, 1);
        assert_eq!(progress.processed["fast"], u64::from(acknowledge));
        assert_eq!(channel.publish(&first).await?.position(), Some(1));
        assert_eq!(
            channel.progress().await?.accepted,
            1,
            "retry must not append again"
        );
        channel.shutdown().await?;
    }
    Ok(())
}

fn admission_options() -> Result<AdmissionOptions> {
    Ok(AdmissionOptions {
        construction_scope: "instance".into(),
        graph_id: "qos".into(),
        component_id: ComponentId::try_new("source")?,
        failure_scope: drasi_core::interface::FailureMode::ProcessRestart,
        max_producers: NonZeroUsize::new(2).expect("producer limit"),
        receipts_per_producer: NonZeroUsize::new(2).expect("receipt window"),
    })
}

fn rejected(error: PipeError, expected: AdmissionRejection) {
    match error {
        PipeError::Backend(error) => assert_eq!(
            error.downcast_ref::<AdmissionRejection>(),
            Some(&expected),
            "{error:#}"
        ),
        error => panic!("expected typed admission rejection, got {error}"),
    }
}

async fn handle_admitted(
    channel: &Arc<QosChannel>,
    definition: &QosChannelDefinition,
    sequence: u64,
) -> Result<()> {
    for subscriber in ["fast", "slow"] {
        let mut connection = endpoint(channel, definition, subscriber, false)?;
        let mut receiver = connection.pipe.take_receiver()?;
        let delivery = receiver.receive().await?.expect("admitted event");
        let progress =
            GraphProducerProgress::from_envelope(delivery.envelope())?.expect("durable producer");
        assert!(progress.identity().persistent());
        assert_eq!(progress.sequence(), sequence);
        assert_eq!(delivery.envelope().system().sequence(), sequence);
        delivery
            .into_parts()
            .1
            .expect("completion")
            .complete(HandlingOutcome::Handled)
            .await?;
        connection.control.cancel();
    }
    Ok(())
}

#[tokio::test]
async fn producer_admission_atomically_replays_receipts_and_bounds_retired_sessions() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let definition = definition(true, RetentionPolicy::Backpressure, 1);
    let channel = persistent(directory.path(), definition.clone()).await?;
    channel.enable_admission(admission_options()?).await?;
    let name = ComponentId::try_new("client-a")?;
    let session = channel.register_producer(name.clone()).await?;
    assert_eq!(channel.register_producer(name.clone()).await?, session);
    let second = channel
        .register_producer(ComponentId::try_new("client-b")?)
        .await?;
    assert!(matches!(
        channel
            .register_producer(ComponentId::try_new("client-c")?)
            .await,
        Err(PipeError::CapacityExhausted)
    ));
    assert!(channel.admission_receipt(&session, 1).await?.is_none());
    rejected(
        channel.admit(&session, 0, &event(1)?).await.unwrap_err(),
        AdmissionRejection::Sequence {
            received: 0,
            expected: Some(1),
        },
    );
    rejected(
        channel.admit(&session, 2, &event(1)?).await.unwrap_err(),
        AdmissionRejection::Sequence {
            received: 2,
            expected: Some(1),
        },
    );
    let receipt = channel.admit(&session, 1, &event(1)?).await?;
    assert_eq!(receipt.position, 1);
    assert_eq!(channel.admit(&session, 1, &event(1)?).await?, receipt);
    rejected(
        channel.admit(&session, 1, &event(2)?).await.unwrap_err(),
        AdmissionRejection::PayloadConflict(1),
    );
    assert!(matches!(
        channel.admit(&second, 1, &event(2)?).await,
        Err(PipeError::CapacityExhausted)
    ));
    rejected(
        channel.retire_producer(&session).await.unwrap_err(),
        AdmissionRejection::PendingObligations,
    );
    assert!(
        channel.publish(&event(2)?).await.is_err(),
        "raw publication cannot bypass producer receipts"
    );
    channel.shutdown().await?;
    drop(channel);

    let restored = persistent(directory.path(), definition.clone()).await?;
    restored.enable_admission(admission_options()?).await?;
    assert_eq!(restored.register_producer(name.clone()).await?, session);
    assert_eq!(
        restored.admission_receipt(&session, 1).await?,
        Some(receipt.clone())
    );
    assert_eq!(restored.admit(&session, 1, &event(1)?).await?, receipt);
    for sequence in 1..=3 {
        if sequence > 1 {
            assert_eq!(
                restored
                    .admit(&session, sequence, &event(sequence)?)
                    .await?
                    .position,
                sequence
            );
        }
        handle_admitted(&restored, &definition, sequence).await?;
    }
    rejected(
        restored.admit(&session, 1, &event(1)?).await.unwrap_err(),
        AdmissionRejection::ReceiptExpired(1),
    );
    rejected(
        restored.admission_receipt(&session, 1).await.unwrap_err(),
        AdmissionRejection::ReceiptExpired(1),
    );
    assert_eq!(
        restored.admit(&session, 2, &event(2)?).await?.position,
        2,
        "receipt survives pruning of its already handled input"
    );
    restored.retire_producer(&session).await?;
    let fresh = restored.register_producer(name).await?;
    assert_ne!(fresh.epoch, session.epoch);
    rejected(
        restored.admit(&session, 4, &event(4)?).await.unwrap_err(),
        AdmissionRejection::SessionExpired,
    );
    assert_eq!(restored.admit(&fresh, 1, &event(4)?).await?.position, 4);
    assert_eq!(restored.progress().await?.accepted, 4);
    restored.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn producer_admission_rejects_volatile_lossy_and_weakened_configuration() -> Result<()> {
    let volatile = QosChannel::volatile(definition(false, RetentionPolicy::Backpressure, 2))?;
    assert!(volatile
        .enable_admission(admission_options()?)
        .await
        .is_err());
    let directory = tempfile::tempdir()?;
    let definition = definition(true, RetentionPolicy::Backpressure, 2);
    let channel = persistent(directory.path(), definition.clone()).await?;
    let mut unsupported = admission_options()?;
    unsupported.failure_scope = drasi_core::interface::FailureMode::PowerLoss;
    assert!(
        channel.enable_admission(unsupported).await.is_err(),
        "RocksDB without sync cannot promise power-loss survival"
    );
    channel.enable_admission(admission_options()?).await?;
    let mut changed = admission_options()?;
    changed.receipts_per_producer = NonZeroUsize::new(3).expect("window");
    assert!(channel.enable_admission(changed).await.is_err());
    channel.shutdown().await?;
    drop(channel);
    let mut lossy = definition;
    lossy.retention = RetentionPolicy::PruneOldest;
    assert!(persistent(directory.path(), lossy).await.is_err());
    Ok(())
}

#[tokio::test]
async fn cancelled_producer_admission_resolves_input_and_receipt_in_one_commit() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    for before_commit in [true, false] {
        let directory = tempfile::tempdir()?;
        let definition = definition(true, RetentionPolicy::Backpressure, 2);
        let fail = Arc::new(AtomicBool::new(false));
        let gate = Arc::new(CommitGate {
            before_commit,
            entered: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        let channel = uncertain_channel(
            directory.path(),
            definition.clone(),
            fail.clone(),
            Some(gate.clone()),
        )
        .await?;
        channel.enable_admission(admission_options()?).await?;
        let session = channel
            .register_producer(ComponentId::try_new("client")?)
            .await?;
        let input = event(1)?;
        fail.store(true, Ordering::Release);
        let mut operation = Box::pin(channel.admit(&session, 1, &input));
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::select! {
                _ = gate.entered.notified() => {},
                result = &mut operation => panic!("commit response should be withheld: {result:?}"),
            }
        })
        .await?;
        rejected(
            channel.admit(&session, 1, &input).await.unwrap_err(),
            AdmissionRejection::Busy,
        );
        drop(operation);
        assert!(
            channel.admission_receipt(&session, 1).await.is_err(),
            "uncertain cached state is not a receipt authority"
        );
        channel.shutdown().await?;
        let reopened = persistent(directory.path(), definition.clone()).await?;
        let receipt = reopened.admission_receipt(&session, 1).await?;
        assert_eq!(receipt.is_some(), !before_commit);
        assert_eq!(
            reopened.progress().await?.accepted,
            u64::from(!before_commit)
        );
        assert_eq!(reopened.admit(&session, 1, &input).await?.position, 1);
        handle_admitted(&reopened, &definition, 1).await?;
        assert_eq!(reopened.progress().await?.accepted, 1);
        reopened.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn lost_producer_registration_and_acceptance_responses_are_recoverable() -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    for registration in [true, false] {
        let directory = tempfile::tempdir()?;
        let definition = definition(true, RetentionPolicy::Backpressure, 2);
        let fail = Arc::new(AtomicBool::new(false));
        let channel =
            uncertain_channel(directory.path(), definition.clone(), fail.clone(), None).await?;
        channel.enable_admission(admission_options()?).await?;
        let producer = ComponentId::try_new("client")?;
        let session = if registration {
            fail.store(true, Ordering::Release);
            assert!(matches!(
                channel.register_producer(producer.clone()).await,
                Err(PipeError::AcknowledgementUnknown { .. })
            ));
            None
        } else {
            let session = channel.register_producer(producer.clone()).await?;
            fail.store(true, Ordering::Release);
            assert!(matches!(
                channel.admit(&session, 1, &event(1)?).await,
                Err(PipeError::AcceptanceUnknown { .. })
            ));
            Some(session)
        };
        channel.shutdown().await?;
        let reopened = persistent(directory.path(), definition).await?;
        let restored = reopened.register_producer(producer).await?;
        assert_eq!(
            restored.epoch, 1,
            "lost registration response must not allocate another session"
        );
        if let Some(session) = session {
            assert_eq!(session, restored);
            assert_eq!(
                reopened
                    .admission_receipt(&session, 1)
                    .await?
                    .expect("committed receipt")
                    .position,
                1
            );
            assert_eq!(reopened.admit(&session, 1, &event(1)?).await?.position, 1);
        } else {
            assert!(reopened.admission_receipt(&restored, 1).await?.is_none());
        }
        reopened.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn admission_metadata_corruption_cannot_fabricate_a_retry_receipt() -> Result<()> {
    for fault in [
        "version",
        "scope",
        "epoch",
        "sequence-gap",
        "position",
        "missing-receipt",
        "bounds",
        "durability",
        "progress-identity",
        "shared-epoch",
        "shared-position",
        "future-client-head",
    ] {
        let directory = tempfile::tempdir()?;
        let desired = definition(true, RetentionPolicy::Backpressure, 2);
        let channel = persistent(directory.path(), desired.clone()).await?;
        channel.enable_admission(admission_options()?).await?;
        let session = channel
            .register_producer(ComponentId::try_new("client")?)
            .await?;
        channel.admit(&session, 1, &event(1)?).await?;
        let second = channel
            .register_producer(ComponentId::try_new("second")?)
            .await?;
        channel.admit(&second, 1, &event(2)?).await?;
        channel.shutdown().await?;
        drop(channel);
        let provider = LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(
            directory.path(),
            false,
            false,
        )));
        let indexes = provider.create_indexes("qos", "channel").await?;
        let checkpoint = indexes.checkpoint_store().expect("checkpoint").clone();
        let saved = checkpoint
            .read_checkpoint("journal")
            .await?
            .expect("metadata");
        let mut metadata: serde_json::Value =
            serde_json::from_slice(saved.source_position.as_ref().expect("saved metadata"))?;
        let admission = &mut metadata["admission"];
        match fault {
            "version" => admission["version"] = serde_json::json!(2),
            "scope" => admission["options"]["graph_id"] = serde_json::json!("another-graph"),
            "epoch" => admission["producers"]["client"]["epoch"] = serde_json::json!(0),
            "sequence-gap" => admission["producers"]["client"]["head"] = serde_json::json!(2),
            "position" => {
                admission["producers"]["client"]["receipts"]["1"]["position"] = serde_json::json!(0)
            }
            "missing-receipt" => {
                admission["producers"]["client"]["receipts"] = serde_json::json!({})
            }
            "bounds" => admission["options"]["max_producers"] = serde_json::json!(1025),
            "durability" => {
                admission["options"]["failure_scope"] =
                    serde_json::to_value(drasi_core::interface::FailureMode::PowerLoss)?
            }
            "progress-identity" => {
                admission["identity"]["incarnation"] =
                    serde_json::json!("b747582e-b71a-4281-85cd-9918953316ee")
            }
            "shared-epoch" => admission["producers"]["second"]["epoch"] = serde_json::json!(1),
            "shared-position" => {
                admission["producers"]["second"]["last_position"] = serde_json::json!(1);
                admission["producers"]["second"]["receipts"]["1"]["position"] =
                    serde_json::json!(1);
            }
            "future-client-head" => {
                admission["producers"]["client"]["head"] = serde_json::json!(u64::MAX)
            }
            _ => unreachable!(),
        }
        let bytes = Bytes::from(serde_json::to_vec(&metadata)?);
        let transaction = drasi_core::computation::ComputationTransaction::try_new(indexes)?;
        transaction
            .run(async {
                checkpoint
                    .stage_checkpoint("journal", 2, Some(&bytes))
                    .await?;
                Ok(())
            })
            .await?;
        drop(checkpoint);
        transaction.shutdown().await?;
        drop((transaction, provider));
        assert!(
            persistent(directory.path(), desired).await.is_err(),
            "{fault}: corrupt admission was accepted"
        );
    }
    Ok(())
}

#[tokio::test]
async fn producer_admission_limits_do_not_advance_session_or_journal_progress() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let desired = definition(true, RetentionPolicy::Backpressure, 2);
    let channel = persistent(directory.path(), desired.clone()).await?;
    channel.enable_admission(admission_options()?).await?;
    rejected(
        channel
            .register_producer(ComponentId::try_new("x".repeat(257))?)
            .await
            .unwrap_err(),
        AdmissionRejection::IdentifierTooLong,
    );
    let one = channel
        .register_producer(ComponentId::try_new("one")?)
        .await?;
    let two = channel
        .register_producer(ComponentId::try_new("two")?)
        .await?;
    let mut oversized = event(1)?;
    oversized.append_annotation(ContextEntry::try_new(
        ComponentId::try_new("client")?,
        "large",
        ContextValue::Bytes(Arc::from(vec![0; 1024 * 1024])),
    )?)?;
    assert!(channel.admit(&one, 1, &oversized).await.is_err());
    assert!(channel.admission_receipt(&one, 1).await?.is_none());
    assert_eq!(channel.producer_status(&one).await?.next_sequence, Some(1));
    assert_eq!(channel.producer_status(&one).await?.earliest_receipt, None);
    assert_eq!(channel.admit(&one, 1, &event(1)?).await?.position, 1);
    assert_eq!(channel.admit(&two, 1, &event(2)?).await?.position, 2);
    handle_admitted(&channel, &desired, 1).await?;
    handle_admitted(&channel, &desired, 2).await?;
    for sequence in 2..=3 {
        channel.admit(&one, sequence, &event(sequence + 1)?).await?;
        handle_admitted(&channel, &desired, sequence + 1).await?;
    }
    assert_eq!(
        channel.producer_status(&one).await?.earliest_receipt,
        Some(2)
    );
    assert_eq!(
        channel.producer_status(&two).await?.earliest_receipt,
        Some(1)
    );
    assert_eq!(channel.admit(&two, 1, &event(2)?).await?.position, 2);
    channel.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn admission_preserves_lineage_without_confusing_client_and_journal_sequences() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let desired = definition(true, RetentionPolicy::Backpressure, 2);
    let channel = persistent(directory.path(), desired.clone()).await?;
    channel.enable_admission(admission_options()?).await?;
    let session = channel
        .register_producer(ComponentId::try_new("client")?)
        .await?;
    let original = event(37)?;
    let mut input = original.derive(
        original.id().clone(),
        original.changes().clone(),
        SystemMetadata::new(StreamId::try_new("client/out")?, 37)
            .with_timestamp(chrono::DateTime::from_timestamp(123, 0).expect("time"))
            .with_source_position(Bytes::from_static(b"client-checkpoint")),
    );
    GraphProducerProgress::annotate(
        &mut input,
        &GraphProducerIdentity::volatile(
            "upstream".into(),
            "graph".into(),
            ComponentId::try_new("client")?,
            StreamId::try_new("client/out")?,
        )?,
        37,
    )?;
    let receipt = channel.admit(&session, 1, &input).await?;
    let mut pipe = endpoint(&channel, &desired, "fast", false)?;
    let mut receiver = pipe.pipe.take_receiver()?;
    let (output, acknowledgement) = receiver
        .receive()
        .await?
        .expect("admitted input")
        .into_parts();
    assert_eq!(output.system().sequence(), 1);
    assert_eq!(output.system().timestamp(), input.system().timestamp());
    assert_eq!(output.system().source_position(), None);
    let lineage = output.lineage().expect("client lineage");
    assert_eq!(lineage.envelope_id(), input.id());
    assert_eq!(
        lineage.system().source_position(),
        input.system().source_position()
    );
    assert_eq!(lineage.system().sequence(), 37);
    assert_eq!(output.annotations().len(), input.annotations().len() + 1);
    let progress = GraphProducerProgress::from_envelope(&output)?.expect("admission progress");
    assert_eq!(progress.sequence(), receipt.position);
    assert_eq!(progress.identity().incarnation(), session.incarnation);
    assert!(progress.identity().persistent());
    acknowledgement
        .expect("completion")
        .complete(HandlingOutcome::Handled)
        .await?;
    pipe.control.cancel();
    drop((pipe, receiver));
    channel.shutdown().await?;
    drop(channel);
    let reopened = persistent(directory.path(), desired).await?;
    assert_eq!(reopened.admit(&session, 1, &input).await?, receipt);
    reopened.shutdown().await?;
    Ok(())
}

// Executed only by the parent crash case, in an isolated process/database.
#[tokio::test]
async fn producer_admission_process_fixture() -> Result<()> {
    let Some(root) = std::env::var_os("DRASI_ADMISSION_CRASH_ROOT") else {
        return Ok(());
    };
    let phase = std::env::var("DRASI_ADMISSION_CRASH_PHASE")?;
    let root = std::path::PathBuf::from(root);
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let gate = Arc::new(CommitGate {
        before_commit: phase == "before",
        entered: tokio::sync::Notify::new(),
        resume: tokio::sync::Notify::new(),
    });
    let channel = uncertain_channel(
        &root,
        definition(true, RetentionPolicy::Backpressure, 2),
        fail.clone(),
        Some(gate.clone()),
    )
    .await?;
    channel.enable_admission(admission_options()?).await?;
    let session = channel
        .register_producer(ComponentId::try_new("crash-client")?)
        .await?;
    std::fs::write(
        root.join("client-session.json"),
        serde_json::to_vec(&session)?,
    )?;
    let input = event(1)?;
    if phase == "accepted" {
        channel.admit(&session, 1, &input).await?;
    } else {
        fail.store(true, std::sync::atomic::Ordering::Release);
        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::select! {
                _ = gate.entered.notified() => {},
                result = channel.admit(&session, 1, &input) => panic!("commit barrier bypassed: {result:?}"),
            }
        }).await?;
    }
    // No Rust destructors or provider shutdown; this is process loss, not power loss.
    std::process::exit(93);
}

#[tokio::test]
async fn producer_admission_survives_fresh_process_crashes_around_commit_and_response() -> Result<()>
{
    for phase in ["before", "after", "accepted"] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().to_path_buf();
        let executable = std::env::current_exe()?;
        let status = tokio::task::spawn_blocking(move || {
            std::process::Command::new(executable)
                .args([
                    "--exact",
                    "producer_admission_process_fixture",
                    "--nocapture",
                ])
                .env("DRASI_ADMISSION_CRASH_ROOT", path)
                .env("DRASI_ADMISSION_CRASH_PHASE", phase)
                .output()
        })
        .await??;
        assert_eq!(
            status.status.code(),
            Some(93),
            "{phase}: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        let session: ProducerSession = serde_json::from_slice(&std::fs::read(
            directory.path().join("client-session.json"),
        )?)?;
        let desired = definition(true, RetentionPolicy::Backpressure, 2);
        let recovered = persistent(directory.path(), desired.clone()).await?;
        assert_eq!(
            recovered.admission_receipt(&session, 1).await?.is_some(),
            phase != "before",
            "{phase}"
        );
        assert_eq!(recovered.admit(&session, 1, &event(1)?).await?.position, 1);
        assert_eq!(recovered.progress().await?.accepted, 1);
        handle_admitted(&recovered, &desired, 1).await?;
        recovered.shutdown().await?;
    }
    Ok(())
}

struct JournalIngress {
    descriptor: ComponentDescriptor,
    admission: Arc<SourceAdmission>,
}
#[async_trait]
impl ComputationComponent for JournalIngress {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for JournalIngress {
    fn admission(&self) -> Option<Arc<SourceAdmission>> {
        Some(self.admission.clone())
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        panic!("journal ingress must be graph-driven, not polled as a second producer");
    }
}

fn admission_builder(
    admission: &Arc<SourceAdmission>,
    fast: Arc<std::sync::Mutex<Vec<u64>>>,
    slow: Arc<std::sync::Mutex<Vec<u64>>>,
) -> Result<ComputationGraphBuilder> {
    let channel = admission.channel();
    let resource = ResourceId::try_new("journal")?;
    let source = Endpoint::new(ComponentId::try_new("source")?, PortId::try_new("out")?);
    Ok(ComputationGraph::builder("qos")
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "journal".into(),
        })?
        .provide_resource(resource.clone(), channel.resource())?
        .source(Box::new(JournalIngress {
            descriptor: descriptor("source", "out", PortDirection::Output),
            admission: admission.clone(),
        }))
        .sink(Box::new(RecordingSink {
            descriptor: descriptor("fast", "in", PortDirection::Input),
            seen: fast,
        }))
        .sink(Box::new(RecordingSink {
            descriptor: descriptor("slow", "in", PortDirection::Input),
            seen: slow,
        }))
        .bind_stream(source, channel.definition().stream.clone()))
}

fn admission_graph(
    admission: &Arc<SourceAdmission>,
    fast: Arc<std::sync::Mutex<Vec<u64>>>,
    slow: Arc<std::sync::Mutex<Vec<u64>>>,
    memory_output: bool,
) -> Result<ComputationGraph> {
    let mut builder = admission_builder(admission, fast, slow)?;
    let resource = ResourceId::try_new("journal")?;
    let source = Endpoint::new(ComponentId::try_new("source")?, PortId::try_new("out")?);
    for subscriber in ["fast", "slow"] {
        let pipe: Box<dyn PipeProvider> = if memory_output {
            Box::new(BoundedPipeConfig { capacity: 2 })
        } else {
            Box::new(
                admission
                    .channel()
                    .definition()
                    .pipe(resource.clone(), subscriber),
            )
        };
        builder = builder.connect(
            EdgeDefinition::new(
                source.clone(),
                Endpoint::new(ComponentId::try_new(subscriber)?, PortId::try_new("in")?),
            ),
            pipe,
        );
    }
    Ok(builder.build()?)
}

async fn graph_admit(
    admission: &SourceAdmission,
    session: &ProducerSession,
    sequence: u64,
    input: &ChangeEnvelope,
) -> Result<AdmissionReceipt, PipeError> {
    loop {
        match admission.admit(session, sequence, input).await {
            Err(PipeError::Backend(error))
                if error.downcast_ref::<AdmissionRejection>()
                    == Some(&AdmissionRejection::Busy) =>
            {
                tokio::task::yield_now().await;
            }
            result => return result,
        }
    }
}

#[tokio::test]
async fn graph_admission_validates_before_one_shared_commit_and_rebinds_after_stop() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let channel = uncertain_channel(
        directory.path(),
        definition(true, RetentionPolicy::Backpressure, 4),
        Arc::new(std::sync::atomic::AtomicBool::new(false)),
        None,
    )
    .await?;
    let mut options = admission_options()?;
    options.construction_scope = "qos".into();
    channel.enable_admission(options).await?;
    let session = channel
        .register_producer(ComponentId::try_new("client")?)
        .await?;
    let admission = SourceAdmission::new(channel.clone(), PortId::try_new("out")?).await?;
    assert!(!admission.is_active()?);
    assert!(matches!(
        admission.admit(&session, 1, &event(1)?).await,
        Err(PipeError::Closed)
    ));
    let fast = Arc::new(std::sync::Mutex::new(Vec::new()));
    let slow = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut graph = admission_graph(&admission, fast.clone(), slow.clone(), false)?;
    let recovery = graph.recovery_report(&RecoveryRequirement {
        consumer: ComponentId::try_new("source")?,
        scope: RecoveryScope::Failure(drasi_core::interface::FailureMode::ProcessRestart),
        guarantees: std::collections::BTreeSet::from([RecoveryGuarantee::Acceptance]),
    })?;
    assert!(recovery.satisfied(), "{recovery:?}");
    let run = graph.run()?;
    let control = run.control();
    let client = async {
        let deployment = control.deployment_report().await?;
        assert_eq!(
            deployment.summary,
            OperationSummary::Completed,
            "{deployment:?}"
        );
        let started = control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await?;
        assert_eq!(started.summary, OperationSummary::Completed, "{started:?}");
        tokio::time::timeout(Duration::from_secs(1), async {
            while !admission.is_active()? {
                tokio::task::yield_now().await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .unwrap_or_else(|_| panic!("admission inactive: {:?}", control.observed()))?;
        assert!(control
            .set_subscriptions(vec![(
                ComponentId::try_new("source")?,
                ComponentId::try_new("fast")?,
            )])
            .await
            .is_err());
        let edge = control.desired_snapshot().edges[0].clone();
        let weakened = control
            .preview(
                GraphRevision(1),
                vec![DesiredMutation::Bind(DesiredRelationship {
                    definition: edge.definition,
                    policy: edge.policy,
                    pipe: DesiredPipe::Bounded { capacity: 2 },
                })],
            )
            .await?;
        assert!(control
            .reconcile(weakened, TopologyBindings::default())
            .await
            .is_err());
        assert_eq!(control.desired_snapshot().revision, GraphRevision(1));
        assert!(admission.is_active()?);
        let original = event(1)?;
        let invalid = original.derive(
            original.id().clone(),
            ChangeSet::try_new(
                original.changes().id().clone(),
                QueryChangeCodec::schema().descriptor().clone(),
                vec![],
            )?,
            original.system().as_ref().clone(),
        );
        let error = admission
            .admit(&session, 1, &invalid)
            .await
            .expect_err("wrong output schema");
        assert!(
            matches!(error, PipeError::Backend(ref error) if error.downcast_ref::<GraphError>().is_some())
        );
        assert_eq!(channel.progress().await?.accepted, 0);
        for sequence in 1..=2 {
            let input = event(sequence)?;
            let receipt = graph_admit(&admission, &session, sequence, &input).await?;
            assert_eq!(receipt.position, sequence);
            assert_eq!(
                graph_admit(&admission, &session, sequence, &input).await?,
                receipt
            );
        }
        while channel
            .progress()
            .await?
            .processed
            .values()
            .any(|position| *position != 2)
        {
            tokio::task::yield_now().await;
        }
        let selection = GraphSelection::Exact(vec![ComponentId::try_new("source")?]);
        control
            .stop_components(GraphRevision(1), selection.clone())
            .await?;
        assert!(!admission.is_active()?);
        assert!(matches!(
            admission.admit(&session, 3, &event(3)?).await,
            Err(PipeError::Closed)
        ));
        control
            .start_components(GraphRevision(1), selection)
            .await?;
        while !admission.is_active()? {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            graph_admit(&admission, &session, 3, &event(3)?)
                .await?
                .position,
            3
        );
        while channel
            .progress()
            .await?
            .processed
            .values()
            .any(|position| *position != 3)
        {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            admission
                .register_producer(session.producer.clone())
                .await?,
            session
        );
        let status = admission.producer_status(&session).await?;
        assert_eq!(status.next_sequence, Some(4));
        assert_eq!(status.earliest_receipt, Some(2));
        assert_eq!(
            admission
                .admission_receipt(&session, 3)
                .await?
                .unwrap()
                .position,
            3
        );
        assert_eq!(admission.admission_receipt(&session, 4).await?, None);
        rejected(
            admission.admission_receipt(&session, 1).await.unwrap_err(),
            AdmissionRejection::ReceiptExpired(1),
        );
        admission.retire_producer(&session).await?;
        rejected(
            admission.producer_status(&session).await.unwrap_err(),
            AdmissionRejection::SessionExpired,
        );
        let next = admission
            .register_producer(session.producer.clone())
            .await?;
        assert!(next.epoch > session.epoch);
        control.cancel();
        Ok::<_, anyhow::Error>(())
    };
    let (run, client) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(run, async {
            let result = client.await;
            control.cancel();
            result
        })
    })
    .await?;
    client?;
    assert!(matches!(run, Err(GraphError::Cancelled)));
    assert_eq!(*fast.lock().expect("fast"), [1, 2, 3]);
    assert_eq!(*slow.lock().expect("slow"), [1, 2, 3]);
    assert_eq!(channel.progress().await?.accepted, 3);
    graph.shutdown().await?;
    channel.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn graph_admission_refuses_memory_outputs_without_accepting_client_input() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let channel = persistent(
        directory.path(),
        definition(true, RetentionPolicy::Backpressure, 2),
    )
    .await?;
    channel.enable_admission(admission_options()?).await?;
    let admission = SourceAdmission::new(channel.clone(), PortId::try_new("out")?).await?;
    let error = admission_graph(
        &admission,
        Arc::new(std::sync::Mutex::new(Vec::new())),
        Arc::new(std::sync::Mutex::new(Vec::new())),
        true,
    )
    .err()
    .expect("memory output rejected before construction");
    assert!(matches!(
        error.downcast_ref::<GraphError>(),
        Some(GraphError::Emission { .. })
    ));
    assert!(!admission.is_active()?);
    assert_eq!(channel.progress().await?.accepted, 0);
    channel.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn graph_admission_mailbox_bounds_cancelled_requests_without_consuming_sequences(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let channel = persistent(
        directory.path(),
        definition(true, RetentionPolicy::Backpressure, 2),
    )
    .await?;
    let mut options = admission_options()?;
    options.construction_scope = "qos".into();
    channel.enable_admission(options).await?;
    let session = channel
        .register_producer(ComponentId::try_new("client")?)
        .await?;
    let admission = SourceAdmission::new(channel.clone(), PortId::try_new("out")?).await?;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut graph = admission_graph(&admission, seen.clone(), seen.clone(), false)?;
    let run = graph.run()?;
    let control = run.control();
    let client = async {
        control.deployment_report().await?;
        let started = control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await?;
        assert_eq!(started.summary, OperationSummary::Completed, "{started:?}");
        while !admission.is_active()? {
            tokio::task::yield_now().await;
        }
        let input = event(1)?;
        let mut pending = Vec::new();
        // The graph driver is not polled while this task fills its mailbox.
        for _ in 0..16 {
            let mut request = Box::pin(admission.admit(&session, 1, &input));
            assert!(futures::poll!(&mut request).is_pending());
            pending.push(request);
        }
        rejected(
            admission.admit(&session, 1, &input).await.unwrap_err(),
            AdmissionRejection::Busy,
        );
        assert_eq!(channel.progress().await?.accepted, 0);
        drop(pending);
        assert_eq!(
            graph_admit(&admission, &session, 1, &input).await?.position,
            1
        );
        while channel
            .progress()
            .await?
            .processed
            .values()
            .any(|position| *position != 1)
        {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    };
    let (result, client) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run, async {
            let result = client.await;
            control.cancel();
            result
        })
    })
    .await?;
    client?;
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(*seen.lock().unwrap(), [1, 1]);
    assert!(!admission.is_active()?);
    graph.shutdown().await?;
    channel.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn graph_admission_cancellation_fences_active_commit_and_closes_queued_requests() -> Result<()>
{
    use std::sync::atomic::{AtomicBool, Ordering};
    for (before_commit, registration) in
        [(true, false), (false, false), (true, true), (false, true)]
    {
        for stop_source in [true, false] {
            let directory = tempfile::tempdir()?;
            let desired = definition(true, RetentionPolicy::Backpressure, 2);
            let fail = Arc::new(AtomicBool::new(false));
            let gate = Arc::new(CommitGate {
                before_commit,
                entered: tokio::sync::Notify::new(),
                resume: tokio::sync::Notify::new(),
            });
            let channel = uncertain_channel(
                directory.path(),
                desired.clone(),
                fail.clone(),
                Some(gate.clone()),
            )
            .await?;
            let mut options = admission_options()?;
            options.construction_scope = "qos".into();
            channel.enable_admission(options).await?;
            let session = channel
                .register_producer(ComponentId::try_new("client")?)
                .await?;
            let admission = SourceAdmission::new(channel.clone(), PortId::try_new("out")?).await?;
            let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut graph = admission_graph(&admission, seen.clone(), seen, false)?;
            let run = graph.run()?;
            let control = run.control();
            let client = async {
                control.deployment_report().await?;
                let started = control
                    .start_components(GraphRevision(1), GraphSelection::All)
                    .await?;
                assert_eq!(started.summary, OperationSummary::Completed, "{started:?}");
                while !admission.is_active()? {
                    tokio::task::yield_now().await;
                }
                fail.store(true, Ordering::Release);
                let input = event(1)?;
                let mut active = Box::pin(async {
                    if registration {
                        admission
                            .register_producer(ComponentId::try_new("next-client").unwrap())
                            .await
                            .map(|_| ())
                    } else {
                        admission.admit(&session, 1, &input).await.map(|_| ())
                    }
                });
                tokio::select! {
                    _ = gate.entered.notified() => {}
                    result = &mut active => panic!("commit should be blocked: {result:?}"),
                }
                let next = event(2)?;
                let mut queued = Box::pin(admission.admit(&session, 2, &next));
                assert!(futures::poll!(&mut queued).is_pending());
                if stop_source {
                    let stopped = control
                        .stop_components(
                            GraphRevision(1),
                            GraphSelection::Exact(vec![ComponentId::try_new("source")?]),
                        )
                        .await?;
                    assert_eq!(stopped.summary, OperationSummary::Completed, "{stopped:?}");
                } else {
                    control.cancel();
                }
                let error = active.await.unwrap_err();
                if registration {
                    assert!(matches!(error, PipeError::AcknowledgementUnknown { .. }));
                } else {
                    assert!(matches!(error, PipeError::AcceptanceUnknown { .. }));
                }
                assert!(matches!(queued.await, Err(PipeError::Closed)));
                assert!(
                    channel.progress().await.is_err(),
                    "cancelled transaction must fence cached state"
                );
                Ok::<_, anyhow::Error>(())
            };
            let (result, client) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(run, async {
                    let result = client.await;
                    control.cancel();
                    result
                })
            })
            .await?;
            client?;
            assert!(matches!(result, Err(GraphError::Cancelled)));
            graph.shutdown().await?;
            channel.shutdown().await?;
            let reopened = persistent(directory.path(), desired).await?;
            assert_eq!(
                reopened.admission_receipt(&session, 1).await?.is_some(),
                !before_commit && !registration
            );
            assert_eq!(
                reopened
                    .admission_receipt(&session, if before_commit || registration { 1 } else { 2 })
                    .await?,
                None
            );
            if registration {
                let next = ProducerSession {
                    incarnation: session.incarnation,
                    producer: ComponentId::try_new("next-client")?,
                    epoch: session.epoch + 1,
                };
                if before_commit {
                    rejected(
                        reopened.producer_status(&next).await.unwrap_err(),
                        AdmissionRejection::SessionExpired,
                    );
                } else {
                    assert_eq!(reopened.producer_status(&next).await?.session, next);
                }
                assert_eq!(
                    reopened.register_producer(next.producer.clone()).await?,
                    next
                );
            }
            assert_eq!(reopened.admit(&session, 1, &event(1)?).await?.position, 1);
            assert_eq!(reopened.progress().await?.accepted, 1);
            reopened.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn graph_admission_rejects_missing_mixed_foreign_and_unbound_outputs() -> Result<()> {
    for fault in ["missing", "mixed", "foreign", "graph", "port", "stream"] {
        let directory = tempfile::tempdir()?;
        let desired = definition(true, RetentionPolicy::Backpressure, 2);
        let channel = persistent(directory.path(), desired.clone()).await?;
        let mut options = admission_options()?;
        if fault == "graph" {
            options.graph_id = "foreign".into();
        }
        channel.enable_admission(options).await?;
        let admission = SourceAdmission::new(
            channel.clone(),
            PortId::try_new(if fault == "port" { "unknown" } else { "out" })?,
        )
        .await?;
        let empty = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut builder = admission_builder(&admission, empty.clone(), empty)?;
        let resource = ResourceId::try_new("journal")?;
        let other_directory = tempfile::tempdir()?;
        let other = persistent(other_directory.path(), desired.clone()).await?;
        let foreign = ResourceId::try_new("foreign")?;
        if fault == "foreign" {
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: foreign.clone(),
                    role: ResourceRole::StateStore,
                    ownership: ResourceOwnership::Borrowed,
                    binding: "foreign".into(),
                })?
                .provide_resource(foreign.clone(), other.resource())?;
        }
        if fault == "stream" {
            builder = builder.bind_stream(
                Endpoint::new(ComponentId::try_new("source")?, PortId::try_new("out")?),
                StreamId::try_new("wrong")?,
            );
        }
        if fault != "missing" {
            for subscriber in ["fast", "slow"] {
                let pipe: Box<dyn PipeProvider> = if fault == "mixed" && subscriber == "slow" {
                    Box::new(BoundedPipeConfig { capacity: 2 })
                } else {
                    Box::new(desired.pipe(
                        if fault == "foreign" {
                            foreign.clone()
                        } else {
                            resource.clone()
                        },
                        subscriber,
                    ))
                };
                builder = builder.connect(
                    EdgeDefinition::new(
                        Endpoint::new(ComponentId::try_new("source")?, PortId::try_new("out")?),
                        Endpoint::new(ComponentId::try_new(subscriber)?, PortId::try_new("in")?),
                    ),
                    pipe,
                );
            }
        }
        assert!(builder.build().is_err(), "{fault} output accepted");
        assert!(!admission.is_active()?);
        assert_eq!(channel.progress().await?.accepted, 0);
        channel.shutdown().await?;
        other.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn graph_admission_rejects_a_foreign_construction_scope_before_activation() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let channel = persistent(
        directory.path(),
        definition(true, RetentionPolicy::Backpressure, 2),
    )
    .await?;
    channel.enable_admission(admission_options()?).await?;
    let admission = SourceAdmission::new(channel.clone(), PortId::try_new("out")?).await?;
    let empty = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut graph = admission_graph(&admission, empty.clone(), empty, false)?;
    let run = graph.run()?;
    let control = run.control();
    let (result, started) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run, async {
            let report = control
                .start_components(GraphRevision(1), GraphSelection::All)
                .await;
            control.cancel();
            report
        })
    })
    .await?;
    let report =
        started.unwrap_or_else(|error| panic!("scope start failed: {error}; driver={result:?}"));
    assert_eq!(
        report.summary,
        OperationSummary::CompletedWithFailures,
        "{report:?}"
    );
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert!(!admission.is_active()?);
    assert_eq!(channel.progress().await?.accepted, 0);
    graph.shutdown().await?;
    channel.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn graph_admission_reconstruction_replays_obligations_without_accepting_retries_twice(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let desired = definition(true, RetentionPolicy::Backpressure, 1);
    let channel = persistent(directory.path(), desired.clone()).await?;
    let mut options = admission_options()?;
    options.construction_scope = "qos".into();
    channel.enable_admission(options).await?;
    let session = channel
        .register_producer(ComponentId::try_new("client")?)
        .await?;
    let input = event(1)?;
    {
        let admission = SourceAdmission::new(channel.clone(), PortId::try_new("out")?).await?;
        let empty = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut graph = admission_graph(&admission, empty.clone(), empty.clone(), false)?;
        let run = graph.run()?;
        let control = run.control();
        let client = async {
            control.deployment_report().await?;
            let started = control
                .start_components(
                    GraphRevision(1),
                    GraphSelection::Exact(vec![ComponentId::try_new("source")?]),
                )
                .await?;
            assert_eq!(started.summary, OperationSummary::Completed, "{started:?}");
            while !admission.is_active()? {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                graph_admit(&admission, &session, 1, &input).await?.position,
                1
            );
            assert!(matches!(
                graph_admit(&admission, &session, 2, &event(2)?).await,
                Err(PipeError::CapacityExhausted)
            ));
            Ok::<_, anyhow::Error>(())
        };
        let (result, client) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(run, async {
                let result = client.await;
                control.cancel();
                result
            })
        })
        .await?;
        client?;
        assert!(matches!(result, Err(GraphError::Cancelled)));
        assert!(empty.lock().unwrap().is_empty());
        assert!(channel
            .progress()
            .await?
            .processed
            .values()
            .all(|position| *position == 0));
        graph.shutdown().await?;
    }
    channel.shutdown().await?;
    let channel = persistent(directory.path(), desired).await?;
    let admission = SourceAdmission::new(channel.clone(), PortId::try_new("out")?).await?;
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut graph = admission_graph(&admission, seen.clone(), seen.clone(), false)?;
    let run = graph.run()?;
    let control = run.control();
    let client = async {
        control.deployment_report().await?;
        let started = control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await?;
        assert_eq!(started.summary, OperationSummary::Completed, "{started:?}");
        while !admission.is_active()? {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            graph_admit(&admission, &session, 1, &input).await?.position,
            1
        );
        while channel
            .progress()
            .await?
            .processed
            .values()
            .any(|position| *position != 1)
        {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            graph_admit(&admission, &session, 2, &event(2)?)
                .await?
                .position,
            2
        );
        while channel
            .progress()
            .await?
            .processed
            .values()
            .any(|position| *position != 2)
        {
            tokio::task::yield_now().await;
        }
        Ok::<_, anyhow::Error>(())
    };
    let (result, client) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(run, async {
            let result = client.await;
            control.cancel();
            result
        })
    })
    .await?;
    client?;
    assert!(matches!(result, Err(GraphError::Cancelled)));
    let mut seen = seen.lock().unwrap().clone();
    seen.sort_unstable();
    assert_eq!(seen, [1, 1, 2, 2]);
    assert_eq!(channel.progress().await?.accepted, 2);
    graph.shutdown().await?;
    channel.shutdown().await?;
    Ok(())
}
