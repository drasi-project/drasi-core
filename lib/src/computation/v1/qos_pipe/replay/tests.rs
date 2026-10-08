// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::computation::v1::Endpoint as GraphEndpoint;
use drasi_core::{
    computation::ComputationIndexProvider,
    evaluation::context::QueryPartEvaluationContext,
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use std::{path::Path, time::Duration};

fn definition(capacity: usize) -> QosChannelDefinition {
    QosChannelDefinition {
        stream: StreamId::try_new("out").unwrap(),
        capacity: NonZeroUsize::new(capacity).unwrap(),
        durable: true,
        retention: RetentionPolicy::Backpressure,
        subscribers: BTreeMap::from([
            ("first".into(), SubscriptionStart::Earliest),
            ("second".into(), SubscriptionStart::Earliest),
        ]),
    }
}

fn options(capacity: usize) -> ReplayOptions {
    ReplayOptions {
        failure_scope: FailureMode::ProcessRestart,
        receipt_capacity: NonZeroUsize::new(capacity).unwrap(),
    }
}

fn codec() -> EnvelopeCodec {
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1 << 20).unwrap());
    codec.register_schema(GraphChangeCodec::schema()).unwrap();
    codec.register_schema(QueryChangeCodec::schema()).unwrap();
    codec
}

async fn resources(path: &Path) -> ComputationIndexes {
    LegacyIndexProviderAdapter::new(Arc::new(RocksDbIndexProvider::new(path, false, false)))
        .create_indexes("replay", "channel")
        .await
        .unwrap()
}

async fn open(path: &Path, definition: QosChannelDefinition) -> Arc<QosChannel> {
    QosChannel::persistent(definition, resources(path).await, codec(), "journal")
        .await
        .unwrap()
}

fn persistent_identity() -> GraphProducerIdentity {
    GraphProducerIdentity::new(
        "scope".into(),
        "graph".into(),
        ComponentId::try_new("producer").unwrap(),
        StreamId::try_new("out").unwrap(),
        true,
    )
    .unwrap()
}

fn graph_output(producer: &GraphProducerIdentity, sequence: u64) -> ChangeEnvelope {
    let mut envelope = GraphChangeCodec::encode_change(
        SourceChange::Insert {
            element: Element::Node {
                metadata: ElementMetadata {
                    reference: ElementReference::new("source", &sequence.to_string()),
                    labels: Arc::from([Arc::from("Item")]),
                    effective_from: sequence,
                },
                properties: ElementPropertyMap::default(),
            },
        },
        StreamId::try_new("out").unwrap(),
        sequence,
        None,
    )
    .unwrap();
    GraphProducerProgress::annotate(&mut envelope, producer, sequence).unwrap();
    envelope
}

fn query_output(input: Option<&ChangeEnvelope>, sequence: u64) -> ChangeEnvelope {
    let component = ComponentId::try_new("query").unwrap();
    let mut envelope = QueryChangeCodec::encode_evaluation(
        input,
        &component,
        SystemMetadata::new(StreamId::try_new("out").unwrap(), sequence),
        &[QueryPartEvaluationContext::Adding {
            after: BTreeMap::new(),
            row_signature: sequence,
        }],
        QueryOutputMetadata {
            query_id: "query".into(),
            source_id: None,
            timestamp: chrono::DateTime::from_timestamp(1, 0).unwrap(),
            metadata: Default::default(),
            profiling: None,
        },
    )
    .unwrap()
    .unwrap();
    QueryRecoveryIdentity::try_new("graph", component.clone(), 42, None)
        .unwrap()
        .annotate(&mut envelope, &component)
        .unwrap();
    envelope
}

fn rejected(error: PipeError, expected: ReplayRejection) {
    let PipeError::Backend(error) = error else {
        panic!("unexpected error: {error}")
    };
    assert_eq!(
        error.downcast_ref::<ReplayRejection>(),
        Some(&expected),
        "{error:#}"
    );
}

