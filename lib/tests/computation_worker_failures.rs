// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! A component's owned worker fails while no operation of the component is in
//! flight (an idle sink): the graph observes it through
//! `ComputationComponent::worker_failures`, fails the node promptly with a
//! retryable `Processing` failure and withdraws its readiness; the next
//! activation starts healthy.

#![cfg(test)]

#[path = "support/mod.rs"]
mod fixture_support;
use fixture_support::FixtureGraphBuilderExt as _;

mod computation_support;

use anyhow::Result;
use async_trait::async_trait;
use computation_support::*;
use drasi_lib::computation::v1::*;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, watch};

const SOURCE: &str = "00000000-0000-0000-0000-0000000006a1";
const SINK: &str = "00000000-0000-0000-0000-0000000006a2";
const BOUND: Duration = Duration::from_secs(10);

fn id(value: &str) -> drasi_core_types::ComponentId {
    drasi_core_types::ComponentId::try_new(value).expect("fixture UUID")
}

struct ChannelSource {
    descriptor: ComponentDescriptor,
    input: mpsc::Receiver<u64>,
    peers: mpsc::UnboundedSender<ControlNotification>,
}

struct Peers(mpsc::UnboundedSender<ControlNotification>);
#[async_trait]
impl ControlHandler for Peers {
    async fn on_message(&self, message: PeerMessage, _: ComponentControl) -> Result<()> {
        let _ = self.0.send(message.notification);
        Ok(())
    }
}

#[async_trait]
impl ComputationComponent for ChannelSource {
    fn description(&self) -> ComponentDescription {
        ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        Some(Arc::new(Peers(self.peers.clone())))
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSource for ChannelSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self
            .input
            .recv()
            .await
            .map(|sequence| output(root(&id(SOURCE), "source", sequence, &[sequence as u16]))))
    }
}

/// The activation's worker state: `Some(reason)` once the worker failed.
struct Worker(watch::Sender<Option<String>>);

#[async_trait]
impl WorkerFailures for Worker {
    async fn wait(&self) -> anyhow::Error {
        let mut failure = self.0.subscribe();
        let reason = failure
            .wait_for(Option::is_some)
            .await
            .map(|reason| reason.clone().unwrap_or_default());
        match reason {
            Ok(reason) => anyhow::anyhow!(reason),
            Err(_) => std::future::pending().await,
        }
    }
}

struct WorkerSink {
    descriptor: ComponentDescriptor,
    worker: Arc<Worker>,
    delivered: Arc<Mutex<Vec<u64>>>,
}

