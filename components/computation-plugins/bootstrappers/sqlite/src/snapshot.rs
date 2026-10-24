// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Read-only file snapshots using the native SQLite source's identities.
//! The caller must order initialization before live writes; this provider does
//! not establish a source replay boundary. Use the source's paired snapshot for
//! coordinated persistent initialization.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    workers::{join_owned_worker_gracefully, spawn_owned_blocking_worker, WorkerCompletion},
    ChangeEnvelope, ComponentId, ComputationBootstrapSnapshot, GraphChangeCodec, StreamId,
};
use drasi_computation_plugin_sdk::{NativeBootstrapProvider, Scope};
use drasi_core_types::models::SourceChange;
use drasi_sqlite_common::{read_table, read_table_schemas, validate_selection, TableKeyConfig};
use rusqlite::{limits::Limit, Connection, ErrorCode, OpenFlags};
use serde::{Deserialize, Serialize};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    ops::ControlFlow,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{mpsc, watch, RwLock},
    task::JoinHandle,
    time::Instant,
};

#[cfg(test)]
mod tests;

/// Explicit bounds for a streamed file snapshot, not a whole-result row limit.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SqliteBootstrapConfig {
    pub path: PathBuf,
    /// Empty selects all ordinary application tables in the main database.
    pub tables: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub table_keys: Vec<TableKeyConfig>,
    pub max_tables: NonZeroUsize,
    /// Bounds schema metadata and each emitted row independently.
    pub max_bytes: NonZeroUsize,
    pub max_duration_ms: NonZeroU64,
    pub output_capacity: NonZeroUsize,
    pub shutdown_timeout_ms: NonZeroU64,
}

impl SqliteBootstrapConfig {
    fn validate(&self) -> Result<()> {
        anyhow::ensure!(
            !self.path.as_os_str().is_empty(),
            "SQLite snapshot needs a file path"
        );
        validate_selection(&self.tables, &self.table_keys)?;
        anyhow::ensure!(
            self.output_capacity.get() <= tokio::sync::Semaphore::MAX_PERMITS,
            "SQLite snapshot channel capacity exceeds Tokio's supported bound"
        );
        anyhow::ensure!(
            self.max_bytes.get() <= i32::MAX as usize,
            "SQLite snapshot byte limit must fit signed 32-bit lengths"
        );
        anyhow::ensure!(
            self.max_duration_ms.get() <= i32::MAX as u64,
            "SQLite snapshot duration exceeds its busy-timeout bound"
        );
        Instant::now()
            .checked_add(self.duration())
            .context("SQLite snapshot deadline is not representable")?;
        Instant::now()
            .checked_add(self.timeout())
            .context("SQLite snapshot cleanup deadline is not representable")?;
        Ok(())
    }

    fn duration(&self) -> Duration {
        Duration::from_millis(self.max_duration_ms.get())
    }

    fn timeout(&self) -> Duration {
        Duration::from_millis(self.shutdown_timeout_ms.get())
    }
}

struct SnapshotRun {
    shutdown: watch::Sender<bool>,
}

struct Shared {
    worker: RwLock<Option<JoinHandle<Result<bool>>>>,
    run: Mutex<Option<Arc<SnapshotRun>>>,
    lifecycle: tokio::sync::Mutex<()>,
    timeout: Duration,
}

impl Shared {
    fn ensure_run(&self, expected: &Arc<SnapshotRun>) -> Result<()> {
        anyhow::ensure!(
            self.run
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite file snapshot run poisoned"))?
                .as_ref()
                .is_some_and(|run| Arc::ptr_eq(run, expected))
                && !*expected.shutdown.borrow(),
            "SQLite file snapshot stream belongs to a stopped or retired worker"
        );
        Ok(())
    }

    fn cancel(&self) -> Result<()> {
        if let Some(run) = self
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite file snapshot run poisoned"))?
            .as_ref()
        {
            run.shutdown.send_replace(true);
        }
        Ok(())
    }

