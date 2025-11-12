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

pub mod config;
pub use config::HttpSourceConfig;

use serde::{Deserialize, Serialize};

mod adaptive;
mod direct_format;

// Re-export the adaptive HTTP source as the main HTTP source
pub use adaptive::{AdaptiveHttpSource as HttpSource, BatchEventRequest};

// Export direct format types and conversion
pub use direct_format::{
    convert_direct_to_source_change,
    // Alias for backward compatibility
    convert_direct_to_source_change as convert_json_to_source_change,
    DirectElement,
    DirectSourceChange,
};

/// Response for event submission
#[derive(Debug, Serialize, Deserialize)]
pub struct EventResponse {
    pub success: bool,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[cfg(test)]
mod http_test;
