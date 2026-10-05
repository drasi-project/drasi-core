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

//! External-consumer proof: drasi-lib is compiled WITHOUT cfg(test).
//! No internal manager/queue access; only public builder, Source/Reaction traits,
//! and a test-owned persistent provider decorator.
#![cfg(feature = "test-support")]

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::{
    interface::{
        CheckpointStore, CreatedIndexes, IndexBackendPlugin, IndexError, LiveResultsWriter,
        OutboxWriter,
    },
    models::{Element, ElementMetadata, ElementReference, SourceChange},
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::{
    channels::{
        ComponentStatus, QueryResult, ResultDiff, SourceEvent, SourceEventWrapper,
        SubscriptionResponse,
    },
    config::SourceSubscriptionSettings,
    error::DrasiError,
    test_support::{DrainReport, QueryTestControl},
    DrasiLib, DrasiLibBuilder, Query, Reaction, ReactionBase, ReactionBaseParams,
    ReactionRuntimeContext, Source, SourceBase, SourceBaseParams, SourceRuntimeContext,
};
use futures::poll;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    future::Future,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::sync::{mpsc, Semaphore};

const LIMIT: Duration = Duration::from_secs(10);
const QUERY: &str = "MATCH (r:Request)
    WHERE drasi.trueLater(r.pending, r.nextCheckAt)
    RETURN r.id AS id, r.generation AS generation,
           r.completedCheck AS completedCheck, r.remoteState AS remoteState,
           r.responseDeadline AS responseDeadline";

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(LIMIT, future)
        .await
        .expect("bounded test operation")
}

#[derive(Clone)]
struct Input {
    base: Arc<SourceBase>,
    positions: Arc<Mutex<HashMap<String, Arc<AtomicU64>>>>,
}

#[async_trait]
impl Source for Input {
    fn id(&self) -> &str {
        self.base.get_id()
    }
    fn type_name(&self) -> &str {
        "controlled-timing-input"
    }
    fn properties(&self) -> HashMap<String, Value> {
        HashMap::new()
    }
    fn auto_start(&self) -> bool {
        true
    }
    fn supports_replay(&self) -> bool {
        true
    }
    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }
    async fn start(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.base.status_handle().get_status().await
    }
    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        let position = Arc::new(AtomicU64::new(0));
        self.positions
            .lock()
            .expect("source positions lock")
            .insert(settings.query_id.clone(), position.clone());
        Ok(SubscriptionResponse {
            query_id: settings.query_id,
            source_id: self.id().into(),
            receiver: self.base.create_streaming_receiver().await?,
            bootstrap_receiver: None,
            position_handle: Some(position),
            bootstrap_result_receiver: None,
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct RecordingReaction {
    base: ReactionBase,
    tx: mpsc::UnboundedSender<QueryResult>,
}

impl std::fmt::Debug for RecordingReaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecordingReaction").finish()
    }
}

#[async_trait]
impl Reaction for RecordingReaction {
    fn id(&self) -> &str {
        &self.base.id
    }
    fn type_name(&self) -> &str {
        "controlled-timing-recorder"
    }
    fn properties(&self) -> HashMap<String, Value> {
        HashMap::new()
    }
    fn query_ids(&self) -> Vec<String> {
        self.base.queries.clone()
    }
    fn auto_start(&self) -> bool {
        true
    }
    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }
    async fn start(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }
    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.tx.send(result)?;
        Ok(())
    }
}

fn builder() -> (DrasiLibBuilder, Input) {
    let input = Input {
        base: Arc::new(SourceBase::new(SourceBaseParams::new("requests")).expect("valid source")),
        positions: Arc::new(Mutex::new(HashMap::new())),
    };
    (DrasiLib::builder().with_source(input.clone()), input)
}

fn query(id: &str, text: &str) -> drasi_lib::config::QueryConfig {
    Query::cypher(id)
        .query(text)
        .from_source("requests")
        .enable_bootstrap(false)
        .auto_start(true)
        .build()
}

struct Harness {
    core: DrasiLib,
    input: Input,
    results: mpsc::UnboundedReceiver<QueryResult>,
    sequence: u64,
}

