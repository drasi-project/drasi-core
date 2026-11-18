// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! The native MySQL replication engine shared by the native source plugin
//! and the in-process wrapper.
//!
//! One activation owns one replication connection and one worker task. The
//! worker decodes binlog events, assembles transactions (or rows), emits
//! them through a bounded channel and validates the host's committed
//! progress. It also implements:
//!
//! - **pause** at a transaction boundary: nothing is decoded or emitted; the
//!   socket keeps being drained into a bounded buffer (heartbeats discarded)
//!   so the server does not time the dump thread out, and the connection is
//!   closed deliberately if the buffer fills;
//! - **reconnection** after transport failures (including a dead peer
//!   detected by the heartbeat watchdog), resuming at the end of the last
//!   transaction it emitted, with jittered exponential backoff bounded by
//!   `reconnectBudgetMs`; permanent [`SourceFailure`]s never reconnect;
//! - the **DDL policy** of [`super::ddl`]: schema statements on selected
//!   tables make the worker re-read their catalog, and only a change to the
//!   facts the cursor is bound to fences the source.

use super::{
    binlog::{Binlog, HistoryMode, Position, Start},
    catalog::{self, field, Profile, Tables},
    connection::Connection,
    errors::{self, SourceFailure},
    progress::{ConsumerBinding, ConsumerUpdates, Positions, Starting},
    rows, MySqlConfig, MySqlOutput, MySqlRetention, MySqlStartPosition,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use drasi_computation_contracts::{
    lifecycle::{CancellationToken, StopMode},
    pause::{PauseControl, PauseGate, PauseState},
    reconnect::BackoffPolicy,
    workers::{
        cancel_owned_worker, join_owned_worker, spawn_owned_worker, WorkerCleanupError,
        WorkerCompletion,
    },
    ComponentDescriptor, ComponentId, ConsumerProgressSnapshot, GraphChangeCodec,
    InputPositionPolicy, OperationFailures, OutputEnvelope, PipeRequirements, PortDescriptor,
    PortDirection, PortId, SourceProgressKey, SourceProgressSnapshot, SourceProgressUpdates,
    SourceProgressView, SourceTransactionCodec, SourceTransactionError, StreamId,
};
use drasi_core_types::interface::StorageDurability;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch, RwLock},
    task::JoinHandle,
    time::Instant,
};

/// First reconnect backoff ceiling; doubles per attempt.
const RECONNECT_BASE: Duration = Duration::from_millis(250);
/// Largest reconnect backoff ceiling.
const RECONNECT_MAX: Duration = Duration::from_secs(30);
/// Missed heartbeats after which the peer is presumed dead.
const DEAD_PEER_HEARTBEATS: u32 = 3;
/// `HEARTBEAT_EVENT`, discarded while paused.
const HEARTBEAT_EVENT: u8 = 27;

/// What the worker should do when the host stops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Shutdown {
    Running,
    Drain,
    Abort,
}

/// Readiness changes reported to the host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceStatus {
    /// Replication is connected and streaming (or paused while connected).
    Ready,
    /// A transport failure interrupted replication; reconnecting.
    Reconnecting(String),
    /// Replication stopped with a failure that `next` reports.
    Failed(String),
}

/// Receives [`SourceStatus`] changes; the plugin forwards them to its
/// `ControlSender`.
pub type StatusSink = Arc<dyn Fn(SourceStatus) + Send + Sync>;

/// Per-activation inputs from the host.
#[derive(Default)]
pub struct Activation<'a> {
    /// Cancels the start.
    pub cancellation: Option<&'a CancellationToken>,
    /// Overall start deadline. Readiness waits (which may include a
    /// coordinated snapshot) are bounded only by it; connection setup is
    /// additionally bounded by `ioTimeoutMs`.
    pub deadline: Option<Instant>,
    /// Host-supplied `(user, password)` replacing the configured ones.
    pub credentials: Option<(String, String)>,
}

/// The binding scope a cursor is bound to.
#[derive(Clone, Debug)]
enum Scope {
    /// Query-owned (or untracked) progress, with its owner.
    Single(Option<(String, String)>),
    /// A fixed consumer set of whole transactions.
    Shared(String),
    /// A fixed consumer set of rows.
    Rows(String),
}

impl Scope {
    fn binding(
        &self,
        config: &MySqlConfig,
        server: &str,
        tables: &Tables,
        source: &ComponentId,
        stream: &StreamId,
        positional: bool,
    ) -> Result<[u8; 32]> {
        let binding = match self {
            Self::Single(owner) => binding(
                config,
                server,
                tables,
                source,
                stream,
                (
                    "mysql-native-v2",
                    owner
                        .as_ref()
                        .map(|(graph, owner)| (graph.as_str(), owner.as_str())),
                ),
                positional,
            )?,
            Self::Shared(graph) => binding(
                config,
                server,
                tables,
                source,
                stream,
                ("mysql-native-consumers-v2", Some((graph, source.as_str()))),
                positional,
            )?,
            Self::Rows(graph) => binding(
                config,
                server,
                tables,
                source,
                stream,
                (
                    "mysql-native-consumer-rows-v2",
                    Some((graph, source.as_str())),
                ),
                positional,
            )?,
        };
        Ok(if positional {
            Sha256::digest(serde_json::to_vec(&("mysql-positional-rows-v2", binding))?).into()
        } else {
            binding
        })
    }
}

/// Where a replication connection starts reading.
#[derive(Clone, Debug)]
enum StartPoint {
    /// A file offset; `anchor` is the transaction re-read and compared there.
    Binlog {
        file: String,
        offset: u32,
        anchor: Option<Position>,
    },
    /// After a GTID set (before the first transaction is decoded).
    Gtid(String),
    /// A coordinated snapshot boundary whose file prefix is re-hashed.
    History {
        file: String,
        boundary: u32,
        digest: [u8; 32],
        confirmed: u64,
    },
}

/// Raised inside the worker when a paused connection was closed
/// deliberately and must be reopened on resume.
#[derive(Debug)]
struct PausedClose;

impl std::fmt::Display for PausedClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MySQL replication connection was closed while paused")
    }
}

impl std::error::Error for PausedClose {}

pub struct MySqlSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    config: MySqlConfig,
    credentials: Option<(String, String)>,
    progress: Option<SourceProgressView>,
    consumers: Option<ConsumerBinding>,
    position_policy: Option<Arc<InputPositionPolicy>>,
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    shutdown: watch::Sender<Shutdown>,
    pause: PauseControl,
    worker: RwLock<Option<JoinHandle<Result<()>>>>,
    cleanup_required: bool,
    sequence: Arc<AtomicU64>,
    reconnect_budget_ms: Arc<AtomicU64>,
    status: Option<StatusSink>,
    snapshot: Option<Arc<super::MySqlSnapshot>>,
    consumer_snapshots: BTreeMap<ComponentId, Arc<super::MySqlSnapshot>>,
    published_snapshot: bool,
    rows: Option<rows::Options>,
    schema: Arc<super::schema::Publication>,
    positional: bool,
    disposed: bool,
    /// Least quiet time before a row source commits a progress record.
    idle_progress: Duration,
}

impl MySqlSource {
    pub fn with_positional_metadata(mut self) -> Result<Self> {
        anyhow::ensure!(
            !self.cleanup_required
                && self.config.output == MySqlOutput::Changes
                && !matches!(self.config.start, MySqlStartPosition::Snapshot { .. })
                && self.progress.is_none()
                && !self.config.table_keys.is_empty(),
            "positional MySQL requires ordinary rows, explicit @N keys and no coordinated snapshot"
        );
        for keys in &self.config.table_keys {
            catalog::positional_keys(&self.config, &keys.table, 4096)?;
        }
        for table in &self.config.tables {
            catalog::positional_keys(&self.config, table, 4096)?;
        }
        self.positional = true;
        Ok(self)
    }

    /// Independent plugin libraries share the query's committed cursor, not a
    /// Rust bootstrap object across the ABI. Rust paired constructors stay strict.
    pub fn with_published_bootstrap(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: Option<SourceProgressView>,
        consumers: Option<ConsumerBinding>,
    ) -> Result<Self> {
        anyhow::ensure!(
            matches!(config.start, MySqlStartPosition::Snapshot { .. }),
            "published MySQL bootstrap requires explicit snapshot mode"
        );
        let mut source = Self::build(id, stream, config, progress, None, consumers, None)?;
        source.published_snapshot = true;
        Ok(source)
    }

    pub fn new(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: Option<SourceProgressView>,
    ) -> Result<Self> {
        Self::new_with_progress_reader(id, stream, config, progress)
    }

    pub fn new_with_progress_reader(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: Option<SourceProgressView>,
    ) -> Result<Self> {
        anyhow::ensure!(
            !matches!(config.start, MySqlStartPosition::Snapshot { .. }),
            "MySQL snapshot mode requires its paired bootstrap provider"
        );
        Self::build(id, stream, config, progress, None, None, None)
    }

