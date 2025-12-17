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

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Data schema for HTTP source events
///
/// This schema closely mirrors drasi_core::models::SourceChange for efficient conversion
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "lowercase")]
pub enum HttpSourceChange {
    /// Insert a new element
    #[serde(rename = "insert")]
    Insert {
        element: HttpElement,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },
    /// Update an existing element
    #[serde(rename = "update")]
    Update {
        element: HttpElement,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },
    /// Delete an element
    #[serde(rename = "delete")]
    Delete {
        id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        labels: Option<Vec<String>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },
}

/// Element that can be either a Node or Relation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HttpElement {
    #[serde(rename = "node")]
    Node {
        id: String,
        labels: Vec<String>,
        #[serde(default)]
        properties: serde_json::Map<String, serde_json::Value>,
    },
    #[serde(rename = "relation")]
    Relation {
        id: String,
        labels: Vec<String>,
        from: String,
        to: String,
        #[serde(default)]
        properties: serde_json::Map<String, serde_json::Value>,
    },
}

/// Convert HttpSourceChange to drasi_core::models::SourceChange
pub fn convert_http_to_source_change(
    http_change: &HttpSourceChange,
    source_id: &str,
) -> Result<drasi_core::models::SourceChange> {
    use drasi_core::models::{ElementMetadata, ElementReference, SourceChange};

    // Get timestamp or use current time in nanoseconds
    let get_timestamp = |ts: Option<u64>| -> u64 {
        ts.unwrap_or_else(|| {
            crate::time::get_system_time_nanos().unwrap_or_else(|e| {
                log::warn!("Failed to get system time for HTTP event: {e}, using fallback");
                // Use current milliseconds * 1M as fallback
                (chrono::Utc::now().timestamp_millis() as u64) * 1_000_000
            })
        })
    };

    match http_change {
        HttpSourceChange::Insert { element, timestamp } => {
            let element = create_element_from_http(element, source_id, get_timestamp(*timestamp))?;
            Ok(SourceChange::Insert { element })
        }
        HttpSourceChange::Update { element, timestamp } => {
            let element = create_element_from_http(element, source_id, get_timestamp(*timestamp))?;
            Ok(SourceChange::Update { element })
        }
        HttpSourceChange::Delete {
            id,
            labels,
            timestamp,
        } => {
            let metadata = ElementMetadata {
                reference: ElementReference {
                    source_id: Arc::from(source_id),
                    element_id: Arc::from(id.as_str()),
                },
                labels: Arc::from(
                    labels
                        .as_ref()
                        .map(|l| l.iter().map(|s| Arc::from(s.as_str())).collect::<Vec<_>>())
                        .unwrap_or_default(),
                ),
                effective_from: get_timestamp(*timestamp),
            };
            Ok(SourceChange::Delete { metadata })
        }
    }
}

/// Create Element from HttpElement
fn create_element_from_http(
    http_element: &HttpElement,
    source_id: &str,
    timestamp: u64,
) -> Result<drasi_core::models::Element> {
    use drasi_core::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference};

    match http_element {
        HttpElement::Node {
            id,
            labels,
            properties,
        } => {
            let metadata = ElementMetadata {
                reference: ElementReference {
                    source_id: Arc::from(source_id),
                    element_id: Arc::from(id.as_str()),
                },
                labels: Arc::from(
                    labels
                        .iter()
                        .map(|s| Arc::from(s.as_str()))
                        .collect::<Vec<_>>(),
                ),
                effective_from: timestamp,
            };

            let mut prop_map = ElementPropertyMap::new();
            for (key, value) in properties {
                prop_map.insert(key, convert_json_to_element_value(value)?);
            }

            Ok(Element::Node {
                metadata,
                properties: prop_map,
            })
        }
        HttpElement::Relation {
            id,
            labels,
            from,
            to,
            properties,
        } => {
            let metadata = ElementMetadata {
                reference: ElementReference {
                    source_id: Arc::from(source_id),
                    element_id: Arc::from(id.as_str()),
                },
                labels: Arc::from(
                    labels
                        .iter()
                        .map(|s| Arc::from(s.as_str()))
                        .collect::<Vec<_>>(),
                ),
                effective_from: timestamp,
            };

            let mut prop_map = ElementPropertyMap::new();
            for (key, value) in properties {
                prop_map.insert(key, convert_json_to_element_value(value)?);
            }

            Ok(Element::Relation {
                metadata,
                properties: prop_map,
                in_node: ElementReference {
                    source_id: Arc::from(source_id),
                    element_id: Arc::from(to.as_str()),
                },
                out_node: ElementReference {
                    source_id: Arc::from(source_id),
                    element_id: Arc::from(from.as_str()),
                },
            })
        }
    }
}

