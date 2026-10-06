// Copyright 2026 The Drasi Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{collections::HashMap, sync::Arc};

use drasi_query_cypher::CypherParser;
use serde_json::json;

use crate::{
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::{Count, Function, FunctionRegistry, Max, Min, Sum},
        variable_value::VariableValue,
    },
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};

struct MaterializedQuery {
    query: ContinuousQuery,
    rows: HashMap<u64, QueryVariables>,
}

impl MaterializedQuery {
    async fn new(query_text: &str) -> Self {
        let functions = Arc::new(FunctionRegistry::new());
        functions.register_function("count", Function::Aggregating(Arc::new(Count {})));
        functions.register_function("sum", Function::Aggregating(Arc::new(Sum {})));
        functions.register_function("min", Function::Aggregating(Arc::new(Min {})));
        functions.register_function("max", Function::Aggregating(Arc::new(Max {})));
        let parser = Arc::new(CypherParser::new(functions.clone()));
        let element_index = Arc::new(InMemoryElementIndex::new());
        let query = QueryBuilder::new(query_text, parser)
            .with_function_registry(functions)
            .with_element_index(element_index.clone())
            .with_archive_index(element_index)
            .with_result_index(Arc::new(InMemoryResultIndex::new()))
            .with_future_queue(Arc::new(InMemoryFutureQueue::new()))
            .build()
            .await;
        Self {
            query,
            rows: HashMap::new(),
        }
    }

    async fn process(&mut self, change: SourceChange) -> Vec<QueryPartEvaluationContext> {
        let changes = self.query.process_source_change(change).await.unwrap();
        for change in &changes {
            match change {
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
                    self.rows.insert(*row_signature, after.clone());
                }
                QueryPartEvaluationContext::Removing { row_signature, .. } => {
                    self.rows.remove(row_signature);
                }
                QueryPartEvaluationContext::Noop => {}
            }
        }
        changes
    }
}

fn item_change(
    update: bool,
    id: &str,
    effective_from: u64,
    is_open: bool,
    state_reason: Option<&str>,
) -> SourceChange {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", id),
            labels: Arc::new([Arc::from("Item")]),
            effective_from,
        },
        properties: ElementPropertyMap::from(json!({
            "id": id,
            "isOpen": is_open,
            "stateReason": state_reason,
        })),
    };
    if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    }
}

fn value<'a>(row: &'a QueryVariables, key: &str) -> &'a VariableValue {
    row.get(key).unwrap()
}

fn assert_item_row(
    row: &QueryVariables,
    id: &str,
    is_open: bool,
    state_reason: Option<&str>,
    count: Option<i64>,
) {
    assert_eq!(value(row, "id"), &VariableValue::from(json!(id)));
    assert_eq!(value(row, "isOpen"), &VariableValue::from(json!(is_open)));
    assert_eq!(
        value(row, "stateReason"),
        &VariableValue::from(json!(state_reason))
    );
    if let Some(count) = count {
        assert_eq!(value(row, "itemCount"), &VariableValue::from(json!(count)));
    }
}

fn assert_group_row(row: &QueryVariables, is_open: bool, state_reason: Option<&str>, count: i64) {
    assert_eq!(value(row, "isOpen"), &VariableValue::from(json!(is_open)));
    assert_eq!(
        value(row, "stateReason"),
        &VariableValue::from(json!(state_reason))
    );
    assert_eq!(value(row, "itemCount"), &VariableValue::from(json!(count)));
}

#[tokio::test]
async fn non_aggregate_replacement_update_adds_newly_matching_row() {
    let mut subject = MaterializedQuery::new(
        "MATCH (item:Item)
         WHERE NOT item.isOpen AND item.stateReason = 'completed'
         RETURN item.id, item.isOpen, item.stateReason",
    )
    .await;

    let insert = subject
        .process(item_change(false, "item", 1, true, None))
        .await;
    assert!(insert.is_empty());

    let update = subject
        .process(item_change(true, "item", 2, false, Some("completed")))
        .await;
    assert_eq!(update.len(), 1);
    assert!(matches!(
        &update[0],
        QueryPartEvaluationContext::Adding { after, .. }
            if {
                assert_item_row(after, "item", false, Some("completed"), None);
                true
            }
    ));
    assert_eq!(subject.rows.len(), 1);
}

