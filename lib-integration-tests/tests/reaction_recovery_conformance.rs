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

//! Full-process reaction recovery conformance.
//!
//! Unlike the in-process suite in `lib/tests/reaction_recovery_e2e.rs`, this
//! test re-executes the current binary as a child (`DRASI_RECOVERY_CONFORMANCE_PHASE`
//! = `seed` then `recover`) so RocksDB/redb state is reconstructed after a real
//! process exit. `run_phase` / `seed_phase` / `recover_phase` implement that
//! handshake.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_index_rocksdb::RocksDbIndexProvider;
use drasi_lib::channels::{ComponentStatus, QueryResult, ResultDiff};
use drasi_lib::reactions::BootstrapContext;
use drasi_lib::{
    CapacityPolicy, DrasiLib, DurabilityConfig, Reaction, ReactionBase, ReactionBaseParams,
    ReactionCheckpoint, ReactionRecoveryPolicy, ReactionRuntimeContext, StateStoreProvider,
    StorageBackendRef,
};
use drasi_source_application::{
    ApplicationSource, ApplicationSourceConfig, ApplicationSourceHandle, PropertyMapBuilder,
};
use drasi_state_store_redb::RedbStateStoreProvider;
use drasi_wal_redb::RedbWalProvider;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const PHASE_ENV: &str = "DRASI_RECOVERY_CONFORMANCE_PHASE";
const ROCKS_PATH_ENV: &str = "DRASI_RECOVERY_CONFORMANCE_ROCKS_PATH";
const STATE_PATH_ENV: &str = "DRASI_RECOVERY_CONFORMANCE_STATE_PATH";
const WAL_PATH_ENV: &str = "DRASI_RECOVERY_CONFORMANCE_WAL_PATH";
const JOURNAL_PATH_ENV: &str = "DRASI_RECOVERY_CONFORMANCE_JOURNAL_PATH";
const READY_PATH_ENV: &str = "DRASI_RECOVERY_CONFORMANCE_READY_PATH";

const SOURCE_ID: &str = "people-source";
const QUERY_ID: &str = "people-query";
const REACTION_ID: &str = "durable-recording-reaction";
const QUERY_TEXT: &str =
    "MATCH (p:Person) RETURN p.personId AS id, p.name AS name, p.active AS active";

#[derive(Clone, Debug)]
struct FixturePaths {
    rocks: PathBuf,
    state: PathBuf,
    wal: PathBuf,
    journal: PathBuf,
    ready: PathBuf,
}

impl FixturePaths {
    fn under(root: &Path) -> Self {
        Self {
            rocks: root.join("rocksdb"),
            state: root.join("reaction-state.redb"),
            wal: root.join("source-wal"),
            journal: root.join("reaction-journal.jsonl"),
            ready: root.join("seed-ready.json"),
        }
    }

    fn from_env() -> Result<Self> {
        Ok(Self {
            rocks: required_path_env(ROCKS_PATH_ENV)?,
            state: required_path_env(STATE_PATH_ENV)?,
            wal: required_path_env(WAL_PATH_ENV)?,
            journal: required_path_env(JOURNAL_PATH_ENV)?,
            ready: required_path_env(READY_PATH_ENV)?,
        })
    }

    fn apply_to(&self, command: &mut Command) {
        command
            .env(ROCKS_PATH_ENV, &self.rocks)
            .env(STATE_PATH_ENV, &self.state)
            .env(WAL_PATH_ENV, &self.wal)
            .env(JOURNAL_PATH_ENV, &self.journal)
            .env(READY_PATH_ENV, &self.ready);
    }
}

fn required_path_env(name: &str) -> Result<PathBuf> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .with_context(|| format!("missing required environment variable {name}"))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum JournalRecord {
    Snapshot {
        query_id: String,
        sequence: u64,
        rows: Vec<serde_json::Value>,
    },
    Result {
        result: QueryResult,
    },
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct SeedReadiness {
    query_sequence: u64,
    reaction_checkpoint: u64,
    outbox_sequences: Vec<u64>,
    journal_sequences: Vec<u64>,
}

#[derive(Clone)]
struct SideEffectJournal {
    path: Arc<PathBuf>,
}

impl SideEffectJournal {
    fn new(path: PathBuf) -> Self {
        Self {
            path: Arc::new(path),
        }
    }

    async fn append(&self, record: &JournalRecord) -> Result<()> {
        let path = self.path.as_ref().clone();
        let mut bytes =
            serde_json::to_vec(record).context("serialize side-effect journal entry")?;
        bytes.push(b'\n');

        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .with_context(|| format!("open side-effect journal {}", path.display()))?;
            file.write_all(&bytes)
                .with_context(|| format!("append side-effect journal {}", path.display()))?;
            file.sync_all()
                .with_context(|| format!("sync side-effect journal {}", path.display()))?;
            Ok(())
        })
        .await
        .context("join side-effect journal writer")??;

        Ok(())
    }
}

