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

//! Decodes MySQL binlog row events into Drasi SourceChange events.

use std::collections::HashMap;
use std::convert::TryFrom;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use mysql_async::Value;
use mysql_common::binlog::events::{OptionalMetadataField, TableMapEvent};
use mysql_common::binlog::row::BinlogRow;
use mysql_common::binlog::value::BinlogValue;
use mysql_common::constants::ColumnType;
use ordered_float::OrderedFloat;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};

use drasi_mysql_common::{
    canonicalize_json_text, enum_label, format_datetime, format_time, format_timestamp_epoch,
    format_value_for_key, parse_timestamp_epoch_text, set_labels, TableKeyConfig,
};

pub struct MySqlDecoder {
    source_id: String,
    table_keys: HashMap<String, Vec<String>>,
}

/// Per-column conversion context derived from a TableMapEvent.
struct ColumnContext {
    col_type: Option<ColumnType>,
    /// Fractional-second precision for temporal types, when known.
    fsp: Option<u8>,
    /// ENUM member labels (declaration order), when this column is ENUM.
    enum_labels: Option<Vec<String>>,
    /// SET member labels (declaration order), when this column is SET.
    set_labels: Option<Vec<String>>,
}

impl MySqlDecoder {
    pub fn new(source_id: impl Into<String>, table_keys: &[TableKeyConfig]) -> Self {
        let mut map = HashMap::new();
        for key in table_keys {
            map.insert(key.table.clone(), key.key_columns.clone());
        }
        Self {
            source_id: source_id.into(),
            table_keys: map,
        }
    }

    pub fn decode_insert(
        &self,
        table: &TableMapEvent<'_>,
        row: &BinlogRow,
    ) -> Result<SourceChange> {
        let (element, _) = self.row_to_element(table, row, None)?;
        Ok(SourceChange::Insert { element })
    }

    pub fn decode_update(
        &self,
        table: &TableMapEvent<'_>,
        before: &BinlogRow,
        after: &BinlogRow,
    ) -> Result<SourceChange> {
        let (element, _) = self.row_to_element(table, after, Some(before))?;
        Ok(SourceChange::Update { element })
    }

    pub fn decode_delete(
        &self,
        table: &TableMapEvent<'_>,
        row: &BinlogRow,
    ) -> Result<SourceChange> {
        let (_, metadata) = self.row_to_element(table, row, None)?;
        Ok(SourceChange::Delete { metadata })
    }

    fn row_to_element(
        &self,
        table: &TableMapEvent<'_>,
        row: &BinlogRow,
        fallback_row: Option<&BinlogRow>,
    ) -> Result<(Element, ElementMetadata)> {
        let table_name = table.table_name().into_owned();
        let label = table_name.clone();

        let mut properties = ElementPropertyMap::new();
        let mut key_parts: Vec<String> = Vec::new();
        let configured_keys = self.table_keys.get(table_name.as_str());
        let column_names = self.extract_column_names(row);
        let column_contexts = build_column_contexts(table, row.len());

        let fallback_key = if configured_keys.is_none() && !column_names.is_empty() {
            column_names
                .iter()
                .find(|name| name.eq_ignore_ascii_case("id"))
                .cloned()
        } else {
            None
        };

        for idx in 0..row.len() {
            let column_key = column_names
                .get(idx)
                .cloned()
                .unwrap_or_else(|| format!("col_{idx}"));
            let ctx = column_contexts.get(idx);
            let value = self.value_at(row, fallback_row, idx, ctx)?;

            if let Some(keys) = configured_keys {
                if keys.contains(&column_key) {
                    key_parts.push(format_value_for_key(&value));
                }
            } else if fallback_key
                .as_ref()
                .is_some_and(|fallback| fallback == &column_key)
            {
                key_parts.push(format_value_for_key(&value));
            }

            properties.insert(&column_key, value);
        }

        if key_parts.is_empty() {
            anyhow::bail!(
                "Cannot construct a deterministic element ID for table '{table_name}': \
                 no key columns configured and no 'id' column found. \
                 Configure key_columns for this table."
            );
        }

        let element_id = format!("{}:{}", table_name, key_parts.join("_"));

        let metadata = ElementMetadata {
            reference: ElementReference::new(&self.source_id, &element_id),
            labels: Arc::from(vec![Arc::from(label)]),
            effective_from: Utc::now().timestamp_millis() as u64,
        };

        let element = Element::Node {
            metadata: metadata.clone(),
            properties,
        };

        Ok((element, metadata))
    }

