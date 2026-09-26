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

use super::*;
use async_trait::async_trait;
use std::result::Result;
use std::{
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::Notify;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RankedInputPipeConfig {
    pub queue: ResourceId,
    pub capacity: usize,
    pub source_rank: usize,
    /// Some identifies a SourceEvent stream; None explicitly uses native producer sequence.
    pub source_id: Option<String>,
    pub drop_when_full: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Closure {
    Open,
    Draining,
    Cancelled,
}

struct Binding {
    rank: usize,
    generation: u64,
    metrics: pipe_metrics::PipeMetrics,
}

#[derive(Clone)]
struct Entry {
    key: (chrono::DateTime<chrono::Utc>, usize, u64),
    generation: u64,
    envelope: ChangeEnvelope,
}

impl crate::channels::Timestamped for Entry {
    fn timestamp(&self) -> chrono::DateTime<chrono::Utc> {
        self.key.0
    }
    fn ordering_tie_breaker(&self) -> Option<(usize, u64)> {
        Some((self.key.1, self.key.2))
    }
}

#[derive(Default)]
struct State {
    heap: crate::channels::priority_queue::OrderedHeap<Entry>,
    bindings: BTreeMap<usize, (Arc<Binding>, Closure)>,
    next_generation: u64,
    next_admission: u64,
    closed: bool,
}

/// One finite priority inbox shared by all inputs to one query.
///
/// Each producer's earliest admitted sequence is its head. Compare those heads
/// by timestamp/rank; a backwards clock cannot jump a recovery watermark past
/// unprocessed work. No idle source is awaited.
pub struct RankedInputQueue {
    capacity: usize,
    state: Mutex<State>,
    changed: Notify,
}

impl RankedInputQueue {
    pub fn new(capacity: usize) -> anyhow::Result<Arc<Self>> {
        anyhow::ensure!(capacity > 0, "ranked input capacity must be nonzero");
        Ok(Arc::new(Self {
            capacity,
            state: Mutex::new(State::default()),
            changed: Notify::new(),
        }))
    }

    pub fn resource(self: &Arc<Self>) -> ResourceHandle {
        ResourceHandle::new(ResourceRole::Pipe, self.clone()).with_cleanup(self.clone())
    }

    fn bind(self: &Arc<Self>, config: &RankedInputPipeConfig) -> Result<RankedPipe, PipeError> {
        let mut state = self.state.lock().map_err(|_| poisoned())?;
        if state.closed {
            return Err(PipeError::Closed);
        }
        if self.capacity != config.capacity || state.bindings.contains_key(&config.source_rank) {
            return Err(PipeError::Backend(anyhow::anyhow!(
                "ranked input capacity differs or source rank is already bound"
            )));
        }
        let generation = state.next_generation;
        state.next_generation = generation.checked_add(1).ok_or_else(exhausted)?;
        let binding = Arc::new(Binding {
            rank: config.source_rank,
            generation,
            metrics: pipe_metrics::PipeMetrics::default(),
        });
        state
            .bindings
            .insert(binding.rank, (binding.clone(), Closure::Open));
        Ok(RankedPipe {
            capabilities: config.capabilities()?,
            sender: Arc::new(Sender {
                queue: self.clone(),
                binding: binding.clone(),
                config: config.clone(),
            }),
            receiver: Some(Receiver {
                queue: self.clone(),
                binding: binding.clone(),
            }),
            control: Arc::new(Control {
                queue: Arc::downgrade(self),
                binding,
            }),
        })
    }

    fn closure(state: &State, binding: &Binding) -> Closure {
        state
            .bindings
            .get(&binding.rank)
            .filter(|(current, _)| current.generation == binding.generation)
            .map_or(Closure::Cancelled, |(_, closure)| *closure)
    }

    fn close(&self, binding: &Binding, cancel: bool) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if Self::closure(&state, binding) == Closure::Cancelled {
            return;
        }
        if cancel {
            let before = state.heap.len();
            state.heap.retain(|entry| {
                entry.key.1 != binding.rank || entry.generation != binding.generation
            });
            binding.metrics.discarded(before - state.heap.len());
            state.bindings.remove(&binding.rank);
        } else if let Some((_, closure)) = state.bindings.get_mut(&binding.rank) {
            *closure = Closure::Draining;
        }
        drop(state);
        self.changed.notify_waiters();
    }
}

fn poisoned() -> PipeError {
    PipeError::Backend(anyhow::anyhow!("ranked input ownership poisoned"))
}
fn exhausted() -> PipeError {
    PipeError::Backend(anyhow::anyhow!(
        "ranked input generation or admission counter exhausted"
    ))
}

#[async_trait]
impl ResourceCleanup for RankedInputQueue {
    async fn shutdown(&self) -> anyhow::Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("ranked input ownership poisoned"))?;
        state.closed = true;
        for entry in std::mem::take(&mut state.heap).drain() {
            if let Some((binding, _)) = state.bindings.get(&entry.key.1) {
                binding.metrics.discarded(1);
            }
        }
        state.bindings.clear();
        drop(state);
        self.changed.notify_waiters();
        Ok(())
    }
}

