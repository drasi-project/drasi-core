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

//! Template engine for webhook payload transformation.
//!
//! This module provides the HTTP-source-specific `TemplateContext` and delegates
//! to the shared `SourceMappingEngine` from `drasi-source-mapping`.

use crate::config::WebhookMapping;
use anyhow::Result;
use drasi_core::models::{ElementValue, SourceChange};
use drasi_source_mapping::SourceMappingEngine;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

// Re-export json_to_element_value for backward compatibility
pub use drasi_source_mapping::json_to_element_value;

/// Template context containing all variables available in templates.
///
/// This is the HTTP-source-specific context. It gets serialized to JSON
/// before being passed to the shared mapping engine.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TemplateContext {
    /// Parsed payload body
    pub payload: JsonValue,
    /// Path parameters extracted from route
    pub route: HashMap<String, String>,
    /// Query parameters
    pub query: HashMap<String, String>,
    /// HTTP headers
    pub headers: HashMap<String, String>,
    /// HTTP method
    pub method: String,
    /// Request path
    pub path: String,
    /// Source ID
    pub source_id: String,
}

/// Compiled template engine with pre-registered templates.
///
/// Delegates to `SourceMappingEngine` from `drasi-source-mapping`.
pub struct TemplateEngine {
    inner: SourceMappingEngine,
}

impl TemplateEngine {
    /// Create a new template engine with custom helpers registered
    pub fn new() -> Self {
        Self {
            inner: SourceMappingEngine::new(),
        }
    }

    /// Render a template string with the given context
    pub fn render_string(&self, template: &str, context: &TemplateContext) -> Result<String> {
        let json_context = context_to_json(context);
        self.inner.render_string(template, &json_context)
    }

    /// Render a template and preserve the JSON value type
    pub fn render_value(&self, template: &str, context: &TemplateContext) -> Result<JsonValue> {
        let json_context = context_to_json(context);
        self.inner.render_value(template, &json_context)
    }

    /// Process a webhook mapping and create a SourceChange
    pub fn process_mapping(
        &self,
        mapping: &WebhookMapping,
        context: &TemplateContext,
        source_id: &str,
    ) -> Result<SourceChange> {
        let json_context = context_to_json(context);
        self.inner
            .process_mapping(mapping, &json_context, source_id)
    }

    /// Render a single property value (used internally by tests)
    pub fn render_property_value(
        &self,
        value: &JsonValue,
        context: &TemplateContext,
    ) -> Result<ElementValue> {
        // For property values that are template strings, render them
        let json_context = context_to_json(context);
        match value {
            JsonValue::String(template) => {
                let rendered = self.inner.render_value(template, &json_context)?;
                json_to_element_value(&rendered)
            }
            other => json_to_element_value(other),
        }
    }
}

