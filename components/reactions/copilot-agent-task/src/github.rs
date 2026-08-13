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

//! GitHub REST/GraphQL client: preflight checks, Agent Task creation,
//! workgraph comment posting, and the ambiguous-creation reconciliation seam.
//!
//! # API-shape caveat
//!
//! The GitHub "Agent Tasks" API (`POST /agents/repos/{owner}/{repo}/tasks`)
//! referenced by this reaction's requirements is a preview/evolving surface.
//! This client implements the shape specified in the requirements
//! (`custom_agent`, `model`, `prompt`, `base_ref`, `create_pull_request`) and
//! makes the following documented adaptations where the exact wire contract
//! is not publicly pinned down:
//!
//! * **Listing endpoint** (`GET /agents/repos/{owner}/{repo}/tasks`) is
//!   assumed for the reconciliation seam. Field parsing is lenient (`id` as
//!   string or number; `url`/`html_url`; `prompt`/`body`) to tolerate minor
//!   shape differences.
//! * **"Clearly unsupported model" detection** on HTTP 422 checks the
//!   response body's `message` (or `error.code`/`error.message`) for
//!   model-related keywords. If GitHub's real error shape differs, adjust
//!   [`is_unsupported_model_error`] — the fallback-triggering contract
//!   (exactly one fallback attempt, only on this condition) is otherwise
//!   unaffected.
//! * **Ambiguous vs. permanent vs. transient classification** on
//!   `create_task`: a transport-level error (no HTTP response received at
//!   all — timeout, connection reset) is treated as **ambiguous** (the task
//!   may or may not have been created). Any *received* HTTP response is
//!   treated as authoritative: 201 is success, the specific 422 is the
//!   unsupported-model case, other 4xx are permanent failures, 5xx/429 are
//!   transient. Real-world edge cases (e.g. a proxy that completes the
//!   server-side request but drops the client's response) are not
//!   distinguishable from this client's vantage point; this is a documented
//!   simplification.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::redact::redact_authorization;

/// Static configuration needed to talk to one GitHub (or GHE) instance.
#[derive(Clone)]
pub struct GitHubConfig {
    pub api_base_url: String,
    pub graphql_url: String,
    pub agent_tasks_api_version: String,
    pub token: String,
    pub request_timeout_ms: u64,
}

impl std::fmt::Debug for GitHubConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubConfig")
            .field("api_base_url", &self.api_base_url)
            .field("graphql_url", &self.graphql_url)
            .field("agent_tasks_api_version", &self.agent_tasks_api_version)
            .field("token", &"[REDACTED]")
            .field("request_timeout_ms", &self.request_timeout_ms)
            .finish()
    }
}

/// A thin GitHub REST/GraphQL client. Never logs or `Debug`-prints the
/// token; see module docs and [`crate::redact`].
pub struct GitHubClient {
    http: reqwest::Client,
    config: GitHubConfig,
}

impl std::fmt::Debug for GitHubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubClient")
            .field("config", &self.config)
            .finish()
    }
}

