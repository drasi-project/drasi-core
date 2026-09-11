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

use drasi_query_ast::ast::{
    BinaryExpression, CaseExpression, IteratorExpression, ProjectionClause, Query, QueryPart,
};

use super::*;
use crate::{
    evaluation::functions::{future::RegisterFutureFunctions, RegisterAggregationFunctions},
    in_memory_index::{
        in_memory_future_queue::InMemoryFutureQueue, in_memory_result_index::InMemoryResultIndex,
        in_memory_temporal_index::InMemoryTemporalIndex,
    },
    models::ElementReference,
};

struct Harness {
    runtime: TemporalRuntime,
    index: Arc<InMemoryTemporalIndex>,
    queue: Arc<InMemoryFutureQueue>,
}

impl Harness {
    async fn new(parts: Vec<QueryPart>) -> Self {
        let namespace = fixtures::namespace();
        let registry = Arc::new(FunctionRegistry::new());
        registry.register_aggregation_functions();
        let result = Arc::new(InMemoryResultIndex::new());
        let queue = Arc::new(InMemoryFutureQueue::new());
        let evaluator = Arc::new(ExpressionEvaluator::new(registry.clone(), result.clone()));
        registry.register_future_functions(
            queue.clone(),
            result.clone(),
            Arc::downgrade(&evaluator),
        );
        let program = TemporalProgram::compile(&Query { parts }, &registry)
            .unwrap()
            .unwrap();
        let index = Arc::new(InMemoryTemporalIndex::new());
        index
            .store_catalog(TemporalCatalog {
                namespace,
                query: "engine test".into(),
                next_revision: SourceRevision(1),
            })
            .await
            .unwrap();
        let mut batch = TemporalBatch::new(namespace);
        batch
            .put(TemporalRecord::Epoch(EpochState::new(namespace)))
            .unwrap();
        index.apply(batch).await.unwrap();
        let runtime = TemporalRuntime::new(
            program,
            namespace,
            index.clone(),
            evaluator,
            queue.clone(),
            registry,
        );
        Self {
            runtime,
            index,
            queue,
        }
    }

    async fn rows(&self, part: usize) -> Vec<RetainedInput> {
        let Some(TemporalRecord::Part(state)) = self
            .index
            .get(&TemporalKey::Part {
                namespace: fixtures::namespace(),
                part: PartId(part),
            })
            .await
            .unwrap()
        else {
            return Vec::new();
        };
        let mut rows = Vec::new();
        for input in state.inputs {
            let Some(TemporalRecord::Input(row)) =
                self.index.get(&TemporalKey::Input(input)).await.unwrap()
            else {
                panic!("missing retained row");
            };
            rows.push(row);
        }
        rows
    }

    async fn due(&self, cutoff: u64) -> Vec<QueryPartEvaluationContext> {
        let mut result = Vec::new();
        while self
            .queue
            .peek_due_time()
            .await
            .unwrap()
            .is_some_and(|due| due <= cutoff)
        {
            let ticket = queue::decode(self.queue.pop().await.unwrap().unwrap()).unwrap();
            result.extend(self.runtime.process_ticket(ticket, cutoff).await.unwrap());
        }
        result
    }
}

fn int(value: i64) -> Expression {
    UnaryExpression::literal(Literal::Integer(value))
}

fn call(name: &str, position: usize, args: Vec<Expression>) -> Expression {
    Expression::FunctionExpression(FunctionExpression {
        name: name.into(),
        position_in_query: position,
        args,
    })
}

fn ident(name: &str) -> Expression {
    UnaryExpression::ident(name)
}

fn sum(position: usize) -> Expression {
    call("sum", position, vec![ident("x")])
}

fn true_for(condition: Expression, position: usize, duration: i64) -> Expression {
    call("drasi.trueFor", position, vec![condition, int(duration)])
}

fn item(predicates: Vec<Expression>) -> QueryPart {
    QueryPart {
        where_clauses: predicates,
        return_clause: ProjectionClause::Item(vec![ident("x")]),
        ..QueryPart::default()
    }
}

fn grouped(predicates: Vec<Expression>, grouping: Vec<Expression>) -> QueryPart {
    QueryPart {
        where_clauses: predicates,
        return_clause: ProjectionClause::GroupBy {
            grouping,
            aggregates: vec![UnaryExpression::alias(sum(100), "total".into())],
        },
        ..QueryPart::default()
    }
}

