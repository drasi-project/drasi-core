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

use chrono::{DateTime, NaiveDateTime, Utc};
use drasi_core::models::ElementValue;
use ordered_float::OrderedFloat;
use postgres_types::Oid;
use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub enum PostgresValue {
    Null,
    Bool(bool),
    Int2(i16),
    Int4(i32),
    Int8(i64),
    Float4(f32),
    Float8(f64),
    Numeric(Decimal),
    Text(String),
    Varchar(String),
    Char(String),
    Uuid(Uuid),
    Timestamp(NaiveDateTime),
    TimestampTz(DateTime<Utc>),
    Date(chrono::NaiveDate),
    Time(chrono::NaiveTime),
    Json(JsonValue),
    Jsonb(JsonValue),
    Array(Vec<PostgresValue>),
    Composite(HashMap<String, PostgresValue>),
    Bytea(Vec<u8>),
}

#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub type_oid: Oid,
    pub type_modifier: i32,
    pub is_key: bool,
}

#[derive(Debug, Clone)]
pub struct RelationInfo {
    pub id: u32,
    pub namespace: String,
    pub name: String,
    pub replica_identity: ReplicaIdentity,
    pub columns: Vec<ColumnInfo>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ReplicaIdentity {
    Default,
    Nothing,
    Full,
    Index,
}

#[derive(Debug, Clone)]
pub struct TransactionInfo {
    pub xid: u32,
    pub commit_lsn: u64,
    pub commit_timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub enum WalMessage {
    Begin(TransactionInfo),
    Commit(TransactionInfo),
    Relation(RelationInfo),
    Insert {
        relation_id: u32,
        tuple: Vec<PostgresValue>,
    },
    Update {
        relation_id: u32,
        old_tuple: Option<Vec<PostgresValue>>,
        new_tuple: Vec<PostgresValue>,
    },
    Delete {
        relation_id: u32,
        old_tuple: Vec<PostgresValue>,
    },
    Truncate {
        relation_ids: Vec<u32>,
    },
}

#[derive(Debug, Clone)]
pub struct ReplicationSlotInfo {
    pub slot_name: String,
    pub consistent_point: String,
    pub snapshot_name: Option<String>,
    pub output_plugin: String,
}

#[derive(Debug, Clone)]
pub struct StandbyStatusUpdate {
    pub write_lsn: u64,
    pub flush_lsn: u64,
    pub apply_lsn: u64,
    pub reply_requested: bool,
}

impl PostgresValue {
    pub fn to_json(&self) -> JsonValue {
        match self {
            PostgresValue::Null => JsonValue::Null,
            PostgresValue::Bool(b) => JsonValue::Bool(*b),
            PostgresValue::Int2(i) => JsonValue::Number((*i).into()),
            PostgresValue::Int4(i) => JsonValue::Number((*i).into()),
            PostgresValue::Int8(i) => JsonValue::Number((*i).into()),
            PostgresValue::Float4(f) => {
                if let Some(n) = serde_json::Number::from_f64(*f as f64) {
                    JsonValue::Number(n)
                } else {
                    JsonValue::Null
                }
            }
            PostgresValue::Float8(f) => {
                if let Some(n) = serde_json::Number::from_f64(*f) {
                    JsonValue::Number(n)
                } else {
                    JsonValue::Null
                }
            }
            PostgresValue::Numeric(d) => JsonValue::String(d.to_string()),
            PostgresValue::Text(s) | PostgresValue::Varchar(s) | PostgresValue::Char(s) => {
                JsonValue::String(s.clone())
            }
            PostgresValue::Uuid(u) => JsonValue::String(u.to_string()),
            PostgresValue::Timestamp(ts) => JsonValue::String(ts.to_string()),
            PostgresValue::TimestampTz(ts) => JsonValue::String(ts.to_rfc3339()),
            PostgresValue::Date(d) => JsonValue::String(d.to_string()),
            PostgresValue::Time(t) => JsonValue::String(t.to_string()),
            PostgresValue::Json(j) | PostgresValue::Jsonb(j) => j.clone(),
            PostgresValue::Array(arr) => {
                JsonValue::Array(arr.iter().map(|v| v.to_json()).collect())
            }
            PostgresValue::Composite(map) => {
                let obj: serde_json::Map<String, JsonValue> =
                    map.iter().map(|(k, v)| (k.clone(), v.to_json())).collect();
                JsonValue::Object(obj)
            }
            PostgresValue::Bytea(bytes) => JsonValue::String(base64::encode(bytes)),
        }
    }

    /// Convert PostgresValue to ElementValue, preserving datetime types
    pub fn to_element_value(&self) -> ElementValue {
        match self {
            PostgresValue::Null => ElementValue::Null,
            PostgresValue::Bool(b) => ElementValue::Bool(*b),
            PostgresValue::Int2(i) => ElementValue::Integer(*i as i64),
            PostgresValue::Int4(i) => ElementValue::Integer(*i as i64),
            PostgresValue::Int8(i) => ElementValue::Integer(*i),
            PostgresValue::Float4(f) => ElementValue::Float(OrderedFloat(*f as f64)),
            PostgresValue::Float8(f) => ElementValue::Float(OrderedFloat(*f)),
            PostgresValue::Numeric(d) => {
                // Convert Decimal to f64 for storage
                ElementValue::Float(OrderedFloat(
                    d.to_string().parse::<f64>().unwrap_or(0.0),
                ))
            }
            PostgresValue::Text(s) | PostgresValue::Varchar(s) | PostgresValue::Char(s) => {
                ElementValue::String(Arc::from(s.as_str()))
            }
            PostgresValue::Uuid(u) => ElementValue::String(Arc::from(u.to_string())),
            PostgresValue::Timestamp(ts) => ElementValue::LocalDateTime(*ts),
            PostgresValue::TimestampTz(ts) => {
                ElementValue::ZonedDateTime(ts.fixed_offset())
            }
            PostgresValue::Date(d) => ElementValue::String(Arc::from(d.to_string())),
            PostgresValue::Time(t) => ElementValue::String(Arc::from(t.to_string())),
            PostgresValue::Json(j) | PostgresValue::Jsonb(j) => {
                ElementValue::String(Arc::from(j.to_string()))
            }
            PostgresValue::Array(arr) => {
                ElementValue::List(arr.iter().map(|v| v.to_element_value()).collect())
            }
            PostgresValue::Composite(_) | PostgresValue::Bytea(_) => {
                // Fall back to JSON string representation for complex types
                ElementValue::String(Arc::from(self.to_json().to_string()))
            }
        }
    }

