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

//! A minimal GitHub GraphQL v4 client: one query surface with correct cursor
//! pagination and a bounded number of concurrently in-flight requests.
//!
//! Every query below aliases its fields to the exact REST/webhook JSON shape
//! that `drasi_source_github_workgraph::mapping::Converter` already knows how
//! to parse (see that crate's `*_PROPS` pointer tables). A handful of GraphQL
//! shapes cannot be aliased into that shape directly (connections that must
//! become flat arrays, enums that must become lowercase strings, and
//! `head`/`base` sub-objects) — the `reshape_*` functions below perform that
//! purely-syntactic, GitHub-API-shape-only transformation. No WorkGraph domain
//! rule (label, relation ID, status derivation, Assignment/Result parsing) is
//! reimplemented anywhere in this crate; all of that stays in `Converter`.

use anyhow::{anyhow, bail, Context, Result};
use log::warn;
use reqwest::Client;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;
use tokio::time::sleep;

use crate::config::DEFAULT_PAGE_SIZE;

const MAX_ATTEMPTS: u32 = 4;
const INITIAL_RETRY_DELAY: Duration = Duration::from_millis(500);

const ORG_QUERY: &str = r#"
query($org: String!) {
  organization(login: $org) {
    node_id: id
    id: databaseId
    login
    url
    avatar_url: avatarUrl
    description
  }
}
"#;

const REPOS_QUERY: &str = r#"
query($org: String!, $cursor: String, $pageSize: Int!) {
  organization(login: $org) {
    repositories(first: $pageSize, after: $cursor, orderBy: {field: NAME, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes {
        node_id: id
        id: databaseId
        name
        full_name: nameWithOwner
        owner { login }
        description
        html_url: url
        private: isPrivate
        archived: isArchived
        fork: isFork
        visibility
        created_at: createdAt
        updated_at: updatedAt
        defaultBranchRef { name }
        repositoryTopics(first: 50) { nodes { topic { name } } }
      }
    }
  }
}
"#;

const ISSUES_QUERY: &str = r#"
query($owner: String!, $name: String!, $cursor: String, $pageSize: Int!) {
  repository(owner: $owner, name: $name) {
    issues(first: $pageSize, after: $cursor, states: [OPEN]) {
      pageInfo { hasNextPage endCursor }
      nodes {
        node_id: id
        id: databaseId
        number
        title
        body
        state
        state_reason: stateReason
        locked
        created_at: createdAt
        updated_at: updatedAt
        closed_at: closedAt
        html_url: url
        author_association: authorAssociation
        user: author { login type: __typename ... on Node { node_id: id } ... on Bot { id: databaseId } ... on Mannequin { id: databaseId } ... on Organization { id: databaseId } ... on User { id: databaseId } }
        assignees(first: 50) { nodes { login } }
        labels(first: $pageSize) {
          pageInfo { hasNextPage endCursor }
          nodes { name }
        }
        comments { totalCount }
      }
    }
  }
}
"#;

const PULL_REQUESTS_QUERY: &str = r#"
query($owner: String!, $name: String!, $cursor: String, $pageSize: Int!) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: $pageSize, after: $cursor, states: [OPEN]) {
      pageInfo { hasNextPage endCursor }
      nodes {
        node_id: id
        id: databaseId
        number
        title
        body
        state
        locked
        created_at: createdAt
        updated_at: updatedAt
        closed_at: closedAt
        html_url: url
        author_association: authorAssociation
        user: author { login type: __typename ... on Node { node_id: id } ... on Bot { id: databaseId } ... on Mannequin { id: databaseId } ... on Organization { id: databaseId } ... on User { id: databaseId } }
        assignees(first: 50) { nodes { login } }
        labels(first: $pageSize) {
          pageInfo { hasNextPage endCursor }
          nodes { name }
        }
        draft: isDraft
        merged
        merged_at: mergedAt
        head_ref_name: headRefName
        head_sha: headRefOid
        base_ref_name: baseRefName
        base_sha: baseRefOid
        comments { totalCount }
        reviews(states: [COMMENTED, APPROVED, CHANGES_REQUESTED, DISMISSED]) { totalCount }
      }
    }
  }
}
"#;

