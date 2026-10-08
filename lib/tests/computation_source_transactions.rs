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

#![cfg(feature = "computation-rocksdb-tests")]

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    computation::{ComputationIndexProvider, InMemoryComputationProvider},
    interface::{
        ElementIndex, MiddlewareError, MiddlewareSetupError, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    middleware::MiddlewareTypeRegistry,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
        SourceMiddlewareConfig,
    },
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::computation::v1::*;
use std::{
    collections::VecDeque,
    num::{NonZeroU64, NonZeroUsize},
    path::Path,
    result::Result,
    sync::{Arc, Mutex},
};

fn limits(count: usize, millis: u64) -> SourceTransactionLimits {
    SourceTransactionLimits {
        max_changes: NonZeroUsize::new(count).expect("count"),
        max_bytes: NonZeroUsize::new(65536).expect("bytes"),
        max_duration_ms: NonZeroU64::new(millis).expect("deadline"),
    }
}

fn execution(count: usize) -> QueryExecutionSettings {
    QueryExecutionSettings {
        source_transactions: Some(limits(count, 5000)),
        ..Default::default()
    }
}

fn provider(path: &Path) -> Arc<dyn ComputationIndexProvider> {
    Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("memory"),
        ),
    ))
}

fn definition() -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: "source-transactions".into(),
        id: ComponentId::try_new("query").expect("query id"),
        query: "MATCH (n:Person) RETURN n.name AS name".into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("query-output").expect("stream"),
        outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
    }
}

fn change(id: &str, name: &str, update: bool, time: u64) -> SourceChange {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("source", id),
            labels: Arc::from([Arc::from("Person")]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(serde_json::json!({"name": name})),
    };
    if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    }
}

fn envelope(sequence: u64, changes: Vec<SourceChange>) -> ChangeEnvelope {
    let mut builder = SourceTransactionBuilder::new("source", limits(16, 5000)).expect("assembly");
    for change in changes {
        builder.push(change).expect("admission");
    }
    builder
        .commit(
            Bytes::copy_from_slice(&sequence.to_be_bytes()),
            Bytes::copy_from_slice(&sequence.to_be_bytes()),
        )
        .expect("commit frame")
        .into_envelope(
            StreamId::try_new("source-output").expect("stream"),
            sequence,
            chrono::DateTime::from_timestamp_millis(1000).expect("timestamp"),
        )
        .expect("envelope")
}

fn input(envelope: ChangeEnvelope) -> InputEnvelope {
    InputEnvelope {
        port: PortId::try_new("in").expect("input"),
        envelope,
    }
}

async fn query(path: &Path, count: usize) -> ContinuousQueryTransformer {
    ContinuousQueryTransformer::new_configured(
        definition(),
        provider(path),
        QueryOptions::default(),
        execution(count),
        None,
    )
    .await
    .expect("transaction query")
}

fn names(query: &ContinuousQueryTransformer) -> Vec<String> {
    let mut rows = query
        .results()
        .snapshot()
        .expect("snapshot")
        .rows
        .values()
        .map(|record| {
            let row = QueryChangeCodec::decode_row(record).expect("row");
            QueryChangeCodec::row_values_to_json(&row.values)["name"]
                .as_str()
                .expect("name")
                .to_owned()
        })
        .collect::<Vec<_>>();
    rows.sort();
    rows
}

