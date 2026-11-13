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

#[path = "support/mod.rs"]
mod fixture_support;
use fixture_support::FixtureGraphBuilderExt as _;

mod computation_support;

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;
use tokio::sync::mpsc;
use tracing::{instrument::WithSubscriber, Instrument};
use tracing_subscriber::{layer::Context, prelude::*, registry::LookupSpan, Layer};

#[derive(Default)]
struct Calls {
    start: AtomicUsize,
    stop: AtomicUsize,
    failures: AtomicUsize,
}

struct ControlledSource {
    descriptor: ComponentDescriptor,
    calls: Arc<Calls>,
    events: VecDeque<OutputEnvelope>,
}

#[async_trait]
impl ComputationComponent for ControlledSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.calls.start.fetch_add(1, Ordering::SeqCst);
        if self
            .calls
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            anyhow::bail!("upstream is unavailable");
        }
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.calls.stop.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for ControlledSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        if let Some(event) = self.events.pop_front() {
            Ok(Some(event))
        } else {
            std::future::pending().await
        }
    }
}

struct ControlledSink {
    descriptor: ComponentDescriptor,
    calls: Arc<Calls>,
    received: mpsc::Sender<ChangeEnvelope>,
}

#[async_trait]
impl ComputationComponent for ControlledSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.calls.start.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.calls.stop.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for ControlledSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }

    async fn handle(&mut self, mut input: InputEnvelope) -> anyhow::Result<()> {
        input
            .envelope
            .append_annotation(annotation(self.descriptor.id(), "controlled-sink"))?;
        self.received
            .send(input.envelope)
            .await
            .map_err(|_| anyhow::anyhow!("test receiver closed"))
    }
}

fn source(
    id: &drasi_core_types::ComponentId,
    calls: Arc<Calls>,
    events: Vec<OutputEnvelope>,
) -> Box<dyn EnvelopeSource> {
    Box::new(ControlledSource {
        descriptor: descriptor(id, &[], &["out"]),
        calls,
        events: events.into(),
    })
}

fn sink(
    id: &drasi_core_types::ComponentId,
    calls: Arc<Calls>,
    received: mpsc::Sender<ChangeEnvelope>,
) -> Box<dyn EnvelopeSink> {
    Box::new(ControlledSink {
        descriptor: descriptor(id, &["in"], &[]),
        calls,
        received,
    })
}

macro_rules! both_tokio_flavors {
    ($name:ident, $scenario:ident) => {
        mod $name {
            #[tokio::test(flavor = "current_thread")]
            async fn current_thread() {
                tokio::time::timeout(super::Duration::from_secs(10), super::$scenario())
                    .await
                    .expect("controller deadlocked");
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn multi_thread() {
                tokio::time::timeout(super::Duration::from_secs(10), super::$scenario())
                    .await
                    .expect("controller deadlocked");
            }
        }
    };
}

#[derive(Clone)]
struct ProducerPollObserver {
    component_id: String,
    sends: Arc<AtomicUsize>,
    polls: Arc<Mutex<Vec<usize>>>,
}

#[derive(Default)]
struct ProducerPoll {
    // The controller's instrumented future enters once per poll; nested entries
    // must not split that poll, and span-local state follows worker migration.
    depth: usize,
    start: usize,
}

#[derive(Default)]
struct ProducerFields {
    component_id: String,
}

impl tracing::field::Visit for ProducerFields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "component_id" {
            self.component_id = format!("{value:?}");
        }
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "component_id" {
            self.component_id = value.to_owned();
        }
    }
}