fn variables(x: i64, group: &str) -> QueryVariables {
    BTreeMap::from([
        ("x".into(), VariableValue::from(x)),
        ("group".into(), VariableValue::from(group)),
    ])
}

fn change(
    id: &str,
    before: Option<QueryVariables>,
    after: Option<QueryVariables>,
    time: u64,
) -> TemporalInputChange {
    TemporalInputChange {
        origin: InputOrigin::Match(MatchIdentity::Fixed {
            slots: vec![Some(ElementReference::new("test", id))],
        }),
        before,
        after,
        context: SavedContext {
            clock: ClockStamp {
                transaction_time: time,
                realtime: time,
            },
            input_grouping_hash: 7,
            solution_signature: Some(7),
            anchor: None,
        },
        row_signature: 7,
    }
}

fn total(output: &QueryPartEvaluationContext) -> &VariableValue {
    let QueryPartEvaluationContext::Aggregation { after, .. } = output else {
        panic!("expected group output: {output:?}");
    };
    &after["total"]
}

#[tokio::test]
async fn net_true_aggregate_update_preserves_activation_and_admits_each_origin_once() {
    let predicate = true_for(BinaryExpression::gt(sum(1), int(5)), 2, 10);
    let harness = Harness::new(vec![grouped(vec![predicate], vec![])]).await;
    assert!(harness
        .runtime
        .process_changes(vec![
            change("a", None, Some(variables(4, "g")), 100),
            change("b", None, Some(variables(4, "g")), 100),
        ])
        .await
        .unwrap()
        .is_empty());
    let rows = harness.rows(1).await;
    assert_eq!(rows.len(), 2);
    assert_ne!(rows[0].id, rows[1].id);
    let first = rows[0].tickets.values().next().unwrap().clone();
    assert!(matches!(
        first.cell.as_ref().unwrap().owner,
        StateOwner::Group(_)
    ));
    assert!(harness
        .runtime
        .process_changes(vec![
            change("a", Some(variables(4, "g")), Some(variables(2, "g")), 105),
            change("b", Some(variables(4, "g")), Some(variables(7, "g")), 105),
        ])
        .await
        .unwrap()
        .is_empty());
    let rows = harness.rows(1).await;
    for row in rows {
        let ticket = row.tickets.values().next().unwrap();
        assert_eq!(ticket.activation, first.activation);
        assert_eq!(ticket.due_time, 110);
    }
    let outputs = harness.due(110).await;
    assert_eq!(outputs.len(), 1);
    assert_eq!(total(&outputs[0]), &VariableValue::from(9));
    assert!(harness
        .runtime
        .process_ticket(first, 110)
        .await
        .unwrap()
        .is_empty());
    assert!(harness.due(200).await.is_empty());
}

#[tokio::test]
async fn equal_public_signatures_do_not_merge_item_lifetimes() {
    let harness = Harness::new(vec![item(vec![true_for(
        BinaryExpression::gt(ident("x"), int(0)),
        1,
        10,
    )])])
    .await;
    harness
        .runtime
        .process_changes(vec![
            change("a", None, Some(variables(2, "g")), 100),
            change("b", None, Some(variables(3, "g")), 100),
        ])
        .await
        .unwrap();
    let output = harness.due(110).await;
    assert_eq!(output.len(), 2);
    assert!(output.iter().all(|output| matches!(
        output,
        QueryPartEvaluationContext::Adding {
            row_signature: 7,
            ..
        }
    )));
    assert_eq!(harness.rows(1).await.len(), 2);
}

#[tokio::test]
async fn rejected_prefilter_is_retained_without_receipts_or_zero_output() {
    let harness = Harness::new(vec![grouped(
        vec![
            BinaryExpression::gt(ident("x"), int(0)),
            true_for(BinaryExpression::gt(sum(1), int(0)), 2, 10),
        ],
        vec![],
    )])
    .await;
    assert!(harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(-1, "g")), 100),])
        .await
        .unwrap()
        .is_empty());
    let rows = harness.rows(1).await;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].applied.predicate.is_empty());
    assert!(rows[0].applied.projection.is_empty());
    assert!(rows[0].tickets.is_empty());
    assert!(harness
        .runtime
        .process_changes(vec![change(
            "a",
            Some(variables(-1, "g")),
            Some(variables(2, "g")),
            105
        ),])
        .await
        .unwrap()
        .is_empty());
    assert_eq!(harness.due(115).await.len(), 1);
}

