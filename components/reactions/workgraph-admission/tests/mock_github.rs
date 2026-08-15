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

//! A small stateful GitHub stand-in for admission integration tests.
//!
//! It models the parts of GitHub the reaction actually depends on: an issue
//! with an authoritative body, a pinned profile blob, an append-only comment
//! list, and a Project item whose single-select status changes when the
//! mutation runs. Statefulness is what lets the tests exercise adoption after
//! an ambiguous write and idempotency across restarts.

#![allow(dead_code)]

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
pub const SOURCE_STATUS: &str = "Triage";
pub const ADMITTED_STATUS: &str = "AwaitingValidation";
pub const PROFILE_BLOB_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
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

/// Shared mutable GitHub state.
#[derive(Clone)]
pub struct GithubState {
    pub comments: Arc<Mutex<Vec<Value>>>,
    pub status: Arc<Mutex<String>>,
    pub issue_body: Arc<Mutex<Option<String>>>,
    pub issue_state: Arc<Mutex<String>>,
    pub issue_node_id: Arc<Mutex<String>>,
    pub comment_seq: Arc<AtomicUsize>,
    pub create_comment_calls: Arc<AtomicUsize>,
    pub status_mutations: Arc<AtomicUsize>,
    comment_delay: Arc<Mutex<Option<Duration>>>,
    profile_blob_sha: Arc<Mutex<String>>,
    profile_calls: Arc<AtomicUsize>,
    status_mutation_fails: Arc<AtomicBool>,
}

impl GithubState {
    fn new(issue_node_id: &str, body: Option<&str>) -> Self {
        Self {
            comments: Arc::new(Mutex::new(Vec::new())),
            status: Arc::new(Mutex::new(SOURCE_STATUS.to_string())),
            issue_body: Arc::new(Mutex::new(body.map(ToString::to_string))),
            issue_state: Arc::new(Mutex::new("open".to_string())),
            issue_node_id: Arc::new(Mutex::new(issue_node_id.to_string())),
            comment_seq: Arc::new(AtomicUsize::new(0)),
            create_comment_calls: Arc::new(AtomicUsize::new(0)),
            status_mutations: Arc::new(AtomicUsize::new(0)),
            comment_delay: Arc::new(Mutex::new(None)),
            profile_blob_sha: Arc::new(Mutex::new(PROFILE_BLOB_SHA.to_string())),
            profile_calls: Arc::new(AtomicUsize::new(0)),
            status_mutation_fails: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Number of `POST .../comments` calls that reached the server.
    pub fn create_comment_calls(&self) -> usize {
        self.create_comment_calls.load(Ordering::SeqCst)
    }

    /// Number of applied status mutations.
    pub fn status_mutations(&self) -> usize {
        self.status_mutations.load(Ordering::SeqCst)
    }

    /// Number of profile-blob (contents API) reads that reached the server.
    ///
    /// The pinned profile is immutable for the life of a run, so a resumed or
    /// duplicate delivery must not need this endpoint at all.
    pub fn profile_calls(&self) -> usize {
        self.profile_calls.load(Ordering::SeqCst)
    }

    /// Move the mutable profile path to a different blob, as an ordinary commit
    /// to `profileBaseRef` would.
    pub fn set_profile_blob_sha(&self, sha: &str) {
        *self.profile_blob_sha.lock().expect("profile lock") = sha.to_string();
    }

    /// Make the status mutation fail without applying, as a transient GitHub
    /// error would.
    pub fn set_status_mutation_failure(&self, fails: bool) {
        self.status_mutation_fails.store(fails, Ordering::SeqCst);
    }

    /// Current Project status.
    pub fn status(&self) -> String {
        self.status.lock().expect("status lock").clone()
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
    ///
    /// This is how an ambiguous write is simulated: the server accepts the
    /// comment but the client never sees the response.
    pub fn set_create_comment_delay(&self, delay: Option<Duration>) {
        *self.comment_delay.lock().expect("delay lock") = delay;
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

    /// Seed a pre-existing comment authored by `author`.
    pub fn seed_comment(&self, body: &str, author: &MockAuthor, edited: bool) -> String {
        let index = self.comment_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let node_id = format!("IC_seed{index}");
        self.comments
            .lock()
            .expect("comments lock")
            .push(comment_value(&node_id, body, author, edited));
        node_id
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
            "body": body,
        }))
    }
}

struct ListCommentsResponder(GithubState);

impl Respond for ListCommentsResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let comments = self.0.comments.lock().expect("comments lock").clone();
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
        let value = comment_value(&node_id, &body, &MockAuthor::trusted(), false);
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
                            {
                                "id": STATUS_FIELD_NODE_ID,
                                "name": "Status",
                                "options": [
                                    { "id": "opt-triage", "name": SOURCE_STATUS },
                                    { "id": "opt-awaiting", "name": ADMITTED_STATUS },
                                    { "id": "opt-done", "name": "Done" }
                                ]
                            }
                        ]
                    }
                },
                "item": {
                    "id": PROJECT_ITEM_NODE_ID,
                    "project": { "id": PROJECT_NODE_ID },
                    "content": {
                        "__typename": "Issue",
                        "id": *self.0.issue_node_id.lock().expect("node lock"),
                        "number": 742,
                        "repository": { "nameWithOwner": REPOSITORY }
                    },
                    "fieldValueByName": { "name": status, "optionId": "opt-current" }
                }
            }
        }))
    }
}

