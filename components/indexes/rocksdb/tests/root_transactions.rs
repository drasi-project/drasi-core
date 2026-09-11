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

use drasi_core::interface::SessionError;

use std::sync::Arc;

use drasi_core::{
    evaluation::{
        context::QueryPartEvaluationContext,
        functions::{FunctionRegistry, RegisterAggregationFunctions},
        EvaluationError,
    },
    index_cache::{
        cached_element_index::CachedElementIndex, cached_result_index::CachedResultIndex,
    },
    interface::{
        CheckpointStore, ElementIndex, IndexError, OutboxWriter, RootOutcome, SessionControl,
        SessionGuard,
    },
    models::{Element, ElementMetadata, ElementReference, SourceChange},
    query::QueryBuilder,
};
use drasi_index_rocksdb::{
    element_index::RocksDbElementIndex, future_queue::RocksDbFutureQueue, open_unified_db,
    result_index::RocksDbResultIndex, RocksDbCheckpointStore, RocksDbMemoryBudget,
    RocksDbOutboxWriter, RocksDbSessionControl, RocksDbSessionState, RocksIndexOptions,
};
use drasi_query_cypher::CypherParser;
use tokio::sync::Notify;

fn metadata(id: &str, label: &str) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new("source", id),
        labels: Arc::new([Arc::from(label)]),
        effective_from: 1,
    }
}

fn node(id: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: metadata(id, "Node"),
            properties: Default::default(),
        },
    }
}

fn edge(id: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: metadata(id, "R"),
            in_node: ElementReference::new("source", "a"),
            out_node: ElementReference::new("source", "b"),
            properties: Default::default(),
        },
    }
}

fn count(results: &[QueryPartEvaluationContext]) -> i64 {
    results
        .iter()
        .filter_map(|result| match result {
            QueryPartEvaluationContext::Aggregation { after, .. } => {
                after.get("n").and_then(|value| value.as_i64())
            }
            _ => None,
        })
        .last()
        .expect("count result")
}

#[tokio::test]
async fn cached_query_outer_rollback_retry_counts_both_edges() {
    for repetition in ["", "*1"] {
        let directory = tempfile::tempdir().unwrap();
        let options = RocksIndexOptions::new(false, false, RocksDbMemoryBudget::default());
        let db = open_unified_db(directory.path().to_str().unwrap(), "root", &options).unwrap();
        let state = Arc::new(RocksDbSessionState::new(db.clone()));
        let control: Arc<dyn SessionControl> = Arc::new(RocksDbSessionControl::new(state.clone()));
        let raw = Arc::new(RocksDbElementIndex::new(
            db.clone(),
            options.clone(),
            state.clone(),
        ));
        let cached = Arc::new(
            CachedElementIndex::new_with_session(raw.clone(), 16, control.clone()).unwrap(),
        );
        let results = Arc::new(
            CachedResultIndex::new_with_session(
                Arc::new(RocksDbResultIndex::new(
                    db.clone(),
                    state.clone(),
                    options.clone(),
                )),
                16,
                control.clone(),
            )
            .unwrap(),
        );
        let queue = Arc::new(RocksDbFutureQueue::new(db.clone(), state.clone(), options));
        let checkpoints = Arc::new(RocksDbCheckpointStore::new(db.clone(), state.clone()));
        let outbox = Arc::new(RocksDbOutboxWriter::new_with_session(db, state));
        let registry = Arc::new(FunctionRegistry::new());
        registry.register_aggregation_functions();
        let parser = Arc::new(CypherParser::new(registry.clone()));
        let text = format!("MATCH (a)-[:R{repetition}]->(b) RETURN count(b) AS n");
        let query = Arc::new(
            QueryBuilder::new(&text, parser)
                .with_function_registry(registry)
                .with_element_index(cached.clone())
                .with_result_index(results)
                .with_future_queue(queue)
                .with_session_control(control.clone())
                .try_build()
                .await
                .unwrap(),
        );
        query.process_source_change(node("a")).await.unwrap();
        query.process_source_change(node("b")).await.unwrap();

        let mut outer = SessionGuard::begin(control.clone()).await.unwrap();
        let first = query
            .process_source_change_in_with_hook(&mut outer, edge("ab"), |results| {
                assert_eq!(count(results), 1);
                let checkpoints = checkpoints.clone();
                let outbox = outbox.clone();
                async move {
                    checkpoints.stage_checkpoint("source", 1, None).await?;
                    outbox.append("root", 1, b"first").await
                }
            })
            .await
            .unwrap();
        let second = query
            .process_source_change_in_with_hook(&mut outer, edge("ab2"), |results| {
                assert_eq!(count(results), 2);
                async { Ok(()) }
            })
            .await
            .unwrap();
        assert_eq!(first.root_id(), second.root_id());
        assert!(matches!(
            query.process_source_change(node("unrelated")).await,
            Err(EvaluationError::IndexError(error)) if error.session_error() == Some(SessionError::SessionBusy)
        ));
        let root = outer.root();
        drop(outer);
        assert_eq!(root.outcome(), RootOutcome::RolledBack);
        query.check_health().unwrap();

        let inspection = SessionGuard::begin(control.clone()).await.unwrap();
        for id in ["ab", "ab2"] {
            let reference = ElementReference::new("source", id);
            assert!(raw.get_element(&reference).await.unwrap().is_none());
            assert!(cached.get_element(&reference).await.unwrap().is_none());
        }
        assert!(checkpoints
            .read_checkpoint("source")
            .await
            .unwrap()
            .is_none());
        assert!(outbox.read_from("root", 0).await.unwrap().is_empty());
        inspection.commit().await.unwrap();
        assert_eq!(
            count(&query.process_source_change(edge("ab")).await.unwrap()),
            1
        );
        assert_eq!(
            count(&query.process_source_change(edge("ab2")).await.unwrap()),
            2
        );

        let entered = Arc::new(Notify::new());
        let worker_entered = entered.clone();
        let worker_query = query.clone();
        let worker_control = control.clone();
        let worker = tokio::spawn(async move {
            let mut outer = SessionGuard::begin(worker_control).await.unwrap();
            worker_query
                .process_source_change_in_with_hook(&mut outer, edge("cancelled"), move |results| {
                    assert_eq!(count(results), 3);
                    async move {
                        worker_entered.notify_one();
                        std::future::pending::<Result<(), IndexError>>().await
                    }
                })
                .await
        });
        entered.notified().await;
        worker.abort();
        assert!(worker.await.unwrap_err().is_cancelled());
        query.check_health().unwrap();
        assert_eq!(
            count(
                &query
                    .process_source_change(edge("cancelled"))
                    .await
                    .unwrap()
            ),
            3
        );
    }
}
