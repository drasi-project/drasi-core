#![allow(unexpected_cfgs)]
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

//! Oracle LogMiner source plugin for Drasi.

pub mod descriptor;
pub mod stream;

pub use drasi_oracle_common::{
    validate_sql_identifier, AuthMode, OracleConnection, OracleError, OracleErrorKind,
    OracleSourceConfig, PrimaryKeyCache, Scn, SslMode, StartPosition, TableKeyConfig,
    ORACLE_BOOTSTRAP_SCN_CONTEXT_PROPERTY,
};

use anyhow::Result;
use async_trait::async_trait;
use drasi_lib::bootstrap::BootstrapProvider;
use drasi_lib::channels::{ComponentStatus, DispatchMode};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::Source;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::{watch, Mutex as AsyncMutex, RwLock};

#[derive(Default)]
pub(crate) struct BootstrapSyncState {
    pub pending_scn: Option<Scn>,
}

pub struct OracleSource {
    source_id: String,
    config: OracleSourceConfig,
    base: SourceBase,
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    shutdown_tx: Arc<Mutex<Option<watch::Sender<bool>>>>,
    bootstrap_sync: Arc<AsyncMutex<BootstrapSyncState>>,
    /// Per-subscriber resume positions from checkpoint recovery.
    /// Populated during subscribe() when a query provides resume_from bytes.
    /// The LogMiner poll loop uses the minimum SCN as its start point.
    subscriber_resume_scns: Arc<RwLock<HashMap<String, Scn>>>,
}

impl OracleSource {
    pub fn new(id: impl Into<String>, config: OracleSourceConfig) -> Result<Self> {
        let source_id = id.into();
        let params = SourceBaseParams::new(&source_id);

        Ok(Self {
            source_id,
            config,
            base: SourceBase::new(params)?,
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(Mutex::new(None)),
            bootstrap_sync: Arc::new(AsyncMutex::new(BootstrapSyncState::default())),
            subscriber_resume_scns: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    pub fn builder(id: impl Into<String>) -> OracleSourceBuilder {
        OracleSourceBuilder::new(id)
    }
}

#[async_trait]
impl Source for OracleSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "oracle"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        use crate::descriptor::OracleSourceConfigDto;

        self.base
            .properties_or_serialize(&OracleSourceConfigDto::from(&self.config))
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting Oracle source".to_string()),
            )
            .await;
        log::info!("Starting Oracle source '{}'", self.base.id);

        self.bootstrap_sync.lock().await.pending_scn = None;

        let validation_config = self.config.clone();
        let validation_result =
            tokio::task::spawn_blocking(move || stream::validate_connection(&validation_config))
                .await
                .map_err(|error| {
                    anyhow::anyhow!("Oracle startup validation task failed: {error}")
                })?;
        if let Err(error) = validation_result {
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!("Oracle startup validation failed: {error}")),
                )
                .await;
            return Err(error);
        }

        let source_id = self.base.id.clone();
        let config = self.config.clone();
        let base = self.base.clone_shared();
        let dispatchers = self.base.dispatchers.clone();
        let bootstrap_sync = self.bootstrap_sync.clone();
        let subscriber_resume_scns = self.subscriber_resume_scns.clone();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        *self
            .shutdown_tx
            .lock()
            .expect("oracle shutdown mutex poisoned") = Some(shutdown_tx);

        let task_handle = tokio::spawn(async move {
            if let Err(error) = stream::run_logminer_stream(
                source_id.clone(),
                config,
                dispatchers,
                bootstrap_sync,
                subscriber_resume_scns,
                base,
                shutdown_rx,
            )
            .await
            {
                log::error!("Oracle LogMiner stream failed for {source_id}: {error}");
            }
        });

        *self.task_handle.write().await = Some(task_handle);
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Oracle source started".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        log::info!("Stopping Oracle source '{}'", self.base.id);
        if let Some(shutdown_tx) = self
            .shutdown_tx
            .lock()
            .expect("oracle shutdown mutex poisoned")
            .take()
        {
            if let Err(error) = shutdown_tx.send(true) {
                log::warn!("Failed to send Oracle shutdown signal: {error}");
            }
        }

        if let Some(handle) = self.task_handle.write().await.take() {
            match tokio::time::timeout(std::time::Duration::from_secs(5), handle).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::warn!("Oracle source task panicked: {error}"),
                Err(_) => log::warn!("Oracle source task did not stop within timeout"),
            }
        }

        // Clear stale dispatchers so a subsequent start()+subscribe() cycle
        // does not dispatch events to dead receivers.
        self.base.clear_dispatchers().await;

        // Clear stale resume positions — they will be repopulated by subscribe()
        // on the next lifecycle with fresh checkpoint data.
        self.subscriber_resume_scns.write().await.clear();

        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Oracle source stopped".to_string()),
            )
            .await;
        Ok(())
    }

    async fn subscribe(
        &self,
        settings: drasi_lib::config::SourceSubscriptionSettings,
    ) -> Result<drasi_lib::channels::SubscriptionResponse> {
        let mut bootstrap_properties = HashMap::new();

        // If the subscriber has a resume_from position, deserialize it to an SCN
        // and store it keyed by query_id. The LogMiner poll loop computes the min
        // across all active subscribers to decide where to start polling from.
        if let Some(ref resume_bytes) = settings.resume_from {
            match Scn::from_bytes(resume_bytes) {
                Ok(scn) => {
                    log::info!(
                        "Subscriber '{}' requesting resume from SCN {} on source '{}'",
                        settings.query_id,
                        scn,
                        self.base.id
                    );
                    self.subscriber_resume_scns
                        .write()
                        .await
                        .insert(settings.query_id.clone(), scn);
                }
                Err(e) => {
                    log::warn!(
                        "Failed to deserialize resume_from bytes to SCN for subscriber '{}': {e}",
                        settings.query_id
                    );
                }
            }
        }

        // resume_from overrides bootstrap: a resuming query already has base
        // state in its persistent index and just needs replay from the
        // requested sequence. Re-bootstrapping would corrupt that state.
        let should_bootstrap = settings.resume_from.is_none() && settings.enable_bootstrap;

        if should_bootstrap {
            let bootstrap_config = self.config.clone();
            let bootstrap_scn =
                tokio::task::spawn_blocking(move || stream::fetch_bootstrap_scn(&bootstrap_config))
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("Oracle bootstrap SCN task failed: {error}")
                    })??;

            self.bootstrap_sync.lock().await.pending_scn = Some(bootstrap_scn);
            bootstrap_properties.insert(
                ORACLE_BOOTSTRAP_SCN_CONTEXT_PROPERTY.to_string(),
                serde_json::Value::Number(serde_json::Number::from(bootstrap_scn.0)),
            );
        }

        self.base
            .subscribe_with_bootstrap_context(&settings, "Oracle", bootstrap_properties)
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
        // Oracle SCNs are 8-byte big-endian u64 — byte-lexicographic comparison is correct.
        self.base
            .set_position_comparator(drasi_lib::sources::ByteLexPositionComparator)
            .await;
    }

    async fn set_bootstrap_provider(&self, provider: Box<dyn BootstrapProvider + 'static>) {
        self.base.set_bootstrap_provider(provider).await;
    }

    async fn remove_position_handle(&self, query_id: &str) {
        // Remove the per-subscriber resume SCN so the LogMiner poll loop no longer
        // pins its start position on behalf of this query.
        self.subscriber_resume_scns.write().await.remove(query_id);
        self.base.remove_position_handle(query_id).await;
    }
}