#[tokio::test]
async fn whole_element_grouping_updates_in_place_by_reference() {
    let mut subject = MaterializedQuery::new(
        "MATCH (item:Item)
         WITH item, count(item) AS itemCount
         RETURN item.id, item.isOpen, item.stateReason, itemCount",
    )
    .await;

    let insert = subject
        .process(item_change(false, "item", 1, true, None))
        .await;
    assert_eq!(insert.len(), 1);
    let row_signature = insert[0].row_signature();

    let update = subject
        .process(item_change(true, "item", 2, false, Some("completed")))
        .await;
    assert_eq!(update.len(), 1);
    assert!(matches!(
        &update[0],
        QueryPartEvaluationContext::Updating {
            before,
            after,
            row_signature: update_signature,
        } if {
            assert_item_row(before, "item", true, None, Some(1));
            assert_item_row(after, "item", false, Some("completed"), Some(1));
            *update_signature == row_signature
        }
    ));
    assert_eq!(subject.rows.len(), 1);
    assert_item_row(
        subject.rows.get(&row_signature).unwrap(),
        "item",
        false,
        Some("completed"),
        Some(1),
    );

    let revert = subject
        .process(item_change(true, "item", 3, true, None))
        .await;
    assert_eq!(revert.len(), 1);
    assert!(matches!(
        &revert[0],
        QueryPartEvaluationContext::Updating {
            before,
            after,
            row_signature: revert_signature,
        } if {
            assert_item_row(before, "item", false, Some("completed"), Some(1));
            assert_item_row(after, "item", true, None, Some(1));
            *revert_signature == row_signature
        }
    ));
    assert_eq!(subject.rows.len(), 1);
    assert_item_row(
        subject.rows.get(&row_signature).unwrap(),
        "item",
        true,
        None,
        Some(1),
    );
}

#[tokio::test]
async fn scalar_group_migration_adds_destination_and_removes_drained_source() {
    let mut subject = MaterializedQuery::new(
        "MATCH (item:Item)
         WITH item.id AS id, item.isOpen AS isOpen,
              item.stateReason AS stateReason, count(item) AS itemCount
         RETURN id, isOpen, stateReason, itemCount",
    )
    .await;

    let insert = subject
        .process(item_change(false, "item", 1, true, None))
        .await;
    assert_eq!(insert.len(), 1);
    let open_signature = insert[0].row_signature();

    let update = subject
        .process(item_change(true, "item", 2, false, Some("completed")))
        .await;
    assert_eq!(update.len(), 2);
    let closed_signature = update
        .iter()
        .find_map(|change| match change {
            QueryPartEvaluationContext::Adding {
                after,
                row_signature,
            } => {
                assert_item_row(after, "item", false, Some("completed"), Some(1));
                Some(*row_signature)
            }
            _ => None,
        })
        .expect("destination group should be added");
    assert!(update.iter().any(|change| matches!(
        change,
        QueryPartEvaluationContext::Removing {
            before,
            row_signature,
        } if {
            assert_item_row(before, "item", true, None, Some(1));
            *row_signature == open_signature
        }
    )));
    assert_ne!(closed_signature, open_signature);
    assert_eq!(subject.rows.len(), 1);
    assert_item_row(
        subject.rows.get(&closed_signature).unwrap(),
        "item",
        false,
        Some("completed"),
        Some(1),
    );

    let revert = subject
        .process(item_change(true, "item", 3, true, None))
        .await;
    assert_eq!(revert.len(), 2);
    assert!(revert.iter().any(|change| matches!(
        change,
        QueryPartEvaluationContext::Adding {
            after,
            row_signature,
        } if {
            assert_item_row(after, "item", true, None, Some(1));
            *row_signature == open_signature
        }
    )));
    assert!(revert.iter().any(|change| matches!(
        change,
        QueryPartEvaluationContext::Removing {
            before,
            row_signature,
        } if {
            assert_item_row(before, "item", false, Some("completed"), Some(1));
            *row_signature == closed_signature
        }
    )));
    assert_eq!(subject.rows.len(), 1);
    assert_item_row(
        subject.rows.get(&open_signature).unwrap(),
        "item",
        true,
        None,
        Some(1),
    );
}

