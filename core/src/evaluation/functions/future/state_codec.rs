// Copyright 2026 The Drasi Authors.
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

//! Stores the per-row state of `drasi.previousValue` and
//! `drasi.previousDistinctValue` as `ElementValue`s.
//!
//! Values that `TryInto<ElementValue>` accepts keep that stored form. Other
//! values are stored as objects tagged with `TYPE_KEY`. A user map that
//! contains `TYPE_KEY` is wrapped in an `Object` envelope, so every tagged
//! object comes from this codec.
//!
//! The state is `{ "v": 1, "0": <encoded>, "1": <encoded> }`. State without
//! `"v"` was written by earlier releases and is migrated on read.

use std::collections::BTreeMap;
use std::iter;
use std::sync::Arc;

use chrono::{Datelike, FixedOffset, NaiveDate, NaiveTime, TimeDelta, Timelike};

use crate::evaluation::functions::aggregation::ValueAccumulator;
use crate::evaluation::variable_value::duration::Duration;
use crate::evaluation::variable_value::zoned_time::ZonedTime;
use crate::evaluation::variable_value::{ListRange, RangeBound, VariableValue};
use crate::evaluation::FunctionEvaluationError;
use crate::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue};

const TYPE_KEY: &str = "__drasi_state_type__";
const VERSION_KEY: &str = "v";
const VERSION: i64 = 1;
const SLOT_KEYS: [&str; 2] = ["0", "1"];
const NANOS_PER_SECOND: i64 = 1_000_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StateCodecError {
    /// The value contains an Expression, which holds query syntax, not data.
    Unsupported,
    /// The stored state is not valid v1 state.
    Corrupt,
}

impl From<StateCodecError> for FunctionEvaluationError {
    fn from(error: StateCodecError) -> Self {
        match error {
            StateCodecError::Unsupported => FunctionEvaluationError::InvalidType {
                expected: "ElementValue".to_string(),
            },
            StateCodecError::Corrupt => FunctionEvaluationError::CorruptData,
        }
    }
}

type Result<T> = std::result::Result<T, StateCodecError>;

pub(super) fn encode(value: &VariableValue) -> Result<ElementValue> {
    match value {
        VariableValue::Null
        | VariableValue::Bool(_)
        | VariableValue::Float(_)
        | VariableValue::Integer(_)
        | VariableValue::String(_)
        | VariableValue::LocalDateTime(_)
        | VariableValue::ZonedDateTime(_) => {
            value.try_into().map_err(|_| StateCodecError::Unsupported)
        }
        VariableValue::List(items) => Ok(ElementValue::List(
            items.iter().map(encode).collect::<Result<_>>()?,
        )),
        VariableValue::Object(map) => {
            let mut encoded = ElementPropertyMap::new();
            for (key, item) in map {
                encoded.insert(key, encode(item)?);
            }
            if map.contains_key(TYPE_KEY) {
                Ok(envelope(
                    "Object",
                    [("value", ElementValue::Object(encoded))],
                ))
            } else {
                Ok(ElementValue::Object(encoded))
            }
        }
        VariableValue::Date(date) => Ok(envelope(
            "Date",
            [("days", integer(date.num_days_from_ce()))],
        )),
        VariableValue::LocalTime(time) => Ok(envelope(
            "LocalTime",
            [
                ("secs", integer(time.num_seconds_from_midnight())),
                ("nanos", integer(time.nanosecond())),
            ],
        )),
        VariableValue::ZonedTime(zoned) => Ok(envelope(
            "ZonedTime",
            [
                ("secs", integer(zoned.time().num_seconds_from_midnight())),
                ("nanos", integer(zoned.time().nanosecond())),
                ("offset", integer(zoned.offset().local_minus_utc())),
            ],
        )),
        VariableValue::Duration(duration) => Ok(envelope(
            "Duration",
            [
                ("years", integer(*duration.year())),
                ("months", integer(*duration.month())),
                ("secs", integer(duration.duration().num_seconds())),
                ("nanos", integer(duration.duration().subsec_nanos())),
            ],
        )),
        VariableValue::ElementReference(reference) => {
            Ok(envelope("ElementReference", reference_fields(reference)))
        }
        VariableValue::ElementMetadata(metadata) => {
            Ok(envelope("ElementMetadata", metadata_fields(metadata)))
        }
        VariableValue::Element(element) => match element.as_ref() {
            Element::Node {
                metadata,
                properties,
            } => Ok(envelope(
                "Node",
                [
                    ("metadata", object(metadata_fields(metadata))),
                    ("properties", ElementValue::Object(properties.clone())),
                ],
            )),
            Element::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => Ok(envelope(
                "Relation",
                [
                    ("metadata", object(metadata_fields(metadata))),
                    ("in_node", object(reference_fields(in_node))),
                    ("out_node", object(reference_fields(out_node))),
                    ("properties", ElementValue::Object(properties.clone())),
                ],
            )),
        },
        VariableValue::Awaiting => Ok(envelope("Awaiting", [])),
        VariableValue::ListRange(range) => Ok(envelope(
            "ListRange",
            [
                ("start", range_bound(&range.start)),
                ("end", range_bound(&range.end)),
            ],
        )),
        VariableValue::Expression(_) => Err(StateCodecError::Unsupported),
    }
}

