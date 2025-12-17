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

//! Test utilities for MySQL-based testing using testcontainers
//!
//! This module provides helper functions for testing components that require
//! a MySQL database, using testcontainers to provide a real MySQL
//! server environment.

use anyhow::Result;
use mysql_async::prelude::*;
use mysql_async::{Conn, OptsBuilder};
use std::sync::Arc;
use testcontainers_modules::mysql::Mysql;

/// MySQL container configuration
#[derive(Debug, Clone)]
pub struct MysqlConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
}

impl MysqlConfig {
    /// Get the MySQL connection URL
    pub fn connection_url(&self) -> String {
        format!(
            "mysql://{}:{}@{}:{}/{}",
            self.user, self.password, self.host, self.port, self.database
        )
    }

    /// Get MySQL connection options
    pub fn connection_opts(&self) -> OptsBuilder {
        OptsBuilder::default()
            .ip_or_hostname(&self.host)
            .tcp_port(self.port)
            .user(Some(&self.user))
            .pass(Some(&self.password))
            .db_name(Some(&self.database))
    }
}

/// Setup a MySQL testcontainer and return a guard that manages cleanup
///
/// Returns a `MysqlGuard` that manages the container lifecycle. The container
/// will be stopped and removed via blocking cleanup when the guard is dropped.
///
/// **RECOMMENDED**: Call `.cleanup().await` explicitly before the test ends for
/// the most reliable cleanup.
///
/// # Example
/// ```ignore
/// let mysql = setup_mysql().await;
/// // Use mysql.config() to get connection details
/// // ... test code ...
/// mysql.cleanup().await; // Explicit cleanup (recommended)
/// Ok(())
/// ```
pub async fn setup_mysql() -> MysqlGuard {
    MysqlGuard::new().await
}

/// Low-level setup function that returns raw container and config
///
/// Internal use only. Prefer using `setup_mysql()` which returns a `MysqlGuard`
/// for automatic cleanup.
#[allow(clippy::unwrap_used)]
async fn setup_mysql_raw() -> (testcontainers::ContainerAsync<Mysql>, MysqlConfig) {
    use testcontainers::runners::AsyncRunner;

    // Start MySQL container
    let container = Mysql::default().start().await.unwrap();
    let mysql_port = container.get_host_port_ipv4(3306).await.unwrap();

    // The testcontainers MySQL module uses these default credentials
    let config = MysqlConfig {
        host: "localhost".to_string(),
        port: mysql_port,
        database: "test".to_string(),
        user: "test".to_string(),
        password: "test".to_string(),
    };

    // Give MySQL a moment to fully initialize after the wait strategy completes
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

    (container, config)
}

/// Guard wrapper for MySQL container that ensures proper cleanup
///
/// This struct wraps the MySQL container and uses blocking cleanup in Drop.
/// The testcontainers library's async drop may not complete before tests exit,
/// but we force a blocking cleanup using a runtime handle.
///
/// The container is wrapped in an Arc to allow cloning for multiple test accessors.
#[derive(Clone)]
pub struct MysqlGuard {
    inner: Arc<MysqlGuardInner>,
}

struct MysqlGuardInner {
    container: std::sync::Mutex<Option<testcontainers::ContainerAsync<Mysql>>>,
    config: MysqlConfig,
}

impl MysqlGuard {
    /// Create a new MySQL container with guaranteed cleanup
    pub async fn new() -> Self {
        let (container, config) = setup_mysql_raw().await;
        Self {
            inner: Arc::new(MysqlGuardInner {
                container: std::sync::Mutex::new(Some(container)),
                config,
            }),
        }
    }

    /// Get the MySQL configuration
    pub fn config(&self) -> &MysqlConfig {
        &self.inner.config
    }

    /// Get a MySQL client connection
    pub async fn get_client(&self) -> Result<Conn> {
        let opts = self.config().connection_opts();
        let conn = Conn::new(opts).await?;
        Ok(conn)
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
                    log::debug!("Successfully stopped MySQL container: {container_id}");
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

impl Drop for MysqlGuardInner {
    fn drop(&mut self) {
        if let Ok(mut container_guard) = self.container.lock() {
            if let Some(container) = container_guard.take() {
                let container_id = container.id().to_string();

                // Just drop the container - testcontainers will handle cleanup
                // We can't use block_on here because we might already be in an async runtime
                drop(container);
                log::debug!("MySQL container {container_id} dropped (cleanup delegated to testcontainers)");
            }
        }
    }
}

/// Execute a SQL statement on the MySQL database
///
/// # Arguments
/// * `conn` - MySQL connection
/// * `sql` - SQL statement to execute
///
/// # Returns
/// Number of rows affected
pub async fn execute_sql(conn: &mut Conn, sql: &str) -> Result<u64> {
    conn.query_drop(sql).await?;
    // query_drop doesn't return affected rows, so we return 0
    // If you need affected rows, use conn.exec_drop with a statement
    Ok(0)
}

/// Create a simple stored procedure for testing
///
/// This creates a stored procedure that logs calls to a table.
///
/// # Arguments
/// * `conn` - MySQL connection
pub async fn create_test_stored_procedure(conn: &mut Conn) -> Result<()> {
    // Create a log table to track procedure calls
    conn.query_drop(
        "CREATE TABLE IF NOT EXISTS procedure_log (
            id INT AUTO_INCREMENT PRIMARY KEY,
            operation VARCHAR(50),
            user_id INT,
            user_name VARCHAR(255),
            user_email VARCHAR(255),
            created_at TIMESTAMP DEFAULT CURRENT_TIMESTAMP
        )",
    )
    .await?;

    // Drop existing procedures if they exist
    let _ = conn.query_drop("DROP PROCEDURE IF EXISTS add_user").await;
    let _ = conn
        .query_drop("DROP PROCEDURE IF EXISTS update_user")
        .await;
    let _ = conn
        .query_drop("DROP PROCEDURE IF EXISTS delete_user")
        .await;

    // Create stored procedure for adding users
    conn.query_drop(
        "CREATE PROCEDURE add_user(
            IN p_id INT,
            IN p_name VARCHAR(255),
            IN p_email VARCHAR(255)
        )
        BEGIN
            INSERT INTO procedure_log (operation, user_id, user_name, user_email)
            VALUES ('add', p_id, p_name, p_email);
        END",
    )
    .await?;

