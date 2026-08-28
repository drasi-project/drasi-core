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
        functions::{Count, Function, FunctionRegistry, ValueAccumulator},
        variable_value::VariableValue,
    },
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    interface::{
        AccumulatorIndex, ResultIndex, ResultKey, ResultOwner, RESULT_INDEX_STATE_VERSION,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::{ContinuousQuery, QueryBuilder},
};

const PLAN_SLOT_QUERY: &str = "
MATCH (plan:Plan)-[:HAS_SLOT]->(slot:Slot)
OPTIONAL MATCH (member:Member)-[:FILLS]->(slot)
WITH plan, slot, count(member) AS memberCount
WITH plan,
  count(CASE WHEN memberCount = 1 THEN 1 ELSE null END) AS exactMemberCount
WHERE exactMemberCount <> plan.declaredCount
RETURN plan.id AS planId, exactMemberCount
";

const FINAL_AGGREGATE_QUERY: &str = "
MATCH (plan:Plan)-[:HAS_SLOT]->(slot:Slot)
OPTIONAL MATCH (member:Member)-[:FILLS]->(slot)
WITH plan, slot, count(member) AS memberCount
RETURN plan.id AS planId,
  count(CASE WHEN memberCount = 1 THEN 1 ELSE null END) AS exactMemberCount
";

const GLOBAL_AGGREGATE_QUERY: &str = "
MATCH (:Plan)-[:HAS_SLOT]->(slot:Slot)
OPTIONAL MATCH (member:Member)-[:FILLS]->(slot)
WITH slot, count(member) AS memberCount
RETURN count(CASE WHEN memberCount = 1 THEN 1 ELSE null END) AS exactMemberCount
";

const FILTERED_SLOT_QUERY: &str = "
MATCH (plan:Plan)-[:HAS_SLOT]->(slot:Slot)
OPTIONAL MATCH (member:Member)-[:FILLS]->(slot)
WITH plan, slot, count(member) AS memberCount
WHERE slot.enabled
RETURN plan.id AS planId,
  count(CASE WHEN memberCount = 1 THEN 1 ELSE null END) AS exactMemberCount
";

const GROUP_MIGRATION_QUERY: &str = "
MATCH (item:Item)
WITH item.id AS itemId, item.bucket AS bucket, count(item) AS innerCount
RETURN bucket, count(itemId) AS itemCount
";

const FILTERED_DEFAULT_QUERY: &str = "
MATCH (item:Item)
WITH item.bucket AS bucket, count(item) AS innerCount
WHERE innerCount = 0
RETURN count(bucket) AS bucketCount
";

const LATE_COMPLETION_QUERY: &str = "
MATCH (parent:Parent)-[:DECLARES]->(slot:Slot)
OPTIONAL MATCH (child:Child)-[:FILLS]->(slot)
OPTIONAL MATCH (result:Result)-[:RESULT_FOR]->(child)
OPTIONAL MATCH (evaluation:Evaluation)-[:EVALUATES]->(result)
WITH parent, slot, child, result, evaluation,
  count(parent) AS basePathCount,
  count(CASE WHEN child IS NOT NULL THEN 1 ELSE null END) AS structuralPathCount
WITH parent, slot, child,
  count(CASE WHEN basePathCount <> 1 THEN 1 ELSE null END) AS invalidPathCount,
  count(CASE WHEN structuralPathCount = 1 AND result IS NOT NULL
    THEN 1 ELSE null END) AS resultCount,
  count(CASE WHEN structuralPathCount = 1 AND evaluation IS NOT NULL
    THEN 1 ELSE null END) AS evaluationCount,
  count(CASE WHEN structuralPathCount = 1
    AND NOT child.isOpen
    AND evaluation.resultId = result.id
    THEN 1 ELSE null END) AS validCompletionCount
WITH parent, slot,
  count(CASE WHEN invalidPathCount = 0
    AND resultCount = 1
    AND evaluationCount = 1
    AND validCompletionCount = 1
    THEN 1 ELSE null END) AS completedSlotCount
WITH parent,
  count(CASE WHEN completedSlotCount = 1 THEN 1 ELSE null END) AS completedChildCount
