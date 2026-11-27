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

//! Per-diff processing: correlation, templating, and AHP dispatch.

use std::collections::HashMap;

use ahp::ahp_types::common::JsonObject;
use ahp::ahp_types::state::PendingMessageKind;
use anyhow::Context;
use handlebars::Handlebars;
use log::{debug, warn};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::reactions::common::base::ReactionBase;
use drasi_lib::reactions::common::{QueryConfig, TemplateSpec};

use crate::activation::{length_prefixed, next_action, Action, Activation, Operation, ResultKey};
use crate::config::{AhpReactionConfig, FollowUpKind, OnDelete};
use crate::host::{AhpHost, OutboundMessage};

const STATE_KEY_PREFIX: &str = "activation";
const META_KEY: &str = "drasi";
const MAX_ACTIVATION_CACHE: usize = 10_000;
/// Attempts per diff before the recovery policy applies. Every operation is
/// idempotent (deterministic ids), so retrying is safe.
const MAX_ATTEMPTS: u32 = 3;
const RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(250);

/// Build the Handlebars registry with the standard `json` helper.
pub fn build_handlebars() -> Handlebars<'static> {
    let mut handlebars = Handlebars::new();
    handlebars.register_helper(
        "json",
        Box::new(
            |h: &handlebars::Helper,
             _: &Handlebars,
             _: &handlebars::Context,
             _: &mut handlebars::RenderContext,
             out: &mut dyn handlebars::Output|
             -> handlebars::HelperResult {
                if let Some(value) = h.param(0) {
                    let rendered =
                        serde_json::to_string(value.value()).unwrap_or_else(|_| "null".to_string());
                    out.write(&rendered)?;
                }
                Ok(())
            },
        ),
    );
    handlebars
}

/// Before/after view of one result diff.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DiffPayload {
    pub operation: Operation,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub data: Option<Value>,
}

impl DiffPayload {
    pub fn from_result_diff(diff: &ResultDiff) -> Option<Self> {
        match diff {
            ResultDiff::Add { data, .. } => Some(Self {
                operation: Operation::Add,
                before: None,
                after: Some(data.clone()),
                data: None,
            }),
            ResultDiff::Delete { data, .. } => Some(Self {
                operation: Operation::Delete,
                before: Some(data.clone()),
                after: None,
                data: None,
            }),
            ResultDiff::Update {
                data,
                before,
                after,
                ..
            } => Some(Self {
                operation: Operation::Update,
                before: Some(before.clone()),
                after: Some(after.clone()),
                data: Some(data.clone()),
            }),
            ResultDiff::Aggregation { before, after, .. } => Some(Self {
                operation: if before.is_some() {
                    Operation::Update
                } else {
                    Operation::Add
                },
                before: before.clone(),
                after: Some(after.clone()),
                data: None,
            }),
            ResultDiff::Noop => None,
        }
    }
}

/// Stateful diff processor owned by the reaction's processing task.
pub(crate) struct Processor {
    pub reaction_id: String,
    pub base: ReactionBase,
    pub config: AhpReactionConfig,
    pub host: AhpHost,
    pub handlebars: Handlebars<'static>,
    pub cache: HashMap<String, Option<Activation>>,
}

impl Processor {
    /// Process every diff of `query_result`; an error leaves the checkpoint
    /// unadvanced (subject to the recovery policy).
    pub async fn process(&mut self, query_result: &QueryResult) -> anyhow::Result<()> {
        for (index, diff) in query_result.results.iter().enumerate() {
            let Some(payload) = DiffPayload::from_result_diff(diff) else {
                continue;
            };
            let diff_index = u32::try_from(index).unwrap_or(u32::MAX);
            let mut attempt = 1;
            loop {
                match self.process_diff(query_result, &payload, diff_index).await {
                    Ok(()) => break,
                    Err(error) if attempt < MAX_ATTEMPTS => {
                        warn!(
                            "[{}] Attempt {attempt}/{MAX_ATTEMPTS} failed for query '{}' seq {}; retrying: {error:#}",
                            self.reaction_id, query_result.query_id, query_result.sequence
                        );
                        tokio::time::sleep(RETRY_BACKOFF * attempt).await;
                        attempt += 1;
                    }
                    Err(error) => {
                        return Err(error).with_context(|| {
                            format!(
                                "failed processing {} diff {diff_index} of query '{}' seq {}",
                                payload.operation.as_str(),
                                query_result.query_id,
                                query_result.sequence
                            )
                        })
                    }
                }
            }
        }
        Ok(())
    }

