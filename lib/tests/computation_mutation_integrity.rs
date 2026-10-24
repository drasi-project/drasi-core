// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

#![cfg(test)]
#![cfg(feature = "computation-rocksdb-tests")]

use std::{collections::BTreeMap, num::NonZeroUsize, sync::Arc, time::Duration};

use drasi_core::{
    computation::{ComputationIndexProvider, ComputationQuery, ComputationQueryError},
    evaluation::{
        functions::{aggregation::ValueAccumulator, FunctionRegistry},
        variable_value::VariableValue,
        EvaluationError,
    },
    interface::{IndexError, ResultKey, ResultOwner},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
    query::QueryBuilder,
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::computation::v1::*;
use drasi_query_cypher::CypherParser;
use serde_json::json;

const GRAPH: &str = "mutation-integrity";
const QUERY: &str = "query";
const TEXT: &str = "MATCH (n:Item) RETURN n.key AS key, n.value AS value";

fn provider(path: &std::path::Path) -> Arc<dyn ComputationIndexProvider> {
    Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).unwrap(),
        ),
    ))
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

fn input(sequence: u64, change: SourceChange) -> InputEnvelope {
    InputEnvelope {
        port: PortId::try_new("in").unwrap(),
        envelope: GraphChangeCodec::encode_change(change, stream(), sequence, None).unwrap(),
    }
}

fn stream() -> StreamId {
    StreamId::try_new("source/out").unwrap()
}

fn definition() -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: GRAPH.into(),
        id: ComponentId::try_new(QUERY).unwrap(),
        query: TEXT.into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("query/out").unwrap(),
        outbox_capacity: NonZeroUsize::new(512).unwrap(),
    }
}

async fn open(
    provider: Arc<dyn ComputationIndexProvider>,
) -> (ContinuousQueryTransformer, Arc<QuerySourceProgress>) {
    let progress =
        Arc::new(QuerySourceProgress::new(GRAPH, ComponentId::try_new(QUERY).unwrap()).unwrap());
    let mut query = ContinuousQueryTransformer::new(definition(), provider)
        .await
        .unwrap()
        .with_source_progress(progress.clone())
        .unwrap();
    query.start().await.unwrap();
    (query, progress)
}

fn rows(query: &ContinuousQueryTransformer) -> BTreeMap<String, VariableValue> {
    query
        .results()
        .snapshot()
        .unwrap()
        .rows
        .values()
        .map(|record| {
            let row = QueryChangeCodec::decode_row(record).unwrap();
            let VariableValue::String(key) = row.values.get("key").unwrap() else {
                panic!("unexpected key in {:?}", row.values);
            };
            (key.clone(), row.values.get("value").unwrap().clone())
        })
        .collect()
}

async fn deliver(query: &mut ContinuousQueryTransformer, event: InputEnvelope) {
    let output = query.transform(event).await.unwrap();
    query.delivery_completed(&output).await.unwrap();
    while query.has_pending_emissions() {
        let output = query.continue_transform().await.unwrap();
        query.delivery_completed(&output).await.unwrap();
    }
}

