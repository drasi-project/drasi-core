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

//! Tests for #823: wipe query output on reconfigure and delete-and-recreate.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::interface::{CheckpointStore, IndexBackendPlugin};
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::channels::{ComponentStatus, QueryResult};
use drasi_lib::queries::Query as QueryInstance;
use drasi_lib::reactions::BootstrapContext;
use drasi_lib::{
    CapacityPolicy, DrasiLib, DurabilityConfig, Query, Reaction, ReactionBase, ReactionBaseParams,
    ReactionCheckpoint, ReactionRecoveryPolicy, ReactionRuntimeContext, RecoveryPolicy,
    StateStoreProvider, StorageBackendRef,
};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use drasi_state_store_redb::RedbStateStoreProvider;
use drasi_wal_redb::RedbWalProvider;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::timeout;

const SOURCE_ID: &str = "people-source";
const QUERY_ID: &str = "people-query";
const REACTION_ID: &str = "wipe-recording-reaction";
const QUERY_TEXT_V1: &str = "MATCH (p:Person) RETURN p.personId AS id, p.name AS name";
const QUERY_TEXT_V2: &str =
    "MATCH (p:Person) RETURN p.personId AS id, p.name AS name, p.active AS active";

struct Paths {
    rocks: PathBuf,
    wal: PathBuf,
    state: PathBuf,
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
            .join("query-output-wipe")
            .join(format!("{}-{}-{label}", std::process::id(), timestamp));
        let paths = Self {
            rocks: root.join("rocksdb"),
            wal: root.join("source-wal"),
            state: root.join("reaction-state.redb"),
        };
        std::fs::create_dir_all(&paths.rocks)?;
        std::fs::create_dir_all(&paths.wal)?;
        if let Some(parent) = paths.state.parent() {
            std::fs::create_dir_all(parent)?;
        }
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
        result_sequence,
        outbox_sequences,
        live_row_count,
    })
}

fn query_config(text: &str) -> drasi_lib::config::QueryConfig {
    Query::cypher(QUERY_ID)
        .query(text)
        .from_source(SOURCE_ID)
        .auto_start(true)
        .enable_bootstrap(false)
        .with_outbox_capacity(32)
        .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
        .with_recovery_policy(RecoveryPolicy::Strict)
        .build()
}

struct RecordingReaction {
    base: ReactionBase,
    tx: mpsc::UnboundedSender<QueryResult>,
    recovery_policy: ReactionRecoveryPolicy,
    snapshot_on_fresh: bool,
    bootstrap_count: Arc<AtomicUsize>,
    bootstrap_as_of: Arc<Mutex<Option<u64>>>,
}

struct RecordingReceiver {
    rx: mpsc::UnboundedReceiver<QueryResult>,
    bootstrap_count: Arc<AtomicUsize>,
    bootstrap_as_of: Arc<Mutex<Option<u64>>>,
}

impl RecordingReceiver {
    async fn wait_for_count(&mut self, count: usize, dur: Duration) -> Vec<QueryResult> {
        let mut results = Vec::new();
        let deadline = tokio::time::Instant::now() + dur;
        while results.len() < count {
            match timeout(deadline - tokio::time::Instant::now(), self.rx.recv()).await {
                Ok(Some(r)) => results.push(r),
                Ok(None) => break,
                Err(_) => break,
            }
        }
        results
    }

    fn drain_available(&mut self) -> Vec<QueryResult> {
        let mut results = Vec::new();
        while let Ok(r) = self.rx.try_recv() {
            results.push(r);
        }
        results
    }

    fn bootstrap_count(&self) -> usize {
        self.bootstrap_count.load(Ordering::SeqCst)
    }

    fn bootstrap_as_of(&self) -> Option<u64> {
        *self.bootstrap_as_of.lock().expect("bootstrap_as_of lock")
    }
}