#[tokio::test]
async fn increasing_transaction_limit_resumes_without_losing_committed_state_or_acknowledging_rejected_input(
) {
    let directory = tempfile::tempdir().expect("directory");
    let first = envelope(1, vec![change("one", "initial", false, 1000)]);
    let second = envelope(
        2,
        vec![change("one", "final", true, 1100), change("two", "second", false, 1200)],
    );
    {
        let mut query = query(directory.path(), 1).await;
        query.start().await.expect("start");
        assert_eq!(
            query
                .transform(input(first.clone()))
                .await
                .expect("first group")
                .len(),
            1
        );
        let error = query
            .transform(input(second.clone()))
            .await
            .expect_err("limit must reject");
        assert!(matches!(
            error.downcast_ref::<SourceTransactionError>(),
            Some(SourceTransactionError::Limit {
                kind: SourceTransactionLimit::Changes,
                limit: 1,
            })
        ));
        assert_eq!(
            query.results().snapshot().expect("snapshot").as_of_sequence,
            1
        );
        assert_eq!(names(&query), ["initial"]);
        query.stop().await.expect("cleanup");
    }
    {
        let mut query = query(directory.path(), 2).await;
        query
            .start()
            .await
            .expect("increase is not a semantic reset");
        assert_eq!(names(&query), ["initial"]);
        assert!(query
            .transform(input(first))
            .await
            .expect("already committed")
            .is_empty());
        let output = query
            .transform(input(second.clone()))
            .await
            .expect("retry complete group");
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].envelope.changes().operations().len(), 2);
        assert!(output[0]
            .envelope
            .annotations()
            .entries()
            .any(|entry| entry.key() == "drasi.source-transaction.v1"));
        assert_eq!(names(&query), ["final", "second"]);
        assert!(query
            .transform(input(second))
            .await
            .expect("whole-group duplicate")
            .is_empty());
        query.stop().await.expect("cleanup");
    }
    let mut ordinary = ContinuousQueryTransformer::new(definition(), provider(directory.path()))
        .await
        .expect("ordinary construction");
    assert!(matches!(
        ordinary
            .start()
            .await
            .expect_err("group mode cannot silently change")
            .downcast_ref::<QueryRecoveryError>(),
        Some(QueryRecoveryError::ConfigurationChanged)
    ));
    ordinary.stop().await.expect("cleanup");
}

#[tokio::test]
async fn transaction_mode_rejects_non_atomic_resources_and_unframed_input() {
    assert!(ContinuousQueryTransformer::new_configured(
        definition(),
        Arc::new(InMemoryComputationProvider),
        QueryOptions::default(),
        execution(2),
        None,
    )
    .await
    .is_err());
    let directory = tempfile::tempdir().expect("directory");
    assert!(ContinuousQueryTransformer::new_configured(
        definition(),
        provider(directory.path()),
        QueryOptions {
            publication: QueryPublicationMode::NonAtomic,
            ..Default::default()
        },
        execution(2),
        None,
    )
    .await
    .is_err());
    let mut query = query(directory.path(), 2).await;
    assert_ne!(query.descriptor(), &definition().descriptor());
    assert_eq!(
        query.descriptor(),
        &definition().descriptor_with_execution(&execution(2))
    );
    query.start().await.expect("start");
    let raw = GraphChangeCodec::encode_changes(
        &[change("one", "one", false, 1000), change("two", "two", false, 1100)],
        StreamId::try_new("source-output").expect("stream"),
        1,
        None,
    )
    .expect("ordinary batch");
    assert!(
        query.transform(input(raw)).await.is_err(),
        "a vector is not proof of a source commit"
    );
    assert!(names(&query).is_empty());
    query.stop().await.expect("cleanup");
}

struct WaitFactory(Arc<tokio::sync::Notify>);
struct Wait(Arc<tokio::sync::Notify>);

impl SourceMiddlewareFactory for WaitFactory {
    fn name(&self) -> String {
        "wait".into()
    }
    fn create(
        &self,
        _: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        Ok(Arc::new(Wait(self.0.clone())))
    }
}

#[async_trait]
impl SourceMiddleware for Wait {
    async fn process(
        &self,
        change: SourceChange,
        _: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        self.0.notified().await;
        Ok(vec![change])
    }
}

