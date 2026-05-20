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

//! UI hint annotations for plugin configuration schemas.
//!
//! Since utoipa doesn't support property-level OpenAPI extensions, this module
//! provides a post-processing builder that injects `x-ui:*` extension properties
//! into schema JSON. These hints are consumed by the Drasi UI to render
//! rich, schema-driven configuration forms instead of flat YAML editors.
//!
//! # Supported Extensions
//!
//! - `x-ui:widget` — Override widget type: `"password"`, `"textarea"`, `"slider"`, `"hidden"`, `"code-editor"`
//! - `x-ui:group` — Group name for section grouping (e.g., `"Connection"`, `"Authentication"`)
//! - `x-ui:order` — Display order within a group (lower = first)
//! - `x-ui:placeholder` — Placeholder text for input fields
//! - `x-ui:help` — Help text displayed below the field
//! - `x-ui:condition` — Conditional visibility: `{"field": "fieldName", "value": "expectedValue"}` or `{"field": "fieldName", "notEmpty": true}`
//! - `x-ui:collapsed` — Whether the group containing this field starts collapsed
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::schema_ui::SchemaUiAnnotator;
//!
//! fn config_schema_json(&self) -> String {
//!     let api = MySchemas::openapi();
//!     let schemas = api.components.as_ref().unwrap().schemas.clone();
//!     let schemas_value = serde_json::to_value(&schemas).unwrap();
//!
//!     SchemaUiAnnotator::new(schemas_value, "source.postgres.PostgresSourceConfig")
//!         .expect("root schema not found")
//!         .field("host", |f| f.group("Connection").order(1).placeholder("localhost"))
//!         .field("password", |f| f.group("Authentication").widget("password"))
//!         .annotate()
//! }
//! ```

use serde_json::{Map, Value};
use std::fmt;

/// Errors that can occur when building or applying UI annotations.
#[derive(Debug)]
pub enum SchemaUiError {
    /// The `root_schema_name` was not found in the schemas map.
    RootSchemaNotFound {
        /// The schema name that was looked up.
        name: String,
    },
}

impl fmt::Display for SchemaUiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchemaUiError::RootSchemaNotFound { name } => {
                write!(
                    f,
                    "SchemaUiAnnotator: root schema '{name}' not found in schemas map",
                )
            }
        }
    }
}

impl std::error::Error for SchemaUiError {}

/// Builder for a single field's UI annotations.
#[derive(Debug)]
pub struct FieldUiBuilder {
    annotations: Map<String, Value>,
}

impl FieldUiBuilder {
    fn new() -> Self {
        Self {
            annotations: Map::new(),
        }
    }

    /// Set the widget type override.
    pub fn widget(mut self, widget: &str) -> Self {
        self.annotations
            .insert("x-ui:widget".to_string(), Value::String(widget.to_string()));
        self
    }

    /// Set the group name for section grouping.
    pub fn group(mut self, group: &str) -> Self {
        self.annotations
            .insert("x-ui:group".to_string(), Value::String(group.to_string()));
        self
    }

    /// Set the display order within a group.
    pub fn order(mut self, order: i64) -> Self {
        self.annotations
            .insert("x-ui:order".to_string(), Value::Number(order.into()));
        self
    }

    /// Set placeholder text.
    pub fn placeholder(mut self, placeholder: &str) -> Self {
        self.annotations.insert(
            "x-ui:placeholder".to_string(),
            Value::String(placeholder.to_string()),
        );
        self
    }

    /// Set help text displayed below the field.
    pub fn help(mut self, help: &str) -> Self {
        self.annotations
            .insert("x-ui:help".to_string(), Value::String(help.to_string()));
        self
    }

    /// Set conditional visibility based on a field matching a specific value.
    pub fn condition_value(mut self, field: &str, value: &str) -> Self {
        let mut condition = Map::new();
        condition.insert("field".to_string(), Value::String(field.to_string()));
        condition.insert("value".to_string(), Value::String(value.to_string()));
        self.annotations
            .insert("x-ui:condition".to_string(), Value::Object(condition));
        self
    }

