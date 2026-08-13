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
use reqwest::{
    header::{HeaderMap, HeaderValue},
    Client, StatusCode,
};
use serde_json::json;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration as StdDuration;
use tokio::sync::Barrier;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

use drasi_lib::state_store::{StateStoreProvider, StateStoreResult};
use drasi_plugin_sdk::prelude::ReactionPluginDescriptor;

use crate::config::GitHubProjectItemRefreshConfig;
use crate::descriptor::GitHubProjectItemRefreshDescriptor;
use crate::destination::{DestinationPublishError, DestinationSourceClient};
use crate::graphql::{rate_limit_retry_after, GitHubGraphqlClient};
use crate::models::{
    DeliveryKey, DeliveryReservation, FetchedProjectItemState, HttpElement, HttpSourceChange,
    ItemVersionRecord, ProjectItemStatusNode, PublicationRecord, PublicationState,
};
use crate::processing::{parse_invalidation_input, AddRowOutcome, RefreshProcessor};
use crate::state_store::RefreshStateStore;

const EXPECTED_STATUS_FIELD_NODE_ID: &str = "PVTSSF_lADOCX0YF84BgNE3zhaadbw";

struct DurableMemoryStateStore {
    inner: drasi_lib::MemoryStateStoreProvider,
    list_keys_calls: AtomicUsize,
    list_keys_delay: StdMutex<Option<StdDuration>>,
}

impl DurableMemoryStateStore {
    fn new() -> Self {
        Self::new_with_list_keys_delay(None)
    }

    fn new_with_list_keys_delay(list_keys_delay: Option<StdDuration>) -> Self {
        Self {
            inner: drasi_lib::MemoryStateStoreProvider::new(),
            list_keys_calls: AtomicUsize::new(0),
            list_keys_delay: StdMutex::new(list_keys_delay),
        }
    }

    fn list_keys_calls(&self) -> usize {
        self.list_keys_calls.load(Ordering::SeqCst)
    }

    fn set_list_keys_delay(&self, delay: Option<StdDuration>) {
        *self
            .list_keys_delay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = delay;
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
        self.list_keys_calls.fetch_add(1, Ordering::SeqCst);
        let delay = *self
            .list_keys_delay
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
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
        "invalidationNodeId": format!("project-item-invalidation:{delivery_id}"),
        "deliveryId": delivery_id,
        "projectItemNodeId": project_item_node_id,
        "projectNodeId": project_node_id,
        "webhookAction": "edited",
        "invalidatedAt": "2026-08-13T18:00:00Z"
    })
}

fn graphql_success_response(
    project_item_node_id: &str,
    project_node_id: &str,
    updated_at: &str,
    status_name: &str,
    status_option_id: &str,
) -> serde_json::Value {
    graphql_success_response_with_field_id(
        project_item_node_id,
        project_node_id,
        updated_at,
        status_name,
        status_option_id,
        EXPECTED_STATUS_FIELD_NODE_ID,
    )
}

fn graphql_success_response_with_field_id(
    project_item_node_id: &str,
    project_node_id: &str,
    updated_at: &str,
    status_name: &str,
    status_option_id: &str,
    status_field_node_id: &str,
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
                    "field": { "id": status_field_node_id }
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
    build_processor_with_status_field_name(
        graphql_server,
        destination_url,
        durable_store,
        allowlist,
        "Status",
    )
    .await
}

async fn build_processor_with_status_field_name(
    graphql_server: &MockServer,
    destination_url: String,
    durable_store: Arc<dyn StateStoreProvider>,
    allowlist: Vec<String>,
    status_field_name: &str,
) -> (RefreshProcessor, RefreshStateStore) {
    build_processor_with_status_field_name_and_expected_status_field_node_id(
        graphql_server,
        destination_url,
        durable_store,
        allowlist,
        status_field_name,
        EXPECTED_STATUS_FIELD_NODE_ID,
    )
    .await
}