WHERE completedChildCount = parent.declaredCount
RETURN parent.id AS parentId, completedChildCount
";

struct MaterializedQuery {
    query: ContinuousQuery,
    rows: HashMap<u64, QueryVariables>,
}

impl MaterializedQuery {
    async fn new(query_text: &str) -> Self {
        let element_index = Arc::new(InMemoryElementIndex::new());
        let result_index = Arc::new(InMemoryResultIndex::new());
        Self::with_indexes(query_text, element_index, result_index).await
    }

    async fn with_indexes(
        query_text: &str,
        element_index: Arc<InMemoryElementIndex>,
        result_index: Arc<InMemoryResultIndex>,
    ) -> Self {
        let functions = Arc::new(FunctionRegistry::new());
        functions.register_function("count", Function::Aggregating(Arc::new(Count {})));
        let parser = Arc::new(CypherParser::new(functions.clone()));
        let query = QueryBuilder::new(query_text, parser)
            .with_function_registry(functions)
            .with_element_index(element_index.clone())
            .with_archive_index(element_index)
            .with_result_index(result_index)
            .with_future_queue(Arc::new(InMemoryFutureQueue::new()))
            .build()
            .await;
        Self {
            query,
            rows: HashMap::new(),
        }
    }

    fn row_integer(row: &QueryVariables, field: &str) -> i64 {
        match row.get(field) {
            Some(VariableValue::Integer(value)) => value.as_i64().unwrap(),
            other => panic!("expected integer {field}, got {other:?}"),
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

    fn exact_member_count(&self) -> i64 {
        assert_eq!(self.rows.len(), 1);
        match self.rows.values().next().unwrap().get("exactMemberCount") {
            Some(VariableValue::Integer(value)) => value.as_i64().unwrap(),
            other => panic!("expected integer count, got {other:?}"),
        }
    }
}

fn insert_node(
    id: &str,
    label: &str,
    effective_from: u64,
    properties: serde_json::Value,
) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", id),
                labels: Arc::new([Arc::from(label)]),
                effective_from,
            },
            properties: ElementPropertyMap::from(properties),
        },
    }
}

fn insert_relation(
    id: &str,
    label: &str,
    effective_from: u64,
    from: &str,
    to: &str,
) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", id),
                labels: Arc::new([Arc::from(label)]),
                effective_from,
            },
            in_node: ElementReference::new("test", from),
            out_node: ElementReference::new("test", to),
            properties: ElementPropertyMap::from(json!({})),
        },
    }
}

fn delete(id: &str, effective_from: u64) -> SourceChange {
    SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("test", id),
            labels: Arc::new([]),
            effective_from,
        },
    }
}

async fn insert_plan_and_slots(subject: &mut MaterializedQuery) {
    subject
        .process(insert_node(
            "node-plan",
            "Plan",
            1,
            json!({"id": "plan", "declaredCount": 2}),
        ))
        .await;
    for (time, slot_id, relation_id) in [
        (2, "node-slot-a", "rel-plan-slot-a"),
        (4, "node-slot-b", "rel-plan-slot-b"),
    ] {
        subject
            .process(insert_node(slot_id, "Slot", time, json!({"id": slot_id})))
            .await;
        subject
            .process(insert_relation(
                relation_id,
                "HAS_SLOT",
                time + 1,
                "node-plan",
                slot_id,
            ))
            .await;
    }
}

async fn insert_member(
    subject: &mut MaterializedQuery,
    name: &str,
    slot: &str,
    effective_from: u64,
) -> Vec<QueryPartEvaluationContext> {
    let node_id = format!("node-member-{name}");
    subject
        .process(insert_node(
            &node_id,
            "Member",
            effective_from,
            json!({"id": name}),
        ))
        .await;
    subject
        .process(insert_relation(
            &format!("rel-member-{name}-{slot}"),
            "FILLS",
            effective_from + 1,
            &node_id,
            &format!("node-slot-{slot}"),
        ))
        .await
}

