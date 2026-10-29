// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    handle::{Command, SqliteHandle},
    progress::{Observed, Reader},
    schema::SqliteSchemaReader,
    worker, SqliteConfig,
};
use anyhow::{Context, Result};
#[cfg(feature = "rest")]
use drasi_computation_contracts::workers::spawn_owned_worker;
use drasi_computation_contracts::{
    workers::{join_owned_worker_gracefully, spawn_owned_blocking_worker, WorkerCompletion},
    ComponentId, OutputEnvelope, StreamId,
};
use std::{
    net::SocketAddr,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot, watch, RwLock},
    task::JoinHandle,
};

pub struct StartContext {
    pub source: ComponentId,
    pub stream: StreamId,
    pub config: SqliteConfig,
    pub sequence: Arc<AtomicU64>,
    pub rest_api: Option<crate::RestApiConfig>,
    pub progress: Option<(Reader, Observed, Option<uuid::Uuid>)>,
}

/// One database worker and its optional HTTP listener. Leases remain owned by
/// the actual workers, including after cancelled cleanup or an owner drop.
pub struct SqliteRuntime {
    commands: Arc<RwLock<Option<mpsc::Sender<Command>>>>,
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    shutdown: watch::Sender<bool>,
    worker: RwLock<Option<JoinHandle<Result<()>>>>,
    rest_address: Option<SocketAddr>,
    rest_worker: RwLock<Option<JoinHandle<Result<()>>>>,
    rest_shutdown: watch::Sender<bool>,
    cleanup_required: bool,
    timeout: Duration,
    schema: Arc<SqliteSchemaReader>,
}

impl SqliteRuntime {
    pub fn new(commands: Arc<RwLock<Option<mpsc::Sender<Command>>>>, timeout: Duration) -> Self {
        Self {
            commands,
            receiver: None,
            shutdown: watch::channel(false).0,
            worker: RwLock::new(None),
            rest_address: None,
            rest_worker: RwLock::new(None),
            rest_shutdown: watch::channel(false).0,
            cleanup_required: false,
            timeout,
            schema: Arc::new(SqliteSchemaReader::new()),
        }
    }

    pub fn bind_commands(
        &mut self,
        commands: Arc<RwLock<Option<mpsc::Sender<Command>>>>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required,
            "native SQLite client must be bound before source startup"
        );
        self.commands = commands;
        Ok(())
    }

    pub fn bound_rest_address(&self) -> Option<SocketAddr> {
        self.rest_address
    }

    pub fn source_schema_reader(&self) -> Arc<SqliteSchemaReader> {
        self.schema.clone()
    }

    pub async fn owns_database_worker(&self) -> bool {
        self.worker.read().await.is_some()
    }

    async fn join(&mut self) -> Result<()> {
        match join_owned_worker_gracefully(self.worker.get_mut(), self.timeout).await? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Absent => Ok(()),
            WorkerCompletion::Cancelled => anyhow::bail!("native SQLite worker was cancelled"),
        }
    }

    async fn join_rest(&mut self) -> Result<()> {
        let result = join_owned_worker_gracefully(self.rest_worker.get_mut(), self.timeout).await;
        if self.rest_worker.get_mut().is_none() {
            self.rest_address = None;
        }
        match result? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Absent => Ok(()),
            WorkerCompletion::Cancelled => anyhow::bail!("native SQLite REST worker was cancelled"),
        }
    }

    pub async fn start<L: Send + Sync + 'static>(
        &mut self,
        context: StartContext,
        ownership: Arc<L>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.cleanup_required,
            "native SQLite source requires awaited cleanup before restart"
        );
        self.cleanup_required = true;
        context.config.validate()?;
        if let Some(rest) = &context.rest_api {
            rest.validate()?;
        }
        anyhow::ensure!(
            context.rest_api.is_none() || cfg!(feature = "rest"),
            "native SQLite REST support is not enabled"
        );
        self.shutdown.send_replace(false);
        self.rest_shutdown.send_replace(false);
        #[cfg(feature = "rest")]
        let listener = match &context.rest_api {
            Some(rest) => Some(
                tokio::net::TcpListener::bind((rest.host.as_str(), rest.port))
                    .await
                    .context("native SQLite REST listener could not bind")?,
            ),
            None => None,
        };
        let (commands, receiver) = mpsc::channel(context.config.command_capacity.get());
        let (outputs, output_rx) = mpsc::channel(context.config.output_capacity.get());
        let (ready, readiness) = oneshot::channel();
        let rest_config = context.config.clone();
        let database_shutdown = self.shutdown.subscribe();
        let database_ownership = ownership.clone();
        self.receiver = Some(output_rx);
        spawn_owned_blocking_worker(&self.worker, move || {
            let _ownership = database_ownership;
            let result = worker::run(
                context.config,
                context.source,
                context.stream,
                context.sequence,
                receiver,
                outputs,
                database_shutdown,
                ready,
                context.progress,
            );
            if let Err(error) = &result {
                log::error!("Native SQLite worker failed: {error:#}");
            }
            result
        })
        .await?;
        if readiness.await.is_err() {
            self.join().await?;
            anyhow::bail!("native SQLite worker exited before readiness");
        }
        *self.commands.write().await = Some(commands);
        let schema_handle = SqliteHandle::new(self.commands.clone(), &rest_config)
            .bind_to_current_worker()
            .await?;
        #[cfg(feature = "rest")]
        if let Some(listener) = listener {
            let address = listener.local_addr()?;
            let handle = schema_handle.clone();
            let tables = (!rest_config.tables.is_empty()).then_some(rest_config.tables);
            let rest_shutdown = self.rest_shutdown.subscribe();
            let registered = spawn_owned_worker(&self.rest_worker, async move {
                let _ownership = ownership;
                crate::rest_api::run_rest_api(
                    listener,
                    handle,
                    tables,
                    rest_config.table_keys,
                    rest_shutdown,
                )
                .await
                .map_err(Into::into)
            })
            .await;
            if let Err(error) = registered {
                self.commands.write().await.take();
                self.rest_shutdown.send_replace(true);
                self.shutdown.send_replace(true);
                return Err(error.into());
            }
            self.rest_address = Some(address);
        }
        self.schema.activate(schema_handle)?;
        Ok(())
    }

    pub async fn stop(&mut self) -> Result<()> {
        self.schema.deactivate();
        self.commands.write().await.take();
        self.rest_shutdown.send_replace(true);
        // Accepted HTTP requests retain their bound SQL client until REST drains.
        self.join_rest().await?;
        self.shutdown.send_replace(true);
        self.join().await?;
        self.receiver = None;
        self.cleanup_required = false;
        Ok(())
    }

    pub async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        let receiver = self
            .receiver
            .as_mut()
            .context("native SQLite source is not running")?;
        let rest = self
            .rest_worker
            .get_mut()
            .as_mut()
            .filter(|_| !*self.rest_shutdown.borrow());
        let (output, rest_finished) = if let Some(rest) = rest {
            tokio::select! {
                output = receiver.recv() => (output, None),
                result = rest => (None, Some(result)),
            }
        } else {
            (receiver.recv().await, None)
        };
        if let Some(result) = rest_finished {
            self.schema.deactivate();
            self.rest_worker.get_mut().take();
            self.rest_address = None;
            self.commands.write().await.take();
            self.rest_shutdown.send_replace(true);
            self.shutdown.send_replace(true);
            result.context("native SQLite REST worker failed")??;
            anyhow::bail!("native SQLite REST worker ended unexpectedly");
        }
        if let Some(output) = output {
            return Ok(Some(output));
        }
        self.schema.deactivate();
        self.commands.write().await.take();
        self.rest_shutdown.send_replace(true);
        self.shutdown.send_replace(true);
        self.join().await?;
        anyhow::bail!("native SQLite worker ended; stop and restart the source")
    }
}

