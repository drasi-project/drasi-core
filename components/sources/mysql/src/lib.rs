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

//! MySQL replication source for Drasi.
//!
//! This source streams MySQL binlog changes and emits Drasi SourceChange events.

mod config;
mod decoder;
mod stream;
#[cfg(test)]
mod tests;
mod types;

pub use config::{MySqlSourceConfig, SslMode, StartPosition, TableKeyConfig};

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;
use log::{error, info};

use drasi_lib::channels::{ComponentEvent, ComponentStatus, ComponentType, DispatchMode};
use drasi_lib::sources::base::{SourceBase, SourceBaseParams};
use drasi_lib::sources::Source;
use drasi_lib::{bootstrap::BootstrapProvider, channels::SubscriptionResponse};

use crate::stream::ReplicationStream;

pub struct MySqlReplicationSource {
    base: SourceBase,
    config: MySqlSourceConfig,
}

impl MySqlReplicationSource {
    pub fn new(id: impl Into<String>, config: MySqlSourceConfig) -> Result<Self> {
        let id = id.into();
        let params = SourceBaseParams::new(id);
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }

    pub fn with_dispatch(
        id: impl Into<String>,
        config: MySqlSourceConfig,
        dispatch_mode: Option<DispatchMode>,
        dispatch_buffer_capacity: Option<usize>,
    ) -> Result<Self> {
        let id = id.into();
        let mut params = SourceBaseParams::new(id);
        if let Some(mode) = dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        Ok(Self {
            base: SourceBase::new(params)?,
            config,
        })
    }

    pub fn builder(id: impl Into<String>) -> MySqlSourceBuilder {
        MySqlSourceBuilder::new(id)
    }
}

