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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use drasi_lib::channels::{QueryResult, ResultDiff};
use drasi_lib::component_graph::ComponentGraph;
use drasi_lib::state_store::{
    StateStoreCompareAndSwapResult, StateStoreCreateIfAbsentResult, StateStoreProvider,
    StateStoreResult,
};
use drasi_lib::{Reaction, ReactionRuntimeContext};
use drasi_reaction_workgraph_router::candidate::RoutingCandidate;
use drasi_reaction_workgraph_router::config::{
    StatusTransition, WorkgraphRouterReactionConfig, ROUTE_QUERY_ID,
};
use drasi_reaction_workgraph_router::decision::RoutingDecision;
use drasi_reaction_workgraph_router::rules::{RoutingPolicyEngine, RulesV1PolicyEngine};
use drasi_reaction_workgraph_router::state::{
    load_reservation, save_reservation, save_routing_state, ReservationRecord, RoutingStateRecord,
    SideEffectProgress,
};
use drasi_reaction_workgraph_router::WorkgraphRouterReaction;
use serde_json::{json, Value};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TEST_REACTION_ID: &str = "workgraph-router-test";

struct DurableMemoryStateStore {
    inner: drasi_lib::MemoryStateStoreProvider,
}

impl DurableMemoryStateStore {
    fn new() -> Self {
        Self {
            inner: drasi_lib::MemoryStateStoreProvider::new(),
        }
    }
}

#[async_trait::async_trait]
impl StateStoreProvider for DurableMemoryStateStore {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
        self.inner.get(store_id, key).await
    }

    async fn set(&self, store_id: &str, key: &str, value: Vec<u8>) -> StateStoreResult<()> {
        self.inner.set(store_id, key, value).await
    }

    async fn create_if_absent(
        &self,
        store_id: &str,
        key: &str,
        value: Vec<u8>,
    ) -> StateStoreResult<StateStoreCreateIfAbsentResult> {
        self.inner.create_if_absent(store_id, key, value).await
    }

    async fn compare_and_swap(
        &self,
        store_id: &str,
        key: &str,
        expected: Option<&[u8]>,
        new_value: Vec<u8>,
    ) -> StateStoreResult<StateStoreCompareAndSwapResult> {
        self.inner
            .compare_and_swap(store_id, key, expected, new_value)
            .await
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

    fn is_durable(&self) -> bool {
        true
    }
}

fn sample_candidate() -> RoutingCandidate {
    RoutingCandidate {
        execution_id: "exec-1".to_string(),
        required_event_type: "CompletedIssueValidation".to_string(),
        event_id: "event-1".to_string(),
        event_type: "CompletedIssueValidation".to_string(),
        outcome: "passed".to_string(),
        subject_repo: "drasi-project/drasi-core".to_string(),
        subject_issue_number: 42,
        project_id: "PVT_project".to_string(),
        project_item_id: "PVTI_item".to_string(),
        project_status: "AwaitingRouting".to_string(),
        route_id: "route-1".to_string(),
        route_expected_event_id: "event-1".to_string(),
        route_expected_event_type: "CompletedIssueValidation".to_string(),
        route_expected_subject_repo: "drasi-project/drasi-core".to_string(),
        route_expected_subject_issue_number: 42,
        route_content_version: "sha256:abc".to_string(),
        route_content_profile: "phase2".to_string(),
        responsibility_id: "resp-1".to_string(),
        responsibility_type: "issue-validation".to_string(),
        responsibility_actor: "bot-user".to_string(),
        submitter_actor: "submitter-user".to_string(),
        launcher_author: "launcher-user".to_string(),
        agent_author: "agent-user".to_string(),
        router_author: "router-user".to_string(),
        routing_author: "router-user".to_string(),
        observed_authors: vec![
            "launcher-user".to_string(),
            "agent-user".to_string(),
            "router-user".to_string(),
        ],
        comment_id: 1000,
        comment_author: "router-user".to_string(),
        comment_body: "{\"source\":\"validated\"}".to_string(),
        comment_edited: false,
        comment_created_at: Some("2026-01-01T00:00:00Z".to_string()),
        comment_updated_at: Some("2026-01-01T00:00:00Z".to_string()),
        comment_provenance_event_id: "event-1".to_string(),
        comment_provenance_event_type: "CompletedIssueValidation".to_string(),
        content_version: "sha256:abc".to_string(),
        content_profile: "phase2".to_string(),
    }
}

fn status_snapshot_response(status: &str) -> Value {
    status_snapshot_response_for_item(
        status,
        "Issue",
        "drasi-project/drasi-core",
        Some(42),
        "PVT_project",
        "PVTI_item",
    )
}

fn status_snapshot_response_for_item(
    status: &str,
    content_type: &str,
    repo_name_with_owner: &str,
    issue_number: Option<u64>,
    project_id: &str,
    item_id: &str,
) -> Value {
    json!({
        "data": {
            "project": {
                "id": project_id,
                "fields": {
                    "nodes": [
                        {
                            "id": "PVTSSF_status",
                            "name": "Status",
                            "options": [
                                {"id":"PVTSSO_awaiting_routing","name":"AwaitingRouting"},
                                {"id":"PVTSSO_awaiting_risk","name":"AwaitingIssueRiskProfiling"},
                                {"id":"PVTSSO_needs_more_information","name":"NeedsMoreInformation"},
                                {"id":"PVTSSO_done","name":"Done"}
                            ]
                        }
                    ]
                }
            },
            "item": {
                "id": item_id,
                "project": {"id": project_id},
                "content": {
                    "__typename": content_type,
                    "number": issue_number,
                    "repository": {
                        "nameWithOwner": repo_name_with_owner,
                        "owner": {
                            "login": repo_name_with_owner.split('/').next().unwrap_or("drasi-project")
                        },
                        "name": repo_name_with_owner.split('/').nth(1).unwrap_or("drasi-core")
                    }
                },
                "fieldValueByName": {
                    "name": status,
                    "optionId": "PVTSSO_current"
                }
            }
        }
    })
}