#[tokio::test]
async fn transaction_deadline_fences_cancelled_evaluation_and_retry_uses_the_whole_group() {
    let directory = tempfile::tempdir().expect("directory");
    let gate = Arc::new(tokio::sync::Notify::new());
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(WaitFactory(gate.clone())));
    let registry = Arc::new(registry);
    let config = drasi_lib::Query::cypher("query")
        .query(definition().query)
        .from_source_with_pipeline("source", vec!["waiting".into()])
        .with_middleware(SourceMiddlewareConfig::new(
            "wait",
            "waiting",
            Default::default(),
        ))
        .build();
    let mut settings = QueryExecutionSettings::from_legacy_config(&config);
    settings.source_transactions = Some(limits(1, 25));
    let event = envelope(1, vec![change("one", "one", false, 1000)]);
    {
        let mut query = ContinuousQueryTransformer::new_configured(
            definition(),
            provider(directory.path()),
            QueryOptions::default(),
            settings.clone(),
            Some(registry.clone()),
        )
        .await
        .expect("query");
        query.start().await.expect("start");
        let error = query
            .transform(input(event.clone()))
            .await
            .expect_err("middleware deadline");
        assert!(matches!(
            error.downcast_ref::<SourceTransactionError>(),
            Some(SourceTransactionError::Deadline)
        ));
        assert!(names(&query).is_empty());
        assert!(
            query.transform(input(event.clone())).await.is_err(),
            "failed owner must not be reused"
        );
        query
            .stop()
            .await
            .expect("await cancelled storage ownership");
    }
    settings.source_transactions = Some(limits(1, 5000));
    let mut query = ContinuousQueryTransformer::new_configured(
        definition(),
        provider(directory.path()),
        QueryOptions::default(),
        settings,
        Some(registry),
    )
    .await
    .expect("replacement");
    query
        .start()
        .await
        .expect("recover unchanged semantic configuration");
    gate.notify_one();
    assert_eq!(
        query
            .transform(input(event))
            .await
            .expect("whole-group retry")
            .len(),
        1
    );
    assert_eq!(names(&query), ["one"]);
    query.stop().await.expect("cleanup");
}

#[tokio::test]
async fn transaction_replay_advances_transport_without_reapplying_committed_logical_input() {
    let directory = tempfile::tempdir().unwrap();
    let progress = Arc::new(
        QuerySourceProgress::new(
            "source-transactions",
            ComponentId::try_new("query").unwrap(),
        )
        .unwrap(),
    );
    let replay = |transport| {
        let mut assembly = SourceTransactionBuilder::new("source", limits(2, 5000)).unwrap();
        assembly.push(change("one", "final", false, 1000)).unwrap();
        assembly
            .commit(
                Bytes::from_static(b"transaction"),
                Bytes::from_static(b"position"),
            )
            .unwrap()
            .into_replay_envelope(
                StreamId::try_new("source-output").unwrap(),
                42,
                transport,
                chrono::DateTime::from_timestamp_millis(1000).unwrap(),
            )
            .unwrap()
    };
    {
        let mut query = query(directory.path(), 2)
            .await
            .with_source_progress(progress.clone())
            .unwrap();
        query.start().await.unwrap();
        assert_eq!(query.transform(input(replay(1))).await.unwrap().len(), 1);
        assert!(query.transform(input(replay(2))).await.unwrap().is_empty());
        assert_eq!(names(&query), ["final"]);
        let snapshot = progress.snapshot();
        assert_eq!(
            snapshot.checkpoints[&SourceProgressKey::Source("source".into())].sequence,
            42
        );
        assert_eq!(
            snapshot.transport_sequences[&StreamId::try_new("source-output").unwrap()],
            2
        );
        query.stop().await.unwrap();
    }
    let mut query = query(directory.path(), 2)
        .await
        .with_source_progress(progress.clone())
        .unwrap();
    query.start().await.unwrap();
    assert!(query.transform(input(replay(3))).await.unwrap().is_empty());
    assert_eq!(names(&query), ["final"]);
    assert_eq!(
        progress.snapshot().transport_sequences[&StreamId::try_new("source-output").unwrap()],
        3
    );
    query.stop().await.unwrap();
}

struct Source {
    descriptor: ComponentDescriptor,
    events: VecDeque<ChangeEnvelope>,
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
        Ok(self.events.pop_front().map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").expect("output"),
            envelope,
        }))
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    results: Arc<Mutex<Vec<ChangeEnvelope>>>,
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
        self.results.lock().expect("results").push(input.envelope);
        Ok(())
    }
}

