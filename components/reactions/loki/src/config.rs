// Copyright 2026 The Drasi Authors.
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

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};

fn default_endpoint() -> String {
    "http://localhost:3100".to_string()
}

fn default_timeout_ms() -> u64 {
    5000
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BasicAuth {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LokiReactionConfig {
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    #[serde(default)]
    pub labels: HashMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tenant_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub basic_auth: Option<BasicAuth>,
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub routes: HashMap<String, QueryConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
}

impl Default for LokiReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            labels: HashMap::new(),
            tenant_id: None,
            token: None,
            basic_auth: None,
            timeout_ms: default_timeout_ms(),
            routes: HashMap::new(),
            default_template: None,
        }
    }
}
