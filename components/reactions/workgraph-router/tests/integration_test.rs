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

//! End-to-end tests for the WorkGraph router reaction.
//!
//! This is a **protocol-target** reaction: it calls GitHub's REST and GraphQL
//! APIs directly, so a stateful local `wiremock` server stands in for GitHub and
//! a durable in-memory state store stands in for the persistent store. Restart
//! is modelled by building a second reaction over the same state store and the
//! same GitHub state, which is exactly what the durability contract promises.
//!
//! Run with:
//! `cargo test -p drasi-reaction-workgraph-router --test integration_test -- --ignored --nocapture`

mod durable_memory_store;
mod mock_github;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_workgraph_router::state::{
    comment_body_hash, create_record_if_absent, load_record, set_open_run, AcceptedCompletion,
    RoutingRecord,
};
use drasi_reaction_workgraph_router::{RoutingCandidate, WorkgraphRouterReaction};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use drasi_workgraph_common::{
    comment::render_comment,
    event::{
        AssignedResponsibilityType, CompletedIssueValidationPayload, ExecutionId,
        ExecutionStartedPayload, ProfileRef, ResponsibilityAssignedPayload, RoutingDecidedPayload,
        RunId, ValidationOutcome, ValidationReasonCode, WorkGraphEvent, WorkGraphEventPayload,
    },
    ids::{body_digest, run_id},
    summary::summary_for,
};
use durable_memory_store::DurableMemoryStateStoreProvider;
use mock_github::{
    GithubState, MockAuthor, FAILED_STATUS, PASSED_STATUS, PROJECT_ITEM_NODE_ID, PROJECT_NODE_ID,
    REPOSITORY, ROUTABLE_STATUS, STATUS_FIELD_NODE_ID, SUBJECT_NUMBER, TRUSTED_AUTHOR_DATABASE_ID,
    TRUSTED_AUTHOR_TYPE,
};
use wiremock::MockServer;

const SOURCE: &str = "router-source";
const QUERY: &str = "route-workgraph-items";
const REACTION: &str = "workgraph-router";
const SUBJECT_NODE_ID: &str = "I_kwDOABCDEF6ABCDE";
const ISSUE_BODY: &str = "Please validate this issue.\n\nworkgraph:validate\n";
const PROFILE_BLOB_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const EXECUTION_SUFFIX: &str = "2f1c9e11-4a9d-4b66-a30d-1b8e7721fa4c";
const TOKEN_ENV: &str = "WORKGRAPH_ROUTER_TEST_TOKEN";
const WARMUP: Duration = Duration::from_millis(150);

fn make_source() -> (ApplicationSource, ApplicationSourceHandle) {
    let config = ApplicationSourceConfig {
        properties: HashMap::new(),
        durability: None,
    };
    ApplicationSource::new(SOURCE, config).expect("create application source")
}

/// The routing query projects one authoritative `CompletedIssueValidation`
/// comment (its body, author, and edited flag) plus the subject issue's
/// `bodyDigest`, exactly as the GitHub Source names those fields.
fn query_text() -> &'static str {
    "MATCH (r:RoutingCandidate) RETURN \
     r.repository AS repository, r.subjectNumber AS subjectNumber, \
     r.subjectNodeId AS subjectNodeId, r.projectNodeId AS projectNodeId, \
     r.projectItemNodeId AS projectItemNodeId, r.projectStatus AS projectStatus, \
     r.bodyDigest AS bodyDigest, r.eventCommentNodeId AS eventCommentNodeId, \
     r.eventBody AS eventBody, r.authorDatabaseId AS authorDatabaseId, \
     r.authorType AS authorType, r.isEdited AS isEdited"
}

fn build_reaction(server_uri: &str, query: &str) -> WorkgraphRouterReaction {
    WorkgraphRouterReaction::builder(REACTION)
        .with_query(query)
        .with_github_rest_url(server_uri.to_string())
        .with_github_graphql_url(format!("{server_uri}/graphql"))
        .with_github_token_env(TOKEN_ENV)
        .with_allowed_repositories(vec![REPOSITORY.to_string()])
        .with_allowed_projects(vec![PROJECT_NODE_ID.to_string()])
        .with_expected_project_status_field_node_id(STATUS_FIELD_NODE_ID)
        .with_trusted_author_database_id(TRUSTED_AUTHOR_DATABASE_ID)
        .with_trusted_author_type(TRUSTED_AUTHOR_TYPE)
        .with_timeout_secs(1)
        .build()
        .expect("reaction builds")
}

async fn start_core(
    server_uri: &str,
    store: Arc<dyn StateStoreProvider>,
    core_id: &str,
) -> (Arc<DrasiLib>, ApplicationSourceHandle) {
    start_core_with_query(server_uri, store, core_id, QUERY).await
}

/// Start a core whose reaction subscribes to `query_id`.
///
/// A second core over the same durable store models a restart of the *router*:
/// its routing records and open-run pointers survive, while the in-process
/// query outbox does not. The query is renamed for that second core because the
/// framework's own reaction checkpoint is keyed by query ID, and a fresh
/// in-memory outbox for an already-checkpointed query is a gap that strict
/// recovery (correctly) refuses. Nothing under test here depends on that
/// framework checkpoint — the durable routing state is what must carry the
/// decision across the restart.
async fn start_core_with_query(
    server_uri: &str,
    store: Arc<dyn StateStoreProvider>,
    core_id: &str,
    query_id: &str,
) -> (Arc<DrasiLib>, ApplicationSourceHandle) {
    let (source, handle) = make_source();
    let core = Arc::new(
        DrasiLib::builder()
            .with_id(core_id)
            .with_source(source)
            .with_query(
                Query::cypher(query_id)
                    .query(query_text())
                    .from_source(SOURCE)
                    .with_outbox_capacity(100)
                    .auto_start(true)
                    .build(),
            )
            .with_reaction(build_reaction(server_uri, query_id))
            .with_state_store_provider(store)
            .build()
            .await
            .expect("build core"),
    );
    core.start().await.expect("start core");
    tokio::time::sleep(WARMUP).await;
    (core, handle)
}

