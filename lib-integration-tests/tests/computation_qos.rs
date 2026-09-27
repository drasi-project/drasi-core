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
    Ok(QosChannel::persistent(definition, indexes, codec, "journal").await?)
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
    for seed in 1..=32_u64 {
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
            for step in 0..128 {
                schedule = schedule
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                let index = ((schedule >> 32) % consumers.len() as u64) as usize;
                let action = ((schedule >> 40) % 8) as usize;
                let head = accepted.len() as u64;
                let oldest = head
                    .saturating_sub(definition.capacity.get() as u64)
                    .saturating_add(1);
                if action == 0 || action == 1 || action == 6 && step < 96 {
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
}
#[async_trait]
impl drasi_core::interface::SessionControl for LoseCommitResponse {
    async fn begin(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        let fail = self.fail.swap(false, std::sync::atomic::Ordering::AcqRel);
        if let Some(gate) = self.gate.as_ref().filter(|gate| fail && gate.before_commit) {
            gate.entered.notify_one();
            std::future::pending::<()>().await;
        }
        self.inner.commit().await?;
        if fail {
            if let Some(gate) = &self.gate {
                gate.entered.notify_one();
                std::future::pending::<()>().await;
            }
            return Err(drasi_core::interface::IndexError::IOError);
        }
        Ok(())
    }
    fn rollback(&self) -> std::result::Result<(), drasi_core::interface::IndexError> {
        self.inner.rollback()
    }
}

async fn uncertain_channel(
    root: &Path,
    definition: QosChannelDefinition,
    fail: Arc<std::sync::atomic::AtomicBool>,
    gate: Option<Arc<CommitGate>>,
) -> Result<Arc<QosChannel>> {
    use drasi_core::{
        computation::{ComputationIndexes, ComputationResource, TransactionDomain},
        interface::IndexSet,
    };
    let provider =
        LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(root, false, false)));
    let original = provider.create_indexes("qos", "channel").await?;
    let control: Arc<dyn drasi_core::interface::SessionControl> = Arc::new(LoseCommitResponse {
        inner: original.indexes().session_control.clone(),
        fail,
        gate,
    });
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
    )?
    .with_cleanup(original.cleanup().expect("storage owner").clone());
    Ok(QosChannel::persistent(
        definition,
        indexes,
        FactoryRegistry::standard()
            .envelope_codec(NonZeroUsize::new(1024 * 1024).expect("codec limit"))?,
        "journal",
    )
    .await?)
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
