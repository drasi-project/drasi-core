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

//! Client-harness integration tests for the OTel source.
//!
//! Run with: cargo test -p drasi-source-otel -- --ignored --nocapture

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_otel::otlp::proto::collector::metrics::v1::metrics_service_client::MetricsServiceClient;
use drasi_source_otel::otlp::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use drasi_source_otel::otlp::proto::collector::trace::v1::trace_service_client::TraceServiceClient;
use drasi_source_otel::otlp::proto::collector::trace::v1::ExportTraceServiceRequest;
use drasi_source_otel::otlp::proto::common::v1::{any_value, AnyValue, KeyValue};
use drasi_source_otel::otlp::proto::metrics::v1::{
    metric, number_data_point, Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics,
};
use drasi_source_otel::otlp::proto::resource::v1::Resource;
use drasi_source_otel::otlp::proto::trace::v1::{span, ResourceSpans, ScopeSpans, Span};
use drasi_source_otel::OtelSource;
use prost::Message;
use tokio::time::sleep;

async fn find_available_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    sleep(Duration::from_millis(50)).await;
    port
}

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

async fn connect_metrics(port: u16) -> MetricsServiceClient<tonic::transport::Channel> {
    let endpoint = format!("http://127.0.0.1:{port}");
    for _ in 0..20 {
        if let Ok(client) = MetricsServiceClient::connect(endpoint.clone()).await {
            return client;
        }
        sleep(Duration::from_millis(50)).await;
    }
    panic!("failed to connect OTLP metrics client on {port}");
}

/// End-to-end CREATE / UPDATE / DELETE through DrasiLib.
#[tokio::test]
#[ignore]
async fn test_change_detection_with_client_harness() {
    let grpc_port = find_available_port().await;
    let http_port = find_available_port().await;

    let source = OtelSource::builder("test-source")
        .with_grpc_bind(format!("127.0.0.1:{grpc_port}"))
        .with_http_bind(format!("127.0.0.1:{http_port}"))
        .with_metric_allowlist(["latency_p99_ms"])
        .with_dependency_ttl_secs(2)
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

    let (metric_reaction, metric_handle) = ApplicationReactionBuilder::new("metric-reaction")
        .with_query("metric-query")
        .build();
    let (dep_reaction, dep_handle) = ApplicationReactionBuilder::new("dep-reaction")
        .with_query("dep-query")
        .build();

    let drasi = Arc::new(
        DrasiLib::builder()
            .with_id("otel-e2e")
            .with_source(source)
            .with_query(metric_query)
            .with_query(dep_query)
            .with_reaction(metric_reaction)
            .with_reaction(dep_reaction)
            .build()
            .await
            .unwrap(),
    );
    drasi.start().await.unwrap();
    sleep(Duration::from_millis(300)).await;

    let mut metric_sub = metric_handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(5)))
        .await
        .unwrap();
    let mut dep_sub = dep_handle
        .subscribe_with_options(SubscriptionOptions::default().with_timeout(Duration::from_secs(8)))
        .await
        .unwrap();

    let mut metrics = connect_metrics(grpc_port).await;
    metrics.export(gauge_request(920.0)).await.unwrap();

    let created = metric_sub.recv().await.expect("CREATE was not detected!");
    match &created.results[0] {
        ResultDiff::Add { data, .. } => {
            assert_eq!(data["service"], "checkout");
            assert_eq!(data["latencyMs"], 920.0);
        }
        other => panic!("expected Add, got {other:?}"),
    }

    metrics.export(gauge_request(700.0)).await.unwrap();
    let updated = metric_sub.recv().await.expect("UPDATE was not detected!");
    match &updated.results[0] {
        ResultDiff::Update { after, .. } => {
            assert_eq!(after["latencyMs"], 700.0);
        }
        ResultDiff::Add { data, .. } => {
            assert_eq!(data["latencyMs"], 700.0);
        }
        other => panic!("expected Update, got {other:?}"),
    }

    let endpoint = format!("http://127.0.0.1:{grpc_port}");
    let mut traces = TraceServiceClient::connect(endpoint).await.unwrap();
    traces.export(client_span_request()).await.unwrap();
    let dep_add = dep_sub
        .recv()
        .await
        .expect("DEPENDS_ON create not detected");
    match &dep_add.results[0] {
        ResultDiff::Add { data, .. } => {
            assert_eq!(data["fromService"], "checkout");
            assert_eq!(data["toService"], "payments");
        }
        other => panic!("expected Add for dependency, got {other:?}"),
    }

    let dep_del = dep_sub.recv().await.expect("DELETE was not detected!");
    assert!(
        matches!(dep_del.results[0], ResultDiff::Delete { .. }),
        "DELETE was not detected! got {:?}",
        dep_del.results[0]
    );

    let body = gauge_request(650.0).encode_to_vec();
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{http_port}/v1/metrics"))
        .header("Content-Type", "application/x-protobuf")
        .body(body)
        .send()
        .await;
    if let Ok(response) = response {
        assert!(
            response.status().is_success(),
            "HTTP OTLP rejected: {}",
            response.status()
        );
    }

    drasi.stop().await.unwrap();
}