#[tokio::test]
async fn deletion_after_maturity_retracts_saved_projection_once_and_keeps_default_group() {
    let harness = Harness::new(vec![grouped(
        vec![true_for(BinaryExpression::gt(sum(1), int(0)), 2, 10)],
        vec![],
    )])
    .await;
    harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(4, "g")), 100)])
        .await
        .unwrap();
    assert_eq!(harness.due(110).await.len(), 1);
    let row = harness.rows(1).await.remove(0);
    let output = harness
        .runtime
        .process_changes(vec![change("a", Some(variables(4, "g")), None, 120)])
        .await
        .unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(total(&output[0]), &VariableValue::from(0));
    assert!(matches!(
        output[0],
        QueryPartEvaluationContext::Aggregation {
            default_after: true,
            ..
        }
    ));
    assert!(harness.rows(1).await.is_empty());
    assert!(harness
        .index
        .get(&TemporalKey::Input(row.id))
        .await
        .unwrap()
        .is_none());
    assert!(harness
        .index
        .get(&TemporalKey::Origin(row.origin_key()))
        .await
        .unwrap()
        .is_none());
    assert_eq!(harness.queue.peek_due_time().await.unwrap(), None);
}

#[tokio::test]
async fn group_movement_retires_old_receipts_and_uses_the_new_group_clock() {
    let harness = Harness::new(vec![grouped(
        vec![true_for(BinaryExpression::gt(sum(1), int(0)), 2, 10)],
        vec![ident("group")],
    )])
    .await;
    harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(4, "old")), 100)])
        .await
        .unwrap();
    assert_eq!(harness.due(110).await.len(), 1);
    let output = harness
        .runtime
        .process_changes(vec![change(
            "a",
            Some(variables(4, "old")),
            Some(variables(4, "new")),
            120,
        )])
        .await
        .unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(total(&output[0]), &VariableValue::from(0));
    assert_eq!(harness.queue.peek_due_time().await.unwrap(), Some(130));
    let output = harness.due(130).await;
    assert_eq!(output.len(), 1);
    assert_eq!(total(&output[0]), &VariableValue::from(4));
}

#[tokio::test]
async fn due_cutoff_preserves_later_requests_and_unchanged_source_input() {
    let harness = Harness::new(vec![item(vec![BinaryExpression::or(
        call(
            "drasi.trueLater",
            1,
            vec![UnaryExpression::literal(Literal::Boolean(true)), int(110)],
        ),
        call(
            "drasi.trueLater",
            2,
            vec![UnaryExpression::literal(Literal::Boolean(true)), int(120)],
        ),
    )])])
    .await;
    harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(4, "g")), 100)])
        .await
        .unwrap();
    let original = harness.rows(1).await.remove(0);
    let original_bytes = codec::encode_record(&TemporalRecord::Input(original.clone())).unwrap();
    assert_eq!(original.tickets.len(), 2);
    let later = original
        .tickets
        .values()
        .find(|ticket| ticket.due_time == 120)
        .unwrap()
        .clone();
    assert!(harness
        .runtime
        .process_ticket(later.clone(), 110)
        .await
        .unwrap()
        .is_empty());
    assert!(harness
        .runtime
        .process_changes(vec![change(
            "a",
            Some(variables(4, "g")),
            Some(variables(4, "g")),
            109
        ),])
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        codec::encode_record(&TemporalRecord::Input(harness.rows(1).await.remove(0))).unwrap(),
        original_bytes
    );
    assert_eq!(harness.due(110).await.len(), 1);
    let refreshed = harness.rows(1).await.remove(0);
    assert_eq!(refreshed.tickets.len(), 1);
    assert!(refreshed.accepts(&later));
    assert_eq!(refreshed.source_revision, original.source_revision);
    assert_eq!(refreshed.source_clock, original.source_clock);
    assert_eq!(harness.queue.peek_due_time().await.unwrap(), Some(120));
    assert!(harness.due(120).await.is_empty());
}

#[tokio::test]
async fn free_row_variable_outside_aggregate_keeps_true_for_input_owned() {
    let harness = Harness::new(vec![grouped(
        vec![true_for(BinaryExpression::gt(sum(1), ident("x")), 2, 10)],
        vec![],
    )])
    .await;
    harness
        .runtime
        .process_changes(vec![
            change("a", None, Some(variables(2, "g")), 100),
            change("b", None, Some(variables(3, "g")), 100),
        ])
        .await
        .unwrap();
    for row in harness.rows(1).await {
        assert!(row.tickets.values().all(|ticket| {
            ticket
                .cell
                .as_ref()
                .is_some_and(|cell| cell.owner == StateOwner::Input(row.id))
        }));
    }
}

