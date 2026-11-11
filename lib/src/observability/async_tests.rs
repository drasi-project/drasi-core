// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]

use super::*;
use crate::computation::v1::*;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use std::{
    future::Future,
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::{filter::filter_fn, prelude::*};

async fn record(future: impl Future<Output = ()>) -> Vec<SpanData> {
    record_filtered(TelemetryProfile::Debug, true, future).await
}

async fn record_filtered(
    profile: TelemetryProfile,
    phase_detail: bool,
    future: impl Future<Output = ()>,
) -> Vec<SpanData> {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let config = TelemetryOptions::new(TelemetrySettings::new(profile).with_metrics(false))
        .with_instance_id("async-tests")
        .with_context_adapter(Arc::new(OpenTelemetryContextAdapter))
        .resolve("async-tests", None)
        .unwrap();
    let owner = config
        .observations(
            crate::test_helpers::component_id("query"),
            ComponentRole::Transformer,
            ComponentSemanticKind::Query,
            1,
        )
        .unwrap();
    async {
        let scope = ExecutionScope::new(Default::default())
            .with_component(
                TelemetryScope::graph("async-tests", "async-tests")
                    .unwrap()
                    .component(
                        crate::test_helpers::component_id("query"),
                        ComponentRole::Transformer,
                        ComponentSemanticKind::Query,
                    ),
                1,
            )
            .with_adapter(config.context_adapter().cloned());
        owner.scope(scope.scope(future)).await;
    }
    .with_subscriber(
        tracing_subscriber::registry().with(
            tracing_opentelemetry::layer()
                .with_tracer(provider.tracer("async-tests"))
                .with_filter(filter_fn(move |metadata| {
                    metadata.is_span()
                        && metadata.target() == TELEMETRY_TARGET
                        && (phase_detail
                            || !matches!(
                                metadata.name(),
                                "query.prepare_input" | "query.prepare_output" | "query.finalize"
                            ))
                })),
        ),
    )
    .await;
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    provider.shutdown().unwrap();
    spans
}

fn definition(text: &str) -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: "async-tests".into(),
        id: crate::test_helpers::component_id("query"),
        query: text.into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("query/out").unwrap(),
        outbox_capacity: NonZeroUsize::new(8).unwrap(),
    }
}

fn input(sequence: u64) -> InputEnvelope {
    InputEnvelope::new(
        PortId::try_new("in").unwrap(),
        GraphChangeCodec::encode_change(
            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("source", &sequence.to_string()),
                        labels: Arc::from([Arc::from("Item")]),
                        effective_from: 1000,
                    },
                    properties: ElementPropertyMap::from(
                        serde_json::json!({"name": "item", "report_time": 1000}),
                    ),
                },
            },
            StreamId::try_new("source/out").unwrap(),
            sequence,
            None,
        )
        .unwrap(),
    )
    .with_execution_context(ExecutionContext::new(
        TraceContextV1::new([1; 16], [1; 8], 1, None).unwrap(),
    ))
}

fn field<'a>(span: &'a SpanData, name: &str) -> Option<&'a opentelemetry::Value> {
    span.attributes
        .iter()
        .rev()
        .find(|field| field.key.as_str() == name)
        .map(|field| &field.value)
}

async fn observed_transform(
    query: &mut ContinuousQueryTransformer,
    input: InputEnvelope,
) -> Vec<OutputEnvelope> {
    let observation = ComponentObservations::current_or_disabled()
        .begin(
            ObservedOperation::Transform,
            &input.execution_context,
            Default::default(),
        )
        .unwrap();
    let expected = observation.context().clone();
    let output = observation
        .run_outputs(query.transform(input))
        .await
        .unwrap();
    assert!(output
        .iter()
        .all(|output| output.execution_context.trace() == expected.trace()));
    output
}

fn query_binding() -> PluginTelemetry {
    let owner = ComponentObservations::current().unwrap();
    super::plugin_telemetry::bind(
        &owner,
        owner.telemetry().scope().unwrap().clone(),
        "native-query",
        None,
    )
    .unwrap()
}

fn query_request(kind: QueryObservationKind) -> QueryObservationRequest {
    QueryObservationRequest {
        kind,
        phase: QueryObservationPhase::Steady,
        parent: None,
        change_count: (kind == QueryObservationKind::Evaluate).then_some(1),
    }
}

