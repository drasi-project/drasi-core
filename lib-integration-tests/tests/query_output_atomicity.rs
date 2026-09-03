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

//! Fault-injection tests for #822: query output must commit atomically with
//! index writes and source checkpoints.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::interface::{
    CheckpointStore, CreatedIndexes, IndexBackendPlugin, IndexError, LiveResultsWriter,
    OutboxWriter, RowMutation, SessionControl, SourceCheckpoint,
};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::channels::{ComponentStatus, QueryResult};
use drasi_lib::{
    CapacityPolicy, DrasiLib, DurabilityConfig, Query, Reaction, ReactionBase, ReactionBaseParams,
    ReactionRuntimeContext, RecoveryPolicy, StorageBackendRef,
};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use drasi_wal_redb::RedbWalProvider;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::time::timeout;

const SOURCE_ID: &str = "people-source";
const QUERY_ID: &str = "people-query";
const REACTION_ID: &str = "capturing-reaction";
const QUERY_TEXT: &str =
    "MATCH (p:Person) RETURN p.personId AS id, p.name AS name, p.active AS active";
const FUTURE_QUERY_TEXT: &str = "\
MATCH (p:Person)
WHERE drasi.trueFor(true, duration ({ milliseconds: 200 }))
RETURN p.personId AS id, p.name AS name, p.active AS active";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PersistStage {
    BeforeSourceCheckpoint,
    BeforeOutboxAppend,
    BeforeLiveResultsApply,
    BeforeResultSequence,
    AfterCommit,
}

struct FaultInjector {
    stage: PersistStage,
    armed: AtomicBool,
    fired: AtomicBool,
}

impl FaultInjector {
    fn new(stage: PersistStage) -> Arc<Self> {
        Arc::new(Self {
            stage,
            armed: AtomicBool::new(false),
            fired: AtomicBool::new(false),
        })
    }

    fn arm(&self) {
        self.armed.store(true, Ordering::SeqCst);
    }

