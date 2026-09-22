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

#![cfg(all(
    feature = "computation-rocksdb-tests",
    any(feature = "middleware-unwind", feature = "middleware-all")
))]

use std::{
    collections::{BTreeMap, HashMap},
    num::NonZeroUsize,
    path::Path,
    result::Result,
    sync::{
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource,
        InMemoryComputationProvider, TransactionDomain,
    },
    interface::{
        CheckpointStore, ElementIndex, ElementStream, IndexError, IndexSet, MiddlewareError,
        MiddlewareSetupError, OutboxWriter, SessionControl, SourceCheckpoint, SourceMiddleware,
        SourceMiddlewareFactory,
    },
    middleware::MiddlewareTypeRegistry,
    models::{
        Element, ElementMetadata, ElementPropertyMap, ElementReference, QueryJoin, SourceChange,
        SourceMiddlewareConfig,
    },
    path_solver::match_path::MatchPath,
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::{
    channels::{SourceEvent, SourceEventWrapper},
    computation::v1::*,
    profiling::ProfilingMetadata,
};
use serde_json::json;

const GRAPH: &str = "middleware-recovery";

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("component ID")
}
fn stream(value: &str) -> StreamId {
    StreamId::try_new(format!("{value}/out")).expect("stream ID")
}
fn capacity(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("nonzero capacity")
}
fn options(size: usize) -> DurableMiddlewareOptions {
    DurableMiddlewareOptions {
        graph_id: GRAPH.into(),
        outbox_capacity: capacity(size),
    }
}
fn scratch() -> tempfile::TempDir {
    let path = std::env::var_os("DRASI_TEST_ARTIFACTS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/test-artifacts"));
    std::fs::create_dir_all(&path).expect("test artifact directory");
    tempfile::Builder::new()
        .prefix("middleware-recovery-")
        .tempdir_in(path)
        .expect("isolated test storage")
}

#[derive(Clone, Copy)]
enum Backend {
    Computation,
    Plugin,
}
fn provider(path: &Path, backend: Backend) -> Arc<dyn ComputationIndexProvider> {
    match backend {
        Backend::Computation => Arc::new(RocksDbComputationProvider::new(
            path,
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20).expect("memory budget"),
            ),
        )),
        Backend::Plugin => LegacyIndexProviderAdapter::new(Arc::new(
            drasi_index_rocksdb::RocksDbIndexProvider::new(path, false, false),
        )),
    }
}

#[derive(Default)]
struct ProbeState {
    calls: AtomicUsize,
    mode: AtomicU8,
    entered: tokio::sync::Notify,
}
struct Probe(Arc<ProbeState>);

impl SourceMiddlewareFactory for Probe {
    fn name(&self) -> String {
        "probe".into()
    }
    fn create(
        &self,
        _: &SourceMiddlewareConfig,
    ) -> Result<Arc<dyn SourceMiddleware>, MiddlewareSetupError> {
        Ok(Arc::new(Self(self.0.clone())))
    }
}

#[async_trait]
impl SourceMiddleware for Probe {
    async fn process(
        &self,
        change: SourceChange,
        _: &dyn ElementIndex,
    ) -> Result<Vec<SourceChange>, MiddlewareError> {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        if change.get_reference().element_id.as_ref() == "bad" {
            match self.0.mode.load(Ordering::SeqCst) {
                1 => return Err(MiddlewareError::SourceChangeError("injected".into())),
                2 => {
                    self.0.entered.notify_one();
                    std::future::pending::<()>().await;
                }
                _ => {}
            }
        }
        if self.0.mode.load(Ordering::SeqCst) == 3 {
            Ok(Vec::new())
        } else {
            Ok(vec![change])
        }
    }
}

fn registry(probe: Arc<ProbeState>) -> Arc<MiddlewareTypeRegistry> {
    let mut registry = MiddlewareTypeRegistry::new();
    registry.register(Arc::new(drasi_middleware::unwind::UnwindFactory::new()));
    registry.register(Arc::new(Probe(probe)));
    Arc::new(registry)
}
fn config(kind: &str, name: &str, value: serde_json::Value) -> SourceMiddlewareConfig {
    SourceMiddlewareConfig::new(
        kind,
        name,
        value.as_object().expect("configuration object").clone(),
    )
}
fn definition() -> MiddlewareTransformerDefinition {
    MiddlewareTransformerDefinition {
        id: id("middleware"),
        output_stream: stream("middleware"),
        middleware: vec![
            config("probe", "probe", json!({})),
            config(
                "unwind",
                "children",
                json!({"Parent": [{"selector":"$.items[*]","label":"Child","key":"$.id","relation":"OWNS"}]}),
            ),
        ],
        pipeline: vec!["probe".into(), "children".into()],
    }
}
async fn open(
    provider: Arc<dyn ComputationIndexProvider>,
    registry: Arc<MiddlewareTypeRegistry>,
    definition: MiddlewareTransformerDefinition,
    size: usize,
) -> MiddlewareTransformer {
    let mut transformer =
        MiddlewareTransformer::new_durable(definition, registry, provider, options(size))
            .await
            .expect("durable middleware");
    transformer.start().await.expect("recover middleware");
    transformer
}
async fn query(provider: Arc<dyn ComputationIndexProvider>) -> ContinuousQueryTransformer {
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: GRAPH.into(),
            id: id("query"),
            query: "MATCH (n:Child) RETURN n.id AS id".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("query"),
            outbox_capacity: capacity(32),
        },
        provider,
    )
    .await
    .expect("durable query");
    query.start().await.expect("recover query");
    query
}
fn parent(source: &str, key: &str, time: u64, items: serde_json::Value) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new(source, key),
            labels: Arc::from([Arc::from("Parent")]),
            effective_from: time,
        },
        properties: ElementPropertyMap::from(json!({"items":items, "kept":42})),
    }
}
fn change(sequence: u64, items: &[&str], update: bool) -> SourceChange {
    let items: Vec<_> = items.iter().map(|id| json!({"id": id})).collect();
    let element = parent("source", "parent", sequence, json!(items));
    if update {
        SourceChange::Update { element }
    } else {
        SourceChange::Insert { element }
    }
}
fn event(
    source: &str,
    sequence: u64,
    emission: u64,
    change: SourceChange,
    raw: bool,
) -> ChangeEnvelope {
    if raw {
        GraphChangeCodec::encode_source_event(
            Arc::new(SourceEventWrapper {
                source_id: source.into(),
                event: SourceEvent::Change(change),
                timestamp: chrono::DateTime::from_timestamp_millis(1_000 + sequence as i64)
                    .expect("event timestamp"),
                sequence,
                source_position: Some(Bytes::copy_from_slice(&sequence.to_be_bytes())),
                profiling: Some(ProfilingMetadata {
                    source_ns: Some(55),
                    ..Default::default()
                }),
            }),
            &id(source),
            stream(source),
            emission,
            None,
        )
        .expect("raw source event")
    } else {
        GraphChangeCodec::encode_change(change, stream(source), sequence, None)
            .expect("graph event")
    }
}
fn input(envelope: ChangeEnvelope) -> InputEnvelope {
    InputEnvelope {
        port: PortId::try_new("in").expect("input port"),
        envelope,
    }
}
fn decoded(envelope: &ChangeEnvelope) -> Vec<SourceChange> {
    GraphChangeCodec::decode_changes(envelope).expect("typed graph changes")
}
fn logical(envelope: &ChangeEnvelope) -> u64 {
    GraphProducerProgress::from_envelope(envelope)
        .expect("valid producer progress")
        .expect("producer progress annotation")
        .sequence()
}
fn assert_deleted(output: &ChangeEnvelope, suffix: &str) {
    assert!(
        decoded(output).iter().any(|change| {
            matches!(change, SourceChange::Delete { metadata } if metadata.reference.element_id.ends_with(suffix))
        }),
        "expected deletion of {suffix}: {:?}",
        decoded(output)
    );
}
async fn apply(query: &mut ContinuousQueryTransformer, output: &[OutputEnvelope]) -> usize {
    for output in output {
        query
            .transform(input(output.envelope.clone()))
            .await
            .expect("query accepts middleware output");
    }
    query
        .results()
        .snapshot()
        .expect("committed query snapshot")
        .rows
        .len()
}

