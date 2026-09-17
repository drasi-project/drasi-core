// Copyright 2025 The Drasi Authors.
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

//! Collector-backed integration tests for the OTel source.
//!
//! Requires Docker. Run with:
//! `cargo test -p drasi-source-otel --test integration_test -- --ignored --nocapture`

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_otel::otlp::proto::collector::logs::v1::logs_service_client::LogsServiceClient;
use drasi_source_otel::otlp::proto::collector::logs::v1::ExportLogsServiceRequest;
use drasi_source_otel::otlp::proto::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
use drasi_source_otel::otlp::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use drasi_source_otel::otlp::proto::collector::trace::v1::trace_service_client::TraceServiceClient;
use drasi_source_otel::otlp::proto::collector::trace::v1::ExportTraceServiceRequest;
use drasi_source_otel::otlp::proto::common::v1::{any_value, AnyValue, KeyValue};
use drasi_source_otel::otlp::proto::logs::v1::{
    LogRecord, ResourceLogs, ScopeLogs, SeverityNumber,
};
use drasi_source_otel::otlp::proto::metrics::v1::{
    metric, number_data_point, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics,
};
use drasi_source_otel::otlp::proto::resource::v1::Resource;
use drasi_source_otel::otlp::proto::trace::v1::{span, ResourceSpans, ScopeSpans, Span};
use drasi_source_otel::OtelSource;
use testcontainers::core::{Host, IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio::time::sleep;

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
    }
}

