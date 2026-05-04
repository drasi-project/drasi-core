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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellCommand {
  pub executable: String,
  pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellExtension {
    pub envs: Option<HashMap<String, String>>,
    pub enable_stdin: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellQueryConfig {
    pub command: ShellCommand,
    pub query_config: QueryConfig,
}


#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ShellReactionConfig {
    pub max_concurrent: usize,
    pub max_stdin_bytes: usize,
    pub capture_limit: usize,
    pub timeout_s: u64,
    pub kill_on_drop: bool,
    pub env: HashMap<String, String>,
    pub routes: HashMap<String, ShellQueryConfig>,
    pub default_template: Option<ShellQueryConfig>,
}