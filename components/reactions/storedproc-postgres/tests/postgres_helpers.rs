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

//! Test utilities for PostgreSQL-based testing using testcontainers
//!
//! This module provides helper functions for testing components that require
//! a PostgreSQL database, using testcontainers to provide a real PostgreSQL
//! server environment.

use anyhow::Result;
use std::sync::Arc;
use testcontainers_modules::postgres::Postgres;
use tokio_postgres::{Client, NoTls};

/// PostgreSQL container configuration
#[derive(Debug, Clone)]
pub struct PostgresConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
}

impl PostgresConfig {
    /// Get the PostgreSQL connection string
    pub fn connection_string(&self) -> String {
        format!(
            "host={} port={} user={} password={} dbname={}",
            self.host, self.port, self.user, self.password, self.database
        )
    }
}

/// Setup a PostgreSQL testcontainer and return a guard that manages cleanup
///
/// Returns a `PostgresGuard` that manages the container lifecycle. The container
/// will be stopped and removed via blocking cleanup when the guard is dropped.
///
/// **RECOMMENDED**: Call `.cleanup().await` explicitly before the test ends for
/// the most reliable cleanup.
///
/// # Example
/// ```ignore
/// let pg = setup_postgres().await;
/// // Use pg.config() to get connection details
/// // ... test code ...
/// pg.cleanup().await; // Explicit cleanup (recommended)
/// Ok(())
/// ```
pub async fn setup_postgres() -> PostgresGuard {
    PostgresGuard::new().await
}

/// Low-level setup function that returns raw container and config
///
/// Internal use only. Prefer using `setup_postgres()` which returns a `PostgresGuard`
/// for automatic cleanup.
#[allow(clippy::unwrap_used)]
async fn setup_postgres_raw() -> (testcontainers::ContainerAsync<Postgres>, PostgresConfig) {
    use testcontainers::runners::AsyncRunner;

    // Start PostgreSQL container
    let container = Postgres::default().start().await.unwrap();
    let pg_port = container.get_host_port_ipv4(5432).await.unwrap();

    let config = PostgresConfig {
        host: "localhost".to_string(), // DevSkim: ignore DS137138
        port: pg_port,
        database: "postgres".to_string(),
        user: "postgres".to_string(),
        password: "postgres".to_string(),
    };

    // Give PostgreSQL a moment to fully initialize after the wait strategy completes
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    (container, config)
}

/// Guard wrapper for PostgreSQL container that ensures proper cleanup
///
/// This struct wraps the PostgreSQL container and uses blocking cleanup in Drop.
/// The testcontainers library's async drop may not complete before tests exit,
/// but we force a blocking cleanup using a runtime handle.
///
/// The container is wrapped in an Arc to allow cloning for multiple test accessors.
#[derive(Clone)]
pub struct PostgresGuard {
    inner: Arc<PostgresGuardInner>,
}

struct PostgresGuardInner {
    container: std::sync::Mutex<Option<testcontainers::ContainerAsync<Postgres>>>,
    config: PostgresConfig,
}

impl PostgresGuard {
    /// Create a new PostgreSQL container with guaranteed cleanup
    pub async fn new() -> Self {
        let (container, config) = setup_postgres_raw().await;
        Self {
            inner: Arc::new(PostgresGuardInner {
                container: std::sync::Mutex::new(Some(container)),
                config,
            }),
        }
    }

    /// Get the PostgreSQL configuration
    pub fn config(&self) -> &PostgresConfig {
        &self.inner.config
    }

