// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Public consumer: one DrasiLib with an assembled native ComponentBatch.
//! Reuses only test fault injection; all runtime/control calls are public APIs.

use super::{
    Fault, Queue, Session, EXPIRY, FAIL_COMMIT, FAIL_QUEUE, PAUSE_AFTER_COMMIT,
    PAUSE_BEFORE_COMMIT, SHORT, WAIT,
};
use std::{
    num::NonZeroUsize,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
};

use anyhow::Result;
use async_trait::async_trait;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource,
        InMemoryComputationProvider, TransactionDomain,
    },
    evaluation::variable_value::VariableValue,
    interface::{IndexError, IndexSet, SessionControl},
    models::{Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange},
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::{computation::v1::*, test_support::QueryTestControl, DrasiLib};
use tokio::sync::{broadcast, mpsc, Notify};

const GRAPH: &str = "native-timing-host";
const FAIL_STOP: u8 = 5;

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("fixture component")
}
fn stream(value: &str) -> StreamId {
    StreamId::try_new(value).expect("fixture stream")
}
fn port(value: &str) -> PortId {
    PortId::try_new(value).expect("fixture port")
}
fn edge(from: &str, to: &str) -> EdgeDefinition {
    EdgeDefinition::new(
        Endpoint::new(id(from), port("out")),
        Endpoint::new(id(to), port("in")),
    )
}

fn definition() -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: GRAPH.into(),
        id: id("query"),
        query: EXPIRY.into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: stream("query/out"),
        outbox_capacity: NonZeroUsize::new(64).expect("capacity"),
    }
}

struct NativeSource {
    descriptor: ComponentDescriptor,
    receiver: mpsc::Receiver<OutputEnvelope>,
}
#[async_trait]
impl ComputationComponent for NativeSource {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl EnvelopeSource for NativeSource {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        Ok(self.receiver.recv().await)
    }
}

struct FailingSibling {
    descriptor: ComponentDescriptor,
    fail: Arc<Notify>,
}
#[async_trait]
impl ComputationComponent for FailingSibling {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait]
impl ComputationService for FailingSibling {
    async fn run(&mut self) -> Result<()> {
        self.fail.notified().await;
        anyhow::bail!("unrelated sibling failed")
    }
}

#[derive(Default)]
struct Gate {
    paused: AtomicBool,
    fail: AtomicBool,
    entered: Notify,
    released: Notify,
}
impl Gate {
    fn release(&self) {
        self.paused.store(false, Ordering::Release);
        self.released.notify_one();
    }
}
struct GatedOutlet {
    inner: QueryResultsOutlet,
    gate: Arc<Gate>,
}
#[async_trait]
impl ComputationComponent for GatedOutlet {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.inner.descriptor()
    }
    async fn start(&mut self) -> Result<()> {
        self.inner.start().await
    }
    async fn stop(&mut self) -> Result<()> {
        self.inner.stop().await
    }
}
#[async_trait]
impl EnvelopeSink for GatedOutlet {
    fn completion(&self) -> SinkCompletion {
        self.inner.completion()
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        if self.gate.paused.load(Ordering::Acquire) {
            self.gate.entered.notify_one();
            self.gate.released.notified().await;
        }
        anyhow::ensure!(
            !self.gate.fail.load(Ordering::Acquire),
            "injected native outlet failure"
        );
        self.inner.handle(input).await
    }
}

struct NativeStorage {
    inner: RocksDbComputationProvider,
    fault: Arc<Fault>,
}
#[async_trait]
impl ComputationIndexProvider for NativeStorage {
    async fn create_indexes(
        &self,
        graph: &str,
        query: &str,
    ) -> std::result::Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, query).await?;
        let set = original.indexes();
        let control: Arc<dyn SessionControl> = Arc::new(Session {
            inner: set.session_control.clone(),
            fault: self.fault.clone(),
        });
        let domain = TransactionDomain::new(control.clone());
        let indexes = ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: Arc::new(Queue {
                    inner: set.future_queue.clone(),
                    fault: self.fault.clone(),
                }),
                session_control: control,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                original
                    .checkpoint_store()
                    .expect("persistent checkpoints")
                    .clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original.outbox_writer().expect("persistent outbox").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original
                    .live_results_writer()
                    .expect("persistent rows")
                    .clone(),
                &domain,
            )),
        )
        .map_err(IndexError::other)?;
        Ok(indexes.with_cleanup(original.cleanup().expect("native storage cleanup").clone()))
    }
    fn is_volatile(&self) -> bool {
        false
    }
}

