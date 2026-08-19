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

//! Project allowlisted OTLP records into bounded graph elements.

use drasi_core::models::{ElementPropertyMap, ElementValue};
use ordered_float::OrderedFloat;

use crate::config::OtelSourceConfig;
use crate::lifecycle::{ElementCategory, ProjectedElement, ProjectedKind};
use crate::otlp::proto::collector::logs::v1::ExportLogsServiceRequest;
use crate::otlp::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use crate::otlp::proto::collector::trace::v1::ExportTraceServiceRequest;
use crate::otlp::proto::common::v1::{any_value, AnyValue, KeyValue};
use crate::otlp::proto::logs::v1::SeverityNumber;
use crate::otlp::proto::metrics::v1::{metric, number_data_point};
use crate::otlp::proto::resource::v1::Resource;
use crate::otlp::proto::trace::v1::span;

/// Result of mapping one OTLP export request.
#[derive(Debug, Default)]
pub struct MapOutcome {
    pub elements: Vec<ProjectedElement>,
    pub accepted: u64,
    pub rejected: u64,
}

const ORIGIN_ATTR: &str = "drasi.source.origin";
const ORIGIN_DERIVED: &str = "derived";

/// Convert an OTLP unix-nano timestamp to Drasi millisecond `effective_from`.
pub fn nanos_to_millis(nanos: u64) -> u64 {
    if nanos == 0 {
        return 0;
    }
    nanos / 1_000_000
}

pub fn map_metrics(
    request: &ExportMetricsServiceRequest,
    config: &OtelSourceConfig,
    received_at_millis: u64,
) -> MapOutcome {
    let mut out = MapOutcome::default();
    let mut next_group = 0u32;
    for rm in &request.resource_metrics {
        if reject_derived(config, rm.resource.as_ref(), &[]) {
            out.rejected += metric_point_count(rm).max(1);
            continue;
        }
        let Some(service) = service_from_resource(rm.resource.as_ref()) else {
            out.rejected += metric_point_count(rm).max(1);
            continue;
        };
        for sm in &rm.scope_metrics {
            for metric in &sm.metrics {
                match metric.data.as_ref() {
                    Some(metric::Data::Gauge(gauge)) => {
                        for dp in &gauge.data_points {
                            let group = next_group;
                            next_group += 1;
                            project_number_metric(
                                &mut out,
                                config,
                                &service,
                                rm.resource.as_ref(),
                                &metric.name,
                                &metric.unit,
                                dp.value.as_ref(),
                                dp.time_unix_nano,
                                &dp.attributes,
                                received_at_millis,
                                group,
                            );
                        }
                    }
                    Some(metric::Data::Sum(sum)) => {
                        for dp in &sum.data_points {
                            let group = next_group;
                            next_group += 1;
                            project_number_metric(
                                &mut out,
                                config,
                                &service,
                                rm.resource.as_ref(),
                                &metric.name,
                                &metric.unit,
                                dp.value.as_ref(),
                                dp.time_unix_nano,
                                &dp.attributes,
                                received_at_millis,
                                group,
                            );
                        }
                    }
                    _ => {
                        out.rejected += 1;
                    }
                }
            }
        }
    }
    out
}

