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

//! Canonical PostgreSQL value representation and ElementValue conversion.

use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use drasi_core::models::ElementValue;
use ordered_float::OrderedFloat;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use uuid::Uuid;

/// Intermediate typed value shared by bootstrap and CDC paths.
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
    Date(NaiveDate),
    Time(NaiveTime),
    Json(JsonValue),
    Jsonb(JsonValue),
    Array(Vec<PostgresValue>),
    Bytea(Vec<u8>),
}

impl PostgresValue {
    /// Convert to JSON (lossy for temporals). Prefer [`to_element_value`] for Drasi paths.
    pub fn to_json(&self) -> JsonValue {
        match self {
            PostgresValue::Null => JsonValue::Null,
            PostgresValue::Bool(b) => JsonValue::Bool(*b),
            PostgresValue::Int2(i) => JsonValue::Number((*i).into()),
            PostgresValue::Int4(i) => JsonValue::Number((*i).into()),
            PostgresValue::Int8(i) => JsonValue::Number((*i).into()),
            PostgresValue::Float4(f) => serde_json::Number::from_f64(*f as f64)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null),
            PostgresValue::Float8(f) => serde_json::Number::from_f64(*f)
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null),
            PostgresValue::Numeric(d) => d
                .to_string()
                .parse::<serde_json::Number>()
                .map(JsonValue::Number)
                .unwrap_or(JsonValue::Null),
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
            PostgresValue::Bytea(bytes) => JsonValue::String(encode_base64(bytes)),
        }
    }

    /// Canonical conversion used by both bootstrap and CDC.
    ///
    /// Mapping:
    /// - `Timestamp` → `LocalDateTime`, `TimestampTz` → `ZonedDateTime`
    /// - `Numeric` → `Float` (always, including whole numbers)
    /// - `Date`, `Time`, `Uuid` → `String`
    /// - `Json` / `Jsonb` → `String` (compact JSON text)
    /// - `Bytea` → `String` (base64 of raw bytes)
    /// - `Char` is expected to already be trimmed by the decoder
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
                ElementValue::Float(OrderedFloat(d.to_f64().unwrap_or(f64::NAN)))
            }
            PostgresValue::Text(s) | PostgresValue::Varchar(s) | PostgresValue::Char(s) => {
                ElementValue::String(Arc::from(s.as_str()))
            }
            PostgresValue::Uuid(u) => ElementValue::String(Arc::from(u.to_string())),
            PostgresValue::Timestamp(ts) => ElementValue::LocalDateTime(*ts),
            PostgresValue::TimestampTz(ts) => ElementValue::ZonedDateTime(ts.fixed_offset()),
            PostgresValue::Date(d) => ElementValue::String(Arc::from(d.to_string())),
            PostgresValue::Time(t) => ElementValue::String(Arc::from(t.to_string())),
            PostgresValue::Json(j) | PostgresValue::Jsonb(j) => {
                ElementValue::String(Arc::from(j.to_string()))
            }
            PostgresValue::Array(arr) => {
                ElementValue::List(arr.iter().map(|v| v.to_element_value()).collect())
            }
            PostgresValue::Bytea(bytes) => ElementValue::String(Arc::from(encode_base64(bytes))),
        }
    }

    /// Returns `true` if this value is [`PostgresValue::Null`].
    pub fn is_null(&self) -> bool {
        matches!(self, PostgresValue::Null)
    }

    /// Stable string form for element ID key parts.
    pub fn to_key_string(&self) -> Option<String> {
        match self {
            PostgresValue::Null => None,
            PostgresValue::Bool(b) => Some(b.to_string()),
            PostgresValue::Int2(i) => Some(i.to_string()),
            PostgresValue::Int4(i) => Some(i.to_string()),
            PostgresValue::Int8(i) => Some(i.to_string()),
            PostgresValue::Float4(f) => Some(f.to_string()),
            PostgresValue::Float8(f) => Some(f.to_string()),
            PostgresValue::Numeric(d) => Some(d.to_string()),
            PostgresValue::Text(s) | PostgresValue::Varchar(s) | PostgresValue::Char(s) => {
                Some(s.clone())
            }
            PostgresValue::Uuid(u) => Some(u.to_string()),
            PostgresValue::Timestamp(ts) => Some(ts.to_string()),
            PostgresValue::TimestampTz(ts) => Some(ts.to_rfc3339()),
            PostgresValue::Date(d) => Some(d.to_string()),
            PostgresValue::Time(t) => Some(t.to_string()),
            PostgresValue::Json(j) | PostgresValue::Jsonb(j) => Some(j.to_string()),
            PostgresValue::Bytea(bytes) => Some(encode_base64(bytes)),
            PostgresValue::Array(_) => Some(self.to_json().to_string()),
        }
    }
}