#[tokio::test]
async fn unwind_reopen_and_lost_ack_replay_remove_children_without_rerunning_pipeline() {
    for backend in [Backend::Computation, Backend::Plugin] {
        for raw in [false, true] {
            let directory = scratch();
            let provider = provider(directory.path(), backend);
            let calls = Arc::new(ProbeState::default());
            let registry = registry(calls.clone());
            let mut middleware = open(provider.clone(), registry.clone(), definition(), 4).await;
            let mut query = query(provider.clone()).await;
            let first = event("source", 1, 10, change(1, &["a", "b"], false), raw);
            let output = middleware.transform(input(first)).await.unwrap();
            assert_eq!(apply(&mut query, &output).await, 2);
            middleware.delivery_completed(&output).await.unwrap();
            middleware.stop().await.unwrap();
            query.stop().await.unwrap();
            drop((middleware, query));

            let mut middleware = open(provider.clone(), registry.clone(), definition(), 4).await;
            let mut query = self::query(provider.clone()).await;
            assert!(!middleware.has_pending_emissions());
            let update = event("source", 2, 1, change(2, &["a"], true), raw);
            let output = middleware.transform(input(update.clone())).await.unwrap();
            assert_eq!(logical(&output[0].envelope), 2);
            assert_deleted(&output[0].envelope, "-b");
            assert_eq!(apply(&mut query, &output).await, 1);
            middleware.stop().await.unwrap();
            query.stop().await.unwrap();
            drop((middleware, query));

            let evaluated = calls.calls.load(Ordering::SeqCst);
            let mut middleware = open(provider.clone(), registry.clone(), definition(), 4).await;
            let mut query = self::query(provider.clone()).await;
            let replay = middleware.on_wakeup().await.unwrap();
            assert_eq!(calls.calls.load(Ordering::SeqCst), evaluated);
            assert_eq!(decoded(&replay[0].envelope), decoded(&output[0].envelope));
            assert!(
                replay[0].envelope.system().sequence() > output[0].envelope.system().sequence()
            );
            assert_eq!(
                replay[0].envelope.changes().id(),
                output[0].envelope.changes().id()
            );
            assert_eq!(
                replay[0].envelope.lineage().unwrap().envelope_id(),
                update.id()
            );
            assert_eq!(
                GraphChangeCodec::source_metadata(&replay[0].envelope).unwrap(),
                GraphChangeCodec::source_metadata(&update).unwrap()
            );
            assert!(query
                .transform(input(replay[0].envelope.clone()))
                .await
                .unwrap()
                .is_empty());
            middleware.delivery_completed(&replay).await.unwrap();
            let duplicate = middleware.transform(input(update)).await.unwrap();
            assert_eq!(
                decoded(&duplicate[0].envelope),
                decoded(&output[0].envelope)
            );
            assert_eq!(logical(&duplicate[0].envelope), 2);
            assert_eq!(calls.calls.load(Ordering::SeqCst), evaluated);
            assert!(query
                .transform(input(duplicate[0].envelope.clone()))
                .await
                .unwrap()
                .is_empty());
            middleware.delivery_completed(&duplicate).await.unwrap();
            let deletion = SourceChange::Delete {
                metadata: parent("source", "parent", 3, json!([]))
                    .get_metadata()
                    .clone(),
            };
            let output = middleware
                .transform(input(event("source", 3, 2, deletion, raw)))
                .await
                .unwrap();
            assert_deleted(&output[0].envelope, "-a");
            assert_eq!(apply(&mut query, &output).await, 0);
            middleware.delivery_completed(&output).await.unwrap();
            middleware.stop().await.unwrap();
            query.stop().await.unwrap();
        }
    }
}

#[tokio::test]
async fn pending_commit_is_replayed_before_a_new_live_input_and_survives_clean_restart() {
    let directory = scratch();
    let provider = provider(directory.path(), Backend::Computation);
    let registry = registry(Arc::new(ProbeState::default()));
    let mut middleware = open(provider.clone(), registry.clone(), definition(), 2).await;
    let first = middleware
        .transform(input(event(
            "source",
            1,
            1,
            change(1, &["a", "b"], false),
            false,
        )))
        .await
        .unwrap();
    middleware.stop().await.unwrap();
    drop(middleware);

    let mut middleware = open(provider.clone(), registry.clone(), definition(), 2).await;
    let mut query = query(provider).await;
    let output = middleware
        .transform(input(event("source", 2, 2, change(2, &["a"], true), false)))
        .await
        .unwrap();
    assert_eq!(logical(&output[0].envelope), 1);
    assert_eq!(decoded(&output[0].envelope), decoded(&first[0].envelope));
    assert_eq!(apply(&mut query, &output).await, 2);
    middleware.delivery_completed(&output).await.unwrap();
    assert!(middleware.has_pending_emissions());
    let output = middleware.continue_transform().await.unwrap();
    assert_eq!(logical(&output[0].envelope), 2);
    assert_deleted(&output[0].envelope, "-b");
    assert_eq!(apply(&mut query, &output).await, 1);
    middleware.delivery_completed(&output).await.unwrap();
    assert!(!middleware.has_pending_emissions());
    middleware.stop().await.unwrap();
    middleware.start().await.unwrap();
    assert!(!middleware.has_pending_emissions());
    let duplicate = middleware
        .transform(input(event("source", 2, 2, change(2, &["a"], true), false)))
        .await
        .unwrap();
    assert!(duplicate[0].envelope.system().sequence() > output[0].envelope.system().sequence());
    assert_eq!(logical(&duplicate[0].envelope), 2);
    middleware.delivery_completed(&duplicate).await.unwrap();
    middleware.stop().await.unwrap();
    query.stop().await.unwrap();
}

#[tokio::test]
async fn retention_full_never_evicts_unconfirmed_output_and_can_retry_after_confirmation() {
    let directory = scratch();
    let provider = provider(directory.path(), Backend::Computation);
    let registry = registry(Arc::new(ProbeState::default()));
    let mut middleware = open(provider.clone(), registry.clone(), definition(), 1).await;
    let first = middleware
        .transform(input(event(
            "source",
            1,
            1,
            change(1, &["a", "b"], false),
            false,
        )))
        .await
        .unwrap();
    let update = event("source", 2, 2, change(2, &["a"], true), false);
    let error = middleware
        .transform(input(update.clone()))
        .await
        .unwrap_err();
    assert!(matches!(
        error.downcast_ref::<MiddlewareRecoveryError>(),
        Some(MiddlewareRecoveryError::RetentionExhausted)
    ));
    middleware.delivery_completed(&first).await.unwrap();
    let output = middleware.transform(input(update)).await.unwrap();
    assert_eq!(logical(&output[0].envelope), 2);
    assert_deleted(&output[0].envelope, "-b");
    middleware.stop().await.unwrap();
    drop(middleware);
    let mut middleware = open(provider, registry, definition(), 1).await;
    let replay = middleware.on_wakeup().await.unwrap();
    assert_eq!(decoded(&replay[0].envelope), decoded(&output[0].envelope));
    middleware.delivery_completed(&replay).await.unwrap();
    assert!(middleware
        .transform(input(event(
            "source",
            1,
            1,
            change(1, &["a", "b"], false),
            false
        )))
        .await
        .unwrap()
        .is_empty());
    middleware.stop().await.unwrap();
}

#[tokio::test]
async fn partial_batch_failure_and_cancellation_roll_back_prior_element_mutations() {
    for cancel in [false, true] {
        let directory = scratch();
        let provider = provider(directory.path(), Backend::Computation);
        let probe = Arc::new(ProbeState::default());
        let registry = registry(probe.clone());
        let mut middleware = open(provider.clone(), registry.clone(), definition(), 4).await;
        let first = middleware
            .transform(input(event(
                "source",
                1,
                1,
                change(1, &["a", "b"], false),
                false,
            )))
            .await
            .unwrap();
        middleware.delivery_completed(&first).await.unwrap();
        probe
            .mode
            .store(if cancel { 2 } else { 1 }, Ordering::SeqCst);
        let batch = GraphChangeCodec::encode_changes(
            &[
                change(2, &["a"], true),
                SourceChange::Insert {
                    element: parent("source", "bad", 2, json!([])),
                },
            ],
            stream("source"),
            2,
            None,
        )
        .unwrap();
        if cancel {
            let mut work = Box::pin(middleware.transform(input(batch)));
            tokio::select! {
                result = &mut work => panic!("must suspend: {result:?}"),
                _ = probe.entered.notified() => {}
            }
            drop(work);
        } else {
            assert!(middleware.transform(input(batch)).await.is_err());
        }
        middleware.stop().await.unwrap();
        assert!(middleware.start().await.is_err());
        drop(middleware);
        probe.mode.store(0, Ordering::SeqCst);
        let mut middleware = open(provider, registry, definition(), 4).await;
        assert!(!middleware.has_pending_emissions());
        let output = middleware
            .transform(input(event("source", 2, 2, change(2, &["a"], true), false)))
            .await
            .unwrap();
        assert_eq!(logical(&output[0].envelope), 2);
        assert_eq!(output[0].envelope.system().sequence(), 2);
        assert_deleted(&output[0].envelope, "-b");
        middleware.delivery_completed(&output).await.unwrap();
        middleware.stop().await.unwrap();
    }
}