impl PipeProvider for RankedInputPipeConfig {
    fn specification(&self) -> Option<DesiredPipe> {
        Some(DesiredPipe::Ranked(self.clone()))
    }
    fn resource_dependencies(&self) -> BTreeMap<ResourceId, ResourceRole> {
        BTreeMap::from([(self.queue.clone(), ResourceRole::Pipe)])
    }
    fn validate_resources(
        &self,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<(), PipeError> {
        if let Some(resource) = resources.get(&self.queue) {
            let queue = resource
                .get::<RankedInputQueue>()
                .map_err(|error| PipeError::Backend(error.into()))?;
            if queue.capacity != self.capacity {
                return Err(PipeError::InvalidCapacity);
            }
        }
        Ok(())
    }
    fn capabilities(&self) -> Result<PipeCapabilities, PipeError> {
        let capacity = NonZeroUsize::new(self.capacity).ok_or(PipeError::InvalidCapacity)?;
        // Ranked ordering is not producer FIFO. Broadcast inputs explicitly
        // permit drop-newest; channel inputs retain blocking backpressure.
        Ok(PipeCapabilities::try_new(
            [PipeCapability::RankedEventOrder]
                .into_iter()
                .chain((!self.drop_when_full).then_some(PipeCapability::Backpressure)),
            Some(capacity),
        )?)
    }
    fn create(&self) -> Result<ProvidedPipe, PipeError> {
        Err(PipeError::Backend(anyhow::anyhow!(
            "ranked input requires its shared queue binding"
        )))
    }
    fn create_with_resources(
        &self,
        resources: &BTreeMap<ResourceId, ResourceHandle>,
    ) -> Result<ProvidedPipe, PipeError> {
        let queue = resources
            .get(&self.queue)
            .ok_or_else(|| PipeError::Backend(anyhow::anyhow!("ranked input queue is unbound")))?
            .get::<RankedInputQueue>()
            .map_err(|error| PipeError::Backend(error.into()))?;
        let pipe = queue.bind(self)?;
        let control = pipe.control.clone();
        Ok(ProvidedPipe {
            pipe: Box::new(pipe),
            control,
        })
    }
}

struct Control {
    queue: Weak<RankedInputQueue>,
    binding: Arc<Binding>,
}
#[async_trait]
impl PipeControl for Control {
    fn close(&self) {
        if let Some(queue) = self.queue.upgrade() {
            queue.close(&self.binding, false);
        }
    }
    fn cancel(&self) {
        if let Some(queue) = self.queue.upgrade() {
            queue.close(&self.binding, true);
        }
    }
    async fn is_idle(&self) -> Result<bool, PipeError> {
        let queue = self.queue.upgrade().ok_or(PipeError::Closed)?;
        let state = queue.state.lock().map_err(|_| poisoned())?;
        if RankedInputQueue::closure(&state, &self.binding) == Closure::Cancelled {
            return Err(PipeError::Closed);
        }
        let idle = !state.heap.iter().any(|entry| {
            entry.key.1 == self.binding.rank && entry.generation == self.binding.generation
        });
        Ok(idle)
    }
    fn metrics(&self) -> Option<PipeMetricsSnapshot> {
        Some(self.binding.metrics.snapshot())
    }
}

struct Sender {
    queue: Arc<RankedInputQueue>,
    binding: Arc<Binding>,
    config: RankedInputPipeConfig,
}
impl Drop for Sender {
    fn drop(&mut self) {
        self.queue.close(&self.binding, false);
    }
}

#[async_trait]
impl EnvelopeSender for Sender {
    async fn send(&self, envelope: ChangeEnvelope) -> Result<EnqueueReceipt, SendFailure> {
        let key = match &self.config.source_id {
            Some(source) => GraphChangeCodec::source_metadata(&envelope)
                .map_err(anyhow::Error::from)
                .and_then(|metadata| {
                    let metadata = metadata.ok_or_else(|| {
                        anyhow::anyhow!("ranked SourceEvent input omitted source metadata")
                    })?;
                    anyhow::ensure!(
                        &metadata.source_id == source,
                        "ranked input source identity changed"
                    );
                    let sequence = metadata.sequence.ok_or_else(|| {
                        anyhow::anyhow!(
                            "source '{source}' omitted its authoritative event sequence"
                        )
                    })?;
                    Ok((metadata.timestamp, self.binding.rank, sequence))
                }),
            None => envelope
                .system()
                .timestamp()
                .map(|timestamp| (timestamp, self.binding.rank, envelope.system().sequence()))
                .ok_or_else(|| anyhow::anyhow!("ranked native input omitted event time")),
        };
        let key = match key {
            Ok(key) => key,
            Err(error) => {
                return Err(SendFailure {
                    envelope,
                    error: PipeError::Backend(error),
                })
            }
        };
        let mut blocked = false;
        loop {
            let changed = self.queue.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = match self.queue.state.lock() {
                    Ok(state) => state,
                    Err(_) => {
                        return Err(SendFailure {
                            envelope,
                            error: poisoned(),
                        })
                    }
                };
                if state.closed || RankedInputQueue::closure(&state, &self.binding) != Closure::Open
                {
                    return Err(SendFailure {
                        envelope,
                        error: PipeError::Closed,
                    });
                }
                if state.heap.len() < self.queue.capacity {
                    let admission = state.next_admission;
                    let Some(next) = admission.checked_add(1) else {
                        return Err(SendFailure {
                            envelope,
                            error: exhausted(),
                        });
                    };
                    state.next_admission = next;
                    let receipt = EnqueueReceipt::new(envelope.id().clone());
                    self.binding.metrics.accepted();
                    state.heap.push(
                        Arc::new(Entry {
                            key,
                            generation: self.binding.generation,
                            envelope,
                        }),
                        admission,
                    );
                    drop(state);
                    self.queue.changed.notify_waiters();
                    return Ok(receipt);
                }
                if self.config.drop_when_full {
                    // Acceptance by a declared lossy input is not delivery.
                    self.binding.metrics.accepted();
                    self.binding.metrics.discarded(1);
                    return Ok(EnqueueReceipt::new(envelope.id().clone()));
                }
                if !blocked {
                    self.binding.metrics.blocked();
                    blocked = true;
                }
            }
            changed.await;
        }
    }
}

