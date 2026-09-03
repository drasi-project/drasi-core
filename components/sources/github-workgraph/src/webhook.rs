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
use crate::config::{
    AdmissionReadCredential, ProtocolTrust, RepositoryFilter, TaskIssueType, WorkflowMappingSet,
};
use crate::lease_ledger::{
    root_comment_fingerprint, task_response_fingerprint, Allocator, LifecycleArtifactRevisionState,
    RootIssueCommentRevisionState, TaskResponseRevisionState, TaskResponseSubject,
    WorkGraphActiveLease,
};
use crate::protocol::{
    derive_workgraph_id, LifecycleArtifactDocument, RootMappingAdmission, WorkGraphProjector,
    LEGACY_WORKFLOW_MAPPING_ID, MAX_WORKGRAPH_ATTEMPTS, WORKGRAPH_ADMISSION_LABEL,
    WORKGRAPH_ERROR_LABEL, WORKGRAPH_IGNORE_LABEL,
};
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
    /// Read-only GitHub credential used to resolve authoritative Issue-label
    /// state during ambiguous webhook ordering. It is resolved from
    /// configuration and is independent of how Root admission is configured, so
    /// a mapping-only Source is exactly as authoritative as a legacy one. No
    /// configured definition file is ever fetched; the Reaction owns the pinned
    /// workflow definition.
    pub admission_read: Option<AdmissionReadCredential>,
    /// The complete ordered set of label→workflow mappings this Source
    /// recognizes, including the implicit legacy `workgraph` mapping.
    pub workflow_mappings: WorkflowMappingSet,
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
    admission_client: Option<AdmissionClient>,
    workflow_mappings: WorkflowMappingSet,
    projection_gate: Mutex<()>,
    notify: Arc<Notify>,
}

pub async fn serve(listener: TcpListener, params: IngressParams) -> Result<()> {
    let admission_client = params
        .admission_read
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
        admission_client,
        workflow_mappings: params.workflow_mappings,
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

impl LeaseValidationResponse {
    fn from_active(active: WorkGraphActiveLease, claim_id: String) -> Self {
        debug_assert!((1..=MAX_WORKGRAPH_ATTEMPTS).contains(&active.attempt));
        Self {
            lease_id: active.lease_id,
            task_id: active.task_id,
            assignment_id: active.assignment_id,
            attempt: active.attempt,
            executor_id: active.executor_id,
            slot_id: active.slot_id,
            claim_id,
            acquired_at: active.acquired_at,
            expires_at: active.expires_at,
        }
    }
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
            Json(LeaseValidationResponse::from_active(
                active,
                request.claim_id,
            )),
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

    if touches_agent.is_none() {
        debug!("[{source_id}] push delivery {delivery_id} does not touch the agent file");
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

    // Single delivery dedup for the agent convergence.
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

    // Single delivery marker for the convergence.
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
        /// Assignees sorted by numeric ID and deduplicated, so an assignment
        /// change is a real state transition the revision fence can see.
        assignees: Vec<crate::protocol::TaskAssignee>,
    },
}

impl IssueAuthorityState {
    fn fingerprint(&self) -> Result<String> {
        let encoded = serde_json::to_vec(self).context("failed to encode canonical Issue state")?;
        Ok(hex::encode(Sha256::digest(encoded)))
    }
}

impl AdmissionClient {
    fn new(credential: impl Into<AdmissionReadCredential>) -> Result<Self> {
        let credential = credential.into();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("bearer {}", credential.token))
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
            api_url: credential.api_base_url,
        })
    }

    async fn is_root_candidate(
        &self,
        mappings: &WorkflowMappingSet,
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
            .filter_map(|label| label.get("name").and_then(Value::as_str))
            .any(|label| mappings.recognizes_label(label));
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
        mappings: &WorkflowMappingSet,
        node_id: &str,
        task_issue_type: &crate::config::TaskIssueType,
    ) -> Result<IssueAuthorityState> {
        let response = self
            .http
            .post(&self.api_url)
            .json(&json!({
                "query": "query($id: ID!) { node(id: $id) { ... on Issue { id databaseId number title body state stateReason issueType { id name } parent { id } repository { id name owner { login } } labels(first: 100) { nodes { name } pageInfo { hasNextPage } } assignees(first: 100) { nodes { databaseId id login } pageInfo { hasNextPage } } } } }",
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
        let assignee_page = issue
            .get("assignees")
            .and_then(Value::as_object)
            .ok_or_else(|| anyhow!("authoritative task state lookup omitted assignees"))?;
        if assignee_page
            .get("pageInfo")
            .and_then(Value::as_object)
            .and_then(|page_info| page_info.get("hasNextPage"))
            .and_then(Value::as_bool)
            == Some(true)
        {
            anyhow::bail!("authoritative task assignee set exceeds 100 entries");
        }
        let assignees = graphql_assignees(&Value::Object(issue.clone()));
        if assignees.len()
            != assignee_page
                .get("nodes")
                .and_then(Value::as_array)
                .context("authoritative task state assignees are missing")?
                .len()
        {
            anyhow::bail!("authoritative task state contains an invalid assignee");
        }
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
            .any(|label| mappings.recognizes_label(label));
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
            assignees,
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

/// The Issue's current assignees, sorted by numeric ID and deduplicated.
///
/// GitHub carries the authoritative set in `issue.assignees` on every `issues`
/// delivery, not only the assignment ones, so the set is read the same way
/// whatever action produced the payload. An entry missing any of the three
/// identity fields is dropped rather than half-recorded.
fn issue_assignees(issue: &serde_json::Value) -> Vec<crate::protocol::TaskAssignee> {
    let mut assignees = issue
        .get("assignees")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|assignee| {
            Some(crate::protocol::TaskAssignee {
                database_id: assignee.get("id").and_then(serde_json::Value::as_u64)?,
                node_id: assignee
                    .get("node_id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())?
                    .to_string(),
                login: assignee
                    .get("login")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())?
                    .to_string(),
            })
        })
        .collect::<Vec<_>>();
    assignees.sort();
    assignees.dedup();
    assignees
}

/// The same set read from the authoritative GraphQL assignee connection.
fn graphql_assignees(issue: &serde_json::Value) -> Vec<crate::protocol::TaskAssignee> {
    let mut assignees = issue
        .pointer("/assignees/nodes")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|assignee| {
            Some(crate::protocol::TaskAssignee {
                database_id: assignee
                    .get("databaseId")
                    .and_then(serde_json::Value::as_u64)?,
                node_id: assignee
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())?
                    .to_string(),
                login: assignee
                    .get("login")
                    .and_then(serde_json::Value::as_str)
                    .filter(|value| !value.is_empty())?
                    .to_string(),
            })
        })
        .collect::<Vec<_>>();
    assignees.sort();
    assignees.dedup();
    assignees
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
        assignees: issue_assignees(issue),
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

/// An ordinary Issue is a Root candidate when it is neither a WorkGraphTask nor
/// task-typed and it carries at least one exact configured selector label.
///
/// The reserved `workgraph:ignore` / `workgraph:error` modifiers never activate
/// a mapping, and an unknown `workgraph:*` label is observed but starts nothing.
fn issue_is_root_candidate(
    issue: &serde_json::Value,
    task_issue_type: &crate::config::TaskIssueType,
    mappings: &WorkflowMappingSet,
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
                labels
                    .iter()
                    .filter_map(|label| label.get("name").and_then(serde_json::Value::as_str))
                    .any(|label| mappings.recognizes_label(label))
            })
}

/// Every exact label on an Issue, in payload order.
fn issue_label_names(issue: &serde_json::Value) -> Vec<&str> {
    issue
        .get("labels")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|label| label.get("name").and_then(serde_json::Value::as_str))
        .collect()
}