#[tokio::test]
async fn scalar_group_migration_updates_populated_source_and_destination() {
    let mut subject = MaterializedQuery::new(
        "MATCH (item:Item)
         WITH item.isOpen AS isOpen, item.stateReason AS stateReason,
              count(item) AS itemCount
         RETURN isOpen, stateReason, itemCount",
    )
    .await;

    subject
        .process(item_change(false, "open-1", 1, true, None))
        .await;
    subject
        .process(item_change(false, "open-2", 2, true, None))
        .await;
    subject
        .process(item_change(false, "closed-1", 3, false, Some("completed")))
        .await;
    assert_eq!(subject.rows.len(), 2);

    let update = subject
        .process(item_change(true, "open-1", 4, false, Some("completed")))
        .await;
    assert_eq!(update.len(), 2);

    let closed_update = update
        .iter()
        .find_map(|change| match change {
            QueryPartEvaluationContext::Updating {
                before,
                after,
                row_signature,
            } if value(after, "isOpen") == &VariableValue::from(json!(false)) => {
                Some((before, after, *row_signature))
            }
            _ => None,
        })
        .expect("populated destination should be updated");
    assert_group_row(closed_update.0, false, Some("completed"), 1);
    assert_group_row(closed_update.1, false, Some("completed"), 2);

    let open_update = update
        .iter()
        .find_map(|change| match change {
            QueryPartEvaluationContext::Updating {
                before,
                after,
                row_signature,
            } if value(after, "isOpen") == &VariableValue::from(json!(true)) => {
                Some((before, after, *row_signature))
            }
            _ => None,
        })
        .expect("populated source should be updated");
    assert_group_row(open_update.0, true, None, 2);
    assert_group_row(open_update.1, true, None, 1);
    assert_ne!(closed_update.2, open_update.2);
    assert_eq!(subject.rows.len(), 2);
    assert!(subject.rows.values().any(|row| value(row, "isOpen")
        == &VariableValue::from(json!(true))
        && value(row, "itemCount") == &VariableValue::from(json!(1))));
    assert!(subject.rows.values().any(|row| value(row, "isOpen")
        == &VariableValue::from(json!(false))
        && value(row, "itemCount") == &VariableValue::from(json!(2))));
}

// Minimize Trading's joined positions to their value/cost contributions, retaining
// the WITH aggregation followed by an ordinary RETURN that exposes #680.
const PORTFOLIO_SUMMARY: &str = "
    MATCH (p:Position)
    WITH sum(p.value) AS totalValue, sum(p.cost) AS totalCost, count(p) AS positionCount
    RETURN totalValue, totalCost, positionCount
";

fn position_change(
    update: bool,
    id: &str,
    effective_from: u64,
    account: &str,
    value: i64,
    cost: i64,
) -> SourceChange {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", id),
            labels: Arc::new([Arc::from("Position")]),
            effective_from,
        },
        properties: ElementPropertyMap::from(json!({
            "account": account,
            "value": value,
            "cost": cost,
        })),
    };
    if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    }
}

fn delete_position(id: &str, effective_from: u64) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", id),
            labels: Arc::new([Arc::from("Position")]),
            effective_from,
        },
    }
}

fn summary(total_value: f64, total_cost: f64, position_count: i64) -> QueryVariables {
    QueryVariables::from([
        ("totalValue".into(), VariableValue::from(json!(total_value))),
        ("totalCost".into(), VariableValue::from(json!(total_cost))),
        (
            "positionCount".into(),
            VariableValue::from(json!(position_count)),
        ),
    ])
}

fn assert_summary_delta(
    changes: &[QueryPartEvaluationContext],
    signature: u64,
    before: Option<QueryVariables>,
    after: Option<QueryVariables>,
) {
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].row_signature(), signature);
    let expected = match (before, after) {
        (None, Some(after)) => QueryPartEvaluationContext::Adding {
            after,
            row_signature: signature,
        },
        (Some(before), Some(after)) => QueryPartEvaluationContext::Updating {
            before,
            after,
            row_signature: signature,
        },
        (Some(before), None) => QueryPartEvaluationContext::Removing {
            before,
            row_signature: signature,
        },
        (None, None) => panic!("a summary delta must have a before or after row"),
    };
    assert_eq!(changes, &[expected]);
}

