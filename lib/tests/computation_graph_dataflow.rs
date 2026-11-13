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

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use bytes::Bytes;
use chrono::{DateTime, Utc};
use computation_support::*;
use drasi_lib::computation::v1::*;

fn named_component(name: &str) -> ComponentId {
    fixture_support::component_id(match name {
        "source" => 77,
        "left" => 2,
        "right" => 3,
        "left-sink" => 4,
        "right-sink" => 5,
        "child-a" => 6,
        "child-b" => 7,
        "left-one" => 8,
        "left-two" => 9,
        "right-one" => 10,
        "right-two" => 11,
        "child-a-sink" => 12,
        "child-b-sink" => 13,
        "transform" => 429,
        other => panic!("missing explicit fixture component ID for {other}"),
    })
}

macro_rules! both_tokio_flavors {
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

fn assert_contributors(envelope: &Envelope, expected: &[(ComponentId, &str)]) {
    let actual = envelope
        .context()
        .entries()
        .map(|entry| {
            assert_eq!(entry.key(), "visited");
            (entry.contributor().clone(), entry.value().clone())
        })
        .collect::<Vec<_>>();
    let expected = expected
        .iter()
        .map(|(id, value)| (id.clone(), ContextValue::String(Arc::from(*value))))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

async fn direct_scenario() {
    let inputs = [
        root(
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            "source",
            0,
            &[2, 3],
        ),
        root(
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            "source",
            4,
            &[5],
        ),
        root(
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            "source",
            9,
            &[8, 13],
        ),
    ];
    let received = Received::default();
    let mut graph = ComputationGraph::builder("native-direct")
        .fixture_source(Box::new(FiniteSource::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            inputs.iter().cloned().map(output).collect(),
        )))
        .fixture_sink(Box::new(CollectSink::new(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            "sink",
            &["in"],
            received.clone(),
        )))
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
        assert_contributors(&actual.envelope, &[(component(77), "source")]);
        assert!(actual.envelope.lineage().is_none());
    }
}

both_tokio_flavors!(direct, direct_scenario);

async fn annotation_history_scenario() {
    let original = root(
        &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
            .expect("fixture UUID"),
        "source",
        1,
        &[2, 4],
    );
    let left = Received::default();
    let right = Received::default();
    let transform = NativeTransform::new(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001aa")
            .expect("fixture UUID"),
        |input| {
            let values: Vec<_> = values(&input.envelope)
                .into_iter()
                .map(|value| value * 2)
                .collect();
            Ok(vec![output(derived(
                &input.envelope,
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001aa")
                    .expect("fixture UUID"),
                "double",
                1,
                changes("double", 1, &values),
            ))])
        },
    );
    let mut graph = ComputationGraph::builder("annotation-history")
        .fixture_source(Box::new(FiniteSource::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            vec![output(original.clone())],
        )))
        .fixture_transformer(Box::new(transform))
        .fixture_sink(Box::new(
            CollectSink::new(
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000002e7")
                    .expect("fixture UUID"),
                "left",
                &["in"],
                left.clone(),
            )
            .with_annotation(),
        ))
        .fixture_sink(Box::new(
            CollectSink::new(
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001d1")
                    .expect("fixture UUID"),
                "right",
                &["in"],
                right.clone(),
            )
            .with_annotation(),
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
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001aa")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("double"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001aa")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001aa")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000002e7")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001aa")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001d1")
                    .expect("fixture UUID"),
            ),
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
    assert_contributors(
        left,
        &[(component(743), "left"), (component(426), "double"), (component(77), "source")],
    );
    assert_contributors(
        right,
        &[(component(465), "right"), (component(426), "double"), (component(77), "source")],
    );
    assert_contributors(&original, &[(component(77), "source")]);
    assert_eq!(
        left.event()
            .lineage()
            .expect("transformed event retains input lineage")
            .envelope_id(),
        original.id()
    );
}

both_tokio_flavors!(annotation_history, annotation_history_scenario);