    fn value_at(
        &self,
        row: &BinlogRow,
        fallback_row: Option<&BinlogRow>,
        idx: usize,
        ctx: Option<&ColumnContext>,
    ) -> Result<ElementValue> {
        let value = row
            .as_ref(idx)
            .or_else(|| fallback_row.and_then(|fallback| fallback.as_ref(idx)));

        match value {
            None => Ok(ElementValue::Null),
            Some(value) => binlog_value_to_element_value(value, ctx),
        }
    }

    fn extract_column_names(&self, row: &BinlogRow) -> Vec<String> {
        row.columns_ref()
            .iter()
            .enumerate()
            .map(|(idx, column)| {
                let name = column.name_str().to_string();
                if name.is_empty() {
                    format!("col_{idx}")
                } else {
                    name
                }
            })
            .collect()
    }
}

/// Build per-column conversion context from TableMapEvent type/metadata + optional ENUM/SET labels.
fn build_column_contexts(table: &TableMapEvent<'_>, column_count: usize) -> Vec<ColumnContext> {
    // Optional metadata lists ENUM/SET definitions in column order among those types only.
    let mut enum_defs: Vec<Vec<String>> = Vec::new();
    let mut set_defs: Vec<Vec<String>> = Vec::new();

    for meta in table.iter_optional_meta() {
        let Ok(field) = meta else {
            continue;
        };
        match field {
            OptionalMetadataField::EnumStrValue(enums) => {
                for entry in enums.iter_values().flatten() {
                    enum_defs.push(
                        entry
                            .values()
                            .iter()
                            .map(|v| v.value().into_owned())
                            .collect(),
                    );
                }
            }
            OptionalMetadataField::SetStrValue(sets) => {
                for entry in sets.iter_values().flatten() {
                    set_defs.push(
                        entry
                            .values()
                            .iter()
                            .map(|v| v.value().into_owned())
                            .collect(),
                    );
                }
            }
            _ => {}
        }
    }

    let mut enum_idx = 0usize;
    let mut set_idx = 0usize;
    let mut contexts = Vec::with_capacity(column_count);

    for idx in 0..column_count {
        let col_type = table.get_column_type(idx).ok().flatten();
        let fsp = temporal_fsp(table, idx, col_type);

        let mut enum_labels = None;
        let mut set_labels_opt = None;

        if matches!(col_type, Some(ColumnType::MYSQL_TYPE_ENUM)) {
            if let Some(labels) = enum_defs.get(enum_idx) {
                enum_labels = Some(labels.clone());
            }
            enum_idx += 1;
        } else if matches!(col_type, Some(ColumnType::MYSQL_TYPE_SET)) {
            if let Some(labels) = set_defs.get(set_idx) {
                set_labels_opt = Some(labels.clone());
            }
            set_idx += 1;
        }

        contexts.push(ColumnContext {
            col_type,
            fsp,
            enum_labels,
            set_labels: set_labels_opt,
        });
    }

    contexts
}

fn temporal_fsp(
    table: &TableMapEvent<'_>,
    col_idx: usize,
    col_type: Option<ColumnType>,
) -> Option<u8> {
    match col_type {
        Some(
            ColumnType::MYSQL_TYPE_TIMESTAMP2
            | ColumnType::MYSQL_TYPE_DATETIME2
            | ColumnType::MYSQL_TYPE_TIME2,
        ) => table
            .get_column_metadata(col_idx)
            .and_then(|meta| meta.first().copied()),
        // Non-*2 temporal types have no fractional seconds.
        Some(
            ColumnType::MYSQL_TYPE_TIMESTAMP
            | ColumnType::MYSQL_TYPE_DATETIME
            | ColumnType::MYSQL_TYPE_TIME
            | ColumnType::MYSQL_TYPE_DATE
            | ColumnType::MYSQL_TYPE_NEWDATE,
        ) => Some(0),
        _ => None,
    }
}

fn binlog_value_to_element_value(
    value: &BinlogValue<'_>,
    ctx: Option<&ColumnContext>,
) -> Result<ElementValue> {
    match value {
        BinlogValue::Value(value) => Ok(mysql_value_to_element_value(value, ctx)),
        BinlogValue::Jsonb(value) => {
            let json = serde_json::Value::try_from(value.clone())
                .context("Failed to convert MySQL JSONB value to JSON")?;
            Ok(ElementValue::String(Arc::from(serde_json::to_string(
                &json,
            )?)))
        }
        BinlogValue::JsonDiff(diff) => Ok(ElementValue::String(Arc::from(format!("{diff:?}")))),
    }
}