impl Harness {
    async fn new(
        queries: &[(&str, &str, QueryTestControl)],
        provider: Option<Arc<dyn IndexBackendPlugin>>,
    ) -> Self {
        let (mut builder, input) = builder();
        for (id, text, control) in queries {
            builder = builder
                .with_query(query(id, text))
                .with_query_test_control(*id, control.clone());
        }
        if let Some(provider) = provider {
            builder = builder.with_default_index_provider("persistent", provider);
        }
        let (tx, results) = mpsc::unbounded_channel();
        let core = bounded(
            builder
                .with_reaction(RecordingReaction {
                    base: ReactionBase::new(ReactionBaseParams::new(
                        "recorder",
                        queries.iter().map(|(id, _, _)| id.to_string()).collect(),
                    )),
                    tx,
                })
                .build(),
        )
        .await
        .expect("build library");
        bounded(core.start()).await.expect("start library");
        Self {
            core,
            input,
            results,
            sequence: 0,
        }
    }

    async fn send(&mut self, change: SourceChange) {
        self.sequence += 1;
        bounded(
            self.input
                .base
                .dispatch_event(SourceEventWrapper::with_sequence(
                    "requests".into(),
                    SourceEvent::Change(change),
                    chrono::DateTime::from_timestamp_millis(self.sequence as i64)
                        .expect("test timestamp"),
                    self.sequence,
                    None,
                )),
        )
        .await
        .expect("dispatch source event");
        bounded(async {
            loop {
                let committed = {
                    let positions = self.input.positions.lock().expect("source positions lock");
                    !positions.is_empty()
                        && positions
                            .values()
                            .all(|p| p.load(Ordering::Acquire) == self.sequence)
                };
                if committed {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
    }

    async fn next(&mut self, query: &str, sequence: u64) -> QueryResult {
        let result = bounded(self.results.recv())
            .await
            .expect("reaction receipt");
        assert_eq!(result.query_id, query);
        assert_eq!(result.sequence, sequence);
        result
    }

    async fn rows(&self, query: &str) -> Vec<Value> {
        let mut rows = bounded(self.core.get_query_results(query))
            .await
            .expect("query rows");
        rows.sort_by_key(|row| row["id"].as_str().expect("row ID").to_string());
        rows
    }

    async fn stop(self) {
        bounded(self.core.shutdown())
            .await
            .expect("shutdown library");
    }
}

fn request(id: &str, time: u64, due: u64, generation: u64) -> Element {
    Element::Node {
        metadata: ElementMetadata {
            reference: ElementReference::new("requests", id),
            labels: Arc::new([Arc::from("Request")]),
            effective_from: time,
        },
        properties: json!({
            "id": id, "pending": true, "nextCheckAt": due, "generation": generation,
            "completedCheck": generation, "remoteState": "pending", "responseDeadline": 1000
        })
        .into(),
    }
}

async fn advance(control: &QueryTestControl, now: u64, sequence: u64) {
    assert_eq!(
        control
            .advance_to(now, LIMIT)
            .await
            .expect("completed drain"),
        DrainReport {
            physical_time_ms: now,
            output_sequence: sequence,
        }
    );
}

#[tokio::test]
async fn public_builder_two_deadlines_and_no_due_duplicate_fences() {
    let control = QueryTestControl::new(0);
    let mut h = Harness::new(&[("checks", QUERY, control.clone())], None).await;
    assert_eq!(control.wake(LIMIT).await.unwrap().output_sequence, 0);
    for (id, due) in [("one", 100), ("two", 200)] {
        h.send(SourceChange::Insert {
            element: request(id, 0, due, 0),
        })
        .await;
    }
    advance(&control, 99, 0).await;
    assert!(h.rows("checks").await.is_empty());
    advance(&control, 100, 1).await;
    assert!(matches!(&h.next("checks", 1).await.results[..],
        [ResultDiff::Add { data, .. }] if data["id"] == "one"));
    assert_eq!(h.rows("checks").await.len(), 1, "T2 is not consumed at T1");
    assert_eq!(control.wake(LIMIT).await.unwrap().output_sequence, 1);
    advance(&control, 200, 2).await;
    h.next("checks", 2).await;
    assert_eq!(h.rows("checks").await.len(), 2);
    advance(&control, 1000, 2).await;
    assert!(
        h.results.try_recv().is_err(),
        "stable rows are not recurring events"
    );
    h.stop().await;
}

#[tokio::test]
async fn completed_unchanged_checks_rearm_and_keep_expiry_frozen() {
    let checks = QueryTestControl::new(0);
    let expiry = QueryTestControl::new(0);
    let expiry_query = QUERY.replace("r.nextCheckAt", "r.responseDeadline");
    let mut h = Harness::new(
        &[("checks", QUERY, checks.clone()), ("expiry", &expiry_query, expiry.clone())],
        None,
    )
    .await;
    h.send(SourceChange::Insert {
        element: request("one", 0, 100, 0),
    })
    .await;
    for generation in 0..3 {
        let due = 100 * (generation + 1);
        advance(&checks, due, 2 * generation + 1).await;
        let result = h.next("checks", 2 * generation + 1).await;
        assert!(matches!(&result.results[..], [ResultDiff::Add { data, .. }]
            if data["generation"] == generation && data["completedCheck"] == generation
                && data["remoteState"] == "pending" && data["responseDeadline"] == 1000));
        advance(&expiry, due, 0).await;
        assert!(h.rows("expiry").await.is_empty());
        if generation < 2 {
            h.send(SourceChange::Update {
                element: request("one", due + 1, due + 100, generation + 1),
            })
            .await;
            let report = checks.wake(LIMIT).await.unwrap();
            assert_eq!(report.output_sequence, 2 * generation + 2);
            assert!(matches!(
                &h.next("checks", report.output_sequence).await.results[..],
                [ResultDiff::Delete { .. }]
            ));
            assert!(
                h.rows("checks").await.is_empty(),
                "stale wake cannot activate new generation"
            );
        }
    }
    advance(&expiry, 999, 0).await;
    advance(&expiry, 1000, 1).await;
    h.next("expiry", 1).await;
    assert_eq!(h.rows("expiry").await[0]["responseDeadline"], 1000);
    h.stop().await;
}

#[tokio::test]
async fn controls_are_instance_local_and_binding_errors_are_explicit() {
    let a = QueryTestControl::new(0);
    let b = QueryTestControl::new(0);
    let mut first = Harness::new(&[("checks", QUERY, a.clone())], None).await;
    let mut second = Harness::new(&[("checks", QUERY, b.clone())], None).await;
    for h in [&mut first, &mut second] {
        h.send(SourceChange::Insert {
            element: request("one", 0, 100, 0),
        })
        .await;
    }
    advance(&a, 100, 1).await;
    first.next("checks", 1).await;
    assert_eq!(b.wake(LIMIT).await.unwrap().physical_time_ms, 0);
    assert!(second.rows("checks").await.is_empty());
    advance(&b, 100, 1).await;
    second.next("checks", 1).await;

    let reused = bounded(
        builder()
            .0
            .with_query(query("checks", QUERY))
            .with_query_test_control("checks", a)
            .build(),
    )
    .await;
    assert!(matches!(reused, Err(DrasiError::InvalidConfig { .. })));
    let missing = bounded(
        builder()
            .0
            .with_query_test_control("missing", QueryTestControl::new(0))
            .build(),
    )
    .await;
    assert!(matches!(missing, Err(DrasiError::ComponentNotFound { .. })));
    let duplicate = bounded(
        builder()
            .0
            .with_query(query("checks", QUERY))
            .with_query_test_control("checks", QueryTestControl::new(0))
            .with_query_test_control("checks", QueryTestControl::new(0))
            .build(),
    )
    .await;
    assert!(matches!(duplicate, Err(DrasiError::InvalidConfig { .. })));
    first.stop().await;
    second.stop().await;
}

// Block the existing public outbox writer to prove a wake/commit is not a
// completed publication fence. This is test-owned, not a library fault API.
struct AppendGate {
    armed: AtomicBool,
    fail: AtomicBool,
    entered: Semaphore,
    release: Semaphore,
}

impl AppendGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            armed: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            entered: Semaphore::new(0),
            release: Semaphore::new(0),
        })
    }
    async fn wait(&self) {
        bounded(self.entered.acquire())
            .await
            .expect("append gate reached")
            .forget();
    }
}

