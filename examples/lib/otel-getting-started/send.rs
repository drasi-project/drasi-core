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

//! Client harness that sends a gauge and optionally a client span.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
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

fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.to_string(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.to_string())),
        }),
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos() as u64
}

fn resource() -> Resource {
    Resource {
        attributes: vec![kv("service.name", "checkout")],
        dropped_attributes_count: 0,
    }
}

fn gauge(value: f64) -> ExportMetricsServiceRequest {
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource: Some(resource()),
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

#[tokio::main]
async fn main() -> Result<()> {
    let endpoint =
        std::env::var("OTEL_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:4317".to_string());
    let value: f64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(920.0);

    let mut metrics = MetricsServiceClient::connect(endpoint.clone()).await?;
    metrics.export(gauge(value)).await?;
    println!("sent latency_p99_ms={value}");

    if std::env::args().any(|a| a == "--span") {
        let mut traces = TraceServiceClient::connect(endpoint).await?;
        traces
            .export(ExportTraceServiceRequest {
                resource_spans: vec![ResourceSpans {
                    resource: Some(resource()),
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
            })
            .await?;
        println!("sent CLIENT span checkout -> payments");
    }
    Ok(())
}