impl ProducerPollObserver {
    fn new(component_id: &drasi_core_types::ComponentId, sends: Arc<AtomicUsize>) -> Self {
        Self {
            component_id: component_id.to_string(),
            sends,
            polls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn assert_budget(&self, expected: usize) {
        let polls = self.polls.lock().expect("producer poll observations");
        assert!(!polls.is_empty(), "no nonempty producer polls observed");
        assert_eq!(self.sends.load(Ordering::SeqCst), expected);
        assert_eq!(polls.iter().sum::<usize>(), expected);
        assert!(
            polls.iter().all(|work| *work <= 64),
            "producer exceeded its per-poll work budget: {polls:?}"
        );
    }
}

impl<S> Layer<S> for ProducerPollObserver
where
    S: tracing::Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(
        &self,
        attributes: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        context: Context<'_, S>,
    ) {
        if attributes.metadata().name() != "computation_component" {
            return;
        }
        let mut fields = ProducerFields::default();
        attributes.record(&mut fields);
        if fields.component_id == self.component_id {
            context
                .span(id)
                .expect("producer span")
                .extensions_mut()
                .insert(ProducerPoll::default());
        }
    }

    fn on_enter(&self, id: &tracing::span::Id, context: Context<'_, S>) {
        let span = context.span(id).expect("entered span");
        let mut extensions = span.extensions_mut();
        if let Some(poll) = extensions.get_mut::<ProducerPoll>() {
            if poll.depth == 0 {
                poll.start = self.sends.load(Ordering::SeqCst);
            }
            poll.depth += 1;
        }
    }

    fn on_exit(&self, id: &tracing::span::Id, context: Context<'_, S>) {
        let span = context.span(id).expect("exited span");
        let mut extensions = span.extensions_mut();
        if let Some(poll) = extensions.get_mut::<ProducerPoll>() {
            poll.depth = poll.depth.checked_sub(1).expect("balanced producer span");
            if poll.depth == 0 {
                let work = self.sends.load(Ordering::SeqCst) - poll.start;
                if work > 0 {
                    self.polls
                        .lock()
                        .expect("producer poll observations")
                        .push(work);
                }
            }
        }
    }
}

#[test]
#[should_panic(expected = "producer exceeded its per-poll work budget")]
fn producer_poll_observer_rejects_unbudgeted_work_with_nested_span_entries() {
    let id = component(429);
    let sends = Arc::new(AtomicUsize::new(0));
    let observer = ProducerPollObserver::new(&id, sends.clone());
    let subscriber = tracing_subscriber::registry().with(observer.clone());
    futures::executor::block_on(
        async {
            let span = tracing::info_span!("computation_component", component_id = %id);
            async {
                for _ in 0..65 {
                    span.in_scope(|| sends.fetch_add(1, Ordering::SeqCst));
                }
            }
            .instrument(span.clone())
            .await;
        }
        .with_subscriber(subscriber),
    );
    assert_eq!(
        *observer.polls.lock().expect("producer poll observations"),
        [65]
    );
    observer.assert_budget(65);
}

struct ReadySource {
    descriptor: ComponentDescriptor,
    produced: Arc<AtomicUsize>,
    limit: usize,
}

#[async_trait]
impl ComputationComponent for ReadySource {
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
impl EnvelopeSource for ReadySource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        let sequence = self.produced.load(Ordering::SeqCst);
        if sequence == self.limit {
            return Ok(None);
        }
        self.produced.fetch_add(1, Ordering::SeqCst);
        Ok(Some(output(root(
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            "source",
            sequence as u64,
            &[1],
        ))))
    }
}

struct ProgressSink {
    descriptor: ComponentDescriptor,
    received: Arc<AtomicUsize>,
}

#[async_trait]
impl ComputationComponent for ProgressSink {
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
impl EnvelopeSink for ProgressSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        self.received.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

async fn ready_source_fairness() {
    const EVENTS: usize = 4096;
    let produced = Arc::new(AtomicUsize::new(0));
    let sends = Arc::new(AtomicUsize::new(0));
    let observer = ProducerPollObserver::new(&component(77), sends.clone());
    let subscriber = tracing_subscriber::registry().with(observer.clone());
    let received = Arc::new(AtomicUsize::new(0));
    let mut graph = ComputationGraph::builder("ready-source-fairness")
        .fixture_source(Box::new(ReadySource {
            descriptor: descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &[],
                &["out"],
            ),
            produced: produced.clone(),
            limit: EVENTS,
        }))
        .fixture_sink(Box::new(ProgressSink {
            descriptor: descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                &["in"],
                &[],
            ),
            received: received.clone(),
        }))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(PollAuditedPipe {
                provider: Box::new(BroadcastPipeConfig {
                    capacity: EVENTS,
                    lag_policy: BroadcastLagPolicy::Report,
                }),
                sends,
            }),
        )
        .build()
        .unwrap();
    async { graph.start().unwrap().await.unwrap() }
        .with_subscriber(subscriber)
        .await;
    assert_eq!(produced.load(Ordering::SeqCst), EVENTS);
    assert_eq!(received.load(Ordering::SeqCst), EVENTS);
    observer.assert_budget(EVENTS);
}

