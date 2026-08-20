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

//! The single worker-file fetch path, shared by the streaming Source and the
//! bootstrapper.
//!
//! It reuses the established GitHub GraphQL v4 host abstraction: the same
//! endpoint, the same bearer credential mechanism, and the same retry/backoff
//! conventions the bootstrapper already uses for Issues and Pull Requests. No
//! separate service, transport, or credential store is introduced.

use crate::workers::{error_code, WorkerFileContent, WorkerFileLocation, MAX_WORKER_FILE_BYTES};
use crate::workgraph::WorkGraphError;
use anyhow::{anyhow, bail, Context, Result};
use log::warn;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::sleep;

const MAX_ATTEMPTS: u32 = 4;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const WORKER_FILE_QUERY: &str = r#"
query($owner: String!, $name: String!, $expression: String!) {
  repository(owner: $owner, name: $name) {
    object(expression: $expression) {
      __typename
      ... on Blob {
        oid
        text
        byteSize
        isTruncated
        isBinary
      }
    }
  }
}
"#;

/// Why a worker file could not be turned into a validated worker set.
///
/// The two variants carry very different operational meaning and are handled
/// differently by callers:
///
/// * [`WorkerFileError::Unavailable`] is a transport, authentication, or
///   server-side failure. It is retryable and does not prove anything about
///   the configured file, so it becomes a component failure rather than a
///   graph assertion that no workers exist.
/// * [`WorkerFileError::Rejected`] is a deterministic, reproducible statement
///   about the configured location or its content — the blob is missing at the
///   exact repository/ref/path, is too large, is not text, or fails the strict
///   `version: 1` grammar. It becomes an explicit `WorkGraphError` node.
#[derive(Debug)]
pub enum WorkerFileError {
    Unavailable(anyhow::Error),
    Rejected(WorkGraphError),
}

impl WorkerFileError {
    fn rejected(code: &'static str, message: impl Into<String>) -> Self {
        Self::Rejected(WorkGraphError::new(code, message))
    }
}

impl std::fmt::Display for WorkerFileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(error) => write!(formatter, "worker file unavailable: {error:#}"),
            Self::Rejected(error) => {
                write!(
                    formatter,
                    "worker file rejected [{}]: {}",
                    error.code, error.message
                )
            }
        }
    }
}

/// Current comments of one Issue, with the author, editor, and timestamps the
/// lease-lifecycle trust rules need.
const TASK_COMMENTS_QUERY: &str = r#"
query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    issue(number: $number) {
      comments(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          node_id: id
          id: databaseId
          body
          created_at: createdAt
          updated_at: updatedAt
          last_edited_at: lastEditedAt
          html_url: url
          author_association: authorAssociation
          user: author { login type: __typename ... on Node { node_id: id } }
          editor { login ... on Node { node_id: id } }
        }
      }
    }
  }
}
"#;

/// A read-only GitHub GraphQL client scoped to fetching one worker file.
#[derive(Clone)]
pub struct WorkerFileClient {
    http: Client,
    api_url: String,
}

