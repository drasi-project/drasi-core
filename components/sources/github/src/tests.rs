use crate::config::{GitHubSourceConfig, ProjectSpec, WebhookConfig};
use crate::descriptor::{GitHubSourceConfigDto, GitHubSourceDescriptor};
use crate::graphql::{
    Connection, FetchedRoot, GitHubGraphQLClient, IssueCommentData, IssueData, LabelRef, NodeIdRef,
    OwnerRef, PageInfo, ProjectIdentityRef, ProjectItemContent, ProjectItemData,
    ProjectItemFieldValue, PullRequestData, PullRequestReviewData, PullRequestReviewRef,
    ReconcileSnapshot, RepositoryData, RepositoryRef, UserRef,
};
use crate::hydrator::{
    load_root_snapshot, process_admission, save_root_snapshot, snapshot_key_for_locator,
    HydratorParams,
};
use crate::mapping::{map_reconcile_snapshot, map_root_diff, node_labels, relation_labels};
use crate::rate_limit::{classify_retry, exp_backoff};
use crate::reconciler::{run_reconciler_loop, ReconcilerParams};
use crate::source::GitHubSourceBuilder;
use crate::types::{HydratorHealth, RootSnapshot, SnapshotElement, WebhookLocator};
use crate::webhook::{
    compact_dedupe_markers, dedupe_key, encode_admission_change, find_delivery_in_wal,
    parse_locator, persist_dedupe_marker, verify_signature,
};
use axum::extract::State;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};
use drasi_core::models::SourceChange;
use drasi_lib::bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest};
use drasi_lib::channels::{ComponentStatus, DispatchMode, SourceEvent};
use drasi_lib::config::SourceSubscriptionSettings;
use drasi_lib::context::SourceRuntimeContext;
use drasi_lib::state_store::{
    MemoryStateStoreProvider, StateStoreError, StateStoreProvider, StateStoreResult,
};
use drasi_lib::wal::{CapacityPolicy, WalError, WalProvider, WriteAheadLogConfig};
use drasi_lib::{DrasiLib, DurabilityConfig, Source};
use drasi_plugin_sdk::resolver::{register_secret_resolver, ResolverError, ValueResolver};
use drasi_plugin_sdk::{ConfigValue, SourcePluginDescriptor};
use drasi_state_store_redb::RedbStateStoreProvider;
use drasi_wal_redb::RedbWalProvider;
use hmac::Mac;
use reqwest::header::{HeaderMap as ReqwestHeaderMap, HeaderValue, AUTHORIZATION};
use serde_json::json;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, RwLock};

fn empty_page_info() -> PageInfo {
    PageInfo {
        has_next_page: false,
        end_cursor: None,
    }
}

fn single_connection<T>(nodes: Vec<T>) -> Connection<T> {
    Connection {
        nodes,
        page_info: empty_page_info(),
    }
}

fn sample_issue(title: &str) -> IssueData {
    IssueData {
        id: "I_1".to_string(),
        number: 42,
        title: title.to_string(),
        body: Some("body".to_string()),
        state: "OPEN".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        closed_at: None,
        url: "https://github.com/acme/repo/issues/42".to_string(),
        author: Some(OwnerRef {
            login: "octocat".to_string(),
        }),
        repository: RepositoryRef {
            id: "R_1".to_string(),
            name_with_owner: "acme/repo".to_string(),
        },
        assignees: single_connection(vec![UserRef {
            id: "U_1".to_string(),
            login: "assignee".to_string(),
        }]),
        labels: single_connection(vec![LabelRef {
            id: "L_1".to_string(),
            name: "bug".to_string(),
        }]),
        comments: single_connection(vec![IssueCommentData {
            id: "IC_1".to_string(),
            body: Some("comment".to_string()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            url: "https://example/comment".to_string(),
            is_minimized: false,
            author: None,
            issue: Some(NodeIdRef {
                id: "I_1".to_string(),
            }),
            pull_request: None,
            repository: RepositoryRef {
                id: "R_1".to_string(),
                name_with_owner: "acme/repo".to_string(),
            },
        }]),
    }
}

fn sample_pull_request(body: Option<String>) -> PullRequestData {
    PullRequestData {
        id: "PR_1".to_string(),
        number: 7,
        title: "Pull request".to_string(),
        body,
        state: "OPEN".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        closed_at: None,
        merged_at: None,
        url: "https://github.com/acme/repo/pull/7".to_string(),
        is_draft: false,
        head_ref_name: Some("feature".to_string()),
        base_ref_name: Some("main".to_string()),
        author: Some(OwnerRef {
            login: "octocat".to_string(),
        }),
        repository: RepositoryRef {
            id: "R_1".to_string(),
            name_with_owner: "acme/repo".to_string(),
        },
        assignees: single_connection(Vec::new()),
        labels: single_connection(Vec::new()),
        comments: single_connection(Vec::new()),
        reviews: single_connection(Vec::new()),
    }
}

fn sample_project_item() -> ProjectItemData {
    ProjectItemData {
        id: "PVTI_1".to_string(),
        item_type: "ISSUE".to_string(),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-01T00:00:00Z".to_string(),
        project: ProjectIdentityRef {
            id: "PVT_1".to_string(),
            number: 1,
            owner: OwnerRef {
                login: "acme".to_string(),
            },
        },
        content: Some(ProjectItemContent::Issue {
            id: "I_1".to_string(),
            number: 42,
            title: "Issue".to_string(),
            state: "OPEN".to_string(),
            repository: RepositoryRef {
                id: "R_1".to_string(),
                name_with_owner: "acme/repo".to_string(),
            },
        }),
        field_values: single_connection(vec![
            ProjectItemFieldValue::ProjectV2ItemFieldSingleSelectValue {
                name: Some("In Progress".to_string()),
                field: Some(crate::graphql::ProjectFieldRef {
                    id: "status".to_string(),
                    name: "Status".to_string(),
                }),
                option_id: Some("opt1".to_string()),
            },
        ]),
    }
}

fn valid_config_with_port(port: u16) -> GitHubSourceConfig {
    GitHubSourceConfig {
        token: "test-token".to_string(),
        repositories: vec!["acme/repo".to_string()],
        projects: vec![ProjectSpec {
            owner: "acme".to_string(),
            number: 1,
        }],
        webhook: WebhookConfig {
            host: "127.0.0.1".to_string(),
            port,
            path: "/webhook".to_string(),
            secret: "secret".to_string(),
            body_limit_bytes: 1024 * 1024,
        },
        reconcile_interval_secs: 60,
        durability: DurabilityConfig {
            enabled: true,
            max_events: 16,
            capacity_policy: CapacityPolicy::RejectIncoming,
        },
        graphql_url: "http://127.0.0.1:9/graphql".to_string(),
        skip_initial_bootstrap: true,
    }
}

async fn find_available_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    listener.local_addr().expect("local addr").port()
}

async fn bootstrap_snapshot_handler(
    Json(payload): Json<serde_json::Value>,
) -> Json<serde_json::Value> {
    let query = payload["query"].as_str().unwrap_or_default();
    if query.contains("issues(first: 100") {
        return Json(json!({
            "data": {
                "repository": {
                    "issues": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": [{
                            "id": "I_1",
                            "number": 42,
                            "title": "Bootstrap issue",
                            "body": "body",
                            "state": "OPEN",
                            "createdAt": "2026-01-01T00:00:00Z",
                            "updatedAt": "2026-01-01T00:00:00Z",
                            "closedAt": null,
                            "url": "https://github.com/acme/repo/issues/42",
                            "author": { "login": "octocat" },
                            "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                            "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                            "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                            "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                        }]
                    }
                }
            }
        }));
    }
    if query.contains("pullRequests(first: 100") {
        return Json(json!({
            "data": {
                "repository": {
                    "pullRequests": {
                        "pageInfo": { "hasNextPage": false, "endCursor": null },
                        "nodes": []
                    }
                }
            }
        }));
    }
    Json(json!({
        "data": {
            "repository": {
                "id": "R_1",
                "name": "repo",
                "nameWithOwner": "acme/repo",
                "owner": { "login": "acme" },
                "description": null,
                "url": "https://github.com/acme/repo",
                "isArchived": false,
                "isPrivate": false,
                "createdAt": "2026-01-01T00:00:00Z",
                "updatedAt": "2026-01-01T00:00:00Z",
                "defaultBranchRef": { "name": "main" }
            }
        }
    }))
}

