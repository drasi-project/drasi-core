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
    config::SslMode,
    connection::{format_lsn, parse_lsn, ReplicationConnection},
};

pub mod coordination;
pub mod definition;
pub mod error;
pub mod rows;
pub mod schema;
pub mod settings;
pub mod shared_transactions;
pub mod transaction;
pub use error::{is_permanent, is_transient, AddressUnresolved, PostgresCdcError};
pub use settings::{
    quote_identifier, quote_literal, validate_slot_name, PostgresTableKey,
    PostgresTransactionConfig, SlotIdentity, SlotOwnership,
};
pub use transaction::{row_element, Changes, Decoded, Transactions};
pub type TableKeys = BTreeMap<(String, String), Vec<String>>;

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
        .get(&SourceProgressKey::Source(source.clone()))
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
            ensure_binding(position.binding, binding)?;
            Ok(position.commit_lsn)
        })
        .transpose()
}

/// Reports a committed cursor whose binding no longer matches as
/// [`PostgresCdcError::SchemaChanged`].
pub fn ensure_binding(committed: [u8; 32], current: [u8; 32]) -> Result<()> {
    if committed == current {
        return Ok(());
    }
    Err(PostgresCdcError::SchemaChanged(
        "the publication, slot, selected tables, key columns, replica identity or row filters differ from the committed cursor".into(),
    )
    .into())
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
    validate_managed_slot(config.managed_prefix()?, &slot)?;
    Ok(config.with_active_slot(slot, position.commit_lsn))
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
    connect_with_options(
        ConnectionOptions {
            host: &config.host,
            port: config.port,
            database: &config.database,
            user: &config.user,
            password: &config.password,
            ssl_mode: config.ssl_mode,
            tls_ca_pem: config.tls_ca_pem.as_deref(),
            max_protocol_bytes: config.max_protocol_bytes,
            timeout: config.timeout(),
        },
        true,
    )
    .await
}

/// The SSLRequest startup packet: length 8, request code 80877103.
const SSL_REQUEST: [u8; 8] = [0, 0, 0, 8, 4, 210, 22, 47];

/// Oldest supported server: `wal_status` and `max_slot_wal_keep_size`
/// appeared in PostgreSQL 13.
pub const MINIMUM_SERVER_VERSION: u32 = 130_000;
/// Row filters and column lists appeared in PostgreSQL 15.
const ROW_FILTER_SERVER_VERSION: u32 = 150_000;

