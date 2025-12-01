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

//! Configuration types for Profiler reactions.

use serde::{Deserialize, Serialize};

fn default_profiler_window_size() -> usize {
    1000
}

fn default_report_interval_secs() -> u64 {
    60
}

/// Profiler reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProfilerReactionConfig {
    /// Window size for profiling statistics
    #[serde(default = "default_profiler_window_size")]
    pub window_size: usize,

    /// Report interval in seconds
    #[serde(default = "default_report_interval_secs")]
    pub report_interval_secs: u64,
}

impl Default for ProfilerReactionConfig {
    fn default() -> Self {
        Self {
            window_size: default_profiler_window_size(),
            report_interval_secs: default_report_interval_secs(),
        }
    }
}
