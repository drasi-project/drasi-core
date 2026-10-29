// Copyright 2025 The Drasi Authors.
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

#![allow(unexpected_cfgs)]

//! SQLite source plugin for Drasi.
//!
//! This source owns an embedded SQLite connection running in a blocking worker.
//! Changes are captured through SQLite hooks:
//! - `preupdate_hook` captures row-level INSERT/UPDATE/DELETE events
//! - `commit_hook` flushes buffered changes
//! - `rollback_hook` discards buffered changes
//!
//! The source supports:
//! - file-backed and in-memory SQLite databases
//! - optional table filtering
//! - optional REST CRUD + transactional batch endpoints
//! - bootstrap via pluggable bootstrap providers

mod config;
mod convert;
pub mod descriptor;
pub mod native;
mod rest_api;
#[cfg(test)]
mod tests;
mod thread;

pub use config::{
    RestApiConfig, SqliteSourceBuilder, SqliteSourceConfig, StartFrom, TableKeyConfig,
};
pub use native::SqliteSource as NativeSqliteSource;
pub use thread::SqliteParam;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, SourceEvent, SourceEventWrapper, SubscriptionResponse};
use drasi_lib::sources::base::SourceBase;
use drasi_lib::Source;
use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot, watch, Mutex, RwLock};
use tokio::task::JoinHandle;
use tracing::Instrument;

use crate::thread::SqliteCommand;
use drasi_lib::context::workers::{
    join_owned_worker_gracefully, spawn_owned_blocking_worker, spawn_owned_worker,
    WorkerAlreadyOwned, WorkerCompletion,
};

/// Cloneable handle for issuing SQL statements against the source-owned connection.
#[derive(Clone)]
pub struct SqliteSourceHandle {
    command_tx: Arc<RwLock<Option<mpsc::UnboundedSender<SqliteCommand>>>>,
    bound_command_tx: Option<mpsc::UnboundedSender<SqliteCommand>>,
    source_id: Arc<str>,
}

