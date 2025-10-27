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

pub mod bootstrap;
pub mod connection;
pub mod decoder;
pub mod protocol;
pub mod scram;
pub mod stream;
pub mod types;

use anyhow::Result;
use async_trait::async_trait;
use log::{error, info};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::channels::*;
use crate::config::SourceConfig;
use crate::sources::{base::SourceBase, Source};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresReplicationConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub tables: Vec<String>,
    pub slot_name: String,
    #[serde(default = "default_publication_name")]
    pub publication_name: String,
    #[serde(default = "default_ssl_mode")]
    pub ssl_mode: String,
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,
}

fn default_publication_name() -> String {
    "drasi_publication".to_string()
}

fn default_ssl_mode() -> String {
    "prefer".to_string()
}

pub struct PostgresReplicationSource {
    base: SourceBase,
}

impl PostgresReplicationSource {
    pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Result<Self> {
        Ok(Self {
            base: SourceBase::new(config, event_tx)?,
        })
    }

    fn parse_config(&self) -> Result<PostgresReplicationConfig> {
        let props = &self.base.config.properties;

        Ok(PostgresReplicationConfig {
            host: props
                .get("host")
                .and_then(|v| v.as_str())
                .unwrap_or("localhost")
                .to_string(),
            port: props.get("port").and_then(|v| v.as_u64()).unwrap_or(5432) as u16,
            database: props
                .get("database")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'database' property"))?
                .to_string(),
            user: props
                .get("user")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow::anyhow!("Missing 'user' property"))?
                .to_string(),
            password: props
                .get("password")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            tables: props
                .get("tables")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str())
                        .map(|s| s.to_string())
                        .collect()
                })
                .unwrap_or_default(),
            slot_name: props
                .get("slot_name")
                .and_then(|v| v.as_str())
                .unwrap_or("drasi_slot")
                .to_string(),
            publication_name: props
                .get("publication_name")
                .and_then(|v| v.as_str())
                .unwrap_or("drasi_publication")
                .to_string(),
            ssl_mode: props
                .get("ssl_mode")
                .and_then(|v| v.as_str())
                .unwrap_or("prefer")
                .to_string(),
            table_keys: props
                .get("table_keys")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| {
                            let obj = v.as_object()?;
                            let table = obj.get("table")?.as_str()?.to_string();
                            let key_columns = obj
                                .get("key_columns")?
                                .as_array()?
                                .iter()
                                .filter_map(|k| k.as_str())
                                .map(|s| s.to_string())
                                .collect();
                            Some(TableKeyConfig { table, key_columns })
                        })
                        .collect()
                })
                .unwrap_or_default(),
        })
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
    config: PostgresReplicationConfig,
    dispatchers: Arc<RwLock<Vec<Box<dyn crate::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>>,
    event_tx: ComponentEventSender,
    status: Arc<RwLock<ComponentStatus>>,
) -> Result<()> {
    info!("Starting replication for source {}", source_id);

    let mut stream =
        stream::ReplicationStream::new(config, source_id, dispatchers, event_tx, status);

    stream.run().await
}
