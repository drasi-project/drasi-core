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

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::Poll,
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_lib::computation::v1::*;
use futures::{future::pending, poll};
use tokio::sync::Notify;

fn component(id: &str) -> ComponentId {
    ComponentId::try_new(id).expect("valid fixture component ID")
}

fn port(id: &str) -> PortId {
    PortId::try_new(id).expect("valid fixture port ID")
}

fn endpoint(node: &str, name: &str) -> Endpoint {
    Endpoint::new(component(node), port(name))
}

fn schema() -> SchemaDescriptor {
    SchemaDescriptor::try_new(
        SchemaId::try_new("test.empty").expect("valid fixture schema ID"),
        SchemaVersion::try_new(1).expect("nonzero schema version"),
        "empty",
        Bytes::from_static(b"no records"),
    )
    .expect("valid empty schema")
}

fn envelope(sequence: u64) -> Envelope {
    let stream = StreamId::try_new("source/out").expect("valid fixture stream");
    Envelope::new(
        emission_id(&stream, sequence).expect("valid emission ID"),
        ChangeSet::try_new(
            ChangeSetId::try_new("test", Bytes::from_static(b"empty")).expect("valid changeset ID"),
            schema(),
            Vec::new(),
        )
        .expect("valid empty changeset"),
        SystemMetadata::new(stream, sequence),
    )
}

#[derive(Clone, Default)]
struct Signals {
    starts: Arc<Mutex<Vec<String>>>,
    stops: Arc<Mutex<Vec<String>>>,
    active: Arc<AtomicUsize>,
    handled: Arc<AtomicUsize>,
    produced: Arc<AtomicUsize>,
    entered: Arc<Notify>,
}

struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn new(signals: &Signals) -> Self {
        signals.active.fetch_add(1, Ordering::SeqCst);
        Self(signals.active.clone())
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Clone, Copy, Default)]
enum Behavior {
    #[default]
    Normal,
    FailStart,
    PendingStart,
    FailStop,
    PendingStop,
    FailProcessing,
    PendingProcessing,
    EmitOnStart,
    RepeatOnStart,
}

struct Fixture {
    descriptor: ComponentDescriptor,
    signals: Signals,
    behavior: Behavior,
    values: VecDeque<Envelope>,
    sequence: u64,
    completion: SinkCompletion,
}

impl Fixture {
    fn new(id: &str, directions: &[(&str, PortDirection)], signals: &Signals) -> Self {
        Self {
            descriptor: ComponentDescriptor::try_new(
                component(id),
                directions
                    .iter()
                    .map(|(id, direction)| {
                        PortDescriptor::new(
                            port(id),
                            *direction,
                            schema(),
                            PipeRequirements::default(),
                        )
                    })
                    .collect(),
            )
            .expect("valid fixture descriptor"),
            signals: signals.clone(),
            behavior: Behavior::Normal,
            values: VecDeque::new(),
            sequence: 0,
            completion: SinkCompletion::Handled,
        }
    }

    fn source(signals: &Signals) -> Self {
        Self::new("source", &[("out", PortDirection::Output)], signals)
    }

    fn sink(signals: &Signals) -> Self {
        Self::new("sink", &[("in", PortDirection::Input)], signals)
    }

    fn transformer(signals: &Signals) -> Self {
        Self::new(
            "transform",
            &[("in", PortDirection::Input), ("out", PortDirection::Output)],
            signals,
        )
    }

    async fn processing(&self) -> anyhow::Result<()> {
        let _active = InFlight::new(&self.signals);
        self.signals.entered.notify_one();
        match self.behavior {
            Behavior::PendingProcessing => pending().await,
            Behavior::FailProcessing => anyhow::bail!("processing failed"),
            _ => Ok(()),
        }
    }
}

#[async_trait]
impl ComputationComponent for Fixture {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }

    async fn start(&mut self) -> anyhow::Result<()> {
        self.signals
            .starts
            .lock()
            .expect("start log lock")
            .push(self.descriptor.id().to_string());
        let _active = InFlight::new(&self.signals);
        if matches!(
            self.behavior,
            Behavior::EmitOnStart | Behavior::RepeatOnStart
        ) {
            if matches!(self.behavior, Behavior::EmitOnStart) {
                self.sequence += 1;
            } else {
                self.sequence = 1;
            }
            self.values.push_back(envelope(self.sequence));
        }
        match self.behavior {
            Behavior::FailStart => anyhow::bail!("start failed"),
            Behavior::PendingStart => {
                self.signals.entered.notify_one();
                pending().await
            }
            _ => Ok(()),
        }
    }

    async fn stop(&mut self) -> anyhow::Result<()> {
        self.signals
            .stops
            .lock()
            .expect("stop log lock")
            .push(self.descriptor.id().to_string());
        if matches!(self.behavior, Behavior::PendingStop) {
            let _active = InFlight::new(&self.signals);
            pending::<()>().await;
        }
        if matches!(self.behavior, Behavior::FailStop) {
            self.behavior = Behavior::Normal;
            anyhow::bail!("stop failed once");
        }
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for Fixture {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        self.processing().await?;
        self.signals.produced.fetch_add(1, Ordering::SeqCst);
        Ok(self.values.pop_front().map(|envelope| OutputEnvelope {
            port: port("out"),
            envelope,
        }))
    }
}

