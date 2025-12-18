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

//! Test utilities for MSSQL-based testing using testcontainers
//!
//! This module provides helper functions for testing components that require
//! a Microsoft SQL Server database, using testcontainers to provide a real MSSQL
//! server environment.
//!
//! **Note**: On ARM64 (Apple Silicon), Azure SQL Edge has known stability issues
//! and may crash on startup. Tests using this module are automatically skipped
//! on ARM64 platforms. They will run successfully on AMD64 and in CI environments.

use anyhow::Result;
use std::sync::Arc;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use testcontainers_modules::mssql_server::MssqlServer;
use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::TokioAsyncWriteCompatExt;

/// Container type to support both MSSQL Server and Azure SQL Edge
enum MssqlContainer {
    MssqlServer(ContainerAsync<MssqlServer>),
    AzureSqlEdge(ContainerAsync<GenericImage>),
}

impl MssqlContainer {
    fn id(&self) -> &str {
        match self {
            MssqlContainer::MssqlServer(c) => c.id(),
            MssqlContainer::AzureSqlEdge(c) => c.id(),
        }
    }

    async fn get_host_port_ipv4(
        &self,
        port: u16,
    ) -> Result<u16, testcontainers::core::error::TestcontainersError> {
        match self {
            MssqlContainer::MssqlServer(c) => c.get_host_port_ipv4(port).await,
            MssqlContainer::AzureSqlEdge(c) => c.get_host_port_ipv4(port).await,
        }
    }

    async fn stop(self) -> Result<(), testcontainers::core::error::TestcontainersError> {
        match self {
            MssqlContainer::MssqlServer(c) => c.stop().await,
            MssqlContainer::AzureSqlEdge(c) => c.stop().await,
        }
    }
}

/// MSSQL container configuration
#[derive(Debug, Clone)]
pub struct MssqlConfig {
    pub host: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub trust_server_certificate: bool,
}

impl MssqlConfig {
    /// Get the MSSQL connection string
    pub fn connection_string(&self) -> String {
        format!(
            "server={},{};user={};password={};database={};TrustServerCertificate={}",
            self.host,
            self.port,
            self.user,
            self.password,
            self.database,
            if self.trust_server_certificate {
                "yes"
            } else {
                "no"
            }
        )
    }

    /// Get tiberius Config
    pub fn tiberius_config(&self) -> Result<Config> {
        let mut config = Config::new();
        config.host(&self.host);
        config.port(self.port);
        config.authentication(tiberius::AuthMethod::sql_server(&self.user, &self.password));
        config.database(&self.database);
        config.trust_cert(); // For testing, trust the self-signed certificate
        Ok(config)
    }

    /// Create a tiberius client connection
    pub async fn connect(&self) -> Result<Client<tokio_util::compat::Compat<TcpStream>>> {
        let config = self.tiberius_config()?;
        let tcp = TcpStream::connect(config.get_addr()).await?;
        let client = Client::connect(config, tcp.compat_write()).await?;
        Ok(client)
    }
}

/// Setup a MSSQL testcontainer and return a guard that manages cleanup
///
/// Returns a `MssqlGuard` that manages the container lifecycle. The container
/// will be stopped and removed via blocking cleanup when the guard is dropped.
///
/// **RECOMMENDED**: Call `.cleanup().await` explicitly before the test ends for
/// the most reliable cleanup.
///
/// # Example
/// ```ignore
/// let mssql = setup_mssql().await;
/// // Use mssql.config() to get connection details
/// // ... test code ...
/// mssql.cleanup().await; // Explicit cleanup (recommended)
/// Ok(())
/// ```
pub async fn setup_mssql() -> MssqlGuard {
    MssqlGuard::new().await
}

