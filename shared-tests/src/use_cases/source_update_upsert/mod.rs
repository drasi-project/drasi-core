#![allow(clippy::unwrap_used)]
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

use std::sync::Arc;

use serde_json::json;

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext, functions::FunctionRegistry,
        variable_value::VariableValue,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_functions_cypher::CypherFunctionSet;
use drasi_query_cypher::CypherParser;

use crate::QueryTestConfig;

mod queries;

macro_rules! variablemap {
  ($( $key: expr => $val: expr ),*) => {{
       let mut map = ::std::collections::BTreeMap::new();
       $( map.insert($key.to_string().into(), $val); )*
       map
  }}
}

/// Validates that first SourceUpdate creates a new entity, and subsequent SourceUpdate operations modify the existing entity.
/// This confirms the fundamental upsert contract: Update creates if absent, otherwise updates.
pub async fn test_upsert_semantics(config: &(impl QueryTestConfig + Send)) {
    //println!("\n=== Test 1: Basic Upsert Semantics ===");

    let all_entities_query = build_query(queries::all_entities_query(), config).await;

    // First SourceUpdate for a new entity - should create it
    {
        //println!("Sending first SourceUpdate for entity1 (should create)...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "entity1"),
                    labels: Arc::new([Arc::from("Entity")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "entity1",
                    "name": "Entity One",
                    "status": "active",
                    "value": 100
                })),
            },
        };

        let result = all_entities_query
            .process_source_change(change)
            .await
            .unwrap();

        assert_eq!(result.len(), 1, "Should have exactly one result");
        assert!(
            result.contains(&QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "id" => VariableValue::from(json!("entity1")),
                    "name" => VariableValue::from(json!("Entity One")),
                    "status" => VariableValue::from(json!("active")),
                    "value" => VariableValue::from(json!(100))
                ),
            }),
            "First SourceUpdate should ADD the entity"
        );
        //println!("✓ First SourceUpdate created new entity");
    }

    // Second SourceUpdate for the same entity - should update it
    {
        //println!("Sending second SourceUpdate for entity1 (should update)...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "entity1"),
                    labels: Arc::new([Arc::from("Entity")]),
                    effective_from: 2000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "entity1",
                    "name": "Entity One Updated",
                    "status": "inactive",
                    "value": 200
                })),
            },
        };

        let result = all_entities_query
            .process_source_change(change)
            .await
            .unwrap();

        assert_eq!(result.len(), 1, "Should have exactly one result");
        assert!(
            result.contains(&QueryPartEvaluationContext::Updating {
                before: variablemap!(
                    "id" => VariableValue::from(json!("entity1")),
                    "name" => VariableValue::from(json!("Entity One")),
                    "status" => VariableValue::from(json!("active")),
                    "value" => VariableValue::from(json!(100))
                ),
                after: variablemap!(
                    "id" => VariableValue::from(json!("entity1")),
                    "name" => VariableValue::from(json!("Entity One Updated")),
                    "status" => VariableValue::from(json!("inactive")),
                    "value" => VariableValue::from(json!(200))
                ),
            }),
            "Second SourceUpdate should UPDATE the existing entity"
        );
        //println!("✓ Second SourceUpdate updated existing entity");
    }

    //println!("✓ Test 1 PASSED: SourceUpdate implements upsert semantics");
}