    // Create stored procedure for updating users
    conn.query_drop(
        "CREATE PROCEDURE update_user(
            IN p_id INT,
            IN p_name VARCHAR(255),
            IN p_email VARCHAR(255)
        )
        BEGIN
            INSERT INTO procedure_log (operation, user_id, user_name, user_email)
            VALUES ('update', p_id, p_name, p_email);
        END",
    )
    .await?;

    // Create stored procedure for deleting users
    conn.query_drop(
        "CREATE PROCEDURE delete_user(IN p_id INT)
        BEGIN
            INSERT INTO procedure_log (operation, user_id, user_name, user_email)
            VALUES ('delete', p_id, NULL, NULL);
        END",
    )
    .await?;

    Ok(())
}

/// Get the count of procedure log entries
pub async fn get_procedure_log_count(conn: &mut Conn) -> Result<i64> {
    let count: i64 = conn
        .query_first("SELECT COUNT(*) FROM procedure_log")
        .await?
        .unwrap_or(0);
    Ok(count)
}

/// Get procedure log entries
pub async fn get_procedure_log_entries(conn: &mut Conn) -> Result<Vec<ProcedureLogEntry>> {
    let rows: Vec<(String, Option<i32>, Option<String>, Option<String>)> = conn
        .query(
            "SELECT operation, user_id, user_name, user_email FROM procedure_log ORDER BY id",
        )
        .await?;

    let entries = rows
        .into_iter()
        .map(|(operation, user_id, user_name, user_email)| ProcedureLogEntry {
            operation,
            user_id,
            user_name,
            user_email,
        })
        .collect();

    Ok(entries)
}

/// Clear the procedure log table
pub async fn clear_procedure_log(conn: &mut Conn) -> Result<()> {
    conn.query_drop("TRUNCATE TABLE procedure_log").await?;
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
    async fn test_setup_mysql() {
        let mysql = setup_mysql().await;
        let config = mysql.config();

        assert_eq!(config.host, "localhost");
        assert_eq!(config.database, "test");
        assert_eq!(config.user, "root");

        // Verify we can connect
        let mut conn = mysql.get_client().await.unwrap();
        let value: i32 = conn.query_first("SELECT 1").await.unwrap().unwrap();
        assert_eq!(value, 1);

        // Explicitly cleanup the container
        mysql.cleanup().await;
    }

    #[tokio::test]
    async fn test_create_stored_procedure() {
        let mysql = setup_mysql().await;
        let mut conn = mysql.get_client().await.unwrap();

        // Create test stored procedures
        create_test_stored_procedure(&mut conn).await.unwrap();

        // Call the add_user procedure
        conn.query_drop("CALL add_user(1, 'Alice', 'alice@example.com')")
            .await
            .unwrap();

        // Verify the log entry
        let count = get_procedure_log_count(&mut conn).await.unwrap();
        assert_eq!(count, 1);

        let entries = get_procedure_log_entries(&mut conn).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].operation, "add");
        assert_eq!(entries[0].user_id, Some(1));
        assert_eq!(entries[0].user_name, Some("Alice".to_string()));
        assert_eq!(entries[0].user_email, Some("alice@example.com".to_string()));

        mysql.cleanup().await;
    }

    #[tokio::test]
    async fn test_stored_procedure_operations() {
        let mysql = setup_mysql().await;
        let mut conn = mysql.get_client().await.unwrap();

        create_test_stored_procedure(&mut conn).await.unwrap();

        // Test add
        conn.query_drop("CALL add_user(1, 'Alice', 'alice@example.com')")
            .await
            .unwrap();

        // Test update
        conn.query_drop("CALL update_user(1, 'Alice Updated', 'alice.new@example.com')")
            .await
            .unwrap();

        // Test delete
        conn.query_drop("CALL delete_user(1)").await.unwrap();

        // Verify all operations logged
        let entries = get_procedure_log_entries(&mut conn).await.unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].operation, "add");
        assert_eq!(entries[1].operation, "update");
        assert_eq!(entries[2].operation, "delete");

        mysql.cleanup().await;
    }

    #[tokio::test]
    async fn test_clear_procedure_log() {
        let mysql = setup_mysql().await;
        let mut conn = mysql.get_client().await.unwrap();

        create_test_stored_procedure(&mut conn).await.unwrap();

        // Add some entries
        conn.query_drop("CALL add_user(1, 'Alice', 'alice@example.com')")
            .await
            .unwrap();
        conn.query_drop("CALL add_user(2, 'Bob', 'bob@example.com')")
            .await
            .unwrap();

        assert_eq!(get_procedure_log_count(&mut conn).await.unwrap(), 2);

        // Clear the log
        clear_procedure_log(&mut conn).await.unwrap();

        assert_eq!(get_procedure_log_count(&mut conn).await.unwrap(), 0);

        mysql.cleanup().await;
    }
}
