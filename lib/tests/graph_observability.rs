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
    ComponentName,
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
        fn description(&self) -> ComponentDescription {
            ComponentDescription::undeclared()
        }
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.0
        }
        drasi_lib::passive_component_lifecycle!();
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
        fn description(&self) -> ComponentDescription {
            ComponentDescription::undeclared()
        }
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.0
        }
        drasi_lib::passive_component_lifecycle!();
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
            id: id("query"),
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
            .source(
                name("source"),
                Box::new(Source(
                    ComponentDescriptor::try_new(
                        id("source"),
                        vec![PortDescriptor::new(
                            port("out"),
                            PortDirection::Output,
                            input_schema,
                            PipeRequirements::default(),
                        )],
                    )
                    .unwrap(),
                    Some(OutputEnvelope::new(port("out"), input)),
                )),
            )
            .transformer(name("query"), Box::new(query))
            .sink(
                name("sink"),
                Box::new(Sink(
                    ComponentDescriptor::try_new(
                        id("sink"),
                        vec![PortDescriptor::new(
                            port("in"),
                            PortDirection::Input,
                            output_schema,
                            PipeRequirements::default(),
                        )],
                    )
                    .unwrap(),
                )),
            )
            .bind_stream(endpoint(&id("source"), "out"), stream("source"))
            .bind_stream(endpoint(&id("query"), "out"), stream("query"))
            .connect(
                edge(&id("source"), &id("query")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                edge(&id("query"), &id("sink")),
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
    for name in [
        "query.process",
        "query.prepare_input",
        "query.evaluate",
        "query.prepare_output",
        "query.commit",
        "query.finalize",
        "query.confirm_outputs",
    ] {
        assert_eq!(
            spans.iter().filter(|span| span.name == name).count(),
            1,
            "{name}"
        );
    }
    assert!(!spans.iter().any(|span| span.name == "component.process"));
    assert!(!spans
        .iter()
        .any(|span| span.name == "component.confirm_outputs"));
    assert_eq!(
        metrics.count(
            "drasi.component.calls",
            &[("component_id", id("query").as_str()), ("operation", "confirm_outputs")]
        ),
        1
    );
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
            &[("component_id", id("query").as_str())]
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
        fn description(&self) -> ComponentDescription {
            ComponentDescription::undeclared()
        }
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        drasi_lib::passive_component_lifecycle!();
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
                .source(
                    name("source"),
                    Box::new(FiniteSource::new(
                        id("source"),
                        vec![output(root(&id("source"), "source", 1, &[1]))],
                    )),
                )
                .transformer(
                    name("pending"),
                    Box::new(Pending {
                        descriptor: descriptor(&id("pending"), &["in"], &["out"]),
                        entered: entered.clone(),
                        cancel,
                    }),
                )
                .sink(
                    name("sink"),
                    Box::new(CollectSink::new(id("sink"), "sink", &["in"], Arc::default())),
                )
                .bind_stream(endpoint(&id("source"), "out"), stream("source"))
                .bind_stream(endpoint(&id("pending"), "out"), stream("pending"))
                .connect(
                    edge(&id("source"), &id("pending")),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .connect(
                    edge(&id("pending"), &id("sink")),
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
                                graph.components[&id("pending")].failure.is_some()
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
                &[("component_id", id("pending").as_str())]
            ),
            0
        );
        assert_eq!(
            metrics.count(
                "drasi.component.outputs_emitted",
                &[("component_id", id("pending").as_str())]
            ),
            0
        );
        let outcome = if cancel { "cancelled" } else { "error" };
        let spans = spans.0.lock().unwrap();
        let processing: Vec<_> = spans
            .iter()
            .filter(|span| span.name == "transformer.process")
            .collect();
        assert_eq!(processing.len(), 1);
        assert_eq!(
            processing[0].fields.get("outcome").map(String::as_str),
            Some(outcome)
        );
        let durations = metrics.matching(
            "drasi.component.operation.duration",
            &[
                ("component_id", id("pending").as_str()),
                ("operation", "transform"),
                ("outcome", outcome),
            ],
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
        fn description(&self) -> ComponentDescription {
            ComponentDescription::undeclared()
        }
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        fn requires_readiness_confirmation(&self) -> bool {
            true
        }
        fn bind_control(&mut self, control: ComponentControl) {
            *self.control.lock().unwrap() = Some(control);
        }
        drasi_lib::passive_component_lifecycle!(@hook initialize, InitializeContext);
        async fn start(&mut self, _: &StartContext) -> anyhow::Result<()> {
            self.started.notify_one();
            Ok(())
        }
        drasi_lib::passive_component_lifecycle!(@hook pause, PauseContext);
        drasi_lib::passive_component_lifecycle!(@hook resume, ResumeContext);
        drasi_lib::passive_component_lifecycle!(@hook stop, StopContext);
        async fn dispose(&mut self, context: &DisposeContext) -> anyhow::Result<()> {
            anyhow::ensure!(
                !context.erases_owned_state(),
                "this component does not support permanent state removal"
            );
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
        .service(
            name("service"),
            Box::new(ReadyService {
                descriptor: descriptor(&id("service"), &[], &[]),
                control: binding.clone(),
                started: started.clone(),
            }),
        )
        .build()
        .unwrap();
    let run = graph.start().unwrap();
    let control = run.control();
    let (result, ()) = tokio::join!(run, async {
        started.notified().await;
        assert_eq!(
            metrics.matching(
                "drasi.component.up",
                &[("component_id", id("service").as_str())]
            )[0]
            .gauge,
            0.0
        );
        let sender = binding.lock().unwrap().clone().unwrap();
        sender.ready().unwrap();
        let mut observed = control.subscribe_observed();
        tokio::time::timeout(
            Duration::from_secs(5),
            observed.wait_for(|graph| graph.components[&id("service")].started),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            metrics.matching(
                "drasi.component.up",
                &[("component_id", id("service").as_str())]
            )[0]
            .gauge,
            1.0
        );
        sender.not_ready().unwrap();
        assert_eq!(
            metrics.matching(
                "drasi.component.up",
                &[("component_id", id("service").as_str())]
            )[0]
            .gauge,
            0.0
        );
        sender.ready().unwrap();
        assert_eq!(
            metrics.matching(
                "drasi.component.up",
                &[("component_id", id("service").as_str())]
            )[0]
            .gauge,
            1.0
        );
        control.cancel();
    });
    assert!(matches!(result, Err(GraphError::Cancelled)));
    graph.shutdown().await.unwrap();
    assert_eq!(
        metrics.matching(
            "drasi.component.up",
            &[("component_id", id("service").as_str())]
        )[0]
        .gauge,
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
            .source(
                name("source"),
                Box::new(FiniteSource::new(
                    id("source"),
                    vec![output(root(&id("source"), "source", 1, &[1]))],
                )),
            )
            .sink(
                name("sink"),
                Box::new(CollectSink::new(id("sink"), "sink", &["in"], Arc::default())),
            )
            .bind_stream(endpoint(&id("source"), "out"), stream("source"))
            .connect(
                edge(&id("source"), &id("sink")),
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
        fn description(&self) -> ComponentDescription {
            ComponentDescription::undeclared()
        }
        fn descriptor(&self) -> &ComponentDescriptor {
            &self.descriptor
        }
        drasi_lib::passive_component_lifecycle!();
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
            .create_indexes("admission-observations", &id("ingress"))
            .await
            .unwrap(),
        codec,
        "ingress",
        QosRecoveryOptions::Admission(AdmissionOptions {
            construction_scope: "admission-observations".into(),
            graph_id: "admission-observations".into(),
            component_id: id("source"),
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
            .source(
                name("source"),
                Box::new(Admitted {
                    descriptor: descriptor(&id("source"), &[], &["out"]),
                    admission: admission.clone(),
                }),
            )
            .sink(
                name("sink"),
                Box::new(CollectSink::new(id("sink"), "sink", &["in"], received.clone())),
            )
            .bind_stream(endpoint(&id("source"), "out"), stream("source"))
            .connect(
                edge(&id("source"), &id("sink")),
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
                    .register_producer(id("client"))
                    .await
                    .unwrap();
                assert_eq!(
                    metrics.count("drasi.component.calls", &[("operation", "publish")]),
                    0
                );
                let input = root(&id("client"), "client", 1, &[1]);
                let receipt = admission.admit(&session, 1, &input).await.unwrap();
                while channel.progress().await.unwrap().processed["sink"] < receipt.position {
                    tokio::task::yield_now().await;
                }
                let duplicate = admission.admit(&session, 1, &input).await.unwrap();
                assert_eq!(receipt, duplicate);
                assert!(admission
                    .admit(&session, 1, &root(&id("client"), "client", 1, &[2]))
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
            &[("component_id", id("source").as_str()), ("operation", "publish")]
        ),
        3
    );
    assert_eq!(metrics.count("drasi.source.events_dispatched", &[]), 2);
    assert_eq!(
        metrics.count(
            "drasi.component.outputs_emitted",
            &[("component_id", id("source").as_str())]
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

/// Explicit fixture UUID per label; the label doubles as the component's name.
fn id(label: &str) -> ComponentId {
    component(match label {
        "source" => 1,
        "query" => 2,
        "sink" => 3,
        "pending" => 4,
        "service" => 5,
        "transform" => 6,
        "one" => 7,
        "two" => 8,
        "client" => 9,
        "ingress" => 10,
        other => panic!("missing explicit fixture component ID for {other}"),
    })
}

fn name(label: &str) -> ComponentName {
    ComponentName::try_new(label).expect("fixture component name")
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
    fn description(&self) -> ComponentDescription {
        ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

impl Burst {
    fn outputs(&mut self) -> Vec<OutputEnvelope> {
        (0..self.count)
            .map(|_| {
                self.sequence += 1;
                output(root(&id("transform"), "transform", self.sequence, &[1]))
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
            .source(
                name("source"),
                Box::new(FiniteSource::new(
                    id("source"),
                    vec![output(root(&id("source"), "source", 1, &[7]))],
                )),
            )
            .transformer(
                name("transform"),
                Box::new(Burst {
                    descriptor: descriptor(&id("transform"), &["in"], &["out"]),
                    count,
                    pending: false,
                    continuation,
                    sequence: 0,
                }),
            )
            .sink(
                name("one"),
                Box::new(CollectSink::new(id("one"), "one", &["in"], collected.clone())),
            )
            .sink(
                name("two"),
                Box::new(CollectSink::new(id("two"), "two", &["in"], collected.clone())),
            )
            .bind_stream(endpoint(&id("source"), "out"), stream("source"))
            .bind_stream(endpoint(&id("transform"), "out"), stream("transform"))
            .connect(
                edge(&id("source"), &id("transform")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                edge(&id("transform"), &id("one")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                edge(&id("transform"), &id("two")),
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
            &[("component_id", id("transform").as_str()), ("operation", "transform")]
        ),
        1
    );
    assert_eq!(
        metrics.count(
            "drasi.component.calls",
            &[
                ("component_id", id("transform").as_str()),
                ("operation", "continue_transform")
            ]
        ),
        u64::from(continuation)
    );
    assert_eq!(
        metrics.count(
            "drasi.component.calls",
            &[
                ("component_id", id("transform").as_str()),
                ("operation", "confirm_outputs")
            ]
        ),
        attempts as u64
    );
    assert_eq!(
        metrics.count(
            "drasi.component.inputs_processed",
            &[("component_id", id("transform").as_str()), ("completion", "transformed")]
        ),
        1
    );
    assert_eq!(
        metrics.count(
            "drasi.component.outputs_emitted",
            &[("component_id", id("transform").as_str())]
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
            ("component_id", id("transform").as_str()),
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
        assert_eq!(number("transformer.process"), attempts);
        assert_eq!(number("transformer.confirm_outputs"), attempts);
        assert_eq!(number("component.confirm_outputs"), 0);
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
            .source(
                name("source"),
                Box::new(FiniteSource::new(
                    id("source"),
                    vec![output(root(&id("source"), "source", 1, &[1]))],
                )),
            )
            .sink(
                name("sink"),
                Box::new(CollectSink::new(id("sink"), "sink", &["in"], Arc::default())),
            )
            .bind_stream(endpoint(&id("source"), "out"), stream("source"))
            .connect(
                edge(&id("source"), &id("sink")),
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
        .source(
            name("source"),
            Box::new(FiniteSource::new(
                id("source"),
                vec![output(root(&id("source"), "source", 1, &[1]))],
            )),
        )
        .sink(
            name("one"),
            Box::new(CollectSink::new(id("one"), "one", &["in"], received.clone())),
        )
        .sink(
            name("two"),
            Box::new(CollectSink::new(id("two"), "two", &["in"], received.clone())),
        )
        .bind_stream(endpoint(&id("source"), "out"), stream("source"))
        .connect(
            edge(&id("source"), &id("one")),
            Box::new(channel.definition().pipe(resource.clone(), "one")),
        )
        .connect(
            edge(&id("source"), &id("two")),
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
            &[("component_id", id("source").as_str())]
        ),
        1
    );
    graph.dispose().await.unwrap();
}

/// Stays pending, so the graph keeps running while the test drives its control.
struct IdleSource(ComponentDescriptor);

#[async_trait]
impl ComputationComponent for IdleSource {
    fn description(&self) -> ComponentDescription {
        ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSource for IdleSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

fn named_pipeline(metrics: &RecordingMetrics) -> ComputationGraph {
    ComputationGraph::builder("names")
        .with_telemetry(settings(TelemetryProfile::Basic, false, metrics))
        .source(
            name("source"),
            Box::new(IdleSource(descriptor(&id("source"), &[], &["out"]))),
        )
        .sink(
            name("one"),
            Box::new(CollectSink::new(id("one"), "one", &["in"], Arc::default())),
        )
        .bind_stream(endpoint(&id("source"), "out"), stream("source"))
        .connect(
            edge(&id("source"), &id("one")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .unwrap()
}

fn component_info(metrics: &RecordingMetrics, component: &str, component_name: &str) -> Vec<f64> {
    metrics
        .matching(
            "drasi.component.info",
            &[
                ("component_id", id(component).as_str()),
                ("component_name", component_name),
            ],
        )
        .into_iter()
        .map(|values| values.gauge)
        .collect()
}

#[tokio::test]
async fn component_info_reports_current_names_and_zeroes_retired_ones() {
    let metrics = RecordingMetrics::default();
    let mut graph = named_pipeline(&metrics);
    let run = graph.run().unwrap();
    let control = run.control();
    let (_, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        assert_eq!(component_info(&metrics, "source", "source"), vec![1.0]);
        assert_eq!(component_info(&metrics, "one", "one"), vec![1.0]);
        assert!(metrics
            .labels("drasi.component.info")
            .iter()
            .all(|labels| labels.contains_key("component_kind")));

        control
            .rename_component(&id("one"), name("renamed"))
            .await
            .unwrap();
        assert_eq!(component_info(&metrics, "one", "one"), vec![0.0]);
        assert_eq!(component_info(&metrics, "one", "renamed"), vec![1.0]);

        let preview = control
            .preview(
                control.desired_snapshot().revision,
                vec![DesiredMutation::RemoveComponents {
                    selection: GraphSelection::Exact(vec![id("one")]),
                    policy: RemovalPolicy::Cascade,
                }],
            )
            .await
            .unwrap();
        control
            .reconcile(preview, TopologyBindings::default())
            .await
            .unwrap();
        assert_eq!(component_info(&metrics, "one", "renamed"), vec![0.0]);
        assert_eq!(component_info(&metrics, "source", "source"), vec![1.0]);
        control.cancel();
    });
    graph.dispose().await.unwrap();
}

#[tokio::test]
async fn component_info_rename_to_an_overlong_name_still_succeeds() {
    let metrics = RecordingMetrics::default();
    let mut graph = named_pipeline(&metrics);
    let run = graph.run().unwrap();
    let control = run.control();
    let long = "a".repeat(1_100);
    let (_, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        control.rename_component(&id("one"), name(&long)).await.unwrap();
        assert_eq!(
            control
                .desired_snapshot()
                .component_by_id(&id("one"))
                .unwrap()
                .name
                .as_str(),
            long
        );
        assert_eq!(component_info(&metrics, "one", "one"), vec![0.0]);
        assert!(metrics
            .labels("drasi.component.info")
            .iter()
            .all(|labels| labels.get("component_name") != Some(&long)));
        control.cancel();
    });
    graph.dispose().await.unwrap();
}

fn edge_info(metrics: &RecordingMetrics) -> Vec<(BTreeMap<String, String>, f64)> {
    metrics
        .labels("drasi.queue.edge_info")
        .into_iter()
        .map(|labels| {
            let selection: Vec<(&str, &str)> = labels
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str()))
                .collect();
            let gauge = metrics.matching("drasi.queue.edge_info", &selection)[0].gauge;
            (labels, gauge)
        })
        .collect()
}

#[tokio::test]
async fn queue_edge_info_names_both_ends_of_each_bound_edge() {
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
    let resource = ResourceId::try_new("physical-channel").unwrap();
    let mut graph = ComputationGraph::builder("multicast-edges")
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
        .source(
            name("source"),
            Box::new(IdleSource(descriptor(&id("source"), &[], &["out"]))),
        )
        .sink(
            name("one"),
            Box::new(CollectSink::new(id("one"), "one", &["in"], Arc::default())),
        )
        .sink(
            name("two"),
            Box::new(CollectSink::new(id("two"), "two", &["in"], Arc::default())),
        )
        .bind_stream(endpoint(&id("source"), "out"), stream("source"))
        .connect(
            edge(&id("source"), &id("one")),
            Box::new(channel.definition().pipe(resource.clone(), "one")),
        )
        .connect(
            edge(&id("source"), &id("two")),
            Box::new(channel.definition().pipe(resource, "two")),
        )
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (_, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let edges = edge_info(&metrics);
        assert_eq!(edges.len(), 2, "{edges:?}");
        let queue = &edges[0].0["queue_id"];
        let mut targets = Vec::new();
        for (labels, gauge) in &edges {
            assert_eq!(&labels["queue_id"], queue);
            assert_eq!(labels["from_component_id"], id("source").as_str());
            assert_eq!(*gauge, 1.0);
            targets.push(labels["to_component_id"].clone());
        }
        targets.sort();
        let mut expected = vec![id("one").to_string(), id("two").to_string()];
        expected.sort();
        assert_eq!(targets, expected);
        control.cancel();
    });
    graph.dispose().await.unwrap();
    assert!(edge_info(&metrics).iter().all(|(_, gauge)| *gauge == 0.0));
}

#[tokio::test]
async fn queue_edge_info_reports_plain_edge_queues() {
    let metrics = RecordingMetrics::default();
    let mut graph = named_pipeline(&metrics);
    let run = graph.run().unwrap();
    let control = run.control();
    let (_, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let edges = edge_info(&metrics);
        assert_eq!(edges.len(), 1, "{edges:?}");
        let (labels, gauge) = &edges[0];
        assert!(labels["queue_id"].starts_with("edge:"), "{labels:?}");
        assert_eq!(labels["from_component_id"], id("source").as_str());
        assert_eq!(labels["to_component_id"], id("one").as_str());
        assert_eq!(*gauge, 1.0);
        control.cancel();
    });
    graph.dispose().await.unwrap();
}

#[tokio::test]
async fn component_info_is_retired_when_the_graph_is_disposed() {
    let metrics = RecordingMetrics::default();
    let mut graph = named_pipeline(&metrics);
    let run = graph.run().unwrap();
    let control = run.control();
    let (_, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        control.cancel();
    });
    assert_eq!(component_info(&metrics, "one", "one"), vec![1.0]);
    graph.dispose().await.unwrap();
    assert_eq!(component_info(&metrics, "source", "source"), vec![0.0]);
    assert_eq!(component_info(&metrics, "one", "one"), vec![0.0]);
}

#[tokio::test]
async fn component_info_is_retired_when_the_graph_is_dropped_without_dispose() {
    let metrics = RecordingMetrics::default();
    let mut graph = named_pipeline(&metrics);
    let run = graph.run().unwrap();
    let control = run.control();
    let (_, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        control.cancel();
    });
    drop(control);
    drop(graph);
    assert_eq!(component_info(&metrics, "one", "one"), vec![0.0]);
}

#[tokio::test]
async fn queue_edge_info_reports_each_producer_of_a_shared_input_queue() {
    let metrics = RecordingMetrics::default();
    let queue = ResourceId::try_new("shared-input").unwrap();
    let ranked = |rank| RankedInputPipeConfig {
        queue: queue.clone(),
        capacity: 8,
        source_rank: rank,
        source_id: None,
        source_dispatch: None,
        drop_when_full: false,
    };
    let mut graph = ComputationGraph::builder("fan-in")
        .with_telemetry(settings(TelemetryProfile::Basic, false, &metrics))
        .declare_resource(ResourceSpecification {
            id: queue.clone(),
            role: ResourceRole::Pipe,
            ownership: ResourceOwnership::Borrowed,
            binding: "shared-input".into(),
        })
        .unwrap()
        .provide_resource(queue.clone(), RankedInputQueue::new(8).unwrap().resource())
        .unwrap()
        .source(
            name("source"),
            Box::new(IdleSource(descriptor(&id("source"), &[], &["out"]))),
        )
        .source(
            name("client"),
            Box::new(IdleSource(descriptor(&id("client"), &[], &["out"]))),
        )
        .sink(
            name("one"),
            Box::new(CollectSink::new(id("one"), "one", &["in"], Arc::default())),
        )
        .bind_stream(endpoint(&id("source"), "out"), stream("source"))
        .bind_stream(endpoint(&id("client"), "out"), stream("client"))
        .connect(edge(&id("source"), &id("one")), Box::new(ranked(0)))
        .connect(edge(&id("client"), &id("one")), Box::new(ranked(1)))
        .build()
        .unwrap();
    let run = graph.run().unwrap();
    let control = run.control();
    let (_, ()) = tokio::join!(run, async {
        control.deployment_report().await.unwrap();
        let edges = edge_info(&metrics);
        assert_eq!(edges.len(), 2, "{edges:?}");
        let mut producers = Vec::new();
        for (labels, gauge) in &edges {
            assert_eq!(labels["queue_id"], "resource:shared-input");
            assert_eq!(labels["to_component_id"], id("one").as_str());
            assert_eq!(*gauge, 1.0);
            producers.push(labels["from_component_id"].clone());
        }
        producers.sort();
        let mut expected = vec![id("source").to_string(), id("client").to_string()];
        expected.sort();
        assert_eq!(producers, expected);
        control.cancel();
    });
    graph.dispose().await.unwrap();
}
