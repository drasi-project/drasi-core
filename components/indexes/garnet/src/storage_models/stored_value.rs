// Copyright 2024 The Drasi Authors.
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

use std::{collections::HashMap, fmt, hash::Hasher, sync::Arc};

use bincode::Options;
use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone, Utc};
use drasi_core::models::{ElementPropertyMap, ElementValue, TypedElementValue};

/// Lists and maps nested deeper than this are stored as one binary blob. Prost
/// refuses to decode messages nested more than 100 levels, so deeper compact
/// nesting could be written but never read back.
const MAX_COMPACT_DEPTH: usize = 16;

/// A stored value that cannot be written or that is corrupt on read.
#[derive(Debug)]
pub enum StoredValueError {
    /// A value slot carried no recognised value.
    MissingValue,
    /// A stored instant is outside the supported date range.
    InvalidTimestamp { seconds: i64, nanos: u32 },
    /// A stored UTC offset is outside the valid range.
    InvalidOffset(i32),
    /// A rich or deeply nested value could not be serialised.
    EncodeBinary(Box<bincode::ErrorKind>),
    /// A stored rich or deeply nested value is malformed, truncated, has
    /// trailing data, or holds a value no valid element can hold.
    DecodeBinary(Box<bincode::ErrorKind>),
}

impl fmt::Display for StoredValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue => write!(f, "stored value has no recognised value"),
            Self::InvalidTimestamp { seconds, nanos } => {
                write!(f, "stored timestamp {seconds}s+{nanos}ns is out of range")
            }
            Self::InvalidOffset(offset) => write!(f, "stored UTC offset {offset}s is invalid"),
            Self::EncodeBinary(error) => write!(f, "cannot encode stored value: {error}"),
            Self::DecodeBinary(error) => write!(f, "stored binary value is corrupt: {error}"),
        }
    }
}

impl std::error::Error for StoredValueError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::EncodeBinary(error) | Self::DecodeBinary(error) => Some(error),
            _ => None,
        }
    }
}

/// Binary form of rich and deeply nested values. Unlike a JSON reader, bincode
/// has no nesting limit, so anything written here can be read back. Reads are
/// bounded by the input length and reject trailing bytes.
fn binary_options() -> impl bincode::Options {
    bincode::options()
        .with_fixint_encoding()
        .reject_trailing_bytes()
}

fn encode_binary<T: serde::Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, StoredValueError> {
    binary_options()
        .serialize(value)
        .map_err(StoredValueError::EncodeBinary)
}

fn decode_binary<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StoredValueError> {
    binary_options()
        .with_limit(bytes.len() as u64)
        .deserialize(bytes)
        .map_err(StoredValueError::DecodeBinary)
}

#[derive(Clone, PartialEq, Hash, ::prost::Message)]
pub struct StoredValueContainer {
    #[prost(oneof = "StoredValue", tags = "1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11")]
    pub value: ::core::option::Option<StoredValue>,
}

#[derive(PartialEq, Clone, ::prost::Oneof)]
pub enum StoredValue {
    #[prost(bool, tag = "1")]
    Bool(bool),

    #[prost(double, tag = "2")]
    Float(f64),

    #[prost(int64, tag = "3")]
    Integer(i64),

    #[prost(string, tag = "4")]
    String(String),

    #[prost(message, tag = "5")]
    List(StoredValueList),

    #[prost(message, tag = "6")]
    Object(StoredValueMap),

    /// Stores NaiveDateTime as seconds and nanoseconds since the Unix epoch
    #[prost(message, tag = "7")]
    LocalDateTime(StoredDateTime),

    /// Stores DateTime<FixedOffset> as an instant plus UTC offset seconds
    #[prost(message, tag = "8")]
    ZonedDateTime(StoredZonedDateTime),

    /// An explicit null. An empty container is corrupt, never null.
    #[prost(message, tag = "9")]
    Null(StoredNull),

    /// A rich value as the binary form of its validated `TypedElementValue`.
    #[prost(bytes = "vec", tag = "10")]
    Typed(Vec<u8>),

    /// A list or map nested beyond `MAX_COMPACT_DEPTH`, as the binary form of
    /// its `ElementValue`.
    #[prost(bytes = "vec", tag = "11")]
    Nested(Vec<u8>),
}

