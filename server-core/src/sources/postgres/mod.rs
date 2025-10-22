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

use crate::bootstrap::{BootstrapContext, BootstrapProviderFactory, BootstrapRequest};
use crate::channels::*;
use crate::config::SourceConfig;
use crate::sources::Source;

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
    config: SourceConfig,
    status: Arc<RwLock<ComponentStatus>>,
    broadcast_tx: SourceBroadcastSender,
    event_tx: ComponentEventSender,
    task_handle: Arc<RwLock<Option<tokio::task::JoinHandle<()>>>>,
}

impl PostgresReplicationSource {
    pub fn new(config: SourceConfig, event_tx: ComponentEventSender) -> Self {
        let (broadcast_tx, _) = tokio::sync::broadcast::channel(1000);

        Self {
            config,
            status: Arc::new(RwLock::new(ComponentStatus::Stopped)),
            broadcast_tx,
            event_tx,
            task_handle: Arc::new(RwLock::new(None)),
        }
    }

    fn parse_config(&self) -> Result<PostgresReplicationConfig> {
        let props = &self.config.properties;

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
        let mut status = self.status.write().await;
        if *status == ComponentStatus::Running {
            return Ok(());
        }

        *status = ComponentStatus::Starting;
        info!("Starting PostgreSQL replication source: {}", self.config.id);

        let config = self.parse_config()?;
        let source_id = self.config.id.clone();
        let broadcast_tx = self.broadcast_tx.clone();
        let event_tx = self.event_tx.clone();
        let status_clone = self.status.clone();

        let task = tokio::spawn(async move {
            if let Err(e) = run_replication(
                source_id.clone(),
                config,
                broadcast_tx,
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

        *self.task_handle.write().await = Some(task);
        *status = ComponentStatus::Running;

        let _ = self
            .event_tx
            .send(ComponentEvent {
                component_id: self.config.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Running,
                timestamp: chrono::Utc::now(),
                message: Some("PostgreSQL replication started".to_string()),
            })
            .await;

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        let mut status = self.status.write().await;
        if *status != ComponentStatus::Running {
            return Ok(());
        }

        *status = ComponentStatus::Stopping;
        info!("Stopping PostgreSQL replication source: {}", self.config.id);

        // Cancel the replication task
        if let Some(task) = self.task_handle.write().await.take() {
            task.abort();
        }

        *status = ComponentStatus::Stopped;

        let _ = self
            .event_tx
            .send(ComponentEvent {
                component_id: self.config.id.clone(),
                component_type: ComponentType::Source,
                status: ComponentStatus::Stopped,
                timestamp: chrono::Utc::now(),
                message: Some("PostgreSQL replication stopped".to_string()),
            })
            .await;

        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.status.read().await.clone()
    }

    fn get_config(&self) -> &SourceConfig {
        &self.config
    }

    async fn subscribe(
        &self,
        query_id: String,
        enable_bootstrap: bool,
        node_labels: Vec<String>,
        relation_labels: Vec<String>,
    ) -> Result<SubscriptionResponse> {
        info!(
            "Query '{}' subscribing to PostgreSQL source '{}' (bootstrap: {})",
            query_id, self.config.id, enable_bootstrap
        );

        let broadcast_receiver = self.broadcast_tx.subscribe();

        // Clone query_id for later use since it will be moved into async block
        let query_id_for_response = query_id.clone();

        // Handle bootstrap if requested and bootstrap provider is configured
        let bootstrap_receiver = if enable_bootstrap {
            if let Some(provider_config) = &self.config.bootstrap_provider {
                info!(
                    "Bootstrap enabled for query '{}' with {} node labels and {} relation labels, delegating to bootstrap provider",
                    query_id,
                    node_labels.len(),
                    relation_labels.len()
                );

                let (tx, rx) = tokio::sync::mpsc::channel(1000);

                // Create bootstrap provider
                let provider = BootstrapProviderFactory::create_provider(provider_config)?;

                // Create bootstrap context
                let context = BootstrapContext::new(
                    self.config.id.clone(), // server_id (using source_id as placeholder)
                    Arc::new(self.config.clone()),
                    self.config.id.clone(),
                );

                // Create bootstrap request
                let request = BootstrapRequest {
                    query_id: query_id.clone(),
                    node_labels,
                    relation_labels,
                    request_id: format!("{}-{}", query_id, uuid::Uuid::new_v4()),
                };

                // Spawn bootstrap task
                tokio::spawn(async move {
                    match provider.bootstrap(request, &context, tx).await {
                        Ok(count) => {
                            info!(
                                "Bootstrap completed successfully for query '{}', sent {} events",
                                query_id, count
                            );
                        }
                        Err(e) => {
                            error!("Bootstrap failed for query '{}': {}", query_id, e);
                        }
                    }
                });

                Some(rx)
            } else {
                info!(
                    "Bootstrap requested for query '{}' but no bootstrap provider configured for source '{}'",
                    query_id, self.config.id
                );
                None
            }
        } else {
            None
        };

        Ok(SubscriptionResponse {
            query_id: query_id_for_response,
            source_id: self.config.id.clone(),
            broadcast_receiver,
            bootstrap_receiver,
        })
    }

    fn get_broadcast_receiver(&self) -> Result<SourceBroadcastReceiver> {
        Ok(self.broadcast_tx.subscribe())
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

async fn run_replication(
    source_id: String,
    config: PostgresReplicationConfig,
    broadcast_tx: SourceBroadcastSender,
    event_tx: ComponentEventSender,
    status: Arc<RwLock<ComponentStatus>>,
) -> Result<()> {
    info!("Starting replication for source {}", source_id);

    let mut stream =
        stream::ReplicationStream::new(config, source_id, broadcast_tx, event_tx, status);

    stream.run().await
}
