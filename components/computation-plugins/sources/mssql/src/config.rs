// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Factory configuration: one flat camelCase object, `stream` beside the
//! shared [`NativeSettings`] that the SQL Server bootstrap providers parse
//! too. `password` is the only secret.

use anyhow::{Context, Result};
use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption, ConfigType};
use drasi_mssql_common::native::{definition, settings::*};
use serde_json::Value;

const STREAM: &str = "stream";

/// A parsed factory configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceConfig {
    pub stream: String,
    pub settings: NativeSettings,
}

impl SourceConfig {
    /// Parses and statically validates `value`. Unknown fields, including
    /// the removed nested `settings` object, are rejected.
    pub fn from_value(value: &Value) -> Result<Self> {
        let mut object = value
            .as_object()
            .context("SQL Server source configuration must be an object")?
            .clone();
        let stream = match object.remove(STREAM) {
            Some(Value::String(stream)) => stream,
            Some(_) => anyhow::bail!("stream must be a string"),
            None => anyhow::bail!("stream is required"),
        };
        let settings = NativeSettings::from_value(Value::Object(object))?;
        let config = Self { stream, settings };
        config.validate()?;
        Ok(config)
    }

    /// Static validation: no I/O and no credentials requirement.
    pub fn validate(&self) -> Result<()> {
        drasi_computation_contracts::StreamId::try_new(self.stream.clone())?;
        self.settings.validate()
    }

    /// The configuration as the factory accepts it, with defaults filled.
    pub fn to_value(&self) -> Result<Value> {
        let Value::Object(mut object) = serde_json::to_value(&self.settings)? else {
            anyhow::bail!("SQL Server settings did not serialize as an object");
        };
        object.insert(STREAM.into(), Value::String(self.stream.clone()));
        Ok(Value::Object(object))
    }
}

/// The published configuration: `stream` beside the shared settings
/// options of `drasi_mssql_common::native::definition`.
pub fn definition() -> ConfigDefinition {
    let mut options = vec![ConfigOption::new(
        STREAM,
        ConfigType::String,
        true,
        "Output stream identifier carried by every emitted change.",
    )];
    options.extend(definition::options());
    ConfigDefinition::try_new(options).expect("constant SQL Server source configuration")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Value {
        json!({"stream": "items/out", "database": "app", "user": "drasi", "password": "p"})
    }

    #[test]
    fn round_trips_with_defaults_and_published_defaults_agree() {
        let config = SourceConfig::from_value(&base()).unwrap();
        let value = config.to_value().unwrap();
        let schema = definition().schema();
        schema.validate(&value).unwrap();
        assert_eq!(SourceConfig::from_value(&value).unwrap(), config);
        let details = definition().details();
        details.validate_schema(&schema).unwrap();
        assert!(details
            .equivalent_with_defaults(&schema, &base(), &value)
            .unwrap());
        for name in SETTINGS_FIELDS {
            assert!(schema.fields.contains_key(*name), "{name} is published");
        }
        assert_eq!(schema.fields.len(), SETTINGS_FIELDS.len() + 1);
    }

    #[test]
    fn password_is_the_only_top_level_secret() {
        let schema = definition().schema();
        let secrets: Vec<_> = schema
            .fields
            .iter()
            .filter(|(_, field)| field.secret)
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(secrets, ["password"]);
    }

    #[test]
    fn legacy_nested_and_misspelled_fields_are_rejected() {
        for (field, value) in [
            ("settings", json!({"connection": {"host": "h"}})),
            ("connection", json!({})),
            ("pasword", json!("x")),
            ("barrier_table", json!("t")),
            ("shutdown_timeout_ms", json!(1)),
        ] {
            let mut config = base();
            config[field] = value;
            assert!(SourceConfig::from_value(&config).is_err(), "{field}");
        }
        let mut missing = base();
        missing.as_object_mut().unwrap().remove("stream");
        assert!(SourceConfig::from_value(&missing).is_err());
    }
}
