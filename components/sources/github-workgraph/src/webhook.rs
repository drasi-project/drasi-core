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

use crate::agent_sync::{push_touches_agent_file, AgentSync, AgentSyncError};
use crate::config::{LeaseTrust, RepositoryFilter, TaskIssueType, WorkflowDefinitionConfig};
use crate::lease_ledger::Allocator;
use crate::mapping::{ConvertError, Converter};
use crate::vnext::WorkGraphProjector;
use anyhow::{anyhow, Context, Result};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use hmac::{Hmac, Mac};
use log::{debug, error, info, warn};
use serde::Deserialize;
use serde_json::json;
use sha2::Sha256;
use std::sync::Arc;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::{Mutex, Notify};

type HmacSha256 = Hmac<Sha256>;

pub struct IngressParams {
    pub source_id: String,
    pub organization: String,
    pub repository_filter: RepositoryFilter,
    pub task_issue_type: TaskIssueType,
    pub lease_trust: Option<LeaseTrust>,
    pub path: String,
    pub secret: String,
    pub lease_validation_token: String,
    pub body_limit_bytes: usize,
    pub allocator: Arc<Allocator>,
    pub agent_sync: Option<Arc<AgentSync>>,
    pub projector: Option<Arc<dyn WorkGraphProjector>>,
    pub workflow_definition: Option<WorkflowDefinitionConfig>,
    pub notify: Arc<Notify>,
    pub shutdown: tokio::sync::watch::Receiver<bool>,
}

struct IngressState {
    source_id: String,
    organization: String,
    repository_filter: RepositoryFilter,
    task_issue_type: TaskIssueType,
    lease_trust: Option<LeaseTrust>,
    secret: Vec<u8>,
    lease_validation_token: Vec<u8>,
    allocator: Arc<Allocator>,
    agent_sync: Option<Arc<AgentSync>>,
    projector: Option<Arc<dyn WorkGraphProjector>>,
    workflow_definition: Option<WorkflowDefinitionConfig>,
    projection_gate: Mutex<()>,
    notify: Arc<Notify>,
}

pub async fn serve(listener: TcpListener, params: IngressParams) -> Result<()> {
    let state = Arc::new(IngressState {
        source_id: params.source_id,
        organization: params.organization,
        repository_filter: params.repository_filter,
        task_issue_type: params.task_issue_type,
        lease_trust: params.lease_trust,
        secret: params.secret.into_bytes(),
        lease_validation_token: params.lease_validation_token.into_bytes(),
        allocator: params.allocator,
        agent_sync: params.agent_sync,
        projector: params.projector,
        workflow_definition: params.workflow_definition,
        projection_gate: Mutex::new(()),
        notify: params.notify,
    });
    let validation_path = format!("{}/lease/validate", params.path.trim_end_matches('/'));
    let router = Router::new()
        .route(&params.path, post(handler))
        .route(&validation_path, post(validate_lease))
        .layer(axum::extract::DefaultBodyLimit::max(
            params.body_limit_bytes,
        ))
        .with_state(state);
    let mut shutdown = params.shutdown;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            let _ = shutdown.changed().await;
        })
        .await
        .context("GitHub WorkGraph webhook server exited with error")
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LeaseValidation {
    task_node_id: String,
    lease_id: String,
    assignment_comment_node_id: String,
    agent_id: String,
    slot_id: String,
}

type Rejection = (StatusCode, String);

fn reject<T>(code: StatusCode, message: impl Into<String>) -> Result<T, Rejection> {
    Err((code, message.into()))
}

fn store_unavailable(source_id: &str, error: impl std::fmt::Display) -> Rejection {
    error!("[{source_id}] delivery dedupe store unavailable: {error}");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "delivery dedupe store unavailable".to_string(),
    )
}

async fn handler(
    State(state): State<Arc<IngressState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match handle_delivery(&state, &headers, &body).await {
        Ok(Some(count)) => (
            StatusCode::ACCEPTED,
            Json(json!({ "status": "accepted", "changes": count })),
        )
            .into_response(),
        Ok(None) => StatusCode::NO_CONTENT.into_response(),
        Err((code, message)) => (code, Json(json!({ "error": message }))).into_response(),
    }
}

