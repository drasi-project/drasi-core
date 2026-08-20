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

//! Text-format PostgreSQL value decoding (pgoutput / bootstrap cast-to-text).

use crate::oid::{self, array_element_oid, is_array_oid};
use crate::value::{parse_bytea_text, PostgresValue};
use anyhow::{anyhow, Result};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use drasi_core::models::ElementValue;
use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use std::sync::Arc;
use uuid::Uuid;

/// Decode a pgoutput/text column into a [`PostgresValue`].
///
/// Unknown or unparseable values fall back to `Text` rather than hard-failing,
/// except where a clear type-specific parse is expected and fails in a way that
/// callers may want to handle (still returns Ok with Text fallback for safety).
pub fn decode_text_to_postgres_value(text: &str, type_oid: u32) -> Result<PostgresValue> {
    let trimmed = text.trim();

    if is_array_oid(type_oid) {
        let elem_oid = array_element_oid(type_oid).unwrap_or(oid::TEXT);
        return parse_array_text(trimmed, elem_oid);
    }

    match type_oid {
        oid::BOOL => {
            // Strict pgoutput forms: t/f/true/false
            let value = match trimmed {
                "t" | "true" => true,
                "f" | "false" => false,
                _ => return Err(anyhow!("Invalid boolean value for OID {type_oid}")),
            };
            Ok(PostgresValue::Bool(value))
        }
        oid::INT2 => {
            Ok(PostgresValue::Int2(trimmed.parse::<i16>().map_err(
                |e| anyhow!("Failed to parse int2 (OID {type_oid}): {e}"),
            )?))
        }
        oid::INT4 => {
            Ok(PostgresValue::Int4(trimmed.parse::<i32>().map_err(
                |e| anyhow!("Failed to parse int4 (OID {type_oid}): {e}"),
            )?))
        }
        oid::INT8 => {
            Ok(PostgresValue::Int8(trimmed.parse::<i64>().map_err(
                |e| anyhow!("Failed to parse int8 (OID {type_oid}): {e}"),
            )?))
        }
        oid::FLOAT4 => {
            Ok(PostgresValue::Float4(trimmed.parse::<f32>().map_err(
                |e| anyhow!("Failed to parse float4 (OID {type_oid}): {e}"),
            )?))
        }
        oid::FLOAT8 => {
            Ok(PostgresValue::Float8(trimmed.parse::<f64>().map_err(
                |e| anyhow!("Failed to parse float8 (OID {type_oid}): {e}"),
            )?))
        }
        oid::NUMERIC => {
            let value = Decimal::from_str_exact(trimmed)
                .or_else(|_| trimmed.parse::<Decimal>())
                .map_err(|e| anyhow!("Failed to parse numeric (OID {type_oid}): {e}"))?;
            Ok(PostgresValue::Numeric(value))
        }
        oid::TEXT | oid::NAME => Ok(PostgresValue::Text(text.to_string())),
        oid::VARCHAR => Ok(PostgresValue::Varchar(text.to_string())),
        oid::CHAR => Ok(PostgresValue::Char(text.trim_end().to_string())),
        oid::UUID => {
            let uuid = Uuid::parse_str(trimmed)
                .map_err(|e| anyhow!("Failed to parse uuid (OID {type_oid}): {e}"))?;
            Ok(PostgresValue::Uuid(uuid))
        }
        oid::TIMESTAMP => {
            if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f") {
                Ok(PostgresValue::Timestamp(dt))
            } else if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
                Ok(PostgresValue::Timestamp(dt))
            } else {
                // Malformed for this OID (e.g. includes timezone) — keep as text
                Ok(PostgresValue::Text(text.to_string()))
            }
        }
        oid::TIMESTAMPTZ => decode_timestamptz_text(trimmed, text),
        oid::DATE => {
            if let Ok(d) = NaiveDate::parse_from_str(trimmed, "%Y-%m-%d") {
                Ok(PostgresValue::Date(d))
            } else {
                Ok(PostgresValue::Text(text.to_string()))
            }
        }
        oid::TIME => {
            if let Ok(t) = NaiveTime::parse_from_str(trimmed, "%H:%M:%S%.f") {
                Ok(PostgresValue::Time(t))
            } else if let Ok(t) = NaiveTime::parse_from_str(trimmed, "%H:%M:%S") {
                Ok(PostgresValue::Time(t))
            } else {
                Ok(PostgresValue::Text(text.to_string()))
            }
        }
        oid::JSON => {
            let value: JsonValue = serde_json::from_str(trimmed)
                .map_err(|e| anyhow!("Failed to parse json (OID {type_oid}): {e}"))?;
            Ok(PostgresValue::Json(value))
        }
        oid::JSONB => {
            // Text mode has no version byte
            let value: JsonValue = serde_json::from_str(trimmed)
                .map_err(|e| anyhow!("Failed to parse jsonb (OID {type_oid}): {e}"))?;
            Ok(PostgresValue::Jsonb(value))
        }
        oid::BYTEA => {
            let bytes = parse_bytea_text(trimmed)?;
            Ok(PostgresValue::Bytea(bytes))
        }
        _ => Ok(PostgresValue::Text(text.to_string())),
    }
}

