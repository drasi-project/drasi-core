// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Projection rules: what each OTLP record becomes, and what is rejected.

use super::*;
use crate::config::OtelSourceConfig;
use crate::otlp::proto::common::v1::{any_value, AnyValue, KeyValue};
use crate::otlp::proto::logs::v1::{LogRecord, ResourceLogs, ScopeLogs, SeverityNumber};
use crate::otlp::proto::metrics::v1::{
    metric, number_data_point, Gauge, Histogram, HistogramDataPoint, Metric, NumberDataPoint,
    ResourceMetrics, ScopeMetrics, Sum,
};
use crate::otlp::proto::resource::v1::Resource;
use crate::otlp::proto::trace::v1::{span, ResourceSpans, ScopeSpans, Span};
use serde_json::json;

pub(crate) const RECEIVED: u64 = 1_700_000_100_000;
pub(crate) const OBSERVED_NANOS: u64 = 1_700_000_000_000_000_000;
const OBSERVED: u64 = 1_700_000_000_000;

pub(crate) fn settings(extra: serde_json::Value) -> Projection {
    let mut value = json!({"stream": "otel"});
    value
        .as_object_mut()
        .expect("object")
        .extend(extra.as_object().expect("object").clone());
    OtelSourceConfig::from_value(value)
        .expect("valid configuration")
        .projection()
}

pub(crate) fn kv(key: &str, value: &str) -> KeyValue {
    KeyValue {
        key: key.into(),
        value: Some(AnyValue {
            value: Some(any_value::Value::StringValue(value.into())),
        }),
    }
}

pub(crate) fn resource(attributes: &[(&str, &str)]) -> Option<Resource> {
    Some(Resource {
        attributes: attributes.iter().map(|(k, v)| kv(k, v)).collect(),
        ..Default::default()
    })
}

pub(crate) fn point(value: f64, nanos: u64, attributes: &[(&str, &str)]) -> NumberDataPoint {
    NumberDataPoint {
        value: Some(number_data_point::Value::AsDouble(value)),
        time_unix_nano: nanos,
        attributes: attributes.iter().map(|(k, v)| kv(k, v)).collect(),
        ..Default::default()
    }
}

pub(crate) fn gauge(name: &str, points: Vec<NumberDataPoint>) -> Metric {
    Metric {
        name: name.into(),
        unit: "ms".into(),
        data: Some(metric::Data::Gauge(Gauge {
            data_points: points,
        })),
        ..Default::default()
    }
}