/// Convert JSON value to ElementValue
fn convert_json_to_element_value(
    value: &serde_json::Value,
) -> Result<drasi_core::models::ElementValue> {
    use drasi_core::models::ElementValue;
    use ordered_float::OrderedFloat;

    match value {
        serde_json::Value::Null => Ok(ElementValue::Null),
        serde_json::Value::Bool(b) => Ok(ElementValue::Bool(*b)),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(ElementValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(ElementValue::Float(OrderedFloat(f)))
            } else {
                Err(anyhow::anyhow!("Invalid number value"))
            }
        }
        serde_json::Value::String(s) => Ok(ElementValue::String(Arc::from(s.as_str()))),
        serde_json::Value::Array(arr) => {
            let elements: Result<Vec<_>> = arr.iter().map(convert_json_to_element_value).collect();
            Ok(ElementValue::List(elements?))
        }
        serde_json::Value::Object(obj) => {
            let mut map = drasi_core::models::ElementPropertyMap::new();
            for (key, val) in obj {
                map.insert(key, convert_json_to_element_value(val)?);
            }
            Ok(ElementValue::Object(map))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_insert_deserialization() {
        let json = r#"{
            "operation": "insert",
            "element": {
                "type": "node",
                "id": "user_123",
                "labels": ["User", "Person"],
                "properties": {
                    "name": "John Doe",
                    "age": 30,
                    "active": true
                }
            },
            "timestamp": 1234567890000
        }"#;

        let result: HttpSourceChange = serde_json::from_str(json).unwrap();
        match result {
            HttpSourceChange::Insert { element, timestamp } => {
                match element {
                    HttpElement::Node {
                        id,
                        labels,
                        properties,
                    } => {
                        assert_eq!(id, "user_123");
                        assert_eq!(labels, vec!["User", "Person"]);
                        assert_eq!(properties["name"], "John Doe");
                        assert_eq!(properties["age"], 30);
                        assert_eq!(properties["active"], true);
                    }
                    _ => panic!("Expected Node element"),
                }
                assert_eq!(timestamp, Some(1234567890000));
            }
            _ => panic!("Expected Insert operation"),
        }
    }

    #[test]
    fn test_relation_insert_deserialization() {
        let json = r#"{
            "operation": "insert",
            "element": {
                "type": "relation",
                "id": "follows_123",
                "labels": ["FOLLOWS"],
                "from": "user_123",
                "to": "user_456",
                "properties": {
                    "since": "2024-01-01"
                }
            }
        }"#;

        let result: HttpSourceChange = serde_json::from_str(json).unwrap();
        match result {
            HttpSourceChange::Insert { element, timestamp } => {
                match element {
                    HttpElement::Relation {
                        id,
                        labels,
                        from,
                        to,
                        properties,
                    } => {
                        assert_eq!(id, "follows_123");
                        assert_eq!(labels, vec!["FOLLOWS"]);
                        assert_eq!(from, "user_123");
                        assert_eq!(to, "user_456");
                        assert_eq!(properties["since"], "2024-01-01");
                    }
                    _ => panic!("Expected Relation element"),
                }
                assert_eq!(timestamp, None);
            }
            _ => panic!("Expected Insert operation"),
        }
    }

    #[test]
    fn test_delete_deserialization() {
        let json = r#"{
            "operation": "delete",
            "id": "user_123",
            "labels": ["User"],
            "timestamp": 1234567890000
        }"#;

        let result: HttpSourceChange = serde_json::from_str(json).unwrap();
        match result {
            HttpSourceChange::Delete {
                id,
                labels,
                timestamp,
            } => {
                assert_eq!(id, "user_123");
                assert_eq!(labels, Some(vec!["User".to_string()]));
                assert_eq!(timestamp, Some(1234567890000));
            }
            _ => panic!("Expected Delete operation"),
        }
    }

    #[test]
    fn test_minimal_delete_deserialization() {
        let json = r#"{
            "operation": "delete",
            "id": "user_123"
        }"#;

        let result: HttpSourceChange = serde_json::from_str(json).unwrap();
        match result {
            HttpSourceChange::Delete {
                id,
                labels,
                timestamp,
            } => {
                assert_eq!(id, "user_123");
                assert_eq!(labels, None);
                assert_eq!(timestamp, None);
            }
            _ => panic!("Expected Delete operation"),
        }
    }
}
