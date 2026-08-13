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

//! End-to-end tests for the Copilot Agent Task reaction.
//!
//! This is a **protocol-target** reaction: it calls the GitHub REST +
//! GraphQL APIs directly. A local `wiremock` server stands in for GitHub
//! (no Docker required, no live GitHub calls), and a `MemoryStateStoreProvider`
//! stands in for durable state so recovery/duplicate-delivery scenarios can
//! be driven deterministically.
//!
//! Run with: `cargo test -p drasi-reaction-copilot-agent-task --test integration_test -- --ignored --nocapture`

mod durable_memory_store;
mod mock_github;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{DrasiLib, Query, Reaction};
use drasi_reaction_copilot_agent_task::config::CommentApiConfig;
use drasi_reaction_copilot_agent_task::github::content_version_of;
use drasi_reaction_copilot_agent_task::ids::execution_id;
use drasi_reaction_copilot_agent_task::state::{load, save, ExecutionRecord, ExecutionStatus};
use drasi_reaction_copilot_agent_task::CopilotAgentTaskReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use durable_memory_store::DurableMemoryStateStoreProvider;
use serde_json::json;
use wiremock::MockServer;

const SOURCE: &str = "launch-source";
const QUERY: &str = "launch-query";
const REACTION: &str = "copilot-launcher";
const OWNER: &str = "drasi-project";
const REPO: &str = "drasi-core";
const REPOSITORY: &str = "drasi-project/drasi-core";
const ISSUE_BODY: &str = "please validate this issue";
const PROFILE_SHA: &str = "abc123sha";
const WARMUP: Duration = Duration::from_millis(150);

fn make_source() -> (
    ApplicationSource,
    drasi_source_application::ApplicationSourceHandle,
) {
    let config = ApplicationSourceConfig {
        properties: HashMap::new(),
        durability: None,
    };
    ApplicationSource::new(SOURCE, config).expect("create application source")
}

fn launch_query_str() -> &'static str {
    "MATCH (r:LaunchRequest) RETURN \
     r.repository AS repository, r.issueNumber AS issueNumber, r.issueUrl AS issueUrl, \
     r.issueNodeId AS issueNodeId, r.projectItemNodeId AS projectItemNodeId, \
     r.routeId AS routeId, r.responsibilityId AS responsibilityId, \
     r.issueContentVersion AS issueContentVersion, r.agentProfile AS agentProfile, \
     r.profileRef AS profileRef, r.requestedModel AS requestedModel, \
     r.fallbackModel AS fallbackModel, r.requiredEventType AS requiredEventType, \
     r.expectedEventId AS expectedEventId, r.baseRef AS baseRef, \
     r.expectedProjectStatus AS expectedProjectStatus"
}

fn build_reaction(server_uri: &str) -> CopilotAgentTaskReaction {
    CopilotAgentTaskReaction::builder(REACTION)
        .with_query(QUERY)
        .with_github_api_base_url(server_uri.to_string())
        .with_github_graphql_url(format!("{server_uri}/graphql"))
        .with_token("ghp_test_token_do_not_log")
        .with_allowed_repositories(vec![REPOSITORY.to_string()])
        .with_allowed_profiles(vec!["issue-validator".to_string()])
        .with_allowed_models(vec!["gpt-5".to_string(), "gpt-4".to_string()])
        .with_comment_api(CommentApiConfig {
            max_attempts: 2,
            retry_backoff_ms: 10,
        })
        .build()
        .expect("reaction builds")
}

/// Insert one launch row into the source. `issue_content_version` should be
/// computed with [`content_version_of`] over the mocked issue body so
/// preflight passes by default; pass a wrong value to exercise the mismatch
/// path.
#[allow(clippy::too_many_arguments)]
async fn insert_row(
    handle: &drasi_source_application::ApplicationSourceHandle,
    node_id: &str,
    route_id: &str,
    responsibility_id: &str,
    issue_number: i64,
    requested_model: &str,
    fallback_model: Option<&str>,
    repository: &str,
    issue_content_version: &str,
    expected_project_status: &str,
) {
    let mut builder = PropertyMapBuilder::new()
        .with_string("repository", repository)
        .with_integer("issueNumber", issue_number)
        .with_string(
            "issueUrl",
            format!("https://github.com/{repository}/issues/{issue_number}"),
        )
        .with_string("issueNodeId", format!("I_{node_id}"))
        .with_string("projectItemNodeId", format!("PVTI_{node_id}"))
        .with_string("routeId", route_id)
        .with_string("responsibilityId", responsibility_id)
        .with_string("issueContentVersion", issue_content_version)
        .with_string("agentProfile", "issue-validator")
        .with_string(
            "profileRef",
            format!("profiles/issue-validator.yml@{PROFILE_SHA}"),
        )
        .with_string("requestedModel", requested_model)
        .with_string("requiredEventType", "CompletedIssueValidation")
        .with_string("expectedEventId", format!("evt-{node_id}"))
        .with_string("baseRef", "main")
        .with_string("expectedProjectStatus", expected_project_status);
    if let Some(fb) = fallback_model {
        builder = builder.with_string("fallbackModel", fb);
    }
    let props = builder.build();
    handle
        .send_node_insert(node_id, vec!["LaunchRequest"], props)
        .await
        .expect("send node insert");
}

