// Copyright 2026 The Drasi Authors.
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

//! Configuration types for dashboard reactions.

use serde::{Deserialize, Serialize};

fn default_dashboard_host() -> String {
    "0.0.0.0".to_string()
}

fn default_dashboard_port() -> u16 {
    3000
}

fn default_heartbeat_interval_ms() -> u64 {
    30_000
}

/// Dashboard reaction configuration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DashboardReactionConfig {
    /// Host to bind the dashboard server.
    #[serde(default = "default_dashboard_host")]
    pub host: String,

    /// Port to bind the dashboard server.
    #[serde(default = "default_dashboard_port")]
    pub port: u16,

    /// WebSocket heartbeat interval in milliseconds.
    #[serde(default = "default_heartbeat_interval_ms")]
    pub heartbeat_interval_ms: u64,

    /// Optional base URL for the DrasiLib results API (e.g., "http://localhost:8080").
    /// When set, the dashboard proxies initial query data from this API
    /// so widgets populate immediately with bootstrap data.
    #[serde(default)]
    pub results_api_url: Option<String>,
}

impl Default for DashboardReactionConfig {
    fn default() -> Self {
        Self {
            host: default_dashboard_host(),
            port: default_dashboard_port(),
            heartbeat_interval_ms: default_heartbeat_interval_ms(),
            results_api_url: None,
        }
    }
}
