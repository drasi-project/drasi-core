// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Shared connection, binding and cursor contracts for native PostgreSQL CDC
//! and its independently hosted coordinated snapshot provider.

use anyhow::{Context, Result};
use bytes::Bytes;
use drasi_computation_contracts::{
    ComponentId, SourceProgressKey, SourceProgressSnapshot, SourceTransactionError,
    SourceTransactionLimits, StreamId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    num::{NonZeroU64, NonZeroUsize},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::Instant,
};
use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};

use crate::{
    config::{PostgresSourceConfig, SslMode},
    connection::{format_lsn, parse_lsn, ReplicationConnection},
};

pub mod rows;
pub mod shared_transactions;
pub mod transaction;
pub use transaction::{row_element, Transactions};
pub type TableKeys = BTreeMap<(String, String), Vec<String>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresTransactionConfig {
    pub connection: PostgresSourceConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    pub start_lsn: String,
    pub transactions: SourceTransactionLimits,
    pub max_protocol_bytes: NonZeroUsize,
    pub io_timeout_ms: NonZeroU64,
    pub feedback_interval_ms: NonZeroU64,
}

impl PostgresTransactionConfig {
    pub fn validate(&self) -> Result<()> {
        self.connection.validate()?;
        anyhow::ensure!(
            [
                &self.connection.host,
                &self.connection.database,
                &self.connection.user,
                &self.connection.password
            ]
            .into_iter()
            .all(|value| !value.contains('\0')),
            "PostgreSQL connection values cannot contain NUL"
        );
        anyhow::ensure!(
            self.connection.slot_name.len() <= 63
                && self
                    .connection
                    .slot_name
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'),
            "replication slot must contain at most 63 lowercase letters, digits or underscores"
        );
        anyhow::ensure!(
            !self.connection.publication_name.contains('\0')
                && self.connection.publication_name.len() <= 63,
            "invalid publication name"
        );
        anyhow::ensure!(
            self.max_protocol_bytes.get() >= 64,
            "PostgreSQL protocol byte limit must be at least 64"
        );
        anyhow::ensure!(
            self.tls_ca_pem.as_ref().is_none_or(|pem| !pem.is_empty()
                && pem.len() <= 64 * 1024
                && pem.len() <= self.max_protocol_bytes.get()
                && !pem.contains('\0')),
            "PostgreSQL CA certificate must fit the protocol limit and 64 KiB without NUL"
        );
        parse_lsn(&self.start_lsn)?;
        Instant::now()
            .checked_add(self.transactions.duration())
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

    pub fn validate_coordinated(&self) -> Result<()> {
        self.validate()?;
        anyhow::ensure!(
            self.connection.slot_name.len() <= 30,
            "managed PostgreSQL slot prefix must contain at most 30 characters"
        );
        Ok(())
    }

    pub fn timeout(&self) -> Duration {
        Duration::from_millis(self.io_timeout_ms.get())
    }
}

pub trait ReplicationIo: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> ReplicationIo for T {}
pub type Connection = ReplicationConnection<Box<dyn ReplicationIo>>;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub version: u32,
    pub binding: [u8; 32],
    pub commit_lsn: u64,
    /// Only a completed query-owned snapshot authorizes a managed slot. Retain
    /// its identity in every later cursor so a separately loaded source can resume.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_slot: Option<String>,
}

pub fn read_position(
    snapshot: &SourceProgressSnapshot,
    source: &ComponentId,
) -> Result<Option<Position>> {
    let Some(checkpoint) = snapshot
        .checkpoints
        .get(&SourceProgressKey::Source(source.as_str().into()))
    else {
        return Ok(None);
    };
    let position: Position = serde_json::from_slice(
        checkpoint
            .source_position
            .as_deref()
            .context("PostgreSQL checkpoint has no cursor")?,
    )?;
    anyhow::ensure!(
        position.version == 1 && position.commit_lsn == checkpoint.sequence,
        "PostgreSQL checkpoint version or sequence mismatch"
    );
    Ok(Some(position))
}

pub fn checkpoint(
    snapshot: &SourceProgressSnapshot,
    source: &ComponentId,
    binding: [u8; 32],
) -> Result<Option<u64>> {
    read_position(snapshot, source)?
        .map(|position| {
            anyhow::ensure!(
                position.binding == binding,
                "PostgreSQL checkpoint source binding mismatch"
            );
            Ok(position.commit_lsn)
        })
        .transpose()
}

pub fn validate_managed_slot(prefix: &str, slot: &str) -> Result<()> {
    let prefix = format!("{prefix}_");
    let suffix = slot
        .strip_prefix(&prefix)
        .context("managed slot prefix changed")?;
    anyhow::ensure!(
        suffix.len() == 32
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
        "invalid PostgreSQL managed slot identity"
    );
    Ok(())
}