struct DurableRecordingReaction {
    base: ReactionBase,
    journal: SideEffectJournal,
    recovery_policy: ReactionRecoveryPolicy,
}

impl DurableRecordingReaction {
    fn new(journal_path: PathBuf, recovery_policy: ReactionRecoveryPolicy) -> Self {
        let params = ReactionBaseParams::new(REACTION_ID, vec![QUERY_ID.to_string()])
            .with_recovery_policy(recovery_policy);
        Self {
            base: ReactionBase::new(params),
            journal: SideEffectJournal::new(journal_path),
            recovery_policy,
        }
    }
}

#[async_trait]
impl Reaction for DurableRecordingReaction {
    fn id(&self) -> &str {
        self.base.get_id()
    }

    fn type_name(&self) -> &str {
        "durable-recording"
    }

    fn properties(&self) -> HashMap<String, serde_json::Value> {
        HashMap::from([(
            "journalPath".to_string(),
            serde_json::Value::String(self.journal.path.to_string_lossy().into_owned()),
        )])
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
                ComponentStatus::Starting,
                Some("Starting durable recording reaction".to_string()),
            )
            .await;

        let mut shutdown = self.base.create_shutdown_channel().await;
        let queue = self.base.priority_queue.clone();
        let base = self.base.clone_shared();
        let journal = self.journal.clone();
        let task = tokio::spawn(async move {
            loop {
                let result = tokio::select! {
                    biased;
                    result = queue.dequeue() => result,
                    _ = &mut shutdown => break,
                };

                let record = JournalRecord::Result {
                    result: result.as_ref().clone(),
                };
                if let Err(error) = journal.append(&record).await {
                    base.set_status(
                        ComponentStatus::Error,
                        Some(format!("Side effect failed: {error:#}")),
                    )
                    .await;
                    break;
                }

                let checkpoint = match base.read_checkpoint(&result.query_id).await {
                    Ok(Some(previous)) => ReactionCheckpoint {
                        sequence: result.sequence,
                        config_hash: previous.config_hash,
                    },
                    Ok(None) => {
                        base.set_status(
                            ComponentStatus::Error,
                            Some(format!(
                                "Missing checkpoint for query '{}' after side effect",
                                result.query_id
                            )),
                        )
                        .await;
                        break;
                    }
                    Err(error) => {
                        base.set_status(
                            ComponentStatus::Error,
                            Some(format!("Checkpoint read failed: {error:#}")),
                        )
                        .await;
                        break;
                    }
                };

                if let Err(error) = base.write_checkpoint(&result.query_id, &checkpoint).await {
                    base.set_status(
                        ComponentStatus::Error,
                        Some(format!("Checkpoint write failed: {error:#}")),
                    )
                    .await;
                    break;
                }
            }
        });
        self.base.set_processing_task(task).await;

        self.base
            .set_status(
                ComponentStatus::Running,
                Some("Durable recording reaction started".to_string()),
            )
            .await;
        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.base.stop_common().await
    }

    async fn status(&self) -> ComponentStatus {
        self.base.get_status().await
    }

    async fn enqueue_query_result(&self, result: QueryResult) -> Result<()> {
        self.base.enqueue_query_result(result).await
    }

    fn is_durable(&self) -> bool {
        true
    }

    fn needs_snapshot_on_fresh_start(&self) -> bool {
        true
    }

    fn default_recovery_policy(&self) -> ReactionRecoveryPolicy {
        self.recovery_policy
    }

    async fn bootstrap(&self, context: BootstrapContext) -> Result<()> {
        let snapshot = context
            .fetch_snapshot()
            .await
            .map_err(|error| anyhow::anyhow!("fetch reaction bootstrap snapshot: {error}"))?;
        let sequence = snapshot.as_of_sequence;
        let config_hash = snapshot.config_hash;
        let rows = snapshot.collect_vec().await;

        self.journal
            .append(&JournalRecord::Snapshot {
                query_id: context.query_id.clone(),
                sequence,
                rows,
            })
            .await?;

        context
            .write_checkpoint(&ReactionCheckpoint {
                sequence,
                config_hash,
            })
            .await?;
        Ok(())
    }
}

