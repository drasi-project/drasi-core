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

//! Wiremock-backed tests exercising the bootstrap provider end-to-end against
//! a fake GitHub GraphQL API. These prove:
//! - GraphQL cursor pagination is followed correctly.
//! - `BootstrapRequest` node/relation label filtering is honored.
//! - `event_count` is exact and `source_position` is always `None`.
//! - WorkGraph `Assignment`/comment/review reconstruction flows entirely
//!   through the shared `drasi_source_github_workgraph::mapping::Converter`
//!   (proving this crate does not duplicate any WorkGraph domain rule).

use crate::GitHubWorkGraphBootstrapProvider;
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::BootstrapEvent;
use drasi_source_github_workgraph::mapping::{NODE_ISSUE, NODE_ORGANIZATION};
use drasi_source_github_workgraph::workgraph::{assignment_element_id, comment_error_element_id};
use serde_json::{json, Value};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ORG_NODE_ID: &str = "O_org1";
const REPO_NODE_ID: &str = "R_repo1";
const ISSUE_NODE_ID: &str = "I_issue1";
const PR_NODE_ID: &str = "PR_pr1";
const RESULT_COMMENT_NODE_ID: &str = "IC_result";
const INVALID_COMMENT_NODE_ID: &str = "IC_invalid";
const ORDINARY_COMMENT_NODE_ID: &str = "IC_ordinary";

const ASSIGNMENT_COMMENT_BODY: &str = "WorkGraphAssignment/v1\n\
Please validate this issue.\n\
```json\n\
{\"assignmentId\":\"assign-1\",\"agentProfile\":\"triage-bot\",\"priority\":1,\"taskType\":\"issue-validation\",\"task\":{\"validationProfile\":\"default\",\"criteria\":[\"title-present\"]}}\n\
```";

const RESULT_COMMENT_BODY: &str = "WorkGraphResult/v1\n\
Validation complete.\n\
```json\n\
{\"assignmentId\":\"assign-1\",\"taskType\":\"issue-validation\",\"outcome\":\"succeeded\",\"summary\":\"Passed.\",\"result\":{\"criteria\":[{\"criterion\":\"title-present\",\"passed\":true,\"evidence\":\"Title is present.\"}]}}\n\
```";

fn org_fixture() -> Value {
    json!({
        "node_id": ORG_NODE_ID,
        "id": 1001,
        "login": "acme",
        "url": "https://github.com/acme",
        "avatar_url": "https://avatars.example/acme.png",
        "description": "Acme org",
    })
}

fn repo_fixture() -> Value {
    json!({
        "node_id": REPO_NODE_ID,
        "id": 2001,
        "name": "widgets",
        "full_name": "acme/widgets",
        "owner": { "login": "acme" },
        "description": "Widgets repo",
        "html_url": "https://github.com/acme/widgets",
        "private": false,
        "archived": false,
        "fork": false,
        "visibility": "PUBLIC",
        "created_at": "2024-01-01T00:00:00Z",
        "updated_at": "2024-01-02T00:00:00Z",
        "defaultBranchRef": { "name": "main" },
        "repositoryTopics": { "nodes": [] },
    })
}

fn issue_fixture() -> Value {
    json!({
        "node_id": ISSUE_NODE_ID,
        "id": 3001,
        "number": 42,
        "title": "Bug report",
        "body": "Something broke",
        "state": "OPEN",
        "state_reason": null,
        "locked": false,
        "created_at": "2024-01-03T00:00:00Z",
        "updated_at": "2024-01-03T01:00:00Z",
        "closed_at": null,
        "html_url": "https://github.com/acme/widgets/issues/42",
        "author_association": "MEMBER",
        "user": { "login": "ada", "node_id": "U_ada", "id": 5001, "type": "User" },
        "assignees": { "nodes": [{ "login": "ada" }] },
        "labels": {
            "pageInfo": { "hasNextPage": true, "endCursor": "LABEL_PAGE_2" },
            "nodes": [{ "name": "bug" }]
        },
        "comments": { "totalCount": 4 },
    })
}