impl GitHubClient {
    pub fn new(config: GitHubConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_millis(config.request_timeout_ms))
            .build()
            .context("failed to build HTTP client")?;
        Ok(Self { http, config })
    }

    fn auth_header_value(&self) -> String {
        format!("Bearer {}", self.config.token)
    }

    fn rest_headers(&self) -> Vec<(&'static str, String)> {
        vec![
            ("Authorization", self.auth_header_value()),
            ("Accept", "application/vnd.github+json".to_string()),
            (
                "X-GitHub-Api-Version",
                self.config.agent_tasks_api_version.clone(),
            ),
            (
                "User-Agent",
                "drasi-reaction-copilot-agent-task".to_string(),
            ),
        ]
    }

    pub async fn authenticated_user_id(&self) -> Result<String, ApiError> {
        let url = format!("{}/user", self.config.api_base_url);
        let mut request = self.http.get(&url);
        for (name, value) in self.rest_headers() {
            request = request.header(name, value);
        }
        let response = request.send().await.map_err(ApiError::from_transport)?;
        let status = response.status();
        let body_text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ApiError::from_status(status, &body_text));
        }
        let body: Value = serde_json::from_str(&body_text).map_err(|error| {
            ApiError::Permanent(format!("user response was not valid JSON: {error}"))
        })?;
        body["id"]
            .as_u64()
            .map(|id| id.to_string())
            .or_else(|| body["id"].as_str().map(ToString::to_string))
            .filter(|id| !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
            .ok_or_else(|| {
                ApiError::Permanent("user response did not contain a numeric id".to_string())
            })
    }

    // ---------------------------------------------------------------
    // Preflight: issue state
    // ---------------------------------------------------------------

    pub async fn get_issue(
        &self,
        owner: &str,
        repo: &str,
        number: u64,
    ) -> Result<IssueInfo, ApiError> {
        let url = format!(
            "{}/repos/{owner}/{repo}/issues/{number}",
            self.config.api_base_url
        );
        let mut req = self.http.get(&url);
        for (k, v) in self.rest_headers() {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(ApiError::from_transport)?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(ApiError::Permanent(format!(
                "issue #{number} not found in {owner}/{repo}"
            )));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::from_status(status, &body));
        }
        let body: Value = resp
            .json()
            .await
            .map_err(|e| ApiError::Permanent(format!("issue response was not valid JSON: {e}")))?;
        Ok(IssueInfo {
            state: body["state"].as_str().unwrap_or_default().to_string(),
            node_id: body["node_id"].as_str().map(|s| s.to_string()),
        })
    }

    // ---------------------------------------------------------------
    // Preflight: Project (v2) item status
    // ---------------------------------------------------------------

    /// Returns authoritative Project item, Project, and linked Issue state.
    pub async fn project_item_status(
        &self,
        project_item_node_id: &str,
        field_name: &str,
    ) -> Result<ProjectItemInfo, ApiError> {
        let query = r#"
            query($id: ID!, $field: String!) {
              node(id: $id) {
                ... on ProjectV2Item {
                  fieldValueByName(name: $field) {
                    ... on ProjectV2ItemFieldSingleSelectValue { name }
                  }
                  project {
                    id
                    number
                    owner {
                      ... on Organization { login }
                      ... on User { login }
                    }
                  }
                  content {
                    ... on Issue { id lastEditedAt createdAt }
                  }
                }
              }
            }
        "#;
        let variables = json!({ "id": project_item_node_id, "field": field_name });
        let data = self.graphql_query(query, variables).await?;
        let status = data["node"]["fieldValueByName"]["name"]
            .as_str()
            .map(|s| s.to_string());
        let linked_issue_id = data["node"]["content"]["id"]
            .as_str()
            .map(|s| s.to_string());
        let content_version = Self::content_version_from_timestamps(
            data["node"]["content"]["lastEditedAt"].as_str(),
            data["node"]["content"]["createdAt"].as_str(),
        )?;
        Ok(ProjectItemInfo {
            status,
            linked_issue_id,
            content_version,
            project_node_id: data["node"]["project"]["id"]
                .as_str()
                .map(ToString::to_string),
            project_owner: data["node"]["project"]["owner"]["login"]
                .as_str()
                .map(ToString::to_string),
            project_number: data["node"]["project"]["number"].as_u64(),
        })
    }

    fn content_version_from_timestamps(
        last_edited_at: Option<&str>,
        created_at: Option<&str>,
    ) -> Result<Option<String>, ApiError> {
        match last_edited_at.or(created_at) {
            Some(value) => Ok(Some(
                crate::row::LaunchRow::normalize_rfc3339_instant(value).map_err(|e| {
                    ApiError::Permanent(format!(
                        "issue content timestamp was not valid RFC 3339: {e:#}"
                    ))
                })?,
            )),
            None => Ok(None),
        }
    }

    // ---------------------------------------------------------------
    // Preflight: profile file blob SHA
    // ---------------------------------------------------------------

    pub async fn blob_sha_at_path(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>, ApiError> {
        let url = format!(
            "{}/repos/{owner}/{repo}/contents/{path}?ref={git_ref}",
            self.config.api_base_url,
            path = urlencode_path(path),
            git_ref = urlencode_component(git_ref),
        );
        let mut req = self.http.get(&url);
        for (k, v) in self.rest_headers() {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(ApiError::from_transport)?;
        let status = resp.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::from_status(status, &body));
        }
        let body: Value = resp.json().await.map_err(|e| {
            ApiError::Permanent(format!("contents response was not valid JSON: {e}"))
        })?;
        Ok(body["sha"].as_str().map(|s| s.to_string()))
    }

    // ---------------------------------------------------------------
    // Agent Task creation
    // ---------------------------------------------------------------

    pub async fn create_task(
        &self,
        owner: &str,
        repo: &str,
        request: &CreateTaskRequest,
    ) -> CreateTaskOutcome {
        let url = format!(
            "{}/agents/repos/{owner}/{repo}/tasks",
            self.config.api_base_url
        );
        let mut req = self.http.post(&url).json(request);
        for (k, v) in self.rest_headers() {
            req = req.header(k, v);
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                warn!(
                    "create_task transport error for {owner}/{repo} (model={}): {e} — outcome is ambiguous, task may or may not have been created",
                    request.model
                );
                return CreateTaskOutcome::Ambiguous;
            }
        };
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        let body_json: Value = serde_json::from_str(&body_text).unwrap_or(Value::Null);

        if status == reqwest::StatusCode::CREATED {
            let id = extract_id(&body_json);
            let url = body_json["html_url"]
                .as_str()
                .or_else(|| body_json["url"].as_str())
                .unwrap_or_default()
                .to_string();
            return match id {
                Some(id) => CreateTaskOutcome::Created { id, url },
                None => CreateTaskOutcome::Permanent(
                    "task creation returned 201 but no task id was present in the response"
                        .to_string(),
                ),
            };
        }

        if status == reqwest::StatusCode::UNPROCESSABLE_ENTITY
            && is_unsupported_model_error(&body_json, &body_text)
        {
            return CreateTaskOutcome::UnsupportedModel(model_error_message(
                &body_json, &body_text,
            ));
        }

        if status.is_client_error() {
            return CreateTaskOutcome::Permanent(format!(
                "create_task failed with {status}: {}",
                truncate(&body_text, 500)
            ));
        }

        CreateTaskOutcome::Transient(format!(
            "create_task failed with {status}: {}",
            truncate(&body_text, 500)
        ))
    }

    // ---------------------------------------------------------------
    // Reconciliation seam
    // ---------------------------------------------------------------

    pub async fn list_recent_tasks(
        &self,
        owner: &str,
        repo: &str,
    ) -> Result<Vec<TaskSummary>, ApiError> {
        let url = format!(
            "{}/agents/repos/{owner}/{repo}/tasks",
            self.config.api_base_url
        );
        let mut req = self.http.get(&url);
        for (k, v) in self.rest_headers() {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(ApiError::from_transport)?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(ApiError::from_status(status, &body));
        }
        let body: Value = resp.json().await.map_err(|e| {
            ApiError::Permanent(format!("task list response was not valid JSON: {e}"))
        })?;
        let items = body.as_array().cloned().unwrap_or_default();
        Ok(items
            .iter()
            .filter_map(|item| {
                let id = extract_id(item)?;
                let url = item["html_url"]
                    .as_str()
                    .or_else(|| item["url"].as_str())
                    .unwrap_or_default()
                    .to_string();
                let searchable = item["prompt"]
                    .as_str()
                    .or_else(|| item["body"].as_str())
                    .unwrap_or_default()
                    .to_string();
                Some(TaskSummary {
                    id,
                    url,
                    searchable,
                })
            })
            .collect())
    }

    /// Search recent tasks for exactly one whose prompt/body contains
    /// `execution_id`. Never guesses: any result other than exactly one match
    /// remains ambiguous and must not trigger another create request.
    pub async fn reconcile(
        &self,
        owner: &str,
        repo: &str,
        execution_id: &str,
    ) -> Result<ReconciliationOutcome, ApiError> {
        let tasks = self.list_recent_tasks(owner, repo).await?;
        let mut matches: Vec<TaskSummary> = tasks
            .into_iter()
            .filter(|t| t.searchable.contains(execution_id))
            .collect();
        Ok(match matches.len() {
            1 => ReconciliationOutcome::ExactMatch(matches.remove(0)),
            _ => ReconciliationOutcome::Ambiguous(matches),
        })
    }

    // ---------------------------------------------------------------
    // Workgraph execution comment (GraphQL addComment)
    // ---------------------------------------------------------------

    pub async fn post_issue_comment(
        &self,
        issue_node_id: &str,
        body: &str,
    ) -> Result<(), CommentError> {
        let mutation = r#"
            mutation($subjectId: ID!, $body: String!) {
              addComment(input: { subjectId: $subjectId, body: $body }) {
                commentEdge { node { id } }
              }
            }
        "#;
        let variables = json!({ "subjectId": issue_node_id, "body": body });
        match self.graphql_query(mutation, variables).await {
            Ok(_) => Ok(()),
            Err(ApiError::GraphQlErrors(errors)) => Err(CommentError::GraphQlErrors(errors)),
            Err(ApiError::Permanent(msg)) => Err(CommentError::Permanent(msg)),
            Err(ApiError::Transient(msg)) => Err(CommentError::Transient(msg)),
        }
    }

    /// Shared GraphQL POST helper. Per the reaction's contract, an HTTP 200
    /// response carrying a non-empty top-level `errors` array is treated as
    /// a **failure**, not a partial success — even though GraphQL's own spec
    /// allows `data` and `errors` to co-exist.
    async fn graphql_query(&self, query: &str, variables: Value) -> Result<Value, ApiError> {
        let mut req = self.http.post(&self.config.graphql_url).json(&json!({
            "query": query,
            "variables": variables,
        }));
        for (k, v) in self.rest_headers() {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(ApiError::from_transport)?;
        let status = resp.status();
        let body_text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(ApiError::from_status(status, &body_text));
        }
        let body: Value = serde_json::from_str(&body_text).map_err(|e| {
            ApiError::Permanent(format!("GraphQL response was not valid JSON: {e}"))
        })?;

        if let Some(errors) = body.get("errors").and_then(|e| e.as_array()) {
            if !errors.is_empty() {
                let messages: Vec<String> = errors
                    .iter()
                    .map(|e| e["message"].as_str().unwrap_or("<no message>").to_string())
                    .collect();
                return Err(ApiError::GraphQlErrors(messages));
            }
        }
        Ok(body.get("data").cloned().unwrap_or(Value::Null))
    }
}

