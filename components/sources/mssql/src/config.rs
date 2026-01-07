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

use serde::{Deserialize, Serialize};

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

/// Starting position when no LSN is found in the state store
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StartPosition {
    /// Start from the beginning (earliest available LSN)
    Beginning,
    /// Start from the current LSN (now)
    Current,
}

impl Default for StartPosition {
    fn default() -> Self {
        Self::Current
    }
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
}