#[tokio::test]
async fn durable_mode_rejects_volatile_resources_and_persistent_queries_reject_volatile_middleware()
{
    let registry = registry(Arc::new(ProbeState::default()));
    assert!(MiddlewareTransformer::new_durable(
        definition(),
        registry.clone(),
        Arc::new(InMemoryComputationProvider),
        options(2),
    )
    .await
    .is_err());
    let directory = scratch();
    let provider = provider(directory.path(), Backend::Computation);
    let mut middleware = MiddlewareTransformer::new(definition(), registry).unwrap();
    middleware.start().await.unwrap();
    let output = middleware
        .transform(input(event("source", 1, 1, change(1, &["a"], false), true)))
        .await
        .unwrap();
    let mut query = query(provider).await;
    let error = query
        .transform(input(output[0].envelope.clone()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("volatile middleware"));
    assert!(query.results().snapshot().unwrap().rows.is_empty());
    query.stop().await.unwrap();
    middleware.stop().await.unwrap();
}

#[tokio::test]
async fn configuration_and_retained_input_identity_mismatches_are_rejected() {
    let directory = scratch();
    let provider = provider(directory.path(), Backend::Computation);
    let registry = registry(Arc::new(ProbeState::default()));
    let mut middleware = open(provider.clone(), registry.clone(), definition(), 2).await;
    let first = middleware
        .transform(input(event("source", 1, 1, change(1, &["a"], false), true)))
        .await
        .unwrap();
    middleware.delivery_completed(&first).await.unwrap();
    middleware.stop().await.unwrap();
    drop(middleware);
    let mut changed = definition();
    changed.pipeline = vec!["probe".into()];
    let mut middleware =
        MiddlewareTransformer::new_durable(changed, registry.clone(), provider.clone(), options(2))
            .await
            .unwrap();
    assert!(middleware
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("configuration"));
    middleware.stop().await.unwrap();
    drop(middleware);
    let mut middleware = open(provider.clone(), registry.clone(), definition(), 2).await;
    assert!(middleware
        .transform(input(event(
            "source",
            1,
            8,
            change(1, &["different"], false),
            true
        )))
        .await
        .unwrap_err()
        .to_string()
        .contains("committed contents"));
    middleware.stop().await.unwrap();
    drop(middleware);
    let mut changed = definition();
    changed.output_stream = stream("different");
    let mut middleware =
        MiddlewareTransformer::new_durable(changed, registry, provider, options(2))
            .await
            .unwrap();
    assert!(middleware.start().await.is_err());
    middleware.stop().await.unwrap();
}

#[tokio::test]
async fn raw_zero_and_generic_progress_are_distinct_from_chained_logical_output_progress() {
    for raw in [false, true] {
        let directory = scratch();
        let provider = provider(directory.path(), Backend::Computation);
        let probe = Arc::new(ProbeState::default());
        probe.mode.store(3, Ordering::SeqCst);
        let registry = registry(probe.clone());
        let source_progress = Arc::new(QuerySourceProgress::new(GRAPH, id("middleware")).unwrap());
        let mut first = MiddlewareTransformer::new_durable(
            definition(),
            registry.clone(),
            provider.clone(),
            options(8),
        )
        .await
        .unwrap()
        .with_source_progress(source_progress.clone())
        .unwrap();
        first.start().await.unwrap();
        let mut second_definition = definition();
        second_definition.id = id("second");
        second_definition.output_stream = stream("second");
        second_definition.pipeline.clear();
        let second_progress = Arc::new(QuerySourceProgress::new(GRAPH, id("second")).unwrap());
        let mut second = MiddlewareTransformer::new_durable(
            second_definition.clone(),
            registry.clone(),
            provider.clone(),
            options(8),
        )
        .await
        .unwrap()
        .with_source_progress(second_progress.clone())
        .unwrap();
        second.start().await.unwrap();
        let mut query = query(provider.clone()).await;
        let mut original = Vec::new();
        for source in ["a", "b"] {
            let input = event(source, 0, 100, change(0, &["filtered"], false), raw);
            original.push(input.clone());
            let output = first.transform(self::input(input)).await.unwrap();
            assert!(decoded(&output[0].envelope).is_empty());
            assert_eq!(
                GraphChangeCodec::source_metadata(&output[0].envelope)
                    .unwrap()
                    .map(|metadata| metadata.sequence),
                raw.then_some(Some(0))
            );
            let downstream = second
                .transform(self::input(output[0].envelope.clone()))
                .await
                .unwrap();
            assert_eq!(apply(&mut query, &downstream).await, 0);
            second.delivery_completed(&downstream).await.unwrap();
            first.delivery_completed(&output).await.unwrap();
        }
        let view = source_progress.snapshot();
        assert!(view.persistent && view.ready);
        for source in ["a", "b"] {
            let key = if raw {
                SourceProgressKey::Source(source.into())
            } else {
                SourceProgressKey::Stream(stream(source))
            };
            assert_eq!(view.checkpoints.get(&key).unwrap().sequence, 0);
        }
        assert_eq!(
            second_progress
                .snapshot()
                .checkpoints
                .get(&SourceProgressKey::Stream(stream("middleware")))
                .unwrap()
                .sequence,
            2
        );
        assert_eq!(second_progress.snapshot().checkpoints.len(), 1);
        first.stop().await.unwrap();
        second.stop().await.unwrap();
        query.stop().await.unwrap();
        drop((first, second, query));

        let mut first = MiddlewareTransformer::new_durable(
            definition(),
            registry.clone(),
            provider.clone(),
            options(8),
        )
        .await
        .unwrap()
        .with_source_progress(source_progress.clone())
        .unwrap();
        first.start().await.unwrap();
        let mut second = open(provider.clone(), registry.clone(), second_definition, 8).await;
        let mut query = self::query(provider).await;
        probe.mode.store(0, Ordering::SeqCst);
        let output = first
            .transform(input(event("a", 1, 1, change(1, &["a"], false), raw)))
            .await
            .unwrap();
        let downstream = second
            .transform(input(output[0].envelope.clone()))
            .await
            .unwrap();
        assert_eq!(logical(&downstream[0].envelope), 3);
        assert_eq!(apply(&mut query, &downstream).await, 1);
        second.delivery_completed(&downstream).await.unwrap();
        first.delivery_completed(&output).await.unwrap();
        let calls = probe.calls.load(Ordering::SeqCst);
        let replay = first.transform(input(original.remove(0))).await.unwrap();
        let downstream = second
            .transform(input(replay[0].envelope.clone()))
            .await
            .unwrap();
        assert_eq!(logical(&replay[0].envelope), 1);
        assert_eq!(logical(&downstream[0].envelope), 1);
        assert!(query
            .transform(input(downstream[0].envelope.clone()))
            .await
            .unwrap()
            .is_empty());
        assert_eq!(probe.calls.load(Ordering::SeqCst), calls);
        second.delivery_completed(&downstream).await.unwrap();
        first.delivery_completed(&replay).await.unwrap();
        first.stop().await.unwrap();
        second.stop().await.unwrap();
        query.stop().await.unwrap();
    }
}

#[tokio::test]
async fn nested_unwind_and_patch_properties_survive_reconstruction() {
    let directory = scratch();
    let provider = provider(directory.path(), Backend::Computation);
    let registry = registry(Arc::new(ProbeState::default()));
    let mut definition = definition();
    definition.middleware = vec![
        config(
            "unwind",
            "groups",
            json!({"Parent":[{"selector":"$.items[*]","label":"Group","key":"$.id"}]}),
        ),
        config(
            "unwind",
            "children",
            json!({"Group":[{"selector":"$.items[*]","label":"Child","key":"$.id"}]}),
        ),
    ];
    definition.pipeline = vec!["groups".into(), "children".into()];
    let mut middleware = open(provider.clone(), registry.clone(), definition.clone(), 4).await;
    let mut query = query(provider.clone()).await;
    let first = SourceChange::Insert {
        element: parent(
            "source",
            "parent",
            1,
            json!([
                {"id":"g1","items":[{"id":"a"},{"id":"b"}]},
                {"id":"g2","items":[{"id":"c"}]}
            ]),
        ),
    };
    let output = middleware
        .transform(input(event("source", 1, 1, first, true)))
        .await
        .unwrap();
    assert_eq!(apply(&mut query, &output).await, 3);
    middleware.delivery_completed(&output).await.unwrap();
    middleware.stop().await.unwrap();
    drop(middleware);
    let mut middleware = open(provider.clone(), registry.clone(), definition.clone(), 4).await;
    let mut updated = parent(
        "source",
        "parent",
        2,
        json!([
            {"id":"g1","items":[{"id":"a"}]}
        ]),
    );
    if let Element::Node { properties, .. } = &mut updated {
        *properties = ElementPropertyMap::from(json!({
            "items":[{"id":"g1","items":[{"id":"a"}]}]
        }));
    }
    let output = middleware
        .transform(input(event(
            "source",
            2,
            2,
            SourceChange::Update { element: updated },
            true,
        )))
        .await
        .unwrap();
    assert_deleted(&output[0].envelope, "-b");
    assert_deleted(&output[0].envelope, "-c");
    assert_eq!(apply(&mut query, &output).await, 1);
    middleware.delivery_completed(&output).await.unwrap();
    middleware.stop().await.unwrap();
    drop(middleware);
    let resources = provider.create_indexes(GRAPH, "middleware").await.unwrap();
    resources.indexes().session_control.begin().await.unwrap();
    let element = resources
        .indexes()
        .element_index
        .get_element(&ElementReference::new("source", "parent"))
        .await
        .unwrap()
        .unwrap();
    if let Element::Node { properties, .. } = element.as_ref() {
        assert_eq!(
            properties.get("kept"),
            Some(&drasi_core::models::ElementValue::Integer(42))
        );
    } else {
        panic!("expected saved parent");
    }
    resources.indexes().session_control.rollback().unwrap();
    resources.cleanup().unwrap().shutdown().await.unwrap();
    drop(resources);
    let mut middleware = open(provider, registry, definition, 4).await;
    let output = middleware
        .transform(input(event(
            "source",
            3,
            3,
            SourceChange::Delete {
                metadata: parent("source", "parent", 3, json!([]))
                    .get_metadata()
                    .clone(),
            },
            true,
        )))
        .await
        .unwrap();
    assert_deleted(&output[0].envelope, "-a");
    assert_eq!(apply(&mut query, &output).await, 0);
    middleware.delivery_completed(&output).await.unwrap();
    middleware.stop().await.unwrap();
    query.stop().await.unwrap();
}

#[test]
fn declarative_durability_requires_persistent_dependencies_and_lossless_transport() {
    let registry_id = ResourceId::try_new("middleware").unwrap();
    let indexes_id = ResourceId::try_new("indexes").unwrap();
    let spec = definition()
        .durable_specification(registry_id.clone(), indexes_id.clone(), options(4))
        .unwrap();
    let factory = MiddlewareTransformerFactory::default();
    factory.validate(&spec).unwrap();
    let output = spec
        .descriptor
        .ports()
        .iter()
        .find(|port| port.id().as_str() == "out")
        .unwrap();
    let input = spec
        .descriptor
        .ports()
        .iter()
        .find(|port| port.id().as_str() == "in")
        .unwrap();
    assert!(PipeCapabilities::try_new(
        [PipeCapability::RankedEventOrder, PipeCapability::Backpressure],
        Some(capacity(4)),
    )
    .unwrap()
    .validate(input.requirements())
    .is_err());
    PipeCapabilities::volatile_bounded(capacity(4))
        .validate(input.requirements())
        .unwrap();
    assert!(PipeCapabilities::volatile_bounded(capacity(4))
        .validate(output.requirements())
        .is_err());
    for capability in [
        PipeCapability::DurableAcceptance,
        PipeCapability::Replay,
        PipeCapability::Backpressure,
        PipeCapability::ExplicitAcknowledgement,
    ] {
        assert!(output.requirements().required().contains(&capability));
    }
    let mut missing = spec.clone();
    missing.dependencies.remove("indexes");
    assert!(factory.validate(&missing).is_err());
    let mut downgraded = spec.clone();
    downgraded.descriptor = definition().descriptor();
    assert!(factory.validate(&downgraded).is_err());
    let resources = BTreeMap::from([
        (
            registry_id,
            ResourceHandle::new(
                ResourceRole::Middleware,
                Arc::new(MiddlewareRegistryResource(registry(Arc::new(
                    ProbeState::default(),
                )))),
            ),
        ),
        (
            indexes_id,
            ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(QueryIndexProviderResource(Arc::new(
                    InMemoryComputationProvider,
                ))),
            ),
        ),
    ]);
    assert!(factory
        .validate_resources(&spec, &BTreeMap::new(), &resources)
        .is_err());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    Element,
    Outbox,
    InputCheckpoint,
    Head,
    Emission,
    Confirmation,
    Commit,
    CommitAcknowledgement,
    CommittedThenWait,
}

