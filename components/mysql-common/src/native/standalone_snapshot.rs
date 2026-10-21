// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    catalog,
    connection::Connection,
    session::{server_id, SessionCleanup},
    snapshot::read_rows,
    MySqlConfig, MySqlOutput,
};
use anyhow::{Context, Result};
use drasi_computation_contracts::{
    workers::{join_owned_worker_gracefully, spawn_owned_worker, WorkerCompletion},
    ChangeEnvelope, ComponentId, ComputationBootstrapSnapshot, OperationFailures, StreamId,
};
use futures_util::StreamExt;
use std::{
    collections::BTreeSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};
use tokio::{
    sync::{mpsc, oneshot, watch, Mutex, RwLock},
    task::JoinHandle,
};

struct Shared {
    worker: RwLock<Option<JoinHandle<Result<()>>>>,
    shutdown: watch::Sender<bool>,
    complete: AtomicBool,
    active: AtomicBool,
    timeout: std::time::Duration,
}

impl Shared {
    async fn join(&self) -> Result<()> {
        match join_owned_worker_gracefully(&mut *self.worker.write().await, self.timeout).await? {
            WorkerCompletion::Absent => Ok(()),
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Cancelled => {
                anyhow::bail!("MySQL standalone snapshot worker aborted")
            }
        }
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        self.shutdown.send_replace(true);
        if let Some(worker) = self.worker.get_mut() {
            worker.abort();
            log::warn!("MySQL standalone snapshot dropped without awaited cleanup");
        }
    }
}

struct Reader {
    receiver: mpsc::Receiver<ChangeEnvelope>,
    shared: Arc<Shared>,
    ended: bool,
}
impl Drop for Reader {
    fn drop(&mut self) {
        self.shared.shutdown.send_replace(true);
    }
}

pub struct StandaloneSnapshot {
    source: ComponentId,
    stream: StreamId,
    config: MySqlConfig,
    selected: Vec<String>,
    shared: Arc<Shared>,
    lifecycle: Mutex<()>,
}

impl StandaloneSnapshot {
    pub fn completed(&self) -> bool {
        self.shared.complete.load(Ordering::Acquire)
    }
    pub fn new(source: ComponentId, stream: StreamId, config: MySqlConfig) -> Result<Self> {
        config.validate()?;
        anyhow::ensure!(
            config.output == MySqlOutput::Changes && config.replay.is_none() && !config.tables.is_empty()
                && matches!(config.start, super::MySqlStartPosition::End),
            "standalone MySQL snapshots require explicit tables, change output, end mode and no replay claim"
        );
        let shared = Arc::new(Shared {
            worker: RwLock::new(None),
            shutdown: watch::channel(false).0,
            complete: AtomicBool::new(false),
            active: AtomicBool::new(false),
            timeout: config.timeout(),
        });
        Ok(Self {
            source,
            stream,
            selected: config.tables.clone(),
            config,
            shared,
            lifecycle: Mutex::new(()),
        })
    }

    pub fn for_labels(mut self, labels: &[String]) -> Self {
        if !labels.is_empty() {
            self.selected.retain(|table| labels.contains(table));
            for label in labels {
                if !self.config.tables.contains(label) {
                    log::warn!("MySQL bootstrap label {label} is outside the configured allowlist");
                }
            }
        }
        self
    }

    pub async fn prepare(&self) -> Result<()> {
        let _guard = self.lifecycle.lock().await;
        anyhow::ensure!(
            !self.shared.active.load(Ordering::Acquire),
            "MySQL standalone snapshot requires awaited cleanup"
        );
        Ok(())
    }