#[test]
fn replay_receipt_and_identifier_limits_match_the_metadata_budget() {
    let mut definition = definition(2);
    definition.stream = StreamId::try_new("x".repeat(256)).unwrap();
    options(1024).validate(&definition).unwrap();
    assert!(options(1025).validate(&definition).is_err());
    let producer = Producer::Graph(
        GraphProducerIdentity::new(
            "x".repeat(256),
            "x".repeat(256),
            ComponentId::try_new("x".repeat(256)).unwrap(),
            definition.stream.clone(),
            true,
        )
        .unwrap(),
    );
    producer.validate(&definition.stream).unwrap();
    let oversized = Producer::Graph(
        GraphProducerIdentity::new(
            "x".repeat(257),
            "graph".into(),
            ComponentId::try_new("producer").unwrap(),
            definition.stream.clone(),
            true,
        )
        .unwrap(),
    );
    assert!(oversized.validate(&definition.stream).is_err());
    let mut replay = ReplayState {
        version: 2,
        identity: Some(uuid::Uuid::new_v4()),
        options: options(1024),
        producer: None,
        sequence: 0,
        receipts: BTreeMap::new(),
    };
    for sequence in u64::MAX - 1024..=u64::MAX {
        replay.record(
            &Candidate {
                producer: producer.clone(),
                sequence,
                digest: [255; 32],
            },
            sequence,
        );
    }
    assert_eq!(replay.receipts.len(), 1024);
    assert_eq!(
        replay.receipts.first_key_value().unwrap().0,
        &(u64::MAX - 1023)
    );
    let mut metadata = initial(&definition).unwrap();
    assert!(serde_json::to_value(&metadata)
        .unwrap()
        .get("replay")
        .is_none());
    metadata.replay = Some(replay);
    metadata.head = u64::MAX;
    metadata.producer_sequence = Some(u64::MAX);
    assert!(serde_json::to_vec(&metadata).unwrap().len() < admission::MAX_METADATA_BYTES);
}

async fn handle(channel: &Arc<QosChannel>, expected: u64) {
    for subscriber in ["first", "second"] {
        let mut connection = channel.bind(subscriber, ReplayGapPolicy::Strict).unwrap();
        let mut receiver = connection.pipe.take_receiver().unwrap();
        let delivery = tokio::time::timeout(Duration::from_secs(3), receiver.receive())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let candidate = Candidate::new(delivery.envelope(), &codec()).unwrap();
        assert_eq!(candidate.sequence, expected);
        delivery
            .into_parts()
            .1
            .unwrap()
            .complete(HandlingOutcome::Handled)
            .await
            .unwrap();
        connection.control.cancel();
    }
}

struct ReplaySource {
    descriptor: ComponentDescriptor,
    outputs: std::collections::VecDeque<ChangeEnvelope>,
}

#[async_trait::async_trait]
impl ComputationComponent for ReplaySource {
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

#[async_trait::async_trait]
impl EnvelopeSource for ReplaySource {
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(self.outputs.pop_front().map(|envelope| OutputEnvelope {
            port: PortId::try_new("out").unwrap(),
            envelope,
        }))
    }
}

struct ReplaySink {
    descriptor: ComponentDescriptor,
    seen: Arc<StdMutex<Vec<u64>>>,
}

#[async_trait::async_trait]
impl ComputationComponent for ReplaySink {
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

#[async_trait::async_trait]
impl EnvelopeSink for ReplaySink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.seen
            .lock()
            .unwrap()
            .push(input.envelope.system().sequence());
        Ok(())
    }
}