pub fn live_config(
    config: &PostgresTransactionConfig,
    progress: &SourceProgressSnapshot,
    source: &ComponentId,
) -> Result<PostgresTransactionConfig> {
    config.validate_coordinated()?;
    anyhow::ensure!(
        progress.bootstrap_complete,
        "PostgreSQL snapshot is incomplete"
    );
    let position = read_position(progress, source)?
        .context("completed PostgreSQL snapshot has no committed position")?;
    let slot = position
        .managed_slot
        .context("PostgreSQL snapshot has no committed slot ownership")?;
    validate_managed_slot(&config.connection.slot_name, &slot)?;
    let mut config = config.clone();
    config.connection.slot_name = slot;
    config.start_lsn = format_lsn(position.commit_lsn);
    Ok(config)
}

pub struct ConnectionOptions<'a> {
    pub host: &'a str,
    pub port: u16,
    pub database: &'a str,
    pub user: &'a str,
    pub password: &'a str,
    pub ssl_mode: SslMode,
    pub tls_ca_pem: Option<&'a str>,
    pub max_protocol_bytes: NonZeroUsize,
    pub timeout: Duration,
}

pub async fn connect(config: &PostgresTransactionConfig) -> Result<Connection> {
    let settings = &config.connection;
    connect_with_options(
        ConnectionOptions {
            host: &settings.host,
            port: settings.port,
            database: &settings.database,
            user: &settings.user,
            password: &settings.password,
            ssl_mode: settings.ssl_mode,
            tls_ca_pem: config.tls_ca_pem.as_deref(),
            max_protocol_bytes: config.max_protocol_bytes,
            timeout: config.timeout(),
        },
        true,
    )
    .await
}

pub async fn connect_with_options(
    settings: ConnectionOptions<'_>,
    replication: bool,
) -> Result<Connection> {
    let mut stream = TcpStream::connect((settings.host, settings.port)).await?;
    stream.set_nodelay(true)?;
    let stream: Box<dyn ReplicationIo> = if settings.ssl_mode == SslMode::Disable {
        Box::new(stream)
    } else {
        stream.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).await?;
        stream.flush().await?;
        match stream.read_u8().await? {
            b'S' => {
                let mut tls = native_tls::TlsConnector::builder();
                if let Some(pem) = settings.tls_ca_pem {
                    tls.add_root_certificate(
                        native_tls::Certificate::from_pem(pem.as_bytes())
                            .context("invalid PostgreSQL CA certificate")?,
                    );
                }
                let mut connector = postgres_native_tls::MakeTlsConnector::new(tls.build()?);
                let connector = <postgres_native_tls::MakeTlsConnector as MakeTlsConnect<
                    TcpStream,
                >>::make_tls_connect(&mut connector, settings.host)?;
                Box::new(connector.connect(stream).await?)
            }
            b'N' if settings.ssl_mode == SslMode::Prefer => Box::new(stream),
            b'N' => anyhow::bail!("PostgreSQL server refused required TLS"),
            _ => anyhow::bail!("invalid PostgreSQL TLS negotiation response"),
        }
    };
    let startup = if replication {
        crate::protocol::StartupMessage::new_replication(settings.database, settings.user)
    } else {
        crate::protocol::StartupMessage::new(settings.database, settings.user)
    };
    let mut connection = ReplicationConnection::from_stream_with_startup(
        stream,
        startup,
        settings.password,
        Some(settings.max_protocol_bytes),
    )
    .await?;
    connection.set_partial_read_timeout(settings.timeout);
    Ok(connection)
}

async fn table_keys(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
) -> Result<TableKeys> {
    let publication = config.connection.publication_name.replace('\'', "''");
    let rows = connection.query_rows(
        format!("SELECT schemaname, tablename FROM pg_publication_tables WHERE pubname = '{publication}'"),
        config.max_protocol_bytes,
    ).await?;
    let mut tables = BTreeSet::new();
    for row in rows {
        tables.insert((field(&row, 0)?, field(&row, 1)?));
    }
    anyhow::ensure!(!tables.is_empty(), "PostgreSQL publication has no tables");
    if !config.connection.tables.is_empty() {
        let selected: BTreeSet<_> = config
            .connection
            .tables
            .iter()
            .map(|table| table_name(table))
            .collect();
        anyhow::ensure!(
            selected.is_subset(&tables),
            "configured table is not in the PostgreSQL publication"
        );
        tables = selected;
    }
    let rows = connection
        .query_rows(crate::PRIMARY_KEY_QUERY.into(), config.max_protocol_bytes)
        .await?;
    let mut keys: TableKeys = tables
        .into_iter()
        .map(|table| (table, Vec::new()))
        .collect();
    for row in rows {
        if let Some(columns) = keys.get_mut(&(field(&row, 0)?, field(&row, 1)?)) {
            columns.push(field(&row, 2)?);
        }
    }
    let mut configured = BTreeSet::new();
    for key in &config.connection.table_keys {
        let table = table_name(&key.table);
        anyhow::ensure!(
            configured.insert(table.clone()),
            "duplicate table key configuration"
        );
        let columns = keys
            .get_mut(&table)
            .context("configured key table is not selected")?;
        anyhow::ensure!(
            !key.key_columns.is_empty()
                && key.key_columns.iter().collect::<BTreeSet<_>>().len() == key.key_columns.len(),
            "table key columns must be nonempty and unique"
        );
        *columns = key.key_columns.clone();
    }
    anyhow::ensure!(
        keys.values().all(|columns| !columns.is_empty()),
        "every transactional table requires a primary or configured key"
    );
    Ok(keys)
}