#[tokio::test]
async fn unrelated_group_does_not_capture_history_or_advance_its_clock() {
    let harness = Harness::new(vec![grouped(
        vec![true_for(BinaryExpression::gt(sum(1), int(0)), 2, 10)],
        vec![ident("group")],
    )])
    .await;
    harness
        .runtime
        .process_changes(vec![
            change("a", None, Some(variables(2, "a")), 100),
            change("b", None, Some(variables(3, "b")), 100),
        ])
        .await
        .unwrap();
    let old = harness
        .rows(1)
        .await
        .into_iter()
        .find(|row| row.variables["group"] == VariableValue::from("b"))
        .unwrap();
    harness
        .runtime
        .process_changes(vec![change(
            "a",
            Some(variables(2, "a")),
            Some(variables(4, "a")),
            105,
        )])
        .await
        .unwrap();
    let new = harness
        .rows(1)
        .await
        .into_iter()
        .find(|row| row.id == old.id)
        .unwrap();
    assert_eq!(
        codec::encode_record(&TemporalRecord::Input(old)).unwrap(),
        codec::encode_record(&TemporalRecord::Input(new)).unwrap()
    );
}

#[tokio::test]
async fn replacement_member_does_not_reset_a_shared_true_for_cell() {
    let harness = Harness::new(vec![grouped(
        vec![true_for(BinaryExpression::gt(sum(1), int(0)), 2, 10)],
        vec![],
    )])
    .await;
    harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(2, "g")), 100)])
        .await
        .unwrap();
    let previous = harness
        .rows(1)
        .await
        .remove(0)
        .tickets
        .into_values()
        .next()
        .unwrap();
    harness
        .runtime
        .process_changes(vec![
            change("a", Some(variables(2, "g")), None, 105),
            change("b", None, Some(variables(3, "g")), 105),
        ])
        .await
        .unwrap();
    let replacement = harness
        .rows(1)
        .await
        .remove(0)
        .tickets
        .into_values()
        .next()
        .unwrap();
    assert_eq!(previous.cell, replacement.cell);
    assert_eq!(previous.activation, replacement.activation);
    assert_eq!(replacement.due_time, 110);
    assert_ne!(previous.id.input, replacement.id.input);
    assert_eq!(harness.due(110).await.len(), 1);
}

#[tokio::test]
async fn expired_sliding_window_retracts_its_nested_aggregate_once() {
    let harness = Harness::new(vec![QueryPart {
        return_clause: ProjectionClause::GroupBy {
            grouping: vec![],
            aggregates: vec![UnaryExpression::alias(
                call("drasi.slidingWindow", 2, vec![int(10), sum(1)]),
                "total".into(),
            )],
        },
        ..QueryPart::default()
    }])
    .await;
    let output = harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(3, "g")), 100)])
        .await
        .unwrap();
    assert_eq!(total(&output[0]), &VariableValue::from(3));
    let before = harness.rows(1).await.remove(0);
    let ticket = before.tickets.values().next().unwrap().clone();
    let output = harness.due(110).await;
    assert_eq!(output.len(), 1);
    assert_eq!(total(&output[0]), &VariableValue::from(0));
    let after = harness.rows(1).await.remove(0);
    assert!(after.applied.projection.is_empty());
    assert_eq!(after.source_revision, before.source_revision);
    assert_eq!(after.source_clock, before.source_clock);
    assert!(harness
        .runtime
        .process_ticket(ticket, 200)
        .await
        .unwrap()
        .is_empty());
    assert!(harness
        .runtime
        .process_changes(vec![change("a", Some(variables(3, "g")), None, 120),])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn history_is_not_captured_again_by_a_timer() {
    let harness = Harness::new(vec![QueryPart {
        return_clause: ProjectionClause::Item(vec![
            UnaryExpression::alias(
                call("drasi.previousValue", 1, vec![ident("x"), int(-1)]),
                "previous".into(),
            ),
            UnaryExpression::alias(
                call(
                    "drasi.trueLater",
                    2,
                    vec![UnaryExpression::literal(Literal::Boolean(true)), int(110)],
                ),
                "ready".into(),
            ),
        ]),
        ..QueryPart::default()
    }])
    .await;
    harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(3, "g")), 100)])
        .await
        .unwrap();
    let before = harness.rows(1).await.remove(0);
    let history_id = before.dependencies.iter().next().unwrap();
    let history = harness
        .index
        .get(&TemporalKey::Cell(history_id.clone()))
        .await
        .unwrap()
        .unwrap();
    let output = harness.due(110).await;
    assert_eq!(output.len(), 1);
    let QueryPartEvaluationContext::Updating { after, .. } = &output[0] else {
        panic!("expected timer projection update");
    };
    assert_eq!(after["previous"], VariableValue::from(-1));
    let refreshed = harness
        .index
        .get(&TemporalKey::Cell(history_id.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        codec::encode_record(&history).unwrap(),
        codec::encode_record(&refreshed).unwrap()
    );
    let output = harness
        .runtime
        .process_changes(vec![change(
            "a",
            Some(variables(3, "g")),
            Some(variables(4, "g")),
            120,
        )])
        .await
        .unwrap();
    let QueryPartEvaluationContext::Updating { after, .. } = &output[0] else {
        panic!("expected source projection update");
    };
    assert_eq!(after["previous"], VariableValue::from(3));
}

