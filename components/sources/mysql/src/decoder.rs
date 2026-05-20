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
use mysql_common::binlog::{events::TableMapEvent, row::BinlogRow, value::BinlogValue};
use ordered_float::OrderedFloat;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};

use drasi_mysql_common::{format_value_for_key, TableKeyConfig};

pub struct MySqlDecoder {
    source_id: String,
    table_keys: HashMap<String, Vec<String>>,
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
            let col_type = table.get_column_type(idx).ok().flatten();
            let value = self.value_at(row, fallback_row, idx, col_type)?;

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
        col_type: Option<mysql_common::constants::ColumnType>,
    ) -> Result<ElementValue> {
        let value = row
            .as_ref(idx)
            .or_else(|| fallback_row.and_then(|fallback| fallback.as_ref(idx)));

        match value {
            None => Ok(ElementValue::Null),
            Some(value) => binlog_value_to_element_value(value, col_type),
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

fn binlog_value_to_element_value(
    value: &BinlogValue<'_>,
    col_type: Option<mysql_common::constants::ColumnType>,
) -> Result<ElementValue> {
    match value {
        BinlogValue::Value(value) => Ok(mysql_value_to_element_value(value, col_type)),
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

fn mysql_value_to_element_value(
    value: &Value,
    col_type: Option<mysql_common::constants::ColumnType>,
) -> ElementValue {
    use mysql_common::constants::ColumnType;

    match value {
        Value::NULL => ElementValue::Null,
        Value::Bytes(bytes) => {
            // YEAR columns come as Bytes in binlog; parse as integer for consistency
            if col_type == Some(ColumnType::MYSQL_TYPE_YEAR) {
                let text = String::from_utf8_lossy(bytes);
                if let Ok(val) = text.parse::<i64>() {
                    return ElementValue::Integer(val);
                }
            }
            ElementValue::String(Arc::from(String::from_utf8_lossy(bytes).into_owned()))
        }
        Value::Int(val) => ElementValue::Integer(*val),
        Value::UInt(val) => {
            if *val <= i64::MAX as u64 {
                ElementValue::Integer(*val as i64)
            } else {
                ElementValue::String(Arc::from(val.to_string()))
            }
        }
        Value::Float(val) => ElementValue::Float(OrderedFloat(*val as f64)),
        Value::Double(val) => ElementValue::Float(OrderedFloat(*val)),
        Value::Date(y, m, d, h, min, s, _) => ElementValue::String(Arc::from(format!(
            "{y:04}-{m:02}-{d:02} {h:02}:{min:02}:{s:02}"
        ))),
        Value::Time(_, days, hours, minutes, seconds, micros) => {
            let total_hours = days * 24 + u32::from(*hours);
            ElementValue::String(Arc::from(format!(
                "{total_hours:03}:{minutes:02}:{seconds:02}.{micros:06}"
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_async::Value;
    use ordered_float::OrderedFloat;

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
    fn test_date() {
        let v = mysql_value_to_element_value(&Value::Date(2024, 6, 15, 13, 45, 30, 0), None);
        assert_eq!(v, ElementValue::String(Arc::from("2024-06-15 13:45:30")));
    }

    #[test]
    fn test_time() {
        let v = mysql_value_to_element_value(&Value::Time(false, 1, 13, 45, 30, 500), None);
        assert_eq!(v, ElementValue::String(Arc::from("037:45:30.000500")));
    }

    #[test]
    fn test_year_bytes_parsed_as_integer() {
        use mysql_common::constants::ColumnType;
        let v = mysql_value_to_element_value(
            &Value::Bytes(b"2025".to_vec()),
            Some(ColumnType::MYSQL_TYPE_YEAR),
        );
        assert_eq!(v, ElementValue::Integer(2025));
    }
}
