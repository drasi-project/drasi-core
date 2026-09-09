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

mod computation_support;

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use computation_support::*;
use drasi_lib::computation::v1::*;

macro_rules! both_runtimes {
    ($name:ident, $scenario:ident) => {
        mod $name {
            #[tokio::test(flavor = "current_thread")]
            async fn current_thread() {
                super::$scenario().await;
            }

            #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
            async fn multi_thread() {
                super::$scenario().await;
            }
        }
    };
}

async fn complete(graph: &mut ComputationGraph) {
    let run = graph.start().expect("graph starts");
    let control = run.control();
    run.await.expect("finite graph drains");
    assert_eq!(control.state(), GraphState::Completed);
    assert_eq!(graph.state(), GraphState::Completed);
}

fn contributors(envelope: &Envelope) -> Vec<String> {
    envelope
        .context()
        .entries()
        .map(|entry| {
            assert_eq!(entry.key(), "visited");
            assert_eq!(
                entry.value(),
                ContextValue::String(Arc::from(entry.contributor()))
            );
            entry.contributor().to_owned()
        })
        .collect()
}

async fn direct_scenario() {
    let inputs = [root("source", 0, &[2, 3]), root("source", 4, &[5]), root("source", 9, &[8, 13])];
    let received = Received::default();
    let mut graph = ComputationGraph::builder("native-direct")
        .source(Box::new(FiniteSource::new(
            "source",
            inputs.iter().cloned().map(output).collect(),
        )))
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            received.clone(),
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("direct graph");

    complete(&mut graph).await;
    let received = received.lock().expect("sink lock");
    assert_eq!(received.len(), inputs.len());
    assert_eq!(
        received
            .iter()
            .flat_map(|input| values(&input.envelope))
            .collect::<Vec<_>>(),
        [2, 3, 5, 8, 13]
    );
    for (actual, expected) in received.iter().zip(&inputs) {
        assert_eq!(actual.port, port("in"));
        assert_eq!(actual.envelope.id(), expected.id());
        assert_eq!(
            actual.envelope.id().value()[0],
            0xff,
            "opaque IDs are accepted"
        );
        assert!(Arc::ptr_eq(actual.envelope.changes(), expected.changes()));
        assert!(Arc::ptr_eq(actual.envelope.system(), expected.system()));
        assert_eq!(contributors(&actual.envelope), ["source"]);
        assert!(actual.envelope.lineage().is_none());
    }
}

both_runtimes!(direct, direct_scenario);

