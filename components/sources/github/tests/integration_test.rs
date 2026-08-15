#![allow(clippy::unwrap_used)]

use async_trait::async_trait;
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use drasi_core::models::{Element, SourceChange};
use drasi_lib::component_graph::ComponentUpdateSender;
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::state_store::{StateStoreProvider, StateStoreResult};
use drasi_lib::wal::{CapacityPolicy, WalProvider};
use drasi_lib::{DurabilityConfig, Source};
use drasi_source_github::config::{GitHubSourceConfig, ProjectSpec, WebhookConfig};
use drasi_source_github::source::GitHubSourceBuilder;
use drasi_state_store_redb::RedbStateStoreProvider;
use drasi_wal_redb::RedbWalProvider;
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde_json::json;
use sha2::Sha256;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::{Mutex, RwLock};
use tokio::time::sleep;

#[derive(Debug, Clone)]
struct MockIssue {
    exists: bool,
    repository: String,
    current_title: String,
    queued_titles: VecDeque<String>,
}

#[derive(Clone, Default)]
struct MockGitHubState {
    issues: Arc<Mutex<HashMap<String, MockIssue>>>,
    graphql_calls: Arc<AtomicUsize>,
    block_issue_fetch: Arc<AtomicBool>,
    project_item_exists: Arc<AtomicBool>,
    project_item_repo: Arc<RwLock<String>>,
    project_item_status: Arc<RwLock<String>>,
}

struct RecordingStateStore {
    inner: Arc<dyn StateStoreProvider>,
    get_keys: Mutex<Vec<String>>,
}

impl RecordingStateStore {
    fn new(inner: Arc<dyn StateStoreProvider>) -> Self {
        Self {
            inner,
            get_keys: Mutex::new(Vec::new()),
        }
    }

    async fn clear_get_keys(&self) {
        self.get_keys.lock().await.clear();
    }

    async fn get_keys(&self) -> Vec<String> {
        self.get_keys.lock().await.clone()
    }
}

#[async_trait]
impl StateStoreProvider for RecordingStateStore {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        self.get_keys.lock().await.push(key.to_string());
        self.inner.get(store_id, key).await
    }

    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        self.inner.set(store_id, key, value).await
    }

    async fn delete(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.delete(store_id, key).await
    }

    async fn contains_key(&self, store_id: &str, key: &str) -> StateStoreResult<bool> {
        self.inner.contains_key(store_id, key).await
    }

    async fn get_many(
        &self,
        store_id: &str,
        keys: &[&str],
    ) -> StateStoreResult<HashMap<String, Vec<u8>>> {
        self.inner.get_many(store_id, keys).await
    }

    async fn set_many(&self, store_id: &str, entries: &[(&str, &[u8])]) -> StateStoreResult<()> {
        self.inner.set_many(store_id, entries).await
    }

    async fn delete_many(&self, store_id: &str, keys: &[&str]) -> StateStoreResult<usize> {
        self.inner.delete_many(store_id, keys).await
    }

    async fn clear_store(&self, store_id: &str) -> StateStoreResult<usize> {
        self.inner.clear_store(store_id).await
    }

    async fn list_keys(&self, store_id: &str) -> StateStoreResult<Vec<String>> {
        self.inner.list_keys(store_id).await
    }

    async fn store_exists(&self, store_id: &str) -> StateStoreResult<bool> {
        self.inner.store_exists(store_id).await
    }

    async fn key_count(&self, store_id: &str) -> StateStoreResult<usize> {
        self.inner.key_count(store_id).await
    }

    async fn sync(&self) -> StateStoreResult<()> {
        self.inner.sync().await
    }

    fn is_durable(&self) -> bool {
        self.inner.is_durable()
    }
}

impl MockGitHubState {
    async fn upsert_issue(&self, id: &str, repository: &str, title: &str, exists: bool) {
        self.issues.lock().await.insert(
            id.to_string(),
            MockIssue {
                exists,
                repository: repository.to_ascii_lowercase(),
                current_title: title.to_string(),
                queued_titles: VecDeque::new(),
            },
        );
    }

    async fn queue_issue_title(&self, id: &str, title: &str) {
        if let Some(issue) = self.issues.lock().await.get_mut(id) {
            issue.queued_titles.push_back(title.to_string());
        }
    }