both_tokio_flavors!(ready_source_work_budget, ready_source_fairness);

struct PollAuditedPipe {
    provider: Box<dyn PipeProvider>,
    sends: Arc<AtomicUsize>,
}

impl PipeProvider for PollAuditedPipe {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError> {
        self.provider.capabilities()
    }

    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        let provided = self.provider.create()?;
        Ok(ProvidedPipe {
            pipe: Box::new(PollAuditedTransport {
                inner: provided.pipe,
                sends: self.sends.clone(),
            }),
            control: provided.control,
        })
    }
}

struct PollAuditedTransport {
    inner: Box<dyn Pipe>,
    sends: Arc<AtomicUsize>,
}

impl Pipe for PollAuditedTransport {
    fn capabilities(&self) -> &PipeCapabilities {
        self.inner.capabilities()
    }

    fn sender(&self) -> Arc<dyn EnvelopeSender> {
        Arc::new(PollAuditedSender {
            inner: self.inner.sender(),
            sends: self.sends.clone(),
        })
    }

    fn take_receiver(&mut self) -> std::result::Result<Box<dyn EnvelopeReceiver>, PipeError> {
        self.inner.take_receiver()
    }
}

struct PollAuditedSender {
    inner: Arc<dyn EnvelopeSender>,
    sends: Arc<AtomicUsize>,
}

#[async_trait]
impl EnvelopeSender for PollAuditedSender {
    async fn send(&self, envelope: Envelope) -> std::result::Result<EnqueueReceipt, SendFailure> {
        self.sends.fetch_add(1, Ordering::SeqCst);
        // Tokio's transport budget must not substitute for the graph's work budget.
        tokio::task::unconstrained(self.inner.send(envelope)).await
    }
}

async fn ready_output_burst_fairness() {
    const EVENTS: usize = 4096;
    let sends = Arc::new(AtomicUsize::new(0));
    let observer = ProducerPollObserver::new(&component(429), sends.clone());
    let subscriber = tracing_subscriber::registry().with(observer.clone());
    let received = Arc::new(AtomicUsize::new(0));
    let mut graph = ComputationGraph::builder("ready-output-burst")
        .fixture_source(Box::new(FiniteSource::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            vec![output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            ))],
        )))
        .fixture_transformer(Box::new(NativeTransform::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                .expect("fixture UUID"),
            |_| {
                Ok((1..=EVENTS)
                    .map(|sequence| {
                        output(root(
                            &drasi_lib::ComponentId::try_new(
                                "00000000-0000-0000-0000-0000000001ad",
                            )
                            .expect("fixture UUID"),
                            "transform",
                            sequence as u64,
                            &[1],
                        ))
                    })
                    .collect())
            },
        )))
        .fixture_sink(Box::new(ProgressSink {
            descriptor: descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                &["in"],
                &[],
            ),
            received: received.clone(),
        }))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("transform"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(PollAuditedPipe {
                provider: Box::new(BoundedPipeConfig { capacity: EVENTS }),
                sends,
            }),
        )
        .build()
        .unwrap();
    async { graph.start().unwrap().await.unwrap() }
        .with_subscriber(subscriber)
        .await;
    assert_eq!(received.load(Ordering::SeqCst), EVENTS);
    observer.assert_budget(EVENTS);
}

both_tokio_flavors!(ready_output_burst_work_budget, ready_output_burst_fairness);

