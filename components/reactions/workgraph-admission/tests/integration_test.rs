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

//! End-to-end tests for the WorkGraph admission reaction.
//!
//! This is a **protocol-target** reaction: it calls GitHub's REST and GraphQL
//! APIs directly, so a stateful local `wiremock` server stands in for GitHub and
//! a durable in-memory state store stands in for the persistent store. Restart
//! is modelled by building a second reaction over the same state store and the
//! same GitHub state, which is exactly what the durability contract promises.
//!
//! Run with:
//! `cargo test -p drasi-reaction-workgraph-admission --test integration_test -- --ignored --nocapture`

mod durable_memory_store;
mod mock_github;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use drasi_lib::channels::ComponentStatus;
use drasi_lib::state_store::StateStoreProvider;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_workgraph_admission::state::{
    create_record_if_absent, load_record, AdmissionRecord,
};
use drasi_reaction_workgraph_admission::{AdmissionCandidate, WorkgraphAdmissionReaction};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use drasi_workgraph_common::{
    comment::render_comment,
    event::{
        AssignedResponsibilityType, ProfileRef, ResponsibilityAssignedPayload, WorkGraphEvent,
        WorkGraphEventPayload,
    },
    ids::{body_digest, run_id},
    summary::{summary_for, SubjectRef},
};
use durable_memory_store::DurableMemoryStateStoreProvider;
use mock_github::{
    GithubState, MockAuthor, ADMITTED_STATUS, PROFILE_BLOB_SHA, PROJECT_ITEM_NODE_ID,
    PROJECT_NODE_ID, REPOSITORY, SOURCE_STATUS, STATUS_FIELD_NODE_ID, TRUSTED_AUTHOR_DATABASE_ID,
    TRUSTED_AUTHOR_TYPE,
};
use wiremock::MockServer;

const SOURCE: &str = "admission-source";
const QUERY: &str = "admit-workgraph-items";
const REACTION: &str = "workgraph-admission";
const SUBJECT_NODE_ID: &str = "I_kwDOABCDEF6ABCDE";
const SUBJECT_NUMBER: u64 = 742;
const ISSUE_BODY: &str = "Please validate this issue.\n\nworkgraph:validate\n";
const TOKEN_ENV: &str = "WORKGRAPH_ADMISSION_TEST_TOKEN";
const WARMUP: Duration = Duration::from_millis(150);
/// The blob `profileBaseRef` resolves to after the profile file is edited.
const MOVED_PROFILE_BLOB_SHA: &str = "89abcdef0123456789abcdef0123456789abcdef";

fn make_source() -> (ApplicationSource, ApplicationSourceHandle) {
    let config = ApplicationSourceConfig {
        properties: HashMap::new(),
        durability: None,
    };
    ApplicationSource::new(SOURCE, config).expect("create application source")
}

fn query_text() -> &'static str {
    "MATCH (r:AdmissionCandidate) RETURN \
     r.repository AS repository, r.subjectNumber AS subjectNumber, \
     r.subjectNodeId AS subjectNodeId, r.projectNodeId AS projectNodeId, \
     r.projectItemNodeId AS projectItemNodeId, r.projectStatus AS projectStatus"
}

fn build_reaction(server_uri: &str) -> WorkgraphAdmissionReaction {
    WorkgraphAdmissionReaction::builder(REACTION)
        .with_query(QUERY)
        .with_github_rest_url(server_uri.to_string())
        .with_github_graphql_url(format!("{server_uri}/graphql"))
        .with_github_token_env(TOKEN_ENV)
        .with_allowed_repositories(vec![REPOSITORY.to_string()])
        .with_allowed_projects(vec![PROJECT_NODE_ID.to_string()])
        .with_expected_project_status_field_node_id(STATUS_FIELD_NODE_ID)
        .with_expected_source_status(SOURCE_STATUS)
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

async fn insert_candidate(handle: &ApplicationSourceHandle, node_id: &str, status: &str) {
    let properties = PropertyMapBuilder::new()
        .with_string("repository", REPOSITORY)
        .with_integer("subjectNumber", SUBJECT_NUMBER as i64)
        .with_string("subjectNodeId", SUBJECT_NODE_ID)
        .with_string("projectNodeId", PROJECT_NODE_ID)
        .with_string("projectItemNodeId", PROJECT_ITEM_NODE_ID)
        .with_string("projectStatus", status)
        .build();
    handle
        .send_node_insert(node_id, vec!["AdmissionCandidate"], properties)
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

/// The exact comment body the reaction must produce for the sample issue.
fn expected_assignment_body(body: &str) -> String {
    let digest = body_digest(Some(body));
    let event = WorkGraphEvent::new(
        run_id(PROJECT_ITEM_NODE_ID, SUBJECT_NODE_ID, &digest),
        PROJECT_ITEM_NODE_ID,
        SUBJECT_NODE_ID,
        WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
            responsibility_type: AssignedResponsibilityType::IssueValidation,
            profile_ref: ProfileRef::new("issue-validator", PROFILE_BLOB_SHA).expect("profile"),
            content_digest: digest,
        }),
    )
    .expect("event");
    let summary = summary_for(
        &event,
        SubjectRef {
            repository: REPOSITORY,
            number: SUBJECT_NUMBER,
        },
    );
    render_comment(&event, &summary).expect("render")
}

