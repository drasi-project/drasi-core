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

//! Real library ingestion, rollback, replay, and bootstrap against persistent providers.

use drasi_core::interface::SessionError;

use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex, Weak,
    },
    time::Duration,
};

use anyhow::{anyhow, ensure, Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use drasi_core::{
    index_cache::{
        cached_element_index::CachedElementIndex, cached_result_index::CachedResultIndex,
    },
    interface::{
        CheckpointStore, CreatedIndexes, ElementIndex, IndexBackendPlugin, IndexError,
        LiveResultsWriter, OutboxWriter, RowMutation, SessionControl, SessionGuard,
        SourceCheckpoint,
    },
    models::{Element, ElementMetadata, ElementReference, SourceChange},
};
use drasi_lib::{
    bootstrap::{BootstrapContext, BootstrapProvider, BootstrapRequest, BootstrapResult},
    channels::{
        dispatcher::ReplayThenLiveReceiver, BootstrapEvent, BootstrapEventSender, ChangeReceiver,
        ComponentStatus, DispatchMode, QueryResult, ResultDiff, SourceEvent, SourceEventWrapper,
        SubscriptionResponse,
    },
    config::SourceSubscriptionSettings,
    context::SourceRuntimeContext,
    queries::manager::Query as RunningQuery,
    sources::base::{SourceBase, SourceBaseParams},
    DrasiLib, Query, Source,
};
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::{Mutex as AsyncMutex, Notify, Semaphore};

const SOURCE: &str = "transaction-source";
const DEADLINE: Duration = Duration::from_secs(10);

type History = Arc<AsyncMutex<Vec<Arc<SourceEventWrapper>>>>;

struct RecordedSource {
    base: Arc<SourceBase>,
    history: History,
    replay_gate: Arc<Semaphore>,
}

struct SourceHandle {
    base: Arc<SourceBase>,
    history: History,
    replay_gate: Arc<Semaphore>,
}

impl RecordedSource {
    fn new(
        history: History,
        paused: bool,
        bootstrap: Option<SnapshotBootstrap>,
    ) -> Result<(Self, SourceHandle)> {
        let mut params = SourceBaseParams::new(SOURCE);
        if let Some(bootstrap) = bootstrap {
            params = params.with_bootstrap_provider(bootstrap);
        }
        let base = Arc::new(SourceBase::new(params)?);
        let replay_gate = Arc::new(Semaphore::new(usize::from(!paused)));
        let handle = SourceHandle {
            base: base.clone(),
            history: history.clone(),
            replay_gate: replay_gate.clone(),
        };
        Ok((
            Self {
                base,
                history,
                replay_gate,
            },
            handle,
        ))
    }
}

impl SourceHandle {
    async fn send(&self, change: SourceChange) -> Result<u64> {
        let mut history = self.history.lock().await;
        let sequence = history.len() as u64 + 1;
        let event = SourceEventWrapper {
            source_id: SOURCE.into(),
            event: SourceEvent::Change(change),
            timestamp: chrono::Utc::now(),
            sequence: Some(sequence),
            source_position: Some(position(sequence)),
            profiling: None,
        };
        history.push(Arc::new(event.clone()));
        self.base.dispatch_event(event).await?;
        Ok(sequence)
    }

    fn resume(&self) {
        self.replay_gate.add_permits(1);
    }
}

struct GatedReplay {
    inner: ReplayThenLiveReceiver<SourceEventWrapper>,
    gate: Arc<Semaphore>,
    started: bool,
}

#[async_trait]
impl ChangeReceiver<SourceEventWrapper> for GatedReplay {
    async fn recv(&mut self) -> Result<Arc<SourceEventWrapper>> {
        if !self.started {
            drop(self.gate.acquire().await?);
            self.started = true;
        }
        self.inner.recv().await
    }
}

#[async_trait]
impl Source for RecordedSource {
    fn id(&self) -> &str {
        SOURCE
    }
    fn type_name(&self) -> &str {
        "recorded-transaction-test"
    }
    fn properties(&self) -> HashMap<String, Value> {
        HashMap::new()
    }
    fn dispatch_mode(&self) -> DispatchMode {
        DispatchMode::Channel
    }
    fn auto_start(&self) -> bool {
        true
    }
    fn supports_replay(&self) -> bool {
        true
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn start(&self) -> Result<()> {
        self.base.set_status(ComponentStatus::Running, None).await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn initialize(&self, context: SourceRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn set_bootstrap_provider(&self, provider: Box<dyn BootstrapProvider>) {
        self.base.set_bootstrap_provider(provider).await;
    }

    async fn subscribe(
        &self,
        settings: SourceSubscriptionSettings,
    ) -> Result<SubscriptionResponse> {
        // The same lock covers recording/live dispatch and replay/live handoff.
        let history = self.history.lock().await;
        let resume = match settings.resume_from.as_ref() {
            Some(bytes) => u64::from_be_bytes(
                bytes
                    .as_ref()
                    .try_into()
                    .context("invalid recorded position")?,
            ),
            None => 0,
        };
        ensure!(
            resume <= history.len() as u64,
            "resume position exceeds recorded history"
        );
        let replay: VecDeque<_> = history.iter().skip(resume as usize).cloned().collect();
        let mut response = self
            .base
            .subscribe_with_bootstrap(&settings, self.type_name())
            .await?;
        response.receiver = Box::new(GatedReplay {
            inner: ReplayThenLiveReceiver::new(replay, response.receiver),
            gate: self.replay_gate.clone(),
            started: false,
        });
        Ok(response)
    }
}

#[derive(Clone)]
struct SnapshotBootstrap {
    changes: Vec<SourceChange>,
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl BootstrapProvider for SnapshotBootstrap {
    async fn bootstrap(
        &self,
        _request: BootstrapRequest,
        context: &BootstrapContext,
        events: BootstrapEventSender,
        _settings: Option<&SourceSubscriptionSettings>,
    ) -> Result<BootstrapResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        for change in &self.changes {
            events
                .send(BootstrapEvent {
                    source_id: SOURCE.into(),
                    change: change.clone(),
                    timestamp: chrono::Utc::now(),
                    sequence: context.next_sequence(),
                })
                .await?;
        }
        Ok(BootstrapResult {
            event_count: self.changes.len(),
            source_position: Some(position(0)),
        })
    }
}

struct WriteFault {
    armed: AtomicBool,
    staged: Notify,
    release: Semaphore,
}

impl Default for WriteFault {
    fn default() -> Self {
        Self {
            armed: AtomicBool::new(false),
            staged: Notify::new(),
            release: Semaphore::new(0),
        }
    }
}

struct FailingLiveWriter {
    inner: Arc<dyn LiveResultsWriter>,
    fault: Arc<WriteFault>,
}

#[async_trait]
impl LiveResultsWriter for FailingLiveWriter {
    async fn apply_mutations(
        &self,
        query_id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        self.inner.apply_mutations(query_id, mutations).await?;
        if self.fault.armed.swap(false, Ordering::SeqCst) {
            self.fault.staged.notify_one();
            self.fault
                .release
                .acquire()
                .await
                .map_err(IndexError::other)?
                .forget();
            return Err(IndexError::IOError);
        }
        Ok(())
    }

    async fn read_snapshot(&self, query_id: &str) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner.read_snapshot(query_id).await
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        self.inner.clear(query_id).await
    }

    async fn row_count(&self, query_id: &str) -> Result<usize, IndexError> {
        self.inner.row_count(query_id).await
    }
}

struct ObservedIndexes {
    control: Weak<dyn SessionControl>,
    elements: Weak<dyn ElementIndex>,
    checkpoint: Weak<dyn CheckpointStore>,
    outbox: Weak<dyn OutboxWriter>,
    live: Weak<dyn LiveResultsWriter>,
}

struct ObservedProvider {
    inner: Arc<dyn IndexBackendPlugin>,
    cached: bool,
    fault: Arc<WriteFault>,
    observed: Mutex<Option<ObservedIndexes>>,
}

impl ObservedProvider {
    fn new(inner: Arc<dyn IndexBackendPlugin>, cached: bool) -> Arc<Self> {
        Arc::new(Self {
            inner,
            cached,
            fault: Arc::new(WriteFault::default()),
            observed: Mutex::new(None),
        })
    }

    fn handles(&self) -> Result<Handles> {
        let observed = self
            .observed
            .lock()
            .map_err(|_| anyhow!("poisoned index observations"))?;
        let observed = observed
            .as_ref()
            .context("provider has not created indexes")?;
        Ok(Handles {
            control: observed
                .control
                .upgrade()
                .context("session control released")?,
            elements: observed
                .elements
                .upgrade()
                .context("element index released")?,
            checkpoint: observed
                .checkpoint
                .upgrade()
                .context("checkpoint store released")?,
            outbox: observed.outbox.upgrade().context("outbox released")?,
            live: observed.live.upgrade().context("live results released")?,
        })
    }
}

#[async_trait]
impl IndexBackendPlugin for ObservedProvider {
    async fn create_indexes(&self, query_id: &str) -> Result<CreatedIndexes, IndexError> {
        let mut indexes = self.inner.create_indexes(query_id).await?;
        let checkpoint = indexes
            .checkpoint_store
            .as_ref()
            .ok_or(IndexError::NotSupported)?;
        let outbox = indexes
            .outbox_writer
            .as_ref()
            .ok_or(IndexError::NotSupported)?;
        let live = indexes
            .live_results_writer
            .as_ref()
            .ok_or(IndexError::NotSupported)?;
        *self.observed.lock().map_err(|_| IndexError::IOError)? = Some(ObservedIndexes {
            control: Arc::downgrade(&indexes.set.session_control),
            elements: Arc::downgrade(&indexes.set.element_index),
            checkpoint: Arc::downgrade(checkpoint),
            outbox: Arc::downgrade(outbox),
            live: Arc::downgrade(live),
        });
        indexes.live_results_writer = Some(Arc::new(FailingLiveWriter {
            inner: live.clone(),
            fault: self.fault.clone(),
        }));
        if self.cached {
            indexes.set.element_index = Arc::new(
                CachedElementIndex::new_with_session(
                    indexes.set.element_index,
                    3,
                    indexes.set.session_control.clone(),
                )
                .expect("capacity 3 is valid"),
            );
            indexes.set.result_index = Arc::new(
                CachedResultIndex::new_with_session(
                    indexes.set.result_index,
                    3,
                    indexes.set.session_control.clone(),
                )
                .expect("capacity 3 is valid"),
            );
        }
        Ok(indexes)
    }

    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }
}

struct Handles {
    control: Arc<dyn SessionControl>,
    elements: Arc<dyn ElementIndex>,
    checkpoint: Arc<dyn CheckpointStore>,
    outbox: Arc<dyn OutboxWriter>,
    live: Arc<dyn LiveResultsWriter>,
}

#[derive(Debug, PartialEq, Eq)]
struct PersistedOutput {
    checkpoint: Option<SourceCheckpoint>,
    sequence: Option<u64>,
    outbox: Vec<(u64, Vec<u8>)>,
    rows: Vec<(u64, Vec<u8>)>,
}

impl Handles {
    async fn root(&self) -> Result<SessionGuard> {
        tokio::time::timeout(DEADLINE, async {
            loop {
                match SessionGuard::begin(self.control.clone()).await {
                    Err(error) if error.session_error() == Some(SessionError::SessionBusy) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    result => return result.map_err(anyhow::Error::from),
                }
            }
        })
        .await
        .context("provider inspection could not acquire a session")?
    }

    async fn persisted(&self, query_id: &str) -> Result<PersistedOutput> {
        let root = self.root().await?;
        let output = self.persisted_in(query_id, &root).await?;
        root.commit().await?;
        Ok(output)
    }

    async fn persisted_in(&self, query_id: &str, _root: &SessionGuard) -> Result<PersistedOutput> {
        let mut rows = self.live.read_snapshot(query_id).await?;
        rows.sort_unstable();
        Ok(PersistedOutput {
            checkpoint: self.checkpoint.read_checkpoint(SOURCE).await?,
            sequence: self.checkpoint.read_result_sequence(query_id).await?,
            outbox: self.outbox.read_from(query_id, 0).await?,
            rows,
        })
    }

    async fn element(&self, id: &str) -> Result<Option<Arc<Element>>> {
        let root = self.root().await?;
        let element = self
            .elements
            .get_element(&ElementReference::new(SOURCE, id))
            .await?;
        root.commit().await?;
        Ok(element)
    }
}

fn position(sequence: u64) -> Bytes {
    Bytes::copy_from_slice(&sequence.to_be_bytes())
}

fn metadata(id: &str, label: &str) -> ElementMetadata {
    ElementMetadata {
        reference: ElementReference::new(SOURCE, id),
        labels: Arc::new([Arc::from(label)]),
        effective_from: 1,
    }
}

fn node(id: &str, label: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: metadata(id, label),
            properties: json!({"id": id}).into(),
        },
    }
}

