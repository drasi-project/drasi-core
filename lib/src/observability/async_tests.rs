// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::*;
use drasi_core::{
    computation::InMemoryComputationProvider,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use std::{future::Future, num::NonZeroUsize, sync::Arc};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::{filter::filter_fn, prelude::*};

async fn record(future: impl Future<Output = ()>) -> Vec<SpanData> {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let config =
        TelemetryOptions::new(TelemetrySettings::new(TelemetryProfile::Debug).with_metrics(false))
            .with_instance_id("async-tests")
            .with_context_adapter(Arc::new(OpenTelemetryContextAdapter))
            .resolve("async-tests", None)
            .unwrap();
    let owner = config
        .observations(
            ComponentId::try_new("query").unwrap(),
            ComponentRole::Query,
            ComponentSemanticKind::Query,
            1,
        )
        .unwrap();
    async {
        let scope = ExecutionScope::new(Default::default())
            .with_component(
                config.scope().unwrap().component(
                    ComponentId::try_new("query").unwrap(),
                    ComponentRole::Query,
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
                .with_filter(filter_fn(|metadata| {
                    metadata.is_span() && metadata.target() == TELEMETRY_TARGET
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
        id: ComponentId::try_new("query").unwrap(),
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
    assert!(!spans.iter().any(|span| span.name == "component.process"));
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
                    "query"
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
