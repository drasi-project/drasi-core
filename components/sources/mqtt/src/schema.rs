// Copyright 2026 The Drasi Authors.
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
use core::time;
use drasi_core::models::{ElementMetadata, ElementReference, SourceChange};
use log::error;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::vec::Vec;

/// Data schema for MQTT source events
///
/// This schema closely mirrors drasi_core::models::SourceChange for efficient conversion
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "lowercase")]
pub enum MqttSourceChange {
    // Currently we only support updates.
    /// Update an existing element
    #[serde(rename = "update")]
    Update {
        element: MqttElement,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },
}

/// Element that can be either a Node or Relation
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum MqttElement {
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

pub fn convert_mqtt_to_source_change(
    mqtt_change: &MqttSourceChange,
    source_id: &str,
) -> Result<drasi_core::models::SourceChange> {
    match mqtt_change {
        MqttSourceChange::Update { element, timestamp } => {
            let element = create_element_from_mqtt(
                element,
                source_id,
                timestamp.unwrap_or_else(|| {
                    let now = std::time::SystemTime::now(); // not reachable. timestamp should always be provided for updates, but we default to current time just in case
                    now.duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_else(|e| {
                            error!("System time is before UNIX EPOCH: {e}");
                            std::time::Duration::from_millis(0)
                        })
                        .as_millis() as u64
                }),
            )?;
            Ok(SourceChange::Update { element })
        }
        _ => Err(anyhow::anyhow!("Unsupported MQTT source change operation")),
    }
}

fn create_element_from_mqtt(
    mqtt_element: &MqttElement,
    source_id: &str,
    timestamp: u64,
) -> Result<drasi_core::models::Element> {
    use drasi_core::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference};

    match mqtt_element {
        MqttElement::Node {
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
            for (k, v) in properties {
                prop_map.insert(k, convert_json_to_element_value(v)?);
            }
            Ok(Element::Node {
                metadata,
                properties: prop_map,
            })
        }
        MqttElement::Relation {
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
            for (k, v) in properties {
                prop_map.insert(k, convert_json_to_element_value(v)?);
            }
            Ok(Element::Relation {
                metadata,
                properties: prop_map,
                in_node: ElementReference {
                    // start node
                    source_id: Arc::from(source_id),
                    element_id: Arc::from(from.as_str()),
                },
                out_node: ElementReference {
                    // end node
                    source_id: Arc::from(source_id),
                    element_id: Arc::from(to.as_str()),
                },
            })
        }
    }
}

