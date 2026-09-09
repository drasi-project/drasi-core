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

pub mod a2a;
pub mod activation;
pub mod client;
pub mod config;
pub mod descriptor;
pub(crate) mod process;

#[cfg(test)]
mod tests;

use drasi_lib::recovery::ReactionRecoveryPolicy;

pub use a2a::A2AReaction;
pub use activation::{
    next_action, Action, Activation, MessageId, Operation, ResultKey, TerminalUpdatePolicy,
};
pub use config::A2AReactionConfig;

pub struct A2AReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: A2AReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    recovery_policy: Option<ReactionRecoveryPolicy>,
}

impl A2AReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: A2AReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
            recovery_policy: None,
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

    pub fn from_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.config.endpoint = endpoint.into();
        self
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.config.token = Some(token.into());
        self
    }

    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.timeout_ms = timeout_ms;
        self
    }

    pub fn with_result_key_fields<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.result_key_fields = fields.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_instruction_template(mut self, template: impl Into<String>) -> Self {
        self.config.instruction_template = Some(template.into());
        self
    }

    pub fn with_terminal_update_policy(mut self, policy: TerminalUpdatePolicy) -> Self {
        self.config.terminal_update_policy = policy;
        self
    }

    pub fn with_return_immediately(mut self, return_immediately: bool) -> Self {
        self.config.return_immediately = return_immediately;
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

    pub fn with_recovery_policy(mut self, policy: ReactionRecoveryPolicy) -> Self {
        self.recovery_policy = Some(policy);
        self
    }

    pub fn with_config(mut self, config: A2AReactionConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> anyhow::Result<A2AReaction> {
        self.config
            .validate(&self.queries, self.priority_queue_capacity)?;
        Ok(A2AReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
            self.recovery_policy,
        ))
    }
}

#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "a2a-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::A2AReactionDescriptor],
    bootstrap_descriptors = [],
);