async fn stop_ready_source() {
    const EVENTS: usize = 4096;
    let produced = Arc::new(AtomicUsize::new(0));
    let mut graph = ComputationGraph::builder("stop-ready-source")
        .fixture_source(Box::new(ReadySource {
            descriptor: descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &[],
                &["out"],
            ),
            produced: produced.clone(),
            limit: EVENTS,
        }))
        .fixture_sink(Box::new(ProgressSink {
            descriptor: descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                &["in"],
                &[],
            ),
            received: Arc::new(AtomicUsize::new(0)),
        }))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BroadcastPipeConfig {
                capacity: EVENTS,
                lag_policy: BroadcastLagPolicy::Report,
            }),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .unwrap();
        while produced.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let report = control
            .stop_components(GraphRevision(1), GraphSelection::All)
            .await
            .unwrap();
        assert_eq!(report.summary, OperationSummary::Completed);
        assert!(
            produced.load(Ordering::SeqCst) < EVENTS,
            "stop waited for source exhaustion"
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

both_tokio_flavors!(ready_source_control_commands, stop_ready_source);

struct ContinuingTransformer {
    descriptor: ComponentDescriptor,
    remaining: usize,
    progressed: Arc<AtomicUsize>,
}

#[async_trait]
impl ComputationComponent for ContinuingTransformer {
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
impl Transformer for ContinuingTransformer {
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.remaining = 4096;
        Ok(vec![])
    }
    fn has_pending_emissions(&self) -> bool {
        self.remaining > 0
    }
    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.remaining -= 1;
        self.progressed.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }
}

async fn stop_ready_continuation() {
    let progressed = Arc::new(AtomicUsize::new(0));
    let mut graph = ComputationGraph::builder("stop-ready-continuation")
        .fixture_source(Box::new(FiniteSource::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            vec![output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[1],
            ))],
        )))
        .fixture_transformer(Box::new(ContinuingTransformer {
            descriptor: descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
                &["in"],
                &["out"],
            ),
            remaining: 0,
            progressed: progressed.clone(),
        }))
        .fixture_sink(Box::new(CollectSink::new(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            "sink",
            &["in"],
            Default::default(),
        )))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("transform"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .unwrap();
        while progressed.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let report = tokio::time::timeout(
            Duration::from_secs(2),
            control.stop_components(GraphRevision(1), GraphSelection::All),
        )
        .await
        .expect("stop must remain bounded")
        .unwrap();
        assert_eq!(report.summary, OperationSummary::Completed);
        assert_eq!(
            progressed.load(Ordering::SeqCst),
            4096,
            "graceful stop completes the admitted input's continuations"
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

both_tokio_flavors!(ready_continuation_control_commands, stop_ready_continuation);

async fn independent_activation() {
    let source_calls = Arc::new(Calls::default());
    source_calls.failures.store(1, Ordering::SeqCst);
    let sink_calls = Arc::new(Calls::default());
    let (received_tx, mut received) = mpsc::channel(4);
    let transform = NativeTransform::new(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
            .expect("fixture UUID"),
        |input| {
            Ok(vec![output(derived(
                &input.envelope,
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
                "transform",
                1,
                input.envelope.changes().clone(),
            ))])
        },
    );
    let mut graph = ComputationGraph::builder("independent-activation")
        .fixture_source(source(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            source_calls.clone(),
            vec![output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "source",
                1,
                &[7],
            ))],
        ))
        .fixture_transformer(Box::new(transform))
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            sink_calls.clone(),
            received_tx,
        ))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("transform"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("valid graph");
    let run = graph.start().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let report = control.startup_report().await.expect("startup report");
        assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
        assert!(matches!(
            report.components[&component(77)],
            StartOutcome::StartFailed(_)
        ));
        assert!(matches!(
            report.components[&component(429)],
            StartOutcome::Started
        ));
        assert!(matches!(
            report.components[&component(101)],
            StartOutcome::Started
        ));
        let observed = control.observed();
        assert_eq!(
            observed.components[&component(77)].realization,
            RealizationState::Created
        );
        assert_eq!(
            observed.components[&component(429)].lifecycle,
            ComponentLifecycle::Running
        );
        assert_eq!(
            observed.components[&component(101)].lifecycle,
            ComponentLifecycle::Running
        );
        assert_eq!(
            observed.relationships[&edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID")
            )]
                .binding,
            BindingState::Bound
        );
        assert_eq!(
            observed.relationships[&edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                    .expect("fixture UUID")
            )]
                .availability,
            DataAvailability::Unavailable
        );
        assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 0);
        assert!(received.try_recv().is_err(), "failure is not data or EOF");

        let stopped = control
            .stop_components(GraphRevision(1), GraphSelection::Exact(vec![component(77)]))
            .await
            .expect("stop failed source before explicit retry");
        assert!(matches!(
            stopped.components[&component(77)],
            StopOutcome::Stopped
        ));
        let restarted = control
            .start_components(GraphRevision(1), GraphSelection::Exact(vec![component(77)]))
            .await
            .expect("explicit retry");
        assert!(matches!(
            restarted.components[&component(77)],
            StartOutcome::Started
        ));
        let result = received
            .recv()
            .await
            .expect("existing consumers receive after source recovers");
        assert_eq!(values(&result), [7]);
        assert_eq!(
            sink_calls.start.load(Ordering::SeqCst),
            1,
            "downstream was not restarted"
        );
        assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 0);
        assert_eq!(source_calls.start.load(Ordering::SeqCst), 2);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(source_calls.stop.load(Ordering::SeqCst), 2);
    assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 1);
}