// Convert JSON value to ElementValue
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
                Err(anyhow::anyhow!("Unsupported number type"))
            }
        }
        serde_json::Value::String(s) => Ok(ElementValue::String(Arc::from(s.as_str()))),
        serde_json::Value::Array(arr) => {
            let mut vec = Vec::new();
            for item in arr {
                vec.push(convert_json_to_element_value(item)?);
            }
            Ok(ElementValue::List(vec))
        }
        serde_json::Value::Object(obj) => {
            let mut map = drasi_core::models::ElementPropertyMap::new();
            for (k, v) in obj {
                map.insert(k, convert_json_to_element_value(v)?);
            }
            Ok(ElementValue::Object(map))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_core::models::{Element, ElementValue, SourceChange};
    use ordered_float::OrderedFloat;
    use serde_json::json;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn sample_node(properties: serde_json::Map<String, serde_json::Value>) -> MqttElement {
        MqttElement::Node {
            id: "device-1".to_string(),
            labels: vec!["Device".to_string(), "Sensor".to_string()],
            properties,
        }
    }

    fn sample_relation(properties: serde_json::Map<String, serde_json::Value>) -> MqttElement {
        MqttElement::Relation {
            id: "rel-1".to_string(),
            labels: vec!["DEVICE_TO_ROOM".to_string()],
            from: "device-1".to_string(),
            to: "room-7".to_string(),
            properties,
        }
    }

    #[test]
    fn convert_update_relation_preserves_direction_and_properties() {
        let mut properties = serde_json::Map::new();
        properties.insert("weight".to_string(), json!(42));

        let mqtt_change = MqttSourceChange::Update {
            element: sample_relation(properties),
            timestamp: Some(99),
        };

        let converted = convert_mqtt_to_source_change(&mqtt_change, "test-source")
            .expect("relation update conversion should succeed");

        match converted {
            SourceChange::Update { element } => match element {
                Element::Relation {
                    metadata,
                    in_node,
                    out_node,
                    properties,
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "rel-1");
                    assert_eq!(metadata.effective_from, 99);
                    assert_eq!(out_node.element_id.as_ref(), "room-7");
                    assert_eq!(in_node.element_id.as_ref(), "device-1");
                    assert_eq!(properties.get("weight"), Some(&ElementValue::Integer(42)));
                }
                _ => panic!("expected relation element"),
            },
            _ => panic!("expected update change"),
        }
    }

    #[test]
    fn create_element_from_mqtt_converts_relation_directly() {
        let mut properties = serde_json::Map::new();
        properties.insert("online".to_string(), json!(false));

        let element = create_element_from_mqtt(&sample_relation(properties), "src-a", 123)
            .expect("direct relation conversion should succeed");

        match element {
            Element::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => {
                assert_eq!(metadata.reference.source_id.as_ref(), "src-a");
                assert_eq!(metadata.effective_from, 123);
                assert_eq!(out_node.source_id.as_ref(), "src-a");
                assert_eq!(out_node.element_id.as_ref(), "room-7");
                assert_eq!(in_node.element_id.as_ref(), "device-1");
                assert_eq!(properties.get("online"), Some(&ElementValue::Bool(false)));
            }
            _ => panic!("expected relation element"),
        }
    }

    #[test]
    fn convert_json_to_element_value_covers_all_supported_types() {
        assert_eq!(
            convert_json_to_element_value(&json!(null)).unwrap(),
            ElementValue::Null
        );
        assert_eq!(
            convert_json_to_element_value(&json!(true)).unwrap(),
            ElementValue::Bool(true)
        );
        assert_eq!(
            convert_json_to_element_value(&json!(7)).unwrap(),
            ElementValue::Integer(7)
        );
        assert_eq!(
            convert_json_to_element_value(&json!(2.25)).unwrap(),
            ElementValue::Float(OrderedFloat(2.25))
        );
        assert_eq!(
            convert_json_to_element_value(&json!("mqtt")).unwrap(),
            ElementValue::String(Arc::from("mqtt"))
        );

        let list = convert_json_to_element_value(&json!([1, "two", false]))
            .expect("list conversion should succeed");
        assert!(matches!(list, ElementValue::List(values) if values.len() == 3));

        let object = convert_json_to_element_value(&json!({"x": 1, "y": [true]}))
            .expect("object conversion should succeed");
        assert!(
            matches!(object, ElementValue::Object(map) if map.get("x") == Some(&ElementValue::Integer(1)))
        );
    }

    #[test]
    fn mqtt_source_change_serializes_and_deserializes_round_trip() {
        let change = MqttSourceChange::Update {
            element: MqttElement::Node {
                id: "node-1".to_string(),
                labels: vec!["Device".to_string()],
                properties: serde_json::Map::from_iter([
                    ("reading".to_string(), json!(12)),
                    ("status".to_string(), json!("ok")),
                ]),
            },
            timestamp: Some(444),
        };

        let serialized = serde_json::to_string(&change).expect("serialization should succeed");
        let deserialized: MqttSourceChange =
            serde_json::from_str(&serialized).expect("deserialization should succeed");

        match deserialized {
            MqttSourceChange::Update { element, timestamp } => {
                assert_eq!(timestamp, Some(444));
                match element {
                    MqttElement::Node {
                        id,
                        labels,
                        properties,
                    } => {
                        assert_eq!(id, "node-1");
                        assert_eq!(labels, vec!["Device".to_string()]);
                        assert_eq!(properties.get("reading"), Some(&json!(12)));
                        assert_eq!(properties.get("status"), Some(&json!("ok")));
                    }
                    _ => panic!("expected node element"),
                }
            }
            _ => panic!("expected update change"),
        }
    }
}
