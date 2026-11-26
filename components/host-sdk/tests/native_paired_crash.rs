// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! RCA-4 and RCA-6: a process dies around the paired source/provider
//! handover, or inside one upstream transaction, with the native database
//! libraries loaded through the Host SDK.
//!
//! Each case runs a child process that builds the real graph: the loaded
//! source library, its paired bootstrap provider node connected to the
//! source's bootstrap input (the source's coordinator opens the provider),
//! the source's data output connected to a durable reference query
//! (RocksDB indexes and query-owned source progress), and a capture sink that
//! appends every delivered result envelope to a file before acknowledging it.
//! Test-only fault injection exits the child at a deterministic point:
//!
//! - `BeforeIntentCommit` (PostgreSQL): the query is about to stage its
//!   provider's ownership intent. A crash here must leave no replication slot.
//! - `MidSnapshot`: the provider stream has yielded its first page and has
//!   another one ready; the child exits instead of yielding it.
//! - `BeforeCompletion`: every snapshot row was admitted and the stream
//!   closed; the child exits when the query asks the provider for its
//!   completion boundary, before the completion commit.
//! - `AfterCompletion`: the child exits right after the query's completion
//!   commit (the session commit that stages its bootstrap-completed marker,
//!   watermarks and handover state), before any change-stream commit.
//! - `MidTransaction` (RCA-6): after initialization, one upstream
//!   multi-statement transaction of `TRANSACTION` changes is written; the
//!   child exits after the query applied `ADMITTED` of them to its element
//!   index, before the query commits the transaction.
//!
//! For the PostgreSQL and MySQL snapshot cases the child changes the database
//! after the first snapshot page (an update, a delete and an insert), so those
//! changes are only visible through the change stream after the snapshot
//! boundary. After the exit the parent changes the database once more, then
//! restarts the same graph in-process over the same indexes, and checks:
//!
//! - every result row is emitted exactly once: replaying the child's delivered
//!   envelopes and the restarted process's envelopes (an envelope repeated
//!   with the same sequence is an at-least-once redelivery and must be
//!   identical) never adds a present row, never updates or deletes an absent
//!   one, and ends at the database's rows (no gap between the snapshot
//!   boundary and the stream start, nothing lost, nothing invented);
//! - the query never exposes part of an upstream transaction: its changes
//!   arrive in one result envelope, and nothing else carries them;
//! - PostgreSQL's replication slot never confirmed a position past the
//!   query's committed progress: after the exit its `confirmed_flush_lsn` is
//!   at or before the WAL position preceding every uncommitted change.
//!
//! Requires Podman (PostgreSQL, MySQL) and the rebuilt libraries
//! (`make build-test-plugins`). SQLite needs no container; its changes go
//! through the source's own REST API, so it has no concurrent snapshot writes
//! and checks the handover and the stream start after restart. SQL Server
//! needs its documented explicit fixture (`fixtures/native_mssql/README.md`)
//! and is not part of this matrix.

#[allow(dead_code)]
#[path = "fixtures/native_mysql/database.rs"]
mod mysql;

