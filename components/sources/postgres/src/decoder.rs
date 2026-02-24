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

use anyhow::{anyhow, Result};
use byteorder::{BigEndian, ReadBytesExt};
use chrono::{DateTime, NaiveDateTime, Utc};
use log::{debug, warn};
use postgres_types::Oid;
use rust_decimal::Decimal;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;
use uuid::Uuid;

use super::types::{
    ColumnInfo, PostgresValue, RelationInfo, ReplicaIdentity, TransactionInfo, WalMessage,
};

#[allow(dead_code)]
const PGOUTPUT_VERSION: u32 = 1;

pub struct PgOutputDecoder {
    relations: HashMap<u32, RelationInfo>,
    current_transaction: Option<TransactionInfo>,
}

impl Default for PgOutputDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl PgOutputDecoder {
    pub fn new() -> Self {
        Self {
            relations: HashMap::new(),
            current_transaction: None,
        }
    }

    pub fn decode_message(&mut self, data: &[u8]) -> Result<Option<WalMessage>> {
        if data.is_empty() {
            return Ok(None);
        }

        let msg_type = data[0];
        let payload = &data[1..];

        match msg_type {
            b'B' => self.decode_begin(payload),
            b'C' => self.decode_commit(payload),
            b'O' => self.decode_origin(payload),
            b'R' => self.decode_relation(payload),
            b'Y' => self.decode_type(payload),
            b'I' => self.decode_insert(payload),
            b'U' => self.decode_update(payload),
            b'D' => self.decode_delete(payload),
            b'T' => self.decode_truncate(payload),
            b'M' => self.decode_message_logical(payload),
            _ => {
                warn!("Unknown pgoutput message type: 0x{msg_type:02x}");
                Ok(None)
            }
        }
    }

    fn decode_begin(&mut self, data: &[u8]) -> Result<Option<WalMessage>> {
        let mut cursor = Cursor::new(data);

        let final_lsn = cursor.read_u64::<BigEndian>()?;
        let commit_timestamp = cursor.read_i64::<BigEndian>()?;
        let xid = cursor.read_u32::<BigEndian>()?;

        // Convert PostgreSQL timestamp to DateTime
        let timestamp = postgres_epoch_to_datetime(commit_timestamp)?;

        let transaction = TransactionInfo {
            xid,
            commit_lsn: final_lsn,
            commit_timestamp: timestamp,
        };

        self.current_transaction = Some(transaction.clone());

        debug!("Begin transaction: xid={xid}, lsn={final_lsn:x}");
        Ok(Some(WalMessage::Begin(transaction)))
    }

    fn decode_commit(&mut self, data: &[u8]) -> Result<Option<WalMessage>> {
        let mut cursor = Cursor::new(data);

        let _flags = cursor.read_u8()?;
        let commit_lsn = cursor.read_u64::<BigEndian>()?;
        let _end_lsn = cursor.read_u64::<BigEndian>()?;
        let commit_timestamp = cursor.read_i64::<BigEndian>()?;

        let timestamp = postgres_epoch_to_datetime(commit_timestamp)?;

        let transaction = TransactionInfo {
            xid: self
                .current_transaction
                .as_ref()
                .map(|t| t.xid)
                .unwrap_or(0),
            commit_lsn,
            commit_timestamp: timestamp,
        };

        self.current_transaction = None;

        debug!("Commit transaction: lsn={commit_lsn:x}");
        Ok(Some(WalMessage::Commit(transaction)))
    }

    fn decode_origin(&mut self, _data: &[u8]) -> Result<Option<WalMessage>> {
        // Origin messages are informational, we can skip them for now
        Ok(None)
    }

