// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::HashMap, sync::Arc};

use drasi_query_cypher::CypherParser;
use serde_json::json;

use crate::{
    computation::{ComputationIndexes, ComputationQuery, ComputationQueryError},
    evaluation::{
        context::{QueryPartEvaluationContext, QueryVariables},
        functions::{aggregation::ValueAccumulator, Count, Function, FunctionRegistry, Sum},
        variable_value::VariableValue,
        EvaluationError,
    },
    in_memory_index::{
        in_memory_element_index::InMemoryElementIndex, in_memory_future_queue::InMemoryFutureQueue,
        in_memory_result_index::InMemoryResultIndex,
    },
    interface::{
        AccumulatorIndex, ElementIndex, IndexError, IndexSet, NoOpSessionControl, ResultKey,
        ResultOwner,
    },
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};

struct Store {
    elements: Arc<InMemoryElementIndex>,
    results: Arc<InMemoryResultIndex>,
    futures: Arc<InMemoryFutureQueue>,
}

impl Store {
    fn new() -> Self {
        Self {
            elements: Arc::new(InMemoryElementIndex::new()),
            results: Arc::new(InMemoryResultIndex::new()),
            futures: Arc::new(InMemoryFutureQueue::new()),
        }
    }

    async fn query(&self, text: &str) -> ComputationQuery {
        let functions = Arc::new(FunctionRegistry::new());
        functions.register_function("count", Function::Aggregating(Arc::new(Count {})));
        functions.register_function("sum", Function::Aggregating(Arc::new(Sum {})));
        let builder = QueryBuilder::new(text, Arc::new(CypherParser::new(functions.clone())))
            .with_function_registry(functions);
        let resources = ComputationIndexes::try_new(
            IndexSet {
                element_index: self.elements.clone(),
                archive_index: self.elements.clone(),
                result_index: self.results.clone(),
                future_queue: self.futures.clone(),
                session_control: Arc::new(NoOpSessionControl),
            },
            None,
            None,
            None,
            None,
        )
        .unwrap();
        ComputationQuery::try_build(builder, resources)
            .await
            .unwrap()
    }
}

struct Subject {
    store: Store,
    query: ComputationQuery,
    rows: HashMap<u64, QueryVariables>,
}

impl Subject {
    async fn new(text: &str) -> Self {
        let store = Store::new();
        let query = store.query(text).await;
        Self {
            store,
            query,
            rows: HashMap::new(),
        }
    }

    async fn apply(&mut self, change: SourceChange) -> Vec<QueryPartEvaluationContext> {
        let changes = self.query.process_source_change(change).await.unwrap();
        for change in changes.iter() {
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
                    assert!(self.rows.remove(row_signature).is_some());
                }
                QueryPartEvaluationContext::Noop => {}
            }
        }
        changes
    }

    fn scalar(&self, name: &str) -> &VariableValue {
        assert_eq!(self.rows.len(), 1);
        self.rows.values().next().unwrap().get(name).unwrap()
    }

    async fn reconstruct(&mut self, text: &str) {
        self.query.shutdown().await.unwrap();
        self.query = self.store.query(text).await;
    }
}

fn metadata(source: &str, id: &str, time: u64) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(source, id),
        labels: Arc::from([Arc::from("Item")]),
        effective_from: time,
    }
}

fn node(source: &str, id: &str, time: u64, value: i64) -> Element {
    Element::Node {
        metadata: metadata(source, id, time),
        properties: ElementPropertyMap::from(json!({"key":format!("{source}/{id}"),"value":value})),
    }
}

fn insert(source: &str, id: &str, time: u64, value: i64) -> SourceChange {
    SourceChange::Insert {
        element: node(source, id, time, value),
    }
}

fn update(source: &str, id: &str, time: u64, value: i64) -> SourceChange {
    SourceChange::Update {
        element: node(source, id, time, value),
    }
}

fn delete(source: &str, id: &str, time: u64) -> SourceChange {
    SourceChange::Delete {
        metadata: metadata(source, id, time),
    }
}