struct Receiver {
    queue: Arc<RankedInputQueue>,
    binding: Arc<Binding>,
}
impl Drop for Receiver {
    fn drop(&mut self) {
        self.queue.close(&self.binding, true);
    }
}
#[async_trait]
impl EnvelopeReceiver for Receiver {
    async fn receive(&mut self) -> Result<Option<Delivery>, PipeError> {
        loop {
            let changed = self.queue.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let mut state = self.queue.state.lock().map_err(|_| poisoned())?;
                let closure = RankedInputQueue::closure(&state, &self.binding);
                if state.closed || closure == Closure::Cancelled {
                    return Ok(None);
                }
                if state.heap.peek().is_some_and(|entry| {
                    entry.key.1 == self.binding.rank && entry.generation == self.binding.generation
                }) {
                    let entry = state.heap.pop().expect("ranked head");
                    self.binding.metrics.delivered();
                    drop(state);
                    self.queue.changed.notify_waiters();
                    return Ok(Some(Delivery::new(entry.envelope.clone(), None)));
                }
                if closure == Closure::Draining
                    && !state.heap.iter().any(|entry| {
                        entry.key.1 == self.binding.rank
                            && entry.generation == self.binding.generation
                    })
                {
                    return Ok(None);
                }
            }
            changed.await;
        }
    }
}

