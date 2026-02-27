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

//! MS SQL connection management for the bootstrap provider

use crate::config::{AuthMode, EncryptionMode, MsSqlBootstrapConfig};
use anyhow::{anyhow, Result};
use log::{debug, info};
use tiberius::{AuthMethod, Client, Config, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

/// MS SQL connection wrapper for bootstrap operations
pub struct MsSqlBootstrapConnection {
    client: Client<Compat<TcpStream>>,
}

impl MsSqlBootstrapConnection {
    /// Create a new connection to MS SQL Server
    pub async fn connect(config: &MsSqlBootstrapConfig) -> Result<Self> {
        info!(
            "Connecting to MS SQL Server at {}:{} database '{}'",
            config.host, config.port, config.database
        );

        let mut tiberius_config = Config::new();
        tiberius_config.host(&config.host);
        tiberius_config.port(config.port);
        tiberius_config.database(&config.database);

        match config.auth_mode {
            AuthMode::SqlServer => {
                debug!("Using SQL Server authentication");
                tiberius_config
                    .authentication(AuthMethod::sql_server(&config.user, &config.password));
            }
            AuthMode::Windows => {
                return Err(anyhow!("Windows authentication not yet implemented"));
            }
            AuthMode::AzureAd => {
                return Err(anyhow!("Azure AD authentication not yet implemented"));
            }
        }

        let encryption_level = match config.encryption {
            EncryptionMode::Off => EncryptionLevel::Off,
            EncryptionMode::On => EncryptionLevel::Required,
            EncryptionMode::NotSupported => EncryptionLevel::NotSupported,
        };
        tiberius_config.encryption(encryption_level);

        if config.trust_server_certificate {
            tiberius_config.trust_cert();
        }

        let tcp = TcpStream::connect((config.host.as_str(), config.port))
            .await
            .map_err(|e| {
                anyhow!(
                    "Failed to connect to {}:{}: {}",
                    config.host,
                    config.port,
                    e
                )
            })?;

        tcp.set_nodelay(true)?;

        let client = Client::connect(tiberius_config, tcp.compat_write())
            .await
            .map_err(|e| anyhow!("Failed to authenticate with MS SQL Server: {e}"))?;

        info!("Successfully connected to MS SQL Server");

        Ok(Self { client })
    }

    /// Get mutable reference to the underlying Tiberius client
    pub fn client_mut(&mut self) -> &mut Client<Compat<TcpStream>> {
        &mut self.client
    }
}
