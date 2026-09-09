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

use async_trait::async_trait;
use drasi_core::{
    computation::{ComputationIndexProvider, InMemoryComputationProvider},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_lib::computation::v1::*;
use std::{
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

struct Bootstrap {
    calls: Arc<AtomicUsize>,
    fail: bool,
}

struct InvalidWatermarks {
    duplicate: bool,
}

#[async_trait]
impl ComputationBootstrapProvider for InvalidWatermarks {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        let watermark = || BootstrapWatermark {
            stream: StreamId::try_new("source/out").expect("stream"),
            source_id: None,
            sequence: 1,
            position: if self.duplicate {
                None
            } else {
                Some(bytes::Bytes::from(vec![
                    0;
                    drasi_lib::sources::SourceBase::MAX_SOURCE_POSITION_BYTES
                        + 1
                ]))
            },
        };
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::once(async {
                panic!("invalid watermark must be rejected before reading snapshot changes")
            })),
            watermarks: if self.duplicate {
                vec![watermark(), watermark()]
            } else {
                vec![watermark()]
            },
        })
    }
}

#[tokio::test]
async fn invalid_bootstrap_watermarks_fail_before_reading_or_publishing_snapshot() {
    for duplicate in [false, true] {
        let mut query = ContinuousQueryTransformer::new(
            definition("MATCH (n:Person) RETURN n.name AS name"),
            Arc::new(InMemoryComputationProvider),
        )
        .await
        .expect("construct")
        .with_bootstrap(Arc::new(InvalidWatermarks { duplicate }));
        let error = query.start().await.expect_err("invalid watermark");
        assert!(error.to_string().contains(if duplicate {
            "duplicate"
        } else {
            "checkpoint limit"
        }));
        assert!(query
            .results()
            .snapshot()
            .expect("snapshot")
            .rows
            .is_empty());
        query.stop().await.expect("cleanup");
    }
}

fn envelope(sequence: u64, name: &str, update: bool) -> ChangeEnvelope {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("people", "one"),
            labels: Arc::from([Arc::from("Person")]),
            effective_from: 1000 + sequence,
        },
        properties: ElementPropertyMap::from(serde_json::json!({ "name": name })),
    };
    GraphChangeCodec::encode_change(
        if update {
            SourceChange::Update { element }
        } else {
            SourceChange::Insert { element }
        },
        StreamId::try_new("source/out").expect("stream"),
        sequence,
        None,
    )
    .expect("graph envelope")
}

#[async_trait]
impl ComputationBootstrapProvider for Bootstrap {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut changes = vec![Ok(envelope(1, "Alice", false))];
        if self.fail {
            changes.push(Err(anyhow::anyhow!("interrupted snapshot")));
        }
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter(changes)),
            watermarks: vec![BootstrapWatermark {
                stream: StreamId::try_new("source/out").expect("stream"),
                source_id: None,
                sequence: 1,
                position: None,
            }],
        })
    }
}

fn definition(query: &str) -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: "recovery".into(),
        id: ComponentId::try_new("query").expect("id"),
        query: query.into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("query/out").expect("stream"),
        outbox_capacity: NonZeroUsize::new(2).expect("capacity"),
    }
}

async fn query(
    provider: Arc<dyn ComputationIndexProvider>,
    text: &str,
    recovery: QueryRecoveryPolicy,
    fail: bool,
    calls: Arc<AtomicUsize>,
) -> ContinuousQueryTransformer {
    ContinuousQueryTransformer::new_with_options(
        definition(text),
        provider,
        QueryOptions {
            recovery,
            publication: QueryPublicationMode::Atomic,
        },
    )
    .await
    .expect("construct")
    .with_bootstrap(Arc::new(Bootstrap { calls, fail }))
}