    async fn join(&self) -> Result<bool> {
        let mut worker = self.worker.write().await;
        let result = join_owned_worker_gracefully(&mut worker, self.timeout).await;
        if worker.is_none() {
            self.run
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite file snapshot run poisoned"))?
                .take();
        }
        match result? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Absent => Ok(false),
            WorkerCompletion::Cancelled => {
                anyhow::bail!("SQLite file snapshot worker was cancelled")
            }
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Err(error) = self.cancel() {
            log::error!("SQLite file snapshot cancellation failed: {error:#}");
        }
        if self.worker.get_mut().is_some() {
            log::warn!("SQLite file snapshot dropped without awaited cleanup");
        }
    }
}

/// A query-scoped, bounded, read-only snapshot. Each query needs its own provider.
pub struct SqliteBootstrapProvider {
    source: ComponentId,
    stream: StreamId,
    config: SqliteBootstrapConfig,
    shared: Arc<Shared>,
    pub(super) query_scope: Option<(Scope, ComponentId)>,
}

impl SqliteBootstrapProvider {
    pub fn new(
        source: ComponentId,
        stream: StreamId,
        config: SqliteBootstrapConfig,
    ) -> Result<Self> {
        config.validate()?;
        let timeout = config.timeout();
        Ok(Self {
            source,
            stream,
            config,
            query_scope: None,
            shared: Arc::new(Shared {
                worker: RwLock::new(None),
                run: Mutex::new(None),
                lifecycle: tokio::sync::Mutex::new(()),
                timeout,
            }),
        })
    }
}

struct Reader {
    receiver: mpsc::Receiver<ChangeEnvelope>,
    shared: Arc<Shared>,
    run: Arc<SnapshotRun>,
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.run.shutdown.send_replace(true);
    }
}

#[async_trait]
impl NativeBootstrapProvider for SqliteBootstrapProvider {
    fn validate_query_scope(
        &self,
        instance_id: &str,
        graph_id: &str,
        component_id: &ComponentId,
    ) -> Result<()> {
        if let Some((scope, query)) = &self.query_scope {
            anyhow::ensure!(
                scope.instance_id == instance_id
                    && scope.graph_id == graph_id
                    && query == component_id,
                "SQLite file snapshot belongs to another query"
            );
        }
        Ok(())
    }

    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let _lifecycle = self.shared.lifecycle.lock().await;
        anyhow::ensure!(
            self.shared.worker.read().await.is_none(),
            "SQLite file snapshot requires awaited cleanup before another snapshot"
        );
        let run = Arc::new(SnapshotRun {
            shutdown: watch::channel(false).0,
        });
        *self
            .shared
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite file snapshot run poisoned"))? = Some(run.clone());
        let (outputs, receiver) = mpsc::channel(self.config.output_capacity.get());
        let work = SnapshotWork {
            source: self.source.clone(),
            stream: self.stream.clone(),
            config: self.config.clone(),
            deadline: Instant::now()
                .checked_add(self.config.duration())
                .context("SQLite snapshot deadline is not representable")?,
            outputs,
            shutdown: run.shutdown.subscribe(),
        };
        spawn_owned_blocking_worker(&self.shared.worker, move || {
            let result = work.read();
            if let Err(error) = &result {
                log::error!("SQLite file snapshot failed: {error:#}");
            }
            result
        })
        .await?;
        let reader = Reader {
            receiver,
            shared: self.shared.clone(),
            run,
        };
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::try_unfold(
                reader,
                |mut reader| async move {
                    reader.shared.ensure_run(&reader.run)?;
                    if let Some(change) = reader.receiver.recv().await {
                        reader.shared.ensure_run(&reader.run)?;
                        return Ok(Some((change, reader)));
                    }
                    let _lifecycle = reader.shared.lifecycle.lock().await;
                    reader.shared.ensure_run(&reader.run)?;
                    anyhow::ensure!(
                        reader.shared.join().await?,
                        "SQLite file snapshot did not complete"
                    );
                    Ok(None)
                },
            )),
            watermarks: Vec::new(),
        })
    }

    async fn stop(&self) -> Result<()> {
        let _lifecycle = self.shared.lifecycle.lock().await;
        self.shared.cancel()?;
        self.shared.join().await?;
        Ok(())
    }
}

