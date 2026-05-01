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
            JsonValue::String(template) => {
                let rendered = self.render_string(template, context)?;
                // Try to parse as a typed value
                Ok(parse_rendered_value(&rendered))
            }
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

/// Parse a rendered string value, attempting to preserve types.
fn parse_rendered_value(value: &str) -> JsonValue {
    // Try integer
    if let Ok(n) = value.parse::<i64>() {
        return JsonValue::Number(n.into());
    }
    // Try float
    if let Ok(n) = value.parse::<f64>() {
        if let Some(num) = serde_json::Number::from_f64(n) {
            return JsonValue::Number(num);
        }
    }
    // Try boolean
    if value == "true" {
        return JsonValue::Bool(true);
    }
    if value == "false" {
        return JsonValue::Bool(false);
    }
    // Try null
    if value == "null" || value.is_empty() {
        return JsonValue::Null;
    }
    // Default to string
    JsonValue::String(value.to_string())
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
    fn test_render_properties() {
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
        assert_eq!(result["age"], json!(30));
    }

    #[test]
    fn test_render_missing_field() {
        let engine = TemplateEngine::new();
        let context = TemplateContext {
            item: json!({"id": "123"}),
            index: 0,
            source_id: "test-source".to_string(),
        };

        // Non-strict mode should render missing fields as empty string
        let result = engine.render_string("{{item.nonexistent}}", &context).unwrap();
        assert_eq!(result, "");
    }

    #[test]
    fn test_parse_rendered_values() {
        assert_eq!(parse_rendered_value("42"), json!(42));
        assert_eq!(parse_rendered_value("3.15"), json!(3.15));
        assert_eq!(parse_rendered_value("true"), json!(true));
        assert_eq!(parse_rendered_value("false"), json!(false));
        assert_eq!(parse_rendered_value("hello"), json!("hello"));
        assert_eq!(parse_rendered_value(""), JsonValue::Null);
    }
}
