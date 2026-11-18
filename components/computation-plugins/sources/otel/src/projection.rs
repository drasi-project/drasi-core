// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Pure projection of OTLP export requests into graph elements.
//!
//! Projection reads only the request, the [`Projection`] settings and the
//! receipt time. Whether an element is an insert or an update, and whether a
//! cardinality cap admits it, is decided afterwards by
//! [`crate::state::ProjectionState`].
//!
//! Elements are emitted in *groups*: every element one record produces (for
//! example a Service, its Metric and the REPORTS relation between them) shares
//! a group, and a group is admitted or dropped as a whole, so the graph never
//! receives a relation without its endpoints.

use crate::config::Projection;
use crate::otlp::proto::collector::logs::v1::ExportLogsServiceRequest;
use crate::otlp::proto::collector::metrics::v1::ExportMetricsServiceRequest;
use crate::otlp::proto::collector::trace::v1::ExportTraceServiceRequest;
use crate::otlp::proto::common::v1::{any_value, AnyValue, KeyValue};
use crate::otlp::proto::logs::v1::ResourceLogs;
use crate::otlp::proto::metrics::v1::{metric, number_data_point, ResourceMetrics};
use crate::otlp::proto::resource::v1::Resource;
use crate::otlp::proto::trace::v1::ResourceSpans;
use crate::state::{ElementCategory, ProjectedElement, ProjectedKind};
use drasi_core_types::models::{ElementPropertyMap, ElementValue, MAX_REASONABLE_MILLIS_TIMESTAMP};
use ordered_float::OrderedFloat;
use std::sync::Arc;

/// Resource or record attribute marking data that Drasi itself exported.
pub const SOURCE_ORIGIN_ATTRIBUTE: &str = "drasi.source.origin";
/// [`SOURCE_ORIGIN_ATTRIBUTE`] value that `rejectDerived` drops.
pub const SOURCE_ORIGIN_DERIVED: &str = "derived";

/// The projection of one OTLP export request.
#[derive(Debug, Default)]
pub struct MapOutcome {
    /// Projected elements, in request order, tagged with their group.
    pub elements: Vec<ProjectedElement>,
    /// Records that produced at least one element.
    pub accepted: u64,
    /// Records refused by an allowlist, filter, schema rule or timestamp check.
    pub rejected: u64,
}

/// Converts an OTLP unix-nanosecond timestamp to milliseconds.
pub fn nanos_to_millis(nanos: u64) -> u64 {
    nanos / 1_000_000
}

/// Projects allowlisted gauge and sum data points. Histograms, exponential
/// histograms and summaries are rejected: send a pre-aggregated gauge instead.
pub fn map_metrics(
    request: &ExportMetricsServiceRequest,
    settings: &Projection,
    received_at_millis: u64,
) -> MapOutcome {
    let mut out = MapOutcome::default();
    let mut next_group = 0u32;
    for rm in &request.resource_metrics {
        let Some(service) = admitted_service(settings, rm.resource.as_ref()) else {
            out.rejected += metric_point_count(rm).max(1);
            continue;
        };
        for sm in &rm.scope_metrics {
            for metric in &sm.metrics {
                let points: Vec<_> = match metric.data.as_ref() {
                    Some(metric::Data::Gauge(gauge)) => gauge
                        .data_points
                        .iter()
                        .map(|point| {
                            (
                                point.value.as_ref(),
                                point.time_unix_nano,
                                &point.attributes,
                            )
                        })
                        .collect(),
                    Some(metric::Data::Sum(sum)) => sum
                        .data_points
                        .iter()
                        .map(|point| {
                            (
                                point.value.as_ref(),
                                point.time_unix_nano,
                                &point.attributes,
                            )
                        })
                        .collect(),
                    _ => {
                        out.rejected += 1;
                        continue;
                    }
                };
                for (value, time_unix_nano, attributes) in points {
                    let group = next_group;
                    next_group += 1;
                    project_number_metric(
                        &mut out,
                        settings,
                        &service,
                        rm.resource.as_ref(),
                        &metric.name,
                        &metric.unit,
                        NumberPoint {
                            value,
                            time_unix_nano,
                            attributes,
                        },
                        received_at_millis,
                        group,
                    );
                }
            }
        }
    }
    out
}

