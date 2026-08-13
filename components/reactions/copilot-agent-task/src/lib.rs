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

#![allow(unexpected_cfgs)]

//! Copilot Agent Task reaction plugin for Drasi.
//!
//! Subscribes to a single WorkGraph "launch query" and, for every row newly
//! **added** to its result set, launches a GitHub Copilot coding-agent task
//! (`POST /agents/repos/{owner}/{repo}/tasks`) and posts a single, pure-JSON
//! `workgraph.execution/v1` issue comment recording the launch. See the
//! crate `README.md` for the full field/config contract, the exact
//! preflight and idempotency model, and integration caveats.
//!
//! ## Quick start
//!
//! ```
//! # fn main() -> anyhow::Result<()> {
//! use drasi_reaction_copilot_agent_task::CopilotAgentTaskReaction;
//!
//! let reaction = CopilotAgentTaskReaction::builder("copilot-launcher")
//!     .with_query("launch-query")
//!     .with_token("ghp_example_do_not_commit_real_tokens")
//!     .with_allowed_repositories(vec!["my-org/my-repo".to_string()])
//!     .with_allowed_profiles(vec!["issue-validator".to_string()])
//!     .with_allowed_models(vec!["gpt-5".to_string(), "gpt-4".to_string()])
//!     .build()?;
//! # let _ = reaction;
//! # Ok(())
//! # }
//! ```

pub mod config;
pub mod descriptor;
pub mod github;
pub mod ids;
pub mod prompt;
pub mod reaction;
pub mod redact;
pub mod row;
pub mod state;

pub use config::{CommentApiConfig, CopilotAgentTaskReactionConfig, AGENT_TASKS_API_VERSION};
pub use reaction::CopilotAgentTaskReaction;
pub use row::LaunchRow;

/// Builder for the Copilot Agent Task reaction.
pub struct CopilotAgentTaskReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: CopilotAgentTaskReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl CopilotAgentTaskReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: CopilotAgentTaskReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    pub fn with_github_api_base_url(mut self, url: impl Into<String>) -> Self {
        self.config.github_api_base_url = url.into();
        self
    }

    pub fn with_github_graphql_url(mut self, url: impl Into<String>) -> Self {
        self.config.github_graphql_url = url.into();
        self
    }

    pub fn with_agent_tasks_api_version(mut self, version: impl Into<String>) -> Self {
        self.config.agent_tasks_api_version = version.into();
        self
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.config.token = token.into();
        self
    }

    pub fn with_allowed_repositories(mut self, repos: Vec<String>) -> Self {
        self.config.allowed_repositories = repos;
        self
    }

    pub fn with_allowed_profiles(mut self, profiles: Vec<String>) -> Self {
        self.config.allowed_profiles = profiles;
        self
    }

    pub fn with_allowed_models(mut self, models: Vec<String>) -> Self {
        self.config.allowed_models = models;
        self
    }

    pub fn with_request_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.request_timeout_ms = timeout_ms;
        self
    }

    pub fn with_comment_api(mut self, comment_api: CommentApiConfig) -> Self {
        self.config.comment_api = comment_api;
        self
    }

    pub fn with_strict_recovery(mut self, strict_recovery: bool) -> Self {
        self.config.strict_recovery = strict_recovery;
        self
    }

    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Replace the full configuration.
    pub fn with_config(mut self, config: CopilotAgentTaskReactionConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> anyhow::Result<CopilotAgentTaskReaction> {
        self.config.validate(&self.queries)?;
        Ok(CopilotAgentTaskReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
        ))
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "copilot-agent-task-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::CopilotAgentTaskReactionDescriptor],
    bootstrap_descriptors = [],
);
