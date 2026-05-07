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
use std::sync::Arc;

use anyhow::Result;
use base64::Engine;
use chrono::Utc;
use log::warn;
use mysql_cdc::events::row_events::mysql_value::MySqlValue;
use mysql_cdc::events::row_events::row_data::{RowData, UpdateRowData};
use mysql_cdc::events::table_map_event::TableMapEvent;
use ordered_float::OrderedFloat;

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};

use crate::config::TableKeyConfig;

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

    pub fn decode_insert(&self, table: &TableMapEvent, row: &RowData) -> Result<SourceChange> {
        let (element, _) = self.row_to_element(table, &row.cells)?;
        Ok(SourceChange::Insert { element })
    }

    pub fn decode_update(
        &self,
        table: &TableMapEvent,
        row: &UpdateRowData,
    ) -> Result<SourceChange> {
        let (element, _) = self.row_to_element(table, &row.after_update.cells)?;
        Ok(SourceChange::Update { element })
    }

    pub fn decode_delete(&self, table: &TableMapEvent, row: &RowData) -> Result<SourceChange> {
        let (_, metadata) = self.row_to_element(table, &row.cells)?;
        Ok(SourceChange::Delete { metadata })
    }

    fn row_to_element(
        &self,
        table: &TableMapEvent,
        cells: &[Option<MySqlValue>],
    ) -> Result<(Element, ElementMetadata)> {
        // Use bare table name for element IDs (no database prefix) to match bootstrap.
        let table_name = &table.table_name;
        let label = table.table_name.clone();

        let mut properties = ElementPropertyMap::new();
        let mut key_parts: Vec<String> = Vec::new();
        let configured_keys = self.table_keys.get(table_name.as_str());
        let column_names = self.extract_column_names(table);

        let fallback_key = if configured_keys.is_none() && !column_names.is_empty() {
            column_names
                .iter()
                .find(|name| name.eq_ignore_ascii_case("id"))
                .cloned()
        } else {
            None
        };

        for (idx, value_opt) in cells.iter().enumerate() {
            let column_key = column_names
                .get(idx)
                .cloned()
                .unwrap_or_else(|| format!("col_{idx}"));
            let value = match value_opt {
                None => ElementValue::Null,
                Some(mysql_value) => mysql_value_to_element_value(mysql_value),
            };

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
            warn!("No primary key mapping for table '{table_name}', falling back to UUID");
        }

        let element_id = if !key_parts.is_empty() {
            format!("{}:{}", table_name, key_parts.join("_"))
        } else {
            format!("{}:{}", table_name, uuid::Uuid::new_v4())
        };

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

    fn extract_column_names(&self, table: &TableMapEvent) -> Vec<String> {
        if let Some(metadata) = &table.table_metadata {
            if let Some(names) = &metadata.column_names {
                return names.clone();
            }
        }
        Vec::new()
    }
}

fn mysql_value_to_element_value(value: &MySqlValue) -> ElementValue {
    match value {
        MySqlValue::TinyInt(v) => ElementValue::Integer(*v as i64),
        MySqlValue::SmallInt(v) => ElementValue::Integer(*v as i64),
        MySqlValue::MediumInt(v) => ElementValue::Integer(*v as i64),
        MySqlValue::Int(v) => ElementValue::Integer(*v as i64),
        MySqlValue::BigInt(v) => ElementValue::Integer(*v as i64),
        MySqlValue::Float(v) => ElementValue::Float(OrderedFloat(*v as f64)),
        MySqlValue::Double(v) => ElementValue::Float(OrderedFloat(*v)),
        MySqlValue::Decimal(v) => v
            .parse::<f64>()
            .map(OrderedFloat)
            .map(ElementValue::Float)
            .unwrap_or_else(|_| ElementValue::String(Arc::from(v.as_str()))),
        MySqlValue::String(v) => ElementValue::String(Arc::from(v.as_str())),
        MySqlValue::Bit(v) => {
            ElementValue::List(v.iter().map(|b| ElementValue::Bool(*b)).collect::<Vec<_>>())
        }
        MySqlValue::Enum(v) => ElementValue::String(Arc::from(v.to_string())),
        MySqlValue::Set(v) => ElementValue::String(Arc::from(v.to_string())),
        MySqlValue::Blob(v) => {
            let encoded = base64::engine::general_purpose::STANDARD.encode(v);
            ElementValue::String(Arc::from(encoded.as_str()))
        }
        MySqlValue::Year(v) => ElementValue::Integer(*v as i64),
        MySqlValue::Date(d) => ElementValue::String(Arc::from(format!(
            "{:04}-{:02}-{:02}",
            d.year, d.month, d.day
        ))),
        MySqlValue::Time(t) => ElementValue::String(Arc::from(format!(
            "{:03}:{:02}:{:02}.{:03}",
            t.hour, t.minute, t.second, t.millis
        ))),
        MySqlValue::DateTime(dt) => ElementValue::String(Arc::from(format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03}",
            dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second, dt.millis
        ))),
        MySqlValue::Timestamp(ts) => {
            let secs = *ts as i64;
            match chrono::DateTime::from_timestamp(secs, 0) {
                Some(dt) => {
                    ElementValue::String(Arc::from(dt.format("%Y-%m-%d %H:%M:%S.000").to_string()))
                }
                None => ElementValue::Integer(secs),
            }
        }
    }
}