/// Projects spans of an allowed kind that name a destination service as
/// `(:Service)-[:DEPENDS_ON]->(:Service)`.
pub fn map_traces(
    request: &ExportTraceServiceRequest,
    settings: &Projection,
    received_at_millis: u64,
) -> MapOutcome {
    let mut out = MapOutcome::default();
    let mut next_group = 0u32;
    for rs in &request.resource_spans {
        let Some(caller) = admitted_service(settings, rs.resource.as_ref()) else {
            out.rejected += span_count(rs).max(1);
            continue;
        };
        for ss in &rs.scope_spans {
            for span in &ss.spans {
                if is_derived(settings, &span.attributes)
                    || !settings.span_kinds.contains(&span.kind)
                {
                    out.rejected += 1;
                    continue;
                }
                let Some(dest_name) = destination_service(settings, &span.attributes) else {
                    out.rejected += 1;
                    continue;
                };
                let Some(observed) = effective_time(span.start_time_unix_nano, received_at_millis)
                else {
                    out.rejected += 1;
                    continue;
                };
                let dest = ServiceIdentity::from_destination(&dest_name, &caller);
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
                    labels: vec!["DEPENDS_ON".into()],
                    properties: props,
                    kind: ProjectedKind::Relation {
                        from: caller.id.clone(),
                        to: dest.id.clone(),
                    },
                    effective_from: observed,
                    category: ElementCategory::DependsOn,
                    ttl_secs: Some(settings.dependency_ttl_secs),
                    group,
                });
                out.accepted += 1;
            }
        }
    }
    out
}

/// Projects log records at or above the minimum severity (optionally limited
/// by event-name allowlist) as `(:Service)-[:EMITS]->(:LogEvent)`, and
/// heartbeat events as `(:Service)-[:HEARTBEAT]->(:Heartbeat)`.
pub fn map_logs(
    request: &ExportLogsServiceRequest,
    settings: &Projection,
    received_at_millis: u64,
) -> MapOutcome {
    let mut out = MapOutcome::default();
    let mut next_group = 0u32;
    for rl in &request.resource_logs {
        let Some(service) = admitted_service(settings, rl.resource.as_ref()) else {
            out.rejected += log_count(rl).max(1);
            continue;
        };
        for sl in &rl.scope_logs {
            for rec in &sl.log_records {
                if is_derived(settings, &rec.attributes) {
                    out.rejected += 1;
                    continue;
                }
                let Some(observed) = effective_time(rec.time_unix_nano, received_at_millis) else {
                    out.rejected += 1;
                    continue;
                };
                let group = next_group;
                next_group += 1;
                let is_heartbeat = settings
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
                }
                let admitted = rec.severity_number >= settings.log_min_severity
                    && (settings.log_event_name_allowlist.is_empty()
                        || allowlist_matches(&settings.log_event_name_allowlist, &rec.event_name));
                if !admitted {
                    if is_heartbeat {
                        out.accepted += 1;
                    } else {
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
                    labels: vec!["LogEvent".into()],
                    properties: props,
                    kind: ProjectedKind::Node,
                    effective_from: observed,
                    category: ElementCategory::LogEvent,
                    ttl_secs: Some(settings.log_event_ttl_secs),
                    group,
                });
                out.elements.push(ProjectedElement {
                    id: format!("emits:{log_id}"),
                    labels: vec!["EMITS".into()],
                    properties: ElementPropertyMap::new(),
                    kind: ProjectedKind::Relation {
                        from: service.id.clone(),
                        to: log_id,
                    },
                    effective_from: observed,
                    category: ElementCategory::Emits,
                    ttl_secs: Some(settings.log_event_ttl_secs),
                    group,
                });
                out.accepted += 1;
            }
        }
    }
    out
}

/// One gauge or sum data point.
struct NumberPoint<'a> {
    value: Option<&'a number_data_point::Value>,
    time_unix_nano: u64,
    attributes: &'a [KeyValue],
}

