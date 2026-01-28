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

//! Configuration types for gRPC reactions.

use drasi_lib::reactions::common::AdaptiveBatchConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_grpc_endpoint() -> String {
    "grpc://localhost:50052".to_string()
}

fn default_timeout_ms() -> u64 {
    5000
}

fn default_batch_size() -> usize {
    100
}

fn default_batch_flush_timeout_ms() -> u64 {
    1000
}

fn default_max_retries() -> u32 {
    3
}

fn default_connection_retry_attempts() -> u32 {
    5
}

fn default_initial_connection_timeout_ms() -> u64 {
    10000
}

fn default_adaptive_enable() -> bool {
    false
}

/// gRPC reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrpcReactionConfig {
    /// gRPC server URL
    #[serde(default = "default_grpc_endpoint")]
    pub endpoint: String,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Batch size for bundling events
    #[serde(default = "default_batch_size")]
    pub batch_size: usize,

    /// Batch flush timeout in milliseconds
    #[serde(default = "default_batch_flush_timeout_ms")]
    pub batch_flush_timeout_ms: u64,

    /// Maximum retries for failed requests
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Connection retry attempts
    #[serde(default = "default_connection_retry_attempts")]
    pub connection_retry_attempts: u32,

    /// Initial connection timeout in milliseconds
    #[serde(default = "default_initial_connection_timeout_ms")]
    pub initial_connection_timeout_ms: u64,

    /// Metadata headers to include in requests
    #[serde(default)]
    pub metadata: HashMap<String, String>,

    /// Adaptive batching enable option
    #[serde(default)]
    pub adaptive_enable: bool,

    /// Adaptive batching configuration (flattened into parent config)
    #[serde(flatten)]
    pub adaptive: AdaptiveBatchConfig,
}

impl Default for GrpcReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: default_grpc_endpoint(),
            timeout_ms: default_timeout_ms(),
            batch_size: default_batch_size(),
            batch_flush_timeout_ms: default_batch_flush_timeout_ms(),
            max_retries: default_max_retries(),
            connection_retry_attempts: default_connection_retry_attempts(),
            initial_connection_timeout_ms: default_initial_connection_timeout_ms(),
            metadata: HashMap::new(),
            adaptive_enable: default_adaptive_enable(),
            adaptive: AdaptiveBatchConfig::default(),
        }
    }
}
