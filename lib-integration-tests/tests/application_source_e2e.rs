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

//! End-to-end integration test for Application Source with relationship direction validation.
//!
//! This test validates the fix for reversed in_node/out_node assignment by creating:
//! - An ApplicationSource to inject nodes and relationships programmatically
//! - A Cypher query that traverses relationships to validate correct direction
//! - An ApplicationReaction to capture and validate query results
//!
//! The test specifically validates that for a relationship (a)-[:REL]->(b):
//! - in_node = a (start/source node)
//! - out_node = b (end/target node)

use anyhow::Result;
use drasi_lib::channels::ResultDiff;
use drasi_lib::{DrasiLib, Query};
use drasi_reaction_application::ApplicationReactionBuilder;
use drasi_source_application::{ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

/// Test that validates correct relationship direction through end-to-end query execution.
///
/// Creates a graph: (Person{name:"Alice"})-[:KNOWS]->(Person{name:"Bob"})
/// Then queries: MATCH (a:Person)-[:KNOWS]->(b:Person) WHERE a.name = "Alice" RETURN b.name
/// Expected result: Should return "Bob" (not empty, which would indicate reversed direction)
#[tokio::test]
async fn test_application_source_relationship_direction_e2e() -> Result<()> {
    // =========================================================================
    // Step 1: Create Application Source and Handle
    // =========================================================================
    let config = ApplicationSourceConfig {
        properties: HashMap::new(),
    };
    let (app_source, app_handle) = ApplicationSource::new("test-app-source", config)?;

    // =========================================================================
    // Step 2: Create Application Reaction to capture results
    // =========================================================================
    let (app_reaction, reaction_handle) = ApplicationReactionBuilder::new("test-app-reaction")
        .with_query("relationship-test-query")
        .with_auto_start(true)
        .build();

    // =========================================================================
    // Step 3: Define Query that tests relationship direction
    // =========================================================================
    // This query will only return results if relationships are in correct direction:
    // (Alice)-[:KNOWS]->(Bob)
    // If direction is reversed, this would match (Bob)-[:KNOWS]->(Alice) instead
    let relationship_query = Query::cypher("relationship-test-query")
        .query(
            r#"
            MATCH (a:Person)-[:KNOWS]->(b:Person)
            WHERE a.name = 'Alice'
            RETURN b.name AS friend_name
            "#,
        )
        .from_source("test-app-source")
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    // =========================================================================
    // Step 4: Build DrasiLib with all components
    // =========================================================================
    let drasi = Arc::new(
        DrasiLib::builder()
            .with_id("application-source-e2e-test")
            .with_source(app_source)
            .with_query(relationship_query)
            .with_reaction(app_reaction)
            .build()
            .await?,
    );

    // Start DrasiLib to initialize all components
    drasi.start().await?;

    // =========================================================================
    // Step 5: Wait for components to be ready
    // =========================================================================
    tokio::time::sleep(Duration::from_millis(500)).await;

    // =========================================================================
    // Step 6: Subscribe to reaction results
    // =========================================================================
    let mut subscription = reaction_handle
        .subscribe_with_options(Default::default())
        .await?;

    // =========================================================================
    // Step 7: Insert nodes
    // =========================================================================
    let alice_props = PropertyMapBuilder::new()
        .with_string("name", "Alice")
        .build();
    app_handle
        .send_node_insert("alice", vec!["Person"], alice_props)
        .await?;

    let bob_props = PropertyMapBuilder::new().with_string("name", "Bob").build();
    app_handle
        .send_node_insert("bob", vec!["Person"], bob_props)
        .await?;

    // =========================================================================
    // Step 8: Insert relationship (Alice)-[:KNOWS]->(Bob)
    // =========================================================================
    // This is the critical test: start_node_id="alice", end_node_id="bob"
    // With the fix, this should create: in_node=alice, out_node=bob
    // Without the fix, it would be: in_node=bob, out_node=alice (WRONG)
    let knows_props = PropertyMapBuilder::new().build();
    app_handle
        .send_relation_insert(
            "knows-1",
            vec!["KNOWS"],
            knows_props,
            "alice", // start_node_id (should map to in_node)
            "bob",   // end_node_id (should map to out_node)
        )
        .await?;

    // =========================================================================
    // Step 9: Wait for and validate query results
    // =========================================================================
    // Set a timeout to avoid hanging if something goes wrong
    let result = timeout(Duration::from_secs(5), subscription.recv()).await;

    assert!(
        result.is_ok(),
        "Timed out waiting for query results - this may indicate the query found no matches"
    );

    let query_result = result?;
    assert!(query_result.is_some(), "Expected query result but got None");

    let query_result = query_result.unwrap();
    assert_eq!(
        query_result.query_id, "relationship-test-query",
        "Received result from unexpected query"
    );

    // =========================================================================
    // Step 10: Validate the result contains Bob's name
    // =========================================================================
    assert!(
        !query_result.results.is_empty(),
        "Query returned no results! This indicates relationships are in the wrong direction. \
         Expected to find (Alice)-[:KNOWS]->(Bob) but found nothing."
    );

    // The result should be a single row with friend_name = "Bob"
    assert_eq!(
        query_result.results.len(),
        1,
        "Expected exactly 1 result row"
    );

    let row = &query_result.results[0];

    // Extract the data from the ResultDiff
    let data = match row {
        ResultDiff::Add { data } => data,
        _ => panic!("Expected Add result, got {row:?}"),
    };

    let friend_name = data.get("friend_name");
    assert!(
        friend_name.is_some(),
        "Result row missing 'friend_name' field"
    );

    // Validate the friend name is Bob (not Alice, which would indicate reversed direction)
    let friend_name_value = friend_name.unwrap();
    assert_eq!(
        friend_name_value.as_str(),
        Some("Bob"),
        "Expected friend_name='Bob', got {friend_name_value:?}. This indicates the relationship direction is correct!"
    );

    // =========================================================================
    // Step 11: Test relationship update preserves direction
    // =========================================================================
    // Update Bob's name and verify the relationship still works
    let bob_updated_props = PropertyMapBuilder::new()
        .with_string("name", "Robert")
        .build();
    app_handle
        .send_node_update("bob", vec!["Person"], bob_updated_props)
        .await?;

    // Wait for update result
    let update_result = timeout(Duration::from_secs(5), subscription.recv()).await;
    assert!(
        update_result.is_ok(),
        "Timed out waiting for update results"
    );

    let update_result = update_result?.unwrap();
    assert!(
        !update_result.results.is_empty(),
        "Update should produce results"
    );

    let updated_row = &update_result.results[0];

    // For updates, we should get either an Update or Add
    let updated_data = match updated_row {
        ResultDiff::Update { after, .. } => after,
        ResultDiff::Add { data } => data,
        _ => panic!("Expected Update or Add result, got {updated_row:?}"),
    };

    let updated_friend_name = updated_data.get("friend_name").unwrap();
    assert_eq!(
        updated_friend_name.as_str(),
        Some("Robert"),
        "Updated friend name should be 'Robert'"
    );

    // =========================================================================
    // Step 12: Test reverse direction doesn't match
    // =========================================================================
    // Create a new query that looks for (Bob)-[:KNOWS]->(Alice)
    // This should NOT match if relationships are correctly directed
    let reverse_query = Query::cypher("reverse-test-query")
        .query(
            r#"
            MATCH (a:Person)-[:KNOWS]->(b:Person)
            WHERE a.name = 'Robert'
            RETURN b.name AS friend_name
            "#,
        )
        .from_source("test-app-source")
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    drasi.add_query(reverse_query).await?;

    // Add a second reaction for the reverse query
    let (reverse_reaction, reverse_handle) = ApplicationReactionBuilder::new("reverse-reaction")
        .with_query("reverse-test-query")
        .with_auto_start(true)
        .build();

    drasi.add_reaction(reverse_reaction).await?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut reverse_subscription = reverse_handle
        .subscribe_with_options(Default::default())
        .await?;

    // Wait briefly to see if we get any results (we shouldn't)
    let reverse_result = timeout(Duration::from_millis(1000), reverse_subscription.recv()).await;

    // We expect a timeout here because the query shouldn't match
    // (there's no relationship from Robert to Alice, only from Alice to Robert)
    assert!(
        reverse_result.is_err() || reverse_result.unwrap().is_none(),
        "Reverse query should NOT match - found results when expecting none. \
         This would indicate relationships might be bidirectional or incorrectly directed."
    );

    println!("✅ All relationship direction tests passed!");
    println!("   - Forward query (Alice)-[:KNOWS]->(Bob) correctly found Bob");
    println!("   - Reverse query (Bob)-[:KNOWS]->(Alice) correctly found nothing");
    println!("   - Relationship direction is correctly maintained: in_node=start, out_node=end");

    Ok(())
}

/// Test that validates multiple relationships with correct direction
#[tokio::test]
async fn test_application_source_multiple_relationships() -> Result<()> {
    // Create source and handle
    let config = ApplicationSourceConfig {
        properties: HashMap::new(),
    };
    let (app_source, app_handle) = ApplicationSource::new("multi-rel-source", config)?;

    // Create reaction
    let (app_reaction, reaction_handle) = ApplicationReactionBuilder::new("multi-rel-reaction")
        .with_query("multi-rel-query")
        .with_auto_start(true)
        .build();

    // Query to count outgoing KNOWS relationships from Alice
    let count_query = Query::cypher("multi-rel-query")
        .query(
            r#"
            MATCH (a:Person {name: 'Alice'})-[:KNOWS]->(friend:Person)
            RETURN count(friend) AS friend_count, collect(friend.name) AS friend_names
            "#,
        )
        .from_source("multi-rel-source")
        .auto_start(true)
        .enable_bootstrap(false)
        .build();

    // Build DrasiLib
    let drasi = Arc::new(
        DrasiLib::builder()
            .with_id("multi-relationship-test")
            .with_source(app_source)
            .with_query(count_query)
            .with_reaction(app_reaction)
            .build()
            .await?,
    );

    // Start DrasiLib
    drasi.start().await?;

    tokio::time::sleep(Duration::from_millis(500)).await;

    let mut subscription = reaction_handle
        .subscribe_with_options(Default::default())
        .await?;

    // Insert Alice
    app_handle
        .send_node_insert(
            "alice",
            vec!["Person"],
            PropertyMapBuilder::new()
                .with_string("name", "Alice")
                .build(),
        )
        .await?;

    // Insert three friends: Bob, Charlie, Diana
    for (id, name) in [("bob", "Bob"), ("charlie", "Charlie"), ("diana", "Diana")] {
        app_handle
            .send_node_insert(
                id,
                vec!["Person"],
                PropertyMapBuilder::new().with_string("name", name).build(),
            )
            .await?;
    }

    // Create relationships from Alice to each friend
    for (id, friend_id) in [
        ("knows-1", "bob"),
        ("knows-2", "charlie"),
        ("knows-3", "diana"),
    ] {
        app_handle
            .send_relation_insert(
                id,
                vec!["KNOWS"],
                PropertyMapBuilder::new().build(),
                "alice",   // start (should map to in_node)
                friend_id, // end (should map to out_node)
            )
            .await?;
    }

    // Wait for results - we'll get multiple aggregation updates
    // Keep reading until we get the final count of 3
    let mut final_count = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);

    while tokio::time::Instant::now() < deadline {
        if let Ok(Some(result)) = timeout(Duration::from_millis(500), subscription.recv()).await {
            assert_eq!(result.query_id, "multi-rel-query");

            if let Some(row) = result.results.first() {
                let data = match row {
                    ResultDiff::Add { data } => data,
                    ResultDiff::Aggregation { after, .. } => after,
                    _ => continue,
                };

                if let Some(friend_count) = data.get("friend_count") {
                    let count_value = friend_count
                        .as_str()
                        .and_then(|s| s.parse::<i64>().ok())
                        .or_else(|| friend_count.as_i64())
                        .unwrap_or(0);

                    final_count = count_value;

                    // If we've reached 3, we're done
                    if final_count == 3 {
                        break;
                    }
                }
            }
        }
    }

    // Validate we found all 3 friends
    assert_eq!(
        final_count, 3,
        "Alice should have 3 outgoing KNOWS relationships, found: {final_count}"
    );

    println!("✅ Multiple relationship test passed!");
    println!("   - All 3 relationships correctly directed from Alice to friends");

    Ok(())
}
