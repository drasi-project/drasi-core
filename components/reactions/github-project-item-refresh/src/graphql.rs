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
use reqwest::{header::HeaderMap, Client};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;

use crate::models::FetchedProjectItemState;

const PROJECT_ITEM_STATUS_QUERY: &str = r#"
query ProjectItemStatus($projectItemNodeId: ID!) {
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
      fieldValueByName(name: "Status") {
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
    #[error("github graphql returned errors: {0}")]
    GraphqlErrors(String),
    #[error("project item '{project_item_node_id}' was not found")]
    MissingItem { project_item_node_id: String },
    #[error("node '{project_item_node_id}' had type '{actual}', expected 'ProjectV2Item'")]
    TypeMismatch {
        project_item_node_id: String,
        actual: String,
    },
    #[error("project item '{project_item_node_id}' is missing a required Status value")]
    MissingStatus { project_item_node_id: String },
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
}

impl GitHubGraphqlClient {
    pub fn new(
        client: Client,
        url: impl Into<String>,
        token: impl Into<String>,
        extra_headers: HashMap<String, String>,
    ) -> Self {
        Self {
            client,
            url: url.into(),
            token: token.into(),
            extra_headers,
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
        let raw_text = response
            .text()
            .await
            .map_err(|e| GraphqlFetchError::Transport(e.to_string()))?;

        if !status.is_success() {
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

        let status_value =
            node.field_value_by_name
                .ok_or_else(|| GraphqlFetchError::MissingStatus {
                    project_item_node_id: project_item_node_id.to_string(),
                })?;

        if status_value.typename != "ProjectV2ItemFieldSingleSelectValue" {
            return Err(GraphqlFetchError::MissingStatus {
                project_item_node_id: project_item_node_id.to_string(),
            });
        }

        let status_option_id =
            status_value
                .option_id
                .ok_or_else(|| GraphqlFetchError::MissingStatus {
                    project_item_node_id: project_item_node_id.to_string(),
                })?;
        let status_name = status_value
            .name
            .ok_or_else(|| GraphqlFetchError::MissingStatus {
                project_item_node_id: project_item_node_id.to_string(),
            })?;
        let status_field_node_id =
            status_value
                .field
                .and_then(|field| field.id)
                .ok_or_else(|| GraphqlFetchError::MissingStatus {
                    project_item_node_id: project_item_node_id.to_string(),
                })?;

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
