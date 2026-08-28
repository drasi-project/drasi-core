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

const AGGREGATE_QUERY: &str = "
MATCH
  (run:Run)-[:USES_PLAN]->
  (plan:Plan)-[:HAS_DEFINITION]->
  (parentDefinition:Definition)<-[:INSTANCE_OF]-
  (parent:Task)-[:IN_RUN]->(run)
MATCH
  (parentDefinition)-[:DECLARES_CHILD]->(childDefinition:Definition)
OPTIONAL MATCH
  (child:Task)-[:INSTANCE_OF]->(childDefinition)
OPTIONAL MATCH (child)-[taskFor:TASK_FOR]->(parent)
WITH run, plan, parent, parentDefinition, childDefinition,
  count(CASE
    WHEN child.runId = run.id
      AND child.definitionId = childDefinition.id
      AND taskFor IS NOT NULL
    THEN 1 ELSE null END) AS realizationCount
WHERE parent.runId = run.id
  AND parent.definitionId = parentDefinition.id
RETURN run.id AS run, parent.id AS parent,
  childDefinition.id AS childDefinition, realizationCount
";

const RAW_QUERY: &str = "
MATCH
  (run:Run)-[:USES_PLAN]->
  (plan:Plan)-[:HAS_DEFINITION]->
  (parentDefinition:Definition)<-[:INSTANCE_OF]-
  (parent:Task)-[:IN_RUN]->(run)
MATCH
  (parentDefinition)-[declares:DECLARES_CHILD]->(childDefinition:Definition)
OPTIONAL MATCH
  (child:Task)-[:INSTANCE_OF]->(childDefinition)
OPTIONAL MATCH (child)-[taskFor:TASK_FOR]->(parent)
WHERE parent.runId = run.id
  AND parent.definitionId = parentDefinition.id
  AND child.runId = run.id
  AND child.definitionId = childDefinition.id
  AND taskFor IS NOT NULL
RETURN run.id AS run, parent.id AS parent,
  childDefinition.id AS childDefinition,
  child.id AS child, declares.id AS declares, taskFor.id AS taskFor
";

const CYCLIC_OPTIONAL_QUERY: &str = "
MATCH
  (run:Run)<-[:IN_RUN]-(parent:Task)-[:INSTANCE_OF]->
  (parentDefinition:Definition)
MATCH
  (parentDefinition)-[:DECLARES_CHILD]->(childDefinition:Definition)
OPTIONAL MATCH
  (childDefinition)<-[:INSTANCE_OF]-(child:Task)-[:TASK_FOR]->(parent)
  -[:IN_RUN]->(run)<-[:IN_RUN]-(child)
RETURN run.id AS run, parent.id AS parent,
  childDefinition.id AS childDefinition, count(child) AS realizationCount
";

const COMMA_OPTIONAL_QUERY: &str = "
MATCH (parent:Parent)
OPTIONAL MATCH
  (parent)-[:HAS_CHILD]->(child:Child),
  (child)-[:HAS_DETAIL]->(detail:Detail)
RETURN parent.id AS parent, count(child) AS childCount,
  count(detail) AS detailCount
";

const INDEPENDENT_OPTIONAL_QUERY: &str = "
MATCH (parent:Parent)
OPTIONAL MATCH (parent)-[:HAS_CHILD]->(child:Child)
OPTIONAL MATCH (parent)-[:HAS_NOTE]->(memo:Note)
RETURN parent.id AS parent, count(child) AS childCount,
  count(memo) AS noteCount
";

const REUSED_BINDINGS_OPTIONAL_QUERY: &str = "
MATCH (a:Point)-[link:LINK]->(b:Point)
OPTIONAL MATCH (a)<-[link]-(b)
RETURN a.id AS a, b.id AS b
";

const TWO_REUSED_BINDINGS_OPTIONAL_QUERY: &str = "
MATCH (a:Point)-[link:LINK]->(b:Point)
OPTIONAL MATCH (a)<-[link]-(b)
OPTIONAL MATCH (a)-[link]->(a)
RETURN a.id AS a, b.id AS b
";

const DEFERRED_SEGMENT_OPTIONAL_QUERY: &str = "
MATCH (a:Point)-[:BASE]->(b:Point)
OPTIONAL MATCH (a)-[link:LINK]->(x:Point)<-[link]-(b)
RETURN a.id AS a, b.id AS b, link.id AS link, x.id AS x
";

const MULTI_AFFINITY_OPTIONAL_QUERY: &str = "
MATCH (parent:Parent)
OPTIONAL MATCH (parent)-[:LINK]->(left:Child)
OPTIONAL MATCH (parent)-[:LINK]->(right:Child)
RETURN parent.id AS parent, count(left) AS leftCount,
  count(right) AS rightCount
";

const MULTI_AFFINITY_RAW_QUERY: &str = "
MATCH (parent:Parent)
OPTIONAL MATCH (parent)-[:LINK]->(left:Child)
OPTIONAL MATCH (parent)-[:LINK]->(right:Child)
RETURN parent.id AS parent, left.id AS left, right.id AS right
";

const OPTIONAL_REPARENT_QUERY: &str = "
MATCH (parent:Parent)
OPTIONAL MATCH (child:Child)-[:TASK_FOR]->(parent)
RETURN parent.id AS parent, count(child) AS childCount
";