fn finish_query(observation: &mut dyn QueryObservation) {
    observation
        .finish(QueryObservationResult {
            outcome: QueryObservationOutcome::Success,
            result_count: None,
        })
        .unwrap();
}

#[tokio::test]
async fn native_query_actual_commit_selects_the_late_root_without_evaluator_context_leaks() {
    use drasi_core::computation::QueryOperation;

    let spans = record(async {
        let binding = query_binding();
        let mut transaction = binding
            .begin_query_observation(&query_request(QueryObservationKind::Transaction))
            .unwrap()
            .unwrap();
        let mut observer = None;
        transaction
            .in_scope(&mut || {
                observer = NativeQueryScope::current().unwrap().observer();
            })
            .unwrap();
        let observer = observer.unwrap();
        let mut root = None;
        transaction
            .in_scope(&mut || {
                root = binding
                    .begin_query_observation(&query_request(QueryObservationKind::FutureQueue))
                    .unwrap();
                root.as_ref()
                    .unwrap()
                    .in_scope(&mut || {
                        let context = ExecutionContext::current();
                        for kind in
                            [QueryObservationKind::Evaluate, QueryObservationKind::PrepareOutput]
                        {
                            let mut observation = binding
                                .begin_query_observation(&query_request(kind))
                                .unwrap()
                                .unwrap();
                            observation
                                .in_scope(&mut || {
                                    assert!(NativeQueryScope::current().is_some());
                                    if kind == QueryObservationKind::PrepareOutput {
                                        assert_eq!(
                                            ExecutionContext::current().trace(),
                                            context.trace()
                                        );
                                    }
                                })
                                .unwrap();
                            finish_query(observation.as_mut());
                            assert_eq!(ExecutionContext::current().trace(), context.trace());
                        }
                    })
                    .unwrap();
            })
            .unwrap();
        assert!(NativeQueryScope::current().is_none());
        assert!(ExecutionContext::current().is_empty());
        let mut commit = observer.start(QueryOperation::Commit, None).unwrap();
        commit.finish(true, None);
        drop(commit);
        // A metadata-only session under the same transaction is not query work.
        let mut probe = observer.start(QueryOperation::Commit, None).unwrap();
        probe.finish(true, None);
        drop(probe);
        root.as_mut()
            .unwrap()
            .finish(QueryObservationResult {
                outcome: QueryObservationOutcome::Success,
                result_count: Some(1),
            })
            .unwrap();
        finish_query(transaction.as_mut());
        drop((root, transaction));
        let empty = binding
            .begin_query_observation(&query_request(QueryObservationKind::Transaction))
            .unwrap()
            .unwrap();
        empty
            .in_scope(&mut || {
                let observer = NativeQueryScope::current().unwrap().observer().unwrap();
                let mut probe = observer.start(QueryOperation::Commit, None).unwrap();
                probe.finish(true, None);
            })
            .unwrap();
        assert!(ExecutionContext::current().is_empty());
    })
    .await;
    let root = spans
        .iter()
        .find(|span| span.name == "component.scheduled")
        .unwrap();
    assert_eq!(field(root, "work_kind"), Some(&"future_queue".into()));
    assert_eq!(field(root, "input_count"), Some(&0_i64.into()));
    assert_eq!(field(root, "output_count"), Some(&1_i64.into()));
    assert!(field(root, "result_change_count").is_none());
    for name in ["query.evaluate", "query.prepare_output", "query.commit"] {
        let children = spans
            .iter()
            .filter(|span| span.name == name)
            .collect::<Vec<_>>();
        assert_eq!(children.len(), 1, "{name}");
        assert_eq!(
            children[0].parent_span_id,
            root.span_context.span_id(),
            "{name}"
        );
    }
    assert_eq!(spans.len(), 4);
}

