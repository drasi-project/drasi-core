// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Factory configuration: one flat camelCase object per component.
//!
//! The connection, selection, start and operational fields are the shared
//! [`MySqlSourceSettings`], which the native MySQL bootstrap providers parse
//! too. Each factory adds its mode fields (`stream`, and where it applies
//! `retention` and `persistent`) at the same level.

use anyhow::{Context, Result};
use drasi_computation_contracts::ComponentId;
use drasi_computation_plugin_sdk::{ConfigDefinition, ConfigOption, ConfigType};
use drasi_mysql_common::native::{
    definition, settings::*, MySqlConfig, MySqlOutput, MySqlRetention, MySqlSourceSettings,
};
use serde_json::{json, Value};

/// Which of the seven factories a configuration belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `drasi.mysql/changes`: untracked row changes.
    Changes,
    /// `drasi.mysql/untracked-transactions`: untracked whole transactions.
    UntrackedTransactions,
    /// `drasi.mysql/transactions`: whole transactions, query-owned progress.
    Transactions,
    /// `drasi.mysql/shared-transactions`: whole transactions, fixed consumer set.
    SharedTransactions,
    /// `drasi.mysql/shared-changes`: row changes, fixed consumer set.
    SharedChanges,
    /// `drasi.mysql/positional-changes`: untracked rows keyed by `@N` offsets.
    PositionalChanges,
    /// `drasi.mysql/shared-positional-changes`: positional rows, fixed consumer set.
    SharedPositionalChanges,
}

/// Every mode, in factory registration order.
pub const MODES: [Mode; 7] = [
    Mode::Changes,
    Mode::UntrackedTransactions,
    Mode::Transactions,
    Mode::SharedTransactions,
    Mode::SharedChanges,
    Mode::PositionalChanges,
    Mode::SharedPositionalChanges,
];

impl Mode {
    /// The factory's implementation name.
    pub fn implementation(self) -> &'static str {
        match self {
            Self::Changes => crate::MYSQL_CHANGES,
            Self::UntrackedTransactions => crate::MYSQL_UNTRACKED_TRANSACTIONS,
            Self::Transactions => crate::MYSQL_TRANSACTIONS,
            Self::SharedTransactions => crate::MYSQL_SHARED_TRANSACTIONS,
            Self::SharedChanges => crate::MYSQL_SHARED_CHANGES,
            Self::PositionalChanges => crate::MYSQL_POSITIONAL_CHANGES,
            Self::SharedPositionalChanges => crate::MYSQL_SHARED_POSITIONAL_CHANGES,
        }
    }
    /// The output port format.
    pub fn output(self) -> MySqlOutput {
        match self {
            Self::UntrackedTransactions | Self::Transactions | Self::SharedTransactions => {
                MySqlOutput::Transactions
            }
            _ => MySqlOutput::Changes,
        }
    }
    /// Whether committed progress (and replay retention) applies.
    pub fn tracked(self) -> bool {
        matches!(
            self,
            Self::Transactions
                | Self::SharedTransactions
                | Self::SharedChanges
                | Self::SharedPositionalChanges
        )
    }
    /// Whether a fixed consumer set owns the progress.
    pub fn shared(self) -> bool {
        matches!(
            self,
            Self::SharedTransactions | Self::SharedChanges | Self::SharedPositionalChanges
        )
    }
    /// Whether rows use positional (`@N`) properties.
    pub fn positional(self) -> bool {
        matches!(
            self,
            Self::PositionalChanges | Self::SharedPositionalChanges
        )
    }
    /// Whether the `persistent` field applies.
    pub fn has_persistent(self) -> bool {
        matches!(self, Self::SharedChanges | Self::SharedPositionalChanges)
    }
    /// Whether `startFrom: snapshot` (a paired coordinated bootstrap) applies.
    pub fn allows_snapshot(self) -> bool {
        matches!(
            self,
            Self::Transactions | Self::SharedTransactions | Self::SharedChanges
        )
    }
    /// Whether the mode requires a fixed initial position (`position` or
    /// `snapshot`) because consumers replay whole transactions from it.
    pub fn requires_fixed_start(self) -> bool {
        matches!(self, Self::Transactions | Self::SharedTransactions)
    }
}

/// A parsed factory configuration.
#[derive(Clone, Debug, PartialEq)]
pub struct SourceConfig {
    pub mode: Mode,
    pub stream: String,
    /// Upstream history retention; tracked modes only.
    pub retention: MySqlRetention,
    pub persistent: bool,
    pub settings: MySqlSourceSettings,
}