#[tokio::test]
async fn same_valued_upstream_output_reaches_later_history_with_a_source_revision() {
    let harness = Harness::new(vec![
        QueryPart {
            return_clause: ProjectionClause::Item(vec![UnaryExpression::alias(int(1), "k".into())]),
            ..QueryPart::default()
        },
        QueryPart {
            return_clause: ProjectionClause::Item(vec![UnaryExpression::alias(
                call("drasi.previousValue", 1, vec![ident("k"), int(-1)]),
                "previous".into(),
            )]),
            ..QueryPart::default()
        },
    ])
    .await;
    let first = harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(3, "g")), 100)])
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    let second = harness
        .runtime
        .process_changes(vec![change(
            "a",
            Some(variables(3, "g")),
            Some(variables(4, "g")),
            105,
        )])
        .await
        .unwrap();
    let QueryPartEvaluationContext::Updating { before, after, .. } = &second[0] else {
        panic!("source-clock refresh did not reach downstream history");
    };
    assert_eq!(before["previous"], VariableValue::from(-1));
    assert_eq!(after["previous"], VariableValue::from(1));
    assert_eq!(
        harness.rows(2).await.remove(0).source_revision,
        SourceRevision(2)
    );
}

#[tokio::test]
async fn noop_batch_does_not_allocate_catalog_or_incarnations() {
    let harness = Harness::new(vec![item(vec![true_for(
        BinaryExpression::gt(ident("x"), int(0)),
        1,
        10,
    )])])
    .await;
    let catalog = harness.index.load_catalog().await.unwrap().unwrap();
    let epoch = harness
        .index
        .get(&TemporalKey::Epoch(fixtures::namespace()))
        .await
        .unwrap()
        .unwrap();
    assert!(harness
        .runtime
        .process_changes(vec![
            change("a", None, Some(variables(3, "g")), 100),
            change("a", Some(variables(3, "g")), None, 100),
        ])
        .await
        .unwrap()
        .is_empty());
    assert!(harness.rows(1).await.is_empty());
    assert_eq!(
        harness.index.load_catalog().await.unwrap().unwrap(),
        catalog
    );
    let after = harness
        .index
        .get(&TemporalKey::Epoch(fixtures::namespace()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        codec::encode_record(&epoch).unwrap(),
        codec::encode_record(&after).unwrap()
    );
}

#[tokio::test]
async fn empty_group_preserves_activation_generation_until_its_default_lifetime_ends() {
    let harness = Harness::new(vec![grouped(
        vec![true_for(BinaryExpression::gt(sum(1), int(0)), 2, 10)],
        vec![],
    )])
    .await;
    harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(2, "g")), 100)])
        .await
        .unwrap();
    let first = harness
        .rows(1)
        .await
        .remove(0)
        .tickets
        .into_values()
        .next()
        .unwrap();
    harness.due(110).await;
    harness
        .runtime
        .process_changes(vec![change("a", Some(variables(2, "g")), None, 115)])
        .await
        .unwrap();
    harness
        .runtime
        .process_changes(vec![change("b", None, Some(variables(3, "g")), 120)])
        .await
        .unwrap();
    let second = harness
        .rows(1)
        .await
        .remove(0)
        .tickets
        .into_values()
        .next()
        .unwrap();
    assert_eq!(first.cell, second.cell);
    assert!(second.activation > first.activation);
    assert_eq!(second.due_time, 130);
}

