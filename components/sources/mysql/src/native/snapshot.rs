// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    binlog::{self, Position},
    catalog,
    connection::{Connection, MySqlServerError},
    source, MySqlConfig, MySqlStartPosition,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{interface::FailureMode, models::SourceChange};
use drasi_lib::{
    computation::v1::*,
    context::workers::{join_owned_worker_gracefully, spawn_owned_worker, WorkerCompletion},
    error::OperationFailures,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch, RwLock},
    task::JoinHandle,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Handover {
    version: u32,
    scope: [u8; 32],
    position: Option<Position>,
    completed: bool,
}

struct Run {
    shutdown: watch::Sender<bool>,
}

struct Shared {
    worker: RwLock<Option<JoinHandle<Result<Option<Handover>>>>>,
    run: Mutex<Option<Arc<Run>>>,
    handover: Mutex<Option<Handover>>,
    prepared: Mutex<bool>,
    lifecycle: tokio::sync::Mutex<()>,
    timeout: Duration,
}

impl Shared {
    fn current(&self) -> Result<Option<Handover>> {
        Ok(self
            .handover
            .lock()
            .map_err(|_| anyhow::anyhow!("MySQL handover poisoned"))?
            .clone())
    }
    fn store(&self, value: Handover) -> Result<()> {
        *self
            .handover
            .lock()
            .map_err(|_| anyhow::anyhow!("MySQL handover poisoned"))? = Some(value);
        Ok(())
    }
    fn ensure_run(&self, run: &Arc<Run>) -> Result<()> {
        anyhow::ensure!(
            self.run
                .lock()
                .map_err(|_| anyhow::anyhow!("MySQL snapshot run poisoned"))?
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, run)),
            "retired MySQL snapshot stream"
        );
        Ok(())
    }
    async fn join(&self) -> Result<Option<Handover>> {
        let mut worker = self.worker.write().await;
        let result = join_owned_worker_gracefully(&mut worker, self.timeout).await;
        if worker.is_none() {
            self.run
                .lock()
                .map_err(|_| anyhow::anyhow!("MySQL snapshot run poisoned"))?
                .take();
        }
        match result? {
            WorkerCompletion::Absent => Ok(None),
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Cancelled => anyhow::bail!("MySQL snapshot worker aborted"),
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.get_mut() {
            worker.abort();
            log::warn!("MySQL snapshot dropped without awaited cleanup");
        }
    }
}

/// Query-scoped initial loading paired with a native MySQL source. Locking is
/// explicitly enabled by `MySqlStartPosition::Snapshot`, never by ordinary start.
pub struct MySqlSnapshot {
    source: ComponentId,
    stream: StreamId,
    config: MySqlConfig,
    progress: SourceProgressReader,
    shared: Arc<Shared>,
}

impl MySqlSnapshot {
    pub fn new(
        source: ComponentId,
        stream: StreamId,
        config: MySqlConfig,
        progress: impl Into<SourceProgressReader>,
    ) -> Result<Self> {
        config.validate()?;
        anyhow::ensure!(
            matches!(config.start, MySqlStartPosition::Snapshot { .. }),
            "MySQL bootstrap requires explicit snapshot locking mode"
        );
        let timeout = config.timeout();
        Ok(Self {
            source,
            stream,
            config,
            progress: progress.into(),
            shared: Arc::new(Shared {
                worker: RwLock::new(None),
                run: Mutex::new(None),
                handover: Mutex::new(None),
                prepared: Mutex::new(false),
                lifecycle: tokio::sync::Mutex::new(()),
                timeout,
            }),
        })
    }

    pub(super) fn validate_source(
        &self,
        source: &ComponentId,
        stream: &StreamId,
        config: &MySqlConfig,
        progress: &SourceProgressReader,
    ) -> Result<()> {
        anyhow::ensure!(
            &self.source == source
                && &self.stream == stream
                && self.progress.same_owner(progress)
                && serde_json::to_value(&self.config)? == serde_json::to_value(config)?,
            "MySQL snapshot does not match its source and actual consumer"
        );
        Ok(())
    }

