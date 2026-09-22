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
use drasi_lib::{
    computation::v1::*,
    state_store::{MemoryStateStoreProvider, StateStoreProvider},
};
use std::{
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

const TEXT: &str = "MATCH (p:Person) RETURN p.name AS name";
const IDENTITY_ANNOTATION: &str = "drasi.query-recovery-identity.v1";

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).unwrap()
}

fn input(envelope: ChangeEnvelope) -> InputEnvelope {
    InputEnvelope {
        port: PortId::try_new("in").unwrap(),
        envelope,
    }
}

fn definition(graph: &str, text: &str, capacity: usize) -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: graph.into(),
        id: id("query"),
        query: text.into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: StreamId::try_new("query/out").unwrap(),
        outbox_capacity: NonZeroUsize::new(capacity).unwrap(),
    }
}

async fn query(capacity: usize) -> ContinuousQueryTransformer {
    let mut query = ContinuousQueryTransformer::new(
        definition("consumers", TEXT, capacity),
        Arc::new(InMemoryComputationProvider),
    )
    .await
    .unwrap();
    query.start().await.unwrap();
    query
}

fn change(sequence: u64, name: &str) -> ChangeEnvelope {
    let element = Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("people", "one"),
            labels: vec!["Person".into()].into(),
            effective_from: sequence,
        },
        properties: ElementPropertyMap::from(serde_json::json!({"name": name})),
    };
    GraphChangeCodec::encode_change(
        if sequence == 1 {
            SourceChange::Insert { element }
        } else {
            SourceChange::Update { element }
        },
        StreamId::try_new("people/out").unwrap(),
        sequence,
        None,
    )
    .unwrap()
}

async fn emit(query: &mut ContinuousQueryTransformer, sequence: u64, name: &str) -> ChangeEnvelope {
    query
        .transform(input(change(sequence, name)))
        .await
        .unwrap()
        .pop()
        .unwrap()
        .envelope
}

fn replay(
    results: QueryResults,
    progress: Arc<dyn ConsumerProgressStore>,
    policy: ConsumerRecoveryPolicy,
) -> QueryReplayTransformer {
    QueryReplayTransformer::new(
        id("replay"),
        "query".into(),
        StreamId::try_new("replay/out").unwrap(),
        results,
        progress,
        policy,
    )
}