const ITEM_LABELS_QUERY: &str = r#"
query($id: ID!, $cursor: String, $pageSize: Int!) {
  node(id: $id) {
    ... on Issue {
      labels(first: $pageSize, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { name }
      }
    }
    ... on PullRequest {
      labels(first: $pageSize, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { name }
      }
    }
  }
}
"#;

const ISSUE_COMMENTS_QUERY: &str = r#"
query($id: ID!, $cursor: String, $pageSize: Int!) {
  node(id: $id) {
    ... on Issue {
      comments(first: $pageSize, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          node_id: id
          id: databaseId
          body
          created_at: createdAt
          updated_at: updatedAt
          html_url: url
          author_association: authorAssociation
          user: author { login type: __typename ... on Node { node_id: id } ... on Bot { id: databaseId } ... on Mannequin { id: databaseId } ... on Organization { id: databaseId } ... on User { id: databaseId } }
        }
      }
    }
  }
}
"#;

const PR_COMMENTS_QUERY: &str = r#"
query($id: ID!, $cursor: String, $pageSize: Int!) {
  node(id: $id) {
    ... on PullRequest {
      comments(first: $pageSize, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          node_id: id
          id: databaseId
          body
          created_at: createdAt
          updated_at: updatedAt
          html_url: url
          author_association: authorAssociation
          user: author { login type: __typename ... on Node { node_id: id } ... on Bot { id: databaseId } ... on Mannequin { id: databaseId } ... on Organization { id: databaseId } ... on User { id: databaseId } }
        }
      }
    }
  }
}
"#;

const PR_REVIEWS_QUERY: &str = r#"
query($id: ID!, $cursor: String, $pageSize: Int!) {
  node(id: $id) {
    ... on PullRequest {
      reviews(
        first: $pageSize
        after: $cursor
        states: [COMMENTED, APPROVED, CHANGES_REQUESTED, DISMISSED]
      ) {
        pageInfo { hasNextPage endCursor }
        nodes {
          node_id: id
          id: databaseId
          state
          body
          submitted_at: submittedAt
          commit { oid }
          html_url: url
          author_association: authorAssociation
          user: author { login type: __typename ... on Node { node_id: id } ... on Bot { id: databaseId } ... on Mannequin { id: databaseId } ... on Organization { id: databaseId } ... on User { id: databaseId } }
        }
      }
    }
  }
}
"#;

/// A minimal, read-only GitHub GraphQL v4 client.
///
/// Holds a semaphore that bounds the number of GraphQL requests in flight at
/// once, regardless of how many logical tasks (repositories, issues, PRs) are
/// concurrently trying to fetch data.
///
/// Cheaply `Clone`: `reqwest::Client` is internally `Arc`-backed and the
/// concurrency semaphore is `Arc`-wrapped, so clones share both the
/// connection pool and the concurrency bound.
#[derive(Clone)]
pub struct GitHubGraphQLClient {
    http: Client,
    api_url: String,
    semaphore: Arc<Semaphore>,
}