const OPTIONAL_RELATION_LABEL_MIGRATION_QUERY: &str = "
MATCH (parent:Parent)
OPTIONAL MATCH (parent)-[:LEFT]->(left:Child)
OPTIONAL MATCH (parent)-[:RIGHT]->(right:Child)
RETURN parent.id AS parent, count(left) AS leftCount, count(right) AS rightCount
";

const FILTERED_OPTIONAL_FANOUT_QUERY: &str = "
MATCH (parent:Parent)
OPTIONAL MATCH (parent)-[:LEFT]->(left:Child)
OPTIONAL MATCH (parent)-[:RIGHT]->(right:Child)
WHERE right.enabled = true
RETURN parent.id AS parent, count(left) AS leftCount, count(right) AS rightCount
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

fn node(id: &str, label: &str, effective_from: u64, properties: serde_json::Value) -> SourceChange {
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

fn relation(id: &str, label: &str, effective_from: u64, from: &str, to: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: ElementMetadata {
                reference: ElementReference::new("test", id),
                labels: Arc::new([Arc::from(label)]),
                effective_from,
            },
            in_node: ElementReference::new("test", from),
            out_node: ElementReference::new("test", to),
            properties: ElementPropertyMap::from(json!({"id": id})),
        },
    }
}

fn update_relation(
    id: &str,
    label: &str,
    effective_from: u64,
    from: &str,
    to: &str,
) -> SourceChange {
    let SourceChange::Insert { element } = relation(id, label, effective_from, from, to) else {
        unreachable!()
    };
    SourceChange::Update { element }
}

fn update_node(
    id: &str,
    label: &str,
    effective_from: u64,
    properties: serde_json::Value,
) -> SourceChange {
    let SourceChange::Insert { element } = node(id, label, effective_from, properties) else {
        unreachable!()
    };
    SourceChange::Update { element }
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

fn definition_events() -> Vec<SourceChange> {
    vec![
        node("plan", "Plan", 1, json!({"id": "plan"})),
        node(
            "root-definition",
            "Definition",
            2,
            json!({"id": "root-definition"}),
        ),
        relation(
            "plan-root-definition",
            "HAS_DEFINITION",
            3,
            "plan",
            "root-definition",
        ),
        node(
            "child-definition-a",
            "Definition",
            4,
            json!({"id": "child-definition-a"}),
        ),
        relation(
            "plan-child-definition-a",
            "HAS_DEFINITION",
            5,
            "plan",
            "child-definition-a",
        ),
        relation(
            "root-declares-a",
            "DECLARES_CHILD",
            6,
            "root-definition",
            "child-definition-a",
        ),
        node(
            "child-definition-b",
            "Definition",
            7,
            json!({"id": "child-definition-b"}),
        ),
        relation(
            "plan-child-definition-b",
            "HAS_DEFINITION",
            8,
            "plan",
            "child-definition-b",
        ),
        relation(
            "root-declares-b",
            "DECLARES_CHILD",
            9,
            "root-definition",
            "child-definition-b",
        ),
    ]
}

fn run_events(prefix: &str, start_time: u64) -> Vec<SourceChange> {
    let run = format!("{prefix}-run");
    let parent = format!("{prefix}-parent");
    let child = format!("{prefix}-child");
    vec![
        node(&run, "Run", start_time, json!({"id": run})),
        relation(
            &format!("{prefix}-run-plan"),
            "USES_PLAN",
            start_time + 1,
            &run,
            "plan",
        ),
        node(
            &parent,
            "Task",
            start_time + 2,
            json!({
                "id": parent,
                "runId": run,
                "definitionId": "root-definition"
            }),
        ),
        relation(
            &format!("{prefix}-parent-definition"),
            "INSTANCE_OF",
            start_time + 3,
            &parent,
            "root-definition",
        ),
        relation(
            &format!("{prefix}-parent-run"),
            "IN_RUN",
            start_time + 4,
            &parent,
            &run,
        ),
        node(
            &child,
            "Task",
            start_time + 5,
            json!({
                "id": child,
                "runId": run,
                "definitionId": "child-definition-a"
            }),
        ),
        relation(
            &format!("{prefix}-child-definition"),
            "INSTANCE_OF",
            start_time + 6,
            &child,
            "child-definition-a",
        ),
        relation(
            &format!("{prefix}-child-run"),
            "IN_RUN",
            start_time + 7,
            &child,
            &run,
        ),
    ]
}

fn task_for(prefix: &str, parent_suffix: &str, effective_from: u64) -> SourceChange {
    relation(
        &format!("{prefix}-task-for"),
        "TASK_FOR",
        effective_from,
        &format!("{prefix}-child"),
        &format!("{prefix}-{parent_suffix}"),
    )
}

async fn process_all(subject: &mut MaterializedQuery, changes: Vec<SourceChange>) {
    for change in changes {
        subject.process(change).await;
    }
}

fn string_value<'a>(row: &'a QueryVariables, key: &str) -> &'a str {
    match row.get(key) {
        Some(VariableValue::String(value)) => value,
        other => panic!("expected string {key}, got {other:?}"),
    }
}

