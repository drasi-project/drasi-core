// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Bootstrap configuration: one flat camelCase object, the same shared
//! [`MySqlSourceSettings`] as the native MySQL source plus `source`,
//! `stream`, `retention` (coordinated factories) and `labels` (standalone).
//!
//! Use the same values as the live source: the coordinated handover is bound
//! to the source's selected tables, keys, stream and identity.

use anyhow::{Context, Result};
use drasi_computation_contracts::{ComponentId, StreamId};
use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption, ConfigType};
use drasi_mysql_common::native::{
    settings::*, MySqlConfig, MySqlOutput, MySqlRetention, MySqlSourceSettings,
};
use serde_json::{json, Value};

/// Which provider a configuration belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Coordinated loading for whole-transaction sources.
    Transactions,
    /// Coordinated loading for shared row sources.
    Rows,
    /// Repeatable-read loading without a handover.
    Standalone,
}

/// A parsed bootstrap configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct BootstrapConfig {
    pub source: ComponentId,
    pub stream: StreamId,
    pub retention: MySqlRetention,
    pub labels: Vec<String>,
    pub settings: MySqlSourceSettings,
}

impl BootstrapConfig {
    /// Parses `value` for `kind`, rejecting unknown and foreign fields.
    pub fn from_value(kind: Kind, value: &Value) -> Result<Self> {
        let mut object = value
            .as_object()
            .context("MySQL bootstrap configuration must be an object")?
            .clone();
        let text = |object: &mut serde_json::Map<String, Value>, name: &str| -> Result<String> {
            match object.remove(name) {
                Some(Value::String(value)) => Ok(value),
                Some(_) => anyhow::bail!("{name} must be a string"),
                None => anyhow::bail!("{name} is required"),
            }
        };
        let source = ComponentId::try_new(text(&mut object, "source")?)?;
        let stream = StreamId::try_new(text(&mut object, "stream")?)?;
        let retention = match object.remove("retention") {
            None => MySqlRetention::Server,
            Some(_) if kind == Kind::Standalone => {
                anyhow::bail!("retention is not supported by the standalone snapshot")
            }
            Some(value) => match value.as_str() {
                Some("server") => MySqlRetention::Server,
                Some("untilProcessed") => MySqlRetention::UntilProcessed,
                _ => anyhow::bail!("retention must be \"server\" or \"untilProcessed\""),
            },
        };
        let labels = match object.remove("labels") {
            None => Vec::new(),
            Some(_) if kind != Kind::Standalone => {
                anyhow::bail!("labels are only supported by the standalone snapshot")
            }
            Some(value) => serde_json::from_value(value).context("labels must be strings")?,
        };
        let settings: MySqlSourceSettings =
            serde_json::from_value(Value::Object(object)).context("invalid MySQL settings")?;
        let config = Self {
            source,
            stream,
            retention,
            labels,
            settings,
        };
        config.engine_config(kind)?;
        Ok(config)
    }

    /// The engine configuration, with `serverId` derived from the paired
    /// source's identity like the source derives it.
    pub fn engine_config(&self, kind: Kind) -> Result<MySqlConfig> {
        let (output, replay) = match kind {
            Kind::Transactions => (MySqlOutput::Transactions, Some(self.retention)),
            Kind::Rows => (MySqlOutput::Changes, Some(self.retention)),
            Kind::Standalone => (MySqlOutput::Changes, None),
        };
        let config = self.settings.engine_config(&self.source, output, replay)?;
        match kind {
            Kind::Rows => config.validate_rows()?,
            _ => config.validate()?,
        }
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

/// The published configuration for `kind`.
pub fn definition(kind: Kind) -> ConfigDefinition {
    let mut options = vec![
        ConfigOption::new(
            "source",
            ConfigType::ComponentId,
            true,
            "The paired native MySQL source component.",
        ),
        string("stream", true, "The paired source's output stream."),
    ];
    options.extend(shared_options());
    match kind {
        Kind::Standalone => options.push(
            ConfigOption::new("labels", ConfigType::Array, false, "Restricts loading to these tables of the allowlist; empty loads the whole allowlist.")
                .default(json!([])),
        ),
        _ => options.push(
            string("retention", false, "The paired source's retention: server or untilProcessed.")
                .default("server")
                .allowed([json!("server"), json!("untilProcessed")]),
        ),
    }
    ConfigDefinition::try_new(options).expect("constant MySQL bootstrap configuration")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Value {
        json!({
            "source": "8b637385-6167-54c4-9589-3dda2fcd1d21",
            "stream": "changes",
            "database": "app",
            "user": "drasi",
            "startFrom": "snapshot"
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
        let mut published: Vec<_> = definition(Kind::Rows)
            .schema()
            .fields
            .keys()
            .map(|name| name.to_string())
            .filter(|name| !matches!(name.as_str(), "source" | "stream" | "retention"))
            .collect();
        published.sort();
        assert_eq!(published, expected);
        let defaults =
            serde_json::to_value(MySqlSourceSettings::new("localhost", 3306, "a", "u")).unwrap();
        for (name, field) in definition(Kind::Rows).details().fields {
            if let (Some(default), Some(actual)) = (field.default, defaults.get(&name)) {
                assert_eq!(actual, &default, "{name}");
            }
        }
    }

    #[test]
    fn flat_configuration_parses_per_kind() {
        let config = BootstrapConfig::from_value(Kind::Transactions, &base()).unwrap();
        assert_eq!(config.retention, MySqlRetention::Server);
        let mut rows = base();
        rows["retention"] = json!("untilProcessed");
        assert_eq!(
            BootstrapConfig::from_value(Kind::Rows, &rows)
                .unwrap()
                .retention,
            MySqlRetention::UntilProcessed
        );
        let mut labels = base();
        labels["labels"] = json!(["items"]);
        assert!(BootstrapConfig::from_value(Kind::Transactions, &labels).is_err());
        let mut standalone = base();
        standalone["startFrom"] = json!("end");
        standalone["tables"] = json!(["items"]);
        standalone["labels"] = json!(["items"]);
        BootstrapConfig::from_value(Kind::Standalone, &standalone).unwrap();
        let mut legacy = base();
        legacy["settings"] = json!({});
        assert!(BootstrapConfig::from_value(Kind::Transactions, &legacy).is_err());
        let mut fake = base();
        fake["source"] = json!("source");
        assert!(BootstrapConfig::from_value(Kind::Transactions, &fake).is_err());
        for kind in [Kind::Transactions, Kind::Rows, Kind::Standalone] {
            let schema = definition(kind).schema();
            let secret: Vec<_> = schema
                .fields
                .iter()
                .filter(|(_, field)| field.secret)
                .map(|(name, _)| name.to_string())
                .collect();
            assert_eq!(secret, vec!["password", "tlsClientKeyPem"]);
            definition(kind).details().validate_schema(&schema).unwrap();
        }
    }
}
