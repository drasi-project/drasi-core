// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The flat sink configuration: one camelCase object. `password` and
//! `tlsClientPrivateKeyPem` are top-level secret fields; templates, routes and
//! every other field are inspectable.

use anyhow::{ensure, Context, Result};
use drasi_computation_contracts::{OutputAliases, SinkOutputs};
use drasi_postgres_common::{
    delivery::progress::{
        ProgressTable, DEFAULT_MAX_STREAMS, DEFAULT_PROGRESS_TABLE, MAX_STREAMS_LIMIT,
    },
    sql::CompiledTemplate,
};
use drasi_reaction_common::OperationType;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap},
    time::Duration,
};

pub const DEFAULT_HOST: &str = "localhost";
pub const DEFAULT_PORT: u16 = 5432;
pub const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
pub const DEFAULT_CONNECT_TIMEOUT_MS: u64 = 10_000;
pub const DEFAULT_RECONNECT_BUDGET_MS: u64 = 60_000;
pub const DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 10_000;
/// Upper bound on the connect, reconnect and shutdown durations.
pub const MAX_DURATION_MS: u64 = 3_600_000;
/// `statement_timeout` is a 32-bit millisecond setting.
pub const MAX_COMMAND_TIMEOUT_MS: u64 = i32::MAX as u64;
/// Largest accepted PEM value.
pub const MAX_PEM_BYTES: usize = 64 * 1024;
/// Largest accepted credential field.
pub const MAX_CREDENTIAL_BYTES: usize = 4096;

/// Transport security for the PostgreSQL connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SslMode {
    /// Verified TLS: the server certificate chain must be trusted (system
    /// roots plus `tlsCaPem`) and must match `host` (libpq `verify-full`).
    #[default]
    Require,
    /// Plaintext. Refused for a non-loopback host unless
    /// `allowRemotePlaintext` is set.
    Disable,
}

/// One operation's command template.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandSpec {
    /// Parameter-only Handlebars SQL. Empty means "no command" for this
    /// operation and stops the fallback to a less specific route.
    pub template: String,
}

/// Commands for the three routed operations.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CommandRoute {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<CommandSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated: Option<CommandSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<CommandSpec>,
}

impl CommandRoute {
    fn select(&self, operation: OperationType) -> Option<&CommandSpec> {
        match operation {
            OperationType::Add => self.added.as_ref(),
            OperationType::Update => self.updated.as_ref(),
            OperationType::Delete => self.deleted.as_ref(),
        }
    }

    fn specs(&self) -> impl Iterator<Item = &CommandSpec> {
        [&self.added, &self.updated, &self.deleted]
            .into_iter()
            .flatten()
    }
}

fn default_host() -> String {
    DEFAULT_HOST.into()
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_progress_table() -> String {
    DEFAULT_PROGRESS_TABLE.into()
}
fn default_max_streams() -> u32 {
    DEFAULT_MAX_STREAMS
}
fn default_command_timeout_ms() -> u64 {
    DEFAULT_COMMAND_TIMEOUT_MS
}
fn default_connect_timeout_ms() -> u64 {
    DEFAULT_CONNECT_TIMEOUT_MS
}
fn default_reconnect_budget_ms() -> u64 {
    DEFAULT_RECONNECT_BUDGET_MS
}
fn default_shutdown_timeout_ms() -> u64 {
    DEFAULT_SHUTDOWN_TIMEOUT_MS
}
fn is_false(value: &bool) -> bool {
    !*value
}

/// The native PostgreSQL stored-procedure sink configuration.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PostgresStoredProcConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: String,
    pub user: String,
    /// Secret. Optional when the host supplies activation credentials.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub password: String,
    #[serde(default)]
    pub ssl_mode: SslMode,
    #[serde(default, skip_serializing_if = "is_false")]
    pub allow_remote_plaintext: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_certificate_pem: Option<String>,
    /// Secret PKCS#8 PEM key for `tlsClientCertificatePem`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_client_private_key_pem: Option<String>,
    /// Optional aliases for the incoming outputs.
    #[serde(default)]
    pub outputs: OutputAliases,
    #[serde(default)]
    pub routes: BTreeMap<String, CommandRoute>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<CommandRoute>,
    #[serde(default = "default_progress_table")]
    pub progress_table: String,
    #[serde(default = "default_max_streams")]
    pub max_streams: u32,
    #[serde(default = "default_command_timeout_ms")]
    pub command_timeout_ms: u64,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_reconnect_budget_ms")]
    pub reconnect_budget_ms: u64,
    #[serde(default = "default_shutdown_timeout_ms")]
    pub shutdown_timeout_ms: u64,
}

