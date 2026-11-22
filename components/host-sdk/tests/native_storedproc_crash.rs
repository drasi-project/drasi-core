// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! RCA-5: a process dies between the destination effect and the graph's
//! acknowledgement of a loaded native stored-procedure consumer.
//!
//! A child process loads the real `storedproc-{mysql,postgres}` library and
//! exits at a deterministic boundary while it delivers one three-operation
//! batch; the parent then reopens the same host ledger and redelivers the
//! unacknowledged batch, as the graph does after a restart.
//!
//! Boundaries, per `deliveryGuarantee`:
//! - `exactlyOnce` uses the host `DeliveryRunner`. The child's ledger storage
//!   exits the process (a) instead of committing the first operation's
//!   completion, after the destination committed that operation and its
//!   cursor row, and (b) right after committing the last operation's
//!   completion, before `handle` returns and the input is acknowledged.
//!   The destination then holds exactly the inputs.
//! - `atLeastOnce` has no host ledger: the destination cursor is the only
//!   progress. The child exits (a) after the first operation's destination
//!   commit, when the second operation fails at the destination, and (b)
//!   after the whole batch committed but before acknowledgement. A restarted
//!   in-memory producer redelivers under a new epoch, so the destination holds
//!   the inputs plus at most that one in-flight batch; a persistent producer
//!   keeps its epoch and the cursor removes the repeat.
//!
//! Requires Podman and the rebuilt libraries (`make build-test-plugins`).
//! SQL Server needs its documented explicit fixture
//! (`fixtures/native_mssql/README.md`) and is not part of this matrix.

#[path = "common/hooks.rs"]
mod hooks;
#[path = "fixtures/native_mysql/database.rs"]
mod mysql;
#[path = "fixtures/native_sink_inputs.rs"]
mod sink_inputs;

use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
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
    interface::{FailureMode, IndexError, IndexSet, SessionControl},
};
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};
use drasi_host_sdk::computation::{self, NativeConsumerProxy, NativeFactory};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::computation::v1::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const INSTANCE: &str = "native-storedproc-crash";
const CASE: &str = "DRASI_STOREDPROC_CRASH_CASE";
const PERSISTENT: &str = "0f6b8f39-6a55-4a43-9f3a-6d1c3b2e7a10";
const TEMPLATE: &str =
    "CALL effect({{param output.alias}},{{param operation}},{{param after.id}},{{param after.value}})";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Kind {
    MySql,
    Postgres,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Guarantee {
    ExactlyOnce,
    AtLeastOnce,
}

/// Where the child process dies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Point {
    /// After the destination committed an operation, before the host
    /// recorded its progress.
    AfterDestinationCommit,
    /// After every progress commit, before the upstream acknowledgement.
    BeforeAcknowledgement,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum Producer {
    /// A durable producer: its identity survives the restart.
    Persistent,
    /// An in-memory query: each run is a new incarnation (a new epoch).
    Volatile,
}

/// Everything the child needs, passed as JSON in [`CASE`].
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Case {
    kind: Kind,
    guarantee: Guarantee,
    point: Point,
    producer: Producer,
    /// The sink's configuration (connection, templates, output alias).
    configuration: Value,
    ledger: PathBuf,
    alias: String,
    first_id: i64,
    incarnation: String,
}

const BATCH: i64 = 3;

impl Case {
    fn component(&self) -> ComponentId {
        id(&format!("{}-sink", self.alias))
    }
    fn producer_id(&self) -> ComponentId {
        id(&format!("{}-producer", self.alias))
    }
    fn exit_code(&self) -> i32 {
        match (self.guarantee, self.point) {
            (Guarantee::ExactlyOnce, Point::AfterDestinationCommit) => 83,
            (Guarantee::ExactlyOnce, Point::BeforeAcknowledgement) => 84,
            (Guarantee::AtLeastOnce, Point::AfterDestinationCommit) => 85,
            (Guarantee::AtLeastOnce, Point::BeforeAcknowledgement) => 86,
        }
    }
    /// The batch a producer run emits: `BATCH` inserts at sequence 1.
    fn input(&self) -> Result<InputEnvelope> {
        let changes: Vec<_> = (self.first_id..self.first_id + BATCH)
            .map(|item| SourceChange::Insert {
                element: Element::Node {
                    metadata: ElementMetadata {
                        reference: ElementReference::new("items", &item.to_string()),
                        labels: Arc::from([Arc::from("items")]),
                        effective_from: 1,
                    },
                    properties: ElementPropertyMap::from(json!({"id": item, "value": "crash"})),
                },
            })
            .collect();
        let producer = self.producer_id();
        let stream = StreamId::try_new(format!("{producer}/out"))?;
        let mut envelope = GraphChangeCodec::encode_changes(&changes, stream.clone(), 1, None)?;
        // A volatile producer's identity is not restart-stable: at-least-once
        // delivery derives the cursor epoch from its incarnation.
        let identity: GraphProducerIdentity = serde_json::from_value(json!({
            "construction_scope": INSTANCE, "graph_id": ComponentBatch::GRAPH_ID,
            "component_id": producer.as_str(), "stream": stream,
            "incarnation": self.incarnation,
            "persistent": self.producer == Producer::Persistent
        }))?;
        GraphProducerProgress::annotate(&mut envelope, &identity, 1)?;
        Ok(InputEnvelope {
            execution_context: Default::default(),
            port: PortId::try_new("in")?,
            envelope,
        })
    }
}

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(mysql::fixture_id(value)).expect("fixture id")
}