fn mysql_value_to_element_value(value: &Value, ctx: Option<&ColumnContext>) -> ElementValue {
    let col_type = ctx.and_then(|c| c.col_type);
    let fsp = ctx.and_then(|c| c.fsp);

    match value {
        Value::NULL => ElementValue::Null,
        Value::Bytes(bytes) => convert_bytes(bytes, col_type, fsp, ctx),
        Value::Int(val) => convert_int(*val, col_type, fsp, ctx),
        Value::UInt(val) => {
            if matches!(col_type, Some(ColumnType::MYSQL_TYPE_ENUM)) {
                let labels = ctx.and_then(|c| c.enum_labels.as_deref()).unwrap_or(&[]);
                return ElementValue::String(Arc::from(enum_label(*val, labels)));
            }
            if *val <= i64::MAX as u64 {
                ElementValue::Integer(*val as i64)
            } else {
                ElementValue::String(Arc::from(val.to_string()))
            }
        }
        Value::Float(val) => ElementValue::Float(OrderedFloat(*val as f64)),
        Value::Double(val) => ElementValue::Float(OrderedFloat(*val)),
        Value::Date(y, m, d, h, min, s, micros) => ElementValue::String(Arc::from(
            format_datetime(*y, *m, *d, *h, *min, *s, *micros, fsp),
        )),
        Value::Time(neg, days, hours, minutes, seconds, micros) => ElementValue::String(Arc::from(
            format_time(*neg, *days, *hours, *minutes, *seconds, *micros, fsp),
        )),
    }
}

fn convert_int(
    val: i64,
    col_type: Option<ColumnType>,
    fsp: Option<u8>,
    ctx: Option<&ColumnContext>,
) -> ElementValue {
    match col_type {
        Some(ColumnType::MYSQL_TYPE_ENUM) => {
            let labels = ctx.and_then(|c| c.enum_labels.as_deref()).unwrap_or(&[]);
            ElementValue::String(Arc::from(enum_label(val as u64, labels)))
        }
        Some(ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2) => {
            ElementValue::String(Arc::from(format_timestamp_epoch(val, 0, fsp)))
        }
        _ => ElementValue::Integer(val),
    }
}

