// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Native whole-transaction PostgreSQL replication, preserving the legacy per-row contract.

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::interface::StorageDurability;
use drasi_lib::{
    computation::v1::{
        ComponentDescriptor, ComponentId, ComponentRecovery, ComputationComponent, EnvelopeSource,
        OutputEnvelope, PipeRequirements, PortDescriptor, PortDirection, PortId,
        QuerySourceProgress, SourceProgressKey, SourceProgressReader, SourceProgressSnapshot,
        SourceProgressUpdates, SourceTransactionCodec, SourceTransactionError,
        SourceTransactionLimits, StreamId,
    },
    context::workers::{join_owned_worker, spawn_owned_worker, WorkerCompletion},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    num::{NonZeroU64, NonZeroUsize},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    sync::{mpsc, watch, RwLock},
    task::JoinHandle,
    time::Instant,
};
use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};

use crate::{
    config::{PostgresSourceConfig, SslMode},
    connection::{parse_lsn, ReplicationConnection},
    protocol::BackendMessage,
    types::StandbyStatusUpdate,
};

mod factory;
mod snapshot;
#[cfg(test)]
mod tests;
mod transaction;
pub use factory::PostgresTransactionSourceFactory;
pub use snapshot::PostgresSnapshot;
use transaction::{TableKeys, Transactions};

/// The slot must already exist and belong exclusively to this source.
/// `start_lsn` is used only before the owner has committed its first transaction.
/// Snapshot handover is not implicit: provision an appropriate initial boundary.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PostgresTransactionConfig {
    pub connection: PostgresSourceConfig,
    /// Optional additional CA certificate; hostname verification remains required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tls_ca_pem: Option<String>,
    pub start_lsn: String,
    pub transactions: SourceTransactionLimits,
    /// Bounds one wire frame, catalog result storage, and cached relation metadata
    /// independently. This is additional to the transaction payload byte limit.
    pub max_protocol_bytes: NonZeroUsize,
    pub io_timeout_ms: NonZeroU64,
    pub feedback_interval_ms: NonZeroU64,
}

impl PostgresTransactionConfig {
    fn validate(&self) -> Result<()> {
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
    fn timeout(&self) -> Duration {
        Duration::from_millis(self.io_timeout_ms.get())
    }
}

trait ReplicationIo: AsyncRead + AsyncWrite + Unpin + Send + Sync {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> ReplicationIo for T {}
type Connection = ReplicationConnection<Box<dyn ReplicationIo>>;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Position {
    version: u32,
    binding: [u8; 32],
    commit_lsn: u64,
}

/// Produces one complete committed transaction per envelope, retaining PostgreSQL
/// WAL until its one bound atomic consumer commits. No per-row acknowledgement.
pub struct PostgresTransactionSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    config: PostgresTransactionConfig,
    progress: SourceProgressReader,
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    shutdown: watch::Sender<bool>,
    worker: RwLock<Option<JoinHandle<Result<()>>>>,
    cleanup_required: bool,
    transport_sequence: Arc<AtomicU64>,
    snapshot: Option<Arc<PostgresSnapshot>>,
}

impl PostgresTransactionSource {
    pub fn new(
        id: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: impl Into<SourceProgressReader>,
    ) -> Result<Self> {
        config.validate()?;
        let descriptor = Self::describe(id)?;
        Ok(Self {
            descriptor,
            stream,
            config,
            progress: progress.into(),
            receiver: None,
            shutdown: watch::channel(false).0,
            worker: RwLock::new(None),
            cleanup_required: false,
            transport_sequence: Arc::new(AtomicU64::new(0)),
            snapshot: None,
        })
    }

    /// Creates a paired query bootstrap provider. This mode manages dedicated
    /// UUID-suffixed slots; the configured slot name is only their prefix.
    pub fn coordinated(
        id: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: impl Into<SourceProgressReader>,
    ) -> Result<(Self, Arc<PostgresSnapshot>)> {
        let progress = progress.into();
        let mut source = Self::new(id.clone(), stream.clone(), config.clone(), progress.clone())?;
        let snapshot = Arc::new(PostgresSnapshot::new(id, stream, config, progress)?);
        source.snapshot = Some(snapshot.clone());
        Ok((source, snapshot))
    }