async fn validate_lease(
    State(state): State<Arc<IngressState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let authorized = header(&headers, "authorization")
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| {
            state
                .lease_validation_token
                .as_slice()
                .ct_eq(token.as_bytes())
                .unwrap_u8()
                == 1
        });
    if !authorized {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let request: LeaseValidation = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    match state
        .allocator
        .validate_active(
            &request.task_node_id,
            &request.lease_id,
            &request.assignment_comment_node_id,
            &request.agent_id,
            &request.slot_id,
            chrono::Utc::now(),
        )
        .await
    {
        Ok(Some(active)) => (
            StatusCode::OK,
            Json(json!({
                "leaseId": active.lease_id, "taskNodeId": active.task_node_id,
                "assignmentCommentNodeId": active.assignment_comment_node_id,
                "agentId": active.agent_id, "slotId": active.slot_id,
                "taskType": active.task_type, "acquiredAt": active.acquired_at,
                "expiresAt": active.expires_at
            })),
        )
            .into_response(),
        Ok(None) => StatusCode::CONFLICT.into_response(),
        Err(error) => {
            error!(
                "[{}] allocator validation failed: {error:#}",
                state.source_id
            );
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn handle_delivery(
    state: &IngressState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<Option<usize>, Rejection> {
    let unauthorized = |m: String| (StatusCode::UNAUTHORIZED, m);
    let bad_request = |m: String| (StatusCode::BAD_REQUEST, m);
    let signature = header(headers, "x-hub-signature-256")
        .ok_or_else(|| unauthorized("missing X-Hub-Signature-256 header".to_string()))?;
    verify_signature(&state.secret, body, signature).map_err(|e| unauthorized(e.to_string()))?;
    let delivery_id = header(headers, "x-github-delivery")
        .ok_or_else(|| bad_request("missing X-GitHub-Delivery header".to_string()))?;
    let event_type = header(headers, "x-github-event")
        .ok_or_else(|| bad_request("missing X-GitHub-Event header".to_string()))?;
    let payload: serde_json::Value = serde_json::from_slice(body)
        .map_err(|e| bad_request(format!("body is not valid JSON: {e}")))?;
    let source_id = &state.source_id;
    if event_type == "ping" {
        debug!("[{source_id}] ping delivery {delivery_id} acknowledged");
        return Ok(None);
    }
    if event_type == "push" {
        return handle_push(state, delivery_id, &payload).await;
    }
    let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;

    // ── VNext normalization attempt ──────────────────────────────────
    // If a projector is available, try to normalize the event as a VNext
    // input. If it matches, project through the projector and return early.
    // Otherwise, fall through to the existing Converter.
    if let Some(projector) = &state.projector {
        let _projection_guard = state.projection_gate.lock().await;
        match try_vnext_normalization(state, event_type, &payload).await {
            Ok(Some(inputs)) => {
                let origin_id = format!("delivery:{delivery_id}:vnext");
                let (appended, rejection) = state
                    .allocator
                    .ingest_vnext(projector.as_ref(), inputs, effective_from, &origin_id)
                    .await
                    .map_err(|error| store_unavailable(source_id, error))?;
                if let Some(rejection) = &rejection {
                    warn!(
                        "[{source_id}] delivery {delivery_id} VNext projection rejected: \
                         {rejection}"
                    );
                }
                if appended > 0 {
                    state.notify.notify_one();
                }
                return Ok(Some(appended));
            }
            Ok(None) => {
                // Not a VNext event; fall through to existing Converter.
            }
            Err(VNextNormError::Untrusted(msg)) => {
                warn!("[{source_id}] delivery {delivery_id} untrusted VNext lifecycle: {msg}");
                return Ok(None);
            }
            Err(VNextNormError::Forbidden(msg)) => {
                warn!("[{source_id}] rejected delivery {delivery_id}: {msg}");
                return reject(StatusCode::FORBIDDEN, msg);
            }
            Err(VNextNormError::InvalidPayload(msg)) => {
                warn!("[{source_id}] delivery {delivery_id} invalid VNext payload: {msg}");
                return reject(StatusCode::UNPROCESSABLE_ENTITY, msg);
            }
        }
    }

    let converter = Converter::new(
        source_id,
        &state.organization,
        &state.task_issue_type,
        effective_from,
    )
    .with_repository_filter(&state.repository_filter);
    let converter = match &state.lease_trust {
        Some(lease_trust) => converter.with_lease_trust(lease_trust),
        None => converter,
    };
    let conversion = match converter.convert(event_type, &payload) {
        Ok(Some(conversion)) => conversion,
        Ok(None) => {
            debug!("[{source_id}] delivery {delivery_id} ({event_type}) has no graph change");
            return Ok(None);
        }
        Err(ConvertError::OrganizationMismatch(m)) => {
            warn!("[{source_id}] rejected delivery {delivery_id}: {m}");
            return reject(StatusCode::FORBIDDEN, m);
        }
        Err(ConvertError::InvalidPayload(m)) => {
            warn!("[{source_id}] invalid delivery {delivery_id}: {m}");
            return reject(StatusCode::UNPROCESSABLE_ENTITY, m);
        }
    };
    let changes = conversion.changes;
    let appended = match conversion.allocation {
        Some(event) => state
            .allocator
            .ingest(delivery_id, event, changes, effective_from)
            .await
            .map(|(appended, _)| appended),
        None => state.allocator.append_delivery(delivery_id, &changes).await,
    }
    .map_err(|error| store_unavailable(source_id, error))?;
    if appended > 0 {
        state.notify.notify_one();
    }
    Ok(Some(appended))
}

/// Converge the agent graph when a `push` touched the exact configured
/// repository, ref, and path.
///
/// A push that is not about the agent file is acknowledged with no content;
/// the Source models no other repository-content state.
async fn handle_push(
    state: &IngressState,
    delivery_id: &str,
    payload: &serde_json::Value,
) -> Result<Option<usize>, Rejection> {
    let source_id = &state.source_id;

    // Determine what this push touches.
    let touches_agent = state.agent_sync.as_ref().and_then(|sync| {
        let location = sync.location();
        let repo = payload
            .pointer("/repository/full_name")
            .and_then(serde_json::Value::as_str)?;
        let pushed_ref = payload.get("ref").and_then(serde_json::Value::as_str)?;
        if location.matches_push(repo, pushed_ref)
            && push_touches_agent_file(payload, &location.path)
        {
            Some(sync.clone())
        } else {
            None
        }
    });

    let touches_definition = state.workflow_definition.as_ref().is_some_and(|wf_config| {
        let repo = payload
            .pointer("/repository/full_name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let pushed_ref = payload
            .get("ref")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let location = wf_config.location();
        location.matches_push(repo, pushed_ref)
            && push_touches_definition_file(payload, &wf_config.path)
    });

    if touches_agent.is_none() && !touches_definition {
        debug!("[{source_id}] push delivery {delivery_id} does not touch agent or definition file");
        return Ok(None);
    }

    // Organization check.
    let organization = payload
        .pointer("/organization/login")
        .and_then(serde_json::Value::as_str);
    if let Some(login) = organization {
        if !login.eq_ignore_ascii_case(&state.organization) {
            let message = format!(
                "delivery organization '{login}' does not match configured organization '{}'",
                state.organization
            );
            warn!("[{source_id}] rejected delivery {delivery_id}: {message}");
            return reject(StatusCode::FORBIDDEN, message);
        }
    }

    // Single delivery dedup for push touching agent AND/OR definition.
    if state
        .allocator
        .completed(delivery_id)
        .await
        .map_err(|error| store_unavailable(source_id, error))?
    {
        debug!("[{source_id}] delivery {delivery_id} already completed; not re-appended");
        return Ok(Some(0));
    }

    let mut total_appended = 0usize;

    // Converge the durable definition sub-projection first. If the subsequent
    // agent convergence fails, redelivery skips this origin record and resumes
    // the unfinished agent side without duplicating projector state or WAL.
    if touches_definition {
        let _projection_guard = state.projection_gate.lock().await;
        match converge_definition_on_push(state, delivery_id, payload).await {
            Ok(Some(n)) => total_appended += n,
            Ok(None) => {}
            Err(rejection) => return Err(rejection),
        }
    }

    // Converge agent file if touched.
    if let Some(agent_sync) = touches_agent {
        let outcome = match agent_sync.converge().await {
            Ok(outcome) => outcome,
            Err(error @ AgentSyncError::Unavailable(_)) => {
                error!("[{source_id}] agent file convergence failed: {error}");
                return reject(
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!("agent file unavailable; redeliver later: {error}"),
                );
            }
            Err(error @ AgentSyncError::Storage(_)) => {
                error!("[{source_id}] agent file convergence failed: {error}");
                return reject(StatusCode::SERVICE_UNAVAILABLE, error.to_string());
            }
        };
        total_appended += outcome.appended;
        info!(
            "[{source_id}] push delivery {delivery_id} converged agent ({} change(s), \
             accepted={})",
            outcome.appended, outcome.accepted
        );
    }

    // Single delivery marker for both convergences.
    state
        .allocator
        .mark_completed(delivery_id)
        .await
        .map_err(|error| store_unavailable(source_id, error))?;

    if total_appended > 0 {
        state.notify.notify_one();
    }
    Ok(Some(total_appended))
}

// ── VNext push definition convergence ────────────────────────────────────

/// Check whether the push payload modified the exact configured definition
/// file path (not just the same repository/ref).
pub fn push_touches_definition_file(payload: &serde_json::Value, path: &str) -> bool {
    push_touches_agent_file(payload, path.trim_start_matches('/'))
}

/// If a push delivery touches the workflow definition file, converge the
/// definition through the projector. Returns the number of changes
/// appended, or `None` if the push didn't touch the definition file.
async fn converge_definition_on_push(
    state: &IngressState,
    delivery_id: &str,
    payload: &serde_json::Value,
) -> Result<Option<usize>, Rejection> {
    let source_id = &state.source_id;
    let (Some(projector), Some(wf_config)) = (&state.projector, &state.workflow_definition) else {
        return Ok(None);
    };
    let repository = payload
        .pointer("/repository/full_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let pushed_ref = payload
        .get("ref")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let location = wf_config.location();
    if !location.matches_push(repository, pushed_ref) {
        return Ok(None);
    }
    if !push_touches_definition_file(payload, &wf_config.path) {
        return Ok(None);
    }
    debug!(
        "[{source_id}] push delivery {delivery_id} touches workflow definition file '{}'",
        wf_config.path
    );

    use crate::agent_client::AgentFileClient;
    use crate::vnext::{definition_source_key, DefinitionDocument, ProjectionInput};

    let client = AgentFileClient::new(&wf_config.token, &wf_config.api_base_url)
        .map_err(|e| store_unavailable(source_id, e))?;
    let content = match client.fetch(&location).await {
        Ok(content) => content,
        Err(error) => {
            // Fetch failure is retryable — never project deletion/empty.
            error!(
                "[{source_id}] failed to fetch workflow definition on push: {error}; \
                 requesting redeliver"
            );
            return reject(
                StatusCode::SERVICE_UNAVAILABLE,
                format!("definition file unavailable; redeliver later: {error}"),
            );
        }
    };
    let source_key =
        definition_source_key(&wf_config.repository, &wf_config.r#ref, &wf_config.path);
    let effective_from = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let origin_id = format!("push:{delivery_id}:definition:{source_key}");

    let input = ProjectionInput::UpsertDefinition(DefinitionDocument {
        source_key: source_key.clone(),
        body: content.text,
    });

    let (appended, rejection) = state
        .allocator
        .ingest_vnext(projector.as_ref(), vec![input], effective_from, &origin_id)
        .await
        .map_err(|e| store_unavailable(source_id, e))?;

    if let Some(rejection) = &rejection {
        warn!("[{source_id}] push definition projection rejected: {rejection}");
    }
    info!(
        "[{source_id}] push delivery {delivery_id} converged definition '{source_key}' \
         ({appended} change(s))"
    );
    Ok(Some(appended))
}

// ── VNext event normalization ────────────────────────────────────────────

#[derive(Debug)]
enum VNextNormError {
    Untrusted(String),
    Forbidden(String),
    InvalidPayload(String),
}

/// Attempt to normalize a webhook event as VNext input(s).
///
/// Returns `Ok(Some(inputs))` if the event matched VNext patterns,
/// `Ok(None)` if it should fall through to the existing Converter,
/// or `Err` for VNext-specific rejections (untrusted, invalid).
async fn try_vnext_normalization(
    state: &IngressState,
    event_type: &str,
    payload: &serde_json::Value,
) -> Result<Option<Vec<crate::vnext::ProjectionInput>>, VNextNormError> {
    use crate::vnext::*;

    match event_type {
        "issues" => try_vnext_issue(state, payload).await,
        "issue_comment" => try_vnext_comment(state, payload),
        "sub_issues" => try_vnext_sub_issue(state, payload).await,
        _ => Ok(None),
    }
}

fn authorize_vnext_repository(
    state: &IngressState,
    payload: &serde_json::Value,
    repository: Option<&serde_json::Value>,
) -> Result<(), VNextNormError> {
    let login = payload
        .pointer("/organization/login")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            VNextNormError::InvalidPayload(
                "payload has no 'organization.login'; configure an organization webhook"
                    .to_string(),
            )
        })?;
    if !login.eq_ignore_ascii_case(&state.organization) {
        return Err(VNextNormError::Forbidden(format!(
            "delivery organization '{login}' does not match configured organization '{}'",
            state.organization
        )));
    }
    let repository = repository
        .or_else(|| payload.get("repository"))
        .ok_or_else(|| VNextNormError::InvalidPayload("missing 'repository'".to_string()))?;
    let included = state
        .repository_filter
        .includes_repository(repository)
        .map_err(|error| VNextNormError::InvalidPayload(error.to_string()))?;
    if !included {
        return Err(VNextNormError::Forbidden(
            "delivery repository is outside the configured repository filter".to_string(),
        ));
    }
    Ok(())
}

/// Normalize an issue event with a `WorkGraphTask/v3` body marker.
async fn try_vnext_issue(
    state: &IngressState,
    payload: &serde_json::Value,
) -> Result<Option<Vec<crate::vnext::ProjectionInput>>, VNextNormError> {
    use crate::vnext::*;

    let action = payload
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let issue = payload.get("issue").ok_or_else(|| {
        VNextNormError::InvalidPayload("issues event missing 'issue'".to_string())
    })?;
    let body = issue
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let node_id = issue
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| VNextNormError::InvalidPayload("issue missing 'node_id'".to_string()))?;

    // Only normalize issues whose body begins with the v3 marker.
    let current_vnext = body.starts_with(VNEXT_TASK_MARKER);
    let previous_vnext = payload
        .pointer("/changes/body/from")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|body| body.starts_with(VNEXT_TASK_MARKER));
    if !current_vnext && !previous_vnext {
        return Ok(None);
    }

    authorize_vnext_repository(state, payload, None)?;
    let removed_type_matches = payload
        .get("type")
        .is_some_and(|issue_type| state.task_issue_type.matches(Some(issue_type)));
    if !state.task_issue_type.matches(issue.get("type"))
        && !matches!(action, "untyped" | "deleted" | "transferred")
        && !removed_type_matches
    {
        return Ok(None);
    }

    let mut inputs = Vec::new();

    match action {
        "deleted" | "transferred" | "untyped" => {
            // Delete the task.
            inputs.push(ProjectionInput::DeleteTask {
                source_key: node_id.to_string(),
            });
            inputs.push(ProjectionInput::DeleteLocator {
                source_key: node_id.to_string(),
            });
        }
        _ if current_vnext => {
            // opened, edited, closed, reopened, labeled, unlabeled, etc.
            let is_open = item_is_open(issue);
            let state_reason = issue
                .get("state_reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string();
            let previous = state
                .allocator
                .latest_vnext_task(node_id)
                .await
                .map_err(|error| VNextNormError::InvalidPayload(error.to_string()))?;
            let task_doc = TaskDocument {
                source_key: node_id.to_string(),
                body: body.to_string(),
                is_open,
                state_reason,
                parent_source_key: previous.and_then(|document| document.parent_source_key),
            };
            inputs.push(ProjectionInput::UpsertTask(task_doc));

            // Also emit a locator so Dogfood can resolve the issue.
            if let Some(locator) = extract_issue_locator(issue, payload) {
                inputs.push(ProjectionInput::UpsertLocator(locator));
            }
        }
        _ => {
            inputs.push(ProjectionInput::DeleteTask {
                source_key: node_id.to_string(),
            });
            inputs.push(ProjectionInput::DeleteLocator {
                source_key: node_id.to_string(),
            });
        }
    }

    Ok(Some(inputs))
}

/// Normalize an issue_comment event with a VNext lifecycle marker.
fn try_vnext_comment(
    state: &IngressState,
    payload: &serde_json::Value,
) -> Result<Option<Vec<crate::vnext::ProjectionInput>>, VNextNormError> {
    use crate::vnext::*;

    let action = payload
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let comment = payload.get("comment").ok_or_else(|| {
        VNextNormError::InvalidPayload("issue_comment event missing 'comment'".to_string())
    })?;
    let comment_id = comment
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| VNextNormError::InvalidPayload("comment missing 'node_id'".to_string()))?;
    let body = comment
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    // Only handle VNext lifecycle markers.
    let trust_role = match lifecycle_trust_role(body) {
        Some(role) => role,
        None => return Ok(None),
    };

    let issue = payload.get("issue").ok_or_else(|| {
        VNextNormError::InvalidPayload("issue_comment missing 'issue'".to_string())
    })?;
    let task_node_id = issue
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| VNextNormError::InvalidPayload("issue missing 'node_id'".to_string()))?;
    let task_body = issue
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !task_body.starts_with(VNEXT_TASK_MARKER)
        || !state.task_issue_type.matches(issue.get("type"))
    {
        return Ok(None);
    }
    authorize_vnext_repository(state, payload, None)?;

    // Trust check: reuse the existing anti-confused-deputy logic.
    if let Some(lease_trust) = &state.lease_trust {
        let author = comment.get("user");
        // On edit, sender is the editor.
        let editor_identity = (action == "edited")
            .then(|| payload.get("sender"))
            .flatten()
            .filter(|s| !s.is_null());
        let editor = comment
            .get("editor")
            .filter(|e| !e.is_null())
            .or(editor_identity);
        let unattributed_edit =
            editor.is_none() && comment.get("updated_at") != comment.get("created_at");

        let role_check: fn(&LeaseTrust, Option<&serde_json::Value>) -> bool = match trust_role {
            LifecycleTrustRole::Assigner => |trust, identity| trust.is_assigner(identity),
            LifecycleTrustRole::Reporter => |trust, identity| trust.is_reporter(identity),
        };

        let trusted = !unattributed_edit
            && role_check(lease_trust, author)
            && editor.is_none_or(|e| role_check(lease_trust, Some(e)));

        if !trusted {
            let role_name = match trust_role {
                LifecycleTrustRole::Assigner => "assigner",
                LifecycleTrustRole::Reporter => "reporter",
            };
            return Err(VNextNormError::Untrusted(format!(
                "VNext lifecycle comment {comment_id} on {task_node_id}: author/editor not \
                 trusted as {role_name}"
            )));
        }
    } else {
        return Err(VNextNormError::Untrusted(format!(
            "VNext lifecycle comment {comment_id} on {task_node_id}: no protocolTrust configured"
        )));
    }

    let mut inputs = Vec::new();

    match action {
        "deleted" => {
            inputs.push(ProjectionInput::DeleteLifecycleArtifact {
                source_key: comment_id.to_string(),
            });
        }
        _ => {
            let artifact = LifecycleArtifactDocument {
                source_key: comment_id.to_string(),
                task_source_key: task_node_id.to_string(),
                body: body.to_string(),
            };
            inputs.push(ProjectionInput::UpsertLifecycleArtifact(artifact));
        }
    }

    Ok(Some(inputs))
}

/// Normalize a sub_issues event (add/remove child parent relationship).
async fn try_vnext_sub_issue(
    state: &IngressState,
    payload: &serde_json::Value,
) -> Result<Option<Vec<crate::vnext::ProjectionInput>>, VNextNormError> {
    use crate::vnext::*;

    let action = payload
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    // sub_issues events have parent_issue and sub_issue.
    let parent_issue = payload.get("parent_issue");
    let sub_issue = payload.get("sub_issue");

    let (parent_issue, sub_issue) = match (parent_issue, sub_issue) {
        (Some(p), Some(s)) => (p, s),
        _ => return Ok(None),
    };

    let child_node_id = sub_issue
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| VNextNormError::InvalidPayload("sub_issue missing 'node_id'".to_string()))?;
    let parent_node_id = parent_issue
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            VNextNormError::InvalidPayload("parent_issue missing 'node_id'".to_string())
        })?;
    let child_repository = payload
        .get("sub_issue_repo")
        .or_else(|| payload.get("repository"));
    authorize_vnext_repository(state, payload, child_repository)?;
    let previous = state
        .allocator
        .latest_vnext_task(child_node_id)
        .await
        .map_err(|error| VNextNormError::InvalidPayload(error.to_string()))?;
    if !state.task_issue_type.matches(sub_issue.get("type")) && previous.is_none() {
        return Ok(None);
    }

    let parent_source_key = match action {
        "sub_issue_added" | "parent_issue_added" => Some(parent_node_id.to_string()),
        "sub_issue_removed" | "parent_issue_removed" => None,
        _ => return Ok(None),
    };

    let child_body = sub_issue
        .get("body")
        .and_then(serde_json::Value::as_str)
        .filter(|body| body.starts_with(VNEXT_TASK_MARKER))
        .map(str::to_string)
        .or_else(|| previous.as_ref().map(|document| document.body.clone()));
    let Some(child_body) = child_body else {
        return Ok(None);
    };
    let is_open = sub_issue.get("state").map_or_else(
        || previous.as_ref().is_none_or(|document| document.is_open),
        |_| item_is_open(sub_issue),
    );
    let state_reason = sub_issue
        .get("state_reason")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .or_else(|| {
            previous
                .as_ref()
                .map(|document| document.state_reason.clone())
        })
        .unwrap_or_default();

    let task_doc = TaskDocument {
        source_key: child_node_id.to_string(),
        body: child_body,
        is_open,
        state_reason,
        parent_source_key,
    };

    let mut inputs = Vec::new();
    inputs.push(ProjectionInput::UpsertTask(task_doc));
    // Also emit a locator for the child.
    if let Some(locator) = extract_issue_locator_from_repository(sub_issue, child_repository) {
        inputs.push(ProjectionInput::UpsertLocator(locator));
    }

    Ok(Some(inputs))
}

