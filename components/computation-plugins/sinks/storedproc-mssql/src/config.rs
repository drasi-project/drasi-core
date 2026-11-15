// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Configuration of `drasi.mssql/storedproc`: fields, published defaults and
//! every check that does not need the graph's incoming edges.
//!
//! The sink owns this struct; it does not reuse the source's
//! `MsSqlSourceConfig`. Unknown fields are rejected. `password` is the only
//! secret and never prints through `Debug`; serialization returns it because
//! `configuration()` is the persistence hook.

use anyhow::{bail, ensure, Context, Result};
use drasi_computation_contracts::{OutputAliases, SinkOutputs};
use drasi_mssql_common::sql::CommandTemplate;
use drasi_reaction_common::{sql_template::validate_parameter_only, OperationType, QueryConfig};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, time::Duration};

/// Default SQL Server TCP port.
pub const DEFAULT_PORT: u16 = 1433;
/// Default destination cursor table.
pub const DEFAULT_PROGRESS_TABLE: &str = "dbo.drasi_delivery_progress";
/// Default number of delivery streams one sink may keep in the cursor table.
pub const DEFAULT_MAX_STREAMS: u32 = 4096;
/// Upper bound on `maxStreams`.
pub const MAX_MAX_STREAMS: u32 = 1_000_000;
/// Default per-round-trip command timeout.
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
/// Upper bound on `commandTimeoutMs`.
pub const MAX_COMMAND_TIMEOUT_MS: u64 = 3_600_000;
/// Default TCP, TLS and login deadline.
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 15_000;
/// Upper bound on `connectTimeoutMs`.
pub const MAX_CONNECT_TIMEOUT_MS: u64 = 300_000;
/// Default total time for reconnecting and retrying one operation.
pub const DEFAULT_RETRY_BUDGET_MS: u64 = 60_000;
/// Upper bound on `retryBudgetMs`.
pub const MAX_RETRY_BUDGET_MS: u64 = 3_600_000;
/// Upper bound on `trustedCaPem`.
pub const MAX_PEM_BYTES: usize = 256 * 1024;
/// SQL Server identifier length limit.
const MAX_IDENTIFIER: usize = 128;

fn port() -> u16 {
    DEFAULT_PORT
}
fn progress_table() -> String {
    DEFAULT_PROGRESS_TABLE.to_owned()
}
fn max_streams() -> u32 {
    DEFAULT_MAX_STREAMS
}
fn command_timeout_ms() -> u64 {
    DEFAULT_COMMAND_TIMEOUT_MS
}
fn connect_timeout_ms() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_MS
}
fn retry_budget_ms() -> u64 {
    DEFAULT_RETRY_BUDGET_MS
}

/// TDS encryption. `required` (the default) encrypts the whole session and
/// verifies the server certificate. The other modes are explicit opt-ins to
/// plaintext and log a warning at start.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Encryption {
    /// TLS for the whole session.
    #[default]
    Required,
    /// TLS for the login packet only; query data is plaintext.
    LoginOnly,
    /// No TLS at all; credentials and data are plaintext.
    Disabled,
}

impl Encryption {
    pub(crate) fn level(self) -> tiberius::EncryptionLevel {
        match self {
            Self::Required => tiberius::EncryptionLevel::Required,
            Self::LoginOnly => tiberius::EncryptionLevel::Off,
            Self::Disabled => tiberius::EncryptionLevel::NotSupported,
        }
    }
}

/// Settings of the native SQL Server stored-procedure sink.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MsSqlStoredProcConfig {
    pub host: String,
    #[serde(default = "port")]
    pub port: u16,
    pub database: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub user: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(default)]
    pub encryption: Encryption,
    #[serde(default)]
    pub trust_server_certificate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trusted_ca_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_name_in_certificate: Option<String>,
    #[serde(default = "progress_table")]
    pub progress_table: String,
    #[serde(default = "max_streams")]
    pub max_streams: u32,
    #[serde(default, skip_serializing_if = "OutputAliases::is_empty")]
    pub outputs: OutputAliases,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, QueryConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
    #[serde(default = "command_timeout_ms")]
    pub command_timeout_ms: u64,
    #[serde(default = "connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "retry_budget_ms")]
    pub retry_budget_ms: u64,
}

