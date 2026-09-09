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

mod mock_server;
mod mock_source;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use drasi_lib::component_graph::ComponentGraph;
use drasi_lib::context::ReactionRuntimeContext;
use drasi_lib::{MemoryStateStoreProvider, Reaction};
use drasi_reaction_a2a::{A2AReaction, TerminalUpdatePolicy};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

struct OrderedJsonRpc {
    next: AtomicUsize,
    bodies: Vec<serde_json::Value>,
}

impl Respond for OrderedJsonRpc {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let index = self.next.fetch_add(1, Ordering::SeqCst);
        let body = self
            .bodies
            .get(index)
            .unwrap_or_else(|| self.bodies.last().expect("ordered JSON-RPC bodies"));
        ResponseTemplate::new(200).set_body_json(body.clone())
    }
}

async fn initialize_with_store(reaction: &A2AReaction, store: Arc<MemoryStateStoreProvider>) {
    let (graph, _rx) = ComponentGraph::new("a2a-it");
    let context = ReactionRuntimeContext::new(
        "a2a-it",
        reaction.id(),
        Some(store),
        graph.update_sender(),
        None,
    );
    reaction.initialize(context).await;
}

fn memory_store() -> Arc<MemoryStateStoreProvider> {
    Arc::new(MemoryStateStoreProvider::new())
}

fn make_reaction(server: &MockServer, policy: TerminalUpdatePolicy) -> A2AReaction {
    A2AReaction::builder("a2a-it")
        .with_query("q1")
        .with_endpoint(server.uri())
        .with_result_key_fields(["invoiceId"])
        .with_terminal_update_policy(policy)
        .build()
        .expect("build reaction")
}

async fn wait_for_requests(server: &MockServer, expected: usize, timeout: Duration) {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let count = server.received_requests().await.unwrap_or_default().len();
        if count >= expected {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for {expected} requests, got {count}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn mount_send_message_task(server: &MockServer, state: &str) {
    let state = if state.starts_with("TASK_STATE_") {
        state.to_string()
    } else {
        format!("TASK_STATE_{state}")
    };
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": "1",
            "result": {
                "task": {
                    "id": "task-1",
                    "contextId": "ctx-1",
                    "status": { "state": state }
                }
            }
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn add_update_delete_sendmessage_followup_and_cancel() {
    let server = mock_server::start().await;
    mount_send_message_task(&server, "WORKING").await;

    let reaction = make_reaction(&server, TerminalUpdatePolicy::Replace);
    initialize_with_store(&reaction, memory_store()).await;
    reaction.start().await.expect("start reaction");

    reaction
        .enqueue_query_result(mock_source::add_result(
            "q1",
            1,
            json!({"invoiceId":"INV-1","amount":100}),
        ))
        .await
        .expect("enqueue add");
    wait_for_requests(&server, 1, Duration::from_secs(3)).await;

    reaction
        .enqueue_query_result(mock_source::update_result(
            "q1",
            2,
            json!({"invoiceId":"INV-1","amount":100}),
            json!({"invoiceId":"INV-1","amount":200}),
        ))
        .await
        .expect("enqueue update");
    wait_for_requests(&server, 2, Duration::from_secs(3)).await;

    reaction
        .enqueue_query_result(mock_source::delete_result(
            "q1",
            3,
            json!({"invoiceId":"INV-1","amount":200}),
        ))
        .await
        .expect("enqueue delete");
    wait_for_requests(&server, 3, Duration::from_secs(3)).await;
    reaction.stop().await.expect("stop reaction");

    let requests = server.received_requests().await.expect("read requests");
    let add_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    let update_body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let delete_body: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();

    assert_eq!(add_body["method"], json!("SendMessage"));
    assert_eq!(add_body["params"]["message"]["role"], json!("ROLE_USER"));
    assert_eq!(
        add_body["params"]["message"]["messageId"],
        json!("a2a-it-q1-INV-1-1")
    );
    assert!(add_body["params"]["message"].get("taskId").is_none());
    assert!(add_body["params"]["message"]["parts"][0]
        .get("kind")
        .is_none());
    assert_eq!(
        add_body["params"]["message"]["parts"][0]["mediaType"],
        json!("application/vnd.drasi.change+json")
    );
    assert_eq!(
        add_body["params"]["message"]["parts"][0]["data"]["queryId"],
        json!("q1")
    );
    assert_eq!(update_body["method"], json!("SendMessage"));
    assert_eq!(update_body["params"]["message"]["taskId"], json!("task-1"));
    assert_eq!(delete_body["method"], json!("CancelTask"));
    assert_eq!(delete_body["params"]["id"], json!("task-1"));
}

#[tokio::test]
async fn one_shot_message_has_no_follow_up_or_cancel() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "jsonrpc": "2.0",
            "id": "1",
            "result": {
                "message": {
                    "role": "ROLE_AGENT",
                    "parts": [{"text":"done"}]
                }
            }
        })))
        .mount(&server)
        .await;

    let reaction = make_reaction(&server, TerminalUpdatePolicy::Replace);
    initialize_with_store(&reaction, memory_store()).await;
    reaction.start().await.expect("start reaction");

    reaction
        .enqueue_query_result(mock_source::add_result(
            "q1",
            1,
            json!({"invoiceId":"INV-2"}),
        ))
        .await
        .expect("enqueue add");
    wait_for_requests(&server, 1, Duration::from_secs(3)).await;

    reaction
        .enqueue_query_result(mock_source::update_result(
            "q1",
            2,
            json!({"invoiceId":"INV-2"}),
            json!({"invoiceId":"INV-2","amount":2}),
        ))
        .await
        .expect("enqueue update");
    reaction
        .enqueue_query_result(mock_source::delete_result(
            "q1",
            3,
            json!({"invoiceId":"INV-2"}),
        ))
        .await
        .expect("enqueue delete");
    tokio::time::sleep(Duration::from_millis(400)).await;
    reaction.stop().await.expect("stop reaction");

    let requests = server.received_requests().await.expect("read requests");
    assert_eq!(requests.len(), 1);
    let add_body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(add_body["method"], json!("SendMessage"));
}

