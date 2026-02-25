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

//! Configuration for MS SQL CDC source

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// Maximum length for SQL identifiers (SQL Server limit is 128)
const MAX_IDENTIFIER_LENGTH: usize = 128;

/// Validate a SQL identifier to prevent SQL injection
///
/// Valid identifiers contain only:
/// - Alphanumeric characters (a-z, A-Z, 0-9)
/// - Underscores (_)
/// - Dots (.) for schema.table notation
///
/// # Arguments
/// * `name` - The identifier to validate
///
/// # Returns
/// * `Ok(())` if the identifier is valid
/// * `Err` if the identifier contains invalid characters or is empty
///
/// # Example
/// ```
/// use drasi_mssql_common::validate_sql_identifier;
///
/// assert!(validate_sql_identifier("orders").is_ok());
/// assert!(validate_sql_identifier("dbo.orders").is_ok());
/// assert!(validate_sql_identifier("order_items").is_ok());
/// assert!(validate_sql_identifier("orders; DROP TABLE users--").is_err());
/// ```
pub fn validate_sql_identifier(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("SQL identifier cannot be empty"));
    }

    if name.len() > MAX_IDENTIFIER_LENGTH {
        return Err(anyhow!(
            "SQL identifier exceeds maximum length of {MAX_IDENTIFIER_LENGTH} characters"
        ));
    }

    // Check that all characters are valid SQL identifier characters
    for (i, c) in name.chars().enumerate() {
        if !c.is_ascii_alphanumeric() && c != '_' && c != '.' {
            return Err(anyhow!(
                "Invalid character '{c}' at position {i} in SQL identifier '{name}'. \
                 Only alphanumeric characters, underscores, and dots are allowed."
            ));
        }
    }

    // Identifier cannot start with a digit
    if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return Err(anyhow!("SQL identifier '{name}' cannot start with a digit"));
    }

    // Check for consecutive dots or leading/trailing dots
    if name.starts_with('.') || name.ends_with('.') || name.contains("..") {
        return Err(anyhow!("SQL identifier '{name}' has invalid dot placement"));
    }

    Ok(())
}

/// Authentication mode for MS SQL Server
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum AuthMode {
    /// SQL Server authentication (username/password)
    #[default]
    SqlServer,
    /// Windows integrated authentication (Kerberos)
    Windows,
    /// Azure AD authentication
    AzureAd,
}


impl std::fmt::Display for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SqlServer => write!(f, "sql_server"),
            Self::Windows => write!(f, "windows"),
            Self::AzureAd => write!(f, "azure_ad"),
        }
    }
}

/// TLS/SSL encryption mode
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum EncryptionMode {
    /// No encryption
    Off,
    /// Require encryption
    On,
    /// Encrypt if supported, otherwise allow unencrypted
    #[default]
    NotSupported,
}


/// Starting position when no LSN is found in the state store
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum StartPosition {
    /// Start from the beginning (earliest available LSN)
    Beginning,
    /// Start from the current LSN (now)
    #[default]
    Current,
}


impl std::fmt::Display for EncryptionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Off => write!(f, "off"),
            Self::On => write!(f, "on"),
            Self::NotSupported => write!(f, "not_supported"),
        }
    }
}

/// Table key configuration for custom primary keys
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableKeyConfig {
    /// Table name
    pub table: String,
    /// Column names that form the primary key
    pub key_columns: Vec<String>,
}

/// MS SQL CDC source configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MsSqlSourceConfig {
    /// MS SQL server hostname or IP address
    #[serde(default = "default_host")]
    pub host: String,

    /// MS SQL server port
    #[serde(default = "default_port")]
    pub port: u16,

    /// Database name
    pub database: String,

    /// Database user
    pub user: String,

    /// Database password
    #[serde(default)]
    pub password: String,

    /// Authentication mode
    #[serde(default)]
    pub auth_mode: AuthMode,

    /// Tables to monitor (empty = all CDC-enabled tables)
    #[serde(default)]
    pub tables: Vec<String>,

    /// CDC polling interval in milliseconds
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,

    /// TLS/SSL encryption mode
    #[serde(default)]
    pub encryption: EncryptionMode,

    /// Trust server certificate (for self-signed certificates)
    #[serde(default)]
    pub trust_server_certificate: bool,

    /// Custom primary key configuration
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,

    /// Starting position when no LSN is found in the state store
    #[serde(default)]
    pub start_position: StartPosition,
}

fn default_host() -> String {
    "localhost".to_string()
}

fn default_port() -> u16 {
    1433
}

fn default_poll_interval_ms() -> u64 {
    1000 // 1 second
}