#[tokio::test]
async fn native_query_bootstrap_detail_and_unsampled_phase_filters_match_reference_policy() {
    for bootstrap in [false, true] {
        let spans = record(async {
            let binding = query_binding();
            let mut request = query_request(QueryObservationKind::Transaction);
            if bootstrap {
                request.phase = QueryObservationPhase::Bootstrap;
            } else {
                request.parent = Some(TraceContextV1::new([2; 16], [2; 8], 0, None).unwrap());
            }
            let transaction = binding.begin_query_observation(&request).unwrap().unwrap();
            transaction
                .in_scope(&mut || {
                    let mut root = if bootstrap {
                        let mut root_request = query_request(QueryObservationKind::Bootstrap);
                        root_request.phase = QueryObservationPhase::Bootstrap;
                        binding.begin_query_observation(&root_request).unwrap()
                    } else {
                        None
                    };
                    let mut work = || {
                        if bootstrap {
                            for _ in 0..2 {
                                binding
                                    .query_observation_event(QueryObservationEvent::Input {
                                        phase: QueryObservationPhase::Bootstrap,
                                    })
                                    .unwrap();
                            }
                        }
                        for kind in
                            [QueryObservationKind::Evaluate, QueryObservationKind::PrepareInput]
                        {
                            request.kind = kind;
                            let observation = binding.begin_query_observation(&request).unwrap();
                            assert_eq!(
                                observation.is_some(),
                                kind == QueryObservationKind::Evaluate
                            );
                            drop(observation);
                        }
                        let observer = NativeQueryScope::current().unwrap().observer();
                        assert_eq!(observer.is_some(), !bootstrap);
                        if let Some(observer) = observer {
                            let mut commit = observer
                                .start(drasi_core::computation::QueryOperation::Commit, None)
                                .unwrap();
                            commit.finish(true, None);
                        }
                    };
                    if let Some(root) = &root {
                        root.in_scope(&mut work).unwrap();
                    } else {
                        work();
                    }
                    if let Some(root) = &mut root {
                        finish_query(root.as_mut());
                    }
                })
                .unwrap();
        })
        .await;
        assert_eq!(spans.len(), usize::from(bootstrap));
        if bootstrap {
            assert_eq!(spans[0].name, "query.bootstrap");
            assert_eq!(field(&spans[0], "input_count"), Some(&2_i64.into()));
        }
    }
}

#[tokio::test]
async fn native_query_bootstrap_inputs_follow_retained_root_after_transaction_returns() {
    let spans = record(async {
        let binding = query_binding();
        let mut request = query_request(QueryObservationKind::Transaction);
        request.phase = QueryObservationPhase::Bootstrap;
        request.parent = Some(TraceContextV1::new([3; 16], [3; 8], 1, None).unwrap());
        let mut transaction = binding.begin_query_observation(&request).unwrap().unwrap();
        let mut root = None;
        transaction
            .in_scope(&mut || {
                request.kind = QueryObservationKind::Bootstrap;
                root = binding.begin_query_observation(&request).unwrap();
            })
            .unwrap();
        finish_query(transaction.as_mut());
        drop(transaction);
        assert!(NativeQueryScope::current().is_none());
        let mut root = root.unwrap();
        root.in_scope(&mut || {
            assert!(NativeQueryScope::current().is_none());
            for _ in 0..3 {
                binding
                    .query_observation_event(QueryObservationEvent::Input {
                        phase: QueryObservationPhase::Bootstrap,
                    })
                    .unwrap();
            }
        })
        .unwrap();
        binding
            .query_observation_event(QueryObservationEvent::Input {
                phase: QueryObservationPhase::Bootstrap,
            })
            .unwrap();
        finish_query(root.as_mut());
        assert!(ExecutionContext::current().is_empty());
    })
    .await;
    assert_eq!(spans.len(), 1);
    assert_eq!(spans[0].name, "query.bootstrap");
    assert_eq!(spans[0].links.len(), 1);
    assert_eq!(field(&spans[0], "input_count"), Some(&3_i64.into()));
}

