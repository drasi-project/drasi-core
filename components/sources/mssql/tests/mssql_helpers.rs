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

//! Test utilities for MSSQL-based integration tests using testcontainers.

use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::Duration;
use testcontainers::core::IntoContainerPort;
use testcontainers::runners::AsyncRunner;
use testcontainers::{ContainerAsync, GenericImage, ImageExt};
use tiberius::{Client, Config};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

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
    /// Get tiberius Config
    pub fn tiberius_config(&self) -> Result<Config> {
        let mut config = Config::new();
        config.host(&self.host);
        config.port(self.port);
        config.authentication(tiberius::AuthMethod::sql_server(&self.user, &self.password));
        config.database(&self.database);
        // Match production default (EncryptionMode::NotSupported). Required/On TLS
        // fails on macOS against SQL Server's self-signed certs (native-tls rejects
        // long validity periods / closes the handshake).
        config.encryption(tiberius::EncryptionLevel::NotSupported);
        if self.trust_server_certificate {
            config.trust_cert();
        }
        Ok(config)
    }

    /// Create a tiberius client connection
    pub async fn connect(&self) -> Result<Client<Compat<TcpStream>>> {
        let config = self.tiberius_config()?;
        let tcp = TcpStream::connect(config.get_addr()).await?;
        let client = Client::connect(config, tcp.compat_write()).await?;
        Ok(client)
    }
}

/// Guard wrapper for MSSQL container that ensures proper cleanup
#[derive(Clone)]
pub struct MssqlGuard {
    inner: Arc<MssqlGuardInner>,
}

struct MssqlGuardInner {
    container: std::sync::Mutex<Option<ContainerAsync<GenericImage>>>,
    config: MssqlConfig,
}

impl MssqlGuard {
    /// Create a new MSSQL container with guaranteed cleanup
    pub async fn new() -> Result<Self> {
        let (container, config) = setup_mssql_raw().await?;
        Ok(Self {
            inner: Arc::new(MssqlGuardInner {
                container: std::sync::Mutex::new(Some(container)),
                config,
            }),
        })
    }

    /// Get the MSSQL configuration
    pub fn config(&self) -> &MssqlConfig {
        &self.inner.config
    }

    /// Get a MSSQL client connection
    pub async fn get_client(&self) -> Result<Client<Compat<TcpStream>>> {
        let config = self.config().tiberius_config()?;
        let tcp = TcpStream::connect(config.get_addr()).await?;
        let client = Client::connect(config, tcp.compat_write()).await?;
        Ok(client)
    }

    /// Explicitly stop and remove the container
    pub async fn cleanup(self) {
        let container_to_stop = {
            if let Ok(mut container_guard) = self.inner.container.lock() {
                container_guard.take()
            } else {
                None
            }
        };

        if let Some(container) = container_to_stop {
            let _ = container.stop().await;
        }
    }
}

/// Setup a MSSQL testcontainer and return a guard that manages cleanup
pub async fn setup_mssql() -> Result<MssqlGuard> {
    MssqlGuard::new().await
}

async fn setup_mssql_raw() -> Result<(ContainerAsync<GenericImage>, MssqlConfig)> {
    let password = "YourStrong@Passw0rd";
    // Full SQL Server is required for CDC (SQL Agent). On Apple Silicon use the
    // linux/amd64 image under emulation rather than Azure SQL Edge, which lacks
    // a usable Agent/CDC path for these tests.
    let init_delay = if cfg!(target_arch = "aarch64") {
        45_000
    } else {
        15_000
    };

    let mut image = GenericImage::new("mcr.microsoft.com/mssql/server", "2022-latest")
        .with_exposed_port(1433.tcp())
        .with_env_var("ACCEPT_EULA", "Y")
        .with_env_var("MSSQL_SA_PASSWORD", password)
        .with_env_var("MSSQL_PID", "Developer")
        .with_env_var("MSSQL_AGENT_ENABLED", "true");

    if cfg!(target_arch = "aarch64") {
        image = image.with_platform("linux/amd64");
    }

    let container = image.start().await?;
    let host_port = container.get_host_port_ipv4(1433.tcp()).await?;
    // Prefer 127.0.0.1 over localhost to avoid IPv6 (::1) resolution issues on macOS
    // when Docker only publishes the IPv4 port mapping.
    let reported_host = container.get_host().await?.to_string();
    let host = if reported_host == "localhost" || reported_host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        reported_host
    };

    let config = MssqlConfig {
        host: host.clone(),
        port: host_port,
        database: "master".to_string(),
        user: "sa".to_string(),
        password: password.to_string(),
        trust_server_certificate: true,
    };

    eprintln!(
        "MSSQL testcontainer started at {}:{} (init_delay_ms={init_delay})",
        config.host, config.port
    );

    tokio::time::sleep(Duration::from_millis(init_delay)).await;
    wait_for_mssql_ready(&config).await?;

    Ok((container, config))
}

async fn wait_for_mssql_ready(config: &MssqlConfig) -> Result<()> {
    // Emulated linux/amd64 SQL Server on Apple Silicon can take several minutes.
    let max_retries = 90;
    let retry_delay = Duration::from_secs(3);
    let mut last_err = None;

    for attempt in 1..=max_retries {
        match config.connect().await {
            Ok(mut client) => match client.query("SELECT 1", &[]).await {
                Ok(stream) => {
                    if stream.into_results().await.is_ok() {
                        eprintln!("MSSQL ready after attempt {attempt}");
                        return Ok(());
                    }
                    last_err = Some(anyhow!("SELECT 1 returned no usable results"));
                }
                Err(e) => last_err = Some(anyhow!("query failed: {e}")),
            },
            Err(e) => last_err = Some(anyhow!("connect failed: {e}")),
        }

        if attempt == 1 || attempt % 10 == 0 {
            if let Some(err) = &last_err {
                eprintln!("MSSQL readiness attempt {attempt}/{max_retries}: {err}");
            }
        }

        if attempt < max_retries {
            tokio::time::sleep(retry_delay).await;
        }
    }

    Err(anyhow!(
        "MSSQL failed to become ready after {max_retries} attempts (host={} port={}): {}",
        config.host,
        config.port,
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    ))
}

/// Execute a SQL statement on the MSSQL database
pub async fn execute_sql(client: &mut Client<Compat<TcpStream>>, sql: &str) -> Result<()> {
    let stream = client.simple_query(sql).await?;
    let _ = stream.into_results().await?;
    Ok(())
}