fn result_comment_fixture() -> Value {
    let mut comment = assignment_comment_fixture();
    comment["node_id"] = json!(RESULT_COMMENT_NODE_ID);
    comment["id"] = json!(6002);
    comment["body"] = json!(RESULT_COMMENT_BODY);
    comment
}

fn invalid_comment_fixture() -> Value {
    let mut comment = assignment_comment_fixture();
    comment["node_id"] = json!(INVALID_COMMENT_NODE_ID);
    comment["id"] = json!(6003);
    comment["body"] = json!("WorkGraphResult/v1\nInvalid payload.\n```json\nnot-json\n```");
    comment
}

fn ordinary_comment_fixture() -> Value {
    let mut comment = assignment_comment_fixture();
    comment["node_id"] = json!(ORDINARY_COMMENT_NODE_ID);
    comment["id"] = json!(6004);
    comment["body"] = json!(" WorkGraphResult/v1\nLeading whitespace makes this ordinary.");
    comment
}

fn assignment_comment_fixture() -> Value {
    json!({
        "node_id": "IC_c1",
        "id": 6001,
        "body": ASSIGNMENT_COMMENT_BODY,
        "created_at": "2024-01-03T02:00:00Z",
        "updated_at": "2024-01-03T02:00:00Z",
        "html_url": "https://github.com/acme/widgets/issues/42#issuecomment-6001",
        "author_association": "MEMBER",
        "user": { "login": "triage-bot", "node_id": "U_bot", "type": "Bot" },
    })
}

fn pr_fixture() -> Value {
    json!({
        "node_id": PR_NODE_ID,
        "id": 7001,
        "number": 7,
        "title": "Add feature",
        "body": "This adds a feature",
        "state": "OPEN",
        "locked": false,
        "created_at": "2024-01-04T00:00:00Z",
        "updated_at": "2024-01-04T01:00:00Z",
        "closed_at": null,
        "html_url": "https://github.com/acme/widgets/pull/7",
        "author_association": "MEMBER",
        "user": { "login": "grace", "node_id": "U_grace", "id": 5003, "type": "User" },
        "assignees": { "nodes": [] },
        "labels": {
            "pageInfo": { "hasNextPage": false, "endCursor": null },
            "nodes": []
        },
        "draft": false,
        "merged": false,
        "merged_at": null,
        "head_ref_name": "feature-branch",
        "head_sha": "abcdef1",
        "base_ref_name": "main",
        "base_sha": "1234567",
        "comments": { "totalCount": 0 },
        "reviews": { "totalCount": 1 },
    })
}

fn review_fixture() -> Value {
    json!({
        "node_id": "PRR_r1",
        "id": 8001,
        "state": "APPROVED",
        "body": "Looks good",
        "submitted_at": "2024-01-04T02:00:00Z",
        "commit": { "oid": "abcdef1" },
        "html_url": "https://github.com/acme/widgets/pull/7#pullrequestreview-8001",
        "author_association": "MEMBER",
        "user": { "login": "bob", "node_id": "U_bob", "id": 5004, "type": "User" },
    })
}

fn connection(nodes: Vec<Value>, has_next: bool, end_cursor: Option<&str>) -> Value {
    json!({
        "pageInfo": { "hasNextPage": has_next, "endCursor": end_cursor },
        "nodes": nodes,
    })
}

async fn mount_query(
    server: &MockServer,
    must_contain: &[&str],
    data: Value,
    expect_calls: Option<u64>,
) {
    let mut mock = Mock::given(method("POST")).and(path("/graphql"));
    for needle in must_contain {
        mock = mock.and(body_string_contains(needle.to_string()));
    }
    let mock = mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": data })));
    let mock = match expect_calls {
        Some(n) => mock.expect(n),
        None => mock,
    };
    mock.mount(server).await;
}

