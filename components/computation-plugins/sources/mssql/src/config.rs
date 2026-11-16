// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Factory configuration: one flat camelCase object, `stream` beside the
//! shared [`NativeSettings`] that the SQL Server bootstrap providers parse
//! too. `password` is the only secret.

use anyhow::{Context, Result};
use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption, ConfigType};
use drasi_mssql_common::{client::Encryption, native::settings::*};
use serde_json::{json, Value};

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

fn string(name: &str, required: bool, description: &str) -> ConfigOption {
    ConfigOption::new(name, ConfigType::String, required, description)
}

fn integer(
    name: &str,
    description: &str,
    default: u64,
    minimum: i64,
    maximum: u64,
) -> ConfigOption {
    ConfigOption::new(name, ConfigType::Integer, false, description)
        .default(default)
        .minimum(minimum)
        .maximum(maximum)
}

/// The published configuration.
pub fn definition() -> ConfigDefinition {
    ConfigDefinition::try_new([
        string(STREAM, true, "Output stream identifier carried by every emitted change."),
        string("host", false, "SQL Server host name or address. Also the endpoint passed to an activation identity provider.").default(DEFAULT_HOST),
        integer("port", "SQL Server TCP port.", u64::from(DEFAULT_PORT), 1, u64::from(u16::MAX)),
        string("database", true, "Database whose CDC capture instances are read."),
        string("user", false, "SQL authentication login. Required unless an activation identity is bound, which supplies the login or a Microsoft Entra ID access token at each start.").default_description("None; required without an identity binding."),
        string("password", false, "SQL authentication password. A secret reference; required unless an activation identity is bound.").secret(),
        string("encryption", false, "required encrypts the session and verifies the server certificate. loginOnly encrypts only the login packet and disabled sends everything in plaintext; both log a warning.")
            .default(json!(Encryption::Required))
            .allowed([json!(Encryption::Required), json!(Encryption::LoginOnly), json!(Encryption::Disabled)]),
        ConfigOption::new("trustServerCertificate", ConfigType::Boolean, false, "Skip server certificate validation (testing only; logs a warning). Mutually exclusive with trustedCaPem.").default(false),
        string("trustedCaPem", false, "PEM CA certificates trusted in addition to the system roots when verifying the server certificate."),
        string("hostNameInCertificate", false, "Host name expected in the server certificate when it differs from host.").default_description("The value of host."),
        integer("connectTimeoutMs", "Deadline for TCP, TLS and login of one connection attempt.", DEFAULT_CONNECT_TIMEOUT_MS, 1, MAX_TIMEOUT_MS),
        integer("commandTimeoutMs", "Deadline for each server round trip (catalog, key and range reads).", DEFAULT_COMMAND_TIMEOUT_MS, 1, MAX_TIMEOUT_MS),
        string("serverTimeZone", false, "The server's time zone as a sys.time_zone_info name (for example UTC or Pacific Standard Time), used to convert CDC commit times to UTC. Required on servers older than SQL Server 2022.").default_description("Detected with CURRENT_TIMEZONE_ID() (SQL Server 2022 and Azure SQL)."),
        ConfigOption::new("tables", ConfigType::Array, false, "Tables to stream, as table or schema.table; bracket-quote names with other characters ([Sales Ops].[Order Lines]). Empty streams every CDC-enabled table except barrierTable; a capture instance added later then stops the source with a schema-change error.").default(json!([])),
        ConfigOption::new("tableKeys", ConfigType::Array, false, "Explicit keys, as {\"table\": \"schema.table\", \"keyColumns\": [..]}, for tables without a primary key. The capture instance must include the key columns.").default(json!([])),
        string("startPosition", false, "Where a source with no committed cursor starts: current (the CDC maximum LSN at first start) or beginning (each capture instance's oldest retained change).")
            .default(json!(StartPosition::Current))
            .allowed([json!(StartPosition::Current), json!(StartPosition::Beginning)]),
        string("barrierTable", false, "Administrator-provisioned CDC table with a uniqueidentifier token column, used by the coordinated bootstrap provider. Erasing removal deletes this source's leftover markers from it."),
        integer("pollIntervalMs", "Delay before polling again after a page with no changes.", DEFAULT_POLL_INTERVAL_MS, 1, MAX_POLL_INTERVAL_MS),
        integer("maxRows", "Page size: the most change groups read and changes emitted per poll.", DEFAULT_MAX_ROWS as u64, 1, MAX_MAX_ROWS as u64),
        integer("maxBytes", "Most record payload bytes per page; one change must fit.", DEFAULT_MAX_BYTES as u64, MIN_MAX_BYTES as i64, MAX_MAX_BYTES as u64),
        integer("reconnectBudgetMs", "Total time to keep reconnecting, with jittered exponential backoff, after a transport failure. 0 disables reconnection.", DEFAULT_RECONNECT_BUDGET_MS, 0, MAX_RECONNECT_BUDGET_MS),
    ])
    .expect("constant SQL Server source configuration")
}

#[cfg(test)]
mod tests {
    use super::*;

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