    pub async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let _guard = self.lifecycle.lock().await;
        anyhow::ensure!(
            !self.shared.active.swap(true, Ordering::AcqRel),
            "MySQL standalone snapshot is already active"
        );
        self.shared.complete.store(false, Ordering::Release);
        self.shared.shutdown.send_replace(false);
        let mut shutdown = self.shared.shutdown.subscribe();
        let (sender, receiver) = mpsc::channel(1);
        let (ready, opened) = oneshot::channel();
        let mut worker = Worker {
            config: self.config.clone(),
            selected: self.selected.clone(),
            source: self.source.clone(),
            stream: self.stream.clone(),
            control: None,
            sessions: Vec::new(),
            server: None,
            killed: BTreeSet::new(),
            ready: Some(ready),
        };
        let shared = self.shared.clone();
        spawn_owned_worker(&self.shared.worker, async move {
            let result = tokio::select! {
                biased;
                _ = shutdown.wait_for(|stopped| *stopped) => Ok(false),
                result = worker.run(&sender) => result.map(|()| true),
            };
            let cleanup = worker.cleanup().await;
            let completed = matches!((&result, &cleanup), (Ok(true), Ok(())));
            let errors: Vec<_> = [result.map(|_| ()), cleanup]
                .into_iter()
                .filter_map(Result::err)
                .collect();
            if !errors.is_empty() {
                return Err(OperationFailures::new(
                    "MySQL standalone snapshot and cleanup failed",
                    errors,
                )
                .into());
            }
            shared.complete.store(completed, Ordering::Release);
            Ok(())
        })
        .await?;
        if opened.await.is_err() {
            self.shared.join().await?;
            anyhow::bail!("MySQL standalone snapshot did not open");
        }
        let reader = Reader {
            receiver,
            shared: self.shared.clone(),
            ended: false,
        };
        let stream = futures_util::stream::unfold(reader, |mut reader| async move {
            if reader.ended {
                return None;
            }
            if let Some(envelope) = reader.receiver.recv().await {
                return Some((Ok(envelope), reader));
            }
            reader.ended = true;
            if let Err(error) = reader.shared.join().await {
                return Some((Err(error), reader));
            }
            if !reader.shared.complete.load(Ordering::Acquire) {
                return Some((
                    Err(anyhow::anyhow!("MySQL standalone snapshot was interrupted")),
                    reader,
                ));
            }
            None
        })
        .boxed();
        Ok(ComputationBootstrapSnapshot {
            changes: stream,
            watermarks: Vec::new(),
        })
    }

    pub async fn complete(&self) -> Result<()> {
        self.shared.join().await?;
        anyhow::ensure!(
            self.shared.complete.load(Ordering::Acquire),
            "MySQL standalone snapshot did not complete"
        );
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        self.shared.shutdown.send_replace(true);
        let _guard = self.lifecycle.lock().await;
        self.shared.join().await?;
        self.shared.active.store(false, Ordering::Release);
        Ok(())
    }
}

struct Worker {
    config: MySqlConfig,
    selected: Vec<String>,
    source: ComponentId,
    stream: StreamId,
    control: Option<Connection>,
    sessions: Vec<Connection>,
    server: Option<String>,
    killed: BTreeSet<u32>,
    ready: Option<oneshot::Sender<()>>,
}
impl Worker {
    async fn connect(&self) -> Result<Connection> {
        super::session::connect(
            &self.config.connection,
            self.config.max_protocol_bytes.get(),
            self.config.timeout(),
        )
        .await
    }
    async fn run(&mut self, output: &mpsc::Sender<ChangeEnvelope>) -> Result<()> {
        self.control = Some(self.connect().await?);
        self.server =
            Some(server_id(self.control.as_mut().context("MySQL snapshot control")?).await?);
        let mut connection = self.connect().await?;
        let actual = server_id(&mut connection).await?;
        if Some(actual) != self.server {
            connection.close().await?;
            anyhow::bail!("MySQL standalone snapshot reached another server");
        }
        self.sessions.push(connection);
        let connection = &mut self.sessions[0];
        connection.execute("SET SESSION time_zone='+00:00'").await?;
        connection
            .execute("SET SESSION TRANSACTION ISOLATION LEVEL REPEATABLE READ")
            .await?;
        let clock = connection
            .query("SELECT CAST(FLOOR(UNIX_TIMESTAMP(CURRENT_TIMESTAMP(6))*1000) AS UNSIGNED)")
            .await?;
        let [clock] = clock.values.as_slice() else {
            anyhow::bail!("MySQL snapshot clock unavailable");
        };
        let timestamp: u64 = catalog::field(clock, 0)?.parse()?;
        connection
            .execute("START TRANSACTION WITH CONSISTENT SNAPSHOT, READ ONLY")
            .await?;
        let tables = if self.selected.is_empty() {
            catalog::Tables::new()
        } else {
            let mut selected = self.config.clone();
            selected.tables = self.selected.clone();
            selected
                .table_keys
                .retain(|key| selected.tables.contains(&key.table));
            catalog::inspect_standalone(connection, &selected).await?
        };
        self.ready
            .take()
            .context("MySQL snapshot readiness")?
            .send(())
            .map_err(|_| anyhow::anyhow!("MySQL snapshot caller cancelled"))?;
        read_rows(
            connection,
            &self.config,
            &tables,
            &self.source,
            &self.stream,
            timestamp,
            output,
        )
        .await?;
        connection.execute("COMMIT").await?;
        Ok(())
    }
    async fn cleanup(&mut self) -> Result<()> {
        SessionCleanup {
            config: &self.config.connection,
            max_bytes: self.config.max_protocol_bytes.get(),
            timeout: self.config.timeout(),
            control: &mut self.control,
            sessions: &mut self.sessions,
            server: &self.server,
            killed: &mut self.killed,
        }
        .cleanup()
        .await
    }
}
