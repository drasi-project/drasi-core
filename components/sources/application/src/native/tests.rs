// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::PropertyMapBuilder;
use anyhow::Result;
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_lib::computation::v1::*;
use std::{future::Future, time::Duration};

fn both_runtimes<F, Fut>(scenario: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    for current_thread in [true, false] {
        let mut builder = if current_thread {
            tokio::runtime::Builder::new_current_thread()
        } else {
            let mut builder = tokio::runtime::Builder::new_multi_thread();
            builder.worker_threads(2);
            builder
        };
        builder.enable_all().build().unwrap().block_on(scenario());
    }
}

fn source(capacity: usize) -> (ApplicationSource, ApplicationSourceHandle) {
    ApplicationSource::channel(
        "test-instance",
        "native-application-input",
        "input",
        StreamId::try_new("input/out").unwrap(),
        NonZeroUsize::new(capacity).unwrap(),
    )
    .unwrap()
}

async fn next(source: &mut ApplicationSource) -> ChangeEnvelope {
    tokio::time::timeout(Duration::from_secs(2), source.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap()
        .envelope
}

#[test]
fn construction_validates_identity_and_real_channel_capacity() {
    assert!(ApplicationSource::channel(
        "test-instance",
        "native-application-input",
        "",
        StreamId::try_new("out").unwrap(),
        NonZeroUsize::new(1).unwrap(),
    )
    .is_err());
    assert!(ApplicationSource::channel(
        "test-instance",
        "native-application-input",
        "input",
        StreamId::try_new("out").unwrap(),
        NonZeroUsize::new(usize::MAX).unwrap(),
    )
    .is_err());
    let (source, _) = source(7);
    let source = source.with_properties(HashMap::from([(
        "name".into(),
        serde_json::json!("example"),
    )]));
    assert_eq!(
        source.configuration().unwrap(),
        serde_json::json!({
            "kind":"application",
            "constructionScope":"test-instance","graphId":"native-application-input",
            "stream":"input/out","durable":false,"capacity":7,"properties":{"name":"example"}
        })
    );
    assert_eq!(source.output().component.as_str(), "input");
    assert_eq!(source.output().port.as_str(), "out");
}

#[test]
fn shared_handles_preserve_nodes_relations_properties_and_source_identity() {
    both_runtimes(|| async {
        let (mut source, handle) = source(8);
        let clone = source.get_handle().unwrap();
        assert_eq!(clone.source_id(), "input");
        let properties = PropertyMapBuilder::new()
            .with_string("name", "Ada")
            .with_integer("value", 7)
            .with_float("ratio", 1.5)
            .with_bool("active", true)
            .build();
        handle
            .send_node_insert("one", vec!["Item", "Named"], properties.clone())
            .await
            .unwrap();
        clone
            .send_node_update("one", vec!["Item"], properties.clone())
            .await
            .unwrap();
        handle
            .send_relation_insert("edge", vec!["LINK"], properties.clone(), "one", "two")
            .await
            .unwrap();
        clone.send_delete("one", vec!["Item"]).await.unwrap();
        source.start().await.unwrap();
        for expected in 1..=4 {
            let envelope = next(&mut source).await;
            assert_eq!(envelope.system().sequence(), expected);
            assert!(GraphChangeCodec::source_metadata(&envelope)
                .unwrap()
                .is_none());
            let progress = GraphProducerProgress::from_envelope(&envelope)
                .unwrap()
                .unwrap();
            assert!(!progress.identity().persistent());
            assert_eq!(progress.sequence(), expected);
            let changes = GraphChangeCodec::decode_changes(&envelope).unwrap();
            assert_eq!(changes.len(), 1);
            assert_eq!(changes[0].get_reference().source_id.as_ref(), "input");
            match (&changes[0], expected) {
                (
                    SourceChange::Insert {
                        element:
                            Element::Node {
                                metadata,
                                properties: actual,
                            },
                    },
                    1,
                )
                | (
                    SourceChange::Update {
                        removed_properties: _,
                        element:
                            Element::Node {
                                metadata,
                                properties: actual,
                            },
                    },
                    2,
                ) => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "one");
                    assert_eq!(*actual, properties);
                    assert!(metadata.labels.iter().any(|label| label.as_ref() == "Item"));
                }
                (
                    SourceChange::Insert {
                        element:
                            Element::Relation {
                                in_node,
                                out_node,
                                properties: actual,
                                ..
                            },
                    },
                    3,
                ) => {
                    assert_eq!(in_node.element_id.as_ref(), "one");
                    assert_eq!(out_node.element_id.as_ref(), "two");
                    assert_eq!(*actual, properties);
                }
                (SourceChange::Delete { metadata }, 4) => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "one")
                }
                other => panic!("unexpected source change: {other:?}"),
            }
        }
        source.stop().await.unwrap();
    });
}

#[test]
fn volatile_identity_survives_soft_restart_but_not_reconstruction() {
    both_runtimes(|| async {
        let (mut first, handle) = source(2);
        first.start().await.unwrap();
        handle.send_delete("one", vec!["Item"]).await.unwrap();
        let initial = GraphProducerProgress::from_envelope(&next(&mut first).await)
            .unwrap()
            .unwrap();
        first.stop().await.unwrap();
        first.start().await.unwrap();
        handle.send_delete("two", vec!["Item"]).await.unwrap();
        let resumed = GraphProducerProgress::from_envelope(&next(&mut first).await)
            .unwrap()
            .unwrap();
        assert_eq!(resumed.identity(), initial.identity());
        assert_eq!(resumed.sequence(), 2);
        first.stop().await.unwrap();
        let (mut rebuilt, handle) = source(2);
        rebuilt.start().await.unwrap();
        handle.send_delete("three", vec!["Item"]).await.unwrap();
        let new = GraphProducerProgress::from_envelope(&next(&mut rebuilt).await)
            .unwrap()
            .unwrap();
        assert_ne!(
            new.identity().incarnation(),
            initial.identity().incarnation()
        );
        assert!(!new.identity().persistent());
        rebuilt.stop().await.unwrap();
    });
}