both_tokio_flavors!(source_failure_is_not_eof, independent_activation);

async fn revision_and_generation() {
    let source_calls = Arc::new(Calls::default());
    let sink_calls = Arc::new(Calls::default());
    let (received, _receiver) = mpsc::channel(4);
    let mut graph = ComputationGraph::builder("revision-and-generation")
        .fixture_source(source(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            source_calls.clone(),
            vec![],
        ))
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            sink_calls.clone(),
            received,
        ))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("caller-driven controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let deployment = control
            .deployment_report()
            .await
            .expect("create without start");
        assert_eq!(deployment.summary, OperationSummary::Completed);
        assert_eq!(source_calls.start.load(Ordering::SeqCst), 0);
        assert_eq!(sink_calls.start.load(Ordering::SeqCst), 0);
        let revision = control
            .set_lifecycle_policy(
                GraphRevision(1),
                component(101),
                LifecyclePolicy { auto_start: false },
            )
            .await
            .expect("CAS policy edit");
        assert_eq!(revision, GraphRevision(2));
        assert!(matches!(
            control
                .start_components(GraphRevision(1), GraphSelection::All)
                .await,
            Err(GraphError::StaleRevision { .. })
        ));
        assert_eq!(source_calls.start.load(Ordering::SeqCst), 0);
        let report = control
            .start_components(revision, GraphSelection::All)
            .await
            .expect("start at current revision");
        assert!(matches!(
            report.components[&component(77)],
            StartOutcome::Started
        ));
        assert!(matches!(
            report.components[&component(101)],
            StartOutcome::NotRequested
        ));
        let observed = control.observed();
        let node = observed.components[&component(77)].clone();
        assert!(matches!(
            control
                .report_health(
                    revision,
                    HealthObservation {
                        component: component(77),
                        generation: ComponentGeneration(node.generation.0 + 1),
                        operation: node.operation,
                        health: ComponentHealth::Unavailable,
                    }
                )
                .await,
            Err(GraphError::StaleGeneration)
        ));
        control
            .report_health(
                revision,
                HealthObservation {
                    component: component(77),
                    generation: node.generation,
                    operation: node.operation,
                    health: ComponentHealth::Degraded,
                },
            )
            .await
            .expect("generation-bound health");
        assert_eq!(
            control.observed().components[&component(77)].health,
            ComponentHealth::Degraded
        );
        let stopped = control
            .stop_components(revision, GraphSelection::All)
            .await
            .expect("stop selection");
        assert!(matches!(
            stopped.components[&component(101)],
            StopOutcome::AlreadyStopped
        ));
        assert!(matches!(
            control
                .report_health(
                    revision,
                    HealthObservation {
                        component: component(77),
                        generation: node.generation,
                        operation: node.operation,
                        health: ComponentHealth::Healthy,
                    }
                )
                .await,
            Err(GraphError::StaleGeneration)
        ));
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(source_calls.stop.load(Ordering::SeqCst), 1);
    assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 0);
    assert!(matches!(
        control
            .start_components(GraphRevision(2), GraphSelection::All)
            .await,
        Err(GraphError::ControllerClosed)
    ));
}