    fn decode_relation(&mut self, data: &[u8]) -> Result<Option<WalMessage>> {
        let mut cursor = Cursor::new(data);

        let relation_id = cursor.read_u32::<BigEndian>()?;
        let namespace = read_cstring(&mut cursor)?;
        let name = read_cstring(&mut cursor)?;
        let replica_identity = match cursor.read_u8()? {
            b'd' => ReplicaIdentity::Default,
            b'n' => ReplicaIdentity::Nothing,
            b'f' => ReplicaIdentity::Full,
            b'i' => ReplicaIdentity::Index,
            other => {
                warn!("Unknown replica identity: {}", other as char);
                ReplicaIdentity::Default
            }
        };

        let column_count = cursor.read_u16::<BigEndian>()?;
        let mut columns = Vec::with_capacity(column_count as usize);

        for _ in 0..column_count {
            let flags = cursor.read_u8()?;
            let is_key = (flags & 1) != 0;
            let column_name = read_cstring(&mut cursor)?;
            let type_oid = cursor.read_u32::<BigEndian>()?;
            let type_modifier = cursor.read_i32::<BigEndian>()?;

            columns.push(ColumnInfo {
                name: column_name,
                type_oid: Oid::from(type_oid),
                type_modifier,
                is_key,
            });
        }

        let relation = RelationInfo {
            id: relation_id,
            namespace,
            name: name.clone(),
            replica_identity,
            columns,
        };

        debug!(
            "Relation: id={}, namespace={}, name={}, columns={}",
            relation_id,
            relation.namespace,
            name,
            relation.columns.len()
        );

        self.relations.insert(relation_id, relation.clone());
        Ok(Some(WalMessage::Relation(relation)))
    }

    fn decode_type(&mut self, _data: &[u8]) -> Result<Option<WalMessage>> {
        // Type messages describe custom types, we'll handle them later if needed
        Ok(None)
    }

    fn decode_insert(&mut self, data: &[u8]) -> Result<Option<WalMessage>> {
        debug!("Decoding insert message, data length: {}", data.len());
        let mut cursor = Cursor::new(data);

        let relation_id = cursor.read_u32::<BigEndian>()?;
        let tuple_type = cursor.read_u8()?;

        debug!(
            "Insert: relation_id={}, tuple_type={}",
            relation_id, tuple_type as char
        );

        if tuple_type != b'N' {
            return Err(anyhow!(
                "Expected 'N' tuple type for insert, got: {tuple_type}"
            ));
        }

        let relation = self
            .relations
            .get(&relation_id)
            .ok_or_else(|| anyhow!("Unknown relation ID: {relation_id}"))?;

        debug!(
            "Insert for table: {}, expected columns: {}",
            relation.name,
            relation.columns.len()
        );
        let tuple = self.decode_tuple_data(&mut cursor, &relation.columns)?;

        debug!(
            "Insert: relation={}, columns={}",
            relation.name,
            tuple.len()
        );
        Ok(Some(WalMessage::Insert { relation_id, tuple }))
    }

    fn decode_update(&mut self, data: &[u8]) -> Result<Option<WalMessage>> {
        let mut cursor = Cursor::new(data);

        let relation_id = cursor.read_u32::<BigEndian>()?;
        let tuple_type = cursor.read_u8()?;

        let relation = self
            .relations
            .get(&relation_id)
            .ok_or_else(|| anyhow!("Unknown relation ID: {relation_id}"))?;

        let mut old_tuple = None;
        let new_tuple;

        match tuple_type {
            b'K' | b'O' => {
                // Has old tuple (key or old)
                old_tuple = Some(self.decode_tuple_data(&mut cursor, &relation.columns)?);
                let next_type = cursor.read_u8()?;
                if next_type != b'N' {
                    return Err(anyhow!("Expected 'N' after old tuple, got: {next_type}"));
                }
                new_tuple = self.decode_tuple_data(&mut cursor, &relation.columns)?;
            }
            b'N' => {
                // Only new tuple
                new_tuple = self.decode_tuple_data(&mut cursor, &relation.columns)?;
            }
            _ => {
                return Err(anyhow!("Unknown tuple type for update: {tuple_type}"));
            }
        }

        debug!(
            "Update: relation={}, has_old={}",
            relation.name,
            old_tuple.is_some()
        );
        Ok(Some(WalMessage::Update {
            relation_id,
            old_tuple,
            new_tuple,
        }))
    }

