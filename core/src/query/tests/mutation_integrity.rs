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
    models::{
        ChangeOrdering, Element, ElementMetadata, ElementPropertyMap, ElementReference,
        SourceChange,
    },
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
        removed_properties: Default::default(),
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
                        removed_properties: Default::default(),
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

const NOTED: &str = "MATCH (n:Item) RETURN n.key AS key, n.value AS value, n.note AS note";

fn noted(time: u64, value: i64, note: &str) -> Element {
    Element::Node {
        metadata: metadata("a", "one", time),
        properties: ElementPropertyMap::from(json!({"key":"a/one","value":value,"note":note})),
    }
}

fn patch(time: u64, properties: serde_json::Value, removed: &[&str]) -> SourceChange {
    SourceChange::update_with_removals(
        Element::Node {
            metadata: metadata("a", "one", time),
            properties: ElementPropertyMap::from(properties),
        },
        removed.iter().map(|name| name.to_string()).collect(),
    )
    .unwrap()
}

async fn stored(subject: &Subject) -> Option<Arc<Element>> {
    subject
        .store
        .elements
        .get_element(&ElementReference::new("a", "one"))
        .await
        .unwrap()
}

fn int(value: i64) -> crate::models::ElementValue {
    crate::models::ElementValue::Integer(value)
}

fn text(value: &str) -> crate::models::ElementValue {
    crate::models::ElementValue::String(Arc::from(value))
}

fn assert_row(
    row: &QueryVariables,
    value: impl Into<VariableValue>,
    note: impl Into<VariableValue>,
) {
    assert_eq!(row.get("value"), Some(&value.into()));
    assert_eq!(row.get("note"), Some(&note.into()));
}

#[tokio::test]
async fn update_removes_only_the_named_property_and_keeps_omitted_ones() {
    let mut subject = Subject::new(NOTED).await;
    subject
        .apply(SourceChange::Insert {
            element: noted(10, 1, "keep"),
        })
        .await;

    // A removal deletes the property; the omitted key and value stay.
    let changes = subject
        .apply(patch(20, json!({"value":2}), &["note"]))
        .await;
    let [QueryPartEvaluationContext::Updating { before, after, .. }] = changes.as_slice() else {
        panic!("removing a property is one row update: {changes:?}");
    };
    assert_row(before, 1, "keep");
    assert_row(after, 2, VariableValue::Null);
    let element = stored(&subject).await.unwrap();
    let properties = element.get_properties();
    assert!(!properties.contains_key("note"));
    assert_eq!(properties.get("key"), Some(&text("a/one")));
    assert_eq!(properties.get("value"), Some(&int(2)));
    assert_eq!(element.get_effective_from(), 20);

    // Omitting a property is not a removal: the earlier value is kept.
    subject
        .apply(SourceChange::Insert {
            element: noted(30, 2, "again"),
        })
        .await;
    subject.apply(patch(40, json!({"value":3}), &[])).await;
    assert_eq!(
        stored(&subject).await.unwrap().get_properties().get("note"),
        Some(&text("again"))
    );
    assert_row(subject.rows.values().next().unwrap(), 3, "again");
}

#[tokio::test]
async fn update_setting_null_stores_null_instead_of_removing_the_property() {
    let mut subject = Subject::new(NOTED).await;
    subject
        .apply(SourceChange::Insert {
            element: noted(10, 1, "keep"),
        })
        .await;
    subject.apply(patch(20, json!({"note":null}), &[])).await;
    let element = stored(&subject).await.unwrap();
    assert_eq!(
        element.get_properties().get("note"),
        Some(&crate::models::ElementValue::Null)
    );
    assert!(element.get_properties().contains_key("note"));
    assert_eq!(element.get_properties().get("value"), Some(&int(1)));
    assert_row(
        subject.rows.values().next().unwrap(),
        1,
        VariableValue::Null,
    );
}

#[tokio::test]
async fn update_without_a_base_creates_the_element_and_ignores_absent_removals() {
    let mut subject = Subject::new(NOTED).await;
    let changes = subject
        .apply(patch(10, json!({"key":"a/one","value":5}), &["note"]))
        .await;
    assert!(matches!(
        changes.as_slice(),
        [QueryPartEvaluationContext::Adding { .. }]
    ));
    let element = stored(&subject).await.unwrap();
    assert!(!element.get_properties().contains_key("note"));
    assert_eq!(element.get_properties().get("value"), Some(&int(5)));
    assert_row(
        subject.rows.values().next().unwrap(),
        5,
        VariableValue::Null,
    );
}