both_tokio_flavors!(live_cas_and_health, revision_and_generation);

struct FailingPipe;

impl PipeProvider for FailingPipe {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError> {
        BoundedPipeConfig { capacity: 1 }.capabilities()
    }

    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        Err(PipeError::Backend(anyhow::anyhow!(
            "injected pipe construction failure"
        )))
    }
}

#[tokio::test]
async fn independent_branch_survives_partial_binding_creation_failure() {
    let failed_source = Arc::new(Calls::default());
    let failed_sink = Arc::new(Calls::default());
    let good_source = Arc::new(Calls::default());
    let good_sink = Arc::new(Calls::default());
    let (received_tx, mut received_rx) = mpsc::channel(4);
    let mut graph = ComputationGraph::builder("partial-binding")
        .fixture_source(source(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ba")
                .expect("fixture UUID"),
            failed_source.clone(),
            vec![],
        ))
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f3")
                .expect("fixture UUID"),
            failed_sink.clone(),
            received_tx.clone(),
        ))
        .fixture_source(source(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001bb")
                .expect("fixture UUID"),
            good_source.clone(),
            vec![output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001bb")
                    .expect("fixture UUID"),
                "good-source",
                1,
                &[9],
            ))],
        ))
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002d7")
                .expect("fixture UUID"),
            good_sink.clone(),
            received_tx,
        ))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ba")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("bad-source"),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001bb")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("good-source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ba")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f3")
                    .expect("fixture UUID"),
            ),
            Box::new(FailingPipe),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001bb")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002d7")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("all declarations valid");
    let run = graph.start().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let deployment = control
            .deployment_report()
            .await
            .expect("per-item creation results");
        assert_eq!(deployment.components.len(), 4);
        assert!(matches!(
            deployment.components[&component(442)],
            CreationOutcome::Blocked { .. }
        ));
        assert!(matches!(
            deployment.components[&component(443)],
            CreationOutcome::Created
        ));
        let startup = control
            .startup_report()
            .await
            .expect("all eligible starts attempted");
        assert!(matches!(
            startup.components[&component(442)],
            StartOutcome::NotCreated
        ));
        assert!(matches!(
            startup.components[&component(727)],
            StartOutcome::Started
        ));
        assert_eq!(
            values(&received_rx.recv().await.expect("independent branch event")),
            [9]
        );
        assert_eq!(
            control.desired_snapshot().nodes.len(),
            4,
            "failed desired items remain"
        );
        assert_eq!(failed_source.start.load(Ordering::SeqCst), 0);
        assert_eq!(failed_sink.start.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(good_sink.stop.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_activation_coupling_blocks_only_the_declared_dependency() {
    let source_calls = Arc::new(Calls::default());
    source_calls.failures.store(1, Ordering::SeqCst);
    let sink_calls = Arc::new(Calls::default());
    let (received, _receiver) = mpsc::channel(1);
    let connection = edge(
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
            .expect("fixture UUID"),
        &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
            .expect("fixture UUID"),
    );
    let mut graph = ComputationGraph::builder("explicit-coupling")
        .fixture_source(source(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            source_calls,
            vec![],
        ))
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            sink_calls.clone(),
            received,
        ))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            connection.clone(),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .relationship_policy(
            connection,
            RelationshipPolicy {
                activation: ActivationCoupling::RequiresRunning,
                ..Default::default()
            },
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deployment");
        let report = control
            .start_components(GraphRevision(1), GraphSelection::All)
            .await
            .expect("partial report");
        assert!(matches!(
            report.components[&component(101)],
            StartOutcome::Blocked { .. }
        ));
        assert_eq!(sink_calls.start.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

struct PendingStartSource {
    descriptor: ComponentDescriptor,
    entered: Arc<tokio::sync::Notify>,
    stops: Arc<AtomicUsize>,
}

#[async_trait]
impl ComputationComponent for PendingStartSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.entered.notify_one();
        std::future::pending().await
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for PendingStartSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        anyhow::bail!("processing cannot begin before a successful start")
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a_live_stop_command_cancels_pending_start_without_waiting_for_its_component_lock() {
    let entered = Arc::new(tokio::sync::Notify::new());
    let source_stops = Arc::new(AtomicUsize::new(0));
    let sink_calls = Arc::new(Calls::default());
    let (received, _receiver) = mpsc::channel(1);
    let mut graph = ComputationGraph::builder("interrupt-start")
        .fixture_source(Box::new(PendingStartSource {
            descriptor: descriptor(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &[],
                &["out"],
            ),
            entered: entered.clone(),
            stops: source_stops.clone(),
        }))
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            sink_calls.clone(),
            received,
        ))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    let run = graph.run().expect("controller");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.expect("deployment");
        let (started, stopped) = tokio::join!(
            control.start_components(GraphRevision(1), GraphSelection::All),
            async {
                entered.notified().await;
                control
                    .stop_components(GraphRevision(1), GraphSelection::Exact(vec![component(77)]))
                    .await
            }
        );
        assert!(matches!(
            started.expect("complete all start outcomes").components[&component(77)],
            StartOutcome::StartFailed(_)
        ));
        assert!(matches!(
            stopped.expect("stop interrupted source").components[&component(77)],
            StopOutcome::Stopped
        ));
        assert_eq!(source_stops.load(Ordering::SeqCst), 1);
        assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 0);
        assert_eq!(
            control.observed().components[&component(101)].lifecycle,
            ComponentLifecycle::Running
        );
        assert_eq!(
            control.observed().relationships[&edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID")
            )]
                .binding,
            BindingState::Bound
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(source_stops.load(Ordering::SeqCst), 1);
    assert_eq!(sink_calls.stop.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_failure_propagation_stops_only_its_dependent_closure() {
    let failed = Arc::new(Calls::default());
    failed.failures.store(1, Ordering::SeqCst);
    let dependent = Arc::new(Calls::default());
    let independent = Arc::new(Calls::default());
    let (received, mut receiver) = mpsc::channel(4);
    let mut graph = ComputationGraph::builder("propagation")
        .fixture_source(source(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000035")
                .expect("fixture UUID"),
            failed,
            vec![],
        ))
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f4")
                .expect("fixture UUID"),
            dependent.clone(),
            received.clone(),
        ))
        .fixture_source(source(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b4")
                .expect("fixture UUID"),
            Arc::new(Calls::default()),
            vec![output(root(
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001b4")
                    .expect("fixture UUID"),
                "independent",
                1,
                &[11],
            ))],
        ))
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f5")
                .expect("fixture UUID"),
            independent.clone(),
            received,
        ))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000035")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("failed"),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b4")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("independent"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000035")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f4")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b4")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f5")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .relationship_policy(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000035")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f4")
                    .expect("fixture UUID"),
            ),
            RelationshipPolicy {
                propagate_failure: true,
                ..Default::default()
            },
        )
        .build()
        .expect("graph");
    let run = graph.start().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.startup_report().await.expect("all start outcomes");
        let mut observed = control.subscribe_observed();
        observed
            .wait_for(|state| {
                state.components[&component(1012)].lifecycle == ComponentLifecycle::Failed
            })
            .await
            .expect("dependent cleanup completed");
        assert_eq!(dependent.stop.load(Ordering::SeqCst), 1);
        assert_eq!(independent.stop.load(Ordering::SeqCst), 0);
        assert_eq!(
            values(
                &receiver
                    .recv()
                    .await
                    .expect("independent branch still runs")
            ),
            [11]
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(
        dependent.stop.load(Ordering::SeqCst),
        1,
        "no duplicate stop after propagation"
    );
}

