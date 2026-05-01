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

//! Handlebars template engine for transforming API response items into
//! Drasi graph elements.

use anyhow::{anyhow, Result};
use handlebars::Handlebars;
use serde_json::Value as JsonValue;
use std::collections::HashMap;

/// Template context containing variables available in templates.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TemplateContext {
    /// The current item being mapped.
    pub item: JsonValue,
    /// The index of the current item in the page.
    pub index: usize,
    /// Source ID.
    pub source_id: String,
}

/// Compiled template engine with Handlebars.
pub struct TemplateEngine {
    handlebars: Handlebars<'static>,
}

impl Default for TemplateEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl TemplateEngine {
    /// Create a new template engine.
    pub fn new() -> Self {
        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(false);
        Self { handlebars }
    }

    /// Render a template string with the given context.
    pub fn render_string(&self, template: &str, context: &TemplateContext) -> Result<String> {
        self.handlebars
            .render_template(template, context)
            .map_err(|e| anyhow!("Template render error: {e}"))
    }

    /// Render a template and preserve the JSON value type.
    ///
    /// If the template is a simple variable reference like `{{item.field}}`,
    /// this returns the original JSON value (preserving int/float/bool/string
    /// types). Otherwise, it renders through Handlebars and returns a string.
    pub fn render_value(&self, template: &str, context: &TemplateContext) -> Result<JsonValue> {
        // If template is a simple reference, resolve directly to preserve type
        if let Some(path) = extract_simple_path(template) {
            let context_json = context_to_json(context);
            if let Some(value) = resolve_path(&context_json, &path) {
                return Ok(value.clone());
            }
        }

        // Fall back to string rendering
        let rendered = self.render_string(template, context)?;
        if rendered.is_empty() {
            Ok(JsonValue::Null)
        } else {
            Ok(JsonValue::String(rendered))
        }
    }

    /// Render properties from a JSON value template specification.
    /// Each value in the map is treated as a Handlebars template.
    pub fn render_properties(
        &self,
        properties: &JsonValue,
        context: &TemplateContext,
    ) -> Result<HashMap<String, JsonValue>> {
        match properties {
            JsonValue::Object(map) => {
                let mut result = HashMap::new();
                for (key, value) in map {
                    let rendered = self.render_property_value(value, context)?;
                    result.insert(key.clone(), rendered);
                }
                Ok(result)
            }
            _ => Err(anyhow!("Properties must be a JSON object")),
        }
    }

    /// Render a single property value.
    fn render_property_value(
        &self,
        value: &JsonValue,
        context: &TemplateContext,
    ) -> Result<JsonValue> {
        match value {
            JsonValue::String(template) => self.render_value(template, context),
            JsonValue::Object(map) => {
                let mut result = serde_json::Map::new();
                for (key, v) in map {
                    let rendered = self.render_property_value(v, context)?;
                    result.insert(key.clone(), rendered);
                }
                Ok(JsonValue::Object(result))
            }
            JsonValue::Array(arr) => {
                let mut result = Vec::new();
                for v in arr {
                    let rendered = self.render_property_value(v, context)?;
                    result.push(rendered);
                }
                Ok(JsonValue::Array(result))
            }
            // Non-string literals pass through unchanged
            other => Ok(other.clone()),
        }
    }
}

/// Check if a template is a simple variable reference like `{{item.field}}`.
/// Returns the path (e.g., `"item.field"`) if it is, None otherwise.
fn extract_simple_path(template: &str) -> Option<String> {
    let trimmed = template.trim();
    if trimmed.starts_with("{{") && trimmed.ends_with("}}") {
        let inner = trimmed[2..trimmed.len() - 2].trim();
        // Simple path: no spaces (helpers), no block markers, no extra braces
        if !inner.contains(' ')
            && !inner.contains('#')
            && !inner.contains('/')
            && !inner.contains('{')
            && !inner.contains('}')
        {
            return Some(inner.to_string());
        }
    }
    None
}

/// Resolve a dot-separated path in a JSON value.
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

/// Convert TemplateContext to JSON for direct path resolution.
fn context_to_json(context: &TemplateContext) -> JsonValue {
    serde_json::to_value(context).unwrap_or(JsonValue::Null)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_render_simple_template() {
        let engine = TemplateEngine::new();
        let context = TemplateContext {
            item: json!({"id": "123", "name": "Alice"}),
            index: 0,
            source_id: "test-source".to_string(),
        };

        let result = engine.render_string("{{item.id}}", &context).unwrap();
        assert_eq!(result, "123");

        let result = engine.render_string("{{item.name}}", &context).unwrap();
        assert_eq!(result, "Alice");
    }

    #[test]
    fn test_render_value_preserves_types() {
        let engine = TemplateEngine::new();
        let context = TemplateContext {
            item: json!({"id": "str-123", "count": 42, "rate": 3.15, "active": true}),
            index: 0,
            source_id: "test-source".to_string(),
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
        let context = TemplateContext {
            item: json!({"id": 42, "prefix": "user"}),
            index: 0,
            source_id: "test-source".to_string(),
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
        let context = TemplateContext {
            item: json!({"id": "123", "name": "Alice", "age": 30}),
            index: 0,
            source_id: "test-source".to_string(),
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
        let context = TemplateContext {
            item: json!({"id": "123"}),
            index: 0,
            source_id: "test-source".to_string(),
        };

        // Non-strict mode: missing fields render as empty string
        let result = engine.render_string("{{item.nonexistent}}", &context).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_extract_simple_path() {
        assert_eq!(
            extract_simple_path("{{item.id}}"),
            Some("item.id".to_string())
        );
        assert_eq!(
            extract_simple_path("{{item.nested.field}}"),
            Some("item.nested.field".to_string())
        );
        // Complex templates are not simple paths
        assert_eq!(extract_simple_path("{{item.a}}-{{item.b}}"), None);
        assert_eq!(extract_simple_path("prefix-{{item.id}}"), None);
        assert_eq!(extract_simple_path("static text"), None);
    }

    #[test]
    fn test_string_value_not_coerced_to_int() {
        let engine = TemplateEngine::new();
        let context = TemplateContext {
            item: json!({"id": "42"}), // String "42", not integer 42
            index: 0,
            source_id: "test-source".to_string(),
        };

        // Should stay as string since the original value is a string
        let result = engine.render_value("{{item.id}}", &context).unwrap();
        assert_eq!(result, json!("42"));
        assert!(result.is_string());
    }
}