async fn annotation_history_scenario() {
    let original = root("source", 1, &[2, 4]);
    let left = Received::default();
    let right = Received::default();
    let transform = NativeTransform::new("double", |input| {
        let values: Vec<_> = values(&input.envelope)
            .into_iter()
            .map(|value| value * 2)
            .collect();
        Ok(vec![output(derived(
            &input.envelope,
            "double",
            1,
            changes("double", 1, &values),
        ))])
    });
    let mut graph = ComputationGraph::builder("annotation-history")
        .source(Box::new(FiniteSource::new(
            "source",
            vec![output(original.clone())],
        )))
        .transformer(Box::new(transform))
        .sink(Box::new(
            CollectSink::new("left", &["in"], left.clone()).with_annotation(),
        ))
        .sink(Box::new(
            CollectSink::new("right", &["in"], right.clone()).with_annotation(),
        ))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("double", "out"), stream("double"))
        .connect(
            edge("source", "double"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("double", "left"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("double", "right"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("annotation graph");

    complete(&mut graph).await;
    let left = left.lock().expect("left sink");
    let right = right.lock().expect("right sink");
    assert_eq!(left.len(), 1);
    assert_eq!(right.len(), 1);
    let left = &left[0].envelope;
    let right = &right[0].envelope;
    assert_eq!(values(left), [4, 8]);
    assert_eq!(values(right), [4, 8]);
    assert!(Arc::ptr_eq(left.event(), right.event()));
    assert!(!Arc::ptr_eq(left.event(), original.event()));
    assert_eq!(contributors(left), ["left", "double", "source"]);
    assert_eq!(contributors(right), ["right", "double", "source"]);
    assert_eq!(contributors(&original), ["source"]);
    assert_eq!(
        left.event()
            .lineage()
            .expect("transformed event retains input lineage")
            .envelope_id(),
        original.id()
    );
}

both_runtimes!(annotation_history, annotation_history_scenario);

async fn chain_scenario() {
    let inputs = [root("source", 20, &[2, 4]), root("source", 30, &[6])];
    let mut double_sequence = 0;
    let double = NativeTransform::new("double", move |input| {
        double_sequence += 1;
        let readings: Vec<_> = values(&input.envelope)
            .into_iter()
            .map(|value| value * 2)
            .collect();
        Ok(vec![output(derived(
            &input.envelope,
            "double",
            double_sequence,
            changes("double", double_sequence, &readings),
        ))])
    });
    let mut add_sequence = 100;
    let add = NativeTransform::new("add", move |input| {
        add_sequence += 1;
        let readings: Vec<_> = values(&input.envelope)
            .into_iter()
            .map(|value| value + 1)
            .collect();
        Ok(vec![output(derived(
            &input.envelope,
            "add",
            add_sequence,
            changes("add", add_sequence, &readings),
        ))])
    });
    let received = Received::default();
    let mut graph = ComputationGraph::builder("native-chain")
        .source(Box::new(FiniteSource::new(
            "source",
            inputs.iter().cloned().map(output).collect(),
        )))
        .transformer(Box::new(double))
        .transformer(Box::new(add))
        .sink(Box::new(CollectSink::new(
            "sink",
            &["in"],
            received.clone(),
        )))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("double", "out"), stream("double"))
        .bind_stream(endpoint("add", "out"), stream("add"))
        .connect(
            edge("source", "double"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("double", "add"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("add", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("two-transform graph");

    complete(&mut graph).await;
    let received = received.lock().expect("sink lock");
    assert_eq!(received.len(), 2);
    assert_eq!(values(&received[0].envelope), [5, 9]);
    assert_eq!(values(&received[1].envelope), [13]);
    for (index, (actual, source)) in received.iter().zip(&inputs).enumerate() {
        let envelope = &actual.envelope;
        let ordinal = u64::try_from(index).expect("index");
        assert_eq!(envelope.system().stream(), &stream("add"));
        assert_eq!(envelope.system().sequence(), 101 + ordinal);
        assert_eq!(contributors(envelope), ["add", "double", "source"]);
        let parent = envelope.lineage().expect("double lineage");
        assert_eq!(
            parent.envelope_id(),
            &emission_id(&stream("double"), 1 + ordinal).expect("ID")
        );
        assert_eq!(parent.system().stream(), &stream("double"));
        assert_eq!(parent.system().sequence(), 1 + ordinal);
        let ancestor = parent.parent().expect("source lineage");
        assert_eq!(ancestor.envelope_id(), source.id());
        assert!(Arc::ptr_eq(ancestor.system(), source.system()));
        assert!(ancestor.parent().is_none());
        assert_eq!(contributors(source), ["source"]);
    }
}

both_runtimes!(two_transform_chain, chain_scenario);

async fn cardinality_scenario() {
    for counts in [vec![], vec![0], vec![1], vec![3], vec![0, 1, 3, 0, 1]] {
        let source_outputs: Vec<_> = counts
            .iter()
            .enumerate()
            .map(|(index, count)| {
                output(root(
                    "source",
                    u64::try_from(index).expect("index"),
                    &[*count],
                ))
            })
            .collect();
        let mut sequence = 0;
        let transform = NativeTransform::new("expand", move |input| {
            let count = values(&input.envelope)[0];
            Ok((0..count)
                .map(|value| {
                    sequence += 1;
                    output(derived(
                        &input.envelope,
                        "expand",
                        sequence,
                        changes("expand", sequence, &[value + 10]),
                    ))
                })
                .collect())
        });
        let received = Received::default();
        let mut graph = ComputationGraph::builder("cardinality")
            .source(Box::new(FiniteSource::new("source", source_outputs)))
            .transformer(Box::new(transform))
            .sink(Box::new(CollectSink::new(
                "sink",
                &["in"],
                received.clone(),
            )))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .bind_stream(endpoint("expand", "out"), stream("expand"))
            .connect(
                edge("source", "expand"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                edge("expand", "sink"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()
            .expect("cardinality graph");
        complete(&mut graph).await;
        let received = received.lock().expect("sink lock");
        let expected: Vec<_> = counts
            .iter()
            .enumerate()
            .flat_map(|(index, count)| (0..*count).map(move |value| (index, value + 10)))
            .collect();
        assert_eq!(received.len(), expected.len(), "counts {counts:?}");
        for (index, (actual, (source_index, value))) in received.iter().zip(expected).enumerate() {
            assert_eq!(values(&actual.envelope), [value]);
            assert_eq!(
                actual.envelope.system().sequence(),
                u64::try_from(index + 1).expect("index")
            );
            assert_eq!(actual.envelope.system().stream(), &stream("expand"));
            assert_eq!(
                actual
                    .envelope
                    .lineage()
                    .expect("source lineage")
                    .system()
                    .sequence(),
                u64::try_from(source_index).expect("source index")
            );
            assert_eq!(contributors(&actual.envelope), ["expand", "source"]);
        }
    }
}

both_runtimes!(zero_one_many_outputs, cardinality_scenario);

async fn fanout_scenario() {
    let original = root("source", 42, &[7, 11]);
    let left = Received::default();
    let right = Received::default();
    let mut builder = ComputationGraph::builder("fanout")
        .source(Box::new(FiniteSource::new(
            "source",
            vec![output(original.clone())],
        )))
        .bind_stream(endpoint("source", "out"), stream("source"));
    for (branch, sink, received) in
        [("left", "left-sink", left.clone()), ("right", "right-sink", right.clone())]
    {
        let mut sequence = 0;
        builder = builder
            .transformer(Box::new(NativeTransform::new(branch, move |input| {
                sequence += 1;
                Ok(vec![output(derived(
                    &input.envelope,
                    branch,
                    sequence,
                    input.envelope.changes().clone(),
                ))])
            })))
            .sink(Box::new(CollectSink::new(sink, &["in"], received)))
            .bind_stream(endpoint(branch, "out"), stream(branch))
            .connect(
                edge("source", branch),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                edge(branch, sink),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
    }
    let mut graph = builder.build().expect("fanout graph");
    complete(&mut graph).await;
    let left = left.lock().expect("left sink");
    let right = right.lock().expect("right sink");
    assert_eq!(left.len(), 1);
    assert_eq!(right.len(), 1);
    let left = &left[0].envelope;
    let right = &right[0].envelope;
    assert!(Arc::ptr_eq(left.changes(), right.changes()));
    assert!(Arc::ptr_eq(left.changes(), original.changes()));
    assert_eq!(values(left), [7, 11]);
    assert_eq!(values(right), [7, 11]);
    assert_eq!(contributors(left), ["left", "source"]);
    assert_eq!(contributors(right), ["right", "source"]);
    assert_eq!(contributors(&original), ["source"]);
    assert_ne!(left.id(), right.id());
    assert_ne!(left.system().stream(), right.system().stream());
    for branch in [left, right] {
        let lineage = branch.lineage().expect("source lineage");
        assert_eq!(lineage.envelope_id(), original.id());
        assert!(Arc::ptr_eq(lineage.system(), original.system()));
        assert!(lineage.parent().is_none());
    }
}

both_runtimes!(fanout_branch_isolation, fanout_scenario);

async fn fanin_scenario() {
    // No cross-stream order is assumed. Both a shared input and distinct input
    // ports must preserve each bound stream's own FIFO, never timestamp order.
    for separate_ports in [false, true] {
        let received = Received::default();
        let inputs: &[&str] = if separate_ports {
            &["left", "right"]
        } else {
            &["in"]
        };
        let mut builder = ComputationGraph::builder("fanin").sink(Box::new(CollectSink::new(
            "sink",
            inputs,
            received.clone(),
        )));
        for (producer, base) in [("left", 10), ("right", 20)] {
            let outputs = [(0, 30), (2, 30), (7, 10), (8, 20)]
                .into_iter()
                .enumerate()
                .map(|(index, (sequence, seconds))| {
                    let root = root(
                        producer,
                        sequence,
                        &[base + u16::try_from(index).expect("index")],
                    );
                    output(
                        Envelope::new(
                            root.id().clone(),
                            root.changes().clone(),
                            SystemMetadata::new(stream(producer), sequence).with_timestamp(
                                DateTime::<Utc>::from_timestamp(seconds, 0).expect("timestamp"),
                            ),
                        )
                        .append_context(annotation(producer))
                        .expect("source context"),
                    )
                })
                .collect();
            builder = builder
                .source(Box::new(FiniteSource::new(producer, outputs)))
                .bind_stream(endpoint(producer, "out"), stream(producer))
                .connect(
                    EdgeDefinition::new(
                        endpoint(producer, "out"),
                        endpoint("sink", if separate_ports { producer } else { "in" }),
                    ),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                );
        }
        let mut graph = builder.build().expect("fanin graph");
        complete(&mut graph).await;
        let received = received.lock().expect("sink lock");
        assert_eq!(received.len(), 8);
        for (producer, base) in [("left", 10), ("right", 20)] {
            let events: Vec<_> = received
                .iter()
                .filter(|input| input.envelope.system().stream() == &stream(producer))
                .collect();
            assert_eq!(events.len(), 4);
            assert_eq!(
                events
                    .iter()
                    .map(|input| input.envelope.system().sequence())
                    .collect::<Vec<_>>(),
                [0, 2, 7, 8]
            );
            assert_eq!(
                events
                    .iter()
                    .map(|input| input
                        .envelope
                        .system()
                        .timestamp()
                        .expect("timestamp")
                        .timestamp())
                    .collect::<Vec<_>>(),
                [30, 30, 10, 20]
            );
            assert_eq!(
                events
                    .iter()
                    .flat_map(|input| values(&input.envelope))
                    .collect::<Vec<_>>(),
                [base, base + 1, base + 2, base + 3]
            );
            for event in events {
                assert_eq!(
                    event.port,
                    port(if separate_ports { producer } else { "in" })
                );
                assert_eq!(contributors(&event.envelope), [producer]);
            }
        }
    }
}

both_runtimes!(fanin_per_stream_fifo, fanin_scenario);

#[derive(Debug, Clone, Copy)]
enum Fault {
    UnknownPort,
    InputPort,
    Schema,
    Stream,
    EqualSequence,
    RegressingSequence,
    DuplicateIdentity,
}

fn invalid_emission(producer: &str, fault: Fault, input: &Envelope) -> OutputEnvelope {
    let sequence = match fault {
        Fault::EqualSequence => 10,
        Fault::RegressingSequence => 9,
        _ => 11,
    };
    let schema = if matches!(fault, Fault::Schema) {
        named_schema("test.other-reading")
    } else {
        schema_descriptor()
    };
    let stream = stream(if matches!(fault, Fault::Stream) {
        "foreign"
    } else {
        producer
    });
    let identity_sequence = if matches!(fault, Fault::DuplicateIdentity) {
        10
    } else {
        sequence
    };
    let envelope = input.derive(
        emission_id(&stream, identity_sequence).expect("output ID"),
        changes_with_schema(schema, producer, sequence, &[99]),
        SystemMetadata::new(stream, sequence),
    );
    OutputEnvelope {
        port: port(match fault {
            Fault::UnknownPort => "missing",
            Fault::InputPort => "in",
            _ => "out",
        }),
        envelope,
    }
}

fn assert_fault(error: GraphError, producer: &str, fault: Fault) {
    match (error, fault) {
        (
            GraphError::Contract(ContractError::SchemaMismatch { expected, actual }),
            Fault::Schema,
        ) => {
            assert_eq!(*expected, schema_descriptor());
            assert_eq!(*actual, named_schema("test.other-reading"));
        }
        (
            GraphError::Emission {
                component: actual,
                reason,
            },
            fault,
        ) => {
            assert_eq!(actual, component(producer));
            let expected = match fault {
                Fault::UnknownPort => "unknown output port",
                Fault::InputPort => "emission targets an input port",
                Fault::Stream => "stream is not owned",
                Fault::EqualSequence | Fault::RegressingSequence => {
                    "sequence must strictly increase"
                }
                Fault::DuplicateIdentity => "duplicate logical envelope ID",
                Fault::Schema => {
                    panic!("schema mismatch must retain its structured contract error")
                }
            };
            assert!(reason.contains(expected), "{fault:?}: {reason}");
        }
        (error, fault) => panic!("unexpected error for {fault:?}: {error:?}"),
    }
}

async fn invalid_source_scenario() {
    for fault in [
        Fault::UnknownPort,
        Fault::Schema,
        Fault::Stream,
        Fault::EqualSequence,
        Fault::RegressingSequence,
    ] {
        let input = root("source", 10, &[1]);
        let mut outputs = Vec::new();
        let needs_prefix = matches!(fault, Fault::EqualSequence | Fault::RegressingSequence);
        if needs_prefix {
            outputs.push(output(input.clone()));
        }
        outputs.push(invalid_emission("source", fault, &input));
        let sends = Arc::new(AtomicUsize::new(0));
        let received = Received::default();
        let mut graph = ComputationGraph::builder("invalid-source")
            .source(Box::new(FiniteSource::new("source", outputs)))
            .sink(Box::new(CollectSink::new("sink", &["in"], received)))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .connect(
                edge("source", "sink"),
                Box::new(AuditedBoundedPipe {
                    sends: sends.clone(),
                }),
            )
            .build()
            .expect("valid descriptors");
        let run = graph.start().expect("graph starts");
        let control = run.control();
        let error = run.await.expect_err("invalid source output fails the run");
        assert_fault(error, "source", fault);
        assert_eq!(control.state(), GraphState::Failed);
        assert_eq!(graph.state(), GraphState::Failed);
        assert_eq!(
            sends.load(Ordering::SeqCst),
            usize::from(needs_prefix),
            "{fault:?}"
        );
    }
}

both_runtimes!(invalid_source_outputs, invalid_source_scenario);

async fn invalid_transform_scenario() {
    for fault in [
        Fault::UnknownPort,
        Fault::InputPort,
        Fault::Schema,
        Fault::Stream,
        Fault::EqualSequence,
        Fault::RegressingSequence,
        Fault::DuplicateIdentity,
    ] {
        let transform = NativeTransform::new("transform", move |input| {
            Ok(vec![
                output(derived(
                    &input.envelope,
                    "transform",
                    10,
                    changes("transform", 10, &[1]),
                )),
                invalid_emission("transform", fault, &input.envelope),
                output(derived(
                    &input.envelope,
                    "transform",
                    12,
                    changes("transform", 12, &[3]),
                )),
            ])
        });
        let sends = Arc::new(AtomicUsize::new(0));
        let received = Received::default();
        let mut graph = ComputationGraph::builder("invalid-transform-batch")
            .source(Box::new(FiniteSource::new(
                "source",
                vec![output(root("source", 1, &[7]))],
            )))
            .transformer(Box::new(transform))
            .sink(Box::new(CollectSink::new(
                "sink",
                &["in"],
                received.clone(),
            )))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .bind_stream(endpoint("transform", "out"), stream("transform"))
            .connect(
                edge("source", "transform"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                edge("transform", "sink"),
                Box::new(AuditedBoundedPipe {
                    sends: sends.clone(),
                }),
            )
            .build()
            .expect("valid descriptors");
        let run = graph.start().expect("graph starts");
        let control = run.control();
        let error = run
            .await
            .expect_err("invalid transform output fails the run");
        assert_fault(error, "transform", fault);
        assert_eq!(control.state(), GraphState::Failed);
        assert_eq!(graph.state(), GraphState::Failed);
        assert_eq!(
            sends.load(Ordering::SeqCst),
            0,
            "{fault:?}: batch prefix must not be sent"
        );
        assert!(received.lock().expect("sink lock").is_empty());
    }
}

both_runtimes!(
    complete_transform_batch_validation,
    invalid_transform_scenario
);

#[test]
fn fixture_schema_rejects_invalid_identities_and_record_images() {
    let descriptor = schema_descriptor();
    let schema = Schema::new(descriptor.clone(), Arc::new(ReadingValidator(descriptor)));
    for (namespace, identity) in
        [("other", vec![1]), ("readings", vec![0]), ("readings", vec![1, 2])]
    {
        assert!(matches!(
            RecordReference::try_new(
                &schema,
                RecordId::try_new(namespace, Bytes::from(identity)).expect("opaque identity"),
            ),
            Err(ContractError::RecordValidation { .. })
        ));
    }
    let identity = RecordId::try_new("readings", Bytes::from_static(&[1])).expect("identity");
    for (image, payload) in [
        (RecordImage::Full, vec![]),
        (RecordImage::Full, vec![1, 0]),
        (RecordImage::Full, vec![2, 0, 7]),
        (RecordImage::Patch, vec![1, 0, 7]),
        (RecordImage::Partial, vec![1]),
    ] {
        assert!(matches!(
            Record::try_new(&schema, identity.clone(), image, Bytes::from(payload)),
            Err(ContractError::RecordValidation { .. })
        ));
    }
    let valid = Record::try_new(
        &schema,
        identity,
        RecordImage::Full,
        Bytes::from_static(&[1, 0, 7]),
    )
    .expect("valid full reading");
    assert_eq!(valid.payload().as_ref(), &[1, 0, 7]);
}