pub(crate) fn metrics(
    resource: Option<Resource>,
    metrics: Vec<Metric>,
) -> ExportMetricsServiceRequest {
    ExportMetricsServiceRequest {
        resource_metrics: vec![ResourceMetrics {
            resource,
            scope_metrics: vec![ScopeMetrics {
                metrics,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

pub(crate) fn client_span(attributes: &[(&str, &str)], nanos: u64) -> Span {
    Span {
        name: "call".into(),
        kind: span::SpanKind::Client as i32,
        start_time_unix_nano: nanos,
        attributes: attributes.iter().map(|(k, v)| kv(k, v)).collect(),
        ..Default::default()
    }
}

pub(crate) fn traces(resource: Option<Resource>, spans: Vec<Span>) -> ExportTraceServiceRequest {
    ExportTraceServiceRequest {
        resource_spans: vec![ResourceSpans {
            resource,
            scope_spans: vec![ScopeSpans {
                spans,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

pub(crate) fn log(event: &str, severity: SeverityNumber, body: &str, nanos: u64) -> LogRecord {
    LogRecord {
        time_unix_nano: nanos,
        severity_number: severity as i32,
        severity_text: format!("{severity:?}").to_uppercase(),
        event_name: event.into(),
        body: Some(AnyValue {
            value: Some(any_value::Value::StringValue(body.into())),
        }),
        ..Default::default()
    }
}

pub(crate) fn logs(
    resource: Option<Resource>,
    records: Vec<LogRecord>,
) -> ExportLogsServiceRequest {
    ExportLogsServiceRequest {
        resource_logs: vec![ResourceLogs {
            resource,
            scope_logs: vec![ScopeLogs {
                log_records: records,
                ..Default::default()
            }],
            ..Default::default()
        }],
    }
}

fn checkout() -> Option<Resource> {
    resource(&[("service.name", "checkout")])
}

fn ids(out: &MapOutcome) -> Vec<&str> {
    out.elements
        .iter()
        .map(|element| element.id.as_str())
        .collect()
}

fn element<'a>(out: &'a MapOutcome, id: &str) -> &'a ProjectedElement {
    out.elements
        .iter()
        .find(|element| element.id == id)
        .unwrap_or_else(|| panic!("no element {id} in {:?}", ids(out)))
}

fn text(element: &ProjectedElement, property: &str) -> Option<String> {
    match element.properties.get(property)? {
        ElementValue::String(value) => Some(value.to_string()),
        other => panic!("{property} is not a string: {other:?}"),
    }
}

#[test]
fn nanoseconds_convert_to_milliseconds() {
    assert_eq!(nanos_to_millis(OBSERVED_NANOS), OBSERVED);
    assert_eq!(nanos_to_millis(999_999), 0);
    assert_eq!(effective_time(0, RECEIVED), Some(RECEIVED));
    assert_eq!(effective_time(OBSERVED_NANOS, RECEIVED), Some(OBSERVED));
    assert_eq!(effective_time(u64::MAX, RECEIVED), None);
}

#[test]
fn an_allowlisted_gauge_projects_service_metric_and_reports_in_one_group() {
    let request = metrics(
        resource(&[
            ("service.name", "checkout"),
            ("service.namespace", "shop"),
            ("deployment.environment.name", "prod"),
            ("service.instance.id", "pod-1"),
            ("service.version", "1.2.3"),
        ]),
        vec![gauge(
            "latency_p99_ms",
            vec![point(920.0, OBSERVED_NANOS, &[])],
        )],
    );
    let out = map_metrics(
        &request,
        &settings(json!({"metricAllowlist": ["latency_p99_ms"]})),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (1, 0));
    assert_eq!(
        ids(&out),
        vec![
            "svc:shop/checkout",
            "metric:svc:shop/checkout:latency_p99_ms",
            "reports:metric:svc:shop/checkout:latency_p99_ms"
        ]
    );
    assert!(out.elements.iter().all(|element| element.group == 0));
    let service = element(&out, "svc:shop/checkout");
    assert_eq!(service.labels, vec!["Service"]);
    assert_eq!(text(service, "name").as_deref(), Some("checkout"));
    assert_eq!(text(service, "namespace").as_deref(), Some("shop"));
    assert_eq!(text(service, "environment").as_deref(), Some("prod"));
    assert_eq!(text(service, "instanceId").as_deref(), Some("pod-1"));
    assert_eq!(text(service, "version").as_deref(), Some("1.2.3"));
    let metric = element(&out, "metric:svc:shop/checkout:latency_p99_ms");
    assert_eq!(
        metric.properties.get("value"),
        Some(&ElementValue::Float(OrderedFloat(920.0)))
    );
    assert_eq!(text(metric, "unit").as_deref(), Some("ms"));
    assert_eq!(
        text(metric, "observedAt").as_deref(),
        Some("2023-11-14T22:13:20+00:00")
    );
    assert_eq!(
        text(metric, "receivedAt").as_deref(),
        Some("2023-11-14T22:15:00+00:00")
    );
    assert_eq!(metric.effective_from, OBSERVED);
    assert_eq!(metric.ttl_secs, None);
    assert_eq!(
        element(&out, "reports:metric:svc:shop/checkout:latency_p99_ms").kind,
        ProjectedKind::Relation {
            from: "svc:shop/checkout".into(),
            to: "metric:svc:shop/checkout:latency_p99_ms".into()
        }
    );
}

#[test]
fn integer_sums_project_like_gauges_and_each_point_is_its_own_group() {
    let sum = Metric {
        name: "requests".into(),
        data: Some(metric::Data::Sum(Sum {
            data_points: vec![
                NumberDataPoint {
                    value: Some(number_data_point::Value::AsInt(7)),
                    ..Default::default()
                },
                NumberDataPoint {
                    value: Some(number_data_point::Value::AsInt(9)),
                    ..Default::default()
                },
            ],
            ..Default::default()
        })),
        ..Default::default()
    };
    let out = map_metrics(
        &metrics(checkout(), vec![sum]),
        &settings(json!({"metricAllowlist": ["*"]})),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (2, 0));
    let groups: Vec<u32> = out.elements.iter().map(|element| element.group).collect();
    // The second point repeats no unchanged Service; its group holds only its own metric.
    assert_eq!(groups, vec![0, 0, 0, 1, 1]);
    // An unset point timestamp uses the receipt time.
    assert_eq!(
        element(&out, "metric:svc:checkout:requests").effective_from,
        RECEIVED
    );
    assert_eq!(
        out.elements[3].properties.get("value"),
        Some(&ElementValue::Float(OrderedFloat(9.0)))
    );
}

#[test]
fn unlisted_histogram_valueless_and_untimed_metrics_are_rejected() {
    let histogram = Metric {
        name: "latency".into(),
        data: Some(metric::Data::Histogram(Histogram {
            data_points: vec![HistogramDataPoint::default()],
            ..Default::default()
        })),
        ..Default::default()
    };
    let valueless = gauge("latency", vec![NumberDataPoint::default()]);
    let unlisted = gauge("other", vec![point(1.0, OBSERVED_NANOS, &[])]);
    let impossible_time = gauge("latency", vec![point(1.0, u64::MAX, &[])]);
    let out = map_metrics(
        &metrics(
            checkout(),
            vec![histogram, valueless, unlisted, impossible_time],
        ),
        &settings(json!({"metricAllowlist": ["latency"]})),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (0, 4));
    assert!(out.elements.is_empty());
}

#[test]
fn an_empty_allowlist_admits_no_metric() {
    let out = map_metrics(
        &metrics(
            checkout(),
            vec![gauge("latency", vec![point(1.0, OBSERVED_NANOS, &[])])],
        ),
        &settings(json!({})),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (0, 1));
}

#[test]
fn records_without_a_service_name_reject_every_record_of_the_resource() {
    let request = metrics(
        resource(&[("host.name", "a")]),
        vec![gauge(
            "latency",
            vec![point(1.0, 0, &[]), point(2.0, 0, &[])],
        )],
    );
    let out = map_metrics(
        &request,
        &settings(json!({"metricAllowlist": ["*"]})),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (0, 2));
    let out = map_metrics(
        &metrics(None, vec![]),
        &settings(json!({"metricAllowlist": ["*"]})),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (0, 1));
    let empty_name = resource(&[("service.name", "")]);
    let out = map_traces(
        &traces(empty_name, vec![client_span(&[("peer.service", "x")], 0)]),
        &settings(json!({})),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (0, 1));
}

#[test]
fn identity_attributes_split_series_and_other_attributes_do_not() {
    let request = metrics(
        checkout(),
        vec![gauge(
            "latency",
            vec![
                point(1.0, 0, &[("region", "us"), ("zone", "a"), ("pod", "1")]),
                point(2.0, 0, &[("zone", "b"), ("region", "eu"), ("pod", "2")]),
                point(3.0, 0, &[("pod", "3")]),
            ],
        )],
    );
    let out = map_metrics(
        &request,
        &settings(
            json!({"metricAllowlist": ["latency"], "metricIdentityAttributes": ["zone", "region"]}),
        ),
        RECEIVED,
    );
    let metric_ids: Vec<&str> = ids(&out)
        .into_iter()
        .filter(|id| id.starts_with("metric:"))
        .collect();
    assert_eq!(
        metric_ids,
        vec![
            "metric:svc:checkout:latency:region=us,zone=a",
            "metric:svc:checkout:latency:region=eu,zone=b",
            "metric:svc:checkout:latency"
        ]
    );
}

#[test]
fn heartbeat_metrics_need_no_allowlisting_and_can_also_be_metrics() {
    let request = metrics(
        checkout(),
        vec![gauge(
            "health.heartbeat",
            vec![point(1.0, OBSERVED_NANOS, &[])],
        )],
    );
    let out = map_metrics(
        &request,
        &settings(json!({"heartbeatMetric": "health.heartbeat"})),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (1, 0));
    assert_eq!(
        ids(&out),
        vec!["svc:checkout", "hb:svc:checkout", "rel-hb:svc:checkout"]
    );
    assert_eq!(
        text(element(&out, "hb:svc:checkout"), "lastSeen").as_deref(),
        Some("2023-11-14T22:13:20+00:00")
    );
    let both = map_metrics(
        &request,
        &settings(json!({"heartbeatMetric": "health.heartbeat", "metricAllowlist": ["health.*"]})),
        RECEIVED,
    );
    assert_eq!((both.accepted, both.rejected), (1, 0));
    assert_eq!(
        ids(&both),
        vec![
            "svc:checkout",
            "hb:svc:checkout",
            "rel-hb:svc:checkout",
            "metric:svc:checkout:health.heartbeat",
            "reports:metric:svc:checkout:health.heartbeat"
        ]
    );
}

#[test]
fn derived_records_are_rejected_unless_allowed() {
    let derived_resource = resource(&[
        ("service.name", "checkout"),
        (SOURCE_ORIGIN_ATTRIBUTE, SOURCE_ORIGIN_DERIVED),
    ]);
    let point_level = gauge(
        "latency",
        vec![point(
            1.0,
            0,
            &[(SOURCE_ORIGIN_ATTRIBUTE, SOURCE_ORIGIN_DERIVED)],
        )],
    );
    let reject = settings(json!({"metricAllowlist": ["*"]}));
    let allow = settings(json!({"metricAllowlist": ["*"], "rejectDerived": false}));
    let resource_level = metrics(
        derived_resource.clone(),
        vec![gauge("latency", vec![point(1.0, 0, &[])])],
    );
    assert_eq!(map_metrics(&resource_level, &reject, RECEIVED).rejected, 1);
    assert_eq!(
        map_metrics(
            &metrics(checkout(), vec![point_level.clone()]),
            &reject,
            RECEIVED
        )
        .rejected,
        1
    );
    assert_eq!(map_metrics(&resource_level, &allow, RECEIVED).accepted, 1);
    assert_eq!(
        map_metrics(&metrics(checkout(), vec![point_level]), &allow, RECEIVED).accepted,
        1
    );

    let span = client_span(
        &[
            ("peer.service", "payments"),
            (SOURCE_ORIGIN_ATTRIBUTE, SOURCE_ORIGIN_DERIVED),
        ],
        0,
    );
    assert_eq!(
        map_traces(&traces(checkout(), vec![span]), &reject, RECEIVED).rejected,
        1
    );
    let record = LogRecord {
        attributes: vec![kv(SOURCE_ORIGIN_ATTRIBUTE, SOURCE_ORIGIN_DERIVED)],
        ..log("boom", SeverityNumber::Error, "x", 0)
    };
    assert_eq!(
        map_logs(&logs(checkout(), vec![record]), &reject, RECEIVED).rejected,
        1
    );
    assert_eq!(
        map_logs(
            &logs(
                derived_resource,
                vec![log("boom", SeverityNumber::Error, "x", 0)]
            ),
            &reject,
            RECEIVED
        )
        .rejected,
        1
    );
}

#[test]
fn client_spans_project_depends_on_with_a_ttl() {
    let out = map_traces(
        &traces(
            checkout(),
            vec![client_span(&[("peer.service", "payments")], OBSERVED_NANOS)],
        ),
        &settings(json!({"dependencyTtlSecs": 120})),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (1, 0));
    assert_eq!(
        ids(&out),
        vec![
            "svc:checkout",
            "svc:payments",
            "dep:svc:checkout:svc:payments"
        ]
    );
    let dependency = element(&out, "dep:svc:checkout:svc:payments");
    assert_eq!(dependency.labels, vec!["DEPENDS_ON"]);
    assert_eq!(dependency.ttl_secs, Some(120));
    assert_eq!(dependency.category, ElementCategory::DependsOn);
    assert_eq!(dependency.effective_from, OBSERVED);
    assert_eq!(
        dependency.kind,
        ProjectedKind::Relation {
            from: "svc:checkout".into(),
            to: "svc:payments".into()
        }
    );
    // The callee placeholder carries only its name.
    let callee = element(&out, "svc:payments");
    assert_eq!(text(callee, "name").as_deref(), Some("payments"));
    assert_eq!(text(callee, "version"), None);
}

#[test]
fn destination_attributes_are_tried_in_order_and_empty_values_skipped() {
    let config = settings(json!({"destinationAttributes": ["peer.service", "server.address"]}));
    let span = client_span(&[("server.address", "db"), ("peer.service", "")], 0);
    let out = map_traces(&traces(checkout(), vec![span]), &config, RECEIVED);
    assert!(ids(&out).contains(&"dep:svc:checkout:svc:db"));
    let none = map_traces(
        &traces(checkout(), vec![client_span(&[("net.peer.name", "x")], 0)]),
        &config,
        RECEIVED,
    );
    assert_eq!((none.accepted, none.rejected), (0, 1));
    let disabled = map_traces(
        &traces(checkout(), vec![client_span(&[("peer.service", "x")], 0)]),
        &settings(json!({"destinationAttributes": []})),
        RECEIVED,
    );
    assert_eq!(disabled.rejected, 1);
}

#[test]
fn span_kinds_filter_dependencies() {
    let server = Span {
        kind: span::SpanKind::Server as i32,
        ..client_span(&[("peer.service", "payments")], 0)
    };
    let default = map_traces(
        &traces(checkout(), vec![server.clone()]),
        &settings(json!({})),
        RECEIVED,
    );
    assert_eq!((default.accepted, default.rejected), (0, 1));
    let allowed = map_traces(
        &traces(checkout(), vec![server]),
        &settings(json!({"spanKinds": ["SERVER"]})),
        RECEIVED,
    );
    assert_eq!(allowed.accepted, 1);
}

#[test]
fn destination_namespaces_follow_the_caller_or_the_value() {
    let shop = resource(&[("service.name", "checkout"), ("service.namespace", "shop")]);
    let out = map_traces(
        &traces(
            shop,
            vec![
                client_span(&[("peer.service", "payments")], 0),
                client_span(&[("peer.service", "bank/ledger")], 0),
                client_span(&[("peer.service", "/root")], 0),
            ],
        ),
        &settings(json!({})),
        RECEIVED,
    );
    let ids = ids(&out);
    assert!(
        ids.contains(&"dep:svc:shop/checkout:svc:shop/payments"),
        "{ids:?}"
    );
    assert!(
        ids.contains(&"dep:svc:shop/checkout:svc:bank/ledger"),
        "{ids:?}"
    );
    assert!(ids.contains(&"dep:svc:shop/checkout:svc:/root"), "{ids:?}");
    let ledger = element(&out, "svc:bank/ledger");
    assert_eq!(text(ledger, "namespace").as_deref(), Some("bank"));
    assert_eq!(text(ledger, "name").as_deref(), Some("ledger"));
    assert_eq!(text(element(&out, "svc:/root"), "namespace"), None);
}

#[test]
fn a_callee_seen_earlier_is_enriched_by_its_own_resource_in_one_request() {
    let request = ExportTraceServiceRequest {
        resource_spans: vec![
            traces(
                checkout(),
                vec![client_span(&[("peer.service", "payments")], 0)],
            )
            .resource_spans
            .remove(0),
            traces(
                resource(&[("service.name", "payments"), ("service.version", "2")]),
                vec![client_span(&[("peer.service", "ledger")], 0)],
            )
            .resource_spans
            .remove(0),
        ],
    };
    let out = map_traces(&request, &settings(json!({})), RECEIVED);
    let payments: Vec<&ProjectedElement> = out
        .elements
        .iter()
        .filter(|element| element.id == "svc:payments")
        .collect();
    assert_eq!(payments.len(), 2, "placeholder, then the enriched node");
    assert_eq!(payments[0].group, 0);
    assert_eq!(payments[1].group, 1);
    assert_eq!(text(payments[1], "version").as_deref(), Some("2"));
    // A self-dependency adds the service only once.
    let selfish = map_traces(
        &traces(
            checkout(),
            vec![client_span(&[("peer.service", "checkout")], 0)],
        ),
        &settings(json!({})),
        RECEIVED,
    );
    assert_eq!(
        selfish
            .elements
            .iter()
            .filter(|element| element.id == "svc:checkout")
            .count(),
        1
    );
}

#[test]
fn logs_at_or_above_the_minimum_severity_become_expiring_log_events() {
    let request = logs(
        checkout(),
        vec![
            log(
                "payment_failed",
                SeverityNumber::Error,
                "card declined",
                OBSERVED_NANOS,
            ),
            log("payment_ok", SeverityNumber::Info, "fine", OBSERVED_NANOS),
            log("", SeverityNumber::Fatal, "disk full", 0),
        ],
    );
    let out = map_logs(
        &request,
        &settings(json!({"logEventTtlSecs": 30})),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (2, 1));
    let id = format!("log:svc:checkout:payment_failed:{OBSERVED_NANOS}");
    let event = element(&out, &id);
    assert_eq!(event.labels, vec!["LogEvent"]);
    assert_eq!(event.ttl_secs, Some(30));
    assert_eq!(text(event, "body").as_deref(), Some("card declined"));
    assert_eq!(text(event, "severity").as_deref(), Some("ERROR"));
    assert_eq!(text(event, "eventName").as_deref(), Some("payment_failed"));
    assert_eq!(text(event, "service").as_deref(), Some("checkout"));
    let emits = element(&out, &format!("emits:{id}"));
    assert_eq!(emits.ttl_secs, Some(30));
    assert_eq!(
        emits.kind,
        ProjectedKind::Relation {
            from: "svc:checkout".into(),
            to: id.clone()
        }
    );
    // A record without an event name is identified by a hash of its body.
    let unnamed = format!("log:svc:checkout:{}:0", hash_text("disk full"));
    assert_eq!(text(element(&out, &unnamed), "eventName"), None);
    let lowered = map_logs(
        &request,
        &settings(json!({"logMinSeverity": "INFO"})),
        RECEIVED,
    );
    assert_eq!((lowered.accepted, lowered.rejected), (3, 0));
}

#[test]
fn log_event_name_allowlists_use_globs() {
    let request = logs(
        checkout(),
        vec![
            log("auth_denied", SeverityNumber::Error, "", 1),
            log("payment_failed", SeverityNumber::Error, "", 2),
            log("", SeverityNumber::Error, "unnamed", 3),
        ],
    );
    let out = map_logs(
        &request,
        &settings(json!({"logEventNameAllowlist": ["auth_*"]})),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (1, 2));
}

#[test]
fn heartbeat_events_refresh_regardless_of_severity_and_allowlist() {
    let request = logs(
        checkout(),
        vec![log(
            "health.heartbeat",
            SeverityNumber::Info,
            "",
            OBSERVED_NANOS,
        )],
    );
    let out = map_logs(
        &request,
        &settings(
            json!({"heartbeatEventName": "health.heartbeat", "logEventNameAllowlist": ["other"]}),
        ),
        RECEIVED,
    );
    assert_eq!((out.accepted, out.rejected), (1, 0));
    assert_eq!(
        ids(&out),
        vec!["svc:checkout", "hb:svc:checkout", "rel-hb:svc:checkout"]
    );
    let admitted = map_logs(
        &logs(
            checkout(),
            vec![log(
                "health.heartbeat",
                SeverityNumber::Error,
                "",
                OBSERVED_NANOS,
            )],
        ),
        &settings(json!({"heartbeatEventName": "health.heartbeat"})),
        RECEIVED,
    );
    assert!(ids(&admitted).iter().any(|id| id.starts_with("log:")));
    assert!(ids(&admitted).contains(&"hb:svc:checkout"));
}

#[test]
fn log_bodies_hash_stably() {
    assert_eq!(hash_text("payment refused"), hash_text("payment refused"));
    assert_ne!(hash_text("payment refused"), hash_text("payment accepted"));
    assert_eq!(hash_text(""), "cbf29ce484222325");
}

#[test]
fn globs_match_only_star_wildcards() {
    for (pattern, text, expected) in [
        ("*", "anything", true),
        ("*", "", true),
        ("latency_p99_ms", "latency_p99_ms", true),
        ("latency_p99_ms", "latency_p99", false),
        ("latency_*", "latency_p99", true),
        ("latency_*", "latency_", true),
        ("latency_*", "error_rate", false),
        ("*_p99", "latency_p99", true),
        ("*_p99", "latency_p95", false),
        ("*lat*", "p99_latency_ms", true),
        ("a*b*c", "aXbYc", true),
        ("a*b*c", "aXcYb", false),
        ("a*a", "a", false),
        ("a*a", "aa", true),
        ("lat?ncy", "latency", false),
        ("lat?ncy", "lat?ncy", true),
        ("**", "x", true),
    ] {
        assert_eq!(glob_match(pattern, text), expected, "{pattern} vs {text}");
    }
    assert!(!allowlist_matches(&[], "anything"));
}
