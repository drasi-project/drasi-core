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

//! A small stateful GitHub stand-in for Copilot Agent Task integration tests.
//!
//! It models the parts of GitHub the reaction actually depends on: an issue
//! with an authoritative body, a pinned agent-profile blob, an append-only
//! comment list, a Project item with a single-select status, and the agent-task
//! list/create endpoints. Statefulness is what lets the tests exercise task
//! adoption after an ambiguous write and comment idempotency across restarts.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use drasi_workgraph_common::trust::ActorType;
use serde_json::{json, Value};
use wiremock::matchers::{body_string_contains, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

pub const OWNER: &str = "drasi-project";
pub const REPO: &str = "drasi-core";
pub const REPOSITORY: &str = "drasi-project/drasi-core";
pub const PROJECT_NODE_ID: &str = "PVT_project";
pub const PROJECT_ITEM_NODE_ID: &str = "PVTI_item";
pub const STATUS_FIELD_NODE_ID: &str = "PVTSSF_status";
pub const AWAITING_VALIDATION: &str = "AwaitingValidation";
pub const PROFILE_NAME: &str = "issue-validator";
pub const PROFILE_BLOB_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
pub const ISSUE_NODE_ID: &str = "I_issue";
pub const ISSUE_NUMBER: u64 = 742;
/// One comment author as GitHub reports it.
///
/// Trust compares the numeric database ID and the actor type only. The node ID
/// is audit data (GitHub may even omit it) and the login is display-only, so
/// both deliberately vary between fixtures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockAuthor {
    pub node_id: Option<String>,
    pub database_id: u64,
    pub actor_type: ActorType,
    pub login: String,
}

impl MockAuthor {
    fn new(node_id: Option<&str>, database_id: u64, actor_type: ActorType, login: &str) -> Self {
        Self {
            node_id: node_id.map(ToString::to_string),
            database_id,
            actor_type,
            login: login.to_string(),
        }
    }

    /// The identity the reaction is configured to trust.
    pub fn trusted() -> Self {
        Self::new(
            Some(TRUSTED_AUTHOR_NODE_ID),
            TRUSTED_AUTHOR_DATABASE_ID,
            ActorType::Bot,
            "workgraph-bot",
        )
    }

    /// A completely different account.
    pub fn untrusted() -> Self {
        Self::new(Some("U_kgDOintruder"), 66, ActorType::User, "mallory")
    }

    /// The trusted account with the audit-only node ID absent: still trusted,
    /// because the node ID never participates in a trust decision.
    pub fn trusted_without_node_id() -> Self {
        Self::new(
            None,
            TRUSTED_AUTHOR_DATABASE_ID,
            ActorType::Bot,
            "workgraph-bot",
        )
    }

    /// The trusted numeric database ID under the wrong actor type.
    pub fn wrong_actor_type() -> Self {
        Self::new(
            Some(TRUSTED_AUTHOR_NODE_ID),
            TRUSTED_AUTHOR_DATABASE_ID,
            ActorType::User,
            "workgraph-bot",
        )
    }

    /// The trusted identity under a renamed login (trust must be unaffected).
    pub fn trusted_renamed() -> Self {
        Self::new(
            Some(TRUSTED_AUTHOR_NODE_ID),
            TRUSTED_AUTHOR_DATABASE_ID,
            ActorType::Bot,
            "renamed-since",
        )
    }

    /// The identity this reaction itself posts under, which is a *different*
    /// account from the one that writes assignments.
    pub fn launcher() -> Self {
        Self::new(
            Some(LAUNCHER_AUTHOR_NODE_ID),
            LAUNCHER_AUTHOR_DATABASE_ID,
            ActorType::Bot,
            "workgraph-launcher",
        )
    }

    fn to_user_json(&self) -> Value {
        let mut user = json!({
            "id": self.database_id,
            "type": self.actor_type.as_str(),
            "login": self.login,
        });
        if let Some(node_id) = &self.node_id {
            user["node_id"] = json!(node_id);
        }
        user
    }
}

/// The audit-only node ID of the trusted account.
pub const TRUSTED_AUTHOR_NODE_ID: &str = "U_kgDOworkgraph";
/// One half of the trust key: the trusted account's numeric database ID.
pub const TRUSTED_AUTHOR_DATABASE_ID: u64 = 4_021_243;
/// The other half of the trust key.
pub const TRUSTED_AUTHOR_TYPE: ActorType = ActorType::Bot;

