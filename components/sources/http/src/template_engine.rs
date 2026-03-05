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
//! Uses Handlebars templates to transform webhook payloads into
//! Drasi source change events.

use crate::config::{
    EffectiveFromConfig, ElementTemplate, ElementType, OperationType, TimestampFormat,
    WebhookMapping,
};
use anyhow::{anyhow, Result};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use handlebars::{
    Context, Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason,
};
use ordered_float::OrderedFloat;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;

/// Template context containing all variables available in templates
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

/// Compiled template engine with pre-registered templates
pub struct TemplateEngine {
    handlebars: Handlebars<'static>,
}

impl TemplateEngine {
    /// Create a new template engine with custom helpers registered
    pub fn new() -> Self {
        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(false);

        // Register custom helpers
        register_helpers(&mut handlebars);

        Self { handlebars }
    }

    /// Render a template string with the given context
    pub fn render_string(&self, template: &str, context: &TemplateContext) -> Result<String> {
        self.handlebars
            .render_template(template, context)
            .map_err(|e| anyhow!("Template render error: {e}"))
    }

    /// Render a template and preserve the JSON value type
    ///
    /// If the template is a simple variable reference like `{{payload.field}}`,
    /// this returns the original JSON value. Otherwise, it returns the rendered string.
    pub fn render_value(&self, template: &str, context: &TemplateContext) -> Result<JsonValue> {
        // Check if this is a simple variable reference
        if let Some(path) = extract_simple_path(template) {
            if let Some(value) = resolve_path(&context_to_json(context), &path) {
                return Ok(value.clone());
            }
        }

        // Fall back to string rendering
        let rendered = self.render_string(template, context)?;

        // Try to parse as JSON, otherwise return as string
        if rendered.is_empty() {
            Ok(JsonValue::Null)
        } else if let Ok(parsed) = serde_json::from_str::<JsonValue>(&rendered) {
            Ok(parsed)
        } else {
            Ok(JsonValue::String(rendered))
        }
    }

    /// Process a webhook mapping and create a SourceChange
    pub fn process_mapping(
        &self,
        mapping: &WebhookMapping,
        context: &TemplateContext,
        source_id: &str,
    ) -> Result<SourceChange> {
        // Determine the operation
        let operation = self.resolve_operation(mapping, context)?;

        // Get effective_from timestamp
        let effective_from = self.resolve_effective_from(mapping, context)?;

        // Build the element based on type
        let element = self.build_element(mapping, context, source_id, effective_from)?;

        // Create the appropriate SourceChange
        match operation {
            OperationType::Insert => Ok(SourceChange::Insert { element }),
            OperationType::Update => Ok(SourceChange::Update { element }),
            OperationType::Delete => {
                // For delete, we only need metadata
                let metadata = match element {
                    Element::Node { metadata, .. } => metadata,
                    Element::Relation { metadata, .. } => metadata,
                };
                Ok(SourceChange::Delete { metadata })
            }
        }
    }

    /// Resolve the operation type from mapping configuration
    fn resolve_operation(
        &self,
        mapping: &WebhookMapping,
        context: &TemplateContext,
    ) -> Result<OperationType> {
        // If static operation is defined, use it
        if let Some(ref op) = mapping.operation {
            return Ok(op.clone());
        }

        // Otherwise, extract from payload using operation_from
        let op_path = mapping
            .operation_from
            .as_ref()
            .ok_or_else(|| anyhow!("No operation or operation_from specified"))?;

        let op_map = mapping
            .operation_map
            .as_ref()
            .ok_or_else(|| anyhow!("operation_map required when using operation_from"))?;

        // Resolve the path value
        let context_json = context_to_json(context);
        let value = resolve_path(&context_json, op_path)
            .ok_or_else(|| anyhow!("operation_from path '{op_path}' not found in context"))?;

        let value_str = match value {
            JsonValue::String(s) => s.clone(),
            JsonValue::Number(n) => n.to_string(),
            JsonValue::Bool(b) => b.to_string(),
            _ => return Err(anyhow!("operation_from value must be a string or number")),
        };

        op_map
            .get(&value_str)
            .cloned()
            .ok_or_else(|| anyhow!("No operation mapping found for value '{value_str}'"))
    }

    /// Resolve effective_from timestamp
    fn resolve_effective_from(
        &self,
        mapping: &WebhookMapping,
        context: &TemplateContext,
    ) -> Result<u64> {
        let Some(ref config) = mapping.effective_from else {
            return Ok(current_time_millis());
        };

        let (template, format) = match config {
            EffectiveFromConfig::Simple(t) => (t.as_str(), None),
            EffectiveFromConfig::Explicit { value, format } => (value.as_str(), Some(format)),
        };

        let rendered = self.render_string(template, context)?;
        if rendered.is_empty() {
            return Ok(current_time_millis());
        }

        parse_timestamp(&rendered, format)
    }

