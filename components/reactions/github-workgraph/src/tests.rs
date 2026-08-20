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

use std::collections::HashMap;

use chrono::{TimeZone, Utc};
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::Reaction;
use drasi_plugin_sdk::ReactionPluginDescriptor;
use serde_json::{json, Value};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::descriptor::GitHubWorkGraphReactionDescriptor;
use crate::model::{lease_comment, DispatchableTask};
use crate::reaction::DispatcherEngine;
use crate::{GitHubWorkGraphReaction, GitHubWorkGraphReactionConfig};

fn task(number: u64, node_id: &str) -> Value {
    task_in_scope(number, node_id, "queue-owner", "queue-repo", "worker-a")
}

fn task_in_scope(
    number: u64,
    node_id: &str,
    repository_owner: &str,
    repository_name: &str,
    worker_id: &str,
) -> Value {
    json!({
        "taskNodeId": node_id,
        "taskNumber": number,
        "repositoryOwner": repository_owner,
        "repositoryName": repository_name,
        "assignmentCommentNodeId": format!("IC-{node_id}"),
        "workerId": worker_id,
        "unusedMetadata": {"ignored": true}
    })
}

fn row(active: &[String], slots: &[&str], tasks: Vec<Value>) -> Value {
    row_in_scope(
        "queue-owner",
        "queue-repo",
        "worker-a",
        active,
        slots,
        tasks,
    )
}

fn row_in_scope(
    repository_owner: &str,
    repository_name: &str,
    worker_id: &str,
    active: &[String],
    slots: &[&str],
    tasks: Vec<Value>,
) -> Value {
    json!({
        "repositoryOwner": repository_owner,
        "repositoryName": repository_name,
        "workerId": worker_id,
        "leaseDurationSeconds": 900,
        "activeLeaseIds": active,
        "freeSlotIds": slots,
        "dispatchableTasks": tasks,
        "agentProfile": "issue-validator",
        "configuredSlotCount": 2,
        "activeLeaseCount": active.len(),
        "dispatchableTaskIds": ["ignored"]
    })
}

fn result(value: Value) -> QueryResult {
    QueryResult::new(
        "capacity".to_string(),
        1,
        Utc::now(),
        vec![ResultDiff::Aggregation {
            before: None,
            after: value,
            row_signature: 1,
        }],
        HashMap::new(),
    )
}

fn engine(server: &MockServer) -> DispatcherEngine {
    let config = GitHubWorkGraphReactionConfig::new("test-token").with_api_base_url(server.uri());
    let reaction =
        GitHubWorkGraphReaction::new("dispatcher", vec!["capacity".into()], config, true)
            .expect("valid reaction");
    DispatcherEngine::new(reaction.build_client().expect("valid client"), server.uri())
}

#[test]
fn canonical_comment_body_is_exact() {
    let task = DispatchableTask {
        task_node_id: "I_task".into(),
        task_number: 42,
        repository_owner: "acme".into(),
        repository_name: "widgets".into(),
        assignment_comment_node_id: "IC_assignment".into(),
        worker_id: "validator-1".into(),
    };
    let acquired = Utc
        .with_ymd_and_hms(2026, 8, 19, 22, 0, 0)
        .single()
        .unwrap();
    let body = lease_comment(
        "0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21",
        "validator-1/1",
        &task,
        acquired,
        900,
    )
    .unwrap();

    assert_eq!(
        body,
        "WorkGraphTaskLease/v1\n\n```json\n{\n  \"leaseId\": \
         \"0198d8c4-7c28-7d43-a8dd-e9f5be8c1b21\",\n  \"assignmentCommentNodeId\": \
         \"IC_assignment\",\n  \"workerId\": \"validator-1\",\n  \"slotId\": \
         \"validator-1/1\",\n  \"acquiredAt\": \"2026-08-19T22:00:00Z\",\n  \"expiresAt\": \
         \"2026-08-19T22:15:00Z\"\n}\n```\n"
    );
}

