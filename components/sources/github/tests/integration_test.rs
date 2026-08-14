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

#![allow(clippy::unwrap_used)]

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use drasi_lib::channels::ResultDiff;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::wal::CapacityPolicy;
use drasi_lib::wal::WalProvider;
use drasi_lib::{DrasiLib, DurabilityConfig, Query};
use drasi_reaction_application::subscription::SubscriptionOptions;
use drasi_reaction_application::ApplicationReaction;
use drasi_source_github::config::{GitHubSourceConfig, ProjectSpec, WebhookConfig};
use drasi_source_github::source::GitHubSourceBuilder;
use drasi_state_store_redb::RedbStateStoreProvider;
use drasi_wal_redb::RedbWalProvider;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::json;
use sha2::Sha256;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::RwLock;
use tokio::time::sleep;

#[derive(Clone, Default)]
struct MockGitHubState {
    issue_exists: Arc<RwLock<bool>>,
    issue_title: Arc<RwLock<String>>,
    force_error: Arc<RwLock<bool>>,
    project_item_status: Arc<RwLock<String>>,
}

async fn find_available_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);
    sleep(Duration::from_millis(50)).await;
    port
}

fn sign(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn send_webhook(
    client: &Client,
    port: u16,
    path: &str,
    secret: &str,
    delivery_id: &str,
    event: &str,
    payload: serde_json::Value,
) -> reqwest::Response {
    let body = serde_json::to_vec(&payload).unwrap();
    let signature = sign(secret, &body);
    client
        .post(format!("http://127.0.0.1:{port}{path}"))
        .header("X-Hub-Signature-256", signature)
        .header("X-GitHub-Delivery", delivery_id)
        .header("X-GitHub-Event", event)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn wait_for_result<F>(
    subscription: &mut drasi_reaction_application::subscription::Subscription,
    description: &str,
    predicate: F,
) where
    F: Fn(&ResultDiff) -> bool,
{
    for _ in 0..80 {
        if let Some(result) = subscription.try_recv() {
            if result.results.iter().any(&predicate) {
                return;
            }
        }
        sleep(Duration::from_millis(150)).await;
    }
    panic!("timed out waiting for {description}");
}

async fn mock_graphql_handler(
    State(state): State<MockGitHubState>,
    Json(payload): Json<serde_json::Value>,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let query = payload
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let variables = payload.get("variables").cloned().unwrap_or(json!({}));
    let id = variables
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if query.contains("ProjectV2Item") && id == "PVTI_1" {
        let status_name = state.project_item_status.read().await.clone();
        return (
            axum::http::StatusCode::OK,
            Json(json!({
                "data": {
                    "node": {
                        "__typename": "ProjectV2Item",
                        "id": "PVTI_1",
                        "type": "ISSUE",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "project": {
                            "id": "PVT_1",
                            "number": 1,
                            "owner": { "login": "acme" }
                        },
                        "content": {
                            "__typename": "Issue",
                            "id": "I_1",
                            "number": 1,
                            "title": "issue title",
                            "state": "OPEN",
                            "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                        },
                        "fieldValues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [
                                {
                                    "__typename": "ProjectV2ItemFieldSingleSelectValue",
                                    "name": status_name,
                                    "optionId": "opt1",
                                    "field": { "id": "status_field", "name": "Status" }
                                }
                            ]
                        }
                    }
                }
            })),
        );
    }

    if query.contains("ProjectV2Item") && id == "PVTI_999" {
        return (
            axum::http::StatusCode::OK,
            Json(json!({
                "data": {
                    "node": {
                        "__typename": "ProjectV2Item",
                        "id": "PVTI_999",
                        "type": "ISSUE",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "project": {
                            "id": "PVT_999",
                            "number": 999,
                            "owner": { "login": "different-org" }
                        },
                        "content": null,
                        "fieldValues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }
                }
            })),
        );
    }

    if *state.force_error.read().await && query.contains("... on Issue") {
        return (
            axum::http::StatusCode::OK,
            Json(json!({ "errors": [ { "message": "transient lag" } ] })),
        );
    }

    if query.contains("... on Issue") {
        if !*state.issue_exists.read().await {
            return (
                axum::http::StatusCode::OK,
                Json(json!({ "data": { "node": null } })),
            );
        }

        let title = state.issue_title.read().await.clone();
        return (
            axum::http::StatusCode::OK,
            Json(json!({
                "data": {
                    "node": {
                        "__typename": "Issue",
                        "id": "I_1",
                        "number": 1,
                        "title": title,
                        "body": "issue body",
                        "state": "OPEN",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "closedAt": null,
                        "url": "https://github.com/acme/repo/issues/1",
                        "author": { "login": "octocat" },
                        "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                        "assignees": { "nodes": [], "pageInfo": { "hasNextPage": false, "endCursor": null }},
                        "labels": { "nodes": [], "pageInfo": { "hasNextPage": false, "endCursor": null }},
                        "comments": { "nodes": [], "pageInfo": { "hasNextPage": false, "endCursor": null }}
                    }
                }
            })),
        );
    }

    // Any unhandled query returns empty data.
    (axum::http::StatusCode::OK, Json(json!({ "data": {} })))
}

#[tokio::test]
async fn github_source_webhook_pipeline_detects_create_update_delete_and_project_item() {
    let webhook_port = find_available_port().await;
    let graphql_port = find_available_port().await;
    let webhook_path = "/webhook";
    let webhook_secret = "integration-secret";

    let mock_state = MockGitHubState {
        issue_exists: Arc::new(RwLock::new(true)),
        issue_title: Arc::new(RwLock::new("Issue created".to_string())),
        force_error: Arc::new(RwLock::new(false)),
        project_item_status: Arc::new(RwLock::new("In Progress".to_string())),
    };

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{graphql_port}"))
        .await
        .unwrap();
    let app = Router::new()
        .route("/graphql", post(mock_graphql_handler))
        .with_state(mock_state.clone());
    let mock_server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let source = GitHubSourceBuilder::new("github-test-source")
        .with_config(GitHubSourceConfig {
            token: "test-token".to_string(),
            repositories: vec!["acme/repo".to_string()],
            projects: vec![ProjectSpec {
                owner: "acme".to_string(),
                number: 1,
            }],
            webhook: WebhookConfig {
                host: "127.0.0.1".to_string(),
                port: webhook_port,
                path: webhook_path.to_string(),
                secret: webhook_secret.to_string(),
                body_limit_bytes: 1024 * 1024,
            },
            reconcile_interval_secs: 3600,
            durability: DurabilityConfig {
                enabled: true,
                max_events: 16,
                capacity_policy: CapacityPolicy::RejectIncoming,
            },
            graphql_url: format!("http://127.0.0.1:{graphql_port}/graphql"),
            skip_initial_bootstrap: true,
        })
        .build()
        .unwrap();

    let issue_query_id = "issue-query";
    let issue_query = Query::cypher(issue_query_id)
        .query("MATCH (i:GitHubIssue) RETURN i.title AS title")
        .from_source("github-test-source")
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    let project_query_id = "project-item-query";
    let project_query = Query::cypher(project_query_id)
        .query("MATCH (p:GitHubProjectItem) RETURN p.statusName AS statusName")
        .from_source("github-test-source")
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    let (issue_reaction, issue_handle) = ApplicationReaction::builder("issue-reaction")
        .with_query(issue_query_id)
        .build();
    let (project_reaction, project_handle) = ApplicationReaction::builder("project-reaction")
        .with_query(project_query_id)
        .build();

    let tmp = TempDir::new().unwrap();
    let wal = Arc::new(RedbWalProvider::new(tmp.path()));
    let state_store = Arc::new(
        RedbStateStoreProvider::new(tmp.path().join("state.redb"))
            .expect("create durable redb state store"),
    );

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("github-source-int-test")
            .with_source(source)
            .with_query(issue_query)
            .with_query(project_query)
            .with_reaction(issue_reaction)
            .with_reaction(project_reaction)
            .with_state_store_provider(state_store.clone())
            .with_wal_provider(wal.clone())
            .build()
            .await
            .unwrap(),
    );

    core.start().await.unwrap();
    sleep(Duration::from_millis(300)).await;

    let mut issue_subscription = issue_handle
        .subscribe_with_options(
            SubscriptionOptions::default().with_timeout(Duration::from_millis(500)),
        )
        .await
        .unwrap();
    let mut project_subscription = project_handle
        .subscribe_with_options(
            SubscriptionOptions::default().with_timeout(Duration::from_millis(500)),
        )
        .await
        .unwrap();
    let client = Client::new();

    // INSERT
    let insert_payload = json!({
        "action": "opened",
        "issue": { "node_id": "I_1" },
        "repository": { "full_name": "acme/repo" }
    });
    let insert_response = send_webhook(
        &client,
        webhook_port,
        webhook_path,
        webhook_secret,
        "delivery-insert",
        "issues",
        insert_payload,
    )
    .await;
    assert_eq!(insert_response.status(), 200);
    wait_for_result(&mut issue_subscription, "issue insert", |diff| match diff {
        ResultDiff::Add { data, .. } => data.get("title") == Some(&json!("Issue created")),
        _ => false,
    })
    .await;

    // UPDATE
    *mock_state.issue_title.write().await = "Issue updated".to_string();
    let update_payload = json!({
        "action": "edited",
        "issue": { "node_id": "I_1" },
        "repository": { "full_name": "acme/repo" }
    });
    let update_response = send_webhook(
        &client,
        webhook_port,
        webhook_path,
        webhook_secret,
        "delivery-update",
        "issues",
        update_payload,
    )
    .await;
    assert_eq!(update_response.status(), 200);
    wait_for_result(&mut issue_subscription, "issue update", |diff| match diff {
        ResultDiff::Update { after, .. } => after.get("title") == Some(&json!("Issue updated")),
        _ => false,
    })
    .await;

    // DELETE
    *mock_state.issue_exists.write().await = false;
    let delete_payload = json!({
        "action": "deleted",
        "issue": { "node_id": "I_1" },
        "repository": { "full_name": "acme/repo" }
    });
    let delete_response = send_webhook(
        &client,
        webhook_port,
        webhook_path,
        webhook_secret,
        "delivery-delete",
        "issues",
        delete_payload,
    )
    .await;
    assert_eq!(delete_response.status(), 200);
    wait_for_result(&mut issue_subscription, "issue delete", |diff| {
        matches!(diff, ResultDiff::Delete { .. })
    })
    .await;

    // BAD SIGNATURE
    let bad_signature = client
        .post(format!("http://127.0.0.1:{webhook_port}{webhook_path}"))
        .header("X-Hub-Signature-256", "sha256=badsignature")
        .header("X-GitHub-Delivery", "bad-signature")
        .header("X-GitHub-Event", "issues")
        .header("Content-Type", "application/json")
        .body(r#"{"action":"opened","issue":{"node_id":"I_1"}}"#)
        .send()
        .await
        .unwrap();
    assert_eq!(bad_signature.status(), 401);

    // PROJECT ITEM
    let project_item_payload = json!({
        "action": "edited",
        "projects_v2_item": {
            "node_id": "PVTI_1",
            "project_node_id": "PVT_1"
        }
    });
    let project_item_response = send_webhook(
        &client,
        webhook_port,
        webhook_path,
        webhook_secret,
        "delivery-project-item",
        "projects_v2_item",
        project_item_payload,
    )
    .await;
    assert_eq!(project_item_response.status(), 200);
    wait_for_result(
        &mut project_subscription,
        "project item upsert",
        |diff| match diff {
            ResultDiff::Add { data, .. } => data.get("statusName") == Some(&json!("In Progress")),
            ResultDiff::Update { after, .. } => {
                after.get("statusName") == Some(&json!("In Progress"))
            }
            _ => false,
        },
    )
    .await;

    // Project scope enforcement: unconfigured project should be skipped safely.
    let unconfigured_project_payload = json!({
        "action": "edited",
        "projects_v2_item": {
            "node_id": "PVTI_999",
            "project_node_id": "PVT_999"
        }
    });
    let unconfigured_response = send_webhook(
        &client,
        webhook_port,
        webhook_path,
        webhook_secret,
        "delivery-project-unconfigured",
        "projects_v2_item",
        unconfigured_project_payload,
    )
    .await;
    assert_eq!(unconfigured_response.status(), 200);
    sleep(Duration::from_millis(500)).await;
    assert!(project_subscription.try_recv().is_none());

    // DEDUPE CRASH SEAM: append persisted, marker missing, retry must not duplicate WAL.
    *mock_state.issue_exists.write().await = true;
    *mock_state.force_error.write().await = true; // keep delivery in WAL (unpruned) during test
    let duplicate_payload = json!({
        "action": "edited",
        "issue": { "node_id": "I_1" },
        "repository": { "full_name": "acme/repo" }
    });
    let duplicate_response = send_webhook(
        &client,
        webhook_port,
        webhook_path,
        webhook_secret,
        "delivery-dedupe",
        "issues",
        duplicate_payload.clone(),
    )
    .await;
    assert_eq!(duplicate_response.status(), 200);
    sleep(Duration::from_millis(200)).await;

    state_store
        .delete("github-test-source", "dedupe:delivery-dedupe")
        .await
        .unwrap();
    let wal_count_before = wal.event_count("github-test-source").await.unwrap();
    let duplicate_response_2 = send_webhook(
        &client,
        webhook_port,
        webhook_path,
        webhook_secret,
        "delivery-dedupe",
        "issues",
        duplicate_payload,
    )
    .await;
    assert_eq!(duplicate_response_2.status(), 200);
    let wal_count_after = wal.event_count("github-test-source").await.unwrap();
    assert_eq!(wal_count_after, wal_count_before);

    // WAL FULL -> 503 (force poison head so WAL is not pruned).
    let mut saw_503 = false;
    for i in 0..32 {
        let response = send_webhook(
            &client,
            webhook_port,
            webhook_path,
            webhook_secret,
            &format!("delivery-full-{i}"),
            "issues",
            json!({
                "action": "edited",
                "issue": { "node_id": "I_1" },
                "repository": { "full_name": "acme/repo" }
            }),
        )
        .await;
        if response.status() == 503 {
            saw_503 = true;
            break;
        }
    }
    assert!(saw_503, "Expected at least one 503 when WAL becomes full");

    core.stop().await.unwrap();
    mock_server.abort();
}
