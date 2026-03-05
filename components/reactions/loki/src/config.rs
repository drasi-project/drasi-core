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

use std::collections::HashMap;

pub use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};

#[derive(Clone, PartialEq, Eq)]
pub struct BasicAuth {
    pub username: String,
    pub password: String,
}

impl std::fmt::Debug for BasicAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BasicAuth")
            .field("username", &self.username)
            .field("password", &"***REDACTED***")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct LokiReactionConfig {
    pub endpoint: String,
    pub labels: HashMap<String, String>,
    pub tenant_id: Option<String>,
    pub token: Option<String>,
    pub basic_auth: Option<BasicAuth>,
    pub timeout_ms: u64,
    pub routes: HashMap<String, QueryConfig>,
    pub default_template: Option<QueryConfig>,
}

impl Default for LokiReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:3100".to_string(),
            labels: HashMap::new(),
            tenant_id: None,
            token: None,
            basic_auth: None,
            timeout_ms: 5000,
            routes: HashMap::new(),
            default_template: None,
        }
    }
}
