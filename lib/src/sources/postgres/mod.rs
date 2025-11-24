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

use crate::channels::*;
use crate::config::SourceConfig;
use crate::sources::{base::SourceBase, Source};

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
            crate::config::SourceSpecificConfig::Postgres(postgres_config) => {
                Ok(postgres_config.clone())
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
        RwLock<Vec<Box<dyn crate::channels::ChangeDispatcher<SourceEventWrapper> + Send + Sync>>>,
    >,
    event_tx: ComponentEventSender,
    status: Arc<RwLock<ComponentStatus>>,
) -> Result<()> {
    info!("Starting replication for source {}", source_id);

    let mut stream =
        stream::ReplicationStream::new(config, source_id, dispatchers, event_tx, status);

    stream.run().await
}
