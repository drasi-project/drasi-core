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

//! Agent Host Protocol (AHP) reaction plugin for Drasi.
//!
//! Turns continuous-query changes into agent work on an
//! [AHP](https://github.com/microsoft/agent-host-protocol) host over
//! JSON-RPC/WebSocket. Each result row is correlated by `resultKeyFields`:
//!
//! * **session mode** (default): `ADD` creates a session and starts a turn
//!   with a templated message, `UPDATE` sends a follow-up
//!   (`chat/pendingMessageSet`), and `DELETE` cancels the active turn and
//!   optionally archives or disposes the session.
//! * **automation mode**: `ADD` runs an existing host automation with an
//!   idempotent `requestId`; `DELETE` requests run cancellation.
//!
//! See [`AhpReactionConfig`] and the crate `README.md` for configuration.
//!
//! ## Quick start
//!
//! ```
//! # fn main() -> anyhow::Result<()> {
//! use drasi_reaction_ahp::AhpReaction;
//!
//! let reaction = AhpReaction::builder("incident-agent")
//!     .with_query("open-incidents")
//!     .with_endpoint("ws://127.0.0.1:8080")
//!     .with_result_key_fields(["incidentId"])
//!     .with_message_template("Investigate incident {{after.incidentId}}: {{after.title}}")
//!     .build()?;
//! # let _ = reaction;
//! # Ok(())
//! # }
//! ```

pub mod activation;
pub mod config;
pub mod descriptor;
pub(crate) mod host;
#[cfg(any(test, feature = "mock-host"))]
pub mod mock_host;
pub(crate) mod process;
pub mod reaction;
pub mod transport;

#[cfg(test)]
mod tests;

use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};
use drasi_lib::recovery::ReactionRecoveryPolicy;

pub use activation::{next_action, Action, Activation, Operation, ResultKey};
pub use config::{AhpMode, AhpReactionConfig, FollowUpKind, OnDelete};
pub use reaction::AhpReaction;

/// Builder for [`AhpReaction`]. `endpoint` and `resultKeyFields` are required.
pub struct AhpReactionBuilder {
    id: String,
    queries: Vec<String>,
    config: AhpReactionConfig,
    priority_queue_capacity: Option<usize>,
    auto_start: bool,
    recovery_policy: Option<ReactionRecoveryPolicy>,
}