impl SqliteSourceHandle {
    async fn sender(&self) -> Result<mpsc::UnboundedSender<SqliteCommand>> {
        if let Some(sender) = &self.bound_command_tx {
            return Ok(sender.clone());
        }
        self.command_tx
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("source '{}' is not running", self.source_id))
    }

    async fn bind_to_current_worker(&self) -> Result<Self> {
        let mut handle = self.clone();
        handle.bound_command_tx = Some(self.sender().await?);
        Ok(handle)
    }

    /// Source ID this handle is associated with.
    pub fn source_id(&self) -> &str {
        &self.source_id
    }

    /// Execute a SQL statement.
    pub async fn execute(&self, sql: impl Into<String>) -> Result<usize> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender().await?.send(SqliteCommand::Execute {
            sql: sql.into(),
            response_tx,
        })?;
        response_rx
            .await
            .map_err(|_| anyhow!("sqlite thread closed response channel"))?
    }

    /// Execute a parameterized SQL statement with bind parameters.
    pub async fn execute_parameterized(
        &self,
        sql: impl Into<String>,
        params: Vec<SqliteParam>,
    ) -> Result<usize> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender()
            .await?
            .send(SqliteCommand::ExecuteParameterized {
                sql: sql.into(),
                params,
                response_tx,
            })?;
        response_rx
            .await
            .map_err(|_| anyhow!("sqlite thread closed response channel"))?
    }

    /// Execute a SQL script batch.
    pub async fn execute_batch(&self, sql: impl Into<String>) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender().await?.send(SqliteCommand::ExecuteBatch {
            sql: sql.into(),
            response_tx,
        })?;
        response_rx
            .await
            .map_err(|_| anyhow!("sqlite thread closed response channel"))?
    }

    /// Execute a query and return rows as JSON objects.
    pub async fn query(
        &self,
        sql: impl Into<String>,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender().await?.send(SqliteCommand::QueryRows {
            sql: sql.into(),
            response_tx,
        })?;
        response_rx
            .await
            .map_err(|_| anyhow!("sqlite thread closed response channel"))?
    }

    /// Execute a parameterized query and return rows as JSON objects.
    pub async fn query_parameterized(
        &self,
        sql: impl Into<String>,
        params: Vec<SqliteParam>,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender()
            .await?
            .send(SqliteCommand::QueryRowsParameterized {
                sql: sql.into(),
                params,
                response_tx,
            })?;
        response_rx
            .await
            .map_err(|_| anyhow!("sqlite thread closed response channel"))?
    }

    async fn begin_transaction(&self) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender()
            .await?
            .send(SqliteCommand::BeginTransaction { response_tx })?;
        response_rx
            .await
            .map_err(|_| anyhow!("sqlite thread closed response channel"))?
    }

    async fn commit_transaction(&self) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender()
            .await?
            .send(SqliteCommand::CommitTransaction { response_tx })?;
        response_rx
            .await
            .map_err(|_| anyhow!("sqlite thread closed response channel"))?
    }

    async fn rollback_transaction(&self) -> Result<()> {
        let (response_tx, response_rx) = oneshot::channel();
        self.sender()
            .await?
            .send(SqliteCommand::RollbackTransaction { response_tx })?;
        response_rx
            .await
            .map_err(|_| anyhow!("sqlite thread closed response channel"))?
    }

    /// Execute a sequence of SQL statements atomically.
    pub async fn execute_statements_in_transaction(&self, statements: Vec<String>) -> Result<()> {
        let handle = self.bind_to_current_worker().await?;
        handle.begin_transaction().await?;

        for statement in statements {
            if let Err(err) = handle.execute(statement).await {
                let _ = handle.rollback_transaction().await;
                return Err(err);
            }
        }

        handle.commit_transaction().await
    }

    /// Execute multiple parameterized statements atomically in a transaction.
    pub async fn execute_parameterized_in_transaction(
        &self,
        statements: Vec<(String, Vec<SqliteParam>)>,
    ) -> Result<()> {
        let handle = self.bind_to_current_worker().await?;
        handle.begin_transaction().await?;

        for (sql, params) in statements {
            if let Err(err) = handle.execute_parameterized(sql, params).await {
                let _ = handle.rollback_transaction().await;
                return Err(err);
            }
        }

        handle.commit_transaction().await
    }

    /// Execute user logic in a transaction scope.
    pub async fn transaction<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(SqliteTxHandle) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let handle = self.bind_to_current_worker().await?;
        handle.begin_transaction().await?;
        let tx_handle = SqliteTxHandle {
            handle: handle.clone(),
        };

        match f(tx_handle).await {
            Ok(value) => {
                handle.commit_transaction().await?;
                Ok(value)
            }
            Err(err) => {
                let _ = handle.rollback_transaction().await;
                Err(err)
            }
        }
    }
}

/// Transaction-scoped SQL handle.
#[derive(Clone)]
pub struct SqliteTxHandle {
    handle: SqliteSourceHandle,
}

impl SqliteTxHandle {
    pub async fn execute(&self, sql: impl Into<String>) -> Result<usize> {
        self.handle.execute(sql).await
    }

    pub async fn execute_batch(&self, sql: impl Into<String>) -> Result<()> {
        self.handle.execute_batch(sql).await
    }
}

/// SQLite source implementation.
pub struct SqliteSource {
    base: SourceBase,
    config: SqliteSourceConfig,
    command_tx: Arc<RwLock<Option<mpsc::UnboundedSender<SqliteCommand>>>>,
    cleanup_required: Mutex<bool>,
    database_commands: RwLock<Option<mpsc::UnboundedSender<SqliteCommand>>>,
    database_worker: RwLock<Option<JoinHandle<Result<()>>>>,
    rest_worker: RwLock<Option<JoinHandle<std::io::Result<()>>>>,
    shutdown_tx: watch::Sender<bool>,
    rest_address: RwLock<Option<SocketAddr>>,
}

impl SqliteSource {
    pub fn builder(id: impl Into<String>) -> SqliteSourceBuilder {
        SqliteSourceBuilder::new(id)
    }

    pub(crate) fn from_parts(base: SourceBase, config: SqliteSourceConfig) -> Result<Self> {
        Ok(Self {
            base,
            config,
            command_tx: Arc::new(RwLock::new(None)),
            cleanup_required: Mutex::new(false),
            database_commands: RwLock::new(None),
            database_worker: RwLock::new(None),
            rest_worker: RwLock::new(None),
            shutdown_tx: watch::channel(false).0,
            rest_address: RwLock::new(None),
        })
    }