#[tokio::test]
async fn query_phase_detail_is_debug_only_bounded_and_keeps_output_parentage() {
    for profile in [
        TelemetryProfile::Off,
        TelemetryProfile::Basic,
        TelemetryProfile::Debug,
        TelemetryProfile::Persistence,
    ] {
        let spans = record_filtered(profile, true, async {
            let mut query = ContinuousQueryTransformer::new(
                definition("MATCH (a:Item), (b:Item) RETURN a.name AS first, b.name AS second"),
                Arc::new(InMemoryComputationProvider),
            )
            .await
            .unwrap();
            query.start().await.unwrap();
            for sequence in 1..=2 {
                let output = observed_transform(&mut query, input(sequence)).await;
                assert_eq!(output.len(), 1);
                assert_eq!(
                    output[0].envelope.changes().operations().len(),
                    if sequence == 1 { 1 } else { 3 }
                );
                query.delivery_completed(&output).await.unwrap();
            }
            query.stop().await.unwrap();
        })
        .await;
        let processing: Vec<_> = spans
            .iter()
            .filter(|span| span.name == "query.process")
            .collect();
        assert_eq!(
            processing.len(),
            if profile == TelemetryProfile::Off {
                0
            } else {
                2
            }
        );
        for name in ["query.prepare_input", "query.prepare_output", "query.finalize"] {
            let phases: Vec<_> = spans.iter().filter(|span| span.name == name).collect();
            assert_eq!(
                phases.len(),
                if profile == TelemetryProfile::Debug {
                    2
                } else {
                    0
                },
                "{profile:?} {name}"
            );
            for phase in phases {
                let parent = processing
                    .iter()
                    .find(|parent| parent.span_context.span_id() == phase.parent_span_id)
                    .unwrap();
                assert_eq!(
                    phase.span_context.trace_id(),
                    parent.span_context.trace_id()
                );
                assert!(phase.start_time >= parent.start_time);
                assert!(phase.end_time <= parent.end_time);
                assert_eq!(
                    field(phase, "outcome"),
                    Some(&opentelemetry::Value::String("success".into()))
                );
            }
        }
        if profile == TelemetryProfile::Debug {
            for parent in processing {
                let child = |name| {
                    spans
                        .iter()
                        .find(|span| {
                            span.name == name
                                && span.parent_span_id == parent.span_context.span_id()
                        })
                        .unwrap()
                };
                assert!(
                    child("query.prepare_input").end_time <= child("query.evaluate").start_time
                );
                assert!(
                    child("query.evaluate").end_time <= child("query.prepare_output").start_time
                );
                assert!(child("query.prepare_output").end_time <= child("query.commit").start_time);
                assert!(child("query.commit").end_time <= child("query.finalize").start_time);
            }
        }
    }
}

#[tokio::test]
async fn query_phase_elapsed_waits_errors_and_cancellation_preserve_caller_context() {
    let spans = record(async {
        let owner = ComponentObservations::current_or_disabled();
        let observation = owner
            .begin(
                ObservedOperation::Transform,
                &input(1).execution_context,
                Default::default(),
            )
            .unwrap();
        let caller = observation.context().clone();
        observation
            .run(async {
                owner
                    .clone()
                    .query_phase(QueryProcessingPhase::PrepareInput, async {
                        assert_eq!(ExecutionContext::current(), caller);
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        assert_eq!(ExecutionContext::current(), caller);
                        Ok::<_, drasi_core::interface::IndexError>(())
                    })
                    .await
                    .unwrap();
                let failure = owner
                    .clone()
                    .query_phase(QueryProcessingPhase::PrepareOutput, async {
                        Err::<(), _>(drasi_core::interface::IndexError::NotSupported)
                    })
                    .await;
                assert!(failure.is_err());
                let mut pending = Box::pin(owner.clone().query_phase(
                    QueryProcessingPhase::Finalize,
                    std::future::pending::<
                        std::result::Result<(), drasi_core::interface::IndexError>,
                    >(),
                ));
                std::future::poll_fn(|cx| {
                    assert!(pending.as_mut().poll(cx).is_pending());
                    std::task::Poll::Ready(())
                })
                .await;
                drop(pending);
                assert_eq!(ExecutionContext::current(), caller);
                Ok::<_, drasi_core::interface::IndexError>(())
            })
            .await
            .unwrap();
    })
    .await;
    for (name, outcome) in [
        ("query.prepare_input", "success"),
        ("query.prepare_output", "error"),
        ("query.finalize", "cancelled"),
    ] {
        let phase = spans.iter().find(|span| span.name == name).unwrap();
        assert_eq!(
            field(phase, "outcome"),
            Some(&opentelemetry::Value::String(outcome.into()))
        );
        if outcome == "success" {
            assert!(
                phase.end_time.duration_since(phase.start_time).unwrap()
                    >= std::time::Duration::from_millis(5)
            );
        }
    }
}

