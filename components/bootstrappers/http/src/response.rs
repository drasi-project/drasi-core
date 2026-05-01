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

//! Response parsing and element extraction.
//!
//! Extracts items from HTTP responses and maps them to Drasi graph elements
//! using the template engine.

use anyhow::{anyhow, Context, Result};
use drasi_core::models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue};
use ordered_float::OrderedFloat;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{ElementMappingConfig, ElementType};
use crate::pagination;
use crate::template_engine::{TemplateContext, TemplateEngine};

/// Extract items array from a response body using the configured items path.
pub fn extract_items(body: &JsonValue, items_path: &str) -> Result<Vec<JsonValue>> {
    // Use the shared path navigator that supports bracket notation and negative indexes
    let items_value = pagination::navigate_path(body, items_path)
        .ok_or_else(|| anyhow!("Items path '{items_path}' did not resolve to a value in response"))?;

    match items_value {
        JsonValue::Array(arr) => Ok(arr.clone()),
        // If it's a single object, wrap it in a vec
        other if other.is_object() => Ok(vec![other.clone()]),
        _ => Err(anyhow!(
            "Items path '{items_path}' did not resolve to an array or object"
        )),
    }
}

/// Map a list of items to Drasi graph elements using the configured mappings.
pub fn map_items_to_elements(
    items: &[JsonValue],
    mappings: &[ElementMappingConfig],
    source_id: &str,
    engine: &TemplateEngine,
) -> Vec<Result<Element>> {
    let mut elements = Vec::new();

    for (index, item) in items.iter().enumerate() {
        let context = TemplateContext {
            item: item.clone(),
            index,
            source_id: source_id.to_string(),
        };

        for mapping in mappings {
            let result = map_single_item(&context, mapping, source_id, engine);
            elements.push(result);
        }
    }

    elements
}

/// Map a single item to a Drasi Element.
fn map_single_item(
    context: &TemplateContext,
    mapping: &ElementMappingConfig,
    source_id: &str,
    engine: &TemplateEngine,
) -> Result<Element> {
    let template = &mapping.template;

    // Render ID
    let id = engine
        .render_string(&template.id, context)
        .context("Failed to render element ID template")?;

    if id.is_empty() {
        return Err(anyhow!("Element ID rendered to empty string"));
    }

    // Render labels
    let mut labels = Vec::new();
    for label_template in &template.labels {
        let label = engine
            .render_string(label_template, context)
            .context("Failed to render label template")?;
        if !label.is_empty() {
            labels.push(Arc::from(label.as_str()));
        }
    }

    // Render properties
    let properties = if let Some(ref props) = template.properties {
        let rendered = engine
            .render_properties(props, context)
            .context("Failed to render properties")?;
        json_map_to_element_properties(&rendered)
    } else {
        ElementPropertyMap::new()
    };

    // Create element based on type
    match mapping.element_type {
        ElementType::Node => {
            let metadata = ElementMetadata {
                reference: ElementReference::new(source_id, &id),
                labels: labels.into(),
                effective_from: 0,
            };
            Ok(Element::Node {
                metadata,
                properties,
            })
        }
        ElementType::Relation => {
            let from_id = template
                .from
                .as_ref()
                .ok_or_else(|| anyhow!("Relation mapping requires 'from' template"))?;
            let to_id = template
                .to
                .as_ref()
                .ok_or_else(|| anyhow!("Relation mapping requires 'to' template"))?;

            let from_rendered = engine
                .render_string(from_id, context)
                .context("Failed to render 'from' template")?;
            let to_rendered = engine
                .render_string(to_id, context)
                .context("Failed to render 'to' template")?;

            let metadata = ElementMetadata {
                reference: ElementReference::new(source_id, &id),
                labels: labels.into(),
                effective_from: 0,
            };

            Ok(Element::Relation {
                metadata,
                properties,
                in_node: ElementReference::new(source_id, &from_rendered),
                out_node: ElementReference::new(source_id, &to_rendered),
            })
        }
    }
}

