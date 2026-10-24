// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]

#[allow(dead_code)]
mod computation_support;

use anyhow::{Context, Result};
use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;
use futures::poll;
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

fn limits(count: usize) -> SinkBatching {
    SinkBatching {
        max_inputs: count,
        max_operations: count,
        max_bytes: 64 * 1024,
        max_delay_ms: 100,
    }
}
async fn acknowledge(delivery: Delivery) -> Result<()> {
    delivery
        .into_parts()
        .1
        .expect("receipt")
        .complete(HandlingOutcome::Handled)
        .await?;
    Ok(())
}
fn sequences(deliveries: &[Delivery]) -> Vec<u64> {
    deliveries
        .iter()
        .map(|delivery| delivery.envelope().system().sequence())
        .collect()
}
fn qos_definition(durable: bool) -> QosChannelDefinition {
    QosChannelDefinition {
        stream: stream("source"),
        capacity: NonZeroUsize::new(8).unwrap(),
        durable,
        retention: RetentionPolicy::Backpressure,
        subscribers: BTreeMap::from([("sink".into(), SubscriptionStart::Earliest)]),
    }
}
fn subscribe(channel: &Arc<QosChannel>) -> Result<ProvidedPipe> {
    let resource = ResourceId::try_new("channel")?;
    Ok(channel
        .definition()
        .pipe(resource.clone(), "sink")
        .create_with_resources(&BTreeMap::from([(resource, channel.resource())]))?)
}