    fn decode_delete(&mut self, data: &[u8]) -> Result<Option<WalMessage>> {
        let mut cursor = Cursor::new(data);

        let relation_id = cursor.read_u32::<BigEndian>()?;
        let tuple_type = cursor.read_u8()?;

        if tuple_type != b'K' && tuple_type != b'O' {
            return Err(anyhow!(
                "Expected 'K' or 'O' tuple type for delete, got: {tuple_type}"
            ));
        }

        let relation = self
            .relations
            .get(&relation_id)
            .ok_or_else(|| anyhow!("Unknown relation ID: {relation_id}"))?;

        let old_tuple = self.decode_tuple_data(&mut cursor, &relation.columns)?;

        debug!("Delete: relation={}", relation.name);
        Ok(Some(WalMessage::Delete {
            relation_id,
            old_tuple,
        }))
    }

    fn decode_truncate(&mut self, data: &[u8]) -> Result<Option<WalMessage>> {
        let mut cursor = Cursor::new(data);

        let relation_count = cursor.read_u32::<BigEndian>()?;
        let _options = cursor.read_u8()?;

        let mut relation_ids = Vec::with_capacity(relation_count as usize);
        for _ in 0..relation_count {
            relation_ids.push(cursor.read_u32::<BigEndian>()?);
        }

        debug!("Truncate: {relation_count} relations");
        Ok(Some(WalMessage::Truncate { relation_ids }))
    }

    fn decode_message_logical(&mut self, _data: &[u8]) -> Result<Option<WalMessage>> {
        // Logical messages are application-specific, skip for now
        Ok(None)
    }

    fn decode_tuple_data(
        &self,
        cursor: &mut Cursor<&[u8]>,
        columns: &[ColumnInfo],
    ) -> Result<Vec<PostgresValue>> {
        let start_pos = cursor.position();
        let total_len = cursor.get_ref().len();
        debug!("decode_tuple_data: start position={start_pos}, total buffer length={total_len}");

        let column_count = cursor.read_u16::<BigEndian>()? as usize;
        debug!("Decoding {column_count} columns");

        if column_count != columns.len() {
            warn!(
                "Column count mismatch: expected {}, got {}",
                columns.len(),
                column_count
            );
        }

        let mut values = Vec::with_capacity(column_count);

        for i in 0..column_count {
            let column = columns
                .get(i)
                .ok_or_else(|| anyhow!("Column index out of bounds: {i}"))?;

            let tuple_type = cursor.read_u8()?;
            debug!(
                "Column {}: type={} ({}), oid={}",
                i, tuple_type as char, tuple_type, column.type_oid
            );

            let value = match tuple_type {
                b'n' => PostgresValue::Null,
                b'u' => PostgresValue::Null, // Unchanged TOAST value
                b't' => {
                    let length = cursor.read_u32::<BigEndian>()? as usize;
                    // Ensure we have enough data to read
                    let pos = cursor.position() as usize;
                    let available = cursor.get_ref().len() - pos;
                    debug!(
                        "Column {i} text value: length={length}, pos={pos}, available={available}"
                    );
                    if available < length {
                        return Err(anyhow!("Not enough data for column {} ({}): need {} bytes, have {} bytes at position {}", 
                            i, column.name, length, available, pos));
                    }
                    let mut data = vec![0u8; length];
                    cursor.read_exact(&mut data).map_err(|e| {
                        anyhow!(
                            "Failed to read {} bytes for column {} ({}): {}",
                            length,
                            i,
                            column.name,
                            e
                        )
                    })?;
                    debug!(
                        "Successfully read {} bytes for column {}, decoding type OID {}",
                        length, i, column.type_oid
                    );
                    self.decode_column_value(&data, column.type_oid)?
                }
                _ => {
                    return Err(anyhow!(
                        "Unknown tuple data type: {} ({})",
                        tuple_type as char,
                        tuple_type
                    ));
                }
            };

            values.push(value);
        }

        Ok(values)
    }