    fn scope(&self) -> Result<[u8; 32]> {
        Ok(Sha256::digest(serde_json::to_vec(&(
            "mysql-snapshot-v1",
            &self.config.connection.database,
            &self.config.tables,
            self.source.as_str(),
            self.stream.as_str(),
            self.progress.graph_id(),
            self.progress.component_id().as_str(),
        ))?)
        .into())
    }

    pub(super) fn validate_live(
        &self,
        progress: &SourceProgressSnapshot,
        binding: [u8; 32],
    ) -> Result<()> {
        let handover = self
            .shared
            .current()?
            .context("MySQL snapshot has not been prepared")?;
        anyhow::ensure!(
            handover.completed && progress.bootstrap_complete,
            "MySQL snapshot is incomplete"
        );
        let boundary = handover
            .position
            .context("MySQL snapshot has no boundary")?;
        let checkpoint = source::checkpoint(progress, &self.source, binding)?
            .context("MySQL snapshot has no committed checkpoint")?;
        anyhow::ensure!(
            boundary.binding == binding
                && checkpoint.sequence()? >= boundary.sequence()?
                && (checkpoint.version != 2 || checkpoint == boundary),
            "MySQL snapshot boundary changed"
        );
        Ok(())
    }
}

struct Reader {
    receiver: mpsc::Receiver<ChangeEnvelope>,
    shared: Arc<Shared>,
    run: Arc<Run>,
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.run.shutdown.send_replace(true);
    }
}