#[derive(Debug, Clone)]
pub struct IssueInfo {
    pub state: String,
    pub node_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProjectItemInfo {
    pub status: Option<String>,
    pub linked_issue_id: Option<String>,
    pub content_version: Option<String>,
    pub project_node_id: Option<String>,
    pub project_owner: Option<String>,
    pub project_number: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct CreateTaskRequest {
    pub custom_agent: String,
    pub model: String,
    pub prompt: String,
    pub base_ref: String,
    pub create_pull_request: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CreateTaskOutcome {
    Created {
        id: String,
        url: String,
    },
    UnsupportedModel(String),
    /// Permanent (non-retryable) rejection — e.g. bad request, auth failure,
    /// unknown repository.
    Permanent(String),
    /// A definite HTTP error response was received (5xx/429): safe to retry
    /// the whole attempt later since the server told us it did not succeed.
    Transient(String),
    /// No HTTP response was received at all: unknown whether the task was
    /// created. Must go through reconciliation before any further action.
    Ambiguous,
}

#[derive(Debug, Clone)]
pub struct TaskSummary {
    pub id: String,
    pub url: String,
    pub searchable: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ReconciliationOutcome {
    ExactMatch(TaskSummary),
    Ambiguous(Vec<TaskSummary>),
}

impl PartialEq for TaskSummary {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.url == other.url && self.searchable == other.searchable
    }
}

#[derive(Debug)]
pub enum ApiError {
    Permanent(String),
    Transient(String),
    GraphQlErrors(Vec<String>),
}

impl ApiError {
    fn from_transport(e: reqwest::Error) -> Self {
        ApiError::Transient(format!("transport error: {e}"))
    }

    fn from_status(status: reqwest::StatusCode, body: &str) -> Self {
        let truncated = truncate(body, 500);
        if status.is_client_error() && status != reqwest::StatusCode::TOO_MANY_REQUESTS {
            ApiError::Permanent(format!("HTTP {status}: {truncated}"))
        } else {
            ApiError::Transient(format!("HTTP {status}: {truncated}"))
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Permanent(m) => write!(f, "permanent API error: {m}"),
            ApiError::Transient(m) => write!(f, "transient API error: {m}"),
            ApiError::GraphQlErrors(errs) => write!(f, "GraphQL errors: {}", errs.join("; ")),
        }
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug)]
pub enum CommentError {
    Permanent(String),
    Transient(String),
    GraphQlErrors(Vec<String>),
}

impl std::fmt::Display for CommentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommentError::Permanent(m) => write!(f, "permanent comment error: {m}"),
            CommentError::Transient(m) => write!(f, "transient comment error: {m}"),
            CommentError::GraphQlErrors(errs) => {
                write!(f, "GraphQL errors posting comment: {}", errs.join("; "))
            }
        }
    }
}