    /// Get a PostgreSQL client connection
    pub async fn get_client(&self) -> Result<Client> {
        let (client, connection) =
            tokio_postgres::connect(&self.config().connection_string(), NoTls).await?;

        // Spawn connection handler
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                log::error!("PostgreSQL connection error: {e}");
            }
        });

        Ok(client)
    }

    /// Explicitly stop and remove the container
    ///
    /// Call this at the end of your test to ensure the container is cleaned up.
    /// This is an async method that properly stops and removes the container.
    pub async fn cleanup(self) {
        // Take the container out while holding the lock, then drop the lock before awaiting
        let container_to_stop = {
            if let Ok(mut container_guard) = self.inner.container.lock() {
                container_guard.take()
            } else {
                None
            }
        };

        // Now await without holding the lock
        if let Some(container) = container_to_stop {
            let container_id = container.id().to_string();

            // Stop and remove the container
            match container.stop().await {
                Ok(_) => {
                    log::debug!("Successfully stopped PostgreSQL container: {container_id}");
                }
                Err(e) => {
                    log::warn!("Error stopping container {container_id}: {e}");
                }
            }

            // Explicit drop to trigger removal
            drop(container);
        }
    }
}

impl Drop for PostgresGuardInner {
    fn drop(&mut self) {
        if let Ok(mut container_guard) = self.container.lock() {
            if let Some(container) = container_guard.take() {
                let container_id = container.id().to_string();

                // Block on cleanup to ensure it completes
                let cleanup_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    // Try to get current runtime, if we're in one
                    if let Ok(handle) = tokio::runtime::Handle::try_current() {
                        // Spawn blocking task to stop container
                        handle.block_on(async move {
                            let _ = container.stop().await;
                            drop(container);
                        });
                    } else {
                        // We're not in a runtime, just drop it
                        drop(container);
                    }
                }));

                if cleanup_result.is_ok() {
                    log::debug!("PostgreSQL container {container_id} cleaned up in Drop");
                } else {
                    log::warn!("Failed to cleanup PostgreSQL container {container_id} in Drop");
                }
            }
        }
    }
}

/// Execute a SQL statement on the PostgreSQL database
///
/// # Arguments
/// * `client` - PostgreSQL client connection
/// * `sql` - SQL statement to execute
///
/// # Returns
/// Number of rows affected
pub async fn execute_sql(client: &Client, sql: &str) -> Result<u64> {
    let result = client.execute(sql, &[]).await?;
    Ok(result)
}