#[allow(clippy::too_many_arguments)]
fn project_number_metric(
    out: &mut MapOutcome,
    settings: &Projection,
    service: &ServiceIdentity,
    resource: Option<&Resource>,
    name: &str,
    unit: &str,
    point: NumberPoint<'_>,
    received_at_millis: u64,
    group: u32,
) {
    if is_derived(settings, point.attributes) {
        out.rejected += 1;
        return;
    }
    let Some(number) = number_value(point.value) else {
        out.rejected += 1;
        return;
    };
    let Some(observed) = effective_time(point.time_unix_nano, received_at_millis) else {
        out.rejected += 1;
        return;
    };
    let is_heartbeat = settings.heartbeat_metric.as_deref() == Some(name);
    if is_heartbeat {
        push_service(&mut out.elements, service, resource, observed, group);
        push_heartbeat(&mut out.elements, service, observed, group);
    }
    if !allowlist_matches(&settings.metric_allowlist, name) {
        if is_heartbeat {
            out.accepted += 1;
        } else {
            out.rejected += 1;
        }
        return;
    }

    let identity_suffix = metric_identity_suffix(settings, point.attributes);
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
        labels: vec!["Metric".into()],
        properties: props,
        kind: ProjectedKind::Node,
        effective_from: observed,
        category: ElementCategory::Metric,
        ttl_secs: None,
        group,
    });
    out.elements.push(ProjectedElement {
        id: format!("reports:{metric_id}"),
        labels: vec!["REPORTS".into()],
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
            name: name.into(),
            namespace: None,
            environment: None,
            instance_id: None,
        }
    }

    /// The callee named by a span attribute. A `namespace/name` value is split
    /// as-is; a bare name inherits the caller's namespace.
    fn from_destination(name: &str, caller: &ServiceIdentity) -> Self {
        if let Some((namespace, short)) = name.split_once('/') {
            return Self {
                id: format!("svc:{name}"),
                name: short.into(),
                namespace: (!namespace.is_empty()).then(|| namespace.into()),
                environment: None,
                instance_id: None,
            };
        }
        match &caller.namespace {
            Some(namespace) => Self {
                id: format!("svc:{namespace}/{name}"),
                name: name.into(),
                namespace: Some(namespace.clone()),
                environment: None,
                instance_id: None,
            },
            None => Self::from_name(name),
        }
    }
}