pub struct OracleSourceBuilder {
    id: String,
    config: OracleSourceConfig,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl OracleSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            config: OracleSourceConfig::default(),
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.config.host = host.into();
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.config.port = port;
        self
    }

    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.config.database = database.into();
        self
    }

    pub fn with_service(self, service: impl Into<String>) -> Self {
        self.with_database(service)
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.config.user = user.into();
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.config.password = password.into();
        self
    }

    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.config.tables = tables;
        self
    }

    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        self.config.tables.push(table.into());
        self
    }

    pub fn with_poll_interval_ms(mut self, interval_ms: u64) -> Self {
        self.config.poll_interval_ms = interval_ms;
        self
    }

    pub fn with_table_keys(mut self, table_keys: Vec<TableKeyConfig>) -> Self {
        self.config.table_keys = table_keys;
        self
    }

    pub fn with_table_key(mut self, table: impl Into<String>, key_columns: Vec<String>) -> Self {
        self.config.table_keys.push(TableKeyConfig {
            table: table.into(),
            key_columns,
        });
        self
    }

    pub fn with_start_position(mut self, start_position: StartPosition) -> Self {
        self.config.start_position = start_position;
        self
    }

    pub fn with_ssl_mode(mut self, ssl_mode: SslMode) -> Self {
        self.config.ssl_mode = ssl_mode;
        self
    }

    pub fn with_dispatch_mode(mut self, mode: DispatchMode) -> Self {
        self.dispatch_mode = Some(mode);
        self
    }

    pub fn with_dispatch_buffer_capacity(mut self, capacity: usize) -> Self {
        self.dispatch_buffer_capacity = Some(capacity);
        self
    }

    pub fn with_bootstrap_provider(mut self, provider: impl BootstrapProvider + 'static) -> Self {
        self.bootstrap_provider = Some(Box::new(provider));
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    pub fn with_config(mut self, config: OracleSourceConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> Result<OracleSource> {
        if self.config.tables.is_empty() {
            anyhow::bail!("At least one Oracle table must be configured");
        }
        self.config.validate()?;

        let mut params = SourceBaseParams::new(&self.id).with_auto_start(self.auto_start);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }

        Ok(OracleSource {
            source_id: self.id.clone(),
            config: self.config,
            base: SourceBase::new(params)?,
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(Mutex::new(None)),
            bootstrap_sync: Arc::new(AsyncMutex::new(BootstrapSyncState::default())),
            subscriber_resume_scns: Arc::new(RwLock::new(HashMap::new())),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_basic() {
        let source = OracleSource::builder("oracle-source")
            .with_user("system")
            .with_password("secret")
            .with_table("HR.EMPLOYEES")
            .build()
            .unwrap();

        assert_eq!(source.type_name(), "oracle");
        assert_eq!(source.config.database, "FREEPDB1");
        assert_eq!(source.config.tables, vec!["HR.EMPLOYEES"]);
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "oracle-source",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [descriptor::OracleSourceDescriptor],
    reaction_descriptors = [],
    bootstrap_descriptors = [],
);