    async fn set_issue_exists(&self, id: &str, exists: bool) {
        if let Some(issue) = self.issues.lock().await.get_mut(id) {
            issue.exists = exists;
        }
    }

    fn set_project_item_exists(&self, exists: bool) {
        self.project_item_exists.store(exists, Ordering::SeqCst);
    }
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
    harness: &Harness,
    delivery_id: &str,
    event: &str,
    payload: serde_json::Value,
) -> reqwest::Response {
    let body = serde_json::to_vec(&payload).unwrap();
    client
        .post(format!(
            "http://{}:{}{}",
            harness.webhook_host, harness.webhook_port, harness.webhook_path
        ))
        .header("X-Hub-Signature-256", sign(&harness.webhook_secret, &body))
        .header("X-GitHub-Delivery", delivery_id)
        .header("X-GitHub-Event", event)
        .header("Content-Type", "application/json")
        .body(body)
        .send()
        .await
        .unwrap()
}

async fn mock_graphql_handler(
    State(state): State<MockGitHubState>,
    headers: HeaderMap,
    Json(payload): Json<serde_json::Value>,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    state.graphql_calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer test-token")
    );
    let query = payload
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if query.contains("content {") && query.contains("... on Issue") {
        assert!(
            query.contains("issueState: state") && query.contains("pullRequestState: state"),
            "Project item documents must alias incompatible state enum fields"
        );
    }
    let variables = payload.get("variables").cloned().unwrap_or(json!({}));
    let id = variables
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();

    if query.contains("query($owner: String!, $number: Int!)") {
        return (
            axum::http::StatusCode::OK,
            Json(json!({
                "data": {
                    "organization": {
                        "projectV2": {
                            "id": "PVT_1",
                            "title": "Project",
                            "number": 1,
                            "url": "https://github.com/orgs/acme/projects/1",
                            "createdAt": "2026-01-01T00:00:00Z",
                            "updatedAt": "2026-01-01T00:00:00Z",
                            "owner": { "login": "acme" },
                            "items": {
                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                "nodes": [{
                                    "id": "PVTI_DISCOVERY",
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
                                        "id": "I_DISCOVERY",
                                        "number": 99,
                                        "title": "Discovery issue",
                                        "state": "OPEN",
                                        "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                                    },
                                    "fieldValues": {
                                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                                        "nodes": []
                                    }
                                }]
                            }
                        }
                    },
                    "user": null
                }
            })),
        );
    }

    if query.contains("ProjectV2Item") && id == "PVTI_1" {
        if !state.project_item_exists.load(Ordering::SeqCst) {
            return (
                axum::http::StatusCode::OK,
                Json(json!({
                    "data": {
                        "node": null
                    }
                })),
            );
        }
        let status_name = state.project_item_status.read().await.clone();
        let repository = state.project_item_repo.read().await.clone();
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
                            "id": "I_4",
                            "number": 4,
                            "title": "Scope growth issue",
                            "state": "OPEN",
                            "repository": { "id": "R_4", "nameWithOwner": repository }
                        },
                        "fieldValues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "__typename": "ProjectV2ItemFieldSingleSelectValue",
                                "name": status_name,
                                "optionId": "opt1",
                                "field": { "id": "status_field", "name": "Status" }
                            }]
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
                            "owner": { "login": "other-org" }
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

    if query.contains("... on Issue") {
        if state.block_issue_fetch.load(Ordering::SeqCst) {
            return (
                axum::http::StatusCode::OK,
                Json(json!({ "errors": [ { "message": "temporarily unavailable" } ] })),
            );
        }

        let issue = state.issues.lock().await.get_mut(id).cloned();
        let Some(mut issue) = issue else {
            return (
                axum::http::StatusCode::OK,
                Json(json!({ "data": { "node": null } })),
            );
        };

        if !issue.exists {
            return (
                axum::http::StatusCode::OK,
                Json(json!({ "data": { "node": null } })),
            );
        }

        if let Some(next_title) = issue.queued_titles.pop_front() {
            issue.current_title = next_title;
            state
                .issues
                .lock()
                .await
                .insert(id.to_string(), issue.clone());
        }

        return (
            axum::http::StatusCode::OK,
            Json(json!({
                "data": {
                    "node": {
                        "__typename": "Issue",
                        "id": id,
                        "number": 1,
                        "title": issue.current_title,
                        "body": "issue body",
                        "state": "OPEN",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "closedAt": null,
                        "url": format!("https://github.com/{}/issues/1", issue.repository),
                        "author": { "login": "octocat" },
                        "repository": { "id": "R_1", "nameWithOwner": issue.repository },
                        "assignees": { "nodes": [], "pageInfo": { "hasNextPage": false, "endCursor": null }},
                        "labels": { "nodes": [], "pageInfo": { "hasNextPage": false, "endCursor": null }},
                        "comments": { "nodes": [], "pageInfo": { "hasNextPage": false, "endCursor": null }}
                    }
                }
            })),
        );
    }

    (axum::http::StatusCode::OK, Json(json!({ "data": {} })))
}

