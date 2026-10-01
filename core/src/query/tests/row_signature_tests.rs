// Copyright 2024 The Drasi Authors.
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

use std::{collections::BTreeMap, sync::Arc};

use drasi_query_cypher::CypherParser;
use serde_json::json;

use crate::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::{Avg, Floor, Function, FunctionRegistry, Sum},
        variable_value::VariableValue,
    },
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, QueryJoin, QueryJoinKey,
        SourceChange,
    },
    query::{ContinuousQuery, QueryBuilder},
};

fn create_registry_with_sum() -> Arc<FunctionRegistry> {
    let registry = Arc::new(FunctionRegistry::new());
    registry.register_function("sum", Function::Aggregating(Arc::new(Sum {})));
    registry
}

async fn build_simple_query(query_str: &str) -> crate::query::ContinuousQuery {
    let function_registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let builder = QueryBuilder::new(query_str, parser);
    builder.build().await
}

async fn build_aggregating_query(query_str: &str) -> crate::query::ContinuousQuery {
    let function_registry = create_registry_with_sum();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let builder =
        QueryBuilder::new(query_str, parser).with_function_registry(function_registry.clone());
    builder.build().await
}

async fn build_query_with_indexes(query_str: &str) -> crate::query::ContinuousQuery {
    let function_registry = Arc::new(FunctionRegistry::new());
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let future_queue = Arc::new(InMemoryFutureQueue::new());
    let builder = QueryBuilder::new(query_str, parser)
        .with_element_index(element_index.clone())
        .with_archive_index(element_index)
        .with_result_index(result_index)
        .with_future_queue(future_queue);
    builder.build().await
}

async fn build_aggregating_query_with_indexes(query_str: &str) -> crate::query::ContinuousQuery {
    let function_registry = create_registry_with_sum();
    let parser = Arc::new(CypherParser::new(function_registry.clone()));
    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let future_queue = Arc::new(InMemoryFutureQueue::new());
    let builder = QueryBuilder::new(query_str, parser)
        .with_function_registry(function_registry.clone())
        .with_element_index(element_index.clone())
        .with_archive_index(element_index)
        .with_result_index(result_index)
        .with_future_queue(future_queue);
    builder.build().await
}

fn make_node(source: &str, id: &str, label: &str, props: serde_json::Value) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source, id),
                labels: Arc::new([Arc::from(label)]),
                effective_from: 1000,
            },
            properties: ElementPropertyMap::from(props),
        },
    }
}

fn make_update(source: &str, id: &str, label: &str, props: serde_json::Value) -> SourceChange {
    SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new(source, id),
                labels: Arc::new([Arc::from(label)]),
                effective_from: 2000,
            },
            properties: ElementPropertyMap::from(props),
        },
    }
}

fn make_delete(source: &str, id: &str) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, id),
            labels: Arc::new([]),
            effective_from: 3000,
        },
    }
}

#[tokio::test]
async fn non_aggregating_insert_has_nonzero_row_signature() {
    let query = build_simple_query("MATCH (n:Sensor) RETURN n.name").await;

    let change = make_node("test", "s1", "Sensor", json!({"name": "temp_1"}));
    let result = query.process_source_change(change).await.unwrap();

    assert_eq!(result.len(), 1);
    assert!(matches!(
        &result[0],
        QueryPartEvaluationContext::Adding { .. }
    ));
    assert_ne!(
        result[0].row_signature(),
        0,
        "Adding result should have a non-zero row_signature"
    );
}

#[tokio::test]
async fn non_aggregating_update_preserves_row_signature() {
    let query = build_query_with_indexes("MATCH (n:Sensor) RETURN n.name").await;

    let insert = make_node("test", "s1", "Sensor", json!({"name": "temp_1"}));
    let insert_result = query.process_source_change(insert).await.unwrap();
    assert_eq!(insert_result.len(), 1);
    let insert_row_signature = insert_result[0].row_signature();

    let update = make_update("test", "s1", "Sensor", json!({"name": "temp_1_updated"}));
    let update_result = query.process_source_change(update).await.unwrap();
    assert_eq!(update_result.len(), 1);
    assert!(matches!(
        &update_result[0],
        QueryPartEvaluationContext::Updating { .. }
    ));
    assert_eq!(
        update_result[0].row_signature(),
        insert_row_signature,
        "Update of same node should preserve row_signature"
    );
}

