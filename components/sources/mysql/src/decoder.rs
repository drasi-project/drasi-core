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
        let table_name = if table.database_name.is_empty() {
            table.table_name.clone()
        } else {
            format!("{}.{}", table.database_name, table.table_name)
        };
        let label = table.table_name.clone();

        let mut properties = ElementPropertyMap::new();
        let mut key_parts: Vec<String> = Vec::new();
        let configured_keys = self.table_keys.get(&table_name);
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

        let mut properties = properties;
        properties.insert(
            "_element_id",
            ElementValue::String(Arc::from(element_id.as_str())),
        );

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
        MySqlValue::Enum(v) => ElementValue::Integer(*v as i64),
        MySqlValue::Set(v) => ElementValue::Integer(*v as i64),
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
        MySqlValue::Timestamp(ts) => ElementValue::Integer(*ts as i64),
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
