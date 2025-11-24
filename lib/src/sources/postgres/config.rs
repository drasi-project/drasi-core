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

use crate::config::common::{SslMode, TableKeyConfig};

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
