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

#![cfg(test)]

use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{
    computation::v1::*,
    wal::{WalError, WalProvider, WriteAheadLogConfig},
};
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

#[derive(Default)]
struct Wal {
    rows: Mutex<BTreeMap<u64, SourceChange>>,
    deletes: AtomicUsize,
    prunes: AtomicUsize,
}

#[async_trait]
impl WalProvider for Wal {
    async fn register(&self, _: &str, _: WriteAheadLogConfig) -> std::result::Result<(), WalError> {
        Ok(())
    }
    async fn append(&self, _: &str, event: &SourceChange) -> std::result::Result<u64, WalError> {
        let mut rows = self.rows.lock().expect("rows");
        let sequence = rows.keys().next_back().copied().unwrap_or(0) + 1;
        rows.insert(sequence, event.clone());
        Ok(sequence)
    }
    async fn read_from(
        &self,
        source: &str,
        sequence: u64,
    ) -> std::result::Result<Vec<(u64, SourceChange)>, WalError> {
        let rows = self.rows.lock().expect("rows");
        if rows.keys().next().is_some_and(|oldest| sequence < *oldest) {
            return Err(WalError::PositionUnavailable {
                source_id: source.into(),
                requested: sequence,
                oldest_available: rows.keys().next().copied(),
            });
        }
        Ok(rows
            .range(sequence..)
            .map(|(sequence, change)| (*sequence, change.clone()))
            .collect())
    }
    async fn prune_up_to(&self, _: &str, sequence: u64) -> std::result::Result<u64, WalError> {
        self.prunes.fetch_add(1, Ordering::SeqCst);
        let mut rows = self.rows.lock().expect("rows");
        let before = rows.len();
        rows.retain(|seq, _| *seq > sequence);
        Ok((before - rows.len()) as u64)
    }
    async fn head_sequence(&self, _: &str) -> std::result::Result<u64, WalError> {
        Ok(self
            .rows
            .lock()
            .expect("rows")
            .keys()
            .next_back()
            .copied()
            .unwrap_or(0))
    }
    async fn oldest_sequence(&self, _: &str) -> std::result::Result<Option<u64>, WalError> {
        Ok(self.rows.lock().expect("rows").keys().next().copied())
    }
    async fn event_count(&self, _: &str) -> std::result::Result<u64, WalError> {
        Ok(self.rows.lock().expect("rows").len() as u64)
    }
    async fn delete_wal(&self, _: &str) -> std::result::Result<(), WalError> {
        self.deletes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn event(id: u64) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", &id.to_string()),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: id,
            },
            properties: ElementPropertyMap::new(),
        },
    }
}

#[tokio::test]
async fn wal_source_resumes_in_order_then_observes_live_appends_without_owning_partition_cleanup() {
    let wal = Arc::new(Wal::default());
    for index in 1..=3 {
        wal.append("source", &event(index)).await.expect("append");
    }
    let resource = Arc::new(WalSourceResource {
        provider: wal.clone(),
        partition: "source".into(),
    });
    let mut source = WalReplaySource::new(
        ComponentId::try_new("replay").expect("id"),
        StreamId::try_new("replay/out").expect("stream"),
        resource,
        1,
    );
    source.start().await.expect("resume after one");
    for sequence in [2, 3] {
        let output = source.next().await.expect("replay").expect("event");
        assert_eq!(
            GraphChangeCodec::source_metadata(&output.envelope)
                .expect("metadata")
                .expect("source")
                .sequence,
            Some(sequence)
        );
        assert_eq!(
            output.envelope.system().sequence(),
            sequence - 1,
            "adapter sequence is separate"
        );
    }
    wal.append("source", &event(4)).await.expect("live append");
    let live = source.next().await.expect("live").expect("event");
    assert_eq!(
        GraphChangeCodec::source_metadata(&live.envelope)
            .expect("metadata")
            .expect("source")
            .sequence,
        Some(4)
    );
    source.stop().await.expect("stop only adapter");
    assert_eq!(wal.deletes.load(Ordering::SeqCst), 0);
    source.start().await.expect("explicit replay restart");
    let replayed = source.next().await.expect("replay").expect("event");
    assert!(replayed.envelope.system().sequence() > live.envelope.system().sequence());
    assert_eq!(
        GraphChangeCodec::source_metadata(&replayed.envelope)
            .expect("metadata")
            .expect("source")
            .sequence,
        Some(2)
    );
    source.stop().await.expect("stop");
}