fn format_value_for_key(value: &ElementValue) -> String {
    match value {
        ElementValue::Null => "null".to_string(),
        ElementValue::Bool(b) => b.to_string(),
        ElementValue::Float(f) => f.to_string(),
        ElementValue::Integer(i) => i.to_string(),
        ElementValue::String(s) => s.to_string(),
        ElementValue::List(l) => l
            .iter()
            .map(format_value_for_key)
            .collect::<Vec<_>>()
            .join("-"),
        ElementValue::Object(_) => "object".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mysql_cdc::events::row_events::mysql_value::MySqlValue;
    use ordered_float::OrderedFloat;

    #[test]
    fn test_tiny_int() {
        let v = mysql_value_to_element_value(&MySqlValue::TinyInt(42));
        assert_eq!(v, ElementValue::Integer(42));
    }

    #[test]
    fn test_small_int() {
        let v = mysql_value_to_element_value(&MySqlValue::SmallInt(1000));
        assert_eq!(v, ElementValue::Integer(1000));
    }

    #[test]
    fn test_medium_int() {
        let v = mysql_value_to_element_value(&MySqlValue::MediumInt(100_000));
        assert_eq!(v, ElementValue::Integer(100_000));
    }

    #[test]
    fn test_int() {
        let v = mysql_value_to_element_value(&MySqlValue::Int(123_456));
        assert_eq!(v, ElementValue::Integer(123_456));
    }

    #[test]
    fn test_big_int() {
        let v = mysql_value_to_element_value(&MySqlValue::BigInt(9_999_999_999));
        assert_eq!(v, ElementValue::Integer(9_999_999_999));
    }

    #[test]
    fn test_float() {
        let v = mysql_value_to_element_value(&MySqlValue::Float(3.14));
        assert!(matches!(v, ElementValue::Float(_)));
    }

    #[test]
    fn test_double() {
        let v = mysql_value_to_element_value(&MySqlValue::Double(2.718281828));
        assert_eq!(v, ElementValue::Float(OrderedFloat(2.718281828)));
    }

    #[test]
    fn test_decimal_valid() {
        let v = mysql_value_to_element_value(&MySqlValue::Decimal("12.34".to_string()));
        assert_eq!(v, ElementValue::Float(OrderedFloat(12.34)));
    }

    #[test]
    fn test_decimal_invalid() {
        let v = mysql_value_to_element_value(&MySqlValue::Decimal("not_a_number".to_string()));
        assert_eq!(v, ElementValue::String(Arc::from("not_a_number")));
    }

    #[test]
    fn test_string() {
        let v = mysql_value_to_element_value(&MySqlValue::String("hello".to_string()));
        assert_eq!(v, ElementValue::String(Arc::from("hello")));
    }

    #[test]
    fn test_bit() {
        let v = mysql_value_to_element_value(&MySqlValue::Bit(vec![true, false, true]));
        assert_eq!(
            v,
            ElementValue::List(vec![
                ElementValue::Bool(true),
                ElementValue::Bool(false),
                ElementValue::Bool(true),
            ])
        );
    }

    #[test]
    fn test_enum() {
        let v = mysql_value_to_element_value(&MySqlValue::Enum(2));
        assert_eq!(v, ElementValue::String(Arc::from("2")));
    }

    #[test]
    fn test_set() {
        let v = mysql_value_to_element_value(&MySqlValue::Set(7));
        assert_eq!(v, ElementValue::String(Arc::from("7")));
    }

    #[test]
    fn test_blob() {
        let v = mysql_value_to_element_value(&MySqlValue::Blob(vec![0xDE, 0xAD]));
        assert_eq!(v, ElementValue::String(Arc::from("3q0=")));
    }

    #[test]
    fn test_year() {
        let v = mysql_value_to_element_value(&MySqlValue::Year(2024));
        assert_eq!(v, ElementValue::Integer(2024));
    }

    #[test]
    fn test_date() {
        use mysql_cdc::events::row_events::mysql_value::Date;
        let v = mysql_value_to_element_value(&MySqlValue::Date(Date {
            year: 2024,
            month: 6,
            day: 15,
        }));
        assert_eq!(v, ElementValue::String(Arc::from("2024-06-15")));
    }

    #[test]
    fn test_time() {
        use mysql_cdc::events::row_events::mysql_value::Time;
        let v = mysql_value_to_element_value(&MySqlValue::Time(Time {
            hour: 13,
            minute: 45,
            second: 30,
            millis: 500,
        }));
        assert_eq!(v, ElementValue::String(Arc::from("013:45:30.500")));
    }

    #[test]
    fn test_datetime() {
        use mysql_cdc::events::row_events::mysql_value::DateTime;
        let v = mysql_value_to_element_value(&MySqlValue::DateTime(DateTime {
            year: 2024,
            month: 1,
            day: 2,
            hour: 3,
            minute: 4,
            second: 5,
            millis: 6,
        }));
        assert_eq!(
            v,
            ElementValue::String(Arc::from("2024-01-02 03:04:05.006"))
        );
    }

    #[test]
    fn test_timestamp_valid() {
        let v = mysql_value_to_element_value(&MySqlValue::Timestamp(1700000000));
        assert!(matches!(v, ElementValue::String(_)));
    }
}
