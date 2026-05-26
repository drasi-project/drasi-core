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

pub use drasi_mysql_common::{is_valid_identifier, TableKeyConfig};

// =============================================================================
// SSL Configuration
// =============================================================================

/// SSL mode for MySQL connections
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SslMode {
    /// Disable SSL encryption
    Disabled,
    /// Try SSL but allow unencrypted connections.
    #[default]
    IfAvailable,
    /// Require SSL encryption.
    Require,
    /// Require SSL with CA verification.
    RequireVerifyCa,
    /// Require SSL with CA and hostname verification.
    RequireVerifyFull,
}

// =============================================================================
// Database Table Configuration
// =============================================================================

/// Where to start the binlog replication stream
#[derive(Debug, Clone, Default, PartialEq)]
pub enum StartPosition {
    FromStart,
    #[default]
    FromEnd,
    FromPosition { file: String, position: u32 },
    FromGtid(String),
}

/// MySQL replication source configuration
#[derive(Clone, PartialEq)]
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

impl std::fmt::Debug for MySqlSourceConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MySqlSourceConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("database", &self.database)
            .field("user", &self.user)
            .field("password", &"[REDACTED]")
            .field("tables", &self.tables)
            .field("ssl_mode", &self.ssl_mode)
            .field("table_keys", &self.table_keys)
            .field("start_position", &self.start_position)
            .field("server_id", &self.server_id)
            .field(
                "heartbeat_interval_seconds",
                &self.heartbeat_interval_seconds,
            )
            .finish()
    }
}

impl MySqlSourceConfig {
    /// Validate the configuration and return an error if invalid.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Host is empty
    /// - Database name is empty
    /// - User is empty
    /// - Port is 0
    /// - Server ID is 0
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.host.trim().is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: host cannot be empty.                  Please specify the MySQL host name or address"
            ));
        }

        if self.database.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: database cannot be empty.                  Please specify the MySQL database name"
            ));
        }

        if self.user.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: user cannot be empty.                  Please specify the MySQL user for replication"
            ));
        }

        if self.port == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: port cannot be 0.                  Please specify a valid port number (1-65535)"
            ));
        }

        if self.server_id == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: server_id cannot be 0.                  Please specify a resolved replication server ID"
            ));
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper function to create a valid baseline configuration
    fn valid_config() -> MySqlSourceConfig {
        MySqlSourceConfig {
            host: "localhost".to_string(),
            port: 3306,
            database: "test_db".to_string(),
            user: "test_user".to_string(),
            password: "test_password".to_string(),
            tables: vec![],
            ssl_mode: SslMode::IfAvailable,
            table_keys: vec![],
            start_position: StartPosition::FromEnd,
            server_id: 1,
            heartbeat_interval_seconds: 30,
        }
    }

    #[test]
    fn test_validate_valid_config_succeeds() {
        let config = valid_config();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_validate_empty_host_fails() {
        let mut config = valid_config();
        config.host = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_whitespace_host_fails() {
        let mut config = valid_config();
        config.host = "   ".to_string();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_database_fails() {
        let mut config = valid_config();
        config.database = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_user_fails() {
        let mut config = valid_config();
        config.user = String::new();
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_port_zero_fails() {
        let mut config = valid_config();
        config.port = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_server_id_zero_fails() {
        let mut config = valid_config();
        config.server_id = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn test_validate_empty_tables_succeeds() {
        let config = valid_config();
        assert_eq!(config.tables.len(), 0);
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_debug_redacts_password() {
        let config = valid_config();
        let debug_str = format!("{config:?}");
        assert!(debug_str.contains("[REDACTED]"));
        assert!(!debug_str.contains("test_password"));
    }
}