#[tokio::test]
async fn query_bootstrap_seeds_durable_delete_state_before_initial_reconcile() {
    #[derive(Clone, Default)]
    struct ApiState {
        deleted: Arc<AtomicBool>,
    }

    async fn handler(
        State(state): State<ApiState>,
        Json(payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let query = payload["query"].as_str().unwrap_or_default();
        if query.contains("query($id: ID!)") {
            assert!(state.deleted.load(Ordering::SeqCst));
            return Json(json!({
                "data": { "node": null },
                "errors": [{
                    "type": "NOT_FOUND",
                    "path": ["node"],
                    "message": "Could not resolve to a node with the global id"
                }]
            }));
        }
        if query.contains("issues(first: 100") {
            return Json(json!({
                "data": {
                    "repository": {
                        "issues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "id": "I_1",
                                "number": 42,
                                "title": "Bootstrapped issue",
                                "body": "body",
                                "state": "OPEN",
                                "createdAt": "2026-01-01T00:00:00Z",
                                "updatedAt": "2026-01-01T00:00:00Z",
                                "closedAt": null,
                                "url": "https://github.com/acme/repo/issues/42",
                                "author": { "login": "octocat" },
                                "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                                "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                                "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                                "comments": {
                                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                                    "nodes": [{
                                        "id": "IC_1",
                                        "body": "comment",
                                        "createdAt": "2026-01-01T00:00:00Z",
                                        "updatedAt": "2026-01-01T00:00:00Z",
                                        "url": "https://github.com/acme/repo/issues/42#issuecomment-1",
                                        "isMinimized": false,
                                        "author": null,
                                        "issue": { "id": "I_1" },
                                        "pullRequest": null,
                                        "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                                    }]
                                }
                            }]
                        }
                    }
                }
            }));
        }
        if query.contains("pullRequests(first: 100") {
            return Json(json!({
                "data": {
                    "repository": {
                        "pullRequests": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }
                }
            }));
        }
        Json(json!({
            "data": {
                "repository": {
                    "id": "R_1",
                    "name": "repo",
                    "nameWithOwner": "acme/repo",
                    "owner": { "login": "acme" },
                    "description": null,
                    "url": "https://github.com/acme/repo",
                    "isArchived": false,
                    "isPrivate": false,
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "defaultBranchRef": { "name": "main" }
                }
            }
        }))
    }

    let api_state = ApiState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("mock server addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(api_state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let state_path = temp.path().join("bootstrap-state.redb");
    let state_store: Arc<dyn StateStoreProvider> =
        Arc::new(RedbStateStoreProvider::new(&state_path).expect("bootstrap state store"));
    let effective_repos = Arc::new(RwLock::new(HashSet::new()));
    let processing_gate = Arc::new(tokio::sync::Mutex::new(()));
    let source_base = Arc::new(OnceLock::new());
    assert!(
        source_base
            .set(test_source_base_with_state_store(
                "github-bootstrap",
                state_store.clone()
            ))
            .is_ok(),
        "set bootstrap dispatcher"
    );
    let mut config = valid_config_with_port(0);
    config.graphql_url = format!("http://{addr}/graphql");
    config.projects.clear();
    config.skip_initial_bootstrap = true;
    let provider = crate::bootstrap::GitHubBootstrapProvider::new(
        config,
        effective_repos,
        processing_gate,
        source_base,
    );
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(100);
    let result = provider
        .bootstrap(
            BootstrapRequest {
                query_id: "query-bootstrap".to_string(),
                node_labels: vec![],
                relation_labels: vec![],
                request_id: "request-1".to_string(),
            },
            &BootstrapContext::new_minimal(
                "bootstrap-test".to_string(),
                "github-bootstrap".to_string(),
            ),
            event_tx,
            None,
        )
        .await
        .expect("query-triggered bootstrap");
    assert!(result.event_count > 0);
    let first_event = event_rx.recv().await.expect("bootstrap event");
    assert_eq!(first_event.source_id, "github-bootstrap");
    assert!(
        crate::hydrator::load_reconcile_index(state_store.as_ref(), "github-bootstrap")
            .await
            .expect("state must be committed before event is observable")
            .contains_key("I_1")
    );

    drop(provider);
    drop(state_store);
    let restarted_state: Arc<dyn StateStoreProvider> =
        Arc::new(RedbStateStoreProvider::new(&state_path).expect("reopen durable state"));
    let restarted_index =
        crate::hydrator::load_reconcile_index(restarted_state.as_ref(), "github-bootstrap")
            .await
            .expect("load bootstrapped state after restart");
    assert!(restarted_index.contains_key("IC_1"));
    assert!(restarted_index.contains_key("COMMENT_ON:IC_1:I_1"));

    api_state.deleted.store(true, Ordering::SeqCst);
    let wal = Arc::new(RedbWalProvider::new(temp.path().join("bootstrap-wal")));
    wal.register(
        "github-bootstrap",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register WAL");
    let base = test_source_base("github-bootstrap");
    let mut receiver = base
        .create_streaming_receiver()
        .await
        .expect("create event receiver");
    let params = HydratorParams {
        source_id: "github-bootstrap".to_string(),
        base,
        wal: wal.clone(),
        state_store: restarted_state.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "deleted".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission =
        encode_admission_change("github-bootstrap", "delete-before-reconcile", &locator)
            .expect("encode deletion");
    let sequence = wal
        .append("github-bootstrap", &admission)
        .await
        .expect("append deletion");
    process_admission(&params, sequence, &admission)
        .await
        .expect("delete from bootstrapped durable state");

    let mut deleted = HashSet::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(20), receiver.recv()).await
    {
        if let drasi_lib::channels::SourceEvent::Change(SourceChange::Delete { metadata }) =
            &event.event
        {
            deleted.insert(metadata.reference.element_id.as_ref().to_string());
        }
    }
    for expected in [
        "I_1",
        "IC_1",
        "COMMENT_ON:IC_1:I_1",
        "IN_REPOSITORY:I_1:R_1",
    ] {
        assert!(deleted.contains(expected), "missing delete for {expected}");
    }
    server.abort();
}

#[tokio::test]
async fn later_query_bootstrap_reconciles_live_subscribers_without_losing_full_snapshot() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let query = payload["query"].as_str().unwrap_or_default();
        if query.contains("issues(first: 100") {
            return Json(json!({
                "data": {
                    "repository": {
                        "issues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [
                                {
                                    "id": "I_1",
                                    "number": 42,
                                    "title": "changed",
                                    "body": "body",
                                    "state": "OPEN",
                                    "createdAt": "2026-01-01T00:00:00Z",
                                    "updatedAt": "2026-01-02T00:00:00Z",
                                    "closedAt": null,
                                    "url": "https://github.com/acme/repo/issues/42",
                                    "author": { "login": "octocat" },
                                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                                    "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                                },
                                {
                                    "id": "I_2",
                                    "number": 43,
                                    "title": "added",
                                    "body": null,
                                    "state": "OPEN",
                                    "createdAt": "2026-01-02T00:00:00Z",
                                    "updatedAt": "2026-01-02T00:00:00Z",
                                    "closedAt": null,
                                    "url": "https://github.com/acme/repo/issues/43",
                                    "author": null,
                                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                                    "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                                }
                            ]
                        }
                    }
                }
            }));
        }
        if query.contains("pullRequests(first: 100") {
            return Json(json!({
                "data": {
                    "repository": {
                        "pullRequests": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }
                }
            }));
        }
        Json(json!({
            "data": {
                "repository": {
                    "id": "R_1",
                    "name": "repo",
                    "nameWithOwner": "acme/repo",
                    "owner": { "login": "acme" },
                    "description": null,
                    "url": "https://github.com/acme/repo",
                    "isArchived": false,
                    "isPrivate": false,
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "defaultBranchRef": { "name": "main" }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("mock server addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, Router::new().route("/graphql", post(handler))).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let state_store: Arc<dyn StateStoreProvider> = Arc::new(
        RedbStateStoreProvider::new(temp.path().join("later-bootstrap.redb")).expect("state store"),
    );
    let mut previous_snapshot = ReconcileSnapshot::default();
    previous_snapshot
        .issues
        .insert("I_1".to_string(), sample_issue("old"));
    previous_snapshot.pull_requests.insert(
        "PR_1".to_string(),
        sample_pull_request(Some("deleted".to_string())),
    );
    let (_, previous_pr_root) = map_root_diff(
        "github-later-bootstrap",
        &FetchedRoot::PullRequest(previous_snapshot.pull_requests["PR_1"].clone()),
        None,
        1,
    )
    .expect("map prior pull request root");
    save_root_snapshot(
        state_store.as_ref(),
        "github-later-bootstrap",
        "root-snapshot:PR_1",
        &previous_pr_root,
    )
    .await
    .expect("seed prior pull request root");
    let (_, previous_index) = map_reconcile_snapshot(
        "github-later-bootstrap",
        &previous_snapshot,
        &HashMap::new(),
        1,
    );
    crate::hydrator::save_reconcile_index(
        state_store.as_ref(),
        "github-later-bootstrap",
        &previous_index,
    )
    .await
    .expect("seed reconcile index");

    let base = test_source_base_with_state_store("github-later-bootstrap", state_store.clone());
    let mut live_rx = base
        .create_streaming_receiver()
        .await
        .expect("existing live receiver");
    let mut bootstrapping_live_rx = base
        .subscribe_with_bootstrap(
            &SourceSubscriptionSettings {
                source_id: "github-later-bootstrap".to_string(),
                enable_bootstrap: false,
                query_id: "later-query".to_string(),
                nodes: HashSet::new(),
                relations: HashSet::new(),
                resume_from: None,
                request_position_handle: false,
            },
            "github",
        )
        .await
        .expect("bootstrapping query live receiver")
        .receiver;
    let source_base = Arc::new(OnceLock::new());
    assert!(source_base.set(base).is_ok());
    let mut config = valid_config_with_port(0);
    config.graphql_url = format!("http://{addr}/graphql");
    config.projects.clear();
    let provider = crate::bootstrap::GitHubBootstrapProvider::new(
        config,
        Arc::new(RwLock::new(HashSet::new())),
        Arc::new(tokio::sync::Mutex::new(())),
        source_base,
    );
    let (event_tx, mut bootstrap_rx) = tokio::sync::mpsc::channel(100);
    let result = provider
        .bootstrap(
            BootstrapRequest {
                query_id: "later-query".to_string(),
                node_labels: vec![],
                relation_labels: vec![],
                request_id: "later-request".to_string(),
            },
            &BootstrapContext::new_minimal(
                "bootstrap-test".to_string(),
                "github-later-bootstrap".to_string(),
            ),
            event_tx,
            None,
        )
        .await
        .expect("later bootstrap");

    let mut live_changes = HashMap::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(20), live_rx.recv()).await
    {
        let SourceEvent::Change(change) = &event.event else {
            continue;
        };
        let (kind, id) = match change {
            SourceChange::Insert { element } => (
                "insert",
                element.get_metadata().reference.element_id.as_ref(),
            ),
            SourceChange::Update { element } => (
                "update",
                element.get_metadata().reference.element_id.as_ref(),
            ),
            SourceChange::Delete { metadata } => ("delete", metadata.reference.element_id.as_ref()),
            SourceChange::Future { .. } => continue,
        };
        live_changes.insert(id.to_string(), kind);
    }
    assert_eq!(live_changes.get("I_1"), Some(&"update"));
    assert_eq!(live_changes.get("I_2"), Some(&"insert"));
    assert_eq!(live_changes.get("PR_1"), Some(&"delete"));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), bootstrapping_live_rx.recv())
            .await
            .is_err(),
        "bootstrapping query must not receive its reconciliation delta"
    );

    let mut bootstrap_ids = HashSet::new();
    for _ in 0..result.event_count {
        let event = bootstrap_rx.recv().await.expect("full bootstrap event");
        assert!(
            !matches!(event.change, SourceChange::Delete { .. }),
            "full bootstrap must not contain reconcile deletions"
        );
        let id = match event.change {
            SourceChange::Insert { element } | SourceChange::Update { element } => element
                .get_metadata()
                .reference
                .element_id
                .as_ref()
                .to_string(),
            SourceChange::Delete { .. } | SourceChange::Future { .. } => unreachable!(),
        };
        bootstrap_ids.insert(id);
    }
    assert!(bootstrap_ids.contains("I_1"));
    assert!(bootstrap_ids.contains("I_2"));
    assert!(!bootstrap_ids.contains("PR_1"));

    let persisted =
        crate::hydrator::load_reconcile_index(state_store.as_ref(), "github-later-bootstrap")
            .await
            .expect("load updated index");
    assert!(persisted.contains_key("I_1"));
    assert!(persisted.contains_key("I_2"));
    assert!(!persisted.contains_key("PR_1"));
    assert!(
        load_root_snapshot(
            state_store.as_ref(),
            "github-later-bootstrap",
            "root-snapshot:PR_1"
        )
        .await
        .expect("load deleted root snapshot")
        .expect("deleted root tombstone")
        .elements
        .is_empty(),
        "deleted root snapshot must be persisted as a tombstone"
    );

    let (repeat_tx, _repeat_rx) = tokio::sync::mpsc::channel(100);
    provider
        .bootstrap(
            BootstrapRequest {
                query_id: "another-query".to_string(),
                node_labels: vec![],
                relation_labels: vec![],
                request_id: "repeat-request".to_string(),
            },
            &BootstrapContext::new_minimal(
                "bootstrap-test".to_string(),
                "github-later-bootstrap".to_string(),
            ),
            repeat_tx,
            None,
        )
        .await
        .expect("unchanged later bootstrap");
    assert!(
        tokio::time::timeout(Duration::from_millis(20), live_rx.recv())
            .await
            .is_err(),
        "unchanged snapshot must not produce a live delta"
    );
    server.abort();
}

fn test_source_base(id: &str) -> drasi_lib::sources::base::SourceBase {
    drasi_lib::sources::base::SourceBase::new(drasi_lib::sources::base::SourceBaseParams::new(id))
        .expect("create source base")
}

fn test_source_base_with_state_store(
    id: &str,
    state_store: Arc<dyn StateStoreProvider>,
) -> drasi_lib::sources::base::SourceBase {
    drasi_lib::sources::base::SourceBase::new(
        drasi_lib::sources::base::SourceBaseParams::new(id).with_state_store(state_store),
    )
    .expect("create source base")
}

struct FaultyStateStoreProvider {
    inner: Arc<dyn StateStoreProvider>,
    fail_store: String,
    fail_key: String,
    fail_get: bool,
    fail_set: bool,
    fail_delete_many: bool,
}

#[async_trait::async_trait]
impl StateStoreProvider for FaultyStateStoreProvider {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        if self.fail_get && store_id == self.fail_store && key == self.fail_key {
            return Err(StateStoreError::StorageError(
                "injected effective-repos load failure".to_string(),
            ));
        }
        self.inner.get(store_id, key).await
    }

    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        if self.fail_set && store_id == self.fail_store && key == self.fail_key {
            return Err(StateStoreError::StorageError(
                "injected reconcile-index commit failure".to_string(),
            ));
        }
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
        if self.fail_delete_many && store_id == self.fail_store {
            return Err(StateStoreError::StorageError(
                "injected dedupe compaction failure".to_string(),
            ));
        }
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

#[tokio::test]
async fn source_specific_state_store_is_shared_by_runtime_and_bootstrap() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("mock server addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            Router::new().route("/graphql", post(bootstrap_snapshot_handler)),
        )
        .await;
    });

    let temp = TempDir::new().expect("tempdir");
    let source_store: Arc<dyn StateStoreProvider> = Arc::new(
        RedbStateStoreProvider::new(temp.path().join("source-state.redb")).expect("source store"),
    );
    let context_store: Arc<dyn StateStoreProvider> = Arc::new(
        RedbStateStoreProvider::new(temp.path().join("context-state.redb")).expect("context store"),
    );
    let mut config = valid_config_with_port(0);
    config.graphql_url = format!("http://{addr}/graphql");
    config.projects.clear();
    let source = GitHubSourceBuilder::new("store-precedence")
        .with_config(config)
        .with_state_store(source_store.clone())
        .build()
        .expect("build source");
    let (update_tx, _update_rx) = tokio::sync::mpsc::channel(8);
    source
        .initialize(SourceRuntimeContext::new(
            "test-instance",
            "store-precedence",
            Some(context_store.clone()),
            update_tx,
            None,
        ))
        .await;

    let response = source
        .subscribe(SourceSubscriptionSettings {
            source_id: "store-precedence".to_string(),
            enable_bootstrap: true,
            query_id: "bootstrap-query".to_string(),
            nodes: HashSet::new(),
            relations: HashSet::new(),
            resume_from: None,
            request_position_handle: false,
        })
        .await
        .expect("subscribe");
    response
        .bootstrap_result_receiver
        .expect("bootstrap result receiver")
        .await
        .expect("bootstrap task result")
        .expect("bootstrap succeeds");

    let source_keys = source_store
        .list_keys("store-precedence")
        .await
        .expect("list source state");
    for expected in [
        "effective-repos",
        "reconcile-index",
        "root-snapshot:R_1",
        "root-snapshot:I_1",
    ] {
        assert!(
            source_keys.iter().any(|key| key == expected),
            "source-specific store missing {expected}: {source_keys:?}"
        );
    }
    assert_eq!(
        context_store
            .key_count("store-precedence")
            .await
            .expect("context state count"),
        0,
        "context fallback must remain unused"
    );
    server.abort();
}

#[tokio::test]
async fn bootstrap_persistence_failure_does_not_publish_live_delta() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("mock server addr");
    let server = tokio::spawn(async move {
        let _ = axum::serve(
            listener,
            Router::new().route("/graphql", post(bootstrap_snapshot_handler)),
        )
        .await;
    });

    let temp = TempDir::new().expect("tempdir");
    let inner: Arc<dyn StateStoreProvider> = Arc::new(
        RedbStateStoreProvider::new(temp.path().join("failed-bootstrap.redb"))
            .expect("state store"),
    );
    let faulty: Arc<dyn StateStoreProvider> = Arc::new(FaultyStateStoreProvider {
        inner: inner.clone(),
        fail_store: "failed-bootstrap".to_string(),
        fail_key: "reconcile-index".to_string(),
        fail_get: false,
        fail_set: true,
        fail_delete_many: false,
    });
    let base = test_source_base_with_state_store("failed-bootstrap", faulty);
    let mut live_rx = base
        .subscribe_with_bootstrap(
            &SourceSubscriptionSettings {
                source_id: "failed-bootstrap".to_string(),
                enable_bootstrap: false,
                query_id: "existing-query".to_string(),
                nodes: HashSet::new(),
                relations: HashSet::new(),
                resume_from: None,
                request_position_handle: false,
            },
            "github",
        )
        .await
        .expect("existing subscription")
        .receiver;
    let source_base = Arc::new(OnceLock::new());
    assert!(source_base.set(base).is_ok());
    let mut config = valid_config_with_port(0);
    config.graphql_url = format!("http://{addr}/graphql");
    config.projects.clear();
    let provider = crate::bootstrap::GitHubBootstrapProvider::new(
        config,
        Arc::new(RwLock::new(HashSet::new())),
        Arc::new(tokio::sync::Mutex::new(())),
        source_base,
    );
    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(100);
    let error = provider
        .bootstrap(
            BootstrapRequest {
                query_id: "bootstrap-query".to_string(),
                node_labels: vec![],
                relation_labels: vec![],
                request_id: "failed-request".to_string(),
            },
            &BootstrapContext::new_minimal(
                "test-instance".to_string(),
                "failed-bootstrap".to_string(),
            ),
            event_tx,
            None,
        )
        .await
        .expect_err("reconcile index persistence must fail");
    assert!(error.to_string().contains("reconcile-index"));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), live_rx.recv())
            .await
            .is_err(),
        "live delta must not be visible before durable commit"
    );
    assert!(
        inner
            .contains_key("failed-bootstrap", "pending-bootstrap-delta")
            .await
            .expect("pending marker"),
        "prepared marker must remain for recovery"
    );
    server.abort();
}

#[tokio::test]
async fn prepared_pending_bootstrap_delta_replays_after_restart_commit() {
    let temp = TempDir::new().expect("tempdir");
    let state_store: Arc<dyn StateStoreProvider> = Arc::new(
        RedbStateStoreProvider::new(temp.path().join("pending-replay.redb")).expect("state store"),
    );
    let mut snapshot = ReconcileSnapshot::default();
    snapshot
        .issues
        .insert("I_1".to_string(), sample_issue("pending"));
    let (changes, next_index) =
        map_reconcile_snapshot("pending-replay", &snapshot, &HashMap::new(), 1);
    let expected_ids = changes
        .iter()
        .map(|change| change.get_reference().element_id.to_string())
        .collect::<HashSet<_>>();
    crate::hydrator::save_reconcile_index(state_store.as_ref(), "pending-replay", &next_index)
        .await
        .expect("persist completed transition index");
    crate::bootstrap::save_pending_bootstrap_delta(
        state_store.as_ref(),
        "pending-replay",
        &crate::bootstrap::PendingBootstrapDelta {
            changes,
            next_index,
            excluded_query_id: "bootstrap-query".to_string(),
            committed: false,
        },
    )
    .await
    .expect("persist prepared marker");

    let base = test_source_base("pending-replay");
    assert!(
        !crate::bootstrap::replay_pending_bootstrap_delta(
            state_store.as_ref(),
            "pending-replay",
            &base,
            None,
        )
        .await
        .expect("defer replay without subscribers"),
        "pending delta must remain until a subscriber exists"
    );
    assert!(state_store
        .contains_key("pending-replay", "pending-bootstrap-delta")
        .await
        .expect("pending marker"));

    let mut receiver = base
        .subscribe_with_bootstrap(
            &SourceSubscriptionSettings {
                source_id: "pending-replay".to_string(),
                enable_bootstrap: false,
                query_id: "existing-query".to_string(),
                nodes: HashSet::new(),
                relations: HashSet::new(),
                resume_from: None,
                request_position_handle: false,
            },
            "github",
        )
        .await
        .expect("subscribe after restart")
        .receiver;
    assert!(crate::bootstrap::replay_pending_bootstrap_delta(
        state_store.as_ref(),
        "pending-replay",
        &base,
        None,
    )
    .await
    .expect("replay committed pending delta"));
    let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
        .await
        .expect("replayed event timeout")
        .expect("replayed event");
    let SourceEvent::Change(change) = &event.event else {
        panic!("expected replayed source change");
    };
    assert!(expected_ids.contains(change.get_reference().element_id.as_ref()));
    assert!(!state_store
        .contains_key("pending-replay", "pending-bootstrap-delta")
        .await
        .expect("pending marker cleared"));
}

struct RecoverableReadWalProvider {
    inner: Arc<dyn WalProvider>,
    inject_after_append: AtomicBool,
    fail_reads: AtomicBool,
}

impl RecoverableReadWalProvider {
    fn recover(&self) {
        self.inject_after_append.store(false, Ordering::SeqCst);
        self.fail_reads.store(false, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl WalProvider for RecoverableReadWalProvider {
    async fn register(&self, source_id: &str, config: WriteAheadLogConfig) -> Result<(), WalError> {
        self.inner.register(source_id, config).await
    }

    async fn append(&self, source_id: &str, event: &SourceChange) -> Result<u64, WalError> {
        let sequence = self.inner.append(source_id, event).await?;
        if self.inject_after_append.load(Ordering::SeqCst) {
            self.fail_reads.store(true, Ordering::SeqCst);
        }
        Ok(sequence)
    }

    async fn read_from(
        &self,
        source_id: &str,
        sequence: u64,
    ) -> Result<Vec<(u64, SourceChange)>, WalError> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(WalError::StorageError(
                "injected terminal WAL read failure".to_string(),
            ));
        }
        self.inner.read_from(source_id, sequence).await
    }

    async fn prune_up_to(&self, source_id: &str, sequence: u64) -> Result<u64, WalError> {
        self.inner.prune_up_to(source_id, sequence).await
    }

    async fn head_sequence(&self, source_id: &str) -> Result<u64, WalError> {
        self.inner.head_sequence(source_id).await
    }

    async fn oldest_sequence(&self, source_id: &str) -> Result<Option<u64>, WalError> {
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err(WalError::StorageError(
                "injected terminal WAL read failure".to_string(),
            ));
        }
        self.inner.oldest_sequence(source_id).await
    }