    pub fn coordinated(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: SourceProgressView,
    ) -> Result<(Self, Arc<super::MySqlSnapshot>)> {
        let snapshot = Arc::new(super::MySqlSnapshot::new(
            id.clone(),
            stream.clone(),
            config.clone(),
            progress.clone(),
        )?);
        let source = Self::with_snapshot(id, stream, config, progress, snapshot.clone())?;
        Ok((source, snapshot))
    }

    pub fn with_snapshot(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: SourceProgressView,
        snapshot: Arc<super::MySqlSnapshot>,
    ) -> Result<Self> {
        snapshot.validate_source(&id, &stream, &config, &progress)?;
        Self::build(
            id,
            stream,
            config,
            Some(progress),
            Some(snapshot),
            None,
            None,
        )
    }

    pub fn new_with_consumer_progress(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        consumers: ConsumerBinding,
    ) -> Result<Self> {
        anyhow::ensure!(
            !matches!(config.start, MySqlStartPosition::Snapshot { .. }),
            "MySQL snapshot mode requires its actual consumer bootstrap providers"
        );
        Self::build(id, stream, config, None, None, Some(consumers), None)
    }

    pub fn with_consumer_snapshots(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        consumers: ConsumerBinding,
        snapshots: impl IntoIterator<Item = Arc<super::MySqlSnapshot>>,
    ) -> Result<Self> {
        anyhow::ensure!(
            matches!(config.start, MySqlStartPosition::Snapshot { .. }),
            "MySQL consumer snapshots require explicit snapshot mode"
        );
        let mut providers = BTreeMap::new();
        for snapshot in snapshots {
            snapshot.validate_consumer_source(&id, &stream, &config, &consumers)?;
            anyhow::ensure!(
                providers
                    .insert(snapshot.consumer_id().clone(), snapshot)
                    .is_none(),
                "duplicate MySQL consumer snapshot"
            );
        }
        anyhow::ensure!(
            providers.keys().eq(consumers.members.iter()),
            "MySQL snapshots must cover every actual consumer exactly once"
        );
        let mut source = Self::build(id, stream, config, None, None, Some(consumers), None)?;
        source.consumer_snapshots = providers;
        Ok(source)
    }

    pub fn shared_rows(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        consumers: ConsumerBinding,
        options: rows::Options,
    ) -> Result<Self> {
        let snapshot = matches!(config.start, MySqlStartPosition::Snapshot { .. });
        let mut source = Self::build(
            id,
            stream,
            config,
            None,
            None,
            Some(consumers),
            Some(options),
        )?;
        source.published_snapshot = snapshot;
        Ok(source)
    }

    fn build(
        id: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: Option<SourceProgressView>,
        snapshot: Option<Arc<super::MySqlSnapshot>>,
        consumers: Option<ConsumerBinding>,
        rows: Option<rows::Options>,
    ) -> Result<Self> {
        if rows.is_some() {
            config.validate_rows()?;
        } else {
            config.validate()?;
        }
        anyhow::ensure!(
            config.replay.is_some() == (progress.is_some() || consumers.is_some())
                && !(progress.is_some() && consumers.is_some()),
            "MySQL replay and actual consumer progress must be configured together"
        );
        if let Some(consumers) = &consumers {
            anyhow::ensure!(
                consumers.producer_id() == &id,
                "MySQL consumer progress belongs to another producer"
            );
        }
        let position_policy = consumers
            .as_ref()
            .map(|_| {
                InputPositionPolicy::new(
                    SourceProgressKey::Source(id.clone()),
                    if rows.is_some() {
                        Arc::new(rows::Positions)
                            as Arc<dyn drasi_computation_contracts::InputPositionComparator>
                    } else {
                        Arc::new(Positions)
                    },
                )
                .map(Arc::new)
            })
            .transpose()?;
        let descriptor = Self::describe(id, config.output)?;
        let reconnect_budget_ms = Arc::new(AtomicU64::new(config.reconnect_budget_ms));
        Ok(Self {
            descriptor,
            stream,
            config,
            progress,
            consumers,
            position_policy,
            receiver: None,
            shutdown: watch::channel(Shutdown::Running).0,
            pause: PauseControl::new(),
            worker: RwLock::new(None),
            cleanup_required: false,
            sequence: Arc::new(AtomicU64::new(0)),
            reconnect_budget_ms,
            credentials: None,
            status: None,
            disposed: false,
            snapshot,
            consumer_snapshots: BTreeMap::new(),
            schema: Arc::new(super::schema::Publication::default()),
            published_snapshot: false,
            rows,
            positional: false,
            idle_progress: rows::IDLE_PROGRESS,
        })
    }

    pub fn ports(output: MySqlOutput) -> Result<Vec<PortDescriptor>> {
        let schema = match output {
            MySqlOutput::Changes => GraphChangeCodec::schema(),
            MySqlOutput::Transactions => SourceTransactionCodec::schema(),
        };
        Ok(vec![PortDescriptor::new(
            PortId::try_new("out")?,
            PortDirection::Output,
            schema.descriptor().clone(),
            PipeRequirements::default(),
        )])
    }

    pub fn describe(id: ComponentId, output: MySqlOutput) -> Result<ComponentDescriptor> {
        Ok(ComponentDescriptor::try_new(id, Self::ports(output)?)?)
    }

    /// The configuration this activation connects with.
    fn activation_config(&self) -> MySqlConfig {
        match &self.credentials {
            Some((user, password)) => self.config.with_credentials(user.clone(), password.clone()),
            None => self.config.clone(),
        }
    }

    fn scope(&self) -> Scope {
        match (&self.consumers, &self.rows, &self.progress) {
            (Some(consumers), Some(_), _) => Scope::Rows(consumers.graph_id().to_owned()),
            (Some(consumers), None, _) => Scope::Shared(consumers.graph_id().to_owned()),
            (None, _, progress) => Scope::Single(progress.as_ref().map(|owner| {
                (
                    owner.graph_id().to_owned(),
                    owner.component_id().as_str().to_owned(),
                )
            })),
        }
    }

    fn profile(&self) -> Profile {
        if self.positional {
            Profile::Positional
        } else if self.config.output == MySqlOutput::Changes {
            if matches!(self.config.start, MySqlStartPosition::Snapshot { .. }) {
                Profile::Transactional
            } else {
                Profile::Rows
            }
        } else {
            Profile::Transactional
        }
    }