#[tokio::test]
async fn non_aggregating_delete_preserves_row_signature() {
    let query = build_query_with_indexes("MATCH (n:Sensor) RETURN n.name").await;

    let insert = make_node("test", "s1", "Sensor", json!({"name": "temp_1"}));
    let insert_result = query.process_source_change(insert).await.unwrap();
    assert_eq!(insert_result.len(), 1);
    let insert_row_signature = insert_result[0].row_signature();

    let delete = make_delete("test", "s1");
    let delete_result = query.process_source_change(delete).await.unwrap();
    assert_eq!(delete_result.len(), 1);
    assert!(matches!(
        &delete_result[0],
        QueryPartEvaluationContext::Removing { .. }
    ));
    assert_eq!(
        delete_result[0].row_signature(),
        insert_row_signature,
        "Delete of same node should preserve row_signature"
    );
}

#[tokio::test]
async fn different_solutions_get_different_row_signatures() {
    let query = build_simple_query("MATCH (n:Sensor) RETURN n.name").await;

    let change1 = make_node("test", "s1", "Sensor", json!({"name": "temp_1"}));
    let result1 = query.process_source_change(change1).await.unwrap();
    assert_eq!(result1.len(), 1);

    let change2 = make_node("test", "s2", "Sensor", json!({"name": "temp_1"}));
    let result2 = query.process_source_change(change2).await.unwrap();
    assert_eq!(result2.len(), 1);

    assert_ne!(
        result1[0].row_signature(),
        result2[0].row_signature(),
        "Different graph solutions should produce different row_signatures even with identical projected values"
    );
}

#[tokio::test]
async fn aggregating_query_has_nonzero_row_signature() {
    let query = build_aggregating_query(
        "MATCH (n:Sensor) RETURN n.region AS region, sum(n.value) AS total",
    )
    .await;

    let change = make_node(
        "test",
        "s1",
        "Sensor",
        json!({"region": "west", "value": 10}),
    );
    let result = query.process_source_change(change).await.unwrap();

    assert_eq!(result.len(), 1);
    assert!(matches!(
        &result[0],
        QueryPartEvaluationContext::Aggregation { .. }
    ));
    assert_ne!(
        result[0].row_signature(),
        0,
        "Aggregation result should have a non-zero row_signature"
    );
}

#[tokio::test]
async fn aggregating_same_group_preserves_row_signature() {
    let query = build_aggregating_query_with_indexes(
        "MATCH (n:Sensor) RETURN n.region AS region, sum(n.value) AS total",
    )
    .await;

    let change1 = make_node(
        "test",
        "s1",
        "Sensor",
        json!({"region": "west", "value": 10}),
    );
    let result1 = query.process_source_change(change1).await.unwrap();
    assert_eq!(result1.len(), 1);
    let first_row_signature = result1[0].row_signature();

    let change2 = make_node(
        "test",
        "s2",
        "Sensor",
        json!({"region": "west", "value": 20}),
    );
    let result2 = query.process_source_change(change2).await.unwrap();
    assert_eq!(result2.len(), 1);
    assert_eq!(
        result2[0].row_signature(),
        first_row_signature,
        "Adding to same aggregation group should preserve row_signature"
    );
}

#[tokio::test]
async fn aggregating_different_groups_get_different_row_signatures() {
    let query = build_aggregating_query(
        "MATCH (n:Sensor) RETURN n.region AS region, sum(n.value) AS total",
    )
    .await;

    let change1 = make_node(
        "test",
        "s1",
        "Sensor",
        json!({"region": "west", "value": 10}),
    );
    let result1 = query.process_source_change(change1).await.unwrap();
    assert_eq!(result1.len(), 1);

    let change2 = make_node(
        "test",
        "s2",
        "Sensor",
        json!({"region": "east", "value": 20}),
    );
    let result2 = query.process_source_change(change2).await.unwrap();
    assert_eq!(result2.len(), 1);

    assert_ne!(
        result1[0].row_signature(),
        result2[0].row_signature(),
        "Different GROUP BY values should produce different row_signatures"
    );
}