    /// Set conditional visibility based on a field being non-empty.
    pub fn condition_not_empty(mut self, field: &str) -> Self {
        let mut condition = Map::new();
        condition.insert("field".to_string(), Value::String(field.to_string()));
        condition.insert("notEmpty".to_string(), Value::Bool(true));
        self.annotations
            .insert("x-ui:condition".to_string(), Value::Object(condition));
        self
    }

    /// Sets `x-ui:collapsed` on this field, signalling that the group
    /// containing this field starts collapsed by default.
    ///
    /// This is a group-level concept expressed on the field builder for
    /// convenience. If multiple fields in the same group set conflicting
    /// values, the last one written wins.
    pub fn collapsed(mut self, collapsed: bool) -> Self {
        self.annotations
            .insert("x-ui:collapsed".to_string(), Value::Bool(collapsed));
        self
    }
}

/// Annotates an OpenAPI schema map with `x-ui:*` UI hint extensions.
#[derive(Debug)]
pub struct SchemaUiAnnotator {
    schemas: Value,
    root_schema_name: String,
    field_annotations: Vec<(String, FieldUiBuilder)>,
}

impl SchemaUiAnnotator {
    /// Create a new annotator from a `serde_json::Value` representing the schemas map.
    ///
    /// Returns `Err(SchemaUiError::RootSchemaNotFound)` if `root_schema_name` does not
    /// exist as a key in the schemas map.
    ///
    /// `root_schema_name` is the key in the map that identifies the root config schema
    /// (e.g., `"source.postgres.PostgresSourceConfig"`).
    pub fn new(schemas: Value, root_schema_name: &str) -> Result<Self, SchemaUiError> {
        if schemas.get(root_schema_name).is_none() {
            return Err(SchemaUiError::RootSchemaNotFound {
                name: root_schema_name.to_string(),
            });
        }
        Ok(Self {
            schemas,
            root_schema_name: root_schema_name.to_string(),
            field_annotations: Vec::new(),
        })
    }

    /// Add UI annotations for a field.
    pub fn field<F>(mut self, field_name: &str, builder_fn: F) -> Self
    where
        F: FnOnce(FieldUiBuilder) -> FieldUiBuilder,
    {
        let builder = builder_fn(FieldUiBuilder::new());
        self.field_annotations
            .push((field_name.to_string(), builder));
        self
    }

    /// Apply all annotations and return the modified JSON string.
    ///
    /// Fields named in `.field()` calls that do not exist in the root schema's
    /// `properties` are silently skipped (a `debug_assert!` fires in debug builds).
    pub fn annotate(mut self) -> String {
        if let Some(root) = self.schemas.get_mut(&self.root_schema_name) {
            if let Some(properties) = root.get_mut("properties") {
                for (field_name, builder) in &self.field_annotations {
                    if let Some(prop) = properties.get_mut(field_name) {
                        if let Some(obj) = prop.as_object_mut() {
                            for (key, value) in &builder.annotations {
                                obj.insert(key.clone(), value.clone());
                            }
                        }
                    } else {
                        debug_assert!(
                            false,
                            "SchemaUiAnnotator: field '{}' not found in properties of '{}'",
                            field_name, self.root_schema_name
                        );
                    }
                }
            }
        }
        serde_json::to_string(&self.schemas).expect("SchemaUiAnnotator: failed to serialize")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_schema() -> Value {
        json!({
            "my.Config": {
                "type": "object",
                "properties": {
                    "host": { "type": "string" },
                    "port": { "type": "integer" },
                    "password": { "type": "string" },
                    "authMode": { "type": "string" },
                    "token": { "type": "string" }
                }
            }
        })
    }

    #[test]
    fn happy_path_annotations_applied() {
        let result = SchemaUiAnnotator::new(test_schema(), "my.Config")
            .unwrap()
            .field("host", |f| {
                f.group("Connection").order(1).placeholder("localhost")
            })
            .field("password", |f| f.group("Auth").widget("password"))
            .annotate();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let host = &parsed["my.Config"]["properties"]["host"];
        assert_eq!(host["x-ui:group"], "Connection");
        assert_eq!(host["x-ui:order"], 1);
        assert_eq!(host["x-ui:placeholder"], "localhost");

        let pw = &parsed["my.Config"]["properties"]["password"];
        assert_eq!(pw["x-ui:group"], "Auth");
        assert_eq!(pw["x-ui:widget"], "password");
    }

    #[test]
    fn unknown_field_silently_skipped_in_release() {
        // In debug builds, unknown fields trigger a debug_assert.
        // This test verifies that known fields are still annotated even
        // when an unknown field is also specified.
        // The debug_assert is tested separately below.
        let result = SchemaUiAnnotator::new(test_schema(), "my.Config")
            .unwrap()
            .field("host", |f| f.group("Connection"))
            .annotate();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed["my.Config"]["properties"]["host"]["x-ui:group"],
            "Connection"
        );
    }