#[tokio::test]
async fn replace_drops_omitted_properties_and_updates_the_row_in_place() {
    let mut subject = Subject::new(NOTED).await;
    subject
        .apply(SourceChange::Insert {
            element: noted(10, 1, "gone"),
        })
        .await;
    let replacement = Element::Node {
        metadata: metadata("a", "one", 20),
        properties: ElementPropertyMap::from(json!({"key":"a/one","value":9})),
    };
    let changes = subject
        .apply(SourceChange::replace(replacement.clone()))
        .await;
    // One full replacement is one row update, not an addition or delete and insert.
    let [QueryPartEvaluationContext::Updating { before, after, .. }] = changes.as_slice() else {
        panic!("replacing an element is one row update: {changes:?}");
    };
    assert_row(before, 1, "gone");
    assert_row(after, 9, VariableValue::Null);
    assert_eq!(stored(&subject).await.as_deref(), Some(&replacement));

    // Without an existing element a replacement creates it.
    let mut fresh = Subject::new(NOTED).await;
    let changes = fresh.apply(SourceChange::replace(replacement)).await;
    assert!(matches!(
        changes.as_slice(),
        [QueryPartEvaluationContext::Adding { .. }]
    ));
}

#[tokio::test]
async fn update_setting_and_removing_one_property_fails_before_changing_state() {
    let mut subject = Subject::new(NOTED).await;
    let initial = noted(10, 1, "keep");
    subject
        .apply(SourceChange::Insert {
            element: initial.clone(),
        })
        .await;
    let error = subject
        .query
        .process_source_change(SourceChange::Update {
            element: Element::Node {
                metadata: metadata("a", "one", 20),
                properties: ElementPropertyMap::from(json!({"note":"set"})),
            },
            removed_properties: ["note".to_string()].into(),
        })
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ComputationQueryError::Evaluation(EvaluationError::InvalidSourceChange(
            crate::models::SourceChangeError::ConflictingProperty { ref property, .. }
        )) if property == "note"
    ));
    assert_eq!(stored(&subject).await.as_deref(), Some(&initial));
    subject.query.shutdown().await.unwrap();
}

struct Stamp;

#[async_trait::async_trait]
impl crate::interface::SourceMiddleware for Stamp {
    async fn process(
        &self,
        change: SourceChange,
        _: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, crate::interface::MiddlewareError> {
        fn stamped(mut element: Element) -> Element {
            if let Element::Node { properties, .. } = &mut element {
                properties.insert("tag", crate::models::ElementValue::String(Arc::from("ran")));
            }
            element
        }
        Ok(vec![match change {
            SourceChange::Insert { element } => SourceChange::Insert {
                element: stamped(element),
            },
            SourceChange::Replace { element } => SourceChange::replace(stamped(element)),
            SourceChange::Update {
                element,
                removed_properties,
            } => SourceChange::Update {
                element: stamped(element),
                removed_properties,
            },
            other => other,
        }])
    }
}

struct StampFactory;

impl crate::interface::SourceMiddlewareFactory for StampFactory {
    fn name(&self) -> String {
        "stamp".into()
    }

