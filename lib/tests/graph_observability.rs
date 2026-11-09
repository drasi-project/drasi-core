// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]

#[allow(dead_code)]
mod computation_support;

use async_trait::async_trait;
use computation_support::*;
use drasi_lib::{
    computation::v1::*,
    observability::{
        MetricRegistrationContext, TelemetryOptions, TelemetryProfile, TelemetrySettings,
        TELEMETRY_TARGET,
    },
};
#[path = "computation_support/metrics.rs"]
mod recording_metrics;
use recording_metrics::RecordingMetrics;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tracing::instrument::WithSubscriber;
use tracing::{
    field::{Field, Visit},
    span::{Attributes, Id, Record},
    Subscriber,
};
use tracing_subscriber::{layer::Context, prelude::*, registry::LookupSpan, Layer};

#[tokio::test]
async fn directly_constructed_query_uses_real_evaluator_and_commit_boundaries() {
    use drasi_core::{
        computation::InMemoryComputationProvider,
        models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    };
    struct Source(ComponentDescriptor, Option<OutputEnvelope>);
    #[async_trait]
    impl ComputationComponent for Source {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.0
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
            Ok(self.1.take())
        }
    }
    struct Sink(ComponentDescriptor);
    #[async_trait]
    impl ComputationComponent for Sink {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.0
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
            assert_eq!(input.envelope.changes().operations().len(), 1);
            Ok(())
        }
    }
    let metrics = RecordingMetrics::default();
    let spans = RecordingSpans::default();
    let query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: "query-boundaries".into(),
            id: component("query"),
            query: "MATCH (n:Person) RETURN n.name AS name".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("query"),
            outbox_capacity: std::num::NonZeroUsize::new(8).unwrap(),
        },
        Arc::new(InMemoryComputationProvider),
    )
    .await
    .unwrap();
    let input_schema = GraphChangeCodec::schema().descriptor().clone();
    let output_schema = query
        .descriptor()
        .ports()
        .iter()
        .find(|port| port.direction() == PortDirection::Output)
        .unwrap()
        .schema()
        .clone();
    let input = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("people", "alice"),
                    labels: Arc::from([Arc::from("Person")]),
                    effective_from: 100,
                },
                properties: ElementPropertyMap::from(serde_json::json!({"name": "Alice"})),
            },
        },
        stream("source"),
        1,
        None,
    )
    .unwrap();
    async {
        let mut graph = ComputationGraph::builder("query-boundaries")
            .with_telemetry(settings(TelemetryProfile::Debug, true, &metrics))
            .source(Box::new(Source(
                ComponentDescriptor::try_new(
                    component("source"),
                    vec![PortDescriptor::new(
                        port("out"),
                        PortDirection::Output,
                        input_schema,
                        PipeRequirements::default(),
                    )],
                )
                .unwrap(),
                Some(OutputEnvelope::new(port("out"), input)),
            )))
            .query(Box::new(query))
            .sink(Box::new(Sink(
                ComponentDescriptor::try_new(
                    component("sink"),
                    vec![PortDescriptor::new(
                        port("in"),
                        PortDirection::Input,
                        output_schema,
                        PipeRequirements::default(),
                    )],
                )
                .unwrap(),
            )))
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
        graph.start().unwrap().await.unwrap();
        graph.dispose().await.unwrap();
    }
    .with_subscriber(tracing_subscriber::registry().with(spans.clone()))
    .await;
    let spans = spans.0.lock().unwrap();
    for name in ["query.process", "query.evaluate", "query.commit"] {
        assert_eq!(
            spans.iter().filter(|span| span.name == name).count(),
            1,
            "{name}"
        );
    }
    assert!(!spans.iter().any(|span| span.name == "component.process"));
    let evaluation = spans
        .iter()
        .find(|span| span.name == "query.evaluate")
        .unwrap();
    assert_eq!(
        evaluation.fields.get("change_count").map(String::as_str),
        Some("1")
    );
    assert_eq!(
        evaluation
            .fields
            .get("result_change_count")
            .map(String::as_str),
        Some("1")
    );
    assert_eq!(
        metrics.count(
            "drasi.component.inputs_processed",
            &[("component_id", "query")]
        ),
        1
    );
    assert_eq!(metrics.count("drasi.query.events_processed", &[]), 1);
    assert_eq!(
        metrics.count("drasi.query.results_emitted", &[("change_kind", "add")]),
        1
    );
    assert_eq!(
        metrics.histogram("drasi.query.engine.duration", &[]).len(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn processing_failure_and_cancellation_have_one_local_outcome() {
    struct Pending {
        descriptor: ComponentDescriptor,
        entered: Arc<tokio::sync::Notify>,
        cancel: bool,
    }
    #[async_trait]
    impl ComputationComponent for Pending {
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
    impl Transformer for Pending {
        async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
            self.entered.notify_one();
            if self.cancel {
                std::future::pending().await
            } else {
                anyhow::bail!("processing rejected")
            }
        }
    }
    for cancel in [false, true] {
        let metrics = RecordingMetrics::default();
        let spans = RecordingSpans::default();
        let entered = Arc::new(tokio::sync::Notify::new());
        async {
            let mut graph = ComputationGraph::builder("interrupted")
                .with_telemetry(settings(TelemetryProfile::Debug, true, &metrics))
                .source(Box::new(FiniteSource::new(
                    "source",
                    vec![output(root("source", 1, &[1]))],
                )))
                .transformer(Box::new(Pending {
                    descriptor: descriptor("pending", &["in"], &["out"]),
                    entered: entered.clone(),
                    cancel,
                }))
                .sink(Box::new(CollectSink::new("sink", &["in"], Arc::default())))
                .bind_stream(endpoint("source", "out"), stream("source"))
                .bind_stream(endpoint("pending", "out"), stream("pending"))
                .connect(
                    edge("source", "pending"),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .connect(
                    edge("pending", "sink"),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .build()
                .unwrap();
            let run = graph.start().unwrap();
            let control = run.control();
            let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(run, async {
                    entered.notified().await;
                    if cancel {
                        tokio::time::advance(Duration::from_millis(17)).await;
                    } else {
                        let mut observed = control.subscribe_observed();
                        observed
                            .wait_for(|graph| {
                                graph.components[&component("pending")].failure.is_some()
                            })
                            .await
                            .unwrap();
                    }
                    control.cancel();
                })
            })
            .await
            .unwrap();
            assert!(result.is_err());
            graph.shutdown().await.unwrap();
        }
        .with_subscriber(tracing_subscriber::registry().with(spans.clone()))
        .await;
        assert_eq!(metrics.count("drasi.errors", &[]), u64::from(!cancel));
        assert_eq!(
            metrics.count(
                "drasi.component.inputs_processed",
                &[("component_id", "pending")]
            ),
            0
        );
        assert_eq!(
            metrics.count(
                "drasi.component.outputs_emitted",
                &[("component_id", "pending")]
            ),
            0
        );
        let outcome = if cancel { "cancelled" } else { "error" };
        let spans = spans.0.lock().unwrap();
        let processing: Vec<_> = spans
            .iter()
            .filter(|span| span.name == "component.process")
            .collect();
        assert_eq!(processing.len(), 1);
        assert_eq!(
            processing[0].fields.get("outcome").map(String::as_str),
            Some(outcome)
        );
        let durations = metrics.matching(
            "drasi.component.operation.duration",
            &[("component_id", "pending"), ("operation", "transform"), ("outcome", outcome)],
        );
        assert_eq!(durations[0].histogram.len(), 1);
        if cancel {
            assert_eq!(durations[0].histogram, vec![0.017]);
        }
    }
}

#[tokio::test]
async fn readiness_updates_synchronously_and_idle_service_stays_up() {
    struct ReadyService {
        descriptor: ComponentDescriptor,
        control: Arc<Mutex<Option<ComponentControl>>>,
        started: Arc<tokio::sync::Notify>,
    }
    #[async_trait]
    impl ComputationComponent for ReadyService {
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        fn requires_readiness_confirmation(&self) -> bool {
            true
        }
        fn bind_control(&mut self, control: ComponentControl) {
            *self.control.lock().unwrap() = Some(control);
        }
        async fn start(&mut self) -> anyhow::Result<()> {
            self.started.notify_one();
            Ok(())
        }
        async fn stop(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }
    #[async_trait]
    impl ComputationService for ReadyService {
        async fn run(&mut self) -> anyhow::Result<()> {
            std::future::pending().await
        }
    }
    let metrics = RecordingMetrics::default();
    let binding = Arc::new(Mutex::new(None));
    let started = Arc::new(tokio::sync::Notify::new());
    let mut graph = ComputationGraph::builder("readiness")
        .with_telemetry(settings(TelemetryProfile::Basic, false, &metrics))
        .service(Box::new(ReadyService {
            descriptor: descriptor("service", &[], &[]),
            control: binding.clone(),
            started: started.clone(),
        }))
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        started.notified().await;
        assert_eq!(
            metrics.matching("drasi.component.up", &[("component_id", "service")])[0].gauge,
            0.0
        );
        let sender = binding.lock().unwrap().clone().unwrap();
        sender.ready().unwrap();
        let mut observed = control.subscribe_observed();
        tokio::time::timeout(
            Duration::from_secs(5),
            observed.wait_for(|graph| graph.components[&component("service")].started),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            metrics.matching("drasi.component.up", &[("component_id", "service")])[0].gauge,
            1.0
        );
        sender.not_ready().unwrap();
        assert_eq!(
            metrics.matching("drasi.component.up", &[("component_id", "service")])[0].gauge,
            0.0
        );
        sender.ready().unwrap();
        assert_eq!(
            metrics.matching("drasi.component.up", &[("component_id", "service")])[0].gauge,
            1.0
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.shutdown().await.unwrap();
    assert_eq!(
        metrics.matching("drasi.component.up", &[("component_id", "service")])[0].gauge,
        0.0
    );
}

#[tokio::test]
async fn supported_acknowledgements_record_physical_queue_and_input_identity() {
    let metrics = RecordingMetrics::default();
    let spans = RecordingSpans::default();
    let resource = ResourceId::try_new("retained").unwrap();
    let capacity = std::num::NonZeroUsize::new(2).unwrap();
    let store = Arc::new(MemoryEnvelopeStore::new(
        capacity,
        RetentionPolicy::Backpressure,
    ));
    async {
        let mut graph = ComputationGraph::builder("acknowledgements")
            .with_telemetry(settings(TelemetryProfile::Debug, true, &metrics))
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "retained".into(),
            })
            .unwrap()
            .provide_resource(
                resource.clone(),
                ResourceHandle::new(
                    ResourceRole::StateStore,
                    Arc::new(RetainedStoreResource(store)),
                ),
            )
            .unwrap()
            .source(Box::new(FiniteSource::new(
                "source",
                vec![output(root("source", 1, &[1]))],
            )))
            .sink(Box::new(CollectSink::new("sink", &["in"], Arc::default())))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .connect(
                edge("source", "sink"),
                Box::new(RetainedPipeConfig {
                    resource,
                    capacity,
                    durable: false,
                    retention: RetentionPolicy::Backpressure,
                    gap_policy: ReplayGapPolicy::Strict,
                }),
            )
            .build()
            .unwrap();
        graph.start().unwrap().await.unwrap();
        graph.dispose().await.unwrap();
    }
    .with_subscriber(tracing_subscriber::registry().with(spans.clone()))
    .await;
    let spans = spans.0.lock().unwrap();
    let acknowledgements: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "pipe.acknowledge")
        .collect();
    assert_eq!(acknowledgements.len(), 1);
    for (field, expected) in [
        ("queue_id", "resource:retained"),
        ("input_port", "in"),
        ("handling_outcome", "handled"),
        ("outcome", "success"),
    ] {
        assert_eq!(
            acknowledgements[0].fields.get(field).map(String::as_str),
            Some(expected)
        );
    }
}

#[cfg(feature = "computation-rocksdb-tests")]
#[tokio::test]
async fn admitted_publication_attempts_and_retries_do_not_duplicate_physical_output() {
    use drasi_core::{computation::ComputationIndexProvider, interface::FailureMode};
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    use std::num::NonZeroUsize;
    struct Admitted {
        descriptor: ComponentDescriptor,
        admission: Arc<SourceAdmission>,
    }
    #[async_trait]
    impl ComputationComponent for Admitted {
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
    impl EnvelopeSource for Admitted {
        fn admission(&self) -> Option<Arc<SourceAdmission>> {
            Some(self.admission.clone())
        }
        async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
            anyhow::bail!("admission must not poll next")
        }
    }
    let directory = tempfile::TempDir::new().unwrap();
    let provider = RocksDbComputationProvider::new(
        directory.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).unwrap(),
        ),
    );
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1 << 20).unwrap());
    codec
        .register_schema(Arc::new(Schema::new(
            schema_descriptor(),
            Arc::new(ReadingValidator(schema_descriptor())),
        )))
        .unwrap();
    let channel = QosChannel::persistent_with_recovery(
        QosChannelDefinition {
            stream: stream("source"),
            capacity: NonZeroUsize::new(4).unwrap(),
            durable: true,
            retention: RetentionPolicy::Backpressure,
            subscribers: BTreeMap::from([("sink".into(), SubscriptionStart::Earliest)]),
        },
        provider
            .create_indexes("admission-observations", "ingress")
            .await
            .unwrap(),
        codec,
        "ingress",
        QosRecoveryOptions::Admission(AdmissionOptions {
            construction_scope: "admission-observations".into(),
            graph_id: "admission-observations".into(),
            component_id: component("source"),
            failure_scope: FailureMode::ProcessRestart,
            max_producers: NonZeroUsize::new(2).unwrap(),
            receipts_per_producer: NonZeroUsize::new(4).unwrap(),
        }),
    )
    .await
    .unwrap();
    let admission = SourceAdmission::new(channel.clone(), port("out"))
        .await
        .unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let resource = ResourceId::try_new("ingress").unwrap();
    let metrics = RecordingMetrics::default();
    let spans = RecordingSpans::default();
    async {
        let mut graph = ComputationGraph::builder("admission-observations")
            .with_telemetry(settings(TelemetryProfile::Debug, true, &metrics))
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Borrowed,
                binding: "ingress".into(),
            })
            .unwrap()
            .provide_resource(resource.clone(), channel.resource())
            .unwrap()
            .source(Box::new(Admitted {
                descriptor: descriptor("source", &[], &["out"]),
                admission: admission.clone(),
            }))
            .sink(Box::new(CollectSink::new(
                "sink",
                &["in"],
                received.clone(),
            )))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .connect(
                edge("source", "sink"),
                Box::new(channel.definition().pipe(resource, "sink")),
            )
            .build()
            .unwrap();
        let run = graph.start().unwrap();
        let control = run.control();
        let (driver, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(run, async {
                control.startup_report().await.unwrap();
                admission.wait_active().await.unwrap();
                let session = admission
                    .register_producer(component("client"))
                    .await
                    .unwrap();
                assert_eq!(
                    metrics.count("drasi.component.calls", &[("operation", "publish")]),
                    0
                );
                let input = root("client", 1, &[1]);
                let receipt = admission.admit(&session, 1, &input).await.unwrap();
                while channel.progress().await.unwrap().processed["sink"] < receipt.position {
                    tokio::task::yield_now().await;
                }
                let duplicate = admission.admit(&session, 1, &input).await.unwrap();
                assert_eq!(receipt, duplicate);
                assert!(admission
                    .admit(&session, 1, &root("client", 1, &[2]))
                    .await
                    .is_err());
                admission.producer_status(&session).await.unwrap();
                admission.admission_receipt(&session, 1).await.unwrap();
                assert_eq!(received.lock().unwrap().len(), 1);
                control.cancel();
            })
        })
        .await
        .unwrap();
        assert!(driver.is_err());
        graph.shutdown().await.unwrap();
    }
    .with_subscriber(tracing_subscriber::registry().with(spans.clone()))
    .await;
    assert_eq!(
        metrics.count(
            "drasi.component.calls",
            &[("component_id", "source"), ("operation", "publish")]
        ),
        3
    );
    assert_eq!(metrics.count("drasi.source.events_dispatched", &[]), 2);
    assert_eq!(
        metrics.count(
            "drasi.component.outputs_emitted",
            &[("component_id", "source")]
        ),
        1
    );
    assert_eq!(metrics.count("drasi.errors", &[]), 1);
    {
        let spans = spans.0.lock().unwrap();
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.name == "source.publish")
                .count(),
            3
        );
        let sends: Vec<_> = spans
            .iter()
            .filter(|span| span.name == "pipe.send")
            .collect();
        assert_eq!(sends.len(), 3);
        assert_eq!(
        sends
            .iter()
            .filter(|span| span.fields.get("acceptance").map(String::as_str) == Some("accepted"))
            .count(),
        2
    );
        assert_eq!(
            sends
                .iter()
                .filter(|span| span.fields.get("acceptance").map(String::as_str)
                    == Some("not_accepted"))
                .count(),
            1
        );
    }
    channel.shutdown().await.unwrap();
}

