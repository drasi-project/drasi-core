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

use async_trait::async_trait;
use chrono::{Duration, Utc};
use reqwest::Client;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use drasi_lib::state_store::{StateStoreProvider, StateStoreResult};

use crate::config::GitHubProjectItemRefreshConfig;
use crate::destination::{DestinationPublishError, DestinationSourceClient};
use crate::graphql::GitHubGraphqlClient;
use crate::models::{
    DeliveryKey, FetchedProjectItemState, HttpElement, HttpSourceChange, ItemVersionRecord,
    ProjectItemStatusNode, PublicationRecord, PublicationState,
};
use crate::processing::{parse_invalidation_input, AddRowOutcome, RefreshProcessor};
use crate::state_store::RefreshStateStore;

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

#[async_trait]
impl StateStoreProvider for DurableMemoryStateStore {
    async fn get(&self, store_id: &str, key: &str) -> StateStoreResult<Option<Vec<u8>>> {
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

    fn is_durable(&self) -> bool {
        true
    }
}

fn test_row(
    delivery_id: &str,
    project_item_node_id: &str,
    project_node_id: &str,
) -> serde_json::Value {
    json!({
        "invalidationNodeId": "INV_test",
        "deliveryId": delivery_id,
        "projectItemNodeId": project_item_node_id,
        "projectNodeId": project_node_id,
        "webhookAction": "edited",
        "webhookUpdatedAt": "2026-08-13T18:00:00Z"
    })
}

fn graphql_success_response(
    project_item_node_id: &str,
    project_node_id: &str,
    updated_at: &str,
    status_name: &str,
    status_option_id: &str,
) -> serde_json::Value {
    json!({
        "data": {
            "node": {
                "__typename": "ProjectV2Item",
                "id": project_item_node_id,
                "updatedAt": updated_at,
                "project": { "id": project_node_id },
                "content": { "__typename": "Issue", "id": "I_123" },
                "fieldValueByName": {
                    "__typename": "ProjectV2ItemFieldSingleSelectValue",
                    "name": status_name,
                    "optionId": status_option_id,
                    "field": { "id": "PVTSSF_status" }
                }
            }
        }
    })
}

fn graphql_missing_status_response(
    project_item_node_id: &str,
    project_node_id: &str,
) -> serde_json::Value {
    json!({
        "data": {
            "node": {
                "__typename": "ProjectV2Item",
                "id": project_item_node_id,
                "updatedAt": "2026-08-13T18:01:00Z",
                "project": { "id": project_node_id },
                "content": { "__typename": "Issue", "id": "I_123" },
                "fieldValueByName": null
            }
        }
    })
}

async fn build_processor(
    graphql_server: &MockServer,
    destination_url: String,
    durable_store: Arc<dyn StateStoreProvider>,
    allowlist: Vec<String>,
) -> (RefreshProcessor, RefreshStateStore) {
    let config = GitHubProjectItemRefreshConfig {
        github_token: "ghp_test_secret".to_string(),
        graphql_url: graphql_server.uri(),
        graphql_headers: HashMap::new(),
        allowlisted_project_ids: allowlist,
        destination_event_url: destination_url.clone(),
        destination_bearer_secret: Some("dest-secret-token".to_string()),
        request_timeout_ms: 1_000,
        delivery_record_ttl_secs: 60,
    };

    let client = Client::builder()
        .user_agent(crate::reaction::HTTP_USER_AGENT)
        .timeout(std::time::Duration::from_millis(config.request_timeout_ms))
        .build()
        .expect("client");
    let store = RefreshStateStore::new(durable_store, "unit-test-reaction");
    let processor = RefreshProcessor::new(
        config.clone(),
        store.clone(),
        GitHubGraphqlClient::new(
            client.clone(),
            config.graphql_url.clone(),
            config.github_token,
            config.graphql_headers,
        ),
        DestinationSourceClient::new(client, destination_url, config.destination_bearer_secret),
    );
    (processor, store)
}

#[test]
fn parses_valid_invalidation_row() {
    let row = test_row("delivery-1", "PVTI_item1", "PVT_project1");
    let parsed = parse_invalidation_input(&row).expect("parse");
    assert_eq!(parsed.delivery_id, "delivery-1");
    assert_eq!(parsed.project_item_node_id, "PVTI_item1");
    assert_eq!(parsed.project_node_id.as_deref(), Some("PVT_project1"));
}

#[test]
fn rejects_missing_required_fields() {
    let row = json!({
        "deliveryId": "delivery-1"
    });
    let err = parse_invalidation_input(&row).expect_err("must fail");
    assert!(err.to_string().contains("missing invalidation node id"));
}

#[test]
fn deterministic_node_id_is_stable() {
    let node_id = ProjectItemStatusNode::deterministic_node_id("PVTI_abcd");
    assert_eq!(node_id, "ProjectItemStatus:PVTI_abcd");
}

#[test]
fn update_payload_matches_http_source_contract() {
    let node = ProjectItemStatusNode {
        id: "ProjectItemStatus:PVTI_abcd".to_string(),
        project_item_node_id: "PVTI_abcd".to_string(),
        project_node_id: "PVT_proj".to_string(),
        status_field_node_id: "PVTSSF_status".to_string(),
        status_option_id: "opt-in-progress".to_string(),
        status_name: "In Progress".to_string(),
        updated_at: Utc::now(),
        refreshed_at: Utc::now(),
        triggering_delivery_id: "delivery-1".to_string(),
    };

    let payload = HttpSourceChange::update_project_item_status(&node).expect("valid timestamp");
    let serialized = serde_json::to_value(payload).expect("serialize");
    assert_eq!(serialized["operation"], "update");
    assert_eq!(serialized["element"]["type"], "node");
    assert_eq!(
        serialized["element"]["labels"],
        json!(["ProjectItemStatus"]),
        "must target deterministic node label"
    );
    assert_eq!(
        serialized["element"]["properties"]["projectItemNodeId"],
        json!("PVTI_abcd")
    );
}

#[test]
fn config_debug_redacts_secrets() {
    let config = GitHubProjectItemRefreshConfig {
        github_token: "ghp_super_secret".to_string(),
        graphql_url: "https://api.github.com/graphql".to_string(),
        graphql_headers: HashMap::from([(
            "X-Api-Key".to_string(),
            "graphql-header-secret".to_string(),
        )]),
        allowlisted_project_ids: vec![],
        destination_event_url: "https://dest.example/changes".to_string(),
        destination_bearer_secret: Some("very-secret".to_string()),
        request_timeout_ms: 1000,
        delivery_record_ttl_secs: 3600,
    };

    let rendered = format!("{config:?}");
    assert!(!rendered.contains("ghp_super_secret"));
    assert!(!rendered.contains("very-secret"));
    assert!(!rendered.contains("graphql-header-secret"));
    assert!(rendered.contains("X-Api-Key"));
    assert!(rendered.contains("[REDACTED]"));
}

#[tokio::test]
async fn process_success_publishes_and_persists_state() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_item1",
                "PVT_project1",
                "2026-08-13T18:10:00Z",
                "In Progress",
                "opt-in-progress",
            )),
        )
        .mount(&graphql_server)
        .await;

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "message": "All 1 events processed successfully"
        })))
        .mount(&destination_server)
        .await;

    let (processor, state_store) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec!["PVT_project1".to_string()],
    )
    .await;

    let outcome = processor
        .process_add_row(&test_row("delivery-1", "PVTI_item1", "PVT_project1"))
        .await
        .expect("processing succeeds");
    assert_eq!(outcome, AddRowOutcome::Published);

    let requests = destination_server
        .received_requests()
        .await
        .expect("requests");
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].headers.get("authorization").is_some(),
        "destination bearer secret should be sent when configured"
    );
    assert_eq!(
        requests[0]
            .headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok()),
        Some("delivery-1:PVTI_item1")
    );
    let destination_payload: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("destination payload");
    let expected_timestamp = chrono::DateTime::parse_from_rfc3339("2026-08-13T18:10:00Z")
        .expect("timestamp")
        .timestamp_nanos_opt()
        .and_then(|value| u64::try_from(value).ok())
        .expect("nanosecond timestamp");
    assert_eq!(destination_payload["timestamp"], json!(expected_timestamp));

    let key = DeliveryKey::new("delivery-1", "PVTI_item1");
    let publication = state_store
        .get_publication(&key)
        .await
        .expect("store read")
        .expect("publication record");
    assert_eq!(publication.state, PublicationState::Published);

    let graphql_requests = graphql_server.received_requests().await.expect("requests");
    assert_eq!(graphql_requests.len(), 1);
    assert_eq!(
        graphql_requests[0]
            .headers
            .get("user-agent")
            .and_then(|value| value.to_str().ok()),
        Some(crate::reaction::HTTP_USER_AGENT)
    );
    let authorization = graphql_requests[0]
        .headers
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .expect("authorization header should be present");
    assert!(
        authorization.starts_with("Bearer "),
        "graphql authorization header should be bearer token"
    );
}

