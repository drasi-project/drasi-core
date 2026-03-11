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

//! Configuration for Oracle source and bootstrap providers.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

const MAX_IDENTIFIER_LENGTH: usize = 128;
pub const ORACLE_BOOTSTRAP_SCN_CONTEXT_PROPERTY: &str = "oracle.bootstrap_scn";

pub fn validate_sql_identifier(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(anyhow!("SQL identifier cannot be empty"));
    }

    if name.len() > MAX_IDENTIFIER_LENGTH {
        return Err(anyhow!(
            "SQL identifier exceeds maximum length of {MAX_IDENTIFIER_LENGTH} characters"
        ));
    }

    for (idx, ch) in name.chars().enumerate() {
        if !ch.is_ascii_alphanumeric() && ch != '_' && ch != '.' {
            return Err(anyhow!(
                "Invalid character '{ch}' at position {idx} in SQL identifier '{name}'. Only alphanumeric characters, underscores, and dots are allowed."
            ));
        }
    }

    if name.chars().next().is_some_and(|ch| ch.is_ascii_digit()) {
        return Err(anyhow!("SQL identifier '{name}' cannot start with a digit"));
    }

    if name.starts_with('.') || name.ends_with('.') || name.contains("..") {
        return Err(anyhow!("SQL identifier '{name}' has invalid dot placement"));
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    #[default]
    Basic,
}

impl std::fmt::Display for AuthMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Basic => write!(f, "basic"),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum StartPosition {
    Beginning,
    #[default]
    Current,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum SslMode {
    #[default]
    Disable,
    Require,
}

impl std::fmt::Display for SslMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disable => write!(f, "disable"),
            Self::Require => write!(f, "require"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OracleSourceConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_database", alias = "service")]
    pub database: String,
    pub user: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub auth_mode: AuthMode,
    #[serde(default)]
    pub tables: Vec<String>,
    #[serde(default = "default_poll_interval_ms")]
    pub poll_interval_ms: u64,
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,
    #[serde(default)]
    pub start_position: StartPosition,
    #[serde(default)]
    pub ssl_mode: SslMode,
}

fn default_host() -> String {
    "localhost".to_string()
}

fn default_port() -> u16 {
    1521
}

fn default_database() -> String {
    "FREEPDB1".to_string()
}

fn default_poll_interval_ms() -> u64 {
    1000
}

impl Default for OracleSourceConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            database: default_database(),
            user: String::new(),
            password: String::new(),
            auth_mode: AuthMode::default(),
            tables: Vec::new(),
            poll_interval_ms: default_poll_interval_ms(),
            table_keys: Vec::new(),
            start_position: StartPosition::default(),
            ssl_mode: SslMode::default(),
        }
    }
}

impl OracleSourceConfig {
    pub fn validate(&self) -> Result<()> {
        if self.database.trim().is_empty() {
            return Err(anyhow!("Oracle service name is required"));
        }

        if self.user.trim().is_empty() {
            return Err(anyhow!("Oracle user is required"));
        }

        if self.poll_interval_ms == 0 {
            return Err(anyhow!("poll_interval_ms must be greater than zero"));
        }

        for table in &self.tables {
            validate_sql_identifier(table)?;
        }

        for table_key in &self.table_keys {
            validate_sql_identifier(&table_key.table)?;
            if table_key.key_columns.is_empty() {
                return Err(anyhow!(
                    "table_keys entry for '{}' must include at least one key column",
                    table_key.table
                ));
            }
            for column in &table_key.key_columns {
                validate_sql_identifier(column)?;
            }
        }

        Ok(())
    }

    pub fn connect_string(&self) -> String {
        match self.ssl_mode {
            SslMode::Disable => format!("{}:{}/{}", self.host, self.port, self.database),
            SslMode::Require => format!("tcps://{}:{}/{}", self.host, self.port, self.database),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = OracleSourceConfig::default();
        assert_eq!(config.host, "localhost");
        assert_eq!(config.port, 1521);
        assert_eq!(config.database, "FREEPDB1");
        assert_eq!(config.poll_interval_ms, 1000);
        assert_eq!(config.auth_mode, AuthMode::Basic);
        assert_eq!(config.start_position, StartPosition::Current);
    }

    #[test]
    fn test_connect_string() {
        let mut config = OracleSourceConfig {
            host: "db.example.com".to_string(),
            database: "ORCLPDB1".to_string(),
            ..OracleSourceConfig::default()
        };
        assert_eq!(config.connect_string(), "db.example.com:1521/ORCLPDB1");

        config.ssl_mode = SslMode::Require;
        assert_eq!(
            config.connect_string(),
            "tcps://db.example.com:1521/ORCLPDB1"
        );
    }

    #[test]
    fn test_validate_sql_identifier_valid() {
        assert!(validate_sql_identifier("HR.EMPLOYEES").is_ok());
        assert!(validate_sql_identifier("employees").is_ok());
        assert!(validate_sql_identifier("order_items").is_ok());
    }

    #[test]
    fn test_validate_sql_identifier_invalid() {
        assert!(validate_sql_identifier("").is_err());
        assert!(validate_sql_identifier("1table").is_err());
        assert!(validate_sql_identifier("hr..employees").is_err());
        assert!(validate_sql_identifier("employees;drop table users").is_err());
    }
}
