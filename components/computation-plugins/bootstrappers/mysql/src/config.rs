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
use drasi_mysql_common::native::{settings::*, MySqlConfig, MySqlOutput, MySqlSourceSettings};
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

fn string(name: &str, required: bool, description: &str) -> ConfigOption {
    ConfigOption::new(name, ConfigType::String, required, description)
}

fn integer(name: &str, description: &str, default: u64, minimum: i64) -> ConfigOption {
    ConfigOption::new(name, ConfigType::Integer, false, description)
        .default(default)
        .minimum(minimum)
}

/// The shared options. They mirror the native MySQL source's definition;
/// a unit test checks both describe exactly [`MySqlSourceSettings`].
fn shared_options() -> Vec<ConfigOption> {
    vec![
        string("host", false, "MySQL host name or address.").default("localhost"),
        ConfigOption::new("port", ConfigType::Integer, false, "MySQL port.")
            .default(DEFAULT_PORT)
            .minimum(1)
            .maximum(65_535),
        string("database", true, "Database (schema) to load. Any MySQL identifier."),
        string("user", true, "Login user. Coordinated loading needs RELOAD (FLUSH TABLES WITH READ LOCK), REPLICATION CLIENT, REPLICATION SLAVE and SELECT; standalone loading needs SELECT."),
        string("password", false, "Login password. A secret reference.").secret(),
        string("sslMode", false, "disabled, if_available, require, require_verify_ca or require_verify_full.")
            .default("if_available")
            .allowed([json!("disabled"), json!("if_available"), json!("require"), json!("require_verify_ca"), json!("require_verify_full")]),
        string("tlsCaPem", false, "Additional trusted CA certificates (PEM, at most 64 KiB)."),
        string("tlsClientCertPem", false, "Client certificate chain (PEM) for mutual TLS."),
        string("tlsClientKeyPem", false, "PKCS#8 private key (PEM) of tlsClientCertPem. A secret reference.").secret(),
        ConfigOption::new("serverId", ConfigType::Integer, false, "The paired source's replica server ID.")
            .minimum(1)
            .maximum(u64::from(u32::MAX))
            .default_description("derived from the source component ID"),
        integer("heartbeatIntervalMs", "The paired source's heartbeat period.", DEFAULT_HEARTBEAT_INTERVAL_MS, 1),
        ConfigOption::new("tables", ConfigType::Array, false, "Tables to load; the standalone snapshot requires an explicit list.")
            .default(json!([])),
        ConfigOption::new("tableKeys", ConfigType::Array, false, "Explicit keys, as {\"table\": \"name\", \"keyColumns\": [..]}.")
            .default(json!([])),
        string("startFrom", false, "Coordinated loading requires snapshot; the standalone snapshot requires end.")
            .default("end")
            .allowed([json!("end"), json!("beginning"), json!("gtid"), json!("position"), json!("snapshot")]),
        string("startGtidSet", false, "Unused by bootstrap; accepted so the source configuration can be shared."),
        string("startBinlogFile", false, "Unused by bootstrap; accepted so the source configuration can be shared."),
        ConfigOption::new("startBinlogPosition", ConfigType::Integer, false, "Unused by bootstrap; accepted so the source configuration can be shared.")
            .minimum(4)
            .maximum(u64::from(u32::MAX)),
        integer("snapshotLockTimeoutMs", "Longest wait for FLUSH TABLES WITH READ LOCK.", DEFAULT_SNAPSHOT_LOCK_TIMEOUT_MS, 1),
        integer("maxTransactionChanges", "The paired source's transaction change limit.", DEFAULT_MAX_TRANSACTION_CHANGES as u64, 1),
        integer("maxTransactionBytes", "The paired source's transaction byte limit.", DEFAULT_MAX_TRANSACTION_BYTES as u64, 1),
        integer("maxTransactionDurationMs", "The paired source's transaction deadline.", DEFAULT_MAX_TRANSACTION_DURATION_MS, 1),
        integer("maxProtocolBytes", "Largest MySQL packet or row accepted, in bytes.", DEFAULT_MAX_PROTOCOL_BYTES as u64, 256),
        integer("ioTimeoutMs", "Deadline for connecting and each packet exchange.", DEFAULT_IO_TIMEOUT_MS, 1),
        ConfigOption::new("reconnectBudgetMs", ConfigType::Integer, false, "The paired source's reconnect budget; unused by bootstrap.")
            .default(DEFAULT_RECONNECT_BUDGET_MS)
            .minimum(0)
            .maximum(MAX_RECONNECT_BUDGET_MS),
    ]
}

/// The published standalone configuration.
pub fn definition() -> ConfigDefinition {
    let mut options = shared_options();
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
    fn shared_options_describe_exactly_the_shared_settings() {
        let mut settings = MySqlSourceSettings::new("h", 3306, "app", "u");
        settings.tls_ca_pem = Some("x".into());
        settings.tls_client_cert_pem = Some("x".into());
        settings.tls_client_key_pem = Some("x".into());
        settings.server_id = std::num::NonZeroU32::new(1);
        settings.start_gtid_set = Some("x".into());
        settings.start_binlog_file = Some("x".into());
        settings.start_binlog_position = Some(4);
        let serialized = serde_json::to_value(&settings).unwrap();
        let mut expected: Vec<_> = serialized.as_object().unwrap().keys().cloned().collect();
        expected.sort();
        let mut published: Vec<_> = definition()
            .schema()
            .fields
            .keys()
            .map(|name| name.to_string())
            .filter(|name| name != "labels")
            .collect();
        published.sort();
        assert_eq!(published, expected);
        let defaults =
            serde_json::to_value(MySqlSourceSettings::new("localhost", 3306, "a", "u")).unwrap();
        for (name, field) in definition().details().fields {
            if let (Some(default), Some(actual)) = (field.default, defaults.get(&name)) {
                assert_eq!(actual, &default, "{name}");
            }
        }
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
