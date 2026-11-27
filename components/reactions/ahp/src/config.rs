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

//! Typed configuration for the AHP reaction.

use std::collections::HashMap;

use anyhow::Context;
use drasi_lib::reactions::common::QueryConfig;
use serde::{Deserialize, Serialize};

fn default_timeout_ms() -> u64 {
    10_000
}

fn default_session_ready_timeout_ms() -> u64 {
    60_000
}

/// How query results are turned into agent work on the AHP host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AhpMode {
    /// `ADD` creates a session and starts a turn on its default chat with a
    /// templated message; `UPDATE` queues/steers a follow-up message;
    /// `DELETE` applies [`OnDelete`].
    #[default]
    Session,
    /// `ADD` runs an existing host automation (`runAutomation`) with an
    /// idempotent `requestId`; `DELETE` requests cancellation of that run.
    Automation,
}

/// Kind of pending message used for `UPDATE` follow-ups in session mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FollowUpKind {
    /// Sent as a new turn once the current turn finishes.
    #[default]
    Queued,
    /// Injected into the active turn at the host's discretion.
    Steering,
}

/// What to do on the AHP host when a correlated result row is deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OnDelete {
    /// Leave the session/run untouched; only forget the correlation.
    None,
    /// Cancel the active turn (session mode) or request run cancellation
    /// (automation mode).
    #[default]
    Cancel,
    /// Cancel, then archive the session (`session/isArchivedChanged`).
    Archive,
    /// Cancel, then dispose the session (`disposeSession`).
    Dispose,
}

/// Configuration for the AHP reaction. Serialized keys are camelCase.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct AhpReactionConfig {
    /// AHP host WebSocket URL (`ws://` or `wss://`). Trusted operator input.
    pub endpoint: String,
    /// Optional bearer token. Sent as `Authorization: Bearer` on the
    /// WebSocket handshake and, when `authResource` is set, pushed with the
    /// AHP `authenticate` command.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Protected resource identifier the token is pushed for via `authenticate`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_resource: Option<String>,
    /// Stable AHP `clientId`. Defaults to `drasi-reaction-<reaction id>`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Per-request and action-acknowledgement timeout. Defaults to 10000 ms.
    #[serde(default = "default_timeout_ms")]
    pub timeout_ms: u64,
    /// How long to wait for a new session to become ready. Defaults to 60000 ms.
    #[serde(default = "default_session_ready_timeout_ms")]
    pub session_ready_timeout_ms: u64,
    /// Session (default) or automation mode.
    #[serde(default)]
    pub mode: AhpMode,
    /// Fields used to correlate diffs with one AHP session or run. Required.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub result_key_fields: Vec<String>,
    /// Agent provider for new sessions (host default when omitted).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Working directory URIs for new sessions.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub working_directories: Vec<String>,
    /// Agent-specific session configuration passed to `createSession`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_config: Option<serde_json::Map<String, serde_json::Value>>,
    /// Optional Handlebars template for the session title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_template: Option<String>,
    /// Per-query message templates (`added` = initial message, `updated` =
    /// follow-up message). Keys are query ids or their last dotted segment.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub routes: HashMap<String, QueryConfig>,
    /// Message templates used when no route matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_template: Option<QueryConfig>,
    /// Pending-message kind for `UPDATE` follow-ups. Defaults to `queued`.
    #[serde(default)]
    pub follow_up_kind: FollowUpKind,
    /// Behavior on `DELETE`. Defaults to `cancel`.
    #[serde(default)]
    pub on_delete: OnDelete,
    /// Automation URI (`ahp-automation:/...`). Required in automation mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub automation: Option<String>,
    /// Declare the reaction durable (`Reaction::is_durable`). Requires a
    /// persistent state store and persistent queries. Defaults to `false`.
    #[serde(default)]
    pub durable: bool,
}

impl Default for AhpReactionConfig {
    fn default() -> Self {
        Self {
            endpoint: "ws://localhost".to_string(),
            token: None,
            auth_resource: None,
            client_id: None,
            timeout_ms: default_timeout_ms(),
            session_ready_timeout_ms: default_session_ready_timeout_ms(),
            mode: AhpMode::default(),
            result_key_fields: Vec::new(),
            provider: None,
            working_directories: Vec::new(),
            session_config: None,
            title_template: None,
            routes: HashMap::new(),
            default_template: None,
            follow_up_kind: FollowUpKind::default(),
            on_delete: OnDelete::default(),
            automation: None,
            durable: false,
        }
    }
}

impl AhpReactionConfig {
    /// Validates operator configuration and compiles every template.
    ///
    /// `endpoint` is trusted input; loopback and private addresses are
    /// allowed so local agent hosts can be used.
    pub fn validate(
        &self,
        query_ids: &[String],
        priority_queue_capacity: Option<usize>,
    ) -> anyhow::Result<()> {
        let url = url::Url::parse(self.endpoint.trim()).context("invalid `endpoint` URL")?;
        match url.scheme() {
            "ws" | "wss" => {}
            scheme => anyhow::bail!("unsupported endpoint scheme '{scheme}'; expected ws or wss"),
        }
        if url.host_str().is_none() {
            anyhow::bail!("`endpoint` must include a host");
        }
        if self.timeout_ms == 0 {
            anyhow::bail!("`timeoutMs` must be greater than 0");
        }
        if self.session_ready_timeout_ms == 0 {
            anyhow::bail!("`sessionReadyTimeoutMs` must be greater than 0");
        }
        if matches!(priority_queue_capacity, Some(0)) {
            anyhow::bail!("`priorityQueueCapacity` must be greater than 0");
        }
        if self.result_key_fields.is_empty() {
            anyhow::bail!("`resultKeyFields` must contain at least one field");
        }
        if self.result_key_fields.iter().any(|f| f.trim().is_empty()) {
            anyhow::bail!("`resultKeyFields` must not contain empty values");
        }
        if self.auth_resource.is_some() && self.token.is_none() {
            anyhow::bail!("`authResource` requires `token`");
        }
        if let Some(client_id) = &self.client_id {
            if client_id.trim().is_empty() {
                anyhow::bail!("`clientId` must not be empty");
            }
        }
        if self.mode == AhpMode::Automation {
            match &self.automation {
                Some(uri) if !uri.trim().is_empty() => {}
                _ => anyhow::bail!("`automation` is required when `mode` is `automation`"),
            }
        }

        for route in self.routes.keys() {
            let matches = query_ids
                .iter()
                .any(|q| q == route || q.rsplit('.').next() == Some(route.as_str()));
            if !matches {
                anyhow::bail!("route '{route}' does not match any subscribed query");
            }
        }

        if let Some(template) = &self.title_template {
            compile("titleTemplate", template)?;
        }
        let configs = self
            .routes
            .iter()
            .map(|(name, cfg)| (format!("routes.{name}"), cfg))
            .chain(
                self.default_template
                    .iter()
                    .map(|cfg| ("defaultTemplate".to_string(), cfg)),
            );
        for (name, cfg) in configs {
            for (op, spec) in [
                ("added", &cfg.added),
                ("updated", &cfg.updated),
                ("deleted", &cfg.deleted),
            ] {
                if let Some(spec) = spec {
                    compile(&format!("{name}.{op}"), &spec.template)?;
                }
            }
        }
        Ok(())
    }
}

fn compile(name: &str, template: &str) -> anyhow::Result<()> {
    handlebars::Template::compile(template)
        .map(|_| ())
        .map_err(|e| anyhow::anyhow!("invalid template `{name}`: {e}"))
}