fn edge(id: &str, from: &str, to: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Relation {
            metadata: metadata(id, "R"),
            in_node: ElementReference::new(SOURCE, from),
            out_node: ElementReference::new(SOURCE, to),
            properties: Default::default(),
        },
    }
}

fn graph(bounded: bool) -> Vec<SourceChange> {
    let mut changes = vec![
        node("a", "Start"),
        node("x", "Start"),
        node("b", "End"),
        node("c", "End"),
        node("middle", "Transit"),
        edge("ab", "a", "b"),
    ];
    if bounded {
        changes.extend([edge("xm", "x", "middle"), edge("mb", "middle", "b")]);
    } else {
        changes.push(edge("xb", "x", "b"));
    }
    changes
}

fn query_text(bounded: bool) -> String {
    let repetition = if bounded { "*1..2" } else { "" };
    format!("MATCH (a:Start)-[:R{repetition}]->(b:End) RETURN a.id AS start, count(b) AS n")
}

fn rows(a: u64, x: u64) -> Vec<Value> {
    vec![json!({"start": "a", "n": a}), json!({"start": "x", "n": x})]
}

fn sorted(mut rows: Vec<Value>) -> Vec<Value> {
    rows.sort_by_cached_key(Value::to_string);
    rows
}

async fn build(
    provider: Arc<ObservedProvider>,
    query_id: &str,
    bounded: bool,
    source: RecordedSource,
) -> Result<DrasiLib> {
    build_text(provider, query_id, &query_text(bounded), source).await
}

