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

//! A deliberately narrow GitHub client.
//!
//! This client exposes exactly the operations the launch flow needs:
//!
//! | Operation | Kind | Purpose |
//! |---|---|---|
//! | `authenticated_user_id` | read | verify the token's identity at startup |
//! | `issue_snapshot` | read | authoritative issue state, node ID, and body |
//! | `project_snapshot` | read | project/item/issue binding and current status |
//! | `blob_sha_at_path` | read | pin the agent profile to an immutable blob |
//! | `list_issue_comments` | read | adopt the assignment / a prior ExecutionStarted |
//! | `create_issue_comment` | write | post one `ExecutionStarted` comment |
//! | `create_task` | write | create one Copilot coding-agent task |
//! | `list_recent_tasks` / `reconcile` | read | recover a task after an ambiguous write |
//!
//! # API-shape caveat
//!
//! The GitHub "Agent Tasks" API (`POST /agents/repos/{owner}/{repo}/tasks`) is
//! a preview/evolving surface. This client implements the requested shape
//! (`custom_agent`, `model`, `prompt`, `base_ref`, `create_pull_request`) and
//! documents these adaptations:
//!
//! * **Listing endpoint** (`GET /agents/repos/{owner}/{repo}/tasks`) is assumed
//!   for reconciliation; field parsing is lenient (`id` as string or number;
//!   `url`/`html_url`; `prompt`/`body`).
//! * **"Clearly unsupported model" detection** on HTTP 422 checks the response
//!   body for model-related keywords; see [`is_unsupported_model_error`].
//! * **Ambiguous vs. permanent vs. transient** on `create_task`: a
//!   transport-level error (no HTTP response — timeout, connection reset) is
//!   **ambiguous** (the task may or may not exist). Any received HTTP response
//!   is authoritative: 201 is success, the specific 422 is the
//!   unsupported-model case, other 4xx are permanent, 5xx/429 are transient.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use drasi_workgraph_common::trust::{
    author_identity_from_github_user, is_trusted, AuthorIdentity, TrustedAuthor,
};
use log::{debug, warn};
use serde::Serialize;
use serde_json::{json, Value};

use crate::redact::redact_authorization;

const USER_AGENT_VALUE: &str = "drasi-reaction-copilot-agent-task";

/// The Project single-select field this reaction reads the status from.
const STATUS_FIELD_NAME: &str = "Status";

const PROJECT_SNAPSHOT_QUERY: &str = r#"
query CopilotAgentTaskProjectSnapshot($projectId: ID!, $projectItemId: ID!, $statusFieldName: String!) {
  project: node(id: $projectId) {
    ... on ProjectV2 {
      id
      fields(first: 100) {
        nodes {
          ... on ProjectV2SingleSelectField {
            id
            name
          }
        }
      }
    }
  }
  item: node(id: $projectItemId) {
    ... on ProjectV2Item {
      id
      project {
        id
      }
      content {
        __typename
        ... on Issue {
          id
          number
          repository {
            nameWithOwner
          }
        }
      }
      fieldValueByName(name: $statusFieldName) {
        ... on ProjectV2ItemFieldSingleSelectValue {
          name
        }
      }
    }
  }
}
"#;

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

/// Authoritative issue state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueSnapshot {
    /// The issue node ID reported by GitHub.
    pub node_id: String,
    /// `open` / `closed`, lowercased by GitHub.
    pub state: String,
    /// The authoritative issue body; `None` when GitHub reports `null`.
    pub body: Option<String>,
}

/// One issue comment with the metadata that identity decisions rely on.
///
/// Authorship is keyed on the numeric database ID (`user.id`) and the actor
/// type (`user.type`) only. The node ID (`user.node_id`) is carried as audit
/// data, the login (`user.login`) is display-only, and no GitHub App ID is read
/// or required. See [`drasi_workgraph_common::trust`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueComment {
    /// The immutable comment node ID.
    pub node_id: String,
    /// The comment body.
    pub body: String,
    /// The author's authoritative identity, when GitHub reports both trust
    /// values; `authorId` and the login ride along as audit/display data.
    pub author: Option<AuthorIdentity>,
    /// Creation timestamp.
    pub created_at: Option<String>,
    /// Last update timestamp; differs from `created_at` after an edit.
    pub updated_at: Option<String>,
}