/// Mounts one full single-repo/single-issue/single-PR/single-review fixture
/// set (the scenario used by most tests below).
async fn mount_full_fixture(server: &MockServer) {
    mount_query(
        server,
        &["avatar_url: avatarUrl"],
        json!({ "organization": org_fixture() }),
        None,
    )
    .await;
    mount_query(
        server,
        &["repositories(first"],
        json!({ "organization": { "repositories": connection(vec![repo_fixture()], false, None) } }),
        None,
    )
    .await;
    mount_query(
        server,
        &["issues(first"],
        json!({ "repository": { "issues": connection(vec![issue_fixture()], false, None) } }),
        None,
    )
    .await;
    mount_query(
        server,
        &["pullRequests(first"],
        json!({ "repository": { "pullRequests": connection(vec![pr_fixture()], false, None) } }),
        None,
    )
    .await;
    mount_query(
        server,
        &["labels(first: $pageSize", "\"cursor\":\"LABEL_PAGE_2\""],
        json!({ "node": { "labels": connection(
            vec![json!({ "name": "status:in-progress" })],
            false,
            None,
        ) } }),
        Some(1),
    )
    .await;
    mount_query(
        server,
        &["... on Issue {", "comments(first"],
        json!({ "node": { "comments": connection(vec![
            assignment_comment_fixture(),
            result_comment_fixture(),
            invalid_comment_fixture(),
            ordinary_comment_fixture(),
        ], false, None) } }),
        None,
    )
    .await;
    mount_query(
        server,
        &[
            "... on PullRequest {",
            "reviews(",
            "states: [COMMENTED, APPROVED, CHANGES_REQUESTED, DISMISSED]",
        ],
        json!({ "node": { "reviews": connection(vec![review_fixture()], false, None) } }),
        None,
    )
    .await;
}

fn provider_for(server: &MockServer) -> GitHubWorkGraphBootstrapProvider {
    GitHubWorkGraphBootstrapProvider::builder()
        .with_organization("acme")
        .with_token("read-only-test-token")
        .with_api_base_url(format!("{}/graphql", server.uri()))
        .with_max_concurrency(2)
        .build()
        .expect("valid config")
}

fn all_labels_request() -> BootstrapRequest {
    BootstrapRequest {
        query_id: "q1".to_string(),
        node_labels: Vec::new(),
        relation_labels: Vec::new(),
        request_id: "r1".to_string(),
    }
}

async fn run_bootstrap(
    provider: &GitHubWorkGraphBootstrapProvider,
    request: BootstrapRequest,
) -> (drasi_lib::bootstrap::BootstrapResult, Vec<BootstrapEvent>) {
    let context = BootstrapContext::new_minimal("test-server".to_string(), "gh-src".to_string());
    let (tx, mut rx) = tokio::sync::mpsc::channel(1024);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .expect("bootstrap should succeed");
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (result, events)
}

fn label_of(event: &BootstrapEvent) -> String {
    match &event.change {
        SourceChange::Insert { element } | SourceChange::Update { element } => element
            .get_metadata()
            .labels
            .first()
            .map(|l| l.to_string())
            .unwrap_or_default(),
        SourceChange::Delete { metadata } => metadata
            .labels
            .first()
            .map(|l| l.to_string())
            .unwrap_or_default(),
        SourceChange::Future { .. } => String::new(),
    }
}

