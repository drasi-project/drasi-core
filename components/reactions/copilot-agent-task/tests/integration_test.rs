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
//! This is a **protocol-target** reaction: it calls GitHub's REST and GraphQL
//! APIs directly, so a stateful local `wiremock` server stands in for GitHub and
//! a durable in-memory state store stands in for the persistent store. A restart
//! is modelled by building a second core over the same state store and the same
//! GitHub state, which is exactly what the durability contract promises.
//!
//! Run with:
//! `cargo test -p drasi-reaction-copilot-agent-task --test integration_test -- --ignored --nocapture`

mod durable_memory_store;
mod mock_github;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_copilot_agent_task::github::{GitHubClient, GitHubConfig};
use drasi_reaction_copilot_agent_task::ids::execution_id;
use drasi_reaction_copilot_agent_task::prompt::build_prompt;
use drasi_reaction_copilot_agent_task::row::LaunchRow;
use drasi_reaction_copilot_agent_task::state::{
    create_record_if_absent, load_record, ExecutionRecord, ExecutionStatus,
};
use drasi_reaction_copilot_agent_task::CopilotAgentTaskReaction;
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use drasi_workgraph_common::comment::render_comment;
use drasi_workgraph_common::event::{
    AssignedResponsibilityType, ExecutionStartedPayload, ProfileRef, ResponsibilityAssignedPayload,
    WorkGraphEvent, WorkGraphEventPayload, WorkGraphEventType,
};
use drasi_workgraph_common::ids::{body_digest, event_id, run_id};
use drasi_workgraph_common::summary::{summary_for, SubjectRef};
use durable_memory_store::DurableMemoryStateStoreProvider;
use mock_github::{
    GithubState, MockAuthor, ISSUE_NODE_ID, ISSUE_NUMBER, LAUNCHER_AUTHOR_DATABASE_ID,
    LAUNCHER_AUTHOR_TYPE, PROFILE_BLOB_SHA, PROFILE_NAME, PROJECT_ITEM_NODE_ID, PROJECT_NODE_ID,
    REPOSITORY, STATUS_FIELD_NODE_ID, TRUSTED_AUTHOR_DATABASE_ID, TRUSTED_AUTHOR_TYPE,
};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SOURCE: &str = "copilot-source";
const QUERY: &str = "launch-workgraph-tasks";
const REACTION: &str = "copilot-agent-task";
const SUBJECT_NODE_ID: &str = ISSUE_NODE_ID;
const SUBJECT_NUMBER: u64 = ISSUE_NUMBER;
const ISSUE_BODY: &str = "Please validate this issue.\n\nworkgraph:validate\n";
const REQUESTED_MODEL: &str = "gpt-5.6-sol";
const FALLBACK_MODEL: &str = "gpt-5.4";
const BASE_REF: &str = "main";
const TEST_TOKEN: &str = "ghp_test_token_do_not_log";
const FIRST_TASK_ID: &str = "task-1";
const WARMUP: Duration = Duration::from_millis(150);

fn make_source() -> (ApplicationSource, ApplicationSourceHandle) {
    let config = ApplicationSourceConfig {
        properties: HashMap::new(),
        durability: None,
    };
    ApplicationSource::new(SOURCE, config).expect("create application source")
}

fn query_text() -> &'static str {
    "MATCH (r:LaunchCandidate) RETURN \
     r.repository AS repository, r.subjectNumber AS subjectNumber, \
     r.subjectNodeId AS subjectNodeId, r.projectNodeId AS projectNodeId, \
     r.projectItemNodeId AS projectItemNodeId, r.runId AS runId, \
     r.requestedModel AS requestedModel, r.fallbackModel AS fallbackModel, \
     r.baseRef AS baseRef"
}

fn build_reaction(server_uri: &str) -> CopilotAgentTaskReaction {
    CopilotAgentTaskReaction::builder(REACTION)
        .with_query(QUERY)
        .with_github_api_base_url(server_uri.to_string())
        .with_github_graphql_url(format!("{server_uri}/graphql"))
        .with_token(TEST_TOKEN)
        .with_allowed_repositories(vec![REPOSITORY.to_string()])
        .with_allowed_profiles(vec![PROFILE_NAME.to_string()])
        .with_allowed_models(vec![
            REQUESTED_MODEL.to_string(),
            FALLBACK_MODEL.to_string(),
        ])
        .with_trusted_assignment_author_database_id(TRUSTED_AUTHOR_DATABASE_ID)
        .with_trusted_assignment_author_type(TRUSTED_AUTHOR_TYPE)
        .with_trusted_execution_author_database_id(LAUNCHER_AUTHOR_DATABASE_ID)
        .with_trusted_execution_author_type(LAUNCHER_AUTHOR_TYPE)
        .with_expected_project_status_field_node_id(STATUS_FIELD_NODE_ID)
        .with_request_timeout_ms(500)
        .build()
        .expect("reaction builds")
}

