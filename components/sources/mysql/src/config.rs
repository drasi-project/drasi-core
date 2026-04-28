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

//! Configuration for the MySQL replication source.
//!
//! This source monitors MySQL databases using binlog replication to stream
//! data changes as they occur.

// =============================================================================
// SSL Configuration
// =============================================================================

/// SSL mode for MySQL connections
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    /// Disable SSL encryption (required by mysql_cdc runtime)
    Disabled,
    /// Try SSL but allow unencrypted connections
    IfAvailable,
    /// Require SSL encryption
    Require,
    /// Require SSL with CA verification
    RequireVerifyCa,
    /// Require SSL with CA and hostname verification
    RequireVerifyFull,
}

impl Default for SslMode {
    fn default() -> Self {
        Self::Disabled
    }
}

// =============================================================================
// Database Table Configuration
// =============================================================================

/// Table key configuration for MySQL sources
#[derive(Debug, Clone, PartialEq)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

/// Where to start the binlog replication stream
#[derive(Debug, Clone, PartialEq)]
pub enum StartPosition {
    FromStart,
    FromEnd,
    FromPosition { file: String, position: u32 },
    FromGtid(String),
}

impl Default for StartPosition {
    fn default() -> Self {
        Self::FromEnd
    }
}

/// MySQL replication source configuration
#[derive(Debug, Clone, PartialEq)]
pub struct MySqlSourceConfig {
    /// MySQL host
    pub host: String,

    /// MySQL port
    pub port: u16,

    /// Database name
    pub database: String,

    /// Database user
    pub user: String,

    /// Database password
    pub password: String,

    /// Tables to replicate
    pub tables: Vec<String>,

    /// SSL mode
    pub ssl_mode: SslMode,

    /// Table key configurations
    pub table_keys: Vec<TableKeyConfig>,

    /// Starting position
    pub start_position: StartPosition,

    /// Replication server ID
    pub server_id: u32,

    /// Heartbeat interval in seconds
    pub heartbeat_interval_seconds: u64,
}

impl MySqlSourceConfig {
    /// Validate the configuration and return an error if invalid.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Database name is empty
    /// - User is empty
    /// - Port is 0
    /// - SSL mode is not Disabled (mysql_cdc does not support SSL at runtime)
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.database.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: database cannot be empty. \
                 Please specify the MySQL database name"
            ));
        }

        if self.user.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: user cannot be empty. \
                 Please specify the MySQL user for replication"
            ));
        }

        if self.port == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: port cannot be 0. \
                 Please specify a valid port number (1-65535)"
            ));
        }

        if self.ssl_mode != SslMode::Disabled {
            return Err(anyhow::anyhow!(
                "Validation error: mysql_cdc does not support SSL at runtime. \
                 Please set ssl_mode to Disabled"
            ));
        }

        Ok(())
    }
}
