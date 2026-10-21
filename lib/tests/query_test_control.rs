// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use std::{collections::HashMap, sync::Arc, time::Duration};

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_lib::{
    channels::{
        ChangeDispatcher, ChannelChangeDispatcher, ComponentStatusHandle,
        QuerySubscriptionResponse, SourceEvent, SourceEventWrapper,
    },
    computation::v1::{ComponentId, ComponentLifecycle},
    ComponentStatus, DrasiLib, Query, Source, SourceRuntimeContext, SourceSubscriptionSettings,
    SubscriptionResponse,
};
use serde_json::json;
use tokio::sync::Mutex;

const WAIT: Duration = Duration::from_secs(10);
const EXPIRY: &str = "MATCH (n:Item)
RETURN n.id AS id, n.revision AS revision,
drasi.trueNowOrLater(datetime.realtime().epochMillis >= n.deadline, n.deadline) AS expired,
datetime.realtime().epochMillis AS logicalTime";

#[derive(Clone)]
struct Input {
    status: ComponentStatusHandle,
    state: Arc<Mutex<InputState>>,
}

#[derive(Default)]
struct InputState {
    history: Vec<Arc<SourceEventWrapper>>,
    subscribers: HashMap<String, ChannelChangeDispatcher<SourceEventWrapper>>,
}

impl Input {
    fn new() -> Self {
        Self {
            status: ComponentStatusHandle::new("input"),
            state: Arc::new(Mutex::new(InputState::default())),
        }
    }

    async fn send(&self, id: &str, logical: u64, deadline: u64, revision: u64) -> Result<()> {
        let element = Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("input", id),
                labels: Arc::from([Arc::from("Item")]),
                effective_from: logical,
            },
            properties: ElementPropertyMap::from(json!({
                "id": id, "deadline": deadline, "revision": revision, "ready": true,
            })),
        };
        let mut state = self.state.lock().await;
        let change = if revision == 0 {
            SourceChange::Insert { element }
        } else {
            SourceChange::Update { element }
        };
        let event = Arc::new(SourceEventWrapper::new(
            "input".into(),
            SourceEvent::Change(change),
            chrono::DateTime::from_timestamp_millis(logical.try_into()?)
                .expect("fixture timestamp"),
            state.history.len() as u64 + 1,
        ));
        state.history.push(event.clone());
        for dispatcher in state.subscribers.values() {
            dispatcher.dispatch_change(event.clone()).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Source for Input {
    fn id(&self) -> &str {
        "input"
    }
    fn type_name(&self) -> &str {
        "controlled-input-fixture"
    }
    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }
    fn supports_replay(&self) -> bool {
        true
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    async fn initialize(&self, context: SourceRuntimeContext) {
        self.status.wire(context.update_tx).await;
    }
    async fn start(&self) -> Result<()> {
        self.status.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }
    async fn stop(&self) -> Result<()> {
        self.state.lock().await.subscribers.clear();
        self.status.set_status(ComponentStatus::Stopped, None).await;
        Ok(())
    }
    async fn status(&self) -> ComponentStatus {
        self.status.get_status().await
    }
    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        let dispatcher = ChannelChangeDispatcher::new(64);
        let receiver = dispatcher.create_receiver().await?;
        let mut state = self.state.lock().await;
        for event in &state.history {
            if event.sequence > settings.resume_sequence.unwrap_or(0) {
                dispatcher.dispatch_change(event.clone()).await?;
            }
        }
        state
            .subscribers
            .insert(settings.query_id.clone(), dispatcher);
        Ok(SubscriptionResponse {
            source_id: "input".into(),
            query_id: settings.query_id,
            receiver,
            bootstrap_receiver: None,
            bootstrap_result_receiver: None,
            position_handle: None,
        })
    }
}

fn query(id: &str, text: &str) -> Query {
    Query::cypher(id)
        .query(text)
        .from_source("input")
        .enable_bootstrap(false)
}

