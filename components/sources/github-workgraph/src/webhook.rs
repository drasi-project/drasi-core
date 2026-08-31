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
use crate::lease_ledger::{root_comment_fingerprint, Allocator, RootIssueCommentRevisionState};
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
use serde::{Deserialize, Serialize};
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LeaseValidationResponse {
    lease_id: String,
    task_id: String,
    assignment_id: String,
    attempt: u64,
    executor_id: String,
    slot_id: String,
    claim_id: String,
    acquired_at: String,
    expires_at: String,
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
            Json(LeaseValidationResponse {
                lease_id: active.lease_id,
                task_id: active.task_id,
                assignment_id: active.assignment_id,
                attempt: active.attempt,
                executor_id: active.executor_id,
                slot_id: active.slot_id,
                claim_id: request.claim_id,
                acquired_at: active.acquired_at,
                expires_at: active.expires_at,
            }),
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase", tag = "presence")]
enum IssueAuthorityState {
    Absent,
    Present {
        source_key: String,
        repository_owner: String,
        repository_name: String,
        repository_node_id: String,
        issue_database_id: u64,
        issue_number: u64,
        title: String,
        body: String,
        is_open: bool,
        state_reason: String,
        labels: Vec<String>,
        issue_type_id: String,
        issue_type_name: String,
        parent_source_key: Option<String>,
        classification: String,
    },
}

impl IssueAuthorityState {
    fn fingerprint(&self) -> Result<String> {
        let encoded = serde_json::to_vec(self).context("failed to encode canonical Issue state")?;
        Ok(hex::encode(Sha256::digest(encoded)))
    }
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

    async fn issue_authority_state(
        &self,
        node_id: &str,
        task_issue_type: &crate::config::TaskIssueType,
    ) -> Result<IssueAuthorityState> {
        let response = self
            .http
            .post(&self.api_url)
            .json(&json!({
                "query": "query($id: ID!) { node(id: $id) { ... on Issue { id databaseId number title body state stateReason issueType { id name } parent { id } repository { id name owner { login } } labels(first: 100) { nodes { name } pageInfo { hasNextPage } } } } }",
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
        let Some(issue) = payload.pointer("/data/node").and_then(Value::as_object) else {
            return Ok(IssueAuthorityState::Absent);
        };
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
        let mut label_names = labels
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
            .map(str::to_string)
            .collect::<Vec<_>>();
        label_names.sort();
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
        let body = issue
            .get("body")
            .and_then(Value::as_str)
            .context("authoritative task state lookup omitted the body")?
            .to_string();
        let admitted = label_names
            .iter()
            .any(|label| label == WORKGRAPH_ADMISSION_LABEL);
        let classification =
            if task_typed && body.starts_with(crate::protocol::WORKGRAPH_TASK_MARKER) {
                "task"
            } else if admitted
                && !task_typed
                && !body.starts_with(crate::protocol::WORKGRAPH_TASK_MARKER)
            {
                "root"
            } else {
                "generic"
            };
        let repository = issue
            .get("repository")
            .and_then(Value::as_object)
            .context("authoritative task state lookup omitted the repository")?;
        Ok(IssueAuthorityState::Present {
            source_key: node_id.to_string(),
            repository_owner: repository
                .get("owner")
                .and_then(Value::as_object)
                .and_then(|owner| owner.get("login"))
                .and_then(Value::as_str)
                .context("authoritative task state lookup omitted the repository owner")?
                .to_string(),
            repository_name: repository
                .get("name")
                .and_then(Value::as_str)
                .context("authoritative task state lookup omitted the repository name")?
                .to_string(),
            repository_node_id: repository
                .get("id")
                .and_then(Value::as_str)
                .context("authoritative task state lookup omitted the repository ID")?
                .to_string(),
            issue_database_id: issue
                .get("databaseId")
                .and_then(Value::as_u64)
                .context("authoritative task state lookup omitted the database ID")?,
            issue_number: issue
                .get("number")
                .and_then(Value::as_u64)
                .context("authoritative task state lookup omitted the issue number")?,
            title: issue
                .get("title")
                .and_then(Value::as_str)
                .context("authoritative task state lookup omitted the title")?
                .to_string(),
            body,
            is_open,
            state_reason: issue
                .get("stateReason")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_ascii_lowercase(),
            labels: label_names,
            issue_type_id: issue_type
                .and_then(|issue_type| issue_type.get("id"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            issue_type_name: issue_type
                .and_then(|issue_type| issue_type.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            parent_source_key,
            classification: classification.to_string(),
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
        "issue_comment" => try_workgraph_comment(state, payload).await,
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

fn task_document(
    issue: &serde_json::Value,
    source_key: &str,
    parent_source_key: Option<String>,
) -> crate::protocol::TaskDocument {
    let (workgraph_labels, workgraph_include) = issue_workgraph_labels(issue);
    crate::protocol::TaskDocument {
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

fn issue_authority_state(
    issue: &serde_json::Value,
    repository: Option<&serde_json::Value>,
    parent_source_key: Option<String>,
    task_issue_type: &crate::config::TaskIssueType,
    absent: bool,
) -> Result<IssueAuthorityState, WorkGraphNormError> {
    if absent {
        return Ok(IssueAuthorityState::Absent);
    }
    let locator = extract_issue_locator_from_repository(issue, repository);
    let source_key = issue
        .get("node_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            WorkGraphNormError::InvalidPayload("Issue state is missing 'node_id'".to_string())
        })?;
    let repository_full_name = repository
        .and_then(|repository| repository.get("full_name"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let (repository_owner, repository_name) =
        repository_full_name.split_once('/').unwrap_or(("", ""));
    let repository_node_id = repository
        .and_then(|repository| repository.get("node_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("");
    let mut labels = issue
        .get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|label| {
            label
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    WorkGraphNormError::InvalidPayload(
                        "Issue state contains an invalid label".to_string(),
                    )
                })
                .map(str::to_string)
        })
        .collect::<Result<Vec<_>, _>>()?;
    labels.sort();
    let issue_type = issue.get("type");
    let issue_type_id = issue_type
        .and_then(|issue_type| issue_type.get("node_id"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let issue_type_name = issue_type
        .and_then(|issue_type| issue_type.get("name"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let body = issue
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let classification = if body.starts_with(crate::protocol::WORKGRAPH_TASK_MARKER)
        && task_issue_type.matches(issue_type)
    {
        "task"
    } else if issue_is_root_candidate(issue, task_issue_type) {
        "root"
    } else {
        "generic"
    };
    Ok(IssueAuthorityState::Present {
        source_key: source_key.to_string(),
        repository_owner: locator
            .as_ref()
            .map(|locator| locator.repository_owner.as_str())
            .unwrap_or(repository_owner)
            .to_string(),
        repository_name: locator
            .as_ref()
            .map(|locator| locator.repository_name.as_str())
            .unwrap_or(repository_name)
            .to_string(),
        repository_node_id: repository_node_id.to_string(),
        issue_database_id: locator
            .as_ref()
            .map(|locator| locator.issue_database_id)
            .unwrap_or_default(),
        issue_number: locator
            .as_ref()
            .map(|locator| locator.issue_number)
            .or_else(|| issue.get("number").and_then(Value::as_u64))
            .unwrap_or_default(),
        title: issue
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        body,
        is_open: item_is_open(issue),
        state_reason: issue
            .get("state_reason")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        labels,
        issue_type_id,
        issue_type_name,
        parent_source_key,
        classification: classification.to_string(),
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

fn lifecycle_created_revision(comment: &serde_json::Value) -> Result<i64, WorkGraphNormError> {
    let value = comment
        .get("created_at")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            WorkGraphNormError::InvalidPayload(
                "lifecycle comment is missing 'created_at'".to_string(),
            )
        })?;
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp_millis())
        .map_err(|error| {
            WorkGraphNormError::InvalidPayload(format!(
                "lifecycle comment has invalid 'created_at': {error}"
            ))
        })
}

fn comment_updated_revision(comment: &serde_json::Value) -> Result<i64, WorkGraphNormError> {
    let value = comment
        .get("updated_at")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            WorkGraphNormError::InvalidPayload("issue comment is missing 'updated_at'".to_string())
        })?;
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|value| value.timestamp_millis())
        .map_err(|error| {
            WorkGraphNormError::InvalidPayload(format!(
                "issue comment has invalid 'updated_at': {error}"
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
    let exclusion_label_event = payload
        .pointer("/label/name")
        .and_then(Value::as_str)
        .is_some_and(|label| matches!(label, WORKGRAPH_IGNORE_LABEL | WORKGRAPH_ERROR_LABEL));
    let authorization_transition = exclusion_label_event
        && ((action == "labeled" && !workgraph_include)
            || (action == "unlabeled" && workgraph_include));
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
    let previous_state_fingerprint = state
        .allocator
        .latest_workgraph_issue_state_fingerprint(node_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let incoming_state = issue_authority_state(
        issue,
        payload.get("repository"),
        previous_task
            .as_ref()
            .and_then(|task| task.parent_source_key.clone()),
        &state.task_issue_type,
        matches!(action, "deleted" | "transferred"),
    )?;
    let incoming_state_fingerprint = incoming_state
        .fingerprint()
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let revision = {
        let revision = authoritative_issue_revision(issue)?;
        if let Some(previous) = previous_revision {
            if revision < previous {
                return Ok(None);
            }
            if revision == previous
                && previous_state_fingerprint.as_deref()
                    != Some(incoming_state_fingerprint.as_str())
            {
                let client = state.admission_client.as_ref().ok_or_else(|| {
                    WorkGraphNormError::Unavailable(
                        "equal-revision Issue state transition requires an authoritative GitHub read"
                            .to_string(),
                    )
                })?;
                let authoritative = client
                    .issue_authority_state(node_id, &state.task_issue_type)
                    .await
                    .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?;
                let authoritative_fingerprint = authoritative
                    .fingerprint()
                    .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?;
                if authoritative_fingerprint != incoming_state_fingerprint {
                    return Ok(None);
                }
            }
        }
        Some(revision)
    };
    let mut inputs = revision
        .map(|revision| {
            vec![ProjectionInput::RecordIssueRevision {
                source_key: node_id.to_string(),
                revision,
                state_fingerprint: incoming_state_fingerprint,
                authorization_transition,
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
            if item_is_open(issue) {
                inputs.push(ProjectionInput::UpsertGitHubIssue(normalize_github_issue(
                    issue, payload,
                )?));
            } else {
                inputs.push(ProjectionInput::DeleteGitHubIssue {
                    source_key: node_id.to_string(),
                });
            }
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
        let task_doc = task_document(
            issue,
            node_id,
            previous_task.and_then(|document| document.parent_source_key),
        );
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

/// Normalize a task lifecycle artifact or human Root Issue comment.
async fn try_workgraph_comment(
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
    let issue = payload.get("issue").ok_or_else(|| {
        WorkGraphNormError::InvalidPayload("issue_comment missing 'issue'".to_string())
    })?;
    let issue_node_id = issue
        .get("node_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| WorkGraphNormError::InvalidPayload("issue missing 'node_id'".to_string()))?;
    let prior_root_comment = state
        .allocator
        .latest_workgraph_root_comment_revision(comment_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let updated_at_revision = comment_updated_revision(comment)?;
    let current_trust_role = lifecycle_trust_role(body);
    let mut inputs = Vec::new();

    if action == "deleted" {
        authorize_workgraph_repository(state, payload, None)?;
        if previous_workgraph || current_trust_role.is_some() {
            inputs.push(ProjectionInput::DeleteLifecycleArtifact {
                source_key: comment_id.to_string(),
            });
        }
        if let Some(previous) = prior_root_comment
            .filter(|previous| should_accept_root_comment_tombstone(previous, updated_at_revision))
        {
            inputs.push(root_comment_deletion(&previous, updated_at_revision));
        }
        return Ok((!inputs.is_empty()).then_some(inputs));
    }

    if action == "edited" && current_trust_role.is_none() && previous_workgraph {
        authorize_workgraph_repository(state, payload, None)?;
        inputs.push(ProjectionInput::DeleteLifecycleArtifact {
            source_key: comment_id.to_string(),
        });
    }

    if let Some(trust_role) = current_trust_role {
        authorize_workgraph_repository(state, payload, None)?;
        if let Some(previous) = prior_root_comment
            .filter(|previous| should_accept_root_comment_tombstone(previous, updated_at_revision))
        {
            inputs.push(root_comment_deletion(&previous, updated_at_revision));
        }
        normalize_lifecycle_artifact(
            state,
            payload,
            comment,
            comment_id,
            body,
            action,
            issue,
            issue_node_id,
            trust_role,
            &mut inputs,
        )?;
        return Ok((!inputs.is_empty()).then_some(inputs));
    }

    let Some(root) = state
        .allocator
        .latest_workgraph_root_issue(issue_node_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?
    else {
        return Ok((!inputs.is_empty()).then_some(inputs));
    };
    authorize_workgraph_repository(state, payload, None)?;
    if !root_comment_issue_matches_cached_admission(state, payload, issue, &root).await? {
        return Ok((!inputs.is_empty()).then_some(inputs));
    }
    if action == "edited" {
        let editor = comment
            .get("editor")
            .filter(|editor| !editor.is_null())
            .or_else(|| payload.get("sender").filter(|sender| !sender.is_null()));
        if editor.is_none_or(identity_is_bot_or_agent) {
            if let Some(previous) = prior_root_comment.as_ref().filter(|previous| {
                should_accept_root_comment_tombstone(previous, updated_at_revision)
            }) {
                inputs.push(root_comment_deletion(previous, updated_at_revision));
            }
            return Ok((!inputs.is_empty()).then_some(inputs));
        }
    }
    let author = comment.get("user").ok_or_else(|| {
        WorkGraphNormError::InvalidPayload("Root Issue comment has no author".to_string())
    })?;
    let author_id = required_string(author, "node_id", "Root Issue comment author")?;
    let author_type = required_string(author, "type", "Root Issue comment author")?;
    let author_login = required_string(author, "login", "Root Issue comment author")?;
    if identity_is_bot_or_agent(author) {
        return Ok((!inputs.is_empty()).then_some(inputs));
    }
    if body.len() > MAX_ROOT_ISSUE_COMMENT_BODY_BYTES {
        return Err(WorkGraphNormError::InvalidPayload(format!(
            "Root Issue comment body exceeds {MAX_ROOT_ISSUE_COMMENT_BODY_BYTES} bytes"
        )));
    }
    let document = RootIssueCommentDocument {
        source_key: comment_id.to_string(),
        root_issue_id: root.source_key,
        admission_id: root.admission_id,
        repository_owner: root.repository_owner,
        repository_name: root.repository_name,
        repository_node_id: root.repository_node_id,
        issue_number: root.issue_number,
        author_id: author_id.to_string(),
        author_type: author_type.to_string(),
        author_login: author_login.to_string(),
        body: body.to_string(),
        created_at_revision: lifecycle_created_revision(comment)?,
        updated_at_revision,
    };
    if document.created_at_revision > document.updated_at_revision {
        return Err(WorkGraphNormError::InvalidPayload(
            "Root Issue comment updated_at precedes created_at".to_string(),
        ));
    }
    let fingerprint = root_comment_fingerprint(&document)
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let accept = match prior_root_comment.as_ref() {
        Some(previous) => {
            should_accept_root_comment_upsert(previous, document.updated_at_revision, &fingerprint)?
        }
        None => true,
    };
    if accept {
        inputs.push(ProjectionInput::UpsertRootIssueComment(document));
    }
    Ok((!inputs.is_empty()).then_some(inputs))
}

#[allow(clippy::too_many_arguments)]
fn normalize_lifecycle_artifact(
    state: &IngressState,
    payload: &serde_json::Value,
    comment: &serde_json::Value,
    comment_id: &str,
    body: &str,
    action: &str,
    issue: &serde_json::Value,
    task_node_id: &str,
    trust_role: crate::protocol::LifecycleTrustRole,
    inputs: &mut Vec<crate::protocol::ProjectionInput>,
) -> Result<(), WorkGraphNormError> {
    use crate::protocol::*;

    let task_body = issue
        .get("body")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    if !task_body.starts_with(WORKGRAPH_TASK_MARKER)
        || !state.task_issue_type.matches(issue.get("type"))
    {
        return Ok(());
    }

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

    let artifact = LifecycleArtifactDocument {
        source_key: comment_id.to_string(),
        task_source_key: task_node_id.to_string(),
        body: body.to_string(),
        created_at_revision: lifecycle_created_revision(comment)?,
    };
    inputs.push(ProjectionInput::UpsertLifecycleArtifact(artifact));
    Ok(())
}

fn should_accept_root_comment_tombstone(
    previous: &RootIssueCommentRevisionState,
    revision: i64,
) -> bool {
    revision > previous.revision || revision == previous.revision && !previous.tombstone
}

fn should_accept_root_comment_upsert(
    previous: &RootIssueCommentRevisionState,
    revision: i64,
    fingerprint: &str,
) -> Result<bool, WorkGraphNormError> {
    if previous.tombstone || revision < previous.revision {
        return Ok(false);
    }
    if revision > previous.revision {
        return Ok(true);
    }
    if fingerprint == previous.fingerprint {
        return Ok(false);
    }
    Err(WorkGraphNormError::Unavailable(
        "equal-revision Root Issue comment content is ambiguous; redeliver after an authoritative \
         comment read"
            .to_string(),
    ))
}

fn root_comment_deletion(
    previous: &RootIssueCommentRevisionState,
    updated_at_revision: i64,
) -> crate::protocol::ProjectionInput {
    crate::protocol::ProjectionInput::DeleteRootIssueComment {
        source_key: previous.identity.source_key.clone(),
        root_issue_id: previous.identity.root_issue_id.clone(),
        admission_id: previous.identity.admission_id.clone(),
        repository_owner: previous.identity.repository_owner.clone(),
        repository_name: previous.identity.repository_name.clone(),
        repository_node_id: previous.identity.repository_node_id.clone(),
        issue_number: previous.identity.issue_number,
        updated_at_revision,
    }
}

fn required_string<'a>(
    value: &'a Value,
    field: &str,
    subject: &str,
) -> Result<&'a str, WorkGraphNormError> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            WorkGraphNormError::InvalidPayload(format!("{subject} is missing non-empty '{field}'"))
        })
}

fn identity_is_bot_or_agent(identity: &Value) -> bool {
    identity
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("bot"))
        || identity
            .get("login")
            .and_then(Value::as_str)
            .is_some_and(|login| login.to_ascii_lowercase().ends_with("[bot]"))
}

async fn root_comment_issue_matches_cached_admission(
    state: &IngressState,
    payload: &Value,
    issue: &Value,
    root: &crate::protocol::RootIssueDocument,
) -> Result<bool, WorkGraphNormError> {
    if !root.is_open
        || !root.workgraph_include
        || !issue_is_root_candidate(issue, &state.task_issue_type)
        || !item_is_open(issue)
    {
        return Ok(false);
    }
    let locator = extract_issue_locator(issue, payload).ok_or_else(|| {
        WorkGraphNormError::InvalidPayload(
            "Root Issue comment payload is missing its repository locator".to_string(),
        )
    })?;
    let repository_node_id = payload
        .pointer("/repository/node_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let (workgraph_labels, workgraph_include) = issue_workgraph_labels(issue);
    let payload_matches = locator.source_key == root.source_key
        && locator.repository_owner == root.repository_owner
        && locator.repository_name == root.repository_name
        && locator.issue_number == root.issue_number
        && repository_node_id == root.repository_node_id
        && workgraph_labels == root.workgraph_labels
        && workgraph_include == root.workgraph_include;
    let revision = authoritative_issue_revision(issue)?;
    let cached_revision = state
        .allocator
        .latest_workgraph_issue_revision(&root.source_key)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?
        .ok_or_else(|| {
            WorkGraphNormError::Unavailable(
                "Root Issue comment arrived before its admission revision was durable".to_string(),
            )
        })?;
    if revision < cached_revision {
        return Ok(false);
    }
    if revision > cached_revision || !payload_matches {
        return Err(WorkGraphNormError::Unavailable(
            "Root Issue comment admission snapshot is ambiguous; redeliver after the Issue state \
             converges"
                .to_string(),
        ));
    }
    Ok(true)
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
    let previous_state_fingerprint = state
        .allocator
        .latest_workgraph_issue_state_fingerprint(child_node_id)
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
    let incoming_state = issue_authority_state(
        sub_issue,
        child_repository,
        task_doc.parent_source_key.clone(),
        &state.task_issue_type,
        false,
    )?;
    let incoming_state_fingerprint = incoming_state
        .fingerprint()
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    if previous_revision == Some(revision)
        && previous_state_fingerprint.as_deref() != Some(incoming_state_fingerprint.as_str())
    {
        let client = state.admission_client.as_ref().ok_or_else(|| {
            WorkGraphNormError::Unavailable(
                "equal-revision task hierarchy state transition requires an authoritative GitHub read"
                    .to_string(),
            )
        })?;
        let authoritative = client
            .issue_authority_state(child_node_id, &state.task_issue_type)
            .await
            .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?;
        let authoritative_fingerprint = authoritative
            .fingerprint()
            .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?;
        if authoritative_fingerprint != incoming_state_fingerprint {
            return Ok(None);
        }
    }

    let mut inputs = Vec::new();
    inputs.push(ProjectionInput::RecordIssueRevision {
        source_key: child_node_id.to_string(),
        revision,
        state_fingerprint: incoming_state_fingerprint,
        authorization_transition: false,
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

    #[test]
    fn lease_validation_response_includes_authoritative_numeric_attempt() {
        let response = serde_json::to_value(LeaseValidationResponse {
            lease_id: "lease-2".to_string(),
            task_id: "task".to_string(),
            assignment_id: "assignment".to_string(),
            attempt: 2,
            executor_id: "executor".to_string(),
            slot_id: "executor/1".to_string(),
            claim_id: "result-claim".to_string(),
            acquired_at: "2026-08-30T20:00:00Z".to_string(),
            expires_at: "2026-08-30T20:05:00Z".to_string(),
        })
        .expect("serialize lease validation response");

        assert_eq!(response["attempt"], json!(2));
        assert_eq!(
            response
                .as_object()
                .expect("response object")
                .keys()
                .cloned()
                .collect::<std::collections::BTreeSet<_>>(),
            [
                "acquiredAt",
                "assignmentId",
                "attempt",
                "claimId",
                "executorId",
                "expiresAt",
                "leaseId",
                "slotId",
                "taskId",
            ]
            .into_iter()
            .map(str::to_string)
            .collect()
        );
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

    async fn seed_root_issue(state: &IngressState, projector: &RecordingProjector) {
        let mut event = payload("labeled", root_issue(&["workgraph"]));
        event["label"] = json!({"name": "workgraph"});
        let inputs = try_workgraph_issue(state, "seed-root", &event)
            .await
            .expect("normalize root")
            .expect("root inputs");
        state
            .allocator
            .ingest_workgraph(projector, inputs, 1, "seed-root")
            .await
            .expect("persist root");
    }

    fn human_root_comment_event(
        comment_id: &str,
        action: &str,
        body: &str,
        updated_at: &str,
    ) -> Value {
        json!({
            "action": action,
            "organization": {"login": "acme"},
            "repository": {
                "name": "widgets",
                "full_name": "acme/widgets",
                "node_id": "R_widgets"
            },
            "sender": {
                "node_id": "U_human",
                "type": "User",
                "login": "octocat"
            },
            "issue": root_issue(&["workgraph"]),
            "comment": {
                "node_id": comment_id,
                "body": body,
                "user": {
                    "node_id": "U_human",
                    "type": "User",
                    "login": "octocat"
                },
                "created_at": "2026-08-01T00:01:00Z",
                "updated_at": updated_at
            }
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
    async fn task_and_generic_issue_classification_transitions_are_atomic() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        let task_body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        let task = try_workgraph_issue(
            &state,
            "task-first",
            &payload("opened", task_issue("I_task", task_body)),
        )
        .await
        .expect("normalize task")
        .expect("task inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), task, 1, "task-first")
            .await
            .expect("persist task");

        let mut ordinary = task_issue("I_task", "ordinary issue");
        ordinary["updated_at"] = json!("2026-08-01T00:01:00Z");
        let mut task_to_generic = payload("edited", ordinary);
        task_to_generic["changes"] = json!({"body": {"from": task_body}});
        let task_to_generic = try_workgraph_issue(&state, "task-to-generic", &task_to_generic)
            .await
            .expect("normalize task to generic")
            .expect("task to generic inputs");
        assert!(task_to_generic.iter().any(|input| matches!(
            input,
            ProjectionInput::DeleteTask { source_key } if source_key == "I_task"
        )));
        assert!(task_to_generic.iter().any(|input| matches!(
            input,
            ProjectionInput::DeleteLocator { source_key } if source_key == "I_task"
        )));
        assert!(task_to_generic.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertGitHubIssue(issue)
                if issue.source_key == "I_task" && issue.body == "ordinary issue"
        )));
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), task_to_generic, 2, "task-to-generic")
            .await
            .expect("persist generic classification");

        let mut restored_task = task_issue("I_task", task_body);
        restored_task["updated_at"] = json!("2026-08-01T00:02:00Z");
        let mut generic_to_task = payload("edited", restored_task);
        generic_to_task["changes"] = json!({"body": {"from": "ordinary issue"}});
        let generic_to_task = try_workgraph_issue(&state, "generic-to-task", &generic_to_task)
            .await
            .expect("normalize generic to task")
            .expect("generic to task inputs");
        assert!(generic_to_task.iter().any(|input| matches!(
            input,
            ProjectionInput::DeleteGitHubIssue { source_key } if source_key == "I_task"
        )));
        assert!(generic_to_task.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertTask(task)
                if task.source_key == "I_task" && task.body == task_body
        )));
        assert!(generic_to_task.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertLocator(locator) if locator.source_key == "I_task"
        )));
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
                        state_fingerprint: "0".repeat(64),
                        authorization_transition: false,
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
        let mut exclusion = payload("labeled", excluded_issue);
        exclusion["label"] = json!({"name": "workgraph:ignore"});
        let excluded = try_workgraph_issue(&state, "root-excluded", &exclusion)
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
        assert!(excluded.iter().any(|input| matches!(
            input,
            ProjectionInput::RecordIssueRevision {
                authorization_transition: true,
                ..
            }
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
        let mut reinclusion = payload("unlabeled", reincluded_issue);
        reinclusion["label"] = json!({"name": "workgraph:ignore"});
        let reincluded = try_workgraph_issue(&state, "root-reincluded", &reinclusion)
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
        assert!(reincluded.iter().any(|input| matches!(
            input,
            ProjectionInput::RecordIssueRevision {
                authorization_transition: true,
                ..
            }
        )));
    }

    #[tokio::test]
    async fn newer_explicit_reinclude_fences_delayed_older_exclusion() {
        let (_temp, projector, state) = ingress_state(None).await;
        let admitted = try_workgraph_issue(
            &state,
            "root-admitted",
            &payload("labeled", root_issue(&["workgraph"])),
        )
        .await
        .expect("normalize admission")
        .expect("admission inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), admitted, 1, "root-admitted")
            .await
            .expect("persist admission");

        let mut reincluded_issue = root_issue(&["workgraph"]);
        reincluded_issue["updated_at"] = json!("2026-08-01T00:02:00Z");
        let mut reinclusion = payload("unlabeled", reincluded_issue);
        reinclusion["label"] = json!({"name": "workgraph:ignore"});
        let reincluded = try_workgraph_issue(&state, "newer-reinclude", &reinclusion)
            .await
            .expect("normalize explicit re-inclusion")
            .expect("re-inclusion inputs");
        assert!(reincluded.iter().any(|input| matches!(
            input,
            ProjectionInput::RecordIssueRevision {
                authorization_transition: true,
                ..
            }
        )));
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), reincluded, 2, "newer-reinclude")
            .await
            .expect("persist re-inclusion fence");

        let mut excluded_issue = root_issue(&["workgraph", "workgraph:ignore"]);
        excluded_issue["updated_at"] = json!("2026-08-01T00:01:00Z");
        let mut delayed_exclusion = payload("labeled", excluded_issue);
        delayed_exclusion["label"] = json!({"name": "workgraph:ignore"});
        assert!(
            try_workgraph_issue(&state, "older-exclusion", &delayed_exclusion)
                .await
                .expect("normalize delayed exclusion")
                .is_none()
        );
        assert!(state
            .allocator
            .latest_workgraph_root_issue("I_root")
            .await
            .expect("read Root Issue")
            .is_some_and(|root| root.workgraph_include));
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
                            "databaseId": 42,
                            "number": 7,
                            "title": "",
                            "body": body,
                            "state": "CLOSED",
                            "stateReason": "COMPLETED",
                            "issueType": {"id": "IT_task", "name": "WorkGraphTask"},
                            "parent": null,
                            "repository": {
                                "id": "R_widgets",
                                "name": "widgets",
                                "owner": {"login": "acme"}
                            },
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
    async fn authoritative_reopen_wins_equal_revision_generic_and_root_close_in_both_orders() {
        for admitted_root in [false, true] {
            for close_first in [true, false] {
                let labels = if admitted_root {
                    vec![json!({"name": "workgraph"})]
                } else {
                    Vec::new()
                };
                let server = MockServer::start().await;
                Mock::given(method("POST"))
                    .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                        "data": {
                            "node": {
                                "id": "I_root",
                                "databaseId": 41,
                                "number": 6,
                                "title": "Root Issue",
                                "body": "Coordinate this work.",
                                "state": "OPEN",
                                "stateReason": null,
                                "issueType": null,
                                "parent": null,
                                "repository": {
                                    "id": "R_widgets",
                                    "name": "widgets",
                                    "owner": {"login": "acme"}
                                },
                                "labels": {
                                    "nodes": labels,
                                    "pageInfo": {"hasNextPage": false}
                                }
                            }
                        }
                    })))
                    .expect(2)
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
                let admission_labels = if admitted_root {
                    vec!["workgraph"]
                } else {
                    Vec::new()
                };
                let initial_issue = root_issue(&admission_labels);
                let initial =
                    try_workgraph_issue(&state, "initial-open", &payload("opened", initial_issue))
                        .await
                        .expect("normalize initial Issue")
                        .expect("initial Issue inputs");
                state
                    .allocator
                    .ingest_workgraph(projector.as_ref(), initial, 1, "initial-open")
                    .await
                    .expect("persist initial Issue");

                let mut closed_issue = root_issue(&admission_labels);
                closed_issue["state"] = json!("closed");
                closed_issue["state_reason"] = json!("completed");
                closed_issue["updated_at"] = json!("2026-08-01T00:01:00Z");
                let close = payload("closed", closed_issue);
                let mut reopened_issue = root_issue(&admission_labels);
                reopened_issue["updated_at"] = json!("2026-08-01T00:01:00Z");
                let reopen = payload("reopened", reopened_issue.clone());
                let deliveries = if close_first {
                    [("equal-close", &close), ("equal-reopen", &reopen)]
                } else {
                    [("equal-reopen", &reopen), ("equal-close", &close)]
                };
                for (index, (delivery_id, delivery)) in deliveries.into_iter().enumerate() {
                    if let Some(inputs) = try_workgraph_issue(&state, delivery_id, delivery)
                        .await
                        .expect("normalize equal-revision Issue")
                    {
                        state
                            .allocator
                            .ingest_workgraph(
                                projector.as_ref(),
                                inputs,
                                index as u64 + 2,
                                &format!("{delivery_id}-{admitted_root}-{close_first}"),
                            )
                            .await
                            .expect("persist equal-revision Issue");
                    }
                }
                assert!(try_workgraph_issue(&state, "replayed-close", &close)
                    .await
                    .expect("normalize replayed close")
                    .is_none());
                if admitted_root {
                    assert!(state
                        .allocator
                        .latest_workgraph_root_issue("I_root")
                        .await
                        .expect("read Root Issue")
                        .is_some_and(|root| root.is_open));
                }
                let expected = issue_authority_state(
                    &reopened_issue,
                    reopen.get("repository"),
                    None,
                    &state.task_issue_type,
                    false,
                )
                .expect("canonical reopened state")
                .fingerprint()
                .expect("reopened fingerprint");
                assert_eq!(
                    state
                        .allocator
                        .latest_workgraph_issue_state_fingerprint("I_root")
                        .await
                        .expect("read Issue fingerprint")
                        .as_deref(),
                    Some(expected.as_str())
                );
            }
        }
    }

    #[tokio::test]
    async fn authoritative_marker_removal_wins_equal_revision_old_task_in_both_orders() {
        let task_body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";
        for removal_first in [true, false] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "data": {
                        "node": {
                            "id": "I_task",
                            "databaseId": 42,
                            "number": 7,
                            "title": "",
                            "body": "ordinary issue",
                            "state": "OPEN",
                            "stateReason": null,
                            "issueType": {"id": "IT_task", "name": "WorkGraphTask"},
                            "parent": null,
                            "repository": {
                                "id": "R_widgets",
                                "name": "widgets",
                                "owner": {"login": "acme"}
                            },
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
            let initial = try_workgraph_issue(
                &state,
                "initial-task",
                &payload("opened", task_issue("I_task", task_body)),
            )
            .await
            .expect("normalize initial task")
            .expect("initial task inputs");
            state
                .allocator
                .ingest_workgraph(projector.as_ref(), initial, 1, "initial-task")
                .await
                .expect("persist initial task");

            let mut removed_issue = task_issue("I_task", "ordinary issue");
            removed_issue["updated_at"] = json!("2026-08-01T00:01:00Z");
            let mut removal = payload("edited", removed_issue);
            removal["changes"] = json!({"body": {"from": task_body}});
            let mut old_task_issue = task_issue("I_task", task_body);
            old_task_issue["updated_at"] = json!("2026-08-01T00:01:00Z");
            let old_task = payload("edited", old_task_issue);
            let deliveries = if removal_first {
                [("equal-removal", &removal), ("equal-old-task", &old_task)]
            } else {
                [("equal-old-task", &old_task), ("equal-removal", &removal)]
            };
            for (index, (delivery_id, delivery)) in deliveries.into_iter().enumerate() {
                if let Some(inputs) = try_workgraph_issue(&state, delivery_id, delivery)
                    .await
                    .expect("normalize equal-revision marker transition")
                {
                    state
                        .allocator
                        .ingest_workgraph(
                            projector.as_ref(),
                            inputs,
                            index as u64 + 2,
                            &format!("{delivery_id}-{removal_first}"),
                        )
                        .await
                        .expect("persist marker transition");
                }
            }
            assert!(state
                .allocator
                .latest_workgraph_task("I_task")
                .await
                .expect("read task classification")
                .is_none());
            assert!(try_workgraph_issue(&state, "replayed-old-task", &old_task)
                .await
                .expect("normalize replayed old task")
                .is_none());
        }
    }

    #[tokio::test]
    async fn authoritative_tombstone_wins_equal_revision_generic_recreation_in_both_orders() {
        for deletion_first in [true, false] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(json!({"data": {"node": null}})),
                )
                .expect(2)
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
            let initial = try_workgraph_issue(
                &state,
                "initial-generic",
                &payload("opened", root_issue(&[])),
            )
            .await
            .expect("normalize initial generic Issue")
            .expect("initial generic inputs");
            state
                .allocator
                .ingest_workgraph(projector.as_ref(), initial, 1, "initial-generic")
                .await
                .expect("persist initial generic Issue");

            let mut deleted_issue = root_issue(&[]);
            deleted_issue["updated_at"] = json!("2026-08-01T00:01:00Z");
            let deletion = payload("deleted", deleted_issue);
            let mut stale_open_issue = root_issue(&[]);
            stale_open_issue["updated_at"] = json!("2026-08-01T00:01:00Z");
            let stale_open = payload("reopened", stale_open_issue);
            let deliveries = if deletion_first {
                [
                    ("equal-delete", &deletion),
                    ("equal-stale-open", &stale_open),
                ]
            } else {
                [
                    ("equal-stale-open", &stale_open),
                    ("equal-delete", &deletion),
                ]
            };
            for (index, (delivery_id, delivery)) in deliveries.into_iter().enumerate() {
                if let Some(inputs) = try_workgraph_issue(&state, delivery_id, delivery)
                    .await
                    .expect("normalize equal-revision tombstone transition")
                {
                    state
                        .allocator
                        .ingest_workgraph(
                            projector.as_ref(),
                            inputs,
                            index as u64 + 2,
                            &format!("{delivery_id}-{deletion_first}"),
                        )
                        .await
                        .expect("persist tombstone transition");
                }
            }
            assert!(
                try_workgraph_issue(&state, "replayed-stale-open", &stale_open)
                    .await
                    .expect("normalize replayed stale generic Issue")
                    .is_none()
            );
            let tombstone = IssueAuthorityState::Absent
                .fingerprint()
                .expect("tombstone fingerprint");
            assert_eq!(
                state
                    .allocator
                    .latest_workgraph_issue_state_fingerprint("I_root")
                    .await
                    .expect("read tombstone")
                    .as_deref(),
                Some(tombstone.as_str())
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
            try_workgraph_comment(&untrusted, &event).await,
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
            .await
            .expect("normalize")
            .expect("lifecycle artifact");
        assert!(matches!(
            &inputs[0],
            ProjectionInput::UpsertLifecycleArtifact(document)
                if document.source_key == "IC_assign"
                    && document.task_source_key == "I_task"
                    && document.body == body
                    && document.created_at_revision == 1_767_225_600_000
        ));
    }

    #[tokio::test]
    async fn route_artifact_uses_reporter_trust_and_preserves_task_source_key() {
        let body = "WorkGraphTaskRoute/v1\n\n```json\n{\"routeId\":\"route-1\"}\n```\n";
        let event = json!({
            "action": "created",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "issue": task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
            "comment": {
                "node_id": "IC_route",
                "body": body,
                "user": {"node_id": "U_report", "login": "reporter"},
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }
        });
        let trust = ProtocolTrust {
            task_creators: vec![TrustedIdentity {
                id: "U_creator".to_string(),
                login: "task-creator".to_string(),
            }],
            dispatchers: Vec::new(),
            reporters: vec![TrustedIdentity {
                id: "U_report".to_string(),
                login: "reporter".to_string(),
            }],
        };
        let (_temp, _projector, state) = ingress_state(Some(trust)).await;
        let inputs = try_workgraph_comment(&state, &event)
            .await
            .expect("normalize Route")
            .expect("Route artifact");
        assert!(matches!(
            &inputs[0],
            ProjectionInput::UpsertLifecycleArtifact(document)
                if document.source_key == "IC_route"
                    && document.task_source_key == "I_task"
                    && document.body == body
        ));
    }

    #[tokio::test]
    async fn human_root_comment_is_durable_revision_fenced_and_retractable() {
        let (_temp, projector, state) = ingress_state(None).await;
        seed_root_issue(&state, projector.as_ref()).await;
        let comment_event = |action, body, updated_at| {
            human_root_comment_event("IC_human", action, body, updated_at)
        };

        let created = comment_event("created", "resume with option B", "2026-08-01T00:01:00Z");
        let body = serde_json::to_vec(&created).expect("encode created");
        let headers = signed_headers("issue_comment", "human-created", &body);
        assert!(matches!(
            handle_delivery(&state, &headers, &body).await,
            Ok(Some(_))
        ));
        let committed = projector.committed.lock().await;
        let document = committed
            .last()
            .expect("comment commit")
            .iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertRootIssueComment(document) => Some(document),
                _ => None,
            })
            .expect("normalized Root Issue comment");
        assert_eq!(document.source_key, "IC_human");
        assert_eq!(document.root_issue_id, "I_root");
        assert_eq!(document.admission_id.len(), 68);
        assert_eq!(document.repository_owner, "acme");
        assert_eq!(document.repository_name, "widgets");
        assert_eq!(document.repository_node_id, "R_widgets");
        assert_eq!(document.issue_number, 6);
        assert_eq!(document.author_id, "U_human");
        assert_eq!(document.author_type, "User");
        assert_eq!(document.author_login, "octocat");
        assert_eq!(document.body, "resume with option B");
        assert_eq!(document.created_at_revision, 1_785_542_460_000);
        assert_eq!(document.updated_at_revision, 1_785_542_460_000);
        drop(committed);

        let edited = comment_event("edited", "resume with option C", "2026-08-01T00:02:00Z");
        let body = serde_json::to_vec(&edited).expect("encode edit");
        let headers = signed_headers("issue_comment", "human-edited", &body);
        assert!(matches!(
            handle_delivery(&state, &headers, &body).await,
            Ok(Some(_))
        ));
        let committed = projector.committed.lock().await;
        assert!(matches!(
            committed.last().expect("edit commit").as_slice(),
            [ProjectionInput::UpsertRootIssueComment(document)]
                if document.body == "resume with option C"
                    && document.updated_at_revision == 1_785_542_520_000
        ));
        drop(committed);

        let deleted = comment_event("deleted", "resume with option B", "2026-08-01T00:03:00Z");
        let body = serde_json::to_vec(&deleted).expect("encode deletion");
        let headers = signed_headers("issue_comment", "human-deleted", &body);
        assert!(matches!(
            handle_delivery(&state, &headers, &body).await,
            Ok(Some(_))
        ));
        let committed = projector.committed.lock().await;
        assert!(matches!(
            committed.last().expect("deletion commit").as_slice(),
            [ProjectionInput::DeleteRootIssueComment {
                source_key,
                root_issue_id,
                admission_id: _,
                updated_at_revision: 1_785_542_580_000,
                ..
            }] if source_key == "IC_human" && root_issue_id == "I_root"
        ));
        let commit_count = committed.len();
        drop(committed);
        let revision = state
            .allocator
            .latest_workgraph_root_comment_revision("IC_human")
            .await
            .expect("read durable comment tombstone")
            .expect("comment revision");
        assert!(revision.tombstone);
        assert!(revision.document.is_none());
        assert_eq!(revision.revision, 1_785_542_580_000);

        let delayed = comment_event("edited", "equal-revision edit", "2026-08-01T00:03:00Z");
        let body = serde_json::to_vec(&delayed).expect("encode delayed edit");
        let headers = signed_headers("issue_comment", "human-delayed", &body);
        assert!(matches!(
            handle_delivery(&state, &headers, &body).await,
            Ok(None)
        ));
        assert_eq!(projector.committed.lock().await.len(), commit_count);
    }

    #[tokio::test]
    async fn bot_root_comment_is_not_projected_as_wait_resume_evidence() {
        let (_temp, projector, state) = ingress_state(None).await;
        seed_root_issue(&state, projector.as_ref()).await;
        let event = json!({
            "action": "created",
            "organization": {"login": "acme"},
            "repository": {
                "name": "widgets",
                "full_name": "acme/widgets",
                "node_id": "R_widgets"
            },
            "issue": root_issue(&["workgraph"]),
            "comment": {
                "node_id": "IC_bot",
                "body": "agent output",
                "user": {
                    "node_id": "BOT_agent",
                    "type": "Bot",
                    "login": "copilot-swe-agent[bot]"
                },
                "created_at": "2026-08-01T00:01:00Z",
                "updated_at": "2026-08-01T00:01:00Z"
            }
        });
        let body = serde_json::to_vec(&event).expect("encode bot event");
        let headers = signed_headers("issue_comment", "bot-comment", &body);
        assert!(matches!(
            handle_delivery(&state, &headers, &body).await,
            Ok(None)
        ));
        assert_eq!(projector.committed.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn human_comment_changed_to_lifecycle_marker_keeps_deletion_tombstone() {
        let (_temp, projector, state) = ingress_state(None).await;
        seed_root_issue(&state, projector.as_ref()).await;
        let created = human_root_comment_event(
            "IC_transition",
            "created",
            "human evidence",
            "2026-08-01T00:01:00Z",
        );
        let body = serde_json::to_vec(&created).expect("encode creation");
        handle_delivery(
            &state,
            &signed_headers("issue_comment", "transition-created", &body),
            &body,
        )
        .await
        .expect("create human evidence");

        let marker = human_root_comment_event(
            "IC_transition",
            "edited",
            "WorkGraphTaskRoute/v1\n\n```json\n{}\n```",
            "2026-08-01T00:02:00Z",
        );
        let body = serde_json::to_vec(&marker).expect("encode marker edit");
        handle_delivery(
            &state,
            &signed_headers("issue_comment", "transition-marker", &body),
            &body,
        )
        .await
        .expect("retract human evidence");
        assert!(state
            .allocator
            .latest_workgraph_root_comment_revision("IC_transition")
            .await
            .expect("read tombstone")
            .is_some_and(|revision| revision.tombstone));

        let mut deleted = marker;
        deleted["action"] = json!("deleted");
        deleted["comment"]["updated_at"] = json!("2026-08-01T00:03:00Z");
        let body = serde_json::to_vec(&deleted).expect("encode newer deletion");
        handle_delivery(
            &state,
            &signed_headers("issue_comment", "transition-deleted", &body),
            &body,
        )
        .await
        .expect("advance existing tombstone");
        let revision = state
            .allocator
            .latest_workgraph_root_comment_revision("IC_transition")
            .await
            .expect("read advanced tombstone")
            .expect("tombstone");
        assert!(revision.tombstone);
        assert_eq!(revision.identity.repository_node_id, "R_widgets");
        assert_eq!(revision.revision, 1_785_542_580_000);

        let later = human_root_comment_event(
            "IC_transition",
            "edited",
            "must not return",
            "2026-08-01T00:04:00Z",
        );
        let body = serde_json::to_vec(&later).expect("encode later edit");
        assert!(matches!(
            handle_delivery(
                &state,
                &signed_headers("issue_comment", "transition-later", &body),
                &body,
            )
            .await,
            Ok(None)
        ));
    }

    #[tokio::test]
    async fn bot_edit_retracts_prior_human_root_comment_evidence() {
        let (_temp, projector, state) = ingress_state(None).await;
        seed_root_issue(&state, projector.as_ref()).await;
        let created =
            human_root_comment_event("IC_bot_edit", "created", "human", "2026-08-01T00:01:00Z");
        let body = serde_json::to_vec(&created).expect("encode creation");
        handle_delivery(
            &state,
            &signed_headers("issue_comment", "bot-edit-created", &body),
            &body,
        )
        .await
        .expect("create evidence");

        let mut edited = human_root_comment_event(
            "IC_bot_edit",
            "edited",
            "agent edit",
            "2026-08-01T00:02:00Z",
        );
        edited["sender"] = json!({
            "node_id": "BOT_agent",
            "type": "Bot",
            "login": "copilot-swe-agent[bot]"
        });
        let body = serde_json::to_vec(&edited).expect("encode bot edit");
        handle_delivery(
            &state,
            &signed_headers("issue_comment", "bot-edit", &body),
            &body,
        )
        .await
        .expect("retract bot-edited evidence");
        assert!(state
            .allocator
            .latest_workgraph_root_comment_revision("IC_bot_edit")
            .await
            .expect("read revision")
            .is_some_and(|revision| revision.tombstone && revision.document.is_none()));
    }

    #[tokio::test]
    async fn equal_revision_different_human_comment_content_fails_closed_in_both_orders() {
        for (first, second) in [("alpha", "omega"), ("omega", "alpha")] {
            let (_temp, projector, state) = ingress_state(None).await;
            seed_root_issue(&state, projector.as_ref()).await;
            let first_event =
                human_root_comment_event("IC_equal", "created", first, "2026-08-01T00:01:00Z");
            let body = serde_json::to_vec(&first_event).expect("encode first");
            handle_delivery(
                &state,
                &signed_headers("issue_comment", "equal-first", &body),
                &body,
            )
            .await
            .expect("accept first content");

            let second_event =
                human_root_comment_event("IC_equal", "edited", second, "2026-08-01T00:01:00Z");
            let body = serde_json::to_vec(&second_event).expect("encode conflicting edit");
            assert!(matches!(
                handle_delivery(
                    &state,
                    &signed_headers("issue_comment", "equal-second", &body),
                    &body,
                )
                .await,
                Err((StatusCode::SERVICE_UNAVAILABLE, _))
            ));
            let revision = state
                .allocator
                .latest_workgraph_root_comment_revision("IC_equal")
                .await
                .expect("read evidence")
                .expect("evidence");
            assert_eq!(revision.document.expect("active evidence").body, first);
        }
    }

    #[tokio::test]
    async fn delayed_comment_snapshot_cannot_bind_to_readmitted_root_generation() {
        let (_temp, projector, state) = ingress_state(None).await;
        seed_root_issue(&state, projector.as_ref()).await;
        let old_comment = human_root_comment_event(
            "IC_old_generation",
            "created",
            "resume",
            "2026-08-01T00:04:00Z",
        );

        let mut removed_issue = root_issue(&[]);
        removed_issue["updated_at"] = json!("2026-08-01T00:02:00Z");
        let mut removed = payload("unlabeled", removed_issue);
        removed["label"] = json!({"name": "workgraph"});
        let inputs = try_workgraph_issue(&state, "root-removed", &removed)
            .await
            .expect("normalize removal")
            .expect("removal inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), inputs, 2, "root-removed")
            .await
            .expect("persist removal");

        let mut readmitted_issue = root_issue(&["workgraph"]);
        readmitted_issue["updated_at"] = json!("2026-08-01T00:03:00Z");
        let mut readmitted = payload("labeled", readmitted_issue);
        readmitted["label"] = json!({"name": "workgraph"});
        let inputs = try_workgraph_issue(&state, "root-readmitted", &readmitted)
            .await
            .expect("normalize readmission")
            .expect("readmission inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), inputs, 3, "root-readmitted")
            .await
            .expect("persist readmission");
        let admission_id = state
            .allocator
            .latest_workgraph_root_issue("I_root")
            .await
            .expect("read root")
            .expect("readmitted root")
            .admission_id;

        let body = serde_json::to_vec(&old_comment).expect("encode delayed comment");
        assert!(matches!(
            handle_delivery(
                &state,
                &signed_headers("issue_comment", "old-generation-comment", &body),
                &body,
            )
            .await,
            Ok(None)
        ));

        let mut current = human_root_comment_event(
            "IC_new_generation",
            "created",
            "resume",
            "2026-08-01T00:04:00Z",
        );
        current["issue"]["updated_at"] = json!("2026-08-01T00:03:00Z");
        let body = serde_json::to_vec(&current).expect("encode current comment");
        handle_delivery(
            &state,
            &signed_headers("issue_comment", "new-generation-comment", &body),
            &body,
        )
        .await
        .expect("accept current generation comment");
        let revision = state
            .allocator
            .latest_workgraph_root_comment_revision("IC_new_generation")
            .await
            .expect("read comment")
            .expect("current comment");
        assert_eq!(
            revision.document.expect("active comment").admission_id,
            admission_id
        );
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
            .await
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
                        "id": "I_root",
                        "databaseId": 41,
                        "number": 6,
                        "title": "Root Issue",
                        "body": "Coordinate this work.",
                        "state": "OPEN",
                        "stateReason": null,
                        "issueType": null,
                        "parent": null,
                        "repository": {
                            "id": "R_widgets",
                            "name": "widgets",
                            "owner": {"login": "acme"}
                        },
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
                        "id": "I_root",
                        "databaseId": 42,
                        "number": 7,
                        "title": "",
                        "body": "WorkGraphTask/v1\n\n```json\n{}\n```\n",
                        "state": "OPEN",
                        "stateReason": null,
                        "issueType": {"id": "IT_task", "name": "WorkGraphTask"},
                        "parent": null,
                        "repository": {
                            "id": "R_widgets",
                            "name": "widgets",
                            "owner": {"login": "acme"}
                        },
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
                        state_fingerprint: "0".repeat(64),
                        authorization_transition: false,
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
    async fn cross_repository_sub_issue_fingerprint_uses_child_repository() {
        let (_temp, _projector, mut state) = ingress_state(Some(task_trust())).await;
        state.repository_filter =
            RepositoryFilter::new("acme", &["widgets".to_string(), "child".to_string()])
                .expect("repository filter");
        let child_repository = json!({
            "name": "child",
            "full_name": "acme/child",
            "node_id": "R_child"
        });
        let event = json!({
            "action": "sub_issue_added",
            "organization": {"login": "acme"},
            "repository": {
                "name": "widgets",
                "full_name": "acme/widgets",
                "node_id": "R_widgets"
            },
            "sub_issue_repo": child_repository,
            "sender": {"node_id": "U_creator", "login": "task-creator"},
            "parent_issue": {"node_id": "I_root"},
            "sub_issue": task_issue(
                "I_child",
                "WorkGraphTask/v1\n\n```json\n{}\n```\n"
            )
        });
        let inputs = try_workgraph_sub_issue(&state, &event)
            .await
            .expect("normalize cross-repository hierarchy")
            .expect("cross-repository task inputs");
        let fingerprint = inputs
            .iter()
            .find_map(|input| match input {
                ProjectionInput::RecordIssueRevision {
                    state_fingerprint, ..
                } => Some(state_fingerprint.as_str()),
                _ => None,
            })
            .expect("state fingerprint");
        let child = event.get("sub_issue").expect("child Issue");
        let expected = issue_authority_state(
            child,
            event.get("sub_issue_repo"),
            Some("I_root".to_string()),
            &state.task_issue_type,
            false,
        )
        .expect("child repository state")
        .fingerprint()
        .expect("child repository fingerprint");
        let parent = issue_authority_state(
            child,
            event.get("repository"),
            Some("I_root".to_string()),
            &state.task_issue_type,
            false,
        )
        .expect("parent repository state")
        .fingerprint()
        .expect("parent repository fingerprint");
        assert_eq!(fingerprint, expected);
        assert_ne!(fingerprint, parent);
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