/// Ensures that partial property updates correctly merge with existing entity state, preserving properties not included in the update.
/// Critical for incremental updates where only changed fields are sent.
pub async fn test_partial_updates(config: &(impl QueryTestConfig + Send)) {
    //println!("\n=== Test 2: Partial Property Updates ===");

    let all_entities_query = build_query(queries::all_entities_query(), config).await;

    // Create entity with multiple properties
    {
        //println!("Creating entity with full properties...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "entity2"),
                    labels: Arc::new([Arc::from("Entity")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "entity2",
                    "name": "Full Entity",
                    "status": "online",
                    "value": 42
                })),
            },
        };

        let result = all_entities_query
            .process_source_change(change)
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert!(matches!(
            result[0],
            QueryPartEvaluationContext::Adding { .. }
        ));
        //println!("✓ Entity created with all properties");
    }

    // Partial update - only update value
    {
        //println!("Sending partial update (only value property)...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "entity2"),
                    labels: Arc::new([Arc::from("Entity")]),
                    effective_from: 2000,
                },
                properties: ElementPropertyMap::from(json!({
                    "value": 84
                })),
            },
        };

        let result = all_entities_query
            .process_source_change(change)
            .await
            .unwrap();

        assert_eq!(result.len(), 1);
        assert!(
            result.contains(&QueryPartEvaluationContext::Updating {
                before: variablemap!(
                    "id" => VariableValue::from(json!("entity2")),
                    "name" => VariableValue::from(json!("Full Entity")),
                    "status" => VariableValue::from(json!("online")),
                    "value" => VariableValue::from(json!(42))
                ),
                after: variablemap!(
                    "id" => VariableValue::from(json!("entity2")),
                    "name" => VariableValue::from(json!("Full Entity")),  // Preserved
                    "status" => VariableValue::from(json!("online")),     // Preserved
                    "value" => VariableValue::from(json!(84))              // Updated
                ),
            }),
            "Partial update should preserve unspecified properties"
        );
        //println!("✓ Partial update preserved unspecified properties");
    }

    //println!("✓ Test 2 PASSED: Partial updates preserve existing properties");
}

/// Simulates stateless event sources that send complete entity snapshots with each event, without tracking whether entities exist.
/// Validates that the engine correctly handles repeated full-state updates from sources like IoT sensors or polling-based adapters.
pub async fn test_stateless_processing(config: &(impl QueryTestConfig + Send)) {
    //println!("\n=== Test 3: Stateless Event Processing ===");
    //println!("Simulating a stateless process sending complete state each time...");

    let sensor_query = build_query(queries::sensor_readings_query(), config).await;

    // Event 1: First event from stateless process
    {
        //println!("Event 1: Initial sensor reading...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("iot", "sensor1"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "sensor1",
                    "temp": 20,
                    "humidity": 60,
                    "battery": 100
                })),
            },
        };

        let result = sensor_query.process_source_change(change).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(
            result.contains(&QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "id" => VariableValue::from(json!("sensor1")),
                    "temp" => VariableValue::from(json!(20)),
                    "humidity" => VariableValue::from(json!(60)),
                    "battery" => VariableValue::from(json!(100))
                ),
            }),
            "First event should create the sensor"
        );
        //println!("✓ Event 1: Created new sensor node");
    }

    // Event 2: Second complete state update
    {
        //println!("Event 2: Updated sensor reading...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("iot", "sensor1"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: 2000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "sensor1",
                    "temp": 21,
                    "humidity": 61,
                    "battery": 99
                })),
            },
        };

        let result = sensor_query.process_source_change(change).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(
            result.contains(&QueryPartEvaluationContext::Updating {
                before: variablemap!(
                    "id" => VariableValue::from(json!("sensor1")),
                    "temp" => VariableValue::from(json!(20)),
                    "humidity" => VariableValue::from(json!(60)),
                    "battery" => VariableValue::from(json!(100))
                ),
                after: variablemap!(
                    "id" => VariableValue::from(json!("sensor1")),
                    "temp" => VariableValue::from(json!(21)),
                    "humidity" => VariableValue::from(json!(61)),
                    "battery" => VariableValue::from(json!(99))
                ),
            }),
            "Second event should update existing sensor"
        );
        //println!("✓ Event 2: Updated existing sensor node");
    }

    // Event 3: Third complete state update
    {
        //println!("Event 3: Another sensor reading...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("iot", "sensor1"),
                    labels: Arc::new([Arc::from("Sensor")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "sensor1",
                    "temp": 22,
                    "humidity": 62,
                    "battery": 98
                })),
            },
        };

        let result = sensor_query.process_source_change(change).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(
            result.contains(&QueryPartEvaluationContext::Updating {
                before: variablemap!(
                    "id" => VariableValue::from(json!("sensor1")),
                    "temp" => VariableValue::from(json!(21)),
                    "humidity" => VariableValue::from(json!(61)),
                    "battery" => VariableValue::from(json!(99))
                ),
                after: variablemap!(
                    "id" => VariableValue::from(json!("sensor1")),
                    "temp" => VariableValue::from(json!(22)),
                    "humidity" => VariableValue::from(json!(62)),
                    "battery" => VariableValue::from(json!(98))
                ),
            }),
            "Third event should update existing sensor"
        );
        //println!("✓ Event 3: Updated existing sensor node");
    }

    //println!("✓ Test 3 PASSED: Stateless processing works correctly with SourceUpdate");
}

