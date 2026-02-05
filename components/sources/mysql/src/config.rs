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

use serde::{Deserialize, Serialize};

// =============================================================================
// SSL Configuration
// =============================================================================

/// SSL mode for MySQL connections
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

/// Where to start the binlog replication stream
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MySqlSourceConfig {
    /// MySQL host
    #[serde(default = "default_mysql_host")]
    pub host: String,

    /// MySQL port
    #[serde(default = "default_mysql_port")]
    pub port: u16,

    /// Database name
    pub database: String,

    /// Database user
    pub user: String,

    /// Database password
    #[serde(default)]
    pub password: String,

    /// Tables to replicate
    #[serde(default)]
    pub tables: Vec<String>,

    /// SSL mode
    #[serde(default)]
    pub ssl_mode: SslMode,

    /// Table key configurations
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,

    /// Starting position
    #[serde(default)]
    pub start_position: StartPosition,

    /// Replication server ID
    #[serde(default = "default_server_id")]
    pub server_id: u32,

    /// Heartbeat interval in seconds
    #[serde(default = "default_heartbeat_seconds")]
    pub heartbeat_interval_seconds: u64,
}

fn default_mysql_host() -> String {
    "localhost".to_string()
}

fn default_mysql_port() -> u16 {
    3306
}

fn default_server_id() -> u32 {
    65535
}

fn default_heartbeat_seconds() -> u64 {
    30
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
