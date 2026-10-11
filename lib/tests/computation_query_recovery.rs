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

struct OwnedBootstrap {
    worker: tokio::sync::RwLock<Option<tokio::task::JoinHandle<()>>>,
    release: Arc<tokio::sync::Notify>,
    stopping: tokio::sync::Notify,
    stops: AtomicUsize,
}

#[async_trait]
impl ComputationBootstrapProvider for OwnedBootstrap {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        let release = self.release.clone();
        drasi_lib::context::workers::spawn_owned_worker(&self.worker, async move {
            release.notified().await;
        })
        .await?;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter([Err(anyhow::anyhow!(
                "snapshot failed after worker registration"
            ))])),
            watermarks: Vec::new(),
        })
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.stopping.notify_one();
        drasi_lib::context::workers::join_owned_worker_gracefully(
            &mut *self.worker.write().await,
            std::time::Duration::from_millis(20),
        )
        .await?;
        Ok(())
    }
}

#[tokio::test(flavor = "current_thread")]
async fn query_retains_bootstrap_cleanup_across_cancelled_and_timed_out_stop() {
    let bootstrap = Arc::new(OwnedBootstrap {
        worker: tokio::sync::RwLock::new(None),
        release: Arc::new(tokio::sync::Notify::new()),
        stopping: tokio::sync::Notify::new(),
        stops: AtomicUsize::new(0),
    });
    let mut query = ContinuousQueryTransformer::new(
        definition("MATCH (n:Person) RETURN n.name AS name"),
        Arc::new(InMemoryComputationProvider),
    )
    .await
    .expect("query")
    .with_bootstrap(bootstrap.clone());
    query.start().await.expect_err("failed snapshot");
    {
        let stop = query.stop();
        tokio::pin!(stop);
        tokio::select! {
            result = &mut stop => panic!("unfinished cleanup returned: {result:?}"),
            _ = bootstrap.stopping.notified() => {}
        }
    }
    assert!(bootstrap.worker.read().await.is_some());
    let error = query.stop().await.expect_err("worker still owns cleanup");
    assert!(error
        .downcast_ref::<drasi_lib::context::workers::WorkerCleanupError>()
        .is_some());
    assert!(bootstrap.worker.read().await.is_some());
    assert!(bootstrap.snapshot().await.is_err());
    bootstrap.release.notify_one();
    query.stop().await.expect("joined snapshot worker");
    assert!(bootstrap.worker.read().await.is_none());
    query
        .deprovision()
        .await
        .expect("deprovision after cleanup");
    assert_eq!(bootstrap.stops.load(Ordering::SeqCst), 4);
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
            generation: 0,
            identity: query
                .results()
                .recovery_view(None)
                .expect("identity")
                .identity,
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
            generation: 0,
            identity: query
                .results()
                .recovery_view(None)
                .expect("identity")
                .identity,
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
                    identity: query
                        .results()
                        .recovery_view(None)
                        .expect("identity")
                        .identity,
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
                let output = output.expect("explicit skip");
                assert_eq!(output.len(), 3);
                assert!(QueryChangeCodec::is_progress_only(&output[0].envelope));
            }
        }
        assert_eq!(
            progress.load("query").await.expect("unmodified checkpoint"),
            Some(ConsumerCheckpoint {
                sequence: 1,
                generation: 0,
                identity: query
                    .results()
                    .recovery_view(None)
                    .expect("identity")
                    .identity,
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
    let identity = QueryRecoveryIdentity::try_new(
        "graph",
        ComponentId::try_new("query").expect("query"),
        42,
        None,
    )
    .expect("identity");
    assert_eq!(first.allocate_sequence(&stream).await.expect("sequence"), 1);
    first
        .commit_handled(
            "query",
            ConsumerCheckpoint {
                sequence: 7,
                generation: 2,
                identity: identity.clone(),
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
            generation: 2,
            identity,
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

struct LateBindingSource {
    descriptor: ComponentDescriptor,
    receiver: tokio::sync::mpsc::UnboundedReceiver<ChangeEnvelope>,
}

#[async_trait]
impl ComputationComponent for LateBindingSource {
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
impl EnvelopeSource for LateBindingSource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.receiver.recv().await.map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").expect("port"),
            envelope,
        }))
    }
}

struct LateBindingSink {
    descriptor: ComponentDescriptor,
    sender: tokio::sync::mpsc::UnboundedSender<ChangeEnvelope>,
}

#[async_trait]
impl ComputationComponent for LateBindingSink {
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
impl EnvelopeSink for LateBindingSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Accepted
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.sender.send(input.envelope).map_err(Into::into)
    }
}