type KeyedSnapshot = BTreeMap<u64, QueryVariables>;

async fn apply_to_snapshot(
    query: &ContinuousQuery,
    snapshot: &mut KeyedSnapshot,
    change: SourceChange,
) {
    for result in query.process_source_change(change).await.unwrap() {
        match result {
            QueryPartEvaluationContext::Adding {
                after,
                row_signature,
            }
            | QueryPartEvaluationContext::Updating {
                after,
                row_signature,
                ..
            }
            | QueryPartEvaluationContext::Aggregation {
                after,
                row_signature,
                ..
            } => {
                snapshot.insert(row_signature, after);
            }
            QueryPartEvaluationContext::Removing { row_signature, .. } => {
                snapshot.remove(&row_signature);
            }
            QueryPartEvaluationContext::Noop => {}
        }
    }
}

const FLOOR_ALERT: &str = "
MATCH (r:Room)-[:PART_OF_FLOOR]->(f:Floor)
WITH f, floor(50+(r.temperature-72)+(r.humidity-42)+CASE WHEN r.co2>500 THEN (r.co2-500)/25 ELSE 0 END) AS RoomComfortLevel
WITH f, avg(RoomComfortLevel) AS ComfortLevel
WHERE ComfortLevel<40 OR ComfortLevel>50
RETURN f.id AS FloorId,f.name AS FloorName,ComfortLevel";

const BUILDING_ALERT: &str = "
MATCH (r:Room)-[:PART_OF_FLOOR]->(f:Floor)-[:PART_OF_BUILDING]->(b:Building)
WITH f,b,floor(50+(r.temperature-72)+(r.humidity-42)+CASE WHEN r.co2>500 THEN (r.co2-500)/25 ELSE 0 END) AS RoomComfortLevel
WITH f,b,avg(RoomComfortLevel) AS FloorComfortLevel
WITH b,avg(FloorComfortLevel) AS ComfortLevel
WHERE ComfortLevel<40 OR ComfortLevel>50
RETURN b.id AS BuildingId,b.name AS BuildingName,ComfortLevel";

async fn build_comfort_query(query: &str) -> ContinuousQuery {
    let registry = Arc::new(FunctionRegistry::new());
    registry.register_function("avg", Function::Aggregating(Arc::new(Avg {})));
    registry.register_function("floor", Function::Scalar(Arc::new(Floor {})));
    let joins = [
        ("PART_OF_FLOOR", "Room", "floor_id", "Floor", "id"),
        ("PART_OF_BUILDING", "Floor", "building_id", "Building", "id"),
    ]
    .into_iter()
    .map(
        |(id, from_label, from_property, to_label, to_property)| QueryJoin {
            id: id.into(),
            keys: vec![
                QueryJoinKey {
                    label: from_label.into(),
                    property: from_property.into(),
                },
                QueryJoinKey {
                    label: to_label.into(),
                    property: to_property.into(),
                },
            ],
        },
    )
    .collect();
    QueryBuilder::new(query, Arc::new(CypherParser::new(registry.clone())))
        .with_function_registry(registry)
        .with_joins(joins)
        .try_build()
        .await
        .unwrap()
}

fn room_properties(floor: usize, room: usize, broken: bool) -> serde_json::Value {
    let (temperature, humidity, co2) = if broken { (40, 20, 700) } else { (70, 40, 10) };
    json!({
        "id": format!("room_01_{floor:02}_{room:02}"),
        "name": format!("Room {room:02}"),
        "floor_id": format!("floor_01_{floor:02}"),
        "temperature": temperature, "humidity": humidity, "co2": co2,
    })
}

async fn seed_comfort(query: &ContinuousQuery, snapshot: &mut KeyedSnapshot) {
    apply_to_snapshot(
        query,
        snapshot,
        make_node(
            "facilities",
            "building_01",
            "Building",
            json!({"id": "building_01", "name": "Building 01"}),
        ),
    )
    .await;
    for floor in 1..=3 {
        apply_to_snapshot(
            query,
            snapshot,
            make_node(
                "facilities",
                &format!("floor_01_{floor:02}"),
                "Floor",
                json!({
                    "id": format!("floor_01_{floor:02}"),
                    "name": format!("Floor {floor:02}"), "building_id": "building_01"
                }),
            ),
        )
        .await;
        for room in 1..=3 {
            apply_to_snapshot(
                query,
                snapshot,
                make_node(
                    "facilities",
                    &format!("room_01_{floor:02}_{room:02}"),
                    "Room",
                    room_properties(floor, room, false),
                ),
            )
            .await;
        }
    }
}

