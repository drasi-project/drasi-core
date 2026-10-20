// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_computation_contracts::{
    BootstrapPreparation, BootstrapWatermark, ChangeEnvelope, ComponentId,
    ComputationBootstrapSnapshot, SourceProgressView, StreamId,
};
use drasi_computation_plugin_sdk::{NativeBootstrapProvider, Scope};
use drasi_postgres_common::{
    connection::parse_lsn,
    replication::{
        connect, field,
        rows::{row_position, RowPosition},
        shared_transactions::{self as transactions, SharedTransactionPosition},
        source_binding, validate_retention, Connection, PostgresTransactionConfig,
    },
};
use std::sync::{Arc, Mutex};
use tokio::sync::{watch, Mutex as AsyncMutex};

use crate::snapshot::{
    export_snapshot, snapshot_timestamp, validate_snapshot_publication, SnapshotReader,
};

struct Run {
    reader: AsyncMutex<Option<SnapshotReader>>,
    shutdown: watch::Sender<bool>,
    completed: Mutex<Option<Position>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Rows,
    Transactions,
}

#[derive(Clone)]
enum Position {
    Rows(RowPosition),
    Transactions(SharedTransactionPosition),
}
impl Position {
    fn lsn(&self) -> u64 {
        match self {
            Self::Rows(position) => position.commit_lsn,
            Self::Transactions(position) => position.commit_lsn,
        }
    }
    fn sequence(&self) -> u64 {
        match self {
            Self::Rows(position) => position.sequence,
            Self::Transactions(position) => position.sequence,
        }
    }
    fn encode(&self) -> Result<Vec<u8>> {
        Ok(match self {
            Self::Rows(position) => serde_json::to_vec(position)?,
            Self::Transactions(position) => serde_json::to_vec(position)?,
        })
    }
}

/// Snapshot initialization for an existing configured slot, with no ownership
/// transfer of that slot and no fabricated persistent bootstrap state.
pub struct PostgresSharedSnapshot {
    source: ComponentId,
    stream: StreamId,
    config: Arc<PostgresTransactionConfig>,
    progress: SourceProgressView,
    mode: Mode,
    run: Mutex<Option<Arc<Run>>>,
    lifecycle: AsyncMutex<()>,
    pub(crate) query_scope: Option<(Scope, ComponentId)>,
}

impl PostgresSharedSnapshot {
    pub fn new(
        source: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: SourceProgressView,
    ) -> Result<Self> {
        Self::with_mode(source, stream, config, progress, Mode::Rows)
    }

    pub fn transactions(
        source: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: SourceProgressView,
    ) -> Result<Self> {
        Self::with_mode(source, stream, config, progress, Mode::Transactions)
    }

    fn with_mode(
        source: ComponentId,
        stream: StreamId,
        config: PostgresTransactionConfig,
        progress: SourceProgressView,
        mode: Mode,
    ) -> Result<Self> {
        config.validate()?;
        Ok(Self {
            source,
            stream,
            config: Arc::new(config),
            progress,
            mode,
            run: Mutex::new(None),
            lifecycle: AsyncMutex::new(()),
            query_scope: None,
        })
    }

    fn current(&self) -> Result<Option<Arc<Run>>> {
        Ok(self
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL snapshot owner poisoned"))?
            .clone())
    }

    async fn begin(&self, run: Arc<Run>) -> Result<Reader> {
        let mut exporter = connect(&self.config).await?;
        validate_retention(&mut exporter, &self.config).await?;
        let confirmed = idle_slot(&mut exporter, &self.config).await?;
        let (keys, binding) =
            source_binding(&mut exporter, &self.config, &self.source, &self.stream).await?;
        validate_snapshot_publication(&mut exporter, &self.config).await?;
        let timestamp = snapshot_timestamp(&mut exporter, &self.config).await?;
        let temporary = format!("drasi_snapshot_{}", uuid::Uuid::new_v4().simple());
        let (boundary, snapshot) =
            export_snapshot(&mut exporter, &self.config, &temporary, true).await?;
        anyhow::ensure!(
            confirmed <= boundary,
            "configured PostgreSQL slot is beyond the exported snapshot"
        );
        let mut connection = connect(&self.config).await?;
        connection.query_rows(
            format!("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY; SET TRANSACTION SNAPSHOT '{snapshot}'"),
            self.config.max_protocol_bytes,
        ).await?;
        // No command is sent to the exporter after EXPORT_SNAPSHOT. The reader
        // now owns the imported snapshot; closing the exporter removes only its temporary slot.
        drop(exporter);
        let reader = SnapshotReader::new(
            connection,
            (*self.config).clone(),
            keys,
            timestamp,
            self.source.clone(),
            self.stream.clone(),
        )
        .await?;
        *run.reader.lock().await = Some(reader);
        let position = match self.mode {
            Mode::Rows => Position::Rows(RowPosition::configured_snapshot(binding, boundary)),
            Mode::Transactions => {
                Position::Transactions(SharedTransactionPosition::configured_snapshot(
                    transactions::transaction_binding(binding),
                    boundary,
                )?)
            }
        };
        Ok(Reader {
            run,
            position,
            confirmed,
            config: self.config.clone(),
        })
    }
}

