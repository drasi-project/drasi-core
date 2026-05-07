use drasi_lib::reactions::common::TemplateRouting;
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
pub use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub fn default_max_concurrent() -> usize {
    100
}

pub fn default_max_stdin_bytes() -> usize {
    1024 * 1024 // 1 MB
}

pub fn default_capture_limit() -> usize {
    1024 * 4 // 4 KB
}

pub fn default_timeout_s() -> u64 {
    60 // 60 seconds
}

pub fn default_kill_on_drop() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ShellCommand {
  pub executable: String,
  #[serde(skip_serializing_if = "Vec::is_empty", default)]
  pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct ShellExtension {
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub envs: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellReactionConfig {
    pub max_concurrent: usize,
    pub max_stdin_bytes: usize,
    pub capture_limit: usize,
    pub timeout_s: u64,
    pub kill_on_drop: bool,
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub env: HashMap<String, String>,
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub routes: HashMap<String, QueryConfig<ShellExtension>>,
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub commands: HashMap<String, ShellCommand>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig<ShellExtension>>,
}

impl TemplateRouting<ShellExtension> for ShellReactionConfig {
    fn routes(&self) -> &HashMap<String, QueryConfig<ShellExtension>> {
        &self.routes
    }

    fn default_template(&self) -> Option<&QueryConfig<ShellExtension>> {
        self.default_template.as_ref()
    }
}