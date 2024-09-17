use std::{
    fmt::{Display, Formatter},
    sync::Arc,
};

use crate::evaluation::variable_value::VariableValue;

use super::{ElementPropertyMap, ElementValue};

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ElementReference {
    pub source_id: Arc<str>,
    pub element_id: Arc<str>,
}

impl ElementReference {
    pub fn new(source_id: &str, element_id: &str) -> Self {
        ElementReference {
            source_id: Arc::from(source_id),
            element_id: Arc::from(element_id),
        }
    }
}

impl Display for ElementReference {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.source_id, self.element_id)
    }
}

pub type ElementTimestamp = u64;

#[derive(Debug, Clone, Hash, PartialEq, Eq)]
pub struct ElementMetadata {
    pub reference: ElementReference,
    pub labels: Arc<[Arc<str>]>,
    pub effective_from: ElementTimestamp,
}

impl Display for ElementMetadata {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "({}, [{}], {})",
            self.reference,
            self.labels.join(","),
            self.effective_from
        )
    }
}

#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub enum Element {
    // Incoming changes get turned into an Element
    Node {
        metadata: ElementMetadata,
        properties: ElementPropertyMap,
    },
    Relation {
        metadata: ElementMetadata,
        in_node: ElementReference,
        out_node: ElementReference,
        properties: ElementPropertyMap,
    },
}

impl Element {
    pub fn get_reference(&self) -> &ElementReference {
        match self {
            Element::Node { metadata, .. } => &metadata.reference,
            Element::Relation { metadata, .. } => &metadata.reference,
        }
    }

    pub fn get_effective_from(&self) -> ElementTimestamp {
        match self {
            Element::Node { metadata, .. } => metadata.effective_from,
            Element::Relation { metadata, .. } => metadata.effective_from,
        }
    }

    pub fn get_metadata(&self) -> &ElementMetadata {
        match self {
            Element::Node { metadata, .. } => metadata,
            Element::Relation { metadata, .. } => metadata,
        }
    }

    pub fn get_property(&self, name: &str) -> &ElementValue {
        let props = match self {
            Element::Node { properties, .. } => properties,
            Element::Relation { properties, .. } => properties,
        };
        &props[name]
    }

    pub fn get_properties(&self) -> &ElementPropertyMap {
        match self {
            Element::Node { properties, .. } => properties,
            Element::Relation { properties, .. } => properties,
        }
    }

    pub fn merge_missing_properties(&mut self, other: &Element) {
        match (self, other) {
            (
                Element::Node {
                    properties,
                    metadata,
                },
                Element::Node {
                    properties: other_properties,
                    metadata: other_metadata,
                },
            ) => {
                assert_eq!(metadata.reference, other_metadata.reference);
                properties.merge(other_properties);
            }
            (
                Element::Relation {
                    in_node: _,
                    out_node: _,
                    properties,
                    metadata,
                },
                Element::Relation {
                    in_node: _other_in_node,
                    out_node: _other_out_node,
                    properties: other_properties,
                    metadata: other_metadata,
                },
            ) => {
                assert_eq!(metadata.reference, other_metadata.reference);
                properties.merge(other_properties);
            }
            _ => panic!("Cannot merge different element types"),
        }
    }

    pub fn to_expression_variable(&self) -> VariableValue {
        VariableValue::Element(Arc::new(self.clone()))
    }

    pub fn update_effective_time(&mut self, timestamp: ElementTimestamp) {
        match self {
            Element::Node { metadata, .. } => metadata.effective_from = timestamp,
            Element::Relation { metadata, .. } => metadata.effective_from = timestamp,
        }
    }
}

impl From<&Element> for serde_json::Value {
    fn from(val: &Element) -> Self {
        match val {
            Element::Node {
                metadata,
                properties,
            } => {
                let mut properties: serde_json::Map<String, serde_json::Value> = properties.into();

                properties.insert(
                    "$metadata".to_string(),
                    serde_json::Value::String(metadata.to_string()),
                );

                serde_json::Value::Object(properties)
            }
            Element::Relation {
                metadata,
                in_node,
                out_node,
                properties,
            } => {
                let mut properties: serde_json::Map<String, serde_json::Value> = properties.into();

                properties.insert(
                    "$metadata".to_string(),
                    serde_json::Value::String(metadata.to_string()),
                );

                properties.insert(
                    "$in_node".to_string(),
                    serde_json::Value::String(in_node.to_string()),
                );
                properties.insert(
                    "$out_node".to_string(),
                    serde_json::Value::String(out_node.to_string()),
                );

                serde_json::Value::Object(properties)
            }
        }
    }
}