#[async_trait]
impl Source for MySqlReplicationSource {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "mysql"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let mut props = HashMap::new();
        props.insert(
            "host".to_string(),
            serde_json::Value::String(self.config.host.clone()),
        );
        props.insert(
            "port".to_string(),
            serde_json::Value::Number(self.config.port.into()),
        );
        props.insert(
            "database".to_string(),
            serde_json::Value::String("MySQL".to_string()),
        );
        props.insert(
            "database_name".to_string(),
            serde_json::Value::String(self.config.database.clone()),
        );
        props.insert(
            "user".to_string(),
            serde_json::Value::String(self.config.user.clone()),
        );
        props.insert(
            "tables".to_string(),
            serde_json::Value::Array(
                self.config
                    .tables
                    .iter()
                    .map(|t| serde_json::Value::String(t.clone()))
                    .collect(),
            ),
        );
        props.insert(
            "ssl_mode".to_string(),
            serde_json::Value::String(format!("{:?}", self.config.ssl_mode)),
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

        self.base.set_status(ComponentStatus::Starting).await;
        info!("Starting MySQL replication source: {}", self.base.id);

        let config = self.config.clone();
        let source_id = self.base.id.clone();
        let base = self.base.clone_shared();
        let status_tx = self.base.status_tx();
        let status_clone = self.base.status.clone();

        let task = tokio::spawn(async move {
            let mut stream = ReplicationStream::new(config, source_id.clone(), base);
            if let Err(e) = stream.run().await {
                error!("Replication task failed for {source_id}: {e}");
                *status_clone.write().await = ComponentStatus::Error;
                if let Some(ref tx) = *status_tx.read().await {
                    let _ = tx
                        .send(ComponentEvent {
                            component_id: source_id,
                            component_type: ComponentType::Source,
                            status: ComponentStatus::Error,
                            timestamp: chrono::Utc::now(),
                            message: Some(format!("Replication failed: {e}")),
                        })
                        .await;
                }
            }
        });

        *self.base.task_handle.write().await = Some(task);
        self.base.set_status(ComponentStatus::Running).await;

        self.base
            .send_component_event(
                ComponentStatus::Running,
                Some("MySQL replication started".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if self.base.get_status().await != ComponentStatus::Running {
            return Ok(());
        }

        info!("Stopping MySQL replication source: {}", self.base.id);

        self.base.set_status(ComponentStatus::Stopping).await;

        if let Some(task) = self.base.task_handle.write().await.take() {
            task.abort();
        }

        self.base.set_status(ComponentStatus::Stopped).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopped,
                Some("MySQL replication stopped".to_string()),
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
        self.base.subscribe_with_bootstrap(&settings, "MySQL").await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn initialize(&self, context: drasi_lib::context::SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(&self, provider: Box<dyn BootstrapProvider + 'static>) {
        self.base.set_bootstrap_provider(provider).await;
    }
}

/// Builder for MySQL source configuration.
///
/// Provides a fluent API for constructing MySQL source configurations
/// with sensible defaults.
pub struct MySqlSourceBuilder {
    id: String,
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    tables: Vec<String>,
    ssl_mode: SslMode,
    table_keys: Vec<TableKeyConfig>,
    start_position: StartPosition,
    server_id: u32,
    heartbeat_interval_seconds: u64,
    dispatch_mode: Option<DispatchMode>,
    dispatch_buffer_capacity: Option<usize>,
    bootstrap_provider: Option<Box<dyn BootstrapProvider + 'static>>,
    auto_start: bool,
}

impl MySqlSourceBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            host: "localhost".to_string(),
            port: 3306,
            database: String::new(),
            user: String::new(),
            password: String::new(),
            tables: Vec::new(),
            ssl_mode: SslMode::default(),
            table_keys: Vec::new(),
            start_position: StartPosition::default(),
            server_id: 65535,
            heartbeat_interval_seconds: 30,
            dispatch_mode: None,
            dispatch_buffer_capacity: None,
            bootstrap_provider: None,
            auto_start: true,
        }
    }

    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = password.into();
        self
    }

    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.tables = tables;
        self
    }

    pub fn add_table(mut self, table: impl Into<String>) -> Self {
        self.tables.push(table.into());
        self
    }

    pub fn with_ssl_mode(mut self, ssl_mode: SslMode) -> Self {
        self.ssl_mode = ssl_mode;
        self
    }

    pub fn with_table_keys(mut self, table_keys: Vec<TableKeyConfig>) -> Self {
        self.table_keys = table_keys;
        self
    }

    pub fn add_table_key(mut self, table_key: TableKeyConfig) -> Self {
        self.table_keys.push(table_key);
        self
    }

    pub fn with_start_position(mut self, start_position: StartPosition) -> Self {
        self.start_position = start_position;
        self
    }

    pub fn with_server_id(mut self, server_id: u32) -> Self {
        self.server_id = server_id;
        self
    }

    pub fn with_heartbeat_interval_seconds(mut self, seconds: u64) -> Self {
        self.heartbeat_interval_seconds = seconds;
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

    pub fn build(self) -> Result<MySqlReplicationSource> {
        let config = MySqlSourceConfig {
            host: self.host,
            port: self.port,
            database: self.database,
            user: self.user,
            password: self.password,
            tables: self.tables,
            ssl_mode: self.ssl_mode,
            table_keys: self.table_keys,
            start_position: self.start_position,
            server_id: self.server_id,
            heartbeat_interval_seconds: self.heartbeat_interval_seconds,
        };
        config.validate()?;

        let mut params = SourceBaseParams::new(self.id);
        if let Some(mode) = self.dispatch_mode {
            params = params.with_dispatch_mode(mode);
        }
        if let Some(capacity) = self.dispatch_buffer_capacity {
            params = params.with_dispatch_buffer_capacity(capacity);
        }
        if let Some(provider) = self.bootstrap_provider {
            params = params.with_bootstrap_provider(provider);
        }
        params = params.with_auto_start(self.auto_start);

        Ok(MySqlReplicationSource {
            base: SourceBase::new(params)?,
            config,
        })
    }
}
