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

//! # Multi-Source Join Integration Tests
//!
//! This test module validates that joins work correctly across multiple application sources
//! in DrasiServerCore. It tests the core join functionality by programmatically creating
//! sources, pushing data incrementally, and verifying that join results are correctly computed.
//!
//! ## Test Approach
//!
//! - **Buffered Result Collection**: Results are collected in a background task into a shared
//!   buffer, avoiding complex async coordination with sleeps.
//! - **Early Termination**: Result collection stops as soon as the expected number of results
//!   is received, making tests faster and more reliable.
//! - **No Bootstrap**: Tests disable bootstrap to validate incremental join processing as data
//!   arrives from different sources.
//!
//! ## Running These Tests
//!
//! ```bash
//! # Run all tests in this module
//! cargo test --test multi_source_join_test
//!
//! # Run with output to see data flow
//! cargo test --test multi_source_join_test -- --nocapture
//! ```

use anyhow::Result;
use drasi_core::models::ElementPropertyMap;
use drasi_lib::config::{QueryJoinConfig, QueryJoinKeyConfig};
use drasi_lib::{DrasiServerCore, PropertyMapBuilder, Query, Reaction, Source};
#[allow(unused_imports)] // Required for .next() on streams, used in async closure
use futures::StreamExt;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

/// Helper function to create property map for Customer nodes
fn create_customer_props(customer_id: &str, name: &str) -> ElementPropertyMap {
    PropertyMapBuilder::new()
        .with_string("customer_id", customer_id)
        .with_string("name", name)
        .build()
}

/// Helper function to create property map for Order nodes
fn create_order_props(order_id: &str, customer_id: &str, product_id: &str) -> ElementPropertyMap {
    PropertyMapBuilder::new()
        .with_string("order_id", order_id)
        .with_string("customer_id", customer_id)
        .with_string("product_id", product_id)
        .build()
}

/// Helper function to create property map for Product nodes
fn create_product_props(product_id: &str, product_name: &str) -> ElementPropertyMap {
    PropertyMapBuilder::new()
        .with_string("product_id", product_id)
        .with_string("product_name", product_name)
        .build()
}

