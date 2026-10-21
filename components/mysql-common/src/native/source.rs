// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    binlog::{Binlog, Position, Start},
    catalog::{self, field},
    connection::Connection,
    progress::{ConsumerBinding, ConsumerUpdates, Positions, Starting},
    rows, MySqlConfig, MySqlOutput, MySqlRetention, MySqlStartPosition,
};
use anyhow::{Context, Result};
use bytes::Bytes;
use drasi_computation_contracts::{
    workers::{join_owned_worker_gracefully, spawn_owned_worker, WorkerCompletion},
    ComponentDescriptor, ComponentId, ConsumerProgressSnapshot, GraphChangeCodec,
    InputPositionPolicy, OperationFailures, OutputEnvelope, PipeRequirements, PortDescriptor,
    PortDirection, PortId, SourceProgressKey, SourceProgressSnapshot, SourceProgressUpdates,
    SourceProgressView, SourceTransactionCodec, SourceTransactionError, StreamId,
};
use drasi_core_types::interface::StorageDurability;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use tokio::{
    sync::{mpsc, watch, RwLock},
    task::JoinHandle,
};

pub struct MySqlSource {
    descriptor: ComponentDescriptor,
    stream: StreamId,
    config: MySqlConfig,
    progress: Option<SourceProgressView>,
    consumers: Option<ConsumerBinding>,
    position_policy: Option<Arc<InputPositionPolicy>>,
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    shutdown: watch::Sender<bool>,
    worker: RwLock<Option<JoinHandle<Result<()>>>>,
    cleanup_required: bool,
    sequence: Arc<AtomicU64>,
    snapshot: Option<Arc<super::MySqlSnapshot>>,
    consumer_snapshots: BTreeMap<ComponentId, Arc<super::MySqlSnapshot>>,
    published_snapshot: bool,
    rows: Option<rows::Options>,
    schema: Arc<super::schema::Publication>,
    positional: bool,
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
                    SourceProgressKey::Source(id.to_string()),
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
        Ok(Self {
            descriptor: Self::describe(id, config.output)?,
            stream,
            config,
            progress,
            consumers,
            position_policy,
            receiver: None,
            shutdown: watch::channel(false).0,
            worker: RwLock::new(None),
            cleanup_required: false,
            sequence: Arc::new(AtomicU64::new(0)),
            snapshot,
            consumer_snapshots: BTreeMap::new(),
            schema: Arc::new(super::schema::Publication::default()),
            published_snapshot: false,
            rows,
            positional: false,
        })
    }

    pub fn describe(id: ComponentId, output: MySqlOutput) -> Result<ComponentDescriptor> {
        let schema = match output {
            MySqlOutput::Changes => GraphChangeCodec::schema(),
            MySqlOutput::Transactions => SourceTransactionCodec::schema(),
        };
        Ok(ComponentDescriptor::try_new(
            id,
            vec![PortDescriptor::new(
                PortId::try_new("out")?,
                PortDirection::Output,
                schema.descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?)
    }

    async fn prepare(&self, snapshot: Starting) -> Result<Worker> {
        let progress = self
            .progress
            .as_ref()
            .map(SourceProgressView::subscribe)
            .transpose()?;
        let mut connection = Connection::connect(
            &self.config.connection,
            self.config.max_protocol_bytes.get(),
            self.config.timeout(),
        )
        .await?;
        let prepared = async {
            let (decoder, binding, confirmed, server) =
                self.prepare_connection(&mut connection, &snapshot).await?;
            let consumer_progress = match (&self.consumers, &snapshot) {
                (Some(consumers), Starting::Consumers(snapshot)) if self.rows.is_none() => Some(
                    ConsumerUpdates::new(consumers, snapshot, self.descriptor.id(), binding)?,
                ),
                (None, _) | (Some(_), Starting::Consumers(_)) => None,
                _ => anyhow::bail!("MySQL consumer progress initialization is missing"),
            };
            let row_progress = match (&self.consumers, &snapshot, self.rows) {
                (Some(consumers), Starting::Consumers(snapshot), Some(options)) => {
                    Some(rows::Updates::new(
                        consumers,
                        snapshot,
                        self.descriptor.id(),
                        binding,
                        options,
                    )?)
                }
                _ => None,
            };
            Ok((
                decoder,
                binding,
                confirmed,
                consumer_progress,
                row_progress,
                server,
            ))
        }
        .await;
        match prepared {
            Ok((decoder, binding, confirmed, consumer_progress, row_progress, server)) => {
                Ok(Worker {
                    config: self.config.clone(),
                    positional: self.positional,
                    schema: self.schema.clone(),
                    server,
                    connection,
                    decoder,
                    binding,
                    confirmed,
                    source: self.descriptor.id().clone(),
                    progress,
                    consumer_progress,
                    row_progress,
                    row_admitted: self.sequence.load(Ordering::Relaxed),
                    stream: self.stream.clone(),
                    sequence: self.sequence.clone(),
                    limits: self.config.transactions,
                    generation: match snapshot {
                        Starting::Single(snapshot) => snapshot.reset_generation,
                        _ => 0,
                    },
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
        connection: &mut Connection,
        starting: &Starting,
    ) -> Result<(Binlog, [u8; 32], u64, String)> {
        let (server, tables) = if self.positional {
            catalog::inspect_positional(connection, &self.config).await?
        } else if self.config.output == MySqlOutput::Changes {
            catalog::inspect_rows(connection, &self.config).await?
        } else {
            catalog::inspect(connection, &self.config).await?
        };
        if self.positional {
            self.schema.capture_positional(&tables)?;
        } else {
            self.schema.capture(&tables)?;
        }
        let mut binding = match &self.consumers {
            Some(consumers) if self.rows.is_some() => row_source_binding(
                &self.config,
                &server,
                &tables,
                self.descriptor.id(),
                &self.stream,
                consumers.graph_id(),
            )?,
            Some(consumers) => shared_source_binding(
                &self.config,
                &server,
                &tables,
                self.descriptor.id(),
                &self.stream,
                consumers.graph_id(),
            )?,
            None => source_binding(
                &self.config,
                &server,
                &tables,
                self.descriptor.id(),
                &self.stream,
                self.progress.as_ref(),
            )?,
        };
        if self.positional {
            binding =
                Sha256::digest(serde_json::to_vec(&("mysql-positional-rows-v1", binding))?).into();
        }
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
        let anchor = if self.rows.is_some() {
            let mut missing = 0;
            let mut initialized = 0;
            for state in starting.states() {
                match rows::checkpoint(state, self.descriptor.id(), binding)? {
                    Some(_) => initialized += 1,
                    None => missing += 1,
                }
            }
            anyhow::ensure!(
                missing == 0 || initialized == 0 || matches!(self.config.start, MySqlStartPosition::Position { .. }),
                "mixed MySQL row group has uninitialized consumers; an explicit snapshot or retained initial position is required"
            );
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
                || anchor.as_ref().is_none_or(|position| position.version == 1),
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
        let gtid = match (&anchor, &self.config.start) {
            (None, MySqlStartPosition::Gtid { set }) => Some(set.as_str()),
            _ => None,
        };
        let (file, offset) = match &anchor {
            Some(position) => (
                position.file.clone(),
                if position.version == 2 || row_anchor.as_ref().is_some_and(|row| row.completed()) {
                    position.end
                } else {
                    position.start
                },
            ),
            None => match &self.config.start {
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
        let rows = connection.query("SHOW BINARY LOGS").await?;
        let mut retained = false;
        for row in &rows.values {
            if field(row, 0)? == file {
                retained = u64::from(offset) <= field(row, 1)?.parse::<u64>()?;
            }
        }
        if !retained && gtid.is_none() {
            return Err(SourceError::PositionUnavailable {
                source_id: self.descriptor.id().as_str().into(),
                requested: Bytes::from(serde_json::to_vec(&MySqlStartPosition::Position {
                    file,
                    position: offset,
                })?),
                earliest_available: None,
            }
            .into());
        }
        if self.positional {
            self.verify_positional_profile(&server, &rows, &file, offset, gtid.is_some())
                .await?;
            let (current_server, current_tables) =
                catalog::inspect_positional(connection, &self.config).await?;
            anyhow::ensure!(
                current_server == server
                    && serde_json::to_vec(&current_tables)? == serde_json::to_vec(&tables)?,
                "positional MySQL catalog changed during startup"
            );
        }
        if let Starting::Consumers(group) = starting {
            self.verify_consumer_history(&server, &tables, &rows, group, binding)
                .await?;
        }
        connection
            .execute(&format!(
                "SET @master_heartbeat_period={}",
                self.config.connection.heartbeat_interval_ms.get() * 1_000_000
            ))
            .await?;
        let history =
            if let Some(position) = anchor.as_ref().filter(|position| position.version == 2) {
                let (format, digest) = Binlog::snapshot_history(
                    connection,
                    &self.config,
                    &file,
                    offset,
                    super::binlog::HistoryMode::Live,
                )
                .await?;
                anyhow::ensure!(
                    digest == position.digest,
                    "MySQL snapshot history has different contents"
                );
                Some(format)
            } else {
                None
            };
        if history.is_none() {
            connection
                .execute("SET @master_binlog_checksum='CRC32'")
                .await?;
            if let Some(set) = gtid {
                connection
                    .start_gtid(self.config.connection.server_id.get(), set)
                    .await?;
            } else {
                connection
                    .start_binlog(self.config.connection.server_id.get(), &file, offset)
                    .await?;
            }
        }
        let mut decoder_config = self.config.clone();
        if self.rows.is_some() {
            decoder_config.output = MySqlOutput::Transactions;
        }
        let mut decoder = Binlog::new(Start {
            source: self.descriptor.id().clone(),
            stream: self.stream.clone(),
            config: decoder_config,
            tables,
            binding,
            file,
            offset,
            anchor: anchor.filter(|position| position.version == 1 && self.rows.is_none()),
            sequence: self.sequence.clone(),
        })?;
        if self.config.output == MySqlOutput::Changes {
            decoder = decoder.with_row_semantics();
        }
        if self.positional {
            decoder = decoder.with_positional_metadata();
        }
        if gtid.is_some() {
            decoder = decoder.with_gtid_start();
        }
        if let Some(format) = history {
            decoder.after_snapshot(format, confirmed);
            return Ok((decoder, binding, confirmed, server));
        }
        // A sent dump request is not evidence that the server accepted the cursor.
        for _ in 0..2 {
            let frame = connection.event().await?;
            anyhow::ensure!(
                decoder.decode(&frame)?.is_empty(),
                "MySQL data arrived before startup completed"
            );
            if decoder.ready() {
                return Ok((decoder, binding, confirmed, server));
            }
        }
        anyhow::bail!("MySQL did not provide its replication format during startup")
    }

    async fn verify_positional_profile(
        &self,
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
                &self.config.connection,
                self.config.max_protocol_bytes.get(),
                self.config.timeout(),
            )
            .await?;
            let proof = async {
                anyhow::ensure!(
                    super::session::server_id(&mut connection).await? == server,
                    "positional catalog history reached another MySQL server"
                );
                tokio::time::timeout(
                    self.config.timeout(),
                    Binlog::verify_positional_catalog_history(
                        &mut connection,
                        &self.config,
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

    async fn verify_consumer_history(
        &self,
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
            let mut retained = false;
            for row in &logs.values {
                if field(row, 0)? == position.file {
                    retained = u64::from(position.end) <= field(row, 1)?.parse::<u64>()?;
                }
            }
            if !retained {
                return Err(SourceError::PositionUnavailable {
                    source_id: self.descriptor.id().to_string(),
                    requested: Bytes::from(serde_json::to_vec(&position)?),
                    earliest_available: None,
                }
                .into());
            }
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
                &self.config.connection,
                self.config.max_protocol_bytes.get(),
                self.config.timeout(),
            )
            .await?;
            let verified = async {
                let identity = connection.query("SELECT @@server_uuid").await?;
                anyhow::ensure!(
                    identity.values.len() == 1 && field(&identity.values[0], 0)? == server,
                    "MySQL consumer history reached another server"
                );
                if position.version == 2 {
                    let (_, digest) = Binlog::snapshot_history(
                        &mut connection,
                        &self.config,
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
                    let mut config = self.config.clone();
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
                    })?;
                    if self.rows.is_some() {
                        decoder = decoder.with_row_semantics();
                    }
                    if self.positional {
                        decoder = decoder.with_positional_metadata();
                    }
                    while decoder.has_replay_anchor() {
                        anyhow::ensure!(
                            decoder.decode(&connection.event().await?)?.is_empty(),
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

    async fn join(&mut self) -> Result<()> {
        match join_owned_worker_gracefully(self.worker.get_mut(), self.config.timeout()).await? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Absent => Ok(()),
            WorkerCompletion::Cancelled => {
                anyhow::bail!("native MySQL worker was unexpectedly cancelled")
            }
        }
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
    pub fn configuration(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({"stream": self.stream.as_str(), "settings": self.config}))
    }
    pub async fn start(&mut self) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required,
            "MySQL source requires awaited cleanup before restart"
        );
        self.cleanup_required = true;
        self.schema.clear()?;
        let snapshot = match (&self.progress, &self.consumers) {
            (Some(progress), None) => Starting::Single(progress.wait_ready().await?),
            (None, Some(consumers)) => {
                Starting::Consumers(consumers.read_only().wait_ready().await?)
            }
            (None, None) => Starting::Untracked,
            (Some(_), Some(_)) => anyhow::bail!("MySQL has conflicting replay progress bindings"),
        };
        if let (Some(consumers), Starting::Consumers(group)) = (&self.consumers, &snapshot) {
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
        let mut worker = tokio::time::timeout(self.config.timeout(), self.prepare(snapshot))
            .await
            .context("MySQL startup timed out")??;
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
            let cleanup = worker.connection.close().await;
            let result = match (result, cleanup) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => Err(OperationFailures::new(
                    "MySQL replication and socket cleanup failed",
                    vec![error, cleanup],
                )
                .into()),
            };
            if let Err(error) = &result {
                log::error!("Native MySQL replication stopped: {error:#}");
            }
            result
        })
        .await?;
        Ok(())
    }
    pub async fn stop(&mut self) -> Result<()> {
        self.shutdown.send_replace(true);
        self.join().await?;
        self.receiver = None;
        self.schema.clear()?;
        self.cleanup_required = false;
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
        self.join().await?;
        anyhow::bail!("native MySQL replication ended; stop and restart the source")
    }
}

impl Drop for MySqlSource {
    fn drop(&mut self) {
        self.schema.retire();
        self.shutdown.send_replace(true);
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
        .get(&SourceProgressKey::Source(source.as_str().into()))
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
        position.binding == binding && position.sequence()? == checkpoint.sequence,
        "MySQL checkpoint binding or sequence mismatch"
    );
    Ok(Some(position))
}

pub fn source_binding(
    config: &MySqlConfig,
    server: &str,
    tables: &catalog::Tables,
    source: &ComponentId,
    stream: &StreamId,
    progress: Option<&SourceProgressView>,
) -> Result<[u8; 32]> {
    binding(
        config,
        server,
        tables,
        source,
        stream,
        (
            "mysql-native-v1",
            progress.map(|owner| (owner.graph_id(), owner.component_id().as_str())),
        ),
    )
}

pub fn shared_source_binding(
    config: &MySqlConfig,
    server: &str,
    tables: &catalog::Tables,
    source: &ComponentId,
    stream: &StreamId,
    graph: &str,
) -> Result<[u8; 32]> {
    binding(
        config,
        server,
        tables,
        source,
        stream,
        ("mysql-native-consumers-v1", Some((graph, source.as_str()))),
    )
}

pub fn row_source_binding(
    config: &MySqlConfig,
    server: &str,
    tables: &catalog::Tables,
    source: &ComponentId,
    stream: &StreamId,
    graph: &str,
) -> Result<[u8; 32]> {
    binding(
        config,
        server,
        tables,
        source,
        stream,
        (
            "mysql-native-consumer-rows-v1",
            Some((graph, source.as_str())),
        ),
    )
}

fn binding(
    config: &MySqlConfig,
    server: &str,
    tables: &catalog::Tables,
    source: &ComponentId,
    stream: &StreamId,
    scope: (&str, Option<(&str, &str)>),
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
    let mut hash = Sha256::new();
    hash.update(serde_json::to_vec(&(
        scope.0,
        server,
        &config.connection.database,
        tables,
        source.as_str(),
        stream.as_str(),
        start,
        scope.1,
    ))?);
    if !config.table_keys.is_empty() {
        hash.update(b"\0mysql-table-keys-v1\0");
        hash.update(serde_json::to_vec(&config.table_keys)?);
    }
    Ok(hash.finalize().into())
}

#[derive(Debug)]
pub enum SourceError {
    PositionUnavailable {
        source_id: String,
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
    config: MySqlConfig,
    positional: bool,
    schema: Arc<super::schema::Publication>,
    server: String,
    connection: Connection,
    decoder: Binlog,
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
}

impl Worker {
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

    async fn run(&mut self, sender: mpsc::Sender<OutputEnvelope>) -> Result<()> {
        loop {
            self.validate_progress()?;
            let deadline = self.decoder.deadline();
            tokio::select! {
                biased;
                changed = progress_changed(&mut self.progress, &mut self.consumer_progress, &mut self.row_progress) => changed?,
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Err(SourceTransactionError::Deadline.into()),
                frame = self.connection.event() => {
                    let frame = frame?;
                    if let Some(table) = self.decoder.new_table(&frame)? {
                        self.refresh_table(table).await?;
                    }
                    for envelope in self.decoder.decode(&frame)? {
                        if self.row_progress.is_some() {
                            let rows = rows::Emission::new(envelope, self.source.clone(), self.stream.clone(), self.sequence.clone(), self.limits)?;
                            for row in rows { self.send(row?, &sender).await?; }
                        } else {
                            self.send(envelope, &sender).await?;
                        }
                    }
                }
            }
        }
    }

    async fn refresh_table(&mut self, table: String) -> Result<()> {
        let mut config = self.config.clone();
        config.tables = vec![table.clone()];
        config.table_keys.retain(|key| key.table == table);
        config.validate()?;
        let mut connection = Connection::connect(
            &config.connection,
            config.max_protocol_bytes.get(),
            config.timeout(),
        )
        .await?;
        let read = if self.positional {
            catalog::inspect_positional(&mut connection, &config).await
        } else {
            catalog::inspect_rows(&mut connection, &config).await
        };
        let close = connection.close().await;
        let (server, mut tables) = match (read, close) {
            (Ok(value), Ok(())) => value,
            (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
            (Err(error), Err(cleanup)) => {
                return Err(OperationFailures::new(
                    "MySQL table discovery and cleanup failed",
                    vec![error, cleanup],
                )
                .into())
            }
        };
        anyhow::ensure!(
            server == self.server,
            "MySQL table discovery reached another server"
        );
        self.decoder.include_table(
            table.clone(),
            tables
                .remove(&table)
                .context("new MySQL table is missing")?,
        )?;
        if self.positional {
            self.schema.capture_positional(self.decoder.catalog())
        } else {
            self.schema.capture(self.decoder.catalog())
        }
    }

    async fn send(
        &mut self,
        envelope: OutputEnvelope,
        sender: &mpsc::Sender<OutputEnvelope>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                biased;
                changed = progress_changed(&mut self.progress, &mut self.consumer_progress, &mut self.row_progress) => {
                    changed?; self.validate_progress()?;
                }
                permit = sender.reserve() => {
                    let permit = permit.context("MySQL output receiver closed")?;
                    self.row_admitted = self.row_admitted.max(envelope.envelope.system().sequence());
                    permit.send(envelope);
                    return Ok(());
                }
            }
        }
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
