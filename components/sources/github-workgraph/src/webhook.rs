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
use crate::config::{ProtocolTrust, RepositoryFilter, TaskIssueType, WorkflowDefinitionConfig};
use crate::lease_ledger::Allocator;
use crate::protocol::WorkGraphProjector;
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
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
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
    pub protocol_trust: Option<ProtocolTrust>,
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
    protocol_trust: Option<ProtocolTrust>,
    secret: Vec<u8>,
    lease_validation_token: Vec<u8>,
    allocator: Arc<Allocator>,
    agent_sync: Option<Arc<AgentSync>>,
    projector: Option<Arc<dyn WorkGraphProjector>>,
    workflow_definition: Option<WorkflowDefinitionConfig>,
    admission_client: Option<AdmissionClient>,
    projection_gate: Mutex<()>,
    notify: Arc<Notify>,
}

pub async fn serve(listener: TcpListener, params: IngressParams) -> Result<()> {
    let admission_client = params
        .workflow_definition
        .as_ref()
        .map(AdmissionClient::new)
        .transpose()?;
    let state = Arc::new(IngressState {
        source_id: params.source_id,
        organization: params.organization,
        repository_filter: params.repository_filter,
        task_issue_type: params.task_issue_type,
        protocol_trust: params.protocol_trust,
        secret: params.secret.into_bytes(),
        lease_validation_token: params.lease_validation_token.into_bytes(),
        allocator: params.allocator,
        agent_sync: params.agent_sync,
        projector: params.projector,
        workflow_definition: params.workflow_definition,
        admission_client,
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
    task_id: String,
    lease_id: String,
    assignment_id: String,
    executor_id: String,
    slot_id: String,
    claim_id: String,
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
    if request.claim_id.is_empty()
        || request.claim_id.len() > 256
        || request.claim_id.chars().any(char::is_whitespace)
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    match state
        .allocator
        .claim_active(
            &request.task_id,
            &request.lease_id,
            &request.assignment_id,
            &request.executor_id,
            &request.slot_id,
            &request.claim_id,
            chrono::Utc::now(),
        )
        .await
    {
        Ok(Some(active)) => (
            StatusCode::OK,
            Json(json!({
                "leaseId": active.lease_id, "taskId": active.task_id,
                "assignmentId": active.assignment_id,
                "executorId": active.executor_id, "slotId": active.slot_id,
                "claimId": request.claim_id,
                "acquiredAt": active.acquired_at,
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

    // WorkGraph v1 is the sole issue/comment/sub-issue projection path.
    if let Some(projector) = &state.projector {
        let _projection_guard = state.projection_gate.lock().await;
        match try_workgraph_normalization(state, event_type, delivery_id, &payload).await {
            Ok(Some(inputs)) => {
                let origin_id = format!("delivery:{delivery_id}:workgraph");
                let (appended, rejection) = state
                    .allocator
                    .ingest_workgraph(projector.as_ref(), inputs, effective_from, &origin_id)
                    .await
                    .map_err(|error| store_unavailable(source_id, error))?;
                if let Some(rejection) = &rejection {
                    warn!(
                        "[{source_id}] delivery {delivery_id} WorkGraph projection rejected: \
                         {rejection}"
                    );
                }
                if appended > 0 {
                    state.notify.notify_one();
                }
                return Ok(Some(appended));
            }
            Ok(None) => return Ok(None),
            Err(WorkGraphNormError::Untrusted(msg)) => {
                warn!("[{source_id}] delivery {delivery_id} untrusted WorkGraph lifecycle: {msg}");
                return Ok(None);
            }
            Err(WorkGraphNormError::Forbidden(msg)) => {
                warn!("[{source_id}] rejected delivery {delivery_id}: {msg}");
                return reject(StatusCode::FORBIDDEN, msg);
            }
            Err(WorkGraphNormError::InvalidPayload(msg)) => {
                warn!("[{source_id}] delivery {delivery_id} invalid WorkGraph payload: {msg}");
                return reject(StatusCode::UNPROCESSABLE_ENTITY, msg);
            }
            Err(WorkGraphNormError::Unavailable(msg)) => {
                error!("[{source_id}] delivery {delivery_id} could not verify admission: {msg}");
                return reject(StatusCode::SERVICE_UNAVAILABLE, msg);
            }
        }
    }

    debug!(
        "[{source_id}] delivery {delivery_id} ({event_type}) ignored because no WorkGraph projector is configured"
    );
    Ok(None)
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

// ── WorkGraph push definition convergence ────────────────────────────────────

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
    use crate::protocol::{definition_source_key, DefinitionDocument, ProjectionInput};

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
        .ingest_workgraph(projector.as_ref(), vec![input], effective_from, &origin_id)
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

// ── WorkGraph event normalization ────────────────────────────────────────────

#[derive(Debug)]
enum WorkGraphNormError {
    Untrusted(String),
    Forbidden(String),
    InvalidPayload(String),
    Unavailable(String),
}

#[derive(Clone)]
struct AdmissionClient {
    http: reqwest::Client,
    api_url: String,
}

#[derive(Debug, PartialEq, Eq)]
struct TaskIssueState {
    document: crate::protocol::TaskDocument,
    task_typed: bool,
}

impl AdmissionClient {
    fn new(config: &WorkflowDefinitionConfig) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("bearer {}", config.token))
                .context("invalid GitHub token header value")?,
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("drasi-github-workgraph-admission"),
        );
        Ok(Self {
            http: reqwest::Client::builder()
                .default_headers(headers)
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .context("failed to build GitHub admission client")?,
            api_url: config.api_base_url.clone(),
        })
    }

    async fn is_root_candidate(
        &self,
        node_id: &str,
        task_issue_type: &crate::config::TaskIssueType,
    ) -> Result<bool> {
        let response = self
            .http
            .post(&self.api_url)
            .json(&json!({
                "query": "query($id: ID!) { node(id: $id) { ... on Issue { body issueType { id name } labels(first: 100) { nodes { name } pageInfo { hasNextPage } } } } }",
                "variables": {"id": node_id}
            }))
            .send()
            .await
            .context("authoritative Root Issue lookup failed")?
            .error_for_status()
            .context("authoritative Root Issue lookup returned an error")?;
        let payload: Value = response
            .json()
            .await
            .context("authoritative Root Issue lookup returned invalid JSON")?;
        if payload
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|errors| !errors.is_empty())
        {
            anyhow::bail!("authoritative Root Issue lookup returned GraphQL errors");
        }
        let issue = payload
            .pointer("/data/node")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("authoritative Root Issue lookup did not return an Issue"))?;
        let labels = issue
            .get("labels")
            .ok_or_else(|| anyhow!("authoritative Root Issue labels are missing"))?;
        let admitted = labels
            .get("nodes")
            .and_then(Value::as_array)
            .context("authoritative Root Issue labels are missing")?
            .iter()
            .any(|label| {
                label.get("name").and_then(Value::as_str) == Some(WORKGRAPH_ADMISSION_LABEL)
            });
        if labels
            .pointer("/pageInfo/hasNextPage")
            .and_then(Value::as_bool)
            == Some(true)
        {
            anyhow::bail!("authoritative Root Issue label set exceeds 100 entries");
        }
        let task_typed = issue.get("issueType").is_some_and(|issue_type| {
            issue_type.get("id").and_then(Value::as_str) == Some(task_issue_type.id.as_str())
                && issue_type.get("name").and_then(Value::as_str)
                    == Some(task_issue_type.name.as_str())
        });
        let task_marked = issue
            .get("body")
            .and_then(Value::as_str)
            .is_some_and(|body| body.starts_with(crate::protocol::WORKGRAPH_TASK_MARKER));
        Ok(admitted && !task_typed && !task_marked)
    }

    async fn workgraph_include(&self, node_id: &str) -> Result<bool> {
        let response = self
            .http
            .post(&self.api_url)
            .json(&json!({
                "query": "query($id: ID!) { node(id: $id) { ... on Issue { labels(first: 100) { nodes { name } pageInfo { hasNextPage } } } } }",
                "variables": {"id": node_id}
            }))
            .send()
            .await
            .context("authoritative task inclusion lookup failed")?
            .error_for_status()
            .context("authoritative task inclusion lookup returned an error")?;
        let payload: Value = response
            .json()
            .await
            .context("authoritative task inclusion lookup returned invalid JSON")?;
        if payload
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|errors| !errors.is_empty())
        {
            anyhow::bail!("authoritative task inclusion lookup returned GraphQL errors");
        }
        let labels = payload
            .pointer("/data/node/labels")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("authoritative task inclusion lookup omitted labels"))?;
        if labels
            .get("pageInfo")
            .and_then(Value::as_object)
            .and_then(|page_info| page_info.get("hasNextPage"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            anyhow::bail!("authoritative task label set exceeds 100 entries");
        }
        Ok(!labels
            .get("nodes")
            .and_then(Value::as_array)
            .context("authoritative task inclusion labels are missing")?
            .iter()
            .any(|label| {
                matches!(
                    label.get("name").and_then(Value::as_str),
                    Some(WORKGRAPH_IGNORE_LABEL | WORKGRAPH_ERROR_LABEL)
                )
            }))
    }

    async fn task_issue_state(
        &self,
        node_id: &str,
        task_issue_type: &crate::config::TaskIssueType,
    ) -> Result<TaskIssueState> {
        let response = self
            .http
            .post(&self.api_url)
            .json(&json!({
                "query": "query($id: ID!) { node(id: $id) { ... on Issue { id body state stateReason issueType { id name } parent { id } labels(first: 100) { nodes { name } pageInfo { hasNextPage } } } } }",
                "variables": {"id": node_id}
            }))
            .send()
            .await
            .context("authoritative task state lookup failed")?
            .error_for_status()
            .context("authoritative task state lookup returned an error")?;
        let payload: Value = response
            .json()
            .await
            .context("authoritative task state lookup returned invalid JSON")?;
        if payload
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|errors| !errors.is_empty())
        {
            anyhow::bail!("authoritative task state lookup returned GraphQL errors");
        }
        let issue = payload
            .pointer("/data/node")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("authoritative task state lookup did not return an Issue"))?;
        anyhow::ensure!(
            issue.get("id").and_then(Value::as_str) == Some(node_id),
            "authoritative task state lookup returned the wrong Issue"
        );
        let labels = issue
            .get("labels")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("authoritative task state lookup omitted labels"))?;
        if labels
            .get("pageInfo")
            .and_then(Value::as_object)
            .and_then(|page_info| page_info.get("hasNextPage"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            anyhow::bail!("authoritative task label set exceeds 100 entries");
        }
        let mut workgraph_labels = labels
            .get("nodes")
            .and_then(Value::as_array)
            .context("authoritative task state labels are missing")?
            .iter()
            .map(|label| {
                label
                    .get("name")
                    .and_then(Value::as_str)
                    .context("authoritative task state contains an invalid label")
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .filter(|label| label.starts_with("workgraph:"))
            .map(str::to_string)
            .collect::<Vec<_>>();
        workgraph_labels.sort();
        let workgraph_include = !workgraph_labels.iter().any(|label| {
            matches!(
                label.as_str(),
                WORKGRAPH_IGNORE_LABEL | WORKGRAPH_ERROR_LABEL
            )
        });
        let parent_source_key = match issue.get("parent") {
            Some(Value::Null) => None,
            Some(parent) => Some(
                parent
                    .get("id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .context("authoritative task state returned an invalid parent")?
                    .to_string(),
            ),
            None => anyhow::bail!("authoritative task state lookup omitted the parent field"),
        };
        let is_open = match issue.get("state").and_then(Value::as_str) {
            Some("OPEN") => true,
            Some("CLOSED") => false,
            _ => anyhow::bail!("authoritative task state lookup returned an invalid state"),
        };
        let issue_type = issue.get("issueType");
        let task_typed = issue_type.is_some_and(|issue_type| {
            issue_type.get("id").and_then(Value::as_str) == Some(task_issue_type.id.as_str())
                && issue_type.get("name").and_then(Value::as_str)
                    == Some(task_issue_type.name.as_str())
        });
        Ok(TaskIssueState {
            document: crate::protocol::TaskDocument {
                source_key: node_id.to_string(),
                body: issue
                    .get("body")
                    .and_then(Value::as_str)
                    .context("authoritative task state lookup omitted the body")?
                    .to_string(),
                is_open,
                state_reason: issue
                    .get("stateReason")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase(),
                parent_source_key,
                workgraph_labels,
                workgraph_include,
            },
            task_typed,
        })
    }

    async fn parent_issue_node_id(&self, node_id: &str) -> Result<Option<String>> {
        let response = self
            .http
            .post(&self.api_url)
            .json(&json!({
                "query": "query($id: ID!) { node(id: $id) { ... on Issue { parent { id } } } }",
                "variables": {"id": node_id}
            }))
            .send()
            .await
            .context("authoritative task parent lookup failed")?
            .error_for_status()
            .context("authoritative task parent lookup returned an error")?;
        let payload: Value = response
            .json()
            .await
            .context("authoritative task parent lookup returned invalid JSON")?;
        if payload
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|errors| !errors.is_empty())
        {
            anyhow::bail!("authoritative task parent lookup returned GraphQL errors");
        }
        let issue = payload
            .pointer("/data/node")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("authoritative task parent lookup did not return an Issue"))?;
        match issue.get("parent") {
            Some(Value::Null) => Ok(None),
            Some(parent) => parent
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
                .map(Some)
                .ok_or_else(|| {
                    anyhow!("authoritative task parent lookup returned an invalid parent")
                }),
            None => anyhow::bail!("authoritative task parent lookup omitted the parent field"),
        }
    }
}

/// Attempt to normalize a webhook event as WorkGraph input(s).
///
/// Returns `Ok(Some(inputs))` if the event matched WorkGraph patterns,
/// `Ok(None)` if the delivery is not part of the WorkGraph v1 protocol,
/// or `Err` for WorkGraph-specific rejections (untrusted, invalid).
async fn try_workgraph_normalization(
    state: &IngressState,
    event_type: &str,
    delivery_id: &str,
    payload: &serde_json::Value,
) -> Result<Option<Vec<crate::protocol::ProjectionInput>>, WorkGraphNormError> {
    use crate::protocol::*;

    match event_type {
        "issues" => try_workgraph_issue(state, delivery_id, payload).await,
        "issue_comment" => try_workgraph_comment(state, payload),
        "sub_issues" => try_workgraph_sub_issue(state, payload).await,
        _ => Ok(None),
    }
}

fn authorize_workgraph_repository(
    state: &IngressState,
    payload: &serde_json::Value,
    repository: Option<&serde_json::Value>,
) -> Result<(), WorkGraphNormError> {
    let login = payload
        .pointer("/organization/login")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            WorkGraphNormError::InvalidPayload(
                "payload has no 'organization.login'; configure an organization webhook"
                    .to_string(),
            )
        })?;
    if !login.eq_ignore_ascii_case(&state.organization) {
        return Err(WorkGraphNormError::Forbidden(format!(
            "delivery organization '{login}' does not match configured organization '{}'",
            state.organization
        )));
    }
    let repository = repository
        .or_else(|| payload.get("repository"))
        .ok_or_else(|| WorkGraphNormError::InvalidPayload("missing 'repository'".to_string()))?;
    let included = state
        .repository_filter
        .includes_repository(repository)
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    if !included {
        return Err(WorkGraphNormError::Forbidden(
            "delivery repository is outside the configured repository filter".to_string(),
        ));
    }
    Ok(())
}

