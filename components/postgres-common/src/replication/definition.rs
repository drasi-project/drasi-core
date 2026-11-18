// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The published options of [`PostgresTransactionConfig`]: one description,
//! default and bound per flat setting, shared by the native source factories
//! and the standalone snapshot so their schemas cannot drift.

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

/// Every [`PostgresTransactionConfig`] option.
pub fn options() -> Vec<ConfigOption> {
    vec![
        string("host", false, "PostgreSQL host name or address.").default("localhost"),
        ConfigOption::new("port", ConfigType::Integer, false, "PostgreSQL port.")
            .default(DEFAULT_PORT)
            .minimum(1)
            .maximum(65_535),
        string("database", true, "Database to replicate. Must match the server's reported database name."),
        string("user", true, "Login role. It needs the REPLICATION attribute and SELECT on the published tables; managed slots also need permission to create and drop slots."),
        string("password", false, "Login password. A secret reference; omit when the host supplies activation credentials.").secret(),
        string("sslMode", false, "disable, prefer or require. require verifies the server certificate and host name; prefer falls back to plaintext only when the server refuses TLS.")
            .default("prefer")
            .allowed([json!("disable"), json!("prefer"), json!("require")]),
        string("tlsCaPem", false, "Additional trusted CA certificates (PEM, at most 64 KiB) for server verification."),
        string("publication", true, "Publication to stream. It must publish insert, update and delete, and must not publish truncate; a TRUNCATE of a selected table published later fails the source."),
        ConfigOption::new("tables", ConfigType::Array, false, "Tables to emit as schema.table (public may be omitted). Empty emits every publication table; a table added to the publication later then stops the source with a schema-change error.")
            .default(json!([])),
        ConfigOption::new("tableKeys", ConfigType::Array, false, "Explicit keys, as {\"table\": \"schema.table\", \"keyColumns\": [..]}, for tables without a usable primary key. The replica identity must carry the key columns.")
            .default(json!([])),
        string("slotOwnership", false, "managed: the source creates its replication slot on first start and drops it on erasing removal; the name is derived from the graph and source identifiers. external: an operator owns slotName and the source never creates or drops it.")
            .default("managed")
            .allowed([json!("managed"), json!("external")]),
        string("slotName", false, "The operator-owned slot for external ownership: 1 to 63 lowercase letters, digits or underscores. Rejected for managed ownership."),
        string("startLsn", false, "Initial cursor (for example 0/16B3748) used before the first checkpoint. When absent, streaming starts at the slot's confirmed position. Not allowed with coordinatedSnapshot."),
        integer("maxTransactionChanges", "Largest number of changes in one whole transaction: a larger commit fails without publishing anything of it, the slot keeps its WAL, and it replays whole after the limit is raised. Row outputs buffer up to this many changes and stream larger commits instead.", DEFAULT_MAX_TRANSACTION_CHANGES as u64, 1),
        integer("maxTransactionBytes", "Largest encoded size of one whole transaction, in bytes, with the same behaviour as maxTransactionChanges (row outputs stream larger commits).", DEFAULT_MAX_TRANSACTION_BYTES as u64, 1),
        integer("maxTransactionDurationMs", "Deadline for assembling and publishing one whole transaction (row outputs: for buffering one).", DEFAULT_MAX_TRANSACTION_DURATION_MS, 1),
        integer("maxProtocolBytes", "Largest PostgreSQL protocol message or catalog result accepted, in bytes.", DEFAULT_MAX_PROTOCOL_BYTES as u64, 64),
        integer("ioTimeoutMs", "Deadline for connecting, catalog queries, feedback writes and each start.", DEFAULT_IO_TIMEOUT_MS, 1),
        integer("feedbackIntervalMs", "Interval between standby status messages. Keep it below the server's wal_sender_timeout (60 s by default); paused sources rely on it to stay connected.", DEFAULT_FEEDBACK_INTERVAL_MS, 1),
        ConfigOption::new("reconnectBudgetMs", ConfigType::Integer, false, "Total time to keep reconnecting, with jittered exponential backoff, after a transport failure. 0 disables reconnection.")
            .default(DEFAULT_RECONNECT_BUDGET_MS)
            .minimum(0)
            .maximum(MAX_RECONNECT_BUDGET_MS),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_computation_plugin_sdk::ConfigDefinition;

    #[test]
    fn options_describe_exactly_the_settings_and_their_defaults() {
        let definition = ConfigDefinition::try_new(options()).unwrap();
        let minimal = json!({"database": "app", "user": "u", "publication": "p"});
        let parsed: PostgresTransactionConfig = serde_json::from_value(minimal.clone()).unwrap();
        let exposed = serde_json::to_value(&parsed).unwrap();
        let schema = definition.schema();
        for name in exposed.as_object().unwrap().keys() {
            assert!(schema.fields.contains_key(name), "{name} is published");
        }
        for (name, field) in definition.details().fields {
            if let Some(default) = field.default {
                // An empty list is omitted from the serialized settings.
                let empty = json!([]);
                let value = exposed.get(&name).unwrap_or(&empty);
                assert_eq!(value, &default, "{name}");
            }
        }
    }
}
