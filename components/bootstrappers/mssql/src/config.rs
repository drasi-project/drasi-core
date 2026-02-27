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

//! Configuration types for the MS SQL bootstrap provider.
//!
//! These types are defined locally to keep this component independent
//! and self-contained, without dependencies on the source component.

use anyhow::anyhow;
use serde::{Deserialize, Serialize};

/// Maximum length for SQL identifiers (SQL Server limit is 128)
const MAX_IDENTIFIER_LENGTH: usize = 128;

/// Validate a SQL identifier to prevent SQL injection
///
/// Valid identifiers contain only:
/// - Alphanumeric characters (a-z, A-Z, 0-9)
/// - Underscores (_)
/// - Dots (.) for schema.table notation
pub fn validate_sql_identifier(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        return Err(anyhow!("SQL identifier cannot be empty"));
    }

    if name.len() > MAX_IDENTIFIER_LENGTH {
        return Err(anyhow!(
            "SQL identifier exceeds maximum length of {MAX_IDENTIFIER_LENGTH} characters"
        ));
    }

    for (i, c) in name.chars().enumerate() {
        if !c.is_ascii_alphanumeric() && c != '_' && c != '.' {
            return Err(anyhow!(
                "Invalid character '{c}' at position {i} in SQL identifier '{name}'. \
                 Only alphanumeric characters, underscores, and dots are allowed."
            ));
        }
    }

    if name.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return Err(anyhow!("SQL identifier '{name}' cannot start with a digit"));
    }

    if name.starts_with('.') || name.ends_with('.') || name.contains("..") {
        return Err(anyhow!("SQL identifier '{name}' has invalid dot placement"));
    }

    Ok(())
}

/// Authentication mode for MS SQL Server
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// SQL Server authentication (username/password)
    SqlServer,
    /// Windows integrated authentication (Kerberos)
    Windows,
    /// Azure AD authentication
    AzureAd,
}

impl Default for AuthMode {
    fn default() -> Self {
        Self::SqlServer
    }
}

/// TLS/SSL encryption mode
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum EncryptionMode {
    /// No encryption
    Off,
    /// Require encryption
    On,
    /// Encrypt if supported, otherwise allow unencrypted
    NotSupported,
}

impl Default for EncryptionMode {
    fn default() -> Self {
        Self::NotSupported
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

/// MS SQL bootstrap provider configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MsSqlBootstrapConfig {
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

    /// Tables to bootstrap
    #[serde(default)]
    pub tables: Vec<String>,

    /// TLS/SSL encryption mode
    #[serde(default)]
    pub encryption: EncryptionMode,

    /// Trust server certificate (for self-signed certificates)
    #[serde(default)]
    pub trust_server_certificate: bool,

    /// Custom primary key configuration
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,
}

fn default_host() -> String {
    "localhost".to_string()
}

fn default_port() -> u16 {
    1433
}

impl Default for MsSqlBootstrapConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            database: String::new(),
            user: String::new(),
            password: String::new(),
            auth_mode: AuthMode::default(),
            tables: Vec::new(),
            encryption: EncryptionMode::default(),
            trust_server_certificate: false,
            table_keys: Vec::new(),
        }
    }
}

impl MsSqlBootstrapConfig {
    /// Validate the configuration and return an error if invalid.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.database.is_empty() {
            return Err(anyhow!("Database name is required"));
        }
        if self.user.is_empty() {
            return Err(anyhow!("Database user is required"));
        }
        if self.port == 0 {
            return Err(anyhow!("Port cannot be 0"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = MsSqlBootstrapConfig::default();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 1433);
        assert_eq!(config.auth_mode, AuthMode::SqlServer);
        assert_eq!(config.encryption, EncryptionMode::NotSupported);
        assert!(!config.trust_server_certificate);
    }

    #[test]
    fn test_validate_sql_identifier_valid() {
        assert!(validate_sql_identifier("orders").is_ok());
        assert!(validate_sql_identifier("dbo.orders").is_ok());
        assert!(validate_sql_identifier("order_items").is_ok());
    }

    #[test]
    fn test_validate_sql_identifier_invalid() {
        assert!(validate_sql_identifier("").is_err());
        assert!(validate_sql_identifier("orders; DROP TABLE users--").is_err());
        assert!(validate_sql_identifier("123table").is_err());
        assert!(validate_sql_identifier(".orders").is_err());
    }
}
