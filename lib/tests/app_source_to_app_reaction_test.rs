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

//! # ApplicationSource to ApplicationReaction Integration Test
//!
//! This test validates that data flows correctly from ApplicationSource through
//! a simple Cypher query to ApplicationReaction.
//!
//! ## Test Flow
//! 1. Create DrasiServerCore with ApplicationSource, Query, ApplicationReaction
//! 2. Inject 3 Person nodes via ApplicationSource
//! 3. Collect results from ApplicationReaction using buffered pattern
//! 4. Validate all 3 results are received with correct data

use anyhow::Result;
use drasi_core::models::ElementPropertyMap;
use drasi_lib::{DrasiServerCore, PropertyMapBuilder, Query, Reaction, Source};
#[allow(unused_imports)]
use futures::StreamExt;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::{timeout, Duration};

/// Helper function to create property map for Person nodes
fn create_person_props(id: &str, name: &str) -> ElementPropertyMap {
    PropertyMapBuilder::new()
        .with_string("id", id)
        .with_string("name", name)
        .build()
}

#[tokio::test]
async fn test_application_source_to_application_reaction() -> Result<()> {
    // ============================================================================
    // SETUP PHASE: Create DrasiServerCore with source, query, reaction
    // ============================================================================

    let core = DrasiServerCore::builder()
        .with_id("test-app-to-app")
        .add_source(
            Source::application("person-source")
                .auto_start(true)
                .build(),
        )
        .add_query(
            Query::cypher("person-query")
                .query("MATCH (n:Person) RETURN n.id as id, n.name as name")
                .from_source("person-source")
                .auto_start(true)
                .build(),
        )
        .add_reaction(
            Reaction::application("person-reaction")
                .subscribe_to("person-query")
                .auto_start(true)
                .build(),
        )
        .build()
        .await?;

    core.start().await?;
    let core = Arc::new(core);

    // ============================================================================
    // RESULT COLLECTION PHASE: Spawn background task to collect results
    // ============================================================================

    let results_buffer: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
    let results_buffer_clone = Arc::clone(&results_buffer);
    let core_clone = Arc::clone(&core);

    const EXPECTED_RESULTS: usize = 3;

    let collection_task = tokio::spawn(async move {
        let handle = core_clone
            .reaction_handle("person-reaction")
            .await
            .expect("Failed to get reaction handle");

        let mut stream = handle
            .as_stream()
            .await
            .expect("Failed to create stream for reaction");

        let mut collected_count = 0;
        let timeout_duration = Duration::from_secs(10);
        let start_time = tokio::time::Instant::now();

        loop {
            if start_time.elapsed() >= timeout_duration {
                break;
            }

            if collected_count >= EXPECTED_RESULTS {
                break;
            }

            match timeout(Duration::from_millis(500), stream.next()).await {
                Ok(Some(result)) => {
                    let mut buffer = results_buffer_clone.lock().await;
                    for r in result.results {
                        buffer.push(r);
                        collected_count += 1;
                    }
                }
                Ok(None) => break,
                Err(_) => continue,
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    // ============================================================================
    // DATA INJECTION PHASE: Push test data through source
    // ============================================================================

    let source_handle = core.source_handle("person-source").await?;

    source_handle
        .send_node_insert(
            "person-1",
            vec!["Person"],
            create_person_props("p1", "Alice"),
        )
        .await?;

    source_handle
        .send_node_insert("person-2", vec!["Person"], create_person_props("p2", "Bob"))
        .await?;

    source_handle
        .send_node_insert(
            "person-3",
            vec!["Person"],
            create_person_props("p3", "Charlie"),
        )
        .await?;

    // ============================================================================
    // VERIFICATION PHASE: Wait for results and validate
    // ============================================================================

    collection_task.await?;
    let results = results_buffer.lock().await;

    assert_eq!(
        results.len(),
        EXPECTED_RESULTS,
        "Should receive exactly 3 results"
    );

    // Validate each result has correct structure and type
    let mut found_ids = std::collections::HashSet::new();
    for result in results.iter() {
        assert_eq!(
            result.get("type").and_then(|v| v.as_str()),
            Some("ADD"),
            "Result type should be ADD"
        );

        let data = result.get("data").expect("Result should have data field");
        let id = data
            .get("id")
            .and_then(|v| v.as_str())
            .expect("Data should have id field");
        let name = data
            .get("name")
            .and_then(|v| v.as_str())
            .expect("Data should have name field");

        found_ids.insert(id.to_string());

        // Validate id-name pairs match what we inserted
        match id {
            "p1" => assert_eq!(name, "Alice"),
            "p2" => assert_eq!(name, "Bob"),
            "p3" => assert_eq!(name, "Charlie"),
            _ => panic!("Unexpected id: {}", id),
        }
    }

    assert_eq!(found_ids.len(), 3, "Should have 3 unique person IDs");

    // ============================================================================
    // CLEANUP PHASE
    // ============================================================================

    core.stop().await?;
    Ok(())
}