pub fn map_traces(
    request: &ExportTraceServiceRequest,
    config: &OtelSourceConfig,
    received_at_millis: u64,
) -> MapOutcome {
    let mut out = MapOutcome::default();
    let allowed_kinds = allowed_span_kinds(config);
    let mut next_group = 0u32;
    for rs in &request.resource_spans {
        if reject_derived(config, rs.resource.as_ref(), &[]) {
            out.rejected += span_count(rs).max(1);
            continue;
        }
        let Some(caller) = service_from_resource(rs.resource.as_ref()) else {
            out.rejected += span_count(rs).max(1);
            continue;
        };
        for ss in &rs.scope_spans {
            for span in &ss.spans {
                if reject_derived(config, None, &span.attributes) {
                    out.rejected += 1;
                    continue;
                }
                if !allowed_kinds.contains(&span.kind) {
                    out.rejected += 1;
                    continue;
                }
                let Some(dest_name) = destination_service(config, &span.attributes) else {
                    out.rejected += 1;
                    continue;
                };
                let dest = ServiceIdentity::from_destination(&dest_name, &caller);
                let observed = effective_time(span.start_time_unix_nano, received_at_millis);
                let group = next_group;
                next_group += 1;
                push_service(
                    &mut out.elements,
                    &caller,
                    rs.resource.as_ref(),
                    observed,
                    group,
                );
                push_service(&mut out.elements, &dest, None, observed, group);
                let mut props = ElementPropertyMap::new();
                insert_string(&mut props, "lastSeen", &millis_iso(observed));
                out.elements.push(ProjectedElement {
                    id: format!("dep:{}:{}", caller.id, dest.id),
                    labels: vec!["DEPENDS_ON".to_string()],
                    properties: props,
                    kind: ProjectedKind::Relation {
                        from: caller.id.clone(),
                        to: dest.id.clone(),
                    },
                    effective_from: observed,
                    category: ElementCategory::DependsOn,
                    ttl_secs: Some(config.dependency_ttl_secs),
                    group,
                });
                out.accepted += 1;
            }
        }
    }
    out
}