#[tokio::test]
async fn graph_connected_replay_reuses_one_multicast_acceptance() {
    let directory = tempfile::tempdir().unwrap();
    let definition = definition(1);
    let channel = open(directory.path(), definition.clone()).await;
    channel.enable_replay(options(2)).await.unwrap();
    let describe = |id, port, direction| {
        ComponentDescriptor::try_new(
            ComponentId::try_new(id).unwrap(),
            vec![PortDescriptor::new(
                PortId::try_new(port).unwrap(),
                direction,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )
        .unwrap()
    };
    let output = graph_output(&persistent_identity(), 1);
    let replayed = output.reemit(2).unwrap();
    let source = GraphEndpoint::new(
        ComponentId::try_new("producer").unwrap(),
        PortId::try_new("out").unwrap(),
    );
    let resource = ResourceId::try_new("channel").unwrap();
    let first = Arc::new(StdMutex::new(Vec::new()));
    let second = Arc::new(StdMutex::new(Vec::new()));
    let mut builder = ComputationGraph::builder("graph")
        .declare_resource(ResourceSpecification {
            id: resource.clone(),
            role: ResourceRole::StateStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "channel".into(),
        })
        .unwrap()
        .provide_resource(resource.clone(), channel.resource())
        .unwrap()
        .source(Box::new(ReplaySource {
            descriptor: describe("producer", "out", PortDirection::Output),
            outputs: [output, replayed].into(),
        }))
        .bind_stream(source.clone(), definition.stream.clone());
    for (id, seen) in [("first", first.clone()), ("second", second.clone())] {
        builder = builder
            .sink(Box::new(ReplaySink {
                descriptor: describe(id, "in", PortDirection::Input),
                seen,
            }))
            .connect(
                EdgeDefinition::new(
                    source.clone(),
                    GraphEndpoint::new(
                        ComponentId::try_new(id).unwrap(),
                        PortId::try_new("in").unwrap(),
                    ),
                ),
                Box::new(definition.pipe(resource.clone(), id)),
            );
    }
    let mut graph = builder.build().unwrap();
    tokio::time::timeout(Duration::from_secs(5), graph.start().unwrap())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*first.lock().unwrap(), [1]);
    assert_eq!(*second.lock().unwrap(), [1]);
    let progress = channel.progress().await.unwrap();
    assert_eq!(progress.accepted, 1);
    assert_eq!(progress.processed["first"], 1);
    assert_eq!(progress.processed["second"], 1);
    graph.shutdown().await.unwrap();
    channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn replay_receipts_survive_transport_changes_pruning_and_reconstruction() {
    for query in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let definition = definition(1);
        let producer = persistent_identity();
        let output = |sequence| {
            if query {
                query_output(None, sequence)
            } else {
                graph_output(&producer, sequence)
            }
        };
        let channel = open(directory.path(), definition.clone()).await;
        channel.enable_replay(options(2)).await.unwrap();
        let first = output(1);
        assert_eq!(channel.publish(&first).await.unwrap().position(), Some(1));
        let first_replay = first.reemit(20).unwrap();
        assert_ne!(first.id(), first_replay.id());
        assert_eq!(
            channel.publish(&first_replay).await.unwrap().position(),
            Some(1)
        );
        assert_eq!(channel.progress().await.unwrap().accepted, 1);
        handle(&channel, 1).await;
        let second = output(4);
        assert_eq!(channel.publish(&second).await.unwrap().position(), Some(2));
        handle(&channel, 4).await;
        channel.shutdown().await.unwrap();
        drop(channel);

        let channel = open(directory.path(), definition).await;
        channel.enable_replay(options(2)).await.unwrap();
        assert_eq!(
            channel.publish(&first_replay).await.unwrap().position(),
            Some(1)
        );
        let mut changed = first_replay.clone();
        changed
            .append_annotation(
                ContextEntry::try_new(
                    ComponentId::try_new("branch").unwrap(),
                    "decision",
                    ContextValue::Bool(true),
                )
                .unwrap(),
            )
            .unwrap();
        rejected(
            channel.publish(&changed).await.unwrap_err(),
            ReplayRejection::PayloadConflict(1),
        );
        assert_eq!(
            channel
                .publish(&second.reemit(30).unwrap())
                .await
                .unwrap()
                .position(),
            Some(2)
        );
        rejected(
            channel
                .publish(&output(8).reemit(2).unwrap())
                .await
                .unwrap_err(),
            ReplayRejection::TransportSequence {
                previous: 4,
                received: 2,
            },
        );
        assert_eq!(channel.progress().await.unwrap().accepted, 2);
        assert_eq!(
            channel
                .publish(&output(8).reemit(40).unwrap())
                .await
                .unwrap()
                .position(),
            Some(3)
        );
        rejected(
            channel.publish(&first).await.unwrap_err(),
            ReplayRejection::ReceiptExpired(1),
        );
        assert_eq!(channel.progress().await.unwrap().accepted, 3);
        assert_eq!(
            channel
                .state
                .lock()
                .await
                .metadata
                .replay
                .as_ref()
                .unwrap()
                .receipts
                .len(),
            2
        );
        channel.shutdown().await.unwrap();
    }
}

#[test]
fn replay_identity_ignores_only_transport_and_immediate_query_completion_timings() {
    let codec = codec();
    let mut input = query_output(None, 3);
    QueryChangeCodec::append_post_commit_profiling(
        &mut input,
        &ComponentId::try_new("query").unwrap(),
        10,
        11,
    )
    .unwrap();
    let original = query_output(Some(&input), 4);
    let mut live = original.reemit(20).unwrap();
    QueryChangeCodec::append_post_commit_profiling(
        &mut live,
        &ComponentId::try_new("query").unwrap(),
        100,
        110,
    )
    .unwrap();
    assert_eq!(
        Candidate::new(&original, &codec).unwrap().digest,
        Candidate::new(&live, &codec).unwrap().digest
    );
    let mut changed_input = input.clone();
    QueryChangeCodec::append_post_commit_profiling(
        &mut changed_input,
        &ComponentId::try_new("query").unwrap(),
        12,
        13,
    )
    .unwrap();
    let changed = query_output(Some(&changed_input), 4);
    assert_ne!(
        Candidate::new(&original, &codec).unwrap().digest,
        Candidate::new(&changed, &codec).unwrap().digest
    );

    let ancestor = GraphProducerIdentity::new(
        "scope".into(),
        "graph".into(),
        ComponentId::try_new("source").unwrap(),
        StreamId::try_new("source").unwrap(),
        true,
    )
    .unwrap();
    let input = GraphChangeCodec::encode_change(
        SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("source", "item"),
                labels: Arc::from([]),
                effective_from: 1,
            },
        },
        ancestor.stream().clone(),
        7,
        None,
    )
    .unwrap();
    let mut input = input;
    GraphProducerProgress::annotate(&mut input, &ancestor, 7).unwrap();
    assert!(matches!(
        Candidate::new(&query_output(Some(&input), 4), &codec)
            .unwrap()
            .producer,
        Producer::Query { .. }
    ));
    let mut transformed = query_output(Some(&input), 4);
    GraphProducerProgress::annotate(&mut transformed, &persistent_identity(), 19).unwrap();
    assert_eq!(Candidate::new(&transformed, &codec).unwrap().sequence, 19);
}

