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

//! A2A JSON-RPC reaction plugin for Drasi.
//!
//! Forwards continuous query diffs to an A2A-compatible JSON-RPC 2.0
//! endpoint via `SendMessage`/`CancelTask`, tracking task activation so
//! `UPDATE`/`DELETE` follow up on or cancel the right task. See
//! [`A2AReactionConfig`] and the crate `README.md` for configuration and
//! wire format.
//!
//! ## Quick start
//!
//! Allocate the reaction's UUID independently of the validated query name.
//! Use `with_query_by_id` instead when selecting an existing query's `ComponentId`.
//!
//! ```
//! # fn main() -> anyhow::Result<()> {
//! use drasi_lib::{ComponentId, ComponentName};
//! use drasi_reaction_a2a::A2AReaction;
//!
//! let reaction = A2AReaction::builder(ComponentId::generate())
//!     .with_query_by_name(ComponentName::try_new("overdue-invoices")?)
//!     .with_endpoint("https://agent.example.com/")
//!     .with_result_key_fields(["invoiceId"])
//!     .build()?;
//! # let _ = reaction;
//! # Ok(())
//! # }
//! ```

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

/// Builder for [`A2AReaction`].
///
/// `endpoint` and `resultKeyFields` are required. See the crate `README.md`
/// for defaults and wire behavior.
pub struct A2AReactionBuilder {
    id: drasi_lib::ComponentId,
    queries: Vec<drasi_lib::config::ComponentReference>,
    config: A2AReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    recovery_policy: Option<ReactionRecoveryPolicy>,
}

impl A2AReactionBuilder {
    pub fn new(id: drasi_lib::ComponentId) -> Self {
        Self {
            id: id.clone(),
            queries: Vec::new(),
            config: A2AReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
            recovery_policy: None,
        }
    }

    /// Set explicit name or ID references for every query input.
    pub fn with_query_inputs(mut self, inputs: Vec<drasi_lib::config::ComponentReference>) -> Self {
        self.queries = inputs;
        self
    }

    /// Set all query inputs by their current names.
    pub fn with_queries_by_name(self, names: Vec<drasi_lib::ComponentName>) -> Self {
        self.with_query_inputs(
            names
                .into_iter()
                .map(drasi_lib::config::ComponentReference::by_name)
                .collect(),
        )
    }

    /// Set all query inputs by their immutable IDs.
    pub fn with_queries_by_id(self, ids: Vec<drasi_lib::ComponentId>) -> Self {
        self.with_query_inputs(
            ids.into_iter()
                .map(drasi_lib::config::ComponentReference::by_id)
                .collect(),
        )
    }

    /// Add a query input by its current name.
    pub fn with_query_by_name(mut self, query_name: drasi_lib::ComponentName) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_name(query_name));
        self
    }

    /// Add a query input by its immutable ID.
    pub fn with_query_by_id(mut self, query_id: drasi_lib::ComponentId) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_id(query_id));
        self
    }

    /// Add a query input by its current name.
    pub fn from_query_by_name(mut self, query_name: drasi_lib::ComponentName) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_name(query_name));
        self
    }

    /// Add a query input by its immutable ID.
    pub fn from_query_by_id(mut self, query_id: drasi_lib::ComponentId) -> Self {
        self.queries
            .push(drasi_lib::config::ComponentReference::by_id(query_id));
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

    /// Fields used to correlate diffs with an A2A task. Required.
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

    /// What to do when a follow-up arrives after the remote task is
    /// terminal. Defaults to [`TerminalUpdatePolicy::Replace`].
    pub fn with_terminal_update_policy(mut self, policy: TerminalUpdatePolicy) -> Self {
        self.config.terminal_update_policy = policy;
        self
    }

    /// When true (default), `SendMessage` uses `returnImmediately` so the
    /// reaction does not wait for the agent to finish.
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
        self.config.validate(
            &drasi_lib::reactions::common::input_aliases(&self.queries),
            self.priority_queue_capacity,
        )?;
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
