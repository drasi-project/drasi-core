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

//! Configuration types for log reaction.

use drasi_lib::config::common::LogLevel;
use serde::{Deserialize, Serialize};

/// Log reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LogReactionConfig {
    /// Log level
    #[serde(default)]
    pub log_level: LogLevel,
}

impl Default for LogReactionConfig {
    fn default() -> Self {
        Self {
            log_level: LogLevel::default(),
        }
    }
}