fn integer_value(row: &QueryVariables, key: &str) -> i64 {
    match row.get(key) {
        Some(VariableValue::Integer(value)) => value.as_i64().unwrap(),
        other => panic!("expected integer {key}, got {other:?}"),
    }
}

fn only_count(subject: &MaterializedQuery, key: &str) -> i64 {
    assert_eq!(subject.rows.len(), 1);
    integer_value(subject.rows.values().next().unwrap(), key)
}

fn parent_child_count(subject: &MaterializedQuery, parent: &str) -> i64 {
    let row = subject
        .rows
        .values()
        .find(|row| row.get("parent") == Some(&VariableValue::from(parent)))
        .unwrap_or_else(|| panic!("missing parent {parent} row"));
    integer_value(row, "childCount")
}

fn assert_parent_transition(
    changes: &[QueryPartEvaluationContext],
    parent: &str,
    before_count: i64,
    after_count: i64,
) {
    assert!(changes.iter().any(|change| {
        matches!(
            change,
            QueryPartEvaluationContext::Aggregation {
                before: Some(before),
                after,
                ..
            } if before.get("parent") == Some(&VariableValue::from(parent))
                && after.get("parent") == Some(&VariableValue::from(parent))
                && integer_value(before, "childCount") == before_count
                && integer_value(after, "childCount") == after_count
        )
    }));
}

fn assert_count_transition(
    changes: &[QueryPartEvaluationContext],
    field: &str,
    before_count: i64,
    after_count: i64,
) {
    assert!(changes.iter().any(|change| {
        matches!(
            change,
            QueryPartEvaluationContext::Aggregation {
                before: Some(before),
                after,
                ..
            } if integer_value(before, field) == before_count
                && integer_value(after, field) == after_count
        )
    }));
}

fn realization_count(
    subject: &MaterializedQuery,
    run: &str,
    parent: &str,
    child_definition: &str,
) -> i64 {
    let row = subject
        .rows
        .values()
        .find(|row| {
            row.get("run") == Some(&VariableValue::from(run))
                && row.get("parent") == Some(&VariableValue::from(parent))
                && row.get("childDefinition") == Some(&VariableValue::from(child_definition))
        })
        .unwrap_or_else(|| panic!("missing {run}/{parent}/{child_definition} row"));
    integer_value(row, "realizationCount")
}

fn assert_valid_realization_outputs(
    subject: &MaterializedQuery,
    changes: &[QueryPartEvaluationContext],
) {
    assert!(subject.rows.len() <= 2);
    for row in subject.rows.values() {
        assert!(matches!(integer_value(row, "realizationCount"), 0 | 1));
    }
    for change in changes {
        let row = match change {
            QueryPartEvaluationContext::Adding { after, .. }
            | QueryPartEvaluationContext::Updating { after, .. }
            | QueryPartEvaluationContext::Aggregation { after, .. } => Some(after),
            QueryPartEvaluationContext::Removing { before, .. } => Some(before),
            QueryPartEvaluationContext::Noop => None,
        };
        if let Some(row) = row {
            assert!(matches!(integer_value(row, "realizationCount"), 0 | 1));
        }
    }
}

async fn build_single_run(query: &str) -> MaterializedQuery {
    let mut subject = MaterializedQuery::new(query).await;
    process_all(&mut subject, definition_events()).await;
    process_all(&mut subject, run_events("one", 10)).await;
    subject
}

async fn build_independent_optional_fanout() -> MaterializedQuery {
    let mut subject = MaterializedQuery::new(OPTIONAL_RELATION_LABEL_MIGRATION_QUERY).await;
    process_all(
        &mut subject,
        vec![
            node("parent", "Parent", 1, json!({"id": "parent"})),
            node("left-one", "Child", 2, json!({"id": "left-one"})),
            node("right-one", "Child", 3, json!({"id": "right-one"})),
            node("right-two", "Child", 4, json!({"id": "right-two"})),
            relation("left-one-rel", "LEFT", 5, "parent", "left-one"),
            relation("right-one-rel", "RIGHT", 6, "parent", "right-one"),
            relation("right-two-rel", "RIGHT", 7, "parent", "right-two"),
        ],
    )
    .await;
    subject
}

#[tokio::test]
async fn later_optional_anchor_cannot_complete_an_earlier_defaulted_clause() {
    for _ in 0..20 {
        let events = run_events("one", 10);
        let mut aggregate = MaterializedQuery::new(AGGREGATE_QUERY).await;
        process_all(&mut aggregate, definition_events()).await;
        process_all(&mut aggregate, events[..6].to_vec()).await;
        aggregate.process(events[7].clone()).await;
        aggregate.process(task_for("one", "parent", 18)).await;
        aggregate.process(events[6].clone()).await;
        assert_eq!(
            realization_count(&aggregate, "one-run", "one-parent", "child-definition-a"),
            1
        );

        let mut raw = MaterializedQuery::new(RAW_QUERY).await;
        process_all(&mut raw, definition_events()).await;
        process_all(&mut raw, events[..6].to_vec()).await;
        raw.process(events[7].clone()).await;
        raw.process(task_for("one", "parent", 18)).await;
        raw.process(events[6].clone()).await;
        assert_eq!(raw.rows.len(), 1);
    }
}