    fn decode_column_value(&self, data: &[u8], type_oid: Oid) -> Result<PostgresValue> {
        // Map common PostgreSQL type OIDs to decoders
        let oid_value = type_oid;
        match oid_value {
            16 => {
                // bool
                Ok(PostgresValue::Bool(data[0] != 0))
            }
            21 => {
                // int2
                if data.len() == 2 {
                    // Binary format
                    let mut cursor = Cursor::new(data);
                    Ok(PostgresValue::Int2(cursor.read_i16::<BigEndian>()?))
                } else {
                    // Text format
                    let text = String::from_utf8_lossy(data);
                    let value = text
                        .trim()
                        .parse::<i16>()
                        .map_err(|e| anyhow!("Failed to parse int2 from '{text}': {e}"))?;
                    Ok(PostgresValue::Int2(value))
                }
            }
            23 => {
                // int4
                if data.len() == 4 {
                    // Binary format
                    let mut cursor = Cursor::new(data);
                    Ok(PostgresValue::Int4(cursor.read_i32::<BigEndian>()?))
                } else {
                    // Text format - parse as string
                    let text = String::from_utf8_lossy(data);
                    let value = text
                        .trim()
                        .parse::<i32>()
                        .map_err(|e| anyhow!("Failed to parse int4 from '{text}': {e}"))?;
                    Ok(PostgresValue::Int4(value))
                }
            }
            20 => {
                // int8
                if data.len() == 8 {
                    // Binary format
                    let mut cursor = Cursor::new(data);
                    Ok(PostgresValue::Int8(cursor.read_i64::<BigEndian>()?))
                } else {
                    // Text format
                    let text = String::from_utf8_lossy(data);
                    let value = text
                        .trim()
                        .parse::<i64>()
                        .map_err(|e| anyhow!("Failed to parse int8 from '{text}': {e}"))?;
                    Ok(PostgresValue::Int8(value))
                }
            }
            700 => {
                // float4
                if data.len() == 4 {
                    // Binary format
                    let mut cursor = Cursor::new(data);
                    Ok(PostgresValue::Float4(cursor.read_f32::<BigEndian>()?))
                } else {
                    // Text format
                    let text = String::from_utf8_lossy(data);
                    let value = text
                        .trim()
                        .parse::<f32>()
                        .map_err(|e| anyhow!("Failed to parse float4 from '{text}': {e}"))?;
                    Ok(PostgresValue::Float4(value))
                }
            }
            701 => {
                // float8
                if data.len() == 8 {
                    // Binary format
                    let mut cursor = Cursor::new(data);
                    Ok(PostgresValue::Float8(cursor.read_f64::<BigEndian>()?))
                } else {
                    // Text format
                    let text = String::from_utf8_lossy(data);
                    let value = text
                        .trim()
                        .parse::<f64>()
                        .map_err(|e| anyhow!("Failed to parse float8 from '{text}': {e}"))?;
                    Ok(PostgresValue::Float8(value))
                }
            }
            1700 => {
                // numeric
                // Check if it's text format (pgoutput default) or binary format
                if data.len() < 8
                    || (!data.is_empty() && data[0] >= b'0' && data[0] <= b'9')
                    || (!data.is_empty() && (data[0] == b'-' || data[0] == b'+' || data[0] == b'.'))
                {
                    // Text format - parse as string
                    let text = String::from_utf8_lossy(data);
                    let value = Decimal::from_str_exact(text.trim())
                        .map_err(|e| anyhow!("Failed to parse numeric from '{text}': {e}"))?;
                    Ok(PostgresValue::Numeric(value))
                } else {
                    // Binary format
                    Ok(PostgresValue::Numeric(decode_numeric(data)?))
                }
            }
            25 | 1043 | 19 => {
                // text, varchar, name
                Ok(PostgresValue::Text(
                    String::from_utf8_lossy(data).to_string(),
                ))
            }
            1042 => {
                // char/bpchar
                let s = String::from_utf8_lossy(data).trim_end().to_string();
                Ok(PostgresValue::Char(s))
            }
            2950 => {
                // uuid
                if data.len() != 16 {
                    return Err(anyhow!("Invalid UUID length: {}", data.len()));
                }
                let uuid = Uuid::from_slice(data)?;
                Ok(PostgresValue::Uuid(uuid))
            }
            1114 => {
                // timestamp
                if data.len() == 8 {
                    // Binary format
                    let mut cursor = Cursor::new(data);
                    let micros = cursor.read_i64::<BigEndian>()?;
                    let timestamp = postgres_epoch_to_naive_datetime(micros)?;
                    Ok(PostgresValue::Timestamp(timestamp))
                } else {
                    // Text format - parse PostgreSQL timestamp string
                    let text = String::from_utf8_lossy(data);
                    let timestamp =
                        NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S%.f")
                            .or_else(|_| {
                                NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S")
                            })
                            .map_err(|e| anyhow!("Failed to parse timestamp from '{text}': {e}"))?;
                    Ok(PostgresValue::Timestamp(timestamp))
                }
            }
            1184 => {
                // timestamptz
                if data.len() == 8 {
                    // Binary format
                    let mut cursor = Cursor::new(data);
                    let micros = cursor.read_i64::<BigEndian>()?;
                    let timestamp = postgres_epoch_to_datetime(micros)?;
                    Ok(PostgresValue::TimestampTz(timestamp))
                } else {
                    // Text format - parse PostgreSQL timestamptz string
                    let text = String::from_utf8_lossy(data);
                    // PostgreSQL sends timestamptz in ISO 8601 format
                    let timestamp = DateTime::parse_from_rfc3339(text.trim())
                        .or_else(|_| {
                            DateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S%.f%z")
                        })
                        .map_err(|e| anyhow!("Failed to parse timestamptz from '{text}': {e}"))?
                        .with_timezone(&Utc);
                    Ok(PostgresValue::TimestampTz(timestamp))
                }
            }
            1082 => {
                // date
                let mut cursor = Cursor::new(data);
                let days = cursor.read_i32::<BigEndian>()?;
                let date = postgres_epoch_to_date(days)?;
                Ok(PostgresValue::Date(date))
            }
            1083 => {
                // time
                let mut cursor = Cursor::new(data);
                let micros = cursor.read_i64::<BigEndian>()?;
                let time = postgres_time_to_naive_time(micros)?;
                Ok(PostgresValue::Time(time))
            }
            114 | 3802 => {
                // json, jsonb
                let json_str = if oid_value == 3802 {
                    // jsonb has a version byte
                    String::from_utf8_lossy(&data[1..]).to_string()
                } else {
                    String::from_utf8_lossy(data).to_string()
                };
                let value: JsonValue = serde_json::from_str(&json_str)?;
                Ok(PostgresValue::Json(value))
            }
            17 => {
                // bytea
                Ok(PostgresValue::Bytea(data.to_vec()))
            }
            _ => {
                // Default to text representation for unknown types
                warn!("Unknown type OID {oid_value}, treating as text");
                Ok(PostgresValue::Text(
                    String::from_utf8_lossy(data).to_string(),
                ))
            }
        }
    }