fn status_update_response() -> Value {
    json!({
        "data": {
            "updateProjectV2ItemFieldValue": {
                "projectV2Item": { "id": "PVTI_item" }
            }
        }
    })
}

fn base_reaction(server: &MockServer, policy_version: &str) -> WorkgraphRouterReaction {
    base_reaction_with_timeouts(server, policy_version, 5, 15)
}

fn base_reaction_with_timeouts(
    server: &MockServer,
    policy_version: &str,
    timeout_secs: u64,
    reservation_lease_secs: u64,
) -> WorkgraphRouterReaction {
    WorkgraphRouterReaction::builder(TEST_REACTION_ID)
        .with_query(ROUTE_QUERY_ID)
        .with_policy_id("policy-1")
        .with_policy_type("rules_v1")
        .with_policy_version(policy_version)
        .with_allowed_projects(vec!["PVT_project".to_string()])
        .with_allowed_repos(vec!["drasi-project/drasi-core".to_string()])
        .with_allowed_event_types(vec!["CompletedIssueValidation".to_string()])
        .with_allowed_status_transitions(vec![
            StatusTransition {
                from: "AwaitingRouting".to_string(),
                to: "AwaitingIssueRiskProfiling".to_string(),
            },
            StatusTransition {
                from: "AwaitingRouting".to_string(),
                to: "NeedsMoreInformation".to_string(),
            },
        ])
        .with_allowed_responsibility_types(vec![
            "issue-validation".to_string(),
            "issue-risk-profiling".to_string(),
            "issue-correction".to_string(),
        ])
        .with_allowed_actors(vec!["bot-user".to_string(), "submitter-user".to_string()])
        .with_trusted_routing_authors(vec!["router-user".to_string()])
        .with_trusted_launcher_authors(vec!["launcher-user".to_string()])
        .with_trusted_agent_authors(vec!["agent-user".to_string()])
        .with_trusted_router_authors(vec!["router-user".to_string()])
        .with_trusted_router_author_node_ids(vec!["MDQ6VXNlcjE=".to_string()])
        .with_github_rest_url(server.uri())
        .with_github_graphql_url(format!("{}/graphql", server.uri()))
        .with_github_token_env("WG_ROUTER_TEST_TOKEN")
        .with_timeout_secs(timeout_secs)
        .with_reservation_lease_secs(reservation_lease_secs)
        .with_strict_recovery(true)
        .build()
        .expect("reaction should build")
}

fn policy_identity_config(policy_version: &str) -> WorkgraphRouterReactionConfig {
    WorkgraphRouterReactionConfig {
        policy_id: "policy-1".to_string(),
        policy_type: "rules_v1".to_string(),
        policy_version: policy_version.to_string(),
        ..WorkgraphRouterReactionConfig::default()
    }
}

async fn seed_failed_reservation_state(
    store: Arc<dyn StateStoreProvider>,
    candidate: &RoutingCandidate,
    policy_version: &str,
) {
    let reservation = ReservationRecord {
        reservation_key: candidate.reservation_key(),
        execution_id: candidate.execution_id.clone(),
        required_event_type: candidate.required_event_type.clone(),
        owner_instance_id: Some("legacy-runner".to_string()),
        fencing_epoch: 1,
        lease_expires_at_unix_secs: 0,
        policy_id: "policy-1".to_string(),
        policy_type: "rules_v1".to_string(),
        policy_version: policy_version.to_string(),
        decision_id: None,
        created_at: "2026-01-01T00:00:00Z".to_string(),
        completed: false,
    };
    let outcome = RulesV1PolicyEngine
        .evaluate(candidate)
        .expect("rules evaluation");
    let decision =
        RoutingDecision::from_policy(&policy_identity_config(policy_version), candidate, outcome)
            .expect("decision");

    let mut state = RoutingStateRecord::new(candidate, &reservation);
    state.decision = Some(decision.clone());
    state.selected_transition = Some((decision.from_status.clone(), decision.to_status.clone()));
    state.progress = SideEffectProgress {
        decision_comment_written: true,
        responsibility_written: true,
        project_status_updated: false,
    };
    state.mark_error("simulated interrupted status update", true);

    save_reservation(store.clone(), TEST_REACTION_ID, &reservation)
        .await
        .expect("seed reservation");
    save_routing_state(store, TEST_REACTION_ID, &state)
        .await
        .expect("seed routing state");
}

async fn initialize_reaction(
    reaction: &WorkgraphRouterReaction,
    state_store: Arc<dyn StateStoreProvider>,
) {
    let (graph, _rx) = ComponentGraph::new("wg-router-integration");
    let context = ReactionRuntimeContext::new(
        "wg-router-integration",
        reaction.id(),
        Some(state_store),
        graph.update_sender(),
        None,
    );
    reaction.initialize(context).await;
}

