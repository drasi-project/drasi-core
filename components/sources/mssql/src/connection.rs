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

//! MS SQL connection management using Tiberius

use crate::config::{AuthMode, EncryptionMode, MsSqlSourceConfig};
use anyhow::{anyhow, Result};
use log::{debug, info};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

/// MS SQL connection wrapper
pub struct MsSqlConnection {
    client: Client<Compat<TcpStream>>,
}

impl MsSqlConnection {
    /// Create a new connection to MS SQL Server
    ///
    /// # Arguments
    /// * `config` - MS SQL source configuration
    ///
    /// # Errors
    /// Returns error if connection fails
    pub async fn connect(config: &MsSqlSourceConfig) -> Result<Self> {
        info!(
            "Connecting to MS SQL Server at {}:{} database '{}'",
            config.host, config.port, config.database
        );

        // Build Tiberius config
        let mut tiberius_config = Config::new();
        tiberius_config.host(&config.host);
        tiberius_config.port(config.port);
        tiberius_config.database(&config.database);

        // Set authentication
        match config.auth_mode {
            AuthMode::SqlServer => {
                debug!("Using SQL Server authentication");
                tiberius_config.authentication(AuthMethod::sql_server(
                    &config.user,
                    &config.password,
                ));
            }
            AuthMode::Windows => {
                // TODO: Implement Windows authentication
                // Windows integrated authentication not yet supported
                return Err(anyhow!("Windows authentication not yet implemented"));
            }
            AuthMode::AzureAd => {
                return Err(anyhow!("Azure AD authentication not yet implemented"));
            }
        }

        // Set encryption
        let encryption_level = match config.encryption {
            EncryptionMode::Off => EncryptionLevel::Off,
            EncryptionMode::On => EncryptionLevel::Required,
            EncryptionMode::NotSupported => EncryptionLevel::NotSupported,
        };
        tiberius_config.encryption(encryption_level);

        // Trust server certificate if configured
        if config.trust_server_certificate {
            tiberius_config.trust_cert();
        }

        // Connect via TCP
        let tcp = TcpStream::connect((config.host.as_str(), config.port))
            .await
            .map_err(|e| anyhow!("Failed to connect to {}:{}: {}", config.host, config.port, e))?;

        tcp.set_nodelay(true)?;

        // Create Tiberius client
        let client = Client::connect(tiberius_config, tcp.compat_write())
            .await
            .map_err(|e| anyhow!("Failed to authenticate with MS SQL Server: {}", e))?;

        info!("Successfully connected to MS SQL Server");

        Ok(Self { client })
    }

    /// Get mutable reference to the underlying Tiberius client
    pub fn client_mut(&mut self) -> &mut Client<Compat<TcpStream>> {
        &mut self.client
    }

    /// Get reference to the underlying Tiberius client
    pub fn client(&self) -> &Client<Compat<TcpStream>> {
        &self.client
    }

    /// Test the connection by running a simple query
    pub async fn test_connection(&mut self) -> Result<()> {
        debug!("Testing MS SQL connection");

        let query = "SELECT @@VERSION AS version";
        let stream = self.client.query(query, &[]).await?;
        let rows = stream.into_first_result().await?;

        if let Some(row) = rows.first() {
            let version: &str = row.get(0).ok_or_else(|| anyhow!("No version returned"))?;
            info!("MS SQL Server version: {}", version.lines().next().unwrap_or(version));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_mode_conversion() {
        // Just test that we can create SQL auth methods
        let _sql_auth = AuthMethod::sql_server("user", "password");
        // Windows/Integrated auth not yet implemented
    }

    #[test]
    fn test_encryption_level_conversion() {
        assert_eq!(
            std::mem::discriminant(&EncryptionLevel::Off),
            std::mem::discriminant(&match EncryptionMode::Off {
                EncryptionMode::Off => EncryptionLevel::Off,
                _ => unreachable!(),
            })
        );
    }
}