fn recording_reaction(
    policy: ReactionRecoveryPolicy,
    snapshot_on_fresh: bool,
    auto_start: bool,
) -> (RecordingReaction, RecordingReceiver) {
    let (tx, rx) = mpsc::unbounded_channel();
    let params = ReactionBaseParams::new(REACTION_ID, vec![QUERY_ID.to_string()])
        .with_recovery_policy(policy)
        .with_auto_start(auto_start);
    let bootstrap_count = Arc::new(AtomicUsize::new(0));
    let bootstrap_as_of = Arc::new(Mutex::new(None));
    (
        RecordingReaction {
            base: ReactionBase::new(params),
            tx,
            recovery_policy: policy,
            snapshot_on_fresh,
            bootstrap_count: bootstrap_count.clone(),
            bootstrap_as_of: bootstrap_as_of.clone(),
        },
        RecordingReceiver {
            rx,
            bootstrap_count,
            bootstrap_as_of,
        },
    )
}

#[async_trait]
impl Reaction for RecordingReaction {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "wipe-recording"
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

    async fn start(&self) -> Result<()> {
        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Recording reaction started".into()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base
            .set_status(
                ComponentStatus::Stopped,
                Some("Recording reaction stopped".into()),
            )
            .await;
        Ok(())
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        let query_id = result.query_id.clone();
        let sequence = result.sequence;
        let config_hash = self
            .base
            .read_checkpoint(&query_id)
            .await?
            .map(|checkpoint| checkpoint.config_hash)
            .unwrap_or(0);

        self.tx
            .send(result)
            .map_err(|_| anyhow::anyhow!("Recording reaction receiver closed"))?;
        self.base
            .write_checkpoint(
                &query_id,
                &ReactionCheckpoint {
                    sequence,
                    config_hash,
                },
            )
            .await?;
        Ok(())
    }

    fn is_durable(&self) -> bool {
        true
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        self.snapshot_on_fresh
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        self.recovery_policy
    }

    async fn bootstrap(&self, context: BootstrapContext) -> Result<()> {
        let snapshot = context
            .fetch_snapshot()
            .await
            .map_err(|error| anyhow::anyhow!("fetch reaction bootstrap snapshot: {error}"))?;
        self.bootstrap_count.fetch_add(1, Ordering::SeqCst);
        *self.bootstrap_as_of.lock().expect("bootstrap_as_of lock") = Some(snapshot.as_of_sequence);
        Ok(())
    }
}

struct CoreOpts {
    query_text: &'static str,
    wal: PathBuf,
    state: Option<Arc<RedbStateStoreProvider>>,
    reaction: Option<RecordingReaction>,
}

