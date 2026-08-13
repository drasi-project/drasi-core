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

//! GitHub ProjectV2 item refresh reaction for Drasi.

pub mod config;
pub mod descriptor;
pub(crate) mod destination;
pub(crate) mod graphql;
pub(crate) mod models;
pub(crate) mod processing;
pub(crate) mod reaction;
pub(crate) mod state_store;

#[cfg(test)]
mod tests;

use drasi_lib::recovery::ReactionRecoveryPolicy;

pub use config::GitHubProjectItemRefreshConfig;
pub use reaction::GitHubProjectItemRefreshReaction;

/// Builder for the GitHub project-item refresh reaction.
pub struct GitHubProjectItemRefreshBuilder {
    id: String,
    queries: Vec<String>,
    config: GitHubProjectItemRefreshConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    recovery_policy: Option<ReactionRecoveryPolicy>,
}

impl GitHubProjectItemRefreshBuilder {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: GitHubProjectItemRefreshConfig::default(),
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

    pub fn with_github_token(mut self, token: impl Into<String>) -> Self {
        self.config.github_token = token.into();
        self
    }

    pub fn with_graphql_url(mut self, url: impl Into<String>) -> Self {
        self.config.graphql_url = url.into();
        self
    }

    pub fn with_graphql_header(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.config
            .graphql_headers
            .insert(name.into(), value.into());
        self
    }

    pub fn with_allowlisted_project_ids(mut self, project_ids: Vec<String>) -> Self {
        self.config.allowlisted_project_ids = project_ids;
        self
    }

    pub fn with_status_field_name(mut self, status_field_name: impl Into<String>) -> Self {
        self.config.status_field_name = status_field_name.into();
        self
    }

    pub fn with_destination_event_url(mut self, url: impl Into<String>) -> Self {
        self.config.destination_event_url = url.into();
        self
    }

    pub fn with_destination_bearer_secret(mut self, bearer_secret: impl Into<String>) -> Self {
        self.config.destination_bearer_secret = Some(bearer_secret.into());
        self
    }

    pub fn with_request_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.request_timeout_ms = timeout_ms;
        self
    }

    pub fn with_delivery_record_ttl_secs(mut self, ttl_secs: u64) -> Self {
        self.config.delivery_record_ttl_secs = ttl_secs;
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

    pub fn with_config(mut self, config: GitHubProjectItemRefreshConfig) -> Self {
        self.config = config;
        self
    }

    pub fn build(self) -> anyhow::Result<GitHubProjectItemRefreshReaction> {
        self.config
            .validate(&self.queries, self.priority_queue_capacity)?;
        Ok(GitHubProjectItemRefreshReaction::from_builder(
            self.id,
            self.queries,
            self.config,
            self.priority_queue_capacity,
            self.auto_start,
            self.recovery_policy,
        ))
    }
}

/// Dynamic plugin entry point.
#[cfg(feature = "dynamic-plugin")]
drasi_plugin_sdk::export_plugin!(
    plugin_id = "github-project-item-refresh-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::GitHubProjectItemRefreshDescriptor],
    bootstrap_descriptors = [],
);