async fn insert_parent_and_child(subject: &mut MaterializedQuery) {
    for change in [
        insert_node(
            "node-parent",
            "Parent",
            1,
            json!({"id": "parent", "declaredCount": 1}),
        ),
        insert_node("node-slot", "Slot", 2, json!({"id": "slot"})),
        insert_relation("rel-parent-slot", "DECLARES", 3, "node-parent", "node-slot"),
        insert_node(
            "node-child",
            "Child",
            4,
            json!({"id": "child", "isOpen": true}),
        ),
        insert_relation("rel-child-slot", "FILLS", 5, "node-child", "node-slot"),
    ] {
        subject.process(change).await;
    }
}

async fn close_child(subject: &mut MaterializedQuery, effective_from: u64) {
    subject
        .process(SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node-child"),
                    labels: Arc::new([Arc::from("Child")]),
                    effective_from,
                },
                properties: ElementPropertyMap::from(json!({"id": "child", "isOpen": false})),
            },
        })
        .await;
}

async fn insert_completion_artifacts(subject: &mut MaterializedQuery, effective_from: u64) {
    for change in [
        insert_node(
            "node-result",
            "Result",
            effective_from,
            json!({"id": "result"}),
        ),
        insert_relation(
            "rel-result-child",
            "RESULT_FOR",
            effective_from + 1,
            "node-result",
            "node-child",
        ),
        insert_node(
            "node-evaluation",
            "Evaluation",
            effective_from + 2,
            json!({"id": "evaluation", "resultId": "result"}),
        ),
        insert_relation(
            "rel-evaluation-result",
            "EVALUATES",
            effective_from + 3,
            "node-evaluation",
            "node-result",
        ),
    ] {
        subject.process(change).await;
    }
}

#[tokio::test]
async fn closed_child_converges_when_result_and_evaluation_replay_later() {
    let mut late_artifacts = MaterializedQuery::new(LATE_COMPLETION_QUERY).await;
    insert_parent_and_child(&mut late_artifacts).await;
    close_child(&mut late_artifacts, 6).await;
    insert_completion_artifacts(&mut late_artifacts, 7).await;

    assert_eq!(late_artifacts.rows.len(), 1);
    assert_eq!(
        MaterializedQuery::row_integer(
            late_artifacts.rows.values().next().unwrap(),
            "completedChildCount",
        ),
        1
    );

    let mut close_last = MaterializedQuery::new(LATE_COMPLETION_QUERY).await;
    insert_parent_and_child(&mut close_last).await;
    insert_completion_artifacts(&mut close_last, 6).await;
    close_child(&mut close_last, 10).await;

    assert_eq!(close_last.rows, late_artifacts.rows);
}

#[tokio::test]
async fn nested_aggregate_tracks_zero_slots_through_full_lifecycle() {
    let mut subject = MaterializedQuery::new(PLAN_SLOT_QUERY).await;
    insert_plan_and_slots(&mut subject).await;

    assert_eq!(subject.exact_member_count(), 0);

    let first_member = insert_member(&mut subject, "a", "a", 6).await;
    assert!(matches!(
        first_member.as_slice(),
        [QueryPartEvaluationContext::Updating { .. }]
    ));
    assert_eq!(subject.exact_member_count(), 1);

    let second_member = insert_member(&mut subject, "b", "b", 8).await;
    assert!(matches!(
        second_member.as_slice(),
        [QueryPartEvaluationContext::Removing { .. }]
    ));
    assert!(subject.rows.is_empty());

    let remove_member = subject.process(delete("rel-member-b-b", 10)).await;
    assert!(matches!(
        remove_member.as_slice(),
        [QueryPartEvaluationContext::Adding { .. }]
    ));
    assert_eq!(subject.exact_member_count(), 1);

    subject
        .process(insert_relation(
            "rel-member-b-b",
            "FILLS",
            11,
            "node-member-b",
            "node-slot-b",
        ))
        .await;
    assert!(subject.rows.is_empty());

    subject.process(delete("rel-member-b-b", 12)).await;
    subject.process(delete("rel-member-a-a", 13)).await;
    assert_eq!(subject.exact_member_count(), 0);

    subject.process(delete("rel-plan-slot-b", 14)).await;
    assert_eq!(
        subject.exact_member_count(),
        0,
        "removing a non-last zero-valued inner group must retain the parent"
    );

    let last_slot = subject.process(delete("rel-plan-slot-a", 15)).await;
    assert!(matches!(
        last_slot.as_slice(),
        [QueryPartEvaluationContext::Removing { .. }]
    ));
    assert!(subject.rows.is_empty());

    let readd = subject
        .process(insert_relation(
            "rel-plan-slot-a",
            "HAS_SLOT",
            16,
            "node-plan",
            "node-slot-a",
        ))
        .await;
    assert!(matches!(
        readd.as_slice(),
        [QueryPartEvaluationContext::Adding { .. }]
    ));
    assert_eq!(subject.exact_member_count(), 0);
}

