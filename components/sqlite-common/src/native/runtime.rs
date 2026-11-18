// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::{
    handle::{Command, SqliteHandle},
    progress::{Observed, Reader},
    schema::SqliteSchemaReader,
    worker::{self, WorkerControl, WorkerOutcome},
    SqliteConfig, SqliteConnectionOptions,
};
use anyhow::{Context, Result};
#[cfg(feature = "rest")]
use drasi_computation_contracts::workers::spawn_owned_worker;
use drasi_computation_contracts::{
    lifecycle::{CancellationToken, StopMode},
    pause::PauseControl,
    workers::{
        cancel_owned_worker, join_owned_worker_gracefully, spawn_owned_blocking_worker,
        WorkerCleanupError, WorkerCompletion,
    },
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
    time::Instant,
};

/// Serves a router on a bound listener until the shutdown future completes,
/// then drains. The default is `axum::serve` with graceful shutdown.
#[cfg(feature = "rest")]
pub type RestServe = Arc<
    dyn Fn(
            tokio::net::TcpListener,
            axum::Router,
            std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
        + Send
        + Sync,
>;

/// The REST listener of one activation.
#[derive(Clone)]
pub struct RestSettings {
    pub address: crate::RestApiConfig,
    /// Checks the `Authorization` header of every route except `/health`.
    pub authorize: Option<Arc<dyn Fn(Option<&str>) -> bool + Send + Sync>>,
    pub max_body_bytes: usize,
    pub request_timeout: Option<Duration>,
    /// Page size bound for row listings; `None` returns every row.
    pub page_limit: Option<usize>,
    pub sql_api: bool,
    /// The listener implementation; `None` uses `axum::serve`.
    #[cfg(feature = "rest")]
    pub serve: Option<RestServe>,
}

impl RestSettings {
    /// The legacy route set: unauthenticated, unpaged, served by axum.
    pub fn legacy(address: crate::RestApiConfig) -> Self {
        Self {
            address,
            authorize: None,
            max_body_bytes: 2 * 1024 * 1024,
            request_timeout: None,
            page_limit: None,
            sql_api: false,
            #[cfg(feature = "rest")]
            serve: None,
        }
    }

    pub fn validate(&self) -> Result<()> {
        self.address.validate()?;
        anyhow::ensure!(
            self.max_body_bytes > 0,
            "SQLite REST body limit must be positive"
        );
        anyhow::ensure!(
            self.page_limit != Some(0),
            "SQLite REST page limit must be positive"
        );
        anyhow::ensure!(
            !self.sql_api || self.authorize.is_some(),
            "SQLite /api/sql requires bearer authentication"
        );
        Ok(())
    }
}

pub struct StartContext {
    pub source: ComponentId,
    pub stream: StreamId,
    pub config: SqliteConfig,
    pub connection: SqliteConnectionOptions,
    pub sequence: Arc<AtomicU64>,
    pub rest: Option<RestSettings>,
    pub progress: Option<(Reader, Observed, Option<uuid::Uuid>)>,
}

/// How a Stop ended.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StopOutcome {
    /// Committed volatile transactions whose output was not delivered before
    /// the worker stopped. Journal modes report zero: their output stays in
    /// the journal and is replayed on the next start.
    pub lost_volatile_transactions: usize,
}

/// One database worker and its optional HTTP listener. Leases remain owned by
/// the actual workers, including after cancelled cleanup or an owner drop.
pub struct SqliteRuntime {
    commands: Arc<RwLock<Option<mpsc::Sender<Command>>>>,
    receiver: Option<mpsc::Receiver<OutputEnvelope>>,
    draining: watch::Sender<bool>,
    stop: watch::Sender<bool>,
    abort: watch::Sender<bool>,
    pause: PauseControl,
    worker: RwLock<Option<JoinHandle<Result<WorkerOutcome>>>>,
    rest_address: Option<SocketAddr>,
    rest_worker: RwLock<Option<JoinHandle<Result<()>>>>,
    rest_shutdown: watch::Sender<bool>,
    cleanup_required: bool,
    timeout: Duration,
    schema: Arc<SqliteSchemaReader>,
    outcome: StopOutcome,
    drain_transactions: bool,
}

fn timed_out(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<WorkerCleanupError>(),
        Some(WorkerCleanupError::TimedOut { .. })
    )
}

fn remaining(deadline: Option<Instant>, fallback: Duration) -> Duration {
    deadline.map_or(fallback, |deadline| {
        deadline.saturating_duration_since(Instant::now())
    })
}

