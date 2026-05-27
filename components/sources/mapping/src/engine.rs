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

//! Source mapping engine using Handlebars templates.

use crate::config::{
    EffectiveFromConfig, ElementTemplate, ElementType, MappingCondition, OperationType,
    SourceMapping, TimestampFormat,
};
use anyhow::{anyhow, Result};
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, ElementValue, SourceChange,
};
use handlebars::{
    Context, Handlebars, Helper, HelperResult, Output, RenderContext, RenderErrorReason,
};
use ordered_float::OrderedFloat;
use regex::Regex;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::sync::Arc;

/// Source mapping engine that transforms payloads into graph change events.
///
/// Uses Handlebars templates to extract element IDs, labels, properties,
/// and operations from arbitrary JSON contexts.
///
/// The context is a `serde_json::Value` that each source builds with its own
/// variables. For example:
/// - HTTP source: `{ "payload": ..., "headers": ..., "route": ..., "query": ... }`
/// - Kafka source: `{ "payload": ..., "key": ..., "topic": ..., "partition": ..., "offset": ... }`
pub struct SourceMappingEngine {
    handlebars: Handlebars<'static>,
    regex_cache: std::sync::Mutex<HashMap<String, Regex>>,
}

impl SourceMappingEngine {
    /// Create a new mapping engine with custom helpers registered
    pub fn new() -> Self {
        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(false);

        register_helpers(&mut handlebars);

        Self {
            handlebars,
            regex_cache: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Render a template string with the given context
    pub fn render_string(&self, template: &str, context: &JsonValue) -> Result<String> {
        self.handlebars
            .render_template(template, context)
            .map_err(|e| anyhow!("Template render error: {e}"))
    }

    /// Render a template and preserve the JSON value type
    ///
    /// If the template is a simple variable reference like `{{payload.field}}`,
    /// this returns the original JSON value. Otherwise, it returns the rendered string.
    pub fn render_value(&self, template: &str, context: &JsonValue) -> Result<JsonValue> {
        // Check if this is a simple variable reference
        if let Some(path) = extract_simple_path(template) {
            if let Some(value) = resolve_path(context, &path) {
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

    /// Process a source mapping and create a SourceChange.
    ///
    /// The `context` should be a JSON object containing all variables available
    /// to templates (e.g., payload, key, topic, headers, etc.).
    pub fn process_mapping(
        &self,
        mapping: &SourceMapping,
        context: &JsonValue,
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

    /// Check if a mapping condition matches the given context.
    ///
    /// The `headers` parameter is optional and only used for header-based conditions.
    pub fn condition_matches(
        &self,
        condition: &MappingCondition,
        context: &JsonValue,
        headers: Option<&std::collections::HashMap<String, String>>,
    ) -> bool {
        let value = if let Some(ref header_name) = condition.header {
            // Check headers
            headers
                .and_then(|h| h.get(header_name))
                .map(|v| JsonValue::String(v.clone()))
        } else if let Some(ref field_path) = condition.field {
            // Check context field (look in payload by default)
            let lookup_path = if field_path.starts_with("payload.")
                || field_path.starts_with("key")
                || field_path.starts_with("topic")
                || field_path.starts_with("headers.")
                || field_path.starts_with("partition")
                || field_path.starts_with("offset")
                || field_path.starts_with("source_id")
            {
                field_path.clone()
            } else {
                format!("payload.{field_path}")
            };
            resolve_path(context, &lookup_path).cloned()
        } else {
            return false;
        };

        let Some(value) = value else {
            return false;
        };

        let value_str = match &value {
            JsonValue::String(s) => s.clone(),
            JsonValue::Number(n) => n.to_string(),
            JsonValue::Bool(b) => b.to_string(),
            _ => serde_json::to_string(&value).unwrap_or_default(),
        };

        if let Some(ref equals) = condition.equals {
            return value_str == *equals;
        }

        if let Some(ref contains) = condition.contains {
            return value_str.contains(contains.as_str());
        }

        if let Some(ref regex_str) = condition.regex {
            if let Ok(mut cache) = self.regex_cache.lock() {
                let re = cache.entry(regex_str.clone()).or_insert_with(|| {
                    regex::Regex::new(regex_str)
                        .unwrap_or_else(|_| regex::Regex::new("(?:)").expect("infallible"))
                });
                return re.is_match(&value_str);
            }
            // Mutex poisoned — fall back to one-shot compile
            if let Ok(re) = regex::Regex::new(regex_str) {
                return re.is_match(&value_str);
            }
        }

        false
    }

    /// Find the first matching mapping from a list based on conditions.
    ///
    /// Returns `None` if no mapping matches.
    pub fn find_matching_mapping<'a>(
        &self,
        mappings: &'a [SourceMapping],
        context: &JsonValue,
        headers: Option<&std::collections::HashMap<String, String>>,
    ) -> Option<&'a SourceMapping> {
        for mapping in mappings {
            if let Some(ref condition) = mapping.when {
                if self.condition_matches(condition, context, headers) {
                    return Some(mapping);
                }
            } else {
                // No condition means always matches
                return Some(mapping);
            }
        }
        None
    }

    /// Resolve the operation type from mapping configuration
    fn resolve_operation(
        &self,
        mapping: &SourceMapping,
        context: &JsonValue,
    ) -> Result<OperationType> {
        // If static operation is defined, use it
        if let Some(ref op) = mapping.operation {
            return Ok(op.clone());
        }

        // Otherwise, extract from context using operation_from
        let op_path = mapping
            .operation_from
            .as_ref()
            .ok_or_else(|| anyhow!("No operation or operation_from specified"))?;

        let op_map = mapping
            .operation_map
            .as_ref()
            .ok_or_else(|| anyhow!("operation_map required when using operation_from"))?;

        // Resolve the path value
        let value = resolve_path(context, op_path)
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
    fn resolve_effective_from(&self, mapping: &SourceMapping, context: &JsonValue) -> Result<u64> {
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
        mapping: &SourceMapping,
        context: &JsonValue,
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
        context: &JsonValue,
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
        context: &JsonValue,
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

impl Default for SourceMappingEngine {
    fn default() -> Self {
        Self::new()
    }
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
            if secs < 0 {
                return Err(anyhow!("Negative Unix timestamp not supported: {secs}"));
            }
            Ok((secs as u64) * 1000)
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
    use crate::config::{ElementTemplate, SourceMapping};

    fn create_test_context() -> JsonValue {
        serde_json::json!({
            "payload": {
                "id": "123",
                "name": "Test Order",
                "customer": "Alice",
                "total": 150,
                "status": "pending",
                "metadata": { "source": "webhook" }
            },
            "key": "order-123",
            "topic": "orders",
            "partition": 0,
            "offset": 42,
            "source_id": "test-source"
        })
    }

    #[test]
    fn test_render_simple_template() {
        let engine = SourceMappingEngine::new();
        let context = create_test_context();

        let result = engine.render_string("{{payload.id}}", &context).unwrap();
        assert_eq!(result, "123");
    }

    #[test]
    fn test_render_value_preserves_type() {
        let engine = SourceMappingEngine::new();
        let context = create_test_context();

        let result = engine.render_value("{{payload.total}}", &context).unwrap();
        assert_eq!(result, JsonValue::Number(150.into()));
    }

    #[test]
    fn test_render_value_preserves_object() {
        let engine = SourceMappingEngine::new();
        let context = create_test_context();

        let result = engine
            .render_value("{{payload.metadata}}", &context)
            .unwrap();
        assert_eq!(result, serde_json::json!({"source": "webhook"}));
    }

    #[test]
    fn test_process_mapping_node_insert() {
        let engine = SourceMappingEngine::new();
        let context = create_test_context();

        let mapping = SourceMapping {
            when: None,
            operation: Some(OperationType::Insert),
            operation_from: None,
            operation_map: None,
            element_type: ElementType::Node,
            effective_from: None,
            template: ElementTemplate {
                id: "{{key}}".to_string(),
                labels: vec!["Order".to_string()],
                properties: Some(JsonValue::String("{{payload}}".to_string())),
                from: None,
                to: None,
            },
        };

        let result = engine
            .process_mapping(&mapping, &context, "test-source")
            .unwrap();
        match result {
            SourceChange::Insert { element } => {
                match element {
                    Element::Node {
                        metadata,
                        properties,
                    } => {
                        assert_eq!(metadata.reference.element_id.as_ref(), "order-123");
                        assert_eq!(metadata.labels[0].as_ref(), "Order");
                        // Properties should contain fields from payload
                        assert!(properties.get("customer").is_some());
                        assert!(properties.get("total").is_some());
                    }
                    _ => panic!("Expected Node element"),
                }
            }
            _ => panic!("Expected Insert"),
        }
    }

    #[test]
    fn test_process_mapping_with_operation_from() {
        let engine = SourceMappingEngine::new();
        let context = serde_json::json!({
            "payload": {
                "action": "updated",
                "id": "order-1",
                "total": 200
            },
            "key": "order-1"
        });

        let mut op_map = std::collections::HashMap::new();
        op_map.insert("created".to_string(), OperationType::Insert);
        op_map.insert("updated".to_string(), OperationType::Update);
        op_map.insert("deleted".to_string(), OperationType::Delete);

        let mapping = SourceMapping {
            when: None,
            operation: None,
            operation_from: Some("payload.action".to_string()),
            operation_map: Some(op_map),
            element_type: ElementType::Node,
            effective_from: None,
            template: ElementTemplate {
                id: "{{payload.id}}".to_string(),
                labels: vec!["Order".to_string()],
                properties: Some(serde_json::json!({
                    "total": "{{payload.total}}"
                })),
                from: None,
                to: None,
            },
        };

        let result = engine
            .process_mapping(&mapping, &context, "test-source")
            .unwrap();
        assert!(matches!(result, SourceChange::Update { .. }));
    }

    #[test]
    fn test_process_mapping_relation() {
        let engine = SourceMappingEngine::new();
        let context = serde_json::json!({
            "payload": {
                "id": "rel-1",
                "customer_id": "cust-1",
                "order_id": "order-1",
                "quantity": 5
            },
            "key": "rel-1"
        });

        let mapping = SourceMapping {
            when: None,
            operation: Some(OperationType::Insert),
            operation_from: None,
            operation_map: None,
            element_type: ElementType::Relation,
            effective_from: None,
            template: ElementTemplate {
                id: "{{payload.id}}".to_string(),
                labels: vec!["PURCHASED".to_string()],
                properties: Some(serde_json::json!({
                    "quantity": "{{payload.quantity}}"
                })),
                from: Some("{{payload.customer_id}}".to_string()),
                to: Some("{{payload.order_id}}".to_string()),
            },
        };

        let result = engine
            .process_mapping(&mapping, &context, "test-source")
            .unwrap();
        match result {
            SourceChange::Insert { element } => match element {
                Element::Relation {
                    metadata,
                    out_node,
                    in_node,
                    ..
                } => {
                    assert_eq!(metadata.reference.element_id.as_ref(), "rel-1");
                    assert_eq!(metadata.labels[0].as_ref(), "PURCHASED");
                    assert_eq!(out_node.element_id.as_ref(), "cust-1");
                    assert_eq!(in_node.element_id.as_ref(), "order-1");
                }
                _ => panic!("Expected Relation element"),
            },
            _ => panic!("Expected Insert"),
        }
    }

    #[test]
    fn test_condition_matches_field_equals() {
        let engine = SourceMappingEngine::new();
        let context = serde_json::json!({
            "payload": {
                "type": "order",
                "id": "123"
            }
        });

        let condition = MappingCondition {
            header: None,
            field: Some("type".to_string()),
            equals: Some("order".to_string()),
            contains: None,
            regex: None,
        };

        assert!(engine.condition_matches(&condition, &context, None));
    }

    #[test]
    fn test_condition_matches_field_not_equals() {
        let engine = SourceMappingEngine::new();
        let context = serde_json::json!({
            "payload": {
                "type": "shipment",
                "id": "123"
            }
        });

        let condition = MappingCondition {
            header: None,
            field: Some("type".to_string()),
            equals: Some("order".to_string()),
            contains: None,
            regex: None,
        };

        assert!(!engine.condition_matches(&condition, &context, None));
    }

    #[test]
    fn test_json_to_element_value_types() {
        let null_val = json_to_element_value(&JsonValue::Null).unwrap();
        assert_eq!(null_val, ElementValue::Null);

        let bool_val = json_to_element_value(&JsonValue::Bool(true)).unwrap();
        assert_eq!(bool_val, ElementValue::Bool(true));

        let int_val = json_to_element_value(&serde_json::json!(42)).unwrap();
        assert_eq!(int_val, ElementValue::Integer(42));

        let str_val = json_to_element_value(&serde_json::json!("hello")).unwrap();
        assert_eq!(str_val, ElementValue::String(Arc::from("hello")));
    }

    #[test]
    fn test_helpers_lowercase() {
        let engine = SourceMappingEngine::new();
        let context = serde_json::json!({"payload": {"name": "HELLO"}});

        let result = engine
            .render_string("{{lowercase payload.name}}", &context)
            .unwrap();
        assert_eq!(result, "hello");
    }

    #[test]
    fn test_helpers_concat() {
        let engine = SourceMappingEngine::new();
        let context = serde_json::json!({"payload": {"id": "123"}});

        let result = engine
            .render_string("{{concat \"prefix-\" payload.id}}", &context)
            .unwrap();
        assert_eq!(result, "prefix-123");
    }

    #[test]
    fn test_extract_simple_path_basic() {
        assert_eq!(
            extract_simple_path("{{payload.name}}"),
            Some("payload.name".to_string())
        );
    }

    #[test]
    fn test_extract_simple_path_with_spaces_around_braces() {
        assert_eq!(extract_simple_path("{{ key }}"), Some("key".to_string()));
    }

    #[test]
    fn test_extract_simple_path_helper_returns_none() {
        assert_eq!(extract_simple_path("{{#if x}}yes{{/if}}"), None);
    }

    #[test]
    fn test_extract_simple_path_with_space_returns_none() {
        // Contains a space inside — indicates a helper call
        assert_eq!(extract_simple_path("{{lowercase payload.name}}"), None);
    }

    #[test]
    fn test_extract_simple_path_not_template() {
        assert_eq!(extract_simple_path("plain-text"), None);
    }

    #[test]
    fn test_parse_with_format_iso8601() {
        let result = parse_with_format("2024-01-15T10:30:00Z", &TimestampFormat::Iso8601).unwrap();
        assert_eq!(result, 1705314600000);
    }

    #[test]
    fn test_parse_with_format_unix_seconds() {
        let result = parse_with_format("1705311000", &TimestampFormat::UnixSeconds).unwrap();
        assert_eq!(result, 1705311000000);
    }

    #[test]
    fn test_parse_with_format_unix_millis() {
        let result = parse_with_format("1705311000123", &TimestampFormat::UnixMillis).unwrap();
        assert_eq!(result, 1705311000123);
    }

    #[test]
    fn test_parse_with_format_unix_nanos() {
        let result = parse_with_format("1705311000123456789", &TimestampFormat::UnixNanos).unwrap();
        assert_eq!(result, 1705311000123);
    }

    #[test]
    fn test_parse_with_format_negative_seconds_rejected() {
        let result = parse_with_format("-100", &TimestampFormat::UnixSeconds);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Negative"));
    }

    #[test]
    fn test_parse_timestamp_auto_detect_seconds() {
        // Under 10 billion → treated as seconds
        let result = parse_timestamp("1705311000", None).unwrap();
        assert_eq!(result, 1705311000000);
    }

    #[test]
    fn test_parse_timestamp_auto_detect_millis() {
        // Between 10B and 10T → treated as milliseconds
        let result = parse_timestamp("1705311000123", None).unwrap();
        assert_eq!(result, 1705311000123);
    }

    #[test]
    fn test_parse_timestamp_auto_detect_nanos() {
        // Over 10T → treated as nanoseconds
        let result = parse_timestamp("1705311000123456789", None).unwrap();
        assert_eq!(result, 1705311000123);
    }

    #[test]
    fn test_parse_timestamp_iso8601() {
        let result = parse_timestamp("2024-01-15T10:30:00Z", None).unwrap();
        assert_eq!(result, 1705314600000);
    }
}
