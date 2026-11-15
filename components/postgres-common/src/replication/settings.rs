// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The flat native PostgreSQL configuration shared by the native source and
//! its bootstrap providers, and the identity of the replication slots they
//! own.
//!
//! The configuration is a single object of camelCase fields. `password` is a
//! top-level field so the host can resolve it from a secret reference. A
//! managed slot's name is derived from the graph and source identifiers, so
//! erasing disposal can find and remove exactly the slots this source owns.

use super::*;
use crate::config::SslMode;

/// Default PostgreSQL port.
pub const DEFAULT_PORT: u16 = 5432;
/// Default bound on one PostgreSQL protocol message or metadata result.
pub const DEFAULT_MAX_PROTOCOL_BYTES: usize = 16 * 1024 * 1024;
/// Default I/O deadline for connection, metadata and feedback operations.
pub const DEFAULT_IO_TIMEOUT_MS: u64 = 30_000;
/// Default standby-status interval. It must stay below the server's
/// `wal_sender_timeout` (60 s by default) so an idle or paused source is not
/// disconnected.
pub const DEFAULT_FEEDBACK_INTERVAL_MS: u64 = 10_000;
/// Default bound on the changes in one source transaction.
pub const DEFAULT_MAX_TRANSACTION_CHANGES: usize = 100_000;
/// Default bound on the encoded bytes of one source transaction.
pub const DEFAULT_MAX_TRANSACTION_BYTES: usize = 64 * 1024 * 1024;
/// Default deadline for assembling and publishing one source transaction.
pub const DEFAULT_MAX_TRANSACTION_DURATION_MS: u64 = 300_000;
/// Default total time allowed for reconnecting after a transport failure.
pub const DEFAULT_RECONNECT_BUDGET_MS: u64 = 300_000;
/// Upper bound on a configured reconnect budget.
pub const MAX_RECONNECT_BUDGET_MS: u64 = 86_400_000;
/// Longest PostgreSQL identifier (`NAMEDATALEN - 1`).
pub const MAX_IDENTIFIER_BYTES: usize = 63;
/// Longest coordinated slot prefix: the provider appends `_` and 32 hex digits.
pub const MAX_MANAGED_PREFIX_BYTES: usize = 30;
/// Largest accepted CA certificate bundle.
pub const MAX_CA_PEM_BYTES: usize = 64 * 1024;

/// Who creates, owns and removes the replication slot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SlotOwnership {
    /// The source creates its slot on first start and drops it on erasing
    /// disposal. The name is derived from the graph and source identifiers.
    #[default]
    Managed,
    /// An operator creates and removes `slotName`; the source never creates
    /// or drops it.
    External,
}

/// An explicit key for a table without a usable primary key.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PostgresTableKey {
    /// `schema.table`, or `table` in `public`.
    pub table: String,
    /// Key columns, in key order.
    pub key_columns: Vec<String>,
}

/// The slot names a managed source owns, derived from its identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SlotIdentity {
    /// Prefix of every slot this source owns, including coordinated snapshot
    /// slots (`{prefix}_{32 hex digits}`).
    pub prefix: String,
    /// The managed live slot used without a coordinated snapshot.
    pub live: String,
}

impl SlotIdentity {
    /// Derives the identity for `source` in `graph`. Stable across restarts,
    /// processes and configuration updates; distinct across graphs.
    pub fn derive(graph: &str, source: &ComponentId) -> Self {
        let digest = |label: &str| {
            let mut hash = Sha256::new();
            hash.update(b"drasi.postgres/slot/v1\0");
            hash.update(graph.as_bytes());
            hash.update(b"\0");
            hash.update(source.as_str().as_bytes());
            hash.update(b"\0");
            hash.update(label.as_bytes());
            hash.finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let prefix = format!("drasi_{}", &digest("prefix")[..24]);
        let live = format!("{prefix}_{}", &digest("live")[..32]);
        Self { prefix, live }
    }

    /// Whether `slot` is one of the slots this identity owns.
    pub fn owns(&self, slot: &str) -> bool {
        validate_managed_slot(&self.prefix, slot).is_ok()
    }
}

/// Runtime-only slot selection; never serialized.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct RuntimeSlot {
    identity: Option<SlotIdentity>,
    active: Option<String>,
}