struct Harness {
    _tmp: TempDir,
    source: drasi_source_github::source::GitHubSource,
    wal: Arc<RedbWalProvider>,
    graphql_server: tokio::task::JoinHandle<()>,
    source_id: String,
    inbox_id: String,
    webhook_host: String,
    webhook_port: u16,
    webhook_path: String,
    webhook_secret: String,
    mock: MockGitHubState,
    state_store: Arc<RecordingStateStore>,
}

async fn build_harness(max_events: u64) -> Harness {
    let tmp = TempDir::new().unwrap();
    let source_id = "github-test-source".to_string();
    let inbox_id = format!("{source_id}::inbox");
    let webhook_host = "127.0.0.1".to_string();
    let webhook_port = find_available_port().await;
    let graphql_port = find_available_port().await;
    let webhook_path = "/webhook".to_string();
    let webhook_secret = "integration-secret".to_string();

    let mock = MockGitHubState::default();
    mock.set_project_item_exists(true);
    mock.project_item_repo
        .write()
        .await
        .clone_from(&"acme/new-repo".to_string());
    mock.project_item_status
        .write()
        .await
        .clone_from(&"In Progress".to_string());
    mock.upsert_issue("I_1", "acme/repo", "Issue A", true).await;
    mock.upsert_issue("I_2", "acme/repo", "Issue B", true).await;
    mock.upsert_issue("I_3", "acme/repo", "Issue C", true).await;
    mock.upsert_issue("I_4", "acme/new-repo", "Issue New", true)
        .await;
    mock.upsert_issue("I_NULL", "acme/repo", "Null", false)
        .await;

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{graphql_port}"))
        .await
        .unwrap();
    let app = Router::new()
        .route("/graphql", post(mock_graphql_handler))
        .with_state(mock.clone());
    let graphql_server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let wal = Arc::new(RedbWalProvider::new(tmp.path().join("wal")));
    let durable_state_store: Arc<dyn StateStoreProvider> = Arc::new(
        RedbStateStoreProvider::new(tmp.path().join("state.redb"))
            .expect("create durable redb state store"),
    );
    let state_store = Arc::new(RecordingStateStore::new(durable_state_store));

    let mut source = GitHubSourceBuilder::new(&source_id)
        .with_config(GitHubSourceConfig {
            token: "test-token".to_string(),
            repositories: Vec::new(),
            projects: vec![ProjectSpec {
                owner: "acme".to_string(),
                number: 1,
            }],
            webhook: WebhookConfig {
                host: webhook_host.clone(),
                port: webhook_port,
                path: webhook_path.clone(),
                secret: webhook_secret.clone(),
                body_limit_bytes: 1024 * 1024,
            },
            durability: DurabilityConfig {
                enabled: true,
                max_events,
                capacity_policy: CapacityPolicy::RejectIncoming,
            },
            graphql_url: format!("http://127.0.0.1:{graphql_port}/graphql"),
        })
        .build()
        .unwrap();

    let (update_tx, _update_rx): (ComponentUpdateSender, _) = tokio::sync::mpsc::channel(256);
    let mut context = SourceRuntimeContext::new(
        "github-int-test",
        source_id.clone(),
        Some(state_store.clone()),
        update_tx,
        None,
    );
    context.wal_provider = Some(wal.clone());
    source.initialize(context).await;
    source.start().await.unwrap();

    Harness {
        _tmp: tmp,
        source,
        wal,
        graphql_server,
        source_id,
        inbox_id,
        webhook_host,
        webhook_port,
        webhook_path,
        webhook_secret,
        mock,
        state_store,
    }
}