#[tokio::test]
async fn every_relationship_order_converges_to_one_raw_solution() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        for _ in 0..20 {
            let events = run_events("one", 10);
            let relationships = [
                events[6].clone(),
                events[7].clone(),
                task_for("one", "parent", 18),
            ];
            let mut subject = MaterializedQuery::new(RAW_QUERY).await;
            process_all(&mut subject, definition_events()).await;
            process_all(&mut subject, events[..6].to_vec()).await;
            for index in order {
                subject.process(relationships[index].clone()).await;
            }
            assert_eq!(subject.rows.len(), 1, "relationship order {order:?}");
        }
    }
}

#[tokio::test]
async fn cyclic_optional_keeps_defaults_when_local_candidates_fail_globally() {
    let mut subject = MaterializedQuery::new(CYCLIC_OPTIONAL_QUERY).await;
    process_all(&mut subject, definition_events()).await;
    let events = run_events("one", 10);
    process_all(&mut subject, events[..5].to_vec()).await;

    assert_eq!(subject.rows.len(), 2);
    assert_eq!(
        realization_count(&subject, "one-run", "one-parent", "child-definition-a"),
        0
    );
    assert_eq!(
        realization_count(&subject, "one-run", "one-parent", "child-definition-b"),
        0
    );
}

#[tokio::test]
async fn comma_separated_optional_patterns_succeed_or_default_together() {
    let mut subject = MaterializedQuery::new(COMMA_OPTIONAL_QUERY).await;
    subject
        .process(node("parent", "Parent", 1, json!({"id": "parent"})))
        .await;
    assert_eq!(only_count(&subject, "childCount"), 0);

    subject
        .process(node("child", "Child", 2, json!({"id": "child"})))
        .await;
    subject
        .process(relation("parent-child", "HAS_CHILD", 3, "parent", "child"))
        .await;
    assert_eq!(only_count(&subject, "childCount"), 0);
    assert_eq!(only_count(&subject, "detailCount"), 0);

    subject
        .process(node("detail", "Detail", 4, json!({"id": "detail"})))
        .await;
    subject
        .process(relation("child-detail", "HAS_DETAIL", 5, "child", "detail"))
        .await;
    assert_eq!(only_count(&subject, "childCount"), 1);
    assert_eq!(only_count(&subject, "detailCount"), 1);
}

#[tokio::test]
async fn independent_later_optional_clause_remains_eligible() {
    let mut subject = MaterializedQuery::new(INDEPENDENT_OPTIONAL_QUERY).await;
    subject
        .process(node("parent", "Parent", 1, json!({"id": "parent"})))
        .await;
    subject
        .process(node("note", "Note", 2, json!({"id": "note"})))
        .await;
    subject
        .process(relation("parent-note", "HAS_NOTE", 3, "parent", "note"))
        .await;

    assert_eq!(only_count(&subject, "childCount"), 0);
    assert_eq!(only_count(&subject, "noteCount"), 1);
}

#[tokio::test]
async fn optional_clause_with_only_reused_bindings_can_default() {
    let mut subject = MaterializedQuery::new(REUSED_BINDINGS_OPTIONAL_QUERY).await;
    process_all(
        &mut subject,
        vec![
            node("a", "Point", 1, json!({"id": "a"})),
            node("b", "Point", 2, json!({"id": "b"})),
            relation("a-b", "LINK", 3, "a", "b"),
        ],
    )
    .await;

    assert_eq!(subject.rows.len(), 1);
    let row = subject.rows.values().next().unwrap();
    assert_eq!(row.get("a"), Some(&VariableValue::from("a")));
    assert_eq!(row.get("b"), Some(&VariableValue::from("b")));
}

#[tokio::test]
async fn every_failed_reused_binding_clause_defaults() {
    let mut subject = MaterializedQuery::new(TWO_REUSED_BINDINGS_OPTIONAL_QUERY).await;
    process_all(
        &mut subject,
        vec![
            node("a", "Point", 1, json!({"id": "a"})),
            node("b", "Point", 2, json!({"id": "b"})),
            relation("a-b", "LINK", 3, "a", "b"),
        ],
    )
    .await;

    assert_eq!(subject.rows.len(), 1);
    let row = subject.rows.values().next().unwrap();
    assert_eq!(row.get("a"), Some(&VariableValue::from("a")));
    assert_eq!(row.get("b"), Some(&VariableValue::from("b")));
}

#[tokio::test]
async fn completed_solution_rechecks_deferred_optional_segments() {
    let mut subject = MaterializedQuery::new(DEFERRED_SEGMENT_OPTIONAL_QUERY).await;
    process_all(
        &mut subject,
        vec![
            node("a", "Point", 1, json!({"id": "a"})),
            node("x", "Point", 2, json!({"id": "x"})),
            relation("link", "LINK", 3, "a", "x"),
            relation("base", "BASE", 4, "a", "b"),
            node("b", "Point", 5, json!({"id": "b"})),
        ],
    )
    .await;

    assert_eq!(subject.rows.len(), 1);
    let row = subject.rows.values().next().unwrap();
    assert_eq!(row.get("a"), Some(&VariableValue::from("a")));
    assert_eq!(row.get("b"), Some(&VariableValue::from("b")));
    assert_eq!(row.get("link"), Some(&VariableValue::Null));
    assert_eq!(row.get("x"), Some(&VariableValue::Null));
}

