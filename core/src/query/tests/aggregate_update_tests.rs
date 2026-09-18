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
        functions::{Count, Function, FunctionRegistry, Sum},
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
