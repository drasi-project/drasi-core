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

use crate::client::GitHubGraphQLClient;
use crate::{GitHubWorkGraphBootstrapProvider, WorkerFileLocation};
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::BootstrapEvent;
use drasi_source_github_workgraph::config::{LeaseTrust, TaskIssueType, TrustedIdentity};
use drasi_source_github_workgraph::lease_ledger::LeaseLedger;
use drasi_source_github_workgraph::mapping::{worker_changes, Converter, WorkerProjection};
use drasi_source_github_workgraph::workers::{parse_worker_file, WorkerFileContent};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TASK_TYPE_ID: &str = "IT_test";
const TASK_TYPE_NAME: &str = "WorkGraphTask";
const TASK_BODY: &str = r#"WorkGraphTask/v1

```yaml
taskType: validate-issue
inputs:
  validationProfile: new-issue-default
```
"#;
const ASSIGNMENT_BODY: &str = r#"WorkGraphTaskAssignment/v1

```json
{
  "agentProfile": "issue-validator"
}
```
"#;
const RESULT_BODY: &str = r#"WorkGraphTaskResult/v1

```json
{
  "taskType": "validate-issue",
  "outcome": "succeeded",
  "summary": "Bootstrap result.",
  "result": {
    "criteria": [
      {
        "criterion": "Contract",
        "passed": true,
        "evidence": "Verified."
      }
    ]
  }
}
```
"#;
const ACCEPTANCE_BODY: &str = r#"WorkGraphTaskResultAcceptance/v1

```json
{
  "resultCommentNodeId": "IC_result",
  "resultBodyDigest": "sha256:0000000000000000000000000000000000000000000000000000000000000000",
  "summary": "Accepted the result."
}
```
"#;

fn task_type() -> TaskIssueType {
    TaskIssueType {
        id: TASK_TYPE_ID.to_string(),
        name: TASK_TYPE_NAME.to_string(),
    }
}

fn org() -> Value {
    json!({"node_id":"O_acme","id":1,"login":"acme"})
}

fn repo(name: &str) -> Value {
    json!({
        "node_id":format!("R_{name}"),"id":2,"name":name,
        "full_name":format!("acme/{name}"),"owner":{"login":"acme"},
        "html_url":format!("https://github.com/acme/{name}"),"private":false,
        "archived":false,"fork":false,"visibility":"PUBLIC",
        "created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-02T00:00:00Z",
        "defaultBranchRef":{"name":"main"},
        "repositoryTopics":{"nodes":[{"topic":{"name":"workgraph"}}]}
    })
}

