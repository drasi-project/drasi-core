// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Factory, conformance, management and real-transport tests.

use super::*;
use crate::otlp::proto::collector::logs::v1::logs_service_client::LogsServiceClient;
use crate::otlp::proto::collector::metrics::v1::{
    metrics_service_client::MetricsServiceClient, ExportMetricsServiceRequest,
    ExportMetricsServiceResponse,
};
use crate::otlp::proto::collector::trace::v1::trace_service_client::TraceServiceClient;
use crate::otlp::proto::logs::v1::SeverityNumber;
use crate::otlp::RpcStatus;
use crate::projection::tests::{client_span, gauge, log, logs, metrics, point, resource, traces};
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{ComponentId, GraphChangeCodec};
use drasi_computation_plugin_sdk::lifecycle::{StopMode, UpdateBoundary};
use drasi_computation_plugin_sdk::testing::{
    contexts, create_source, Offer, SourceConformance, SourceProbe,
};
use drasi_computation_plugin_sdk::{NativeComponent, NativeSource};
use drasi_core_types::models::SourceChange;
use prost::Message;
use serde_json::{json, Value};
use std::io::Write;
use std::time::Duration;
use tonic::{metadata::MetadataValue, transport::Channel, Code};

const ID: &str = "6a1d2c3b-4e5f-4a6b-8c7d-9e0f1a2b3c4d";
const TOKEN: &str = "0123456789abcdef-token";

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free port")
}

fn grpc_config(port: u16) -> Value {
    json!({"stream": "otel", "grpcPort": port, "metricAllowlist": ["latency"]})
}

fn http_config(port: u16) -> Value {
    json!({"stream": "otel", "protocols": ["http"], "httpPort": port, "metricAllowlist": ["latency"]})
}

fn with(mut value: Value, extra: Value) -> Value {
    if let (Some(value), Some(extra)) = (value.as_object_mut(), extra.as_object()) {
        value.extend(extra.clone());
    }
    value
}

fn latency(service: &str, value: f64) -> ExportMetricsServiceRequest {
    metrics(
        resource(&[("service.name", service)]),
        vec![gauge("latency", vec![point(value, 0, &[])])],
    )
}

async fn channel(port: u16) -> Result<Channel, tonic::transport::Error> {
    Channel::from_shared(format!("http://127.0.0.1:{port}"))
        .expect("endpoint")
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(5))
        .connect()
        .await
}

fn authorized<T>(message: T, token: Option<&str>) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    if let Some(token) = token {
        request.metadata_mut().insert(
            "authorization",
            MetadataValue::try_from(format!("Bearer {token}")).expect("header"),
        );
    }
    request
}

async fn export_grpc(
    port: u16,
    token: Option<&str>,
    request: ExportMetricsServiceRequest,
) -> Result<Result<ExportMetricsServiceResponse, tonic::Status>> {
    let mut client = MetricsServiceClient::new(channel(port).await?);
    Ok(client
        .export(authorized(request, token))
        .await
        .map(tonic::Response::into_inner))
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .expect("client")
}

async fn post_http(
    port: u16,
    path: &str,
    body: Vec<u8>,
    headers: &[(&str, &str)],
) -> Result<reqwest::Response> {
    let mut request = http_client()
        .post(format!("http://127.0.0.1:{port}{path}"))
        .body(body);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    Ok(request.send().await?)
}

const PROTOBUF_HEADER: (&str, &str) = ("content-type", "application/x-protobuf");

struct GrpcProbe {
    token: Option<&'static str>,
}

#[async_trait]
impl SourceProbe for GrpcProbe {
    async fn offer(&self, configuration: &Value) -> Result<Offer> {
        let port = configuration["grpcPort"].as_u64().context("grpcPort")? as u16;
        let Ok(channel) = channel(port).await else {
            return Ok(Offer::Refused);
        };
        match MetricsServiceClient::new(channel)
            .export(authorized(latency("checkout", 1.0), self.token))
            .await
        {
            Ok(_) => Ok(Offer::Accepted),
            Err(status) if status.code() == Code::Unavailable => Ok(Offer::Refused),
            Err(status) => anyhow::bail!("unexpected status {status}"),
        }
    }
}