/// Check if a GitHub issue/PR is in an open state.
pub fn item_is_open(item: &serde_json::Value) -> bool {
    item.get("state")
        .and_then(serde_json::Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("open"))
        .unwrap_or(true)
}

/// Extract a `GitHubIssueLocator` from an issue payload.
pub fn extract_issue_locator(
    issue: &serde_json::Value,
    payload: &serde_json::Value,
) -> Option<crate::vnext::GitHubIssueLocator> {
    extract_issue_locator_from_repository(issue, payload.get("repository"))
}

fn extract_issue_locator_from_repository(
    issue: &serde_json::Value,
    repository: Option<&serde_json::Value>,
) -> Option<crate::vnext::GitHubIssueLocator> {
    let node_id = issue.get("node_id")?.as_str()?;
    let number = issue.get("number")?.as_u64()?;
    let repo = repository?;
    let full_name = repo.get("full_name")?.as_str()?;
    let (owner, name) = full_name.split_once('/')?;
    Some(crate::vnext::GitHubIssueLocator {
        source_key: node_id.to_string(),
        repository_owner: owner.to_string(),
        repository_name: name.to_string(),
        issue_number: number,
        issue_node_id: node_id.to_string(),
    })
}

fn header<'a>(headers: &'a HeaderMap, key: &str) -> Option<&'a str> {
    headers
        .get(key)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub fn verify_signature(secret: &[u8], body: &[u8], signature_header: &str) -> Result<()> {
    let expected_hex = signature_header
        .strip_prefix("sha256=")
        .ok_or_else(|| anyhow!("signature must start with 'sha256='"))?;
    let expected = hex::decode(expected_hex).context("signature hex decode failed")?;
    let mut mac = HmacSha256::new_from_slice(secret).context("invalid HMAC secret")?;
    mac.update(body);
    let actual = mac.finalize().into_bytes().to_vec();
    if actual.as_slice().ct_eq(expected.as_slice()).unwrap_u8() == 1 {
        Ok(())
    } else {
        Err(anyhow!("signature mismatch"))
    }
}