#[tokio::test]
async fn duplicate_affinity_merges_all_affected_optional_clauses() {
    let mut subject = MaterializedQuery::new(MULTI_AFFINITY_OPTIONAL_QUERY).await;
    subject
        .process(node("parent", "Parent", 1, json!({"id": "parent"})))
        .await;
    subject
        .process(node("child", "Child", 2, json!({"id": "child"})))
        .await;
    let changes = subject
        .process(relation("parent-child", "LINK", 3, "parent", "child"))
        .await;

    assert_eq!(only_count(&subject, "leftCount"), 1);
    assert_eq!(only_count(&subject, "rightCount"), 1);
    assert!(matches!(
        changes.as_slice(),
        [QueryPartEvaluationContext::Aggregation {
            before: Some(before),
            after,
            ..
        }] if integer_value(before, "leftCount") == 0
            && integer_value(before, "rightCount") == 0
            && integer_value(after, "leftCount") == 1
            && integer_value(after, "rightCount") == 1
    ));
}

#[tokio::test]
async fn multi_affinity_delete_does_not_emit_a_joint_default() {
    let mut subject = MaterializedQuery::new(MULTI_AFFINITY_RAW_QUERY).await;
    process_all(
        &mut subject,
        vec![
            node("parent", "Parent", 1, json!({"id": "parent"})),
            node("first", "Child", 2, json!({"id": "first"})),
            node("second", "Child", 3, json!({"id": "second"})),
            relation("first-link", "LINK", 4, "parent", "first"),
            relation("second-link", "LINK", 5, "parent", "second"),
        ],
    )
    .await;
    assert_eq!(subject.rows.len(), 4);

    subject.process(delete("second-link", 6)).await;

    assert_eq!(subject.rows.len(), 1);
    let row = subject.rows.values().next().unwrap();
    assert_eq!(row.get("left"), Some(&VariableValue::from("first")));
    assert_eq!(row.get("right"), Some(&VariableValue::from("first")));
}

#[tokio::test]
async fn relation_endpoint_update_restores_source_default_and_retracts_destination_default() {
    let mut subject = MaterializedQuery::new(OPTIONAL_REPARENT_QUERY).await;
    process_all(
        &mut subject,
        vec![
            node("parent-a", "Parent", 1, json!({"id": "a"})),
            node("parent-b", "Parent", 2, json!({"id": "b"})),
            node("child", "Child", 3, json!({"id": "child"})),
            relation("task-for", "TASK_FOR", 4, "child", "parent-a"),
        ],
    )
    .await;
    assert_eq!(parent_child_count(&subject, "a"), 1);
    assert_eq!(parent_child_count(&subject, "b"), 0);

    let away = subject
        .process(update_relation(
            "task-for", "TASK_FOR", 5, "child", "parent-b",
        ))
        .await;
    assert_eq!(away.len(), 2);
    assert_parent_transition(&away, "a", 1, 0);
    assert_parent_transition(&away, "b", 0, 1);
    assert_eq!(parent_child_count(&subject, "a"), 0);
    assert_eq!(parent_child_count(&subject, "b"), 1);

    let back = subject
        .process(update_relation(
            "task-for", "TASK_FOR", 6, "child", "parent-a",
        ))
        .await;
    assert_eq!(back.len(), 2);
    assert_parent_transition(&back, "a", 0, 1);
    assert_parent_transition(&back, "b", 1, 0);
    assert_eq!(parent_child_count(&subject, "a"), 1);
    assert_eq!(parent_child_count(&subject, "b"), 0);
}

#[tokio::test]
async fn endpoint_update_converges_with_delete_then_insert() {
    async fn build() -> MaterializedQuery {
        let mut subject = MaterializedQuery::new(OPTIONAL_REPARENT_QUERY).await;
        process_all(
            &mut subject,
            vec![
                node("parent-a", "Parent", 1, json!({"id": "a"})),
                node("parent-b", "Parent", 2, json!({"id": "b"})),
                node("child", "Child", 3, json!({"id": "child"})),
                relation("task-for", "TASK_FOR", 4, "child", "parent-a"),
            ],
        )
        .await;
        subject
    }

    let mut updated = build().await;
    updated
        .process(update_relation(
            "task-for", "TASK_FOR", 5, "child", "parent-b",
        ))
        .await;

    let mut replaced = build().await;
    replaced.process(delete("task-for", 5)).await;
    replaced
        .process(relation("task-for", "TASK_FOR", 6, "child", "parent-b"))
        .await;

    assert_eq!(updated.rows, replaced.rows);
    assert_eq!(parent_child_count(&updated, "a"), 0);
    assert_eq!(parent_child_count(&updated, "b"), 1);
}