#[tokio::test]
async fn replay_rejects_unstable_missing_foreign_and_recovery_only_output_identities() {
    let directory = tempfile::tempdir().unwrap();
    let channel = open(directory.path(), definition(8)).await;
    channel.enable_replay(options(2)).await.unwrap();
    let producer = persistent_identity();
    let stable = graph_output(&producer, 1);
    channel.publish(&stable).await.unwrap();
    rejected(
        channel
            .publish(&graph_output(&persistent_identity(), 2))
            .await
            .unwrap_err(),
        ReplayRejection::ProducerChanged,
    );
    let volatile = GraphProducerIdentity::volatile(
        "scope".into(),
        "graph".into(),
        ComponentId::try_new("producer").unwrap(),
        StreamId::try_new("out").unwrap(),
    )
    .unwrap();
    rejected(
        channel
            .publish(&graph_output(&volatile, 2))
            .await
            .unwrap_err(),
        ReplayRejection::MissingProgress,
    );
    let missing = ChangeEnvelope::from_event(stable.event().clone());
    rejected(
        channel.publish(&missing).await.unwrap_err(),
        ReplayRejection::MissingProgress,
    );
    channel.shutdown().await.unwrap();

    let directory = tempfile::tempdir().unwrap();
    let channel = open(directory.path(), definition(8)).await;
    channel.enable_replay(options(2)).await.unwrap();
    let stable = query_output(None, 1);
    channel.publish(&stable).await.unwrap();
    let component = ComponentId::try_new("query").unwrap();
    let mut changed = stable.clone();
    QueryChangeCodec::set_generation(&mut changed, &component, 1).unwrap();
    rejected(
        channel.publish(&changed).await.unwrap_err(),
        ReplayRejection::ProducerChanged,
    );
    let mut changed = stable.clone();
    QueryRecoveryIdentity::try_new("graph", component.clone(), 43, None)
        .unwrap()
        .annotate(&mut changed, &component)
        .unwrap();
    rejected(
        channel.publish(&changed).await.unwrap_err(),
        ReplayRejection::ProducerChanged,
    );
    let mut changed = stable.clone();
    QueryRecoveryIdentity::try_new("graph", component.clone(), 42, Some(uuid::Uuid::new_v4()))
        .unwrap()
        .annotate(&mut changed, &component)
        .unwrap();
    rejected(
        channel.publish(&changed).await.unwrap_err(),
        ReplayRejection::MissingProgress,
    );
    let mut control = QueryChangeCodec::progress_envelope(
        "query",
        1,
        0,
        &component,
        SystemMetadata::new(StreamId::try_new("out").unwrap(), 30),
    )
    .unwrap();
    QueryRecoveryIdentity::from_envelope(&stable)
        .unwrap()
        .annotate(&mut control, &component)
        .unwrap();
    rejected(
        channel.publish(&control).await.unwrap_err(),
        ReplayRejection::MissingProgress,
    );
    assert_eq!(channel.progress().await.unwrap().accepted, 1);
    channel.shutdown().await.unwrap();
}

