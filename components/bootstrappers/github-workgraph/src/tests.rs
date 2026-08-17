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
use crate::GitHubWorkGraphBootstrapProvider;
use drasi_core::models::{Element, SourceChange};
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::BootstrapEvent;
use drasi_source_github_workgraph::config::TaskIssueType;
use serde_json::{json, Value};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TASK_TYPE_ID: &str = "IT_test";
const TASK_TYPE_NAME: &str = "WorkGraphTask";
const TASK_BODY: &str = r#"{
  "assignmentId": "bootstrap-task",
  "agentProfile": "issue-validator",
  "priority": 3,
  "taskType": "issue-validation",
  "task": {
    "validationProfile": "default"
  }
}"#;
const RESULT_BODY: &str = r#"WorkGraphTaskResult/v1

```json
{
  "assignmentId": "bootstrap-task",
  "taskType": "issue-validation",
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
        "defaultBranchRef":{"name":"main"},"repositoryTopics":{"nodes":[]}
    })
}

fn labels() -> Value {
    json!({"pageInfo":{"hasNextPage":false,"endCursor":null},"nodes":[]})
}

fn issue(id: &str, body: &str, state: &str, typed: bool, comments: u64) -> Value {
    json!({
        "node_id":id,"id":10,"number":10,"title":id,"body":body,"state":state,
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
        json!({"repository":{"issues":connection(
            vec![issue("I_generic","Generic body","OPEN",false,0)],false,None
        )}}),
        Some(1),
    )
    .await;
    mount_query(
        server,
        &["search(query:", "state:open"],
        json!({"search":connection(
            vec![task("I_task_open","OPEN",task_body,2)],false,None
        )}),
        Some(1),
    )
    .await;
    mount_query(
        server,
        &["search(query:", "state:closed"],
        json!({"search":connection(
            vec![task("I_task_closed","CLOSED",TASK_BODY,1)],false,None
        )}),
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
            comment("IC_result",result_body)
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
    assert!(events
        .iter()
        .any(|event| id(event) == "I_parent" && label(event) == "GitHubIssue"));
    assert!(events
        .iter()
        .any(|event| id(event) == "IC_plain" && label(event) == "GitHubIssueComment"));
    for result_id in ["IC_result", "IC_closed_result"] {
        assert!(events
            .iter()
            .any(|event| id(event) == result_id && label(event) == "WorkGraphTaskResult"));
    }
    assert_eq!(
        events
            .iter()
            .filter(|event| label(event) == "TASK_FOR")
            .count(),
        2
    );
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
        &["search(query:", "state:open", "\"cursor\":null"],
        json!({"search":connection(
            vec![task("I_open_1","OPEN",TASK_BODY,0),wrong_type],true,Some("NEXT")
        )}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["search(query:", "state:open", "\"cursor\":\"NEXT\""],
        json!({"search":connection(
            vec![task("I_open_2","OPEN",TASK_BODY,0)],false,None
        )}),
        Some(1),
    )
    .await;
    mount_query(
        &server,
        &["search(query:", "state:closed"],
        json!({"search":connection(
            vec![task("I_closed","CLOSED",TASK_BODY,0)],false,None
        )}),
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
    for state in ["open", "closed"] {
        mount_query(
            &server,
            &["search(query:", &format!("state:{state}")],
            json!({"search":connection(vec![],false,None)}),
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