#[tokio::test]
async fn endpoint_update_restores_default_only_after_last_source_match() {
    let mut subject = MaterializedQuery::new(OPTIONAL_REPARENT_QUERY).await;
    process_all(
        &mut subject,
        vec![
            node("parent-a", "Parent", 1, json!({"id": "a"})),
            node("parent-b", "Parent", 2, json!({"id": "b"})),
            node("child-one", "Child", 3, json!({"id": "child-one"})),
            node("child-two", "Child", 4, json!({"id": "child-two"})),
            relation("task-for-one", "TASK_FOR", 5, "child-one", "parent-a"),
            relation("task-for-two", "TASK_FOR", 6, "child-two", "parent-a"),
        ],
    )
    .await;
    assert_eq!(parent_child_count(&subject, "a"), 2);
    assert_eq!(parent_child_count(&subject, "b"), 0);

    let non_last = subject
        .process(update_relation(
            "task-for-one",
            "TASK_FOR",
            7,
            "child-one",
            "parent-b",
        ))
        .await;
    assert_parent_transition(&non_last, "a", 2, 1);
    assert_parent_transition(&non_last, "b", 0, 1);
    assert_eq!(parent_child_count(&subject, "a"), 1);
    assert_eq!(parent_child_count(&subject, "b"), 1);

    let last = subject
        .process(update_relation(
            "task-for-two",
            "TASK_FOR",
            8,
            "child-two",
            "parent-b",
        ))
        .await;
    assert_parent_transition(&last, "a", 1, 0);
    assert_parent_transition(&last, "b", 1, 2);
    assert_eq!(parent_child_count(&subject, "a"), 0);
    assert_eq!(parent_child_count(&subject, "b"), 2);
}

#[tokio::test]
async fn relationship_and_node_match_updates_restore_optional_defaults() {
    let mut subject = MaterializedQuery::new(OPTIONAL_REPARENT_QUERY).await;
    process_all(
        &mut subject,
        vec![
            node("parent", "Parent", 1, json!({"id": "parent"})),
            node("child", "Child", 2, json!({"id": "child"})),
            relation("task-for", "TASK_FOR", 3, "child", "parent"),
        ],
    )
    .await;
    assert_eq!(parent_child_count(&subject, "parent"), 1);

    let relation_away = subject
        .process(update_relation("task-for", "IGNORED", 4, "child", "parent"))
        .await;
    assert_parent_transition(&relation_away, "parent", 1, 0);

    let relation_back = subject
        .process(update_relation(
            "task-for", "TASK_FOR", 5, "child", "parent",
        ))
        .await;
    assert_parent_transition(&relation_back, "parent", 0, 1);

    let node_away = subject
        .process(update_node("child", "Ignored", 6, json!({"id": "child"})))
        .await;
    assert_parent_transition(&node_away, "parent", 1, 0);

    let node_back = subject
        .process(update_node("child", "Child", 7, json!({"id": "child"})))
        .await;
    assert_parent_transition(&node_back, "parent", 0, 1);
    assert_eq!(parent_child_count(&subject, "parent"), 1);
}

#[tokio::test]
async fn update_between_independent_optional_clauses_preserves_raw_fanout() {
    let mut updated = build_independent_optional_fanout().await;
    assert_eq!(only_count(&updated, "leftCount"), 2);
    assert_eq!(only_count(&updated, "rightCount"), 2);

    let changes = updated
        .process(update_relation(
            "left-one-rel",
            "RIGHT",
            8,
            "parent",
            "left-one",
        ))
        .await;
    assert_eq!(changes.len(), 1);
    assert_eq!(only_count(&updated, "leftCount"), 0);
    assert_eq!(only_count(&updated, "rightCount"), 3);

    let mut replaced = build_independent_optional_fanout().await;
    let deleted = replaced.process(delete("left-one-rel", 8)).await;
    assert_eq!(deleted.len(), 1);
    assert_eq!(only_count(&replaced, "leftCount"), 0);
    assert_eq!(only_count(&replaced, "rightCount"), 2);
    replaced
        .process(relation("left-one-rel", "RIGHT", 9, "parent", "left-one"))
        .await;

    assert_eq!(updated.rows, replaced.rows);

    updated.process(delete("parent", 10)).await;
    updated
        .process(node("parent", "Parent", 11, json!({"id": "parent"})))
        .await;
    assert_eq!(only_count(&updated, "leftCount"), 0);
    assert_eq!(only_count(&updated, "rightCount"), 3);
}

#[tokio::test]
async fn optional_match_cardinality_survives_query_restart() {
    let element_index = Arc::new(InMemoryElementIndex::new());
    let result_index = Arc::new(InMemoryResultIndex::new());
    let mut subject = MaterializedQuery::with_indexes(
        OPTIONAL_RELATION_LABEL_MIGRATION_QUERY,
        element_index.clone(),
        result_index.clone(),
    )
    .await;
    process_all(
        &mut subject,
        vec![
            node("parent", "Parent", 1, json!({"id": "parent"})),
            node("left", "Child", 2, json!({"id": "left"})),
            node("right-one", "Child", 3, json!({"id": "right-one"})),
            node("right-two", "Child", 4, json!({"id": "right-two"})),
            relation("left-rel", "LEFT", 5, "parent", "left"),
            relation("right-one-rel", "RIGHT", 6, "parent", "right-one"),
            relation("right-two-rel", "RIGHT", 7, "parent", "right-two"),
        ],
    )
    .await;
    assert_eq!(only_count(&subject, "leftCount"), 2);
    drop(subject);

    let mut restarted = MaterializedQuery::with_indexes(
        OPTIONAL_RELATION_LABEL_MIGRATION_QUERY,
        element_index,
        result_index,
    )
    .await;
    restarted
        .process(node(
            "right-three",
            "Child",
            8,
            json!({"id": "right-three"}),
        ))
        .await;
    let added = restarted
        .process(relation(
            "right-three-rel",
            "RIGHT",
            9,
            "parent",
            "right-three",
        ))
        .await;
    assert_count_transition(&added, "rightCount", 2, 3);
    assert_eq!(only_count(&restarted, "leftCount"), 3);
    assert_eq!(only_count(&restarted, "rightCount"), 3);

    let removed = restarted.process(delete("right-three-rel", 10)).await;
    assert_count_transition(&removed, "rightCount", 3, 2);
    assert_eq!(only_count(&restarted, "leftCount"), 2);
    assert_eq!(only_count(&restarted, "rightCount"), 2);
}

