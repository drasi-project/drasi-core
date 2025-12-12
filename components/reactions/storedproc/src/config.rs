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

//! Configuration for the Stored Procedure reaction.

use serde::{Deserialize, Serialize};

/// Database client type
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DatabaseClient {
    /// PostgreSQL database
    #[serde(rename = "pg")]
    PostgreSQL,
    /// MySQL database
    #[serde(rename = "mysql")]
    MySQL,
    /// Microsoft SQL Server
    #[serde(rename = "mssql")]
    MsSQL,
}

impl Default for DatabaseClient {
    fn default() -> Self {
        Self::PostgreSQL
    }
}

impl std::fmt::Display for DatabaseClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PostgreSQL => write!(f, "PostgreSQL"),
            Self::MySQL => write!(f, "MySQL"),
            Self::MsSQL => write!(f, "MS SQL Server"),
        }
    }
}

/// Configuration for the Stored Procedure reaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredProcReactionConfig {
    /// Database client type (pg, mysql, mssql)
    #[serde(default)]
    pub database_client: DatabaseClient,

    /// Database hostname or IP address
    #[serde(default = "default_hostname")]
    pub hostname: String,

    /// Database port (defaults: PostgreSQL=5432, MySQL=3306, MsSQL=1433)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,

    /// Database user
    pub user: String,

    /// Database password
    pub password: String,

    /// Database name
    pub database: String,

    /// Enable SSL/TLS
    #[serde(default)]
    pub ssl: bool,

    /// Stored procedure command for added results
    /// Use @fieldName to reference query result fields
    /// Example: "CALL add_user(@id, @name, @email)"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub added_command: Option<String>,

    /// Stored procedure command for updated results
    /// Use @fieldName to reference query result fields
    /// Example: "CALL update_user(@id, @name, @email)"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_command: Option<String>,

    /// Stored procedure command for deleted results
    /// Use @fieldName to reference query result fields
    /// Example: "CALL delete_user(@id)"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_command: Option<String>,

    /// Connection pool size
    #[serde(default = "default_pool_size")]
    pub connection_pool_size: usize,

    /// Command timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub command_timeout_ms: u64,

    /// Number of retry attempts on failure
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,
}

impl Default for StoredProcReactionConfig {
    fn default() -> Self {
        Self {
            database_client: DatabaseClient::PostgreSQL,
            hostname: default_hostname(),
            port: None,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            ssl: false,
            added_command: None,
            updated_command: None,
            deleted_command: None,
            connection_pool_size: default_pool_size(),
            command_timeout_ms: default_timeout_ms(),
            retry_attempts: default_retry_attempts(),
        }
    }
}

fn default_hostname() -> String {
    "localhost".to_string()
}

fn default_pool_size() -> usize {
    10
}

fn default_timeout_ms() -> u64 {
    30000
}

fn default_retry_attempts() -> u32 {
    3
}

impl StoredProcReactionConfig {
    /// Get the port for the database, using the default if not specified
    pub fn get_port(&self) -> u16 {
        self.port.unwrap_or_else(|| match self.database_client {
            DatabaseClient::PostgreSQL => 5432,
            DatabaseClient::MySQL => 3306,
            DatabaseClient::MsSQL => 1433,
        })
    }

    /// Validate the configuration
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.user.is_empty() {
            anyhow::bail!("Database user is required");
        }
        if self.database.is_empty() {
            anyhow::bail!("Database name is required");
        }
        if self.added_command.is_none()
            && self.updated_command.is_none()
            && self.deleted_command.is_none()
        {
            anyhow::bail!("At least one command (added, updated, or deleted) must be specified");
        }
        Ok(())
    }
}