    async fn event_count(&self, source_id: &str) -> Result<u64, WalError> {
        self.inner.event_count(source_id).await
    }

    async fn delete_wal(&self, source_id: &str) -> Result<(), WalError> {
        self.inner.delete_wal(source_id).await
    }
}

#[test]
fn signature_validation_accepts_valid_signature() {
    let secret = b"top-secret";
    let body = br#"{"action":"opened"}"#;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret).expect("hmac init");
    mac.update(body);
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    assert!(verify_signature(secret, body, &signature).is_ok());
}

#[test]
fn signature_validation_rejects_tampered_payload() {
    let secret = b"top-secret";
    let body = br#"{"action":"opened"}"#;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret).expect("hmac init");
    mac.update(body);
    let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));

    let tampered = br#"{"action":"edited"}"#;
    assert!(verify_signature(secret, tampered, &signature).is_err());
}

#[test]
fn signature_validation_rejects_malformed_header() {
    assert!(verify_signature(b"secret", b"{}", "abcdef").is_err());
}

#[tokio::test]
async fn dedupe_compaction_bounds_markers_and_allows_pruned_delivery_readmission() {
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    wal.register(
        "dedupe-bounded",
        DurabilityConfig {
            enabled: true,
            max_events: 256,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register WAL");
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let mut sequences = Vec::new();
    for index in 0..100 {
        let delivery_id = format!("delivery-{index}");
        let change =
            encode_admission_change("dedupe-bounded", &delivery_id, &locator).expect("encode");
        let sequence = wal.append("dedupe-bounded", &change).await.expect("append");
        sequences.push(sequence);
        persist_dedupe_marker(
            state_store.as_ref(),
            "dedupe-bounded",
            &dedupe_key(&delivery_id),
            &delivery_id,
            sequence,
            "issues",
            "edited",
        )
        .await
        .expect("persist marker");

        if index >= 10 {
            wal.prune_up_to("dedupe-bounded", sequences[index - 10])
                .await
                .expect("prune old WAL");
            compact_dedupe_markers(state_store.as_ref(), wal.as_ref(), "dedupe-bounded")
                .await
                .expect("compact markers");
        }
        let marker_count = state_store
            .list_keys("dedupe-bounded")
            .await
            .expect("list markers")
            .into_iter()
            .filter(|key| key.starts_with("dedupe:"))
            .count();
        assert!(marker_count <= 10, "marker count was {marker_count}");
    }

    let old_delivery = "delivery-0";
    assert!(!state_store
        .contains_key("dedupe-bounded", &dedupe_key(old_delivery))
        .await
        .expect("check old marker"));
    assert_eq!(
        find_delivery_in_wal(wal.as_ref(), "dedupe-bounded", old_delivery)
            .await
            .expect("scan WAL"),
        None
    );

    let readmitted = encode_admission_change("dedupe-bounded", old_delivery, &locator)
        .expect("encode readmission");
    let readmitted_sequence = wal
        .append("dedupe-bounded", &readmitted)
        .await
        .expect("readmit old delivery after its WAL and marker were pruned");
    persist_dedupe_marker(
        state_store.as_ref(),
        "dedupe-bounded",
        &dedupe_key(old_delivery),
        old_delivery,
        readmitted_sequence,
        "issues",
        "edited",
    )
    .await
    .expect("persist readmitted marker");
}

#[tokio::test]
async fn retained_wal_delivery_remains_deduped_across_provider_restart() {
    let temp = TempDir::new().expect("tempdir");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_retained".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let config = DurabilityConfig {
        enabled: true,
        max_events: 32,
        capacity_policy: CapacityPolicy::RejectIncoming,
    }
    .to_wal_config();
    let sequence = {
        let wal = RedbWalProvider::new(temp.path());
        wal.register("dedupe-restart", config.clone())
            .await
            .expect("register first provider");
        let change = encode_admission_change("dedupe-restart", "retained-delivery", &locator)
            .expect("encode");
        let sequence = wal.append("dedupe-restart", &change).await.expect("append");
        persist_dedupe_marker(
            state_store.as_ref(),
            "dedupe-restart",
            &dedupe_key("retained-delivery"),
            "retained-delivery",
            sequence,
            "issues",
            "edited",
        )
        .await
        .expect("persist marker");
        sequence
    };

    let restarted = RedbWalProvider::new(temp.path());
    restarted
        .register("dedupe-restart", config)
        .await
        .expect("register restarted provider");
    compact_dedupe_markers(state_store.as_ref(), &restarted, "dedupe-restart")
        .await
        .expect("compact after restart");
    assert!(state_store
        .contains_key("dedupe-restart", &dedupe_key("retained-delivery"))
        .await
        .expect("check marker"));
    assert_eq!(
        find_delivery_in_wal(&restarted, "dedupe-restart", "retained-delivery")
            .await
            .expect("scan restarted WAL"),
        Some(sequence)
    );
}

#[tokio::test]
async fn dedupe_compaction_error_retains_marker_conservatively() {
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "dedupe-compaction-error",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register WAL");
    let inner = Arc::new(MemoryStateStoreProvider::new());
    inner
        .set(
            "dedupe-compaction-error",
            &dedupe_key("old"),
            serde_json::to_vec(&json!({
                "deliveryId": "old",
                "admittedSequence": 0,
                "eventType": "issues",
                "action": "edited"
            }))
            .expect("serialize marker"),
        )
        .await
        .expect("seed marker");
    let faulty = FaultyStateStoreProvider {
        inner: inner.clone(),
        fail_store: "dedupe-compaction-error".to_string(),
        fail_key: String::new(),
        fail_get: false,
        fail_set: false,
        fail_delete_many: true,
    };

    let error = compact_dedupe_markers(&faulty, wal.as_ref(), "dedupe-compaction-error")
        .await
        .expect_err("compaction delete must fail");
    assert!(format!("{error:#}").contains("dedupe compaction"));
    assert!(inner
        .contains_key("dedupe-compaction-error", &dedupe_key("old"))
        .await
        .expect("check retained marker"));
}

#[test]
fn mapping_issue_produces_expected_nodes_and_relations() {
    let mut issue = sample_issue("initial title");
    issue.body = Some("Context\nWorkGraph-Validation: pass\n".to_string());
    let root = FetchedRoot::Issue(issue);
    let (changes, snapshot): (Vec<SourceChange>, RootSnapshot) =
        map_root_diff("github-src", &root, None, 1_000).expect("map");

    assert!(!changes.is_empty());
    assert!(snapshot.elements.contains_key("I_1"));
    assert!(snapshot.elements.contains_key("IN_REPOSITORY:I_1:R_1"));
    assert!(snapshot.elements.contains_key("COMMENT_ON:IC_1:I_1"));
    let properties = &snapshot.elements["I_1"].properties;
    assert_eq!(
        properties["body"],
        json!("Context\nWorkGraph-Validation: pass\n")
    );
    assert_eq!(
        properties["bodyDigest"],
        json!("sha256:9faac769ff6962c7f331881d97518ff6a9df338da679c5d4851577cb7404a7fa")
    );
}

#[test]
fn mapping_pull_request_preserves_body_and_adds_authoritative_digest() {
    let body = "Context\nWorkGraph-Validation: pass\n";
    let root = FetchedRoot::PullRequest(sample_pull_request(Some(body.to_string())));
    let (_, snapshot) = map_root_diff("github-src", &root, None, 1_000).expect("map");

    let properties = &snapshot.elements["PR_1"].properties;
    assert_eq!(properties["body"], json!(body));
    assert_eq!(
        properties["bodyDigest"],
        json!("sha256:9faac769ff6962c7f331881d97518ff6a9df338da679c5d4851577cb7404a7fa")
    );
}

#[test]
fn mapping_body_digest_hashes_missing_body_as_empty_string() {
    let root = FetchedRoot::PullRequest(sample_pull_request(None));
    let (_, snapshot) = map_root_diff("github-src", &root, None, 1_000).expect("map");

    let properties = &snapshot.elements["PR_1"].properties;
    assert_eq!(properties["body"], serde_json::Value::Null);
    assert_eq!(
        properties["bodyDigest"],
        json!("sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    );
}

#[test]
fn mapping_project_item_emits_tracks_relation() {
    let item = sample_project_item();
    let root = FetchedRoot::ProjectItem(item);
    let (_, snapshot) = map_root_diff("github-src", &root, None, 1_000).expect("map");

    assert!(snapshot.elements.contains_key("IN_PROJECT:PVTI_1:PVT_1"));
    assert!(snapshot.elements.contains_key("TRACKS:PVTI_1:I_1"));
    assert!(!snapshot.elements.contains_key("HAS_ITEM:PVT_1:PVTI_1"));
    let properties = &snapshot.elements["PVTI_1"].properties;
    assert_eq!(properties["statusFieldId"], json!("status"));
    assert_eq!(properties["statusOptionId"], json!("opt1"));
    assert_eq!(properties["statusName"], json!("In Progress"));
}

#[test]
fn mapping_comment_review_shapes_include_author_fields() {
    let issue_comment = IssueCommentData {
        id: "IC_meta".to_string(),
        body: Some("comment".to_string()),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-02T00:00:00Z".to_string(),
        url: "https://github.com/acme/repo/issues/1#issuecomment-1".to_string(),
        is_minimized: false,
        author: Some(crate::graphql::ActorRef {
            id: Some("U_NODE_1".to_string()),
            login: Some("octocat".to_string()),
            actor_type: Some("User".to_string()),
            database_id: Some(42),
        }),
        issue: Some(NodeIdRef {
            id: "I_1".to_string(),
        }),
        pull_request: None,
        repository: RepositoryRef {
            id: "R_1".to_string(),
            name_with_owner: "acme/repo".to_string(),
        },
    };
    let review = PullRequestReviewData {
        id: "RV_1".to_string(),
        state: "APPROVED".to_string(),
        body: Some("looks good".to_string()),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-02T00:00:00Z".to_string(),
        url: "https://github.com/acme/repo/pull/1#review-1".to_string(),
        author: Some(crate::graphql::ActorRef {
            id: Some("U_NODE_2".to_string()),
            login: Some("reviewer".to_string()),
            actor_type: Some("Bot".to_string()),
            database_id: Some(77),
        }),
        pull_request: crate::graphql::PullRequestRef {
            id: "PR_1".to_string(),
            repository: RepositoryRef {
                id: "R_1".to_string(),
                name_with_owner: "acme/repo".to_string(),
            },
        },
        comments: single_connection(Vec::new()),
    };
    let review_comment = crate::graphql::PullRequestReviewCommentData {
        id: "RC_1".to_string(),
        body: Some("nit".to_string()),
        path: Some("src/lib.rs".to_string()),
        position: Some(1),
        line: Some(10),
        diff_hunk: Some("@@".to_string()),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        updated_at: "2026-01-02T00:00:00Z".to_string(),
        url: "https://github.com/acme/repo/pull/1#discussion_r1".to_string(),
        author: Some(crate::graphql::ActorRef {
            id: Some("U_NODE_3".to_string()),
            login: Some("reviewer2".to_string()),
            actor_type: Some("User".to_string()),
            database_id: Some(88),
        }),
        repository: RepositoryRef {
            id: "R_1".to_string(),
            name_with_owner: "acme/repo".to_string(),
        },
        pull_request_review: PullRequestReviewRef {
            id: "RV_1".to_string(),
            pull_request: crate::graphql::PullRequestRef {
                id: "PR_1".to_string(),
                repository: RepositoryRef {
                    id: "R_1".to_string(),
                    name_with_owner: "acme/repo".to_string(),
                },
            },
        },
    };

    let (_, comment_snapshot) = map_root_diff(
        "github-src",
        &FetchedRoot::IssueComment(issue_comment),
        None,
        1_000,
    )
    .expect("map issue comment");
    let (_, review_snapshot) = map_root_diff(
        "github-src",
        &FetchedRoot::PullRequestReview(review),
        None,
        1_000,
    )
    .expect("map review");
    let (_, review_comment_snapshot) = map_root_diff(
        "github-src",
        &FetchedRoot::PullRequestReviewComment(review_comment),
        None,
        1_000,
    )
    .expect("map review comment");

    let comment_props = &comment_snapshot.elements["IC_meta"].properties;
    assert_eq!(comment_props["authorId"], json!("U_NODE_1"));
    assert_eq!(comment_props["authorDatabaseId"], json!(42));
    assert_eq!(comment_props["authorType"], json!("User"));
    assert!(comment_props.get("performedViaGithubAppId").is_none());
    assert_eq!(comment_props["isEdited"], json!(true));

    let review_props = &review_snapshot.elements["RV_1"].properties;
    assert_eq!(review_props["authorId"], json!("U_NODE_2"));
    assert_eq!(review_props["authorDatabaseId"], json!(77));
    assert_eq!(review_props["authorType"], json!("Bot"));
    assert!(review_props.get("performedViaGithubAppId").is_none());

    let review_comment_props = &review_comment_snapshot.elements["RC_1"].properties;
    assert_eq!(review_comment_props["authorId"], json!("U_NODE_3"));
    assert_eq!(review_comment_props["authorDatabaseId"], json!(88));
    assert_eq!(review_comment_props["authorType"], json!("User"));
    assert!(review_comment_props
        .get("performedViaGithubAppId")
        .is_none());
    assert_eq!(review_comment_props["isEdited"], json!(true));
}

#[test]
fn relation_labels_match_contract() {
    let labels = relation_labels().into_iter().collect::<HashSet<_>>();
    let expected = HashSet::from([
        "IN_PROJECT".to_string(),
        "TRACKS".to_string(),
        "IN_REPOSITORY".to_string(),
        "COMMENT_ON".to_string(),
        "REVIEW_OF".to_string(),
        "PART_OF_REVIEW".to_string(),
    ]);
    assert_eq!(labels, expected);
}

#[test]
fn node_labels_match_contract() {
    let labels = node_labels().into_iter().collect::<HashSet<_>>();
    let expected = HashSet::from([
        "GitHubRepository".to_string(),
        "GitHubIssue".to_string(),
        "GitHubPullRequest".to_string(),
        "GitHubIssueComment".to_string(),
        "GitHubPullRequestReview".to_string(),
        "GitHubPullRequestReviewComment".to_string(),
        "GitHubProject".to_string(),
        "GitHubProjectItem".to_string(),
    ]);
    assert_eq!(labels, expected);
}

#[test]
fn mapping_update_emits_update_change() {
    let initial = FetchedRoot::Issue(sample_issue("initial"));
    let (_, snapshot) = map_root_diff("github-src", &initial, None, 1_000).expect("map initial");

    let updated = FetchedRoot::Issue(sample_issue("updated"));
    let (changes, _) =
        map_root_diff("github-src", &updated, Some(&snapshot), 2_000).expect("map update");

    assert!(changes.iter().any(|change| match change {
        SourceChange::Update { element } => element.get_reference().element_id.as_ref() == "I_1",
        _ => false,
    }));
}

#[test]
fn config_dto_deserialization_applies_defaults() {
    let config = serde_json::json!({
        "token": { "kind": "Secret", "name": "pat" },
        "webhook": {
            "secret": { "kind": "Secret", "name": "hook" }
        },
        "repositories": ["acme/repo"]
    });

    let dto: GitHubSourceConfigDto = serde_json::from_value(config).expect("dto");
    assert_eq!(dto.reconcile_interval_secs, ConfigValue::Static(300));
    match dto.token {
        ConfigValue::Secret { name } => assert_eq!(name, "pat"),
        _ => panic!("token must be secret"),
    }
}

#[test]
fn config_dto_accepts_exact_dogfood_shape() {
    let config = serde_json::json!({
        "token": { "kind": "Secret", "name": "github-pat" },
        "repositories": ["drasi-project/drasi-workgraph-demo"],
        "projects": [{ "owner": "drasi-project", "number": 3 }],
        "webhook": {
            "host": "${WEBHOOK_HOST:-127.0.0.1}",
            "port": "${WEBHOOK_PORT:-9000}",
            "path": "/github/events",
            "secret": { "kind": "Secret", "name": "github-webhook-secret" },
            "bodyLimitBytes": 10485760
        },
        "reconcileIntervalSecs": 300,
        "durability": {
            "enabled": true,
            "max_events": 10000,
            "capacity_policy": "RejectIncoming"
        },
        "graphqlUrl": "https://api.github.com/graphql",
        "skipInitialBootstrap": false
    });

    let dto: GitHubSourceConfigDto = serde_json::from_value(config).expect("dogfood DTO");
    assert_eq!(
        dto.repositories,
        vec![ConfigValue::Static(
            "drasi-project/drasi-workgraph-demo".to_string()
        )]
    );
    assert_eq!(
        dto.projects[0].owner,
        ConfigValue::Static("drasi-project".to_string())
    );
    assert_eq!(dto.projects[0].number, ConfigValue::Static(3));
    assert_eq!(
        dto.webhook.body_limit_bytes,
        ConfigValue::Static(10_485_760)
    );
    assert!(dto.durability.enabled);
    assert_eq!(dto.durability.max_events, 10_000);
    assert_eq!(
        dto.durability.capacity_policy,
        CapacityPolicy::RejectIncoming
    );
}

#[test]
fn config_dto_denies_unknown_fields() {
    let config = serde_json::json!({
        "token": { "kind": "Secret", "name": "pat" },
        "webhook": {
            "secret": { "kind": "Secret", "name": "hook" },
            "unknownField": true
        },
        "repositories": ["acme/repo"]
    });
    assert!(serde_json::from_value::<GitHubSourceConfigDto>(config).is_err());
}

#[test]
fn descriptor_schema_has_no_dangling_references() {
    fn check_refs(value: &serde_json::Value, schemas: &serde_json::Map<String, serde_json::Value>) {
        match value {
            serde_json::Value::Object(object) => {
                if let Some(reference) = object.get("$ref").and_then(|value| value.as_str()) {
                    let name = reference
                        .strip_prefix("#/components/schemas/")
                        .expect("schema references must target components/schemas");
                    assert!(
                        schemas.contains_key(name),
                        "schema reference {reference} is not registered"
                    );
                }
                for child in object.values() {
                    check_refs(child, schemas);
                }
            }
            serde_json::Value::Array(array) => {
                for child in array {
                    check_refs(child, schemas);
                }
            }
            _ => {}
        }
    }

    let schemas: serde_json::Value =
        serde_json::from_str(&GitHubSourceDescriptor.config_schema_json()).expect("schema JSON");
    let schemas = schemas.as_object().expect("schema map");
    assert!(schemas.contains_key("source.github.GitHubSourceConfig"));
    check_refs(&serde_json::Value::Object(schemas.clone()), schemas);
}

#[tokio::test]
async fn direct_builder_properties_are_complete_and_round_trip() {
    let pat = "literal-pat-for-protected-persistence";
    let webhook_secret = "literal-webhook-secret-for-protected-persistence";
    let mut config = valid_config_with_port(0);
    config.token = pat.to_string();
    config.webhook.secret = webhook_secret.to_string();
    let expected = config.clone();
    let source = GitHubSourceBuilder::new("github-secret-test")
        .with_config(config)
        .with_auto_start(false)
        .build()
        .expect("build source");

    let properties = source.properties();
    let properties_json = serde_json::to_string(&properties).expect("properties JSON");
    assert!(properties_json.contains(pat));
    assert!(properties_json.contains(webhook_secret));
    let rebuilt_config: GitHubSourceConfig =
        serde_json::from_value(serde_json::to_value(properties).expect("properties value"))
            .expect("deserialize complete properties");
    assert_eq!(rebuilt_config, expected);
    let rebuilt = GitHubSourceBuilder::new("github-secret-test-rebuilt")
        .with_config(rebuilt_config)
        .build()
        .expect("rebuild source from persisted properties");
    assert_eq!(
        serde_json::to_value(rebuilt.properties()).expect("rebuilt properties"),
        serde_json::to_value(source.properties()).expect("original properties")
    );

    let core = DrasiLib::builder()
        .with_id("github-secret-core")
        .with_source(source)
        .build()
        .await
        .expect("build core");
    let snapshot_json =
        serde_json::to_string(&core.snapshot_configuration().await.expect("snapshot"))
            .expect("snapshot JSON");
    assert!(snapshot_json.contains(pat));
    assert!(snapshot_json.contains(webhook_secret));
}

#[tokio::test]
async fn descriptor_properties_preserve_secret_references_without_resolved_values() {
    struct TestSecretResolver;

    #[async_trait::async_trait]
    impl ValueResolver for TestSecretResolver {
        async fn resolve_to_string(
            &self,
            value: &ConfigValue<String>,
        ) -> Result<String, ResolverError> {
            match value {
                ConfigValue::Secret { name } if name == "github-pat-ref" => {
                    Ok("resolved-pat-must-not-persist".to_string())
                }
                ConfigValue::Secret { name } if name == "github-hook-ref" => {
                    Ok("resolved-hook-must-not-persist".to_string())
                }
                _ => Err(ResolverError::WrongResolverType),
            }
        }
    }

    register_secret_resolver(Arc::new(TestSecretResolver));
    let raw = json!({
        "token": { "kind": "Secret", "name": "github-pat-ref" },
        "repositories": ["acme/repo"],
        "projects": [],
        "webhook": {
            "host": "127.0.0.1",
            "port": 8080,
            "path": "/webhook",
            "secret": { "kind": "Secret", "name": "github-hook-ref" },
            "bodyLimitBytes": 1048576
        },
        "reconcileIntervalSecs": 60,
        "durability": {
            "enabled": true,
            "maxEvents": 16,
            "capacityPolicy": "RejectIncoming"
        },
        "graphqlUrl": "https://api.github.com/graphql",
        "skipInitialBootstrap": true
    });
    let source = GitHubSourceDescriptor
        .create_source("descriptor-source", &raw, false)
        .await
        .expect("create descriptor source");
    let persisted = serde_json::to_value(source.properties()).expect("persist properties");
    assert_eq!(persisted["token"], raw["token"]);
    assert_eq!(persisted["webhook"]["secret"], raw["webhook"]["secret"]);
    let text = persisted.to_string();
    assert!(!text.contains("resolved-pat-must-not-persist"));
    assert!(!text.contains("resolved-hook-must-not-persist"));
}

#[test]
fn broadcast_dispatch_mode_is_rejected() {
    let result = GitHubSourceBuilder::new("github-broadcast")
        .with_config(valid_config_with_port(0))
        .with_dispatch_mode(DispatchMode::Broadcast)
        .build();
    let error = match result {
        Ok(_) => panic!("broadcast mode must be rejected"),
        Err(error) => error,
    };
    assert!(error
        .to_string()
        .contains("only supports DispatchMode::Channel"));
}

#[test]
fn config_rejects_overwrite_oldest_policy() {
    let mut config = valid_config_with_port(8080);
    config.durability.capacity_policy = CapacityPolicy::OverwriteOldest;
    assert!(config.validate().is_err());
}

#[test]
fn config_accepts_reject_incoming_policy() {
    let config = valid_config_with_port(8080);
    assert!(config.validate().is_ok());
}

#[test]
fn rate_limit_retry_after_header_is_honored() {
    let mut headers = ReqwestHeaderMap::new();
    headers.insert("retry-after", HeaderValue::from_static("3"));
    let decision = classify_retry(reqwest::StatusCode::TOO_MANY_REQUESTS, &headers, 0);
    assert!(decision.retryable);
    assert_eq!(decision.delay.as_secs(), 3);
}

#[test]
fn rate_limit_forbidden_retries_only_when_exhausted() {
    let mut exhausted_headers = ReqwestHeaderMap::new();
    exhausted_headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
    exhausted_headers.insert("retry-after", HeaderValue::from_static("1"));
    let exhausted = classify_retry(reqwest::StatusCode::FORBIDDEN, &exhausted_headers, 0);
    assert!(exhausted.retryable);
    assert_eq!(exhausted.delay.as_secs(), 1);

    let mut non_exhausted_headers = ReqwestHeaderMap::new();
    non_exhausted_headers.insert("x-ratelimit-remaining", HeaderValue::from_static("42"));
    let non_exhausted = classify_retry(reqwest::StatusCode::FORBIDDEN, &non_exhausted_headers, 0);
    assert!(!non_exhausted.retryable);
}

#[test]
fn rate_limit_reset_header_is_honored_for_exhausted_forbidden() {
    let reset_epoch = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
        + 2)
    .to_string();
    let mut headers = ReqwestHeaderMap::new();
    headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
    headers.insert(
        "x-ratelimit-reset",
        HeaderValue::from_str(&reset_epoch).unwrap(),
    );
    let decision = classify_retry(reqwest::StatusCode::FORBIDDEN, &headers, 0);
    assert!(decision.retryable);
    assert!(decision.delay.as_secs() <= 2);
}

#[test]
fn exponential_backoff_is_capped() {
    assert_eq!(exp_backoff(0).as_secs(), 1);
    assert_eq!(exp_backoff(6).as_secs(), 64);
    assert_eq!(exp_backoff(9).as_secs(), 64);
}

#[test]
fn locator_parsing_extracts_issue_node_id() {
    let payload = br#"{
        "action":"opened",
        "issue":{"node_id":"I_abc"},
        "repository":{"full_name":"Acme/Repo"}
    }"#;
    let locator = parse_locator("issues", payload).expect("parse");
    assert_eq!(locator.node_id.as_deref(), Some("I_abc"));
    assert_eq!(locator.repository_full_name.as_deref(), Some("acme/repo"));
}

#[test]
fn locator_parsing_handles_missing_optional_fields() {
    let payload = br#"{"action":"edited"}"#;
    let locator = parse_locator("issues", payload).expect("parse");
    assert_eq!(locator.action, "edited");
    assert!(locator.node_id.is_none());
}

#[test]
fn locator_parsing_issue_comment_prefers_comment_node_id() {
    let payload = br#"{
        "action":"created",
        "issue":{"node_id":"I_parent"},
        "comment":{"node_id":"IC_child"},
        "repository":{"full_name":"acme/repo"}
    }"#;
    let locator = parse_locator("issue_comment", payload).expect("parse");
    assert_eq!(locator.node_id.as_deref(), Some("IC_child"));
}

