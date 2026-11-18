// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The separately built native OpenTelemetry source library, loaded through
//! the public Host SDK and driven over real OTLP/gRPC and OTLP/HTTP.
//!
//! Requires the native plugin libraries:
//! make build-test-plugins
//! cargo test --locked -p drasi-host-sdk --test native_otel_loopback

#[path = "common/hooks.rs"]
mod hooks;

use anyhow::{Context, Result};
use drasi_computation_source_otel::proto::collector::metrics::v1::{
    metrics_service_client::MetricsServiceClient, ExportMetricsServiceRequest,
};
use drasi_computation_source_otel::proto::collector::trace::v1::{
    trace_service_client::TraceServiceClient, ExportTraceServiceRequest,
};
use drasi_computation_source_otel::proto::common::v1::{any_value, AnyValue, KeyValue};
use drasi_computation_source_otel::proto::metrics::v1::{
    metric, number_data_point, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics,
};
use drasi_computation_source_otel::proto::resource::v1::Resource;
use drasi_computation_source_otel::proto::trace::v1::{span, ResourceSpans, ScopeSpans, Span};
use drasi_core::models::SourceChange;
use drasi_host_sdk::computation::{self, NativeComponentProxy, NativePlugin};
use drasi_lib::computation::v1::*;
use prost::Message;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

const DEADLINE: Duration = Duration::from_secs(10);
const OTEL_SOURCE: &str = "drasi.network/otel-source";
const PACKAGE: &str = "drasi-computation-source-otel";
const SOURCE_ID: &str = "2f6c7a52-8d2e-5b0b-9a51-3c1e0f4b7d19";
const TOKEN: &str = "0123456789abcdef-token";

fn plugin() -> Result<Arc<NativePlugin>> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .context("workspace")?
        .to_owned();
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| workspace.join("target"));
    let target = if target.is_absolute() {
        target
    } else {
        workspace.join(target)
    };
    let directory = std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| target.join("debug/plugins"));
    let path = directory.join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        PACKAGE.replace('-', "_"),
        std::env::consts::DLL_SUFFIX,
    ));
    anyhow::ensure!(
        path.is_file(),
        "missing {}; first run make build-test-plugins",
        path.display()
    );
    let plugin = computation::load(path)?;
    let exported: Vec<String> = plugin
        .factories()
        .iter()
        .map(|factory| factory.metadata().implementation.name.as_ref().to_owned())
        .collect();
    anyhow::ensure!(
        exported == [OTEL_SOURCE] && plugin.metadata().plugin.id.as_ref() == PACKAGE,
        "{PACKAGE} must expose exactly {OTEL_SOURCE} under its own plugin identity, not {exported:?}"
    );
    Ok(plugin)
}

fn source(configuration: Value) -> Result<NativeComponentProxy> {
    let plugin = plugin()?;
    plugin.factories()[0].create_component_with_inputs(
        ComponentId::try_new(SOURCE_ID)?,
        configuration,
        Vec::new(),
    )
}

fn free_port() -> Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port())
}

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.into())),
        }),
    }
}

fn service(name: &str) -> Option<Resource> {
    Some(Resource {
        attributes: vec![kv("service.name", name)],
        ..Default::default()
    })
}

