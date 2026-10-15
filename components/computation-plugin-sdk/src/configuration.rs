// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Human-readable defaults and supported values, independently negotiated without
//! changing the base configuration wire format.

use crate::{abi, ConfigField, ConfigSchema, ConfigType, PluginMetadata};
use anyhow::{ensure, Result};
use drasi_lib::computation::v1::ImplementationIdentity;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFieldDetails {
    pub description: String,
    #[serde(
        default,
        deserialize_with = "present_default",
        skip_serializing_if = "Option::is_none"
    )]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minimum: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maximum: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_values: Vec<Value>,
    #[serde(default)]
    pub deprecated: bool,
}

fn present_default<'de, D>(deserializer: D) -> std::result::Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

impl ConfigFieldDetails {
    fn validate_value(&self, name: &str, value: &Value) -> Result<()> {
        ensure!(
            self.allowed_values.is_empty() || self.allowed_values.contains(value),
            "native configuration field {name} is not one of its supported values"
        );
        if let Some(minimum) = self.minimum {
            let valid = match value.as_i64() {
                Some(value) => value >= minimum,
                None => value
                    .as_u64()
                    .is_some_and(|value| minimum <= 0 || value >= minimum as u64),
            };
            ensure!(
                valid,
                "native configuration field {name} is below its minimum"
            );
        }
        if let Some(maximum) = self.maximum {
            let valid = match value.as_u64() {
                Some(value) => value <= maximum,
                None => value.as_i64().is_some_and(|value| value < 0),
            };
            ensure!(
                valid,
                "native configuration field {name} exceeds its maximum"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigDetails {
    pub fields: BTreeMap<String, ConfigFieldDetails>,
}

impl ConfigDetails {
    pub fn validate_schema(&self, schema: &ConfigSchema) -> Result<()> {
        for (name, details) in &self.fields {
            let field = schema.fields.get(name).ok_or_else(|| {
                anyhow::anyhow!("configuration details name unknown field {name}")
            })?;
            ensure!(
                !details.description.trim().is_empty(),
                "configuration field {name} has no description"
            );
            ensure!(
                (details.minimum.is_none() && details.maximum.is_none())
                    || field.value_type == ConfigType::Integer,
                "numeric limits require an integer field: {name}"
            );
            if let (Some(minimum), Some(maximum)) = (details.minimum, details.maximum) {
                ensure!(
                    minimum <= 0 || minimum as u64 <= maximum,
                    "invalid configuration bounds for {name}"
                );
            }
            for value in &details.allowed_values {
                ensure!(
                    field.value_type.accepts(value),
                    "configuration choice has the wrong type for {name}"
                );
                details.validate_value(name, value)?;
            }
            if let Some(value) = &details.default {
                ensure!(
                    !field.secret,
                    "secret configuration field {name} cannot publish a literal default"
                );
                ensure!(
                    field.value_type.accepts(value),
                    "configuration default has the wrong type for {name}"
                );
                details.validate_value(name, value)?;
            }
        }
        Ok(())
    }

    /// Validate explicit values. Constructors remain responsible for applying
    /// defaults and checking constraints involving several fields or resources.
    pub fn validate(&self, configuration: &Value) -> Result<()> {
        let object = configuration
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("native component configuration must be an object"))?;
        for (name, details) in &self.fields {
            if let Some(value) = object.get(name) {
                details.validate_value(name, value)?;
            }
        }
        Ok(())
    }
}

/// One declaration supplies both the old field/type schema and optional details.
pub struct ConfigOption {
    name: String,
    field: ConfigField,
    details: ConfigFieldDetails,
}

impl ConfigOption {
    pub fn new(name: &str, value_type: ConfigType, required: bool, description: &str) -> Self {
        Self {
            name: name.into(),
            field: ConfigField {
                value_type,
                required,
                secret: false,
            },
            details: ConfigFieldDetails {
                description: description.into(),
                ..Default::default()
            },
        }
    }
    pub fn default(mut self, value: impl Into<Value>) -> Self {
        self.details.default = Some(value.into());
        self
    }
    pub fn default_description(mut self, description: &str) -> Self {
        self.details.default_description = Some(description.into());
        self
    }
    pub fn minimum(mut self, value: i64) -> Self {
        self.details.minimum = Some(value);
        self
    }
    pub fn maximum(mut self, value: u64) -> Self {
        self.details.maximum = Some(value);
        self
    }
    pub fn allowed(mut self, values: impl IntoIterator<Item = Value>) -> Self {
        self.details.allowed_values = values.into_iter().collect();
        self
    }
    pub fn deprecated(mut self) -> Self {
        self.details.deprecated = true;
        self
    }
}

pub struct ConfigDefinition {
    options: Vec<ConfigOption>,
}

impl ConfigDefinition {
    pub fn try_new(options: impl IntoIterator<Item = ConfigOption>) -> Result<Self> {
        let definition = Self {
            options: options.into_iter().collect(),
        };
        let mut names = std::collections::BTreeSet::new();
        for option in &definition.options {
            drasi_lib::computation::v1::ComponentId::try_new(option.name.as_str())?;
            ensure!(
                names.insert(&option.name),
                "duplicate configuration field {}",
                option.name
            );
        }
        definition.details().validate_schema(&definition.schema())?;
        Ok(definition)
    }
    pub fn schema(&self) -> ConfigSchema {
        ConfigSchema {
            fields: self
                .options
                .iter()
                .map(|option| (option.name.clone(), option.field.clone()))
                .collect(),
            allow_additional: false,
        }
    }
    pub fn details(&self) -> ConfigDetails {
        ConfigDetails {
            fields: self
                .options
                .iter()
                .map(|option| (option.name.clone(), option.details.clone()))
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactoryConfiguration {
    pub implementation: ImplementationIdentity,
    pub details: ConfigDetails,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigurationMetadata {
    pub version: u32,
    pub factories: Vec<FactoryConfiguration>,
}

impl ConfigurationMetadata {
    pub fn validate(&self, plugin: &PluginMetadata) -> Result<()> {
        ensure!(
            self.version == abi::configuration::VERSION,
            "unsupported native configuration-description version"
        );
        let mut seen = std::collections::BTreeSet::new();
        for configuration in &self.factories {
            ensure!(
                seen.insert(&configuration.implementation),
                "duplicate native configuration description"
            );
            let factory = plugin
                .factories
                .iter()
                .find(|factory| factory.implementation == configuration.implementation)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "configuration description does not match a declared native factory"
                    )
                })?;
            configuration
                .details
                .validate_schema(&factory.configuration)?;
        }
        Ok(())
    }
}

/// Produce the same JSON Schema for API discovery and generated documentation.
/// Missing optional details leave the original field/type contract intact.
pub fn configuration_json_schema(schema: &ConfigSchema, details: Option<&ConfigDetails>) -> Value {
    let mut properties = serde_json::Map::new();
    let mut required = Vec::new();
    for (name, field) in &schema.fields {
        let mut property = serde_json::Map::new();
        let kind = match field.value_type {
            ConfigType::Boolean => Some("boolean"),
            ConfigType::Integer => Some("integer"),
            ConfigType::String => Some("string"),
            ConfigType::Object => Some("object"),
            ConfigType::Array => Some("array"),
            ConfigType::Json => None,
        };
        if let Some(kind) = kind {
            property.insert("type".into(), json!(kind));
            if kind == "array" {
                property.insert("items".into(), json!({}));
            }
        }
        if field.secret {
            property.insert("writeOnly".into(), json!(true));
        }
        if let Some(details) = details.and_then(|details| details.fields.get(name)) {
            let description = match &details.default_description {
                Some(default) => format!("{} Default: {default}", details.description),
                None => details.description.clone(),
            };
            property.insert("description".into(), json!(description));
            if let Some(default) = &details.default {
                property.insert("default".into(), default.clone());
            }
            if let Some(minimum) = details.minimum {
                property.insert("minimum".into(), json!(minimum));
            }
            if let Some(maximum) = details.maximum {
                property.insert("maximum".into(), json!(maximum));
            }
            if !details.allowed_values.is_empty() {
                property.insert("enum".into(), json!(details.allowed_values));
            }
            if details.deprecated {
                property.insert("deprecated".into(), json!(true));
            }
        }
        properties.insert(name.clone(), Value::Object(property));
        if field.required {
            required.push(name.clone());
        }
    }
    json!({"type":"object", "properties":properties, "required":required, "additionalProperties":schema.allow_additional})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition() -> ConfigDefinition {
        ConfigDefinition::try_new([
            ConfigOption::new(
                "enabled",
                ConfigType::Boolean,
                false,
                "Must remain disabled.",
            )
            .default(false)
            .allowed([json!(false)]),
            ConfigOption::new("count", ConfigType::Integer, false, "Number of items.")
                .default(2)
                .minimum(1)
                .maximum(4),
        ])
        .expect("valid definition")
    }

    #[test]
    fn rich_description_does_not_change_base_metadata_shape() {
        assert_eq!(
            definition().schema(),
            ConfigSchema::from_fields([
                ("enabled", ConfigType::Boolean, false),
                ("count", ConfigType::Integer, false),
            ])
        );
        let schema =
            configuration_json_schema(&definition().schema(), Some(&definition().details()));
        assert_eq!(schema["properties"]["enabled"]["enum"], json!([false]));
        assert_eq!(schema["properties"]["count"]["default"], 2);
        assert_eq!(schema["properties"]["count"]["minimum"], 1);
        assert_eq!(schema["properties"]["count"]["maximum"], 4);
        let old = configuration_json_schema(&definition().schema(), None);
        assert!(old["properties"]["count"].get("default").is_none());
    }

    #[test]
    fn validates_actual_boundary_values_without_applying_defaults() -> Result<()> {
        let description = definition().details();
        for value in [
            json!({}),
            json!({"count":1}),
            json!({"count":4,"enabled":false}),
        ] {
            description.validate(&value)?;
        }
        for value in [
            json!({"count":0}),
            json!({"count":5}),
            json!({"count":-1}),
            json!({"count":u64::MAX}),
            json!({"enabled":true}),
        ] {
            assert!(description.validate(&value).is_err(), "{value}");
        }
        Ok(())
    }

    #[test]
    fn definitions_reject_duplicates_invalid_defaults_and_incompatible_limits() {
        assert!(ConfigDefinition::try_new([
            ConfigOption::new("field", ConfigType::String, false, "One."),
            ConfigOption::new("field", ConfigType::String, false, "Two."),
        ])
        .is_err());
        for option in [
            ConfigOption::new("field", ConfigType::Integer, false, "Wrong default.")
                .default("text"),
            ConfigOption::new("field", ConfigType::Integer, false, "Out of range.")
                .minimum(1)
                .default(0),
            ConfigOption::new("field", ConfigType::String, false, "Wrong constraint.").minimum(1),
            ConfigOption::new("field", ConfigType::Boolean, false, "Wrong choice.")
                .allowed([json!("false")]),
            ConfigOption::new("field", ConfigType::Integer, false, "Reversed bounds.")
                .minimum(5)
                .maximum(4),
        ] {
            assert!(ConfigDefinition::try_new([option]).is_err());
        }
    }

    #[test]
    fn details_cannot_invent_fields_or_publish_secret_defaults() {
        let mut schema = definition().schema();
        schema.fields.remove("count");
        assert!(definition().details().validate_schema(&schema).is_err());
        let mut schema = definition().schema();
        schema.fields.get_mut("count").expect("field").secret = true;
        assert!(definition().details().validate_schema(&schema).is_err());
    }

    #[test]
    fn explicit_null_default_survives_metadata_round_trip() -> Result<()> {
        let definition = ConfigDefinition::try_new([ConfigOption::new(
            "value",
            ConfigType::Json,
            false,
            "Optional JSON value.",
        )
        .default(Value::Null)])?;
        let details: ConfigDetails =
            serde_json::from_slice(&serde_json::to_vec(&definition.details())?)?;
        assert_eq!(details.fields["value"].default, Some(Value::Null));
        let schema = configuration_json_schema(&definition.schema(), Some(&details));
        assert_eq!(
            schema["properties"]["value"].get("default"),
            Some(&Value::Null)
        );
        Ok(())
    }
}