struct Injection {
    point: Fault,
    armed: AtomicBool,
    committed: tokio::sync::Notify,
}
impl Injection {
    fn new(point: Fault) -> Arc<Self> {
        Arc::new(Self {
            point,
            armed: AtomicBool::new(false),
            committed: tokio::sync::Notify::new(),
        })
    }
    fn take(&self, point: Fault) -> bool {
        self.point == point && self.armed.swap(false, Ordering::SeqCst)
    }
}
struct Session {
    inner: Arc<dyn SessionControl>,
    injection: Arc<Injection>,
}
#[async_trait]
impl SessionControl for Session {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> Result<(), IndexError> {
        if self.injection.take(Fault::Commit) {
            return Err(IndexError::IOError);
        }
        self.inner.commit().await?;
        if self.injection.take(Fault::CommitAcknowledgement) {
            return Err(IndexError::IOError);
        }
        if self.injection.take(Fault::CommittedThenWait) {
            self.injection.committed.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}
struct Elements {
    inner: Arc<dyn ElementIndex>,
    injection: Arc<Injection>,
}
#[async_trait]
impl ElementIndex for Elements {
    async fn get_element(
        &self,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.inner.get_element(reference).await
    }
    async fn set_element(&self, element: &Element, slots: &Vec<usize>) -> Result<(), IndexError> {
        self.inner.set_element(element, slots).await?;
        if self.injection.take(Fault::Element) {
            return Err(IndexError::IOError);
        }
        Ok(())
    }
    async fn delete_element(&self, reference: &ElementReference) -> Result<(), IndexError> {
        self.inner.delete_element(reference).await
    }
    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.inner.get_slot_element_by_ref(slot, reference).await
    }
    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.inner
            .get_slot_elements_by_inbound(slot, reference)
            .await
    }
    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        reference: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.inner
            .get_slot_elements_by_outbound(slot, reference)
            .await
    }
    async fn clear(&self) -> Result<(), IndexError> {
        self.inner.clear().await
    }
    async fn set_joins(&self, path: &MatchPath, joins: &Vec<Arc<QueryJoin>>) {
        self.inner.set_joins(path, joins).await;
    }
}
struct Checkpoints {
    inner: Arc<dyn CheckpointStore>,
    injection: Arc<Injection>,
}
#[async_trait]
impl CheckpointStore for Checkpoints {
    fn is_persistent(&self) -> bool {
        true
    }
    async fn stage_checkpoint(
        &self,
        key: &str,
        sequence: u64,
        position: Option<&Bytes>,
    ) -> Result<(), IndexError> {
        self.inner.stage_checkpoint(key, sequence, position).await?;
        let point = if key.starts_with("computation:input:") {
            Some(Fault::InputCheckpoint)
        } else if key == "\0computation:middleware-head:v1" {
            Some(Fault::Head)
        } else if key == "\0computation:middleware-emission:v1" {
            Some(Fault::Emission)
        } else if key == "\0computation:middleware-confirmed:v1" {
            Some(Fault::Confirmation)
        } else {
            None
        };
        if point.is_some_and(|point| self.injection.take(point)) {
            return Err(IndexError::IOError);
        }
        Ok(())
    }
    async fn read_checkpoint(&self, key: &str) -> Result<Option<SourceCheckpoint>, IndexError> {
        self.inner.read_checkpoint(key).await
    }
    async fn read_all_checkpoints(&self) -> Result<HashMap<String, SourceCheckpoint>, IndexError> {
        self.inner.read_all_checkpoints().await
    }
    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        self.inner.clear_checkpoints().await
    }
    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        self.inner.write_config_hash(hash).await
    }
    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        self.inner.read_config_hash().await
    }
    async fn stage_result_sequence(&self, key: &str, sequence: u64) -> Result<(), IndexError> {
        self.inner.stage_result_sequence(key, sequence).await
    }
    async fn read_result_sequence(&self, key: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_result_sequence(key).await
    }
}
struct Outbox {
    inner: Arc<dyn OutboxWriter>,
    injection: Arc<Injection>,
}
#[async_trait]
impl OutboxWriter for Outbox {
    async fn append(&self, key: &str, sequence: u64, bytes: &[u8]) -> Result<(), IndexError> {
        self.inner.append(key, sequence, bytes).await
    }
    async fn append_and_trim(
        &self,
        key: &str,
        sequence: u64,
        bytes: &[u8],
        retain: u64,
    ) -> Result<usize, IndexError> {
        let result = self
            .inner
            .append_and_trim(key, sequence, bytes, retain)
            .await?;
        if self.injection.take(Fault::Outbox) {
            return Err(IndexError::IOError);
        }
        Ok(result)
    }
    async fn read_from(&self, key: &str, after: u64) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner.read_from(key, after).await
    }
    async fn read_latest_sequence(&self, key: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_latest_sequence(key).await
    }
    async fn clear(&self, key: &str) -> Result<(), IndexError> {
        self.inner.clear(key).await
    }
    async fn trim_before(&self, key: &str, retain: u64) -> Result<usize, IndexError> {
        self.inner.trim_before(key, retain).await
    }
    async fn trim_to_capacity(&self, key: &str, size: usize) -> Result<usize, IndexError> {
        self.inner.trim_to_capacity(key, size).await
    }
}
struct InjectingProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    injection: Arc<Injection>,
}
#[async_trait]
impl ComputationIndexProvider for InjectingProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        id: &str,
    ) -> Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, id).await?;
        let set = original.indexes();
        let session: Arc<dyn SessionControl> = Arc::new(Session {
            inner: set.session_control.clone(),
            injection: self.injection.clone(),
        });
        let domain = TransactionDomain::new(session.clone());
        let indexes = ComputationIndexes::try_new(
            IndexSet {
                element_index: Arc::new(Elements {
                    inner: set.element_index.clone(),
                    injection: self.injection.clone(),
                }),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: session,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                Arc::new(Checkpoints {
                    inner: original.checkpoint_store().expect("checkpoint").clone(),
                    injection: self.injection.clone(),
                }),
                &domain,
            )),
            Some(ComputationResource::participating(
                Arc::new(Outbox {
                    inner: original.outbox_writer().expect("outbox").clone(),
                    injection: self.injection.clone(),
                }),
                &domain,
            )),
            Some(ComputationResource::participating(
                original
                    .live_results_writer()
                    .expect("live projection")
                    .clone(),
                &domain,
            )),
        )
        .map_err(IndexError::other)?;
        Ok(indexes.with_cleanup(original.cleanup().expect("cleanup owner").clone()))
    }
    fn is_volatile(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn element_outbox_checkpoint_and_commit_failures_rollback_the_whole_input() {
    for point in [
        Fault::Element,
        Fault::Outbox,
        Fault::InputCheckpoint,
        Fault::Head,
        Fault::Emission,
        Fault::Commit,
    ] {
        let directory = scratch();
        let provider = provider(directory.path(), Backend::Computation);
        let injection = Injection::new(point);
        let faulty: Arc<dyn ComputationIndexProvider> = Arc::new(InjectingProvider {
            inner: provider.clone(),
            injection: injection.clone(),
        });
        let registry = registry(Arc::new(ProbeState::default()));
        let progress = Arc::new(QuerySourceProgress::new(GRAPH, id("middleware")).unwrap());
        let mut middleware =
            MiddlewareTransformer::new_durable(definition(), registry.clone(), faulty, options(1))
                .await
                .unwrap()
                .with_source_progress(progress.clone())
                .unwrap();
        middleware.start().await.unwrap();
        let first = middleware
            .transform(input(event(
                "source",
                1,
                1,
                change(1, &["a", "b"], false),
                true,
            )))
            .await
            .unwrap();
        middleware.delivery_completed(&first).await.unwrap();
        injection.armed.store(true, Ordering::SeqCst);
        let update = event("source", 2, 2, change(2, &["a"], true), true);
        assert!(
            middleware.transform(input(update.clone())).await.is_err(),
            "{point:?}"
        );
        assert_eq!(
            progress
                .snapshot()
                .checkpoints
                .get(&SourceProgressKey::Source("source".into()))
                .unwrap()
                .sequence,
            1
        );
        assert!(progress.snapshot().failure.is_some());
        middleware.stop().await.unwrap();
        drop(middleware);
        let mut middleware = open(provider, registry, definition(), 1).await;
        assert!(!middleware.has_pending_emissions(), "{point:?}");
        let retried = middleware.transform(input(update)).await.unwrap();
        assert_eq!(logical(&retried[0].envelope), 2, "{point:?}");
        assert_eq!(retried[0].envelope.system().sequence(), 2, "{point:?}");
        assert_deleted(&retried[0].envelope, "-b");
        middleware.delivery_completed(&retried).await.unwrap();
        middleware.stop().await.unwrap();
    }
}

#[tokio::test]
async fn uncertain_commit_and_interrupted_commit_acknowledgement_recover_exact_output() {
    for point in [Fault::CommitAcknowledgement, Fault::CommittedThenWait] {
        let directory = scratch();
        let provider = provider(directory.path(), Backend::Computation);
        let injection = Injection::new(point);
        let faulty: Arc<dyn ComputationIndexProvider> = Arc::new(InjectingProvider {
            inner: provider.clone(),
            injection: injection.clone(),
        });
        let calls = Arc::new(ProbeState::default());
        let registry = registry(calls.clone());
        let mut middleware = open(faulty, registry.clone(), definition(), 2).await;
        injection.armed.store(true, Ordering::SeqCst);
        let mut work = Box::pin(middleware.transform(input(event(
            "source",
            0,
            20,
            change(0, &["a", "b"], false),
            true,
        ))));
        if point == Fault::CommittedThenWait {
            tokio::select! {
                result = &mut work => panic!("must wait after commit: {result:?}"),
                _ = injection.committed.notified() => {}
            }
        } else {
            assert!(work.as_mut().await.is_err());
        }
        drop(work);
        middleware.stop().await.unwrap();
        drop(middleware);
        let evaluated = calls.calls.load(Ordering::SeqCst);
        let mut middleware = open(provider.clone(), registry, definition(), 2).await;
        let mut query = query(provider).await;
        let output = middleware.on_wakeup().await.unwrap();
        assert_eq!(calls.calls.load(Ordering::SeqCst), evaluated);
        assert_eq!(logical(&output[0].envelope), 1);
        assert_eq!(output[0].envelope.system().sequence(), 2);
        assert_eq!(
            GraphChangeCodec::source_metadata(&output[0].envelope)
                .unwrap()
                .unwrap()
                .sequence,
            Some(0)
        );
        assert_eq!(apply(&mut query, &output).await, 2);
        middleware.delivery_completed(&output).await.unwrap();
        middleware.stop().await.unwrap();
        query.stop().await.unwrap();
    }
}

#[tokio::test]
async fn failed_delivery_confirmation_and_replay_reservation_retain_pending_output() {
    for point in [Fault::Confirmation, Fault::Emission] {
        let directory = scratch();
        let provider = provider(directory.path(), Backend::Computation);
        let injection = Injection::new(point);
        let faulty: Arc<dyn ComputationIndexProvider> = Arc::new(InjectingProvider {
            inner: provider.clone(),
            injection: injection.clone(),
        });
        let registry = registry(Arc::new(ProbeState::default()));
        let mut middleware = open(faulty, registry.clone(), definition(), 1).await;
        let output = middleware
            .transform(input(event(
                "source",
                1,
                1,
                change(1, &["a"], false),
                false,
            )))
            .await
            .unwrap();
        injection.armed.store(true, Ordering::SeqCst);
        if point == Fault::Confirmation {
            assert!(middleware.delivery_completed(&output).await.is_err());
        } else {
            let duplicate = event("source", 1, 1, change(1, &["a"], false), false);
            assert!(middleware.transform(input(duplicate)).await.is_err());
        }
        middleware.stop().await.unwrap();
        drop(middleware);
        let mut middleware = open(provider, registry, definition(), 1).await;
        let replay = middleware.on_wakeup().await.unwrap();
        assert_eq!(decoded(&replay[0].envelope), decoded(&output[0].envelope));
        assert_eq!(replay[0].envelope.system().sequence(), 2);
        middleware.delivery_completed(&replay).await.unwrap();
        middleware.stop().await.unwrap();
    }
}

#[tokio::test]
async fn logical_gaps_changed_producers_and_missing_annotations_fail_before_query_evaluation() {
    let directory = scratch();
    let provider = provider(directory.path(), Backend::Computation);
    let registry = registry(Arc::new(ProbeState::default()));
    let mut middleware = open(provider.clone(), registry.clone(), definition(), 4).await;
    let first = middleware
        .transform(input(event("source", 1, 1, change(1, &["a"], false), true)))
        .await
        .unwrap();
    middleware.delivery_completed(&first).await.unwrap();
    let second = middleware
        .transform(input(event(
            "source",
            2,
            2,
            change(2, &["a", "b"], true),
            true,
        )))
        .await
        .unwrap();
    let mut query = query(provider.clone()).await;
    assert!(query
        .transform(input(second[0].envelope.clone()))
        .await
        .unwrap_err()
        .to_string()
        .contains("gap"));
    assert!(query.results().snapshot().unwrap().rows.is_empty());
    query.stop().await.unwrap();
    drop(query);
    let mut query = self::query(provider.clone()).await;
    assert_eq!(apply(&mut query, &first).await, 1);
    query.stop().await.unwrap();
    drop(query);

    let mut missing = event("source", 2, 2, change(2, &["a", "b"], true), true);
    missing =
        GraphChangeCodec::derive_changes(&missing, &decoded(&missing), stream("middleware"), 10)
            .unwrap();
    let mut query = self::query(provider.clone()).await;
    assert!(query
        .transform(input(missing))
        .await
        .unwrap_err()
        .to_string()
        .contains("omitted"));
    query.stop().await.unwrap();
    drop(query);

    let other_directory = scratch();
    let other_provider = self::provider(other_directory.path(), Backend::Computation);
    let mut replacement = open(other_provider, registry, definition(), 4).await;
    let fresh = replacement
        .transform(input(event(
            "source",
            1,
            1,
            change(1, &["impostor"], false),
            true,
        )))
        .await
        .unwrap();
    let mut query = self::query(provider).await;
    assert!(query
        .transform(input(fresh[0].envelope.clone()))
        .await
        .unwrap_err()
        .to_string()
        .contains("identity changed"));
    assert_eq!(query.results().snapshot().unwrap().rows.len(), 1);
    query.stop().await.unwrap();
    replacement.stop().await.unwrap();
    middleware.stop().await.unwrap();
}

#[tokio::test]
async fn graph_progress_metadata_is_versioned_and_validates_its_immediate_owner() {
    let directory = scratch();
    let provider = provider(directory.path(), Backend::Computation);
    let registry = registry(Arc::new(ProbeState::default()));
    let mut middleware = open(provider, registry, definition(), 2).await;
    let output = middleware
        .transform(input(event("source", 1, 1, change(1, &["a"], false), true)))
        .await
        .unwrap()
        .remove(0)
        .envelope;
    let progress = GraphProducerProgress::from_envelope(&output)
        .unwrap()
        .unwrap();
    let original = serde_json::to_value(progress).unwrap();
    for (path, value) in [
        (vec!["version"], json!(2)),
        (vec!["sequence"], json!(0)),
        (vec!["identity", "stream"], json!("other/out")),
        (vec!["identity", "component_id"], json!("other")),
        (
            vec!["identity", "incarnation"],
            json!("00000000-0000-0000-0000-000000000000"),
        ),
    ] {
        let mut value_tree = original.clone();
        let mut slot = &mut value_tree;
        for name in path {
            slot = &mut slot[name];
        }
        *slot = value;
        let mut invalid = output.clone();
        invalid
            .append_annotation(
                ContextEntry::try_new(
                    id("middleware"),
                    "drasi.graph-producer-progress.v1",
                    ContextValue::Bytes(Arc::from(serde_json::to_vec(&value_tree).unwrap())),
                )
                .unwrap(),
            )
            .unwrap();
        assert!(GraphProducerProgress::from_envelope(&invalid).is_err());
    }
    middleware.stop().await.unwrap();
}

struct ProgressSource {
    descriptor: ComponentDescriptor,
    progress: Arc<QuerySourceProgress>,
}
#[async_trait]
impl ComputationComponent for ProgressSource {
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
impl EnvelopeSource for ProgressSource {
    fn recovery_progress(&self) -> Option<Arc<QuerySourceProgress>> {
        Some(self.progress.clone())
    }
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        Ok(None)
    }
}
struct ProgressSink(ComponentDescriptor);
#[async_trait]
impl ComputationComponent for ProgressSink {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    async fn start(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> anyhow::Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSink for ProgressSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _: InputEnvelope) -> anyhow::Result<()> {
        Ok(())
    }
}

#[test]
fn source_resume_checkpoint_cannot_skip_an_intervening_middleware_boundary() {
    let descriptor = |component, direction| {
        ComponentDescriptor::try_new(
            id(component),
            vec![PortDescriptor::new(
                PortId::try_new(if direction == PortDirection::Input {
                    "in"
                } else {
                    "out"
                })
                .unwrap(),
                direction,
                GraphChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )
        .unwrap()
    };
    let endpoint =
        |component: &str, port: &str| Endpoint::new(id(component), PortId::try_new(port).unwrap());
    let progress = Arc::new(QuerySourceProgress::new(GRAPH, id("query")).unwrap());
    let builder = ComputationGraph::builder(GRAPH)
        .source(Box::new(ProgressSource {
            descriptor: descriptor("source", PortDirection::Output),
            progress,
        }))
        .transformer(Box::new(
            MiddlewareTransformer::new(definition(), registry(Arc::new(ProbeState::default())))
                .unwrap(),
        ))
        .sink(Box::new(ProgressSink(descriptor(
            "query",
            PortDirection::Input,
        ))))
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("middleware", "out"), stream("middleware"))
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("middleware", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        )
        .connect(
            EdgeDefinition::new(endpoint("middleware", "out"), endpoint("query", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        );
    assert!(builder
        .build()
        .err()
        .unwrap()
        .to_string()
        .contains("immediate consumer"));
}

#[derive(Clone, Copy)]
enum ResourceShape {
    MissingOutbox,
    IndependentOutbox,
    VolatileCheckpoint,
    NoCleanup,
    NoQuiescence,
}
struct ShutdownOnly(Arc<dyn drasi_core::computation::ComputationResourceCleanup>);
#[async_trait]
impl drasi_core::computation::ComputationResourceCleanup for ShutdownOnly {
    fn cancel(&self) {
        self.0.cancel();
    }
    async fn shutdown(&self) -> Result<(), IndexError> {
        self.0.shutdown().await
    }
}
struct IncompleteProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    shape: ResourceShape,
}
#[async_trait]
impl ComputationIndexProvider for IncompleteProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        id: &str,
    ) -> Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, id).await?;
        let set = original.indexes();
        let domain = TransactionDomain::new(set.session_control.clone());
        let checkpoint: Arc<dyn CheckpointStore> = if matches!(
            self.shape,
            ResourceShape::VolatileCheckpoint
        ) {
            Arc::new(drasi_core::in_memory_index::in_memory_checkpoint_store::InMemoryCheckpointStore::new())
        } else {
            original.checkpoint_store().expect("checkpoint").clone()
        };
        let outbox = match self.shape {
            ResourceShape::MissingOutbox => None,
            ResourceShape::IndependentOutbox => Some(ComputationResource::independent(
                original.outbox_writer().expect("outbox").clone(),
            )),
            _ => Some(ComputationResource::participating(
                original.outbox_writer().expect("outbox").clone(),
                &domain,
            )),
        };
        let indexes = ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: set.session_control.clone(),
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(checkpoint, &domain)),
            outbox,
            Some(ComputationResource::participating(
                original
                    .live_results_writer()
                    .expect("live projection")
                    .clone(),
                &domain,
            )),
        )
        .map_err(IndexError::other)?;
        Ok(if matches!(self.shape, ResourceShape::NoCleanup) {
            indexes
        } else if matches!(self.shape, ResourceShape::NoQuiescence) {
            indexes.with_cleanup(Arc::new(ShutdownOnly(
                original.cleanup().expect("cleanup owner").clone(),
            )))
        } else {
            indexes.with_cleanup(original.cleanup().expect("cleanup owner").clone())
        })
    }
    fn is_volatile(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn durable_mode_rejects_incomplete_non_atomic_and_unowned_cleanup_resources() {
    for shape in [
        ResourceShape::MissingOutbox,
        ResourceShape::IndependentOutbox,
        ResourceShape::VolatileCheckpoint,
        ResourceShape::NoCleanup,
    ] {
        let directory = scratch();
        let provider: Arc<dyn ComputationIndexProvider> = Arc::new(IncompleteProvider {
            inner: provider(directory.path(), Backend::Computation),
            shape,
        });
        assert!(MiddlewareTransformer::new_durable(
            definition(),
            registry(Arc::new(ProbeState::default())),
            provider,
            options(2),
        )
        .await
        .is_err());
    }
    let directory = scratch();
    let provider: Arc<dyn ComputationIndexProvider> = Arc::new(IncompleteProvider {
        inner: provider(directory.path(), Backend::Computation),
        shape: ResourceShape::NoQuiescence,
    });
    let mut middleware = MiddlewareTransformer::new_durable(
        definition(),
        registry(Arc::new(ProbeState::default())),
        provider,
        options(2),
    )
    .await
    .unwrap();
    assert!(
        middleware.start().await.is_err(),
        "reject missing lifecycle support before any input"
    );
    middleware.stop().await.unwrap();
}

#[tokio::test]
async fn durable_emission_overflow_cannot_mutate_input_state_or_discard_pending_output() {
    for pending in [false, true] {
        let directory = scratch();
        let provider = provider(directory.path(), Backend::Computation);
        let calls = Arc::new(ProbeState::default());
        let registry = registry(calls.clone());
        let mut middleware = open(provider.clone(), registry.clone(), definition(), 2).await;
        let first = middleware
            .transform(input(event(
                "source",
                1,
                1,
                change(1, &["a", "b"], false),
                false,
            )))
            .await
            .unwrap();
        if !pending {
            middleware.delivery_completed(&first).await.unwrap();
        }
        middleware.stop().await.unwrap();
        drop(middleware);
        let resources = provider.create_indexes(GRAPH, "middleware").await.unwrap();
        resources.indexes().session_control.begin().await.unwrap();
        resources
            .checkpoint_store()
            .unwrap()
            .stage_checkpoint("\0computation:middleware-emission:v1", u64::MAX, None)
            .await
            .unwrap();
        resources.indexes().session_control.commit().await.unwrap();
        resources.cleanup().unwrap().shutdown().await.unwrap();
        drop(resources);

        let evaluated = calls.calls.load(Ordering::SeqCst);
        let mut middleware = open(provider.clone(), registry, definition(), 2).await;
        let error = if pending {
            middleware.on_wakeup().await.unwrap_err()
        } else {
            middleware
                .transform(input(event("source", 2, 2, change(2, &["a"], true), false)))
                .await
                .unwrap_err()
        };
        assert!(error.to_string().contains("sequence exhausted"));
        assert_eq!(calls.calls.load(Ordering::SeqCst), evaluated);
        middleware.stop().await.unwrap();
        drop(middleware);
        let resources = provider.create_indexes(GRAPH, "middleware").await.unwrap();
        let entries = resources
            .outbox_writer()
            .unwrap()
            .read_from("computation:middleware-output:v1", 0)
            .await
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, 1);
        assert_eq!(
            resources
                .checkpoint_store()
                .unwrap()
                .read_checkpoint("\0computation:middleware-head:v1")
                .await
                .unwrap()
                .unwrap()
                .sequence,
            1
        );
        resources.cleanup().unwrap().shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn redb_wal_reopens_at_middleware_commit_while_undelivered_output_replays_from_rocksdb() {
    use drasi_lib::wal::{WalProvider, WriteAheadLogConfig};

    let directory = scratch();
    let provider = provider(&directory.path().join("indexes"), Backend::Computation);
    let registry = registry(Arc::new(ProbeState::default()));
    let progress = Arc::new(QuerySourceProgress::new(GRAPH, id("middleware")).unwrap());
    let wal = Arc::new(drasi_wal_redb::RedbWalProvider::new(
        directory.path().join("wal"),
    ));
    wal.register("source", WriteAheadLogConfig::default())
        .await
        .unwrap();
    assert_eq!(
        wal.append("source", &change(1, &["a", "b"], false))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        wal.append("source", &change(2, &["a"], true))
            .await
            .unwrap(),
        2
    );
    let mut middleware = MiddlewareTransformer::new_durable(
        definition(),
        registry.clone(),
        provider.clone(),
        options(1),
    )
    .await
    .unwrap()
    .with_source_progress(progress.clone())
    .unwrap();
    middleware.start().await.unwrap();
    assert!(progress.replay_only());
    let mut source = WalReplaySource::new(
        id("source"),
        stream("source"),
        Arc::new(WalSourceResource {
            provider: wal.clone(),
            partition: "source".into(),
        }),
        0,
    )
    .with_source_progress(progress.clone());
    source.start().await.unwrap();
    let source_output = source.next().await.unwrap().unwrap();
    let committed = middleware
        .transform(input(source_output.envelope))
        .await
        .unwrap();
    assert_eq!(logical(&committed[0].envelope), 1);
    assert_eq!(
        progress.snapshot().checkpoints[&SourceProgressKey::Source("source".into())].sequence,
        1
    );
    // Input can be pruned at the middleware commit: its still-unconfirmed
    // transformed output is now the durable recovery boundary.
    assert_eq!(wal.prune_up_to("source", 1).await.unwrap(), 1);
    source.stop().await.unwrap();
    middleware.stop().await.unwrap();
    drop((source, middleware, wal));

    let wal = Arc::new(drasi_wal_redb::RedbWalProvider::new(
        directory.path().join("wal"),
    ));
    wal.register("source", WriteAheadLogConfig::default())
        .await
        .unwrap();
    let mut middleware =
        MiddlewareTransformer::new_durable(definition(), registry, provider.clone(), options(1))
            .await
            .unwrap()
            .with_source_progress(progress.clone())
            .unwrap();
    middleware.start().await.unwrap();
    let resource = Arc::new(WalSourceResource {
        provider: wal.clone(),
        partition: "source".into(),
    });
    let mut invalid = WalReplaySource::new(id("invalid"), stream("invalid"), resource.clone(), 2)
        .with_source_progress(progress.clone());
    assert!(invalid
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("committed recovery boundary"));
    invalid.stop().await.unwrap();
    let mut source = WalReplaySource::new(id("source"), stream("source"), resource, 0)
        .with_source_progress(progress);
    source.start().await.unwrap();
    let mut query = query(provider).await;
    let replay = middleware.on_wakeup().await.unwrap();
    assert_eq!(
        decoded(&replay[0].envelope),
        decoded(&committed[0].envelope)
    );
    assert_eq!(apply(&mut query, &replay).await, 2);
    middleware.delivery_completed(&replay).await.unwrap();
    let next = source.next().await.unwrap().unwrap();
    assert_eq!(
        GraphChangeCodec::source_metadata(&next.envelope)
            .unwrap()
            .unwrap()
            .sequence,
        Some(2)
    );
    assert_eq!(next.envelope.system().sequence(), 1);
    let output = middleware.transform(input(next.envelope)).await.unwrap();
    assert_eq!(logical(&output[0].envelope), 2);
    assert_eq!(output[0].envelope.system().sequence(), 3);
    assert_deleted(&output[0].envelope, "-b");
    assert_eq!(apply(&mut query, &output).await, 1);
    middleware.delivery_completed(&output).await.unwrap();
    source.stop().await.unwrap();
    middleware.stop().await.unwrap();
    query.stop().await.unwrap();
}

#[tokio::test]
async fn durable_control_notifications_checkpoint_logical_progress_before_later_graph_changes() {
    let directory = scratch();
    let provider = provider(directory.path(), Backend::Computation);
    let registry = registry(Arc::new(ProbeState::default()));
    let mut middleware = open(provider.clone(), registry, definition(), 4).await;
    let mut query = query(provider).await;
    let signal = GraphChangeCodec::encode_source_event(
        Arc::new(SourceEventWrapper::with_sequence(
            "source".into(),
            SourceEvent::Control(drasi_lib::channels::SourceControl::FuturesDue),
            chrono::DateTime::from_timestamp_millis(1).unwrap(),
            0,
            None,
        )),
        &id("source"),
        stream("source"),
        1,
        None,
    )
    .unwrap();
    let output = middleware.transform(input(signal.clone())).await.unwrap();
    assert_eq!(logical(&output[0].envelope), 1);
    assert!(output[0].envelope.changes().is_empty());
    assert!(query
        .transform(input(output[0].envelope.clone()))
        .await
        .unwrap()
        .is_empty());
    while query.has_pending_emissions() {
        assert!(query.continue_transform().await.unwrap().is_empty());
    }
    middleware.delivery_completed(&output).await.unwrap();
    let output = middleware
        .transform(input(event("source", 1, 2, change(1, &["a"], false), true)))
        .await
        .unwrap();
    assert_eq!(logical(&output[0].envelope), 2);
    assert_eq!(apply(&mut query, &output).await, 1);
    middleware.delivery_completed(&output).await.unwrap();
    let replay = middleware.transform(input(signal)).await.unwrap();
    assert!(query
        .transform(input(replay[0].envelope.clone()))
        .await
        .unwrap()
        .is_empty());
    middleware.delivery_completed(&replay).await.unwrap();
    middleware.stop().await.unwrap();
    query.stop().await.unwrap();
}

#[tokio::test]
async fn replayed_middleware_control_does_not_evaluate_not_yet_due_query_futures() {
    let directory = scratch();
    let provider = provider(directory.path(), Backend::Computation);
    let registry = registry(Arc::new(ProbeState::default()));
    let mut middleware = open(provider.clone(), registry, definition(), 4).await;
    let due = chrono::Utc::now().timestamp_millis() + 60_000;
    let mut query = ContinuousQueryTransformer::new(
        ContinuousQueryDefinition {
            graph_id: GRAPH.into(),
            id: id("query"),
            query: format!("MATCH (n:Child) WHERE drasi.trueLater(true, {due}) RETURN n.id AS id"),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("query"),
            outbox_capacity: capacity(4),
        },
        provider,
    )
    .await
    .unwrap();
    query.start().await.unwrap();
    let output = middleware
        .transform(input(event("source", 1, 1, change(1, &["a"], false), true)))
        .await
        .unwrap();
    assert_eq!(apply(&mut query, &output).await, 0);
    middleware.delivery_completed(&output).await.unwrap();
    let signal = GraphChangeCodec::encode_source_event(
        Arc::new(SourceEventWrapper::with_sequence(
            "source".into(),
            SourceEvent::Control(drasi_lib::channels::SourceControl::FuturesDue),
            chrono::Utc::now(),
            2,
            None,
        )),
        &id("source"),
        stream("source"),
        2,
        None,
    )
    .unwrap();
    for _ in 0..2 {
        let output = middleware.transform(input(signal.clone())).await.unwrap();
        assert_eq!(logical(&output[0].envelope), 2);
        assert!(query
            .transform(input(output[0].envelope.clone()))
            .await
            .unwrap()
            .is_empty());
        assert!(
            !query.has_pending_emissions(),
            "a stale control cue must not drain future work"
        );
        assert!(query.results().snapshot().unwrap().rows.is_empty());
        middleware.delivery_completed(&output).await.unwrap();
    }
    middleware.stop().await.unwrap();
    query.stop().await.unwrap();
}

struct GatedSource {
    descriptor: ComponentDescriptor,
    output: Option<OutputEnvelope>,
    release: Arc<tokio::sync::Notify>,
    progress: Arc<QuerySourceProgress>,
}
#[async_trait]
impl ComputationComponent for GatedSource {
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
impl EnvelopeSource for GatedSource {
    fn recovery_progress(&self) -> Option<Arc<QuerySourceProgress>> {
        Some(self.progress.clone())
    }
    async fn next(&mut self) -> anyhow::Result<Option<OutputEnvelope>> {
        if self.output.is_some() {
            self.release.notified().await;
        }
        Ok(self.output.take())
    }
}
struct SignalSink {
    descriptor: ComponentDescriptor,
    received: Arc<std::sync::Mutex<Vec<ChangeEnvelope>>>,
    release: Arc<tokio::sync::Notify>,
}
#[async_trait]
impl ComputationComponent for SignalSink {
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
impl EnvelopeSink for SignalSink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> anyhow::Result<()> {
        self.received
            .lock()
            .expect("received results")
            .push(input.envelope);
        self.release.notify_one();
        Ok(())
    }
}

#[tokio::test]
async fn declared_graph_confirms_wakeup_continuation_and_live_fanout_only_after_durable_acceptance()
{
    let directory = scratch();
    let provider = provider(directory.path(), Backend::Computation);
    let calls = Arc::new(ProbeState::default());
    let registry = registry(calls.clone());
    let mut original = open(provider.clone(), registry.clone(), definition(), 2).await;
    for (sequence, items, update) in [(1, vec!["a", "b"], false), (2, vec!["a"], true)] {
        original
            .transform(input(event(
                "source",
                sequence,
                sequence,
                change(sequence, &items, update),
                false,
            )))
            .await
            .unwrap();
    }
    original.stop().await.unwrap();
    drop(original);
    assert_eq!(calls.calls.load(Ordering::SeqCst), 2);

    let progress = Arc::new(QuerySourceProgress::new(GRAPH, id("middleware")).unwrap());
    let registry_id = ResourceId::try_new("registry").unwrap();
    let provider_id = ResourceId::try_new("indexes").unwrap();
    let progress_id = ResourceId::try_new("progress").unwrap();
    let mut specification = definition()
        .durable_specification(registry_id.clone(), provider_id.clone(), options(2))
        .unwrap();
    specification
        .dependencies
        .insert(Arc::from("source_progress"), vec![progress_id.clone()]);
    let endpoint =
        |component: &str, port: &str| Endpoint::new(id(component), PortId::try_new(port).unwrap());
    let release = Arc::new(tokio::sync::Notify::new());
    let deletion = SourceChange::Delete {
        metadata: parent("source", "parent", 3, json!([]))
            .get_metadata()
            .clone(),
    };
    let mut builder = ComputationGraph::builder(GRAPH)
        .source(Box::new(GatedSource {
            descriptor: ComponentDescriptor::try_new(
                id("source"),
                vec![PortDescriptor::new(
                    PortId::try_new("out").unwrap(),
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )
            .unwrap(),
            output: Some(OutputEnvelope {
                port: PortId::try_new("out").unwrap(),
                envelope: event("source", 3, 3, deletion, false),
            }),
            release: release.clone(),
            progress: progress.clone(),
        }))
        .component(
            specification,
            Arc::new(MiddlewareTransformerFactory::default()),
        )
        .bind_stream(endpoint("source", "out"), stream("source"))
        .bind_stream(endpoint("middleware", "out"), stream("middleware"))
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("middleware", "in")),
            Box::new(BoundedPipeConfig { capacity: 1 }),
        );
    for (resource_id, role, handle) in [
        (
            registry_id,
            ResourceRole::Middleware,
            ResourceHandle::new(
                ResourceRole::Middleware,
                Arc::new(MiddlewareRegistryResource(registry.clone())),
            ),
        ),
        (
            provider_id,
            ResourceRole::IndexBackend,
            ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(QueryIndexProviderResource(provider.clone())),
            ),
        ),
        (
            progress_id,
            ResourceRole::Checkpoint,
            ResourceHandle::new(
                ResourceRole::Checkpoint,
                Arc::new(QuerySourceProgressResource(progress)),
            ),
        ),
    ] {
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource_id.clone(),
                role,
                ownership: ResourceOwnership::Borrowed,
                binding: Arc::from(resource_id.as_str()),
            })
            .unwrap()
            .provide_resource(resource_id, handle)
            .unwrap();
    }
    let mut received = Vec::new();
    for name in ["a", "b"] {
        let query_id = format!("query-{name}");
        let sink_id = format!("sink-{name}");
        let resource_id = ResourceId::try_new(format!("pipe-{name}")).unwrap();
        let indexes = provider
            .create_indexes(GRAPH, resource_id.as_str())
            .await
            .unwrap();
        let mut codec = EnvelopeCodec::new(capacity(8 * 1024 * 1024));
        codec.register_schema(GraphChangeCodec::schema()).unwrap();
        let store = Arc::new(RetainedStoreResource(Arc::new(
            IndexedEnvelopeStore::try_new(
                indexes,
                Arc::new(codec),
                name,
                capacity(1),
                RetentionPolicy::Backpressure,
            )
            .unwrap(),
        )));
        let query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: GRAPH.into(),
                id: id(&query_id),
                query: "MATCH (n:Child) RETURN n.id AS id".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: stream(&query_id),
                outbox_capacity: capacity(8),
            },
            provider.clone(),
        )
        .await
        .unwrap();
        let branch = Arc::new(std::sync::Mutex::new(Vec::new()));
        received.push(branch.clone());
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource_id.clone(),
                role: ResourceRole::StateStore,
                ownership: ResourceOwnership::Graph,
                binding: Arc::from(resource_id.as_str()),
            })
            .unwrap()
            .provide_resource(
                resource_id.clone(),
                ResourceHandle::new(ResourceRole::StateStore, store.clone()).with_cleanup(store),
            )
            .unwrap()
            .query(Box::new(query))
            .sink(Box::new(SignalSink {
                descriptor: ComponentDescriptor::try_new(
                    id(&sink_id),
                    vec![PortDescriptor::new(
                        PortId::try_new("in").unwrap(),
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )
                .unwrap(),
                received: branch,
                release: release.clone(),
            }))
            .bind_stream(endpoint(&query_id, "out"), stream(&query_id))
            .connect(
                EdgeDefinition::new(endpoint("middleware", "out"), endpoint(&query_id, "in")),
                Box::new(RetainedPipeConfig {
                    resource: resource_id,
                    capacity: capacity(1),
                    durable: true,
                    retention: RetentionPolicy::Backpressure,
                    gap_policy: ReplayGapPolicy::Strict,
                }),
            )
            .connect(
                EdgeDefinition::new(endpoint(&query_id, "out"), endpoint(&sink_id, "in")),
                Box::new(BoundedPipeConfig { capacity: 4 }),
            );
    }
    let mut graph = builder.build().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), graph.start().unwrap())
        .await
        .unwrap()
        .unwrap();
    graph.dispose().await.unwrap();
    drop(graph);
    assert_eq!(
        calls.calls.load(Ordering::SeqCst),
        3,
        "recovery did not rerun middleware"
    );
    for branch in received {
        let branch = branch.lock().unwrap();
        assert_eq!(branch.len(), 3);
        assert_eq!(
            branch
                .iter()
                .map(|output| output.system().sequence())
                .collect::<Vec<_>>(),
            [1, 2, 3]
        );
    }
    let mut middleware = open(provider, registry, definition(), 2).await;
    assert!(
        !middleware.has_pending_emissions(),
        "every branch accepted all three outputs"
    );
    middleware.stop().await.unwrap();
}
