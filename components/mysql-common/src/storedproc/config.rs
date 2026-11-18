// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Flat configuration of the native MySQL stored-procedure sink.
//!
//! Every check that does not need the graph's incoming edges lives here.
//! Serialization returns secrets because `configuration()` is the
//! persistence hook; `Debug` never prints them.

use crate::storedproc;
use anyhow::{ensure, Context, Result};
use drasi_computation_contracts::{
    DeliveryGuarantee, OutputAliases, PermanentFailurePolicy, SinkOutputs,
};
use drasi_computation_storedproc_common::settings;
pub use drasi_computation_storedproc_common::settings::SslMode;
use drasi_reaction_common::{OperationType, QueryConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, fmt};

/// Default MySQL port.
pub const DEFAULT_PORT: u16 = 3306;
/// Default deadline for one SQL command (each statement of an operation).
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
/// Default deadline for one connection attempt (TCP, TLS, authentication).
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = settings::DEFAULT_CONNECT_TIMEOUT_MS;
/// Default bound on Stop and Dispose cleanup when the host sets no deadline.
pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = settings::DEFAULT_SHUTDOWN_TIMEOUT_MS;
/// Default maximum MySQL protocol packet, which bounds one command.
pub const DEFAULT_MAX_PROTOCOL_BYTES: usize = 8 << 20;
/// Upper bound on `maxProtocolBytes`.
pub const MAX_PROTOCOL_BYTES: usize = 64 << 20;
/// Lower bound on `maxProtocolBytes`.
pub const MIN_PROTOCOL_BYTES: usize = 4096;
/// Upper bound on any configured duration.
pub const MAX_DURATION_MS: u64 = settings::MAX_DURATION_MS;
/// Upper bound on `maxStreams`.
pub const MAX_STREAMS: u32 = drasi_computation_storedproc_common::cursor::MAX_STREAMS_LIMIT;

fn host() -> String {
    "localhost".into()
}
fn port() -> u16 {
    DEFAULT_PORT
}
fn command_timeout() -> u64 {
    DEFAULT_COMMAND_TIMEOUT_MS
}
fn connect_timeout() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_MS
}
fn shutdown_timeout() -> u64 {
    DEFAULT_SHUTDOWN_TIMEOUT_MS
}
fn protocol_bytes() -> usize {
    DEFAULT_MAX_PROTOCOL_BYTES
}
fn progress_table() -> String {
    storedproc::delivery::DEFAULT_PROGRESS_TABLE.into()
}
fn max_streams() -> u32 {
    storedproc::delivery::DEFAULT_MAX_STREAMS
}

/// Settings of the native MySQL stored-procedure sink.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MySqlStoredProcConfig {
    #[serde(default = "host")]
    pub host: String,
    #[serde(default = "port")]
    pub port: u16,
    pub database: String,
    /// Empty when activation credentials supply the account.
    #[serde(default)]
    pub user: String,
    /// Secret. Empty when activation credentials supply the account.
    #[serde(default)]
    pub password: String,
    /// Verified TLS (the default) or plaintext, which is refused for a
    /// non-loopback host unless `allow_remote_plaintext` is set.
    #[serde(default)]
    pub ssl_mode: SslMode,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub allow_remote_plaintext: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    /// Optional aliases for the incoming outputs; the graph supplies each
    /// output's producing component, port and stream.
    #[serde(default, skip_serializing_if = "OutputAliases::is_empty")]
    pub outputs: OutputAliases,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, QueryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
    /// Destination cursor table in `database`.
    #[serde(default = "progress_table")]
    pub progress_table: String,
    /// Delivery streams this component may hold in the cursor table.
    #[serde(default = "max_streams")]
    pub max_streams: u32,
    /// `exactlyOnce` (default) accepts only producers with persistent
    /// progress; `atLeastOnce` also accepts in-memory queries.
    #[serde(default)]
    pub delivery_guarantee: DeliveryGuarantee,
    /// `stall` (default) or `skip`: a skipped operation is recorded in the
    /// dead-letter table atomically with the cursor advance.
    #[serde(default)]
    pub on_permanent_failure: PermanentFailurePolicy,
    #[serde(default = "command_timeout")]
    pub command_timeout_ms: u64,
    #[serde(default = "connect_timeout")]
    pub connect_timeout_ms: u64,
    #[serde(default = "shutdown_timeout")]
    pub shutdown_timeout_ms: u64,
    #[serde(default = "protocol_bytes")]
    pub max_protocol_bytes: usize,
}