#[tokio::test]
async fn duplicate_delivery_is_deduplicated() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_item2",
                "PVT_project1",
                "2026-08-13T18:20:00Z",
                "Done",
                "opt-done",
            )),
        )
        .mount(&graphql_server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "message": "All 1 events processed successfully"
        })))
        .mount(&destination_server)
        .await;

    let (processor, _) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec![],
    )
    .await;

    let row = test_row("delivery-dup", "PVTI_item2", "PVT_project1");
    assert_eq!(
        processor
            .process_add_row(&row)
            .await
            .expect("first publish"),
        AddRowOutcome::Published
    );
    assert_eq!(
        processor.process_add_row(&row).await.expect("duplicate"),
        AddRowOutcome::Duplicate
    );

    let destination_requests = destination_server
        .received_requests()
        .await
        .expect("requests");
    assert_eq!(destination_requests.len(), 1);
}

#[tokio::test]
async fn graphql_http_200_errors_are_explicit_failures() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "errors": [{"message": "bad credentials"}]
        })))
        .mount(&graphql_server)
        .await;

    let (processor, state_store) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec![],
    )
    .await;

    let row = test_row("delivery-err", "PVTI_item_err", "PVT_project1");
    let err = processor
        .process_add_row(&row)
        .await
        .expect_err("must fail");
    assert!(err.to_string().contains("github graphql returned errors"));

    let key = DeliveryKey::new("delivery-err", "PVTI_item_err");
    let publication = state_store
        .get_publication(&key)
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(publication.state, PublicationState::Failed);
}