#[tokio::test(start_paused = true)]
async fn retained_cancelled_collection_retains_identity_and_contiguous_prefix() -> Result<()> {
    let store = Arc::new(MemoryEnvelopeStore::new(
        NonZeroUsize::new(8).unwrap(),
        RetentionPolicy::Backpressure,
    ));
    let mut pipe = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict)?;
    let sender = pipe.sender();
    let control = pipe.control();
    let mut receiver = pipe.take_receiver()?;
    let first = root("source", 1, &[1]);
    sender.send(first.clone()).await?;
    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        receiver.receive_batch(limits(3), &port("in"))
    )
    .await
    .is_err());
    assert!(
        !control.is_idle().await?,
        "cancelled collection still owns its lease"
    );
    for sequence in 2..=3 {
        sender
            .send(root("source", sequence, &[sequence as u16]))
            .await?;
    }
    let mut batch = receiver.receive_batch(limits(3), &port("in")).await?;
    assert_eq!(sequences(&batch), [1, 2, 3]);
    assert_eq!(batch[0].envelope().id(), first.id());
    assert_eq!(store.progress(store.generation()).await?, 0);
    assert!(
        acknowledge(batch.remove(1)).await.is_err(),
        "out-of-order ACK must not skip"
    );
    assert_eq!(store.progress(store.generation()).await?, 0);
    acknowledge(batch.remove(0)).await?;
    assert_eq!(store.progress(store.generation()).await?, 1);
    assert!(!control.is_idle().await?);
    drop(batch);
    let replay = receiver.receive_batch(limits(2), &port("in")).await?;
    assert_eq!(
        sequences(&replay),
        [2, 3],
        "dropped suffix is replayed, not acknowledged"
    );
    for delivery in replay {
        acknowledge(delivery).await?;
    }
    assert_eq!(store.progress(store.generation()).await?, 3);
    assert!(control.is_idle().await?);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn qos_cancelled_collection_partial_ack_and_dropped_suffix_preserve_obligations() -> Result<()>
{
    let channel = QosChannel::volatile(qos_definition(false))?;
    let mut pipe = subscribe(&channel)?;
    let mut receiver = pipe.pipe.take_receiver()?;
    channel.publish(&root("source", 1, &[1])).await?;
    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        receiver.receive_batch(limits(3), &port("in"))
    )
    .await
    .is_err());
    assert!(!pipe.control.is_idle().await?);
    for sequence in 2..=3 {
        channel
            .publish(&root("source", sequence, &[sequence as u16]))
            .await?;
    }
    let mut batch = receiver.receive_batch(limits(3), &port("in")).await?;
    assert_eq!(sequences(&batch), [1, 2, 3]);
    acknowledge(batch.remove(0)).await?;
    assert_eq!(channel.progress().await?.processed["sink"], 1);
    let mut waiting = Box::pin(receiver.receive());
    assert!(
        poll!(&mut waiting).is_pending(),
        "the remaining batch lease excludes another receive"
    );
    drop(waiting);
    drop(batch);
    let mut replay = receiver.receive_batch(limits(2), &port("in")).await?;
    assert_eq!(sequences(&replay), [2, 3]);
    assert!(acknowledge(replay.remove(1)).await.is_err());
    assert_eq!(channel.progress().await?.processed["sink"], 1);
    drop(replay);
    for delivery in receiver.receive_batch(limits(2), &port("in")).await? {
        acknowledge(delivery).await?;
    }
    assert_eq!(channel.progress().await?.processed["sink"], 3);
    assert!(pipe.control.is_idle().await?);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn lease_collection_flushes_on_bytes_deadline_and_control_barriers() -> Result<()> {
    for qos in [false, true] {
        let mut retained = if qos {
            None
        } else {
            Some(RetainedPipe::new(
                Arc::new(MemoryEnvelopeStore::new(
                    NonZeroUsize::new(8).unwrap(),
                    RetentionPolicy::Backpressure,
                )),
                ReplayGapPolicy::Strict,
            )?)
        };
        let channel = if qos {
            Some(QosChannel::volatile(qos_definition(false))?)
        } else {
            None
        };
        let mut provided = channel.as_ref().map(subscribe).transpose()?;
        let (sender, mut receiver) = if let Some(pipe) = &mut retained {
            (pipe.sender(), pipe.take_receiver()?)
        } else {
            let pipe = &mut provided.as_mut().unwrap().pipe;
            (pipe.sender(), pipe.take_receiver()?)
        };
        let a = root("source", 1, &[1]);
        let original = root("source", 2, &[2]);
        let barrier = ChangeEnvelope::new(
            original.id().clone(),
            ChangeSet::try_new(original.changes().id().clone(), schema_descriptor(), vec![])?,
            original.system().as_ref().clone(),
        );
        let c = root("source", 3, &[3]);
        for event in [a, barrier, c] {
            sender.send(event).await?;
        }
        for sequence in 1..=3 {
            let batch = receiver.receive_batch(limits(3), &port("in")).await?;
            assert_eq!(sequences(&batch), [sequence]);
            for delivery in batch {
                acknowledge(delivery).await?;
            }
        }
        let a = root("source", 4, &[4]);
        let b = root("source", 5, &[5]);
        let mut bound = limits(3);
        bound.max_bytes = SinkBatching::input_size(&InputEnvelope {
            port: port("in"),
            envelope: a.clone(),
        })? + 5;
        sender.send(a).await?;
        sender.send(b).await?;
        let batch = receiver.receive_batch(bound, &port("in")).await?;
        assert_eq!(sequences(&batch), [4]);
        for delivery in batch {
            acknowledge(delivery).await?;
        }
        let started = tokio::time::Instant::now();
        let batch = receiver.receive_batch(limits(3), &port("in")).await?;
        assert_eq!(sequences(&batch), [5]);
        assert!(
            started.elapsed() >= Duration::from_millis(100),
            "sparse batch must use its deadline"
        );
        for delivery in batch {
            acknowledge(delivery).await?;
        }
    }
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cancelled_receiver_cannot_release_prefetched_events_after_its_binding_is_fenced(
) -> Result<()> {
    let store = Arc::new(MemoryEnvelopeStore::new(
        NonZeroUsize::new(8).unwrap(),
        RetentionPolicy::Backpressure,
    ));
    let mut old = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict)?;
    old.sender().send(root("source", 1, &[1])).await?;
    let mut receiver = old.take_receiver()?;
    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        receiver.receive_batch(limits(3), &port("in"))
    )
    .await
    .is_err());
    old.control().cancel();
    assert!(matches!(
        receiver.receive_batch(limits(3), &port("in")).await,
        Err(PipeError::Closed)
    ));
    assert!(matches!(
        receiver.take_pending_batch(),
        Err(PipeError::Closed)
    ));
    assert!(receiver.receive().await?.is_none());
    drop(receiver);
    let mut replacement = RetainedPipe::new(store.clone(), ReplayGapPolicy::Strict)?;
    let mut receiver = replacement.take_receiver()?;
    let replay = receiver.receive_batch(limits(1), &port("in")).await?;
    assert_eq!(sequences(&replay), [1]);
    for delivery in replay {
        acknowledge(delivery).await?;
    }

    let channel = QosChannel::volatile(qos_definition(false))?;
    let mut old = subscribe(&channel)?;
    channel.publish(&root("source", 1, &[1])).await?;
    let mut receiver = old.pipe.take_receiver()?;
    assert!(tokio::time::timeout(
        Duration::from_millis(10),
        receiver.receive_batch(limits(3), &port("in"))
    )
    .await
    .is_err());
    old.control.cancel();
    assert!(matches!(
        receiver.receive_batch(limits(3), &port("in")).await,
        Err(PipeError::Closed)
    ));
    assert!(matches!(
        receiver.take_pending_batch(),
        Err(PipeError::Closed)
    ));
    assert!(matches!(receiver.receive().await, Err(PipeError::Closed)));
    drop(receiver);
    let mut replacement = subscribe(&channel)?;
    let mut receiver = replacement.pipe.take_receiver()?;
    let replay = receiver.receive_batch(limits(1), &port("in")).await?;
    assert_eq!(sequences(&replay), [1]);
    for delivery in replay {
        acknowledge(delivery).await?;
    }
    assert_eq!(channel.progress().await?.processed["sink"], 1);
    Ok(())
}