pub async fn source_binding(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
    source: &ComponentId,
    stream: &StreamId,
) -> Result<(TableKeys, [u8; 32])> {
    let system = connection.identify_system().await?;
    let system_id = system
        .get("systemid")
        .context("missing PostgreSQL system identifier")?;
    let database = system
        .get("dbname")
        .context("missing PostgreSQL database identity")?;
    anyhow::ensure!(
        database == &config.connection.database,
        "PostgreSQL database identity changed"
    );
    let keys = table_keys(connection, config).await?;
    let publication = config.connection.publication_name.replace('\'', "''");
    let flags = connection.query_rows(
        format!("SELECT pubinsert, pubupdate, pubdelete, pubtruncate, puballtables, pubviaroot FROM pg_publication WHERE pubname = '{publication}'"),
        config.max_protocol_bytes,
    ).await?;
    let [flags_row] = flags.as_slice() else {
        anyhow::bail!("missing PostgreSQL publication");
    };
    anyhow::ensure!(
        (0..3).all(|index| flags_row.get(index).and_then(Option::as_deref) == Some(b"t")),
        "native PostgreSQL requires publication of inserts, updates and deletes"
    );
    let version = connection
        .query_rows("SHOW server_version_num".into(), config.max_protocol_bytes)
        .await?;
    let [version] = version.as_slice() else {
        anyhow::bail!("missing PostgreSQL version");
    };
    let filters = if field(version, 0)?.parse::<u32>()? >= 150000 {
        connection.query_rows(
            format!("SELECT schemaname, tablename, attnames::text, rowfilter FROM pg_publication_tables WHERE pubname = '{publication}' ORDER BY schemaname, tablename"),
            config.max_protocol_bytes,
        ).await?
    } else {
        Vec::new()
    };
    let columns = connection.query_rows(
        format!("SELECT p.schemaname, p.tablename, a.attname, a.atttypid::text, a.atttypmod::text, c.relreplident::text FROM pg_publication_tables p JOIN pg_namespace n ON n.nspname = p.schemaname JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = p.tablename JOIN pg_attribute a ON a.attrelid = c.oid WHERE p.pubname = '{publication}' AND a.attnum > 0 AND NOT a.attisdropped ORDER BY p.schemaname, p.tablename, a.attnum"),
        config.max_protocol_bytes,
    ).await?;
    let binding = Sha256::digest(serde_json::to_vec(&(
        1u32,
        system_id,
        database,
        &config.connection.slot_name,
        &config.connection.publication_name,
        keys.iter().collect::<Vec<_>>(),
        source.as_str(),
        stream.as_str(),
        flags,
        filters,
        columns,
    ))?)
    .into();
    Ok((keys, binding))
}

pub async fn validate_retention(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
) -> Result<()> {
    let rows = connection
        .query_rows(
            format!(
                "SELECT current_setting('max_slot_wal_keep_size', true), \
         current_setting('idle_replication_slot_timeout', true), temporary \
         FROM pg_replication_slots WHERE slot_name = '{}' AND database = current_database()",
                config.connection.slot_name,
            ),
            config.max_protocol_bytes,
        )
        .await?;
    let [row] = rows.as_slice() else {
        anyhow::bail!("native PostgreSQL source requires an existing slot in this database");
    };
    anyhow::ensure!(
        field(row, 0)? == "-1",
        "native PostgreSQL requires max_slot_wal_keep_size = -1"
    );
    if let Some(Some(timeout)) = row.get(1) {
        anyhow::ensure!(
            timeout.as_ref() == b"0",
            "native PostgreSQL requires idle_replication_slot_timeout = 0"
        );
    }
    anyhow::ensure!(
        field(row, 2)? == "f",
        "native PostgreSQL requires a persistent replication slot"
    );
    Ok(())
}

pub fn field(row: &[Option<Bytes>], index: usize) -> Result<String> {
    Ok(std::str::from_utf8(
        row.get(index)
            .and_then(Option::as_deref)
            .context("incomplete PostgreSQL catalog row")?,
    )?
    .to_owned())
}

fn table_name(table: &str) -> (String, String) {
    let (namespace, table) = table.split_once('.').unwrap_or(("public", table));
    (namespace.into(), table.into())
}