#[test]
fn queued_input_and_backpressure_survive_stop_restart_and_cancelled_reads() {
    both_runtimes(|| async {
        let (mut source, handle) = source(1);
        handle.send_delete("first", vec!["Item"]).await.unwrap();
        let mut blocked = Box::pin(handle.send_delete("second", vec!["Item"]));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut blocked)
                .await
                .is_err()
        );
        assert!(source.next().await.is_err());
        source.start().await.unwrap();
        assert_eq!(next(&mut source).await.system().sequence(), 1);
        blocked.await.unwrap();
        source.stop().await.unwrap();
        assert!(source.next().await.is_err());
        source.start().await.unwrap();
        let second = next(&mut source).await;
        assert_eq!(second.system().sequence(), 2);
        assert_eq!(
            GraphChangeCodec::decode_changes(&second).unwrap()[0]
                .get_reference()
                .element_id
                .as_ref(),
            "second"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(10), source.next())
                .await
                .is_err()
        );
        handle.send_delete("third", vec!["Item"]).await.unwrap();
        assert_eq!(next(&mut source).await.system().sequence(), 3);
        source.stop().await.unwrap();
    });
}

#[test]
fn batches_and_closed_receivers_keep_the_existing_handle_contract() {
    both_runtimes(|| async {
        let (mut source, handle) = source(4);
        let changes: Vec<_> = (0..3)
            .map(|index| SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("input", &index.to_string()),
                    labels: Arc::from(vec![Arc::from("Item")]),
                    effective_from: index,
                },
            })
            .collect();
        handle.send_batch(changes.clone()).await.unwrap();
        source.start().await.unwrap();
        for expected in changes {
            assert_eq!(
                GraphChangeCodec::decode_changes(&next(&mut source).await).unwrap(),
                [expected]
            );
        }
        source.stop().await.unwrap();
        drop(source);
        assert!(handle.send_delete("closed", vec!["Item"]).await.is_err());
    });
}

#[test]
fn encoding_failure_retains_the_admitted_event_and_sequence() {
    both_runtimes(|| async {
        let (mut source, handle) = source(1);
        handle
            .send(SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("input", "bad-time"),
                    labels: Arc::from(vec![]),
                    effective_from: u64::MAX,
                },
            })
            .await
            .unwrap();
        source.start().await.unwrap();
        assert!(source.next().await.is_err());
        let Input::Channel(input) = &source.input else {
            unreachable!()
        };
        assert!(input.pending.is_some());
        assert_eq!(input.sequence, 0);
        source.stop().await.unwrap();
        source.start().await.unwrap();
        assert!(source.next().await.is_err());
        let Input::Channel(input) = &source.input else {
            unreachable!()
        };
        assert!(input.pending.is_some());
        source.stop().await.unwrap();
    });
}

struct Capture {
    descriptor: ComponentDescriptor,
    output: mpsc::Sender<Vec<SourceChange>>,
}

#[async_trait]
impl ComputationComponent for Capture {
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
impl EnvelopeSink for Capture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.output
            .send(GraphChangeCodec::decode_changes(&input.envelope)?)
            .await?;
        Ok(())
    }
}

#[test]
fn the_graph_polls_application_input_without_an_adapter_or_processing_task() {
    both_runtimes(|| async {
        let (source, handle) = source(2);
        let output = source.output();
        let (sender, mut receiver) = mpsc::channel(2);
        let sink = Capture {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("capture").unwrap(),
                vec![PortDescriptor::new(
                    PortId::try_new("in").unwrap(),
                    PortDirection::Input,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .unwrap(),
            output: sender,
        };
        let mut graph = ComputationGraph::builder("native-application-input")
            .source(Box::new(source))
            .sink(Box::new(sink))
            .bind_stream(output.clone(), StreamId::try_new("input/out").unwrap())
            .connect(
                EdgeDefinition::new(
                    output,
                    Endpoint::new(
                        ComponentId::try_new("capture").unwrap(),
                        PortId::try_new("in").unwrap(),
                    ),
                ),
                Box::new(BoundedPipeConfig { capacity: 2 }),
            )
            .build()
            .unwrap();
        let run = graph.start().unwrap();
        let control = run.control();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(run, async {
                control.startup_report().await.unwrap();
                handle
                    .send_node_insert(
                        "one",
                        vec!["Item"],
                        PropertyMapBuilder::new().with_integer("value", 7).build(),
                    )
                    .await
                    .unwrap();
                let changes = receiver.recv().await.unwrap();
                assert_eq!(changes.len(), 1);
                assert_eq!(changes[0].get_reference().element_id.as_ref(), "one");
                assert_eq!(control.observed().components.len(), 2);
                control.cancel();
            })
        })
        .await
        .unwrap();
        assert!(matches!(result, Err(GraphError::Cancelled)));
        graph.shutdown().await.unwrap();
    });
}