/// The audit-only node ID of the account this reaction posts under.
pub const LAUNCHER_AUTHOR_NODE_ID: &str = "U_kgDOlauncher";
/// The numeric database ID of the account this reaction posts under.
pub const LAUNCHER_AUTHOR_DATABASE_ID: u64 = 90_210;
/// The actor type of the account this reaction posts under.
pub const LAUNCHER_AUTHOR_TYPE: ActorType = ActorType::Bot;

/// Shared mutable GitHub state.
#[derive(Clone)]
pub struct GithubState {
    pub comments: Arc<Mutex<Vec<Value>>>,
    pub tasks: Arc<Mutex<Vec<Value>>>,
    pub status: Arc<Mutex<String>>,
    pub issue_body: Arc<Mutex<Option<String>>>,
    pub issue_state: Arc<Mutex<String>>,
    pub issue_node_id: Arc<Mutex<String>>,
    pub profile_sha: Arc<Mutex<Option<String>>>,
    pub unsupported_models: Arc<Mutex<Vec<String>>>,
    pub forced_task_status: Arc<Mutex<Option<(u16, Value)>>>,
    comment_seq: Arc<AtomicUsize>,
    task_seq: Arc<AtomicUsize>,
    create_comment_calls: Arc<AtomicUsize>,
    create_task_calls: Arc<AtomicUsize>,
    list_task_calls: Arc<AtomicUsize>,
    comment_delay: Arc<Mutex<Option<Duration>>>,
    task_delay: Arc<Mutex<Option<Duration>>>,
    comment_visibility_delay: Arc<Mutex<Option<Duration>>>,
    task_visibility_delay: Arc<Mutex<Option<Duration>>>,
    comment_visible_after: Arc<Mutex<HashMap<String, Instant>>>,
    task_visible_after: Arc<Mutex<HashMap<String, Instant>>>,
}

