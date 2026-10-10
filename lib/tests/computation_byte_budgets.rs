// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#[allow(dead_code)]
mod computation_support;

use std::{
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

use computation_support::*;
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_lib::computation::v1::*;
use futures::poll;

fn size(envelope: &ChangeEnvelope) -> usize {
    BinaryEnvelopeCodec::encoded_size(envelope).expect("encoded size")
}

fn configured(capacity: usize, max_bytes: usize) -> ProvidedPipe {
    ByteBoundedPipeConfig {
        capacity,
        max_bytes,
    }
    .create()
    .expect("byte-bounded pipe")
}

fn larger(mut envelope: ChangeEnvelope) -> ChangeEnvelope {
    envelope
        .append_annotation(
            ContextEntry::try_new(
                component("context"),
                "payload",
                ContextValue::Bytes(Arc::from(vec![7; 4096])),
            )
            .expect("annotation"),
        )
        .expect("append");
    envelope
}

#[test]
fn size_counts_the_exact_full_frame_without_a_schema_registry() {
    let mut codec = BinaryEnvelopeCodec::new(NonZeroUsize::new(2 << 20).unwrap());
    codec
        .register_schema(Arc::new(Schema::new(
            schema_descriptor(),
            Arc::new(ReadingValidator(schema_descriptor())),
        )))
        .unwrap();
    let mut envelope = root("source", 1, &[1, 2, 3]);
    for length in [0, 31, 32, 255, 256, 65_535, 65_536] {
        envelope = derived(
            &envelope,
            "next",
            length as u64 + 1,
            envelope.changes().clone(),
        );
        envelope
            .append_annotation(
                ContextEntry::try_new(
                    component("context"),
                    "data",
                    ContextValue::Bytes(Arc::from(vec![9; length])),
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(size(&envelope), codec.encode(&envelope).unwrap().len());
        assert_eq!(size(&envelope.clone()), size(&envelope));
    }
    let root = root("metadata", 8, &[1]);
    assert!(size(&larger(root.clone())) > size(&root));
    let with_position = ChangeEnvelope::new(
        root.id().clone(),
        root.changes().clone(),
        SystemMetadata::new(stream("metadata"), 8)
            .with_source_position(bytes::Bytes::from(vec![1; 1024])),
    );
    assert!(size(&with_position) > size(&root));
}

#[test]
fn invalid_byte_and_count_limits_fail_before_pipe_construction() {
    for config in [
        ByteBoundedPipeConfig {
            capacity: 0,
            max_bytes: 1,
        },
        ByteBoundedPipeConfig {
            capacity: usize::MAX,
            max_bytes: 1,
        },
        ByteBoundedPipeConfig {
            capacity: 1,
            max_bytes: 0,
        },
        ByteBoundedPipeConfig {
            capacity: 1,
            max_bytes: usize::MAX,
        },
    ] {
        assert!(config.capabilities().is_err());
        assert!(config.create().is_err());
    }
    assert!(ByteBoundedPipeConfig {
        capacity: 1,
        max_bytes: 1
    }
    .capabilities()
    .is_ok());
}

#[test]
fn count_only_configuration_shape_is_unchanged_and_byte_policy_is_explicit() {
    assert_eq!(
        serde_json::to_value(DesiredPipe::Bounded { capacity: 8 }).unwrap(),
        serde_json::json!({"Bounded": {"capacity": 8}})
    );
    let value = serde_json::json!({"ByteBounded": {"capacity": 8, "max_bytes": 1024}});
    let desired: DesiredPipe = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        desired,
        ByteBoundedPipeConfig {
            capacity: 8,
            max_bytes: 1024
        }
        .specification()
        .unwrap()
    );
    assert_eq!(serde_json::to_value(desired).unwrap(), value);
    assert!(serde_json::from_value::<DesiredPipe>(
        serde_json::json!({"ByteBounded": {"capacity": 8, "max_bytes": 1024, "unknown": true}})
    )
    .is_err());
}

async fn exact_budget_and_oversized_singleton() {
    let normal = root("source", 1, &[1]);
    let oversized = larger(root("source", 2, &[2]));
    let limit = size(&normal) * 2;
    assert!(size(&oversized) > limit);
    let mut provided = configured(8, limit);
    let sender = provided.pipe.sender();
    let mut receiver = provided.pipe.take_receiver().expect("receiver");
    sender
        .send(normal.clone())
        .await
        .expect("first normal input");
    sender
        .send(normal.clone())
        .await
        .expect("second normal input");
    let mut large_send = Box::pin(sender.send(oversized.clone()));
    assert!(poll!(&mut large_send).is_pending());
    let first = receiver
        .receive()
        .await
        .expect("receive")
        .expect("first input");
    assert!(
        poll!(&mut large_send).is_pending(),
        "oversized input shared remaining queued work"
    );
    let second = receiver
        .receive()
        .await
        .expect("receive")
        .expect("second input");
    large_send.await.expect("oversized singleton");
    // Receipt, not destruction or downstream handling, releases bounded capacity.
    assert_eq!(first.envelope().id(), normal.id());
    assert_eq!(second.envelope().id(), normal.id());

    let mut next_normal = Box::pin(sender.send(normal.clone()));
    let mut next_large = Box::pin(sender.send(oversized.clone()));
    assert!(poll!(&mut next_normal).is_pending());
    assert!(poll!(&mut next_large).is_pending());
    assert_eq!(
        receiver
            .receive()
            .await
            .expect("receive")
            .expect("oversized input")
            .envelope()
            .id(),
        oversized.id()
    );
    next_normal.await.expect("next normal input");
    assert!(poll!(&mut next_large).is_pending());
    assert_eq!(
        receiver
            .receive()
            .await
            .expect("receive")
            .expect("normal input")
            .envelope()
            .id(),
        normal.id()
    );
    next_large.await.expect("next oversized input");
    assert_eq!(
        receiver
            .receive()
            .await
            .expect("receive")
            .expect("oversized input")
            .envelope()
            .id(),
        oversized.id()
    );
    assert!(provided.control.is_idle().await.expect("idle check"));
    let metrics = provided.control.metrics().expect("queue metrics");
    assert_eq!(metrics.accepted, 5);
    assert_eq!(metrics.delivered, 5);
    assert_eq!(metrics.blocked_sends, 3);
}

#[tokio::test(flavor = "current_thread")]
async fn exact_budget_and_oversized_singleton_current_thread() {
    tokio::time::timeout(
        Duration::from_secs(5),
        exact_budget_and_oversized_singleton(),
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exact_budget_and_oversized_singleton_multi_thread() {
    tokio::time::timeout(
        Duration::from_secs(5),
        exact_budget_and_oversized_singleton(),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn exact_byte_boundary_and_count_capacity_are_independent() {
    let event = root("source", 1, &[1]);
    for (capacity, bytes) in [(1, size(&event) * 100), (8, size(&event))] {
        let mut provided = configured(capacity, bytes);
        let sender = provided.pipe.sender();
        let mut receiver = provided.pipe.take_receiver().unwrap();
        sender.send(event.clone()).await.unwrap();
        let mut next = Box::pin(sender.send(event.clone()));
        assert!(poll!(&mut next).is_pending());
        receiver.receive().await.unwrap().unwrap();
        next.await.unwrap();
        receiver.receive().await.unwrap().unwrap();
        assert!(provided.control.is_idle().await.unwrap());
    }
}

#[tokio::test]
async fn cancelled_admission_and_receive_do_not_leak_byte_or_count_capacity() {
    let event = root("source", 1, &[1]);
    for (capacity, bytes) in [(2, size(&event)), (1, size(&event) * 4)] {
        let mut provided = configured(capacity, bytes);
        let sender = provided.pipe.sender();
        let mut receiver = provided.pipe.take_receiver().unwrap();
        sender.send(event.clone()).await.unwrap();
        for _ in 0..128 {
            let mut waiting = Box::pin(sender.send(event.clone()));
            assert!(poll!(&mut waiting).is_pending());
            drop(waiting);
        }
        receiver.receive().await.unwrap().unwrap();
        let mut empty_receive = Box::pin(receiver.receive());
        assert!(poll!(&mut empty_receive).is_pending());
        drop(empty_receive);
        sender.send(event.clone()).await.unwrap();
        assert_eq!(
            receiver.receive().await.unwrap().unwrap().envelope().id(),
            event.id()
        );
        assert!(provided.control.is_idle().await.unwrap());
        assert_eq!(provided.control.metrics().unwrap().accepted, 2);
    }
}

#[tokio::test]
async fn close_cancel_and_receiver_drop_wake_waiters_and_preserve_rejection_identity() {
    tokio::time::timeout(Duration::from_secs(5), async {
        let event = root("source", 1, &[1]);
        for operation in ["close", "cancel", "drop"] {
            let mut provided = configured(2, size(&event));
            let sender = provided.pipe.sender();
            let mut receiver = provided.pipe.take_receiver().unwrap();
            sender.send(event.clone()).await.unwrap();
            let rejected = larger(root("source", 2, &[2]));
            let mut waiting = Box::pin(sender.send(rejected.clone()));
            assert!(poll!(&mut waiting).is_pending());
            match operation {
                "close" => {
                    provided.control.close();
                    assert_eq!(
                        receiver.receive().await.unwrap().unwrap().envelope().id(),
                        event.id()
                    );
                    assert!(receiver.receive().await.unwrap().is_none());
                    assert!(provided.control.is_idle().await.unwrap());
                }
                "cancel" => {
                    provided.control.cancel();
                    assert!(receiver.receive().await.unwrap().is_none());
                    assert!(provided.control.is_idle().await.is_err());
                    assert_eq!(provided.control.metrics().unwrap().discarded, 1);
                }
                "drop" => {
                    drop(receiver);
                    assert_eq!(provided.control.metrics().unwrap().discarded, 1);
                }
                _ => unreachable!(),
            }
            let failed = waiting.await.unwrap_err();
            assert!(matches!(failed.error, PipeError::Closed));
            assert_eq!(failed.envelope.id(), rejected.id());
            assert_eq!(size(&failed.envelope), size(&rejected));
        }
    })
    .await
    .expect("pending send must not hold up closure");
}

#[tokio::test]
async fn natural_sender_closure_drains_without_a_sender_owned_by_control() {
    let mut provided = configured(1, 1);
    let sender = provided.pipe.sender();
    let mut receiver = provided.pipe.take_receiver().unwrap();
    let event = root("source", 1, &[1]);
    sender.send(event.clone()).await.unwrap();
    drop(provided.pipe);
    drop(sender);
    assert_eq!(
        receiver.receive().await.unwrap().unwrap().envelope().id(),
        event.id()
    );
    assert!(receiver.receive().await.unwrap().is_none());
    assert!(provided.control.is_idle().await.unwrap());
}

#[tokio::test]
async fn graph_export_restores_byte_policy_without_external_pipe_bindings() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let events = vec![output(root("source", 1, &[1])), output(larger(root("source", 2, &[2])))];
    let graph = ComputationGraph::builder("byte-policy")
        .source(Box::new(FiniteSource::new("source", events.clone())))
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            received.clone(),
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(ByteBoundedPipeConfig {
                capacity: 8,
                max_bytes: 1,
            }),
        )
        .build()
        .unwrap();
    let desired = graph.snapshot().select(GraphSelection::All).unwrap();
    let mut topology = ComputationTopologySource::new(
        component("topology"),
        stream("topology"),
        graph.inspector(),
    );
    topology.start().await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let event = topology.next().await.unwrap().unwrap();
            for change in GraphChangeCodec::decode_changes(&event.envelope).unwrap() {
                if let SourceChange::Insert {
                    element:
                        Element::Node {
                            metadata,
                            properties,
                        },
                } = change
                {
                    if metadata
                        .labels
                        .iter()
                        .any(|label| label.as_ref() == "ComputationPipe")
                    {
                        assert_eq!(
                            properties.get("profile"),
                            Some(&ElementValue::String("ByteBounded".into()))
                        );
                        assert_eq!(properties.get("capacity"), Some(&ElementValue::Integer(8)));
                        assert_eq!(properties.get("maxBytes"), Some(&ElementValue::Integer(1)));
                        return;
                    }
                }
            }
        }
    })
    .await
    .expect("byte policy must be queryable");
    topology.stop().await.unwrap();
    let restored = DesiredTopology::from_json(&desired.to_json().unwrap()).unwrap();
    assert_eq!(
        restored.relationships[0].pipe,
        DesiredPipe::ByteBounded(ByteBoundedPipeConfig {
            capacity: 8,
            max_bytes: 1,
        })
    );
    let mut bindings = TopologyBindings::default();
    for declaration in &restored.components {
        let ComponentConstruction::External { binding } = &declaration.construction else {
            panic!("expected a supplied component");
        };
        let instance = if declaration.descriptor.id() == &component("source") {
            ConstructedComponent::source(Box::new(FiniteSource::new("source", events.clone())))
        } else {
            ConstructedComponent::sink(Box::new(CollectSink::new(
                "sink",
                &["in"],
                received.clone(),
            )))
        };
        bindings.components.insert(binding.clone(), instance);
    }
    let mut restored = restored.build(bindings).unwrap();
    tokio::time::timeout(Duration::from_secs(5), restored.start().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        received
            .lock()
            .unwrap()
            .iter()
            .map(|input| values(&input.envelope))
            .collect::<Vec<_>>(),
        vec![vec![1], vec![2]],
    );
    restored.dispose().await.unwrap();
}