pub async fn connect_with_options(
    settings: ConnectionOptions<'_>,
    replication: bool,
) -> Result<Connection> {
    let addresses = tokio::net::lookup_host((settings.host, settings.port))
        .await
        .map_err(|source| error::AddressUnresolved {
            host: settings.host.to_owned(),
            source,
        })?
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(error::AddressUnresolved {
            host: settings.host.to_owned(),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "no addresses"),
        }
        .into());
    }
    let mut stream = TcpStream::connect(addresses.as_slice()).await?;
    stream.set_nodelay(true)?;
    let stream: Box<dyn ReplicationIo> = if settings.ssl_mode == SslMode::Disable {
        Box::new(stream)
    } else {
        stream.write_all(&SSL_REQUEST).await?;
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
    let publication = quote_literal(&config.publication);
    let rows = connection.query_rows(
        format!("SELECT schemaname, tablename FROM pg_publication_tables WHERE pubname = {publication}"),
        config.max_protocol_bytes,
    ).await?;
    let mut tables = BTreeSet::new();
    for row in rows {
        tables.insert((field(&row, 0)?, field(&row, 1)?));
    }
    anyhow::ensure!(!tables.is_empty(), "PostgreSQL publication has no tables");
    if !config.tables.is_empty() {
        let selected: BTreeSet<_> = config
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
    for key in &config.table_keys {
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

/// The server's `server_version_num`, rejecting servers older than
/// [`MINIMUM_SERVER_VERSION`].
pub async fn server_version(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
) -> Result<u32> {
    let rows = connection
        .query_rows("SHOW server_version_num".into(), config.max_protocol_bytes)
        .await?;
    let [row] = rows.as_slice() else {
        anyhow::bail!("missing PostgreSQL version");
    };
    let version = field(row, 0)?.parse::<u32>()?;
    if version < MINIMUM_SERVER_VERSION {
        return Err(PostgresCdcError::Unsupported(format!(
            "server version {version} is older than PostgreSQL 13"
        ))
        .into());
    }
    Ok(version)
}

/// Computes the source binding: the facts a committed cursor depends on.
///
/// The binding covers the server and database identity, the slot and
/// publication, the publication's operations, the selected tables with their
/// key columns, key column types and replica identity, and any row filters.
/// Non-key columns are deliberately excluded, so adding, dropping or retyping
/// a non-key column does not invalidate committed cursors (decision D-7). A
/// change to anything bound is reported as [`PostgresCdcError::SchemaChanged`]
/// when a cursor is checked.
pub async fn source_binding(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
    source: &ComponentId,
    stream: &StreamId,
) -> Result<(TableKeys, [u8; 32])> {
    let system = connection.identify_system().await?;
    let system_id = system
        .get("systemid")
        .context("missing PostgreSQL system identifier")?
        .clone();
    let database = system
        .get("dbname")
        .context("missing PostgreSQL database identity")?
        .clone();
    anyhow::ensure!(
        database == config.database,
        "PostgreSQL database identity changed"
    );
    let version = server_version(connection, config).await?;
    let publication = quote_literal(&config.publication);
    let flags = connection.query_rows(
        format!("SELECT pubinsert::text, pubupdate::text, pubdelete::text, pubtruncate::text, puballtables::text, pubviaroot::text FROM pg_publication WHERE pubname = {publication}"),
        config.max_protocol_bytes,
    ).await?;
    let [flags] = flags.as_slice() else {
        return Err(PostgresCdcError::Unsupported(format!(
            "publication {} does not exist",
            config.publication
        ))
        .into());
    };
    let flags = (0..6)
        .map(|index| field(flags, index).map(|value| value == "true"))
        .collect::<Result<Vec<_>>>()?;
    if !(flags[0] && flags[1] && flags[2]) {
        return Err(PostgresCdcError::Unsupported(
            "the publication must publish inserts, updates and deletes".into(),
        )
        .into());
    }
    // Decision D-9: a TRUNCATE cannot be represented as row changes, so a
    // publication that publishes it is refused before any cursor is used.
    if flags[3] {
        return Err(PostgresCdcError::Unsupported(format!(
            "publication {} publishes TRUNCATE; run ALTER PUBLICATION {} SET (publish = 'insert, update, delete')",
            config.publication,
            quote_identifier(&config.publication)
        ))
        .into());
    }
    let keys = table_keys(connection, config).await?;
    let selected = keys
        .keys()
        .map(|(namespace, table)| {
            format!("({}, {})", quote_literal(namespace), quote_literal(table))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let filters = if version >= ROW_FILTER_SERVER_VERSION {
        connection.query_rows(
            format!("SELECT schemaname, tablename, coalesce(rowfilter, '') FROM pg_publication_tables WHERE pubname = {publication} AND (schemaname, tablename) IN ({selected}) ORDER BY schemaname, tablename"),
            config.max_protocol_bytes,
        ).await?
        .iter()
        .map(|row| Ok((field(row, 0)?, field(row, 1)?, field(row, 2)?)))
        .collect::<Result<Vec<_>>>()?
    } else {
        Vec::new()
    };
    let columns = connection.query_rows(
        format!("SELECT n.nspname, c.relname, c.relreplident::text, a.attname, a.atttypid::text, a.atttypmod::text FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace JOIN pg_attribute a ON a.attrelid = c.oid WHERE (n.nspname, c.relname) IN ({selected}) AND a.attnum > 0 AND NOT a.attisdropped ORDER BY n.nspname, c.relname, a.attnum"),
        config.max_protocol_bytes,
    ).await?;
    let mut bound = BTreeMap::<(String, String), (String, Vec<(String, String, String)>)>::new();
    for row in &columns {
        let table = (field(row, 0)?, field(row, 1)?);
        let column = field(row, 3)?;
        let Some(key) = keys.get(&table) else {
            continue;
        };
        let entry = bound
            .entry(table)
            .or_insert_with(|| (String::new(), Vec::new()));
        entry.0 = field(row, 2)?;
        if key.contains(&column) {
            entry.1.push((column, field(row, 4)?, field(row, 5)?));
        }
    }
    for (table, key) in &keys {
        let found = bound.get(table).map_or(0, |(_, columns)| columns.len());
        if found != key.len() {
            return Err(PostgresCdcError::SchemaChanged(format!(
                "key columns of {}.{} are missing",
                table.0, table.1
            ))
            .into());
        }
    }
    let binding = Sha256::digest(serde_json::to_vec(&(
        2u32,
        system_id,
        database,
        config.slot_name(),
        &config.publication,
        &flags,
        keys.iter().collect::<Vec<_>>(),
        bound.iter().collect::<Vec<_>>(),
        filters,
        source.as_str(),
        stream.as_str(),
    ))?)
    .into();
    Ok((keys, binding))
}

/// Checks the slot this configuration streams from: it must exist in this
/// database, be persistent, and still retain its WAL (decision D-8: a finite
/// `max_slot_wal_keep_size` is allowed, and an invalidated slot is reported
/// as [`PostgresCdcError::PositionUnavailable`]).
pub async fn validate_retention(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
) -> Result<()> {
    let rows = connection
        .query_rows(
            format!(
                "SELECT current_setting('max_slot_wal_keep_size', true), temporary::text, coalesce(wal_status, ''), plugin \
                 FROM pg_replication_slots WHERE slot_name = {} AND database = current_database()",
                quote_literal(config.slot_name()),
            ),
            config.max_protocol_bytes,
        )
        .await?;
    let [row] = rows.as_slice() else {
        return Err(PostgresCdcError::PositionUnavailable(format!(
            "replication slot {} does not exist in database {}",
            config.slot_name(),
            config.database
        ))
        .into());
    };
    if field(row, 3)? != "pgoutput" {
        return Err(PostgresCdcError::Unsupported(format!(
            "replication slot {} does not use pgoutput",
            config.slot_name()
        ))
        .into());
    }
    if field(row, 1)? != "false" {
        return Err(PostgresCdcError::Unsupported(
            "native PostgreSQL requires a persistent replication slot".into(),
        )
        .into());
    }
    if field(row, 2)? == "lost" {
        return Err(PostgresCdcError::PositionUnavailable(format!(
            "replication slot {} was invalidated and its WAL removed (wal_status = lost)",
            config.slot_name()
        ))
        .into());
    }
    let keep = row.first().cloned().flatten();
    if keep.as_deref() != Some(b"-1") {
        log::warn!(
            "PostgreSQL max_slot_wal_keep_size is finite; slot {} is invalidated if the source falls that far behind",
            config.slot_name()
        );
    }
    Ok(())
}

/// Whether `slot` exists in the configured database.
pub async fn slot_exists(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
    slot: &str,
) -> Result<bool> {
    let rows = connection
        .query_rows(
            format!(
                "SELECT 1 FROM pg_replication_slots WHERE slot_name = {} AND database = current_database()",
                quote_literal(slot)
            ),
            config.max_protocol_bytes,
        )
        .await?;
    Ok(!rows.is_empty())
}

/// Ensures the slot this configuration streams from exists.
///
/// A managed configuration creates its derived slot when `may_create` is
/// true; the derived name is owned by construction, so a slot of that name
/// left by an earlier attempt is reused. An external slot is never created.
/// Returns whether the slot was created by this call.
pub async fn ensure_slot(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
    may_create: bool,
) -> Result<bool> {
    let slot = config.slot_name().to_owned();
    if slot_exists(connection, config, &slot).await? {
        return Ok(false);
    }
    if !(may_create && config.creates_slot()) {
        return Err(PostgresCdcError::PositionUnavailable(format!(
            "replication slot {slot} does not exist{}",
            if config.slot_ownership == SlotOwnership::Managed {
                " and the committed cursor cannot be recovered without it"
            } else {
                "; create it or use managed slot ownership"
            }
        ))
        .into());
    }
    validate_slot_name(&slot)?;
    match connection
        .query_rows(
            format!("CREATE_REPLICATION_SLOT {slot} LOGICAL pgoutput"),
            config.max_protocol_bytes,
        )
        .await
    {
        Ok(_) => {
            log::info!("Created managed PostgreSQL replication slot {slot}");
            Ok(true)
        }
        Err(error) if sqlstate(&error) == Some("42710") => Ok(false),
        Err(error) => Err(error.context(format!("cannot create replication slot {slot}"))),
    }
}

/// Drops every slot a managed configuration owns in its database: its live
/// slot and any coordinated snapshot slots. External slots are never
/// touched. Idempotent; refuses while an owned slot is active.
pub async fn drop_owned_slots(config: &PostgresTransactionConfig) -> Result<Vec<String>> {
    let Some(identity) = config.slot_identity().cloned() else {
        return Ok(Vec::new());
    };
    let mut connection = connect(config).await?;
    let pattern = format!("{}_", identity.prefix);
    let rows = connection
        .query_rows(
            format!(
                "SELECT slot_name, active::text FROM pg_replication_slots WHERE database = current_database() AND left(slot_name, {}) = {}",
                pattern.len(),
                quote_literal(&pattern)
            ),
            config.max_protocol_bytes,
        )
        .await?;
    let mut owned = Vec::new();
    for row in &rows {
        let slot = field(row, 0)?;
        if !identity.owns(&slot) {
            continue;
        }
        if field(row, 1)? == "true" {
            return Err(PostgresCdcError::StateInUse(slot).into());
        }
        owned.push(slot);
    }
    for slot in &owned {
        match connection
            .query_rows(
                format!("DROP_REPLICATION_SLOT {slot}"),
                config.max_protocol_bytes,
            )
            .await
        {
            Ok(_) => log::info!("Dropped managed PostgreSQL replication slot {slot}"),
            Err(error) if sqlstate(&error) == Some("42704") => {}
            Err(error) if sqlstate(&error) == Some("55006") => {
                return Err(PostgresCdcError::StateInUse(slot.clone()).into())
            }
            Err(error) => return Err(error.context(format!("cannot drop replication slot {slot}"))),
        }
    }
    let _ = connection.close().await;
    Ok(owned)
}

/// The slot's retained (`restart_lsn`) and confirmed positions.
pub async fn slot_positions(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
) -> Result<(u64, u64)> {
    let slot = connection
        .get_replication_slot_info(config.slot_name())
        .await?;
    if slot.output_plugin != "pgoutput" {
        return Err(PostgresCdcError::Unsupported(format!(
            "replication slot {} does not use pgoutput",
            config.slot_name()
        ))
        .into());
    }
    let retained = parse_lsn(slot.restart_lsn.as_deref().ok_or_else(|| {
        PostgresCdcError::PositionUnavailable(format!(
            "replication slot {} retains no WAL",
            config.slot_name()
        ))
    })?)?;
    Ok((retained, parse_lsn(&slot.consistent_point)?))
}

/// Requires the slot to still retain `resume` and to not have confirmed
/// beyond `feedback`.
pub fn ensure_retained(retained: u64, confirmed: u64, resume: u64, feedback: u64) -> Result<()> {
    if retained <= resume && confirmed <= feedback {
        return Ok(());
    }
    Err(PostgresCdcError::PositionUnavailable(format!(
        "the slot retains WAL from {} and confirmed {}, but the cursor resumes at {} with feedback {}",
        format_lsn(retained),
        format_lsn(confirmed),
        format_lsn(resume),
        format_lsn(feedback)
    ))
    .into())
}

/// Starts pgoutput streaming at `resume`.
pub async fn start_streaming(
    connection: &mut Connection,
    config: &PostgresTransactionConfig,
    resume: u64,
) -> Result<()> {
    connection
        .start_replication(
            config.slot_name(),
            Some(resume),
            std::collections::HashMap::from([
                ("proto_version".into(), "1".into()),
                (
                    "publication_names".into(),
                    quote_identifier(&config.publication).replace('\'', "''"),
                ),
            ]),
        )
        .await
        .map_err(|error| match sqlstate(&error) {
            Some("55000") => anyhow::Error::new(PostgresCdcError::PositionUnavailable(format!(
                "replication slot {} can no longer stream",
                config.slot_name()
            )))
            .context(error),
            Some("42704") => anyhow::Error::new(PostgresCdcError::PositionUnavailable(format!(
                "replication slot {} does not exist",
                config.slot_name()
            ))),
            _ => error,
        })
}

/// The SQLSTATE of a server error anywhere in `error`'s chain.
pub fn sqlstate(error: &anyhow::Error) -> Option<&str> {
    error.chain().find_map(|cause| {
        cause
            .downcast_ref::<crate::protocol::ErrorResponse>()
            .map(|error| error.code.as_str())
    })
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
