// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Catalogue eligibility does not imply that production instrumentation exists.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObservationDetail {
    Basic,
    Debug,
    Persistence,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MetricUnit {
    /// Dimensionless counts, including occupancy and boolean readiness gauges.
    One,
    Seconds,
}

pub const GRAPH_LABELS: &[&str] = &["instance_id", "graph_id"];
pub const COMPONENT_LABELS: &[&str] =
    &["instance_id", "graph_id", "component_id", "component_kind"];
pub const RESERVED_LABELS: &[&str] = &[
    "instance_id",
    "graph_id",
    "component_id",
    "component_kind",
    "plugin_kind",
    "generation",
    "role",
    "trace_id",
    "span_id",
    "sequence",
    "event_sequence",
    "error",
    "error_message",
];
pub const COMPONENT_TRACE_FIELDS: &[&str] = &[
    "instance_id",
    "graph_id",
    "component_id",
    "component_kind",
    "role",
    "generation",
    "plugin_kind",
    "operation",
    "outcome",
    "error_kind",
];

#[derive(Debug)]
pub struct SpanDefinition {
    pub name: &'static str,
    pub detail: ObservationDetail,
    pub owner: &'static str,
    pub boundary: &'static str,
    pub fields: &'static [&'static str],
}

macro_rules! span {
    ($name:literal, $detail:ident, $owner:literal, $boundary:literal, [$($field:literal),*]) => {
        SpanDefinition {
            name: $name,
            detail: ObservationDetail::$detail,
            owner: $owner,
            boundary: $boundary,
            fields: &[$($field),*],
        }
    };
}

/// Shared identity/outcome fields are in `COMPONENT_TRACE_FIELDS`; `fields`
/// lists only additional fields. Durations use local monotonic clocks.
pub const SPANS: &[SpanDefinition] = &[
    span!("source.publish", Basic, "graph source publication", "One available-envelope publication attempt, excluding idle next() waiting", ["output_port", "publication_mode", "change_count"]),
    span!("component.process", Basic, "graph transformer execution or actual service work owner", "One transform, input-driven continuation or bounded service work unit, excluding idle waiting and subsequent forwarding; a continuation consumes no new input", ["input_port", "input_count", "output_count"]),
    span!("query.evaluate", Debug, "query evaluator", "Actual inner evaluation only, excluding result hooks and commit; never output-only replay", ["query_id", "change_count", "result_change_count"]),
    span!("query.commit", Debug, "query transaction owner", "One actual commit attempt, not downstream handling", ["query_id"]),
    span!("component.confirm_outputs", Debug, "graph delivery_completed invocation", "One confirmation hook after outgoing acceptance; an empty emission batch is allowed", ["output_count"]),
    span!("pipe.send", Debug, "physical pipe provider", "One acceptance attempt including capacity wait, not one per logical edge; partial or uncertain acceptance is not rollback", ["queue_id", "output_port", "acceptance"]),
    span!("sink.handle", Basic, "graph sink execution", "One input handler call to its declared accepted or handled boundary", ["input_port", "input_count", "change_count", "completion"]),
    span!("sink.handle_batch", Basic, "graph sink execution", "One actual batch handler call, excluding batch collection", ["input_port", "input_count", "change_count", "completion", "links_truncated"]),
    span!("pipe.acknowledge", Debug, "acknowledgement provider", "One supported acknowledgement completion; dropping a delivery is not acknowledgement", ["queue_id", "input_port", "handling_outcome"]),
    span!("query.bootstrap", Basic, "query bootstrap owner", "One aggregate bootstrap run, not a span per snapshot row", ["query_id", "source_count", "input_count", "change_count"]),
    span!("component.scheduled", Basic, "graph/query scheduled execution", "One actual due-work unit, not an idle wakeup; do not duplicate component.process", ["work_kind", "output_count"]),
    span!("bootstrap.snapshot", Basic, "bootstrap provider", "One bounded provider snapshot run, not one span per row", ["source_count", "input_count", "change_count"]),
    span!("reaction.process", Basic, "ordinary reaction worker", "Actual dequeued result handling, distinct from graph acceptance", ["input_count", "change_count", "completion"]),
    span!("reaction.process_batch", Basic, "ordinary reaction worker", "Actual batch handling, excluding collection", ["input_count", "change_count", "completion", "links_truncated"]),
    span!("component.start", Basic, "graph lifecycle execution", "Actual startup call including awaited initialization; success is not unconditional readiness", []),
    span!("component.stop", Basic, "graph lifecycle execution", "Actual stop call; interruption does not establish completed cleanup", []),
    span!("component.reconfigure", Basic, "graph lifecycle execution", "One reconfiguration call, without configuration contents", []),
    span!("control.apply", Basic, "graph/library control boundary", "One control request to its declared response boundary, not component readiness", ["target_count"]),
];