    pub fn get_relation(&self, relation_id: u32) -> Option<&RelationInfo> {
        self.relations.get(&relation_id)
    }
}

fn read_cstring(cursor: &mut Cursor<&[u8]>) -> Result<String> {
    let mut buffer = Vec::new();
    loop {
        let byte = cursor.read_u8()?;
        if byte == 0 {
            break;
        }
        buffer.push(byte);
    }
    Ok(String::from_utf8_lossy(&buffer).to_string())
}

fn postgres_epoch_to_datetime(micros: i64) -> Result<DateTime<Utc>> {
    // PostgreSQL epoch is 2000-01-01 00:00:00
    const POSTGRES_EPOCH: i64 = 946684800000000; // microseconds since Unix epoch
    let unix_micros = micros + POSTGRES_EPOCH;
    let secs = unix_micros / 1_000_000;
    let nanos = ((unix_micros % 1_000_000) * 1000) as u32;

    DateTime::from_timestamp(secs, nanos).ok_or_else(|| anyhow!("Invalid timestamp"))
}

fn postgres_epoch_to_naive_datetime(micros: i64) -> Result<NaiveDateTime> {
    let dt = postgres_epoch_to_datetime(micros)?;
    Ok(dt.naive_utc())
}

fn postgres_epoch_to_date(days: i32) -> Result<chrono::NaiveDate> {
    // PostgreSQL date epoch is 2000-01-01
    const POSTGRES_DATE_EPOCH: i32 = 10957; // days since Unix epoch (1970-01-01)
    let unix_days = days + POSTGRES_DATE_EPOCH;

    let epoch =
        chrono::NaiveDate::from_ymd_opt(1970, 1, 1).ok_or_else(|| anyhow!("Invalid epoch date"))?;

    Ok(epoch + chrono::Duration::days(unix_days as i64))
}

