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

//! Azure Event Grid reaction implementation.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use handlebars::Handlebars;
use log::{debug, error, info, warn};
use serde_json::{json, Map, Value};
use tokio::sync::oneshot;

use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::identity::{CredentialContext, Credentials};
use drasi_lib::managers::log_component_start;
use drasi_lib::reactions::common::base::{ReactionBase, ReactionBaseParams};
use drasi_lib::reactions::common::{OperationType, TemplateRouting};
use drasi_lib::recovery::ReactionRecoveryPolicy;
use drasi_lib::Reaction;

use crate::config::{EventGridReactionConfig, EventGridSchema, OutputFormat, EVENT_GRID_SCOPE};
use crate::event::{
    deterministic_id, unpacked_notification, ChangeOp, EventEnvelope, CHANGE_EVENT_TYPE,
};
use crate::publish::{build_client, publish, Auth};
use crate::EventGridReactionBuilder;

/// Azure Event Grid reaction: publishes query-result changes to a custom topic.
pub struct EventGridReaction {
    pub(crate) base: ReactionBase,
    pub(crate) config: EventGridReactionConfig,
}

impl EventGridReaction {
    /// Create a builder for the Event Grid reaction.
    pub fn builder(id: impl Into<String>) -> EventGridReactionBuilder {
        EventGridReactionBuilder::new(id)
    }

    /// Create a new Event Grid reaction from a config, validating it against
    /// `queries`. Mirrors the builder's `build()` so direct construction cannot
    /// produce an invalid reaction that only fails at `start()`.
    pub fn new(
        id: impl Into<String>,
        queries: Vec<String>,
        config: EventGridReactionConfig,
    ) -> Result<Self> {
        config.validate(&queries, None)?;
        let params = ReactionBaseParams::new(id.into(), queries);
        Ok(Self {
            base: ReactionBase::new(params),
            config,
        })
    }

    pub(crate) fn from_builder(
        id: String,
        queries: Vec<String>,
        config: EventGridReactionConfig,
        priority_queue_capacity: Option<usize>,
        auto_start: bool,
        recovery_policy: Option<ReactionRecoveryPolicy>,
    ) -> Self {
        let mut params = ReactionBaseParams::new(id, queries).with_auto_start(auto_start);
        if let Some(capacity) = priority_queue_capacity {
            params = params.with_priority_queue_capacity(capacity);
        }
        if let Some(policy) = recovery_policy {
            params = params.with_recovery_policy(policy);
        }
        Self {
            base: ReactionBase::new(params),
            config,
        }
    }
}

/// Build a Handlebars registry with the shared `json` helper.
pub(crate) fn build_handlebars() -> Handlebars<'static> {
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
                    let json_str =
                        serde_json::to_string(value.value()).unwrap_or_else(|_| "null".to_string());
                    out.write(&json_str)?;
                }
                Ok(())
            },
        ),
    );
    handlebars
}

/// Build the Handlebars render context for a single diff.
fn build_context(
    query_id: &str,
    op: ChangeOp,
    diff: &ResultDiff,
    timestamp: &str,
) -> Map<String, Value> {
    let mut context = Map::new();
    context.insert("query_name".into(), json!(query_id));
    context.insert("query_id".into(), json!(query_id));
    context.insert("timestamp".into(), json!(timestamp));
    let op_name = match op {
        ChangeOp::Insert => "ADD",
        ChangeOp::Update => "UPDATE",
        ChangeOp::Delete => "DELETE",
    };
    context.insert("operation".into(), json!(op_name));

    match diff {
        ResultDiff::Add { data, .. } => {
            context.insert("after".into(), data.clone());
        }
        ResultDiff::Delete { data, .. } => {
            context.insert("before".into(), data.clone());
        }
        ResultDiff::Update {
            before,
            after,
            data,
            ..
        } => {
            context.insert("before".into(), before.clone());
            context.insert("after".into(), after.clone());
            context.insert("data".into(), data.clone());
        }
        ResultDiff::Aggregation { before, after, .. } => {
            if let Some(before) = before {
                context.insert("before".into(), before.clone());
            }
            context.insert("after".into(), after.clone());
        }
        ResultDiff::Noop => {}
    }
    context
}