async fn build_core(paths: &Paths, opts: CoreOpts) -> Result<(DrasiLib, ApplicationSourceHandle)> {
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
        .with_id("query-output-wipe")
        .with_source(source)
        .with_query(query_config(opts.query_text))
        .with_wal_provider(wal)
        .with_index_provider(
            "rocks",
            Arc::new(RocksDbIndexProvider::new(&paths.rocks, false, false)),
        );

    if let Some(store) = opts.state {
        builder = builder.with_state_store_provider(store);
    }
    if let Some(reaction) = opts.reaction {
        builder = builder.with_reaction(reaction);
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

async fn wait_for_snapshot_seq(core: &DrasiLib, min_seq: u64) -> Result<u64> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let query = core
            .query_manager()
            .get_query_instance(QUERY_ID)
            .await
            .map_err(anyhow::Error::msg)?;
        let snapshot = query.fetch_snapshot().await?;
        if snapshot.as_of_sequence >= min_seq {
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

async fn fetch_query(core: &DrasiLib) -> Result<Arc<dyn QueryInstance>> {
    core.query_manager()
        .get_query_instance(QUERY_ID)
        .await
        .map_err(anyhow::Error::msg)
}

async fn outbox_sequences(core: &DrasiLib) -> Result<(u64, Vec<u64>, u64)> {
    let query = fetch_query(core).await?;
    let outbox = query.fetch_outbox(0).await?;
    Ok((
        outbox.latest_sequence,
        outbox.results.iter().map(|r| r.sequence).collect(),
        outbox.config_hash,
    ))
}

async fn read_reaction_checkpoint(
    store: &RedbStateStoreProvider,
) -> Result<Option<ReactionCheckpoint>> {
    let key = format!("checkpoint:{QUERY_ID}");
    match store.get(REACTION_ID, &key).await? {
        Some(bytes) => Ok(Some(bincode::deserialize(&bytes)?)),
        None => Ok(None),
    }
}

async fn stop_reaction_and_wait(core: &DrasiLib) -> Result<()> {
    core.stop_reaction(REACTION_ID).await?;
    wait_for_status(core, REACTION_ID, ComponentStatus::Stopped).await
}

/// 1. Update query text in-process: old output disappears, next emission is seq 1 of H2.
#[tokio::test]
async fn update_query_text_wipes_output_in_process() -> Result<()> {
    let paths = Paths::new("update-in-process")?;
    let (core, source) = build_core(
        &paths,
        CoreOpts {
            query_text: QUERY_TEXT_V1,
            wal: paths.wal.clone(),
            state: None,
            reaction: None,
        },
    )
    .await?;
    start_running(&core).await?;

    insert_person(&source, "p1", "Alice").await?;
    insert_person(&source, "p2", "Bob").await?;
    insert_person(&source, "p3", "Carol").await?;
    let seq = wait_for_snapshot_seq(&core, 3).await?;
    assert_eq!(seq, 3);
    let (latest, sequences, hash_v1) = outbox_sequences(&core).await?;
    assert_eq!(latest, 3);
    assert_eq!(sequences, vec![1, 2, 3]);

    core.update_query(QUERY_ID, query_config(QUERY_TEXT_V2))
        .await?;
    wait_for_status(&core, QUERY_ID, ComponentStatus::Running).await?;

    let query = fetch_query(&core).await?;
    let snapshot = query.fetch_snapshot().await?;
    assert_eq!(
        snapshot.as_of_sequence, 0,
        "reconfigure must reset sequence to 0"
    );
    assert!(
        snapshot.is_empty(),
        "snapshot after update must not contain old-config rows: {:?}",
        snapshot.to_vec()
    );
    assert_ne!(snapshot.config_hash, hash_v1);
    let outbox = query.fetch_outbox(0).await?;
    assert!(
        outbox.results.is_empty(),
        "fetch_outbox(0) must not return old-config QueryResults, got {:?}",
        outbox
            .results
            .iter()
            .map(|r| r.sequence)
            .collect::<Vec<_>>()
    );
    assert_eq!(outbox.latest_sequence, 0);
    assert_eq!(outbox.config_hash, snapshot.config_hash);

    insert_person(&source, "p4", "Dana").await?;
    let seq = wait_for_snapshot_seq(&core, 1).await?;
    assert_eq!(seq, 1, "next emission after reconfigure must be sequence 1");
    let (latest, sequences, hash_v2) = outbox_sequences(&core).await?;
    assert_eq!(latest, 1);
    assert_eq!(sequences, vec![1]);
    assert_eq!(hash_v2, snapshot.config_hash);

    core.shutdown().await?;
    paths.cleanup();
    Ok(())
}

/// 2. Update then new-process restart: hydrated state is the new config only.
#[tokio::test]
async fn update_then_new_process_restart_hydrates_new_config_only() -> Result<()> {
    let paths = Paths::new("update-restart")?;
    {
        let (core, source) = build_core(
            &paths,
            CoreOpts {
                query_text: QUERY_TEXT_V1,
                wal: paths.wal.clone(),
                state: None,
                reaction: None,
            },
        )
        .await?;
        start_running(&core).await?;
        insert_person(&source, "p1", "Alice").await?;
        insert_person(&source, "p2", "Bob").await?;
        insert_person(&source, "p3", "Carol").await?;
        wait_for_snapshot_seq(&core, 3).await?;

        core.update_query(QUERY_ID, query_config(QUERY_TEXT_V2))
            .await?;
        wait_for_status(&core, QUERY_ID, ComponentStatus::Running).await?;
        insert_person(&source, "p4", "Dana").await?;
        wait_for_snapshot_seq(&core, 1).await?;
        core.shutdown().await?;
    }

    let durable = inspect_durable(&paths.rocks).await?;
    assert_eq!(
        durable.result_sequence,
        Some(1),
        "durable result sequence must be the new-config head, not leftover 3"
    );
    assert_eq!(
        durable.outbox_sequences,
        vec![1],
        "old outbox keys must not coexist with the reset sequence, got {:?}",
        durable.outbox_sequences
    );
    assert_eq!(
        durable.live_row_count, 1,
        "old live rows must not come back after reconfigure"
    );

    {
        let wal_b = paths.wal.parent().unwrap().join("source-wal-b");
        std::fs::create_dir_all(&wal_b)?;
        let (core, _source) = build_core(
            &paths,
            CoreOpts {
                query_text: QUERY_TEXT_V2,
                wal: wal_b,
                state: None,
                reaction: None,
            },
        )
        .await?;
        start_running(&core).await?;
        let snapshot = fetch_query(&core).await?.fetch_snapshot().await?;
        assert_eq!(snapshot.as_of_sequence, 1);
        assert_eq!(snapshot.len(), 1);
        let (latest, sequences, _) = outbox_sequences(&core).await?;
        assert_eq!(latest, 1);
        assert_eq!(sequences, vec![1]);
        core.shutdown().await?;
    }

    paths.cleanup();
    Ok(())
}

/// 3. Delete-and-recreate the same query id with cleanup starts at sequence 0.
///
/// `DrasiLib::remove_query` always performs persistent cleanup (there is no
/// `cleanup: false` for queries, unlike sources/reactions). Recreating the
/// same id must not see preserved outbox/snapshot/sequence.
#[tokio::test]
async fn delete_and_recreate_same_query_id_starts_at_sequence_zero() -> Result<()> {
    let paths = Paths::new("delete-recreate")?;
    let (core, source) = build_core(
        &paths,
        CoreOpts {
            query_text: QUERY_TEXT_V1,
            wal: paths.wal.clone(),
            state: None,
            reaction: None,
        },
    )
    .await?;
    start_running(&core).await?;
    insert_person(&source, "p1", "Alice").await?;
    insert_person(&source, "p2", "Bob").await?;
    wait_for_snapshot_seq(&core, 2).await?;

    core.remove_query(QUERY_ID).await?;
    core.shutdown().await?;

    let durable = inspect_durable(&paths.rocks).await?;
    assert_eq!(durable.result_sequence.unwrap_or(0), 0);
    assert!(
        durable.outbox_sequences.is_empty(),
        "outbox must be empty after delete, got {:?}",
        durable.outbox_sequences
    );
    assert_eq!(durable.live_row_count, 0);

    let wal_b = paths.wal.parent().unwrap().join("source-wal-b");
    std::fs::create_dir_all(&wal_b)?;
    let (core, source) = build_core(
        &paths,
        CoreOpts {
            query_text: QUERY_TEXT_V1,
            wal: wal_b,
            state: None,
            reaction: None,
        },
    )
    .await?;
    start_running(&core).await?;
    let snapshot = fetch_query(&core).await?.fetch_snapshot().await?;
    assert_eq!(snapshot.as_of_sequence, 0);
    assert!(snapshot.is_empty());
    let (latest, sequences, _) = outbox_sequences(&core).await?;
    assert_eq!(latest, 0);
    assert!(sequences.is_empty());

    insert_person(&source, "p1", "Alice").await?;
    let seq = wait_for_snapshot_seq(&core, 1).await?;
    assert_eq!(seq, 1);
    core.shutdown().await?;
    paths.cleanup();
    Ok(())
}

/// 7. Negative control: stop_query with the same config preserves output.
#[tokio::test]
async fn stop_query_same_config_preserves_output() -> Result<()> {
    let paths = Paths::new("stop-preserve")?;
    let (core, source) = build_core(
        &paths,
        CoreOpts {
            query_text: QUERY_TEXT_V1,
            wal: paths.wal.clone(),
            state: None,
            reaction: None,
        },
    )
    .await?;
    start_running(&core).await?;
    insert_person(&source, "p1", "Alice").await?;
    insert_person(&source, "p2", "Bob").await?;
    wait_for_snapshot_seq(&core, 2).await?;

    core.stop_query(QUERY_ID).await?;
    wait_for_status(&core, QUERY_ID, ComponentStatus::Stopped).await?;
    core.start_query(QUERY_ID).await?;
    wait_for_status(&core, QUERY_ID, ComponentStatus::Running).await?;

    let snapshot = fetch_query(&core).await?.fetch_snapshot().await?;
    assert_eq!(
        snapshot.as_of_sequence, 2,
        "stop/start must not wipe sequence"
    );
    assert_eq!(snapshot.len(), 2);
    let (latest, sequences, _) = outbox_sequences(&core).await?;
    assert_eq!(latest, 2);
    assert_eq!(sequences, vec![1, 2]);

    insert_person(&source, "p3", "Carol").await?;
    let seq = wait_for_snapshot_seq(&core, 3).await?;
    assert_eq!(
        seq, 3,
        "next emission after stop/start must continue at N+1"
    );
    core.shutdown().await?;
    paths.cleanup();
    Ok(())
}

async fn seed_reaction_then_update_query(
    paths: &Paths,
    policy: ReactionRecoveryPolicy,
    snapshot_on_fresh: bool,
) -> Result<(
    DrasiLib,
    ApplicationSourceHandle,
    RecordingReceiver,
    Arc<RedbStateStoreProvider>,
    u64,
)> {
    let store = Arc::new(RedbStateStoreProvider::new(&paths.state)?);
    let (reaction, mut receiver) = recording_reaction(policy, snapshot_on_fresh, false);
    let (core, source) = build_core(
        paths,
        CoreOpts {
            query_text: QUERY_TEXT_V1,
            wal: paths.wal.clone(),
            state: Some(store.clone()),
            reaction: Some(reaction),
        },
    )
    .await?;
    start_running(&core).await?;
    core.start_reaction(REACTION_ID).await?;
    wait_for_status(&core, REACTION_ID, ComponentStatus::Running).await?;

    insert_person(&source, "p1", "Alice").await?;
    insert_person(&source, "p2", "Bob").await?;
    insert_person(&source, "p3", "Carol").await?;
    wait_for_snapshot_seq(&core, 3).await?;
    let initial = receiver.wait_for_count(3, Duration::from_secs(5)).await;
    assert_eq!(
        initial.len(),
        3,
        "reaction should see the original 3 results"
    );

    let hash_v1 = fetch_query(&core)
        .await?
        .fetch_snapshot()
        .await?
        .config_hash;
    stop_reaction_and_wait(&core).await?;

    let checkpoint = read_reaction_checkpoint(store.as_ref())
        .await?
        .context("reaction checkpoint after initial run")?;
    assert_eq!(checkpoint.sequence, 3);
    assert_eq!(checkpoint.config_hash, hash_v1);

    core.update_query(QUERY_ID, query_config(QUERY_TEXT_V2))
        .await?;
    wait_for_status(&core, QUERY_ID, ComponentStatus::Running).await?;
    Ok((core, source, receiver, store, hash_v1))
}

/// 4. Strict + hash mismatch: reaction start fails; no new-hash side effects.
#[tokio::test]
async fn reaction_strict_hash_mismatch_fails_start() -> Result<()> {
    let paths = Paths::new("strict-mismatch")?;
    let (core, source, mut receiver, _store, hash_v1) =
        seed_reaction_then_update_query(&paths, ReactionRecoveryPolicy::Strict, true).await?;

    insert_person(&source, "p4", "Dana").await?;
    wait_for_snapshot_seq(&core, 1).await?;
    let hash_v2 = fetch_query(&core)
        .await?
        .fetch_snapshot()
        .await?
        .config_hash;
    assert_ne!(hash_v2, hash_v1);

    let result = core.start_reaction(REACTION_ID).await;
    assert!(
        result.is_err(),
        "Strict policy should fail on config-hash mismatch"
    );
    let msg = format!("{}", result.unwrap_err());
    assert!(
        msg.contains("Strict") || msg.contains("manual"),
        "Error should mention Strict policy: {msg}"
    );
    let extra = receiver.drain_available();
    assert!(
        extra.is_empty(),
        "Strict must not deliver new-hash sequences, got {:?}",
        extra.iter().map(|r| r.sequence).collect::<Vec<_>>()
    );

    core.shutdown().await?;
    paths.cleanup();
    Ok(())
}

/// 5. AutoReset + hash mismatch: bootstrap snapshot of the new query, checkpoint (as_of, H2).
#[tokio::test]
async fn reaction_autoreset_hash_mismatch_bootstraps_new_query() -> Result<()> {
    let paths = Paths::new("autoreset-mismatch")?;
    let (core, source, receiver, store, hash_v1) =
        seed_reaction_then_update_query(&paths, ReactionRecoveryPolicy::AutoReset, true).await?;

    insert_person(&source, "p4", "Dana").await?;
    let as_of = wait_for_snapshot_seq(&core, 1).await?;
    let hash_v2 = fetch_query(&core)
        .await?
        .fetch_snapshot()
        .await?
        .config_hash;
    assert_ne!(hash_v2, hash_v1);

    core.start_reaction(REACTION_ID).await?;
    wait_for_status(&core, REACTION_ID, ComponentStatus::Running).await?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while receiver.bootstrap_count() == 0 && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        receiver.bootstrap_count() > 0,
        "AutoReset must fetch a snapshot / wipe the downstream view"
    );
    assert_eq!(receiver.bootstrap_as_of(), Some(as_of));

    let checkpoint = read_reaction_checkpoint(store.as_ref())
        .await?
        .context("reaction checkpoint after AutoReset")?;
    assert_eq!(checkpoint.sequence, as_of);
    assert_eq!(checkpoint.config_hash, hash_v2);

    core.shutdown().await?;
    paths.cleanup();
    Ok(())
}

/// 6. AutoSkipGap + hash mismatch: jump to new head, no historical replay, live events after.
#[tokio::test]
async fn reaction_autoskipgap_hash_mismatch_jumps_to_head() -> Result<()> {
    let paths = Paths::new("autoskip-mismatch")?;
    let (core, source, mut receiver, store, hash_v1) =
        seed_reaction_then_update_query(&paths, ReactionRecoveryPolicy::AutoSkipGap, false).await?;

    insert_person(&source, "p4", "Dana").await?;
    insert_person(&source, "p5", "Eve").await?;
    let head = wait_for_snapshot_seq(&core, 2).await?;
    assert_eq!(head, 2);
    let hash_v2 = fetch_query(&core)
        .await?
        .fetch_snapshot()
        .await?
        .config_hash;
    assert_ne!(hash_v2, hash_v1);

    core.start_reaction(REACTION_ID).await?;
    wait_for_status(&core, REACTION_ID, ComponentStatus::Running).await?;

    let extra = receiver.drain_available();
    assert!(
        extra.is_empty(),
        "AutoSkipGap must not replay historical new-hash results, got {:?}",
        extra.iter().map(|r| r.sequence).collect::<Vec<_>>()
    );
    let checkpoint = read_reaction_checkpoint(store.as_ref())
        .await?
        .context("reaction checkpoint after AutoSkipGap")?;
    assert_eq!(checkpoint.sequence, head);
    assert_eq!(checkpoint.config_hash, hash_v2);

    insert_person(&source, "p6", "Fay").await?;
    let live = receiver.wait_for_count(1, Duration::from_secs(5)).await;
    assert_eq!(
        live.len(),
        1,
        "live events after the jump must be delivered"
    );
    assert_eq!(live[0].sequence, head + 1);

    core.shutdown().await?;
    paths.cleanup();
    Ok(())
}
