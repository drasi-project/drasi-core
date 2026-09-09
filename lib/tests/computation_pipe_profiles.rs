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

#![cfg(feature = "computation")]

#[allow(dead_code)]
mod computation_support;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;

#[tokio::test]
async fn broadcast_retains_exact_capacity_and_reports_lag_without_faking_backpressure() {
    let mut pipe = BroadcastPipe::new(BroadcastPipeConfig {
        capacity: 3,
        lag_policy: BroadcastLagPolicy::Report,
    })
    .expect("broadcast");
    assert_eq!(pipe.capabilities().capacity().expect("bounded").get(), 3);
    assert!(!pipe
        .capabilities()
        .supported()
        .contains(&PipeCapability::Backpressure));
    assert!(!pipe
        .capabilities()
        .supported()
        .contains(&PipeCapability::DurableAcceptance));
    let sender = pipe.sender();
    let control = pipe.control();
    let mut receiver = pipe.take_receiver().expect("receiver");
    drop(pipe);
    for sequence in 1..=5 {
        sender
            .send(root("source", sequence, &[sequence as u16]))
            .await
            .expect("nonblocking send");
    }
    let metrics = control.metrics().expect("metrics");
    assert_eq!(
        (
            metrics.accepted,
            metrics.discarded,
            metrics.queued,
            metrics.max_queued
        ),
        (5, 2, 3, 3)
    );
    assert!(matches!(
        receiver.receive().await,
        Err(PipeError::Lagged { skipped: 2 })
    ));
    control.close();
    for sequence in 3..=5 {
        let event = receiver
            .receive()
            .await
            .expect("receive")
            .expect("retained event");
        assert_eq!(event.envelope().system().sequence(), sequence);
    }
    assert!(receiver.receive().await.expect("drained").is_none());
    let metrics = control.metrics().expect("metrics");
    assert_eq!((metrics.delivered, metrics.queued), (3, 0));
    assert!(matches!(
        sender
            .send(root("source", 6, &[6]))
            .await
            .expect_err("closed")
            .error,
        PipeError::Closed
    ));
}

#[tokio::test]
async fn explicitly_lossy_broadcast_policy_continues_after_recorded_gap() {
    let mut pipe = BroadcastPipe::new(BroadcastPipeConfig {
        capacity: 1,
        lag_policy: BroadcastLagPolicy::SkipWithNotification,
    })
    .expect("pipe");
    let sender = pipe.sender();
    let control = pipe.control();
    let mut receiver = pipe.take_receiver().expect("receiver");
    sender.send(root("source", 1, &[1])).await.expect("one");
    let original = root("source", 2, &[2]);
    sender.send(original.clone()).await.expect("two");
    let (mut envelope, ack) = receiver
        .receive()
        .await
        .expect("explicitly skip lag")
        .expect("latest")
        .into_parts();
    assert!(ack.is_none());
    assert!(Arc::ptr_eq(envelope.event(), original.event()));
    envelope
        .append_annotation(annotation("sink"))
        .expect("annotation");
    assert_eq!(original.annotations().len(), 1);
    assert_eq!(envelope.annotations().len(), 2);
    assert_eq!(control.metrics().expect("metrics").discarded, 1);
    drop(receiver);
    assert!(sender.send(root("source", 3, &[3])).await.is_err());
}

struct LimitedSender {
    inner: Arc<dyn EnvelopeSender>,
    control: Arc<dyn PipeControl>,
    attempts: AtomicUsize,
}

#[async_trait]
impl EnvelopeSender for LimitedSender {
    async fn send(
        &self,
        envelope: ChangeEnvelope,
    ) -> std::result::Result<EnqueueReceipt, SendFailure> {
        if self.attempts.fetch_add(1, Ordering::SeqCst) == 2 {
            self.control.close();
        }
        self.inner.send(envelope).await
    }
}