#[async_trait]
impl ComputationComponent for WorkerSink {
    fn description(&self) -> ComponentDescription {
        ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn worker_failures(&self) -> Option<Arc<dyn WorkerFailures>> {
        Some(self.worker.clone())
    }
    drasi_lib::passive_component_lifecycle!(@hook initialize, InitializeContext);
    drasi_lib::passive_component_lifecycle!(@hook pause, PauseContext);
    drasi_lib::passive_component_lifecycle!(@hook resume, ResumeContext);
    drasi_lib::passive_component_lifecycle!(@hook stop, StopContext);
    async fn dispose(&mut self, context: &DisposeContext) -> Result<()> {
        anyhow::ensure!(
            !context.erases_owned_state(),
            "this component does not support permanent state removal"
        );
        Ok(())
    }
    /// A new activation owns a new, healthy worker.
    async fn start(&mut self, _: &StartContext) -> Result<()> {
        self.worker.0.send_replace(None);
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSink for WorkerSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.delivered
            .lock()
            .unwrap()
            .push(input.envelope.system().sequence());
        Ok(())
    }
}

async fn observe(
    control: &GraphControl,
    describe: &str,
    accept: impl Fn(&ObservedComponent) -> bool,
) -> ObservedComponent {
    let mut observed = control.subscribe_observed();
    let graph = tokio::time::timeout(
        BOUND,
        observed.wait_for(|graph| graph.components.get(&id(SINK)).is_some_and(&accept)),
    )
    .await
    .unwrap_or_else(|_| panic!("sink did not become {describe}"))
    .expect("observed graph")
    .clone();
    graph.components[&id(SINK)].clone()
}

async fn notification(
    peers: &mut mpsc::UnboundedReceiver<ControlNotification>,
    wanted: ControlNotification,
) {
    tokio::time::timeout(BOUND, async {
        while let Some(notification) = peers.recv().await {
            if notification == wanted {
                return;
            }
        }
        panic!("peer notifications ended");
    })
    .await
    .unwrap_or_else(|_| panic!("no {wanted:?} notification"));
}

async fn idle_worker_failure_fails_the_node() {
    let (input, receiver) = mpsc::channel(4);
    let (peers, mut notifications) = mpsc::unbounded_channel();
    let worker = Arc::new(Worker(watch::channel(None).0));
    let delivered = Arc::new(Mutex::new(Vec::new()));
    let mut graph = ComputationGraph::builder("worker-failures")
        .fixture_source(Box::new(ChannelSource {
            descriptor: descriptor(&id(SOURCE), &[], &["out"]),
            input: receiver,
            peers,
        }))
        .fixture_sink(Box::new(WorkerSink {
            descriptor: descriptor(&id(SINK), &["in"], &[]),
            worker: worker.clone(),
            delivered: delivered.clone(),
        }))
        .bind_stream(endpoint(&id(SOURCE), "out"), stream("source"))
        .connect(
            edge(&id(SOURCE), &id(SINK)),
            Box::new(BoundedPipeConfig { capacity: 4 }),
        )
        .build()
        .expect("graph");
    let run = graph.start().expect("start");
    let control = run.control();
    let (outcome, ()) = tokio::join!(run, async {
        control.startup_report().await.expect("startup");
        let first = observe(&control, "running", |node| {
            node.lifecycle == ComponentLifecycle::Running
        })
        .await;
        notification(&mut notifications, ControlNotification::Ready).await;
        input.send(1).await.unwrap();

        // The sink is idle: no handle is in flight when its worker fails.
        worker.0.send_replace(Some("listener worker failed".into()));
        let failed = observe(&control, "failed", |node| {
            node.lifecycle == ComponentLifecycle::Failed
        })
        .await;
        let failure = failed.failure.as_ref().expect("failure");
        assert_eq!(failure.phase, FailurePhase::Processing);
        assert_eq!(failure.disposition, FailureDisposition::Retryable);
        assert!(format!("{:#}", failure.cause).contains("listener worker failed"));
        assert_eq!(failed.health, ComponentHealth::Unavailable);
        assert_eq!(failed.generation, first.generation);
        notification(&mut notifications, ControlNotification::NotReady).await;

        // The failure belongs to the activation: Stop then Start recovers.
        let revision = control.desired_snapshot().revision;
        control
            .stop_components(revision, GraphSelection::Exact(vec![id(SINK)]))
            .await
            .expect("stop");
        control
            .start_components(revision, GraphSelection::Exact(vec![id(SINK)]))
            .await
            .expect("restart");
        let restarted = observe(&control, "running again", |node| {
            node.lifecycle == ComponentLifecycle::Running && node.failure.is_none()
        })
        .await;
        assert_eq!(restarted.generation, first.generation);
        input.send(2).await.unwrap();
        tokio::time::timeout(BOUND, async {
            while !delivered.lock().unwrap().contains(&2) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("delivery after restart");
        control.cancel();
    });
    assert!(matches!(outcome, Err(GraphError::Cancelled)), "{outcome:?}");
    graph.dispose().await.expect("cleanup");
}

#[tokio::test(flavor = "current_thread")]
async fn idle_worker_failure_fails_the_node_current_thread() {
    idle_worker_failure_fails_the_node().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn idle_worker_failure_fails_the_node_multi_thread() {
    idle_worker_failure_fails_the_node().await;
}
