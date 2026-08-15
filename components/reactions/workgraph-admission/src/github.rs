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
//! This client exposes exactly the operations admission needs:
//!
//! | Operation | Kind | Purpose |
//! |---|---|---|
//! | `issue_snapshot` | read | authoritative issue state, node ID, and body |
//! | `profile_blob_sha` | read | pin the agent profile to an immutable blob |
//! | `project_snapshot` | read | verify item/project/issue binding and status |
//! | `list_issue_comments` | read | adopt a comment written by an ambiguous retry |
//! | `create_issue_comment` | write | post one `WorkGraphEvent/v1` comment |
//! | `update_project_status` | write | set one single-select status option |
//!
//! There is no generic request method: GraphQL documents are compile-time
//! constants in this file, so configuration cannot introduce a new mutation.

use std::collections::HashMap;

use anyhow::Context;
use drasi_workgraph_common::trust::{
    author_identity_from_github_user, is_trusted, AuthorIdentity, TrustedAuthor,
};
use reqwest::header::{ACCEPT, USER_AGENT};
use serde_json::Value;

use crate::config::WorkgraphAdmissionReactionConfig;

const USER_AGENT_VALUE: &str = "drasi-workgraph-admission";

const PROJECT_SNAPSHOT_QUERY: &str = r#"
query WorkgraphAdmissionProjectSnapshot($projectId: ID!, $projectItemId: ID!, $statusFieldName: String!) {
  project: node(id: $projectId) {
    ... on ProjectV2 {
      id
      fields(first: 100) {
        nodes {
          ... on ProjectV2SingleSelectField {
            id
            name
            options {
              id
              name
            }
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
          optionId
        }
      }
    }
  }
}
"#;

const UPDATE_PROJECT_STATUS_MUTATION: &str = r#"
mutation WorkgraphAdmissionUpdateProjectStatus(
  $projectId: ID!
  $projectItemId: ID!
  $statusFieldId: ID!
  $statusOptionId: String!
) {
  updateProjectV2ItemFieldValue(input: {
    projectId: $projectId,
    itemId: $projectItemId,
    fieldId: $statusFieldId,
    value: { singleSelectOptionId: $statusOptionId }
  }) {
    projectV2Item {
      id
    }
  }
}
"#;

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

/// The Project item a read or mutation is bound to.
///
/// Bundling these five values keeps every binding check (item to project, item
/// to issue node, number, repository) impossible to pass partially or in the
/// wrong order at a call site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectItemRef<'a> {
    /// The Project (v2) node ID the item must belong to.
    pub project_node_id: &'a str,
    /// The Project (v2) item node ID.
    pub project_item_node_id: &'a str,
    /// The issue node ID the item must be linked to.
    pub subject_node_id: &'a str,
    /// `owner/repo` the linked issue must belong to.
    pub repository: &'a str,
    /// The issue number the item must carry.
    pub subject_number: u64,
}

/// Project item + status field snapshot.
#[derive(Debug, Clone)]
pub struct ProjectSnapshot {
    /// The item's current single-select status value.
    pub current_status: String,
    /// The status field node ID.
    pub status_field_id: String,
    /// Status option name to option ID.
    pub status_option_ids: HashMap<String, String>,
}

/// Result of a status mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateStatusOutcome {
    /// The mutation moved the item.
    Applied,
    /// The item was already at the destination status.
    AlreadyAtDestination,
}

/// The narrow GitHub client.
#[derive(Clone)]
pub struct GithubClient {
    http: reqwest::Client,
    rest_url: String,
    graphql_url: String,
    token_env: String,
    status_field_name: String,
    expected_status_field_node_id: String,
}

impl std::fmt::Debug for GithubClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GithubClient")
            .field("rest_url", &self.rest_url)
            .field("graphql_url", &self.graphql_url)
            .field("token_env", &self.token_env)
            .finish_non_exhaustive()
    }
}