    pub fn describe(id: ComponentId) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(
            id,
            vec![PortDescriptor::new(
                PortId::try_new("out")?,
                PortDirection::Output,
                SourceTransactionCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?)
    }

    async fn prepare(&self, snapshot: Arc<SourceProgressSnapshot>) -> Result<Worker> {
        anyhow::ensure!(
            snapshot.persistent,
            "PostgreSQL transactions require persistent owner progress"
        );
        let config = match &self.snapshot {
            Some(provider) => provider.live_config(&snapshot)?,
            None => self.config.clone(),
        };
        let progress = self.progress.subscribe()?;
        let mut connection = connect(&config).await?;
        validate_retention(&mut connection, &config).await?;
        let (keys, binding) =
            source_binding(&mut connection, &config, self.descriptor.id(), &self.stream).await?;
        let initial = parse_lsn(&config.start_lsn)?;
        let resume = checkpoint(&snapshot, self.descriptor.id(), binding)?.unwrap_or(initial);
        self.transport_sequence.fetch_max(
            snapshot
                .transport_sequences
                .get(&self.stream)
                .copied()
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        let slot = connection
            .get_replication_slot_info(&config.connection.slot_name)
            .await?;
        anyhow::ensure!(
            slot.output_plugin == "pgoutput",
            "replication slot is not pgoutput"
        );
        let retained = parse_lsn(
            slot.restart_lsn
                .as_deref()
                .context("slot has no retained WAL")?,
        )?;
        let confirmed = parse_lsn(&slot.consistent_point)?;
        anyhow::ensure!(
            retained <= resume && confirmed <= resume,
            "requested PostgreSQL cursor is no longer available; explicit recovery is required"
        );
        let publication = format!(
            "\"{}\"",
            config.connection.publication_name.replace('"', "\"\"")
        );
        connection
            .start_replication(
                &config.connection.slot_name,
                Some(resume),
                HashMap::from([
                    ("proto_version".into(), "1".into()),
                    ("publication_names".into(), publication.replace('\'', "''")),
                ]),
            )
            .await?;
        Ok(Worker {
            connection,
            transactions: Transactions {
                decoder: crate::decoder::PgOutputDecoder::strict(config.max_protocol_bytes),
                pending: None,
                keys,
                source: self.descriptor.id().clone(),
                stream: self.stream.clone(),
                limits: config.transactions,
                binding,
                resume,
                transport_sequence: self.transport_sequence.clone(),
                filtered: !config.connection.tables.is_empty(),
            },
            progress,
            reset_generation: snapshot.reset_generation,
            confirmed: resume,
            next_feedback: Instant::now(),
            config,
        })
    }

    async fn join(&mut self) -> Result<()> {
        match join_owned_worker(self.worker.get_mut(), self.config.timeout()).await? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Cancelled | WorkerCompletion::Absent => Ok(()),
        }
    }
}

#[async_trait]
impl ComputationComponent for PostgresTransactionSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    fn recovery_contract(&self) -> ComponentRecovery {
        ComponentRecovery::admitted(StorageDurability::LOCAL_PROCESS_RESTART)
            .replay_until(self.progress.component_id().clone())
    }
    fn configuration(&self) -> Result<serde_json::Value> {
        let mut value =
            serde_json::json!({"stream": self.stream.as_str(), "settings": self.config});
        if self.snapshot.is_some() {
            value["coordinated_snapshot"] = true.into();
        }
        Ok(value)
    }
    async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required,
            "PostgreSQL source requires awaited cleanup before restart"
        );
        self.cleanup_required = true;
        let progress = self.progress.wait_ready().await?;
        let mut worker = tokio::time::timeout(self.config.timeout(), self.prepare(progress))
            .await
            .context("PostgreSQL startup timed out")??;
        let (sender, receiver) = mpsc::channel(1);
        self.receiver = Some(receiver);
        self.shutdown.send_replace(false);
        let mut shutdown = self.shutdown.subscribe();
        spawn_owned_worker(&self.worker, async move {
            let result = tokio::select! {
                biased;
                _ = shutdown.wait_for(|stopped| *stopped) => Ok(()),
                result = worker.run(sender) => result,
            };
            if let Err(error) = &result {
                log::error!("Native PostgreSQL replication stopped: {error:#}");
            }
            result
        })
        .await?;
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        self.shutdown.send_replace(true);
        self.join().await?;
        self.receiver = None;
        self.cleanup_required = false;
        Ok(())
    }
}