async fn start_core(
    server_uri: &str,
    store: Arc<dyn StateStoreProvider>,
    core_id: &str,
) -> (Arc<DrasiLib>, ApplicationSourceHandle) {
    let (source, handle) = make_source();
    let core = Arc::new(
        DrasiLib::builder()
            .with_id(core_id)
            .with_source(source)
            .with_query(
                Query::cypher(QUERY)
                    .query(query_text())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(build_reaction(server_uri))
            .with_state_store_provider(store)
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;
    (core, handle)
}

async fn insert_launch_row(
    handle: &ApplicationSourceHandle,
    node_id: &str,
    run_id_value: &str,
    fallback: Option<&str>,
) {
    let mut builder = PropertyMapBuilder::new()
        .with_string("repository", REPOSITORY)
        .with_integer("subjectNumber", SUBJECT_NUMBER as i64)
        .with_string("subjectNodeId", SUBJECT_NODE_ID)
        .with_string("projectNodeId", PROJECT_NODE_ID)
        .with_string("projectItemNodeId", PROJECT_ITEM_NODE_ID)
        .with_string("runId", run_id_value)
        .with_string("requestedModel", REQUESTED_MODEL)
        .with_string("baseRef", BASE_REF);
    if let Some(fallback) = fallback {
        builder = builder.with_string("fallbackModel", fallback);
    }
    handle
        .send_node_insert(node_id, vec!["LaunchCandidate"], builder.build())
        .await
        .expect("send node insert");
}

async fn wait_until<F, Fut>(mut condition: F, max_ms: u64) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + Duration::from_millis(max_ms);
    loop {
        if condition().await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// The deterministic run ID the reaction derives for `body`.
fn run_id_for(body: &str) -> String {
    run_id(
        PROJECT_ITEM_NODE_ID,
        SUBJECT_NODE_ID,
        &body_digest(Some(body)),
    )
    .as_str()
    .to_string()
}

/// The stable execution ID for the run derived from `body`.
fn execution_for(body: &str) -> String {
    execution_id(&run_id_for(body)).as_str().to_string()
}

/// A trusted `ResponsibilityAssigned` comment body that pins `profile@sha`.
fn assignment_body_with_profile(body: &str, profile: &str, sha: &str) -> String {
    let digest = body_digest(Some(body));
    let event = WorkGraphEvent::new(
        run_id(PROJECT_ITEM_NODE_ID, SUBJECT_NODE_ID, &digest),
        PROJECT_ITEM_NODE_ID,
        SUBJECT_NODE_ID,
        WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
            responsibility_type: AssignedResponsibilityType::IssueValidation,
            profile_ref: ProfileRef::new(profile, sha).expect("profile ref"),
            content_digest: digest,
        }),
    )
    .expect("assignment event");
    let summary = summary_for(
        &event,
        SubjectRef {
            repository: REPOSITORY,
            number: SUBJECT_NUMBER,
        },
    );
    render_comment(&event, &summary).expect("render assignment")
}

/// The canonical trusted assignment for the sample issue.
fn assignment_body(body: &str) -> String {
    assignment_body_with_profile(body, PROFILE_NAME, PROFILE_BLOB_SHA)
}

/// The exact `ExecutionStarted` comment body the reaction must produce for
/// `body` once `task_id` is created — computed independently of the reaction.
fn execution_started_body(body: &str, task_id: &str) -> String {
    let digest = body_digest(Some(body));
    let run = run_id(PROJECT_ITEM_NODE_ID, SUBJECT_NODE_ID, &digest);
    let execution = execution_id(run.as_str());
    let event = WorkGraphEvent::new(
        run,
        PROJECT_ITEM_NODE_ID,
        SUBJECT_NODE_ID,
        WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
            execution_id: execution,
            task_id: task_id.to_string(),
        }),
    )
    .expect("execution started event");
    let summary = summary_for(
        &event,
        SubjectRef {
            repository: REPOSITORY,
            number: SUBJECT_NUMBER,
        },
    );
    render_comment(&event, &summary).expect("render execution started")
}

