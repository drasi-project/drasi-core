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

pub mod candidate;
pub mod config;
pub mod decision;
pub mod descriptor;
pub mod github_client;
pub mod reaction;
pub mod reconciliation;
pub mod rules;
pub mod state;
pub mod validation;

#[cfg(test)]
mod tests;

pub use config::{StatusTransition, WorkgraphRouterReactionConfig};
use drasi_lib::recovery::ReactionRecoveryPolicy;
pub use reaction::WorkgraphRouterReaction;

/// Builder for [`WorkgraphRouterReaction`].
pub struct WorkgraphRouterReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: WorkgraphRouterReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    recovery_policy: Option<ReactionRecoveryPolicy>,
}

impl WorkgraphRouterReactionBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: WorkgraphRouterReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
            recovery_policy: None,
        }
    }

    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.queries.push(query.into());
        self
    }

    pub fn from_query(mut self, query: impl Into<String>) -> Self {
        self.queries.push(query.into());
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

    pub fn with_policy_id(mut self, value: impl Into<String>) -> Self {
        self.config.policy_id = value.into();
        self
    }

    pub fn with_policy_type(mut self, value: impl Into<String>) -> Self {
        self.config.policy_type = value.into();
        self
    }

    pub fn with_policy_version(mut self, value: impl Into<String>) -> Self {
        self.config.policy_version = value.into();
        self
    }

    pub fn with_allowed_projects(mut self, values: Vec<String>) -> Self {
        self.config.allowed_projects = values;
        self
    }

    pub fn with_allowed_repos(mut self, values: Vec<String>) -> Self {
        self.config.allowed_repos = values;
        self
    }

    pub fn with_allowed_event_types(mut self, values: Vec<String>) -> Self {
        self.config.allowed_event_types = values;
        self
    }

    pub fn with_allowed_status_transitions(mut self, values: Vec<StatusTransition>) -> Self {
        self.config.allowed_status_transitions = values;
        self
    }

    pub fn with_allowed_responsibility_types(mut self, values: Vec<String>) -> Self {
        self.config.allowed_responsibility_types = values;
        self
    }

    pub fn with_allowed_actors(mut self, values: Vec<String>) -> Self {
        self.config.allowed_actors = values;
        self
    }

    pub fn with_trusted_routing_authors(mut self, values: Vec<String>) -> Self {
        self.config.trusted_routing_authors = values;
        self
    }

    pub fn with_trusted_launcher_authors(mut self, values: Vec<String>) -> Self {
        self.config.trusted_launcher_authors = values;
        self
    }

    pub fn with_trusted_agent_authors(mut self, values: Vec<String>) -> Self {
        self.config.trusted_agent_authors = values;
        self
    }

    pub fn with_trusted_router_authors(mut self, values: Vec<String>) -> Self {
        self.config.trusted_router_authors = values;
        self
    }

    pub fn with_github_graphql_url(mut self, value: impl Into<String>) -> Self {
        self.config.github_graphql_url = value.into();
        self
    }

    pub fn with_github_rest_url(mut self, value: impl Into<String>) -> Self {
        self.config.github_rest_url = value.into();
        self
    }

    pub fn with_github_token_env(mut self, value: impl Into<String>) -> Self {
        self.config.github_token_env = value.into();
        self
    }

    pub fn with_project_status_field_name(mut self, value: impl Into<String>) -> Self {
        self.config.project_status_field_name = value.into();
        self
    }

    pub fn with_timeout_secs(mut self, value: u64) -> Self {
        self.config.timeout_secs = value;
        self
    }

    pub fn with_reservation_lease_secs(mut self, value: u64) -> Self {
        self.config.reservation_lease_secs = value;
        self
    }

    pub fn with_strict_recovery(mut self, value: bool) -> Self {
        self.config.strict_recovery = value;
        self
    }

    pub fn with_config(mut self, config: WorkgraphRouterReactionConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> anyhow::Result<WorkgraphRouterReaction> {
        self.config
            .validate(&self.queries, self.priority_queue_capacity)?;
        Ok(WorkgraphRouterReaction::from_builder(
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
    plugin_id = "workgraph-router-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::WorkgraphRouterReactionDescriptor],
    bootstrap_descriptors = [],
);