impl std::error::Error for CommentError {}

/// Extract an `id` field as a string regardless of whether it was encoded as
/// a JSON string or number.
fn extract_id(value: &Value) -> Option<String> {
    match value.get("id") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Detect a "clearly unsupported model" 422 response. See module docs for
/// the assumption this makes about the error body shape.
fn is_unsupported_model_error(body: &Value, raw: &str) -> bool {
    let candidates = [
        body["message"].as_str().unwrap_or_default(),
        body["error"]["message"].as_str().unwrap_or_default(),
        body["error"]["code"].as_str().unwrap_or_default(),
        raw,
    ];
    candidates.iter().any(|s| {
        let lower = s.to_lowercase();
        lower.contains("model")
            && (lower.contains("not supported")
                || lower.contains("unsupported")
                || lower.contains("invalid model")
                || lower.contains("unknown model"))
    })
}

fn model_error_message(body: &Value, raw: &str) -> String {
    body["message"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| truncate(raw, 300))
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let t: String = s.chars().take(max_chars).collect();
        format!("{t}…")
    }
}

fn urlencode_path(path: &str) -> String {
    path.split('/')
        .map(urlencode_component)
        .collect::<Vec<_>>()
        .join("/")
}

fn urlencode_component(s: &str) -> String {
    // Minimal percent-encoding sufficient for path segments and refs used in
    // GitHub REST URLs (avoids pulling in a URL-encoding crate for a handful
    // of characters).
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Log a debug line for an outgoing request without ever including the raw
/// `Authorization` header value.
#[allow(dead_code)]
fn debug_log_request(method: &str, url: &str, auth_header: &str) {
    debug!(
        "{method} {url} (Authorization: {})",
        redact_authorization(auth_header)
    );
}

/// Errors specific to preflight checks: permanent (fail-closed, skip) vs.
/// transient (retry the whole attempt after restart).
#[derive(Debug)]
pub enum PreflightError {
    Permanent(String),
    Transient(String),
}

impl std::fmt::Display for PreflightError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PreflightError::Permanent(m) => write!(f, "preflight failed (permanent): {m}"),
            PreflightError::Transient(m) => write!(f, "preflight failed (transient): {m}"),
        }
    }
}