#[tokio::test]
async fn aggregate_snapshot_global_sum_tracks_bootstrap_updates_and_deletes() {
    let mut subject = MaterializedQuery::new(PORTFOLIO_SUMMARY).await;
    assert!(subject.rows.is_empty());

    let first = subject
        .process(position_change(false, "aapl", 1, "portfolio", 1100, 800))
        .await;
    assert_eq!(first.len(), 1);
    let signature = first[0].row_signature();
    assert_summary_delta(&first, signature, None, Some(summary(1100.0, 800.0, 1)));
    assert_eq!(
        subject.rows,
        HashMap::from([(signature, summary(1100.0, 800.0, 1))])
    );

    let second = subject
        .process(position_change(false, "msft", 2, "portfolio", 900, 1000))
        .await;
    assert_eq!(
        subject.rows,
        HashMap::from([(signature, summary(2000.0, 1800.0, 2))]),
        "bootstrap must replace the intermediate 1100 row, not retain it"
    );
    assert_summary_delta(
        &second,
        signature,
        Some(summary(1100.0, 800.0, 1)),
        Some(summary(2000.0, 1800.0, 2)),
    );

    for (time, id, value, cost, before, after) in [
        (3, "aapl", 1150, 800, 2000.0, 2050.0),
        (4, "aapl", 1250, 800, 2050.0, 2150.0),
        (5, "msft", 950, 1000, 2150.0, 2200.0),
        (6, "msft", 900, 1000, 2200.0, 2150.0),
    ] {
        let changes = subject
            .process(position_change(true, id, time, "portfolio", value, cost))
            .await;
        assert_eq!(
            subject.rows,
            HashMap::from([(signature, summary(after, 1800.0, 2))]),
            "the current snapshot must contain no historical aggregate rows"
        );
        assert_summary_delta(
            &changes,
            signature,
            Some(summary(before, 1800.0, 2)),
            Some(summary(after, 1800.0, 2)),
        );
    }

    let deletion = subject.process(delete_position("msft", 7)).await;
    assert_summary_delta(
        &deletion,
        signature,
        Some(summary(2150.0, 1800.0, 2)),
        Some(summary(1250.0, 800.0, 1)),
    );
    assert_eq!(
        subject.rows,
        HashMap::from([(signature, summary(1250.0, 800.0, 1))])
    );

    let last_deletion = subject.process(delete_position("aapl", 8)).await;
    assert_summary_delta(
        &last_deletion,
        signature,
        Some(summary(1250.0, 800.0, 1)),
        None,
    );
    assert!(subject.rows.is_empty());

    let reinsert = subject
        .process(position_change(false, "aapl", 9, "portfolio", 1100, 800))
        .await;
    assert_summary_delta(&reinsert, signature, None, Some(summary(1100.0, 800.0, 1)));
    subject
        .process(position_change(false, "msft", 10, "portfolio", 900, 1000))
        .await;
    assert_eq!(
        subject.rows,
        HashMap::from([(signature, summary(2000.0, 1800.0, 2))])
    );

    let mut fresh = MaterializedQuery::new(PORTFOLIO_SUMMARY).await;
    fresh
        .process(position_change(false, "msft", 10, "portfolio", 900, 1000))
        .await;
    fresh
        .process(position_change(false, "aapl", 9, "portfolio", 1100, 800))
        .await;
    assert_eq!(subject.rows, fresh.rows);
}

#[tokio::test]
async fn aggregate_snapshot_equal_valued_groups_keep_distinct_identities() {
    let mut subject = MaterializedQuery::new(
        "MATCH (p:Position)
         WITH p.account AS account, sum(p.value) AS totalValue,
              sum(p.cost) AS totalCost, count(p) AS positionCount
         RETURN totalValue, totalCost, positionCount",
    )
    .await;

    let first = subject
        .process(position_change(false, "a", 1, "account-a", 100, 80))
        .await;
    let a_signature = first[0].row_signature();
    let second = subject
        .process(position_change(false, "b", 2, "account-b", 100, 80))
        .await;
    let b_signature = second[0].row_signature();
    assert_ne!(a_signature, b_signature);
    assert_eq!(
        subject.rows,
        HashMap::from([
            (a_signature, summary(100.0, 80.0, 1)),
            (b_signature, summary(100.0, 80.0, 1)),
        ]),
        "equal projected values do not make two independent groups the same row"
    );

    let update = subject
        .process(position_change(true, "b", 3, "account-b", 150, 80))
        .await;
    assert_summary_delta(
        &update,
        b_signature,
        Some(summary(100.0, 80.0, 1)),
        Some(summary(150.0, 80.0, 1)),
    );
    assert_eq!(
        subject.rows,
        HashMap::from([
            (a_signature, summary(100.0, 80.0, 1)),
            (b_signature, summary(150.0, 80.0, 1)),
        ])
    );

    let migration = subject
        .process(position_change(true, "b", 4, "account-a", 100, 80))
        .await;
    assert_eq!(migration.len(), 2);
    assert!(migration.iter().any(|change| matches!(
        change,
        QueryPartEvaluationContext::Removing { before, row_signature }
            if *row_signature == b_signature && before == &summary(150.0, 80.0, 1)
    )));
    assert!(migration.iter().any(|change| matches!(
        change,
        QueryPartEvaluationContext::Updating { before, after, row_signature }
            if *row_signature == a_signature
                && before == &summary(100.0, 80.0, 1)
                && after == &summary(200.0, 160.0, 2)
    )));
    assert_eq!(
        subject.rows,
        HashMap::from([(a_signature, summary(200.0, 160.0, 2))])
    );
}

#[tokio::test]
async fn aggregate_snapshot_terminal_aggregation_keeps_existing_empty_group_semantics() {
    let mut subject = MaterializedQuery::new(
        "MATCH (p:Position)
         RETURN sum(p.value) AS totalValue, sum(p.cost) AS totalCost,
                count(p) AS positionCount",
    )
    .await;
    let first = subject
        .process(position_change(false, "aapl", 1, "portfolio", 1100, 800))
        .await;
    let signature = first[0].row_signature();
    let deletion = subject.process(delete_position("aapl", 2)).await;

    // Unlike aggregation followed by projection, terminal aggregations retain
    // identity-valued rows today. Changing that requires #384/#409, not #680.
    assert_eq!(deletion.len(), 1);
    assert!(matches!(
        &deletion[0],
        QueryPartEvaluationContext::Aggregation { after, default_after: true, row_signature, .. }
            if *row_signature == signature && after == &summary(0.0, 0.0, 0)
    ));
    assert_eq!(
        subject.rows,
        HashMap::from([(signature, summary(0.0, 0.0, 0))])
    );
}