impl SqliteRuntime {
    pub fn new(commands: Arc<RwLock<Option<mpsc::Sender<Command>>>>, timeout: Duration) -> Self {
        Self {
            commands,
            receiver: None,
            draining: watch::channel(false).0,
            stop: watch::channel(false).0,
            abort: watch::channel(false).0,
            pause: PauseControl::new(),
            worker: RwLock::new(None),
            rest_address: None,
            rest_worker: RwLock::new(None),
            rest_shutdown: watch::channel(false).0,
            cleanup_required: false,
            timeout,
            schema: Arc::new(SqliteSchemaReader::new()),
            outcome: StopOutcome::default(),
            drain_transactions: false,
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

    /// Whether a `Drain` stop lets an open transaction scope finish (within
    /// the stop deadline) instead of rolling it back. Off by default, which
    /// is the host-linked source's behaviour: Drain finishes in-flight HTTP
    /// requests but rolls back an open Rust transaction callback.
    pub fn drain_open_transactions(&mut self, drain: bool) {
        self.drain_transactions = drain;
    }

    /// The fallback cleanup bound used when a lifecycle call has no deadline.
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
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

    /// Whether an activation (or its unfinished cleanup) is still owned.
    pub fn cleanup_required(&self) -> bool {
        self.cleanup_required
    }

    /// Whether the worker has acknowledged a Pause at a transaction boundary.
    pub fn paused(&self) -> bool {
        self.pause.is_paused()
    }

    /// Signals every activation worker to stop now, without waiting. Used when
    /// a start is cancelled or fails part-way; a later [`Self::stop`] joins.
    pub fn request_abort(&self) {
        self.schema.deactivate();
        self.rest_shutdown.send_replace(true);
        self.draining.send_replace(true);
        self.abort.send_replace(true);
        self.stop.send_replace(true);
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
        self.outcome = StopOutcome::default();
        context.config.validate()?;
        context.connection.validate(&context.config)?;
        if let Some(rest) = &context.rest {
            rest.validate()?;
        }
        anyhow::ensure!(
            context.rest.is_none() || cfg!(feature = "rest"),
            "native SQLite REST support is not enabled"
        );
        self.draining.send_replace(false);
        self.stop.send_replace(false);
        self.abort.send_replace(false);
        self.pause.resume();
        self.rest_shutdown.send_replace(false);
        #[cfg(feature = "rest")]
        let listener = match &context.rest {
            Some(rest) => Some(
                tokio::net::TcpListener::bind((rest.address.host.as_str(), rest.address.port))
                    .await
                    .context("native SQLite REST listener could not bind")?,
            ),
            None => None,
        };
        let (commands, receiver) = mpsc::channel(context.config.command_capacity.get());
        let (outputs, output_rx) = mpsc::channel(context.config.output_capacity.get());
        let (ready, readiness) = oneshot::channel();
        let rest_config = context.config.clone();
        let control = WorkerControl {
            draining: self.draining.subscribe(),
            drain_transactions: self.drain_transactions,
            stop: self.stop.subscribe(),
            abort: self.abort.subscribe(),
            pause: self.pause.gate(),
        };
        let database_ownership = ownership.clone();
        self.receiver = Some(output_rx);
        spawn_owned_blocking_worker(&self.worker, move || {
            let _ownership = database_ownership;
            let result = worker::run(
                context.config,
                context.connection,
                context.source,
                context.stream,
                context.sequence,
                receiver,
                outputs,
                control,
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
            self.join(self.timeout).await?;
            anyhow::bail!("native SQLite worker exited before readiness");
        }
        *self.commands.write().await = Some(commands);
        let schema_handle = SqliteHandle::new(self.commands.clone(), &rest_config)
            .bind_to_current_worker()
            .await?;
        #[cfg(feature = "rest")]
        if let (Some(listener), Some(rest)) = (listener, context.rest) {
            let address = listener.local_addr()?;
            let options = crate::rest_api::RestOptions {
                tables: (!rest_config.tables.is_empty()).then_some(rest_config.tables),
                table_keys: rest_config.table_keys,
                authorize: rest.authorize,
                max_body_bytes: rest.max_body_bytes,
                request_timeout: rest.request_timeout,
                paused: Some(self.pause.requests()),
                stopping: self.rest_shutdown.subscribe(),
                page_limit: rest.page_limit,
                sql_api: rest.sql_api,
                retry_after_secs: 1,
            };
            let router = crate::rest_api::router(schema_handle.clone(), options);
            let mut rest_shutdown = self.rest_shutdown.subscribe();
            let shutdown: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> =
                Box::pin(async move {
                    let _ = rest_shutdown.wait_for(|stopped| *stopped).await;
                });
            let serve = rest.serve;
            let registered = spawn_owned_worker(&self.rest_worker, async move {
                let _ownership = ownership;
                match serve {
                    Some(serve) => serve(listener, router, shutdown).await,
                    None => axum::serve(listener, router)
                        .with_graceful_shutdown(shutdown)
                        .await
                        .map_err(Into::into),
                }
            })
            .await;
            if let Err(error) = registered {
                self.commands.write().await.take();
                self.request_abort();
                return Err(error.into());
            }
            self.rest_address = Some(address);
        }
        self.schema.activate(schema_handle)?;
        Ok(())
    }

    /// Asks the worker to park at its next transaction boundary and waits for
    /// its acknowledgement. REST writes are refused (503) from the moment the
    /// request is made. A cancelled or expired Pause reopens admission.
    pub async fn pause(
        &mut self,
        deadline: Option<Instant>,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let Some(worker) = self.worker.get_mut().as_ref() else {
            return Ok(());
        };
        anyhow::ensure!(
            !worker.is_finished(),
            "native SQLite worker has ended; stop and restart the source"
        );
        let deadline = Instant::now() + remaining(deadline, self.timeout);
        self.pause
            .pause(Some(deadline), cancellation)
            .await
            .map_err(anyhow::Error::new)
            .context(
                "native SQLite could not reach a transaction boundary before the pause deadline",
            )
    }

    /// Reopens what Pause closed: REST writes, handle writes and output.
    pub fn resume(&mut self) {
        self.pause.resume();
    }

    async fn join(&mut self, timeout: Duration) -> Result<()> {
        match join_owned_worker_gracefully(self.worker.get_mut(), timeout).await {
            Ok(WorkerCompletion::Completed(Ok(outcome))) => {
                self.outcome.lost_volatile_transactions += outcome.lost_volatile_transactions;
                Ok(())
            }
            Ok(WorkerCompletion::Completed(Err(error))) => Err(error),
            Ok(WorkerCompletion::Absent) => Ok(()),
            Ok(WorkerCompletion::Cancelled) => anyhow::bail!("native SQLite worker was cancelled"),
            Err(error) => Err(error.into()),
        }
    }

    async fn join_rest(&mut self, mode: StopMode, timeout: Duration) -> Result<()> {
        let result = match mode {
            StopMode::Abort => cancel_owned_worker(self.rest_worker.get_mut(), timeout).await,
            _ => join_owned_worker_gracefully(self.rest_worker.get_mut(), timeout).await,
        };
        if self.rest_worker.get_mut().is_none() {
            self.rest_address = None;
        }
        match result? {
            WorkerCompletion::Completed(result) => result,
            WorkerCompletion::Absent | WorkerCompletion::Cancelled => Ok(()),
        }
    }

    /// Stops the activation in two phases.
    ///
    /// First, REST stops accepting requests and the worker starts draining:
    /// it keeps serving requests already accepted, but never waits for
    /// output, so an accepted request can always finish (undeliverable
    /// volatile output is dropped and reported in [`StopOutcome`]; journal
    /// output stays in the journal). `Drain` waits for in-flight HTTP
    /// requests; `Abort` drops connections and rolls back at once. Then the
    /// worker refuses queued commands and exits; an open transaction scope
    /// finishes first only when [`Self::drain_open_transactions`] is set.
    ///
    /// On expiry the error is retryable: ownership of every unfinished worker
    /// is kept and a later call completes the join. An earlier worker
    /// failure, already reported through [`Self::next`], does not fail Stop.
    pub async fn stop(&mut self, mode: StopMode, deadline: Option<Instant>) -> Result<StopOutcome> {
        self.schema.deactivate();
        self.commands.write().await.take();
        self.rest_shutdown.send_replace(true);
        self.draining.send_replace(true);
        if mode == StopMode::Abort {
            self.abort.send_replace(true);
            self.stop.send_replace(true);
        }
        match self
            .join_rest(mode, remaining(deadline, self.timeout))
            .await
        {
            Ok(()) => {}
            Err(error) if timed_out(&error) => {
                return Err(error.context(
                    "native SQLite REST listener did not drain within its deadline; retry Stop",
                ));
            }
            Err(error) => log::debug!("Native SQLite REST listener had already failed: {error:#}"),
        }
        self.stop.send_replace(true);
        match self.join(remaining(deadline, self.timeout)).await {
            Ok(()) => {}
            Err(error) if timed_out(&error) => {
                return Err(error
                    .context("native SQLite worker did not stop within its deadline; retry Stop"));
            }
            Err(error) => log::debug!("Native SQLite worker had already failed: {error:#}"),
        }
        self.receiver = None;
        self.cleanup_required = false;
        self.pause.resume();
        let outcome = std::mem::take(&mut self.outcome);
        if outcome.lost_volatile_transactions > 0 {
            log::warn!(
                "Native SQLite stopped with {} committed volatile transaction(s) undelivered",
                outcome.lost_volatile_transactions
            );
        }
        Ok(outcome)
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
            self.rest_worker.get_mut().take();
            self.rest_address = None;
            self.commands.write().await.take();
            self.request_abort();
            result.context("native SQLite REST worker failed")??;
            anyhow::bail!("native SQLite REST worker ended unexpectedly");
        }
        if let Some(output) = output {
            return Ok(Some(output));
        }
        self.commands.write().await.take();
        self.request_abort();
        self.join(self.timeout).await?;
        anyhow::bail!("native SQLite worker ended; stop and restart the source")
    }
}

impl Drop for SqliteRuntime {
    fn drop(&mut self) {
        self.request_abort();
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
                source: ComponentId::try_new("ed31460a-3a84-513d-9642-88f49710c80e")?,
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
                connection: SqliteConnectionOptions::default(),
                sequence: Arc::new(AtomicU64::new(0)),
                rest: Some(RestSettings::legacy(crate::RestApiConfig {
                    host: "127.0.0.1".into(),
                    port: 0,
                })),
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
        runtime.stop(StopMode::Drain, None).await?;
        Ok(())
    }
}