    async fn prepare(&self, snapshot: Starting) -> Result<Worker> {
        let config = self.activation_config();
        let progress = self
            .progress
            .as_ref()
            .map(SourceProgressView::subscribe)
            .transpose()?;
        let mut connection = Connection::connect(
            &config.connection,
            config.max_protocol_bytes.get(),
            config.timeout(),
        )
        .await?;
        let prepared = async {
            let prepared = self
                .prepare_connection(&config, &mut connection, &snapshot)
                .await?;
            let consumer_progress = match (&self.consumers, &snapshot) {
                (Some(consumers), Starting::Consumers(snapshot)) if self.rows.is_none() => {
                    Some(ConsumerUpdates::new(
                        consumers,
                        snapshot,
                        self.descriptor.id(),
                        prepared.binding,
                    )?)
                }
                (None, _) | (Some(_), Starting::Consumers(_)) => None,
                _ => anyhow::bail!("MySQL consumer progress initialization is missing"),
            };
            let row_progress = match (&self.consumers, &snapshot, self.rows) {
                (Some(consumers), Starting::Consumers(snapshot), Some(options)) => {
                    Some(rows::Updates::new(
                        consumers,
                        snapshot,
                        self.descriptor.id(),
                        prepared.binding,
                        options,
                    )?)
                }
                _ => None,
            };
            Ok((prepared, consumer_progress, row_progress))
        }
        .await;
        match prepared {
            Ok((prepared, consumer_progress, row_progress)) => {
                let mut decoder_config = config.clone();
                if self.rows.is_some() {
                    decoder_config.output = MySqlOutput::Transactions;
                }
                Ok(Worker {
                    decoder_config,
                    positional: self.positional,
                    profile: self.profile(),
                    scope: self.scope(),
                    schema: self.schema.clone(),
                    server: prepared.server,
                    connection: Some(connection),
                    decoder: prepared.decoder,
                    start: prepared.start,
                    binding: prepared.binding,
                    confirmed: prepared.confirmed,
                    source: self.descriptor.id().clone(),
                    progress,
                    consumer_progress,
                    row_progress,
                    row_admitted: self.sequence.load(Ordering::Relaxed),
                    stream: self.stream.clone(),
                    sequence: self.sequence.clone(),
                    limits: config.transactions,
                    generation: match snapshot {
                        Starting::Single(snapshot) => snapshot.reset_generation,
                        _ => 0,
                    },
                    holding: prepared.origin.is_some(),
                    output: prepared.origin.into_iter().collect(),
                    emission: None,
                    last_emission: Instant::now(),
                    idle_progress: self.idle_progress,
                    pause: self.pause.gate(),
                    reconnect_budget_ms: self.reconnect_budget_ms.clone(),
                    status: self.status.clone(),
                    buffered: VecDeque::new(),
                    buffered_bytes: 0,
                    config,
                })
            }
            Err(error) => match connection.close().await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(OperationFailures::new(
                    "MySQL preparation and socket cleanup failed",
                    vec![error, cleanup],
                )
                .into()),
            },
        }
    }

    async fn prepare_connection(
        &self,
        config: &MySqlConfig,
        connection: &mut Connection,
        starting: &Starting,
    ) -> Result<Prepared> {
        let found = catalog::inspect_profile(connection, config, self.profile()).await?;
        let (server, tables) = (found.server.clone(), found.tables.clone());
        if self.positional {
            self.schema.capture_positional(&tables)?;
        } else {
            self.schema.capture(&tables)?;
        }
        let binding = self.scope().binding(
            config,
            &server,
            &tables,
            self.descriptor.id(),
            &self.stream,
            self.positional,
        )?;
        let snapshot = match self.rows {
            Some(options) => rows::minimum(starting, self.descriptor.id(), binding, options)?,
            None => starting.minimum(self.descriptor.id(), binding)?,
        };
        let row_anchor = if self.rows.is_some() {
            snapshot
                .map(|snapshot| rows::checkpoint(snapshot, self.descriptor.id(), binding))
                .transpose()?
                .flatten()
        } else {
            None
        };
        // A group mixing fresh and resumed members resumes at its minimum:
        // fresh members join there. Only a group with no cursor at all needs
        // an origin, which is committed before any row is published.
        let fresh = self.rows.is_some() && {
            let mut fresh = true;
            for state in starting.states() {
                fresh &= rows::checkpoint(state, self.descriptor.id(), binding)?.is_none();
            }
            fresh
        };
        let anchor = if self.rows.is_some() {
            row_anchor.as_ref().map(|row| row.position.clone())
        } else {
            snapshot
                .map(|snapshot| checkpoint(snapshot, self.descriptor.id(), binding))
                .transpose()?
                .flatten()
        };
        if let Some(provider) = &self.snapshot {
            provider.validate_live(snapshot.context("MySQL snapshot has no owner")?, binding)?;
        }
        if let Starting::Consumers(group) = starting {
            for (id, provider) in &self.consumer_snapshots {
                provider.validate_live(
                    group
                        .consumers
                        .get(id)
                        .context("MySQL snapshot consumer is missing")?,
                    binding,
                )?;
            }
        }
        if self.published_snapshot {
            for progress in starting.states() {
                anyhow::ensure!(
                    progress.bootstrap_complete
                        && if self.rows.is_some() {
                            rows::checkpoint(progress, self.descriptor.id(), binding)?.is_some()
                        } else {
                            checkpoint(progress, self.descriptor.id(), binding)?.is_some()
                        },
                    "MySQL initialization has no complete consumer-owned boundary"
                );
            }
        }
        anyhow::ensure!(
            self.snapshot.is_some()
                || !self.consumer_snapshots.is_empty()
                || self.published_snapshot
                || anchor
                    .as_ref()
                    .is_none_or(|position| matches!(position.version, 1 | 3)),
            "snapshot cursor requires its paired bootstrap provider"
        );
        let confirmed = anchor
            .as_ref()
            .map(Position::sequence)
            .transpose()?
            .unwrap_or(0);
        self.sequence.fetch_max(
            starting
                .states()
                .filter_map(|snapshot| snapshot.transport_sequences.get(&self.stream).copied())
                .max()
                .unwrap_or(0),
            Ordering::Relaxed,
        );
        if self.rows.is_some() {
            for state in starting.states() {
                if let Some(position) = rows::checkpoint(state, self.descriptor.id(), binding)? {
                    self.sequence
                        .fetch_max(position.sequence, Ordering::Relaxed);
                }
            }
        }
        let gtid = match (&anchor, &config.start) {
            (None, MySqlStartPosition::Gtid { set }) => Some(set.clone()),
            _ => None,
        };
        let (file, offset) = match &anchor {
            Some(position) => (
                position.file.clone(),
                if matches!(position.version, 2 | 3)
                    || row_anchor.as_ref().is_some_and(|row| row.completed())
                {
                    position.end
                } else {
                    position.start
                },
            ),
            None => match &config.start {
                MySqlStartPosition::Gtid { .. } => (String::new(), 4),
                MySqlStartPosition::Start => {
                    let logs = connection.query("SHOW BINARY LOGS").await?;
                    let first = logs
                        .values
                        .first()
                        .context("MySQL binary log is unavailable")?;
                    (field(first, 0)?.to_string(), 4)
                }
                MySqlStartPosition::Position { file, position } => (file.clone(), *position),
                MySqlStartPosition::End => {
                    let statement = if connection.version >= (8, 4, 0) {
                        "SHOW BINARY LOG STATUS"
                    } else {
                        "SHOW MASTER STATUS"
                    };
                    let rows = connection.query(statement).await?;
                    let [row] = rows.values.as_slice() else {
                        anyhow::bail!("MySQL binary log is unavailable");
                    };
                    (field(row, 0)?.to_string(), field(row, 1)?.parse()?)
                }
                MySqlStartPosition::Snapshot { .. } => {
                    anyhow::bail!("MySQL snapshot has no committed boundary")
                }
            },
        };
        if gtid.is_none() {
            super::file_number(&file)?;
        }
        let logs = connection.query("SHOW BINARY LOGS").await?;
        if gtid.is_none() {
            ensure_retained(self.descriptor.id(), &logs, &file, offset)?;
        }
        if self.positional {
            self.verify_positional_profile(config, &server, &logs, &file, offset, gtid.is_some())
                .await?;
            let (current_server, current_tables) =
                catalog::inspect_positional(connection, config).await?;
            anyhow::ensure!(
                current_server == server
                    && serde_json::to_vec(&current_tables)? == serde_json::to_vec(&tables)?,
                "positional MySQL catalog changed during startup"
            );
        }
        if let Starting::Consumers(group) = starting {
            self.verify_consumer_history(config, &server, &tables, &logs, group, binding)
                .await?;
        }
        // A fresh row group starting at the end records that end as a
        // progress boundary, proven like a snapshot boundary.
        let origin = if fresh && matches!(config.start, MySqlStartPosition::End) {
            Some(Position {
                version: 3,
                binding,
                file: file.clone(),
                start: 4,
                end: offset,
                digest: self.boundary_digest(config, &server, &file, offset).await?,
            })
        } else {
            None
        };
        let anchor = anchor.or_else(|| origin.clone());
        let confirmed = match &origin {
            Some(origin) => origin.sequence()?,
            None => confirmed,
        };
        let start = match (&anchor, gtid) {
            (Some(position), _) if matches!(position.version, 2 | 3) => StartPoint::History {
                file: file.clone(),
                boundary: offset,
                digest: position.digest,
                confirmed,
            },
            (_, Some(set)) => StartPoint::Gtid(set),
            _ => StartPoint::Binlog {
                file: file.clone(),
                offset,
                anchor: anchor
                    .clone()
                    .filter(|position| position.version == 1 && self.rows.is_none()),
            },
        };
        let mut decoder_config = config.clone();
        if self.rows.is_some() {
            decoder_config.output = MySqlOutput::Transactions;
        }
        let decoder = open_decoder_with(
            connection,
            &decoder_config,
            DecoderStart {
                source: self.descriptor.id(),
                stream: &self.stream,
                tables,
                skipped: found.skipped,
                binding,
                sequence: &self.sequence,
                positional: self.positional,
                point: &start,
            },
            self.config.output == MySqlOutput::Changes,
        )
        .await?;
        let origin = origin
            .map(|position| {
                rows::progress_record(position, self.descriptor.id(), &self.stream, &self.sequence)
            })
            .transpose()?;
        Ok(Prepared {
            decoder,
            binding,
            confirmed,
            server,
            start,
            origin,
        })
    }

    async fn verify_positional_profile(
        &self,
        config: &MySqlConfig,
        server: &str,
        logs: &super::connection::Rows,
        file: &str,
        offset: u32,
        gtid: bool,
    ) -> Result<()> {
        let floor = if gtid {
            None
        } else {
            Some(super::file_number(file)?)
        };
        for row in &logs.values {
            let name = field(row, 0)?;
            let number = super::file_number(name)?;
            if floor.is_some_and(|floor| number < floor) {
                continue;
            }
            let start = if !gtid && name == file { offset } else { 4 };
            let end: u32 = field(row, 1)?.parse()?;
            let mut connection = Connection::connect(
                &config.connection,
                config.max_protocol_bytes.get(),
                config.timeout(),
            )
            .await?;
            let proof = async {
                anyhow::ensure!(
                    super::session::server_id(&mut connection).await? == server,
                    "positional catalog history reached another MySQL server"
                );
                tokio::time::timeout(
                    config.timeout(),
                    Binlog::verify_positional_catalog_history(
                        &mut connection,
                        config,
                        name,
                        start,
                        end,
                    ),
                )
                .await
                .context("positional MySQL catalog history deadline")?
            }
            .await;
            let cleanup = connection.close().await;
            match (proof, cleanup) {
                (Ok(()), Ok(())) => {}
                (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
                (Err(error), Err(cleanup)) => {
                    return Err(OperationFailures::new(
                        "positional catalog history and close failed",
                        vec![error, cleanup],
                    )
                    .into())
                }
            }
        }
        Ok(())
    }

    /// The file identity of `file` read at `boundary` through a separate,
    /// finite history connection.
    async fn boundary_digest(
        &self,
        config: &MySqlConfig,
        server: &str,
        file: &str,
        boundary: u32,
    ) -> Result<[u8; 32]> {
        let mut connection = Connection::connect(
            &config.connection,
            config.max_protocol_bytes.get(),
            config.timeout(),
        )
        .await?;
        let proof = async {
            anyhow::ensure!(
                super::session::server_id(&mut connection).await? == server,
                "MySQL origin history reached another server"
            );
            Binlog::boundary_identity(
                &mut connection,
                config,
                file,
                boundary,
                super::binlog::HistoryMode::Verify,
            )
            .await
            .map(|(_, digest)| digest)
        }
        .await;
        let cleanup = connection.close().await;
        match (proof, cleanup) {
            (Ok(digest), Ok(())) => Ok(digest),
            (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
            (Err(error), Err(cleanup)) => Err(OperationFailures::new(
                "MySQL origin history and close failed",
                vec![error, cleanup],
            )
            .into()),
        }
    }

    async fn verify_consumer_history(
        &self,
        config: &MySqlConfig,
        server: &str,
        tables: &catalog::Tables,
        logs: &super::connection::Rows,
        group: &ConsumerProgressSnapshot,
        binding: [u8; 32],
    ) -> Result<()> {
        let mut positions = BTreeMap::new();
        for snapshot in group.consumers.values() {
            let position = if self.rows.is_some() {
                rows::checkpoint(snapshot, self.descriptor.id(), binding)?.map(|row| row.position)
            } else {
                checkpoint(snapshot, self.descriptor.id(), binding)?
            };
            let Some(position) = position else {
                continue;
            };
            ensure_retained(self.descriptor.id(), logs, &position.file, position.end)?;
            let previous = positions.insert(
                (position.file.clone(), position.end, position.version),
                position.clone(),
            );
            anyhow::ensure!(
                previous.is_none_or(|previous| previous == position),
                "MySQL consumers disagree about retained transaction contents"
            );
        }
        for position in positions.into_values() {
            let mut connection = Connection::connect(
                &config.connection,
                config.max_protocol_bytes.get(),
                config.timeout(),
            )
            .await?;
            let verified = async {
                let identity = connection.query("SELECT @@server_uuid").await?;
                anyhow::ensure!(
                    identity.values.len() == 1 && field(&identity.values[0], 0)? == server,
                    "MySQL consumer history reached another server"
                );
                if matches!(position.version, 2 | 3) {
                    let (_, digest) = Binlog::boundary_identity(
                        &mut connection,
                        config,
                        &position.file,
                        position.end,
                        super::binlog::HistoryMode::Verify,
                    )
                    .await?;
                    anyhow::ensure!(
                        digest == position.digest,
                        "MySQL consumer snapshot history changed"
                    );
                } else {
                    connection
                        .execute("SET @master_binlog_checksum='CRC32'")
                        .await?;
                    connection
                        .start_binlog_history(&position.file, position.start)
                        .await?;
                    let mut config = config.clone();
                    config.output = MySqlOutput::Transactions;
                    let mut decoder = Binlog::new(Start {
                        source: self.descriptor.id().clone(),
                        stream: self.stream.clone(),
                        config,
                        tables: tables.clone(),
                        binding,
                        file: position.file.clone(),
                        offset: position.start,
                        anchor: Some(position),
                        sequence: Arc::new(AtomicU64::new(0)),
                        skipped: Default::default(),
                    })?;
                    if self.rows.is_some() {
                        decoder = decoder.with_row_semantics();
                    }
                    if self.positional {
                        decoder = decoder.with_positional_metadata();
                    }
                    while decoder.has_replay_anchor() {
                        anyhow::ensure!(
                            decoder
                                .decode(&read_event(&mut connection).await?)?
                                .is_empty(),
                            "MySQL history verification emitted unexpected live data"
                        );
                    }
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let cleanup = connection.close().await;
            match (verified, cleanup) {
                (Ok(()), Ok(())) => {}
                (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
                (Err(error), Err(cleanup)) => {
                    return Err(OperationFailures::new(
                        "MySQL consumer history verification and socket cleanup failed",
                        vec![error, cleanup],
                    )
                    .into())
                }
            }
        }
        Ok(())
    }
}

/// The result of preparing a replication connection.
struct Prepared {
    decoder: Binlog,
    binding: [u8; 32],
    confirmed: u64,
    server: String,
    start: StartPoint,
    /// A fresh row group's origin record, emitted before any row.
    origin: Option<OutputEnvelope>,
}

/// What a fresh decoder needs besides its configuration.
struct DecoderStart<'a> {
    source: &'a ComponentId,
    stream: &'a StreamId,
    tables: Tables,
    skipped: std::collections::BTreeSet<String>,
    binding: [u8; 32],
    sequence: &'a Arc<AtomicU64>,
    positional: bool,
    point: &'a StartPoint,
}

/// Reads one binlog event, mapping a binlog-dump refusal to its typed
/// permanent failure.
async fn read_event(connection: &mut Connection) -> Result<Bytes> {
    connection
        .event()
        .await
        .map_err(errors::classify_dump_error)
}

/// Fails with a typed, permanent `PositionUnavailable` when `file` is not
/// retained up to `offset`.
fn ensure_retained(
    source: &ComponentId,
    logs: &super::connection::Rows,
    file: &str,
    offset: u32,
) -> Result<()> {
    let mut retained = false;
    for row in &logs.values {
        if field(row, 0)? == file {
            retained = u64::from(offset) <= field(row, 1)?.parse::<u64>()?;
        }
    }
    if retained {
        return Ok(());
    }
    let requested = serde_json::to_vec(&MySqlStartPosition::Position {
        file: file.to_owned(),
        position: offset,
    })?;
    Err(
        anyhow::Error::new(SourceFailure::PositionUnavailable(format!(
            "binlog {file} at {offset} is no longer retained by the server"
        )))
        .context(SourceError::PositionUnavailable {
            source_id: source.clone(),
            requested: Bytes::from(requested),
            earliest_available: None,
        }),
    )
}

/// Starts the binlog dump at `point` and returns a decoder that has seen the
/// stream's format description. The heartbeat period keeps an idle stream
/// observable by the dead-peer watchdog. `row_semantics` is set for sources
/// whose output is row changes (including tracked rows, whose decoder still
/// assembles whole transactions).
async fn open_decoder_with(
    connection: &mut Connection,
    config: &MySqlConfig,
    start: DecoderStart<'_>,
    row_semantics: bool,
) -> Result<Binlog> {
    connection
        .execute(&format!(
            "SET @master_heartbeat_period={}",
            config.connection.heartbeat_interval_ms.get() * 1_000_000
        ))
        .await?;
    let (file, offset, anchor, history) = match start.point {
        StartPoint::History {
            file,
            boundary,
            digest,
            confirmed,
        } => {
            let (format, found) =
                Binlog::boundary_identity(connection, config, file, *boundary, HistoryMode::Live)
                    .await
                    .map_err(errors::classify_dump_error)?;
            anyhow::ensure!(
                &found == digest,
                "MySQL snapshot history has different contents"
            );
            (file.clone(), *boundary, None, Some((format, *confirmed)))
        }
        StartPoint::Gtid(set) => {
            connection
                .execute("SET @master_binlog_checksum='CRC32'")
                .await?;
            connection
                .start_gtid(config.connection.server_id.get(), set)
                .await?;
            (String::new(), 4, None, None)
        }
        StartPoint::Binlog {
            file,
            offset,
            anchor,
        } => {
            connection
                .execute("SET @master_binlog_checksum='CRC32'")
                .await?;
            connection
                .start_binlog(config.connection.server_id.get(), file, *offset)
                .await?;
            (file.clone(), *offset, anchor.clone(), None)
        }
    };
    let mut decoder = Binlog::new(Start {
        source: start.source.clone(),
        stream: start.stream.clone(),
        config: config.clone(),
        tables: start.tables,
        binding: start.binding,
        file,
        offset,
        anchor,
        sequence: start.sequence.clone(),
        skipped: start.skipped,
    })?;
    if row_semantics {
        decoder = decoder.with_row_semantics();
    }
    if start.positional {
        decoder = decoder.with_positional_metadata();
    }
    if matches!(start.point, StartPoint::Gtid(_)) {
        decoder = decoder.with_gtid_start();
    }
    if let Some((format, confirmed)) = history {
        decoder.after_snapshot(format, confirmed);
        return Ok(decoder);
    }
    // A sent dump request is not evidence that the server accepted the cursor.
    for _ in 0..2 {
        let frame = read_event(connection).await?;
        anyhow::ensure!(
            decoder.decode(&frame)?.is_empty(),
            "MySQL data arrived before startup completed"
        );
        if decoder.ready() {
            return Ok(decoder);
        }
    }
    anyhow::bail!("MySQL did not provide its replication format during startup")
}

/// Races `future` against cancellation and an optional deadline.
async fn bounded<T>(
    what: &str,
    cancellation: Option<&CancellationToken>,
    deadline: Option<Instant>,
    future: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let timed = async {
        match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline, future)
                .await
                .map_err(|_| anyhow::anyhow!("MySQL {what} exceeded its deadline"))?,
            None => future.await,
        }
    };
    match cancellation {
        Some(token) => tokio::select! {
            biased;
            _ = token.cancelled() => anyhow::bail!("MySQL {what} was cancelled"),
            result = timed => result,
        },
        None => timed.await,
    }
}

impl MySqlSource {
    pub fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    pub fn replay_retention(&self) -> Option<StorageDurability> {
        (self.config.replay == Some(MySqlRetention::UntilProcessed))
            .then_some(StorageDurability::LOCAL_PROCESS_RESTART)
    }
    pub fn consumer_progress(&self) -> Option<ConsumerBinding> {
        self.consumers.clone()
    }
    pub fn position_policy(&self) -> Option<Arc<InputPositionPolicy>> {
        self.position_policy.clone()
    }
    /// The engine configuration, nested under `settings` (the in-process
    /// wrapper's configuration shape).
    pub fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"stream": self.stream.as_str(), "settings": self.config}))
    }
    /// The engine configuration this object runs with.
    pub fn config(&self) -> &MySqlConfig {
        &self.config
    }
    /// Whether this object uses positional (`@N`) rows.
    pub fn is_positional(&self) -> bool {
        self.positional
    }
    /// Reports readiness changes to `sink`.
    pub fn with_status(mut self, sink: StatusSink) -> Self {
        self.status = Some(sink);
        self
    }
    /// Whether a worker is owned (running, or awaiting cleanup).
    pub fn is_running(&mut self) -> bool {
        self.worker.get_mut().is_some()
    }
    /// Whether the worker has acknowledged a pause.
    pub fn is_paused(&self) -> bool {
        self.pause.is_paused()
    }
    /// Overrides the least quiet time before a row source commits a
    /// transaction without selected rows as progress (tests).
    #[doc(hidden)]
    pub fn set_idle_progress(&mut self, interval: Duration) {
        self.idle_progress = interval;
    }

    /// Applies a new `reconnectBudgetMs` to the running worker's next
    /// transport failure.
    pub fn set_reconnect_budget(&mut self, millis: u64) {
        self.reconnect_budget_ms.store(millis, Ordering::Relaxed);
        self.config.reconnect_budget_ms = millis;
    }
    /// Replaces the connection and operational settings of a stopped source.
    /// The selection and the output contract must stay unchanged; the caller
    /// checks start-position changes against the committed cursor.
    pub fn reconfigure(&mut self, config: MySqlConfig) -> Result<()> {
        anyhow::ensure!(
            self.worker.get_mut().is_none(),
            "MySQL reconfiguration requires a stopped source"
        );
        let current = &self.config;
        let snapshot =
            |start: &MySqlStartPosition| matches!(start, MySqlStartPosition::Snapshot { .. });
        anyhow::ensure!(
            current.connection.database == config.connection.database
                && current.tables == config.tables
                && current.table_keys == config.table_keys
                && snapshot(&current.start) == snapshot(&config.start)
                && current.output == config.output
                && current.replay == config.replay,
            "MySQL reconfiguration cannot change fields bound to the committed cursor"
        );
        if self.rows.is_some() {
            config.validate_rows()?;
        } else {
            config.validate()?;
        }
        self.reconnect_budget_ms
            .store(config.reconnect_budget_ms, Ordering::Relaxed);
        self.config = config;
        Ok(())
    }

    fn report(&self, status: SourceStatus) {
        if let Some(sink) = &self.status {
            sink(status);
        }
    }

    /// Starts with no host deadline, cancellation or credentials.
    pub async fn start(&mut self) -> Result<()> {
        self.start_with(Activation::default()).await
    }

    /// Waits for progress readiness (bounded only by the host deadline and
    /// cancellation), opens replication within `ioTimeoutMs`, and spawns the
    /// worker. A failed or cancelled start keeps the cleanup guard: Stop is
    /// required before the next start.
    pub async fn start_with(&mut self, activation: Activation<'_>) -> Result<()> {
        anyhow::ensure!(!self.disposed, "MySQL source was disposed");
        anyhow::ensure!(
            !self.cleanup_required,
            "MySQL source requires awaited cleanup before restart"
        );
        self.credentials = activation.credentials;
        self.cleanup_required = true;
        self.schema.clear()?;
        let io = self.config.timeout();
        let opened = bounded(
            "start",
            activation.cancellation,
            activation.deadline,
            async {
                let snapshot = match (&self.progress, &self.consumers) {
                    (Some(progress), None) => Starting::Single(progress.wait_ready().await?),
                    (None, Some(consumers)) => {
                        Starting::Consumers(consumers.read_only().wait_ready().await?)
                    }
                    (None, None) => Starting::Untracked,
                    (Some(_), Some(_)) => {
                        anyhow::bail!("MySQL has conflicting replay progress bindings")
                    }
                };
                if let (Some(consumers), Starting::Consumers(group)) = (&self.consumers, &snapshot)
                {
                    anyhow::ensure!(
                        consumers.members.iter().eq(group.consumers.keys()),
                        "MySQL required consumer membership changed before initialization"
                    );
                }
                if let Some(options) = self.rows {
                    for state in snapshot.states() {
                        rows::validate(state, options)?;
                    }
                } else {
                    snapshot.validate()?;
                }
                tokio::time::timeout(io, self.prepare(snapshot))
                    .await
                    .map_err(|_| anyhow::anyhow!("MySQL connection setup exceeded ioTimeoutMs"))?
            },
        )
        .await;
        let mut worker = opened?;
        let (sender, receiver) = mpsc::channel(1);
        self.receiver = Some(receiver);
        self.shutdown.send_replace(Shutdown::Running);
        self.pause.resume();
        let mut shutdown = self.shutdown.subscribe();
        let status = self.status.clone();
        spawn_owned_worker(&self.worker, async move {
            let stopped = tokio::select! {
                biased;
                mode = shutdown.wait_for(|mode| *mode != Shutdown::Running) => {
                    Ok(mode.is_ok_and(|mode| *mode == Shutdown::Drain))
                }
                result = worker.run(sender) => Err(result),
            };
            let result = match stopped {
                Ok(drain) => {
                    if drain {
                        worker.drain().await;
                    }
                    Ok(())
                }
                Err(result) => {
                    worker.drain().await;
                    result
                }
            };
            // The worker's pause gate drops with it, so a pending pause
            // settles: nothing is read or emitted after exit.
            drop(worker);
            if let Err(error) = &result {
                log::error!("Native MySQL replication stopped: {error:#}");
                if let Some(status) = status {
                    status(SourceStatus::Failed(format!("{error:#}")));
                }
            }
            result
        })
        .await?;
        self.report(SourceStatus::Ready);
        Ok(())
    }

    /// Stops reading and emitting at the next transaction boundary. The
    /// connection stays open while the paused socket fits a bounded buffer.
    /// A cancelled or expired pause leaves the source running.
    pub async fn pause(
        &mut self,
        deadline: Option<Instant>,
        cancellation: Option<&CancellationToken>,
    ) -> Result<()> {
        if self.worker.get_mut().is_none() {
            return Ok(());
        }
        let deadline = deadline.unwrap_or_else(|| Instant::now() + self.config.timeout());
        let cancellation = cancellation.cloned().unwrap_or_default();
        self.pause
            .pause(Some(deadline), &cancellation)
            .await
            .map_err(anyhow::Error::new)
            .context("MySQL could not reach a transaction boundary before the pause deadline")
    }

    /// Reopens exactly what Pause closed.
    pub fn resume(&mut self) {
        self.pause.resume();
    }

    /// Stops with `Drain` and the configured I/O timeout.
    pub async fn stop(&mut self) -> Result<()> {
        self.stop_with(StopMode::Drain, None).await
    }

    /// `Drain` closes the replication connection gracefully; `Abort` cancels
    /// the worker. On expiry the worker stays owned and the error is
    /// retryable: a later Stop completes the cleanup. An earlier replication
    /// failure was reported through `next` and does not fail Stop.
    pub async fn stop_with(&mut self, mode: StopMode, deadline: Option<Instant>) -> Result<()> {
        let shutdown = match mode {
            StopMode::Abort => Shutdown::Abort,
            StopMode::Drain => Shutdown::Drain,
        };
        self.shutdown.send_replace(shutdown);
        let timeout = deadline.map_or(self.config.timeout(), |deadline| {
            deadline.saturating_duration_since(Instant::now())
        });
        let joined = if shutdown == Shutdown::Abort {
            cancel_owned_worker(self.worker.get_mut(), timeout).await
        } else {
            join_owned_worker(self.worker.get_mut(), timeout).await
        };
        match joined {
            Ok(WorkerCompletion::Completed(Err(error))) => {
                log::debug!("MySQL worker had already failed: {error:#}");
            }
            Ok(_) => {}
            Err(error @ WorkerCleanupError::TimedOut { .. }) => {
                return Err(anyhow::Error::new(error)
                    .context("MySQL replication did not stop within its deadline"))
            }
            Err(error) => return Err(error.into()),
        }
        self.receiver = None;
        self.schema.clear()?;
        self.cleanup_required = false;
        Ok(())
    }

    /// Retires the schema publication. The source owns no server-side state,
    /// so erasing disposal has nothing to remove; it is idempotent and
    /// refuses while a worker is owned.
    pub fn dispose(&mut self) -> Result<()> {
        anyhow::ensure!(
            self.worker.get_mut().is_none(),
            "dispose requires a stopped MySQL source"
        );
        self.schema.retire();
        self.receiver = None;
        self.disposed = true;
        Ok(())
    }
}

