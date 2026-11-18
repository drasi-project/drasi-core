// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The standalone snapshot's configuration: one flat camelCase object, the
//! same shared [`MySqlSourceSettings`] as the native MySQL source plus
//! `labels`. The served source (whose element identities and server id the
//! snapshot uses) and its stream come from the graph relation, not from this
//! configuration. The coordinated providers take no configuration: the
//! paired source's coordinator uses the source's own settings.

use anyhow::{Context, Result};
use drasi_computation_contracts::ComponentId;
use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption, ConfigType};
use drasi_mysql_common::native::{definition, MySqlConfig, MySqlOutput, MySqlSourceSettings};
use serde_json::{json, Value};

/// A parsed standalone snapshot configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct BootstrapConfig {
    /// Upper bound on the tables loaded, within `settings.tables`; empty
    /// loads the whole allowlist. A query's labels narrow it further.
    pub labels: Vec<String>,
    pub settings: MySqlSourceSettings,
}

impl BootstrapConfig {
    /// Parses `value`, rejecting unknown and foreign fields.
    pub fn from_value(value: &Value) -> Result<Self> {
        let mut object = value
            .as_object()
            .context("MySQL bootstrap configuration must be an object")?
            .clone();
        let labels = match object.remove("labels") {
            None => Vec::new(),
            Some(value) => serde_json::from_value(value).context("labels must be strings")?,
        };
        let settings: MySqlSourceSettings =
            serde_json::from_value(Value::Object(object)).context("invalid MySQL settings")?;
        Ok(Self { labels, settings })
    }

    /// The engine configuration for the served `source`, with `serverId`
    /// derived from its identity like the source derives it.
    pub fn engine_config(&self, source: &ComponentId) -> Result<MySqlConfig> {
        let config = self
            .settings
            .engine_config(source, MySqlOutput::Changes, None)?;
        config.validate()?;
        Ok(config)
    }
}

/// The published standalone configuration: `labels` beside the native
/// source's own settings options (`drasi_mysql_common::native::definition`).
pub fn definition() -> ConfigDefinition {
    let mut options = definition::options();
    options.push(
        ConfigOption::new("labels", ConfigType::Array, false, "Upper bound on the tables loaded, within the allowlist; empty loads the whole allowlist. Each query's labels narrow it further.")
            .default(json!([])),
    );
    ConfigDefinition::try_new(options).expect("constant MySQL bootstrap configuration")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Value {
        json!({
            "database": "app",
            "user": "drasi",
            "tables": ["items"]
        })
    }

    #[test]
    fn options_are_the_source_settings_options_plus_labels() {
        let source = ConfigDefinition::try_new(definition::options()).unwrap();
        let published = definition().schema();
        let mut names: Vec<_> = published.fields.keys().cloned().collect();
        names.retain(|name| name != "labels");
        assert!(names.iter().eq(source.schema().fields.keys()));
        drasi_computation_plugin_sdk::testing::ensure_camel_case_configuration(&published).unwrap();
        let mut nested = base();
        nested["tableKeys"] = json!([{"table": "items", "key_columns": ["id"]}]);
        assert!(BootstrapConfig::from_value(&nested).is_err());
    }

    #[test]
    fn flat_configuration_has_no_source_or_stream() {
        let mut labels = base();
        labels["labels"] = json!(["items"]);
        let config = BootstrapConfig::from_value(&labels).unwrap();
        assert_eq!(config.labels, ["items"]);
        let mut legacy = base();
        legacy["settings"] = json!({});
        assert!(BootstrapConfig::from_value(&legacy).is_err());
        for field in ["source", "stream", "retention"] {
            let mut foreign = base();
            foreign[field] = json!("x");
            assert!(
                BootstrapConfig::from_value(&foreign).is_err(),
                "{field} comes from the graph relation or the paired source"
            );
        }
        let schema = definition().schema();
        let secret: Vec<_> = schema
            .fields
            .iter()
            .filter(|(_, field)| field.secret)
            .map(|(name, _)| name.to_string())
            .collect();
        assert_eq!(secret, vec!["password", "tlsClientKeyPem"]);
        definition().details().validate_schema(&schema).unwrap();
    }
}