    /// Get a handle for issuing SQL operations against this source.
    pub fn handle(&self) -> SqliteSourceHandle {
        SqliteSourceHandle {
            command_tx: self.command_tx.clone(),
            bound_command_tx: None,
            source_id: Arc::from(self.base.id.as_str()),
        }
    }

    /// Last bound REST listener address, cleared after successful cleanup.
    pub async fn bound_rest_address(&self) -> Option<SocketAddr> {
        *self.rest_address.read().await
    }

    async fn join_database_worker(&self) -> Result<()> {
        match join_owned_worker_gracefully(
            &mut *self.database_worker.write().await,
            Duration::from_secs(5),
        )
        .await
        .context("SQLite database worker cleanup")?
        {
            WorkerCompletion::Completed(result) => result.context("SQLite database worker failed"),
            WorkerCompletion::Absent => Ok(()),
            WorkerCompletion::Cancelled => Err(anyhow!("SQLite database worker was cancelled")),
        }
    }

    async fn join_rest_worker(&self) -> Result<()> {
        match join_owned_worker_gracefully(
            &mut *self.rest_worker.write().await,
            Duration::from_secs(5),
        )
        .await
        .context("SQLite REST worker cleanup")?
        {
            WorkerCompletion::Completed(result) => result.context("SQLite REST server failed"),
            WorkerCompletion::Absent => Ok(()),
            WorkerCompletion::Cancelled => Err(anyhow!("SQLite REST worker was cancelled")),
        }
    }

    async fn start_workers(&self) -> Result<()> {
        self.shutdown_tx.send_replace(false);
        let (command_tx, command_rx) = mpsc::unbounded_channel::<SqliteCommand>();
        *self.database_commands.write().await = Some(command_tx.clone());
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<thread::ChangeEvent>();
        let (ready, readiness) = oneshot::channel();

        let thread_config = thread::SqliteThreadConfig {
            path: self.config.path.clone(),
            tables: self.config.tables.clone(),
        };
        let source_id_for_thread = self.base.id.clone();
        spawn_owned_blocking_worker(&self.database_worker, move || {
            let result = thread::run_sqlite_thread(thread_config, command_rx, event_tx, ready);
            if let Err(err) = &result {
                log::error!("sqlite source thread '{source_id_for_thread}' failed: {err}");
            }
            result
        })
        .await?;
        if readiness.await.is_err() {
            self.join_database_worker().await?;
            return Err(anyhow!(
                "SQLite database worker exited before reporting readiness"
            ));
        }

        let listener = if let Some(config) = &self.config.rest_api {
            Some(tokio::net::TcpListener::bind((config.host.as_str(), config.port)).await?)
        } else {
            None
        };

        let configured_keys = self
            .config
            .table_keys
            .iter()
            .map(|item| (item.table.clone(), item.key_columns.clone()))
            .collect::<HashMap<_, _>>();

        let source_id = self.base.id.clone();
        let base = self.base.clone_shared();
        let shutdown = self.shutdown_tx.subscribe();
        let instance_id = self
            .base
            .context()
            .await
            .map(|ctx| ctx.instance_id)
            .unwrap_or_default();

        let span = tracing::info_span!(
            "sqlite_source_dispatcher",
            instance_id = %instance_id,
            component_id = %source_id,
            component_type = "source"
        );
        spawn_owned_worker(
            &self.base.task_handle,
            async move {
                while let Some(event) = event_rx.recv().await {
                    let configured = configured_keys.get(&event.table).map(|v| v.as_slice());
                    let change =
                        convert::change_event_to_source_change(event, &source_id, configured);
                    let mut profiling = drasi_lib::profiling::ProfilingMetadata::new();
                    profiling.source_send_ns = Some(drasi_lib::profiling::timestamp_ns());

                    let wrapper = SourceEventWrapper::with_profiling(
                        source_id.clone(),
                        SourceEvent::Change(change),
                        chrono::Utc::now(),
                        profiling,
                        base.next_sequence(),
                    );
                    if let Err(err) = base.dispatch_event(wrapper).await {
                        log::debug!("failed dispatching sqlite change for '{source_id}': {err}");
                    }
                }
                let stopping = *shutdown.borrow();
                if !stopping {
                    base.set_status(
                        ComponentStatus::Error,
                        Some("SQLite database worker exited unexpectedly".into()),
                    )
                    .await;
                }
            }
            .instrument(span),
        )
        .await?;

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("SQLite source running".to_string()),
            )
            .await;

