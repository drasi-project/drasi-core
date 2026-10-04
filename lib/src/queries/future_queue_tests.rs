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

//! Exercises the real signaler, priority queue, manager drain and output dispatch.
//! The physical clock and persistence fault gates are controlled. Timeouts bound
//! hangs, not correctness.

use super::*;
use drasi_core::interface::{CreatedIndexes, IndexBackendPlugin, IndexError, SessionControl};
use drasi_core::models::{Element, ElementMetadata, ElementReference, SourceChange};
use drasi_index_rocksdb::RocksDbIndexProvider;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

const QUERY: &str = "MATCH (r:Request)
    WHERE drasi.trueLater(r.pending, r.nextCheckAt)
    RETURN r.id AS id, r.generation AS generation,
           r.completedCheck AS completedCheck, r.remoteState AS remoteState,
           r.responseDeadline AS responseDeadline";

struct TestSource {
    base: crate::sources::SourceBase,
    confirmed: Arc<AtomicU64>,
}

#[async_trait]
impl Source for TestSource {
    fn id(&self) -> &str {
        self.base.get_id()
    }
    fn type_name(&self) -> &str {
        "deadline-test"
    }
    fn properties(&self) -> HashMap<String, Value> {
        HashMap::new()
    }
    fn auto_start(&self) -> bool {
        false
    }
    fn supports_replay(&self) -> bool {
        true
    }
    async fn initialize(&self, context: crate::context::SourceRuntimeContext) {
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
        Ok(SubscriptionResponse {
            query_id: settings.query_id,
            source_id: self.id().into(),
            receiver: self.base.create_streaming_receiver().await?,
            bootstrap_receiver: None,
            position_handle: Some(self.confirmed.clone()),
            bootstrap_result_receiver: None,
        })
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

struct Harness {
    query: DrasiQuery,
    now: Arc<AtomicU64>,
    receiver: Box<dyn ChangeReceiver<QueryResult>>,
    sequence: u64,
}

impl Harness {
    async fn new() -> Self {
        Self::configured(QUERY, None, 0).await
    }

    async fn configured(
        query_text: &str,
        provider: Option<Arc<dyn IndexBackendPlugin>>,
        time: u64,
    ) -> Self {
        let (mut graph, _updates) = ComponentGraph::new("deadline-tests");
        graph.register_source("requests", HashMap::new()).unwrap();
        let update_tx = graph.update_sender();
        let source_manager = Arc::new(SourceManager::new(
            "deadline-tests",
            crate::managers::get_or_init_global_registry(),
            Arc::new(RwLock::new(graph)),
            update_tx,
        ));
        source_manager
            .provision_source(TestSource {
                base: crate::sources::SourceBase::new(crate::sources::SourceBaseParams::new(
                    "requests",
                ))
                .unwrap(),
                confirmed: Arc::new(AtomicU64::new(0)),
            })
            .await
            .unwrap();
        source_manager
            .get_source_instance("requests")
            .await
            .unwrap()
            .start()
            .await
            .unwrap();
        let (providers, backend) = match provider {
            Some(provider) => (
                HashMap::from([("persistent".into(), provider)]),
                Some(crate::indexes::StorageBackendRef::Named(
                    "persistent".into(),
                )),
            ),
            None => (HashMap::new(), None),
        };
        let mut query = DrasiQuery::new(
            "deadline-tests",
            crate::Query::cypher("checks")
                .query(query_text)
                .from_source("requests")
                .enable_bootstrap(false)
                .build(),
            source_manager,
            Arc::new(crate::indexes::IndexFactory::new_with_default(
                vec![],
                providers,
                backend,
            )),
            Arc::new(MiddlewareTypeRegistry::new()),
            None,
        )
        .unwrap();
        let now = Arc::new(AtomicU64::new(time));
        query.future_now_override = Some(now.clone());
        let receiver = query.base.subscribe("test").await.unwrap().receiver;
        query.start().await.unwrap();
        query.wait_until_running().await.unwrap();
        let sequence = query
            .checkpoint_store
            .read()
            .await
            .as_ref()
            .unwrap()
            .read_all_checkpoints()
            .await
            .unwrap()
            .get("requests")
            .map_or(0, |cp| cp.sequence);
        Self {
            query,
            now,
            receiver,
            sequence,
        }
    }

    async fn send(&mut self, change: SourceChange) {
        self.sequence += 1;
        let event = SourceEventWrapper::with_sequence(
            "requests".into(),
            SourceEvent::Change(change),
            chrono::DateTime::from_timestamp_millis(10_000 + self.sequence as i64).unwrap(),
            self.sequence,
            None,
        );
        let source = self
            .query
            .source_manager
            .get_source_instance("requests")
            .await
            .unwrap();
        let source = source.as_any().downcast_ref::<TestSource>().unwrap();
        source.base.dispatch_event(event).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                // Checkpoint reads can see staged writes. This handle advances
                // only after the manager observes a successful Core commit.
                if source.confirmed.load(Ordering::Acquire) == self.sequence {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("source change committed");
    }

    async fn fence(&mut self) {
        // A later priority-queue event proves that the preceding due drain finished.
        self.send(SourceChange::Delete {
            metadata: ElementMetadata {
                reference: ElementReference::new("requests", "fence"),
                labels: Arc::new([]),
                effective_from: 0,
            },
        })
        .await;
    }

    async fn stale_signal(&self) {
        self.query
            .priority_queue
            .enqueue_wait(Arc::new(SourceEventWrapper::new(
                "__future_queue__".into(),
                SourceEvent::Control(SourceControl::FuturesDue),
                chrono::DateTime::from_timestamp_millis(100).unwrap(),
            )))
            .await;
    }

    async fn next(&mut self) -> Arc<QueryResult> {
        tokio::time::timeout(Duration::from_secs(10), self.receiver.recv())
            .await
            .expect("timer result without new source events")
            .unwrap()
    }

    async fn rows(&mut self) -> Vec<Value> {
        self.fence().await;
        let mut rows = self.query.get_current_results().await;
        rows.sort_by_key(|row| row["id"].as_str().unwrap().to_owned());
        rows
    }

    async fn stop(self) {
        self.query.stop().await.unwrap();
    }

    async fn crash(self) {
        // Cancel the processor exactly at the injected boundary, without graceful
        // draining. Abort drops its session guard; all DB owners are then dropped.
        let task = self.query.base.task_handle.write().await.take().unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        self.query.stop().await.unwrap();
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
            "id": id, "pending": true, "nextCheckAt": due,
            "generation": generation, "completedCheck": generation,
            "remoteState": "pending", "responseDeadline": 1000
        })
        .into(),
    }
}

#[tokio::test]
async fn two_deadlines_do_not_drain_the_later_future() {
    let mut h = Harness::new().await;
    h.send(SourceChange::Insert {
        element: request("one", 0, 100, 0),
    })
    .await;
    h.send(SourceChange::Insert {
        element: request("two", 0, 200, 0),
    })
    .await;
    h.now.store(100, Ordering::Release);
    let first = h.next().await;
    assert_eq!(first.sequence, 1);
    let rows = h.rows().await;
    assert_eq!(rows.len(), 1, "T1 must not activate T2");
    assert_eq!(rows[0]["id"], "one");
    h.now.store(200, Ordering::Release);
    assert_eq!(h.next().await.sequence, 2);
    assert_eq!(h.rows().await.len(), 2);
    h.stale_signal().await;
    assert_eq!(h.rows().await.len(), 2);
    assert_eq!(h.query.output_state.read().await.as_of_sequence(), 2);
    // Re-delivering an already committed source sequence cannot move the
    // deadline or introduce a second activation.
    let replay = SourceEventWrapper::with_sequence(
        "requests".into(),
        SourceEvent::Change(SourceChange::Update {
            element: request("one", 201, 300, 1),
        }),
        chrono::DateTime::from_timestamp_millis(10_000 + h.sequence as i64).unwrap(),
        h.sequence,
        None,
    );
    let source = h
        .query
        .source_manager
        .get_source_instance("requests")
        .await
        .unwrap();
    source
        .as_any()
        .downcast_ref::<TestSource>()
        .unwrap()
        .base
        .dispatch_event(replay)
        .await
        .unwrap();
    assert_eq!(h.rows().await[0]["generation"], 0);
    h.now.store(300, Ordering::Release);
    h.stale_signal().await;
    assert_eq!(h.rows().await.len(), 2);
    assert_eq!(h.query.output_state.read().await.as_of_sequence(), 2);
    h.stop().await;
}

#[tokio::test]
async fn stale_signal_does_not_consume_rescheduled_deadline() {
    let mut h = Harness::new().await;
    h.send(SourceChange::Insert {
        element: request("one", 0, 100, 0),
    })
    .await;
    h.send(SourceChange::Update {
        element: request("one", 50, 200, 1),
    })
    .await;
    h.now.store(100, Ordering::Release);
    h.stale_signal().await;
    assert!(
        h.rows().await.is_empty(),
        "old signal cannot activate generation 1"
    );
    h.now.store(200, Ordering::Release);
    let result = h.next().await;
    assert_eq!(result.sequence, 1);
    assert_eq!(h.rows().await[0]["generation"], 1);
    h.stop().await;
}

#[tokio::test]
async fn unchanged_checks_rearm_without_extending_response_deadline() {
    let mut checks = Harness::new().await;
    let mut expiry = Harness::configured(
        &QUERY.replace("r.nextCheckAt", "r.responseDeadline"),
        None,
        0,
    )
    .await;
    let initial = SourceChange::Insert {
        element: request("one", 0, 100, 0),
    };
    checks.send(initial.clone()).await;
    expiry.send(initial).await;
    for generation in 0..3 {
        let due = 100 * (generation + 1);
        checks.now.store(due, Ordering::Release);
        expiry.now.store(due, Ordering::Release);
        let event = checks.next().await;
        assert!(matches!(&event.results[..], [ResultDiff::Add { data, .. }]
            if data["generation"] == generation && data["completedCheck"] == generation
                && data["remoteState"] == "pending" && data["responseDeadline"] == 1000));
        assert!(expiry.rows().await.is_empty());
        if generation < 2 {
            // A completed check is a new observation even when remote state is unchanged.
            let completed = SourceChange::Update {
                element: request("one", due + 1, due + 100, generation + 1),
            };
            checks.send(completed.clone()).await;
            expiry.send(completed).await;
            assert!(matches!(
                &checks.next().await.results[..],
                [ResultDiff::Delete { .. }]
            ));
            assert!(checks.rows().await.is_empty());
            checks.stale_signal().await;
            assert!(checks.rows().await.is_empty());
        }
    }
    expiry.now.store(999, Ordering::Release);
    expiry.stale_signal().await;
    assert!(expiry.rows().await.is_empty());
    expiry.now.store(1000, Ordering::Release);
    assert_eq!(expiry.next().await.sequence, 1);
    assert_eq!(expiry.rows().await[0]["responseDeadline"], 1000);
    checks.stop().await;
    expiry.stop().await;
}

#[tokio::test]
async fn cancellation_retraction_and_duplicate_signals_do_not_reactivate() {
    let mut h = Harness::new().await;
    h.send(SourceChange::Insert {
        element: request("one", 0, 100, 0),
    })
    .await;
    h.now.store(100, Ordering::Release);
    assert!(matches!(
        &h.next().await.results[..],
        [ResultDiff::Add { .. }]
    ));
    let mut cancelled = request("one", 101, 200, 1);
    if let Element::Node { properties, .. } = &mut cancelled {
        properties.insert("pending", drasi_core::models::ElementValue::Bool(false));
    }
    h.send(SourceChange::Update { element: cancelled }).await;
    assert!(matches!(
        &h.next().await.results[..],
        [ResultDiff::Delete { .. }]
    ));
    h.now.store(200, Ordering::Release);
    for _ in 0..3 {
        h.stale_signal().await;
    }
    assert!(h.rows().await.is_empty());
    assert_eq!(h.query.output_state.read().await.as_of_sequence(), 2);
    h.send(SourceChange::Insert {
        element: request("deleted", 200, 300, 0),
    })
    .await;
    h.send(SourceChange::Delete {
        metadata: request("deleted", 250, 300, 0).get_metadata().clone(),
    })
    .await;
    h.now.store(300, Ordering::Release);
    h.stale_signal().await;
    assert!(h.rows().await.is_empty());
    assert_eq!(h.query.output_state.read().await.as_of_sequence(), 2);
    h.stop().await;
}

#[derive(Clone, Copy, PartialEq)]
enum FaultPoint {
    BeforeCommit,
    BeforeAppend,
    AfterAppend,
}

struct Fault {
    point: FaultPoint,
    armed: AtomicBool,
    reached: Notify,
    release: Notify,
}

impl Fault {
    fn new(point: FaultPoint) -> Arc<Self> {
        Arc::new(Self {
            point,
            armed: AtomicBool::new(false),
            reached: Notify::new(),
            release: Notify::new(),
        })
    }
    async fn hit(&self, point: FaultPoint) {
        if self.point == point && self.armed.swap(false, Ordering::AcqRel) {
            self.reached.notify_one();
            self.release.notified().await;
        }
    }
    async fn wait(&self) {
        tokio::time::timeout(Duration::from_secs(10), self.reached.notified())
            .await
            .expect("injected persistence boundary reached");
    }
}

struct FaultSession {
    inner: Arc<dyn SessionControl>,
    fault: Arc<Fault>,
}

#[async_trait]
impl SessionControl for FaultSession {
    async fn begin(&self) -> std::result::Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> std::result::Result<(), IndexError> {
        self.fault.hit(FaultPoint::BeforeCommit).await;
        self.inner.commit().await
    }
    fn rollback(&self) -> std::result::Result<(), IndexError> {
        self.inner.rollback()
    }
}

struct FaultOutbox {
    inner: Arc<dyn OutboxWriter>,
    fault: Arc<Fault>,
}

#[async_trait]
impl OutboxWriter for FaultOutbox {
    async fn append(&self, id: &str, seq: u64, data: &[u8]) -> std::result::Result<(), IndexError> {
        self.fault.hit(FaultPoint::BeforeAppend).await;
        self.inner.append(id, seq, data).await?;
        self.fault.hit(FaultPoint::AfterAppend).await;
        Ok(())
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

struct FaultProvider {
    inner: RocksDbIndexProvider,
    fault: Arc<Fault>,
}

#[async_trait]
impl IndexBackendPlugin for FaultProvider {
    async fn create_indexes(&self, id: &str) -> std::result::Result<CreatedIndexes, IndexError> {
        let mut indexes = self.inner.create_indexes(id).await?;
        indexes.set.session_control = Arc::new(FaultSession {
            inner: indexes.set.session_control,
            fault: self.fault.clone(),
        });
        indexes.outbox_writer = Some(Arc::new(FaultOutbox {
            inner: indexes.outbox_writer.unwrap(),
            fault: self.fault.clone(),
        }));
        Ok(indexes)
    }
    fn is_volatile(&self) -> bool {
        false
    }
}

async fn assert_crash_boundary(point: FaultPoint) {
    let dir = tempfile::tempdir().unwrap();
    let fault = Fault::new(point);
    let provider = Arc::new(FaultProvider {
        inner: RocksDbIndexProvider::new(dir.path(), false, false),
        fault: fault.clone(),
    });
    let mut h = Harness::configured(QUERY, Some(provider), 0).await;
    h.send(SourceChange::Insert {
        element: request("one", 0, 100, 0),
    })
    .await;
    fault.armed.store(true, Ordering::Release);
    h.now.store(100, Ordering::Release);
    fault.wait().await;
    h.crash().await;

    // Reopen the real DB with fresh providers/queues/manager/output state.
    let provider = Arc::new(RocksDbIndexProvider::new(dir.path(), false, false));
    let mut reopened = Harness::configured(QUERY, Some(provider.clone()), 0).await;
    let fq = reopened
        .query
        .future_queue_source
        .read()
        .await
        .as_ref()
        .unwrap()
        .future_queue_for_test();
    let outbox = reopened.query.outbox_writer.read().await.clone().unwrap();
    match point {
        FaultPoint::BeforeCommit => {
            assert_eq!(fq.peek_due_time().await.unwrap(), Some(100));
            assert!(outbox.read_from("checks", 0).await.unwrap().is_empty());
            reopened.now.store(100, Ordering::Release);
            assert_eq!(reopened.next().await.sequence, 1);
            assert_eq!(reopened.rows().await.len(), 1);
        }
        FaultPoint::BeforeAppend => {
            assert_eq!(fq.peek_due_time().await.unwrap(), None);
            assert!(outbox.read_from("checks", 0).await.unwrap().is_empty());
            reopened.now.store(100, Ordering::Release);
            reopened.stale_signal().await;
            assert!(
                reopened.rows().await.is_empty(),
                "committed pop is not a durable notification"
            );
            assert_eq!(reopened.query.output_state.read().await.as_of_sequence(), 0);
            // A fresh completed-check generation can reconcile/rearm, not a retry loop.
            reopened
                .send(SourceChange::Update {
                    element: request("one", 101, 200, 1),
                })
                .await;
            reopened.fence().await;
            reopened.now.store(200, Ordering::Release);
            let result = reopened.next().await;
            assert!(matches!(&result.results[..], [ResultDiff::Delete { .. }]));
            let result = reopened.next().await;
            assert!(
                matches!(&result.results[..], [ResultDiff::Add { data, .. }] if data["generation"] == 1)
            );
            assert_eq!(reopened.rows().await[0]["generation"], 1);
        }
        FaultPoint::AfterAppend => {
            assert_eq!(fq.peek_due_time().await.unwrap(), None);
            assert_eq!(outbox.read_from("checks", 0).await.unwrap().len(), 1);
            assert_eq!(
                reopened.rows().await.len(),
                1,
                "durable outbox repairs the missing live snapshot"
            );
            assert_eq!(reopened.query.output_state.read().await.as_of_sequence(), 1);
            reopened.now.store(100, Ordering::Release);
            reopened.stale_signal().await;
            assert_eq!(reopened.rows().await.len(), 1);
            assert_eq!(reopened.query.output_state.read().await.as_of_sequence(), 1);
        }
    }
    drop(fq);
    drop(outbox);
    reopened.stop().await;
}

#[tokio::test]
async fn persistent_precommit_cancellation_restores_timer_on_reopen() {
    assert_crash_boundary(FaultPoint::BeforeCommit).await;
}

#[tokio::test]
async fn persistent_postcommit_preappend_crash_loses_notification() {
    assert_crash_boundary(FaultPoint::BeforeAppend).await;
}

#[tokio::test]
async fn persistent_postappend_crash_recovers_snapshot_without_refiring() {
    assert_crash_boundary(FaultPoint::AfterAppend).await;
}

#[tokio::test]
async fn persistent_reschedule_serializes_a_queued_stale_signal() {
    let dir = tempfile::tempdir().unwrap();
    let fault = Fault::new(FaultPoint::BeforeCommit);
    let provider = Arc::new(FaultProvider {
        inner: RocksDbIndexProvider::new(dir.path(), false, false),
        fault: fault.clone(),
    });
    let mut h = Harness::configured(QUERY, Some(provider), 0).await;
    h.send(SourceChange::Insert {
        element: request("one", 0, 100, 0),
    })
    .await;
    let now = h.now.clone();
    let queue = h.query.priority_queue.clone();
    fault.armed.store(true, Ordering::Release);
    {
        let update = h.send(SourceChange::Update {
            element: request("one", 50, 200, 1),
        });
        tokio::pin!(update);
        tokio::select! {
            _ = fault.wait() => {},
            _ = &mut update => panic!("source reschedule must wait at its commit boundary"),
        }
        now.store(100, Ordering::Release);
        queue
            .enqueue_wait(Arc::new(SourceEventWrapper::new(
                "__future_queue__".into(),
                SourceEvent::Control(SourceControl::FuturesDue),
                chrono::DateTime::from_timestamp_millis(100).unwrap(),
            )))
            .await;
        fault.release.notify_one();
        update.await;
    }
    assert!(h.rows().await.is_empty());
    h.now.store(200, Ordering::Release);
    assert!(
        matches!(&h.next().await.results[..], [ResultDiff::Add { data, .. }] if data["generation"] == 1)
    );
    h.stop().await;
}

#[tokio::test]
async fn persistent_pending_deadlines_survive_reopen_and_fire_once_each() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(RocksDbIndexProvider::new(dir.path(), false, false));
    let mut h = Harness::configured(QUERY, Some(provider.clone()), 0).await;
    for (id, due) in [("one", 100), ("two", 200)] {
        h.send(SourceChange::Insert {
            element: request(id, 0, due, 0),
        })
        .await;
    }
    h.stop().await;
    let mut h = Harness::configured(QUERY, Some(provider.clone()), 100).await;
    assert_eq!(h.next().await.sequence, 1);
    assert_eq!(h.rows().await.len(), 1);
    h.stop().await;
    let mut h = Harness::configured(QUERY, Some(provider), 200).await;
    assert_eq!(h.next().await.sequence, 2);
    assert_eq!(h.rows().await.len(), 2);
    h.stale_signal().await;
    assert_eq!(h.rows().await.len(), 2);
    assert_eq!(h.query.output_state.read().await.as_of_sequence(), 2);
    h.stop().await;
}

#[tokio::test]
async fn late_wakeup_preserves_scheduled_logical_realtime() {
    let mut h = Harness::configured(
        "MATCH (r:Request) WHERE drasi.trueLater(r.pending, r.nextCheckAt)
                 RETURN datetime.realtime().epochMillis AS realtime,
                        datetime.transaction().epochMillis AS transactionTime",
        None,
        0,
    )
    .await;
    h.send(SourceChange::Insert {
        element: request("one", 7, 100, 0),
    })
    .await;
    h.now.store(900, Ordering::Release);
    let event = h.next().await;
    assert!(matches!(&event.results[..], [ResultDiff::Add { data, .. }]
                if data["realtime"] == 100 && data["transactionTime"] == 7));
    h.stop().await;
}
