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

mod config;
pub mod descriptor;
mod shell;
mod executor;
mod metric;

use std::collections::HashMap;
use config::QueryConfig;

use crate::config::ShellQueryConfig;
use crate::config::ShellReactionConfig;
use crate::shell::ShellReaction;
use crate::config::{default_capture_limit, default_kill_on_drop, default_max_concurrent, default_max_stdin_bytes, default_timeout_s};


pub struct ShellReactionBuilder {
    id: String,
    queries: Vec<String>,
    routes: HashMap<String, ShellQueryConfig>,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    default_template: Option<ShellQueryConfig>,

    max_concurrent: usize,
    max_stdin_bytes: usize,
    capture_limit: usize,
    timeout_s: u64,
    kill_on_drop: bool,
    env: HashMap<String, String>, // global env vars for all commands
}

impl ShellReactionBuilder {
    /// Create a new ShellReactionBuilder with the given id.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            routes: HashMap::new(),
            priority_queue_capacity: None,
            auto_start: true,
            default_template: None,

            max_concurrent: config::default_max_concurrent(),
            max_stdin_bytes: config::default_max_stdin_bytes(),
            capture_limit: config::default_capture_limit(),
            timeout_s: config::default_timeout_s(),
            kill_on_drop: config::default_kill_on_drop(),
            env: HashMap::new(),
        }
    }

    /// Set the query IDs to subscibe to
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Add a query ID to subscribe to
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Add a route configuration for a specific query
    pub fn with_route(mut self, query_id: impl Into<String>, config: ShellQueryConfig) -> Self {
        self.routes.insert(query_id.into(), config);
        self
    }

    /// Set the priority queue capacity
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Set whether to auto start the reaction
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Set the default template to use when a query doesn't have a specific route config
    pub fn with_default_template(mut self, template: ShellQueryConfig) -> Self {
        self.default_template = Some(template);
        self
    }

    /// Set the maximum number of concurrent commands to run
    pub fn with_max_concurrent(mut self, max_concurrent: usize) -> Self {
        self.max_concurrent = max_concurrent;
        self
    }

    /// Set the maximum number of bytes to read from stdin
    pub fn with_max_stdin_bytes(mut self, max_stdin_bytes: usize) -> Self {
        self.max_stdin_bytes = max_stdin_bytes;
        self
    }

    /// Set the maximum number of bytes to capture from stdout/stderr
    pub fn with_capture_limit(mut self, capture_limit: usize) -> Self {
        self.capture_limit = capture_limit;
        self
    }

    /// Set the command timeout in seconds
    pub fn with_timeout_s(mut self, timeout_s: u64) -> Self {
        self.timeout_s = timeout_s;
        self
    }

    /// Set whether to kill the command on drop
    pub fn with_kill_on_drop(mut self, kill_on_drop: bool) -> Self {
        self.kill_on_drop = kill_on_drop;
        self
    }

    /// Add a global environment variable for all commands
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Add multiple global environment variables for all commands
    pub fn with_envs(mut self, envs: HashMap<String, String>) -> Self {
        self.env = envs;
        self
    }

    /// Set the full configuration at once
    pub fn with_config(mut self, config: ShellReactionConfig) -> Self {
        self.max_concurrent = self.max_concurrent;
        self.max_stdin_bytes = self.max_stdin_bytes;
        self.capture_limit = self.capture_limit;
        self.timeout_s = self.timeout_s;
        self.kill_on_drop = self.kill_on_drop;
        self.env = config.env;
        self.routes = config.routes;
        self.default_template = config.default_template;
        self
    }

    /// Build the ShellReaction
    pub fn build(self) -> anyhow::Result<ShellReaction> {
        let config = ShellReactionConfig {
            max_concurrent: self.max_concurrent,
            max_stdin_bytes: self.max_stdin_bytes,
            capture_limit: self.capture_limit,
            timeout_s: self.timeout_s,
            kill_on_drop: self.kill_on_drop,
            env: self.env,
            routes: self.routes,
            default_template: self.default_template,
        };

        ShellReaction::from_builder(
            self.id,
            self.queries,
            config,
            self.priority_queue_capacity,
            self.auto_start,
        )
    }
}