#[test]
fn replay_cannot_borrow_query_identity_or_sequence_from_an_ancestor() {
    let input = query_output(None, 1);
    let output = query_output(Some(&input), 2);
    for key in ["drasi.query-recovery-identity.v1", "drasi.query-output-sequence.v1"] {
        let mut removed = false;
        let mut entries: Vec<_> = output
            .annotations()
            .entries()
            .filter(|entry| {
                if entry.key() == key && !removed {
                    removed = true;
                    false
                } else {
                    true
                }
            })
            .collect();
        entries.reverse();
        let incomplete = ChangeEnvelope::restore(
            output.id().clone(),
            output.changes().clone(),
            output.system().as_ref().clone(),
            output.lineage().cloned(),
            output.context_identity(),
            entries,
        )
        .unwrap();
        assert!(removed);
        assert!(Candidate::new(&incomplete, &codec()).is_err(), "{key}");
    }
}

#[tokio::test]
async fn replay_configuration_is_opt_in_immutable_and_durability_checked() {
    let mut volatile_definition = definition(2);
    volatile_definition.durable = false;
    let volatile = QosChannel::volatile(volatile_definition).unwrap();
    assert!(volatile.enable_replay(options(2)).await.is_err());
    for retention in [RetentionPolicy::Backpressure, RetentionPolicy::PruneOldest] {
        let directory = tempfile::tempdir().unwrap();
        let mut definition = definition(2);
        definition.retention = retention;
        let channel = open(directory.path(), definition.clone()).await;
        let mut power = options(2);
        power.failure_scope = FailureMode::PowerLoss;
        assert!(channel.enable_replay(power).await.is_err());
        assert!(channel.enable_replay(options(1025)).await.is_err());
        if retention == RetentionPolicy::PruneOldest {
            assert!(channel.enable_replay(options(2)).await.is_err());
        } else {
            channel.enable_replay(options(2)).await.unwrap();
            channel.enable_replay(options(2)).await.unwrap();
            assert!(channel.enable_replay(options(3)).await.is_err());
            assert!(channel
                .enable_admission(AdmissionOptions {
                    construction_scope: "scope".into(),
                    graph_id: "graph".into(),
                    component_id: ComponentId::try_new("source").unwrap(),
                    failure_scope: FailureMode::ProcessRestart,
                    max_producers: NonZeroUsize::new(1).unwrap(),
                    receipts_per_producer: NonZeroUsize::new(1).unwrap(),
                })
                .await
                .is_err());
        }
        channel.shutdown().await.unwrap();
        drop(channel);
        if retention == RetentionPolicy::Backpressure {
            definition.capacity = NonZeroUsize::new(3).unwrap();
            assert!(QosChannel::persistent(
                definition,
                resources(directory.path()).await,
                codec(),
                "journal"
            )
            .await
            .is_err());
        }
    }
    let directory = tempfile::tempdir().unwrap();
    let channel = open(directory.path(), definition(2)).await;
    let output = graph_output(&persistent_identity(), 1);
    channel.publish(&output).await.unwrap();
    assert!(channel.enable_replay(options(2)).await.is_err());
    channel.publish(&output.reemit(2).unwrap()).await.unwrap();
    assert_eq!(
        channel.progress().await.unwrap().accepted,
        2,
        "ordinary mode still uses transport identity"
    );
    assert!(channel.state.lock().await.metadata.replay.is_none());
    channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn replay_journal_identity_survives_reopen_and_legacy_receipt_upgrade() {
    let directory = tempfile::tempdir().unwrap();
    let other_directory = tempfile::tempdir().unwrap();
    let channel = open(directory.path(), definition(2)).await;
    assert_eq!(channel.output_journal_identity(), None);
    channel.enable_replay(options(2)).await.unwrap();
    let first = channel.output_journal_identity().unwrap();
    assert!(!first.is_nil());
    let other = open(other_directory.path(), definition(2)).await;
    other.enable_replay(options(2)).await.unwrap();
    assert_ne!(other.output_journal_identity(), Some(first));
    other.shutdown().await.unwrap();
    let producer = persistent_identity();
    let output = graph_output(&producer, 1);
    channel.publish(&output).await.unwrap();
    channel.shutdown().await.unwrap();
    drop(channel);
    let channel = open(directory.path(), definition(2)).await;
    assert_eq!(channel.output_journal_identity(), Some(first));
    channel.shutdown().await.unwrap();
    drop(channel);

    let indexes = resources(directory.path()).await;
    let checkpoint = indexes.checkpoint_store().unwrap().clone();
    let saved = checkpoint
        .read_checkpoint("journal")
        .await
        .unwrap()
        .unwrap();
    let mut metadata: Metadata =
        serde_json::from_slice(saved.source_position.as_ref().unwrap()).unwrap();
    let replay = metadata.replay.as_mut().unwrap();
    replay.version = 1;
    replay.identity = None;
    let bytes = Bytes::from(serde_json::to_vec(&metadata).unwrap());
    let owner = ComputationTransaction::try_new(indexes).unwrap();
    owner
        .run(async {
            checkpoint
                .stage_checkpoint("journal", 1, Some(&bytes))
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    owner.shutdown().await.unwrap();
    drop((owner, checkpoint));

    let channel = open(directory.path(), definition(2)).await;
    let upgraded = channel.output_journal_identity().unwrap();
    assert_ne!(upgraded, first);
    assert_eq!(
        channel
            .publish(&output.reemit(2).unwrap())
            .await
            .unwrap()
            .position(),
        Some(1)
    );
    assert_eq!(channel.progress().await.unwrap().accepted, 1);
    channel.shutdown().await.unwrap();
    drop(channel);
    let channel = open(directory.path(), definition(2)).await;
    assert_eq!(channel.output_journal_identity(), Some(upgraded));
    channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn replay_configuration_cannot_race_or_follow_pipe_binding() {
    let directory = tempfile::tempdir().unwrap();
    let channel = open(directory.path(), definition(2)).await;
    let setup = channel.begin_replay_setup().unwrap();
    assert!(channel.bind("first", ReplayGapPolicy::Strict).is_err());
    drop(setup);
    let pipe = channel.bind("first", ReplayGapPolicy::Strict).unwrap();
    assert!(channel.enable_replay(options(2)).await.is_err());
    pipe.control.cancel();
    assert!(channel.enable_replay(options(2)).await.is_err());
    assert_eq!(channel.output_journal_identity(), None);
    channel.shutdown().await.unwrap();
}

#[tokio::test]
async fn replay_reconstruction_rejects_corrupt_receipts_without_modifying_storage() {
    for fault in [
        "version",
        "missing-journal",
        "nil-journal",
        "identity",
        "sequence",
        "missing",
        "position",
        "digest",
        "capacity",
        "transport",
    ] {
        let directory = tempfile::tempdir().unwrap();
        let channel = open(directory.path(), definition(2)).await;
        channel.enable_replay(options(2)).await.unwrap();
        let producer = persistent_identity();
        channel.publish(&graph_output(&producer, 1)).await.unwrap();
        channel.publish(&graph_output(&producer, 2)).await.unwrap();
        channel.shutdown().await.unwrap();
        drop(channel);
        let indexes = resources(directory.path()).await;
        let checkpoint = indexes.checkpoint_store().unwrap().clone();
        let saved = checkpoint
            .read_checkpoint("journal")
            .await
            .unwrap()
            .unwrap();
        let mut metadata: Metadata =
            serde_json::from_slice(saved.source_position.as_ref().unwrap()).unwrap();
        let corrupt_record = if fault == "transport" {
            let records = indexes
                .outbox_writer()
                .unwrap()
                .read_from("journal", 0)
                .await
                .unwrap();
            let envelope = codec().decode(&records[1].1).unwrap().reemit(1).unwrap();
            metadata.producer_sequence = Some(1);
            Some(codec().encode(&envelope).unwrap())
        } else {
            None
        };
        let replay = metadata.replay.as_mut().unwrap();
        match fault {
            "version" => replay.version = 3,
            "missing-journal" => replay.identity = None,
            "nil-journal" => replay.identity = Some(uuid::Uuid::nil()),
            "identity" => replay.producer = None,
            "sequence" => replay.sequence = 3,
            "missing" => {
                replay.receipts.remove(&1);
            }
            "position" => replay.receipts.get_mut(&1).unwrap().position = 2,
            "digest" => replay.receipts.get_mut(&1).unwrap().digest[0] ^= 1,
            "capacity" => replay.options.receipt_capacity = NonZeroUsize::new(1025).unwrap(),
            "transport" => {}
            _ => unreachable!(),
        }
        let corrupted = Bytes::from(serde_json::to_vec(&metadata).unwrap());
        let transaction = ComputationTransaction::try_new(indexes).unwrap();
        transaction
            .run(async {
                if let Some(record) = &corrupt_record {
                    transaction
                        .resources()
                        .outbox_writer()
                        .unwrap()
                        .append("journal", 2, record)
                        .await?;
                }
                checkpoint
                    .stage_checkpoint("journal", 2, Some(&corrupted))
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
        transaction.shutdown().await.unwrap();
        drop(transaction);
        drop(checkpoint);
        assert!(
            QosChannel::persistent(
                definition(2),
                resources(directory.path()).await,
                codec(),
                "journal"
            )
            .await
            .is_err(),
            "{fault}"
        );
        let indexes = resources(directory.path()).await;
        let after = indexes
            .checkpoint_store()
            .unwrap()
            .read_checkpoint("journal")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after.source_position.unwrap(), corrupted, "{fault}");
        if let Some(record) = corrupt_record {
            let records = indexes
                .outbox_writer()
                .unwrap()
                .read_from("journal", 0)
                .await
                .unwrap();
            assert_eq!(records[1].1, record);
        }
        let owner = ComputationTransaction::try_new(indexes).unwrap();
        owner.shutdown().await.unwrap();
    }
}
