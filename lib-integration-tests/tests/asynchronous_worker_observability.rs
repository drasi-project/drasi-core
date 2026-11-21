// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(feature = "otel-context")]

use drasi_lib::{
    observability::{
        ComponentLogRuntime, ExecutionContext, ExecutionScope, LogPolicy,
        OpenTelemetryContextAdapter, TelemetryOptions, TelemetryProfile, TelemetrySettings,
        TraceContextV1, TELEMETRY_TARGET,
    },
    ComponentStatus, DrasiLib, Query,
};
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use opentelemetry::trace::TracerProvider;
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};
use std::{sync::Arc, time::Duration};
use tracing::instrument::WithSubscriber;
use tracing_subscriber::{filter::filter_fn, prelude::*};

async fn ready(drasi: &DrasiLib, id: &str) {
    let mut events = drasi.subscribe_all_component_events();
    if drasi
        .get_graph()
        .await
        .unwrap()
        .nodes
        .iter()
        .any(|node| node.id == id && node.status == ComponentStatus::Running)
    {
        return;
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let event = events.recv().await.unwrap();
            if event.component_id == id && event.status == ComponentStatus::Running {
                return;
            }
        }
    })
    .await
    .unwrap();
}

fn field<'a>(span: &'a SpanData, name: &str) -> Option<&'a opentelemetry::Value> {
    span.attributes
        .iter()
        .rev()
        .find(|field| field.key.as_str() == name)
        .map(|field| &field.value)
}

async fn ordinary_workers(instance: &str) {
    let exporter = InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(exporter.clone())
        .build();
    let logging = ComponentLogRuntime::new(LogPolicy::new("info").unwrap()).unwrap();
    let subscriber = tracing_subscriber::registry().with(logging.layer()).with(
        tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("ordinary-workers"))
            .with_filter(filter_fn(|metadata| {
                metadata.is_span() && metadata.target() == TELEMETRY_TARGET
            })),
    );
    async {
        let (source, input) = ApplicationSource::new(
            "source",
            ApplicationSourceConfig {
                properties: Default::default(),
                durability: None,
            },
        )
        .unwrap();
        let (reaction, output) = ApplicationReactionBuilder::new("reaction")
            .with_query("query")
            .with_auto_start(true)
            .build();
        let drasi = DrasiLib::builder()
            .with_id(instance)
            .with_component_logging(logging.clone())
            .with_telemetry(
                TelemetryOptions::new(
                    TelemetrySettings::new(TelemetryProfile::Debug).with_metrics(false),
                )
                .with_context_adapter(Arc::new(OpenTelemetryContextAdapter)),
            )
            .with_source(source)
            .with_query(
                Query::cypher("query")
                    .query("MATCH (n:Item) RETURN n.name AS name")
                    .from_input("source")
                    .enable_bootstrap(false)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(reaction)
            .build()
            .await
            .unwrap();
        drasi.start().await.unwrap();
        for id in ["source", "query", "reaction"] {
            ready(&drasi, id).await;
        }
        let mut results = output
            .subscribe_with_options(Default::default())
            .await
            .unwrap();
        for (id, sampled) in [(11, true), (12, false)] {
            let context = ExecutionContext::new(
                TraceContextV1::new([id; 16], [id; 8], u8::from(sampled), None).unwrap(),
            );
            ExecutionScope::new(context)
                .with_adapter(Some(Arc::new(OpenTelemetryContextAdapter)))
                .scope(
                    input.send_node_insert(
                        format!("item-{id}"),
                        vec!["Item"],
                        PropertyMapBuilder::new()
                            .with_string("name", format!("item-{id}"))
                            .build(),
                    ),
                )
                .await
                .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(10), results.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(result.query_id, "query");
            assert!(!result.results.is_empty());
        }
        drasi.shutdown().await.unwrap();
    }
    .with_subscriber(subscriber)
    .await;
    logging.shutdown(Duration::from_secs(2)).await.unwrap();
    provider.force_flush().unwrap();
    let spans = exporter.get_finished_spans().unwrap();
    provider.shutdown().unwrap();
    let workers: Vec<_> = spans
        .iter()
        .filter(|span| span.name == "reaction.process")
        .collect();
    assert_eq!(
        workers.len(),
        1,
        "unsampled input must not turn into a sampled worker root"
    );
    let worker = workers[0];
    assert_eq!(worker.span_context.trace_id().to_bytes(), [11; 16]);
    assert_eq!(
        field(worker, "component_id"),
        Some(&opentelemetry::Value::String("reaction".into()))
    );
    assert_eq!(
        field(worker, "instance_id"),
        Some(&opentelemetry::Value::String(instance.to_owned().into()))
    );
    let acceptance = spans
        .iter()
        .find(|span| {
            span.name == "sink.handle"
                && field(span, "component_id")
                    == Some(&opentelemetry::Value::String("reaction".into()))
        })
        .unwrap();
    assert_eq!(worker.parent_span_id, acceptance.span_context.span_id());
    assert_eq!(
        field(acceptance, "completion"),
        Some(&opentelemetry::Value::String("accepted".into()))
    );
    assert!(acceptance.end_time <= worker.end_time);
    assert!(spans
        .iter()
        .any(|span| span.name == "source.publish"
            && span.span_context.trace_id().to_bytes() == [11; 16]));
}

#[tokio::test(flavor = "current_thread")]
async fn ordinary_worker_context_current_thread() {
    ordinary_workers("async-current").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ordinary_worker_context_multi_thread() {
    ordinary_workers("async-multi").await;
}