async fn build_processor_with_status_field_name_and_expected_status_field_node_id(
    graphql_server: &MockServer,
    destination_url: String,
    durable_store: Arc<dyn StateStoreProvider>,
    allowlist: Vec<String>,
    status_field_name: &str,
    expected_status_field_node_id: &str,
) -> (RefreshProcessor, RefreshStateStore) {
    let config = GitHubProjectItemRefreshConfig {
        github_token: "ghp_test_secret".to_string(),
        graphql_url: graphql_server.uri(),
        graphql_headers: HashMap::new(),
        allowlisted_project_ids: allowlist,
        status_field_name: status_field_name.to_string(),
        expected_status_field_node_id: expected_status_field_node_id.to_string(),
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
            config.status_field_name,
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
fn parses_pascal_case_dogfood_contract_fields() {
    let row = json!({
        "InvalidationNodeId": "INV_pascal",
        "DeliveryId": "delivery-pascal",
        "ProjectItemNodeId": "PVTI_pascal",
        "ProjectNodeId": "PVT_pascal",
        "StatusFieldNodeId": EXPECTED_STATUS_FIELD_NODE_ID,
        "StateSourceUrl": "http://127.0.0.1:9001/sources/github-project-state/events",
        "InvalidatedAt": "2026-08-13T18:00:00Z"
    });
    let parsed = parse_invalidation_input(&row).expect("parse");
    assert_eq!(parsed.invalidation_node_id, "INV_pascal");
    assert_eq!(parsed.delivery_id, "delivery-pascal");
    assert_eq!(parsed.project_item_node_id, "PVTI_pascal");
    assert_eq!(parsed.project_node_id.as_deref(), Some("PVT_pascal"));
    assert_eq!(
        parsed.status_field_node_id.as_deref(),
        Some(EXPECTED_STATUS_FIELD_NODE_ID)
    );
    assert_eq!(
        parsed.state_source_url.as_deref(),
        Some("http://127.0.0.1:9001/sources/github-project-state/events")
    );
    assert!(parsed.webhook_updated_at.is_some());
}

#[test]
fn invalidated_at_maps_to_webhook_updated_at() {
    let row = json!({
        "InvalidationNodeId": "INV_ts",
        "DeliveryId": "delivery-ts",
        "ProjectItemNodeId": "PVTI_ts",
        "InvalidatedAt": "2026-08-13T18:15:00Z"
    });
    let parsed = parse_invalidation_input(&row).expect("parse");
    assert_eq!(parsed.delivery_id, "delivery-ts");
    assert_eq!(parsed.project_item_node_id, "PVTI_ts");
    let parsed_timestamp = parsed
        .webhook_updated_at
        .expect("invalidatedAt should populate webhook timestamp");
    assert_eq!(
        parsed_timestamp.to_rfc3339(),
        "2026-08-13T18:15:00+00:00".to_string()
    );
}

#[test]
fn lower_camel_invalidated_at_maps_to_webhook_updated_at() {
    let row = json!({
        "invalidationNodeId": "INV_ts_lc",
        "deliveryId": "delivery-ts-lc",
        "projectItemNodeId": "PVTI_ts_lc",
        "invalidatedAt": "2026-08-13T18:16:00Z"
    });
    let parsed = parse_invalidation_input(&row).expect("parse");
    let parsed_timestamp = parsed
        .webhook_updated_at
        .expect("lower-camel invalidatedAt should populate webhook timestamp");
    assert_eq!(
        parsed_timestamp.to_rfc3339(),
        "2026-08-13T18:16:00+00:00".to_string()
    );
}

#[test]
fn deterministic_node_id_is_stable() {
    let node_id = ProjectItemStatusNode::deterministic_node_id("PVTI_abcd");
    assert_eq!(node_id, "project-item-status:PVTI_abcd");
}

#[test]
fn update_payload_matches_http_source_contract() {
    let node = ProjectItemStatusNode {
        id: "project-item-status:PVTI_abcd".to_string(),
        project_item_node_id: "PVTI_abcd".to_string(),
        project_node_id: "PVT_proj".to_string(),
        status_field_node_id: EXPECTED_STATUS_FIELD_NODE_ID.to_string(),
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
        status_field_name: "Status".to_string(),
        expected_status_field_node_id: EXPECTED_STATUS_FIELD_NODE_ID.to_string(),
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

#[test]
fn config_validation_rejects_empty_status_field_name() {
    let config = GitHubProjectItemRefreshConfig {
        github_token: "ghp_test_secret".to_string(),
        graphql_url: "https://api.github.com/graphql".to_string(),
        graphql_headers: HashMap::new(),
        allowlisted_project_ids: vec![],
        status_field_name: "   ".to_string(),
        expected_status_field_node_id: EXPECTED_STATUS_FIELD_NODE_ID.to_string(),
        destination_event_url: "https://dest.example/changes".to_string(),
        destination_bearer_secret: None,
        request_timeout_ms: 1000,
        delivery_record_ttl_secs: 3600,
    };

    let err = config
        .validate(&["query".to_string()], Some(10))
        .expect_err("empty statusFieldName should fail validation");
    assert!(err.to_string().contains("statusFieldName"));
}

#[test]
fn config_deserialization_rejects_missing_expected_status_field_node_id() {
    let json = json!({
        "githubToken": "ghp_test_secret",
        "graphqlUrl": "https://api.github.com/graphql",
        "statusFieldName": "Status",
        "destinationEventUrl": "https://dest.example/changes",
        "requestTimeoutMs": 1000,
        "deliveryRecordTtlSecs": 3600
    });

    let err = serde_json::from_value::<GitHubProjectItemRefreshConfig>(json)
        .expect_err("missing expectedStatusFieldNodeId should fail deserialization");
    assert!(err.to_string().contains("expectedStatusFieldNodeId"));
}

#[test]
fn config_validation_rejects_empty_expected_status_field_node_id() {
    let config = GitHubProjectItemRefreshConfig {
        github_token: "ghp_test_secret".to_string(),
        graphql_url: "https://api.github.com/graphql".to_string(),
        graphql_headers: HashMap::new(),
        allowlisted_project_ids: vec![],
        status_field_name: "Status".to_string(),
        expected_status_field_node_id: "   ".to_string(),
        destination_event_url: "https://dest.example/changes".to_string(),
        destination_bearer_secret: None,
        request_timeout_ms: 1000,
        delivery_record_ttl_secs: 3600,
    };

    let err = config
        .validate(&["query".to_string()], Some(10))
        .expect_err("empty expectedStatusFieldNodeId should fail validation");
    assert!(err.to_string().contains("expectedStatusFieldNodeId"));
}

#[test]
fn config_validation_rejects_invalid_expected_status_field_node_id() {
    let config = GitHubProjectItemRefreshConfig {
        github_token: "ghp_test_secret".to_string(),
        graphql_url: "https://api.github.com/graphql".to_string(),
        graphql_headers: HashMap::new(),
        allowlisted_project_ids: vec![],
        status_field_name: "Status".to_string(),
        expected_status_field_node_id: "PVT_invalid".to_string(),
        destination_event_url: "https://dest.example/changes".to_string(),
        destination_bearer_secret: None,
        request_timeout_ms: 1000,
        delivery_record_ttl_secs: 3600,
    };

    let err = config
        .validate(&["query".to_string()], Some(10))
        .expect_err("invalid expectedStatusFieldNodeId should fail validation");
    assert!(err.to_string().contains("expectedStatusFieldNodeId"));
}

#[test]
fn config_validation_rejects_whitespace_expected_status_field_node_id() {
    let config = GitHubProjectItemRefreshConfig {
        github_token: "ghp_test_secret".to_string(),
        graphql_url: "https://api.github.com/graphql".to_string(),
        graphql_headers: HashMap::new(),
        allowlisted_project_ids: vec![],
        status_field_name: "Status".to_string(),
        expected_status_field_node_id: format!("{EXPECTED_STATUS_FIELD_NODE_ID} bad"),
        destination_event_url: "https://dest.example/changes".to_string(),
        destination_bearer_secret: None,
        request_timeout_ms: 1000,
        delivery_record_ttl_secs: 3600,
    };

    let err = config
        .validate(&["query".to_string()], Some(10))
        .expect_err("whitespace in expectedStatusFieldNodeId should fail validation");
    assert!(err.to_string().contains("expectedStatusFieldNodeId"));
}

#[test]
fn config_validation_accepts_valid_expected_status_field_node_id() {
    let config = GitHubProjectItemRefreshConfig {
        github_token: "ghp_test_secret".to_string(),
        graphql_url: "https://api.github.com/graphql".to_string(),
        graphql_headers: HashMap::new(),
        allowlisted_project_ids: vec![],
        status_field_name: "Status".to_string(),
        expected_status_field_node_id: EXPECTED_STATUS_FIELD_NODE_ID.to_string(),
        destination_event_url: "https://dest.example/changes".to_string(),
        destination_bearer_secret: None,
        request_timeout_ms: 1000,
        delivery_record_ttl_secs: 3600,
    };

    config
        .validate(&["query".to_string()], Some(10))
        .expect("valid expectedStatusFieldNodeId should pass validation");
}

#[tokio::test]
async fn descriptor_resolves_env_expected_status_field_node_id_and_roundtrips() {
    let descriptor = GitHubProjectItemRefreshDescriptor;
    let expected_status_field_expr =
        "${GH_STATUS_FIELD_NODE_ID_TEST:-PVTSSF_lADOCX0YF84BgNE3zhaadbw}";
    let config_json = json!({
        "githubToken": "ghp_test_secret",
        "graphqlUrl": "https://api.github.com/graphql",
        "statusFieldName": "Status",
        "expectedStatusFieldNodeId": expected_status_field_expr,
        "destinationEventUrl": "http://127.0.0.1:9001/sources/github-project-state/events"
    });

    let reaction = descriptor
        .create_reaction(
            "descriptor-roundtrip",
            vec!["q".to_string()],
            &config_json,
            true,
        )
        .await
        .expect("descriptor should resolve config values");
    let properties = reaction.properties();
    assert_eq!(
        properties.get("expectedStatusFieldNodeId"),
        Some(&json!(expected_status_field_expr))
    );
}

#[tokio::test]
async fn descriptor_fails_when_expected_status_field_node_id_env_is_unset_without_default() {
    let descriptor = GitHubProjectItemRefreshDescriptor;
    let config_json = json!({
        "githubToken": "ghp_test_secret",
        "graphqlUrl": "https://api.github.com/graphql",
        "statusFieldName": "Status",
        "expectedStatusFieldNodeId": "${GH_STATUS_FIELD_NODE_ID_TEST_MISSING}",
        "destinationEventUrl": "http://127.0.0.1:9001/sources/github-project-state/events"
    });

    let err = descriptor
        .create_reaction(
            "descriptor-env-missing",
            vec!["q".to_string()],
            &config_json,
            true,
        )
        .await
        .err()
        .expect("missing env var should fail descriptor resolution");
    assert!(err
        .to_string()
        .contains("GH_STATUS_FIELD_NODE_ID_TEST_MISSING"));
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
    let graphql_body: serde_json::Value =
        serde_json::from_slice(&graphql_requests[0].body).expect("graphql request body");
    assert_eq!(
        graphql_body["variables"]["statusFieldName"],
        json!("Status")
    );
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
async fn successful_adds_within_interval_trigger_one_prune_scan() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store = Arc::new(DurableMemoryStateStore::new());

    for (item, updated_at) in [
        ("PVTI_prune_interval_1", "2026-08-13T19:00:00Z"),
        ("PVTI_prune_interval_2", "2026-08-13T19:01:00Z"),
        ("PVTI_prune_interval_3", "2026-08-13T19:02:00Z"),
    ] {
        Mock::given(method("POST"))
            .and(body_string_contains(item))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(graphql_success_response(
                    item,
                    "PVT_project1",
                    updated_at,
                    "In Progress",
                    "opt-ip",
                )),
            )
            .mount(&graphql_server)
            .await;
    }
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
        durable_store.clone(),
        vec![],
    )
    .await;

    for (delivery_id, item_id) in [
        ("delivery-prune-interval-1", "PVTI_prune_interval_1"),
        ("delivery-prune-interval-2", "PVTI_prune_interval_2"),
        ("delivery-prune-interval-3", "PVTI_prune_interval_3"),
    ] {
        let outcome = processor
            .process_add_row(&test_row(delivery_id, item_id, "PVT_project1"))
            .await
            .expect("invalidation should be published");
        assert_eq!(outcome, AddRowOutcome::Published);
    }

    assert_eq!(
        durable_store.list_keys_calls(),
        1,
        "multiple successful ADDs within interval should run one prune scan"
    );
}

#[tokio::test]
async fn concurrent_adds_across_clones_share_prune_throttle() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store = Arc::new(DurableMemoryStateStore::new_with_list_keys_delay(Some(
        StdDuration::from_millis(150),
    )));

    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_prune_clone_1"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_prune_clone_1",
                "PVT_project1",
                "2026-08-13T19:10:00Z",
                "In Progress",
                "opt-ip",
            )),
        )
        .mount(&graphql_server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_prune_clone_2"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_prune_clone_2",
                "PVT_project1",
                "2026-08-13T19:11:00Z",
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

    let (processor, _) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store.clone(),
        vec![],
    )
    .await;
    let processor_one = processor.clone();
    let processor_two = processor;
    let start_barrier = Arc::new(Barrier::new(3));

    let row_one = test_row(
        "delivery-prune-clone-1",
        "PVTI_prune_clone_1",
        "PVT_project1",
    );
    let row_two = test_row(
        "delivery-prune-clone-2",
        "PVTI_prune_clone_2",
        "PVT_project1",
    );

    let barrier_one = Arc::clone(&start_barrier);
    let task_one = tokio::spawn(async move {
        barrier_one.wait().await;
        processor_one.process_add_row(&row_one).await
    });
    let barrier_two = Arc::clone(&start_barrier);
    let task_two = tokio::spawn(async move {
        barrier_two.wait().await;
        processor_two.process_add_row(&row_two).await
    });

    start_barrier.wait().await;
    assert_eq!(
        task_one
            .await
            .expect("task one join")
            .expect("task one result"),
        AddRowOutcome::Published
    );
    assert_eq!(
        task_two
            .await
            .expect("task two join")
            .expect("task two result"),
        AddRowOutcome::Published
    );

    assert_eq!(
        durable_store.list_keys_calls(),
        1,
        "concurrent cloned processors should dedupe prune scans"
    );
}