fn decode_timestamptz_text(trimmed: &str, original: &str) -> Result<PostgresValue> {
    // RFC3339 first
    if let Ok(dt) = DateTime::parse_from_rfc3339(trimmed) {
        return Ok(PostgresValue::TimestampTz(dt.with_timezone(&Utc)));
    }
    // Full offset with optional fractional seconds: +00:00 / +0000 / short +00 / Z
    if let Ok(dt) = DateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f%#z") {
        return Ok(PostgresValue::TimestampTz(dt.with_timezone(&Utc)));
    }
    if let Ok(dt) = DateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%#z") {
        return Ok(PostgresValue::TimestampTz(dt.with_timezone(&Utc)));
    }
    if let Ok(dt) = DateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f%#z") {
        return Ok(PostgresValue::TimestampTz(dt.with_timezone(&Utc)));
    }
    // No offset — assume UTC (matches existing bootstrap text helper)
    if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S%.f") {
        return Ok(PostgresValue::TimestampTz(dt.and_utc()));
    }
    if let Ok(dt) = NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S") {
        return Ok(PostgresValue::TimestampTz(dt.and_utc()));
    }
    Ok(PostgresValue::Text(original.to_string()))
}

/// Decode text directly to [`ElementValue`] using the canonical mapping.
pub fn decode_text_to_element_value(text: &str, type_oid: i32) -> Result<ElementValue> {
    Ok(decode_text_to_postgres_value(text, type_oid as u32)?.to_element_value())
}

/// Back-compat alias used by existing tests in the source crate.
pub fn decode_column_value_text(text: &str, type_oid: i32) -> Result<ElementValue> {
    decode_text_to_element_value(text, type_oid)
}

/// Parse a PostgreSQL 1-D array text literal like `{1,2,NULL,"a"}`.
///
/// Quote markers are tracked separately from element content so a quoted
/// `"NULL"` string is preserved while an unquoted `NULL` becomes
/// [`PostgresValue::Null`].
fn parse_array_text(text: &str, element_oid: u32) -> Result<PostgresValue> {
    let s = text.trim();
    let inner = if let Some(body) = s.strip_prefix('{').and_then(|x| x.strip_suffix('}')) {
        body
    } else {
        // Not an array literal — keep as text
        return Ok(PostgresValue::Text(text.to_string()));
    };

    if inner.is_empty() {
        return Ok(PostgresValue::Array(vec![]));
    }

    let mut elements = Vec::new();
    let mut cur = String::new();
    let mut in_quotes = false;
    let mut escape = false;
    // True if this element used PG array double-quote delimiters.
    let mut was_quoted = false;
    for c in inner.chars() {
        if escape {
            cur.push(c);
            escape = false;
            continue;
        }
        match c {
            '\\' if in_quotes => {
                escape = true;
            }
            '"' => {
                in_quotes = !in_quotes;
                was_quoted = true;
            }
            ',' if !in_quotes => {
                elements.push(parse_array_element(&cur, element_oid, was_quoted)?);
                cur.clear();
                was_quoted = false;
            }
            _ => cur.push(c),
        }
    }
    elements.push(parse_array_element(&cur, element_oid, was_quoted)?);

    Ok(PostgresValue::Array(elements))
}