impl Drop for SqliteRuntime {
    fn drop(&mut self) {
        self.schema.deactivate();
        self.rest_shutdown.send_replace(true);
        self.shutdown.send_replace(true);
        if self.worker.get_mut().is_some() {
            log::warn!("Native SQLite source dropped without awaited database cleanup");
        }
        if self.rest_worker.get_mut().is_some() {
            log::warn!("Native SQLite source dropped without awaited REST cleanup");
        }
    }
}

#[cfg(all(test, feature = "rest"))]
mod tests {
    use super::*;
    use crate::native::SqliteOutput;
    use drasi_computation_contracts::SourceTransactionLimits;
    use std::num::{NonZeroU64, NonZeroUsize};

    #[tokio::test(flavor = "current_thread")]
    async fn unexpected_rest_worker_failure_is_a_native_source_failure() -> Result<()> {
        let context = || -> Result<StartContext> {
            Ok(StartContext {
                source: ComponentId::try_new("sqlite")?,
                stream: StreamId::try_new("changes")?,
                config: SqliteConfig {
                    path: None,
                    tables: Vec::new(),
                    table_keys: Vec::new(),
                    output: SqliteOutput::Changes,
                    replay: None,
                    transactions: SourceTransactionLimits {
                        max_changes: NonZeroUsize::new(32).expect("limit"),
                        max_bytes: NonZeroUsize::new(1 << 20).expect("limit"),
                        max_duration_ms: NonZeroU64::new(5000).expect("deadline"),
                    },
                    max_sql_bytes: NonZeroUsize::new(1 << 16).expect("limit"),
                    command_capacity: NonZeroUsize::new(4).expect("capacity"),
                    output_capacity: NonZeroUsize::new(4).expect("capacity"),
                    shutdown_timeout_ms: NonZeroU64::new(5000).expect("deadline"),
                },
                sequence: Arc::new(AtomicU64::new(0)),
                rest_api: Some(crate::RestApiConfig {
                    host: "127.0.0.1".into(),
                    port: 0,
                }),
                progress: None,
            })
        };
        let mut runtime = SqliteRuntime::new(Arc::new(RwLock::new(None)), Duration::from_secs(5));
        runtime.start(context()?, Arc::new(())).await?;
        runtime
            .rest_worker
            .get_mut()
            .as_ref()
            .context("REST owner")?
            .abort();
        let error = runtime
            .next()
            .await
            .expect_err("REST failure must not leave a healthy idle source");
        assert!(format!("{error:#}").contains("REST worker"), "{error:#}");
        assert!(runtime.bound_rest_address().is_none());
        assert!(runtime.start(context()?, Arc::new(())).await.is_err());
        runtime.stop().await?;
        Ok(())
    }
}