#[derive(Clone, Copy, Debug)]
enum Body {
    Direct,
    Transaction,
    TransactionWithStopFault,
}

struct StopFault(Arc<Fault>);
#[async_trait]
impl ComputationBootstrapProvider for StopFault {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures::stream::empty()),
            watermarks: Vec::new(),
        })
    }
    async fn stop(&self) -> Result<()> {
        anyhow::ensure!(
            self.0.mode.load(Ordering::Acquire) != FAIL_STOP,
            "injected native query cleanup failure"
        );
        Ok(())
    }
}

struct Host {
    lib: DrasiLib,
    clock: QueryTestControl,
    query: ComponentHandle,
    results: QueryResults,
    applied: Arc<QuerySourceProgress>,
    published: broadcast::Receiver<ChangeEnvelope>,
    input: mpsc::Sender<OutputEnvelope>,
    gate: Arc<Gate>,
    fault: Arc<Fault>,
}

impl Host {
    async fn open(path: &std::path::Path, now: u64, body: Body) -> Result<Self> {
        Self::open_with_sibling(path, now, body, None).await
    }

    async fn open_with_sibling(
        path: &std::path::Path,
        now: u64,
        body: Body,
        sibling: Option<Arc<Notify>>,
    ) -> Result<Self> {
        let fault = Arc::new(Fault::default());
        let provider = Arc::new(NativeStorage {
            inner: RocksDbComputationProvider::new(
                path,
                RocksIndexOptions::new(
                    false,
                    false,
                    RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
                ),
            ),
            fault: fault.clone(),
        });
        let catalog = QueryResultsCatalog::new(GRAPH)?;
        // This observes the real outlet, not the query's earlier commit.
        let published = catalog.subscribe();
        let clock = QueryTestControl::new(now);
        let applied = Arc::new(QuerySourceProgress::new(GRAPH, id("query"))?);
        let mut query = ContinuousQueryTransformer::new(definition(), provider)
            .await?
            .with_source_progress(applied.clone())?
            .with_result_catalog(&catalog)?;
        if matches!(body, Body::TransactionWithStopFault) {
            query = query.with_bootstrap(Arc::new(StopFault(fault.clone())));
        }
        let results = query.results();
        let query: Box<dyn Transformer> = match body {
            Body::Direct => Box::new(query.with_test_control(clock.clone())?),
            Body::Transaction | Body::TransactionWithStopFault => Box::new(
                TransactionTransformer::from_query(query).with_test_control(clock.clone())?,
            ),
        };
        let (input, receiver) = mpsc::channel(8);
        let source = NativeSource {
            descriptor: ComponentDescriptor::try_new(
                id("source"),
                vec![PortDescriptor::new(
                    port("out"),
                    PortDirection::Output,
                    GraphChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            receiver,
        };
        let gate = Arc::new(Gate::default());
        let mut batch = ComponentBatch::builder()
            .source(Box::new(source))
            .query(query)
            .sink(Box::new(GatedOutlet {
                inner: QueryResultsOutlet::new(id("outlet"), catalog),
                gate: gate.clone(),
            }))
            .bind_stream(
                Endpoint::new(id("source"), port("out")),
                stream("source/out"),
            )
            .bind_stream(Endpoint::new(id("query"), port("out")), stream("query/out"))
            .connect(
                edge("source", "query"),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                edge("query", "outlet"),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
        if let Some(fail) = sibling {
            batch = batch.service(Box::new(FailingSibling {
                descriptor: ComponentDescriptor::try_new(id("sibling"), vec![])?,
                fail,
            }));
        }
        let lib = DrasiLib::builder()
            .with_id(GRAPH)
            .with_components(batch.build()?)
            .build()
            .await?;
        lib.start().await?;
        let graph = lib.computation_control()?;
        for component in ["source", "query", "outlet"] {
            tokio::time::timeout(WAIT, graph.component_handle(&id(component))?.wait_started())
                .await??;
        }
        tokio::time::timeout(WAIT, results.wait_ready()).await??;
        let query = graph.component_handle(&id("query"))?;
        Ok(Self {
            lib,
            clock,
            query,
            results,
            applied,
            published,
            input,
            gate,
            fault,
        })
    }

    async fn insert(&self, sequence: u64, deadline: u64) -> Result<()> {
        self.input
            .send(OutputEnvelope {
                port: port("out"),
                envelope: GraphChangeCodec::encode_change(
                    SourceChange::Insert {
                        element: Element::Node {
                            metadata: ElementMetadata {
                                reference: ElementReference::new("input", &sequence.to_string()),
                                labels: Arc::from([Arc::from("Item")]),
                                effective_from: 1000,
                            },
                            properties: ElementPropertyMap::from(serde_json::json!({
                                "id": sequence.to_string(), "revision": 0, "deadline": deadline,
                            })),
                        },
                    },
                    stream("source/out"),
                    sequence,
                    None,
                )?,
            })
            .await?;
        // Source enqueue is NOT the input fence. Wait on this actual query's
        // committed checkpoint publication before changing eligibility time.
        let mut progress = self.applied.subscribe();
        tokio::time::timeout(WAIT, async {
            loop {
                let snapshot = progress.borrow_and_update().clone();
                anyhow::ensure!(snapshot.failure.is_none(), "query input processing failed");
                if snapshot
                    .checkpoints
                    .get(&SourceProgressKey::Stream(stream("source/out")))
                    .is_some_and(|checkpoint| checkpoint.sequence >= sequence)
                {
                    return Ok::<_, anyhow::Error>(());
                }
                progress.changed().await?;
            }
        })
        .await??;
        Ok(())
    }

    async fn observe(&mut self, sequence: u64) -> Result<()> {
        tokio::time::timeout(WAIT, async {
            loop {
                let event = self.published.recv().await?;
                if QueryChangeCodec::query_sequence(&event)? == sequence {
                    assert_eq!(
                        QueryChangeCodec::query_generation(&event)?,
                        self.results.snapshot()?.generation
                    );
                    return Ok::<_, anyhow::Error>(());
                }
            }
        })
        .await??;
        Ok(())
    }

    fn expired(&self, item: &str) -> Result<bool> {
        for row in self.results.snapshot()?.rows.values() {
            let row = QueryChangeCodec::decode_row(row)?;
            if row.values.get("id") == Some(&VariableValue::String(item.into())) {
                return match row.values.get("expired") {
                    Some(VariableValue::Bool(value)) => Ok(*value),
                    Some(VariableValue::Awaiting) => Ok(false),
                    value => anyhow::bail!("unexpected expiration value: {value:?}"),
                };
            }
        }
        anyhow::bail!("missing fixture row {item}")
    }
}

async fn deadlines() -> Result<()> {
    for body in [Body::Direct, Body::Transaction] {
        let directory = tempfile::tempdir()?;
        let mut host = Host::open(directory.path(), 1000, body).await?;
        host.insert(1, 2000).await?;
        host.observe(1).await?;
        host.insert(2, 3000).await?;
        host.observe(2).await?;
        let initial = host.clock.advance_to(1999, WAIT).await?;
        assert_eq!(initial.output_sequence, 2);
        host.gate.paused.store(true, Ordering::Release);
        let report = host.clock.advance_to(2000, WAIT).await?;
        assert_eq!(report.physical_time_ms, 2000);
        assert_eq!(report.output_sequence, 3);
        assert_eq!(report.output_generation, initial.output_generation);
        assert!(report.output_persistent);
        assert_eq!(report.publication, QueryPublicationMode::Atomic);
        assert!(host.expired("1")?);
        assert!(!host.expired("2")?);
        tokio::time::timeout(WAIT, host.gate.entered.notified()).await?;
        assert!(
            matches!(
                host.published.try_recv(),
                Err(broadcast::error::TryRecvError::Empty)
            ),
            "pipe frontier must not be mistaken for outlet observation"
        );
        host.gate.release();
        host.observe(3).await?;
        assert_eq!(host.clock.advance_to(2999, WAIT).await?.output_sequence, 3);
        assert_eq!(host.clock.advance_to(3000, WAIT).await?.output_sequence, 4);
        host.observe(4).await?;
        assert!(host.expired("2")?);
        host.lib.shutdown().await?;
        assert!(host.clock.wake(WAIT).await.is_err());
    }
    Ok(())
}

#[tokio::test]
async fn assembled_native_graph_has_separate_commit_and_outlet_frontiers() -> Result<()> {
    deadlines().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assembled_native_graph_supports_multithread_runtime() -> Result<()> {
    deadlines().await
}

async fn blocked(body: Body) -> Result<(tempfile::TempDir, Host)> {
    let directory = tempfile::tempdir()?;
    let mut host = Host::open(directory.path(), 1000, body).await?;
    for sequence in 1..=6 {
        host.insert(sequence, 2000).await?;
        host.observe(sequence).await?;
    }
    host.clock.wake(WAIT).await?;
    host.gate.paused.store(true, Ordering::Release);
    let error = host
        .clock
        .advance_to(2000, SHORT)
        .await
        .expect_err("native pipe is blocked");
    assert!(error.to_string().contains("timed out"), "{error}");
    let head = host.results.snapshot()?.as_of_sequence;
    assert!(
        head > 6 && head < 12,
        "{body:?}: committed but blocked head {head}"
    );
    assert!(host.clock.advance_to(3000, SHORT).await.is_err());
    assert_eq!(host.clock.now(), 2000);
    Ok((directory, host))
}

#[tokio::test]
async fn native_blocked_output_stop_and_shutdown_revoke_pending_frontiers() -> Result<()> {
    for body in [Body::Direct, Body::Transaction] {
        let (_directory, host) = blocked(body).await?;
        let wait = host.clock.wake(WAIT);
        tokio::pin!(wait);
        assert!(futures::poll!(&mut wait).is_pending());
        tokio::time::timeout(WAIT, host.query.stop()).await??;
        let error = wait
            .await
            .expect_err("stop cannot report a completed drain");
        assert!(!error.to_string().contains("timed out"), "{error}");
        tokio::time::timeout(WAIT, host.lib.shutdown()).await??;
    }
    let (_directory, host) = blocked(Body::Transaction).await?;
    let wait = host.clock.wake(WAIT);
    tokio::pin!(wait);
    assert!(futures::poll!(&mut wait).is_pending());
    tokio::time::timeout(WAIT, host.lib.shutdown()).await??;
    assert!(!wait
        .await
        .expect_err("shutdown revokes requests")
        .to_string()
        .contains("timed out"));
    Ok(())
}

#[tokio::test]
async fn native_failed_cleanup_is_reported_and_retry_remains_graph_owned() -> Result<()> {
    for shutdown in [false, true] {
        let (_directory, host) = blocked(Body::TransactionWithStopFault).await?;
        host.fault.mode.store(FAIL_STOP, Ordering::Release);
        let waiting = host.clock.wake(WAIT);
        tokio::pin!(waiting);
        assert!(futures::poll!(&mut waiting).is_pending());
        let error = if shutdown {
            tokio::time::timeout(WAIT, host.lib.shutdown())
                .await?
                .expect_err("shutdown must report failed query cleanup")
                .to_string()
        } else {
            tokio::time::timeout(WAIT, host.query.stop())
                .await?
                .expect_err("component stop must report failed query cleanup")
                .to_string()
        };
        assert!(
            error.contains("injected native query cleanup failure"),
            "{error}"
        );
        let error = waiting
            .await
            .expect_err("failed cleanup cannot complete a drain");
        assert!(!error.to_string().contains("timed out"), "{error}");
        host.fault.mode.store(0, Ordering::Release);
        let retry = tokio::time::timeout(WAIT, host.lib.shutdown()).await?;
        if shutdown {
            // A failed driver run is reported even after disposal succeeds;
            // the next shutdown observes the already disposed owner.
            let original = retry.expect_err("disposal must not erase the driver's failure");
            assert!(
                original
                    .to_string()
                    .contains("injected native query cleanup failure"),
                "{original}"
            );
        } else {
            retry?;
        }
        tokio::time::timeout(WAIT, host.lib.shutdown()).await??;
    }
    Ok(())
}

#[tokio::test]
async fn native_transaction_body_replays_unconfirmed_output_on_retained_start() -> Result<()> {
    let (_directory, mut host) = blocked(Body::Transaction).await?;
    let committed = host.results.snapshot()?.as_of_sequence;
    host.query.stop().await?;
    host.gate.release();
    host.query.start().await?;
    let report = host.clock.wake(WAIT).await?;
    assert!(report.output_sequence >= committed);
    assert_eq!(report.output_sequence, 12);
    host.observe(12).await?;
    assert_eq!(host.clock.wake(WAIT).await?, report);
    host.lib.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_persistent_restart_recovers_deadline_and_interrupted_output_commit() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut host = Host::open(directory.path(), 1000, Body::Transaction).await?;
    host.insert(1, 2000).await?;
    host.observe(1).await?;
    let before = host.clock.advance_to(1999, WAIT).await?;
    host.lib.shutdown().await?;
    drop(host);
    let host = Host::open(directory.path(), 1999, Body::Transaction).await?;
    assert_eq!(host.clock.wake(WAIT).await?, before);
    host.fault.mode.store(PAUSE_AFTER_COMMIT, Ordering::Release);
    {
        let drain = host.clock.advance_to(2000, WAIT);
        tokio::pin!(drain);
        tokio::select! {
            result = &mut drain => panic!("commit did not finish publication: {result:?}"),
            result = tokio::time::timeout(WAIT, host.fault.entered.notified()) => { result?; },
        }
        assert_eq!(host.results.snapshot()?.as_of_sequence, 1);
        tokio::time::timeout(WAIT, host.lib.shutdown()).await??;
        assert!(
            drain.await.is_err(),
            "cancelled publication cannot become a successful frontier"
        );
    }
    drop(host);
    let mut host = Host::open(directory.path(), 2000, Body::Transaction).await?;
    let recovered = host.clock.wake(WAIT).await?;
    assert_eq!(recovered.output_sequence, 2);
    assert_eq!(recovered.output_generation, before.output_generation);
    host.observe(2).await?;
    assert!(host.expired("1")?);
    assert_eq!(host.clock.wake(WAIT).await?, recovered);
    host.lib.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_direct_query_reopens_persisted_deadline_with_a_fresh_control() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut host = Host::open(directory.path(), 1000, Body::Direct).await?;
    host.insert(1, 2000).await?;
    host.observe(1).await?;
    let before = host.clock.advance_to(1999, WAIT).await?;
    host.lib.shutdown().await?;
    drop(host);

    let mut host = Host::open(directory.path(), 1999, Body::Direct).await?;
    assert_eq!(host.clock.wake(WAIT).await?, before);
    let report = host.clock.advance_to(2000, WAIT).await?;
    assert_eq!(report.output_sequence, 2);
    assert_eq!(report.output_generation, before.output_generation);
    host.observe(2).await?;
    assert!(host.expired("1")?);
    host.lib.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_scheduling_and_commit_errors_are_not_timeouts_or_success() -> Result<()> {
    for body in [Body::Direct, Body::Transaction] {
        for (mode, message) in [
            (FAIL_QUEUE, "injected scheduling read failure"),
            (FAIL_COMMIT, "injected output commit failure"),
        ] {
            let directory = tempfile::tempdir()?;
            let mut host = Host::open(directory.path(), 1000, body).await?;
            host.insert(1, 2000).await?;
            host.observe(1).await?;
            host.clock.wake(WAIT).await?;
            host.fault.mode.store(mode, Ordering::Release);
            let error = host
                .clock
                .advance_to(2000, WAIT)
                .await
                .expect_err("native failure must fail frontier");
            assert!(
                format!("{error:#}").contains(message),
                "{body:?}: {error:#}"
            );
            let repeated = host
                .clock
                .wake(WAIT)
                .await
                .expect_err("failure remains visible");
            assert!(format!("{repeated:#}").contains(message), "{repeated:#}");
            host.fault.mode.store(0, Ordering::Release);
            host.query.stop().await?;
            host.query.start().await?;
            let recovered = host.clock.wake(WAIT).await?;
            assert_eq!(recovered.output_sequence, 2);
            host.observe(2).await?;
            assert_eq!(host.clock.wake(WAIT).await?, recovered);
            host.lib.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_peer_failure_reaches_a_blocked_query_frontier() -> Result<()> {
    let (_directory, host) = blocked(Body::Transaction).await?;
    host.gate.fail.store(true, Ordering::Release);
    host.gate.release();
    let error = host
        .clock
        .wake(WAIT)
        .await
        .expect_err("outlet failure must fail pending drain");
    assert!(
        format!("{error:#}").contains("injected native outlet failure"),
        "{error:#}"
    );
    host.lib.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_query_commit_pause_and_caller_drop_preserve_graph_owned_work() -> Result<()> {
    for body in [Body::Direct, Body::Transaction] {
        let directory = tempfile::tempdir()?;
        let mut host = Host::open(directory.path(), 1000, body).await?;
        host.insert(1, 2000).await?;
        host.observe(1).await?;
        host.clock.wake(WAIT).await?;
        host.fault
            .mode
            .store(PAUSE_BEFORE_COMMIT, Ordering::Release);
        {
            let drain = host.clock.advance_to(2000, WAIT);
            tokio::pin!(drain);
            tokio::select! {
                result = &mut drain => panic!("paused commit completed frontier: {result:?}"),
                result = tokio::time::timeout(WAIT, host.fault.entered.notified()) => { result?; },
            }
            assert_eq!(host.results.snapshot()?.as_of_sequence, 1);
            assert!(host.clock.advance_to(3000, SHORT).await.is_err());
            assert_eq!(host.clock.now(), 2000);
        }
        host.fault.mode.store(0, Ordering::Release);
        host.fault.release.notify_one();
        let report = host.clock.wake(WAIT).await?;
        assert_eq!(report.physical_time_ms, 2000);
        assert_eq!(report.output_sequence, 2);
        host.observe(2).await?;
        host.lib.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn native_control_does_not_observe_unrelated_sibling_failure() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let fail = Arc::new(Notify::new());
    let mut host =
        Host::open_with_sibling(directory.path(), 1000, Body::Direct, Some(fail.clone())).await?;
    host.insert(1, 2000).await?;
    host.observe(1).await?;
    let graph = host.lib.computation_control()?;
    let sibling = graph.component_handle(&id("sibling"))?;
    sibling.wait_started().await?;
    let mut changes = graph.subscribe_observed();
    fail.notify_one();
    tokio::time::timeout(WAIT, async {
        loop {
            if changes
                .borrow_and_update()
                .components
                .get(&id("sibling"))
                .is_some_and(|node| node.failure.is_some())
            {
                break;
            }
            changes.changed().await.expect("graph publication");
        }
    })
    .await?;
    assert_eq!(host.clock.advance_to(2000, WAIT).await?.output_sequence, 2);
    host.observe(2).await?;
    host.lib.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_attachment_requires_unique_control_and_graph_ownership() -> Result<()> {
    async fn query() -> Result<ContinuousQueryTransformer> {
        ContinuousQueryTransformer::new(definition(), Arc::new(InMemoryComputationProvider)).await
    }
    let clock = QueryTestControl::new(1000);
    let attached = query().await?.with_test_control(clock.clone())?;
    assert!(query().await?.with_test_control(clock.clone()).is_err());
    assert!(attached
        .with_test_control(QueryTestControl::new(1000))
        .is_err());
    assert!(query()
        .await?
        .with_scheduling(Arc::new(QuerySchedulingResource::default()))?
        .with_test_control(QueryTestControl::new(1000))
        .is_err());
    assert!(query()
        .await?
        .with_test_control(QueryTestControl::new(1000))?
        .with_scheduling(Arc::new(QuerySchedulingResource::default()))
        .is_err());
    let mut unowned = query()
        .await?
        .with_test_control(QueryTestControl::new(1000))?;
    let error = unowned
        .start()
        .await
        .expect_err("query requires an owning graph");
    assert!(
        error.to_string().contains("graph lifecycle ownership"),
        "{error}"
    );
    let mut activated = query().await?;
    activated.start().await?;
    activated.stop().await?;
    assert!(activated
        .with_test_control(QueryTestControl::new(1000))
        .is_err());
    Ok(())
}

#[tokio::test]
async fn native_linear_transaction_rejects_query_test_control_without_binding_it() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let provider = Arc::new(RocksDbComputationProvider::new(
        directory.path(),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    ));
    let linear = TransactionTransformer::new(
        TransactionTransformerDefinition {
            graph_id: GRAPH.into(),
            id: id("linear"),
            output_stream: stream("linear/out"),
            steps: vec![TransactionStepDefinition {
                id: id("step"),
                implementation: ImplementationIdentity::try_new(
                    "drasi/middleware-transformer",
                    "1",
                )?,
                configuration_version: 1,
                configuration: serde_json::json!({"middleware": [], "pipeline": []}),
            }],
            outbox_capacity: NonZeroUsize::new(64).expect("capacity"),
        },
        Arc::new(TransactionalTransformerRegistry::standard(Arc::new(
            drasi_core::middleware::MiddlewareTypeRegistry::new(),
        ))),
        provider,
    )
    .await?;
    let clock = QueryTestControl::new(1000);
    let error = linear
        .with_test_control(clock.clone())
        .err()
        .expect("linear body is unsupported");
    assert!(
        error.to_string().contains("linear transaction steps"),
        "{error}"
    );
    let _query =
        ContinuousQueryTransformer::new(definition(), Arc::new(InMemoryComputationProvider))
            .await?
            .with_test_control(clock)?;
    Ok(())
}