struct HttpProbe;

#[async_trait]
impl SourceProbe for HttpProbe {
    async fn offer(&self, configuration: &Value) -> Result<Offer> {
        let port = configuration["httpPort"].as_u64().context("httpPort")? as u16;
        let response = match post_http(
            port,
            "/v1/metrics",
            latency("checkout", 1.0).encode_to_vec(),
            &[PROTOBUF_HEADER],
        )
        .await
        {
            Ok(response) => response,
            Err(_) => return Ok(Offer::Refused),
        };
        match response.status().as_u16() {
            200 => Ok(Offer::Accepted),
            503 => Ok(Offer::Refused),
            status => anyhow::bail!("unexpected status {status}"),
        }
    }
}

fn conformance() -> SourceConformance<'static> {
    SourceConformance::new(&OtelSourceFactory, ID).expect("conformance")
}

fn create(configuration: Value) -> Result<Box<dyn NativeSource>> {
    create_source(
        &OtelSourceFactory,
        &ComponentId::try_new(ID)?,
        configuration,
    )
}

async fn started(configuration: Value) -> Result<Box<dyn NativeSource>> {
    let mut source = create(configuration)?;
    source.initialize(&contexts::initialize()).await?;
    source.start(&mut contexts::start()).await?;
    Ok(source)
}

/// Receives queued changes until `count` have arrived.
async fn receive(source: &mut dyn NativeSource, count: usize) -> Result<Vec<SourceChange>> {
    let mut changes = Vec::new();
    while changes.len() < count {
        let envelope = tokio::time::timeout(Duration::from_secs(5), source.next())
            .await
            .context("next timed out")??
            .context("envelope")?;
        changes.extend(GraphChangeCodec::decode_changes(&envelope.envelope)?);
    }
    Ok(changes)
}