struct RankedPipe {
    capabilities: PipeCapabilities,
    sender: Arc<Sender>,
    receiver: Option<Receiver>,
    control: Arc<Control>,
}
impl Pipe for RankedPipe {
    fn metrics(&self) -> Option<PipeMetricsSnapshot> {
        self.control.metrics()
    }
    fn capabilities(&self) -> &PipeCapabilities {
        &self.capabilities
    }
    fn sender(&self) -> Arc<dyn EnvelopeSender> {
        self.sender.clone()
    }
    fn take_receiver(&mut self) -> Result<Box<dyn EnvelopeReceiver>, PipeError> {
        self.receiver
            .take()
            .map(|receiver| Box::new(receiver) as Box<dyn EnvelopeReceiver>)
            .ok_or(PipeError::ReceiverTaken)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        channels::{SourceControl, SourceEvent, SourceEventWrapper},
        profiling::ProfilingMetadata,
        queries::priority_queue::QueryEventQueue,
    };
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use futures::{stream::FuturesUnordered, StreamExt};
    use std::time::Duration;

    fn event(source: &str, sequence: u64, time: i64) -> Arc<SourceEventWrapper> {
        let change = if source == crate::sources::future_queue_source::FUTURE_QUEUE_SOURCE_ID {
            SourceEvent::Control(SourceControl::FuturesDue)
        } else {
            SourceEvent::Change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(source, &sequence.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: 99_000 - sequence,
                    },
                    properties: ElementPropertyMap::default(),
                },
            })
        };
        let mut event = SourceEventWrapper::with_sequence(
            source.into(),
            change,
            chrono::DateTime::from_timestamp_millis(time).unwrap(),
            sequence,
            Some(ProfilingMetadata {
                source_ns: Some(77),
                source_receive_ns: Some(sequence),
                ..Default::default()
            }),
        );
        event.set_source_position(bytes::Bytes::copy_from_slice(&sequence.to_be_bytes()));
        Arc::new(event)
    }

    fn config(
        queue: &ResourceId,
        rank: usize,
        source: &str,
        capacity: usize,
    ) -> RankedInputPipeConfig {
        RankedInputPipeConfig {
            queue: queue.clone(),
            capacity,
            source_rank: rank,
            source_id: Some(source.into()),
            drop_when_full: false,
        }
    }
    fn encode(event: Arc<SourceEventWrapper>, sequence: u64) -> ChangeEnvelope {
        GraphChangeCodec::encode_source_event(
            event.clone(),
            &ComponentId::try_new("adapter").unwrap(),
            StreamId::try_new(format!("native/{}", event.source_id)).unwrap(),
            sequence,
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn ranked_queues_match_time_declaration_rank_and_raw_sequence_with_all_metadata() {
        let future = crate::sources::future_queue_source::FUTURE_QUEUE_SOURCE_ID;
        for sources in [["z-source", "a-source", "earlier"], ["a-source", "z-source", "earlier"]] {
            let legacy = QueryEventQueue::new(16, sources).unwrap();
            let queue_id = ResourceId::try_new("inbox").unwrap();
            let queue = RankedInputQueue::new(16).unwrap();
            let bindings = BTreeMap::from([(queue_id.clone(), queue.resource())]);
            let names: Vec<_> = sources.into_iter().chain([future]).collect();
            let mut pipes: Vec<_> = names
                .iter()
                .enumerate()
                .map(|(rank, source)| {
                    config(&queue_id, rank, source, 16)
                        .create_with_resources(&bindings)
                        .unwrap()
                })
                .collect();
            let frames = [
                event("z-source", 2, 2_000),
                event("a-source", 2, 2_000),
                event("z-source", 1, 2_000),
                event("a-source", 1, 2_000),
                event("earlier", 1, 1_000),
                event(future, 1, 2_000),
            ];
            for (ordinal, frame) in frames.iter().enumerate() {
                legacy.enqueue_wait(frame.clone()).await.unwrap();
                let rank = names
                    .iter()
                    .position(|source| *source == frame.source_id)
                    .unwrap();
                pipes[rank]
                    .pipe
                    .sender()
                    .send(encode(frame.clone(), ordinal as u64 + 100))
                    .await
                    .unwrap();
            }
            let mut receivers: Vec<_> = pipes
                .iter_mut()
                .map(|pipe| pipe.pipe.take_receiver().unwrap())
                .collect();
            async fn receive(
                receiver: &mut Box<dyn EnvelopeReceiver>,
            ) -> (&mut Box<dyn EnvelopeReceiver>, Delivery) {
                let delivery = receiver.receive().await.unwrap().unwrap();
                (receiver, delivery)
            }
            let mut pending: FuturesUnordered<_> = receivers.iter_mut().map(receive).collect();
            let mut trace = Vec::new();
            for _ in &frames {
                let expected = legacy.dequeue().await;
                let (receiver, delivered) =
                    tokio::time::timeout(Duration::from_secs(2), pending.next())
                        .await
                        .unwrap()
                        .unwrap();
                let metadata = GraphChangeCodec::source_metadata(delivered.envelope())
                    .unwrap()
                    .unwrap();
                assert_eq!(metadata.source_id, expected.source_id);
                assert_eq!(metadata.sequence, Some(expected.sequence));
                assert_eq!(metadata.timestamp, expected.timestamp);
                assert_eq!(metadata.profiling, expected.profiling);
                assert_eq!(
                    metadata.source_position.as_deref(),
                    expected.source_position.as_deref()
                );
                assert_ne!(
                    delivered.envelope().system().sequence(),
                    metadata.sequence.unwrap()
                );
                match &expected.event {
                    SourceEvent::Change(change) => assert_eq!(
                        GraphChangeCodec::decode_changes(delivered.envelope()).unwrap(),
                        vec![change.clone()]
                    ),
                    SourceEvent::Control(SourceControl::FuturesDue) => {
                        assert!(GraphChangeCodec::is_futures_due(delivered.envelope()))
                    }
                    _ => unreachable!(),
                }
                trace.push((metadata.source_id, metadata.sequence.unwrap()));
                pending.push(receive(receiver));
            }
            assert_eq!(
                trace,
                vec![
                    ("earlier".into(), 1),
                    (sources[0].into(), 1),
                    (sources[0].into(), 2),
                    (sources[1].into(), 1),
                    (sources[1].into(), 2),
                    (future.into(), 1),
                ]
            );
        }
    }

    #[tokio::test]
    async fn shared_capacity_closure_and_generation_fences_do_not_wait_for_quiet_sources() {
        let id = ResourceId::try_new("inbox").unwrap();
        let queue = RankedInputQueue::new(1).unwrap();
        let resources = BTreeMap::from([(id.clone(), queue.resource())]);
        let mut first = config(&id, 0, "a", 1)
            .create_with_resources(&resources)
            .unwrap();
        let mut second = config(&id, 1, "b", 1)
            .create_with_resources(&resources)
            .unwrap();
        let first_receiver = first.pipe.take_receiver().unwrap();
        let mut second_receiver = second.pipe.take_receiver().unwrap();
        let old_sender = first.pipe.sender();
        old_sender
            .send(encode(event("a", 1, 1000), 10))
            .await
            .unwrap();
        let second_sender = second.pipe.sender();
        let mut blocked = Box::pin(second_sender.send(encode(event("b", 1, 1000), 11)));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut blocked)
                .await
                .is_err()
        );
        drop(first_receiver);
        tokio::time::timeout(Duration::from_secs(2), blocked)
            .await
            .unwrap()
            .unwrap();
        let mut replacement = config(&id, 0, "a", 1)
            .create_with_resources(&resources)
            .unwrap();
        let _quiet = replacement.pipe.take_receiver().unwrap();
        first.control.cancel();
        assert!(old_sender
            .send(encode(event("a", 2, 1000), 12))
            .await
            .is_err());
        let delivered = tokio::time::timeout(Duration::from_secs(2), second_receiver.receive())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            GraphChangeCodec::source_metadata(delivered.envelope())
                .unwrap()
                .unwrap()
                .source_id,
            "b"
        );
        assert!(queue.state.lock().unwrap().heap.is_empty());
    }

    #[tokio::test]
    async fn lossy_admission_and_missing_source_sequence_are_explicit() {
        use crate::computation::v1::{ChangeEnvelope, ComponentId, ContextEntry, ContextValue};

        let id = ResourceId::try_new("inbox").unwrap();
        let queue = RankedInputQueue::new(1).unwrap();
        let resources = BTreeMap::from([(id.clone(), queue.resource())]);
        let mut config = config(&id, 0, "source", 1);
        config.drop_when_full = true;
        let pipe = config.create_with_resources(&resources).unwrap();
        let sender = pipe.pipe.sender();
        sender
            .send(encode(event("source", 1, 1000), 10))
            .await
            .unwrap();
        sender
            .send(encode(event("source", 2, 1000), 11))
            .await
            .unwrap();
        assert_eq!(queue.state.lock().unwrap().heap.len(), 1);
        assert_eq!(pipe.control.metrics().unwrap().discarded, 1);
        let original = encode(event("source", 3, 1000), 12);
        let mut metadata = GraphChangeCodec::source_metadata(&original)
            .unwrap()
            .unwrap();
        metadata.sequence = None;
        let mut missing = ChangeEnvelope::new(
            original.id().clone(),
            original.changes().clone(),
            original.system().as_ref().clone(),
        );
        missing
            .append_annotation(
                ContextEntry::try_new(
                    ComponentId::try_new("source").unwrap(),
                    "drasi.legacy-source.v1",
                    ContextValue::Bytes(serde_json::to_vec(&metadata).unwrap().into()),
                )
                .unwrap(),
            )
            .unwrap();
        let error = sender.send(missing).await.unwrap_err();
        assert!(format!("{error:#}").contains("authoritative event sequence"));
    }

    #[tokio::test]
    async fn native_and_compatibility_queues_distinguish_colliding_source_from_scheduled_work() {
        let future = crate::sources::future_queue_source::FUTURE_QUEUE_SOURCE_ID;
        let config = crate::Query::cypher("query")
            .query("MATCH (n) RETURN n")
            .from_source(future)
            .build();
        let settings = QueryExecutionSettings::from_legacy_config(&config);
        settings.validate(None).unwrap();

        let timestamp = chrono::DateTime::from_timestamp_millis(2000).unwrap();
        let data = Arc::new(SourceEventWrapper::with_sequence(
            future.into(),
            SourceEvent::Change(SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(future, "data"),
                        labels: vec!["Item".into()].into(),
                        effective_from: 1,
                    },
                    properties: ElementPropertyMap::default(),
                },
            }),
            timestamp,
            129,
            None,
        ));
        let scheduled = Arc::new(SourceEventWrapper::with_sequence(
            future.into(),
            SourceEvent::Control(SourceControl::FuturesDue),
            timestamp,
            1,
            None,
        ));
        let compatibility = QueryEventQueue::new(2, [future]).unwrap();
        compatibility.enqueue_wait(scheduled.clone()).await.unwrap();
        compatibility.enqueue_wait(data.clone()).await.unwrap();
        assert!(Arc::ptr_eq(&compatibility.dequeue().await, &data));
        assert!(Arc::ptr_eq(&compatibility.dequeue().await, &scheduled));

        let queue_id = ResourceId::try_new("colliding-inputs").unwrap();
        let queue = RankedInputQueue::new(2).unwrap();
        let bindings = BTreeMap::from([(queue_id.clone(), queue.resource())]);
        let mut data_pipe = RankedInputPipeConfig {
            queue: queue_id.clone(),
            capacity: 2,
            source_rank: 0,
            source_id: Some(future.into()),
            drop_when_full: false,
        }
        .create_with_resources(&bindings)
        .unwrap();
        let mut scheduled_pipe = RankedInputPipeConfig {
            queue: queue_id,
            capacity: 2,
            source_rank: 1,
            source_id: None,
            drop_when_full: false,
        }
        .create_with_resources(&bindings)
        .unwrap();
        scheduled_pipe
            .pipe
            .sender()
            .send(encode(scheduled, 1))
            .await
            .unwrap();
        data_pipe
            .pipe
            .sender()
            .send(encode(data, 130))
            .await
            .unwrap();
        let data = data_pipe
            .pipe
            .take_receiver()
            .unwrap()
            .receive()
            .await
            .unwrap()
            .unwrap();
        let scheduled = scheduled_pipe
            .pipe
            .take_receiver()
            .unwrap()
            .receive()
            .await
            .unwrap()
            .unwrap();
        assert!(!GraphChangeCodec::is_futures_due(data.envelope()));
        assert!(GraphChangeCodec::is_futures_due(scheduled.envelope()));
        assert_eq!(
            GraphChangeCodec::source_metadata(data.envelope())
                .unwrap()
                .unwrap()
                .sequence,
            Some(129)
        );
    }
}