fn subscription_settings(
    source_id: &str,
    query_id: &str,
    resume_from: Option<u64>,
    request_position_handle: bool,
) -> SourceSubscriptionSettings {
    SourceSubscriptionSettings {
        source_id: source_id.to_string(),
        enable_bootstrap: false,
        query_id: query_id.to_string(),
        nodes: HashSet::new(),
        relations: HashSet::new(),
        resume_from: resume_from.map(|seq| bytes::Bytes::from(seq.to_be_bytes().to_vec())),
        request_position_handle,
    }
}

fn issue_change_id(change: &SourceChange) -> Option<String> {
    match change {
        SourceChange::Insert { element } | SourceChange::Update { element } => match element {
            Element::Node { metadata, .. } => metadata
                .labels
                .iter()
                .any(|l| l.as_ref() == "GitHubIssue")
                .then(|| metadata.reference.element_id.as_ref().to_string()),
            Element::Relation { .. } => None,
        },
        SourceChange::Delete { metadata } => metadata
            .labels
            .iter()
            .any(|l| l.as_ref() == "GitHubIssue")
            .then(|| metadata.reference.element_id.as_ref().to_string()),
        SourceChange::Future { .. } => None,
    }
}

fn admission_delivery_id(change: &SourceChange) -> Option<String> {
    let SourceChange::Insert { element } = change else {
        return None;
    };
    let Element::Node { properties, .. } = element else {
        return None;
    };
    properties.get("deliveryId").and_then(|value| match value {
        drasi_core::models::ElementValue::String(s) => Some(s.to_string()),
        _ => None,
    })
}

async fn recv_event(
    receiver: &mut Box<
        dyn drasi_lib::channels::ChangeReceiver<drasi_lib::channels::SourceEventWrapper>,
    >,
    timeout: Duration,
) -> drasi_lib::channels::SourceEventWrapper {
    let arc = tokio::time::timeout(timeout, receiver.recv())
        .await
        .expect("timed out waiting for source event")
        .expect("source receiver closed");
    (*arc).clone()
}