struct ProfileBlobResponder(GithubState);

impl Respond for ProfileBlobResponder {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        self.0.profile_calls.fetch_add(1, Ordering::SeqCst);
        let sha = self
            .0
            .profile_blob_sha
            .lock()
            .expect("profile lock")
            .clone();
        ResponseTemplate::new(200).set_body_json(json!({
            "sha": sha,
            "path": ".github/agents/issue-validator.agent.md",
        }))
    }
}

struct UpdateStatusResponder(GithubState);

impl Respond for UpdateStatusResponder {
    fn respond(&self, request: &Request) -> ResponseTemplate {
        if self.0.status_mutation_fails.load(Ordering::SeqCst) {
            // The mutation never applies, so the status is unchanged and the
            // client sees a hard failure.
            return ResponseTemplate::new(500).set_body_string("status mutation unavailable");
        }
        let payload: Value = serde_json::from_slice(&request.body).expect("graphql body is JSON");
        let option = payload["variables"]["statusOptionId"]
            .as_str()
            .unwrap_or_default();
        let status = match option {
            "opt-awaiting" => ADMITTED_STATUS,
            "opt-triage" => SOURCE_STATUS,
            _ => "Done",
        };
        *self.0.status.lock().expect("status lock") = status.to_string();
        self.0.status_mutations.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(json!({
            "data": {
                "updateProjectV2ItemFieldValue": {
                    "projectV2Item": { "id": PROJECT_ITEM_NODE_ID }
                }
            }
        }))
    }
}

/// Mount every endpoint the admission reaction uses and return shared state.
pub async fn mount(
    server: &MockServer,
    issue_node_id: &str,
    issue_body: Option<&str>,
) -> GithubState {
    let state = GithubState::new(issue_node_id, issue_body);

    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/issues/742")))
        .respond_with(IssueResponder(state.clone()))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path_regex(r"^/repos/[^/]+/[^/]+/contents/.*$"))
        .respond_with(ProfileBlobResponder(state.clone()))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(format!("/repos/{OWNER}/{REPO}/issues/742/comments")))
        .respond_with(ListCommentsResponder(state.clone()))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path(format!("/repos/{OWNER}/{REPO}/issues/742/comments")))
        .respond_with(CreateCommentResponder(state.clone()))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphAdmissionProjectSnapshot"))
        .respond_with(ProjectSnapshotResponder(state.clone()))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains(
            "WorkgraphAdmissionUpdateProjectStatus",
        ))
        .respond_with(UpdateStatusResponder(state.clone()))
        .mount(server)
        .await;

    state
}
