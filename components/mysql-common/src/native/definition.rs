// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The published options of [`MySqlSourceSettings`]: one description,
//! default and bound per flat setting, shared by the native source factories
//! and the bootstrap providers so their schemas cannot drift.

use super::settings::*;
use drasi_computation_plugin_sdk::{ConfigOption, ConfigType};
use serde_json::json;

fn string(name: &str, required: bool, description: &str) -> ConfigOption {
    ConfigOption::new(name, ConfigType::String, required, description)
}

fn integer(name: &str, description: &str, default: u64, minimum: i64) -> ConfigOption {
    ConfigOption::new(name, ConfigType::Integer, false, description)
        .default(default)
        .minimum(minimum)
}

/// Every [`MySqlSourceSettings`] option, used by the source factories and
/// the bootstrap providers.
pub fn options() -> Vec<ConfigOption> {
    vec![
        string("host", false, "MySQL host name or address.").default("localhost"),
        ConfigOption::new("port", ConfigType::Integer, false, "MySQL port.")
            .default(DEFAULT_PORT)
            .minimum(1)
            .maximum(65_535),
        string("database", true, "Database (schema) to capture. Any MySQL identifier."),
        string("user", true, "Login user. It needs REPLICATION SLAVE, REPLICATION CLIENT and SELECT on the captured tables (see CONFIGURATION.md)."),
        string("password", false, "Login password. A secret reference; omit when the host supplies activation credentials.").secret(),
        string("sslMode", false, "disabled, if_available, require, require_verify_ca or require_verify_full. Only require_verify_full verifies the server certificate and host name.")
            .default("if_available")
            .allowed([json!("disabled"), json!("if_available"), json!("require"), json!("require_verify_ca"), json!("require_verify_full")]),
        string("tlsCaPem", false, "Additional trusted CA certificates (PEM, at most 64 KiB) for server verification."),
        string("tlsClientCertPem", false, "Client certificate chain (PEM) for mutual TLS. Requires tlsClientKeyPem."),
        string("tlsClientKeyPem", false, "PKCS#8 private key (PEM) of tlsClientCertPem. A secret reference.").secret(),
        ConfigOption::new("serverId", ConfigType::Integer, false, "Replica server ID used for the binlog dump. Must be unique among the server's replicas.")
            .minimum(1)
            .maximum(u64::from(u32::MAX))
            .default_description("derived from the component ID"),
        integer("heartbeatIntervalMs", "Heartbeat period requested from the server. No event or heartbeat for three periods (at least ioTimeoutMs) is a dead connection.", DEFAULT_HEARTBEAT_INTERVAL_MS, 1),
        ConfigOption::new("tables", ConfigType::Array, false, "Tables to capture. Empty captures every supported base table and skips (with a warning) tables it cannot capture.")
            .default(json!([])),
        ConfigOption::new("tableKeys", ConfigType::Array, false, "Explicit keys, as {\"table\": \"name\", \"keyColumns\": [..]}, for tables without a usable primary key. Positional factories use @N column offsets.")
            .default(json!([])),
        string("startFrom", false, "Where streaming starts before the first checkpoint: end, beginning, gtid (startGtidSet), position (startBinlogFile and startBinlogPosition) or snapshot (a coordinated bootstrap).")
            .default("end")
            .allowed([json!("end"), json!("beginning"), json!("gtid"), json!("position"), json!("snapshot")]),
        string("startGtidSet", false, "GTID set for startFrom gtid."),
        string("startBinlogFile", false, "Binary log file for startFrom position."),
        ConfigOption::new("startBinlogPosition", ConfigType::Integer, false, "Offset in startBinlogFile for startFrom position (at least 4).")
            .minimum(4)
            .maximum(u64::from(u32::MAX)),
        integer("snapshotLockTimeoutMs", "Longest wait for FLUSH TABLES WITH READ LOCK while a coordinated snapshot establishes its view.", DEFAULT_SNAPSHOT_LOCK_TIMEOUT_MS, 1),
        integer("maxTransactionChanges", "Largest number of changes in one source transaction.", DEFAULT_MAX_TRANSACTION_CHANGES as u64, 1),
        integer("maxTransactionBytes", "Largest encoded size of one source transaction, in bytes.", DEFAULT_MAX_TRANSACTION_BYTES as u64, 1),
        integer("maxTransactionDurationMs", "Deadline for assembling and publishing one source transaction.", DEFAULT_MAX_TRANSACTION_DURATION_MS, 1),
        integer("maxProtocolBytes", "Largest MySQL packet, binlog event or catalog result accepted, in bytes. Also bounds the pause buffer.", DEFAULT_MAX_PROTOCOL_BYTES as u64, 256),
        integer("ioTimeoutMs", "Deadline for connecting, catalog queries and each packet exchange; also bounds connection setup during start.", DEFAULT_IO_TIMEOUT_MS, 1),
        ConfigOption::new("reconnectBudgetMs", ConfigType::Integer, false, "Total time to keep reconnecting, with jittered exponential backoff, after a transport failure. 0 disables reconnection.")
            .default(DEFAULT_RECONNECT_BUDGET_MS)
            .minimum(0)
            .maximum(MAX_RECONNECT_BUDGET_MS),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native::MySqlSourceSettings;
    use drasi_computation_plugin_sdk::ConfigDefinition;

    #[test]
    fn options_describe_exactly_the_settings_and_their_defaults() {
        let mut settings = MySqlSourceSettings::new("h", 3306, "app", "u");
        settings.tls_ca_pem = Some("x".into());
        settings.tls_client_cert_pem = Some("x".into());
        settings.tls_client_key_pem = Some("x".into());
        settings.server_id = std::num::NonZeroU32::new(1);
        settings.start_gtid_set = Some("x".into());
        settings.start_binlog_file = Some("x".into());
        settings.start_binlog_position = Some(4);
        let serialized = serde_json::to_value(&settings).unwrap();
        let definition = ConfigDefinition::try_new(options()).unwrap();
        assert!(definition
            .schema()
            .fields
            .keys()
            .eq(serialized.as_object().unwrap().keys()));
        let defaults =
            serde_json::to_value(MySqlSourceSettings::new("localhost", 3306, "a", "u")).unwrap();
        for (name, field) in definition.details().fields {
            if let (Some(default), Some(actual)) = (field.default, defaults.get(&name)) {
                assert_eq!(actual, &default, "{name}");
            }
        }
    }
}