/// Native PostgreSQL CDC configuration, shared by the source and its
/// bootstrap providers so both compute the same binding.
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PostgresTransactionConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    pub database: String,
    pub user: String,
    /// Optional when the host supplies activation credentials.
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub ssl_mode: SslMode,
    /// Additional trusted CA certificates (PEM) for TLS verification.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    /// The publication to stream. It must publish inserts, updates and
    /// deletes, and must not publish TRUNCATE.
    pub publication: String,
    /// Tables to emit (`schema.table`); empty means every publication table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tables: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub table_keys: Vec<PostgresTableKey>,
    #[serde(default)]
    pub slot_ownership: SlotOwnership,
    /// Required for `external` ownership; rejected for `managed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot_name: Option<String>,
    /// Initial cursor before the first checkpoint. When absent, streaming
    /// starts at the slot's confirmed position.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_lsn: Option<String>,
    #[serde(default = "default_max_transaction_changes")]
    pub max_transaction_changes: NonZeroUsize,
    #[serde(default = "default_max_transaction_bytes")]
    pub max_transaction_bytes: NonZeroUsize,
    #[serde(default = "default_max_transaction_duration_ms")]
    pub max_transaction_duration_ms: NonZeroU64,
    #[serde(default = "default_max_protocol_bytes")]
    pub max_protocol_bytes: NonZeroUsize,
    #[serde(default = "default_io_timeout_ms")]
    pub io_timeout_ms: NonZeroU64,
    #[serde(default = "default_feedback_interval_ms")]
    pub feedback_interval_ms: NonZeroU64,
    /// Total time to keep reconnecting after a transport failure; `0`
    /// disables reconnection.
    #[serde(default = "default_reconnect_budget_ms")]
    pub reconnect_budget_ms: u64,
    #[serde(skip)]
    runtime: RuntimeSlot,
}

impl std::fmt::Debug for PostgresTransactionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresTransactionConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"[REDACTED]")
            .field("ssl_mode", &self.ssl_mode)
            .field("publication", &self.publication)
            .field("tables", &self.tables)
            .field("slot_ownership", &self.slot_ownership)
            .field("slot", &self.slot_name())
            .field("start_lsn", &self.start_lsn)
            .finish_non_exhaustive()
    }
}

fn default_host() -> String {
    "localhost".into()
}
fn default_port() -> u16 {
    DEFAULT_PORT
}
fn default_max_transaction_changes() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_TRANSACTION_CHANGES).expect("nonzero constant")
}
fn default_max_transaction_bytes() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_TRANSACTION_BYTES).expect("nonzero constant")
}
fn default_max_transaction_duration_ms() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_MAX_TRANSACTION_DURATION_MS).expect("nonzero constant")
}
fn default_max_protocol_bytes() -> NonZeroUsize {
    NonZeroUsize::new(DEFAULT_MAX_PROTOCOL_BYTES).expect("nonzero constant")
}
fn default_io_timeout_ms() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_IO_TIMEOUT_MS).expect("nonzero constant")
}
fn default_feedback_interval_ms() -> NonZeroU64 {
    NonZeroU64::new(DEFAULT_FEEDBACK_INTERVAL_MS).expect("nonzero constant")
}
fn default_reconnect_budget_ms() -> u64 {
    DEFAULT_RECONNECT_BUDGET_MS
}