async fn chain_scenario() {
    let inputs = [
        root(
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            "source",
            20,
            &[2, 4],
        ),
        root(
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            "source",
            30,
            &[6],
        ),
    ];
    let mut double_sequence = 0;
    let double = NativeTransform::new(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001aa")
            .expect("fixture UUID"),
        move |input| {
            double_sequence += 1;
            let readings: Vec<_> = values(&input.envelope)
                .into_iter()
                .map(|value| value * 2)
                .collect();
            Ok(vec![output(derived(
                &input.envelope,
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001aa")
                    .expect("fixture UUID"),
                "double",
                double_sequence,
                changes("double", double_sequence, &readings),
            ))])
        },
    );
    let mut add_sequence = 100;
    let add = NativeTransform::new(
        drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ab")
            .expect("fixture UUID"),
        move |input| {
            add_sequence += 1;
            let readings: Vec<_> = values(&input.envelope)
                .into_iter()
                .map(|value| value + 1)
                .collect();
            Ok(vec![output(derived(
                &input.envelope,
                &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001ab")
                    .expect("fixture UUID"),
                "add",
                add_sequence,
                changes("add", add_sequence, &readings),
            ))])
        },
    );
    let received = Received::default();
    let mut graph = ComputationGraph::builder("native-chain")
        .fixture_source(Box::new(FiniteSource::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            inputs.iter().cloned().map(output).collect(),
        )))
        .fixture_transformer(Box::new(double))
        .fixture_transformer(Box::new(add))
        .fixture_sink(Box::new(CollectSink::new(
            drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                .expect("fixture UUID"),
            "sink",
            &["in"],
            received.clone(),
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
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001aa")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("double"),
        )
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ab")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("add"),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001aa")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001aa")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ab")
                    .expect("fixture UUID"),
            ),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ab")
                    .expect("fixture UUID"),
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
            ),
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
        assert_contributors(
            envelope,
            &[(component(427), "add"), (component(426), "double"), (component(77), "source")],
        );
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
        assert_contributors(source, &[(component(77), "source")]);
    }
}

both_tokio_flavors!(two_transform_chain, chain_scenario);

async fn cardinality_scenario() {
    for counts in [vec![], vec![0], vec![1], vec![3], vec![0, 1, 3, 0, 1]] {
        let source_outputs: Vec<_> = counts
            .iter()
            .enumerate()
            .map(|(index, count)| {
                output(root(
                    &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "source",
                    u64::try_from(index).expect("index"),
                    &[*count],
                ))
            })
            .collect();
        let mut sequence = 0;
        let transform = NativeTransform::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ac")
                .expect("fixture UUID"),
            move |input| {
                let count = values(&input.envelope)[0];
                Ok((0..count)
                    .map(|value| {
                        sequence += 1;
                        output(derived(
                            &input.envelope,
                            &drasi_lib::ComponentId::try_new(
                                "00000000-0000-0000-0000-0000000001ac",
                            )
                            .expect("fixture UUID"),
                            "expand",
                            sequence,
                            changes("expand", sequence, &[value + 10]),
                        ))
                    })
                    .collect())
            },
        );
        let received = Received::default();
        let mut graph = ComputationGraph::builder("cardinality")
            .fixture_source(Box::new(FiniteSource::new(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                source_outputs,
            )))
            .fixture_transformer(Box::new(transform))
            .fixture_sink(Box::new(CollectSink::new(
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                &["in"],
                received.clone(),
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
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ac")
                        .expect("fixture UUID"),
                    "out",
                ),
                stream("expand"),
            )
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ac")
                        .expect("fixture UUID"),
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ac")
                        .expect("fixture UUID"),
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                        .expect("fixture UUID"),
                ),
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
            assert_contributors(
                &actual.envelope,
                &[(component(428), "expand"), (component(77), "source")],
            );
        }
    }
}

both_tokio_flavors!(zero_one_many_outputs, cardinality_scenario);