fn expected_run_id(body: &str) -> String {
    run_id(
        PROJECT_ITEM_NODE_ID,
        SUBJECT_NODE_ID,
        &body_digest(Some(body)),
    )
    .as_str()
    .to_string()
}

async fn record(store: &Arc<dyn StateStoreProvider>, body: &str) -> Option<AdmissionRecord> {
    load_record(store.clone(), REACTION, &expected_run_id(body))
        .await
        .expect("load record")
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

fn set_token() {
    std::env::set_var(TOKEN_ENV, "ghp_test_token_do_not_log");
}

// ---------------------------------------------------------------------
// 1. Happy path: one assignment comment, then the admitted status.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore] // Run with: cargo test -- --ignored
async fn admits_item_with_one_comment_then_status() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-happy").await;
    insert_candidate(&handle, "candidate-1", SOURCE_STATUS).await;

    assert!(
        wait_until(|| async { github.status() == ADMITTED_STATUS }, 5000).await,
        "project item was never admitted"
    );

    assert_eq!(github.create_comment_calls(), 1, "exactly one comment");
    assert_eq!(
        github.comment_bodies(),
        vec![expected_assignment_body(ISSUE_BODY)],
        "the comment must be the canonical WorkGraphEvent/v1 body"
    );
    assert_eq!(github.status_mutations(), 1, "exactly one status mutation");

    let record = record(&store, ISSUE_BODY)
        .await
        .expect("admission record exists");
    assert!(record.is_complete());
    assert_eq!(record.comment_node_id.as_deref(), Some("IC_created1"));
    assert_eq!(
        record.profile_ref,
        format!("issue-validator@{PROFILE_BLOB_SHA}")
    );
    assert_eq!(
        record.content_digest,
        body_digest(Some(ISSUE_BODY)).as_str()
    );

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 2. Duplicate delivery of the same row must not write twice.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn duplicate_delivery_admits_exactly_once() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-duplicate").await;
    insert_candidate(&handle, "candidate-1", SOURCE_STATUS).await;
    assert!(
        wait_until(|| async { github.status() == ADMITTED_STATUS }, 5000).await,
        "first admission never completed"
    );

    // A second, distinct row for the same Project Item + body: the run identity
    // is the same, so the durable record must suppress every side effect.
    insert_candidate(&handle, "candidate-2", SOURCE_STATUS).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(github.create_comment_calls(), 1, "no second comment");
    assert_eq!(github.status_mutations(), 1, "no second mutation");
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 3. An ambiguous comment write is persisted and never proceeds to status.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn ambiguous_comment_write_is_persisted_and_halts_before_status() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    // The server accepts the comment but the response never arrives, so the
    // reaction cannot know whether the write landed.
    github.set_create_comment_delay(Some(Duration::from_secs(3)));

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-ambiguous").await;
    insert_candidate(&handle, "candidate-1", SOURCE_STATUS).await;

    assert!(
        wait_until(|| async { github.create_comment_calls() == 1 }, 5000).await,
        "the ambiguous write never reached the server"
    );
    assert!(
        wait_until(
            || async {
                record(&store, ISSUE_BODY)
                    .await
                    .is_some_and(|record| record.ambiguous)
            },
            5000
        )
        .await,
        "the ambiguous outcome was not persisted"
    );

    let record = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(
        record.comment_node_id.is_none(),
        "an unconfirmed comment must not be recorded as durable"
    );
    assert!(!record.status_applied);
    assert_eq!(
        github.status_mutations(),
        0,
        "admission must not proceed to the status write while the comment is unconfirmed"
    );
    assert_eq!(github.status(), SOURCE_STATUS);
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 4. Restarting after that ambiguity adopts the comment instead of re-posting.
//
// A restart is modelled the way the durability contract defines it: the
// durable record and GitHub survive, the in-process query outbox does not.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn restart_after_ambiguous_write_adopts_the_existing_comment() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    // The write that the previous process could not confirm did in fact land.
    let seeded_node_id = github.seed_comment(
        &expected_assignment_body(ISSUE_BODY),
        &MockAuthor::trusted(),
        false,
    );

    // ...and the previous process left an ambiguous intent record behind.
    let digest = body_digest(Some(ISSUE_BODY));
    let run = run_id(PROJECT_ITEM_NODE_ID, SUBJECT_NODE_ID, &digest);
    let event = WorkGraphEvent::new(
        run.clone(),
        PROJECT_ITEM_NODE_ID,
        SUBJECT_NODE_ID,
        WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
            responsibility_type: AssignedResponsibilityType::IssueValidation,
            profile_ref: ProfileRef::new("issue-validator", PROFILE_BLOB_SHA).expect("profile"),
            content_digest: digest.clone(),
        }),
    )
    .expect("event");
    let mut seeded = AdmissionRecord::new(
        run.as_str(),
        event.event_id.as_str(),
        &AdmissionCandidate {
            repository: REPOSITORY.to_string(),
            subject_number: SUBJECT_NUMBER,
            subject_node_id: SUBJECT_NODE_ID.to_string(),
            project_node_id: PROJECT_NODE_ID.to_string(),
            project_item_node_id: PROJECT_ITEM_NODE_ID.to_string(),
            project_status: SOURCE_STATUS.to_string(),
        },
        digest.as_str(),
        &format!("issue-validator@{PROFILE_BLOB_SHA}"),
    );
    seeded.set_error("create comment request failed: operation timed out", true);
    create_record_if_absent(store.clone(), REACTION, &seeded)
        .await
        .expect("seed ambiguous record");

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-restart").await;
    insert_candidate(&handle, "candidate-1", SOURCE_STATUS).await;

    assert!(
        wait_until(|| async { github.status() == ADMITTED_STATUS }, 5000).await,
        "restart never completed the admission"
    );
    assert_eq!(
        github.create_comment_calls(),
        0,
        "the existing comment must be adopted, not re-posted"
    );
    assert_eq!(github.comment_bodies().len(), 1, "no duplicate comment");

    let record = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(record.is_complete());
    assert!(!record.ambiguous);
    assert_eq!(
        record.comment_node_id.as_deref(),
        Some(seeded_node_id.as_str())
    );
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 5. Only trusted, unedited comments may be adopted.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn forged_and_edited_comments_are_never_adopted() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let canonical = expected_assignment_body(ISSUE_BODY);
    // Same canonical event, but written by somebody else entirely...
    github.seed_comment(&canonical, &MockAuthor::untrusted(), false);
    // ...by the trusted numeric database ID under the wrong actor type...
    github.seed_comment(&canonical, &MockAuthor::wrong_actor_type(), false);
    // ...and by us, then edited.
    github.seed_comment(&canonical, &MockAuthor::trusted(), true);

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-forged").await;
    insert_candidate(&handle, "candidate-1", SOURCE_STATUS).await;

    assert!(
        wait_until(|| async { github.status() == ADMITTED_STATUS }, 5000).await,
        "admission never completed"
    );
    assert_eq!(
        github.create_comment_calls(),
        1,
        "neither the forged nor the edited comment may be adopted"
    );
    let record = record(&store, ISSUE_BODY).await.expect("record exists");
    assert_eq!(record.comment_node_id.as_deref(), Some("IC_created4"));
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 6. Two trusted comments claiming one event ID fail closed.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn conflicting_duplicate_events_fail_closed() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    // Same run and event type — therefore the same deterministic event ID —
    // but contradictory payloads.
    let digest = body_digest(Some(ISSUE_BODY));
    let run = run_id(PROJECT_ITEM_NODE_ID, SUBJECT_NODE_ID, &digest);
    let conflicting = WorkGraphEvent::new(
        run,
        PROJECT_ITEM_NODE_ID,
        SUBJECT_NODE_ID,
        WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
            responsibility_type: AssignedResponsibilityType::IssueValidation,
            profile_ref: ProfileRef::new("issue-validator", &"f".repeat(40)).expect("profile"),
            content_digest: digest,
        }),
    )
    .expect("event");
    github.seed_comment(
        &expected_assignment_body(ISSUE_BODY),
        &MockAuthor::trusted(),
        false,
    );
    github.seed_comment(
        &render_comment(&conflicting, "WorkGraph assigned issue validation").expect("render"),
        // A renamed login is still the same trusted identity: trust is keyed on
        // the numeric database ID and actor type, so this comment is compared,
        // not ignored.
        &MockAuthor::trusted_renamed(),
        false,
    );

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-conflict").await;
    insert_candidate(&handle, "candidate-1", SOURCE_STATUS).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(
        github.create_comment_calls(),
        0,
        "a contradiction must never be resolved by writing again"
    );
    assert_eq!(github.status(), SOURCE_STATUS, "status must not move");
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 7. A single pre-existing comment that claims this event ID but carries
//    different content is never adopted.
//
// `eventId` hashes the run and the event type only — it does not cover the
// payload — so a lone divergent comment would otherwise be mistaken for this
// reaction's own completed write.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_divergent_preexisting_assignment_is_never_adopted() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    // Trusted, unedited, same deterministic event ID as the assignment this
    // reaction intends to publish — but a different pinned profile blob.
    let digest = body_digest(Some(ISSUE_BODY));
    let divergent = WorkGraphEvent::new(
        run_id(PROJECT_ITEM_NODE_ID, SUBJECT_NODE_ID, &digest),
        PROJECT_ITEM_NODE_ID,
        SUBJECT_NODE_ID,
        WorkGraphEventPayload::ResponsibilityAssigned(ResponsibilityAssignedPayload {
            responsibility_type: AssignedResponsibilityType::IssueValidation,
            profile_ref: ProfileRef::new("issue-validator", &"a".repeat(40)).expect("profile"),
            content_digest: digest,
        }),
    )
    .expect("event");
    let intended = expected_assignment_body(ISSUE_BODY);
    let divergent_body =
        render_comment(&divergent, "WorkGraph assigned issue validation").expect("render");
    assert_ne!(divergent_body, intended, "the payloads must differ");
    github.seed_comment(&divergent_body, &MockAuthor::trusted(), false);

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-divergent").await;
    insert_candidate(&handle, "candidate-1", SOURCE_STATUS).await;

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
    assert_eq!(
        github.comment_bodies().len(),
        1,
        "no further comment may be written"
    );
    assert_eq!(
        github.status(),
        SOURCE_STATUS,
        "the status must not drift after a failed adoption"
    );
    assert_eq!(github.status_mutations(), 0, "no status mutation at all");

    let record = record(&store, ISSUE_BODY).await.expect("intent is durable");
    assert!(
        record.comment_node_id.is_none(),
        "the divergent comment must never be recorded as ours"
    );
    assert!(!record.status_applied, "the status step must not run");
    assert!(!record.is_complete());

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 8. The audit-only node ID never blocks adoption.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn an_author_reported_without_a_node_id_is_still_adopted() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    // GitHub reports the trusted account's database ID and actor type but no
    // node ID: trust is unaffected, so this is our own earlier write.
    let seeded = github.seed_comment(
        &expected_assignment_body(ISSUE_BODY),
        &MockAuthor::trusted_without_node_id(),
        false,
    );

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-no-node-id").await;
    insert_candidate(&handle, "candidate-1", SOURCE_STATUS).await;

    assert!(
        wait_until(|| async { github.status() == ADMITTED_STATUS }, 5000).await,
        "admission never completed"
    );
    assert_eq!(
        github.create_comment_calls(),
        0,
        "the existing comment must be adopted, not re-posted"
    );
    let record = record(&store, ISSUE_BODY).await.expect("record exists");
    assert_eq!(record.comment_node_id.as_deref(), Some(seeded.as_str()));

    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 9. A stale or mis-bound row produces no side effects at all.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn stale_and_misbound_rows_have_zero_side_effects() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-stale").await;

    // The Project moved on since the query observed the row.
    github.set_status("Done");
    insert_candidate(&handle, "candidate-stale", SOURCE_STATUS).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(github.create_comment_calls(), 0, "stale row must not write");
    assert_eq!(github.status(), "Done", "stale row must not mutate");

    // The row's subject node ID does not match the issue GitHub resolves.
    github.set_status(SOURCE_STATUS);
    github.set_issue_node_id("I_someOtherIssue");
    insert_candidate(&handle, "candidate-misbound", SOURCE_STATUS).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        github.create_comment_calls(),
        0,
        "mis-bound subject must not write"
    );

    // A closed issue is never admitted.
    github.set_issue_node_id(SUBJECT_NODE_ID);
    github.set_issue_state("closed");
    insert_candidate(&handle, "candidate-closed", SOURCE_STATUS).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        github.create_comment_calls(),
        0,
        "closed issue must not write"
    );
    assert_eq!(github.status(), SOURCE_STATUS);

    // The reaction is still healthy: a good row now succeeds.
    github.set_issue_state("open");
    insert_candidate(&handle, "candidate-good", SOURCE_STATUS).await;
    assert!(
        wait_until(|| async { github.status() == ADMITTED_STATUS }, 5000).await,
        "permanent rejections must not wedge the reaction"
    );
    assert_eq!(github.create_comment_calls(), 1);
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 10. Editing the issue body starts a new run, not a duplicate assignment.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn editing_the_issue_body_produces_a_distinct_run() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-edit").await;
    insert_candidate(&handle, "candidate-1", SOURCE_STATUS).await;
    assert!(
        wait_until(|| async { github.status() == ADMITTED_STATUS }, 5000).await,
        "first admission never completed"
    );

    const EDITED: &str = "Edited body without the marker.\n";
    github.set_issue_body(Some(EDITED));
    github.set_status(SOURCE_STATUS);
    insert_candidate(&handle, "candidate-2", SOURCE_STATUS).await;
    assert!(
        wait_until(|| async { github.create_comment_calls() == 2 }, 5000).await,
        "an edited body must be admitted as a new run"
    );

    let first = record(&store, ISSUE_BODY).await.expect("first run");
    let second = record(&store, EDITED).await.expect("second run");
    assert_ne!(first.run_id, second.run_id, "runs must be distinct");
    assert_ne!(first.event_id, second.event_id, "events must be distinct");
    assert_ne!(first.content_digest, second.content_digest);
    assert!(first.is_complete() && second.is_complete());
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 11. A completed run replayed after the profile file moves stays a no-op.
//
// `profileBaseRef` is mutable, so the blob it resolves to drifts with ordinary
// commits. A run that is already assigned and admitted is bound to the
// *immutable* pin its record captured, so drift must neither re-open the run
// nor wedge the reaction.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn a_completed_run_replayed_after_profile_drift_is_a_no_op() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-drift").await;
    insert_candidate(&handle, "candidate-1", SOURCE_STATUS).await;
    assert!(
        wait_until(|| async { github.status() == ADMITTED_STATUS }, 5000).await,
        "the first admission never completed"
    );
    assert!(
        wait_until(
            || async {
                record(&store, ISSUE_BODY)
                    .await
                    .is_some_and(|r| r.is_complete())
            },
            5000
        )
        .await,
        "the completed run was never persisted"
    );
    assert_eq!(
        github.profile_calls(),
        1,
        "a new run pins the profile exactly once"
    );

    // The profile file moves on `profileBaseRef`.
    github.set_profile_blob_sha(MOVED_PROFILE_BLOB_SHA);

    // The same run is delivered again.
    insert_candidate(&handle, "candidate-2", SOURCE_STATUS).await;
    tokio::time::sleep(Duration::from_millis(750)).await;

    assert_eq!(github.create_comment_calls(), 1, "no second comment");
    assert_eq!(github.status_mutations(), 1, "no second status mutation");
    assert_eq!(
        github.profile_calls(),
        1,
        "a completed run must be decided from its record alone, without \
         resolving the mutable profile again"
    );
    assert_eq!(
        reaction_status(&core).await,
        ComponentStatus::Running,
        "profile drift must not wedge a completed run"
    );

    let record = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(record.is_complete());
    assert_eq!(
        record.profile_ref,
        format!("issue-validator@{PROFILE_BLOB_SHA}"),
        "the recorded pin is immutable for the life of the run"
    );
    core.stop().await.expect("stop core");
}