#[tokio::test]
async fn query_phase_detail_does_not_create_detached_root_traces() {
    let spans = record(async {
        ComponentObservations::current_or_disabled()
            .query_phase(QueryProcessingPhase::PrepareInput, async {
                Ok::<_, drasi_core::interface::IndexError>(())
            })
            .await
            .unwrap();
    })
    .await;
    assert!(spans.is_empty());
}

#[tokio::test]
async fn query_phase_detail_preserves_unsampled_parent_decisions() {
    let spans = record(async {
        let owner = ComponentObservations::current_or_disabled();
        let parent = ExecutionContext::new(TraceContextV1::new([1; 16], [1; 8], 0, None).unwrap());
        let observation = owner
            .begin(ObservedOperation::Transform, &parent, Default::default())
            .unwrap();
        observation
            .run(async {
                for phase in [
                    QueryProcessingPhase::PrepareInput,
                    QueryProcessingPhase::PrepareOutput,
                    QueryProcessingPhase::Finalize,
                ] {
                    owner
                        .clone()
                        .query_phase(phase, async {
                            assert!(!ExecutionContext::current().trace().unwrap().sampled());
                            Ok::<_, drasi_core::interface::IndexError>(())
                        })
                        .await
                        .unwrap();
                }
                Ok::<_, drasi_core::interface::IndexError>(())
            })
            .await
            .unwrap();
    })
    .await;
    assert!(!spans.iter().any(|span| matches!(
        span.name.as_ref(),
        "query.prepare_input" | "query.prepare_output" | "query.finalize"
    )));
}

#[tokio::test]
#[ignore = "local timing characterization, not a portable performance threshold"]
async fn query_phase_enabled_overhead_characterization() {
    const INPUTS: u64 = 32;
    let mut baseline = Vec::new();
    let mut detailed = Vec::new();
    for round in 0..6 {
        for enabled in if round % 2 == 0 {
            [false, true]
        } else {
            [true, false]
        } {
            let mut elapsed = std::time::Duration::ZERO;
            let spans = record_filtered(TelemetryProfile::Debug, enabled, async {
                let mut query = ContinuousQueryTransformer::new(
                    definition("MATCH (n:Item) RETURN n.name AS name"),
                    Arc::new(InMemoryComputationProvider),
                )
                .await
                .unwrap();
                query.start().await.unwrap();
                for sequence in 1..=4 {
                    let output = observed_transform(&mut query, input(sequence)).await;
                    query.delivery_completed(&output).await.unwrap();
                }
                let started = std::time::Instant::now();
                for sequence in 5..5 + INPUTS {
                    let output = observed_transform(&mut query, input(sequence)).await;
                    query.delivery_completed(&output).await.unwrap();
                }
                elapsed = started.elapsed();
                query.stop().await.unwrap();
            })
            .await;
            let phases = spans
                .iter()
                .filter(|span| {
                    matches!(
                        span.name.as_ref(),
                        "query.prepare_input" | "query.prepare_output" | "query.finalize"
                    )
                })
                .count();
            assert_eq!(
                phases,
                if enabled {
                    3 * (INPUTS as usize + 4)
                } else {
                    0
                }
            );
            let sample = elapsed.as_secs_f64() * 1_000_000.0 / INPUTS as f64;
            if enabled {
                detailed.push(sample);
            } else {
                baseline.push(sample);
            }
        }
    }
    baseline.sort_by(f64::total_cmp);
    detailed.sort_by(f64::total_cmp);
    let median = |samples: &[f64]| (samples[2] + samples[3]) / 2.0;
    println!(
        "Local debug-build query loop, in-memory synchronous span export, six interleaved pairs: phases filtered out {:.2} us/input; phases enabled {:.2} us/input; delta {:.2} us/input. Both paths include phase gating; this is not a before/after production benchmark.",
        median(&baseline), median(&detailed), median(&detailed) - median(&baseline),
    );
}