#[tokio::test]
async fn cancelled_prune_is_immediately_retryable() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store = Arc::new(DurableMemoryStateStore::new_with_list_keys_delay(Some(
        StdDuration::from_secs(5),
    )));

    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_prune_after_cancel"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_prune_after_cancel",
                "PVT_project1",
                "2026-08-13T19:12:00Z",
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

    let (processor, _) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store.clone(),
        vec![],
    )
    .await;
    let cancelled_processor = processor.clone();
    let cancelled = tokio::spawn(async move {
        cancelled_processor
            .process_add_row(&test_row(
                "delivery-prune-cancelled",
                "PVTI_prune_cancelled",
                "PVT_project1",
            ))
            .await
    });

    for _ in 0..100 {
        if durable_store.list_keys_calls() == 1 {
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(5)).await;
    }
    assert_eq!(durable_store.list_keys_calls(), 1, "prune should start");
    durable_store.set_list_keys_delay(None);
    cancelled.abort();
    assert!(cancelled
        .await
        .expect_err("task should be cancelled")
        .is_cancelled());

    let outcome = processor
        .process_add_row(&test_row(
            "delivery-prune-after-cancel",
            "PVTI_prune_after_cancel",
            "PVT_project1",
        ))
        .await
        .expect("next ADD should retry pruning and continue");
    assert_eq!(outcome, AddRowOutcome::Published);
    assert_eq!(
        durable_store.list_keys_calls(),
        2,
        "cancelled prune must not leave the shared throttle stuck"
    );
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
    assert!(err
        .to_string()
        .contains("missing a required 'Status' value"));

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
                status_field_node_id: EXPECTED_STATUS_FIELD_NODE_ID.to_string(),
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
        status_field_node_id: EXPECTED_STATUS_FIELD_NODE_ID.to_string(),
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
        status_field_node_id: EXPECTED_STATUS_FIELD_NODE_ID.to_string(),
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