// ---------------------------------------------------------------------
// 12. A status write that fails after publication is finished from the recorded
//     pin when the batch replays, even though the profile moved in between.
//
// The failure halts the reaction without advancing its checkpoint, so recovery
// is exactly what the durability contract promises: the operator clears the
// fault and the reaction replays the same batch from the query outbox. The
// assignment is already public at the originally pinned blob, so the resumed
// attempt must complete *that* assignment — no second comment, exactly one
// status mutation, and no read of the mutable profile path.
// ---------------------------------------------------------------------
#[tokio::test]
#[ignore]
async fn replay_after_a_failed_status_write_finishes_from_the_recorded_pin() {
    set_token();
    let server = MockServer::start().await;
    let github = mock_github::mount(&server, SUBJECT_NODE_ID, Some(ISSUE_BODY)).await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStoreProvider::new());

    // The comment lands; the status mutation that follows it does not.
    github.set_status_mutation_failure(true);

    let (core, handle) = start_core(&server.uri(), store.clone(), "admission-status-fail").await;
    insert_candidate(&handle, "candidate-1", SOURCE_STATUS).await;

    assert!(
        wait_for_halt(&core).await,
        "a failed status write must halt the reaction; it is still {:?}",
        reaction_status(&core).await
    );
    assert_eq!(
        github.create_comment_calls(),
        1,
        "the assignment is published exactly once"
    );
    assert_eq!(
        github.status(),
        SOURCE_STATUS,
        "a failed mutation must not move the status"
    );

    let published = record(&store, ISSUE_BODY).await.expect("record exists");
    let published_comment = published
        .comment_node_id
        .clone()
        .expect("the publication is durable before the status step");
    assert!(!published.status_applied);
    assert!(
        published.ambiguous,
        "the unconfirmed status write is recorded as ambiguous"
    );

    // Restart: the durable record, the checkpoint (never advanced past the
    // failed batch) and GitHub all survive — and meanwhile the profile file
    // moved on `profileBaseRef`.
    github.set_profile_blob_sha(MOVED_PROFILE_BLOB_SHA);
    github.set_status_mutation_failure(false);
    let profile_calls_before_replay = github.profile_calls();

    // `Error -> Starting` is the framework's retry transition: the reaction
    // starts again and the manager replays the un-checkpointed batch from the
    // query outbox.
    core.start_reaction(REACTION)
        .await
        .expect("restart reaction");

    assert!(
        wait_until(|| async { github.status() == ADMITTED_STATUS }, 5000).await,
        "the replay never finished the admission; the reaction is {:?}",
        reaction_status(&core).await
    );
    assert_eq!(
        github.create_comment_calls(),
        1,
        "the recorded publication must never be repeated"
    );
    assert_eq!(
        github.comment_bodies(),
        vec![expected_assignment_body(ISSUE_BODY)],
        "the published assignment still names the originally pinned blob"
    );
    assert_eq!(
        github.status_mutations(),
        1,
        "the status is applied exactly once"
    );
    assert_eq!(
        github.profile_calls(),
        profile_calls_before_replay,
        "a resumed run must finish from its recorded pin, not the moved one"
    );

    let finished = record(&store, ISSUE_BODY).await.expect("record exists");
    assert!(finished.is_complete());
    assert!(!finished.ambiguous);
    assert!(finished.last_error.is_none());
    assert_eq!(
        finished.comment_node_id.as_deref(),
        Some(published_comment.as_str()),
        "the same comment remains the assignment"
    );
    assert_eq!(
        finished.profile_ref,
        format!("issue-validator@{PROFILE_BLOB_SHA}"),
        "the pin recorded before publication is what the run completes at"
    );
    assert_eq!(
        reaction_status(&core).await,
        ComponentStatus::Running,
        "the resumed run must not halt"
    );

    core.stop().await.expect("stop core");
}

/// Keep the shared-state type referenced even when a test set is filtered out.
#[allow(dead_code)]
fn _assert_state_type(_: &GithubState) {}
