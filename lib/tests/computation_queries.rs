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

#![cfg(feature = "computation")]

use std::{
    collections::VecDeque,
    num::NonZeroUsize,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use drasi_core::{
    computation::{ComputationIndexProvider, InMemoryComputationProvider},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::{channels::ResultDiff, computation::v1::*};

struct Source {
    descriptor: ComponentDescriptor,
    events: VecDeque<OutputEnvelope>,
}
#[async_trait]
impl ComputationComponent for Source {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for Source {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.events.pop_front())
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    events: Arc<Mutex<Vec<ChangeEnvelope>>>,
}
#[async_trait]
impl ComputationComponent for Sink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.events.lock().expect("results").push(input.envelope);
        Ok(())
    }
}

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("id")
}
fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("port")
}
fn stream(value: &str) -> StreamId {
    StreamId::try_new(value).expect("stream")
}
fn endpoint(node: &str, name: &str) -> Endpoint {
    Endpoint::new(id(node), port(name))
}

fn person(name: &str, update: bool, time: u64) -> SourceChange {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("people", "one"),
            labels: Arc::from([Arc::from("Person")]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(serde_json::json!({ "name": name })),
    };
    if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    }
}

fn definition(query: &str) -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: "query-graph".into(),
        id: id("query"),
        query: query.into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: stream("query/out"),
        outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
    }
}

