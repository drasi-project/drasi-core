// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Validated, inline configuration constraints shared by plugins and graph admission.

use std::{fmt, sync::Arc};

use anyhow::{ensure, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{json, Map, Value};

#[derive(Clone)]
pub struct ConfigurationConstraint {
    schema: Map<String, Value>,
    compiled: Arc<jsonschema::JSONSchema>,
}

impl ConfigurationConstraint {
    /// Validate and compile an inline schema without network or filesystem resolution.
    pub fn try_new(schema: Value) -> Result<Self> {
        let object = schema
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("configuration constraint must be a schema object"))?;
        let mut pending = vec![&schema];
        while let Some(value) = pending.pop() {
            let Some(object) = value.as_object() else {
                continue;
            };
            ensure!(
                !object.contains_key("$ref") && !object.contains_key("$id"),
                "configuration constraints must be inline, without references or base URIs"
            );
            for keyword in [
                "additionalItems",
                "contains",
                "additionalProperties",
                "propertyNames",
                "if",
                "then",
                "else",
                "not",
            ] {
                if let Some(schema) = object.get(keyword) {
                    pending.push(schema);
                }
            }
            if let Some(items) = object.get("items") {
                if let Some(schemas) = items.as_array() {
                    pending.extend(schemas);
                } else {
                    pending.push(items);
                }
            }
            for keyword in ["allOf", "anyOf", "oneOf"] {
                if let Some(schemas) = object.get(keyword).and_then(Value::as_array) {
                    pending.extend(schemas);
                }
            }
            for keyword in [
                "definitions",
                "properties",
                "patternProperties",
                "dependencies",
            ] {
                if let Some(schemas) = object.get(keyword).and_then(Value::as_object) {
                    pending.extend(schemas.values());
                }
            }
        }
        let compiled = jsonschema::JSONSchema::options()
            .with_draft(jsonschema::Draft::Draft7)
            .should_validate_formats(true)
            .with_format("uuid", |value| crate::ComponentId::try_new(value).is_ok())
            .compile(&schema)
            .map_err(|error| {
                anyhow::anyhow!(
                    "invalid configuration constraint at {}",
                    error.instance_path
                )
            })?;
        Ok(Self {
            schema: object.clone(),
            compiled: Arc::new(compiled),
        })
    }

    pub fn accepts(&self, value: &Value) -> bool {
        self.compiled.is_valid(value)
    }

    pub fn schema(&self) -> &Map<String, Value> {
        &self.schema
    }

    /// A sink's alias map. Aliases are presentation labels, not component names.
    pub fn output_aliases() -> Self {
        Self::try_new(json!({
            "type": "object",
            "propertyNames": {"minLength": 1, "pattern": crate::ComponentName::JSON_SCHEMA_PATTERN},
            "additionalProperties": Self::endpoint(false),
        }))
        .expect("constant output alias schema")
    }

    /// Ordered, ID-only query execution inputs; no authoring-name shorthand.
    pub fn query_inputs() -> Self {
        let mut endpoint = Self::endpoint(false);
        let properties = endpoint["properties"]
            .as_object_mut()
            .expect("constant object");
        for field in ["nodes", "relations", "pipeline"] {
            properties.insert(
                field.into(),
                json!({"type": "array", "items": {"type": "string"}}),
            );
        }
        Self::try_new(json!({"type": "array", "items": endpoint}))
            .expect("constant query input schema")
    }

    /// Snapshot opening requires the exact component UUID and output port.
    pub fn snapshot_inputs() -> Self {
        Self::try_new(json!({"type": "array", "items": Self::endpoint(true)}))
            .expect("constant snapshot input schema")
    }

    /// Identity constraints inside the shared execution settings object.
    pub fn query_execution() -> Self {
        Self::try_new(json!({
            "type": "object",
            "properties": {"inputs": Self::query_inputs().schema()},
        }))
        .expect("constant query execution identity schema")
    }

    /// Assigned ordinary query metadata retains authoring name/ID references.
    pub fn assigned_query_identity() -> Self {
        let name = json!({
            "type": "string", "minLength": 1,
            "pattern": crate::ComponentName::JSON_SCHEMA_PATTERN,
        });
        let id_reference = json!({
            "type": "object", "required": ["id"],
            "properties": {"id": {"type": "string", "format": "uuid"}},
            "additionalProperties": false,
        });
        let mut input = Self::endpoint(false);
        input["properties"]["component"] = json!({"anyOf": [name, id_reference]});
        for field in ["nodes", "relations", "pipeline"] {
            input["properties"][field] = json!({"type": "array", "items": {"type": "string"}});
        }
        Self::try_new(json!({
            "type": "object", "required": ["id", "name"],
            "properties": {
                "id": {"type": "string", "format": "uuid"},
                "name": name,
                "inputs": {"type": "array", "items": {"anyOf": [name, id_reference, input]}},
            },
        }))
        .expect("constant assigned query identity schema")
    }

    fn endpoint(port_required: bool) -> Value {
        json!({
            "type": "object",
            "required": if port_required { vec!["component", "port"] } else { vec!["component"] },
            "properties": {
                "component": {"type": "string", "format": "uuid"},
                "port": {"type": if port_required { json!("string") } else { json!(["string", "null"]) }, "minLength": 1, "pattern": crate::ComponentName::JSON_SCHEMA_PATTERN},
            },
            "additionalProperties": false,
        })
    }
}

impl fmt::Debug for ConfigurationConstraint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.schema.fmt(formatter)
    }
}

impl PartialEq for ConfigurationConstraint {
    fn eq(&self, other: &Self) -> bool {
        self.schema == other.schema
    }
}

impl Eq for ConfigurationConstraint {}

impl Serialize for ConfigurationConstraint {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        self.schema.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ConfigurationConstraint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Self::try_new(Value::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}