#[tokio::test]
async fn invalid_input_timestamps_are_not_accepted_as_source_frontiers() {
    for change in [
        insert("a", "one", u64::MAX, 7),
        update("a", "one", u64::MAX, 7),
        delete("a", "one", u64::MAX),
    ] {
        let store = Store::new();
        let query = store.query("MATCH (n:Item) RETURN n.value AS value").await;
        let error = query.process_source_change(change).await.unwrap_err();
        assert!(matches!(
            error,
            ComputationQueryError::Evaluation(EvaluationError::InvalidSourceTime {
                timestamp: u64::MAX,
                ..
            })
        ));
        let reference = ElementReference::new("a", "one");
        assert!(store
            .elements
            .get_element(&reference)
            .await
            .unwrap()
            .is_none());
        assert!(store
            .results
            .get(
                &ResultKey::Element(reference),
                &ResultOwner::ElementTombstone
            )
            .await
            .unwrap()
            .is_none());
        query.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn corrupt_persisted_frontiers_fail_instead_of_silently_skipping_valid_input() {
    for corruption in 0..3 {
        let store = Store::new();
        let query = store.query("MATCH (n:Item) RETURN n.value AS value").await;
        let reference = ElementReference::new("a", "one");
        if corruption != 0 {
            store
                .elements
                .set_element(
                    &node("a", "one", if corruption == 1 { u64::MAX } else { 10 }, 1),
                    &Vec::new(),
                )
                .await
                .unwrap();
        }
        if corruption != 1 {
            store
                .results
                .set(
                    ResultKey::Element(reference.clone()),
                    ResultOwner::ElementTombstone,
                    Some(ValueAccumulator::TimeMarker {
                        timestamp: if corruption == 0 { u64::MAX } else { 20 },
                    }),
                )
                .await
                .unwrap();
        }
        let before = store.elements.get_element(&reference).await.unwrap();
        let error = query
            .process_source_change(update("a", "one", 30, 7))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ComputationQueryError::Evaluation(EvaluationError::IndexError(
                IndexError::CorruptedData
            ))
        ));
        assert_eq!(
            store.elements.get_element(&reference).await.unwrap(),
            before
        );
        query.shutdown().await.unwrap();
    }
}

const PROJECTION: &str = "MATCH (n:Item) RETURN n.key AS key, n.value AS value";