async fn persisted_model() {
    let temp = tempfile::tempdir().unwrap();
    let provider = provider(temp.path());
    let mut model: BTreeMap<String, (u64, Option<i64>)> = BTreeMap::new();
    let mut sequence = 0;
    let mut random = 0x5316_2635_u64;
    let mut last_input: Option<InputEnvelope> = None;
    for cycle in 0..6 {
        let (mut query, progress) = open(provider.clone()).await;
        let expected = |model: &BTreeMap<String, (u64, Option<i64>)>| {
            model
                .iter()
                .filter_map(|(key, (_, value))| {
                    value.map(|value| (key.clone(), VariableValue::from(json!(value))))
                })
                .collect::<BTreeMap<_, _>>()
        };
        assert_eq!(rows(&query), expected(&model), "reconstruction {cycle}");
        if sequence != 0 {
            assert_eq!(
                progress
                    .snapshot()
                    .checkpoints
                    .get(&SourceProgressKey::Stream(stream()))
                    .map(|checkpoint| checkpoint.sequence),
                Some(sequence)
            );
        }
        if let Some(last) = &last_input {
            let before = query.results().snapshot().unwrap().as_of_sequence;
            deliver(&mut query, last.clone()).await;
            assert_eq!(query.results().snapshot().unwrap().as_of_sequence, before);
        }
        for _ in 0..64 {
            random = random.wrapping_mul(6364136223846793005).wrapping_add(1);
            let source = if random & 1 == 0 { "a" } else { "b" };
            let id = format!("{}", (random >> 8) % 7);
            let key = format!("{source}/{id}");
            let time = 1 + cycle * 40 + ((random >> 16) % 80);
            let value = ((random >> 24) % 100) as i64;
            let delete = (random >> 32) % 5 == 0;
            let change = if delete {
                SourceChange::Delete {
                    metadata: metadata(source, &id, time),
                }
            } else if random & 2 == 0 {
                SourceChange::Insert {
                    element: node(source, &id, time, value),
                }
            } else {
                SourceChange::Update {
                    element: node(source, &id, time, value),
                }
            };
            if model.get(&key).map_or(true, |(latest, _)| time >= *latest) {
                model.insert(key, (time, (!delete).then_some(value)));
            }
            sequence += 1;
            let event = input(sequence, change);
            deliver(&mut query, event.clone()).await;
            last_input = Some(event);
            assert_eq!(rows(&query), expected(&model), "input {sequence}");
            assert_eq!(
                progress
                    .snapshot()
                    .checkpoints
                    .get(&SourceProgressKey::Stream(stream()))
                    .map(|checkpoint| checkpoint.sequence),
                Some(sequence),
                "even a skipped stale change must commit its input progress"
            );
        }
        query.stop().await.unwrap();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn native_persistent_mutations_match_independent_model_current_thread() {
    tokio::time::timeout(Duration::from_secs(60), persisted_model())
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_persistent_mutations_match_independent_model_multi_thread() {
    tokio::time::timeout(Duration::from_secs(60), persisted_model())
        .await
        .unwrap();
}

async fn core_query(provider: &Arc<dyn ComputationIndexProvider>) -> ComputationQuery {
    let functions = Arc::new(FunctionRegistry::new());
    ComputationQuery::try_build(
        QueryBuilder::new(TEXT, Arc::new(CypherParser::new(functions.clone())))
            .with_function_registry(functions),
        provider.create_indexes(GRAPH, QUERY).await.unwrap(),
    )
    .await
    .unwrap()
}

async fn assert_element(query: &ComputationQuery, value: Option<i64>, deleted_at: Option<u64>) {
    let reference = ElementReference::new("a", "one");
    let mut stored = None;
    let mut tombstone = None;
    query
        .resource_transaction(|| async {
            stored = query
                .resources()
                .indexes()
                .element_index
                .get_element(&reference)
                .await?;
            tombstone = query
                .resources()
                .indexes()
                .result_index
                .get(
                    &ResultKey::Element(reference.clone()),
                    &ResultOwner::ElementTombstone,
                )
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    match (stored, value) {
        (Some(element), Some(value)) => {
            assert_eq!(element.get_property("value"), &(&json!(value)).into())
        }
        (None, None) => {}
        (stored, value) => panic!("unexpected element {stored:?}, expected {value:?}"),
    }
    match (tombstone, deleted_at) {
        (Some(ValueAccumulator::TimeMarker { timestamp }), Some(expected)) => {
            assert_eq!(timestamp, expected)
        }
        (None, None) => {}
        (stored, expected) => panic!("unexpected tombstone {stored:?}, expected {expected:?}"),
    }
}

#[tokio::test]
async fn deletion_and_recreation_frontiers_share_the_real_index_transaction() {
    let temp = tempfile::tempdir().unwrap();
    let provider = provider(temp.path());
    {
        let query = core_query(&provider).await;
        query
            .process_source_change(SourceChange::Insert {
                element: node("a", "one", 10, 1),
            })
            .await
            .unwrap();
        let transaction = query.resources().atomic_result_transaction().unwrap();
        let failure = query
            .process_source_change_with_result_hook(
                SourceChange::Delete {
                    metadata: metadata("a", "one", 30),
                },
                &transaction,
                |_| async { Err(IndexError::IOError) },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            failure,
            ComputationQueryError::Index(IndexError::IOError)
        ));
        query.shutdown().await.unwrap();
    }
    {
        let query = core_query(&provider).await;
        assert_element(&query, Some(1), None).await;
        query
            .process_source_change(SourceChange::Update {
                element: node("a", "one", 20, 2),
            })
            .await
            .unwrap();
        query
            .process_source_change(SourceChange::Delete {
                metadata: metadata("a", "one", 30),
            })
            .await
            .unwrap();
        let transaction = query.resources().atomic_result_transaction().unwrap();
        let failure = query
            .process_source_change_with_result_hook(
                SourceChange::Insert {
                    element: node("a", "one", 40, 4),
                },
                &transaction,
                |_| async { Err(IndexError::IOError) },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            failure,
            ComputationQueryError::Index(IndexError::IOError)
        ));
        query.shutdown().await.unwrap();
    }
    let query = core_query(&provider).await;
    assert_element(&query, None, Some(30)).await;
    assert!(query
        .process_source_change(SourceChange::Insert {
            element: node("a", "one", 20, 2)
        })
        .await
        .unwrap()
        .is_empty());
    query
        .process_source_change(SourceChange::Insert {
            element: node("a", "one", 50, 5),
        })
        .await
        .unwrap();
    assert_element(&query, Some(5), None).await;
    query.shutdown().await.unwrap();
}

#[tokio::test]
async fn cancelled_delete_rolls_back_the_frontier_and_cross_kind_batch_rolls_back_every_change() {
    let temp = tempfile::tempdir().unwrap();
    let provider = provider(temp.path());
    {
        let query = core_query(&provider).await;
        query
            .process_source_change(SourceChange::Insert {
                element: node("a", "one", 10, 1),
            })
            .await
            .unwrap();
        let transaction = query.resources().atomic_result_transaction().unwrap();
        let entered = tokio::sync::Notify::new();
        {
            let work = query.process_source_change_with_result_hook(
                SourceChange::Delete {
                    metadata: metadata("a", "one", 30),
                },
                &transaction,
                |_| async {
                    entered.notify_one();
                    std::future::pending::<std::result::Result<(), IndexError>>().await
                },
            );
            tokio::pin!(work);
            tokio::select! {
                result = &mut work => panic!("gated commit completed: {result:?}"),
                _ = entered.notified() => {}
            }
        }
        assert!(query.recovery_required());
        query.shutdown().await.unwrap();
    }
    {
        let query = core_query(&provider).await;
        assert_element(&query, Some(1), None).await;
        let transaction = query.resources().atomic_result_transaction().unwrap();
        let failure = query
            .process_source_changes_with_result_hook(
                vec![
                    SourceChange::Update {
                        element: node("a", "one", 20, 2),
                    },
                    SourceChange::Insert {
                        element: Element::Relation {
                            metadata: metadata("a", "one", 21),
                            in_node: ElementReference::new("a", "left"),
                            out_node: ElementReference::new("a", "right"),
                            properties: ElementPropertyMap::new(),
                        },
                    },
                ],
                &transaction,
                |_| async { panic!("invalid batch must not reach the output hook") },
            )
            .await
            .unwrap_err();
        assert!(matches!(
            failure,
            ComputationQueryError::Evaluation(EvaluationError::ElementKindConflict { .. })
        ));
        query.shutdown().await.unwrap();
    }
    let query = core_query(&provider).await;
    assert_element(&query, Some(1), None).await;
    query
        .process_source_change(SourceChange::Update {
            element: node("a", "one", 15, 15),
        })
        .await
        .unwrap();
    assert_element(&query, Some(15), None).await;
    query.shutdown().await.unwrap();
}

#[tokio::test]
async fn prior_mutation_semantics_are_rejected_without_resetting_persistent_state() {
    use bytes::Bytes;
    use drasi_core::computation::ComputationTransaction;
    use std::num::NonZeroU64;
    const CONFIGURATION: &str = "\0computation:query-configuration:v1";
    for grouped in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let provider = provider(temp.path());
        let limits = SourceTransactionLimits {
            max_changes: NonZeroUsize::new(4).unwrap(),
            max_bytes: NonZeroUsize::new(65536).unwrap(),
            max_duration_ms: NonZeroU64::new(5000).unwrap(),
        };
        let execution = QueryExecutionSettings {
            source_transactions: grouped.then_some(limits),
            ..Default::default()
        };
        let create = || {
            ContinuousQueryTransformer::new_configured(
                definition(),
                provider.clone(),
                QueryOptions::default(),
                execution.clone(),
                None,
            )
        };
        let mut query = create().await.unwrap();
        query.start().await.unwrap();
        let change = SourceChange::Insert {
            element: node("a", "one", 10, 7),
        };
        let event = if grouped {
            let mut batch = SourceTransactionBuilder::new("a", limits).unwrap();
            batch.push(change).unwrap();
            InputEnvelope {
                port: PortId::try_new("in").unwrap(),
                envelope: batch
                    .commit(
                        Bytes::from_static(b"transaction"),
                        Bytes::from_static(b"position"),
                    )
                    .unwrap()
                    .into_envelope(
                        stream(),
                        1,
                        chrono::DateTime::from_timestamp_millis(10).unwrap(),
                    )
                    .unwrap(),
            }
        } else {
            input(1, change)
        };
        deliver(&mut query, event).await;
        let expected = rows(&query);
        assert_eq!(expected.len(), 1);
        query.stop().await.unwrap();
        drop(query);

        let storage = provider.create_indexes(GRAPH, QUERY).await.unwrap();
        let checkpoints = storage.checkpoint_store().unwrap().clone();
        let owner = ComputationTransaction::try_new(storage).unwrap();
        let current = checkpoints
            .read_checkpoint(CONFIGURATION)
            .await
            .unwrap()
            .unwrap()
            .source_position
            .unwrap();
        let mut prior: Vec<serde_json::Value> = serde_json::from_slice(&current).unwrap();
        assert_eq!(prior[0], json!(if grouped { 6 } else { 5 }));
        assert_eq!(
            prior.pop().unwrap(),
            json!(drasi_core::query::SOURCE_CHANGE_SEMANTICS_VERSION)
        );
        prior[0] = json!(if grouped { 4 } else { 3 });
        let prior = Bytes::from(serde_json::to_vec(&prior).unwrap());
        owner
            .run(async {
                checkpoints
                    .stage_checkpoint(CONFIGURATION, 1, Some(&prior))
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        owner.shutdown().await.unwrap();
        drop((owner, checkpoints));

        let mut incompatible = create().await.unwrap();
        let error = incompatible.start().await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<QueryRecoveryError>(),
            Some(QueryRecoveryError::ConfigurationChanged)
        ));
        incompatible.stop().await.unwrap();
        drop(incompatible);
        let storage = provider.create_indexes(GRAPH, QUERY).await.unwrap();
        let checkpoints = storage.checkpoint_store().unwrap().clone();
        let owner = ComputationTransaction::try_new(storage).unwrap();
        assert_eq!(
            checkpoints
                .read_checkpoint(CONFIGURATION)
                .await
                .unwrap()
                .unwrap()
                .source_position,
            Some(prior)
        );
        owner
            .run(async {
                checkpoints
                    .stage_checkpoint(CONFIGURATION, 1, Some(&current))
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        owner.shutdown().await.unwrap();
        drop((owner, checkpoints));
        let mut reopened = create().await.unwrap();
        reopened.start().await.unwrap();
        assert_eq!(
            rows(&reopened),
            expected,
            "rejected upgrade cannot discard existing rows"
        );
        reopened.stop().await.unwrap();
    }
}
