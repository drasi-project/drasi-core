// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    MetricKind, MetricRegistration, MetricUnit, Result, TelemetryError, TelemetryScope,
    TraceContextV1,
};
use serde::{
    de::{DeserializeOwned, Error, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use std::{collections::BTreeSet, fmt, marker::PhantomData};

pub const MAX_TELEMETRY_RECORD_BYTES: usize = 64 * 1024;
pub const MAX_TELEMETRY_TEXT_BYTES: usize = 4096;
pub const MAX_TELEMETRY_ATTRIBUTES: usize = 64;
pub const MAX_TELEMETRY_EVENTS: usize = 32;
pub const MAX_TELEMETRY_LINKS: usize = 32;
pub const MAX_NATIVE_OPEN_SPANS: usize = 4096;
pub const MAX_NATIVE_BINDINGS: usize = 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct TelemetryLimits {
    pub records: usize,
    pub bytes: usize,
    pub record_bytes: usize,
    pub open_spans: usize,
    pub metric_handles: usize,
    pub bindings: usize,
}

impl Default for TelemetryLimits {
    fn default() -> Self {
        Self {
            records: 1024,
            bytes: 16 * 1024 * 1024,
            record_bytes: MAX_TELEMETRY_RECORD_BYTES,
            open_spans: 256,
            metric_handles: 256,
            bindings: 256,
        }
    }
}

impl TelemetryLimits {
    pub fn validate(self) -> Result<Self> {
        if [
            self.records,
            self.bytes,
            self.record_bytes,
            self.open_spans,
            self.metric_handles,
            self.bindings,
        ]
        .contains(&0)
            || self.bytes < self.record_bytes
            || self.record_bytes > MAX_TELEMETRY_RECORD_BYTES
            || self.open_spans > MAX_NATIVE_OPEN_SPANS
            || self.bindings > MAX_NATIVE_BINDINGS
            || self.metric_handles > 16_384
        {
            return Err(TelemetryError::Invalid(
                "invalid bounded telemetry limits".into(),
            ));
        }
        Ok(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u32)]
pub enum TelemetrySubmission {
    Accepted = 0,
    Full = 1,
    Invalid = 2,
    Closed = 3,
}

impl TryFrom<u32> for TelemetrySubmission {
    type Error = TelemetryError;

    fn try_from(value: u32) -> Result<Self> {
        match value {
            0 => Ok(Self::Accepted),
            1 => Ok(Self::Full),
            2 => Ok(Self::Invalid),
            3 => Ok(Self::Closed),
            _ => Err(TelemetryError::Invalid(
                "unknown telemetry submission outcome".into(),
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TelemetryAttribute {
    Bool(bool),
    I64(i64),
    F64(f64),
    String(#[serde(deserialize_with = "text")] String),
}

pub type TelemetryAttributes = Vec<(String, TelemetryAttribute)>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TelemetrySpanStatus {
    Unset,
    Ok,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetrySpanLink {
    pub context: TraceContextV1,
    #[serde(deserialize_with = "attributes")]
    pub attributes: TelemetryAttributes,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetrySpanEvent {
    #[serde(deserialize_with = "text")]
    pub name: String,
    pub time_unix_ns: u64,
    #[serde(deserialize_with = "attributes")]
    pub attributes: TelemetryAttributes,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletedSpan {
    #[serde(deserialize_with = "text")]
    pub name: String,
    #[serde(deserialize_with = "text")]
    pub target: String,
    pub context: TraceContextV1,
    pub parent_span_id: Option<[u8; 8]>,
    #[serde(deserialize_with = "links")]
    pub links: Vec<TelemetrySpanLink>,
    pub start_time_unix_ns: u64,
    pub end_time_unix_ns: u64,
    pub status: TelemetrySpanStatus,
    #[serde(deserialize_with = "attributes")]
    pub attributes: TelemetryAttributes,
    #[serde(deserialize_with = "events")]
    pub events: Vec<TelemetrySpanEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TelemetryLogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TelemetryLogRecord {
    pub time_unix_ns: u64,
    pub level: TelemetryLogLevel,
    #[serde(deserialize_with = "text")]
    pub target: String,
    #[serde(deserialize_with = "text")]
    pub message: String,
    #[serde(deserialize_with = "attributes")]
    pub attributes: TelemetryAttributes,
    pub context: Option<TraceContextV1>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootSpanRequest {
    pub trace_id: [u8; 16],
    #[serde(deserialize_with = "text")]
    pub name: String,
    #[serde(deserialize_with = "text")]
    pub target: String,
    #[serde(deserialize_with = "attributes")]
    pub attributes: TelemetryAttributes,
    #[serde(deserialize_with = "links")]
    pub links: Vec<TelemetrySpanLink>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootSpanDecision {
    pub sampled: bool,
    pub trace_state: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PluginRootSampling {
    Always,
    Never,
    Ratio(f64),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginTelemetryConfig {
    pub scope: TelemetryScope,
    pub generation: u64,
    #[serde(deserialize_with = "text")]
    pub plugin_kind: String,
    pub tracing: bool,
    pub metrics: bool,
    pub native_metrics_required: bool,
    #[serde(deserialize_with = "text")]
    pub trace_directives: String,
    #[serde(deserialize_with = "text")]
    pub log_directives: String,
    #[serde(deserialize_with = "exclusions")]
    pub metric_exclusions: Vec<String>,
    pub sampling: PluginRootSampling,
    pub limits: TelemetryLimits,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginTelemetryScope {
    pub config: PluginTelemetryConfig,
    pub context: Option<TraceContextV1>,
}

impl TelemetryRecord for PluginTelemetryScope {
    fn validate(&self) -> Result<()> {
        self.config.validate()
    }
}

impl PluginTelemetryConfig {
    pub fn allows_metric(&self, leaf: &str) -> bool {
        self.metrics
            && !self
                .metric_exclusions
                .iter()
                .any(|part| format!("drasi.plugin.{}.{}", self.plugin_kind, leaf).contains(part))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginMetricRegistration {
    #[serde(deserialize_with = "text")]
    pub leaf: String,
    pub kind: MetricKind,
    pub unit: MetricUnit,
    #[serde(deserialize_with = "text")]
    pub description: String,
    #[serde(deserialize_with = "labels")]
    pub labels: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct PluginMetricHandle(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum PluginMetricUpdate {
    CounterIncrement {
        handle: PluginMetricHandle,
        value: u64,
    },
    CounterAbsolute {
        handle: PluginMetricHandle,
        value: u64,
    },
    GaugeSet {
        handle: PluginMetricHandle,
        value: f64,
    },
    GaugeIncrement {
        handle: PluginMetricHandle,
        value: f64,
    },
    GaugeDecrement {
        handle: PluginMetricHandle,
        value: f64,
    },
    HistogramRecord {
        handle: PluginMetricHandle,
        value: f64,
    },
}

impl PluginMetricUpdate {
    pub fn handle(self) -> PluginMetricHandle {
        match self {
            Self::CounterIncrement { handle, .. }
            | Self::CounterAbsolute { handle, .. }
            | Self::GaugeSet { handle, .. }
            | Self::GaugeIncrement { handle, .. }
            | Self::GaugeDecrement { handle, .. }
            | Self::HistogramRecord { handle, .. } => handle,
        }
    }
}

pub trait TelemetryRecord: Serialize + DeserializeOwned {
    fn validate(&self) -> Result<()>;
}

pub fn encode_telemetry<T: TelemetryRecord>(value: &T, limit: usize) -> Result<Vec<u8>> {
    value.validate()?;
    crate::encode_bounded_messagepack(value, limit.min(MAX_TELEMETRY_RECORD_BYTES))
        .map_err(|error| TelemetryError::Invalid(error.to_string()))
}

pub fn decode_telemetry<T: TelemetryRecord>(bytes: &[u8], limit: usize) -> Result<T> {
    let value: T = crate::decode_bounded_messagepack(bytes, limit.min(MAX_TELEMETRY_RECORD_BYTES))
        .map_err(|error| TelemetryError::Invalid(error.to_string()))?;
    value.validate()?;
    Ok(value)
}

pub fn validate_telemetry_text(value: &str, limit: usize) -> Result<()> {
    if value.len() > limit || value.contains('\0') {
        return Err(TelemetryError::Invalid(
            "oversized or invalid telemetry text".into(),
        ));
    }
    Ok(())
}

pub fn validate_telemetry_attributes(values: &TelemetryAttributes) -> Result<()> {
    if values.len() > MAX_TELEMETRY_ATTRIBUTES {
        return Err(TelemetryError::Capacity("telemetry attributes"));
    }
    let mut names = BTreeSet::new();
    for (name, value) in values {
        validate_telemetry_text(name, 256)?;
        if name.is_empty()
            || !names.insert(name)
            || [
                "instance_id",
                "graph_id",
                "component_id",
                "component_kind",
                "plugin_kind",
                "generation",
                "role",
                "trace_id",
                "span_id",
            ]
            .contains(&name.as_str())
        {
            return Err(TelemetryError::Invalid(
                "duplicate or reserved telemetry attribute".into(),
            ));
        }
        match value {
            TelemetryAttribute::String(value) => {
                validate_telemetry_text(value, MAX_TELEMETRY_TEXT_BYTES)?
            }
            TelemetryAttribute::F64(value) if !value.is_finite() => {
                return Err(TelemetryError::Invalid("non-finite telemetry value".into()))
            }
            _ => {}
        }
    }
    Ok(())
}

fn validate_links(values: &[TelemetrySpanLink]) -> Result<()> {
    if values.len() > MAX_TELEMETRY_LINKS {
        return Err(TelemetryError::Capacity("span links"));
    }
    for value in values {
        validate_telemetry_attributes(&value.attributes)?;
    }
    Ok(())
}

impl TelemetryRecord for CompletedSpan {
    fn validate(&self) -> Result<()> {
        validate_telemetry_text(&self.name, 256)?;
        validate_telemetry_text(&self.target, 256)?;
        if self.name.is_empty()
            || self.end_time_unix_ns < self.start_time_unix_ns
            || self.parent_span_id == Some([0; 8])
        {
            return Err(TelemetryError::Invalid(
                "invalid span name, parent or timestamps".into(),
            ));
        }
        validate_telemetry_attributes(&self.attributes)?;
        validate_links(&self.links)?;
        if self.events.len() > MAX_TELEMETRY_EVENTS {
            return Err(TelemetryError::Capacity("span events"));
        }
        for event in &self.events {
            validate_telemetry_text(&event.name, 256)?;
            if event.time_unix_ns < self.start_time_unix_ns
                || event.time_unix_ns > self.end_time_unix_ns
            {
                return Err(TelemetryError::Invalid(
                    "span event time is outside the span".into(),
                ));
            }
            validate_telemetry_attributes(&event.attributes)?;
        }
        Ok(())
    }
}

impl TelemetryRecord for TelemetryLogRecord {
    fn validate(&self) -> Result<()> {
        validate_telemetry_text(&self.target, 256)?;
        validate_telemetry_text(&self.message, MAX_TELEMETRY_TEXT_BYTES)?;
        validate_telemetry_attributes(&self.attributes)
    }
}

impl TelemetryRecord for RootSpanRequest {
    fn validate(&self) -> Result<()> {
        if self.trace_id == [0; 16] || self.name.is_empty() {
            return Err(TelemetryError::Invalid("invalid root span identity".into()));
        }
        validate_telemetry_text(&self.name, 256)?;
        validate_telemetry_text(&self.target, 256)?;
        validate_telemetry_attributes(&self.attributes)?;
        validate_links(&self.links)
    }
}

impl TelemetryRecord for RootSpanDecision {
    fn validate(&self) -> Result<()> {
        TraceContextV1::new(
            [1; 16],
            [1; 8],
            u8::from(self.sampled),
            self.trace_state.as_deref().map(Into::into),
        )?;
        Ok(())
    }
}

impl TelemetryRecord for PluginTelemetryConfig {
    fn validate(&self) -> Result<()> {
        self.limits.validate()?;
        if self.scope.component_id().is_none() {
            return Err(TelemetryError::Invalid(
                "plugin telemetry requires component scope".into(),
            ));
        }
        MetricRegistration::plugin_name(&self.plugin_kind, "validation")?;
        validate_telemetry_text(&self.plugin_kind, 128)?;
        validate_telemetry_text(&self.trace_directives, MAX_TELEMETRY_TEXT_BYTES)?;
        validate_telemetry_text(&self.log_directives, MAX_TELEMETRY_TEXT_BYTES)?;
        if self.metric_exclusions.len() > 64 {
            return Err(TelemetryError::Capacity("metric exclusions"));
        }
        for value in &self.metric_exclusions {
            validate_telemetry_text(value, 256)?;
        }
        if let PluginRootSampling::Ratio(value) = self.sampling {
            super::SamplingRatio::new(value)?;
        }
        Ok(())
    }
}

impl TelemetryRecord for PluginMetricRegistration {
    fn validate(&self) -> Result<()> {
        MetricRegistration::plugin_name("validation", &self.leaf)?;
        validate_telemetry_text(&self.leaf, 256)?;
        validate_telemetry_text(&self.description, 1024)?;
        if self.labels.len() > 16 {
            return Err(TelemetryError::Capacity("metric labels"));
        }
        for (key, value) in &self.labels {
            validate_telemetry_text(key, 64)?;
            validate_telemetry_text(value, 1024)?;
        }
        Ok(())
    }
}

impl TelemetryRecord for PluginMetricUpdate {
    fn validate(&self) -> Result<()> {
        if self.handle().0 == 0 {
            return Err(TelemetryError::Invalid("zero metric handle".into()));
        }
        match self {
            Self::GaugeSet { value, .. }
            | Self::GaugeIncrement { value, .. }
            | Self::GaugeDecrement { value, .. }
            | Self::HistogramRecord { value, .. }
                if !value.is_finite() =>
            {
                Err(TelemetryError::Invalid("non-finite metric update".into()))
            }
            _ => Ok(()),
        }
    }
}

fn text<'de, D: Deserializer<'de>>(deserializer: D) -> std::result::Result<String, D::Error> {
    struct Text;
    impl Visitor<'_> for Text {
        type Value = String;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("bounded telemetry text")
        }
        fn visit_str<E: Error>(self, value: &str) -> std::result::Result<String, E> {
            validate_telemetry_text(value, MAX_TELEMETRY_TEXT_BYTES).map_err(E::custom)?;
            Ok(value.to_owned())
        }
    }
    deserializer.deserialize_str(Text)
}

fn sequence<'de, T: Deserialize<'de>, D: Deserializer<'de>, const N: usize>(
    deserializer: D,
) -> std::result::Result<Vec<T>, D::Error> {
    struct Sequence<T, const N: usize>(PhantomData<T>);
    impl<'de, T: Deserialize<'de>, const N: usize> Visitor<'de> for Sequence<T, N> {
        type Value = Vec<T>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "at most {N} telemetry entries")
        }
        fn visit_seq<A: SeqAccess<'de>>(
            self,
            mut sequence: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            if sequence.size_hint().is_some_and(|count| count > N) {
                return Err(A::Error::custom("telemetry collection limit"));
            }
            let mut values = Vec::with_capacity(sequence.size_hint().unwrap_or(0).min(N));
            while let Some(value) = sequence.next_element()? {
                if values.len() == N {
                    return Err(A::Error::custom("telemetry collection limit"));
                }
                values.push(value);
            }
            Ok(values)
        }
    }
    deserializer.deserialize_seq(Sequence::<T, N>(PhantomData))
}

fn attributes<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<TelemetryAttributes, D::Error> {
    sequence::<_, _, MAX_TELEMETRY_ATTRIBUTES>(deserializer)
}
fn links<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<TelemetrySpanLink>, D::Error> {
    sequence::<_, _, MAX_TELEMETRY_LINKS>(deserializer)
}
fn events<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<TelemetrySpanEvent>, D::Error> {
    sequence::<_, _, MAX_TELEMETRY_EVENTS>(deserializer)
}
fn labels<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<(String, String)>, D::Error> {
    sequence::<_, _, 16>(deserializer)
}
fn exclusions<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<String>, D::Error> {
    sequence::<_, _, 64>(deserializer)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span() -> CompletedSpan {
        CompletedSpan {
            name: "plugin.decode".into(),
            target: "test".into(),
            context: TraceContextV1::new([1; 16], [2; 8], 1, Some("vendor=value".into())).unwrap(),
            parent_span_id: Some([3; 8]),
            links: vec![],
            start_time_unix_ns: 10,
            end_time_unix_ns: 20,
            status: TelemetrySpanStatus::Ok,
            attributes: vec![("count".into(), TelemetryAttribute::I64(5))],
            events: vec![TelemetrySpanEvent {
                name: "decoded".into(),
                time_unix_ns: 15,
                attributes: vec![],
            }],
        }
    }

    #[test]
    fn completed_records_preserve_identity_timing_fields_and_limits() {
        let span = span();
        let bytes = encode_telemetry(&span, MAX_TELEMETRY_RECORD_BYTES).unwrap();
        assert_eq!(
            decode_telemetry::<CompletedSpan>(&bytes, bytes.len()).unwrap(),
            span
        );
        assert!(decode_telemetry::<CompletedSpan>(&bytes, bytes.len() - 1).is_err());
        assert!(encode_telemetry(&span, bytes.len() - 1).is_err());
        assert!(decode_telemetry::<CompletedSpan>(
            &bytes[..bytes.len() - 1],
            MAX_TELEMETRY_RECORD_BYTES
        )
        .is_err());
        assert!(decode_telemetry::<CompletedSpan>(
            &[0xdd, 0xff, 0xff, 0xff, 0xff],
            MAX_TELEMETRY_RECORD_BYTES
        )
        .is_err());
    }

    #[test]
    fn decoding_cannot_allocate_unbounded_collections_or_bypass_record_validation() {
        for fault in 0..6 {
            let mut value = span();
            match fault {
                0 => {
                    value.attributes = vec![
                        ("x".into(), TelemetryAttribute::Bool(true));
                        MAX_TELEMETRY_ATTRIBUTES + 1
                    ]
                }
                1 => value.name = "x".repeat(MAX_TELEMETRY_TEXT_BYTES + 1),
                2 => value.parent_span_id = Some([0; 8]),
                3 => value.events[0].time_unix_ns = 21,
                4 => {
                    value.attributes = vec![(
                        "instance_id".into(),
                        TelemetryAttribute::String("spoof".into()),
                    )]
                }
                _ => {
                    value.attributes =
                        vec![("value".into(), TelemetryAttribute::F64(f64::INFINITY))]
                }
            }
            let bytes = rmp_serde::to_vec_named(&value).unwrap();
            assert!(
                decode_telemetry::<CompletedSpan>(&bytes, MAX_TELEMETRY_RECORD_BYTES).is_err(),
                "fault {fault}"
            );
        }
    }

    #[test]
    fn collection_limits_and_submission_codes_are_not_silent_defaults() {
        let limits = TelemetryLimits::default();
        assert!(limits.validate().is_ok());
        for value in [
            TelemetryLimits {
                records: 0,
                ..limits
            },
            TelemetryLimits {
                bytes: limits.record_bytes - 1,
                ..limits
            },
            TelemetryLimits {
                record_bytes: MAX_TELEMETRY_RECORD_BYTES + 1,
                ..limits
            },
            TelemetryLimits {
                open_spans: MAX_NATIVE_OPEN_SPANS + 1,
                ..limits
            },
            TelemetryLimits {
                bindings: MAX_NATIVE_BINDINGS + 1,
                ..limits
            },
        ] {
            assert!(value.validate().is_err());
        }
        for code in 0..4 {
            assert_eq!(TelemetrySubmission::try_from(code).unwrap() as u32, code);
        }
        assert!(TelemetrySubmission::try_from(4).is_err());
    }
}
