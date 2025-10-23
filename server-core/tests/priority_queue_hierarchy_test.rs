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

//! Integration tests for priority queue capacity three-level hierarchy:
//! 1. Component-specific override (highest priority)
//! 2. Server global setting
//! 3. Hardcoded default (10000)

use drasi_server_core::{DrasiServerCore, Query, Reaction, Source};

#[tokio::test]
async fn test_priority_queue_capacity_hierarchy_all_defaults() {
    // No global setting, no component overrides → all should use 10000
    let core = DrasiServerCore::builder()
        // No global priority_queue_capacity set (defaults to None)
        .add_source(Source::application("source1").build())
        .add_query(
            Query::cypher("query1")
                .query("MATCH (n) RETURN n")
                .from_source("source1")
                .build(),
        )
        .add_reaction(Reaction::log("reaction1").subscribe_to("query1").build())
        .build()
        .await
        .unwrap();

    let config = core.get_current_config().await.unwrap();

    // Server global should be None
    assert_eq!(config.server_core.priority_queue_capacity, None);

    // Query and reaction should inherit 10000 (the hardcoded default)
    assert_eq!(
        config.queries[0].priority_queue_capacity,
        Some(10000),
        "Query should inherit hardcoded default of 10000"
    );
    assert_eq!(
        config.reactions[0].priority_queue_capacity,
        Some(10000),
        "Reaction should inherit hardcoded default of 10000"
    );
}

#[tokio::test]
async fn test_priority_queue_capacity_hierarchy_global_override() {
    // Global setting of 20000, no component overrides → all should use 20000
    let yaml = r#"
server_core:
  id: test-server
  priority_queue_capacity: 20000
sources:
  - id: source1
    source_type: application
    auto_start: true
queries:
  - id: query1
    query: "MATCH (n) RETURN n"
    sources: ["source1"]
    auto_start: false
reactions:
  - id: reaction1
    reaction_type: log
    queries: ["query1"]
    auto_start: false
"#;

    let core = DrasiServerCore::from_config_str(yaml).await.unwrap();
    let config = core.get_current_config().await.unwrap();

    // Server global should be 20000
    assert_eq!(config.server_core.priority_queue_capacity, Some(20000));

    // Query and reaction should inherit 20000
    assert_eq!(
        config.queries[0].priority_queue_capacity,
        Some(20000),
        "Query should inherit global setting of 20000"
    );
    assert_eq!(
        config.reactions[0].priority_queue_capacity,
        Some(20000),
        "Reaction should inherit global setting of 20000"
    );
}

#[tokio::test]
async fn test_priority_queue_capacity_hierarchy_component_override() {
    // Global setting of 20000, but components override → components use their own values
    let yaml = r#"
server_core:
  id: test-server
  priority_queue_capacity: 20000
sources:
  - id: source1
    source_type: application
    auto_start: true
queries:
  - id: query1
    query: "MATCH (n) RETURN n"
    sources: ["source1"]
    auto_start: false
    priority_queue_capacity: 50000
  - id: query2
    query: "MATCH (m) RETURN m"
    sources: ["source1"]
    auto_start: false
    # No override - should inherit global 20000
reactions:
  - id: reaction1
    reaction_type: log
    queries: ["query1"]
    auto_start: false
    priority_queue_capacity: 100000
  - id: reaction2
    reaction_type: log
    queries: ["query2"]
    auto_start: false
    # No override - should inherit global 20000
"#;

    let core = DrasiServerCore::from_config_str(yaml).await.unwrap();
    let config = core.get_current_config().await.unwrap();

    // Server global should be 20000
    assert_eq!(config.server_core.priority_queue_capacity, Some(20000));

    // Find configs by ID (HashMap doesn't preserve order)
    let query1 = config.queries.iter().find(|q| q.id == "query1").unwrap();
    let query2 = config.queries.iter().find(|q| q.id == "query2").unwrap();
    let reaction1 = config.reactions.iter().find(|r| r.id == "reaction1").unwrap();
    let reaction2 = config.reactions.iter().find(|r| r.id == "reaction2").unwrap();

    // query1 has explicit override
    assert_eq!(
        query1.priority_queue_capacity,
        Some(50000),
        "query1 should use its own override of 50000"
    );

    // query2 has no override, should inherit global
    assert_eq!(
        query2.priority_queue_capacity,
        Some(20000),
        "query2 should inherit global setting of 20000"
    );

    // reaction1 has explicit override
    assert_eq!(
        reaction1.priority_queue_capacity,
        Some(100000),
        "reaction1 should use its own override of 100000"
    );

    // reaction2 has no override, should inherit global
    assert_eq!(
        reaction2.priority_queue_capacity,
        Some(20000),
        "reaction2 should inherit global setting of 20000"
    );
}

#[tokio::test]
async fn test_priority_queue_capacity_hierarchy_mixed() {
    // Mix of all three levels in a single configuration
    let yaml = r#"
server_core:
  id: test-server
  priority_queue_capacity: 30000
sources:
  - id: source1
    source_type: application
queries:
  - id: query_with_override
    query: "MATCH (n) RETURN n"
    sources: ["source1"]
    priority_queue_capacity: 75000
  - id: query_inherits_global
    query: "MATCH (m) RETURN m"
    sources: ["source1"]
reactions:
  - id: reaction_with_override
    reaction_type: log
    queries: ["query_with_override"]
    priority_queue_capacity: 150000
  - id: reaction_inherits_global
    reaction_type: log
    queries: ["query_inherits_global"]
"#;

    let core = DrasiServerCore::from_config_str(yaml).await.unwrap();
    let config = core.get_current_config().await.unwrap();

    assert_eq!(config.server_core.priority_queue_capacity, Some(30000));

    // Find configs by ID (HashMap doesn't preserve order)
    let query_with_override = config
        .queries
        .iter()
        .find(|q| q.id == "query_with_override")
        .unwrap();
    let query_inherits = config
        .queries
        .iter()
        .find(|q| q.id == "query_inherits_global")
        .unwrap();
    let reaction_with_override = config
        .reactions
        .iter()
        .find(|r| r.id == "reaction_with_override")
        .unwrap();
    let reaction_inherits = config
        .reactions
        .iter()
        .find(|r| r.id == "reaction_inherits_global")
        .unwrap();

    assert_eq!(query_with_override.priority_queue_capacity, Some(75000));
    assert_eq!(query_inherits.priority_queue_capacity, Some(30000));
    assert_eq!(reaction_with_override.priority_queue_capacity, Some(150000));
    assert_eq!(reaction_inherits.priority_queue_capacity, Some(30000));
}

#[tokio::test]
async fn test_priority_queue_capacity_omitted_everywhere() {
    // Test that completely omitting all capacity settings works
    let yaml = r#"
server_core:
  id: test-server
sources:
  - id: source1
    source_type: application
queries:
  - id: query1
    query: "MATCH (n) RETURN n"
    sources: ["source1"]
reactions:
  - id: reaction1
    reaction_type: log
    queries: ["query1"]
"#;

    let core = DrasiServerCore::from_config_str(yaml).await.unwrap();
    let config = core.get_current_config().await.unwrap();

    // Everything should default to 10000
    assert_eq!(config.server_core.priority_queue_capacity, None);
    assert_eq!(config.queries[0].priority_queue_capacity, Some(10000));
    assert_eq!(config.reactions[0].priority_queue_capacity, Some(10000));
}
