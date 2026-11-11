// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use async_trait::async_trait;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::{computation::v1::*, observability::*};
use std::{collections::VecDeque, num::NonZeroUsize, sync::Arc, time::Duration};
use tokio::sync::{Notify, Semaphore};

#[allow(dead_code)]
mod computation_support;
pub(crate) use computation_support::component;
use computation_support::{edge, endpoint, name, port, stream};
#[path = "computation_support/metrics.rs"]
mod recording;
use recording::RecordingMetrics;

const LATENCY: &str = "drasi.pipeline.end_to_end.duration";

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
            .map(|input| OutputEnvelope::new(port("out"), input)))
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    completion: SinkCompletion,
    entered: Arc<Notify>,
    permits: Arc<Semaphore>,
    finished: Arc<Notify>,
    fail: bool,
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
        self.completion
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        assert_eq!(input.envelope.changes().operations().len(), 2);
        self.entered.notify_one();
        self.permits.acquire().await?.forget();
        self.finished.notify_one();
        anyhow::ensure!(!self.fail, "controlled sink failure");
        Ok(())
    }
}

fn descriptor(
    id: &str,
    direction: PortDirection,
    schema: &SchemaDescriptor,
) -> ComponentDescriptor {
    ComponentDescriptor::try_new(
        component(id),
        vec![PortDescriptor::new(
            port(if direction == PortDirection::Output {
                "out"
            } else {
                "in"
            }),
            direction,
            schema.clone(),
            Default::default(),
        )],
    )
    .expect("valid handled-latency component descriptor")
}

fn input() -> ChangeEnvelope {
    input_sequence(1)
}

fn input_sequence(sequence: u64) -> ChangeEnvelope {
    let changes = ["a", "b"].map(|id| SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", &format!("{id}-{sequence}")),
                labels: Arc::from([Arc::from("Person")]),
                effective_from: 100,
            },
            properties: ElementPropertyMap::from(serde_json::json!({"name": id})),
        },
    });
    GraphChangeCodec::encode_changes(&changes, stream("source"), sequence, None)
        .expect("encode handled-latency input changes")
}

async fn query() -> ContinuousQueryTransformer {
    ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "handled-latency".into(),
            id: component("query"),
            query: "MATCH (n:Person) RETURN n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("query"),
            outbox_capacity: NonZeroUsize::new(8).expect("nonzero test outbox capacity"),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await
    .expect("construct handled-latency query")
}

fn options(metrics: &RecordingMetrics, enabled: bool, excluded: bool) -> TelemetryOptions {
    let mut settings = TelemetrySettings::new(TelemetryProfile::Basic)
        .with_tracing(false)
        .with_sampling(RootSampling::Never);
    if enabled {
        settings = settings.with_handled_branch_latency(Default::default());
    }
    if excluded {
        settings = settings.exclude_metrics("end_to_end");
    }
    TelemetryOptions::new(settings)
        .with_instance_id("instance")
        .with_metric_context(
            MetricRegistrationContext::new(Arc::new(metrics.clone()), Default::default())
                .expect("valid handled-latency metric context"),
        )
}

type Gate = (Arc<Notify>, Arc<Semaphore>, Arc<Notify>);
fn sink(id: &str, completion: SinkCompletion, fail: bool) -> (Sink, Gate) {
    let gate = (
        Arc::new(Notify::new()),
        Arc::new(Semaphore::new(0)),
        Arc::new(Notify::new()),
    );
    (
        Sink {
            descriptor: descriptor(
                id,
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor(),
            ),
            completion,
            fail,
            entered: gate.0.clone(),
            permits: gate.1.clone(),
            finished: gate.2.clone(),
        },
        gate,
    )
}