impl IssueComment {
    /// Whether GitHub reports the comment as never edited.
    pub fn is_unedited(&self) -> bool {
        match (&self.created_at, &self.updated_at) {
            (Some(created), Some(updated)) => created == updated,
            _ => false,
        }
    }

    /// Whether the author is the configured trusted author (numeric database
    /// ID + actor type).
    pub fn is_authored_by(&self, trusted: &TrustedAuthor) -> bool {
        is_trusted(self.author.as_ref(), trusted)
    }
}

/// Raw Project item + status snapshot. The reaction performs every binding and
/// status comparison, so a mismatch here is a permanent semantic rejection
/// rather than a transport failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectSnapshot {
    /// The project node ID the item actually belongs to.
    pub item_project_node_id: String,
    /// The item content `__typename` (expected `Issue`).
    pub content_type: Option<String>,
    /// The linked issue node ID.
    pub content_issue_node_id: Option<String>,
    /// The linked issue number.
    pub content_number: Option<u64>,
    /// The linked issue `owner/repo`.
    pub content_repository: Option<String>,
    /// The node ID of the project's single-select `Status` field.
    pub status_field_node_id: Option<String>,
    /// The item's current `Status` value.
    pub current_status: Option<String>,
}

/// A thin GitHub REST/GraphQL client. Never logs or `Debug`-prints the token;
/// see module docs and [`crate::redact`].
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
            ("User-Agent", USER_AGENT_VALUE.to_string()),
        ]
    }

    fn rest_get(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.get(url);
        for (name, value) in self.rest_headers() {
            req = req.header(name, value);
        }
        req
    }

    fn rest_post(&self, url: &str) -> reqwest::RequestBuilder {
        let mut req = self.http.post(url);
        for (name, value) in self.rest_headers() {
            req = req.header(name, value);
        }
        req
    }

    pub async fn authenticated_user_id(&self) -> Result<String, ApiError> {
        let url = format!("{}/user", self.config.api_base_url);
        let response = self
            .rest_get(&url)
            .send()
            .await
            .map_err(ApiError::from_transport)?;
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
    // Reads (authoritative GitHub state)
    // ---------------------------------------------------------------

    /// Read the authoritative issue snapshot (state, node ID, body).
    pub async fn issue_snapshot(&self, repository: &str, number: u64) -> Result<IssueSnapshot> {
        let url = format!(
            "{}/repos/{repository}/issues/{number}",
            self.config.api_base_url
        );
        let response = self
            .rest_get(&url)
            .send()
            .await
            .context("GitHub issue read failed")?;
        if !response.status().is_success() {
            anyhow::bail!("GitHub issue read failed with HTTP {}", response.status());
        }
        let value: Value = response
            .json()
            .await
            .context("failed to parse GitHub issue response")?;
        Ok(IssueSnapshot {
            node_id: value
                .get("node_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("issue response missing node_id"))?
                .to_string(),
            state: value
                .get("state")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("issue response missing state"))?
                .to_string(),
            body: value.get("body").and_then(|body| match body {
                Value::String(text) => Some(text.clone()),
                _ => None,
            }),
        })
    }

    /// Resolve the immutable blob SHA of a file at `git_ref`.
    ///
    /// Returns `Ok(None)` when GitHub reports the file does not exist (404) so
    /// the reaction can treat a missing profile as a permanent rejection rather
    /// than a transport failure.
    pub async fn blob_sha_at_path(
        &self,
        owner: &str,
        repo: &str,
        path: &str,
        git_ref: &str,
    ) -> Result<Option<String>> {
        let url = format!(
            "{}/repos/{owner}/{repo}/contents/{encoded_path}?ref={encoded_ref}",
            self.config.api_base_url,
            encoded_path = urlencode_path(path),
            encoded_ref = urlencode_component(git_ref),
        );
        let response = self
            .rest_get(&url)
            .send()
            .await
            .context("GitHub profile blob read failed")?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            anyhow::bail!("GitHub profile blob read failed with HTTP {status}");
        }
        let value: Value = response
            .json()
            .await
            .context("failed to parse GitHub contents response")?;
        Ok(value
            .get("sha")
            .and_then(Value::as_str)
            .map(ToString::to_string))
    }

    /// Read the raw Project item snapshot. Every binding/status comparison is
    /// performed by the reaction; this method only fails on genuine transport
    /// or GraphQL errors, or when the response is structurally unusable.
    pub async fn project_snapshot(
        &self,
        project_node_id: &str,
        project_item_node_id: &str,
    ) -> Result<ProjectSnapshot> {
        let data = self
            .graphql(
                PROJECT_SNAPSHOT_QUERY,
                json!({
                    "projectId": project_node_id,
                    "projectItemId": project_item_node_id,
                    "statusFieldName": STATUS_FIELD_NAME,
                }),
            )
            .await
            .context("failed to read project snapshot")?;

        let item_project_node_id = data
            .pointer("/item/project/id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("project snapshot missing item.project.id"))?
            .to_string();

        let status_field_node_id = data
            .pointer("/project/fields/nodes")
            .and_then(Value::as_array)
            .and_then(|fields| {
                fields
                    .iter()
                    .find(|field| {
                        field.get("name").and_then(Value::as_str) == Some(STATUS_FIELD_NAME)
                    })
                    .and_then(|field| field.get("id").and_then(Value::as_str))
                    .map(ToString::to_string)
            });

        Ok(ProjectSnapshot {
            item_project_node_id,
            content_type: data
                .pointer("/item/content/__typename")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            content_issue_node_id: data
                .pointer("/item/content/id")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            content_number: data.pointer("/item/content/number").and_then(Value::as_u64),
            content_repository: data
                .pointer("/item/content/repository/nameWithOwner")
                .and_then(Value::as_str)
                .map(ToString::to_string),
            status_field_node_id,
            current_status: data
                .pointer("/item/fieldValueByName/name")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        })
    }

    /// List every comment on an issue (paginated REST).
    pub async fn list_issue_comments(
        &self,
        repository: &str,
        number: u64,
    ) -> Result<Vec<IssueComment>> {
        let mut page = 1_u32;
        let mut all = Vec::new();
        loop {
            let url = format!(
                "{}/repos/{repository}/issues/{number}/comments?per_page=100&page={page}",
                self.config.api_base_url
            );
            let response = self
                .rest_get(&url)
                .send()
                .await
                .context("GitHub list comments request failed")?;
            if !response.status().is_success() {
                anyhow::bail!(
                    "GitHub list comments failed with HTTP {}",
                    response.status()
                );
            }
            let values: Vec<Value> = response
                .json()
                .await
                .context("failed to parse list comments response")?;
            let count = values.len();
            for value in &values {
                all.push(parse_comment(value)?);
            }
            if count < 100 {
                break;
            }
            page += 1;
            if page > 100 {
                anyhow::bail!("GitHub list comments pagination exceeded 100 pages");
            }
        }
        Ok(all)
    }

    // ---------------------------------------------------------------
    // Writes
    // ---------------------------------------------------------------

    /// Post one issue comment (REST).
    pub async fn create_issue_comment(
        &self,
        repository: &str,
        number: u64,
        body: &str,
    ) -> Result<IssueComment> {
        let url = format!(
            "{}/repos/{repository}/issues/{number}/comments",
            self.config.api_base_url
        );
        let response = self
            .rest_post(&url)
            .json(&json!({ "body": body }))
            .send()
            .await
            .context("GitHub create comment request failed")?;
        if !response.status().is_success() {
            anyhow::bail!(
                "GitHub create comment failed with HTTP {}",
                response.status()
            );
        }
        parse_comment(
            &response
                .json::<Value>()
                .await
                .context("failed to parse create comment response")?,
        )
    }

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
        let resp = match self.rest_post(&url).json(request).send().await {
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
        let resp = self
            .rest_get(&url)
            .send()
            .await
            .map_err(ApiError::from_transport)?;
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

    /// Shared GraphQL POST helper. Per the reaction's contract, an HTTP 200
    /// response carrying a non-empty top-level `errors` array is treated as a
    /// **failure**, not a partial success.
    async fn graphql(&self, query: &str, variables: Value) -> Result<Value> {
        let response = self
            .rest_post(&self.config.graphql_url)
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .context("GitHub GraphQL request failed")?;
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .context("failed to parse GitHub GraphQL response")?;
        if !status.is_success() {
            anyhow::bail!("GitHub GraphQL request failed with HTTP {status}");
        }
        if body
            .get("errors")
            .and_then(Value::as_array)
            .is_some_and(|errors| !errors.is_empty())
        {
            anyhow::bail!("GitHub GraphQL response contained errors");
        }
        body.get("data")
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("GitHub GraphQL response missing data"))
    }
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
    /// A definite HTTP error response was received (5xx/429): safe to retry the
    /// whole attempt later since the server told us it did not succeed.
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

/// Extract an `id` field as a string regardless of whether it was encoded as a
/// JSON string or number.
fn extract_id(value: &Value) -> Option<String> {
    match value.get("id") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Detect a "clearly unsupported model" 422 response. See module docs for the
/// assumption this makes about the error body shape.
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
    // GitHub REST URLs (avoids pulling in a URL-encoding crate for a handful of
    // characters).
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

fn parse_comment(value: &Value) -> Result<IssueComment> {
    Ok(IssueComment {
        node_id: value
            .get("node_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("comment payload missing node_id"))?
            .to_string(),
        body: value
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        author: author_identity_from_github_user(value.get("user")),
        created_at: value
            .get("created_at")
            .and_then(Value::as_str)
            .map(ToString::to_string),
        updated_at: value
            .get("updated_at")
            .and_then(Value::as_str)
            .map(ToString::to_string),
    })
}

/// Timestamp helper kept here so `reaction.rs` doesn't need a direct chrono
/// import solely for this.
pub fn now() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_workgraph_common::trust::ActorType;

    fn comment_json() -> Value {
        json!({
            "node_id": "IC_node",
            "body": "hello",
            "user": {
                "node_id": "U_kgDOBmvcSA",
                "id": 4021243,
                "type": "Bot",
                "login": "drasi-bot"
            },
            "created_at": "2026-08-14T00:00:00Z",
            "updated_at": "2026-08-14T00:00:00Z"
        })
    }

    fn trusted() -> TrustedAuthor {
        TrustedAuthor::new(4021243, ActorType::Bot)
    }

    #[test]
    fn parse_comment_extracts_author_and_timestamps() {
        let comment = parse_comment(&comment_json()).expect("parse");
        assert_eq!(comment.node_id, "IC_node");
        assert_eq!(
            comment.author,
            Some(
                AuthorIdentity::new(4021243, ActorType::Bot)
                    .with_author_id("U_kgDOBmvcSA")
                    .with_login("drasi-bot")
            )
        );
        assert!(comment.is_unedited());
        assert!(comment.is_authored_by(&trusted()));
        assert!(!comment.is_authored_by(&TrustedAuthor::new(1, ActorType::Bot)));
        assert!(!comment.is_authored_by(&TrustedAuthor::new(4021243, ActorType::User)));
    }

    #[test]
    fn a_renamed_login_does_not_change_trust() {
        let mut value = comment_json();
        value["user"]["login"] = json!("someone-else");
        assert!(parse_comment(&value)
            .expect("parse")
            .is_authored_by(&trusted()));
    }

    #[test]
    fn edited_comment_is_not_unedited() {
        let mut value = comment_json();
        value["updated_at"] = json!("2026-08-15T00:00:00Z");
        let comment = parse_comment(&value).expect("parse");
        assert!(!comment.is_unedited());
    }

    #[test]
    fn comment_without_both_trust_values_is_never_trusted() {
        for user in [
            Value::Null,
            json!({ "login": "drasi-bot" }),
            json!({ "node_id": "U_kgDOBmvcSA", "type": "Bot" }),
            json!({ "node_id": "U_kgDOBmvcSA", "id": 4021243 }),
            json!({ "node_id": "U_kgDOBmvcSA", "id": 4021243, "type": "Mannequin" }),
        ] {
            let mut value = comment_json();
            value["user"] = user.clone();
            let comment = parse_comment(&value).expect("parse");
            assert_eq!(comment.author, None, "unexpected identity for {user}");
            assert!(!comment.is_authored_by(&trusted()));
        }
    }

    #[test]
    fn a_missing_node_id_on_the_author_does_not_block_trust() {
        // The node ID is audit data: an author GitHub reports without one is
        // still fully identified by its database ID and actor type.
        let mut value = comment_json();
        value["user"]["node_id"] = Value::Null;
        let comment = parse_comment(&value).expect("parse");
        assert_eq!(
            comment.author,
            Some(AuthorIdentity::new(4021243, ActorType::Bot).with_login("drasi-bot"))
        );
        assert!(comment.is_authored_by(&trusted()));
    }

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
}