#[tokio::test]
async fn bootstrap_is_separate_from_creation_and_filters_covered_live_input() {
    let calls = Arc::new(AtomicUsize::new(0));
    let mut query = query(
        Arc::new(InMemoryComputationProvider),
        "MATCH (n:Person) RETURN n.name AS name",
        QueryRecoveryPolicy::Strict,
        false,
        calls.clone(),
    )
    .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "construction does not run bootstrap"
    );
    query.start().await.expect("bootstrap");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(query.results().snapshot().expect("snapshot").rows.len(), 1);
    assert_eq!(
        query.results().snapshot().expect("snapshot").as_of_sequence,
        0
    );
    assert!(query
        .transform(InputEnvelope {
            port: PortId::try_new("in").expect("port"),
            envelope: envelope(1, "Alice", false)
        })
        .await
        .expect("covered live event")
        .is_empty());
    let output = query
        .transform(InputEnvelope {
            port: PortId::try_new("in").expect("port"),
            envelope: envelope(2, "Alicia", true),
        })
        .await
        .expect("new live event");
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].envelope.system().sequence(), 1);
    query.stop().await.expect("stop");
    query.start().await.expect("healthy restart");
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "retained healthy instance is not re-bootstrapped"
    );
    query.stop().await.expect("stop");
}

#[derive(Default)]
struct ConsumerState {
    rows: std::sync::Mutex<std::collections::HashMap<RecordId, Record>>,
    snapshots: AtomicUsize,
    fail_snapshot: std::sync::atomic::AtomicBool,
}

struct RecoverySink {
    descriptor: ComponentDescriptor,
    state: Arc<ConsumerState>,
}

