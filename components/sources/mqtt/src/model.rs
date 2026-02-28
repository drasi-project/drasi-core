use anyhow::Result;
use core::time;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::vec::Vec;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Eq)]
pub enum QualityOfService {
    AtMostOnce,
    AtLeastOnce,
    ExactlyOnce,
}

/// Data schema for MQTT source events
///
/// This schema closely mirrors drasi_core::models::SourceChange for efficient conversion
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "lowercase")]
pub enum MqttSourceChange {
    /// Insert a new element
    #[serde(rename = "insert")]
    Insert {
        element: MQTTElement,
        #[serde(skip_serializing_if = "Option::is_none")]
        timestamp: Option<u64>,
    },
    /// Update an existing element
    #[serde(rename = "update")]
    Update {
        element: MQTTElement,
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
pub enum MQTTElement {
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

pub fn map_json_to_mqtt_source_change(json_str: &str) -> Result<MqttSourceChange> {
    let change: MqttSourceChange = serde_json::from_str(json_str)?;
    Ok(change)
}

pub fn convert_mqtt_to_source_change(
    mqtt_change: &MqttSourceChange,
    source_id: &str,
) -> Result<drasi_core::models::SourceChange> {
    use drasi_core::models::{ElementMetadata, ElementReference, SourceChange};
    match mqtt_change {
        MqttSourceChange::Insert { element, timestamp } => {
            let element = create_element_from_mqtt(
                element,
                source_id,
                timestamp.unwrap_or_else(|| {
                    let now = std::time::SystemTime::now();
                    now.duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64
                }),
            )?;
            Ok(SourceChange::Insert { element })
        }
        MqttSourceChange::Update { element, timestamp } => {
            let element = create_element_from_mqtt(
                element,
                source_id,
                timestamp.unwrap_or_else(|| {
                    let now = std::time::SystemTime::now();
                    now.duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64
                }),
            )?;
            Ok(SourceChange::Update { element })
        }
        MqttSourceChange::Delete {
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
                effective_from: timestamp.unwrap_or_else(|| {
                    let now = std::time::SystemTime::now();
                    now.duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64
                }),
            };
            Ok(SourceChange::Delete { metadata })
        }
    }
}

fn create_element_from_mqtt(
    mqtt_element: &MQTTElement,
    source_id: &str,
    timestamp: u64,
) -> Result<drasi_core::models::Element> {
    use drasi_core::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference};

    match mqtt_element {
        MQTTElement::Node {
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
        MQTTElement::Relation {
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