fn convert_bytes(
    bytes: &[u8],
    col_type: Option<ColumnType>,
    fsp: Option<u8>,
    ctx: Option<&ColumnContext>,
) -> ElementValue {
    match col_type {
        // YEAR columns come as Bytes in binlog; parse as integer for consistency
        Some(ColumnType::MYSQL_TYPE_YEAR) => {
            let text = String::from_utf8_lossy(bytes);
            if let Ok(val) = text.parse::<i64>() {
                ElementValue::Integer(val)
            } else {
                ElementValue::String(Arc::from(text.into_owned()))
            }
        }
        Some(ColumnType::MYSQL_TYPE_SET) => {
            let labels = ctx.and_then(|c| c.set_labels.as_deref()).unwrap_or(&[]);
            ElementValue::String(Arc::from(set_labels(bytes, labels)))
        }
        Some(ColumnType::MYSQL_TYPE_TIMESTAMP | ColumnType::MYSQL_TYPE_TIMESTAMP2) => {
            let text = String::from_utf8_lossy(bytes);
            if let Some((secs, micros)) = parse_timestamp_epoch_text(&text) {
                ElementValue::String(Arc::from(format_timestamp_epoch(secs, micros, fsp)))
            } else {
                // Already a formatted datetime, or unparsable — pass through.
                ElementValue::String(Arc::from(text.into_owned()))
            }
        }
        Some(ColumnType::MYSQL_TYPE_JSON) => {
            let text = String::from_utf8_lossy(bytes);
            ElementValue::String(Arc::from(canonicalize_json_text(&text)))
        }
        _ => ElementValue::String(Arc::from(String::from_utf8_lossy(bytes).into_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::Value;
    use ordered_float::OrderedFloat;

    fn ctx(
        col_type: ColumnType,
        fsp: Option<u8>,
        enum_labels: Option<Vec<String>>,
        set_labels: Option<Vec<String>>,
    ) -> ColumnContext {
        ColumnContext {
            col_type: Some(col_type),
            fsp,
            enum_labels,
            set_labels,
        }
    }

    #[test]
    fn test_null() {
        let v = mysql_value_to_element_value(&Value::NULL, None);
        assert_eq!(v, ElementValue::Null);
    }

    #[test]
    fn test_int() {
        let v = mysql_value_to_element_value(&Value::Int(123_456), None);
        assert_eq!(v, ElementValue::Integer(123_456));
    }

    #[test]
    fn test_uint_overflow() {
        let v = mysql_value_to_element_value(&Value::UInt((i64::MAX as u64) + 1), None);
        assert_eq!(
            v,
            ElementValue::String(Arc::from(((i64::MAX as u64) + 1).to_string()))
        );
    }

    #[test]
    fn test_float() {
        let v = mysql_value_to_element_value(&Value::Float(1.23), None);
        assert_eq!(v, ElementValue::Float(OrderedFloat(f64::from(1.23_f32))));
    }

    #[test]
    fn test_double() {
        let v = mysql_value_to_element_value(&Value::Double(1.23456789), None);
        assert_eq!(v, ElementValue::Float(OrderedFloat(1.23456789)));
    }

    #[test]
    fn test_bytes() {
        let v = mysql_value_to_element_value(&Value::Bytes(b"hello".to_vec()), None);
        assert_eq!(v, ElementValue::String(Arc::from("hello")));
    }

    #[test]
    fn test_date_without_fraction() {
        let c = ctx(ColumnType::MYSQL_TYPE_DATETIME, Some(0), None, None);
        let v = mysql_value_to_element_value(&Value::Date(2024, 6, 15, 13, 45, 30, 0), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from("2024-06-15 13:45:30")));
    }

    #[test]
    fn test_date_with_micros() {
        let c = ctx(ColumnType::MYSQL_TYPE_DATETIME2, Some(6), None, None);
        let v =
            mysql_value_to_element_value(&Value::Date(2025, 6, 15, 13, 45, 30, 123456), Some(&c));
        assert_eq!(
            v,
            ElementValue::String(Arc::from("2025-06-15 13:45:30.123456"))
        );
    }

    #[test]
    fn test_time() {
        let v = mysql_value_to_element_value(&Value::Time(false, 1, 13, 45, 30, 500), None);
        assert_eq!(v, ElementValue::String(Arc::from("037:45:30.000500")));
    }

    #[test]
    fn test_year_bytes_parsed_as_integer() {
        let c = ctx(ColumnType::MYSQL_TYPE_YEAR, None, None, None);
        let v = mysql_value_to_element_value(&Value::Bytes(b"2025".to_vec()), Some(&c));
        assert_eq!(v, ElementValue::Integer(2025));
    }

    #[test]
    fn test_enum_ordinal_to_label() {
        let c = ctx(
            ColumnType::MYSQL_TYPE_ENUM,
            None,
            Some(vec!["red".into(), "green".into(), "blue".into()]),
            None,
        );
        let v = mysql_value_to_element_value(&Value::Int(2), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from("green")));
    }

    #[test]
    fn test_set_bitmask_to_labels() {
        let c = ctx(
            ColumnType::MYSQL_TYPE_SET,
            None,
            None,
            Some(vec!["a".into(), "b".into(), "c".into()]),
        );
        // 0b101 = a,c
        let v = mysql_value_to_element_value(&Value::Bytes(vec![0b101]), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from("a,c")));
    }

    #[test]
    fn test_timestamp_epoch_int() {
        let c = ctx(ColumnType::MYSQL_TYPE_TIMESTAMP, Some(0), None, None);
        // 2025-06-15 13:45:30 UTC
        let secs = chrono::DateTime::parse_from_rfc3339("2025-06-15T13:45:30Z")
            .unwrap()
            .timestamp();
        let v = mysql_value_to_element_value(&Value::Int(secs), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from("2025-06-15 13:45:30")));
    }

    #[test]
    fn test_timestamp2_epoch_bytes() {
        let c = ctx(ColumnType::MYSQL_TYPE_TIMESTAMP2, Some(6), None, None);
        let secs = chrono::DateTime::parse_from_rfc3339("2025-06-15T13:45:30Z")
            .unwrap()
            .timestamp();
        let payload = format!("{secs}.123456");
        let v = mysql_value_to_element_value(&Value::Bytes(payload.into_bytes()), Some(&c));
        assert_eq!(
            v,
            ElementValue::String(Arc::from("2025-06-15 13:45:30.123456"))
        );
    }

    #[test]
    fn test_json_bytes_canonicalized() {
        let c = ctx(ColumnType::MYSQL_TYPE_JSON, None, None, None);
        let v = mysql_value_to_element_value(&Value::Bytes(br#"{"k": 1}"#.to_vec()), Some(&c));
        assert_eq!(v, ElementValue::String(Arc::from(r#"{"k":1}"#)));
    }
}
