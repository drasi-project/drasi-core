// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The one template engine for HTTP source mappings, shared by webhook
//! routes and the REST bootstrap reader (HTA-14).
//!
//! It delegates to the shared [`SourceMappingEngine`] (`drasi-source-mapping`):
//! the same Handlebars configuration, helpers (`lowercase`, `uppercase`,
//! `concat`, `default`, `json`, ...) and value rules apply on both paths:
//!
//! - a template that is a single path (`{{payload.id}}`, `{{item.count}}`)
//!   yields the referenced JSON value with its type;
//! - any other template renders to text, which becomes JSON when it parses as
//!   JSON (`"42"` is a number) and a string otherwise;
//! - an empty rendering (for example a missing field) is `null`.
//!
//! Only the variables differ: a webhook renders against a [`WebhookContext`]
//! and a bootstrap item against a [`BootstrapItemContext`].

use anyhow::{anyhow, Result};
use drasi_core_types::models::ElementValue;
#[cfg(feature = "webhook")]
use drasi_core_types::models::SourceChange;
use drasi_core_types::ComponentId;
pub use drasi_source_mapping::json_to_element_value;
use drasi_source_mapping::SourceMappingEngine;
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

/// The variables of a webhook request.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookContext {
    /// Parsed payload body.
    pub payload: JsonValue,
    /// Path parameters extracted from the route.
    pub route: HashMap<String, String>,
    /// Query parameters.
    pub query: HashMap<String, String>,
    /// HTTP headers.
    pub headers: HashMap<String, String>,
    /// HTTP method.
    pub method: String,
    /// Request path.
    pub path: String,
    /// Actual component owner, independent of the output data namespace.
    pub source_id: ComponentId,
}

/// The variables of one item of a bootstrap response page.
#[derive(Debug, Clone, Serialize)]
pub struct BootstrapItemContext {
    /// The current item being mapped.
    pub item: JsonValue,
    /// The index of the current item in the page.
    pub index: usize,
    /// Source ID.
    pub source_id: ComponentId,
}

/// The shared HTTP template engine.
pub struct TemplateEngine {
    inner: SourceMappingEngine,
}

impl Default for TemplateEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TemplateEngine {
    pub fn new() -> Self {
        Self {
            inner: SourceMappingEngine::new(),
        }
    }

    /// Renders a template to text.
    pub fn render_string(&self, template: &str, context: &impl Serialize) -> Result<String> {
        self.inner
            .render_string(template, &context_to_json(context)?)
    }

    /// Renders a template to a JSON value under the module's value rules.
    pub fn render_value(&self, template: &str, context: &impl Serialize) -> Result<JsonValue> {
        self.inner
            .render_value(template, &context_to_json(context)?)
    }

    /// Renders every string of a JSON property specification as a template,
    /// recursively; other literals pass through.
    pub fn render_properties(
        &self,
        properties: &JsonValue,
        context: &impl Serialize,
    ) -> Result<HashMap<String, JsonValue>> {
        let JsonValue::Object(map) = properties else {
            return Err(anyhow!("Properties must be a JSON object"));
        };
        let context = context_to_json(context)?;
        map.iter()
            .map(|(key, value)| Ok((key.clone(), self.render_json(value, &context)?)))
            .collect()
    }

    fn render_json(&self, value: &JsonValue, context: &JsonValue) -> Result<JsonValue> {
        match value {
            JsonValue::String(template) => self.inner.render_value(template, context),
            JsonValue::Object(map) => Ok(JsonValue::Object(
                map.iter()
                    .map(|(key, value)| Ok((key.clone(), self.render_json(value, context)?)))
                    .collect::<Result<_>>()?,
            )),
            JsonValue::Array(items) => Ok(JsonValue::Array(
                items
                    .iter()
                    .map(|value| self.render_json(value, context))
                    .collect::<Result<_>>()?,
            )),
            other => Ok(other.clone()),
        }
    }

    /// Processes a webhook mapping into a source change.
    #[cfg(feature = "webhook")]
    pub fn process_mapping(
        &self,
        mapping: &crate::webhook::config::WebhookMapping,
        context: &WebhookContext,
        namespace: &str,
    ) -> Result<SourceChange> {
        self.inner
            .process_mapping(mapping, &context_to_json(context)?, namespace)
    }

    /// Renders one property value to an element value.
    pub fn render_property_value(
        &self,
        value: &JsonValue,
        context: &impl Serialize,
    ) -> Result<ElementValue> {
        match value {
            JsonValue::String(template) => {
                json_to_element_value(&self.render_value(template, context)?)
            }
            other => json_to_element_value(other),
        }
    }
}

fn context_to_json(context: &impl Serialize) -> Result<JsonValue> {
    serde_json::to_value(context).map_err(|error| anyhow!("template context: {error}"))
}

#[cfg(all(test, feature = "webhook"))]
mod webhook_tests {
    use super::*;
    use crate::webhook::config::{
        ElementTemplate, ElementType, OperationType, TimestampFormat, WebhookMapping,
    };
    use drasi_core_types::models::{Element, ElementValue, SourceChange};

    fn create_test_context() -> WebhookContext {
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

        WebhookContext {
            payload,
            route,
            query,
            headers,
            method: "POST".to_string(),
            path: "/webhooks/test".to_string(),
            source_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-00000000001d",
            )
            .expect("fixture UUID"),
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

        let context = WebhookContext {
            payload,
            route: HashMap::new(),
            query: HashMap::new(),
            headers: HashMap::new(),
            method: "POST".to_string(),
            path: "/events".to_string(),
            source_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-000000000051",
            )
            .expect("fixture UUID"),
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

#[cfg(all(test, feature = "bootstrap"))]
mod bootstrap_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_render_simple_template() {
        let engine = TemplateEngine::new();
        let context = BootstrapItemContext {
            item: json!({"id": "123", "name": "Alice"}),
            index: 0,
            source_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-00000000001d",
            )
            .expect("fixture UUID"),
        };

        let result = engine.render_string("{{item.id}}", &context).unwrap();
        assert_eq!(result, "123");

        let result = engine.render_string("{{item.name}}", &context).unwrap();
        assert_eq!(result, "Alice");
    }

