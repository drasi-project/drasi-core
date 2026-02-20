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
use postgres_types::Oid;
use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
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
            PostgresValue::Numeric(d) => {
                // Convert Decimal to Number via string parsing
                // This ensures precision is maintained and the value is valid JSON number
                d.to_string()
                    .parse::<serde_json::Number>()
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null)
            }
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
    use std::str::FromStr;

    #[test]
    fn test_decimal_to_json_as_number() {
        // Test that decimal values are serialized as numbers, not strings
        let decimal = Decimal::from_str("123.45").unwrap();
        let pg_value = PostgresValue::Numeric(decimal);
        let json = pg_value.to_json();

        // Should be a Number, not a String
        assert!(json.is_number(), "Decimal should be serialized as a number");

        // Verify the value is correct
        let num = json.as_f64().unwrap();
        assert_eq!(num, 123.45);
    }

    #[test]
    fn test_decimal_integer_to_json() {
        let decimal = Decimal::from_str("100").unwrap();
        let pg_value = PostgresValue::Numeric(decimal);
        let json = pg_value.to_json();

        assert!(
            json.is_number(),
            "Integer decimal should be serialized as a number"
        );

        let num = json.as_f64().unwrap();
        assert_eq!(num, 100.0);
    }

    #[test]
    fn test_decimal_small_value_to_json() {
        let decimal = Decimal::from_str("0.00001").unwrap();
        let pg_value = PostgresValue::Numeric(decimal);
        let json = pg_value.to_json();

        assert!(
            json.is_number(),
            "Small decimal should be serialized as a number"
        );

        let num = json.as_f64().unwrap();
        assert_eq!(num, 0.00001);
    }

    #[test]
    fn test_decimal_negative_to_json() {
        let decimal = Decimal::from_str("-999.99").unwrap();
        let pg_value = PostgresValue::Numeric(decimal);
        let json = pg_value.to_json();

        assert!(
            json.is_number(),
            "Negative decimal should be serialized as a number"
        );

        let num = json.as_f64().unwrap();
        assert_eq!(num, -999.99);
    }

    #[test]
    fn test_decimal_zero_to_json() {
        let decimal = Decimal::from_str("0").unwrap();
        let pg_value = PostgresValue::Numeric(decimal);
        let json = pg_value.to_json();

        assert!(
            json.is_number(),
            "Zero decimal should be serialized as a number"
        );

        let num = json.as_f64().unwrap();
        assert_eq!(num, 0.0);
    }
}