impl std::fmt::Debug for MsSqlStoredProcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MsSqlStoredProcConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .field("encryption", &self.encryption)
            .field("trust_server_certificate", &self.trust_server_certificate)
            .field("trusted_ca_pem", &self.trusted_ca_pem.is_some())
            .field("host_name_in_certificate", &self.host_name_in_certificate)
            .field("progress_table", &self.progress_table)
            .field("max_streams", &self.max_streams)
            .field("outputs", &self.outputs)
            .field("routes", &self.routes)
            .field("default_template", &self.default_template)
            .field("command_timeout_ms", &self.command_timeout_ms)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .field("retry_budget_ms", &self.retry_budget_ms)
            .finish()
    }
}

impl MsSqlStoredProcConfig {
    /// Field descriptions, defaults and ranges published to hosts.
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        use serde_json::json;
        // Constant declarations; `metadata_is_valid` proves they are accepted.
        ConfigDefinition::try_new([
            Field::new("host", String, true, "SQL Server host name or IP address. Also the endpoint passed to an activation identity provider.").default_description("Required; no default."),
            Field::new("port", Integer, false, "SQL Server TCP port.").default(DEFAULT_PORT).minimum(1).maximum(u16::MAX as u64),
            Field::new("database", String, true, "Database that holds the procedures and the progress table.").default_description("Required; no default."),
            Field::new("user", String, false, "SQL authentication login. Required unless an activation identity is bound, which supplies the login (or an Entra ID access token) at each start.").default_description("None; required without an identity binding."),
            Field::new("password", String, false, "SQL authentication password. Required unless an activation identity is bound.").secret(),
            Field::new("encryption", String, false, "required encrypts the session and verifies the server certificate. loginOnly encrypts only the login packet and disabled sends everything in plaintext; both log a warning.").default(json!(Encryption::Required)).allowed([json!(Encryption::Required), json!(Encryption::LoginOnly), json!(Encryption::Disabled)]),
            Field::new("trustServerCertificate", Boolean, false, "Skip server certificate validation (testing only; logs a warning). Mutually exclusive with trustedCaPem.").default(false),
            Field::new("trustedCaPem", String, false, "PEM CA certificates trusted in addition to the system roots when verifying the server certificate."),
            Field::new("hostNameInCertificate", String, false, "Host name expected in the server certificate when it differs from host.").default_description("The value of host."),
            Field::new("progressTable", String, false, "Destination cursor table, as table or schema.table (letters, digits and underscores). Created at start when missing; see CONFIGURATION.md for grants.").default(DEFAULT_PROGRESS_TABLE),
            Field::new("maxStreams", Integer, false, "Delivery streams this sink may keep in the progress table (one per producer output).").default(DEFAULT_MAX_STREAMS).minimum(1).maximum(MAX_MAX_STREAMS as u64),
            Field::new("outputs", Object, false, "Optional aliases for incoming outputs: a map of alias to {component, port?}. Every incoming edge is accepted; an unaliased output is named after its producing component. Routes match the alias.").constraint(drasi_computation_contracts::ConfigurationConstraint::output_aliases()),
            Field::new("routes", Object, false, "Per-output-alias command templates: {alias: {added?, updated?, deleted?: {template}}}. A route may also name the last dotted segment of an alias.").default(json!({})),
            Field::new("defaultTemplate", Object, false, "Command templates used when no route matches: {added?, updated?, deleted?: {template}}. At least one template must be configured across routes and defaultTemplate."),
            Field::new("commandTimeoutMs", Integer, false, "Deadline for each server round trip of a delivery statement (never for a whole input). An expired command drops the connection.").default(DEFAULT_COMMAND_TIMEOUT_MS).minimum(1).maximum(MAX_COMMAND_TIMEOUT_MS),
            Field::new("connectTimeoutMs", Integer, false, "Deadline for TCP, TLS and login of one connection attempt.").default(DEFAULT_CONNECT_TIMEOUT_MS).minimum(1).maximum(MAX_CONNECT_TIMEOUT_MS),
            Field::new("retryBudgetMs", Integer, false, "Total time to reconnect and retry one operation (or the start preparation) after a retryable failure, with jittered exponential backoff. 0 disables in-call retries.").default(DEFAULT_RETRY_BUDGET_MS).minimum(0).maximum(MAX_RETRY_BUDGET_MS),
        ])
        .expect("constant SQL Server stored-procedure configuration")
    }

    /// Deserializes and validates everything that does not depend on the
    /// graph's incoming edges, including template compilation.
    pub fn parse(value: Value) -> Result<Self> {
        check_template_shape(&value)?;
        let config: Self = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    /// Field checks that need no identity binding information.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.host.is_empty() && self.host.len() <= 253 && !self.host.contains('\0'),
            "host must be a nonempty host name of at most 253 bytes"
        );
        ensure!(self.port != 0, "port must be in 1..=65535");
        ensure!(
            !self.database.is_empty()
                && self.database.chars().count() <= MAX_IDENTIFIER
                && !self.database.contains('\0'),
            "database must be a nonempty name of at most 128 characters"
        );
        ensure!(
            self.user.len() <= 1024 && !self.user.contains('\0'),
            "user must be at most 1024 bytes"
        );
        if let Some(password) = &self.password {
            ensure!(
                password.len() <= 1024 && !password.contains('\0'),
                "password must be at most 1024 bytes"
            );
        }
        if let Some(pem) = &self.trusted_ca_pem {
            ensure!(
                pem.len() <= MAX_PEM_BYTES && pem.contains("-----BEGIN CERTIFICATE-----"),
                "trustedCaPem must hold PEM certificates of at most {MAX_PEM_BYTES} bytes"
            );
            ensure!(
                !self.trust_server_certificate,
                "trustServerCertificate and trustedCaPem are mutually exclusive"
            );
        }
        if self.encryption == Encryption::Disabled {
            ensure!(
                !self.trust_server_certificate
                    && self.trusted_ca_pem.is_none()
                    && self.host_name_in_certificate.is_none(),
                "certificate settings require encryption"
            );
        }
        if let Some(name) = &self.host_name_in_certificate {
            ensure!(
                !name.is_empty() && name.len() <= 253 && !name.contains('\0'),
                "hostNameInCertificate must be a nonempty host name"
            );
        }
        ProgressTable::parse(&self.progress_table)?;
        ensure!(
            (1..=MAX_MAX_STREAMS).contains(&self.max_streams),
            "maxStreams must be in 1..={MAX_MAX_STREAMS}"
        );
        ensure!(
            (1..=MAX_COMMAND_TIMEOUT_MS).contains(&self.command_timeout_ms),
            "commandTimeoutMs must be in 1..={MAX_COMMAND_TIMEOUT_MS}"
        );
        ensure!(
            (1..=MAX_CONNECT_TIMEOUT_MS).contains(&self.connect_timeout_ms),
            "connectTimeoutMs must be in 1..={MAX_CONNECT_TIMEOUT_MS}"
        );
        ensure!(
            self.retry_budget_ms <= MAX_RETRY_BUDGET_MS,
            "retryBudgetMs must be in 0..={MAX_RETRY_BUDGET_MS}"
        );
        self.outputs.validate()?;
        let mut configured = 0;
        for (template, location) in self.templates() {
            if template.trim().is_empty() {
                continue;
            }
            configured += 1;
            validate_parameter_only(template).with_context(|| format!("{location} template"))?;
            CommandTemplate::compile(template).with_context(|| format!("{location} template"))?;
        }
        ensure!(
            configured > 0,
            "configure at least one command template in routes or defaultTemplate"
        );
        Ok(())
    }

    /// Credential checks for construction: literal SQL credentials are
    /// required unless the host binds an activation identity.
    pub fn validate_credentials(&self, identity_bound: bool) -> Result<()> {
        if !identity_bound {
            ensure!(
                !self.user.is_empty() && self.password.as_deref().is_some_and(|p| !p.is_empty()),
                "user and password are required unless an activation identity is bound"
            );
        }
        Ok(())
    }

    /// Checks that depend on the incoming edges: at most 256 outputs, and
    /// every route must name one (fully or by its last dotted segment).
    pub fn validate_outputs(&self, outputs: &SinkOutputs) -> Result<()> {
        ensure!(
            outputs.aliases().count() <= 256,
            "the SQL Server sink accepts at most 256 outputs"
        );
        for route in self.routes.keys() {
            ensure!(
                outputs.contains(route)
                    || outputs
                        .aliases()
                        .any(|alias| alias.rsplit('.').next() == Some(route.as_str())),
                "route {route} does not match an output alias"
            );
        }
        Ok(())
    }

    /// The validated cursor table.
    pub fn progress_table(&self) -> Result<ProgressTable> {
        ProgressTable::parse(&self.progress_table)
    }

    pub fn command_timeout(&self) -> Duration {
        Duration::from_millis(self.command_timeout_ms)
    }

    pub fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms)
    }

    pub fn retry_budget(&self) -> Duration {
        Duration::from_millis(self.retry_budget_ms)
    }

    /// Every configured template with a description of where it is.
    pub(crate) fn templates(&self) -> Vec<(&str, String)> {
        fn add<'a>(
            templates: &mut Vec<(&'a str, String)>,
            config: &'a QueryConfig,
            location: &str,
        ) {
            for (operation, spec) in [
                ("added", &config.added),
                ("updated", &config.updated),
                ("deleted", &config.deleted),
            ] {
                if let Some(spec) = spec {
                    templates.push((spec.template.as_str(), format!("{location}.{operation}")));
                }
            }
        }
        let mut templates = Vec::new();
        let mut routes: Vec<_> = self.routes.iter().collect();
        routes.sort_by(|left, right| left.0.cmp(right.0));
        for (alias, config) in routes {
            add(&mut templates, config, &format!("routes.{alias}"));
        }
        if let Some(config) = &self.default_template {
            add(&mut templates, config, "defaultTemplate");
        }
        templates
    }

    /// The template for an output alias and operation: full alias, then the
    /// alias's last dotted segment, then `defaultTemplate`.
    pub(crate) fn template_for(&self, alias: &str, operation: OperationType) -> Option<&str> {
        drasi_mssql_common::sql::resolve_template(
            &self.routes,
            self.default_template.as_ref(),
            alias,
            operation,
        )
        .map(|spec| spec.template.as_str())
    }

    /// Whether the session will be plaintext.
    pub(crate) fn plaintext(&self) -> bool {
        self.encryption != Encryption::Required
    }
}