#[tokio::test]
async fn missing_status_is_explicit_failure() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_missing_status_response(
                "PVTI_missing_status",
                "PVT_project1",
            )),
        )
        .mount(&graphql_server)
        .await;

    let (processor, state_store) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec![],
    )
    .await;

    let row = test_row(
        "delivery-missing-status",
        "PVTI_missing_status",
        "PVT_project1",
    );
    let err = processor
        .process_add_row(&row)
        .await
        .expect_err("must fail");
    assert!(err.to_string().contains("missing a required Status"));

    let key = DeliveryKey::new("delivery-missing-status", "PVTI_missing_status");
    let publication = state_store
        .get_publication(&key)
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(publication.state, PublicationState::Failed);
}

#[tokio::test]
async fn stale_ordering_is_rejected_without_republish() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_stale_item",
                "PVT_project1",
                "2026-08-13T18:00:00Z",
                "Todo",
                "opt-todo",
            )),
        )
        .mount(&graphql_server)
        .await;

    let (processor, state_store) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec![],
    )
    .await;

    state_store
        .set_item_version(
            "PVTI_stale_item",
            &ItemVersionRecord {
                project_item_node_id: "PVTI_stale_item".to_string(),
                project_node_id: "PVT_project1".to_string(),
                status_field_node_id: "PVTSSF_status".to_string(),
                status_option_id: "opt-done".to_string(),
                status_name: "Done".to_string(),
                updated_at: Utc::now() + Duration::minutes(5),
                refreshed_at: Utc::now(),
                triggering_delivery_id: "previous-delivery".to_string(),
                published_at: Utc::now(),
            },
        )
        .await
        .expect("seed version");

    let outcome = processor
        .process_add_row(&test_row(
            "delivery-stale",
            "PVTI_stale_item",
            "PVT_project1",
        ))
        .await
        .expect("stale should be handled");
    assert_eq!(outcome, AddRowOutcome::Stale);

    let destination_requests = destination_server
        .received_requests()
        .await
        .expect("requests");
    assert_eq!(destination_requests.len(), 0);
}