#[async_trait]
impl EnvelopeSource for PostgresTransactionSource {
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        Some(self.progress.clone())
    }
    fn recovery_progress(&self) -> Option<Arc<QuerySourceProgress>> {
        self.progress.local_owner()
    }
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let receiver = self
            .receiver
            .as_mut()
            .context("PostgreSQL source is not running")?;
        if let Some(envelope) = receiver.recv().await {
            return Ok(Some(envelope));
        }
        self.join().await?;
        anyhow::bail!("PostgreSQL replication ended; stop and restart the source")
    }
}

impl Drop for PostgresTransactionSource {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        if let Some(worker) = self.worker.get_mut() {
            worker.abort();
            log::warn!("Native PostgreSQL source dropped without awaited worker cleanup");
        }
    }
}

struct Worker {
    connection: Connection,
    transactions: Transactions,
    progress: Box<dyn SourceProgressUpdates>,
    reset_generation: u64,
    confirmed: u64,
    next_feedback: Instant,
    config: PostgresTransactionConfig,
}

impl Worker {
    async fn feedback(&mut self) -> Result<()> {
        let snapshot = self.progress.snapshot()?;
        anyhow::ensure!(
            snapshot.failure.is_none(),
            "PostgreSQL consumer progress is fenced"
        );
        anyhow::ensure!(
            snapshot.reset_generation == self.reset_generation,
            "PostgreSQL consumer was reset; explicit source reconstruction is required"
        );
        if snapshot.ready {
            anyhow::ensure!(
                snapshot.persistent,
                "PostgreSQL consumer lost persistent progress"
            );
            if let Some(next) = checkpoint(
                &snapshot,
                &self.transactions.source,
                self.transactions.binding,
            )? {
                anyhow::ensure!(
                    next >= self.confirmed && next <= self.transactions.resume,
                    "PostgreSQL committed cursor moved outside the admitted range"
                );
                self.confirmed = next;
            }
        }
        let io_deadline = Instant::now() + self.config.timeout();
        let assembly_deadline = self
            .transactions
            .pending
            .as_ref()
            .map(|(_, builder)| builder.deadline());
        let deadline = assembly_deadline.map_or(io_deadline, |deadline| deadline.min(io_deadline));
        let result = tokio::time::timeout_at(
            deadline,
            self.connection.send_standby_status(StandbyStatusUpdate {
                write_lsn: self.confirmed,
                flush_lsn: self.confirmed,
                apply_lsn: self.confirmed,
                reply_requested: false,
            }),
        )
        .await;
        if result.is_err() && assembly_deadline.is_some_and(|deadline| deadline <= Instant::now()) {
            return Err(SourceTransactionError::Deadline.into());
        }
        result.context("PostgreSQL feedback timed out")??;
        self.next_feedback =
            Instant::now() + Duration::from_millis(self.config.feedback_interval_ms.get());
        Ok(())
    }