/// Every value one routing row carries, so a test can vary exactly one of them.
struct RowSpec<'a> {
    /// The item status the query observed.
    project_status: &'a str,
    /// The subject issue's `bodyDigest`.
    body_digest: String,
    /// The node ID of the comment the row names as the completion.
    comment_node_id: String,
    /// The comment body the Source projected.
    event_body: String,
    /// The comment's author, as the Source projected it.
    author: MockAuthor,
    /// The comment's `isEdited` flag.
    is_edited: bool,
}

impl<'a> RowSpec<'a> {
    /// A row carrying the completion for `body`, `outcome`, and `execution`.
    fn completion(
        comment_node_id: &str,
        body: &str,
        outcome: ValidationOutcome,
        execution: &str,
    ) -> Self {
        Self {
            project_status: ROUTABLE_STATUS,
            body_digest: body_digest(Some(body)).as_str().to_string(),
            comment_node_id: comment_node_id.to_string(),
            event_body: completion_body(body, outcome, execution),
            author: MockAuthor::trusted(),
            is_edited: false,
        }
    }

    /// The canonical row: the trusted, unedited passing completion.
    fn passing(comment_node_id: &str) -> Self {
        Self::completion(
            comment_node_id,
            ISSUE_BODY,
            ValidationOutcome::Passed,
            EXECUTION_SUFFIX,
        )
    }

    fn with_status(mut self, status: &'a str) -> Self {
        self.project_status = status;
        self
    }

    fn with_author(mut self, author: MockAuthor) -> Self {
        self.author = author;
        self
    }

    fn edited(mut self) -> Self {
        self.is_edited = true;
        self
    }
}

async fn insert_row(handle: &ApplicationSourceHandle, node_id: &str, spec: RowSpec<'_>) {
    let properties = PropertyMapBuilder::new()
        .with_string("repository", REPOSITORY)
        .with_integer("subjectNumber", SUBJECT_NUMBER as i64)
        .with_string("subjectNodeId", SUBJECT_NODE_ID)
        .with_string("projectNodeId", PROJECT_NODE_ID)
        .with_string("projectItemNodeId", PROJECT_ITEM_NODE_ID)
        .with_string("projectStatus", spec.project_status)
        .with_string("bodyDigest", spec.body_digest)
        .with_string("eventCommentNodeId", spec.comment_node_id)
        .with_string("eventBody", spec.event_body)
        .with_integer("authorDatabaseId", spec.author.database_id as i64)
        .with_string("authorType", spec.author.actor_type.as_str())
        .with_bool("isEdited", spec.is_edited)
        .build();
    handle
        .send_node_insert(node_id, vec!["RoutingCandidate"], properties)
        .await
        .expect("send node insert");
}

/// Insert the canonical trusted passing-completion row for `comment_node_id`.
async fn insert_candidate(handle: &ApplicationSourceHandle, node_id: &str, comment_node_id: &str) {
    insert_row(handle, node_id, RowSpec::passing(comment_node_id)).await;
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

fn event_for(body: &str, payload: WorkGraphEventPayload) -> WorkGraphEvent {
    WorkGraphEvent::new(
        run_for(body),
        PROJECT_ITEM_NODE_ID,
        SUBJECT_NODE_ID,
        payload,
    )
    .expect("event")
}

fn body_for(event: &WorkGraphEvent) -> String {
    let summary = summary_for(event);
    render_comment(event, &summary).expect("render")
}

fn assignment_body(body: &str) -> String {
    body_for(&event_for(
        body,
        WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
            responsibility_type: AssignedResponsibilityType::IssueValidation,
            profile_ref: ProfileRef::new("issue-validator", PROFILE_BLOB_SHA).expect("profile"),
            content_digest: body_digest(Some(body)),
        }),
    ))
}

fn started_body(body: &str) -> String {
    let execution_id = ExecutionId::from_run_id(&run_for(body));
    body_for(&event_for(
        body,
        WorkGraphEventPayload::ExecutionStarted(ExecutionStartedPayload {
            execution_id,
            task_id: "agent-task-1234".to_string(),
        }),
    ))
}

fn completion_body(body: &str, outcome: ValidationOutcome, execution_suffix: &str) -> String {
    let reason = match outcome {
        ValidationOutcome::Passed => ValidationReasonCode::RequiredMarkerPresent,
        ValidationOutcome::Failed => ValidationReasonCode::RequiredMarkerMissing,
    };
    let expected_execution = ExecutionId::from_run_id(&run_for(body));
    let canonical = body_for(&event_for(
        body,
        WorkGraphEventPayload::CompletedIssueValidation(CompletedIssueValidationPayload {
            execution_id: expected_execution.clone(),
            outcome,
            reason_code: reason,
        }),
    ));
    if execution_suffix == EXECUTION_SUFFIX {
        canonical
    } else {
        let other_execution =
            ExecutionId::from_run_id(&run_id("PVTI_other", &body_digest(Some(body))));
        canonical.replace(expected_execution.as_str(), other_execution.as_str())
    }
}

/// The exact `RoutingDecided` comment the reaction must produce.
fn expected_decision_body(body: &str, outcome: ValidationOutcome) -> String {
    body_for(&event_for(
        body,
        WorkGraphEventPayload::RoutingDecided(RoutingDecidedPayload::for_outcome(outcome)),
    ))
}

/// Seed the trusted assignment + start + completion chain for one outcome.
fn seed_chain(github: &GithubState, body: &str, outcome: ValidationOutcome) -> String {
    github.seed_comment(&assignment_body(body), &MockAuthor::trusted(), false);
    github.seed_comment(&started_body(body), &MockAuthor::trusted(), false);
    github.seed_comment(
        &completion_body(body, outcome, EXECUTION_SUFFIX),
        &MockAuthor::trusted(),
        false,
    )
}