#[derive(Default, Debug)]
struct SpanFields(BTreeMap<String, String>);

impl Visit for SpanFields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
}

#[derive(Debug)]
struct FinishedSpan {
    name: &'static str,
    fields: BTreeMap<String, String>,
}

#[derive(Default, Clone)]
struct RecordingSpans(Arc<Mutex<Vec<FinishedSpan>>>);

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for RecordingSpans {
    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        if attributes.metadata().target() == TELEMETRY_TARGET {
            let mut fields = SpanFields::default();
            attributes.record(&mut fields);
            context.span(id).unwrap().extensions_mut().insert(fields);
        }
    }
    fn on_record(&self, id: &Id, record: &Record<'_>, context: Context<'_, S>) {
        if let Some(fields) = context
            .span(id)
            .unwrap()
            .extensions_mut()
            .get_mut::<SpanFields>()
        {
            record.record(fields);
        }
    }
    fn on_close(&self, id: Id, context: Context<'_, S>) {
        let span = context.span(&id).unwrap();
        if let Some(fields) = span.extensions_mut().remove::<SpanFields>() {
            self.0.lock().unwrap().push(FinishedSpan {
                name: span.name(),
                fields: fields.0,
            });
        };
    }
}

fn settings(
    profile: TelemetryProfile,
    tracing: bool,
    metrics: &RecordingMetrics,
) -> TelemetryOptions {
    TelemetryOptions::new(TelemetrySettings::new(profile).with_tracing(tracing))
        .with_instance_id("observability-tests")
        .with_metric_context(
            MetricRegistrationContext::new(Arc::new(metrics.clone()), Default::default()).unwrap(),
        )
}