#[tokio::test]
async fn bootstraps_full_repository_via_shared_converter() {
    let server = MockServer::start().await;
    mount_full_fixture(&server).await;
    let provider = provider_for(&server);

    let (result, events) = run_bootstrap(&provider, all_labels_request()).await;

    assert_eq!(
        result.source_position, None,
        "webhooks have no replay boundary"
    );
    assert_eq!(result.event_count, events.len());
    assert_eq!(result.event_count, 18, "unexpected event set: {events:#?}");
    assert!(
        events
            .iter()
            .all(|event| matches!(event.change, SourceChange::Insert { .. })),
        "bootstrap state must contain inserts only"
    );

    let org_events: Vec<_> = events
        .iter()
        .filter(|e| label_of(e) == NODE_ORGANIZATION)
        .collect();
    assert_eq!(org_events.len(), 1, "organization node must be deduped");
    assert!(
        matches!(org_events[0].change, SourceChange::Insert { .. }),
        "bootstrap snapshot must always Insert, even though Converter emits \
         the organization node as Update on every repository conversion"
    );

    let issue_event = events
        .iter()
        .find(|e| label_of(e) == NODE_ISSUE)
        .expect("issue node present");
    let SourceChange::Insert { element } = &issue_event.change else {
        panic!("issue must be an Insert");
    };
    assert_eq!(
        element.get_property("statusLabel"),
        &drasi_core::models::ElementValue::from(&json!("status:in-progress")),
        "status label must be derived by the shared mapping::derive_status, not reimplemented"
    );

    let assignment_id = assignment_element_id(ORG_NODE_ID, "assign-1");
    let assignment_event = events
        .iter()
        .find(|e| e.change.get_reference().element_id.as_ref() == assignment_id)
        .expect("WorkGraphAssignment node reconstructed via the shared workgraph parser");
    assert_eq!(label_of(assignment_event), "WorkGraphAssignment");

    let result_event = events
        .iter()
        .find(|event| event.change.get_reference().element_id.as_ref() == RESULT_COMMENT_NODE_ID)
        .expect("WorkGraphResult node reconstructed via the shared workgraph parser");
    assert_eq!(label_of(result_event), "WorkGraphResult");

    let result_for = events
        .iter()
        .find(|event| label_of(event) == "RESULT_FOR")
        .expect("RESULT_FOR relation reconstructed by the shared converter");
    let SourceChange::Insert {
        element: Element::Relation {
            in_node, out_node, ..
        },
    } = &result_for.change
    else {
        panic!("RESULT_FOR must be an inserted relation");
    };
    assert_eq!(in_node.element_id.as_ref(), RESULT_COMMENT_NODE_ID);
    assert_eq!(out_node.element_id.as_ref(), assignment_id);

    let error_id = comment_error_element_id(INVALID_COMMENT_NODE_ID);
    let error_event = events
        .iter()
        .find(|event| event.change.get_reference().element_id.as_ref() == error_id)
        .expect("recognized malformed WorkGraph comment becomes WorkGraphError");
    assert_eq!(label_of(error_event), "WorkGraphError");
    let SourceChange::Insert { element } = &error_event.change else {
        panic!("WorkGraphError must be inserted");
    };
    assert_eq!(
        element.get_property("errorCode"),
        &ElementValue::from(&json!("invalid-json"))
    );

    let ordinary_event = events
        .iter()
        .find(|event| event.change.get_reference().element_id.as_ref() == ORDINARY_COMMENT_NODE_ID)
        .expect("lookalike marker remains an ordinary comment");
    assert_eq!(label_of(ordinary_event), "GitHubIssueComment");

    let relation_labels: Vec<String> = events
        .iter()
        .filter(|e| {
            let l = label_of(e);
            drasi_source_github_workgraph::mapping::RELATION_LABELS.contains(&l.as_str())
        })
        .map(label_of)
        .collect();
    assert!(relation_labels.contains(&"IN_ORGANIZATION".to_string()));
    assert!(relation_labels.contains(&"IN_REPOSITORY".to_string()));
    assert!(relation_labels.contains(&"COMMENT_ON".to_string()));
    assert!(relation_labels.contains(&"REVIEW_OF".to_string()));
    assert!(relation_labels.contains(&"RESULT_FOR".to_string()));
}

#[tokio::test]
async fn filters_events_by_requested_node_labels() {
    let server = MockServer::start().await;
    mount_full_fixture(&server).await;
    let provider = provider_for(&server);

    let request = BootstrapRequest {
        query_id: "q2".to_string(),
        node_labels: vec![NODE_ISSUE.to_string()],
        relation_labels: Vec::new(),
        request_id: "r2".to_string(),
    };
    let (result, events) = run_bootstrap(&provider, request).await;

    assert_eq!(
        result.event_count, 1,
        "only the requested node label should be sent"
    );
    assert_eq!(label_of(&events[0]), NODE_ISSUE);
}

