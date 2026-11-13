// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]

#[path = "support/mod.rs"]
mod fixture_support;
use fixture_support::FixtureGraphBuilderExt as _;

mod computation_support;

use async_trait::async_trait;
use computation_support::*;
use drasi_core_types::ComponentName;
use drasi_lib::{computation::v1::*, DrasiLib};
use std::{
    future::Future,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Notify};

#[derive(Debug, thiserror::Error)]
#[error("invalid native component state")]
struct TerminalFailure;

enum Event {
    Data(OutputEnvelope),
    Fail,
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
    fn configuration(&self) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({"retained":true}))
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
            Some(Event::Data(value)) => Ok(Some(value)),
            Some(Event::Fail) => Err(TerminalFailure.into()),
            None => Ok(None),
        }
    }
}
struct Handler {
    calls: AtomicUsize,
    handled: Notify,
}
#[async_trait]
impl ControlHandler for Handler {
    async fn on_message(&self, message: PeerMessage, _: ComponentControl) -> anyhow::Result<()> {
        if matches!(message.notification, ControlNotification::Custom { .. }) {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                return Err(TerminalFailure.into());
            }
            self.handled.notify_one();
        }
        Ok(())
    }
}
struct Sink {
    descriptor: ComponentDescriptor,
    output: mpsc::Sender<ChangeEnvelope>,
    handler: Arc<Handler>,
}
#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        Some(self.handler.clone())
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
        self.output.send(input.envelope).await?;
        Ok(())
    }
}

async fn step<T>(phase: &str, work: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(2), work)
        .await
        .unwrap_or_else(|_| panic!("native failure scenario stalled during {phase}"))
}