fn checkout_resource() -> Resource {
    Resource {
        attributes: vec![kv("service.name", "checkout")],
        dropped_attributes_count: 0,
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn gauge_request(value: f64) -> ExportMetricsServiceRequest {
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(checkout_resource()),
            scope_metrics: vec![ScopeMetrics {
                scope: None,
                metrics: vec![Metric {
                    name: "latency_p99_ms".to_string(),
                    description: String::new(),
                    unit: "ms".to_string(),
                    data: Some(metric::Data::Gauge(Gauge {
                        data_points: vec![NumberDataPoint {
                            attributes: vec![],
                            start_time_unix_nano: 0,
                            time_unix_nano: now_nanos(),
                            exemplars: vec![],
                            flags: 0,
                            value: Some(number_data_point::Value::AsDouble(value)),
                        }],
                    })),
                    metadata: vec![],
                }],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

fn client_span_request() -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource: Some(checkout_resource()),
            scope_spans: vec![ScopeSpans {
                scope: None,
                spans: vec![Span {
                    trace_id: vec![1; 16],
                    span_id: vec![2; 8],
                    trace_state: String::new(),
                    parent_span_id: vec![],
                    flags: 0,
                    name: "checkout->payments".to_string(),
                    kind: span::SpanKind::Client as i32,
                    start_time_unix_nano: now_nanos(),
                    end_time_unix_nano: 0,
                    attributes: vec![kv("peer.service", "payments")],
                    dropped_attributes_count: 0,
                    events: vec![],
                    dropped_events_count: 0,
                    links: vec![],
                    dropped_links_count: 0,
                    status: None,
                }],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

fn error_log_request() -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource: Some(checkout_resource()),
            scope_logs: vec![ScopeLogs {
                scope: None,
                log_records: vec![LogRecord {
                    time_unix_nano: now_nanos(),
                    observed_time_unix_nano: 0,
                    severity_number: SeverityNumber::Error as i32,
                    severity_text: "ERROR".to_string(),
                    body: Some(AnyValue {
                        value: Some(any_value::Value::StringValue("card declined".to_string())),
                    }),
                    attributes: vec![],
                    dropped_attributes_count: 0,
                    flags: 0,
                    trace_id: vec![],
                    span_id: vec![],
                    event_name: "payment_failed".to_string(),
                }],
                schema_url: String::new(),
            }],
            schema_url: String::new(),
        }],
    }
}

async fn find_available_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    sleep(Duration::from_millis(50)).await;
    port
}

fn collector_config(source_port: u16) -> String {
    format!(
        r#"
receivers:
  otlp:
    protocols:
      grpc:
        endpoint: 0.0.0.0:4317
exporters:
  otlp/drasi:
    endpoint: host.docker.internal:{source_port}
    tls:
      insecure: true
extensions:
  health_check:
    endpoint: 0.0.0.0:13133
service:
  extensions: [health_check]
  pipelines:
    traces:
      receivers: [otlp]
      exporters: [otlp/drasi]
    metrics:
      receivers: [otlp]
      exporters: [otlp/drasi]
    logs:
      receivers: [otlp]
      exporters: [otlp/drasi]
"#
    )
}

async fn start_collector(source_port: u16) -> ContainerAsync<GenericImage> {
    let config_dir = tempfile::tempdir().unwrap();
    let config_path = config_dir.path().join("config.yaml");
    std::fs::write(&config_path, collector_config(source_port)).unwrap();

    let container = GenericImage::new("otel/opentelemetry-collector", "0.136.0")
        .with_exposed_port(4317.tcp())
        .with_exposed_port(13133.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Everything is ready"))
        .with_copy_to("/etc/otelcol/config.yaml", config_path)
        .with_cmd(["--config=/etc/otelcol/config.yaml"])
        .with_host("host.docker.internal", Host::HostGateway)
        .start()
        .await
        .expect("failed to start OpenTelemetry Collector");

    let health_port = container.get_host_port_ipv4(13133.tcp()).await.unwrap();
    wait_for_collector_health(health_port).await;
    // Keep the temp dir alive until copy completes; container now owns the file.
    drop(config_dir);
    container
}

async fn wait_for_collector_health(port: u16) {
    let url = format!("http://127.0.0.1:{port}/");
    for _ in 0..60 {
        if let Ok(response) = reqwest::get(&url).await {
            if response.status().is_success() {
                return;
            }
        }
        sleep(Duration::from_millis(250)).await;
    }
    panic!("Collector health check failed on {url}");
}

async fn connect_metrics(port: u16) -> MetricsServiceClient<tonic::transport::Channel> {
    connect_client(port, MetricsServiceClient::connect).await
}

async fn connect_traces(port: u16) -> TraceServiceClient<tonic::transport::Channel> {
    connect_client(port, TraceServiceClient::connect).await
}

async fn connect_logs(port: u16) -> LogsServiceClient<tonic::transport::Channel> {
    connect_client(port, LogsServiceClient::connect).await
}

async fn connect_client<T, F, Fut>(port: u16, connect: F) -> T
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<T, tonic::transport::Error>>,
{
    let endpoint = format!("http://127.0.0.1:{port}");
    for _ in 0..40 {
        if let Ok(client) = connect(endpoint.clone()).await {
            return client;
        }
        sleep(Duration::from_millis(100)).await;
    }
    panic!("failed to connect OTLP client on {port}");
}

fn assert_add(diff: &ResultDiff, expected: &[(&str, serde_json::Value)]) {
    match diff {
        ResultDiff::Add { data, .. } => {
            for (key, value) in expected {
                assert_eq!(&data[*key], value, "field {key}");
            }
        }
        other => panic!("expected Add, got {other:?}"),
    }
}

/// Metrics, traces, and logs flow through a real Collector into Drasi.
#[tokio::test]
#[ignore]
async fn test_change_detection_through_collector() {
    let _ = env_logger::try_init();

    let source_port = find_available_port().await;
    let source = OtelSource::builder("test-source")
        .with_grpc_bind(format!("0.0.0.0:{source_port}"))
        .with_metric_allowlist(["latency_p99_ms"])
        .with_dependency_ttl_secs(2)
        .with_log_event_ttl_secs(30)
        .with_auto_start(true)
        .build()
        .unwrap();

    let metric_query = Query::cypher("metric-query")
        .query("MATCH (s:Service)-[:REPORTS]->(m:Metric) RETURN s.name AS service, m.value AS latencyMs")
        .from_source("test-source")
        .auto_start(true)
        .build();
    let dep_query = Query::cypher("dep-query")
        .query("MATCH (a:Service)-[:DEPENDS_ON]->(b:Service) RETURN a.name AS fromService, b.name AS toService")
        .from_source("test-source")
        .auto_start(true)
        .build();
    let log_query = Query::cypher("log-query")
        .query("MATCH (s:Service)-[:EMITS]->(e:LogEvent) RETURN s.name AS service, e.body AS body, e.eventName AS eventName")
        .from_source("test-source")
        .auto_start(true)
        .build();

    let (metric_reaction, metric_handle) = ApplicationReactionBuilder::new("metric-reaction")
        .with_query("metric-query")
        .build();
    let (dep_reaction, dep_handle) = ApplicationReactionBuilder::new("dep-reaction")
        .with_query("dep-query")
        .build();
    let (log_reaction, log_handle) = ApplicationReactionBuilder::new("log-reaction")
        .with_query("log-query")
        .build();

    let drasi = Arc::new(
        DrasiLib::builder()
            .with_id("otel-e2e")
            .with_source(source)
            .with_query(metric_query)
            .with_query(dep_query)
            .with_query(log_query)
            .with_reaction(metric_reaction)
            .with_reaction(dep_reaction)
            .with_reaction(log_reaction)
            .build()
            .await
            .unwrap(),
    );
    drasi.start().await.unwrap();
    sleep(Duration::from_millis(300)).await;

    let collector = start_collector(source_port).await;
    let collector_port = collector.get_host_port_ipv4(4317.tcp()).await.unwrap();

    let mut metric_sub = metric_handle
        .subscribe_with_options(
            SubscriptionOptions::default().with_timeout(Duration::from_secs(10)),
        )
        .await
        .unwrap();
    let mut dep_sub = dep_handle
        .subscribe_with_options(
            SubscriptionOptions::default().with_timeout(Duration::from_secs(10)),
        )
        .await
        .unwrap();
    let mut log_sub = log_handle
        .subscribe_with_options(
            SubscriptionOptions::default().with_timeout(Duration::from_secs(10)),
        )
        .await
        .unwrap();

    let mut metrics = connect_metrics(collector_port).await;
    metrics.export(gauge_request(920.0)).await.unwrap();
    let created = metric_sub
        .recv()
        .await
        .expect("metric CREATE was not detected!");
    assert_add(
        &created.results[0],
        &[
            ("service", serde_json::json!("checkout")),
            ("latencyMs", serde_json::json!(920.0)),
        ],
    );

    metrics.export(gauge_request(700.0)).await.unwrap();
    let updated = metric_sub
        .recv()
        .await
        .expect("metric UPDATE was not detected!");
    match &updated.results[0] {
        ResultDiff::Update { after, .. } => assert_eq!(after["latencyMs"], 700.0),
        ResultDiff::Add { data, .. } => assert_eq!(data["latencyMs"], 700.0),
        other => panic!("expected Update, got {other:?}"),
    }

    let mut traces = connect_traces(collector_port).await;
    traces.export(client_span_request()).await.unwrap();
    let dep_add = dep_sub
        .recv()
        .await
        .expect("DEPENDS_ON create not detected");
    assert_add(
        &dep_add.results[0],
        &[
            ("fromService", serde_json::json!("checkout")),
            ("toService", serde_json::json!("payments")),
        ],
    );

    let mut logs = connect_logs(collector_port).await;
    logs.export(error_log_request()).await.unwrap();
    let log_add = log_sub.recv().await.expect("LogEvent create not detected");
    assert_add(
        &log_add.results[0],
        &[
            ("service", serde_json::json!("checkout")),
            ("body", serde_json::json!("card declined")),
            ("eventName", serde_json::json!("payment_failed")),
        ],
    );

    let dep_del = dep_sub
        .recv()
        .await
        .expect("DEPENDS_ON DELETE was not detected!");
    assert!(
        matches!(dep_del.results[0], ResultDiff::Delete { .. }),
        "DELETE was not detected! got {:?}",
        dep_del.results[0]
    );

    drasi.stop().await.unwrap();
}