    async fn process_diff(
        &mut self,
        query_result: &QueryResult,
        payload: &DiffPayload,
        diff_index: u32,
    ) -> anyhow::Result<()> {
        let result_key = match extract_result_key(payload, &self.config.result_key_fields) {
            Ok(key) => key,
            Err(error) => {
                warn!(
                    "[{}] Dropping {} diff of query '{}' seq {}: {error:#}",
                    self.reaction_id,
                    payload.operation.as_str(),
                    query_result.query_id,
                    query_result.sequence
                );
                return Ok(());
            }
        };
        let sequence = query_result.sequence;
        let state_key = format!(
            "{STATE_KEY_PREFIX}:{}",
            length_prefixed(&[&query_result.query_id, result_key.as_str()])
        );
        let activation = self.load(&state_key).await?;
        let work_id = work_id(
            &self.reaction_id,
            &query_result.query_id,
            &result_key,
            sequence,
            diff_index,
        );

        let action = next_action(
            self.config.mode,
            payload.operation,
            activation.as_ref(),
            sequence,
            diff_index,
        );
        match action {
            Action::StartSession => {
                self.start_session(
                    query_result,
                    payload,
                    &result_key,
                    &work_id,
                    &state_key,
                    diff_index,
                )
                .await?;
            }
            Action::RunAutomation => {
                let automation = self
                    .config
                    .automation
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("`automation` is not configured"))?;
                let mut meta = JsonObject::new();
                meta.insert(
                    META_KEY.to_string(),
                    change_envelope(query_result, payload, &result_key),
                );
                let run = self
                    .host
                    .run_automation(&automation, &work_id, meta)
                    .await?;
                self.save(
                    &state_key,
                    Some(Activation::Automation {
                        run,
                        sequence,
                        diff_index,
                    }),
                )
                .await?;
            }
            Action::FollowUp { session, chat } => {
                let message = self.render_message(query_result, payload, &result_key);
                let kind = match self.config.follow_up_kind {
                    FollowUpKind::Queued => PendingMessageKind::Queued,
                    FollowUpKind::Steering => PendingMessageKind::Steering,
                };
                let delivered = self
                    .host
                    .set_pending_message(&chat, &work_id, kind, message)
                    .await?;
                if delivered {
                    let updated = Activation::Session {
                        session,
                        chat,
                        sequence,
                        diff_index,
                    };
                    self.save(&state_key, Some(updated)).await?;
                } else {
                    debug!(
                        "[{}] AHP chat '{chat}' is gone; starting a new session for '{result_key}'",
                        self.reaction_id
                    );
                    self.start_session(
                        query_result,
                        payload,
                        &result_key,
                        &work_id,
                        &state_key,
                        diff_index,
                    )
                    .await?;
                }
            }
            Action::Close(existing) => {
                self.close(query_result, payload, &result_key, &work_id, existing)
                    .await?;
                self.save(&state_key, None).await?;
            }
            Action::Drop { reason } => {
                debug!(
                    "[{}] Dropping {} diff for query '{}' key '{result_key}': {reason}",
                    self.reaction_id,
                    payload.operation.as_str(),
                    query_result.query_id
                );
            }
        }
        Ok(())
    }

    async fn start_session(
        &mut self,
        query_result: &QueryResult,
        payload: &DiffPayload,
        result_key: &ResultKey,
        work_id: &str,
        state_key: &str,
        diff_index: u32,
    ) -> anyhow::Result<()> {
        let session_uri = format!("ahp-session:/{work_id}");
        let title = self.render_title(query_result, payload, result_key);
        let chat = self.host.create_session(&session_uri, title).await?;
        let message = self.render_message(query_result, payload, result_key);
        self.host.start_turn(&chat, work_id, message).await?;
        self.save(
            state_key,
            Some(Activation::Session {
                session: session_uri,
                chat,
                sequence: query_result.sequence,
                diff_index,
            }),
        )
        .await
    }

    async fn close(
        &mut self,
        query_result: &QueryResult,
        payload: &DiffPayload,
        result_key: &ResultKey,
        work_id: &str,
        existing: Activation,
    ) -> anyhow::Result<()> {
        match existing {
            Activation::Session { session, chat, .. } => {
                if let Some(message) = self.render_template_only(query_result, payload, result_key)
                {
                    let kind = match self.config.follow_up_kind {
                        FollowUpKind::Queued => PendingMessageKind::Queued,
                        FollowUpKind::Steering => PendingMessageKind::Steering,
                    };
                    self.host
                        .set_pending_message(&chat, work_id, kind, message)
                        .await?;
                }
                if self.config.on_delete != OnDelete::None {
                    self.host.cancel_active_turn(&chat).await?;
                }
                match self.config.on_delete {
                    OnDelete::Archive => self.host.archive_session(&session).await?,
                    OnDelete::Dispose => self.host.dispose_session(&session).await?,
                    OnDelete::None | OnDelete::Cancel => {}
                }
            }
            Activation::Automation { run, .. } => {
                if self.config.on_delete != OnDelete::None {
                    self.host.cancel_run(&run).await?;
                }
            }
        }
        Ok(())
    }

    fn template_spec(&self, query_id: &str, operation: Operation) -> Option<&TemplateSpec> {
        let config = resolve_query_config(
            query_id,
            &self.config.routes,
            self.config.default_template.as_ref(),
        )?;
        match operation {
            Operation::Add => config.added.as_ref(),
            Operation::Update => config.updated.as_ref(),
            Operation::Delete => config.deleted.as_ref(),
        }
    }

    /// Render the configured template for `payload`, falling back to the
    /// default text on a missing template or render failure.
    fn render_message(
        &self,
        query_result: &QueryResult,
        payload: &DiffPayload,
        result_key: &ResultKey,
    ) -> OutboundMessage {
        let text = self
            .render_configured(query_result, payload, result_key)
            .unwrap_or_else(|| default_text(query_result, payload, result_key));
        self.outbound(query_result, payload, result_key, text)
    }

    /// Render only an explicitly configured template (used for `deleted`).
    fn render_template_only(
        &self,
        query_result: &QueryResult,
        payload: &DiffPayload,
        result_key: &ResultKey,
    ) -> Option<OutboundMessage> {
        let text = self.render_configured(query_result, payload, result_key)?;
        Some(self.outbound(query_result, payload, result_key, text))
    }

    fn render_configured(
        &self,
        query_result: &QueryResult,
        payload: &DiffPayload,
        result_key: &ResultKey,
    ) -> Option<String> {
        let spec = self.template_spec(&query_result.query_id, payload.operation)?;
        if spec.template.trim().is_empty() {
            return None;
        }
        let context = template_context(query_result, payload, result_key);
        match self.handlebars.render_template(&spec.template, &context) {
            Ok(text) if !text.trim().is_empty() => Some(text),
            Ok(_) => None,
            Err(error) => {
                warn!(
                    "[{}] Message template render failed for query '{}'; using default text: {error}",
                    self.reaction_id, query_result.query_id
                );
                None
            }
        }
    }

    fn render_title(
        &self,
        query_result: &QueryResult,
        payload: &DiffPayload,
        result_key: &ResultKey,
    ) -> Option<String> {
        let template = self.config.title_template.as_ref()?;
        let context = template_context(query_result, payload, result_key);
        match self.handlebars.render_template(template, &context) {
            Ok(title) => Some(title),
            Err(error) => {
                warn!(
                    "[{}] Title template render failed; leaving host default title: {error}",
                    self.reaction_id
                );
                None
            }
        }
    }

    fn outbound(
        &self,
        query_result: &QueryResult,
        payload: &DiffPayload,
        result_key: &ResultKey,
        text: String,
    ) -> OutboundMessage {
        let mut meta = JsonObject::new();
        meta.insert(
            META_KEY.to_string(),
            change_envelope(query_result, payload, result_key),
        );
        OutboundMessage { text, meta }
    }

    async fn load(&mut self, state_key: &str) -> anyhow::Result<Option<Activation>> {
        if let Some(cached) = self.cache.get(state_key) {
            return Ok(cached.clone());
        }
        let activation =
            match self.base.state_store().await {
                Some(store) => match store.get(&self.base.id, state_key).await.map_err(|e| {
                    anyhow::anyhow!("state store read failed for '{state_key}': {e}")
                })? {
                    Some(bytes) => Some(serde_json::from_slice(&bytes).with_context(|| {
                        format!("failed decoding activation state '{state_key}'")
                    })?),
                    None => None,
                },
                None => None,
            };
        self.remember(state_key, activation.clone());
        Ok(activation)
    }

    async fn save(
        &mut self,
        state_key: &str,
        activation: Option<Activation>,
    ) -> anyhow::Result<()> {
        if let Some(store) = self.base.state_store().await {
            match &activation {
                Some(value) => {
                    let bytes = serde_json::to_vec(value).context("failed encoding activation")?;
                    store
                        .set(&self.base.id, state_key, bytes)
                        .await
                        .map_err(|e| {
                            anyhow::anyhow!("state store write failed for '{state_key}': {e}")
                        })?;
                }
                None => {
                    store.delete(&self.base.id, state_key).await.map_err(|e| {
                        anyhow::anyhow!("state store delete failed for '{state_key}': {e}")
                    })?;
                }
            }
        }
        self.remember(state_key, activation);
        Ok(())
    }

    fn remember(&mut self, state_key: &str, activation: Option<Activation>) {
        if self.cache.len() >= MAX_ACTIVATION_CACHE && !self.cache.contains_key(state_key) {
            self.cache.clear();
        }
        self.cache.insert(state_key.to_string(), activation);
    }
}

