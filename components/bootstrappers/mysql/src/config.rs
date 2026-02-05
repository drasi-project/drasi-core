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

//! Configuration types for the MySQL bootstrap provider.
//!
//! These types are defined locally to keep this component independent
//! and self-contained, without dependencies on other components.

use serde::{Deserialize, Serialize};

/// Table key configuration for MySQL bootstrap
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

/// MySQL bootstrap provider configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MySqlBootstrapConfig {
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

    /// Tables to bootstrap
    #[serde(default)]
    pub tables: Vec<String>,

    /// Table key configurations
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,
}

fn default_mysql_host() -> String {
    "localhost".to_string()
}

fn default_mysql_port() -> u16 {
    3306
}

pub(crate) fn is_valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

impl MySqlBootstrapConfig {
    /// Validate the configuration and return an error if invalid.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Database name is empty
    /// - User is empty
    /// - Port is 0
    /// - Tables list is empty
    /// - Tables contain invalid identifiers
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
                 Please specify the MySQL user"
            ));
        }

        if self.port == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: port cannot be 0. \
                 Please specify a valid port number (1-65535)"
            ));
        }

        if self.tables.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: tables cannot be empty. \
                 Please configure at least one table to bootstrap"
            ));
        }

        for table in &self.tables {
            if !is_valid_identifier(table) {
                return Err(anyhow::anyhow!(
                    "Validation error: table '{table}' contains invalid characters. \
                     Only letters, numbers, and underscores are allowed"
                ));
            }
        }

        Ok(())
    }
}
