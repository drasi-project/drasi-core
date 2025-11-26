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

//! PostgreSQL source plugin for Drasi
//!
//! This plugin provides the PostgreSQL replication source implementation
//! for the Drasi plugin architecture.

pub mod bootstrap;
pub mod config;
pub mod connection;
pub mod decoder;
pub mod protocol;
pub mod scram;
pub mod stream;
pub mod types;

pub use config::PostgresSourceConfig;

use anyhow::Result;
use async_trait::async_trait;
use log::{error, info};
use std::sync::Arc;
use tokio::sync::RwLock;

use drasi_lib::channels::*;
use drasi_lib::config::SourceConfig;
use drasi_lib::sources::{base::SourceBase, Source};

pub struct PostgresReplicationSource {
    base: SourceBase,
}

impl PostgresReplicationSource {
    pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
        Ok(Self {
            base: SourceBase::new(config, event_tx)?,
        })
    }

    fn parse_config(&self) -> Result<PostgresSourceConfig> {
        match &self.base.config.config {
            drasi_lib::config::SourceSpecificConfig::Postgres(postgres_config) => {
                // Deserialize the HashMap into PostgresSourceConfig
                let config: PostgresSourceConfig = serde_json::from_value(
                    serde_json::to_value(postgres_config)?
                )?;
                Ok(config)
            }
            _ => Err(anyhow::anyhow!("Invalid config type for PostgreSQL source")),
        }
    }
}

