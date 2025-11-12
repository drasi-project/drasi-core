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

//! Configuration for the HTTP source.
//!
//! The HTTP source receives data changes via HTTP endpoints.

use serde::{Deserialize, Serialize};

use crate::config::common::TableKeyConfig;

/// HTTP source configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpSourceConfig {
    /// HTTP host
    pub host: String,

    /// HTTP port
    pub port: u16,

    /// Optional endpoint path
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Tables to monitor (for adaptive HTTP)
    #[serde(default)]
    pub tables: Vec<String>,

    /// Table key configurations (for adaptive HTTP)
    #[serde(default)]
    pub table_keys: Vec<TableKeyConfig>,

    /// PostgreSQL database (for adaptive HTTP bootstrap)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,

    /// PostgreSQL user (for adaptive HTTP bootstrap)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,

    /// PostgreSQL password (for adaptive HTTP bootstrap)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,

    /// Adaptive batching: maximum batch size
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_batch_size: Option<usize>,

    /// Adaptive batching: minimum batch size
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_batch_size: Option<usize>,

    /// Adaptive batching: maximum wait time in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_max_wait_ms: Option<u64>,

    /// Adaptive batching: minimum wait time in milliseconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_min_wait_ms: Option<u64>,

    /// Adaptive batching: throughput window in seconds
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_window_secs: Option<u64>,

    /// Whether adaptive batching is enabled
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_enabled: Option<bool>,
}

fn default_timeout_ms() -> u64 {
    10000
}
