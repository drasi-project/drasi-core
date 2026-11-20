// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! RCA-8: a panic inside an owned worker (`spawn_owned_worker`), not inside an
//! operation future, of a loaded native component fails the node promptly,
//! withdraws its readiness, and Retry restarts it cleanly, on both Tokio
//! flavours. Run `make build-test-plugins` before this target.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_host_sdk::computation::{self, NativeFactory, NativePlugin};
use drasi_lib::computation::v1::*;
use serde_json::json;
use tokio::sync::mpsc;

#[path = "fixtures/native_worker_failure.rs"]
mod fixture;

/// Generous bound for a prompt failure; the worker panics immediately.
const BOUND: Duration = Duration::from_secs(10);

fn source_id() -> ComponentId {
    ComponentId::try_new("6f1d3a52-6f0e-5b1e-9a51-8c2b9b4b7c11").expect("fixture UUID")
}

fn sink_id() -> ComponentId {
    ComponentId::try_new("0b8f8d39-2d3e-5c61-8f2e-1b6a4c9d2e77").expect("fixture UUID")
}

fn stream() -> StreamId {
    StreamId::try_new("source/out").expect("fixture stream")
}

fn library() -> Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let debug = executable
        .parent()
        .and_then(Path::parent)
        .context("test target directory")?;
    let directory = std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| debug.join("plugins"));
    let library = directory.join("examples").join(format!(
        "{}native_worker_failure{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX,
    ));
    anyhow::ensure!(
        library.is_file(),
        "missing worker-failure fixture {}; first run make build-test-plugins",
        library.display()
    );
    Ok(library)
}

fn factory(loaded: bool) -> Result<Arc<NativeFactory>> {
    let plugin = if loaded {
        computation::load(library()?)?
    } else {
        NativePlugin::from_definition(fixture::plugin()?)?
    };
    plugin
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == fixture::IMPLEMENTATION)
        .cloned()
        .context("missing worker-failure factory")
}

#[derive(Debug, PartialEq, Eq)]
enum Peer {
    Ready,
    NotReady,
    Handled(u64),
}

/// The sink's upstream peer: emits on request and records what the sink tells it.
struct Upstream {
    descriptor: ComponentDescriptor,
    inputs: mpsc::Receiver<u64>,
    peers: mpsc::UnboundedSender<Peer>,
}

struct Observer(mpsc::UnboundedSender<Peer>);
#[async_trait]
impl ControlHandler for Observer {
    async fn on_message(&self, message: PeerMessage, _: ComponentControl) -> Result<()> {
        let event = match message.notification {
            ControlNotification::Ready => Peer::Ready,
            ControlNotification::NotReady => Peer::NotReady,
            ControlNotification::Custom { kind, payload } if kind == "fixture.handled" => {
                Peer::Handled(payload.as_u64().context("handled sequence")?)
            }
            _ => return Ok(()),
        };
        let _ = self.0.send(event);
        Ok(())
    }
}

#[async_trait]
impl ComputationComponent for Upstream {
    fn description(&self) -> ComponentDescription {
        ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn control_handler(&self) -> Option<Arc<dyn ControlHandler>> {
        Some(Arc::new(Observer(self.peers.clone())))
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSource for Upstream {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let Some(sequence) = self.inputs.recv().await else {
            return Ok(None);
        };
        let envelope = GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", &sequence.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: 0,
                    },
                    properties: ElementPropertyMap::from(json!({"value": sequence})),
                },
            },
            stream(),
            sequence,
            None,
        )?;
        Ok(Some(OutputEnvelope { execution_context: Default::default(),
            port: PortId::try_new("out")?,
            envelope,
        }))
    }
}

async fn next_peer(
    peers: &mut mpsc::UnboundedReceiver<Peer>,
    wanted: impl Fn(&Peer) -> bool,
) -> Result<Peer> {
    tokio::time::timeout(BOUND, async {
        loop {
            let peer = peers.recv().await.context("upstream observer closed")?;
            if wanted(&peer) {
                return Ok(peer);
            }
        }
    })
    .await
    .context("peer notification did not arrive within the bound")?
}

async fn observe(
    control: &GraphControl,
    describe: &str,
    predicate: impl Fn(&ObservedComponent) -> bool,
) -> Result<ObservedComponent> {
    let mut changes = control.subscribe_observed();
    let id = sink_id();
    let state = tokio::time::timeout(
        BOUND,
        changes.wait_for(|state| state.components.get(&id).is_some_and(&predicate)),
    )
    .await
    .with_context(|| {
        format!(
            "sink did not become {describe} within {BOUND:?}: {:?}",
            control.observed().components.get(&id)
        )
    })??;
    Ok(state.components[&id].clone())
}