/// Base64-encode bytes (standard alphabet with padding).
pub fn encode_base64(input: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(input)
}

/// Parse PostgreSQL text-format bytea (`\xdeadbeef` or escaped).
pub fn parse_bytea_text(text: &str) -> anyhow::Result<Vec<u8>> {
    let trimmed = text.trim();
    if let Some(hex) = trimmed
        .strip_prefix("\\x")
        .or_else(|| trimmed.strip_prefix(r"\x"))
    {
        if hex.len() % 2 != 0 {
            anyhow::bail!("Odd-length bytea hex string");
        }
        let mut out = Vec::with_capacity(hex.len() / 2);
        for i in (0..hex.len()).step_by(2) {
            let byte = u8::from_str_radix(&hex[i..i + 2], 16)
                .map_err(|e| anyhow::anyhow!("Invalid bytea hex: {e}"))?;
            out.push(byte);
        }
        return Ok(out);
    }
    // Fallback: treat as raw UTF-8 bytes of the text representation
    Ok(trimmed.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use rust_decimal::Decimal;
    use std::str::FromStr;

    #[test]
    fn timestamp_to_element_value_is_local_datetime() {
        let ts = NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_opt(10, 30, 45)
            .unwrap();
        assert_eq!(
            PostgresValue::Timestamp(ts).to_element_value(),
            ElementValue::LocalDateTime(ts)
        );
    }

    #[test]
    fn timestamptz_to_element_value_is_zoned_datetime() {
        let ts = Utc.with_ymd_and_hms(2024, 6, 15, 10, 30, 45).unwrap();
        assert_eq!(
            PostgresValue::TimestampTz(ts).to_element_value(),
            ElementValue::ZonedDateTime(ts.fixed_offset())
        );
    }

    #[test]
    fn numeric_whole_number_is_float() {
        let dec = Decimal::from_str("4200").unwrap();
        match PostgresValue::Numeric(dec).to_element_value() {
            ElementValue::Float(f) => assert_eq!(f.into_inner(), 4200.0),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn bytea_is_base64_of_raw_bytes() {
        let bytes = vec![0xde, 0xad, 0xbe, 0xef];
        let ev = PostgresValue::Bytea(bytes.clone()).to_element_value();
        assert_eq!(ev, ElementValue::String(Arc::from(encode_base64(&bytes))));
        // Must not be JSON-quoted
        if let ElementValue::String(s) = ev {
            assert!(!s.starts_with('"'));
        }
    }

    #[test]
    fn parse_bytea_hex_text() {
        assert_eq!(
            parse_bytea_text(r"\xdeadbeef").unwrap(),
            vec![0xde, 0xad, 0xbe, 0xef]
        );
    }

    #[test]
    fn null_to_element_value() {
        assert_eq!(PostgresValue::Null.to_element_value(), ElementValue::Null);
        assert!(PostgresValue::Null.is_null());
    }

    #[test]
    fn uuid_to_element_value_is_string() {
        let uuid = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        assert_eq!(
            PostgresValue::Uuid(uuid).to_element_value(),
            ElementValue::String(Arc::from(uuid.to_string()))
        );
    }

    #[test]
    fn json_to_element_value_is_string() {
        let j = serde_json::json!({"k": 1});
        let ev = PostgresValue::Json(j.clone()).to_element_value();
        assert_eq!(ev, ElementValue::String(Arc::from(j.to_string())));
        let evb = PostgresValue::Jsonb(j.clone()).to_element_value();
        assert_eq!(evb, ElementValue::String(Arc::from(j.to_string())));
    }

    #[test]
    fn array_to_element_value_is_list() {
        let arr = PostgresValue::Array(vec![
            PostgresValue::Int4(1),
            PostgresValue::Null,
            PostgresValue::Text("x".into()),
        ]);
        match arr.to_element_value() {
            ElementValue::List(items) => {
                assert_eq!(items.len(), 3);
                assert_eq!(items[0], ElementValue::Integer(1));
                assert_eq!(items[1], ElementValue::Null);
                assert_eq!(items[2], ElementValue::String(Arc::from("x")));
            }
            other => panic!("expected List, got {other:?}"),
        }
    }
}