impl GithubState {
    fn new(issue_node_id: &str, body: Option<&str>) -> Self {
        Self {
            comments: Arc::new(Mutex::new(Vec::new())),
            tasks: Arc::new(Mutex::new(Vec::new())),
            status: Arc::new(Mutex::new(AWAITING_VALIDATION.to_string())),
            issue_body: Arc::new(Mutex::new(body.map(ToString::to_string))),
            issue_state: Arc::new(Mutex::new("open".to_string())),
            issue_node_id: Arc::new(Mutex::new(issue_node_id.to_string())),
            profile_sha: Arc::new(Mutex::new(Some(PROFILE_BLOB_SHA.to_string()))),
            unsupported_models: Arc::new(Mutex::new(Vec::new())),
            forced_task_status: Arc::new(Mutex::new(None)),
            comment_seq: Arc::new(AtomicUsize::new(0)),
            task_seq: Arc::new(AtomicUsize::new(0)),
            create_comment_calls: Arc::new(AtomicUsize::new(0)),
            create_task_calls: Arc::new(AtomicUsize::new(0)),
            list_task_calls: Arc::new(AtomicUsize::new(0)),
            comment_delay: Arc::new(Mutex::new(None)),
            task_delay: Arc::new(Mutex::new(None)),
            comment_visibility_delay: Arc::new(Mutex::new(None)),
            task_visibility_delay: Arc::new(Mutex::new(None)),
            comment_visible_after: Arc::new(Mutex::new(HashMap::new())),
            task_visible_after: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Number of `POST .../comments` calls that reached the server.
    pub fn create_comment_calls(&self) -> usize {
        self.create_comment_calls.load(Ordering::SeqCst)
    }

    /// Number of `POST .../tasks` calls that reached the server.
    pub fn create_task_calls(&self) -> usize {
        self.create_task_calls.load(Ordering::SeqCst)
    }

    /// Number of `GET .../tasks` calls that reached the server.
    pub fn list_task_calls(&self) -> usize {
        self.list_task_calls.load(Ordering::SeqCst)
    }

    /// Current Project status.
    pub fn status(&self) -> String {
        self.status.lock().expect("status lock").clone()
    }

    /// Number of agent tasks recorded (created or seeded).
    pub fn task_count(&self) -> usize {
        self.tasks.lock().expect("tasks lock").len()
    }

    /// Every task prompt currently recorded.
    pub fn task_prompts(&self) -> Vec<String> {
        self.tasks
            .lock()
            .expect("tasks lock")
            .iter()
            .map(|task| task["prompt"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Every comment body currently on the issue.
    pub fn comment_bodies(&self) -> Vec<String> {
        self.comments
            .lock()
            .expect("comments lock")
            .iter()
            .map(|comment| comment["body"].as_str().unwrap_or_default().to_string())
            .collect()
    }

    /// Delay the create-comment *response* while still recording the write.
    pub fn set_create_comment_delay(&self, delay: Option<Duration>) {
        *self.comment_delay.lock().expect("delay lock") = delay;
    }

    /// Delay the create-task *response* while still recording the task. This is
    /// how an ambiguous write is simulated: the server accepts the task but the
    /// client times out before it sees the response.
    pub fn set_create_task_delay(&self, delay: Option<Duration>) {
        *self.task_delay.lock().expect("task delay lock") = delay;
    }

    /// Delay when newly created comments become visible to authoritative lists.
    pub fn set_comment_visibility_delay(&self, delay: Option<Duration>) {
        *self
            .comment_visibility_delay
            .lock()
            .expect("comment visibility delay lock") = delay;
    }

    /// Delay when newly created tasks become visible to authoritative lists.
    pub fn set_task_visibility_delay(&self, delay: Option<Duration>) {
        *self
            .task_visibility_delay
            .lock()
            .expect("task visibility delay lock") = delay;
    }

    /// Mark a model so that creating a task with it returns a clearly
    /// "unsupported model" 422.
    pub fn add_unsupported_model(&self, model: &str) {
        self.unsupported_models
            .lock()
            .expect("model lock")
            .push(model.to_string());
    }

    /// Force the next create-task calls to return a specific status + JSON body
    /// regardless of model (used to exercise unrelated 4xx/5xx handling).
    pub fn force_task_status(&self, status: u16, body: Value) {
        *self.forced_task_status.lock().expect("forced lock") = Some((status, body));
    }

    /// Replace the authoritative issue body.
    pub fn set_issue_body(&self, body: Option<&str>) {
        *self.issue_body.lock().expect("body lock") = body.map(ToString::to_string);
    }

    /// Replace the issue state (`open` / `closed`).
    pub fn set_issue_state(&self, state: &str) {
        *self.issue_state.lock().expect("state lock") = state.to_string();
    }

    /// Replace the issue node ID reported by GitHub.
    pub fn set_issue_node_id(&self, node_id: &str) {
        *self.issue_node_id.lock().expect("node lock") = node_id.to_string();
    }

    /// Replace the Project status.
    pub fn set_status(&self, status: &str) {
        *self.status.lock().expect("status lock") = status.to_string();
    }

    /// Replace the profile blob SHA (`None` => the file 404s).
    pub fn set_profile_sha(&self, sha: Option<&str>) {
        *self.profile_sha.lock().expect("profile lock") = sha.map(ToString::to_string);
    }

    /// Seed a pre-existing comment authored by `user_id`.
    pub fn seed_comment(&self, body: &str, author: &MockAuthor, edited: bool) -> String {
        let index = self.comment_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let node_id = format!("IC_seed{index}");
        self.comments
            .lock()
            .expect("comments lock")
            .push(comment_value(&node_id, body, author, edited));
        node_id
    }

    /// Seed a pre-existing agent task whose prompt is `prompt`.
    pub fn seed_task(&self, prompt: &str) -> String {
        let index = self.task_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("task-seed-{index}");
        self.tasks.lock().expect("tasks lock").push(json!({
            "id": id,
            "html_url": format!("https://github.com/{OWNER}/{REPO}/agents/tasks/{id}"),
            "prompt": prompt,
        }));
        id
    }

    /// Model a task write whose response was lost and whose list visibility is
    /// delayed, as if it came from a previous process.
    pub fn seed_lost_task(&self, prompt: &str, visibility_delay: Duration) -> String {
        self.create_task_calls.fetch_add(1, Ordering::SeqCst);
        let id = self.seed_task(prompt);
        self.task_visible_after
            .lock()
            .expect("task visibility lock")
            .insert(id.clone(), Instant::now() + visibility_delay);
        id
    }

    /// Model a comment write whose response was lost and whose list visibility
    /// is delayed, as if it came from a previous process.
    pub fn seed_lost_comment(&self, body: &str, visibility_delay: Duration) -> String {
        self.create_comment_calls.fetch_add(1, Ordering::SeqCst);
        let id = self.seed_comment(body, &MockAuthor::launcher(), false);
        self.comment_visible_after
            .lock()
            .expect("comment visibility lock")
            .insert(id.clone(), Instant::now() + visibility_delay);
        id
    }
}

fn comment_value(node_id: &str, body: &str, author: &MockAuthor, edited: bool) -> Value {
    json!({
        "id": 1,
        "node_id": node_id,
        "body": body,
        "user": author.to_user_json(),
        "created_at": "2026-08-14T00:00:00Z",
        "updated_at": if edited { "2026-08-14T01:00:00Z" } else { "2026-08-14T00:00:00Z" },
    })
}

struct IssueResponder(GithubState);

impl Respond for IssueResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let body = self.0.issue_body.lock().expect("body lock").clone();
        ResponseTemplate::new(200).set_body_json(json!({
            "node_id": *self.0.issue_node_id.lock().expect("node lock"),
            "state": *self.0.issue_state.lock().expect("state lock"),
            "number": ISSUE_NUMBER,
            "body": body,
        }))
    }
}

struct ContentsResponder(GithubState);

impl Respond for ContentsResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        match self.0.profile_sha.lock().expect("profile lock").clone() {
            Some(sha) => ResponseTemplate::new(200).set_body_json(json!({
                "sha": sha,
                "path": format!(".github/agents/{PROFILE_NAME}.agent.md"),
            })),
            None => ResponseTemplate::new(404).set_body_json(json!({ "message": "Not Found" })),
        }
    }
}

struct ListCommentsResponder(GithubState);

impl Respond for ListCommentsResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let visible_after = self
            .0
            .comment_visible_after
            .lock()
            .expect("comment visibility lock")
            .clone();
        let now = Instant::now();
        let comments = self
            .0
            .comments
            .lock()
            .expect("comments lock")
            .iter()
            .filter(|comment| {
                comment["node_id"]
                    .as_str()
                    .and_then(|id| visible_after.get(id))
                    .is_none_or(|visible_at| *visible_at <= now)
            })
            .cloned()
            .collect();
        ResponseTemplate::new(200).set_body_json(Value::Array(comments))
    }
}