/// Map a [`ResultDiff`] to its [`OperationType`] (for template routing).
fn operation_type(diff: &ResultDiff) -> Option<OperationType> {
    match diff {
        ResultDiff::Add { .. } => Some(OperationType::Add),
        ResultDiff::Update { .. } | ResultDiff::Aggregation { .. } => Some(OperationType::Update),
        ResultDiff::Delete { .. } => Some(OperationType::Delete),
        ResultDiff::Noop => None,
    }
}

/// Build the events for one [`QueryResult`] according to the configured format.
pub(crate) fn build_events(
    reaction_id: &str,
    config: &EventGridReactionConfig,
    handlebars: &Handlebars<'static>,
    query_result: &QueryResult,
    reaction_name: &str,
) -> Vec<EventEnvelope> {
    let query_id = &query_result.query_id;
    let time = query_result.timestamp.to_rfc3339();
    let mut events = Vec::new();

    match config.format {
        OutputFormat::Packed => {
            // Only emit when there is at least one meaningful change.
            let has_change = query_result
                .results
                .iter()
                .any(|d| !matches!(d, ResultDiff::Noop));
            if !has_change {
                return events;
            }
            let data = serde_json::to_value(query_result).unwrap_or(Value::Null);
            events.push(EventEnvelope {
                id: deterministic_id(reaction_id, query_id, query_result.sequence, "packed", 0),
                subject: query_id.clone(),
                event_type: CHANGE_EVENT_TYPE.to_string(),
                time,
                data,
                metadata: Map::new(),
            });
        }
        OutputFormat::Unpacked => {
            for (index, diff) in query_result.results.iter().enumerate() {
                let Some((op, data)) = unpacked_notification(query_result, diff) else {
                    continue;
                };
                events.push(EventEnvelope {
                    id: deterministic_id(
                        reaction_id,
                        query_id,
                        query_result.sequence,
                        op.code(),
                        index,
                    ),
                    subject: query_id.clone(),
                    event_type: CHANGE_EVENT_TYPE.to_string(),
                    time: time.clone(),
                    data,
                    metadata: Map::new(),
                });
            }
        }
        OutputFormat::Template => {
            for (index, diff) in query_result.results.iter().enumerate() {
                let Some((op, fallback_data)) = unpacked_notification(query_result, diff) else {
                    continue;
                };
                let Some(operation) = operation_type(diff) else {
                    continue;
                };

                let spec = config.get_template_spec(query_id, operation);
                let (data, metadata) = match spec {
                    Some(spec) if !spec.template.is_empty() => {
                        let context = build_context(query_id, op, diff, &time);
                        let rendered = handlebars.render_template(&spec.template, &context);
                        let data = match rendered {
                            Ok(text) => match serde_json::from_str::<Value>(&text) {
                                Ok(v) => v,
                                Err(e) => {
                                    warn!(
                                        "[{reaction_name}] Template for query '{query_id}' ({}) rendered invalid JSON: {e} — using unpacked notification",
                                        op.code()
                                    );
                                    fallback_data.clone()
                                }
                            },
                            Err(e) => {
                                warn!(
                                    "[{reaction_name}] Template render failed for query '{query_id}' ({}): {e} — using unpacked notification",
                                    op.code()
                                );
                                fallback_data.clone()
                            }
                        };
                        let metadata: Map<String, Value> = spec
                            .extension
                            .metadata
                            .iter()
                            .map(|(k, v)| (k.clone(), json!(v)))
                            .collect();
                        (data, metadata)
                    }
                    // No template configured for this op: fall back to unpacked.
                    _ => (fallback_data.clone(), Map::new()),
                };

                events.push(EventEnvelope {
                    id: deterministic_id(
                        reaction_id,
                        query_id,
                        query_result.sequence,
                        op.code(),
                        index,
                    ),
                    subject: query_id.clone(),
                    event_type: CHANGE_EVENT_TYPE.to_string(),
                    time: time.clone(),
                    data,
                    metadata,
                });
            }
        }
    }

    events
}