#[test]
fn rate_limit_delay_prefers_retry_after_and_is_bounded() {
    let mut headers = HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, HeaderValue::from_static("1"));
    headers.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
    headers.insert(
        "x-ratelimit-reset",
        HeaderValue::from_str(&(Utc::now() + Duration::seconds(3)).timestamp().to_string())
            .expect("reset header"),
    );
    assert_eq!(
        rate_limit_retry_after(StatusCode::FORBIDDEN, &headers),
        Some(std::time::Duration::from_secs(1)),
        "Retry-After must take precedence over x-ratelimit-reset"
    );

    headers.insert(
        reqwest::header::RETRY_AFTER,
        HeaderValue::from_static("9999"),
    );
    assert_eq!(
        rate_limit_retry_after(StatusCode::FORBIDDEN, &headers),
        Some(std::time::Duration::from_secs(120)),
        "server-provided delays must be bounded"
    );
}

#[tokio::test]
async fn rate_limit_403_retries_with_retry_after() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_rate_limited"))
        .respond_with(
            ResponseTemplate::new(403)
                .append_header("retry-after", "0")
                .append_header("x-ratelimit-remaining", "0"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&graphql_server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_rate_limited"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_rate_limited",
                "PVT_project1",
                "2026-08-13T19:15:00Z",
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

    let (processor, _) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec![],
    )
    .await;

    let outcome = processor
        .process_add_row(&test_row(
            "delivery-rate-limit",
            "PVTI_rate_limited",
            "PVT_project1",
        ))
        .await
        .expect("rate-limited fetch should retry");
    assert_eq!(outcome, AddRowOutcome::Published);

    let graphql_requests = graphql_server.received_requests().await.expect("requests");
    assert_eq!(graphql_requests.len(), 2, "expected exactly one retry");
}