/// Verifies that entities correctly enter and exit query result sets when property changes affect filter condition matching.
/// Tests the engine's ability to track entities transitioning between matching and non-matching states.
pub async fn test_query_matching(config: &(impl QueryTestConfig + Send)) {
    //println!("\n=== Test 4: Query Matching Verification ===");

    let online_devices = build_query(queries::online_devices_query(), config).await;

    // Create device with status=online (should match query)
    {
        //println!("Creating device with status='online'...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "device1"),
                    labels: Arc::new([Arc::from("Device")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "device1",
                    "name": "Test Device",
                    "status": "online",
                    "temp": 25,
                    "humidity": 50
                })),
            },
        };

        let result = online_devices.process_source_change(change).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(
            result.contains(&QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "id" => VariableValue::from(json!("device1")),
                    "name" => VariableValue::from(json!("Test Device")),
                    "temp" => VariableValue::from(json!(25)),
                    "humidity" => VariableValue::from(json!(50))
                ),
            }),
            "Device with status='online' should match query"
        );
        //println!("✓ Device created and matched query");
    }

    // Update device to status=offline (should unmatch query)
    {
        //println!("Updating device to status='offline'...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "device1"),
                    labels: Arc::new([Arc::from("Device")]),
                    effective_from: 2000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "device1",
                    "name": "Test Device",
                    "status": "offline",
                    "temp": 25,
                    "humidity": 50
                })),
            },
        };

        let result = online_devices.process_source_change(change).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(
            result.contains(&QueryPartEvaluationContext::Removing {
                before: variablemap!(
                    "id" => VariableValue::from(json!("device1")),
                    "name" => VariableValue::from(json!("Test Device")),
                    "temp" => VariableValue::from(json!(25)),
                    "humidity" => VariableValue::from(json!(50))
                ),
            }),
            "Device with status='offline' should be removed from results"
        );
        //println!("✓ Device updated and removed from query results");
    }

    // Update device back to status=online (should match again)
    {
        //println!("Updating device back to status='online'...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "device1"),
                    labels: Arc::new([Arc::from("Device")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "device1",
                    "name": "Test Device",
                    "status": "online",
                    "temp": 26,
                    "humidity": 51
                })),
            },
        };

        let result = online_devices.process_source_change(change).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(
            result.contains(&QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "id" => VariableValue::from(json!("device1")),
                    "name" => VariableValue::from(json!("Test Device")),
                    "temp" => VariableValue::from(json!(26)),
                    "humidity" => VariableValue::from(json!(51))
                ),
            }),
            "Device with status='online' should match query again"
        );
        //println!("✓ Device re-added to query results");
    }

    //println!("✓ Test 4 PASSED: Query matching works correctly with SourceUpdate");
}