impl PostgresTransactionConfig {
    /// A configuration with every operational default, for the given
    /// connection, publication and slot ownership.
    pub fn new(
        host: impl Into<String>,
        port: u16,
        database: impl Into<String>,
        user: impl Into<String>,
        publication: impl Into<String>,
    ) -> Self {
        Self {
            host: host.into(),
            port,
            database: database.into(),
            user: user.into(),
            password: String::new(),
            ssl_mode: SslMode::default(),
            tls_ca_pem: None,
            publication: publication.into(),
            tables: Vec::new(),
            table_keys: Vec::new(),
            slot_ownership: SlotOwnership::Managed,
            slot_name: None,
            start_lsn: None,
            max_transaction_changes: default_max_transaction_changes(),
            max_transaction_bytes: default_max_transaction_bytes(),
            max_transaction_duration_ms: default_max_transaction_duration_ms(),
            max_protocol_bytes: default_max_protocol_bytes(),
            io_timeout_ms: default_io_timeout_ms(),
            feedback_interval_ms: default_feedback_interval_ms(),
            reconnect_budget_ms: default_reconnect_budget_ms(),
            runtime: RuntimeSlot::default(),
        }
    }

    /// Uses an operator-owned slot.
    pub fn with_external_slot(mut self, slot: impl Into<String>) -> Self {
        self.slot_ownership = SlotOwnership::External;
        self.slot_name = Some(slot.into());
        self
    }

    /// Source transaction limits built from the flat limit fields.
    pub fn transactions(&self) -> SourceTransactionLimits {
        SourceTransactionLimits {
            max_changes: self.max_transaction_changes,
            max_bytes: self.max_transaction_bytes,
            max_duration_ms: self.max_transaction_duration_ms,
        }
    }

    /// Binds a managed configuration to its owner. Required before any
    /// connection for `managed` ownership; a no-op for `external`.
    pub fn resolve(&mut self, graph: &str, source: &ComponentId) -> Result<()> {
        drasi_computation_contracts::validate_identifier("graph", graph)?;
        if self.slot_ownership == SlotOwnership::Managed {
            self.runtime.identity = Some(SlotIdentity::derive(graph, source));
        }
        Ok(())
    }

    /// A copy that streams from `slot`, a slot this configuration owns
    /// (a coordinated snapshot's slot), starting at `start`.
    pub fn with_active_slot(&self, slot: String, start: u64) -> Self {
        let mut config = self.clone();
        config.runtime.active = Some(slot);
        config.start_lsn = Some(format_lsn(start));
        config
    }

    /// The managed identity, when ownership is managed and resolved.
    pub fn slot_identity(&self) -> Option<&SlotIdentity> {
        self.runtime.identity.as_ref()
    }

    /// The slot this configuration streams from. Empty only for an unresolved
    /// managed configuration, which [`Self::validate`] rejects.
    pub fn slot_name(&self) -> &str {
        if let Some(active) = &self.runtime.active {
            return active;
        }
        match self.slot_ownership {
            SlotOwnership::External => self.slot_name.as_deref().unwrap_or_default(),
            SlotOwnership::Managed => self
                .runtime
                .identity
                .as_ref()
                .map_or("", |identity| identity.live.as_str()),
        }
    }

    /// The coordinated snapshot slot prefix. Coordinated snapshots require
    /// managed ownership.
    pub fn managed_prefix(&self) -> Result<&str> {
        Ok(&self
            .slot_identity()
            .context("coordinated PostgreSQL snapshots require managed slot ownership")?
            .prefix)
    }

    /// Whether the source may create the slot it streams from.
    pub fn creates_slot(&self) -> bool {
        self.slot_ownership == SlotOwnership::Managed && self.runtime.active.is_none()
    }

    /// The initial cursor, when configured.
    pub fn start_lsn(&self) -> Result<Option<u64>> {
        self.start_lsn.as_deref().map(parse_lsn).transpose()
    }

    /// Validates the configuration without I/O. Managed configurations must
    /// be resolved first.
    pub fn validate(&self) -> Result<()> {
        self.validate_static()?;
        anyhow::ensure!(
            !self.slot_name().is_empty(),
            "managed PostgreSQL slot ownership is not bound to a graph and source"
        );
        Ok(())
    }