#[async_trait]
impl EnvelopeSink for Fixture {
    fn completion(&self) -> SinkCompletion {
        self.completion
    }

    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        self.processing().await?;
        self.signals.handled.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl Transformer for Fixture {
    async fn transform(&mut self, input: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.processing().await?;
        self.sequence += 1;
        let stream = StreamId::try_new("transform/out").expect("valid transform stream");
        Ok(vec![OutputEnvelope {
            port: port("out"),
            envelope: input.envelope.derive(
                emission_id(&stream, self.sequence).expect("valid transform emission ID"),
                input.envelope.changes().clone(),
                SystemMetadata::new(stream, self.sequence),
            ),
        }])
    }
}

fn direct(
    source: Fixture,
    sink: Fixture,
    provider: Box<dyn PipeProvider>,
) -> ComputationGraphBuilder {
    ComputationGraph::builder("lifecycle")
        .source(Box::new(source))
        .sink(Box::new(sink))
        .bind_stream(
            endpoint("source", "out"),
            StreamId::try_new("source/out").expect("valid fixture stream"),
        )
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("sink", "in")),
            provider,
        )
}

fn chain(source: Fixture, transform: Fixture, sink: Fixture) -> ComputationGraph {
    ComputationGraph::builder("chain")
        .source(Box::new(source))
        .transformer(Box::new(transform))
        .sink(Box::new(sink))
        .bind_stream(
            endpoint("source", "out"),
            StreamId::try_new("source/out").expect("valid fixture stream"),
        )
        .bind_stream(
            endpoint("transform", "out"),
            StreamId::try_new("transform/out").expect("valid transform stream"),
        )
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("transform", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("transform", "out"), endpoint("sink", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("valid chain graph")
}

async fn pipe_boundaries() {
    assert!(matches!(
        BoundedPipe::new(0),
        Err(PipeError::InvalidCapacity)
    ));
    assert!(matches!(
        BoundedPipe::new(usize::MAX),
        Err(PipeError::InvalidCapacity)
    ));
    assert!(matches!(
        BoundedPipeConfig {
            capacity: usize::MAX
        }
        .capabilities(),
        Err(PipeError::InvalidCapacity)
    ));
    let mut pipe = BoundedPipe::new(1).expect("valid capacity");
    assert_eq!(
        pipe.capabilities().supported(),
        PipeCapabilities::volatile_bounded(
            std::num::NonZeroUsize::new(1).expect("nonzero capacity")
        )
        .supported()
    );
    let control = pipe.control();
    let sender = pipe.sender();
    let mut receiver = pipe.take_receiver().expect("first receiver");
    assert!(matches!(
        pipe.take_receiver(),
        Err(PipeError::ReceiverTaken)
    ));
    drop(pipe);

    let first = envelope(1);
    sender
        .send(first.clone())
        .await
        .expect("enqueue first event");
    let mut blocked = Box::pin(sender.send(envelope(2)));
    assert!(poll!(&mut blocked).is_pending());
    drop(blocked); // Cancelling capacity wait never inserts event 2.
    let (received, ack) = receiver
        .receive()
        .await
        .expect("receive event")
        .expect("first event")
        .into_parts();
    assert!(ack.is_none());
    assert!(Arc::ptr_eq(first.changes(), received.changes()));
    assert!(Arc::ptr_eq(first.system(), received.system()));
    let mut waiting = Box::pin(receiver.receive());
    assert!(poll!(&mut waiting).is_pending());
    drop(waiting);
    sender
        .send(envelope(3))
        .await
        .expect("enqueue after cancelled receive");
    let mut blocked = Box::pin(sender.send(envelope(4)));
    assert!(poll!(&mut blocked).is_pending());
    control.close();
    let failure = blocked
        .await
        .expect_err("closed while waiting for capacity");
    assert!(matches!(failure.error, PipeError::Closed));
    assert_eq!(failure.envelope.system().sequence(), 4);
    assert_eq!(
        receiver
            .receive()
            .await
            .expect("drain closed pipe")
            .expect("last accepted event")
            .envelope()
            .system()
            .sequence(),
        3
    );
    assert!(receiver.receive().await.expect("drained pipe").is_none());

    let mut pipe = BoundedPipe::new(1).expect("valid capacity");
    let sender = pipe.sender();
    let receiver = pipe.take_receiver().expect("first receiver");
    sender.send(envelope(1)).await.expect("first enqueue");
    let mut blocked = Box::pin(sender.send(envelope(2)));
    assert!(poll!(&mut blocked).is_pending());
    drop(receiver);
    assert!(matches!(
        blocked.await.expect_err("receiver dropped").error,
        PipeError::Closed
    ));

    let mut pipe = BoundedPipe::new(1).expect("valid capacity");
    let control = pipe.control();
    let sender = pipe.sender();
    let mut receiver = pipe.take_receiver().expect("first receiver");
    drop(pipe);
    sender.send(envelope(1)).await.expect("first enqueue");
    let mut blocked = Box::pin(sender.send(envelope(2)));
    assert!(poll!(&mut blocked).is_pending());
    control.cancel();
    assert!(matches!(
        blocked.await.expect_err("pipe cancelled").error,
        PipeError::Closed
    ));
    assert!(receiver.receive().await.expect("cancelled pipe").is_none());
    assert!(matches!(
        sender
            .send(envelope(3))
            .await
            .expect_err("stale sender closed")
            .error,
        PipeError::Closed
    ));

    let mut pipe = BoundedPipe::new(1).expect("valid capacity");
    let sender = pipe.sender();
    let mut receiver = pipe.take_receiver().expect("first receiver");
    drop(pipe);
    sender.send(envelope(1)).await.expect("first enqueue");
    drop(sender);
    assert!(receiver
        .receive()
        .await
        .expect("drain last event")
        .is_some());
    assert!(receiver
        .receive()
        .await
        .expect("all senders dropped")
        .is_none());
}

#[tokio::test(flavor = "current_thread")]
async fn bounded_pipe_current_thread() {
    pipe_boundaries().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bounded_pipe_multi_thread() {
    pipe_boundaries().await;
}

async fn startup_failures() {
    for failing in ["sink", "transform", "source"] {
        let signals = Signals::default();
        let mut source = Fixture::source(&signals);
        let mut transform = Fixture::transformer(&signals);
        let mut sink = Fixture::sink(&signals);
        match failing {
            "source" => source.behavior = Behavior::FailStart,
            "transform" => transform.behavior = Behavior::FailStart,
            _ => sink.behavior = Behavior::FailStart,
        }
        let mut graph = chain(source, transform, sink);
        let run = graph.run().expect("run valid graph");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            control
                .deployment_report()
                .await
                .expect("deploy before activation");
            let report = control
                .start_components(GraphRevision(1), GraphSelection::All)
                .await
                .expect("all startup outcomes");
            assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
            assert_eq!(report.components.len(), 3);
            assert!(matches!(
                report.components[&component(failing)],
                StartOutcome::StartFailed(_)
            ));
            assert!(signals.stops.lock().expect("stop log").is_empty());
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
        let started = signals.starts.lock().expect("start log lock").clone();
        let stopped = signals.stops.lock().expect("stop log lock").clone();
        assert_eq!(started, ["sink", "transform", "source"]);
        assert_eq!(stopped, started.into_iter().rev().collect::<Vec<_>>());
        assert_eq!(signals.active.load(Ordering::SeqCst), 0);
        assert_eq!(graph.state(), GraphState::Cancelled);
        assert!(matches!(
            graph.start(),
            Err(GraphError::InvalidState {
                state: GraphState::Cancelled
            })
        ));
        graph.shutdown().await.expect("no remaining cleanup");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn failed_start_is_local_and_cleanup_preserves_every_attempted_resource() {
    startup_failures().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_start_multi_thread() {
    startup_failures().await;
}

async fn wait_for_pending_operation(run: &mut std::pin::Pin<Box<GraphRun<'_>>>, signals: &Signals) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            tokio::select! {
                result = run.as_mut() => panic!("scope ended before pending operation: {result:?}"),
                _ = signals.entered.notified() => {}
            }
            if signals.active.load(Ordering::SeqCst) == 1 {
                break;
            }
        }
    })
    .await
    .expect("component operation did not become pending");
}

async fn cancel_operations() {
    for position in ["source", "transform", "sink"] {
        for startup in [false, true] {
            let signals = Signals::default();
            let mut source = Fixture::source(&signals);
            source.values.push_back(envelope(1));
            let mut transform = Fixture::transformer(&signals);
            let mut sink = Fixture::sink(&signals);
            let target = match position {
                "source" => &mut source,
                "transform" => &mut transform,
                _ => &mut sink,
            };
            target.behavior = if startup {
                Behavior::PendingStart
            } else {
                Behavior::PendingProcessing
            };
            let mut graph = chain(source, transform, sink);
            let mut run = Box::pin(graph.start().expect("start valid graph"));
            let control = run.control();
            wait_for_pending_operation(&mut run, &signals).await;
            control.cancel();
            control.cancel();
            assert!(matches!(run.await, Err(GraphError::Cancelled)));
            assert_eq!(signals.active.load(Ordering::SeqCst), 0);
            assert_eq!(graph.state(), GraphState::Cancelled);
            let stops = signals.stops.lock().expect("stop log lock").len();
            graph.shutdown().await.expect("idempotent shutdown");
            graph
                .shutdown()
                .await
                .expect("repeated idempotent shutdown");
            assert_eq!(signals.stops.lock().expect("stop log lock").len(), stops);
            assert!(matches!(
                graph.start(),
                Err(GraphError::InvalidState { .. })
            ));
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_interrupts_component_calls_without_locks() {
    cancel_operations().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_interrupts_calls_multi_thread() {
    cancel_operations().await;
}

#[tokio::test]
async fn drop_retains_cleanup_ownership_without_detached_operations() {
    for behavior in [Behavior::PendingStart, Behavior::PendingProcessing] {
        let signals = Signals::default();
        let mut source = Fixture::source(&signals);
        source.values.push_back(envelope(1));
        let mut sink = Fixture::sink(&signals);
        sink.behavior = behavior;
        let mut graph = direct(source, sink, Box::new(BoundedPipeConfig { capacity: 1 }))
            .build()
            .unwrap();
        let mut run = Box::pin(graph.start().unwrap());
        let control = run.control();
        wait_for_pending_operation(&mut run, &signals).await;
        drop(run);
        assert_eq!(signals.active.load(Ordering::SeqCst), 0);
        assert_eq!(control.state(), GraphState::CleanupRequired);
        assert!(
            signals.stops.lock().unwrap().is_empty(),
            "drop cannot call async hooks"
        );
        assert!(matches!(
            graph.start(),
            Err(GraphError::InvalidState { .. })
        ));
        graph.shutdown().await.unwrap();
        assert_eq!(graph.state(), GraphState::Cancelled);
        assert_eq!(
            signals.stops.lock().unwrap().len(),
            signals.starts.lock().unwrap().len()
        );
    }
}

#[tokio::test]
async fn dropping_unpolled_run_has_no_start_effects_and_is_terminal() {
    let signals = Signals::default();
    let mut graph = direct(
        Fixture::source(&signals),
        Fixture::sink(&signals),
        Box::new(BoundedPipeConfig { capacity: 1 }),
    )
    .build()
    .unwrap();
    drop(graph.start().unwrap());
    graph.shutdown().await.unwrap();
    assert_eq!(graph.state(), GraphState::Cancelled);
    assert!(signals.starts.lock().unwrap().is_empty());
    assert!(signals.stops.lock().unwrap().is_empty());
}

#[tokio::test]
async fn precancellation_does_not_create_pipes_or_stop_unstarted_components() {
    let signals = Signals::default();
    let creates = Arc::new(AtomicUsize::new(0));
    let mut graph = direct(
        Fixture::source(&signals),
        Fixture::sink(&signals),
        Box::new(FailingProvider {
            creates: creates.clone(),
            capabilities: BoundedPipeConfig { capacity: 1 }.capabilities().unwrap(),
        }),
    )
    .build()
    .unwrap();
    let run = graph.start().unwrap();
    run.control().cancel();
    assert!(matches!(run.await, Err(GraphError::Cancelled)));
    assert_eq!(creates.load(Ordering::SeqCst), 0);
    assert!(signals.starts.lock().unwrap().is_empty());
    assert!(signals.stops.lock().unwrap().is_empty());
    assert_eq!(graph.state(), GraphState::Cancelled);
}

#[tokio::test]
async fn processing_errors_are_visible_without_stopping_independent_components() {
    for position in ["source", "transform", "sink"] {
        let signals = Signals::default();
        let mut source = Fixture::source(&signals);
        source.values.push_back(envelope(1));
        let mut transform = Fixture::transformer(&signals);
        let mut sink = Fixture::sink(&signals);
        let target = match position {
            "source" => &mut source,
            "transform" => &mut transform,
            _ => &mut sink,
        };
        target.behavior = Behavior::FailProcessing;
        let mut graph = chain(source, transform, sink);
        let run = graph.run().unwrap();
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            control.deployment_report().await.unwrap();
            control
                .start_components(GraphRevision(1), GraphSelection::All)
                .await
                .unwrap();
            let mut observed = control.subscribe_observed();
            observed
                .wait_for(|state| {
                    state.components[&component(position)]
                        .failure
                        .as_ref()
                        .is_some_and(|failure| failure.phase == FailurePhase::Processing)
                })
                .await
                .unwrap();
            assert!(signals.stops.lock().unwrap().is_empty());
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
        assert_eq!(graph.state(), GraphState::Cancelled);
        assert_eq!(
            &*signals.stops.lock().unwrap(),
            &["source", "transform", "sink"]
        );
    }
}

#[tokio::test]
async fn stop_errors_are_retained_and_cleanup_retry_is_not_restart() {
    let signals = Signals::default();
    let mut sink = Fixture::sink(&signals);
    sink.behavior = Behavior::FailStop;
    let mut graph = direct(
        Fixture::source(&signals),
        sink,
        Box::new(BoundedPipeConfig { capacity: 1 }),
    )
    .build()
    .unwrap();
    assert!(matches!(
        graph.start().unwrap().await,
        Err(GraphError::Cleanup { .. })
    ));
    assert_eq!(graph.state(), GraphState::CleanupRequired);
    graph.shutdown().await.unwrap();
    assert_eq!(graph.state(), GraphState::Cancelled);
    assert!(matches!(
        graph.start(),
        Err(GraphError::InvalidState { .. })
    ));
    assert_eq!(&*signals.stops.lock().unwrap(), &["source", "sink", "sink"]);
}

struct ObservedProvider {
    senders: Arc<Mutex<Vec<Arc<dyn EnvelopeSender>>>>,
    creations: Arc<AtomicUsize>,
}

impl PipeProvider for ObservedProvider {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError> {
        BoundedPipeConfig { capacity: 1 }.capabilities()
    }

    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        let pipe = BoundedPipe::new(1)?;
        self.creations.fetch_add(1, Ordering::SeqCst);
        self.senders
            .lock()
            .expect("observed sender lock")
            .push(pipe.sender());
        Ok(ProvidedPipe {
            control: pipe.control(),
            pipe: Box::new(pipe),
        })
    }
}

async fn full_queue_cancel() {
    let signals = Signals::default();
    let mut source = Fixture::source(&signals);
    source.values.extend((1..=10).map(envelope));
    let mut sink = Fixture::sink(&signals);
    sink.behavior = Behavior::PendingProcessing;
    let senders = Arc::new(Mutex::new(Vec::new()));
    let provider = ObservedProvider {
        senders: senders.clone(),
        creations: Arc::new(AtomicUsize::new(0)),
    };
    let mut graph = direct(source, sink, Box::new(provider))
        .build()
        .expect("valid graph");
    let mut run = Box::pin(graph.start().expect("start valid graph"));
    let control = run.control();
    // Source can hold one blocked emission, queue one, sink one: never all ten.
    assert!(poll!(&mut run).is_pending());
    assert!(signals.produced.load(Ordering::SeqCst) <= 3);
    let stale = senders.lock().expect("observed sender lock")[0].clone();
    let mut blocked = Box::pin(stale.send(envelope(999)));
    assert!(poll!(&mut blocked).is_pending());
    control.cancel();
    let (result, failed) = tokio::join!(run, blocked);
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert!(matches!(
        failed.expect_err("cancelled pipe").error,
        PipeError::Closed
    ));
    assert_eq!(signals.active.load(Ordering::SeqCst), 0);
    assert_eq!(signals.stops.lock().expect("stop log lock").len(), 2);
    assert!(matches!(
        stale
            .send(envelope(1000))
            .await
            .expect_err("stale sender rejected")
            .error,
        PipeError::Closed
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn full_queue_stop_and_stale_sender_current_thread() {
    full_queue_cancel().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_queue_stop_and_stale_sender_multi_thread() {
    full_queue_cancel().await;
}

#[tokio::test]
async fn drop_closes_old_senders_before_explicit_async_cleanup() {
    let signals = Signals::default();
    let mut source = Fixture::source(&signals);
    source.values.extend((1..=10).map(envelope));
    let mut sink = Fixture::sink(&signals);
    sink.behavior = Behavior::PendingProcessing;
    let senders = Arc::new(Mutex::new(Vec::new()));
    let mut graph = direct(
        source,
        sink,
        Box::new(ObservedProvider {
            senders: senders.clone(),
            creations: Arc::new(AtomicUsize::new(0)),
        }),
    )
    .build()
    .unwrap();
    let mut run = Box::pin(graph.start().unwrap());
    assert!(poll!(&mut run).is_pending());
    let stale = senders.lock().unwrap()[0].clone();
    let mut blocked = Box::pin(stale.send(envelope(999)));
    assert!(poll!(&mut blocked).is_pending());
    drop(run);
    assert!(matches!(
        blocked.await.unwrap_err().error,
        PipeError::Closed
    ));
    assert!(signals.stops.lock().unwrap().is_empty());
    graph.shutdown().await.unwrap();
}

#[tokio::test]
async fn finite_drain_restarts_retained_components_with_fresh_pipes_and_controls() {
    let signals = Signals::default();
    let mut source = Fixture::source(&signals);
    source.values.push_back(envelope(1));
    let mut graph = direct(
        source,
        Fixture::sink(&signals),
        Box::new(BoundedPipeConfig { capacity: 1 }),
    )
    .build()
    .unwrap();
    let run = graph.start().unwrap();
    let old = run.control();
    run.await.unwrap();
    assert_eq!(graph.state(), GraphState::Completed);
    let run = graph.start().unwrap();
    let new = run.control();
    old.cancel();
    run.await.unwrap();
    assert_eq!(new.state(), GraphState::Completed);
    assert_eq!(signals.handled.load(Ordering::SeqCst), 1);
    assert_eq!(signals.starts.lock().unwrap().len(), 4);
    assert_eq!(signals.stops.lock().unwrap().len(), 4);
}

struct FailingProvider {
    creates: Arc<AtomicUsize>,
    capabilities: PipeCapabilities,
}

impl PipeProvider for FailingProvider {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError> {
        Ok(self.capabilities.clone())
    }

    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        Err(PipeError::Closed)
    }
}

#[tokio::test]
async fn provider_failure_before_start_is_visible_and_terminal() {
    let signals = Signals::default();
    let creates = Arc::new(AtomicUsize::new(0));
    let mut graph = direct(
        Fixture::source(&signals),
        Fixture::sink(&signals),
        Box::new(FailingProvider {
            creates: creates.clone(),
            capabilities: BoundedPipeConfig { capacity: 1 }.capabilities().unwrap(),
        }),
    )
    .build()
    .unwrap();
    assert_eq!(creates.load(Ordering::SeqCst), 0);
    assert!(matches!(
        graph.start().unwrap().await,
        Err(GraphError::StartupIncomplete)
    ));
    assert!(matches!(
        graph
            .observed()
            .relationships
            .values()
            .next()
            .unwrap()
            .failure
            .as_ref()
            .unwrap()
            .cause
            .underlying(),
        GraphError::Pipe { .. }
    ));
    assert!(signals.starts.lock().unwrap().is_empty());
    assert_eq!(graph.state(), GraphState::Failed);
}

#[test]
fn whole_topology_preflight_rejects_invalid_structure_before_any_side_effects() {
    for case in [
        "duplicate-node",
        "unknown-node",
        "unknown-port",
        "wrong-direction",
        "wrong-schema",
        "role",
        "unconnected",
        "duplicate-edge",
        "missing-stream",
        "duplicate-stream",
        "input-binding",
        "zero-capacity",
        "oversized-capacity",
        "unsupported",
        "missing-fifo",
        "missing-backpressure",
        "unbounded",
        "cycle",
        "self-loop",
        "graph-id",
    ] {
        let signals = Signals::default();
        let creates = Arc::new(AtomicUsize::new(0));
        let source = Fixture::source(&signals);
        let mut sink = Fixture::sink(&signals);
        if case == "wrong-schema" {
            sink.descriptor = ComponentDescriptor::try_new(
                component("sink"),
                vec![PortDescriptor::new(
                    port("in"),
                    PortDirection::Input,
                    SchemaDescriptor::try_new(
                        SchemaId::try_new("test.empty").unwrap(),
                        SchemaVersion::try_new(1).unwrap(),
                        "empty",
                        Bytes::from_static(b"different full definition"),
                    )
                    .unwrap(),
                    PipeRequirements::default(),
                )],
            )
            .unwrap();
        }
        let mut builder = ComputationGraph::builder(if case == "graph-id" {
            "invalid id"
        } else {
            "preflight"
        })
        .source(Box::new(source));
        if case == "role" {
            builder = builder.source(Box::new(sink));
        } else {
            builder = builder.sink(Box::new(sink));
        }
        if case == "duplicate-node" {
            builder = builder.source(Box::new(Fixture::source(&signals)));
        }
        if case != "missing-stream" {
            builder = builder.bind_stream(
                if case == "input-binding" {
                    endpoint("sink", "in")
                } else {
                    endpoint("source", "out")
                },
                StreamId::try_new("source/out").unwrap(),
            );
        }
        if case == "duplicate-stream" {
            builder = builder.bind_stream(
                endpoint("source", "out"),
                StreamId::try_new("source/out").unwrap(),
            );
        }
        let definition = match case {
            "unknown-node" => {
                EdgeDefinition::new(endpoint("unknown", "out"), endpoint("sink", "in"))
            }
            "unknown-port" => {
                EdgeDefinition::new(endpoint("source", "unknown"), endpoint("sink", "in"))
            }
            "wrong-direction" => {
                EdgeDefinition::new(endpoint("sink", "in"), endpoint("source", "out"))
            }
            _ => EdgeDefinition::new(endpoint("source", "out"), endpoint("sink", "in")),
        };
        let capabilities = match case {
            "unsupported" => PipeCapabilities::try_new(
                [
                    PipeCapability::FifoPerStream,
                    PipeCapability::Backpressure,
                    PipeCapability::DurableAcceptance,
                ],
                std::num::NonZeroUsize::new(1),
            )
            .unwrap(),
            "missing-fifo" => PipeCapabilities::try_new(
                [PipeCapability::Backpressure],
                std::num::NonZeroUsize::new(1),
            )
            .unwrap(),
            "missing-backpressure" => PipeCapabilities::try_new(
                [PipeCapability::FifoPerStream],
                std::num::NonZeroUsize::new(1),
            )
            .unwrap(),
            "unbounded" => PipeCapabilities::try_new(
                [PipeCapability::FifoPerStream, PipeCapability::Backpressure],
                None,
            )
            .unwrap(),
            _ => BoundedPipeConfig { capacity: 1 }.capabilities().unwrap(),
        };
        if case != "unconnected" {
            let provider: Box<dyn PipeProvider> =
                if matches!(case, "zero-capacity" | "oversized-capacity") {
                    Box::new(BoundedPipeConfig {
                        capacity: if case == "zero-capacity" {
                            0
                        } else {
                            usize::MAX
                        },
                    })
                } else {
                    Box::new(FailingProvider {
                        creates: creates.clone(),
                        capabilities,
                    })
                };
            builder = builder.connect(definition.clone(), provider);
        }
        if case == "duplicate-edge" {
            builder = builder.connect(definition, Box::new(BoundedPipeConfig { capacity: 1 }));
        }
        if matches!(case, "cycle" | "self-loop") {
            let second = Fixture::new(
                "second",
                &[("in", PortDirection::Input), ("out", PortDirection::Output)],
                &signals,
            );
            builder = builder
                .transformer(Box::new(Fixture::transformer(&signals)))
                .transformer(Box::new(second))
                .bind_stream(
                    endpoint("transform", "out"),
                    StreamId::try_new("transform/out").unwrap(),
                )
                .bind_stream(
                    endpoint("second", "out"),
                    StreamId::try_new("second/out").unwrap(),
                );
            for (from, to) in if case == "cycle" {
                [("transform", "second"), ("second", "transform")]
            } else {
                [("transform", "transform"), ("second", "second")]
            } {
                builder = builder.connect(
                    EdgeDefinition::new(endpoint(from, "out"), endpoint(to, "in")),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                );
            }
        }
        assert!(builder.build().is_err(), "{case}");
        assert_eq!(creates.load(Ordering::SeqCst), 0, "{case}");
        assert!(signals.starts.lock().unwrap().is_empty(), "{case}");
        assert!(signals.stops.lock().unwrap().is_empty(), "{case}");
    }
}

#[test]
fn duplicate_ports_and_empty_graph_are_rejected() {
    assert!(matches!(
        ComponentDescriptor::try_new(
            component("duplicate"),
            vec![
                PortDescriptor::new(
                    port("out"),
                    PortDirection::Output,
                    schema(),
                    PipeRequirements::default()
                ),
                PortDescriptor::new(
                    port("out"),
                    PortDirection::Output,
                    schema(),
                    PipeRequirements::default()
                ),
            ]
        ),
        Err(ContractError::DuplicatePort { .. })
    ));
    assert!(ComputationGraph::builder("empty").build().is_err());
    let signals = Signals::default();
    for timeout in [std::time::Duration::ZERO, std::time::Duration::MAX] {
        assert!(direct(
            Fixture::source(&signals),
            Fixture::sink(&signals),
            Box::new(BoundedPipeConfig { capacity: 1 })
        )
        .cleanup_timeout(timeout)
        .build()
        .is_err());
    }
    assert!(signals.starts.lock().unwrap().is_empty());
}

#[tokio::test]
async fn waiting_receiver_wakes_on_all_senders_drop_and_close() {
    for close in [false, true] {
        let mut pipe = BoundedPipe::new(1).unwrap();
        let control = pipe.control();
        let sender = pipe.sender();
        let mut receiver = pipe.take_receiver().unwrap();
        drop(pipe);
        let mut waiting = Box::pin(receiver.receive());
        assert!(matches!(poll!(&mut waiting), Poll::Pending));
        if close {
            control.close();
        }
        drop(sender);
        assert!(waiting.await.unwrap().is_none());
    }
}

#[test]
fn endpoint_and_graph_requirements_never_upgrade_accepted_sinks() {
    for location in ["graph", "input", "output"] {
        for capability in [
            PipeCapability::ExplicitAcknowledgement,
            PipeCapability::Transactions,
            PipeCapability::ExactlyOnce,
        ] {
            let signals = Signals::default();
            let mut source = Fixture::source(&signals);
            let mut sink = Fixture::sink(&signals);
            sink.completion = SinkCompletion::Accepted;
            let requirements = PipeRequirements::new([capability]);
            if location == "input" {
                sink.descriptor = ComponentDescriptor::try_new(
                    component("sink"),
                    vec![PortDescriptor::new(
                        port("in"),
                        PortDirection::Input,
                        schema(),
                        requirements.clone(),
                    )],
                )
                .unwrap();
            } else if location == "output" {
                source.descriptor = ComponentDescriptor::try_new(
                    component("source"),
                    vec![PortDescriptor::new(
                        port("out"),
                        PortDirection::Output,
                        schema(),
                        requirements.clone(),
                    )],
                )
                .unwrap();
            }
            let mut builder = direct(source, sink, Box::new(BoundedPipeConfig { capacity: 1 }));
            if location == "graph" {
                builder = builder.requirements(requirements);
            }
            assert!(matches!(
                builder.build(),
                Err(GraphError::Contract(
                    ContractError::InsufficientSinkCompletion { .. }
                ))
            ));
            assert!(signals.starts.lock().unwrap().is_empty());
        }
    }
    let signals = Signals::default();
    assert!(matches!(
        direct(
            Fixture::source(&signals),
            Fixture::sink(&signals),
            Box::new(BoundedPipeConfig { capacity: 1 })
        )
        .requirements(PipeRequirements::new([PipeCapability::Replay]))
        .build(),
        Err(GraphError::Contract(ContractError::UnsupportedCapability {
            capability: PipeCapability::Replay
        }))
    ));
}

struct WrappedProvider {
    controls: Arc<Mutex<Vec<Arc<dyn PipeControl>>>>,
    failure: &'static str,
}

impl PipeProvider for WrappedProvider {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError> {
        BoundedPipeConfig { capacity: 1 }.capabilities()
    }

    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        if self.failure == "create" {
            return Err(PipeError::Closed);
        }
        let mut pipe = BoundedPipe::new(if self.failure == "capabilities" { 2 } else { 1 })?;
        if self.failure == "receiver" {
            // The actual graph take must fail, without leaving earlier controls open.
            drop(pipe.take_receiver()?);
        }
        let control = pipe.control();
        if self.failure == "closed" {
            control.close();
        }
        self.controls
            .lock()
            .expect("observed controls lock")
            .push(control.clone());
        Ok(ProvidedPipe {
            pipe: Box::new(pipe),
            control,
        })
    }
}

#[tokio::test]
async fn partial_provider_setup_retains_good_bindings_until_scoped_cleanup() {
    for failure in ["create", "receiver", "capabilities"] {
        let signals = Signals::default();
        let retained = Arc::new(Mutex::new(Vec::new()));
        let mut builder = direct(
            Fixture::source(&signals),
            Fixture::sink(&signals),
            Box::new(ObservedProvider {
                senders: retained.clone(),
                creations: Arc::new(AtomicUsize::new(0)),
            }),
        );
        builder = builder
            .sink(Box::new(Fixture::new(
                "second",
                &[("in", PortDirection::Input)],
                &signals,
            )))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("second", "in")),
                Box::new(WrappedProvider {
                    controls: Arc::new(Mutex::new(Vec::new())),
                    failure,
                }),
            );
        let mut graph = builder.build().unwrap();
        let run = graph.run().unwrap();
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            let report = control.deployment_report().await.unwrap();
            assert_eq!(report.summary, OperationSummary::CompletedWithFailures);
            let good = retained.lock().unwrap()[0].clone();
            good.send(envelope(1))
                .await
                .expect("independent binding stays open");
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
        assert!(signals.starts.lock().unwrap().is_empty());
        let old = retained.lock().unwrap()[0].clone();
        assert!(matches!(
            old.send(envelope(1)).await.unwrap_err().error,
            PipeError::Closed
        ));
    }
}

#[tokio::test]
async fn clean_restart_creates_fresh_provider_instances_and_old_controls_are_inert() {
    let signals = Signals::default();
    let controls = Arc::new(Mutex::new(Vec::new()));
    let mut source = Fixture::source(&signals);
    source.values.push_back(envelope(1));
    let mut graph = direct(
        source,
        Fixture::sink(&signals),
        Box::new(WrappedProvider {
            controls: controls.clone(),
            failure: "",
        }),
    )
    .build()
    .unwrap();
    graph.start().unwrap().await.unwrap();
    assert_eq!(controls.lock().unwrap().len(), 1);
    let old = controls.lock().unwrap()[0].clone();
    let run = graph.start().unwrap();
    old.cancel();
    run.await.unwrap();
    assert_eq!(controls.lock().unwrap().len(), 2);
    assert_eq!(graph.state(), GraphState::Completed);
}

#[tokio::test]
async fn cleanup_deadline_polls_all_hooks_and_reports_incomplete_cleanup() {
    let signals = Signals::default();
    let mut source = Fixture::source(&signals);
    source.behavior = Behavior::PendingStop;
    let mut graph = direct(
        source,
        Fixture::sink(&signals),
        Box::new(BoundedPipeConfig { capacity: 1 }),
    )
    .cleanup_timeout(std::time::Duration::from_nanos(1))
    .build()
    .unwrap();
    assert!(matches!(
        graph.start().unwrap().await,
        Err(GraphError::Cleanup { .. })
    ));
    assert_eq!(graph.state(), GraphState::CleanupRequired);
    assert_eq!(&*signals.stops.lock().unwrap(), &["source", "sink"]);
    assert_eq!(signals.active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn fanout_send_failure_reports_prior_branch_acceptance_without_retry() {
    let signals = Signals::default();
    let mut source = Fixture::source(&signals);
    source.values.push_back(envelope(1));
    let mut graph = direct(
        source,
        Fixture::sink(&signals),
        Box::new(BoundedPipeConfig { capacity: 1 }),
    )
    .sink(Box::new(Fixture::new(
        "second",
        &[("in", PortDirection::Input)],
        &signals,
    )))
    .connect(
        EdgeDefinition::new(endpoint("source", "out"), endpoint("second", "in")),
        Box::new(WrappedProvider {
            controls: Arc::new(Mutex::new(Vec::new())),
            failure: "closed",
        }),
    )
    .build()
    .unwrap();
    let error = graph.start().unwrap().await.unwrap_err();
    match error.underlying() {
        GraphError::Forward {
            accepted_branches,
            source,
            edge,
        } => {
            assert_eq!(*accepted_branches, 1);
            assert_eq!(*edge, 1);
            assert_eq!(source.envelope.system().sequence(), 1);
            assert!(matches!(source.error, PipeError::Closed));
        }
        other => panic!("expected partial fanout failure, got {other}"),
    }
    assert_eq!(signals.produced.load(Ordering::SeqCst), 1);
    assert_eq!(graph.state(), GraphState::Failed);
}

#[tokio::test]
async fn one_slow_fanout_branch_does_not_prevent_whole_graph_cancellation() {
    let signals = Signals::default();
    let mut source = Fixture::source(&signals);
    source.values.extend((1..=10).map(envelope));
    let mut second = Fixture::new("second", &[("in", PortDirection::Input)], &signals);
    second.behavior = Behavior::PendingProcessing;
    let mut graph = direct(
        source,
        Fixture::sink(&signals),
        Box::new(BoundedPipeConfig { capacity: 1 }),
    )
    .sink(Box::new(second))
    .connect(
        EdgeDefinition::new(endpoint("source", "out"), endpoint("second", "in")),
        Box::new(BoundedPipeConfig { capacity: 1 }),
    )
    .build()
    .unwrap();
    let mut run = Box::pin(graph.start().unwrap());
    let control = run.control();
    wait_for_pending_operation(&mut run, &signals).await;
    assert!(signals.produced.load(Ordering::SeqCst) <= 3);
    control.cancel();
    assert!(matches!(run.await, Err(GraphError::Cancelled)));
    assert_eq!(signals.active.load(Ordering::SeqCst), 0);
    assert_eq!(signals.stops.lock().unwrap().len(), 3);
}

#[tokio::test]
async fn clean_restart_retains_sequence_state_and_rejects_regression() {
    for behavior in [Behavior::EmitOnStart, Behavior::RepeatOnStart] {
        let signals = Signals::default();
        let mut source = Fixture::source(&signals);
        source.behavior = behavior;
        let mut graph = direct(
            source,
            Fixture::sink(&signals),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
        graph.start().unwrap().await.unwrap();
        let result = graph.start().unwrap().await;
        if matches!(behavior, Behavior::EmitOnStart) {
            result.unwrap();
            assert_eq!(signals.handled.load(Ordering::SeqCst), 2);
        } else {
            assert!(matches!(
                result.as_ref().err().map(GraphError::underlying),
                Some(GraphError::Emission { .. })
            ));
            assert_eq!(signals.handled.load(Ordering::SeqCst), 1);
        }
        assert_eq!(signals.starts.lock().unwrap().len(), 4);
        assert_eq!(signals.stops.lock().unwrap().len(), 4);
    }
}

type ObservedEpoch = (Arc<dyn EnvelopeSender>, Arc<dyn PipeControl>);

struct RestartProvider(Arc<Mutex<Vec<ObservedEpoch>>>);

impl PipeProvider for RestartProvider {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError> {
        BoundedPipeConfig { capacity: 1 }.capabilities()
    }

    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        let pipe = BoundedPipe::new(1)?;
        let control = pipe.control();
        // Deliberate test instrumentation: explicit close compensates for this
        // external clone, which would otherwise prevent natural drain.
        self.0
            .lock()
            .expect("observed epochs lock")
            .push((pipe.sender(), control.clone()));
        Ok(ProvidedPipe {
            pipe: Box::new(pipe),
            control,
        })
    }
}

#[tokio::test]
async fn stale_senders_stay_closed_across_clean_restart() {
    let signals = Signals::default();
    let mut source = Fixture::source(&signals);
    source.behavior = Behavior::EmitOnStart;
    let epochs = Arc::new(Mutex::new(Vec::new()));
    let mut graph = direct(
        source,
        Fixture::sink(&signals),
        Box::new(RestartProvider(epochs.clone())),
    )
    .build()
    .unwrap();
    graph.start().unwrap().await.unwrap();
    let (old_sender, old_control) = epochs.lock().unwrap()[0].clone();
    graph.start().unwrap().await.unwrap();
    assert!(matches!(
        old_sender.send(envelope(100)).await.unwrap_err().error,
        PipeError::Closed
    ));
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let (new_sender, _) = epochs.lock().unwrap()[2].clone();
        assert!(!Arc::ptr_eq(&old_sender, &new_sender));
        old_control.cancel();
        new_sender
            .send(envelope(100))
            .await
            .expect("old control cannot close a new binding");
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    assert_eq!(graph.state(), GraphState::Cancelled);
    assert_eq!(signals.handled.load(Ordering::SeqCst), 2);
}

#[test]
fn one_stream_cannot_reconverge_through_multiple_ports_of_one_component() {
    let signals = Signals::default();
    let source = Fixture::source(&signals);
    let sink = Fixture::new(
        "sink",
        &[("left", PortDirection::Input), ("right", PortDirection::Input)],
        &signals,
    );
    let creates = Arc::new(AtomicUsize::new(0));
    let mut builder = ComputationGraph::builder("no-duplicate-stream-queues")
        .source(Box::new(source))
        .sink(Box::new(sink))
        .bind_stream(
            endpoint("source", "out"),
            StreamId::try_new("source/out").unwrap(),
        );
    for input in ["left", "right"] {
        builder = builder.connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("sink", input)),
            Box::new(FailingProvider {
                creates: creates.clone(),
                capabilities: BoundedPipeConfig { capacity: 1 }.capabilities().unwrap(),
            }),
        );
    }
    assert!(matches!(builder.build(), Err(GraphError::Topology { .. })));
    assert_eq!(creates.load(Ordering::SeqCst), 0);
    assert!(signals.starts.lock().unwrap().is_empty());
}