    fn create(
        &self,
        _: &crate::models::SourceMiddlewareConfig,
    ) -> Result<Arc<dyn crate::interface::SourceMiddleware>, crate::interface::MiddlewareSetupError>
    {
        Ok(Arc::new(Stamp))
    }
}

const TAGGED: &str =
    "MATCH (n:Item) RETURN n.key AS key, n.value AS value, n.note AS note, n.tag AS tag";
const OUTPUT: &str = "drasi.output/0123abcd";

impl Store {
    /// Pipeline `stamp` is registered only under `pipeline_key`.
    async fn query_with_pipeline(&self, pipeline_key: &str) -> ComputationQuery {
        let functions = Arc::new(FunctionRegistry::new());
        let mut registry = crate::middleware::MiddlewareTypeRegistry::new();
        registry.register(Arc::new(StampFactory));
        let builder = QueryBuilder::new(TAGGED, Arc::new(CypherParser::new(functions.clone())))
            .with_function_registry(functions)
            .with_middleware_registry(Arc::new(registry))
            .with_source_middleware(Arc::new(crate::models::SourceMiddlewareConfig {
                kind: Arc::from("stamp"),
                name: Arc::from("stamp"),
                config: Default::default(),
            }))
            .with_source_pipeline(pipeline_key, &["stamp".to_string()]);
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

async fn stamped_row(
    pipeline_key: &str,
    change: SourceChange,
    scope: Option<&str>,
) -> (Subject, Vec<QueryPartEvaluationContext>) {
    let store = Store::new();
    let query = store.query_with_pipeline(pipeline_key).await;
    let mut batch = crate::computation::QueryInputBatch::from(vec![change]);
    if let Some(scope) = scope {
        batch = batch.with_input_scope(scope);
    }
    let results = query
        .process_source_changes_with_non_atomic_result_hook(batch, |_| async { Ok(()) })
        .await
        .unwrap();
    let results = results.to_vec();
    (
        Subject {
            store,
            query,
            rows: HashMap::new(),
        },
        results,
    )
}

fn tag_of(results: &[QueryPartEvaluationContext]) -> VariableValue {
    let [QueryPartEvaluationContext::Adding { after, .. }] = results else {
        panic!("expected one added row: {results:?}");
    };
    after.get("tag").cloned().unwrap()
}

#[tokio::test]
async fn input_scope_selects_the_pipeline_without_changing_foreign_element_identity() {
    let change = insert(OUTPUT, "one", 10, 1);
    let (subject, results) = stamped_row("upstream", change, Some("upstream")).await;
    assert_eq!(tag_of(&results), VariableValue::from("ran"));

    let reference = ElementReference::new(OUTPUT, "one");
    let stored = subject
        .store
        .elements
        .get_element(&reference)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.get_reference(), &reference);
    assert_eq!(stored.get_effective_from(), 10);
    assert!(subject
        .store
        .elements
        .get_element(&ElementReference::new("upstream", "one"))
        .await
        .unwrap()
        .is_none());
    subject.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn without_input_scope_the_element_namespace_still_selects_the_pipeline() {
    let (subject, results) = stamped_row("upstream", insert("upstream", "one", 10, 1), None).await;
    assert_eq!(tag_of(&results), VariableValue::from("ran"));
    subject.query.shutdown().await.unwrap();

    // The foreign namespace has no pipeline, so a named pipeline cannot run.
    let (subject, results) = stamped_row("upstream", insert(OUTPUT, "one", 10, 1), None).await;
    assert_eq!(tag_of(&results), VariableValue::Null);
    subject.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn input_scope_without_a_pipeline_does_not_fall_back_to_the_element_namespace() {
    let (subject, results) = stamped_row(
        "upstream",
        insert("upstream", "one", 10, 1),
        Some("another-input"),
    )
    .await;
    assert_eq!(tag_of(&results), VariableValue::Null);
    subject.query.shutdown().await.unwrap();
}

#[tokio::test]
async fn input_scope_preserves_replace_removals_and_producer_sequence_ordering() {
    let store = Store::new();
    let query = store.query_with_pipeline("upstream").await;
    let run = |changes: Vec<SourceChange>, ordering| {
        crate::computation::QueryInputBatch::new(changes, ordering).with_input_scope("upstream")
    };
    let element = |time: u64, properties: serde_json::Value| Element::Node {
        metadata: metadata(OUTPUT, "one", time),
        properties: ElementPropertyMap::from(properties),
    };
    let stored = || async {
        store
            .elements
            .get_element(&ElementReference::new(OUTPUT, "one"))
            .await
            .unwrap()
            .unwrap()
    };

    let first = element(
        100,
        json!({"key":"k","value":1,"note":"keep","extra":"drop"}),
    );
    query
        .process_source_changes_with_non_atomic_result_hook(
            run(
                vec![SourceChange::Insert { element: first }],
                ChangeOrdering::EventTime,
            ),
            |_| async { Ok(()) },
        )
        .await
        .unwrap();

    // An earlier time is accepted only because the owner declared producer order.
    let patch = SourceChange::update_with_removals(
        element(50, json!({"value":2})),
        ["extra".to_string()].into(),
    )
    .unwrap();
    query
        .process_source_changes_with_non_atomic_result_hook(
            run(vec![patch], ChangeOrdering::ProducerSequence),
            |_| async { Ok(()) },
        )
        .await
        .unwrap();
    let current = stored().await;
    assert_eq!(current.get_effective_from(), 50);
    let properties = current.get_properties();
    assert_eq!(properties.get("value"), Some(&int(2)));
    assert_eq!(properties.get("note"), Some(&text("keep")));
    assert_eq!(properties.get("tag"), Some(&text("ran")));
    assert!(properties.get("extra").is_none());

    let replacement = SourceChange::replace(element(40, json!({"key":"k","value":3})));
    query
        .process_source_changes_with_non_atomic_result_hook(
            run(vec![replacement], ChangeOrdering::ProducerSequence),
            |_| async { Ok(()) },
        )
        .await
        .unwrap();
    let current = stored().await;
    let properties = current.get_properties();
    assert_eq!(current.get_effective_from(), 40);
    assert_eq!(properties.get("value"), Some(&int(3)));
    assert_eq!(properties.get("tag"), Some(&text("ran")));
    assert!(properties.get("note").is_none());

    // Event-time ordering still rejects the stale change under the same scope.
    let stale = SourceChange::Insert {
        element: element(10, json!({"key":"k","value":9})),
    };
    query
        .process_source_changes_with_non_atomic_result_hook(
            run(vec![stale], ChangeOrdering::EventTime),
            |_| async { Ok(()) },
        )
        .await
        .unwrap();
    assert_eq!(stored().await.get_effective_from(), 40);
    query.shutdown().await.unwrap();
}