impl std::fmt::Debug for PostgresStoredProcConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresStoredProcConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"[REDACTED]")
            .field("ssl_mode", &self.ssl_mode)
            .field("progress_table", &self.progress_table)
            .finish_non_exhaustive()
    }
}

fn command_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "added": {"type": "object", "additionalProperties": false, "required": ["template"], "properties": {"template": {"type": "string"}}},
            "updated": {"type": "object", "additionalProperties": false, "required": ["template"], "properties": {"template": {"type": "string"}}},
            "deleted": {"type": "object", "additionalProperties": false, "required": ["template"], "properties": {"template": {"type": "string"}}}
        }
    })
}

impl PostgresStoredProcConfig {
    /// Field descriptions, defaults and ranges published to hosts.
    pub fn definition() -> drasi_computation_plugin_sdk::ConfigDefinition {
        use drasi_computation_contracts::ConfigurationConstraint as Constraint;
        use drasi_computation_plugin_sdk::{
            ConfigDefinition, ConfigOption as Field, ConfigType::*,
        };
        // Constant declarations; the factory tests prove they are accepted.
        ConfigDefinition::try_new([
            Field::new("host", String, false, "PostgreSQL host name or address. TLS verifies the certificate against this name.").default(DEFAULT_HOST),
            Field::new("port", Integer, false, "PostgreSQL port.").default(DEFAULT_PORT).minimum(1).maximum(u16::MAX as u64),
            Field::new("database", String, true, "Database that holds the procedures and the progress table.").default_description("Required; no default."),
            Field::new("user", String, true, "Login role. Replaced for one activation by host activation credentials.").default_description("Required; no default."),
            Field::new("password", String, false, "Password of user. Replaced for one activation by host activation credentials.").secret().default_description("Empty."),
            Field::new("sslMode", String, false, "require: verified TLS (trusted chain and matching host name, libpq verify-full). disable: plaintext, refused for a non-loopback host unless allowRemotePlaintext is true.").default(json!(SslMode::Require)).allowed([json!(SslMode::Require), json!(SslMode::Disable)]),
            Field::new("allowRemotePlaintext", Boolean, false, "Permit sslMode disable for a host that is not localhost or a loopback address. Query data and authentication travel unencrypted.").default(false),
            Field::new("tlsCaPem", String, false, "PEM CA certificates trusted in addition to the system roots; requires sslMode require."),
            Field::new("tlsClientCertificatePem", String, false, "PEM client certificate for mutual TLS; requires tlsClientPrivateKeyPem."),
            Field::new("tlsClientPrivateKeyPem", String, false, "PKCS#8 PEM private key for tlsClientCertificatePem.").secret(),
            Field::new("outputs", Object, false, "Optional aliases for incoming outputs: a map of alias to {component, port?}. An unaliased output is named after its producing component. The alias selects the route and appears in the template context.").default(json!({})).constraint(Constraint::output_aliases()),
            Field::new("routes", Object, false, "Per-output-alias commands: {alias: {added?, updated?, deleted?: {template}}}. A key matches the full alias or its last dotted segment.").default(json!({})).constraint(Constraint::try_new(json!({"type": "object", "additionalProperties": command_schema()})).expect("constant route schema")),
            Field::new("defaultTemplate", Object, false, "Commands used when no route supplies the operation: {added?, updated?, deleted?: {template}}.").constraint(Constraint::try_new(command_schema()).expect("constant command schema")),
            Field::new("progressTable", String, false, "Schema-qualified destination cursor table (lowercase identifiers). Shared by several consumers; each owns only its rows.").default(DEFAULT_PROGRESS_TABLE),
            Field::new("maxStreams", Integer, false, "Destination cursor rows (producer streams) this consumer may own.").default(DEFAULT_MAX_STREAMS).minimum(1).maximum(MAX_STREAMS_LIMIT as u64),
            Field::new("commandTimeoutMs", Integer, false, "Server statement_timeout for each command; the client waits this long plus a 5 second grace before discarding the connection.").default(DEFAULT_COMMAND_TIMEOUT_MS).minimum(1).maximum(MAX_COMMAND_TIMEOUT_MS),
            Field::new("connectTimeoutMs", Integer, false, "Deadline for one connection attempt (TCP, TLS and authentication).").default(DEFAULT_CONNECT_TIMEOUT_MS).minimum(1).maximum(MAX_DURATION_MS),
            Field::new("reconnectBudgetMs", Integer, false, "Total time one operation may spend reconnecting and retrying transient failures with jittered exponential backoff; 0 makes a single attempt.").default(DEFAULT_RECONNECT_BUDGET_MS).minimum(0).maximum(MAX_DURATION_MS),
            Field::new("shutdownTimeoutMs", Integer, false, "Bound on closing the connection at Stop when the host gives no earlier deadline.").default(DEFAULT_SHUTDOWN_TIMEOUT_MS).minimum(1).maximum(MAX_DURATION_MS),
        ])
        .expect("constant PostgreSQL stored-procedure configuration")
    }