impl std::error::Error for PreflightError {}

impl From<ApiError> for PreflightError {
    fn from(e: ApiError) -> Self {
        match e {
            ApiError::Permanent(m) => PreflightError::Permanent(m),
            ApiError::Transient(m) => PreflightError::Transient(m),
            ApiError::GraphQlErrors(errs) => PreflightError::Transient(errs.join("; ")),
        }
    }
}

/// Run all preflight checks required before a launch: issue open, issue
/// content version unchanged, Project status unchanged, and profile file
/// blob SHA pinned as expected.
pub async fn run_preflight(
    client: &GitHubClient,
    row: &crate::row::LaunchRow,
) -> Result<(), PreflightError> {
    let (owner, repo) = row
        .owner_and_repo()
        .map_err(|e| PreflightError::Permanent(e.to_string()))?;

    let issue = client.get_issue(owner, repo, row.issue_number).await?;
    if issue.state != "open" {
        return Err(PreflightError::Permanent(format!(
            "issue {}#{} is not open (state={})",
            row.repository, row.issue_number, issue.state
        )));
    }
    // Cross-check the row's issueNodeId against the issue GitHub actually
    // resolved for `repository`+`issueNumber`, so a row cannot point its
    // WorkGraph correlation IDs (and therefore the comment target) at an
    // issue node ID that doesn't correspond to the repository/number it
    // also claims.
    if issue.node_id.as_deref() != Some(row.issue_node_id.as_str()) {
        return Err(PreflightError::Permanent(format!(
            "issueNodeId '{}' does not match the node id GitHub returned for {}#{} ({:?})",
            row.issue_node_id, row.repository, row.issue_number, issue.node_id
        )));
    }
    let project_item = client
        .project_item_status(&row.project_item_node_id, "Status")
        .await?;
    if project_item.status.as_deref() != Some(row.expected_project_status.as_str()) {
        return Err(PreflightError::Permanent(format!(
            "project status changed: expected '{}', found {:?}",
            row.expected_project_status, project_item.status
        )));
    }
    if project_item.project_node_id.as_deref() != Some(row.project_node_id.as_str())
        || project_item.project_owner.as_deref() != Some(row.project_owner.as_str())
        || project_item.project_number != Some(row.project_number)
    {
        return Err(PreflightError::Permanent(format!(
            "project identity changed: expected node={} owner={} number={}, found node={:?} owner={:?} number={:?}",
            row.project_node_id,
            row.project_owner,
            row.project_number,
            project_item.project_node_id,
            project_item.project_owner,
            project_item.project_number
        )));
    }
    // Cross-check that the project item is actually linked to this issue —
    // otherwise `projectItemNodeId` could name an unrelated project item
    // that merely happens to have a matching `Status` value.
    if project_item.linked_issue_id.as_deref() != Some(row.issue_node_id.as_str()) {
        return Err(PreflightError::Permanent(format!(
            "projectItemNodeId '{}' is not linked to issue '{}' (linked to {:?})",
            row.project_item_node_id, row.issue_node_id, project_item.linked_issue_id
        )));
    }
    let expected_content_version =
        crate::row::LaunchRow::parse_rfc3339_instant(&row.issue_content_version)
            .map_err(|e| PreflightError::Permanent(e.to_string()))?;
    let live_content_version = project_item
        .content_version
        .as_deref()
        .ok_or_else(|| {
            PreflightError::Permanent(
                "project item linked issue did not return a content version".to_string(),
            )
        })
        .and_then(|value| {
            crate::row::LaunchRow::parse_rfc3339_instant(value)
                .map_err(|e| PreflightError::Permanent(e.to_string()))
        })?;
    if live_content_version != expected_content_version {
        return Err(PreflightError::Permanent(format!(
            "issue content version changed: expected {}, found {}",
            row.issue_content_version, live_content_version
        )));
    }

    let (path, expected_sha) = row
        .profile_path_and_sha()
        .map_err(|e| PreflightError::Permanent(e.to_string()))?;
    let live_sha = client
        .blob_sha_at_path(owner, repo, &path, &row.base_ref)
        .await?;
    if live_sha.as_deref() != Some(expected_sha) {
        return Err(PreflightError::Permanent(format!(
            "profile '{}' at {} does not match pinned blob SHA {expected_sha} (found {:?})",
            path, row.base_ref, live_sha
        )));
    }

    Ok(())
}