#[tokio::test]
async fn pairs_in_supplied_order_and_sends_exact_request_shape() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201))
        .expect(2)
        .mount(&server)
        .await;
    let mut engine = engine(&server);

    engine
        .process_query_result(&result(row(
            &[],
            &["worker-a/2", "worker-a/1", "worker-a/3"],
            vec![task(22, "I_second"), task(11, "I_first")],
        )))
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].url.path(),
        "/repos/queue-owner/queue-repo/issues/22/comments"
    );
    assert_eq!(
        requests[1].url.path(),
        "/repos/queue-owner/queue-repo/issues/11/comments"
    );
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer test-token"
    );
    assert_eq!(
        requests[0].headers.get("accept").unwrap().to_str().unwrap(),
        "application/vnd.github+json"
    );
    let first: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let first_body = first["body"].as_str().unwrap();
    assert!(first_body.contains("\"slotId\": \"worker-a/2\""));
    assert!(first_body.contains("\"assignmentCommentNodeId\": \"IC-I_second\""));
    assert!(first_body.starts_with("WorkGraphTaskLease/v1\n\n```json\n"));
    assert!(first_body.ends_with("\n```\n"));
}

#[tokio::test]
async fn pending_suppresses_repeats_until_exact_active_lease_acknowledgment() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(201))
        .mount(&server)
        .await;
    let mut engine = engine(&server);
    let capacity = row(&[], &["worker-a/1"], vec![task(42, "I_task")]);

    engine
        .process_query_result(&result(capacity.clone()))
        .await
        .unwrap();
    engine
        .process_query_result(&result(capacity.clone()))
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(engine.pending().len(), 1);

    let lease_id = engine.pending().keys().next().unwrap().clone();
    engine
        .process_query_result(&result(row_in_scope(
            "other-owner",
            "other-repo",
            "worker-b",
            std::slice::from_ref(&lease_id),
            &["worker-a/1"],
            vec![task_in_scope(
                42,
                "I_task",
                "other-owner",
                "other-repo",
                "worker-b",
            )],
        )))
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
    assert_eq!(engine.pending().len(), 1);

    engine
        .process_query_result(&result(row(&[lease_id], &[], vec![])))
        .await
        .unwrap();
    assert!(engine.pending().is_empty());

    engine
        .process_query_result(&result(capacity))
        .await
        .unwrap();
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    assert_eq!(engine.pending().len(), 1);
}

#[test]
fn embedded_properties_do_not_expose_resolved_token() {
    let reaction = GitHubWorkGraphReaction::new(
        "dispatcher",
        vec!["capacity".into()],
        GitHubWorkGraphReactionConfig::new("literal-resolved-token"),
        true,
    )
    .unwrap();

    assert!(!reaction.properties().contains_key("token"));
    assert_eq!(
        reaction.properties().get("apiBaseUrl"),
        Some(&json!("https://api.github.com"))
    );
}

#[tokio::test]
async fn malformed_rows_and_http_errors_surface_without_pending_entries() {
    let server = MockServer::start().await;
    let mut engine = engine(&server);
    let malformed = json!({
        "repositoryOwner": "queue-owner",
        "repositoryName": "queue-repo",
        "workerId": "worker-a"
    });
    let error = engine
        .process_query_result(&result(malformed))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("malformed capacity row"));
    assert!(engine.pending().is_empty());

    let error = engine
        .process_query_result(&result(row(
            &[],
            &["worker-a/1"],
            vec![task_in_scope(
                42,
                "I_task",
                "queue-owner",
                "queue-repo",
                "worker-b",
            )],
        )))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("workerId does not match"));

    let error = engine
        .process_query_result(&result(row(
            &[],
            &["worker-a/1"],
            vec![task_in_scope(
                42,
                "I_task",
                "other-owner",
                "other-repo",
                "worker-a",
            )],
        )))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("repository does not match"));

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(422))
        .expect(1)
        .mount(&server)
        .await;
    let error = engine
        .process_query_result(&result(row(&[], &["worker-a/1"], vec![task(42, "I_task")])))
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("GitHub issue comment was rejected"));
    assert!(engine.pending().is_empty());
}

#[tokio::test]
async fn descriptor_requires_exactly_one_query_and_preserves_raw_token_reference() {
    let descriptor = GitHubWorkGraphReactionDescriptor;
    let config = json!({
        "token": "${WORKGRAPH_TOKEN:-test-token}",
        "apiBaseUrl": "https://api.github.com"
    });
    for queries in [vec![], vec!["one".into(), "two".into()]] {
        let error = descriptor
            .create_reaction("dispatcher", queries, &config, true)
            .await
            .err()
            .expect("invalid query count");
        assert!(error.to_string().contains("exactly one capacity query"));
    }

    let reaction = descriptor
        .create_reaction("dispatcher", vec!["capacity".into()], &config, true)
        .await
        .unwrap();
    assert_eq!(
        reaction.properties().get("token"),
        Some(&json!("${WORKGRAPH_TOKEN:-test-token}"))
    );
}