type Calls = Arc<Mutex<Vec<(bool, Vec<u64>)>>>;
struct BatchSink {
    descriptor: ComponentDescriptor,
    policy: SinkBatching,
    calls: Calls,
}
#[async_trait]
impl ComputationComponent for BatchSink {
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
impl EnvelopeSink for BatchSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn batching(&self) -> Result<Option<SinkBatching>> {
        Ok(Some(self.policy))
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push((false, vec![input.envelope.system().sequence()]));
        Ok(())
    }
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        self.policy.validate_inputs(&inputs)?;
        self.calls.lock().unwrap().push((
            true,
            inputs
                .iter()
                .map(|input| input.envelope.system().sequence())
                .collect(),
        ));
        Ok(())
    }
}

async fn graph_batches() -> Result<()> {
    for retained in [false, true] {
        let calls = Calls::default();
        let mut builder = ComputationGraph::builder("sink-batches")
            .source(Box::new(FiniteSource::new(
                "source",
                (1..=5)
                    .map(|sequence| output(root("source", sequence, &[sequence as u16])))
                    .collect(),
            )))
            .sink(Box::new(BatchSink {
                descriptor: descriptor("sink", &["in"], &[]),
                policy: limits(2),
                calls: calls.clone(),
            }))
            .bind_stream(endpoint("source", "out"), stream("source"));
        let store = Arc::new(MemoryEnvelopeStore::new(
            NonZeroUsize::new(8).unwrap(),
            RetentionPolicy::Backpressure,
        ));
        if retained {
            let resource = ResourceId::try_new("retained")?;
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: resource.clone(),
                    role: ResourceRole::StateStore,
                    ownership: ResourceOwnership::Borrowed,
                    binding: "retained".into(),
                })?
                .provide_resource(
                    resource.clone(),
                    ResourceHandle::new(
                        ResourceRole::StateStore,
                        Arc::new(RetainedStoreResource(store.clone())),
                    ),
                )?
                .connect(
                    edge("source", "sink"),
                    Box::new(RetainedPipeConfig {
                        resource,
                        capacity: NonZeroUsize::new(8).unwrap(),
                        durable: false,
                        retention: RetentionPolicy::Backpressure,
                        gap_policy: ReplayGapPolicy::Strict,
                    }),
                );
        } else {
            builder = builder.connect(
                edge("source", "sink"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
        }
        let mut graph = builder.build()?;
        tokio::time::timeout(Duration::from_secs(5), graph.start()?)
            .await?
            .with_context(|| format!("running batched graph, retained={retained}"))?;
        assert_eq!(
            *calls.lock().unwrap(),
            [(true, vec![1, 2]), (true, vec![3, 4]), (true, vec![5])]
        );
        assert_eq!(
            graph.state(),
            GraphState::Completed,
            "retained input must be acknowledged before close can drain"
        );
    }
    Ok(())
}
#[tokio::test(flavor = "current_thread")]
async fn graph_owned_batches_preserve_fifo_and_acknowledge_complete_groups() -> Result<()> {
    graph_batches().await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_owned_batches_work_on_multithreaded_runtime() -> Result<()> {
    graph_batches().await
}

struct ManualSource {
    descriptor: ComponentDescriptor,
    receiver: tokio::sync::mpsc::Receiver<OutputEnvelope>,
}
#[async_trait]
impl ComputationComponent for ManualSource {
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
impl EnvelopeSource for ManualSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self.receiver.recv().await)
    }
}
#[derive(Default)]
struct Gate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    calls: Mutex<Vec<Vec<u64>>>,
}
struct GatedSink {
    descriptor: ComponentDescriptor,
    gate: Arc<Gate>,
}
#[async_trait]
impl ComputationComponent for GatedSink {
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
impl EnvelopeSink for GatedSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn batching(&self) -> Result<Option<SinkBatching>> {
        Ok(Some(SinkBatching {
            max_delay_ms: 1000,
            ..limits(3)
        }))
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.handle_batch(vec![input]).await
    }
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> Result<()> {
        self.gate.entered.notify_one();
        self.gate.release.notified().await;
        self.gate.calls.lock().unwrap().push(
            inputs
                .iter()
                .map(|input| input.envelope.system().sequence())
                .collect(),
        );
        Ok(())
    }
}
#[tokio::test(flavor = "current_thread")]
async fn graceful_stop_flushes_a_partial_batch_waits_for_effects_and_restarts() -> Result<()> {
    let (sender, receiver) = tokio::sync::mpsc::channel(4);
    let gate = Arc::new(Gate::default());
    let batch = ComponentBatch::builder()
        .source(Box::new(ManualSource {
            descriptor: descriptor("source", &[], &["out"]),
            receiver,
        }))
        .sink(Box::new(GatedSink {
            descriptor: descriptor("sink", &["in"], &[]),
            gate: gate.clone(),
        }))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 4 }),
        )
        .build()?;
    let core = drasi_lib::DrasiLib::builder()
        .with_components(batch)
        .build()
        .await?;
    core.start().await?;
    sender.send(output(root("source", 1, &[1]))).await?;
    // The source must dispatch before requesting a sink-only stop.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let sink = core.computation_component("sink")?;
    {
        let stopping = sink.stop();
        tokio::pin!(stopping);
        tokio::select! {
            result = &mut stopping => panic!("stop skipped a retained batch: {result:?}"),
            entered = tokio::time::timeout(Duration::from_secs(2),gate.entered.notified()) => { entered?; },
        }
        assert!(gate.calls.lock().unwrap().is_empty());
        gate.release.notify_one();
        tokio::time::timeout(Duration::from_secs(2), stopping).await??;
    }
    assert_eq!(*gate.calls.lock().unwrap(), [vec![1]]);
    sink.start().await?;
    for sequence in 2..=4 {
        sender
            .send(output(root("source", sequence, &[sequence as u16])))
            .await?;
    }
    tokio::time::timeout(Duration::from_secs(2), gate.entered.notified()).await?;
    gate.release.notify_one();
    core.stop().await?;
    assert_eq!(*gate.calls.lock().unwrap(), [vec![1], vec![2, 3, 4]]);
    core.shutdown().await?;
    Ok(())
}