impl Default for MsSqlSourceConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            database: String::new(),
            user: String::new(),
            password: String::new(),
            auth_mode: AuthMode::default(),
            tables: Vec::new(),
            poll_interval_ms: default_poll_interval_ms(),
            encryption: EncryptionMode::default(),
            trust_server_certificate: false,
            table_keys: Vec::new(),
            start_position: StartPosition::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = MsSqlSourceConfig::default();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 1433);
        assert_eq!(config.poll_interval_ms, 1000);
        assert_eq!(config.auth_mode, AuthMode::SqlServer);
        assert_eq!(config.encryption, EncryptionMode::NotSupported);
        assert!(!config.trust_server_certificate);
    }

    #[test]
    fn test_config_serialization() {
        let config = MsSqlSourceConfig {
            host: "sqlserver.example.com".to_string(),
            port: 1433,
            database: "production".to_string(),
            user: "drasi_user".to_string(),
            password: "secret".to_string(),
            auth_mode: AuthMode::SqlServer,
            tables: vec!["orders".to_string(), "customers".to_string()],
            poll_interval_ms: 2000,
            encryption: EncryptionMode::On,
            trust_server_certificate: true,
            table_keys: vec![TableKeyConfig {
                table: "orders".to_string(),
                key_columns: vec!["order_id".to_string()],
            }],
            start_position: StartPosition::Beginning,
        };

        let json = serde_json::to_string(&config).unwrap();
        let deserialized: MsSqlSourceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, deserialized);
    }

    #[test]
    fn test_auth_mode_display() {
        assert_eq!(AuthMode::SqlServer.to_string(), "sql_server");
        assert_eq!(AuthMode::Windows.to_string(), "windows");
        assert_eq!(AuthMode::AzureAd.to_string(), "azure_ad");
    }

    #[test]
    fn test_encryption_mode_display() {
        assert_eq!(EncryptionMode::Off.to_string(), "off");
        assert_eq!(EncryptionMode::On.to_string(), "on");
        assert_eq!(EncryptionMode::NotSupported.to_string(), "not_supported");
    }

    #[test]
    fn test_table_key_config() {
        let tk = TableKeyConfig {
            table: "orders".to_string(),
            key_columns: vec!["order_id".to_string(), "line_item".to_string()],
        };

        assert_eq!(tk.table, "orders");
        assert_eq!(tk.key_columns.len(), 2);
    }

    #[test]
    fn test_start_position_default() {
        assert_eq!(StartPosition::default(), StartPosition::Current);
    }

    #[test]
    fn test_start_position_serialization() {
        let json = serde_json::to_string(&StartPosition::Beginning).unwrap();
        assert_eq!(json, "\"beginning\"");

        let json = serde_json::to_string(&StartPosition::Current).unwrap();
        assert_eq!(json, "\"current\"");
    }

    #[test]
    fn test_validate_sql_identifier_valid() {
        // Simple table names
        assert!(validate_sql_identifier("orders").is_ok());
        assert!(validate_sql_identifier("Orders").is_ok());
        assert!(validate_sql_identifier("order_items").is_ok());
        assert!(validate_sql_identifier("Order_Items_2024").is_ok());

        // Schema-qualified names
        assert!(validate_sql_identifier("dbo.orders").is_ok());
        assert!(validate_sql_identifier("sales.order_items").is_ok());
        assert!(validate_sql_identifier("MySchema.MyTable").is_ok());
    }

    #[test]
    fn test_validate_sql_identifier_sql_injection() {
        // SQL injection attempts
        assert!(validate_sql_identifier("orders; DROP TABLE users--").is_err());
        assert!(validate_sql_identifier("orders'; DELETE FROM users;--").is_err());
        assert!(validate_sql_identifier("orders OR 1=1").is_err());
        assert!(validate_sql_identifier("orders/**/UNION/**/SELECT").is_err());
        assert!(validate_sql_identifier("orders\n; DROP TABLE").is_err());
    }

    #[test]
    fn test_validate_sql_identifier_empty() {
        assert!(validate_sql_identifier("").is_err());
    }

    #[test]
    fn test_validate_sql_identifier_too_long() {
        let long_name = "a".repeat(129);
        assert!(validate_sql_identifier(&long_name).is_err());

        let valid_long_name = "a".repeat(128);
        assert!(validate_sql_identifier(&valid_long_name).is_ok());
    }

    #[test]
    fn test_validate_sql_identifier_invalid_start() {
        assert!(validate_sql_identifier("123table").is_err());
        assert!(validate_sql_identifier("1orders").is_err());
    }

    #[test]
    fn test_validate_sql_identifier_invalid_dots() {
        assert!(validate_sql_identifier(".orders").is_err());
        assert!(validate_sql_identifier("orders.").is_err());
        assert!(validate_sql_identifier("dbo..orders").is_err());
    }
}