const STREAM: &str = "stream";
const RETENTION: &str = "retention";
const PERSISTENT: &str = "persistent";

/// A placeholder identity for static validation; the real component ID only
/// changes the derived `serverId`.
fn validation_id() -> Result<ComponentId> {
    Ok(ComponentId::try_new(
        "00000000-0000-4000-8000-000000000000",
    )?)
}

impl SourceConfig {
    /// Parses and statically validates `value` for `mode`. Unknown fields,
    /// including another mode's fields, are rejected.
    pub fn from_value(mode: Mode, value: &Value) -> Result<Self> {
        let mut object = value
            .as_object()
            .context("MySQL source configuration must be an object")?
            .clone();
        let stream = match object.remove(STREAM) {
            Some(Value::String(stream)) => stream,
            Some(_) => anyhow::bail!("stream must be a string"),
            None => anyhow::bail!("stream is required"),
        };
        let retention = match object.remove(RETENTION) {
            None => MySqlRetention::Server,
            Some(_) if !mode.tracked() => {
                anyhow::bail!("retention is not supported by this untracked factory")
            }
            Some(value) => retention_from(&value)?,
        };
        let persistent = match object.remove(PERSISTENT) {
            None => false,
            Some(Value::Bool(value)) if mode.has_persistent() => value,
            Some(Value::Bool(_)) => anyhow::bail!("persistent is not supported by this factory"),
            Some(_) => anyhow::bail!("persistent must be a boolean"),
        };
        let settings: MySqlSourceSettings =
            serde_json::from_value(Value::Object(object)).context("invalid MySQL settings")?;
        let config = Self {
            mode,
            stream,
            retention,
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
        let start = self.settings.start_from;
        if start == StartFrom::Snapshot {
            anyhow::ensure!(
                self.mode.allows_snapshot(),
                "startFrom snapshot needs a paired coordinated bootstrap provider, which this factory does not support"
            );
        }
        if self.mode.requires_fixed_start() {
            anyhow::ensure!(
                matches!(start, StartFrom::Position | StartFrom::Snapshot),
                "whole-transaction replay requires startFrom position or snapshot"
            );
        }
        if self.mode.positional() {
            anyhow::ensure!(
                !self.settings.table_keys.is_empty(),
                "positional MySQL requires explicit @N tableKeys"
            );
        }
        let engine = self.engine_config(&validation_id()?)?;
        if self.mode.tracked() && self.mode.output() == MySqlOutput::Changes {
            engine.validate_rows()
        } else {
            engine.validate()
        }
    }

    /// The engine configuration for component `id`.
    pub fn engine_config(&self, id: &ComponentId) -> Result<MySqlConfig> {
        self.settings.engine_config(
            id,
            self.mode.output(),
            self.mode.tracked().then_some(self.retention),
        )
    }

    /// The configuration as the factory accepts it, with defaults filled.
    pub fn to_value(&self) -> Result<Value> {
        let Value::Object(mut object) = serde_json::to_value(&self.settings)? else {
            anyhow::bail!("MySQL settings did not serialize as an object");
        };
        object.insert(STREAM.into(), Value::String(self.stream.clone()));
        if self.mode.tracked() {
            object.insert(RETENTION.into(), json!(retention_name(self.retention)));
        }
        if self.mode.has_persistent() {
            object.insert(PERSISTENT.into(), Value::Bool(self.persistent));
        }
        Ok(Value::Object(object))
    }
}

/// The camelCase name of a retention mode.
pub fn retention_name(retention: MySqlRetention) -> &'static str {
    match retention {
        MySqlRetention::Server => "server",
        MySqlRetention::UntilProcessed => "untilProcessed",
    }
}

/// Parses a camelCase retention name.
pub fn retention_from(value: &Value) -> Result<MySqlRetention> {
    match value.as_str() {
        Some("server") => Ok(MySqlRetention::Server),
        Some("untilProcessed") => Ok(MySqlRetention::UntilProcessed),
        _ => anyhow::bail!("retention must be \"server\" or \"untilProcessed\""),
    }
}

fn string(name: &str, required: bool, description: &str) -> ConfigOption {
    ConfigOption::new(name, ConfigType::String, required, description)
}