async fn subscribe(core: &DrasiLib, id: &str) -> Result<QuerySubscriptionResponse> {
    let inspector = core.inspect_query_computation(id).await?;
    let mut changes = inspector.subscribe();
    tokio::time::timeout(WAIT, async {
        loop {
            let snapshot = changes.borrow_and_update().clone();
            let node = &snapshot.observed.components[&ComponentId::try_new(id)?];
            if let Some(failure) = &node.failure {
                anyhow::bail!("{:#}", failure.cause);
            }
            if node.lifecycle == ComponentLifecycle::Running {
                return Ok::<_, anyhow::Error>(());
            }
            changes.changed().await?;
        }
    })
    .await??;
    let query = core
        .query_manager()
        .get_query_instance(id)
        .await
        .map_err(anyhow::Error::msg)?;
    query.subscribe("test".into()).await
}

async fn receive(subscription: &mut QuerySubscriptionResponse, sequence: u64) -> Result<()> {
    let result = tokio::time::timeout(WAIT, subscription.receiver.recv()).await??;
    assert_eq!(result.sequence, sequence);
    Ok(())
}

async fn rows(core: &DrasiLib, id: &str) -> Result<(u64, Vec<serde_json::Value>)> {
    let query = core
        .query_manager()
        .get_query_instance(id)
        .await
        .map_err(anyhow::Error::msg)?;
    let snapshot = query.fetch_snapshot().await?;
    Ok((snapshot.as_of_sequence, snapshot.to_vec()))
}

// Runs both with the feature off and with it enabled but no attached control.
#[tokio::test]
async fn production_default_scheduler_uses_wall_clock_without_test_controls() -> Result<()> {
    let source = Input::new();
    let core = DrasiLib::builder()
        .with_source(source.clone())
        .with_query(query("query", EXPIRY).build())
        .build()
        .await?;
    core.start().await?;
    let mut output = subscribe(&core, "query").await?;
    source.send("a", 1000, 2000, 0).await?;
    receive(&mut output, 1).await?;
    receive(&mut output, 2).await?;
    let (sequence, rows) = rows(&core, "query").await?;
    assert_eq!(sequence, 2);
    assert_eq!(rows[0]["expired"], true);
    assert_eq!(rows[0]["logicalTime"], 2000);
    assert_eq!(source.state.lock().await.history.len(), 1);
    core.shutdown().await?;
    Ok(())
}

#[cfg(feature = "test-support")]
mod controlled {
    use super::*;
    use drasi_core::interface::{
        CreatedIndexes, FutureElementRef, FutureQueue, IndexBackendPlugin, IndexError, PushType,
        SessionControl,
    };
    use drasi_index_rocksdb::RocksDbIndexProvider;
    use drasi_lib::{
        computation::v1::QueryPublicationMode, test_support::QueryTestControl, StorageBackendRef,
    };
    use std::sync::atomic::{AtomicU8, Ordering};
    use tokio::sync::Notify;

    const SHORT: Duration = Duration::from_millis(50);
    const TRUE_FOR: &str = "MATCH (n:Item)
RETURN n.id AS id, n.revision AS revision,
drasi.trueFor(n.ready, duration({milliseconds:1000})) AS expired";

    async fn open(
        source: &Input,
        control: &QueryTestControl,
        text: &str,
        provider: Option<Arc<dyn IndexBackendPlugin>>,
        small_output: bool,
    ) -> Result<(DrasiLib, QuerySubscriptionResponse)> {
        let mut query = query("query", text);
        if small_output {
            query = query
                .with_outbox_capacity(2)
                .with_dispatch_buffer_capacity(1);
        }
        let mut builder = DrasiLib::builder()
            .with_id("timing-tests")
            .with_source(source.clone());
        if let Some(provider) = provider {
            query = query.with_storage_backend(StorageBackendRef::Named("store".into()));
            builder = builder.with_index_provider("store", provider);
        }
        let core = builder
            .with_query(query.build())
            .with_query_test_control("query", control.clone())
            .build()
            .await?;
        core.start().await?;
        let output = subscribe(&core, "query").await?;
        Ok((core, output))
    }

    async fn assert_expired(core: &DrasiLib, id: &str, expired: bool) -> Result<()> {
        let (_, rows) = rows(core, "query").await?;
        let expected = if expired {
            json!(true)
        } else {
            json!("Awaiting")
        };
        assert!(
            rows.iter()
                .any(|row| row["id"] == id && row["expired"] == expected),
            "{rows:?}"
        );
        Ok(())
    }