#[derive(Debug)]
pub struct MetricDefinition {
    pub name: &'static str,
    pub kind: MetricKind,
    pub unit: MetricUnit,
    pub detail: ObservationDetail,
    pub component: bool,
    pub labels: &'static [&'static str],
    pub owner: &'static str,
    pub semantics: &'static str,
}

macro_rules! metric {
    ($name:literal, $kind:ident, $unit:ident, $detail:ident, $component:literal, [$($label:literal),*], $owner:literal, $semantics:literal) => {
        MetricDefinition {
            name: $name,
            kind: MetricKind::$kind,
            unit: MetricUnit::$unit,
            detail: ObservationDetail::$detail,
            component: $component,
            labels: &[$($label),*],
            owner: $owner,
            semantics: $semantics,
        }
    };
}

/// Identity labels are appended by the host. Generic and specialized counters
/// are different views, not additive totals or globally deduplicated events.
pub const METRICS: &[MetricDefinition] = &[
    metric!("drasi.component.up", Gauge, One, Basic, true, [], "graph lifecycle/readiness owner", "One after successful start and required readiness; zero when stopped, failed, exhausted or quiesced, not while idle"),
    metric!("drasi.component.calls", Counter, One, Basic, true, ["operation"], "graph execution or actual service adapter", "One per invocation including failure/cancellation; wrapper and adapter must not double count"),
    metric!("drasi.component.inputs_processed", Counter, One, Basic, true, ["completion"], "actual processing owner", "Successful input envelopes at the declared accepted/handled/transformed boundary; not continuation invocations"),
    metric!("drasi.component.outputs_emitted", Counter, One, Basic, true, [], "output validation owner", "Validated output envelopes per attempt before fan-out, not subscriber deliveries"),
    metric!("drasi.component.operation.duration", Histogram, Seconds, Basic, true, ["operation", "outcome"], "operation owner", "Monotonic call duration, independent of trace sampling; source waiting is a distinct operation"),
    metric!("drasi.source.events_dispatched", Counter, One, Basic, true, ["source_id", "phase"], "source publication owner", "Envelope attempts accepted by all bound destinations, once before fan-out"),
    metric!("drasi.query.events_processed", Counter, One, Basic, true, ["query_id", "phase"], "query input owner", "Completed input envelopes; no increment for output-only replay or dedup skips"),
    metric!("drasi.query.results_emitted", Counter, One, Basic, true, ["query_id", "change_kind"], "query result publication owner", "New result-row changes published locally; replay is not a new computed result"),
    metric!("drasi.reaction.results_processed", Counter, One, Basic, true, ["reaction_id", "query_id"], "ordinary reaction processor", "Successfully handled non-Noop result-row changes, not envelopes, enqueue acceptance or checkpoint advances"),
    metric!("drasi.queue.depth", Gauge, One, Basic, false, ["queue_id"], "physical queue provider", "Current pending deliveries; one series per actual queue, absent when unsupported"),
    metric!("drasi.queue.depth_max", Gauge, One, Basic, false, ["queue_id"], "physical queue provider", "Peak pending deliveries for the binding; not duplicated for shared edges or multicast"),
    metric!("drasi.queue.blocked_enqueues", Counter, One, Basic, false, ["queue_id"], "physical queue provider", "Provider-observed blocking events, not reconstructed from coalescing inspection"),
    metric!("drasi.queue.drops", Counter, One, Basic, false, ["queue_id"], "physical queue provider", "Provider-observed discarded deliveries; lossy acceptance can still discard"),
    metric!("drasi.reaction.checkpoint_lag_events", Gauge, One, Persistence, true, ["reaction_id", "query_id"], "ordinary reaction checkpoint owner", "Known checkpoint lag only; no fabricated zero for opaque sinks"),
    metric!("drasi.errors", Counter, One, Basic, true, ["error_kind"], "failed operation owner", "One failed operation, not each reporting or propagation of its cause"),
    metric!("drasi.query.engine.duration", Histogram, Seconds, Basic, true, ["query_id", "phase"], "query evaluator", "Actual inner evaluation duration, excluding result hooks/commit and output-only replay"),
    metric!("drasi.query.commit.duration", Histogram, Seconds, Persistence, true, ["query_id", "phase", "outcome"], "query transaction owner", "Actual session commit attempt duration, including failure/cancellation, excluding evaluation and result hooks"),
    metric!("drasi.query.outbox.entries", Gauge, One, Persistence, true, ["query_id"], "query output owner", "Known retained output entry count, not storage-engine or disk statistics"),
    metric!("drasi.query.live_results", Gauge, One, Persistence, true, ["query_id"], "query output owner", "Known live result cardinality at publication/recovery boundaries"),
    metric!("drasi.reaction.dedup_skips", Counter, One, Persistence, true, ["reaction_id", "query_id"], "reaction dedup owner", "Actual skipped result envelopes, once at the deciding adapter or worker"),
    metric!("drasi.reaction.gaps_detected", Counter, One, Persistence, true, ["reaction_id", "query_id"], "reaction recovery owner", "Actual gap detections, not coalesced inspection reports"),
    metric!("drasi.reaction.recovery_triggers", Counter, One, Persistence, true, ["reaction_id", "query_id", "policy"], "reaction recovery owner", "Actual recovery-policy invocations, not reset completions"),
    metric!("drasi.pipeline.end_to_end.duration", Histogram, Seconds, Deferred, true, ["query_id", "phase"], "future handled-branch timing owner", "Reserved and never emitted until validated source-to-handled-sink timing exists"),
];