#[async_trait]
impl ComputationBootstrapProvider for MySqlSnapshot {
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        let _guard = self.shared.lifecycle.lock().await;
        anyhow::ensure!(
            self.shared.worker.read().await.is_none(),
            "MySQL snapshot requires awaited cleanup"
        );
        state.durability().require(FailureMode::ProcessRestart)?;
        let progress = self.progress.snapshot()?;
        anyhow::ensure!(
            progress.recovered && progress.persistent && progress.failure.is_none(),
            "MySQL snapshot requires recovered persistent consumer progress"
        );
        let handover = state
            .read()
            .await?
            .map(|bytes| serde_json::from_slice::<Handover>(&bytes))
            .transpose()?;
        if let Some(value) = &handover {
            anyhow::ensure!(
                value.version == 1
                    && value.scope == self.scope()?
                    && value.completed == value.position.is_some()
                    && value.completed == progress.bootstrap_complete,
                "MySQL snapshot ownership or completion state changed"
            );
            if let Some(boundary) = &value.position {
                anyhow::ensure!(
                    boundary.version == 2,
                    "invalid MySQL snapshot boundary kind"
                );
                let checkpoint = source::checkpoint(&progress, &self.source, boundary.binding)?
                    .context("MySQL completed snapshot has no owner checkpoint")?;
                anyhow::ensure!(
                    checkpoint.sequence()? >= boundary.sequence()?
                        && (checkpoint.version != 2 || checkpoint == *boundary),
                    "MySQL snapshot checkpoint predates its boundary"
                );
            } else {
                anyhow::ensure!(
                    !progress
                        .checkpoints
                        .contains_key(&SourceProgressKey::Source(self.source.to_string())),
                    "unfinished MySQL snapshot has committed source progress"
                );
            }
        } else {
            anyhow::ensure!(
                !progress.bootstrap_complete
                    && !progress
                        .checkpoints
                        .contains_key(&SourceProgressKey::Source(self.source.to_string())),
                "MySQL snapshot ownership metadata is missing"
            );
        }
        *self
            .shared
            .handover
            .lock()
            .map_err(|_| anyhow::anyhow!("MySQL handover poisoned"))? = handover;
        *self
            .shared
            .prepared
            .lock()
            .map_err(|_| anyhow::anyhow!("MySQL preparation poisoned"))? = true;
        Ok(BootstrapPreparation::Ready)
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("MySQL snapshot requires query-owned initialization state")
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        let _guard = self.shared.lifecycle.lock().await;
        anyhow::ensure!(
            self.shared.worker.read().await.is_none(),
            "MySQL snapshot requires awaited cleanup"
        );
        {
            let mut prepared = self
                .shared
                .prepared
                .lock()
                .map_err(|_| anyhow::anyhow!("MySQL preparation poisoned"))?;
            anyhow::ensure!(*prepared, "MySQL snapshot requires preparation");
            *prepared = false;
        }
        anyhow::ensure!(
            self.shared.current()?.is_none_or(|value| !value.completed),
            "completed MySQL initialization requires explicit retirement"
        );
        let handover = Handover {
            version: 1,
            scope: self.scope()?,
            position: None,
            completed: false,
        };
        state
            .write(Bytes::from(serde_json::to_vec(&handover)?))
            .await?;
        self.shared.store(handover.clone())?;
        let run = Arc::new(Run {
            shutdown: watch::channel(false).0,
        });
        *self
            .shared
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("MySQL snapshot run poisoned"))? = Some(run.clone());
        let (outputs, receiver) = mpsc::channel(1);
        let (ready, started) = oneshot::channel();
        let mut work = Work {
            source: self.source.clone(),
            stream: self.stream.clone(),
            config: self.config.clone(),
            owner: self.progress.clone(),
            handover,
            ready: Some(ready),
            control: None,
            sessions: Vec::new(),
            server: None,
            killed: BTreeSet::new(),
        };
        let mut shutdown = run.shutdown.subscribe();
        spawn_owned_worker(&self.shared.worker, async move {
            let result = tokio::select! {
                biased;
                _ = shutdown.wait_for(|stop| *stop) => Ok(None),
                result = work.run(&outputs) => result.map(Some),
            };
            work.ready.take();
            drop(outputs);
            let cleanup = work.cleanup().await;
            match (result, cleanup) {
                (Ok(value), Ok(())) => Ok(value),
                (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => Err(OperationFailures::new(
                    "MySQL snapshot and cleanup failed",
                    vec![error, cleanup],
                )
                .into()),
            }
        })
        .await?;
        // This guard also cancels startup if the caller drops its future.
        let reader = Reader {
            receiver,
            shared: self.shared.clone(),
            run,
        };
        if started.await.is_err() {
            self.shared.join().await?;
            anyhow::bail!("MySQL snapshot startup was cancelled");
        }
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures_util::stream::try_unfold(
                reader,
                |mut reader| async move {
                    reader.shared.ensure_run(&reader.run)?;
                    if let Some(change) = reader.receiver.recv().await {
                        reader.shared.ensure_run(&reader.run)?;
                        return Ok(Some((change, reader)));
                    }
                    let _guard = reader.shared.lifecycle.lock().await;
                    reader.shared.ensure_run(&reader.run)?;
                    let handover = reader
                        .shared
                        .join()
                        .await?
                        .context("MySQL snapshot was cancelled")?;
                    reader.shared.store(handover)?;
                    Ok(None)
                },
            )),
            watermarks: Vec::new(),
        })
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        let handover = self.shared.current()?.context("missing MySQL handover")?;
        anyhow::ensure!(handover.completed, "MySQL snapshot stream did not complete");
        let position = handover
            .position
            .context("missing MySQL snapshot boundary")?;
        Ok(vec![BootstrapWatermark {
            stream: self.stream.clone(),
            source_id: Some(self.source.to_string()),
            sequence: position.sequence()?,
            position: Some(Bytes::from(serde_json::to_vec(&position)?)),
        }])
    }
    fn completion_state(&self) -> Result<Option<Bytes>> {
        let handover = self.shared.current()?.context("missing MySQL handover")?;
        anyhow::ensure!(handover.completed, "MySQL snapshot is incomplete");
        Ok(Some(Bytes::from(serde_json::to_vec(&handover)?)))
    }
    async fn stop(&self) -> Result<()> {
        let _guard = self.shared.lifecycle.lock().await;
        if let Some(run) = self
            .shared
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("MySQL snapshot run poisoned"))?
            .as_ref()
        {
            run.shutdown.send_replace(true);
        }
        *self
            .shared
            .prepared
            .lock()
            .map_err(|_| anyhow::anyhow!("MySQL preparation poisoned"))? = false;
        self.shared.join().await?;
        Ok(())
    }
}