impl GitHubGraphQLClient {
    pub fn new(token: &str, api_base_url: &str, max_concurrency: usize) -> Result<Self> {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            reqwest::header::HeaderValue::from_str(&format!("bearer {token}"))
                .context("invalid GitHub token header value")?,
        );
        headers.insert(
            reqwest::header::USER_AGENT,
            reqwest::header::HeaderValue::from_static("drasi-bootstrap-github-workgraph"),
        );
        let http = Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .context("failed to build GitHub GraphQL HTTP client")?;
        Ok(Self {
            http,
            api_url: api_base_url.to_string(),
            semaphore: Arc::new(Semaphore::new(max_concurrency.max(1))),
        })
    }

    /// Execute one GraphQL request, retrying on transport errors, GitHub rate
    /// limiting (429/403 secondary limit), and 5xx responses.
    async fn execute(&self, query: &str, variables: Value) -> Result<Value> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| anyhow!("GraphQL client semaphore closed"))?;
        let body = json!({ "query": query, "variables": variables });
        let mut delay = INITIAL_RETRY_DELAY;
        for attempt in 1..=MAX_ATTEMPTS {
            let response = match self.http.post(&self.api_url).json(&body).send().await {
                Ok(response) => response,
                Err(err) if attempt < MAX_ATTEMPTS => {
                    warn!("GitHub GraphQL request failed ({err}); retrying in {delay:?}");
                    sleep(delay).await;
                    delay *= 2;
                    continue;
                }
                Err(err) => return Err(err).context("GitHub GraphQL request failed"),
            };
            let status = response.status();
            if status.as_u16() == 429 || status.is_server_error() {
                if attempt >= MAX_ATTEMPTS {
                    bail!("GitHub GraphQL API error after retries: {status}");
                }
                warn!("GitHub GraphQL API rate limited/server error ({status}); retrying");
                sleep(delay).await;
                delay *= 2;
                continue;
            }
            if !status.is_success() {
                let text = response.text().await.unwrap_or_default();
                bail!("GitHub GraphQL API request failed: {status}: {text}");
            }
            let payload: Value = response
                .json()
                .await
                .context("failed to decode GitHub GraphQL response as JSON")?;
            if let Some(errors) = payload.get("errors").and_then(Value::as_array) {
                if !errors.is_empty() {
                    bail!("GitHub GraphQL API returned errors: {errors:?}");
                }
            }
            return payload
                .get("data")
                .cloned()
                .ok_or_else(|| anyhow!("GitHub GraphQL response missing 'data'"));
        }
        unreachable!("loop always returns or bails");
    }

    /// Fetch every page of a `nodes`/`pageInfo` connection located at `path`
    /// within the response, calling `build_vars` with the current cursor.
    /// `initial_cursor` is `None` for a full connection and set when continuing
    /// after a connection page already embedded in a parent query.
    async fn fetch_connection<F>(
        &self,
        query: &str,
        path: &[&str],
        initial_cursor: Option<String>,
        mut build_vars: F,
    ) -> Result<Vec<Value>>
    where
        F: FnMut(Option<&str>) -> Value,
    {
        let mut cursor = initial_cursor;
        let mut all = Vec::new();
        let path_display = path.join(".");
        loop {
            let data = self.execute(query, build_vars(cursor.as_deref())).await?;
            let mut connection = &data;
            for key in path {
                connection = connection
                    .get(key)
                    .filter(|value| !value.is_null())
                    .ok_or_else(|| {
                        anyhow!("GitHub GraphQL response missing connection '{path_display}'")
                    })?;
            }
            let nodes = connection
                .get("nodes")
                .and_then(Value::as_array)
                .ok_or_else(|| {
                    anyhow!("GitHub GraphQL connection '{path_display}' missing 'nodes'")
                })?;
            all.extend(nodes.iter().cloned());
            let page_info = connection.get("pageInfo").ok_or_else(|| {
                anyhow!("GitHub GraphQL connection '{path_display}' missing 'pageInfo'")
            })?;
            let has_next = page_info
                .get("hasNextPage")
                .and_then(Value::as_bool)
                .ok_or_else(|| {
                    anyhow!("GitHub GraphQL connection '{path_display}' has invalid 'hasNextPage'")
                })?;
            if !has_next {
                break;
            }
            let next_cursor = page_info
                .get("endCursor")
                .and_then(Value::as_str)
                .filter(|cursor| !cursor.is_empty())
                .ok_or_else(|| {
                    anyhow!(
                        "GitHub GraphQL connection '{path_display}' has another page but no cursor"
                    )
                })?;
            if cursor.as_deref() == Some(next_cursor) {
                bail!("GitHub GraphQL connection '{path_display}' returned a non-advancing cursor");
            }
            cursor = Some(next_cursor.to_string());
        }
        Ok(all)
    }

    pub async fn fetch_organization(&self, org: &str) -> Result<Value> {
        let data = self.execute(ORG_QUERY, json!({ "org": org })).await?;
        data.get("organization")
            .filter(|v| !v.is_null())
            .cloned()
            .ok_or_else(|| anyhow!("GitHub organization '{org}' not found or inaccessible"))
    }

    pub async fn fetch_repositories(&self, org: &str) -> Result<Vec<Value>> {
        let org = org.to_string();
        let nodes = self
            .fetch_connection(
                REPOS_QUERY,
                &["organization", "repositories"],
                None,
                move |cursor| {
                    json!({ "org": org, "cursor": cursor, "pageSize": DEFAULT_PAGE_SIZE })
                },
            )
            .await?;
        Ok(nodes.into_iter().map(reshape_repository).collect())
    }

    pub async fn fetch_issues(&self, owner: &str, name: &str) -> Result<Vec<Value>> {
        let (owner, name) = (owner.to_string(), name.to_string());
        let nodes = self
            .fetch_connection(
                ISSUES_QUERY,
                &["repository", "issues"],
                None,
                move |cursor| {
                    json!({ "owner": owner, "name": name, "cursor": cursor, "pageSize": DEFAULT_PAGE_SIZE })
                },
            )
            .await?;
        let mut items = Vec::with_capacity(nodes.len());
        for node in nodes {
            items.push(reshape_work_item(self.complete_item_labels(node).await?));
        }
        Ok(items)
    }

    pub async fn fetch_pull_requests(&self, owner: &str, name: &str) -> Result<Vec<Value>> {
        let (owner, name) = (owner.to_string(), name.to_string());
        let nodes = self
            .fetch_connection(
                PULL_REQUESTS_QUERY,
                &["repository", "pullRequests"],
                None,
                move |cursor| {
                    json!({ "owner": owner, "name": name, "cursor": cursor, "pageSize": DEFAULT_PAGE_SIZE })
                },
            )
            .await?;
        let mut items = Vec::with_capacity(nodes.len());
        for node in nodes {
            let node = self.complete_item_labels(node).await?;
            items.push(reshape_pull_request(reshape_work_item(node)));
        }
        Ok(items)
    }

    pub async fn fetch_issue_comments(&self, node_id: &str) -> Result<Vec<Value>> {
        let id = node_id.to_string();
        let nodes = self
            .fetch_connection(
                ISSUE_COMMENTS_QUERY,
                &["node", "comments"],
                None,
                move |cursor| json!({ "id": id, "cursor": cursor, "pageSize": DEFAULT_PAGE_SIZE }),
            )
            .await?;
        Ok(nodes.into_iter().map(reshape_comment).collect())
    }

    pub async fn fetch_pr_comments(&self, node_id: &str) -> Result<Vec<Value>> {
        let id = node_id.to_string();
        let nodes = self
            .fetch_connection(
                PR_COMMENTS_QUERY,
                &["node", "comments"],
                None,
                move |cursor| json!({ "id": id, "cursor": cursor, "pageSize": DEFAULT_PAGE_SIZE }),
            )
            .await?;
        Ok(nodes.into_iter().map(reshape_comment).collect())
    }

    pub async fn fetch_pr_reviews(&self, node_id: &str) -> Result<Vec<Value>> {
        let id = node_id.to_string();
        let nodes = self
            .fetch_connection(
                PR_REVIEWS_QUERY,
                &["node", "reviews"],
                None,
                move |cursor| json!({ "id": id, "cursor": cursor, "pageSize": DEFAULT_PAGE_SIZE }),
            )
            .await?;
        Ok(nodes.into_iter().map(reshape_review).collect())
    }

    async fn complete_item_labels(&self, mut node: Value) -> Result<Value> {
        let page_info = node
            .pointer("/labels/pageInfo")
            .ok_or_else(|| anyhow!("GitHub work item labels missing 'pageInfo'"))?;
        let has_next = page_info
            .get("hasNextPage")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("GitHub work item labels have invalid 'hasNextPage'"))?;
        if !has_next {
            return Ok(node);
        }

        let cursor = page_info
            .get("endCursor")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty())
            .ok_or_else(|| anyhow!("GitHub work item labels have another page but no cursor"))?
            .to_string();
        let id = node
            .get("node_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("GitHub work item missing 'node_id'"))?
            .to_string();
        let additional = self
            .fetch_connection(
                ITEM_LABELS_QUERY,
                &["node", "labels"],
                Some(cursor),
                move |cursor| json!({ "id": id, "cursor": cursor, "pageSize": DEFAULT_PAGE_SIZE }),
            )
            .await?;
        node.pointer_mut("/labels/nodes")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| anyhow!("GitHub work item labels missing 'nodes'"))?
            .extend(additional);
        Ok(node)
    }
}