#[tokio::test]
async fn rate_limit_403_retries_with_retry_after_http_date() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    let retry_after_http_date = httpdate::fmt_http_date(std::time::UNIX_EPOCH);
    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_rate_limited_http_date"))
        .respond_with(
            ResponseTemplate::new(403)
                .append_header("retry-after", retry_after_http_date)
                .append_header("x-ratelimit-remaining", "0"),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&graphql_server)
        .await;
    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_rate_limited_http_date"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_rate_limited_http_date",
                "PVT_project1",
                "2026-08-13T19:16:00Z",
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

    let (processor, _) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec![],
    )
    .await;

    let outcome = processor
        .process_add_row(&test_row(
            "delivery-rate-limit-http-date",
            "PVTI_rate_limited_http_date",
            "PVT_project1",
        ))
        .await
        .expect("http-date retry-after should be honored");
    assert_eq!(outcome, AddRowOutcome::Published);

    let graphql_requests = graphql_server.received_requests().await.expect("requests");
    assert_eq!(graphql_requests.len(), 2, "expected exactly one retry");
}

#[tokio::test]
async fn non_rate_limit_403_does_not_retry() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_forbidden"))
        .respond_with(
            ResponseTemplate::new(403)
                .append_header("x-ratelimit-reset", "9999999999")
                .append_header("x-ratelimit-remaining", "42")
                .set_body_string("forbidden"),
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

    let err = processor
        .process_add_row(&test_row("delivery-403", "PVTI_forbidden", "PVT_project1"))
        .await
        .expect_err("non-rate-limit 403 should be permanent");
    assert!(err.to_string().contains("HTTP 403"));

    let graphql_requests = graphql_server.received_requests().await.expect("requests");
    assert_eq!(
        graphql_requests.len(),
        1,
        "403 without rate-limit headers must not retry"
    );

    let destination_requests = destination_server
        .received_requests()
        .await
        .expect("destination requests");
    assert_eq!(destination_requests.len(), 0);

    let publication = state_store
        .get_publication(&DeliveryKey::new("delivery-403", "PVTI_forbidden"))
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(publication.state, PublicationState::Failed);
}