#[tokio::test(flavor = "current_thread")]
async fn bootstrapped_query_delivers_live_updates_to_second_batch_consumers() -> anyhow::Result<()>
{
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        for (start_before_binding, factory_query) in [(false, false), (true, false), (false, true), (true, true)] {
            let core = drasi_lib::DrasiLib::builder().build().await?;
            let query = query(
                Arc::new(InMemoryComputationProvider),
                "MATCH (n:Person) RETURN collect({name:n.name}) AS records",
                QueryRecoveryPolicy::Strict, false, Arc::new(AtomicUsize::new(0)),
            ).await;
            let mut quiet_definition = definition("MATCH (n:Person) RETURN count(n) AS count");
            quiet_definition.id = ComponentId::try_new("quiet")?;
            quiet_definition.output_stream = StreamId::try_new("quiet/out")?;
            let quiet = ContinuousQueryTransformer::new_with_options(
                quiet_definition, Arc::new(InMemoryComputationProvider), QueryOptions::default(),
            ).await?.with_bootstrap(Arc::new(Bootstrap { calls: Arc::new(AtomicUsize::new(0)), fail: false }));
            let endpoint = |component: &str, port: &str| Endpoint::new(ComponentId::try_new(component).expect("component"), PortId::try_new(port).expect("port"));
            let (send_input, receive_input) = tokio::sync::mpsc::unbounded_channel();
            let (early_sender, mut early_receiver) = tokio::sync::mpsc::unbounded_channel();
            let source = LateBindingSource {
                descriptor: ComponentDescriptor::try_new(ComponentId::try_new("source")?, vec![
                    PortDescriptor::new(PortId::try_new("out")?, PortDirection::Output, GraphChangeCodec::schema().descriptor().clone(), PipeRequirements::default()),
                ])?, receiver: receive_input,
            };
            let sink = |id: &str, sender| -> anyhow::Result<LateBindingSink> {
                Ok(LateBindingSink { descriptor: ComponentDescriptor::try_new(ComponentId::try_new(id)?, vec![
                    PortDescriptor::new(PortId::try_new("in")?, PortDirection::Input, QueryChangeCodec::schema().descriptor().clone(), PipeRequirements::default()),
                ])?, sender })
            };
            let first = ComponentBatch::builder().source(Box::new(source))
                .query(Box::new(quiet)).sink(Box::new(sink("early", early_sender)?));
            let first = if factory_query {
                let indexes = ResourceId::try_new("late-binding-indexes")?;
                let bootstrap = ResourceId::try_new("late-binding-bootstrap")?;
                let factory = Arc::new(ContinuousQueryFactory::default());
                let specification = ComponentSpecification {
                    descriptor: query.descriptor().clone(), role: ComponentRole::Query, completion: None,
                    implementation: factory.descriptor().implementation.clone(), configuration_version: 1,
                    configuration: std::collections::BTreeMap::from([
                        (Arc::from("query"), ConfigurationValue::Literal("MATCH (n:Person) RETURN collect({name:n.name}) AS records".into())),
                        (Arc::from("stream"), ConfigurationValue::Literal("query/out".into())),
                    ]),
                    dependencies: std::collections::BTreeMap::from([
                        (Arc::from("indexes"), vec![indexes.clone()]),
                        (Arc::from("bootstrap"), vec![bootstrap.clone()]),
                    ]),
                };
                drop(query);
                first.declare_resource(ResourceSpecification {
                    id: indexes.clone(), role: ResourceRole::IndexBackend, ownership: ResourceOwnership::Borrowed, binding: Arc::from("indexes"),
                })?.provide_resource(indexes, ResourceHandle::new(ResourceRole::IndexBackend,
                    Arc::new(QueryIndexProviderResource(Arc::new(InMemoryComputationProvider)))))?
                    .declare_resource(ResourceSpecification {
                        id: bootstrap.clone(), role: ResourceRole::Bootstrap, ownership: ResourceOwnership::Borrowed, binding: Arc::from("bootstrap"),
                    })?.provide_resource(bootstrap, ResourceHandle::new(ResourceRole::Bootstrap,
                        Arc::new(QueryBootstrapResource(Arc::new(Bootstrap { calls: Arc::new(AtomicUsize::new(0)), fail: false })))))?
                    .component(specification, factory)
            } else {
                first.query(Box::new(query))
            };
            let first = first
                .bind_stream(endpoint("source", "out"), StreamId::try_new("source/out")?)
                .bind_stream(endpoint("query", "out"), StreamId::try_new("query/out")?)
                .bind_stream(endpoint("quiet", "out"), StreamId::try_new("quiet/out")?)
                .connect(EdgeDefinition::new(endpoint("source","out"),endpoint("query","in")), Box::new(BoundedPipeConfig { capacity: 32 }))
                .connect(EdgeDefinition::new(endpoint("source","out"),endpoint("quiet","in")), Box::new(BoundedPipeConfig { capacity: 32 }))
                .connect(EdgeDefinition::new(endpoint("query","out"),endpoint("early","in")), Box::new(BoundedPipeConfig { capacity: 32 }))
                .connect(EdgeDefinition::new(endpoint("quiet","out"),endpoint("early","in")), Box::new(BoundedPipeConfig { capacity: 32 }))
                .build()?;
            assert!(core.add_components(first).await?.committed);
            if start_before_binding {
                core.start().await?;
                core.computation_component("query")?.wait_started().await?;
                let reader = core.query_manager().get_query_instance("query").await.map_err(anyhow::Error::msg)?;
                let snapshot = reader.fetch_snapshot().await?;
                assert_eq!(core.get_query_results("query").await?.len(), 1);
                assert_eq!(snapshot.as_of_sequence, 0);
            }
            let (late_sender, mut late_receiver) = tokio::sync::mpsc::unbounded_channel();
            let mut second = ComponentBatch::builder().sink(Box::new(sink("late", late_sender)?)).build()?;
            for producer in ["query", "quiet"] {
                second.definition.relationships.push(DesiredRelationship {
                    definition: EdgeDefinition::new(endpoint(producer,"out"),endpoint("late","in")),
                    pipe: DesiredPipe::Bounded { capacity: 32 }, policy: RelationshipPolicy::default(),
                });
            }
            let report = core.add_components(second).await?;
            assert!(report.committed && report.summary == OperationSummary::Completed, "{report:?}");
            if !start_before_binding { core.start().await?; }
            core.computation_component("late")?.wait_started().await?;
            for (sequence, name) in [(2, "Alicia"), (3, "Alice")] {
                send_input.send(envelope(sequence, name, true))?;
                for receiver in [&mut early_receiver, &mut late_receiver] {
                    let output = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv()).await?
                        .ok_or_else(|| anyhow::anyhow!("consumer closed after binding"))?;
                    assert_eq!(output.system().sequence(), sequence - 1);
                    let decoded = QueryChangeCodec::decode_evaluation(&output)?;
                    let values = match &decoded[..] {
                        [drasi_core::evaluation::context::QueryPartEvaluationContext::Aggregation { after, .. }] => after,
                        _ => anyhow::bail!("expected one aggregation update: {decoded:?}"),
                    };
                    assert_eq!(serde_json::to_value(values)?, serde_json::json!({"records":[{"name":name}]}));
                }
            }
            core.shutdown().await?;
        }
        Ok::<_, anyhow::Error>(())
    }).await?
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
    let results = query.results();
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
            generation: 0,
            identity: results.recovery_view(None).expect("identity").identity,
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

    struct CoordinatedBootstrap {
        fail: bool,
        observed: std::sync::Mutex<Vec<Option<bytes::Bytes>>>,
        calls: AtomicUsize,
    }

    #[tokio::test(flavor = "current_thread")]
    async fn failed_snapshot_releases_published_storage_views_before_same_object_restart() {
        let temp = tempfile::tempdir().expect("temp");
        let calls = Arc::new(AtomicUsize::new(0));
        let mut query = query(
            provider(temp.path()),
            "MATCH (n:Person) RETURN n.name AS name",
            QueryRecoveryPolicy::Strict,
            true,
            calls,
        )
        .await;
        let results = query.results();
        query.start().await.expect_err("partial bootstrap");
        query.stop().await.expect("release failed storage owner");
        let error = query
            .start()
            .await
            .expect_err("partial bootstrap requires reset");
        assert!(
            matches!(
                error.downcast_ref::<QueryRecoveryError>(),
                Some(QueryRecoveryError::IncompleteBootstrap)
            ),
            "restart must read recovery state, not retain its old database lock: {error:#}"
        );
        query.stop().await.expect("release reconstructed owner");
        query
            .deprovision()
            .await
            .expect("reopen and clear incomplete state");
        query
            .start()
            .await
            .expect_err("new bootstrap still intentionally fails");
        assert_eq!(results.snapshot().expect("view").rows.len(), 1);
        query.stop().await.expect("cleanup");
    }

    #[async_trait]
    impl ComputationBootstrapProvider for CoordinatedBootstrap {
        async fn prepare_with_state(
            &self,
            state: &dyn BootstrapState,
        ) -> anyhow::Result<BootstrapPreparation> {
            state
                .durability()
                .require(drasi_core::interface::FailureMode::ProcessRestart)?;
            let recovered = state.read().await?;
            self.observed.lock().expect("observed").push(recovered);
            Ok(BootstrapPreparation::Ready)
        }

        async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
            anyhow::bail!("coordinated bootstrap requires query-owned state")
        }

        async fn snapshot_with_state(
            &self,
            state: &dyn BootstrapState,
        ) -> anyhow::Result<ComputationBootstrapSnapshot> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(state.write(bytes::Bytes::new()).await.is_err());
            assert!(state
                .write(bytes::Bytes::from(vec![0; MAX_BOOTSTRAP_STATE_BYTES + 1]))
                .await
                .is_err());
            state
                .write(bytes::Bytes::from_static(b"initialization-intent"))
                .await?;
            let mut changes = vec![Ok(envelope(1, "Alice", false))];
            if self.fail {
                changes.push(Err(anyhow::anyhow!("interrupted snapshot")));
            }
            Ok(ComputationBootstrapSnapshot {
                changes: Box::pin(futures::stream::iter(changes)),
                watermarks: vec![BootstrapWatermark {
                    stream: StreamId::try_new("source/out")?,
                    source_id: Some("people".into()),
                    sequence: 17,
                    position: Some(bytes::Bytes::from_static(b"consistent-boundary")),
                }],
            })
        }

        fn completion_state(&self) -> anyhow::Result<Option<bytes::Bytes>> {
            assert!(!self.fail, "failed snapshot cannot reach completion");
            Ok(Some(bytes::Bytes::from_static(b"completed-initialization")))
        }
    }

    #[tokio::test]
    async fn bootstrap_handover_intent_survives_reset_and_completes_with_its_watermark() {
        let temp = tempfile::tempdir().expect("temp");
        let provider = provider(temp.path());
        let text = "MATCH (n:Person) RETURN n.name AS name";
        for (index, recovery, fail) in [
            (0, QueryRecoveryPolicy::Strict, true),
            (1, QueryRecoveryPolicy::AutoReset, false),
            (2, QueryRecoveryPolicy::Strict, false),
        ] {
            let bootstrap = Arc::new(CoordinatedBootstrap {
                fail,
                observed: std::sync::Mutex::new(Vec::new()),
                calls: AtomicUsize::new(0),
            });
            let progress = Arc::new(
                QuerySourceProgress::new("recovery", ComponentId::try_new("query").expect("id"))
                    .expect("progress"),
            );
            let mut query = ContinuousQueryTransformer::new_with_options(
                definition(text),
                provider.clone(),
                QueryOptions {
                    recovery,
                    publication: QueryPublicationMode::Atomic,
                },
            )
            .await
            .expect("construct")
            .with_source_progress(progress.clone())
            .expect("bind")
            .with_bootstrap(bootstrap.clone());
            if fail {
                query.start().await.expect_err("partial snapshot");
                assert!(!progress.snapshot().ready);
                assert!(progress.snapshot().checkpoints.is_empty());
            } else {
                query.start().await.expect("complete or recover");
                let snapshot = progress.snapshot();
                assert!(snapshot.ready && snapshot.bootstrap_complete);
                let checkpoint = &snapshot.checkpoints[&SourceProgressKey::Source("people".into())];
                assert_eq!(checkpoint.sequence, 17);
                assert_eq!(
                    checkpoint.source_position.as_deref(),
                    Some(b"consistent-boundary".as_slice())
                );
                assert_eq!(query.results().snapshot().expect("snapshot").rows.len(), 1);
            }
            let expected = match index {
                0 => None,
                1 => Some(bytes::Bytes::from_static(b"initialization-intent")),
                _ => Some(bytes::Bytes::from_static(b"completed-initialization")),
            };
            assert_eq!(
                *bootstrap.observed.lock().expect("observed"),
                vec![expected]
            );
            assert_eq!(
                bootstrap.calls.load(Ordering::SeqCst),
                usize::from(index < 2)
            );
            query.stop().await.expect("cleanup");
        }
    }

    #[tokio::test]
    async fn corrupt_persisted_output_is_explicit_under_strict_and_resettable_under_auto_reset() {
        use drasi_core::interface::RowMutation;

        for corrupt in ["outbox", "snapshot", "metadata", "generation", "identity"] {
            let temp = tempfile::tempdir().expect("temp");
            let provider = provider(temp.path());
            let calls = Arc::new(AtomicUsize::new(0));
            let text = "MATCH (n:Person) RETURN n.name AS name";
            {
                let mut first = query(
                    provider.clone(),
                    text,
                    QueryRecoveryPolicy::Strict,
                    false,
                    calls.clone(),
                )
                .await;
                first.start().await.expect("bootstrap");
                first
                    .transform(InputEnvelope {
                        port: PortId::try_new("in").expect("port"),
                        envelope: envelope(2, "Alicia", true),
                    })
                    .await
                    .expect("committed output");
                first.stop().await.expect("stop");
            }
            {
                let resources = provider
                    .create_indexes("recovery", "query")
                    .await
                    .expect("resources");
                let outbox = resources.outbox_writer().expect("outbox");
                let live = resources.live_results_writer().expect("live rows");
                let retained = outbox.read_from("query", 0).await.expect("history");
                let stored_rows = live.read_snapshot("query").await.expect("snapshot");
                assert_eq!(retained.len(), 1);
                assert_eq!(stored_rows.len(), 1);
                resources
                    .indexes()
                    .session_control
                    .begin()
                    .await
                    .expect("begin");
                if corrupt == "snapshot" {
                    live.apply_mutations(
                        "query",
                        &[RowMutation {
                            row_signature: stored_rows[0].0,
                            data: Some(b"corrupt-snapshot"),
                        }],
                    )
                    .await
                    .expect("corrupt row");
                } else {
                    let bytes = if corrupt == "outbox" {
                        bytes::Bytes::from_static(b"corrupt-outbox")
                    } else {
                        let mut codec =
                            EnvelopeCodec::new(NonZeroUsize::new(64 * 1024 * 1024).expect("limit"));
                        codec
                            .register_schema(QueryChangeCodec::schema())
                            .expect("schema");
                        let mut output = codec.decode(&retained[0].1).expect("valid original");
                        let key = match corrupt {
                            "metadata" => "drasi.query-output.v1",
                            "generation" => "drasi.query-generation.v1",
                            "identity" => "drasi.query-recovery-identity.v1",
                            _ => unreachable!(),
                        };
                        output
                            .append_annotation(
                                ContextEntry::try_new(
                                    ComponentId::try_new("query").expect("id"),
                                    key,
                                    ContextValue::Bool(true),
                                )
                                .expect("annotation"),
                            )
                            .expect("append");
                        codec
                            .encode(&output)
                            .expect("envelope with invalid query annotation")
                    };
                    outbox
                        .append("query", retained[0].0, &bytes)
                        .await
                        .expect("corrupt output");
                }
                resources
                    .indexes()
                    .session_control
                    .commit()
                    .await
                    .expect("commit corruption");
                resources
                    .cleanup()
                    .expect("owner")
                    .shutdown()
                    .await
                    .expect("close");
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
                let error = strict.start().await.expect_err(corrupt);
                assert!(
                    matches!(
                        error.downcast_ref::<QueryRecoveryError>(),
                        Some(QueryRecoveryError::Inconsistent(_))
                    ),
                    "{corrupt}: {error:#}"
                );
                assert!(error
                    .to_string()
                    .contains("inconsistent durable query output"));
                assert!(
                    error.chain().count() > 1,
                    "the original decoding cause must be retained"
                );
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "Strict does not bootstrap away corruption"
                );
                strict.stop().await.expect("stop failed recovery");
            }
            let mut reset = query(
                provider,
                text,
                QueryRecoveryPolicy::AutoReset,
                false,
                calls.clone(),
            )
            .await;
            reset.start().await.expect(corrupt);
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            let snapshot = reset.results().snapshot().expect("rebuilt snapshot");
            assert_eq!(
                snapshot.as_of_sequence, 1,
                "do not reuse committed output numbers"
            );
            assert_eq!(snapshot.rows.len(), 1);
            assert!(snapshot.generation > 0);
            assert_eq!(
                QueryChangeCodec::decode_row(snapshot.rows.values().next().expect("row"))
                    .expect("row")
                    .values
                    .get("name"),
                Some(&drasi_core::evaluation::variable_value::VariableValue::from("Alice")),
            );
            reset.stop().await.expect("stop reset");
        }
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
    async fn deprovision_advances_output_generation_so_recreated_sequences_are_never_reused() {
        let temp = tempfile::tempdir().expect("temp");
        let provider = provider(temp.path());
        let calls = Arc::new(AtomicUsize::new(0));
        let mut published = Vec::new();
        for incarnation in 0..3 {
            let mut query = query(
                provider.clone(),
                "MATCH (n:Person) RETURN n.name AS name",
                QueryRecoveryPolicy::Strict,
                false,
                calls.clone(),
            )
            .await;
            query
                .start()
                .await
                .unwrap_or_else(|error| panic!("incarnation {incarnation} starts: {error:#}"));
            let output = query
                .transform(InputEnvelope {
                    port: PortId::try_new("in").expect("port"),
                    envelope: envelope(2, "Alicia", true),
                })
                .await
                .expect("live");
            assert_eq!(
                output[0].envelope.system().sequence(),
                1,
                "a recreated query restarts its output sequence"
            );
            let generation =
                QueryChangeCodec::query_generation(&output[0].envelope).expect("generation");
            assert_eq!(
                generation,
                query.results().snapshot().expect("snapshot").generation
            );
            published.push(generation);
            query.stop().await.expect("stop");
            query.deprovision().await.expect("deprovision");
        }
        assert!(
            published.windows(2).all(|pair| pair[0] < pair[1]),
            "every incarnation must publish under a newer generation than any \
             position a consumer could hold for an earlier one: {published:?}"
        );
    }

    #[tokio::test]
    async fn query_reset_retains_output_high_water_and_requires_snapshot_catchup() {
        let temp = tempfile::tempdir().expect("temp");
        let provider = provider(temp.path());
        let calls = Arc::new(AtomicUsize::new(0));
        let old_identity = {
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
            let identity = first
                .results()
                .recovery_view(None)
                .expect("identity")
                .identity;
            first.stop().await.expect("stop");
            identity
        };
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
                        identity: old_identity,
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
                .stage_result_sequence("query", u64::MAX)
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