/// The published configuration for `mode`: `stream`, every shared settings
/// option (`drasi_mysql_common::native::definition`) and the mode fields.
pub fn definition(mode: Mode) -> ConfigDefinition {
    let mut options = vec![string(
        STREAM,
        true,
        "Output stream identifier carried by every emitted change.",
    )];
    options.extend(definition::options());
    if mode.tracked() {
        options.push(
            string(RETENTION, false, "server: resume only while the server retains the history (a gap fails). untilProcessed: requires automatic binlog expiry disabled; never purges logs itself.")
                .default("server")
                .allowed([json!("server"), json!("untilProcessed")]),
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
    ConfigDefinition::try_new(options).expect("constant MySQL source configuration")
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn base(mode: Mode) -> Value {
        let mut value = json!({"stream": "changes", "database": "app", "user": "drasi"});
        if mode.requires_fixed_start() {
            value["startFrom"] = json!("position");
            value["startBinlogFile"] = json!("binlog.000001");
            value["startBinlogPosition"] = json!(4);
        }
        if mode.positional() {
            value["tables"] = json!(["items"]);
            value["tableKeys"] = json!([{"table": "items", "keyColumns": ["@0"]}]);
        }
        value
    }

    #[test]
    fn every_mode_round_trips_with_defaults_and_rejects_foreign_fields() {
        for mode in MODES {
            let config = SourceConfig::from_value(mode, &base(mode)).unwrap();
            let value = config.to_value().unwrap();
            definition(mode).schema().validate(&value).unwrap();
            definition(mode).details().validate(&value).unwrap();
            assert_eq!(SourceConfig::from_value(mode, &value).unwrap(), config);
            for (field, foreign) in [
                ("settings", json!({})),
                ("autoCreateSlot", json!(true)),
                ("ssl_mode", json!("disabled")),
            ] {
                let mut unknown = base(mode);
                unknown[field] = foreign;
                assert!(
                    SourceConfig::from_value(mode, &unknown).is_err(),
                    "{mode:?} {field}"
                );
            }
            let mut persistent = base(mode);
            persistent["persistent"] = json!(true);
            assert_eq!(
                SourceConfig::from_value(mode, &persistent).is_ok(),
                mode.has_persistent(),
                "{mode:?} persistent"
            );
            let mut retention = base(mode);
            retention["retention"] = json!("untilProcessed");
            assert_eq!(
                SourceConfig::from_value(mode, &retention).is_ok(),
                mode.tracked(),
                "{mode:?} retention"
            );
        }
    }

    #[test]
    fn published_defaults_equal_serde_defaults() {
        let parsed: MySqlSourceSettings =
            serde_json::from_value(json!({"database": "app", "user": "drasi"})).unwrap();
        let serialized = serde_json::to_value(&parsed).unwrap();
        for (name, field) in definition(Mode::SharedChanges).details().fields {
            if let Some(default) = field.default {
                let actual = match name.as_ref() {
                    RETENTION => json!("server"),
                    PERSISTENT => json!(false),
                    other => serialized[other].clone(),
                };
                assert_eq!(actual, default, "{name}");
            }
        }
    }

    #[test]
    fn start_rules_follow_the_factory() {
        let mut snapshot = base(Mode::Changes);
        snapshot["startFrom"] = json!("snapshot");
        assert!(SourceConfig::from_value(Mode::Changes, &snapshot).is_err());
        assert!(SourceConfig::from_value(Mode::PositionalChanges, &{
            let mut value = base(Mode::PositionalChanges);
            value["startFrom"] = json!("snapshot");
            value
        })
        .is_err());
        let mut transactions = base(Mode::Transactions);
        transactions["startFrom"] = json!("end");
        transactions
            .as_object_mut()
            .unwrap()
            .remove("startBinlogFile");
        transactions
            .as_object_mut()
            .unwrap()
            .remove("startBinlogPosition");
        assert!(SourceConfig::from_value(Mode::Transactions, &transactions).is_err());
        transactions["startFrom"] = json!("snapshot");
        SourceConfig::from_value(Mode::Transactions, &transactions).unwrap();
        let mut positional = base(Mode::PositionalChanges);
        positional["tableKeys"] = json!([]);
        assert!(SourceConfig::from_value(Mode::PositionalChanges, &positional).is_err());
    }

    #[test]
    fn only_the_password_and_client_key_are_secret() {
        for mode in MODES {
            let schema = definition(mode).schema();
            let secret: Vec<_> = schema
                .fields
                .iter()
                .filter(|(_, field)| field.secret)
                .map(|(name, _)| name.to_string())
                .collect();
            assert_eq!(secret, vec!["password", "tlsClientKeyPem"]);
        }
    }
}