struct Work {
    source: ComponentId,
    stream: StreamId,
    config: MySqlConfig,
    owner: SourceProgressReader,
    handover: Handover,
    ready: Option<oneshot::Sender<()>>,
    control: Option<Connection>,
    sessions: Vec<Connection>,
    server: Option<String>,
    killed: BTreeSet<u32>,
}

impl Work {
    async fn connect(&self) -> Result<Connection> {
        Connection::connect(
            &self.config.connection,
            self.config.max_protocol_bytes.get(),
            self.config.timeout(),
        )
        .await
    }
    async fn server(connection: &mut Connection) -> Result<String> {
        let rows = connection.query("SELECT @@server_uuid").await?;
        let [row] = rows.values.as_slice() else {
            anyhow::bail!("missing MySQL server identity");
        };
        Ok(catalog::field(row, 0)?.to_string())
    }
    async fn run(&mut self, outputs: &mpsc::Sender<ChangeEnvelope>) -> Result<Handover> {
        self.control = Some(self.connect().await?);
        self.server = Some(
            Self::server(
                self.control
                    .as_mut()
                    .context("missing MySQL control connection")?,
            )
            .await?,
        );
        for _ in 0..2 {
            let mut connection = self.connect().await?;
            anyhow::ensure!(
                Some(Self::server(&mut connection).await?) == self.server,
                "MySQL snapshot connections reached different servers"
            );
            self.sessions.push(connection);
        }
        let MySqlStartPosition::Snapshot { lock_timeout_ms } = self.config.start else {
            anyhow::bail!("snapshot locking was not enabled");
        };
        let (tables, file, boundary) = tokio::time::timeout(
            Duration::from_millis(lock_timeout_ms.get()),
            self.establish(lock_timeout_ms.get()),
        )
        .await
        .context("MySQL snapshot lock deadline expired")??;
        self.ready
            .take()
            .context("missing MySQL readiness sender")?
            .send(())
            .map_err(|_| anyhow::anyhow!("MySQL snapshot caller cancelled"))?;
        let binding = source::source_binding(
            &self.config,
            self.server.as_deref().context("missing server")?,
            &tables,
            &self.source,
            &self.stream,
            Some(&self.owner),
        )?;
        let (_, digest) = tokio::time::timeout(
            self.config.timeout(),
            binlog::Binlog::snapshot_history(&mut self.sessions[0], &self.config, &file, boundary),
        )
        .await
        .context("MySQL snapshot history verification timed out")??;
        let decoder = crate::decoder::MySqlDecoder::new(self.source.to_string(), &[]);
        let mut sequence = 0u64;
        for (table, expected) in &tables {
            let keys = expected
                .iter()
                .filter(|column| column.key)
                .map(|column| column.name.clone())
                .collect::<Vec<_>>();
            let mut cursor = self.sessions[1]
                .cursor(&format!("SELECT * FROM `{table}`"))
                .await?;
            catalog::validate_snapshot_columns(expected, &cursor.columns)?;
            while let Some(values) = self.sessions[1].fetch(&mut cursor).await? {
                anyhow::ensure!(
                    expected
                        .iter()
                        .zip(&values)
                        .all(|(column, value)| column.nullable
                            || !matches!(value, mysql_common::Value::NULL)),
                    "NULL in non-null MySQL snapshot column"
                );
                let element = decoder.native_snapshot_element(
                    &self.config.connection.database,
                    table,
                    &cursor.columns,
                    values,
                    &keys,
                    self.config.max_protocol_bytes.get(),
                )?;
                sequence = sequence
                    .checked_add(1)
                    .context("MySQL snapshot sequence exhausted")?;
                let envelope = GraphChangeCodec::encode_change(
                    SourceChange::Insert { element },
                    self.stream.clone(),
                    sequence,
                    None,
                )?;
                anyhow::ensure!(
                    binlog::envelope_bytes(&envelope)? <= self.config.max_protocol_bytes.get(),
                    "MySQL decoded snapshot row exceeds protocol byte limit"
                );
                outputs
                    .send(envelope)
                    .await
                    .context("MySQL snapshot reader closed")?;
            }
            self.sessions[1].close_cursor(cursor).await?;
        }
        self.sessions[1].execute("COMMIT").await?;
        let mut handover = self.handover.clone();
        handover.position = Some(Position {
            version: 2,
            binding,
            file,
            start: 4,
            end: boundary,
            digest,
        });
        handover.completed = true;
        Ok(handover)
    }

