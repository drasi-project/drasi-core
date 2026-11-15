// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Factory configuration: one flat camelCase object per component.
//!
//! The connection, publication, slot and operational fields are the shared
//! [`PostgresTransactionConfig`], which the bootstrap providers parse too.
//! Each factory adds its mode fields (`stream`, `coordinatedSnapshot`,
//! `persistent`) at the same level.

use anyhow::{Context, Result};
use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption, ConfigType};
use drasi_postgres_common::replication::settings::*;
use serde_json::{json, Value};

/// Which of the four factories a configuration belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `drasi/postgres-transactions`: whole commits, query-owned progress.
    Transactions,
    /// `drasi.postgres/changes`: one envelope per row change.
    Rows,
    /// `drasi.postgres/shared-changes`: rows with a fixed consumer set.
    SharedRows,
    /// `drasi.postgres/shared-transactions`: whole commits with a fixed
    /// consumer set.
    SharedTransactions,
}

impl Mode {
    fn has_coordinated_snapshot(self) -> bool {
        matches!(self, Self::Transactions | Self::Rows)
    }
    fn has_persistent(self) -> bool {
        matches!(self, Self::Rows | Self::SharedRows)
    }
}

/// A parsed factory configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceConfig {
    pub mode: Mode,
    pub stream: String,
    pub coordinated_snapshot: bool,
    pub persistent: bool,
    pub settings: PostgresTransactionConfig,
}

const STREAM: &str = "stream";
const COORDINATED: &str = "coordinatedSnapshot";
const PERSISTENT: &str = "persistent";

impl SourceConfig {
    /// Parses and statically validates `value` for `mode`. Unknown fields,
    /// including another mode's fields, are rejected.
    pub fn from_value(mode: Mode, value: &Value) -> Result<Self> {
        let mut object = value
            .as_object()
            .context("PostgreSQL source configuration must be an object")?
            .clone();
        let stream = match object.remove(STREAM) {
            Some(Value::String(stream)) => stream,
            Some(_) => anyhow::bail!("stream must be a string"),
            None => anyhow::bail!("stream is required"),
        };
        let mut flag = |name: &str, allowed: bool| -> Result<bool> {
            match object.remove(name) {
                None => Ok(false),
                Some(Value::Bool(value)) if allowed => Ok(value),
                Some(Value::Bool(_)) => anyhow::bail!("{name} is not supported by this factory"),
                Some(_) => anyhow::bail!("{name} must be a boolean"),
            }
        };
        let coordinated_snapshot = flag(COORDINATED, mode.has_coordinated_snapshot())?;
        let persistent = flag(PERSISTENT, mode.has_persistent())?;
        let settings: PostgresTransactionConfig =
            serde_json::from_value(Value::Object(object)).context("invalid PostgreSQL settings")?;
        let config = Self {
            mode,
            stream,
            coordinated_snapshot,
            persistent,
            settings,
        };
        config.validate()?;
        Ok(config)
    }

    /// Static validation: no I/O and no owner identity.
    pub fn validate(&self) -> Result<()> {
        drasi_computation_contracts::StreamId::try_new(self.stream.clone())?;
        self.settings.validate_static()?;
        if self.coordinated_snapshot {
            anyhow::ensure!(
                self.settings.slot_ownership == SlotOwnership::Managed,
                "coordinatedSnapshot creates and hands over its own slots; it requires managed slot ownership"
            );
            anyhow::ensure!(
                self.settings.start_lsn.is_none(),
                "coordinatedSnapshot starts at its snapshot boundary; remove startLsn"
            );
            anyhow::ensure!(
                self.mode != Mode::Rows || self.persistent,
                "coordinated PostgreSQL row bootstrap requires persistent query storage"
            );
        }
        Ok(())
    }

    /// The configuration as the factory accepts it, with defaults filled.
    pub fn to_value(&self) -> Result<Value> {
        let Value::Object(mut object) = serde_json::to_value(&self.settings)? else {
            anyhow::bail!("PostgreSQL settings did not serialize as an object");
        };
        object.insert(STREAM.into(), Value::String(self.stream.clone()));
        if self.mode.has_coordinated_snapshot() {
            object.insert(COORDINATED.into(), Value::Bool(self.coordinated_snapshot));
        }
        if self.mode.has_persistent() {
            object.insert(PERSISTENT.into(), Value::Bool(self.persistent));
        }
        Ok(Value::Object(object))
    }