impl MySqlSource {
    pub fn schema_reader(&self) -> Arc<dyn drasi_computation_contracts::SourceSchemaReader> {
        self.schema.reader()
    }
    pub fn recovery_reader(&self) -> Option<SourceProgressView> {
        self.progress.clone()
    }
    pub async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let receiver = self
            .receiver
            .as_mut()
            .context("native MySQL source is not running")?;
        if let Some(envelope) = receiver.recv().await {
            return Ok(Some(envelope));
        }
        match join_owned_worker(self.worker.get_mut(), self.config.timeout()).await? {
            WorkerCompletion::Completed(Err(error)) => Err(error),
            _ => anyhow::bail!("native MySQL replication ended; stop and restart the source"),
        }
    }
}

impl Drop for MySqlSource {
    fn drop(&mut self) {
        self.schema.retire();
        self.shutdown.send_replace(Shutdown::Abort);
        if let Some(worker) = self.worker.get_mut() {
            worker.abort();
            log::warn!("Native MySQL source dropped without awaited worker cleanup");
        }
    }
}

pub fn checkpoint(
    snapshot: &SourceProgressSnapshot,
    source: &ComponentId,
    binding: [u8; 32],
) -> Result<Option<Position>> {
    let Some(checkpoint) = snapshot
        .checkpoints
        .get(&SourceProgressKey::Source(source.clone()))
    else {
        return Ok(None);
    };
    let cursor = checkpoint
        .source_position
        .as_deref()
        .context("MySQL checkpoint has no cursor")?;
    anyhow::ensure!(
        cursor.len() <= drasi_computation_contracts::MAX_SOURCE_POSITION_BYTES,
        "MySQL checkpoint cursor exceeds the supported position limit"
    );
    let position: Position = serde_json::from_slice(cursor)?;
    anyhow::ensure!(
        position.sequence()? == checkpoint.sequence,
        "MySQL checkpoint sequence mismatch"
    );
    if position.binding != binding {
        return Err(errors::schema_changed(
            "the committed MySQL cursor is bound to different selected tables, keys or binding format",
        ));
    }
    Ok(Some(position))
}