impl GithubClient {
    /// Build a client from validated configuration.
    pub fn from_config(config: &WorkgraphAdmissionReactionConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(config.timeout_secs))
            .build()
            .context("failed to build GitHub HTTP client")?;
        Ok(Self {
            http,
            rest_url: config.github_rest_url.trim_end_matches('/').to_string(),
            graphql_url: config.github_graphql_url.clone(),
            token_env: config.github_token_env.clone(),
            status_field_name: config.project_status_field_name.clone(),
            expected_status_field_node_id: config.expected_project_status_field_node_id.clone(),
        })
    }

    fn token(&self) -> anyhow::Result<String> {
        std::env::var(&self.token_env)
            .with_context(|| format!("environment variable '{}' is not set", self.token_env))
    }

    /// Read the authoritative issue snapshot.
    pub async fn issue_snapshot(
        &self,
        repository: &str,
        number: u64,
    ) -> anyhow::Result<IssueSnapshot> {
        let url = format!("{}/repos/{repository}/issues/{number}", self.rest_url);
        let response = self
            .http
            .get(url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, "application/vnd.github+json")
            .bearer_auth(self.token()?)
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
                Value::Null => None,
                Value::String(text) => Some(text.clone()),
                _ => None,
            }),
        })
    }

    /// Resolve the immutable blob SHA of the agent profile at `git_ref`.
    pub async fn profile_blob_sha(
        &self,
        repository: &str,
        path: &str,
        git_ref: &str,
    ) -> anyhow::Result<String> {
        let url = format!("{}/repos/{repository}/contents/{path}", self.rest_url);
        let response = self
            .http
            .get(url)
            .query(&[("ref", git_ref)])
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, "application/vnd.github+json")
            .bearer_auth(self.token()?)
            .send()
            .await
            .context("GitHub profile blob read failed")?;
        if !response.status().is_success() {
            anyhow::bail!(
                "GitHub profile blob read failed with HTTP {}",
                response.status()
            );
        }
        let value: Value = response
            .json()
            .await
            .context("failed to parse GitHub contents response")?;
        value
            .get("sha")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .ok_or_else(|| anyhow::anyhow!("contents response for '{path}' missing sha"))
    }

    /// Read and bind the Project item snapshot.
    ///
    /// Fails unless the item belongs to `project_node_id`, its content is the
    /// expected issue (node ID, number, and repository all match), and the
    /// status field is the pinned `PVTSSF_...` node.
    pub async fn project_snapshot(
        &self,
        item: ProjectItemRef<'_>,
    ) -> anyhow::Result<ProjectSnapshot> {
        let ProjectItemRef {
            project_node_id,
            project_item_node_id,
            subject_node_id: expected_issue_node_id,
            repository: expected_repository,
            subject_number: expected_number,
        } = item;
        let data = self
            .graphql(
                PROJECT_SNAPSHOT_QUERY,
                serde_json::json!({
                    "projectId": project_node_id,
                    "projectItemId": project_item_node_id,
                    "statusFieldName": self.status_field_name,
                }),
            )
            .await
            .context("failed to read project snapshot")?;

        let item_project_id = data
            .pointer("/item/project/id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("project snapshot missing item.project.id"))?;
        if item_project_id != project_node_id {
            anyhow::bail!(
                "project item '{project_item_node_id}' belongs to project '{item_project_id}', not '{project_node_id}'"
            );
        }

        let content_type = data
            .pointer("/item/content/__typename")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("project item content type is missing"))?;
        if content_type != "Issue" {
            anyhow::bail!("project item content type '{content_type}' is not Issue");
        }
        let content_id = data
            .pointer("/item/content/id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("project item content id is missing"))?;
        if content_id != expected_issue_node_id {
            anyhow::bail!(
                "project item is linked to issue '{content_id}', not '{expected_issue_node_id}'"
            );
        }
        let content_number = data
            .pointer("/item/content/number")
            .and_then(Value::as_u64)
            .ok_or_else(|| anyhow::anyhow!("project item content number is missing"))?;
        if content_number != expected_number {
            anyhow::bail!(
                "project item issue number '{content_number}' does not match '{expected_number}'"
            );
        }
        let content_repo = data
            .pointer("/item/content/repository/nameWithOwner")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("project item repository is missing"))?;
        if content_repo != expected_repository {
            anyhow::bail!(
                "project item repository '{content_repo}' does not match '{expected_repository}'"
            );
        }

        let current_status = data
            .pointer("/item/fieldValueByName/name")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("project item has no status value"))?
            .to_string();

        let fields = data
            .pointer("/project/fields/nodes")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("project snapshot missing fields"))?;
        let status_field = fields
            .iter()
            .find(|field| {
                field.get("name").and_then(Value::as_str) == Some(self.status_field_name.as_str())
            })
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "project '{project_node_id}' is missing single-select field '{}'",
                    self.status_field_name
                )
            })?;
        let status_field_id = status_field
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("status field payload missing id"))?
            .to_string();
        if status_field_id != self.expected_status_field_node_id {
            anyhow::bail!(
                "project status field '{status_field_id}' does not match expected '{}'",
                self.expected_status_field_node_id
            );
        }
        let status_option_ids = status_field
            .get("options")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("status field payload missing options"))?
            .iter()
            .filter_map(|option| {
                Some((
                    option.get("name").and_then(Value::as_str)?.to_string(),
                    option.get("id").and_then(Value::as_str)?.to_string(),
                ))
            })
            .collect::<HashMap<_, _>>();
        if status_option_ids.is_empty() {
            anyhow::bail!(
                "status field '{}' has no configured options",
                self.status_field_name
            );
        }

        Ok(ProjectSnapshot {
            current_status,
            status_field_id,
            status_option_ids,
        })
    }

    /// Post one issue comment.
    pub async fn create_issue_comment(
        &self,
        repository: &str,
        number: u64,
        body: &str,
    ) -> anyhow::Result<IssueComment> {
        let url = format!(
            "{}/repos/{repository}/issues/{number}/comments",
            self.rest_url
        );
        let response = self
            .http
            .post(url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, "application/vnd.github+json")
            .bearer_auth(self.token()?)
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
            &response
                .json::<Value>()
                .await
                .context("failed to parse create comment response")?,
        )
    }

    /// List every comment on an issue.
    pub async fn list_issue_comments(
        &self,
        repository: &str,
        number: u64,
    ) -> anyhow::Result<Vec<IssueComment>> {
        let mut page = 1_u32;
        let mut all = Vec::new();
        loop {
            let url = format!(
                "{}/repos/{repository}/issues/{number}/comments?per_page=100&page={page}",
                self.rest_url
            );
            let response = self
                .http
                .get(url)
                .header(USER_AGENT, USER_AGENT_VALUE)
                .header(ACCEPT, "application/vnd.github+json")
                .bearer_auth(self.token()?)
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

    /// Move the Project item from `expected_source_status` to `target_status`.
    ///
    /// The mutation is skipped (and reported as
    /// [`UpdateStatusOutcome::AlreadyAtDestination`]) when the item is already
    /// at the destination, which is what makes a retry after an ambiguous write
    /// safe. Any other status is a stale row and fails.
    pub async fn update_project_status(
        &self,
        item: ProjectItemRef<'_>,
        expected_source_status: &str,
        target_status: &str,
    ) -> anyhow::Result<UpdateStatusOutcome> {
        let project_node_id = item.project_node_id;
        let project_item_node_id = item.project_item_node_id;
        let snapshot = self.project_snapshot(item).await?;

        if snapshot.current_status == target_status {
            return Ok(UpdateStatusOutcome::AlreadyAtDestination);
        }
        if snapshot.current_status != expected_source_status {
            anyhow::bail!(
                "project item '{project_item_node_id}' status is '{}' (expected '{expected_source_status}' or '{target_status}')",
                snapshot.current_status
            );
        }
        let option_id = snapshot
            .status_option_ids
            .get(target_status)
            .cloned()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "status field '{}' has no option named '{target_status}'",
                    self.status_field_name
                )
            })?;

        let data = self
            .graphql(
                UPDATE_PROJECT_STATUS_MUTATION,
                serde_json::json!({
                    "projectId": project_node_id,
                    "projectItemId": project_item_node_id,
                    "statusFieldId": snapshot.status_field_id,
                    "statusOptionId": option_id,
                }),
            )
            .await
            .with_context(|| {
                format!("failed to set '{project_item_node_id}' status to '{target_status}'")
            })?;
        let updated = data
            .pointer("/updateProjectV2ItemFieldValue/projectV2Item/id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("status mutation response missing project item id"))?;
        if updated != project_item_node_id {
            anyhow::bail!(
                "status mutation returned item '{updated}' instead of '{project_item_node_id}'"
            );
        }
        Ok(UpdateStatusOutcome::Applied)
    }

    async fn graphql(&self, query: &str, variables: Value) -> anyhow::Result<Value> {
        let response = self
            .http
            .post(&self.graphql_url)
            .header(USER_AGENT, USER_AGENT_VALUE)
            .header(ACCEPT, "application/vnd.github+json")
            .bearer_auth(self.token()?)
            .json(&serde_json::json!({ "query": query, "variables": variables }))
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
        // GraphQL allows `data` and `errors` to coexist; a partial response is
        // never treated as success.
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

fn parse_comment(value: &Value) -> anyhow::Result<IssueComment> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use drasi_workgraph_common::trust::ActorType;

    fn comment_json() -> Value {
        serde_json::json!({
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
    fn comment_metadata_is_parsed_from_immutable_fields() {
        let comment = parse_comment(&comment_json()).expect("parses");
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
        value["user"]["login"] = serde_json::json!("someone-else");
        assert!(parse_comment(&value)
            .expect("parses")
            .is_authored_by(&trusted()));
    }

    #[test]
    fn edited_comments_are_detected() {
        let mut value = comment_json();
        value["updated_at"] = serde_json::json!("2026-08-14T01:00:00Z");
        assert!(!parse_comment(&value).expect("parses").is_unedited());
    }

    #[test]
    fn missing_timestamps_are_treated_as_edited() {
        let mut value = comment_json();
        value["updated_at"] = Value::Null;
        assert!(!parse_comment(&value).expect("parses").is_unedited());
    }

    #[test]
    fn comments_without_both_trust_values_are_never_trusted() {
        for user in [
            serde_json::json!({ "login": "drasi-bot" }),
            serde_json::json!({ "node_id": "U_kgDOBmvcSA", "type": "Bot" }),
            serde_json::json!({ "node_id": "U_kgDOBmvcSA", "id": 4021243 }),
            serde_json::json!({
                "node_id": "U_kgDOBmvcSA", "id": 4021243, "type": "Mannequin"
            }),
        ] {
            let mut value = comment_json();
            value["user"] = user.clone();
            let comment = parse_comment(&value).expect("parses");
            assert!(comment.author.is_none(), "unexpected identity for {user}");
            assert!(!comment.is_authored_by(&trusted()));
        }
    }

    #[test]
    fn a_missing_node_id_on_the_author_does_not_block_trust() {
        // The node ID is audit data: an author GitHub reports without one is
        // still fully identified by its database ID and actor type.
        let mut value = comment_json();
        value["user"]["node_id"] = Value::Null;
        let comment = parse_comment(&value).expect("parses");
        assert_eq!(
            comment.author,
            Some(AuthorIdentity::new(4021243, ActorType::Bot).with_login("drasi-bot"))
        );
        assert!(comment.is_authored_by(&trusted()));
    }

    #[test]
    fn missing_node_id_is_rejected() {
        let mut value = comment_json();
        value["node_id"] = Value::Null;
        assert!(parse_comment(&value).is_err());
    }
}