use std::{
    collections::BTreeMap,
    io::Write,
    net::TcpListener as StdListener,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::{
    computation::{
        ComputationIndexProvider, ComputationIndexes, ComputationResource, TransactionDomain,
    },
    interface::{
        CheckpointKey, CheckpointStore, ElementIndex, ElementStream, IndexError, IndexSet,
        OutputPartition, SessionControl, SourceCheckpoint,
    },
    models::{Element, ElementReference},
};
use drasi_host_sdk::computation::{
    self, NativeBootstrapFactory, NativeBootstrapNodeFactory, NativePlugin,
    NativeSourceBootstrapResource,
};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::{computation::v1::*, DrasiLib};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const INSTANCE: &str = "native-paired-crash";
const CASE: &str = "DRASI_PAIRED_CRASH_CASE";
const EVIDENCE: &str = "PAIRED-CRASH-LSN:";
const DEADLINE: Duration = Duration::from_secs(60);
const STREAM: &str = "crash/out";
/// Changes in the RCA-6 upstream transaction, and how many of them the
/// query applies before the process exits.
const TRANSACTION: usize = 4;
const ADMITTED: usize = 2;
/// Filler rows keep the snapshot longer than one provider page.
const FILLER: std::ops::RangeInclusive<i64> = 100..=399;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Kind {
    Postgres,
    MySql,
    Sqlite,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Point {
    BeforeIntentCommit,
    MidSnapshot,
    BeforeCompletion,
    AfterCompletion,
    MidTransaction,
}

impl Point {
    fn exit_code(self) -> i32 {
        match self {
            Self::BeforeIntentCommit => 85,
            Self::MidSnapshot => 81,
            Self::BeforeCompletion => 82,
            Self::AfterCompletion => 83,
            Self::MidTransaction => 84,
        }
    }
}

/// Everything the child needs, passed as JSON in [`CASE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Case {
    kind: Kind,
    point: Point,
    /// The owned Podman container (PostgreSQL, MySQL).
    container: String,
    /// The database port (PostgreSQL, MySQL) or the SQLite REST port.
    port: u16,
    /// The SQLite database file.
    file: PathBuf,
    indexes: PathBuf,
    /// The capture file the running process appends delivered envelopes to.
    capture: PathBuf,
}

fn id(name: &str) -> ComponentId {
    let number = match name {
        "source" => 0x51,
        "provider" => 0x52,
        "query" => 0x53,
        "results" => 0x54,
        "capture" => 0x55,
        other => panic!("unknown fixture component {other}"),
    };
    ComponentId::try_new(format!("00000000-0000-0000-0000-{number:012x}")).expect("fixture UUID")
}
fn component_name(name: &str) -> ComponentName {
    ComponentName::try_new(name).expect("fixture component name")
}
fn endpoint(component: &str, port: &str) -> Endpoint {
    Endpoint::new(id(component), PortId::try_new(port).expect("fixture port"))
}
fn resource(name: &str) -> ResourceId {
    ResourceId::try_new(name).expect("fixture resource")
}

// ---------------------------------------------------------------------------
// Database fixtures
// ---------------------------------------------------------------------------

/// A Podman PostgreSQL server with logical replication, owned by this test.
struct Postgres {
    container: String,
    port: u16,
}

impl Postgres {
    async fn start() -> Result<Self> {
        let output = tokio::process::Command::new("podman")
            .args([
                "run",
                "--detach",
                "--publish",
                "127.0.0.1::5432",
                "--env",
                "POSTGRES_PASSWORD=native-test",
                "--label",
                "drasi.qualification=native-paired-crash",
                "postgres:16-alpine",
                "-c",
                "wal_level=logical",
                "-c",
                "max_replication_slots=16",
                "-c",
                "max_wal_senders=16",
            ])
            .kill_on_drop(true)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "Podman PostgreSQL startup: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let container = String::from_utf8(output.stdout)?.trim().to_owned();
        anyhow::ensure!(
            container.len() == 64 && container.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "invalid owned container ID"
        );
        let mut database = Self { container, port: 0 };
        let output = tokio::process::Command::new("podman")
            .args(["port", &database.container, "5432/tcp"])
            .output()
            .await?;
        anyhow::ensure!(output.status.success(), "PostgreSQL port unavailable");
        database.port = String::from_utf8(output.stdout)?
            .trim()
            .strip_prefix("127.0.0.1:")
            .context("loopback binding")?
            .parse()?;
        // The image restarts the server once after initialization; wait for
        // the final server through TCP.
        tokio::time::timeout(DEADLINE, async {
            loop {
                if sql(Kind::Postgres, &database.container, "SELECT 1")
                    .await
                    .is_ok()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .context("PostgreSQL fixture readiness")?;
        Ok(database)
    }
}

impl Drop for Postgres {
    fn drop(&mut self) {
        // Remove only the container this fixture started.
        match Command::new("podman")
            .args(["rm", "--force", "--volumes", &self.container])
            .output()
        {
            Ok(output) if output.status.success() => {}
            Ok(output) => eprintln!(
                "PostgreSQL fixture cleanup: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => eprintln!("PostgreSQL fixture cleanup: {error}"),
        }
    }
}

enum Database {
    Postgres(Postgres),
    MySql(mysql::Database),
    Sqlite(tempfile::TempDir),
}

impl Database {
    async fn start(kind: Kind) -> Result<Self> {
        Ok(match kind {
            Kind::Postgres => Self::Postgres(Postgres::start().await?),
            Kind::MySql => Self::MySql(mysql::Database::new().await?),
            Kind::Sqlite => Self::Sqlite(tempfile::tempdir()?),
        })
    }
    fn container(&self) -> String {
        match self {
            Self::Postgres(database) => database.container.clone(),
            Self::MySql(database) => database.container.clone(),
            Self::Sqlite(_) => String::new(),
        }
    }
    fn port(&self) -> u16 {
        match self {
            Self::Postgres(database) => database.port,
            Self::MySql(database) => database.port,
            Self::Sqlite(_) => 0,
        }
    }
}

/// Runs `statement` in the owned PostgreSQL or MySQL container.
async fn sql(kind: Kind, container: &str, statement: &str) -> Result<String> {
    let mut command = tokio::process::Command::new("podman");
    command.args(["exec", container]);
    match kind {
        Kind::Postgres => command.args([
            "psql",
            "-h",
            "127.0.0.1",
            "-U",
            "postgres",
            "-d",
            "postgres",
            "-X",
            "-v",
            "ON_ERROR_STOP=1",
            "-At",
            "-c",
            statement,
        ]),
        Kind::MySql => command.args([
            "mysql",
            "-uroot",
            "-pnative-test",
            "--batch",
            "--skip-column-names",
            "test",
            "-e",
            statement,
        ]),
        Kind::Sqlite => anyhow::bail!("SQLite changes go through the source"),
    };
    let output = command.kill_on_drop(true).output().await?;
    anyhow::ensure!(
        output.status.success(),
        "{kind:?} fixture SQL {statement:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

/// The rows each case starts with: three named rows and the fillers.
fn initial_rows() -> BTreeMap<i64, String> {
    let mut rows = BTreeMap::from([
        (1, "a".to_owned()),
        (2, "b".to_owned()),
        (3, "c".to_owned()),
    ]);
    for item in FILLER {
        rows.insert(item, "fill".to_owned());
    }
    rows
}

fn insert_values(rows: &BTreeMap<i64, String>) -> String {
    rows.iter()
        .map(|(id, value)| format!("({id},'{value}')"))
        .collect::<Vec<_>>()
        .join(",")
}

async fn prepare(database: &Database) -> Result<()> {
    let values = insert_values(&initial_rows());
    match database {
        Database::Postgres(postgres) => {
            sql(
                Kind::Postgres,
                &postgres.container,
                &format!(
                    "CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL); \
                     INSERT INTO items VALUES {values}; \
                     CREATE PUBLICATION crash FOR TABLE items WITH (publish = 'insert, update, delete');"
                ),
            )
            .await?;
        }
        Database::MySql(mysql) => {
            sql(
                Kind::MySql,
                &mysql.container,
                &format!(
                    "CREATE TABLE items(id INT PRIMARY KEY, value VARCHAR(80) NOT NULL); \
                     INSERT INTO items VALUES {values};"
                ),
            )
            .await?;
        }
        Database::Sqlite(directory) => {
            rusqlite::Connection::open(directory.path().join("source.db"))?.execute_batch(
                &format!(
                    "CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL); \
                     INSERT INTO items VALUES {values};"
                ),
            )?;
        }
    }
    Ok(())
}

/// The changes made after the first snapshot page (PostgreSQL, MySQL).
const DURING: &str = "UPDATE items SET value='during' WHERE id=1; \
     DELETE FROM items WHERE id=2; INSERT INTO items VALUES(4,'during');";
/// The change made while no process runs (or, for SQLite, after restart).
const AFTER: &str = "UPDATE items SET value='after' WHERE id=3;";
/// The RCA-6 upstream transaction: `TRANSACTION` changes.
const UPSTREAM: &str = "BEGIN; UPDATE items SET value='tx' WHERE id=1; \
     UPDATE items SET value='tx' WHERE id=3; INSERT INTO items VALUES(5,'tx'); \
     DELETE FROM items WHERE id=2; COMMIT;";

fn apply_during(rows: &mut BTreeMap<i64, String>) {
    rows.insert(1, "during".into());
    rows.remove(&2);
    rows.insert(4, "during".into());
}
fn apply_after(rows: &mut BTreeMap<i64, String>) {
    rows.insert(3, "after".into());
}
fn apply_upstream(rows: &mut BTreeMap<i64, String>) {
    rows.insert(1, "tx".into());
    rows.insert(3, "tx".into());
    rows.insert(5, "tx".into());
    rows.remove(&2);
}

/// Writes through the SQLite source's REST API as one source transaction.
async fn sqlite_batch(port: u16, operations: Value) -> Result<()> {
    reqwest::Client::builder()
        .timeout(DEADLINE)
        .build()?
        .post(format!("http://127.0.0.1:{port}/api/batch"))
        .json(&json!({ "operations": operations }))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

/// The current WAL position, printed by the child before it changes the
/// database: no change after it is committed by the query.
async fn print_lsn(case: &Case) -> Result<()> {
    if case.kind == Kind::Postgres {
        let lsn = sql(
            Kind::Postgres,
            &case.container,
            "SELECT pg_current_wal_lsn()",
        )
        .await?;
        let mut stdout = std::io::stdout();
        writeln!(stdout, "\n{EVIDENCE}{lsn}")?;
        stdout.flush()?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Fault injection
// ---------------------------------------------------------------------------

/// The child's exit points, shared by the provider, the indexes and the
/// child's driver.
struct Faults {
    case: Case,
    /// Set when the query stages its bootstrap-completed marker: the session
    /// commit that follows is the completion commit; the process exits after
    /// it.
    exit_after_commit: AtomicBool,
    /// Element writes left before the process exits (0: disarmed); the exit
    /// happens at the write that brings it to zero, before it takes effect.
    writes_until_exit: AtomicUsize,
    /// Whether the changes after the first snapshot page were made.
    during_written: AtomicBool,
}

impl Faults {
    fn exit(&self) -> ! {
        std::process::exit(self.case.point.exit_code())
    }
    fn element_write(&self) {
        let hit =
            self.writes_until_exit
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                    left.checked_sub(1)
                })
                == Ok(1);
        if hit {
            self.exit();
        }
    }
}

/// The paired provider, as the source's coordinator opened it, with the
/// snapshot exit points.
struct FaultProvider {
    inner: Arc<dyn ComputationBootstrapProvider>,
    faults: Arc<Faults>,
}

#[async_trait]
impl ComputationBootstrapProvider for FaultProvider {
    fn freeze_for_retirement(&self) -> Result<Box<dyn BootstrapRetirement>> {
        self.inner.freeze_for_retirement()
    }
    fn validate_query_scope(&self, instance: &str, graph: &str, query: &ComponentId) -> Result<()> {
        self.inner.validate_query_scope(instance, graph, query)
    }
    fn recovery_reader(&self) -> Option<SourceProgressReader> {
        self.inner.recovery_reader()
    }
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    fn has_pending_snapshot(&self) -> Result<bool> {
        self.inner.has_pending_snapshot()
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot().await
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        let mut snapshot = self.inner.snapshot_with_state(state).await?;
        let faults = self.faults.clone();
        snapshot.changes = Box::pin(futures_util::stream::try_unfold(
            (snapshot.changes, faults, 0_usize),
            |(mut stream, faults, yielded)| async move {
                let next = stream.next().await.transpose()?;
                if yielded == 1 && faults.case.point == Point::MidSnapshot {
                    // The first page was admitted and another one is ready.
                    anyhow::ensure!(
                        next.is_some(),
                        "the snapshot fit in one provider page; it cannot be interrupted"
                    );
                    faults.exit();
                }
                if yielded == 0
                    && next.is_some()
                    && matches!(faults.case.kind, Kind::Postgres | Kind::MySql)
                    && faults.case.point != Point::MidTransaction
                {
                    // The snapshot view is established: these changes are
                    // only visible through the change stream.
                    print_lsn(&faults.case).await?;
                    sql(faults.case.kind, &faults.case.container, DURING).await?;
                    faults.during_written.store(true, Ordering::Release);
                }
                Ok(next.map(|input| (input, (stream, faults, yielded + 1))))
            },
        ));
        Ok(snapshot)
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        if self.faults.case.point == Point::BeforeCompletion {
            anyhow::ensure!(
                self.faults.case.kind == Kind::Sqlite
                    || self.faults.during_written.load(Ordering::Acquire),
                "the snapshot closed before the concurrent changes were made"
            );
            self.faults.exit();
        }
        self.inner.complete_snapshot().await
    }
    fn completion_state(&self) -> Result<Option<bytes::Bytes>> {
        self.inner.completion_state()
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

#[async_trait]
impl ResourceCleanup for FaultProvider {
    async fn shutdown(&self) -> Result<()> {
        self.inner.stop().await
    }
}

/// The provider node for the paired bootstrap factory, connected to the
/// source's bootstrap input. Each session resolves the coordinator the source
/// owns, exactly as `NativeBootstrapNodeFactory` does; the child wraps the
/// provider with its exit points.
struct ProviderNode {
    node: NativeBootstrapNodeFactory,
    bootstrap: Arc<NativeBootstrapFactory>,
    faults: Option<Arc<Faults>>,
}

#[async_trait]
impl ComponentFactory for ProviderNode {
    fn descriptor(&self) -> &FactoryDescriptor {
        self.node.descriptor()
    }
    fn describe(&self, context: &ConstructionContext) -> Result<ComponentDescription> {
        self.node.describe(context)
    }
    fn specification_from_config(
        &self,
        id: ComponentId,
        configuration: Value,
    ) -> Result<ComponentSpecification> {
        self.node.specification_from_config(id, configuration)
    }
    fn validate(&self, specification: &ComponentSpecification) -> Result<()> {
        self.node.validate(specification)
    }
    async fn create(
        &self,
        context: ConstructionContext,
    ) -> std::result::Result<ConstructedComponent, ComponentCreationError> {
        let Some(faults) = self.faults.clone() else {
            return self.node.create(context).await;
        };
        self.node
            .validate(&context.specification)
            .map_err(ComponentCreationError::terminal)?;
        let configuration = serde_json::to_value(context.configuration())
            .map_err(ComponentCreationError::terminal)?;
        let scope = drasi_computation_plugin_sdk::Scope {
            instance_id: context.instance_id.to_string(),
            graph_id: context.graph_id.to_string(),
            generation: context.generation.0,
        };
        let bootstrap = self.bootstrap.clone();
        let running = context.output_catalogue().clone();
        let sessions: BootstrapProviderFactory = Arc::new(move |request| {
            let origin = request.origin().clone();
            let source = NativeSourceBootstrapResource::registered(
                &scope.instance_id,
                &scope.graph_id,
                &request.consumer,
                Some(&running),
            )
            .context("source coordinator is not registered")?;
            let owner = source
                .owner_for(&origin)
                .context("source has no progress for the consumer")?;
            let inner = bootstrap.create_paired_query_provider(
                origin,
                configuration.clone(),
                scope.clone(),
                owner,
                source,
                request.labels.clone(),
            )?;
            Ok(Arc::new(FaultProvider {
                inner,
                faults: faults.clone(),
            }) as Arc<dyn ComputationBootstrapProvider>)
        });
        let description = self
            .node
            .describe(&context)
            .map_err(ComponentCreationError::terminal)?;
        let component = BootstrapProviderComponent::with_descriptor(
            context.specification.descriptor.clone(),
            sessions,
        )
        .map_err(ComponentCreationError::terminal)?
        .with_description(description);
        Ok(ConstructedComponent::bootstrap_provider(Box::new(
            component,
        )))
    }
}

/// Exits the process after the completion commit.
struct FaultControl {
    inner: Arc<dyn SessionControl>,
    faults: Arc<Faults>,
}

#[async_trait]
impl SessionControl for FaultControl {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> Result<(), IndexError> {
        self.inner.commit().await?;
        if self.faults.exit_after_commit.load(Ordering::Acquire) {
            self.faults.exit();
        }
        Ok(())
    }
    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

/// Counts the query's element writes of the armed upstream transaction.
struct FaultElements {
    inner: Arc<dyn ElementIndex>,
    faults: Arc<Faults>,
}

#[async_trait]
impl ElementIndex for FaultElements {
    async fn get_element(
        &self,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.inner.get_element(element_ref).await
    }
    async fn set_element(
        &self,
        element: &Element,
        slot_affinity: &Vec<usize>,
    ) -> Result<(), IndexError> {
        self.faults.element_write();
        self.inner.set_element(element, slot_affinity).await
    }
    async fn delete_element(&self, element_ref: &ElementReference) -> Result<(), IndexError> {
        self.faults.element_write();
        self.inner.delete_element(element_ref).await
    }
    async fn get_slot_element_by_ref(
        &self,
        slot: usize,
        element_ref: &ElementReference,
    ) -> Result<Option<Arc<Element>>, IndexError> {
        self.inner.get_slot_element_by_ref(slot, element_ref).await
    }
    async fn get_slot_elements(&self, slot: usize) -> Result<ElementStream, IndexError> {
        self.inner.get_slot_elements(slot).await
    }
    async fn get_slot_elements_by_inbound(
        &self,
        slot: usize,
        inbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.inner
            .get_slot_elements_by_inbound(slot, inbound_ref)
            .await
    }
    async fn get_slot_elements_by_outbound(
        &self,
        slot: usize,
        outbound_ref: &ElementReference,
    ) -> Result<ElementStream, IndexError> {
        self.inner
            .get_slot_elements_by_outbound(slot, outbound_ref)
            .await
    }
    async fn clear(&self) -> Result<(), IndexError> {
        self.inner.clear().await
    }
    async fn set_joins(
        &self,
        joins: &[drasi_core::interface::IndexJoin],
    ) -> Result<(), IndexError> {
        self.inner.set_joins(joins).await
    }
}

/// The query's checkpoints; staging the bootstrap-completed marker arms the
/// exit after the completion commit.
struct FaultCheckpoints {
    inner: Arc<dyn CheckpointStore>,
    faults: Arc<Faults>,
}

/// The query's private bootstrap checkpoint key (sequence 0: started,
/// 1: completed).
const BOOTSTRAP_COMPLETED: &str = "\0computation:query-bootstrap:v1";

#[async_trait]
impl CheckpointStore for FaultCheckpoints {
    fn is_persistent(&self) -> bool {
        self.inner.is_persistent()
    }
    async fn stage_checkpoint(
        &self,
        key: &CheckpointKey,
        sequence: u64,
        source_position: Option<&bytes::Bytes>,
    ) -> Result<(), IndexError> {
        if self.faults.case.point == Point::BeforeIntentCommit
            && matches!(key, CheckpointKey::Metadata(name) if name == drasi_computation_contracts::BOOTSTRAP_STATE_CHECKPOINT)
            && source_position.is_some()
        {
            self.faults.exit();
        }
        if self.faults.case.point == Point::AfterCompletion
            && matches!(key, CheckpointKey::Metadata(name) if name == BOOTSTRAP_COMPLETED)
            // Sequence 0 records the initialization intent; 1 completes it.
            && sequence == 1
        {
            self.faults.exit_after_commit.store(true, Ordering::Release);
        }
        self.inner
            .stage_checkpoint(key, sequence, source_position)
            .await
    }
    async fn read_checkpoint(
        &self,
        key: &CheckpointKey,
    ) -> Result<Option<SourceCheckpoint>, IndexError> {
        self.inner.read_checkpoint(key).await
    }
    async fn read_all_checkpoints(
        &self,
    ) -> Result<std::collections::HashMap<CheckpointKey, SourceCheckpoint>, IndexError> {
        self.inner.read_all_checkpoints().await
    }
    async fn clear_checkpoints(&self) -> Result<(), IndexError> {
        self.inner.clear_checkpoints().await
    }
    async fn stage_clear_checkpoints(&self) -> Result<(), IndexError> {
        self.inner.stage_clear_checkpoints().await
    }
    async fn write_config_hash(&self, hash: u64) -> Result<(), IndexError> {
        self.inner.write_config_hash(hash).await
    }
    async fn read_config_hash(&self) -> Result<Option<u64>, IndexError> {
        self.inner.read_config_hash().await
    }
    async fn stage_result_sequence(
        &self,
        output: &OutputPartition,
        sequence: u64,
    ) -> Result<(), IndexError> {
        self.inner.stage_result_sequence(output, sequence).await
    }
    async fn write_result_sequence(
        &self,
        output: &OutputPartition,
        sequence: u64,
    ) -> Result<(), IndexError> {
        self.inner.write_result_sequence(output, sequence).await
    }
    async fn read_result_sequence(
        &self,
        output: &OutputPartition,
    ) -> Result<Option<u64>, IndexError> {
        self.inner.read_result_sequence(output).await
    }
    async fn write_output_generation(
        &self,
        output: &OutputPartition,
        generation: u64,
    ) -> Result<(), IndexError> {
        self.inner.write_output_generation(output, generation).await
    }
    async fn read_output_generation(
        &self,
        output: &OutputPartition,
    ) -> Result<Option<u64>, IndexError> {
        self.inner.read_output_generation(output).await
    }
}

/// The query's RocksDB indexes with the child's commit and element-write
/// exit points.
struct FaultIndexes {
    inner: Arc<dyn ComputationIndexProvider>,
    faults: Arc<Faults>,
}

#[async_trait]
impl ComputationIndexProvider for FaultIndexes {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &drasi_core_types::ComponentId,
    ) -> Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, component).await?;
        let control: Arc<dyn SessionControl> = Arc::new(FaultControl {
            inner: original.indexes().session_control.clone(),
            faults: self.faults.clone(),
        });
        let domain = TransactionDomain::new(control.clone());
        let set = original.indexes();
        Ok(ComputationIndexes::try_new(
            IndexSet {
                element_index: Arc::new(FaultElements {
                    inner: set.element_index.clone(),
                    faults: self.faults.clone(),
                }),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: control,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                Arc::new(FaultCheckpoints {
                    inner: original
                        .checkpoint_store()
                        .expect("RocksDB checkpoints")
                        .clone(),
                    faults: self.faults.clone(),
                }) as Arc<dyn CheckpointStore>,
                &domain,
            )),
            Some(ComputationResource::participating(
                original.outbox_writer().expect("RocksDB outbox").clone(),
                &domain,
            )),
            Some(ComputationResource::participating(
                original
                    .live_results_writer()
                    .expect("RocksDB live results")
                    .clone(),
                &domain,
            )),
        )
        .expect("wrapped RocksDB indexes")
        .with_cleanup(original.cleanup().expect("RocksDB cleanup").clone())
        .with_durability(original.durability()))
    }
    fn is_volatile(&self) -> bool {
        self.inner.is_volatile()
    }
}

// ---------------------------------------------------------------------------
// Captured results
// ---------------------------------------------------------------------------

/// One delivered result envelope, as the capture file records it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Delivered {
    sequence: u64,
    operations: Vec<Operation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Operation {
    kind: String,
    identity: String,
    row: Option<Value>,
}

/// Appends each delivered envelope to a file before acknowledging it, so a
/// process exit loses nothing that was delivered.
struct Capture {
    descriptor: ComponentDescriptor,
    path: PathBuf,
}

#[async_trait]
impl ComputationComponent for Capture {
    fn description(&self) -> ComponentDescription {
        ComponentDescription::undeclared()
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    drasi_lib::passive_component_lifecycle!();
}

#[async_trait]
impl EnvelopeSink for Capture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        let row = |record: &Record| -> Result<Value> {
            Ok(QueryChangeCodec::row_values_to_json(
                &QueryChangeCodec::decode_row(record)?.values,
            ))
        };
        let operations = input
            .envelope
            .changes()
            .operations()
            .iter()
            .map(|operation| {
                let (kind, row) = match operation {
                    ChangeOperation::Added { after, .. } => ("added", Some(row(after)?)),
                    ChangeOperation::Updated { after, .. } => ("updated", Some(row(after)?)),
                    ChangeOperation::Deleted { before, .. } => {
                        ("deleted", before.as_ref().map(row).transpose()?)
                    }
                };
                Ok(Operation {
                    kind: kind.into(),
                    identity: format!("{:?}", operation.identity()),
                    row,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let delivered = Delivered {
            sequence: input.envelope.system().sequence(),
            operations,
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(file, "{}", serde_json::to_string(&delivered)?)?;
        file.sync_all()?;
        Ok(())
    }
}

fn delivered(path: &Path) -> Result<Vec<Delivered>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    std::fs::read_to_string(path)?
        .lines()
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

/// Replays the delivered envelopes in order over `base`, the rows the
/// completed initialization committed (initial results are query state, not
/// delivered changes), and returns the resulting rows. An envelope delivered
/// again under the same sequence must be identical (at-least-once
/// redelivery) and is applied once.
fn replay(base: &BTreeMap<i64, String>, envelopes: &[Delivered]) -> Result<BTreeMap<i64, String>> {
    let mut seen = BTreeMap::new();
    let mut rows = base.clone();
    let key = |operation: &Operation| -> Result<(i64, Option<String>)> {
        let row = operation
            .row
            .as_ref()
            .with_context(|| format!("result operation without its row: {operation:?}"))?;
        Ok((
            row["id"].as_i64().context("row id")?,
            row["value"].as_str().map(str::to_owned),
        ))
    };
    for envelope in envelopes {
        if let Some(previous) = seen.insert(envelope.sequence, envelope.clone()) {
            anyhow::ensure!(
                previous == *envelope,
                "two different result envelopes share sequence {}",
                envelope.sequence
            );
            continue;
        }
        for operation in &envelope.operations {
            let (id, value) = key(operation)?;
            match operation.kind.as_str() {
                "added" => anyhow::ensure!(
                    rows.insert(id, value.context("added value")?).is_none(),
                    "a result row was added twice: {operation:?}"
                ),
                "updated" => anyhow::ensure!(
                    rows.insert(id, value.context("updated value")?).is_some(),
                    "an absent result row was updated: {operation:?}"
                ),
                _ => anyhow::ensure!(
                    rows.remove(&id).is_some(),
                    "an absent result row was deleted: {operation:?}"
                ),
            }
        }
    }
    Ok(rows)
}

// ---------------------------------------------------------------------------
// The graph
// ---------------------------------------------------------------------------

fn load(name: &str) -> Result<Arc<NativePlugin>> {
    let directory = match std::env::var_os("DRASI_NATIVE_PLUGIN_DIR") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?
            .parent()
            .context("test executable directory")?
            .parent()
            .context("native library directory")?
            .join("plugins"),
    };
    let library = directory.join(format!(
        "{}drasi_computation_{name}{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ));
    anyhow::ensure!(
        library.is_file(),
        "missing {}; first run make build-test-plugins",
        library.display()
    );
    computation::load(library)
}

struct Settings(Value);
#[async_trait]
impl ConfigurationResolver for Settings {
    fn validate_reference(&self, key: &str) -> Result<()> {
        anyhow::ensure!(key == "password", "unknown fixture setting");
        Ok(())
    }
    async fn resolve(&self, key: &str) -> Result<Value> {
        self.validate_reference(key)?;
        Ok(self.0.clone())
    }
}

/// The source and paired provider implementations and the source's flat
/// configuration (without its password).
fn source_configuration(case: &Case) -> (&'static str, &'static str, Value) {
    match case.kind {
        Kind::Postgres => (
            drasi_computation_source_postgres::POSTGRES_SOURCE,
            drasi_computation_bootstrap_postgres::POSTGRES_BOOTSTRAP,
            json!({
                "host":"127.0.0.1","port":case.port,"database":"postgres","user":"postgres",
                "sslMode":"disable","tables":["items"],"slotOwnership":"managed",
                "publication":"crash","tableKeys":[{"table":"items","keyColumns":["id"]}],
                "maxTransactionChanges":64,"maxTransactionBytes":1048576,
                "maxTransactionDurationMs":10000,"maxProtocolBytes":1048576,"ioTimeoutMs":10000,
                "feedbackIntervalMs":100,"stream":STREAM,"coordinatedSnapshot":true,
            }),
        ),
        Kind::MySql => (
            drasi_computation_source_mysql::MYSQL_TRANSACTIONS,
            drasi_computation_bootstrap_mysql::MYSQL_BOOTSTRAP,
            json!({
                "host":"127.0.0.1","port":case.port,"database":"test","user":"root",
                "sslMode":"disabled","serverId":812,"heartbeatIntervalMs":100,
                "tables":["items"],"startFrom":"snapshot","snapshotLockTimeoutMs":5000,
                "retention":"untilProcessed","maxTransactionChanges":64,
                "maxTransactionBytes":1048576,"maxTransactionDurationMs":10000,
                "maxProtocolBytes":1048576,"ioTimeoutMs":10000,"stream":STREAM,
            }),
        ),
        Kind::Sqlite => (
            drasi_computation_source_sqlite::SQLITE_REPLAY,
            drasi_computation_bootstrap_sqlite::COORDINATED_BOOTSTRAP,
            json!({
                "stream":STREAM,"path":case.file,"tables":["items"],"tableKeys":[],
                "maxTransactionChanges":64,"maxTransactionBytes":1048576,
                "maxTransactionDurationMs":5000,"maxSqlBytes":65536,"commandCapacity":8,
                "outputCapacity":1,"shutdownTimeoutMs":5000,"pageSize":16,
                "maxJournalTransactions":16,"maxJournalBytes":8388608,
                "coordinatedSnapshot":true,"restApi":true,"restPort":case.port,
            }),
        ),
    }
}

fn provide(
    builder: ComponentBatchBuilder,
    name: &str,
    role: ResourceRole,
    ownership: ResourceOwnership,
    handle: ResourceHandle,
) -> Result<ComponentBatchBuilder> {
    Ok(builder
        .declare_resource(ResourceSpecification {
            id: resource(name),
            role,
            ownership,
            binding: name.into(),
        })?
        .provide_resource(resource(name), handle)?)
}

struct Graph {
    core: DrasiLib,
    owner: Arc<QuerySourceProgress>,
}

impl Graph {
    async fn rows(&self) -> Result<BTreeMap<i64, String>> {
        self.core
            .get_query_results_by_name(&component_name("query"))
            .await?
            .into_iter()
            .map(|row| {
                Ok((
                    row["id"].as_i64().context("row id")?,
                    row["value"].as_str().context("row value")?.to_owned(),
                ))
            })
            .collect()
    }

    async fn wait_rows(&self, expected: &BTreeMap<i64, String>) -> Result<()> {
        tokio::time::timeout(DEADLINE, async {
            loop {
                if self.rows().await? == *expected {
                    return Ok::<_, anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .with_context(|| format!("query results did not converge to {} rows", expected.len()))?
    }
}

async fn graph(case: &Case, faults: Option<Arc<Faults>>) -> Result<Graph> {
    let (source_name, bootstrap_name, configuration) = source_configuration(case);
    let (source_library, bootstrap_library) = match case.kind {
        Kind::Postgres => ("source_postgres", "bootstrap_postgres"),
        Kind::MySql => ("source_mysql", "bootstrap_mysql"),
        Kind::Sqlite => ("source_sqlite", "bootstrap_sqlite"),
    };
    let source_plugin = load(source_library)?;
    let bootstrap_plugin = load(bootstrap_library)?;
    let source_factory = source_plugin
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == source_name)
        .cloned()
        .context("loaded source factory")?;
    let bootstrap = bootstrap_plugin
        .bootstrap_factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == bootstrap_name)
        .cloned()
        .context("loaded paired bootstrap factory")?;

    let owner = Arc::new(QuerySourceProgress::new(
        ComponentBatch::GRAPH_ID,
        id("query"),
    )?);
    let mut builder = provide(
        ComponentBatch::builder(),
        "progress",
        ResourceRole::Checkpoint,
        ResourceOwnership::Borrowed,
        // The query keeps the checkpoints in its RocksDB indexes.
        ResourceHandle::new(
            ResourceRole::Checkpoint,
            Arc::new(QuerySourceProgressResource(owner.clone())),
        )
        .with_capabilities(ResourceCapabilities::DURABLE),
    )?;
    let mut source = if case.kind == Kind::Sqlite {
        source_factory.specification(id("source"), configuration)?
    } else {
        builder = provide(
            builder,
            "settings",
            ResourceRole::SecretStore,
            ResourceOwnership::Borrowed,
            ResourceHandle::new(
                ResourceRole::SecretStore,
                Arc::new(ConfigurationResolverResource(Arc::new(Settings(json!(
                    "native-test"
                ))))),
            ),
        )?;
        let mut source = source_factory.specification(id("source"), configuration)?;
        source.configuration.insert(
            "password".into(),
            ConfigurationValue::Reference {
                resource: resource("settings"),
                key: "password".into(),
                secret: true,
            },
        );
        source
    };
    source.dependencies = BTreeMap::from([("source_progress".into(), vec![resource("progress")])]);

    let provider = Arc::new(ProviderNode {
        node: NativeBootstrapNodeFactory::new(bootstrap.clone()),
        bootstrap,
        faults: faults.clone(),
    });
    let provider_specification = provider.specification_from_config(id("provider"), json!({}))?;

    let rocks: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
        &case.indexes,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    ));
    let indexes: Arc<dyn ComputationIndexProvider> = match &faults {
        Some(faults) => Arc::new(FaultIndexes {
            inner: rocks,
            faults: faults.clone(),
        }),
        None => rocks,
    };
    builder = provide(
        builder,
        "indexes",
        ResourceRole::IndexBackend,
        ResourceOwnership::Borrowed,
        QueryIndexProviderResource::handle(ResourceRole::IndexBackend, indexes),
    )?;
    builder = provide(
        builder,
        "catalog",
        ResourceRole::QueryCatalog,
        ResourceOwnership::Graph,
        ResourceHandle::new(
            ResourceRole::QueryCatalog,
            Arc::new(QueryResultsCatalog::new(ComponentBatch::GRAPH_ID)?),
        ),
    )?;
    let factories = FactoryRegistry::standard();
    let query_factory = factories
        .get(
            &ContinuousQueryFactory::default()
                .descriptor()
                .implementation,
        )
        .context("reference query factory")?
        .clone();
    let outlet_factory = factories
        .get(
            &QueryResultsOutletFactory::default()
                .descriptor()
                .implementation,
        )
        .context("query results outlet factory")?
        .clone();
    let mut query = query_factory.specification_from_config(
        id("query"),
        json!({
            "query":"MATCH (n:items) RETURN n.id AS id, n.value AS value",
            "stream":"query/out","outbox_capacity":32,
            "execution":QueryExecutionSettings {
                source_transactions: Some(serde_json::from_value(json!({
                    "max_changes":64,"max_bytes":1048576,"max_duration_ms":10000
                }))?),
                ..Default::default()
            },
            // A partial initialization needs a reset; `strict` reports it
            // to the operator, `auto_reset` re-initializes by itself.
            "recovery":"auto_reset",
        }),
    )?;
    query.dependencies = BTreeMap::from([
        ("indexes".into(), vec![resource("indexes")]),
        ("catalog".into(), vec![resource("catalog")]),
        ("source_progress".into(), vec![resource("progress")]),
    ]);
    let results = ComponentSpecification {
        descriptor: QueryResultsOutlet::new(
            id("results"),
            QueryResultsCatalog::new(ComponentBatch::GRAPH_ID)?,
        )
        .descriptor()
        .clone(),
        role: ComponentRole::Sink,
        completion: Some(SinkCompletion::Handled),
        implementation: outlet_factory.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration: BTreeMap::new(),
        dependencies: BTreeMap::from([("catalog".into(), vec![resource("catalog")])]),
    };
    let capture = Capture {
        descriptor: ComponentDescriptor::try_new(
            id("capture"),
            vec![PortDescriptor::new(
                PortId::try_new("in")?,
                PortDirection::Input,
                QueryChangeCodec::schema().descriptor().clone(),
                PipeRequirements::default(),
            )],
        )?,
        path: case.capture.clone(),
    };
    let pipe = || Box::new(BoundedPipeConfig { capacity: 8 });
    let batch = builder
        .component(component_name("provider"), provider_specification, provider)
        .component(component_name("source"), source, source_factory)
        .component(component_name("query"), query, query_factory)
        .component(component_name("results"), results, outlet_factory)
        .sink(component_name("capture"), Box::new(capture))
        .bind_stream(endpoint("source", "out"), StreamId::try_new(STREAM)?)
        .bind_stream(endpoint("query", "out"), StreamId::try_new("query/out")?)
        .connect(
            EdgeDefinition::new(
                endpoint("provider", "snapshot"),
                endpoint("source", "bootstrap"),
            ),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(endpoint("query", "out"), endpoint("results", "in")),
            pipe(),
        )
        .connect(
            EdgeDefinition::new(endpoint("query", "out"), endpoint("capture", "in")),
            pipe(),
        )
        .build()?;
    let core = DrasiLib::builder()
        .with_allow_external_component_ids(true)
        .with_query_factory(query_factory_for_builder()?)
        .with_id(INSTANCE)
        .with_components(batch)
        .build()
        .await?;
    Ok(Graph { core, owner })
}

fn query_factory_for_builder() -> Result<Arc<dyn ComponentFactory>> {
    FactoryRegistry::standard()
        .get(&ImplementationIdentity::try_new(
            "drasi/continuous-query",
            "1",
        )?)
        .cloned()
        .context("reference query factory")
}

async fn start(graph: &Graph) -> Result<()> {
    let inspector = graph.core.inspect_computation_graph()?;
    tokio::time::timeout(DEADLINE, graph.core.start())
        .await
        .with_context(|| {
            format!(
                "graph startup did not settle: {:?}",
                inspector.snapshot().observed
            )
        })??;
    Ok(())
}

// ---------------------------------------------------------------------------
// Child and parents
// ---------------------------------------------------------------------------

/// The child: run the case's graph and die at its exit point.
///
/// Launched only by the parents below with [`CASE`] set; without it (for
/// example under a blanket `--ignored` run) it has nothing to do.
#[tokio::test(flavor = "current_thread")]
#[ignore = "process-exit child launched by the native_paired_crash parents"]
async fn paired_crash_child() -> Result<()> {
    let Some(case) = std::env::var_os(CASE) else {
        return Ok(());
    };
    let case: Case = serde_json::from_str(case.to_str().context("case encoding")?)?;
    let faults = Arc::new(Faults {
        case: case.clone(),
        exit_after_commit: AtomicBool::new(false),
        writes_until_exit: AtomicUsize::new(0),
        during_written: AtomicBool::new(false),
    });
    let graph = graph(&case, Some(faults.clone())).await?;
    let started = start(&graph).await;
    if case.point != Point::MidTransaction {
        anyhow::bail!(
            "the child passed its {:?} exit point: {started:?}",
            case.point
        );
    }
    started?;
    graph.wait_rows(&initial_rows()).await?;
    anyhow::ensure!(graph.owner.snapshot().bootstrap_complete);
    print_lsn(&case).await?;
    // Exit at the write after the first `ADMITTED` of the transaction.
    faults
        .writes_until_exit
        .store(ADMITTED + 1, Ordering::Release);
    match case.kind {
        Kind::Sqlite => {
            // The source commits and emits the transaction; its REST reply
            // may never arrive because the process exits.
            let _ = tokio::time::timeout(DEADLINE, sqlite_batch(case.port, upstream_operations()))
                .await;
            tokio::time::sleep(DEADLINE).await;
        }
        kind => {
            sql(kind, &case.container, UPSTREAM).await?;
            tokio::time::sleep(DEADLINE).await;
        }
    }
    anyhow::bail!("the query committed the transaction without reaching its exit point")
}

fn upstream_operations() -> Value {
    json!([
        {"op":"update","table":"items","id":"1","data":{"value":"tx"}},
        {"op":"update","table":"items","id":"3","data":{"value":"tx"}},
        {"op":"insert","table":"items","data":{"id":5,"value":"tx"}},
        {"op":"delete","table":"items","id":"2"}
    ])
}

/// Runs the child to its exit point; returns the WAL position it printed.
async fn crash(case: &Case) -> Result<Option<String>> {
    let child = tokio::time::timeout(
        Duration::from_secs(180),
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "paired_crash_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CASE, serde_json::to_string(case)?)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let stdout = String::from_utf8_lossy(&child.stdout);
    anyhow::ensure!(
        child.status.code() == Some(case.point.exit_code()),
        "crash child for {case:?} ended with {}:\n{stdout}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stderr)
    );
    Ok(stdout
        .lines()
        .find_map(|line| line.strip_prefix(EVIDENCE))
        .map(str::to_owned))
}

async fn qualify(kind: Kind, point: Point) -> Result<()> {
    let database = Database::start(kind).await?;
    prepare(&database).await?;
    let directory = tempfile::tempdir()?;
    let file = match &database {
        Database::Sqlite(directory) => directory.path().join("source.db"),
        _ => PathBuf::new(),
    };
    let port = match kind {
        Kind::Sqlite => StdListener::bind("127.0.0.1:0")?.local_addr()?.port(),
        _ => database.port(),
    };
    let mut case = Case {
        kind,
        point,
        container: database.container(),
        port,
        file,
        indexes: directory.path().join("indexes"),
        capture: directory.path().join("child.jsonl"),
    };
    let lsn = crash(&case).await?;
    let mut expected = initial_rows();
    if point == Point::MidTransaction {
        apply_upstream(&mut expected);
    } else if kind != Kind::Sqlite && point != Point::BeforeIntentCommit {
        apply_during(&mut expected);
    }

    // The slot never confirmed anything the query did not commit.
    if point == Point::BeforeIntentCommit {
        anyhow::ensure!(kind == Kind::Postgres);
        let slots = sql(
            kind,
            &case.container,
            "SELECT count(*) FROM pg_replication_slots",
        )
        .await?;
        anyhow::ensure!(
            slots == "0",
            "uncommitted provider intent left {slots} external slots"
        );
    } else if kind == Kind::Postgres {
        let lsn = lsn.context("the child did not report its WAL position")?;
        let slots = sql(
            kind,
            &case.container,
            &format!(
                "SELECT count(*) || ':' || coalesce(bool_and(confirmed_flush_lsn IS NULL \
                 OR confirmed_flush_lsn <= '{lsn}'::pg_lsn), true) FROM pg_replication_slots"
            ),
        )
        .await?;
        anyhow::ensure!(
            slots.ends_with(":true"),
            "a replication slot confirmed a position past the query's committed progress \
             ({slots}, uncommitted changes start after {lsn})"
        );
        anyhow::ensure!(
            point == Point::MidSnapshot || slots.starts_with("1:"),
            "the handed-over slot is missing: {slots}"
        );
    }
    if kind != Kind::Sqlite && point != Point::MidTransaction {
        sql(kind, &case.container, AFTER).await?;
        apply_after(&mut expected);
    }

    // Restart: the same graph, indexes and progress in a new process.
    let child_delivered = delivered(&case.capture)?;
    case.capture = directory.path().join("restarted.jsonl");
    let restarted = graph(&case, None).await?;
    let proof = async {
        start(&restarted).await?;
        if kind == Kind::Sqlite && point != Point::MidTransaction {
            sqlite_batch(
                case.port,
                json!([{"op":"update","table":"items","id":"3","data":{"value":"after"}}]),
            )
            .await?;
            apply_after(&mut expected);
        }
        restarted.wait_rows(&expected).await?;
        let state = restarted.owner.snapshot();
        anyhow::ensure!(state.persistent && state.bootstrap_complete);
        // Only an initialization that never completed is redone.
        let redone = matches!(
            point,
            Point::BeforeIntentCommit | Point::MidSnapshot | Point::BeforeCompletion
        );
        anyhow::ensure!(
            state.reset_generation == u64::from(redone),
            "reset generation {} after {point:?}",
            state.reset_generation
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let stopped = restarted.core.shutdown().await;
    proof?;
    stopped?;

    let restarted_delivered = delivered(&case.capture)?;
    let all: Vec<_> = child_delivered
        .iter()
        .chain(&restarted_delivered)
        .cloned()
        .collect();
    let summary: Vec<_> = all
        .iter()
        .map(|envelope| (envelope.sequence, envelope.operations.len()))
        .collect();
    eprintln!(
        "delivered (sequence, operations): child {} envelopes, all {summary:?}",
        child_delivered.len()
    );
    // The completed initialization's rows: the child's snapshot, or (after
    // an exit before completion) the restarted process's re-initialization,
    // which reads the database as it is then.
    let base = match point {
        Point::AfterCompletion | Point::MidTransaction => initial_rows(),
        Point::BeforeIntentCommit | Point::MidSnapshot | Point::BeforeCompletion => {
            let mut base = initial_rows();
            if kind != Kind::Sqlite {
                if point != Point::BeforeIntentCommit {
                    apply_during(&mut base);
                }
                apply_after(&mut base);
            }
            base
        }
    };
    assert_eq!(
        replay(&base, &all)?,
        expected,
        "the delivered results do not emit every row exactly once"
    );
    if point == Point::MidTransaction {
        // One upstream transaction, one result envelope, all of it.
        let carrying: Vec<_> = all
            .iter()
            .filter(|envelope| {
                envelope.operations.iter().any(|operation| {
                    operation.kind == "deleted"
                        && operation.row.as_ref().is_some_and(|row| row["id"] == 2)
                        || operation
                            .row
                            .as_ref()
                            .is_some_and(|row| row["value"] == "tx")
                })
            })
            .collect();
        anyhow::ensure!(
            !carrying.is_empty()
                && carrying
                    .iter()
                    .all(|envelope| envelope.sequence == carrying[0].sequence),
            "the transaction's changes are spread over result envelopes: {carrying:?}"
        );
        anyhow::ensure!(
            carrying[0].operations.len() == TRANSACTION,
            "a partial transaction was exposed: {:?}",
            carrying[0]
        );
        anyhow::ensure!(
            child_delivered
                .iter()
                .all(|envelope| !carrying.contains(&envelope)),
            "the exited process delivered part of the uncommitted transaction"
        );
    }
    Ok(())
}

async fn matrix(kind: Kind, points: &[Point]) -> Result<()> {
    for point in points {
        qualify(kind, *point)
            .await
            .with_context(|| format!("{kind:?} {point:?}"))?;
    }
    Ok(())
}

const HANDOVER: [Point; 3] = [
    Point::MidSnapshot,
    Point::BeforeCompletion,
    Point::AfterCompletion,
];

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and the rebuilt native PostgreSQL libraries"]
async fn native_paired_crash_postgres_before_intent_commit() -> Result<()> {
    qualify(Kind::Postgres, Point::BeforeIntentCommit).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and the rebuilt native PostgreSQL libraries"]
async fn native_paired_crash_postgres_handover() -> Result<()> {
    matrix(Kind::Postgres, &HANDOVER).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and the rebuilt native MySQL libraries"]
async fn native_paired_crash_mysql_handover() -> Result<()> {
    matrix(Kind::MySql, &HANDOVER).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires the rebuilt native SQLite libraries"]
async fn native_paired_crash_sqlite_handover() -> Result<()> {
    matrix(Kind::Sqlite, &HANDOVER).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and the rebuilt native PostgreSQL libraries"]
async fn native_paired_crash_postgres_transaction() -> Result<()> {
    matrix(Kind::Postgres, &[Point::MidTransaction]).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and the rebuilt native MySQL libraries"]
async fn native_paired_crash_mysql_transaction() -> Result<()> {
    matrix(Kind::MySql, &[Point::MidTransaction]).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires the rebuilt native SQLite libraries"]
async fn native_paired_crash_sqlite_transaction() -> Result<()> {
    matrix(Kind::Sqlite, &[Point::MidTransaction]).await
}