fn latency(value: f64) -> ExportMetricsServiceRequest {
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: service("checkout"),
            scope_metrics: vec![ScopeMetrics {
                metrics: vec![Metric {
                    name: "latency_p99_ms".into(),
                    unit: "ms".into(),
                    data: Some(metric::Data::Gauge(Gauge {
                        data_points: vec![NumberDataPoint {
                            value: Some(number_data_point::Value::AsDouble(value)),
                            time_unix_nano: 1_700_000_000_000_000_000,
                            ..Default::default()
                        }],
                    })),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

fn dependency() -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: service("checkout"),
            scope_spans: vec![ScopeSpans {
                spans: vec![Span {
                    kind: span::SpanKind::Client as i32,
                    attributes: vec![kv("peer.service", "payments")],
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

fn bearer<T>(message: T) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.metadata_mut().insert(
        "authorization",
        format!("Bearer {TOKEN}").parse().expect("header"),
    );
    request
}

/// Receives changes until `count` have arrived, checking the volatile
/// producer identity of every envelope.
async fn receive(source: &mut NativeComponentProxy, count: usize) -> Result<Vec<SourceChange>> {
    let mut changes = Vec::new();
    while changes.len() < count {
        let output = tokio::time::timeout(DEADLINE, source.next())
            .await??
            .context("unexpected EOF")?;
        assert_eq!(output.port.as_str(), "out");
        let progress = GraphProducerProgress::from_envelope(&output.envelope)?
            .context("real producer identity")?;
        assert!(!progress.identity().persistent());
        changes.extend(GraphChangeCodec::decode_changes(&output.envelope)?);
    }
    Ok(changes)
}

fn summary(changes: &[SourceChange]) -> Vec<(&'static str, String)> {
    changes
        .iter()
        .map(|change| {
            let kind = match change {
                SourceChange::Insert { .. } => "insert",
                SourceChange::Update { .. } => "update",
                SourceChange::Delete { .. } => "delete",
                _ => "other",
            };
            (kind, change.get_reference().element_id.to_string())
        })
        .collect()
}

#[test]
fn the_loaded_library_publishes_its_configuration() -> Result<()> {
    let plugin = plugin()?;
    let factory = &plugin.factories()[0];
    let details = factory
        .configuration_details()
        .context("missing configuration descriptions")?;
    assert_eq!(
        details.fields.len(),
        factory.metadata().configuration.fields.len()
    );
    let schema = factory.configuration_schema();
    assert_eq!(schema["properties"]["host"]["default"], "127.0.0.1");
    assert_eq!(schema["properties"]["grpcPort"]["default"], 4317);
    assert_eq!(schema["properties"]["httpPort"]["default"], 4318);
    assert_eq!(schema["properties"]["ingressCapacity"]["minimum"], 8);
    assert!(schema["properties"].get("port").is_none());
    for secret in ["authToken", "authTokenSecondary", "tlsPrivateKeyPem"] {
        assert!(
            factory.metadata().configuration.fields[secret].secret,
            "{secret} must be a secret field"
        );
    }
    assert!(factory.metadata().capabilities.management);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn otlp_exports_reach_the_graph_through_the_loaded_library() -> Result<()> {
    let (grpc_port, http_port) = (free_port()?, free_port()?);
    let mut source = source(json!({
        "stream": "otel/out", "protocols": ["grpc", "http"],
        "grpcPort": grpc_port, "httpPort": http_port, "authToken": TOKEN,
        "metricAllowlist": ["latency_*"], "dependencyTtlSecs": 1
    }))?;
    source.initialize(&hooks::initialize_ctx()).await?;
    source.start(&hooks::start_ctx()).await?;

    let endpoint = format!("http://127.0.0.1:{grpc_port}");
    let unauthenticated = MetricsServiceClient::connect(endpoint.clone())
        .await?
        .export(latency(1.0))
        .await
        .expect_err("a bearer token is required");
    assert_eq!(unauthenticated.code(), tonic::Code::Unauthenticated);
    let response = MetricsServiceClient::connect(endpoint.clone())
        .await?
        .export(bearer(latency(920.0)))
        .await?
        .into_inner();
    assert_eq!(response.partial_success, None);
    let http = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{http_port}/v1/metrics"))
        .header("content-type", "application/x-protobuf")
        .bearer_auth(TOKEN)
        .body(latency(700.0).encode_to_vec())
        .send()
        .await?;
    assert_eq!(http.status().as_u16(), 200);
    assert_eq!(
        summary(&receive(&mut source, 6).await?),
        vec![
            ("insert", "svc:checkout".into()),
            ("insert", "metric:svc:checkout:latency_p99_ms".into()),
            (
                "insert",
                "reports:metric:svc:checkout:latency_p99_ms".into()
            ),
            ("update", "svc:checkout".into()),
            ("update", "metric:svc:checkout:latency_p99_ms".into()),
            (
                "update",
                "reports:metric:svc:checkout:latency_p99_ms".into()
            ),
        ]
    );

    // A dependency is inserted, then deleted by the sweeper once its TTL elapses.
    TraceServiceClient::connect(endpoint.clone())
        .await?
        .export(bearer(dependency()))
        .await?;
    let changes = receive(&mut source, 4).await?;
    assert_eq!(
        summary(&changes)[3],
        ("delete", "dep:svc:checkout:svc:payments".into())
    );

    // Pause refuses exports on both transports.
    source.pause(&hooks::pause_ctx()).await?;
    let paused = MetricsServiceClient::connect(endpoint.clone())
        .await?
        .export(bearer(latency(1.0)))
        .await
        .expect_err("paused");
    assert_eq!(paused.code(), tonic::Code::Unavailable);
    let paused = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{http_port}/v1/metrics"))
        .header("content-type", "application/x-protobuf")
        .bearer_auth(TOKEN)
        .body(latency(1.0).encode_to_vec())
        .send()
        .await?;
    assert_eq!(paused.status().as_u16(), 503);
    source.resume(&hooks::resume_ctx()).await?;

    source.stop(&hooks::stop_ctx()).await?;
    assert!(
        MetricsServiceClient::connect(endpoint).await.is_err(),
        "the listener closed on stop"
    );
    source.dispose(&hooks::erase_ctx()).await?;
    Ok(())
}
