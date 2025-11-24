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

//! Configuration types for Application reactions.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Application reaction configuration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApplicationReactionConfig {
    /// Application-specific properties (for now, keep flexible)
    #[serde(flatten)]
    pub properties: HashMap<String, serde_json::Value>,
}