#[tokio::test]
async fn snapshot_refresh_preserves_the_context_of_the_applied_receipt() {
    let harness = Harness::new(vec![grouped(
        vec![true_for(BinaryExpression::gt(sum(1), int(0)), 2, 10)],
        vec![],
    )])
    .await;
    harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(2, "g")), 100)])
        .await
        .unwrap();
    let before = harness.rows(1).await.remove(0).applied.predicate.remove(0);
    harness.due(110).await;
    let after = harness.rows(1).await.remove(0).applied.predicate.remove(0);
    assert_eq!(before.context.clock, after.context.clock);
    assert_eq!(after.context.clock.realtime, 100);
}

#[tokio::test]
async fn multiple_projection_aggregates_publish_one_settled_group_snapshot() {
    let harness = Harness::new(vec![QueryPart {
        return_clause: ProjectionClause::GroupBy {
            grouping: vec![],
            aggregates: vec![
                UnaryExpression::alias(sum(1), "one".into()),
                UnaryExpression::alias(sum(2), "two".into()),
                UnaryExpression::alias(
                    call("drasi.previousValue", 3, vec![sum(4), int(-1)]),
                    "previous".into(),
                ),
            ],
        },
        ..QueryPart::default()
    }])
    .await;
    let output = harness
        .runtime
        .process_changes(vec![
            change("a", None, Some(variables(2, "g")), 100),
            change("b", None, Some(variables(3, "g")), 100),
        ])
        .await
        .unwrap();
    assert_eq!(output.len(), 1);
    let QueryPartEvaluationContext::Aggregation { after, .. } = &output[0] else {
        panic!("expected one group");
    };
    assert_eq!(after["one"], VariableValue::from(5));
    assert_eq!(after["two"], VariableValue::from(5));
    assert_eq!(after["previous"], VariableValue::from(-1));
    let output = harness
        .runtime
        .process_changes(vec![change(
            "a",
            Some(variables(2, "g")),
            Some(variables(4, "g")),
            110,
        )])
        .await
        .unwrap();
    assert_eq!(output.len(), 1);
    let QueryPartEvaluationContext::Aggregation { after, .. } = &output[0] else {
        panic!("expected one changed group");
    };
    assert_eq!(after["one"], VariableValue::from(7));
    assert_eq!(after["two"], VariableValue::from(7));
    assert_eq!(after["previous"], VariableValue::from(5));
}

#[tokio::test]
async fn a_prefiltered_nonsubscriber_in_the_same_group_keeps_its_history_and_clock() {
    let harness = Harness::new(vec![grouped(
        vec![
            BinaryExpression::ge(
                call("drasi.previousValue", 10, vec![ident("x"), int(100)]),
                int(0),
            ),
            BinaryExpression::gt(ident("x"), int(0)),
            true_for(BinaryExpression::gt(sum(1), int(0)), 2, 10),
        ],
        vec![],
    )])
    .await;
    harness
        .runtime
        .process_changes(vec![
            change("a", None, Some(variables(2, "g")), 100),
            change("b", None, Some(variables(-1, "g")), 100),
        ])
        .await
        .unwrap();
    let before = harness
        .rows(1)
        .await
        .into_iter()
        .find(|row| row.variables["x"] == VariableValue::from(-1))
        .unwrap();
    assert!(before
        .dependencies
        .iter()
        .all(|cell| matches!(cell.owner, StateOwner::Input(_))));
    harness
        .runtime
        .process_changes(vec![change(
            "a",
            Some(variables(2, "g")),
            Some(variables(4, "g")),
            105,
        )])
        .await
        .unwrap();
    let after = harness
        .rows(1)
        .await
        .into_iter()
        .find(|row| row.id == before.id)
        .unwrap();
    assert_eq!(
        codec::encode_record(&TemporalRecord::Input(before)).unwrap(),
        codec::encode_record(&TemporalRecord::Input(after)).unwrap()
    );
}

#[tokio::test]
async fn expired_window_keeps_its_snapshot_subscription_without_reapplying_a_receipt() {
    let harness = Harness::new(vec![QueryPart {
        return_clause: ProjectionClause::Item(vec![UnaryExpression::alias(
            call("drasi.slidingWindow", 2, vec![int(10), sum(1)]),
            "total".into(),
        )]),
        ..QueryPart::default()
    }])
    .await;
    harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(3, "g")), 100)])
        .await
        .unwrap();
    harness.due(110).await;
    let expired = harness.rows(1).await.remove(0);
    assert!(expired.applied.projection.is_empty());
    assert!(expired
        .dependencies
        .iter()
        .any(|cell| matches!(cell.owner, StateOwner::Group(_))));
    let output = harness
        .runtime
        .process_changes(vec![change("b", None, Some(variables(4, "g")), 120)])
        .await
        .unwrap();
    assert_eq!(output.len(), 2);
    for row in harness.rows(1).await {
        let after = delivered(&row.applied.last_delivered).unwrap();
        assert_eq!(after.variables["total"], VariableValue::from(4));
        if row.id == expired.id {
            assert!(row.applied.projection.is_empty());
        }
    }
}