/// Confirms that updates to one entity are properly isolated and do not affect other entities in the query.
/// Ensures the engine correctly manages concurrent entity state without cross-contamination.
pub async fn test_multiple_entities(config: &(impl QueryTestConfig + Send)) {
    //println!("\n=== Test 5: Multiple Entities Isolation ===");

    let all_entities = build_query(queries::all_entities_query(), config).await;

    // Create three entities
    //println!("Creating three entities...");
    for i in 1..=3 {
        let id = format!("entity{}", i);
        let name = format!("Entity {}", i);

        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", &id),
                    labels: Arc::new([Arc::from("Entity")]),
                    effective_from: (i * 1000) as u64,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": &id,
                    "name": &name,
                    "status": "active",
                    "value": i * 10
                })),
            },
        };

        let result = all_entities.process_source_change(change).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(matches!(
            result[0],
            QueryPartEvaluationContext::Adding { .. }
        ));
        //println!("✓ Created {}", id);
    }

    // Update only entity2
    {
        //println!("Updating only entity2...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "entity2"),
                    labels: Arc::new([Arc::from("Entity")]),
                    effective_from: 4000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "entity2",
                    "name": "Entity 2 Updated",
                    "status": "inactive",
                    "value": 99
                })),
            },
        };

        let result = all_entities.process_source_change(change).await.unwrap();

        assert_eq!(result.len(), 1, "Should only affect entity2");
        assert!(
            result.contains(&QueryPartEvaluationContext::Updating {
                before: variablemap!(
                    "id" => VariableValue::from(json!("entity2")),
                    "name" => VariableValue::from(json!("Entity 2")),
                    "status" => VariableValue::from(json!("active")),
                    "value" => VariableValue::from(json!(20))
                ),
                after: variablemap!(
                    "id" => VariableValue::from(json!("entity2")),
                    "name" => VariableValue::from(json!("Entity 2 Updated")),
                    "status" => VariableValue::from(json!("inactive")),
                    "value" => VariableValue::from(json!(99))
                ),
            }),
            "Should update only entity2"
        );
        //println!("✓ Updated only entity2, others unaffected");
    }

    //println!("✓ Test 5 PASSED: Multiple entities are properly isolated");
}

/// Validates that upsert semantics work correctly for relationships (edges), not just nodes.
/// Tests creation and modification of graph edges using SourceUpdate operations.
pub async fn test_relationship_upsert(config: &(impl QueryTestConfig + Send)) {
    //println!("\n=== Test 6: Relationship Upsert ===");

    let connected_query = build_query(queries::connected_devices_query(), config).await;

    // First create the nodes
    for id in ["device1", "device2"] {
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", id),
                    labels: Arc::new([Arc::from("Device")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": id
                })),
            },
        };
        let _ = connected_query.process_source_change(change).await;
    }

    // First SourceUpdate for relationship - should create it
    {
        //println!("Creating relationship with SourceUpdate...");
        let change = SourceChange::Update {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "conn1"),
                    labels: Arc::new([Arc::from("CONNECTED_TO")]),
                    effective_from: 2000,
                },
                in_node: ElementReference::new("test", "device1"),
                out_node: ElementReference::new("test", "device2"),
                properties: ElementPropertyMap::from(json!({
                    "strength": 50
                })),
            },
        };

        let result = connected_query.process_source_change(change).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(
            result.contains(&QueryPartEvaluationContext::Adding {
                after: variablemap!(
                    "from_id" => VariableValue::from(json!("device1")),
                    "to_id" => VariableValue::from(json!("device2")),
                    "strength" => VariableValue::from(json!(50))
                ),
            }),
            "First relationship SourceUpdate should create it"
        );
        //println!("✓ Relationship created");
    }

    // Second SourceUpdate for same relationship - should update it
    {
        //println!("Updating relationship with SourceUpdate...");
        let change = SourceChange::Update {
            element: Element::Relation {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "conn1"),
                    labels: Arc::new([Arc::from("CONNECTED_TO")]),
                    effective_from: 3000,
                },
                in_node: ElementReference::new("test", "device1"),
                out_node: ElementReference::new("test", "device2"),
                properties: ElementPropertyMap::from(json!({
                    "strength": 75
                })),
            },
        };

        let result = connected_query.process_source_change(change).await.unwrap();

        assert_eq!(result.len(), 1);
        assert!(
            result.contains(&QueryPartEvaluationContext::Updating {
                before: variablemap!(
                    "from_id" => VariableValue::from(json!("device1")),
                    "to_id" => VariableValue::from(json!("device2")),
                    "strength" => VariableValue::from(json!(50))
                ),
                after: variablemap!(
                    "from_id" => VariableValue::from(json!("device1")),
                    "to_id" => VariableValue::from(json!("device2")),
                    "strength" => VariableValue::from(json!(75))
                ),
            }),
            "Second relationship SourceUpdate should update it"
        );
        //println!("✓ Relationship updated");
    }

    //println!("✓ Test 6 PASSED: Relationship upsert works correctly");
}