    /// Returns true if this value is null
    pub fn is_null(&self) -> bool {
        matches!(self, PostgresValue::Null)
    }
}

// Add base64 encoding support
mod base64 {
    pub fn encode(input: &[u8]) -> String {
        const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut result = String::new();
        let mut i = 0;

        while i < input.len() {
            let b1 = input[i];
            let b2 = if i + 1 < input.len() { input[i + 1] } else { 0 };
            let b3 = if i + 2 < input.len() { input[i + 2] } else { 0 };

            result.push(TABLE[(b1 >> 2) as usize] as char);
            result.push(TABLE[(((b1 & 0x03) << 4) | (b2 >> 4)) as usize] as char);

            if i + 1 < input.len() {
                result.push(TABLE[(((b2 & 0x0f) << 2) | (b3 >> 6)) as usize] as char);
            } else {
                result.push('=');
            }

            if i + 2 < input.len() {
                result.push(TABLE[(b3 & 0x3f) as usize] as char);
            } else {
                result.push('=');
            }

            i += 3;
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};

    fn sample_naive_datetime() -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_opt(10, 30, 45)
            .unwrap()
    }

    fn sample_utc_datetime() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2024, 6, 15, 10, 30, 45).unwrap()
    }

    #[test]
    fn timestamp_to_element_value_is_local_datetime() {
        let ts = sample_naive_datetime();
        let pv = PostgresValue::Timestamp(ts);
        let ev = pv.to_element_value();
        assert_eq!(ev, ElementValue::LocalDateTime(ts));
    }

    #[test]
    fn timestamptz_to_element_value_is_zoned_datetime() {
        let ts = sample_utc_datetime();
        let pv = PostgresValue::TimestampTz(ts);
        let ev = pv.to_element_value();
        assert_eq!(ev, ElementValue::ZonedDateTime(ts.fixed_offset()));
    }

    #[test]
    fn null_to_element_value() {
        let pv = PostgresValue::Null;
        assert_eq!(pv.to_element_value(), ElementValue::Null);
        assert!(pv.is_null());
    }

    #[test]
    fn bool_to_element_value() {
        let pv = PostgresValue::Bool(true);
        assert_eq!(pv.to_element_value(), ElementValue::Bool(true));
        assert!(!pv.is_null());
    }

    #[test]
    fn int_types_to_element_value() {
        assert_eq!(
            PostgresValue::Int2(42).to_element_value(),
            ElementValue::Integer(42)
        );
        assert_eq!(
            PostgresValue::Int4(100_000).to_element_value(),
            ElementValue::Integer(100_000)
        );
        assert_eq!(
            PostgresValue::Int8(9_000_000_000).to_element_value(),
            ElementValue::Integer(9_000_000_000)
        );
    }

    #[test]
    fn float_types_to_element_value() {
        let ev = PostgresValue::Float4(3.14).to_element_value();
        match ev {
            ElementValue::Float(f) => assert!((f.into_inner() - 3.14f64).abs() < 0.001),
            other => panic!("Expected Float, got {other:?}"),
        }

        let ev = PostgresValue::Float8(2.718281828).to_element_value();
        match ev {
            ElementValue::Float(f) => {
                assert!((f.into_inner() - 2.718281828).abs() < 1e-9)
            }
            other => panic!("Expected Float, got {other:?}"),
        }
    }

    #[test]
    fn text_to_element_value() {
        let pv = PostgresValue::Text("hello".to_string());
        assert_eq!(
            pv.to_element_value(),
            ElementValue::String(Arc::from("hello"))
        );
    }

    #[test]
    fn array_with_timestamps_to_element_value() {
        let ts = sample_naive_datetime();
        let pv = PostgresValue::Array(vec![
            PostgresValue::Timestamp(ts),
            PostgresValue::Int4(42),
        ]);
        let ev = pv.to_element_value();
        match ev {
            ElementValue::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], ElementValue::LocalDateTime(ts));
                assert_eq!(items[1], ElementValue::Integer(42));
            }
            other => panic!("Expected List, got {other:?}"),
        }
    }

    #[test]
    fn timestamp_to_json_is_string() {
        // to_json() should still produce a string (backwards compat)
        let ts = sample_naive_datetime();
        let pv = PostgresValue::Timestamp(ts);
        let json = pv.to_json();
        assert!(json.is_string());
    }

    #[test]
    fn timestamptz_to_json_is_string() {
        let ts = sample_utc_datetime();
        let pv = PostgresValue::TimestampTz(ts);
        let json = pv.to_json();
        assert!(json.is_string());
    }
}