fn postgres_time_to_naive_time(micros: i64) -> Result<chrono::NaiveTime> {
    let total_secs = micros / 1_000_000;
    let hours = (total_secs / 3600) as u32;
    let minutes = ((total_secs % 3600) / 60) as u32;
    let seconds = (total_secs % 60) as u32;
    let nanos = ((micros % 1_000_000) * 1000) as u32;

    chrono::NaiveTime::from_hms_nano_opt(hours, minutes, seconds, nanos)
        .ok_or_else(|| anyhow!("Invalid time"))
}

/// Decode a column value from text format (used by bootstrap)
pub fn decode_column_value_text(
    text: &str,
    type_oid: i32,
) -> Result<drasi_core::models::ElementValue> {
    use drasi_core::models::ElementValue;

    match type_oid as u32 {
        16 => {
            // bool
            let value = text.parse::<bool>().or_else(|_| match text {
                "t" => Ok(true),
                "f" => Ok(false),
                _ => Err(anyhow!("Invalid boolean value")),
            })?;
            Ok(ElementValue::Bool(value))
        }
        21 => {
            // int2
            let value = text.parse::<i16>()?;
            Ok(ElementValue::Integer(value as i64))
        }
        23 => {
            // int4
            let value = text.parse::<i32>()?;
            Ok(ElementValue::Integer(value as i64))
        }
        20 => {
            // int8
            let value = text.parse::<i64>()?;
            Ok(ElementValue::Integer(value))
        }
        700 => {
            // float4
            let value = text.parse::<f32>()?;
            Ok(ElementValue::Float(ordered_float::OrderedFloat(
                value as f64,
            )))
        }
        701 => {
            // float8
            let value = text.parse::<f64>()?;
            Ok(ElementValue::Float(ordered_float::OrderedFloat(value)))
        }
        1700 => {
            // numeric/decimal
            let value = text.parse::<f64>()?;
            Ok(ElementValue::Float(ordered_float::OrderedFloat(value)))
        }
        25 | 1043 | 19 => {
            // text, varchar, name
            Ok(ElementValue::String(Arc::from(text)))
        }
        1114 => {
            // timestamp (without timezone)
            // Try parsing as NaiveDateTime first, then fall back to zoned formats
            if let Ok(dt) = NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S%.f") {
                Ok(ElementValue::LocalDateTime(dt))
            } else if let Ok(dt) = NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S") {
                Ok(ElementValue::LocalDateTime(dt))
            } else if let Ok(dt) = DateTime::parse_from_rfc3339(text.trim()) {
                Ok(ElementValue::ZonedDateTime(dt.fixed_offset()))
            } else if let Ok(dt) = DateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S%.f%z") {
                Ok(ElementValue::ZonedDateTime(dt.fixed_offset()))
            } else {
                // Fall back to string if parsing fails
                Ok(ElementValue::String(Arc::from(text)))
            }
        }
        1184 => {
            // timestamptz — always produce ZonedDateTime to match CDC binary path.
            // Try offset-aware formats first, then treat offset-less text as UTC.
            if let Ok(dt) = DateTime::parse_from_rfc3339(text.trim()) {
                Ok(ElementValue::ZonedDateTime(dt.fixed_offset()))
            } else if let Ok(dt) = DateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S%.f%z") {
                Ok(ElementValue::ZonedDateTime(dt.fixed_offset()))
            } else if let Ok(dt) = NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S%.f") {
                // No offset in text but OID says timestamptz — assume UTC
                Ok(ElementValue::ZonedDateTime(dt.and_utc().fixed_offset()))
            } else if let Ok(dt) = NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%d %H:%M:%S") {
                // No offset in text but OID says timestamptz — assume UTC
                Ok(ElementValue::ZonedDateTime(dt.and_utc().fixed_offset()))
            } else {
                // Fall back to string if parsing fails
                Ok(ElementValue::String(Arc::from(text)))
            }
        }
        1082 => {
            // date
            Ok(ElementValue::String(Arc::from(text)))
        }
        2950 => {
            // uuid
            Ok(ElementValue::String(Arc::from(text)))
        }
        _ => {
            // Default to string for unknown types
            Ok(ElementValue::String(Arc::from(text)))
        }
    }
}