async fn enqueue_add(
    reaction: &WorkgraphRouterReaction,
    candidate: &RoutingCandidate,
    sequence: u64,
) {
    let result = QueryResult::new(
        ROUTE_QUERY_ID.to_string(),
        sequence,
        Utc::now(),
        vec![ResultDiff::Add {
            data: serde_json::to_value(candidate).expect("candidate json"),
            row_signature: 1,
        }],
        HashMap::new(),
    );
    reaction
        .enqueue_query_result(result)
        .await
        .expect("enqueue add");
}

async fn enqueue_terminal_rejections_then_valid(reaction: &WorkgraphRouterReaction, sequence: u64) {
    let mut invalid_candidate = sample_candidate();
    invalid_candidate.execution_id = "exec-invalid".to_string();
    invalid_candidate.outcome = "unknown".to_string();

    let result = QueryResult::new(
        ROUTE_QUERY_ID.to_string(),
        sequence,
        Utc::now(),
        vec![
            ResultDiff::Add {
                data: json!({"unexpected": "shape"}),
                row_signature: 0,
            },
            ResultDiff::Add {
                data: serde_json::to_value(&invalid_candidate).expect("invalid candidate json"),
                row_signature: 0,
            },
            ResultDiff::Add {
                data: serde_json::to_value(sample_candidate()).expect("valid candidate json"),
                row_signature: 103,
            },
        ],
        HashMap::new(),
    );
    reaction
        .enqueue_query_result(result)
        .await
        .expect("enqueue mixed terminal and valid rows");
}

async fn enqueue_update(reaction: &WorkgraphRouterReaction, sequence: u64) {
    let result = QueryResult::new(
        ROUTE_QUERY_ID.to_string(),
        sequence,
        Utc::now(),
        vec![ResultDiff::Update {
            data: json!({"id": "a"}),
            before: json!({"id": "a"}),
            after: json!({"id": "a", "name": "new"}),
            grouping_keys: None,
            row_signature: 2,
        }],
        HashMap::new(),
    );
    reaction
        .enqueue_query_result(result)
        .await
        .expect("enqueue update");
}

async fn enqueue_delete(reaction: &WorkgraphRouterReaction, sequence: u64) {
    let result = QueryResult::new(
        ROUTE_QUERY_ID.to_string(),
        sequence,
        Utc::now(),
        vec![ResultDiff::Delete {
            data: json!({"id":"a"}),
            row_signature: 3,
        }],
        HashMap::new(),
    );
    reaction
        .enqueue_query_result(result)
        .await
        .expect("enqueue delete");
}

async fn wait_for_count<F>(server: &MockServer, mut selector: F, expected: usize)
where
    F: FnMut(&wiremock::Request) -> bool,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let requests = server.received_requests().await.unwrap_or_default();
        let count = requests.iter().filter(|req| selector(req)).count();
        if count >= expected {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("timed out waiting for {expected} matching request(s); observed {count}");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn comment_type_from_request(req: &wiremock::Request) -> Option<String> {
    if req.method.as_str() != "POST" || !req.url.path().ends_with("/comments") {
        return None;
    }
    let outer: Value = serde_json::from_slice(&req.body).ok()?;
    let body = outer.get("body")?.as_str()?;
    let inner: Value = serde_json::from_str(body).ok()?;
    inner
        .get("type")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn is_project_status_mutation(req: &wiremock::Request) -> bool {
    if req.method.as_str() != "POST" || req.url.path() != "/graphql" {
        return false;
    }
    std::str::from_utf8(&req.body)
        .map(|body| {
            body.contains("WorkgraphRouterUpdateProjectV2Status")
                || body.contains("updateProjectV2ItemFieldValue")
        })
        .unwrap_or(false)
}

async fn mount_common_success_mocks(server: &MockServer, preflight_status: &str) {
    let issue = json!({"id": 42, "state":"open"});
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(issue))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(server)
        .await;

    let preflight = status_snapshot_response(preflight_status);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(ResponseTemplate::new(200).set_body_json(preflight))
        .mount(server)
        .await;

    let mutation = status_update_response();
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterUpdateProjectV2Status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(mutation))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 11,
            "body": "ok",
            "user": {"login": "router-user", "node_id": "MDQ6VXNlcjE=", "id": 1001},
            "created_at": "2026-01-01T00:00:00Z",
            "updated_at": "2026-01-01T00:00:00Z"
        })))
        .mount(server)
        .await;
}

#[tokio::test]
#[ignore]
async fn pass_routes_to_risk_profiling_and_applies_side_effects() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-pass");
    let server = MockServer::start().await;
    mount_common_success_mocks(&server, "AwaitingRouting").await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");
    enqueue_add(&reaction, &sample_candidate(), 1).await;
    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 2).await;
    wait_for_count(&server, is_project_status_mutation, 1).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let comment_types: Vec<String> = requests
        .iter()
        .filter_map(comment_type_from_request)
        .collect();
    assert!(comment_types
        .iter()
        .any(|t| t == "workgraph.routing-decision/v1"));
    assert!(comment_types
        .iter()
        .any(|t| t == "workgraph.routing-responsibility/v1"));
}

