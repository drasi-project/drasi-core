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
use reqwest::header::{ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::WorkgraphRouterReactionConfig;

const PRE_FLIGHT_STATUS_QUERY: &str = r#"
query WorkgraphRouterPreflight($projectItemId: ID!) {
  node(id: $projectItemId) {
    ... on ProjectV2Item {
      id
      fieldValueByName(name: "Status") {
        ... on ProjectV2ItemFieldSingleSelectValue {
          name
        }
      }
    }
  }
}
"#;

const PROJECT_STATUS_MUTATION: &str = r#"
mutation WorkgraphRouterUpdateStatus($projectId: ID!, $projectItemId: ID!, $statusName: String!) {
  workgraphRouterUpdateStatus(projectId: $projectId, projectItemId: $projectItemId, statusName: $statusName) {
    projectItemId
    statusName
  }
}
"#;

#[derive(Debug, Clone)]
pub struct GithubClient {
    http: reqwest::Client,
    pub rest_url: String,
    pub graphql_url: String,
    pub token_env: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueComment {
    pub id: u64,
    pub body: String,
    pub author_login: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl IssueComment {
    pub fn is_unedited(&self) -> bool {
        match (&self.created_at, &self.updated_at) {
            (Some(created), Some(updated)) => created == updated,
            _ => true,
        }
    }
}

impl GithubClient {
    pub fn from_config(config: &WorkgraphRouterReactionConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("failed to build GitHub HTTP client")?;
        Ok(Self {
            http,
            rest_url: config.github_rest_url.trim_end_matches('/').to_string(),
            graphql_url: config.github_graphql_url.clone(),
            token_env: config.github_token_env.clone(),
        })
    }

    pub async fn issue_is_open(&self, repo: &str, issue_number: u64) -> anyhow::Result<bool> {
        let url = format!("{}/repos/{repo}/issues/{issue_number}", self.rest_url);
        let token = self.read_token()?;
        let response = self
            .http
            .get(url)
            .header(USER_AGENT, "drasi-workgraph-router")
            .header(ACCEPT, "application/vnd.github+json")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .send()
            .await
            .context("GitHub issue preflight request failed")?;
        if !response.status().is_success() {
            anyhow::bail!(
                "GitHub issue preflight failed with HTTP {}",
                response.status()
            );
        }
        let value: Value = response
            .json()
            .await
            .context("failed to parse GitHub issue preflight response")?;
        Ok(value
            .get("state")
            .and_then(Value::as_str)
            .is_some_and(|state| state.eq_ignore_ascii_case("open")))
    }

    pub async fn current_project_status(&self, project_item_id: &str) -> anyhow::Result<String> {
        let data = self
            .graphql(
                PRE_FLIGHT_STATUS_QUERY,
                serde_json::json!({ "projectItemId": project_item_id }),
            )
            .await?;
        data.pointer("/node/fieldValueByName/name")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| {
                anyhow::anyhow!("GraphQL preflight response missing project item status")
            })
    }

    pub async fn update_project_status(
        &self,
        project_id: &str,
        project_item_id: &str,
        status_name: &str,
    ) -> anyhow::Result<()> {
        let _ = self
            .graphql(
                PROJECT_STATUS_MUTATION,
                serde_json::json!({
                    "projectId": project_id,
                    "projectItemId": project_item_id,
                    "statusName": status_name
                }),
            )
            .await
            .with_context(|| {
                format!(
                    "failed to update project status for projectItemId='{project_item_id}' to '{status_name}'"
                )
            })?;
        Ok(())
    }

    pub async fn create_issue_comment(
        &self,
        repo: &str,
        issue_number: u64,
        body: &str,
    ) -> anyhow::Result<IssueComment> {
        let url = format!(
            "{}/repos/{repo}/issues/{issue_number}/comments",
            self.rest_url
        );
        let token = self.read_token()?;
        let response = self
            .http
            .post(url)
            .header(USER_AGENT, "drasi-workgraph-router")
            .header(ACCEPT, "application/vnd.github+json")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .json(&serde_json::json!({ "body": body }))
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
            response
                .json()
                .await
                .context("failed to parse create comment response body")?,
        )
    }

    pub async fn list_issue_comments(
        &self,
        repo: &str,
        issue_number: u64,
    ) -> anyhow::Result<Vec<IssueComment>> {
        let token = self.read_token()?;
        let mut page = 1_u32;
        let mut all = Vec::new();

        loop {
            let url = format!(
                "{}/repos/{repo}/issues/{issue_number}/comments?per_page=100&page={page}",
                self.rest_url
            );
            let response = self
                .http
                .get(url)
                .header(USER_AGENT, "drasi-workgraph-router")
                .header(ACCEPT, "application/vnd.github+json")
                .header(AUTHORIZATION, format!("Bearer {token}"))
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
                .context("failed to parse list comments response body")?;

            let count = values.len();
            for value in values {
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

    pub async fn graphql(&self, query: &str, variables: Value) -> anyhow::Result<Value> {
        let token = self.read_token()?;
        let response = self
            .http
            .post(&self.graphql_url)
            .header(USER_AGENT, "drasi-workgraph-router")
            .header(ACCEPT, "application/vnd.github+json")
            .header(AUTHORIZATION, format!("Bearer {token}"))
            .json(&serde_json::json!({
                "query": query,
                "variables": variables
            }))
            .send()
            .await
            .context("GitHub GraphQL request failed")?;

        let status = response.status();
        let body: Value = response
            .json()
            .await
            .context("failed to parse GitHub GraphQL response body")?;

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

    fn read_token(&self) -> anyhow::Result<String> {
        std::env::var(&self.token_env)
            .with_context(|| format!("environment variable '{}' is not set", self.token_env))
    }
}

fn parse_comment(value: Value) -> anyhow::Result<IssueComment> {
    let id = value
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("comment payload missing numeric id"))?;
    let body = value
        .get("body")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("comment payload missing body"))?
        .to_string();
    let author_login = value
        .pointer("/user/login")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("comment payload missing user.login"))?
        .to_string();
    let created_at = value
        .get("created_at")
        .and_then(Value::as_str)
        .map(ToString::to_string);
    let updated_at = value
        .get("updated_at")
        .and_then(Value::as_str)
        .map(ToString::to_string);

    Ok(IssueComment {
        id,
        body,
        author_login,
        created_at,
        updated_at,
    })
}
