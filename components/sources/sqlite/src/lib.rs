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

//! SQLite source plugin for Drasi.
//!
//! This source owns an embedded SQLite connection running in a dedicated thread.
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
mod rest_api;
mod thread;

pub use config::{
    RestApiConfig, SqliteSourceBuilder, SqliteSourceConfig, StartFrom, TableKeyConfig,
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use drasi_lib::channels::{ComponentStatus, SourceEvent, SourceEventWrapper, SubscriptionResponse};
use drasi_lib::sources::base::SourceBase;
use drasi_lib::Source;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::Instrument;

use crate::thread::SqliteCommand;

/// Cloneable handle for issuing SQL statements against the source-owned connection.
#[derive(Clone)]
pub struct SqliteSourceHandle {
    command_tx: Arc<RwLock<Option<mpsc::UnboundedSender<SqliteCommand>>>>,
    source_id: Arc<str>,
}

impl SqliteSourceHandle {
    async fn sender(&self) -> Result<mpsc::UnboundedSender<SqliteCommand>> {
        self.command_tx
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow!("source '{}' is not running", self.source_id))
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
        self.begin_transaction().await?;

        for statement in statements {
            if let Err(err) = self.execute(statement).await {
                let _ = self.rollback_transaction().await;
                return Err(err);
            }
        }

        self.commit_transaction().await
    }

    /// Execute user logic in a transaction scope.
    pub async fn transaction<F, Fut, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(SqliteTxHandle) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        self.begin_transaction().await?;
        let tx_handle = SqliteTxHandle {
            handle: self.clone(),
        };

        match f(tx_handle).await {
            Ok(value) => {
                self.commit_transaction().await?;
                Ok(value)
            }
            Err(err) => {
                let _ = self.rollback_transaction().await;
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
    thread_handle: Arc<RwLock<Option<std::thread::JoinHandle<()>>>>,
    rest_shutdown_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
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
            thread_handle: Arc::new(RwLock::new(None)),
            rest_shutdown_tx: Arc::new(RwLock::new(None)),
        })
    }

    /// Get a handle for issuing SQL operations against this source.
    pub fn handle(&self) -> SqliteSourceHandle {
        SqliteSourceHandle {
            command_tx: self.command_tx.clone(),
            source_id: Arc::from(self.base.id.as_str()),
        }
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
        let mut props = HashMap::new();
        props.insert(
            "path".to_string(),
            match &self.config.path {
                Some(path) => serde_json::Value::String(path.clone()),
                None => serde_json::Value::String(":memory:".to_string()),
            },
        );

        if let Some(tables) = &self.config.tables {
            props.insert(
                "tables".to_string(),
                serde_json::Value::Array(
                    tables
                        .iter()
                        .map(|table| serde_json::Value::String(table.clone()))
                        .collect(),
                ),
            );
        }
        props.insert(
            "rest_api_enabled".to_string(),
            serde_json::Value::Bool(self.config.rest_api.is_some()),
        );
        props
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base
            .set_status_with_event(
                ComponentStatus::Starting,
                Some("Starting SQLite source".to_string()),
            )
            .await?;

        let (command_tx, command_rx) = mpsc::unbounded_channel::<SqliteCommand>();
        *self.command_tx.write().await = Some(command_tx.clone());
        let (event_tx, mut event_rx) = mpsc::unbounded_channel::<thread::ChangeEvent>();

        let thread_config = thread::SqliteThreadConfig {
            path: self.config.path.clone(),
            tables: self.config.tables.clone(),
        };
        let source_id_for_thread = self.base.id.clone();
        let handle = std::thread::spawn(move || {
            if let Err(err) = thread::run_sqlite_thread(thread_config, command_rx, event_tx) {
                log::error!("sqlite source thread '{source_id_for_thread}' failed: {err}");
            }
        });
        *self.thread_handle.write().await = Some(handle);

        let configured_keys = self
            .config
            .table_keys
            .iter()
            .map(|item| (item.table.clone(), item.key_columns.clone()))
            .collect::<HashMap<_, _>>();

        let source_id = self.base.id.clone();
        let dispatchers = self.base.dispatchers.clone();
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
        let task = tokio::spawn(
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
                    );
                    if let Err(err) =
                        SourceBase::dispatch_from_task(dispatchers.clone(), wrapper, &source_id)
                            .await
                    {
                        log::debug!("failed dispatching sqlite change for '{source_id}': {err}");
                    }
                }
            }
            .instrument(span),
        );
        *self.base.task_handle.write().await = Some(task);

        if let Some(rest_config) = &self.config.rest_api {
            let (shutdown_tx, shutdown_rx) = oneshot::channel();
            rest_api::start_rest_api(
                rest_config.clone(),
                self.handle(),
                self.config.tables.clone(),
                self.config.table_keys.clone(),
                shutdown_rx,
            )
            .await?;
            *self.rest_shutdown_tx.write().await = Some(shutdown_tx);
        }

        self.base
            .set_status_with_event(
                ComponentStatus::Running,
                Some("SQLite source running".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if self.base.get_status().await != ComponentStatus::Running {
            return Ok(());
        }

        self.base
            .set_status_with_event(
                ComponentStatus::Stopping,
                Some("Stopping SQLite source".to_string()),
            )
            .await?;

        if let Some(rest_shutdown_tx) = self.rest_shutdown_tx.write().await.take() {
            let _ = rest_shutdown_tx.send(());
        }

        if let Some(sender) = self.command_tx.write().await.take() {
            let _ = sender.send(SqliteCommand::Shutdown);
        }

        if let Some(handle) = self.thread_handle.write().await.take() {
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }

        if let Some(task) = self.base.task_handle.write().await.take() {
            task.abort();
        }

        self.base
            .set_status_with_event(
                ComponentStatus::Stopped,
                Some("SQLite source stopped".to_string()),
            )
            .await?;

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