    /// Build an Element from the template
    fn build_element(
        &self,
        mapping: &WebhookMapping,
        context: &TemplateContext,
        source_id: &str,
        effective_from: u64,
    ) -> Result<Element> {
        let template = &mapping.template;

        // Render ID
        let id = self.render_string(&template.id, context)?;
        if id.is_empty() {
            return Err(anyhow!("Template rendered empty ID"));
        }

        // Render labels
        let labels: Result<Vec<Arc<str>>> = template
            .labels
            .iter()
            .map(|l| {
                let rendered = self.render_string(l, context)?;
                Ok(Arc::from(rendered.as_str()))
            })
            .collect();
        let labels = labels?;

        // Build metadata
        let metadata = ElementMetadata {
            reference: ElementReference {
                source_id: Arc::from(source_id),
                element_id: Arc::from(id.as_str()),
            },
            labels: Arc::from(labels),
            effective_from,
        };

        // Render properties
        let properties = self.render_properties(template, context)?;

        match mapping.element_type {
            ElementType::Node => Ok(Element::Node {
                metadata,
                properties,
            }),
            ElementType::Relation => {
                let from_template = template
                    .from
                    .as_ref()
                    .ok_or_else(|| anyhow!("Relation template missing 'from' field"))?;
                let to_template = template
                    .to
                    .as_ref()
                    .ok_or_else(|| anyhow!("Relation template missing 'to' field"))?;

                let from_id = self.render_string(from_template, context)?;
                let to_id = self.render_string(to_template, context)?;

                Ok(Element::Relation {
                    metadata,
                    properties,
                    in_node: ElementReference {
                        source_id: Arc::from(source_id),
                        element_id: Arc::from(to_id.as_str()),
                    },
                    out_node: ElementReference {
                        source_id: Arc::from(source_id),
                        element_id: Arc::from(from_id.as_str()),
                    },
                })
            }
        }
    }

    /// Render properties from template
    fn render_properties(
        &self,
        template: &ElementTemplate,
        context: &TemplateContext,
    ) -> Result<ElementPropertyMap> {
        let mut props = ElementPropertyMap::new();

        let Some(ref prop_value) = template.properties else {
            return Ok(props);
        };

        match prop_value {
            JsonValue::Object(obj) => {
                for (key, value) in obj {
                    let rendered = self.render_property_value(value, context)?;
                    props.insert(key, rendered);
                }
            }
            JsonValue::String(template_str) => {
                // Single template that should resolve to an object
                let rendered = self.render_value(template_str, context)?;
                if let JsonValue::Object(obj) = rendered {
                    for (key, value) in obj {
                        props.insert(&key, json_to_element_value(&value)?);
                    }
                }
            }
            _ => {
                return Err(anyhow!("Properties must be an object or a template string"));
            }
        }

        Ok(props)
    }

    /// Render a single property value
    fn render_property_value(
        &self,
        value: &JsonValue,
        context: &TemplateContext,
    ) -> Result<ElementValue> {
        match value {
            JsonValue::String(template) => {
                let rendered = self.render_value(template, context)?;
                json_to_element_value(&rendered)
            }
            JsonValue::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Ok(ElementValue::Integer(i))
                } else if let Some(f) = n.as_f64() {
                    Ok(ElementValue::Float(OrderedFloat(f)))
                } else {
                    Err(anyhow!("Invalid number"))
                }
            }
            JsonValue::Bool(b) => Ok(ElementValue::Bool(*b)),
            JsonValue::Null => Ok(ElementValue::Null),
            JsonValue::Array(arr) => {
                let items: Result<Vec<_>> = arr
                    .iter()
                    .map(|v| self.render_property_value(v, context))
                    .collect();
                Ok(ElementValue::List(items?))
            }
            JsonValue::Object(obj) => {
                let mut map = ElementPropertyMap::new();
                for (k, v) in obj {
                    map.insert(k, self.render_property_value(v, context)?);
                }
                Ok(ElementValue::Object(map))
            }
        }
    }
}

impl Default for TemplateEngine {
    fn default() -> Self {
        Self::new()
    }
}