struct GatedOutbox {
    inner: Arc<dyn OutboxWriter>,
    gate: Arc<AppendGate>,
}

#[async_trait]
impl OutboxWriter for GatedOutbox {
    async fn append(&self, id: &str, seq: u64, data: &[u8]) -> std::result::Result<(), IndexError> {
        if self.gate.armed.swap(false, Ordering::AcqRel) {
            self.gate.entered.add_permits(1);
            self.gate
                .release
                .acquire()
                .await
                .expect("append gate released")
                .forget();
        }
        if self.gate.fail.swap(false, Ordering::AcqRel) {
            return Err(IndexError::IOError);
        }
        self.inner.append(id, seq, data).await
    }
    async fn read_from(
        &self,
        id: &str,
        seq: u64,
    ) -> std::result::Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner.read_from(id, seq).await
    }
    async fn read_latest_sequence(&self, id: &str) -> std::result::Result<Option<u64>, IndexError> {
        self.inner.read_latest_sequence(id).await
    }
    async fn clear(&self, id: &str) -> std::result::Result<(), IndexError> {
        self.inner.clear(id).await
    }
    async fn trim_to_capacity(
        &self,
        id: &str,
        capacity: usize,
    ) -> std::result::Result<usize, IndexError> {
        self.inner.trim_to_capacity(id, capacity).await
    }
}

