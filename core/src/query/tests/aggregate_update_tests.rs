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
        functions::{Count, Function, FunctionRegistry},
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