#[async_trait]
impl NativeBootstrapProvider for PostgresSharedSnapshot {
    fn recovery_reader(&self) -> Option<SourceProgressView> {
        Some(self.progress.clone())
    }

    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        anyhow::ensure!(
            self.progress.graph_id() == graph && self.progress.component_id() == query,
            "shared PostgreSQL snapshot belongs to another query"
        );
        if let Some((scope, owner)) = &self.query_scope {
            anyhow::ensure!(
                scope.instance_id == instance && scope.graph_id == graph && owner == query,
                "shared PostgreSQL snapshot query scope changed"
            );
        }
        Ok(())
    }

    async fn prepare(&self) -> Result<BootstrapPreparation> {
        let progress = self.progress.snapshot()?;
        anyhow::ensure!(
            progress.recovered && progress.failure.is_none(),
            "shared PostgreSQL snapshot requires its recovered owner"
        );
        anyhow::ensure!(
            self.mode != Mode::Transactions || progress.persistent,
            "shared PostgreSQL transaction snapshot requires persistent query progress"
        );
        let present = match self.mode {
            Mode::Rows => {
                let position = row_position(&progress, &self.source)?;
                anyhow::ensure!(
                    position
                        .as_ref()
                        .is_none_or(|position| position.managed_slot.is_none()),
                    "shared PostgreSQL snapshot cannot adopt managed-slot progress"
                );
                position.is_some()
            }
            Mode::Transactions => transactions::read_position(&progress, &self.source)?.is_some(),
        };
        anyhow::ensure!(
            !progress.bootstrap_complete || present,
            "completed shared PostgreSQL snapshot has no input position"
        );
        if present && !progress.persistent {
            return Ok(BootstrapPreparation::RefreshVolatile);
        }
        if present && !progress.bootstrap_complete {
            return Ok(BootstrapPreparation::ResetRequired);
        }
        Ok(BootstrapPreparation::Ready)
    }

    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let _lifecycle = self.lifecycle.lock().await;
        anyhow::ensure!(
            self.current()?.is_none(),
            "shared PostgreSQL snapshot requires awaited cleanup before reuse"
        );
        let progress = self.progress.snapshot()?;
        anyhow::ensure!(
            progress.recovered
                && !progress.ready
                && !progress.bootstrap_complete
                && (self.mode != Mode::Transactions || progress.persistent)
                && progress.failure.is_none(),
            "shared PostgreSQL snapshot requires a fenced, recovered initialization owner"
        );
        let run = Arc::new(Run {
            reader: AsyncMutex::new(None),
            shutdown: watch::channel(false).0,
            completed: Mutex::new(None),
        });
        *self
            .run
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL snapshot owner poisoned"))? =
            Some(run.clone());
        let mut shutdown = run.shutdown.subscribe();
        let reader = tokio::select! {
            biased;
            _ = shutdown.wait_for(|stopped| *stopped) => anyhow::bail!("shared PostgreSQL snapshot is stopped"),
            result = tokio::time::timeout(self.config.timeout(), self.begin(run.clone())) =>
                result.context("shared PostgreSQL snapshot initialization timed out")??,
        };
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::try_unfold(
                reader,
                |mut reader| async move {
                    let item = reader.next().await?;
                    Ok(item.map(|item| (item, reader)))
                },
            )),
            watermarks: Vec::new(),
        })
    }

    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        let run = self
            .current()?
            .context("shared PostgreSQL snapshot is not prepared")?;
        let position = run
            .completed
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL snapshot completion poisoned"))?
            .clone()
            .context("shared PostgreSQL snapshot was not exhausted")?;
        Ok(vec![BootstrapWatermark {
            stream: self.stream.clone(),
            source_id: Some(self.source.to_string()),
            sequence: position.sequence(),
            position: Some(position.encode()?.into()),
        }])
    }

    async fn stop(&self) -> Result<()> {
        if let Some(run) = self.current()? {
            run.shutdown.send_replace(true);
        }
        let _lifecycle = tokio::time::timeout(self.config.timeout(), self.lifecycle.lock())
            .await
            .context("shared PostgreSQL snapshot initialization cleanup timed out")?;
        if let Some(run) = self.current()? {
            run.shutdown.send_replace(true);
            tokio::time::timeout(self.config.timeout(), run.reader.lock())
                .await
                .context("shared PostgreSQL snapshot reader cleanup timed out")?
                .take();
        }
        self.run
            .lock()
            .map_err(|_| anyhow::anyhow!("PostgreSQL snapshot owner poisoned"))?
            .take();
        Ok(())
    }
}