#[tokio::test]
async fn scheduled_work_is_a_new_attempt_even_without_output_and_empty_probes_are_silent() {
    let spans = record(async {
        let mut query = ContinuousQueryTransformer::new(
            definition("MATCH (n:Item) WITH n, drasi.trueNowOrLater(datetime.realtime().epochMillis >= n.report_time + 1000, n.report_time + 1000) AS expired RETURN n.name AS name"),
            Arc::new(InMemoryComputationProvider),
        ).await.unwrap();
        query.start().await.unwrap();
        assert!(query.on_wakeup().await.unwrap().is_empty());
        let output = query.transform(input(1)).await.unwrap();
        assert_eq!(output.len(), 1);
        query.delivery_completed(&output).await.unwrap();
        let output = query.on_wakeup().await.unwrap();
        assert!(output.is_empty(), "due work should leave the projected row unchanged");
        assert!(!query.processing_context().unwrap().is_empty());
        query.delivery_completed(&output).await.unwrap();
        assert!(query.on_wakeup().await.unwrap().is_empty());
        assert!(query.processing_context().unwrap().is_empty());
        query.stop().await.unwrap();
    }).await;
    let scheduled: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "component.scheduled")
        .collect();
    assert_eq!(scheduled.len(), 1);
    let scheduled = scheduled[0];
    assert_eq!(
        scheduled.parent_span_id,
        opentelemetry::trace::SpanId::INVALID
    );
    assert!(scheduled.links.links.is_empty());
    assert_ne!(scheduled.span_context.trace_id().to_bytes(), [1; 16]);
    assert_eq!(
        field(scheduled, "work_kind"),
        Some(&opentelemetry::Value::String("future_queue".into()))
    );
    assert_eq!(
        field(scheduled, "output_count"),
        Some(&opentelemetry::Value::I64(0))
    );
    assert!(!spans.iter().any(|span| span.name == "component.process"
        || span.name == "transformer.process"
        || span.name == "query.process"));
    for name in ["query.evaluate", "query.commit"] {
        let children: Vec<_> = spans
            .iter()
            .filter(|span| {
                span.name == name && span.parent_span_id == scheduled.span_context.span_id()
            })
            .collect();
        assert_eq!(children.len(), 1, "{name}");
    }
}

#[tokio::test]
async fn worker_handling_keeps_parentage_and_batch_links_without_metrics_or_start_parents() {
    let spans = record(async {
        let worker = WorkerTelemetry::capture();
        let one = input(1).execution_context;
        let two = ExecutionContext::new(TraceContextV1::new([2; 16], [2; 8], 1, None).unwrap());
        worker
            .reaction(&one, 1)
            .unwrap()
            .run(async {
                tokio::task::yield_now().await;
                assert_ne!(ExecutionContext::current(), one);
                assert_eq!(
                    ExecutionScope::current_component()
                        .unwrap()
                        .identity
                        .component_id()
                        .unwrap()
                        .as_str(),
                    crate::test_helpers::component_id("query").as_str()
                );
                Ok::<_, anyhow::Error>(())
            })
            .await
            .unwrap();
        worker
            .reaction_batch([&one, &two], 2, 2)
            .unwrap()
            .run(async { Err::<(), _>(anyhow::anyhow!("effect failed")) })
            .await
            .unwrap_err();
        let pending = worker
            .reaction(&one, 1)
            .unwrap()
            .run(std::future::pending::<anyhow::Result<()>>());
        tokio::pin!(pending);
        assert!(futures::poll!(&mut pending).is_pending());
    })
    .await;
    let single: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "reaction.process")
        .collect();
    assert_eq!(single.len(), 2);
    assert!(single
        .iter()
        .all(|span| span.parent_span_id.to_bytes() == [1; 8]));
    assert!(single
        .iter()
        .any(|span| field(span, "outcome")
            == Some(&opentelemetry::Value::String("cancelled".into()))));
    let batch = spans
        .iter()
        .find(|span| span.name == "reaction.process_batch")
        .unwrap();
    assert_eq!(batch.parent_span_id, opentelemetry::trace::SpanId::INVALID);
    assert_eq!(batch.links.links.len(), 2);
    assert_eq!(
        field(batch, "outcome"),
        Some(&opentelemetry::Value::String("error".into()))
    );
}

struct ReplayBootstrap {
    pending: AtomicBool,
    replay: Mutex<Vec<ChangeEnvelope>>,
    preparations: Mutex<Vec<ExecutionContext>>,
}