const WORKGRAPH_ADMISSION_LABEL: &str = "workgraph";
const WORKGRAPH_IGNORE_LABEL: &str = "workgraph:ignore";
const WORKGRAPH_ERROR_LABEL: &str = "workgraph:error";

fn issue_workgraph_labels(issue: &serde_json::Value) -> (Vec<String>, bool) {
    let mut labels = issue
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|label| label.get("name").and_then(serde_json::Value::as_str))
        .filter(|name| name.starts_with("workgraph:"))
        .map(str::to_string)
        .collect::<Vec<_>>();
    labels.sort();
    let included = !labels.iter().any(|name| {
        matches!(
            name.as_str(),
            WORKGRAPH_IGNORE_LABEL | WORKGRAPH_ERROR_LABEL
        )
    });
    (labels, included)
}

fn task_issue_state(
    issue: &serde_json::Value,
    source_key: &str,
    parent_source_key: Option<String>,
    task_issue_type: &crate::config::TaskIssueType,
) -> TaskIssueState {
    let (workgraph_labels, workgraph_include) = issue_workgraph_labels(issue);
    TaskIssueState {
        document: crate::protocol::TaskDocument {
            source_key: source_key.to_string(),
            body: issue
                .get("body")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            is_open: item_is_open(issue),
            state_reason: issue
                .get("state_reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string(),
            parent_source_key,
            workgraph_labels,
            workgraph_include,
        },
        task_typed: task_issue_type.matches(issue.get("type")),
    }
}

fn normalize_github_issue(
    issue: &serde_json::Value,
    payload: &serde_json::Value,
) -> Result<crate::protocol::GitHubIssueDocument, WorkGraphNormError> {
    let locator = extract_issue_locator(issue, payload).ok_or_else(|| {
        WorkGraphNormError::InvalidPayload(
            "GitHub Issue is missing its repository locator".to_string(),
        )
    })?;
    if locator.issue_database_id > i64::MAX as u64 || locator.issue_number > i64::MAX as u64 {
        return Err(WorkGraphNormError::InvalidPayload(
            "GitHub Issue numeric identity exceeds the supported range".to_string(),
        ));
    }
    let repository_node_id = payload
        .pointer("/repository/node_id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            WorkGraphNormError::InvalidPayload(
                "GitHub Issue repository is missing 'node_id'".to_string(),
            )
        })?;
    let labels = issue
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|label| label.get("name").and_then(serde_json::Value::as_str))
        .map(str::to_string)
        .collect();
    let (workgraph_labels, workgraph_include) = issue_workgraph_labels(issue);
    Ok(crate::protocol::GitHubIssueDocument {
        source_key: locator.source_key,
        repository_owner: locator.repository_owner,
        repository_name: locator.repository_name,
        repository_node_id: repository_node_id.to_string(),
        issue_database_id: locator.issue_database_id,
        issue_number: locator.issue_number,
        title: issue
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        body: issue
            .get("body")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        is_open: item_is_open(issue),
        state_reason: issue
            .get("state_reason")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        labels,
        workgraph_labels,
        workgraph_include,
    })
}

fn issue_is_root_candidate(
    issue: &serde_json::Value,
    task_issue_type: &crate::config::TaskIssueType,
) -> bool {
    !task_issue_type.matches(issue.get("type"))
        && !issue
            .get("body")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|body| body.starts_with(crate::protocol::WORKGRAPH_TASK_MARKER))
        && issue
            .get("labels")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|labels| {
                labels.iter().any(|label| {
                    label.get("name").and_then(serde_json::Value::as_str)
                        == Some(WORKGRAPH_ADMISSION_LABEL)
                })
            })
}

fn authoritative_issue_revision(issue: &serde_json::Value) -> Result<i64, WorkGraphNormError> {
    let value = issue
        .get("updated_at")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            WorkGraphNormError::InvalidPayload("issue event is missing 'updated_at'".to_string())
        })?;
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp_millis())
        .map_err(|error| {
            WorkGraphNormError::InvalidPayload(format!(
                "issue event has invalid 'updated_at': {error}"
            ))
        })
}

fn admission_generation_id(root_issue_id: &str, delivery_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"workgraph-v1-admission-generation\0");
    digest.update(root_issue_id.as_bytes());
    digest.update([0]);
    digest.update(delivery_id.as_bytes());
    format!("wga-{}", hex::encode(digest.finalize()))
}