/// Register custom Handlebars helpers
fn register_helpers(handlebars: &mut Handlebars) {
    // lowercase helper
    handlebars.register_helper(
        "lowercase",
        Box::new(
            |h: &Helper,
             _: &Handlebars,
             _: &Context,
             _: &mut RenderContext,
             out: &mut dyn Output|
             -> HelperResult {
                let param = h
                    .param(0)
                    .ok_or(RenderErrorReason::ParamNotFoundForIndex("lowercase", 0))?;
                let value = param.value().as_str().unwrap_or("");
                out.write(&value.to_lowercase())?;
                Ok(())
            },
        ),
    );

    // uppercase helper
    handlebars.register_helper(
        "uppercase",
        Box::new(
            |h: &Helper,
             _: &Handlebars,
             _: &Context,
             _: &mut RenderContext,
             out: &mut dyn Output|
             -> HelperResult {
                let param = h
                    .param(0)
                    .ok_or(RenderErrorReason::ParamNotFoundForIndex("uppercase", 0))?;
                let value = param.value().as_str().unwrap_or("");
                out.write(&value.to_uppercase())?;
                Ok(())
            },
        ),
    );

    // now helper - returns current timestamp in milliseconds
    handlebars.register_helper(
        "now",
        Box::new(
            |_: &Helper,
             _: &Handlebars,
             _: &Context,
             _: &mut RenderContext,
             out: &mut dyn Output|
             -> HelperResult {
                out.write(&current_time_millis().to_string())?;
                Ok(())
            },
        ),
    );

    // concat helper
    handlebars.register_helper(
        "concat",
        Box::new(
            |h: &Helper,
             _: &Handlebars,
             _: &Context,
             _: &mut RenderContext,
             out: &mut dyn Output|
             -> HelperResult {
                let mut result = String::new();
                for param in h.params() {
                    if let Some(s) = param.value().as_str() {
                        result.push_str(s);
                    } else {
                        result.push_str(&param.value().to_string());
                    }
                }
                out.write(&result)?;
                Ok(())
            },
        ),
    );

    // default helper
    handlebars.register_helper(
        "default",
        Box::new(
            |h: &Helper,
             _: &Handlebars,
             _: &Context,
             _: &mut RenderContext,
             out: &mut dyn Output|
             -> HelperResult {
                let value = h.param(0).map(|p| p.value());
                let default = h.param(1).map(|p| p.value());

                let output = match value {
                    Some(v) if !v.is_null() && v.as_str() != Some("") => v,
                    _ => default.unwrap_or(&JsonValue::Null),
                };

                if let Some(s) = output.as_str() {
                    out.write(s)?;
                } else {
                    out.write(&output.to_string())?;
                }
                Ok(())
            },
        ),
    );

    // json helper - serialize value to JSON string
    handlebars.register_helper(
        "json",
        Box::new(
            |h: &Helper,
             _: &Handlebars,
             _: &Context,
             _: &mut RenderContext,
             out: &mut dyn Output|
             -> HelperResult {
                let param = h
                    .param(0)
                    .ok_or(RenderErrorReason::ParamNotFoundForIndex("json", 0))?;
                let json_str =
                    serde_json::to_string(param.value()).unwrap_or_else(|_| "null".to_string());
                out.write(&json_str)?;
                Ok(())
            },
        ),
    );
}

/// Extract a simple variable path from a template like `{{payload.field}}`
fn extract_simple_path(template: &str) -> Option<String> {
    let trimmed = template.trim();
    if trimmed.starts_with("{{") && trimmed.ends_with("}}") {
        let inner = trimmed[2..trimmed.len() - 2].trim();
        // Check if it's a simple path (no spaces, no helpers)
        if !inner.contains(' ') && !inner.contains('#') && !inner.contains('/') {
            return Some(inner.to_string());
        }
    }
    None
}

/// Resolve a dot-separated path in a JSON value
fn resolve_path<'a>(value: &'a JsonValue, path: &str) -> Option<&'a JsonValue> {
    let mut current = value;
    for part in path.split('.') {
        current = match current {
            JsonValue::Object(obj) => obj.get(part)?,
            JsonValue::Array(arr) => {
                let index: usize = part.parse().ok()?;
                arr.get(index)?
            }
            _ => return None,
        };
    }
    Some(current)
}

/// Convert TemplateContext to JSON value for path resolution
fn context_to_json(context: &TemplateContext) -> JsonValue {
    serde_json::to_value(context).unwrap_or(JsonValue::Null)
}

/// Convert JSON value to ElementValue
pub fn json_to_element_value(value: &JsonValue) -> Result<ElementValue> {
    match value {
        JsonValue::Null => Ok(ElementValue::Null),
        JsonValue::Bool(b) => Ok(ElementValue::Bool(*b)),
        JsonValue::Number(n) => {
            if let Some(i) = n.as_i64() {
                Ok(ElementValue::Integer(i))
            } else if let Some(f) = n.as_f64() {
                Ok(ElementValue::Float(OrderedFloat(f)))
            } else {
                Err(anyhow!("Invalid number value"))
            }
        }
        JsonValue::String(s) => Ok(ElementValue::String(Arc::from(s.as_str()))),
        JsonValue::Array(arr) => {
            let items: Result<Vec<_>> = arr.iter().map(json_to_element_value).collect();
            Ok(ElementValue::List(items?))
        }
        JsonValue::Object(obj) => {
            let mut map = ElementPropertyMap::new();
            for (k, v) in obj {
                map.insert(k, json_to_element_value(v)?);
            }
            Ok(ElementValue::Object(map))
        }
    }
}