#[tokio::test]
async fn filtered_optional_fanout_tracks_raw_and_projected_cardinality() {
    let mut subject = MaterializedQuery::new(FILTERED_OPTIONAL_FANOUT_QUERY).await;
    process_all(
        &mut subject,
        vec![
            node("parent", "Parent", 1, json!({"id": "parent"})),
            node("left", "Child", 2, json!({"id": "left"})),
            node(
                "right-one",
                "Child",
                3,
                json!({"id": "right-one", "enabled": true}),
            ),
            node(
                "right-two",
                "Child",
                4,
                json!({"id": "right-two", "enabled": false}),
            ),
            relation("left-rel", "LEFT", 5, "parent", "left"),
            relation("right-one-rel", "RIGHT", 6, "parent", "right-one"),
            relation("right-two-rel", "RIGHT", 7, "parent", "right-two"),
        ],
    )
    .await;
    assert_eq!(only_count(&subject, "leftCount"), 1);
    assert_eq!(only_count(&subject, "rightCount"), 1);

    let enabled = subject
        .process(update_node(
            "right-two",
            "Child",
            8,
            json!({"id": "right-two", "enabled": true}),
        ))
        .await;
    assert!(matches!(
        enabled.as_slice(),
        [QueryPartEvaluationContext::Aggregation { after, .. }]
            if integer_value(after, "leftCount") == 2
                && integer_value(after, "rightCount") == 2
    ));
    assert_eq!(only_count(&subject, "leftCount"), 2);
    assert_eq!(only_count(&subject, "rightCount"), 2);

    subject.process(delete("left-rel", 9)).await;
    assert_eq!(only_count(&subject, "leftCount"), 0);
    assert_eq!(only_count(&subject, "rightCount"), 2);
}

#[tokio::test]
async fn raw_solution_cardinality_excludes_cross_paired_relationships() {
    for _ in 0..32 {
        let mut subject = build_single_run(RAW_QUERY).await;
        let changes = subject.process(task_for("one", "parent", 18)).await;

        assert_eq!(changes.len(), 1);
        assert_eq!(subject.rows.len(), 1);
        let row = subject.rows.values().next().unwrap();
        assert_eq!(string_value(row, "run"), "one-run");
        assert_eq!(string_value(row, "parent"), "one-parent");
        assert_eq!(string_value(row, "childDefinition"), "child-definition-a");
        assert_eq!(string_value(row, "child"), "one-child");
        assert_eq!(string_value(row, "declares"), "root-declares-a");
        assert_eq!(string_value(row, "taskFor"), "one-task-for");
    }
}

#[tokio::test]
async fn solver_preserves_direction_and_self_loop_constraints() {
    let events = vec![
        node("a", "Point", 1, json!({"id": "a"})),
        node("b", "Point", 2, json!({"id": "b"})),
        relation("edge", "LINK", 3, "a", "b"),
        relation("loop", "LINK", 4, "a", "a"),
    ];

    let mut incoming = MaterializedQuery::new(
        "MATCH (end:Point)<-[link:LINK]-(start:Point)
         WHERE link.id = 'edge'
         RETURN start.id AS start, link.id AS link, end.id AS end",
    )
    .await;
    process_all(&mut incoming, events.clone()).await;
    assert_eq!(incoming.rows.len(), 1);
    let row = incoming.rows.values().next().unwrap();
    assert_eq!(string_value(row, "start"), "a");
    assert_eq!(string_value(row, "link"), "edge");
    assert_eq!(string_value(row, "end"), "b");

    let mut either = MaterializedQuery::new(
        "MATCH (left:Point)-[link:LINK]-(right:Point)
         WHERE link.id = 'edge'
         RETURN left.id AS left, link.id AS link, right.id AS right",
    )
    .await;
    process_all(&mut either, events.clone()).await;
    assert_eq!(either.rows.len(), 2);
    let orientations = either
        .rows
        .values()
        .map(|row| (string_value(row, "left"), string_value(row, "right")))
        .collect::<Vec<_>>();
    assert!(orientations.contains(&("a", "b")));
    assert!(orientations.contains(&("b", "a")));

    let mut self_loop = MaterializedQuery::new(
        "MATCH (point:Point)-[link:LINK]->(point)
         RETURN point.id AS point, link.id AS link",
    )
    .await;
    process_all(&mut self_loop, events.clone()).await;
    assert_eq!(self_loop.rows.len(), 1);
    let row = self_loop.rows.values().next().unwrap();
    assert_eq!(string_value(row, "point"), "a");
    assert_eq!(string_value(row, "link"), "loop");

    let mut reused_relation = MaterializedQuery::new(
        "MATCH (start:Point)-[link:LINK]->(end:Point)
         MATCH (start)-[link]->(other:Point)
         WHERE link.id = 'edge'
         RETURN start.id AS start, link.id AS link,
           end.id AS end, other.id AS other",
    )
    .await;
    process_all(&mut reused_relation, events).await;
    assert_eq!(reused_relation.rows.len(), 1);
    let row = reused_relation.rows.values().next().unwrap();
    assert_eq!(string_value(row, "start"), "a");
    assert_eq!(string_value(row, "link"), "edge");
    assert_eq!(string_value(row, "end"), "b");
    assert_eq!(string_value(row, "other"), "b");
}