struct GatedProvider {
    inner: RocksDbIndexProvider,
    gate: Arc<AppendGate>,
    created: tokio::sync::Mutex<Option<PublishedState>>,
}

struct PublishedState {
    outbox: Arc<dyn OutboxWriter>,
    live: Arc<dyn LiveResultsWriter>,
    checkpoint: Arc<dyn CheckpointStore>,
}

#[async_trait]
impl IndexBackendPlugin for GatedProvider {
    async fn create_indexes(&self, id: &str) -> std::result::Result<CreatedIndexes, IndexError> {
        let mut indexes = self.inner.create_indexes(id).await?;
        indexes.outbox_writer = Some(Arc::new(GatedOutbox {
            inner: indexes.outbox_writer.clone().expect("persistent outbox"),
            gate: self.gate.clone(),
        }));
        *self.created.lock().await = Some(PublishedState {
            outbox: indexes.outbox_writer.clone().expect("persistent outbox"),
            live: indexes
                .live_results_writer
                .clone()
                .expect("persistent live results"),
            checkpoint: indexes
                .checkpoint_store
                .clone()
                .expect("persistent checkpoint store"),
        });
        Ok(indexes)
    }
    fn is_volatile(&self) -> bool {
        false
    }
}

async fn persistent(dir: &tempfile::TempDir) -> (Harness, QueryTestControl, Arc<GatedProvider>) {
    let provider = Arc::new(GatedProvider {
        inner: RocksDbIndexProvider::new(dir.path(), false, false),
        gate: AppendGate::new(),
        created: tokio::sync::Mutex::new(None),
    });
    let control = QueryTestControl::new(0);
    let mut h = Harness::new(
        &[("checks", QUERY, control.clone())],
        Some(provider.clone()),
    )
    .await;
    for (id, due) in [("one", 100), ("two", 200)] {
        h.send(SourceChange::Insert {
            element: request(id, 0, due, 0),
        })
        .await;
    }
    (h, control, provider)
}

#[tokio::test]
async fn fence_waits_for_publication_and_serializes_cloned_clock_commands() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, control, provider) = persistent(&dir).await;
    provider.gate.armed.store(true, Ordering::Release);
    let clone = control.clone();
    let mut first = Box::pin(control.advance_to(100, LIMIT));
    fn assert_send<T: Send>(_: &T) {}
    assert_send(&first);
    assert!(poll!(first.as_mut()).is_pending());
    provider.gate.wait().await;
    assert!(
        poll!(first.as_mut()).is_pending(),
        "commit/wake is not publication"
    );
    assert!(h.results.try_recv().is_err());
    let mut second = Box::pin(clone.advance_to(200, LIMIT));
    assert!(poll!(second.as_mut()).is_pending());
    assert_eq!(
        control.now(),
        100,
        "second command must not move the first clock"
    );
    provider.gate.release.add_permits(1);
    assert_eq!(
        first.await.unwrap(),
        DrainReport {
            physical_time_ms: 100,
            output_sequence: 1
        }
    );
    assert_eq!(
        second.await.unwrap(),
        DrainReport {
            physical_time_ms: 200,
            output_sequence: 2
        }
    );
    h.next("checks", 1).await;
    h.next("checks", 2).await;
    {
        let created = provider.created.lock().await;
        let indexes = created.as_ref().unwrap();
        assert_eq!(
            indexes.outbox.read_latest_sequence("checks").await.unwrap(),
            Some(2)
        );
        assert_eq!(
            indexes
                .checkpoint
                .read_result_sequence("checks")
                .await
                .unwrap(),
            Some(2)
        );
        assert_eq!(indexes.live.read_snapshot("checks").await.unwrap().len(), 2);
    }
    h.stop().await;
}