async fn load(store: &Arc<dyn StateStoreProvider>, body: &str) -> Option<ExecutionRecord> {
    load_record(store.clone(), REACTION, &run_id_for(body))
        .await
        .expect("load execution record")
        .map(|persisted| persisted.record)
}

/// The reaction's lifecycle status, as the core reports it.
///
/// A hard halt (fail-closed) drives the reaction to `Error`; a permanent,
/// skippable rejection leaves it `Running`. Tests that require a halt assert on
/// this rather than only on the absence of side effects, which would also hold
/// if the row had simply not been processed yet.
async fn reaction_status(core: &Arc<DrasiLib>) -> ComponentStatus {
    core.snapshot_configuration()
        .await
        .expect("configuration snapshot")
        .reactions
        .iter()
        .find(|reaction| reaction.id == REACTION)
        .expect("the reaction is in the snapshot")
        .status
}

/// Wait for the reaction to fail closed.
async fn wait_for_halt(core: &Arc<DrasiLib>) -> bool {
    wait_until(
        || async { reaction_status(core).await == ComponentStatus::Error },
        5000,
    )
    .await
}

// ---------------------------------------------------------------------
// 1. Happy path: exactly one task and one canonical ExecutionStarted comment.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored
async fn happy_path_creates_one_task_and_one_execution_started_comment() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-happy").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;

    assert!(
        wait_until(|| async { github.create_comment_calls() == 1 }, 5000).await,
        "the ExecutionStarted comment was never posted"
    );

    assert_eq!(github.create_task_calls(), 1, "exactly one task creation");
    assert_eq!(github.task_count(), 1, "exactly one task recorded");
    assert_eq!(
        github.create_comment_calls(),
        1,
        "exactly one comment write"
    );

    // The reaction's comment must be byte-identical to an independently rendered
    // canonical ExecutionStarted event.
    let bodies = github.comment_bodies();
    assert_eq!(bodies.len(), 2, "seeded assignment plus the new comment");
    assert_eq!(
        bodies[1],
        execution_started_body(ISSUE_BODY, FIRST_TASK_ID),
        "the posted comment must be the canonical WorkGraphEvent/v1 body"
    );

    // The reporter prompt must carry only subjectNumber and executionId.
    let prompts = github.task_prompts();
    assert_eq!(prompts.len(), 1);
    let prompt = &prompts[0];
    assert!(
        prompt.contains(&SUBJECT_NUMBER.to_string()),
        "carries the number"
    );
    assert!(
        prompt.contains(&execution_for(ISSUE_BODY)),
        "carries the execution id"
    );
    for forbidden in [
        PROJECT_ITEM_NODE_ID,
        PROJECT_NODE_ID,
        SUBJECT_NODE_ID,
        "run:",
        "routeId",
        "responsibilityId",
        "profileRef",
        "AwaitingRouting",
    ] {
        assert!(
            !prompt.contains(forbidden),
            "prompt must not carry the value '{forbidden}'"
        );
    }

    let record = load(&store, ISSUE_BODY).await.expect("record exists");
    assert!(record.is_complete());
    assert_eq!(record.task_id.as_deref(), Some(FIRST_TASK_ID));
    assert_eq!(record.model_used.as_deref(), Some(REQUESTED_MODEL));
    assert!(!record.used_fallback);
    assert!(record.comment_node_id.is_some());

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 2. Duplicate delivery of the same row must not write twice.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn duplicate_delivery_creates_one_task_and_one_comment() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-duplicate").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;
    assert!(
        wait_until(|| async { github.create_comment_calls() == 1 }, 5000).await,
        "the first launch never completed"
    );

    // A second, distinct row for the same run identity: the durable record must
    // suppress every side effect.
    insert_launch_row(&handle, "candidate-2", &run_id_for(ISSUE_BODY), None).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(github.create_task_calls(), 1, "no second task");
    assert_eq!(github.create_comment_calls(), 1, "no second comment");
    assert_eq!(github.task_count(), 1);
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 3. A current body whose digest no longer matches runId: zero side effects.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn stale_body_digest_yields_zero_side_effects() {
    const EDITED: &str = "This issue body was edited after the assignment.\n";
    let server = MockServer::start().await;
    // GitHub now serves an edited body, but the row still nominates the run
    // derived from the original body.
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(EDITED)).await;
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-stale").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(github.create_task_calls(), 0, "no task on a stale digest");
    assert_eq!(
        github.create_comment_calls(),
        0,
        "no comment on a stale digest"
    );
    assert!(
        load(&store, ISSUE_BODY).await.is_none(),
        "no durable record may be created before the digest is bound"
    );
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 4. No trusted assignment comment at all: zero side effects.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn missing_assignment_yields_zero_side_effects() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-noassign").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(github.create_task_calls(), 0);
    assert_eq!(github.create_comment_calls(), 0);
    assert!(load(&store, ISSUE_BODY).await.is_none());
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 5. An assignment authored by an untrusted user ID is never adopted.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn untrusted_author_assignment_is_ignored() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    github.seed_comment(
        &assignment_body(ISSUE_BODY),
        &MockAuthor::untrusted(),
        false,
    );
    // The trusted numeric database ID under the wrong actor type is not the
    // trusted author either.
    github.seed_comment(
        &assignment_body(ISSUE_BODY),
        &MockAuthor::wrong_actor_type(),
        false,
    );
    // Neither is this reaction's own identity: it may not write its own
    // assignment.
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::launcher(), false);
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-untrusted").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(github.create_task_calls(), 0);
    assert_eq!(github.create_comment_calls(), 0);
    assert!(load(&store, ISSUE_BODY).await.is_none());
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 6. An edited trusted assignment is never adopted.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn edited_assignment_is_ignored() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), true);
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-edited").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(github.create_task_calls(), 0);
    assert_eq!(github.create_comment_calls(), 0);
    assert!(load(&store, ISSUE_BODY).await.is_none());
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 7. Two trusted comments claiming one assignment event ID fail closed.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn conflicting_assignments_fail_closed() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    // Same run and event type — therefore the same deterministic event ID — but
    // contradictory profile pins.
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    github.seed_comment(
        &assignment_body_with_profile(ISSUE_BODY, PROFILE_NAME, &"a".repeat(40)),
        &MockAuthor::trusted(),
        false,
    );
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-conflict").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        github.create_task_calls(),
        0,
        "a contradiction must never be resolved by launching"
    );
    assert_eq!(
        github.create_comment_calls(),
        0,
        "no comment on a contradiction"
    );
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 8. Profile blob SHA drift versus the assignment's pin: zero side effects.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn profile_blob_sha_drift_yields_zero_side_effects() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    // The live profile blob moved on since the assignment pinned it.
    github.set_profile_sha(Some(&"b".repeat(40)));
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-drift").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(github.create_task_calls(), 0);
    assert_eq!(github.create_comment_calls(), 0);
    assert!(load(&store, ISSUE_BODY).await.is_none());
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 9. A Project status other than AwaitingValidation: zero side effects.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn wrong_project_status_yields_zero_side_effects() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    github.set_status("AwaitingIssueRiskProfiling");
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-status").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;
    tokio::time::sleep(Duration::from_millis(600)).await;

    assert_eq!(github.create_task_calls(), 0);
    assert_eq!(github.create_comment_calls(), 0);
    assert!(load(&store, ISSUE_BODY).await.is_none());

    // The reaction stays healthy: restoring the status lets a good row succeed.
    github.set_status(mock_github::AWAITING_VALIDATION);
    insert_launch_row(&handle, "candidate-good", &run_id_for(ISSUE_BODY), None).await;
    assert!(
        wait_until(|| async { github.create_comment_calls() == 1 }, 5000).await,
        "a permanent rejection must not wedge the reaction"
    );
    assert_eq!(github.create_task_calls(), 1);
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 10. Exactly-once model fallback on a clearly-unsupported-model 422.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn exactly_once_model_fallback_on_unsupported_model() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    github.add_unsupported_model(REQUESTED_MODEL);
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-fallback").await;
    insert_launch_row(
        &handle,
        "candidate-1",
        &run_id_for(ISSUE_BODY),
        Some(FALLBACK_MODEL),
    )
    .await;

    assert!(
        wait_until(|| async { github.create_comment_calls() == 1 }, 5000).await,
        "the fallback launch never completed"
    );
    assert_eq!(
        github.create_task_calls(),
        2,
        "the requested model is tried once, then the fallback exactly once"
    );
    assert_eq!(github.task_count(), 1, "only the fallback task is recorded");

    let record = load(&store, ISSUE_BODY).await.expect("record exists");
    assert!(record.is_complete());
    assert!(record.used_fallback);
    assert_eq!(record.model_used.as_deref(), Some(FALLBACK_MODEL));
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 11. An unrelated 422 is never retried with the fallback model.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn no_fallback_on_unrelated_422() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    github.force_task_status(
        422,
        serde_json::json!({ "message": "Validation failed: base_ref does not exist" }),
    );
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-unrelated").await;
    insert_launch_row(
        &handle,
        "candidate-1",
        &run_id_for(ISSUE_BODY),
        Some(FALLBACK_MODEL),
    )
    .await;

    assert!(
        wait_until(
            || async {
                load(&store, ISSUE_BODY)
                    .await
                    .is_some_and(|record| record.status == ExecutionStatus::Failed)
            },
            5000
        )
        .await,
        "an unrelated 422 must be recorded as a permanent failure"
    );
    assert_eq!(
        github.create_task_calls(),
        1,
        "an unrelated 422 must not trigger the fallback model"
    );
    assert_eq!(
        github.create_comment_calls(),
        0,
        "a failed task posts no comment"
    );
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 12. An ambiguous task creation is persisted and posts no comment.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn ambiguous_task_creation_persists_and_posts_no_comment() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    // The server records the task but the response never arrives inside the
    // client timeout, so the create outcome is ambiguous.
    github.set_create_task_delay(Some(Duration::from_millis(1500)));

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-ambiguous").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;

    assert!(
        wait_until(
            || async {
                load(&store, ISSUE_BODY)
                    .await
                    .is_some_and(|record| record.status == ExecutionStatus::Ambiguous)
            },
            5000
        )
        .await,
        "the ambiguous outcome was not persisted"
    );
    let record = load(&store, ISSUE_BODY).await.expect("record exists");
    assert!(record.ambiguous);
    assert!(
        record.task_id.is_none(),
        "an unconfirmed task is not durable"
    );
    assert_eq!(github.create_task_calls(), 1);
    assert_eq!(github.task_count(), 1, "the task did land on the server");
    assert_eq!(
        github.create_comment_calls(),
        0,
        "no comment while unconfirmed"
    );
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 13. A restart after that ambiguity adopts the single correlated task and
//     posts exactly one comment.
//
// A restart is modelled the way the durability contract defines it: the durable
// record and GitHub survive, the in-process query outbox does not. That is
// reproduced here by pre-seeding the ambiguous record and the landed task, then
// starting a single fresh core.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn restart_after_ambiguous_adopts_task_and_posts_one_comment() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    // The write the previous process could not confirm did in fact land: a task
    // whose prompt carries this run's execution ID exists on the server.
    let digest = body_digest(Some(ISSUE_BODY));
    let run = run_id(PROJECT_ITEM_NODE_ID, SUBJECT_NODE_ID, &digest);
    let execution = execution_id(run.as_str());
    let started_event_id = event_id(&run, WorkGraphEventType::ExecutionStarted);
    let seeded_task_id = github.seed_task(&build_prompt(SUBJECT_NUMBER, execution.as_str()));

    // ...and the previous process left an ambiguous intent record behind.
    let row = LaunchRow {
        repository: REPOSITORY.to_string(),
        subject_number: SUBJECT_NUMBER,
        subject_node_id: SUBJECT_NODE_ID.to_string(),
        project_node_id: PROJECT_NODE_ID.to_string(),
        project_item_node_id: PROJECT_ITEM_NODE_ID.to_string(),
        run_id: run.as_str().to_string(),
        requested_model: REQUESTED_MODEL.to_string(),
        fallback_model: None,
        base_ref: BASE_REF.to_string(),
    };
    let mut seeded = ExecutionRecord::new(
        run.as_str(),
        started_event_id.as_str(),
        execution.as_str(),
        &row,
        digest.as_str(),
        &format!("{PROFILE_NAME}@{PROFILE_BLOB_SHA}"),
    );
    seeded.set_attempt_model(REQUESTED_MODEL, false);
    seeded.set_ambiguous("create task outcome ambiguous (transport error)");
    create_record_if_absent(store.clone(), REACTION, &seeded)
        .await
        .expect("seed ambiguous record");

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-restart").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;

    assert!(
        wait_until(|| async { github.create_comment_calls() == 1 }, 5000).await,
        "the restart never adopted the task and posted the comment"
    );
    assert_eq!(
        github.create_task_calls(),
        0,
        "the existing task must be adopted, not re-created"
    );
    assert_eq!(github.task_count(), 1, "no duplicate task");

    let record = load(&store, ISSUE_BODY).await.expect("record exists");
    assert!(record.is_complete());
    assert!(!record.ambiguous);
    assert_eq!(record.task_id.as_deref(), Some(seeded_task_id.as_str()));

    // The adopted-task comment is still the canonical body for that task.
    let bodies = github.comment_bodies();
    assert_eq!(
        bodies.last().map(String::as_str),
        Some(execution_started_body(ISSUE_BODY, &seeded_task_id).as_str())
    );
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 14. A pre-existing ExecutionStarted comment that claims this event ID but
//     carries different content is never adopted.
//
// `eventId` hashes the run and the event type only — it does not cover the
// payload — so a lone divergent comment would otherwise be mistaken for this
// reaction's own completed write and the run would be marked complete against
// a task it never launched.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_divergent_preexisting_execution_started_is_never_adopted() {
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    // The task for this execution already exists, so it is adopted and the run
    // reaches the comment step with no task write.
    let digest = body_digest(Some(ISSUE_BODY));
    let run = run_id(PROJECT_ITEM_NODE_ID, SUBJECT_NODE_ID, &digest);
    let execution = execution_id(run.as_str());
    let seeded_task_id = github.seed_task(&build_prompt(SUBJECT_NUMBER, execution.as_str()));

    // A trusted, unedited comment written by *this* reaction's identity that
    // carries the intended ExecutionStarted event ID but names another task.
    let divergent = WorkGraphEvent::new(
        run.clone(),
        PROJECT_ITEM_NODE_ID,
        SUBJECT_NODE_ID,
        WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
            execution_id: execution.clone(),
            task_id: "task-somebody-elses".to_string(),
        }),
    )
    .expect("event");
    let divergent_body = render_comment(
        &divergent,
        &summary_for(
            &divergent,
            SubjectRef {
                repository: REPOSITORY,
                number: SUBJECT_NUMBER,
            },
        ),
    )
    .expect("render");
    assert_ne!(
        divergent_body,
        execution_started_body(ISSUE_BODY, &seeded_task_id),
        "the payloads must differ"
    );
    github.seed_comment(&divergent_body, &MockAuthor::launcher(), false);

    let (core, handle) = start_core(&server.uri(), store.clone(), "copilot-divergent").await;
    insert_launch_row(&handle, "candidate-1", &run_id_for(ISSUE_BODY), None).await;

    assert!(
        wait_for_halt(&core).await,
        "a divergent published event must fail closed, not be skipped; \
         the reaction is still {:?}",
        reaction_status(&core).await
    );
    assert_eq!(
        github.create_comment_calls(),
        0,
        "a divergent published event must halt, not be re-posted"
    );
    assert_eq!(github.create_task_calls(), 0, "the task was adopted");
    assert_eq!(github.task_count(), 1, "no second task may be created");
    assert_eq!(
        github.comment_bodies().len(),
        2,
        "no further comment may be written"
    );

    let record = load(&store, ISSUE_BODY).await.expect("intent is durable");
    assert!(
        record.comment_node_id.is_none(),
        "the divergent comment must never be recorded as ours"
    );
    assert!(!record.is_complete(), "the run must not be marked complete");

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 15. The token is sent to GitHub but redacted in Debug output.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn token_is_sent_to_github_but_redacted_in_debug() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/user"))
        .and(header("authorization", format!("Bearer {TEST_TOKEN}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": LAUNCHER_AUTHOR_DATABASE_ID,
            "login": "launcher"
        })))
        .mount(&server)
        .await;

    let client = GitHubClient::new(GitHubConfig {
        api_base_url: server.uri(),
        graphql_url: format!("{}/graphql", server.uri()),
        agent_tasks_api_version: "2026-03-10".to_string(),
        token: TEST_TOKEN.to_string(),
        request_timeout_ms: 2000,
    })
    .expect("client builds");

    // The `/user` route only answers when the Bearer token is actually sent.
    let id = client
        .authenticated_user_id()
        .await
        .expect("token must be sent so the request is authorized");
    assert_eq!(id, LAUNCHER_AUTHOR_DATABASE_ID.to_string());

    let debug = format!("{client:?}");
    assert!(
        !debug.contains(TEST_TOKEN),
        "Debug output must not leak the token"
    );
    assert!(
        debug.contains("[REDACTED]"),
        "Debug output must mark the token as redacted"
    );
}

/// Keep the shared-state type referenced even when a test subset is filtered out.
#[allow(dead_code)]
fn _assert_state_type(_: &GithubState) {}