#[tokio::test(start_paused = true)]
async fn exact_fan_out_duration_is_per_envelope_and_sink_without_tracing() {
    let metrics = RecordingMetrics::default();
    let (one, a) = sink("a", SinkCompletion::Handled, false);
    let (two, b) = sink("b", SinkCompletion::Handled, false);
    let mut graph = ComputationGraph::builder("handled-latency")
        .with_telemetry(options(&metrics, true, false))
        .source(
            name("source"),
            Box::new(Source {
                descriptor: descriptor(
                    "source",
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor(),
                ),
                inputs: [input()].into(),
            }),
        )
        .transformer(name("query"), Box::new(query().await))
        .sink(name("a"), Box::new(one))
        .sink(name("b"), Box::new(two))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("query", "out"), stream("query"))
        .connect(
            edge("source", "query"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("query", "a"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("query", "b"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        tokio::join!(a.0.notified(), b.0.notified());
        assert!(metrics.histogram(LATENCY, &[]).is_empty());
        tokio::time::advance(Duration::from_millis(100)).await;
        a.1.add_permits(1);
        metrics
            .wait_for(|| {
                metrics
                    .histogram(LATENCY, &[("component_id", component("a").as_str())])
                    .len()
                    == 1
            })
            .await;
        let query = component("query");
        let query_label = ("query_id", query.as_str());
        assert_eq!(
            metrics.histogram(
                LATENCY,
                &[("component_id", component("a").as_str()), query_label, ("phase", "steady")]
            ),
            [0.100]
        );
        assert!(metrics
            .histogram(LATENCY, &[("component_id", component("b").as_str())])
            .is_empty());
        tokio::time::advance(Duration::from_millis(150)).await;
        b.1.add_permits(1);
        metrics
            .wait_for(|| {
                metrics
                    .histogram(LATENCY, &[("component_id", component("b").as_str())])
                    .len()
                    == 1
            })
            .await;
        assert_eq!(
            metrics.histogram(LATENCY, &[("component_id", component("b").as_str())]),
            [0.250]
        );
        control.cancel();
    });
    assert!(matches!(result, Ok(()) | Err(GraphError::Cancelled)));
    graph.shutdown().await.unwrap();
    assert_eq!(metrics.histogram(LATENCY, &[]).len(), 2);
}

#[tokio::test]
async fn disabled_excluded_and_acceptance_only_paths_never_record_success_latency() {
    for (enabled, excluded, completion) in [
        (false, false, SinkCompletion::Handled),
        (true, true, SinkCompletion::Handled),
        (true, false, SinkCompletion::Accepted),
    ] {
        let metrics = RecordingMetrics::default();
        let (sink, gate) = sink("sink", completion, false);
        let mut graph = ComputationGraph::builder("handled-latency")
            .with_telemetry(options(&metrics, enabled, excluded))
            .source(
                name("source"),
                Box::new(Source {
                    descriptor: descriptor(
                        "source",
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor(),
                    ),
                    inputs: [input()].into(),
                }),
            )
            .transformer(name("query"), Box::new(query().await))
            .sink(name("sink"), Box::new(sink))
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
        assert_eq!(
            graph
                .telemetry()
                .handled_branch_diagnostics()
                .unwrap()
                .is_some(),
            enabled && !excluded
        );
        let run = graph.start().unwrap();
        let control = run.control();
        let (result, ()) = tokio::join!(run, async {
            gate.0.notified().await;
            gate.1.add_permits(1);
            metrics
                .wait_for(|| {
                    metrics.count(
                        "drasi.component.inputs_processed",
                        &[("component_id", component("sink").as_str())],
                    ) == 1
                })
                .await;
            control.cancel();
        });
        assert!(matches!(result, Ok(()) | Err(GraphError::Cancelled)));
        graph.shutdown().await.unwrap();
        assert!(metrics.matching(LATENCY, &[]).is_empty());
    }
}

#[tokio::test]
async fn failed_handler_never_records_success_latency() {
    let metrics = RecordingMetrics::default();
    let (sink, gate) = sink("sink", SinkCompletion::Handled, true);
    let mut graph = ComputationGraph::builder("handled-latency")
        .with_telemetry(options(&metrics, true, false))
        .source(
            name("source"),
            Box::new(Source {
                descriptor: descriptor(
                    "source",
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor(),
                ),
                inputs: [input()].into(),
            }),
        )
        .transformer(name("query"), Box::new(query().await))
        .sink(name("sink"), Box::new(sink))
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
    let (result, ()) = tokio::join!(run, async {
        gate.0.notified().await;
        gate.1.add_permits(1);
        metrics
            .wait_for(|| {
                metrics.count(
                    "drasi.errors",
                    &[("component_id", component("sink").as_str())],
                ) == 1
            })
            .await;
        control.cancel();
    });
    assert!(result.is_err());
    assert!(metrics.histogram(LATENCY, &[]).is_empty());
    graph.shutdown().await.unwrap();
}

#[allow(dead_code)]
pub(crate) async fn native_transformer_pipeline(
    transformer: Box<dyn Transformer>,
    expected_sample: bool,
) {
    let metrics = RecordingMetrics::default();
    let (sink, gate) = sink("sink", SinkCompletion::Handled, false);
    let mut graph = ComputationGraph::builder("handled-latency")
        .with_telemetry(options(&metrics, true, false))
        .source(
            name("source"),
            Box::new(Source {
                descriptor: descriptor(
                    "source",
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor(),
                ),
                inputs: [input()].into(),
            }),
        )
        .transformer(name("query"), Box::new(query().await))
        .transformer(name("native"), transformer)
        .sink(name("sink"), Box::new(sink))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("query", "out"), stream("query"))
        .bind_stream(endpoint("native", "out"), stream("native"))
        .connect(
            edge("source", "query"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("query", "native"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            edge("native", "sink"),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("build native handled-latency graph");
    let run = graph.start().expect("start native handled-latency graph");
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        if tokio::time::timeout(Duration::from_secs(10), gate.0.notified())
            .await
            .is_err()
        {
            let observed = control.observed();
            let failures = observed
                .components
                .iter()
                .filter_map(|(id, component)| {
                    component.failure.as_ref().map(|failure| (id, failure))
                })
                .collect::<Vec<_>>();
            panic!("native transformer did not deliver its retained input: {failures:?}");
        }
        assert!(metrics.histogram(LATENCY, &[]).is_empty());
        tokio::time::advance(Duration::from_millis(123)).await;
        gate.1.add_permits(1);
        if expected_sample {
            metrics
                .wait_for(|| !metrics.histogram(LATENCY, &[]).is_empty())
                .await;
            assert_eq!(
                metrics.histogram(LATENCY, &[("query_id", component("query").as_str())]),
                [0.123]
            );
        } else {
            metrics
                .wait_for(|| {
                    metrics.count(
                        "drasi.component.inputs_processed",
                        &[("component_id", component("sink").as_str())],
                    ) == 1
                })
                .await;
            assert!(metrics.histogram(LATENCY, &[]).is_empty());
        }
        control.cancel();
    });
    assert!(matches!(result, Ok(()) | Err(GraphError::Cancelled)));
    graph
        .shutdown()
        .await
        .expect("shut down native handled-latency graph");
}

struct DrivenSource {
    descriptor: ComponentDescriptor,
    receive: tokio::sync::mpsc::Receiver<ChangeEnvelope>,
}
#[async_trait]
impl ComputationComponent for DrivenSource {
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
impl EnvelopeSource for DrivenSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self
            .receive
            .recv()
            .await
            .map(|input| OutputEnvelope::new(port("out"), input)))
    }
}
struct BatchSink(Sink);
#[async_trait]
impl ComputationComponent for BatchSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.0.descriptor()
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for BatchSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn batching(&self) -> anyhow::Result<Option<SinkBatching>> {
        Ok(Some(SinkBatching {
            max_inputs: 2,
            max_operations: 4,
            max_bytes: 1024 * 1024,
            max_delay_ms: 1000,
        }))
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.0.handle(input).await
    }
    async fn handle_batch(&mut self, inputs: Vec<InputEnvelope>) -> anyhow::Result<()> {
        assert_eq!(inputs.len(), 2);
        self.0.handle(inputs[0].clone()).await
    }
}