struct CreateCommentResponder(GithubState);

impl Respond for CreateCommentResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.0.create_comment_calls.fetch_add(1, Ordering::SeqCst);
        let payload: Value = serde_json::from_slice(&request.body).expect("comment body is JSON");
        let body = payload["body"].as_str().unwrap_or_default().to_string();
        let index = self.0.comment_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let node_id = format!("IC_created{index}");
        // Comments this reaction posts are authored by its own identity, not
        // by the account that writes assignments.
        let value = comment_value(&node_id, &body, &MockAuthor::launcher(), false);
        if let Some(delay) = *self
            .0
            .comment_visibility_delay
            .lock()
            .expect("comment visibility delay lock")
        {
            self.0
                .comment_visible_after
                .lock()
                .expect("comment visibility lock")
                .insert(node_id.clone(), Instant::now() + delay);
        }
        self.0
            .comments
            .lock()
            .expect("comments lock")
            .push(value.clone());

        let response = ResponseTemplate::new(201).set_body_json(value);
        match *self.0.comment_delay.lock().expect("delay lock") {
            Some(delay) => response.set_delay(delay),
            None => response,
        }
    }
}

struct ListTasksResponder(GithubState);

impl Respond for ListTasksResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.0.list_task_calls.fetch_add(1, Ordering::SeqCst);
        let visible_after = self
            .0
            .task_visible_after
            .lock()
            .expect("task visibility lock")
            .clone();
        let now = Instant::now();
        let tasks = self
            .0
            .tasks
            .lock()
            .expect("tasks lock")
            .iter()
            .filter(|task| {
                task["id"]
                    .as_str()
                    .and_then(|id| visible_after.get(id))
                    .is_none_or(|visible_at| *visible_at <= now)
            })
            .cloned()
            .collect();
        ResponseTemplate::new(200).set_body_json(Value::Array(tasks))
    }
}