#[tokio::test]
async fn explicit_creation_dependency_blocks_when_its_required_instance_is_unrealized() {
    let (received, _receiver) = mpsc::channel(1);
    let mut graph = ComputationGraph::builder("creation-dependency")
        .fixture_source(source(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            Arc::new(Calls::default()),
            vec![],
        ))
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f6")
                .expect("fixture UUID"),
            Arc::new(Calls::default()),
            received.clone(),
        ))
        .fixture_sink(sink(
            &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f4")
                .expect("fixture UUID"),
            Arc::new(Calls::default()),
            received,
        ))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f6")
                    .expect("fixture UUID"),
            ),
            Box::new(FailingPipe),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f4")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .relationship_policy(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f4")
                    .expect("fixture UUID"),
            ),
            RelationshipPolicy {
                required_for_creation: true,
                ..Default::default()
            },
        )
        .build()
        .expect("valid declarations");
    let run = graph.run().expect("scope");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        let deployment = control.deployment_report().await.expect("deployment");
        let CreationOutcome::Blocked {
            dependencies,
            resources,
        } = &deployment.components[&component(1012)]
        else {
            panic!("dependent was constructed without its required source");
        };
        assert!(dependencies.contains(&component(77)));
        assert!(resources.is_empty());
        let observed = control.observed();
        assert_eq!(
            observed.relationships[&edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f6")
                    .expect("fixture UUID")
            )]
                .binding,
            BindingState::Failed
        );
        assert_eq!(
            observed.relationships[&edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f4")
                    .expect("fixture UUID")
            )]
                .binding,
            BindingState::Declared
        );
        assert_eq!(
            observed.relationships[&edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000003f4")
                    .expect("fixture UUID")
            )]
                .availability,
            DataAvailability::Unavailable
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
}