    /// Validates the configuration for a coordinated snapshot, which creates
    /// and hands over its own managed slots.
    pub fn validate_coordinated(&self) -> Result<()> {
        self.validate()?;
        anyhow::ensure!(
            self.managed_prefix()?.len() <= MAX_MANAGED_PREFIX_BYTES,
            "managed PostgreSQL slot prefix is too long"
        );
        Ok(())
    }

    /// Validation that needs no owner identity.
    pub fn validate_static(&self) -> Result<()> {
        for (name, value) in [
            ("host", &self.host),
            ("database", &self.database),
            ("user", &self.user),
            ("publication", &self.publication),
        ] {
            anyhow::ensure!(
                !value.is_empty() && !value.contains('\0'),
                "PostgreSQL {name} must be nonempty and contain no NUL"
            );
        }
        anyhow::ensure!(self.port != 0, "PostgreSQL port must be 1..=65535");
        anyhow::ensure!(
            !self.password.contains('\0'),
            "PostgreSQL password cannot contain NUL"
        );
        anyhow::ensure!(
            self.database.len() <= MAX_IDENTIFIER_BYTES
                && self.publication.len() <= MAX_IDENTIFIER_BYTES,
            "PostgreSQL database and publication names are at most 63 bytes"
        );
        match self.slot_ownership {
            SlotOwnership::External => {
                let slot = self
                    .slot_name
                    .as_deref()
                    .context("external PostgreSQL slot ownership requires slotName")?;
                validate_slot_name(slot)?;
            }
            SlotOwnership::Managed => anyhow::ensure!(
                self.slot_name.is_none(),
                "managed PostgreSQL slots are named by the source; remove slotName or use external ownership"
            ),
        }
        for table in &self.tables {
            anyhow::ensure!(
                !table.is_empty() && !table.contains('\0'),
                "PostgreSQL table names must be nonempty and contain no NUL"
            );
        }
        let mut keyed = BTreeSet::new();
        for key in &self.table_keys {
            anyhow::ensure!(
                keyed.insert(table_name(&key.table)),
                "duplicate PostgreSQL table key configuration for {}",
                key.table
            );
            anyhow::ensure!(
                !key.key_columns.is_empty()
                    && key.key_columns.iter().collect::<BTreeSet<_>>().len()
                        == key.key_columns.len(),
                "PostgreSQL table key columns must be nonempty and unique"
            );
        }
        anyhow::ensure!(
            self.max_protocol_bytes.get() >= 64,
            "PostgreSQL protocol byte limit must be at least 64"
        );
        anyhow::ensure!(
            self.tls_ca_pem.as_ref().is_none_or(|pem| !pem.is_empty()
                && pem.len() <= MAX_CA_PEM_BYTES
                && pem.len() <= self.max_protocol_bytes.get()
                && !pem.contains('\0')),
            "PostgreSQL CA certificate must fit the protocol limit and 64 KiB without NUL"
        );
        anyhow::ensure!(
            self.reconnect_budget_ms <= MAX_RECONNECT_BUDGET_MS,
            "PostgreSQL reconnectBudgetMs is at most {MAX_RECONNECT_BUDGET_MS}"
        );
        self.start_lsn()?;
        Instant::now()
            .checked_add(self.transactions().duration())
            .ok_or(SourceTransactionError::InvalidDeadline)?;
        anyhow::ensure!(
            Instant::now().checked_add(self.timeout()).is_some()
                && Instant::now()
                    .checked_add(Duration::from_millis(self.feedback_interval_ms.get()))
                    .is_some(),
            "PostgreSQL I/O or heartbeat duration is not representable"
        );
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.io_timeout_ms.get())
    }

    pub fn feedback_interval(&self) -> Duration {
        Duration::from_millis(self.feedback_interval_ms.get())
    }

    /// The reconnect budget, or `None` when reconnection is disabled.
    pub fn reconnect_budget(&self) -> Option<Duration> {
        (self.reconnect_budget_ms > 0).then(|| Duration::from_millis(self.reconnect_budget_ms))
    }

    /// The configuration with activation credentials applied.
    pub fn with_credentials(&self, user: String, password: String) -> Self {
        let mut config = self.clone();
        config.user = user;
        config.password = password;
        config
    }
}