#[tokio::test]
#[ignore]
async fn deleted_webhook_skips_graphql_and_emits_one_delete() {
    let mut harness = build_harness(16).await;
    let client = Client::new();
    let mut receiver = harness
        .source
        .subscribe(subscription_settings(
            &harness.source_id,
            "delete-fast-path",
            None,
            false,
        ))
        .await
        .unwrap()
        .receiver;

    let malformed = send_webhook(
        &client,
        &harness,
        "delivery-delete-malformed",
        "issues",
        json!({"action":"deleted","issue":{},"repository":{"full_name":"acme/repo"}}),
    )
    .await;
    assert_eq!(malformed.status(), 400);
    assert_eq!(harness.wal.event_count(&harness.inbox_id).await.unwrap(), 0);

    let graphql_calls_before = harness.mock.graphql_calls.load(Ordering::SeqCst);
    let output_head_before = harness.wal.head_sequence(&harness.source_id).await.unwrap();
    let response = send_webhook(
        &client,
        &harness,
        "delivery-delete",
        "issues",
        json!({"action":"deleted","issue":{"node_id":"I_DELETE"},"repository":{"full_name":"acme/repo"}}),
    )
    .await;
    assert_eq!(response.status(), 200);

    let wrapper = recv_event(&mut receiver, Duration::from_secs(10)).await;
    let drasi_lib::channels::SourceEvent::Change(SourceChange::Delete { metadata }) =
        &wrapper.event
    else {
        panic!("expected one object delete");
    };
    assert_eq!(metadata.reference.source_id.as_ref(), harness.source_id);
    assert_eq!(metadata.reference.element_id.as_ref(), "I_DELETE");
    assert_eq!(
        metadata
            .labels
            .iter()
            .map(|label| label.as_ref())
            .collect::<Vec<_>>(),
        vec!["GitHubIssue"]
    );

    let output = harness
        .wal
        .read_from(&harness.source_id, output_head_before.saturating_add(1))
        .await
        .unwrap();
    assert_eq!(output.len(), 1, "deleted action must append one output");
    assert!(matches!(output[0].1, SourceChange::Delete { .. }));
    assert_eq!(
        harness.mock.graphql_calls.load(Ordering::SeqCst),
        graphql_calls_before,
        "deleted action must not call GraphQL"
    );

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while harness.wal.event_count(&harness.inbox_id).await.unwrap() != 0
        && tokio::time::Instant::now() < deadline
    {
        sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(harness.wal.event_count(&harness.inbox_id).await.unwrap(), 0);

    harness.source.stop().await.unwrap();
    harness.graphql_server.abort();
}

#[tokio::test]
#[ignore]
async fn update_maps_only_fetched_current_state_without_snapshot_lookup() {
    let mut harness = build_harness(16).await;
    let client = Client::new();
    let mut receiver = harness
        .source
        .subscribe(subscription_settings(
            &harness.source_id,
            "current-update",
            None,
            false,
        ))
        .await
        .unwrap()
        .receiver;

    harness
        .mock
        .queue_issue_title("I_2", "Authoritative current title")
        .await;
    harness.state_store.clear_get_keys().await;
    let response = send_webhook(
        &client,
        &harness,
        "delivery-current-update",
        "issues",
        json!({"action":"edited","issue":{"node_id":"I_2"},"repository":{"full_name":"acme/repo"}}),
    )
    .await;
    assert_eq!(response.status(), 200);

    let mut saw_current_issue = false;
    for _ in 0..8 {
        let wrapper = recv_event(&mut receiver, Duration::from_secs(10)).await;
        let drasi_lib::channels::SourceEvent::Change(SourceChange::Update { element }) =
            &wrapper.event
        else {
            continue;
        };
        let Element::Node {
            metadata,
            properties,
        } = element
        else {
            continue;
        };
        if metadata.reference.element_id.as_ref() != "I_2" {
            continue;
        }
        assert_eq!(
            properties.get("title"),
            Some(&drasi_core::models::ElementValue::String(Arc::from(
                "Authoritative current title"
            )))
        );
        saw_current_issue = true;
        break;
    }
    assert!(saw_current_issue, "expected current-state issue update");
    assert!(
        harness
            .state_store
            .get_keys()
            .await
            .iter()
            .all(|key| !key.starts_with("root-snapshot:")),
        "update hydration must not load prior object snapshots"
    );
    assert!(
        harness
            .state_store
            .list_keys(&harness.source_id)
            .await
            .unwrap()
            .iter()
            .all(|key| !key.starts_with("root-snapshot:")),
        "update hydration must not persist object snapshots"
    );

    harness.source.stop().await.unwrap();
    harness.graphql_server.abort();
}

#[tokio::test]
#[ignore]
async fn github_source_minimal_v1_fifo_durability_replay_scope_and_backpressure() {
    let mut harness = build_harness(16).await;
    let client = Client::new();

    let mut q1 = harness
        .source
        .subscribe(subscription_settings(&harness.source_id, "q1", None, false))
        .await
        .unwrap()
        .receiver;

    // Hold the worker on transient GraphQL failure.
    harness.mock.block_issue_fetch.store(true, Ordering::SeqCst);

    let d1 = send_webhook(
        &client,
        &harness,
        "delivery-1",
        "issues",
        json!({"action":"opened","issue":{"node_id":"I_1"},"repository":{"full_name":"acme/repo"}}),
    )
    .await;
    assert_eq!(d1.status(), 200);
    assert_eq!(harness.wal.event_count(&harness.inbox_id).await.unwrap(), 1);

    // Retained duplicate does not append.
    let d1_dup = send_webhook(
        &client,
        &harness,
        "delivery-1",
        "issues",
        json!({"action":"opened","issue":{"node_id":"I_1"},"repository":{"full_name":"acme/repo"}}),
    )
    .await;
    assert_eq!(d1_dup.status(), 200);
    assert_eq!(harness.wal.event_count(&harness.inbox_id).await.unwrap(), 1);

    let (d2, d3) = tokio::join!(
        send_webhook(
            &client,
            &harness,
            "delivery-2",
            "issues",
            json!({"action":"opened","issue":{"node_id":"I_2"},"repository":{"full_name":"acme/repo"}})
        ),
        send_webhook(
            &client,
            &harness,
            "delivery-3",
            "issues",
            json!({"action":"opened","issue":{"node_id":"I_3"},"repository":{"full_name":"acme/repo"}})
        )
    );
    assert_eq!(d2.status(), 200);
    assert_eq!(d3.status(), 200);
    assert_eq!(harness.wal.event_count(&harness.inbox_id).await.unwrap(), 3);

    // Backpressure: inbox WAL full while worker is stalled.
    let mut saw_503 = false;
    for i in 0..40 {
        let response = send_webhook(
            &client,
            &harness,
            &format!("delivery-full-{i}"),
            "ping",
            json!({"action":"ping"}),
        )
        .await;
        if response.status() == 503 {
            saw_503 = true;
            break;
        }
    }
    assert!(saw_503, "expected at least one 503 when inbox WAL is full");

    // Unblock and assert strict FIFO hydration order.
    harness
        .mock
        .block_issue_fetch
        .store(false, Ordering::SeqCst);

    let mut inbox_order = Vec::new();
    let inbox_entries = harness
        .wal
        .read_from(
            &harness.inbox_id,
            harness
                .wal
                .oldest_sequence(&harness.inbox_id)
                .await
                .unwrap()
                .unwrap(),
        )
        .await
        .unwrap();
    for (_, change) in &inbox_entries {
        if let Some(delivery_id) = admission_delivery_id(change) {
            inbox_order.push(delivery_id);
        }
    }

    let delivery_to_issue = HashMap::from([
        ("delivery-1".to_string(), "I_1".to_string()),
        ("delivery-2".to_string(), "I_2".to_string()),
        ("delivery-3".to_string(), "I_3".to_string()),
    ]);
    let expected_issue_order = inbox_order
        .iter()
        .filter_map(|d| delivery_to_issue.get(d).cloned())
        .collect::<Vec<_>>();

    let mut received_issue_order = Vec::new();
    while received_issue_order.len() < expected_issue_order.len() {
        let wrapper = recv_event(&mut q1, Duration::from_secs(25)).await;
        let drasi_lib::channels::SourceEvent::Change(change) = &wrapper.event else {
            continue;
        };
        let Some(issue_id) = issue_change_id(change) else {
            continue;
        };
        if !["I_1", "I_2", "I_3"].contains(&issue_id.as_str()) {
            continue;
        }
        assert!(
            matches!(change, SourceChange::Insert { .. }),
            "opened issue must emit INSERT current state"
        );
        received_issue_order.push(issue_id.clone());

        let pos = wrapper
            .source_position
            .clone()
            .expect("live issue event must carry source_position");
        let seq = u64::from_be_bytes(pos.as_ref().try_into().unwrap());
        let persisted = harness
            .wal
            .read_from(&harness.source_id, seq)
            .await
            .unwrap();
        assert!(
            persisted
                .iter()
                .any(|(s, c)| *s == seq && issue_change_id(c) == Some(issue_id.clone())),
            "output WAL must contain event before/at dispatch seq={seq}"
        );
    }
    assert_eq!(received_issue_order, expected_issue_order);

    // After retained dedupe expires, current-state delivery is forwarded again.
    let duplicate_after_prune = send_webhook(
        &client,
        &harness,
        "delivery-1",
        "issues",
        json!({"action":"opened","issue":{"node_id":"I_1"},"repository":{"full_name":"acme/repo"}}),
    )
    .await;
    assert_eq!(duplicate_after_prune.status(), 200);
    let mut saw_repeated_current_state = false;
    for _ in 0..8 {
        let wrapper = recv_event(&mut q1, Duration::from_secs(10)).await;
        if let drasi_lib::channels::SourceEvent::Change(change) = &wrapper.event {
            if issue_change_id(change).as_deref() == Some("I_1") {
                saw_repeated_current_state = true;
                break;
            }
        }
    }
    assert!(
        saw_repeated_current_state,
        "current state must be forwarded without source-side no-op suppression"
    );

    // Project item hydration can grow scope to acme/new-repo.
    let project_item = send_webhook(
        &client,
        &harness,
        "delivery-project-grow",
        "projects_v2_item",
        json!({"action":"edited","projects_v2_item":{"node_id":"PVTI_1","project_node_id":"PVT_1"}}),
    )
    .await;
    assert_eq!(project_item.status(), 200);
    sleep(Duration::from_millis(300)).await;

    let new_repo_issue = send_webhook(
        &client,
        &harness,
        "delivery-new-repo",
        "issues",
        json!({"action":"opened","issue":{"node_id":"I_4"},"repository":{"full_name":"acme/new-repo"}}),
    )
    .await;
    assert_eq!(new_repo_issue.status(), 200);

    let mut saw_new_repo_issue = false;
    for _ in 0..24 {
        let wrapper = recv_event(&mut q1, Duration::from_secs(10)).await;
        let drasi_lib::channels::SourceEvent::Change(change) = &wrapper.event else {
            continue;
        };
        if issue_change_id(change).as_deref() == Some("I_4") {
            saw_new_repo_issue = true;
            break;
        }
    }
    assert!(
        saw_new_repo_issue,
        "project-item-driven scope growth should admit new repository events"
    );

    // Archived is not a literal delete: it still hydrates authoritatively and emits no inferred delete.
    harness.mock.set_project_item_exists(false);
    let graphql_before_archive = harness.mock.graphql_calls.load(Ordering::SeqCst);
    let output_before_archive = harness.wal.head_sequence(&harness.source_id).await.unwrap();
    let project_item_archived = send_webhook(
        &client,
        &harness,
        "delivery-project-item-archived",
        "projects_v2_item",
        json!({"action":"archived","projects_v2_item":{"node_id":"PVTI_1","project_node_id":"PVT_1"}}),
    )
    .await;
    assert_eq!(project_item_archived.status(), 200);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while harness.wal.event_count(&harness.inbox_id).await.unwrap() != 0
        && tokio::time::Instant::now() < deadline
    {
        sleep(Duration::from_millis(25)).await;
    }
    assert!(
        harness.mock.graphql_calls.load(Ordering::SeqCst) > graphql_before_archive,
        "archived action must retain authoritative hydration"
    );
    assert_eq!(
        harness.wal.head_sequence(&harness.source_id).await.unwrap(),
        output_before_archive,
        "archived null response must not infer a snapshot-derived delete"
    );

    // Non-delete null retries should eventually advance FIFO.
    let null_event = send_webhook(
        &client,
        &harness,
        "delivery-null",
        "issues",
        json!({"action":"edited","issue":{"node_id":"I_NULL"},"repository":{"full_name":"acme/repo"}}),
    )
    .await;
    assert_eq!(null_event.status(), 200);
    harness
        .mock
        .queue_issue_title("I_2", "Issue B advanced")
        .await;
    let after_null = send_webhook(
        &client,
        &harness,
        "delivery-after-null",
        "issues",
        json!({"action":"edited","issue":{"node_id":"I_2"},"repository":{"full_name":"acme/repo"}}),
    )
    .await;
    assert_eq!(after_null.status(), 200);

    let mut advanced = false;
    for _ in 0..40 {
        let Ok(Ok(wrapper)) = tokio::time::timeout(Duration::from_secs(2), q1.recv()).await else {
            continue;
        };
        let drasi_lib::channels::SourceEvent::Change(change) = &wrapper.event else {
            continue;
        };
        if issue_change_id(change).as_deref() == Some("I_2") {
            advanced = true;
            break;
        }
    }
    assert!(advanced, "FIFO should advance after bounded null retries");

    // Fresh subscriber replays from oldest retained output WAL.
    let oldest_before = harness
        .wal
        .oldest_sequence(&harness.source_id)
        .await
        .unwrap();
    assert!(oldest_before.is_some());
    let mut q_fresh = harness
        .source
        .subscribe(subscription_settings(
            &harness.source_id,
            "q-fresh",
            None,
            false,
        ))
        .await
        .unwrap()
        .receiver;
    let replay = recv_event(&mut q_fresh, Duration::from_secs(2)).await;
    assert_eq!(
        replay
            .source_position
            .as_ref()
            .map(|b| u64::from_be_bytes(b.as_ref().try_into().unwrap())),
        oldest_before
    );

    // Resume subscriber starts from requested position.
    let head = harness.wal.head_sequence(&harness.source_id).await.unwrap();
    let mut q_resume = harness
        .source
        .subscribe(subscription_settings(
            &harness.source_id,
            "q-resume",
            Some(head),
            false,
        ))
        .await
        .unwrap()
        .receiver;

    harness
        .mock
        .queue_issue_title("I_2", "Issue B resume")
        .await;
    let resume_event = send_webhook(
        &client,
        &harness,
        "delivery-resume",
        "issues",
        json!({"action":"edited","issue":{"node_id":"I_2"},"repository":{"full_name":"acme/repo"}}),
    )
    .await;
    assert_eq!(resume_event.status(), 200);
    let resumed = recv_event(&mut q_resume, Duration::from_secs(10)).await;
    let resumed_seq = resumed
        .source_position
        .as_ref()
        .map(|b| u64::from_be_bytes(b.as_ref().try_into().unwrap()))
        .unwrap();
    assert!(resumed_seq > head);

    harness.source.stop().await.unwrap();
    harness.graphql_server.abort();
}

#[tokio::test]
#[ignore]
async fn github_source_prunes_output_only_from_confirmed_positions_and_retains_without_subscribers()
{
    let mut harness = build_harness(16).await;
    let client = Client::new();

    let first = send_webhook(
        &client,
        &harness,
        "retain-1",
        "issues",
        json!({"action":"opened","issue":{"node_id":"I_1"},"repository":{"full_name":"acme/repo"}}),
    )
    .await;
    assert_eq!(first.status(), 200);

    harness.mock.queue_issue_title("I_2", "Issue B2").await;
    let second = send_webhook(
        &client,
        &harness,
        "retain-2",
        "issues",
        json!({"action":"edited","issue":{"node_id":"I_2"},"repository":{"full_name":"acme/repo"}}),
    )
    .await;
    assert_eq!(second.status(), 200);

    // Wait for output WAL to populate.
    for _ in 0..40 {
        if harness.wal.event_count(&harness.source_id).await.unwrap() >= 2 {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    let oldest_before = harness
        .wal
        .oldest_sequence(&harness.source_id)
        .await
        .unwrap()
        .unwrap();

    // No subscribers => retention stays stable.
    sleep(Duration::from_secs(2)).await;
    let oldest_after = harness
        .wal
        .oldest_sequence(&harness.source_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(oldest_after, oldest_before);

    // Add subscriber with position handle and confirm first retained sequence.
    let response = harness
        .source
        .subscribe(subscription_settings(
            &harness.source_id,
            "q-prune",
            None,
            true,
        ))
        .await
        .unwrap();
    let handle = response.position_handle.expect("position handle");
    let mut receiver = response.receiver;
    let first_replay = recv_event(&mut receiver, Duration::from_secs(2)).await;
    let first_seq = first_replay
        .source_position
        .as_ref()
        .map(|b| u64::from_be_bytes(b.as_ref().try_into().unwrap()))
        .unwrap();
    handle.store(first_seq, Ordering::Release);

    for _ in 0..30 {
        if let Some(oldest) = harness
            .wal
            .oldest_sequence(&harness.source_id)
            .await
            .unwrap()
        {
            if oldest > first_seq {
                break;
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    let pruned_oldest = harness
        .wal
        .oldest_sequence(&harness.source_id)
        .await
        .unwrap()
        .unwrap();
    assert!(pruned_oldest > first_seq);

    // Resume from pruned position must fail with PositionUnavailable.
    let result = harness
        .source
        .subscribe(subscription_settings(
            &harness.source_id,
            "q-gap",
            Some(first_seq.saturating_sub(1)),
            false,
        ))
        .await;
    assert!(result.is_err());
    let err = match result {
        Ok(_) => panic!("expected subscribe gap error"),
        Err(err) => err,
    };
    assert!(
        err.downcast_ref::<drasi_lib::sources::SourceError>()
            .is_some(),
        "expected SourceError::PositionUnavailable, got {err:#}"
    );

    harness.source.stop().await.unwrap();
    harness.graphql_server.abort();
}