    async fn run(&mut self, sender: mpsc::Sender<OutputEnvelope>) -> Result<()> {
        let mut pending = None;
        loop {
            if let Some((_, builder)) = &mut self.transactions.pending {
                builder.check_deadline()?;
            }
            if pending.is_some() {
                tokio::select! {
                    permit = sender.reserve() => {
                        permit.context("PostgreSQL output closed")?
                            .send(pending.take().context("pending PostgreSQL transaction")?);
                    }
                    _ = tokio::time::sleep_until(self.next_feedback) => self.feedback().await?,
                    changed = self.progress.changed() => {
                        changed?;
                        self.feedback().await?;
                    }
                }
                continue;
            }
            let deadline = self
                .transactions
                .pending
                .as_ref()
                .map(|(_, builder)| builder.deadline());
            tokio::select! {
                biased;
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Err(SourceTransactionError::Deadline.into()),
                _ = tokio::time::sleep_until(self.next_feedback) => self.feedback().await?,
                changed = self.progress.changed() => {
                    changed?;
                    self.feedback().await?;
                }
                message = self.connection.read_replication_message() => {
                    match message? {
                        BackendMessage::CopyData(data) => match data.first() {
                            Some(b'w') => {
                                anyhow::ensure!(data.len() > 25, "truncated PostgreSQL XLogData");
                                pending = self.transactions.decode(&data[25..])?;
                            }
                            Some(b'k') => {
                                anyhow::ensure!(data.len() == 18 && data[17] <= 1, "invalid PostgreSQL keepalive");
                                if data[17] == 1 { self.feedback().await?; }
                            }
                            _ => anyhow::bail!("unknown PostgreSQL CopyData frame"),
                        },
                        BackendMessage::NoticeResponse(notice) => log::info!("PostgreSQL: {}", notice.message),
                        BackendMessage::ErrorResponse(error) => anyhow::bail!("PostgreSQL replication failed: {}", error.message),
                        message => anyhow::bail!("unexpected PostgreSQL replication message: {message:?}"),
                    }
                }
            }
        }
    }
}

fn checkpoint(
    snapshot: &SourceProgressSnapshot,
    source: &ComponentId,
    binding: [u8; 32],
) -> Result<Option<u64>> {
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
        position.version == 1
            && position.binding == binding
            && position.commit_lsn == checkpoint.sequence,
        "PostgreSQL checkpoint source binding or sequence mismatch"
    );
    Ok(Some(position.commit_lsn))
}

async fn connect(config: &PostgresTransactionConfig) -> Result<Connection> {
    let settings = &config.connection;
    let mut stream = TcpStream::connect((settings.host.as_str(), settings.port)).await?;
    stream.set_nodelay(true)?;
    let stream: Box<dyn ReplicationIo> = if settings.ssl_mode == SslMode::Disable {
        Box::new(stream)
    } else {
        stream.write_all(&[0, 0, 0, 8, 4, 210, 22, 47]).await?;
        stream.flush().await?;
        let response = stream.read_u8().await?;
        match response {
            b'S' => {
                let mut tls = native_tls::TlsConnector::builder();
                if let Some(pem) = &config.tls_ca_pem {
                    tls.add_root_certificate(
                        native_tls::Certificate::from_pem(pem.as_bytes())
                            .context("invalid PostgreSQL CA certificate")?,
                    );
                }
                let mut connector = postgres_native_tls::MakeTlsConnector::new(tls.build()?);
                let connector = <postgres_native_tls::MakeTlsConnector as MakeTlsConnect<
                    TcpStream,
                >>::make_tls_connect(
                    &mut connector, &settings.host
                )?;
                Box::new(connector.connect(stream).await?)
            }
            b'N' if settings.ssl_mode == SslMode::Prefer => Box::new(stream),
            b'N' => anyhow::bail!("PostgreSQL server refused required TLS"),
            _ => anyhow::bail!("invalid PostgreSQL TLS negotiation response"),
        }
    };
    let mut connection = ReplicationConnection::from_stream(
        stream,
        &settings.database,
        &settings.user,
        &settings.password,
        Some(config.max_protocol_bytes),
    )
    .await?;
    connection.set_partial_read_timeout(config.timeout());
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

async fn source_binding(
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

async fn validate_retention(
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

fn field(row: &[Option<Bytes>], index: usize) -> Result<String> {
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
