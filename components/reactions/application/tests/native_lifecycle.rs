// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use async_trait::async_trait;
use drasi_lib::computation::v1::*;
use drasi_reaction_application::{native::ApplicationReactionError, NativeApplicationReaction};
use tokio::sync::oneshot;

fn schema() -> SchemaDescriptor {
    QueryChangeCodec::schema().descriptor().clone()
}

struct OneEvent {
    descriptor: ComponentDescriptor,
    event: Option<OutputEnvelope>,
    ready: Option<oneshot::Receiver<()>>,
}

impl OneEvent {
    fn new() -> Self {
        let port = PortId::try_new("out").expect("output port");
        Self {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("source").expect("source ID"),
                vec![PortDescriptor::new(
                    port.clone(),
                    PortDirection::Output,
                    schema(),
                    PipeRequirements::default(),
                )],
            )
            .expect("source descriptor"),
            event: Some(OutputEnvelope {
                port,
                envelope: ChangeEnvelope::new(
                    EnvelopeId::try_new("event", vec![1].into()).expect("envelope ID"),
                    ChangeSet::try_new(
                        ChangeSetId::try_new("change", vec![1].into()).expect("change ID"),
                        schema(),
                        vec![],
                    )
                    .expect("change set"),
                    SystemMetadata::new(StreamId::try_new("source").expect("source stream"), 1),
                ),
            }),
            ready: None,
        }
    }
}

#[async_trait]
impl ComputationComponent for OneEvent {
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
impl EnvelopeSource for OneEvent {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        if let Some(ready) = self.ready.as_mut() {
            ready.await?;
            self.ready = None;
        }
        Ok(self.event.take())
    }
}

fn graph(reaction: NativeApplicationReaction, source: OneEvent) -> ComputationGraphBuilder {
    let input = reaction.input();
    let output = Endpoint::new(
        source.descriptor.id().clone(),
        PortId::try_new("out").expect("output port"),
    );
    ComputationGraph::builder("native-application-lifecycle")
        .source(Box::new(source))
        .sink(Box::new(reaction))
        .bind_stream(
            output.clone(),
            StreamId::try_new("source").expect("source stream"),
        )
        .connect(
            EdgeDefinition::new(output, input),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .cleanup_timeout(Duration::from_millis(25))
}

fn cause(error: &GraphError) -> &GraphError {
    match error {
        GraphError::Reported { cause: reported } => cause(reported),
        _ => error,
    }
}

fn assert_stop_timeout(error: GraphError) {
    let GraphError::Cleanup { errors, .. } = error else {
        panic!("expected retained cleanup failure, got {error:?}");
    };
    assert!(
        errors.iter().any(|error| matches!(
            cause(error),
            GraphError::StopTimeout { component } if component.as_str() == "callback"
        )),
        "{errors:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn graph_drop_and_cancellation_retain_callback_through_cleanup_deadlines() {
    for cancel in [false, true] {
        let (started, mut entered) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let submitted = Arc::new(AtomicUsize::new(0));
        let completed = Arc::new(AtomicUsize::new(0));
        let calls = submitted.clone();
        let finished = completed.clone();
        let mut job = Some((started, blocked));
        let reaction = NativeApplicationReaction::callback("callback", schema(), move |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            let (started, blocked) = job.take().expect("one callback invocation");
            let finished = finished.clone();
            async move {
                tokio::task::spawn_blocking(move || {
                    started.send(()).unwrap();
                    blocked.recv().unwrap();
                    finished.fetch_add(1, Ordering::SeqCst);
                })
                .await?;
                Ok(())
            }
        })
        .unwrap();
        let mut graph = graph(reaction, OneEvent::new()).build().unwrap();
        let mut run = Box::pin(graph.start().unwrap());
        let control = run.control();
        tokio::select! {
            result = &mut run => panic!("graph exited before callback submission: {result:?}"),
            result = &mut entered => result.unwrap(),
            _ = tokio::time::sleep(Duration::from_secs(5)) => panic!("callback was not submitted"),
        }
        if cancel {
            control.cancel();
            assert_stop_timeout(
                tokio::time::timeout(Duration::from_secs(5), run)
                    .await
                    .unwrap()
                    .unwrap_err(),
            );
        } else {
            drop(run);
        }
        assert_eq!(control.state(), GraphState::CleanupRequired);
        assert!(matches!(
            graph.start(),
            Err(GraphError::InvalidState {
                state: GraphState::CleanupRequired
            })
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(5), graph.shutdown())
                .await
                .is_err(),
            "cancelling cleanup must not drop the submitted operation"
        );
        assert_stop_timeout(graph.shutdown().await.unwrap_err());
        assert_eq!(control.state(), GraphState::CleanupRequired);
        assert_eq!(submitted.load(Ordering::SeqCst), 1);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), graph.shutdown())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completed.load(Ordering::SeqCst), 1);
        assert_eq!(submitted.load(Ordering::SeqCst), 1);
        assert_eq!(graph.state(), GraphState::Cancelled);
    }
}

#[derive(Debug, thiserror::Error)]
#[error("application operation failed")]
struct ApplicationFailure;

#[tokio::test(flavor = "current_thread")]
async fn graph_preserves_callback_errors_panics_and_receiver_closure() {
    let failed = NativeApplicationReaction::callback("failed", schema(), |_| async {
        Err(ApplicationFailure.into())
    })
    .unwrap();
    let panicked = NativeApplicationReaction::callback("panicked", schema(), |_| async {
        panic!("actual application panic");
    })
    .unwrap();
    let (closed, mut receiver) =
        NativeApplicationReaction::channel("closed", schema(), NonZeroUsize::new(1).unwrap())
            .unwrap();
    for (reaction, expected) in [
        (failed, "failed"),
        (panicked, "panicked"),
        (closed, "closed"),
    ] {
        let (release, ready) = oneshot::channel();
        let mut source = OneEvent::new();
        source.ready = Some(ready);
        let mut graph = graph(reaction, source).build().unwrap();
        let run = graph.start().unwrap();
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            control.startup_report().await.unwrap();
            if expected == "closed" {
                // Close after startup, exercising the actual handle failure.
                receiver.close();
            }
            release.send(()).unwrap();
        });
        let error = result.unwrap_err();
        let GraphError::Component {
            component,
            operation,
            source,
        } = cause(&error)
        else {
            panic!("graph did not preserve the component failure: {error:?}");
        };
        assert_eq!(component.as_str(), expected);
        assert_eq!(*operation, "handle");
        match expected {
            "failed" => assert!(source.is::<ApplicationFailure>()),
            "panicked" => assert!(matches!(
                source.downcast_ref(),
                Some(ApplicationReactionError::CallbackPanicked(message))
                    if message == "actual application panic"
            )),
            "closed" => assert!(matches!(
                source.downcast_ref(),
                Some(ApplicationReactionError::ReceiverClosed)
            )),
            _ => unreachable!(),
        }
        graph.shutdown().await.unwrap();
    }
}

#[test]
fn graph_rejects_acknowledgement_claims_for_application_channel() {
    for capability in [
        PipeCapability::ExplicitAcknowledgement,
        PipeCapability::Transactions,
        PipeCapability::ExactlyOnce,
    ] {
        let (reaction, _receiver) =
            NativeApplicationReaction::channel("channel", schema(), NonZeroUsize::new(1).unwrap())
                .unwrap();
        assert!(matches!(
            graph(reaction, OneEvent::new())
                .requirements(PipeRequirements::new([capability]))
                .build(),
            Err(GraphError::Contract(
                ContractError::InsufficientSinkCompletion { .. }
            ))
        ));
    }
}