async fn build_text(
    provider: Arc<ObservedProvider>,
    query_id: &str,
    text: &str,
    source: RecordedSource,
) -> Result<DrasiLib> {
    Ok(DrasiLib::builder()
        .with_default_index_provider("transaction-test", provider)
        .with_source(source)
        .with_query(
            Query::cypher(query_id)
                .query(text)
                .from_source(SOURCE)
                .build(),
        )
        .build()
        .await?)
}

async fn query(lib: &DrasiLib, id: &str) -> Result<Arc<dyn RunningQuery>> {
    lib.query_manager()
        .get_query_instance(id)
        .await
        .map_err(|message| anyhow!(message))
}

async fn wait_status(lib: &DrasiLib, id: &str, expected: ComponentStatus) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let current = lib.get_query_status(id).await?;
            if current == expected {
                return Ok(());
            }
            ensure!(
                current != ComponentStatus::Error,
                "query entered Error while waiting for {expected:?}"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .context("query status deadline")?
}

async fn wait_checkpoint(handles: &Handles, sequence: u64) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let root = handles.root().await?;
            let actual = handles.checkpoint.read_checkpoint(SOURCE).await?;
            root.commit().await?;
            if actual == Some(SourceCheckpoint::new(sequence, Some(position(sequence)))) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("checkpoint did not reach {sequence}"))?
}