#[async_trait::async_trait]
impl ComputationBootstrapProvider for ReplayBootstrap {
    async fn prepare(&self) -> anyhow::Result<BootstrapPreparation> {
        if self.pending.load(Ordering::Acquire) {
            let trace = BootstrapTrace::current().expect("query preparation binds lazy tracing");
            let context = trace.start()?;
            let scope = ExecutionScope::capture().with_context(context.clone());
            let work = drasi_computation_contracts::workers::spawn_scoped_worker(scope.scope(
                async move {
                    tokio::task::yield_now().await;
                    assert_eq!(ExecutionContext::current(), context);
                },
            ));
            work.await?;
            self.preparations.lock().unwrap().push(trace.start()?);
        }
        Ok(BootstrapPreparation::Ready)
    }

    fn has_pending_snapshot(&self) -> anyhow::Result<bool> {
        Ok(self.pending.load(Ordering::Acquire))
    }

    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        self.pending.store(false, Ordering::Release);
        if let Some(context) = self.preparations.lock().unwrap().last() {
            assert_eq!(
                ExecutionContext::current().trace().unwrap().trace_id(),
                context.trace().unwrap().trace_id()
            );
        }
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter([Ok(input(1).envelope)])),
            watermarks: vec![],
        })
    }

    fn take_input_replay(&self) -> anyhow::Result<Vec<ChangeEnvelope>> {
        Ok(std::mem::take(&mut *self.replay.lock().unwrap()))
    }
}

#[tokio::test]
async fn prepared_input_replay_is_not_bootstrap_and_does_not_duplicate_processing() {
    let spans = record(async {
        let bootstrap = Arc::new(ReplayBootstrap {
            pending: AtomicBool::new(true),
            replay: Mutex::new(vec![]),
            preparations: Mutex::new(vec![]),
        });
        let mut query = ContinuousQueryTransformer::new(
            definition("MATCH (n:Item) RETURN n.name AS name"),
            Arc::new(InMemoryComputationProvider),
        )
        .await
        .unwrap()
        .with_bootstrap(bootstrap.clone());
        query.start().await.unwrap();
        query.stop().await.unwrap();
        bootstrap.replay.lock().unwrap().push(input(2).envelope);
        query.start().await.unwrap();
        let outputs = query.on_wakeup().await.unwrap();
        assert_eq!(outputs.len(), 1);
        assert_eq!(
            outputs[0].execution_context,
            query.processing_context().unwrap()
        );
        query.delivery_completed(&outputs).await.unwrap();
        assert!(query.on_wakeup().await.unwrap().is_empty());
        query.stop().await.unwrap();
        bootstrap.pending.store(true, Ordering::Release);
        query.start().await.unwrap();
        query.stop().await.unwrap();
        let contexts = bootstrap.preparations.lock().unwrap();
        assert!(contexts.len() >= 2);
        assert_ne!(
            contexts[0].trace().unwrap().trace_id(),
            contexts.last().unwrap().trace().unwrap().trace_id()
        );
    })
    .await;
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.name == "query.bootstrap")
            .count(),
        2
    );
    let scheduled: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "component.scheduled")
        .collect();
    assert_eq!(scheduled.len(), 1);
    assert_eq!(
        field(scheduled[0], "work_kind"),
        Some(&opentelemetry::Value::String("input_replay".into()))
    );
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.name == "query.evaluate")
            .count(),
        1
    );
    assert!(!spans.iter().any(|span| span.name == "component.process"
        || span.name == "transformer.process"
        || span.name == "query.process"));
    let evaluation = spans
        .iter()
        .find(|span| span.name == "query.evaluate")
        .unwrap();
    assert_eq!(
        evaluation.parent_span_id,
        scheduled[0].span_context.span_id()
    );
}

#[tokio::test]
async fn service_work_is_bounded_and_idle_waiting_has_no_processing_span() {
    let spans = record(async {
        let worker = WorkerTelemetry::capture();
        for id in 1..=2 {
            let context =
                ExecutionContext::new(TraceContextV1::new([id; 16], [id; 8], 1, None).unwrap());
            worker
                .service_work(&context)
                .unwrap()
                .run(async {
                    tokio::task::yield_now().await;
                    assert_eq!(
                        ExecutionContext::current().trace().unwrap().trace_id(),
                        [id; 16]
                    );
                    Ok::<_, anyhow::Error>(())
                })
                .await
                .unwrap();
        }
        worker
            .scope(async {
                tokio::task::yield_now().await;
                assert!(ExecutionContext::current().is_empty());
            })
            .await;
    })
    .await;
    let processing: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "component.process")
        .collect();
    assert_eq!(processing.len(), 2);
    assert!(processing.iter().all(|span| field(span, "operation")
        == Some(&opentelemetry::Value::String("service_work".into()))));
}