fn kinds(changes: &[SourceChange]) -> Vec<(&'static str, String)> {
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
fn metadata_is_valid() -> Result<()> {
    let metadata = OtelSourceFactory.metadata();
    assert_eq!(&*metadata.implementation.name, OTEL_SOURCE);
    assert!(metadata.capabilities.management);
    assert!(
        !OtelSourceFactory
            .metadata()
            .capabilities
            .services
            .source_admission
    );
    metadata.descriptor(ComponentId::try_new(ID)?)?;
    plugin()?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_lifecycle_conformance_current_thread() -> Result<()> {
    conformance()
        .lifecycle(grpc_config(free_port()), &GrpcProbe { token: None })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn grpc_lifecycle_conformance_multi_thread() -> Result<()> {
    conformance()
        .lifecycle(grpc_config(free_port()), &GrpcProbe { token: None })
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn http_lifecycle_conformance_current_thread() -> Result<()> {
    conformance()
        .lifecycle(http_config(free_port()), &HttpProbe)
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn http_lifecycle_conformance_multi_thread() -> Result<()> {
    conformance()
        .lifecycle(http_config(free_port()), &HttpProbe)
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_start_conformance() -> Result<()> {
    conformance()
        .cancelled_start(grpc_config(free_port()), &GrpcProbe { token: None })
        .await?;
    conformance()
        .cancelled_start(http_config(free_port()), &HttpProbe)
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn erase_is_idempotent() -> Result<()> {
    conformance().erase(grpc_config(free_port())).await
}

#[test]
fn configuration_round_trips() -> Result<()> {
    conformance().configuration_round_trip(&[
        json!({"stream": "otel"}),
        with(
            http_config(4318),
            json!({
                "protocols": ["http", "grpc"], "grpcPort": 4317,
                "authToken": TOKEN, "authTokenSecondary": "fedcba9876543210-token",
                "rateLimitPerSecond": 100, "rateLimitBurst": 200,
                "metricIdentityAttributes": ["region"], "destinationAttributes": ["peer.service", "server.address"],
                "spanKinds": ["CLIENT", "PRODUCER"], "heartbeatMetric": "health.heartbeat",
                "heartbeatEventName": "health.heartbeat", "logMinSeverity": "WARN",
                "logEventNameAllowlist": ["payment_*"], "logEventTtlSecs": 30,
                "dependencyTtlSecs": 600, "maxServices": 10, "maxMetrics": 20,
                "maxDependencies": 30, "maxLogEvents": 40, "rejectDerived": false
            }),
        ),
    ])
}

#[test]
fn assessment_matrix() -> Result<()> {
    let current = with(grpc_config(4317), json!({"authToken": TOKEN}));
    let case =
        |extra: Value, boundary: Option<UpdateBoundary>| (with(current.clone(), extra), boundary);
    use UpdateBoundary::*;
    conformance().assess_matrix(
        &current,
        &[
            case(json!({}), Some(Pause)),
            case(json!({"stream": "other"}), Some(Replace)),
            case(
                json!({"metricIdentityAttributes": ["region"]}),
                Some(Replace),
            ),
            case(json!({"host": "127.0.0.2"}), Some(Stop)),
            case(json!({"protocols": ["grpc", "http"]}), Some(Stop)),
            case(json!({"grpcPort": 5317}), Some(Stop)),
            case(json!({"httpPort": 5318}), Some(Stop)),
            case(json!({"timeoutMs": 1000}), Some(Stop)),
            case(json!({"ingressCapacity": 16}), Some(Stop)),
            case(json!({"maxMessageBytes": 1024}), Some(Stop)),
            case(json!({"maxConnections": 8}), Some(Stop)),
            case(json!({"headerReadTimeoutMs": 500}), Some(Stop)),
            case(json!({"authToken": "fedcba9876543210-token"}), Some(Pause)),
            case(
                json!({"authTokenSecondary": "fedcba9876543210-token"}),
                Some(Pause),
            ),
            case(json!({"rateLimitPerSecond": 5}), Some(Pause)),
            case(json!({"allowUnauthenticated": true}), Some(Pause)),
            case(json!({"metricAllowlist": ["*"]}), Some(Pause)),
            case(json!({"destinationAttributes": []}), Some(Pause)),
            case(json!({"spanKinds": ["SERVER"]}), Some(Pause)),
            case(json!({"heartbeatMetric": "hb"}), Some(Pause)),
            case(json!({"heartbeatEventName": "hb"}), Some(Pause)),
            case(json!({"logMinSeverity": "INFO"}), Some(Pause)),
            case(json!({"logEventNameAllowlist": ["x"]}), Some(Pause)),
            case(json!({"logEventTtlSecs": 5}), Some(Pause)),
            case(json!({"dependencyTtlSecs": 5}), Some(Pause)),
            case(json!({"maxServices": 5}), Some(Pause)),
            case(json!({"maxMetrics": 5}), Some(Pause)),
            case(json!({"maxDependencies": 5}), Some(Pause)),
            case(json!({"maxLogEvents": 5}), Some(Pause)),
            case(json!({"rejectDerived": false}), Some(Pause)),
            // The strongest boundary among the changed fields wins.
            case(
                json!({"grpcPort": 5317, "metricAllowlist": ["*"]}),
                Some(Stop),
            ),
            // Invalid or exposed proposals are refused.
            case(json!({"maxServices": 0}), None),
            case(json!({"unknown": true}), None),
        ],
    )?;
    // An unauthenticated non-loopback proposal is refused.
    conformance().assess_matrix(
        &grpc_config(4317),
        &[(with(grpc_config(4317), json!({"host": "0.0.0.0"})), None)],
    )
}

#[tokio::test(flavor = "current_thread")]
async fn in_place_and_restart_updates_apply_to_a_running_source() -> Result<()> {
    let port = free_port();
    conformance()
        .apply(
            grpc_config(port),
            with(
                grpc_config(port),
                json!({"metricAllowlist": ["latency", "errors"]}),
            ),
            &GrpcProbe { token: None },
        )
        .await?;
    conformance()
        .apply(
            grpc_config(free_port()),
            grpc_config(free_port()),
            &GrpcProbe { token: None },
        )
        .await
}

#[tokio::test(flavor = "current_thread")]
async fn an_in_place_update_changes_the_projection_of_the_next_export() -> Result<()> {
    let port = free_port();
    let current = grpc_config(port);
    let proposed = with(current.clone(), json!({"metricAllowlist": ["errors"]}));
    let mut source = started(current.clone()).await?;
    let request = metrics(
        resource(&[("service.name", "checkout")]),
        vec![gauge("errors", vec![point(1.0, 0, &[])])],
    );
    let response = export_grpc(port, None, request.clone()).await??;
    assert_eq!(
        response
            .partial_success
            .map(|partial| partial.rejected_data_points),
        Some(1)
    );
    let update = drasi_computation_plugin_sdk::testing::update_request(
        &OtelSourceFactory,
        &ComponentId::try_new(ID)?,
        current,
        proposed,
    );
    let assessment = source.management().context("management")?.assess(&update)?;
    let plan = assessment.plan().context("plan")?;
    source.pause(&contexts::pause()).await?;
    source
        .management()
        .context("management")?
        .apply(
            &contexts::apply(UpdateBoundary::Pause),
            &drasi_computation_plugin_sdk::ApplyUpdateRequest {
                operation_id: "update".into(),
                request: update,
                plan,
                fingerprint: assessment.fingerprint().map(str::to_owned),
            },
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    source.resume(&contexts::resume()).await?;
    let response = export_grpc(port, None, request).await??;
    assert_eq!(response.partial_success, None);
    assert_eq!(receive(source.as_mut(), 3).await?.len(), 3);
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_exports_of_every_signal_reach_next_in_order() -> Result<()> {
    let port = free_port();
    let mut source = started(with(grpc_config(port), json!({"heartbeatEventName": "hb"}))).await?;
    let channel = channel(port).await?;
    MetricsServiceClient::new(channel.clone())
        .export(latency("checkout", 920.0))
        .await?;
    TraceServiceClient::new(channel.clone())
        .export(traces(
            resource(&[("service.name", "checkout")]),
            vec![client_span(&[("peer.service", "payments")], 0)],
        ))
        .await?;
    LogsServiceClient::new(channel)
        .export(logs(
            resource(&[("service.name", "checkout")]),
            vec![log("payment_failed", SeverityNumber::Error, "declined", 7)],
        ))
        .await?;
    let changes = receive(source.as_mut(), 9).await?;
    assert_eq!(
        kinds(&changes),
        vec![
            ("insert", "svc:checkout".into()),
            ("insert", "metric:svc:checkout:latency".into()),
            ("insert", "reports:metric:svc:checkout:latency".into()),
            ("update", "svc:checkout".into()),
            ("insert", "svc:payments".into()),
            ("insert", "dep:svc:checkout:svc:payments".into()),
            ("update", "svc:checkout".into()),
            ("insert", "log:svc:checkout:payment_failed:7".into()),
            ("insert", "emits:log:svc:checkout:payment_failed:7".into()),
        ]
    );
    // Elements are namespaced by the component UUID.
    assert!(changes
        .iter()
        .all(|change| change.get_reference().namespace.as_ref() == ID));
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_reports_partial_success_and_accepts_gzip() -> Result<()> {
    let port = free_port();
    let mut source = started(grpc_config(port)).await?;
    let mut request = latency("checkout", 1.0);
    request.resource_metrics.extend(
        metrics(
            resource(&[("host.name", "x")]),
            vec![gauge("latency", vec![point(1.0, 0, &[])])],
        )
        .resource_metrics,
    );
    let mut client = MetricsServiceClient::new(channel(port).await?)
        .send_compressed(tonic::codec::CompressionEncoding::Gzip);
    let response = client.export(request).await?.into_inner();
    let partial = response.partial_success.context("partial success")?;
    assert_eq!(partial.rejected_data_points, 1);
    assert!(!partial.error_message.is_empty());
    assert_eq!(receive(source.as_mut(), 3).await?.len(), 3);
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn grpc_refuses_oversized_messages() -> Result<()> {
    let port = free_port();
    let source = started(with(grpc_config(port), json!({"maxMessageBytes": 64}))).await?;
    let mut big = latency("checkout", 1.0);
    big.resource_metrics[0].resource = resource(&[("service.name", &"x".repeat(200))]);
    let status = export_grpc(port, None, big).await?.expect_err("too large");
    // tonic refuses a message above max_decoding_message_size as OUT_OF_RANGE.
    assert_eq!(status.code(), Code::OutOfRange);
    drop(source);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn bearer_tokens_guard_both_transports_and_rotate() -> Result<()> {
    let (grpc_port, http_port) = (free_port(), free_port());
    let mut source = started(json!({
        "stream": "otel", "protocols": ["grpc", "http"], "grpcPort": grpc_port, "httpPort": http_port,
        "metricAllowlist": ["latency"], "authToken": TOKEN, "authTokenSecondary": "fedcba9876543210-token"
    }))
    .await?;
    for token in [None, Some("wrong-token-0123456789")] {
        let status = export_grpc(grpc_port, token, latency("checkout", 1.0))
            .await?
            .expect_err("unauthenticated");
        assert_eq!(status.code(), Code::Unauthenticated);
    }
    export_grpc(grpc_port, Some(TOKEN), latency("checkout", 1.0)).await??;
    export_grpc(
        grpc_port,
        Some("fedcba9876543210-token"),
        latency("checkout", 1.0),
    )
    .await??;
    let body = latency("checkout", 1.0).encode_to_vec();
    let refused = post_http(http_port, "/v1/metrics", body.clone(), &[PROTOBUF_HEADER]).await?;
    assert_eq!(refused.status().as_u16(), 401);
    let status = RpcStatus::decode(refused.bytes().await?)?;
    assert_eq!(status.code, 16);
    let bearer = format!("Bearer {TOKEN}");
    let accepted = post_http(
        http_port,
        "/v1/metrics",
        body,
        &[PROTOBUF_HEADER, ("authorization", &bearer)],
    )
    .await?;
    assert_eq!(accepted.status().as_u16(), 200);
    assert_eq!(receive(source.as_mut(), 9).await?.len(), 9);
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn http_answers_protobuf_responses_and_partial_success() -> Result<()> {
    let port = free_port();
    let mut source = started(http_config(port)).await?;
    let mut request = latency("checkout", 1.0);
    request
        .resource_metrics
        .extend(latency("", 2.0).resource_metrics);
    let response = post_http(
        port,
        "/v1/metrics",
        request.encode_to_vec(),
        &[("content-type", "application/x-protobuf; charset=binary")],
    )
    .await?;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(
        response.headers()["content-type"].to_str()?,
        "application/x-protobuf"
    );
    let decoded = ExportMetricsServiceResponse::decode(response.bytes().await?)?;
    assert_eq!(
        decoded
            .partial_success
            .map(|partial| partial.rejected_data_points),
        Some(1)
    );
    assert_eq!(receive(source.as_mut(), 3).await?.len(), 3);
    // Traces and logs routes answer their own response types.
    let traces_body = traces(
        resource(&[("service.name", "checkout")]),
        vec![client_span(&[("peer.service", "db")], 0)],
    )
    .encode_to_vec();
    let response = post_http(port, "/v1/traces", traces_body, &[PROTOBUF_HEADER]).await?;
    assert_eq!(response.status().as_u16(), 200);
    let logs_body = logs(
        resource(&[("service.name", "checkout")]),
        vec![log("x", SeverityNumber::Info, "", 1)],
    )
    .encode_to_vec();
    let response = post_http(port, "/v1/logs", logs_body, &[PROTOBUF_HEADER]).await?;
    assert_eq!(response.status().as_u16(), 200);
    let decoded = crate::otlp::proto::collector::logs::v1::ExportLogsServiceResponse::decode(
        response.bytes().await?,
    )?;
    assert_eq!(
        decoded
            .partial_success
            .map(|partial| partial.rejected_log_records),
        Some(1)
    );
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn http_refuses_unsupported_media_malformed_and_oversized_bodies() -> Result<()> {
    let port = free_port();
    let source = started(with(http_config(port), json!({"maxMessageBytes": 256}))).await?;
    let body = latency("checkout", 1.0).encode_to_vec();
    let json_body = post_http(
        port,
        "/v1/metrics",
        b"{}".to_vec(),
        &[("content-type", "application/json")],
    )
    .await?;
    assert_eq!(json_body.status().as_u16(), 415);
    let no_type = post_http(port, "/v1/metrics", body.clone(), &[]).await?;
    assert_eq!(no_type.status().as_u16(), 415);
    let brotli = post_http(
        port,
        "/v1/metrics",
        body.clone(),
        &[PROTOBUF_HEADER, ("content-encoding", "br")],
    )
    .await?;
    assert_eq!(brotli.status().as_u16(), 415);
    let garbage = post_http(port, "/v1/metrics", vec![0xff; 16], &[PROTOBUF_HEADER]).await?;
    assert_eq!(garbage.status().as_u16(), 400);
    assert_eq!(RpcStatus::decode(garbage.bytes().await?)?.code, 3);
    let oversized = post_http(port, "/v1/metrics", vec![0; 257], &[PROTOBUF_HEADER]).await?;
    assert_eq!(oversized.status().as_u16(), 413);
    let not_gzip = post_http(
        port,
        "/v1/metrics",
        body,
        &[PROTOBUF_HEADER, ("content-encoding", "gzip")],
    )
    .await?;
    assert_eq!(not_gzip.status().as_u16(), 400);
    let unknown = post_http(port, "/v1/profiles", vec![], &[PROTOBUF_HEADER]).await?;
    assert_eq!(unknown.status().as_u16(), 404);
    let get = http_client()
        .get(format!("http://127.0.0.1:{port}/v1/metrics"))
        .send()
        .await?;
    assert_eq!(get.status().as_u16(), 405);
    drop(source);
    Ok(())
}

fn gzip(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

#[tokio::test(flavor = "current_thread")]
async fn http_decompresses_gzip_within_the_message_limit() -> Result<()> {
    let port = free_port();
    let mut source = started(with(http_config(port), json!({"maxMessageBytes": 1024}))).await?;
    let compressed = gzip(&latency("checkout", 1.0).encode_to_vec())?;
    let response = post_http(
        port,
        "/v1/metrics",
        compressed,
        &[PROTOBUF_HEADER, ("content-encoding", "gzip")],
    )
    .await?;
    assert_eq!(response.status().as_u16(), 200);
    assert_eq!(receive(source.as_mut(), 3).await?.len(), 3);
    // A small body that inflates beyond the limit is refused, not inflated.
    let bomb = gzip(&vec![0u8; 64 * 1024])?;
    assert!(bomb.len() < 1024);
    let response = post_http(
        port,
        "/v1/metrics",
        bomb,
        &[PROTOBUF_HEADER, ("content-encoding", "gzip")],
    )
    .await?;
    assert_eq!(response.status().as_u16(), 413);
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn paused_http_answers_retryable_503() -> Result<()> {
    let port = free_port();
    let mut source = started(http_config(port)).await?;
    source.pause(&contexts::pause()).await?;
    let response = post_http(
        port,
        "/v1/metrics",
        latency("checkout", 1.0).encode_to_vec(),
        &[PROTOBUF_HEADER],
    )
    .await?;
    assert_eq!(response.status().as_u16(), 503);
    assert_eq!(response.headers()["retry-after"].to_str()?, "1");
    assert_eq!(RpcStatus::decode(response.bytes().await?)?.code, 14);
    source.resume(&contexts::resume()).await?;
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn rate_limits_apply_per_client() -> Result<()> {
    let port = free_port();
    let source = started(with(
        grpc_config(port),
        json!({"rateLimitPerSecond": 1, "rateLimitBurst": 1}),
    ))
    .await?;
    export_grpc(port, None, latency("checkout", 1.0)).await??;
    let status = export_grpc(port, None, latency("checkout", 1.0))
        .await?
        .expect_err("rate limited");
    assert_eq!(status.code(), Code::ResourceExhausted);
    drop(source);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn both_protocols_serve_one_projection_state() -> Result<()> {
    let (grpc_port, http_port) = (free_port(), free_port());
    let mut source = started(json!({
        "stream": "otel", "protocols": ["grpc", "http"], "grpcPort": grpc_port,
        "httpPort": http_port, "metricAllowlist": ["latency"]
    }))
    .await?;
    export_grpc(grpc_port, None, latency("checkout", 1.0)).await??;
    let response = post_http(
        http_port,
        "/v1/metrics",
        latency("checkout", 2.0).encode_to_vec(),
        &[PROTOBUF_HEADER],
    )
    .await?;
    assert_eq!(response.status().as_u16(), 200);
    let changes = receive(source.as_mut(), 6).await?;
    let kinds: Vec<&str> = kinds(&changes).into_iter().map(|(kind, _)| kind).collect();
    assert_eq!(
        kinds,
        vec!["insert", "insert", "insert", "update", "update", "update"]
    );
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn projection_state_survives_stop_and_start_of_the_same_object() -> Result<()> {
    let port = free_port();
    let mut source = started(grpc_config(port)).await?;
    export_grpc(port, None, latency("checkout", 1.0)).await??;
    receive(source.as_mut(), 3).await?;
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    source.start(&mut contexts::start()).await?;
    export_grpc(port, None, latency("checkout", 2.0)).await??;
    let updates: Vec<&str> = kinds(&receive(source.as_mut(), 3).await?)
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(updates, vec!["update", "update", "update"]);
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    // A new object starts with empty state.
    let mut fresh = started(grpc_config(port)).await?;
    export_grpc(port, None, latency("checkout", 3.0)).await??;
    let inserts: Vec<&str> = kinds(&receive(fresh.as_mut(), 3).await?)
        .into_iter()
        .map(|(kind, _)| kind)
        .collect();
    assert_eq!(inserts, vec!["insert", "insert", "insert"]);
    fresh.stop(&contexts::stop(StopMode::Abort)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn the_running_sweeper_deletes_expired_dependencies() -> Result<()> {
    let port = free_port();
    let mut source = started(with(grpc_config(port), json!({"dependencyTtlSecs": 1}))).await?;
    TraceServiceClient::new(channel(port).await?)
        .export(traces(
            resource(&[("service.name", "checkout")]),
            vec![client_span(&[("peer.service", "payments")], 0)],
        ))
        .await?;
    receive(source.as_mut(), 3).await?;
    let deletes = receive(source.as_mut(), 1).await?;
    assert_eq!(
        kinds(&deletes),
        vec![("delete", "dep:svc:checkout:svc:payments".into())]
    );
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn a_failed_bind_reports_the_error_and_leaves_nothing_running() -> Result<()> {
    let occupied = std::net::TcpListener::bind("127.0.0.1:0")?;
    let busy = occupied.local_addr()?.port();
    let free = free_port();
    let mut source = create(json!({
        "stream": "otel", "protocols": ["grpc", "http"], "grpcPort": free, "httpPort": busy
    }))?;
    source.initialize(&contexts::initialize()).await?;
    let error = source
        .start(&mut contexts::start())
        .await
        .expect_err("busy port");
    assert!(format!("{error:#}").contains("bind"), "{error:#}");
    // The gRPC port bound first was released.
    std::net::TcpListener::bind(("127.0.0.1", free))?;
    Ok(())
}

#[test]
fn unauthenticated_non_loopback_listeners_are_refused_at_construction() -> Result<()> {
    assert!(create(json!({"stream": "otel", "host": "0.0.0.0"})).is_err());
    create(json!({"stream": "otel", "host": "0.0.0.0", "authToken": TOKEN}))?;
    create(json!({"stream": "otel", "host": "0.0.0.0", "allowUnauthenticated": true}))?;
    Ok(())
}

fn certificate() -> Result<(String, String)> {
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()])?;
    Ok((certificate.cert.pem(), certificate.key_pair.serialize_pem()))
}

#[tokio::test(flavor = "current_thread")]
async fn tls_listeners_serve_grpc_and_http() -> Result<()> {
    let (certificate_pem, key_pem) = certificate()?;
    let (grpc_port, http_port) = (free_port(), free_port());
    let mut source = started(json!({
        "stream": "otel", "protocols": ["grpc", "http"], "grpcPort": grpc_port, "httpPort": http_port,
        "metricAllowlist": ["latency"], "tlsCertificatePem": certificate_pem, "tlsPrivateKeyPem": key_pem
    }))
    .await?;
    let tls = tonic::transport::ClientTlsConfig::new()
        .ca_certificate(tonic::transport::Certificate::from_pem(&certificate_pem))
        .domain_name("localhost");
    let channel = Channel::from_shared(format!("https://127.0.0.1:{grpc_port}"))?
        .tls_config(tls)?
        .connect()
        .await?;
    MetricsServiceClient::new(channel)
        .export(latency("checkout", 1.0))
        .await?;
    let client = reqwest::Client::builder()
        .add_root_certificate(reqwest::Certificate::from_pem(certificate_pem.as_bytes())?)
        .resolve("localhost", ([127, 0, 0, 1], http_port).into())
        .build()?;
    let response = client
        .post(format!("https://localhost:{http_port}/v1/metrics"))
        .header("content-type", "application/x-protobuf")
        .body(latency("checkout", 2.0).encode_to_vec())
        .send()
        .await?;
    assert_eq!(response.status().as_u16(), 200);
    // Plaintext is refused by the TLS listener.
    assert!(export_grpc(grpc_port, None, latency("checkout", 3.0))
        .await
        .map_or(true, |result| result.is_err()));
    assert_eq!(receive(source.as_mut(), 6).await?.len(), 6);
    source.stop(&contexts::stop(StopMode::Drain)).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn abort_stop_ends_promptly_and_dispose_requires_stop() -> Result<()> {
    let port = free_port();
    let mut source = started(grpc_config(port)).await?;
    assert!(source
        .dispose(&contexts::dispose(
            drasi_computation_plugin_sdk::lifecycle::DisposeReason::Shutdown,
            drasi_computation_plugin_sdk::lifecycle::StateIntent::Preserve,
            1,
        )?)
        .await
        .is_err());
    tokio::time::timeout(
        Duration::from_secs(5),
        source.stop(&contexts::stop(StopMode::Abort)),
    )
    .await
    .context("abort stop timed out")??;
    assert!(channel(port).await.is_err(), "listener closed");
    Ok(())
}