/// Get current time in milliseconds
fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Parse a timestamp string into milliseconds since epoch
fn parse_timestamp(value: &str, format: Option<&TimestampFormat>) -> Result<u64> {
    if let Some(fmt) = format {
        return parse_with_format(value, fmt);
    }

    // Auto-detect format
    let trimmed = value.trim();

    // Try ISO 8601 first (contains 'T' or '-')
    if trimmed.contains('T') || (trimmed.contains('-') && !trimmed.starts_with('-')) {
        if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(trimmed) {
            return Ok(dt.timestamp_millis() as u64);
        }
        // Try without timezone
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S") {
            return Ok(dt.and_utc().timestamp_millis() as u64);
        }
        if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f") {
            return Ok(dt.and_utc().timestamp_millis() as u64);
        }
    }

    // Try parsing as number
    if let Ok(num) = trimmed.parse::<i64>() {
        let abs = num.unsigned_abs();
        // Heuristic based on magnitude
        if abs < 10_000_000_000 {
            // Seconds (before year 2286)
            return Ok(abs * 1000);
        } else if abs < 10_000_000_000_000 {
            // Milliseconds
            return Ok(abs);
        } else {
            // Nanoseconds
            return Ok(abs / 1_000_000);
        }
    }

    Err(anyhow!(
        "Unable to parse timestamp '{value}'. Expected ISO 8601 or Unix timestamp"
    ))
}

/// Parse timestamp with explicit format
fn parse_with_format(value: &str, format: &TimestampFormat) -> Result<u64> {
    match format {
        TimestampFormat::Iso8601 => {
            let dt = chrono::DateTime::parse_from_rfc3339(value.trim())
                .map_err(|e| anyhow!("Invalid ISO 8601 timestamp: {e}"))?;
            Ok(dt.timestamp_millis() as u64)
        }
        TimestampFormat::UnixSeconds => {
            let secs: i64 = value
                .trim()
                .parse()
                .map_err(|e| anyhow!("Invalid Unix seconds: {e}"))?;
            Ok((secs * 1000) as u64)
        }
        TimestampFormat::UnixMillis => {
            let millis: u64 = value
                .trim()
                .parse()
                .map_err(|e| anyhow!("Invalid Unix milliseconds: {e}"))?;
            Ok(millis)
        }
        TimestampFormat::UnixNanos => {
            let nanos: u64 = value
                .trim()
                .parse()
                .map_err(|e| anyhow!("Invalid Unix nanoseconds: {e}"))?;
            Ok(nanos / 1_000_000)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ElementTemplate, WebhookMapping};

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
    fn test_parse_timestamp_iso8601() {
        let result = parse_timestamp("2024-01-15T10:30:00Z", None).unwrap();
        assert!(result > 0);

        let result =
            parse_timestamp("2024-01-15T10:30:00Z", Some(&TimestampFormat::Iso8601)).unwrap();
        assert!(result > 0);
    }

    #[test]
    fn test_parse_timestamp_unix_seconds() {
        let result = parse_timestamp("1705315800", None).unwrap();
        assert_eq!(result, 1705315800000); // Converted to millis

        let result = parse_timestamp("1705315800", Some(&TimestampFormat::UnixSeconds)).unwrap();
        assert_eq!(result, 1705315800000);
    }

    #[test]
    fn test_parse_timestamp_unix_millis() {
        let result = parse_timestamp("1705315800000", None).unwrap();
        assert_eq!(result, 1705315800000);

        let result = parse_timestamp("1705315800000", Some(&TimestampFormat::UnixMillis)).unwrap();
        assert_eq!(result, 1705315800000);
    }

    #[test]
    fn test_parse_timestamp_unix_nanos() {
        let result = parse_timestamp("1705315800000000000", None).unwrap();
        assert_eq!(result, 1705315800000); // Converted to millis

        let result =
            parse_timestamp("1705315800000000000", Some(&TimestampFormat::UnixNanos)).unwrap();
        assert_eq!(result, 1705315800000);
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

    #[test]
    fn test_extract_simple_path() {
        assert_eq!(
            extract_simple_path("{{payload.id}}"),
            Some("payload.id".to_string())
        );
        assert_eq!(
            extract_simple_path("{{ payload.id }}"),
            Some("payload.id".to_string())
        );
        assert_eq!(extract_simple_path("{{#if condition}}"), None);
        assert_eq!(extract_simple_path("prefix-{{id}}"), None);
        assert_eq!(extract_simple_path("{{lowercase name}}"), None);
    }
}
