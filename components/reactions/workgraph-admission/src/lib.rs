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

//! WorkGraph admission reaction plugin for Drasi.
//!
//! Admission is the entry point of the minimal WorkGraph workflow. For one
//! eligible Project Item + Issue pair it assigns the `issue-validation`
//! responsibility as a single `WorkGraphEvent/v1` comment and then moves the
//! Project Item to `AwaitingValidation`.
//!
//! ```text
//! admission          -> ResponsibilityAssigned  -> status AwaitingValidation
//! copilot-agent-task -> ExecutionStarted
//! issue-validator    -> CompletedIssueValidation
//! workgraph-router   -> RoutingDecided          -> AwaitingIssueRiskProfiling
//!                                                  or NeedsMoreInformation
//! ```
//!
//! See `README.md` for the row contract, configuration, and recovery model.
//!
//! ## Quick start
//!
//! ```
//! # fn main() -> anyhow::Result<()> {
//! use drasi_reaction_workgraph_admission::{ActorType, WorkgraphAdmissionReaction};
//!
//! let reaction = WorkgraphAdmissionReaction::builder("workgraph-admission")
//!     .with_query("admit-workgraph-items")
//!     .with_allowed_repositories(vec!["drasi-project/drasi-core".to_string()])
//!     .with_allowed_projects(vec!["PVT_project".to_string()])
//!     .with_expected_project_status_field_node_id("PVTSSF_status")
//!     .with_expected_source_status("Triage")
//!     .with_trusted_author_database_id(4021243)
//!     .with_trusted_author_type(ActorType::Bot)
//!     .build()?;
//! # let _ = reaction;
//! # Ok(())
//! # }
//! ```

pub mod candidate;
pub mod config;
pub mod descriptor;
pub mod github;
pub mod reaction;
pub mod state;

pub use candidate::AdmissionCandidate;
pub use config::{WorkgraphAdmissionReactionConfig, ADMISSION_PROFILE, ADMITTED_STATUS};
pub use drasi_workgraph_common::trust::{ActorType, TrustedAuthor};
pub use reaction::WorkgraphAdmissionReaction;
pub use state::AdmissionRecord;

/// Builder for [`WorkgraphAdmissionReaction`].
pub struct WorkgraphAdmissionReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: WorkgraphAdmissionReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
}

impl WorkgraphAdmissionReactionBuilder {
    /// Start building a reaction with the given ID.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: WorkgraphAdmissionReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
        }
    }

    /// Replace the subscribed queries.
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Subscribe to one query.
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.queries.push(query.into());
        self
    }

    /// Override the GitHub REST base URL.
    pub fn with_github_rest_url(mut self, value: impl Into<String>) -> Self {
        self.config.github_rest_url = value.into();
        self
    }

    /// Override the GitHub GraphQL endpoint.
    pub fn with_github_graphql_url(mut self, value: impl Into<String>) -> Self {
        self.config.github_graphql_url = value.into();
        self
    }

    /// Override the environment variable holding the GitHub token.
    pub fn with_github_token_env(mut self, value: impl Into<String>) -> Self {
        self.config.github_token_env = value.into();
        self
    }

    /// Set the repository allowlist.
    pub fn with_allowed_repositories(mut self, values: Vec<String>) -> Self {
        self.config.allowed_repositories = values;
        self
    }

    /// Set the Project allowlist.
    pub fn with_allowed_projects(mut self, values: Vec<String>) -> Self {
        self.config.allowed_projects = values;
        self
    }

    /// Override the Project status field name.
    pub fn with_project_status_field_name(mut self, value: impl Into<String>) -> Self {
        self.config.project_status_field_name = value.into();
        self
    }

    /// Pin the Project status field node ID.
    pub fn with_expected_project_status_field_node_id(mut self, value: impl Into<String>) -> Self {
        self.config.expected_project_status_field_node_id = value.into();
        self
    }

    /// Set the status an eligible Project Item must hold.
    pub fn with_expected_source_status(mut self, value: impl Into<String>) -> Self {
        self.config.expected_source_status = value.into();
        self
    }

    /// Override the assigned agent profile.
    pub fn with_agent_profile(mut self, value: impl Into<String>) -> Self {
        self.config.agent_profile = value.into();
        self
    }

    /// Override the ref the profile blob is pinned from.
    pub fn with_profile_base_ref(mut self, value: impl Into<String>) -> Self {
        self.config.profile_base_ref = value.into();
        self
    }

    /// Set the numeric GitHub database ID of the identity this reaction posts
    /// as. Required: it is one half of the trust key.
    pub fn with_trusted_author_database_id(mut self, value: u64) -> Self {
        self.config.trusted_author_database_id = value;
        self
    }

    /// Set the actor type of the identity this reaction posts as (the other
    /// half of the trust key).
    pub fn with_trusted_author_type(mut self, value: ActorType) -> Self {
        self.config.trusted_author_type = value;
        self
    }

    /// Override the per-request timeout.
    pub fn with_timeout_secs(mut self, value: u64) -> Self {
        self.config.timeout_secs = value;
        self
    }

    /// Override strict recovery (must remain `true`).
    pub fn with_strict_recovery(mut self, value: bool) -> Self {
        self.config.strict_recovery = value;
        self
    }

    /// Override the reaction input queue capacity.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Control auto-start behavior.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Replace the whole configuration.
    pub fn with_config(mut self, config: WorkgraphAdmissionReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Validate the configuration and build the reaction.
    pub fn build(self) -> anyhow::Result<WorkgraphAdmissionReaction> {
        self.config.validate(&self.queries)?;
        Ok(WorkgraphAdmissionReaction::from_builder(
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
    plugin_id = "workgraph-admission-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::WorkgraphAdmissionReactionDescriptor],
    bootstrap_descriptors = [],
);