#[tokio::test]
async fn destination_failure_and_ambiguity_are_persisted() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_dest_fail",
                "PVT_project1",
                "2026-08-13T18:30:00Z",
                "Blocked",
                "opt-blocked",
            )),
        )
        .mount(&graphql_server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&destination_server)
        .await;

    let (processor, state_store) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store.clone(),
        vec![],
    )
    .await;
    let row = test_row("delivery-dest-fail", "PVTI_dest_fail", "PVT_project1");
    let err = processor
        .process_add_row(&row)
        .await
        .expect_err("must fail");
    assert!(err
        .to_string()
        .contains("destination source rejected payload"));
    let publication = state_store
        .get_publication(&DeliveryKey::new("delivery-dest-fail", "PVTI_dest_fail"))
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(publication.state, PublicationState::Failed);
    let rejected_requests = destination_server
        .received_requests()
        .await
        .expect("destination requests");
    assert_eq!(
        rejected_requests.len(),
        1,
        "destination writes must not retry automatically"
    );

    let (ambiguous_processor, ambiguous_store) = build_processor(
        &graphql_server,
        "http://127.0.0.1:9/unreachable".to_string(),
        durable_store,
        vec![],
    )
    .await;
    let ambiguous_row = test_row("delivery-ambiguous", "PVTI_ambiguous", "PVT_project1");
    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_ambiguous"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_ambiguous",
                "PVT_project1",
                "2026-08-13T18:31:00Z",
                "In Progress",
                "opt-ip",
            )),
        )
        .mount(&graphql_server)
        .await;

    let _ = ambiguous_processor
        .process_add_row(&ambiguous_row)
        .await
        .expect_err("transport failure should fail");
    let ambiguous_publication = ambiguous_store
        .get_publication(&DeliveryKey::new("delivery-ambiguous", "PVTI_ambiguous"))
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(ambiguous_publication.state, PublicationState::Ambiguous);
}

#[tokio::test]
async fn destination_requires_a_valid_positive_acknowledgement() {
    let destination_server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not an HTTP source response"))
        .mount(&destination_server)
        .await;

    let client = DestinationSourceClient::new(
        Client::new(),
        destination_server.uri(),
        Option::<String>::None,
    );
    let node = ProjectItemStatusNode {
        id: ProjectItemStatusNode::deterministic_node_id("PVTI_ack"),
        project_item_node_id: "PVTI_ack".to_string(),
        project_node_id: "PVT_project1".to_string(),
        status_field_node_id: "PVTSSF_status".to_string(),
        status_option_id: "opt-ip".to_string(),
        status_name: "In Progress".to_string(),
        updated_at: Utc::now(),
        refreshed_at: Utc::now(),
        triggering_delivery_id: "delivery-ack".to_string(),
    };

    let error = client
        .publish_project_item_status(&node)
        .await
        .expect_err("invalid acknowledgement must fail");
    assert!(matches!(
        error,
        DestinationPublishError::InvalidAcknowledgement { .. }
    ));
    assert!(error.is_ambiguous());
}

#[tokio::test]
async fn recovery_from_fetched_state_replays_the_persisted_payload() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());
    let (processor, state_store) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec![],
    )
    .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "message": "All 1 events processed successfully"
        })))
        .mount(&destination_server)
        .await;

    let key = DeliveryKey::new("delivery-fetched", "PVTI_fetched");
    let fetched_state = FetchedProjectItemState {
        project_item_node_id: "PVTI_fetched".to_string(),
        project_node_id: "PVT_project1".to_string(),
        content_node_id: Some("I_123".to_string()),
        content_type: Some("Issue".to_string()),
        status_field_node_id: "PVTSSF_status".to_string(),
        status_option_id: "opt-persisted".to_string(),
        status_name: "Persisted".to_string(),
        updated_at: chrono::DateTime::parse_from_rfc3339("2026-08-13T19:05:00Z")
            .expect("timestamp")
            .with_timezone(&Utc),
        refreshed_at: Utc::now(),
        triggering_delivery_id: "delivery-fetched".to_string(),
    };
    state_store
        .set_publication(
            &key,
            &PublicationRecord {
                state: PublicationState::Fetched,
                attempts: 1,
                last_error: None,
                fetched_state: Some(fetched_state),
                completed_at: None,
            },
        )
        .await
        .expect("persist fetched state");

    let outcome = processor
        .process_add_row(&test_row(
            "delivery-fetched",
            "PVTI_fetched",
            "PVT_project1",
        ))
        .await
        .expect("fetched recovery succeeds");
    assert_eq!(outcome, AddRowOutcome::Published);
    assert!(
        graphql_server
            .received_requests()
            .await
            .expect("GraphQL requests")
            .is_empty(),
        "recovery must not replace the persisted payload with a refetch"
    );
    let destination_requests = destination_server
        .received_requests()
        .await
        .expect("destination requests");
    let payload: serde_json::Value =
        serde_json::from_slice(&destination_requests[0].body).expect("destination payload");
    assert_eq!(
        payload["element"]["properties"]["statusOptionId"],
        json!("opt-persisted")
    );
}