/// Rejects names PostgreSQL would fold, truncate or need quoting for in the
/// replication protocol.
pub fn validate_slot_name(slot: &str) -> Result<()> {
    anyhow::ensure!(
        !slot.is_empty()
            && slot.len() <= MAX_IDENTIFIER_BYTES
            && slot
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
        "replication slot must contain 1 to 63 lowercase letters, digits or underscores"
    );
    Ok(())
}

/// Quotes a SQL string literal for the simple-query protocol.
pub fn quote_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// Quotes a SQL identifier.
pub fn quote_identifier(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_identity_is_stable_distinct_and_fits_postgres_limits() {
        let orders = ComponentId::generate();
        let first = SlotIdentity::derive("graph-a", &orders);
        assert_eq!(first, SlotIdentity::derive("graph-a", &orders));
        assert_ne!(first, SlotIdentity::derive("graph-b", &orders));
        assert_ne!(
            first,
            SlotIdentity::derive("graph-a", &ComponentId::generate())
        );
        assert!(first.prefix.len() <= MAX_MANAGED_PREFIX_BYTES);
        assert!(first.live.len() <= MAX_IDENTIFIER_BYTES);
        validate_slot_name(&first.live).unwrap();
        assert!(first.owns(&first.live));
        assert!(first.owns(&format!("{}_{}", first.prefix, "0".repeat(32))));
        assert!(!first.owns(&format!("{}_extra", first.prefix)));
        let other = SlotIdentity::derive("graph-b", &orders);
        assert!(!first.owns(&other.live));
    }

    #[test]
    fn defaults_fill_every_operational_field() {
        let config: PostgresTransactionConfig = serde_json::from_value(serde_json::json!({
            "database": "app", "user": "drasi", "publication": "pub"
        }))
        .unwrap();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.slot_ownership, SlotOwnership::Managed);
        assert_eq!(config.reconnect_budget_ms, DEFAULT_RECONNECT_BUDGET_MS);
        config.validate_static().unwrap();
        assert!(
            config.validate().is_err(),
            "an unresolved managed slot is rejected"
        );
        let mut resolved = config.clone();
        resolved.resolve("graph", &ComponentId::generate()).unwrap();
        resolved.validate_coordinated().unwrap();
        assert!(resolved.slot_name().starts_with("drasi_"));
        assert_eq!(
            serde_json::to_value(&resolved).unwrap(),
            serde_json::to_value(&config).unwrap(),
            "runtime slot selection is never serialized"
        );
    }

    #[test]
    fn slot_ownership_rules_are_explicit() {
        let base = PostgresTransactionConfig::new("h", 5432, "db", "u", "pub");
        let mut managed_with_name = base.clone();
        managed_with_name.slot_name = Some("mine".into());
        assert!(managed_with_name.validate_static().is_err());
        let mut external_without_name = base.clone();
        external_without_name.slot_ownership = SlotOwnership::External;
        assert!(external_without_name.validate_static().is_err());
        let external = base.clone().with_external_slot("ops_slot");
        external.validate().unwrap();
        assert_eq!(external.slot_name(), "ops_slot");
        assert!(!external.creates_slot());
        assert!(external.managed_prefix().is_err());
        assert!(base
            .clone()
            .with_external_slot("Bad-Name")
            .validate_static()
            .is_err());
    }

    #[test]
    fn debug_redacts_the_password() {
        let mut config = PostgresTransactionConfig::new("h", 5432, "db", "u", "pub");
        config.password = "hunter2".into();
        assert!(!format!("{config:?}").contains("hunter2"));
    }

    #[test]
    fn quoting_doubles_embedded_quotes() {
        assert_eq!(quote_literal("o'brien"), "'o''brien'");
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
    }
}