#[test]
fn locator_parsing_review_prefers_review_node_id() {
    let payload = br#"{
        "action":"submitted",
        "pull_request":{"node_id":"PR_parent"},
        "review":{"node_id":"R_child"},
        "repository":{"full_name":"acme/repo"}
    }"#;
    let locator = parse_locator("pull_request_review", payload).expect("parse");
    assert_eq!(locator.node_id.as_deref(), Some("R_child"));
}

#[test]
fn locator_parsing_project_item_uses_project_node_id_shape() {
    let payload = br#"{
        "action":"edited",
        "projects_v2_item":{"node_id":"PVTI_1","project_node_id":"PVT_1"}
    }"#;
    let locator = parse_locator("projects_v2_item", payload).expect("parse");
    assert_eq!(locator.node_id.as_deref(), Some("PVTI_1"));
    assert_eq!(locator.project_id.as_deref(), Some("PVT_1"));
    assert!(locator.project_owner.is_none());
    assert!(locator.project_number.is_none());
}

#[tokio::test]
async fn graphql_client_sends_bearer_token_header() {
    #[derive(Clone, Default)]
    struct AuthState {
        auth_header: Arc<RwLock<Option<String>>>,
    }

    async fn handler(
        State(state): State<AuthState>,
        headers: HeaderMap,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let auth = headers
            .get(AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .map(str::to_string);
        *state.auth_header.write().await = auth;
        Json(json!({
            "data": {
                "repository": {
                    "id": "R_1",
                    "name": "repo",
                    "nameWithOwner": "acme/repo",
                    "owner": { "login": "acme" },
                    "description": null,
                    "url": "https://github.com/acme/repo",
                    "isArchived": false,
                    "isPrivate": false,
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "defaultBranchRef": { "name": "main" }
                }
            }
        }))
    }

    let state = AuthState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("mock local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(
        format!("http://{addr}/graphql"),
        "test-pat-token".to_string(),
    )
    .expect("client");
    assert!(!format!("{client:?}").contains("test-pat-token"));
    client
        .fetch_repository("acme", "repo")
        .await
        .expect("fetch repository");

    let auth = state.auth_header.read().await.clone().expect("auth header");
    assert_eq!(auth, "Bearer test-pat-token");
    let invalid_token = "invalid-pat-must-not-leak\n";
    let error =
        GitHubGraphQLClient::new(format!("http://{addr}/graphql"), invalid_token.to_string())
            .expect_err("newline makes an invalid header value");
    assert!(!format!("{error:#?}").contains(invalid_token.trim()));
    let request_error_client = GitHubGraphQLClient::new(
        "not a valid URL".to_string(),
        "request-error-pat-must-not-leak".to_string(),
    )
    .expect("construct client with deferred invalid URL");
    let request_error = request_error_client
        .fetch_repository("acme", "repo")
        .await
        .expect_err("invalid URL must fail request");
    assert!(!format!("{request_error:#?}").contains("request-error-pat-must-not-leak"));
    server.abort();
}

#[test]
fn resolved_config_debug_and_schema_redact_secret_literals() {
    let mut config = valid_config_with_port(8080);
    config.token = "debug-pat-must-not-leak".to_string();
    config.webhook.secret = "debug-hook-must-not-leak".to_string();
    let debug = format!("{config:?}");
    assert!(!debug.contains(&config.token));
    assert!(!debug.contains(&config.webhook.secret));

    let schema = GitHubSourceDescriptor.config_schema_json();
    assert!(!schema.contains(&config.token));
    assert!(!schema.contains(&config.webhook.secret));

    let dto: GitHubSourceConfigDto = serde_json::from_value(json!({
        "token": "dto-pat-must-not-leak",
        "repositories": ["acme/repo"],
        "webhook": { "secret": "dto-hook-must-not-leak" }
    }))
    .expect("parse DTO for debug test");
    let dto_debug = format!("{dto:?}");
    assert!(!dto_debug.contains("dto-pat-must-not-leak"));
    assert!(!dto_debug.contains("dto-hook-must-not-leak"));
}

#[tokio::test]
async fn graphql_fetch_issue_comment_parses_authoritative_shape_fields() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let query = payload["query"].as_str().expect("query document");
        assert!(!query.contains("performedViaGithubApp"));
        assert!(!query.contains("__typename\n        id\n        login"));
        for actor_type in ["User", "Bot", "Organization", "Mannequin"] {
            assert!(query.contains(&format!("... on {actor_type} {{ id databaseId }}")));
        }
        assert!(query.contains("... on EnterpriseUserAccount { id }"));
        Json(json!({
            "data": {
                "node": {
                    "__typename": "IssueComment",
                    "id": "IC_1",
                    "body": "comment",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-02T00:00:00Z",
                    "url": "https://github.com/acme/repo/issues/1#issuecomment-1",
                    "isMinimized": false,
                    "author": {
                        "__typename": "User",
                        "id": "U_NODE_1",
                        "login": "octocat",
                        "databaseId": 7
                    },
                    "issue": { "id": "I_1" },
                    "pullRequest": null,
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let comment = client
        .fetch_issue_comment("IC_1")
        .await
        .expect("fetch")
        .expect("comment");

    assert_eq!(
        comment
            .author
            .as_ref()
            .and_then(|author| author.id.as_deref()),
        Some("U_NODE_1")
    );
    assert_eq!(
        comment
            .author
            .as_ref()
            .and_then(|author| author.database_id),
        Some(7)
    );
    assert_eq!(
        comment
            .author
            .as_ref()
            .and_then(|author| author.actor_type.as_deref()),
        Some("User")
    );
    server.abort();
}

#[tokio::test]
async fn graphql_client_retries_5xx_with_backoff_and_succeeds() {
    #[derive(Clone, Default)]
    struct RetryState {
        calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<RetryState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> (axum::http::StatusCode, Json<serde_json::Value>) {
        let attempt = state.calls.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({ "error": "temporary outage" })),
            );
        }
        (
            axum::http::StatusCode::OK,
            Json(json!({
                "data": {
                    "repository": {
                        "id": "R_1",
                        "name": "repo",
                        "nameWithOwner": "acme/repo",
                        "owner": { "login": "acme" },
                        "description": null,
                        "url": "https://github.com/acme/repo",
                        "isArchived": false,
                        "isPrivate": false,
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "defaultBranchRef": { "name": "main" }
                    }
                }
            })),
        )
    }

    let state = RetryState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let fetch_task = tokio::spawn(async move { client.fetch_repository("acme", "repo").await });
    let result = fetch_task.await.expect("task join").expect("fetch success");
    assert!(result.is_some());
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn graphql_client_retries_transient_transport_failure() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");

    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept");
            if attempt == 0 {
                drop(stream);
                continue;
            }

            let mut request_buffer = [0u8; 2048];
            let _ = stream.read(&mut request_buffer).await;
            let body = json!({
                "data": {
                    "repository": {
                        "id": "R_1",
                        "name": "repo",
                        "nameWithOwner": "acme/repo",
                        "owner": { "login": "acme" },
                        "description": null,
                        "url": "https://github.com/acme/repo",
                        "isArchived": false,
                        "isPrivate": false,
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "defaultBranchRef": { "name": "main" }
                    }
                }
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let fetch_task = tokio::spawn(async move { client.fetch_repository("acme", "repo").await });
    let result = fetch_task.await.expect("task join").expect("fetch success");
    assert!(result.is_some());
    server.await.expect("server task");
}

#[tokio::test]
async fn graphql_client_retries_retryable_graphql_error_then_succeeds() {
    #[derive(Clone, Default)]
    struct RetryState {
        calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<RetryState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let attempt = state.calls.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            return Json(json!({
                "errors": [{
                    "message": "Secondary rate limit. Please try again shortly.",
                    "type": "RATE_LIMITED"
                }]
            }));
        }
        Json(json!({
            "data": {
                "repository": {
                    "id": "R_1",
                    "name": "repo",
                    "nameWithOwner": "acme/repo",
                    "owner": { "login": "acme" },
                    "description": null,
                    "url": "https://github.com/acme/repo",
                    "isArchived": false,
                    "isPrivate": false,
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "defaultBranchRef": { "name": "main" }
                }
            }
        }))
    }

    let state = RetryState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let fetch_task = tokio::spawn(async move { client.fetch_repository("acme", "repo").await });
    let result = fetch_task.await.expect("task join").expect("fetch success");
    assert!(result.is_some());
    assert_eq!(state.calls.load(Ordering::SeqCst), 2);
    server.abort();
}

#[tokio::test]
async fn graphql_client_does_not_retry_permanent_graphql_errors() {
    #[derive(Clone, Default)]
    struct RetryState {
        calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<RetryState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.calls.fetch_add(1, Ordering::SeqCst);
        Json(json!({
            "errors": [{
                "message": "Could not resolve to a node with the global id",
                "type": "NOT_FOUND"
            }]
        }))
    }

    let state = RetryState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let err = client
        .fetch_repository("acme", "repo")
        .await
        .expect_err("permanent error should fail");
    assert!(format!("{err:#}").contains("returned errors"));
    assert_eq!(state.calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn fetch_repository_treats_path_not_found_as_absent() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": { "repository": null },
            "errors": [{
                "message": "Could not resolve to a Repository",
                "type": "NOT_FOUND",
                "path": ["repository"]
            }]
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let repository = client
        .fetch_repository("acme", "deleted")
        .await
        .expect("path-specific NOT_FOUND should be authoritative absence");
    assert!(repository.is_none());
    server.abort();
}

#[tokio::test]
async fn project_owner_lookup_accepts_only_alternate_namespace_not_found() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let query = payload["query"].as_str().expect("query document");
        assert!(!query.contains("owner { login }"));
        assert!(query.contains("... on Organization { login }"));
        assert!(query.contains("... on User { login }"));
        let owner = payload["variables"]["owner"].as_str().unwrap_or_default();
        let project = |id: &str, owner: &str| {
            json!({
                "id": id,
                "title": "Roadmap",
                "number": 1,
                "url": format!("https://github.com/users/{owner}/projects/1"),
                "createdAt": "2026-01-01T00:00:00Z",
                "updatedAt": "2026-01-01T00:00:00Z",
                "owner": { "login": owner },
                "items": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": []
                }
            })
        };

        if owner == "acme" {
            Json(json!({
                "data": {
                    "organization": { "projectV2": project("PVT_org", owner) },
                    "user": null
                },
                "errors": [{
                    "type": "NOT_FOUND",
                    "path": ["user"],
                    "locations": [{ "line": 8, "column": 3 }],
                    "message": "Could not resolve to a User with the login of 'acme'."
                }]
            }))
        } else {
            Json(json!({
                "data": {
                    "organization": null,
                    "user": { "projectV2": project("PVT_user", owner) }
                },
                "errors": [{
                    "type": "NOT_FOUND",
                    "path": ["organization"],
                    "locations": [{ "line": 2, "column": 3 }],
                    "message": "Could not resolve to an Organization with the login of 'octocat'."
                }]
            }))
        }
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");

    let organization_project = client
        .fetch_project_by_owner_number("acme", 1)
        .await
        .expect("organization project")
        .expect("organization project exists");
    assert_eq!(organization_project.id, "PVT_org");
    assert_eq!(organization_project.owner.login, "acme");

    let user_project = client
        .fetch_project_by_owner_number("octocat", 1)
        .await
        .expect("user project")
        .expect("user project exists");
    assert_eq!(user_project.id, "PVT_user");
    assert_eq!(user_project.owner.login, "octocat");
    server.abort();
}

#[tokio::test]
async fn nullable_node_lookup_only_accepts_path_specific_not_found() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let id = payload["variables"]["id"].as_str().unwrap_or_default();
        if id == "I_deleted" {
            return Json(json!({
                "data": { "node": null },
                "errors": [{
                    "type": "NOT_FOUND",
                    "path": ["node"],
                    "locations": [{ "line": 2, "column": 3 }],
                    "message": "Could not resolve to a node with the global id of 'I_deleted'"
                }]
            }));
        }
        Json(json!({
            "data": { "node": null },
            "errors": [{
                "type": "FORBIDDEN",
                "path": ["node"],
                "message": "Resource not accessible by personal access token"
            }]
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");

    assert!(client
        .fetch_issue("I_deleted")
        .await
        .expect("deleted node is authoritative absence")
        .is_none());
    let err = client
        .fetch_issue("I_forbidden")
        .await
        .expect_err("permission errors must not become absence");
    assert!(format!("{err:#}").contains("Resource not accessible"));
    server.abort();
}

#[tokio::test]
async fn fetch_issue_paginates_comments_across_pages() {
    #[derive(Clone, Default)]
    struct ServerState {
        comment_page_calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<ServerState>,
        Json(payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let cursor = payload
            .get("variables")
            .and_then(|v| v.get("cursor"))
            .and_then(|v| v.as_str());

        if query.contains("comments(first: 100, after: $cursor)") {
            state.comment_page_calls.fetch_add(1, Ordering::SeqCst);
            return Json(json!({
                "data": {
                    "node": {
                        "__typename": "Issue",
                        "comments": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "id": "IC_2",
                                "body": "second",
                                "createdAt": "2026-01-02T00:00:00Z",
                                "updatedAt": "2026-01-02T00:00:00Z",
                                "url": "https://github.com/acme/repo/issues/1#issuecomment-2",
                                "isMinimized": false,
                                "author": { "__typename": "User", "id": "U_2", "login": "user2", "databaseId": 2 },
                                "issue": { "id": "I_1" },
                                "pullRequest": null,
                                "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                            }]
                        }
                    }
                }
            }));
        }

        assert!(cursor.is_none());
        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "Issue",
                    "body": "body",
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": { "login": "octocat" },
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "c1" },
                        "nodes": [{
                            "id": "IC_1",
                            "body": "first",
                            "createdAt": "2026-01-01T00:00:00Z",
                            "updatedAt": "2026-01-01T00:00:00Z",
                            "url": "https://github.com/acme/repo/issues/1#issuecomment-1",
                            "isMinimized": false,
                            "author": { "__typename": "Bot", "id": "U_1", "login": "user1", "databaseId": 1 },
                            "issue": { "id": "I_1" },
                            "pullRequest": null,
                            "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                        }]
                    }
                }
            }
        }))
    }

    let state = ServerState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let issue = client
        .fetch_issue("I_1")
        .await
        .expect("fetch")
        .expect("issue");

    assert_eq!(issue.comments.nodes.len(), 2);
    assert_eq!(issue.comments.nodes[0].id, "IC_1");
    assert_eq!(issue.comments.nodes[1].id, "IC_2");
    assert_eq!(
        issue.comments.nodes[0]
            .author
            .as_ref()
            .and_then(|actor| actor.actor_type.as_deref()),
        Some("Bot")
    );
    assert_eq!(state.comment_page_calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn fetch_pull_request_paginates_reviews_and_review_comments() {
    #[derive(Clone, Default)]
    struct ServerState {
        review_page_calls: Arc<AtomicUsize>,
        review_comment_page_calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<ServerState>,
        Json(payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let cursor = payload
            .get("variables")
            .and_then(|v| v.get("cursor"))
            .and_then(|v| v.as_str());

        if query.contains("reviews(first: 100, after: $cursor)") {
            state.review_page_calls.fetch_add(1, Ordering::SeqCst);
            return Json(json!({
                "data": {
                    "node": {
                        "__typename": "PullRequest",
                        "reviews": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "id": "R_2",
                                "state": "COMMENTED",
                                "body": null,
                                "createdAt": "2026-01-03T00:00:00Z",
                                "updatedAt": "2026-01-03T00:00:00Z",
                                "url": "https://github.com/acme/repo/pull/1#review-2",
                                "author": { "__typename": "User", "id": "U_2", "login": "reviewer2", "databaseId": 2 },
                                "pullRequest": { "id": "PR_1", "repository": { "id": "R_1", "nameWithOwner": "acme/repo" } },
                                "comments": {
                                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                                    "nodes": []
                                }
                            }]
                        }
                    }
                }
            }));
        }

        if query.contains("... on PullRequestReview {") && query.contains("after: $cursor") {
            state
                .review_comment_page_calls
                .fetch_add(1, Ordering::SeqCst);
            return Json(json!({
                "data": {
                    "node": {
                        "__typename": "PullRequestReview",
                        "comments": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "id": "RC_2",
                                "body": "second review comment",
                                "path": "src/lib.rs",
                                "position": 2,
                                "line": 20,
                                "diffHunk": "@@",
                                "createdAt": "2026-01-02T00:00:00Z",
                                "updatedAt": "2026-01-02T00:00:00Z",
                                "url": "https://github.com/acme/repo/pull/1#discussion_r2",
                                "author": { "__typename": "Bot", "id": "U_3", "login": "reviewer3", "databaseId": 3 },
                                "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                                "pullRequestReview": { "id": "R_1", "pullRequest": { "id": "PR_1", "repository": { "id": "R_1", "nameWithOwner": "acme/repo" } } }
                            }]
                        }
                    }
                }
            }));
        }

        assert!(cursor.is_none());
        let response = serde_json::from_str::<serde_json::Value>(
            r#"{
              "data": {
                "node": {
                  "__typename": "PullRequest",
                  "id": "PR_1",
                  "number": 1,
                  "title": "PR title",
                  "body": "PR body",
                  "state": "OPEN",
                  "createdAt": "2026-01-01T00:00:00Z",
                  "updatedAt": "2026-01-01T00:00:00Z",
                  "closedAt": null,
                  "mergedAt": null,
                  "url": "https://github.com/acme/repo/pull/1",
                  "isDraft": false,
                  "headRefName": "feature",
                  "baseRefName": "main",
                  "author": { "login": "octocat" },
                  "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                  "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                  "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                  "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                  "reviews": {
                    "pageInfo": { "hasNextPage": true, "endCursor": "r1" },
                    "nodes": [{
                      "id": "R_1",
                      "state": "APPROVED",
                      "body": null,
                      "createdAt": "2026-01-01T00:00:00Z",
                      "updatedAt": "2026-01-01T00:00:00Z",
                      "url": "https://github.com/acme/repo/pull/1#review-1",
                      "author": { "__typename": "User", "id": "U_1", "login": "reviewer1", "databaseId": 1 },
                      "pullRequest": { "id": "PR_1", "repository": { "id": "R_1", "nameWithOwner": "acme/repo" } },
                      "comments": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "rc1" },
                        "nodes": [{
                          "id": "RC_1",
                          "body": "first review comment",
                          "path": "src/lib.rs",
                          "position": 1,
                          "line": 10,
                          "diffHunk": "@@",
                          "createdAt": "2026-01-01T00:00:00Z",
                          "updatedAt": "2026-01-01T00:00:00Z",
                          "url": "https://github.com/acme/repo/pull/1#discussion_r1",
                          "pullRequestReview": { "id": "R_1", "pullRequest": { "id": "PR_1", "repository": { "id": "R_1", "nameWithOwner": "acme/repo" } } },
                          "author": { "__typename": "User", "id": "U_1", "login": "reviewer1", "databaseId": 1 },
                          "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                        }]
                      }
                    }]
                  }
                }
              }
            }"#,
        )
        .expect("valid pull request response json");
        Json(response)
    }

    let state = ServerState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let pr = client
        .fetch_pull_request("PR_1")
        .await
        .expect("fetch")
        .expect("pr");

    assert_eq!(pr.reviews.nodes.len(), 2);
    assert_eq!(pr.reviews.nodes[0].comments.nodes.len(), 2);
    assert_eq!(pr.reviews.nodes[0].comments.nodes[1].id, "RC_2");
    assert_eq!(
        pr.reviews.nodes[0]
            .author
            .as_ref()
            .and_then(|author| author.actor_type.as_deref()),
        Some("User")
    );
    assert_eq!(state.review_page_calls.load(Ordering::SeqCst), 1);
    assert_eq!(state.review_comment_page_calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn fetch_project_item_paginates_field_values() {
    #[derive(Clone, Default)]
    struct ServerState {
        field_values_page_calls: Arc<AtomicUsize>,
    }

    async fn handler(
        State(state): State<ServerState>,
        Json(payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if query.contains("fieldValues(first: 50, after: $cursor)") {
            state.field_values_page_calls.fetch_add(1, Ordering::SeqCst);
            return Json(json!({
                "data": {
                    "node": {
                        "__typename": "ProjectV2Item",
                        "fieldValues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "__typename": "ProjectV2ItemFieldTextValue",
                                "text": "extra",
                                "field": { "id": "f2", "name": "Notes" }
                            }]
                        }
                    }
                }
            }));
        }

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
                        "title": "Issue",
                        "state": "OPEN",
                        "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                    },
                    "fieldValues": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "fv1" },
                        "nodes": [{
                            "__typename": "ProjectV2ItemFieldSingleSelectValue",
                            "name": "In Progress",
                            "optionId": "opt1",
                            "field": { "id": "f1", "name": "Status" }
                        }]
                    }
                }
            }
        }))
    }

    let state = ServerState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let item = client
        .fetch_project_item("PVTI_1")
        .await
        .expect("fetch")
        .expect("item");

    assert_eq!(item.field_values.nodes.len(), 2);
    assert_eq!(state.field_values_page_calls.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn fetch_issue_errors_when_has_next_page_missing_end_cursor() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "Issue",
                    "body": "body",
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": { "login": "octocat" },
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": true, "endCursor": null }, "nodes": [] }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let err = client
        .fetch_issue("I_1")
        .await
        .expect_err("missing cursor must fail");
    assert!(format!("{err:#}").contains("hasNextPage=true but endCursor was absent"));
    server.abort();
}