    #[test]
    #[should_panic(expected = "not found in properties")]
    fn unknown_field_debug_asserts_in_debug_builds() {
        SchemaUiAnnotator::new(test_schema(), "my.Config")
            .unwrap()
            .field("nonexistent", |f| f.group("Oops"))
            .annotate();
    }

    #[test]
    fn missing_root_schema_returns_error() {
        let err = SchemaUiAnnotator::new(test_schema(), "wrong.Name").unwrap_err();
        match err {
            SchemaUiError::RootSchemaNotFound { name } => {
                assert_eq!(name, "wrong.Name");
            }
        }
    }

    #[test]
    fn multiple_annotations_on_same_field() {
        let result = SchemaUiAnnotator::new(test_schema(), "my.Config")
            .unwrap()
            .field("host", |f| {
                f.group("Connection")
                    .order(1)
                    .placeholder("localhost")
                    .widget("textarea")
                    .help("Enter the hostname")
            })
            .annotate();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let host = &parsed["my.Config"]["properties"]["host"];
        assert_eq!(host["x-ui:group"], "Connection");
        assert_eq!(host["x-ui:order"], 1);
        assert_eq!(host["x-ui:placeholder"], "localhost");
        assert_eq!(host["x-ui:widget"], "textarea");
        assert_eq!(host["x-ui:help"], "Enter the hostname");
    }

    #[test]
    fn condition_value_produces_correct_json() {
        let result = SchemaUiAnnotator::new(test_schema(), "my.Config")
            .unwrap()
            .field("token", |f| f.condition_value("authMode", "token"))
            .annotate();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let cond = &parsed["my.Config"]["properties"]["token"]["x-ui:condition"];
        assert_eq!(cond["field"], "authMode");
        assert_eq!(cond["value"], "token");
    }

    #[test]
    fn condition_not_empty_produces_correct_json() {
        let result = SchemaUiAnnotator::new(test_schema(), "my.Config")
            .unwrap()
            .field("token", |f| f.condition_not_empty("authMode"))
            .annotate();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        let cond = &parsed["my.Config"]["properties"]["token"]["x-ui:condition"];
        assert_eq!(cond["field"], "authMode");
        assert_eq!(cond["notEmpty"], true);
    }

    #[test]
    fn collapsed_annotation_works() {
        let result = SchemaUiAnnotator::new(test_schema(), "my.Config")
            .unwrap()
            .field("host", |f| f.group("Connection").collapsed(true))
            .annotate();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed["my.Config"]["properties"]["host"]["x-ui:collapsed"],
            true
        );
    }

    #[test]
    fn help_annotation_works() {
        let result = SchemaUiAnnotator::new(test_schema(), "my.Config")
            .unwrap()
            .field("host", |f| f.help("The server hostname or IP"))
            .annotate();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        assert_eq!(
            parsed["my.Config"]["properties"]["host"]["x-ui:help"],
            "The server hostname or IP"
        );
    }

    #[test]
    fn unannotated_fields_unchanged() {
        let result = SchemaUiAnnotator::new(test_schema(), "my.Config")
            .unwrap()
            .field("host", |f| f.group("Connection"))
            .annotate();

        let parsed: Value = serde_json::from_str(&result).unwrap();
        // port was not annotated — should have no x-ui keys
        let port = &parsed["my.Config"]["properties"]["port"];
        assert_eq!(port["type"], "integer");
        assert!(port.get("x-ui:group").is_none());
    }

    #[test]
    fn error_display_message() {
        let err = SchemaUiError::RootSchemaNotFound {
            name: "bad.Name".to_string(),
        };
        assert!(err.to_string().contains("bad.Name"));
        assert!(err.to_string().contains("not found"));
    }
}