struct RecoveryFixture {
    core: DrasiLib,
    source: ApplicationSourceHandle,
    state_store: Arc<RedbStateStoreProvider>,
}

async fn build_fixture(paths: &FixturePaths) -> Result<RecoveryFixture> {
    std::fs::create_dir_all(&paths.rocks)
        .with_context(|| format!("create RocksDB directory {}", paths.rocks.display()))?;
    std::fs::create_dir_all(&paths.wal)
        .with_context(|| format!("create WAL directory {}", paths.wal.display()))?;
    if let Some(parent) = paths.state.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create fixture directory {}", parent.display()))?;
    }

    let source_config = ApplicationSourceConfig {
        properties: HashMap::new(),
        durability: Some(DurabilityConfig {
            enabled: true,
            max_events: 128,
            capacity_policy: CapacityPolicy::RejectIncoming,
        }),
    };
    let (source, source_handle) = ApplicationSource::new(SOURCE_ID, source_config)?;
    let reaction =
        DurableRecordingReaction::new(paths.journal.clone(), ReactionRecoveryPolicy::Strict);
    let rocks = Arc::new(RocksDbIndexProvider::new(&paths.rocks, false, false));
    let state_store = Arc::new(RedbStateStoreProvider::new(&paths.state)?);
    let wal = Arc::new(RedbWalProvider::new(&paths.wal));

    let core = DrasiLib::builder()
        .with_id("reaction-recovery-conformance")
        .with_source(source)
        .with_query(
            drasi_lib::Query::cypher(QUERY_ID)
                .query(QUERY_TEXT)
                .from_source(SOURCE_ID)
                .auto_start(true)
                .enable_bootstrap(false)
                .with_outbox_capacity(32)
                .with_storage_backend(StorageBackendRef::Named("rocks".to_string()))
                .build(),
        )
        .with_reaction(reaction)
        .with_index_provider("rocks", rocks)
        .with_state_store_provider(state_store.clone())
        .with_wal_provider(wal)
        .build()
        .await?;

    Ok(RecoveryFixture {
        core,
        source: source_handle,
        state_store,
    })
}

async fn insert_person(
    source: &ApplicationSourceHandle,
    id: &str,
    name: &str,
    active: bool,
) -> Result<()> {
    let properties = PropertyMapBuilder::new()
        .with_string("personId", id)
        .with_string("name", name)
        .with_bool("active", active)
        .build();
    source
        .send_node_insert(id, vec!["Person"], properties)
        .await
}

async fn update_person(
    source: &ApplicationSourceHandle,
    id: &str,
    name: &str,
    active: bool,
) -> Result<()> {
    let properties = PropertyMapBuilder::new()
        .with_string("personId", id)
        .with_string("name", name)
        .with_bool("active", active)
        .build();
    source
        .send_node_update(id, vec!["Person"], properties)
        .await
}

async fn wait_for_status(
    core: &DrasiLib,
    component_id: &str,
    expected: ComponentStatus,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let graph = core.get_graph().await;
        if graph
            .nodes
            .iter()
            .any(|node| node.id == component_id && node.status == expected)
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("component '{component_id}' did not reach {expected:?}: {graph:?}");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn query_instance(core: &DrasiLib) -> Result<Arc<dyn drasi_lib::queries::Query>> {
    core.query_manager()
        .get_query_instance(QUERY_ID)
        .await
        .map_err(anyhow::Error::msg)
}

async fn read_reaction_checkpoint(
    state_store: &dyn StateStoreProvider,
) -> Result<ReactionCheckpoint> {
    let bytes = state_store
        .get(REACTION_ID, &format!("checkpoint:{QUERY_ID}"))
        .await?
        .context("durable reaction checkpoint is missing")?;
    bincode::deserialize(&bytes).context("deserialize durable reaction checkpoint")
}

fn read_journal(path: &Path) -> Result<Vec<JournalRecord>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read side-effect journal {}", path.display()));
        }
    };

    contents
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_str(line).context("parse side-effect journal record"))
        .collect()
}