fn grouped_position(id: &str, group: serde_json::Value, value: i64, time: u64) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", id),
            labels: Arc::from([Arc::from("Position")]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(json!({"group": group, "value": value})),
    }
}

#[tokio::test]
async fn grouping_numeric_representation_update_keeps_one_identity() {
    for (before_group, after_group) in [(json!(1), json!(1.0)), (json!(1.0), json!(1))] {
        let mut subject = MaterializedQuery::new(
            "MATCH (p:Position)
             WITH p.group AS groupKey, sum(p.value) AS totalValue
             RETURN groupKey, totalValue",
        )
        .await;
        let first = subject
            .process(SourceChange::Insert {
                element: grouped_position("p", before_group.clone(), 10, 1),
            })
            .await;
        assert_eq!(first.len(), 1);
        let signature = first[0].row_signature();

        let update = subject
            .process(SourceChange::Update {
                element: grouped_position("p", after_group.clone(), 20, 2),
            })
            .await;
        assert_eq!(
            subject.rows.len(),
            1,
            "equivalent numeric keys are one group"
        );
        assert_eq!(update.len(), 1);
        assert_eq!(update[0].row_signature(), signature);
        assert_eq!(
            value(&subject.rows[&signature], "totalValue"),
            &VariableValue::from(json!(20.0))
        );

        subject
            .process(SourceChange::Insert {
                element: grouped_position("q", before_group, 5, 3),
            })
            .await;
        assert_eq!(subject.rows.len(), 1);
        assert_eq!(
            value(&subject.rows[&signature], "totalValue"),
            &VariableValue::from(json!(25.0))
        );

        let deletion = subject.process(delete_position("p", 4)).await;
        assert_eq!(deletion.len(), 1);
        assert_eq!(deletion[0].row_signature(), signature);
        assert_eq!(subject.rows.len(), 1);
        assert_eq!(
            value(&subject.rows[&signature], "totalValue"),
            &VariableValue::from(json!(5.0))
        );
    }
}

#[tokio::test]
async fn terminal_aggregate_suppresses_unchanged_zero_deletion() {
    let mut subject =
        MaterializedQuery::new("MATCH (p:Position) RETURN sum(p.value) AS totalValue").await;
    let first = subject
        .process(position_change(false, "p", 1, "a", 0, 0))
        .await;
    assert!(
        matches!(first.as_slice(), [QueryPartEvaluationContext::Aggregation {
        default_before: true, after, ..
    }] if value(after, "totalValue") == &VariableValue::from(json!(0.0)))
    );
    let initial = subject.rows.clone();
    let deletion = subject.process(delete_position("p", 2)).await;
    assert!(
        deletion.is_empty(),
        "an existing terminal zero result must not notify 0 -> 0: {deletion:?}"
    );
    assert_eq!(subject.rows, initial);

    let added = subject
        .process(position_change(false, "q", 3, "a", 10, 0))
        .await;
    assert_eq!(added.len(), 1);
    assert_eq!(added[0].row_signature(), first[0].row_signature());
    let deletion = subject.process(delete_position("q", 4)).await;
    assert!(
        matches!(deletion.as_slice(), [QueryPartEvaluationContext::Aggregation {
        before: Some(before), after, default_after: true, ..
    }] if value(before, "totalValue") == &VariableValue::from(json!(10.0))
        && value(after, "totalValue") == &VariableValue::from(json!(0.0)))
    );
    assert_eq!(
        subject.rows, initial,
        "terminal empty-group retention is unchanged"
    );
}

#[tokio::test]
async fn projected_zero_aggregate_retains_internal_default_transition() {
    let mut subject = MaterializedQuery::new(
        "MATCH (p:Position) WITH sum(p.value) AS totalValue RETURN totalValue",
    )
    .await;
    let first = subject
        .process(position_change(false, "p", 1, "a", 0, 0))
        .await;
    assert!(matches!(
        first.as_slice(),
        [QueryPartEvaluationContext::Adding { after, .. }]
            if value(after, "totalValue") == &VariableValue::from(json!(0.0))
    ));
    let signature = first[0].row_signature();
    let deleted = subject.process(delete_position("p", 2)).await;
    assert!(matches!(
        deleted.as_slice(),
        [QueryPartEvaluationContext::Removing { before, row_signature }]
            if *row_signature == signature
                && value(before, "totalValue") == &VariableValue::from(json!(0.0))
    ));
    assert!(subject.rows.is_empty());
}