/// Normalize either a labeled Root Issue or an authorized generated task.
async fn try_workgraph_issue(
    state: &IngressState,
    delivery_id: &str,
    payload: &serde_json::Value,
) -> Result<Option<Vec<crate::protocol::ProjectionInput>>, WorkGraphNormError> {
    use crate::protocol::*;

    let action = payload
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let issue = payload.get("issue").ok_or_else(|| {
        WorkGraphNormError::InvalidPayload("issues event missing 'issue'".to_string())
    })?;
    let body = issue
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let node_id = issue
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| WorkGraphNormError::InvalidPayload("issue missing 'node_id'".to_string()))?;

    let current_workgraph = body.starts_with(WORKGRAPH_TASK_MARKER);
    let previous_workgraph = payload
        .pointer("/changes/body/from")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|body| body.starts_with(WORKGRAPH_TASK_MARKER));
    let (workgraph_labels, workgraph_include) = issue_workgraph_labels(issue);
    let labeled_root_candidate = issue_is_root_candidate(issue, &state.task_issue_type);
    let admission_added = action == "labeled"
        && payload
            .pointer("/label/name")
            .and_then(serde_json::Value::as_str)
            == Some(WORKGRAPH_ADMISSION_LABEL);
    authorize_workgraph_repository(state, payload, None)?;
    let previous_task = state
        .allocator
        .latest_workgraph_task(node_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let admitted = labeled_root_candidate
        && !current_workgraph
        && !previous_workgraph
        && previous_task.is_none();
    let previous_root = state
        .allocator
        .latest_workgraph_root_issue(node_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let previous_revision = state
        .allocator
        .latest_workgraph_issue_revision(node_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let revision = {
        let revision = authoritative_issue_revision(issue)?;
        if let Some(previous) = previous_revision {
            if revision < previous {
                return Ok(None);
            }
            if revision == previous && admitted != previous_root.is_some() {
                let client = state.admission_client.as_ref().ok_or_else(|| {
                    WorkGraphNormError::Unavailable(
                        "equal-revision admission transition requires an authoritative GitHub read"
                            .to_string(),
                    )
                })?;
                let authoritative = client
                    .is_root_candidate(node_id, &state.task_issue_type)
                    .await
                    .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?;
                if authoritative != admitted {
                    return Ok(None);
                }
            }
            if revision == previous
                && previous_root
                    .as_ref()
                    .is_some_and(|root| root.workgraph_include != workgraph_include)
            {
                let client = state.admission_client.as_ref().ok_or_else(|| {
                    WorkGraphNormError::Unavailable(
                        "equal-revision Root Issue inclusion transition requires an authoritative GitHub read"
                            .to_string(),
                    )
                })?;
                let authoritative = client
                    .workgraph_include(node_id)
                    .await
                    .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?;
                if authoritative != workgraph_include {
                    return Ok(None);
                }
            }
        }
        Some(revision)
    };
    if revision == previous_revision {
        if let Some(previous) = previous_task.as_ref() {
            let incoming = task_issue_state(
                issue,
                node_id,
                previous.parent_source_key.clone(),
                &state.task_issue_type,
            );
            if incoming.document != *previous || !incoming.task_typed {
                let client = state.admission_client.as_ref().ok_or_else(|| {
                    WorkGraphNormError::Unavailable(
                        "equal-revision task state transition requires an authoritative GitHub read"
                            .to_string(),
                    )
                })?;
                let authoritative = client
                    .task_issue_state(node_id, &state.task_issue_type)
                    .await
                    .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?;
                if authoritative != incoming {
                    return Ok(None);
                }
            }
        }
    }
    let mut inputs = revision
        .map(|revision| {
            vec![ProjectionInput::RecordIssueRevision {
                source_key: node_id.to_string(),
                revision,
            }]
        })
        .unwrap_or_default();

    if matches!(action, "deleted" | "transferred") {
        if previous_task.is_none() && !previous_workgraph {
            inputs.push(ProjectionInput::DeleteGitHubIssue {
                source_key: node_id.to_string(),
            });
        }
        if previous_task.is_some() || previous_workgraph {
            inputs.push(ProjectionInput::DeleteTask {
                source_key: node_id.to_string(),
            });
            inputs.push(ProjectionInput::DeleteLocator {
                source_key: node_id.to_string(),
            });
        }
        if previous_root.is_some() {
            inputs.push(ProjectionInput::DeleteRootIssue {
                source_key: node_id.to_string(),
            });
        }
        return Ok(Some(inputs));
    }

    if admitted {
        inputs.push(ProjectionInput::UpsertGitHubIssue(normalize_github_issue(
            issue, payload,
        )?));
        let locator = extract_issue_locator(issue, payload).ok_or_else(|| {
            WorkGraphNormError::InvalidPayload(
                "admitted Root Issue is missing its repository locator".to_string(),
            )
        })?;
        let repository_node_id = payload
            .pointer("/repository/node_id")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                WorkGraphNormError::InvalidPayload(
                    "admitted Root Issue repository is missing 'node_id'".to_string(),
                )
            })?;
        let begins_new_generation = previous_root.is_none()
            || admission_added
                && revision.is_some_and(|revision| {
                    previous_revision.is_none_or(|previous| revision > previous)
                });
        let admission_id = if begins_new_generation {
            admission_generation_id(node_id, delivery_id)
        } else {
            previous_root
                .as_ref()
                .expect("an existing generation supplies its admission ID")
                .admission_id
                .clone()
        };
        let admitted_title = if begins_new_generation {
            issue
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("")
                .to_string()
        } else {
            previous_root
                .as_ref()
                .expect("an existing generation supplies its frozen title")
                .title
                .clone()
        };
        let admitted_body = if begins_new_generation {
            body.to_string()
        } else {
            previous_root
                .as_ref()
                .expect("an existing generation supplies its frozen body")
                .body
                .clone()
        };
        inputs.push(ProjectionInput::UpsertRootIssue(RootIssueDocument {
            source_key: node_id.to_string(),
            repository_owner: locator.repository_owner,
            repository_name: locator.repository_name,
            repository_node_id: repository_node_id.to_string(),
            issue_number: locator.issue_number,
            title: admitted_title,
            body: admitted_body,
            is_open: item_is_open(issue),
            admission_id,
            workgraph_labels,
            workgraph_include,
        }));

        if previous_task.is_some() {
            inputs.push(ProjectionInput::DeleteTask {
                source_key: node_id.to_string(),
            });
            inputs.push(ProjectionInput::DeleteLocator {
                source_key: node_id.to_string(),
            });
        }

        return Ok(Some(inputs));
    }

    // Admission retraction must not be blocked by a task-shaped replacement body.
    if previous_root.is_some() {
        if item_is_open(issue) {
            inputs.push(ProjectionInput::UpsertGitHubIssue(normalize_github_issue(
                issue, payload,
            )?));
        } else {
            inputs.push(ProjectionInput::DeleteGitHubIssue {
                source_key: node_id.to_string(),
            });
        }
        inputs.push(ProjectionInput::DeleteRootIssue {
            source_key: node_id.to_string(),
        });
        return Ok(Some(inputs));
    }

    if current_workgraph || previous_workgraph || previous_task.is_some() {
        let typed = state.task_issue_type.matches(issue.get("type"));
        if !current_workgraph || !typed || action == "untyped" {
            inputs.push(ProjectionInput::DeleteTask {
                source_key: node_id.to_string(),
            });
            inputs.push(ProjectionInput::DeleteLocator {
                source_key: node_id.to_string(),
            });
            return Ok(Some(inputs));
        }

        let Some(protocol_trust) = &state.protocol_trust else {
            return Err(WorkGraphNormError::Untrusted(format!(
                "WorkGraph task {node_id}: no protocolTrust configured"
            )));
        };
        let creator = issue.get("user");
        let editor = payload.get("sender");
        if !protocol_trust.is_task_creator(creator) || !protocol_trust.is_task_creator(editor) {
            return Err(WorkGraphNormError::Untrusted(format!(
                "WorkGraph task {node_id}: creator or webhook actor is not a trusted task creator"
            )));
        }

        inputs.push(ProjectionInput::DeleteGitHubIssue {
            source_key: node_id.to_string(),
        });
        let task_doc = task_issue_state(
            issue,
            node_id,
            previous_task.and_then(|document| document.parent_source_key),
            &state.task_issue_type,
        )
        .document;
        inputs.push(ProjectionInput::UpsertTask(task_doc));
        if let Some(locator) = extract_issue_locator(issue, payload) {
            inputs.push(ProjectionInput::UpsertLocator(locator));
        }
        return Ok(Some(inputs));
    }

    if item_is_open(issue) {
        inputs.push(ProjectionInput::UpsertGitHubIssue(normalize_github_issue(
            issue, payload,
        )?));
    } else {
        inputs.push(ProjectionInput::DeleteGitHubIssue {
            source_key: node_id.to_string(),
        });
    }
    Ok(Some(inputs))
}

/// Normalize an issue_comment event with a WorkGraph lifecycle marker.
fn try_workgraph_comment(
    state: &IngressState,
    payload: &serde_json::Value,
) -> Result<Option<Vec<crate::protocol::ProjectionInput>>, WorkGraphNormError> {
    use crate::protocol::*;

    let action = payload
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let comment = payload.get("comment").ok_or_else(|| {
        WorkGraphNormError::InvalidPayload("issue_comment event missing 'comment'".to_string())
    })?;
    let comment_id = comment
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            WorkGraphNormError::InvalidPayload("comment missing 'node_id'".to_string())
        })?;
    let body = comment
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let previous_workgraph = payload
        .pointer("/changes/body/from")
        .and_then(serde_json::Value::as_str)
        .is_some_and(is_workgraph_lifecycle_marker);

    if action == "deleted"
        || (action == "edited" && lifecycle_trust_role(body).is_none() && previous_workgraph)
    {
        authorize_workgraph_repository(state, payload, None)?;
        return Ok(Some(vec![ProjectionInput::DeleteLifecycleArtifact {
            source_key: comment_id.to_string(),
        }]));
    }

    // Only handle WorkGraph lifecycle markers.
    let Some(trust_role) = lifecycle_trust_role(body) else {
        return Ok(None);
    };

    let issue = payload.get("issue").ok_or_else(|| {
        WorkGraphNormError::InvalidPayload("issue_comment missing 'issue'".to_string())
    })?;
    let task_node_id = issue
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| WorkGraphNormError::InvalidPayload("issue missing 'node_id'".to_string()))?;
    let task_body = issue
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !task_body.starts_with(WORKGRAPH_TASK_MARKER)
        || !state.task_issue_type.matches(issue.get("type"))
    {
        return Ok(None);
    }
    authorize_workgraph_repository(state, payload, None)?;

    // Trust check: reuse the existing anti-confused-deputy logic.
    if let Some(protocol_trust) = &state.protocol_trust {
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

        let role_check: fn(&ProtocolTrust, Option<&serde_json::Value>) -> bool = match trust_role {
            LifecycleTrustRole::Assigner => |trust, identity| trust.is_assigner(identity),
            LifecycleTrustRole::Reporter => |trust, identity| trust.is_reporter(identity),
        };

        let trusted = !unattributed_edit
            && role_check(protocol_trust, author)
            && editor.is_none_or(|e| role_check(protocol_trust, Some(e)));

        if !trusted {
            let role_name = match trust_role {
                LifecycleTrustRole::Assigner => "assigner",
                LifecycleTrustRole::Reporter => "reporter",
            };
            return Err(WorkGraphNormError::Untrusted(format!(
                "WorkGraph lifecycle comment {comment_id} on {task_node_id}: author/editor not \
                 trusted as {role_name}"
            )));
        }
    } else {
        return Err(WorkGraphNormError::Untrusted(format!(
            "WorkGraph lifecycle comment {comment_id} on {task_node_id}: no protocolTrust configured"
        )));
    }

    let mut inputs = Vec::new();
    let artifact = LifecycleArtifactDocument {
        source_key: comment_id.to_string(),
        task_source_key: task_node_id.to_string(),
        body: body.to_string(),
    };
    inputs.push(ProjectionInput::UpsertLifecycleArtifact(artifact));

    Ok(Some(inputs))
}

