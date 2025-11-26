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

//! Configuration types for gRPC adaptive reaction.

use drasi_lib::reactions::common::AdaptiveBatchConfig;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

fn default_grpc_endpoint() -> String {
    "grpc://localhost:50052".to_string()
}

fn default_timeout_ms() -> u64 {
    5000
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

/// gRPC Adaptive reaction configuration with adaptive batching
///
/// This reaction extends the standard gRPC reaction with intelligent, throughput-based
/// batching that automatically adjusts batch sizes and wait times based on real-time
/// traffic patterns.
///
/// # Key Features
///
/// - **Dynamic Batching**: Adjusts batch size (min to max) based on throughput
/// - **Lazy Connection**: Connects only when first batch is ready
/// - **Automatic Retry**: Exponential backoff with configurable retries
/// - **Traffic Classification**: Five levels (Idle/Low/Medium/High/Burst)
///
/// # Example
///
/// ```rust
/// use drasi_lib::config::{ReactionConfig, ReactionSpecificConfig};
/// use drasi_lib::reactions::grpc_adaptive::GrpcAdaptiveReactionConfig;
/// use drasi_lib::reactions::common::AdaptiveBatchConfig;
/// use std::collections::HashMap;
///
/// let config = ReactionConfig {
///     id: "high-throughput".to_string(),
///     queries: vec!["event-stream".to_string()],
///     auto_start: true,
///     config: ReactionSpecificConfig::GrpcAdaptive(GrpcAdaptiveReactionConfig {
///         endpoint: "grpc://event-processor:9090".to_string(),
///         timeout_ms: 15000,
///         max_retries: 5,
///         connection_retry_attempts: 5,
///         initial_connection_timeout_ms: 10000,
///         metadata: HashMap::new(),
///         adaptive: AdaptiveBatchConfig {
///             adaptive_min_batch_size: 50,
///             adaptive_max_batch_size: 2000,
///             adaptive_window_size: 100,  // 10 seconds
///             adaptive_batch_timeout_ms: 500,
///         },
///     }),
///     priority_queue_capacity: None,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GrpcAdaptiveReactionConfig {
    /// gRPC server endpoint URL (e.g., "grpc://localhost:50052")
    #[serde(default = "default_grpc_endpoint")]
    pub endpoint: String,

    /// Request timeout in milliseconds
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,

    /// Maximum retries for failed batch requests
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,

    /// Number of connection retry attempts before giving up
    #[serde(default = "default_connection_retry_attempts")]
    pub connection_retry_attempts: u32,

    /// Initial connection timeout in milliseconds (used for lazy connection)
    #[serde(default = "default_initial_connection_timeout_ms")]
    pub initial_connection_timeout_ms: u64,

    /// gRPC metadata headers to include in all requests
    #[serde(default)]
    pub metadata: HashMap<String, String>,

    /// Adaptive batching configuration (flattened into parent config)
    #[serde(flatten)]
    pub adaptive: AdaptiveBatchConfig,
}

impl Default for GrpcAdaptiveReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: default_grpc_endpoint(),
            timeout_ms: default_timeout_ms(),
            max_retries: default_max_retries(),
            connection_retry_attempts: default_connection_retry_attempts(),
            initial_connection_timeout_ms: default_initial_connection_timeout_ms(),
            metadata: HashMap::new(),
            adaptive: AdaptiveBatchConfig::default(),
        }
    }
}
