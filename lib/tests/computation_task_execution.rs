// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]

#[allow(dead_code)]
mod computation_support;

use std::{
    future::pending,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;
use tokio::sync::{mpsc, Notify};

macro_rules! both_flavors {
    ($name:ident, $scenario:ident) => {
        mod $name {
            #[tokio::test(flavor = "current_thread")]
            async fn current_thread() {
                tokio::time::timeout(super::Duration::from_secs(10), super::$scenario())
                    .await
                    .expect("owned task scenario did not finish");
            }
            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn multi_thread() {
                tokio::time::timeout(super::Duration::from_secs(10), super::$scenario())
                    .await
                    .expect("owned task scenario did not finish");
            }
        }
    };
}

#[derive(Default)]
struct Gate {
    entered: Notify,
    release: Notify,
    active: AtomicUsize,
    completed: AtomicUsize,
    dropped: AtomicUsize,
}

struct Pending {
    gate: Arc<Gate>,
    completed: bool,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.gate.active.fetch_sub(1, Ordering::SeqCst);
        if !self.completed {
            self.gate.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }
}

async fn gated(gate: &Arc<Gate>) {
    gate.active.fetch_add(1, Ordering::SeqCst);
    let mut owner = Pending {
        gate: gate.clone(),
        completed: false,
    };
    gate.entered.notify_one();
    gate.release.notified().await;
    owner.completed = true;
    gate.completed.fetch_add(1, Ordering::SeqCst);
}

struct Service {
    descriptor: ComponentDescriptor,
    start: Option<Arc<Gate>>,
    stop: Option<Arc<Gate>>,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

impl Service {
    fn new(id: &str) -> Self {
        Self {
            descriptor: descriptor(id, &[], &[]),
            start: None,
            stop: None,
            starts: Arc::default(),
            stops: Arc::default(),
        }
    }
}

#[async_trait]
impl ComputationComponent for Service {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.start {
            gated(gate).await;
        }
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        if let Some(gate) = &self.stop {
            gated(gate).await;
        }
        Ok(())
    }
}

#[async_trait]
impl ComputationService for Service {
    async fn run(&mut self) -> anyhow::Result<()> {
        pending().await
    }
}

fn selection(id: &str) -> GraphSelection {
    GraphSelection::Exact(vec![component(id)])
}

async fn independent_start_and_stop() {
    for starting in [true, false] {
        let gate = Arc::new(Gate::default());
        let mut held = Service::new("held");
        if starting {
            held.start = Some(gate.clone());
        } else {
            held.stop = Some(gate.clone());
        }
        let healthy = Service::new("healthy");
        let healthy_starts = healthy.starts.clone();
        let healthy_stops = healthy.stops.clone();
        let mut graph = ComputationGraph::builder("independent-lifecycle")
            .service(Box::new(held))
            .service(Box::new(healthy))
            .build()
            .unwrap();
        let run = graph.run().unwrap();
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            control.deployment_report().await.unwrap();
            let revision = control.desired_snapshot().revision;
            if !starting {
                control
                    .start_components(revision, selection("held"))
                    .await
                    .unwrap();
            }
            let held = async {
                if starting {
                    control
                        .start_components(revision, selection("held"))
                        .await
                        .map(|report| report.summary)
                } else {
                    control
                        .stop_components(revision, selection("held"))
                        .await
                        .map(|report| report.summary)
                }
            };
            tokio::pin!(held);
            tokio::select! {
                result = &mut held => panic!("held lifecycle completed without its gate: {result:?}"),
                _ = gate.entered.notified() => {}
            }
            for _ in 0..3 {
                assert_eq!(
                    tokio::time::timeout(
                        Duration::from_millis(500),
                        control.start_components(revision, selection("healthy"))
                    )
                    .await
                    .expect("unrelated start was blocked")
                    .unwrap()
                    .summary,
                    OperationSummary::Completed,
                );
                assert_eq!(
                    tokio::time::timeout(
                        Duration::from_millis(500),
                        control.stop_components(revision, selection("healthy"))
                    )
                    .await
                    .expect("unrelated stop was blocked")
                    .unwrap()
                    .summary,
                    OperationSummary::Completed,
                );
            }
            assert_eq!(healthy_starts.load(Ordering::SeqCst), 3);
            assert_eq!(healthy_stops.load(Ordering::SeqCst), 3);
            assert_eq!(gate.active.load(Ordering::SeqCst), 1);
            assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
            gate.release.notify_one();
            assert_eq!(held.await.unwrap(), OperationSummary::Completed);
            if starting {
                control
                    .stop_components(revision, selection("held"))
                    .await
                    .unwrap();
            }
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
    }
}

both_flavors!(disjoint_pending_start_and_stop, independent_start_and_stop);

enum Event {
    Output(OutputEnvelope),
    Panic,
    AbiPanic,
}

struct Source {
    descriptor: ComponentDescriptor,
    input: mpsc::Receiver<Event>,
    control: Arc<Mutex<Option<ComponentControl>>>,
    starts: Arc<AtomicUsize>,
}

#[async_trait]
impl ComputationComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn bind_control(&mut self, control: ComponentControl) {
        *self.control.lock().unwrap() = Some(control);
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        match self.input.recv().await {
            Some(Event::Output(output)) => Ok(Some(output)),
            Some(Event::Panic) => panic!("native processing panic"),
            Some(Event::AbiPanic) => Err(ComponentOperationError::new(
                FailureDisposition::Retryable,
                ComponentPanic::new(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "retained native ABI panic",
                )),
            )
            .into()),
            None => Ok(None),
        }
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    output: mpsc::Sender<ChangeEnvelope>,
    gate: Option<Arc<Gate>>,
    panic_control: bool,
}

