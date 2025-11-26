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

//! Configuration types for gRPC source.

use serde::{Deserialize, Serialize};

/// Default timeout in milliseconds
fn default_timeout_ms() -> u64 {
    5000
}

/// gRPC source configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrpcSourceConfig {
    /// gRPC server host
    #[serde(default = "default_host")]
    pub host: String,

    /// gRPC server port
    #[serde(default = "default_port")]
    pub port: u16,

    /// Optional service endpoint
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    50051
}

impl Default for GrpcSourceConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            endpoint: None,
            timeout_ms: default_timeout_ms(),
        }
    }
}