fn issue_authority_state(
    issue: &serde_json::Value,
    repository: Option<&serde_json::Value>,
    parent_source_key: Option<String>,
    task_issue_type: &crate::config::TaskIssueType,
    mappings: &WorkflowMappingSet,
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
    } else if issue_is_root_candidate(issue, task_issue_type, mappings) {
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
        assignees: issue_assignees(issue),
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

/// Derives one mapping activation's admission generation ID.
///
/// The exact GitHub delivery that observed the activation supplies the
/// generation entropy, and the mapping ID plus its exact selector label are
/// framed inputs, so an Issue opened with several selector labels in one
/// delivery still derives a distinct ID per mapping.
fn admission_generation_id(
    root_issue_id: &str,
    delivery_id: &str,
    mapping_id: &str,
    label: &str,
) -> String {
    derive_workgraph_id(
        "admission",
        &[root_issue_id, delivery_id, mapping_id, label],
    )
}

/// Resolves the ordered set of currently active mapping admissions.
///
/// A mapping already present in `previous` keeps its admission generation:
/// adding another selector label must never regenerate an unrelated mapping.
/// A mapping absent from `previous` starts a fresh generation, so removing and
/// re-adding a selector label can never resume a retracted generation.
///
/// `relabeled` names the selector label of an explicit `labeled` delivery whose
/// revision strictly advanced. A `labeled` event for a label the cached document
/// already records can only mean an unobserved remove/re-add round trip, so that
/// one mapping regenerates while every other active mapping is preserved.
fn active_mapping_admissions(
    mappings: &WorkflowMappingSet,
    labels: &[&str],
    previous: Option<&crate::protocol::RootIssueDocument>,
    relabeled: Option<&str>,
    root_issue_id: &str,
    delivery_id: &str,
    title: &str,
    body: &str,
) -> Vec<RootMappingAdmission> {
    mappings
        .active_for_labels(labels.iter().copied())
        .into_iter()
        .map(|mapping| {
            let previous_activation = previous
                .filter(|_| relabeled != Some(mapping.label.as_str()))
                .and_then(|previous| previous.mapping_admission(&mapping.id))
                .filter(|active| active.label == mapping.label);
            let (admission_id, title, body) = previous_activation.map_or_else(
                || {
                    (
                        admission_generation_id(
                            root_issue_id,
                            delivery_id,
                            &mapping.id,
                            &mapping.label,
                        ),
                        title.to_string(),
                        body.to_string(),
                    )
                },
                |active| {
                    (
                        active.admission_id.clone(),
                        active.title.clone(),
                        active.body.clone(),
                    )
                },
            );
            RootMappingAdmission {
                mapping_id: mapping.id.clone(),
                label: mapping.label.clone(),
                admission_id,
                title,
                body,
                definition_repository: mapping.definition_repository.clone(),
                definition_ref: mapping.definition_ref.clone(),
                definition_path: mapping.definition_path.clone(),
            }
        })
        .collect()
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
    let labeled_root_candidate =
        issue_is_root_candidate(issue, &state.task_issue_type, &state.workflow_mappings);
    let relabeled_selector = (action == "labeled")
        .then(|| {
            payload
                .pointer("/label/name")
                .and_then(serde_json::Value::as_str)
        })
        .flatten()
        .filter(|label| state.workflow_mappings.recognizes_label(label));
    authorize_workgraph_repository(state, payload, None)?;
    let previous_task = state
        .allocator
        .latest_workgraph_task(node_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let typed = state.task_issue_type.matches(issue.get("type"));
    let previous_parent_source_key = previous_task
        .as_ref()
        .and_then(|task| task.parent_source_key.clone());
    let task_parent_source_key = if current_workgraph && typed && action == "opened" {
        match previous_parent_source_key {
            Some(parent) => Some(parent),
            None => match &state.admission_client {
                Some(client) => client
                    .parent_issue_node_id(node_id)
                    .await
                    .map_err(|error| WorkGraphNormError::Unavailable(error.to_string()))?,
                None => None,
            },
        }
    } else {
        previous_parent_source_key
    };
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
        task_parent_source_key.clone(),
        &state.task_issue_type,
        &state.workflow_mappings,
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
                    .issue_authority_state(
                        &state.workflow_mappings,
                        node_id,
                        &state.task_issue_type,
                    )
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
        // A mapping the previous document already admitted keeps its
        // generation; every newly observed selector label starts its own. An
        // explicit `labeled` delivery that advanced the revision regenerates
        // only the mapping it names.
        let relabeled_mapping = relabeled_selector.filter(|_| {
            revision.is_some_and(|revision| {
                previous_revision.is_none_or(|previous| revision > previous)
            })
        });
        let current_title = issue
            .get("title")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let workflow_mappings = active_mapping_admissions(
            &state.workflow_mappings,
            &issue_label_names(issue),
            previous_root.as_ref(),
            relabeled_mapping,
            node_id,
            delivery_id,
            current_title,
            body,
        );
        if workflow_mappings.is_empty() {
            return Err(WorkGraphNormError::InvalidPayload(
                "admitted Root Issue carries no configured workflow mapping label".to_string(),
            ));
        }
        let admission_id = RootIssueDocument::legacy_admission_id(&workflow_mappings)
            .expect("a non-empty mapping set selects a legacy admission ID")
            .to_string();
        // Each activation owns immutable Root content. The document-level
        // compatibility fields follow the mapping selected by `admissionId`;
        // adding or re-adding a sibling mapping cannot mutate an existing run.
        let compatibility_mapping = workflow_mappings
            .iter()
            .find(|mapping| mapping.admission_id == admission_id)
            .expect("the compatibility admission selects an active mapping");
        inputs.push(ProjectionInput::UpsertRootIssue(RootIssueDocument {
            source_key: node_id.to_string(),
            repository_owner: locator.repository_owner,
            repository_name: locator.repository_name,
            repository_node_id: repository_node_id.to_string(),
            issue_number: locator.issue_number,
            title: compatibility_mapping.title.clone(),
            body: compatibility_mapping.body.clone(),
            is_open: item_is_open(issue),
            admission_id,
            workflow_mappings,
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
        // Granting authority is restricted: an assignment is authored by the
        // service actor that runs the workflow, never by the assignee, so
        // being assigned never grants the authority to change the Issue.
        //
        // Revoking it is not. An `unassigned` delivery is signed GitHub state,
        // and refusing it because the sender is untrusted would leave Core
        // believing a human is still assigned after GitHub says they are not,
        // keeping stale response authority alive. Such a delivery is accepted
        // for its removals only: the recorded assignee set is intersected with
        // what Core already knew, so an untrusted sender can shrink it and
        // never grow it, and nothing else on the Issue moves.
        let assignment_action = matches!(action, "assigned" | "unassigned");
        let actor_is_trusted = protocol_trust.is_task_creator(editor)
            || assignment_action && protocol_trust.is_assigner(editor);
        if !protocol_trust.is_task_creator(creator) {
            return Err(WorkGraphNormError::Untrusted(format!(
                "WorkGraph task {node_id}: creator is not a trusted task creator"
            )));
        }
        if !actor_is_trusted {
            let Some(previous) = previous_task.as_ref().filter(|_| action == "unassigned") else {
                return Err(WorkGraphNormError::Untrusted(format!(
                    "WorkGraph task {node_id}: webhook actor is not a trusted task creator"
                )));
            };
            let observed = issue_assignees(issue);
            let retained = previous
                .assignees
                .iter()
                .filter(|assignee| {
                    observed
                        .iter()
                        .any(|current| current.database_id == assignee.database_id)
                })
                .cloned()
                .collect::<Vec<_>>();
            if retained.len() == previous.assignees.len() {
                return Ok(None);
            }
            // The revision watermark deliberately does not advance here.
            //
            // Only the assignee removal is applied; every other field stays
            // the cached one, so the incoming payload's state fingerprint
            // describes a state Core did not adopt. Recording it would also
            // move the watermark to this delivery's `updated_at`, which is
            // attacker-chosen: a tampered unassignment could then fence out a
            // trusted change that GitHub emitted earlier but delivered later.
            // Leaving the watermark alone keeps that delayed trusted delivery
            // strictly newer, and replaying this removal is idempotent because
            // the intersection above becomes a no-op.
            inputs.retain(|input| !matches!(input, ProjectionInput::RecordIssueRevision { .. }));
            let mut task_doc = previous.clone();
            task_doc.assignees = retained;
            inputs.push(ProjectionInput::UpsertTask(task_doc));
            return Ok(Some(inputs));
        }

        inputs.push(ProjectionInput::DeleteGitHubIssue {
            source_key: node_id.to_string(),
        });
        let task_doc = task_document(issue, node_id, task_parent_source_key);
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
    let previous_trust_role = payload
        .pointer("/changes/body/from")
        .and_then(serde_json::Value::as_str)
        .and_then(lifecycle_trust_role);
    let previous_workgraph = previous_trust_role.is_some();
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
    let prior_lifecycle_artifact = state
        .allocator
        .latest_workgraph_lifecycle_artifact_revision(comment_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    let updated_at_revision = comment_updated_revision(comment)?;
    let current_trust_role = lifecycle_trust_role(body);
    let mut inputs = Vec::new();

    if action == "deleted" {
        authorize_workgraph_repository(state, payload, None)?;
        if (previous_workgraph || current_trust_role.is_some())
            && (prior_lifecycle_artifact.is_some()
                || current_trust_role
                    .or(previous_trust_role)
                    .is_some_and(|role| lifecycle_author_is_trusted(state, comment, role)))
            && prior_lifecycle_artifact.as_ref().is_none_or(|previous| {
                should_accept_lifecycle_tombstone(previous, updated_at_revision)
            })
        {
            inputs.push(ProjectionInput::DeleteLifecycleArtifact {
                source_key: comment_id.to_string(),
                updated_at_revision,
            });
        }
        if let Some(previous) = prior_root_comment
            .filter(|previous| should_accept_root_comment_tombstone(previous, updated_at_revision))
        {
            inputs.push(root_comment_deletion(&previous, updated_at_revision));
        }
        // A deleted comment may also be a natural task response. Its tombstone
        // is emitted here rather than in the response path, which this early
        // return precedes, and is independent of the lifecycle and Root
        // tombstones above: one deleted comment is only ever one of the three.
        if let Some(previous) = state
            .allocator
            .latest_workgraph_task_response_revision(comment_id)
            .await
            .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?
            .filter(|previous| !previous.tombstone && previous.revision <= updated_at_revision)
        {
            inputs.push(ProjectionInput::DeleteTaskResponse {
                source_key: comment_id.to_string(),
                task_source_key: previous.identity.task_source_key.clone(),
                task_id: previous.identity.task_id.clone(),
                actor_id: previous.identity.actor_id.clone(),
                updated_at_revision,
            });
        }
        return Ok((!inputs.is_empty()).then_some(inputs));
    }

    if action == "edited"
        && current_trust_role.is_none()
        && previous_workgraph
        && (prior_lifecycle_artifact.is_some()
            || previous_trust_role
                .is_some_and(|role| lifecycle_author_is_trusted(state, comment, role)))
        && prior_lifecycle_artifact
            .as_ref()
            .is_none_or(|previous| should_accept_lifecycle_tombstone(previous, updated_at_revision))
    {
        authorize_workgraph_repository(state, payload, None)?;
        inputs.push(ProjectionInput::DeleteLifecycleArtifact {
            source_key: comment_id.to_string(),
            updated_at_revision,
        });
    }

    if let Some(trust_role) = current_trust_role {
        authorize_workgraph_repository(state, payload, None)?;
        if let Some(previous) = prior_root_comment
            .filter(|previous| should_accept_root_comment_tombstone(previous, updated_at_revision))
        {
            inputs.push(root_comment_deletion(&previous, updated_at_revision));
        }
        let normalization = normalize_lifecycle_artifact(
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
        );
        match normalization {
            Ok(()) => {}
            Err(WorkGraphNormError::Untrusted(message))
                if action == "edited" && previous_workgraph =>
            {
                if (prior_lifecycle_artifact.is_some()
                    || previous_trust_role
                        .is_some_and(|role| lifecycle_author_is_trusted(state, comment, role)))
                    && prior_lifecycle_artifact.as_ref().is_none_or(|previous| {
                        should_accept_lifecycle_tombstone(previous, updated_at_revision)
                    })
                {
                    warn!(
                        "[{}] retracted lifecycle artifact {comment_id} after an untrusted \
                         lifecycle-to-lifecycle edit: {message}",
                        state.source_id
                    );
                    inputs.push(ProjectionInput::DeleteLifecycleArtifact {
                        source_key: comment_id.to_string(),
                        updated_at_revision,
                    });
                }
                return Ok((!inputs.is_empty()).then_some(inputs));
            }
            Err(error) => return Err(error),
        }
        if let Some(position) = inputs
            .iter()
            .position(|input| matches!(input, ProjectionInput::UpsertLifecycleArtifact(_)))
        {
            let ProjectionInput::UpsertLifecycleArtifact(document) = &inputs[position] else {
                unreachable!()
            };
            if let Some(previous) = &prior_lifecycle_artifact {
                if !should_accept_lifecycle_upsert(previous, document)? {
                    inputs.remove(position);
                }
            }
        }
        return Ok((!inputs.is_empty()).then_some(inputs));
    }

    // A natural task response is authenticated between lifecycle artifacts and
    // Root Issue comments: it is neither a signed artifact nor a Root reply.
    if let Some(task) = state
        .allocator
        .latest_workgraph_task(issue_node_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?
    {
        return try_workgraph_task_response(
            state,
            payload,
            comment,
            comment_id,
            body,
            action,
            issue_node_id,
            &task,
            updated_at_revision,
            inputs,
        )
        .await;
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
    // Every mapping admission active on the Root when the comment was
    // observed, not just the ordering-dependent compatibility selection. The
    // guard above proved the payload's Issue state matches this exact cached
    // Root document, so the set is authoritative for this comment.
    let admission_ids = root.active_admission_ids();
    let document = RootIssueCommentDocument {
        source_key: comment_id.to_string(),
        root_issue_id: root.source_key,
        admission_id: root.admission_id,
        admission_ids,
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

/// Authenticate and bind one natural response a catalog human wrote on a
/// WorkGraph task Issue.
///
/// A response is admitted only when every one of these holds:
///
/// * the first non-whitespace line opens with the exact `@workgraph` mention;
/// * the author is a non-bot `User` whose exact GitHub triple is a declared
///   `version: 2` human actor;
/// * that same account is currently assigned to this task Issue;
/// * the comment body is non-empty and bounded.
///
/// Core never interprets the body. Deciding what the human meant, and whether
/// it answers anything, is the Reaction's call against the pinned definition.
#[allow(clippy::too_many_arguments)]
async fn try_workgraph_task_response(
    state: &IngressState,
    payload: &serde_json::Value,
    comment: &serde_json::Value,
    comment_id: &str,
    body: &str,
    action: &str,
    issue_node_id: &str,
    task: &crate::protocol::TaskDocument,
    updated_at_revision: i64,
    mut inputs: Vec<crate::protocol::ProjectionInput>,
) -> Result<Option<Vec<crate::protocol::ProjectionInput>>, WorkGraphNormError> {
    use crate::protocol::*;

    let prior = state
        .allocator
        .latest_workgraph_task_response_revision(comment_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;

    // A response edited so it no longer addresses the workflow is retracted
    // with the same fence that admitted it. Deletion is handled earlier, in
    // the shared comment-deletion branch.
    if !body_opens_with_workgraph_mention(body) {
        if let Some(previous) = prior
            .as_ref()
            .filter(|previous| !previous.tombstone && previous.revision <= updated_at_revision)
        {
            authorize_workgraph_repository(state, payload, None)?;
            inputs.push(ProjectionInput::DeleteTaskResponse {
                source_key: comment_id.to_string(),
                task_source_key: previous.identity.task_source_key.clone(),
                task_id: previous.identity.task_id.clone(),
                actor_id: previous.identity.actor_id.clone(),
                updated_at_revision,
            });
        }
        return Ok((!inputs.is_empty()).then_some(inputs));
    }

    authorize_workgraph_repository(state, payload, None)?;
    let author = comment.get("user").ok_or_else(|| {
        WorkGraphNormError::InvalidPayload("task response has no author".to_string())
    })?;
    if identity_is_bot_or_agent(author) {
        return Ok((!inputs.is_empty()).then_some(inputs));
    }
    let author_type = required_string(author, "type", "task response author")?;
    if author_type != "User" {
        return Ok((!inputs.is_empty()).then_some(inputs));
    }
    let author_id = required_string(author, "node_id", "task response author")?.to_string();
    let author_login = required_string(author, "login", "task response author")?.to_string();
    let author_database_id = author
        .get("id")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| {
            WorkGraphNormError::InvalidPayload(
                "task response author has no numeric GitHub ID".to_string(),
            )
        })?;
    // An edit is authored by whoever performed it, so a third party editing a
    // human's response can never keep it admitted under that human's identity.
    let editor = comment
        .get("editor")
        .filter(|editor| !editor.is_null())
        .or_else(|| payload.get("sender").filter(|sender| !sender.is_null()));
    if action == "edited"
        && editor.is_some_and(|editor| {
            identity_is_bot_or_agent(editor)
                || editor.get("node_id").and_then(Value::as_str) != Some(author_id.as_str())
        })
    {
        return Ok((!inputs.is_empty()).then_some(inputs));
    }
    // A human speaks for a task only while GitHub reports them as a current
    // WorkGraph-managed assignee, so authority is revoked the moment they are
    // removed. The numeric ID is the identity: it survives a rename and both
    // node ID encodings.
    if !task
        .assignees
        .iter()
        .any(|assignee| assignee.database_id == author_database_id)
    {
        return Ok((!inputs.is_empty()).then_some(inputs));
    }
    // A response always answers an open lifecycle subject, and the subject is
    // what resolves the actor: a worker against the metadata its lease was
    // acquired with, an evaluator against the current catalog. Without a
    // subject there is nothing on this task for a human to respond to.
    let Some((actor_id, subject)) = state
        .allocator
        .workgraph_task_response_subject(issue_node_id, author_database_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?
    else {
        return Ok((!inputs.is_empty()).then_some(inputs));
    };
    let (role, dispatch_id, lease_id, result_id) = match subject {
        TaskResponseSubject::Worker {
            dispatch_id,
            lease_id,
        } => (
            TaskResponseRole::Worker,
            Some(dispatch_id),
            Some(lease_id),
            None,
        ),
        TaskResponseSubject::Evaluator { result_id } => {
            (TaskResponseRole::Evaluator, None, None, Some(result_id))
        }
    };
    if body.trim().is_empty() {
        return Ok((!inputs.is_empty()).then_some(inputs));
    }
    if body.len() > MAX_TASK_RESPONSE_BODY_BYTES {
        return Err(WorkGraphNormError::InvalidPayload(format!(
            "task response body exceeds {MAX_TASK_RESPONSE_BODY_BYTES} bytes"
        )));
    }
    // The task identity was validated when the task was admitted, so the
    // response binds to the projected identity rather than re-reading a body
    // Core does not own the parser for.
    let Some((task_id, root_issue_id, workflow_run_id)) = state
        .allocator
        .workgraph_task_identity(issue_node_id)
        .await
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?
    else {
        return Ok((!inputs.is_empty()).then_some(inputs));
    };
    let document = TaskResponseDocument {
        source_key: comment_id.to_string(),
        task_source_key: issue_node_id.to_string(),
        actor_id,
        task_id,
        root_issue_id,
        workflow_run_id,
        role,
        dispatch_id,
        lease_id,
        result_id,
        author_database_id,
        author_id,
        author_login,
        body_digest: derive_workgraph_response_body_digest(body),
        body: body.to_string(),
        created_at_revision: lifecycle_created_revision(comment)?,
        updated_at_revision,
    };
    if document.created_at_revision > document.updated_at_revision {
        return Err(WorkGraphNormError::InvalidPayload(
            "task response updated_at precedes created_at".to_string(),
        ));
    }
    let fingerprint = task_response_fingerprint(&document)
        .map_err(|error| WorkGraphNormError::InvalidPayload(error.to_string()))?;
    if let Some(previous) = prior.as_ref() {
        if !should_accept_task_response_upsert(previous, updated_at_revision, &fingerprint)? {
            return Ok((!inputs.is_empty()).then_some(inputs));
        }
    }
    inputs.push(ProjectionInput::UpsertTaskResponse(document));
    Ok(Some(inputs))
}

/// Whether one observed task response supersedes what is already recorded.
///
/// Mirrors the Root Issue comment rule exactly. A newer revision wins. An
/// equal revision carrying the same GitHub evidence is a redelivery and emits
/// nothing, so an already-recorded response is never rebound to whatever
/// lifecycle subject happens to be open now. Genuine same-revision divergence
/// is ambiguous rather than invalid: GitHub is asked again.
fn should_accept_task_response_upsert(
    previous: &TaskResponseRevisionState,
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
        "equal-revision task response content is ambiguous; redeliver after an authoritative \
         comment read"
            .to_string(),
    ))
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
        updated_at_revision: comment_updated_revision(comment)?,
    };
    inputs.push(ProjectionInput::UpsertLifecycleArtifact(artifact));
    Ok(())
}

fn should_accept_lifecycle_tombstone(
    previous: &LifecycleArtifactRevisionState,
    revision: i64,
) -> bool {
    revision > previous.revision || revision == previous.revision && !previous.tombstone
}

fn lifecycle_author_is_trusted(
    state: &IngressState,
    comment: &serde_json::Value,
    role: crate::protocol::LifecycleTrustRole,
) -> bool {
    let author = comment.get("user");
    state
        .protocol_trust
        .as_ref()
        .is_some_and(|trust| match role {
            crate::protocol::LifecycleTrustRole::Assigner => trust.is_assigner(author),
            crate::protocol::LifecycleTrustRole::Reporter => trust.is_reporter(author),
        })
}

fn should_accept_lifecycle_upsert(
    previous: &LifecycleArtifactRevisionState,
    document: &LifecycleArtifactDocument,
) -> Result<bool, WorkGraphNormError> {
    if document.updated_at_revision < previous.revision {
        return Ok(false);
    }
    if document.updated_at_revision > previous.revision {
        return Ok(true);
    }
    if previous.tombstone {
        return Ok(false);
    }
    if previous.document.as_ref() == Some(document) {
        return Ok(false);
    }
    Err(WorkGraphNormError::Unavailable(
        "equal-revision WorkGraph lifecycle comment content is ambiguous; redeliver after an \
         authoritative comment read"
            .to_string(),
    ))
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
        || !issue_is_root_candidate(issue, &state.task_issue_type, &state.workflow_mappings)
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
    if issue_is_root_candidate(sub_issue, &state.task_issue_type, &state.workflow_mappings) {
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
            .is_root_candidate(
                &state.workflow_mappings,
                child_node_id,
                &state.task_issue_type,
            )
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
        // A `sub_issues` payload carries the hierarchy transition, not the
        // child's assignee set, so the cached authority is preserved and the
        // next `issues` delivery remains the only writer of assignees.
        assignees: previous
            .as_ref()
            .map(|document| document.assignees.clone())
            .unwrap_or_else(|| issue_assignees(sub_issue)),
    };
    let incoming_state = issue_authority_state(
        sub_issue,
        child_repository,
        task_doc.parent_source_key.clone(),
        &state.task_issue_type,
        &state.workflow_mappings,
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
            .issue_authority_state(
                &state.workflow_mappings,
                child_node_id,
                &state.task_issue_type,
            )
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
    use crate::config::{TaskIssueType, TrustedIdentity, WorkflowDefinitionConfig};
    use crate::protocol::{
        derive_workgraph_id, GitHubIssueLocator, PreparedProjection, PreparedProjectionCommit,
        ProjectionInput, TaskDocument, WorkGraphAllocatorProjection, WorkGraphAssignmentBinding,
        WorkGraphDispatchBinding, WorkGraphResultBinding, WorkGraphTaskBinding,
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

    fn test_task_id(seed: &str) -> String {
        derive_workgraph_id("task", &[seed])
    }

    #[test]
    fn admission_generation_uses_canonical_cross_vector() {
        assert_eq!(
            admission_generation_id("I_root", "delivery-123", "workgraph", "workgraph"),
            derive_workgraph_id(
                "admission",
                &["I_root", "delivery-123", "workgraph", "workgraph"]
            )
        );
    }

    #[test]
    fn admission_generation_is_distinct_per_mapping_in_one_delivery() {
        let foo = admission_generation_id("I_root", "delivery-123", "foo", "workgraph:foo");
        let bar = admission_generation_id("I_root", "delivery-123", "bar", "workgraph:bar");
        let legacy = admission_generation_id("I_root", "delivery-123", "workgraph", "workgraph");
        assert_ne!(foo, bar);
        assert_ne!(foo, legacy);
        assert_ne!(bar, legacy);
        for id in [&foo, &bar, &legacy] {
            assert!(id.starts_with("urn:drasi:workgraph:id:v1:admission:sha256:"));
        }
    }

    #[derive(Default)]
    struct RecordingProjector {
        committed: Arc<Mutex<Vec<Vec<ProjectionInput>>>>,
        restored: Arc<Mutex<Vec<Vec<u8>>>>,
        change_count: usize,
        /// A complete allocator projection a test pins, standing in for the
        /// whole-graph rebuild a real projector performs on every batch.
        lifecycle: Arc<Mutex<Option<WorkGraphAllocatorProjection>>>,
    }

    struct RecordingCommit {
        inputs: Vec<ProjectionInput>,
        committed: Arc<Mutex<Vec<Vec<ProjectionInput>>>>,
    }

    #[test]
    fn lease_validation_response_includes_authoritative_numeric_attempt() {
        let task_id = test_task_id("task");
        let assignment_id = derive_workgraph_id("assignment", &["assignment"]);
        let active = WorkGraphActiveLease {
            dispatch_id: String::new(),
            actor_kind: crate::agents::ActorKind::Agent,
            actor_github: None,
            lease_id: derive_workgraph_id("lease", &[&task_id, &assignment_id, "2"]),
            task_source_key: "task-source".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: derive_workgraph_id("workflow-run", &["run"]),
            task_id,
            task_element_id: "task-element".to_string(),
            assignment_source_key: "assignment-source".to_string(),
            assignment_id,
            attempt: 2,
            executor_id: "executor".to_string(),
            slot_id: "executor/1".to_string(),
            slot_number: 1,
            acquired_at: "2026-08-30T20:00:00Z".to_string(),
            expires_at: "2026-08-30T20:05:00Z".to_string(),
            has_dispatch: true,
            completed: false,
            completion_eligible: false,
            route_selected: false,
        };
        let response = serde_json::to_value(LeaseValidationResponse::from_active(
            active,
            "result-claim".to_string(),
        ))
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
                        task_id: test_task_id(&document.source_key),
                        task_element_id: format!("task:{}", document.source_key),
                        root_issue_id: "root".to_string(),
                        workflow_run_id: derive_workgraph_id("workflow-run", &["run"]),
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
            let allocator = match self.lifecycle.lock().await.clone() {
                Some(projection) => projection,
                None => WorkGraphAllocatorProjection {
                    tasks,
                    ..WorkGraphAllocatorProjection::default()
                },
            };
            Ok(PreparedProjection {
                changes,
                allocator,
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

    /// The implicit legacy mapping every pre-existing test relies on.
    fn legacy_mapping() -> crate::config::ResolvedWorkflowMapping {
        crate::config::ResolvedWorkflowMapping {
            id: LEGACY_WORKFLOW_MAPPING_ID.to_string(),
            label: WORKGRAPH_ADMISSION_LABEL.to_string(),
            definition_repository: "acme/widgets".to_string(),
            definition_ref: "main".to_string(),
            definition_path: ".github/workgraph/workflows/issue-lifecycle-v1.body".to_string(),
        }
    }

    fn named_mapping(id: &str, label: &str, path: &str) -> crate::config::ResolvedWorkflowMapping {
        crate::config::ResolvedWorkflowMapping {
            id: id.to_string(),
            label: label.to_string(),
            definition_repository: "acme/widgets".to_string(),
            definition_ref: "main".to_string(),
            definition_path: path.to_string(),
        }
    }

    fn legacy_mapping_set() -> WorkflowMappingSet {
        WorkflowMappingSet::new(vec![legacy_mapping()])
    }

    async fn ingress_state(
        trust: Option<ProtocolTrust>,
    ) -> (TempDir, Arc<RecordingProjector>, IngressState) {
        ingress_state_with_mappings(trust, legacy_mapping_set()).await
    }

    /// The same state, plus the durable store and WAL behind it, so a test can
    /// rebuild the allocator exactly as a process restart does.
    async fn restartable_ingress_state(
        trust: Option<ProtocolTrust>,
    ) -> (
        TempDir,
        Arc<RecordingProjector>,
        IngressState,
        Arc<MemoryStateStoreProvider>,
        Arc<RedbWalProvider>,
    ) {
        let temp = TempDir::new().expect("tempdir");
        let store = Arc::new(MemoryStateStoreProvider::new());
        let wal = Arc::new(RedbWalProvider::new(temp.path().join("wal")));
        wal.register("source", WriteAheadLogConfig::default())
            .await
            .expect("register WAL");
        let allocator = Arc::new(Allocator::new(
            "source".to_string(),
            store.clone(),
            wal.clone(),
        ));
        let projector = Arc::new(RecordingProjector::default());
        let state = IngressState {
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
            admission_client: None,
            workflow_mappings: legacy_mapping_set(),
            projection_gate: Mutex::new(()),
            notify: Arc::new(Notify::new()),
        };
        (temp, projector, state, store, wal)
    }

    async fn ingress_state_with_mappings(
        trust: Option<ProtocolTrust>,
        workflow_mappings: WorkflowMappingSet,
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
                admission_client: None,
                workflow_mappings,
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

    #[tokio::test]
    async fn new_task_issue_uses_authoritative_parent_after_hierarchy_delivery() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": {"node": {"parent": {"id": "I_root"}}}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_temp, _projector, mut state) = ingress_state(Some(task_trust())).await;
        state.admission_client = Some(
            AdmissionClient::new(&WorkflowDefinitionConfig {
                token: "token".to_string(),
                api_base_url: server.uri(),
                ..WorkflowDefinitionConfig::default()
            })
            .expect("admission client"),
        );
        let body = "WorkGraphTask/v1\n\n```json\n{}\n```\n";

        let inputs = try_workgraph_issue(
            &state,
            "task-opened",
            &payload("opened", task_issue("I_task", body)),
        )
        .await
        .expect("normalize task Issue")
        .expect("task projection");

        assert!(inputs.iter().any(|input| matches!(
            input,
            ProjectionInput::UpsertTask(document)
                if document.parent_source_key.as_deref() == Some("I_root")
        )));
        let fingerprint = inputs
            .iter()
            .find_map(|input| match input {
                ProjectionInput::RecordIssueRevision {
                    state_fingerprint, ..
                } => Some(state_fingerprint),
                _ => None,
            })
            .expect("Issue revision fingerprint");
        assert_eq!(
            fingerprint,
            &issue_authority_state(
                &task_issue("I_task", body),
                payload("opened", task_issue("I_task", body)).get("repository"),
                Some("I_root".to_string()),
                &state.task_issue_type,
                &state.workflow_mappings,
                false,
            )
            .expect("authoritative task state")
            .fingerprint()
            .expect("authoritative task fingerprint")
        );
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

    // ── Human parity: assignees and natural task responses ────────────

    /// The one deterministic human the WorkGraph mock simulates.
    const HUMAN_ACTOR_ID: &str = "human-agentofreality";
    const HUMAN_DATABASE_ID: u64 = 4_021_243;
    const HUMAN_NODE_ID: &str = "MDQ6VXNlcjQwMjEyNDM=";
    const HUMAN_LOGIN: &str = "agentofreality";
    const HUMAN_ACTOR_FILE: &str = "version: 2\nactors:\n- actorId: executor\n  kind: agent\n  \
                                    slots: 1\n  leaseDuration: PT15M\n- actorId: \
                                    human-agentofreality\n  kind: human\n  slots: 1\n  \
                                    leaseDuration: PT8H\n  github:\n    databaseId: 4021243\n    \
                                    nodeId: MDQ6VXNlcjQwMjEyNDM=\n    login: agentofreality\n";

    fn human_webhook_user() -> Value {
        json!({
            "id": HUMAN_DATABASE_ID,
            "node_id": HUMAN_NODE_ID,
            "login": HUMAN_LOGIN,
            "type": "User"
        })
    }

    /// A task Issue payload carrying the exact assignee shape GitHub sends.
    fn assigned_task_issue(node_id: &str, assignees: &[Value], updated_at: &str) -> Value {
        let mut issue = task_issue(node_id, "WorkGraphTask/v1\n\n```json\n{}\n```\n");
        issue["updated_at"] = json!(updated_at);
        issue["assignee"] = assignees.first().cloned().unwrap_or(Value::Null);
        issue["assignees"] = json!(assignees);
        issue
    }

    async fn seed_human_actor_catalog(state: &IngressState) {
        let file = crate::agents::parse_agent_file(HUMAN_ACTOR_FILE).expect("actor catalog");
        state
            .allocator
            .sync_agents(
                &crate::agents::AgentFileLocation {
                    repository: "acme/widgets".to_string(),
                    r#ref: "main".to_string(),
                    path: ".github/workgraph/agents.yaml".to_string(),
                },
                &file,
                &crate::agents::AgentFileContent {
                    text: HUMAN_ACTOR_FILE.to_string(),
                    oid: "oid".to_string(),
                },
                1,
            )
            .await
            .expect("sync actor catalog");
    }

    /// Seeds a task Issue assigned to the catalog human.
    async fn seed_assigned_task(
        state: &IngressState,
        projector: &RecordingProjector,
        assignees: &[Value],
    ) {
        seed_root_issue(state, projector).await;
        let event = payload(
            "opened",
            assigned_task_issue("I_task", assignees, "2026-08-01T00:00:00Z"),
        );
        let inputs = try_workgraph_issue(state, "seed-task", &event)
            .await
            .expect("normalize task")
            .expect("task inputs");
        state
            .allocator
            .ingest_workgraph(projector, inputs, 2, "seed-task")
            .await
            .expect("persist task");
    }

    const ASSIGNMENT_SOURCE: &str = "IC_assignment";
    const DISPATCH_SOURCE: &str = "IC_dispatch";
    const RESULT_SOURCE: &str = "IC_result";

    fn lifecycle_artifact(source_key: &str, marker: &str) -> ProjectionInput {
        lifecycle_artifact_at(source_key, marker, 1)
    }

    /// The same artifact at an explicit revision, so a test that watched one
    /// be retracted can re-supply it past the retraction's revision fence.
    fn lifecycle_artifact_at(source_key: &str, marker: &str, revision: i64) -> ProjectionInput {
        ProjectionInput::UpsertLifecycleArtifact(LifecycleArtifactDocument {
            source_key: source_key.to_string(),
            task_source_key: "I_task".to_string(),
            body: format!("{marker}\n\n```json\n{{}}\n```\n"),
            created_at_revision: 1,
            updated_at_revision: revision,
        })
    }

    fn human_assignment_binding() -> WorkGraphAssignmentBinding {
        WorkGraphAssignmentBinding {
            source_key: ASSIGNMENT_SOURCE.to_string(),
            task_source_key: "I_task".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: derive_workgraph_id("workflow-run", &["run"]),
            task_id: test_task_id("I_task"),
            assignment_id: derive_workgraph_id("assignment", &["assignment"]),
            permitted_executors: vec![HUMAN_ACTOR_ID.to_string()],
        }
    }

    fn human_task_projection() -> WorkGraphAllocatorProjection {
        WorkGraphAllocatorProjection {
            tasks: vec![WorkGraphTaskBinding {
                source_key: "I_task".to_string(),
                task_id: test_task_id("I_task"),
                task_element_id: "task:I_task".to_string(),
                root_issue_id: "root".to_string(),
                workflow_run_id: derive_workgraph_id("workflow-run", &["run"]),
            }],
            assignments: vec![human_assignment_binding()],
            ..WorkGraphAllocatorProjection::default()
        }
    }

    fn human_dispatch_binding(lease_id: &str) -> WorkGraphDispatchBinding {
        WorkGraphDispatchBinding {
            source_key: DISPATCH_SOURCE.to_string(),
            task_source_key: "I_task".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: derive_workgraph_id("workflow-run", &["run"]),
            task_id: test_task_id("I_task"),
            assignment_id: derive_workgraph_id("assignment", &["assignment"]),
            lease_id: lease_id.to_string(),
            executor_id: HUMAN_ACTOR_ID.to_string(),
            slot_id: format!("{HUMAN_ACTOR_ID}/1"),
            dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
        }
    }

    fn agent_task_projection() -> WorkGraphAllocatorProjection {
        let mut projection = human_task_projection();
        projection.assignments[0].permitted_executors = vec!["executor".to_string()];
        projection
    }

    fn agent_dispatch_binding(lease_id: &str) -> WorkGraphDispatchBinding {
        WorkGraphDispatchBinding {
            executor_id: "executor".to_string(),
            slot_id: "executor/1".to_string(),
            ..human_dispatch_binding(lease_id)
        }
    }

    /// Drives the task to an open worker subject held by the agent executor.
    async fn seed_agent_worker_lease(
        state: &IngressState,
        projector: &RecordingProjector,
    ) -> String {
        *projector.lifecycle.lock().await = Some(agent_task_projection());
        state
            .allocator
            .ingest_workgraph(
                projector,
                vec![lifecycle_artifact(
                    ASSIGNMENT_SOURCE,
                    "WorkGraphTaskAssignment/v1",
                )],
                3,
                "seed-agent-assignment",
            )
            .await
            .expect("persist agent assignment");
        let lease_id = derive_workgraph_id(
            "lease",
            &[
                &test_task_id("I_task"),
                &derive_workgraph_id("assignment", &["assignment"]),
                "1",
            ],
        );
        let mut dispatched = agent_task_projection();
        dispatched.dispatches = vec![agent_dispatch_binding(&lease_id)];
        *projector.lifecycle.lock().await = Some(dispatched);
        state
            .allocator
            .ingest_workgraph(
                projector,
                vec![lifecycle_artifact(
                    DISPATCH_SOURCE,
                    "WorkGraphTaskDispatch/v1",
                )],
                4,
                "seed-agent-dispatch",
            )
            .await
            .expect("persist agent dispatch");
        lease_id
    }

    /// Drives the task to an open worker subject: the catalog human holds the
    /// task's active lease and its Dispatch exists.
    ///
    /// A human takes a lease at exactly the same lifecycle point an agent
    /// does, so this is the ordinary allocator path with a human executor.
    async fn seed_worker_lease(state: &IngressState, projector: &RecordingProjector) -> String {
        *projector.lifecycle.lock().await = Some(human_task_projection());
        state
            .allocator
            .ingest_workgraph(
                projector,
                vec![lifecycle_artifact(
                    ASSIGNMENT_SOURCE,
                    "WorkGraphTaskAssignment/v1",
                )],
                3,
                "seed-assignment",
            )
            .await
            .expect("persist assignment");
        let lease_id = derive_workgraph_id(
            "lease",
            &[
                &test_task_id("I_task"),
                &derive_workgraph_id("assignment", &["assignment"]),
                "1",
            ],
        );
        let mut dispatched = human_task_projection();
        dispatched.dispatches = vec![human_dispatch_binding(&lease_id)];
        *projector.lifecycle.lock().await = Some(dispatched);
        state
            .allocator
            .ingest_workgraph(
                projector,
                vec![lifecycle_artifact(
                    DISPATCH_SOURCE,
                    "WorkGraphTaskDispatch/v1",
                )],
                4,
                "seed-dispatch",
            )
            .await
            .expect("persist dispatch");
        lease_id
    }

    fn task_response_event(comment_id: &str, action: &str, body: &str, updated_at: &str) -> Value {
        json!({
            "action": action,
            "organization": {"login": "acme"},
            "repository": {
                "name": "widgets",
                "full_name": "acme/widgets",
                "node_id": "R_widgets"
            },
            "sender": human_webhook_user(),
            "issue": assigned_task_issue(
                "I_task",
                &[human_webhook_user()],
                "2026-08-01T00:00:00Z"
            ),
            "comment": {
                "id": 900,
                "node_id": comment_id,
                "body": body,
                "user": human_webhook_user(),
                "created_at": "2026-08-01T00:01:00Z",
                "updated_at": updated_at
            }
        })
    }

    /// Persists one normalized batch, keeping the task binding present.
    ///
    /// `RecordingProjector` derives its allocator projection from the inputs
    /// of the batch it is handed, while a real projector rebuilds the whole
    /// desired graph, so the task upsert is replayed alongside the batch to
    /// keep the double's task identity set complete.
    async fn persist_with_task(
        state: &IngressState,
        projector: &RecordingProjector,
        inputs: Vec<ProjectionInput>,
        effective_from: u64,
        origin: &str,
    ) {
        let task_event = payload(
            "opened",
            assigned_task_issue("I_task", &[human_webhook_user()], "2026-08-01T00:00:00Z"),
        );
        let mut batch = try_workgraph_issue(state, "replay-task", &task_event)
            .await
            .expect("normalize task")
            .expect("task inputs");
        batch.extend(inputs);
        state
            .allocator
            .ingest_workgraph(projector, batch, effective_from, origin)
            .await
            .expect("persist batch");
    }

    fn upserted_task(inputs: &[ProjectionInput]) -> &TaskDocument {
        inputs
            .iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertTask(document) => Some(document),
                _ => None,
            })
            .expect("normalized task")
    }

    fn upserted_task_response(
        inputs: &[ProjectionInput],
    ) -> &crate::protocol::TaskResponseDocument {
        inputs
            .iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertTaskResponse(document) => Some(document),
                _ => None,
            })
            .expect("normalized task response")
    }

    /// The exact authoritative Issue query the WorkGraph mock recognizes.
    ///
    /// The mock matches the query string byte-for-byte, so this is a contract
    /// assertion, not a formatting preference: if the Source and the mock ever
    /// disagree the loopback read fails as an unknown operation.
    const MOCK_SOURCE_ISSUE_AUTHORITY_QUERY: &str = "query($id: ID!) { node(id: $id) { ... on Issue { id databaseId number title body state stateReason issueType { id name } parent { id } repository { id name owner { login } } labels(first: 100) { nodes { name } pageInfo { hasNextPage } } assignees(first: 100) { nodes { databaseId id login } pageInfo { hasNextPage } } } } }";

    #[tokio::test]
    async fn the_authoritative_issue_query_matches_the_mock_contract_exactly() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"data": {"node": null}})))
            .expect(1)
            .mount(&server)
            .await;
        let client = AdmissionClient::new(&WorkflowDefinitionConfig {
            repository: "acme/widgets".to_string(),
            r#ref: "main".to_string(),
            path: ".github/workgraph/workflow.body".to_string(),
            token: "token".to_string(),
            api_base_url: server.uri(),
        })
        .expect("admission client");
        client
            .issue_authority_state(
                &legacy_mapping_set(),
                "I_task",
                &TaskIssueType {
                    id: "IT_task".to_string(),
                    name: "WorkGraphTask".to_string(),
                },
            )
            .await
            .expect("authoritative read");
        let requests = server.received_requests().await.expect("recorded requests");
        let body: Value = serde_json::from_slice(&requests[0].body).expect("request body is JSON");
        assert_eq!(
            body["query"].as_str().expect("query string"),
            MOCK_SOURCE_ISSUE_AUTHORITY_QUERY
        );
    }

    #[tokio::test]
    async fn assignee_transitions_are_accepted_only_from_the_trusted_service_actor() {
        let (_temp, _projector, state) = ingress_state(Some(task_trust())).await;
        let issue = assigned_task_issue("I_task", &[human_webhook_user()], "2026-08-01T00:05:00Z");

        // The service actor that runs the workflow performs the assignment.
        let inputs = try_workgraph_issue(&state, "assign-1", &payload("assigned", issue.clone()))
            .await
            .expect("trusted assignment")
            .expect("assignment inputs");
        assert_eq!(
            upserted_task(&inputs).assignees,
            vec![crate::protocol::TaskAssignee {
                database_id: HUMAN_DATABASE_ID,
                node_id: HUMAN_NODE_ID.to_string(),
                login: HUMAN_LOGIN.to_string(),
            }]
        );

        // An assigner is equally authoritative for an assignment transition.
        let mut assigner_event = payload(
            "unassigned",
            assigned_task_issue("I_task", &[], "2026-08-01T00:06:00Z"),
        );
        assigner_event["sender"] = json!({"node_id": "U_dispatch", "login": "dispatcher"});
        let inputs = try_workgraph_issue(&state, "assign-2", &assigner_event)
            .await
            .expect("assigner unassignment")
            .expect("unassignment inputs");
        assert!(upserted_task(&inputs).assignees.is_empty());

        // The assignee is the subject of the assignment, never its author.
        let mut human_event = payload("assigned", issue.clone());
        human_event["sender"] = human_webhook_user();
        assert!(matches!(
            try_workgraph_issue(&state, "assign-3", &human_event).await,
            Err(WorkGraphNormError::Untrusted(_))
        ));

        // Every other task action keeps the exact creator-only gate.
        let mut assigner_edit = payload("edited", issue);
        assigner_edit["sender"] = json!({"node_id": "U_dispatch", "login": "dispatcher"});
        assert!(matches!(
            try_workgraph_issue(&state, "assign-4", &assigner_edit).await,
            Err(WorkGraphNormError::Untrusted(_))
        ));
    }

    #[tokio::test]
    async fn assignees_are_sorted_deduplicated_and_fence_the_issue_state() {
        let (_temp, _projector, state) = ingress_state(Some(task_trust())).await;
        let second = json!({
            "id": 11,
            "node_id": "U_second",
            "login": "second",
            "type": "User"
        });
        let issue = assigned_task_issue(
            "I_task",
            &[human_webhook_user(), second.clone(), human_webhook_user()],
            "2026-08-01T00:05:00Z",
        );
        let inputs =
            try_workgraph_issue(&state, "assign-order", &payload("assigned", issue.clone()))
                .await
                .expect("assignment")
                .expect("assignment inputs");
        let assignees = &upserted_task(&inputs).assignees;
        assert_eq!(assignees.len(), 2);
        assert_eq!(assignees[0].database_id, 11);
        assert_eq!(assignees[1].database_id, HUMAN_DATABASE_ID);

        // An assignment change is a real state transition, so the recorded
        // fingerprint differs from the same Issue with no assignee.
        let with_assignee = issue_authority_state(
            &issue,
            None,
            None,
            &state.task_issue_type,
            &state.workflow_mappings,
            false,
        )
        .expect("assigned state")
        .fingerprint()
        .expect("fingerprint");
        let without_assignee = issue_authority_state(
            &assigned_task_issue("I_task", &[], "2026-08-01T00:05:00Z"),
            None,
            None,
            &state.task_issue_type,
            &state.workflow_mappings,
            false,
        )
        .expect("unassigned state")
        .fingerprint()
        .expect("fingerprint");
        assert_ne!(with_assignee, without_assignee);
    }

    #[tokio::test]
    async fn a_natural_task_response_from_the_assigned_human_is_bound_and_fenced() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        let lease_id = seed_worker_lease(&state, projector.as_ref()).await;

        let inputs = try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_response",
                "created",
                "@workgraph looks good, shipping it",
                "2026-08-01T00:01:00Z",
            ),
        )
        .await
        .expect("normalize response")
        .expect("response inputs");
        let document = upserted_task_response(&inputs);
        assert_eq!(document.source_key, "IC_response");
        assert_eq!(document.task_source_key, "I_task");
        assert_eq!(document.actor_id, HUMAN_ACTOR_ID);
        assert_eq!(document.task_id, test_task_id("I_task"));
        assert_eq!(document.root_issue_id, "root");
        assert_eq!(document.author_database_id, HUMAN_DATABASE_ID);
        assert_eq!(document.author_id, HUMAN_NODE_ID);
        assert_eq!(document.author_login, HUMAN_LOGIN);
        // The human holds the task's active Dispatch and lease, so the
        // response is bound to that worker subject.
        assert_eq!(document.role, crate::protocol::TaskResponseRole::Worker);
        assert_eq!(
            document.dispatch_id.as_deref(),
            Some(derive_workgraph_id("dispatch", &["dispatch"]).as_str())
        );
        assert_eq!(document.lease_id.as_deref(), Some(lease_id.as_str()));
        assert_eq!(document.result_id, None);
        // Core binds and authenticates; it never interprets the body.
        assert_eq!(document.body, "@workgraph looks good, shipping it");
        // The kernel's framed, domain-separated contract, pinned to the same
        // fixed vector the protocol module asserts.
        assert_eq!(
            document.body_digest,
            crate::protocol::derive_workgraph_response_body_digest(&document.body)
        );
        assert_eq!(
            document.body_digest,
            "sha256:e85beef21c44c265825ca0b6fc461e6f580debac902577978ae005a9f1994467"
        );
        assert_eq!(document.created_at_revision, document.updated_at_revision);

        // A comment that does not address the workflow is simply ignored.
        assert!(try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_chat",
                "created",
                "just chatting",
                "2026-08-01T00:02:00Z"
            ),
        )
        .await
        .expect("normalize chatter")
        .is_none());

        // The mention must open the first non-whitespace line.
        assert!(try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_late",
                "created",
                "context first\n@workgraph later",
                "2026-08-01T00:02:00Z"
            ),
        )
        .await
        .expect("normalize trailing mention")
        .is_none());

        // Leading blank lines are still the first non-whitespace line.
        let inputs = try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_indented",
                "created",
                "\n\n   @workgraph ready",
                "2026-08-01T00:02:00Z",
            ),
        )
        .await
        .expect("normalize indented mention")
        .expect("indented inputs");
        assert_eq!(upserted_task_response(&inputs).source_key, "IC_indented");
    }

    #[tokio::test]
    async fn a_task_response_is_refused_from_a_bot_impostor_or_unassigned_human() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        seed_worker_lease(&state, projector.as_ref()).await;

        // A bot may never speak as a human responder.
        let mut bot = task_response_event(
            "IC_bot",
            "created",
            "@workgraph done",
            "2026-08-01T00:02:00Z",
        );
        bot["comment"]["user"] = json!({
            "id": 99,
            "node_id": "U_bot",
            "login": "workgraph[bot]",
            "type": "Bot"
        });
        assert!(try_workgraph_comment(&state, &bot)
            .await
            .expect("normalize bot response")
            .is_none());

        // A GitHub account that is not in the human catalog has no authority.
        let mut stranger = task_response_event(
            "IC_stranger",
            "created",
            "@workgraph done",
            "2026-08-01T00:02:00Z",
        );
        stranger["comment"]["user"] = json!({
            "id": 12345,
            "node_id": "U_stranger",
            "login": "stranger",
            "type": "User"
        });
        assert!(try_workgraph_comment(&state, &stranger)
            .await
            .expect("normalize stranger response")
            .is_none());

        // The numeric ID is the identity. A renamed login or a re-encoded node
        // ID is the same GitHub account and must keep its authority, because
        // GitHub itself signed the numeric ID in this payload.
        for (index, (field, value)) in [
            ("login", json!("agent-of-reality")),
            ("node_id", json!("U_kgDOAD1Q2w")),
        ]
        .into_iter()
        .enumerate()
        {
            let comment_id = format!("IC_renamed_{index}");
            let mut renamed = task_response_event(
                &comment_id,
                "created",
                "@workgraph done",
                "2026-08-01T00:02:00Z",
            );
            renamed["comment"]["user"][field] = value;
            renamed["issue"]["assignees"][0][field] = renamed["comment"]["user"][field].clone();
            let inputs = try_workgraph_comment(&state, &renamed)
                .await
                .expect("normalize renamed response")
                .unwrap_or_else(|| panic!("a changed {field} must not revoke authority"));
            assert_eq!(upserted_task_response(&inputs).actor_id, HUMAN_ACTOR_ID);
        }

        // A different, or malformed, numeric ID is a different account.
        for (index, id) in [json!(999_999), json!(0)].into_iter().enumerate() {
            let comment_id = format!("IC_wrongid_{index}");
            let mut foreign = task_response_event(
                &comment_id,
                "created",
                "@workgraph done",
                "2026-08-01T00:02:00Z",
            );
            foreign["comment"]["user"]["id"] = id.clone();
            foreign["issue"]["assignees"][0]["id"] = id;
            assert!(
                try_workgraph_comment(&state, &foreign)
                    .await
                    .expect("normalize foreign numeric ID")
                    .is_none(),
                "a mismatched numeric ID must not authenticate"
            );
        }

        // A third party editing the human's response cannot keep it admitted.
        let mut hijacked = task_response_event(
            "IC_hijack",
            "edited",
            "@workgraph approved",
            "2026-08-01T00:03:00Z",
        );
        hijacked["sender"] =
            json!({"node_id": "U_creator", "login": "task-creator", "type": "User"});
        hijacked["changes"] = json!({"body": {"from": "@workgraph pending"}});
        assert!(try_workgraph_comment(&state, &hijacked)
            .await
            .expect("normalize hijacked edit")
            .is_none());
    }

    #[tokio::test]
    async fn an_evaluator_response_binds_the_result_awaiting_evaluation() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        // The agent worker holds the lease; the human is only an assignee, so
        // the human never has a worker subject of its own here.
        let lease_id = seed_agent_worker_lease(&state, projector.as_ref()).await;

        // The attempt produces a Result that no Evaluation names yet.
        let mut completed = agent_task_projection();
        completed.dispatches = vec![agent_dispatch_binding(&lease_id)];
        completed.results = vec![WorkGraphResultBinding {
            source_key: RESULT_SOURCE.to_string(),
            task_source_key: "I_task".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: derive_workgraph_id("workflow-run", &["run"]),
            task_id: test_task_id("I_task"),
            result_id: derive_workgraph_id("result", &["result"]),
            lease_id: lease_id.clone(),
            attempt: 1,
        }];
        *projector.lifecycle.lock().await = Some(completed);
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![lifecycle_artifact(RESULT_SOURCE, "WorkGraphTaskResult/v1")],
                5,
                "seed-result",
            )
            .await
            .expect("persist result");

        let inputs = try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_evaluation",
                "created",
                "@workgraph this meets the bar",
                "2026-08-01T00:05:00Z",
            ),
        )
        .await
        .expect("normalize evaluator response")
        .expect("evaluator inputs");
        let document = upserted_task_response(&inputs);
        // No worker lease is open, so the human answers the Result instead,
        // and an evaluator never takes a lease of its own.
        assert_eq!(document.role, crate::protocol::TaskResponseRole::Evaluator);
        assert_eq!(
            document.result_id.as_deref(),
            Some(derive_workgraph_id("result", &["result"]).as_str())
        );
        assert_eq!(document.dispatch_id, None);
        assert_eq!(document.lease_id, None);
    }

    #[tokio::test]
    async fn a_lease_holding_worker_never_becomes_the_evaluator_of_its_own_result() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        let lease_id = seed_worker_lease(&state, projector.as_ref()).await;

        // The human worker's own attempt produced the pending Result.
        let mut completed = human_task_projection();
        completed.dispatches = vec![human_dispatch_binding(&lease_id)];
        completed.results = vec![WorkGraphResultBinding {
            source_key: RESULT_SOURCE.to_string(),
            task_source_key: "I_task".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: derive_workgraph_id("workflow-run", &["run"]),
            task_id: test_task_id("I_task"),
            result_id: derive_workgraph_id("result", &["result"]),
            lease_id: lease_id.clone(),
            attempt: 1,
        }];
        *projector.lifecycle.lock().await = Some(completed);
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![lifecycle_artifact(RESULT_SOURCE, "WorkGraphTaskResult/v1")],
                5,
                "own-result",
            )
            .await
            .expect("persist result");

        let inputs = try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_self",
                "created",
                "@workgraph I judge my own work good",
                "2026-08-01T00:06:00Z",
            ),
        )
        .await
        .expect("normalize self response")
        .expect("self inputs");
        // Still the worker: holding the lease is what decides the role, so a
        // worker can never evaluate the Result it produced.
        let document = upserted_task_response(&inputs);
        assert_eq!(document.role, crate::protocol::TaskResponseRole::Worker);
        assert_eq!(document.lease_id.as_deref(), Some(lease_id.as_str()));
        assert_eq!(document.result_id, None);
    }

    #[tokio::test]
    async fn mention_case_variants_are_admitted_and_longer_mentions_are_not() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        seed_worker_lease(&state, projector.as_ref()).await;

        // GitHub mentions are case-insensitive, so every ASCII case variant
        // reaches the protocol.
        for (index, body) in ["@WorkGraph ready", "@WORKGRAPH ready", "@wOrKgRaPh ready"]
            .into_iter()
            .enumerate()
        {
            let comment_id = format!("IC_case_{index}");
            let inputs = try_workgraph_comment(
                &state,
                &task_response_event(&comment_id, "created", body, "2026-08-01T00:05:00Z"),
            )
            .await
            .expect("normalize case variant")
            .unwrap_or_else(|| panic!("{body:?} must be admitted"));
            let document = upserted_task_response(&inputs);
            // The body is carried exactly as written, case and all.
            assert_eq!(document.body, body);
            assert_eq!(
                document.body_digest,
                crate::protocol::derive_workgraph_response_body_digest(body)
            );
        }

        // A longer mention is a different GitHub account, not ours.
        for (index, body) in [
            "@workgraphs ready",
            "@WORKGRAPHS ready",
            "@workgraph-bot ready",
        ]
        .into_iter()
        .enumerate()
        {
            let comment_id = format!("IC_other_{index}");
            assert!(
                try_workgraph_comment(
                    &state,
                    &task_response_event(&comment_id, "created", body, "2026-08-01T00:05:00Z"),
                )
                .await
                .expect("normalize foreign mention")
                .is_none(),
                "{body:?} must not be admitted"
            );
        }
    }

    #[tokio::test]
    async fn the_response_body_bound_admits_exactly_its_limit_and_no_more() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        seed_worker_lease(&state, projector.as_ref()).await;

        // Exactly at the bound is admitted.
        let mention = "@workgraph ";
        let bound = crate::protocol::MAX_TASK_RESPONSE_BODY_BYTES;
        let exact = format!("{mention}{}", "x".repeat(bound - mention.len()));
        assert_eq!(exact.len(), bound);
        let inputs = try_workgraph_comment(
            &state,
            &task_response_event("IC_exact", "created", &exact, "2026-08-01T00:05:00Z"),
        )
        .await
        .expect("normalize bounded response")
        .expect("bounded inputs");
        assert_eq!(upserted_task_response(&inputs).body.len(), bound);

        // One byte over is refused.
        let oversized = format!("{exact}x");
        assert_eq!(oversized.len(), bound + 1);
        assert!(matches!(
            try_workgraph_comment(
                &state,
                &task_response_event("IC_huge", "created", &oversized, "2026-08-01T00:06:00Z"),
            )
            .await,
            Err(WorkGraphNormError::InvalidPayload(_))
        ));
    }

    #[tokio::test]
    async fn a_deleted_task_response_is_retracted_and_fenced() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        seed_worker_lease(&state, projector.as_ref()).await;

        let created = task_response_event(
            "IC_response",
            "created",
            "@workgraph first",
            "2026-08-01T00:05:00Z",
        );
        let inputs = try_workgraph_comment(&state, &created)
            .await
            .expect("normalize create")
            .expect("create inputs");
        persist_with_task(&state, projector.as_ref(), inputs, 6, "response-create").await;

        // The real GitHub `deleted` action retracts the response, which the
        // shared comment-deletion branch must reach.
        let inputs = try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_response",
                "deleted",
                "@workgraph first",
                "2026-08-01T00:06:00Z",
            ),
        )
        .await
        .expect("normalize delete")
        .expect("delete inputs");
        assert!(matches!(
            inputs.as_slice(),
            [ProjectionInput::DeleteTaskResponse { source_key, task_id, actor_id, .. }]
                if source_key == "IC_response"
                    && task_id == &test_task_id("I_task")
                    && actor_id == HUMAN_ACTOR_ID
        ));
        persist_with_task(&state, projector.as_ref(), inputs, 7, "response-delete").await;

        // The tombstone fences a stale redelivery of the original comment.
        assert!(try_workgraph_comment(&state, &created)
            .await
            .expect("normalize stale create")
            .is_none());
        // Deleting again emits nothing further.
        assert!(try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_response",
                "deleted",
                "@workgraph first",
                "2026-08-01T00:07:00Z"
            ),
        )
        .await
        .expect("normalize repeat delete")
        .is_none());
    }

    #[tokio::test]
    async fn deleting_a_lifecycle_or_root_comment_still_works_alongside_responses() {
        // The shared deletion branch gained a task-response tombstone; the
        // lifecycle and Root paths through it must be untouched.
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_root_issue(&state, projector.as_ref()).await;

        let mut lifecycle = human_root_comment_event(
            "IC_lifecycle",
            "deleted",
            "WorkGraphTaskResult/v1\n\n```json\n{}\n```\n",
            "2026-08-01T00:02:00Z",
        );
        lifecycle["comment"]["user"] = json!({"node_id": "U_reporter", "login": "reporter"});
        lifecycle["sender"] = json!({"node_id": "U_reporter", "login": "reporter"});
        let inputs = try_workgraph_comment(&state, &lifecycle)
            .await
            .expect("normalize lifecycle delete")
            .expect("lifecycle delete inputs");
        assert!(inputs
            .iter()
            .any(|input| matches!(input, ProjectionInput::DeleteLifecycleArtifact { .. })));
        assert!(inputs
            .iter()
            .all(|input| !matches!(input, ProjectionInput::DeleteTaskResponse { .. })));

        let created =
            human_root_comment_event("IC_root", "created", "resume now", "2026-08-01T00:03:00Z");
        let inputs = try_workgraph_comment(&state, &created)
            .await
            .expect("normalize root comment")
            .expect("root comment inputs");
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), inputs, 4, "root-comment")
            .await
            .expect("persist root comment");
        let inputs = try_workgraph_comment(
            &state,
            &human_root_comment_event("IC_root", "deleted", "resume now", "2026-08-01T00:04:00Z"),
        )
        .await
        .expect("normalize root delete")
        .expect("root delete inputs");
        assert!(inputs
            .iter()
            .any(|input| matches!(input, ProjectionInput::DeleteRootIssueComment { .. })));
        assert!(inputs
            .iter()
            .all(|input| !matches!(input, ProjectionInput::DeleteTaskResponse { .. })));
    }

    #[tokio::test]
    async fn a_malformed_projected_dispatch_identity_is_refused_before_any_response() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        *projector.lifecycle.lock().await = Some(human_task_projection());
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![lifecycle_artifact(
                    ASSIGNMENT_SOURCE,
                    "WorkGraphTaskAssignment/v1",
                )],
                3,
                "malformed-assignment",
            )
            .await
            .expect("persist assignment");
        let lease_id = derive_workgraph_id(
            "lease",
            &[
                &test_task_id("I_task"),
                &derive_workgraph_id("assignment", &["assignment"]),
                "1",
            ],
        );

        // Anything that is not empty must be a canonical typed dispatch ID.
        for (attempt, malformed) in [
            "not-a-dispatch",
            "urn:drasi:workgraph:id:v1:dispatch:sha256:short",
            &derive_workgraph_id("lease", &["wrong-type"]),
        ]
        .into_iter()
        .enumerate()
        {
            let mut projection = human_task_projection();
            projection.dispatches = vec![WorkGraphDispatchBinding {
                dispatch_id: malformed.to_string(),
                ..human_dispatch_binding(&lease_id)
            }];
            *projector.lifecycle.lock().await = Some(projection);
            let (_, rejection) = state
                .allocator
                .ingest_workgraph(
                    projector.as_ref(),
                    vec![lifecycle_artifact_at(
                        DISPATCH_SOURCE,
                        "WorkGraphTaskDispatch/v1",
                        1 + attempt as i64,
                    )],
                    4 + attempt as u64,
                    &format!("malformed-{malformed}"),
                )
                .await
                .expect("the delivery is acknowledged with a fail-closed retraction");
            // Admission refuses it and the Dispatch is retracted rather than
            // persisted onto the lease.
            assert!(
                rejection
                    .as_deref()
                    .is_some_and(|message| message.contains("invalid or duplicate dispatch")),
                "{rejection:?}"
            );
        }

        // Nothing was recorded, so there is no worker subject and no response
        // can bind to a Dispatch identity Core never admitted.
        assert!(state
            .allocator
            .workgraph_task_response_subject("I_task", HUMAN_DATABASE_ID)
            .await
            .expect("subject lookup")
            .is_none());
        assert!(try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_malformed",
                "created",
                "@workgraph done",
                "2026-08-01T00:07:00Z"
            ),
        )
        .await
        .expect("normalize response")
        .is_none());
    }

    #[tokio::test]
    async fn an_untrusted_removal_never_fences_out_a_delayed_trusted_change() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;

        // A trusted change GitHub emitted at 00:05 has not been delivered yet.
        let mut trusted = payload(
            "edited",
            assigned_task_issue("I_task", &[human_webhook_user()], "2026-08-01T00:05:00Z"),
        );
        trusted["issue"]["title"] = json!("Updated by the workflow");

        // Meanwhile an untrusted unassignment arrives carrying a newer
        // `updated_at`. It applies the removal only.
        let mut tampered = payload(
            "unassigned",
            assigned_task_issue("I_task", &[], "2026-08-01T00:09:00Z"),
        );
        tampered["issue"]["title"] = json!("Tampered");
        tampered["sender"] =
            json!({"id": 77, "node_id": "U_stranger", "login": "stranger", "type": "User"});
        let inputs = try_workgraph_issue(&state, "tampered", &tampered)
            .await
            .expect("untrusted revocation is accepted")
            .expect("revocation inputs");
        assert!(upserted_task(&inputs).assignees.is_empty());
        assert!(!upserted_task(&inputs).body.contains("Tampered"));
        // The revision watermark must not advance on state Core did not adopt.
        assert!(
            inputs
                .iter()
                .all(|input| !matches!(input, ProjectionInput::RecordIssueRevision { .. })),
            "an untrusted removal must not record a revision it did not apply"
        );
        state
            .allocator
            .ingest_workgraph(projector.as_ref(), inputs, 5, "tampered")
            .await
            .expect("persist revocation");

        // The delayed trusted change still lands: it was never fenced out by
        // the attacker-chosen timestamp.
        let inputs = try_workgraph_issue(&state, "delayed-trusted", &trusted)
            .await
            .expect("delayed trusted change is still accepted")
            .expect("trusted inputs");
        assert!(inputs
            .iter()
            .any(|input| matches!(input, ProjectionInput::RecordIssueRevision { .. })));
        assert!(inputs
            .iter()
            .any(|input| matches!(input, ProjectionInput::UpsertTask(_))));

        // Replaying the untrusted removal is a no-op.
        assert!(try_workgraph_issue(&state, "tampered-replay", &tampered)
            .await
            .expect("replayed revocation")
            .is_none());
    }

    #[tokio::test]
    async fn a_dispatch_identity_backfills_onto_a_lease_dispatched_before_the_rollout() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;

        // A projector that does not publish Dispatch identities yet.
        *projector.lifecycle.lock().await = Some(human_task_projection());
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![lifecycle_artifact(
                    ASSIGNMENT_SOURCE,
                    "WorkGraphTaskAssignment/v1",
                )],
                3,
                "rollout-assignment",
            )
            .await
            .expect("persist assignment");
        let lease_id = derive_workgraph_id(
            "lease",
            &[
                &test_task_id("I_task"),
                &derive_workgraph_id("assignment", &["assignment"]),
                "1",
            ],
        );
        let mut legacy = human_task_projection();
        legacy.dispatches = vec![WorkGraphDispatchBinding {
            dispatch_id: String::new(),
            ..human_dispatch_binding(&lease_id)
        }];
        *projector.lifecycle.lock().await = Some(legacy);
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![lifecycle_artifact(
                    DISPATCH_SOURCE,
                    "WorkGraphTaskDispatch/v1",
                )],
                4,
                "rollout-dispatch",
            )
            .await
            .expect("persist legacy dispatch");
        // The lease is dispatched but carries no Dispatch identity, so there
        // is no worker subject to bind a response to yet.
        assert!(state
            .allocator
            .workgraph_task_response_subject("I_task", HUMAN_DATABASE_ID)
            .await
            .expect("subject lookup")
            .is_none());

        // The projector rolls out canonical Dispatch identities. The already
        // dispatched lease must pick it up rather than stay empty forever.
        let mut upgraded = human_task_projection();
        upgraded.dispatches = vec![human_dispatch_binding(&lease_id)];
        *projector.lifecycle.lock().await = Some(upgraded);
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![lifecycle_artifact(
                    DISPATCH_SOURCE,
                    "WorkGraphTaskDispatch/v1",
                )],
                5,
                "rollout-dispatch-upgraded",
            )
            .await
            .expect("persist upgraded dispatch");
        assert_eq!(
            state
                .allocator
                .workgraph_task_response_subject("I_task", HUMAN_DATABASE_ID)
                .await
                .expect("subject lookup"),
            Some((
                HUMAN_ACTOR_ID.to_string(),
                crate::lease_ledger::TaskResponseSubject::Worker {
                    dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
                    lease_id: lease_id.clone(),
                }
            ))
        );

        // A projector rollback never erases what was already observed, and
        // the backfilled identity survives a replayed projection.
        let mut rolled_back = human_task_projection();
        rolled_back.dispatches = vec![WorkGraphDispatchBinding {
            dispatch_id: String::new(),
            ..human_dispatch_binding(&lease_id)
        }];
        *projector.lifecycle.lock().await = Some(rolled_back);
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![lifecycle_artifact(
                    DISPATCH_SOURCE,
                    "WorkGraphTaskDispatch/v1",
                )],
                6,
                "rollout-dispatch-rollback",
            )
            .await
            .expect("persist rolled back dispatch");
        assert_eq!(
            state
                .allocator
                .workgraph_task_response_subject("I_task", HUMAN_DATABASE_ID)
                .await
                .expect("subject lookup"),
            Some((
                HUMAN_ACTOR_ID.to_string(),
                crate::lease_ledger::TaskResponseSubject::Worker {
                    dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
                    lease_id,
                }
            ))
        );

        // And a response now binds to that backfilled subject.
        let inputs = try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_backfilled",
                "created",
                "@workgraph done",
                "2026-08-01T00:07:00Z",
            ),
        )
        .await
        .expect("normalize response")
        .expect("response inputs");
        assert_eq!(
            upserted_task_response(&inputs).dispatch_id.as_deref(),
            Some(derive_workgraph_id("dispatch", &["dispatch"]).as_str())
        );
    }

    #[tokio::test]
    async fn a_multiline_crlf_and_code_fenced_response_keeps_its_exact_bytes() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        seed_worker_lease(&state, projector.as_ref()).await;

        // Humans paste logs and diffs. CR and code fences are ordinary human
        // text, not a canonicalization failure: Core carries the exact bounded
        // bytes and lets the projector encode them.
        let body = "@workgraph here is the failure\r\n\r\n```console\r\n$ cargo test\r\n                    error: boom\r\n```\r\n\r\nRetrying now.";
        let inputs = try_workgraph_comment(
            &state,
            &task_response_event("IC_fenced", "created", body, "2026-08-01T00:05:00Z"),
        )
        .await
        .expect("normalize fenced response")
        .expect("fenced inputs");
        let document = upserted_task_response(&inputs);
        assert_eq!(document.body, body);
        assert!(document.body.contains('\r'));
        assert!(document.body.contains("```console"));
        assert_eq!(
            document.body_digest,
            crate::protocol::derive_workgraph_response_body_digest(body)
        );
        // The digest is over the raw bytes, so it differs from the same text
        // with CR stripped.
        assert_ne!(
            document.body_digest,
            crate::protocol::derive_workgraph_response_body_digest(&body.replace('\r', ""))
        );
    }

    const SECOND_HUMAN_ACTOR_ID: &str = "human-reviewer";
    const SECOND_HUMAN_DATABASE_ID: u64 = 5_150_001;
    const TWO_HUMAN_ACTOR_FILE: &str =
        "version: 2\nactors:\n- actorId: executor\n  kind: agent\n  slots: 1\n  \
         leaseDuration: PT15M\n- actorId: human-agentofreality\n  kind: human\n  slots: 1\n  \
         leaseDuration: PT8H\n  github:\n    databaseId: 4021243\n    \
         nodeId: MDQ6VXNlcjQwMjEyNDM=\n    login: agentofreality\n- actorId: human-reviewer\n  \
         kind: human\n  slots: 1\n  leaseDuration: PT8H\n  github:\n    databaseId: 5150001\n    \
         nodeId: MDQ6VXNlcjUxNTAwMDE=\n    login: reviewer\n";

    fn second_human_webhook_user() -> Value {
        json!({
            "id": SECOND_HUMAN_DATABASE_ID,
            "node_id": "MDQ6VXNlcjUxNTAwMDE=",
            "login": "reviewer",
            "type": "User"
        })
    }

    async fn seed_two_human_catalog(state: &IngressState) {
        let file = crate::agents::parse_agent_file(TWO_HUMAN_ACTOR_FILE).expect("actor catalog");
        state
            .allocator
            .sync_agents(
                &crate::agents::AgentFileLocation {
                    repository: "acme/widgets".to_string(),
                    r#ref: "main".to_string(),
                    path: ".github/workgraph/agents.yaml".to_string(),
                },
                &file,
                &crate::agents::AgentFileContent {
                    text: TWO_HUMAN_ACTOR_FILE.to_string(),
                    oid: "oid".to_string(),
                },
                1,
            )
            .await
            .expect("sync actor catalog");
    }

    /// Drives a human worker all the way to a Result awaiting Evaluation.
    async fn seed_human_result_awaiting_evaluation(
        state: &IngressState,
        projector: &RecordingProjector,
    ) -> String {
        let lease_id = seed_worker_lease(state, projector).await;
        let mut completed = human_task_projection();
        completed.dispatches = vec![human_dispatch_binding(&lease_id)];
        completed.results = vec![WorkGraphResultBinding {
            source_key: RESULT_SOURCE.to_string(),
            task_source_key: "I_task".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: derive_workgraph_id("workflow-run", &["run"]),
            task_id: test_task_id("I_task"),
            result_id: derive_workgraph_id("result", &["result"]),
            lease_id: lease_id.clone(),
            attempt: 1,
        }];
        *projector.lifecycle.lock().await = Some(completed);
        state
            .allocator
            .ingest_workgraph(
                projector,
                vec![lifecycle_artifact(RESULT_SOURCE, "WorkGraphTaskResult/v1")],
                5,
                "human-result",
            )
            .await
            .expect("persist result");
        lease_id
    }

    #[tokio::test]
    async fn a_worker_never_reviews_its_own_result_whatever_became_of_its_lease() {
        // Self review must stay refused for as long as the Result is
        // judgeable, so it cannot depend on the lease still being live.
        for (label, teardown) in [
            ("active", None),
            ("expired", Some("expire")),
            ("released", Some("release")),
            ("closed", Some("close")),
            ("restarted", Some("restart")),
        ] {
            let (_temp, projector, mut state, store, wal) =
                restartable_ingress_state(Some(task_trust())).await;
            seed_two_human_catalog(&state).await;
            seed_assigned_task(
                &state,
                projector.as_ref(),
                &[human_webhook_user(), second_human_webhook_user()],
            )
            .await;
            seed_human_result_awaiting_evaluation(&state, projector.as_ref()).await;

            match teardown {
                Some("expire") => {
                    // Every lease is long past its expiry.
                    state
                        .allocator
                        .expire(chrono::Utc::now() + chrono::Duration::days(3650), 6)
                        .await
                        .expect("expire leases");
                }
                Some("release") => {
                    // The lease is released out of the active set, and the
                    // pending Result is then recomputed from that released
                    // state: the producer must be resolved from the retained
                    // lease rather than from the active one.
                    state
                        .allocator
                        .expire(chrono::Utc::now() + chrono::Duration::days(3650), 6)
                        .await
                        .expect("release lease");
                    state
                        .allocator
                        .ingest_workgraph(
                            projector.as_ref(),
                            vec![lifecycle_artifact_at(
                                RESULT_SOURCE,
                                "WorkGraphTaskResult/v1",
                                2,
                            )],
                            7,
                            "recompute-after-release",
                        )
                        .await
                        .expect("recompute pending results");
                }
                Some("close") => {
                    let mut closed = payload(
                        "closed",
                        assigned_task_issue(
                            "I_task",
                            &[human_webhook_user(), second_human_webhook_user()],
                            "2026-08-01T00:09:00Z",
                        ),
                    );
                    closed["issue"]["state"] = json!("closed");
                    closed["issue"]["state_reason"] = json!("completed");
                    let inputs = try_workgraph_issue(&state, "close", &closed)
                        .await
                        .expect("normalize close")
                        .expect("close inputs");
                    state
                        .allocator
                        .ingest_workgraph(projector.as_ref(), inputs, 6, "close")
                        .await
                        .expect("persist close");
                }
                Some("restart") => {
                    // A fresh allocator over the same durable store, exactly
                    // as a process restart replays it.
                    state.allocator = Arc::new(Allocator::new(
                        "source".to_string(),
                        store.clone(),
                        wal.clone(),
                    ));
                }
                _ => {}
            }

            // The worker that produced the Result is never classified as its
            // evaluator, whatever became of the lease. While the lease is
            // live they are still the worker; once it is gone they have no
            // subject at all.
            assert!(
                !matches!(
                    state
                        .allocator
                        .workgraph_task_response_subject("I_task", HUMAN_DATABASE_ID)
                        .await
                        .expect("subject lookup"),
                    Some((
                        _,
                        crate::lease_ledger::TaskResponseSubject::Evaluator { .. }
                    ))
                ),
                "{label}: the producing worker must never be classified evaluator"
            );
            let producer_subject = state
                .allocator
                .workgraph_task_response_subject("I_task", HUMAN_DATABASE_ID)
                .await
                .expect("subject lookup");
            match label {
                // A live lease, including one replayed across a restart,
                // keeps its worker answering the Dispatch it holds.
                "active" | "restarted" => assert!(
                    matches!(
                        producer_subject,
                        Some((_, crate::lease_ledger::TaskResponseSubject::Worker { .. }))
                    ),
                    "{label}: a live lease keeps its worker subject"
                ),
                // Once the lease is gone the producer has no subject at all:
                // not a worker any more, and never this Result's evaluator.
                _ => assert!(
                    producer_subject.is_none(),
                    "{label}: a worker whose lease is gone has no subject left"
                ),
            }

            // A distinct assigned catalog human still evaluates it.
            assert_eq!(
                state
                    .allocator
                    .workgraph_task_response_subject("I_task", SECOND_HUMAN_DATABASE_ID)
                    .await
                    .expect("subject lookup"),
                Some((
                    SECOND_HUMAN_ACTOR_ID.to_string(),
                    crate::lease_ledger::TaskResponseSubject::Evaluator {
                        result_id: derive_workgraph_id("result", &["result"]),
                    }
                )),
                "{label}: a distinct human evaluator must still work"
            );
        }
    }

    #[tokio::test]
    async fn a_distinct_assigned_human_evaluates_a_human_workers_result() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_two_human_catalog(&state).await;
        seed_assigned_task(
            &state,
            projector.as_ref(),
            &[human_webhook_user(), second_human_webhook_user()],
        )
        .await;
        seed_human_result_awaiting_evaluation(&state, projector.as_ref()).await;
        state
            .allocator
            .expire(chrono::Utc::now() + chrono::Duration::days(3650), 6)
            .await
            .expect("expire leases");

        let mut event = task_response_event(
            "IC_review",
            "created",
            "@workgraph this meets the bar",
            "2026-08-01T00:10:00Z",
        );
        event["comment"]["user"] = second_human_webhook_user();
        event["sender"] = second_human_webhook_user();
        event["issue"]["assignees"] = json!([human_webhook_user(), second_human_webhook_user()]);
        let inputs = try_workgraph_comment(&state, &event)
            .await
            .expect("normalize evaluator response")
            .expect("evaluator inputs");
        let document = upserted_task_response(&inputs);
        assert_eq!(document.role, crate::protocol::TaskResponseRole::Evaluator);
        assert_eq!(document.actor_id, SECOND_HUMAN_ACTOR_ID);
        assert_eq!(
            document.result_id.as_deref(),
            Some(derive_workgraph_id("result", &["result"]).as_str())
        );
    }

    #[tokio::test]
    async fn a_response_without_an_open_lifecycle_subject_is_refused() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        // The human is a catalog actor and a current assignee, but the task
        // has neither an open Dispatch nor a Result awaiting Evaluation.
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        assert!(try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_early",
                "created",
                "@workgraph anything to do?",
                "2026-08-01T00:02:00Z"
            ),
        )
        .await
        .expect("normalize subjectless response")
        .is_none());
    }

    #[tokio::test]
    async fn a_worker_subject_belongs_only_to_the_executor_holding_the_lease() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        // The catalog gives the lease to an agent, and the human is only an
        // assignee, so the open worker subject is not the human's to answer.
        let catalog = "version: 2\nactors:\n- actorId: executor\n  kind: agent\n  slots: 1\n  \
                       leaseDuration: PT15M\n- actorId: human-agentofreality\n  kind: human\n  \
                       slots: 1\n  leaseDuration: PT8H\n  github:\n    databaseId: 4021243\n    \
                       nodeId: MDQ6VXNlcjQwMjEyNDM=\n    login: agentofreality\n";
        let file = crate::agents::parse_agent_file(catalog).expect("actor catalog");
        state
            .allocator
            .sync_agents(
                &crate::agents::AgentFileLocation {
                    repository: "acme/widgets".to_string(),
                    r#ref: "main".to_string(),
                    path: ".github/workgraph/agents.yaml".to_string(),
                },
                &file,
                &crate::agents::AgentFileContent {
                    text: catalog.to_string(),
                    oid: "oid".to_string(),
                },
                1,
            )
            .await
            .expect("sync catalog");
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;

        let mut agent_projection = human_task_projection();
        agent_projection.assignments[0].permitted_executors = vec!["executor".to_string()];
        *projector.lifecycle.lock().await = Some(agent_projection.clone());
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![lifecycle_artifact(
                    ASSIGNMENT_SOURCE,
                    "WorkGraphTaskAssignment/v1",
                )],
                3,
                "agent-assignment",
            )
            .await
            .expect("persist agent assignment");

        assert!(try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_wrong-worker",
                "created",
                "@workgraph done",
                "2026-08-01T00:02:00Z"
            ),
        )
        .await
        .expect("normalize foreign worker response")
        .is_none());
    }

    #[tokio::test]
    async fn a_marker_later_in_the_body_is_untrusted_text() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        seed_worker_lease(&state, projector.as_ref()).await;

        // The first non-whitespace line decides the path. A lifecycle marker
        // quoted further down is carried verbatim as untrusted human text and
        // never reinterpreted as a signed artifact.
        let body = "@workgraph here is what I saw\n\nWorkGraphTaskResult/v1\n\n```json\n{}\n```\n";
        let inputs = try_workgraph_comment(
            &state,
            &task_response_event("IC_quoting", "created", body, "2026-08-01T00:05:00Z"),
        )
        .await
        .expect("normalize quoting response")
        .expect("quoting inputs");
        let document = upserted_task_response(&inputs);
        assert_eq!(document.body, body);
        assert!(inputs
            .iter()
            .all(|input| !matches!(input, ProjectionInput::UpsertLifecycleArtifact(_))));
    }

    #[tokio::test]
    async fn any_sender_may_revoke_an_assignee_but_only_trust_may_grant_one() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        seed_worker_lease(&state, projector.as_ref()).await;
        // The human currently speaks for the task.
        assert!(try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_before",
                "created",
                "@workgraph on it",
                "2026-08-01T00:05:00Z"
            ),
        )
        .await
        .expect("normalize response")
        .is_some());

        // Revocation is signed GitHub state, so it lands whoever sent it: the
        // human themselves, or an untrusted third party.
        for (index, sender) in [
            human_webhook_user(),
            json!({"id": 77, "node_id": "U_stranger", "login": "stranger", "type": "User"}),
        ]
        .into_iter()
        .enumerate()
        {
            let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
            seed_human_actor_catalog(&state).await;
            seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
            seed_worker_lease(&state, projector.as_ref()).await;
            let mut event = payload(
                "unassigned",
                assigned_task_issue("I_task", &[], "2026-08-01T00:06:00Z"),
            );
            event["sender"] = sender;
            let inputs = try_workgraph_issue(&state, &format!("revoke-{index}"), &event)
                .await
                .expect("untrusted revocation is accepted")
                .expect("revocation inputs");
            assert!(upserted_task(&inputs).assignees.is_empty());
            // The revocation batch carries its own task upsert, so it is
            // ingested as-is.
            state
                .allocator
                .ingest_workgraph(projector.as_ref(), inputs, 6, "revoke")
                .await
                .expect("persist revocation");

            // Authority is gone immediately.
            assert!(try_workgraph_comment(
                &state,
                &task_response_event(
                    "IC_after",
                    "created",
                    "@workgraph still here?",
                    "2026-08-01T00:07:00Z"
                ),
            )
            .await
            .expect("normalize revoked response")
            .is_none());
        }
    }

    #[tokio::test]
    async fn an_untrusted_sender_can_only_shrink_the_assignee_set() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;

        let stranger =
            json!({"id": 77, "node_id": "U_stranger", "login": "stranger", "type": "User"});
        // An untrusted `assigned` never grants, whoever it names.
        let mut grant = payload(
            "assigned",
            assigned_task_issue(
                "I_task",
                &[human_webhook_user(), stranger.clone()],
                "2026-08-01T00:06:00Z",
            ),
        );
        grant["sender"] = stranger.clone();
        assert!(matches!(
            try_workgraph_issue(&state, "untrusted-grant", &grant).await,
            Err(WorkGraphNormError::Untrusted(_))
        ));

        // An untrusted `unassigned` carrying a smuggled addition applies only
        // the removals: the recorded set is intersected with what Core knew.
        let mut smuggled = payload(
            "unassigned",
            assigned_task_issue(
                "I_task",
                std::slice::from_ref(&stranger),
                "2026-08-01T00:07:00Z",
            ),
        );
        smuggled["sender"] = stranger;
        let inputs = try_workgraph_issue(&state, "smuggled", &smuggled)
            .await
            .expect("untrusted revocation is accepted")
            .expect("revocation inputs");
        assert!(upserted_task(&inputs).assignees.is_empty());

        // And it moves nothing else on the Issue.
        let mut retitled = payload(
            "unassigned",
            assigned_task_issue("I_task", &[], "2026-08-01T00:08:00Z"),
        );
        retitled["issue"]["body"] =
            json!("WorkGraphTask/v1\n\n```json\n{\"tampered\":true}\n```\n");
        retitled["sender"] =
            json!({"id": 77, "node_id": "U_stranger", "login": "stranger", "type": "User"});
        let inputs = try_workgraph_issue(&state, "retitled", &retitled)
            .await
            .expect("untrusted revocation is accepted")
            .expect("revocation inputs");
        assert!(!upserted_task(&inputs).body.contains("tampered"));
        assert!(inputs
            .iter()
            .all(|input| !matches!(input, ProjectionInput::UpsertLocator(_))));
    }

    #[tokio::test]
    async fn an_identical_redelivery_after_a_lifecycle_advance_is_a_no_op() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        let lease_id = seed_worker_lease(&state, projector.as_ref()).await;

        let created = task_response_event(
            "IC_response",
            "created",
            "@workgraph on it",
            "2026-08-01T00:05:00Z",
        );
        let inputs = try_workgraph_comment(&state, &created)
            .await
            .expect("normalize create")
            .expect("create inputs");
        assert_eq!(
            upserted_task_response(&inputs).role,
            crate::protocol::TaskResponseRole::Worker
        );
        persist_with_task(&state, projector.as_ref(), inputs, 6, "response-create").await;

        // The lifecycle advances underneath: the attempt produces a Result.
        let mut completed = human_task_projection();
        completed.dispatches = vec![human_dispatch_binding(&lease_id)];
        completed.results = vec![WorkGraphResultBinding {
            source_key: RESULT_SOURCE.to_string(),
            task_source_key: "I_task".to_string(),
            root_issue_id: "root".to_string(),
            workflow_run_id: derive_workgraph_id("workflow-run", &["run"]),
            task_id: test_task_id("I_task"),
            result_id: derive_workgraph_id("result", &["result"]),
            lease_id,
            attempt: 1,
        }];
        *projector.lifecycle.lock().await = Some(completed);
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![lifecycle_artifact(RESULT_SOURCE, "WorkGraphTaskResult/v1")],
                7,
                "advance",
            )
            .await
            .expect("persist result");

        // GitHub redelivers the identical comment. The subject has moved, but
        // the comment evidence has not, so this is a replay: nothing is
        // emitted and the recorded response keeps its original subject.
        assert!(try_workgraph_comment(&state, &created)
            .await
            .expect("normalize redelivery")
            .is_none());
    }

    #[tokio::test]
    async fn a_catalog_rename_never_wedges_an_active_lease_or_revokes_its_worker() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        let lease_id = seed_worker_lease(&state, projector.as_ref()).await;

        // The human renames and GitHub re-encodes their node ID. The catalog
        // is updated to match while their lease is still active.
        let renamed = "version: 2\nactors:\n- actorId: executor\n  kind: agent\n  slots: 1\n  \
                       leaseDuration: PT15M\n- actorId: human-agentofreality\n  kind: human\n  \
                       slots: 1\n  leaseDuration: PT8H\n  github:\n    databaseId: 4021243\n    \
                       nodeId: U_kgDOAD1Q2w\n    login: agent-of-reality\n";
        let file = crate::agents::parse_agent_file(renamed).expect("renamed catalog");
        state
            .allocator
            .sync_agents(
                &crate::agents::AgentFileLocation {
                    repository: "acme/widgets".to_string(),
                    r#ref: "main".to_string(),
                    path: ".github/workgraph/agents.yaml".to_string(),
                },
                &file,
                &crate::agents::AgentFileContent {
                    text: renamed.to_string(),
                    oid: "renamed".to_string(),
                },
                6,
            )
            .await
            .expect("catalog update must not wedge an active lease");

        // The in-flight lease keeps the metadata it was acquired with, and the
        // worker still speaks for it under the new login and node ID.
        assert_eq!(
            state
                .allocator
                .workgraph_task_response_subject("I_task", HUMAN_DATABASE_ID)
                .await
                .expect("subject lookup"),
            Some((
                HUMAN_ACTOR_ID.to_string(),
                crate::lease_ledger::TaskResponseSubject::Worker {
                    dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
                    lease_id,
                }
            ))
        );
        let mut event = task_response_event(
            "IC_renamed",
            "created",
            "@workgraph still me",
            "2026-08-01T00:07:00Z",
        );
        event["comment"]["user"]["login"] = json!("agent-of-reality");
        event["comment"]["user"]["node_id"] = json!("U_kgDOAD1Q2w");
        event["sender"] = event["comment"]["user"].clone();
        let inputs = try_workgraph_comment(&state, &event)
            .await
            .expect("normalize renamed worker response")
            .expect("renamed inputs");
        assert_eq!(upserted_task_response(&inputs).actor_id, HUMAN_ACTOR_ID);

        // A lease taken after the update carries the new catalog snapshot.
        let next_lease_id = derive_workgraph_id(
            "lease",
            &[
                &test_task_id("I_task"),
                &derive_workgraph_id("assignment", &["assignment"]),
                "2",
            ],
        );
        assert_ne!(next_lease_id, derive_workgraph_id("lease", &["seed"]));
    }

    #[tokio::test]
    async fn a_new_lease_after_a_catalog_update_uses_the_new_snapshot() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        // The catalog already carries the renamed identity when the lease is
        // acquired, so the snapshot is the new one.
        let renamed = "version: 2\nactors:\n- actorId: executor\n  kind: agent\n  slots: 1\n  \
                       leaseDuration: PT15M\n- actorId: human-agentofreality\n  kind: human\n  \
                       slots: 1\n  leaseDuration: PT8H\n  github:\n    databaseId: 4021243\n    \
                       nodeId: U_kgDOAD1Q2w\n    login: agent-of-reality\n";
        let file = crate::agents::parse_agent_file(renamed).expect("renamed catalog");
        state
            .allocator
            .sync_agents(
                &crate::agents::AgentFileLocation {
                    repository: "acme/widgets".to_string(),
                    r#ref: "main".to_string(),
                    path: ".github/workgraph/agents.yaml".to_string(),
                },
                &file,
                &crate::agents::AgentFileContent {
                    text: renamed.to_string(),
                    oid: "renamed".to_string(),
                },
                1,
            )
            .await
            .expect("sync renamed catalog");
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        let lease_id = seed_worker_lease(&state, projector.as_ref()).await;

        // The numeric ID is unchanged, so the same human still resolves.
        assert_eq!(
            state
                .allocator
                .workgraph_task_response_subject("I_task", HUMAN_DATABASE_ID)
                .await
                .expect("subject lookup"),
            Some((
                HUMAN_ACTOR_ID.to_string(),
                crate::lease_ledger::TaskResponseSubject::Worker {
                    dispatch_id: derive_workgraph_id("dispatch", &["dispatch"]),
                    lease_id,
                }
            ))
        );
    }

    #[tokio::test]
    async fn an_unassigned_human_loses_task_response_authority() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        // The catalog human exists but GitHub reports no assignee.
        seed_assigned_task(&state, projector.as_ref(), &[]).await;
        assert!(try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_unassigned",
                "created",
                "@workgraph done",
                "2026-08-01T00:02:00Z"
            ),
        )
        .await
        .expect("normalize unassigned response")
        .is_none());
    }

    #[tokio::test]
    async fn a_task_response_edit_delete_and_replay_are_revision_fenced() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        seed_worker_lease(&state, projector.as_ref()).await;

        let created = task_response_event(
            "IC_response",
            "created",
            "@workgraph first",
            "2026-08-01T00:01:00Z",
        );
        let inputs = try_workgraph_comment(&state, &created)
            .await
            .expect("normalize create")
            .expect("create inputs");
        persist_with_task(&state, projector.as_ref(), inputs, 3, "response-create").await;

        // Replaying the exact delivery emits nothing: the recorded response
        // is never rebound to whatever subject is open now.
        assert!(try_workgraph_comment(&state, &created)
            .await
            .expect("normalize replay")
            .is_none());

        // Genuine same-revision divergence is ambiguous, not invalid: GitHub
        // is asked again rather than the delivery being refused outright.
        let conflicting = task_response_event(
            "IC_response",
            "edited",
            "@workgraph second",
            "2026-08-01T00:01:00Z",
        );
        assert!(matches!(
            try_workgraph_comment(&state, &conflicting).await,
            Err(WorkGraphNormError::Unavailable(_))
        ));

        // A newer revision supersedes it.
        let edited = task_response_event(
            "IC_response",
            "edited",
            "@workgraph second",
            "2026-08-01T00:02:00Z",
        );
        let inputs = try_workgraph_comment(&state, &edited)
            .await
            .expect("normalize edit")
            .expect("edit inputs");
        assert_eq!(upserted_task_response(&inputs).body, "@workgraph second");
        persist_with_task(&state, projector.as_ref(), inputs, 4, "response-edit").await;

        // Editing away the mention retracts the response.
        let withdrawn = task_response_event(
            "IC_response",
            "edited",
            "never mind",
            "2026-08-01T00:03:00Z",
        );
        let inputs = try_workgraph_comment(&state, &withdrawn)
            .await
            .expect("normalize withdrawal")
            .expect("withdrawal inputs");
        assert!(matches!(
            inputs.as_slice(),
            [ProjectionInput::DeleteTaskResponse { source_key, task_id, actor_id, .. }]
                if source_key == "IC_response"
                    && task_id == &test_task_id("I_task")
                    && actor_id == HUMAN_ACTOR_ID
        ));
        persist_with_task(&state, projector.as_ref(), inputs, 5, "response-withdraw").await;

        // A stale delivery can never resurrect the retracted response.
        assert!(try_workgraph_comment(
            &state,
            &task_response_event(
                "IC_response",
                "edited",
                "@workgraph second",
                "2026-08-01T00:02:00Z"
            ),
        )
        .await
        .expect("normalize stale delivery")
        .is_none());

        // Deleting an already-retracted response emits nothing further.
        assert!(try_workgraph_comment(
            &state,
            &task_response_event("IC_response", "deleted", "", "2026-08-01T00:04:00Z"),
        )
        .await
        .expect("normalize delete")
        .is_none());
    }

    #[tokio::test]
    async fn a_lifecycle_artifact_on_a_task_is_never_read_as_a_natural_response() {
        let (_temp, projector, state) = ingress_state(Some(task_trust())).await;
        seed_human_actor_catalog(&state).await;
        seed_assigned_task(&state, projector.as_ref(), &[human_webhook_user()]).await;
        seed_worker_lease(&state, projector.as_ref()).await;
        // A body opening with a lifecycle marker takes the artifact path, so
        // trust is still decided by the reporter/assigner role, never by the
        // human catalog.
        let mut artifact = task_response_event(
            "IC_artifact",
            "created",
            "WorkGraphTaskResult/v1\n\n```json\n{}\n```\n",
            "2026-08-01T00:02:00Z",
        );
        artifact["comment"]["user"] = human_webhook_user();
        assert!(matches!(
            try_workgraph_comment(&state, &artifact).await,
            Err(WorkGraphNormError::Untrusted(_)) | Ok(None)
        ));
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
                        assignees: Vec::new(),
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
                    assignees: Vec::new(),
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

    // ── Multi-mapping Root admission ──────────────────────────────────

    fn multi_mapping_set() -> WorkflowMappingSet {
        WorkflowMappingSet::new(vec![
            legacy_mapping(),
            named_mapping(
                "foo",
                "workgraph:foo",
                ".github/workgraph/workflows/foo-v1.body",
            ),
            named_mapping(
                "bar",
                "workgraph:bar",
                ".github/workgraph/workflows/bar-v1.body",
            ),
        ])
    }

    fn upserted_root(inputs: &[ProjectionInput]) -> crate::protocol::RootIssueDocument {
        inputs
            .iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertRootIssue(document) => Some(document.clone()),
                _ => None,
            })
            .expect("an upserted Root Issue")
    }

    fn mapping_ids(document: &crate::protocol::RootIssueDocument) -> Vec<&str> {
        document
            .workflow_mappings
            .iter()
            .map(|mapping| mapping.mapping_id.as_str())
            .collect()
    }

    fn admission_of<'a>(
        document: &'a crate::protocol::RootIssueDocument,
        mapping_id: &str,
    ) -> &'a str {
        document
            .mapping_admission(mapping_id)
            .unwrap_or_else(|| panic!("mapping '{mapping_id}' must be active"))
            .admission_id
            .as_str()
    }

    fn content_of<'a>(
        document: &'a crate::protocol::RootIssueDocument,
        mapping_id: &str,
    ) -> (&'a str, &'a str) {
        let mapping = document
            .mapping_admission(mapping_id)
            .unwrap_or_else(|| panic!("mapping '{mapping_id}' must be active"));
        (&mapping.title, &mapping.body)
    }

    /// Runs one Issue delivery against the mapping-aware ingress and persists
    /// it, so the next delivery observes the durable previous document.
    async fn admit(
        state: &IngressState,
        projector: &RecordingProjector,
        delivery: &str,
        sequence: u64,
        event: serde_json::Value,
    ) -> Vec<ProjectionInput> {
        let inputs = try_workgraph_issue(state, delivery, &event)
            .await
            .expect("normalize delivery")
            .unwrap_or_default();
        state
            .allocator
            .ingest_workgraph(projector, inputs.clone(), sequence, delivery)
            .await
            .expect("persist delivery");
        inputs
    }

    fn labeled(labels: &[&str], selector: &str, updated_at: &str) -> serde_json::Value {
        let mut issue = root_issue(labels);
        issue["updated_at"] = json!(updated_at);
        let mut event = payload("labeled", issue);
        event["label"] = json!({ "name": selector });
        event
    }

    fn unlabeled(labels: &[&str], selector: &str, updated_at: &str) -> serde_json::Value {
        let mut issue = root_issue(labels);
        issue["updated_at"] = json!(updated_at);
        let mut event = payload("unlabeled", issue);
        event["label"] = json!({ "name": selector });
        event
    }

    #[tokio::test]
    async fn issue_opened_with_two_selector_labels_admits_both_mappings_distinctly() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let mut issue = root_issue(&["workgraph:foo", "workgraph:bar"]);
        issue["updated_at"] = json!("2026-08-01T00:00:00Z");
        let inputs = admit(
            &state,
            projector.as_ref(),
            "delivery-open",
            1,
            payload("opened", issue),
        )
        .await;
        let document = upserted_root(&inputs);
        assert_eq!(mapping_ids(&document), vec!["bar", "foo"]);
        let foo = admission_of(&document, "foo");
        let bar = admission_of(&document, "bar");
        assert_ne!(foo, bar, "one delivery derives distinct per-mapping IDs");
        // Definition locations are projected verbatim from configuration.
        let foo_mapping = document.mapping_admission("foo").expect("foo");
        assert_eq!(foo_mapping.label, "workgraph:foo");
        assert_eq!(foo_mapping.definition_repository, "acme/widgets");
        assert_eq!(foo_mapping.definition_ref, "main");
        assert_eq!(
            foo_mapping.definition_path,
            ".github/workgraph/workflows/foo-v1.body"
        );
        // Without the legacy mapping active, the compatibility admission ID is
        // the first ordered activation.
        assert_eq!(document.admission_id, bar);
    }

    #[tokio::test]
    async fn adding_a_second_selector_label_does_not_regenerate_the_first() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let first = admit(
            &state,
            projector.as_ref(),
            "delivery-foo",
            1,
            labeled(&["workgraph:foo"], "workgraph:foo", "2026-08-01T00:00:00Z"),
        )
        .await;
        let first = upserted_root(&first);
        assert_eq!(mapping_ids(&first), vec!["foo"]);
        let foo_admission = admission_of(&first, "foo").to_string();

        let second = admit(
            &state,
            projector.as_ref(),
            "delivery-bar",
            2,
            labeled(
                &["workgraph:foo", "workgraph:bar"],
                "workgraph:bar",
                "2026-08-01T00:01:00Z",
            ),
        )
        .await;
        let second = upserted_root(&second);
        assert_eq!(mapping_ids(&second), vec!["bar", "foo"]);
        assert_eq!(
            admission_of(&second, "foo"),
            foo_admission,
            "adding bar must not regenerate foo"
        );
        assert_ne!(admission_of(&second, "bar"), foo_admission);
    }

    #[tokio::test]
    async fn removing_one_mapping_retracts_only_that_admission() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let opened = admit(
            &state,
            projector.as_ref(),
            "delivery-open",
            1,
            labeled(
                &["workgraph:foo", "workgraph:bar"],
                "workgraph:foo",
                "2026-08-01T00:00:00Z",
            ),
        )
        .await;
        let opened = upserted_root(&opened);
        let bar_admission = admission_of(&opened, "bar").to_string();
        let foo_admission = admission_of(&opened, "foo").to_string();

        let removed = admit(
            &state,
            projector.as_ref(),
            "delivery-remove-foo",
            2,
            unlabeled(&["workgraph:bar"], "workgraph:foo", "2026-08-01T00:02:00Z"),
        )
        .await;
        assert!(
            !removed
                .iter()
                .any(|input| matches!(input, ProjectionInput::DeleteRootIssue { .. })),
            "one surviving mapping keeps the Root admitted"
        );
        let removed = upserted_root(&removed);
        assert_eq!(mapping_ids(&removed), vec!["bar"]);
        assert_eq!(admission_of(&removed, "bar"), bar_admission);
        assert_eq!(removed.admission_id, bar_admission);

        // Re-adding foo creates a fresh generation, never the retracted one.
        let readded = admit(
            &state,
            projector.as_ref(),
            "delivery-readd-foo",
            3,
            labeled(
                &["workgraph:foo", "workgraph:bar"],
                "workgraph:foo",
                "2026-08-01T00:03:00Z",
            ),
        )
        .await;
        let readded = upserted_root(&readded);
        assert_eq!(mapping_ids(&readded), vec!["bar", "foo"]);
        assert_ne!(admission_of(&readded, "foo"), foo_admission);
        assert_eq!(
            admission_of(&readded, "bar"),
            bar_admission,
            "re-adding foo must not regenerate bar"
        );
    }

    /// Adding a second selector label freezes current content for that new
    /// activation without mutating the existing mapping's generation or
    /// content.
    #[tokio::test]
    async fn adding_a_mapping_refreezes_content_and_keeps_the_active_generation() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let first = upserted_root(
            &admit(
                &state,
                projector.as_ref(),
                "delivery-foo",
                1,
                labeled(&["workgraph:foo"], "workgraph:foo", "2026-08-01T00:00:00Z"),
            )
            .await,
        );
        assert_eq!(first.title, "Root Issue");

        // An ordinary edit between activations never restates frozen content.
        let mut edited = root_issue(&["workgraph:foo"]);
        edited["title"] = json!("Retitled while foo is active");
        edited["updated_at"] = json!("2026-08-01T00:00:30Z");
        let edited = upserted_root(
            &admit(
                &state,
                projector.as_ref(),
                "delivery-edit",
                2,
                payload("edited", edited),
            )
            .await,
        );
        assert_eq!(edited.title, "Root Issue", "an edit never refreezes");
        assert_eq!(admission_of(&edited, "foo"), admission_of(&first, "foo"));

        let mut issue = root_issue(&["workgraph:foo", "workgraph:bar"]);
        issue["title"] = json!("Retitled while foo is active");
        issue["updated_at"] = json!("2026-08-01T00:01:00Z");
        let mut event = payload("labeled", issue);
        event["label"] = json!({ "name": "workgraph:bar" });
        let second =
            upserted_root(&admit(&state, projector.as_ref(), "delivery-bar", 3, event).await);
        assert_eq!(mapping_ids(&second), vec!["bar", "foo"]);
        assert_eq!(
            second.title, "Retitled while foo is active",
            "the compatibility fields follow the first ordered mapping"
        );
        assert_eq!(
            content_of(&second, "foo"),
            ("Root Issue", "Coordinate this work."),
            "adding bar must not mutate foo's frozen content"
        );
        assert_eq!(
            content_of(&second, "bar"),
            ("Retitled while foo is active", "Coordinate this work."),
            "bar freezes content from its own activation"
        );
        assert_eq!(
            admission_of(&second, "foo"),
            admission_of(&first, "foo"),
            "adding bar must not regenerate foo"
        );
    }

    #[tokio::test]
    async fn an_unobserved_readmission_regenerates_only_its_own_mapping() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let first = upserted_root(
            &admit(
                &state,
                projector.as_ref(),
                "delivery-open",
                1,
                labeled(
                    &["workgraph:foo", "workgraph:bar"],
                    "workgraph:foo",
                    "2026-08-01T00:00:00Z",
                ),
            )
            .await,
        );
        let bar_admission = admission_of(&first, "bar").to_string();

        // A `labeled` delivery for a label the cached document already records
        // can only be an unobserved remove/re-add round trip.
        let mut issue = root_issue(&["workgraph:foo", "workgraph:bar"]);
        issue["title"] = json!("Readmitted Root Issue");
        issue["updated_at"] = json!("2026-08-01T00:02:00Z");
        let mut event = payload("labeled", issue);
        event["label"] = json!({ "name": "workgraph:foo" });
        let second =
            upserted_root(&admit(&state, projector.as_ref(), "delivery-refoo", 2, event).await);
        assert_eq!(
            second.title, "Root Issue",
            "the compatibility fields continue to follow surviving bar"
        );
        assert_eq!(
            content_of(&second, "foo"),
            ("Readmitted Root Issue", "Coordinate this work.")
        );
        assert_eq!(
            content_of(&second, "bar"),
            ("Root Issue", "Coordinate this work.")
        );
        assert_ne!(admission_of(&second, "foo"), admission_of(&first, "foo"));
        assert_eq!(
            admission_of(&second, "bar"),
            bar_admission,
            "the unrelated mapping keeps its generation"
        );
    }

    /// Replays one selector remove/re-add twice: once with the `unlabeled`
    /// delivery observed, once with it lost. Both orders must converge on the
    /// same frozen title, frozen body, and admission IDs.
    async fn readmit_foo_with_surviving_bar(
        observe_removal: bool,
    ) -> crate::protocol::RootIssueDocument {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let mut opened = root_issue(&["workgraph:foo", "workgraph:bar"]);
        opened["title"] = json!("Original title");
        opened["body"] = json!("Original body");
        opened["updated_at"] = json!("2026-08-01T00:00:00Z");
        let mut event = payload("labeled", opened);
        event["label"] = json!({ "name": "workgraph:foo" });
        admit(&state, projector.as_ref(), "delivery-open", 1, event).await;

        if observe_removal {
            let mut removed = root_issue(&["workgraph:bar"]);
            removed["title"] = json!("Retitled while unlabeled");
            removed["body"] = json!("Rewritten while unlabeled");
            removed["updated_at"] = json!("2026-08-01T00:01:00Z");
            let mut event = payload("unlabeled", removed);
            event["label"] = json!({ "name": "workgraph:foo" });
            let removed = upserted_root(
                &admit(&state, projector.as_ref(), "delivery-remove", 2, event).await,
            );
            assert_eq!(mapping_ids(&removed), vec!["bar"]);
            assert_eq!(
                removed.title, "Original title",
                "retracting one mapping never refreezes the survivor's content"
            );
        }

        // The exact same re-add delivery in both orders.
        let mut readded = root_issue(&["workgraph:foo", "workgraph:bar"]);
        readded["title"] = json!("Readmitted title");
        readded["body"] = json!("Readmitted body");
        readded["updated_at"] = json!("2026-08-01T00:02:00Z");
        let mut event = payload("labeled", readded);
        event["label"] = json!({ "name": "workgraph:foo" });
        upserted_root(&admit(&state, projector.as_ref(), "delivery-readd", 3, event).await)
    }

    #[tokio::test]
    async fn observed_and_unobserved_readmission_converge_on_identical_content() {
        let observed = readmit_foo_with_surviving_bar(true).await;
        let unobserved = readmit_foo_with_surviving_bar(false).await;
        assert_eq!(
            observed, unobserved,
            "webhook delivery order must not change the admitted Root document"
        );
        assert_eq!(observed.title, "Original title");
        assert_eq!(observed.body, "Original body");
        assert_eq!(
            content_of(&observed, "foo"),
            ("Readmitted title", "Readmitted body")
        );
        assert_eq!(
            content_of(&observed, "bar"),
            ("Original title", "Original body")
        );
        assert_eq!(mapping_ids(&observed), vec!["bar", "foo"]);
        assert_eq!(
            admission_of(&observed, "foo"),
            admission_of(&unobserved, "foo")
        );
        assert_eq!(
            admission_of(&observed, "bar"),
            admission_of(&unobserved, "bar"),
            "the surviving sibling keeps one generation in both orders"
        );
    }

    /// The same convergence when the removal is a full retraction: the Root is
    /// deleted and then readmitted from scratch.
    #[tokio::test]
    async fn observed_and_unobserved_sole_mapping_readmission_converge() {
        async fn run(observe_removal: bool) -> crate::protocol::RootIssueDocument {
            let (_temp, projector, state) =
                ingress_state_with_mappings(None, multi_mapping_set()).await;
            let mut opened = root_issue(&["workgraph:foo"]);
            opened["title"] = json!("Original title");
            opened["updated_at"] = json!("2026-08-01T00:00:00Z");
            let mut event = payload("labeled", opened);
            event["label"] = json!({ "name": "workgraph:foo" });
            admit(&state, projector.as_ref(), "delivery-open", 1, event).await;
            if observe_removal {
                admit(
                    &state,
                    projector.as_ref(),
                    "delivery-remove",
                    2,
                    unlabeled(&[], "workgraph:foo", "2026-08-01T00:01:00Z"),
                )
                .await;
            }
            let mut readded = root_issue(&["workgraph:foo"]);
            readded["title"] = json!("Readmitted title");
            readded["updated_at"] = json!("2026-08-01T00:02:00Z");
            let mut event = payload("labeled", readded);
            event["label"] = json!({ "name": "workgraph:foo" });
            upserted_root(&admit(&state, projector.as_ref(), "delivery-readd", 3, event).await)
        }
        assert_eq!(run(true).await, run(false).await);
        assert_eq!(run(true).await.title, "Readmitted title");
    }

    #[tokio::test]
    async fn removing_the_last_mapping_retracts_the_whole_root() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        admit(
            &state,
            projector.as_ref(),
            "delivery-open",
            1,
            labeled(&["workgraph:foo"], "workgraph:foo", "2026-08-01T00:00:00Z"),
        )
        .await;
        let removed = admit(
            &state,
            projector.as_ref(),
            "delivery-remove",
            2,
            unlabeled(&[], "workgraph:foo", "2026-08-01T00:01:00Z"),
        )
        .await;
        assert!(removed
            .iter()
            .any(|input| matches!(input, ProjectionInput::DeleteRootIssue { source_key } if source_key == "I_root")));
        assert!(!removed
            .iter()
            .any(|input| matches!(input, ProjectionInput::UpsertRootIssue(_))));
    }

    #[tokio::test]
    async fn unknown_and_reserved_labels_are_observed_but_start_nothing() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let inputs = admit(
            &state,
            projector.as_ref(),
            "delivery-unknown",
            1,
            labeled(
                &["workgraph:unknown", "workgraph:ignore"],
                "workgraph:unknown",
                "2026-08-01T00:00:00Z",
            ),
        )
        .await;
        assert!(
            !inputs
                .iter()
                .any(|input| matches!(input, ProjectionInput::UpsertRootIssue(_))),
            "an unknown workgraph:* label never admits a Root"
        );
        let generic = inputs
            .iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertGitHubIssue(document) => Some(document),
                _ => None,
            })
            .expect("the Issue is still observed generically");
        assert_eq!(
            generic.workgraph_labels,
            vec![
                "workgraph:ignore".to_string(),
                "workgraph:unknown".to_string()
            ]
        );
        assert!(!generic.workgraph_include);
    }

    #[tokio::test]
    async fn exclusion_modifiers_never_activate_a_mapping_but_still_exclude() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let inputs = admit(
            &state,
            projector.as_ref(),
            "delivery-ignored",
            1,
            labeled(
                &["workgraph:foo", "workgraph:ignore"],
                "workgraph:foo",
                "2026-08-01T00:00:00Z",
            ),
        )
        .await;
        let document = upserted_root(&inputs);
        assert_eq!(
            mapping_ids(&document),
            vec!["foo"],
            "workgraph:ignore never becomes a mapping activation"
        );
        assert!(!document.workgraph_include);
        assert!(document
            .workgraph_labels
            .contains(&"workgraph:ignore".to_string()));
    }

    #[tokio::test]
    async fn two_mappings_may_share_one_definition_location() {
        let mappings = WorkflowMappingSet::new(vec![
            named_mapping("foo", "workgraph:foo", "shared/definition.body"),
            named_mapping("bar", "workgraph:bar", "shared/definition.body"),
        ]);
        let (_temp, projector, state) = ingress_state_with_mappings(None, mappings).await;
        let inputs = admit(
            &state,
            projector.as_ref(),
            "delivery-shared",
            1,
            labeled(
                &["workgraph:foo", "workgraph:bar"],
                "workgraph:foo",
                "2026-08-01T00:00:00Z",
            ),
        )
        .await;
        let document = upserted_root(&inputs);
        assert_eq!(mapping_ids(&document), vec!["bar", "foo"]);
        assert_ne!(
            admission_of(&document, "foo"),
            admission_of(&document, "bar")
        );
        assert_eq!(
            document.workflow_mappings[0].definition_path,
            document.workflow_mappings[1].definition_path
        );
    }

    #[tokio::test]
    async fn legacy_exact_workgraph_label_keeps_admitting_and_selects_the_legacy_admission() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let inputs = admit(
            &state,
            projector.as_ref(),
            "delivery-legacy",
            1,
            labeled(
                &["workgraph", "workgraph:foo"],
                "workgraph",
                "2026-08-01T00:00:00Z",
            ),
        )
        .await;
        let document = upserted_root(&inputs);
        assert_eq!(mapping_ids(&document), vec!["foo", "workgraph"]);
        assert_eq!(
            document.admission_id,
            admission_of(&document, LEGACY_WORKFLOW_MAPPING_ID),
            "the legacy activation is the deterministic compatibility admission"
        );
        // The exact `workgraph:` prefixed labels are still reported verbatim,
        // and the bare legacy label is not one of them.
        assert_eq!(document.workgraph_labels, vec!["workgraph:foo".to_string()]);
    }

    #[tokio::test]
    async fn source_never_reads_definition_contents_for_a_mapping() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        // No admission client is configured, so any attempt to reach GitHub for
        // definition content would fail the delivery instead of succeeding.
        assert!(state.admission_client.is_none());
        let inputs = admit(
            &state,
            projector.as_ref(),
            "delivery-no-fetch",
            1,
            labeled(&["workgraph:foo"], "workgraph:foo", "2026-08-01T00:00:00Z"),
        )
        .await;
        let document = upserted_root(&inputs);
        let mapping = document.mapping_admission("foo").expect("foo");
        assert_eq!(
            mapping.definition_path,
            ".github/workgraph/workflows/foo-v1.body"
        );
        // Only the location is carried; no definition body ever reaches Core.
        assert!(!document.body.contains("WorkGraphWorkflowDefinition"));
    }

    #[tokio::test]
    async fn a_replayed_delivery_reuses_every_mapping_generation() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let event = labeled(
            &["workgraph:foo", "workgraph:bar"],
            "workgraph:foo",
            "2026-08-01T00:00:00Z",
        );
        let first =
            upserted_root(&admit(&state, projector.as_ref(), "delivery-1", 1, event.clone()).await);
        let replay = try_workgraph_issue(&state, "delivery-1", &event)
            .await
            .expect("normalize replay")
            .unwrap_or_default();
        let replay = upserted_root(&replay);
        assert_eq!(replay, first, "an exact replay is byte-identical");
    }

    // ── Root comment admission sets ───────────────────────────────────────

    /// A human comment on a Root Issue carrying `labels`.
    fn multi_mapping_comment_event(comment_id: &str, labels: &[&str], body: &str) -> Value {
        let mut event =
            human_root_comment_event(comment_id, "created", body, "2026-08-01T00:05:00Z");
        event["issue"] = root_issue(labels);
        event["issue"]["updated_at"] = json!("2026-08-01T00:00:00Z");
        event
    }

    async fn upserted_comment(
        state: &IngressState,
        event: &Value,
    ) -> crate::protocol::RootIssueCommentDocument {
        try_workgraph_comment(state, event)
            .await
            .expect("normalize Root Issue comment")
            .expect("comment inputs")
            .into_iter()
            .find_map(|input| match input {
                ProjectionInput::UpsertRootIssueComment(document) => Some(document),
                _ => None,
            })
            .expect("an upserted Root Issue comment")
    }

    #[tokio::test]
    async fn a_root_comment_carries_every_active_mapping_admission() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let root = upserted_root(
            &admit(
                &state,
                projector.as_ref(),
                "delivery-open",
                1,
                labeled(
                    &["workgraph:foo", "workgraph:bar"],
                    "workgraph:foo",
                    "2026-08-01T00:00:00Z",
                ),
            )
            .await,
        );
        let bar = admission_of(&root, "bar").to_string();
        let foo = admission_of(&root, "foo").to_string();

        let comment = upserted_comment(
            &state,
            &multi_mapping_comment_event(
                "IC_human",
                &["workgraph:foo", "workgraph:bar"],
                "resume with option B",
            ),
        )
        .await;
        let mut expected = vec![bar.clone(), foo.clone()];
        expected.sort();
        assert_eq!(
            comment.admission_ids, expected,
            "the comment records every mapping admission active when it was written"
        );
        assert_eq!(
            comment.effective_admission_ids(),
            expected.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert_eq!(
            comment.admission_id, root.admission_id,
            "the compatibility admission stays the Root's deterministic selection"
        );
        assert!(comment.admission_ids.contains(&comment.admission_id));
    }

    #[tokio::test]
    async fn a_comment_written_under_two_mappings_survives_removing_one() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let root = upserted_root(
            &admit(
                &state,
                projector.as_ref(),
                "delivery-open",
                1,
                labeled(
                    &["workgraph:foo", "workgraph:bar"],
                    "workgraph:foo",
                    "2026-08-01T00:00:00Z",
                ),
            )
            .await,
        );
        let foo = admission_of(&root, "foo").to_string();
        let comment = upserted_comment(
            &state,
            &multi_mapping_comment_event(
                "IC_human",
                &["workgraph:foo", "workgraph:bar"],
                "resume with option B",
            ),
        )
        .await;

        let removed = upserted_root(
            &admit(
                &state,
                projector.as_ref(),
                "delivery-remove-bar",
                2,
                unlabeled(&["workgraph:foo"], "workgraph:bar", "2026-08-01T00:06:00Z"),
            )
            .await,
        );
        assert_eq!(mapping_ids(&removed), vec!["foo"]);
        assert_ne!(
            removed.admission_id, comment.admission_id,
            "the compatibility admission moved to the surviving mapping"
        );
        assert!(
            comment.effective_admission_ids().contains(&foo.as_str()),
            "the comment is still evidence for the mapping that survived"
        );
        // The stale generation is gone, so it can never match a new run.
        assert!(!removed
            .active_admission_ids()
            .contains(&comment.admission_id));
    }

    #[tokio::test]
    async fn a_comment_cannot_follow_a_mapping_into_a_fresh_generation() {
        let (_temp, projector, state) =
            ingress_state_with_mappings(None, multi_mapping_set()).await;
        let root = upserted_root(
            &admit(
                &state,
                projector.as_ref(),
                "delivery-open",
                1,
                labeled(
                    &["workgraph:foo", "workgraph:bar"],
                    "workgraph:foo",
                    "2026-08-01T00:00:00Z",
                ),
            )
            .await,
        );
        let comment = upserted_comment(
            &state,
            &multi_mapping_comment_event(
                "IC_human",
                &["workgraph:foo", "workgraph:bar"],
                "resume with option B",
            ),
        )
        .await;
        let stale_foo = admission_of(&root, "foo").to_string();

        // foo is removed and re-added while bar survives untouched.
        admit(
            &state,
            projector.as_ref(),
            "delivery-remove-foo",
            2,
            unlabeled(&["workgraph:bar"], "workgraph:foo", "2026-08-01T00:06:00Z"),
        )
        .await;
        let readded = upserted_root(
            &admit(
                &state,
                projector.as_ref(),
                "delivery-readd-foo",
                3,
                labeled(
                    &["workgraph:foo", "workgraph:bar"],
                    "workgraph:foo",
                    "2026-08-01T00:07:00Z",
                ),
            )
            .await,
        );
        let fresh_foo = admission_of(&readded, "foo").to_string();
        assert_ne!(fresh_foo, stale_foo);
        assert!(
            comment
                .effective_admission_ids()
                .contains(&stale_foo.as_str()),
            "the comment still names the generation it was written under"
        );
        assert!(
            !comment
                .effective_admission_ids()
                .contains(&fresh_foo.as_str()),
            "an old comment must never name a generation created after it"
        );
        assert!(
            comment
                .effective_admission_ids()
                .contains(&admission_of(&readded, "bar")),
            "the surviving sibling keeps the comment projected"
        );
    }

    #[test]
    fn a_legacy_comment_document_reports_its_single_admission() {
        let document = crate::protocol::RootIssueCommentDocument {
            source_key: "IC_legacy".to_string(),
            root_issue_id: "I_root".to_string(),
            admission_id: derive_workgraph_id("admission", &["I_root", "legacy"]),
            admission_ids: Vec::new(),
            repository_owner: "acme".to_string(),
            repository_name: "widgets".to_string(),
            repository_node_id: "R_widgets".to_string(),
            issue_number: 6,
            author_id: "U_human".to_string(),
            author_type: "User".to_string(),
            author_login: "octocat".to_string(),
            body: "resume".to_string(),
            created_at_revision: 1,
            updated_at_revision: 1,
        };
        assert_eq!(
            document.effective_admission_ids(),
            vec![document.admission_id.as_str()]
        );
        // A legacy row also round-trips through the wire schema unchanged.
        let encoded = serde_json::to_value(&document).expect("encode");
        assert_eq!(encoded["admissionIds"], json!([]));
        let decoded: crate::protocol::RootIssueCommentDocument = serde_json::from_value(json!({
            "sourceKey": "IC_legacy",
            "rootIssueId": "I_root",
            "admissionId": document.admission_id,
            "repositoryOwner": "acme",
            "repositoryName": "widgets",
            "repositoryNodeId": "R_widgets",
            "issueNumber": 6,
            "authorId": "U_human",
            "authorType": "User",
            "authorLogin": "octocat",
            "body": "resume",
            "createdAtRevision": 1,
            "updatedAtRevision": 1
        }))
        .expect("a document without admissionIds still decodes");
        assert_eq!(decoded, document);
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
                            },
                            "assignees": {
                                "nodes": [],
                                "pageInfo": {"hasNextPage": false}
                            }
                        }
                    }
                })))
                .expect(3)
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
                                },
                                "assignees": {
                                    "nodes": [],
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
                    &state.workflow_mappings,
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
                            },
                            "assignees": {
                                "nodes": [],
                                "pageInfo": {"hasNextPage": false}
                            }
                        }
                    }
                })))
                .expect(3)
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
            lifecycle: Arc::default(),
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
                    assignees: Vec::new(),
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
        let body = "WorkGraphTaskAssignment/v1\n\n```json\n{\"operationId\":\"op\"}\n```\n";
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
    async fn signed_error_artifact_requires_reporter_trust() {
        let body = "WorkGraphTaskError/v1\n\n```json\n{\"id\":\"error-1\"}\n```\n";
        let event = json!({
            "action": "created",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "issue": task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
            "comment": {
                "node_id": "IC_error",
                "body": body,
                "user": {"node_id": "U_report", "login": "reporter"},
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }
        });
        let encoded = serde_json::to_vec(&event).expect("encode Error webhook");

        let (_temp, untrusted_projector, untrusted) = ingress_state(None).await;
        assert!(matches!(
            handle_delivery(
                &untrusted,
                &signed_headers("issue_comment", "error-untrusted", &encoded),
                &encoded,
            )
            .await,
            Ok(None)
        ));
        assert!(untrusted_projector.committed.lock().await.is_empty());

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
        let (_temp, projector, trusted) = ingress_state(Some(trust)).await;
        assert!(matches!(
            handle_delivery(
                &trusted,
                &signed_headers("issue_comment", "error-trusted", &encoded),
                &encoded,
            )
            .await,
            Ok(Some(_))
        ));
        let committed = projector.committed.lock().await;
        assert!(matches!(
            committed.last().expect("Error commit").as_slice(),
            [ProjectionInput::UpsertLifecycleArtifact(document)]
                if document.source_key == "IC_error"
                    && document.task_source_key == "I_task"
                    && document.body == body
        ));
    }

    #[tokio::test]
    async fn signed_fork_and_join_require_assigner_trust() {
        for (marker, node_id, delivery_stub) in [
            ("WorkGraphTaskFork/v1\n", "IC_fork", "fork"),
            ("WorkGraphTaskJoin/v1\n", "IC_join", "join"),
        ] {
            let body = format!("{marker}\n```json\n{{\"id\":\"{delivery_stub}-1\"}}\n```\n");
            let event = json!({
                "action": "created",
                "organization": {"login": "acme"},
                "repository": {"name": "widgets", "full_name": "acme/widgets"},
                "issue": task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
                "comment": {
                    "node_id": node_id,
                    "body": body,
                    "user": {"node_id": "U_dispatch", "login": "dispatcher"},
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z"
                }
            });
            let encoded = serde_json::to_vec(&event).expect("encode Fork/Join webhook");

            // A reporter-only trust set (no assigners) must reject Fork/Join.
            let reporter_only = ProtocolTrust {
                task_creators: vec![TrustedIdentity {
                    id: "U_creator".to_string(),
                    login: "task-creator".to_string(),
                }],
                dispatchers: Vec::new(),
                reporters: vec![TrustedIdentity {
                    id: "U_dispatch".to_string(),
                    login: "dispatcher".to_string(),
                }],
            };
            let (_temp, untrusted_projector, untrusted) = ingress_state(Some(reporter_only)).await;
            assert!(matches!(
                handle_delivery(
                    &untrusted,
                    &signed_headers(
                        "issue_comment",
                        &format!("{delivery_stub}-untrusted"),
                        &encoded
                    ),
                    &encoded,
                )
                .await,
                Ok(None)
            ));
            assert!(untrusted_projector.committed.lock().await.is_empty());

            // The assigner trust role admits Fork/Join, exactly like Assignment.
            let assigner_trust = ProtocolTrust {
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
            let (_temp, projector, trusted) = ingress_state(Some(assigner_trust)).await;
            assert!(matches!(
                handle_delivery(
                    &trusted,
                    &signed_headers(
                        "issue_comment",
                        &format!("{delivery_stub}-trusted"),
                        &encoded
                    ),
                    &encoded,
                )
                .await,
                Ok(Some(_))
            ));
            let committed = projector.committed.lock().await;
            assert!(matches!(
                committed.last().expect("Fork/Join commit").as_slice(),
                [ProjectionInput::UpsertLifecycleArtifact(document)]
                    if document.source_key == node_id
                        && document.task_source_key == "I_task"
                        && document.body == body
            ));
        }
    }

    #[tokio::test]
    async fn untrusted_cross_role_edit_retracts_the_prior_lifecycle_artifact() {
        let assignment = "WorkGraphTaskAssignment/v1\n\n```json\n{\"id\":\"assignment-1\"}\n```\n";
        let error = "WorkGraphTaskError/v1\n\n```json\n{\"id\":\"error-1\"}\n```\n";
        let trust = ProtocolTrust {
            task_creators: vec![TrustedIdentity {
                id: "U_creator".to_string(),
                login: "task-creator".to_string(),
            }],
            dispatchers: vec![TrustedIdentity {
                id: "U_dispatch".to_string(),
                login: "dispatcher".to_string(),
            }],
            reporters: vec![TrustedIdentity {
                id: "U_report".to_string(),
                login: "reporter".to_string(),
            }],
        };
        let (_temp, projector, state) = ingress_state(Some(trust)).await;
        let created = json!({
            "action": "created",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "issue": task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
            "comment": {
                "node_id": "IC_cross_role",
                "body": assignment,
                "user": {"node_id": "U_dispatch", "login": "dispatcher"},
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:00:00Z"
            }
        });
        let encoded = serde_json::to_vec(&created).expect("encode Assignment webhook");
        assert!(matches!(
            handle_delivery(
                &state,
                &signed_headers("issue_comment", "cross-role-created", &encoded),
                &encoded,
            )
            .await,
            Ok(Some(_))
        ));

        let edited = json!({
            "action": "edited",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "issue": task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
            "comment": {
                "node_id": "IC_cross_role",
                "body": error,
                "user": {"node_id": "U_dispatch", "login": "dispatcher"},
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:01:00Z"
            },
            "sender": {"node_id": "U_dispatch", "login": "dispatcher"},
            "changes": {"body": {"from": assignment}}
        });
        let encoded = serde_json::to_vec(&edited).expect("encode Error edit webhook");
        assert!(matches!(
            handle_delivery(
                &state,
                &signed_headers("issue_comment", "cross-role-edited", &encoded),
                &encoded,
            )
            .await,
            Ok(Some(_))
        ));
        let committed = projector.committed.lock().await;
        assert!(matches!(
            committed.last().expect("edit commit").as_slice(),
            [ProjectionInput::DeleteLifecycleArtifact { source_key, .. }]
                if source_key == "IC_cross_role"
        ));
        drop(committed);

        let restored = json!({
            "action": "edited",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "issue": task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
            "comment": {
                "node_id": "IC_cross_role",
                "body": assignment,
                "user": {"node_id": "U_dispatch", "login": "dispatcher"},
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:02:00Z"
            },
            "sender": {"node_id": "U_dispatch", "login": "dispatcher"},
            "changes": {"body": {"from": error}}
        });
        let encoded = serde_json::to_vec(&restored).expect("encode restored Assignment");
        let restored_result = handle_delivery(
            &state,
            &signed_headers("issue_comment", "cross-role-restored", &encoded),
            &encoded,
        )
        .await
        .expect("restore Assignment");
        assert!(restored_result.is_some());
        let commit_count = projector.committed.lock().await.len();

        let delayed = json!({
            "action": "edited",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "issue": task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
            "comment": {
                "node_id": "IC_cross_role",
                "body": error,
                "user": {"node_id": "U_dispatch", "login": "dispatcher"},
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:01:30Z"
            },
            "sender": {"node_id": "U_dispatch", "login": "dispatcher"},
            "changes": {"body": {"from": assignment}}
        });
        let encoded = serde_json::to_vec(&delayed).expect("encode delayed Error edit");
        assert!(matches!(
            handle_delivery(
                &state,
                &signed_headers("issue_comment", "cross-role-delayed", &encoded),
                &encoded,
            )
            .await,
            Ok(None)
        ));
        assert_eq!(projector.committed.lock().await.len(), commit_count);
    }

    #[tokio::test]
    async fn untrusted_deletion_without_prior_artifact_does_not_create_a_tombstone() {
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
        let (_temp, projector, state) = ingress_state(Some(trust)).await;
        let event = json!({
            "action": "deleted",
            "organization": {"login": "acme"},
            "repository": {"name": "widgets", "full_name": "acme/widgets"},
            "issue": task_issue("I_task", "WorkGraphTask/v1\n\n```json\n{}\n```\n"),
            "comment": {
                "node_id": "IC_untrusted_deleted",
                "body": "WorkGraphTaskError/v1\n\n```json\n{\"id\":\"error-1\"}\n```\n",
                "user": {"node_id": "U_other", "login": "other"},
                "created_at": "2026-01-01T00:00:00Z",
                "updated_at": "2026-01-01T00:01:00Z"
            }
        });
        let encoded = serde_json::to_vec(&event).expect("encode untrusted deletion");
        assert!(matches!(
            handle_delivery(
                &state,
                &signed_headers("issue_comment", "untrusted-deleted", &encoded),
                &encoded,
            )
            .await,
            Ok(None)
        ));
        assert!(projector.committed.lock().await.is_empty());
        assert!(state
            .allocator
            .latest_workgraph_lifecycle_artifact_revision("IC_untrusted_deleted")
            .await
            .unwrap()
            .is_none());
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
        assert!(crate::protocol::is_typed_workgraph_id(
            &document.admission_id,
            "admission"
        ));
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
        let (_temp, projector, state) = ingress_state(None).await;
        let previous = "WorkGraphTaskAssignment/v1\n\n```json\n{\"operationId\":\"op\"}\n```\n";
        state
            .allocator
            .ingest_workgraph(
                projector.as_ref(),
                vec![ProjectionInput::UpsertLifecycleArtifact(
                    LifecycleArtifactDocument {
                        source_key: "IC_assign".to_string(),
                        task_source_key: "I_task".to_string(),
                        body: previous.to_string(),
                        created_at_revision: 1_767_225_600_000,
                        updated_at_revision: 1_767_225_600_000,
                    },
                )],
                1,
                "seed-lifecycle-artifact",
            )
            .await
            .expect("seed accepted lifecycle artifact");
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
                    "from": previous
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
                updated_at_revision: 1_767_225_660_000,
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
                    assignees: Vec::new(),
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
                        assignees: Vec::new(),
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
                assignees: Vec::new(),
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
                        assignees: Vec::new(),
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
                    assignees: Vec::new(),
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
                        assignees: Vec::new(),
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
                        },
                        "assignees": {
                            "nodes": [],
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
                        },
                        "assignees": {
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
                        },
                        "assignees": {
                            "nodes": [],
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
                        },
                        "assignees": {
                            "nodes": [],
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
                    assignees: Vec::new(),
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
            assignees: Vec::new(),
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
            &state.workflow_mappings,
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
            &state.workflow_mappings,
            false,
        )
        .expect("parent repository state")
        .fingerprint()
        .expect("parent repository fingerprint");
        assert_eq!(fingerprint, expected);
        assert_ne!(fingerprint, parent);
    }

    #[tokio::test]
    async fn matching_push_never_fetches_or_projects_a_definition() {
        // The Reaction owns the pinned workflow definition. A push that
        // touches the configured definition file is acknowledged with no
        // content: the Source has no definition fetch or projection path, and
        // no `IngressState` field that could reach GitHub for one.
        let (_temp, projector, state) = ingress_state(None).await;
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
            handle_push(&state, "push-1", &event)
                .await
                .expect("push is acknowledged"),
            None,
            "a definition-only push must be acknowledged with no content"
        );
        assert!(
            projector.committed.lock().await.is_empty(),
            "no definition may be projected"
        );
    }
}