#[cfg(feature = "computation-rocksdb-tests")]
mod persistent {
    use super::*;
    use drasi_core::computation::{ComputationIndexProvider, ComputationIndexes};
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    use std::path::Path;

    async fn indexes(path: &Path) -> Result<ComputationIndexes> {
        Ok(RocksDbComputationProvider::new(
            path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        )
        .create_indexes("sink-batches", "journal")
        .await?)
    }
    fn codec() -> Result<EnvelopeCodec> {
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1 << 20).unwrap());
        codec.register_schema(Arc::new(Schema::new(
            schema_descriptor(),
            Arc::new(ReadingValidator(schema_descriptor())),
        )))?;
        Ok(codec)
    }
    async fn store(path: &Path) -> Result<Arc<IndexedEnvelopeStore>> {
        Ok(Arc::new(IndexedEnvelopeStore::try_new_with_byte_budget(
            indexes(path).await?,
            Arc::new(codec()?),
            "edge",
            NonZeroUsize::new(8).unwrap(),
            RetentionPolicy::Backpressure,
            NonZeroUsize::new(1 << 20).unwrap(),
        )?))
    }

    #[tokio::test]
    async fn retained_batch_reconstruction_replays_only_unacknowledged_suffix() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let initial = store(directory.path()).await?;
        let mut pipe = RetainedPipe::new(initial.clone(), ReplayGapPolicy::Strict)?;
        let sender = pipe.sender();
        let mut receiver = pipe.take_receiver()?;
        for sequence in 1..=3 {
            sender
                .send(root("source", sequence, &[sequence as u16]))
                .await?;
        }
        let mut batch = receiver.receive_batch(limits(3), &port("in")).await?;
        acknowledge(batch.remove(0)).await?;
        drop(batch);
        pipe.control().cancel();
        drop(receiver);
        drop(sender);
        drop(pipe);
        initial.shutdown().await?;
        drop(initial);
        let restored = store(directory.path()).await?;
        let mut pipe = RetainedPipe::new(restored.clone(), ReplayGapPolicy::Strict)?;
        assert_eq!(restored.progress(restored.generation()).await?, 1);
        let mut receiver = pipe.take_receiver()?;
        let batch = receiver.receive_batch(limits(2), &port("in")).await?;
        assert_eq!(sequences(&batch), [2, 3]);
        for delivery in batch {
            acknowledge(delivery).await?;
        }
        assert_eq!(restored.progress(restored.generation()).await?, 3);
        pipe.control().cancel();
        drop(receiver);
        drop(pipe);
        restored.shutdown().await?;
        Ok(())
    }

    async fn channel(path: &Path) -> Result<Arc<QosChannel>> {
        Ok(QosChannel::persistent_with_options(
            qos_definition(true),
            indexes(path).await?,
            codec()?,
            "edge",
            QosJournalOptions {
                recovery: None,
                max_bytes: Some(NonZeroUsize::new(1 << 20).unwrap()),
                page_limits: Some(drasi_core::interface::OutboxPageLimits {
                    max_records: NonZeroUsize::new(2).unwrap(),
                    max_bytes: NonZeroUsize::new(4096).unwrap(),
                }),
            },
        )
        .await?)
    }

    #[tokio::test]
    async fn qos_batch_reconstruction_preserves_contiguous_cursor_across_persistent_pages(
    ) -> Result<()> {
        let directory = tempfile::tempdir()?;
        let original = channel(directory.path()).await?;
        let mut pipe = subscribe(&original)?;
        let mut receiver = pipe.pipe.take_receiver()?;
        for sequence in 1..=5 {
            original
                .publish(&root("source", sequence, &[sequence as u16]))
                .await?;
        }
        let mut batch = receiver.receive_batch(limits(5), &port("in")).await?;
        assert_eq!(sequences(&batch), [1, 2, 3, 4, 5]);
        acknowledge(batch.remove(0)).await?;
        acknowledge(batch.remove(0)).await?;
        drop(batch);
        pipe.control.cancel();
        drop(receiver);
        drop(pipe);
        original.shutdown().await?;
        drop(original);
        let restored = channel(directory.path()).await?;
        assert_eq!(restored.progress().await?.processed["sink"], 2);
        let mut pipe = subscribe(&restored)?;
        let mut receiver = pipe.pipe.take_receiver()?;
        let batch = receiver.receive_batch(limits(3), &port("in")).await?;
        assert_eq!(sequences(&batch), [3, 4, 5]);
        for delivery in batch {
            acknowledge(delivery).await?;
        }
        assert_eq!(restored.progress().await?.processed["sink"], 5);
        pipe.control.cancel();
        drop(receiver);
        drop(pipe);
        restored.shutdown().await?;
        Ok(())
    }
}
