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

//! Configuration for the PostgreSQL Stored Procedure reaction.

use serde::{Deserialize, Serialize};

/// Configuration for the PostgreSQL Stored Procedure reaction
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostgresStoredProcReactionConfig {
    /// Database hostname or IP address
    #[serde(default = "default_hostname")]
    pub hostname: String,

    /// Database port (default: 5432)
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

    /// Command timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub command_timeout_ms: u64,

    /// Number of retry attempts on failure
    #[serde(default = "default_retry_attempts")]
    pub retry_attempts: u32,
}

impl Default for PostgresStoredProcReactionConfig {
    fn default() -> Self {
        Self {
            hostname: default_hostname(),
            port: None,
            user: String::new(),
            password: String::new(),
            database: String::new(),
            ssl: false,
            added_command: None,
            updated_command: None,
            deleted_command: None,
            command_timeout_ms: default_timeout_ms(),
            retry_attempts: default_retry_attempts(),
        }
    }
}

fn default_hostname() -> String {
    "localhost".to_string()
}

fn default_timeout_ms() -> u64 {
    30000
}

fn default_retry_attempts() -> u32 {
    3
}

impl PostgresStoredProcReactionConfig {
    /// Get the port for the database, using the default if not specified
    pub fn get_port(&self) -> u16 {
        self.port.unwrap_or(5432)
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