async fn worker_panic_fails_node_and_retry_restarts(loaded: bool) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let effects = directory.path().join("effects.txt");
    let factory = factory(loaded)?;
    let specification =
        factory.specification(sink_id(), json!({"path": effects, "panic_at": 2}))?;
    let (send, inputs) = mpsc::channel(4);
    let (peers, mut observed_peers) = mpsc::unbounded_channel();
    let upstream = Upstream {
        descriptor: ComponentDescriptor::try_new(
            source_id(),
            vec![PortDescriptor::new(
                PortId::try_new("out")?,
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?,
        inputs,
        peers,
    };
    let mut graph = ComputationGraph::builder("worker-failure")
        .source(ComponentName::try_new("upstream")?, Box::new(upstream))
        .component(ComponentName::try_new("sink")?, specification, factory)
        .bind_stream(
            Endpoint::new(source_id(), PortId::try_new("out")?),
            stream(),
        )
        .connect(
            EdgeDefinition::new(
                Endpoint::new(source_id(), PortId::try_new("out")?),
                Endpoint::new(sink_id(), PortId::try_new("in")?),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()?;
    let run = graph.run()?;
    let control = run.control();
    let scenario = async {
        let deployment = control.deployment_report().await?;
        anyhow::ensure!(
            deployment.summary == OperationSummary::Completed,
            "{deployment:?}"
        );
        control
            .start_components(deployment.revision, GraphSelection::All)
            .await?;
        let first = observe(&control, "running", |node| {
            node.lifecycle == ComponentLifecycle::Running
        })
        .await?;
        next_peer(&mut observed_peers, |peer| *peer == Peer::Ready).await?;

        send.send(1).await?;
        assert_eq!(
            next_peer(&mut observed_peers, |peer| matches!(peer, Peer::Handled(_))).await?,
            Peer::Handled(1)
        );

        // The worker panics after `handle` returned: no operation is in flight.
        send.send(2).await?;
        assert_eq!(
            next_peer(&mut observed_peers, |peer| matches!(peer, Peer::Handled(_))).await?,
            Peer::Handled(2)
        );
        let failed = observe(&control, "failed", |node| {
            node.lifecycle == ComponentLifecycle::Failed
        })
        .await?;
        let failure = failed
            .failure
            .as_ref()
            .context("failed node carries its failure")?;
        assert_eq!(
            failure.disposition,
            FailureDisposition::Retryable,
            "{failure:?}"
        );
        assert_eq!(failure.phase, FailurePhase::Processing, "{failure:?}");
        let cause = format!("{:#}", failure.cause);
        assert!(
            cause.contains("injected worker panic at sequence 2"),
            "{cause}"
        );
        assert_eq!(failed.health, ComponentHealth::Unavailable);
        assert_eq!(failed.generation, first.generation);
        // Readiness is withdrawn from the upstream peer.
        next_peer(&mut observed_peers, |peer| *peer == Peer::NotReady).await?;

        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::Retry(GraphSelection::Exact(vec![
                    sink_id(),
                ]))],
            )
            .await?;
        let report = control
            .reconcile(preview, TopologyBindings::default())
            .await?;
        anyhow::ensure!(report.summary == OperationSummary::Completed, "{report:?}");
        let restarted = observe(&control, "restarted", |node| {
            node.generation != first.generation && node.lifecycle == ComponentLifecycle::Running
        })
        .await?;
        assert!(restarted.failure.is_none(), "{restarted:?}");
        next_peer(&mut observed_peers, |peer| *peer == Peer::Ready).await?;

        send.send(3).await?;
        assert_eq!(
            next_peer(&mut observed_peers, |peer| matches!(peer, Peer::Handled(_))).await?,
            Peer::Handled(3)
        );
        let current = control.observed().components[&sink_id()].clone();
        assert_eq!(current.lifecycle, ComponentLifecycle::Running);
        assert!(current.failure.is_none(), "{current:?}");
        assert_eq!(std::fs::read_to_string(&effects)?, "1\n2\n3\n");
        Ok::<_, anyhow::Error>(())
    };
    // The run ends however the scenario ends.
    let scenario = async {
        let result = scenario.await;
        control.cancel();
        result
    };
    let (_, scenario) = tokio::join!(run, scenario);
    scenario
}

#[tokio::test(flavor = "current_thread")]
async fn loaded_worker_panic_fails_promptly_and_retry_restarts_current_thread() -> Result<()> {
    worker_panic_fails_node_and_retry_restarts(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn loaded_worker_panic_fails_promptly_and_retry_restarts_multi_thread() -> Result<()> {
    worker_panic_fails_node_and_retry_restarts(true).await
}

#[tokio::test(flavor = "current_thread")]
async fn linked_worker_panic_fails_promptly_and_retry_restarts() -> Result<()> {
    worker_panic_fails_node_and_retry_restarts(false).await
}