#[tokio::test]
#[ignore]
async fn failed_validation_routes_to_issue_correction() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-fail");
    let server = MockServer::start().await;
    mount_common_success_mocks(&server, "AwaitingRouting").await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");

    let mut candidate = sample_candidate();
    candidate.outcome = "failed".to_string();
    enqueue_add(&reaction, &candidate, 1).await;
    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 2).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let bodies: Vec<String> = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments"))
        .filter_map(|req| {
            serde_json::from_slice::<Value>(&req.body)
                .ok()
                .and_then(|outer| {
                    outer
                        .get("body")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
        })
        .collect();
    assert!(bodies
        .iter()
        .any(|body| body.contains("NeedsMoreInformation")));
    assert!(bodies.iter().any(|body| body.contains("issue-correction")));
}

#[tokio::test]
#[ignore]
async fn duplicate_rows_do_not_duplicate_comments() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-dup");
    let server = MockServer::start().await;
    mount_common_success_mocks(&server, "AwaitingRouting").await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");

    let candidate = sample_candidate();
    enqueue_add(&reaction, &candidate, 1).await;
    enqueue_add(&reaction, &candidate, 2).await;
    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 2).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let comments = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    assert_eq!(
        comments, 2,
        "duplicate execution should not create extra comments"
    );
}

#[tokio::test]
#[ignore]
async fn terminal_rejections_do_not_poison_later_rows_or_replay() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-terminal-rejection");
    let server = MockServer::start().await;
    mount_common_success_mocks(&server, "AwaitingRouting").await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    let first = base_reaction(&server, "1.0.0");
    initialize_reaction(&first, Arc::clone(&store)).await;
    first.start().await.expect("first reaction start");
    enqueue_terminal_rejections_then_valid(&first, 1).await;
    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 2).await;
    wait_for_count(&server, is_project_status_mutation, 1).await;
    first.stop().await.expect("first reaction stop");

    let keys = store
        .list_keys(TEST_REACTION_ID)
        .await
        .expect("list first-run state");
    assert_eq!(
        keys.iter()
            .filter(|key| key.starts_with("workgraph-router/rejections/"))
            .count(),
        2,
        "malformed and invalid rows must each have one durable rejection"
    );
    let mut rejected_reservation = load_reservation(
        Arc::clone(&store),
        TEST_REACTION_ID,
        "exec-invalid:CompletedIssueValidation",
    )
    .await
    .expect("load rejected reservation")
    .expect("post-reservation rejection must retain a reservation tombstone");
    assert!(
        rejected_reservation.completed
            && rejected_reservation
                .decision_id
                .as_deref()
                .is_some_and(|id| id.starts_with("terminal-rejection:")),
        "post-reservation rejection must durably complete its reservation"
    );

    let mut pending_rejection_key = None;
    for key in keys
        .iter()
        .filter(|key| key.starts_with("workgraph-router/rejections/"))
    {
        let bytes = store
            .get(TEST_REACTION_ID, key)
            .await
            .expect("read rejection record")
            .expect("rejection record exists");
        let mut record: Value =
            serde_json::from_slice(&bytes).expect("valid rejection record json");
        if record.get("reservationKey").is_some() {
            record["finalized"] = json!(false);
            store
                .set(
                    TEST_REACTION_ID,
                    key,
                    serde_json::to_vec(&record).expect("serialize pending rejection"),
                )
                .await
                .expect("restore pending rejection");
            pending_rejection_key = Some(key.clone());
            break;
        }
    }
    let pending_rejection_key =
        pending_rejection_key.expect("post-reservation rejection record exists");
    rejected_reservation.completed = false;
    rejected_reservation.decision_id = None;
    rejected_reservation.owner_instance_id = Some("crashed-runner".to_string());
    rejected_reservation.lease_expires_at_unix_secs = Utc::now().timestamp() - 1;
    save_reservation(Arc::clone(&store), TEST_REACTION_ID, &rejected_reservation)
        .await
        .expect("restore pre-tombstone reservation");

    assert!(
        store
            .delete(TEST_REACTION_ID, &format!("checkpoint:{ROUTE_QUERY_ID}"))
            .await
            .expect("delete first-run checkpoint"),
        "first run must advance the checkpoint"
    );

    let replay = base_reaction(&server, "1.0.0");
    initialize_reaction(&replay, Arc::clone(&store)).await;
    replay.start().await.expect("replay reaction start");
    enqueue_terminal_rejections_then_valid(&replay, 1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    replay.stop().await.expect("replay reaction stop");

    assert!(
        store
            .contains_key(TEST_REACTION_ID, &format!("checkpoint:{ROUTE_QUERY_ID}"))
            .await
            .expect("read replay checkpoint"),
        "replay must advance the checkpoint past terminal rows"
    );
    let finalized_rejection: Value = serde_json::from_slice(
        &store
            .get(TEST_REACTION_ID, &pending_rejection_key)
            .await
            .expect("read replayed rejection")
            .expect("replayed rejection exists"),
    )
    .expect("valid replayed rejection json");
    assert_eq!(
        finalized_rejection
            .get("finalized")
            .and_then(Value::as_bool),
        Some(true),
        "replay must finalize a rejection only after restoring its tombstone"
    );
    let replayed_reservation = load_reservation(
        Arc::clone(&store),
        TEST_REACTION_ID,
        "exec-invalid:CompletedIssueValidation",
    )
    .await
    .expect("load replayed reservation")
    .expect("replayed reservation exists");
    assert!(
        replayed_reservation.completed,
        "replay must complete a pending terminal reservation"
    );
    let replay_keys = store
        .list_keys(TEST_REACTION_ID)
        .await
        .expect("list replay state");
    assert_eq!(
        replay_keys
            .iter()
            .filter(|key| key.starts_with("workgraph-router/rejections/"))
            .count(),
        2,
        "replay must reuse terminal rejection records"
    );

    let requests = server.received_requests().await.expect("requests");
    assert_eq!(
        requests
            .iter()
            .filter(|req| {
                req.method.as_str() == "POST" && req.url.path().ends_with("/comments")
            })
            .count(),
        2,
        "the later valid row must write each comment exactly once"
    );
    assert_eq!(
        requests
            .iter()
            .filter(|req| is_project_status_mutation(req))
            .count(),
        1,
        "the later valid row must update status exactly once"
    );
}