impl WorkerFileClient {
    pub fn new(token: &str, api_base_url: &str) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("bearer {token}"))
                .context("invalid GitHub token header value")?,
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("drasi-github-workgraph-worker-config"),
        );
        let http = Client::builder()
            .default_headers(headers)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to build GitHub worker file HTTP client")?;
        Ok(Self {
            http,
            api_url: api_base_url.to_string(),
        })
    }

    /// Fetch the exact blob at `location`.
    pub async fn fetch(
        &self,
        location: &WorkerFileLocation,
    ) -> Result<WorkerFileContent, WorkerFileError> {
        let variables = json!({
            "owner": location.owner(),
            "name": location.name(),
            "expression": location.expression(),
        });
        let data = self
            .execute(WORKER_FILE_QUERY, variables)
            .await
            .map_err(WorkerFileError::Unavailable)?;

        let missing = || {
            WorkerFileError::rejected(
                error_code::WORKER_FILE_UNAVAILABLE,
                format!(
                    "no worker file blob exists at '{}' ref '{}' path '{}'",
                    location.repository, location.r#ref, location.path
                ),
            )
        };
        let object = data
            .get("repository")
            .filter(|repository| !repository.is_null())
            .ok_or_else(missing)?
            .get("object")
            .filter(|object| !object.is_null())
            .ok_or_else(missing)?;
        if object.get("__typename").and_then(Value::as_str) != Some("Blob") {
            return Err(missing());
        }

        let byte_size = object.get("byteSize").and_then(Value::as_u64).unwrap_or(0);
        if byte_size > MAX_WORKER_FILE_BYTES {
            return Err(WorkerFileError::rejected(
                error_code::WORKER_FILE_TOO_LARGE,
                format!(
                    "the worker file is {byte_size} bytes, exceeding the {MAX_WORKER_FILE_BYTES} \
                     byte limit"
                ),
            ));
        }
        if object.get("isTruncated").and_then(Value::as_bool) == Some(true)
            || object.get("isBinary").and_then(Value::as_bool) == Some(true)
        {
            return Err(WorkerFileError::rejected(
                error_code::WORKER_FILE_TOO_LARGE,
                "the worker file must be complete UTF-8 text; GitHub reported it as truncated or \
                 binary",
            ));
        }
        let text = object
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                WorkerFileError::rejected(
                    error_code::WORKER_FILE_TOO_LARGE,
                    "the worker file has no readable UTF-8 text content",
                )
            })?
            .to_string();
        if text.len() as u64 > MAX_WORKER_FILE_BYTES {
            return Err(WorkerFileError::rejected(
                error_code::WORKER_FILE_TOO_LARGE,
                format!(
                    "the worker file is {} bytes, exceeding the {MAX_WORKER_FILE_BYTES} byte limit",
                    text.len()
                ),
            ));
        }
        let oid = object
            .get("oid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(WorkerFileContent { text, oid })
    }

    /// Fetch every current comment of one Issue, following each cursor.
    ///
    /// This is the reconciliation read: after a clean bootstrap the Source's
    /// ledger has never seen a task, so a lifecycle delivery for it is applied
    /// against GitHub's current comments rather than against an empty ledger.
    pub async fn fetch_task_comments(
        &self,
        owner: &str,
        name: &str,
        number: u64,
    ) -> Result<Vec<Value>> {
        let mut cursor: Option<String> = None;
        let mut all = Vec::new();
        loop {
            let data = self
                .execute(
                    TASK_COMMENTS_QUERY,
                    json!({
                        "owner": owner,
                        "name": name,
                        "number": number,
                        "cursor": cursor,
                    }),
                )
                .await?;
            let connection = data
                .get("repository")
                .filter(|value| !value.is_null())
                .and_then(|repository| repository.get("issue"))
                .filter(|value| !value.is_null())
                .and_then(|issue| issue.get("comments"))
                .filter(|value| !value.is_null())
                .ok_or_else(|| {
                    anyhow!("GitHub returned no comments for {owner}/{name}#{number}")
                })?;
            let nodes = connection
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("GitHub comment connection has no 'nodes'"))?;
            all.extend(nodes.iter().cloned());
            let page = connection
                .get("pageInfo")
                .ok_or_else(|| anyhow!("GitHub comment connection has no 'pageInfo'"))?;
            if page.get("hasNextPage").and_then(Value::as_bool) != Some(true) {
                break;
            }
            let next = page
                .get("endCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .ok_or_else(|| anyhow!("GitHub comment connection has a page but no cursor"))?;
            if cursor.as_deref() == Some(next) {
                bail!("GitHub comment connection returned a non-advancing cursor");
            }
            cursor = Some(next.to_string());
        }
        Ok(all)
    }

    /// Execute one GraphQL request, retrying transport errors, GitHub rate
    /// limiting, and 5xx responses with exponential backoff.
    async fn execute(&self, query: &str, variables: Value) -> Result<Value> {
        let body = json!({ "query": query, "variables": variables });
        let mut delay = INITIAL_RETRY_DELAY;
        for attempt in 1..=MAX_ATTEMPTS {
            let response = match self.http.post(&self.api_url).json(&body).send().await {
                Ok(response) => response,
                Err(error) if attempt < MAX_ATTEMPTS => {
                    warn!("GitHub worker file request failed ({error}); retrying in {delay:?}");
                    sleep(delay).await;
                    delay *= 2;
                    continue;
                }
                Err(error) => return Err(error).context("GitHub worker file request failed"),
            };
            let status = response.status();
            if status.as_u16() == 429 || status.is_server_error() {
                if attempt >= MAX_ATTEMPTS {
                    bail!("GitHub worker file API error after retries: {status}");
                }
                warn!("GitHub worker file API rate limited/server error ({status}); retrying");
                sleep(delay).await;
                delay *= 2;
                continue;
            }
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                bail!("GitHub worker file API request failed: {status}: {text}");
            }
            let payload: Value = response
                .json()
                .await
                .context("failed to decode GitHub worker file response as JSON")?;
            if let Some(errors) = payload.get("errors").and_then(Value::as_array) {
                if !errors.is_empty() {
                    bail!("GitHub worker file API returned errors: {errors:?}");
                }
            }
            return payload
                .get("data")
                .cloned()
                .ok_or_else(|| anyhow!("GitHub worker file response missing 'data'"));
        }
        unreachable!("loop always returns or bails");
    }
}