#[tokio::test]
async fn fetch_issue_errors_when_root_disappears_after_first_page() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if query.contains("comments(first: 100, after: $cursor)") {
            return Json(json!({ "data": { "node": null } }));
        }

        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "Issue",
                    "body": "body",
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": { "login": "octocat" },
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": true, "endCursor": "c1" }, "nodes": [] }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let err = client
        .fetch_issue("I_1")
        .await
        .expect_err("disappearing paginated root must fail");
    assert!(format!("{err:#}").contains("disappeared after first page"));
    server.abort();
}

#[tokio::test]
async fn fetch_all_issues_errors_when_repository_disappears_after_first_page() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let cursor = payload
            .get("variables")
            .and_then(|v| v.get("cursor"))
            .and_then(|v| v.as_str());

        if cursor.is_some() {
            return Json(json!({ "data": { "repository": null } }));
        }

        Json(json!({
            "data": {
                "repository": {
                    "issues": {
                        "pageInfo": { "hasNextPage": true, "endCursor": "next" },
                        "nodes": [{
                            "id": "I_1",
                            "number": 1,
                            "title": "Issue",
                            "body": "body",
                            "state": "OPEN",
                            "createdAt": "2026-01-01T00:00:00Z",
                            "updatedAt": "2026-01-01T00:00:00Z",
                            "closedAt": null,
                            "url": "https://github.com/acme/repo/issues/1",
                            "author": { "login": "octocat" },
                            "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                            "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                            "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                            "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                        }]
                    }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let client = GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
        .expect("client");
    let err = client
        .fetch_all_issues("acme/repo")
        .await
        .expect_err("repository disappearance must fail");
    assert!(format!("{err:#}").contains("Pagination root disappeared after first page"));
    server.abort();
}

#[tokio::test]
async fn hydrator_does_not_commit_partial_snapshot_on_pagination_failure() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        if query.contains("comments(first: 100, after: $cursor)") {
            return Json(json!({ "data": { "node": null } }));
        }

        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "from-graphql",
                    "body": "body",
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": { "login": "octocat" },
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": true, "endCursor": "c1" }, "nodes": [] }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let base = test_source_base("src");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let key = snapshot_key_for_locator(&locator, None);
    let (_, previous_snapshot) = map_root_diff(
        "src",
        &FetchedRoot::Issue(sample_issue("stable")),
        None,
        1_000,
    )
    .expect("snapshot");
    save_root_snapshot(state_store.as_ref(), "src", &key, &previous_snapshot)
        .await
        .expect("save previous");

    let admission = encode_admission_change("src", "delivery-partial", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    let err = process_admission(&params, sequence, &admission)
        .await
        .expect_err("pagination failure must fail the admission");
    assert!(format!("{err:#}").contains("disappeared after first page"));

    let persisted = load_root_snapshot(state_store.as_ref(), "src", &key)
        .await
        .expect("load persisted snapshot")
        .expect("snapshot exists");
    assert_eq!(
        persisted.elements["I_1"].properties["title"],
        json!("stable")
    );
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_some());
    server.abort();
}

#[tokio::test]
async fn processing_gate_serializes_reconcile_and_hydrator_delete() {
    #[derive(Clone)]
    struct ServerState {
        reconcile_pause_used: Arc<AtomicUsize>,
        reconcile_started: Arc<Notify>,
        reconcile_release: Arc<Notify>,
    }

    async fn handler(
        State(state): State<ServerState>,
        Json(payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let query = payload
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        let cursor = payload
            .get("variables")
            .and_then(|v| v.get("cursor"))
            .and_then(|v| v.as_str());

        if query.contains("query($owner: String!, $name: String!)")
            && query.contains("defaultBranchRef")
        {
            return Json(json!({
                "data": {
                    "repository": {
                        "id": "R_1",
                        "name": "repo",
                        "nameWithOwner": "acme/repo",
                        "owner": { "login": "acme" },
                        "description": null,
                        "url": "https://github.com/acme/repo",
                        "isArchived": false,
                        "isPrivate": false,
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "defaultBranchRef": null
                    }
                }
            }));
        }

        if query.contains("issues(first: 100, after: $cursor") {
            if state.reconcile_pause_used.fetch_add(1, Ordering::SeqCst) == 0 {
                state.reconcile_started.notify_waiters();
                state.reconcile_release.notified().await;
            }

            if cursor.is_some() {
                return Json(json!({
                    "data": {
                        "repository": {
                            "issues": {
                                "pageInfo": { "hasNextPage": false, "endCursor": null },
                                "nodes": []
                            }
                        }
                    }
                }));
            }

            return Json(json!({
                "data": {
                    "repository": {
                        "issues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": [{
                                "id": "I_1",
                                "number": 1,
                                "title": "stale issue",
                                "body": "body",
                                "state": "OPEN",
                                "createdAt": "2026-01-01T00:00:00Z",
                                "updatedAt": "2026-01-01T00:00:00Z",
                                "closedAt": null,
                                "url": "https://github.com/acme/repo/issues/1",
                                "author": { "login": "octocat" },
                                "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                                "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                                "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                                "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                            }]
                        }
                    }
                }
            }));
        }

        if query.contains("pullRequests(first: 100, after: $cursor") {
            return Json(json!({
                "data": {
                    "repository": {
                        "pullRequests": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }
                }
            }));
        }

        if query.contains("query($id: ID!)") && query.contains("... on Issue") {
            return Json(json!({ "data": { "node": null } }));
        }

        Json(json!({ "data": {} }))
    }

    let server_state = ServerState {
        reconcile_pause_used: Arc::new(AtomicUsize::new(0)),
        reconcile_started: Arc::new(Notify::new()),
        reconcile_release: Arc::new(Notify::new()),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(server_state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let processing_gate = Arc::new(tokio::sync::Mutex::new(()));
    let api_client = Arc::new(
        GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
            .expect("client"),
    );
    let effective_repos = Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()])));

    let issue = sample_issue("stable");
    let (_, root_snapshot) = map_root_diff("src", &FetchedRoot::Issue(issue.clone()), None, 1_000)
        .expect("root snapshot");
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "deleted".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let root_key = snapshot_key_for_locator(&locator, None);
    save_root_snapshot(state_store.as_ref(), "src", &root_key, &root_snapshot)
        .await
        .expect("save initial root snapshot");

    let mut reconcile_snapshot = ReconcileSnapshot::default();
    reconcile_snapshot.repositories.insert(
        "R_1".to_string(),
        RepositoryData {
            id: "R_1".to_string(),
            name: "repo".to_string(),
            name_with_owner: "acme/repo".to_string(),
            owner: OwnerRef {
                login: "acme".to_string(),
            },
            description: None,
            url: "https://github.com/acme/repo".to_string(),
            is_archived: false,
            is_private: false,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            default_branch_ref: None,
        },
    );
    reconcile_snapshot
        .issues
        .insert(issue.id.clone(), issue.clone());
    let (_, reconcile_index) =
        map_reconcile_snapshot("src", &reconcile_snapshot, &HashMap::new(), 1_000);
    crate::hydrator::save_reconcile_index(state_store.as_ref(), "src", &reconcile_index)
        .await
        .expect("seed reconcile index");

    let (reconcile_shutdown_tx, reconcile_shutdown_rx) = tokio::sync::watch::channel(false);
    let reconcile_params = ReconcilerParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        state_store: state_store.clone(),
        api_client: api_client.clone(),
        projects: vec![],
        static_repos: HashSet::from(["acme/repo".to_string()]),
        effective_repos: effective_repos.clone(),
        interval_secs: 3600,
        run_initial_pass: true,
        processing_gate: processing_gate.clone(),
        shutdown: reconcile_shutdown_rx,
    };
    let reconciler_task = tokio::spawn(async move { run_reconciler_loop(reconcile_params).await });

    tokio::time::timeout(
        Duration::from_secs(2),
        server_state.reconcile_started.notified(),
    )
    .await
    .expect("reconcile should begin and pause");

    let admission =
        encode_admission_change("src", "delivery-gated-delete", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    let hydrate_params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: api_client.clone(),
        projects: vec![],
        effective_repos,
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: processing_gate.clone(),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let hydrate_task =
        tokio::spawn(async move { process_admission(&hydrate_params, sequence, &admission).await });

    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        !hydrate_task.is_finished(),
        "hydrator should wait for the shared processing gate while reconcile holds it"
    );

    server_state.reconcile_release.notify_waiters();
    hydrate_task
        .await
        .expect("join hydrate task")
        .expect("hydrate delete should succeed");

    reconcile_shutdown_tx.send(true).expect("send shutdown");
    tokio::time::timeout(Duration::from_secs(2), reconciler_task)
        .await
        .expect("reconciler should stop quickly")
        .expect("join reconciler task")
        .expect("reconciler loop should exit cleanly");

    let tombstone = load_root_snapshot(state_store.as_ref(), "src", &root_key)
        .await
        .expect("load root snapshot")
        .expect("root snapshot should exist");
    assert!(tombstone.elements.is_empty());

    let index = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load reconcile index");
    assert!(!index.contains_key("I_1"));
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    server.abort();
}