fn lower_str(value: &mut Value, key: &str) {
    if let Some(s) = value.get(key).and_then(Value::as_str) {
        let lowered = s.to_lowercase();
        value[key] = Value::String(lowered);
    }
}

/// Flatten `defaultBranchRef { name }` and `repositoryTopics.nodes[].topic.name`
/// into the flat `default_branch`/`topics` shape the REST/webhook payload
/// (and therefore `Converter`) uses, and lowercase the `visibility` enum.
fn reshape_repository(mut node: Value) -> Value {
    let default_branch = node
        .get("defaultBranchRef")
        .and_then(|r| r.get("name"))
        .and_then(Value::as_str)
        .map(str::to_string);
    if let Value::Object(map) = &mut node {
        map.remove("defaultBranchRef");
        map.insert(
            "default_branch".to_string(),
            default_branch.map(Value::String).unwrap_or(Value::Null),
        );
        let topics: Vec<Value> = map
            .remove("repositoryTopics")
            .and_then(|v| v.get("nodes").cloned())
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|n| n.pointer("/topic/name").cloned())
            .collect();
        map.insert("topics".to_string(), Value::Array(topics));
    }
    lower_str(&mut node, "visibility");
    node
}

/// Common reshaping for both Issues and PRs: lowercase `state`/`state_reason`,
/// flatten `comments.totalCount` into a private `_comment_count` marker used
/// only for deciding whether a follow-up comments fetch is worthwhile, and
/// unwrap the `assignees`/`labels` connections' `nodes` array into a flat
/// array of `{login}`/`{name}` objects — the exact REST/webhook shape
/// `mapping::names()` expects (it calls `.get(field)` on each element, so the
/// elements must stay objects, not be reduced to bare strings).
fn reshape_work_item(mut node: Value) -> Value {
    lower_str(&mut node, "state");
    lower_str(&mut node, "state_reason");
    if let Value::Object(map) = &mut node {
        if let Some(count) = map
            .remove("comments")
            .and_then(|v| v.get("totalCount").cloned())
        {
            map.insert("_comment_count".to_string(), count);
        }
        if let Some(assignees) = map.remove("assignees") {
            let nodes = assignees
                .get("nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            map.insert("assignees".to_string(), Value::Array(nodes));
        }
        if let Some(labels) = map.remove("labels") {
            let nodes = labels
                .get("nodes")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            map.insert("labels".to_string(), Value::Array(nodes));
        }
    }
    node
}

/// PR-only reshaping: flatten `reviews.totalCount` into `_review_count`, and
/// `head_ref_name`/`head_sha`/`base_ref_name`/`base_sha` into the nested
/// `head: {ref, sha}` / `base: {ref, sha}` shape `Converter` expects.
fn reshape_pull_request(mut node: Value) -> Value {
    if let Value::Object(map) = &mut node {
        if let Some(count) = map
            .remove("reviews")
            .and_then(|v| v.get("totalCount").cloned())
        {
            map.insert("_review_count".to_string(), count);
        }
        let head_ref = map.remove("head_ref_name").unwrap_or(Value::Null);
        let head_sha = map.remove("head_sha").unwrap_or(Value::Null);
        let base_ref = map.remove("base_ref_name").unwrap_or(Value::Null);
        let base_sha = map.remove("base_sha").unwrap_or(Value::Null);
        map.insert(
            "head".to_string(),
            json!({ "ref": head_ref, "sha": head_sha }),
        );
        map.insert(
            "base".to_string(),
            json!({ "ref": base_ref, "sha": base_sha }),
        );
    }
    node
}

fn reshape_comment(node: Value) -> Value {
    node
}

/// Flatten `commit { oid }` into the flat `commit_id` string REST/webhook
/// payloads use, and lowercase the review `state` enum.
fn reshape_review(mut node: Value) -> Value {
    lower_str(&mut node, "state");
    if let Value::Object(map) = &mut node {
        let commit_id = map
            .remove("commit")
            .and_then(|c| c.get("oid").cloned())
            .unwrap_or(Value::Null);
        map.insert("commit_id".to_string(), commit_id);
    }
    node
}

/// Read back the `_comment_count`/`_review_count` markers `reshape_work_item`/
/// `reshape_pull_request` attach, defaulting to 0 (treated as "fetch anyway"
/// callers should not rely on a missing marker meaning zero).
pub fn item_comment_count(item: &Value) -> u64 {
    item.get("_comment_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

pub fn pr_review_count(item: &Value) -> u64 {
    item.get("_review_count")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reshape_repository_flattens_topics_and_default_branch() {
        let raw = json!({
            "node_id": "R_1",
            "visibility": "PUBLIC",
            "defaultBranchRef": { "name": "main" },
            "repositoryTopics": { "nodes": [
                { "topic": { "name": "graphs" } },
                { "topic": { "name": "streaming" } }
            ]}
        });
        let shaped = reshape_repository(raw);
        assert_eq!(shaped["default_branch"], json!("main"));
        assert_eq!(shaped["topics"], json!(["graphs", "streaming"]));
        assert_eq!(shaped["visibility"], json!("public"));
        assert!(shaped.get("defaultBranchRef").is_none());
        assert!(shaped.get("repositoryTopics").is_none());
    }

    #[test]
    fn reshape_repository_handles_empty_repo() {
        let raw = json!({ "node_id": "R_1", "defaultBranchRef": null, "repositoryTopics": null });
        let shaped = reshape_repository(raw);
        assert_eq!(shaped["default_branch"], Value::Null);
        assert_eq!(shaped["topics"], json!([]));
    }

    #[test]
    fn reshape_work_item_lowercases_state_and_flattens_lists() {
        let raw = json!({
            "state": "OPEN",
            "state_reason": "NOT_PLANNED",
            "comments": { "totalCount": 3 },
            "assignees": { "nodes": [{ "login": "ada" }, { "login": "grace" }] },
            "labels": { "nodes": [{ "name": "status: needs-review" }] }
        });
        let shaped = reshape_work_item(raw);
        assert_eq!(shaped["state"], json!("open"));
        assert_eq!(shaped["state_reason"], json!("not_planned"));
        assert_eq!(shaped["_comment_count"], json!(3));
        assert_eq!(
            shaped["assignees"],
            json!([{ "login": "ada" }, { "login": "grace" }])
        );
        assert_eq!(
            shaped["labels"],
            json!([{ "name": "status: needs-review" }])
        );
        assert_eq!(item_comment_count(&shaped), 3);
    }

    #[test]
    fn reshape_pull_request_nests_head_and_base() {
        let raw = json!({
            "head_ref_name": "feature",
            "head_sha": "abc123",
            "base_ref_name": "main",
            "base_sha": "def456",
            "reviews": { "totalCount": 2 }
        });
        let shaped = reshape_pull_request(raw);
        assert_eq!(shaped["head"], json!({ "ref": "feature", "sha": "abc123" }));
        assert_eq!(shaped["base"], json!({ "ref": "main", "sha": "def456" }));
        assert_eq!(pr_review_count(&shaped), 2);
    }

    #[test]
    fn reshape_review_flattens_commit_and_lowercases_state() {
        let raw = json!({ "state": "CHANGES_REQUESTED", "commit": { "oid": "sha1" } });
        let shaped = reshape_review(raw);
        assert_eq!(shaped["state"], json!("changes_requested"));
        assert_eq!(shaped["commit_id"], json!("sha1"));
    }
}