#[tokio::test]
async fn structural_child_aggregate_tracks_task_for_lifecycle() {
    let mut subject = build_single_run(AGGREGATE_QUERY).await;
    assert_eq!(subject.rows.len(), 2);
    assert_eq!(
        realization_count(&subject, "one-run", "one-parent", "child-definition-a"),
        0
    );
    assert_eq!(
        realization_count(&subject, "one-run", "one-parent", "child-definition-b"),
        0
    );

    let add = subject.process(task_for("one", "parent", 18)).await;
    assert_eq!(add.len(), 1);
    assert_eq!(
        realization_count(&subject, "one-run", "one-parent", "child-definition-a"),
        1
    );

    let remove = subject.process(delete("one-task-for", 19)).await;
    assert_eq!(remove.len(), 1);
    assert_eq!(
        realization_count(&subject, "one-run", "one-parent", "child-definition-a"),
        0
    );

    let readd = subject.process(task_for("one", "parent", 20)).await;
    assert_eq!(readd.len(), 1);
    assert_eq!(
        realization_count(&subject, "one-run", "one-parent", "child-definition-a"),
        1
    );

    process_all(
        &mut subject,
        vec![
            node(
                "one-parent-two",
                "Task",
                21,
                json!({
                    "id": "one-parent-two",
                    "runId": "one-run",
                    "definitionId": "root-definition"
                }),
            ),
            relation(
                "one-parent-two-definition",
                "INSTANCE_OF",
                22,
                "one-parent-two",
                "root-definition",
            ),
            relation(
                "one-parent-two-run",
                "IN_RUN",
                23,
                "one-parent-two",
                "one-run",
            ),
        ],
    )
    .await;
    assert_eq!(
        realization_count(&subject, "one-run", "one-parent-two", "child-definition-a"),
        0
    );

    subject.process(delete("one-task-for", 24)).await;
    subject.process(task_for("one", "parent-two", 25)).await;
    assert_eq!(
        realization_count(&subject, "one-run", "one-parent", "child-definition-a"),
        0
    );
    assert_eq!(
        realization_count(&subject, "one-run", "one-parent-two", "child-definition-a"),
        1
    );
}

#[tokio::test]
async fn shared_definitions_do_not_cross_pair_runs_or_declared_children() {
    let mut subject = MaterializedQuery::new(AGGREGATE_QUERY).await;
    process_all(&mut subject, definition_events()).await;
    process_all(&mut subject, run_events("one", 10)).await;
    process_all(&mut subject, run_events("two", 30)).await;
    subject.process(task_for("one", "parent", 50)).await;
    subject.process(task_for("two", "parent", 51)).await;

    assert_eq!(subject.rows.len(), 4);
    for prefix in ["one", "two"] {
        assert_eq!(
            realization_count(
                &subject,
                &format!("{prefix}-run"),
                &format!("{prefix}-parent"),
                "child-definition-a"
            ),
            1
        );
        assert_eq!(
            realization_count(
                &subject,
                &format!("{prefix}-run"),
                &format!("{prefix}-parent"),
                "child-definition-b"
            ),
            0
        );
    }
}

#[tokio::test]
async fn structural_child_results_converge_across_event_order() {
    let mut interleaved = MaterializedQuery::new(AGGREGATE_QUERY).await;
    let mut nodes_first = MaterializedQuery::new(AGGREGATE_QUERY).await;
    let mut events = definition_events();
    events.extend(run_events("one", 10));
    events.push(task_for("one", "parent", 18));
    for event in events.clone() {
        let changes = interleaved.process(event).await;
        assert_valid_realization_outputs(&interleaved, &changes);
    }

    let (mut relations, nodes): (Vec<_>, Vec<_>) = events.into_iter().partition(|change| {
        matches!(
            change,
            SourceChange::Insert {
                element: Element::Relation { .. }
            }
        )
    });
    relations.reverse();
    for event in nodes.into_iter().chain(relations) {
        let changes = nodes_first.process(event).await;
        assert_valid_realization_outputs(&nodes_first, &changes);
    }

    assert_eq!(interleaved.rows, nodes_first.rows);
    assert_eq!(interleaved.rows.len(), 2);
    assert_eq!(
        realization_count(&interleaved, "one-run", "one-parent", "child-definition-a"),
        1
    );
}