#[tokio::test]
async fn start_rejects_non_durable_state_store() {
    let source = GitHubSourceBuilder::new("github-source-durable-test")
        .with_config(valid_config_with_port(0))
        .build()
        .expect("build source");

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    let core = DrasiLib::builder()
        .with_id("github-source-durable-core")
        .with_source(source)
        .with_state_store_provider(Arc::new(MemoryStateStoreProvider::new()))
        .with_wal_provider(wal)
        .build()
        .await
        .expect("build drasi");

    let err = core.start().await.expect_err("start should fail");
    assert!(format!("{err:#}").contains("is_durable"));
}

#[tokio::test]
async fn start_fails_fast_on_corrupted_effective_repos_state() {
    let source_id = "github-source-corrupt-effective-repos";
    let source = GitHubSourceBuilder::new(source_id)
        .with_config(valid_config_with_port(0))
        .build()
        .expect("build source");

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    let state_store = Arc::new(
        RedbStateStoreProvider::new(temp.path().join("corrupt-state.redb")).expect("state store"),
    );
    state_store
        .set(source_id, "effective-repos", b"{invalid".to_vec())
        .await
        .expect("seed corrupted state");

    let core = DrasiLib::builder()
        .with_id("github-source-corrupt-effective-core")
        .with_source(source)
        .with_state_store_provider(state_store.clone())
        .with_wal_provider(wal)
        .build()
        .await
        .expect("build drasi");

    let err = core.start().await.expect_err("start should fail");
    let err_text = format!("{err:#}");
    assert!(err_text.contains("Failed to load persisted effective repositories"));

    let persisted = state_store
        .get(source_id, "effective-repos")
        .await
        .expect("read persisted state")
        .expect("state present");
    assert_eq!(persisted, b"{invalid".to_vec());
}

#[tokio::test]
async fn start_fails_fast_when_loading_effective_repos_errors() {
    let source_id = "github-source-faulty-effective-repos";
    let source = GitHubSourceBuilder::new(source_id)
        .with_config(valid_config_with_port(0))
        .build()
        .expect("build source");

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    let durable_inner: Arc<dyn StateStoreProvider> = Arc::new(
        RedbStateStoreProvider::new(temp.path().join("faulty-state.redb")).expect("state store"),
    );
    let faulty_store: Arc<dyn StateStoreProvider> = Arc::new(FaultyStateStoreProvider {
        inner: durable_inner,
        fail_store: source_id.to_string(),
        fail_key: "effective-repos".to_string(),
        fail_get: true,
        fail_set: false,
        fail_delete_many: false,
    });

    let core = DrasiLib::builder()
        .with_id("github-source-faulty-effective-core")
        .with_source(source)
        .with_state_store_provider(faulty_store)
        .with_wal_provider(wal)
        .build()
        .await
        .expect("build drasi");

    let err = core.start().await.expect_err("start should fail");
    let err_text = format!("{err:#}");
    assert!(err_text.contains("Failed to load persisted effective repositories"));
    assert!(err_text.contains("effective-repos"));
}

#[tokio::test]
async fn stop_aborts_hung_graphql_task_and_allows_listener_restart() {
    #[derive(Clone, Default)]
    struct HungState {
        request_started: Arc<Notify>,
    }

    async fn hung_handler(
        State(state): State<HungState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.request_started.notify_one();
        std::future::pending().await
    }

    let hung_state = HungState::default();
    let graphql_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind GraphQL server");
    let graphql_addr = graphql_listener.local_addr().expect("GraphQL addr");
    let server_state = hung_state.clone();
    let graphql_server = tokio::spawn(async move {
        let _ = axum::serve(
            graphql_listener,
            Router::new()
                .route("/graphql", post(hung_handler))
                .with_state(server_state),
        )
        .await;
    });

    let webhook_port = find_available_port().await;
    let mut config = valid_config_with_port(webhook_port);
    config.graphql_url = format!("http://{graphql_addr}/graphql");
    let source = GitHubSourceBuilder::new("github-hung-stop")
        .with_config(config)
        .build()
        .expect("build source");
    let temp = TempDir::new().expect("tempdir");
    let core = DrasiLib::builder()
        .with_id("github-hung-stop-core")
        .with_source(source)
        .with_state_store_provider(Arc::new(
            RedbStateStoreProvider::new(temp.path().join("state.redb")).expect("state store"),
        ))
        .with_wal_provider(Arc::new(RedbWalProvider::new(temp.path())))
        .build()
        .await
        .expect("build core");
    core.start().await.expect("start core");

    let body = br#"{"action":"edited","issue":{"node_id":"I_hung"},"repository":{"full_name":"acme/repo"}}"#;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"secret").expect("hmac");
    mac.update(body);
    let response = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{webhook_port}/webhook"))
        .header(
            "X-Hub-Signature-256",
            format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
        )
        .header("X-GitHub-Delivery", "hung-delivery")
        .header("X-GitHub-Event", "issues")
        .body(body.as_slice().to_vec())
        .send()
        .await
        .expect("send webhook");
    assert!(response.status().is_success());
    tokio::time::timeout(
        Duration::from_secs(2),
        hung_state.request_started.notified(),
    )
    .await
    .expect("hung GraphQL request should start");

    tokio::time::timeout(Duration::from_secs(8), core.stop())
        .await
        .expect("stop must be bounded")
        .expect("stop core");
    drasi_lib::wait_for_status(
        &core.component_graph(),
        "github-hung-stop",
        &[drasi_lib::channels::ComponentStatus::Stopped],
        Duration::from_secs(2),
    )
    .await
    .expect("stopped status must be observed");
    core.start()
        .await
        .expect("listener must restart on the same port");
    tokio::time::timeout(Duration::from_secs(8), core.stop())
        .await
        .expect("second stop must be bounded")
        .expect("second stop");
    graphql_server.abort();
    let _ = graphql_server.await;
}

