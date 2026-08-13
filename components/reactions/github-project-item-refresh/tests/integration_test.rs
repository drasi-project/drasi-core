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
use drasi_lib::state_store::{StateStoreProvider, StateStoreResult};
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_github_project_item_refresh::GitHubProjectItemRefreshReaction;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{body_string_contains, method};
use wiremock::{Mock, MockServer, ResponseTemplate};

const EXPECTED_STATUS_FIELD_NODE_ID: &str = "PVTSSF_lADOCX0YF84BgNE3zhaadbw";

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

fn graphql_success_response(
    project_item_node_id: &str,
    project_node_id: &str,
) -> serde_json::Value {
    json!({
        "data": {
            "node": {
                "__typename": "ProjectV2Item",
                "id": project_item_node_id,
                "updatedAt": "2026-08-13T20:00:00Z",
                "project": { "id": project_node_id },
                "content": { "__typename": "Issue", "id": "I_123" },
                "fieldValueByName": {
                    "__typename": "ProjectV2ItemFieldSingleSelectValue",
                    "name": "In Progress",
                    "optionId": "opt-in-progress",
                    "field": { "id": EXPECTED_STATUS_FIELD_NODE_ID }
                }
            }
        }
    })
}

async fn wait_for_requests(server: &MockServer, expected: usize, timeout_ms: u64) {
    let deadline = tokio::time::Instant::now() + Duration::from_millis(timeout_ms);
    while tokio::time::Instant::now() < deadline {
        let requests = server.received_requests().await.expect("received requests");
        if requests.len() >= expected {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let requests = server.received_requests().await.expect("received requests");
    panic!(
        "timed out waiting for {expected} requests (got {})",
        requests.len()
    );
}

#[tokio::test]
#[ignore]
async fn github_project_item_refresh_end_to_end_add_update_delete() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .is_test(true)
        .try_init();

    let graphql_server = MockServer::start().await;
    let destination_server = MockServer::start().await;

    Mock::given(method("POST"))
        .and(body_string_contains("PVTI_e2e_item"))
        .and(body_string_contains("\"statusFieldName\""))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(graphql_success_response(
                "PVTI_e2e_item",
                "PVT_allowlisted_project",
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

    let (source, handle) = ApplicationSource::new(
        "invalidation-source",
        ApplicationSourceConfig {
            properties: HashMap::new(),
            durability: None,
        },
    )
    .expect("create source");

    let query = Query::cypher("invalidation-query")
        .query(
            "MATCH (n:ProjectItemInvalidation)
             RETURN n.InvalidationNodeId AS InvalidationNodeId,
                    n.DeliveryId AS DeliveryId,
                    n.ProjectItemNodeId AS ProjectItemNodeId,
                    n.ProjectNodeId AS ProjectNodeId,
                    n.StatusFieldNodeId AS StatusFieldNodeId,
                    n.StateSourceUrl AS StateSourceUrl,
                    n.webhookAction AS webhookAction,
                    n.InvalidatedAt AS InvalidatedAt",
        )
        .from_source("invalidation-source")
        .auto_start(true)
        .build();

    let reaction = GitHubProjectItemRefreshReaction::builder("gh-refresh-e2e")
        .with_query("invalidation-query")
        .with_github_token("ghp_test_secret")
        .with_graphql_url(graphql_server.uri())
        .with_destination_event_url(destination_server.uri())
        .with_destination_bearer_secret("dest-secret")
        .with_allowlisted_project_ids(vec!["PVT_allowlisted_project".to_string()])
        .with_expected_status_field_node_id(EXPECTED_STATUS_FIELD_NODE_ID)
        .build()
        .expect("build reaction");

    let drasi = DrasiLib::builder()
        .with_id("gh-refresh-integration")
        .with_state_store_provider(Arc::new(DurableMemoryStateStore::new()))
        .with_source(source)
        .with_query(query)
        .with_reaction(reaction)
        .build()
        .await
        .expect("build drasi");

    drasi.start().await.expect("start drasi");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // INSERT -> should trigger fetch + publish
    handle
        .send_node_insert(
            "inv-1",
            vec!["ProjectItemInvalidation"],
            PropertyMapBuilder::new()
                .with_string("InvalidationNodeId", "INV_e2e_1")
                .with_string("DeliveryId", "delivery-e2e-1")
                .with_string("ProjectItemNodeId", "PVTI_e2e_item")
                .with_string("ProjectNodeId", "PVT_allowlisted_project")
                .with_string("StatusFieldNodeId", EXPECTED_STATUS_FIELD_NODE_ID)
                .with_string("StateSourceUrl", destination_server.uri())
                .with_string("webhookAction", "edited")
                .with_string("invalidatedAt", "2026-08-13T19:59:00Z")
                .build(),
        )
        .await
        .expect("send insert");
    wait_for_requests(&destination_server, 1, 5_000).await;

    // UPDATE -> should be ignored by the reaction.
    handle
        .send_node_update(
            "inv-1",
            vec!["ProjectItemInvalidation"],
            PropertyMapBuilder::new()
                .with_string("InvalidationNodeId", "INV_e2e_1")
                .with_string("DeliveryId", "delivery-e2e-1")
                .with_string("ProjectItemNodeId", "PVTI_e2e_item")
                .with_string("ProjectNodeId", "PVT_allowlisted_project")
                .with_string("StatusFieldNodeId", EXPECTED_STATUS_FIELD_NODE_ID)
                .with_string("StateSourceUrl", destination_server.uri())
                .with_string("webhookAction", "edited")
                .with_string("invalidatedAt", "2026-08-13T20:00:01Z")
                .build(),
        )
        .await
        .expect("send update");

    // DELETE -> should be ignored by the reaction.
    handle
        .send_delete("inv-1", vec!["ProjectItemInvalidation"])
        .await
        .expect("send delete");

    tokio::time::sleep(Duration::from_millis(500)).await;
    let destination_requests = destination_server
        .received_requests()
        .await
        .expect("destination requests");
    assert_eq!(
        destination_requests.len(),
        1,
        "only ADD rows should republish to destination source"
    );

    let graphql_requests = graphql_server
        .received_requests()
        .await
        .expect("graphql requests");
    assert_eq!(graphql_requests.len(), 1, "update/delete must not refetch");
    let graphql_body: serde_json::Value =
        serde_json::from_slice(&graphql_requests[0].body).expect("graphql request json");
    assert_eq!(graphql_body["variables"]["statusFieldName"], "Status");

    let body: serde_json::Value =
        serde_json::from_slice(&destination_requests[0].body).expect("destination body json");
    assert_eq!(body["operation"], "update");
    assert_eq!(
        body["element"]["id"], "project-item-status:PVTI_e2e_item",
        "deterministic ProjectItemStatus node id"
    );
    assert_eq!(
        body["element"]["properties"]["triggeringDeliveryId"],
        "delivery-e2e-1"
    );

    drasi.stop().await.expect("stop drasi");
}