pub fn metric_definition(name: &str) -> Option<&'static MetricDefinition> {
    METRICS.iter().find(|definition| definition.name == name)
}

pub fn valid_category(key: &str, value: &str) -> bool {
    match key {
        "phase" => matches!(value, "bootstrap" | "steady"),
        "change_kind" => matches!(value, "add" | "update" | "delete"),
        "outcome" => matches!(value, "success" | "error" | "cancelled"),
        "completion" => matches!(value, "accepted" | "handled" | "transformed"),
        "operation" => matches!(
            value,
            "next"
                | "publish"
                | "transform"
                | "continue_transform"
                | "handle"
                | "handle_batch"
                | "scheduled"
                | "start"
                | "stop"
                | "reconfigure"
                | "run"
                | "confirm_outputs"
                | "acknowledge"
                | "reaction"
                | "reaction_batch"
                | "service_work"
        ),
        "policy" => matches!(value, "strict" | "auto_reset" | "auto_skip_gap"),
        "error_kind" => matches!(
            value,
            "configuration"
                | "dependency"
                | "processing"
                | "storage"
                | "transport"
                | "timeout"
                | "cancelled"
                | "other"
        ),
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn catalogues_have_unique_safe_definitions() {
        let mut names = BTreeSet::new();
        for span in SPANS {
            assert!(names.insert(span.name));
            assert!(!span.name.starts_with("drasi."));
            assert!(span.name.contains('.'));
            assert!(span
                .name
                .bytes()
                .all(|ch| ch.is_ascii_lowercase() || ch == b'.' || ch == b'_'));
            assert!(!span.owner.is_empty() && !span.boundary.is_empty());
        }
        names.clear();
        for metric in METRICS {
            assert!(names.insert(metric.name));
            assert!(metric.name.starts_with("drasi."));
            assert!(!metric.owner.is_empty() && !metric.semantics.is_empty());
            assert_eq!(
                metric.kind == MetricKind::Histogram,
                metric.unit == MetricUnit::Seconds
            );
            let mut labels = BTreeSet::new();
            for key in metric.labels {
                assert!(labels.insert(key));
                assert!(!RESERVED_LABELS.contains(key));
            }
        }
    }
}