#[tokio::test]
async fn fatal_wal_read_marks_source_error_rejects_admission_and_recovers_after_restart() {
    async fn send_delivery(port: u16, delivery_id: &str) -> reqwest::Response {
        let body = br#"{"action":"updated","repository":{"full_name":"acme/repo"}}"#;
        let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(b"secret").expect("create HMAC");
        mac.update(body);
        reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/webhook"))
            .header(
                "X-Hub-Signature-256",
                format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
            )
            .header("X-GitHub-Delivery", delivery_id)
            .header("X-GitHub-Event", "push")
            .body(body.as_slice().to_vec())
            .send()
            .await
            .expect("send webhook")
    }

    let source_id = "github-terminal-wal";
    let webhook_port = find_available_port().await;
    let source = GitHubSourceBuilder::new(source_id)
        .with_config(valid_config_with_port(webhook_port))
        .build()
        .expect("build source");
    let temp = TempDir::new().expect("tempdir");
    let inner_wal: Arc<dyn WalProvider> = Arc::new(RedbWalProvider::new(temp.path()));
    let wal = Arc::new(RecoverableReadWalProvider {
        inner: inner_wal.clone(),
        inject_after_append: AtomicBool::new(true),
        fail_reads: AtomicBool::new(false),
    });
    let core = DrasiLib::builder()
        .with_id("github-terminal-wal-core")
        .with_source(source)
        .with_state_store_provider(Arc::new(
            RedbStateStoreProvider::new(temp.path().join("terminal-state.redb"))
                .expect("state store"),
        ))
        .with_wal_provider(wal.clone())
        .build()
        .await
        .expect("build core");
    core.start().await.expect("start core");

    let admitted = send_delivery(webhook_port, "fatal-trigger").await;
    assert_eq!(admitted.status(), reqwest::StatusCode::OK);
    drasi_lib::wait_for_status(
        &core.component_graph(),
        source_id,
        &[ComponentStatus::Error],
        Duration::from_secs(2),
    )
    .await
    .expect("fatal WAL read must transition source to Error");

    let retained_before = inner_wal
        .event_count(source_id)
        .await
        .expect("count WAL before rejected request");
    let rejected = send_delivery(webhook_port, "must-not-ack").await;
    assert_eq!(rejected.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let retained_after = inner_wal
        .event_count(source_id)
        .await
        .expect("count WAL after rejected request");
    assert_eq!(retained_after, retained_before);

    let direct_restart = core
        .start_source(source_id)
        .await
        .expect_err("direct start from Error must require an explicit stop");
    assert!(format!("{direct_restart:#}").contains("call stop first"));
    let rejected_after_start = send_delivery(webhook_port, "must-not-ack-after-start").await;
    assert_eq!(
        rejected_after_start.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        inner_wal
            .event_count(source_id)
            .await
            .expect("count WAL after direct start"),
        retained_before
    );

    core.stop_source(source_id)
        .await
        .expect("stop failed source");
    drasi_lib::wait_for_status(
        &core.component_graph(),
        source_id,
        &[ComponentStatus::Stopped],
        Duration::from_secs(2),
    )
    .await
    .expect("failed source must finish stopping");
    wal.recover();
    core.start_source(source_id)
        .await
        .expect("restart recovered source");
    drasi_lib::wait_for_status(
        &core.component_graph(),
        source_id,
        &[ComponentStatus::Running],
        Duration::from_secs(2),
    )
    .await
    .expect("source must return to Running");
    let recovered = send_delivery(webhook_port, "after-recovery").await;
    assert_eq!(recovered.status(), reqwest::StatusCode::OK);
    core.stop_source(source_id)
        .await
        .expect("stop recovered source");
    core.stop().await.expect("stop core");
}

#[tokio::test]
async fn hydrator_null_node_for_non_delete_action_returns_error() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({ "data": { "node": null } }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let base = test_source_base("src");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![ProjectSpec {
            owner: "acme".to_string(),
            number: 1,
        }],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission = encode_admission_change("src", "delivery-1", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");

    let err = process_admission(&params, sequence, &admission)
        .await
        .expect_err("non-delete null should retry");
    assert!(format!("{err:#}").contains("node=null"));
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_some());
    server.abort();
}

#[tokio::test]
async fn snapshot_delete_cleans_incident_tracks_without_duplicate_or_item_delete() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": { "node": null },
            "errors": [{
                "type": "NOT_FOUND",
                "path": ["node"],
                "locations": [{ "line": 2, "column": 3 }],
                "message": "Could not resolve to a node with the global id of 'I_1'"
            }]
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let base = test_source_base("src");
    let mut receiver = base
        .create_streaming_receiver()
        .await
        .expect("create event receiver");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![ProjectSpec {
            owner: "acme".to_string(),
            number: 1,
        }],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let previous_snapshot_key = "root-snapshot:I_1".to_string();
    let (_, previous_snapshot) = map_root_diff(
        "src",
        &FetchedRoot::Issue(sample_issue("existing")),
        None,
        1_000,
    )
    .expect("snapshot");
    save_root_snapshot(
        state_store.as_ref(),
        "src",
        &previous_snapshot_key,
        &previous_snapshot,
    )
    .await
    .expect("save previous");
    let mut reconcile_index = previous_snapshot.elements.clone();
    reconcile_index.insert(
        "PVTI_1".to_string(),
        SnapshotElement {
            element_type: "node".to_string(),
            id: "PVTI_1".to_string(),
            labels: vec!["GitHubProjectItem".to_string()],
            properties: json!({}),
            in_node_id: None,
            out_node_id: None,
        },
    );
    reconcile_index.insert(
        "TRACKS:PVTI_1:I_1".to_string(),
        SnapshotElement {
            element_type: "relation".to_string(),
            id: "TRACKS:PVTI_1:I_1".to_string(),
            labels: vec!["TRACKS".to_string()],
            properties: json!({}),
            in_node_id: Some("I_1".to_string()),
            out_node_id: Some("PVTI_1".to_string()),
        },
    );
    crate::hydrator::save_reconcile_index(state_store.as_ref(), "src", &reconcile_index)
        .await
        .expect("seed reconcile index");

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "deleted".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission = encode_admission_change("src", "delivery-delete", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    process_admission(&params, sequence, &admission)
        .await
        .expect("delete path");

    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    let tombstone = load_root_snapshot(state_store.as_ref(), "src", &previous_snapshot_key)
        .await
        .expect("load snapshot")
        .expect("snapshot exists");
    assert!(tombstone.elements.is_empty());
    assert_eq!(
        tombstone.committed_delivery_id.as_deref(),
        Some("delivery-delete")
    );
    let updated_index = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load updated reconcile index");
    assert!(!updated_index.contains_key("I_1"));
    assert!(!updated_index.contains_key("TRACKS:PVTI_1:I_1"));
    assert!(updated_index.contains_key("PVTI_1"));

    let mut delete_counts = HashMap::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(20), receiver.recv()).await
    {
        if let drasi_lib::channels::SourceEvent::Change(SourceChange::Delete { metadata }) =
            &event.event
        {
            *delete_counts
                .entry(metadata.reference.element_id.as_ref().to_string())
                .or_insert(0usize) += 1;
        }
    }
    assert_eq!(delete_counts.get("I_1"), Some(&1));
    assert_eq!(delete_counts.get("TRACKS:PVTI_1:I_1"), Some(&1));
    assert!(!delete_counts.contains_key("PVTI_1"));
    assert!(delete_counts.values().all(|count| *count == 1));
    server.abort();
}

#[tokio::test]
async fn hydrator_delete_uses_reconcile_index_when_root_snapshot_missing() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({ "data": { "node": null } }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let base = test_source_base("src");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let issue = sample_issue("existing");
    let mut reconcile_snapshot = ReconcileSnapshot::default();
    reconcile_snapshot.repositories.insert(
        issue.repository.id.clone(),
        RepositoryData {
            id: issue.repository.id.clone(),
            name: "repo".to_string(),
            name_with_owner: issue.repository.name_with_owner.clone(),
            owner: OwnerRef {
                login: "acme".to_string(),
            },
            description: None,
            url: "https://github.com/acme/repo".to_string(),
            is_archived: false,
            is_private: false,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            updated_at: "2026-01-01T00:00:00Z".to_string(),
            default_branch_ref: None,
        },
    );
    reconcile_snapshot
        .issues
        .insert(issue.id.clone(), issue.clone());
    for comment in &issue.comments.nodes {
        reconcile_snapshot
            .issue_comments
            .insert(comment.id.clone(), comment.clone());
    }
    let (_, mut reconcile_index) =
        map_reconcile_snapshot("src", &reconcile_snapshot, &HashMap::new(), 1_000);
    reconcile_index.insert(
        "PVTI_1".to_string(),
        SnapshotElement {
            element_type: "node".to_string(),
            id: "PVTI_1".to_string(),
            labels: vec!["GitHubProjectItem".to_string()],
            properties: json!({}),
            in_node_id: None,
            out_node_id: None,
        },
    );
    reconcile_index.insert(
        "TRACKS:PVTI_1:I_1".to_string(),
        SnapshotElement {
            element_type: "relation".to_string(),
            id: "TRACKS:PVTI_1:I_1".to_string(),
            labels: vec!["TRACKS".to_string()],
            properties: json!({}),
            in_node_id: Some("I_1".to_string()),
            out_node_id: Some("PVTI_1".to_string()),
        },
    );
    crate::hydrator::save_reconcile_index(state_store.as_ref(), "src", &reconcile_index)
        .await
        .expect("save reconcile index");

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "deleted".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission =
        encode_admission_change("src", "delivery-reconcile-delete", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    process_admission(&params, sequence, &admission)
        .await
        .expect("delete should succeed from reconcile index");

    let key = snapshot_key_for_locator(&locator, None);
    let tombstone = load_root_snapshot(state_store.as_ref(), "src", &key)
        .await
        .expect("load root snapshot")
        .expect("tombstone exists");
    assert!(tombstone.elements.is_empty());

    let updated_index = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load updated index");
    assert!(!updated_index.contains_key("I_1"));
    assert!(!updated_index.contains_key("IC_1"));
    assert!(!updated_index.contains_key("COMMENT_ON:IC_1:I_1"));
    assert!(!updated_index.contains_key("IN_REPOSITORY:I_1:R_1"));
    assert!(!updated_index.contains_key("TRACKS:PVTI_1:I_1"));
    assert!(updated_index.contains_key("PVTI_1"));
    assert!(updated_index.contains_key("R_1"));
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    server.abort();
}

#[tokio::test]
async fn archived_project_item_replay_uses_authoritative_current_state() {
    #[derive(Clone, Default)]
    struct ArchiveApiState {
        calls: Arc<AtomicUsize>,
        phase: Arc<AtomicUsize>,
    }

    async fn archive_handler(
        State(state): State<ArchiveApiState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.calls.fetch_add(1, Ordering::SeqCst);
        match state.phase.load(Ordering::SeqCst) {
            0 => Json(json!({ "data": { "node": null } })),
            1 => Json(json!({
                "data": {
                    "node": {
                        "__typename": "ProjectV2Item",
                        "id": "PVTI_1",
                        "type": "ISSUE",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-02-01T00:00:00Z",
                        "project": {
                            "id": "PVT_1",
                            "number": 1,
                            "owner": { "login": "acme" }
                        },
                        "content": {
                            "__typename": "Issue",
                            "id": "I_1",
                            "number": 42,
                            "title": "Restored issue",
                            "state": "OPEN",
                            "repository": { "id": "R_1", "nameWithOwner": "acme/repo" }
                        },
                        "fieldValues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }
                }
            })),
            _ => Json(json!({
                "data": { "node": null },
                "errors": [{
                    "type": "FORBIDDEN",
                    "path": ["node"],
                    "message": "Resource not accessible by integration"
                }]
            })),
        }
    }

    let api_state = ArchiveApiState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind GraphQL endpoint");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(archive_handler))
        .with_state(api_state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let index = HashMap::from([
        (
            "PVTI_1".to_string(),
            SnapshotElement {
                element_type: "node".to_string(),
                id: "PVTI_1".to_string(),
                labels: vec!["GitHubProjectItem".to_string()],
                properties: json!({}),
                in_node_id: None,
                out_node_id: None,
            },
        ),
        (
            "I_1".to_string(),
            SnapshotElement {
                element_type: "node".to_string(),
                id: "I_1".to_string(),
                labels: vec!["GitHubIssue".to_string()],
                properties: json!({}),
                in_node_id: None,
                out_node_id: None,
            },
        ),
        (
            "IN_PROJECT:PVTI_1:PVT_1".to_string(),
            SnapshotElement {
                element_type: "relation".to_string(),
                id: "IN_PROJECT:PVTI_1:PVT_1".to_string(),
                labels: vec!["IN_PROJECT".to_string()],
                properties: json!({}),
                in_node_id: Some("PVT_1".to_string()),
                out_node_id: Some("PVTI_1".to_string()),
            },
        ),
        (
            "TRACKS:PVTI_1:I_1".to_string(),
            SnapshotElement {
                element_type: "relation".to_string(),
                id: "TRACKS:PVTI_1:I_1".to_string(),
                labels: vec!["TRACKS".to_string()],
                properties: json!({}),
                in_node_id: Some("I_1".to_string()),
                out_node_id: Some("PVTI_1".to_string()),
            },
        ),
    ]);
    assert!(!index.contains_key("PVT_1"));
    crate::hydrator::save_reconcile_index(state_store.as_ref(), "src", &index)
        .await
        .expect("seed index");
    let locator = parse_locator(
        "projects_v2_item",
        br#"{"action":"archived","projects_v2_item":{"node_id":"PVTI_1","project_node_id":"PVT_1"}}"#,
    )
    .expect("parse archived project item webhook locator");
    let (_, item_snapshot) = map_root_diff(
        "src",
        &FetchedRoot::ProjectItem(sample_project_item()),
        None,
        1_000,
    )
    .expect("map standalone project item snapshot");
    save_root_snapshot(
        state_store.as_ref(),
        "src",
        &snapshot_key_for_locator(&locator, None),
        &item_snapshot,
    )
    .await
    .expect("save standalone project item snapshot");
    let base = test_source_base("src");
    let mut receiver = base
        .create_streaming_receiver()
        .await
        .expect("create event receiver");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![ProjectSpec {
            owner: "acme".to_string(),
            number: 1,
        }],
        effective_repos: Arc::new(RwLock::new(HashSet::new())),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let admission =
        encode_admission_change("src", "delivery-project-archived", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");

    tokio::time::timeout(
        Duration::from_secs(2),
        process_admission(&params, sequence, &admission),
    )
    .await
    .expect("authoritative archive lookup must complete")
    .expect("null archived item is an authoritative removal");

    let updated = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load index");
    assert!(!updated.contains_key("PVTI_1"));
    assert!(!updated.contains_key("IN_PROJECT:PVTI_1:PVT_1"));
    assert!(!updated.contains_key("TRACKS:PVTI_1:I_1"));
    assert!(updated.contains_key("I_1"));
    assert_eq!(api_state.calls.load(Ordering::SeqCst), 1);
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());

    let mut delete_counts = HashMap::new();
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(20), receiver.recv()).await
    {
        if let drasi_lib::channels::SourceEvent::Change(SourceChange::Delete { metadata }) =
            &event.event
        {
            *delete_counts
                .entry(metadata.reference.element_id.as_ref().to_string())
                .or_insert(0usize) += 1;
        }
    }
    assert_eq!(delete_counts.get("PVTI_1"), Some(&1));
    assert_eq!(delete_counts.get("IN_PROJECT:PVTI_1:PVT_1"), Some(&1));
    assert_eq!(delete_counts.get("TRACKS:PVTI_1:I_1"), Some(&1));
    assert!(!delete_counts.contains_key("PVT_1"));
    assert!(!delete_counts.contains_key("I_1"));
    assert!(delete_counts.values().all(|count| *count == 1));

    // A restore is authoritatively visible and re-seeds the item after the
    // archive admission and dedupe state have been pruned/compacted.
    api_state.phase.store(1, Ordering::SeqCst);
    let restored_locator = WebhookLocator {
        action: "edited".to_string(),
        ..locator.clone()
    };
    let restored_admission =
        encode_admission_change("src", "delivery-project-restored", &restored_locator)
            .expect("encode restore");
    let restored_sequence = wal
        .append("src", &restored_admission)
        .await
        .expect("append restore");
    process_admission(&params, restored_sequence, &restored_admission)
        .await
        .expect("restore current project item");

    // Redeliver the old archive after pruning. The current item wins, so the
    // stale archive must preserve/update it rather than deleting by signed ID.
    let replay_sequence = wal.append("src", &admission).await.expect("replay archive");
    process_admission(&params, replay_sequence, &admission)
        .await
        .expect("stale archived redelivery must converge to current item");
    let restored_index = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load restored index");
    assert!(restored_index.contains_key("PVTI_1"));
    assert!(restored_index.contains_key("IN_PROJECT:PVTI_1:PVT_1"));
    assert!(restored_index.contains_key("TRACKS:PVTI_1:I_1"));
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());

    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(20), receiver.recv()).await
    {
        if let drasi_lib::channels::SourceEvent::Change(SourceChange::Delete { metadata }) =
            &event.event
        {
            panic!(
                "restore/stale archive unexpectedly deleted {}",
                metadata.reference.element_id
            );
        }
    }

    // API unavailability is retryable and leaves the poison head durable.
    api_state.phase.store(2, Ordering::SeqCst);
    let unavailable_admission =
        encode_admission_change("src", "delivery-project-unavailable", &locator)
            .expect("encode unavailable archive");
    let unavailable_sequence = wal
        .append("src", &unavailable_admission)
        .await
        .expect("append unavailable archive");
    let err = process_admission(&params, unavailable_sequence, &unavailable_admission)
        .await
        .expect_err("API unavailable archive must retry");
    assert!(format!("{err:#}").contains("Resource not accessible"));
    assert_eq!(
        wal.oldest_sequence("src").await.expect("poison head"),
        Some(unavailable_sequence)
    );
    server.abort();
}