fn decode_numeric(data: &[u8]) -> Result<Decimal> {
    if data.len() < 8 {
        return Err(anyhow!("Numeric data too short"));
    }

    let mut cursor = Cursor::new(data);
    let ndigits = cursor.read_u16::<BigEndian>()?;
    let weight = cursor.read_i16::<BigEndian>()?;
    let sign = cursor.read_u16::<BigEndian>()?;
    let dscale = cursor.read_u16::<BigEndian>()?;

    if sign == 0xC000 {
        // NaN
        return Ok(Decimal::ZERO);
    }

    let mut digits = Vec::with_capacity(ndigits as usize);
    for _ in 0..ndigits {
        digits.push(cursor.read_u16::<BigEndian>()?);
    }

    // Convert PostgreSQL numeric to Decimal
    // This is a simplified implementation
    let mut result = Decimal::ZERO;
    let base = Decimal::from(10000);

    for (i, &digit) in digits.iter().enumerate() {
        let power = weight as i32 - i as i32;
        let digit_value = Decimal::from(digit as i64);
        let multiplier = if power >= 0 {
            let mut result = Decimal::ONE;
            for _ in 0..power {
                result *= base;
            }
            result
        } else {
            let mut result = Decimal::ONE;
            for _ in 0..(-power) {
                result /= base;
            }
            result
        };
        result += digit_value * multiplier;
    }

    if sign == 0x4000 {
        result = -result;
    }

    // Apply scale
    if dscale > 0 {
        let mut scale_divisor = Decimal::ONE;
        for _ in 0..dscale {
            scale_divisor *= Decimal::from(10);
        }
        result /= scale_divisor;
    }

    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use drasi_core::models::ElementValue;

    // ── decode_column_value_text: timestamp (OID 1114) ─────────────────

    #[test]
    fn decode_timestamp_with_fractional_seconds() {
        let ev = decode_column_value_text("2024-06-15 10:30:45.123456", 1114).unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_micro_opt(10, 30, 45, 123456)
            .unwrap();
        assert_eq!(ev, ElementValue::LocalDateTime(expected));
    }

    #[test]
    fn decode_timestamp_without_fractional_seconds() {
        let ev = decode_column_value_text("2024-06-15 10:30:45", 1114).unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_opt(10, 30, 45)
            .unwrap();
        assert_eq!(ev, ElementValue::LocalDateTime(expected));
    }

    #[test]
    fn decode_timestamp_with_leading_trailing_whitespace() {
        let ev = decode_column_value_text("  2024-06-15 10:30:45  ", 1114).unwrap();
        let expected = NaiveDate::from_ymd_opt(2024, 6, 15)
            .unwrap()
            .and_hms_opt(10, 30, 45)
            .unwrap();
        assert_eq!(ev, ElementValue::LocalDateTime(expected));
    }

    // ── decode_column_value_text: timestamptz (OID 1184) ───────────────

    #[test]
    fn decode_timestamptz_rfc3339() {
        let ev = decode_column_value_text("2024-06-15T10:30:45+01:00", 1184).unwrap();
        match ev {
            ElementValue::ZonedDateTime(dt) => {
                assert_eq!(dt.offset().local_minus_utc(), 3600);
                assert_eq!(dt.naive_local().to_string(), "2024-06-15 10:30:45");
            }
            other => panic!("Expected ZonedDateTime, got {other:?}"),
        }
    }

    #[test]
    fn decode_timestamptz_with_offset_format() {
        let ev = decode_column_value_text("2024-06-15 10:30:45.123456+0200", 1184).unwrap();
        match ev {
            ElementValue::ZonedDateTime(dt) => {
                assert_eq!(dt.offset().local_minus_utc(), 7200); // +02:00
            }
            other => panic!("Expected ZonedDateTime, got {other:?}"),
        }
    }

    #[test]
    fn decode_timestamptz_utc_via_rfc3339() {
        let ev = decode_column_value_text("2024-06-15T10:30:45+00:00", 1184).unwrap();
        match ev {
            ElementValue::ZonedDateTime(dt) => {
                assert_eq!(dt.offset().local_minus_utc(), 0);
            }
            other => panic!("Expected ZonedDateTime, got {other:?}"),
        }
    }

    // ── decode_column_value_text: timestamp OID with ambiguous input ───

    #[test]
    fn decode_timestamptz_oid_with_plain_datetime_string_assumes_utc() {
        // OID 1184 (timestamptz) but the string has no tz info → ZonedDateTime (UTC)
        // This matches the CDC binary path which always produces ZonedDateTime for OID 1184
        let ev = decode_column_value_text("2024-06-15 10:30:45", 1184).unwrap();
        match ev {
            ElementValue::ZonedDateTime(dt) => {
                assert_eq!(dt.offset().local_minus_utc(), 0, "Expected UTC offset");
                assert_eq!(dt.naive_local().to_string(), "2024-06-15 10:30:45");
            }
            other => panic!("Expected ZonedDateTime for timestamptz OID with no tz info, got {other:?}"),
        }
    }

    #[test]
    fn decode_timestamptz_oid_with_fractional_no_offset_assumes_utc() {
        // OID 1184 with fractional seconds but no offset → ZonedDateTime (UTC)
        let ev = decode_column_value_text("2024-06-15 10:30:45.123456", 1184).unwrap();
        match ev {
            ElementValue::ZonedDateTime(dt) => {
                assert_eq!(dt.offset().local_minus_utc(), 0, "Expected UTC offset");
            }
            other => panic!("Expected ZonedDateTime, got {other:?}"),
        }
    }

    #[test]
    fn decode_timestamp_oid_with_rfc3339_string() {
        // OID 1114 (timestamp) but the string has tz info → ZonedDateTime
        let ev = decode_column_value_text("2024-06-15T10:30:45+05:00", 1114).unwrap();
        assert!(
            matches!(ev, ElementValue::ZonedDateTime(_)),
            "Expected ZonedDateTime for rfc3339 input on timestamp OID, got {ev:?}"
        );
    }

    #[test]
    fn decode_unparseable_timestamp_falls_back_to_string() {
        let ev = decode_column_value_text("not-a-date", 1114).unwrap();
        assert!(
            matches!(ev, ElementValue::String(_)),
            "Expected String fallback, got {ev:?}"
        );
    }

    // ── decode_column_value_text: non-timestamp types still work ───────

    #[test]
    fn decode_bool_text() {
        assert_eq!(
            decode_column_value_text("true", 16).unwrap(),
            ElementValue::Bool(true)
        );
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
    fn decode_int4_text() {
        assert_eq!(
            decode_column_value_text("42", 23).unwrap(),
            ElementValue::Integer(42)
        );
    }

    #[test]
    fn decode_text_type() {
        assert_eq!(
            decode_column_value_text("hello world", 25).unwrap(),
            ElementValue::String(Arc::from("hello world"))
        );
    }
}