struct PendingBootstrap;

#[async_trait::async_trait]
impl ComputationBootstrapProvider for PendingBootstrap {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::pending()),
            watermarks: vec![],
        })
    }
}

#[tokio::test]
async fn cancelled_bootstrap_closes_its_root_without_publishing_success() {
    let spans = record(async {
        let mut query = ContinuousQueryTransformer::new(
            definition("MATCH (n:Item) RETURN n.name AS name"),
            Arc::new(InMemoryComputationProvider),
        )
        .await
        .unwrap()
        .with_bootstrap(Arc::new(PendingBootstrap));
        {
            let start = query.start();
            tokio::pin!(start);
            assert!(futures::poll!(&mut start).is_pending());
        }
        assert!(BootstrapTrace::current().is_none());
        query.stop().await.unwrap();
    })
    .await;
    for name in ["query.bootstrap", "bootstrap.snapshot"] {
        let matched: Vec<_> = spans.iter().filter(|span| span.name == name).collect();
        assert_eq!(matched.len(), 1);
        assert_eq!(
            field(matched[0], "outcome"),
            Some(&opentelemetry::Value::String("cancelled".into()))
        );
        assert_eq!(
            field(matched[0], "input_count"),
            Some(&opentelemetry::Value::I64(0))
        );
    }
}

#[cfg(feature = "computation-rocksdb-tests")]
#[tokio::test]
async fn output_replay_after_reconstruction_has_new_context_but_no_evaluation() {
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    let directory = tempfile::tempdir().unwrap();
    let spans = record(async {
        let make = || {
            Arc::new(RocksDbComputationProvider::new(
                directory.path(),
                RocksIndexOptions::new(
                    false,
                    false,
                    RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).unwrap(),
                ),
            ))
        };
        let mut first = ContinuousQueryTransformer::new(
            definition("MATCH (n:Item) RETURN n.name AS name"),
            make(),
        )
        .await
        .unwrap()
        .with_delivery_tracking();
        first.start().await.unwrap();
        let output = first.transform(input(1)).await.unwrap();
        assert_eq!(output.len(), 1);
        let identity = GraphProducerProgress::from_envelope(&output[0].envelope)
            .unwrap()
            .unwrap()
            .identity()
            .clone();
        let original_context = output[0].execution_context.clone();
        first.stop().await.unwrap();
        drop(first);
        let mut recovered = ContinuousQueryTransformer::new(
            definition("MATCH (n:Item) RETURN n.name AS name"),
            make(),
        )
        .await
        .unwrap()
        .with_delivery_tracking();
        recovered.start().await.unwrap();
        let replay = recovered.on_wakeup().await.unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(
            QueryChangeCodec::query_sequence(&replay[0].envelope).unwrap(),
            1
        );
        assert_eq!(
            GraphProducerProgress::from_envelope(&replay[0].envelope)
                .unwrap()
                .unwrap()
                .identity(),
            &identity
        );
        assert!(!replay[0].execution_context.is_empty());
        assert_ne!(replay[0].execution_context, original_context);
        assert_eq!(
            replay[0].execution_context,
            recovered.processing_context().unwrap()
        );
        recovered.delivery_completed(&replay).await.unwrap();
        assert!(recovered.on_wakeup().await.unwrap().is_empty());
        recovered.stop().await.unwrap();
    })
    .await;
    assert_eq!(
        spans
            .iter()
            .filter(|span| span.name == "query.evaluate")
            .count(),
        1
    );
    let scheduled: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "component.scheduled")
        .collect();
    assert_eq!(scheduled.len(), 1);
    assert_eq!(
        field(scheduled[0], "work_kind"),
        Some(&opentelemetry::Value::String("output_replay".into()))
    );
    assert!(spans.iter().any(|span| span.name == "query.commit"
        && span.parent_span_id == scheduled[0].span_context.span_id()));
}
