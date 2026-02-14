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
//! This reaction implements gRPC communication for Drasi.
//! Supports fixed and adaptive batching.
//!
//! Fixed batching example:
//!
//! ```rust,ignore
//! use drasi_lib::config::{ReactionConfig, ReactionSpecificConfig};
//! use drasi_reaction_grpc::GrpcReactionConfig;
//! use drasi_reaction_grpc::BatchingConfig;
//! use std::collections::HashMap;
//!
//! let grpc_config = GrpcReactionConfig {
//!     endpoint: "grpc://localhost:50052".to_string(),
//!     timeout_ms: 5000,
//!     max_retries: 3,
//!     connection_retry_attempts: 5,
//!     initial_connection_timeout_ms: 10000,
//!     metadata: HashMap::new(),
//!     batch_config: BatchingConfig::Fixed {
//!         batch_size: 100,
//!         batch_timeout_ms: 1000,
//!     },
//! };
//! let reaction_config = ReactionConfig {
//!     id: "my-grpc-reaction".to_string(),
//!     queries: vec!["query1".to_string()],
//!     auto_start: true,
//!     config: ReactionSpecificConfig::Grpc(grpc_config),
//!     priority_queue_capacity: Some(500),
//! };
//! ```
//! Adaptive batching example:
//!
//! ```rust,ignore
//! use drasi_lib::config::{ReactionConfig, ReactionSpecificConfig};
//! use drasi_reaction_grpc::GrpcReactionConfig;
//! use drasi_reaction_grpc::BatchingConfig;
//! use std::collections::HashMap;
//!
//! let reaction_config = ReactionConfig {
//!     id: "high-throughput".to_string(),
//!     queries: vec!["event-stream".to_string()],
//!     auto_start: true,
//!     config: ReactionSpecificConfig::Grpc(GrpcReactionConfig {
//!         endpoint: "grpc://event-processor:9090".to_string(),
//!         timeout_ms: 15000,
//!         max_retries: 5,
//!         connection_retry_attempts: 5,
//!         initial_connection_timeout_ms: 10000,
//!         metadata: HashMap::new(),
//!         batch_config: BatchingConfig::Adaptive {
//!             min_batch_size: 50,
//!             max_batch_size: 2000,
//!             batch_timeout_ms: 500,
//!         },
//!     }),
//!     priority_queue_capacity: None,
//! };
//! ```
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

fn default_batch_timeout_ms() -> u64 {
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

fn default_adaptive_min_batch_size() -> usize {
    1
}

fn default_adaptive_max_batch_size() -> usize {
    100
}

fn default_adaptive_batch_timeout_ms() -> u64 {
    1000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "strategy", rename_all = "lowercase")]
pub enum BatchingConfig {
    Fixed {
        #[serde(default = "default_batch_size")]
        batch_size: usize,
        #[serde(default = "default_batch_timeout_ms")]
        batch_timeout_ms: u64,
    },
    Adaptive {
        #[serde(default = "default_adaptive_min_batch_size")]
        min_batch_size: usize,
        #[serde(default = "default_adaptive_max_batch_size")]
        max_batch_size: usize,
        #[serde(default = "default_adaptive_batch_timeout_ms")]
        batch_timeout_ms: u64,
    },
}

impl Default for BatchingConfig {
    fn default() -> Self {
        BatchingConfig::Fixed {
            batch_size: default_batch_size(),
            batch_timeout_ms: default_batch_timeout_ms(),
        }
    }
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

    /// Batching configuration (flattened into parent config)
    /// Choose between Fixed or Adaptive batching strategy
    #[serde(flatten)]
    pub batch_config: BatchingConfig,
}

impl Default for GrpcReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: default_grpc_endpoint(),
            timeout_ms: default_timeout_ms(),
            max_retries: default_max_retries(),
            connection_retry_attempts: default_connection_retry_attempts(),
            initial_connection_timeout_ms: default_initial_connection_timeout_ms(),
            metadata: HashMap::new(),
            batch_config: BatchingConfig::Fixed {
                batch_size: default_batch_size(),
                batch_timeout_ms: default_batch_timeout_ms(),
            },
        }
    }
}