#[tokio::test]
async fn grouping_numeric_compound_contributors_share_accumulators_and_drain() {
    for (integer, float) in [
        (json!(1), json!(1.0)),
        (json!(0), json!(-0.0)),
        (
            json!(9_007_199_254_740_992_i64),
            json!(9_007_199_254_740_992.0),
        ),
        (
            json!([1, {"nested": [0, -1]}]),
            json!([1.0, {"nested": [-0.0, -1.0]}]),
        ),
    ] {
        for (first_group, second_group) in [(integer.clone(), float.clone()), (float, integer)] {
            for projection in ["groupKey, totalValue, low, high", "totalValue, low, high"] {
                let mut subject = MaterializedQuery::new(&format!(
                    "MATCH (p:Position)
                     WITH p.group AS groupKey, sum(p.value) AS totalValue,
                          min(p.value) AS low, max(p.value) AS high
                     RETURN {projection}"
                ))
                .await;
                let first = subject
                    .process(SourceChange::Insert {
                        element: grouped_position("p", first_group.clone(), 10, 1),
                    })
                    .await;
                let signature = first[0].row_signature();
                let second = subject
                    .process(SourceChange::Insert {
                        element: grouped_position("q", second_group.clone(), 5, 2),
                    })
                    .await;
                assert_eq!(second.len(), 1);
                assert_eq!(second[0].row_signature(), signature);
                assert_eq!(subject.rows.len(), 1);
                assert_eq!(
                    value(&subject.rows[&signature], "totalValue"),
                    &VariableValue::from(json!(15.0))
                );
                assert_eq!(
                    value(&subject.rows[&signature], "low"),
                    &VariableValue::from(json!(5.0))
                );
                assert_eq!(
                    value(&subject.rows[&signature], "high"),
                    &VariableValue::from(json!(10.0))
                );

                let unchanged = subject
                    .process(SourceChange::Update {
                        element: grouped_position("p", second_group.clone(), 10, 3),
                    })
                    .await;
                assert!(unchanged.is_empty(), "{unchanged:?}");
                let changed = subject
                    .process(SourceChange::Update {
                        element: grouped_position("p", second_group.clone(), 20, 4),
                    })
                    .await;
                assert_eq!(changed.len(), 1);
                assert_eq!(changed[0].row_signature(), signature);
                assert_eq!(
                    value(&subject.rows[&signature], "totalValue"),
                    &VariableValue::from(json!(25.0))
                );
                assert_eq!(
                    value(&subject.rows[&signature], "high"),
                    &VariableValue::from(json!(20.0))
                );

                subject.process(delete_position("q", 5)).await;
                let drained = subject.process(delete_position("p", 6)).await;
                assert!(
                    matches!(drained.as_slice(), [QueryPartEvaluationContext::Removing { row_signature, .. }] if *row_signature == signature),
                    "{drained:?}"
                );
                assert!(
                    subject.rows.is_empty(),
                    "normalized group must drain its original baseline"
                );
            }
        }
    }
}

#[tokio::test]
async fn grouping_numeric_precision_boundary_migrates_through_scalar_projection() {
    let exact = json!(9_007_199_254_740_992.0);
    let distinct = json!(9_007_199_254_740_993_i64);
    for (first_group, second_group) in [(exact.clone(), distinct.clone()), (distinct, exact)] {
        let mut subject = MaterializedQuery::new(
            "MATCH (p:Position)
             WITH p.group AS key, p.value AS value
             WITH key AS groupKey, sum(value) AS totalValue
             RETURN totalValue",
        )
        .await;
        let first = subject
            .process(SourceChange::Insert {
                element: grouped_position("p", first_group, 10, 1),
            })
            .await;
        let old_signature = first[0].row_signature();
        let migrated = subject
            .process(SourceChange::Update {
                element: grouped_position("p", second_group, 10, 2),
            })
            .await;
        assert_eq!(
            migrated.len(),
            2,
            "distinct exact numeric identities must migrate: {migrated:?}"
        );
        assert!(migrated.iter().any(
            |change| matches!(change, QueryPartEvaluationContext::Removing {
            row_signature, ..
        } if *row_signature == old_signature)
        ));
        assert_eq!(subject.rows.len(), 1);
        assert!(!subject.rows.contains_key(&old_signature));
        assert_eq!(
            value(subject.rows.values().next().unwrap(), "totalValue"),
            &VariableValue::from(json!(10.0))
        );
    }
}