    #[tokio::test]
    async fn boundaries_and_stale_wakes_use_real_query_output() -> Result<()> {
        two_deadlines().await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boundaries_also_work_on_multithread_runtime() -> Result<()> {
        two_deadlines().await
    }

    async fn two_deadlines() -> Result<()> {
        let source = Input::new();
        let control = QueryTestControl::new(1000);
        let (core, mut output) = open(&source, &control, EXPIRY, None, false).await?;
        source.send("a", 1000, 2000, 0).await?;
        receive(&mut output, 1).await?;
        source.send("b", 1000, 3000, 0).await?;
        receive(&mut output, 2).await?;
        assert_eq!(control.advance_to(1999, WAIT).await?.output_sequence, 2);
        assert_expired(&core, "a", false).await?;
        let first = control.advance_to(2000, WAIT).await?;
        assert_eq!((first.physical_time_ms, first.output_sequence), (2000, 3));
        assert!(!first.output_persistent);
        assert_expired(&core, "a", true).await?;
        assert_expired(&core, "b", false).await?;
        receive(&mut output, 3).await?;
        assert_eq!(control.wake(WAIT).await?, first);
        assert_eq!(control.advance_to(2999, WAIT).await?.output_sequence, 3);
        assert_eq!(control.advance_to(9000, WAIT).await?.output_sequence, 4);
        let (_, rows) = rows(&core, "query").await?;
        assert_eq!(
            rows.iter()
                .find(|row| row["id"] == "b")
                .expect("second scheduled row")["logicalTime"],
            3000,
            "late physical wake must evaluate at logical due time"
        );
        assert_eq!(
            source.state.lock().await.history.len(),
            2,
            "drains cannot publish source ingress"
        );
        assert!(control.advance_to(8999, WAIT).await.is_err());
        assert!(control.advance_to(u64::MAX, WAIT).await.is_err());
        assert_eq!(control.now(), 9000);
        core.stop_query("query").await?;
        assert!(control.wake(WAIT).await.is_err());
        core.start_query("query").await?;
        subscribe(&core, "query").await?;
        assert_eq!(control.wake(WAIT).await?.output_sequence, 4);
        core.shutdown().await?;
        assert!(control.wake(WAIT).await.is_err());
        Ok(())
    }

    #[tokio::test]
    async fn independently_controlled_queries_share_no_clock_with_each_other_or_production(
    ) -> Result<()> {
        let source = Input::new();
        let check = QueryTestControl::new(1000);
        let deadline = QueryTestControl::new(1000);
        let core = DrasiLib::builder()
            .with_source(source.clone())
            .with_query(query("check", EXPIRY).build())
            .with_query(query("deadline", EXPIRY).build())
            .with_query(query("uncontrolled", EXPIRY).build())
            .with_query_test_control("check", check.clone())
            .with_query_test_control("deadline", deadline.clone())
            .build()
            .await?;
        core.start().await?;
        let mut c = subscribe(&core, "check").await?;
        let mut d = subscribe(&core, "deadline").await?;
        let mut u = subscribe(&core, "uncontrolled").await?;
        source.send("a", 1000, 2000, 0).await?;
        receive(&mut c, 1).await?;
        receive(&mut d, 1).await?;
        receive(&mut u, 1).await?;
        receive(&mut u, 2).await?;
        assert_eq!(check.advance_to(1999, WAIT).await?.output_sequence, 1);
        assert_eq!(check.advance_to(2000, WAIT).await?.output_sequence, 2);
        assert_eq!(deadline.now(), 1000);
        assert_eq!(deadline.wake(WAIT).await?.output_sequence, 1);
        assert_eq!(deadline.advance_to(2000, WAIT).await?.output_sequence, 2);
        core.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn unchanged_checks_keep_the_original_deadline() -> Result<()> {
        let source = Input::new();
        let control = QueryTestControl::new(1000);
        let (core, mut output) = open(&source, &control, TRUE_FOR, None, false).await?;
        source.send("a", 1000, 0, 0).await?;
        receive(&mut output, 1).await?;
        for revision in 1..=9 {
            source.send("a", 1000 + revision * 100, 0, revision).await?;
            receive(&mut output, revision + 1).await?;
            assert_eq!(
                control
                    .advance_to(1000 + revision * 100, WAIT)
                    .await?
                    .output_sequence,
                revision + 1
            );
            assert_expired(&core, "a", false).await?;
        }
        assert_eq!(control.advance_to(1999, WAIT).await?.output_sequence, 10);
        assert_eq!(control.advance_to(2000, WAIT).await?.output_sequence, 11);
        assert_expired(&core, "a", true).await?;
        core.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn stale_wakes_cannot_consume_a_newer_generation() -> Result<()> {
        let source = Input::new();
        let control = QueryTestControl::new(1000);
        let (core, mut output) = open(&source, &control, EXPIRY, None, false).await?;
        source.send("a", 1000, 2000, 0).await?;
        receive(&mut output, 1).await?;
        source.send("a", 1500, 5000, 1).await?;
        receive(&mut output, 2).await?;
        for _ in 0..3 {
            assert_eq!(control.advance_to(2000, WAIT).await?.output_sequence, 2);
            assert_eq!(control.wake(WAIT).await?.output_sequence, 2);
        }
        assert_expired(&core, "a", false).await?;
        assert_eq!(control.advance_to(4999, WAIT).await?.output_sequence, 2);
        assert_eq!(control.advance_to(5000, WAIT).await?.output_sequence, 3);
        assert_expired(&core, "a", true).await?;
        core.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn true_later_rearms_at_exact_boundaries_without_extra_source_events() -> Result<()> {
        let source = Input::new();
        let control = QueryTestControl::new(1000);
        let text = "MATCH (n:Item) RETURN n.id AS id, n.revision AS revision,
drasi.trueLater(n.ready, n.deadline) AS expired,
datetime.realtime().epochMillis AS logicalTime";
        let (core, mut output) = open(&source, &control, text, None, false).await?;
        for generation in 0..3 {
            let now = 1000 + generation * 1000;
            let due = now + 1000;
            source.send("a", now, due, generation).await?;
            receive(&mut output, generation * 2 + 1).await?;
            assert_eq!(
                control.wake(WAIT).await?.output_sequence,
                generation * 2 + 1
            );
            assert_expired(&core, "a", false).await?;
            assert_eq!(
                control.advance_to(due - 1, WAIT).await?.output_sequence,
                generation * 2 + 1
            );
            let report = control.advance_to(due, WAIT).await?;
            assert_eq!(report.output_sequence, generation * 2 + 2);
            receive(&mut output, report.output_sequence).await?;
            let (_, rows) = rows(&core, "query").await?;
            assert_eq!(rows[0]["logicalTime"], due);
            assert_expired(&core, "a", true).await?;
            assert_eq!(control.wake(WAIT).await?, report);
        }
        assert_eq!(source.state.lock().await.history.len(), 3);
        core.shutdown().await?;
        Ok(())
    }

    async fn blocked_output() -> Result<(DrasiLib, QueryTestControl, QuerySubscriptionResponse)> {
        let source = Input::new();
        let control = QueryTestControl::new(1000);
        let (core, mut output) = open(&source, &control, EXPIRY, None, true).await?;
        for n in 0..8 {
            source.send(&n.to_string(), 1000, 2000, 0).await?;
            receive(&mut output, n + 1).await?;
        }
        let error = control
            .advance_to(2000, SHORT)
            .await
            .expect_err("output is blocked");
        assert!(error.to_string().contains("timed out"));
        let (sequence, _) = rows(&core, "query").await?;
        assert!(sequence > 8 && sequence < 16, "committed prefix {sequence}");
        Ok((core, control, output))
    }

    #[tokio::test]
    async fn output_backpressure_and_cancelled_callers_retain_the_real_frontier() -> Result<()> {
        let (core, control, mut output) = blocked_output().await?;
        assert!(control.advance_to(3000, SHORT).await.is_err());
        assert_eq!(
            control.now(),
            2000,
            "cancelled caller cannot release graph-owned serialization"
        );
        for sequence in 9..=16 {
            receive(&mut output, sequence).await?;
        }
        let report = control.wake(WAIT).await?;
        assert_eq!(
            (report.physical_time_ms, report.output_sequence),
            (2000, 16)
        );
        core.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_preserves_output_cleanup_and_revokes_pending_control_requests() -> Result<()>
    {
        let (core, control, output) = blocked_output().await?;
        let waiting = control.wake(WAIT);
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        let shutdown = core.shutdown();
        tokio::pin!(shutdown);
        assert!(futures::poll!(&mut shutdown).is_pending());
        // The production outlet owns graceful subscriber cleanup. A test clock
        // must not bypass it; release the deliberately blocked subscriber.
        drop(output);
        tokio::time::timeout(WAIT, &mut shutdown).await??;
        let error = waiting.await.unwrap_err();
        assert!(!error.to_string().contains("timed out"), "{error}");
        Ok(())
    }

    const FAIL_QUEUE: u8 = 1;
    const FAIL_COMMIT: u8 = 2;
    const PAUSE_BEFORE_COMMIT: u8 = 3;
    const PAUSE_AFTER_COMMIT: u8 = 4;

    #[derive(Default)]
    struct Fault {
        mode: AtomicU8,
        entered: Notify,
        release: Notify,
    }
    struct Storage {
        inner: RocksDbIndexProvider,
        fault: Arc<Fault>,
    }

    impl Storage {
        fn new(path: &std::path::Path, fault: Arc<Fault>) -> Result<Arc<Self>> {
            Ok(Arc::new(Self {
                inner: RocksDbIndexProvider::new(path, false, false)
                    .with_memory_budget_bytes(32 << 20)?,
                fault,
            }))
        }
        fn wrap(&self, mut indexes: CreatedIndexes) -> CreatedIndexes {
            indexes.set.session_control = Arc::new(Session {
                inner: indexes.set.session_control,
                fault: self.fault.clone(),
            });
            indexes.set.future_queue = Arc::new(Queue {
                inner: indexes.set.future_queue,
                fault: self.fault.clone(),
            });
            indexes
        }
    }

    #[async_trait]
    impl IndexBackendPlugin for Storage {
        async fn create_indexes(
            &self,
            id: &str,
        ) -> std::result::Result<CreatedIndexes, IndexError> {
            Ok(self.wrap(self.inner.create_indexes(id).await?))
        }
        async fn create_scoped_indexes(
            &self,
            scope: &str,
            id: &str,
        ) -> std::result::Result<CreatedIndexes, IndexError> {
            Ok(self.wrap(self.inner.create_scoped_indexes(scope, id).await?))
        }
        fn is_volatile(&self) -> bool {
            false
        }
        fn supports_atomic_query_output(&self) -> bool {
            self.inner.supports_atomic_query_output()
        }
    }

    struct Session {
        inner: Arc<dyn SessionControl>,
        fault: Arc<Fault>,
    }
    #[async_trait]
    impl SessionControl for Session {
        async fn begin(&self) -> std::result::Result<(), IndexError> {
            self.inner.begin().await
        }
        async fn commit(&self) -> std::result::Result<(), IndexError> {
            let fault = self.fault.mode.swap(0, Ordering::AcqRel);
            if fault == FAIL_COMMIT {
                return Err(IndexError::other(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "injected output commit failure",
                )));
            }
            if fault == PAUSE_BEFORE_COMMIT {
                self.fault.entered.notify_one();
                self.fault.release.notified().await;
            }
            self.inner.commit().await?;
            if fault == PAUSE_AFTER_COMMIT {
                self.fault.entered.notify_one();
                self.fault.release.notified().await;
            }
            Ok(())
        }
        fn rollback(&self) -> std::result::Result<(), IndexError> {
            self.inner.rollback()
        }
    }

    struct Queue {
        inner: Arc<dyn FutureQueue>,
        fault: Arc<Fault>,
    }
    #[async_trait]
    impl FutureQueue for Queue {
        async fn push(
            &self,
            kind: PushType,
            position: usize,
            group: u64,
            element: &ElementReference,
            original: u64,
            due: u64,
        ) -> std::result::Result<bool, IndexError> {
            self.inner
                .push(kind, position, group, element, original, due)
                .await
        }
        async fn remove(&self, position: usize, group: u64) -> std::result::Result<(), IndexError> {
            self.inner.remove(position, group).await
        }
        async fn pop(&self) -> std::result::Result<Option<FutureElementRef>, IndexError> {
            if self
                .fault
                .mode
                .compare_exchange(FAIL_QUEUE, 0, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Err(IndexError::other(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    "injected scheduling read failure",
                )));
            }
            self.inner.pop().await
        }
        async fn peek_due_time(&self) -> std::result::Result<Option<u64>, IndexError> {
            self.inner.peek_due_time().await
        }
        async fn clear(&self) -> std::result::Result<(), IndexError> {
            self.inner.clear().await
        }
    }

    #[tokio::test]
    async fn scheduling_and_commit_failures_surface_without_false_completion() -> Result<()> {
        for (mode, message) in [
            (FAIL_QUEUE, "injected scheduling read failure"),
            (FAIL_COMMIT, "injected output commit failure"),
        ] {
            let directory = tempfile::tempdir()?;
            let fault = Arc::new(Fault::default());
            let storage = Storage::new(directory.path(), fault.clone())?;
            assert!(storage.supports_atomic_query_output());
            let source = Input::new();
            let control = QueryTestControl::new(1000);
            let (core, mut output) = open(&source, &control, EXPIRY, Some(storage), false).await?;
            source.send("a", 1000, 2000, 0).await?;
            receive(&mut output, 1).await?;
            control.wake(WAIT).await?;
            fault.mode.store(mode, Ordering::Release);
            let error = control.advance_to(2000, WAIT).await.unwrap_err();
            assert!(format!("{error:#}").contains(message), "{error:#}");
            assert!(!error.to_string().contains("timed out"));
            let repeated = control.wake(WAIT).await.unwrap_err();
            assert!(format!("{repeated:#}").contains(message), "{repeated:#}");
            core.stop_query("query").await?;
            core.start_query("query").await?;
            subscribe(&core, "query").await?;
            assert_eq!(control.wake(WAIT).await?.output_sequence, 2);
            core.shutdown().await?;
        }
        Ok(())
    }

    #[tokio::test]
    async fn query_failure_does_not_revoke_an_independent_control_in_the_shared_graph() -> Result<()>
    {
        let directory = tempfile::tempdir()?;
        let fault = Arc::new(Fault::default());
        let source = Input::new();
        let failed = QueryTestControl::new(1000);
        let healthy = QueryTestControl::new(1000);
        let core = DrasiLib::builder()
            .with_source(source.clone())
            .with_index_provider("store", Storage::new(directory.path(), fault.clone())?)
            .with_query(
                query("failed", EXPIRY)
                    .with_storage_backend(StorageBackendRef::Named("store".into()))
                    .build(),
            )
            .with_query(query("healthy", EXPIRY).build())
            .with_query_test_control("failed", failed.clone())
            .with_query_test_control("healthy", healthy.clone())
            .build()
            .await?;
        core.start().await?;
        let mut failed_output = subscribe(&core, "failed").await?;
        let mut healthy_output = subscribe(&core, "healthy").await?;
        source.send("a", 1000, 2000, 0).await?;
        receive(&mut failed_output, 1).await?;
        receive(&mut healthy_output, 1).await?;
        failed.wake(WAIT).await?;
        fault.mode.store(FAIL_QUEUE, Ordering::Release);
        let error = failed.advance_to(2000, WAIT).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("injected scheduling read failure"),
            "{error:#}"
        );
        assert_eq!(healthy.advance_to(1999, WAIT).await?.output_sequence, 1);
        assert_eq!(healthy.advance_to(2000, WAIT).await?.output_sequence, 2);
        receive(&mut healthy_output, 2).await?;
        let (sequence, rows) = rows(&core, "healthy").await?;
        assert_eq!(sequence, 2);
        assert_eq!(rows[0]["expired"], true);
        assert_eq!(rows[0]["logicalTime"], 2000);
        assert!(failed.wake(WAIT).await.is_err());
        core.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn frontier_cannot_complete_before_actual_query_commit() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let fault = Arc::new(Fault::default());
        let source = Input::new();
        let control = QueryTestControl::new(1000);
        let (core, mut output) = open(
            &source,
            &control,
            EXPIRY,
            Some(Storage::new(directory.path(), fault.clone())?),
            false,
        )
        .await?;
        source.send("a", 1000, 2000, 0).await?;
        receive(&mut output, 1).await?;
        control.wake(WAIT).await?;
        fault.mode.store(PAUSE_BEFORE_COMMIT, Ordering::Release);
        let drain = control.advance_to(2000, WAIT);
        tokio::pin!(drain);
        tokio::select! {
            result = &mut drain => panic!("premature drain: {result:?}"),
            result = tokio::time::timeout(WAIT, fault.entered.notified()) => { result?; },
        }
        assert_eq!(rows(&core, "query").await?.0, 1);
        assert!(futures::poll!(&mut drain).is_pending());
        fault.release.notify_one();
        let report = drain.await?;
        assert_eq!(report.output_sequence, 2);
        assert!(report.output_persistent);
        assert_eq!(report.publication, QueryPublicationMode::Atomic);
        core.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn retained_restart_preserves_identity_and_replays_without_recomputation() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let fault = Arc::new(Fault::default());
        let source = Input::new();
        let control = QueryTestControl::new(1000);
        let (core, mut output) = open(
            &source,
            &control,
            EXPIRY,
            Some(Storage::new(directory.path(), fault.clone())?),
            false,
        )
        .await?;
        source.send("a", 1000, 2000, 0).await?;
        receive(&mut output, 1).await?;
        let before = control.advance_to(1999, WAIT).await?;
        core.shutdown().await?;
        drop(output);
        drop(core);

        let control = QueryTestControl::new(1999);
        let (core, output) = open(
            &source,
            &control,
            EXPIRY,
            Some(Storage::new(directory.path(), fault.clone())?),
            false,
        )
        .await?;
        assert_eq!(control.wake(WAIT).await?, before);
        fault.mode.store(PAUSE_AFTER_COMMIT, Ordering::Release);
        {
            let drain = control.advance_to(2000, WAIT);
            tokio::pin!(drain);
            tokio::select! {
                result = &mut drain => panic!("premature drain: {result:?}"),
                result = tokio::time::timeout(WAIT, fault.entered.notified()) => { result?; },
            }
            assert_eq!(
                rows(&core, "query").await?.0,
                1,
                "committed output not yet published"
            );
        }
        {
            let shutdown = core.shutdown();
            tokio::pin!(shutdown);
            tokio::select! {
                result = &mut shutdown => panic!("storage cleanup must await blocked commit: {result:?}"),
                result = control.wake(WAIT) => {
                    let error = result.expect_err("stop must revoke endpoint before storage release");
                    assert!(!error.to_string().contains("timed out"), "{error}");
                },
            }
            fault.release.notify_one();
            tokio::time::timeout(WAIT, &mut shutdown).await??;
        }
        drop(output);
        drop(core);

        let control = QueryTestControl::new(2000);
        let (core, _output) = open(
            &source,
            &control,
            EXPIRY,
            Some(Storage::new(directory.path(), fault)?),
            false,
        )
        .await?;
        let recovered = control.wake(WAIT).await?;
        assert_eq!(recovered.output_sequence, 2);
        assert_eq!(recovered.output_generation, before.output_generation);
        assert_expired(&core, "a", true).await?;
        assert_eq!(
            control.wake(WAIT).await?,
            recovered,
            "no recomputation or duplicate output"
        );
        core.shutdown().await?;
        Ok(())
    }

    #[tokio::test]
    async fn invalid_bindings_and_initial_times_are_rejected() -> Result<()> {
        let control = QueryTestControl::new(1000);
        assert!(control.wake(WAIT).await.is_err());
        assert!(DrasiLib::builder()
            .with_query_test_control("missing", control.clone())
            .build()
            .await
            .is_err());
        assert!(DrasiLib::builder()
            .with_query(query("query", EXPIRY).build())
            .with_query_test_control("query", control.clone())
            .with_query_test_control("query", control.clone())
            .build()
            .await
            .is_err());
        assert!(DrasiLib::builder()
            .with_query(query("query", EXPIRY).build())
            .with_query_test_control("query", QueryTestControl::new(u64::MAX))
            .build()
            .await
            .is_err());
        let source = Input::new();
        let (core, _output) = open(&source, &control, EXPIRY, None, false).await?;
        assert!(open(&Input::new(), &control, EXPIRY, None, false)
            .await
            .is_err());
        core.shutdown().await?;
        Ok(())
    }
}