#[tokio::test]
async fn hydrator_project_scope_resolution_uses_authoritative_project_identity() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let id = payload
            .get("variables")
            .and_then(|v| v.get("id"))
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let response = if id == "PVTI_1" {
            json!({
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
                        "content": null,
                        "fieldValues": {
                            "pageInfo": { "hasNextPage": false, "endCursor": null },
                            "nodes": []
                        }
                    }
                }
            })
        } else {
            json!({
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
            })
        };
        Json(response)
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());

    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![ProjectSpec {
            owner: "acme".to_string(),
            number: 1,
        }],
        effective_repos: Arc::new(RwLock::new(HashSet::new())),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let configured_locator = WebhookLocator {
        event_type: "projects_v2_item".to_string(),
        action: "edited".to_string(),
        node_id: Some("PVTI_1".to_string()),
        repository_full_name: None,
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: Some("PVT_1".to_string()),
        project_owner: None,
        project_number: None,
    };
    let configured_admission =
        encode_admission_change("src", "delivery-project-configured", &configured_locator)
            .expect("encode");
    let configured_sequence = wal
        .append("src", &configured_admission)
        .await
        .expect("append");
    process_admission(&params, configured_sequence, &configured_admission)
        .await
        .expect("configured project should pass");

    let configured_key = snapshot_key_for_locator(&configured_locator, None);
    let configured_snapshot = load_root_snapshot(state_store.as_ref(), "src", &configured_key)
        .await
        .unwrap();
    assert!(configured_snapshot.is_some());

    let unconfigured_locator = WebhookLocator {
        event_type: "projects_v2_item".to_string(),
        action: "edited".to_string(),
        node_id: Some("PVTI_999".to_string()),
        repository_full_name: None,
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: Some("PVT_999".to_string()),
        project_owner: None,
        project_number: None,
    };
    let unconfigured_admission = encode_admission_change(
        "src",
        "delivery-project-unconfigured",
        &unconfigured_locator,
    )
    .expect("encode");
    let unconfigured_sequence = wal
        .append("src", &unconfigured_admission)
        .await
        .expect("append");
    process_admission(&params, unconfigured_sequence, &unconfigured_admission)
        .await
        .expect("skip unconfigured project");

    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    server.abort();
}

#[tokio::test]
async fn hydrator_skips_unsupported_event_type() {
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");

    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: Arc::new(MemoryStateStoreProvider::new()),
        api_client: Arc::new(
            GitHubGraphQLClient::new("http://127.0.0.1:9/graphql".to_string(), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::new())),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let locator = WebhookLocator {
        event_type: "member".to_string(),
        action: "added".to_string(),
        node_id: None,
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission =
        encode_admission_change("src", "delivery-unsupported", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    process_admission(&params, sequence, &admission)
        .await
        .expect("unsupported event should be skipped");
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
}

#[tokio::test]
async fn hydrator_replay_unpruned_delivery_uses_committed_marker() {
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new("http://127.0.0.1:9/graphql".to_string(), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission = encode_admission_change("src", "delivery-replay", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");

    let (changes, mut snapshot) = map_root_diff(
        "src",
        &FetchedRoot::Issue(sample_issue("existing")),
        None,
        1_000,
    )
    .expect("map");
    assert!(!changes.is_empty());
    snapshot.committed_delivery_id = Some("delivery-replay".to_string());
    snapshot.committed_sequence = Some(sequence);
    let key = snapshot_key_for_locator(&locator, None);
    save_root_snapshot(state_store.as_ref(), "src", &key, &snapshot)
        .await
        .expect("save committed snapshot");

    process_admission(&params, sequence, &admission)
        .await
        .expect("replay should prune");
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
}

#[tokio::test]
async fn hydrator_replay_without_marker_still_converges_to_latest_state() {
    #[derive(Clone)]
    struct ServerState {
        title: Arc<RwLock<String>>,
    }

    async fn handler(
        State(state): State<ServerState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        let title = state.title.read().await.clone();
        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": title,
                    "body": "body",
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": { "login": "octocat" },
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                }
            }
        }))
    }

    let server_state = ServerState {
        title: Arc::new(RwLock::new("initial".to_string())),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(server_state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(tokio::sync::Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };

    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };

    let admission_1 = encode_admission_change("src", "delivery-converge-1", &locator).unwrap();
    let seq_1 = wal.append("src", &admission_1).await.unwrap();
    process_admission(&params, seq_1, &admission_1)
        .await
        .unwrap();

    let key = snapshot_key_for_locator(&locator, None);
    state_store.delete("src", &key).await.unwrap();

    let admission_2 = encode_admission_change("src", "delivery-converge-2", &locator).unwrap();
    let seq_2 = wal.append("src", &admission_2).await.unwrap();
    process_admission(&params, seq_2, &admission_2)
        .await
        .unwrap();

    *server_state.title.write().await = "updated".to_string();
    let admission_3 = encode_admission_change("src", "delivery-converge-3", &locator).unwrap();
    let seq_3 = wal.append("src", &admission_3).await.unwrap();
    process_admission(&params, seq_3, &admission_3)
        .await
        .unwrap();

    let final_snapshot = load_root_snapshot(state_store.as_ref(), "src", &key)
        .await
        .unwrap()
        .unwrap();
    let issue_props = &final_snapshot.elements["I_1"].properties;
    assert_eq!(issue_props["title"], json!("updated"));
    assert!(wal.oldest_sequence("src").await.unwrap().is_none());
    server.abort();
}

#[tokio::test]
async fn webhook_only_create_updates_index_and_empty_reconcile_emits_delete() {
    async fn handler(Json(payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        let query = payload["query"].as_str().unwrap_or_default();
        if query.contains("query($id: ID!)") && query.contains("... on Issue") {
            return Json(json!({
                "data": {
                    "node": {
                        "__typename": "Issue",
                        "id": "I_webhook",
                        "number": 7,
                        "title": "Webhook only",
                        "body": null,
                        "state": "OPEN",
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "closedAt": null,
                        "url": "https://github.com/acme/repo/issues/7",
                        "author": null,
                        "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                        "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                        "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                        "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                    }
                }
            }));
        }
        if query.contains("defaultBranchRef") {
            return Json(json!({
                "data": {
                    "repository": {
                        "id": "R_1",
                        "name": "repo",
                        "nameWithOwner": "acme/repo",
                        "owner": { "login": "acme" },
                        "description": null,
                        "url": "https://github.com/acme/repo",
                        "isArchived": false,
                        "isPrivate": false,
                        "createdAt": "2026-01-01T00:00:00Z",
                        "updatedAt": "2026-01-01T00:00:00Z",
                        "defaultBranchRef": null
                    }
                }
            }));
        }
        if query.contains("issues(first: 100, after: $cursor") {
            return Json(json!({
                "data": { "repository": { "issues": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": []
                }}}
            }));
        }
        if query.contains("pullRequests(first: 100, after: $cursor") {
            return Json(json!({
                "data": { "repository": { "pullRequests": {
                    "pageInfo": { "hasNextPage": false, "endCursor": null },
                    "nodes": []
                }}}
            }));
        }
        Json(json!({ "data": {} }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let hydration_base = test_source_base("src");
    let reconcile_base = test_source_base("src");
    let mut receiver = reconcile_base
        .create_streaming_receiver()
        .await
        .expect("create event receiver");
    let api_client = Arc::new(
        GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
            .expect("client"),
    );
    let effective_repos = Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()])));
    let processing_gate = Arc::new(tokio::sync::Mutex::new(()));
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: hydration_base,
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: api_client.clone(),
        projects: vec![],
        effective_repos: effective_repos.clone(),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: processing_gate.clone(),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "opened".to_string(),
        node_id: Some("I_webhook".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission =
        encode_admission_change("src", "delivery-webhook-only", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    process_admission(&params, sequence, &admission)
        .await
        .expect("hydrate webhook create");

    let index = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load webhook-updated index");
    assert!(index.contains_key("I_webhook"));

    let reconcile_params = ReconcilerParams {
        source_id: "src".to_string(),
        base: reconcile_base,
        state_store: state_store.clone(),
        api_client,
        projects: vec![],
        static_repos: HashSet::from(["acme/repo".to_string()]),
        effective_repos,
        interval_secs: 60,
        run_initial_pass: false,
        processing_gate,
        shutdown: tokio::sync::watch::channel(false).1,
    };
    crate::reconciler::reconcile_once(&reconcile_params)
        .await
        .expect("empty reconcile");

    let mut saw_issue_delete = false;
    while let Ok(Ok(event)) = tokio::time::timeout(Duration::from_millis(20), receiver.recv()).await
    {
        if let drasi_lib::channels::SourceEvent::Change(SourceChange::Delete { metadata }) =
            &event.event
        {
            saw_issue_delete |= metadata.reference.element_id.as_ref() == "I_webhook";
        }
    }
    assert!(
        saw_issue_delete,
        "empty reconcile must emit the missed delete"
    );
    let index = crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
        .await
        .expect("load reconciled index");
    assert!(!index.contains_key("I_webhook"));
    server.abort();
}

#[tokio::test]
async fn reconcile_index_commit_failure_keeps_webhook_admission_in_wal() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "Issue",
                    "body": null,
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/acme/repo/issues/1",
                    "author": null,
                    "repository": { "id": "R_1", "nameWithOwner": "acme/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let inner: Arc<dyn StateStoreProvider> = Arc::new(MemoryStateStoreProvider::new());
    let state_store: Arc<dyn StateStoreProvider> = Arc::new(FaultyStateStoreProvider {
        inner,
        fail_store: "src".to_string(),
        fail_key: "reconcile-index".to_string(),
        fail_get: false,
        fail_set: true,
        fail_delete_many: false,
    });
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store,
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "opened".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission =
        encode_admission_change("src", "delivery-index-failure", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    let err = process_admission(&params, sequence, &admission)
        .await
        .expect_err("index commit must fail admission");
    assert!(format!("{err:#}").contains("reconcile-index"));
    assert_eq!(
        wal.oldest_sequence("src").await.expect("oldest"),
        Some(sequence)
    );
    server.abort();
}

#[tokio::test]
async fn stale_update_before_durable_delete_prunes_only_stale_head() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": { "node": null },
            "errors": [{
                "type": "NOT_FOUND",
                "path": ["node"],
                "message": "Could not resolve to a node with the global id of 'I_1'"
            }]
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: Arc::new(MemoryStateStoreProvider::new()),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let mut locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let stale = encode_admission_change("src", "delivery-stale", &locator).expect("encode stale");
    let stale_sequence = wal.append("src", &stale).await.expect("append stale");
    locator.action = "deleted".to_string();
    let delete =
        encode_admission_change("src", "delivery-delete", &locator).expect("encode delete");
    let delete_sequence = wal.append("src", &delete).await.expect("append delete");

    process_admission(&params, stale_sequence, &stale)
        .await
        .expect("stale head converges to queued delete");
    assert_eq!(
        wal.oldest_sequence("src").await.expect("oldest"),
        Some(delete_sequence),
        "queued delete must remain durable"
    );
    process_admission(&params, delete_sequence, &delete)
        .await
        .expect("authoritative delete");
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    server.abort();
}

#[tokio::test]
async fn repository_scope_is_rechecked_after_waiting_for_processing_gate() {
    #[derive(Clone, Default)]
    struct ServerState {
        calls: Arc<AtomicUsize>,
    }
    async fn handler(
        State(state): State<ServerState>,
        Json(_payload): Json<serde_json::Value>,
    ) -> Json<serde_json::Value> {
        state.calls.fetch_add(1, Ordering::SeqCst);
        Json(json!({ "data": { "node": null } }))
    }

    let server_state = ServerState::default();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new()
        .route("/graphql", post(handler))
        .with_state(server_state.clone());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let processing_gate = Arc::new(tokio::sync::Mutex::new(()));
    let gate_guard = processing_gate.lock().await;
    let effective_repos = Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()])));
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: Arc::new(MemoryStateStoreProvider::new()),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: effective_repos.clone(),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: processing_gate.clone(),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "edited".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission = encode_admission_change("src", "delivery-gate-race", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    let task = tokio::spawn(async move { process_admission(&params, sequence, &admission).await });
    tokio::task::yield_now().await;
    effective_repos.write().await.clear();
    drop(gate_guard);

    task.await
        .expect("join hydrator")
        .expect("out-of-scope delivery is skipped");
    assert_eq!(server_state.calls.load(Ordering::SeqCst), 0);
    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    server.abort();
}

#[tokio::test]
async fn fetched_authoritative_repository_mismatch_is_skipped() {
    async fn handler(Json(_payload): Json<serde_json::Value>) -> Json<serde_json::Value> {
        Json(json!({
            "data": {
                "node": {
                    "__typename": "Issue",
                    "id": "I_1",
                    "number": 1,
                    "title": "Transferred",
                    "body": null,
                    "state": "OPEN",
                    "createdAt": "2026-01-01T00:00:00Z",
                    "updatedAt": "2026-01-01T00:00:00Z",
                    "closedAt": null,
                    "url": "https://github.com/other/repo/issues/1",
                    "author": null,
                    "repository": { "id": "R_2", "nameWithOwner": "other/repo" },
                    "assignees": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "labels": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] },
                    "comments": { "pageInfo": { "hasNextPage": false, "endCursor": null }, "nodes": [] }
                }
            }
        }))
    }

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock server");
    let addr = listener.local_addr().expect("local addr");
    let app = Router::new().route("/graphql", post(handler));
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let temp = TempDir::new().expect("tempdir");
    let wal = Arc::new(RedbWalProvider::new(temp.path()));
    wal.register(
        "src",
        DurabilityConfig {
            enabled: true,
            max_events: 32,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }
        .to_wal_config(),
    )
    .await
    .expect("register wal");
    let state_store = Arc::new(MemoryStateStoreProvider::new());
    let params = HydratorParams {
        source_id: "src".to_string(),
        base: test_source_base("src"),
        wal: wal.clone(),
        state_store: state_store.clone(),
        api_client: Arc::new(
            GitHubGraphQLClient::new(format!("http://{addr}/graphql"), "pat".to_string())
                .expect("client"),
        ),
        projects: vec![],
        effective_repos: Arc::new(RwLock::new(HashSet::from(["acme/repo".to_string()]))),
        notify: Arc::new(Notify::new()),
        health: Arc::new(RwLock::new(HydratorHealth::default())),
        processing_gate: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio::sync::watch::channel(false).1,
    };
    let locator = WebhookLocator {
        event_type: "issues".to_string(),
        action: "transferred".to_string(),
        node_id: Some("I_1".to_string()),
        repository_full_name: Some("acme/repo".to_string()),
        parent_issue_id: None,
        parent_pull_request_id: None,
        project_id: None,
        project_owner: None,
        project_number: None,
    };
    let admission =
        encode_admission_change("src", "delivery-transferred", &locator).expect("encode");
    let sequence = wal.append("src", &admission).await.expect("append");
    process_admission(&params, sequence, &admission)
        .await
        .expect("authoritative out-of-scope object is skipped");

    assert!(wal.oldest_sequence("src").await.expect("oldest").is_none());
    assert!(
        crate::hydrator::load_reconcile_index(state_store.as_ref(), "src")
            .await
            .expect("load index")
            .is_empty()
    );
    assert!(load_root_snapshot(
        state_store.as_ref(),
        "src",
        &snapshot_key_for_locator(&locator, None)
    )
    .await
    .expect("load root snapshot")
    .is_none());
    server.abort();
}
