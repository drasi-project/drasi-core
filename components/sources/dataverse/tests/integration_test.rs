// Copyright 2025 The Drasi Authors.
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

//! Integration test for the Dataverse source using wiremock to simulate the
//! Dataverse OData Web API. This is a client harness test since we cannot run
//! a real Dataverse instance in a container.
//!
//! The test simulates the full change tracking lifecycle:
//! 1. OAuth2 token acquisition
//! 2. Initial change tracking request (get delta token)
//! 3. Polling with delta token → INSERT detected
//! 4. Polling with delta token → UPDATE detected
//! 5. Polling with delta token → DELETE detected

#[cfg(test)]
mod integration_tests {
    use drasi_lib::builder::Query;
    use drasi_lib::channels::ResultDiff;
    use drasi_lib::DrasiLib;
    use drasi_reaction_application::subscription::SubscriptionOptions;
    use drasi_reaction_application::ApplicationReaction;
    use drasi_source_dataverse::DataverseSource;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;
    use wiremock::matchers::{header, method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const SOURCE_ID: &str = "test-dv-source";
    const QUERY_ID: &str = "test-dv-query";

    /// Helper: Create a mock OAuth2 token response
    fn token_response() -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "token_type": "Bearer",
            "expires_in": 3600,
            "access_token": "mock-access-token-12345"
        }))
    }

    /// Helper: Create an OData response with delta link (no data, initial tracking)
    fn empty_delta_response(mock_server_uri: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{mock_server_uri}/api/data/v9.2/$metadata#accounts"),
            "value": [],
            "@odata.deltaLink": format!("{mock_server_uri}/api/data/v9.2/accounts?$deltatoken=initial-token-1")
        }))
    }

    /// Helper: Create an OData delta response with a new/updated record
    fn insert_delta_response(mock_server_uri: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{mock_server_uri}/api/data/v9.2/$metadata#accounts"),
            "value": [
                {
                    "@odata.etag": "W/\"1001\"",
                    "accountid": "aaaa-bbbb-cccc-0001",
                    "name": "Contoso Ltd",
                    "revenue": 5000000.0,
                    "statecode": 0
                }
            ],
            "@odata.deltaLink": format!("{mock_server_uri}/api/data/v9.2/accounts?$deltatoken=after-insert-token-2")
        }))
    }

    /// Helper: Create an OData delta response with an updated record
    fn update_delta_response(mock_server_uri: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{mock_server_uri}/api/data/v9.2/$metadata#accounts"),
            "value": [
                {
                    "@odata.etag": "W/\"1002\"",
                    "accountid": "aaaa-bbbb-cccc-0001",
                    "name": "Contoso Updated",
                    "revenue": 7500000.0,
                    "statecode": 0
                }
            ],
            "@odata.deltaLink": format!("{mock_server_uri}/api/data/v9.2/accounts?$deltatoken=after-update-token-3")
        }))
    }

    /// Helper: Create an OData delta response with a deleted record
    fn delete_delta_response(mock_server_uri: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{mock_server_uri}/api/data/v9.2/$metadata#accounts"),
            "value": [
                {
                    "@odata.context": format!("{mock_server_uri}/api/data/v9.2/$metadata#accounts/$deletedEntity"),
                    "id": "aaaa-bbbb-cccc-0001",
                    "reason": "deleted"
                }
            ],
            "@odata.deltaLink": format!("{mock_server_uri}/api/data/v9.2/accounts?$deltatoken=after-delete-token-4")
        }))
    }

    /// Helper: Create an empty delta response (no changes)
    fn no_changes_response(mock_server_uri: &str, token: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "@odata.context": format!("{mock_server_uri}/api/data/v9.2/$metadata#accounts"),
            "value": [],
            "@odata.deltaLink": format!("{mock_server_uri}/api/data/v9.2/accounts?$deltatoken={token}")
        }))
    }

    /// Full integration test: CREATE → UPDATE → DELETE via wiremock client harness
    ///
    /// This test simulates the Dataverse Web API using wiremock and verifies that
    /// the source detects INSERT, UPDATE, and DELETE changes flowing through to
    /// an ApplicationReaction.
    #[tokio::test]
    #[ignore] // Run with: cargo test -p drasi-source-dataverse -- --ignored --nocapture
    async fn test_dataverse_change_detection_end_to_end() {
        // 1. Start mock server
        let mock_server = MockServer::start().await;
        let mock_uri = mock_server.uri();

        // Track how many times the delta endpoint is called to sequence responses
        let delta_call_count = Arc::new(AtomicUsize::new(0));
        let delta_count_clone = delta_call_count.clone();

        // 2. Mount OAuth2 token mock (always available)
        Mock::given(method("POST"))
            .and(path_regex(r".*/oauth2/v2.0/token"))
            .respond_with(token_response())
            .expect(1..)
            .mount(&mock_server)
            .await;

        // 3. Mount initial change tracking request (GET /api/data/v9.2/accounts with Prefer header)
        let mock_uri_for_initial = mock_uri.clone();
        Mock::given(method("GET"))
            .and(path("/api/data/v9.2/accounts"))
            .and(header("Prefer", "odata.track-changes"))
            .respond_with(empty_delta_response(&mock_uri_for_initial))
            .expect(1)
            .mount(&mock_server)
            .await;

        // 4. Mount delta link endpoint - returns sequenced responses
        // We use a custom responder that returns different responses based on call count
        let mock_uri_for_delta = mock_uri.clone();
        let mock_uri_for_delta2 = mock_uri.clone();
        let mock_uri_for_delta3 = mock_uri.clone();
        let mock_uri_for_delta4 = mock_uri.clone();

        // Since wiremock doesn't natively support stateful responses easily,
        // we'll mount multiple mocks with different deltatoken query params.
        // The source follows delta links, so each response directs to the next token.

        // Initial token → returns INSERT
        Mock::given(method("GET"))
            .and(path_regex(r".*/accounts\?.*deltatoken=initial-token-1"))
            .respond_with(insert_delta_response(&mock_uri_for_delta))
            .expect(1)
            .mount(&mock_server)
            .await;

        // After-insert token → returns UPDATE
        Mock::given(method("GET"))
            .and(path_regex(
                r".*/accounts\?.*deltatoken=after-insert-token-2",
            ))
            .respond_with(update_delta_response(&mock_uri_for_delta2))
            .expect(1)
            .mount(&mock_server)
            .await;

        // After-update token → returns DELETE
        Mock::given(method("GET"))
            .and(path_regex(
                r".*/accounts\?.*deltatoken=after-update-token-3",
            ))
            .respond_with(delete_delta_response(&mock_uri_for_delta3))
            .expect(1)
            .mount(&mock_server)
            .await;

        // After-delete token → returns no changes (stabilize)
        Mock::given(method("GET"))
            .and(path_regex(
                r".*/accounts\?.*deltatoken=after-delete-token-4",
            ))
            .respond_with(no_changes_response(
                &mock_uri_for_delta4,
                "after-delete-token-4",
            ))
            .expect(0..)
            .mount(&mock_server)
            .await;

        // 5. Create Dataverse source pointing to mock server
        let source = DataverseSource::builder(SOURCE_ID)
            .with_environment_url(&mock_uri)
            .with_tenant_id("mock-tenant-id")
            .with_client_id("mock-client-id")
            .with_client_secret("mock-client-secret")
            .with_entities(vec!["account".to_string()])
            .with_min_interval_ms(100) // Fast polling for test
            .with_max_interval_seconds(1)
            .with_auto_start(true)
            .build()
            .expect("Failed to build DataverseSource");

        // 6. Create query
        let query = Query::cypher(QUERY_ID)
            .query(
                r#"
                MATCH (n:account)
                RETURN n.accountid AS accountid, n.name AS name, n.revenue AS revenue
            "#,
            )
            .from_source(SOURCE_ID)
            .auto_start(true)
            .enable_bootstrap(false) // No bootstrap - we test CDC only
            .build();

        // 7. Create application reaction to capture results
        let (reaction, handle) = ApplicationReaction::builder("test-reaction")
            .with_query(QUERY_ID)
            .build();

        // 8. Build and start DrasiLib
        let drasi = DrasiLib::builder()
            .with_id("dataverse-integration-test")
            .with_source(source)
            .with_query(query)
            .with_reaction(reaction)
            .build()
            .await
            .expect("Failed to build DrasiLib");

        drasi.start().await.expect("Failed to start DrasiLib");

        // 9. Create subscription
        let mut subscription = handle
            .subscribe_with_options(
                SubscriptionOptions::default().with_timeout(Duration::from_secs(2)),
            )
            .await
            .expect("Failed to subscribe");

        // Wait for source to start and do initial delta token acquisition
        tokio::time::sleep(Duration::from_secs(1)).await;

        // 10. Collect results - expecting INSERT, UPDATE, DELETE
        let mut found_insert = false;
        let mut found_update = false;
        let mut found_delete = false;

        // Keep collecting for up to 15 seconds
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);

        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_secs(2), subscription.recv()).await {
                Ok(Some(result)) => {
                    println!(
                        "[TEST] Got QueryResult for query '{}' with {} diffs",
                        result.query_id,
                        result.results.len()
                    );

                    for diff in &result.results {
                        match diff {
                            ResultDiff::Add { data } => {
                                println!("[TEST] ADD: {data}");
                                if let Some(name) = data.get("name").and_then(|v| v.as_str()) {
                                    if name == "Contoso Ltd" {
                                        found_insert = true;
                                        println!("[TEST] ✓ INSERT detected: Contoso Ltd");
                                    }
                                }
                            }
                            ResultDiff::Update {
                                before,
                                after,
                                data,
                                ..
                            } => {
                                println!("[TEST] UPDATE: before={before}, after={after}");
                                if let Some(name) = after.get("name").and_then(|v| v.as_str()) {
                                    if name == "Contoso Updated" {
                                        found_update = true;
                                        println!("[TEST] ✓ UPDATE detected: Contoso Updated");
                                    }
                                }
                            }
                            ResultDiff::Delete { data } => {
                                println!("[TEST] DELETE: {data}");
                                found_delete = true;
                                println!("[TEST] ✓ DELETE detected");
                            }
                            _ => {
                                println!("[TEST] Other diff: {diff:?}");
                            }
                        }
                    }

                    if found_insert && found_update && found_delete {
                        println!("[TEST] All changes detected!");
                        break;
                    }
                }
                Ok(None) => {
                    println!("[TEST] Subscription ended");
                    break;
                }
                Err(_) => {
                    // Timeout on recv - continue polling
                    println!("[TEST] Recv timeout, continuing...");
                }
            }
        }

        // 11. Stop DrasiLib
        drasi.stop().await.expect("Failed to stop DrasiLib");

        // 12. Assert all changes were detected
        assert!(
            found_insert,
            "INSERT was not detected! The source did not emit the new account record."
        );
        assert!(
            found_update,
            "UPDATE was not detected! The source did not emit the updated account record."
        );
        assert!(
            found_delete,
            "DELETE was not detected! The source did not emit the deleted account record."
        );

        println!("[TEST] ✓ All assertions passed - INSERT, UPDATE, DELETE detected.");
    }
}