struct SnapshotWork {
    source: ComponentId,
    stream: StreamId,
    config: SqliteBootstrapConfig,
    deadline: Instant,
    outputs: mpsc::Sender<ChangeEnvelope>,
    shutdown: watch::Receiver<bool>,
}

impl SnapshotWork {
    fn read(mut self) -> Result<bool> {
        let result = self.read_inner();
        if let Err(error) = &result {
            if *self.shutdown.borrow()
                && matches!(
                    error.downcast_ref::<rusqlite::Error>(),
                    Some(rusqlite::Error::SqliteFailure(error, _))
                        if error.code == ErrorCode::OperationInterrupted
                )
            {
                return Ok(false);
            }
        }
        result
    }

    fn read_inner(&mut self) -> Result<bool> {
        if *self.shutdown.borrow() {
            return Ok(false);
        }
        self.check_deadline()?;
        let connection =
            Connection::open_with_flags(&self.config.path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                .context("opening SQLite snapshot file read-only")?;
        connection.busy_timeout(self.deadline.saturating_duration_since(Instant::now()))?;
        connection.set_limit(
            Limit::SQLITE_LIMIT_LENGTH,
            i32::try_from(self.config.max_bytes.get())?,
        );
        let shutdown = self.shutdown.clone();
        let deadline = self.deadline;
        connection.progress_handler(
            1000,
            Some(move || *shutdown.borrow() || Instant::now() >= deadline),
        );
        let file: String = connection.query_row(
            "SELECT file FROM pragma_database_list WHERE name='main'",
            [],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            !file.is_empty(),
            "SQLite file snapshot cannot read an in-memory database"
        );
        connection.execute_batch("BEGIN DEFERRED")?;
        let tables = read_table_schemas(
            &connection,
            &self.config.tables,
            &self.config.table_keys,
            self.config.max_tables.get(),
            self.config.max_bytes.get(),
        )?;
        for table in self
            .config
            .tables
            .iter()
            .chain(self.config.table_keys.iter().map(|key| &key.table))
        {
            anyhow::ensure!(
                tables.contains_key(table),
                "SQLite snapshot table '{table}' is missing or unsupported"
            );
        }
        let timestamp = u64::try_from(chrono::Utc::now().timestamp_millis())?;
        let mut ordinal = 0u64;
        let runtime = tokio::runtime::Handle::current();
        let source = self.source.clone();
        for (name, table) in tables {
            let outcome = read_table(
                &connection,
                source.as_str(),
                &name,
                &table,
                timestamp,
                |element| {
                    if *self.shutdown.borrow() {
                        return Ok(ControlFlow::Break(()));
                    }
                    self.check_deadline()?;
                    let change = SourceChange::Insert { element };
                    anyhow::ensure!(
                        bincode::serialized_size(&change)? <= self.config.max_bytes.get() as u64,
                        "SQLite file snapshot row exceeds byte limit"
                    );
                    ordinal = ordinal
                        .checked_add(1)
                        .context("SQLite file snapshot ordinal exhausted")?;
                    let change = GraphChangeCodec::encode_change(
                        change,
                        self.stream.clone(),
                        ordinal,
                        None,
                    )?;
                    let sent = runtime.block_on(async {
                    tokio::time::timeout_at(self.deadline, async {
                        tokio::select! {
                            biased;
                            _ = self.shutdown.wait_for(|stopped| *stopped) => Ok(false),
                            result = self.outputs.send(change) => result.map(|_| true).context("SQLite file snapshot receiver closed"),
                        }
                    }).await.context("SQLite file snapshot deadline exceeded")?
                })?;
                    Ok(if sent {
                        ControlFlow::Continue(())
                    } else {
                        ControlFlow::Break(())
                    })
                },
            )?;
            if outcome.is_break() {
                return Ok(false);
            }
        }
        self.check_deadline()?;
        connection.execute_batch("ROLLBACK")?;
        Ok(!*self.shutdown.borrow())
    }

    fn check_deadline(&self) -> Result<()> {
        anyhow::ensure!(
            Instant::now() < self.deadline,
            "SQLite file snapshot deadline exceeded"
        );
        Ok(())
    }
}