async fn fanout_scenario() {
    let original = root(
        &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
            .expect("fixture UUID"),
        "source",
        42,
        &[7, 11],
    );
    let left = Received::default();
    let right = Received::default();
    let mut builder = ComputationGraph::builder("fanout")
        .fixture_source(Box::new(FiniteSource::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            vec![output(original.clone())],
        )))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        );
    for (branch, sink, received) in
        [("left", "left-sink", left.clone()), ("right", "right-sink", right.clone())]
    {
        let branch_id = named_component(branch);
        let sink_id = named_component(sink);
        let emitted_branch_id = branch_id.clone();
        let mut sequence = 0;
        builder = builder
            .fixture_transformer(Box::new(NativeTransform::new(
                branch_id.clone(),
                move |input| {
                    sequence += 1;
                    Ok(vec![output(derived(
                        &input.envelope,
                        &emitted_branch_id,
                        branch,
                        sequence,
                        input.envelope.changes().clone(),
                    ))])
                },
            )))
            .fixture_sink(Box::new(CollectSink::new(
                sink_id.clone(),
                sink,
                &["in"],
                received,
            )))
            .bind_stream(endpoint(&branch_id, "out"), stream(branch))
            .connect(
                edge(
                    &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    &branch_id,
                ),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                edge(&branch_id, &sink_id),
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
    assert_contributors(
        left,
        &[(named_component("left"), "left"), (component(77), "source")],
    );
    assert_contributors(
        right,
        &[(named_component("right"), "right"), (component(77), "source")],
    );
    assert_contributors(&original, &[(component(77), "source")]);
    assert_ne!(left.id(), right.id());
    assert_ne!(left.system().stream(), right.system().stream());
    for branch in [left, right] {
        let lineage = branch.lineage().expect("source lineage");
        assert_eq!(lineage.envelope_id(), original.id());
        assert!(Arc::ptr_eq(lineage.system(), original.system()));
        assert!(lineage.parent().is_none());
    }
}

both_tokio_flavors!(fanout_branch_isolation, fanout_scenario);

async fn mixed_multilevel_fanout_scenario() {
    const INPUTS: u16 = 32;
    let mut builder = ComputationGraph::builder("mixed-multilevel-fanout")
        .fixture_source(Box::new(FiniteSource::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            (1..=INPUTS)
                .map(|value| {
                    output(root(
                        &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                            .expect("fixture UUID"),
                        "source",
                        u64::from(value),
                        &[value],
                    ))
                })
                .collect(),
        )))
        .bind_stream(
            endpoint(
                &drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                "out",
            ),
            stream("source"),
        );
    let mut calls = Vec::new();
    for (producer, transform, factor) in [
        ("source", "left", 2),
        ("source", "right", 3),
        ("left", "child-a", 5),
        ("left", "child-b", 7),
    ] {
        let producer_id = named_component(producer);
        let transform_id = named_component(transform);
        let emitted_transform_id = transform_id.clone();
        let count = Arc::new(AtomicUsize::new(0));
        calls.push(count.clone());
        let mut sequence = 0;
        builder = builder
            .fixture_transformer(Box::new(NativeTransform::new(
                transform_id.clone(),
                move |input| {
                    count.fetch_add(1, Ordering::SeqCst);
                    sequence += 1;
                    let readings: Vec<_> = values(&input.envelope)
                        .into_iter()
                        .map(|value| value * factor)
                        .collect();
                    Ok(vec![output(derived(
                        &input.envelope,
                        &emitted_transform_id,
                        transform,
                        sequence,
                        changes(transform, sequence, &readings),
                    ))])
                },
            )))
            .bind_stream(endpoint(&transform_id, "out"), stream(transform))
            .connect(
                edge(&producer_id, &transform_id),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
    }
    let mut observations = Vec::new();
    for (producer, sink, factor) in [
        ("left", "left-one", 2),
        ("left", "left-two", 2),
        ("right", "right-one", 3),
        ("right", "right-two", 3),
        ("child-a", "child-a-sink", 10),
        ("child-b", "child-b-sink", 14),
    ] {
        let producer_id = named_component(producer);
        let sink_id = named_component(sink);
        let received = Received::default();
        observations.push((producer, sink, factor, received.clone()));
        builder = builder
            .fixture_sink(Box::new(CollectSink::new(
                sink_id.clone(),
                sink,
                &["in"],
                received,
            )))
            .connect(
                edge(&producer_id, &sink_id),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
    }
    let mut graph = builder.build().expect("mixed fanout graph");
    tokio::time::timeout(std::time::Duration::from_secs(5), complete(&mut graph))
        .await
        .expect("all subscribers drain");
    for (producer, sink, factor, received) in observations {
        let received = received.lock().unwrap();
        assert_eq!(received.len(), usize::from(INPUTS), "{sink}");
        for (index, input) in received.iter().enumerate() {
            let sequence = u16::try_from(index + 1).unwrap();
            assert_eq!(values(&input.envelope), [sequence * factor], "{sink}");
            assert_eq!(input.envelope.system().sequence(), u64::from(sequence));
            assert_eq!(input.envelope.system().stream(), &stream(producer));
        }
    }
    for count in calls {
        assert_eq!(
            count.load(Ordering::SeqCst),
            usize::from(INPUTS),
            "a transform executes once per input, not once per subscriber"
        );
    }
}

both_tokio_flavors!(mixed_multilevel_fanout, mixed_multilevel_fanout_scenario);

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
        let mut builder =
            ComputationGraph::builder("fanin").fixture_sink(Box::new(CollectSink::new(
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                inputs,
                received.clone(),
            )));
        for (producer, base) in [("left", 10), ("right", 20)] {
            let producer_id = named_component(producer);
            let outputs = [(0, 30), (2, 30), (7, 10), (8, 20)]
                .into_iter()
                .enumerate()
                .map(|(index, (sequence, seconds))| {
                    let root = root(
                        &producer_id,
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
                        .append_context(annotation(&producer_id, producer))
                        .expect("source context"),
                    )
                })
                .collect();
            builder = builder
                .fixture_source(Box::new(FiniteSource::new(producer_id.clone(), outputs)))
                .bind_stream(endpoint(&producer_id, "out"), stream(producer))
                .connect(
                    EdgeDefinition::new(
                        endpoint(&producer_id, "out"),
                        endpoint(
                            &drasi_core_types::ComponentId::try_new(
                                "00000000-0000-0000-0000-000000000065",
                            )
                            .expect("fixture UUID"),
                            if separate_ports { producer } else { "in" },
                        ),
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
                assert_contributors(&event.envelope, &[(named_component(producer), producer)]);
            }
        }
    }
}

both_tokio_flavors!(fanin_per_stream_fifo, fanin_scenario);

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

fn assert_fault(error: &GraphError, producer: &str, fault: Fault) {
    match (error.underlying(), fault) {
        (
            GraphError::Contract(ContractError::SchemaMismatch { expected, actual }),
            Fault::Schema,
        ) => {
            assert_eq!(**expected, schema_descriptor());
            assert_eq!(**actual, named_schema("test.other-reading"));
        }
        (
            GraphError::Emission {
                component: actual,
                reason,
            },
            fault,
        ) => {
            assert_eq!(actual, &named_component(producer));
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
        let input = root(
            &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                .expect("fixture UUID"),
            "source",
            10,
            &[1],
        );
        let mut outputs = Vec::new();
        let needs_prefix = matches!(fault, Fault::EqualSequence | Fault::RegressingSequence);
        if needs_prefix {
            outputs.push(output(input.clone()));
        }
        outputs.push(invalid_emission("source", fault, &input));
        let sends = Arc::new(AtomicUsize::new(0));
        let received = Received::default();
        let mut graph = ComputationGraph::builder("invalid-source")
            .fixture_source(Box::new(FiniteSource::new(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                outputs,
            )))
            .fixture_sink(Box::new(CollectSink::new(
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                &["in"],
                received,
            )))
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
                Box::new(AuditedBoundedPipe {
                    sends: sends.clone(),
                }),
            )
            .build()
            .expect("valid descriptors");
        let run = graph.start().expect("graph starts");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            let mut observed = control.subscribe_observed();
            observed
                .wait_for(|state| state.components[&component(77)].failure.is_some())
                .await
                .unwrap();
            let failed = control.observed().components[&component(77)]
                .failure
                .clone()
                .unwrap();
            assert_fault(&failed.cause, "source", fault);
            assert_eq!(failed.disposition, FailureDisposition::Terminal);
            assert_eq!(control.state(), GraphState::Running);
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
        assert_eq!(
            sends.load(Ordering::SeqCst),
            usize::from(needs_prefix),
            "{fault:?}"
        );
    }
}

both_tokio_flavors!(invalid_source_outputs, invalid_source_scenario);

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
        let transform = NativeTransform::new(
            drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                .expect("fixture UUID"),
            move |input| {
                Ok(vec![
                    output(derived(
                        &input.envelope,
                        &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                            .expect("fixture UUID"),
                        "transform",
                        10,
                        changes("transform", 10, &[1]),
                    )),
                    invalid_emission("transform", fault, &input.envelope),
                    output(derived(
                        &input.envelope,
                        &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-0000000001ad")
                            .expect("fixture UUID"),
                        "transform",
                        12,
                        changes("transform", 12, &[3]),
                    )),
                ])
            },
        );
        let sends = Arc::new(AtomicUsize::new(0));
        let received = Received::default();
        let mut graph = ComputationGraph::builder("invalid-transform-batch")
            .fixture_source(Box::new(FiniteSource::new(
                drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                    .expect("fixture UUID"),
                vec![output(root(
                    &drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-00000000004d")
                        .expect("fixture UUID"),
                    "source",
                    1,
                    &[7],
                ))],
            )))
            .fixture_transformer(Box::new(transform))
            .fixture_sink(Box::new(CollectSink::new(
                drasi_lib::ComponentId::try_new("00000000-0000-0000-0000-000000000065")
                    .expect("fixture UUID"),
                "sink",
                &["in"],
                received.clone(),
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
                Box::new(AuditedBoundedPipe {
                    sends: sends.clone(),
                }),
            )
            .build()
            .expect("valid descriptors");
        let run = graph.start().expect("graph starts");
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            let mut observed = control.subscribe_observed();
            observed
                .wait_for(|state| state.components[&component(429)].failure.is_some())
                .await
                .unwrap();
            let failed = control.observed().components[&component(429)]
                .failure
                .clone()
                .unwrap();
            assert_fault(&failed.cause, "transform", fault);
            assert_eq!(failed.disposition, FailureDisposition::Terminal);
            assert_eq!(control.state(), GraphState::Running);
            control.cancel();
        });
        assert!(matches!(result, Err(GraphError::Cancelled)));
        assert_eq!(
            sends.load(Ordering::SeqCst),
            0,
            "{fault:?}: batch prefix must not be sent"
        );
        assert!(received.lock().expect("sink lock").is_empty());
    }
}

both_tokio_flavors!(
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