    /// Deserializes and validates everything that does not depend on the
    /// graph's incoming edges, including template compilation.
    pub fn parse(value: Value) -> Result<Self> {
        let config: Self = serde_json::from_value(value)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.port > 0
                && !self.host.is_empty()
                && !self.database.is_empty()
                && !self.user.is_empty(),
            "PostgreSQL host, port, database and user are required"
        );
        for (name, value) in [
            ("host", &self.host),
            ("database", &self.database),
            ("user", &self.user),
            ("password", &self.password),
        ] {
            ensure!(
                !value.contains('\0') && value.len() <= MAX_CREDENTIAL_BYTES,
                "PostgreSQL {name} must not contain NUL and must be at most {MAX_CREDENTIAL_BYTES} bytes"
            );
        }
        ensure!(
            (1..=MAX_COMMAND_TIMEOUT_MS).contains(&self.command_timeout_ms),
            "commandTimeoutMs must be in 1..={MAX_COMMAND_TIMEOUT_MS}"
        );
        for (name, value, minimum) in [
            ("connectTimeoutMs", self.connect_timeout_ms, 1),
            ("reconnectBudgetMs", self.reconnect_budget_ms, 0),
            ("shutdownTimeoutMs", self.shutdown_timeout_ms, 1),
        ] {
            ensure!(
                (minimum..=MAX_DURATION_MS).contains(&value),
                "{name} must be in {minimum}..={MAX_DURATION_MS}"
            );
        }
        ensure!(
            (1..=MAX_STREAMS_LIMIT).contains(&self.max_streams),
            "maxStreams must be in 1..={MAX_STREAMS_LIMIT}"
        );
        self.table()?;
        ensure!(
            self.tls_client_certificate_pem.is_some() == self.tls_client_private_key_pem.is_some(),
            "tlsClientCertificatePem and tlsClientPrivateKeyPem must be supplied together"
        );
        for (name, value) in [
            ("tlsCaPem", &self.tls_ca_pem),
            ("tlsClientCertificatePem", &self.tls_client_certificate_pem),
            ("tlsClientPrivateKeyPem", &self.tls_client_private_key_pem),
        ] {
            if let Some(value) = value {
                ensure!(
                    self.ssl_mode == SslMode::Require,
                    "{name} requires sslMode require"
                );
                ensure!(
                    !value.is_empty() && value.len() <= MAX_PEM_BYTES && !value.contains('\0'),
                    "{name} must be 1..={MAX_PEM_BYTES} bytes without NUL"
                );
            }
        }
        if let Some(pem) = &self.tls_ca_pem {
            native_tls::Certificate::from_pem(pem.as_bytes()).context("invalid tlsCaPem")?;
        }
        if let (Some(certificate), Some(key)) = (
            &self.tls_client_certificate_pem,
            &self.tls_client_private_key_pem,
        ) {
            native_tls::Identity::from_pkcs8(certificate.as_bytes(), key.as_bytes())
                .context("invalid tlsClientCertificatePem or tlsClientPrivateKeyPem")?;
        }
        ensure!(
            self.ssl_mode == SslMode::Require
                || self.allow_remote_plaintext
                || is_loopback_host(&self.host),
            "sslMode disable sends credentials and data unencrypted to the non-loopback host '{}'; \
             use sslMode require or set allowRemotePlaintext",
            self.host
        );
        self.outputs.validate()?;
        let commands = Commands::compile(self)?;
        ensure!(
            !commands.compiled.is_empty(),
            "PostgreSQL sink requires at least one nonempty stored-procedure template"
        );
        Ok(())
    }

    /// Checks that depend on the incoming edges: every route must name an
    /// output of this sink.
    pub fn validate_outputs(&self, outputs: &SinkOutputs) -> Result<()> {
        for key in self.routes.keys() {
            ensure!(
                outputs
                    .aliases()
                    .any(|alias| alias == key || alias.rsplit('.').next() == Some(key.as_str())),
                "route '{key}' does not match an output alias or its last dotted segment"
            );
        }
        Ok(())
    }

    pub fn table(&self) -> Result<ProgressTable> {
        ProgressTable::parse(&self.progress_table)
    }

    pub fn command_timeout(&self) -> Duration {
        Duration::from_millis(self.command_timeout_ms)
    }

    /// The client-side bound on one command: the server timeout plus a grace
    /// so the server's own cancellation (a clean rollback) normally wins.
    pub fn client_command_timeout(&self) -> Duration {
        self.command_timeout() + Duration::from_secs(5)
    }

    pub fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms)
    }

    pub fn reconnect_budget(&self) -> Duration {
        Duration::from_millis(self.reconnect_budget_ms)
    }

    pub fn shutdown_timeout(&self) -> Duration {
        Duration::from_millis(self.shutdown_timeout_ms)
    }
}