impl Default for TemplateEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Convert TemplateContext to JSON value for the shared engine
fn context_to_json(context: &TemplateContext) -> JsonValue {
    serde_json::to_value(context).unwrap_or(JsonValue::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ElementTemplate, ElementType, OperationType, TimestampFormat, WebhookMapping,
    };
    use drasi_core::models::{Element, ElementValue, SourceChange};

    fn create_test_context() -> TemplateContext {
        let payload = serde_json::json!({
            "id": "123",
            "name": "Test Event",
            "value": 42,
            "nested": {
                "field": "nested_value"
            },
            "items": ["a", "b", "c"],
            "customer": {
                "name": "John",
                "email": "john@example.com"
            }
        });

        let mut route = HashMap::new();
        route.insert("user_id".to_string(), "user_456".to_string());

        let mut query = HashMap::new();
        query.insert("filter".to_string(), "active".to_string());

        let mut headers = HashMap::new();
        headers.insert("X-Request-ID".to_string(), "req-789".to_string());

        TemplateContext {
            payload,
            route,
            query,
            headers,
            method: "POST".to_string(),
            path: "/webhooks/test".to_string(),
            source_id: "test-source".to_string(),
        }
    }

    #[test]
    fn test_simple_template_rendering() {
        let engine = TemplateEngine::new();
        let context = create_test_context();

        let result = engine.render_string("{{payload.name}}", &context).unwrap();
        assert_eq!(result, "Test Event");

        let result = engine.render_string("{{route.user_id}}", &context).unwrap();
        assert_eq!(result, "user_456");

        let result = engine.render_string("{{method}}", &context).unwrap();
        assert_eq!(result, "POST");
    }

    #[test]
    fn test_nested_path_rendering() {
        let engine = TemplateEngine::new();
        let context = create_test_context();

        let result = engine
            .render_string("{{payload.nested.field}}", &context)
            .unwrap();
        assert_eq!(result, "nested_value");
    }

    #[test]
    fn test_concatenation_template() {
        let engine = TemplateEngine::new();
        let context = create_test_context();

        let result = engine
            .render_string("event-{{payload.id}}-{{route.user_id}}", &context)
            .unwrap();
        assert_eq!(result, "event-123-user_456");
    }

    #[test]
    fn test_lowercase_helper() {
        let engine = TemplateEngine::new();
        let context = create_test_context();

        let result = engine
            .render_string("{{lowercase payload.name}}", &context)
            .unwrap();
        assert_eq!(result, "test event");
    }

    #[test]
    fn test_uppercase_helper() {
        let engine = TemplateEngine::new();
        let context = create_test_context();

        let result = engine
            .render_string("{{uppercase payload.name}}", &context)
            .unwrap();
        assert_eq!(result, "TEST EVENT");
    }

    #[test]
    fn test_concat_helper() {
        let engine = TemplateEngine::new();
        let context = create_test_context();

        let result = engine
            .render_string("{{concat payload.id \"-\" route.user_id}}", &context)
            .unwrap();
        assert_eq!(result, "123-user_456");
    }

    #[test]
    fn test_default_helper() {
        let engine = TemplateEngine::new();
        let context = create_test_context();

        let result = engine
            .render_string("{{default payload.missing \"fallback\"}}", &context)
            .unwrap();
        assert_eq!(result, "fallback");

        let result = engine
            .render_string("{{default payload.name \"fallback\"}}", &context)
            .unwrap();
        assert_eq!(result, "Test Event");
    }

    #[test]
    fn test_json_helper() {
        let engine = TemplateEngine::new();
        let context = create_test_context();

        let result = engine
            .render_string("{{json payload.customer}}", &context)
            .unwrap();
        assert!(result.contains("\"name\":\"John\""));
        assert!(result.contains("\"email\":\"john@example.com\""));
    }

    #[test]
    fn test_render_value_preserves_types() {
        let engine = TemplateEngine::new();
        let context = create_test_context();

        // Number should be preserved
        let result = engine.render_value("{{payload.value}}", &context).unwrap();
        assert_eq!(result, JsonValue::Number(42.into()));

        // Object should be preserved
        let result = engine
            .render_value("{{payload.customer}}", &context)
            .unwrap();
        assert!(result.is_object());
        assert_eq!(result["name"], "John");

        // Array should be preserved
        let result = engine.render_value("{{payload.items}}", &context).unwrap();
        assert!(result.is_array());
    }

    #[test]
    fn test_json_to_element_value() {
        let json = serde_json::json!({
            "string": "hello",
            "number": 42,
            "float": 3.15,
            "bool": true,
            "null": null,
            "array": [1, 2, 3],
            "object": {"key": "value"}
        });

        if let JsonValue::Object(obj) = json {
            let string_val = json_to_element_value(&obj["string"]).unwrap();
            assert!(matches!(string_val, ElementValue::String(_)));

            let num_val = json_to_element_value(&obj["number"]).unwrap();
            assert!(matches!(num_val, ElementValue::Integer(42)));

            let bool_val = json_to_element_value(&obj["bool"]).unwrap();
            assert!(matches!(bool_val, ElementValue::Bool(true)));

            let null_val = json_to_element_value(&obj["null"]).unwrap();
            assert!(matches!(null_val, ElementValue::Null));

            let arr_val = json_to_element_value(&obj["array"]).unwrap();
            assert!(matches!(arr_val, ElementValue::List(_)));

            let obj_val = json_to_element_value(&obj["object"]).unwrap();
            assert!(matches!(obj_val, ElementValue::Object(_)));
        }
    }

    #[test]
    fn test_process_mapping_insert() {
        let engine = TemplateEngine::new();
        let context = create_test_context();

        let mapping = WebhookMapping {
            when: None,
            operation: Some(OperationType::Insert),
            operation_from: None,
            operation_map: None,
            element_type: ElementType::Node,
            effective_from: None,
            template: ElementTemplate {
                id: "event-{{payload.id}}".to_string(),
                labels: vec!["Event".to_string(), "Test".to_string()],
                properties: Some(serde_json::json!({
                    "name": "{{payload.name}}",
                    "value": "{{payload.value}}"
                })),
                from: None,
                to: None,
            },
        };

        let result = engine
            .process_mapping(&mapping, &context, "test-source")
            .unwrap();

        match result {
            SourceChange::Insert { element } => match element {
                Element::Node {
                    metadata,
                    properties,
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "event-123");
                    assert_eq!(metadata.labels.len(), 2);
                    assert!(properties.get("name").is_some());
                }
                _ => panic!("Expected Node element"),
            },
            _ => panic!("Expected Insert operation"),
        }
    }

    #[test]
    fn test_process_mapping_relation() {
        let engine = TemplateEngine::new();
        let context = create_test_context();

        let mapping = WebhookMapping {
            when: None,
            operation: Some(OperationType::Insert),
            operation_from: None,
            operation_map: None,
            element_type: ElementType::Relation,
            effective_from: None,
            template: ElementTemplate {
                id: "rel-{{payload.id}}".to_string(),
                labels: vec!["LINKS_TO".to_string()],
                properties: None,
                from: Some("node-{{route.user_id}}".to_string()),
                to: Some("node-{{payload.id}}".to_string()),
            },
        };

        let result = engine
            .process_mapping(&mapping, &context, "test-source")
            .unwrap();

        match result {
            SourceChange::Insert { element } => match element {
                Element::Relation {
                    metadata,
                    in_node,
                    out_node,
                    ..
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "rel-123");
                    assert_eq!(out_node.element_id.as_ref(), "node-user_456");
                    assert_eq!(in_node.element_id.as_ref(), "node-123");
                }
                _ => panic!("Expected Relation element"),
            },
            _ => panic!("Expected Insert operation"),
        }
    }

    #[test]
    fn test_process_mapping_with_operation_map() {
        let engine = TemplateEngine::new();

        let payload = serde_json::json!({
            "id": "123",
            "action": "created"
        });

        let context = TemplateContext {
            payload,
            route: HashMap::new(),
            query: HashMap::new(),
            headers: HashMap::new(),
            method: "POST".to_string(),
            path: "/events".to_string(),
            source_id: "test".to_string(),
        };

        let mut operation_map = HashMap::new();
        operation_map.insert("created".to_string(), OperationType::Insert);
        operation_map.insert("updated".to_string(), OperationType::Update);
        operation_map.insert("deleted".to_string(), OperationType::Delete);

        let mapping = WebhookMapping {
            when: None,
            operation: None,
            operation_from: Some("payload.action".to_string()),
            operation_map: Some(operation_map),
            element_type: ElementType::Node,
            effective_from: None,
            template: ElementTemplate {
                id: "{{payload.id}}".to_string(),
                labels: vec!["Event".to_string()],
                properties: None,
                from: None,
                to: None,
            },
        };

        let result = engine.process_mapping(&mapping, &context, "test").unwrap();
        assert!(matches!(result, SourceChange::Insert { .. }));
    }
}