/// Create a simple stored procedure for testing
///
/// This creates a stored procedure that logs calls to a table.
///
/// # Arguments
/// * `client` - PostgreSQL client connection
pub async fn create_test_stored_procedure(client: &Client) -> Result<()> {
    // Create a log table to track procedure calls
    execute_sql(
        client,
        "CREATE TABLE IF NOT EXISTS procedure_log (
            id SERIAL PRIMARY KEY,
            operation VARCHAR(50),
            user_id INTEGER,
            user_name VARCHAR(255),
            user_email VARCHAR(255),
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .await?;

    // Create stored procedure for adding users
    execute_sql(
        client,
        "CREATE OR REPLACE PROCEDURE add_user(p_id INTEGER, p_name VARCHAR, p_email VARCHAR)
        LANGUAGE plpgsql
        AS $$
        BEGIN
            INSERT INTO procedure_log (operation, user_id, user_name, user_email)
            VALUES ('add', p_id, p_name, p_email);
        END;
        $$",
    )
    .await?;

    // Create stored procedure for updating users
    execute_sql(
        client,
        "CREATE OR REPLACE PROCEDURE update_user(p_id INTEGER, p_name VARCHAR, p_email VARCHAR)
        LANGUAGE plpgsql
        AS $$
        BEGIN
            INSERT INTO procedure_log (operation, user_id, user_name, user_email)
            VALUES ('update', p_id, p_name, p_email);
        END;
        $$",
    )
    .await?;

    // Create stored procedure for deleting users
    execute_sql(
        client,
        "CREATE OR REPLACE PROCEDURE delete_user(p_id INTEGER)
        LANGUAGE plpgsql
        AS $$
        BEGIN
            INSERT INTO procedure_log (operation, user_id, user_name, user_email)
            VALUES ('delete', p_id, NULL, NULL);
        END;
        $$",
    )
    .await?;

    Ok(())
}

/// Get the count of procedure log entries
pub async fn get_procedure_log_count(client: &Client) -> Result<i64> {
    let row = client
        .query_one("SELECT COUNT(*) FROM procedure_log", &[])
        .await?;
    let count: i64 = row.get(0);
    Ok(count)
}

/// Get procedure log entries
pub async fn get_procedure_log_entries(client: &Client) -> Result<Vec<ProcedureLogEntry>> {
    let rows = client
        .query(
            "SELECT operation, user_id, user_name, user_email FROM procedure_log ORDER BY id",
            &[],
        )
        .await?;

    let mut entries = Vec::new();
    for row in rows {
        entries.push(ProcedureLogEntry {
            operation: row.get(0),
            user_id: row.get(1),
            user_name: row.get(2),
            user_email: row.get(3),
        });
    }

    Ok(entries)
}

/// Clear the procedure log table
pub async fn clear_procedure_log(client: &Client) -> Result<()> {
    execute_sql(client, "TRUNCATE TABLE procedure_log").await?;
    Ok(())
}

/// Procedure log entry structure
#[derive(Debug, Clone, PartialEq)]
pub struct ProcedureLogEntry {
    pub operation: String,
    pub user_id: Option<i32>,
    pub user_name: Option<String>,
    pub user_email: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_setup_postgres() {
        let pg = setup_postgres().await;
        let config = pg.config();

        assert_eq!(config.host, "localhost"); // DevSkim: ignore DS137138
        assert_eq!(config.database, "postgres");
        assert_eq!(config.user, "postgres");

        // Verify we can connect
        let client = pg.get_client().await.unwrap();
        let row = client.query_one("SELECT 1", &[]).await.unwrap();
        let value: i32 = row.get(0);
        assert_eq!(value, 1);

        // Explicitly cleanup the container
        pg.cleanup().await;
    }

    #[tokio::test]
    async fn test_create_stored_procedure() {
        let pg = setup_postgres().await;
        let client = pg.get_client().await.unwrap();

        // Create test stored procedures
        create_test_stored_procedure(&client).await.unwrap();

        // Call the add_user procedure
        client
            .execute("CALL add_user(1, 'Alice', 'alice@example.com')", &[])
            .await
            .unwrap();

        // Verify the log entry
        let count = get_procedure_log_count(&client).await.unwrap();
        assert_eq!(count, 1);

        let entries = get_procedure_log_entries(&client).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].operation, "add");
        assert_eq!(entries[0].user_id, Some(1));
        assert_eq!(entries[0].user_name, Some("Alice".to_string()));
        assert_eq!(entries[0].user_email, Some("alice@example.com".to_string()));

        pg.cleanup().await;
    }

    #[tokio::test]
    async fn test_stored_procedure_operations() {
        let pg = setup_postgres().await;
        let client = pg.get_client().await.unwrap();

        create_test_stored_procedure(&client).await.unwrap();

        // Test add
        client
            .execute("CALL add_user(1, 'Alice', 'alice@example.com')", &[])
            .await
            .unwrap();

        // Test update
        client
            .execute(
                "CALL update_user(1, 'Alice Updated', 'alice.new@example.com')",
                &[],
            )
            .await
            .unwrap();

        // Test delete
        client.execute("CALL delete_user(1)", &[]).await.unwrap();

        // Verify all operations logged
        let entries = get_procedure_log_entries(&client).await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].operation, "add");
        assert_eq!(entries[1].operation, "update");
        assert_eq!(entries[2].operation, "delete");

        pg.cleanup().await;
    }

    #[tokio::test]
    async fn test_clear_procedure_log() {
        let pg = setup_postgres().await;
        let client = pg.get_client().await.unwrap();

        create_test_stored_procedure(&client).await.unwrap();

        // Add some entries
        client
            .execute("CALL add_user(1, 'Alice', 'alice@example.com')", &[])
            .await
            .unwrap();
        client
            .execute("CALL add_user(2, 'Bob', 'bob@example.com')", &[])
            .await
            .unwrap();

        assert_eq!(get_procedure_log_count(&client).await.unwrap(), 2);

        // Clear the log
        clear_procedure_log(&client).await.unwrap();

        assert_eq!(get_procedure_log_count(&client).await.unwrap(), 0);

        pg.cleanup().await;
    }
}