/// Convert a HashMap<String, JsonValue> to ElementPropertyMap.
fn json_map_to_element_properties(map: &HashMap<String, JsonValue>) -> ElementPropertyMap {
    let mut props = ElementPropertyMap::new();
    for (key, value) in map {
        if let Some(elem_value) = json_value_to_element_value(value) {
            props.insert(key.as_str(), elem_value);
        }
    }
    props
}

/// Convert a serde_json::Value to an ElementValue.
fn json_value_to_element_value(value: &JsonValue) -> Option<ElementValue> {
    match value {
        JsonValue::Null => Some(ElementValue::Null),
        JsonValue::Bool(b) => Some(ElementValue::Bool(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Some(ElementValue::Integer(i))
            } else {
                n.as_f64().map(|f| ElementValue::Float(OrderedFloat(f)))
            }
        }
        JsonValue::String(s) => Some(ElementValue::String(s.clone().into())),
        JsonValue::Array(arr) => {
            let elements: Vec<ElementValue> = arr
                .iter()
                .filter_map(json_value_to_element_value)
                .collect();
            Some(ElementValue::List(elements))
        }
        JsonValue::Object(map) => {
            // Convert object to a JSON string representation
            Some(ElementValue::String(serde_json::to_string(map).unwrap_or_default().into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_extract_items_top_level_array() {
        let body = json!([{"id": "1"}, {"id": "2"}]);
        let items = extract_items(&body, "$").unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_extract_items_nested_path() {
        let body = json!({"data": [{"id": "1"}, {"id": "2"}]});
        let items = extract_items(&body, "$.data").unwrap();
        assert_eq!(items.len(), 2);
    }

    #[test]
    fn test_extract_items_deep_nested() {
        let body = json!({"response": {"results": [{"id": "1"}]}});
        let items = extract_items(&body, "$.response.results").unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn test_map_items_to_nodes() {
        let items = vec![
            json!({"id": "1", "name": "Alice"}),
            json!({"id": "2", "name": "Bob"}),
        ];

        let mappings = vec![ElementMappingConfig {
            element_type: ElementType::Node,
            template: crate::config::ElementTemplate {
                id: "{{item.id}}".to_string(),
                labels: vec!["User".to_string()],
                properties: Some(json!({"name": "{{item.name}}"})),
                from: None,
                to: None,
            },
        }];

        let engine = TemplateEngine::new();
        let results = map_items_to_elements(&items, &mappings, "test-source", &engine);
        assert_eq!(results.len(), 2);

        let elem = results[0].as_ref().unwrap();
        match elem {
            Element::Node { metadata, properties } => {
                assert_eq!(&*metadata.reference.element_id, "1");
                assert_eq!(metadata.labels.len(), 1);
                assert_eq!(&*metadata.labels[0], "User");
            }
            _ => panic!("Expected Node"),
        }
    }

    #[test]
    fn test_map_items_to_relations() {
        let items = vec![json!({"id": "r1", "from": "n1", "to": "n2", "type": "KNOWS"})];

        let mappings = vec![ElementMappingConfig {
            element_type: ElementType::Relation,
            template: crate::config::ElementTemplate {
                id: "{{item.id}}".to_string(),
                labels: vec!["{{item.type}}".to_string()],
                properties: None,
                from: Some("{{item.from}}".to_string()),
                to: Some("{{item.to}}".to_string()),
            },
        }];

        let engine = TemplateEngine::new();
        let results = map_items_to_elements(&items, &mappings, "test-source", &engine);
        assert_eq!(results.len(), 1);

        let elem = results[0].as_ref().unwrap();
        match elem {
            Element::Relation {
                metadata,
                in_node,
                out_node,
                ..
            } => {
                assert_eq!(&*metadata.reference.element_id, "r1");
                assert_eq!(&*in_node.element_id, "n1");
                assert_eq!(&*out_node.element_id, "n2");
            }
            _ => panic!("Expected Relation"),
        }
    }
}
