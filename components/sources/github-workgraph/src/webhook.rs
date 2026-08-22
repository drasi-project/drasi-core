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
use crate::config::{LeaseTrust, RepositoryFilter, TaskIssueType};
use crate::lease_ledger::Allocator;
use crate::mapping::{ConvertError, Converter};
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
use tokio::sync::Notify;

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
    let Some(agent_sync) = &state.agent_sync else {
        debug!("[{source_id}] push delivery {delivery_id} ignored; no agent file is configured");
        return Ok(None);
    };
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
    let repository = payload
        .pointer("/repository/full_name")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "push delivery has no 'repository.full_name'".to_string(),
            )
        })?;
    let pushed_ref = payload
        .get("ref")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "push delivery has no 'ref'".to_string(),
            )
        })?;
    let location = agent_sync.location();
    if !location.matches_push(repository, pushed_ref) {
        debug!(
            "[{source_id}] push delivery {delivery_id} on {repository}@{pushed_ref} is not the \
             configured agent file location"
        );
        return Ok(None);
    }
    if !push_touches_agent_file(payload, &location.path) {
        debug!(
            "[{source_id}] push delivery {delivery_id} did not touch '{}'",
            location.path
        );
        return Ok(None);
    }

    if state
        .allocator
        .completed(delivery_id)
        .await
        .map_err(|error| store_unavailable(source_id, error))?
    {
        debug!("[{source_id}] delivery {delivery_id} already completed; not re-appended");
        return Ok(Some(0));
    }

    let outcome = match agent_sync.converge().await {
        Ok(outcome) => outcome,
        // An unreadable agent file proves nothing about configured capacity.
        // Ask GitHub to redeliver instead of asserting an agent pool state.
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

    state
        .allocator
        .mark_completed(delivery_id)
        .await
        .map_err(|error| store_unavailable(source_id, error))?;
    state.notify.notify_one();
    info!(
        "[{source_id}] push delivery {delivery_id} converged the agent graph ({} change(s), \
         accepted={})",
        outcome.appended, outcome.accepted
    );
    Ok(Some(outcome.appended))
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