async fn change_room(
    query: &ContinuousQuery,
    snapshot: &mut KeyedSnapshot,
    floor: usize,
    room: usize,
    broken: bool,
) {
    apply_to_snapshot(
        query,
        snapshot,
        make_update(
            "facilities",
            &format!("room_01_{floor:02}_{room:02}"),
            "Room",
            room_properties(floor, room, broken),
        ),
    )
    .await;
}

#[tokio::test]
async fn projected_floor_alert_snapshot_clears_after_reset() {
    let query = build_comfort_query(FLOOR_ALERT).await;
    let mut snapshot = KeyedSnapshot::new();
    seed_comfort(&query, &mut snapshot).await;
    assert!(snapshot.is_empty());

    change_room(&query, &mut snapshot, 1, 1, true).await;
    assert_eq!(snapshot.len(), 1);
    let floor_one_key = *snapshot.keys().next().unwrap();
    assert_eq!(
        snapshot[&floor_one_key]["ComfortLevel"],
        VariableValue::Float(32.0.into())
    );
    change_room(&query, &mut snapshot, 1, 2, true).await;
    assert_eq!(snapshot.len(), 1, "another room must update the same floor");
    assert_eq!(
        snapshot[&floor_one_key]["ComfortLevel"],
        VariableValue::Float(18.0.into())
    );

    for floor in 1..=3 {
        for room in 1..=3 {
            change_room(&query, &mut snapshot, floor, room, true).await;
        }
    }
    assert_eq!(snapshot.len(), 3);
    for row in snapshot.values() {
        assert_eq!(row["ComfortLevel"], VariableValue::Float(4.0.into()));
    }
    for floor in 1..=3 {
        let floor_id = VariableValue::String(format!("floor_01_{floor:02}"));
        assert_eq!(
            snapshot
                .values()
                .filter(|row| row.get("FloorId") == Some(&floor_id))
                .count(),
            1,
            "each floor must have exactly one current alert"
        );
    }
    let group_keys: Vec<_> = snapshot.keys().copied().collect();

    for floor in 1..=3 {
        for room in 1..=3 {
            change_room(&query, &mut snapshot, floor, room, false).await;
        }
    }
    assert!(
        snapshot.is_empty(),
        "reset must remove every aggregate alert"
    );

    for floor in 1..=3 {
        change_room(&query, &mut snapshot, floor, 3, true).await;
    }
    assert_eq!(
        snapshot.keys().copied().collect::<Vec<_>>(),
        group_keys,
        "filter reentry must retain the group identity, independent of contributor"
    );
}

#[tokio::test]
async fn projected_building_alert_snapshot_clears_after_reset() {
    let query = build_comfort_query(BUILDING_ALERT).await;
    let mut snapshot = KeyedSnapshot::new();
    seed_comfort(&query, &mut snapshot).await;
    assert!(snapshot.is_empty());

    change_room(&query, &mut snapshot, 1, 1, true).await;
    assert!(
        snapshot.is_empty(),
        "one cold room does not alert the building"
    );
    change_room(&query, &mut snapshot, 1, 2, true).await;
    assert_eq!(snapshot.len(), 1);
    let building_key = *snapshot.keys().next().unwrap();
    assert_eq!(
        snapshot[&building_key]["BuildingId"],
        VariableValue::String("building_01".to_string())
    );

    for floor in 1..=3 {
        for room in 1..=3 {
            change_room(&query, &mut snapshot, floor, room, true).await;
        }
    }
    assert_eq!(snapshot.len(), 1);
    assert_eq!(
        snapshot[&building_key]["ComfortLevel"],
        VariableValue::Float(4.0.into())
    );
    for floor in 1..=3 {
        for room in 1..=3 {
            change_room(&query, &mut snapshot, floor, room, false).await;
        }
    }
    assert!(snapshot.is_empty(), "reset must remove the building alert");
}