async fn run_query(
    definition: ContinuousQueryDefinition,
    provider: Arc<dyn ComputationIndexProvider>,
    changes: Vec<SourceChange>,
) -> (QueryResults, Vec<ChangeEnvelope>) {
    let query = ContinuousQueryTransformer::new(definition, provider)
        .await
        .expect("construct graph query");
    let results = query.results();
    let events = Arc::new(Mutex::new(Vec::new()));
    let source = Source {
        descriptor: ComponentDescriptor::try_new(
            id("source"),
            vec![PortDescriptor::new(
                port("out"),
                PortDirection::Output,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )
        .expect("source"),
        events: changes
            .into_iter()
            .enumerate()
            .map(|(index, change)| OutputEnvelope {
                port: port("out"),
                envelope: GraphChangeCodec::encode_change(
                    change,
                    stream("source/out"),
                    index as u64 + 1,
                    None,
                )
                .expect("graph change"),
            })
            .collect(),
    };
    let sink = Sink {
        descriptor: ComponentDescriptor::try_new(
            id("sink"),
            vec![PortDescriptor::new(
                port("in"),
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )
        .expect("sink"),
        events: events.clone(),
    };
    let mut graph = ComputationGraph::builder("query-graph")
        .source(Box::new(source))
        .query(Box::new(query))
        .sink(Box::new(sink))
        .bind_stream(endpoint("source", "out"), stream("source/out"))
        .bind_stream(endpoint("query", "out"), stream("query/out"))
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("query", "out"), endpoint("sink", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    tokio::time::timeout(Duration::from_secs(15), graph.start().expect("scope"))
        .await
        .expect("query graph deadlocked")
        .expect("query processing");
    let collected = events.lock().expect("events").clone();
    drop(graph);
    (results, collected)
}

#[tokio::test(flavor = "current_thread")]
async fn real_cypher_query_runs_inside_computation_graph_with_typed_add_update_delete() {
    let removed = SourceChange::Delete {
        metadata: ElementMetadata {
            reference: ElementReference::new("people", "one"),
            labels: Arc::from([Arc::from("Person")]),
            effective_from: 3000,
        },
    };
    let (results, output) = run_query(
        definition("MATCH (n:Person) RETURN n.name AS name"),
        Arc::new(InMemoryComputationProvider),
        vec![person("Alice", false, 1000), person("Alicia", true, 2000), removed],
    )
    .await;
    assert_eq!(output.len(), 3);
    let first = QueryChangeCodec::to_legacy_result(&output[0]).expect("projection");
    assert!(matches!(&first.results[0], ResultDiff::Add { data, .. } if data["name"] == "Alice"));
    assert!(matches!(
        QueryChangeCodec::to_legacy_result(&output[1])
            .expect("projection")
            .results[0],
        ResultDiff::Update { .. }
    ));
    assert!(matches!(
        QueryChangeCodec::to_legacy_result(&output[2])
            .expect("projection")
            .results[0],
        ResultDiff::Delete { .. }
    ));
    assert_eq!(results.snapshot().expect("snapshot").as_of_sequence, 3);
    assert!(results.snapshot().expect("snapshot").rows.is_empty());
    assert_eq!(results.replay(0).expect("retained output").len(), 3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn future_work_runs_after_source_eof_and_preserves_original_source_provenance() {
    let (results, output) = run_query(
        definition("MATCH (n:Person) WHERE drasi.trueLater(true, 2000) RETURN n.name AS name"),
        Arc::new(InMemoryComputationProvider),
        vec![person("Alice", false, 1000)],
    )
    .await;
    assert_eq!(output.len(), 1);
    assert_eq!(
        QueryChangeCodec::metadata(&output[0])
            .expect("metadata")
            .source_id
            .as_deref(),
        Some("people")
    );
    assert_eq!(
        output[0]
            .system()
            .timestamp()
            .expect("future time")
            .timestamp_millis(),
        2000
    );
    assert!(output[0].lineage().is_some());
    assert_eq!(results.snapshot().expect("snapshot").as_of_sequence, 1);
}

#[cfg(feature = "computation-rocksdb-tests")]
#[tokio::test]
async fn graph_query_atomically_persists_and_recovers_typed_output_without_legacy_managers() {
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    let temp = tempfile::tempdir().expect("temp");
    let provider: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
        temp.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
        ),
    ));
    let definition = definition("MATCH (n:Person) RETURN n.name AS name");
    let (_, output) = run_query(
        definition.clone(),
        provider.clone(),
        vec![person("Alice", false, 1000)],
    )
    .await;
    assert_eq!(output.len(), 1);
    let mut query = ContinuousQueryTransformer::new(definition, provider)
        .await
        .expect("reopen query");
    query.start().await.expect("reconcile committed bundle");
    assert_eq!(
        query.results().snapshot().expect("snapshot").as_of_sequence,
        1
    );
    assert_eq!(query.results().snapshot().expect("snapshot").rows.len(), 1);
    assert_eq!(query.results().replay(0).expect("retained output").len(), 1);
    let duplicate = GraphChangeCodec::encode_change(
        person("Alice", false, 1000),
        stream("source/out"),
        1,
        None,
    )
    .expect("input");
    assert!(query
        .transform(InputEnvelope {
            port: port("in"),
            envelope: duplicate
        })
        .await
        .expect("checkpoint dedup")
        .is_empty());
    query.stop().await.expect("stop healthy query");
}

#[tokio::test]
async fn gql_and_multi_change_envelopes_use_the_same_graph_owned_query_path() {
    let mut gql = definition("MATCH (n:Person) RETURN n.name AS name");
    gql.language = ComputationQueryLanguage::Gql;
    let (_, output) = run_query(
        gql,
        Arc::new(InMemoryComputationProvider),
        vec![person("Alice", false, 1000)],
    )
    .await;
    assert_eq!(output.len(), 1);
    assert!(
        matches!(&QueryChangeCodec::to_legacy_result(&output[0]).expect("row").results[0],
        ResultDiff::Add { data, .. } if data["name"] == "Alice")
    );
}

#[cfg(feature = "computation-rocksdb-tests")]
mod atomic_failure {
    use super::*;
    use drasi_core::{
        computation::{ComputationIndexes, ComputationResource, TransactionDomain},
        interface::{IndexError, IndexSet, OutboxWriter},
    };
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    use std::result::Result;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct FailingOutbox {
        inner: Arc<dyn OutboxWriter>,
        fail: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl OutboxWriter for FailingOutbox {
        async fn append(&self, query: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err(IndexError::CorruptedData);
            }
            self.inner.append(query, sequence, data).await
        }
        async fn read_from(
            &self,
            query: &str,
            after: u64,
        ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
            self.inner.read_from(query, after).await
        }
        async fn read_latest_sequence(&self, query: &str) -> Result<Option<u64>, IndexError> {
            self.inner.read_latest_sequence(query).await
        }
        async fn clear(&self, query: &str) -> Result<(), IndexError> {
            self.inner.clear(query).await
        }
        async fn trim_to_capacity(
            &self,
            query: &str,
            capacity: usize,
        ) -> Result<usize, IndexError> {
            self.inner.trim_to_capacity(query, capacity).await
        }
    }

    struct Provider {
        inner: Arc<dyn ComputationIndexProvider>,
        fail: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
    }
    #[async_trait]
    impl ComputationIndexProvider for Provider {
        async fn create_indexes(
            &self,
            graph: &str,
            query: &str,
        ) -> Result<ComputationIndexes, IndexError> {
            let original = self.inner.create_indexes(graph, query).await?;
            let set = original.indexes();
            let domain = TransactionDomain::new(set.session_control.clone());
            let resources = ComputationIndexes::try_new(
                IndexSet {
                    element_index: set.element_index.clone(),
                    archive_index: set.archive_index.clone(),
                    result_index: set.result_index.clone(),
                    future_queue: set.future_queue.clone(),
                    session_control: set.session_control.clone(),
                },
                Some(domain.clone()),
                Some(ComputationResource::participating(
                    original.checkpoint_store().expect("checkpoint").clone(),
                    &domain,
                )),
                Some(ComputationResource::participating(
                    Arc::new(FailingOutbox {
                        inner: original.outbox_writer().expect("outbox").clone(),
                        fail: self.fail.clone(),
                        calls: self.calls.clone(),
                    }),
                    &domain,
                )),
                Some(ComputationResource::participating(
                    original.live_results_writer().expect("live").clone(),
                    &domain,
                )),
            )
            .map_err(IndexError::other)?;
            Ok(resources.with_cleanup(original.cleanup().expect("cleanup").clone()))
        }
        fn is_volatile(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn query_output_failure_rolls_back_and_fences_before_any_later_input_can_mutate_indexes()
    {
        let temp = tempfile::tempdir().expect("temp");
        let base: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
            temp.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
            ),
        ));
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(Provider {
            inner: base.clone(),
            fail: Arc::new(AtomicBool::new(true)),
            calls: calls.clone(),
        });
        let definition = definition("MATCH (n:Person) RETURN n.name AS name");
        let mut query = ContinuousQueryTransformer::new(definition.clone(), provider)
            .await
            .expect("construct");
        let results = query.results();
        query.start().await.expect("start");
        let input = || InputEnvelope {
            port: port("in"),
            envelope: GraphChangeCodec::encode_change(
                person("Alice", false, 1000),
                stream("source/out"),
                1,
                None,
            )
            .expect("input"),
        };
        assert!(query.transform(input()).await.is_err());
        assert!(
            query.transform(input()).await.is_err(),
            "failure ingress fence rejects later input"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(results.snapshot().expect("snapshot").as_of_sequence, 0);
        assert!(results.snapshot().expect("snapshot").rows.is_empty());
        query.stop().await.expect("await failed query cleanup");
        drop(query);
        let mut recovered = ContinuousQueryTransformer::new(definition, base)
            .await
            .expect("reopen");
        recovered.start().await.expect("recover rollback");
        let output = recovered
            .transform(input())
            .await
            .expect("input checkpoint did not advance on failed output");
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].envelope.system().sequence(), 1);
        recovered.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn non_atomic_publication_failure_keeps_a_visible_durable_pending_fence() {
        let temp = tempfile::tempdir().expect("temp");
        let base: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
            temp.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
            ),
        ));
        let provider = Arc::new(Provider {
            inner: base.clone(),
            fail: Arc::new(AtomicBool::new(true)),
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let options = QueryOptions {
            recovery: QueryRecoveryPolicy::Strict,
            publication: QueryPublicationMode::NonAtomic,
        };
        let definition = definition("MATCH (n:Person) RETURN n.name AS name");
        {
            let mut query =
                ContinuousQueryTransformer::new_with_options(definition.clone(), provider, options)
                    .await
                    .expect("construct");
            query.start().await.expect("start");
            assert!(query
                .transform(InputEnvelope {
                    port: port("in"),
                    envelope: GraphChangeCodec::encode_change(
                        person("Alice", false, 1000),
                        stream("source/out"),
                        1,
                        None
                    )
                    .expect("input"),
                })
                .await
                .is_err());
            query.stop().await.expect("stop failed publication");
        }
        let mut reopened = ContinuousQueryTransformer::new_with_options(definition, base, options)
            .await
            .expect("reopen");
        let error = reopened
            .start()
            .await
            .expect_err("incomplete publication cannot silently resume");
        assert!(matches!(
            error.downcast_ref::<QueryRecoveryError>(),
            Some(QueryRecoveryError::PendingPublication)
        ));
        reopened.stop().await.expect("cleanup");
    }
}