#[derive(Clone, PartialEq, Hash, ::prost::Message)]
pub struct StoredNull {}

/// Protobuf message for storing a local datetime without losing precision
#[derive(Clone, PartialEq, Hash, ::prost::Message)]
pub struct StoredDateTime {
    /// Seconds since Unix epoch
    #[prost(int64, tag = "1")]
    pub seconds: i64,
    /// Nanoseconds within the second (above 999,999,999 only for a leap second)
    #[prost(uint32, tag = "2")]
    pub nanos: u32,
}

/// Protobuf message for storing a zoned datetime
#[derive(Clone, PartialEq, Hash, ::prost::Message)]
pub struct StoredZonedDateTime {
    /// Seconds since Unix epoch (UTC)
    #[prost(int64, tag = "1")]
    pub seconds: i64,
    /// Nanoseconds within the second (above 999,999,999 only for a leap second)
    #[prost(uint32, tag = "2")]
    pub nanos: u32,
    /// UTC offset in seconds (e.g. 3600 for +01:00)
    #[prost(int32, tag = "3")]
    pub offset_seconds: i32,
}

impl std::hash::Hash for StoredValue {
    fn hash<H: Hasher>(&self, state: &mut H) {
        match self {
            StoredValue::Bool(b) => {
                0.hash(state);
                b.hash(state)
            }
            StoredValue::Float(f) => {
                1.hash(state);
                f.to_bits().hash(state)
            }
            StoredValue::Integer(i) => {
                2.hash(state);
                i.hash(state)
            }
            StoredValue::String(s) => {
                3.hash(state);
                s.hash(state)
            }
            StoredValue::List(l) => {
                4.hash(state);
                l.hash(state)
            }
            StoredValue::Object(o) => {
                5.hash(state);
                o.hash(state)
            }
            StoredValue::LocalDateTime(dt) => {
                6.hash(state);
                dt.hash(state)
            }
            StoredValue::ZonedDateTime(zdt) => {
                7.hash(state);
                zdt.hash(state)
            }
            StoredValue::Null(n) => {
                8.hash(state);
                n.hash(state)
            }
            StoredValue::Typed(bytes) => {
                9.hash(state);
                bytes.hash(state)
            }
            StoredValue::Nested(bytes) => {
                10.hash(state);
                bytes.hash(state)
            }
        }
    }
}

#[derive(Clone, PartialEq, Hash, ::prost::Message)]
pub struct StoredValueList {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<StoredValueContainer>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
pub struct StoredValueMap {
    #[prost(map = "string, message", tag = "1")]
    pub values: HashMap<String, StoredValueContainer>,
}

impl StoredValueMap {
    pub fn new() -> Self {
        StoredValueMap {
            values: HashMap::new(),
        }
    }

    /// The stored value of a property; null and absent properties are both `None`.
    pub fn get(&self, key: &str) -> Option<&StoredValue> {
        match self.values.get(key) {
            Some(v) => match v.value.as_ref() {
                Some(StoredValue::Null(_)) => None,
                value => value,
            },
            None => None,
        }
    }
}

impl std::hash::Hash for StoredValueMap {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for (key, value) in self.values.iter() {
            key.hash(state);
            value.hash(state);
        }
    }
}

impl TryFrom<&ElementPropertyMap> for StoredValueMap {
    type Error = StoredValueError;

    fn try_from(map: &ElementPropertyMap) -> Result<Self, Self::Error> {
        Self::from_properties(map, 0)
    }
}

impl StoredValueMap {
    fn from_properties(map: &ElementPropertyMap, depth: usize) -> Result<Self, StoredValueError> {
        let values = map
            .map_iter(move |key, value| {
                StoredValueContainer::from_value(value, depth).map(|value| (key.to_string(), value))
            })
            .collect::<Result<HashMap<_, _>, _>>()?;

        Ok(StoredValueMap { values })
    }
}

impl TryFrom<StoredValueMap> for ElementPropertyMap {
    type Error = StoredValueError;

    fn try_from(val: StoredValueMap) -> Result<Self, Self::Error> {
        let mut map = ElementPropertyMap::new();

        for (key, value) in val.values {
            map.insert(&key, ElementValue::try_from(value)?);
        }

        Ok(map)
    }
}

impl TryFrom<&ElementValue> for StoredValueContainer {
    type Error = StoredValueError;