#[tokio::test]
async fn filters_events_by_requested_relation_labels() {
    let server = MockServer::start().await;
    mount_full_fixture(&server).await;
    let provider = provider_for(&server);

    let request = BootstrapRequest {
        query_id: "q3".to_string(),
        node_labels: Vec::new(),
        relation_labels: vec!["REVIEW_OF".to_string()],
        request_id: "r3".to_string(),
    };
    let (result, events) = run_bootstrap(&provider, request).await;

    assert_eq!(result.event_count, 1);
    assert_eq!(label_of(&events[0]), "REVIEW_OF");
}

#[tokio::test]
async fn paginates_repositories_across_multiple_pages() {
    let server = MockServer::start().await;
    mount_query(
        &server,
        &["avatar_url: avatarUrl"],
        json!({ "organization": org_fixture() }),
        None,
    )
    .await;

    let mut repo_page_2 = repo_fixture();
    repo_page_2["node_id"] = json!("R_repo2");
    repo_page_2["name"] = json!("gadgets");
    repo_page_2["full_name"] = json!("acme/gadgets");

    // Page 1: cursor is null, has a next page.
    mount_query(
        &server,
        &["repositories(first", "\"cursor\":null"],
        json!({ "organization": { "repositories": connection(vec![repo_fixture()], true, Some("PAGE2")) } }),
        Some(1),
    )
    .await;
    // Page 2: cursor is "PAGE2", no more pages.
    mount_query(
        &server,
        &["repositories(first", "\"cursor\":\"PAGE2\""],
        json!({ "organization": { "repositories": connection(vec![repo_page_2], false, None) } }),
        Some(1),
    )
    .await;

    // Both repositories have no issues/PRs, to keep this test focused on
    // repository-connection pagination.
    mount_query(
        &server,
        &["issues(first"],
        json!({ "repository": { "issues": connection(vec![], false, None) } }),
        None,
    )
    .await;
    mount_query(
        &server,
        &["pullRequests(first"],
        json!({ "repository": { "pullRequests": connection(vec![], false, None) } }),
        None,
    )
    .await;

    let provider = provider_for(&server);
    let (result, events) = run_bootstrap(&provider, all_labels_request()).await;

    // org(1) + 2x(repo + IN_ORGANIZATION) == 5
    assert_eq!(
        result.event_count, 5,
        "both pages of repositories must be processed: {events:#?}"
    );
    let repo_ids: std::collections::HashSet<String> = events
        .iter()
        .filter(|e| label_of(e) == "GitHubRepository")
        .map(|e| e.change.get_reference().element_id.to_string())
        .collect();
    assert_eq!(repo_ids.len(), 2);
    assert!(repo_ids.contains(REPO_NODE_ID));
    assert!(repo_ids.contains("R_repo2"));
}

#[tokio::test]
async fn fails_without_emitting_a_partial_repository_snapshot() {
    let server = MockServer::start().await;
    mount_query(
        &server,
        &["avatar_url: avatarUrl"],
        json!({ "organization": org_fixture() }),
        None,
    )
    .await;
    mount_query(
        &server,
        &["repositories(first"],
        json!({ "organization": { "repositories": connection(vec![repo_fixture()], false, None) } }),
        None,
    )
    .await;
    mount_query(
        &server,
        &["issues(first"],
        json!({ "repository": { "issues": connection(vec![], false, None) } }),
        None,
    )
    .await;
    mount_query(
        &server,
        &["pullRequests(first"],
        json!({ "repository": null }),
        None,
    )
    .await;

    let provider = provider_for(&server);
    let context = BootstrapContext::new_minimal("test-server".to_string(), "gh-src".to_string());
    let (tx, mut rx) = tokio::sync::mpsc::channel(32);
    let error = provider
        .bootstrap(all_labels_request(), &context, tx, None)
        .await
        .expect_err("a missing repository connection must fail bootstrap");

    assert!(error.to_string().contains("repository task"));
    assert!(
        rx.try_recv().is_err(),
        "no events may be sent from a partial snapshot"
    );
}