fn journal_sequences(records: &[JournalRecord]) -> Vec<u64> {
    records
        .iter()
        .filter_map(|record| match record {
            JournalRecord::Result { result } => Some(result.sequence),
            JournalRecord::Snapshot { .. } => None,
        })
        .collect()
}

async fn wait_for_journal_sequences(
    path: &Path,
    expected_count: usize,
) -> Result<Vec<JournalRecord>> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let records = read_journal(path)?;
        if journal_sequences(&records).len() >= expected_count {
            return Ok(records);
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!(
                "timed out waiting for {expected_count} journal records, got {}",
                journal_sequences(&records).len()
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn wait_for_checkpoint_sequence(
    state_store: &dyn StateStoreProvider,
    expected: u64,
) -> Result<ReactionCheckpoint> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        match read_reaction_checkpoint(state_store).await {
            Ok(checkpoint) if checkpoint.sequence >= expected => return Ok(checkpoint),
            Ok(_) | Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            let observed = read_reaction_checkpoint(state_store).await.ok();
            anyhow::bail!(
                "timed out waiting for checkpoint sequence {expected}, last={observed:?}"
            );
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PersonRow {
    id: String,
    name: String,
    active: bool,
}

impl PersonRow {
    fn from_value(value: &serde_json::Value) -> Result<Self> {
        Ok(Self {
            id: value
                .get("id")
                .and_then(serde_json::Value::as_str)
                .context("result row is missing string field 'id'")?
                .to_string(),
            name: value
                .get("name")
                .and_then(serde_json::Value::as_str)
                .context("result row is missing string field 'name'")?
                .to_string(),
            active: value
                .get("active")
                .and_then(serde_json::Value::as_bool)
                .context("result row is missing boolean field 'active'")?,
        })
    }
}

fn sorted_people(values: &[serde_json::Value]) -> Result<Vec<PersonRow>> {
    let mut people = values
        .iter()
        .map(PersonRow::from_value)
        .collect::<Result<Vec<_>>>()?;
    people.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(people)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Emission {
    sequence: u64,
    diff: DiffObservation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DiffObservation {
    Add(PersonRow),
    Delete(PersonRow),
    Update {
        before: PersonRow,
        after: PersonRow,
    },
    Aggregation {
        before: Option<PersonRow>,
        after: PersonRow,
    },
}

fn observe_diff(diff: &ResultDiff) -> Result<Option<DiffObservation>> {
    match diff {
        ResultDiff::Add { data, .. } => PersonRow::from_value(data)
            .map(DiffObservation::Add)
            .map(Some)
            .context("parse Add result"),
        ResultDiff::Delete { data, .. } => PersonRow::from_value(data)
            .map(DiffObservation::Delete)
            .map(Some)
            .context("parse Delete result"),
        ResultDiff::Update { before, after, .. } => Ok(Some(DiffObservation::Update {
            before: PersonRow::from_value(before).context("parse Update before row")?,
            after: PersonRow::from_value(after).context("parse Update after row")?,
        })),
        ResultDiff::Aggregation { before, after, .. } => Ok(Some(DiffObservation::Aggregation {
            before: before
                .as_ref()
                .map(PersonRow::from_value)
                .transpose()
                .context("parse Aggregation before row")?,
            after: PersonRow::from_value(after).context("parse Aggregation after row")?,
        })),
        ResultDiff::Noop => Ok(None),
    }
}

fn result_diffs(result: &QueryResult) -> Result<Vec<DiffObservation>> {
    result
        .results
        .iter()
        .map(observe_diff)
        .collect::<Result<Vec<_>>>()
        .map(|diffs| diffs.into_iter().flatten().collect())
}

fn outbox_emissions(results: &[Arc<QueryResult>]) -> Result<Vec<Emission>> {
    let mut emissions = Vec::new();
    for result in results {
        for diff in result_diffs(result)? {
            emissions.push(Emission {
                sequence: result.sequence,
                diff,
            });
        }
    }
    Ok(emissions)
}

fn journal_emissions(records: &[JournalRecord]) -> Result<Vec<Emission>> {
    let mut emissions = Vec::new();
    for record in records {
        if let JournalRecord::Result { result } = record {
            for diff in result_diffs(result)? {
                emissions.push(Emission {
                    sequence: result.sequence,
                    diff,
                });
            }
        }
    }
    Ok(emissions)
}

#[derive(Debug, Eq, PartialEq)]
struct SnapshotObservation {
    sequence: u64,
    people: Vec<PersonRow>,
}

#[derive(Debug, Eq, PartialEq)]
struct RecoveryObservation {
    checkpoint_before_start: u64,
    restart_snapshot: SnapshotObservation,
    restart_outbox: Vec<Emission>,
    restart_outbox_latest: u64,
    journal_after_restart: Vec<Emission>,
    final_snapshot: SnapshotObservation,
    final_outbox: Vec<Emission>,
    final_outbox_latest: u64,
    final_journal: Vec<Emission>,
    final_checkpoint: u64,
}

fn person(id: &str, name: &str, active: bool) -> PersonRow {
    PersonRow {
        id: id.to_string(),
        name: name.to_string(),
        active,
    }
}

fn add_emission(sequence: u64, id: &str, name: &str, active: bool) -> Emission {
    Emission {
        sequence,
        diff: DiffObservation::Add(person(id, name, active)),
    }
}

fn update_emission(
    sequence: u64,
    id: &str,
    old_name: &str,
    old_active: bool,
    new_name: &str,
    new_active: bool,
) -> Emission {
    Emission {
        sequence,
        diff: DiffObservation::Update {
            before: person(id, old_name, old_active),
            after: person(id, new_name, new_active),
        },
    }
}

async fn observe_snapshot(core: &DrasiLib) -> Result<SnapshotObservation> {
    let snapshot = query_instance(core).await?.fetch_snapshot().await?;
    Ok(SnapshotObservation {
        sequence: snapshot.as_of_sequence,
        people: sorted_people(&snapshot.to_vec())?,
    })
}

async fn wait_for_snapshot_row(
    core: &DrasiLib,
    expected: &PersonRow,
) -> Result<SnapshotObservation> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let observation = observe_snapshot(core).await?;
        if observation.people.contains(expected) {
            return Ok(observation);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(observation);
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

async fn seed_phase(paths: &FixturePaths) -> Result<()> {
    let fixture = build_fixture(paths).await?;
    fixture.core.start().await?;
    wait_for_status(&fixture.core, SOURCE_ID, ComponentStatus::Running).await?;
    wait_for_status(&fixture.core, QUERY_ID, ComponentStatus::Running).await?;
    wait_for_status(&fixture.core, REACTION_ID, ComponentStatus::Running).await?;

    insert_person(&fixture.source, "p1", "Alice", true).await?;
    insert_person(&fixture.source, "p2", "Bob", false).await?;

    let initial_records = wait_for_journal_sequences(&paths.journal, 2).await?;
    assert_eq!(
        journal_emissions(&initial_records)?,
        vec![
            add_emission(1, "p1", "Alice", true),
            add_emission(2, "p2", "Bob", false),
        ],
        "the durable reaction must record the first two side effects in sequence order"
    );

    let initial_checkpoint = wait_for_checkpoint_sequence(fixture.state_store.as_ref(), 2).await?;
    assert_eq!(
        initial_checkpoint.sequence, 2,
        "the reaction checkpoint must follow the synced side-effect journal"
    );
    assert_ne!(
        initial_checkpoint.config_hash, 0,
        "the reaction checkpoint must preserve the query configuration hash"
    );

    fixture.core.stop_reaction(REACTION_ID).await?;
    wait_for_status(&fixture.core, REACTION_ID, ComponentStatus::Stopped).await?;

    insert_person(&fixture.source, "p3", "Carol", true).await?;

    let query = query_instance(&fixture.core).await?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let persisted_snapshot = loop {
        let snapshot = query.fetch_snapshot().await?;
        if snapshot.as_of_sequence == 3 && snapshot.len() == 3 {
            break snapshot;
        }
        if tokio::time::Instant::now() >= deadline {
            break snapshot;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    assert_eq!(
        SnapshotObservation {
            sequence: persisted_snapshot.as_of_sequence,
            people: sorted_people(&persisted_snapshot.to_vec())?,
        },
        SnapshotObservation {
            sequence: 3,
            people: vec![
                person("p1", "Alice", true),
                person("p2", "Bob", false),
                person("p3", "Carol", true),
            ],
        },
        "the query snapshot must include the result produced while the reaction is unavailable"
    );

    let persisted_outbox = query.fetch_outbox(0).await?;
    assert_eq!(
        outbox_emissions(&persisted_outbox.results)?,
        vec![
            add_emission(1, "p1", "Alice", true),
            add_emission(2, "p2", "Bob", false),
            add_emission(3, "p3", "Carol", true),
        ],
        "the query outbox must retain every emission before the crash"
    );
    assert_eq!(persisted_outbox.latest_sequence, 3);

    let stopped_records = read_journal(&paths.journal)?;
    assert_eq!(
        journal_emissions(&stopped_records)?,
        vec![
            add_emission(1, "p1", "Alice", true),
            add_emission(2, "p2", "Bob", false),
        ],
        "the unavailable reaction must not observe p3 before reconstruction"
    );

    fixture.state_store.sync().await?;
    assert!(paths.rocks.join(QUERY_ID).join("CURRENT").exists());
    assert!(paths.wal.join(format!("{SOURCE_ID}.redb")).exists());
    assert!(paths.state.exists());
    assert!(paths.journal.exists());

    let readiness = SeedReadiness {
        query_sequence: persisted_outbox.latest_sequence,
        reaction_checkpoint: initial_checkpoint.sequence,
        outbox_sequences: persisted_outbox
            .results
            .iter()
            .map(|result| result.sequence)
            .collect(),
        journal_sequences: journal_sequences(&stopped_records),
    };
    write_synced_file(&paths.ready, &serde_json::to_vec(&readiness)?)?;

    std::process::exit(0);
}

async fn recover_phase(paths: &FixturePaths) -> Result<()> {
    anyhow::ensure!(
        paths.ready.exists(),
        "seed phase did not publish its durability marker"
    );

    let fixture = build_fixture(paths).await?;
    let checkpoint_before_start = read_reaction_checkpoint(fixture.state_store.as_ref()).await?;
    fixture.core.start().await?;
    wait_for_status(&fixture.core, SOURCE_ID, ComponentStatus::Running).await?;
    wait_for_status(&fixture.core, QUERY_ID, ComponentStatus::Running).await?;
    wait_for_status(&fixture.core, REACTION_ID, ComponentStatus::Running).await?;

    let restart_snapshot = observe_snapshot(&fixture.core).await?;
    let query = query_instance(&fixture.core).await?;
    let restart_outbox_response = query.fetch_outbox(0).await?;
    let restart_outbox = outbox_emissions(&restart_outbox_response.results)?;

    let journal_after_restart = wait_for_journal_sequences(&paths.journal, 3).await?;
    let journal_after_restart = journal_emissions(&journal_after_restart)?;

    update_person(&fixture.source, "p1", "Alicia", false).await?;
    let updated_p1 = person("p1", "Alicia", false);
    let final_snapshot = wait_for_snapshot_row(&fixture.core, &updated_p1).await?;
    let final_outbox_response = query.fetch_outbox(0).await?;
    let final_outbox = outbox_emissions(&final_outbox_response.results)?;
    let final_journal = wait_for_journal_sequences(&paths.journal, 4).await?;
    let final_journal = journal_emissions(&final_journal)?;

    fixture.state_store.sync().await?;
    let final_checkpoint = read_reaction_checkpoint(fixture.state_store.as_ref()).await?;

    let observed = RecoveryObservation {
        checkpoint_before_start: checkpoint_before_start.sequence,
        restart_snapshot,
        restart_outbox,
        restart_outbox_latest: restart_outbox_response.latest_sequence,
        journal_after_restart,
        final_snapshot,
        final_outbox,
        final_outbox_latest: final_outbox_response.latest_sequence,
        final_journal,
        final_checkpoint: final_checkpoint.sequence,
    };
    let expected = RecoveryObservation {
        checkpoint_before_start: 2,
        restart_snapshot: SnapshotObservation {
            sequence: 3,
            people: vec![
                person("p1", "Alice", true),
                person("p2", "Bob", false),
                person("p3", "Carol", true),
            ],
        },
        restart_outbox: vec![
            add_emission(1, "p1", "Alice", true),
            add_emission(2, "p2", "Bob", false),
            add_emission(3, "p3", "Carol", true),
        ],
        restart_outbox_latest: 3,
        journal_after_restart: vec![
            add_emission(1, "p1", "Alice", true),
            add_emission(2, "p2", "Bob", false),
            add_emission(3, "p3", "Carol", true),
        ],
        final_snapshot: SnapshotObservation {
            sequence: 4,
            people: vec![
                person("p1", "Alicia", false),
                person("p2", "Bob", false),
                person("p3", "Carol", true),
            ],
        },
        final_outbox: vec![
            add_emission(1, "p1", "Alice", true),
            add_emission(2, "p2", "Bob", false),
            add_emission(3, "p3", "Carol", true),
            update_emission(4, "p1", "Alice", true, "Alicia", false),
        ],
        final_outbox_latest: 4,
        final_journal: vec![
            add_emission(1, "p1", "Alice", true),
            add_emission(2, "p2", "Bob", false),
            add_emission(3, "p3", "Carol", true),
            update_emission(4, "p1", "Alice", true, "Alicia", false),
        ],
        final_checkpoint: 4,
    };

    fixture.core.shutdown().await?;
    assert_eq!(
        observed, expected,
        "fresh-process reconstruction must restore the complete snapshot and outbox, replay the unavailable result, and continue result sequences monotonically"
    );
    Ok(())
}

fn write_synced_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file =
        File::create(path).with_context(|| format!("create synced file {}", path.display()))?;
    file.write_all(contents)
        .with_context(|| format!("write synced file {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("sync file {}", path.display()))?;
    Ok(())
}

fn read_seed_readiness(path: &Path) -> Result<SeedReadiness> {
    let bytes =
        std::fs::read(path).with_context(|| format!("read seed readiness {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("parse seed readiness {}", path.display()))
}

fn remove_test_root(root: &Path) {
    if let Err(error) = std::fs::remove_dir_all(root) {
        eprintln!("failed to clean up test root {}: {error}", root.display());
    }
}

fn test_root() -> Result<PathBuf> {
    let repository = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("lib-integration-tests must have a repository parent")?
        .to_path_buf();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX epoch")?
        .as_nanos();
    Ok(repository
        .join("target")
        .join("reaction-recovery-conformance")
        .join(format!("{}-{timestamp}", std::process::id())))
}

fn run_phase(phase: &str, paths: &FixturePaths) -> Result<Output> {
    let executable = std::env::current_exe().context("resolve current test executable")?;
    let mut command = Command::new(executable);
    command
        .arg("--exact")
        .arg("reaction_recovery_conformance_phase")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(PHASE_ENV, phase);
    paths.apply_to(&mut command);
    command.output().context("spawn conformance phase child")
}

fn child_failure(phase: &str, output: &Output) -> String {
    format!(
        "{phase} child failed with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Seed durable state in one child process, then recover it in a second child.
#[tokio::test(flavor = "current_thread")]
async fn rocksdb_redb_full_process_reconstruction() -> Result<()> {
    if std::env::var_os(PHASE_ENV).is_some() {
        return Ok(());
    }

    let root = test_root()?;
    std::fs::create_dir_all(&root)
        .with_context(|| format!("create test root {}", root.display()))?;
    let paths = FixturePaths::under(&root);

    let seed = run_phase("seed", &paths)?;
    if !seed.status.success() {
        let message = child_failure("seed", &seed);
        remove_test_root(&root);
        anyhow::bail!(message);
    }
    let readiness = match read_seed_readiness(&paths.ready) {
        Ok(readiness) => readiness,
        Err(error) => {
            remove_test_root(&root);
            return Err(error);
        }
    };
    let expected_readiness = SeedReadiness {
        query_sequence: 3,
        reaction_checkpoint: 2,
        outbox_sequences: vec![1, 2, 3],
        journal_sequences: vec![1, 2],
    };
    if readiness != expected_readiness {
        remove_test_root(&root);
        anyhow::bail!(
            "seed child durability observation did not match: expected {expected_readiness:?}, got {readiness:?}"
        );
    }

    let recover = run_phase("recover", &paths)?;
    let failure = (!recover.status.success()).then(|| child_failure("recover", &recover));
    let cleanup = std::fs::remove_dir_all(&root);
    if let Err(error) = &cleanup {
        eprintln!("failed to clean up test root {}: {error}", root.display());
    }
    if let Some(message) = failure {
        anyhow::bail!(message);
    }
    cleanup.with_context(|| format!("remove test root {}", root.display()))?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn reaction_recovery_conformance_phase() -> Result<()> {
    match std::env::var(PHASE_ENV) {
        Ok(phase) if phase == "seed" => seed_phase(&FixturePaths::from_env()?).await,
        Ok(phase) if phase == "recover" => recover_phase(&FixturePaths::from_env()?).await,
        Ok(other) => anyhow::bail!("unknown conformance phase '{other}'"),
        Err(std::env::VarError::NotPresent) => Ok(()),
        Err(error) => Err(error).context("read conformance phase environment variable"),
    }
}