/// The resource's service, unless the resource is derived (and `rejectDerived`
/// is set) or has no `service.name`.
fn admitted_service(settings: &Projection, resource: Option<&Resource>) -> Option<ServiceIdentity> {
    let resource = resource?;
    if is_derived(settings, &resource.attributes) {
        return None;
    }
    let attrs = &resource.attributes;
    let name = attr_string(attrs, "service.name").filter(|name| !name.is_empty())?;
    let namespace = attr_string(attrs, "service.namespace").filter(|ns| !ns.is_empty());
    let id = match &namespace {
        Some(namespace) => format!("svc:{namespace}/{name}"),
        None => format!("svc:{name}"),
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

/// Adds the service's node to the request's elements. A repeated service in
/// the same request is added again only when an authoritative resource adds
/// metadata the earlier entry lacked (a callee placeholder enriched by its own
/// resource), in the enriching record's group.
fn push_service(
    elements: &mut Vec<ProjectedElement>,
    service: &ServiceIdentity,
    resource: Option<&Resource>,
    observed: u64,
    group: u32,
) {
    let previous = elements
        .iter()
        .rev()
        .find(|element| element.id == service.id);
    if previous.is_some() && resource.is_none() {
        return;
    }
    let mut props = ElementPropertyMap::new();
    insert_string(&mut props, "name", &service.name);
    if let Some(namespace) = &service.namespace {
        insert_string(&mut props, "namespace", namespace);
    }
    if let Some(environment) = &service.environment {
        insert_string(&mut props, "environment", environment);
    }
    if let Some(instance) = &service.instance_id {
        insert_string(&mut props, "instanceId", instance);
    }
    if let Some(version) = resource.and_then(|r| attr_string(&r.attributes, "service.version")) {
        insert_string(&mut props, "version", &version);
    }
    insert_string(&mut props, "lastSeen", &millis_iso(observed));
    if let Some(previous) = previous {
        let metadata_changed = props
            .map_iter(|name, value| {
                name.as_ref() != "lastSeen" && previous.properties.get(name.as_ref()) != Some(value)
            })
            .any(|changed| changed);
        if !metadata_changed {
            return;
        }
    }
    elements.push(ProjectedElement {
        id: service.id.clone(),
        labels: vec!["Service".into()],
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
        labels: vec!["Heartbeat".into()],
        properties: props,
        kind: ProjectedKind::Node,
        effective_from: observed,
        category: ElementCategory::Heartbeat,
        ttl_secs: None,
        group,
    });
    elements.push(ProjectedElement {
        id: format!("rel-hb:{}", service.id),
        labels: vec!["HEARTBEAT".into()],
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

fn is_derived(settings: &Projection, attributes: &[KeyValue]) -> bool {
    settings.reject_derived
        && attr_string(attributes, SOURCE_ORIGIN_ATTRIBUTE).as_deref()
            == Some(SOURCE_ORIGIN_DERIVED)
}

fn destination_service(settings: &Projection, attrs: &[KeyValue]) -> Option<String> {
    settings
        .destination_attributes
        .iter()
        .find_map(|key| attr_string(attrs, key).filter(|value| !value.is_empty()))
}

/// `:key=value,...` for the configured identity attributes present on the
/// point, sorted, or empty when none is present.
fn metric_identity_suffix(settings: &Projection, attributes: &[KeyValue]) -> String {
    let mut parts: Vec<String> = settings
        .metric_identity_attributes
        .iter()
        .filter_map(|key| attr_string(attributes, key).map(|value| format!("{key}={value}")))
        .collect();
    if parts.is_empty() {
        return String::new();
    }
    parts.sort();
    format!(":{}", parts.join(","))
}

fn number_value(value: Option<&number_data_point::Value>) -> Option<f64> {
    match value? {
        number_data_point::Value::AsDouble(value) => Some(*value),
        number_data_point::Value::AsInt(value) => Some(*value as f64),
    }
}

fn attr_string(attrs: &[KeyValue], key: &str) -> Option<String> {
    attrs
        .iter()
        .find(|kv| kv.key == key)
        .and_then(|kv| any_value_string(kv.value.as_ref()))
}

/// Scalar attribute and body values as text; arrays, maps and bytes have none.
fn any_value_string(value: Option<&AnyValue>) -> Option<String> {
    match value?.value.as_ref()? {
        any_value::Value::StringValue(value) => Some(value.clone()),
        any_value::Value::BoolValue(value) => Some(value.to_string()),
        any_value::Value::IntValue(value) => Some(value.to_string()),
        any_value::Value::DoubleValue(value) => Some(value.to_string()),
        _ => None,
    }
}

fn insert_string(props: &mut ElementPropertyMap, key: &str, value: &str) {
    props.insert(key, ElementValue::String(Arc::from(value)));
}

/// The record's effective time in milliseconds: its OTLP timestamp, or the
/// receipt time when the timestamp is unset. A timestamp beyond the graph's
/// millisecond range cannot be an effective time and rejects the record.
fn effective_time(nanos: u64, received_at_millis: u64) -> Option<u64> {
    match nanos_to_millis(nanos) {
        0 => Some(received_at_millis),
        millis if millis <= MAX_REASONABLE_MILLIS_TIMESTAMP => Some(millis),
        _ => None,
    }
}

fn millis_iso(millis: u64) -> String {
    i64::try_from(millis)
        .ok()
        .and_then(chrono::DateTime::<chrono::Utc>::from_timestamp_millis)
        .map_or_else(|| millis.to_string(), |time| time.to_rfc3339())
}

/// A stable 64-bit FNV-1a hash, used to identify a log record without an event name.
fn hash_text(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn metric_point_count(rm: &ResourceMetrics) -> u64 {
    rm.scope_metrics
        .iter()
        .flat_map(|sm| sm.metrics.iter())
        .map(|metric| match metric.data.as_ref() {
            Some(metric::Data::Gauge(gauge)) => gauge.data_points.len(),
            Some(metric::Data::Sum(sum)) => sum.data_points.len(),
            _ => 1,
        })
        .sum::<usize>() as u64
}

fn span_count(rs: &ResourceSpans) -> u64 {
    rs.scope_spans
        .iter()
        .map(|ss| ss.spans.len())
        .sum::<usize>() as u64
}

fn log_count(rl: &ResourceLogs) -> u64 {
    rl.scope_logs
        .iter()
        .map(|sl| sl.log_records.len())
        .sum::<usize>() as u64
}

/// Whether any pattern matches. An empty pattern list matches nothing.
pub(crate) fn allowlist_matches(patterns: &[String], value: &str) -> bool {
    patterns.iter().any(|pattern| glob_match(pattern, value))
}

/// Matches `*` as any (possibly empty) sequence; every other character is literal.
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let Some((first, rest_parts)) = pattern.split_once('*') else {
        return pattern == text;
    };
    let Some(mut rest) = text.strip_prefix(first) else {
        return false;
    };
    let parts: Vec<&str> = rest_parts.split('*').collect();
    let (last, middle) = parts.split_last().unwrap_or((&"", &[]));
    for part in middle.iter().filter(|part| !part.is_empty()) {
        match rest.find(part) {
            Some(index) => rest = &rest[index + part.len()..],
            None => return false,
        }
    }
    rest.len() >= last.len() && rest.ends_with(last)
}

#[cfg(test)]
#[path = "projection_tests.rs"]
pub(crate) mod tests;