fn endpoint(id: &str, port: &str) -> Endpoint {
    Endpoint::new(
        ComponentId::try_new(id).expect("component"),
        PortId::try_new(port).expect("port"),
    )
}

#[tokio::test]
async fn actual_graph_delivers_one_final_update_through_typed_transaction_and_result_pipes() {
    for factory_created in [false, true] {
        let directory = tempfile::tempdir().expect("directory");
        let results = Arc::new(Mutex::new(Vec::new()));
        let source = Source {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("source").unwrap(),
                vec![PortDescriptor::new(
                    PortId::try_new("out").unwrap(),
                    PortDirection::Output,
                    SourceTransactionCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .unwrap(),
            events: VecDeque::from([envelope(
                1,
                vec![
                    change("one", "first", false, 1000),
                    change("one", "middle", true, 1100),
                    change("one", "last", true, 1200),
                ],
            )]),
        };
        let sink = Sink {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("sink").unwrap(),
                vec![PortDescriptor::new(
                    PortId::try_new("in").unwrap(),
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .unwrap(),
            results: results.clone(),
        };
        let graph = ComputationGraph::builder("source-transactions")
            .source(Box::new(source))
            .sink(Box::new(sink));
        let graph = if factory_created {
            let factory = Arc::new(ContinuousQueryFactory::default());
            let indexes = ResourceId::try_new("indexes").unwrap();
            let mut specification = ComponentSpecification {
                descriptor: definition().descriptor_with_execution(&execution(3)),
                role: ComponentRole::Query,
                completion: None,
                implementation: factory.descriptor().implementation.clone(),
                configuration_version: 1,
                configuration: std::collections::BTreeMap::from([
                    (
                        Arc::from("query"),
                        ConfigurationValue::Literal(definition().query.into()),
                    ),
                    (
                        Arc::from("stream"),
                        ConfigurationValue::Literal("query-output".into()),
                    ),
                    (
                        Arc::from("execution"),
                        ConfigurationValue::Literal(serde_json::to_value(execution(3)).unwrap()),
                    ),
                ]),
                dependencies: std::collections::BTreeMap::from([(
                    Arc::from("indexes"),
                    vec![indexes.clone()],
                )]),
            };
            factory.validate(&specification).unwrap();
            specification.descriptor = definition().descriptor();
            assert!(
                factory.validate(&specification).is_err(),
                "grouping cannot bind an ordinary source port"
            );
            specification.descriptor = definition().descriptor_with_execution(&execution(3));
            graph
                .declare_resource(ResourceSpecification {
                    id: indexes.clone(),
                    role: ResourceRole::IndexBackend,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from("indexes"),
                })
                .unwrap()
                .provide_resource(
                    indexes,
                    ResourceHandle::new(
                        ResourceRole::IndexBackend,
                        Arc::new(QueryIndexProviderResource(provider(directory.path()))),
                    ),
                )
                .unwrap()
                .component(specification, factory)
        } else {
            graph.query(Box::new(query(directory.path(), 3).await))
        };
        let mut graph = graph
            .bind_stream(
                endpoint("source", "out"),
                StreamId::try_new("source-output").unwrap(),
            )
            .bind_stream(
                endpoint("query", "out"),
                StreamId::try_new("query-output").unwrap(),
            )
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("query", "out"), endpoint("sink", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), graph.start().unwrap())
            .await
            .unwrap()
            .unwrap();
        graph.shutdown().await.unwrap();
        let outputs = results.lock().unwrap();
        assert_eq!(outputs.len(), 1);
        let [ChangeOperation::Added { after, .. }] = outputs[0].changes().operations() else {
            panic!("one final addition");
        };
        let row = QueryChangeCodec::decode_row(after).unwrap();
        assert_eq!(
            QueryChangeCodec::row_values_to_json(&row.values),
            serde_json::json!({"name": "last"})
        );
    }
}