struct CreateTaskResponder(GithubState);

impl Respond for CreateTaskResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        self.0.create_task_calls.fetch_add(1, Ordering::SeqCst);
        let payload: Value = serde_json::from_slice(&request.body).expect("task body is JSON");
        let model = payload["model"].as_str().unwrap_or_default().to_string();
        let prompt = payload["prompt"].as_str().unwrap_or_default().to_string();

        if let Some((status, body)) = self
            .0
            .forced_task_status
            .lock()
            .expect("forced lock")
            .clone()
        {
            return ResponseTemplate::new(status).set_body_json(body);
        }

        if self
            .0
            .unsupported_models
            .lock()
            .expect("model lock")
            .iter()
            .any(|m| m == &model)
        {
            return ResponseTemplate::new(422).set_body_json(json!({
                "message": format!("The model {model} is not supported for this operation."),
            }));
        }

        let index = self.0.task_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("task-{index}");
        let html_url = format!("https://github.com/{OWNER}/{REPO}/agents/tasks/{id}");
        if let Some(delay) = *self
            .0
            .task_visibility_delay
            .lock()
            .expect("task visibility delay lock")
        {
            self.0
                .task_visible_after
                .lock()
                .expect("task visibility lock")
                .insert(id.clone(), Instant::now() + delay);
        }
        self.0.tasks.lock().expect("tasks lock").push(json!({
            "id": id,
            "html_url": html_url,
            "prompt": prompt,
        }));

        let response = ResponseTemplate::new(201).set_body_json(json!({
            "id": id,
            "html_url": html_url,
        }));
        match *self.0.task_delay.lock().expect("task delay lock") {
            Some(delay) => response.set_delay(delay),
            None => response,
        }
    }
}

struct ProjectSnapshotResponder(GithubState);

impl Respond for ProjectSnapshotResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let status = self.0.status.lock().expect("status lock").clone();
        ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "project": {
                    "id": PROJECT_NODE_ID,
                    "fields": {
                        "nodes": [
                            { "id": STATUS_FIELD_NODE_ID, "name": "Status" }
                        ]
                    }
                },
                "item": {
                    "id": PROJECT_ITEM_NODE_ID,
                    "project": { "id": PROJECT_NODE_ID },
                    "content": {
                        "__typename": "Issue",
                        "id": *self.0.issue_node_id.lock().expect("node lock"),
                        "number": ISSUE_NUMBER,
                        "repository": { "nameWithOwner": REPOSITORY }
                    },
                    "fieldValueByName": { "name": status }
                }
            }
        }))
    }
}

/// Mount every endpoint the Copilot Agent Task reaction uses and return the
/// shared state so tests can seed inputs and assert on recorded writes.
pub async fn mount(
    server: &MockServer,
    issue_node_id: &str,
    issue_body: Option<&str>,
) -> GithubState {
    let state = GithubState::new(issue_node_id, issue_body);

    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/issues/{ISSUE_NUMBER}")))
        .respond_with(IssueResponder(state.clone()))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/[^/]+/[^/]+/contents/.*$"))
        .respond_with(ContentsResponder(state.clone()))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{OWNER}/{REPO}/issues/{ISSUE_NUMBER}/comments"
        )))
        .respond_with(ListCommentsResponder(state.clone()))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!(
            "/repos/{OWNER}/{REPO}/issues/{ISSUE_NUMBER}/comments"
        )))
        .respond_with(CreateCommentResponder(state.clone()))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/agents/repos/{OWNER}/{REPO}/tasks")))
        .respond_with(ListTasksResponder(state.clone()))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!("/agents/repos/{OWNER}/{REPO}/tasks")))
        .respond_with(CreateTaskResponder(state.clone()))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("CopilotAgentTaskProjectSnapshot"))
        .respond_with(ProjectSnapshotResponder(state.clone()))
        .mount(server)
        .await;

    state
}

/// Mount a `GET /user` responder so the token-owner guard can resolve.
pub async fn mount_authenticated_user(server: &MockServer, user_id: u64) {
    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": user_id,
            "login": "launcher"
        })))
        .mount(server)
        .await;
}