#[tokio::test]
async fn unavailable_wal_position_is_reported_instead_of_silently_skipped() {
    let wal = Arc::new(Wal::default());
    for index in 1..=3 {
        wal.append("source", &event(index)).await.expect("append");
    }
    wal.prune_up_to("source", 2).await.expect("prune");
    let mut source = WalReplaySource::new(
        ComponentId::try_new("replay").expect("id"),
        StreamId::try_new("replay/out").expect("stream"),
        Arc::new(WalSourceResource {
            provider: wal,
            partition: "source".into(),
        }),
        0,
    );
    assert!(source
        .start()
        .await
        .expect_err("strict replay")
        .downcast_ref::<WalError>()
        .is_some());
}

#[cfg(feature = "computation-rocksdb-tests")]
mod persistent_progress {
    use super::*;
    use drasi_core::computation::ComputationIndexProvider;
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    use std::{num::NonZeroUsize, time::Duration};
    use tokio::sync::mpsc;

    struct Sink {
        descriptor: ComponentDescriptor,
        received: mpsc::Sender<ChangeEnvelope>,
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
            self.received.send(input.envelope).await?;
            Ok(())
        }
    }

    fn query_definition() -> ContinuousQueryDefinition {
        ContinuousQueryDefinition {
            graph_id: "source-progress".into(),
            id: ComponentId::try_new("query").expect("id"),
            query: "MATCH (n:Item) RETURN n AS item".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("query/out").expect("stream"),
            outbox_capacity: NonZeroUsize::new(8).expect("capacity"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wal_factory_binds_the_concrete_consumer_set_to_two_actual_query_owners(
    ) -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let indexes: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
            directory.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        ));
        let wal = Arc::new(Wal::default());
        for sequence in 1..=2 {
            wal.append("source", &event(sequence)).await?;
        }
        let endpoint = |name: &str, port: &str| {
            Endpoint::new(
                ComponentId::try_new(name).expect("component"),
                PortId::try_new(port).expect("port"),
            )
        };
        let (sent, mut received) = mpsc::channel(8);
        let mut builder = ComputationGraph::builder("source-progress");
        let mut owners = Vec::new();
        let mut results = Vec::new();
        for name in ["left", "right"] {
            let mut definition = query_definition();
            definition.id = ComponentId::try_new(name)?;
            definition.output_stream = StreamId::try_new(format!("{name}/out"))?;
            let owner = Arc::new(QuerySourceProgress::new(
                "source-progress",
                definition.id.clone(),
            )?);
            let query = ContinuousQueryTransformer::new(definition, indexes.clone())
                .await?
                .with_source_progress(owner.clone())?;
            results.push(query.results());
            owners.push(owner);
            let sink = format!("{name}-sink");
            builder = builder
                .query(Box::new(query))
                .sink(Box::new(Sink {
                    descriptor: ComponentDescriptor::try_new(
                        ComponentId::try_new(sink.as_str())?,
                        vec![PortDescriptor::new(
                            PortId::try_new("in")?,
                            PortDirection::Input,
                            QueryChangeCodec::schema().descriptor().clone(),
                            PipeRequirements::default(),
                        )],
                    )?,
                    received: sent.clone(),
                }))
                .bind_stream(
                    endpoint(name, "out"),
                    StreamId::try_new(format!("{name}/out"))?,
                )
                .connect(
                    EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                )
                .connect(
                    EdgeDefinition::new(endpoint(name, "out"), endpoint(&sink, "in")),
                    Box::new(BoundedPipeConfig { capacity: 1 }),
                );
        }
        let consumers = Arc::new(ConsumerProgressSet::new(
            "source-progress",
            ComponentId::try_new("source")?,
            owners.clone(),
        )?);
        let factory = Arc::new(WalReplaySourceFactory::default());
        let specification = ComponentSpecification {
            descriptor: WalReplaySource::new(
                ComponentId::try_new("source")?,
                StreamId::try_new("source/out")?,
                Arc::new(WalSourceResource {
                    provider: wal.clone(),
                    partition: "source".into(),
                }),
                0,
            )
            .descriptor()
            .clone(),
            role: ComponentRole::Source,
            completion: None,
            implementation: factory.descriptor().implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([(
                Arc::from("stream"),
                ConfigurationValue::Literal("source/out".into()),
            )]),
            dependencies: BTreeMap::from([
                (Arc::from("wal"), vec![ResourceId::try_new("wal")?]),
                (
                    Arc::from("consumer_progress"),
                    vec![ResourceId::try_new("consumers")?],
                ),
            ]),
        };
        let mut conflicting = specification.clone();
        conflicting.dependencies.insert(
            Arc::from("source_progress"),
            vec![ResourceId::try_new("single")?],
        );
        assert!(factory.validate(&conflicting).is_err());
        for (name, role, handle) in [
            (
                "wal",
                ResourceRole::Wal,
                ResourceHandle::new(
                    ResourceRole::Wal,
                    Arc::new(WalSourceResource {
                        provider: wal.clone(),
                        partition: "source".into(),
                    }),
                ),
            ),
            (
                "consumers",
                ResourceRole::Checkpoint,
                ResourceHandle::new(ResourceRole::Checkpoint, consumers),
            ),
        ] {
            let id = ResourceId::try_new(name)?;
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: id.clone(),
                    role,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from(name),
                })?
                .provide_resource(id, handle)?;
        }
        let mut graph = builder
            .component(specification, factory)
            .bind_stream(endpoint("source", "out"), StreamId::try_new("source/out")?)
            .build()?;
        assert!(owners.iter().all(|owner| !owner.snapshot().ready));
        let run = graph.start()?;
        let control = run.control();
        let (execution, observed) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(run, async {
                let collected = async {
                    let mut by_query: BTreeMap<String, Vec<u64>> = BTreeMap::new();
                    for _ in 0..4 {
                        let output = received
                            .recv()
                            .await
                            .ok_or_else(|| anyhow::anyhow!("query output closed"))?;
                        by_query
                            .entry(QueryChangeCodec::metadata(&output)?.query_id)
                            .or_default()
                            .push(QueryChangeCodec::query_sequence(&output)?);
                    }
                    anyhow::ensure!(
                        by_query
                            == BTreeMap::from([
                                ("left".into(), vec![1, 2]),
                                ("right".into(), vec![1, 2]),
                            ]),
                        "WAL fan-out differed: {by_query:?}"
                    );
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                control.cancel();
                collected
            })
        })
        .await?;
        observed?;
        assert!(matches!(execution, Err(GraphError::Cancelled)));
        for (owner, results) in owners.iter().zip(results) {
            assert_eq!(
                owner.snapshot().checkpoints[&SourceProgressKey::Source("source".into())].sequence,
                2
            );
            assert_eq!(results.snapshot()?.rows.len(), 2);
        }
        assert_eq!(wal.deletes.load(Ordering::SeqCst), 0);
        assert_eq!(wal.prunes.load(Ordering::SeqCst), 0);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn wal_fanout_resumes_from_the_slowest_actual_owner_without_repeating_fast_owner_effects()
    {
        let directory = tempfile::tempdir().expect("indexes");
        let indexes: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
            directory.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
            ),
        ));
        let wal = Arc::new(Wal::default());
        for sequence in 1..=3 {
            wal.append("source", &event(sequence))
                .await
                .expect("append");
        }
        let resource = Arc::new(WalSourceResource {
            provider: wal.clone(),
            partition: "source".into(),
        });
        let source_id = ComponentId::try_new("source").expect("source");
        let output_stream = StreamId::try_new("source/out").expect("stream");
        let mut queries = Vec::new();
        let mut owners = Vec::new();
        for name in ["left", "right"] {
            let mut definition = query_definition();
            definition.id = ComponentId::try_new(name).expect("consumer");
            definition.output_stream = StreamId::try_new(format!("{name}/out")).expect("output");
            let owner = Arc::new(
                QuerySourceProgress::new("source-progress", definition.id.clone()).expect("owner"),
            );
            let mut query = ContinuousQueryTransformer::new(definition, indexes.clone())
                .await
                .expect("query")
                .with_source_progress(owner.clone())
                .expect("binding");
            query.start().await.expect("start query");
            queries.push(query);
            owners.push(owner);
        }
        let mut seed = WalReplaySource::new(
            source_id.clone(),
            output_stream.clone(),
            resource.clone(),
            0,
        );
        seed.start().await.expect("seed source");
        let first = seed.next().await.expect("first").expect("event");
        let second = seed.next().await.expect("second").expect("event");
        for query in &mut queries {
            query
                .transform(InputEnvelope {
                    port: PortId::try_new("in").expect("port"),
                    envelope: first.envelope.clone(),
                })
                .await
                .expect("first handled");
        }
        queries[0]
            .transform(InputEnvelope {
                port: PortId::try_new("in").expect("port"),
                envelope: second.envelope,
            })
            .await
            .expect("fast consumer handles second");
        seed.stop().await.expect("stop seed");
        let consumers = Arc::new(
            ConsumerProgressSet::new("source-progress", source_id.clone(), owners.clone())
                .expect("actual consumers"),
        );
        let mut invalid = WalReplaySource::new(
            source_id.clone(),
            output_stream.clone(),
            resource.clone(),
            2,
        )
        .with_consumer_progress(consumers.clone())
        .expect("binding");
        assert!(
            invalid.start().await.is_err(),
            "configured resume must not skip the slow owner"
        );
        invalid.stop().await.expect("cleanup rejected start");
        let mut source = WalReplaySource::new(source_id, output_stream.clone(), resource, 0)
            .with_consumer_progress(consumers)
            .expect("multi-owner binding");
        source.start().await.expect("resume all owners");
        let replay = source.next().await.expect("replay").expect("second event");
        assert_eq!(
            GraphChangeCodec::source_metadata(&replay.envelope)
                .expect("metadata")
                .expect("source")
                .sequence,
            Some(2),
        );
        assert_eq!(replay.envelope.system().sequence(), 3);
        let input = InputEnvelope {
            port: PortId::try_new("in").expect("port"),
            envelope: replay.envelope,
        };
        assert!(queries[0]
            .transform(input.clone())
            .await
            .expect("fast duplicate")
            .is_empty());
        assert!(!queries[1]
            .transform(input)
            .await
            .expect("slow catch-up")
            .is_empty());
        let third = source.next().await.expect("next").expect("third event");
        assert_eq!(third.envelope.system().sequence(), 4);
        for query in &mut queries {
            assert!(!query
                .transform(InputEnvelope {
                    port: PortId::try_new("in").expect("port"),
                    envelope: third.envelope.clone(),
                })
                .await
                .expect("third handled")
                .is_empty());
            assert_eq!(query.results().snapshot().expect("snapshot").rows.len(), 3);
        }
        for owner in &owners {
            assert_eq!(
                owner.snapshot().checkpoints[&SourceProgressKey::Source("source".into())].sequence,
                3,
            );
            assert_eq!(owner.snapshot().transport_sequences[&output_stream], 4);
        }
        source.stop().await.expect("stop source");
        for query in &mut queries {
            query.stop().await.expect("stop query");
        }
        assert_eq!(wal.deletes.load(Ordering::SeqCst), 0);
        assert_eq!(wal.prunes.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn wal_resume_waits_for_and_uses_real_committed_query_progress_across_reconstruction() {
        let temp = tempfile::tempdir().expect("temp");
        let indexes: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
            temp.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
            ),
        ));
        let progress = Arc::new(
            QuerySourceProgress::new(
                "source-progress",
                ComponentId::try_new("query").expect("id"),
            )
            .expect("scope"),
        );
        let wal = Arc::new(Wal::default());
        wal.append("source", &event(1)).await.expect("first");
        let wal_resource = Arc::new(WalSourceResource {
            provider: wal.clone(),
            partition: "source".into(),
        });
        let source_factory = Arc::new(WalReplaySourceFactory::default());
        let query_factory = Arc::new(ContinuousQueryFactory::default());
        let endpoint = |node: &str, port: &str| {
            Endpoint::new(
                ComponentId::try_new(node).expect("id"),
                PortId::try_new(port).expect("port"),
            )
        };
        let resource_id = |id: &str| ResourceId::try_new(id).expect("resource");
        let literal = |value: &str| ConfigurationValue::Literal(value.into());
        let (received, mut output) = mpsc::channel(8);
        let source = ComponentSpecification {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("source").expect("id"),
                vec![PortDescriptor::new(
                    PortId::try_new("out").expect("port"),
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("source"),
            role: ComponentRole::Source,
            completion: None,
            implementation: source_factory.descriptor().implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([(Arc::from("stream"), literal("source/out"))]),
            dependencies: BTreeMap::from([
                (Arc::from("wal"), vec![resource_id("wal")]),
                (Arc::from("source_progress"), vec![resource_id("progress")]),
            ]),
        };
        let query = ComponentSpecification {
            descriptor: query_definition().descriptor(),
            role: ComponentRole::Query,
            completion: None,
            implementation: query_factory.descriptor().implementation.clone(),
            configuration_version: 1,
            configuration: BTreeMap::from([
                (Arc::from("stream"), literal("query/out")),
                (Arc::from("query"), literal(&query_definition().query)),
            ]),
            dependencies: BTreeMap::from([
                (Arc::from("indexes"), vec![resource_id("indexes")]),
                (Arc::from("source_progress"), vec![resource_id("progress")]),
            ]),
        };
        let sink = Sink {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("sink").expect("id"),
                vec![PortDescriptor::new(
                    PortId::try_new("in").expect("port"),
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("sink"),
            received,
        };
        let mut builder = ComputationGraph::builder("source-progress")
            .component(source, source_factory)
            .component(query, query_factory)
            .sink(Box::new(sink))
            .bind_stream(
                endpoint("source", "out"),
                StreamId::try_new("source/out").expect("stream"),
            )
            .bind_stream(
                endpoint("query", "out"),
                StreamId::try_new("query/out").expect("stream"),
            )
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("query", "out"), endpoint("sink", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
        for (id, role, handle) in [
            (
                "wal",
                ResourceRole::Wal,
                ResourceHandle::new(ResourceRole::Wal, wal_resource.clone()),
            ),
            (
                "indexes",
                ResourceRole::IndexBackend,
                ResourceHandle::new(
                    ResourceRole::IndexBackend,
                    Arc::new(QueryIndexProviderResource(indexes.clone())),
                ),
            ),
            (
                "progress",
                ResourceRole::Checkpoint,
                ResourceHandle::new(
                    ResourceRole::Checkpoint,
                    Arc::new(QuerySourceProgressResource(progress.clone())),
                ),
            ),
        ] {
            builder = builder
                .declare_resource(ResourceSpecification {
                    id: resource_id(id),
                    role,
                    ownership: ResourceOwnership::Borrowed,
                    binding: Arc::from(id),
                })
                .expect("declare")
                .provide_resource(resource_id(id), handle)
                .expect("provide");
        }
        let mut graph = builder.build().expect("preflight");
        assert!(
            !progress.snapshot().ready,
            "construction is not activation or acknowledgement"
        );
        let run = graph.start().expect("scope");
        let control = run.control();
        let (result, ()) = tokio::time::timeout(Duration::from_secs(15), async {
            tokio::join!(run, async {
                let startup = control.startup_report().await.expect("startup");
                assert_eq!(startup.summary, OperationSummary::Completed, "{startup:?}");
                assert_eq!(
                    output
                        .recv()
                        .await
                        .expect("first output")
                        .system()
                        .sequence(),
                    1
                );
                let key = SourceProgressKey::Source("source".into());
                assert_eq!(progress.snapshot().checkpoints[&key].sequence, 1);
                assert_eq!(
                    progress.snapshot().checkpoints[&key]
                        .source_position
                        .as_deref(),
                    Some(&1u64.to_be_bytes()[..])
                );
                wal.append("source", &event(2)).await.expect("live append");
                assert_eq!(
                    output
                        .recv()
                        .await
                        .expect("live output")
                        .system()
                        .sequence(),
                    2
                );
                assert_eq!(progress.snapshot().checkpoints[&key].sequence, 2);
                control.cancel();
            })
        })
        .await
        .expect("progress handoff deadlocked");
        assert!(matches!(result, Err(GraphError::Cancelled)));
        drop(graph);
        assert!(!progress.snapshot().ready);

        let mut query = ContinuousQueryTransformer::new(query_definition(), indexes)
            .await
            .expect("reconstruct query")
            .with_source_progress(progress.clone())
            .expect("same owner");
        let mut source = WalReplaySource::new(
            ComponentId::try_new("source").expect("id"),
            StreamId::try_new("source/out").expect("stream"),
            wal_resource,
            0,
        )
        .with_source_progress(progress.clone());
        let (source_started, query_started) = tokio::join!(source.start(), query.start());
        source_started.expect("source waits for durable query reconciliation");
        query_started.expect("query recovery");
        wal.append("source", &event(3)).await.expect("next append");
        let next = source.next().await.expect("resume").expect("event");
        assert_eq!(
            GraphChangeCodec::source_metadata(&next.envelope)
                .expect("metadata")
                .expect("raw")
                .sequence,
            Some(3)
        );
        assert_eq!(
            next.envelope.system().sequence(),
            3,
            "new adapter continues the committed transport sequence instead of reusing it"
        );
        let output = query
            .transform(InputEnvelope {
                port: PortId::try_new("in").expect("port"),
                envelope: next.envelope,
            })
            .await
            .expect("process");
        assert_eq!(output[0].envelope.system().sequence(), 3);
        source.stop().await.expect("stop source");
        query.stop().await.expect("stop query");
        assert_eq!(wal.deletes.load(Ordering::SeqCst), 0);
    }
}