async fn wait_until<F, Fut>(mut cond: F, max_ms: u64)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + Duration::from_millis(max_ms);
    loop {
        if cond().await {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out after {max_ms}ms waiting for condition");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn mount_happy_path_preflight(server: &MockServer, issue_number: u64, node_id: &str) {
    let issue_node_id = format!("I_{node_id}");
    mock_github::mount_issue(
        server,
        OWNER,
        REPO,
        issue_number,
        "open",
        ISSUE_BODY,
        &issue_node_id,
    )
    .await;
    mock_github::mount_contents(server, OWNER, REPO, PROFILE_SHA).await;
    mock_github::mount_project_status(server, "In Progress", &issue_node_id).await;
}

// ---------------------------------------------------------------------
// 1. Success: full happy path launches a task and posts one comment.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored
async fn success_launches_task_and_posts_one_comment() {
    let server = MockServer::start().await;
    mount_happy_path_preflight(&server, 1, "issue-1").await;
    mock_github::mount_create_task_success(
        &server,
        OWNER,
        REPO,
        "task-1",
        "https://github.com/tasks/1",
    )
    .await;
    mock_github::mount_add_comment_success(&server).await;

    let (source, handle) = make_source();
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let reaction = build_reaction(&server.uri());

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("success-core")
            .with_source(source)
            .with_query(
                Query::cypher(QUERY)
                    .query(launch_query_str())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(reaction)
            .with_state_store_provider(store.clone())
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;

    let version = content_version_of(Some(ISSUE_BODY));
    insert_row(
        &handle,
        "issue-1",
        "route-1",
        "resp-1",
        1,
        "gpt-5",
        Some("gpt-4"),
        REPOSITORY,
        &version,
        "In Progress",
    )
    .await;

    wait_until(
        || async { mock_github::count_create_task_requests(&server, OWNER, REPO).await == 1 },
        5000,
    )
    .await;
    wait_until(
        || async { mock_github::count_add_comment_requests(&server).await == 1 },
        5000,
    )
    .await;

    let exec_id = execution_id(REACTION, "route-1", "resp-1", 1);
    let record = load(store.as_ref(), REACTION, "route-1", "resp-1", 1)
        .await
        .expect("load ok")
        .expect("record exists");
    assert_eq!(record.execution_id, exec_id);
    assert_eq!(record.status, ExecutionStatus::Started);
    assert!(record.comment_posted);
    assert_eq!(record.task_id.as_deref(), Some("task-1"));
    assert_eq!(record.model_used.as_deref(), Some("gpt-5"));
    assert!(!record.used_fallback);

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 2. Validation: disallowed repository never launches.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn validation_rejects_disallowed_repository_and_never_launches() {
    let server = MockServer::start().await;
    // No preflight or create-task mocks are mounted for the disallowed repo:
    // if the reaction incorrectly attempted to launch, the request would go
    // unmatched and the assertions below would fail.

    let (source, handle) = make_source();
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let reaction = build_reaction(&server.uri());

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("validation-core")
            .with_source(source)
            .with_query(
                Query::cypher(QUERY)
                    .query(launch_query_str())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(reaction)
            .with_state_store_provider(store.clone())
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;

    insert_row(
        &handle,
        "issue-2",
        "route-2",
        "resp-2",
        2,
        "gpt-5",
        None,
        "other-org/other-repo",
        "any-version",
        "In Progress",
    )
    .await;

    wait_until(
        || async {
            async {
                load(store.as_ref(), REACTION, "route-2", "resp-2", 1)
                    .await
                    .unwrap()
                    .is_some()
            }
            .await
        },
        5000,
    )
    .await;

    let record = load(store.as_ref(), REACTION, "route-2", "resp-2", 1)
        .await
        .expect("load ok")
        .expect("failed record persisted");
    assert_eq!(record.status, ExecutionStatus::Failed);
    assert!(record
        .last_error
        .unwrap()
        .contains("not in the allowed-repositories list"));
    assert_eq!(
        mock_github::count_create_task_requests(&server, "other-org", "other-repo").await,
        0
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 3. Fallback: unsupported requested model triggers exactly one fallback.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn fallback_used_exactly_once_on_unsupported_model() {
    let server = MockServer::start().await;
    mount_happy_path_preflight(&server, 3, "issue-3").await;
    mock_github::mount_create_task_unsupported_model(
        &server,
        OWNER,
        REPO,
        "gpt-5",
        "task-fb",
        "https://github.com/tasks/fb",
    )
    .await;
    mock_github::mount_add_comment_success(&server).await;

    let (source, handle) = make_source();
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let reaction = build_reaction(&server.uri());

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("fallback-core")
            .with_source(source)
            .with_query(
                Query::cypher(QUERY)
                    .query(launch_query_str())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(reaction)
            .with_state_store_provider(store.clone())
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;

    let version = content_version_of(Some(ISSUE_BODY));
    insert_row(
        &handle,
        "issue-3",
        "route-3",
        "resp-3",
        3,
        "gpt-5",
        Some("gpt-4"),
        REPOSITORY,
        &version,
        "In Progress",
    )
    .await;

    wait_until(
        || async { mock_github::count_create_task_requests(&server, OWNER, REPO).await == 2 },
        5000,
    )
    .await;

    let record = load(store.as_ref(), REACTION, "route-3", "resp-3", 1)
        .await
        .expect("load ok")
        .expect("record exists");
    assert_eq!(record.status, ExecutionStatus::Started);
    assert!(record.used_fallback);
    assert_eq!(record.model_used.as_deref(), Some("gpt-4"));
    assert_eq!(record.task_id.as_deref(), Some("task-fb"));

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 4. No fallback: a non-model-related 422 never triggers a fallback retry.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn no_fallback_on_unrelated_422() {
    let server = MockServer::start().await;
    mount_happy_path_preflight(&server, 4, "issue-4").await;
    mock_github::mount_create_task_permanent_422(
        &server,
        OWNER,
        REPO,
        "Validation failed: base_ref does not exist",
    )
    .await;

    let (source, handle) = make_source();
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let reaction = build_reaction(&server.uri());

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("no-fallback-core")
            .with_source(source)
            .with_query(
                Query::cypher(QUERY)
                    .query(launch_query_str())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(reaction)
            .with_state_store_provider(store.clone())
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;

    let version = content_version_of(Some(ISSUE_BODY));
    insert_row(
        &handle,
        "issue-4",
        "route-4",
        "resp-4",
        4,
        "gpt-5",
        Some("gpt-4"),
        REPOSITORY,
        &version,
        "In Progress",
    )
    .await;

    wait_until(
        || async {
            async {
                load(store.as_ref(), REACTION, "route-4", "resp-4", 1)
                    .await
                    .unwrap()
                    .map(|r| r.status == ExecutionStatus::Failed)
                    .unwrap_or(false)
            }
            .await
        },
        5000,
    )
    .await;

    // Exactly one create-task call: no fallback retry was attempted.
    assert_eq!(
        mock_github::count_create_task_requests(&server, OWNER, REPO).await,
        1
    );
    let record = load(store.as_ref(), REACTION, "route-4", "resp-4", 1)
        .await
        .unwrap()
        .unwrap();
    assert!(!record.used_fallback);

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 5. Duplicate delivery: the same (routeId, responsibilityId) delivered
//    twice (e.g. a duplicate upstream emission) only launches once.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn duplicate_delivery_launches_only_once() {
    let server = MockServer::start().await;
    mount_happy_path_preflight(&server, 5, "issue-5a").await;
    mock_github::mount_create_task_success(
        &server,
        OWNER,
        REPO,
        "task-dup",
        "https://github.com/tasks/dup",
    )
    .await;
    mock_github::mount_add_comment_success(&server).await;

    let (source, handle) = make_source();
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let reaction = build_reaction(&server.uri());

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("dup-core")
            .with_source(source)
            .with_query(
                Query::cypher(QUERY)
                    .query(launch_query_str())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(reaction)
            .with_state_store_provider(store.clone())
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;

    let version = content_version_of(Some(ISSUE_BODY));
    insert_row(
        &handle,
        "issue-5a",
        "route-5",
        "resp-5",
        5,
        "gpt-5",
        None,
        REPOSITORY,
        &version,
        "In Progress",
    )
    .await;
    wait_until(
        || async { mock_github::count_create_task_requests(&server, OWNER, REPO).await == 1 },
        5000,
    )
    .await;
    wait_until(
        || async { mock_github::count_add_comment_requests(&server).await == 1 },
        5000,
    )
    .await;

    // Same routeId/responsibilityId delivered again under a different node
    // id (simulating a duplicate upstream emission at a new sequence).
    insert_row(
        &handle,
        "issue-5b",
        "route-5",
        "resp-5",
        5,
        "gpt-5",
        None,
        REPOSITORY,
        &version,
        "In Progress",
    )
    .await;
    tokio::time::sleep(Duration::from_millis(1000)).await;

    assert_eq!(
        mock_github::count_create_task_requests(&server, OWNER, REPO).await,
        1,
        "duplicate delivery must not create a second task"
    );
    assert_eq!(
        mock_github::count_add_comment_requests(&server).await,
        1,
        "duplicate delivery must not post a second comment"
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 6. Crash/recovery boundary: a record left in `Starting` (simulating a
//    crash between reservation and confirmed task creation) is reconciled
//    — not blindly retried — the next time the row is processed.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn crash_recovery_adopts_exactly_one_existing_task() {
    let server = MockServer::start().await;
    mount_happy_path_preflight(&server, 6, "issue-6").await;
    let exec_id = execution_id(REACTION, "route-6", "resp-6", 1);
    mock_github::mount_list_tasks(
        &server,
        OWNER,
        REPO,
        vec![json!({
            "id": "task-recovered",
            "html_url": "https://github.com/tasks/rec",
            "prompt": format!("...{exec_id}...")
        })],
    )
    .await;
    mock_github::mount_add_comment_success(&server).await;

    let (source, handle) = make_source();
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    // Pre-seed a `Starting` record — simulating a crash right after the
    // durable reservation was marked Starting but before task creation was
    // confirmed, and before the checkpoint advanced (so this row will be
    // redelivered on "restart").
    let mut record = ExecutionRecord::new_reserved(
        "route-6",
        "resp-6",
        1,
        &exec_id,
        "evt-issue-6",
        "CompletedIssueValidation",
        REPOSITORY,
        6,
        "gpt-5",
        None,
    );
    record.status = ExecutionStatus::Starting;
    save(store.as_ref(), REACTION, &record)
        .await
        .expect("seed Starting record");

    let reaction = build_reaction(&server.uri());
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("crash-recovery-core")
            .with_source(source)
            .with_query(
                Query::cypher(QUERY)
                    .query(launch_query_str())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(reaction)
            .with_state_store_provider(store.clone())
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;

    let version = content_version_of(Some(ISSUE_BODY));
    insert_row(
        &handle,
        "issue-6",
        "route-6",
        "resp-6",
        6,
        "gpt-5",
        None,
        REPOSITORY,
        &version,
        "In Progress",
    )
    .await;

    wait_until(
        || async { mock_github::count_add_comment_requests(&server).await == 1 },
        5000,
    )
    .await;

    let record = load(store.as_ref(), REACTION, "route-6", "resp-6", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.status, ExecutionStatus::Started);
    assert_eq!(record.task_id.as_deref(), Some("task-recovered"));
    assert_eq!(
        mock_github::count_create_task_requests(&server, OWNER, REPO).await,
        0,
        "recovery must adopt the correlated task without creating another"
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 7. Ambiguous reconciliation: no correlated task is visible in the recent
//    listing. Absence is not proof that creation failed, so the reaction
//    stays Ambiguous and never launches a duplicate.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn ambiguous_reconciliation_with_no_match_never_retries() {
    let server = MockServer::start().await;
    mount_happy_path_preflight(&server, 7, "issue-7").await;

    let (source, handle) = make_source();
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let exec_id = execution_id(REACTION, "route-7", "resp-7", 1);
    mock_github::mount_list_tasks(&server, OWNER, REPO, vec![]).await;

    let mut record = ExecutionRecord::new_reserved(
        "route-7",
        "resp-7",
        1,
        &exec_id,
        "evt-issue-7",
        "CompletedIssueValidation",
        REPOSITORY,
        7,
        "gpt-5",
        None,
    );
    record.status = ExecutionStatus::Starting;
    save(store.as_ref(), REACTION, &record)
        .await
        .expect("seed Starting record");

    let reaction = build_reaction(&server.uri());
    let core = Arc::new(
        DrasiLib::builder()
            .with_id("ambiguous-core")
            .with_source(source)
            .with_query(
                Query::cypher(QUERY)
                    .query(launch_query_str())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(reaction)
            .with_state_store_provider(store.clone())
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;

    let version = content_version_of(Some(ISSUE_BODY));
    insert_row(
        &handle,
        "issue-7",
        "route-7",
        "resp-7",
        7,
        "gpt-5",
        None,
        REPOSITORY,
        &version,
        "In Progress",
    )
    .await;

    wait_until(
        || async {
            async {
                load(store.as_ref(), REACTION, "route-7", "resp-7", 1)
                    .await
                    .unwrap()
                    .map(|r| r.status == ExecutionStatus::Ambiguous)
                    .unwrap_or(false)
            }
            .await
        },
        5000,
    )
    .await;

    // Never blindly retried creation.
    assert_eq!(
        mock_github::count_create_task_requests(&server, OWNER, REPO).await,
        0
    );
    // Confirm reconciliation actually ran (not just that status ended up
    // Ambiguous by some other path): the specific "candidate tasks matched"
    // wording only comes from the `ReconciliationOutcome::Ambiguous` arm in
    // `reconcile_and_resume`.
    let record = load(store.as_ref(), REACTION, "route-7", "resp-7", 1)
        .await
        .unwrap()
        .unwrap();
    assert!(
        record
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("0 candidate tasks matched executionId"),
        "expected reconciliation's ambiguous-match error, got {:?}",
        record.last_error
    );
    // The reaction stopped for manual/automatic intervention (Strict policy).
    wait_until(
        || async {
            async {
                matches!(
                    core.get_reaction_status(REACTION).await,
                    Ok(ComponentStatus::Error)
                )
            }
            .await
        },
        5000,
    )
    .await;

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 8. GraphQL errors: an HTTP 200 response carrying `errors` is treated as
//    a failure, both for the Project-status preflight query and for the
//    `addComment` mutation.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn graphql_errors_on_project_status_are_treated_as_failure() {
    let server = MockServer::start().await;
    mock_github::mount_issue(&server, OWNER, REPO, 8, "open", ISSUE_BODY, "I_issue-8").await;
    mock_github::mount_contents(&server, OWNER, REPO, PROFILE_SHA).await;
    mock_github::mount_project_status_graphql_error(&server, "Something went wrong").await;

    let (source, handle) = make_source();
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let reaction = build_reaction(&server.uri());

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("graphql-error-core")
            .with_source(source)
            .with_query(
                Query::cypher(QUERY)
                    .query(launch_query_str())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(reaction)
            .with_state_store_provider(store.clone())
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;

    let version = content_version_of(Some(ISSUE_BODY));
    insert_row(
        &handle,
        "issue-8",
        "route-8",
        "resp-8",
        8,
        "gpt-5",
        None,
        REPOSITORY,
        &version,
        "In Progress",
    )
    .await;

    // Treated as a transient preflight failure: the reaction stops (Strict)
    // without creating a task, rather than proceeding past the GraphQL error.
    wait_until(
        || async {
            async {
                matches!(
                    core.get_reaction_status(REACTION).await,
                    Ok(ComponentStatus::Error)
                )
            }
            .await
        },
        5000,
    )
    .await;
    assert_eq!(
        mock_github::count_create_task_requests(&server, OWNER, REPO).await,
        0
    );

    core.stop().await.expect("stop core");
}

#[tokio::test]
#[ignore]
async fn graphql_errors_on_add_comment_are_treated_as_failure() {
    let server = MockServer::start().await;
    mount_happy_path_preflight(&server, 9, "issue-9").await;
    mock_github::mount_create_task_success(
        &server,
        OWNER,
        REPO,
        "task-9",
        "https://github.com/tasks/9",
    )
    .await;
    mock_github::mount_add_comment_graphql_error(&server, "subjectId is not commentable").await;

    let (source, handle) = make_source();
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let reaction = build_reaction(&server.uri());

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("comment-graphql-error-core")
            .with_source(source)
            .with_query(
                Query::cypher(QUERY)
                    .query(launch_query_str())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(reaction)
            .with_state_store_provider(store.clone())
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;

    let version = content_version_of(Some(ISSUE_BODY));
    insert_row(
        &handle,
        "issue-9",
        "route-9",
        "resp-9",
        9,
        "gpt-5",
        None,
        REPOSITORY,
        &version,
        "In Progress",
    )
    .await;

    // The task IS created (recorded Started) but the comment failure stops
    // the reaction before comment_posted is set — the task is never
    // recreated on any subsequent attempt.
    wait_until(
        || async {
            async {
                load(store.as_ref(), REACTION, "route-9", "resp-9", 1)
                    .await
                    .unwrap()
                    .map(|r| r.task_id.is_some())
                    .unwrap_or(false)
            }
            .await
        },
        5000,
    )
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let record = load(store.as_ref(), REACTION, "route-9", "resp-9", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.status, ExecutionStatus::Started);
    assert!(
        !record.comment_posted,
        "comment must not be marked posted on GraphQL error"
    );
    assert_eq!(
        mock_github::count_create_task_requests(&server, OWNER, REPO).await,
        1
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 9. Secret redaction: the reaction sends the correct bearer token to
//    GitHub (proving auth actually works) while never exposing it via
//    `Debug`. Full redaction-logic coverage lives in `config.rs` /
//    `redact.rs` unit tests; this asserts the wiring end to end.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn token_is_sent_to_github_but_never_exposed_via_debug() {
    let server = MockServer::start().await;
    mount_happy_path_preflight(&server, 10, "issue-10").await;
    mock_github::mount_create_task_success(
        &server,
        OWNER,
        REPO,
        "task-10",
        "https://github.com/tasks/10",
    )
    .await;
    mock_github::mount_add_comment_success(&server).await;

    let (source, handle) = make_source();
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let reaction = build_reaction(&server.uri());

    // `properties()` is the framework's config-persistence hook and is
    // *intentionally* lossless (it must include secrets so the reaction can
    // be recreated identically on restart — see `Reaction::properties()`
    // docs). It is not a log-safe or display surface. Redaction instead
    // applies to `Debug`/log output, covered by
    // `CopilotAgentTaskReactionConfig`'s and `GitHubClient`'s `Debug` impls
    // (unit-tested in `config.rs` / `github.rs`); this test additionally
    // confirms the token still reaches GitHub correctly (below).
    assert_eq!(
        reaction.properties().get("token"),
        Some(&serde_json::Value::String(
            "ghp_test_token_do_not_log".to_string()
        )),
        "properties() must retain the token for lossless config persistence"
    );

    // `Debug`/log-safe surfaces, in contrast, must redact it.
    let github_config_debug = format!(
        "{:?}",
        drasi_reaction_copilot_agent_task::github::GitHubConfig {
            api_base_url: server.uri(),
            graphql_url: format!("{}/graphql", server.uri()),
            agent_tasks_api_version: drasi_reaction_copilot_agent_task::AGENT_TASKS_API_VERSION
                .to_string(),
            token: "ghp_test_token_do_not_log".to_string(),
            request_timeout_ms: 1000,
        }
    );
    assert!(!github_config_debug.contains("ghp_test_token_do_not_log"));
    assert!(github_config_debug.contains("[REDACTED]"));

    let core = Arc::new(
        DrasiLib::builder()
            .with_id("redaction-core")
            .with_source(source)
            .with_query(
                Query::cypher(QUERY)
                    .query(launch_query_str())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(reaction)
            .with_state_store_provider(store.clone())
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;

    let version = content_version_of(Some(ISSUE_BODY));
    insert_row(
        &handle,
        "issue-10",
        "route-10",
        "resp-10",
        10,
        "gpt-5",
        None,
        REPOSITORY,
        &version,
        "In Progress",
    )
    .await;

    wait_until(
        || async { mock_github::count_create_task_requests(&server, OWNER, REPO).await == 1 },
        5000,
    )
    .await;

    // The mock server actually received the bearer token (auth wiring
    // works) even though it never appears in any Debug/log-safe output.
    let received = server.received_requests().await.unwrap_or_default();
    let saw_bearer = received.iter().any(|r| {
        r.headers
            .get("Authorization")
            .map(|v| v.to_str().unwrap_or_default() == "Bearer ghp_test_token_do_not_log")
            .unwrap_or(false)
    });
    assert!(
        saw_bearer,
        "expected at least one request with the bearer token"
    );

    core.stop().await.expect("stop core");
}