/// Whether plaintext to `host` stays on this machine.
pub fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

/// Compiled command templates and their routing.
pub(crate) struct Commands {
    routes: BTreeMap<String, CommandRoute>,
    default: Option<CommandRoute>,
    compiled: HashMap<String, CompiledTemplate>,
}

/// The command chosen for one operation.
pub(crate) enum Command<'a> {
    /// No template is configured (or it is empty) for this operation.
    None,
    Template(&'a CompiledTemplate),
}

impl Commands {
    pub(crate) fn compile(config: &PostgresStoredProcConfig) -> Result<Self> {
        let mut compiled = HashMap::new();
        for (name, route) in config
            .routes
            .iter()
            .map(|(alias, route)| (format!("routes.{alias}"), route))
            .chain(
                config
                    .default_template
                    .iter()
                    .map(|route| ("defaultTemplate".to_owned(), route)),
            )
        {
            for spec in route.specs() {
                if !spec.template.trim().is_empty() && !compiled.contains_key(&spec.template) {
                    compiled.insert(
                        spec.template.clone(),
                        CompiledTemplate::compile_parameter_only(&spec.template)
                            .with_context(|| format!("invalid command template in {name}"))?,
                    );
                }
            }
        }
        Ok(Self {
            routes: config.routes.clone(),
            default: config.default_template.clone(),
            compiled,
        })
    }

    /// Full alias, then its last dotted segment, then the default, per
    /// operation. The first configured spec wins, even an empty one.
    pub(crate) fn resolve(&self, alias: &str, operation: OperationType) -> Command<'_> {
        let spec = self
            .routes
            .get(alias)
            .and_then(|route| route.select(operation))
            .or_else(|| {
                self.routes
                    .get(alias.rsplit('.').next().unwrap_or(alias))
                    .and_then(|route| route.select(operation))
            })
            .or_else(|| {
                self.default
                    .as_ref()
                    .and_then(|route| route.select(operation))
            });
        match spec {
            Some(spec) if !spec.template.trim().is_empty() => self
                .compiled
                .get(&spec.template)
                .map_or(Command::None, Command::Template),
            _ => Command::None,
        }
    }
}