async fn assert_snapshot(lib: &DrasiLib, id: &str, expected: Vec<Value>) -> Result<u64> {
    let expected = sorted(expected);
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let snapshot = query(lib, id).await?.fetch_snapshot().await?;
        let sequence = snapshot.as_of_sequence;
        let actual = sorted(snapshot.stream().collect().await);
        if actual == expected {
            return Ok(sequence);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "snapshot mismatch: expected {expected:?}, got {actual:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

async fn seed(handle: &SourceHandle, handles: &Handles, bounded: bool) -> Result<u64> {
    let mut sequence = 0;
    for change in graph(bounded) {
        sequence = handle.send(change).await?;
    }
    wait_checkpoint(handles, sequence).await?;
    Ok(sequence)
}

async fn assert_no_output(receiver: &mut dyn ChangeReceiver<QueryResult>) -> Result<()> {
    match tokio::time::timeout(Duration::from_millis(150), receiver.recv()).await {
        Err(_) => Ok(()),
        Ok(result) => Err(anyhow!("unexpected query publication: {:?}", result?)),
    }
}

async fn output_history(lib: &DrasiLib, id: &str) -> Result<Value> {
    let outbox = query(lib, id).await?.fetch_outbox(0).await?;
    Ok(serde_json::to_value(
        outbox.results.iter().map(Arc::as_ref).collect::<Vec<_>>(),
    )?)
}

fn emitted_rows(result: &QueryResult) -> Vec<Value> {
    result
        .results
        .iter()
        .map(|diff| match diff {
            ResultDiff::Add { data, .. } => data.clone(),
            ResultDiff::Update { after, .. } | ResultDiff::Aggregation { after, .. } => {
                after.clone()
            }
            other => panic!("expected an added or updated row, got {other:?}"),
        })
        .collect()
}

/// Public snapshots must not read tentative provider rows while the first output is staged.
pub async fn unpublished_rows_are_invisible(
    factory: impl Fn() -> Arc<dyn IndexBackendPlugin>,
    query_id: &str,
    cached: bool,
) -> Result<()> {
    let provider = ObservedProvider::new(factory(), cached);
    let (source, handle) = RecordedSource::new(History::default(), false, None)?;
    let lib = DrasiLib::builder()
        .with_default_index_provider("transaction-test", provider.clone())
        .with_source(source)
        .with_query(
            Query::cypher(query_id)
                .query("MATCH (n:End) RETURN n.id AS end")
                .from_source(SOURCE)
                .build(),
        )
        .build()
        .await?;
    lib.start().await?;
    wait_status(&lib, query_id, ComponentStatus::Running).await?;
    let handles = provider.handles()?;
    let baseline = handles.persisted(query_id).await?;
    assert!(baseline.rows.is_empty());
    assert!(baseline.outbox.is_empty());
    assert!(baseline.checkpoint.is_none());
    assert_eq!(assert_snapshot(&lib, query_id, vec![]).await?, 0);
    let running_query = query(&lib, query_id).await?;
    let mut output = running_query.subscribe("snapshot-observer".into()).await?;

    provider.fault.armed.store(true, Ordering::SeqCst);
    handle.send(node("tentative", "End")).await?;
    tokio::time::timeout(DEADLINE, provider.fault.staged.notified())
        .await
        .context("first live output was not staged")?;
    let snapshot = tokio::time::timeout(DEADLINE, running_query.fetch_snapshot()).await;
    let public_rows = tokio::time::timeout(DEADLINE, lib.get_query_results(query_id)).await;
    let outbox = tokio::time::timeout(DEADLINE, running_query.fetch_outbox(0)).await;
    provider.fault.release.add_permits(1);

    let snapshot = snapshot.context("snapshot blocked behind the uncommitted output")??;
    assert_eq!(snapshot.as_of_sequence, 0);
    assert!(
        snapshot.is_empty(),
        "public snapshot exposed tentative rows"
    );
    assert!(
        public_rows
            .context("public results blocked behind the uncommitted output")??
            .is_empty(),
        "public results exposed tentative rows"
    );
    let outbox = outbox.context("outbox blocked behind the uncommitted output")??;
    assert_eq!(outbox.latest_sequence, 0);
    assert!(
        outbox.results.is_empty(),
        "public outbox exposed tentative output"
    );
    wait_status(&lib, query_id, ComponentStatus::Error).await?;
    assert_no_output(output.receiver.as_mut()).await?;
    assert_eq!(handles.persisted(query_id).await?, baseline);
    assert_eq!(handle.base.compute_confirmed_position().await, None);
    assert!(handles.element("tentative").await?.is_none());
    drop(handles);
    drop(output);
    drop(running_query);
    lib.shutdown().await?;
    Ok(())
}

/// A post-write failure must roll back the entire event, retain its successor,
/// and restore both output rows and sequences when the real library reopens.
pub async fn rollback_and_replay(
    factory: impl Fn() -> Arc<dyn IndexBackendPlugin>,
    query_id: &str,
    cached: bool,
    bounded: bool,
) -> Result<()> {
    let history = History::default();
    let provider = ObservedProvider::new(factory(), cached);
    let (source, handle) = RecordedSource::new(history.clone(), false, None)?;
    let lib = build(provider.clone(), query_id, bounded, source).await?;
    lib.start().await?;
    wait_status(&lib, query_id, ComponentStatus::Running).await?;
    let handles = provider.handles()?;
    let source_sequence = seed(&handle, &handles, bounded).await?;
    let output_sequence = assert_snapshot(&lib, query_id, rows(1, 1)).await?;
    ensure!(
        output_sequence > 0,
        "baseline must contain streaming output"
    );
    let persisted = handles.persisted(query_id).await?;
    assert_eq!(persisted.sequence, Some(output_sequence));
    assert_eq!(persisted.rows.len(), 2);
    let history_before_failure = output_history(&lib, query_id).await?;
    let metrics = query(&lib, query_id)
        .await?
        .output_metrics()
        .context("query output metrics")?;
    let metrics_before_failure = metrics.snapshot();
    let mut output = query(&lib, query_id)
        .await?
        .subscribe("transaction-observer".into())
        .await?;
    assert_eq!(
        handle.base.compute_confirmed_position().await,
        Some(source_sequence)
    );
    assert_eq!(
        handle.base.compute_confirmed_source_position().await,
        Some(position(source_sequence))
    );

    provider.fault.armed.store(true, Ordering::SeqCst);
    handle.send(edge("ac", "a", "c")).await?;
    tokio::time::timeout(DEADLINE, provider.fault.staged.notified())
        .await
        .context("injected live writer was not reached")?;
    handle.send(edge("xc", "x", "c")).await?;
    provider.fault.release.add_permits(1);
    wait_status(&lib, query_id, ComponentStatus::Error).await?;
    assert_no_output(output.receiver.as_mut()).await?;
    assert_eq!(handles.persisted(query_id).await?, persisted);
    assert_eq!(metrics.snapshot().outbox_latest_seq, output_sequence);
    assert_eq!(
        metrics.snapshot().result_seq_advances,
        metrics_before_failure.result_seq_advances
    );
    assert_eq!(
        handle.base.compute_confirmed_position().await,
        Some(source_sequence)
    );
    assert_eq!(
        handle.base.compute_confirmed_source_position().await,
        Some(position(source_sequence))
    );
    for id in ["ac", "xc"] {
        assert!(
            handles.element(id).await?.is_none(),
            "uncommitted edge {id} escaped rollback"
        );
    }

    drop(handles);
    drop(output);
    lib.shutdown().await?;
    drop(lib);
    drop(handle);
    drop(provider);

    let provider = ObservedProvider::new(factory(), cached);
    let (source, handle) = RecordedSource::new(history, true, None)?;
    let lib = build(provider.clone(), query_id, bounded, source).await?;
    lib.start().await?;
    wait_status(&lib, query_id, ComponentStatus::Running).await?;
    let handles = provider.handles()?;
    assert_eq!(handles.persisted(query_id).await?, persisted);
    assert_eq!(
        assert_snapshot(&lib, query_id, rows(1, 1)).await?,
        output_sequence
    );
    assert_eq!(
        output_history(&lib, query_id).await?,
        history_before_failure
    );
    let mut output = query(&lib, query_id)
        .await?
        .subscribe("replay-observer".into())
        .await?;
    handle.resume();
    let first = tokio::time::timeout(DEADLINE, output.receiver.recv()).await??;
    assert_eq!(
        first.sequence,
        output_sequence + 1,
        "replay restarted or skipped the output sequence"
    );
    assert_eq!(emitted_rows(&first), vec![json!({"start": "a", "n": 2})]);
    let second = tokio::time::timeout(DEADLINE, output.receiver.recv()).await??;
    assert_eq!(second.sequence, output_sequence + 2);
    assert_eq!(emitted_rows(&second), vec![json!({"start": "x", "n": 2})]);
    wait_checkpoint(&handles, source_sequence + 2).await?;
    assert_eq!(
        assert_snapshot(&lib, query_id, rows(2, 2)).await?,
        output_sequence + 2
    );
    let outbox = query(&lib, query_id)
        .await?
        .fetch_outbox(output_sequence)
        .await?;
    assert_eq!(
        outbox
            .results
            .iter()
            .map(|r| r.sequence)
            .collect::<Vec<_>>(),
        vec![output_sequence + 1, output_sequence + 2]
    );
    for id in ["ac", "xc"] {
        assert!(handles.element(id).await?.is_some());
    }
    let deleted = handle
        .send(SourceChange::Delete {
            metadata: metadata("ac", "R"),
        })
        .await?;
    wait_checkpoint(&handles, deleted).await?;
    assert_eq!(
        assert_snapshot(&lib, query_id, rows(1, 2)).await?,
        output_sequence + 3
    );
    drop(handles);
    drop(output);
    lib.shutdown().await?;
    Ok(())
}

/// Holding another root must delay an input, not discard or acknowledge it.
pub async fn busy_retains_input(
    factory: impl Fn() -> Arc<dyn IndexBackendPlugin>,
    query_id: &str,
    cached: bool,
) -> Result<()> {
    let provider = ObservedProvider::new(factory(), cached);
    let (source, handle) = RecordedSource::new(History::default(), false, None)?;
    let lib = build(provider.clone(), query_id, true, source).await?;
    lib.start().await?;
    wait_status(&lib, query_id, ComponentStatus::Running).await?;
    let handles = provider.handles()?;
    let baseline = seed(&handle, &handles, true).await?;
    let sequence = assert_snapshot(&lib, query_id, rows(1, 1)).await?;
    let persisted = handles.persisted(query_id).await?;
    let mut output = query(&lib, query_id)
        .await?
        .subscribe("busy-observer".into())
        .await?;
    let root = handles.root().await?;
    assert_eq!(handle.send(edge("ac", "a", "c")).await?, baseline + 1);
    assert_no_output(output.receiver.as_mut()).await?;
    assert_eq!(handles.persisted_in(query_id, &root).await?, persisted);
    assert_eq!(
        handle.base.compute_confirmed_position().await,
        Some(baseline)
    );
    assert_eq!(
        lib.get_query_status(query_id).await?,
        ComponentStatus::Running
    );
    root.commit().await?;
    let result = tokio::time::timeout(DEADLINE, output.receiver.recv()).await??;
    assert_eq!(result.sequence, sequence + 1);
    wait_checkpoint(&handles, baseline + 1).await?;
    assert_eq!(
        assert_snapshot(&lib, query_id, rows(2, 1)).await?,
        sequence + 1
    );
    drop(handles);
    drop(output);
    lib.shutdown().await?;
    Ok(())
}

/// Bootstrap rows survive a reopen even when no streaming sequence exists yet.
pub async fn bootstrap_survives_reopen(
    factory: impl Fn() -> Arc<dyn IndexBackendPlugin>,
    query_id: &str,
    cached: bool,
) -> Result<()> {
    bootstrap_reopen_with_text(factory, query_id, cached, true, &query_text(true)).await
}

pub async fn temporal_bootstrap_survives_reopen(
    factory: impl Fn() -> Arc<dyn IndexBackendPlugin>,
    query_id: &str,
    cached: bool,
    bounded: bool,
) -> Result<()> {
    let text = query_text(bounded).replace(" RETURN ", " WHERE drasi.trueLater(true, 0) RETURN ");
    bootstrap_reopen_with_text(factory, query_id, cached, bounded, &text).await
}

async fn bootstrap_reopen_with_text(
    factory: impl Fn() -> Arc<dyn IndexBackendPlugin>,
    query_id: &str,
    cached: bool,
    bounded: bool,
    text: &str,
) -> Result<()> {
    let bootstrap = SnapshotBootstrap {
        changes: graph(bounded),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let history = History::default();
    let provider = ObservedProvider::new(factory(), cached);
    let (source, handle) = RecordedSource::new(history.clone(), false, Some(bootstrap.clone()))?;
    let lib = build_text(provider.clone(), query_id, text, source).await?;
    lib.start().await?;
    wait_status(&lib, query_id, ComponentStatus::Running).await?;
    assert_eq!(assert_snapshot(&lib, query_id, rows(1, 1)).await?, 0);
    let handles = provider.handles()?;
    wait_checkpoint(&handles, 0).await?;
    let persisted = handles.persisted(query_id).await?;
    assert!(persisted.outbox.is_empty());
    assert_eq!(persisted.rows.len(), 2);
    assert_eq!(bootstrap.calls.load(Ordering::SeqCst), 1);
    drop(handles);
    lib.shutdown().await?;
    drop(lib);
    drop(handle);
    drop(provider);

    let provider = ObservedProvider::new(factory(), cached);
    let (source, handle) = RecordedSource::new(history, false, Some(bootstrap.clone()))?;
    let lib = build_text(provider.clone(), query_id, text, source).await?;
    lib.start().await?;
    wait_status(&lib, query_id, ComponentStatus::Running).await?;
    assert_eq!(assert_snapshot(&lib, query_id, rows(1, 1)).await?, 0);
    assert_eq!(
        bootstrap.calls.load(Ordering::SeqCst),
        1,
        "recovery bootstrapped again instead of restoring"
    );
    let handles = provider.handles()?;
    assert_eq!(handles.persisted(query_id).await?, persisted);
    let mut output = query(&lib, query_id)
        .await?
        .subscribe("bootstrap-observer".into())
        .await?;
    assert_no_output(output.receiver.as_mut()).await?;
    assert_eq!(handle.send(edge("ac", "a", "c")).await?, 1);
    let result = tokio::time::timeout(DEADLINE, output.receiver.recv()).await??;
    assert_eq!(result.sequence, 1);
    wait_checkpoint(&handles, 1).await?;
    assert_eq!(assert_snapshot(&lib, query_id, rows(2, 1)).await?, 1);
    drop(handles);
    drop(output);
    lib.shutdown().await?;
    Ok(())
}