struct Burst {
    descriptor: ComponentDescriptor,
    count: usize,
    pending: bool,
    continuation: bool,
    sequence: u64,
}

#[async_trait]
impl ComputationComponent for Burst {
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

impl Burst {
    fn outputs(&mut self) -> Vec<OutputEnvelope> {
        (0..self.count)
            .map(|_| {
                self.sequence += 1;
                output(root("transform", self.sequence, &[1]))
            })
            .collect()
    }
}

#[async_trait]
impl Transformer for Burst {
    async fn transform(&mut self, _: InputEnvelope) -> anyhow::Result<Vec<OutputEnvelope>> {
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.pending = self.continuation;
        Ok(self.outputs())
    }
    fn has_pending_emissions(&self) -> bool {
        self.pending
    }
    async fn continue_transform(&mut self) -> anyhow::Result<Vec<OutputEnvelope>> {
        self.pending = false;
        Ok(self.outputs())
    }
}

async fn burst_case(count: usize, continuation: bool, tracing: bool) {
    let metrics = RecordingMetrics::default();
    let spans = RecordingSpans::default();
    let collected = Arc::new(Mutex::new(Vec::new()));
    async {
        let mut graph = ComputationGraph::builder("burst")
            .with_telemetry(settings(TelemetryProfile::Debug, tracing, &metrics))
            .source(Box::new(FiniteSource::new(
                "source",
                vec![output(root("source", 1, &[7]))],
            )))
            .transformer(Box::new(Burst {
                descriptor: descriptor("transform", &["in"], &["out"]),
                count,
                pending: false,
                continuation,
                sequence: 0,
            }))
            .sink(Box::new(CollectSink::new(
                "one",
                &["in"],
                collected.clone(),
            )))
            .sink(Box::new(CollectSink::new(
                "two",
                &["in"],
                collected.clone(),
            )))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .bind_stream(endpoint("transform", "out"), stream("transform"))
            .connect(
                edge("source", "transform"),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                edge("transform", "one"),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                edge("transform", "two"),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .build()
            .unwrap();
        graph.start().unwrap().await.unwrap();
        graph.dispose().await.unwrap();
    }
    .with_subscriber(tracing_subscriber::registry().with(spans.clone()))
    .await;
    let attempts = 1 + usize::from(continuation);
    let produced = attempts * count;
    assert_eq!(collected.lock().unwrap().len(), produced * 2);
    assert_eq!(
        metrics.count(
            "drasi.component.calls",
            &[("component_id", "transform"), ("operation", "transform")]
        ),
        1
    );
    assert_eq!(
        metrics.count(
            "drasi.component.calls",
            &[("component_id", "transform"), ("operation", "continue_transform")]
        ),
        u64::from(continuation)
    );
    assert_eq!(
        metrics.count(
            "drasi.component.inputs_processed",
            &[("component_id", "transform"), ("completion", "transformed")]
        ),
        1
    );
    assert_eq!(
        metrics.count(
            "drasi.component.outputs_emitted",
            &[("component_id", "transform")]
        ),
        produced as u64
    );
    assert_eq!(
        metrics.count(
            "drasi.component.inputs_processed",
            &[("completion", "handled")]
        ),
        (2 * produced) as u64
    );
    assert_eq!(metrics.count("drasi.source.events_dispatched", &[]), 1);
    assert_eq!(metrics.count("drasi.errors", &[]), 0);
    let durations = metrics.matching(
        "drasi.component.operation.duration",
        &[
            ("component_id", "transform"),
            ("operation", "transform"),
            ("outcome", "success"),
        ],
    );
    assert_eq!(durations.len(), 1);
    assert_eq!(durations[0].histogram.len(), 1);
    assert!(durations[0].histogram[0] >= 0.005);
    let finished = spans.0.lock().unwrap();
    let number = |name| finished.iter().filter(|span| span.name == name).count();
    if tracing {
        assert_eq!(number("source.publish"), 1);
        assert_eq!(number("component.process"), attempts);
        assert_eq!(number("component.confirm_outputs"), attempts);
        assert_eq!(number("pipe.send"), 1 + 2 * produced);
        assert_eq!(number("sink.handle"), 2 * produced);
        assert!(finished
            .iter()
            .all(|span| span.fields.get("outcome").map(String::as_str) == Some("success")));
    } else {
        assert!(finished.is_empty());
    }
    assert!(metrics
        .matching("drasi.query.engine.duration", &[])
        .is_empty());
    assert!(metrics
        .matching("drasi.pipeline.end_to_end.duration", &[])
        .is_empty());
    assert!(metrics
        .matching("drasi.component.up", &[])
        .iter()
        .all(|value| value.gauge == 0.0));
}

#[tokio::test(start_paused = true)]
async fn exact_zero_one_many_outputs_and_continuations() {
    for count in [0, 1, 3] {
        for continuation in [false, true] {
            burst_case(count, continuation, true).await;
            burst_case(count, continuation, false).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn graph_observations_on_multi_thread_runtime() {
    burst_case(3, true, true).await;
}

#[tokio::test]
async fn off_has_no_production_series_or_operation_spans() {
    let metrics = RecordingMetrics::default();
    let spans = RecordingSpans::default();
    async {
        let mut graph = ComputationGraph::builder("off")
            .with_telemetry(settings(TelemetryProfile::Off, true, &metrics))
            .source(Box::new(FiniteSource::new(
                "source",
                vec![output(root("source", 1, &[1]))],
            )))
            .sink(Box::new(CollectSink::new("sink", &["in"], Arc::default())))
            .bind_stream(endpoint("source", "out"), stream("source"))
            .connect(
                edge("source", "sink"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()
            .unwrap();
        graph.start().unwrap().await.unwrap();
        graph.dispose().await.unwrap();
    }
    .with_subscriber(tracing_subscriber::registry().with(spans.clone()))
    .await;
    assert!(metrics.is_empty());
    assert!(spans.0.lock().unwrap().is_empty());
}

#[tokio::test]
async fn retained_and_multicast_backlog_counts_physical_records_not_subscribers_or_history() {
    let metrics = RecordingMetrics::default();
    let telemetry = settings(TelemetryProfile::Basic, false, &metrics);
    let channel = QosChannel::volatile(QosChannelDefinition {
        stream: stream("source"),
        capacity: std::num::NonZeroUsize::new(8).unwrap(),
        durable: false,
        retention: RetentionPolicy::Backpressure,
        subscribers: BTreeMap::from([
            ("one".into(), SubscriptionStart::Earliest),
            ("two".into(), SubscriptionStart::Earliest),
        ]),
    })
    .unwrap();
    let received = Arc::new(Mutex::new(Vec::new()));
    let resource = ResourceId::try_new("physical-channel").unwrap();
    let mut graph = ComputationGraph::builder("multicast")
        .with_telemetry(telemetry)
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "physical-channel".into(),
        })
        .unwrap()
        .provide_resource(resource.clone(), channel.resource())
        .unwrap()
        .source(Box::new(FiniteSource::new(
            "source",
            vec![output(root("source", 1, &[1]))],
        )))
        .sink(Box::new(CollectSink::new("one", &["in"], received.clone())))
        .sink(Box::new(CollectSink::new("two", &["in"], received.clone())))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .connect(
            edge("source", "one"),
            Box::new(channel.definition().pipe(resource.clone(), "one")),
        )
        .connect(
            edge("source", "two"),
            Box::new(channel.definition().pipe(resource, "two")),
        )
        .build()
        .unwrap();
    graph.start().unwrap().await.unwrap();
    assert_eq!(received.lock().unwrap().len(), 2);
    let depth = metrics.matching("drasi.queue.depth", &[]);
    let peak = metrics.matching("drasi.queue.depth_max", &[]);
    assert_eq!(depth.len(), 1);
    assert_eq!(peak.len(), 1);
    assert_eq!(depth[0].gauge, 0.0);
    assert!(peak[0].gauge_samples.contains(&1.0));
    assert_eq!(
        metrics.count(
            "drasi.component.outputs_emitted",
            &[("component_id", "source")]
        ),
        1
    );
    graph.dispose().await.unwrap();
}