#[tokio::test]
async fn configurable_status_field_name_is_sent_as_graphql_variable() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_custom_status"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_custom_status",
                "PVT_project1",
                "2026-08-13T19:20:00Z",
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

    let (processor, _) = build_processor_with_status_field_name(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec![],
        "Workflow Status",
    )
    .await;

    let outcome = processor
        .process_add_row(&test_row(
            "delivery-custom-status",
            "PVTI_custom_status",
            "PVT_project1",
        ))
        .await
        .expect("custom status field should still hydrate and publish");
    assert_eq!(outcome, AddRowOutcome::Published);

    let graphql_requests = graphql_server.received_requests().await.expect("requests");
    assert_eq!(graphql_requests.len(), 1);
    let graphql_body: serde_json::Value =
        serde_json::from_slice(&graphql_requests[0].body).expect("graphql request body");
    assert_eq!(
        graphql_body["variables"]["statusFieldName"],
        json!("Workflow Status")
    );
    assert!(
        graphql_body["query"]
            .as_str()
            .expect("query string")
            .contains("$statusFieldName"),
        "query must use statusFieldName variable"
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

#[tokio::test]
async fn row_status_field_node_id_mismatch_blocks_all_network_and_remains_canonical_on_retry() {
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

    let row = json!({
        "InvalidationNodeId": "INV_status_mismatch",
        "DeliveryId": "delivery-status-mismatch",
        "ProjectItemNodeId": "PVTI_status_mismatch",
        "ProjectNodeId": "PVT_project1",
        "StatusFieldNodeId": "PVTSSF_wrong",
        "InvalidatedAt": "2026-08-13T20:14:00Z"
    });
    let err = processor
        .process_add_row(&row)
        .await
        .expect_err("status field mismatch must fail");
    assert!(err.to_string().contains("expectedStatusFieldNodeId"));

    let key = DeliveryKey::new("delivery-status-mismatch", "PVTI_status_mismatch");
    let reservation = state_store
        .get_reservation(&key)
        .await
        .expect("read reservation")
        .expect("reservation");
    assert_eq!(
        reservation.status_field_node_id.as_deref(),
        Some("PVTSSF_wrong")
    );

    let replay_without_constraint = json!({
        "InvalidationNodeId": "INV_status_mismatch",
        "DeliveryId": "delivery-status-mismatch",
        "ProjectItemNodeId": "PVTI_status_mismatch",
        "ProjectNodeId": "PVT_project1",
        "InvalidatedAt": "2026-08-13T20:14:00Z"
    });
    let replay_error = processor
        .process_add_row(&replay_without_constraint)
        .await
        .expect_err("persisted status field constraint must remain authoritative");
    assert!(replay_error
        .to_string()
        .contains("expectedStatusFieldNodeId"));

    let publication = state_store
        .get_publication(&key)
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(publication.state, PublicationState::Failed);

    let destination_requests = destination_server
        .received_requests()
        .await
        .expect("destination requests");
    assert_eq!(destination_requests.len(), 0);

    let graphql_requests = graphql_server
        .received_requests()
        .await
        .expect("graphql requests");
    assert_eq!(
        graphql_requests.len(),
        0,
        "row mismatch must fail before GraphQL"
    );
}

#[tokio::test]
async fn retry_cannot_override_reserved_status_field_with_mismatching_current_row() {
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
    let key = DeliveryKey::new("delivery-retry-mismatch", "PVTI_retry_mismatch");
    state_store
        .set_reservation(
            &key,
            &DeliveryReservation {
                delivery_id: "delivery-retry-mismatch".to_string(),
                project_item_node_id: "PVTI_retry_mismatch".to_string(),
                invalidation_node_id: "INV_retry_mismatch".to_string(),
                project_node_id: Some("PVT_project1".to_string()),
                status_field_node_id: Some(EXPECTED_STATUS_FIELD_NODE_ID.to_string()),
                state_source_url: None,
                webhook_action: Some("edited".to_string()),
                webhook_updated_at: None,
                reserved_at: Utc::now(),
            },
        )
        .await
        .expect("persist reservation");

    let conflicting_retry = json!({
        "invalidationNodeId": "INV_retry_mismatch",
        "deliveryId": "delivery-retry-mismatch",
        "projectItemNodeId": "PVTI_retry_mismatch",
        "projectNodeId": "PVT_project1",
        "statusFieldNodeId": "PVTSSF_wrong",
        "invalidatedAt": "2026-08-13T20:15:00Z"
    });
    let error = processor
        .process_add_row(&conflicting_retry)
        .await
        .expect_err("current row mismatch must fail despite a valid reservation");
    assert!(error.to_string().contains("expectedStatusFieldNodeId"));
    assert!(graphql_server
        .received_requests()
        .await
        .expect("GraphQL requests")
        .is_empty());
    assert!(destination_server
        .received_requests()
        .await
        .expect("destination requests")
        .is_empty());
    let publication = state_store
        .get_publication(&key)
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(publication.state, PublicationState::Failed);
}

#[tokio::test]
async fn graphql_field_id_mismatch_blocks_destination_even_when_row_omits_status_field() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(
            graphql_success_response_with_field_id(
                "PVTI_graphql_mismatch",
                "PVT_project1",
                "2026-08-13T20:15:00Z",
                "In Progress",
                "opt-ip",
                "PVTSSF_different",
            ),
        ))
        .mount(&graphql_server)
        .await;

    let (processor, state_store) = build_processor(
        &graphql_server,
        destination_server.uri(),
        durable_store,
        vec![],
    )
    .await;

    let row = json!({
        "invalidationNodeId": "INV_graphql_mismatch",
        "deliveryId": "delivery-graphql-mismatch",
        "projectItemNodeId": "PVTI_graphql_mismatch",
        "projectNodeId": "PVT_project1",
        "invalidatedAt": "2026-08-13T20:14:00Z"
    });

    let err = processor
        .process_add_row(&row)
        .await
        .expect_err("authoritative GraphQL field mismatch must fail");
    assert!(err.to_string().contains("expectedStatusFieldNodeId"));

    let publication = state_store
        .get_publication(&DeliveryKey::new(
            "delivery-graphql-mismatch",
            "PVTI_graphql_mismatch",
        ))
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(publication.state, PublicationState::Failed);

    let graphql_requests = graphql_server
        .received_requests()
        .await
        .expect("graphql requests");
    assert_eq!(graphql_requests.len(), 1);

    let destination_requests = destination_server
        .received_requests()
        .await
        .expect("destination requests");
    assert_eq!(destination_requests.len(), 0);
}