pub(super) fn decode(value: &ElementValue) -> Result<VariableValue> {
    match value {
        ElementValue::List(items) => Ok(VariableValue::List(
            items.iter().map(decode).collect::<Result<_>>()?,
        )),
        ElementValue::Object(map) => match map.get(TYPE_KEY) {
            Some(type_name) => decode_envelope(type_name, map),
            None => decode_map(map),
        },
        ElementValue::Null
        | ElementValue::Bool(_)
        | ElementValue::Float(_)
        | ElementValue::Integer(_)
        | ElementValue::String(_)
        | ElementValue::LocalDateTime(_)
        | ElementValue::ZonedDateTime(_) => Ok(VariableValue::from(value)),
    }
}

/// Returns the encoded values of slots `"0"` and `"1"`.
///
/// For v1 state, this function only checks that both slots exist. Each caller
/// decodes the slot that it returns. A slot that a call overwrites without
/// returning is not checked.
pub(super) fn read_state(state: &ElementPropertyMap) -> Result<[ElementValue; 2]> {
    match state.get(VERSION_KEY) {
        None => {
            let [first, second] = SLOT_KEYS.map(|slot| match state.get(slot) {
                Some(stored) => encode(&VariableValue::from(stored)),
                None => Ok(ElementValue::Null),
            });
            Ok([first?, second?])
        }
        Some(ElementValue::Integer(VERSION)) => {
            let [first, second] =
                SLOT_KEYS.map(|slot| state.get(slot).cloned().ok_or(StateCodecError::Corrupt));
            Ok([first?, second?])
        }
        Some(_) => Err(StateCodecError::Corrupt),
    }
}

pub(super) fn write_state(first: ElementValue, second: ElementValue) -> ValueAccumulator {
    let mut state = ElementPropertyMap::new();
    state.insert(VERSION_KEY, ElementValue::Integer(VERSION));
    state.insert(SLOT_KEYS[0], first);
    state.insert(SLOT_KEYS[1], second);
    ValueAccumulator::Map(state)
}

fn integer(value: impl Into<i64>) -> ElementValue {
    ElementValue::Integer(value.into())
}

fn range_bound(bound: &RangeBound) -> ElementValue {
    match bound {
        RangeBound::Index(index) => ElementValue::Integer(*index),
        RangeBound::Unbounded => ElementValue::Null,
    }
}