impl AhpReactionBuilder {
    /// Create a builder for a reaction with the given id.
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            queries: Vec::new(),
            config: AhpReactionConfig::default(),
            priority_queue_capacity: None,
            auto_start: true,
            recovery_policy: None,
        }
    }

    /// Replace the subscribed query list.
    pub fn with_queries(mut self, queries: Vec<String>) -> Self {
        self.queries = queries;
        self
    }

    /// Subscribe to a query.
    pub fn with_query(mut self, query_id: impl Into<String>) -> Self {
        self.queries.push(query_id.into());
        self
    }

    /// Alias of [`with_query`](Self::with_query).
    pub fn from_query(self, query_id: impl Into<String>) -> Self {
        self.with_query(query_id)
    }

    /// AHP host WebSocket URL (`ws://` or `wss://`).
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.config.endpoint = endpoint.into();
        self
    }

    /// Bearer token for the WebSocket handshake and `authenticate`.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.config.token = Some(token.into());
        self
    }

    /// Protected resource the token is pushed for via `authenticate`.
    pub fn with_auth_resource(mut self, resource: impl Into<String>) -> Self {
        self.config.auth_resource = Some(resource.into());
        self
    }

    /// Stable AHP client id (defaults to `drasi-reaction-<id>`).
    pub fn with_client_id(mut self, client_id: impl Into<String>) -> Self {
        self.config.client_id = Some(client_id.into());
        self
    }

    /// Request and acknowledgement timeout in milliseconds.
    pub fn with_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.timeout_ms = timeout_ms;
        self
    }

    /// How long to wait for a new session to become ready.
    pub fn with_session_ready_timeout_ms(mut self, timeout_ms: u64) -> Self {
        self.config.session_ready_timeout_ms = timeout_ms;
        self
    }

    /// Session (default) or automation mode.
    pub fn with_mode(mut self, mode: AhpMode) -> Self {
        self.config.mode = mode;
        self
    }

    /// Switch to automation mode, running `automation` (an `ahp-automation:` URI).
    pub fn with_automation(mut self, automation: impl Into<String>) -> Self {
        self.config.mode = AhpMode::Automation;
        self.config.automation = Some(automation.into());
        self
    }

    /// Fields used to correlate diffs with AHP work. Required.
    pub fn with_result_key_fields<I, S>(mut self, fields: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.result_key_fields = fields.into_iter().map(Into::into).collect();
        self
    }

    /// Agent provider for new sessions.
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.config.provider = Some(provider.into());
        self
    }

    /// Working directory URIs for new sessions.
    pub fn with_working_directories<I, S>(mut self, dirs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.config.working_directories = dirs.into_iter().map(Into::into).collect();
        self
    }

    /// Agent-specific configuration passed to `createSession`.
    pub fn with_session_config(
        mut self,
        config: serde_json::Map<String, serde_json::Value>,
    ) -> Self {
        self.config.session_config = Some(config);
        self
    }

    /// Handlebars template for the session title.
    pub fn with_title_template(mut self, template: impl Into<String>) -> Self {
        self.config.title_template = Some(template.into());
        self
    }

    /// Default template for the initial message (`ADD`).
    pub fn with_message_template(mut self, template: impl Into<String>) -> Self {
        self.config
            .default_template
            .get_or_insert_with(QueryConfig::default)
            .added = Some(TemplateSpec::new(template));
        self
    }

    /// Default template for follow-up messages (`UPDATE`).
    pub fn with_follow_up_template(mut self, template: impl Into<String>) -> Self {
        self.config
            .default_template
            .get_or_insert_with(QueryConfig::default)
            .updated = Some(TemplateSpec::new(template));
        self
    }

    /// Default template for a closing message sent on `DELETE`.
    pub fn with_closing_template(mut self, template: impl Into<String>) -> Self {
        self.config
            .default_template
            .get_or_insert_with(QueryConfig::default)
            .deleted = Some(TemplateSpec::new(template));
        self
    }

    /// Per-query templates (query id or its last dotted segment).
    pub fn with_route(mut self, query_id: impl Into<String>, config: QueryConfig) -> Self {
        self.config.routes.insert(query_id.into(), config);
        self
    }

    /// Templates used when no route matches.
    pub fn with_default_template(mut self, config: QueryConfig) -> Self {
        self.config.default_template = Some(config);
        self
    }

    /// Pending-message kind for follow-ups.
    pub fn with_follow_up_kind(mut self, kind: FollowUpKind) -> Self {
        self.config.follow_up_kind = kind;
        self
    }

    /// Behavior when a correlated row is deleted.
    pub fn with_on_delete(mut self, on_delete: OnDelete) -> Self {
        self.config.on_delete = on_delete;
        self
    }

    /// Declare the reaction durable (requires a persistent state store).
    pub fn with_durable(mut self, durable: bool) -> Self {
        self.config.durable = durable;
        self
    }

    /// Priority queue capacity.
    pub fn with_priority_queue_capacity(mut self, capacity: usize) -> Self {
        self.priority_queue_capacity = Some(capacity);
        self
    }

    /// Whether the reaction starts with the host.
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Per-instance recovery policy (defaults to `Strict`).
    pub fn with_recovery_policy(mut self, policy: ReactionRecoveryPolicy) -> Self {
        self.recovery_policy = Some(policy);
        self
    }

    /// Replace the entire reaction configuration.
    pub fn with_config(mut self, config: AhpReactionConfig) -> Self {
        self.config = config;
        self
    }

    /// Validate the configuration and build the reaction.
    pub fn build(self) -> anyhow::Result<AhpReaction> {
        self.config
            .validate(&self.queries, self.priority_queue_capacity)?;
        Ok(AhpReaction::from_builder(
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
    plugin_id = "ahp-reaction",
    core_version = env!("CARGO_PKG_VERSION"),
    lib_version = env!("CARGO_PKG_VERSION"),
    plugin_version = env!("CARGO_PKG_VERSION"),
    source_descriptors = [],
    reaction_descriptors = [descriptor::AhpReactionDescriptor],
    bootstrap_descriptors = [],
);
