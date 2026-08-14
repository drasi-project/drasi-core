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

//! Authoritative GitHub GraphQL client used by hydrator/reconciler/bootstrap.

use crate::config::ProjectSpec;
use crate::rate_limit::{classify_retry, exp_backoff};
use crate::types::WebhookLocator;
use anyhow::{anyhow, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::time::sleep;

/// Fetched root object used by hydrator.
#[derive(Debug, Clone, PartialEq)]
pub enum FetchedRoot {
    Repository(RepositoryData),
    Issue(IssueData),
    PullRequest(PullRequestData),
    IssueComment(IssueCommentData),
    PullRequestReview(PullRequestReviewData),
    PullRequestReviewComment(PullRequestReviewCommentData),
    Project(ProjectData),
    ProjectItem(ProjectItemData),
}

impl FetchedRoot {
    pub fn root_id(&self) -> &str {
        match self {
            FetchedRoot::Repository(v) => &v.id,
            FetchedRoot::Issue(v) => &v.id,
            FetchedRoot::PullRequest(v) => &v.id,
            FetchedRoot::IssueComment(v) => &v.id,
            FetchedRoot::PullRequestReview(v) => &v.id,
            FetchedRoot::PullRequestReviewComment(v) => &v.id,
            FetchedRoot::Project(v) => &v.id,
            FetchedRoot::ProjectItem(v) => &v.id,
        }
    }

    pub fn root_kind(&self) -> &'static str {
        match self {
            FetchedRoot::Repository(_) => "GitHubRepository",
            FetchedRoot::Issue(_) => "GitHubIssue",
            FetchedRoot::PullRequest(_) => "GitHubPullRequest",
            FetchedRoot::IssueComment(_) => "GitHubIssueComment",
            FetchedRoot::PullRequestReview(_) => "GitHubPullRequestReview",
            FetchedRoot::PullRequestReviewComment(_) => "GitHubPullRequestReviewComment",
            FetchedRoot::Project(_) => "GitHubProject",
            FetchedRoot::ProjectItem(_) => "GitHubProjectItem",
        }
    }

    pub fn repository_full_name(&self) -> Option<&str> {
        match self {
            FetchedRoot::Repository(v) => Some(v.name_with_owner.as_str()),
            FetchedRoot::Issue(v) => Some(v.repository.name_with_owner.as_str()),
            FetchedRoot::PullRequest(v) => Some(v.repository.name_with_owner.as_str()),
            FetchedRoot::IssueComment(v) => Some(v.repository.name_with_owner.as_str()),
            FetchedRoot::PullRequestReview(v) => {
                Some(v.pull_request.repository.name_with_owner.as_str())
            }
            FetchedRoot::PullRequestReviewComment(v) => Some(v.repository.name_with_owner.as_str()),
            FetchedRoot::Project(_) | FetchedRoot::ProjectItem(_) => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GitHubGraphQLClient {
    client: reqwest::Client,
    graphql_url: String,
    token: String,
    default_headers: HeaderMap,
}

impl GitHubGraphQLClient {
    pub fn new(graphql_url: String, token: String) -> Result<Self> {
        let mut headers = HeaderMap::new();
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static("drasi-source-github/1.0"),
        );
        headers.insert(
            ACCEPT,
            HeaderValue::from_static("application/vnd.github+json"),
        );
        let auth = format!("Bearer {token}");
        headers.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&auth).context("Failed to build auth header")?,
        );

        let client = reqwest::Client::builder()
            .default_headers(headers.clone())
            .build()
            .context("Failed to construct reqwest client")?;

        Ok(Self {
            client,
            graphql_url,
            token,
            default_headers: headers,
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub async fn fetch_root_from_locator(
        &self,
        locator: &WebhookLocator,
    ) -> Result<Option<FetchedRoot>> {
        match locator.event_type.as_str() {
            "repository" => {
                let Some(repo) = locator.repository_full_name.as_deref() else {
                    return Ok(None);
                };
                let (owner, name) = split_repo(repo)?;
                self.fetch_repository(&owner, &name)
                    .await
                    .map(|o| o.map(FetchedRoot::Repository))
            }
            "issues" => {
                if let Some(id) = locator.node_id.as_deref() {
                    self.fetch_issue(id)
                        .await
                        .map(|o| o.map(FetchedRoot::Issue))
                } else {
                    Ok(None)
                }
            }
            "pull_request" => {
                if let Some(id) = locator.node_id.as_deref() {
                    self.fetch_pull_request(id)
                        .await
                        .map(|o| o.map(FetchedRoot::PullRequest))
                } else {
                    Ok(None)
                }
            }
            "issue_comment" => {
                if let Some(id) = locator.node_id.as_deref() {
                    self.fetch_issue_comment(id)
                        .await
                        .map(|o| o.map(FetchedRoot::IssueComment))
                } else {
                    Ok(None)
                }
            }
            "pull_request_review" => {
                if let Some(id) = locator.node_id.as_deref() {
                    self.fetch_pull_request_review(id)
                        .await
                        .map(|o| o.map(FetchedRoot::PullRequestReview))
                } else {
                    Ok(None)
                }
            }
            "pull_request_review_comment" => {
                if let Some(id) = locator.node_id.as_deref() {
                    self.fetch_pull_request_review_comment(id)
                        .await
                        .map(|o| o.map(FetchedRoot::PullRequestReviewComment))
                } else {
                    Ok(None)
                }
            }
            "projects_v2" => {
                if let Some(project_id) = locator.project_id.as_deref() {
                    self.fetch_project(project_id)
                        .await
                        .map(|o| o.map(FetchedRoot::Project))
                } else if let (Some(owner), Some(number)) =
                    (locator.project_owner.as_deref(), locator.project_number)
                {
                    self.fetch_project_by_owner_number(owner, number)
                        .await
                        .map(|o| o.map(FetchedRoot::Project))
                } else {
                    Ok(None)
                }
            }
            "projects_v2_item" => {
                if let Some(id) = locator.node_id.as_deref() {
                    self.fetch_project_item(id)
                        .await
                        .map(|o| o.map(FetchedRoot::ProjectItem))
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
        }
    }

    pub async fn fetch_project_by_owner_number(
        &self,
        owner: &str,
        number: u32,
    ) -> Result<Option<ProjectData>> {
        let query = r#"
query($owner: String!, $number: Int!) {
  organization(login: $owner) {
    projectV2(number: $number) {
      ...ProjectFields
    }
  }
  user(login: $owner) {
    projectV2(number: $number) {
      ...ProjectFields
    }
  }
}

fragment ProjectFields on ProjectV2 {
  id
  title
  number
  url
  createdAt
  updatedAt
  owner { ... on Organization { login } ... on User { login } }
  items(first: 100) {
    pageInfo { hasNextPage endCursor }
    nodes { ...ProjectItemFields }
  }
}

fragment ProjectItemFields on ProjectV2Item {
  id
  type
  createdAt
  updatedAt
  project { id number owner { login } }
  content {
    __typename
    ... on Issue { id number title state repository { id nameWithOwner } }
    ... on PullRequest { id number title state repository { id nameWithOwner } }
    ... on DraftIssue { id title body }
  }
  fieldValues(first: 50) {
    pageInfo { hasNextPage endCursor }
    nodes {
      __typename
      ... on ProjectV2ItemFieldSingleSelectValue {
        name
        field { ... on ProjectV2SingleSelectField { id name } }
        optionId
      }
      ... on ProjectV2ItemFieldTextValue {
        text
        field { ... on ProjectV2FieldCommon { id name } }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            organization: Option<OwnerProjectNode>,
            user: Option<OwnerProjectNode>,
        }
        #[derive(Debug, Deserialize)]
        struct OwnerProjectNode {
            #[serde(rename = "projectV2")]
            project_v2: Option<ProjectData>,
        }

        let variables = json!({ "owner": owner, "number": number as i64 });
        let data: Resp = self
            .execute_nullable_lookups(query, variables, &["organization", "user"])
            .await?;
        let project = data
            .organization
            .and_then(|owner| owner.project_v2)
            .or_else(|| data.user.and_then(|owner| owner.project_v2));
        match project {
            Some(project) => Ok(Some(self.with_paginated_project_items(project).await?)),
            None => Ok(None),
        }
    }

    pub async fn fetch_project(&self, node_id: &str) -> Result<Option<ProjectData>> {
        let query = r#"
query($id: ID!) {
  node(id: $id) {
    __typename
    ... on ProjectV2 {
      id
      title
      number
      url
      createdAt
      updatedAt
      owner { ... on Organization { login } ... on User { login } }
      items(first: 100) {
        pageInfo { hasNextPage endCursor }
        nodes { ...ProjectItemFields }
      }
    }
  }
}

fragment ProjectItemFields on ProjectV2Item {
  id
  type
  createdAt
  updatedAt
  project { id number owner { login } }
  content {
    __typename
    ... on Issue { id number title state repository { id nameWithOwner } }
    ... on PullRequest { id number title state repository { id nameWithOwner } }
    ... on DraftIssue { id title body }
  }
  fieldValues(first: 50) {
    pageInfo { hasNextPage endCursor }
    nodes {
      __typename
      ... on ProjectV2ItemFieldSingleSelectValue {
        name
        field { ... on ProjectV2SingleSelectField { id name } }
        optionId
      }
      ... on ProjectV2ItemFieldTextValue {
        text
        field { ... on ProjectV2FieldCommon { id name } }
      }
    }
  }
}
"#;
        let variables = json!({ "id": node_id });
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            ProjectV2(ProjectData),
        }
        let data: Resp = self
            .execute_nullable_lookup(query, variables, "node")
            .await?;
        match data.node {
            Some(Node::ProjectV2(project)) => {
                Ok(Some(self.with_paginated_project_items(project).await?))
            }
            None => Ok(None),
        }
    }

    pub async fn fetch_project_item(&self, node_id: &str) -> Result<Option<ProjectItemData>> {
        let query = r#"
query($id: ID!) {
  node(id: $id) {
    __typename
    ... on ProjectV2Item {
      id
      type
      createdAt
      updatedAt
      project { id number owner { login } }
      content {
        __typename
        ... on Issue { id number title state repository { id nameWithOwner } }
        ... on PullRequest { id number title state repository { id nameWithOwner } }
        ... on DraftIssue { id title body }
      }
      fieldValues(first: 50) {
        pageInfo { hasNextPage endCursor }
        nodes {
          __typename
          ... on ProjectV2ItemFieldSingleSelectValue {
            name
            field { ... on ProjectV2SingleSelectField { id name } }
            optionId
          }
          ... on ProjectV2ItemFieldTextValue {
            text
            field { ... on ProjectV2FieldCommon { id name } }
          }
        }
      }
    }
  }
}
"#;
        let variables = json!({ "id": node_id });
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            ProjectV2Item(ProjectItemData),
        }
        let data: Resp = self
            .execute_nullable_lookup(query, variables, "node")
            .await?;
        match data.node {
            Some(Node::ProjectV2Item(item)) => Ok(Some(
                self.with_paginated_project_item_field_values(item).await?,
            )),
            None => Ok(None),
        }
    }

    pub async fn fetch_repository(
        &self,
        owner: &str,
        name: &str,
    ) -> Result<Option<RepositoryData>> {
        let query = r#"
query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    id
    name
    nameWithOwner
    owner { login }
    description
    url
    isArchived
    isPrivate
    createdAt
    updatedAt
    defaultBranchRef { name }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            repository: Option<RepositoryData>,
        }
        let variables = json!({ "owner": owner, "name": name });
        let data: Resp = self
            .execute_nullable_lookup(query, variables, "repository")
            .await?;
        Ok(data.repository)
    }

    pub async fn fetch_issue(&self, node_id: &str) -> Result<Option<IssueData>> {
        let query = r#"
query($id: ID!) {
  node(id: $id) {
    __typename
    ... on Issue {
      ...IssueFields
    }
  }
}

fragment IssueFields on Issue {
  id
  number
  title
  body
  state
  createdAt
  updatedAt
  closedAt
  url
  author { login }
  repository { id nameWithOwner }
  assignees(first: 20) { pageInfo { hasNextPage endCursor } nodes { id login } }
  labels(first: 20) { pageInfo { hasNextPage endCursor } nodes { id name } }
  comments(first: 100) {
    pageInfo { hasNextPage endCursor }
    nodes { ...IssueCommentFields }
  }
}

fragment IssueCommentFields on IssueComment {
  id
  body
  createdAt
  updatedAt
  url
  isMinimized
  author {
    __typename
    id
    login
    ... on User { databaseId }
    ... on Bot { databaseId }
    ... on Organization { databaseId }
    ... on Mannequin { databaseId }
    ... on EnterpriseUserAccount { databaseId }
  }
  performedViaGithubApp { databaseId }
  issue { id }
  pullRequest { id }
  repository { id nameWithOwner }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            Issue(IssueData),
        }
        let variables = json!({ "id": node_id });
        let data: Resp = self
            .execute_nullable_lookup(query, variables, "node")
            .await?;
        match data.node {
            Some(Node::Issue(issue)) => Ok(Some(self.with_paginated_issue(issue).await?)),
            None => Ok(None),
        }
    }

    pub async fn fetch_pull_request(&self, node_id: &str) -> Result<Option<PullRequestData>> {
        let query = r#"
query($id: ID!) {
  node(id: $id) {
    __typename
    ... on PullRequest {
      ...PullRequestFields
    }
  }
}

fragment PullRequestFields on PullRequest {
  id
  number
  title
  body
  state
  createdAt
  updatedAt
  closedAt
  mergedAt
  url
  isDraft
  headRefName
  baseRefName
  author { login }
  repository { id nameWithOwner }
  assignees(first: 20) { pageInfo { hasNextPage endCursor } nodes { id login } }
  labels(first: 20) { pageInfo { hasNextPage endCursor } nodes { id name } }
  comments(first: 100) { pageInfo { hasNextPage endCursor } nodes { ...IssueCommentFields } }
  reviews(first: 100) { pageInfo { hasNextPage endCursor } nodes { ...ReviewFields } }
}

fragment IssueCommentFields on IssueComment {
  id
  body
  createdAt
  updatedAt
  url
  isMinimized
  author {
    __typename
    id
    login
    ... on User { databaseId }
    ... on Bot { databaseId }
    ... on Organization { databaseId }
    ... on Mannequin { databaseId }
    ... on EnterpriseUserAccount { databaseId }
  }
  performedViaGithubApp { databaseId }
  issue { id }
  pullRequest { id }
  repository { id nameWithOwner }
}

fragment ReviewFields on PullRequestReview {
  id
  state
  body
  createdAt
  updatedAt
  url
  author {
    __typename
    id
    login
    ... on User { databaseId }
    ... on Bot { databaseId }
    ... on Organization { databaseId }
    ... on Mannequin { databaseId }
    ... on EnterpriseUserAccount { databaseId }
  }
  performedViaGithubApp { databaseId }
  pullRequest { id repository { id nameWithOwner } }
  comments(first: 100) {
    pageInfo { hasNextPage endCursor }
    nodes { ...ReviewCommentFields }
  }
}

fragment ReviewCommentFields on PullRequestReviewComment {
  id
  body
  path
  position
  line
  diffHunk
  createdAt
  updatedAt
  url
  performedViaGithubApp { databaseId }
  pullRequestReview { id pullRequest { id repository { id nameWithOwner } } }
  author {
    __typename
    id
    login
    ... on User { databaseId }
    ... on Bot { databaseId }
    ... on Organization { databaseId }
    ... on Mannequin { databaseId }
    ... on EnterpriseUserAccount { databaseId }
  }
  repository { id nameWithOwner }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            PullRequest(PullRequestData),
        }
        let data: Resp = self
            .execute_nullable_lookup(query, json!({ "id": node_id }), "node")
            .await?;
        match data.node {
            Some(Node::PullRequest(pr)) => Ok(Some(self.with_paginated_pull_request(pr).await?)),
            None => Ok(None),
        }
    }

    pub async fn fetch_issue_comment(&self, node_id: &str) -> Result<Option<IssueCommentData>> {
        let query = r#"
query($id: ID!) {
  node(id: $id) {
    __typename
    ... on IssueComment {
      id
      body
      createdAt
      updatedAt
      url
      isMinimized
      author {
        __typename
        id
        login
        ... on User { databaseId }
        ... on Bot { databaseId }
        ... on Organization { databaseId }
        ... on Mannequin { databaseId }
        ... on EnterpriseUserAccount { databaseId }
      }
      performedViaGithubApp { databaseId }
      issue { id }
      pullRequest { id }
      repository { id nameWithOwner }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            IssueComment(IssueCommentData),
        }
        let data: Resp = self
            .execute_nullable_lookup(query, json!({ "id": node_id }), "node")
            .await?;
        Ok(data.node.map(|Node::IssueComment(comment)| comment))
    }

    pub async fn fetch_pull_request_review(
        &self,
        node_id: &str,
    ) -> Result<Option<PullRequestReviewData>> {
        let query = r#"
query($id: ID!) {
  node(id: $id) {
    __typename
    ... on PullRequestReview {
      id
      state
      body
      createdAt
      updatedAt
      url
      author {
        __typename
        id
        login
        ... on User { databaseId }
        ... on Bot { databaseId }
        ... on Organization { databaseId }
        ... on Mannequin { databaseId }
        ... on EnterpriseUserAccount { databaseId }
      }
      performedViaGithubApp { databaseId }
      pullRequest { id repository { id nameWithOwner } }
      comments(first: 100) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          body
          path
          position
          line
          diffHunk
          createdAt
          updatedAt
          url
          author {
            __typename
            id
            login
            ... on User { databaseId }
            ... on Bot { databaseId }
            ... on Organization { databaseId }
            ... on Mannequin { databaseId }
            ... on EnterpriseUserAccount { databaseId }
          }
          performedViaGithubApp { databaseId }
          repository { id nameWithOwner }
          pullRequestReview { id pullRequest { id repository { id nameWithOwner } } }
        }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            PullRequestReview(PullRequestReviewData),
        }
        let data: Resp = self
            .execute_nullable_lookup(query, json!({ "id": node_id }), "node")
            .await?;
        match data.node {
            Some(Node::PullRequestReview(review)) => {
                Ok(Some(self.with_paginated_review(review).await?))
            }
            None => Ok(None),
        }
    }

    pub async fn fetch_pull_request_review_comment(
        &self,
        node_id: &str,
    ) -> Result<Option<PullRequestReviewCommentData>> {
        let query = r#"
query($id: ID!) {
  node(id: $id) {
    __typename
    ... on PullRequestReviewComment {
      id
      body
      path
      position
      line
      diffHunk
      createdAt
      updatedAt
      url
      author {
        __typename
        id
        login
        ... on User { databaseId }
        ... on Bot { databaseId }
        ... on Organization { databaseId }
        ... on Mannequin { databaseId }
        ... on EnterpriseUserAccount { databaseId }
      }
      performedViaGithubApp { databaseId }
      repository { id nameWithOwner }
      pullRequestReview { id pullRequest { id repository { id nameWithOwner } } }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            PullRequestReviewComment(PullRequestReviewCommentData),
        }
        let data: Resp = self
            .execute_nullable_lookup(query, json!({ "id": node_id }), "node")
            .await?;
        Ok(data
            .node
            .map(|Node::PullRequestReviewComment(comment)| comment))
    }

    pub async fn fetch_all_issues(&self, repo: &str) -> Result<Vec<IssueData>> {
        let (owner, name) = split_repo(repo)?;
        let query = r#"
query($owner: String!, $name: String!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    issues(first: 100, after: $cursor, orderBy: {field: UPDATED_AT, direction: DESC}) {
      pageInfo { hasNextPage endCursor }
      nodes { ...IssueFields }
    }
  }
}

fragment IssueFields on Issue {
  id
  number
  title
  body
  state
  createdAt
  updatedAt
  closedAt
  url
  author { login }
  repository { id nameWithOwner }
  assignees(first: 20) { pageInfo { hasNextPage endCursor } nodes { id login } }
  labels(first: 20) { pageInfo { hasNextPage endCursor } nodes { id name } }
  comments(first: 50) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id
      body
      createdAt
      updatedAt
      url
      isMinimized
      author {
        __typename
        id
        login
        ... on User { databaseId }
        ... on Bot { databaseId }
        ... on Organization { databaseId }
        ... on Mannequin { databaseId }
        ... on EnterpriseUserAccount { databaseId }
      }
      performedViaGithubApp { databaseId }
      issue { id }
      pullRequest { id }
      repository { id nameWithOwner }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            repository: Option<Repo>,
        }
        #[derive(Debug, Deserialize)]
        struct Repo {
            issues: Connection<IssueData>,
        }
        let issues = self
            .fetch_paginated(
                query,
                json!({ "owner": owner, "name": name }),
                |data: Resp| data.repository.map(|r| r.issues),
            )
            .await?;

        let mut out = Vec::with_capacity(issues.len());
        for issue in issues {
            out.push(self.with_paginated_issue(issue).await?);
        }
        Ok(out)
    }

    pub async fn fetch_all_pull_requests(&self, repo: &str) -> Result<Vec<PullRequestData>> {
        let (owner, name) = split_repo(repo)?;
        let query = r#"
query($owner: String!, $name: String!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: 100, after: $cursor, orderBy: {field: UPDATED_AT, direction: DESC}) {
      pageInfo { hasNextPage endCursor }
      nodes { ...PullRequestFields }
    }
  }
}

fragment PullRequestFields on PullRequest {
  id
  number
  title
  body
  state
  createdAt
  updatedAt
  closedAt
  mergedAt
  url
  isDraft
  headRefName
  baseRefName
  author { login }
  repository { id nameWithOwner }
  assignees(first: 20) { pageInfo { hasNextPage endCursor } nodes { id login } }
  labels(first: 20) { pageInfo { hasNextPage endCursor } nodes { id name } }
  comments(first: 50) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id
      body
      createdAt
      updatedAt
      url
      isMinimized
      author {
        __typename
        id
        login
        ... on User { databaseId }
        ... on Bot { databaseId }
        ... on Organization { databaseId }
        ... on Mannequin { databaseId }
        ... on EnterpriseUserAccount { databaseId }
      }
      performedViaGithubApp { databaseId }
      issue { id }
      pullRequest { id }
      repository { id nameWithOwner }
    }
  }
  reviews(first: 50) {
    pageInfo { hasNextPage endCursor }
    nodes {
      id
      state
      body
      createdAt
      updatedAt
      url
      author {
        __typename
        id
        login
        ... on User { databaseId }
        ... on Bot { databaseId }
        ... on Organization { databaseId }
        ... on Mannequin { databaseId }
        ... on EnterpriseUserAccount { databaseId }
      }
      performedViaGithubApp { databaseId }
      pullRequest { id repository { id nameWithOwner } }
      comments(first: 50) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          body
          path
          position
          line
          diffHunk
          createdAt
          updatedAt
          url
          author {
            __typename
            id
            login
            ... on User { databaseId }
            ... on Bot { databaseId }
            ... on Organization { databaseId }
            ... on Mannequin { databaseId }
            ... on EnterpriseUserAccount { databaseId }
          }
          performedViaGithubApp { databaseId }
          repository { id nameWithOwner }
          pullRequestReview { id pullRequest { id repository { id nameWithOwner } } }
        }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            repository: Option<Repo>,
        }
        #[derive(Debug, Deserialize)]
        struct Repo {
            #[serde(rename = "pullRequests")]
            pull_requests: Connection<PullRequestData>,
        }
        let pull_requests = self
            .fetch_paginated(
                query,
                json!({ "owner": owner, "name": name }),
                |data: Resp| data.repository.map(|r| r.pull_requests),
            )
            .await?;

        let mut out = Vec::with_capacity(pull_requests.len());
        for pr in pull_requests {
            out.push(self.with_paginated_pull_request(pr).await?);
        }
        Ok(out)
    }

    pub async fn fetch_project_items(&self, project: &ProjectSpec) -> Result<Vec<ProjectItemData>> {
        let Some(project_data) = self
            .fetch_project_by_owner_number(&project.owner, project.number)
            .await?
        else {
            return Ok(Vec::new());
        };
        Ok(project_data.items.nodes)
    }

    async fn with_paginated_project_items(&self, mut project: ProjectData) -> Result<ProjectData> {
        let mut all_items = self
            .with_paginated_project_item_field_values_batch(project.items.nodes)
            .await?;
        let mut cursor = next_cursor_or_done(&project.items.page_info, "Project items first page")?;

        while let Some(next_cursor) = cursor {
            let page = self
                .fetch_project_items_page(&project.id, Some(next_cursor))
                .await
                .with_context(|| {
                    format!("Failed fetching paginated project items for {}", project.id)
                })?;
            let mut page_items = self
                .with_paginated_project_item_field_values_batch(page.nodes)
                .await?;
            all_items.append(&mut page_items);
            cursor = next_cursor_or_done(&page.page_info, "Project items next page")?;
        }

        project.items = Connection {
            nodes: all_items,
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        };
        Ok(project)
    }

    async fn fetch_project_items_page(
        &self,
        project_id: &str,
        cursor: Option<String>,
    ) -> Result<Connection<ProjectItemData>> {
        let query = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    __typename
    ... on ProjectV2 {
      items(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { ...ProjectItemFields }
      }
    }
  }
}

fragment ProjectItemFields on ProjectV2Item {
  id
  type
  createdAt
  updatedAt
  project { id number owner { login } }
  content {
    __typename
    ... on Issue { id number title state repository { id nameWithOwner } }
    ... on PullRequest { id number title state repository { id nameWithOwner } }
    ... on DraftIssue { id title body }
  }
  fieldValues(first: 50) {
    pageInfo { hasNextPage endCursor }
    nodes {
      __typename
      ... on ProjectV2ItemFieldSingleSelectValue {
        name
        field { ... on ProjectV2SingleSelectField { id name } }
        optionId
      }
      ... on ProjectV2ItemFieldTextValue {
        text
        field { ... on ProjectV2FieldCommon { id name } }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            ProjectV2 { items: Connection<ProjectItemData> },
        }

        let variables = json!({ "id": project_id, "cursor": cursor });
        let data: Resp = self.execute(query, variables).await?;
        match data.node {
            Some(Node::ProjectV2 { items }) => Ok(items),
            None => Err(anyhow!(
                "Project {project_id} disappeared after first page while paginating items"
            )),
        }
    }

    async fn with_paginated_issue(&self, mut issue: IssueData) -> Result<IssueData> {
        let issue_id = issue.id.clone();
        let assignees_conn = issue.assignees;
        let mut assignees = assignees_conn.nodes;
        let mut cursor =
            next_cursor_or_done(&assignees_conn.page_info, "Issue assignees first page")?;
        while let Some(next_cursor) = cursor {
            let page = self
                .fetch_issue_assignees_page(&issue_id, Some(next_cursor))
                .await?;
            assignees.extend(page.nodes);
            cursor = next_cursor_or_done(&page.page_info, "Issue assignees next page")?;
        }
        issue.assignees = Connection {
            nodes: assignees,
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        };

        let labels_conn = issue.labels;
        let mut labels = labels_conn.nodes;
        let mut cursor = next_cursor_or_done(&labels_conn.page_info, "Issue labels first page")?;
        while let Some(next_cursor) = cursor {
            let page = self
                .fetch_issue_labels_page(&issue_id, Some(next_cursor))
                .await?;
            labels.extend(page.nodes);
            cursor = next_cursor_or_done(&page.page_info, "Issue labels next page")?;
        }
        issue.labels = Connection {
            nodes: labels,
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        };

        let comments_conn = issue.comments;
        let mut comments = comments_conn.nodes;
        let mut cursor =
            next_cursor_or_done(&comments_conn.page_info, "Issue comments first page")?;
        while let Some(next_cursor) = cursor {
            let page = self
                .fetch_issue_comments_page(&issue_id, Some(next_cursor))
                .await?;
            comments.extend(page.nodes);
            cursor = next_cursor_or_done(&page.page_info, "Issue comments next page")?;
        }
        issue.comments = Connection {
            nodes: comments,
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        };
        Ok(issue)
    }

    async fn with_paginated_pull_request(
        &self,
        mut pr: PullRequestData,
    ) -> Result<PullRequestData> {
        let pr_id = pr.id.clone();
        let assignees_conn = pr.assignees;
        let mut assignees = assignees_conn.nodes;
        let mut cursor = next_cursor_or_done(
            &assignees_conn.page_info,
            "Pull request assignees first page",
        )?;
        while let Some(next_cursor) = cursor {
            let page = self
                .fetch_pull_request_assignees_page(&pr_id, Some(next_cursor))
                .await?;
            assignees.extend(page.nodes);
            cursor = next_cursor_or_done(&page.page_info, "Pull request assignees next page")?;
        }
        pr.assignees = Connection {
            nodes: assignees,
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        };

        let labels_conn = pr.labels;
        let mut labels = labels_conn.nodes;
        let mut cursor =
            next_cursor_or_done(&labels_conn.page_info, "Pull request labels first page")?;
        while let Some(next_cursor) = cursor {
            let page = self
                .fetch_pull_request_labels_page(&pr_id, Some(next_cursor))
                .await?;
            labels.extend(page.nodes);
            cursor = next_cursor_or_done(&page.page_info, "Pull request labels next page")?;
        }
        pr.labels = Connection {
            nodes: labels,
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        };

        let comments_conn = pr.comments;
        let mut comments = comments_conn.nodes;
        let mut cursor =
            next_cursor_or_done(&comments_conn.page_info, "Pull request comments first page")?;
        while let Some(next_cursor) = cursor {
            let page = self
                .fetch_pull_request_comments_page(&pr_id, Some(next_cursor))
                .await?;
            comments.extend(page.nodes);
            cursor = next_cursor_or_done(&page.page_info, "Pull request comments next page")?;
        }
        pr.comments = Connection {
            nodes: comments,
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        };

        let reviews_conn = pr.reviews;
        let mut reviews = Vec::new();
        for review in reviews_conn.nodes {
            reviews.push(self.with_paginated_review(review).await?);
        }
        let mut cursor =
            next_cursor_or_done(&reviews_conn.page_info, "Pull request reviews first page")?;
        while let Some(next_cursor) = cursor {
            let page = self
                .fetch_pull_request_reviews_page(&pr_id, Some(next_cursor))
                .await?;
            for review in page.nodes {
                reviews.push(self.with_paginated_review(review).await?);
            }
            cursor = next_cursor_or_done(&page.page_info, "Pull request reviews next page")?;
        }
        pr.reviews = Connection {
            nodes: reviews,
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        };

        Ok(pr)
    }

    async fn with_paginated_review(
        &self,
        mut review: PullRequestReviewData,
    ) -> Result<PullRequestReviewData> {
        let review_id = review.id.clone();
        let comments_conn = review.comments;
        let mut comments = comments_conn.nodes;
        let mut cursor = next_cursor_or_done(
            &comments_conn.page_info,
            "Pull request review comments first page",
        )?;
        while let Some(next_cursor) = cursor {
            let page = self
                .fetch_pull_request_review_comments_page(&review_id, Some(next_cursor))
                .await?;
            comments.extend(page.nodes);
            cursor =
                next_cursor_or_done(&page.page_info, "Pull request review comments next page")?;
        }
        review.comments = Connection {
            nodes: comments,
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        };
        Ok(review)
    }

    async fn with_paginated_project_item_field_values_batch(
        &self,
        items: Vec<ProjectItemData>,
    ) -> Result<Vec<ProjectItemData>> {
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            out.push(self.with_paginated_project_item_field_values(item).await?);
        }
        Ok(out)
    }

    async fn with_paginated_project_item_field_values(
        &self,
        mut item: ProjectItemData,
    ) -> Result<ProjectItemData> {
        let item_id = item.id.clone();
        let values_conn = item.field_values;
        let mut values = values_conn.nodes;
        let mut cursor = next_cursor_or_done(
            &values_conn.page_info,
            "Project item field values first page",
        )?;
        while let Some(next_cursor) = cursor {
            let page = self
                .fetch_project_item_field_values_page(&item_id, Some(next_cursor))
                .await?;
            values.extend(page.nodes);
            cursor = next_cursor_or_done(&page.page_info, "Project item field values next page")?;
        }
        item.field_values = Connection {
            nodes: values,
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        };
        Ok(item)
    }

    async fn fetch_issue_assignees_page(
        &self,
        issue_id: &str,
        cursor: Option<String>,
    ) -> Result<Connection<UserRef>> {
        let query = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    __typename
    ... on Issue {
      assignees(first: 20, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { id login }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            Issue { assignees: Connection<UserRef> },
        }
        let data: Resp = self
            .execute(query, json!({ "id": issue_id, "cursor": cursor }))
            .await?;
        match data.node {
            Some(Node::Issue { assignees }) => Ok(assignees),
            None => Err(anyhow!(
                "Issue {issue_id} disappeared after first page while paginating assignees"
            )),
        }
    }

    async fn fetch_issue_labels_page(
        &self,
        issue_id: &str,
        cursor: Option<String>,
    ) -> Result<Connection<LabelRef>> {
        let query = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    __typename
    ... on Issue {
      labels(first: 20, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { id name }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            Issue { labels: Connection<LabelRef> },
        }
        let data: Resp = self
            .execute(query, json!({ "id": issue_id, "cursor": cursor }))
            .await?;
        match data.node {
            Some(Node::Issue { labels }) => Ok(labels),
            None => Err(anyhow!(
                "Issue {issue_id} disappeared after first page while paginating labels"
            )),
        }
    }

    async fn fetch_issue_comments_page(
        &self,
        issue_id: &str,
        cursor: Option<String>,
    ) -> Result<Connection<IssueCommentData>> {
        let query = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    __typename
    ... on Issue {
      comments(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          body
          createdAt
          updatedAt
          url
          isMinimized
          author {
            __typename
            id
            login
            ... on User { databaseId }
            ... on Bot { databaseId }
            ... on Organization { databaseId }
            ... on Mannequin { databaseId }
            ... on EnterpriseUserAccount { databaseId }
          }
          performedViaGithubApp { databaseId }
          issue { id }
          pullRequest { id }
          repository { id nameWithOwner }
        }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            Issue {
                comments: Connection<IssueCommentData>,
            },
        }
        let data: Resp = self
            .execute(query, json!({ "id": issue_id, "cursor": cursor }))
            .await?;
        match data.node {
            Some(Node::Issue { comments }) => Ok(comments),
            None => Err(anyhow!(
                "Issue {issue_id} disappeared after first page while paginating comments"
            )),
        }
    }

    async fn fetch_pull_request_assignees_page(
        &self,
        pull_request_id: &str,
        cursor: Option<String>,
    ) -> Result<Connection<UserRef>> {
        let query = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    __typename
    ... on PullRequest {
      assignees(first: 20, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { id login }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            PullRequest { assignees: Connection<UserRef> },
        }
        let data: Resp = self
            .execute(query, json!({ "id": pull_request_id, "cursor": cursor }))
            .await?;
        match data.node {
            Some(Node::PullRequest { assignees }) => Ok(assignees),
            None => Err(anyhow!(
                "Pull request {pull_request_id} disappeared after first page while paginating assignees"
            )),
        }
    }

    async fn fetch_pull_request_labels_page(
        &self,
        pull_request_id: &str,
        cursor: Option<String>,
    ) -> Result<Connection<LabelRef>> {
        let query = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    __typename
    ... on PullRequest {
      labels(first: 20, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes { id name }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            PullRequest { labels: Connection<LabelRef> },
        }
        let data: Resp = self
            .execute(query, json!({ "id": pull_request_id, "cursor": cursor }))
            .await?;
        match data.node {
            Some(Node::PullRequest { labels }) => Ok(labels),
            None => Err(anyhow!(
                "Pull request {pull_request_id} disappeared after first page while paginating labels"
            )),
        }
    }

    async fn fetch_pull_request_comments_page(
        &self,
        pull_request_id: &str,
        cursor: Option<String>,
    ) -> Result<Connection<IssueCommentData>> {
        let query = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    __typename
    ... on PullRequest {
      comments(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          body
          createdAt
          updatedAt
          url
          isMinimized
          author {
            __typename
            id
            login
            ... on User { databaseId }
            ... on Bot { databaseId }
            ... on Organization { databaseId }
            ... on Mannequin { databaseId }
            ... on EnterpriseUserAccount { databaseId }
          }
          performedViaGithubApp { databaseId }
          issue { id }
          pullRequest { id }
          repository { id nameWithOwner }
        }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            PullRequest {
                comments: Connection<IssueCommentData>,
            },
        }
        let data: Resp = self
            .execute(query, json!({ "id": pull_request_id, "cursor": cursor }))
            .await?;
        match data.node {
            Some(Node::PullRequest { comments }) => Ok(comments),
            None => Err(anyhow!(
                "Pull request {pull_request_id} disappeared after first page while paginating comments"
            )),
        }
    }

    async fn fetch_pull_request_reviews_page(
        &self,
        pull_request_id: &str,
        cursor: Option<String>,
    ) -> Result<Connection<PullRequestReviewData>> {
        let query = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    __typename
    ... on PullRequest {
      reviews(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          state
          body
          createdAt
          updatedAt
          url
          author {
            __typename
            id
            login
            ... on User { databaseId }
            ... on Bot { databaseId }
            ... on Organization { databaseId }
            ... on Mannequin { databaseId }
            ... on EnterpriseUserAccount { databaseId }
          }
          performedViaGithubApp { databaseId }
          pullRequest { id repository { id nameWithOwner } }
          comments(first: 100) {
            pageInfo { hasNextPage endCursor }
            nodes {
              id
              body
              path
              position
              line
              diffHunk
              createdAt
              updatedAt
              url
              pullRequestReview { id pullRequest { id repository { id nameWithOwner } } }
              author {
                __typename
                id
                login
                ... on User { databaseId }
                ... on Bot { databaseId }
                ... on Organization { databaseId }
                ... on Mannequin { databaseId }
                ... on EnterpriseUserAccount { databaseId }
              }
              performedViaGithubApp { databaseId }
              repository { id nameWithOwner }
            }
          }
        }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            PullRequest {
                reviews: Connection<PullRequestReviewData>,
            },
        }
        let data: Resp = self
            .execute(query, json!({ "id": pull_request_id, "cursor": cursor }))
            .await?;
        match data.node {
            Some(Node::PullRequest { reviews }) => Ok(reviews),
            None => Err(anyhow!(
                "Pull request {pull_request_id} disappeared after first page while paginating reviews"
            )),
        }
    }

    async fn fetch_pull_request_review_comments_page(
        &self,
        review_id: &str,
        cursor: Option<String>,
    ) -> Result<Connection<PullRequestReviewCommentData>> {
        let query = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    __typename
    ... on PullRequestReview {
      comments(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          body
          path
          position
          line
          diffHunk
          createdAt
          updatedAt
          url
          author {
            __typename
            id
            login
            ... on User { databaseId }
            ... on Bot { databaseId }
            ... on Organization { databaseId }
            ... on Mannequin { databaseId }
            ... on EnterpriseUserAccount { databaseId }
          }
          performedViaGithubApp { databaseId }
          repository { id nameWithOwner }
          pullRequestReview { id pullRequest { id repository { id nameWithOwner } } }
        }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            PullRequestReview {
                comments: Connection<PullRequestReviewCommentData>,
            },
        }
        let data: Resp = self
            .execute(query, json!({ "id": review_id, "cursor": cursor }))
            .await?;
        match data.node {
            Some(Node::PullRequestReview { comments }) => Ok(comments),
            None => Err(anyhow!(
                "Pull request review {review_id} disappeared after first page while paginating comments"
            )),
        }
    }

    async fn fetch_project_item_field_values_page(
        &self,
        item_id: &str,
        cursor: Option<String>,
    ) -> Result<Connection<ProjectItemFieldValue>> {
        let query = r#"
query($id: ID!, $cursor: String) {
  node(id: $id) {
    __typename
    ... on ProjectV2Item {
      fieldValues(first: 50, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          __typename
          ... on ProjectV2ItemFieldSingleSelectValue {
            name
            field { ... on ProjectV2SingleSelectField { id name } }
            optionId
          }
          ... on ProjectV2ItemFieldTextValue {
            text
            field { ... on ProjectV2FieldCommon { id name } }
          }
        }
      }
    }
  }
}
"#;
        #[derive(Debug, Deserialize)]
        struct Resp {
            node: Option<Node>,
        }
        #[derive(Debug, Deserialize)]
        #[serde(tag = "__typename")]
        enum Node {
            ProjectV2Item {
                #[serde(rename = "fieldValues")]
                field_values: Connection<ProjectItemFieldValue>,
            },
        }
        let data: Resp = self
            .execute(query, json!({ "id": item_id, "cursor": cursor }))
            .await?;
        match data.node {
            Some(Node::ProjectV2Item { field_values }) => Ok(field_values),
            None => Err(anyhow!(
                "Project item {item_id} disappeared after first page while paginating field values"
            )),
        }
    }

    async fn fetch_paginated<T, D, F>(
        &self,
        query: &str,
        base_vars: serde_json::Value,
        select_conn: F,
    ) -> Result<Vec<T>>
    where
        T: DeserializeOwned,
        D: DeserializeOwned,
        F: Fn(D) -> Option<Connection<T>>,
    {
        let mut cursor: Option<String> = None;
        let mut out = Vec::new();
        let mut page = 0usize;
        loop {
            let mut vars = base_vars.clone();
            if let Some(map) = vars.as_object_mut() {
                map.insert(
                    "cursor".to_string(),
                    cursor
                        .clone()
                        .map(serde_json::Value::String)
                        .unwrap_or(serde_json::Value::Null),
                );
            }
            let data: D = self.execute(query, vars).await?;
            let Some(conn) = select_conn(data) else {
                if page == 0 {
                    break;
                }
                return Err(anyhow!(
                    "Pagination root disappeared after first page for query selection"
                ));
            };
            out.extend(conn.nodes);
            cursor = next_cursor_or_done(&conn.page_info, "Top-level pagination page")?;
            if cursor.is_none() {
                break;
            }
            page = page.saturating_add(1);
        }
        Ok(out)
    }

    async fn execute<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
    ) -> Result<T> {
        self.execute_with_nullable_lookups(query, variables, None)
            .await
    }

    async fn execute_nullable_lookup<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
        nullable_path: &str,
    ) -> Result<T> {
        self.execute_with_nullable_lookups(
            query,
            variables,
            Some(std::slice::from_ref(&nullable_path)),
        )
        .await
    }

    async fn execute_nullable_lookups<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
        nullable_paths: &[&str],
    ) -> Result<T> {
        self.execute_with_nullable_lookups(query, variables, Some(nullable_paths))
            .await
    }

    async fn execute_with_nullable_lookups<T: DeserializeOwned>(
        &self,
        query: &str,
        variables: serde_json::Value,
        nullable_paths: Option<&[&str]>,
    ) -> Result<T> {
        let body = json!({ "query": query, "variables": variables });
        const MAX_RETRIES: u32 = 7;

        let mut attempt = 0u32;
        loop {
            let response = match self
                .client
                .post(&self.graphql_url)
                .headers(self.default_headers.clone())
                .json(&body)
                .send()
                .await
            {
                Ok(response) => response,
                Err(err) => {
                    if attempt >= MAX_RETRIES || !is_retryable_transport_error(&err) {
                        return Err(err).context("GitHub GraphQL request failed");
                    }
                    sleep(exp_backoff(attempt)).await;
                    attempt = attempt.saturating_add(1);
                    continue;
                }
            };

            if response.status().is_success() {
                let envelope: GraphQlEnvelope<T> = response
                    .json()
                    .await
                    .context("Failed to decode GraphQL response")?;
                if let Some(errors) = envelope.errors {
                    if errors.is_empty() {
                        return envelope
                            .data
                            .ok_or_else(|| anyhow!("GraphQL response missing data"));
                    }

                    if nullable_paths
                        .is_some_and(|paths| graphql_errors_are_lookup_not_found(&errors, paths))
                    {
                        return envelope.data.ok_or_else(|| {
                            anyhow!("GraphQL nullable lookup response missing data")
                        });
                    }

                    if attempt < MAX_RETRIES && graphql_errors_retryable(&errors) {
                        sleep(exp_backoff(attempt)).await;
                        attempt = attempt.saturating_add(1);
                        continue;
                    }

                    return Err(anyhow!(
                        "GitHub GraphQL returned errors: {}",
                        summarize_graphql_errors(&errors)
                    ));
                }
                return envelope
                    .data
                    .ok_or_else(|| anyhow!("GraphQL response missing data"));
            }

            let status = response.status();
            let headers = response.headers().clone();
            let body_text = response.text().await.unwrap_or_default();
            let mut decision = classify_retry(status, &headers, attempt);
            if status == reqwest::StatusCode::FORBIDDEN && forbidden_body_retryable(&body_text) {
                decision.retryable = true;
                if decision.delay.is_zero() {
                    decision.delay = exp_backoff(attempt);
                }
            }
            if !decision.retryable || attempt >= MAX_RETRIES {
                return Err(anyhow!(
                    "GitHub GraphQL request failed: status={status}, body={body_text}"
                ));
            }

            sleep(decision.delay).await;
            attempt = attempt.saturating_add(1);
        }
    }
}

fn split_repo(input: &str) -> Result<(String, String)> {
    let mut parts = input.split('/');
    let owner = parts
        .next()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("Invalid repository '{input}'"))?;
    let name = parts
        .next()
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| anyhow!("Invalid repository '{input}'"))?;
    if parts.next().is_some() {
        return Err(anyhow!("Invalid repository '{input}'"));
    }
    Ok((owner.to_string(), name.to_string()))
}

fn next_cursor_or_done(page_info: &PageInfo, context: &str) -> Result<Option<String>> {
    if !page_info.has_next_page {
        return Ok(None);
    }

    page_info
        .end_cursor
        .clone()
        .map(Some)
        .ok_or_else(|| anyhow!("{context}: received hasNextPage=true but endCursor was absent"))
}

fn is_retryable_transport_error(error: &reqwest::Error) -> bool {
    error.is_timeout() || error.is_connect() || error.is_request()
}

fn forbidden_body_retryable(body: &str) -> bool {
    let normalized = body.to_ascii_lowercase();
    normalized.contains("rate limit")
        || normalized.contains("secondary rate limit")
        || normalized.contains("abuse detection")
}

fn graphql_errors_retryable(errors: &[GraphQlError]) -> bool {
    !errors.is_empty() && errors.iter().all(is_retryable_graphql_error)
}

fn graphql_errors_are_lookup_not_found(errors: &[GraphQlError], expected_paths: &[&str]) -> bool {
    !errors.is_empty()
        && errors.iter().all(|error| {
            graphql_error_kind(error).is_some_and(|kind| kind.eq_ignore_ascii_case("NOT_FOUND"))
                && matches!(
                    error.path.as_deref(),
                    Some([serde_json::Value::String(path)])
                        if expected_paths.iter().any(|expected| path == expected)
                )
        })
}

fn is_retryable_graphql_error(error: &GraphQlError) -> bool {
    let message = error.message.to_ascii_lowercase();
    if message.contains("rate limit")
        || message.contains("secondary rate limit")
        || message.contains("abuse detection")
        || message.contains("temporarily unavailable")
        || message.contains("service unavailable")
        || message.contains("internal server error")
        || message.contains("please try again")
        || message.contains("timeout")
    {
        return true;
    }

    let kind = graphql_error_kind(error);

    matches!(
        kind.map(|v| v.to_ascii_uppercase()),
        Some(kind)
            if kind.contains("RATE_LIMIT")
                || kind.contains("ABUSE")
                || kind.contains("SERVICE_UNAVAILABLE")
                || kind.contains("INTERNAL")
                || kind.contains("TIMEOUT")
                || kind.contains("TRANSIENT")
    )
}

fn graphql_error_kind(error: &GraphQlError) -> Option<&str> {
    error
        .error_type
        .as_deref()
        .or_else(|| {
            error
                .extensions
                .as_ref()
                .and_then(|ext| ext.get("type"))
                .and_then(|value| value.as_str())
        })
        .or_else(|| {
            error
                .extensions
                .as_ref()
                .and_then(|ext| ext.get("code"))
                .and_then(|value| value.as_str())
        })
}

fn summarize_graphql_errors(errors: &[GraphQlError]) -> String {
    errors
        .iter()
        .map(|error| error.message.clone())
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OwnerRef {
    pub login: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepositoryData {
    pub id: String,
    pub name: String,
    #[serde(rename = "nameWithOwner")]
    pub name_with_owner: String,
    pub owner: OwnerRef,
    pub description: Option<String>,
    pub url: String,
    #[serde(rename = "isArchived")]
    pub is_archived: bool,
    #[serde(rename = "isPrivate")]
    pub is_private: bool,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "defaultBranchRef")]
    pub default_branch_ref: Option<BranchRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BranchRef {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActorRef {
    pub id: Option<String>,
    pub login: Option<String>,
    #[serde(rename = "__typename")]
    pub actor_type: Option<String>,
    #[serde(rename = "databaseId")]
    pub database_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitHubAppRef {
    #[serde(rename = "databaseId")]
    pub database_id: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectIdentityRef {
    pub id: String,
    pub number: i64,
    pub owner: OwnerRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LabelRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UserRef {
    pub id: String,
    pub login: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IssueData {
    pub id: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "closedAt")]
    pub closed_at: Option<String>,
    pub url: String,
    pub author: Option<OwnerRef>,
    pub repository: RepositoryRef,
    pub assignees: Connection<UserRef>,
    pub labels: Connection<LabelRef>,
    pub comments: Connection<IssueCommentData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PullRequestData {
    pub id: String,
    pub number: i64,
    pub title: String,
    pub body: Option<String>,
    pub state: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "closedAt")]
    pub closed_at: Option<String>,
    #[serde(rename = "mergedAt")]
    pub merged_at: Option<String>,
    pub url: String,
    #[serde(rename = "isDraft")]
    pub is_draft: bool,
    #[serde(rename = "headRefName")]
    pub head_ref_name: Option<String>,
    #[serde(rename = "baseRefName")]
    pub base_ref_name: Option<String>,
    pub author: Option<OwnerRef>,
    pub repository: RepositoryRef,
    pub assignees: Connection<UserRef>,
    pub labels: Connection<LabelRef>,
    pub comments: Connection<IssueCommentData>,
    pub reviews: Connection<PullRequestReviewData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RepositoryRef {
    pub id: String,
    #[serde(rename = "nameWithOwner")]
    pub name_with_owner: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IssueCommentData {
    pub id: String,
    pub body: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    pub url: String,
    #[serde(rename = "isMinimized")]
    pub is_minimized: bool,
    pub author: Option<ActorRef>,
    #[serde(rename = "performedViaGithubApp")]
    pub performed_via_github_app: Option<GitHubAppRef>,
    pub issue: Option<NodeIdRef>,
    #[serde(rename = "pullRequest")]
    pub pull_request: Option<NodeIdRef>,
    pub repository: RepositoryRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PullRequestReviewData {
    pub id: String,
    pub state: String,
    pub body: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    pub url: String,
    pub author: Option<ActorRef>,
    #[serde(rename = "performedViaGithubApp")]
    pub performed_via_github_app: Option<GitHubAppRef>,
    #[serde(rename = "pullRequest")]
    pub pull_request: PullRequestRef,
    pub comments: Connection<PullRequestReviewCommentData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PullRequestReviewCommentData {
    pub id: String,
    pub body: Option<String>,
    pub path: Option<String>,
    pub position: Option<i64>,
    pub line: Option<i64>,
    #[serde(rename = "diffHunk")]
    pub diff_hunk: Option<String>,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    pub url: String,
    pub author: Option<ActorRef>,
    #[serde(rename = "performedViaGithubApp")]
    pub performed_via_github_app: Option<GitHubAppRef>,
    pub repository: RepositoryRef,
    #[serde(rename = "pullRequestReview")]
    pub pull_request_review: PullRequestReviewRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PullRequestReviewRef {
    pub id: String,
    #[serde(rename = "pullRequest")]
    pub pull_request: PullRequestRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PullRequestRef {
    pub id: String,
    pub repository: RepositoryRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct NodeIdRef {
    pub id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectData {
    pub id: String,
    pub title: String,
    pub number: i64,
    pub url: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    pub owner: OwnerRef,
    pub items: Connection<ProjectItemData>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectItemData {
    pub id: String,
    #[serde(rename = "type")]
    pub item_type: String,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    pub project: ProjectIdentityRef,
    pub content: Option<ProjectItemContent>,
    #[serde(rename = "fieldValues")]
    pub field_values: Connection<ProjectItemFieldValue>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "__typename")]
pub enum ProjectItemContent {
    Issue {
        id: String,
        number: i64,
        title: String,
        state: String,
        repository: RepositoryRef,
    },
    PullRequest {
        id: String,
        number: i64,
        title: String,
        state: String,
        repository: RepositoryRef,
    },
    DraftIssue {
        id: String,
        title: String,
        body: Option<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "__typename")]
pub enum ProjectItemFieldValue {
    ProjectV2ItemFieldSingleSelectValue {
        name: Option<String>,
        field: Option<ProjectFieldRef>,
        #[serde(rename = "optionId")]
        option_id: Option<String>,
    },
    ProjectV2ItemFieldTextValue {
        text: Option<String>,
        field: Option<ProjectFieldRef>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ProjectFieldRef {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(bound(deserialize = "T: serde::Deserialize<'de>"))]
pub struct Connection<T> {
    #[serde(default)]
    pub nodes: Vec<T>,
    #[serde(rename = "pageInfo")]
    pub page_info: PageInfo,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PageInfo {
    #[serde(rename = "hasNextPage")]
    pub has_next_page: bool,
    #[serde(rename = "endCursor")]
    pub end_cursor: Option<String>,
}

impl<T> Default for Connection<T> {
    fn default() -> Self {
        Self {
            nodes: Vec::new(),
            page_info: PageInfo {
                has_next_page: false,
                end_cursor: None,
            },
        }
    }
}

/// Bundle used during full-reconcile passes.
#[derive(Debug, Clone, Default)]
pub struct ReconcileSnapshot {
    pub repositories: HashMap<String, RepositoryData>,
    pub issues: HashMap<String, IssueData>,
    pub pull_requests: HashMap<String, PullRequestData>,
    pub issue_comments: HashMap<String, IssueCommentData>,
    pub reviews: HashMap<String, PullRequestReviewData>,
    pub review_comments: HashMap<String, PullRequestReviewCommentData>,
    pub projects: HashMap<String, ProjectData>,
    pub project_items: HashMap<String, ProjectItemData>,
}

impl GitHubGraphQLClient {
    pub async fn fetch_reconcile_snapshot(
        &self,
        repos: &[String],
        projects: &[ProjectSpec],
    ) -> Result<ReconcileSnapshot> {
        let mut snapshot = ReconcileSnapshot::default();

        for repo in repos {
            let (owner, name) = split_repo(repo)?;
            let Some(repository) = self.fetch_repository(&owner, &name).await? else {
                continue;
            };
            snapshot
                .repositories
                .insert(repository.id.clone(), repository.clone());

            for issue in self.fetch_all_issues(repo).await? {
                snapshot.issues.insert(issue.id.clone(), issue.clone());
                for comment in &issue.comments.nodes {
                    snapshot
                        .issue_comments
                        .insert(comment.id.clone(), comment.clone());
                }
            }

            for pr in self.fetch_all_pull_requests(repo).await? {
                snapshot.pull_requests.insert(pr.id.clone(), pr.clone());
                for comment in &pr.comments.nodes {
                    snapshot
                        .issue_comments
                        .insert(comment.id.clone(), comment.clone());
                }
                for review in &pr.reviews.nodes {
                    snapshot.reviews.insert(review.id.clone(), review.clone());
                    for review_comment in &review.comments.nodes {
                        snapshot
                            .review_comments
                            .insert(review_comment.id.clone(), review_comment.clone());
                    }
                }
            }
        }

        for project_spec in projects {
            if let Some(project) = self
                .fetch_project_by_owner_number(&project_spec.owner, project_spec.number)
                .await?
            {
                for item in &project.items.nodes {
                    snapshot.project_items.insert(item.id.clone(), item.clone());
                }
                snapshot.projects.insert(project.id.clone(), project);
            }
        }

        Ok(snapshot)
    }
}

#[derive(Debug, Deserialize)]
struct GraphQlEnvelope<T> {
    data: Option<T>,
    errors: Option<Vec<GraphQlError>>,
}

#[derive(Debug, Deserialize)]
struct GraphQlError {
    message: String,
    #[serde(rename = "type")]
    error_type: Option<String>,
    extensions: Option<serde_json::Value>,
    path: Option<Vec<serde_json::Value>>,
}