#[tokio::test]
#[ignore]
async fn concurrent_replicas_share_reservation_and_emit_side_effects_once() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-concurrent");
    let server = MockServer::start().await;
    mount_common_success_mocks(&server, "AwaitingRouting").await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction_a = base_reaction(&server, "1.0.0");
    let reaction_b = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction_a, Arc::clone(&store)).await;
    initialize_reaction(&reaction_b, Arc::clone(&store)).await;
    reaction_a.start().await.expect("reaction A start");
    reaction_b.start().await.expect("reaction B start");

    let candidate = sample_candidate();
    tokio::join!(
        enqueue_add(&reaction_a, &candidate, 1),
        enqueue_add(&reaction_b, &candidate, 1)
    );

    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 2).await;
    wait_for_count(&server, is_project_status_mutation, 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    reaction_a.stop().await.expect("reaction A stop");
    reaction_b.stop().await.expect("reaction B stop");

    let requests = server.received_requests().await.expect("requests");
    let comment_posts = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    let status_updates = requests
        .iter()
        .filter(|req| is_project_status_mutation(req))
        .count();
    assert_eq!(
        comment_posts, 2,
        "concurrent replicas must emit exactly one decision/responsibility comment pair"
    );
    assert_eq!(
        status_updates, 1,
        "concurrent replicas must emit exactly one project status mutation"
    );
}

#[tokio::test]
#[ignore]
async fn newer_policy_version_cannot_reroute_existing_reservation() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-policy");
    let server = MockServer::start().await;
    mount_common_success_mocks(&server, "AwaitingRouting").await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let candidate = sample_candidate();

    let reaction_v1 = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction_v1, Arc::clone(&store)).await;
    reaction_v1.start().await.expect("reaction start");
    enqueue_add(&reaction_v1, &candidate, 1).await;
    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 2).await;
    reaction_v1.stop().await.expect("reaction stop");

    let reaction_v2 = base_reaction(&server, "1.1.0");
    initialize_reaction(&reaction_v2, Arc::clone(&store)).await;
    reaction_v2.start().await.expect("reaction start");
    enqueue_add(&reaction_v2, &candidate, 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    reaction_v2.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let comments = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    assert_eq!(
        comments, 2,
        "policy version change must not reroute completion"
    );
}

#[tokio::test]
#[ignore]
async fn retry_with_closed_issue_fails_preflight_and_emits_no_side_effects() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-closed-retry");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"closed"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(status_snapshot_response("AwaitingRouting")),
        )
        .mount(&server)
        .await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let candidate = sample_candidate();
    seed_failed_reservation_state(Arc::clone(&store), &candidate, "1.0.0").await;

    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");
    enqueue_add(&reaction, &candidate, 1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let comment_posts = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    let status_updates = requests
        .iter()
        .filter(|req| is_project_status_mutation(req))
        .count();
    assert_eq!(
        comment_posts, 0,
        "retry preflight failure must not write comments"
    );
    assert_eq!(
        status_updates, 0,
        "retry preflight failure must not overwrite project status"
    );
}

#[tokio::test]
#[ignore]
async fn retry_with_competing_status_fails_preflight_and_does_not_overwrite() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-competing-status");
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"open"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(status_snapshot_response("InProgress")),
        )
        .mount(&server)
        .await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let candidate = sample_candidate();
    seed_failed_reservation_state(Arc::clone(&store), &candidate, "1.0.0").await;

    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");
    enqueue_add(&reaction, &candidate, 1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let comment_posts = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    let status_updates = requests
        .iter()
        .filter(|req| is_project_status_mutation(req))
        .count();
    assert_eq!(comment_posts, 0, "competing status must block side effects");
    assert_eq!(
        status_updates, 0,
        "competing status must not be overwritten by retry"
    );
}

#[tokio::test]
#[ignore]
async fn status_change_between_comment_writes_aborts_second_comment_and_mutation() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-preflight-race-status");
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"open"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let status_calls = Arc::new(AtomicUsize::new(0));
    let status_counter = Arc::clone(&status_calls);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(move |_req: &wiremock::Request| {
            let call = status_counter.fetch_add(1, Ordering::SeqCst);
            let status = if call < 2 {
                "AwaitingRouting"
            } else {
                "InProgress"
            };
            ResponseTemplate::new(200).set_body_json(status_snapshot_response(status))
        })
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterUpdateProjectV2Status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(status_update_response()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 200,
            "body": "ok",
            "user": {"login":"router-user", "node_id":"MDQ6VXNlcjE=", "id": 1001},
            "created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z"
        })))
        .mount(&server)
        .await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");
    enqueue_add(&reaction, &sample_candidate(), 1).await;
    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 1).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let comment_posts = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    let status_updates = requests
        .iter()
        .filter(|req| is_project_status_mutation(req))
        .count();
    assert_eq!(
        comment_posts, 1,
        "second comment must be blocked by status race"
    );
    assert_eq!(status_updates, 0, "status mutation must not run after race");
}

