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

use std::{collections::BTreeMap, sync::Arc};

use drasi_core::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::{Avg, Floor, Function, FunctionRegistry},
        variable_value::VariableValue,
    },
    models::SourceChange,
    query::{ContinuousQuery, QueryBuilder},
};
use drasi_query_cypher::CypherParser;
use serde_json::json;

use super::{
    data::{
        delete_tutorial_node, get_tutorial_bootstrap_data, insert_tutorial_node,
        tutorial_room_properties, update_tutorial_node,
    },
    queries::{tutorial_joins, TUTORIAL_BUILDING_ALERT, TUTORIAL_FLOOR_ALERT},
};
use crate::QueryTestConfig;

type KeyedSnapshot = BTreeMap<u64, QueryVariables>;

/// Folds every emitted delta into a row-signature-keyed downstream view.
/// Adding, Updating and Aggregation replace that key's row; Removing evicts it.
/// Returns the deltas so scenarios can also assert their kind, count and exact keys.
async fn apply_to_snapshot(
    query: &ContinuousQuery,
    snapshot: &mut KeyedSnapshot,
    change: SourceChange,
) -> Vec<QueryPartEvaluationContext> {
    let results = query.process_source_change(change).await.unwrap();
    for result in &results {
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
                snapshot.insert(*row_signature, after.clone());
            }
            QueryPartEvaluationContext::Removing { row_signature, .. } => {
                snapshot.remove(row_signature);
            }
            QueryPartEvaluationContext::Noop => {}
        }
    }
    results
}

async fn build_comfort_query(
    query: &str,
    config: &(impl QueryTestConfig + Send),
) -> ContinuousQuery {
    let registry = Arc::new(FunctionRegistry::new());
    registry.register_function("avg", Function::Aggregating(Arc::new(Avg {})));
    registry.register_function("floor", Function::Scalar(Arc::new(Floor {})));
    let builder = QueryBuilder::new(query, Arc::new(CypherParser::new(registry.clone())))
        .with_function_registry(registry)
        .with_joins(tutorial_joins());
    config
        .config_query(builder)
        .await
        .try_build()
        .await
        .unwrap()
}

async fn seed_comfort(
    query: &ContinuousQuery,
    snapshot: &mut KeyedSnapshot,
    floors: usize,
    rooms_per_floor: usize,
) {
    for change in get_tutorial_bootstrap_data(floors, rooms_per_floor) {
        apply_to_snapshot(query, snapshot, change).await;
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
        update_tutorial_node(
            &format!("room_01_{floor:02}_{room:02}"),
            "Room",
            tutorial_room_properties(floor, room, broken),
        ),
    )
    .await;
}

pub async fn projected_floor_alert_snapshot_clears_after_reset(
    config: &(impl QueryTestConfig + Send),
) {
    let query = build_comfort_query(TUTORIAL_FLOOR_ALERT, config).await;
    let mut snapshot = KeyedSnapshot::new();
    seed_comfort(&query, &mut snapshot, 3, 3).await;
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

pub async fn projected_building_alert_snapshot_clears_after_reset(
    config: &(impl QueryTestConfig + Send),
) {
    let query = build_comfort_query(TUTORIAL_BUILDING_ALERT, config).await;
    let mut snapshot = KeyedSnapshot::new();
    seed_comfort(&query, &mut snapshot, 3, 3).await;
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

pub async fn projected_alert_snapshots_track_room_deletion(config: &(impl QueryTestConfig + Send)) {
    for (query_text, remaining_comfort) in [
        (TUTORIAL_FLOOR_ALERT, 25.0),
        (TUTORIAL_BUILDING_ALERT, 39.0),
    ] {
        let query = build_comfort_query(query_text, config).await;
        let mut snapshot = KeyedSnapshot::new();
        seed_comfort(&query, &mut snapshot, 3, 3).await;
        change_room(&query, &mut snapshot, 1, 1, true).await;
        change_room(&query, &mut snapshot, 1, 2, true).await;
        assert_eq!(snapshot.len(), 1);
        let key = *snapshot.keys().next().unwrap();

        let results =
            apply_to_snapshot(&query, &mut snapshot, delete_tutorial_node("room_01_01_02")).await;
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].row_signature(), key);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot[&key]["ComfortLevel"],
            VariableValue::Float(remaining_comfort.into())
        );
        let before = snapshot[&key].clone();

        let results =
            apply_to_snapshot(&query, &mut snapshot, delete_tutorial_node("room_01_01_01")).await;
        assert_eq!(
            results,
            vec![QueryPartEvaluationContext::Removing {
                before,
                row_signature: key,
            }]
        );
        assert!(snapshot.is_empty(), "deletion must evict the original key");

        apply_to_snapshot(
            &query,
            &mut snapshot,
            insert_tutorial_node(
                "room_01_01_02",
                "Room",
                tutorial_room_properties(1, 2, true),
                4000,
            ),
        )
        .await;
        assert_eq!(snapshot.len(), 1);
        assert_eq!(
            snapshot[&key]["ComfortLevel"],
            VariableValue::Float(remaining_comfort.into()),
            "a different contributor must reenter with the original group key"
        );
    }
}