/// Timestamp helper kept here so `reaction.rs` doesn't need a direct chrono
/// import solely for this.
pub fn now() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_unsupported_model_message_field() {
        let body = json!({ "message": "The requested model is not supported for this operation." });
        assert!(is_unsupported_model_error(&body, ""));
    }

    #[test]
    fn detects_unsupported_model_error_code() {
        let body = json!({ "error": { "code": "unsupported_model" } });
        assert!(is_unsupported_model_error(&body, ""));
    }

    #[test]
    fn does_not_flag_unrelated_422() {
        let body = json!({ "message": "Validation failed: base_ref does not exist" });
        assert!(!is_unsupported_model_error(&body, ""));
    }

    #[test]
    fn extract_id_handles_string_and_number() {
        assert_eq!(extract_id(&json!({"id": "abc"})), Some("abc".to_string()));
        assert_eq!(extract_id(&json!({"id": 42})), Some("42".to_string()));
        assert_eq!(extract_id(&json!({})), None);
    }

    #[test]
    fn urlencode_component_escapes_special_chars() {
        assert_eq!(
            urlencode_component("feature/my branch"),
            "feature%2Fmy%20branch"
        );
        assert_eq!(urlencode_component("main"), "main");
    }

    #[test]
    fn from_status_classifies_client_vs_server_errors() {
        assert!(matches!(
            ApiError::from_status(reqwest::StatusCode::BAD_REQUEST, "bad"),
            ApiError::Permanent(_)
        ));
        assert!(matches!(
            ApiError::from_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR, "oops"),
            ApiError::Transient(_)
        ));
        assert!(matches!(
            ApiError::from_status(reqwest::StatusCode::TOO_MANY_REQUESTS, "slow down"),
            ApiError::Transient(_)
        ));
    }

    #[test]
    fn content_version_prefers_last_edited_at_and_normalizes_to_utc() {
        let version = GitHubClient::content_version_from_timestamps(
            Some("2026-08-13T12:00:00-07:00"),
            Some("2026-08-01T00:00:00Z"),
        )
        .unwrap();
        assert_eq!(version.as_deref(), Some("2026-08-13T19:00:00Z"));
    }

    #[test]
    fn content_version_falls_back_to_created_at() {
        let version =
            GitHubClient::content_version_from_timestamps(None, Some("2026-08-01T00:00:00Z"))
                .unwrap();
        assert_eq!(version.as_deref(), Some("2026-08-01T00:00:00Z"));
    }
}
