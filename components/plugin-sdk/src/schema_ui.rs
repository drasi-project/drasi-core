//! UI hint annotations for plugin configuration schemas.
//!
//! Since utoipa doesn't support property-level OpenAPI extensions, this module
//! provides a post-processing builder that injects `x-ui:*` extension properties
//! into serialized schema JSON. These hints are consumed by the Drasi UI to render
//! rich, schema-driven configuration forms instead of flat YAML editors.
//!
//! # Supported Extensions
//!
//! - `x-ui:widget` — Override widget type: `"password"`, `"textarea"`, `"slider"`, `"hidden"`, `"code-editor"`
//! - `x-ui:group` — Group name for section grouping (e.g., `"Connection"`, `"Authentication"`)
//! - `x-ui:order` — Display order within a group (lower = first)
//! - `x-ui:placeholder` — Placeholder text for input fields
//! - `x-ui:condition` — Conditional visibility: `{"field": "fieldName", "value": "expectedValue"}` or `{"field": "fieldName", "notEmpty": true}`
//! - `x-ui:collapsed` — Whether a group section starts collapsed
//!
//! # Example
//!
//! ```rust,ignore
//! use drasi_plugin_sdk::schema_ui::SchemaUiAnnotator;
//!
//! fn config_schema_json(&self) -> String {
//!     let api = MySchemas::openapi();
//!     let raw = serde_json::to_string(&api.components.as_ref().unwrap().schemas).unwrap();
//!     
//!     SchemaUiAnnotator::new(&raw, "source.postgres.PostgresSourceConfig")
//!         .field("host", |f| f.group("Connection").order(1).placeholder("localhost"))
//!         .field("password", |f| f.group("Authentication").widget("password"))
//!         .annotate()
//! }
//! ```

use serde_json::{Map, Value};

/// Builder for a single field's UI annotations.
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

    /// Set whether the group section starts collapsed.
    pub fn collapsed(mut self, collapsed: bool) -> Self {
        self.annotations
            .insert("x-ui:collapsed".to_string(), Value::Bool(collapsed));
        self
    }
}

/// Annotates an OpenAPI schema map with `x-ui:*` UI hint extensions.
pub struct SchemaUiAnnotator {
    schemas: Value,
    root_schema_name: String,
    field_annotations: Vec<(String, FieldUiBuilder)>,
}

impl SchemaUiAnnotator {
    /// Create a new annotator from a serialized schemas map JSON string.
    ///
    /// `root_schema_name` is the key in the map that identifies the root config schema
    /// (e.g., `"source.postgres.PostgresSourceConfig"`).
    pub fn new(schemas_json: &str, root_schema_name: &str) -> Self {
        let schemas: Value =
            serde_json::from_str(schemas_json).expect("SchemaUiAnnotator: invalid JSON input");
        Self {
            schemas,
            root_schema_name: root_schema_name.to_string(),
            field_annotations: Vec::new(),
        }
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
                    }
                }
            }
        }
        serde_json::to_string(&self.schemas).expect("SchemaUiAnnotator: failed to serialize")
    }
}