/// Route lookup: exact query id → last dotted segment → default template.
pub(crate) fn resolve_query_config<'a>(
    query_id: &str,
    routes: &'a HashMap<String, QueryConfig>,
    default_template: Option<&'a QueryConfig>,
) -> Option<&'a QueryConfig> {
    routes
        .get(query_id)
        .or_else(|| {
            query_id
                .contains('.')
                .then(|| query_id.rsplit('.').next())
                .flatten()
                .and_then(|name| routes.get(name))
        })
        .or(default_template)
}

/// Deterministic, URI-safe identifier for the work triggered by one diff.
pub(crate) fn work_id(
    reaction_id: &str,
    query_id: &str,
    result_key: &ResultKey,
    sequence: u64,
    diff_index: u32,
) -> String {
    let identity = length_prefixed(&[reaction_id, query_id, result_key.as_str()]);
    let digest = Sha256::digest(identity.as_bytes());
    let hex: String = digest[..12].iter().map(|b| format!("{b:02x}")).collect();
    format!("drasi-{hex}-{sequence}-{diff_index}")
}

pub(crate) fn extract_result_key(
    payload: &DiffPayload,
    fields: &[String],
) -> anyhow::Result<ResultKey> {
    let source = match payload.operation {
        Operation::Delete => payload.before.as_ref(),
        Operation::Add | Operation::Update => payload.after.as_ref(),
    }
    .ok_or_else(|| anyhow::anyhow!("diff has no row to read result key fields from"))?;

    let lookup = |field: &str| -> anyhow::Result<String> {
        let mut value = source;
        for segment in field.split('.') {
            value = value
                .get(segment)
                .ok_or_else(|| anyhow::anyhow!("missing result key field '{field}'"))?;
        }
        Ok(match value {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
    };

    if let [field] = fields {
        return lookup(field).map(ResultKey);
    }
    let mut parts = Vec::with_capacity(fields.len());
    for field in fields {
        let value = lookup(field)?;
        parts.push(format!(
            "{}={}",
            urlencoding::encode(field),
            urlencoding::encode(&value)
        ));
    }
    Ok(ResultKey(parts.join("&")))
}

/// Standard template context (README §11) plus `result_key` and `sequence`.
pub(crate) fn template_context(
    query_result: &QueryResult,
    payload: &DiffPayload,
    result_key: &ResultKey,
) -> Value {
    let query_name = query_result
        .query_id
        .rsplit('.')
        .next()
        .unwrap_or(&query_result.query_id);
    json!({
        "query_id": query_result.query_id,
        "query_name": query_name,
        "operation": payload.operation.as_str(),
        "timestamp": query_result.timestamp.timestamp_millis(),
        "sequence": query_result.sequence,
        "result_key": result_key.as_str(),
        "metadata": query_result.metadata,
        "before": payload.before,
        "after": payload.after,
        "data": payload.data,
    })
}

/// Canonical change envelope (README §10) carried in `Message._meta.drasi`.
pub(crate) fn change_envelope(
    query_result: &QueryResult,
    payload: &DiffPayload,
    result_key: &ResultKey,
) -> Value {
    let mut root = Map::new();
    root.insert("queryId".into(), json!(query_result.query_id));
    root.insert("sequenceId".into(), json!(query_result.sequence));
    root.insert(
        "timestamp".into(),
        json!(query_result.timestamp.timestamp_millis()),
    );
    root.insert("operation".into(), json!(payload.operation.as_str()));
    root.insert("resultKey".into(), json!(result_key.as_str()));
    if let Some(before) = &payload.before {
        root.insert("before".into(), before.clone());
    }
    if let Some(after) = &payload.after {
        root.insert("after".into(), after.clone());
    }
    if !query_result.metadata.is_empty() {
        root.insert(
            "metadata".into(),
            Value::Object(query_result.metadata.clone().into_iter().collect()),
        );
    }
    Value::Object(root)
}

fn default_text(
    query_result: &QueryResult,
    payload: &DiffPayload,
    result_key: &ResultKey,
) -> String {
    let pretty = |v: &Option<Value>| {
        v.as_ref()
            .and_then(|v| serde_json::to_string_pretty(v).ok())
            .unwrap_or_else(|| "null".to_string())
    };
    match payload.operation {
        Operation::Add => format!(
            "Drasi continuous query `{}` has a new result `{result_key}`:\n\n```json\n{}\n```",
            query_result.query_id,
            pretty(&payload.after)
        ),
        Operation::Update => format!(
            "Drasi continuous query `{}` updated result `{result_key}`.\n\nBefore:\n```json\n{}\n```\n\nAfter:\n```json\n{}\n```",
            query_result.query_id,
            pretty(&payload.before),
            pretty(&payload.after)
        ),
        Operation::Delete => format!(
            "Drasi continuous query `{}` no longer contains result `{result_key}`.",
            query_result.query_id
        ),
    }
}

/// Settings derived for the host connection from `config`.
pub(crate) fn host_settings(
    reaction_id: &str,
    config: &AhpReactionConfig,
) -> crate::host::HostSettings {
    crate::host::HostSettings {
        endpoint: config.endpoint.trim().to_string(),
        token: config.token.clone(),
        auth_resource: config.auth_resource.clone(),
        client_id: config
            .client_id
            .clone()
            .unwrap_or_else(|| format!("drasi-reaction-{reaction_id}")),
        timeout: std::time::Duration::from_millis(config.timeout_ms),
        ready_timeout: std::time::Duration::from_millis(config.session_ready_timeout_ms),
        provider: config.provider.clone(),
        working_directories: config.working_directories.clone(),
        session_config: config.session_config.clone(),
    }
}