impl RecoverySink {
    fn new(state: Arc<ConsumerState>) -> Self {
        Self {
            descriptor: ComponentDescriptor::try_new(
                ComponentId::try_new("sink").expect("id"),
                vec![PortDescriptor::new(
                    PortId::try_new("in").expect("port"),
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .expect("descriptor"),
            state,
        }
    }
    fn apply(&self, input: &ChangeEnvelope, reset: bool) {
        let mut rows = self.state.rows.lock().expect("rows");
        if reset {
            rows.clear();
        }
        for operation in input.changes().operations() {
            match operation {
                ChangeOperation::Added { after, .. } | ChangeOperation::Updated { after, .. } => {
                    rows.insert(after.identity().clone(), after.clone());
                }
                ChangeOperation::Deleted { identity, .. } => {
                    rows.remove(identity.identity());
                }
            }
        }
    }
}

#[async_trait]
impl ComputationComponent for RecoverySink {
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
impl EnvelopeSink for RecoverySink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    fn supports_snapshot(&self) -> bool {
        true
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.apply(&input.envelope, false);
        Ok(())
    }
    async fn replace_snapshot(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        if self.state.fail_snapshot.swap(false, Ordering::SeqCst) {
            anyhow::bail!("snapshot handler failed");
        }
        self.apply(&input.envelope, true);
        self.state.snapshots.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn replay(
    results: QueryResults,
    progress: Arc<dyn ConsumerProgressStore>,
    policy: ConsumerRecoveryPolicy,
) -> QueryReplayTransformer {
    QueryReplayTransformer::new(
        ComponentId::try_new("replay").expect("id"),
        "query".into(),
        StreamId::try_new("replay/out").expect("stream"),
        results,
        progress,
        policy,
    )
}

#[tokio::test]
async fn failed_consumer_snapshot_does_not_seed_checkpoint_and_replay_handoff_is_lossless() {
    let mut query = query(
        Arc::new(InMemoryComputationProvider),
        "MATCH (n:Person) RETURN n.name AS name",
        QueryRecoveryPolicy::Strict,
        false,
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    query.start().await.expect("bootstrap query");
    let progress = Arc::new(MemoryConsumerProgress::default());
    let state = Arc::new(ConsumerState::default());
    state.fail_snapshot.store(true, Ordering::SeqCst);
    let mut sink =
        CheckpointedSink::new(Box::new(RecoverySink::new(state.clone())), progress.clone())
            .expect("handled sink");
    let mut replay = replay(
        query.results(),
        progress.clone(),
        ConsumerRecoveryPolicy::Strict,
    );
    replay.start().await.expect("replay start");
    let initial = replay
        .on_wakeup()
        .await
        .expect("snapshot")
        .pop()
        .expect("snapshot envelope");
    assert!(sink
        .handle(InputEnvelope {
            port: PortId::try_new("in").expect("port"),
            envelope: initial.envelope
        })
        .await
        .is_err());
    assert_eq!(progress.load("query").await.expect("checkpoint"), None);
    replay.stop().await.expect("stop failed startup");
    replay.start().await.expect("retry snapshot");
    let initial = replay
        .on_wakeup()
        .await
        .expect("snapshot retry")
        .pop()
        .expect("snapshot");
    sink.handle(InputEnvelope {
        port: PortId::try_new("in").expect("port"),
        envelope: initial.envelope,
    })
    .await
    .expect("actual snapshot handled");
    assert_eq!(
        progress.load("query").await.expect("checkpoint"),
        Some(ConsumerCheckpoint {
            sequence: 0,
            generation: 0
        })
    );
    let live = query
        .transform(InputEnvelope {
            port: PortId::try_new("in").expect("port"),
            envelope: envelope(2, "Alicia", true),
        })
        .await
        .expect("live")
        .pop()
        .expect("output");
    let forwarded = replay
        .transform(InputEnvelope {
            port: PortId::try_new("in").expect("port"),
            envelope: live.envelope.clone(),
        })
        .await
        .expect("handoff");
    assert_eq!(forwarded.len(), 1);
    sink.handle(InputEnvelope {
        port: PortId::try_new("in").expect("port"),
        envelope: forwarded[0].envelope.clone(),
    })
    .await
    .expect("handle live");
    assert!(replay
        .transform(InputEnvelope {
            port: PortId::try_new("in").expect("port"),
            envelope: live.envelope
        })
        .await
        .expect("overlap dedup")
        .is_empty());
    assert_eq!(
        progress.load("query").await.expect("checkpoint"),
        Some(ConsumerCheckpoint {
            sequence: 1,
            generation: 0
        })
    );
    assert_eq!(state.snapshots.load(Ordering::SeqCst), 1);
    query.stop().await.expect("stop query");
}

#[tokio::test]
async fn retained_history_gap_policies_are_explicit_and_do_not_persist_staged_progress() {
    let mut query = query(
        Arc::new(InMemoryComputationProvider),
        "MATCH (n:Person) RETURN n.name AS name",
        QueryRecoveryPolicy::Strict,
        false,
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    query.start().await.expect("start");
    for sequence in 2..=5 {
        query
            .transform(InputEnvelope {
                port: PortId::try_new("in").expect("port"),
                envelope: envelope(sequence, &format!("name-{sequence}"), true),
            })
            .await
            .expect("live output");
    }
    for policy in [
        ConsumerRecoveryPolicy::Strict,
        ConsumerRecoveryPolicy::AutoReset,
        ConsumerRecoveryPolicy::AutoSkipGap,
    ] {
        let progress = Arc::new(MemoryConsumerProgress::default());
        progress
            .commit_handled(
                "query",
                ConsumerCheckpoint {
                    sequence: 1,
                    generation: 0,
                },
            )
            .await
            .expect("prior handled checkpoint");
        let mut replay = replay(query.results(), progress.clone(), policy);
        replay.start().await.expect("start replay");
        let output = replay.on_wakeup().await;
        match policy {
            ConsumerRecoveryPolicy::Strict => assert!(output.is_err()),
            ConsumerRecoveryPolicy::AutoReset => assert!(QueryChangeCodec::is_snapshot(
                &output.expect("snapshot")[0].envelope
            )),
            ConsumerRecoveryPolicy::AutoSkipGap => {
                assert_eq!(output.expect("explicit skip").len(), 2)
            }
        }
        assert_eq!(
            progress.load("query").await.expect("unmodified checkpoint"),
            Some(ConsumerCheckpoint {
                sequence: 1,
                generation: 0
            })
        );
    }
    query.stop().await.expect("stop");
}

#[tokio::test]
async fn state_store_consumer_checkpoints_and_producer_sequences_survive_wrapper_reconstruction() {
    let provider = Arc::new(drasi_lib::state_store::MemoryStateStoreProvider::new());
    let first =
        StateStoreConsumerProgress::new("graph", "consumer", provider.clone()).expect("scope");
    let stream = StreamId::try_new("replay/out").expect("stream");
    assert_eq!(first.allocate_sequence(&stream).await.expect("sequence"), 1);
    first
        .commit_handled(
            "query",
            ConsumerCheckpoint {
                sequence: 7,
                generation: 2,
            },
        )
        .await
        .expect("handled progress");
    let restored =
        StateStoreConsumerProgress::new("graph", "consumer", provider).expect("same scope");
    assert_eq!(
        restored
            .allocate_sequence(&stream)
            .await
            .expect("new producer sequence"),
        2
    );
    assert_eq!(
        restored.load("query").await.expect("restored progress"),
        Some(ConsumerCheckpoint {
            sequence: 7,
            generation: 2
        })
    );
}

struct LiveSource {
    descriptor: ComponentDescriptor,
    events: std::collections::VecDeque<ChangeEnvelope>,
}
#[async_trait]
impl ComputationComponent for LiveSource {
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
impl EnvelopeSource for LiveSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.events.pop_front().map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").expect("port"),
            envelope,
        }))
    }
}

#[tokio::test(flavor = "current_thread")]
async fn graph_bootstrap_snapshot_and_live_handoff_run_through_real_components() {
    let query = query(
        Arc::new(InMemoryComputationProvider),
        "MATCH (n:Person) RETURN n.name AS name",
        QueryRecoveryPolicy::Strict,
        false,
        Arc::new(AtomicUsize::new(0)),
    )
    .await;
    let progress = Arc::new(MemoryConsumerProgress::default());
    let replay = replay(
        query.results(),
        progress.clone(),
        ConsumerRecoveryPolicy::AutoReset,
    );
    let state = Arc::new(ConsumerState::default());
    let sink = CheckpointedSink::new(Box::new(RecoverySink::new(state.clone())), progress.clone())
        .expect("sink");
    let source = LiveSource {
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
        events: vec![envelope(1, "Alice", false), envelope(2, "Alicia", true)].into(),
    };
    let endpoint = |node: &str, port: &str| {
        Endpoint::new(
            ComponentId::try_new(node).expect("id"),
            PortId::try_new(port).expect("port"),
        )
    };
    let mut graph = ComputationGraph::builder("snapshot-handoff")
        .source(Box::new(source))
        .query(Box::new(query))
        .transformer(Box::new(replay))
        .sink(Box::new(sink))
        .bind_stream(
            endpoint("source", "out"),
            StreamId::try_new("source/out").expect("stream"),
        )
        .bind_stream(
            endpoint("query", "out"),
            StreamId::try_new("query/out").expect("stream"),
        )
        .bind_stream(
            endpoint("replay", "out"),
            StreamId::try_new("replay/out").expect("stream"),
        )
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("query", "out"), endpoint("replay", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("replay", "out"), endpoint("sink", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .build()
        .expect("graph");
    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        graph.start().expect("scope"),
    )
    .await
    .expect("handoff deadlocked")
    .expect("complete graph");
    assert_eq!(state.snapshots.load(Ordering::SeqCst), 1);
    let row = {
        let rows = state.rows.lock().expect("rows");
        assert_eq!(rows.len(), 1);
        QueryChangeCodec::decode_row(rows.values().next().expect("one row")).expect("row")
    };
    assert_eq!(
        row.values.get("name"),
        Some(&drasi_core::evaluation::variable_value::VariableValue::from("Alicia"))
    );
    assert_eq!(
        progress.load("query").await.expect("handled checkpoint"),
        Some(ConsumerCheckpoint {
            sequence: 1,
            generation: 0
        })
    );
}

#[cfg(feature = "computation-rocksdb-tests")]
mod persistent {
    use super::*;
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };

    fn provider(path: &std::path::Path) -> Arc<dyn ComputationIndexProvider> {
        Arc::new(RocksDbComputationProvider::new(
            path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("budget"),
            ),
        ))
    }

    #[tokio::test]
    async fn interrupted_bootstrap_stays_visible_until_explicit_reset_rebuilds_it() {
        let temp = tempfile::tempdir().expect("temp");
        let provider = provider(temp.path());
        let calls = Arc::new(AtomicUsize::new(0));
        let text = "MATCH (n:Person) RETURN n.name AS name";
        {
            let mut failed = query(
                provider.clone(),
                text,
                QueryRecoveryPolicy::Strict,
                true,
                calls.clone(),
            )
            .await;
            assert!(failed.start().await.is_err());
            failed.stop().await.expect("cleanup interrupted bootstrap");
        }
        {
            let mut strict = query(
                provider.clone(),
                text,
                QueryRecoveryPolicy::Strict,
                false,
                calls.clone(),
            )
            .await;
            let error = strict
                .start()
                .await
                .expect_err("do not silently reuse partial index");
            assert!(matches!(
                error.downcast_ref::<QueryRecoveryError>(),
                Some(QueryRecoveryError::IncompleteBootstrap)
            ));
            strict.stop().await.expect("cleanup");
        }
        {
            let mut reset = query(
                provider.clone(),
                text,
                QueryRecoveryPolicy::AutoReset,
                false,
                calls.clone(),
            )
            .await;
            reset.start().await.expect("explicit safe reset");
            assert_eq!(reset.results().snapshot().expect("snapshot").rows.len(), 1);
            assert_eq!(
                reset.results().snapshot().expect("snapshot").as_of_sequence,
                0
            );
            reset.stop().await.expect("stop");
        }
        {
            let mut reopened = query(
                provider,
                text,
                QueryRecoveryPolicy::Strict,
                true,
                calls.clone(),
            )
            .await;
            reopened
                .start()
                .await
                .expect("completed bootstrap marker survives restart");
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            reopened.stop().await.expect("stop");
        }
    }

    #[tokio::test]
    async fn query_reset_retains_output_high_water_and_requires_snapshot_catchup() {
        let temp = tempfile::tempdir().expect("temp");
        let provider = provider(temp.path());
        let calls = Arc::new(AtomicUsize::new(0));
        {
            let mut first = query(
                provider.clone(),
                "MATCH (n:Person) RETURN n.name AS name",
                QueryRecoveryPolicy::Strict,
                false,
                calls.clone(),
            )
            .await;
            first.start().await.expect("start");
            let output = first
                .transform(InputEnvelope {
                    port: PortId::try_new("in").expect("port"),
                    envelope: envelope(2, "Alicia", true),
                })
                .await
                .expect("live");
            assert_eq!(output[0].envelope.system().sequence(), 1);
            first.stop().await.expect("stop");
        }
        let text = "MATCH (n:Person) RETURN n.name AS renamed";
        {
            let mut reset = query(
                provider.clone(),
                text,
                QueryRecoveryPolicy::AutoReset,
                false,
                calls.clone(),
            )
            .await;
            reset.start().await.expect("changed query reset");
            assert_eq!(
                reset.results().snapshot().expect("snapshot").as_of_sequence,
                1,
                "no sequence regression"
            );
            assert!(matches!(
                reset.results().replay(0),
                Err(QueryHistoryError::Unavailable { .. })
            ));
            assert_eq!(reset.results().snapshot().expect("snapshot").rows.len(), 1);
            assert!(reset.results().snapshot().expect("snapshot").generation > 0);
            let progress = Arc::new(MemoryConsumerProgress::default());
            progress
                .commit_handled(
                    "query",
                    ConsumerCheckpoint {
                        sequence: 1,
                        generation: 0,
                    },
                )
                .await
                .expect("old generation");
            let mut replay = replay(
                reset.results(),
                progress.clone(),
                ConsumerRecoveryPolicy::AutoReset,
            );
            replay.start().await.expect("restart consumer");
            let output = replay
                .on_wakeup()
                .await
                .expect("reset snapshot")
                .pop()
                .expect("snapshot at same sequence");
            let state = Arc::new(ConsumerState::default());
            let mut sink =
                CheckpointedSink::new(Box::new(RecoverySink::new(state.clone())), progress.clone())
                    .expect("sink");
            sink.handle(InputEnvelope {
                port: PortId::try_new("in").expect("port"),
                envelope: output.envelope,
            })
            .await
            .expect("new generation must replace snapshot");
            assert_eq!(state.snapshots.load(Ordering::SeqCst), 1);
            assert_eq!(
                progress
                    .load("query")
                    .await
                    .expect("new checkpoint")
                    .expect("checkpoint")
                    .generation,
                reset.results().snapshot().expect("snapshot").generation
            );
            reset.stop().await.expect("stop");
        }
        {
            let mut recovered =
                query(provider, text, QueryRecoveryPolicy::Strict, false, calls).await;
            recovered
                .start()
                .await
                .expect("empty retained history is valid at a recorded reset baseline");
            assert_eq!(
                recovered
                    .results()
                    .snapshot()
                    .expect("snapshot")
                    .as_of_sequence,
                1
            );
            let output = recovered
                .transform(InputEnvelope {
                    port: PortId::try_new("in").expect("port"),
                    envelope: envelope(2, "Alicia", true),
                })
                .await
                .expect("new live output");
            assert_eq!(output[0].envelope.system().sequence(), 2);
            recovered.stop().await.expect("stop");
        }
    }

    #[tokio::test]
    async fn reset_preserves_pending_publication_high_water() {
        let temp = tempfile::tempdir().expect("temp");
        let provider = provider(temp.path());
        {
            let resources = provider
                .create_indexes("recovery", "query")
                .await
                .expect("resources");
            let session = &resources.indexes().session_control;
            session.begin().await.expect("begin");
            resources
                .checkpoint_store()
                .expect("checkpoint")
                .stage_checkpoint("\0computation:pending-output:v1", 17, None)
                .await
                .expect("pending output");
            session.commit().await.expect("commit");
            resources
                .cleanup()
                .expect("cleanup owner")
                .shutdown()
                .await
                .expect("cleanup");
        }
        let mut reset = query(
            provider,
            "MATCH (n:Person) RETURN n.name AS name",
            QueryRecoveryPolicy::AutoReset,
            false,
            Arc::new(AtomicUsize::new(0)),
        )
        .await;
        reset.start().await.expect("explicit reset");
        assert_eq!(
            reset.results().snapshot().expect("snapshot").as_of_sequence,
            17
        );
        let output = reset
            .transform(InputEnvelope {
                port: PortId::try_new("in").expect("port"),
                envelope: envelope(2, "Alicia", true),
            })
            .await
            .expect("live");
        assert_eq!(output[0].envelope.system().sequence(), 18);
        reset.stop().await.expect("stop");
    }

    #[tokio::test]
    async fn malformed_bootstrap_marker_is_retained_and_rejected() {
        for (sequence, position) in [(2, None), (1, Some(bytes::Bytes::from_static(b"invalid")))] {
            let temp = tempfile::tempdir().expect("temp");
            let provider = provider(temp.path());
            {
                let resources = provider
                    .create_indexes("recovery", "query")
                    .await
                    .expect("resources");
                resources
                    .indexes()
                    .session_control
                    .begin()
                    .await
                    .expect("begin");
                resources
                    .checkpoint_store()
                    .expect("checkpoint")
                    .stage_checkpoint(
                        "\0computation:query-bootstrap:v1",
                        sequence,
                        position.as_ref(),
                    )
                    .await
                    .expect("invalid marker");
                resources
                    .indexes()
                    .session_control
                    .commit()
                    .await
                    .expect("commit");
                resources
                    .cleanup()
                    .expect("cleanup owner")
                    .shutdown()
                    .await
                    .expect("cleanup");
            }
            let calls = Arc::new(AtomicUsize::new(0));
            let mut strict = query(
                provider,
                "MATCH (n:Person) RETURN n.name AS name",
                QueryRecoveryPolicy::Strict,
                false,
                calls.clone(),
            )
            .await;
            let error = strict.start().await.expect_err("invalid stored marker");
            assert!(matches!(
                error.downcast_ref::<QueryRecoveryError>(),
                Some(QueryRecoveryError::Inconsistent(_))
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            strict.stop().await.expect("cleanup");
        }
    }

    #[tokio::test]
    async fn no_op_at_output_sequence_limit_preserves_last_valid_source_cursor() {
        let temp = tempfile::tempdir().expect("temp");
        let provider = provider(temp.path());
        {
            let resources = provider
                .create_indexes("recovery", "query")
                .await
                .expect("resources");
            resources
                .indexes()
                .session_control
                .begin()
                .await
                .expect("begin");
            resources
                .checkpoint_store()
                .expect("checkpoint")
                .write_result_sequence("query", u64::MAX)
                .await
                .expect("high water");
            resources
                .outbox_writer()
                .expect("outbox")
                .append(
                    "computation-reset-v1:query",
                    1,
                    &serde_json::to_vec(&serde_json::json!({
                        "sequence": u64::MAX, "in_progress": false, "generation": 1
                    }))
                    .expect("marker"),
                )
                .await
                .expect("reset baseline");
            resources
                .indexes()
                .session_control
                .commit()
                .await
                .expect("commit");
            resources
                .cleanup()
                .expect("cleanup owner")
                .shutdown()
                .await
                .expect("cleanup");
        }
        {
            let mut query = ContinuousQueryTransformer::new(
                definition("MATCH (n:Missing) RETURN n.name AS name"),
                provider.clone(),
            )
            .await
            .expect("construct");
            query.start().await.expect("recover maximum sequence");
            for (sequence, position) in [
                (1, bytes::Bytes::from_static(b"valid-cursor")),
                (
                    2,
                    bytes::Bytes::from(vec![
                        0;
                        drasi_lib::sources::SourceBase::MAX_SOURCE_POSITION_BYTES
                            + 1
                    ]),
                ),
            ] {
                let original = envelope(sequence, "Alice", sequence > 1);
                let input = ChangeEnvelope::new(
                    original.id().clone(),
                    original.changes().clone(),
                    SystemMetadata::new(original.system().stream().clone(), sequence)
                        .with_source_position(position),
                );
                assert!(query
                    .transform(InputEnvelope {
                        port: PortId::try_new("in").expect("port"),
                        envelope: input,
                    })
                    .await
                    .expect("no output allocation for a no-op")
                    .is_empty());
            }
            query.stop().await.expect("stop");
        }
        let resources = provider
            .create_indexes("recovery", "query")
            .await
            .expect("resources");
        let checkpoints = resources
            .checkpoint_store()
            .expect("checkpoint")
            .read_all_checkpoints()
            .await
            .expect("checkpoints");
        let input = checkpoints
            .iter()
            .find(|(key, _)| key.starts_with("computation:input:"))
            .expect("input checkpoint")
            .1;
        assert_eq!(input.sequence, 2);
        assert_eq!(input.source_position.as_deref(), Some(&b"valid-cursor"[..]));
        resources
            .cleanup()
            .expect("cleanup owner")
            .shutdown()
            .await
            .expect("cleanup");
    }
}