async fn set_room_comfort(
    query: &ContinuousQuery,
    snapshot: &mut KeyedSnapshot,
    room: usize,
    comfort: i32,
) -> Vec<QueryPartEvaluationContext> {
    let mut properties = tutorial_room_properties(1, room, false);
    // With humidity 40 and CO2 below 500, the tutorial formula is temperature - 24.
    properties["temperature"] = json!(comfort + 24);
    apply_to_snapshot(
        query,
        snapshot,
        update_tutorial_node(&format!("room_01_01_{room:02}"), "Room", properties),
    )
    .await
}

pub async fn projected_alert_snapshots_respect_boundaries(config: &(impl QueryTestConfig + Send)) {
    for query_text in [TUTORIAL_FLOOR_ALERT, TUTORIAL_BUILDING_ALERT] {
        let query = build_comfort_query(query_text, config).await;
        let mut snapshot = KeyedSnapshot::new();
        // Ten integer room scores produce the just-outside averages 39.9 and 50.1.
        seed_comfort(&query, &mut snapshot, 1, 10).await;
        assert!(snapshot.is_empty());
        let mut group_key = None;

        for (boundary, outside_room, outside_average) in [(40, 39, 39.9), (50, 51, 50.1)] {
            for room in 1..=10 {
                set_room_comfort(&query, &mut snapshot, room, boundary).await;
                assert!(snapshot.is_empty(), "40 through 50 must not alert");
            }

            let results = set_room_comfort(&query, &mut snapshot, 10, outside_room).await;
            assert_eq!(results.len(), 1);
            assert_eq!(snapshot.len(), 1);
            let key = *snapshot.keys().next().unwrap();
            if let Some(previous) = group_key {
                assert_eq!(key, previous, "lower and upper alerts share group identity");
            }
            group_key = Some(key);
            assert_eq!(
                snapshot[&key]["ComfortLevel"],
                VariableValue::Float(outside_average.into())
            );
            let before = snapshot[&key].clone();

            let results = set_room_comfort(&query, &mut snapshot, 10, boundary).await;
            assert_eq!(
                results,
                vec![QueryPartEvaluationContext::Removing {
                    before,
                    row_signature: key,
                }],
                "returning exactly to the boundary must remove the alert"
            );
            assert!(snapshot.is_empty());
        }
    }
}

fn floor_alert_row(floor: usize, comfort: f64) -> QueryVariables {
    BTreeMap::from([
        (
            "FloorId".into(),
            VariableValue::String(format!("floor_01_{floor:02}")),
        ),
        (
            "FloorName".into(),
            VariableValue::String(format!("Floor {floor:02}")),
        ),
        ("ComfortLevel".into(), VariableValue::Float(comfort.into())),
    ])
}

pub async fn projected_floor_alert_snapshot_tracks_room_migration(
    config: &(impl QueryTestConfig + Send),
) {
    let query = build_comfort_query(TUTORIAL_FLOOR_ALERT, config).await;
    let mut snapshot = KeyedSnapshot::new();
    seed_comfort(&query, &mut snapshot, 3, 3).await;
    change_room(&query, &mut snapshot, 1, 1, true).await;
    assert_eq!(snapshot.len(), 1);
    let first_key = *snapshot.keys().next().unwrap();
    change_room(&query, &mut snapshot, 3, 1, true).await;
    assert_eq!(snapshot.len(), 2);
    let original = snapshot.clone();
    let mut unaffected = original.clone();
    assert_eq!(
        unaffected.remove(&first_key),
        Some(floor_alert_row(1, 32.0))
    );

    let mut properties = tutorial_room_properties(1, 1, true);
    properties["floor_id"] = json!("floor_01_02");
    let results = apply_to_snapshot(
        &query,
        &mut snapshot,
        update_tutorial_node("room_01_01_01", "Room", properties),
    )
    .await;
    assert_eq!(
        results.len(),
        2,
        "one change must affect two floor rows: {results:?}; snapshot: {snapshot:?}"
    );
    assert!(results.contains(&QueryPartEvaluationContext::Removing {
        before: floor_alert_row(1, 32.0),
        row_signature: first_key,
    }));
    assert_eq!(snapshot.len(), 2);
    let second_key = *snapshot
        .keys()
        .find(|key| !unaffected.contains_key(key))
        .unwrap();
    assert_ne!(first_key, second_key);
    assert!(results.contains(&QueryPartEvaluationContext::Adding {
        after: floor_alert_row(2, 35.5),
        row_signature: second_key,
    }));
    let mut expected = unaffected;
    expected.insert(second_key, floor_alert_row(2, 35.5));
    assert_eq!(snapshot, expected, "the third floor must stay unchanged");

    let results = apply_to_snapshot(
        &query,
        &mut snapshot,
        update_tutorial_node(
            "room_01_01_01",
            "Room",
            tutorial_room_properties(1, 1, true),
        ),
    )
    .await;
    assert_eq!(results.len(), 2);
    assert!(results.contains(&QueryPartEvaluationContext::Removing {
        before: floor_alert_row(2, 35.5),
        row_signature: second_key,
    }));
    assert!(results.contains(&QueryPartEvaluationContext::Adding {
        after: floor_alert_row(1, 32.0),
        row_signature: first_key,
    }));
    assert_eq!(
        snapshot, original,
        "both directions must preserve group keys"
    );
}