#[tokio::test]
async fn follow_up_message_keeps_task_so_delete_still_cancels() {
    let server = mock_server::start().await;
    Mock::given(method("POST"))
        .and(path("/"))
        .respond_with(OrderedJsonRpc {
            next: AtomicUsize::new(0),
            bodies: vec![
                json!({
                    "jsonrpc": "2.0",
                    "id": "1",
                    "result": {
                        "task": {
                            "id": "task-1",
                            "contextId": "ctx-1",
                            "status": { "state": "TASK_STATE_WORKING" }
                        }
                    }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": "1",
                    "result": {
                        "message": {
                            "role": "ROLE_AGENT",
                            "parts": [{"text":"ack"}]
                        }
                    }
                }),
                json!({
                    "jsonrpc": "2.0",
                    "id": "1",
                    "result": { "id": "task-1" }
                }),
            ],
        })
        .mount(&server)
        .await;

    let reaction = make_reaction(&server, TerminalUpdatePolicy::Replace);
    initialize_with_store(&reaction, memory_store()).await;
    reaction.start().await.expect("start reaction");

    reaction
        .enqueue_query_result(mock_source::add_result(
            "q1",
            1,
            json!({"invoiceId":"INV-7"}),
        ))
        .await
        .expect("enqueue add");
    wait_for_requests(&server, 1, Duration::from_secs(3)).await;

    reaction
        .enqueue_query_result(mock_source::update_result(
            "q1",
            2,
            json!({"invoiceId":"INV-7"}),
            json!({"invoiceId":"INV-7","amount":7}),
        ))
        .await
        .expect("enqueue update");
    wait_for_requests(&server, 2, Duration::from_secs(3)).await;

    reaction
        .enqueue_query_result(mock_source::delete_result(
            "q1",
            3,
            json!({"invoiceId":"INV-7"}),
        ))
        .await
        .expect("enqueue delete");
    wait_for_requests(&server, 3, Duration::from_secs(3)).await;
    reaction.stop().await.expect("stop reaction");

    let requests = server.received_requests().await.expect("read requests");
    let update_body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let delete_body: serde_json::Value = serde_json::from_slice(&requests[2].body).unwrap();
    assert_eq!(update_body["method"], json!("SendMessage"));
    assert_eq!(update_body["params"]["message"]["taskId"], json!("task-1"));
    assert_eq!(delete_body["method"], json!("CancelTask"));
    assert_eq!(delete_body["params"]["id"], json!("task-1"));
}

