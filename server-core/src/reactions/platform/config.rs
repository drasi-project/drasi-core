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

//! Configuration types for Platform reactions (Redis Streams publishing).

use serde::{Deserialize, Serialize};

fn default_batch_size() -> usize {
    100
}

fn default_batch_wait_ms() -> u64 {
    100
}

/// Platform reaction configuration (Redis Streams publishing)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PlatformReactionConfig {
    /// Redis connection URL
    pub redis_url: String,

    /// PubSub name/topic
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pubsub_name: Option<String>,

    /// Source name
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_name: Option<String>,

    /// Maximum stream length
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_stream_length: Option<usize>,

    /// Whether to emit control events
    #[serde(default)]
    pub emit_control_events: bool,

    /// Whether batching is enabled
    #[serde(default)]
    pub batch_enabled: bool,

    /// Maximum batch size
    #[serde(default = "default_batch_size")]
    pub batch_max_size: usize,

    /// Maximum batch wait time in milliseconds
    #[serde(default = "default_batch_wait_ms")]
    pub batch_max_wait_ms: u64,
}