#[tokio::test]
async fn failed_optional_binding_never_becomes_available_idle_or_exhausted() {
    for finite in [false, true] {
        let (received, _receiver) = mpsc::channel(2);
        let source: Box<dyn EnvelopeSource> = if finite {
            Box::new(FiniteSource::new(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                vec![output(root(
                    &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "source",
                    1,
                    &[1],
                ))],
            ))
        } else {
            source(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                Arc::new(Calls::default()),
                vec![],
            )
        };
        let mut graph = ComputationGraph::builder("optional-binding")
            .fixture_source(source)
            .fixture_sink(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001cf")
                    .expect("fixture UUID"),
                Arc::new(Calls::default()),
                received.clone(),
            ))
            .fixture_sink(sink(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c5")
                    .expect("fixture UUID"),
                Arc::new(Calls::default()),
                received,
            ))
            .bind_stream(
                endpoint(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("source"),
            )
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001cf")
                        .expect("fixture UUID"),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c5")
                        .expect("fixture UUID"),
                ),
                Box::new(FailingPipe),
            )
            .relationship_policy(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c5")
                        .expect("fixture UUID"),
                ),
                RelationshipPolicy {
                    required_for_binding: false,
                    ..Default::default()
                },
            )
            .build()
            .expect("valid graph");
        let run = graph.run().expect("scope");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            control.deployment_report().await.expect("partial bindings");
            control
                .start_components(GraphRevision(1), GraphSelection::All)
                .await
                .expect("independent startup");
            if finite {
                let mut observed = control.subscribe_observed();
                observed
                    .wait_for(|state| {
                        state.relationships[&edge(
                            &drasi_core_types::ComponentId::try_new(
                                "00000000-0000-0000-0000-00000000004d",
                            )
                            .expect("fixture UUID"),
                            &drasi_core_types::ComponentId::try_new(
                                "00000000-0000-0000-0000-0000000001cf",
                            )
                            .expect("fixture UUID"),
                        )]
                            .availability
                            == DataAvailability::Exhausted
                    })
                    .await
                    .expect("real finite EOF");
            }
            let node = control.observed().components[&component(77)].clone();
            control
                .report_health(
                    GraphRevision(1),
                    HealthObservation {
                        component: component(77),
                        generation: node.generation,
                        operation: node.operation,
                        health: ComponentHealth::Healthy,
                    },
                )
                .await
                .expect("valid health observation");
            assert_eq!(
                control.observed().relationships[&edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c5")
                        .expect("fixture UUID")
                )]
                    .binding,
                BindingState::Failed
            );
            assert_eq!(
                control.observed().relationships[&edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c5")
                        .expect("fixture UUID")
                )]
                    .availability,
                DataAvailability::Unavailable
            );
            control
                .stop_components(GraphRevision(1), GraphSelection::Exact(vec![component(77)]))
                .await
                .expect("stop source");
            assert_eq!(
                control.observed().relationships[&edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001c5")
                        .expect("fixture UUID")
                )]
                    .availability,
                DataAvailability::Unavailable
            );
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
    }
}
