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

//! Configuration for the PostgreSQL replication source.
//!
//! This source monitors PostgreSQL databases using logical replication to stream
//! data changes as they occur.

use serde::{Deserialize, Serialize};

// =============================================================================
// SSL Configuration
// =============================================================================

/// SSL mode for PostgreSQL connections
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum SslMode {
    /// Disable SSL encryption
    Disable,
    /// Prefer SSL but allow unencrypted connections
    #[default]
    Prefer,
    /// Require SSL encryption
    Require,
}


impl std::fmt::Display for SslMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Disable => write!(f, "disable"),
            Self::Prefer => write!(f, "prefer"),
            Self::Require => write!(f, "require"),
        }
    }
}

// =============================================================================
// Database Table Configuration
// =============================================================================

/// Table key configuration for PostgreSQL sources
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TableKeyConfig {
    pub table: String,
    pub key_columns: Vec<String>,
}

/// PostgreSQL replication source configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PostgresSourceConfig {
    /// PostgreSQL host
    #[serde(default = "default_postgres_host")]
    pub host: String,

    /// PostgreSQL port
    #[serde(default = "default_postgres_port")]
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

    /// Replication slot name
    #[serde(default = "default_slot_name")]
    pub slot_name: String,

    /// Publication name
    #[serde(default = "default_publication_name")]
    pub publication_name: String,

    /// SSL mode
    #[serde(default)]
    pub ssl_mode: SslMode,

    /// Table key configurations
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,
}

fn default_postgres_host() -> String {
    "localhost".to_string()
}

fn default_postgres_port() -> u16 {
    5432
}

fn default_slot_name() -> String {
    "drasi_slot".to_string()
}

fn default_publication_name() -> String {
    "drasi_publication".to_string()
}

impl PostgresSourceConfig {
    /// Validate the configuration and return an error if invalid.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Database name is empty
    /// - User is empty
    /// - Port is 0
    /// - Slot name is empty
    /// - Publication name is empty
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.database.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: database cannot be empty. \
                 Please specify the PostgreSQL database name"
            ));
        }

        if self.user.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: user cannot be empty. \
                 Please specify the PostgreSQL user for replication"
            ));
        }

        if self.port == 0 {
            return Err(anyhow::anyhow!(
                "Validation error: port cannot be 0. \
                 Please specify a valid port number (1-65535)"
            ));
        }

        if self.slot_name.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: slot_name cannot be empty. \
                 Please specify a replication slot name"
            ));
        }

        if self.publication_name.is_empty() {
            return Err(anyhow::anyhow!(
                "Validation error: publication_name cannot be empty. \
                 Please specify a publication name"
            ));
        }

        Ok(())
    }
}
