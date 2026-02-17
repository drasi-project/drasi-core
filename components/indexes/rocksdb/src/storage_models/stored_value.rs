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

use std::{collections::HashMap, hash::Hasher, sync::Arc};

use chrono::{DateTime, FixedOffset, NaiveDateTime, TimeZone};
use drasi_core::models::{ElementPropertyMap, ElementValue};

#[derive(Clone, PartialEq, Hash, ::prost::Message)]
pub struct StoredValueContainer {
    #[prost(oneof = "StoredValue", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
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

    /// Stores NaiveDateTime as microseconds since Unix epoch
    #[prost(int64, tag = "7")]
    LocalDateTime(i64),

    /// Stores DateTime<FixedOffset> as timestamp micros + UTC offset seconds
    #[prost(message, tag = "8")]
    ZonedDateTime(StoredZonedDateTime),
}

/// Protobuf message for storing a zoned datetime
#[derive(Clone, PartialEq, Hash, ::prost::Message)]
pub struct StoredZonedDateTime {
    /// Microseconds since Unix epoch (UTC)
    #[prost(int64, tag = "1")]
    pub timestamp_micros: i64,
    /// UTC offset in seconds (e.g. 3600 for +01:00)
    #[prost(int32, tag = "2")]
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
            StoredValue::LocalDateTime(micros) => {
                6.hash(state);
                micros.hash(state)
            }
            StoredValue::ZonedDateTime(zdt) => {
                7.hash(state);
                zdt.hash(state)
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

    pub fn get(&self, key: &str) -> Option<&StoredValue> {
        match self.values.get(key) {
            Some(v) => v.value.as_ref(),
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

impl From<&ElementPropertyMap> for StoredValueMap {
    fn from(map: &ElementPropertyMap) -> Self {
        let mut values = HashMap::new();

        map.map_iter(|key, value| (key.to_string(), value.into()))
            .for_each(|(key, value)| {
                values.insert(key, value);
            });

        StoredValueMap { values }
    }
}

impl From<StoredValueMap> for ElementPropertyMap {
    fn from(val: StoredValueMap) -> Self {
        let mut map = ElementPropertyMap::new();

        for (key, value) in val.values {
            map.insert(&key, value.into());
        }

        map
    }
}

impl From<&ElementValue> for StoredValueContainer {
    fn from(value: &ElementValue) -> Self {
        match value {
            ElementValue::Null => StoredValueContainer { value: None },
            ElementValue::Bool(b) => StoredValueContainer {
                value: Some(StoredValue::Bool(*b)),
            },
            ElementValue::Float(f) => StoredValueContainer {
                value: Some(StoredValue::Float(f.into_inner())),
            },
            ElementValue::Integer(i) => StoredValueContainer {
                value: Some(StoredValue::Integer(*i)),
            },
            ElementValue::String(s) => StoredValueContainer {
                value: Some(StoredValue::String(s.to_string())),
            },
            ElementValue::List(l) => StoredValueContainer {
                value: Some(StoredValue::List(StoredValueList {
                    values: l.iter().map(|v| v.into()).collect(),
                })),
            },
            ElementValue::Object(o) => StoredValueContainer {
                value: Some(StoredValue::Object(o.into())),
            },
            ElementValue::LocalDateTime(dt) => StoredValueContainer {
                value: Some(StoredValue::LocalDateTime(
                    dt.and_utc().timestamp_micros(),
                )),
            },
            ElementValue::ZonedDateTime(dt) => StoredValueContainer {
                value: Some(StoredValue::ZonedDateTime(StoredZonedDateTime {
                    timestamp_micros: dt.timestamp_micros(),
                    offset_seconds: dt.offset().local_minus_utc(),
                })),
            },
        }
    }
}

impl From<StoredValueContainer> for ElementValue {
    fn from(val: StoredValueContainer) -> Self {
        match val.value {
            None => ElementValue::Null,
            Some(StoredValue::Bool(b)) => ElementValue::Bool(b),
            Some(StoredValue::Float(f)) => ElementValue::Float(f.into()),
            Some(StoredValue::Integer(i)) => ElementValue::Integer(i),
            Some(StoredValue::String(s)) => ElementValue::String(Arc::from(s.as_str())),
            Some(StoredValue::List(l)) => {
                ElementValue::List(l.values.into_iter().map(|v| v.into()).collect())
            }
            Some(StoredValue::Object(o)) => ElementValue::Object(o.into()),
            Some(StoredValue::LocalDateTime(micros)) => {
                let dt = DateTime::from_timestamp_micros(micros)
                    .unwrap_or_default()
                    .naive_utc();
                ElementValue::LocalDateTime(dt)
            }
            Some(StoredValue::ZonedDateTime(zdt)) => {
                let offset = FixedOffset::east_opt(zdt.offset_seconds)
                    .unwrap_or_else(|| FixedOffset::east_opt(0).unwrap());
                let utc_dt = DateTime::from_timestamp_micros(zdt.timestamp_micros)
                    .unwrap_or_default();
                let dt = offset.from_utc_datetime(&utc_dt.naive_utc());
                ElementValue::ZonedDateTime(dt)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use prost::Message;

    #[test]
    fn roundtrip_local_datetime() {
        let dt = NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_micro_opt(10, 30, 45, 123_456)
            .unwrap();
        let original = ElementValue::LocalDateTime(dt);

        // Serialize to StoredValueContainer
        let stored: StoredValueContainer = (&original).into();

        // Verify it uses the LocalDateTime variant, not String
        match &stored.value {
            Some(StoredValue::LocalDateTime(_)) => {} // correct
            other => panic!("Expected LocalDateTime variant, got {:?}", other),
        }

        // Encode to protobuf bytes and decode back
        let mut buf = Vec::new();
        stored.encode(&mut buf).unwrap();
        let decoded = StoredValueContainer::decode(buf.as_slice()).unwrap();

        // Convert back to ElementValue
        let result: ElementValue = decoded.into();
        assert_eq!(result, original);
    }

    #[test]
    fn roundtrip_zoned_datetime() {
        let offset = FixedOffset::east_opt(5 * 3600 + 1800).unwrap(); // +05:30
        let dt = offset
            .with_ymd_and_hms(2024, 1, 15, 14, 30, 0)
            .unwrap();
        let original = ElementValue::ZonedDateTime(dt);

        // Serialize to StoredValueContainer
        let stored: StoredValueContainer = (&original).into();

        // Verify it uses the ZonedDateTime variant, not String
        match &stored.value {
            Some(StoredValue::ZonedDateTime(zdt)) => {
                assert_eq!(zdt.offset_seconds, 5 * 3600 + 1800);
            }
            other => panic!("Expected ZonedDateTime variant, got {:?}", other),
        }

        // Encode to protobuf bytes and decode back
        let mut buf = Vec::new();
        stored.encode(&mut buf).unwrap();
        let decoded = StoredValueContainer::decode(buf.as_slice()).unwrap();

        // Convert back to ElementValue
        let result: ElementValue = decoded.into();
        assert_eq!(result, original);
    }

    #[test]
    fn roundtrip_zoned_datetime_negative_offset() {
        let offset = FixedOffset::west_opt(8 * 3600).unwrap(); // -08:00 (PST)
        let dt = offset
            .with_ymd_and_hms(2024, 12, 25, 0, 0, 0)
            .unwrap();
        let original = ElementValue::ZonedDateTime(dt);

        let stored: StoredValueContainer = (&original).into();
        let mut buf = Vec::new();
        stored.encode(&mut buf).unwrap();
        let decoded = StoredValueContainer::decode(buf.as_slice()).unwrap();
        let result: ElementValue = decoded.into();

        assert_eq!(result, original);
        match &result {
            ElementValue::ZonedDateTime(dt) => {
                assert_eq!(dt.offset().local_minus_utc(), -8 * 3600);
            }
            _ => panic!("Expected ZonedDateTime"),
        }
    }

    #[test]
    fn roundtrip_zoned_datetime_utc() {
        let offset = FixedOffset::east_opt(0).unwrap(); // UTC
        let dt = offset
            .with_ymd_and_hms(2024, 3, 1, 12, 0, 0)
            .unwrap();
        let original = ElementValue::ZonedDateTime(dt);

        let stored: StoredValueContainer = (&original).into();
        let mut buf = Vec::new();
        stored.encode(&mut buf).unwrap();
        let decoded = StoredValueContainer::decode(buf.as_slice()).unwrap();
        let result: ElementValue = decoded.into();

        assert_eq!(result, original);
    }

    #[test]
    fn local_datetime_not_stored_as_string() {
        let dt = NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_opt(10, 30, 45)
            .unwrap();
        let original = ElementValue::LocalDateTime(dt);
        let stored: StoredValueContainer = (&original).into();

        // Must NOT be stored as a String
        match &stored.value {
            Some(StoredValue::String(_)) => {
                panic!("LocalDateTime should not be stored as String")
            }
            Some(StoredValue::LocalDateTime(_)) => {} // correct
            other => panic!("Unexpected variant: {:?}", other),
        }
    }

    #[test]
    fn zoned_datetime_not_stored_as_string() {
        let offset = FixedOffset::east_opt(3600).unwrap();
        let dt = offset
            .with_ymd_and_hms(2024, 6, 15, 10, 30, 45)
            .unwrap();
        let original = ElementValue::ZonedDateTime(dt);
        let stored: StoredValueContainer = (&original).into();

        // Must NOT be stored as a String
        match &stored.value {
            Some(StoredValue::String(_)) => {
                panic!("ZonedDateTime should not be stored as String")
            }
            Some(StoredValue::ZonedDateTime(_)) => {} // correct
            other => panic!("Unexpected variant: {:?}", other),
        }
    }
}