async fn scenario(invalid_emission: bool) {
    let mut builder = ComponentBatch::builder();
    let mut inputs = Vec::new();
    let mut outputs = Vec::new();
    let mut senders = Vec::new();
    let starts = Arc::new(AtomicUsize::new(0));
    let handler = Arc::new(Handler {
        calls: AtomicUsize::new(0),
        handled: Notify::new(),
    });
    for (name, name_id, target, target_id) in [
        (
            "bad",
            fixture_support::component_id(453),
            "bad-sink",
            fixture_support::component_id(2),
        ),
        (
            "good",
            fixture_support::component_id(463),
            "good-sink",
            fixture_support::component_id(727),
        ),
    ] {
        let (input, receiver) = mpsc::channel(8);
        let (output, received) = mpsc::channel(8);
        let sender = Arc::new(Mutex::new(None));
        builder = builder
            .source(
                ComponentName::try_new(name).unwrap(),
                Box::new(Source {
                    descriptor: descriptor(&name_id, &[], &["out"]),
                    input: receiver,
                    control: sender.clone(),
                    starts: starts.clone(),
                }),
            )
            .sink(
                ComponentName::try_new(target).unwrap(),
                Box::new(Sink {
                    descriptor: descriptor(&target_id, &["in"], &[]),
                    output,
                    handler: handler.clone(),
                }),
            )
            .bind_stream(endpoint(&name_id, "out"), stream(name))
            .connect(
                edge(&name_id, &target_id),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            );
        inputs.push(input);
        outputs.push(received);
        senders.push(sender);
    }
    let drasi = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(
            drasi_lib::computation::v1::FactoryRegistry::standard()
                .get(
                    &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_components(builder.build().unwrap())
        .build()
        .await
        .unwrap();
    step("initial start", drasi.start()).await.unwrap();
    let control = drasi.computation_control().unwrap();
    inputs[1]
        .send(Event::Data(output(root(
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001cf")
                .expect("fixture UUID"),
            "good",
            1,
            &[10],
        ))))
        .await
        .unwrap();
    let first = tokio::time::timeout(Duration::from_secs(2), outputs[1].recv())
        .await
        .unwrap_or_else(|_| {
            panic!(
                "initial healthy output stalled (invalid_emission={invalid_emission}): {:?}",
                control.observed()
            )
        })
        .unwrap();
    assert_eq!(values(&first), [10]);
    let generation = control.observed().components[&component(453)].generation;
    if invalid_emission {
        inputs[0]
            .send(Event::Data(OutputEnvelope {
                port: port("undeclared"),
                envelope: root(
                    &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001c5")
                        .expect("fixture UUID"),
                    "bad",
                    1,
                    &[1],
                ),
            }))
            .await
            .unwrap();
    } else {
        inputs[0].send(Event::Fail).await.unwrap();
    }
    let mut observed = control.subscribe_observed();
    step(
        "source failure publication",
        observed.wait_for(|state| state.components[&component(453)].failure.is_some()),
    )
    .await
    .unwrap();
    let state = control.observed();
    let failed = &state.components[&component(453)];
    assert_eq!(failed.lifecycle, ComponentLifecycle::Failed);
    assert_eq!(failed.generation, generation);
    assert_eq!(
        failed.failure.as_ref().unwrap().disposition,
        FailureDisposition::Terminal
    );
    if !invalid_emission {
        assert!(
            std::error::Error::source(failed.failure.as_ref().unwrap().cause.as_ref())
                .unwrap()
                .is::<TerminalFailure>()
        );
    }
    assert_eq!(control.state(), GraphState::Running);
    assert!(control
        .desired_snapshot()
        .nodes
        .iter()
        .any(|node| node.descriptor.id() == &component(453)));
    inputs[1]
        .send(Event::Data(output(root(
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001cf")
                .expect("fixture UUID"),
            "good",
            2,
            &[20],
        ))))
        .await
        .unwrap();
    assert_eq!(
        values(
            &step("healthy output after sibling failure", outputs[1].recv())
                .await
                .unwrap()
        ),
        [20]
    );
    assert_eq!(
        starts.load(Ordering::SeqCst),
        2,
        "no implicit retry of terminal work"
    );

    let peer = senders[1].lock().unwrap().clone().unwrap();
    peer.notify_neighbor(
        &component(727),
        ControlNotification::Custom {
            kind: "first".into(),
            payload: serde_json::Value::Null,
        },
    )
    .unwrap();
    step(
        "control failure publication",
        observed.wait_for(|state| {
            state.components[&component(727)]
                .failure
                .as_ref()
                .is_some_and(|failure| failure.phase == FailurePhase::Control)
        }),
    )
    .await
    .unwrap();
    peer.notify_neighbor(
        &component(727),
        ControlNotification::Custom {
            kind: "second".into(),
            payload: serde_json::Value::Null,
        },
    )
    .unwrap();
    step("control handler retry", handler.handled.notified()).await;
    assert_eq!(
        handler.calls.load(Ordering::SeqCst),
        2,
        "one rejected notification cannot close the inbox"
    );

    let handle = drasi
        .component_by_name(
            &drasi_core_types::ComponentName::try_new("bad").expect("fixture component name"),
        )
        .unwrap();
    step("failed source stop", handle.stop()).await.unwrap();
    step("failed source restart", handle.start()).await.unwrap();
    inputs[0]
        .send(Event::Data(output(root(
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001c5")
                .expect("fixture UUID"),
            "bad",
            2,
            &[42],
        ))))
        .await
        .unwrap();
    assert_eq!(
        values(
            &step("restarted source output", outputs[0].recv())
                .await
                .unwrap()
        ),
        [42]
    );
    assert_eq!(
        control.observed().components[&component(453)].generation,
        generation
    );
    step("instance stop", drasi.stop()).await.unwrap();
    step("instance shutdown", drasi.shutdown()).await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn native_errors_keep_nodes_controller_siblings_and_control_requests_usable_current_thread() {
    for invalid in [false, true] {
        tokio::time::timeout(Duration::from_secs(10), scenario(invalid))
            .await
            .unwrap();
    }
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_errors_keep_nodes_controller_siblings_and_control_requests_usable_multi_thread() {
    for invalid in [false, true] {
        tokio::time::timeout(Duration::from_secs(10), scenario(invalid))
            .await
            .unwrap();
    }
}