/// Encode a batch of events into a JSON array for `schema`, warning once if any
/// metadata had to be dropped (native EventGrid schema).
fn encode_batch(events: &[EventEnvelope], schema: EventGridSchema, reaction_name: &str) -> Value {
    let mut dropped = false;
    let arr: Vec<Value> = events
        .iter()
        .map(|e| {
            let (value, dropped_meta) = e.to_value(schema);
            dropped |= dropped_meta;
            value
        })
        .collect();
    if dropped {
        warn!(
            "[{reaction_name}] Template metadata dropped: native EventGrid schema has no extension-attribute support (use CloudEvents schema to carry metadata)"
        );
    }
    Value::Array(arr)
}

/// Resolve the auth mode: prefer the static access key, else an AAD bearer token
/// from the identity provider (scoped to Event Grid).
async fn resolve_auth(config: &EventGridReactionConfig, base: &ReactionBase) -> Result<Auth> {
    if let Some(key) = &config.access_key {
        if !key.is_empty() {
            return Ok(Auth::AccessKey(key.clone()));
        }
    }

    let provider = base.identity_provider().await.context(
        "no access key configured and no identity provider available for AAD authentication",
    )?;
    let ctx = CredentialContext::new().with_property("scope", EVENT_GRID_SCOPE);
    let credentials = provider
        .get_credentials(&ctx)
        .await
        .context("failed to acquire Event Grid AAD token")?;
    match credentials {
        Credentials::Token { token, .. } => Ok(Auth::Bearer(token)),
        other => anyhow::bail!(
            "identity provider returned unsupported credential type for Event Grid: {other:?}"
        ),
    }
}

/// Initial backoff before retrying a failed auth acquisition; doubles each attempt.
const AUTH_INITIAL_BACKOFF: Duration = Duration::from_millis(500);
/// Upper bound for the auth-retry backoff.
const AUTH_MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Outcome of an auth resolution that may back off and wait.
enum AuthOutcome {
    /// Auth was resolved successfully.
    Ok(Auth),
    /// A shutdown signal arrived while retrying; the loop should stop.
    Shutdown,
}

/// Resolve auth, retrying with capped exponential backoff on failure.
///
/// A transient identity-provider outage would otherwise cause the caller to
/// drop the current batch and hot-loop on the next one. Retrying here preserves
/// the dequeued batch until auth succeeds, and paces retries so a persistent
/// outage does not spin or spam the log. Returns [`AuthOutcome::Shutdown`] if a
/// shutdown signal arrives while backing off.
async fn resolve_auth_with_retry(
    config: &EventGridReactionConfig,
    base: &ReactionBase,
    reaction_name: &str,
    shutdown_rx: &mut oneshot::Receiver<()>,
) -> AuthOutcome {
    let mut backoff = AUTH_INITIAL_BACKOFF;
    let mut attempt: usize = 0;
    loop {
        match resolve_auth(config, base).await {
            Ok(auth) => return AuthOutcome::Ok(auth),
            Err(e) => {
                attempt += 1;
                warn!(
                    "[{reaction_name}] Authentication attempt {attempt} failed: {e:#} — retrying in {backoff:?}"
                );
                tokio::select! {
                    biased;
                    _ = &mut *shutdown_rx => {
                        debug!("[{reaction_name}] Shutdown signal received during auth retry backoff");
                        return AuthOutcome::Shutdown;
                    }
                    _ = tokio::time::sleep(backoff) => {}
                }
                backoff = (backoff * 2).min(AUTH_MAX_BACKOFF);
            }
        }
    }
}

#[async_trait]
impl Reaction for EventGridReaction {
    fn id(&self) -> &str {
        &self.base.id
    }

