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

//! The single agent-file fetch path used by the streaming Source.
//!
//! It uses the GitHub GraphQL v4 endpoint, bearer credential mechanism, and
//! bounded retry/backoff without introducing another service or credential store.

use crate::agents::{error_code, AgentFileContent, AgentFileLocation, MAX_AGENT_FILE_BYTES};
use crate::model::WorkGraphError;
use anyhow::{anyhow, bail, Context, Result};
use log::warn;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;
use tokio::time::sleep;

const MAX_ATTEMPTS: u32 = 4;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

const AGENT_FILE_QUERY: &str = r#"
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

/// Why an agent file could not be turned into a validated agent set.
///
/// The two variants carry very different operational meaning and are handled
/// differently by callers:
///
/// * [`AgentFileError::Unavailable`] is a transport, authentication, or
///   server-side failure. It is retryable and does not prove anything about
///   the configured file, so it becomes a component failure rather than a
///   graph assertion that no agents exist.
/// * [`AgentFileError::Rejected`] is a deterministic, reproducible statement
///   about the configured location or its content — the blob is missing at the
///   exact repository/ref/path, is too large, is not text, or fails the strict
///   `version: 1` grammar. It becomes an explicit `WorkGraphError` node.
#[derive(Debug)]
pub enum AgentFileError {
    Unavailable(anyhow::Error),
    Rejected(WorkGraphError),
}

impl AgentFileError {
    fn rejected(code: &'static str, message: impl Into<String>) -> Self {
        Self::Rejected(WorkGraphError::new(code, message))
    }
}

impl std::fmt::Display for AgentFileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unavailable(error) => write!(formatter, "agent file unavailable: {error:#}"),
            Self::Rejected(error) => {
                write!(
                    formatter,
                    "agent file rejected [{}]: {}",
                    error.code, error.message
                )
            }
        }
    }
}

/// A read-only GitHub GraphQL client scoped to fetching one agent file.
#[derive(Clone)]
pub struct AgentFileClient {
    http: Client,
    api_url: String,
}

impl AgentFileClient {
    pub fn new(token: &str, api_base_url: &str) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("bearer {token}"))
                .context("invalid GitHub token header value")?,
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("drasi-github-workgraph-agent-config"),
        );
        let http = Client::builder()
            .default_headers(headers)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .context("failed to build GitHub agent file HTTP client")?;
        Ok(Self {
            http,
            api_url: api_base_url.to_string(),
        })
    }

    /// Fetch the exact blob at `location`.
    pub async fn fetch(
        &self,
        location: &AgentFileLocation,
    ) -> Result<AgentFileContent, AgentFileError> {
        let variables = json!({
            "owner": location.owner(),
            "name": location.name(),
            "expression": location.expression(),
        });
        let data = self
            .execute(AGENT_FILE_QUERY, variables)
            .await
            .map_err(AgentFileError::Unavailable)?;

        let missing = || {
            AgentFileError::rejected(
                error_code::AGENT_FILE_UNAVAILABLE,
                format!(
                    "no agent file blob exists at '{}' ref '{}' path '{}'",
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
        if byte_size > MAX_AGENT_FILE_BYTES {
            return Err(AgentFileError::rejected(
                error_code::AGENT_FILE_TOO_LARGE,
                format!(
                    "the agent file is {byte_size} bytes, exceeding the {MAX_AGENT_FILE_BYTES} \
                     byte limit"
                ),
            ));
        }
        if object.get("isTruncated").and_then(Value::as_bool) == Some(true)
            || object.get("isBinary").and_then(Value::as_bool) == Some(true)
        {
            return Err(AgentFileError::rejected(
                error_code::AGENT_FILE_TOO_LARGE,
                "the agent file must be complete UTF-8 text; GitHub reported it as truncated or \
                 binary",
            ));
        }
        let text = object
            .get("text")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                AgentFileError::rejected(
                    error_code::AGENT_FILE_TOO_LARGE,
                    "the agent file has no readable UTF-8 text content",
                )
            })?
            .to_string();
        if text.len() as u64 > MAX_AGENT_FILE_BYTES {
            return Err(AgentFileError::rejected(
                error_code::AGENT_FILE_TOO_LARGE,
                format!(
                    "the agent file is {} bytes, exceeding the {MAX_AGENT_FILE_BYTES} byte limit",
                    text.len()
                ),
            ));
        }
        let oid = object
            .get("oid")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        Ok(AgentFileContent { text, oid })
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
                    warn!("GitHub agent file request failed ({error}); retrying in {delay:?}");
                    sleep(delay).await;
                    delay *= 2;
                    continue;
                }
                Err(error) => return Err(error).context("GitHub agent file request failed"),
            };
            let status = response.status();
            if status.as_u16() == 429 || status.is_server_error() {
                if attempt >= MAX_ATTEMPTS {
                    bail!("GitHub agent file API error after retries: {status}");
                }
                warn!("GitHub agent file API rate limited/server error ({status}); retrying");
                sleep(delay).await;
                delay *= 2;
                continue;
            }
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                bail!("GitHub agent file API request failed: {status}: {text}");
            }
            let payload: Value = response
                .json()
                .await
                .context("failed to decode GitHub agent file response as JSON")?;
            if let Some(errors) = payload.get("errors").and_then(Value::as_array) {
                if !errors.is_empty() {
                    bail!("GitHub agent file API returned errors: {errors:?}");
                }
            }
            return payload
                .get("data")
                .cloned()
                .ok_or_else(|| anyhow!("GitHub agent file response missing 'data'"));
        }
        unreachable!("loop always returns or bails");
    }
}