#[tokio::test(start_paused = true)]
async fn combined_batch_preserves_distinct_ingress_starts_and_excludes_source_wait() {
    let metrics = RecordingMetrics::default();
    let (sink, gate) = sink("sink", SinkCompletion::Handled, false);
    let (send, receive) = tokio::sync::mpsc::channel(2);
    let mut graph = ComputationGraph::builder("handled-latency")
        .with_telemetry(options(&metrics, true, false))
        .source(
            name("source"),
            Box::new(DrivenSource {
                descriptor: descriptor(
                    "source",
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor(),
                ),
                receive,
            }),
        )
        .transformer(name("query"), Box::new(query().await))
        .sink(name("sink"), Box::new(BatchSink(sink)))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("query", "out"), stream("query"))
        .connect(
            edge("source", "query"),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .connect(
            edge("query", "sink"),
            Box::new(BoundedPipeConfig { capacity: 2 }),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        // Source polling/receipt waiting happens before the timing origin exists.
        tokio::time::advance(Duration::from_secs(10)).await;
        send.send(input_sequence(1)).await.unwrap();
        metrics
            .wait_for(|| metrics.count("drasi.query.events_processed", &[]) == 1)
            .await;
        tokio::time::advance(Duration::from_millis(50)).await;
        send.send(input_sequence(2)).await.unwrap();
        gate.0.notified().await;
        assert!(metrics.histogram(LATENCY, &[]).is_empty());
        tokio::time::advance(Duration::from_millis(200)).await;
        gate.1.add_permits(1);
        metrics
            .wait_for(|| metrics.histogram(LATENCY, &[]).len() == 2)
            .await;
        assert_eq!(metrics.histogram(LATENCY, &[]), [0.250, 0.200]);
        control.cancel();
    });
    assert!(matches!(result, Ok(()) | Err(GraphError::Cancelled)));
    graph.shutdown().await.unwrap();
}