impl fmt::Debug for MySqlStoredProcConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("MySqlStoredProcConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"[REDACTED]")
            .field("ssl_mode", &self.ssl_mode)
            .field("outputs", &self.outputs)
            .field("routes", &self.routes)
            .field("default_template", &self.default_template)
            .field("progress_table", &self.progress_table)
            .field("max_streams", &self.max_streams)
            .field("delivery_guarantee", &self.delivery_guarantee)
            .field("on_permanent_failure", &self.on_permanent_failure)
            .finish_non_exhaustive()
    }
}

/// Template keys accepted inside `routes.<alias>` and `defaultTemplate`.
const OPERATIONS: [&str; 3] = ["added", "updated", "deleted"];

/// `QueryConfig` flattens an extension and so accepts unknown keys; reject
/// misspellings (`adedd`, `templat`) before deserializing.
fn validate_template_shape(value: &Value) -> Result<()> {
    let check = |config: &Value, location: &str| -> Result<()> {
        let object = config
            .as_object()
            .with_context(|| format!("{location} must be an object"))?;
        for (operation, spec) in object {
            ensure!(
                OPERATIONS.contains(&operation.as_str()),
                "{location}: unknown operation '{operation}' (expected added, updated or deleted)"
            );
            let spec = spec
                .as_object()
                .with_context(|| format!("{location}.{operation} must be an object"))?;
            for key in spec.keys() {
                ensure!(
                    key == "template",
                    "{location}.{operation}: unknown field '{key}' (expected template)"
                );
            }
        }
        Ok(())
    };
    if let Some(default) = value.get("defaultTemplate").filter(|v| !v.is_null()) {
        check(default, "defaultTemplate")?;
    }
    if let Some(routes) = value.get("routes") {
        for (alias, config) in routes.as_object().context("routes must be an object")? {
            check(config, &format!("routes.{alias}"))?;
        }
    }
    Ok(())
}

impl MySqlStoredProcConfig {
    /// Deserializes and validates everything that does not depend on the
    /// graph's incoming edges.
    pub fn parse(value: Value) -> Result<Self> {
        Self::parse_with_activation_credentials(value, false)
    }

    /// As [`Self::parse`]; `credentials_bound` permits an empty account
    /// because activation credentials supply it at Start.
    pub fn parse_with_activation_credentials(
        value: Value,
        credentials_bound: bool,
    ) -> Result<Self> {
        validate_template_shape(&value)?;
        let config: Self = serde_json::from_value(value)?;
        config.validate_with_activation_credentials(credentials_bound)?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        self.validate_with_activation_credentials(false)
    }