#[tokio::test]
async fn terminal_replace_creates_new_task_without_task_id() {
    let server = mock_server::start().await;
    mount_send_message_task(&server, "COMPLETED").await;

    let reaction = make_reaction(&server, TerminalUpdatePolicy::Replace);
    initialize_with_store(&reaction, memory_store()).await;
    reaction.start().await.expect("start reaction");

    reaction
        .enqueue_query_result(mock_source::add_result(
            "q1",
            1,
            json!({"invoiceId":"INV-3"}),
        ))
        .await
        .expect("enqueue add");
    wait_for_requests(&server, 1, Duration::from_secs(3)).await;

    reaction
        .enqueue_query_result(mock_source::update_result(
            "q1",
            2,
            json!({"invoiceId":"INV-3"}),
            json!({"invoiceId":"INV-3","amount":300}),
        ))
        .await
        .expect("enqueue update");
    wait_for_requests(&server, 2, Duration::from_secs(3)).await;
    reaction.stop().await.expect("stop reaction");

    let requests = server.received_requests().await.expect("read requests");
    let body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(body["method"], json!("SendMessage"));
    assert!(body["params"]["message"]["taskId"].is_null());
}

#[tokio::test]
async fn terminal_ignore_sends_no_additional_rpc() {
    let server = mock_server::start().await;
    mount_send_message_task(&server, "COMPLETED").await;

    let reaction = make_reaction(&server, TerminalUpdatePolicy::Ignore);
    initialize_with_store(&reaction, memory_store()).await;
    reaction.start().await.expect("start reaction");

    reaction
        .enqueue_query_result(mock_source::add_result(
            "q1",
            1,
            json!({"invoiceId":"INV-4"}),
        ))
        .await
        .expect("enqueue add");
    wait_for_requests(&server, 1, Duration::from_secs(3)).await;

    reaction
        .enqueue_query_result(mock_source::update_result(
            "q1",
            2,
            json!({"invoiceId":"INV-4"}),
            json!({"invoiceId":"INV-4","amount":400}),
        ))
        .await
        .expect("enqueue update");
    tokio::time::sleep(Duration::from_millis(400)).await;
    reaction.stop().await.expect("stop reaction");

    let requests = server.received_requests().await.expect("read requests");
    assert_eq!(requests.len(), 1);
}

#[tokio::test]
async fn replay_does_not_issue_extra_create() {
    let server = mock_server::start().await;
    mount_send_message_task(&server, "WORKING").await;

    let reaction = make_reaction(&server, TerminalUpdatePolicy::Replace);
    initialize_with_store(&reaction, memory_store()).await;
    reaction.start().await.expect("start reaction");

    reaction
        .enqueue_query_result(mock_source::add_result(
            "q1",
            7,
            json!({"invoiceId":"INV-5"}),
        ))
        .await
        .expect("enqueue add");
    wait_for_requests(&server, 1, Duration::from_secs(3)).await;

    reaction
        .enqueue_query_result(mock_source::add_result(
            "q1",
            7,
            json!({"invoiceId":"INV-5"}),
        ))
        .await
        .expect("enqueue replay add");
    wait_for_requests(&server, 2, Duration::from_secs(3)).await;
    reaction.stop().await.expect("stop reaction");

    let requests = server.received_requests().await.expect("read requests");
    let replay_body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(replay_body["method"], json!("SendMessage"));
    assert_eq!(replay_body["params"]["message"]["taskId"], json!("task-1"));
}

#[tokio::test]
async fn activation_survives_process_restart() {
    let server = mock_server::start().await;
    mount_send_message_task(&server, "WORKING").await;
    let store = memory_store();

    let first = make_reaction(&server, TerminalUpdatePolicy::Replace);
    initialize_with_store(&first, store.clone()).await;
    first.start().await.expect("start first reaction");
    first
        .enqueue_query_result(mock_source::add_result(
            "q1",
            1,
            json!({"invoiceId":"INV-6"}),
        ))
        .await
        .expect("enqueue add");
    wait_for_requests(&server, 1, Duration::from_secs(3)).await;
    first.stop().await.expect("stop first reaction");

    let second = make_reaction(&server, TerminalUpdatePolicy::Replace);
    initialize_with_store(&second, store).await;
    second.start().await.expect("start second reaction");
    second
        .enqueue_query_result(mock_source::update_result(
            "q1",
            2,
            json!({"invoiceId":"INV-6"}),
            json!({"invoiceId":"INV-6","amount":6}),
        ))
        .await
        .expect("enqueue update after restart");
    wait_for_requests(&server, 2, Duration::from_secs(3)).await;
    second.stop().await.expect("stop second reaction");

    let requests = server.received_requests().await.expect("read requests");
    let update_body: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert_eq!(update_body["method"], json!("SendMessage"));
    assert_eq!(update_body["params"]["message"]["taskId"], json!("task-1"));
}