/// Test that a 3-way join across Customer, Order, and Product sources works correctly.
///
/// This test validates:
/// - Join configuration with QueryJoinConfig for multi-source joins
/// - Incremental data push without bootstrap
/// - Result collection using buffered approach with early termination
/// - Correct join results when data arrives in various orders
///
/// **Data Model**:
/// - Customer nodes with `customer_id` property
/// - Order nodes with `customer_id` and `product_id` properties (join keys)
/// - Product nodes with `product_id` property
///
/// **Expected Join Results** (3 total):
/// - (C1, O1, Widget) - Alice orders Widget
/// - (C2, O2, Gadget) - Bob orders Gadget
/// - (C1, O3, Gadget) - Alice orders Gadget (second order)
#[tokio::test]
async fn test_three_way_join_incremental() -> Result<()> {
    // ============================================================================
    // SETUP PHASE: Create DrasiServerCore with 3 sources, 1 query, 1 reaction
    // ============================================================================

    let core = DrasiServerCore::builder()
        .with_id("test-multi-source-join")
        // Create 3 application sources for Customer, Order, Product
        .add_source(Source::application("customer-source").auto_start(true).build())
        .add_source(Source::application("order-source").auto_start(true).build())
        .add_source(Source::application("product-source").auto_start(true).build())
        // Create query with 3-way join
        .add_query({
            let query = Query::cypher("join-query")
                .query(
                    "MATCH (c:Customer)-[:CUSTOMER_ORDER]->(o:Order)-[:ORDER_PRODUCT]->(p:Product) \
                     RETURN c.customer_id as customer_id, o.order_id as order_id, p.product_name as product_name"
                )
                .from_source("customer-source")
                .from_source("order-source")
                .from_source("product-source")
                .auto_start(true);

            // Configure joins
            let join1 = QueryJoinConfig {
                id: "CUSTOMER_ORDER".to_string(),
                keys: vec![
                    QueryJoinKeyConfig {
                        label: "Customer".to_string(),
                        property: "customer_id".to_string(),
                    },
                    QueryJoinKeyConfig {
                        label: "Order".to_string(),
                        property: "customer_id".to_string(),
                    },
                ],
            };

            let join2 = QueryJoinConfig {
                id: "ORDER_PRODUCT".to_string(),
                keys: vec![
                    QueryJoinKeyConfig {
                        label: "Order".to_string(),
                        property: "product_id".to_string(),
                    },
                    QueryJoinKeyConfig {
                        label: "Product".to_string(),
                        property: "product_id".to_string(),
                    },
                ],
            };

            query.with_join(join1).with_join(join2).build()
        })
        // Create application reaction to collect results
        .add_reaction(
            Reaction::application("test-reaction")
                .subscribe_to("join-query")
                .auto_start(true)
                .build(),
        )
        .build()
        .await?;

    // Start the server
    core.start().await?;

    // Wrap in Arc for sharing across tasks
    let core = Arc::new(core);

    // ============================================================================
    // RESULT COLLECTION PHASE: Spawn background task to collect results
    // ============================================================================

    // Shared buffer for collecting results
    let results_buffer: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let results_buffer_clone = Arc::clone(&results_buffer);
    let core_clone = Arc::clone(&core);

    // Expected number of join results
    const EXPECTED_RESULTS: usize = 3;

    // Spawn task to collect results from reaction
    let collection_task = tokio::spawn(async move {
        let handle = core_clone
            .reaction_handle("test-reaction")
            .await
            .expect("Failed to get reaction handle");

        let mut stream = handle
            .as_stream()
            .await
            .expect("Failed to create stream for reaction");

        let mut collected_count = 0;

        // Collect results with timeout, but stop early if we get expected count
        let timeout_duration = Duration::from_secs(10);
        let start_time = tokio::time::Instant::now();

        loop {
            // Check if we've exceeded the timeout
            if start_time.elapsed() >= timeout_duration {
                break;
            }

            // Check if we've collected enough results
            if collected_count >= EXPECTED_RESULTS {
                break;
            }

            // Try to get next result with short timeout
            match timeout(Duration::from_millis(500), stream.next()).await {
                Ok(Some(result)) => {
                    // Collect all results from this query result
                    let mut buffer = results_buffer_clone.lock().await;
                    for r in result.results {
                        buffer.push(r);
                        collected_count += 1;
                    }
                }
                Ok(None) => break, // Stream closed
                Err(_) => {
                    // Timeout on this iteration, continue if we haven't hit overall timeout
                    continue;
                }
            }
        }
    });

    // Give the reaction a moment to set up its stream
    tokio::time::sleep(Duration::from_millis(100)).await;

    // ============================================================================
    // DATA INJECTION PHASE: Push data to all 3 sources without delays
    // ============================================================================

    // Get source handles
    let customer_source = core.source_handle("customer-source").await?;
    let order_source = core.source_handle("order-source").await?;
    let product_source = core.source_handle("product-source").await?;

    // Push Customer C1 (Alice)
    customer_source
        .send_node_insert("C1", vec!["Customer"], create_customer_props("C1", "Alice"))
        .await?;

    // Push Product P1 (Widget)
    product_source
        .send_node_insert("P1", vec!["Product"], create_product_props("P1", "Widget"))
        .await?;

    // Push Order O1 (C1 -> P1) - creates first join
    order_source
        .send_node_insert("O1", vec!["Order"], create_order_props("O1", "C1", "P1"))
        .await?;

    // Push Customer C2 (Bob)
    customer_source
        .send_node_insert("C2", vec!["Customer"], create_customer_props("C2", "Bob"))
        .await?;

    // Push Product P2 (Gadget)
    product_source
        .send_node_insert("P2", vec!["Product"], create_product_props("P2", "Gadget"))
        .await?;

    // Push Order O2 (C2 -> P2) - creates second join
    order_source
        .send_node_insert("O2", vec!["Order"], create_order_props("O2", "C2", "P2"))
        .await?;

    // Push Order O3 (C1 -> P2) - creates third join (C1 orders both products)
    order_source
        .send_node_insert("O3", vec!["Order"], create_order_props("O3", "C1", "P2"))
        .await?;

    // ============================================================================
    // VERIFICATION PHASE: Wait for results and verify correctness
    // ============================================================================

    // Wait for collection task to complete
    collection_task.await?;

    // Extract results from buffer
    let results = results_buffer.lock().await;

    // Parse join results into a set of tuples for order-independent comparison
    let mut join_results = HashSet::new();
    for result in results.iter() {
        // Results have structure: { "type": "ADD", "data": { "customer_id": "C1", ... } }
        let data = result.get("data").expect("Result should have data field");

        let customer_id = data
            .get("customer_id")
            .and_then(|v| v.as_str())
            .expect("customer_id should be a string");
        let order_id = data
            .get("order_id")
            .and_then(|v| v.as_str())
            .expect("order_id should be a string");
        let product_name = data
            .get("product_name")
            .and_then(|v| v.as_str())
            .expect("product_name should be a string");

        join_results.insert((
            customer_id.to_string(),
            order_id.to_string(),
            product_name.to_string(),
        ));
    }

    // Verify we got exactly 3 unique join results
    assert_eq!(
        join_results.len(),
        3,
        "Expected 3 join results, got {}",
        join_results.len()
    );

    // Verify each expected join is present
    assert!(
        join_results.contains(&("C1".to_string(), "O1".to_string(), "Widget".to_string())),
        "Missing join: (C1, O1, Widget)"
    );
    assert!(
        join_results.contains(&("C2".to_string(), "O2".to_string(), "Gadget".to_string())),
        "Missing join: (C2, O2, Gadget)"
    );
    assert!(
        join_results.contains(&("C1".to_string(), "O3".to_string(), "Gadget".to_string())),
        "Missing join: (C1, O3, Gadget)"
    );

    // ============================================================================
    // CLEANUP PHASE
    // ============================================================================

    core.stop().await?;

    Ok(())
}