struct PanickingHandler;
#[async_trait]
impl ControlHandler for PanickingHandler {
    async fn on_message(&self, message: PeerMessage, _: ComponentControl) -> anyhow::Result<()> {
        if matches!(message.notification, ControlNotification::Custom { .. }) {
            panic!("native control panic");
        }
        Ok(())
    }
}

#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        self.panic_control
            .then(|| Arc::new(PanickingHandler) as Arc<dyn ControlHandler>)
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        if let Some(gate) = &self.gate {
            gated(gate).await;
        }
        self.output.send(input.envelope).await?;
        Ok(())
    }
}

async fn panic_is_local_and_retained() {
    for (control_panic, abi_panic) in [(false, false), (false, true), (true, false)] {
        let mut builder = ComputationGraph::builder("panic-isolation");
        let gate = Arc::new(Gate::default());
        let mut inputs = Vec::new();
        let mut received = Vec::new();
        let mut controls = Vec::new();
        let mut starts = Vec::new();
        for (source, sink) in [("bad", "bad-sink"), ("good", "good-sink")] {
            let (input, events) = mpsc::channel(4);
            let (output, result) = mpsc::channel(4);
            let control = Arc::new(Mutex::new(None));
            let started = Arc::new(AtomicUsize::new(0));
            builder = builder
                .source(Box::new(Source {
                    descriptor: descriptor(source, &[], &["out"]),
                    input: events,
                    control: control.clone(),
                    starts: started.clone(),
                }))
                .sink(Box::new(Sink {
                    descriptor: descriptor(sink, &["in"], &[]),
                    output,
                    gate: (control_panic && source == "bad").then(|| gate.clone()),
                    panic_control: control_panic && source == "bad",
                }))
                .bind_stream(endpoint(source, "out"), stream(source))
                .connect(
                    edge(source, sink),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                );
            inputs.push(input);
            received.push(result);
            controls.push(control);
            starts.push(started);
        }
        let mut graph = builder.build().unwrap();
        let run = graph.run().unwrap();
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            control.deployment_report().await.unwrap();
            let revision = control.desired_snapshot().revision;
            control
                .start_components(revision, GraphSelection::All)
                .await
                .unwrap();
            let bad = if control_panic { "bad-sink" } else { "bad" };
            let generation = control.observed().components[&component(bad)].generation;
            if control_panic {
                inputs[0]
                    .send(Event::Output(output(root("bad", 1, &[7]))))
                    .await
                    .unwrap();
                gate.entered.notified().await;
                controls[0]
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .notify_neighbor(
                        &component("bad-sink"),
                        ControlNotification::Custom {
                            kind: "panic".into(),
                            payload: serde_json::Value::Null,
                        },
                    )
                    .unwrap();
            } else {
                inputs[0]
                    .send(if abi_panic {
                        Event::AbiPanic
                    } else {
                        Event::Panic
                    })
                    .await
                    .unwrap();
            }
            let mut observed = control.subscribe_observed();
            let failed = observed
                .wait_for(|state| {
                    let node = &state.components[&component(bad)];
                    node.lifecycle == ComponentLifecycle::Failed
                        && node.failure.as_ref().is_some_and(|failure| {
                            failure.disposition == FailureDisposition::Terminal
                        })
                })
                .await
                .unwrap()
                .components[&component(bad)]
                .clone();
            assert_eq!(failed.lifecycle, ComponentLifecycle::Failed);
            assert_eq!(failed.generation, generation);
            let cause = failed.failure.unwrap().cause;
            let mut chain: Option<&(dyn std::error::Error + 'static)> = Some(cause.as_ref());
            let mut marker = false;
            let mut identity = abi_panic;
            while let Some(error) = chain {
                marker |= error.is::<ComponentPanic>();
                if let Some(error) = error.downcast_ref::<ComponentUnwind>() {
                    assert_eq!(error.component, component(bad));
                    assert_eq!(error.generation, generation);
                    assert_eq!(
                        error.phase,
                        if control_panic {
                            FailurePhase::Control
                        } else {
                            FailurePhase::Processing
                        }
                    );
                    identity = true;
                }
                chain = error.source();
            }
            assert!(
                marker && identity,
                "panic identity and original typed source must survive"
            );
            control
                .stop_components(revision, selection(bad))
                .await
                .unwrap();
            let restart = control
                .start_components(revision, selection(bad))
                .await
                .unwrap();
            assert_eq!(restart.summary, OperationSummary::CompletedWithFailures);
            assert!(matches!(
                restart.components[&component(bad)],
                StartOutcome::StartFailed(_)
            ));
            assert!(control.observed().components[&component(bad)]
                .failure
                .is_some());
            assert_eq!(starts[0].load(Ordering::SeqCst), 1);
            if control_panic {
                assert_eq!(
                    gate.dropped.load(Ordering::SeqCst),
                    1,
                    "quarantine must cancel and join damaged data work"
                );
                assert_eq!(gate.active.load(Ordering::SeqCst), 0);
            }
            for sequence in 1..=3 {
                inputs[1]
                    .send(Event::Output(output(root(
                        "good",
                        sequence,
                        &[sequence as u16],
                    ))))
                    .await
                    .unwrap();
                assert_eq!(
                    values(&received[1].recv().await.unwrap()),
                    [sequence as u16]
                );
            }
            control
                .stop_components(revision, selection("good"))
                .await
                .unwrap();
            control
                .start_components(revision, selection("good"))
                .await
                .unwrap();
            inputs[1]
                .send(Event::Output(output(root("good", 4, &[44]))))
                .await
                .unwrap();
            assert_eq!(values(&received[1].recv().await.unwrap()), [44]);
            assert_eq!(control.state(), GraphState::Running);
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
        graph.shutdown().await.unwrap();
    }
}

both_flavors!(
    retained_processing_and_control_panics,
    panic_is_local_and_retained
);

async fn independent_graceful_drain() {
    let (input, events) = mpsc::channel(4);
    let (output_tx, mut output_rx) = mpsc::channel(4);
    let gate = Arc::new(Gate::default());
    let mut graph = ComputationGraph::builder("independent-drain")
        .source(Box::new(Source {
            descriptor: descriptor("source", &[], &["out"]),
            input: events,
            control: Arc::default(),
            starts: Arc::default(),
        }))
        .sink(Box::new(Sink {
            descriptor: descriptor("sink", &["in"], &[]),
            output: output_tx,
            gate: Some(gate.clone()),
            panic_control: false,
        }))
        .service(Box::new(Service::new("healthy")))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let revision = control.desired_snapshot().revision;
        control
            .start_components(revision, GraphSelection::All)
            .await
            .unwrap();
        input
            .send(Event::Output(output(root("source", 1, &[42]))))
            .await
            .unwrap();
        gate.entered.notified().await;
        let stop = control.stop_components(
            revision,
            GraphSelection::Exact(vec![component("source"), component("sink")]),
        );
        tokio::pin!(stop);
        let mut observed = control.subscribe_observed();
        tokio::select! {
            result = &mut stop => panic!("drain completed before handling: {result:?}"),
            result = observed.wait_for(|state| state.components[&component("source")].lifecycle == ComponentLifecycle::Quiesced) => { result.unwrap(); }
        }
        let independent = async {
            control
                .stop_components(revision, selection("healthy"))
                .await
                .unwrap();
            control
                .start_components(revision, selection("healthy"))
                .await
                .unwrap();
        };
        tokio::time::timeout(Duration::from_millis(500), independent)
            .await
            .expect("a drain blocked unrelated lifecycle");
        assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
        assert_eq!(gate.active.load(Ordering::SeqCst), 1);
        gate.release.notify_one();
        assert_eq!(values(&output_rx.recv().await.unwrap()), [42]);
        assert_eq!(stop.await.unwrap().summary, OperationSummary::Completed);
        assert_eq!(gate.completed.load(Ordering::SeqCst), 1);
        assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

both_flavors!(
    disjoint_control_during_graceful_drain,
    independent_graceful_drain
);

async fn scoped_force_escalates_without_reusing_an_uncertain_generation() {
    scoped_force_with_companion(false).await;
}

async fn scoped_force_joins_idle_peer_control() {
    scoped_force_with_companion(true).await;
}

async fn scoped_force_with_companion(idle_companion: bool) {
    let (input, events) = mpsc::channel(4);
    let (output_tx, mut output_rx) = mpsc::channel(4);
    let gate = Arc::new(Gate::default());
    let shared = Arc::new(7usize);
    let resource = || {
        ResourceHandle::new(ResourceRole::StateStore, Arc::new(0usize))
            .with_shared_identity(shared.clone())
    };
    let healthy = Service::new("healthy");
    let healthy_starts = healthy.starts.clone();
    let mut graph = ComputationGraph::builder("scoped-force")
        .source(Box::new(Source {
            descriptor: descriptor("source", &[], &["out"]),
            input: events,
            control: Arc::default(),
            starts: Arc::default(),
        }))
        .sink(Box::new(Sink {
            descriptor: descriptor("sink", &["in"], &[]),
            output: output_tx,
            gate: Some(gate.clone()),
            panic_control: false,
        }))
        .service(Box::new(Service::new("companion")))
        .service(Box::new(healthy))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let revision = control.desired_snapshot().revision;
        control
            .start_components(
                revision,
                if idle_companion {
                    GraphSelection::Exact(vec![
                        component("source"),
                        component("sink"),
                        component("healthy"),
                    ])
                } else {
                    GraphSelection::All
                },
            )
            .await
            .unwrap();
        for id in ["sink", "companion"] {
            let id = component(id);
            control
                .observe_resources(
                    &id,
                    control.observed().components[&id].generation,
                    vec![(
                        ResourceSpecification {
                            id: ResourceId::try_new("provider").unwrap(),
                            role: ResourceRole::StateStore,
                            ownership: ResourceOwnership::Borrowed,
                            binding: Arc::from("provider"),
                        },
                        resource(),
                    )],
                )
                .unwrap();
        }
        let mut observed = control.subscribe_observed();
        observed
            .wait_for(|state| state.revision.0 >= revision.0 + 2)
            .await
            .unwrap();
        let revision = control.desired_snapshot().revision;
        input
            .send(Event::Output(output(root("source", 1, &[42]))))
            .await
            .unwrap();
        gate.entered.notified().await;
        let rejected = control
            .force_cancel_components(revision, selection("sink"))
            .await
            .unwrap_err();
        assert!(matches!(rejected, GraphError::Validation { ref source, .. }
            if source.downcast_ref::<ForceCancellationScopeError>().is_some_and(|scope|
                scope.required.contains(&component("companion")))));
        assert_eq!(gate.active.load(Ordering::SeqCst), 1);
        assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
        let graceful = control.stop_components(revision, selection("sink"));
        tokio::pin!(graceful);
        tokio::select! {
            result = &mut graceful => panic!("handling was not drained: {result:?}"),
            result = observed.wait_for(|state|
                state.components[&component("sink")].lifecycle == ComponentLifecycle::Quiescing) => { result.unwrap(); }
        }
        let forced = tokio::time::timeout(
            Duration::from_secs(1),
            control.force_cancel_components(
                revision,
                GraphSelection::Exact(vec![component("sink"), component("companion")]),
            ),
        )
        .await
        .expect("force cancellation is bounded")
        .unwrap();
        assert_eq!(forced.summary, OperationSummary::CompletedWithFailures);
        assert_eq!(forced.cleanup.summary, OperationSummary::Completed);
        assert_eq!(forced.interrupted.len(), if idle_companion { 1 } else { 2 });
        assert_eq!(
            forced.interrupted.contains_key(&component("companion")),
            !idle_companion
        );
        if idle_companion {
            assert!(matches!(
                forced.cleanup.components[&component("companion")],
                StopOutcome::AlreadyStopped
            ));
            assert_eq!(
                control
                    .start_components(revision, selection("companion"))
                    .await
                    .unwrap()
                    .summary,
                OperationSummary::Completed
            );
            assert_eq!(
                control
                    .stop_components(revision, selection("companion"))
                    .await
                    .unwrap()
                    .summary,
                OperationSummary::Completed
            );
        }
        assert!(
            graceful.await.is_err(),
            "forced handling must not masquerade as a completed drain"
        );
        assert_eq!(gate.active.load(Ordering::SeqCst), 0);
        assert_eq!(gate.dropped.load(Ordering::SeqCst), 1);
        assert_eq!(gate.completed.load(Ordering::SeqCst), 0);
        assert!(output_rx.try_recv().is_err());
        let restart = control
            .start_components(revision, selection("sink"))
            .await
            .unwrap();
        assert_eq!(restart.summary, OperationSummary::CompletedWithFailures);
        for _ in 0..2 {
            control
                .stop_components(revision, selection("healthy"))
                .await
                .unwrap();
            control
                .start_components(revision, selection("healthy"))
                .await
                .unwrap();
        }
        assert_eq!(healthy_starts.load(Ordering::SeqCst), 3);
        assert_eq!(
            control.observed().components[&component("healthy")].lifecycle,
            ComponentLifecycle::Running
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
    graph.dispose().await.unwrap();
}

both_flavors!(
    scoped_force_cancellation,
    scoped_force_escalates_without_reusing_an_uncertain_generation
);

both_flavors!(
    force_joins_idle_peer_control,
    scoped_force_joins_idle_peer_control
);

async fn reverse_registration_empty_pipeline_drains() {
    let (_input, events) = mpsc::channel(1);
    let (output, _received) = mpsc::channel(1);
    let mut graph = ComputationGraph::builder("reverse-registration")
        .sink(Box::new(Sink {
            descriptor: descriptor("sink", &["in"], &[]),
            output,
            gate: None,
            panic_control: false,
        }))
        .source(Box::new(Source {
            descriptor: descriptor("source", &[], &["out"]),
            input: events,
            control: Arc::default(),
            starts: Arc::default(),
        }))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    assert_eq!(
        graph.snapshot().nodes[0].descriptor.id(),
        &component("sink")
    );
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let revision = control.desired_snapshot().revision;
        control
            .start_components(revision, GraphSelection::All)
            .await
            .unwrap();
        control
            .quiesce_components(revision, GraphSelection::All)
            .await
            .unwrap();
        assert!(control
            .observed()
            .components
            .values()
            .all(|node| node.lifecycle == ComponentLifecycle::Quiesced));
        control
            .start_components(revision, GraphSelection::All)
            .await
            .unwrap();
        assert_eq!(
            control
                .stop_components(revision, GraphSelection::All)
                .await
                .unwrap()
                .summary,
            OperationSummary::Completed,
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

both_flavors!(
    reverse_registration_quiescence,
    reverse_registration_empty_pipeline_drains
);

async fn dropped_cleanup_keeps_the_actual_stop_task() {
    let gate = Arc::new(Gate::default());
    let mut service = Service::new("service");
    service.stop = Some(gate.clone());
    let starts = service.starts.clone();
    let stops = service.stops.clone();
    let mut graph = ComputationGraph::builder("retained-cleanup")
        .service(Box::new(service))
        .build()
        .unwrap();
    let mut run = Box::pin(graph.start().unwrap());
    while starts.load(Ordering::SeqCst) != 1 {
        tokio::select! {
            result = &mut run => panic!("scope ended before startup: {result:?}"),
            _ = tokio::task::yield_now() => {}
        }
    }
    drop(run);
    assert_eq!(graph.state(), GraphState::CleanupRequired);
    {
        let mut cleanup = Box::pin(graph.shutdown());
        tokio::select! {
            result = &mut cleanup => panic!("cleanup ended before its stop gate: {result:?}"),
            _ = gate.entered.notified() => {}
        }
    }
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(gate.active.load(Ordering::SeqCst), 1);
    assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
    assert!(matches!(
        graph.start(),
        Err(GraphError::InvalidState { .. })
    ));
    gate.release.notify_one();
    graph.shutdown().await.unwrap();
    assert_eq!(
        stops.load(Ordering::SeqCst),
        1,
        "retry must join the old stop, not recreate it"
    );
    assert_eq!(gate.active.load(Ordering::SeqCst), 0);
    assert_eq!(gate.completed.load(Ordering::SeqCst), 1);
    assert_eq!(gate.dropped.load(Ordering::SeqCst), 0);
}

both_flavors!(
    cancelled_cleanup_retains_join_and_instance,
    dropped_cleanup_keeps_the_actual_stop_task
);

async fn slow_cleanup_does_not_starve_independent_owners() {
    let gate = Arc::new(Gate::default());
    let mut held = Service::new("held");
    held.stop = Some(gate.clone());
    let healthy = Service::new("healthy");
    let healthy_stops = healthy.stops.clone();
    let mut graph = ComputationGraph::builder("fair-cleanup")
        .cleanup_timeout(Duration::from_millis(30))
        .service(Box::new(held))
        .service(Box::new(healthy))
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        control
            .start_components(control.desired_snapshot().revision, GraphSelection::All)
            .await
            .unwrap();
        control.cancel();
        gate.entered.notified().await;
        tokio::time::timeout(Duration::from_millis(20), async {
            while healthy_stops.load(Ordering::SeqCst) != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("independent cleanup waited behind a slow owner");
        assert_eq!(gate.completed.load(Ordering::SeqCst), 0);
    });
    assert!(
        matches!(result, Err(GraphError::Cleanup { .. })),
        "{result:?}"
    );
    assert_eq!(gate.active.load(Ordering::SeqCst), 0);
    assert_eq!(gate.dropped.load(Ordering::SeqCst), 1);
    gate.release.notify_one();
    graph.shutdown().await.unwrap();
    assert_eq!(healthy_stops.load(Ordering::SeqCst), 1);
}

both_flavors!(
    independent_cleanup_gets_its_budget,
    slow_cleanup_does_not_starve_independent_owners
);

struct DestructorService {
    descriptor: ComponentDescriptor,
    panic_bind: bool,
    panic_drop: bool,
    stops: Arc<AtomicUsize>,
}
impl Drop for DestructorService {
    fn drop(&mut self) {
        assert!(!self.panic_drop, "native destructor panic");
    }
}
#[async_trait]
impl ComputationComponent for DestructorService {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn bind_control(&mut self, _: ComponentControl) {
        assert!(!self.panic_bind, "synchronous bind panic");
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[async_trait]
impl ComputationService for DestructorService {
    async fn run(&mut self) -> anyhow::Result<()> {
        pending().await
    }
}

async fn synchronous_callbacks_and_retirement_are_contained() {
    let stops = Arc::new(AtomicUsize::new(0));
    let mut graph = ComputationGraph::builder("synchronous-faults")
        .service(Box::new(DestructorService {
            descriptor: descriptor("bad", &[], &[]),
            panic_bind: true,
            panic_drop: true,
            stops: stops.clone(),
        }))
        .service(Box::new(Service::new("healthy")))
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let report = control
            .start_components(control.desired_snapshot().revision, GraphSelection::All)
            .await
            .unwrap();
        assert!(matches!(
            report.components[&component("bad")],
            StartOutcome::StartFailed(_)
        ));
        assert_eq!(
            control.observed().components[&component("healthy")].lifecycle,
            ComponentLifecycle::Running
        );
        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::RemoveComponents {
                    selection: selection("bad"),
                    policy: RemovalPolicy::Reject,
                }],
            )
            .await
            .unwrap();
        let removed = control
            .reconcile(preview, TopologyBindings::default())
            .await
            .unwrap();
        assert!(
            removed.committed,
            "destructor failure must not falsely report an uncommitted topology"
        );
        assert!(removed.removed.contains(&component("bad")));
        assert_eq!(
            stops.load(Ordering::SeqCst),
            1,
            "a binding panic must not bypass owned cleanup"
        );
        assert_eq!(removed.summary, OperationSummary::CompletedWithFailures);
        assert!(removed
            .failures
            .iter()
            .any(|failure| failure.phase == FailurePhase::Removal));
        let revision = control.desired_snapshot().revision;
        control
            .stop_components(revision, selection("healthy"))
            .await
            .unwrap();
        control
            .start_components(revision, selection("healthy"))
            .await
            .unwrap();
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)), "{result:?}");
}

both_flavors!(
    synchronous_callback_and_destructor_faults,
    synchronous_callbacks_and_retirement_are_contained
);

#[test]
fn native_cpu_work_is_parallel_with_external_watchdog() {
    let output = tempfile::NamedTempFile::new().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "native_cpu_parallel_probe", "--nocapture"])
        .env("DRASI_CPU_PARALLEL_PROBE", "1")
        .stdout(output.as_file().try_clone().unwrap())
        .stderr(output.as_file().try_clone().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "CPU parallel probe failed: {status}\n{}",
                std::fs::read_to_string(output.path()).unwrap()
            );
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!(
                "native CPU work was serialized; external watchdog terminated the probe\n{}",
                std::fs::read_to_string(output.path()).unwrap()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
#[ignore = "subprocess helper for native_cpu_work_is_parallel_with_external_watchdog"]
fn native_cpu_parallel_probe() {
    assert!(
        std::env::var_os("DRASI_CPU_PARALLEL_PROBE").is_some(),
        "launch this probe through its external watchdog"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let entered = Arc::new(AtomicUsize::new(0));
        let overlap = Arc::new(AtomicBool::new(false));
        let threads = Arc::new(Mutex::new(Vec::new()));
        let received: Received = Arc::default();
        let mut builder = ComputationGraph::builder("cpu-parallel");
        for (source, transform, sink, value) in [("a", "ta", "sa", 7), ("b", "tb", "sb", 9)] {
            let entered = entered.clone();
            let overlap = overlap.clone();
            let threads = threads.clone();
            builder = builder
                .source(Box::new(FiniteSource::new(
                    source,
                    vec![output(root(source, 1, &[value]))],
                )))
                .transformer(Box::new(NativeTransform::new(transform, move |input| {
                    threads.lock().unwrap().push(std::thread::current().id());
                    entered.fetch_add(1, Ordering::SeqCst);
                    // Intentionally synchronous CPU work: only an external process
                    // watchdog can diagnose a shared single-task scheduler here.
                    while entered.load(Ordering::SeqCst) != 2 {
                        std::hint::spin_loop();
                    }
                    overlap.store(true, Ordering::SeqCst);
                    let mut checksum = values(&input.envelope)[0] as u64;
                    for offset in 0..2_000_000u64 {
                        checksum =
                            std::hint::black_box(checksum.wrapping_mul(31).wrapping_add(offset));
                    }
                    std::hint::black_box(checksum);
                    Ok(vec![output(derived(
                        &input.envelope,
                        transform,
                        1,
                        changes(transform, 1, &[value + 1]),
                    ))])
                })))
                .sink(Box::new(CollectSink::new(sink, &["in"], received.clone())))
                .bind_stream(endpoint(source, "out"), stream(source))
                .bind_stream(endpoint(transform, "out"), stream(transform))
                .connect(
                    edge(source, transform),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .connect(
                    edge(transform, sink),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                );
        }
        let mut graph = builder.build().unwrap();
        graph.start().unwrap().await.unwrap();
        assert!(overlap.load(Ordering::SeqCst));
        let threads = threads.lock().unwrap();
        assert_eq!(threads.len(), 2);
        assert_ne!(threads[0], threads[1]);
        let mut actual: Vec<_> = received
            .lock()
            .unwrap()
            .iter()
            .flat_map(|input| values(&input.envelope))
            .collect();
        actual.sort();
        assert_eq!(actual, [8, 10]);
    });
}