#[tokio::test]
async fn default_snapshot_uses_the_last_admitted_row_not_an_earlier_rejected_row() {
    let harness = Harness::new(vec![QueryPart {
        where_clauses: vec![BinaryExpression::gt(ident("x"), int(0))],
        return_clause: ProjectionClause::GroupBy {
            grouping: vec![],
            aggregates: vec![
                UnaryExpression::alias(sum(1), "total".into()),
                UnaryExpression::alias(
                    call("drasi.previousValue", 2, vec![sum(3), int(-1)]),
                    "previous".into(),
                ),
            ],
        },
        ..QueryPart::default()
    }])
    .await;
    harness
        .runtime
        .process_changes(vec![
            change("a", None, Some(variables(-1, "g")), 100),
            change("b", None, Some(variables(3, "g")), 100),
        ])
        .await
        .unwrap();
    let output = harness
        .runtime
        .process_changes(vec![change("b", Some(variables(3, "g")), None, 110)])
        .await
        .unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(total(&output[0]), &VariableValue::from(0));
    assert!(harness
        .runtime
        .process_changes(vec![change("c", None, Some(variables(-2, "g")), 120)])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn public_default_hints_do_not_replace_authoritative_real_group_presence() {
    let harness = Harness::new(vec![
        grouped(
            vec![call(
                "drasi.trueLater",
                1,
                vec![UnaryExpression::literal(Literal::Boolean(true)), int(0)],
            )],
            vec![],
        ),
        QueryPart {
            return_clause: ProjectionClause::GroupBy {
                grouping: vec![],
                aggregates: vec![UnaryExpression::alias(
                    call("sum", 101, vec![ident("total")]),
                    "total".into(),
                )],
            },
            ..QueryPart::default()
        },
    ])
    .await;
    harness
        .runtime
        .process_changes(vec![
            change("a", None, Some(variables(2, "g")), 100),
            change("b", None, Some(variables(3, "g")), 100),
        ])
        .await
        .unwrap();
    let output = harness
        .runtime
        .process_changes(vec![change("a", Some(variables(2, "g")), None, 110)])
        .await
        .unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(total(&output[0]), &VariableValue::from(3));
    let output = harness
        .runtime
        .process_changes(vec![change("c", None, Some(variables(4, "g")), 120)])
        .await
        .unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(total(&output[0]), &VariableValue::from(7));
    let downstream = harness.rows(2).await;
    assert_eq!(downstream.len(), 1);
    assert_eq!(downstream[0].applied.projection.len(), 1);
}

#[tokio::test]
async fn missing_catalog_and_exhausted_revision_do_not_guess_new_history() {
    let harness = Harness::new(vec![item(vec![true_for(
        BinaryExpression::gt(ident("x"), int(0)),
        1,
        10,
    )])])
    .await;
    let mut catalog = harness.index.load_catalog().await.unwrap().unwrap();
    catalog.next_revision = SourceRevision(u64::MAX);
    harness.index.store_catalog(catalog).await.unwrap();
    assert!(matches!(
        harness
            .runtime
            .process_changes(vec![change("a", None, Some(variables(2, "g")), 100)])
            .await,
        Err(EvaluationError::OverflowError)
    ));
    assert!(harness.rows(1).await.is_empty());
    harness.index.clear().await.unwrap();
    assert!(matches!(
        harness
            .runtime
            .process_changes(vec![change("a", None, Some(variables(2, "g")), 100)])
            .await,
        Err(EvaluationError::IndexError(_))
    ));
}

#[tokio::test]
async fn derived_group_windows_use_the_source_event_not_the_representatives_old_clock() {
    let harness = Harness::new(vec![
        grouped(vec![], vec![]),
        QueryPart {
            return_clause: ProjectionClause::GroupBy {
                grouping: vec![],
                aggregates: vec![UnaryExpression::alias(
                    call(
                        "drasi.slidingWindow",
                        101,
                        vec![int(10), call("sum", 102, vec![ident("total")])],
                    ),
                    "total".into(),
                )],
            },
            ..QueryPart::default()
        },
    ])
    .await;
    harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(3, "g")), 100)])
        .await
        .unwrap();
    assert_eq!(harness.queue.peek_due_time().await.unwrap(), Some(110));
    let output = harness
        .runtime
        .process_changes(vec![change("b", None, Some(variables(4, "g")), 105)])
        .await
        .unwrap();
    assert_eq!(total(&output[0]), &VariableValue::from(7));
    assert_eq!(harness.queue.peek_due_time().await.unwrap(), Some(115));
    assert!(harness.due(110).await.is_empty());
    let output = harness.due(115).await;
    assert_eq!(total(&output[0]), &VariableValue::from(0));
}