    pub fn validate_with_activation_credentials(&self, credentials_bound: bool) -> Result<()> {
        ensure!(!self.host.trim().is_empty(), "host is required");
        ensure!(self.port != 0, "port must be in 1..=65535");
        ensure!(
            crate::is_valid_identifier(&self.database) && self.database.len() <= 64,
            "database must be a MySQL identifier of letters, digits and underscores (at most 64)"
        );
        ensure!(
            credentials_bound || !self.user.is_empty(),
            "user is required unless activation credentials are bound"
        );
        ensure!(
            [&self.host, &self.database, &self.user, &self.password]
                .iter()
                .all(|value| !value.contains('\0') && value.len() <= 4096),
            "connection settings must not contain NUL and must be at most 4096 bytes"
        );
        ensure!(
            (MIN_PROTOCOL_BYTES..=MAX_PROTOCOL_BYTES).contains(&self.max_protocol_bytes),
            "maxProtocolBytes must be in {MIN_PROTOCOL_BYTES}..={MAX_PROTOCOL_BYTES}"
        );
        ensure!(
            self.ssl_mode == SslMode::Require || self.tls_ca_pem.is_none(),
            "tlsCaPem requires sslMode require"
        );
        settings::validate_transport(self.ssl_mode, self.allow_remote_plaintext, &self.host)?;
        if let Some(pem) = &self.tls_ca_pem {
            ensure!(
                !pem.is_empty() && pem.len() <= 64 * 1024,
                "tlsCaPem must be a nonempty PEM of at most 64 KiB"
            );
        }
        self.progress()?;
        settings::validate_max_streams(self.max_streams)?;
        for (name, value) in [
            ("commandTimeoutMs", self.command_timeout_ms),
            ("connectTimeoutMs", self.connect_timeout_ms),
            ("shutdownTimeoutMs", self.shutdown_timeout_ms),
        ] {
            settings::validate_duration(name, value)?;
        }
        self.outputs.validate()?;
        ensure!(
            !self.routes.is_empty() || self.default_template.is_some(),
            "configure routes or defaultTemplate with at least one command template"
        );
        for template in self.templates() {
            drasi_reaction_common::sql_template::validate_parameter_only(template)
                .context("MySQL command template")?;
        }
        Ok(())
    }

    /// Every configured, nonempty command template.
    pub fn templates(&self) -> impl Iterator<Item = &str> {
        self.routes
            .values()
            .chain(self.default_template.iter())
            .flat_map(|query| [&query.added, &query.updated, &query.deleted])
            .flatten()
            .map(|spec| spec.template.as_str())
            .filter(|template| !template.trim().is_empty())
    }

    /// Checks that depend on the incoming edges: every route must name an
    /// output of this sink.
    pub fn validate_outputs(&self, outputs: &SinkOutputs) -> Result<()> {
        storedproc::validate_routes(
            &self.routes,
            &outputs.aliases().map(str::to_owned).collect::<Vec<_>>(),
        )
    }

    /// The template for an output alias and operation: the alias route, then
    /// its last dotted segment, then the default.
    pub fn template(&self, query: &str, operation: OperationType) -> Option<&str> {
        storedproc::resolve_template(
            &self.routes,
            self.default_template.as_ref(),
            query,
            operation,
        )
        .map(|template| template.template.as_str())
    }

