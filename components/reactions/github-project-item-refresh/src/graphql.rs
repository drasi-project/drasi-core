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

use anyhow::Context;
use chrono::{DateTime, Utc};
use reqwest::{header::HeaderMap, Client, StatusCode};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::models::FetchedProjectItemState;

const RATE_LIMIT_FALLBACK_BACKOFF: Duration = Duration::from_millis(100);
const MAX_RATE_LIMIT_WAIT: Duration = Duration::from_secs(120);

const PROJECT_ITEM_STATUS_QUERY: &str = r#"
query ProjectItemStatus($projectItemNodeId: ID!, $statusFieldName: String!) {
  node(id: $projectItemNodeId) {
    __typename
    ... on ProjectV2Item {
      id
      updatedAt
      project {
        id
      }
      content {
        __typename
        ... on Issue {
          id
        }
        ... on PullRequest {
          id
        }
        ... on DraftIssue {
          id
        }
      }
      fieldValueByName(name: $statusFieldName) {
        __typename
        ... on ProjectV2ItemFieldSingleSelectValue {
          name
          optionId
          field {
            ... on ProjectV2SingleSelectField {
              id
            }
          }
        }
      }
    }
  }
}
"#;

#[derive(Debug, thiserror::Error)]
pub enum GraphqlFetchError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("github graphql responded with HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("github graphql rate-limited with HTTP {status}, retrying in {retry_after:?}")]
    RateLimited { status: u16, retry_after: Duration },
    #[error("github graphql returned errors: {0}")]
    GraphqlErrors(String),
    #[error("project item '{project_item_node_id}' was not found")]
    MissingItem { project_item_node_id: String },
    #[error("node '{project_item_node_id}' had type '{actual}', expected 'ProjectV2Item'")]
    TypeMismatch {
        project_item_node_id: String,
        actual: String,
    },
    #[error(
        "project item '{project_item_node_id}' is missing a required '{status_field_name}' value"
    )]
    MissingStatus {
        project_item_node_id: String,
        status_field_name: String,
    },
    #[error("project item '{project_item_node_id}' is missing required field '{field_name}'")]
    MissingField {
        project_item_node_id: String,
        field_name: &'static str,
    },
}

impl GraphqlFetchError {
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::RateLimited { .. } => true,
            Self::HttpStatus { status, .. } => *status >= 500 || *status == 429,
            _ => false,
        }
    }
}

#[derive(Clone)]
pub struct GitHubGraphqlClient {
    client: Client,
    url: String,
    token: String,
    extra_headers: HashMap<String, String>,
    status_field_name: String,
}

impl GitHubGraphqlClient {
    pub fn new(
        client: Client,
        url: impl Into<String>,
        token: impl Into<String>,
        extra_headers: HashMap<String, String>,
        status_field_name: impl Into<String>,
    ) -> Self {
        Self {
            client,
            url: url.into(),
            token: token.into(),
            extra_headers,
            status_field_name: status_field_name.into(),
        }
    }

    pub async fn fetch_project_item_status(
        &self,
        project_item_node_id: &str,
        triggering_delivery_id: &str,
        refreshed_at: DateTime<Utc>,
    ) -> Result<FetchedProjectItemState, GraphqlFetchError> {
        let body = json!({
            "query": PROJECT_ITEM_STATUS_QUERY,
            "variables": {
                "projectItemNodeId": project_item_node_id,
                "statusFieldName": &self.status_field_name,
            }
        });

        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.token))
                .map_err(|e| GraphqlFetchError::Transport(e.to_string()))?,
        );
        headers.insert(
            reqwest::header::ACCEPT,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        headers.insert(
            "X-GitHub-Api-Version",
            reqwest::header::HeaderValue::from_static("2022-11-28"),
        );
        for (name, value) in &self.extra_headers {
            let header_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| GraphqlFetchError::Transport(e.to_string()))?;
            let header_value = reqwest::header::HeaderValue::from_str(value)
                .map_err(|e| GraphqlFetchError::Transport(e.to_string()))?;
            headers.insert(header_name, header_value);
        }

        let response = self
            .client
            .post(&self.url)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .map_err(|e| GraphqlFetchError::Transport(e.to_string()))?;

        let status = response.status();
        let response_headers = response.headers().clone();
        let raw_text = response
            .text()
            .await
            .map_err(|e| GraphqlFetchError::Transport(e.to_string()))?;

        if !status.is_success() {
            if let Some(retry_after) = rate_limit_retry_after(status, &response_headers) {
                return Err(GraphqlFetchError::RateLimited {
                    status: status.as_u16(),
                    retry_after,
                });
            }
            return Err(GraphqlFetchError::HttpStatus {
                status: status.as_u16(),
                body: truncate_for_error(&raw_text),
            });
        }

        let envelope: GraphqlEnvelope = serde_json::from_str(&raw_text)
            .with_context(|| "deserializing GitHub GraphQL response")
            .map_err(|e| GraphqlFetchError::Transport(e.to_string()))?;

        if let Some(errors) = envelope.errors {
            if !errors.is_empty() {
                let combined = errors
                    .iter()
                    .map(|e| e.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ");
                return Err(GraphqlFetchError::GraphqlErrors(combined));
            }
        }

        let data = envelope
            .data
            .ok_or_else(|| GraphqlFetchError::MissingItem {
                project_item_node_id: project_item_node_id.to_string(),
            })?;
        let node = data.node.ok_or_else(|| GraphqlFetchError::MissingItem {
            project_item_node_id: project_item_node_id.to_string(),
        })?;

        if node.typename != "ProjectV2Item" {
            return Err(GraphqlFetchError::TypeMismatch {
                project_item_node_id: project_item_node_id.to_string(),
                actual: node.typename,
            });
        }

        let item_id = node.id.ok_or_else(|| GraphqlFetchError::MissingField {
            project_item_node_id: project_item_node_id.to_string(),
            field_name: "id",
        })?;
        let updated_at = node
            .updated_at
            .ok_or_else(|| GraphqlFetchError::MissingField {
                project_item_node_id: project_item_node_id.to_string(),
                field_name: "updatedAt",
            })?;
        let project_node_id = node.project.and_then(|project| project.id).ok_or_else(|| {
            GraphqlFetchError::MissingField {
                project_item_node_id: project_item_node_id.to_string(),
                field_name: "project.id",
            }
        })?;

        let content_node_id = node.content.as_ref().and_then(|content| content.id.clone());
        let content_type = node
            .content
            .as_ref()
            .map(|content| content.typename.clone());

        let missing_status = || GraphqlFetchError::MissingStatus {
            project_item_node_id: project_item_node_id.to_string(),
            status_field_name: self.status_field_name.clone(),
        };
        let status_value = node.field_value_by_name.ok_or_else(&missing_status)?;

        if status_value.typename != "ProjectV2ItemFieldSingleSelectValue" {
            return Err(missing_status());
        }

        let status_option_id = status_value.option_id.ok_or_else(&missing_status)?;
        let status_name = status_value.name.ok_or_else(&missing_status)?;
        let status_field_node_id = status_value
            .field
            .and_then(|field| field.id)
            .ok_or_else(missing_status)?;

        Ok(FetchedProjectItemState {
            project_item_node_id: item_id,
            project_node_id,
            content_node_id,
            content_type,
            status_field_node_id,
            status_option_id,
            status_name,
            updated_at,
            refreshed_at,
            triggering_delivery_id: triggering_delivery_id.to_string(),
        })
    }
}