pub fn source_binding(
    config: &MySqlConfig,
    server: &str,
    tables: &Tables,
    source: &ComponentId,
    stream: &StreamId,
    progress: Option<&SourceProgressView>,
) -> Result<[u8; 32]> {
    Scope::Single(progress.map(|owner| {
        (
            owner.graph_id().to_owned(),
            owner.component_id().as_str().to_owned(),
        )
    }))
    .binding(config, server, tables, source, stream, false)
}

pub fn shared_source_binding(
    config: &MySqlConfig,
    server: &str,
    tables: &Tables,
    source: &ComponentId,
    stream: &StreamId,
    graph: &str,
) -> Result<[u8; 32]> {
    Scope::Shared(graph.to_owned()).binding(config, server, tables, source, stream, false)
}

pub fn row_source_binding(
    config: &MySqlConfig,
    server: &str,
    tables: &Tables,
    source: &ComponentId,
    stream: &StreamId,
    graph: &str,
) -> Result<[u8; 32]> {
    Scope::Rows(graph.to_owned()).binding(config, server, tables, source, stream, false)
}

/// The binding hash (format v2): server, database, the selected table set
/// with each table's key columns and their declared types, source, stream,
/// configured start and owner scope. Non-key column changes do not change
/// it, so they never invalidate a committed cursor.
fn binding(
    config: &MySqlConfig,
    server: &str,
    tables: &Tables,
    source: &ComponentId,
    stream: &StreamId,
    scope: (&str, Option<(&str, &str)>),
    positional: bool,
) -> Result<[u8; 32]> {
    #[derive(serde::Serialize)]
    #[serde(untagged)]
    enum StartBinding<'a> {
        Snapshot { mode: &'static str },
        Configured(&'a MySqlStartPosition),
    }
    let start = match &config.start {
        MySqlStartPosition::Snapshot { .. } => StartBinding::Snapshot { mode: "snapshot" },
        other => StartBinding::Configured(other),
    };
    let facts = tables
        .iter()
        .map(|(table, columns)| {
            Ok((
                table.as_str(),
                catalog::key_facts(config, table, columns, positional)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(&(
        scope.0,
        server,
        &config.connection.database,
        facts,
        source.as_str(),
        stream.as_str(),
        start,
        scope.1,
    ))?);
    Ok(hash.finalize().into())
}

#[derive(Debug)]
pub enum SourceError {
    PositionUnavailable {
        source_id: ComponentId,
        requested: Bytes,
        earliest_available: Option<Bytes>,
    },
}
impl std::fmt::Display for SourceError {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PositionUnavailable { source_id, .. } => write!(
                output,
                "MySQL binlog position is unavailable for source {source_id}"
            ),
        }
    }
}
impl std::error::Error for SourceError {}

struct Worker {
    /// The activation configuration (activation credentials applied).
    config: MySqlConfig,
    decoder_config: MySqlConfig,
    positional: bool,
    profile: Profile,
    scope: Scope,
    schema: Arc<super::schema::Publication>,
    server: String,
    connection: Option<Connection>,
    decoder: Binlog,
    /// Where the current decoder started, used when it has not completed a
    /// transaction yet.
    start: StartPoint,
    source: ComponentId,
    binding: [u8; 32],
    progress: Option<Box<dyn SourceProgressUpdates>>,
    consumer_progress: Option<ConsumerUpdates>,
    row_progress: Option<rows::Updates>,
    row_admitted: u64,
    stream: StreamId,
    sequence: Arc<AtomicU64>,
    limits: drasi_computation_contracts::SourceTransactionLimits,
    generation: u64,
    confirmed: u64,
    /// Decoded envelopes not yet admitted by the host.
    output: VecDeque<OutputEnvelope>,
    /// Holds rows until every member committed the origin record.
    holding: bool,
    /// When the worker last admitted an envelope.
    last_emission: Instant,
    /// Least quiet time before a transaction without selected rows is
    /// committed as a progress record.
    idle_progress: Duration,
    /// Rows of one decoded transaction, expanded as they are admitted.
    emission: Option<rows::Emission>,
    pause: PauseGate,
    reconnect_budget_ms: Arc<AtomicU64>,
    status: Option<StatusSink>,
    /// Raw events read while paused, decoded on resume.
    buffered: VecDeque<Bytes>,
    buffered_bytes: usize,
}

/// Reads the next event within the dead-peer window.
async fn read_live(connection: &mut Option<Connection>, window: Duration) -> Result<Bytes> {
    let connection = connection
        .as_mut()
        .context("MySQL replication connection is not open")?;
    match tokio::time::timeout(window, read_event(connection)).await {
        Ok(frame) => frame,
        Err(_) => Err(anyhow::Error::new(errors::DeadPeer(window))),
    }
}

impl Worker {
    fn paused(&self) -> bool {
        self.pause.is_pause_requested()
    }

    fn report(&self, status: SourceStatus) {
        if let Some(sink) = &self.status {
            sink(status);
        }
    }

    fn dead_peer_window(&self) -> Duration {
        Duration::from_millis(self.config.connection.heartbeat_interval_ms.get())
            .saturating_mul(DEAD_PEER_HEARTBEATS)
            .max(self.config.timeout())
    }

    fn validate_progress(&mut self) -> Result<()> {
        if let Some(progress) = &mut self.row_progress {
            return progress.validate(&self.source, self.binding, self.row_admitted);
        }
        if let Some(progress) = &mut self.consumer_progress {
            let confirmed = progress.validate(&self.source, self.binding, self.decoder.admitted)?;
            anyhow::ensure!(
                confirmed >= self.confirmed,
                "MySQL consumer minimum moved backwards"
            );
            self.confirmed = confirmed;
            return Ok(());
        }
        let Some(progress) = &mut self.progress else {
            return Ok(());
        };
        let snapshot = progress.snapshot()?;
        anyhow::ensure!(
            snapshot.failure.is_none()
                && snapshot.ready
                && snapshot.recovered
                && snapshot.admitting
                && snapshot.reset_generation == self.generation
                && snapshot.persistent,
            "MySQL consumer progress was fenced, reset or lost persistence"
        );
        if let Some(position) = checkpoint(&snapshot, &self.source, self.binding)? {
            let sequence = position.sequence()?;
            anyhow::ensure!(
                sequence >= self.confirmed && sequence <= self.decoder.admitted,
                "MySQL committed progress moved outside the admitted range"
            );
            self.confirmed = sequence;
        }
        Ok(())
    }

    fn has_output(&self) -> bool {
        !self.output.is_empty() || self.emission.is_some()
    }

    /// The next envelope to admit, expanding row emissions lazily.
    fn take_output(&mut self) -> Result<Option<OutputEnvelope>> {
        if let Some(envelope) = self.output.pop_front() {
            return Ok(Some(envelope));
        }
        let Some(emission) = &mut self.emission else {
            return Ok(None);
        };
        match emission.next() {
            Some(row) => Ok(Some(row?)),
            None => {
                self.emission = None;
                Ok(None)
            }
        }
    }

    /// Streams until a permanent failure. A transport failure reconnects at
    /// the end of the last emitted transaction with jittered exponential
    /// backoff until `reconnectBudgetMs` is spent.
    async fn run(&mut self, sender: mpsc::Sender<OutputEnvelope>) -> Result<()> {
        loop {
            let error = match self.run_connected(&sender).await {
                Ok(()) => return Ok(()),
                Err(error) => error,
            };
            let deliberate = error.is::<PausedClose>();
            let budget = match self.reconnect_budget_ms.load(Ordering::Relaxed) {
                // A deliberate close while paused is not a failure: reopen
                // once even when reconnection after failures is disabled.
                0 if deliberate => self.config.timeout(),
                0 => return Err(error),
                millis => Duration::from_millis(millis),
            };
            if !deliberate && !errors::is_transient(&error) {
                return Err(error);
            }
            self.close_quietly().await;
            if !deliberate {
                self.report(SourceStatus::Reconnecting(format!("{error:#}")));
            }
            let mut backoff =
                BackoffPolicy::with_budget(RECONNECT_BASE, RECONNECT_MAX, budget).start();
            let mut failure = error;
            let mut first = deliberate;
            loop {
                let delay = if first {
                    first = false;
                    Duration::ZERO
                } else {
                    match backoff.next_delay() {
                        Some(delay) => delay,
                        None => {
                            return Err(failure.context("MySQL reconnect budget exhausted"));
                        }
                    }
                };
                if !delay.is_zero() {
                    log::warn!(
                        "MySQL replication interrupted; reconnect attempt {} in {delay:?}: {failure:#}",
                        backoff.attempt()
                    );
                }
                self.idle(delay).await;
                match tokio::time::timeout(self.config.timeout(), self.reopen()).await {
                    Ok(Ok(())) => {
                        log::info!(
                            "MySQL replication reconnected after {} attempt(s)",
                            backoff.attempt().max(1)
                        );
                        self.report(SourceStatus::Ready);
                        break;
                    }
                    Ok(Err(error)) if errors::is_transient(&error) => failure = error,
                    Ok(Err(error)) => return Err(error),
                    Err(elapsed) => {
                        failure = anyhow::Error::new(elapsed).context("MySQL reconnect timed out")
                    }
                }
            }
        }
    }

    /// Sleeps without a connection, acknowledging any pause meanwhile, and
    /// waits for Resume before reconnecting: nothing is read while paused.
    async fn idle(&mut self, delay: Duration) {
        let until = Instant::now() + delay;
        loop {
            let paused = self.pause.sync() == PauseState::Paused;
            if !paused && Instant::now() >= until {
                return;
            }
            tokio::select! {
                _ = tokio::time::sleep_until(until), if !paused => return,
                _ = self.pause.changed() => {}
            }
        }
    }

    async fn close_quietly(&mut self) {
        if let Some(connection) = self.connection.take() {
            let _ = tokio::time::timeout(Duration::from_secs(1), connection.close()).await;
        }
    }

    /// Reopens replication at the end of the last emitted transaction (or
    /// where the current decoder started), after verifying that the server
    /// and the bound catalog facts are unchanged.
    async fn reopen(&mut self) -> Result<()> {
        let point = match self.decoder.resume_point() {
            Some(resume) => StartPoint::Binlog {
                file: resume.file.clone(),
                offset: resume
                    .anchor
                    .as_ref()
                    .map_or(resume.end, |anchor| anchor.start),
                anchor: resume.anchor.clone(),
            },
            None => self.start.clone(),
        };
        let mut connection = Connection::connect(
            &self.config.connection,
            self.config.max_protocol_bytes.get(),
            self.config.timeout(),
        )
        .await?;
        let opened = async {
            let found = catalog::inspect_profile(&mut connection, &self.config, self.profile)
                .await?;
            if found.server != self.server {
                return Err(errors::unsupported(format!(
                    "MySQL server identity changed from {} to {} across a reconnect; resuming on \
                     another server (failover) is not supported",
                    self.server, found.server
                )));
            }
            let current = self.decoder.catalog().clone();
            if self.config.replay.is_some() {
                let binding = self.scope.binding(
                    &self.config,
                    &found.server,
                    &found.tables,
                    &self.source,
                    &self.stream,
                    self.positional,
                )?;
                if binding != self.binding {
                    return Err(errors::schema_changed(
                        "the selected tables or their keys changed while replication was disconnected",
                    ));
                }
            } else {
                for (table, columns) in &current {
                    match found.tables.get(table) {
                        Some(found_columns)
                            if catalog::key_facts(&self.config, table, columns, self.positional)?
                                != catalog::key_facts(
                                    &self.config,
                                    table,
                                    found_columns,
                                    self.positional,
                                )? =>
                        {
                            return Err(errors::schema_changed(format!(
                                "the key columns of selected table {table} changed while disconnected"
                            )));
                        }
                        Some(_) => {}
                        None if !self.config.tables.is_empty() => {
                            return Err(errors::schema_changed(format!(
                                "selected table {table} disappeared while disconnected"
                            )))
                        }
                        None => {}
                    }
                }
            }
            let logs = connection.query("SHOW BINARY LOGS").await?;
            match &point {
                StartPoint::Binlog { file, offset, .. } => {
                    ensure_retained(&self.source, &logs, file, *offset)?
                }
                StartPoint::History { file, boundary, .. } => {
                    ensure_retained(&self.source, &logs, file, *boundary)?
                }
                StartPoint::Gtid(_) => {}
            }
            let row_semantics = self.positional || self.config.output == MySqlOutput::Changes;
            open_decoder_with(
                &mut connection,
                &self.decoder_config,
                DecoderStart {
                    source: &self.source,
                    stream: &self.stream,
                    tables: current,
                    skipped: self.decoder.skipped().clone(),
                    binding: self.binding,
                    sequence: &self.sequence,
                    positional: self.positional,
                    point: &point,
                },
                row_semantics,
            )
            .await
        }
        .await;
        match opened {
            Ok(decoder) => {
                self.decoder = decoder;
                self.start = point;
                self.connection = Some(connection);
                Ok(())
            }
            Err(error) => {
                let _ = tokio::time::timeout(Duration::from_secs(1), connection.close()).await;
                Err(error)
            }
        }
    }

    /// Closes the connection gracefully for a Drain stop or after a failure.
    async fn drain(&mut self) {
        if let Some(connection) = self.connection.take() {
            if let Err(error) = tokio::time::timeout(self.config.timeout(), connection.close())
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
            {
                log::debug!("MySQL replication close reported: {error:#}");
            }
        }
    }

    async fn run_connected(&mut self, sender: &mpsc::Sender<OutputEnvelope>) -> Result<()> {
        loop {
            self.validate_progress()?;
            let paused = self.paused();
            // Pause at a transaction boundary: decoded output is held, and
            // the socket is drained into a bounded buffer so the server keeps
            // the connection.
            if paused && self.decoder.at_boundary() {
                if self.pause.sync() == PauseState::Paused {
                    self.paused_wait().await?;
                }
                continue;
            }
            if self.holding && !self.has_output() {
                if self
                    .row_progress
                    .as_ref()
                    .is_some_and(rows::Updates::initialized)
                {
                    self.holding = false;
                    log::info!("MySQL consumers committed the origin; streaming rows");
                } else {
                    // Events read meanwhile are buffered as during a pause.
                    self.paused_wait().await?;
                    continue;
                }
            }
            let has_output = self.has_output();
            if !has_output && self.buffered.is_empty() && self.connection.is_none() {
                return Err(anyhow::Error::new(PausedClose));
            }
            let deadline = self.decoder.deadline();
            let window = self.dead_peer_window();
            let between = self.decoder.at_boundary();
            tokio::select! {
                biased;
                changed = progress_changed(&mut self.progress, &mut self.consumer_progress, &mut self.row_progress) => changed?,
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Err(SourceTransactionError::Deadline.into()),
                _ = self.pause.pause_requested(), if !paused && between => {}
                permit = sender.reserve(), if has_output => {
                    let permit = permit.context("MySQL output receiver closed")?;
                    if let Some(envelope) = self.take_output()? {
                        self.row_admitted = self.row_admitted.max(envelope.envelope.system().sequence());
                        self.last_emission = Instant::now();
                        permit.send(envelope);
                    }
                }
                frame = read_live(&mut self.connection, window), if !has_output && self.buffered.is_empty() => {
                    self.process(frame?).await?;
                }
                _ = std::future::ready(()), if !has_output && !self.buffered.is_empty() => {
                    if let Some(frame) = self.buffered.pop_front() {
                        self.buffered_bytes -= frame.len();
                        self.process(frame).await?;
                    }
                }
            }
        }
    }

    /// One wait while paused at a boundary: a pause or progress change, or
    /// one event read into the bounded buffer.
    async fn paused_wait(&mut self) -> Result<()> {
        let window = self.dead_peer_window();
        let open = self.connection.is_some();
        tokio::select! {
            biased;
            _ = self.pause.changed() => {}
            changed = progress_changed(&mut self.progress, &mut self.consumer_progress, &mut self.row_progress) => changed?,
            frame = read_live(&mut self.connection, window), if open => match frame {
                Ok(frame) => self.hold(frame).await,
                Err(error) if errors::is_transient(&error) => {
                    log::info!("MySQL replication connection ended while paused; it reopens on resume: {error:#}");
                    self.close_quietly().await;
                }
                Err(error) => return Err(error),
            },
        }
        Ok(())
    }

    /// Buffers an event read while paused. Heartbeats are discarded; when
    /// the buffer would exceed `maxProtocolBytes`, the connection is closed
    /// deliberately and the buffer dropped, so Resume reopens at the last
    /// emitted transaction instead of holding unbounded memory.
    async fn hold(&mut self, frame: Bytes) {
        if frame.get(4) == Some(&HEARTBEAT_EVENT) {
            return;
        }
        if self.buffered_bytes + frame.len() > self.config.max_protocol_bytes.get() {
            log::info!(
                "MySQL pause buffer is full; closing the replication connection until resume"
            );
            self.buffered.clear();
            self.buffered_bytes = 0;
            // Buffered frames were never decoded; reopening starts at the
            // decoder's last completed transaction.
            self.close_quietly().await;
            return;
        }
        self.buffered_bytes += frame.len();
        self.buffered.push_back(frame);
    }

    async fn process(&mut self, frame: Bytes) -> Result<()> {
        if let Some(table) = self.decoder.new_table(&frame)? {
            self.refresh_table(table).await?;
        }
        if let Some(tables) = self.decoder.pending_inspection(&frame)? {
            self.inspect(&frame, tables).await?;
        }
        for envelope in self.decoder.decode(&frame)? {
            if self.row_progress.is_some() {
                anyhow::ensure!(
                    self.emission.is_none(),
                    "MySQL row emission overlaps another transaction"
                );
                let emission = rows::Emission::new(
                    envelope,
                    self.source.clone(),
                    self.stream.clone(),
                    self.sequence.clone(),
                    self.limits,
                )?;
                if !emission.is_empty() {
                    self.emission = Some(emission);
                } else if self.last_emission.elapsed() >= self.idle_progress {
                    // A transaction without selected rows: while the selected
                    // tables are quiet, commit its end so cursors follow
                    // binlog retention.
                    self.output.push_back(emission.progress_record()?);
                }
            } else {
                self.output.push_back(envelope);
            }
        }
        Ok(())
    }

    /// Reads the current catalog of `tables` through a separate connection.
    async fn read_catalog(&self, tables: &[String]) -> Result<catalog::Inspection> {
        let mut config = self.config.clone();
        config.tables = tables.to_vec();
        config.table_keys.retain(|key| tables.contains(&key.table));
        let mut connection = Connection::connect(
            &config.connection,
            config.max_protocol_bytes.get(),
            config.timeout(),
        )
        .await?;
        let read = catalog::inspect_tables(&mut connection, &config, self.profile).await;
        let close = connection.close().await;
        let found = match (read, close) {
            (Ok(value), Ok(())) => value,
            (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
            (Err(error), Err(cleanup)) => {
                return Err(OperationFailures::new(
                    "MySQL table inspection and cleanup failed",
                    vec![error, cleanup],
                )
                .into())
            }
        };
        if found.server != self.server {
            return Err(errors::unsupported(
                "MySQL table inspection reached another server",
            ));
        }
        Ok(found)
    }

    /// Re-reads selected tables named by a schema statement and applies the
    /// DDL policy.
    async fn inspect(&mut self, frame: &[u8], tables: Vec<String>) -> Result<()> {
        let found = match self.read_catalog(&tables).await {
            Ok(found) => found,
            Err(error) if errors::is_transient(&error) => return Err(error),
            Err(error) if errors::is_permanent_source_failure(&error) => return Err(error),
            Err(error) => {
                return Err(errors::schema_changed(format!(
                    "selected tables {} can no longer be captured: {error:#}",
                    tables.join(", ")
                )))
            }
        };
        self.decoder
            .apply_inspection(frame, &tables, found.tables)?;
        log::info!(
            "MySQL schema statement on selected tables {} kept their keys; continuing",
            tables.join(", ")
        );
        self.capture_schema()
    }

    fn capture_schema(&self) -> Result<()> {
        if self.positional {
            self.schema.capture_positional(self.decoder.catalog())
        } else {
            self.schema.capture(self.decoder.catalog())
        }
    }

    /// Adds a table that appeared in an untracked database-wide row source,
    /// or skips it when it cannot be captured.
    async fn refresh_table(&mut self, table: String) -> Result<()> {
        let found = match self.read_catalog(std::slice::from_ref(&table)).await {
            Ok(found) => found,
            Err(error) if errors::is_transient(&error) => return Err(error),
            Err(error) => {
                log::warn!("Native MySQL skips new table {table}: {error:#}");
                self.decoder.skip_table(table);
                return Ok(());
            }
        };
        let mut tables = found.tables;
        match tables.remove(&table) {
            Some(columns) => self.decoder.include_table(table, columns)?,
            None => {
                log::warn!("Native MySQL skips new table {table}: it cannot be captured");
                self.decoder.skip_table(table);
                return Ok(());
            }
        }
        self.capture_schema()
    }
}

async fn progress_changed(
    progress: &mut Option<Box<dyn SourceProgressUpdates>>,
    consumers: &mut Option<ConsumerUpdates>,
    rows: &mut Option<rows::Updates>,
) -> Result<()> {
    if let Some(rows) = rows {
        anyhow::ensure!(
            progress.is_none() && consumers.is_none(),
            "conflicting MySQL row progress bindings"
        );
        return rows.changed().await;
    }
    match (progress, consumers) {
        (Some(progress), None) => progress.changed().await,
        (None, Some(consumers)) => consumers.changed().await,
        (None, None) => std::future::pending().await,
        (Some(_), Some(_)) => anyhow::bail!("MySQL has conflicting progress subscriptions"),
    }
}