#[tokio::test]
async fn success_with_canonical_status_field_node_id() {
    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;
    let durable_store: Arc<dyn StateStoreProvider> = Arc::new(DurableMemoryStateStore::new());

    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_status_ok",
                "PVT_project1",
                "2026-08-13T20:16:00Z",
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
        destination_server.uri(),
        durable_store,
        vec![],
    )
    .await;

    let row = json!({
        "invalidationNodeId": "INV_status_ok",
        "deliveryId": "delivery-status-ok",
        "projectItemNodeId": "PVTI_status_ok",
        "projectNodeId": "PVT_project1",
        "statusFieldNodeId": EXPECTED_STATUS_FIELD_NODE_ID,
        "invalidatedAt": "2026-08-13T20:15:30Z"
    });
    let outcome = processor
        .process_add_row(&row)
        .await
        .expect("matching canonical status field id should publish");
    assert_eq!(outcome, AddRowOutcome::Published);

    let publication = state_store
        .get_publication(&DeliveryKey::new("delivery-status-ok", "PVTI_status_ok"))
        .await
        .expect("store read")
        .expect("publication");
    assert_eq!(publication.state, PublicationState::Published);

    assert_eq!(
        destination_server
            .received_requests()
            .await
            .expect("destination requests")
            .len(),
        1
    );
}