    async fn establish(&mut self, lock_ms: u64) -> Result<(catalog::Tables, String, u32)> {
        for connection in &mut self.sessions {
            connection
                .execute(&format!(
                    "SET SESSION lock_wait_timeout={}",
                    lock_ms.div_ceil(1000)
                ))
                .await?;
        }
        self.sessions[0]
            .execute("FLUSH TABLES WITH READ LOCK")
            .await?;
        self.sessions[1]
            .execute("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .await?;
        self.sessions[1]
            .execute("SET SESSION time_zone='+00:00'")
            .await?;
        self.sessions[1].execute("SET SESSION sql_mode=''").await?;
        self.sessions[1]
            .execute("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")
            .await?;
        // Hold each table's metadata lock until the read transaction completes,
        // so DDL cannot change a table halfway through the initial scan.
        for table in &self.config.tables {
            self.sessions[1]
                .query(&format!("SELECT * FROM `{table}` LIMIT 0"))
                .await?;
        }
        let (_, tables) = catalog::inspect(&mut self.sessions[1], &self.config).await?;
        let sql = if self.sessions[0].version >= (8, 4, 0) {
            "SHOW BINARY LOG STATUS"
        } else {
            "SHOW MASTER STATUS"
        };
        let rows = self.sessions[0].query(sql).await?;
        let [row] = rows.values.as_slice() else {
            anyhow::bail!("MySQL binary log is unavailable");
        };
        let file = catalog::field(row, 0)?.to_string();
        super::file_number(&file)?;
        let boundary = catalog::field(row, 1)?.parse()?;
        self.sessions[0].execute("UNLOCK TABLES").await?;
        Ok((tables, file, boundary))
    }

    async fn cleanup(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        // A closed socket alone does not prove a blocked server statement ended.
        // Retain this worker through server-observed retirement; stop timeouts keep
        // its join handle, rather than abandoning a possibly pending global lock.
        while !self.sessions.is_empty() {
            let result = self.retire().await;
            if let Err(error) = result {
                log::error!("MySQL snapshot cleanup pending: {error:#}");
                if failures.is_empty() {
                    failures.push(error);
                }
                if let Some(control) = self.control.take() {
                    if let Err(error) = control.close().await {
                        log::error!("MySQL control close failed: {error:#}");
                    }
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        if let Some(control) = self.control.take() {
            if let Err(error) = control.close().await {
                failures.push(error);
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(
                OperationFailures::new("MySQL snapshot cleanup encountered failures", failures)
                    .into(),
            )
        }
    }

    async fn retire(&mut self) -> Result<()> {
        if self.control.is_none() {
            let mut control = self.connect().await?;
            anyhow::ensure!(
                Some(Self::server(&mut control).await?) == self.server,
                "cannot retire snapshot connections on a different MySQL server"
            );
            self.control = Some(control);
        }
        let control = self
            .control
            .as_mut()
            .context("missing MySQL cleanup connection")?;
        for connection in &self.sessions {
            if self.killed.contains(&connection.connection_id) {
                continue;
            }
            if let Err(error) = control
                .execute(&format!("KILL CONNECTION {}", connection.connection_id))
                .await
            {
                if error
                    .downcast_ref::<MySqlServerError>()
                    .is_none_or(|error| error.code != 1094)
                {
                    return Err(error);
                }
            }
            self.killed.insert(connection.connection_id);
        }
        for connection in &self.sessions {
            let rows = control
                .query(&format!(
                    "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE ID={}",
                    connection.connection_id
                ))
                .await?;
            anyhow::ensure!(rows.values.len() == 1, "invalid MySQL retirement result");
            if catalog::field(&rows.values[0], 0)? != "0" {
                tokio::time::sleep(Duration::from_millis(10)).await;
                return Ok(());
            }
        }
        while let Some(connection) = self.sessions.pop() {
            connection.close().await?;
        }
        Ok(())
    }
}