#[tokio::test]
async fn equal_deadline_iterator_occurrences_keep_distinct_tickets_and_prune_old_values() {
    let harness = Harness::new(vec![QueryPart {
        return_clause: ProjectionClause::Item(vec![UnaryExpression::alias(
            IteratorExpression::map(
                "deadline".into(),
                ident("deadlines"),
                call(
                    "drasi.trueLater",
                    1,
                    vec![
                        UnaryExpression::literal(Literal::Boolean(true)),
                        ident("deadline"),
                    ],
                ),
            ),
            "values".into(),
        )]),
        ..QueryPart::default()
    }])
    .await;
    let before = BTreeMap::from([(
        "deadlines".into(),
        VariableValue::List(vec![
            VariableValue::from(110),
            VariableValue::from(110),
            VariableValue::from(120),
        ]),
    )]);
    harness
        .runtime
        .process_changes(vec![change("a", None, Some(before.clone()), 100)])
        .await
        .unwrap();
    let row = harness.rows(1).await.remove(0);
    assert_eq!(row.tickets.len(), 3);
    assert_eq!(
        row.tickets
            .keys()
            .map(|ticket| &ticket.call.occurrence)
            .collect::<BTreeSet<_>>()
            .len(),
        3
    );
    assert_eq!(harness.due(110).await.len(), 1);
    assert_eq!(harness.queue.peek_due_time().await.unwrap(), Some(120));
    let after = BTreeMap::from([(
        "deadlines".into(),
        VariableValue::List(vec![VariableValue::from(120)]),
    )]);
    harness
        .runtime
        .process_changes(vec![change("a", Some(before), Some(after), 115)])
        .await
        .unwrap();
    let row = harness.rows(1).await.remove(0);
    assert_eq!(row.tickets.len(), 1);
    assert_eq!(row.values.len(), 1);
    assert_eq!(row.values.keys().next().unwrap().occurrence, vec![0]);
    assert_eq!(harness.due(120).await.len(), 1);
}

#[tokio::test]
async fn an_unvisited_branch_unsubscribes_and_preserves_a_live_groups_generation() {
    let harness = Harness::new(vec![grouped(
        vec![CaseExpression::case(
            None,
            vec![(
                BinaryExpression::gt(ident("x"), int(0)),
                true_for(BinaryExpression::gt(sum(1), int(0)), 2, 10),
            )],
            Some(UnaryExpression::literal(Literal::Boolean(false))),
        )],
        vec![],
    )])
    .await;
    harness
        .runtime
        .process_changes(vec![change("a", None, Some(variables(2, "g")), 100)])
        .await
        .unwrap();
    let first = harness
        .rows(1)
        .await
        .remove(0)
        .tickets
        .into_values()
        .next()
        .unwrap();
    harness
        .runtime
        .process_changes(vec![change(
            "a",
            Some(variables(2, "g")),
            Some(variables(-1, "g")),
            105,
        )])
        .await
        .unwrap();
    let row = harness.rows(1).await.remove(0);
    assert!(row.dependencies.is_empty());
    assert!(row.values.is_empty());
    assert!(row.applied.predicate.is_empty());
    assert!(row.tickets.is_empty());
    harness
        .runtime
        .process_changes(vec![change(
            "a",
            Some(variables(-1, "g")),
            Some(variables(2, "g")),
            120,
        )])
        .await
        .unwrap();
    let next = harness
        .rows(1)
        .await
        .remove(0)
        .tickets
        .into_values()
        .next()
        .unwrap();
    assert_eq!(first.cell, next.cell);
    assert!(next.activation > first.activation);
    assert_eq!(next.due_time, 130);
}