#[tokio::test]
async fn state_source_url_mismatch_is_rejected_without_redirect_or_fetch() {
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

    let row = json!({
        "InvalidationNodeId": "INV_url_mismatch",
        "DeliveryId": "delivery-url-mismatch",
        "ProjectItemNodeId": "PVTI_url_mismatch",
        "ProjectNodeId": "PVT_project1",
        "StateSourceUrl": "http://example.invalid/sources/github-project-state/events",
        "InvalidatedAt": "2026-08-13T20:20:00Z"
    });

    let outcome = processor
        .process_add_row(&row)
        .await
        .expect("mismatched state source url should be rejected");
    assert_eq!(outcome, AddRowOutcome::Rejected);

    let publication = state_store
        .get_publication(&DeliveryKey::new(
            "delivery-url-mismatch",
            "PVTI_url_mismatch",
        ))
        .await
        .expect("read publication")
        .expect("publication");
    assert_eq!(publication.state, PublicationState::Rejected);

    let destination_requests = destination_server
        .received_requests()
        .await
        .expect("destination requests");
    assert_eq!(destination_requests.len(), 0);

    let graphql_requests = graphql_server
        .received_requests()
        .await
        .expect("graphql requests");
    assert_eq!(
        graphql_requests.len(),
        0,
        "must reject before GraphQL fetch"
    );

    let malformed_matching_url = json!({
        "InvalidationNodeId": "INV_url_malformed",
        "DeliveryId": "delivery-url-malformed",
        "ProjectItemNodeId": "PVTI_url_malformed",
        "ProjectNodeId": "PVT_project1",
        "StateSourceUrl": "\u{2003}http://127.0.0.1:9001/sources/github-project-state/events",
        "InvalidatedAt": "2026-08-13T20:21:00Z"
    });
    let outcome = processor
        .process_add_row(&malformed_matching_url)
        .await
        .expect("malformed URL should be rejected without network access");
    assert_eq!(outcome, AddRowOutcome::Rejected);
    assert!(graphql_server
        .received_requests()
        .await
        .expect("graphql requests")
        .is_empty());
    assert!(destination_server
        .received_requests()
        .await
        .expect("destination requests")
        .is_empty());
}

#[test]
fn http_source_change_uses_node_contract() {
    let node = ProjectItemStatusNode {
        id: "project-item-status:PVTI_x".to_string(),
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