        if let Some(listener) = listener {
            let address = listener.local_addr()?;
            let mut handle = self.handle();
            handle.bound_command_tx = Some(command_tx.clone());
            let tables = self.config.tables.clone();
            let table_keys = self.config.table_keys.clone();
            let shutdown = self.shutdown_tx.subscribe();
            let status = self.base.status_handle();
            spawn_owned_worker(&self.rest_worker, async move {
                let result =
                    rest_api::run_rest_api(listener, handle, tables, table_keys, shutdown).await;
                if let Err(error) = &result {
                    log::error!("SQLite REST server failed: {error}");
                    status
                        .set_status(
                            ComponentStatus::Error,
                            Some(format!("SQLite REST server failed: {error}")),
                        )
                        .await;
                }
                result
            })
            .await?;
            *self.rest_address.write().await = Some(address);
        }
        *self.command_tx.write().await = Some(command_tx);
        Ok(())
    }
}

#[async_trait]
impl Source for SqliteSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "sqlite"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        use crate::descriptor::SqliteSourceConfigDto;
        self.base
            .properties_or_serialize(&SqliteSourceConfigDto::from(&self.config))
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        let mut cleanup_required = self.cleanup_required.lock().await;
        if *cleanup_required {
            let stopping = *self.shutdown_tx.borrow();
            let database_running = self
                .database_worker
                .read()
                .await
                .as_ref()
                .is_some_and(|worker| !worker.is_finished());
            let dispatcher_running = self
                .base
                .task_handle
                .read()
                .await
                .as_ref()
                .is_some_and(|worker| !worker.is_finished());
            let rest_running = self.config.rest_api.is_none()
                || self
                    .rest_worker
                    .read()
                    .await
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished());
            if !stopping
                && database_running
                && dispatcher_running
                && rest_running
                && self.command_tx.read().await.is_some()
                && self.base.get_status().await == ComponentStatus::Running
            {
                return Ok(());
            }
            return Err(WorkerAlreadyOwned.into());
        }
        if self.database_worker.read().await.is_some()
            || self.rest_worker.read().await.is_some()
            || self.base.task_handle.read().await.is_some()
            || self.database_commands.read().await.is_some()
        {
            return Err(WorkerAlreadyOwned.into());
        }
        *cleanup_required = true;
        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting SQLite source".into()),
            )
            .await;
        let result = self.start_workers().await;
        if let Err(error) = &result {
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!("SQLite source failed to start: {error}")),
                )
                .await;
        }
        result
    }

    async fn stop(&self) -> Result<()> {
        let mut cleanup_required = self.cleanup_required.lock().await;
        *cleanup_required = true;
        self.shutdown_tx.send_replace(true);
        self.command_tx.write().await.take();

        self.base
            .set_status(
                ComponentStatus::Stopping,
                Some("Stopping SQLite source".to_string()),
            )
            .await;

        self.join_rest_worker().await?;
        if let Some(sender) = self.database_commands.write().await.take() {
            if sender.send(SqliteCommand::Shutdown).is_err() {
                log::debug!("SQLite database command channel already closed; joining its worker");
            }
        }
        self.join_database_worker().await?;
        if matches!(
            join_owned_worker_gracefully(
                &mut *self.base.task_handle.write().await,
                Duration::from_secs(5),
            )
            .await
            .context("SQLite dispatcher cleanup")?,
            WorkerCompletion::Cancelled
        ) {
            return Err(anyhow!("SQLite dispatcher was cancelled"));
        }
        self.base.stop_common().await?;
        *self.rest_address.write().await = None;
        *cleanup_required = false;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(&settings, "SQLite")
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(
        &self,
        provider: Box<dyn drasi_lib::bootstrap::BootstrapProvider + 'static>,
    ) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "sqlite-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::SqliteSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