#[tokio::test]
async fn grouping_numeric_signed_and_large_positive_boundaries_are_distinct() {
    let mut subject = MaterializedQuery::new(
        "MATCH (p:Position)
             WITH p.group AS groupKey, sum(p.value) AS totalValue
             RETURN groupKey, totalValue",
    )
    .await;
    let negative = subject
        .process(SourceChange::Insert {
            element: grouped_position("negative", json!(i64::MIN), 10, 1),
        })
        .await;
    let positive = subject
        .process(SourceChange::Insert {
            element: grouped_position("positive", json!(9_223_372_036_854_775_808.0), 20, 2),
        })
        .await;
    assert_eq!(negative.len(), 1);
    assert_eq!(positive.len(), 1);
    assert_ne!(negative[0].row_signature(), positive[0].row_signature());
    assert_eq!(subject.rows.len(), 2);
    assert_eq!(
        value(&subject.rows[&negative[0].row_signature()], "totalValue"),
        &VariableValue::from(json!(10.0))
    );
    assert_eq!(
        value(&subject.rows[&positive[0].row_signature()], "totalValue"),
        &VariableValue::from(json!(20.0))
    );
    let negative_signature = negative[0].row_signature();
    let positive_signature = positive[0].row_signature();
    for (id, group, number, time, signature) in [
        (
            "negative",
            json!(i64::MIN as f64),
            15,
            3,
            negative_signature,
        ),
        (
            "positive",
            json!(9_223_372_036_854_775_808.0),
            25,
            4,
            positive_signature,
        ),
    ] {
        let changes = subject
            .process(SourceChange::Update {
                element: grouped_position(id, group, number, time),
            })
            .await;
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].row_signature(), signature);
        assert_eq!(subject.rows.len(), 2);
        assert_eq!(
            value(&subject.rows[&signature], "totalValue"),
            &VariableValue::from(json!(number as f64))
        );
    }
    for (id, time, signature, remaining) in [
        ("negative", 5, negative_signature, 1),
        ("positive", 6, positive_signature, 0),
    ] {
        let changes = subject.process(delete_position(id, time)).await;
        assert!(
            matches!(changes.as_slice(), [QueryPartEvaluationContext::Removing { row_signature, .. }] if *row_signature == signature)
        );
        assert!(!subject.rows.contains_key(&signature));
        assert_eq!(subject.rows.len(), remaining);
    }
}

fn account_summary(account: &str, total: f64) -> QueryVariables {
    QueryVariables::from([
        ("account".into(), VariableValue::from(json!(account))),
        ("totalValue".into(), VariableValue::from(json!(total))),
    ])
}

#[tokio::test]
async fn chained_aggregate_groups_replace_each_identity_domain() {
    let mut subject = MaterializedQuery::new(
        "MATCH (p:Position)
         WITH p.group[0] AS account, p.group[1] AS desk, sum(p.value) AS deskValue
         WITH account, sum(deskValue) AS totalValue
         RETURN account, totalValue",
    )
    .await;
    let first = subject
        .process(SourceChange::Insert {
            element: grouped_position("p", json!(["a", "x"]), 10, 1),
        })
        .await;
    let a = first[0].row_signature();
    let second = subject
        .process(SourceChange::Insert {
            element: grouped_position("q", json!(["a", "y"]), 20, 2),
        })
        .await;
    assert_summary_delta(
        &second,
        a,
        Some(account_summary("a", 10.0)),
        Some(account_summary("a", 30.0)),
    );
    let third = subject
        .process(SourceChange::Insert {
            element: grouped_position("r", json!(["b", "y"]), 7, 3),
        })
        .await;
    let b = third[0].row_signature();
    assert_ne!(a, b);
    assert_eq!(
        subject.rows,
        HashMap::from([
            (a, account_summary("a", 30.0)),
            (b, account_summary("b", 7.0))
        ])
    );

    let changed = subject
        .process(SourceChange::Update {
            element: grouped_position("p", json!(["a", "x"]), 15, 4),
        })
        .await;
    assert_summary_delta(
        &changed,
        a,
        Some(account_summary("a", 30.0)),
        Some(account_summary("a", 35.0)),
    );

    let migrated = subject
        .process(SourceChange::Update {
            element: grouped_position("p", json!(["b", "z"]), 15, 5),
        })
        .await;
    assert_eq!(migrated.len(), 2);
    for (signature, account, before, after) in [(a, "a", 35.0, 20.0), (b, "b", 7.0, 22.0)] {
        let change = migrated
            .iter()
            .find(|c| c.row_signature() == signature)
            .unwrap();
        assert_summary_delta(
            std::slice::from_ref(change),
            signature,
            Some(account_summary(account, before)),
            Some(account_summary(account, after)),
        );
    }
    assert_eq!(
        subject.rows,
        HashMap::from([
            (a, account_summary("a", 20.0)),
            (b, account_summary("b", 22.0))
        ])
    );
    let deleted = subject.process(delete_position("q", 6)).await;
    // Existing chained-empty behavior, also verified on the unchanged baseline:
    // the last contribution updates the outer group to zero rather than removing it.
    assert_summary_delta(
        &deleted,
        a,
        Some(account_summary("a", 20.0)),
        Some(account_summary("a", 0.0)),
    );
    assert_eq!(
        subject.rows,
        HashMap::from([
            (a, account_summary("a", 0.0)),
            (b, account_summary("b", 22.0)),
        ])
    );
}

