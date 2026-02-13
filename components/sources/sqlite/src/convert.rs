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

use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use ordered_float::OrderedFloat;
use rusqlite::hooks::Action;
use serde_json::Value;
use std::sync::Arc;

use crate::thread::ChangeEvent;

pub fn json_value_to_element_value(value: &Value) -> ElementValue {
    match value {
        Value::Null => ElementValue::Null,
        Value::Bool(v) => ElementValue::Bool(*v),
        Value::Number(v) => {
            if let Some(i) = v.as_i64() {
                ElementValue::Integer(i)
            } else if let Some(f) = v.as_f64() {
                ElementValue::Float(OrderedFloat(f))
            } else {
                ElementValue::Null
            }
        }
        Value::String(v) => ElementValue::String(Arc::from(v.as_str())),
        Value::Array(values) => ElementValue::List(
            values
                .iter()
                .map(json_value_to_element_value)
                .collect::<Vec<_>>(),
        ),
        Value::Object(values) => {
            let mut map = ElementPropertyMap::new();
            for (key, value) in values {
                map.insert(key, json_value_to_element_value(value));
            }
            ElementValue::Object(map)
        }
    }
}

fn to_property_map(values: &[(String, Value)]) -> ElementPropertyMap {
    let mut map = ElementPropertyMap::new();
    for (name, value) in values {
        map.insert(name, json_value_to_element_value(value));
    }
    map
}

fn key_part(value: &Value) -> String {
    match value {
        Value::String(v) => v.clone(),
        _ => value.to_string(),
    }
}

fn generate_element_id(
    table: &str,
    values: &[(String, Value)],
    configured_key_columns: Option<&[String]>,
    fallback_pk_columns: &[String],
    row_id: Option<i64>,
) -> String {
    let selected_keys = configured_key_columns
        .filter(|keys| !keys.is_empty())
        .unwrap_or(fallback_pk_columns);

    let mut parts = Vec::new();
    for key in selected_keys {
        if let Some((_, value)) = values.iter().find(|(name, _)| name == key) {
            parts.push(key_part(value));
        }
    }

    if parts.is_empty() {
        if let Some(id) = row_id {
            return format!("{table}:{id}");
        }
        return format!("{table}:unknown");
    }

    format!("{table}:{}", parts.join(":"))
}

pub fn change_event_to_source_change(
    event: ChangeEvent,
    source_id: &str,
    configured_key_columns: Option<&[String]>,
) -> SourceChange {
    let labels: Arc<[Arc<str>]> = Arc::from([Arc::from(event.table.as_str())]);
    let effective_from = event.timestamp_ms;

    match event.action {
        Action::SQLITE_INSERT => {
            let values = event.new_values.unwrap_or_default();
            let element_id = generate_element_id(
                &event.table,
                &values,
                configured_key_columns,
                &event.table_pk_columns,
                event.new_row_id,
            );

            SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(source_id, &element_id),
                        labels,
                        effective_from,
                    },
                    properties: to_property_map(&values),
                },
            }
        }
        Action::SQLITE_UPDATE => {
            let values = event.new_values.unwrap_or_default();
            let element_id = generate_element_id(
                &event.table,
                &values,
                configured_key_columns,
                &event.table_pk_columns,
                event.new_row_id.or(event.old_row_id),
            );

            SourceChange::Update {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new(source_id, &element_id),
                        labels,
                        effective_from,
                    },
                    properties: to_property_map(&values),
                },
            }
        }
        Action::SQLITE_DELETE => {
            let values = event.old_values.unwrap_or_default();
            let element_id = generate_element_id(
                &event.table,
                &values,
                configured_key_columns,
                &event.table_pk_columns,
                event.old_row_id,
            );

            SourceChange::Delete {
                metadata: ElementMetadata {
                    reference: ElementReference::new(source_id, &element_id),
                    labels,
                    effective_from,
                },
            }
        }
        _ => SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new(source_id, "unknown"),
                labels,
                effective_from,
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_change_uses_configured_keys_for_reference_id() {
        let event = ChangeEvent {
            action: Action::SQLITE_INSERT,
            table: "sensors".to_string(),
            old_values: None,
            new_values: Some(vec![
                ("id".to_string(), serde_json::json!(7)),
                ("name".to_string(), serde_json::json!("sensor-seven")),
            ]),
            old_row_id: None,
            new_row_id: Some(99),
            table_pk_columns: vec!["id".to_string()],
            timestamp_ms: 123,
        };

        let change =
            change_event_to_source_change(event, "sqlite-source", Some(&["id".to_string()]));
        match change {
            SourceChange::Insert {
                element: Element::Node { metadata, .. },
            } => {
                assert_eq!(metadata.reference.source_id.as_ref(), "sqlite-source");
                assert_eq!(metadata.reference.element_id.as_ref(), "sensors:7");
                assert_eq!(metadata.effective_from, 123);
            }
            other => panic!("unexpected change type: {other:?}"),
        }
    }

    #[test]
    fn update_change_falls_back_to_rowid_without_keys() {
        let event = ChangeEvent {
            action: Action::SQLITE_UPDATE,
            table: "devices".to_string(),
            old_values: Some(vec![("name".to_string(), serde_json::json!("before"))]),
            new_values: Some(vec![("name".to_string(), serde_json::json!("after"))]),
            old_row_id: Some(12),
            new_row_id: Some(12),
            table_pk_columns: Vec::new(),
            timestamp_ms: 456,
        };

        let change = change_event_to_source_change(event, "sqlite-source", None);
        match change {
            SourceChange::Update {
                element: Element::Node { metadata, .. },
            } => {
                assert_eq!(metadata.reference.element_id.as_ref(), "devices:12");
                assert_eq!(metadata.effective_from, 456);
            }
            other => panic!("unexpected change type: {other:?}"),
        }
    }

    #[test]
    fn delete_change_uses_old_values_for_id_generation() {
        let event = ChangeEvent {
            action: Action::SQLITE_DELETE,
            table: "events".to_string(),
            old_values: Some(vec![
                ("tenant".to_string(), serde_json::json!("t1")),
                ("event_id".to_string(), serde_json::json!("abc")),
            ]),
            new_values: None,
            old_row_id: Some(88),
            new_row_id: None,
            table_pk_columns: vec!["tenant".to_string(), "event_id".to_string()],
            timestamp_ms: 789,
        };

        let change = change_event_to_source_change(event, "sqlite-source", None);
        match change {
            SourceChange::Delete { metadata } => {
                assert_eq!(metadata.reference.element_id.as_ref(), "events:t1:abc");
                assert_eq!(metadata.effective_from, 789);
            }
            other => panic!("unexpected change type: {other:?}"),
        }
    }
}