    #[test]
    fn test_render_value_preserves_types() {
        let engine = TemplateEngine::new();
        let context = BootstrapItemContext {
            item: json!({"id": "str-123", "count": 42, "rate": 3.15, "active": true}),
            index: 0,
            source_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-00000000001d",
            )
            .expect("fixture UUID"),
        };

        // Simple references preserve original type
        assert_eq!(
            engine.render_value("{{item.id}}", &context).unwrap(),
            json!("str-123")
        );
        assert_eq!(
            engine.render_value("{{item.count}}", &context).unwrap(),
            json!(42)
        );
        assert_eq!(
            engine.render_value("{{item.rate}}", &context).unwrap(),
            json!(3.15)
        );
        assert_eq!(
            engine.render_value("{{item.active}}", &context).unwrap(),
            json!(true)
        );
    }

    #[test]
    fn test_render_value_complex_template_returns_string() {
        let engine = TemplateEngine::new();
        let context = BootstrapItemContext {
            item: json!({"id": 42, "prefix": "user"}),
            index: 0,
            source_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-00000000001d",
            )
            .expect("fixture UUID"),
        };

        // Complex templates always produce strings
        let result = engine
            .render_value("{{item.prefix}}-{{item.id}}", &context)
            .unwrap();
        assert_eq!(result, json!("user-42"));
    }

    #[test]
    fn test_render_properties_preserves_types() {
        let engine = TemplateEngine::new();
        let context = BootstrapItemContext {
            item: json!({"id": "123", "name": "Alice", "age": 30}),
            index: 0,
            source_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-00000000001d",
            )
            .expect("fixture UUID"),
        };

        let props = json!({
            "name": "{{item.name}}",
            "age": "{{item.age}}"
        });

        let result = engine.render_properties(&props, &context).unwrap();
        assert_eq!(result["name"], json!("Alice"));
        assert_eq!(result["age"], json!(30)); // preserved as integer
    }

    #[test]
    fn test_render_missing_field() {
        let engine = TemplateEngine::new();
        let context = BootstrapItemContext {
            item: json!({"id": "123"}),
            index: 0,
            source_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-00000000001d",
            )
            .expect("fixture UUID"),
        };

        // Non-strict mode: missing fields render as empty string
        let result = engine
            .render_string("{{item.nonexistent}}", &context)
            .unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_string_value_not_coerced_to_int() {
        let engine = TemplateEngine::new();
        let context = BootstrapItemContext {
            item: json!({"id": "42"}), // String "42", not integer 42
            index: 0,
            source_id: drasi_core_types::ComponentId::try_new(
                "00000000-0000-0000-0000-00000000001d",
            )
            .expect("fixture UUID"),
        };

        // Should stay as string since the original value is a string
        let result = engine.render_value("{{item.id}}", &context).unwrap();
        assert_eq!(result, json!("42"));
        assert!(result.is_string());
    }
}

/// One XML document reaches templates as the same value whether it arrives
/// as a webhook body or a bootstrap response (HTA-14).
#[cfg(all(test, feature = "webhook", feature = "bootstrap"))]
mod parity_tests {
    use super::*;
    use crate::content::{parse_content, ContentType};
    use serde_json::json;

    const XML: &str = r#"<orders><order id="7"><sku>A&amp;B</sku><qty>3</qty></order><order id="8"><sku>C</sku><qty>1</qty></order></orders>"#;

    fn source_id() -> ComponentId {
        ComponentId::try_new("00000000-0000-0000-0000-00000000001d").expect("fixture UUID")
    }

    #[test]
    fn one_xml_document_renders_identically_on_both_paths() {
        let engine = TemplateEngine::new();
        let document = parse_content(XML.as_bytes(), ContentType::Xml).expect("valid XML");
        // The bootstrap reader extracts the items at `order`.
        let items = crate::bootstrap::response::extract_items(&document, "order").expect("items");
        assert_eq!(items.len(), 2);
        for (index, item) in items.iter().enumerate() {
            let bootstrap = BootstrapItemContext {
                item: item.clone(),
                index,
                source_id: source_id(),
            };
            let webhook = WebhookContext {
                payload: document["order"][index].clone(),
                route: HashMap::new(),
                query: HashMap::new(),
                headers: HashMap::new(),
                method: "POST".into(),
                path: "/hooks".into(),
                source_id: source_id(),
            };
            for (bootstrap_template, webhook_template) in [
                ("{{item.@id}}", "{{payload.@id}}"),
                ("{{item.sku}}", "{{payload.sku}}"),
                ("{{item.qty}}", "{{payload.qty}}"),
                (
                    r#"order-{{lookup item "@id"}}"#,
                    r#"order-{{lookup payload "@id"}}"#,
                ),
                ("{{lowercase item.sku}}", "{{lowercase payload.sku}}"),
            ] {
                assert_eq!(
                    engine
                        .render_value(bootstrap_template, &bootstrap)
                        .expect("bootstrap"),
                    engine
                        .render_value(webhook_template, &webhook)
                        .expect("webhook"),
                    "{bootstrap_template}"
                );
            }
        }
        // Element text is a plain string, never quick-xml's {"$text": ...}.
        assert_eq!(items[0]["sku"], json!("A&B"));
        assert_eq!(items[0]["qty"], json!("3"));
        assert_eq!(items[0]["@id"], json!("7"));
    }
}