    fn try_from(value: &ElementValue) -> Result<Self, Self::Error> {
        Self::from_value(value, 0)
    }
}

impl StoredValueContainer {
    fn from_value(value: &ElementValue, depth: usize) -> Result<Self, StoredValueError> {
        let value = match value {
            ElementValue::Null => StoredValue::Null(StoredNull {}),
            ElementValue::Bool(b) => StoredValue::Bool(*b),
            ElementValue::Float(f) => StoredValue::Float(f.into_inner()),
            ElementValue::Integer(i) => StoredValue::Integer(*i),
            ElementValue::String(s) => StoredValue::String(s.to_string()),
            ElementValue::List(_) | ElementValue::Object(_) if depth >= MAX_COMPACT_DEPTH => {
                StoredValue::Nested(encode_binary(value)?)
            }
            ElementValue::List(l) => StoredValue::List(StoredValueList {
                values: l
                    .iter()
                    .map(|value| Self::from_value(value, depth + 1))
                    .collect::<Result<_, _>>()?,
            }),
            ElementValue::Object(o) => {
                StoredValue::Object(StoredValueMap::from_properties(o, depth + 1)?)
            }
            ElementValue::LocalDateTime(dt) => {
                let utc = dt.and_utc();
                StoredValue::LocalDateTime(StoredDateTime {
                    seconds: utc.timestamp(),
                    nanos: utc.timestamp_subsec_nanos(),
                })
            }
            ElementValue::ZonedDateTime(dt) => StoredValue::ZonedDateTime(StoredZonedDateTime {
                seconds: dt.timestamp(),
                nanos: dt.timestamp_subsec_nanos(),
                offset_seconds: dt.offset().local_minus_utc(),
            }),
            ElementValue::Typed(value) => StoredValue::Typed(encode_binary(&**value)?),
        };

        Ok(StoredValueContainer { value: Some(value) })
    }
}

fn utc_from_parts(seconds: i64, nanos: u32) -> Result<DateTime<Utc>, StoredValueError> {
    DateTime::from_timestamp(seconds, nanos)
        .ok_or(StoredValueError::InvalidTimestamp { seconds, nanos })
}

impl TryFrom<StoredValueContainer> for ElementValue {
    type Error = StoredValueError;

