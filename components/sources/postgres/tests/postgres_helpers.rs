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

//! PostgreSQL test helpers for replication integration tests.

use anyhow::{anyhow, Result};
use std::sync::Arc;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tokio_postgres::{Client, NoTls};

#[derive(Debug, Clone)]
pub struct ReplicationPostgresConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
}

impl ReplicationPostgresConfig {
    pub fn connection_string(&self) -> String {
        format!(
            "host={} port={} user={} password={} dbname={}",
            self.host, self.port, self.user, self.password, self.database
        )
    }
}

#[derive(Clone)]
pub struct ReplicationPostgresGuard {
    inner: Arc<ReplicationPostgresGuardInner>,
}

struct ReplicationPostgresGuardInner {
    container: std::sync::Mutex<Option<ContainerAsync<GenericImage>>>,
    config: ReplicationPostgresConfig,
}

impl ReplicationPostgresGuard {
    pub async fn new() -> Self {
        let (container, config) = setup_postgres_raw().await;
        Self {
            inner: Arc::new(ReplicationPostgresGuardInner {
                container: std::sync::Mutex::new(Some(container)),
                config,
            }),
        }
    }

    pub fn config(&self) -> &ReplicationPostgresConfig {
        &self.inner.config
    }

    pub async fn get_client(&self) -> Result<Client> {
        let mut last_error = None;
        for _ in 0..20 {
            match tokio_postgres::connect(&self.config().connection_string(), NoTls).await {
                Ok((client, connection)) => {
                    tokio::spawn(async move {
                        if let Err(e) = connection.await {
                            log::error!("PostgreSQL connection error: {e}");
                        }
                    });

                    return Ok(client);
                }
                Err(e) => {
                    last_error = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            }
        }

        Err(anyhow!(
            "Failed to connect to PostgreSQL after retries: {last_error:?}"
        ))
    }

    pub async fn cleanup(self) {
        let container_to_stop = {
            if let Ok(mut container_guard) = self.inner.container.lock() {
                container_guard.take()
            } else {
                None
            }
        };

        if let Some(container) = container_to_stop {
            let container_id = container.id().to_string();
            match container.stop().await {
                Ok(_) => log::debug!("Successfully stopped PostgreSQL container: {container_id}"),
                Err(e) => log::warn!("Error stopping container {container_id}: {e}"),
            }
            drop(container);
        }
    }
}

impl Drop for ReplicationPostgresGuardInner {
    fn drop(&mut self) {
        if let Ok(mut container_guard) = self.container.lock() {
            if let Some(container) = container_guard.take() {
                drop(container);
            }
        }
    }
}

pub async fn setup_replication_postgres() -> ReplicationPostgresGuard {
    ReplicationPostgresGuard::new().await
}

async fn setup_postgres_raw() -> (ContainerAsync<GenericImage>, ReplicationPostgresConfig) {
    use testcontainers::runners::AsyncRunner;

    let image = GenericImage::new("postgres", "16-alpine")
        .with_exposed_port(testcontainers::core::ContainerPort::Tcp(5432))
        .with_env_var("POSTGRES_PASSWORD", "postgres")
        .with_env_var("POSTGRES_USER", "postgres")
        .with_env_var("POSTGRES_DB", "postgres")
        .with_cmd([
            "-c",
            "wal_level=logical",
            "-c",
            "max_replication_slots=10",
            "-c",
            "max_wal_senders=10",
        ]);

    let container = image
        .start()
        .await
        .expect("Failed to start PostgreSQL container");
    let pg_port = container
        .get_host_port_ipv4(5432)
        .await
        .expect("Failed to resolve Postgres port");

    let config = ReplicationPostgresConfig {
        host: "localhost".to_string(), // DevSkim: ignore DS137138
        port: pg_port,
        database: "postgres".to_string(),
        user: "postgres".to_string(),
        password: "postgres".to_string(),
    };

    tokio::time::sleep(std::time::Duration::from_secs(1)).await;

    (container, config)
}

pub async fn execute_sql(client: &Client, sql: &str) -> Result<u64> {
    let result = client.execute(sql, &[]).await?;
    Ok(result)
}

pub async fn create_test_table(client: &Client, table_name: &str) -> Result<()> {
    let create_sql = format!(
        "CREATE TABLE IF NOT EXISTS {table_name} (\n    id INTEGER PRIMARY KEY,\n    name TEXT NOT NULL\n)"
    );
    execute_sql(client, &create_sql).await?;

    let replica_sql = format!("ALTER TABLE {table_name} REPLICA IDENTITY FULL");
    execute_sql(client, &replica_sql).await?;

    Ok(())
}

pub async fn create_publication(
    client: &Client,
    publication_name: &str,
    tables: &[String],
) -> Result<()> {
    let tables_list = tables.join(", ");
    let sql = format!(
        "CREATE PUBLICATION {publication_name} FOR TABLE {tables_list}"
    );
    execute_sql(client, &sql).await?;
    Ok(())
}

pub async fn insert_test_row(client: &Client, table: &str, id: i32, name: &str) -> Result<()> {
    let sql = format!("INSERT INTO {table} (id, name) VALUES ($1, $2)");
    client.execute(&sql, &[&id, &name]).await?;
    Ok(())
}

pub async fn update_test_row(client: &Client, table: &str, id: i32, name: &str) -> Result<()> {
    let sql = format!("UPDATE {table} SET name = $1 WHERE id = $2");
    client.execute(&sql, &[&name, &id]).await?;
    Ok(())
}

pub async fn delete_test_row(client: &Client, table: &str, id: i32) -> Result<()> {
    let sql = format!("DELETE FROM {table} WHERE id = $1");
    client.execute(&sql, &[&id]).await?;
    Ok(())
}

pub async fn grant_replication(client: &Client, user: &str) -> Result<()> {
    let sql = format!("ALTER ROLE {user} WITH REPLICATION");
    execute_sql(client, &sql).await?;
    Ok(())
}

pub async fn grant_table_access(client: &Client, table: &str, user: &str) -> Result<()> {
    let sql = format!("GRANT SELECT ON TABLE {table} TO {user}");
    execute_sql(client, &sql).await?;
    Ok(())
}

pub async fn create_logical_replication_slot(client: &Client, slot_name: &str) -> Result<()> {
    let sql = format!("SELECT pg_create_logical_replication_slot('{slot_name}', 'pgoutput')");
    let _ = client.query_one(&sql, &[]).await?;
    Ok(())
}
