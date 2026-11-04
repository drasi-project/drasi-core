// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]

use async_trait::async_trait;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::{
    computation::v1::*,
    observability::{
        MetricRegistrationContext, RootSampling, TelemetryOptions, TelemetryProfile,
        TelemetrySettings,
    },
};
use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

#[allow(dead_code)]
mod computation_support;
use computation_support::{component, edge, endpoint, port, stream};
#[path = "computation_support/metrics.rs"]
mod recording;
use recording::RecordingMetrics;

struct Source {
    descriptor: ComponentDescriptor,
    inputs: VecDeque<ChangeEnvelope>,
}

#[async_trait]
impl ComputationComponent for Source {
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
impl EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self
            .inputs
            .pop_front()
            .map(|envelope| OutputEnvelope::new(port("out"), envelope)))
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    received: Arc<Mutex<Vec<ChangeEnvelope>>>,
    expected: usize,
    done: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ComputationComponent for Sink {
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
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let mut received = self.received.lock().unwrap();
        received.push(input.envelope);
        if received.len() == self.expected {
            self.done.notify_one();
        }
        Ok(())
    }
}

struct Bootstrap(Vec<ChangeEnvelope>);

#[async_trait]
impl ComputationBootstrapProvider for Bootstrap {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter(
                self.0.iter().cloned().map(Ok).collect::<Vec<_>>(),
            )),
            watermarks: Vec::new(),
        })
    }
}

fn element(id: &str, name: &str, label: &str) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: Arc::from([Arc::from(label)]),
            effective_from: 100,
        },
        properties: ElementPropertyMap::from(serde_json::json!({"name": name})),
    }
}

fn input(sequence: u64, changes: Vec<SourceChange>) -> ChangeEnvelope {
    GraphChangeCodec::encode_changes(&changes, stream("source"), sequence, None).unwrap()
}

async fn run(
    metrics: &RecordingMetrics,
    profile: TelemetryProfile,
    inputs: Vec<ChangeEnvelope>,
    bootstrap: Option<Vec<ChangeEnvelope>>,
    expected: usize,
) -> Vec<ChangeEnvelope> {
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "query-metrics".into(),
            id: component("query"),
            query: "MATCH (n:Person) RETURN n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("query"),
            outbox_capacity: NonZeroUsize::new(8).unwrap(),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await
    .unwrap();
    if let Some(bootstrap) = bootstrap {
        query = query.with_bootstrap(Arc::new(Bootstrap(bootstrap)));
    }
    let received = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(tokio::sync::Notify::new());
    let mut graph = ComputationGraph::builder("query-metrics")
        .with_telemetry(
            TelemetryOptions::new(
                TelemetrySettings::new(profile)
                    .with_tracing(false)
                    .with_sampling(RootSampling::Never),
            )
            .with_instance_id("instance")
            .with_metric_context(
                MetricRegistrationContext::new(Arc::new(metrics.clone()), Default::default())
                    .unwrap(),
            ),
        )
        .source(Box::new(Source {
            descriptor: ComponentDescriptor::try_new(
                component("source"),
                vec![PortDescriptor::new(
                    port("out"),
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    Default::default(),
                )],
            )
            .unwrap(),
            inputs: inputs.into(),
        }))
        .query(Box::new(query))
        .sink(Box::new(Sink {
            descriptor: ComponentDescriptor::try_new(
                component("sink"),
                vec![PortDescriptor::new(
                    port("in"),
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    Default::default(),
                )],
            )
            .unwrap(),
            received: received.clone(),
            expected,
            done: done.clone(),
        }))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("query", "out"), stream("query"))
        .connect(
            edge("source", "query"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("query", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(run, async {
            let mut observed = control.subscribe_observed();
            observed
                .wait_for(|graph| {
                    graph
                        .components
                        .get(&component("query"))
                        .is_some_and(|query| query.lifecycle == ComponentLifecycle::Running)
                })
                .await
                .unwrap();
            if expected > 0 {
                done.notified().await;
            }
            control.cancel();
        })
    })
    .await
    .unwrap();
    assert!(matches!(result, Ok(()) | Err(GraphError::Cancelled)));
    graph.shutdown().await.unwrap();
    let received = received.lock().unwrap().clone();
    received
}

#[tokio::test]
async fn counts_input_envelopes_and_new_result_rows_without_tracing() {
    let metrics = RecordingMetrics::default();
    let changes = vec![
        input(
            1,
            vec![
                SourceChange::Insert {
                    element: element("a", "Alice", "Person"),
                },
                SourceChange::Insert {
                    element: element("b", "Bob", "Person"),
                },
            ],
        ),
        input(
            2,
            vec![SourceChange::update(element("a", "Alicia", "Person"))],
        ),
        input(
            3,
            vec![SourceChange::Insert {
                element: element("other", "Other", "Other"),
            }],
        ),
        input(
            4,
            vec![SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", "b"),
                    labels: Arc::from([Arc::from("Person")]),
                    effective_from: 200,
                },
            }],
        ),
    ];
    let output = run(&metrics, TelemetryProfile::Persistence, changes, None, 3).await;
    assert_eq!(output.len(), 3);
    assert_eq!(
        metrics.count("drasi.query.events_processed", &[("phase", "steady")]),
        4
    );
    assert_eq!(
        metrics.count("drasi.query.results_emitted", &[("change_kind", "add")]),
        2
    );
    assert_eq!(
        metrics.count("drasi.query.results_emitted", &[("change_kind", "update")]),
        1
    );
    assert_eq!(
        metrics.count("drasi.query.results_emitted", &[("change_kind", "delete")]),
        1
    );
    assert_eq!(
        metrics
            .histogram("drasi.query.engine.duration", &[("phase", "steady")])
            .len(),
        4
    );
    assert!(metrics
        .gauge_samples("drasi.query.live_results", &[])
        .contains(&1.0));
    assert!(metrics
        .gauge_samples("drasi.query.outbox.entries", &[])
        .contains(&3.0));
    assert!(metrics
        .matching("drasi.pipeline.end_to_end.duration", &[])
        .is_empty());
}

#[tokio::test]
async fn bootstrap_evaluates_rows_without_claiming_result_stream_emission() {
    for rows in [0, 1, 25] {
        let metrics = RecordingMetrics::default();
        let bootstrap = (1..=rows)
            .map(|row| {
                input(
                    row,
                    vec![SourceChange::Insert {
                        element: element(&row.to_string(), "Name", "Person"),
                    }],
                )
            })
            .collect();
        run(
            &metrics,
            TelemetryProfile::Persistence,
            Vec::new(),
            Some(bootstrap),
            0,
        )
        .await;
        assert_eq!(
            metrics.count("drasi.query.events_processed", &[("phase", "bootstrap")]),
            rows
        );
        assert_eq!(
            metrics
                .histogram("drasi.query.engine.duration", &[("phase", "bootstrap")])
                .len(),
            rows as usize
        );
        assert_eq!(metrics.count("drasi.query.results_emitted", &[]), 0);
        assert!(metrics
            .gauge_samples("drasi.query.live_results", &[])
            .contains(&(rows as f64)));
    }
}

#[tokio::test]
async fn off_keeps_the_recorder_empty() {
    let metrics = RecordingMetrics::default();
    run(
        &metrics,
        TelemetryProfile::Off,
        vec![input(
            1,
            vec![SourceChange::Insert {
                element: element("a", "Alice", "Person"),
            }],
        )],
        None,
        1,
    )
    .await;
    assert!(metrics.is_empty());
}