#[tokio::test]
async fn timeout_does_not_acknowledge_or_release_an_unfinished_drain() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, control, provider) = persistent(&dir).await;
    provider.gate.armed.store(true, Ordering::Release);
    let mut drain = Box::pin(control.advance_to(100, LIMIT));
    assert!(poll!(drain.as_mut()).is_pending());
    provider.gate.wait().await;
    tokio::time::pause();
    tokio::time::advance(LIMIT).await;
    assert!(matches!(
        drain.await,
        Err(DrasiError::OperationFailed { .. })
    ));
    assert!(matches!(
        control.advance_to(200, Duration::ZERO).await,
        Err(DrasiError::OperationFailed { .. })
    ));
    assert_eq!(control.now(), 100);
    tokio::time::resume();
    provider.gate.release.add_permits(1);
    assert_eq!(control.wake(LIMIT).await.unwrap().output_sequence, 1);
    h.next("checks", 1).await;
    assert!(matches!(
        control.advance_to(99, LIMIT).await,
        Err(DrasiError::InvalidConfig { .. })
    ));
    assert_eq!(control.now(), 100);
    advance(&control, 200, 2).await;
    h.next("checks", 2).await;
    h.stop().await;
    assert!(matches!(
        control.wake(LIMIT).await,
        Err(DrasiError::InvalidState { .. })
    ));
}

#[tokio::test]
async fn stopping_unblocks_an_inflight_fence_and_unbound_controls_fail() {
    let unbound = QueryTestControl::new(0);
    assert!(matches!(
        unbound.advance_to(100, LIMIT).await,
        Err(DrasiError::InvalidState { .. })
    ));
    assert_eq!(unbound.now(), 0);
    let dir = tempfile::tempdir().unwrap();
    let (h, control, provider) = persistent(&dir).await;
    provider.gate.armed.store(true, Ordering::Release);
    let mut drain = Box::pin(control.advance_to(100, LIMIT));
    assert!(poll!(drain.as_mut()).is_pending());
    provider.gate.wait().await;
    let (result, stopped) = bounded(async {
        tokio::join!(
            async {
                let result = drain.await;
                provider.gate.release.add_permits(1);
                result
            },
            h.core.stop_query("checks"),
        )
    })
    .await;
    assert!(matches!(result, Err(DrasiError::InvalidState { .. })));
    stopped.unwrap();
    assert!(matches!(
        control.wake(LIMIT).await,
        Err(DrasiError::InvalidState { .. })
    ));
    h.stop().await;
}

#[tokio::test]
async fn same_query_restart_reconnects_the_control() {
    let control = QueryTestControl::new(100);
    let h = Harness::new(&[("checks", QUERY, control.clone())], None).await;
    bounded(h.core.stop_query("checks")).await.unwrap();
    drasi_lib::wait_for_status(
        &h.core.component_graph(),
        "checks",
        &[ComponentStatus::Stopped],
        LIMIT,
    )
    .await
    .unwrap();
    assert!(matches!(
        control.wake(LIMIT).await,
        Err(DrasiError::InvalidState { .. })
    ));
    bounded(h.core.start_query("checks")).await.unwrap();
    advance(&control, 200, 0).await;
    h.stop().await;
}

#[tokio::test]
async fn cancelling_a_waiter_does_not_release_its_clock_while_processing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut h, control, provider) = persistent(&dir).await;
    provider.gate.armed.store(true, Ordering::Release);
    let mut cancelled = Box::pin(control.advance_to(100, LIMIT));
    assert!(poll!(cancelled.as_mut()).is_pending());
    provider.gate.wait().await;
    drop(cancelled);
    assert!(matches!(
        control.advance_to(200, Duration::ZERO).await,
        Err(DrasiError::OperationFailed { .. })
    ));
    assert_eq!(control.now(), 100);
    provider.gate.release.add_permits(1);
    assert_eq!(control.wake(LIMIT).await.unwrap().output_sequence, 1);
    h.next("checks", 1).await;
    advance(&control, 200, 2).await;
    h.next("checks", 2).await;
    h.stop().await;
}

#[tokio::test]
async fn output_failure_is_not_a_successful_empty_fence() {
    let dir = tempfile::tempdir().unwrap();
    let (h, control, provider) = persistent(&dir).await;
    provider.gate.fail.store(true, Ordering::Release);
    assert!(matches!(
        control.advance_to(100, LIMIT).await,
        Err(DrasiError::OperationFailed { .. })
    ));
    assert!(matches!(
        control.wake(LIMIT).await,
        Err(DrasiError::OperationFailed { .. })
    ));
    h.stop().await;
}