impl Drop for PostgresSharedSnapshot {
    fn drop(&mut self) {
        if let Ok(Some(run)) = self.current() {
            run.shutdown.send_replace(true);
            match run.reader.try_lock() {
                Ok(mut reader) => {
                    reader.take();
                }
                Err(_) => log::warn!(
                    "Shared PostgreSQL snapshot dropped during I/O without awaited cleanup"
                ),
            }
        }
    }
}

struct Reader {
    run: Arc<Run>,
    position: Position,
    confirmed: u64,
    config: Arc<PostgresTransactionConfig>,
}

impl Reader {
    async fn next(&mut self) -> Result<Option<ChangeEnvelope>> {
        anyhow::ensure!(
            !*self.run.shutdown.borrow(),
            "shared PostgreSQL snapshot stream is retired"
        );
        let run = self.run.clone();
        let mut shutdown = run.shutdown.subscribe();
        let mut owner = run.reader.lock().await;
        let reader = owner
            .as_mut()
            .context("shared PostgreSQL snapshot is stopped")?;
        tokio::select! {
            biased;
            _ = shutdown.wait_for(|stopped| *stopped) => anyhow::bail!("shared PostgreSQL snapshot is stopped"),
            result = tokio::time::timeout(self.config.timeout(), self.read(reader)) =>
                result.context("shared PostgreSQL snapshot read timed out")?,
        }
    }

    async fn read(&self, reader: &mut SnapshotReader) -> Result<Option<ChangeEnvelope>> {
        let item = reader.next().await?;
        if item.is_none() {
            let confirmed = idle_slot(reader.connection(), &self.config).await?;
            anyhow::ensure!(
                confirmed == self.confirmed,
                "configured PostgreSQL feedback advanced during required snapshot initialization"
            );
            let slot = reader
                .connection()
                .get_replication_slot_info(&self.config.connection.slot_name)
                .await?;
            anyhow::ensure!(
                parse_lsn(
                    slot.restart_lsn
                        .as_deref()
                        .context("configured slot has no retained WAL")?
                )? <= self.position.lsn(),
                "configured PostgreSQL slot no longer retains the snapshot boundary"
            );
            *self
                .run
                .completed
                .lock()
                .map_err(|_| anyhow::anyhow!("PostgreSQL snapshot completion poisoned"))? =
                Some(self.position.clone());
        }
        Ok(item)
    }
}

impl Drop for Reader {
    fn drop(&mut self) {
        self.run.shutdown.send_replace(true);
        if let Ok(mut reader) = self.run.reader.try_lock() {
            reader.take();
        }
    }
}

async fn idle_slot(connection: &mut Connection, config: &PostgresTransactionConfig) -> Result<u64> {
    loop {
        let rows = connection
            .query_rows(
                format!(
            "SELECT plugin, temporary, active, confirmed_flush_lsn::text FROM pg_replication_slots \
             WHERE slot_name='{}' AND database=current_database()", config.connection.slot_name,
        ),
                config.max_protocol_bytes,
            )
            .await?;
        let [row] = rows.as_slice() else {
            anyhow::bail!("configured PostgreSQL slot is missing");
        };
        anyhow::ensure!(
            field(row, 0)? == "pgoutput" && field(row, 1)? == "f",
            "configured PostgreSQL slot is not persistent pgoutput"
        );
        match field(row, 2)?.as_str() {
            "f" => return parse_lsn(&field(row, 3)?),
            "t" => tokio::time::sleep(std::time::Duration::from_millis(10)).await,
            _ => anyhow::bail!("invalid PostgreSQL slot activity"),
        }
    }
}