#[tokio::test]
async fn one_event_can_create_multiple_zero_valued_inner_groups() {
    let mut subject = MaterializedQuery::new(PLAN_SLOT_QUERY).await;
    for (time, slot_id, relation_id) in [
        (1, "node-slot-a", "rel-plan-slot-a"),
        (3, "node-slot-b", "rel-plan-slot-b"),
    ] {
        subject
            .process(insert_node(slot_id, "Slot", time, json!({"id": slot_id})))
            .await;
        subject
            .process(insert_relation(
                relation_id,
                "HAS_SLOT",
                time + 1,
                "node-plan",
                slot_id,
            ))
            .await;
    }

    let add = subject
        .process(insert_node(
            "node-plan",
            "Plan",
            5,
            json!({"id": "plan", "declaredCount": 2}),
        ))
        .await;

    assert!(matches!(
        add.as_slice(),
        [QueryPartEvaluationContext::Adding { .. }]
    ));
    assert_eq!(subject.exact_member_count(), 0);
}

#[tokio::test]
async fn final_and_global_nested_aggregates_materialize_zero_groups() {
    for query in [FINAL_AGGREGATE_QUERY, GLOBAL_AGGREGATE_QUERY] {
        let mut subject = MaterializedQuery::new(query).await;
        insert_plan_and_slots(&mut subject).await;

        assert_eq!(subject.exact_member_count(), 0);
        insert_member(&mut subject, "a", "a", 6).await;
        assert_eq!(subject.exact_member_count(), 1);
        subject.process(delete("rel-member-a-a", 8)).await;
        assert_eq!(subject.exact_member_count(), 0);
    }
}

#[tokio::test]
async fn filtering_inner_groups_updates_outer_cardinality() {
    let mut subject = MaterializedQuery::new(FILTERED_SLOT_QUERY).await;
    subject
        .process(insert_node(
            "node-plan",
            "Plan",
            1,
            json!({"id": "plan", "declaredCount": 2}),
        ))
        .await;
    for (time, slot_id, relation_id) in [
        (2, "node-slot-a", "rel-plan-slot-a"),
        (4, "node-slot-b", "rel-plan-slot-b"),
    ] {
        subject
            .process(insert_node(
                slot_id,
                "Slot",
                time,
                json!({"id": slot_id, "enabled": true}),
            ))
            .await;
        subject
            .process(insert_relation(
                relation_id,
                "HAS_SLOT",
                time + 1,
                "node-plan",
                slot_id,
            ))
            .await;
    }

    assert_eq!(subject.exact_member_count(), 0);

    let disable = SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "node-slot-b"),
                labels: Arc::new([Arc::from("Slot")]),
                effective_from: 6,
            },
            properties: ElementPropertyMap::from(json!({"id": "node-slot-b", "enabled": false})),
        },
    };
    subject.process(disable).await;
    assert_eq!(
        subject.exact_member_count(),
        0,
        "a filtered non-last contributor must not remove the outer group"
    );

    let disable_last = SourceChange::Update {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", "node-slot-a"),
                labels: Arc::new([Arc::from("Slot")]),
                effective_from: 7,
            },
            properties: ElementPropertyMap::from(json!({"id": "node-slot-a", "enabled": false})),
        },
    };
    let removal = subject.process(disable_last).await;
    assert!(matches!(
        removal.as_slice(),
        [QueryPartEvaluationContext::Aggregation {
            default_before: false,
            default_after: true,
            ..
        }]
    ));
}