/// A new UUID for each in-memory producer run.
fn fresh_incarnation() -> String {
    static RUN: AtomicUsize = AtomicUsize::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos();
    let run = RUN.fetch_add(1, Ordering::Relaxed) as u128;
    let bits = (nanos << 16) ^ run ^ (u128::from(std::process::id()) << 96);
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (bits >> 96) as u32,
        (bits >> 80) as u16,
        (bits >> 68) as u16 & 0xfff,
        (bits >> 56) as u16 & 0xfff,
        bits as u64 & 0xffff_ffff_ffff
    )
}

fn scope() -> drasi_computation_plugin_sdk::Scope {
    drasi_computation_plugin_sdk::Scope {
        instance_id: INSTANCE.into(),
        graph_id: ComponentBatch::GRAPH_ID.into(),
        generation: 1,
    }
}

fn rocks(path: &Path) -> Result<Arc<dyn ComputationIndexProvider>> {
    Ok(Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    )))
}

fn factory(kind: Kind) -> Result<Arc<NativeFactory>> {
    let directory = match std::env::var_os("DRASI_NATIVE_PLUGIN_DIR") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?
            .parent()
            .context("test executable directory")?
            .parent()
            .context("native library directory")?
            .join("plugins"),
    };
    let name = match kind {
        Kind::MySql => "sink_storedproc_mysql",
        Kind::Postgres => "sink_storedproc_postgres",
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
    computation::load(library)?
        .factories()
        .iter()
        .find(|factory| factory.consumer_mode().is_some())
        .cloned()
        .context("stored-procedure consumer factory")
}

async fn consumer(
    case: &Case,
    provider: Arc<dyn ComputationIndexProvider>,
) -> Result<NativeConsumerProxy> {
    let mut consumer = factory(case.kind)?
        .create_consumer_with_inputs(
            case.component(),
            case.configuration.clone(),
            scope(),
            provider,
            DeliveryOptions {
                scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
                max_streams: NonZeroUsize::new(4).expect("streams"),
                receipts_per_stream: NonZeroUsize::new(4).expect("receipts"),
            },
            sink_inputs::inputs_for(&case.configuration),
        )
        .await?;
    consumer.initialize(&hooks::initialize_ctx()).await?;
    consumer.start(&hooks::start_ctx()).await?;
    Ok(consumer)
}

/// Exits the process at the `at`-th ledger commit after it is armed,
/// before or after the commit takes effect.
struct ExitAt {
    remaining: AtomicUsize,
    after: bool,
    code: i32,
}

struct ExitControl {
    inner: Arc<dyn SessionControl>,
    exit: Arc<ExitAt>,
}

#[async_trait]
impl SessionControl for ExitControl {
    async fn begin(&self) -> Result<(), IndexError> {
        self.inner.begin().await
    }
    async fn commit(&self) -> Result<(), IndexError> {
        let hit =
            self.exit
                .remaining
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                    remaining.checked_sub(1)
                })
                == Ok(1);
        if hit && !self.exit.after {
            std::process::exit(self.exit.code);
        }
        self.inner.commit().await?;
        if hit {
            std::process::exit(self.exit.code);
        }
        Ok(())
    }
    fn rollback(&self) -> Result<(), IndexError> {
        self.inner.rollback()
    }
}

struct ExitProvider {
    inner: Arc<dyn ComputationIndexProvider>,
    exit: Arc<ExitAt>,
}