fn object(fields: impl IntoIterator<Item = (&'static str, ElementValue)>) -> ElementValue {
    let mut map = ElementPropertyMap::new();
    for (key, value) in fields {
        map.insert(key, value);
    }
    ElementValue::Object(map)
}

fn envelope(
    type_name: &str,
    fields: impl IntoIterator<Item = (&'static str, ElementValue)>,
) -> ElementValue {
    let tag = (TYPE_KEY, ElementValue::String(Arc::from(type_name)));
    object(iter::once(tag).chain(fields))
}

fn reference_fields(reference: &ElementReference) -> [(&'static str, ElementValue); 2] {
    [
        (
            "source_id",
            ElementValue::String(reference.source_id.clone()),
        ),
        (
            "element_id",
            ElementValue::String(reference.element_id.clone()),
        ),
    ]
}

fn metadata_fields(metadata: &ElementMetadata) -> [(&'static str, ElementValue); 3] {
    [
        ("reference", object(reference_fields(&metadata.reference))),
        (
            "labels",
            ElementValue::List(
                metadata
                    .labels
                    .iter()
                    .map(|label| ElementValue::String(label.clone()))
                    .collect(),
            ),
        ),
        // ElementValue::Integer is an i64 and cannot hold every u64.
        (
            "effective_from",
            ElementValue::String(Arc::from(metadata.effective_from.to_string())),
        ),
    ]
}

fn decode_map(map: &ElementPropertyMap) -> Result<VariableValue> {
    map.map_iter(|key, value| decode(value).map(|decoded| (key.to_string(), decoded)))
        .collect::<Result<BTreeMap<_, _>>>()
        .map(VariableValue::Object)
}

fn decode_envelope(type_name: &ElementValue, map: &ElementPropertyMap) -> Result<VariableValue> {
    let ElementValue::String(type_name) = type_name else {
        return Err(StateCodecError::Corrupt);
    };
    match type_name.as_ref() {
        "Object" => decode_map(object_field(map, "value")?),
        "Date" => NaiveDate::from_num_days_from_ce_opt(int_field(map, "days")?)
            .map(VariableValue::Date)
            .ok_or(StateCodecError::Corrupt),
        "LocalTime" => decode_time(map).map(VariableValue::LocalTime),
        "ZonedTime" => {
            let time = decode_time(map)?;
            let offset =
                FixedOffset::east_opt(int_field(map, "offset")?).ok_or(StateCodecError::Corrupt)?;
            Ok(VariableValue::ZonedTime(ZonedTime::new(time, offset)))
        }
        "Duration" => decode_duration(map).map(VariableValue::Duration),
        "ElementReference" => decode_reference(map).map(VariableValue::ElementReference),
        "ElementMetadata" => decode_metadata(map).map(VariableValue::ElementMetadata),
        "Node" => Ok(VariableValue::Element(Arc::new(Element::Node {
            metadata: decode_metadata(object_field(map, "metadata")?)?,
            properties: object_field(map, "properties")?.clone(),
        }))),
        "Relation" => Ok(VariableValue::Element(Arc::new(Element::Relation {
            metadata: decode_metadata(object_field(map, "metadata")?)?,
            in_node: decode_reference(object_field(map, "in_node")?)?,
            out_node: decode_reference(object_field(map, "out_node")?)?,
            properties: object_field(map, "properties")?.clone(),
        }))),
        "Awaiting" => Ok(VariableValue::Awaiting),
        "ListRange" => Ok(VariableValue::ListRange(ListRange {
            start: decode_range_bound(map, "start")?,
            end: decode_range_bound(map, "end")?,
        })),
        _ => Err(StateCodecError::Corrupt),
    }
}

fn decode_time(map: &ElementPropertyMap) -> Result<NaiveTime> {
    NaiveTime::from_num_seconds_from_midnight_opt(int_field(map, "secs")?, int_field(map, "nanos")?)
        .ok_or(StateCodecError::Corrupt)
}

fn decode_duration(map: &ElementPropertyMap) -> Result<Duration> {
    let secs: i64 = int_field(map, "secs")?;
    let nanos: i64 = int_field(map, "nanos")?;
    // TimeDelta::num_seconds and subsec_nanos both round toward zero, so the
    // parts never have opposite signs.
    if nanos <= -NANOS_PER_SECOND
        || nanos >= NANOS_PER_SECOND
        || (secs < 0 && nanos > 0)
        || (secs > 0 && nanos < 0)
    {
        return Err(StateCodecError::Corrupt);
    }
    let duration = TimeDelta::try_seconds(secs)
        .and_then(|duration| duration.checked_add(&TimeDelta::nanoseconds(nanos)))
        .ok_or(StateCodecError::Corrupt)?;
    Ok(Duration::new(
        duration,
        int_field(map, "years")?,
        int_field(map, "months")?,
    ))
}

fn decode_reference(map: &ElementPropertyMap) -> Result<ElementReference> {
    Ok(ElementReference {
        source_id: str_field(map, "source_id")?.clone(),
        element_id: str_field(map, "element_id")?.clone(),
    })
}

fn decode_metadata(map: &ElementPropertyMap) -> Result<ElementMetadata> {
    let labels = list_field(map, "labels")?
        .iter()
        .map(|label| match label {
            ElementValue::String(label) => Ok(label.clone()),
            _ => Err(StateCodecError::Corrupt),
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ElementMetadata {
        reference: decode_reference(object_field(map, "reference")?)?,
        labels: labels.into(),
        effective_from: str_field(map, "effective_from")?
            .parse()
            .map_err(|_| StateCodecError::Corrupt)?,
    })
}

fn decode_range_bound(map: &ElementPropertyMap, key: &str) -> Result<RangeBound> {
    match field(map, key)? {
        ElementValue::Integer(index) => Ok(RangeBound::Index(*index)),
        ElementValue::Null => Ok(RangeBound::Unbounded),
        _ => Err(StateCodecError::Corrupt),
    }
}

fn field<'a>(map: &'a ElementPropertyMap, key: &str) -> Result<&'a ElementValue> {
    map.get(key).ok_or(StateCodecError::Corrupt)
}

fn int_field<T: TryFrom<i64>>(map: &ElementPropertyMap, key: &str) -> Result<T> {
    match field(map, key)? {
        ElementValue::Integer(value) => T::try_from(*value).map_err(|_| StateCodecError::Corrupt),
        _ => Err(StateCodecError::Corrupt),
    }
}

fn str_field<'a>(map: &'a ElementPropertyMap, key: &str) -> Result<&'a Arc<str>> {
    match field(map, key)? {
        ElementValue::String(value) => Ok(value),
        _ => Err(StateCodecError::Corrupt),
    }
}

fn list_field<'a>(map: &'a ElementPropertyMap, key: &str) -> Result<&'a [ElementValue]> {
    match field(map, key)? {
        ElementValue::List(value) => Ok(value),
        _ => Err(StateCodecError::Corrupt),
    }
}