#[tokio::test]
#[ignore]
async fn issue_closes_before_status_mutation_aborts_without_overwrite() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-preflight-race-issue");
    let server = MockServer::start().await;

    let issue_calls = Arc::new(AtomicUsize::new(0));
    let issue_counter = Arc::clone(&issue_calls);
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(move |_req: &wiremock::Request| {
            let call = issue_counter.fetch_add(1, Ordering::SeqCst);
            let state = if call < 2 { "open" } else { "closed" };
            ResponseTemplate::new(200).set_body_json(json!({ "state": state }))
        })
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(status_snapshot_response("AwaitingRouting")),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterUpdateProjectV2Status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(status_update_response()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 201,
            "body": "ok",
            "user": {"login":"router-user", "node_id":"MDQ6VXNlcjE=", "id": 1001},
            "created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z"
        })))
        .mount(&server)
        .await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");
    enqueue_add(&reaction, &sample_candidate(), 1).await;
    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 2).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let comment_posts = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    let status_updates = requests
        .iter()
        .filter(|req| is_project_status_mutation(req))
        .count();
    assert_eq!(
        comment_posts, 2,
        "comments should happen before issue closes"
    );
    assert_eq!(
        status_updates, 0,
        "closed issue before mutation must block project status overwrite"
    );
}

#[tokio::test]
#[ignore]
async fn status_race_between_preflight_and_mutation_rejects_update() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-status-race-pre-mutation");
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"open"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let snapshot_calls = Arc::new(AtomicUsize::new(0));
    let snapshot_counter = Arc::clone(&snapshot_calls);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(move |_req: &wiremock::Request| {
            let call = snapshot_counter.fetch_add(1, Ordering::SeqCst);
            let status = if call < 4 {
                "AwaitingRouting"
            } else {
                "InProgress"
            };
            ResponseTemplate::new(200).set_body_json(status_snapshot_response(status))
        })
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterUpdateProjectV2Status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(status_update_response()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 202,
            "body": "ok",
            "user": {"login":"router-user", "node_id":"MDQ6VXNlcjE=", "id": 1001},
            "created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z"
        })))
        .mount(&server)
        .await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");
    enqueue_add(&reaction, &sample_candidate(), 1).await;
    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 2).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let status_updates = requests
        .iter()
        .filter(|req| is_project_status_mutation(req))
        .count();
    assert_eq!(
        status_updates, 0,
        "status race between preflight and mutation must abort without mutation"
    );
}

#[tokio::test]
#[ignore]
async fn already_at_destination_snapshot_skips_mutation() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-status-already-destination");
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"open"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let snapshot_calls = Arc::new(AtomicUsize::new(0));
    let snapshot_counter = Arc::clone(&snapshot_calls);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(move |_req: &wiremock::Request| {
            let call = snapshot_counter.fetch_add(1, Ordering::SeqCst);
            let status = if call < 4 {
                "AwaitingRouting"
            } else {
                "AwaitingIssueRiskProfiling"
            };
            ResponseTemplate::new(200).set_body_json(status_snapshot_response(status))
        })
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterUpdateProjectV2Status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(status_update_response()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 203,
            "body": "ok",
            "user": {"login":"router-user", "node_id":"MDQ6VXNlcjE=", "id": 1001},
            "created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z"
        })))
        .mount(&server)
        .await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");
    enqueue_add(&reaction, &sample_candidate(), 1).await;
    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 2).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let status_updates = requests
        .iter()
        .filter(|req| is_project_status_mutation(req))
        .count();
    assert_eq!(
        status_updates, 0,
        "already-at-destination snapshot must not send mutation"
    );
}

#[tokio::test]
#[ignore]
async fn mismatched_project_item_issue_rejects_before_side_effects() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-mismatch-item-issue");
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"open"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(status_snapshot_response_for_item(
                "AwaitingRouting",
                "Issue",
                "drasi-project/drasi-core",
                Some(99),
                "PVT_project",
                "PVTI_item",
            )),
        )
        .mount(&server)
        .await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");
    enqueue_add(&reaction, &sample_candidate(), 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let comment_posts = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    let status_updates = requests
        .iter()
        .filter(|req| is_project_status_mutation(req))
        .count();
    assert_eq!(comment_posts, 0);
    assert_eq!(status_updates, 0);
}

#[tokio::test]
#[ignore]
async fn mismatched_project_item_repo_rejects_before_side_effects() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-mismatch-item-repo");
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"open"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(status_snapshot_response_for_item(
                "AwaitingRouting",
                "Issue",
                "drasi-project/other-repo",
                Some(42),
                "PVT_project",
                "PVTI_item",
            )),
        )
        .mount(&server)
        .await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");
    enqueue_add(&reaction, &sample_candidate(), 1).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let comment_posts = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    let status_updates = requests
        .iter()
        .filter(|req| is_project_status_mutation(req))
        .count();
    assert_eq!(comment_posts, 0);
    assert_eq!(status_updates, 0);
}

