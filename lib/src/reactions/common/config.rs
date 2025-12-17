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

//! Common configuration types shared across reactions.

use serde::{Deserialize, Serialize};

fn default_adaptive_min_batch_size() -> usize {
    1
}

fn default_adaptive_max_batch_size() -> usize {
    100
}

fn default_adaptive_window_size() -> usize {
    10
}

fn default_adaptive_batch_timeout_ms() -> u64 {
    1000
}

/// Adaptive batching configuration shared by adaptive reactions
///
/// This configuration is used by both HTTP Adaptive and gRPC Adaptive reactions to
/// dynamically adjust batch size and timing based on throughput patterns.
///
/// # Adaptive Algorithm
///
/// The adaptive batcher monitors throughput over a configurable window and adjusts
/// batch size and wait time based on traffic level:
///
/// | Throughput Level | Messages/Sec | Batch Size | Wait Time |
/// |-----------------|--------------|------------|-----------|
/// | Idle           | < 1          | min_batch_size | 1ms |
/// | Low            | 1-100        | 2 × min | 1ms |
/// | Medium         | 100-1K       | 25% of max | 10ms |
/// | High           | 1K-10K       | 50% of max | 25ms |
/// | Burst          | > 10K        | max_batch_size | 50ms |
///
/// # Example
///
/// ```rust
/// use drasi_lib::reactions::common::AdaptiveBatchConfig;
///
/// // High-throughput configuration
/// let config = AdaptiveBatchConfig {
///     adaptive_min_batch_size: 50,
///     adaptive_max_batch_size: 2000,
///     adaptive_window_size: 100,  // 10 seconds
///     adaptive_batch_timeout_ms: 500,
/// };
///
/// // Low-latency configuration
/// let config = AdaptiveBatchConfig {
///     adaptive_min_batch_size: 1,
///     adaptive_max_batch_size: 50,
///     adaptive_window_size: 30,  // 3 seconds
///     adaptive_batch_timeout_ms: 10,
/// };
/// ```
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AdaptiveBatchConfig {
    /// Minimum batch size (events per batch) used during idle/low traffic
    #[serde(default = "default_adaptive_min_batch_size")]
    pub adaptive_min_batch_size: usize,

    /// Maximum batch size (events per batch) used during burst traffic
    #[serde(default = "default_adaptive_max_batch_size")]
    pub adaptive_max_batch_size: usize,

    /// Window size for throughput monitoring in 100ms units (range: 1-255)
    ///
    /// For example:
    /// - 10 = 1 second
    /// - 50 = 5 seconds
    /// - 100 = 10 seconds
    ///
    /// Larger windows provide more stable adaptation but respond slower to traffic changes.
    #[serde(default = "default_adaptive_window_size")]
    pub adaptive_window_size: usize,

    /// Maximum time to wait before flushing a partial batch (milliseconds)
    ///
    /// This timeout ensures results are delivered even when batch size is not reached.
    #[serde(default = "default_adaptive_batch_timeout_ms")]
    pub adaptive_batch_timeout_ms: u64,
}

impl Default for AdaptiveBatchConfig {
    fn default() -> Self {
        Self {
            adaptive_min_batch_size: default_adaptive_min_batch_size(),
            adaptive_max_batch_size: default_adaptive_max_batch_size(),
            adaptive_window_size: default_adaptive_window_size(),
            adaptive_batch_timeout_ms: default_adaptive_batch_timeout_ms(),
        }
    }
}
