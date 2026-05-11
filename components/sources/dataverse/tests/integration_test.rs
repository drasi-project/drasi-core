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

//! Integration tests for the Dataverse source's HTTP client + change parsing
//! pipeline using `wiremock`.
//!
//! These tests stand the OData client against an in-process mock server and
//! drive the same call sequence the polling worker performs:
//!
//! 1. Initial change tracking request → returns a `@odata.deltaLink`
//! 2. Follow delta link → INSERT detected
//! 3. Follow next delta link → UPDATE detected
//! 4. Follow next delta link → DELETE detected
//! 5. Follow next delta link → no changes (loop stabilizes)
//!
//! This avoids depending on the full `DrasiLib` runtime (sources, queries,
//! reactions, dispatchers) so the test can run reliably in CI.

use async_trait::async_trait;
use drasi_lib::identity::{CredentialContext, Credentials, IdentityProvider};
use drasi_source_dataverse::client::DataverseClient;
use drasi_source_dataverse::types::{parse_delta_changes, DataverseChange};
use serde_json::json;
use std::sync::Arc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Identity provider that returns a static token so the client can focus on
/// HTTP behaviour without exercising a real OAuth2 flow.
struct StaticToken;

#[async_trait]
impl IdentityProvider for StaticToken {
    async fn get_credentials(&self, _ctx: &CredentialContext) -> anyhow::Result<Credentials> {
        Ok(Credentials::Token {
            username: "test".to_string(),
            token: "mock-access-token-12345".to_string(),
        })
    }

    fn clone_box(&self) -> Box<dyn IdentityProvider> {
        Box::new(StaticToken)
    }
}

/// Drive the full INSERT → UPDATE → DELETE delta sequence against a mock
/// Dataverse Web API and verify that each change is parsed correctly.
#[tokio::test]
async fn dataverse_client_full_change_lifecycle() {
    let server = MockServer::start().await;
    let uri = server.uri();

    // 1. Initial change tracking request: empty value, returns an initial delta token.
    Mock::given(method("GET"))
        .and(path("/api/data/v9.2/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{uri}/api/data/v9.2/$metadata#accounts"),
            "value": [],
            "@odata.deltaLink":
                format!("{uri}/api/data/v9.2/accounts?$deltatoken=after-initial")
        })))
        .mount(&server)
        .await;

    let client = DataverseClient::new(&uri, "v9.2", Arc::new(StaticToken));

    // Step 1: initial change tracking → expect a delta link, no changes.
    let initial = client
        .initial_change_tracking("accounts", None)
        .await
        .expect("initial change tracking should succeed");
    assert!(initial.value.is_empty(), "no records on initial");
    let initial_delta = initial
        .delta_link
        .clone()
        .expect("initial delta link must be present");

    // 2. Re-mount the path with INSERT response. Because every call hits the
    //    same path, we replace the mock for each subsequent stage by resetting
    //    the server.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/data/v9.2/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{uri}/api/data/v9.2/$metadata#accounts"),
            "value": [{
                "@odata.etag": "W/\"1001\"",
                "accountid": "aaaa-bbbb-cccc-0001",
                "name": "Contoso Ltd",
                "revenue": 5_000_000.0,
                "statecode": 0
            }],
            "@odata.deltaLink":
                format!("{uri}/api/data/v9.2/accounts?$deltatoken=after-insert")
        })))
        .mount(&server)
        .await;

    // Step 2: follow delta link → INSERT detected.
    let after_insert = client
        .follow_delta_link(&initial_delta)
        .await
        .expect("follow_delta_link should succeed");
    let changes = parse_delta_changes(&after_insert, "account");
    assert_eq!(changes.len(), 1, "exactly one INSERT change expected");
    match &changes[0] {
        DataverseChange::NewOrUpdated {
            id,
            entity_name,
            attributes,
        } => {
            assert_eq!(id, "aaaa-bbbb-cccc-0001");
            assert_eq!(entity_name, "account");
            assert_eq!(
                attributes.get("name").and_then(|v| v.as_str()),
                Some("Contoso Ltd")
            );
        }
        other => panic!("expected NewOrUpdated, got {other:?}"),
    }
    let insert_delta = after_insert
        .delta_link
        .clone()
        .expect("delta link after insert");

    // 3. UPDATE response.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/data/v9.2/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{uri}/api/data/v9.2/$metadata#accounts"),
            "value": [{
                "@odata.etag": "W/\"1002\"",
                "accountid": "aaaa-bbbb-cccc-0001",
                "name": "Contoso Updated",
                "revenue": 7_500_000.0,
                "statecode": 0
            }],
            "@odata.deltaLink":
                format!("{uri}/api/data/v9.2/accounts?$deltatoken=after-update")
        })))
        .mount(&server)
        .await;

    let after_update = client
        .follow_delta_link(&insert_delta)
        .await
        .expect("follow_delta_link for update should succeed");
    let changes = parse_delta_changes(&after_update, "account");
    assert_eq!(changes.len(), 1);
    match &changes[0] {
        DataverseChange::NewOrUpdated { attributes, .. } => {
            assert_eq!(
                attributes.get("name").and_then(|v| v.as_str()),
                Some("Contoso Updated")
            );
        }
        other => panic!("expected NewOrUpdated for update, got {other:?}"),
    }
    let update_delta = after_update
        .delta_link
        .clone()
        .expect("delta link after update");

    // 4. DELETE response (entry with `$deletedEntity` context).
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/data/v9.2/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{uri}/api/data/v9.2/$metadata#accounts"),
            "value": [{
                "@odata.context": format!(
                    "{uri}/api/data/v9.2/$metadata#accounts/$deletedEntity"
                ),
                "id": "aaaa-bbbb-cccc-0001",
                "reason": "deleted"
            }],
            "@odata.deltaLink":
                format!("{uri}/api/data/v9.2/accounts?$deltatoken=after-delete")
        })))
        .mount(&server)
        .await;

    let after_delete = client
        .follow_delta_link(&update_delta)
        .await
        .expect("follow_delta_link for delete should succeed");
    let changes = parse_delta_changes(&after_delete, "account");
    assert_eq!(changes.len(), 1, "exactly one DELETE change expected");
    match &changes[0] {
        DataverseChange::Deleted { id, entity_name } => {
            assert_eq!(id, "aaaa-bbbb-cccc-0001");
            assert_eq!(entity_name, "account");
        }
        other => panic!("expected Deleted, got {other:?}"),
    }
    let delete_delta = after_delete
        .delta_link
        .clone()
        .expect("delta link after delete");

    // 5. Final no-changes poll: the polling loop must stabilize without
    //    emitting spurious changes.
    server.reset().await;
    Mock::given(method("GET"))
        .and(path("/api/data/v9.2/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{uri}/api/data/v9.2/$metadata#accounts"),
            "value": [],
            "@odata.deltaLink":
                format!("{uri}/api/data/v9.2/accounts?$deltatoken=after-delete")
        })))
        .mount(&server)
        .await;

    let stable = client
        .follow_delta_link(&delete_delta)
        .await
        .expect("stabilizing poll should succeed");
    let changes = parse_delta_changes(&stable, "account");
    assert!(
        changes.is_empty(),
        "no changes expected on stabilization, got {} changes",
        changes.len()
    );
}

