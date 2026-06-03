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

//! Configuration types for application source.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use drasi_lib::DurabilityConfig;

/// Application source configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplicationSourceConfig {
    /// Application-specific properties (for now, keep flexible)
    #[serde(flatten)]
    pub properties: HashMap<String, serde_json::Value>,

    /// Optional WAL durability configuration.
    ///
    /// When present and enabled, the source persists incoming events to a
    /// local Write-Ahead Log before acknowledging the caller, enabling
    /// crash recovery and replay for persistent queries.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub durability: Option<DurabilityConfig>,
}