/// Tests that aggregation queries correctly update aggregate values when entities are created or modified via SourceUpdate.
/// Validates incremental aggregation maintenance across entity lifecycle transitions.
pub async fn test_aggregation_with_upserts(config: &(impl QueryTestConfig + Send)) {
    //println!("\n=== Test 7: Aggregation with Upserts ===");

    let count_query = build_query(queries::entity_count_by_status_query(), config).await;

    // Create first entity with status=active
    {
        //println!("Creating first entity with status='active'...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "e1"),
                    labels: Arc::new([Arc::from("Entity")]),
                    effective_from: 1000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "e1",
                    "status": "active"
                })),
            },
        };

        let result = count_query.process_source_change(change).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(result.contains(&QueryPartEvaluationContext::Aggregation {
            grouping_keys: vec!["status".to_string()],
            default_before: true,
            default_after: false,
            before: Some(variablemap!(
                "status" => VariableValue::from(json!("active")),
                "count" => VariableValue::from(json!(0))
            )),
            after: variablemap!(
                "status" => VariableValue::from(json!("active")),
                "count" => VariableValue::from(json!(1))
            ),
        }));
        //println!("✓ Count for 'active' = 1");
    }

    // Create second entity with status=active
    {
        //println!("Creating second entity with status='active'...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "e2"),
                    labels: Arc::new([Arc::from("Entity")]),
                    effective_from: 2000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "e2",
                    "status": "active"
                })),
            },
        };

        let result = count_query.process_source_change(change).await.unwrap();
        assert_eq!(result.len(), 1);
        assert!(matches!(&result[0],
            QueryPartEvaluationContext::Aggregation { after, .. }
            if after.get("count") == Some(&VariableValue::from(json!(2)))
        ));
        //println!("✓ Count for 'active' = 2");
    }

    // Update first entity to status=inactive
    {
        //println!("Updating first entity to status='inactive'...");
        let change = SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "e1"),
                    labels: Arc::new([Arc::from("Entity")]),
                    effective_from: 3000,
                },
                properties: ElementPropertyMap::from(json!({
                    "id": "e1",
                    "status": "inactive"
                })),
            },
        };

        let result = count_query.process_source_change(change).await.unwrap();
        assert_eq!(result.len(), 2); // Two aggregation changes

        // Check active count decreased to 1
        assert!(result.iter().any(|ctx| matches!(ctx,
            QueryPartEvaluationContext::Aggregation { after, .. }
            if after.get("status") == Some(&VariableValue::from(json!("active")))
            && after.get("count") == Some(&VariableValue::from(json!(1)))
        )));

        // Check inactive count increased to 1
        assert!(result.iter().any(|ctx| matches!(ctx,
            QueryPartEvaluationContext::Aggregation { after, before, .. }
            if after.get("status") == Some(&VariableValue::from(json!("inactive")))
            && after.get("count") == Some(&VariableValue::from(json!(1)))
            && before.as_ref().and_then(|b| b.get("count")) == Some(&VariableValue::from(json!(0)))
        )));

        //println!("✓ Count for 'active' = 1, 'inactive' = 1");
    }

    //println!("✓ Test 7 PASSED: Aggregations work correctly with upserts");
}

// Helper function to build a query
async fn build_query(query_str: &str, config: &(impl QueryTestConfig + Send)) -> ContinuousQuery {
    let function_registry = Arc::new(FunctionRegistry::new()).with_cypher_function_set();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let mut builder =
        QueryBuilder::new(query_str, parser).with_function_registry(function_registry);
    builder = config.config_query(builder).await;
    builder.build().await
}