#[tokio::test]
#[ignore]
async fn stale_owner_with_delayed_preflight_cannot_emit_duplicate_side_effects() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-delayed-lease-fence");
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"open"})))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let snapshot_calls = Arc::new(AtomicUsize::new(0));
    let snapshot_counter = Arc::clone(&snapshot_calls);
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(move |_req: &wiremock::Request| {
            let call = snapshot_counter.fetch_add(1, Ordering::SeqCst);
            if call == 0 {
                std::thread::sleep(Duration::from_millis(800));
            }
            ResponseTemplate::new(200).set_body_json(status_snapshot_response("AwaitingRouting"))
        })
        .mount(&server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterUpdateProjectV2Status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(status_update_response()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 204,
            "body": "ok",
            "user": {"login":"router-user", "node_id":"MDQ6VXNlcjE=", "id": 1001},
            "created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z"
        })))
        .mount(&server)
        .await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction_a = base_reaction_with_timeouts(&server, "1.0.0", 1, 3);
    let reaction_b = base_reaction_with_timeouts(&server, "1.0.0", 1, 3);
    initialize_reaction(&reaction_a, Arc::clone(&store)).await;
    initialize_reaction(&reaction_b, Arc::clone(&store)).await;
    reaction_a.start().await.expect("reaction a start");
    reaction_b.start().await.expect("reaction b start");

    let candidate = sample_candidate();
    enqueue_add(&reaction_a, &candidate, 1).await;

    for _ in 0..40 {
        if load_reservation(
            Arc::clone(&store),
            TEST_REACTION_ID,
            &candidate.reservation_key(),
        )
        .await
        .expect("load reservation")
        .is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    if let Some(mut reservation) = load_reservation(
        Arc::clone(&store),
        TEST_REACTION_ID,
        &candidate.reservation_key(),
    )
    .await
    .expect("load reservation after enqueue")
    {
        reservation.lease_expires_at_unix_secs = Utc::now().timestamp() - 1;
        save_reservation(Arc::clone(&store), TEST_REACTION_ID, &reservation)
            .await
            .expect("force lease expiry");
    }

    enqueue_add(&reaction_b, &candidate, 1).await;
    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 2).await;
    wait_for_count(&server, is_project_status_mutation, 1).await;
    tokio::time::sleep(Duration::from_millis(450)).await;
    reaction_a.stop().await.expect("reaction a stop");
    reaction_b.stop().await.expect("reaction b stop");

    let requests = server.received_requests().await.expect("requests");
    let comment_posts = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    let status_updates = requests
        .iter()
        .filter(|req| is_project_status_mutation(req))
        .count();
    assert_eq!(
        comment_posts, 2,
        "stale owner must not emit duplicate decision/responsibility comments"
    );
    assert_eq!(
        status_updates, 1,
        "stale owner must not duplicate status mutation"
    );
}

#[tokio::test]
#[ignore]
async fn interrupted_old_policy_reservation_resumes_with_persisted_decision_contract() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-old-policy-resume");
    let server = MockServer::start().await;
    mount_common_success_mocks(&server, "AwaitingRouting").await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let candidate = sample_candidate();
    let mut failed_outcome_candidate = candidate.clone();
    failed_outcome_candidate.outcome = "failed".to_string();
    let old_decision = RoutingDecision::from_policy(
        &policy_identity_config("1.0.0"),
        &failed_outcome_candidate,
        RulesV1PolicyEngine
            .evaluate(&failed_outcome_candidate)
            .expect("rules evaluation"),
    )
    .expect("old decision");

    let reservation = ReservationRecord {
        reservation_key: candidate.reservation_key(),
        execution_id: candidate.execution_id.clone(),
        required_event_type: candidate.required_event_type.clone(),
        owner_instance_id: Some("legacy-runner".to_string()),
        fencing_epoch: 1,
        lease_expires_at_unix_secs: 0,
        policy_id: "policy-1".to_string(),
        policy_type: "rules_v1".to_string(),
        policy_version: "1.0.0".to_string(),
        decision_id: Some(old_decision.decision_id.clone()),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        completed: false,
    };
    let mut state = RoutingStateRecord::new(&candidate, &reservation);
    state.decision = Some(old_decision.clone());
    state.selected_transition = Some((
        old_decision.from_status.clone(),
        old_decision.to_status.clone(),
    ));
    state.mark_error("simulated interruption", true);
    save_reservation(Arc::clone(&store), TEST_REACTION_ID, &reservation)
        .await
        .expect("seed reservation");
    save_routing_state(Arc::clone(&store), TEST_REACTION_ID, &state)
        .await
        .expect("seed state");

    let retry = base_reaction(&server, "2.0.0");
    initialize_reaction(&retry, Arc::clone(&store)).await;
    retry.start().await.expect("retry start");
    enqueue_add(&retry, &candidate, 2).await;
    wait_for_count(&server, |req| req.url.path().ends_with("/comments"), 2).await;
    wait_for_count(&server, is_project_status_mutation, 1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    retry.stop().await.expect("retry stop");

    let requests = server.received_requests().await.expect("requests");
    let decision_payloads: Vec<Value> = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .filter_map(|req| {
            let outer: Value = serde_json::from_slice(&req.body).ok()?;
            let body = outer.get("body")?.as_str()?;
            let inner: Value = serde_json::from_str(body).ok()?;
            if inner.get("type").and_then(Value::as_str) == Some("workgraph.routing-decision/v1") {
                Some(inner)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(decision_payloads.len(), 1);
    let payload = &decision_payloads[0];
    assert_eq!(
        payload.pointer("/policy/version").and_then(Value::as_str),
        Some("1.0.0"),
        "retry must resume with persisted policy contract, not current config policy version"
    );
    assert_eq!(
        payload
            .pointer("/transition/toStatus")
            .and_then(Value::as_str),
        Some("NeedsMoreInformation"),
        "retry must use persisted decision transition"
    );
}

#[tokio::test]
#[ignore]
async fn untrusted_inputs_are_rejected_without_side_effects() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-untrusted");
    let server = MockServer::start().await;
    mount_common_success_mocks(&server, "AwaitingRouting").await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");

    let mut candidate = sample_candidate();
    candidate.comment_author = "forged-author".to_string();
    enqueue_add(&reaction, &candidate, 1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let side_effects = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    assert_eq!(
        side_effects, 0,
        "untrusted input must be rejected before writes"
    );
}

#[tokio::test]
#[ignore]
async fn stale_content_and_wrong_status_are_rejected() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-status");
    let server = MockServer::start().await;
    mount_common_success_mocks(&server, "Done").await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");

    let mut candidate = sample_candidate();
    candidate.content_version = "sha256:stale".to_string();
    enqueue_add(&reaction, &candidate, 1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let comments = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    assert_eq!(comments, 0);
}

#[tokio::test]
#[ignore]
async fn graphql_200_errors_are_treated_as_failures() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-graphql");
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"open"})))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors":[{"message":"boom"}],
            "data": null
        })))
        .mount(&server)
        .await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");
    enqueue_add(&reaction, &sample_candidate(), 1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    let comments = requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    assert_eq!(comments, 0);
}