/// `routes` and `defaultTemplate` use the shared `QueryConfig`, which accepts
/// unknown keys. Reject misspelled operations or template fields instead.
fn check_template_shape(value: &Value) -> Result<()> {
    fn operations(config: &Value, location: &str) -> Result<()> {
        let Some(config) = config.as_object() else {
            bail!("{location} must be an object");
        };
        for (operation, spec) in config {
            ensure!(
                matches!(operation.as_str(), "added" | "updated" | "deleted"),
                "{location}: unknown operation {operation}; use added, updated or deleted"
            );
            let Some(spec) = spec.as_object() else {
                bail!("{location}.{operation} must be an object with a template");
            };
            for key in spec.keys() {
                ensure!(
                    key == "template",
                    "{location}.{operation}: unknown field {key}; only template is supported"
                );
            }
        }
        Ok(())
    }
    if let Some(routes) = value.get("routes") {
        let Some(routes) = routes.as_object() else {
            bail!("routes must be an object");
        };
        for (alias, config) in routes {
            operations(config, &format!("routes.{alias}"))?;
        }
    }
    if let Some(config) = value
        .get("defaultTemplate")
        .filter(|value| !value.is_null())
    {
        operations(config, "defaultTemplate")?;
    }
    Ok(())
}

/// A validated `table` or `schema.table` name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgressTable {
    schema: Option<String>,
    table: String,
}

impl ProgressTable {
    pub fn parse(name: &str) -> Result<Self> {
        let parts: Vec<&str> = name.split('.').collect();
        let (schema, table) = match parts.as_slice() {
            [table] => (None, *table),
            [schema, table] => (Some(*schema), *table),
            _ => bail!("progressTable must be table or schema.table"),
        };
        for part in schema.iter().chain(std::iter::once(&table)) {
            let mut characters = part.chars();
            ensure!(
                part.len() <= MAX_IDENTIFIER
                    && characters
                        .next()
                        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
                    && characters.all(|c| c.is_ascii_alphanumeric() || c == '_'),
                "progressTable parts must start with a letter or underscore and contain only \
                 letters, digits and underscores (at most 128 each)"
            );
        }
        Ok(Self {
            schema: schema.map(str::to_owned),
            table: table.to_owned(),
        })
    }

    /// The bracket-quoted name used in SQL text.
    pub fn quoted(&self) -> String {
        let quote = |part: &str| format!("[{}]", part.replace(']', "]]"));
        match &self.schema {
            Some(schema) => format!("{}.{}", quote(schema), quote(&self.table)),
            None => quote(&self.table),
        }
    }
}
