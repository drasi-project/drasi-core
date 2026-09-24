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

//! CDC/WAL types for the PostgreSQL source.
//!
//! Value conversion lives in `drasi-postgres-common` so bootstrap and CDC share
//! one canonical `ElementValue` mapping.

use chrono::DateTime;
use chrono::Utc;
// Re-export shared value type used throughout the decoder/stream.
pub use drasi_postgres_common::PostgresValue;
use postgres_types::Oid;

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
    /// The earliest WAL position retained by this slot (`restart_lsn` from pg_replication_slots).
    /// Only populated when querying an existing slot via `get_replication_slot_info`.
    pub restart_lsn: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StandbyStatusUpdate {
    pub write_lsn: u64,
    pub flush_lsn: u64,
    pub apply_lsn: u64,
    pub reply_requested: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, TimeZone, Utc};
    use drasi_core::models::ElementValue;
    use drasi_postgres_common::decode_column_value_text;
    use rust_decimal::Decimal;
    use std::str::FromStr;
    use std::sync::Arc;
    use uuid::Uuid;

    fn sample_naive_datetime() -> chrono::NaiveDateTime {
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
        assert_eq!(pv.to_element_value(), ElementValue::LocalDateTime(ts));
    }

    #[test]
    fn timestamptz_to_element_value_is_zoned_datetime() {
        let ts = sample_utc_datetime();
        let pv = PostgresValue::TimestampTz(ts);
        assert_eq!(
            pv.to_element_value(),
            ElementValue::ZonedDateTime(ts.fixed_offset())
        );
    }

    #[test]
    fn null_to_element_value() {
        let pv = PostgresValue::Null;
        assert_eq!(pv.to_element_value(), ElementValue::Null);
        assert!(pv.is_null());
    }

    #[test]
    fn bool_to_element_value() {
        assert_eq!(
            PostgresValue::Bool(true).to_element_value(),
            ElementValue::Bool(true)
        );
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
    fn array_with_timestamps_to_element_value() {
        let ts = sample_naive_datetime();
        let pv = PostgresValue::Array(vec![PostgresValue::Timestamp(ts), PostgresValue::Int4(42)]);
        match pv.to_element_value() {
            ElementValue::List(items) => {
                assert_eq!(items.len(), 2);
                assert_eq!(items[0], ElementValue::LocalDateTime(ts));
                assert_eq!(items[1], ElementValue::Integer(42));
            }
            other => panic!("Expected List, got {other:?}"),
        }
    }

    #[test]
    fn parity_bool_true() {
        let cdc = PostgresValue::Bool(true).to_element_value();
        let bootstrap = decode_column_value_text("true", 16).unwrap();
        assert_eq!(cdc, bootstrap);
    }

    #[test]
    fn parity_int4() {
        let cdc = PostgresValue::Int4(100_000).to_element_value();
        let bootstrap = decode_column_value_text("100000", 23).unwrap();
        assert_eq!(cdc, bootstrap);
    }

    #[test]
    fn parity_numeric() {
        let dec = Decimal::from_str_exact("123.45").unwrap();
        let cdc = PostgresValue::Numeric(dec).to_element_value();
        let bootstrap = decode_column_value_text("123.45", 1700).unwrap();
        assert_eq!(cdc, bootstrap);
    }

    #[test]
    fn parity_numeric_whole_number_is_float() {
        let dec = Decimal::from_str("4200").unwrap();
        let cdc = PostgresValue::Numeric(dec).to_element_value();
        let bootstrap = decode_column_value_text("4200", 1700).unwrap();
        assert_eq!(cdc, bootstrap);
        assert!(matches!(cdc, ElementValue::Float(_)));
    }

    #[test]
    fn parity_timestamp() {
        let dt = NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_micro_opt(10, 30, 45, 123456)
            .unwrap();
        let cdc = PostgresValue::Timestamp(dt).to_element_value();
        let bootstrap = decode_column_value_text("2024-06-15 10:30:45.123456", 1114).unwrap();
        assert_eq!(cdc, bootstrap);
        assert!(matches!(cdc, ElementValue::LocalDateTime(_)));
    }

    #[test]
    fn parity_timestamptz_utc() {
        let utc_dt = Utc.with_ymd_and_hms(2024, 6, 15, 10, 30, 45).unwrap();
        let cdc = PostgresValue::TimestampTz(utc_dt).to_element_value();
        let bootstrap = decode_column_value_text("2024-06-15T10:30:45+00:00", 1184).unwrap();
        assert_eq!(cdc, bootstrap);
        assert!(matches!(cdc, ElementValue::ZonedDateTime(_)));
    }

    #[test]
    fn parity_date() {
        let cdc =
            PostgresValue::Date(NaiveDate::from_ymd_opt(2024, 6, 15).unwrap()).to_element_value();
        let bootstrap = decode_column_value_text("2024-06-15", 1082).unwrap();
        assert_eq!(cdc, bootstrap);
    }

    #[test]
    fn parity_uuid() {
        let uuid = Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let cdc = PostgresValue::Uuid(uuid).to_element_value();
        let bootstrap =
            decode_column_value_text("550e8400-e29b-41d4-a716-446655440000", 2950).unwrap();
        assert_eq!(cdc, bootstrap);
    }

    #[test]
    fn parity_time() {
        let t = chrono::NaiveTime::from_hms_micro_opt(10, 30, 45, 0).unwrap();
        let cdc = PostgresValue::Time(t).to_element_value();
        let bootstrap = decode_column_value_text("10:30:45", 1083).unwrap();
        assert_eq!(cdc, bootstrap);
    }

    #[test]
    fn parity_bytea() {
        let bytes = vec![0xde, 0xad, 0xbe, 0xef];
        let cdc = PostgresValue::Bytea(bytes).to_element_value();
        let bootstrap = decode_column_value_text(r"\xdeadbeef", 17).unwrap();
        assert_eq!(cdc, bootstrap);
        if let ElementValue::String(s) = cdc {
            assert_eq!(&*s, "3q2+7w=="); // base64 of deadbeef
        } else {
            panic!("expected String");
        }
    }

    #[test]
    fn parity_char_trimmed() {
        let cdc = PostgresValue::Char("abc".to_string()).to_element_value();
        let bootstrap = decode_column_value_text("abc     ", 1042).unwrap();
        assert_eq!(cdc, bootstrap);
        assert_eq!(cdc, ElementValue::String(Arc::from("abc")));
    }

    #[test]
    fn parity_jsonb() {
        let j = serde_json::json!({"k": 1});
        let cdc = PostgresValue::Jsonb(j).to_element_value();
        let bootstrap = decode_column_value_text(r#"{"k":1}"#, 3802).unwrap();
        assert_eq!(cdc, bootstrap);
    }

    #[test]
    fn test_decimal_to_json_as_number() {
        let decimal = Decimal::from_str("123.45").unwrap();
        let json = PostgresValue::Numeric(decimal).to_json();
        assert!(json.is_number());
        assert_eq!(json.as_f64().unwrap(), 123.45);
    }
}