fn labels() -> Value {
    json!({"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]})
}

fn named_labels(names: &[&str]) -> Value {
    json!({
        "pageInfo":{"hasNextPage":false,"endCursor":null},
        "nodes":names.iter().enumerate().map(|(index, name)| {
            json!({"name":name,"node_id":format!("L_{index}")})
        }).collect::<Vec<_>>()
    })
}

fn fixture_database_id(id: &str) -> u64 {
    id.bytes().fold(0_u64, |value, byte| {
        value.wrapping_mul(31).wrapping_add(u64::from(byte))
    })
}

fn issue(id: &str, body: &str, state: &str, typed: bool, comments: u64) -> Value {
    let database_id = fixture_database_id(id);
    json!({
        "node_id":id,"id":database_id,"number":10,"title":id,"body":body,"state":state,
        "state_reason":if state == "CLOSED" { json!("COMPLETED") } else { Value::Null },
        "locked":false,"created_at":"2026-01-01T00:00:00Z",
        "updated_at":"2026-01-02T00:00:00Z",
        "closed_at":if state == "CLOSED" { json!("2026-01-02T00:00:00Z") } else { Value::Null },
        "html_url":format!("https://github.com/acme/widgets/issues/{id}"),
        "type":if typed { json!({"node_id":TASK_TYPE_ID,"name":TASK_TYPE_NAME}) } else { Value::Null },
        "user":{"login":"ada","node_id":"U_ada","id":3,"type":"User"},
        "author_association":"MEMBER","assignees":{"nodes":[]},"labels":labels(),
        "comments":{"totalCount":comments}
    })
}

fn parent() -> Value {
    let mut parent = issue("I_parent", "Parent body", "OPEN", false, 0);
    parent["repository"] = repo("parents");
    parent
}

fn task(id: &str, state: &str, body: &str, comments: u64) -> Value {
    let mut task = issue(id, body, state, true, comments);
    task["parent"] = parent();
    task
}

fn comment_by(id: &str, body: &str, login: &str) -> Value {
    let mut value = comment(id, body);
    value["user"] = json!({
        "login": login,
        "node_id": format!("U_{login}"),
        "id": 7,
        "type": "User"
    });
    value
}

fn comment(id: &str, body: &str) -> Value {
    json!({
        "node_id":id,"id":20,"body":body,
        "created_at":"2026-01-03T00:00:00Z","updated_at":"2026-01-03T00:00:00Z",
        "html_url":format!("https://github.com/acme/widgets/issues/10#issuecomment-{id}"),
        "user":{"login":"bot","node_id":"U_bot","id":4,"type":"Bot"}
    })
}

fn connection(nodes: Vec<Value>, has_next: bool, cursor: Option<&str>) -> Value {
    json!({"pageInfo":{"hasNextPage":has_next,"endCursor":cursor},"nodes":nodes})
}

async fn mount_query(server: &MockServer, needles: &[&str], data: Value, expected: Option<u64>) {
    let mut mock = Mock::given(method("POST")).and(path("/graphql"));
    for needle in needles {
        mock = mock.and(body_string_contains((*needle).to_string()));
    }
    let mock = mock.respond_with(ResponseTemplate::new(200).set_body_json(json!({"data":data})));
    match expected {
        Some(count) => mock.expect(count).mount(server).await,
        None => mock.mount(server).await,
    }
}

async fn mount_snapshot(server: &MockServer, task_body: &str, result_body: &str) {
    mount_query(
        server,
        &["avatar_url: avatarUrl"],
        json!({"organization":org()}),
        Some(1),
    )
    .await;
    mount_query(
        server,
        &["repositories(first"],
        json!({"organization":{"repositories":connection(vec![repo("widgets")],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        server,
        &["states: [OPEN]", "type: issueType"],
        json!({"repository":{"issues":connection(vec![{
            let mut issue = issue("I_generic","Generic body","OPEN",false,0);
            issue["labels"] = named_labels(&[
                "status:New",
                "workgraph:error",
                "status:Awaiting-Triage",
            ]);
            issue
        }],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        server,
        &[
            "filterBy: {type: $issueType}",
            "\"state\":\"OPEN\"",
            "\"issueType\":\"WorkGraphTask\"",
        ],
        json!({"repository":{"issues":connection(
            vec![task("I_task_open","OPEN",task_body,4)],false,None
        )}}),
        Some(1),
    )
    .await;
    mount_query(
        server,
        &[
            "filterBy: {type: $issueType}",
            "\"state\":\"CLOSED\"",
            "\"issueType\":\"WorkGraphTask\"",
        ],
        json!({"repository":{"issues":connection(
            vec![task("I_task_closed","CLOSED",TASK_BODY,1)],false,None
        )}}),
        Some(1),
    )
    .await;
    mount_query(
        server,
        &["pullRequests(first"],
        json!({"repository":{"pullRequests":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        server,
        &["... on Issue {", "\"id\":\"I_task_open\""],
        json!({"node":{"comments":connection(vec![
            comment("IC_plain","Ordinary task comment."),
            comment("IC_assignment",ASSIGNMENT_BODY),
            comment("IC_result",result_body),
            comment("IC_acceptance",ACCEPTANCE_BODY)
        ],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        server,
        &["... on Issue {", "\"id\":\"I_task_closed\""],
        json!({"node":{"comments":connection(vec![
            comment("IC_closed_result",RESULT_BODY)
        ],false,None)}}),
        Some(1),
    )
    .await;
}

fn provider(server: &MockServer, repositories: Vec<String>) -> GitHubWorkGraphBootstrapProvider {
    GitHubWorkGraphBootstrapProvider::builder()
        .with_organization("acme")
        .with_task_issue_type(task_type())
        .with_repositories(repositories)
        .with_token("read-only-token")
        .with_api_base_url(format!("{}/graphql", server.uri()))
        .with_max_concurrency(2)
        .build()
        .unwrap()
}

fn request() -> BootstrapRequest {
    BootstrapRequest {
        query_id: "q".to_string(),
        node_labels: vec![],
        relation_labels: vec![],
        request_id: "r".to_string(),
    }
}

async fn run(
    provider: &GitHubWorkGraphBootstrapProvider,
    request: BootstrapRequest,
) -> (drasi_lib::bootstrap::BootstrapResult, Vec<BootstrapEvent>) {
    let context = BootstrapContext::new_minimal("server".to_string(), "gh".to_string());
    let (tx, mut rx) = tokio::sync::mpsc::channel(512);
    let result = provider
        .bootstrap(request, &context, tx, None)
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    (result, events)
}

fn label(event: &BootstrapEvent) -> &str {
    let metadata = match &event.change {
        SourceChange::Insert { element } | SourceChange::Update { element } => {
            element.get_metadata()
        }
        SourceChange::Delete { metadata } => metadata,
        SourceChange::Future { .. } => panic!("unexpected future change"),
    };
    &metadata.labels[0]
}

fn id(event: &BootstrapEvent) -> &str {
    &event.change.get_reference().element_id
}

fn node_property<'a>(events: &'a [BootstrapEvent], node_id: &str, key: &str) -> &'a ElementValue {
    events
        .iter()
        .find_map(|event| match &event.change {
            SourceChange::Insert {
                element: Element::Node { properties, .. },
            } if id(event) == node_id => properties.get(key),
            _ => None,
        })
        .unwrap_or_else(|| panic!("missing {node_id}.{key}"))
}

#[tokio::test]
async fn snapshots_generic_open_and_open_closed_tasks_with_parents_and_comments() {
    let server = MockServer::start().await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;
    let (result, events) = run(&provider(&server, vec![]), request()).await;

    assert_eq!(result.source_position, None);
    assert_eq!(result.event_count, events.len());
    assert!(events
        .iter()
        .all(|event| matches!(event.change, SourceChange::Insert { .. })));
    for node_id in ["I_task_open", "I_task_closed"] {
        assert!(events
            .iter()
            .any(|event| id(event) == node_id && label(event) == "WorkGraphTask"));
        assert!(!events
            .iter()
            .any(|event| id(event) == node_id && label(event) == "GitHubIssue"));
    }
    assert!(events
        .iter()
        .any(|event| id(event) == "I_generic" && label(event) == "GitHubIssue"));
    assert_eq!(
        node_property(&events, "I_generic", "statusLabels"),
        &ElementValue::from(&json!(["status:New", "status:Awaiting-Triage"]))
    );
    assert_eq!(
        node_property(&events, "I_generic", "currentStatus"),
        &ElementValue::String("error".into())
    );
    assert_eq!(
        node_property(&events, "I_generic", "workgraphLabels"),
        &ElementValue::from(&json!(["workgraph:error"]))
    );
    assert_eq!(
        node_property(&events, "I_generic", "workgraphInclude"),
        &ElementValue::Bool(false)
    );
    assert_eq!(
        node_property(&events, "I_generic", "state"),
        &ElementValue::from(&json!("open"))
    );
    assert_eq!(
        node_property(&events, "I_generic", "stateReason"),
        &ElementValue::Null
    );
    assert_eq!(
        node_property(&events, "I_generic", "isOpen"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&events, "I_task_open", "state"),
        &ElementValue::from(&json!("open"))
    );
    assert_eq!(
        node_property(&events, "I_task_open", "isOpen"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&events, "I_task_closed", "state"),
        &ElementValue::from(&json!("closed"))
    );
    assert_eq!(
        node_property(&events, "I_task_closed", "stateReason"),
        &ElementValue::from(&json!("completed"))
    );
    assert_eq!(
        node_property(&events, "I_task_closed", "isOpen"),
        &ElementValue::Bool(false)
    );
    assert!(events
        .iter()
        .any(|event| id(event) == "I_parent" && label(event) == "GitHubIssue"));
    let parent_repository = events
        .iter()
        .find(|event| id(event) == "R_parents" && label(event) == "GitHubRepository")
        .expect("parent repository from child task query");
    let SourceChange::Insert { element } = &parent_repository.change else {
        panic!("bootstrap nodes are inserts");
    };
    assert_eq!(
        element.get_property("defaultBranch"),
        &drasi_core::models::ElementValue::from(&json!("main"))
    );
    assert_eq!(
        element.get_property("topics"),
        &drasi_core::models::ElementValue::from(&json!(["workgraph"]))
    );
    assert!(events
        .iter()
        .any(|event| id(event) == "IC_plain" && label(event) == "GitHubIssueComment"));
    for result_id in ["IC_result", "IC_closed_result"] {
        assert!(events
            .iter()
            .any(|event| id(event) == result_id && label(event) == "WorkGraphTaskResult"));
    }
    assert!(events.iter().any(|event| {
        id(event) == "IC_assignment" && label(event) == "WorkGraphTaskAssignment"
    }));
    assert!(events.iter().any(|event| {
        id(event) == "IC_acceptance" && label(event) == "WorkGraphTaskResultAcceptance"
    }));
    for relation in ["ASSIGNMENT_FOR", "ACCEPTS_RESULT"] {
        assert!(events.iter().any(|event| label(event) == relation));
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| label(event) == "TASK_FOR")
            .count(),
        2
    );
    for task_id in ["I_task_open", "I_task_closed"] {
        let relation_id = format!("TASK_FOR:{}", fixture_database_id(task_id));
        let relation = events
            .iter()
            .find(|event| id(event) == relation_id && label(event) == "TASK_FOR")
            .expect("TASK_FOR uses the child database ID");
        let SourceChange::Insert {
            element: Element::Relation {
                in_node, out_node, ..
            },
        } = &relation.change
        else {
            panic!("bootstrap relation is an insert");
        };
        assert_eq!(in_node.element_id.as_ref(), task_id);
        assert_eq!(out_node.element_id.as_ref(), "I_parent");
    }
    for event in events.iter().filter(|event| label(event) == "RESULT_FOR") {
        let SourceChange::Insert {
            element: Element::Relation {
                in_node, out_node, ..
            },
        } = &event.change
        else {
            panic!("bootstrap relations are inserts");
        };
        assert!(in_node.element_id.starts_with("IC_"));
        assert!(out_node.element_id.starts_with("I_task_"));
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| label(event) == "RESULT_FOR")
            .count(),
        2
    );
    let acceptance = events
        .iter()
        .find(|event| label(event) == "ACCEPTS_RESULT")
        .expect("acceptance relation");
    let SourceChange::Insert {
        element: Element::Relation {
            in_node, out_node, ..
        },
    } = &acceptance.change
    else {
        panic!("bootstrap relations are inserts");
    };
    assert_eq!(in_node.element_id.as_ref(), "IC_acceptance");
    assert_eq!(out_node.element_id.as_ref(), "IC_result");
}

#[tokio::test]
async fn malformed_task_and_marked_result_use_shared_error_conversion() {
    let server = MockServer::start().await;
    mount_snapshot(
        &server,
        "{}",
        "WorkGraphTaskResult/v1\n\n```json\n{}\n```\n",
    )
    .await;
    let (_, events) = run(&provider(&server, vec![]), request()).await;
    assert!(events.iter().any(|event| {
        id(event) == "workgraph-error:task:I_task_open" && label(event) == "WorkGraphError"
    }));
    assert_eq!(
        node_property(&events, "workgraph-error:task:I_task_open", "isOpen"),
        &ElementValue::Bool(true)
    );
    assert!(events.iter().any(|event| {
        id(event) == "workgraph-error:comment:IC_result" && label(event) == "WorkGraphError"
    }));
    assert!(!events
        .iter()
        .any(|event| id(event) == "I_task_open" && label(event) == "WorkGraphTask"));
}

#[tokio::test]
async fn repository_allowlist_applies_to_authoritative_parent_repository() {
    let server = MockServer::start().await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;
    let (_, events) = run(&provider(&server, vec!["widgets".to_string()]), request()).await;
    assert!(!events
        .iter()
        .any(|event| id(event) == "I_parent" && label(event) == "GitHubIssue"));
    assert!(!events
        .iter()
        .any(|event| id(event) == "R_parents" && label(event) == "GitHubRepository"));
    assert!(events.iter().any(|event| label(event) == "TASK_FOR"));
}

#[tokio::test]
async fn requested_labels_filter_task_snapshot() {
    let server = MockServer::start().await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;
    let mut task_request = request();
    task_request.node_labels = vec!["WorkGraphTask".to_string()];
    let (result, events) = run(&provider(&server, vec![]), task_request).await;
    assert_eq!(result.event_count, 2);
    assert!(events.iter().all(|event| label(event) == "WorkGraphTask"));
}

#[tokio::test]
async fn task_open_and_closed_connections_follow_every_cursor() {
    let server = MockServer::start().await;
    let mut wrong_type = task("I_wrong_type", "OPEN", TASK_BODY, 0);
    wrong_type["type"]["node_id"] = json!("IT_other");
    mount_query(
        &server,
        &[
            "filterBy: {type: $issueType}",
            "\"state\":\"OPEN\"",
            "\"issueType\":\"WorkGraphTask\"",
            "\"cursor\":null",
        ],
        json!({"repository":{"issues":connection(
            vec![task("I_open_1","OPEN",TASK_BODY,0),wrong_type],true,Some("NEXT")
        )}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &[
            "states: [$state]",
            "filterBy: {type: $issueType}",
            "\"state\":\"OPEN\"",
            "\"issueType\":\"WorkGraphTask\"",
            "\"cursor\":\"NEXT\"",
        ],
        json!({"repository":{"issues":connection(
            vec![task("I_open_2","OPEN",TASK_BODY,0)],false,None
        )}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &[
            "filterBy: {type: $issueType}",
            "\"state\":\"CLOSED\"",
            "\"issueType\":\"WorkGraphTask\"",
        ],
        json!({"repository":{"issues":connection(
            vec![task("I_closed","CLOSED",TASK_BODY,0)],false,None
        )}}),
        Some(1),
    )
    .await;
    let client =
        GitHubGraphQLClient::new("token", &format!("{}/graphql", server.uri()), 1).unwrap();
    let tasks = client
        .fetch_tasks("acme", "widgets", &task_type())
        .await
        .unwrap();
    assert_eq!(tasks.len(), 3);
    assert_eq!(
        tasks
            .iter()
            .map(|task| task["node_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["I_open_1", "I_open_2", "I_closed"]
    );
}

#[tokio::test]
async fn repository_issue_pagination_is_complete_beyond_search_cap() {
    let server = MockServer::start().await;
    for page in 0..=10 {
        let cursor_needle = if page == 0 {
            "\"cursor\":null".to_string()
        } else {
            format!("\"cursor\":\"PAGE_{page}\"")
        };
        let count = if page == 10 { 1 } else { 100 };
        let first = page * 100;
        let nodes = (first..first + count)
            .map(|index| issue(&format!("I_{index}"), TASK_BODY, "OPEN", true, 0))
            .collect();
        let next = (page < 10).then(|| format!("PAGE_{}", page + 1));
        mount_query(
            &server,
            &[
                "issues(first: $pageSize",
                "filterBy: {type: $issueType}",
                "\"state\":\"OPEN\"",
                "\"issueType\":\"WorkGraphTask\"",
                &cursor_needle,
            ],
            json!({"repository":{"issues":connection(
                nodes, page < 10, next.as_deref()
            )}}),
            Some(1),
        )
        .await;
    }
    mount_query(
        &server,
        &[
            "filterBy: {type: $issueType}",
            "\"state\":\"CLOSED\"",
            "\"issueType\":\"WorkGraphTask\"",
        ],
        json!({"repository":{"issues":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;

    let client =
        GitHubGraphQLClient::new("token", &format!("{}/graphql", server.uri()), 1).unwrap();
    let tasks = client
        .fetch_tasks("acme", "widgets", &task_type())
        .await
        .unwrap();
    assert_eq!(tasks.len(), 1001);
    assert_eq!(tasks.last().unwrap()["node_id"], json!("I_1000"));
}

#[tokio::test]
async fn ordinary_open_issue_with_result_marker_stays_generic_comment() {
    let server = MockServer::start().await;
    mount_query(
        &server,
        &["avatar_url: avatarUrl"],
        json!({"organization":org()}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["repositories(first"],
        json!({"organization":{"repositories":connection(vec![repo("widgets")],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["states: [OPEN]", "type: issueType"],
        json!({"repository":{"issues":connection(
            vec![issue("I_generic","Generic","OPEN",false,1)],false,None
        )}}),
        Some(1),
    )
    .await;
    for state in ["OPEN", "CLOSED"] {
        mount_query(
            &server,
            &[
                "filterBy: {type: $issueType}",
                &format!("\"state\":\"{state}\""),
                "\"issueType\":\"WorkGraphTask\"",
            ],
            json!({"repository":{"issues":connection(vec![],false,None)}}),
            Some(1),
        )
        .await;
    }
    mount_query(
        &server,
        &["pullRequests(first"],
        json!({"repository":{"pullRequests":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["... on Issue {", "\"id\":\"I_generic\""],
        json!({"node":{"comments":connection(
            vec![comment("IC_generic",RESULT_BODY)],false,None
        )}}),
        Some(1),
    )
    .await;
    let (_, events) = run(&provider(&server, vec![]), request()).await;
    assert!(events
        .iter()
        .any(|event| id(event) == "IC_generic" && label(event) == "GitHubIssueComment"));
}

// ---------------------------------------------------------------------------
// Worker queue: worker file snapshot, ordering ahead of task artifacts, and
// bootstrap/live parity for the Assignment/Lease/Result lifecycle.
// ---------------------------------------------------------------------------

const WORKER_FILE: &str = "version: 1\nworkers:\n  - workerId: validator-1\n    agentProfile: \
                           issue-validator\n    slots: 2\n    leaseDuration: PT15M\n";
const LEASE_ID: &str = "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21";
const LEASE_ANCHOR_ID: &str = "workgraph-lease:I_task_open:0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21";

const ASSIGNMENT_V2_BODY: &str = r#"WorkGraphTaskAssignment/v2

```json
{
  "agentProfile": "issue-validator",
  "workerId": "validator-1"
}
```
"#;

const LEASE_BODY: &str = r#"WorkGraphTaskLease/v1

```json
{
  "leaseId": "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21",
  "assignmentCommentNodeId": "IC_assignment",
  "workerId": "validator-1",
  "slotId": "validator-1/1",
  "acquiredAt": "2026-08-19T22:00:00Z",
  "expiresAt": "2026-08-19T22:15:00Z"
}
```
"#;

const RESULT_V2_BODY: &str = r#"WorkGraphTaskResult/v2

```json
{
  "taskType": "validate-issue",
  "leaseId": "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21",
  "outcome": "succeeded",
  "summary": "Bootstrap result.",
  "result": {
    "criteria": [
      {
        "criterion": "Contract",
        "passed": true,
        "evidence": "Verified."
      }
    ]
  }
}
```
"#;

fn worker_location() -> WorkerFileLocation {
    WorkerFileLocation {
        repository: "acme/widgets".to_string(),
        r#ref: "main".to_string(),
        path: ".github/workgraph/workers.yaml".to_string(),
    }
}

/// Mount the shared worker-file blob query with an explicit GraphQL response.
async fn mount_worker_file(server: &MockServer, object: Value) {
    mount_query(
        server,
        &["object(expression: $expression)"],
        json!({ "repository": { "object": object } }),
        Some(1),
    )
    .await;
}

async fn mount_worker_blob(server: &MockServer, text: &str) {
    mount_worker_file(
        server,
        json!({
            "__typename": "Blob",
            "oid": "blob-oid",
            "text": text,
            "byteSize": text.len(),
            "isTruncated": false,
            "isBinary": false,
        }),
    )
    .await;
}

fn bootstrap_lease_trust() -> LeaseTrust {
    // The comment fixtures author every comment as "bot", matching the
    // prototype's single-identity deployment.
    let bot = TrustedIdentity {
        id: "U_bot".to_string(),
        login: "bot".to_string(),
    };
    LeaseTrust {
        dispatchers: vec![bot.clone()],
        reporters: vec![bot],
    }
}

fn worker_provider(server: &MockServer) -> GitHubWorkGraphBootstrapProvider {
    GitHubWorkGraphBootstrapProvider::builder()
        .with_organization("acme")
        .with_task_issue_type(task_type())
        .with_token("read-only-token")
        .with_api_base_url(format!("{}/graphql", server.uri()))
        .with_max_concurrency(2)
        .with_worker_config(worker_location())
        .with_lease_trust(bootstrap_lease_trust())
        .build()
        .unwrap()
}

fn node_property_opt<'a>(
    events: &'a [BootstrapEvent],
    node_id: &str,
    key: &str,
) -> Option<&'a ElementValue> {
    events.iter().find_map(|event| match &event.change {
        SourceChange::Insert {
            element: Element::Node { properties, .. },
        } if id(event) == node_id => properties.get(key),
        _ => None,
    })
}

fn position(events: &[BootstrapEvent], node_id: &str) -> usize {
    events
        .iter()
        .position(|event| id(event) == node_id)
        .unwrap_or_else(|| panic!("missing {node_id}"))
}

#[tokio::test]
async fn worker_snapshot_precedes_every_task_artifact() {
    let server = MockServer::start().await;
    mount_worker_blob(&server, WORKER_FILE).await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

    let (result, events) = run(&worker_provider(&server), request()).await;
    assert_eq!(result.event_count, events.len());
    assert!(events
        .iter()
        .all(|event| matches!(event.change, SourceChange::Insert { .. })));

    for node_id in [
        "workgraph-worker:validator-1",
        "workgraph-worker-slot:validator-1/1",
        "workgraph-worker-slot:validator-1/2",
    ] {
        assert!(
            events.iter().any(|event| id(event) == node_id),
            "missing {node_id}"
        );
    }
    assert!(events.iter().any(|event| label(event) == "HAS_SLOT"));

    // Every worker element is snapshotted before the organization, the
    // repositories, and any Issue, task, or task comment that references it.
    let last_worker = events
        .iter()
        .rposition(|event| {
            matches!(
                label(event),
                "WorkGraphWorker" | "WorkGraphWorkerSlot" | "HAS_SLOT"
            )
        })
        .expect("worker elements are snapshotted");
    let first_other = events
        .iter()
        .position(|event| {
            !matches!(
                label(event),
                "WorkGraphWorker" | "WorkGraphWorkerSlot" | "HAS_SLOT"
            )
        })
        .expect("task artifacts are snapshotted");
    assert!(
        last_worker < first_other,
        "worker elements must precede all other snapshot state"
    );
    assert!(position(&events, "I_task_open") > last_worker);

    assert_eq!(
        node_property(
            &events,
            "workgraph-worker:validator-1",
            "configuredSlotCount"
        ),
        &ElementValue::Integer(2)
    );
    assert_eq!(
        node_property(
            &events,
            "workgraph-worker:validator-1",
            "leaseDurationSeconds"
        ),
        &ElementValue::Integer(900)
    );
    assert_eq!(
        node_property(&events, "workgraph-worker-slot:validator-1/2", "enabled"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&events, "workgraph-worker:validator-1", "configPath"),
        &ElementValue::from(&json!(".github/workgraph/workers.yaml"))
    );

    // Historical v1 Assignments and Results keep working after a clean
    // bootstrap that also projects the worker queue: they are readable, they
    // name no worker queue, and they bind no lease.
    assert_eq!(
        node_property(&events, "IC_assignment", "version"),
        &ElementValue::Integer(1)
    );
    assert_eq!(
        node_property(&events, "IC_assignment", "workerId"),
        &ElementValue::Null
    );
    assert_eq!(
        node_property(&events, "IC_result", "version"),
        &ElementValue::Integer(1)
    );
    assert_eq!(
        node_property(&events, "IC_result", "leaseId"),
        &ElementValue::Null
    );
    assert!(events.iter().any(|event| label(event) == "ASSIGNMENT_FOR"));
    assert!(events.iter().any(|event| label(event) == "RESULT_FOR"));
    for absent in [
        "ASSIGNED_TO",
        "RESULT_FOR_LEASE",
        "LEASE_ANCHOR",
        "WorkGraphTaskLeaseAnchor",
    ] {
        assert!(
            !events.iter().any(|event| label(event) == absent),
            "{absent} must not appear for a v1-only task"
        );
    }
}

#[tokio::test]
async fn malformed_worker_file_snapshots_an_error_and_no_workers() {
    for body in [
        "version: 1\nworkers: []\n",
        "version: 2\nworkers:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: PT1M\n",
        "workers: not-a-list\n",
        "version: 1\nworkers:\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: PT1M\n  - workerId: w\n    agentProfile: issue-validator\n    slots: 1\n    leaseDuration: PT1M\n",
    ] {
        let server = MockServer::start().await;
        mount_worker_blob(&server, body).await;
        mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

        let (_, events) = run(&worker_provider(&server), request()).await;
        assert!(
            events
                .iter()
                .any(|event| id(event) == "workgraph-error:worker-config"),
            "expected a worker-config error for: {body}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                label(event),
                "WorkGraphWorker" | "WorkGraphWorkerSlot" | "HAS_SLOT"
            )),
            "a rejected worker file must never project a worker pool: {body}"
        );
        assert_eq!(
            node_property(&events, "workgraph-error:worker-config", "errorKind"),
            &ElementValue::from(&json!("invalid-workgraph-worker-config"))
        );
        // The rest of the snapshot still completes.
        assert!(events.iter().any(|event| id(event) == "I_task_open"));
    }
}

#[tokio::test]
async fn missing_worker_file_is_a_configuration_error_not_an_empty_pool() {
    let server = MockServer::start().await;
    mount_worker_file(&server, Value::Null).await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

    let (_, events) = run(&worker_provider(&server), request()).await;
    assert_eq!(
        node_property(&events, "workgraph-error:worker-config", "errorCode"),
        &ElementValue::from(&json!("worker-file-unavailable"))
    );
    assert!(!events.iter().any(|event| label(event) == "WorkGraphWorker"));
}

#[tokio::test]
async fn oversized_or_binary_worker_file_is_rejected() {
    for object in [
        json!({
            "__typename": "Blob", "oid": "o", "text": "version: 1\n",
            "byteSize": 512 * 1024, "isTruncated": false, "isBinary": false
        }),
        json!({
            "__typename": "Blob", "oid": "o", "text": "version: 1\n",
            "byteSize": 10, "isTruncated": true, "isBinary": false
        }),
        json!({
            "__typename": "Blob", "oid": "o", "text": Value::Null,
            "byteSize": 10, "isTruncated": false, "isBinary": true
        }),
        // A tree at the configured path is not a worker file.
        json!({ "__typename": "Tree" }),
    ] {
        let server = MockServer::start().await;
        mount_worker_file(&server, object.clone()).await;
        mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

        let (_, events) = run(&worker_provider(&server), request()).await;
        assert!(
            events
                .iter()
                .any(|event| id(event) == "workgraph-error:worker-config"),
            "expected rejection for: {object}"
        );
        assert!(!events.iter().any(|event| label(event) == "WorkGraphWorker"));
    }
}

#[tokio::test]
async fn unreadable_worker_file_fails_the_bootstrap() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("object(expression: $expression)"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let context = BootstrapContext::new_minimal("server".to_string(), "gh".to_string());
    let (tx, _rx) = tokio::sync::mpsc::channel(512);
    let error = worker_provider(&server)
        .bootstrap(request(), &context, tx, None)
        .await
        .expect_err("an unreadable worker file must fail the bootstrap");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("worker file"),
        "unexpected error: {rendered}"
    );
}

#[tokio::test]
async fn omitted_worker_config_snapshots_no_worker_elements() {
    let server = MockServer::start().await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

    let (_, events) = run(&provider(&server, vec![]), request()).await;
    assert!(!events.iter().any(|event| matches!(
        label(event),
        "WorkGraphWorker" | "WorkGraphWorkerSlot" | "HAS_SLOT"
    )));
    assert!(!events
        .iter()
        .any(|event| id(event) == "workgraph-error:worker-config"));
    assert!(events.iter().any(|event| id(event) == "I_task_open"));
}

#[tokio::test]
async fn requested_labels_filter_the_worker_snapshot() {
    let server = MockServer::start().await;
    mount_worker_blob(&server, WORKER_FILE).await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

    let request = BootstrapRequest {
        node_labels: vec!["WorkGraphWorkerSlot".to_string()],
        ..request()
    };
    let (_, events) = run(&worker_provider(&server), request).await;
    assert!(!events.is_empty());
    assert!(events
        .iter()
        .all(|event| label(event) == "WorkGraphWorkerSlot"));
}

#[tokio::test]
async fn bootstrap_folds_the_lease_lifecycle_without_inventing_a_verdict() {
    let server = MockServer::start().await;
    mount_worker_blob(&server, WORKER_FILE).await;
    mount_query(
        &server,
        &["avatar_url: avatarUrl"],
        json!({"organization":org()}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["repositories(first"],
        json!({"organization":{"repositories":connection(vec![repo("widgets")],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["states: [OPEN]", "type: issueType"],
        json!({"repository":{"issues":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["filterBy: {type: $issueType}", "\"state\":\"OPEN\""],
        json!({"repository":{"issues":connection(
            vec![task("I_task_open","OPEN",TASK_BODY,3)],false,None
        )}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["filterBy: {type: $issueType}", "\"state\":\"CLOSED\""],
        json!({"repository":{"issues":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["pullRequests(first"],
        json!({"repository":{"pullRequests":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["... on Issue {", "\"id\":\"I_task_open\""],
        json!({"node":{"comments":connection(vec![
            comment("IC_assignment",ASSIGNMENT_V2_BODY),
            comment("IC_lease",LEASE_BODY),
            comment("IC_result",RESULT_V2_BODY)
        ],false,None)}}),
        Some(1),
    )
    .await;

    let (_, events) = run(&worker_provider(&server), request()).await;

    // The v2 Assignment queues the task on its exact worker.
    assert_eq!(
        node_property(&events, "IC_assignment", "workerId"),
        &ElementValue::from(&json!("validator-1"))
    );
    assert_eq!(
        node_property(&events, "IC_assignment", "version"),
        &ElementValue::Integer(2)
    );
    assert!(events.iter().any(|event| label(event) == "ASSIGNED_TO"));

    // The Lease binds its task and its configured slot.
    assert!(events.iter().any(|event| label(event) == "LEASE_FOR"));
    assert!(events.iter().any(|event| label(event) == "LEASES_SLOT"));
    assert!(events.iter().any(|event| label(event) == "LEASE_ANCHOR"));
    assert!(events
        .iter()
        .any(|event| label(event) == "RESULT_FOR_LEASE"));

    // Exactly one lease anchor exists. The Lease opened it and the trusted
    // Result closed it, and the folded snapshot carries the same final
    // lifecycle the live Source converges to.
    assert_eq!(
        events
            .iter()
            .filter(|event| id(event) == LEASE_ANCHOR_ID)
            .count(),
        1
    );
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "isActive"),
        &ElementValue::Bool(false)
    );
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "endReason"),
        &ElementValue::from(&json!("result"))
    );
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "endCommentNodeId"),
        &ElementValue::from(&json!("IC_result"))
    );
    // The anchor holds only its own key; every acquisition fact stays on the
    // Lease comment node, where exactly one writer authored it.
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "taskNodeId"),
        &ElementValue::from(&json!("I_task_open"))
    );
    for acquisition in ["workerId", "slotId", "acquiredAt", "leaseCommentNodeId"] {
        assert!(
            node_property_opt(&events, LEASE_ANCHOR_ID, acquisition).is_none(),
            "the anchor must not carry {acquisition}"
        );
        assert!(
            node_property_opt(&events, "IC_lease", acquisition).is_some()
                || acquisition == "leaseCommentNodeId",
            "the Lease node must carry {acquisition}"
        );
    }
    assert_eq!(
        node_property(&events, "IC_lease", "trusted"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&events, "IC_result", "trusted"),
        &ElementValue::Bool(true)
    );

    // History is retained: the Lease and the Result remain their own nodes.
    assert!(events.iter().any(|event| id(event) == "IC_lease"));
    assert!(events.iter().any(|event| id(event) == "IC_result"));
}

#[tokio::test]
async fn bootstrap_and_live_conversion_agree_on_worker_queue_state() {
    let server = MockServer::start().await;
    mount_worker_blob(&server, WORKER_FILE).await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;
    let (_, events) = run(&worker_provider(&server), request()).await;

    // The same worker file, projected by the shared mapper the live Source
    // uses, yields exactly the same worker element identities and properties.
    let file = parse_worker_file(WORKER_FILE).unwrap();
    let content = WorkerFileContent {
        text: WORKER_FILE.to_string(),
        oid: "blob-oid".to_string(),
    };
    let live = worker_changes(
        "gh",
        1,
        &worker_location(),
        &WorkerProjection::Loaded {
            file: &file,
            content: &content,
        },
        &BTreeMap::new(),
        &BTreeMap::new(),
    );

    let live_ids: Vec<String> = live
        .iter()
        .filter(|change| !matches!(change, SourceChange::Delete { .. }))
        .map(|change| change.get_reference().element_id.to_string())
        .collect();
    let snapshot_ids: Vec<String> = events
        .iter()
        .filter(|event| {
            matches!(
                label(event),
                "WorkGraphWorker" | "WorkGraphWorkerSlot" | "HAS_SLOT"
            )
        })
        .map(|event| id(event).to_string())
        .collect();
    assert_eq!(live_ids, snapshot_ids);

    for key in [
        "workerId",
        "agentProfile",
        "configuredSlotCount",
        "leaseDuration",
        "leaseDurationSeconds",
        "configRepository",
        "configRef",
        "configPath",
        "configBlobOid",
        "configDigest",
    ] {
        let live_value = live
            .iter()
            .find_map(|change| match change {
                SourceChange::Insert {
                    element:
                        Element::Node {
                            metadata,
                            properties,
                        },
                }
                | SourceChange::Update {
                    element:
                        Element::Node {
                            metadata,
                            properties,
                        },
                } if metadata.reference.element_id.as_ref() == "workgraph-worker:validator-1" => {
                    properties.get(key)
                }
                _ => None,
            })
            .unwrap_or_else(|| panic!("live projection missing {key}"));
        assert_eq!(
            node_property(&events, "workgraph-worker:validator-1", key),
            live_value,
            "bootstrap and live disagree on {key}"
        );
    }
}

#[tokio::test]
async fn repeated_bootstraps_produce_identical_worker_snapshots() {
    let snapshot = |events: Vec<BootstrapEvent>| -> Vec<(String, String)> {
        events
            .iter()
            .map(|event| (label(event).to_string(), id(event).to_string()))
            .collect()
    };

    let server = MockServer::start().await;
    mount_worker_blob(&server, WORKER_FILE).await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;
    let (_, first) = run(&worker_provider(&server), request()).await;

    let server = MockServer::start().await;
    mount_worker_blob(&server, WORKER_FILE).await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;
    let (_, second) = run(&worker_provider(&server), request()).await;

    assert_eq!(snapshot(first), snapshot(second));
}

#[tokio::test]
async fn an_untrusted_end_cannot_forge_lease_state_in_a_snapshot() {
    // The bootstrap path must apply exactly the same trust gate as the live
    // Source: a drive-by Result naming a real leaseId must not appear as a
    // completed lease in the snapshot.
    let server = MockServer::start().await;
    mount_worker_blob(&server, WORKER_FILE).await;
    mount_query(
        &server,
        &["avatar_url: avatarUrl"],
        json!({"organization":org()}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["repositories(first"],
        json!({"organization":{"repositories":connection(vec![repo("widgets")],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["states: [OPEN]", "type: issueType"],
        json!({"repository":{"issues":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["filterBy: {type: $issueType}", "\"state\":\"OPEN\""],
        json!({"repository":{"issues":connection(
            vec![task("I_task_open","OPEN",TASK_BODY,2)],false,None
        )}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["filterBy: {type: $issueType}", "\"state\":\"CLOSED\""],
        json!({"repository":{"issues":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["pullRequests(first"],
        json!({"repository":{"pullRequests":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["... on Issue {", "\"id\":\"I_task_open\""],
        json!({"node":{"comments":connection(vec![
            comment("IC_lease",LEASE_BODY),
            comment_by("IC_drive_by",RESULT_V2_BODY,"attacker")
        ],false,None)}}),
        Some(1),
    )
    .await;

    let (_, events) = run(&worker_provider(&server), request()).await;

    // The lease is snapshotted as acquired and still active.
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "isActive"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "endReason"),
        &ElementValue::from(&json!("none"))
    );
    // The untrusted Result is retained for visibility, flagged, and bound to
    // nothing.
    assert_eq!(
        node_property(&events, "IC_drive_by", "trusted"),
        &ElementValue::Bool(false)
    );
    assert!(!events
        .iter()
        .any(|event| label(event) == "RESULT_FOR_LEASE"));
}

/// Mount a task-comment snapshot and run a worker-enabled bootstrap.
async fn snapshot_with_task_comments(comments: Vec<Value>) -> Vec<BootstrapEvent> {
    let server = MockServer::start().await;
    mount_worker_blob(&server, WORKER_FILE).await;
    mount_query(
        &server,
        &["avatar_url: avatarUrl"],
        json!({"organization":org()}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["repositories(first"],
        json!({"organization":{"repositories":connection(vec![repo("widgets")],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["states: [OPEN]", "type: issueType"],
        json!({"repository":{"issues":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;
    let count = comments.len() as u64;
    mount_query(
        &server,
        &["filterBy: {type: $issueType}", "\"state\":\"OPEN\""],
        json!({"repository":{"issues":connection(
            vec![task("I_task_open","OPEN",TASK_BODY,count)],false,None
        )}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["filterBy: {type: $issueType}", "\"state\":\"CLOSED\""],
        json!({"repository":{"issues":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["pullRequests(first"],
        json!({"repository":{"pullRequests":connection(vec![],false,None)}}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["... on Issue {", "\"id\":\"I_task_open\""],
        json!({"node":{"comments":connection(comments,false,None)}}),
        Some(1),
    )
    .await;
    run(&worker_provider(&server), request()).await.1
}

#[tokio::test]
async fn a_bootstrap_snapshot_folds_current_comments_to_the_same_lifecycle() {
    // A Lease plus a trusted Result: the snapshot reflects the ended lease.
    let events = snapshot_with_task_comments(vec![
        comment("IC_lease", LEASE_BODY),
        comment("IC_result", RESULT_V2_BODY),
    ])
    .await;
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "isActive"),
        &ElementValue::Bool(false)
    );
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "endCommentNodeId"),
        &ElementValue::from(&json!("IC_result"))
    );

    // The same lease with the Result no longer present is active again: a
    // snapshot is current state, not accumulated history.
    let events = snapshot_with_task_comments(vec![comment("IC_lease", LEASE_BODY)]).await;
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "isActive"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "endReason"),
        &ElementValue::from(&json!("none"))
    );

    // Duplicate acquisitions fail closed in a snapshot too.
    let second_lease = LEASE_BODY.replace("validator-1/1", "validator-1/2");
    let events = snapshot_with_task_comments(vec![
        comment("IC_lease", LEASE_BODY),
        comment("IC_lease_2", &second_lease),
    ])
    .await;
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "isActive"),
        &ElementValue::Bool(false)
    );
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "endReason"),
        &ElementValue::from(&json!("conflict"))
    );

    // An end with no acquisition materializes no anchor at all.
    let events = snapshot_with_task_comments(vec![comment("IC_result", RESULT_V2_BODY)]).await;
    assert!(!events
        .iter()
        .any(|event| label(event) == "WorkGraphTaskLeaseAnchor"));
}

#[tokio::test]
async fn a_bootstrap_untrusted_editor_cannot_forge_a_lifecycle_artifact() {
    // Bootstrap sees GitHub's `editor` rather than a webhook `sender`, and must
    // apply the same rule: an untrusted editor removes trust.
    let mut edited = comment("IC_lease", LEASE_BODY);
    edited["editor"] = json!({"login": "attacker", "node_id": "U_attacker"});
    let events = snapshot_with_task_comments(vec![edited]).await;
    assert_eq!(
        node_property(&events, "IC_lease", "trusted"),
        &ElementValue::Bool(false)
    );
    assert_eq!(
        node_property(&events, "IC_lease", "editorLogin"),
        &ElementValue::from(&json!("attacker"))
    );
    assert!(!events
        .iter()
        .any(|event| label(event) == "WorkGraphTaskLeaseAnchor"));

    // An edit by the trusted author is accepted.
    let mut edited = comment("IC_lease", LEASE_BODY);
    edited["editor"] = json!({"login": "bot", "node_id": "U_bot"});
    let events = snapshot_with_task_comments(vec![edited]).await;
    assert_eq!(
        node_property(&events, "IC_lease", "trusted"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "isActive"),
        &ElementValue::Bool(true)
    );
}

#[tokio::test]
async fn bootstrap_and_live_agree_on_the_same_current_comments() {
    // The bootstrapper's fold and the live Source's ledger must project the
    // same anchor for the same set of current comments.
    let comments = vec![
        comment("IC_lease", LEASE_BODY),
        comment("IC_result", RESULT_V2_BODY),
    ];
    let events = snapshot_with_task_comments(comments).await;

    let mut ledger = LeaseLedger::new();
    let trust = bootstrap_lease_trust();
    for (comment_id, body) in [("IC_lease", LEASE_BODY), ("IC_result", RESULT_V2_BODY)] {
        let payload = json!({
            "action": "created",
            "organization": org(),
            "repository": repo("widgets"),
            "issue": task("I_task_open", "OPEN", TASK_BODY, 2),
            "comment": comment(comment_id, body),
        });
        let conversion = Converter::new("gh", "acme", &task_type(), 1)
            .with_lease_trust(&trust)
            .convert("issue_comment", &payload)
            .unwrap()
            .unwrap();
        for intent in &conversion.lifecycle {
            ledger.apply(intent);
        }
    }
    let live = ledger
        .project(LEASE_ANCHOR_ID)
        .expect("the live ledger projects the anchor");

    assert!(!live.is_active);
    assert_eq!(live.end_reason, "result");
    assert_eq!(live.end_comment_node_id.as_deref(), Some("IC_result"));
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "isActive"),
        &ElementValue::Bool(live.is_active)
    );
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "endReason"),
        &ElementValue::from(&json!(live.end_reason))
    );
    assert_eq!(
        node_property(&events, LEASE_ANCHOR_ID, "endCommentNodeId"),
        &ElementValue::from(&json!(live.end_comment_node_id))
    );
}