#[async_trait]
impl Source for PostgresReplicationSource {
    async fn start(&self) -> Result<()> {
        if self.base.get_status().await == ComponentStatus::Running {
            return Ok(());
        }

        self.base.set_status(ComponentStatus::Starting).await;
        info!(
            "Starting PostgreSQL replication source: {}",
            self.base.config.id
        );

        let config = self.parse_config()?;
        let source_id = self.base.config.id.clone();
        let dispatchers = self.base.dispatchers.clone();
        let event_tx = self.base.event_tx.clone();
        let status_clone = self.base.status.clone();

        let task = tokio::spawn(async move {
            if let Err(e) = run_replication(
                source_id.clone(),
                config,
                dispatchers,
                event_tx.clone(),
                status_clone.clone(),
            )
            .await
            {
                error!("Replication task failed for {}: {}", source_id, e);
                *status_clone.write().await = ComponentStatus::Error;
                let _ = event_tx
                    .send(ComponentEvent {
                        component_id: source_id,
                        component_type: ComponentType::Source,
                        status: ComponentStatus::Error,
                        timestamp: chrono::Utc::now(),
                        message: Some(format!("Replication failed: {}", e)),
                    })
                    .await;
            }
        });

        *self.base.task_handle.write().await = Some(task);
        self.base.set_status(ComponentStatus::Running).await;

        self.base
            .send_component_event(
                ComponentStatus::Running,
                Some("PostgreSQL replication started".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        if self.base.get_status().await != ComponentStatus::Running {
            return Ok(());
        }

        info!(
            "Stopping PostgreSQL replication source: {}",
            self.base.config.id
        );

        self.base.set_status(ComponentStatus::Stopping).await;

        // Cancel the replication task
        if let Some(task) = self.base.task_handle.write().await.take() {
            task.abort();
        }

        self.base.set_status(ComponentStatus::Stopped).await;
        self.base
            .send_component_event(
                ComponentStatus::Stopped,
                Some("PostgreSQL replication stopped".to_string()),
            )
            .await?;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    fn get_config(&self) -> &SourceConfig {
        &self.base.config
    }

    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse> {
        self.base
            .subscribe_with_bootstrap(
                query_id,
                enable_bootstrap,
                node_labels,
                relation_labels,
                "PostgreSQL",
            )
            .await
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

async fn run_replication(
    source_id: String,
    config: PostgresSourceConfig,
    dispatchers: Arc<
        RwLock<Vec<Box<dyn drasi_lib::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>,
    >,
    event_tx: ComponentEventSender,
    status: Arc<RwLock<ComponentStatus>>,
) -> Result<()> {
    info!("Starting replication for source {}", source_id);

    let mut stream =
        stream::ReplicationStream::new(config, source_id, dispatchers, event_tx, status);

    stream.run().await
}

/// Extension trait for creating PostgreSQL sources
///
/// This trait is implemented on `SourceConfig` to provide a fluent builder API
/// for configuring PostgreSQL replication sources.
///
/// # Example
///
/// ```no_run
/// use drasi_lib::config::SourceConfig;
/// use drasi_plugin_postgres::SourceConfigPostgresExt;
///
/// let config = SourceConfig::postgres()
///     .with_host("localhost")
///     .with_port(5432)
///     .with_database("mydb")
///     .with_user("postgres")
///     .with_password("password")
///     .build();
/// ```
pub trait SourceConfigPostgresExt {
    /// Create a new PostgreSQL source configuration builder
    fn postgres() -> PostgresSourceBuilder;
}

/// Builder for PostgreSQL source configuration
pub struct PostgresSourceBuilder {
    host: String,
    port: u16,
    database: String,
    user: String,
    password: String,
    tables: Vec<String>,
    slot_name: String,
    publication_name: String,
}

impl Default for PostgresSourceBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl PostgresSourceBuilder {
    /// Create a new PostgreSQL source builder with default values
    pub fn new() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 5432,
            database: String::new(),
            user: String::new(),
            password: String::new(),
            tables: Vec::new(),
            slot_name: "drasi_slot".to_string(),
            publication_name: "drasi_publication".to_string(),
        }
    }

    /// Set the PostgreSQL host
    pub fn with_host(mut self, host: impl Into<String>) -> Self {
        self.host = host.into();
        self
    }

    /// Set the PostgreSQL port
    pub fn with_port(mut self, port: u16) -> Self {
        self.port = port;
        self
    }

    /// Set the database name
    pub fn with_database(mut self, database: impl Into<String>) -> Self {
        self.database = database.into();
        self
    }

    /// Set the database user
    pub fn with_user(mut self, user: impl Into<String>) -> Self {
        self.user = user.into();
        self
    }

    /// Set the database password
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = password.into();
        self
    }

    /// Set the tables to replicate
    pub fn with_tables(mut self, tables: Vec<String>) -> Self {
        self.tables = tables;
        self
    }

    /// Add a table to replicate
    pub fn add_table(mut self, table: impl Into<String>) -> Self {
        self.tables.push(table.into());
        self
    }

    /// Set the replication slot name
    pub fn with_slot_name(mut self, slot_name: impl Into<String>) -> Self {
        self.slot_name = slot_name.into();
        self
    }

    /// Set the publication name
    pub fn with_publication_name(mut self, publication_name: impl Into<String>) -> Self {
        self.publication_name = publication_name.into();
        self
    }

    /// Build the PostgreSQL source configuration
    pub fn build(self) -> PostgresSourceConfig {
        PostgresSourceConfig {
            host: self.host,
            port: self.port,
            database: self.database,
            user: self.user,
            password: self.password,
            tables: self.tables,
            slot_name: self.slot_name,
            publication_name: self.publication_name,
            ssl_mode: Default::default(),
            table_keys: Vec::new(),
        }
    }
}

impl SourceConfigPostgresExt for SourceConfig {
    fn postgres() -> PostgresSourceBuilder {
        PostgresSourceBuilder::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_postgres_builder_defaults() {
        let config = PostgresSourceBuilder::new().build();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 5432);
        assert_eq!(config.slot_name, "drasi_slot");
        assert_eq!(config.publication_name, "drasi_publication");
    }

    #[test]
    fn test_postgres_builder_custom_values() {
        let config = PostgresSourceBuilder::new()
            .with_host("db.example.com")
            .with_port(5433)
            .with_database("production")
            .with_user("app_user")
            .with_password("secret")
            .with_tables(vec!["users".to_string(), "orders".to_string()])
            .build();

        assert_eq!(config.host, "db.example.com");
        assert_eq!(config.port, 5433);
        assert_eq!(config.database, "production");
        assert_eq!(config.user, "app_user");
        assert_eq!(config.password, "secret");
        assert_eq!(config.tables.len(), 2);
        assert_eq!(config.tables[0], "users");
        assert_eq!(config.tables[1], "orders");
    }

    #[test]
    fn test_postgres_builder_fluent_api() {
        let config = SourceConfig::postgres()
            .with_host("localhost")
            .with_database("test_db")
            .with_user("postgres")
            .build();

        assert_eq!(config.host, "localhost");
        assert_eq!(config.database, "test_db");
        assert_eq!(config.user, "postgres");
    }
}