#[tokio::test]
async fn batch_failure_preserves_accepted_receipts_failed_event_and_unattempted_suffix() {
    let mut pipe = BoundedPipe::new(2).expect("pipe");
    let control = pipe.control();
    let mut receiver = pipe.take_receiver().expect("receiver");
    let sender = LimitedSender {
        inner: pipe.sender(),
        control: control.clone(),
        attempts: AtomicUsize::new(0),
    };
    let failure = sender
        .send_batch(
            (1..=5)
                .map(|sequence| root("source", sequence, &[1]))
                .collect(),
        )
        .await
        .expect_err("third send closes");
    assert_eq!(failure.accepted.len(), 2);
    assert_eq!(failure.failed.envelope.system().sequence(), 3);
    assert!(matches!(failure.failed.error, PipeError::Closed));
    assert_eq!(
        failure
            .not_attempted
            .iter()
            .map(|event| event.system().sequence())
            .collect::<Vec<_>>(),
        [4, 5]
    );
    assert_eq!(sender.attempts.load(Ordering::SeqCst), 3);
    for sequence in [1, 2] {
        assert_eq!(
            receiver
                .receive()
                .await
                .expect("receive")
                .expect("event")
                .envelope()
                .system()
                .sequence(),
            sequence
        );
    }
    assert!(receiver
        .receive()
        .await
        .expect("closed and drained")
        .is_none());
    let metrics = control.metrics().expect("metrics");
    assert_eq!(
        (metrics.accepted, metrics.delivered, metrics.queued),
        (2, 2, 0)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_batch_backpressure_metrics_and_cancellation_are_explicit() {
    let mut pipe = BoundedPipe::new(1).expect("pipe");
    let sender = pipe.sender();
    let control = pipe.control();
    let mut receiver = pipe.take_receiver().expect("receiver");
    sender.send(root("source", 1, &[1])).await.expect("first");
    let mut blocked = Box::pin(sender.send(root("source", 2, &[2])));
    assert!(futures::poll!(&mut blocked).is_pending());
    assert_eq!(control.metrics().expect("metrics").blocked_sends, 1);
    control.cancel();
    assert!(matches!(
        blocked.await.expect_err("cancelled wait").error,
        PipeError::Closed
    ));
    assert!(receiver.receive().await.expect("cancelled queue").is_none());
    let metrics = control.metrics().expect("metrics");
    assert_eq!(
        (
            metrics.accepted,
            metrics.delivered,
            metrics.discarded,
            metrics.queued
        ),
        (1, 0, 1, 0)
    );
}

#[tokio::test]
async fn broadcast_waiters_wake_when_all_senders_drop_or_control_cancels() {
    for cancel in [false, true] {
        let mut pipe = BroadcastPipe::new(BroadcastPipeConfig {
            capacity: 1,
            lag_policy: BroadcastLagPolicy::Report,
        })
        .expect("pipe");
        let mut receiver = pipe.take_receiver().expect("receiver");
        let control = pipe.control();
        let mut receive = Box::pin(receiver.receive());
        assert!(futures::poll!(&mut receive).is_pending());
        if cancel {
            control.cancel();
        } else {
            drop(pipe);
        }
        assert!(receive.await.expect("woken").is_none());
    }
}

struct DrainingSource {
    inner: FiniteSource,
    completed: Arc<AtomicUsize>,
    ready: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ComputationComponent for DrainingSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.inner.start().await
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.inner.stop().await
    }
}

#[async_trait]
impl EnvelopeSource for DrainingSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let next = self.inner.next().await?;
        if next.is_none() {
            self.completed.fetch_add(1, Ordering::SeqCst);
            self.ready.notify_waiters();
        }
        Ok(next)
    }
}

struct PrimedSink {
    descriptor: ComponentDescriptor,
    completed: Arc<AtomicUsize>,
    ready: Arc<tokio::sync::Notify>,
    received: Received,
}

#[async_trait]
impl ComputationComponent for PrimedSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        loop {
            let notified = self.ready.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.completed.load(Ordering::SeqCst) == 2 {
                return Ok(());
            }
            notified.await;
        }
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for PrimedSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.received.lock().expect("received").push(input);
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn event_time_merge_orders_available_stream_heads_without_reordering_a_producer() {
    let completed = Arc::new(AtomicUsize::new(0));
    let ready = Arc::new(tokio::sync::Notify::new());
    let received = Received::default();
    let mut builder = ComputationGraph::builder("event-time-merge")
        .sink(Box::new(PrimedSink {
            descriptor: descriptor("sink", &["a", "b"], &[]),
            completed: completed.clone(),
            ready: ready.clone(),
            received: received.clone(),
        }))
        .input_merge(component("sink"), InputMergePolicy::EventTimeAcrossStreams);
    for (name, times) in [("a", [50, 10]), ("b", [30, 60])] {
        let events = times
            .into_iter()
            .enumerate()
            .map(|(index, time)| {
                let sequence = index as u64 + 1;
                output(ChangeEnvelope::new(
                    emission_id(&stream(name), sequence).expect("id"),
                    changes(name, sequence, &[time as u16]),
                    SystemMetadata::new(stream(name), sequence).with_timestamp(
                        chrono::DateTime::from_timestamp(time, 0).expect("timestamp"),
                    ),
                ))
            })
            .collect();
        builder = builder
            .source(Box::new(DrainingSource {
                inner: FiniteSource::new(name, events),
                completed: completed.clone(),
                ready: ready.clone(),
            }))
            .bind_stream(endpoint(name, "out"), stream(name))
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint("sink", name)),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            );
    }
    let mut graph = builder.build().expect("graph");
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        graph.start().expect("scope"),
    )
    .await
    .expect("no all-start gate deadlock")
    .expect("finite merge");
    let received = received.lock().expect("received");
    let observed: Vec<_> = received
        .iter()
        .map(|input| {
            (
                input.envelope.system().stream().as_str().to_owned(),
                input.envelope.system().sequence(),
                input
                    .envelope
                    .system()
                    .timestamp()
                    .expect("unaltered timestamp")
                    .timestamp(),
            )
        })
        .collect();
    assert_eq!(
        observed,
        [
            ("b/out".into(), 1, 30),
            ("a/out".into(), 1, 50),
            ("a/out".into(), 2, 10),
            ("b/out".into(), 2, 60),
        ]
    );
}