#[cfg(test)]
mod vnext_tests {
    use super::*;
    use crate::config::{TaskIssueType, TrustedIdentity};
    use crate::vnext::{
        PreparedProjection, PreparedProjectionCommit, ProjectionInput, TaskDocument,
    };
    use async_trait::async_trait;
    use drasi_core::models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
    };
    use drasi_lib::wal::{WalError, WalProvider, WriteAheadLogConfig};
    use drasi_lib::MemoryStateStoreProvider;
    use drasi_wal_redb::RedbWalProvider;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::Mutex;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[derive(Default)]
    struct RecordingProjector {
        committed: Arc<Mutex<Vec<Vec<ProjectionInput>>>>,
        restored: Arc<Mutex<Vec<Vec<u8>>>>,
        change_count: usize,
    }

    struct RecordingCommit {
        inputs: Vec<ProjectionInput>,
        committed: Arc<Mutex<Vec<Vec<ProjectionInput>>>>,
    }

    #[async_trait]
    impl PreparedProjectionCommit for RecordingCommit {
        async fn commit(self: Box<Self>) {
            self.committed.lock().await.push(self.inputs);
        }
    }

    #[async_trait]
    impl WorkGraphProjector for RecordingProjector {
        async fn prepare(
            &self,
            inputs: Vec<ProjectionInput>,
            _effective_from: u64,
        ) -> anyhow::Result<PreparedProjection> {
            let checkpoint = serde_json::to_vec(&inputs)?;
            let changes = (0..self.change_count)
                .map(|index| SourceChange::Insert {
                    element: Element::Node {
                        metadata: ElementMetadata {
                            reference: ElementReference::new(
                                "source",
                                &format!("projected-{index}"),
                            ),
                            labels: vec![Arc::from("Projected")].into(),
                            effective_from: 1,
                        },
                        properties: ElementPropertyMap::new(),
                    },
                })
                .collect();
            Ok(PreparedProjection {
                changes,
                rejection: None,
                state_changed: true,
                checkpoint,
                commit: Box::new(RecordingCommit {
                    inputs,
                    committed: self.committed.clone(),
                }),
            })
        }

        async fn restore(&self, checkpoint: &[u8]) -> anyhow::Result<()> {
            self.restored.lock().await.push(checkpoint.to_vec());
            Ok(())
        }

        fn source_id(&self) -> &str {
            "source"
        }
    }

    struct FailOnceWal {
        inner: Arc<RedbWalProvider>,
        append_calls: AtomicUsize,
    }

    #[async_trait]
    impl WalProvider for FailOnceWal {
        async fn register(
            &self,
            source_id: &str,
            config: WriteAheadLogConfig,
        ) -> Result<(), WalError> {
            self.inner.register(source_id, config).await
        }

        async fn append(&self, source_id: &str, event: &SourceChange) -> Result<u64, WalError> {
            if self.append_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                return Err(WalError::StorageError("injected failure".to_string()));
            }
            self.inner.append(source_id, event).await
        }

        async fn read_from(
            &self,
            source_id: &str,
            sequence: u64,
        ) -> Result<Vec<(u64, SourceChange)>, WalError> {
            self.inner.read_from(source_id, sequence).await
        }

        async fn prune_up_to(&self, source_id: &str, sequence: u64) -> Result<u64, WalError> {
            self.inner.prune_up_to(source_id, sequence).await
        }

        async fn head_sequence(&self, source_id: &str) -> Result<u64, WalError> {
            self.inner.head_sequence(source_id).await
        }

        async fn oldest_sequence(&self, source_id: &str) -> Result<Option<u64>, WalError> {
            self.inner.oldest_sequence(source_id).await
        }

        async fn event_count(&self, source_id: &str) -> Result<u64, WalError> {
            self.inner.event_count(source_id).await
        }

        async fn delete_wal(&self, source_id: &str) -> Result<(), WalError> {
            self.inner.delete_wal(source_id).await
        }
    }

    async fn ingress_state(
        trust: Option<LeaseTrust>,
    ) -> (TempDir, Arc<RecordingProjector>, IngressState) {
        let temp = TempDir::new().expect("tempdir");
        let store = Arc::new(MemoryStateStoreProvider::new());
        let wal = Arc::new(RedbWalProvider::new(temp.path().join("wal")));
        wal.register("source", WriteAheadLogConfig::default())
            .await
            .expect("register WAL");
        let allocator = Arc::new(Allocator::new("source".to_string(), store, wal));
        let projector = Arc::new(RecordingProjector::default());
        (
            temp,
            projector.clone(),
            IngressState {
                source_id: "source".to_string(),
                organization: "acme".to_string(),
                repository_filter: RepositoryFilter::new("acme", &["widgets".to_string()])
                    .expect("repository filter"),
                task_issue_type: TaskIssueType {
                    id: "IT_task".to_string(),
                    name: "WorkGraphTask".to_string(),
                },
                lease_trust: trust,
                secret: b"secret".to_vec(),
                lease_validation_token: b"validation".to_vec(),
                allocator,
                agent_sync: None,
                projector: Some(projector.clone()),
                workflow_definition: None,
                projection_gate: Mutex::new(()),
                notify: Arc::new(Notify::new()),
            },
        )
    }

    fn task_issue(node_id: &str, body: &str) -> serde_json::Value {
        json!({
            "id": 42,
            "node_id": node_id,
            "number": 7,
            "body": body,
            "state": "open",
            "state_reason": null,
            "type": {"node_id": "IT_task", "name": "WorkGraphTask"}
        })
    }

    fn payload(action: &str, issue: serde_json::Value) -> serde_json::Value {
        json!({
            "action": action,
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "issue": issue
        })
    }

    #[tokio::test]
    async fn authorized_task_normalizes_as_one_task_and_locator_batch() {
        let (_temp, _projector, state) = ingress_state(None).await;
        let inputs = try_vnext_issue(
            &state,
            &payload(
                "opened",
                task_issue("I_task", "WorkGraphTask/v3\n\n```json\n{}\n```\n"),
            ),
        )
        .await
        .expect("normalize")
        .expect("VNext task");
        assert_eq!(inputs.len(), 2);
        assert!(matches!(&inputs[0], ProjectionInput::UpsertTask(_)));
        assert!(matches!(&inputs[1], ProjectionInput::UpsertLocator(locator)
            if locator.repository_owner == "acme"
                && locator.repository_name == "widgets"
                && locator.issue_number == 7));
    }

    #[tokio::test]
    async fn durable_projection_batch_is_committed_and_deduplicated_once() {
        let (_temp, projector, state) = ingress_state(None).await;
        let inputs = try_vnext_issue(
            &state,
            &payload(
                "opened",
                task_issue("I_task", "WorkGraphTask/v3\n\n```json\n{}\n```\n"),
            ),
        )
        .await
        .expect("normalize")
        .expect("VNext task");
        let first = state
            .allocator
            .ingest_vnext(projector.as_ref(), inputs.clone(), 7, "delivery-1")
            .await
            .expect("first projection");
        let duplicate = state
            .allocator
            .ingest_vnext(projector.as_ref(), inputs, 7, "delivery-1")
            .await
            .expect("duplicate projection");
        assert_eq!(first.0, 0);
        assert_eq!(duplicate.0, 0);
        assert_eq!(projector.committed.lock().await.len(), 1);
        let checkpoint = state
            .allocator
            .vnext_checkpoint()
            .await
            .expect("durable checkpoint");
        assert_eq!(
            serde_json::from_slice::<Vec<ProjectionInput>>(&checkpoint)
                .expect("decode checkpoint")
                .len(),
            2
        );
        let restarted = RecordingProjector::default();
        restarted
            .restore(&checkpoint)
            .await
            .expect("restore checkpoint");
        assert_eq!(restarted.restored.lock().await.as_slice(), &[checkpoint]);
    }

    #[tokio::test]
    async fn wal_failure_keeps_projector_uncommitted_until_pending_recovery() {
        let temp = TempDir::new().expect("tempdir");
        let store = Arc::new(MemoryStateStoreProvider::new());
        let inner = Arc::new(RedbWalProvider::new(temp.path().join("wal")));
        inner
            .register("source", WriteAheadLogConfig::default())
            .await
            .expect("register WAL");
        let wal = Arc::new(FailOnceWal {
            inner: inner.clone(),
            append_calls: AtomicUsize::new(0),
        });
        let allocator = Allocator::new("source".to_string(), store, wal);
        let projector = RecordingProjector {
            committed: Arc::default(),
            restored: Arc::default(),
            change_count: 2,
        };
        let input = ProjectionInput::DeleteTask {
            source_key: "I_task".to_string(),
        };

        assert!(allocator
            .ingest_vnext(&projector, vec![input], 7, "delivery-1")
            .await
            .is_err());
        assert!(projector.committed.lock().await.is_empty());

        let checkpoint = allocator
            .vnext_checkpoint()
            .await
            .expect("pending WAL recovery");
        assert!(!checkpoint.is_empty());
        assert_eq!(inner.event_count("source").await.expect("event count"), 2);
        assert_eq!(projector.committed.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn excluded_repository_is_rejected_before_projection() {
        let (_temp, _projector, state) = ingress_state(None).await;
        let mut event = payload(
            "opened",
            task_issue("I_task", "WorkGraphTask/v3\n\n```json\n{}\n```\n"),
        );
        event["repository"] = json!({"name": "other", "full_name": "acme/other"});
        assert!(matches!(
            try_vnext_issue(&state, &event).await,
            Err(VNextNormError::Forbidden(_))
        ));
    }

    #[tokio::test]
    async fn issue_update_preserves_parent_from_durable_projection_history() {
        let (_temp, projector, state) = ingress_state(None).await;
        let body = "WorkGraphTask/v3\n\n```json\n{}\n```\n";
        state
            .allocator
            .ingest_vnext(
                projector.as_ref(),
                vec![ProjectionInput::UpsertTask(TaskDocument {
                    source_key: "I_child".to_string(),
                    body: body.to_string(),
                    is_open: true,
                    state_reason: String::new(),
                    parent_source_key: Some("I_parent".to_string()),
                })],
                1,
                "seed",
            )
            .await
            .expect("seed task");

        let inputs = try_vnext_issue(&state, &payload("edited", task_issue("I_child", body)))
            .await
            .expect("normalize")
            .expect("VNext task");
        assert!(matches!(
            &inputs[0],
            ProjectionInput::UpsertTask(document)
                if document.parent_source_key.as_deref() == Some("I_parent")
        ));
    }

    #[tokio::test]
    async fn lifecycle_artifact_requires_trust_and_preserves_exact_body() {
        let body = "WorkGraphTaskAssign/v1\n\n```json\n{\"operationId\":\"op\"}\n```\n";
        let issue = task_issue("I_task", "WorkGraphTask/v3\n\n```json\n{}\n```\n");
        let event = json!({
            "action": "created",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "issue": issue,
            "comment": {
                "node_id": "IC_assign",
                "body": body,
                "user": {"node_id": "U_dispatch", "login": "dispatcher"},
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }
        });
        let (_temp, _projector, untrusted) = ingress_state(None).await;
        assert!(matches!(
            try_vnext_comment(&untrusted, &event),
            Err(VNextNormError::Untrusted(_))
        ));

        let trust = LeaseTrust {
            dispatchers: vec![TrustedIdentity {
                id: "U_dispatch".to_string(),
                login: "dispatcher".to_string(),
            }],
            reporters: Vec::new(),
        };
        let (_temp, _projector, trusted) = ingress_state(Some(trust)).await;
        let inputs = try_vnext_comment(&trusted, &event)
            .expect("normalize")
            .expect("lifecycle artifact");
        assert!(matches!(
            &inputs[0],
            ProjectionInput::UpsertLifecycleArtifact(document)
                if document.source_key == "IC_assign"
                    && document.task_source_key == "I_task"
                    && document.body == body
        ));
    }

    #[tokio::test]
    async fn sub_issue_removal_reuses_document_and_clears_parent() {
        let (_temp, projector, state) = ingress_state(None).await;
        let body = "WorkGraphTask/v3\n\n```json\n{}\n```\n";
        state
            .allocator
            .ingest_vnext(
                projector.as_ref(),
                vec![ProjectionInput::UpsertTask(TaskDocument {
                    source_key: "I_child".to_string(),
                    body: body.to_string(),
                    is_open: true,
                    state_reason: String::new(),
                    parent_source_key: Some("I_parent".to_string()),
                })],
                1,
                "seed",
            )
            .await
            .expect("seed task");
        let event = json!({
            "action": "parent_issue_removed",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "parent_issue": {"node_id": "I_parent"},
            "sub_issue": {
                "node_id": "I_child",
                "number": 7,
                "type": {"node_id": "IT_task", "name": "WorkGraphTask"}
            }
        });
        let inputs = try_vnext_sub_issue(&state, &event)
            .await
            .expect("normalize")
            .expect("sub-issue task");
        assert!(matches!(
            &inputs[0],
            ProjectionInput::UpsertTask(document)
                if document.body == body && document.parent_source_key.is_none()
        ));
    }

    #[tokio::test]
    async fn matching_push_fetches_and_projects_configured_definition() {
        let server = MockServer::start().await;
        let body = "WorkGraphWorkflowDefinition/v1\n\n```json\n{}\n```\n";
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "repository": {
                        "object": {
                            "__typename": "Blob",
                            "oid": "definition-oid",
                            "text": body,
                            "byteSize": body.len(),
                            "isTruncated": false,
                            "isBinary": false
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_temp, projector, mut state) = ingress_state(None).await;
        state.workflow_definition = Some(WorkflowDefinitionConfig {
            repository: "acme/widgets".to_string(),
            r#ref: "main".to_string(),
            path: ".github/workgraph/workflows/issue-lifecycle-vnext.body".to_string(),
            token: "token".to_string(),
            api_base_url: server.uri(),
        });
        let event = json!({
            "ref": "refs/heads/main",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "commits": [{
                "added": [],
                "modified": [".github/workgraph/workflows/issue-lifecycle-vnext.body"],
                "removed": []
            }],
            "size": 1
        });

        assert_eq!(
            converge_definition_on_push(&state, "push-1", &event)
                .await
                .expect("definition convergence"),
            Some(0)
        );
        let committed = projector.committed.lock().await;
        assert!(matches!(
            &committed[0][0],
            ProjectionInput::UpsertDefinition(document)
                if document.body == body
                    && document.source_key
                        == "github:definition:acme/widgets:main:.github/workgraph/workflows/issue-lifecycle-vnext.body"
        ));
    }
}