pub(crate) fn rate_limit_retry_after(status: StatusCode, headers: &HeaderMap) -> Option<Duration> {
    match status {
        StatusCode::FORBIDDEN if is_rate_limited_403(headers) => {
            Some(parse_rate_limit_delay(headers).unwrap_or(RATE_LIMIT_FALLBACK_BACKOFF))
        }
        StatusCode::TOO_MANY_REQUESTS => parse_rate_limit_delay(headers),
        _ => None,
    }
}

fn is_rate_limited_403(headers: &HeaderMap) -> bool {
    headers.contains_key(reqwest::header::RETRY_AFTER)
        || headers
            .get("x-ratelimit-remaining")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            == Some("0")
}

fn parse_rate_limit_delay(headers: &HeaderMap) -> Option<Duration> {
    parse_retry_after_delay(headers).or_else(|| parse_rate_limit_reset_delay(headers))
}

fn parse_retry_after_delay(headers: &HeaderMap) -> Option<Duration> {
    let retry_after = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = retry_after.parse::<u64>() {
        return Some(Duration::from_secs(seconds).min(MAX_RATE_LIMIT_WAIT));
    }

    let at = httpdate::parse_http_date(retry_after).ok()?;
    let delay = at
        .duration_since(SystemTime::now())
        .unwrap_or_else(|_| Duration::from_secs(0));
    Some(delay.min(MAX_RATE_LIMIT_WAIT))
}

fn parse_rate_limit_reset_delay(headers: &HeaderMap) -> Option<Duration> {
    let reset_epoch = headers
        .get("x-ratelimit-reset")?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()?;
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())?;
    let delay_secs = reset_epoch.saturating_sub(now_epoch);
    Some(Duration::from_secs(delay_secs).min(MAX_RATE_LIMIT_WAIT))
}

fn truncate_for_error(raw: &str) -> String {
    const MAX: usize = 512;
    if raw.chars().count() <= MAX {
        return raw.to_string();
    }
    let mut truncated = raw.chars().take(MAX).collect::<String>();
    truncated.push('…');
    truncated
}

#[derive(Debug, Deserialize)]
struct GraphqlEnvelope {
    #[serde(default)]
    data: Option<GraphqlData>,
    #[serde(default)]
    errors: Option<Vec<GraphqlErrorItem>>,
}

#[derive(Debug, Deserialize)]
struct GraphqlErrorItem {
    message: String,
}

#[derive(Debug, Deserialize)]
struct GraphqlData {
    #[serde(default)]
    node: Option<GraphqlNode>,
}

#[derive(Debug, Deserialize)]
struct GraphqlNode {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default, rename = "updatedAt")]
    updated_at: Option<DateTime<Utc>>,
    #[serde(default)]
    project: Option<GraphqlProject>,
    #[serde(default)]
    content: Option<GraphqlContent>,
    #[serde(default, rename = "fieldValueByName")]
    field_value_by_name: Option<GraphqlStatusValue>,
}

#[derive(Debug, Deserialize)]
struct GraphqlProject {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphqlContent {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(default)]
    id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GraphqlStatusValue {
    #[serde(rename = "__typename")]
    typename: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default, rename = "optionId")]
    option_id: Option<String>,
    #[serde(default)]
    field: Option<GraphqlStatusField>,
}

#[derive(Debug, Deserialize)]
struct GraphqlStatusField {
    #[serde(default)]
    id: Option<String>,
}