#[async_trait]
impl ComputationIndexProvider for ExitProvider {
    async fn create_indexes(
        &self,
        graph: &str,
        component: &drasi_core_types::ComponentId,
    ) -> Result<ComputationIndexes, IndexError> {
        let original = self.inner.create_indexes(graph, component).await?;
        let control: Arc<dyn SessionControl> = Arc::new(ExitControl {
            inner: original.indexes().session_control.clone(),
            exit: self.exit.clone(),
        });
        let domain = TransactionDomain::new(control.clone());
        let set = original.indexes();
        Ok(ComputationIndexes::try_new(
            IndexSet {
                element_index: set.element_index.clone(),
                archive_index: set.archive_index.clone(),
                result_index: set.result_index.clone(),
                future_queue: set.future_queue.clone(),
                session_control: control,
            },
            Some(domain.clone()),
            Some(ComputationResource::participating(
                original
                    .checkpoint_store()
                    .expect("RocksDB checkpoints")
                    .clone(),
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

/// The child: deliver the case's batch and die at its boundary.
///
/// Launched only by the parents below with [`CASE`] set; without it (for
/// example under a blanket `--ignored` run) it has nothing to do.
#[tokio::test(flavor = "current_thread")]
#[ignore = "process-exit child launched by the native_storedproc_crash parents"]
async fn storedproc_crash_child() -> Result<()> {
    let Some(case) = std::env::var_os(CASE) else {
        return Ok(());
    };
    let case: Case = serde_json::from_str(case.to_str().context("case encoding")?)?;
    let exit = Arc::new(ExitAt {
        remaining: AtomicUsize::new(0),
        after: case.point == Point::BeforeAcknowledgement,
        code: case.exit_code(),
    });
    let mut sink = consumer(
        &case,
        Arc::new(ExitProvider {
            inner: rocks(&case.ledger)?,
            exit: exit.clone(),
        }),
    )
    .await?;
    let input = case.input()?;
    match case.guarantee {
        Guarantee::ExactlyOnce => {
            // Ledger commits during one delivery: the recorded input, then
            // each operation's completion.
            let at = match case.point {
                Point::AfterDestinationCommit => 2,
                Point::BeforeAcknowledgement => 1 + BATCH as usize,
            };
            exit.remaining.store(at, Ordering::Release);
            let result = sink.handle(input).await;
            anyhow::bail!("the exactly-once delivery passed its exit boundary: {result:?}")
        }
        Guarantee::AtLeastOnce => {
            let result = sink.handle(input).await;
            match (case.point, result) {
                // The second operation failed at the destination after the
                // first one committed; the process dies before any retry or
                // acknowledgement.
                (Point::AfterDestinationCommit, Err(_)) => std::process::exit(case.exit_code()),
                // The batch is committed at the destination; the process dies
                // before the graph acknowledges it.
                (Point::BeforeAcknowledgement, Ok(())) => std::process::exit(case.exit_code()),
                (point, result) => anyhow::bail!("unexpected delivery at {point:?}: {result:?}"),
            }
        }
    }
}

/// A Podman PostgreSQL fixture owned by this test.
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
                "drasi.qualification=native-storedproc-crash",
                "postgres:16-alpine",
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
        anyhow::ensure!(
            output.status.success(),
            "PostgreSQL fixture port unavailable"
        );
        database.port = String::from_utf8(output.stdout)?
            .trim()
            .strip_prefix("127.0.0.1:")
            .context("loopback binding")?
            .parse()?;
        // The image restarts the server once after initialization; wait for
        // the final server through the published TCP port.
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                let ready = tokio::process::Command::new("podman")
                    .args([
                        "exec",
                        &database.container,
                        "pg_isready",
                        "-h",
                        "127.0.0.1",
                        "-U",
                        "postgres",
                    ])
                    .output()
                    .await?;
                if ready.status.success() && database.sql("SELECT 1").await.is_ok() {
                    return Result::<()>::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await
        .context("PostgreSQL fixture readiness")??;
        Ok(database)
    }

    async fn sql(&self, sql: &str) -> Result<String> {
        let output = tokio::process::Command::new("podman")
            .args([
                "exec",
                &self.container,
                "psql",
                "-h",
                "127.0.0.1",
                "-U",
                "postgres",
                "-d",
                "postgres",
                "-v",
                "ON_ERROR_STOP=1",
                "-At",
                "-c",
                sql,
            ])
            .kill_on_drop(true)
            .output()
            .await?;
        anyhow::ensure!(
            output.status.success(),
            "PostgreSQL fixture SQL: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
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
            Ok(output) => log::error!(
                "PostgreSQL fixture cleanup: {}",
                String::from_utf8_lossy(&output.stderr)
            ),
            Err(error) => log::error!("PostgreSQL fixture cleanup: {error}"),
        }
    }
}

enum Destination {
    MySql(mysql::Database),
    Postgres(Postgres),
}

impl Destination {
    async fn start(kind: Kind) -> Result<Self> {
        let destination = match kind {
            Kind::MySql => Self::MySql(mysql::Database::new().await?),
            Kind::Postgres => Self::Postgres(Postgres::start().await?),
        };
        destination.prepare().await?;
        Ok(destination)
    }

    fn kind(&self) -> Kind {
        match self {
            Self::MySql(_) => Kind::MySql,
            Self::Postgres(_) => Kind::Postgres,
        }
    }

    async fn sql(&self, sql: &str) -> Result<String> {
        match self {
            Self::MySql(database) => database.sql(sql).await,
            Self::Postgres(database) => database.sql(sql).await,
        }
    }

    /// `effect` audits every application; it fails for the gated id while the
    /// gate is enabled, rolling back that operation's destination transaction.
    async fn prepare(&self) -> Result<()> {
        match self {
            Self::MySql(database) => {
                database
                    .sql(
                        "CREATE TABLE audit(query_id VARCHAR(64),operation VARCHAR(16),id INT); \
                         CREATE TABLE crash_gate(failed_id INT NOT NULL); \
                         INSERT INTO crash_gate VALUES(0);\n\
                         DELIMITER //\n\
                         CREATE PROCEDURE effect(IN q VARCHAR(64),IN op VARCHAR(16),IN i INT,IN v TEXT) BEGIN \
                         INSERT INTO audit VALUES(q,op,i); \
                         IF (SELECT failed_id=i FROM crash_gate) THEN \
                         SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT='gated destination failure'; END IF; END //\n\
                         DELIMITER ;\n",
                    )
                    .await?;
            }
            Self::Postgres(database) => {
                database
                    .sql(
                        "CREATE TABLE audit(query_id text, operation text, id bigint); \
                         CREATE TABLE crash_gate(failed_id bigint NOT NULL); \
                         INSERT INTO crash_gate VALUES(0); \
                         CREATE PROCEDURE effect(q text, op text, i bigint, v text) LANGUAGE plpgsql AS $$ \
                         BEGIN INSERT INTO audit VALUES(q, op, i); \
                         IF (SELECT failed_id = i FROM crash_gate) THEN \
                         RAISE EXCEPTION 'gated destination failure'; END IF; END $$;",
                    )
                    .await?;
            }
        }
        Ok(())
    }

    fn configuration(
        &self,
        case_alias: &str,
        producer: &ComponentId,
        guarantee: Guarantee,
    ) -> Value {
        let template = json!({"template": TEMPLATE});
        let mut configuration = match self {
            Self::MySql(database) => json!({
                "host": "127.0.0.1", "sslMode": "disable", "port": database.port,
                "database": "test", "user": "root", "password": "native-test",
            }),
            Self::Postgres(database) => json!({
                "host": "127.0.0.1", "sslMode": "disable", "port": database.port,
                "database": "postgres", "user": "postgres", "password": "native-test",
            }),
        };
        configuration["outputs"] = json!({case_alias: {"component": producer, "port": "out"}});
        configuration["defaultTemplate"] =
            json!({"added": template, "updated": template, "deleted": template});
        configuration["progressTable"] = json!(format!("crash_progress_{case_alias}"));
        configuration["deliveryGuarantee"] = json!(match guarantee {
            Guarantee::ExactlyOnce => "exactlyOnce",
            Guarantee::AtLeastOnce => "atLeastOnce",
        });
        configuration
    }

    async fn applied(&self, case: &Case) -> Result<i64> {
        Ok(self
            .sql(&format!(
                "SELECT COUNT(*) FROM audit WHERE query_id='{}'",
                case.alias
            ))
            .await?
            .parse()?)
    }

    /// The case's destination cursor rows as `epoch` values (empty for
    /// exactly-once cursors).
    async fn epochs(&self, case: &Case) -> Result<Vec<String>> {
        // A marker keeps an empty epoch from disappearing as an empty line.
        let rows = self
            .sql(&format!(
                "SELECT CONCAT('epoch:', COALESCE(epoch,'')) FROM crash_progress_{}",
                case.alias
            ))
            .await?;
        rows.lines()
            .map(|row| {
                row.strip_prefix("epoch:")
                    .map(str::to_owned)
                    .with_context(|| format!("unexpected cursor row {row:?}"))
            })
            .collect()
    }
}

/// Runs the child to its boundary and checks its exit status.
async fn crash(case: &Case) -> Result<()> {
    let child = tokio::time::timeout(
        Duration::from_secs(120),
        tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "storedproc_crash_child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CASE, serde_json::to_string(case)?)
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    anyhow::ensure!(
        child.status.code() == Some(case.exit_code()),
        "crash child for {case:?} ended with {}:\n{}\n{}",
        child.status,
        String::from_utf8_lossy(&child.stdout),
        String::from_utf8_lossy(&child.stderr)
    );
    Ok(())
}

async fn qualify(
    destination: &Destination,
    guarantee: Guarantee,
    point: Point,
    producer: Producer,
    index: i64,
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let alias = format!("c{index}");
    let first_id = 1000 * (index + 1);
    let producer_id = id(&format!("{alias}-producer"));
    let mut case = Case {
        kind: destination.kind(),
        guarantee,
        point,
        producer,
        configuration: destination.configuration(&alias, &producer_id, guarantee),
        ledger: directory.path().join("ledger"),
        alias,
        first_id,
        incarnation: match producer {
            Producer::Persistent => PERSISTENT.into(),
            Producer::Volatile => fresh_incarnation(),
        },
    };
    let gated = guarantee == Guarantee::AtLeastOnce && point == Point::AfterDestinationCommit;
    if gated {
        destination
            .sql(&format!("UPDATE crash_gate SET failed_id={}", first_id + 1))
            .await?;
    }
    crash(&case).await?;
    let applied = destination.applied(&case).await?;
    let in_flight = match point {
        Point::AfterDestinationCommit => 1,
        Point::BeforeAcknowledgement => BATCH,
    };
    assert_eq!(applied, in_flight, "effects committed before the crash");
    let crash_epochs = destination.epochs(&case).await?;
    if gated {
        destination.sql("UPDATE crash_gate SET failed_id=0").await?;
    }

    // Restart: the same ledger and the unacknowledged batch, redelivered by
    // the producer's next run.
    if producer == Producer::Volatile {
        case.incarnation = fresh_incarnation();
    }
    let mut sink = consumer(&case, rocks(&case.ledger)?).await?;
    sink.handle(case.input()?).await?;
    // A repeat after acknowledgement changes nothing.
    sink.handle(case.input()?).await?;
    sink.stop(&hooks::stop_ctx()).await?;
    let applied = destination.applied(&case).await?;
    let epochs = destination.epochs(&case).await?;
    assert_eq!(epochs.len(), 1, "one cursor row per stream: {epochs:?}");
    match (guarantee, producer) {
        (Guarantee::ExactlyOnce, _) => {
            assert_eq!(applied, BATCH, "exactly once: destination rows == inputs");
            assert_eq!(
                epochs,
                [String::new()],
                "exactly-once cursors carry no epoch"
            );
        }
        (Guarantee::AtLeastOnce, Producer::Persistent) => {
            assert_eq!(applied, BATCH, "a durable producer keeps its epoch");
            assert_eq!(epochs, crash_epochs, "the cursor epoch is unchanged");
        }
        (Guarantee::AtLeastOnce, Producer::Volatile) => {
            assert_eq!(
                applied,
                BATCH + in_flight,
                "at least once: inputs plus the one in-flight batch's committed part"
            );
            assert!(applied <= 2 * BATCH, "never more than one repeated batch");
            assert_eq!(epochs[0].len(), 64, "{epochs:?}");
            assert!(epochs[0].bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert_eq!(crash_epochs.len(), 1);
            assert_ne!(epochs, crash_epochs, "a new producer run is a new epoch");
        }
    }
    Ok(())
}

async fn matrix(kind: Kind) -> Result<()> {
    let destination = Destination::start(kind).await?;
    let cases = [
        (
            Guarantee::ExactlyOnce,
            Point::AfterDestinationCommit,
            Producer::Persistent,
        ),
        (
            Guarantee::ExactlyOnce,
            Point::BeforeAcknowledgement,
            Producer::Persistent,
        ),
        (
            Guarantee::AtLeastOnce,
            Point::AfterDestinationCommit,
            Producer::Volatile,
        ),
        (
            Guarantee::AtLeastOnce,
            Point::BeforeAcknowledgement,
            Producer::Volatile,
        ),
        (
            Guarantee::AtLeastOnce,
            Point::BeforeAcknowledgement,
            Producer::Persistent,
        ),
    ];
    for (index, (guarantee, point, producer)) in cases.into_iter().enumerate() {
        qualify(&destination, guarantee, point, producer, index as i64)
            .await
            .with_context(|| format!("{kind:?} {guarantee:?} {point:?} {producer:?}"))?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and the rebuilt native MySQL stored-procedure library"]
async fn native_storedproc_crash_mysql() -> Result<()> {
    matrix(Kind::MySql).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and the rebuilt native PostgreSQL stored-procedure library"]
async fn native_storedproc_crash_postgres() -> Result<()> {
    matrix(Kind::Postgres).await
}
