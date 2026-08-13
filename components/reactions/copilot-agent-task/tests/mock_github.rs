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

//! Shared wiremock helpers standing in for the GitHub REST + GraphQL API.

use serde_json::{json, Value};
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Mount a successful `GET /repos/{owner}/{repo}/issues/{n}` responder.
/// `node_id` is the GraphQL node ID GitHub would report for this issue —
/// callers must pass the same value used as the launch row's `issueNodeId`
/// for the preflight cross-check to pass.
pub async fn mount_issue(
    server: &MockServer,
    owner: &str,
    repo: &str,
    number: u64,
    state: &str,
    body: &str,
    node_id: &str,
) {
    Mock::given(method("GET"))
        .and(path(format!("/repos/{owner}/{repo}/issues/{number}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "state": state,
            "body": body,
            "node_id": node_id,
        })))
        .mount(server)
        .await;
}

/// Mount a `GET /repos/{owner}/{repo}/contents/{path}` responder returning a blob SHA.
pub async fn mount_contents(server: &MockServer, owner: &str, repo: &str, sha: &str) {
    Mock::given(method("GET"))
        .and(path_regex(format!("^/repos/{owner}/{repo}/contents/.*$")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "sha": sha })))
        .mount(server)
        .await;
}

/// Mount the Project (v2) item status GraphQL query responder. `linked_issue_node_id`
/// is the issue node ID the project item is linked to (`content { ... on Issue { id } }`)
/// — must match the launch row's `issueNodeId` for the preflight cross-check to pass.
pub async fn mount_project_status(server: &MockServer, status: &str, linked_issue_node_id: &str) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fieldValueByName"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "node": {
                    "fieldValueByName": { "name": status },
                    "content": { "id": linked_issue_node_id }
                }
            }
        })))
        .mount(server)
        .await;
}

/// Mount the Project (v2) item status GraphQL query responder returning
/// top-level GraphQL `errors` (HTTP 200) — must be treated as a failure.
pub async fn mount_project_status_graphql_error(server: &MockServer, message: &str) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("fieldValueByName"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{ "message": message }]
        })))
        .mount(server)
        .await;
}

/// Mount the `addComment` GraphQL mutation responder (success).
pub async fn mount_add_comment_success(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("addComment"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "addComment": { "commentEdge": { "node": { "id": "IC_kwDOtest" } } } }
        })))
        .mount(server)
        .await;
}

/// Mount the `addComment` GraphQL mutation responder returning top-level
/// GraphQL `errors` (HTTP 200) — must be treated as a failure.
pub async fn mount_add_comment_graphql_error(server: &MockServer, message: &str) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("addComment"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{ "message": message }]
        })))
        .mount(server)
        .await;
}

/// Mount the create-task responder returning HTTP 201 with the given id/url.
pub async fn mount_create_task_success(
    server: &MockServer,
    owner: &str,
    repo: &str,
    task_id: &str,
    task_url: &str,
) {
    Mock::given(method("POST"))
        .and(path(format!("/agents/repos/{owner}/{repo}/tasks")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": task_id,
            "html_url": task_url,
        })))
        .mount(server)
        .await;
}

/// Mount a create-task responder that returns 422 for a specific `model`
/// value (an "unsupported model" response) and 201 for anything else.
pub async fn mount_create_task_unsupported_model(
    server: &MockServer,
    owner: &str,
    repo: &str,
    unsupported_model: &str,
    task_id: &str,
    task_url: &str,
) {
    Mock::given(method("POST"))
        .and(path(format!("/agents/repos/{owner}/{repo}/tasks")))
        .and(body_string_contains(format!("\"model\":\"{unsupported_model}\"")))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({
            "message": format!("The model '{unsupported_model}' is not supported for this operation."),
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("/agents/repos/{owner}/{repo}/tasks")))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": task_id,
            "html_url": task_url,
        })))
        .mount(server)
        .await;
}

/// Mount a create-task responder that always returns a permanent (non-model)
/// 422 validation error.
pub async fn mount_create_task_permanent_422(
    server: &MockServer,
    owner: &str,
    repo: &str,
    message: &str,
) {
    Mock::given(method("POST"))
        .and(path(format!("/agents/repos/{owner}/{repo}/tasks")))
        .respond_with(ResponseTemplate::new(422).set_body_json(json!({ "message": message })))
        .mount(server)
        .await;
}

/// Mount the task-listing responder used by the reconciliation seam.
pub async fn mount_list_tasks(server: &MockServer, owner: &str, repo: &str, tasks: Vec<Value>) {
    Mock::given(method("GET"))
        .and(path(format!("/agents/repos/{owner}/{repo}/tasks")))
        .respond_with(ResponseTemplate::new(200).set_body_json(Value::Array(tasks)))
        .mount(server)
        .await;
}

/// Count how many `POST .../tasks` (create-task) requests the server has
/// received so far (excludes the `GET` listing endpoint).
pub async fn count_create_task_requests(server: &MockServer, owner: &str, repo: &str) -> usize {
    let expected_path = format!("/agents/repos/{owner}/{repo}/tasks");
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str() == "POST" && r.url.path() == expected_path)
        .count()
}

/// Count how many `addComment` GraphQL mutations were sent.
pub async fn count_add_comment_requests(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| {
            r.url.path() == "/graphql" && String::from_utf8_lossy(&r.body).contains("addComment")
        })
        .count()
}