    fn should_fail(&self, stage: PersistStage) -> bool {
        if self.armed.load(Ordering::SeqCst) && self.stage == stage {
            self.fired.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

fn injected(stage: PersistStage) -> IndexError {
    IndexError::other(std::io::Error::other(format!(
        "injected failure at {stage:?}"
    )))
}

struct FaultInjectingIndexProvider {
    inner: RocksDbIndexProvider,
    fault: Arc<FaultInjector>,
}

#[async_trait]
impl IndexBackendPlugin for FaultInjectingIndexProvider {
    async fn create_indexes(&self, query_id: &str) -> Result<CreatedIndexes, IndexError> {
        let mut created = self.inner.create_indexes(query_id).await?;
        created.set.session_control = Arc::new(FailingSessionControl {
            inner: created.set.session_control,
            fault: self.fault.clone(),
        });
        created.checkpoint_store = created.checkpoint_store.map(|store| {
            Arc::new(FailingCheckpointStore {
                inner: store,
                fault: self.fault.clone(),
            }) as Arc<dyn CheckpointStore>
        });
        created.outbox_writer = created.outbox_writer.map(|writer| {
            Arc::new(FailingOutboxWriter {
                inner: writer,
                fault: self.fault.clone(),
            }) as Arc<dyn OutboxWriter>
        });
        created.live_results_writer = created.live_results_writer.map(|writer| {
            Arc::new(FailingLiveResultsWriter {
                inner: writer,
                fault: self.fault.clone(),
            }) as Arc<dyn LiveResultsWriter>
        });
        Ok(created)
    }

    fn is_volatile(&self) -> bool {
        false
    }
}

struct FailingSessionControl {
    inner: Arc<dyn SessionControl>,
    fault: Arc<FaultInjector>,
}

#[async_trait]
impl SessionControl for FailingSessionControl {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }

    async fn commit(&self) -> Result<(), IndexError> {
        self.inner.commit().await?;
        if self.fault.should_fail(PersistStage::AfterCommit) {
            return Err(injected(PersistStage::AfterCommit));
        }
        Ok(())
    }

    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

struct FailingCheckpointStore {
    inner: Arc<dyn CheckpointStore>,
    fault: Arc<FaultInjector>,
}

#[async_trait]
impl CheckpointStore for FailingCheckpointStore {
    fn is_persistent(&self) -> bool {
        self.inner.is_persistent()
    }

    async fn stage_checkpoint(
        &self,
        source_id: &str,
        sequence: u64,
        source_position: Option<&bytes::Bytes>,
    ) -> Result<(), IndexError> {
        if self.fault.should_fail(PersistStage::BeforeSourceCheckpoint) {
            return Err(injected(PersistStage::BeforeSourceCheckpoint));
        }
        self.inner
            .stage_checkpoint(source_id, sequence, source_position)
            .await
    }

    async fn read_checkpoint(
        &self,
        source_id: &str,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        self.inner.read_checkpoint(source_id).await
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

    async fn stage_result_sequence(&self, query_id: &str, sequence: u64) -> Result<(), IndexError> {
        if self.fault.should_fail(PersistStage::BeforeResultSequence) {
            return Err(injected(PersistStage::BeforeResultSequence));
        }
        self.inner.stage_result_sequence(query_id, sequence).await
    }

    async fn write_result_sequence(&self, query_id: &str, sequence: u64) -> Result<(), IndexError> {
        self.inner.write_result_sequence(query_id, sequence).await
    }

    async fn read_result_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_result_sequence(query_id).await
    }
}

struct FailingOutboxWriter {
    inner: Arc<dyn OutboxWriter>,
    fault: Arc<FaultInjector>,
}

#[async_trait]
impl OutboxWriter for FailingOutboxWriter {
    async fn append(&self, query_id: &str, sequence: u64, data: &[u8]) -> Result<(), IndexError> {
        if self.fault.should_fail(PersistStage::BeforeOutboxAppend) {
            return Err(injected(PersistStage::BeforeOutboxAppend));
        }
        self.inner.append(query_id, sequence, data).await
    }

    async fn read_from(
        &self,
        query_id: &str,
        after_sequence: u64,
    ) -> Result<Vec<(u64, Vec<u8>)>, IndexError> {
        self.inner.read_from(query_id, after_sequence).await
    }

    async fn read_latest_sequence(&self, query_id: &str) -> Result<Option<u64>, IndexError> {
        self.inner.read_latest_sequence(query_id).await
    }

    async fn clear(&self, query_id: &str) -> Result<(), IndexError> {
        self.inner.clear(query_id).await
    }

    async fn trim_to_capacity(&self, query_id: &str, capacity: usize) -> Result<usize, IndexError> {
        self.inner.trim_to_capacity(query_id, capacity).await
    }
}

struct FailingLiveResultsWriter {
    inner: Arc<dyn LiveResultsWriter>,
    fault: Arc<FaultInjector>,
}

#[async_trait]
impl LiveResultsWriter for FailingLiveResultsWriter {
    async fn apply_mutations(
        &self,
        query_id: &str,
        mutations: &[RowMutation<'_>],
    ) -> Result<(), IndexError> {
        if self.fault.should_fail(PersistStage::BeforeLiveResultsApply) {
            return Err(injected(PersistStage::BeforeLiveResultsApply));
        }
        self.inner.apply_mutations(query_id, mutations).await
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

struct CapturingReaction {
    base: ReactionBase,
    captured: Arc<RwLock<Vec<u64>>>,
}

impl CapturingReaction {
    fn new(captured: Arc<RwLock<Vec<u64>>>) -> Self {
        Self {
            base: ReactionBase::new(ReactionBaseParams::new(
                REACTION_ID,
                vec![QUERY_ID.to_string()],
            )),
            captured,
        }
    }
}

#[async_trait]
impl Reaction for CapturingReaction {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "capturing"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::new()
    }

    fn query_ids(&self) -> Vec<String> {
        self.base.get_queries().to_vec()
    }

    fn auto_start(&self) -> bool {
        self.base.get_auto_start()
    }

    async fn initialize(&self, context: ReactionRuntimeContext) {
        self.base.initialize(context).await;
    }

    async fn start(&self) -> anyhow::Result<()> {
        self.base
            .set_status(ComponentStatus::Running, Some("Started".to_string()))
            .await;
        let queue = self.base.priority_queue.clone();
        let captured = self.captured.clone();
        let mut shutdown = self.base.create_shutdown_channel().await;
        let task = tokio::spawn(async move {
            loop {
                let result = tokio::select! {
                    biased;
                    _ = &mut shutdown => break,
                    result = queue.dequeue() => result,
                };
                captured.write().await.push(result.sequence);
            }
        });
        self.base.set_processing_task(task).await;
        Ok(())
    }

    async fn stop(&self) -> anyhow::Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn deprovision(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> anyhow::Result<()> {
        self.base.enqueue_query_result(result).await
    }
}

struct Paths {
    rocks: PathBuf,
    wal_a: PathBuf,
    wal_b: PathBuf,
}

impl Paths {
    fn new(label: &str) -> Result<Self> {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("clock")?
            .as_nanos();
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .context("repo parent")?
            .join("target")
            .join("query-output-atomicity")
            .join(format!("{}-{}-{label}", std::process::id(), timestamp));
        let paths = Self {
            rocks: root.join("rocksdb"),
            wal_a: root.join("source-wal-a"),
            wal_b: root.join("source-wal-b"),
        };
        std::fs::create_dir_all(&paths.rocks)?;
        std::fs::create_dir_all(&paths.wal_a)?;
        std::fs::create_dir_all(&paths.wal_b)?;
        Ok(paths)
    }

    fn cleanup(&self) {
        if let Some(parent) = self.rocks.parent() {
            let _ = std::fs::remove_dir_all(parent);
        }
    }
}

#[derive(Debug)]
struct DurableObservation {
    source_sequence: Option<u64>,
    result_sequence: Option<u64>,
    outbox_sequences: Vec<u64>,
    live_row_count: usize,
}

async fn inspect_durable(rocks: &Path) -> Result<DurableObservation> {
    let provider = RocksDbIndexProvider::new(rocks, false, false);
    let created = provider.create_indexes(QUERY_ID).await?;
    let store = created
        .checkpoint_store
        .as_ref()
        .context("checkpoint store")?;
    let source_sequence = store
        .read_checkpoint(SOURCE_ID)
        .await?
        .map(|cp| cp.sequence);
    let result_sequence = store.read_result_sequence(QUERY_ID).await?;
    let outbox_sequences = created
        .outbox_writer
        .as_ref()
        .context("outbox")?
        .read_from(QUERY_ID, 0)
        .await?
        .into_iter()
        .map(|(seq, _)| seq)
        .collect();
    let live_row_count = created
        .live_results_writer
        .as_ref()
        .context("live results")?
        .row_count(QUERY_ID)
        .await?;
    drop(created);
    drop(provider);
    Ok(DurableObservation {
        source_sequence,
        result_sequence,
        outbox_sequences,
        live_row_count,
    })
}

struct FixtureOpts {
    query_text: &'static str,
    fault: Option<Arc<FaultInjector>>,
    include_reaction: bool,
    captured: Arc<RwLock<Vec<u64>>>,
    wal: PathBuf,
}

async fn build_core(
    paths: &Paths,
    opts: FixtureOpts,
) -> Result<(DrasiLib, ApplicationSourceHandle)> {
    let source_config = ApplicationSourceConfig {
        properties: HashMap::new(),
        durability: Some(DurabilityConfig {
            enabled: true,
            max_events: 128,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }),
    };
    let (source, handle) = ApplicationSource::new(SOURCE_ID, source_config)?;
    let wal = Arc::new(RedbWalProvider::new(&opts.wal));
    let mut builder = DrasiLib::builder()
        .with_id("query-output-atomicity")
        .with_source(source)
        .with_query(
            Query::cypher(QUERY_ID)
                .query(opts.query_text)
                .from_source(SOURCE_ID)
                .auto_start(true)
                .enable_bootstrap(false)
                .with_outbox_capacity(32)
                .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
                .with_recovery_policy(RecoveryPolicy::Strict)
                .build(),
        )
        .with_wal_provider(wal);

    if let Some(fault) = opts.fault {
        builder = builder.with_index_provider(
            "rocks",
            Arc::new(FaultInjectingIndexProvider {
                inner: RocksDbIndexProvider::new(&paths.rocks, false, false),
                fault,
            }),
        );
    } else {
        builder = builder.with_index_provider(
            "rocks",
            Arc::new(RocksDbIndexProvider::new(&paths.rocks, false, false)),
        );
    }

    if opts.include_reaction {
        builder = builder.with_reaction(CapturingReaction::new(opts.captured));
    }

    Ok((builder.build().await?, handle))
}

async fn wait_for_status(core: &DrasiLib, id: &str, expected: ComponentStatus) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let graph = core.get_graph().await;
        if graph
            .nodes
            .iter()
            .any(|node| node.id == id && node.status == expected)
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("component '{id}' did not reach {expected:?}: {graph:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn insert_person(source: &ApplicationSourceHandle, id: &str, name: &str) -> Result<()> {
    let properties = PropertyMapBuilder::new()
        .with_string("personId", id)
        .with_string("name", name)
        .with_bool("active", true)
        .build();
    source
        .send_node_insert(id, vec!["Person"], properties)
        .await
}

async fn wait_for_fault(fault: &FaultInjector) -> Result<()> {
    timeout(Duration::from_secs(8), async {
        loop {
            if fault.fired() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("timed out waiting for injected failure")?;
    // Give rollback a moment to finish.
    tokio::time::sleep(Duration::from_millis(50)).await;
    Ok(())
}

async fn wait_for_snapshot_seq(core: &DrasiLib, min_seq: u64) -> Result<u64> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let query = core
            .query_manager()
            .get_query_instance(QUERY_ID)
            .await
            .map_err(anyhow::Error::msg)?;
        let snapshot = query.fetch_snapshot().await?;
        if snapshot.as_of_sequence >= min_seq && !snapshot.to_vec().is_empty() {
            return Ok(snapshot.as_of_sequence);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "snapshot did not reach seq {min_seq}; got seq={} rows={:?}",
                snapshot.as_of_sequence,
                snapshot.to_vec()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn start_running(core: &DrasiLib) -> Result<()> {
    core.start().await?;
    wait_for_status(core, SOURCE_ID, ComponentStatus::Running).await?;
    wait_for_status(core, QUERY_ID, ComponentStatus::Running).await?;
    Ok(())
}

fn assert_output_absent(obs: &DurableObservation, context: &str) {
    assert_eq!(
        obs.result_sequence.unwrap_or(0),
        0,
        "{context}: result sequence must not advance"
    );
    assert!(
        obs.outbox_sequences.is_empty(),
        "{context}: outbox must be empty, got {:?}",
        obs.outbox_sequences
    );
    assert_eq!(
        obs.live_row_count, 0,
        "{context}: live results must be empty"
    );
}

async fn run_mid_txn_failure(stage: PersistStage) -> Result<()> {
    let paths = Paths::new(&format!("{stage:?}"))?;
    let fault = FaultInjector::new(stage);
    {
        let (core, source) = build_core(
            &paths,
            FixtureOpts {
                query_text: QUERY_TEXT,
                fault: Some(fault.clone()),
                include_reaction: false,
                captured: Arc::new(RwLock::new(Vec::new())),
                wal: paths.wal_a.clone(),
            },
        )
        .await?;
        start_running(&core).await?;
        fault.arm();
        insert_person(&source, "p1", "Alice").await?;
        wait_for_fault(&fault).await?;
        core.shutdown().await?;
    }

    let failed = inspect_durable(&paths.rocks).await?;
    assert!(
        failed.source_sequence.unwrap_or(0) == 0,
        "{stage:?}: source checkpoint must not advance, got {:?}",
        failed.source_sequence
    );
    assert_output_absent(&failed, &format!("{stage:?} after rollback"));

    {
        let (core, source) = build_core(
            &paths,
            FixtureOpts {
                query_text: QUERY_TEXT,
                fault: None,
                include_reaction: false,
                captured: Arc::new(RwLock::new(Vec::new())),
                wal: paths.wal_b.clone(),
            },
        )
        .await?;
        start_running(&core).await?;
        // Checkpoint did not advance, so a restarted source would replay. This
        // in-process fixture has no WAL lock we can reopen; re-insert simulates replay.
        insert_person(&source, "p1", "Alice").await?;
        let seq = wait_for_snapshot_seq(&core, 1).await?;
        assert_eq!(seq, 1, "{stage:?}: restart must reprocess the source event");
        let recovered = {
            let query = core
                .query_manager()
                .get_query_instance(QUERY_ID)
                .await
                .map_err(anyhow::Error::msg)?;
            query.fetch_outbox(0).await?
        };
        assert_eq!(
            recovered
                .results
                .iter()
                .map(|r| r.sequence)
                .collect::<Vec<_>>(),
            vec![1],
            "{stage:?}: restart outbox should contain sequence 1"
        );
        core.shutdown().await?;
    }

    let durable = inspect_durable(&paths.rocks).await?;
    assert_eq!(durable.source_sequence, Some(1));
    assert_eq!(durable.result_sequence, Some(1));
    assert_eq!(durable.outbox_sequences, vec![1]);
    assert_eq!(durable.live_row_count, 1);
    paths.cleanup();
    Ok(())
}

#[tokio::test]
async fn fail_after_index_before_source_checkpoint_rolls_back() -> Result<()> {
    run_mid_txn_failure(PersistStage::BeforeSourceCheckpoint).await
}

#[tokio::test]
async fn fail_after_source_checkpoint_before_outbox_rolls_back() -> Result<()> {
    run_mid_txn_failure(PersistStage::BeforeOutboxAppend).await
}

#[tokio::test]
async fn fail_after_outbox_before_live_results_rolls_back() -> Result<()> {
    run_mid_txn_failure(PersistStage::BeforeLiveResultsApply).await
}

#[tokio::test]
async fn fail_after_live_results_before_result_sequence_rolls_back() -> Result<()> {
    run_mid_txn_failure(PersistStage::BeforeResultSequence).await
}

#[tokio::test]
async fn fail_after_commit_before_in_memory_update_hydrates_on_restart() -> Result<()> {
    let paths = Paths::new("after-commit")?;
    let fault = FaultInjector::new(PersistStage::AfterCommit);
    let captured = Arc::new(RwLock::new(Vec::new()));
    {
        let (core, source) = build_core(
            &paths,
            FixtureOpts {
                query_text: QUERY_TEXT,
                fault: Some(fault.clone()),
                include_reaction: true,
                captured: captured.clone(),
                wal: paths.wal_a.clone(),
            },
        )
        .await?;
        start_running(&core).await?;
        wait_for_status(&core, REACTION_ID, ComponentStatus::Running).await?;
        fault.arm();
        insert_person(&source, "p1", "Alice").await?;
        wait_for_fault(&fault).await?;
        core.shutdown().await?;
    }

    let committed = inspect_durable(&paths.rocks).await?;
    assert_eq!(committed.source_sequence, Some(1));
    assert_eq!(committed.result_sequence, Some(1));
    assert_eq!(committed.outbox_sequences, vec![1]);
    assert_eq!(committed.live_row_count, 1);
    assert!(
        captured.read().await.is_empty(),
        "dispatch must not happen after a post-commit failure"
    );

    {
        let captured_restart = Arc::new(RwLock::new(Vec::new()));
        let (core, _source) = build_core(
            &paths,
            FixtureOpts {
                query_text: QUERY_TEXT,
                fault: None,
                include_reaction: true,
                captured: captured_restart,
                wal: paths.wal_b.clone(),
            },
        )
        .await?;
        start_running(&core).await?;
        wait_for_status(&core, REACTION_ID, ComponentStatus::Running).await?;
        let seq = wait_for_snapshot_seq(&core, 1).await?;
        assert_eq!(seq, 1, "restart must hydrate the committed output");
        let outbox = core
            .query_manager()
            .get_query_instance(QUERY_ID)
            .await
            .map_err(anyhow::Error::msg)?
            .fetch_outbox(0)
            .await?;
        assert_eq!(
            outbox
                .results
                .iter()
                .map(|result| result.sequence)
                .collect::<Vec<_>>(),
            vec![1],
            "hydrated outbox must contain the committed sequence"
        );
        // Live dispatch after hydrate is at-least-once and may be absent for a
        // non-durable capturing reaction; durable replay is covered by hydrate.
        core.shutdown().await?;
    }
    paths.cleanup();
    Ok(())
}

#[tokio::test]
async fn fail_during_process_due_futures_output_persist_rolls_back() -> Result<()> {
    let paths = Paths::new("due-futures")?;
    let fault = FaultInjector::new(PersistStage::BeforeOutboxAppend);
    {
        let (core, source) = build_core(
            &paths,
            FixtureOpts {
                query_text: FUTURE_QUERY_TEXT,
                fault: Some(fault.clone()),
                include_reaction: false,
                captured: Arc::new(RwLock::new(Vec::new())),
                wal: paths.wal_a.clone(),
            },
        )
        .await?;
        start_running(&core).await?;
        fault.arm();
        insert_person(&source, "p1", "Alice").await?;
        wait_for_fault(&fault).await?;
        core.shutdown().await?;
    }

    let failed = inspect_durable(&paths.rocks).await?;
    assert_eq!(
        failed.source_sequence,
        Some(1),
        "insert checkpoint should commit; futures persist is a later txn"
    );
    assert_output_absent(&failed, "due-futures after rollback");

    {
        let (core, _source) = build_core(
            &paths,
            FixtureOpts {
                query_text: FUTURE_QUERY_TEXT,
                fault: None,
                include_reaction: false,
                captured: Arc::new(RwLock::new(Vec::new())),
                wal: paths.wal_b.clone(),
            },
        )
        .await?;
        start_running(&core).await?;
        let seq = wait_for_snapshot_seq(&core, 1).await?;
        assert_eq!(seq, 1, "rolled-back future must be reprocessed");
        core.shutdown().await?;
    }

    let durable = inspect_durable(&paths.rocks).await?;
    assert_eq!(durable.result_sequence, Some(1));
    assert_eq!(durable.outbox_sequences, vec![1]);
    assert_eq!(durable.live_row_count, 1);
    paths.cleanup();
    Ok(())
}

#[tokio::test]
async fn happy_path_commits_source_checkpoint_result_sequence_outbox_and_live_rows() -> Result<()> {
    let paths = Paths::new("happy")?;
    let captured = Arc::new(RwLock::new(Vec::new()));
    {
        let (core, source) = build_core(
            &paths,
            FixtureOpts {
                query_text: QUERY_TEXT,
                fault: None,
                include_reaction: true,
                captured: captured.clone(),
                wal: paths.wal_a.clone(),
            },
        )
        .await?;
        start_running(&core).await?;
        wait_for_status(&core, REACTION_ID, ComponentStatus::Running).await?;
        insert_person(&source, "p1", "Alice").await?;
        let seq = wait_for_snapshot_seq(&core, 1).await?;
        assert_eq!(seq, 1);

        let query = core
            .query_manager()
            .get_query_instance(QUERY_ID)
            .await
            .map_err(anyhow::Error::msg)?;
        let snapshot = query.fetch_snapshot().await?;
        let outbox = query.fetch_outbox(0).await?;
        assert_eq!(snapshot.as_of_sequence, 1);
        assert_eq!(snapshot.to_vec().len(), 1);
        assert_eq!(
            outbox
                .results
                .iter()
                .map(|r| r.sequence)
                .collect::<Vec<_>>(),
            vec![1]
        );
        core.shutdown().await?;
    }

    let durable = inspect_durable(&paths.rocks).await?;
    assert_eq!(durable.source_sequence, Some(1), "source checkpoint S");
    assert_eq!(durable.result_sequence, Some(1), "result sequence N");
    assert_eq!(durable.outbox_sequences, vec![1], "outbox key N");
    assert_eq!(durable.live_row_count, 1, "live row set at N");

    let sequences = captured.read().await.clone();
    assert!(
        sequences.contains(&1),
        "reaction should receive sequence 1, got {sequences:?}"
    );
    paths.cleanup();
    Ok(())
}