#[derive(Default)]
struct Handled {
    rows: Mutex<im::HashMap<RecordId, Record>>,
    sequences: Mutex<Vec<u64>>,
    replacements: AtomicUsize,
    fail_snapshot: AtomicBool,
    fail_sequence: AtomicU64,
    block_snapshot: AtomicBool,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl Handled {
    fn apply(&self, envelope: &ChangeEnvelope, replace: bool) {
        let mut rows = self.rows.lock().unwrap();
        if replace {
            rows.clear();
        }
        for operation in envelope.changes().operations() {
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

    fn names(&self) -> Vec<String> {
        self.rows
            .lock()
            .unwrap()
            .values()
            .map(|row| {
                QueryChangeCodec::decode_row(row).unwrap().values["name"]
                    .to_string()
                    .trim_matches('"')
                    .to_owned()
            })
            .collect()
    }
}

struct Sink {
    descriptor: ComponentDescriptor,
    state: Arc<Handled>,
    completion: SinkCompletion,
}

impl Sink {
    fn new(state: Arc<Handled>) -> Self {
        Self {
            descriptor: ComponentDescriptor::try_new(
                id("sink"),
                vec![PortDescriptor::new(
                    PortId::try_new("in").unwrap(),
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .unwrap(),
            state,
            completion: SinkCompletion::Handled,
        }
    }
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
        self.completion
    }
    fn supports_snapshot(&self) -> bool {
        true
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        let sequence = QueryChangeCodec::query_sequence(&input.envelope)?;
        if self.state.fail_sequence.load(Ordering::Acquire) == sequence {
            self.state.fail_sequence.store(0, Ordering::Release);
            anyhow::bail!("injected handling failure");
        }
        self.state.apply(&input.envelope, false);
        self.state.sequences.lock().unwrap().push(sequence);
        Ok(())
    }
    async fn replace_snapshot(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        if self.state.block_snapshot.swap(false, Ordering::AcqRel) {
            self.state.entered.notify_one();
            self.state.release.notified().await;
        }
        if self.state.fail_snapshot.swap(false, Ordering::AcqRel) {
            self.state.rows.lock().unwrap().clear();
            anyhow::bail!("injected partial snapshot failure");
        }
        self.state.apply(&input.envelope, true);
        self.state.replacements.fetch_add(1, Ordering::AcqRel);
        Ok(())
    }
}

fn sink(
    progress: Arc<dyn ConsumerProgressStore>,
    state: Arc<Handled>,
    skips: bool,
) -> CheckpointedSink {
    let sink = CheckpointedSink::new(Box::new(Sink::new(state)), progress).unwrap();
    if skips {
        sink.allow_skipped_resets()
    } else {
        sink
    }
}

async fn deliver(sink: &mut CheckpointedSink, output: Vec<OutputEnvelope>) {
    for output in output {
        sink.handle(input(output.envelope)).await.unwrap();
    }
}

async fn initial(
    replay: &mut QueryReplayTransformer,
    sink: &mut CheckpointedSink,
) -> ChangeEnvelope {
    replay.start().await.unwrap();
    let envelope = replay.on_wakeup().await.unwrap().pop().unwrap().envelope;
    sink.handle(input(envelope.clone())).await.unwrap();
    envelope
}

fn checkpoint(results: &QueryResults, sequence: u64) -> ConsumerCheckpoint {
    let view = results.recovery_view(None).unwrap();
    ConsumerCheckpoint {
        sequence,
        generation: view.snapshot.generation,
        identity: view.identity,
    }
}

#[tokio::test]
async fn recreated_volatile_producers_require_explicit_recovery_even_at_identical_positions() {
    for empty in [false, true] {
        for policy in [
            ConsumerRecoveryPolicy::Strict,
            ConsumerRecoveryPolicy::AutoReset,
            ConsumerRecoveryPolicy::AutoSkipGap,
        ] {
            let mut old = query(4).await;
            let alice = emit(&mut old, 1, "Alice").await;
            let progress = Arc::new(MemoryConsumerProgress::default());
            let handled = Arc::new(Handled::default());
            let mut sink = sink(progress.clone(), handled.clone(), true);
            let mut first = replay(old.results(), progress.clone(), policy);
            let old_snapshot = initial(&mut first, &mut sink).await;
            let old_checkpoint = progress.load("query").await.unwrap().unwrap();
            old.stop().await.unwrap();

            let mut new = query(4).await;
            if !empty {
                emit(&mut new, 1, "Bob").await;
            }
            let identity = new.results().recovery_view(None).unwrap().identity;
            assert_ne!(identity, old_checkpoint.identity);
            let mut restored = replay(new.results(), progress.clone(), policy);
            restored.start().await.unwrap();
            let output = restored.on_wakeup().await;
            if policy == ConsumerRecoveryPolicy::Strict {
                assert!(output
                    .unwrap_err()
                    .downcast_ref::<QueryIdentityError>()
                    .is_some());
                assert_eq!(progress.load("query").await.unwrap(), Some(old_checkpoint));
                assert_eq!(handled.names(), ["Alice"]);
            } else {
                let output = output.unwrap();
                assert_eq!(output.len(), 1);
                if policy == ConsumerRecoveryPolicy::AutoSkipGap {
                    assert!(QueryChangeCodec::is_progress_only(&output[0].envelope));
                    assert!(output[0].envelope.changes().operations().is_empty());
                    let mut unwilling = sink_without_skips(progress.clone());
                    assert!(unwilling
                        .handle(input(output[0].envelope.clone()))
                        .await
                        .is_err());
                } else {
                    assert!(QueryChangeCodec::is_snapshot(&output[0].envelope));
                }
                deliver(&mut sink, output).await;
                let saved = progress.load("query").await.unwrap().unwrap();
                assert_eq!(saved.identity, identity);
                assert_eq!(saved.sequence, if empty { 0 } else { 1 });
                if policy == ConsumerRecoveryPolicy::AutoReset {
                    assert_eq!(
                        handled.names(),
                        if empty {
                            vec![]
                        } else {
                            vec!["Bob".to_owned()]
                        }
                    );
                } else {
                    assert_eq!(handled.names(), ["Alice"]);
                }
                assert!(sink.handle(input(alice.clone())).await.is_err());
                assert!(sink.handle(input(old_snapshot)).await.is_err());
                assert!(restored.transform(input(alice)).await.is_err());
                assert_eq!(progress.load("query").await.unwrap(), Some(saved));
            }
            new.stop().await.unwrap();
        }
    }
}

fn sink_without_skips(progress: Arc<dyn ConsumerProgressStore>) -> CheckpointedSink {
    sink(progress, Arc::new(Handled::default()), false)
}

#[tokio::test]
async fn a_dropped_live_event_is_replayed_before_the_later_event_and_duplicates_are_ignored() {
    let mut query = query(4).await;
    emit(&mut query, 1, "Alice").await;
    let progress = Arc::new(MemoryConsumerProgress::default());
    let handled = Arc::new(Handled::default());
    let mut sink = sink(progress.clone(), handled.clone(), false);
    let mut replay = replay(
        query.results(),
        progress.clone(),
        ConsumerRecoveryPolicy::Strict,
    );
    initial(&mut replay, &mut sink).await;
    let second = emit(&mut query, 2, "Bob").await;
    let third = emit(&mut query, 3, "Charlie").await;
    let output = replay.transform(input(third.clone())).await.unwrap();
    assert_eq!(
        output
            .iter()
            .map(|output| QueryChangeCodec::query_sequence(&output.envelope).unwrap())
            .collect::<Vec<_>>(),
        [2, 3]
    );
    assert_eq!(progress.load("query").await.unwrap().unwrap().sequence, 1);
    deliver(&mut sink, output).await;
    assert_eq!(handled.names(), ["Charlie"]);
    assert_eq!(*handled.sequences.lock().unwrap(), [2, 3]);
    assert_eq!(
        progress.load("query").await.unwrap(),
        Some(checkpoint(&query.results(), 3))
    );
    assert!(replay.transform(input(second)).await.unwrap().is_empty());
    assert!(replay.transform(input(third)).await.unwrap().is_empty());
    query.stop().await.unwrap();
}

#[tokio::test]
async fn checkpointed_sink_independently_rejects_unexplained_gaps_and_acceptance_only_sinks() {
    let mut query = query(4).await;
    let first = emit(&mut query, 1, "Alice").await;
    let second = emit(&mut query, 2, "Bob").await;
    let third = emit(&mut query, 3, "Charlie").await;
    for skips in [false, true] {
        let progress = Arc::new(MemoryConsumerProgress::default());
        let handled = Arc::new(Handled::default());
        let mut sink = sink(progress.clone(), handled.clone(), skips);
        sink.handle(input(first.clone())).await.unwrap();
        let error = sink.handle(input(third.clone())).await.unwrap_err();
        assert!(matches!(
            error.downcast_ref::<ConsumerRecoveryError>(),
            Some(ConsumerRecoveryError::Gap { .. })
        ));
        assert_eq!(
            progress.load("query").await.unwrap(),
            Some(checkpoint(&query.results(), 1))
        );
        assert_eq!(handled.names(), ["Alice"]);
        sink.handle(input(second.clone())).await.unwrap();
        sink.handle(input(third.clone())).await.unwrap();
        sink.handle(input(third.clone())).await.unwrap();
        assert_eq!(*handled.sequences.lock().unwrap(), [1, 2, 3]);
    }
    let mut accepted = Sink::new(Arc::new(Handled::default()));
    accepted.completion = SinkCompletion::Accepted;
    assert!(CheckpointedSink::new(
        Box::new(accepted),
        Arc::new(MemoryConsumerProgress::default()),
    )
    .is_err());
    query.stop().await.unwrap();
}

#[tokio::test]
async fn live_retention_gaps_fail_reset_or_emit_an_explicit_skip_before_retained_deltas() {
    for policy in [
        ConsumerRecoveryPolicy::Strict,
        ConsumerRecoveryPolicy::AutoReset,
        ConsumerRecoveryPolicy::AutoSkipGap,
    ] {
        let mut query = query(1).await;
        emit(&mut query, 1, "Alice").await;
        let progress = Arc::new(MemoryConsumerProgress::default());
        let handled = Arc::new(Handled::default());
        let mut sink = sink(progress.clone(), handled.clone(), true);
        let mut replay = replay(query.results(), progress.clone(), policy);
        initial(&mut replay, &mut sink).await;
        emit(&mut query, 2, "Bob").await;
        let third = emit(&mut query, 3, "Charlie").await;
        let output = replay.transform(input(third.clone())).await;
        if policy == ConsumerRecoveryPolicy::Strict {
            assert!(output
                .unwrap_err()
                .downcast_ref::<QueryHistoryError>()
                .is_some());
            assert!(replay.transform(input(third)).await.is_err());
            assert_eq!(progress.load("query").await.unwrap().unwrap().sequence, 1);
            assert_eq!(handled.names(), ["Alice"]);
        } else {
            let output = output.unwrap();
            if policy == ConsumerRecoveryPolicy::AutoReset {
                assert_eq!(output.len(), 1);
                assert!(QueryChangeCodec::is_snapshot(&output[0].envelope));
            } else {
                assert_eq!(output.len(), 2);
                assert!(QueryChangeCodec::is_progress_only(&output[0].envelope));
                assert_eq!(
                    QueryChangeCodec::query_sequence(&output[0].envelope).unwrap(),
                    2
                );
                assert_eq!(
                    ConsumerRecoveryDecision::from_envelope(&output[0].envelope)
                        .unwrap()
                        .unwrap()
                        .previous,
                    Some(checkpoint(&query.results(), 1))
                );
                assert_eq!(
                    QueryChangeCodec::query_sequence(&output[1].envelope).unwrap(),
                    3
                );
            }
            deliver(&mut sink, output).await;
            assert_eq!(handled.names(), ["Charlie"]);
            assert_eq!(progress.load("query").await.unwrap().unwrap().sequence, 3);
        }
        query.stop().await.unwrap();
    }
}

#[tokio::test]
async fn partial_handling_failure_restarts_from_the_last_successfully_handled_delta() {
    let mut query = query(4).await;
    emit(&mut query, 1, "Alice").await;
    let progress = Arc::new(MemoryConsumerProgress::default());
    let handled = Arc::new(Handled::default());
    let mut sink = sink(progress.clone(), handled.clone(), false);
    let mut replay = replay(
        query.results(),
        progress.clone(),
        ConsumerRecoveryPolicy::Strict,
    );
    initial(&mut replay, &mut sink).await;
    emit(&mut query, 2, "Bob").await;
    let third = emit(&mut query, 3, "Charlie").await;
    let output = replay.transform(input(third)).await.unwrap();
    handled.fail_sequence.store(3, Ordering::Release);
    sink.handle(input(output[0].envelope.clone()))
        .await
        .unwrap();
    assert!(sink
        .handle(input(output[1].envelope.clone()))
        .await
        .is_err());
    assert_eq!(progress.load("query").await.unwrap().unwrap().sequence, 2);
    replay.stop().await.unwrap();
    replay.start().await.unwrap();
    let retry = replay.on_wakeup().await.unwrap();
    assert_eq!(retry.len(), 1);
    assert_eq!(
        QueryChangeCodec::query_sequence(&retry[0].envelope).unwrap(),
        3
    );
    deliver(&mut sink, retry).await;
    assert_eq!(handled.names(), ["Charlie"]);
    assert_eq!(*handled.sequences.lock().unwrap(), [2, 3]);
    query.stop().await.unwrap();
}

#[tokio::test]
async fn failed_or_cancelled_replacement_never_adopts_the_new_producer_checkpoint() {
    for cancel in [false, true] {
        let mut old = query(2).await;
        emit(&mut old, 1, "Alice").await;
        let progress = Arc::new(MemoryConsumerProgress::default());
        let handled = Arc::new(Handled::default());
        let mut sink = sink(progress.clone(), handled.clone(), false);
        let mut first = replay(
            old.results(),
            progress.clone(),
            ConsumerRecoveryPolicy::AutoReset,
        );
        initial(&mut first, &mut sink).await;
        let saved = progress.load("query").await.unwrap();
        let mut replacement = query(2).await;
        emit(&mut replacement, 1, "Bob").await;
        let mut restored = replay(
            replacement.results(),
            progress.clone(),
            ConsumerRecoveryPolicy::AutoReset,
        );
        restored.start().await.unwrap();
        let reset = restored.on_wakeup().await.unwrap().pop().unwrap().envelope;
        if cancel {
            handled.block_snapshot.store(true, Ordering::Release);
            tokio::time::timeout(Duration::from_secs(5), async {
                let handling = sink.handle(input(reset));
                tokio::pin!(handling);
                tokio::select! {
                    _ = handled.entered.notified() => {},
                    result = &mut handling => panic!("snapshot completed before cancellation: {result:?}"),
                }
            }).await.unwrap();
        } else {
            handled.fail_snapshot.store(true, Ordering::Release);
            assert!(sink.handle(input(reset)).await.is_err());
        }
        assert_eq!(progress.load("query").await.unwrap(), saved);
        restored.stop().await.unwrap();
        restored.start().await.unwrap();
        deliver(&mut sink, restored.on_wakeup().await.unwrap()).await;
        assert_eq!(handled.names(), ["Bob"]);
        assert_eq!(
            progress.load("query").await.unwrap(),
            Some(checkpoint(&replacement.results(), 1))
        );
        old.stop().await.unwrap();
        replacement.stop().await.unwrap();
    }
}

fn without_identity(envelope: &ChangeEnvelope) -> ChangeEnvelope {
    let mut result = ChangeEnvelope::new(
        envelope.id().clone(),
        envelope.changes().clone(),
        envelope.system().as_ref().clone(),
    );
    let mut entries: Vec<_> = envelope.annotations().entries().collect();
    entries.reverse();
    for entry in entries {
        if entry.key() != IDENTITY_ANNOTATION {
            result.append_annotation(entry).unwrap();
        }
    }
    result
}

#[tokio::test]
async fn untrusted_missing_malformed_wrong_scope_and_stale_identity_are_not_invented_or_adopted() {
    let mut query = query(4).await;
    let valid = emit(&mut query, 1, "Alice").await;
    let owner = QueryRecoveryIdentity::from_envelope(&valid).unwrap();
    let mut malformed = valid.clone();
    malformed
        .append_annotation(
            ContextEntry::try_new(
                id("invalid"),
                IDENTITY_ANNOTATION,
                ContextValue::Bytes(Arc::from(&b"not-json"[..])),
            )
            .unwrap(),
        )
        .unwrap();
    let mut wrong_scope = valid.clone();
    QueryRecoveryIdentity::try_new(
        "other-graph",
        id("query"),
        owner.configuration_hash(),
        owner.incarnation(),
    )
    .unwrap()
    .annotate(&mut wrong_scope, &id("invalid"))
    .unwrap();
    let mut wrong_config = valid.clone();
    QueryRecoveryIdentity::try_new(
        owner.graph_id(),
        id("query"),
        owner.configuration_hash() ^ 1,
        owner.incarnation(),
    )
    .unwrap()
    .annotate(&mut wrong_config, &id("invalid"))
    .unwrap();
    for invalid in [without_identity(&valid), malformed, wrong_scope, wrong_config] {
        let progress = Arc::new(MemoryConsumerProgress::default());
        let mut replay = replay(
            query.results(),
            progress.clone(),
            ConsumerRecoveryPolicy::AutoReset,
        );
        replay.start().await.unwrap();
        assert!(replay.transform(input(invalid.clone())).await.is_err());
        assert_eq!(progress.load("query").await.unwrap(), None);
        let mut sink = sink_without_skips(progress.clone());
        sink.handle(input(valid.clone())).await.unwrap();
        assert!(sink.handle(input(invalid)).await.is_err());
        assert_eq!(
            progress.load("query").await.unwrap(),
            Some(checkpoint(&query.results(), 1))
        );
    }
    query.stop().await.unwrap();
}

#[tokio::test]
async fn unidentified_unknown_or_malformed_persisted_progress_is_rejected_without_overwrite() {
    let mut query = query(2).await;
    emit(&mut query, 1, "Alice").await;
    let checkpoint = checkpoint(&query.results(), 1);
    let record = serde_json::json!({"version": 2, "checkpoint": checkpoint});
    let mut unknown = record.clone();
    unknown["version"] = 999.into();
    let mut missing_identity = record.clone();
    missing_identity["checkpoint"]
        .as_object_mut()
        .unwrap()
        .remove("identity");
    let mut missing_incarnation = record.clone();
    missing_incarnation["checkpoint"]["identity"]
        .as_object_mut()
        .unwrap()
        .remove("incarnation");
    let mut missing_scope = record.clone();
    missing_scope["checkpoint"]["identity"]
        .as_object_mut()
        .unwrap()
        .remove("construction_scope");
    let mut wrong_query = record.clone();
    wrong_query["checkpoint"]["identity"]["query_id"] = "other".into();
    for bytes in [
        vec![0; 16],
        b"{".to_vec(),
        serde_json::to_vec(&unknown).unwrap(),
        serde_json::to_vec(&missing_identity).unwrap(),
        serde_json::to_vec(&missing_incarnation).unwrap(),
        serde_json::to_vec(&missing_scope).unwrap(),
        serde_json::to_vec(&wrong_query).unwrap(),
    ] {
        let provider = Arc::new(MemoryStateStoreProvider::new());
        provider
            .set("computation-consumer:67:63", "handled:query", bytes.clone())
            .await
            .unwrap();
        let progress =
            Arc::new(StateStoreConsumerProgress::new("g", "c", provider.clone()).unwrap());
        assert!(progress.load("query").await.is_err());
        assert!(progress
            .commit_handled("query", checkpoint.clone())
            .await
            .is_err());
        let mut replay = replay(query.results(), progress, ConsumerRecoveryPolicy::AutoReset);
        replay.start().await.unwrap();
        assert!(replay.on_wakeup().await.is_err());
        assert_eq!(
            provider
                .get("computation-consumer:67:63", "handled:query")
                .await
                .unwrap(),
            Some(bytes)
        );
    }

    query.stop().await.unwrap();
}

#[cfg(feature = "computation-rocksdb-tests")]
mod construction_scopes {
    use super::*;
    use drasi_core::interface::{CreatedIndexes, IndexBackendPlugin, IndexError, OutboxWriter};
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use drasi_lib::{ComponentStatus, DrasiLib, ExecutionMode, Query, StorageBackendRef};
    use drasi_source_application::{
        ApplicationSource, ApplicationSourceConfig, PropertyMapBuilder,
    };
    use std::{collections::HashMap, path::Path, result::Result};

    type Captured = Arc<Mutex<Vec<(String, Vec<u8>)>>>;

    struct CaptureProvider {
        inner: RocksDbIndexProvider,
        captured: Captured,
    }

    #[async_trait]
    impl IndexBackendPlugin for CaptureProvider {
        async fn create_indexes(&self, query: &str) -> Result<CreatedIndexes, IndexError> {
            self.create_scoped_indexes(query, query).await
        }

        async fn create_scoped_indexes(
            &self,
            scope: &str,
            query: &str,
        ) -> Result<CreatedIndexes, IndexError> {
            let mut indexes = self.inner.create_scoped_indexes(scope, query).await?;
            indexes.outbox_writer = indexes.outbox_writer.map(|inner| {
                Arc::new(CaptureOutbox {
                    inner,
                    scope: scope.to_owned(),
                    captured: self.captured.clone(),
                }) as Arc<dyn OutboxWriter>
            });
            Ok(indexes)
        }

        fn is_volatile(&self) -> bool {
            self.inner.is_volatile()
        }

        fn supports_atomic_query_output(&self) -> bool {
            self.inner.supports_atomic_query_output()
        }
    }

    struct CaptureOutbox {
        inner: Arc<dyn OutboxWriter>,
        scope: String,
        captured: Captured,
    }

    impl CaptureOutbox {
        fn record(&self, query: &str, data: &[u8]) {
            if query == "query" {
                self.captured
                    .lock()
                    .unwrap()
                    .push((self.scope.clone(), data.to_vec()));
            }
        }
    }

    #[async_trait]
    impl OutboxWriter for CaptureOutbox {
        async fn append(&self, query: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
            self.inner.append(query, sequence, data).await?;
            self.record(query, data);
            Ok(())
        }

        async fn append_and_trim(
            &self,
            query: &str,
            sequence: u64,
            data: &[u8],
            retain_from: u64,
        ) -> Result<usize, IndexError> {
            let count = self
                .inner
                .append_and_trim(query, sequence, data, retain_from)
                .await?;
            self.record(query, data);
            Ok(count)
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

        async fn trim_before(&self, query: &str, sequence: u64) -> Result<usize, IndexError> {
            self.inner.trim_before(query, sequence).await
        }

        async fn trim_to_capacity(
            &self,
            query: &str,
            capacity: usize,
        ) -> Result<usize, IndexError> {
            self.inner.trim_to_capacity(query, capacity).await
        }
    }

    async fn ordinary_output(
        root: &Path,
        instance: &str,
        name: &str,
        head: u64,
        provider: Arc<CaptureProvider>,
    ) -> anyhow::Result<(String, ChangeEnvelope)> {
        let (source, handle) = ApplicationSource::new(
            "people",
            ApplicationSourceConfig {
                properties: HashMap::new(),
                durability: Some(drasi_lib::DurabilityConfig {
                    enabled: true,
                    ..Default::default()
                }),
            },
        )?;
        let before = provider.captured.lock().unwrap().len();
        let core = DrasiLib::builder()
            .with_id(instance)
            .with_execution_mode(ExecutionMode::ComputationGraph)
            .with_index_provider("rocks", provider.clone())
            .with_wal_provider(Arc::new(drasi_wal_redb::RedbWalProvider::new(
                root.join(format!("wal-{instance}")),
            )))
            .with_source(source)
            .with_query(
                Query::cypher("query")
                    .query(TEXT)
                    .from_source("people")
                    .enable_bootstrap(false)
                    .with_storage_backend(StorageBackendRef::Named("rocks".into()))
                    .build(),
            )
            .build()
            .await?;
        core.start().await?;
        tokio::time::timeout(Duration::from_secs(15), async {
            while core.get_query_status("query").await? != ComponentStatus::Running {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        handle
            .send_node_insert(
                name,
                vec!["Person"],
                PropertyMapBuilder::new().with_string("name", name).build(),
            )
            .await?;
        tokio::time::timeout(Duration::from_secs(15), async {
            let query = core
                .query_manager()
                .get_query_instance("query")
                .await
                .map_err(anyhow::Error::msg)?;
            loop {
                let snapshot = query.fetch_snapshot().await?;
                if snapshot.as_of_sequence == head {
                    break;
                }
                anyhow::ensure!(snapshot.as_of_sequence < head, "unexpected output sequence");
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await??;
        let (scope, bytes) = {
            let captured = provider.captured.lock().unwrap();
            assert!(captured.len() > before);
            captured.last().unwrap().clone()
        };
        let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1 << 20).unwrap());
        codec.register_schema(QueryChangeCodec::schema())?;
        let output = codec.decode(&bytes)?;
        core.shutdown().await?;
        drop(handle);
        drop(core);
        Ok((scope, output))
    }

    #[tokio::test]
    async fn ordinary_persistent_producers_bind_instance_scope_and_keep_it_across_reopen() {
        std::fs::create_dir_all("target/computation-consumer-recovery").unwrap();
        let directory = tempfile::Builder::new()
            .tempdir_in("target/computation-consumer-recovery")
            .unwrap();
        let provider = Arc::new(CaptureProvider {
            inner: RocksDbIndexProvider::new(directory.path().join("indexes"), false, false),
            captured: Arc::default(),
        });
        let (scope_a, alice) =
            ordinary_output(directory.path(), "alpha", "Alice", 1, provider.clone())
                .await
                .unwrap();
        let (scope_b, bob) = ordinary_output(directory.path(), "beta", "Bob", 1, provider.clone())
            .await
            .unwrap();
        let a = QueryRecoveryIdentity::from_envelope(&alice).unwrap();
        let b = QueryRecoveryIdentity::from_envelope(&bob).unwrap();
        assert_eq!(QueryChangeCodec::query_sequence(&alice).unwrap(), 1);
        assert_eq!(QueryChangeCodec::query_sequence(&bob).unwrap(), 1);
        assert_eq!(
            QueryChangeCodec::query_generation(&alice).unwrap(),
            QueryChangeCodec::query_generation(&bob).unwrap(),
        );
        assert_ne!(
            scope_a, scope_b,
            "the backend partitions really are independent"
        );
        assert_eq!(
            a.graph_id(),
            b.graph_id(),
            "ordinary query graph names are local"
        );
        assert_eq!(a.query_id(), b.query_id());
        assert_eq!(a.configuration_hash(), b.configuration_hash());
        assert!(a.incarnation().is_none() && b.incarnation().is_none());
        assert_eq!(a.construction_scope(), "alpha");
        assert_eq!(b.construction_scope(), "beta");
        assert_ne!(a, b);
        let progress = Arc::new(MemoryConsumerProgress::default());
        let handled = Arc::new(Handled::default());
        let mut sink = sink(progress.clone(), handled.clone(), false);
        sink.handle(input(alice)).await.unwrap();
        assert!(sink.handle(input(bob)).await.is_err());
        assert_eq!(handled.names(), ["Alice"]);
        assert_eq!(progress.load("query").await.unwrap().unwrap().identity, a);

        let (reopened_scope, reopened) =
            ordinary_output(directory.path(), "alpha", "Alicia", 2, provider)
                .await
                .unwrap();
        assert_eq!(reopened_scope, scope_a);
        assert_eq!(QueryRecoveryIdentity::from_envelope(&reopened).unwrap(), a);
        sink.handle(input(reopened)).await.unwrap();
        assert_eq!(progress.load("query").await.unwrap().unwrap().sequence, 2);
    }
}

#[tokio::test]
async fn authorized_new_incarnations_can_reset_lower_but_stale_commits_and_decisions_cannot() {
    let mut old = query(2).await;
    emit(&mut old, 1, "Alice").await;
    let mut new = query(2).await;
    let previous = checkpoint(&old.results(), 7);
    let target = checkpoint(&new.results(), 0);
    let provider = Arc::new(MemoryStateStoreProvider::new());
    let stores: Vec<Arc<dyn ConsumerProgressStore>> = vec![
        Arc::new(MemoryConsumerProgress::default()),
        Arc::new(StateStoreConsumerProgress::new("g", "c", provider.clone()).unwrap()),
    ];
    for progress in stores {
        progress
            .commit_handled("query", previous.clone())
            .await
            .unwrap();
        assert!(progress
            .commit_handled("query", target.clone())
            .await
            .is_err());
        progress
            .commit_recovery("query", Some(previous.clone()), target.clone())
            .await
            .unwrap();
        assert!(progress
            .commit_handled("query", previous.clone())
            .await
            .is_err());
        assert!(progress
            .commit_recovery("query", Some(previous.clone()), previous.clone())
            .await
            .is_err());
        assert_eq!(progress.load("query").await.unwrap(), Some(target.clone()));
    }
    let reopened = StateStoreConsumerProgress::new("g", "c", provider.clone()).unwrap();
    assert_eq!(reopened.load("query").await.unwrap(), Some(target));
    provider
        .set(
            "computation-consumer:67:63",
            "producer:replay/out",
            u64::MAX.to_be_bytes().to_vec(),
        )
        .await
        .unwrap();
    assert!(reopened
        .allocate_sequence(&StreamId::try_new("replay/out").unwrap())
        .await
        .is_err());
    old.stop().await.unwrap();
    new.stop().await.unwrap();
}

#[tokio::test]
async fn consumer_identity_and_producer_sequence_survive_an_actual_state_store_reopen() {
    std::fs::create_dir_all("target/computation-consumer-recovery").unwrap();
    let directory = tempfile::Builder::new()
        .tempdir_in("target/computation-consumer-recovery")
        .unwrap();
    let path = directory.path().join("consumer.redb");
    let mut query = query(2).await;
    emit(&mut query, 1, "Alice").await;
    let saved = checkpoint(&query.results(), 1);
    let stream = StreamId::try_new("replay/out").unwrap();
    {
        let provider =
            Arc::new(drasi_state_store_redb::RedbStateStoreProvider::new(&path).unwrap());
        let progress = StateStoreConsumerProgress::new("graph", "consumer", provider).unwrap();
        progress
            .commit_handled("query", saved.clone())
            .await
            .unwrap();
        assert_eq!(progress.allocate_sequence(&stream).await.unwrap(), 1);
    }
    {
        let provider =
            Arc::new(drasi_state_store_redb::RedbStateStoreProvider::new(&path).unwrap());
        let progress = StateStoreConsumerProgress::new("graph", "consumer", provider).unwrap();
        assert_eq!(progress.load("query").await.unwrap(), Some(saved));
        assert_eq!(progress.allocate_sequence(&stream).await.unwrap(), 2);
    }
    query.stop().await.unwrap();
}

struct MutableBootstrap(Arc<Mutex<Vec<ChangeEnvelope>>>);

#[async_trait]
impl ComputationBootstrapProvider for MutableBootstrap {
    async fn snapshot(&self) -> anyhow::Result<ComputationBootstrapSnapshot> {
        let rows = self.0.lock().unwrap().clone();
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::iter(rows.into_iter().map(Ok))),
            watermarks: Vec::new(),
        })
    }
}

#[tokio::test]
async fn live_query_reset_wakes_recovery_and_replaces_with_an_empty_snapshot() {
    let rows = Arc::new(Mutex::new(vec![change(1, "Alice")]));
    let mut query = ContinuousQueryTransformer::new_with_options(
        definition("consumers", TEXT, 2),
        Arc::new(InMemoryComputationProvider),
        QueryOptions {
            recovery: QueryRecoveryPolicy::AutoReset,
            publication: QueryPublicationMode::Atomic,
        },
    )
    .await
    .unwrap()
    .with_bootstrap(Arc::new(MutableBootstrap(rows.clone())));
    query.start().await.unwrap();
    let progress = Arc::new(MemoryConsumerProgress::default());
    let handled = Arc::new(Handled::default());
    let mut sink = sink(progress.clone(), handled.clone(), false);
    let mut replay = replay(
        query.results(),
        progress.clone(),
        ConsumerRecoveryPolicy::AutoReset,
    );
    let initial = initial(&mut replay, &mut sink).await;
    let old = emit(&mut query, 1, "Alicia").await;
    deliver(
        &mut sink,
        replay.transform(input(old.clone())).await.unwrap(),
    )
    .await;
    let previous = progress.load("query").await.unwrap().unwrap();
    rows.lock().unwrap().clear();
    assert!(query.transform(input(initial)).await.is_err());
    query.stop().await.unwrap();
    query.start().await.unwrap();
    let wakeup = replay.wakeup_source().unwrap();
    assert!(wakeup.has_pending().await.unwrap());
    tokio::time::timeout(Duration::from_secs(5), wakeup.wait())
        .await
        .unwrap()
        .unwrap();
    let output = replay.on_wakeup().await.unwrap();
    assert_eq!(output.len(), 1);
    assert!(QueryChangeCodec::is_snapshot(&output[0].envelope));
    assert!(output[0].envelope.changes().operations().is_empty());
    deliver(&mut sink, output).await;
    let recovered = progress.load("query").await.unwrap().unwrap();
    assert_eq!(recovered.identity, previous.identity);
    assert!(recovered.generation > previous.generation);
    assert_eq!(recovered.sequence, previous.sequence);
    assert!(handled.names().is_empty());
    assert!(sink.handle(input(old)).await.is_err());
    query.stop().await.unwrap();
}

#[cfg(feature = "computation-rocksdb-tests")]
#[tokio::test]
async fn persistent_reopen_keeps_identity_and_owner_verified_old_outbox_gets_in_memory_identity() {
    use drasi_index_rocksdb::{
        computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
    };
    std::fs::create_dir_all("target/computation-consumer-recovery").unwrap();
    let directory = tempfile::Builder::new()
        .tempdir_in("target/computation-consumer-recovery")
        .unwrap();
    let provider: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
        directory.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).unwrap(),
        ),
    ));
    let identity = {
        let mut first =
            ContinuousQueryTransformer::new(definition("persistent", TEXT, 4), provider.clone())
                .await
                .unwrap();
        first.start().await.unwrap();
        let output = emit(&mut first, 1, "Alice").await;
        let identity = first.results().recovery_view(None).unwrap().identity;
        assert!(identity.incarnation().is_none());
        assert_eq!(
            QueryRecoveryIdentity::from_envelope(&output).unwrap(),
            identity
        );
        first.stop().await.unwrap();
        identity
    };
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(1 << 20).unwrap());
    codec.register_schema(QueryChangeCodec::schema()).unwrap();
    {
        let resources = provider
            .create_indexes("persistent", "query")
            .await
            .unwrap();
        let outbox = resources.outbox_writer().unwrap();
        let stored = outbox.read_from("query", 0).await.unwrap();
        let unidentified = without_identity(&codec.decode(&stored[0].1).unwrap());
        resources.indexes().session_control.begin().await.unwrap();
        outbox
            .append("query", 1, &codec.encode(&unidentified).unwrap())
            .await
            .unwrap();
        resources.indexes().session_control.commit().await.unwrap();
        resources.cleanup().unwrap().shutdown().await.unwrap();
    }
    let mut reopened =
        ContinuousQueryTransformer::new(definition("persistent", TEXT, 4), provider.clone())
            .await
            .unwrap();
    reopened.start().await.unwrap();
    let view = reopened.results().recovery_view(Some(0)).unwrap();
    assert_eq!(view.identity, identity);
    assert_eq!(view.snapshot.as_of_sequence, 1);
    let output = &view.retained.as_ref().unwrap()[0];
    assert_eq!(
        QueryRecoveryIdentity::from_envelope(output).unwrap(),
        identity
    );
    reopened.stop().await.unwrap();
    drop(reopened);
    {
        let resources = provider
            .create_indexes("persistent", "query")
            .await
            .unwrap();
        let stored = resources
            .outbox_writer()
            .unwrap()
            .read_from("query", 0)
            .await
            .unwrap();
        assert!(
            QueryRecoveryIdentity::from_envelope(&codec.decode(&stored[0].1).unwrap()).is_err(),
            "old persisted bytes are not migrated"
        );
        resources.indexes().session_control.begin().await.unwrap();
        let checkpoints = resources.checkpoint_store().unwrap();
        checkpoints.clear_checkpoints().await.unwrap();
        checkpoints.stage_result_sequence("query", 1).await.unwrap();
        resources.indexes().session_control.commit().await.unwrap();
        resources.cleanup().unwrap().shutdown().await.unwrap();
    }
    for _ in 0..2 {
        let mut unverified =
            ContinuousQueryTransformer::new(definition("persistent", TEXT, 4), provider.clone())
                .await
                .unwrap();
        let error = unverified.start().await.unwrap_err();
        assert!(
            error.to_string().contains("verified owner configuration"),
            "{error:#}"
        );
        unverified.stop().await.unwrap();
    }
}