    fn type_name(&self) -> &str {
        "eventgrid"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        let dto = crate::descriptor::EventGridReactionConfigDto::from(&self.config);
        self.base.properties_or_serialize(&dto)
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: drasi_lib::context::ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_identity_provider(
        &self,
        provider: std::sync::Arc<dyn drasi_lib::identity::IdentityProvider>,
    ) {
        self.base.set_identity_provider(provider).await;
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        ReactionRecoveryPolicy::AutoSkipGap
    }

    async fn start(&self) -> Result<()> {
        log_component_start("Event Grid Reaction", &self.base.id);
        info!(
            "[{}] Event Grid reaction starting - endpoint: {}, schema: {:?}, format: {:?}",
            self.base.id, self.config.endpoint, self.config.schema, self.config.format
        );

        if let Err(e) = self.config.validate(&self.base.queries, None) {
            error!(
                "[{}] Invalid Event Grid reaction config: {e:#}",
                self.base.id
            );
            self.base
                .set_status(
                    ComponentStatus::Error,
                    Some(format!("Invalid Event Grid reaction config: {e:#}")),
                )
                .await;
            return Err(e);
        }

        // Fail fast if neither access key nor an identity provider is available.
        let has_access_key = self
            .config
            .access_key
            .as_ref()
            .map(|k| !k.is_empty())
            .unwrap_or(false);
        if !has_access_key && self.base.identity_provider().await.is_none() {
            let msg =
                "no access key configured and no identity provider available for authentication";
            error!("[{}] {msg}", self.base.id);
            self.base
                .set_status(ComponentStatus::Error, Some(msg.to_string()))
                .await;
            anyhow::bail!(msg);
        }

        let client = match build_client(self.config.timeout_ms) {
            Ok(c) => c,
            Err(e) => {
                error!("[{}] Failed to create HTTP client: {e}", self.base.id);
                self.base
                    .set_status(
                        ComponentStatus::Error,
                        Some(format!("Failed to create HTTP client: {e}")),
                    )
                    .await;
                return Err(e);
            }
        };

        self.base
            .set_status(
                ComponentStatus::Starting,
                Some("Starting Event Grid reaction".to_string()),
            )
            .await;

        let mut shutdown_rx = self.base.create_shutdown_channel().await;
        let base = self.base.clone_shared();
        let config = self.config.clone();
        let reaction_name = self.base.id.clone();
        let status_handle = self.base.status_handle();

        let handle = tokio::spawn(async move {
            let handlebars = build_handlebars();
            let content_type = config.schema.content_type();

            status_handle
                .set_status(
                    ComponentStatus::Running,
                    Some("Event Grid reaction started".to_string()),
                )
                .await;

            loop {
                let query_result_arc = tokio::select! {
                    biased;
                    _ = &mut shutdown_rx => {
                        debug!("[{reaction_name}] Shutdown signal received");
                        break;
                    }
                    result = base.priority_queue.dequeue() => result,
                };
                let query_result = query_result_arc.as_ref();

                if query_result.results.is_empty() {
                    continue;
                }

                let events = build_events(
                    &reaction_name,
                    &config,
                    &handlebars,
                    query_result,
                    &reaction_name,
                );
                if events.is_empty() {
                    continue;
                }

                let body = encode_batch(&events, config.schema, &reaction_name);

                let auth = match resolve_auth_with_retry(
                    &config,
                    &base,
                    &reaction_name,
                    &mut shutdown_rx,
                )
                .await
                {
                    AuthOutcome::Ok(a) => a,
                    AuthOutcome::Shutdown => break,
                };

                if let Err(e) = publish(
                    &client,
                    &config.endpoint,
                    &auth,
                    content_type,
                    &body,
                    &reaction_name,
                )
                .await
                {
                    error!(
                        "[{reaction_name}] Failed to publish {} event(s) for query '{}': {e:#}",
                        events.len(),
                        query_result.query_id
                    );
                } else {
                    debug!(
                        "[{reaction_name}] Published {} event(s) for query '{}'",
                        events.len(),
                        query_result.query_id
                    );
                }
            }

            info!("[{reaction_name}] Event Grid reaction stopped");
            status_handle
                .set_status(
                    ComponentStatus::Stopped,
                    Some("Event Grid reaction processing task stopped".to_string()),
                )
                .await;
        });

        self.base.set_processing_task(handle).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await?;
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Event Grid reaction stopped".to_string()),
            )
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }
}