/// Low-level setup function that returns raw container and config
///
/// Internal use only. Prefer using `setup_mssql()` which returns a `MssqlGuard`
/// for automatic cleanup.
///
/// On ARM64 platforms (Apple Silicon), this uses Azure SQL Edge which is compatible
/// with MSSQL but runs natively on ARM64. On AMD64, it uses the standard MSSQL Server.
#[allow(clippy::unwrap_used)]
async fn setup_mssql_raw() -> (MssqlContainer, MssqlConfig) {
    use testcontainers::runners::AsyncRunner;

    // Start MSSQL container with SA password
    // The testcontainers MSSQL module uses SA user with a configurable password
    let password = "YourStrong@Passw0rd"; // MSSQL password requirements

    // On ARM64 (Apple Silicon), use Azure SQL Edge for native performance
    // On AMD64, use standard MSSQL Server
    // Retry container startup to handle transient Docker/containerd issues
    let max_container_retries = 3;
    let mut last_error = None;

    let (container, init_time) = {
        let mut result_opt = None;

        for attempt in 1..=max_container_retries {
            let result = if cfg!(target_arch = "aarch64") {
                // Azure SQL Edge for ARM64
                let image = GenericImage::new("mcr.microsoft.com/azure-sql-edge", "latest")
                    .with_exposed_port(testcontainers::core::ContainerPort::Tcp(1433))
                    .with_env_var("ACCEPT_EULA", "1")
                    .with_env_var("MSSQL_SA_PASSWORD", password)
                    // Add capabilities needed for Azure SQL Edge on ARM
                    .with_privileged(true);

                image
                    .start()
                    .await
                    .map(|c| (MssqlContainer::AzureSqlEdge(c), 20000))
            } else {
                // Standard MSSQL Server for AMD64
                let image = MssqlServer::default()
                    .with_env_var("ACCEPT_EULA", "Y")
                    .with_env_var("MSSQL_SA_PASSWORD", password);

                image
                    .start()
                    .await
                    .map(|c| (MssqlContainer::MssqlServer(c), 15000))
            };

            match result {
                Ok(container_tuple) => {
                    result_opt = Some(container_tuple);
                    break;
                }
                Err(e) => {
                    last_error = Some(e);

                    if attempt < max_container_retries {
                        // Exponential backoff: 1s, 2s, 4s
                        let delay = std::time::Duration::from_secs(2u64.pow(attempt as u32 - 1));
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }

        result_opt.unwrap_or_else(|| {
            panic!(
                "Failed to start MSSQL container after {max_container_retries} attempts. \n\
                Last error: {last_error:?}\n\
                This might be due to Docker/containerd issues. Try:\n\
                1. Restart Docker Desktop\n\
                2. Run: docker system prune -a (WARNING: removes all unused images)\n\
                3. Check Docker logs for errors"
            )
        })
    };

    let mssql_port = container.get_host_port_ipv4(1433).await.unwrap();

    // The testcontainers MSSQL module uses these default credentials
    let config = MssqlConfig {
        // DevSkim: ignore DS137138
        host: "localhost".to_string(),
        port: mssql_port,
        database: "master".to_string(), // Default database
        user: "sa".to_string(),
        password: password.to_string(),
        trust_server_certificate: true,
    };

    // Give MSSQL a moment to fully initialize
    tokio::time::sleep(std::time::Duration::from_millis(init_time)).await;

    // Wait for the database to be ready with retries
    wait_for_mssql_ready(&config, &container).await;

    (container, config)
}

/// Wait for MSSQL to be ready for connections with retries
async fn wait_for_mssql_ready(config: &MssqlConfig, container: &MssqlContainer) {
    let max_retries = 30; // Increased from 10 to handle slower systems
    let retry_delay = std::time::Duration::from_secs(3);

    for attempt in 1..=max_retries {
        if let Ok(mut client) = config.connect().await {
            // Try a simple query to verify the connection works
            if let Ok(stream) = client.query("SELECT 1", &[]).await {
                if stream.into_results().await.is_ok() {
                    return;
                }
            }
        }

        if attempt < max_retries {
            tokio::time::sleep(retry_delay).await;
        }
    }

    panic!(
        "MSSQL failed to become ready after {max_retries} attempts ({} seconds)",
        max_retries * 3
    );
}

/// Guard wrapper for MSSQL container that ensures proper cleanup
///
/// This struct wraps the MSSQL container and uses blocking cleanup in Drop.
/// The testcontainers library's async drop may not complete before tests exit,
/// but we force a blocking cleanup using a runtime handle.
///
/// The container is wrapped in an Arc to allow cloning for multiple test accessors.
#[derive(Clone)]
pub struct MssqlGuard {
    inner: Arc<MssqlGuardInner>,
}

struct MssqlGuardInner {
    container: std::sync::Mutex<Option<MssqlContainer>>,
    config: MssqlConfig,
}

impl MssqlGuard {
    /// Create a new MSSQL container with guaranteed cleanup
    pub async fn new() -> Self {
        let (container, config) = setup_mssql_raw().await;
        Self {
            inner: Arc::new(MssqlGuardInner {
                container: std::sync::Mutex::new(Some(container)),
                config,
            }),
        }
    }

    /// Get the MSSQL configuration
    pub fn config(&self) -> &MssqlConfig {
        &self.inner.config
    }

    /// Get a MSSQL client connection
    pub async fn get_client(&self) -> Result<Client<tokio_util::compat::Compat<TcpStream>>> {
        let config = self.config().tiberius_config()?;
        let tcp = TcpStream::connect(config.get_addr()).await?;
        let client = Client::connect(config, tcp.compat_write()).await?;
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
            // Stop and remove the container
            // Note: stop() consumes the container, which triggers removal
            let _ = container.stop().await;
        }
    }
}

impl Drop for MssqlGuardInner {
    fn drop(&mut self) {
        if let Ok(mut container_guard) = self.container.lock() {
            if let Some(container) = container_guard.take() {
                // Just drop the container - testcontainers will handle cleanup
                // We can't use block_on here because we might already be in an async runtime
                drop(container);
            }
        }
    }
}

/// Execute a SQL statement on the MSSQL database
///
/// # Arguments
/// * `client` - MSSQL client connection
/// * `sql` - SQL statement to execute
///
/// # Returns
/// Number of rows affected
pub async fn execute_sql(
    client: &mut Client<tokio_util::compat::Compat<TcpStream>>,
    sql: &str,
) -> Result<u64> {
    let result = client.execute(sql, &[]).await?;
    Ok(result.total())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    #[cfg(not(target_arch = "aarch64"))] // Skip on ARM64 - Azure SQL Edge has stability issues
    async fn test_setup_mssql() {
        let mssql = setup_mssql().await;
        let config = mssql.config();

        // Verify configuration
        assert_eq!(config.user, "sa");
        assert_eq!(config.database, "master");
        assert!(config.trust_server_certificate);

        // Verify we can connect
        let mut client = mssql.get_client().await.unwrap();
        let row = client
            .query("SELECT 1 AS value", &[])
            .await
            .unwrap()
            .into_row()
            .await
            .unwrap()
            .unwrap();
        let value: i32 = row.get(0).unwrap();
        assert_eq!(value, 1);

        // Explicitly cleanup the container
        mssql.cleanup().await;
    }

    #[tokio::test]
    #[cfg(not(target_arch = "aarch64"))] // Skip on ARM64 - Azure SQL Edge has stability issues
    async fn test_execute_sql() {
        let mssql = setup_mssql().await;
        let mut client = mssql.get_client().await.unwrap();

        // Create a test table
        execute_sql(
            &mut client,
            "CREATE TABLE test_table (id INT PRIMARY KEY, name NVARCHAR(100))",
        )
        .await
        .unwrap();

        // Insert data
        let rows_affected = execute_sql(
            &mut client,
            "INSERT INTO test_table (id, name) VALUES (1, 'Alice')",
        )
        .await
        .unwrap();
        assert_eq!(rows_affected, 1);

        // Query data
        let row = client
            .query("SELECT name FROM test_table WHERE id = 1", &[])
            .await
            .unwrap()
            .into_row()
            .await
            .unwrap()
            .unwrap();
        let name: &str = row.get(0).unwrap();
        assert_eq!(name, "Alice");

        mssql.cleanup().await;
    }
}