#[tokio::test]
#[ignore]
async fn partial_side_effect_recovery_reconciles_trusted_comments() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-recovery");
    let first = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"open"})))
        .mount(&first)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&first)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(status_snapshot_response("AwaitingRouting")),
        )
        .mount(&first)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(201).set_body_json(json!({
            "id": 101,
            "body": "ok",
            "user": {"login":"router-user", "node_id":"MDQ6VXNlcjE=", "id": 1001},
            "created_at":"2026-01-01T00:00:00Z",
            "updated_at":"2026-01-01T00:00:00Z"
        })))
        .mount(&first)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterUpdateProjectV2Status"))
        .respond_with(ResponseTemplate::new(500).set_body_string("status update failed"))
        .mount(&first)
        .await;

    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let candidate = sample_candidate();
    let reaction = base_reaction(&first, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("start first");
    enqueue_add(&reaction, &candidate, 1).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    reaction.stop().await.expect("stop first");

    let outcome = RulesV1PolicyEngine
        .evaluate(&candidate)
        .expect("rules evaluation");
    let decision = RoutingDecision::from_policy(
        &WorkgraphRouterReactionConfig {
            policy_id: "policy-1".to_string(),
            policy_type: "rules_v1".to_string(),
            policy_version: "1.0.0".to_string(),
            ..WorkgraphRouterReactionConfig::default()
        },
        &candidate,
        outcome,
    )
    .expect("decision");
    let decision_comment = decision
        .decision_comment(&candidate)
        .expect("decision body");
    let responsibility_comment = decision
        .responsibility_comment(&candidate)
        .expect("responsibility body");

    let second = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state":"open"})))
        .mount(&second)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {
                "id": 1,
                "body": decision_comment,
                "user": {"login": "router-user", "node_id":"MDQ6VXNlcjE=", "id": 1001},
                "created_at":"2026-01-01T00:00:00Z",
                "updated_at":"2026-01-01T00:00:00Z"
            },
            {
                "id": 2,
                "body": responsibility_comment,
                "user": {"login": "router-user", "node_id":"MDQ6VXNlcjE=", "id": 1001},
                "created_at":"2026-01-01T00:00:00Z",
                "updated_at":"2026-01-01T00:00:00Z"
            }
        ])))
        .mount(&second)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterProjectStatusSnapshot"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(status_snapshot_response("AwaitingRouting")),
        )
        .mount(&second)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_string_contains("WorkgraphRouterUpdateProjectV2Status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(status_update_response()))
        .mount(&second)
        .await;
    Mock::given(method("POST"))
        .and(path("/repos/drasi-project/drasi-core/issues/42/comments"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&second)
        .await;

    let retry = base_reaction(&second, "1.0.0");
    initialize_reaction(&retry, Arc::clone(&store)).await;
    retry.start().await.expect("start retry");
    enqueue_add(&retry, &candidate, 2).await;
    wait_for_count(&second, is_project_status_mutation, 1).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    retry.stop().await.expect("stop retry");

    let retry_requests = second.received_requests().await.expect("requests");
    let comment_posts = retry_requests
        .iter()
        .filter(|req| req.url.path().ends_with("/comments") && req.method.as_str() == "POST")
        .count();
    assert_eq!(
        comment_posts, 0,
        "reconciliation should prevent duplicate comment writes"
    );
}

#[tokio::test]
#[ignore]
async fn update_and_delete_results_are_ignored() {
    std::env::set_var("WG_ROUTER_TEST_TOKEN", "token-ignore");
    let server = MockServer::start().await;
    mount_common_success_mocks(&server, "AwaitingRouting").await;
    let store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let reaction = base_reaction(&server, "1.0.0");
    initialize_reaction(&reaction, Arc::clone(&store)).await;
    reaction.start().await.expect("reaction start");
    enqueue_update(&reaction, 1).await;
    enqueue_delete(&reaction, 2).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    reaction.stop().await.expect("reaction stop");

    let requests = server.received_requests().await.expect("requests");
    assert!(
        requests.is_empty(),
        "updated/deleted rows must not trigger routing side effects"
    );
}