/// Verify that the client correctly walks through a multi-page initial
/// response via `@odata.nextLink`, accumulating records from every page
/// and returning the final delta token.
#[tokio::test]
async fn dataverse_client_paginates_initial_change_tracking() {
    let server = MockServer::start().await;
    let uri = server.uri();

    // Page 1: returns nextLink to a `/page2` URL on the same server.
    Mock::given(method("GET"))
        .and(path("/api/data/v9.2/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{uri}/api/data/v9.2/$metadata#accounts"),
            "value": [
                { "accountid": "id-1", "name": "One" },
                { "accountid": "id-2", "name": "Two" }
            ],
            "@odata.nextLink": format!("{uri}/api/data/v9.2/accounts/page2")
        })))
        .mount(&server)
        .await;

    // Page 2: returns the final delta link, no nextLink.
    Mock::given(method("GET"))
        .and(path("/api/data/v9.2/accounts/page2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{uri}/api/data/v9.2/$metadata#accounts"),
            "value": [
                { "accountid": "id-3", "name": "Three" }
            ],
            "@odata.deltaLink":
                format!("{uri}/api/data/v9.2/accounts?$deltatoken=final")
        })))
        .mount(&server)
        .await;

    let client = DataverseClient::new(&uri, "v9.2", Arc::new(StaticToken));

    let page1 = client
        .initial_change_tracking("accounts", None)
        .await
        .expect("page 1 should succeed");
    assert_eq!(page1.value.len(), 2);
    assert!(page1.delta_link.is_none());
    let next = page1.next_link.expect("page 1 must hand off via nextLink");

    let page2 = client
        .follow_next_link(&next)
        .await
        .expect("page 2 should succeed");
    assert_eq!(page2.value.len(), 1);
    assert!(page2.next_link.is_none());
    let final_delta = page2.delta_link.expect("final delta link required");
    assert!(final_delta.contains("$deltatoken=final"));
}