#[tokio::test]
async fn filtered_default_transition_does_not_remove_an_absent_outer_group() {
    let mut subject = MaterializedQuery::new(FILTERED_DEFAULT_QUERY).await;
    let changes = subject
        .process(insert_node(
            "node-item",
            "Item",
            1,
            json!({"id": "item", "bucket": "a"}),
        ))
        .await;

    assert!(changes.is_empty());
    assert!(subject.rows.is_empty());
}

#[tokio::test]
async fn nested_aggregate_outer_group_migration_updates_both_groups() {
    let mut subject = MaterializedQuery::new(GROUP_MIGRATION_QUERY).await;
    for (time, id, bucket) in [(1, "one", "a"), (2, "two", "a"), (3, "three", "b")] {
        subject
            .process(insert_node(
                &format!("node-{id}"),
                "Item",
                time,
                json!({"id": id, "bucket": bucket}),
            ))
            .await;
    }
    assert_eq!(subject.rows.len(), 2);

    let migration = subject
        .process(SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node-one"),
                    labels: Arc::new([Arc::from("Item")]),
                    effective_from: 4,
                },
                properties: ElementPropertyMap::from(json!({"id": "one", "bucket": "b"})),
            },
        })
        .await;

    assert_eq!(migration.len(), 2);
    let by_bucket: HashMap<_, _> = subject
        .rows
        .values()
        .map(|row| {
            (
                row.get("bucket").unwrap().clone(),
                MaterializedQuery::row_integer(row, "itemCount"),
            )
        })
        .collect();
    assert_eq!(by_bucket.get(&VariableValue::from(json!("a"))), Some(&1));
    assert_eq!(by_bucket.get(&VariableValue::from(json!("b"))), Some(&2));

    let duplicate = subject
        .process(SourceChange::Update {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("test", "node-one"),
                    labels: Arc::new([Arc::from("Item")]),
                    effective_from: 5,
                },
                properties: ElementPropertyMap::from(json!({"id": "one", "bucket": "b"})),
            },
        })
        .await;
    assert!(duplicate.is_empty());
}

#[tokio::test]
async fn retained_cardinality_state_survives_query_restart() {
    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let mut subject = MaterializedQuery::with_indexes(
        PLAN_SLOT_QUERY,
        element_index.clone(),
        result_index.clone(),
    )
    .await;
    insert_plan_and_slots(&mut subject).await;
    assert_eq!(subject.exact_member_count(), 0);
    drop(subject);

    let mut restarted =
        MaterializedQuery::with_indexes(PLAN_SLOT_QUERY, element_index, result_index).await;
    let first_member = insert_member(&mut restarted, "a", "a", 6).await;
    assert!(matches!(
        first_member.as_slice(),
        [QueryPartEvaluationContext::Updating { .. }]
    ));
    assert_eq!(restarted.exact_member_count(), 1);
}

#[tokio::test]
async fn due_future_entrypoint_validates_result_index_state() {
    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let subject =
        MaterializedQuery::with_indexes(PLAN_SLOT_QUERY, element_index, result_index.clone()).await;

    assert!(subject.query.process_due_futures().await.unwrap().is_none());
    assert!(matches!(
        result_index
            .get(&ResultKey::InputHash(0), &ResultOwner::QueryState)
            .await
            .unwrap(),
        Some(ValueAccumulator::Signature(RESULT_INDEX_STATE_VERSION))
    ));

    let legacy_element_index = Arc::new(InMemoryElementIndex::new());
    let legacy_result_index = Arc::new(InMemoryResultIndex::new());
    legacy_result_index
        .set(
            ResultKey::InputHash(42),
            ResultOwner::Function(1),
            Some(ValueAccumulator::Count { value: 1 }),
        )
        .await
        .unwrap();
    let legacy =
        MaterializedQuery::with_indexes(PLAN_SLOT_QUERY, legacy_element_index, legacy_result_index)
            .await;

    let error = match legacy.query.process_due_futures().await {
        Err(error) => error,
        Ok(_) => panic!("markerless non-empty result index should be rejected"),
    };
    assert!(matches!(
        error,
        crate::evaluation::EvaluationError::IndexError(crate::interface::IndexError::Other(_))
    ));
}
