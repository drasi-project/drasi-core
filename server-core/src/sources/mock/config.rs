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

//! Configuration for the mock source.
//!
//! The mock source generates synthetic data for testing and development purposes.

use serde::{Deserialize, Serialize};

/// Mock source configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MockSourceConfig {
    /// Type of data to generate: "counter", "sensor", or "generic"
    #[serde(default = "default_data_type")]
    pub data_type: String,

    /// Interval between data generation in milliseconds
    #[serde(default = "default_interval_ms")]
    pub interval_ms: u64,
}

fn default_data_type() -> String {
    "generic".to_string()
}

fn default_interval_ms() -> u64 {
    5000
}

impl Default for MockSourceConfig {
    fn default() -> Self {
        Self {
            data_type: default_data_type(),
            interval_ms: default_interval_ms(),
        }
    }
}