    /// Whether committed progress survives a process restart.
    pub fn recoverable(&self) -> bool {
        match self.mode {
            Mode::Transactions | Mode::SharedTransactions => true,
            Mode::Rows => self.persistent || self.coordinated_snapshot,
            Mode::SharedRows => self.persistent,
        }
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

/// The published configuration for `mode`.
pub fn definition(mode: Mode) -> ConfigDefinition {
    let mut options = vec![
        string(STREAM, true, "Output stream identifier carried by every emitted change."),
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
        string("publication", true, "Publication to stream. It must publish insert, update and delete, and must not publish truncate."),
        ConfigOption::new("tables", ConfigType::Array, false, "Tables to emit as schema.table (public may be omitted). Empty emits every publication table; a table added to the publication later then stops the source with a schema-change error.")
            .default(json!([])),
        ConfigOption::new("tableKeys", ConfigType::Array, false, "Explicit keys, as {\"table\": \"schema.table\", \"keyColumns\": [..]}, for tables without a usable primary key. The replica identity must carry the key columns.")
            .default(json!([])),
        string("slotOwnership", false, "managed: the source creates its replication slot on first start and drops it on erasing removal; the name is derived from the graph and source identifiers. external: an operator owns slotName and the source never creates or drops it.")
            .default("managed")
            .allowed([json!("managed"), json!("external")]),
        string("slotName", false, "The operator-owned slot for external ownership: 1 to 63 lowercase letters, digits or underscores. Rejected for managed ownership."),
        string("startLsn", false, "Initial cursor (for example 0/16B3748) used before the first checkpoint. When absent, streaming starts at the slot's confirmed position. Not allowed with coordinatedSnapshot."),
        integer("maxTransactionChanges", "Largest number of changes in one source transaction; a larger transaction fails and is retried after the limit is raised.", DEFAULT_MAX_TRANSACTION_CHANGES as u64, 1),
        integer("maxTransactionBytes", "Largest encoded size of one source transaction, in bytes.", DEFAULT_MAX_TRANSACTION_BYTES as u64, 1),
        integer("maxTransactionDurationMs", "Deadline for assembling and publishing one source transaction.", DEFAULT_MAX_TRANSACTION_DURATION_MS, 1),
        integer("maxProtocolBytes", "Largest PostgreSQL protocol message or catalog result accepted, in bytes.", DEFAULT_MAX_PROTOCOL_BYTES as u64, 64),
        integer("ioTimeoutMs", "Deadline for connecting, catalog queries, feedback writes and each start.", DEFAULT_IO_TIMEOUT_MS, 1),
        integer("feedbackIntervalMs", "Interval between standby status messages. Keep it below the server's wal_sender_timeout (60 s by default); paused sources rely on it to stay connected.", DEFAULT_FEEDBACK_INTERVAL_MS, 1),
        ConfigOption::new("reconnectBudgetMs", ConfigType::Integer, false, "Total time to keep reconnecting, with jittered exponential backoff, after a transport failure. 0 disables reconnection.")
            .default(DEFAULT_RECONNECT_BUDGET_MS)
            .minimum(0)
            .maximum(MAX_RECONNECT_BUDGET_MS),
    ];
    if mode.has_coordinated_snapshot() {
        options.push(
            ConfigOption::new(COORDINATED, ConfigType::Boolean, false, "Start from a coordinated snapshot taken by the paired bootstrap provider, which creates and hands over a managed slot.")
                .default(false),
        );
    }
    if mode.has_persistent() {
        options.push(
            ConfigOption::new(
                PERSISTENT,
                ConfigType::Boolean,
                false,
                "Require persistent consumer progress so committed rows survive a process restart.",
            )
            .default(false),
        );
    }
    ConfigDefinition::try_new(options).expect("constant PostgreSQL source configuration")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Value {
        json!({
            "stream": "changes",
            "database": "app",
            "user": "drasi",
            "publication": "pub"
        })
    }

    #[test]
    fn round_trips_with_defaults_and_rejects_foreign_fields() {
        for mode in [
            Mode::Transactions,
            Mode::Rows,
            Mode::SharedRows,
            Mode::SharedTransactions,
        ] {
            let config = SourceConfig::from_value(mode, &base()).unwrap();
            let value = config.to_value().unwrap();
            definition(mode).schema().validate(&value).unwrap();
            assert_eq!(SourceConfig::from_value(mode, &value).unwrap(), config);
            let mut unknown = base();
            unknown["autoCreateSlot"] = json!(true);
            assert!(SourceConfig::from_value(mode, &unknown).is_err());
        }
        let mut coordinated = base();
        coordinated["coordinatedSnapshot"] = json!(true);
        assert!(SourceConfig::from_value(Mode::SharedRows, &coordinated).is_err());
        let mut persistent = base();
        persistent["persistent"] = json!(true);
        assert!(SourceConfig::from_value(Mode::Transactions, &persistent).is_err());
    }

    #[test]
    fn coordinated_snapshot_requires_managed_slots_and_no_start_lsn() {
        let mut value = base();
        value["coordinatedSnapshot"] = json!(true);
        SourceConfig::from_value(Mode::Transactions, &value).unwrap();
        let mut external = value.clone();
        external["slotOwnership"] = json!("external");
        external["slotName"] = json!("ops");
        assert!(SourceConfig::from_value(Mode::Transactions, &external).is_err());
        let mut start = value.clone();
        start["startLsn"] = json!("0/16B3748");
        assert!(SourceConfig::from_value(Mode::Transactions, &start).is_err());
        assert!(
            SourceConfig::from_value(Mode::Rows, &value).is_err(),
            "coordinated rows need persistent storage"
        );
        value["persistent"] = json!(true);
        SourceConfig::from_value(Mode::Rows, &value).unwrap();
    }

    #[test]
    fn password_is_a_top_level_secret() {
        let schema = definition(Mode::Transactions).schema();
        assert!(schema.fields["password"].secret);
        assert!(schema.fields.values().filter(|field| field.secret).count() == 1);
    }
}