pub fn map_logs(
    request: &ExportLogsServiceRequest,
    config: &OtelSourceConfig,
    received_at_millis: u64,
) -> MapOutcome {
    let mut out = MapOutcome::default();
    let min_severity = parse_min_severity(&config.log_min_severity);
    let mut next_group = 0u32;
    for rl in &request.resource_logs {
        if reject_derived(config, rl.resource.as_ref(), &[]) {
            out.rejected += log_count(rl).max(1);
            continue;
        }
        let Some(service) = service_from_resource(rl.resource.as_ref()) else {
            out.rejected += log_count(rl).max(1);
            continue;
        };
        for sl in &rl.scope_logs {
            for rec in &sl.log_records {
                if reject_derived(config, None, &rec.attributes) {
                    out.rejected += 1;
                    continue;
                }
                let observed = effective_time(rec.time_unix_nano, received_at_millis);
                let group = next_group;
                next_group += 1;
                let is_heartbeat = config
                    .heartbeat_event_name
                    .as_ref()
                    .is_some_and(|name| rec.event_name == *name);
                if is_heartbeat {
                    push_service(
                        &mut out.elements,
                        &service,
                        rl.resource.as_ref(),
                        observed,
                        group,
                    );
                    push_heartbeat(&mut out.elements, &service, observed, group);
                    out.accepted += 1;
                }

                if rec.severity_number < min_severity {
                    if !is_heartbeat {
                        out.rejected += 1;
                    }
                    continue;
                }
                if !config.log_event_name_allowlist.is_empty()
                    && !allowlist_matches(&config.log_event_name_allowlist, &rec.event_name)
                {
                    if !is_heartbeat {
                        out.rejected += 1;
                    }
                    continue;
                }

                let body = any_value_string(rec.body.as_ref()).unwrap_or_default();
                let key_part = if rec.event_name.is_empty() {
                    hash_text(&body)
                } else {
                    rec.event_name.clone()
                };
                let log_id = format!("log:{}:{}:{}", service.id, key_part, rec.time_unix_nano);
                push_service(
                    &mut out.elements,
                    &service,
                    rl.resource.as_ref(),
                    observed,
                    group,
                );
                let mut props = ElementPropertyMap::new();
                insert_string(&mut props, "service", &service.name);
                insert_string(&mut props, "severity", &rec.severity_text);
                insert_string(&mut props, "body", &body);
                if !rec.event_name.is_empty() {
                    insert_string(&mut props, "eventName", &rec.event_name);
                }
                insert_string(&mut props, "observedAt", &millis_iso(observed));
                out.elements.push(ProjectedElement {
                    id: log_id.clone(),
                    labels: vec!["LogEvent".to_string()],
                    properties: props,
                    kind: ProjectedKind::Node,
                    effective_from: observed,
                    category: ElementCategory::LogEvent,
                    ttl_secs: Some(config.log_event_ttl_secs),
                    group,
                });
                out.elements.push(ProjectedElement {
                    id: format!("emits:{log_id}"),
                    labels: vec!["EMITS".to_string()],
                    properties: ElementPropertyMap::new(),
                    kind: ProjectedKind::Relation {
                        from: service.id.clone(),
                        to: log_id,
                    },
                    effective_from: observed,
                    category: ElementCategory::Emits,
                    ttl_secs: Some(config.log_event_ttl_secs),
                    group,
                });
                out.accepted += 1;
            }
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn project_number_metric(
    out: &mut MapOutcome,
    config: &OtelSourceConfig,
    service: &ServiceIdentity,
    resource: Option<&Resource>,
    name: &str,
    unit: &str,
    value: Option<&number_data_point::Value>,
    time_unix_nano: u64,
    attributes: &[KeyValue],
    received_at_millis: u64,
    group: u32,
) {
    if reject_derived(config, None, attributes) {
        out.rejected += 1;
        return;
    }
    let Some(number) = number_value(value) else {
        out.rejected += 1;
        return;
    };
    let observed = effective_time(time_unix_nano, received_at_millis);
    let is_heartbeat = config
        .heartbeat_metric
        .as_ref()
        .is_some_and(|hb| hb == name);
    if is_heartbeat {
        push_service(&mut out.elements, service, resource, observed, group);
        push_heartbeat(&mut out.elements, service, observed, group);
        out.accepted += 1;
    }
    if !allowlist_matches(&config.metric_allowlist, name) {
        if !is_heartbeat {
            out.rejected += 1;
        }
        return;
    }

    let identity_suffix = metric_identity_suffix(config, attributes);
    let metric_id = format!("metric:{}:{name}{identity_suffix}", service.id);
    push_service(&mut out.elements, service, resource, observed, group);
    let mut props = ElementPropertyMap::new();
    insert_string(&mut props, "name", name);
    insert_string(&mut props, "unit", unit);
    props.insert("value", ElementValue::Float(OrderedFloat(number)));
    insert_string(&mut props, "observedAt", &millis_iso(observed));
    insert_string(&mut props, "receivedAt", &millis_iso(received_at_millis));
    out.elements.push(ProjectedElement {
        id: metric_id.clone(),
        labels: vec!["Metric".to_string()],
        properties: props,
        kind: ProjectedKind::Node,
        effective_from: observed,
        category: ElementCategory::Metric,
        ttl_secs: None,
        group,
    });
    out.elements.push(ProjectedElement {
        id: format!("reports:{metric_id}"),
        labels: vec!["REPORTS".to_string()],
        properties: ElementPropertyMap::new(),
        kind: ProjectedKind::Relation {
            from: service.id.clone(),
            to: metric_id,
        },
        effective_from: observed,
        category: ElementCategory::Reports,
        ttl_secs: None,
        group,
    });
    out.accepted += 1;
}

#[derive(Debug, Clone)]
struct ServiceIdentity {
    id: String,
    name: String,
    namespace: Option<String>,
    environment: Option<String>,
    instance_id: Option<String>,
}

impl ServiceIdentity {
    fn from_name(name: &str) -> Self {
        Self {
            id: format!("svc:{name}"),
            name: name.to_string(),
            namespace: None,
            environment: None,
            instance_id: None,
        }
    }

    fn from_destination(name: &str, caller: &ServiceIdentity) -> Self {
        if name.contains('/') {
            let (namespace, short) = name.split_once('/').unwrap_or(("", name));
            return Self {
                id: format!("svc:{name}"),
                name: short.to_string(),
                namespace: if namespace.is_empty() {
                    None
                } else {
                    Some(namespace.to_string())
                },
                environment: None,
                instance_id: None,
            };
        }
        if let Some(ns) = &caller.namespace {
            return Self {
                id: format!("svc:{ns}/{name}"),
                name: name.to_string(),
                namespace: Some(ns.clone()),
                environment: None,
                instance_id: None,
            };
        }
        Self::from_name(name)
    }
}

fn service_from_resource(resource: Option<&Resource>) -> Option<ServiceIdentity> {
    let attrs = &resource?.attributes;
    let name = attr_string(attrs, "service.name")?;
    let namespace = attr_string(attrs, "service.namespace");
    let id = match &namespace {
        Some(ns) if !ns.is_empty() => format!("svc:{ns}/{name}"),
        _ => format!("svc:{name}"),
    };
    Some(ServiceIdentity {
        id,
        name,
        namespace,
        environment: attr_string(attrs, "deployment.environment.name")
            .or_else(|| attr_string(attrs, "deployment.environment")),
        instance_id: attr_string(attrs, "service.instance.id"),
    })
}

fn push_service(
    elements: &mut Vec<ProjectedElement>,
    service: &ServiceIdentity,
    resource: Option<&Resource>,
    observed: u64,
    group: u32,
) {
    if elements.iter().any(|e| e.id == service.id) {
        return;
    }
    let mut props = ElementPropertyMap::new();
    insert_string(&mut props, "name", &service.name);
    if let Some(ns) = &service.namespace {
        insert_string(&mut props, "namespace", ns);
    }
    if let Some(env) = &service.environment {
        insert_string(&mut props, "environment", env);
    }
    if let Some(instance) = &service.instance_id {
        insert_string(&mut props, "instanceId", instance);
    }
    if let Some(resource) = resource {
        if let Some(version) = attr_string(&resource.attributes, "service.version") {
            insert_string(&mut props, "version", &version);
        }
    }
    insert_string(&mut props, "lastSeen", &millis_iso(observed));
    elements.push(ProjectedElement {
        id: service.id.clone(),
        labels: vec!["Service".to_string()],
        properties: props,
        kind: ProjectedKind::Node,
        effective_from: observed,
        category: ElementCategory::Service,
        ttl_secs: None,
        group,
    });
}

fn push_heartbeat(
    elements: &mut Vec<ProjectedElement>,
    service: &ServiceIdentity,
    observed: u64,
    group: u32,
) {
    let hb_id = format!("hb:{}", service.id);
    let mut props = ElementPropertyMap::new();
    insert_string(&mut props, "lastSeen", &millis_iso(observed));
    elements.push(ProjectedElement {
        id: hb_id.clone(),
        labels: vec!["Heartbeat".to_string()],
        properties: props,
        kind: ProjectedKind::Node,
        effective_from: observed,
        category: ElementCategory::Heartbeat,
        ttl_secs: None,
        group,
    });
    elements.push(ProjectedElement {
        id: format!("rel-hb:{}", service.id),
        labels: vec!["HEARTBEAT".to_string()],
        properties: ElementPropertyMap::new(),
        kind: ProjectedKind::Relation {
            from: service.id.clone(),
            to: hb_id,
        },
        effective_from: observed,
        category: ElementCategory::HeartbeatRel,
        ttl_secs: None,
        group,
    });
}

fn reject_derived(
    config: &OtelSourceConfig,
    resource: Option<&Resource>,
    extra: &[KeyValue],
) -> bool {
    if !config.reject_derived {
        return false;
    }
    if let Some(resource) = resource {
        if attr_string(&resource.attributes, ORIGIN_ATTR).as_deref() == Some(ORIGIN_DERIVED) {
            return true;
        }
    }
    attr_string(extra, ORIGIN_ATTR).as_deref() == Some(ORIGIN_DERIVED)
}

fn destination_service(config: &OtelSourceConfig, attrs: &[KeyValue]) -> Option<String> {
    for key in &config.destination_attributes {
        if let Some(value) = attr_string(attrs, key) {
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

fn allowed_span_kinds(config: &OtelSourceConfig) -> Vec<i32> {
    config
        .span_kinds
        .iter()
        .filter_map(|kind| match kind.to_ascii_uppercase().as_str() {
            "UNSPECIFIED" => Some(span::SpanKind::Unspecified as i32),
            "INTERNAL" => Some(span::SpanKind::Internal as i32),
            "SERVER" => Some(span::SpanKind::Server as i32),
            "CLIENT" => Some(span::SpanKind::Client as i32),
            "PRODUCER" => Some(span::SpanKind::Producer as i32),
            "CONSUMER" => Some(span::SpanKind::Consumer as i32),
            _ => None,
        })
        .collect()
}

fn metric_identity_suffix(config: &OtelSourceConfig, attributes: &[KeyValue]) -> String {
    if config.metric_identity_attributes.is_empty() {
        return String::new();
    }
    let mut parts: Vec<String> = config
        .metric_identity_attributes
        .iter()
        .filter_map(|key| attr_string(attributes, key).map(|value| format!("{key}={value}")))
        .collect();
    parts.sort();
    if parts.is_empty() {
        String::new()
    } else {
        format!(":{}", parts.join(","))
    }
}

fn number_value(value: Option<&number_data_point::Value>) -> Option<f64> {
    match value {
        Some(number_data_point::Value::AsDouble(v)) => Some(*v),
        Some(number_data_point::Value::AsInt(v)) => Some(*v as f64),
        None => None,
    }
}

fn attr_string(attrs: &[KeyValue], key: &str) -> Option<String> {
    attrs.iter().find_map(|kv| {
        if kv.key != key {
            return None;
        }
        any_value_string(kv.value.as_ref())
    })
}

fn any_value_string(value: Option<&AnyValue>) -> Option<String> {
    match value?.value.as_ref()? {
        any_value::Value::StringValue(s) => Some(s.clone()),
        any_value::Value::BoolValue(b) => Some(b.to_string()),
        any_value::Value::IntValue(i) => Some(i.to_string()),
        any_value::Value::DoubleValue(d) => Some(d.to_string()),
        _ => None,
    }
}

fn insert_string(props: &mut ElementPropertyMap, key: &str, value: &str) {
    props.insert(key, ElementValue::String(std::sync::Arc::from(value)));
}

fn effective_time(nanos: u64, received_at_millis: u64) -> u64 {
    let millis = nanos_to_millis(nanos);
    if millis == 0 {
        received_at_millis
    } else {
        millis
    }
}

fn millis_iso(millis: u64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp_millis(millis as i64)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| millis.to_string())
}

fn hash_text(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn metric_point_count(rm: &crate::otlp::proto::metrics::v1::ResourceMetrics) -> u64 {
    rm.scope_metrics
        .iter()
        .flat_map(|sm| sm.metrics.iter())
        .map(|metric| match metric.data.as_ref() {
            Some(metric::Data::Gauge(g)) => g.data_points.len(),
            Some(metric::Data::Sum(s)) => s.data_points.len(),
            _ => 1,
        })
        .sum::<usize>() as u64
}

fn span_count(rs: &crate::otlp::proto::trace::v1::ResourceSpans) -> u64 {
    rs.scope_spans
        .iter()
        .map(|ss| ss.spans.len())
        .sum::<usize>() as u64
}

fn log_count(rl: &crate::otlp::proto::logs::v1::ResourceLogs) -> u64 {
    rl.scope_logs
        .iter()
        .map(|sl| sl.log_records.len())
        .sum::<usize>() as u64
}

/// `*` matches any sequence. Empty allowlist matches nothing.
fn allowlist_matches(patterns: &[String], value: &str) -> bool {
    patterns.iter().any(|pattern| glob_match(pattern, value))
}

fn glob_match(pattern: &str, text: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return pattern == text;
    }

    let parts: Vec<&str> = pattern.split('*').collect();
    let mut rest = text;

    if let Some(first) = parts.first() {
        if !first.is_empty() {
            if !rest.starts_with(first) {
                return false;
            }
            rest = &rest[first.len()..];
        }
    }

    let Some(last) = parts.last() else {
        return true;
    };
    for part in &parts[1..parts.len().saturating_sub(1)] {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(idx) => rest = &rest[idx + part.len()..],
            None => return false,
        }
    }
    if last.is_empty() {
        true
    } else {
        rest.ends_with(last)
    }
}

fn parse_min_severity(name: &str) -> i32 {
    match name.to_ascii_uppercase().as_str() {
        "TRACE" => SeverityNumber::Trace as i32,
        "DEBUG" => SeverityNumber::Debug as i32,
        "INFO" => SeverityNumber::Info as i32,
        "WARN" | "WARNING" => SeverityNumber::Warn as i32,
        "FATAL" => SeverityNumber::Fatal as i32,
        _ => SeverityNumber::Error as i32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::otlp::proto::collector::metrics::v1::ExportMetricsServiceRequest;
    use crate::otlp::proto::common::v1::KeyValue;
    use crate::otlp::proto::metrics::v1::{
        Gauge, Metric, NumberDataPoint, ResourceMetrics, ScopeMetrics,
    };
    use crate::otlp::proto::resource::v1::Resource;
    use crate::otlp::proto::trace::v1::{ResourceSpans, ScopeSpans, Span};

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
            attributes: vec![
                kv("service.name", "checkout"),
                kv("deployment.environment.name", "prod"),
            ],
            dropped_attributes_count: 0,
        }
    }

    fn allowlist_config() -> OtelSourceConfig {
        OtelSourceConfig {
            metric_allowlist: vec!["latency_p99_ms".to_string()],
            ..OtelSourceConfig::default()
        }
    }

    #[test]
    fn nanos_are_converted_to_millis() {
        assert_eq!(
            nanos_to_millis(1_713_456_789_000_000_000),
            1_713_456_789_000
        );
        assert_eq!(nanos_to_millis(0), 0);
    }

    #[test]
    fn gauge_projects_service_metric_and_reports() {
        let request = ExportMetricsServiceRequest {
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
                                time_unix_nano: 1_713_456_789_000_000_000,
                                exemplars: vec![],
                                flags: 0,
                                value: Some(number_data_point::Value::AsDouble(920.0)),
                            }],
                        })),
                        metadata: vec![],
                    }],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };
        let out = map_metrics(&request, &allowlist_config(), 2_000);
        assert_eq!(out.rejected, 0);
        assert_eq!(out.accepted, 1);
        assert!(out.elements.iter().any(|e| e.id == "svc:checkout"));
        assert!(out
            .elements
            .iter()
            .any(|e| e.labels.iter().any(|l| l == "Metric")));
        assert!(out
            .elements
            .iter()
            .any(|e| e.labels.iter().any(|l| l == "REPORTS")));
    }

    #[test]
    fn unknown_metric_is_rejected() {
        let mut request = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(checkout_resource()),
                scope_metrics: vec![ScopeMetrics {
                    scope: None,
                    metrics: vec![Metric {
                        name: "other".to_string(),
                        description: String::new(),
                        unit: String::new(),
                        data: Some(metric::Data::Gauge(Gauge {
                            data_points: vec![NumberDataPoint {
                                attributes: vec![],
                                start_time_unix_nano: 0,
                                time_unix_nano: 1,
                                exemplars: vec![],
                                flags: 0,
                                value: Some(number_data_point::Value::AsDouble(1.0)),
                            }],
                        })),
                        metadata: vec![],
                    }],
                    schema_url: String::new(),
                }],
                schema_url: String::new(),
            }],
        };
        let out = map_metrics(&request, &allowlist_config(), 2_000);
        assert_eq!(out.accepted, 0);
        assert!(out.rejected > 0);
        request.resource_metrics[0]
            .resource
            .as_mut()
            .unwrap()
            .attributes = vec![
            kv("service.name", "checkout"),
            kv(ORIGIN_ATTR, ORIGIN_DERIVED),
        ];
        let rejected = map_metrics(&request, &allowlist_config(), 2_000);
        assert!(rejected.rejected > 0);
        assert!(rejected.elements.is_empty());
    }

    #[test]
    fn client_span_projects_depends_on() {
        let request = ExportTraceServiceRequest {
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
                        name: "call".to_string(),
                        kind: span::SpanKind::Client as i32,
                        start_time_unix_nano: 1_713_456_789_000_000_000,
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
        };
        let out = map_traces(&request, &OtelSourceConfig::default(), 2_000);
        assert_eq!(out.accepted, 1);
        assert!(out
            .elements
            .iter()
            .any(|e| e.id == "dep:svc:checkout:svc:payments"));
    }

    #[test]
    fn namespaced_caller_prefixes_bare_destination() {
        let mut resource = checkout_resource();
        resource.attributes.push(kv("service.namespace", "shop"));
        let request = ExportTraceServiceRequest {
            resource_spans: vec![ResourceSpans {
                resource: Some(resource),
                scope_spans: vec![ScopeSpans {
                    scope: None,
                    spans: vec![Span {
                        trace_id: vec![1; 16],
                        span_id: vec![2; 8],
                        trace_state: String::new(),
                        parent_span_id: vec![],
                        flags: 0,
                        name: "call".to_string(),
                        kind: span::SpanKind::Client as i32,
                        start_time_unix_nano: 1_713_456_789_000_000_000,
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
        };
        let out = map_traces(&request, &OtelSourceConfig::default(), 2_000);
        assert!(out
            .elements
            .iter()
            .any(|e| e.id == "dep:svc:shop/checkout:svc:shop/payments"));
        assert!(out.elements.iter().any(|e| e.id == "svc:shop/payments"));
    }

    #[test]
    fn log_body_hash_is_stable() {
        assert_eq!(hash_text("payment refused"), hash_text("payment refused"));
        assert_ne!(hash_text("payment refused"), hash_text("payment accepted"));
    }

    fn gauge_named(name: &str) -> ExportMetricsServiceRequest {
        ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(checkout_resource()),
                scope_metrics: vec![ScopeMetrics {
                    scope: None,
                    metrics: vec![Metric {
                        name: name.to_string(),
                        description: String::new(),
                        unit: "ms".to_string(),
                        data: Some(metric::Data::Gauge(Gauge {
                            data_points: vec![NumberDataPoint {
                                attributes: vec![],
                                start_time_unix_nano: 0,
                                time_unix_nano: 1_713_456_789_000_000_000,
                                exemplars: vec![],
                                flags: 0,
                                value: Some(number_data_point::Value::AsDouble(1.0)),
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

    #[test]
    fn star_allowlist_accepts_any_metric() {
        let config = OtelSourceConfig {
            metric_allowlist: vec!["*".to_string()],
            ..OtelSourceConfig::default()
        };
        let out = map_metrics(&gauge_named("anything.goes"), &config, 2_000);
        assert_eq!(out.accepted, 1);
        assert_eq!(out.rejected, 0);
    }

    #[test]
    fn glob_allowlist_matches_prefix() {
        let config = OtelSourceConfig {
            metric_allowlist: vec!["latency_*".to_string()],
            ..OtelSourceConfig::default()
        };
        assert_eq!(
            map_metrics(&gauge_named("latency_p99_ms"), &config, 2_000).accepted,
            1
        );
        assert_eq!(
            map_metrics(&gauge_named("error_rate"), &config, 2_000).accepted,
            0
        );
    }

    #[test]
    fn missing_service_name_is_rejected() {
        let request = ExportMetricsServiceRequest {
            resource_metrics: vec![ResourceMetrics {
                resource: Some(Resource {
                    attributes: vec![],
                    dropped_attributes_count: 0,
                }),
                scope_metrics: vec![],
                schema_url: String::new(),
            }],
        };
        let out = map_metrics(&request, &allowlist_config(), 2_000);
        assert!(out.rejected > 0);
        assert!(out.elements.is_empty());
    }
}