fn expected_run_id(body: &str) -> String {
    run_for(body).as_str().to_string()
}

fn run_for(body: &str) -> RunId {
    run_id(PROJECT_ITEM_NODE_ID, &body_digest(Some(body)))
}

async fn record(store: &Arc<dyn StateStoreProvider>, body: &str) -> Option<RoutingRecord> {
    load_record(store.clone(), REACTION, &expected_run_id(body))
        .await
        .expect("load record")
        .map(|persisted| persisted.record)
}

/// The row a seeded durable record was created from.
fn candidate(completion_node_id: &str) -> RoutingCandidate {
    RoutingCandidate {
        repository: REPOSITORY.to_string(),
        subject_number: SUBJECT_NUMBER,
        subject_node_id: SUBJECT_NODE_ID.to_string(),
        project_node_id: PROJECT_NODE_ID.to_string(),
        project_item_node_id: PROJECT_ITEM_NODE_ID.to_string(),
        project_status: ROUTABLE_STATUS.to_string(),
        body_digest: body_digest(Some(ISSUE_BODY)).as_str().to_string(),
        event_comment_node_id: completion_node_id.to_string(),
        event_body: completion_body(ISSUE_BODY, ValidationOutcome::Passed, EXECUTION_SUFFIX),
        author_database_id: TRUSTED_AUTHOR_DATABASE_ID,
        author_type: TRUSTED_AUTHOR_TYPE.as_str().to_string(),
        is_edited: false,
    }
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

fn set_token() {
    std::env::set_var(TOKEN_ENV, "ghp_test_token_do_not_log");
}

// ---------------------------------------------------------------------
// 1. A passing validation routes straight to AwaitingIssueRiskProfiling.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored
async fn passing_validation_routes_directly_to_risk_profiling() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-passed").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;

    assert!(
        wait_until(|| async { github.status() == PASSED_STATUS }, 5000).await,
        "project item was never routed; status is '{}'",
        github.status()
    );

    assert_eq!(github.create_comment_calls(), 1, "exactly one comment");
    let bodies = github.comment_bodies();
    assert_eq!(bodies.len(), 4, "three seeded comments plus one decision");
    assert_eq!(
        bodies[3],
        expected_decision_body(ISSUE_BODY, ValidationOutcome::Passed),
        "the comment must be the canonical WorkGraphEvent/v1 RoutingDecided body"
    );
    assert!(
        bodies[3].contains("\"nextResponsibilityType\":\"issue-risk-profiling\""),
        "the next responsibility travels inside the decision payload: {}",
        bodies[3]
    );
    assert_eq!(github.status_mutations(), 1, "exactly one status mutation");

    let record = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(record.is_complete());
    assert_eq!(record.to_status, PASSED_STATUS);
    assert_eq!(record.outcome, "passed");
    assert_eq!(
        record.decision_comment_node_id.as_deref(),
        Some("IC_created4")
    );
    assert_eq!(
        record.accepted_completion.comment_node_id, completion_node_id,
        "the record must pin the physical completion comment it decided from"
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 2. A failing validation routes straight to NeedsMoreInformation.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn failing_validation_routes_directly_to_needs_more_information() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Failed);

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-failed").await;
    insert_row(
        &handle,
        "candidate-1",
        RowSpec::completion(
            &completion_node_id,
            ISSUE_BODY,
            ValidationOutcome::Failed,
            EXECUTION_SUFFIX,
        ),
    )
    .await;

    assert!(
        wait_until(|| async { github.status() == FAILED_STATUS }, 5000).await,
        "project item was never routed; status is '{}'",
        github.status()
    );

    let bodies = github.comment_bodies();
    assert_eq!(
        bodies[3],
        expected_decision_body(ISSUE_BODY, ValidationOutcome::Failed)
    );
    assert!(bodies[3].contains("\"nextResponsibilityType\":\"issue-correction\""));

    let record = record(&store, ISSUE_BODY).await.expect("record exists");
    assert_eq!(record.to_status, FAILED_STATUS);
    assert_eq!(record.outcome, "failed");

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 3. The item never passes through an intermediate routing status.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn routing_never_visits_an_intermediate_status() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-direct").await;

    // Sample the status continuously while routing happens.
    let observed = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sampler_state = github.clone();
    let sampler_out = observed.clone();
    let sampler = tokio::spawn(async move {
        for _ in 0..400 {
            let status = sampler_state.status();
            {
                let mut seen = sampler_out.lock().expect("sample lock");
                if seen.last().map(String::as_str) != Some(status.as_str()) {
                    seen.push(status);
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });

    insert_candidate(&handle, "candidate-1", &completion_node_id).await;
    assert!(
        wait_until(|| async { github.status() == PASSED_STATUS }, 5000).await,
        "project item was never routed"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    sampler.abort();

    let seen = observed.lock().expect("sample lock").clone();
    assert_eq!(
        seen,
        vec![ROUTABLE_STATUS.to_string(), PASSED_STATUS.to_string()],
        "the item must move directly from '{ROUTABLE_STATUS}' to '{PASSED_STATUS}'"
    );
    assert!(
        !seen.iter().any(|status| status == "AwaitingRouting"),
        "AwaitingRouting must not exist"
    );
    assert_eq!(
        github.status_mutations(),
        1,
        "one decision means one status mutation"
    );

    // Exactly one WorkGraph event was written, and it is the decision.
    let bodies = github.comment_bodies();
    assert_eq!(bodies.len(), 4);
    assert_eq!(
        bodies
            .iter()
            .filter(|body| body.contains("\"eventType\":\"RoutingDecided\""))
            .count(),
        1,
        "exactly one RoutingDecided comment"
    );
    assert!(
        !bodies.iter().any(
            |body| body.contains("\"eventType\":\"ResponsibilityAssigned\"")
                && body.contains("issue-risk-profiling")
        ),
        "the router must not post a fifth assignment event"
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 4. Duplicate delivery of the same row must not write twice.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn duplicate_delivery_routes_exactly_once() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-duplicate").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;
    assert!(
        wait_until(|| async { github.status() == PASSED_STATUS }, 5000).await,
        "first delivery never routed"
    );

    // The same logical row arrives again under a different node ID.
    insert_candidate(&handle, "candidate-2", &completion_node_id).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        github.create_comment_calls(),
        1,
        "the second delivery must not post a second decision"
    );
    assert_eq!(github.status_mutations(), 1, "one status mutation only");
    assert_eq!(github.status(), PASSED_STATUS);

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 5. An edited issue body has zero side effects.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_body_edited_since_validation_yields_zero_side_effects() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    // The whole chain was written for the original body...
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);
    // ...but the issue has been edited since.
    github.set_issue_body(Some("Completely rewritten body."));

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-edited-body").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        github.create_comment_calls(),
        0,
        "a stale run must never post a decision"
    );
    assert_eq!(github.status_mutations(), 0, "status must not move");
    assert_eq!(github.status(), ROUTABLE_STATUS);
    assert!(
        record(&store, ISSUE_BODY).await.is_none(),
        "no record for the old run"
    );
    assert!(
        record(&store, "Completely rewritten body.").await.is_none(),
        "no record for the new run either"
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 6. Only trusted, unedited comments can drive a decision.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn untrusted_and_edited_completions_are_never_routed() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    github.seed_comment(&started_body(ISSUE_BODY), &MockAuthor::trusted(), false);

    let completion = completion_body(ISSUE_BODY, ValidationOutcome::Passed, EXECUTION_SUFFIX);
    // A completely different account...
    let untrusted = github.seed_comment(&completion, &MockAuthor::untrusted(), false);
    // ...the trusted numeric database ID under the wrong actor type...
    github.seed_comment(&completion, &MockAuthor::wrong_actor_type(), false);
    // ...and the trusted account, but edited afterwards.
    let edited = github.seed_comment(&completion, &MockAuthor::trusted(), true);

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-untrusted").await;
    // Rows whose own Source author metadata is untrusted, or that report the
    // edit, are refused before any GitHub call...
    insert_row(
        &handle,
        "candidate-untrusted",
        RowSpec::passing(&untrusted).with_author(MockAuthor::untrusted()),
    )
    .await;
    insert_row(
        &handle,
        "candidate-wrong-type",
        RowSpec::passing(&untrusted).with_author(MockAuthor::wrong_actor_type()),
    )
    .await;
    insert_row(
        &handle,
        "candidate-edited",
        RowSpec::passing(&edited).edited(),
    )
    .await;
    // ...and a row that claims those comments are trusted and unedited is still
    // refused, because no trusted, unedited completion exists on the issue.
    insert_candidate(&handle, "candidate-1", &untrusted).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        github.create_comment_calls(),
        0,
        "no decision may be posted"
    );
    assert_eq!(github.status_mutations(), 0, "status must not move");
    assert!(record(&store, ISSUE_BODY).await.is_none(), "no record");

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 7. A renamed login is still the same trusted identity.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_renamed_login_still_routes() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    github.seed_comment(&started_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    let completion_node_id = github.seed_comment(
        &completion_body(ISSUE_BODY, ValidationOutcome::Passed, EXECUTION_SUFFIX),
        // Same node ID, database ID, and actor type; different login.
        &MockAuthor::trusted_renamed(),
        false,
    );

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-renamed").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;

    assert!(
        wait_until(|| async { github.status() == PASSED_STATUS }, 5000).await,
        "a renamed login must not break routing"
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 8. Without a completion (or without a start) nothing happens.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn an_incomplete_chain_yields_zero_side_effects() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    // Assignment and start, but validation has not reported yet.
    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    github.seed_comment(&started_body(ISSUE_BODY), &MockAuthor::trusted(), false);

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-incomplete").await;
    // A well-formed row for a completion that is not on the issue at all.
    insert_candidate(&handle, "candidate-1", "IC_not_posted").await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(github.create_comment_calls(), 0);
    assert_eq!(github.status_mutations(), 0);
    assert_eq!(github.status(), ROUTABLE_STATUS);

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 9. A completion from a different execution is rejected.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_completion_from_another_execution_is_rejected() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    github.seed_comment(&started_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    const OTHER_EXECUTION: &str = "11111111-2222-3333-4444-555555555555";
    let completion_node_id = github.seed_comment(
        &completion_body(ISSUE_BODY, ValidationOutcome::Passed, OTHER_EXECUTION),
        &MockAuthor::trusted(),
        false,
    );

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-exec-mismatch").await;
    insert_row(
        &handle,
        "candidate-1",
        RowSpec::completion(
            &completion_node_id,
            ISSUE_BODY,
            ValidationOutcome::Passed,
            OTHER_EXECUTION,
        ),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(github.create_comment_calls(), 0);
    assert_eq!(github.status_mutations(), 0);

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 10. Two trusted completions claiming one event ID fail closed.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn conflicting_duplicate_completions_fail_closed() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    github.seed_comment(&started_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    // Same run and event type — therefore the same deterministic event ID —
    // but contradictory outcomes.
    let completion_node_id = github.seed_comment(
        &completion_body(ISSUE_BODY, ValidationOutcome::Passed, EXECUTION_SUFFIX),
        &MockAuthor::trusted(),
        false,
    );
    github.seed_comment(
        &completion_body(ISSUE_BODY, ValidationOutcome::Failed, EXECUTION_SUFFIX),
        &MockAuthor::trusted(),
        false,
    );

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-conflict").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        github.create_comment_calls(),
        0,
        "a contradiction must never be resolved by writing"
    );
    assert_eq!(github.status_mutations(), 0, "status must not move");
    assert!(record(&store, ISSUE_BODY).await.is_none(), "no record");

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 11. Byte-identical duplicate completions coalesce to one decision.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn identical_duplicate_completions_coalesce() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    github.seed_comment(&assignment_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    github.seed_comment(&started_body(ISSUE_BODY), &MockAuthor::trusted(), false);
    let first = github.seed_comment(
        &completion_body(ISSUE_BODY, ValidationOutcome::Passed, EXECUTION_SUFFIX),
        &MockAuthor::trusted(),
        false,
    );
    // The reporter retried and the same event landed twice, with a different
    // (non-authoritative) summary line.
    let duplicate = format!(
        "WorkGraphEvent/v1\n\nA differently worded summary\n\n{}",
        completion_body(ISSUE_BODY, ValidationOutcome::Passed, EXECUTION_SUFFIX)
            .split("\n\n")
            .nth(2)
            .expect("json section")
    );
    github.seed_comment(&duplicate, &MockAuthor::trusted(), false);

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-duplicates").await;
    insert_candidate(&handle, "candidate-1", &first).await;

    assert!(
        wait_until(|| async { github.status() == PASSED_STATUS }, 5000).await,
        "identical duplicates must coalesce, not block"
    );
    assert_eq!(github.create_comment_calls(), 1, "one decision only");

    let record = record(&store, ISSUE_BODY).await.expect("record exists");
    assert_eq!(
        record.accepted_completion.comment_node_id, first,
        "the earliest physical completion is the accepted one"
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 12. A restart after an ambiguous write adopts the existing comment.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn restart_after_ambiguous_write_adopts_the_existing_comment() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);

    // The write the previous process could not confirm did in fact land.
    let decision_node_id = github.seed_comment(
        &expected_decision_body(ISSUE_BODY, ValidationOutcome::Passed),
        &MockAuthor::trusted(),
        false,
    );

    // ...and the previous process left an ambiguous intent record behind.
    let decision = RoutingDecidedPayload::for_outcome(ValidationOutcome::Passed);
    let event = event_for(
        ISSUE_BODY,
        WorkGraphEventPayload::RoutingDecided(decision.clone()),
    );
    let mut seeded = RoutingRecord::new(
        &expected_run_id(ISSUE_BODY),
        event.event_id.as_str(),
        &candidate(&completion_node_id),
        body_digest(Some(ISSUE_BODY)).as_str(),
        AcceptedCompletion {
            comment_node_id: completion_node_id.clone(),
            body_hash: comment_body_hash(&completion_body(
                ISSUE_BODY,
                ValidationOutcome::Passed,
                EXECUTION_SUFFIX,
            )),
        },
        "passed",
        PASSED_STATUS,
        &event.to_canonical_json(),
    );
    seeded.set_error("create comment request failed: operation timed out", true);
    create_record_if_absent(store.clone(), REACTION, &seeded)
        .await
        .expect("seed ambiguous record");

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-restart").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;

    assert!(
        wait_until(|| async { github.status() == PASSED_STATUS }, 5000).await,
        "restart never completed the routing"
    );
    assert_eq!(
        github.create_comment_calls(),
        0,
        "the existing decision comment must be adopted, not re-posted"
    );
    assert_eq!(github.comment_bodies().len(), 4, "no duplicate comment");

    let record = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(record.is_complete());
    assert!(!record.ambiguous);
    assert_eq!(
        record.decision_comment_node_id.as_deref(),
        Some(decision_node_id.as_str())
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 13. A completion edited after acceptance stops a resumed decision.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_completion_edited_after_acceptance_halts_the_resumed_run() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);

    // A previous attempt accepted the completion and recorded its body hash,
    // but had not yet posted the decision.
    let decision = RoutingDecidedPayload::for_outcome(ValidationOutcome::Passed);
    let event = event_for(
        ISSUE_BODY,
        WorkGraphEventPayload::RoutingDecided(decision.clone()),
    );
    let seeded = RoutingRecord::new(
        &expected_run_id(ISSUE_BODY),
        event.event_id.as_str(),
        &candidate(&completion_node_id),
        body_digest(Some(ISSUE_BODY)).as_str(),
        AcceptedCompletion {
            comment_node_id: completion_node_id.clone(),
            body_hash: comment_body_hash("a completely different accepted body"),
        },
        "passed",
        PASSED_STATUS,
        &event.to_canonical_json(),
    );
    create_record_if_absent(store.clone(), REACTION, &seeded)
        .await
        .expect("seed record");

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-edited-completion").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        github.create_comment_calls(),
        0,
        "an edited completion must never be routed"
    );
    assert_eq!(github.status_mutations(), 0, "status must not move");
    assert_eq!(github.status(), ROUTABLE_STATUS);

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 14. Stale and mis-bound rows have zero side effects.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn stale_and_misbound_rows_have_zero_side_effects() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-stale").await;

    // A row claiming a status the item does not hold.
    insert_row(
        &handle,
        "candidate-stale-status",
        RowSpec::passing(&completion_node_id).with_status("Triage"),
    )
    .await;
    // A row whose subject node does not match what GitHub reports.
    github.set_issue_node_id("I_somethingelse");
    insert_candidate(&handle, "candidate-misbound", &completion_node_id).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(github.create_comment_calls(), 0, "no comment may be posted");
    assert_eq!(github.status_mutations(), 0, "status must not move");
    assert_eq!(github.status(), ROUTABLE_STATUS);

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 15. An ambiguous comment write is persisted before the status moves.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn ambiguous_comment_write_is_persisted_and_halts_before_status() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);

    // The server accepts the comment but the client times out waiting.
    github.set_create_comment_delay(Some(Duration::from_millis(2500)));

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-ambiguous").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;

    assert!(
        wait_until(
            || async {
                record(&store, ISSUE_BODY)
                    .await
                    .is_some_and(|record| record.ambiguous)
            },
            8000
        )
        .await,
        "the ambiguous write must be persisted"
    );

    let record = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(record.ambiguous, "ambiguity must be durable");
    assert!(record.last_error.is_some(), "the error must be recorded");
    assert!(record.decision_comment_node_id.is_none());
    assert!(!record.status_applied);
    assert_eq!(
        github.status(),
        ROUTABLE_STATUS,
        "the status must not move while the comment outcome is unknown"
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 16. A silently rewritten completion is not routed on a fresh run.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_rewritten_completion_body_is_not_routed() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);

    // The comment text no longer parses as the completion it claimed to be.
    github.silently_rewrite_comment(
        &completion_node_id,
        "WorkGraphEvent/v1\n\nstill looks official\n\nnot json at all",
    );

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-rewritten").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(github.create_comment_calls(), 0);
    assert_eq!(github.status_mutations(), 0);
    assert_eq!(github.status(), ROUTABLE_STATUS);

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 17. A completion whose body changed only in its non-authoritative summary
//     still halts a resumed decision.
//
// GitHub does not flag this as an edit (`updated_at` is untouched), and the
// event JSON is byte-identical, so neither the edit check nor duplicate
// coalescing would notice it. Only the persisted hash of the exact accepted
// body does.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_silently_resummarised_completion_halts_the_resumed_run() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);
    let accepted_body = completion_body(ISSUE_BODY, ValidationOutcome::Passed, EXECUTION_SUFFIX);

    // A previous attempt accepted that exact body and recorded its hash.
    let event = event_for(
        ISSUE_BODY,
        WorkGraphEventPayload::RoutingDecided(RoutingDecidedPayload::for_outcome(
            ValidationOutcome::Passed,
        )),
    );
    let seeded = RoutingRecord::new(
        &expected_run_id(ISSUE_BODY),
        event.event_id.as_str(),
        &candidate(&completion_node_id),
        body_digest(Some(ISSUE_BODY)).as_str(),
        AcceptedCompletion {
            comment_node_id: completion_node_id.clone(),
            body_hash: comment_body_hash(&accepted_body),
        },
        "passed",
        PASSED_STATUS,
        &event.to_canonical_json(),
    );
    create_record_if_absent(store.clone(), REACTION, &seeded)
        .await
        .expect("seed record");

    // Same event JSON, different summary line, no edit reported by GitHub.
    let mut sections = accepted_body.splitn(3, "\n\n");
    let marker = sections.next().expect("marker");
    let _original_summary = sections.next().expect("summary");
    let json = sections.next().expect("json");
    github.silently_rewrite_comment(
        &completion_node_id,
        &format!("{marker}\n\nQuietly reworded summary\n\n{json}"),
    );

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-resummarised").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        github.create_comment_calls(),
        0,
        "a completion body that changed since acceptance must not be routed"
    );
    assert_eq!(github.status_mutations(), 0, "status must not move");
    assert_eq!(github.status(), ROUTABLE_STATUS);

    let record = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(
        record.decision_comment_node_id.is_none(),
        "no decision may be recorded"
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 18. A single pre-existing RoutingDecided comment that claims this run's
//     event ID but carries a different decision is never adopted.
//
// `eventId` hashes the run and the event type only — it does not cover the
// payload — so a lone divergent decision would otherwise be adopted and the
// item moved to a status this run never decided.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_divergent_preexisting_decision_is_never_adopted() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);

    // Trusted, unedited, and carrying the exact event ID this run will decide —
    // but the opposite decision.
    let intended = expected_decision_body(ISSUE_BODY, ValidationOutcome::Passed);
    let divergent = expected_decision_body(ISSUE_BODY, ValidationOutcome::Failed);
    assert_ne!(intended, divergent, "the payloads must differ");
    github.seed_comment(&divergent, &MockAuthor::trusted(), false);

    let (core, handle) = start_core(&server.uri(), store.clone(), "router-divergent").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;

    assert!(
        wait_for_halt(&core).await,
        "a divergent published decision must fail closed, not be skipped; \
         the reaction is still {:?}",
        reaction_status(&core).await
    );
    assert_eq!(
        github.create_comment_calls(),
        0,
        "a divergent published decision must halt, not be re-posted"
    );
    assert_eq!(
        github.comment_bodies().len(),
        4,
        "no further comment may be written"
    );
    assert_eq!(github.status_mutations(), 0, "no status mutation at all");
    assert_eq!(
        github.status(),
        ROUTABLE_STATUS,
        "the status must not drift after a failed adoption"
    );

    let record = record(&store, ISSUE_BODY).await.expect("intent is durable");
    assert!(
        record.decision_comment_node_id.is_none(),
        "the divergent comment must never be recorded as ours"
    );
    assert!(!record.status_applied, "the status step must not run");

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 19. A status update that fails after the decision is published is finished
//     from durable state on replay — even though the issue body changed in
//     the meantime — and is applied exactly once.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_status_failure_after_publication_is_finished_from_durable_state() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);

    // The decision comment lands, then the status mutation fails transiently.
    github.fail_next_status_mutations(1);

    let (first, handle) = start_core(&server.uri(), store.clone(), "router-poststatus-1").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;
    assert!(
        wait_until(
            || async {
                record(&store, ISSUE_BODY)
                    .await
                    .is_some_and(|record| record.ambiguous)
            },
            8000
        )
        .await,
        "the failed status write must be persisted as ambiguous"
    );
    let published = record(&store, ISSUE_BODY).await.expect("record exists");
    assert_eq!(
        github.create_comment_calls(),
        1,
        "the decision was published"
    );
    assert!(
        published.decision_comment_node_id.is_some(),
        "publication must be durable before the status move"
    );
    assert!(!published.status_applied);
    assert_eq!(github.status(), ROUTABLE_STATUS, "the status did not move");
    first.stop().await.expect("stop first core");

    // The issue body is edited before the replay: a fresh derivation would now
    // produce a different runId with no chain at all, which must NOT strand the
    // decision that is already visible in the thread.
    github.set_issue_body(Some("Rewritten while the router was down.\n"));

    let (second, handle) = start_core_with_query(
        &server.uri(),
        store.clone(),
        "router-poststatus-2",
        "route-workgraph-items-after-restart",
    )
    .await;
    insert_candidate(&handle, "candidate-replay", &completion_node_id).await;

    assert!(
        wait_until(|| async { github.status() == PASSED_STATUS }, 5000).await,
        "the replay never applied the persisted decision; status is '{}'",
        github.status()
    );
    assert_eq!(
        github.create_comment_calls(),
        1,
        "the replay must not publish a second decision"
    );
    assert_eq!(
        github.status_mutations(),
        1,
        "the persisted status move must be applied exactly once"
    );

    let finished = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(finished.is_complete());
    assert!(!finished.ambiguous);
    assert_eq!(finished.to_status, PASSED_STATUS);
    assert_eq!(
        finished.decision_comment_node_id, published.decision_comment_node_id,
        "the replay must finish the decision it published, not a new one"
    );

    // Delivering the row yet again is a no-op: the run is complete. Wait for
    // the reaction to actually read GitHub again, so "no further writes" is a
    // statement about a processed row and not about an unprocessed one.
    let reads_before = github.issue_reads();
    insert_candidate(&handle, "candidate-replay-2", &completion_node_id).await;
    assert!(
        wait_until(|| async { github.issue_reads() > reads_before }, 5000).await,
        "the repeated delivery was never processed"
    );
    assert_eq!(github.status_mutations(), 1, "still exactly once");
    assert_eq!(github.create_comment_calls(), 1, "still one comment");
    assert_eq!(
        reaction_status(&second).await,
        ComponentStatus::Running,
        "a completed run must not wedge the reaction"
    );

    second.stop().await.expect("stop second core");
}

// ---------------------------------------------------------------------
// 20. A published decision comment that is edited, deleted, or replaced by a
//     different event halts the resumed run with zero side effects.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_tampered_published_decision_halts_the_resumed_run() {
    #[derive(Clone, Copy)]
    enum Tamper {
        Edited,
        Deleted,
        Divergent,
    }

    async fn run_case(case: Tamper, core_id: &str) {
        set_token();
        let server = MockServer::start().await;
        let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
        let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
        let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);

        // A previous attempt published the decision and recorded it, but had
        // not applied the status yet.
        let decision_node_id = github.seed_comment(
            &expected_decision_body(ISSUE_BODY, ValidationOutcome::Passed),
            &MockAuthor::trusted(),
            false,
        );
        let event = event_for(
            ISSUE_BODY,
            WorkGraphEventPayload::RoutingDecided(RoutingDecidedPayload::for_outcome(
                ValidationOutcome::Passed,
            )),
        );
        let mut seeded = RoutingRecord::new(
            &expected_run_id(ISSUE_BODY),
            event.event_id.as_str(),
            &candidate(&completion_node_id),
            body_digest(Some(ISSUE_BODY)).as_str(),
            AcceptedCompletion {
                comment_node_id: completion_node_id.clone(),
                body_hash: comment_body_hash(&completion_body(
                    ISSUE_BODY,
                    ValidationOutcome::Passed,
                    EXECUTION_SUFFIX,
                )),
            },
            "passed",
            PASSED_STATUS,
            &event.to_canonical_json(),
        );
        seeded.set_decision_comment(decision_node_id.clone());
        create_record_if_absent(store.clone(), REACTION, &seeded)
            .await
            .expect("seed published record");
        set_open_run(
            store.clone(),
            REACTION,
            PROJECT_ITEM_NODE_ID,
            &expected_run_id(ISSUE_BODY),
        )
        .await
        .expect("seed open-run pointer");

        // ...and then somebody tampered with the published decision.
        match case {
            Tamper::Edited => github.mark_comment_edited(&decision_node_id),
            Tamper::Deleted => github.delete_comment(&decision_node_id),
            Tamper::Divergent => github.silently_rewrite_comment(
                &decision_node_id,
                &expected_decision_body(ISSUE_BODY, ValidationOutcome::Failed),
            ),
        }

        let (core, handle) = start_core(&server.uri(), store.clone(), core_id).await;
        insert_candidate(&handle, "candidate-1", &completion_node_id).await;

        assert!(
            wait_for_halt(&core).await,
            "a tampered published decision must fail closed, not be skipped; \
             the reaction is still {:?}",
            reaction_status(&core).await
        );
        assert_eq!(
            github.status_mutations(),
            0,
            "a tampered decision must never be completed"
        );
        assert_eq!(github.status(), ROUTABLE_STATUS, "status must not move");
        assert_eq!(
            github.create_comment_calls(),
            0,
            "no comment may be written either"
        );

        let record = record(&store, ISSUE_BODY).await.expect("record exists");
        assert!(!record.status_applied, "the status step must not run");
        assert_eq!(
            record.decision_comment_node_id.as_deref(),
            Some(decision_node_id.as_str()),
            "the record must still point at the published decision"
        );

        core.stop().await.expect("stop core");
    }

    run_case(Tamper::Edited, "router-tampered-edited").await;
    run_case(Tamper::Deleted, "router-tampered-deleted").await;
    run_case(Tamper::Divergent, "router-tampered-divergent").await;
}

// ---------------------------------------------------------------------
// 21. An ambiguous create-comment error whose write actually landed is
//     reconciled on replay, even after the issue body changes.
//
// This is the regression the durable "publication attempted" marker exists
// for: the first attempt never learns the comment node ID, so before the
// marker the record looked pre-publication, and a replay whose issue body had
// changed since would re-derive a *different* run, find no chain, and skip the
// row forever — stranding a `RoutingDecided` comment that is already visible in
// the issue thread.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn an_ambiguous_write_that_landed_is_adopted_after_the_body_changes() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);
    let decision_body = expected_decision_body(ISSUE_BODY, ValidationOutcome::Passed);

    // The server accepts the comment but the client never sees the response.
    github.set_create_comment_delay(Some(Duration::from_millis(2500)));

    let (first, handle) = start_core(&server.uri(), store.clone(), "router-landed-1").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;

    assert!(
        wait_until(
            || async {
                record(&store, ISSUE_BODY)
                    .await
                    .is_some_and(|record| record.ambiguous)
            },
            8000
        )
        .await,
        "the ambiguous write must be persisted"
    );
    let attempted = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(
        attempted.decision_publish_attempted,
        "the attempt must be durable before the write"
    );
    assert!(
        attempted.decision_comment_node_id.is_none(),
        "the write outcome was never observed"
    );
    assert!(!attempted.status_applied);
    assert_eq!(github.create_comment_calls(), 1);
    assert_eq!(
        github
            .comment_bodies()
            .iter()
            .filter(|body| *body == &decision_body)
            .count(),
        1,
        "the decision did land, unobserved"
    );
    assert_eq!(github.status(), ROUTABLE_STATUS, "the status did not move");
    first.stop().await.expect("stop first core");

    // The issue body is edited before the replay: a fresh derivation would now
    // produce a different runId with no chain at all, which must NOT strand the
    // decision that is already visible in the thread.
    github.set_create_comment_delay(None);
    github.set_issue_body(Some("Rewritten while the router was down.\n"));

    let (second, handle) = start_core_with_query(
        &server.uri(),
        store.clone(),
        "router-landed-2",
        "route-workgraph-items-after-restart",
    )
    .await;
    insert_candidate(&handle, "candidate-replay", &completion_node_id).await;

    assert!(
        wait_until(|| async { github.status() == PASSED_STATUS }, 5000).await,
        "the replay never completed the landed decision; status is '{}' and the reaction is {:?}",
        github.status(),
        reaction_status(&second).await
    );
    assert_eq!(
        github.create_comment_calls(),
        1,
        "the landed decision must be adopted, not re-posted"
    );
    assert_eq!(
        github
            .comment_bodies()
            .iter()
            .filter(|body| *body == &decision_body)
            .count(),
        1,
        "exactly one decision comment may exist"
    );
    assert_eq!(
        github.status_mutations(),
        1,
        "the persisted status move must be applied exactly once"
    );

    let finished = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(finished.is_complete());
    assert!(!finished.ambiguous);
    assert_eq!(finished.to_status, PASSED_STATUS);
    assert!(
        finished.decision_comment_node_id.is_some(),
        "the adopted comment ID must be durable"
    );

    // Delivering the row again is a no-op: the run is complete.
    let reads_before = github.issue_reads();
    insert_candidate(&handle, "candidate-replay-2", &completion_node_id).await;
    assert!(
        wait_until(|| async { github.issue_reads() > reads_before }, 5000).await,
        "the repeated delivery was never processed"
    );
    assert_eq!(github.status_mutations(), 1, "still exactly once");
    assert_eq!(github.create_comment_calls(), 1, "still one comment");
    assert_eq!(reaction_status(&second).await, ComponentStatus::Running);

    second.stop().await.expect("stop second core");
}

// ---------------------------------------------------------------------
// 22. An ambiguous create-comment error whose write did *not* land publishes
//     the pinned decision on replay — exactly once — even after the issue body
//     changes.
//
// The mirror image of case 21: the same durable state resolves to the opposite
// physical outcome, and the harness distinguishes them by the number of
// create-comment calls the server saw.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn an_ambiguous_write_that_never_landed_publishes_the_pinned_decision() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());
    let completion_node_id = seed_chain(&github, ISSUE_BODY, ValidationOutcome::Passed);
    let decision_body = expected_decision_body(ISSUE_BODY, ValidationOutcome::Passed);

    // The create fails without appending the comment.
    github.fail_next_comment_creates(1);

    let (first, handle) = start_core(&server.uri(), store.clone(), "router-unlanded-1").await;
    insert_candidate(&handle, "candidate-1", &completion_node_id).await;

    assert!(
        wait_until(
            || async {
                record(&store, ISSUE_BODY)
                    .await
                    .is_some_and(|record| record.ambiguous)
            },
            8000
        )
        .await,
        "the failed write must be persisted as ambiguous"
    );
    let attempted = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(
        attempted.decision_publish_attempted,
        "the attempt must be durable even though nothing landed"
    );
    assert!(attempted.decision_comment_node_id.is_none());
    assert_eq!(github.create_comment_calls(), 1);
    assert_eq!(
        github
            .comment_bodies()
            .iter()
            .filter(|body| *body == &decision_body)
            .count(),
        0,
        "nothing landed"
    );
    assert_eq!(github.status(), ROUTABLE_STATUS);
    first.stop().await.expect("stop first core");

    // The issue body changes here too: the replay must publish the *pinned*
    // decision from durable state rather than re-deriving anything.
    github.set_issue_body(Some("Rewritten while the router was down.\n"));

    let (second, handle) = start_core_with_query(
        &server.uri(),
        store.clone(),
        "router-unlanded-2",
        "route-workgraph-items-after-restart",
    )
    .await;
    insert_candidate(&handle, "candidate-replay", &completion_node_id).await;

    assert!(
        wait_until(|| async { github.status() == PASSED_STATUS }, 5000).await,
        "the replay never published the pinned decision; status is '{}' and the reaction is {:?}",
        github.status(),
        reaction_status(&second).await
    );
    assert_eq!(
        github.create_comment_calls(),
        2,
        "exactly one further create-comment call was needed"
    );
    assert_eq!(
        github
            .comment_bodies()
            .iter()
            .filter(|body| *body == &decision_body)
            .count(),
        1,
        "the replay publishes exactly one decision, byte-identical to the pinned one"
    );
    assert_eq!(github.status_mutations(), 1, "applied exactly once");

    let finished = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(finished.is_complete());
    assert!(!finished.ambiguous);
    assert_eq!(finished.to_status, PASSED_STATUS);

    second.stop().await.expect("stop second core");
}