/// Normalize a sub_issues event (add/remove child parent relationship).
async fn try_workgraph_sub_issue(
    state: &IngressState,
    payload: &serde_json::Value,
) -> Result<Option<Vec<crate::protocol::ProjectionInput>>, WorkGraphNormError> {
    use crate::protocol::*;

    let action = payload
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");

    let adding_parent = match action {
        "sub_issue_added" | "parent_issue_added" => true,
        "sub_issue_removed" | "parent_issue_removed" => false,
        _ => return Ok(None),
    };

    let parent_issue = payload.get("parent_issue");
    let sub_issue = payload.get("sub_issue");
    let event_parent_node_id = parent_issue
        .and_then(|issue| issue.get("node_id"))
        .and_then(serde_json::Value::as_str);
    let child_repository = payload
        .get("sub_issue_repo")
        .or_else(|| payload.get("repository"));
    authorize_workgraph_repository(state, payload, child_repository)?;

    if !adding_parent && sub_issue.is_none() {
        let Some(issue_database_id) = payload
            .get("sub_issue_id")
            .and_then(serde_json::Value::as_u64)
        else {
            return Ok(None);
        };
        let child_node_id = state
            .allocator
            .workgraph_issue_node_id(issue_database_id)
            .await
            .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?
            .ok_or_else(|| {
                WorkGraphNormError::Unavailable(format!(
                    "sub_issue_removed references unknown GitHub database ID {issue_database_id}"
                ))
            })?;
        let mut previous = state
            .allocator
            .latest_workgraph_task(&child_node_id)
            .await
            .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?
            .ok_or_else(|| {
                WorkGraphNormError::Unavailable(format!(
                    "sub_issue_removed task {child_node_id} is missing from durable state"
                ))
            })?;
        let client = state.admission_client.as_ref().ok_or_else(|| {
            WorkGraphNormError::Unavailable(
                "sparse task hierarchy removal requires an authoritative GitHub read".to_string(),
            )
        })?;
        let authoritative_parent = client
            .parent_issue_node_id(&child_node_id)
            .await
            .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?;
        if previous.parent_source_key == authoritative_parent {
            return Ok(None);
        }
        previous.parent_source_key = authoritative_parent;
        return Ok(Some(vec![ProjectionInput::UpsertTask(previous)]));
    }

    let Some(sub_issue) = sub_issue else {
        return Ok(None);
    };
    let child_node_id = sub_issue
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            WorkGraphNormError::InvalidPayload("sub_issue missing 'node_id'".to_string())
        })?;
    if issue_is_root_candidate(sub_issue, &state.task_issue_type) {
        return Ok(None);
    }
    let previous_root = state
        .allocator
        .latest_workgraph_root_issue(child_node_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let previous_revision = state
        .allocator
        .latest_workgraph_issue_revision(child_node_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let revision = authoritative_issue_revision(sub_issue)?;
    if previous_revision.is_some_and(|previous| revision < previous) {
        return Ok(None);
    }
    let previous = state
        .allocator
        .latest_workgraph_task(child_node_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let (workgraph_labels, workgraph_include) = if sub_issue.get("labels").is_some() {
        issue_workgraph_labels(sub_issue)
    } else {
        previous
            .as_ref()
            .map(|document| {
                (
                    document.workgraph_labels.clone(),
                    document.workgraph_include,
                )
            })
            .unwrap_or_else(|| (Vec::new(), true))
    };
    if previous_root.is_some() {
        let client = state.admission_client.as_ref().ok_or_else(|| {
            WorkGraphNormError::Unavailable(
                "Root Issue hierarchy transition requires an authoritative GitHub read".to_string(),
            )
        })?;
        if client
            .is_root_candidate(child_node_id, &state.task_issue_type)
            .await
            .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?
        {
            return Ok(None);
        }
    }
    let parent_source_key = if previous.is_some() || event_parent_node_id.is_none() {
        let client = state.admission_client.as_ref().ok_or_else(|| {
            WorkGraphNormError::Unavailable(
                "task hierarchy transition requires an authoritative GitHub read".to_string(),
            )
        })?;
        client
            .parent_issue_node_id(child_node_id)
            .await
            .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?
    } else {
        adding_parent.then(|| {
            event_parent_node_id
                .expect("the non-authoritative path requires an event parent")
                .to_string()
        })
    };
    if adding_parent && parent_source_key.is_some() {
        if previous.is_none() && !state.task_issue_type.matches(sub_issue.get("type")) {
            return Ok(None);
        }
        let Some(protocol_trust) = &state.protocol_trust else {
            return Err(WorkGraphNormError::Untrusted(format!(
                "WorkGraph task {child_node_id}: no protocolTrust configured"
            )));
        };
        if !protocol_trust.is_task_creator(sub_issue.get("user"))
            || !protocol_trust.is_task_creator(payload.get("sender"))
        {
            return Err(WorkGraphNormError::Untrusted(format!(
                "WorkGraph task {child_node_id}: creator or hierarchy actor is not a trusted task creator"
            )));
        }
    } else if previous.is_none() {
        return Ok(None);
    }
    let child_body = previous
        .as_ref()
        .map(|document| document.body.clone())
        .or_else(|| {
            sub_issue
                .get("body")
                .and_then(serde_json::Value::as_str)
                .filter(|body| body.starts_with(WORKGRAPH_TASK_MARKER))
                .map(str::to_string)
        });
    let Some(child_body) = child_body else {
        return Ok(None);
    };
    let is_open = previous
        .as_ref()
        .map(|document| document.is_open)
        .unwrap_or_else(|| item_is_open(sub_issue));
    let state_reason = previous
        .as_ref()
        .map(|document| document.state_reason.clone())
        .or_else(|| {
            sub_issue
                .get("state_reason")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default();

    let task_doc = TaskDocument {
        source_key: child_node_id.to_string(),
        body: child_body,
        is_open,
        state_reason,
        parent_source_key,
        workgraph_labels,
        workgraph_include,
    };
    if previous_revision == Some(revision)
        && previous
            .as_ref()
            .is_some_and(|previous| previous != &task_doc)
    {
        let client = state.admission_client.as_ref().ok_or_else(|| {
            WorkGraphNormError::Unavailable(
                "equal-revision task hierarchy state transition requires an authoritative GitHub read"
                    .to_string(),
            )
        })?;
        let incoming = TaskIssueState {
            document: task_doc.clone(),
            task_typed: state.task_issue_type.matches(sub_issue.get("type")),
        };
        let authoritative = client
            .task_issue_state(child_node_id, &state.task_issue_type)
            .await
            .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?;
        if authoritative != incoming {
            return Ok(None);
        }
    }

    let mut inputs = Vec::new();
    inputs.push(ProjectionInput::RecordIssueRevision {
        source_key: child_node_id.to_string(),
        revision,
    });
    if previous_root.is_some() {
        inputs.push(ProjectionInput::DeleteRootIssue {
            source_key: child_node_id.to_string(),
        });
    }
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
) -> Option<crate::protocol::GitHubIssueLocator> {
    extract_issue_locator_from_repository(issue, payload.get("repository"))
}

fn extract_issue_locator_from_repository(
    issue: &serde_json::Value,
    repository: Option<&serde_json::Value>,
) -> Option<crate::protocol::GitHubIssueLocator> {
    let database_id = issue.get("id")?.as_u64()?;
    let node_id = issue.get("node_id")?.as_str()?;
    let number = issue.get("number")?.as_u64()?;
    let repo = repository?;
    let full_name = repo.get("full_name")?.as_str()?;
    let (owner, name) = full_name.split_once('/')?;
    Some(crate::protocol::GitHubIssueLocator {
        source_key: node_id.to_string(),
        repository_owner: owner.to_string(),
        repository_name: name.to_string(),
        issue_database_id: database_id,
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
mod workgraph_tests {
    use super::*;
    use crate::config::{TaskIssueType, TrustedIdentity};
    use crate::protocol::{
        GitHubIssueLocator, PreparedProjection, PreparedProjectionCommit, ProjectionInput,
        TaskDocument, WorkGraphAllocatorProjection, WorkGraphTaskBinding,
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
            let tasks = inputs
                .iter()
                .filter_map(|input| match input {
                    ProjectionInput::UpsertTask(document) => Some(WorkGraphTaskBinding {
                        source_key: document.source_key.clone(),
                        task_id: document.source_key.clone(),
                        task_element_id: format!("task:{}", document.source_key),
                        root_issue_id: "root".to_string(),
                        workflow_run_id: "run".to_string(),
                    }),
                    _ => None,
                })
                .collect();
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
                allocator: WorkGraphAllocatorProjection {
                    tasks,
                    ..WorkGraphAllocatorProjection::default()
                },
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
        trust: Option<ProtocolTrust>,
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
                protocol_trust: trust,
                secret: b"secret".to_vec(),
                lease_validation_token: b"validation".to_vec(),
                allocator,
                agent_sync: None,
                projector: Some(projector.clone()),
                workflow_definition: None,
                admission_client: None,
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
            "updated_at": "2026-08-01T00:00:00Z",
            "state_reason": null,
            "user": {"node_id": "U_creator", "login": "task-creator"},
            "type": {"node_id": "IT_task", "name": "WorkGraphTask"}
        })
    }

    fn payload(action: &str, issue: serde_json::Value) -> serde_json::Value {
        json!({
            "action": action,
            "organization": {"login": "acme"},
            "repository": {
                "name": "widgets",
                "full_name": "acme/widgets",
                "node_id": "R_widgets"
            },
            "sender": {"node_id": "U_creator", "login": "task-creator"},
            "issue": issue
        })
    }

    fn signed_headers(event: &str, delivery: &str, body: &[u8]) -> HeaderMap {
        let mut mac = HmacSha256::new_from_slice(b"secret").expect("HMAC key");
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hub-signature-256",
            signature.parse().expect("signature header"),
        );
        headers.insert(
            "x-github-delivery",
            delivery.parse().expect("delivery header"),
        );
        headers.insert("x-github-event", event.parse().expect("event header"));
        headers
    }

    fn root_issue(labels: &[&str]) -> serde_json::Value {
        json!({
            "id": 41,
            "node_id": "I_root",
            "number": 6,
            "title": "Root Issue",
            "body": "Coordinate this work.",
            "state": "open",
            "updated_at": "2026-08-01T00:00:00Z",
            "state_reason": null,
            "labels": labels.iter().map(|name| json!({"name": name})).collect::<Vec<_>>()
        })
    }

    fn task_trust() -> ProtocolTrust {
        ProtocolTrust {
            task_creators: vec![TrustedIdentity {
                id: "U_creator".to_string(),
                login: "task-creator".to_string(),
            }],
            dispatchers: vec![TrustedIdentity {
                id: "U_dispatch".to_string(),
                login: "dispatcher".to_string(),
            }],
            reporters: vec![TrustedIdentity {
                id: "U_reporter".to_string(),
                login: "reporter".to_string(),
            }],
        }
    }

    #[tokio::test]
    async fn authorized_task_normalizes_as_one_task_and_locator_batch() {
        let (_temp, _projector, state) = ingress_state(Some(task_trust())).await;
        let inputs = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload(
                "opened",
                task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
            ),
        )
        .await
        .expect("normalize")
        .expect("WorkGraph task");
        assert_eq!(inputs.len(), 4);
        assert!(matches!(
            &inputs[0],
            ProjectionInput::RecordIssueRevision { source_key, .. }
                if source_key == "I_task"
        ));
        assert!(matches!(
            &inputs[1],
            ProjectionInput::DeleteGitHubIssue { source_key } if source_key == "I_task"
        ));
        assert!(matches!(&inputs[2], ProjectionInput::UpsertTask(_)));
        assert!(matches!(&inputs[3], ProjectionInput::UpsertLocator(locator)
            if locator.repository_owner == "acme"
                && locator.repository_name == "widgets"
                && locator.issue_number == 7));
    }

    #[tokio::test]
    async fn task_inclusion_uses_only_exact_case_sensitive_exclusion_labels() {
        for (labels, expected_labels, expected_include) in [
            (vec![], vec![], true),
            (vec!["workgraph:custom"], vec!["workgraph:custom"], true),
            (
                vec!["workgraph:Error", "workgraph:custom"],
                vec!["workgraph:Error", "workgraph:custom"],
                true,
            ),
            (
                vec!["workgraph:ignore", "workgraph:custom"],
                vec!["workgraph:custom", "workgraph:ignore"],
                false,
            ),
            (
                vec!["workgraph:error", "workgraph:ignore"],
                vec!["workgraph:error", "workgraph:ignore"],
                false,
            ),
        ] {
            let (_temp, _projector, state) = ingress_state(Some(task_trust())).await;
            let mut issue = task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n");
            issue["labels"] = serde_json::Value::Array(
                labels
                    .into_iter()
                    .map(|name| json!({"name": name}))
                    .collect(),
            );
            let inputs = try_workgraph_issue(&state, "delivery-labels", &payload("opened", issue))
                .await
                .expect("normalize")
                .expect("WorkGraph task");
            let task = inputs
                .iter()
                .find_map(|input| match input {
                    ProjectionInput::UpsertTask(task) => Some(task),
                    _ => None,
                })
                .expect("task document");
            assert_eq!(task.workgraph_labels, expected_labels);
            assert_eq!(task.workgraph_include, expected_include);
        }
    }

    #[tokio::test]
    async fn delayed_issue_delivery_cannot_overwrite_newer_exclusion_state() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![
                    ProjectionInput::RecordIssueRevision {
                        source_key: "I_task".to_string(),
                        revision: chrono::DateTime::parse_from_rfc3339("2026-08-02T00:00:00Z")
                            .expect("revision")
                            .timestamp_millis(),
                    },
                    ProjectionInput::UpsertTask(TaskDocument {
                        source_key: "I_task".to_string(),
                        body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
                        is_open: true,
                        state_reason: String::new(),
                        parent_source_key: Some("I_root".to_string()),
                        workgraph_labels: vec!["workgraph:ignore".to_string()],
                        workgraph_include: false,
                    }),
                ],
                1,
                "newer-issue-exclusion",
            )
            .await
            .expect("persist newer exclusion");

        let mut delayed = task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n");
        delayed["labels"] = json!([]);
        delayed["updated_at"] = json!("2026-08-01T00:00:00Z");
        assert!(try_workgraph_issue(
            &state,
            "delayed-before-exclusion",
            &payload("edited", delayed),
        )
        .await
        .expect("ignore stale delivery")
        .is_none());
        assert!(state
            .allocator
            .latest_workgraph_task("I_task")
            .await
            .expect("read durable task")
            .is_some_and(|task| !task.workgraph_include));
    }

    #[tokio::test]
    async fn generic_issue_projects_sorted_workgraph_labels_and_exact_inclusion() {
        for (labels, expected_labels, expected_include) in [
            (
                vec!["workgraph:custom", "workgraph:Error"],
                vec!["workgraph:Error", "workgraph:custom"],
                true,
            ),
            (
                vec!["workgraph:ignore", "workgraph:custom"],
                vec!["workgraph:custom", "workgraph:ignore"],
                false,
            ),
            (
                vec!["workgraph:error", "workgraph:Error"],
                vec!["workgraph:Error", "workgraph:error"],
                false,
            ),
        ] {
            let (_temp, _projector, state) = ingress_state(None).await;
            let inputs = try_workgraph_issue(
                &state,
                "generic-labels",
                &payload("labeled", root_issue(&labels)),
            )
            .await
            .expect("normalize")
            .expect("generic Issue");
            let issue = inputs
                .iter()
                .find_map(|input| match input {
                    ProjectionInput::UpsertGitHubIssue(issue) => Some(issue),
                    _ => None,
                })
                .expect("generic Issue document");
            assert_eq!(issue.workgraph_labels, expected_labels);
            assert_eq!(issue.workgraph_include, expected_include);
            let changes = crate::mapping::generic_issue_changes("source", 1, &inputs);
            assert!(changes.iter().any(|change| matches!(
                change,
                SourceChange::Update {
                    element: Element::Node {
                        metadata,
                        properties
                    }
                } if metadata.reference.element_id.as_ref() == "I_root"
                    && properties.get("workgraphInclude")
                        == Some(&drasi_core::models::ElementValue::Bool(expected_include))
            )));
        }
    }

    #[tokio::test]
    async fn task_requires_exact_trusted_creator_and_webhook_actor() {
        let (_temp, _projector, state) = ingress_state(Some(task_trust())).await;
        let mut event = payload(
            "opened",
            task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
        );
        event["issue"]["user"]["login"] = json!("other");
        assert!(matches!(
            try_workgraph_issue(&state, "delivery-1", &event).await,
            Err(WorkGraphNormError::Untrusted(_))
        ));

        let mut event = payload(
            "opened",
            task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
        );
        event["sender"]["node_id"] = json!("U_other");
        assert!(matches!(
            try_workgraph_issue(&state, "delivery-2", &event).await,
            Err(WorkGraphNormError::Untrusted(_))
        ));
    }

    #[tokio::test]
    async fn root_issue_label_removal_and_readdition_create_a_fresh_generation() {
        let (_temp, projector, state) = ingress_state(None).await;
        let generic = try_workgraph_issue(
            &state,
            "ignored",
            &payload("opened", root_issue(&["WorkGraph"])),
        )
        .await
        .expect("normalize")
        .expect("generic Issue");
        assert!(matches!(
            generic.as_slice(),
            [
                ProjectionInput::RecordIssueRevision { .. },
                ProjectionInput::UpsertGitHubIssue(document)
            ]
                if document.workgraph_include
                && document.workgraph_labels.is_empty()
        ));

        let first = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload("labeled", root_issue(&["workgraph"])),
        )
        .await
        .expect("normalize")
        .expect("admitted Root Issue");
        let first_admission = first
            .iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertRootIssue(document) => {
                    assert_eq!(document.source_key, "I_root");
                    Some(document.admission_id.clone())
                }
                _ => None,
            })
            .expect("Root Issue upsert");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), first, 1, "root-1")
            .await
            .expect("persist first admission");

        let mut task_shaped_root = root_issue(&[]);
        task_shaped_root["body"] = json!("WorkGraphTask/v1\n\n```json\n{}\n```\n");
        task_shaped_root["type"] = json!({"node_id": "IT_task", "name": "WorkGraphTask"});
        task_shaped_root["updated_at"] = json!("2026-08-01T00:01:00Z");
        let removed = try_workgraph_issue(
            &state,
            "delivery-2",
            &payload("unlabeled", task_shaped_root),
        )
        .await
        .expect("normalize")
        .expect("retract Root Issue");
        assert!(removed.iter().any(|input| matches!(
            input,
            ProjectionInput::DeleteRootIssue { source_key } if source_key == "I_root"
        )));
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), removed, 2, "root-2")
            .await
            .expect("persist retraction");

        let mut readmitted = root_issue(&["workgraph"]);
        readmitted["updated_at"] = json!("2026-08-01T00:02:00Z");
        let second = try_workgraph_issue(&state, "delivery-3", &payload("labeled", readmitted))
            .await
            .expect("normalize")
            .expect("readmit Root Issue");
        let second_admission = second
            .iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertRootIssue(document) => Some(document.admission_id.clone()),
                _ => None,
            })
            .expect("Root Issue upsert");
        assert_ne!(first_admission, second_admission);
    }

    #[tokio::test]
    async fn root_exclusion_preserves_admission_generation_and_projects_an_excluded_upsert() {
        let (_temp, projector, state) = ingress_state(None).await;
        let admitted = try_workgraph_issue(
            &state,
            "root-admitted",
            &payload("labeled", root_issue(&["workgraph"])),
        )
        .await
        .expect("normalize")
        .expect("admit Root Issue");
        let admission_id = admitted
            .iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertRootIssue(root) => Some(root.admission_id.clone()),
                _ => None,
            })
            .expect("Root Issue upsert");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), admitted, 1, "root-admitted")
            .await
            .expect("persist admission");

        let mut excluded_issue = root_issue(&["workgraph", "workgraph:ignore"]);
        excluded_issue["updated_at"] = json!("2026-08-01T00:01:00Z");
        let excluded =
            try_workgraph_issue(&state, "root-excluded", &payload("labeled", excluded_issue))
                .await
                .expect("normalize")
                .expect("exclude Root Issue");
        assert!(!excluded
            .iter()
            .any(|input| matches!(input, ProjectionInput::DeleteRootIssue { .. })));
        assert!(excluded.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertRootIssue(root)
                if root.admission_id == admission_id && !root.workgraph_include
        )));
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), excluded, 2, "root-excluded")
            .await
            .expect("persist exclusion");
        assert!(state
            .allocator
            .latest_workgraph_root_issue("I_root")
            .await
            .expect("read Root Issue")
            .is_some_and(|root| { root.admission_id == admission_id && !root.workgraph_include }));

        let mut reincluded_issue = root_issue(&["workgraph"]);
        reincluded_issue["updated_at"] = json!("2026-08-01T00:02:00Z");
        let reincluded = try_workgraph_issue(
            &state,
            "root-reincluded",
            &payload("unlabeled", reincluded_issue),
        )
        .await
        .expect("normalize")
        .expect("re-include Root Issue");
        assert!(!reincluded
            .iter()
            .any(|input| matches!(input, ProjectionInput::DeleteRootIssue { .. })));
        assert!(reincluded.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertRootIssue(root)
                if root.admission_id == admission_id && root.workgraph_include
        )));
    }

    #[tokio::test]
    async fn generated_task_is_never_admitted_as_a_root_issue() {
        let (_temp, _projector, state) = ingress_state(Some(task_trust())).await;
        let mut issue = task_issue("I_root", "WorkGraphTask/v1\n\n```json\n{}\n```\n");
        issue["title"] = json!("Root Issue");
        issue["labels"] = json!([{"name": "workgraph"}]);

        let inputs = try_workgraph_issue(&state, "delivery-1", &payload("labeled", issue))
            .await
            .expect("normalize")
            .expect("generated task");
        assert!(!inputs
            .iter()
            .any(|input| matches!(input, ProjectionInput::UpsertRootIssue(_))));
        assert!(inputs.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertTask(document) if document.source_key == "I_root"
        )));
    }

    #[tokio::test]
    async fn untyped_labeled_task_is_retracted_instead_of_admitted() {
        let (_temp, projector, state) = ingress_state(None).await;
        let body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![ProjectionInput::UpsertTask(TaskDocument {
                    source_key: "I_task".to_string(),
                    body: body.to_string(),
                    is_open: true,
                    state_reason: String::new(),
                    parent_source_key: Some("I_root".to_string()),
                    workgraph_labels: Vec::new(),
                    workgraph_include: true,
                })],
                1,
                "seed-untyped-task",
            )
            .await
            .expect("seed task");
        let mut issue = task_issue("I_task", body);
        issue["labels"] = json!([{"name": "workgraph"}]);
        issue["type"] = serde_json::Value::Null;
        issue["updated_at"] = json!("2026-08-01T00:01:00Z");

        let inputs = try_workgraph_issue(&state, "delivery-1", &payload("untyped", issue))
            .await
            .expect("normalize")
            .expect("task retraction");

        assert!(inputs.iter().any(|input| matches!(
            input,
            ProjectionInput::DeleteTask { source_key } if source_key == "I_task"
        )));
        assert!(!inputs
            .iter()
            .any(|input| matches!(input, ProjectionInput::UpsertRootIssue(_))));
    }

    #[tokio::test]
    async fn reordered_readmission_starts_a_fresh_generation_before_delayed_retraction() {
        let (_temp, projector, state) = ingress_state(None).await;
        let admitted = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload("labeled", root_issue(&["workgraph"])),
        )
        .await
        .expect("normalize initial admission")
        .expect("initial admission");
        let first_admission = admitted
            .iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertRootIssue(document) => Some(document.admission_id.clone()),
                _ => None,
            })
            .expect("initial Root Issue");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), admitted, 1, "root-1")
            .await
            .expect("persist initial admission");

        let mut readmitted = root_issue(&["workgraph"]);
        readmitted["title"] = json!("Readmitted Root Issue");
        readmitted["updated_at"] = json!("2026-08-01T00:02:00Z");
        let mut event = payload("labeled", readmitted);
        event["label"] = json!({"name": "workgraph"});
        let inputs = try_workgraph_issue(&state, "delivery-3", &event)
            .await
            .expect("normalize reordered readmission")
            .expect("reordered readmission");
        let second = inputs
            .iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertRootIssue(document) => Some(document),
                _ => None,
            })
            .expect("replacement Root Issue");
        assert_ne!(second.admission_id, first_admission);
        assert_eq!(second.title, "Readmitted Root Issue");
        let second_admission = second.admission_id.clone();
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), inputs, 2, "root-3")
            .await
            .expect("persist reordered readmission");

        let mut removed = root_issue(&[]);
        removed["updated_at"] = json!("2026-08-01T00:01:00Z");
        let mut delayed = payload("unlabeled", removed);
        delayed["label"] = json!({"name": "workgraph"});
        assert!(try_workgraph_issue(&state, "delivery-2", &delayed)
            .await
            .expect("normalize delayed retraction")
            .is_none());
        assert_eq!(
            state
                .allocator
                .latest_workgraph_root_issue("I_root")
                .await
                .expect("read Root Issue")
                .expect("Root Issue remains admitted")
                .admission_id,
            second_admission
        );
    }

    #[tokio::test]
    async fn stale_task_issue_event_cannot_reopen_a_closed_task() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        let body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        let opened = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload("opened", task_issue("I_task", body)),
        )
        .await
        .expect("normalize task")
        .expect("task inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), opened, 1, "task-1")
            .await
            .expect("persist task");

        let mut closed_issue = task_issue("I_task", body);
        closed_issue["state"] = json!("closed");
        closed_issue["updated_at"] = json!("2026-08-01T00:03:00Z");
        let closed = try_workgraph_issue(&state, "delivery-3", &payload("closed", closed_issue))
            .await
            .expect("normalize close")
            .expect("close inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), closed, 2, "task-3")
            .await
            .expect("persist close");

        let mut stale_open = task_issue("I_task", body);
        stale_open["updated_at"] = json!("2026-08-01T00:02:00Z");
        assert!(
            try_workgraph_issue(&state, "delivery-2", &payload("reopened", stale_open),)
                .await
                .expect("normalize stale reopen")
                .is_none()
        );
        assert!(
            !state
                .allocator
                .latest_workgraph_task("I_task")
                .await
                .expect("read task")
                .expect("task remains")
                .is_open
        );
    }

    #[tokio::test]
    async fn authoritative_close_wins_equal_revision_reopen_in_both_delivery_orders() {
        let body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        for close_first in [true, false] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": {
                        "node": {
                            "id": "I_task",
                            "body": body,
                            "state": "CLOSED",
                            "stateReason": "COMPLETED",
                            "issueType": {"id": "IT_task", "name": "WorkGraphTask"},
                            "parent": null,
                            "labels": {
                                "nodes": [],
                                "pageInfo": {"hasNextPage": false}
                            }
                        }
                    }
                })))
                .expect(2)
                .mount(&server)
                .await;
            let (_temp, projector, mut state) = ingress_state(Some(task_trust())).await;
            state.admission_client = Some(
                AdmissionClient::new(&WorkflowDefinitionConfig {
                    token: "token".to_string(),
                    api_base_url: server.uri(),
                    ..WorkflowDefinitionConfig::default()
                })
                .expect("admission client"),
            );
            let opened = try_workgraph_issue(
                &state,
                "delivery-open",
                &payload("opened", task_issue("I_task", body)),
            )
            .await
            .expect("normalize initial open")
            .expect("initial task inputs");
            state
                .allocator
                .ingest_workgraph(projector.as_ref(), opened, 1, "equal-open")
                .await
                .expect("persist initial open");

            let mut closed_issue = task_issue("I_task", body);
            closed_issue["state"] = json!("closed");
            closed_issue["state_reason"] = json!("completed");
            closed_issue["updated_at"] = json!("2026-08-01T00:01:00Z");
            let close = payload("closed", closed_issue);
            let mut reopened_issue = task_issue("I_task", body);
            reopened_issue["updated_at"] = json!("2026-08-01T00:01:00Z");
            let reopen = payload("reopened", reopened_issue);
            let deliveries = if close_first {
                [("delivery-close", &close), ("delivery-reopen", &reopen)]
            } else {
                [("delivery-reopen", &reopen), ("delivery-close", &close)]
            };
            for (index, (delivery_id, delivery)) in deliveries.into_iter().enumerate() {
                if let Some(inputs) = try_workgraph_issue(&state, delivery_id, delivery)
                    .await
                    .expect("normalize equal-revision delivery")
                {
                    state
                        .allocator
                        .ingest_workgraph(
                            projector.as_ref(),
                            inputs,
                            index as u64 + 2,
                            &format!("{delivery_id}-{close_first}"),
                        )
                        .await
                        .expect("persist equal-revision delivery");
                }
            }

            let task = state
                .allocator
                .latest_workgraph_task("I_task")
                .await
                .expect("read durable task")
                .expect("task remains");
            assert!(!task.is_open);
            assert_eq!(task.state_reason, "completed");
            assert!(try_workgraph_issue(&state, "delivery-replay", &reopen)
                .await
                .expect("normalize replayed reopen")
                .is_none());
            assert!(
                !state
                    .allocator
                    .latest_workgraph_task("I_task")
                    .await
                    .expect("read task after replay")
                    .expect("task remains after replay")
                    .is_open
            );
        }
    }

    #[tokio::test]
    async fn stale_labeled_delivery_cannot_resurrect_a_retracted_root_issue() {
        let (_temp, projector, state) = ingress_state(None).await;
        let admitted = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload("labeled", root_issue(&["workgraph"])),
        )
        .await
        .expect("normalize admission")
        .expect("admission inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), admitted, 1, "root-1")
            .await
            .expect("persist admission");

        let mut retracted = root_issue(&[]);
        retracted["updated_at"] = json!("2026-08-01T00:02:00Z");
        let retraction =
            try_workgraph_issue(&state, "delivery-2", &payload("unlabeled", retracted))
                .await
                .expect("normalize retraction")
                .expect("retraction inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), retraction, 2, "root-2")
            .await
            .expect("persist retraction");

        let mut stale = root_issue(&["workgraph"]);
        stale["updated_at"] = json!("2026-08-01T00:01:00Z");
        assert!(
            try_workgraph_issue(&state, "delivery-stale", &payload("labeled", stale))
                .await
                .expect("normalize stale delivery")
                .is_none()
        );
        assert!(state
            .allocator
            .latest_workgraph_root_issue("I_root")
            .await
            .expect("read Root Issue")
            .is_none());
    }

    #[tokio::test]
    async fn unseen_admission_retraction_tombstones_an_older_label_delivery() {
        let (_temp, projector, state) = ingress_state(None).await;
        let mut retracted = root_issue(&[]);
        retracted["updated_at"] = json!("2026-08-01T00:02:00Z");
        let mut event = payload("unlabeled", retracted);
        event["label"] = json!({"name": "workgraph"});
        let tombstone = try_workgraph_issue(&state, "delivery-2", &event)
            .await
            .expect("normalize retraction")
            .expect("revision tombstone");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), tombstone, 1, "root-2")
            .await
            .expect("persist revision tombstone");

        let mut stale = root_issue(&["workgraph"]);
        stale["updated_at"] = json!("2026-08-01T00:01:00Z");
        assert!(
            try_workgraph_issue(&state, "delivery-1", &payload("labeled", stale))
                .await
                .expect("normalize stale delivery")
                .is_none()
        );
    }

    #[tokio::test]
    async fn admitted_root_issue_content_is_frozen_for_its_generation() {
        let (_temp, projector, state) = ingress_state(None).await;
        let admitted = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload("labeled", root_issue(&["workgraph"])),
        )
        .await
        .expect("normalize admission")
        .expect("admission inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), admitted, 1, "root-1")
            .await
            .expect("persist admission");

        let mut edited = root_issue(&["workgraph"]);
        edited["title"] = json!("Changed title");
        edited["body"] = json!("Changed body");
        edited["updated_at"] = json!("2026-08-01T00:01:00Z");
        let inputs = try_workgraph_issue(&state, "delivery-2", &payload("edited", edited))
            .await
            .expect("normalize edit")
            .expect("Root Issue update");
        let document = inputs
            .iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertRootIssue(document) => Some(document),
                _ => None,
            })
            .expect("Root Issue document");
        assert_eq!(document.title, "Root Issue");
        assert_eq!(document.body, "Coordinate this work.");
    }

    #[tokio::test]
    async fn durable_projection_batch_is_committed_and_deduplicated_once() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        let inputs = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload(
                "opened",
                task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
            ),
        )
        .await
        .expect("normalize")
        .expect("WorkGraph task");
        let first = state
            .allocator
            .ingest_workgraph(projector.as_ref(), inputs.clone(), 7, "delivery-1")
            .await
            .expect("first projection");
        let duplicate = state
            .allocator
            .ingest_workgraph(projector.as_ref(), inputs, 7, "delivery-1")
            .await
            .expect("duplicate projection");
        assert_eq!(first.0, 1);
        assert_eq!(duplicate.0, 0);
        assert_eq!(projector.committed.lock().await.len(), 1);
        let checkpoint = state
            .allocator
            .workgraph_checkpoint()
            .await
            .expect("durable checkpoint");
        assert_eq!(
            serde_json::from_slice::<Vec<ProjectionInput>>(&checkpoint)
                .expect("decode checkpoint")
                .len(),
            3
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
            .ingest_workgraph(&projector, vec![input], 7, "delivery-1")
            .await
            .is_err());
        assert!(projector.committed.lock().await.is_empty());

        let checkpoint = allocator
            .workgraph_checkpoint()
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
            task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
        );
        event["repository"] = json!({"name": "other", "full_name": "acme/other"});
        assert!(matches!(
            try_workgraph_issue(&state, "delivery-1", &event).await,
            Err(WorkGraphNormError::Forbidden(_))
        ));
    }

    #[tokio::test]
    async fn issue_update_preserves_parent_from_durable_projection_history() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        let body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![ProjectionInput::UpsertTask(TaskDocument {
                    source_key: "I_child".to_string(),
                    body: body.to_string(),
                    is_open: true,
                    state_reason: String::new(),
                    parent_source_key: Some("I_parent".to_string()),
                    workgraph_labels: Vec::new(),
                    workgraph_include: true,
                })],
                1,
                "seed",
            )
            .await
            .expect("seed task");

        let inputs = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload("edited", task_issue("I_child", body)),
        )
        .await
        .expect("normalize")
        .expect("WorkGraph task");
        assert!(inputs.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertTask(document)
                if document.parent_source_key.as_deref() == Some("I_parent")
        )));
    }

    #[tokio::test]
    async fn lifecycle_artifact_requires_trust_and_preserves_exact_body() {
        let body = "WorkGraphTaskAssign/v1\n\n```json\n{\"operationId\":\"op\"}\n```\n";
        let issue = task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n");
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
            try_workgraph_comment(&untrusted, &event),
            Err(WorkGraphNormError::Untrusted(_))
        ));

        let trust = ProtocolTrust {
            task_creators: vec![TrustedIdentity {
                id: "U_creator".to_string(),
                login: "task-creator".to_string(),
            }],
            dispatchers: vec![TrustedIdentity {
                id: "U_dispatch".to_string(),
                login: "dispatcher".to_string(),
            }],
            reporters: Vec::new(),
        };
        let (_temp, _projector, trusted) = ingress_state(Some(trust)).await;
        let inputs = try_workgraph_comment(&trusted, &event)
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
    async fn editing_away_lifecycle_marker_retracts_without_requiring_new_trust() {
        let (_temp, _projector, state) = ingress_state(None).await;
        let event = json!({
            "action": "edited",
            "organization": {"login": "acme"},
            "repository": {
                "name": "widgets",
                "full_name": "acme/widgets",
                "node_id": "R_widgets"
            },
            "sender": {"node_id": "U_other", "login": "other"},
            "issue": task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
            "comment": {
                "node_id": "IC_assign",
                "body": "ordinary comment",
                "user": {"node_id": "U_dispatch", "login": "dispatcher"},
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:01:00Z"
            },
            "changes": {
                "body": {
                    "from": "WorkGraphTaskAssign/v1\n\n```json\n{\"operationId\":\"op\"}\n```\n"
                }
            }
        });

        let inputs = try_workgraph_comment(&state, &event)
            .expect("normalize")
            .expect("retract lifecycle artifact");
        assert_eq!(
            inputs,
            vec![ProjectionInput::DeleteLifecycleArtifact {
                source_key: "IC_assign".to_string(),
            }]
        );
    }

    #[tokio::test]
    async fn sub_issue_removal_reuses_document_and_clears_parent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"node": {"parent": null}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_temp, projector, mut state) = ingress_state(None).await;
        let body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![ProjectionInput::UpsertTask(TaskDocument {
                    source_key: "I_child".to_string(),
                    body: body.to_string(),
                    is_open: true,
                    state_reason: String::new(),
                    parent_source_key: Some("I_parent".to_string()),
                    workgraph_labels: Vec::new(),
                    workgraph_include: true,
                })],
                1,
                "seed",
            )
            .await
            .expect("seed task");
        state.admission_client = Some(
            AdmissionClient::new(&WorkflowDefinitionConfig {
                token: "token".to_string(),
                api_base_url: server.uri(),
                ..WorkflowDefinitionConfig::default()
            })
            .expect("admission client"),
        );
        let event = json!({
            "action": "parent_issue_removed",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "sub_issue": {
                "node_id": "I_child",
                "number": 7,
                "updated_at": "2026-08-01T00:01:00Z",
                "type": {"node_id": "IT_task", "name": "WorkGraphTask"}
            }
        });
        let inputs = try_workgraph_sub_issue(&state, &event)
            .await
            .expect("normalize")
            .expect("sub-issue task");
        assert!(inputs.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertTask(document)
                if document.body == body && document.parent_source_key.is_none()
        )));
    }

    #[tokio::test]
    async fn sparse_sub_issue_removal_resolves_durable_database_id() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"node": {"parent": null}}
            })))
            .expect(2)
            .mount(&server)
            .await;
        let (_temp, projector, mut state) = ingress_state(None).await;
        let body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![
                    ProjectionInput::UpsertTask(TaskDocument {
                        source_key: "I_child".to_string(),
                        body: body.to_string(),
                        is_open: true,
                        state_reason: String::new(),
                        parent_source_key: Some("I_parent".to_string()),
                        workgraph_labels: Vec::new(),
                        workgraph_include: true,
                    }),
                    ProjectionInput::UpsertLocator(GitHubIssueLocator {
                        source_key: "I_child".to_string(),
                        repository_owner: "acme".to_string(),
                        repository_name: "widgets".to_string(),
                        issue_database_id: 42,
                        issue_number: 7,
                        issue_node_id: "I_child".to_string(),
                    }),
                ],
                1,
                "seed-sparse-removal",
            )
            .await
            .expect("seed task and locator");
        state.admission_client = Some(
            AdmissionClient::new(&WorkflowDefinitionConfig {
                token: "token".to_string(),
                api_base_url: server.uri(),
                ..WorkflowDefinitionConfig::default()
            })
            .expect("admission client"),
        );
        let event = json!({
            "action": "sub_issue_removed",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "parent_issue": {"node_id": "I_parent"},
            "sub_issue_id": 42
        });

        let inputs = try_workgraph_sub_issue(&state, &event)
            .await
            .expect("normalize sparse removal")
            .expect("task parent retraction");

        assert_eq!(
            inputs,
            vec![ProjectionInput::UpsertTask(TaskDocument {
                source_key: "I_child".to_string(),
                body: body.to_string(),
                is_open: true,
                state_reason: String::new(),
                parent_source_key: None,
                workgraph_labels: Vec::new(),
                workgraph_include: true,
            })]
        );
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), inputs, 2, "apply-sparse-removal")
            .await
            .expect("persist parent retraction");
        let stale_addition = json!({
            "action": "sub_issue_added",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "parent_issue": {"node_id": "I_parent"},
            "sub_issue": {
                "id": 42,
                "node_id": "I_child",
                "number": 7,
                "body": body,
                "state": "open",
                "updated_at": "2026-08-01T00:00:00Z",
                "type": {"node_id": "IT_task", "name": "WorkGraphTask"}
            }
        });

        let stale_inputs = try_workgraph_sub_issue(&state, &stale_addition)
            .await
            .expect("authoritatively reject stale addition")
            .expect("revision bookkeeping");
        assert!(!stale_inputs.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertTask(document)
                if document.parent_source_key.as_deref() == Some("I_parent")
        )));
    }

    #[tokio::test]
    async fn sparse_sub_issue_removal_fails_closed_for_unknown_database_id() {
        let (_temp, _projector, state) = ingress_state(None).await;
        let event = json!({
            "action": "sub_issue_removed",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "parent_issue": {"node_id": "I_parent"},
            "sub_issue_id": 999
        });

        let error = try_workgraph_sub_issue(&state, &event)
            .await
            .expect_err("unknown database ID must be retryable");

        assert!(matches!(error, WorkGraphNormError::Unavailable(message)
            if message.contains("database ID 999")));
    }

    #[tokio::test]
    async fn delayed_sparse_removal_preserves_authoritatively_restored_parent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"node": {"parent": {"id": "I_parent"}}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_temp, projector, mut state) = ingress_state(None).await;
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![
                    ProjectionInput::UpsertTask(TaskDocument {
                        source_key: "I_child".to_string(),
                        body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
                        is_open: true,
                        state_reason: String::new(),
                        parent_source_key: Some("I_parent".to_string()),
                        workgraph_labels: Vec::new(),
                        workgraph_include: true,
                    }),
                    ProjectionInput::UpsertLocator(GitHubIssueLocator {
                        source_key: "I_child".to_string(),
                        repository_owner: "acme".to_string(),
                        repository_name: "widgets".to_string(),
                        issue_database_id: 42,
                        issue_number: 7,
                        issue_node_id: "I_child".to_string(),
                    }),
                ],
                1,
                "seed-restored-parent",
            )
            .await
            .expect("seed task and locator");
        state.admission_client = Some(
            AdmissionClient::new(&WorkflowDefinitionConfig {
                token: "token".to_string(),
                api_base_url: server.uri(),
                ..WorkflowDefinitionConfig::default()
            })
            .expect("admission client"),
        );
        let event = json!({
            "action": "sub_issue_removed",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "parent_issue": {"node_id": "I_parent"},
            "sub_issue_id": 42
        });

        assert!(try_workgraph_sub_issue(&state, &event)
            .await
            .expect("reconcile delayed removal")
            .is_none());
        assert_eq!(
            state
                .allocator
                .latest_workgraph_task("I_child")
                .await
                .expect("read task")
                .and_then(|task| task.parent_source_key)
                .as_deref(),
            Some("I_parent")
        );
    }

    #[tokio::test]
    async fn delayed_full_removal_converges_to_new_authoritative_parent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"node": {"parent": {"id": "I_parent_b"}}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_temp, projector, mut state) = ingress_state(None).await;
        let body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![ProjectionInput::UpsertTask(TaskDocument {
                    source_key: "I_child".to_string(),
                    body: body.to_string(),
                    is_open: true,
                    state_reason: String::new(),
                    parent_source_key: Some("I_parent_b".to_string()),
                    workgraph_labels: Vec::new(),
                    workgraph_include: true,
                })],
                1,
                "seed-new-parent",
            )
            .await
            .expect("seed task");
        state.admission_client = Some(
            AdmissionClient::new(&WorkflowDefinitionConfig {
                token: "token".to_string(),
                api_base_url: server.uri(),
                ..WorkflowDefinitionConfig::default()
            })
            .expect("admission client"),
        );
        let event = json!({
            "action": "sub_issue_removed",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "parent_issue": {"node_id": "I_parent_a"},
            "sub_issue": {
                "id": 42,
                "node_id": "I_child",
                "number": 7,
                "body": body,
                "state": "open",
                "updated_at": "2026-08-01T00:00:00Z",
                "type": {"node_id": "IT_task", "name": "WorkGraphTask"}
            }
        });

        let inputs = try_workgraph_sub_issue(&state, &event)
            .await
            .expect("reconcile delayed full removal")
            .expect("task projection");

        assert!(inputs.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertTask(document)
                if document.parent_source_key.as_deref() == Some("I_parent_b")
        )));
    }

    #[tokio::test]
    async fn signed_sparse_removal_commits_once_and_clears_parent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"node": {"parent": null}}
            })))
            .expect(2)
            .mount(&server)
            .await;
        let (_temp, projector, mut state) = ingress_state(None).await;
        let body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![
                    ProjectionInput::UpsertTask(TaskDocument {
                        source_key: "I_child".to_string(),
                        body: body.to_string(),
                        is_open: true,
                        state_reason: String::new(),
                        parent_source_key: Some("I_parent".to_string()),
                        workgraph_labels: Vec::new(),
                        workgraph_include: true,
                    }),
                    ProjectionInput::UpsertLocator(GitHubIssueLocator {
                        source_key: "I_child".to_string(),
                        repository_owner: "acme".to_string(),
                        repository_name: "widgets".to_string(),
                        issue_database_id: 42,
                        issue_number: 7,
                        issue_node_id: "I_child".to_string(),
                    }),
                ],
                1,
                "seed-signed-removal",
            )
            .await
            .expect("seed task and locator");
        state.admission_client = Some(
            AdmissionClient::new(&WorkflowDefinitionConfig {
                token: "token".to_string(),
                api_base_url: server.uri(),
                ..WorkflowDefinitionConfig::default()
            })
            .expect("admission client"),
        );
        let payload = json!({
            "action": "sub_issue_removed",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "parent_issue": {"node_id": "I_parent"},
            "sub_issue_id": 42
        });
        let body = serde_json::to_vec(&payload).expect("serialize payload");
        let headers = signed_headers("sub_issues", "sparse-removal", &body);

        handle_delivery(&state, &headers, &body)
            .await
            .expect("signed sparse removal");
        assert!(handle_delivery(&state, &headers, &body)
            .await
            .expect("deduplicate removal")
            .is_none());
        let committed = projector.committed.lock().await;
        assert!(committed.last().is_some_and(|inputs| matches!(
            inputs.as_slice(),
            [ProjectionInput::UpsertTask(document)]
                if document.source_key == "I_child"
                    && document.parent_source_key.is_none()
        )));
    }

    #[tokio::test]
    async fn sub_issue_event_cannot_reclassify_an_admitted_root_issue() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "body": "ordinary issue",
                        "issueType": null,
                        "labels": {
                            "nodes": [{"name": "workgraph"}],
                            "pageInfo": {"hasNextPage": false}
                        }
                    }
                }
            })))
            .mount(&server)
            .await;
        let (_temp, projector, mut state) = ingress_state(None).await;
        state.admission_client = Some(
            AdmissionClient::new(&WorkflowDefinitionConfig {
                token: "token".to_string(),
                api_base_url: server.uri(),
                ..WorkflowDefinitionConfig::default()
            })
            .expect("admission client"),
        );
        let admitted = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload("labeled", root_issue(&["workgraph"])),
        )
        .await
        .expect("normalize admission")
        .expect("admission inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), admitted, 1, "root-1")
            .await
            .expect("persist admission");
        let event = json!({
            "action": "sub_issue_added",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "parent_issue": {"node_id": "I_parent"},
            "sub_issue": {
                "node_id": "I_root",
                "number": 6,
                "body": "WorkGraphTask/v1\n\n```json\n{}\n```\n",
                "updated_at": "2026-08-01T00:01:00Z",
                "type": {"node_id": "IT_task", "name": "WorkGraphTask"}
            }
        });

        assert!(try_workgraph_sub_issue(&state, &event)
            .await
            .expect("normalize hierarchy event")
            .is_none());
        assert!(state
            .allocator
            .latest_workgraph_task("I_root")
            .await
            .expect("read task")
            .is_none());
    }

    #[tokio::test]
    async fn newer_authoritative_sub_issue_transition_replaces_stale_root_classification() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "body": "ordinary issue",
                        "issueType": null,
                        "labels": {
                            "nodes": [],
                            "pageInfo": {"hasNextPage": false}
                        }
                    }
                }
            })))
            .mount(&server)
            .await;
        let (_temp, projector, mut state) = ingress_state(Some(task_trust())).await;
        state.admission_client = Some(
            AdmissionClient::new(&WorkflowDefinitionConfig {
                token: "token".to_string(),
                api_base_url: server.uri(),
                ..WorkflowDefinitionConfig::default()
            })
            .expect("admission client"),
        );
        let admitted = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload("labeled", root_issue(&["workgraph"])),
        )
        .await
        .expect("normalize admission")
        .expect("admission inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), admitted, 1, "root-1")
            .await
            .expect("persist admission");

        let body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        let mut child = task_issue("I_root", body);
        child["updated_at"] = json!("2026-08-01T00:02:00Z");
        let event = json!({
            "action": "sub_issue_added",
            "organization": {"login": "acme"},
            "repository": {
                "name": "widgets",
                "full_name": "acme/widgets",
                "node_id": "R_widgets"
            },
            "sender": {"node_id": "U_creator", "login": "task-creator"},
            "parent_issue": {"node_id": "I_parent"},
            "sub_issue": child
        });
        let transition = try_workgraph_sub_issue(&state, &event)
            .await
            .expect("normalize authoritative hierarchy transition")
            .expect("hierarchy transition");
        assert!(transition.iter().any(|input| matches!(
            input,
            ProjectionInput::DeleteRootIssue { source_key } if source_key == "I_root"
        )));
        assert!(transition.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertTask(document)
                if document.source_key == "I_root"
                    && document.parent_source_key.as_deref() == Some("I_parent")
        )));
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), transition, 2, "task-2")
            .await
            .expect("persist hierarchy transition");
        assert!(state
            .allocator
            .latest_workgraph_root_issue("I_root")
            .await
            .expect("read Root Issue")
            .is_none());
        assert_eq!(
            state
                .allocator
                .latest_workgraph_task("I_root")
                .await
                .expect("read task")
                .expect("task classification")
                .parent_source_key
                .as_deref(),
            Some("I_parent")
        );

        let mut delayed_root = root_issue(&["workgraph"]);
        delayed_root["updated_at"] = json!("2026-08-01T00:01:00Z");
        assert!(
            try_workgraph_issue(&state, "delivery-stale", &payload("labeled", delayed_root),)
                .await
                .expect("normalize stale Root Issue")
                .is_none()
        );
    }

    #[tokio::test]
    async fn equal_revision_readmission_uses_authoritative_github_state() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "body": "ordinary issue",
                        "issueType": null,
                        "labels": {
                            "nodes": [{"name": "workgraph"}],
                            "pageInfo": {"hasNextPage": false}
                        }
                    }
                }
            })))
            .mount(&server)
            .await;
        let (_temp, projector, mut state) = ingress_state(None).await;
        state.admission_client = Some(
            AdmissionClient::new(&WorkflowDefinitionConfig {
                token: "token".to_string(),
                api_base_url: server.uri(),
                ..WorkflowDefinitionConfig::default()
            })
            .expect("admission client"),
        );
        let admitted = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload("labeled", root_issue(&["workgraph"])),
        )
        .await
        .expect("normalize admission")
        .expect("admission inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), admitted, 1, "root-1")
            .await
            .expect("persist admission");

        let mut removed = root_issue(&[]);
        removed["updated_at"] = json!("2026-08-01T00:01:00Z");
        let retraction = try_workgraph_issue(&state, "delivery-2", &payload("unlabeled", removed))
            .await
            .expect("normalize retraction")
            .expect("retraction inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), retraction, 2, "root-2")
            .await
            .expect("persist retraction");

        let mut readmitted = root_issue(&["workgraph"]);
        readmitted["updated_at"] = json!("2026-08-01T00:01:00Z");
        assert!(
            try_workgraph_issue(&state, "delivery-3", &payload("labeled", readmitted))
                .await
                .expect("normalize equal-revision readmission")
                .is_some()
        );
    }

    #[tokio::test]
    async fn equal_revision_task_conversion_retracts_root_classification() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {
                    "node": {
                        "body": "WorkGraphTask/v1\n\n```json\n{}\n```\n",
                        "issueType": {"id": "IT_task", "name": "WorkGraphTask"},
                        "labels": {
                            "nodes": [{"name": "workgraph"}],
                            "pageInfo": {"hasNextPage": false}
                        }
                    }
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_temp, projector, mut state) = ingress_state(None).await;
        state.admission_client = Some(
            AdmissionClient::new(&WorkflowDefinitionConfig {
                token: "token".to_string(),
                api_base_url: server.uri(),
                ..WorkflowDefinitionConfig::default()
            })
            .expect("admission client"),
        );
        let admitted = try_workgraph_issue(
            &state,
            "delivery-1",
            &payload("labeled", root_issue(&["workgraph"])),
        )
        .await
        .expect("normalize admission")
        .expect("admission");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), admitted, 1, "seed-root")
            .await
            .expect("persist root");
        let mut converted = task_issue("I_root", "WorkGraphTask/v1\n\n```json\n{}\n```\n");
        converted["labels"] = json!([{"name": "workgraph"}]);

        let inputs = try_workgraph_issue(&state, "delivery-2", &payload("typed", converted))
            .await
            .expect("normalize equal-revision conversion")
            .expect("root retraction");

        assert!(inputs.iter().any(|input| matches!(
            input,
            ProjectionInput::DeleteRootIssue { source_key } if source_key == "I_root"
        )));
        assert!(!inputs
            .iter()
            .any(|input| matches!(input, ProjectionInput::UpsertRootIssue(_))));
    }

    #[tokio::test]
    async fn labeled_generated_task_remains_a_sub_issue_task() {
        let (_temp, _projector, state) = ingress_state(Some(task_trust())).await;
        let event = json!({
            "action": "sub_issue_added",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "sender": {"node_id": "U_creator", "login": "task-creator"},
            "parent_issue": {"node_id": "I_parent"},
            "sub_issue": {
                "id": 41,
                "node_id": "I_root",
                "number": 6,
                "body": "WorkGraphTask/v1\n\n```json\n{}\n```\n",
                "state": "open",
                "state_reason": null,
                "updated_at": "2026-08-01T00:00:00Z",
                "user": {"node_id": "U_creator", "login": "task-creator"},
                "labels": [{"name": "workgraph"}],
                "type": {"node_id": "IT_task", "name": "WorkGraphTask"}
            }
        });

        let inputs = try_workgraph_sub_issue(&state, &event)
            .await
            .expect("normalize hierarchy event")
            .expect("generated sub-issue task");
        assert!(inputs.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertTask(document)
                if document.source_key == "I_root"
                    && document.parent_source_key.as_deref() == Some("I_parent")
        )));
    }

    #[tokio::test]
    async fn task_marked_prior_task_converges_hierarchy_when_type_is_missing() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"node": {"parent": {"id": "I_parent"}}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_temp, projector, mut state) = ingress_state(Some(task_trust())).await;
        let body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![ProjectionInput::UpsertTask(TaskDocument {
                    source_key: "I_task".to_string(),
                    body: body.to_string(),
                    is_open: true,
                    state_reason: String::new(),
                    parent_source_key: None,
                    workgraph_labels: Vec::new(),
                    workgraph_include: true,
                })],
                1,
                "seed-untyped-hierarchy-task",
            )
            .await
            .expect("seed task");
        state.admission_client = Some(
            AdmissionClient::new(&WorkflowDefinitionConfig {
                token: "token".to_string(),
                api_base_url: server.uri(),
                ..WorkflowDefinitionConfig::default()
            })
            .expect("admission client"),
        );
        let event = json!({
            "action": "sub_issue_added",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "sender": {"node_id": "U_creator", "login": "task-creator"},
            "parent_issue": {"node_id": "I_parent"},
            "sub_issue": {
                "id": 42,
                "node_id": "I_task",
                "number": 7,
                "body": body,
                "state": "open",
                "updated_at": "2026-08-01T00:00:00Z",
                "user": {"node_id": "U_creator", "login": "task-creator"},
                "labels": [{"name": "workgraph"}],
                "type": null
            }
        });

        let inputs = try_workgraph_sub_issue(&state, &event)
            .await
            .expect("normalize hierarchy")
            .expect("task hierarchy update");

        assert!(inputs.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertTask(document)
                if document.parent_source_key.as_deref() == Some("I_parent")
        )));
    }

    #[tokio::test]
    async fn delayed_sub_issue_delivery_cannot_overwrite_newer_exclusion_state() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        let excluded = TaskDocument {
            source_key: "I_child".to_string(),
            body: "WorkGraphTask/v1\n\n```json\n{}\n```\n".to_string(),
            is_open: true,
            state_reason: String::new(),
            parent_source_key: Some("I_root".to_string()),
            workgraph_labels: vec!["workgraph:ignore".to_string()],
            workgraph_include: false,
        };
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![
                    ProjectionInput::RecordIssueRevision {
                        source_key: "I_child".to_string(),
                        revision: chrono::DateTime::parse_from_rfc3339("2026-08-02T00:00:00Z")
                            .expect("revision")
                            .timestamp_millis(),
                    },
                    ProjectionInput::UpsertTask(excluded),
                    ProjectionInput::UpsertLocator(GitHubIssueLocator {
                        source_key: "I_child".to_string(),
                        repository_owner: "acme".to_string(),
                        repository_name: "widgets".to_string(),
                        issue_database_id: 42,
                        issue_number: 7,
                        issue_node_id: "I_child".to_string(),
                    }),
                ],
                1,
                "newer-exclusion",
            )
            .await
            .expect("persist newer exclusion");

        let mut child = task_issue("I_child", "WorkGraphTask/v1\n\n```json\n{}\n```\n");
        child["labels"] = json!([]);
        child["updated_at"] = json!("2026-08-01T00:00:00Z");
        let delayed = json!({
            "action": "sub_issue_added",
            "organization": {"login": "acme"},
            "repository": {
                "name": "widgets",
                "full_name": "acme/widgets",
                "node_id": "R_widgets"
            },
            "sender": {"node_id": "U_creator", "login": "task-creator"},
            "parent_issue": {"node_id": "I_root"},
            "sub_issue": child
        });

        assert!(try_workgraph_sub_issue(&state, &delayed)
            .await
            .expect("ignore stale delivery")
            .is_none());
        assert!(state
            .allocator
            .latest_workgraph_task("I_child")
            .await
            .expect("read durable task")
            .is_some_and(
                |task| !task.workgraph_include && task.workgraph_labels == ["workgraph:ignore"]
            ));
    }

    #[tokio::test]
    async fn sub_issue_addition_requires_trusted_task_creator_and_actor() {
        let (_temp, _projector, state) = ingress_state(Some(task_trust())).await;
        let event = json!({
            "action": "sub_issue_added",
            "organization": {"login": "acme"},
            "repository": {
                "name": "widgets",
                "full_name": "acme/widgets",
                "node_id": "R_widgets"
            },
            "sender": {"node_id": "U_creator", "login": "task-creator"},
            "parent_issue": {"node_id": "I_root"},
            "sub_issue": task_issue(
                "I_child",
                "WorkGraphTask/v1\n\n```json\n{}\n```\n"
            )
        });
        let inputs = try_workgraph_sub_issue(&state, &event)
            .await
            .expect("normalize")
            .expect("sub-issue task");
        assert!(inputs.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertTask(document)
                if document.parent_source_key.as_deref() == Some("I_root")
        )));

        let mut untrusted = event;
        untrusted["sender"]["login"] = json!("other");
        assert!(matches!(
            try_workgraph_sub_issue(&state, &untrusted).await,
            Err(WorkGraphNormError::Untrusted(_))
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
            path: ".github/workgraph/workflows/issue-lifecycle-v1.body".to_string(),
            token: "token".to_string(),
            api_base_url: server.uri(),
        });
        let event = json!({
            "ref": "refs/heads/main",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "commits": [{
                "added": [],
                "modified": [".github/workgraph/workflows/issue-lifecycle-v1.body"],
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
                        == "github:definition:acme/widgets:main:.github/workgraph/workflows/issue-lifecycle-v1.body"
        ));
    }
}
