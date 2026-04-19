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

pub use drasi_oracle_common::config;
pub use drasi_oracle_common::connection;
pub use drasi_oracle_common::error;
pub use drasi_oracle_common::keys;
pub use drasi_oracle_common::logminer;
pub use drasi_oracle_common::scn;
pub use drasi_oracle_common::types;
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
use drasi_lib::state_store::StateStoreProvider;
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
    state_store: Arc<RwLock<Option<Arc<dyn StateStoreProvider>>>>,
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
    shutdown_tx: Arc<Mutex<Option<watch::Sender<bool>>>>,
    bootstrap_sync: Arc<AsyncMutex<BootstrapSyncState>>,
}

impl OracleSource {
    pub fn new(id: impl Into<String>, config: OracleSourceConfig) -> Result<Self> {
        let source_id = id.into();
        let params = SourceBaseParams::new(&source_id);

        Ok(Self {
            source_id,
            config,
            base: SourceBase::new(params)?,
            state_store: Arc::new(RwLock::new(None)),
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(Mutex::new(None)),
            bootstrap_sync: Arc::new(AsyncMutex::new(BootstrapSyncState::default())),
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
        let mut props = HashMap::new();
        props.insert("host".to_string(), self.config.host.clone().into());
        props.insert("port".to_string(), self.config.port.into());
        props.insert("service".to_string(), self.config.database.clone().into());
        props.insert("user".to_string(), self.config.user.clone().into());
        props.insert(
            "tables".to_string(),
            serde_json::Value::Array(
                self.config
                    .tables
                    .iter()
                    .cloned()
                    .map(serde_json::Value::String)
                    .collect(),
            ),
        );
        props.insert(
            "poll_interval_ms".to_string(),
            serde_json::Value::Number(self.config.poll_interval_ms.into()),
        );
        props
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
        let state_store = self.state_store.read().await.clone();
        let bootstrap_sync = self.bootstrap_sync.clone();
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
                state_store,
                bootstrap_sync,
                shutdown_rx,
            )
            .await
            {
                log::error!("Oracle LogMiner stream failed for {source_id}: {error}");
                let _ = base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("Oracle LogMiner stream failed: {error}")),
                    )
                    .await;
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
        let query_id = settings.query_id.clone();
        let source_id = self.base.id.clone();
        let mut bootstrap_properties = HashMap::new();
        let receiver = self.base.create_streaming_receiver().await?;

        if settings.enable_bootstrap {
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

        let bootstrap_receiver = if settings.enable_bootstrap {
            self.base
                .create_bootstrap_receiver(&settings, "Oracle", bootstrap_properties)
                .await?
        } else {
            None
        };

        Ok(drasi_lib::channels::SubscriptionResponse {
            query_id,
            source_id,
            receiver,
            bootstrap_receiver,
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context.clone()).await;
        if let Some(state_store) = context.state_store {
            *self.state_store.write().await = Some(state_store);
        }
    }

    async fn set_bootstrap_provider(&self, provider: Box<dyn BootstrapProvider + 'static>) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

pub struct OracleSourceBuilder {
    id: String,
    config: OracleSourceConfig,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>,
    state_store: Option<Arc<dyn StateStoreProvider>>,
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
            state_store: None,
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

    pub fn with_state_store(mut self, state_store: Arc<dyn StateStoreProvider>) -> Self {
        self.state_store = Some(state_store);
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
            state_store: Arc::new(RwLock::new(self.state_store)),
            task_handle: Arc::new(RwLock::new(None)),
            shutdown_tx: Arc::new(Mutex::new(None)),
            bootstrap_sync: Arc::new(AsyncMutex::new(BootstrapSyncState::default())),
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