    fn try_from(val: StoredValueContainer) -> Result<Self, Self::Error> {
        Ok(match val.value.ok_or(StoredValueError::MissingValue)? {
            StoredValue::Null(_) => ElementValue::Null,
            StoredValue::Bool(b) => ElementValue::Bool(b),
            StoredValue::Float(f) => ElementValue::Float(f.into()),
            StoredValue::Integer(i) => ElementValue::Integer(i),
            StoredValue::String(s) => ElementValue::String(Arc::from(s)),
            StoredValue::List(l) => ElementValue::List(
                l.values
                    .into_iter()
                    .map(ElementValue::try_from)
                    .collect::<Result<_, _>>()?,
            ),
            StoredValue::Object(o) => ElementValue::Object(ElementPropertyMap::try_from(o)?),
            StoredValue::LocalDateTime(dt) => {
                let utc: NaiveDateTime = utc_from_parts(dt.seconds, dt.nanos)?.naive_utc();
                ElementValue::LocalDateTime(utc)
            }
            StoredValue::ZonedDateTime(zdt) => {
                let offset = FixedOffset::east_opt(zdt.offset_seconds)
                    .ok_or(StoredValueError::InvalidOffset(zdt.offset_seconds))?;
                let utc = utc_from_parts(zdt.seconds, zdt.nanos)?;
                ElementValue::ZonedDateTime(offset.from_utc_datetime(&utc.naive_utc()))
            }
            StoredValue::Typed(bytes) => {
                let value: TypedElementValue = decode_binary(&bytes)?;
                ElementValue::Typed(Box::new(value))
            }
            StoredValue::Nested(bytes) => decode_binary(&bytes)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime};
    use drasi_core::evaluation::variable_value::{
        duration::Duration, integer::Integer, zoned_datetime::ZonedDateTime, zoned_time::ZonedTime,
        ListRange, RangeBound, VariableValue,
    };
    use drasi_core::models::ElementReference;
    use drasi_query_ast::ast::{Expression, Literal, UnaryExpression};
    use ordered_float::OrderedFloat;
    use prost::Message;

    fn encode(value: &ElementValue) -> Vec<u8> {
        StoredValueContainer::try_from(value)
            .unwrap()
            .encode_to_vec()
    }

    fn decode(bytes: &[u8]) -> Result<ElementValue, StoredValueError> {
        ElementValue::try_from(StoredValueContainer::decode(bytes).unwrap())
    }

    fn roundtrip(value: &ElementValue) -> ElementValue {
        decode(&encode(value)).unwrap()
    }

    fn typed(value: VariableValue) -> ElementValue {
        let value: ElementValue = (&value).try_into().unwrap();
        assert!(matches!(value, ElementValue::Typed(_)), "{value:?}");
        value
    }

    fn zoned(naive: NaiveDateTime, offset_seconds: i32) -> DateTime<FixedOffset> {
        FixedOffset::east_opt(offset_seconds)
            .unwrap()
            .from_local_datetime(&naive)
            .single()
            .unwrap()
    }

    fn precise_datetime() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_nano_opt(10, 30, 45, 123_456_789)
            .unwrap()
    }

    fn reference() -> ElementReference {
        ElementReference {
            source_id: Arc::from("source"),
            element_id: Arc::from("element"),
        }
    }

    #[test]
    fn roundtrip_local_datetime_keeps_nanoseconds() {
        let original = ElementValue::LocalDateTime(precise_datetime());
        let stored = StoredValueContainer::try_from(&original).unwrap();

        match &stored.value {
            Some(StoredValue::LocalDateTime(dt)) => assert_eq!(dt.nanos, 123_456_789),
            other => panic!("Expected LocalDateTime variant, got {other:?}"),
        }
        assert_eq!(roundtrip(&original), original);
    }

    #[test]
    fn roundtrip_datetimes_outside_nanosecond_i64_range() {
        let early = NaiveDate::from_ymd_opt(1500, 1, 1)
            .unwrap()
            .and_hms_nano_opt(0, 0, 0, 1)
            .unwrap();
        let late = NaiveDate::from_ymd_opt(9999, 12, 31)
            .unwrap()
            .and_hms_nano_opt(23, 59, 59, 999_999_999)
            .unwrap();

        for naive in [early, late] {
            let local = ElementValue::LocalDateTime(naive);
            assert_eq!(roundtrip(&local), local);
            let zoned = ElementValue::ZonedDateTime(zoned(naive, -(8 * 3600)));
            assert_eq!(roundtrip(&zoned), zoned);
        }
    }

    #[test]
    fn roundtrip_zoned_datetime_offsets() {
        for offset_seconds in [5 * 3600 + 1800, -8 * 3600, 0] {
            let original = ElementValue::ZonedDateTime(zoned(precise_datetime(), offset_seconds));
            let stored = StoredValueContainer::try_from(&original).unwrap();

            match &stored.value {
                Some(StoredValue::ZonedDateTime(zdt)) => {
                    assert_eq!(zdt.offset_seconds, offset_seconds);
                    assert_eq!(zdt.nanos, 123_456_789);
                }
                other => panic!("Expected ZonedDateTime variant, got {other:?}"),
            }
            match roundtrip(&original) {
                ElementValue::ZonedDateTime(dt) => {
                    assert_eq!(dt.offset().local_minus_utc(), offset_seconds);
                    assert_eq!(ElementValue::ZonedDateTime(dt), original);
                }
                other => panic!("Expected ZonedDateTime, got {other:?}"),
            }
        }
    }

    #[test]
    fn datetimes_are_not_stored_as_strings() {
        let local =
            StoredValueContainer::try_from(&ElementValue::LocalDateTime(precise_datetime()))
                .unwrap();
        assert!(matches!(local.value, Some(StoredValue::LocalDateTime(_))));

        let zoned = StoredValueContainer::try_from(&ElementValue::ZonedDateTime(zoned(
            precise_datetime(),
            3600,
        )))
        .unwrap();
        assert!(matches!(zoned.value, Some(StoredValue::ZonedDateTime(_))));
    }

    #[test]
    fn roundtrip_primitives_and_integer_limits() {
        let values = [
            ElementValue::Null,
            ElementValue::Bool(true),
            ElementValue::Bool(false),
            ElementValue::Integer(i64::MIN),
            ElementValue::Integer(0),
            ElementValue::Integer(i64::MAX),
            ElementValue::String(Arc::from("")),
            ElementValue::String(Arc::from("héllo")),
            ElementValue::Float(OrderedFloat(1.5)),
        ];
        for value in values {
            assert_eq!(roundtrip(&value), value);
        }
    }

    #[test]
    fn roundtrip_unsigned_limits() {
        for value in [u64::MAX, i64::MAX as u64 + 1, u64::MAX - 1] {
            let original = typed(VariableValue::Integer(Integer::from(value)));
            assert_eq!(roundtrip(&original), original);

            match VariableValue::from(&roundtrip(&original)) {
                VariableValue::Integer(integer) => assert_eq!(integer.as_u64(), Some(value)),
                other => panic!("Expected Integer, got {other:?}"),
            }
        }
    }

    #[test]
    fn roundtrip_nonfinite_float_bits() {
        let bits = [
            f64::NAN.to_bits(),
            0x7ff8_0000_0000_1234,
            0xfff8_0000_0000_0001,
            f64::INFINITY.to_bits(),
            f64::NEG_INFINITY.to_bits(),
            (-0.0_f64).to_bits(),
            f64::MIN_POSITIVE.to_bits() >> 1,
        ];
        for bits in bits {
            let original = ElementValue::Float(OrderedFloat(f64::from_bits(bits)));
            match roundtrip(&original) {
                ElementValue::Float(value) => assert_eq!(value.into_inner().to_bits(), bits),
                other => panic!("Expected Float, got {other:?}"),
            }
        }
    }

    #[test]
    fn roundtrip_rich_values() {
        let time = NaiveTime::from_hms_nano_opt(1, 2, 3, 456_789_012).unwrap();
        let rich = vec![
            VariableValue::Date(NaiveDate::from_ymd_opt(2024, 2, 29).unwrap()),
            VariableValue::LocalTime(time),
            VariableValue::ZonedTime(ZonedTime::new(
                time,
                FixedOffset::west_opt(7 * 3600 + 1800).unwrap(),
            )),
            VariableValue::Duration(Duration::new(
                chrono::Duration::try_seconds(90).unwrap() + chrono::Duration::nanoseconds(123),
                2,
                -3,
            )),
            VariableValue::ListRange(ListRange {
                start: RangeBound::Index(-1),
                end: RangeBound::Unbounded,
            }),
            VariableValue::ElementReference(reference()),
            VariableValue::Awaiting,
        ];
        for value in rich {
            let original = typed(value);
            assert_eq!(roundtrip(&original), original);
        }
    }

    #[test]
    fn roundtrip_named_zoned_datetime_keeps_zone_and_instant() {
        let datetime = zoned(precise_datetime(), 2 * 3600);
        let original = typed(VariableValue::ZonedDateTime(ZonedDateTime::new(
            datetime,
            Some("Europe/Berlin".to_string()),
        )));

        match VariableValue::from(&roundtrip(&original)) {
            VariableValue::ZonedDateTime(value) => {
                assert_eq!(value.timezone_name().as_deref(), Some("Europe/Berlin"));
                assert_eq!(value.datetime(), &datetime);
                assert_eq!(value.datetime().offset().local_minus_utc(), 2 * 3600);
            }
            other => panic!("Expected ZonedDateTime, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_nested_lists_and_maps_with_rich_values() {
        let mut inner = ElementPropertyMap::new();
        inner.insert(
            "date",
            typed(VariableValue::Date(
                NaiveDate::from_ymd_opt(1999, 12, 31).unwrap(),
            )),
        );
        inner.insert(
            "list",
            ElementValue::List(vec![
                typed(VariableValue::Awaiting),
                ElementValue::Null,
                ElementValue::LocalDateTime(precise_datetime()),
            ]),
        );
        let mut outer = ElementPropertyMap::new();
        outer.insert("inner", ElementValue::Object(inner));
        outer.insert(
            "max",
            typed(VariableValue::Integer(Integer::from(u64::MAX))),
        );
        outer.insert(
            "nan",
            ElementValue::Float(OrderedFloat(f64::from_bits(0x7ff8_0000_0000_1234))),
        );
        let original = ElementValue::List(vec![
            ElementValue::Object(outer),
            ElementValue::List(vec![ElementValue::Integer(1)]),
        ]);

        assert_eq!(roundtrip(&original), original);
    }

    #[test]
    fn property_map_roundtrip_keeps_null_properties() {
        let mut map = ElementPropertyMap::new();
        map.insert("null", ElementValue::Null);
        map.insert(
            "max",
            typed(VariableValue::Integer(Integer::from(u64::MAX))),
        );

        let stored = StoredValueMap::try_from(&map).unwrap();
        assert!(stored.get("null").is_none());
        assert!(stored.get("max").is_some());
        assert_eq!(ElementPropertyMap::try_from(stored).unwrap(), map);
    }

    #[test]
    fn empty_container_is_an_error_not_null() {
        let result = ElementValue::try_from(StoredValueContainer { value: None });
        assert!(matches!(result, Err(StoredValueError::MissingValue)));

        // A container holding only an unknown field decodes to an empty container.
        let unknown_field = [0x98, 0x06, 0x01];
        assert!(matches!(
            decode(&unknown_field),
            Err(StoredValueError::MissingValue)
        ));
    }

    #[test]
    fn nested_empty_container_is_an_error() {
        let stored = StoredValueContainer {
            value: Some(StoredValue::List(StoredValueList {
                values: vec![StoredValueContainer { value: None }],
            })),
        };
        assert!(matches!(
            ElementValue::try_from(stored),
            Err(StoredValueError::MissingValue)
        ));
    }

    fn typed_bytes(value: VariableValue) -> Vec<u8> {
        let stored = StoredValueContainer::decode(encode(&typed(value)).as_slice()).unwrap();
        match stored.value {
            Some(StoredValue::Typed(bytes)) => bytes,
            other => panic!("Expected Typed, got {other:?}"),
        }
    }

    fn assert_corrupt_typed(bytes: Vec<u8>) {
        let result = ElementValue::try_from(StoredValueContainer {
            value: Some(StoredValue::Typed(bytes.clone())),
        });
        assert!(
            matches!(result, Err(StoredValueError::DecodeBinary(_))),
            "{bytes:?}: {result:?}"
        );
    }

    fn nested_expression(levels: usize) -> Expression {
        let mut expression = UnaryExpression::literal(Literal::Boolean(true));
        for _ in 0..levels {
            expression = Expression::UnaryExpression(UnaryExpression::Not(Box::new(expression)));
        }
        expression
    }

    fn nested_value(levels: usize, leaf: ElementValue) -> ElementValue {
        let mut value = leaf;
        for level in 0..levels {
            value = if level % 2 == 0 {
                ElementValue::List(vec![value])
            } else {
                let mut properties = ElementPropertyMap::new();
                properties.insert("key", value);
                ElementValue::Object(properties)
            };
        }
        value
    }

    #[test]
    fn malformed_rich_values_are_errors() {
        let date = typed_bytes(VariableValue::Date(
            NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(),
        ));
        let mut cases: Vec<Vec<u8>> = vec![Vec::new(), date[..date.len() - 1].to_vec()];

        let mut trailing = date.clone();
        trailing.push(0);
        cases.push(trailing);

        let mut bad_variant = date.clone();
        bad_variant[..4].copy_from_slice(&u32::MAX.to_le_bytes());
        cases.push(bad_variant);

        // The string length prefix follows the 4-byte variant index.
        let mut oversized = date.clone();
        oversized[4..12].copy_from_slice(&u64::MAX.to_le_bytes());
        cases.push(oversized);

        // Well-formed data that no valid value can hold: an offset beyond one day.
        let mut bad_offset = typed_bytes(VariableValue::ZonedTime(ZonedTime::new(
            NaiveTime::from_hms_opt(12, 0, 0).unwrap(),
            FixedOffset::east_opt(3600).unwrap(),
        )));
        let end = bad_offset.len();
        bad_offset[end - 4..].copy_from_slice(&100_000_i32.to_le_bytes());
        cases.push(bad_offset);

        // Variant index, seconds, then the nanosecond remainder.
        let mut bad_nanos = typed_bytes(VariableValue::Duration(Duration::new(
            chrono::Duration::try_seconds(1).unwrap(),
            0,
            0,
        )));
        bad_nanos[12..16].copy_from_slice(&2_000_000_000_i32.to_le_bytes());
        cases.push(bad_nanos);

        for bytes in cases {
            assert_corrupt_typed(bytes);
        }
    }

    #[test]
    fn rich_expression_nested_beyond_json_limit_roundtrips() {
        let original = typed(VariableValue::Expression(nested_expression(300)));

        // The previous JSON form wrote this successfully but could not read it back.
        let ElementValue::Typed(inner) = &original else {
            panic!("expected a typed value");
        };
        let json = serde_json::to_vec(&**inner).unwrap();
        assert!(serde_json::from_slice::<TypedElementValue>(&json).is_err());

        assert_eq!(roundtrip(&original), original);
    }

    #[test]
    fn deep_rich_expression_rejects_truncation_and_trailing_data() {
        let bytes = typed_bytes(VariableValue::Expression(nested_expression(300)));
        assert_corrupt_typed(bytes[..bytes.len() - 1].to_vec());
        assert_corrupt_typed(bytes[..bytes.len() / 2].to_vec());

        let mut trailing = bytes;
        trailing.push(0);
        assert_corrupt_typed(trailing);
    }

    #[test]
    fn lists_and_maps_nested_beyond_protobuf_limit_roundtrip() {
        let leaves = [
            ElementValue::Integer(1),
            typed(VariableValue::Awaiting),
            ElementValue::Null,
        ];
        for levels in [
            1,
            MAX_COMPACT_DEPTH - 1,
            MAX_COMPACT_DEPTH,
            MAX_COMPACT_DEPTH + 1,
            200,
            500,
        ] {
            for leaf in &leaves {
                let original = nested_value(levels, leaf.clone());
                assert_eq!(roundtrip(&original), original, "{levels} levels");
            }
        }
    }

    #[test]
    fn corrupt_nested_values_are_errors() {
        let original = nested_value(MAX_COMPACT_DEPTH + 4, ElementValue::Integer(1));
        let mut properties = ElementPropertyMap::new();
        properties.insert("deep", original);
        let stored = StoredValueMap::try_from(&properties).unwrap();

        fn find_nested(container: &StoredValueContainer) -> Option<Vec<u8>> {
            match container.value.as_ref()? {
                StoredValue::Nested(bytes) => Some(bytes.clone()),
                StoredValue::List(list) => list.values.iter().find_map(find_nested),
                StoredValue::Object(map) => map.values.values().find_map(find_nested),
                _ => None,
            }
        }
        let bytes = find_nested(&stored.values["deep"]).expect("a nested blob");

        let mut trailing = bytes.clone();
        trailing.push(0);
        for corrupt in [
            Vec::new(),
            bytes[..bytes.len() - 1].to_vec(),
            trailing,
            vec![0xff; 16],
        ] {
            let result = ElementValue::try_from(StoredValueContainer {
                value: Some(StoredValue::Nested(corrupt)),
            });
            assert!(matches!(result, Err(StoredValueError::DecodeBinary(_))));
        }
    }

    #[test]
    fn truncated_protobuf_is_an_error() {
        let bytes = encode(&typed(VariableValue::Integer(Integer::from(u64::MAX))));
        assert!(StoredValueContainer::decode(&bytes[..bytes.len() - 1]).is_err());
    }

    #[test]
    fn invalid_stored_temporals_are_errors() {
        let cases = [
            StoredValue::LocalDateTime(StoredDateTime {
                seconds: i64::MAX,
                nanos: 0,
            }),
            StoredValue::LocalDateTime(StoredDateTime {
                seconds: 0,
                nanos: 2_000_000_000,
            }),
            StoredValue::ZonedDateTime(StoredZonedDateTime {
                seconds: i64::MIN,
                nanos: 0,
                offset_seconds: 0,
            }),
        ];
        for value in cases {
            let result = ElementValue::try_from(StoredValueContainer { value: Some(value) });
            assert!(matches!(
                result,
                Err(StoredValueError::InvalidTimestamp { .. })
            ));
        }

        let result = ElementValue::try_from(StoredValueContainer {
            value: Some(StoredValue::ZonedDateTime(StoredZonedDateTime {
                seconds: 0,
                nanos: 0,
                offset_seconds: 100_000,
            })),
        });
        assert!(matches!(
            result,
            Err(StoredValueError::InvalidOffset(100_000))
        ));
    }
}