#[tokio::test]
async fn stale_mutations_preserve_rows_and_do_not_stop_later_input() {
    let mut subject = Subject::new(PROJECTION).await;
    subject.apply(insert("a", "one", 30, 3)).await;
    assert!(subject.apply(update("a", "one", 20, 2)).await.is_empty());
    assert!(subject.apply(delete("a", "one", 15)).await.is_empty());
    assert_eq!(subject.scalar("value"), &VariableValue::from(json!(3)));
    let stored = subject
        .store
        .elements
        .get_element(&ElementReference::new("a", "one"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.get_effective_from(), 30);
    assert_eq!(
        stored.get_property("value"),
        &crate::models::ElementValue::from(&json!(3))
    );
    let changes = subject.apply(update("a", "one", 40, 4)).await;
    assert!(matches!(
        changes.as_slice(),
        [QueryPartEvaluationContext::Updating { .. }]
    ));
    assert_eq!(subject.scalar("value"), &VariableValue::from(json!(4)));
    assert!(!subject.query.recovery_required());
}

#[tokio::test]
async fn repeated_insert_is_an_upsert_not_an_additional_aggregate_contribution() {
    let mut subject =
        Subject::new("MATCH (n:Item) RETURN count(n) AS count, sum(n.value) AS total").await;
    subject.apply(insert("a", "one", 10, 10)).await;
    assert!(subject.apply(insert("a", "one", 10, 10)).await.is_empty());
    subject.apply(insert("a", "one", 20, 20)).await;
    assert_eq!(subject.scalar("count"), &VariableValue::from(json!(1)));
    assert_eq!(subject.scalar("total"), &VariableValue::from(json!(20.0)));
    subject.apply(insert("a", "two", 21, 5)).await;
    assert_eq!(subject.scalar("count"), &VariableValue::from(json!(2)));
    assert_eq!(subject.scalar("total"), &VariableValue::from(json!(25.0)));
    subject.apply(delete("a", "one", 30)).await;
    assert_eq!(subject.scalar("count"), &VariableValue::from(json!(1)));
    assert_eq!(subject.scalar("total"), &VariableValue::from(json!(5.0)));
}

#[tokio::test]
async fn delete_frontier_survives_reconstruction_and_blocks_stale_resurrection() {
    let mut subject = Subject::new(PROJECTION).await;
    subject.apply(insert("a", "one", 10, 1)).await;
    subject.apply(delete("a", "one", 30)).await;
    subject.reconstruct(PROJECTION).await;
    assert!(subject.apply(insert("a", "one", 20, 2)).await.is_empty());
    assert!(subject.apply(update("a", "one", 29, 2)).await.is_empty());
    subject.apply(delete("a", "one", 40)).await;
    subject.reconstruct(PROJECTION).await;
    assert!(subject.apply(insert("a", "one", 35, 3)).await.is_empty());
    assert!(subject.rows.is_empty());
    subject.apply(insert("a", "one", 50, 5)).await;
    assert_eq!(subject.scalar("value"), &VariableValue::from(json!(5)));
}

#[tokio::test]
async fn deletion_of_absent_element_establishes_a_source_scoped_frontier() {
    let mut subject = Subject::new(PROJECTION).await;
    subject.apply(delete("a", "one", 50)).await;
    assert!(subject.apply(insert("a", "one", 40, 4)).await.is_empty());
    subject.apply(insert("b", "one", 10, 1)).await;
    assert_eq!(subject.scalar("key"), &VariableValue::from(json!("b/one")));
    subject.apply(insert("a", "one", 60, 6)).await;
    assert_eq!(subject.rows.len(), 2);
}

#[tokio::test]
async fn all_three_event_orders_converge_without_resurrecting_deleted_state() {
    for order in [
        [0, 1, 2],
        [0, 2, 1],
        [1, 0, 2],
        [1, 2, 0],
        [2, 0, 1],
        [2, 1, 0],
    ] {
        let mut subject = Subject::new(PROJECTION).await;
        for (step, event) in order.into_iter().enumerate() {
            let change = match event {
                0 => insert("a", "one", 10, 1),
                1 => update("a", "one", 20, 2),
                2 => delete("a", "one", 30),
                _ => unreachable!(),
            };
            subject.apply(change).await;
            if step == 1 {
                subject.reconstruct(PROJECTION).await;
            }
        }
        assert!(subject.rows.is_empty(), "order {order:?}");
        assert!(subject
            .store
            .elements
            .get_element(&ElementReference::new("a", "one"))
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn equal_source_times_keep_admitted_order_and_full_insert_replaces_properties() {
    let mut subject = Subject::new(PROJECTION).await;
    subject.apply(insert("a", "one", 10, 1)).await;
    subject.apply(update("a", "one", 10, 2)).await;
    assert_eq!(subject.scalar("value"), &VariableValue::from(json!(2)));
    subject
        .apply(SourceChange::Insert {
            element: Element::Node {
                metadata: metadata("a", "one", 10),
                properties: ElementPropertyMap::from(json!({"key":"a/one"})),
            },
        })
        .await;
    assert_eq!(subject.scalar("value"), &VariableValue::Null);
}

#[tokio::test]
async fn incompatible_insert_and_update_fail_before_changing_the_existing_element() {
    for relation_first in [false, true] {
        for is_update in [false, true] {
            let mut subject = Subject::new(PROJECTION).await;
            let relation = |time| Element::Relation {
                metadata: metadata("a", "one", time),
                in_node: ElementReference::new("a", "left"),
                out_node: ElementReference::new("a", "right"),
                properties: ElementPropertyMap::from(json!({"value":99})),
            };
            let initial = if relation_first {
                relation(10)
            } else {
                node("a", "one", 10, 1)
            };
            subject
                .apply(SourceChange::Insert {
                    element: initial.clone(),
                })
                .await;
            let replacement = if relation_first {
                node("a", "one", 20, 2)
            } else {
                relation(20)
            };
            let error = subject
                .query
                .process_source_change(if is_update {
                    SourceChange::Update {
                        element: replacement,
                    }
                } else {
                    SourceChange::Insert {
                        element: replacement,
                    }
                })
                .await
                .unwrap_err();
            assert!(matches!(
                error,
                crate::computation::ComputationQueryError::Evaluation(
                    EvaluationError::ElementKindConflict { reference, .. }
                ) if reference == ElementReference::new("a", "one")
            ));
            assert_eq!(
                subject
                    .store
                    .elements
                    .get_element(&ElementReference::new("a", "one"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some(&initial)
            );
            assert!(subject.query.recovery_required());
            subject.query.shutdown().await.unwrap();
        }
    }
}
