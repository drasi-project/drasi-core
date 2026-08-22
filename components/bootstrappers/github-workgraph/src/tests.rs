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
use crate::{AgentFileLocation, GitHubWorkGraphBootstrapProvider};
use drasi_core::models::{Element, ElementValue, SourceChange};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::BootstrapEvent;
use drasi_source_github_workgraph::agents::{parse_agent_file, AgentFileContent};
use drasi_source_github_workgraph::config::{LeaseTrust, TaskIssueType, TrustedIdentity};
use drasi_source_github_workgraph::mapping::{agent_changes, AgentProjection, Converter};
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
  "agentId": "issue-validator"
}
```
"#;
const UNKNOWN_AGENT_ASSIGNMENT_BODY: &str = r#"WorkGraphTaskAssignment/v1

```json
{
  "agentId": "issue-risk-profiler"
}
```
"#;
const RESULT_BODY: &str = r#"WorkGraphTaskResult/v1

```json
{
  "taskType": "validate-issue",
  "leaseId": "00000000-0000-7000-8000-000000000001",
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
    mount_snapshot_with_assignment(server, task_body, ASSIGNMENT_BODY, result_body).await;
}

async fn mount_snapshot_with_assignment(
    server: &MockServer,
    task_body: &str,
    assignment_body: &str,
    result_body: &str,
) {
    mount_snapshot_with_comments(
        server,
        task_body,
        assignment_body,
        result_body,
        "IC_closed_result",
        RESULT_BODY,
    )
    .await;
}