    /// The validated cursor table (`table` lives in `database`).
    pub fn progress(&self) -> Result<storedproc::delivery::ProgressTable> {
        storedproc::delivery::ProgressTable::parse(&self.progress_table)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base() -> Value {
        json!({
            "database": "test", "user": "u", "password": "p",
            "defaultTemplate": {"added": {"template": "CALL a({{param after.id}})"}}
        })
    }

    #[test]
    fn defaults_are_exact() -> Result<()> {
        let config = MySqlStoredProcConfig::parse(base())?;
        assert_eq!(
            serde_json::to_value(&config)?,
            json!({
                "host": "localhost", "port": 3306, "database": "test", "user": "u",
                "password": "p", "sslMode": "require",
                "defaultTemplate": {"added": {"template": "CALL a({{param after.id}})"}},
                "progressTable": "drasi_delivery_progress", "maxStreams": 4096,
                "deliveryGuarantee": "exactlyOnce", "onPermanentFailure": "stall",
                "commandTimeoutMs": 30000, "connectTimeoutMs": 10000, "shutdownTimeoutMs": 10000,
                "maxProtocolBytes": 8388608
            })
        );
        assert!(!format!("{config:?}").contains("\"p\""));
        Ok(())
    }

    #[test]
    fn delivery_guarantee_is_opt_in_and_camel_case() -> Result<()> {
        assert_eq!(
            MySqlStoredProcConfig::parse(base())?.delivery_guarantee,
            DeliveryGuarantee::ExactlyOnce
        );
        for (value, expected) in [
            ("exactlyOnce", DeliveryGuarantee::ExactlyOnce),
            ("atLeastOnce", DeliveryGuarantee::AtLeastOnce),
        ] {
            let mut config = base();
            config["deliveryGuarantee"] = json!(value);
            let parsed = MySqlStoredProcConfig::parse(config)?;
            assert_eq!(parsed.delivery_guarantee, expected);
            assert_eq!(serde_json::to_value(&parsed)?["deliveryGuarantee"], value);
        }
        for invalid in [json!("at_least_once"), json!("AtLeastOnce"), json!(true)] {
            let mut config = base();
            config["deliveryGuarantee"] = invalid;
            assert!(MySqlStoredProcConfig::parse(config).is_err());
        }
        Ok(())
    }

    #[test]
    fn rejects_invalid_fields_and_legacy_keys() {
        for field in [
            json!({"port": 0}),
            json!({"database": "bad-name"}),
            json!({"user": ""}),
            json!({"sslMode": "disable", "tlsCaPem": "pem-without-ssl"}),
            json!({"tlsCaPem": ""}),
            json!({"host": "db.example", "sslMode": "disable"}),
            json!({"hostname": "localhost"}),
            json!({"ssl": false}),
            json!({"progressTable": "bad table"}),
            json!({"progressTable": ""}),
            json!({"progressTable": "x".repeat(52)}),
            json!({"maxStreams": 0}),
            json!({"connectRetryAttempts": 3}),
            json!({"commandTimeoutMs": 0}),
            json!({"connectTimeoutMs": 0}),
            json!({"shutdownTimeoutMs": 0}),
            json!({"reconnectBudgetMs": 1000}),
            json!({"onPermanentFailure": "deadLetter"}),
            json!({"maxProtocolBytes": 100}),
            json!({"maxProtocolBytes": (64 << 20) + 1}),
            json!({"retry_attempts": 3}),
            json!({"command_timeout_ms": 3}),
            json!({"default_template": {}}),
            json!({"max_protocol_bytes": 1024}),
            json!({"defaultTemplate": null, "routes": {}}),
            json!({"defaultTemplate": {"adedd": {"template": "CALL x()"}}}),
            json!({"defaultTemplate": {"added": {"templat": "CALL x()"}}}),
            json!({"defaultTemplate": {"added": {"template": "CALL x('{{after.name}}')"}}}),
            json!({"defaultTemplate": {"added": {"template": "CALL x({{json after}})"}}}),
            json!({"defaultTemplate": {"added": {"template": "CALL x({{{after.name}}})"}}}),
            json!({"defaultTemplate": {"added": {"template": "CALL {{param after.proc}}x({{> partial}})"}}}),
            json!({"defaultTemplate": {"added": {"template": "CALL x({{param after.id)"}}}),
        ] {
            let mut value = base();
            for (key, field) in field.as_object().unwrap() {
                value[key] = field.clone();
            }
            assert!(
                MySqlStoredProcConfig::parse(value.clone()).is_err(),
                "{value}"
            );
        }
    }

    #[test]
    fn activation_credentials_permit_an_empty_account() -> Result<()> {
        let mut value = base();
        value.as_object_mut().unwrap().remove("user");
        value.as_object_mut().unwrap().remove("password");
        assert!(MySqlStoredProcConfig::parse(value.clone()).is_err());
        MySqlStoredProcConfig::parse_with_activation_credentials(value, true)?;
        Ok(())
    }

    #[test]
    fn control_flow_with_parameters_is_accepted() -> Result<()> {
        let mut value = base();
        value["defaultTemplate"]["updated"] = json!({
            "template": "{{#if after.active}}CALL on({{param after.id}}){{else}}CALL off({{param after.id}}){{/if}}"
        });
        MySqlStoredProcConfig::parse(value)?;
        Ok(())
    }
}