#[tokio::test]
async fn future_reprocess_unchanged_aggregate_reaches_downstream_filter() {
    let mut subject = MaterializedQuery::new(
        "MATCH (p:Position)
         WITH p.group AS groupKey, sum(p.value) AS totalValue
         WHERE drasi.trueFor(totalValue >= 0, 10)
         RETURN groupKey, totalValue",
    )
    .await;
    let inserted = subject
        .process(SourceChange::Insert {
            element: grouped_position("p", json!("a"), 0, 1),
        })
        .await;
    assert!(inserted.is_empty());
    assert!(subject.rows.is_empty());
    let queue = subject.query.future_queue();
    assert_eq!(queue.peek_due_time().await.unwrap(), Some(11));
    let future_ref = queue
        .pop()
        .await
        .unwrap()
        .expect("filter must schedule reprocessing");
    assert_eq!(future_ref.original_time, 1);
    assert_eq!(future_ref.due_time, 11);
    let emitted = subject.process(SourceChange::Future { future_ref }).await;
    assert!(
        matches!(emitted.as_slice(), [QueryPartEvaluationContext::Adding { after, .. }]
        if value(after, "totalValue") == &VariableValue::from(json!(0.0)))
    );
    assert_eq!(
        subject.rows.len(),
        1,
        "unchanged aggregate input must still cross the clock-dependent filter"
    );
    assert_eq!(queue.peek_due_time().await.unwrap(), None);
}

#[tokio::test]
async fn future_reprocess_preserves_terminal_unchanged_value_policy() {
    let mut subject = MaterializedQuery::new(
        "MATCH (p:Position)
         WITH p, drasi.trueUntil(true, 11) AS live
         RETURN sum(CASE WHEN live THEN p.value ELSE 0 END) AS totalValue",
    )
    .await;
    let inserted = subject
        .process(SourceChange::Insert {
            element: grouped_position("p", json!("a"), 0, 1),
        })
        .await;
    assert_eq!(inserted.len(), 1, "initial zero aggregate must be emitted");
    let before = subject.rows.clone();
    let queue = subject.query.future_queue();
    let future_ref = queue
        .pop()
        .await
        .unwrap()
        .expect("expiry must schedule a future");
    assert_eq!(future_ref.due_time, 11);
    let expired = subject.process(SourceChange::Future { future_ref }).await;
    assert!(
        expired.is_empty(),
        "clock-driven reevaluation must not notify an unchanged terminal total"
    );
    assert_eq!(subject.rows, before);
    assert_eq!(queue.peek_due_time().await.unwrap(), None);
}

#[tokio::test]
async fn grouping_lazy_extrema_use_typed_keys_and_stable_element_references() {
    let mut subject = MaterializedQuery::new(
        "MATCH (p:Position)
         WITH p.group AS groupKey, min(p.value) AS low, max(p.value) AS high
         RETURN groupKey, low, high",
    )
    .await;
    for (id, group, number, time) in [("p", json!(1), 10, 1), ("q", json!("1"), 20, 2)] {
        subject
            .process(SourceChange::Insert {
                element: grouped_position(id, group, number, time),
            })
            .await;
    }
    assert_eq!(subject.rows.len(), 2);
    assert!(subject.rows.values().any(|row| value(row, "groupKey")
        == &VariableValue::from(json!("1"))
        && value(row, "low") == &VariableValue::from(json!(20))));

    let mut subject = MaterializedQuery::new(
        "MATCH (p:Position) WITH p, min(p.value) AS low, max(p.value) AS high
         RETURN low, high",
    )
    .await;
    let first = subject
        .process(SourceChange::Insert {
            element: grouped_position("p", json!(1), 10, 1),
        })
        .await;
    let signature = first[0].row_signature();
    let changed = subject
        .process(SourceChange::Update {
            element: grouped_position("p", json!(1), 20, 2),
        })
        .await;
    assert_eq!(changed.len(), 1);
    assert_eq!(changed[0].row_signature(), signature);
    assert_eq!(
        value(&subject.rows[&signature], "low"),
        &VariableValue::from(json!(20))
    );
    assert_eq!(
        value(&subject.rows[&signature], "high"),
        &VariableValue::from(json!(20))
    );
    subject.process(delete_position("p", 3)).await;
    assert!(subject.rows.is_empty());
}