fn parse_array_element(raw: &str, element_oid: u32, was_quoted: bool) -> Result<PostgresValue> {
    let t = raw.trim();
    // Unquoted NULL is SQL null. Quoted "NULL" is the literal string (or typed value).
    if !was_quoted && t.eq_ignore_ascii_case("NULL") {
        return Ok(PostgresValue::Null);
    }
    decode_text_to_postgres_value(t, element_oid)
}

/// Format helper used when a column cannot be converted — preserve as string ElementValue.
pub fn string_element(s: impl AsRef<str>) -> ElementValue {
    ElementValue::String(Arc::from(s.as_ref()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, NaiveDate, Utc};

    #[test]
    fn decode_uuid_text() {
        let pv = decode_text_to_postgres_value("550e8400-e29b-41d4-a716-446655440000", oid::UUID)
            .unwrap();
        match pv {
            PostgresValue::Uuid(u) => {
                assert_eq!(u.to_string(), "550e8400-e29b-41d4-a716-446655440000");
            }
            other => panic!("expected Uuid, got {other:?}"),
        }
    }

    #[test]
    fn decode_date_text() {
        let pv = decode_text_to_postgres_value("2024-06-15", oid::DATE).unwrap();
        assert!(matches!(
            pv,
            PostgresValue::Date(d) if d == NaiveDate::from_ymd_opt(2024, 6, 15).unwrap()
        ));
    }

    #[test]
    fn decode_time_text() {
        let pv = decode_text_to_postgres_value("10:30:45.123456", oid::TIME).unwrap();
        match pv {
            PostgresValue::Time(t) => assert_eq!(t.to_string(), "10:30:45.123456"),
            other => panic!("expected Time, got {other:?}"),
        }
    }

    #[test]
    fn decode_jsonb_text_no_version_byte() {
        let pv = decode_text_to_postgres_value(r#"{"k":1}"#, oid::JSONB).unwrap();
        match pv {
            PostgresValue::Jsonb(v) => assert_eq!(v["k"], 1),
            other => panic!("expected Jsonb, got {other:?}"),
        }
    }

    #[test]
    fn decode_bytea_hex() {
        let pv = decode_text_to_postgres_value(r"\xdeadbeef", oid::BYTEA).unwrap();
        match pv {
            PostgresValue::Bytea(b) => assert_eq!(b, vec![0xde, 0xad, 0xbe, 0xef]),
            other => panic!("expected Bytea, got {other:?}"),
        }
    }

    #[test]
    fn decode_int_array() {
        let pv = decode_text_to_postgres_value("{1,2,3}", oid::INT4_ARRAY).unwrap();
        match pv {
            PostgresValue::Array(items) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(items[0], PostgresValue::Int4(1)));
                assert!(matches!(items[2], PostgresValue::Int4(3)));
            }
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn decode_char_trims_padding() {
        let pv = decode_text_to_postgres_value("abc     ", oid::CHAR).unwrap();
        match pv {
            PostgresValue::Char(s) => assert_eq!(s, "abc"),
            other => panic!("expected Char, got {other:?}"),
        }
    }

    #[test]
    fn decode_timestamp_fractional() {
        let ev = decode_column_value_text("2024-06-15 10:30:45.123456", 1114).unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_micro_opt(10, 30, 45, 123456)
            .unwrap();
        assert_eq!(ev, ElementValue::LocalDateTime(expected));
    }

    #[test]
    fn decode_bool_t_f() {
        assert_eq!(
            decode_column_value_text("t", 16).unwrap(),
            ElementValue::Bool(true)
        );
        assert_eq!(
            decode_column_value_text("f", 16).unwrap(),
            ElementValue::Bool(false)
        );
    }

    #[test]
    fn parity_numeric_whole() {
        let pv = decode_text_to_postgres_value("4200", oid::NUMERIC).unwrap();
        match pv.to_element_value() {
            ElementValue::Float(f) => assert_eq!(f.into_inner(), 4200.0),
            other => panic!("expected Float, got {other:?}"),
        }
    }

    #[test]
    fn decode_text_array_distinguishes_null_and_quoted_null() {
        let pv = decode_text_to_postgres_value(r#"{NULL,"NULL","a"}"#, oid::TEXT_ARRAY).unwrap();
        match pv {
            PostgresValue::Array(items) => {
                assert_eq!(items.len(), 3);
                assert!(matches!(items[0], PostgresValue::Null));
                assert!(matches!(items[1], PostgresValue::Text(ref s) if s == "NULL"));
                assert!(matches!(items[2], PostgresValue::Text(ref s) if s == "a"));
            }
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn decode_int_array_null_element() {
        let pv = decode_text_to_postgres_value("{1,NULL,3}", oid::INT4_ARRAY).unwrap();
        match pv {
            PostgresValue::Array(items) => {
                assert!(matches!(items[0], PostgresValue::Int4(1)));
                assert!(matches!(items[1], PostgresValue::Null));
                assert!(matches!(items[2], PostgresValue::Int4(3)));
            }
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn decode_timestamptz_rfc3339() {
        let pv =
            decode_text_to_postgres_value("2024-06-15T10:30:45+02:00", oid::TIMESTAMPTZ).unwrap();
        match pv {
            PostgresValue::TimestampTz(ts) => {
                assert_eq!(ts.to_rfc3339(), "2024-06-15T08:30:45+00:00");
            }
            other => panic!("expected TimestampTz, got {other:?}"),
        }
    }

    #[test]
    fn decode_timestamptz_postgres_offset() {
        let pv =
            decode_text_to_postgres_value("2024-06-15 10:30:45.123456+02:00", oid::TIMESTAMPTZ)
                .unwrap();
        let expected = DateTime::parse_from_rfc3339("2024-06-15T10:30:45.123456+02:00")
            .unwrap()
            .with_timezone(&Utc);
        match pv {
            PostgresValue::TimestampTz(ts) => {
                assert_eq!(ts, expected);
            }
            other => panic!("expected TimestampTz, got {other:?}"),
        }
    }

    #[test]
    fn decode_timestamptz_without_offset_assumes_utc() {
        let pv = decode_text_to_postgres_value("2024-06-15 10:30:45", oid::TIMESTAMPTZ).unwrap();
        match pv {
            PostgresValue::TimestampTz(ts) => {
                assert_eq!(ts.to_rfc3339(), "2024-06-15T10:30:45+00:00");
            }
            other => panic!("expected TimestampTz, got {other:?}"),
        }
    }

    #[test]
    fn decode_timestamptz_offset_matches_rfc3339_instant() {
        let a =
            decode_text_to_postgres_value("2024-06-15T10:30:45+02:00", oid::TIMESTAMPTZ).unwrap();
        let b =
            decode_text_to_postgres_value("2024-06-15 10:30:45+02:00", oid::TIMESTAMPTZ).unwrap();
        match (a, b) {
            (PostgresValue::TimestampTz(ta), PostgresValue::TimestampTz(tb)) => {
                assert_eq!(ta.timestamp_micros(), tb.timestamp_micros());
            }
            other => panic!("expected TimestampTz pair, got {other:?}"),
        }
    }
}