fn object_field<'a>(map: &'a ElementPropertyMap, key: &str) -> Result<&'a ElementPropertyMap> {
    match field(map, key)? {
        ElementValue::Object(value) => Ok(value),
        _ => Err(StateCodecError::Corrupt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use chrono::DateTime;
    use drasi_query_ast::ast;
    use serde_json::json;

    use crate::evaluation::variable_value::float::Float;
    use crate::evaluation::variable_value::integer::Integer;
    use crate::evaluation::variable_value::zoned_datetime::ZonedDateTime;

    fn stored(value: serde_json::Value) -> ElementValue {
        ElementValue::from(&value)
    }

    fn state(entries: &[(&str, ElementValue)]) -> ElementPropertyMap {
        let mut map = ElementPropertyMap::new();
        for (key, value) in entries {
            map.insert(key, value.clone());
        }
        map
    }

    fn date(y: i32, m: u32, d: u32) -> VariableValue {
        VariableValue::Date(NaiveDate::from_ymd_opt(y, m, d).unwrap())
    }

    fn time(h: u32, m: u32, s: u32, nanos: u32) -> NaiveTime {
        NaiveTime::from_hms_nano_opt(h, m, s, nanos).unwrap()
    }

    fn zoned_time(time: NaiveTime, offset_secs: i32) -> VariableValue {
        VariableValue::ZonedTime(ZonedTime::new(
            time,
            FixedOffset::east_opt(offset_secs).unwrap(),
        ))
    }

    fn duration(delta: TimeDelta, years: i64, months: i64) -> VariableValue {
        VariableValue::Duration(Duration::new(delta, years, months))
    }

    fn variable_object(entries: Vec<(&str, VariableValue)>) -> VariableValue {
        VariableValue::Object(
            entries
                .into_iter()
                .map(|(key, value)| (key.to_string(), value))
                .collect(),
        )
    }

    fn metadata(effective_from: u64) -> ElementMetadata {
        ElementMetadata {
            reference: ElementReference::new("src", "e1"),
            labels: Arc::new([Arc::from("A"), Arc::from("B")]),
            effective_from,
        }
    }

    fn properties_with_type_key() -> ElementPropertyMap {
        state(&[
            (TYPE_KEY, stored(json!("Date"))),
            ("days", stored(json!(1))),
            ("name", stored(json!("x"))),
        ])
    }

    fn node(properties: ElementPropertyMap) -> VariableValue {
        VariableValue::Element(Arc::new(Element::Node {
            metadata: metadata(1_700_000_000_000),
            properties,
        }))
    }

    fn relation(properties: ElementPropertyMap) -> VariableValue {
        VariableValue::Element(Arc::new(Element::Relation {
            metadata: metadata(1_700_000_000_000),
            in_node: ElementReference::new("src", "n1"),
            out_node: ElementReference::new("src", "n2"),
            properties,
        }))
    }

    fn list_range(start: RangeBound, end: RangeBound) -> VariableValue {
        VariableValue::ListRange(ListRange { start, end })
    }

    fn expression() -> VariableValue {
        VariableValue::Expression(ast::Expression::UnaryExpression(
            ast::UnaryExpression::Literal(ast::Literal::Null),
        ))
    }

    fn assert_round_trip(name: &str, value: &VariableValue) {
        let encoded = encode(value).unwrap();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(&decoded, value, "{name}");
        assert_eq!(encode(&decoded).unwrap(), encoded, "{name}: not canonical");
    }

    #[test]
    fn new_types_round_trip() {
        let cases = vec![
            ("min date", VariableValue::Date(NaiveDate::MIN)),
            ("max date", VariableValue::Date(NaiveDate::MAX)),
            ("date", date(2024, 1, 1)),
            (
                "leap second",
                VariableValue::LocalTime(time(23, 59, 59, 1_500_000_000)),
            ),
            (
                "local time nanos",
                VariableValue::LocalTime(time(12, 34, 56, 123_456_789)),
            ),
            ("max east offset", zoned_time(time(1, 2, 3, 4), 86_399)),
            ("max west offset", zoned_time(time(1, 2, 3, 4), -86_399)),
            (
                "zoned leap second",
                zoned_time(time(23, 59, 59, 1_999_999_999), 3_600),
            ),
            ("zero duration", duration(TimeDelta::zero(), 0, 0)),
            ("+1 ns", duration(TimeDelta::nanoseconds(1), 0, 0)),
            ("-1 ns", duration(TimeDelta::nanoseconds(-1), 0, 0)),
            ("+1.5 s", duration(TimeDelta::milliseconds(1_500), 0, 0)),
            ("-1.5 s", duration(TimeDelta::milliseconds(-1_500), 0, 0)),
            ("min duration", duration(TimeDelta::MIN, 0, 0)),
            (
                "min duration + 1 ns",
                duration(TimeDelta::MIN + TimeDelta::nanoseconds(1), 0, 0),
            ),
            ("max duration", duration(TimeDelta::MAX, 0, 0)),
            (
                "negative years and months",
                duration(TimeDelta::seconds(5), -3, -7),
            ),
            (
                "element reference",
                VariableValue::ElementReference(ElementReference::new("src", "e1")),
            ),
            (
                "metadata with max effective_from",
                VariableValue::ElementMetadata(metadata(u64::MAX)),
            ),
            ("node", node(properties_with_type_key())),
            ("relation", relation(properties_with_type_key())),
            ("awaiting", VariableValue::Awaiting),
            (
                "bounded range",
                list_range(RangeBound::Index(1), RangeBound::Index(3)),
            ),
            (
                "range without end",
                list_range(RangeBound::Index(-2), RangeBound::Unbounded),
            ),
            (
                "range without start",
                list_range(RangeBound::Unbounded, RangeBound::Index(2)),
            ),
            (
                "unbounded range",
                list_range(RangeBound::Unbounded, RangeBound::Unbounded),
            ),
            (
                "range with i64 limits",
                list_range(RangeBound::Index(i64::MIN), RangeBound::Index(i64::MAX)),
            ),
        ];
        for (name, value) in &cases {
            assert_round_trip(name, value);
        }
    }

    #[test]
    fn duration_parts_round_toward_zero() {
        let encoded = encode(&duration(TimeDelta::milliseconds(-1_500), 0, 0)).unwrap();
        let ElementValue::Object(map) = encoded else {
            panic!("expected an object");
        };
        assert_eq!(map.get("secs"), Some(&ElementValue::Integer(-1)));
        assert_eq!(map.get("nanos"), Some(&ElementValue::Integer(-500_000_000)));
    }

    #[test]
    fn envelope_formats_are_pinned() {
        let element_metadata = json!({
            "reference": {"source_id": "src", "element_id": "e1"},
            "labels": ["A", "B"],
            "effective_from": "1700000000000",
        });
        let cases = vec![
            (
                "Date",
                date(2024, 1, 1),
                json!({TYPE_KEY: "Date", "days": 738_886}),
            ),
            (
                "LocalTime",
                VariableValue::LocalTime(time(12, 34, 56, 123_456_789)),
                json!({TYPE_KEY: "LocalTime", "secs": 45_296, "nanos": 123_456_789}),
            ),
            (
                "ZonedTime",
                zoned_time(time(12, 34, 56, 0), -3_600),
                json!({TYPE_KEY: "ZonedTime", "secs": 45_296, "nanos": 0, "offset": -3_600}),
            ),
            (
                "Duration",
                duration(TimeDelta::milliseconds(-1_500), -1, 2),
                json!({
                    TYPE_KEY: "Duration",
                    "years": -1,
                    "months": 2,
                    "secs": -1,
                    "nanos": -500_000_000,
                }),
            ),
            (
                "ElementReference",
                VariableValue::ElementReference(ElementReference::new("src", "e1")),
                json!({TYPE_KEY: "ElementReference", "source_id": "src", "element_id": "e1"}),
            ),
            (
                "ElementMetadata",
                VariableValue::ElementMetadata(metadata(1_700_000_000_000)),
                json!({
                    TYPE_KEY: "ElementMetadata",
                    "reference": {"source_id": "src", "element_id": "e1"},
                    "labels": ["A", "B"],
                    "effective_from": "1700000000000",
                }),
            ),
            (
                "Node",
                node(state(&[("name", stored(json!("x")))])),
                json!({
                    TYPE_KEY: "Node",
                    "metadata": element_metadata,
                    "properties": {"name": "x"},
                }),
            ),
            (
                "Relation",
                relation(state(&[("name", stored(json!("x")))])),
                json!({
                    TYPE_KEY: "Relation",
                    "metadata": element_metadata,
                    "in_node": {"source_id": "src", "element_id": "n1"},
                    "out_node": {"source_id": "src", "element_id": "n2"},
                    "properties": {"name": "x"},
                }),
            ),
            (
                "Awaiting",
                VariableValue::Awaiting,
                json!({TYPE_KEY: "Awaiting"}),
            ),
            (
                "ListRange",
                list_range(RangeBound::Index(1), RangeBound::Unbounded),
                json!({TYPE_KEY: "ListRange", "start": 1, "end": null}),
            ),
            (
                "Object",
                variable_object(vec![
                    (TYPE_KEY, VariableValue::String("Date".to_string())),
                    ("days", VariableValue::Integer(Integer::from(1))),
                ]),
                json!({TYPE_KEY: "Object", "value": {TYPE_KEY: "Date", "days": 1}}),
            ),
        ];
        for (name, value, expected) in cases {
            assert_eq!(encode(&value).unwrap(), stored(expected), "{name}");
        }
    }

    #[test]
    fn legacy_supported_values_keep_their_stored_form() {
        let local = NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_nano_opt(12, 0, 0, 123_456_789)
            .unwrap();
        let zoned = DateTime::parse_from_rfc3339("2024-01-01T12:00:00+01:00").unwrap();
        let cases = vec![
            ("null", VariableValue::Null, ElementValue::Null),
            ("bool", VariableValue::Bool(true), ElementValue::Bool(true)),
            (
                "integer",
                VariableValue::Integer(Integer::from(-42)),
                ElementValue::Integer(-42),
            ),
            (
                "integer above i64::MAX",
                VariableValue::Integer(Integer::from(u64::MAX)),
                ElementValue::Integer(0),
            ),
            (
                "float",
                VariableValue::Float(Float::from(1.5)),
                ElementValue::Float(1.5.into()),
            ),
            (
                "NaN",
                VariableValue::Float(Float::from(f64::NAN)),
                ElementValue::Float(0.0.into()),
            ),
            (
                "infinity",
                VariableValue::Float(Float::from(f64::INFINITY)),
                ElementValue::Float(0.0.into()),
            ),
            (
                "negative infinity",
                VariableValue::Float(Float::from(f64::NEG_INFINITY)),
                ElementValue::Float(0.0.into()),
            ),
            (
                "string",
                VariableValue::String("hello".to_string()),
                stored(json!("hello")),
            ),
            (
                "local datetime",
                VariableValue::LocalDateTime(local),
                ElementValue::LocalDateTime(local),
            ),
            (
                "zoned datetime with zone name",
                VariableValue::ZonedDateTime(ZonedDateTime::new(
                    zoned,
                    Some("Europe/Berlin".to_string()),
                )),
                ElementValue::ZonedDateTime(zoned),
            ),
        ];
        for (name, value, expected) in cases {
            let legacy: ElementValue = (&value).try_into().unwrap();
            let encoded = encode(&value).unwrap();
            assert_eq!(encoded, legacy, "{name}");
            assert_eq!(encoded, expected, "{name}");
        }
    }

    #[test]
    fn nested_values_round_trip() {
        let cases = vec![
            (
                "list of dates",
                VariableValue::List(vec![date(2024, 1, 1), date(2024, 1, 2)]),
            ),
            (
                "list of lists of dates",
                VariableValue::List(vec![VariableValue::List(vec![date(2024, 1, 1)])]),
            ),
            (
                "map with a date",
                variable_object(vec![("d", date(2024, 1, 1))]),
            ),
            (
                "map in a list in a map",
                variable_object(vec![(
                    "outer",
                    VariableValue::List(vec![variable_object(vec![
                        ("d", date(2024, 1, 1)),
                        ("s", VariableValue::String("x".to_string())),
                    ])]),
                )]),
            ),
            (
                "list of ranges",
                VariableValue::List(vec![
                    list_range(RangeBound::Index(1), RangeBound::Index(3)),
                    list_range(RangeBound::Unbounded, RangeBound::Index(2)),
                ]),
            ),
            (
                "map with a range",
                variable_object(vec![(
                    "r",
                    list_range(RangeBound::Index(1), RangeBound::Unbounded),
                )]),
            ),
        ];
        for (name, value) in &cases {
            assert_round_trip(name, value);
        }
    }

    #[test]
    fn user_maps_with_the_type_key_are_escaped() {
        let envelope_shaped = variable_object(vec![
            (TYPE_KEY, VariableValue::String("Date".to_string())),
            ("days", VariableValue::Integer(Integer::from(1))),
        ]);
        let cases = vec![
            ("top level", envelope_shaped.clone()),
            (
                "in a list",
                VariableValue::List(vec![envelope_shaped.clone()]),
            ),
            (
                "in a map",
                variable_object(vec![("m", envelope_shaped.clone())]),
            ),
            (
                "as the type key value",
                variable_object(vec![(TYPE_KEY, envelope_shaped.clone())]),
            ),
            (
                "holding a date",
                variable_object(vec![(TYPE_KEY, date(2024, 1, 1))]),
            ),
        ];
        for (name, value) in &cases {
            assert_round_trip(name, value);
        }
    }

    #[test]
    fn expressions_are_unsupported() {
        let cases = vec![
            ("expression", expression()),
            (
                "expression in a list",
                VariableValue::List(vec![expression()]),
            ),
            (
                "expression in a map",
                variable_object(vec![("e", expression())]),
            ),
        ];
        for (name, value) in &cases {
            assert_eq!(encode(value), Err(StateCodecError::Unsupported), "{name}");
        }
    }

    #[test]
    fn invalid_envelopes_are_corrupt() {
        let max = TimeDelta::MAX;
        let cases = vec![
            ("unknown type", json!({TYPE_KEY: "Bogus"})),
            ("non-string type", json!({TYPE_KEY: 1})),
            ("missing field", json!({TYPE_KEY: "Date"})),
            ("wrong field type", json!({TYPE_KEY: "Date", "days": "1"})),
            (
                "days above i32",
                json!({TYPE_KEY: "Date", "days": 3_000_000_000_i64}),
            ),
            (
                "days above max date",
                json!({TYPE_KEY: "Date", "days": i32::MAX}),
            ),
            (
                "secs past midnight",
                json!({TYPE_KEY: "LocalTime", "secs": 86_400, "nanos": 0}),
            ),
            (
                "negative secs",
                json!({TYPE_KEY: "LocalTime", "secs": -1, "nanos": 0}),
            ),
            (
                "leap nanos off :59",
                json!({TYPE_KEY: "LocalTime", "secs": 0, "nanos": 1_000_000_000}),
            ),
            (
                "nanos 2e9",
                json!({TYPE_KEY: "LocalTime", "secs": 59, "nanos": 2_000_000_000}),
            ),
            (
                "offset 86400",
                json!({TYPE_KEY: "ZonedTime", "secs": 0, "nanos": 0, "offset": 86_400}),
            ),
            (
                "offset -86400",
                json!({TYPE_KEY: "ZonedTime", "secs": 0, "nanos": 0, "offset": -86_400}),
            ),
            (
                "duration nanos 1e9",
                json!({TYPE_KEY: "Duration", "years": 0, "months": 0, "secs": 0, "nanos": 1_000_000_000}),
            ),
            (
                "duration nanos -1e9",
                json!({TYPE_KEY: "Duration", "years": 0, "months": 0, "secs": 0, "nanos": -1_000_000_000}),
            ),
            (
                "duration nanos i64::MIN",
                json!({TYPE_KEY: "Duration", "years": 0, "months": 0, "secs": 0, "nanos": i64::MIN}),
            ),
            (
                "duration secs i64::MAX",
                json!({TYPE_KEY: "Duration", "years": 0, "months": 0, "secs": i64::MAX, "nanos": 0}),
            ),
            (
                "duration above max",
                json!({
                    TYPE_KEY: "Duration",
                    "years": 0,
                    "months": 0,
                    "secs": max.num_seconds(),
                    "nanos": max.subsec_nanos() + 1,
                }),
            ),
            (
                "duration +secs -nanos",
                json!({TYPE_KEY: "Duration", "years": 0, "months": 0, "secs": 1, "nanos": -1}),
            ),
            (
                "duration -secs +nanos",
                json!({TYPE_KEY: "Duration", "years": 0, "months": 0, "secs": -1, "nanos": 1}),
            ),
            (
                "non-numeric effective_from",
                json!({
                    TYPE_KEY: "ElementMetadata",
                    "reference": {"source_id": "src", "element_id": "e1"},
                    "labels": [],
                    "effective_from": "abc",
                }),
            ),
            (
                "negative effective_from",
                json!({
                    TYPE_KEY: "ElementMetadata",
                    "reference": {"source_id": "src", "element_id": "e1"},
                    "labels": [],
                    "effective_from": "-1",
                }),
            ),
            (
                "effective_from above u64::MAX",
                json!({
                    TYPE_KEY: "ElementMetadata",
                    "reference": {"source_id": "src", "element_id": "e1"},
                    "labels": [],
                    "effective_from": "18446744073709551616",
                }),
            ),
            (
                "non-string label",
                json!({
                    TYPE_KEY: "ElementMetadata",
                    "reference": {"source_id": "src", "element_id": "e1"},
                    "labels": [1],
                    "effective_from": "0",
                }),
            ),
            (
                "escaped value not a map",
                json!({TYPE_KEY: "Object", "value": [1]}),
            ),
            (
                "range without end field",
                json!({TYPE_KEY: "ListRange", "start": 1}),
            ),
            (
                "string range bound",
                json!({TYPE_KEY: "ListRange", "start": "1", "end": null}),
            ),
            (
                "float range bound",
                json!({TYPE_KEY: "ListRange", "start": null, "end": 1.5}),
            ),
            ("corrupt item in a list", json!([{TYPE_KEY: "Bogus"}])),
            ("corrupt value in a map", json!({"m": {TYPE_KEY: "Bogus"}})),
        ];
        for (name, value) in cases {
            assert_eq!(
                decode(&stored(value)).map(|_| ()),
                Err(StateCodecError::Corrupt),
                "{name}"
            );
        }
    }

    #[test]
    fn invalid_state_is_corrupt() {
        let cases = vec![
            (
                "v1 state without slot 1",
                state(&[("v", stored(json!(1))), ("0", ElementValue::Null)]),
            ),
            (
                "unknown version",
                state(&[
                    ("v", stored(json!(2))),
                    ("0", ElementValue::Null),
                    ("1", ElementValue::Null),
                ]),
            ),
            (
                "non-integer version",
                state(&[
                    ("v", stored(json!("1"))),
                    ("0", ElementValue::Null),
                    ("1", ElementValue::Null),
                ]),
            ),
        ];
        for (name, value) in &cases {
            assert_eq!(read_state(value), Err(StateCodecError::Corrupt), "{name}");
        }
    }

    #[test]
    fn v1_state_round_trips() {
        let first = encode(&date(2024, 1, 1)).unwrap();
        let second = encode(&VariableValue::Awaiting).unwrap();
        let ValueAccumulator::Map(written) = write_state(first.clone(), second.clone()) else {
            panic!("expected a map");
        };
        assert_eq!(
            written,
            state(&[
                ("v", stored(json!(1))),
                ("0", first.clone()),
                ("1", second.clone())
            ])
        );
        assert_eq!(read_state(&written), Ok([first, second]));
    }

    #[test]
    fn legacy_state_reads_as_before() {
        let local = NaiveDate::from_ymd_opt(2024, 1, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let envelope_shaped = stored(json!({TYPE_KEY: "Date", "days": 1}));
        let cases = vec![
            (
                "envelope-shaped user map and [Null]",
                Some(envelope_shaped.clone()),
                Some(stored(json!([null]))),
            ),
            (
                "local datetime and a missing slot",
                Some(ElementValue::LocalDateTime(local)),
                None,
            ),
        ];
        for (name, first, second) in cases {
            let mut legacy = ElementPropertyMap::new();
            for (slot, value) in SLOT_KEYS.iter().zip([&first, &second]) {
                if let Some(value) = value {
                    legacy.insert(slot, value.clone());
                }
            }
            let migrated = read_state(&legacy).unwrap();
            for (stored_value, encoded) in [&first, &second].into_iter().zip(&migrated) {
                let expected = match stored_value {
                    Some(value) => VariableValue::from(value),
                    None => VariableValue::Null,
                };
                assert_eq!(decode(encoded).unwrap(), expected, "{name}");
            }
        }

        let legacy = state(&[("0", envelope_shaped)]);
        let [escaped, _] = read_state(&legacy).unwrap();
        assert_eq!(
            escaped,
            stored(json!({TYPE_KEY: "Object", "value": {TYPE_KEY: "Date", "days": 1}}))
        );
    }
}