#[tokio::test]
async fn retries_safe_reads_and_recovery_from_ambiguous_publication() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_retry"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&graphql_server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_retry"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_retry",
                "PVT_project1",
                "2026-08-13T19:00:00Z",
                "In Progress",
                "opt-ip",
            )),
        )
        .mount(&graphql_server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "success": true,
            "message": "All 1 events processed successfully"
        })))
        .mount(&destination_server)
        .await;

    let (processor, state_store) = build_processor(
        &graphql_server,
        "http://127.0.0.1:9/unreachable".to_string(),
        durable_store.clone(),
        vec![],
    )
    .await;
    let row = test_row("delivery-retry", "PVTI_retry", "PVT_project1");
    let _ = processor
        .process_add_row(&row)
        .await
        .expect_err("first publish is ambiguous");
    let first_state = state_store
        .get_publication(&DeliveryKey::new("delivery-retry", "PVTI_retry"))
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(first_state.state, PublicationState::Ambiguous);

    let (recovery_processor, recovery_store) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec![],
    )
    .await;
    let outcome = recovery_processor
        .process_add_row(&row)
        .await
        .expect("recovery publish succeeds");
    assert_eq!(outcome, AddRowOutcome::Published);
    let second_state = recovery_store
        .get_publication(&DeliveryKey::new("delivery-retry", "PVTI_retry"))
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(second_state.state, PublicationState::Published);
    assert!(second_state.attempts >= 2);

    let graphql_requests = graphql_server.received_requests().await.expect("requests");
    assert!(
        graphql_requests.len() >= 2,
        "expected safe read retries; got {}",
        graphql_requests.len()
    );
}

#[tokio::test]
async fn allowlist_rejection_skips_processing() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    let (processor, state_store) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec!["PVT_allowlisted".to_string()],
    )
    .await;

    let outcome = processor
        .process_add_row(&test_row(
            "delivery-rejected",
            "PVTI_rejected",
            "PVT_not_allowlisted",
        ))
        .await
        .expect("allowlist rejection should be non-fatal");
    assert_eq!(outcome, AddRowOutcome::Rejected);

    let publication = state_store
        .get_publication(&DeliveryKey::new("delivery-rejected", "PVTI_rejected"))
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(publication.state, PublicationState::Rejected);

    let graphql_requests = graphql_server.received_requests().await.expect("requests");
    assert_eq!(graphql_requests.len(), 0);
}

#[test]
fn http_source_change_uses_node_contract() {
    let node = ProjectItemStatusNode {
        id: "ProjectItemStatus:PVTI_x".to_string(),
        project_item_node_id: "PVTI_x".to_string(),
        project_node_id: "PVT_p".to_string(),
        status_field_node_id: "PVTSSF_s".to_string(),
        status_option_id: "opt".to_string(),
        status_name: "Todo".to_string(),
        updated_at: Utc::now(),
        refreshed_at: Utc::now(),
        triggering_delivery_id: "delivery".to_string(),
    };
    let payload = HttpSourceChange::update_project_item_status(&node).expect("valid timestamp");
    let HttpSourceChange::Update { element, .. } = payload;
    let HttpElement::Node { labels, .. } = element;
    assert_eq!(labels, vec!["ProjectItemStatus".to_string()]);
}