async fn mount_snapshot_with_comments(
    server: &MockServer,
    task_body: &str,
    assignment_body: &str,
    result_body: &str,
    closed_comment_id: &str,
    closed_comment_body: &str,
) {
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
            comment("IC_assignment",assignment_body),
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
            comment(closed_comment_id,closed_comment_body)
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

const AGENT_FILE: &str =
    "version: 1\nagents:\n  - agentId: issue-validator\n    slots: 2\n    leaseDuration: PT15M\n";

fn agent_location() -> AgentFileLocation {
    AgentFileLocation {
        repository: "acme/widgets".to_string(),
        r#ref: "main".to_string(),
        path: ".github/workgraph/agents.yaml".to_string(),
    }
}

/// Mount the shared agent-file blob query with an explicit GraphQL response.
async fn mount_agent_file(server: &MockServer, object: Value) {
    mount_query(
        server,
        &["object(expression: $expression)"],
        json!({ "repository": { "object": object } }),
        Some(1),
    )
    .await;
}

async fn mount_agent_blob(server: &MockServer, text: &str) {
    mount_agent_file(
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

fn bootstrap_protocol_trust() -> LeaseTrust {
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

fn agent_provider(server: &MockServer) -> GitHubWorkGraphBootstrapProvider {
    GitHubWorkGraphBootstrapProvider::builder()
        .with_organization("acme")
        .with_task_issue_type(task_type())
        .with_token("read-only-token")
        .with_api_base_url(format!("{}/graphql", server.uri()))
        .with_max_concurrency(2)
        .with_agent_config(agent_location())
        .with_lease_trust(bootstrap_protocol_trust())
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
async fn agent_snapshot_precedes_every_task_artifact() {
    let server = MockServer::start().await;
    mount_agent_blob(&server, AGENT_FILE).await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

    let (result, events) = run(&agent_provider(&server), request()).await;
    assert_eq!(result.event_count, events.len());
    assert!(events
        .iter()
        .all(|event| matches!(event.change, SourceChange::Insert { .. })));

    for node_id in [
        "workgraph-agent:issue-validator",
        "workgraph-agent-slot:issue-validator/1",
        "workgraph-agent-slot:issue-validator/2",
    ] {
        assert!(
            events.iter().any(|event| id(event) == node_id),
            "missing {node_id}"
        );
    }
    assert!(events.iter().any(|event| label(event) == "HAS_SLOT"));

    // Every agent element is snapshotted before the organization, the
    // repositories, and any Issue, task, or task comment that references it.
    let last_agent = events
        .iter()
        .rposition(|event| {
            matches!(
                label(event),
                "WorkGraphAgent" | "WorkGraphAgentSlot" | "HAS_SLOT"
            )
        })
        .expect("agent elements are snapshotted");
    let first_other = events
        .iter()
        .position(|event| {
            !matches!(
                label(event),
                "WorkGraphAgent" | "WorkGraphAgentSlot" | "HAS_SLOT"
            )
        })
        .expect("task artifacts are snapshotted");
    assert!(
        last_agent < first_other,
        "agent elements must precede all other snapshot state"
    );
    assert!(position(&events, "I_task_open") > last_agent);

    assert_eq!(
        node_property(
            &events,
            "workgraph-agent:issue-validator",
            "configuredSlotCount"
        ),
        &ElementValue::Integer(2)
    );
    assert_eq!(
        node_property(
            &events,
            "workgraph-agent:issue-validator",
            "leaseDurationSeconds"
        ),
        &ElementValue::Integer(900)
    );
    assert_eq!(
        node_property(&events, "workgraph-agent-slot:issue-validator/2", "enabled"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&events, "workgraph-agent:issue-validator", "configPath"),
        &ElementValue::from(&json!(".github/workgraph/agents.yaml"))
    );

    // Protocol artifacts remain ordinary snapshot graph data; bootstrap never
    // synthesizes Source-owned active Lease state.
    assert_eq!(
        node_property(&events, "IC_assignment", "version"),
        &ElementValue::Integer(1)
    );
    assert_eq!(
        node_property(&events, "IC_assignment", "agentId"),
        &ElementValue::from(&json!("issue-validator"))
    );
    assert_eq!(
        node_property(&events, "IC_assignment", "trusted"),
        &ElementValue::Bool(true)
    );
    assert_eq!(
        node_property(&events, "IC_result", "version"),
        &ElementValue::Integer(1)
    );
    assert_eq!(
        node_property(&events, "IC_result", "leaseId"),
        &ElementValue::from(&json!("00000000-0000-7000-8000-000000000001"))
    );
    assert!(events.iter().any(|event| label(event) == "ASSIGNMENT_FOR"));
    assert!(events.iter().any(|event| label(event) == "ASSIGNED_TO"));
    assert!(events.iter().any(|event| label(event) == "RESULT_FOR"));
    for absent in [
        "RESULT_FOR_LEASE",
        "WorkGraphTaskLease",
        "LEASE_FOR",
        "LEASES_SLOT",
    ] {
        assert!(
            !events.iter().any(|event| label(event) == absent),
            "{absent} must not appear for a v1-only task"
        );
    }
}

#[tokio::test]
async fn unknown_agent_assignment_is_visible_but_untrusted_in_snapshot() {
    let server = MockServer::start().await;
    mount_agent_blob(&server, AGENT_FILE).await;
    mount_snapshot_with_assignment(
        &server,
        TASK_BODY,
        UNKNOWN_AGENT_ASSIGNMENT_BODY,
        RESULT_BODY,
    )
    .await;

    let (_, events) = run(&agent_provider(&server), request()).await;
    assert_eq!(
        node_property(&events, "IC_assignment", "agentId"),
        &ElementValue::from(&json!("issue-risk-profiler"))
    );
    assert_eq!(
        node_property(&events, "IC_assignment", "trusted"),
        &ElementValue::Bool(false)
    );
    assert!(events.iter().any(|event| {
        id(event) == "ASSIGNED_TO:IC_assignment:workgraph-agent:issue-risk-profiler"
            && label(event) == "ASSIGNED_TO"
    }));
    assert!(!events
        .iter()
        .any(|event| id(event) == "workgraph-agent:issue-risk-profiler"));
}

#[tokio::test]
async fn closed_task_assignment_is_visible_but_untrusted_in_snapshot() {
    let server = MockServer::start().await;
    mount_agent_blob(&server, AGENT_FILE).await;
    mount_snapshot_with_comments(
        &server,
        TASK_BODY,
        ASSIGNMENT_BODY,
        RESULT_BODY,
        "IC_closed_assignment",
        ASSIGNMENT_BODY,
    )
    .await;

    let (_, events) = run(&agent_provider(&server), request()).await;
    assert_eq!(
        node_property(&events, "IC_closed_assignment", "agentId"),
        &ElementValue::from(&json!("issue-validator"))
    );
    assert_eq!(
        node_property(&events, "IC_closed_assignment", "trusted"),
        &ElementValue::Bool(false)
    );
    assert!(events.iter().any(|event| {
        id(event) == "ASSIGNED_TO:IC_closed_assignment:workgraph-agent:issue-validator"
            && label(event) == "ASSIGNED_TO"
    }));
    assert!(!events
        .iter()
        .any(|event| label(event) == "WorkGraphTaskLease"));
}

#[tokio::test]
async fn malformed_agent_file_snapshots_an_error_and_no_agents() {
    for body in [
        "version: 1\nagents: []\n",
        "version: 2\nagents:\n  - agentId: w\n    slots: 1\n    leaseDuration: PT1M\n",
        "agents: not-a-list\n",
        "version: 1\nagents:\n  - agentId: w\n    slots: 1\n    leaseDuration: PT1M\n  - agentId: w\n    slots: 1\n    leaseDuration: PT1M\n",
    ] {
        let server = MockServer::start().await;
        mount_agent_blob(&server, body).await;
        mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

        let (_, events) = run(&agent_provider(&server), request()).await;
        assert!(
            events
                .iter()
                .any(|event| id(event) == "workgraph-error:agent-config"),
            "expected an agent-config error for: {body}"
        );
        assert!(
            !events.iter().any(|event| matches!(
                label(event),
                "WorkGraphAgent" | "WorkGraphAgentSlot" | "HAS_SLOT"
            )),
            "a rejected agent file must never project an agent pool: {body}"
        );
        assert_eq!(
            node_property(&events, "workgraph-error:agent-config", "errorKind"),
            &ElementValue::from(&json!("invalid-workgraph-agent-config"))
        );
        // The rest of the snapshot still completes.
        assert!(events.iter().any(|event| id(event) == "I_task_open"));
    }
}

#[tokio::test]
async fn missing_agent_file_is_a_configuration_error_not_an_empty_pool() {
    let server = MockServer::start().await;
    mount_agent_file(&server, Value::Null).await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

    let (_, events) = run(&agent_provider(&server), request()).await;
    assert_eq!(
        node_property(&events, "workgraph-error:agent-config", "errorCode"),
        &ElementValue::from(&json!("agent-file-unavailable"))
    );
    assert!(!events.iter().any(|event| label(event) == "WorkGraphAgent"));
}

#[tokio::test]
async fn oversized_or_binary_agent_file_is_rejected() {
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
        // A tree at the configured path is not an agent file.
        json!({ "__typename": "Tree" }),
    ] {
        let server = MockServer::start().await;
        mount_agent_file(&server, object.clone()).await;
        mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

        let (_, events) = run(&agent_provider(&server), request()).await;
        assert!(
            events
                .iter()
                .any(|event| id(event) == "workgraph-error:agent-config"),
            "expected rejection for: {object}"
        );
        assert!(!events.iter().any(|event| label(event) == "WorkGraphAgent"));
    }
}

#[tokio::test]
async fn unreadable_agent_file_fails_the_bootstrap() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("object(expression: $expression)"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let context = BootstrapContext::new_minimal("server".to_string(), "gh".to_string());
    let (tx, _rx) = tokio::sync::mpsc::channel(512);
    let error = agent_provider(&server)
        .bootstrap(request(), &context, tx, None)
        .await
        .expect_err("an unreadable agent file must fail the bootstrap");
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("agent file"),
        "unexpected error: {rendered}"
    );
}

#[tokio::test]
async fn omitted_agent_config_snapshots_no_agent_elements() {
    let server = MockServer::start().await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

    let (_, events) = run(&provider(&server, vec![]), request()).await;
    assert!(!events.iter().any(|event| matches!(
        label(event),
        "WorkGraphAgent" | "WorkGraphAgentSlot" | "HAS_SLOT"
    )));
    assert!(!events
        .iter()
        .any(|event| id(event) == "workgraph-error:agent-config"));
    assert!(events.iter().any(|event| id(event) == "I_task_open"));
}

#[tokio::test]
async fn requested_labels_filter_the_agent_snapshot() {
    let server = MockServer::start().await;
    mount_agent_blob(&server, AGENT_FILE).await;
    mount_snapshot(&server, TASK_BODY, RESULT_BODY).await;

    let request = BootstrapRequest {
        node_labels: vec!["WorkGraphAgentSlot".to_string()],
        ..request()
    };
    let (_, events) = run(&agent_provider(&server), request).await;
    assert!(!events.is_empty());
    assert!(events
        .iter()
        .all(|event| label(event) == "WorkGraphAgentSlot"));
}
