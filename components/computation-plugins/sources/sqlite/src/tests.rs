// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_contracts::{
    lifecycle::{CancellationToken, LifecycleErrorKind, OperationScope, StopMode},
    ComponentId, PortDirection, StreamId,
};
use drasi_computation_plugin_sdk::{
    lifecycle::{
        DisposeContext, DisposeReason, InitializeContext, PauseContext, ResumeContext,
        StartContext, StateIntent, StopContext, UpdateBoundary,
    },
    testing::{create_source, Offer, SourceConformance, SourceProbe},
    ExportedPlugin, NativeComponent, NativeSource,
};
use serde_json::{json, Value};
use std::{
    path::Path,
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

const SOURCE_ID: &str = "00000000-0000-0000-0000-00000000004d";
const TOKEN: &str = "0123456789abcdef";
/// Bound on every awaited step, so a deadlock fails instead of hanging.
const STEP: Duration = Duration::from_secs(20);

async fn step<T>(name: &str, future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(STEP, future)
        .await
        .unwrap_or_else(|_| panic!("step `{name}` timed out"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .expect("free port")
}

fn database(path: &Path) {
    rusqlite::Connection::open(path)
        .and_then(|connection| {
            connection
                .execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL)")
        })
        .expect("test database");
}

fn scope(name: &str, deadline: Option<Duration>) -> OperationScope {
    OperationScope::new(
        name,
        "sqlite-test",
        1,
        deadline.map(|deadline| tokio::time::Instant::now() + deadline),
        CancellationToken::new(),
    )
}

fn stop_ctx(mode: StopMode, deadline: Duration) -> StopContext {
    StopContext::new(scope("stop", Some(deadline)), mode)
}

fn start_ctx() -> StartContext {
    StartContext::new(scope("start", Some(Duration::from_secs(10))), None)
}

fn factory(mode: Mode) -> SqliteFactory {
    SqliteFactory(mode)
}

fn rest_config(path: &Path, port: u16) -> Value {
    json!({
        "stream": "sqlite/out",
        "path": path,
        "tables": ["items"],
        "restApi": true,
        "restPort": port,
        "outputCapacity": 4,
        "shutdownTimeoutMs": 5000,
    })
}

fn create(mode: Mode, configuration: Value) -> Box<dyn NativeSource> {
    create_source(
        &factory(mode),
        &ComponentId::try_new(SOURCE_ID).expect("id"),
        configuration,
    )
    .expect("source")
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client")
}

/// Posts one row through REST, reading the port and token from the source's
/// exposed configuration.
struct RestProbe {
    next: AtomicI64,
}

impl RestProbe {
    fn new() -> Self {
        Self {
            next: AtomicI64::new(1),
        }
    }
}

#[async_trait::async_trait]
impl SourceProbe for RestProbe {
    async fn offer(&self, configuration: &Value) -> anyhow::Result<Offer> {
        let port = configuration["restPort"].as_u64().unwrap_or_default();
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let mut request = client()
            .post(format!("http://127.0.0.1:{port}/api/tables/items"))
            .json(&json!({"id": id, "value": "probe"}));
        if let Some(token) = configuration["authToken"].as_str() {
            request = request.bearer_auth(token);
        }
        Ok(match request.send().await {
            Ok(response) if response.status().is_success() => Offer::Accepted,
            _ => Offer::Refused,
        })
    }
}

#[test]
fn factory_metadata_constructs_without_database_io() -> anyhow::Result<()> {
    ExportedPlugin::new(plugin()?)?;
    #[cfg(feature = "dynamic-plugin")]
    assert!(!drasi_computation_plugin_metadata().is_null());
    for mode in MODES {
        let metadata = SqliteFactory(mode).metadata();
        assert_eq!(
            metadata.implementation,
            ImplementationIdentity::try_new(implementation(mode), "1")?
        );
        assert_eq!(metadata.role, ComponentRole::Source);
        assert_eq!(metadata.ports, SqliteSource::ports(mode.output())?);
        assert_eq!(metadata.ports[0].id().as_str(), "out");
        assert_eq!(metadata.ports[0].direction(), PortDirection::Output);
        assert!(metadata.capabilities.management);
        assert!(metadata.capabilities.erases_owned_state);
        let details = SqliteFactory(mode)
            .configuration_details()
            .expect("configuration details");
        details.validate_schema(&metadata.configuration)?;
        let id = ComponentId::generate();
        let descriptor = SqliteSource::describe(id.clone(), mode.output())?;
        assert_eq!(descriptor.ports(), metadata.ports.as_slice());
    }
    assert!(SqliteFactory(Mode::Replay).supports_source_progress());
    assert!(SqliteFactory(Mode::Shared).supports_consumer_progress());
    Ok(())
}

#[test]
fn journal_factories_refuse_standalone_construction_and_memory_paths() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("app.db");
    for mode in [Mode::Replay, Mode::Shared] {
        let request = drasi_computation_plugin_sdk::testing::create_request(
            &factory(mode),
            &ComponentId::try_new(SOURCE_ID).expect("id"),
            json!({"stream": "sqlite/out", "path": path}),
        );
        assert!(factory(mode)
            .create(&request, ControlSender::default())
            .is_err());
    }
    let memory = drasi_computation_plugin_sdk::testing::create_request(
        &factory(Mode::Changes),
        &ComponentId::try_new(SOURCE_ID).expect("id"),
        json!({"stream": "sqlite/out", "path": null}),
    );
    let error = match factory(Mode::Changes).create(&memory, ControlSender::default()) {
        Ok(_) => panic!("in-memory source without initSql must be refused"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}").contains("initSql or sqlApi"),
        "{error:#}"
    );
}

async fn conformance() -> anyhow::Result<()> {
    for mode in [Mode::Changes, Mode::Transactions] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("app.db");
        database(&path);
        let factory = factory(mode);
        let suite = SourceConformance::new(&factory, SOURCE_ID)?;
        let config = rest_config(&path, free_port());
        let probe = RestProbe::new();
        suite.lifecycle(config.clone(), &probe).await?;
        suite.cancelled_start(config.clone(), &probe).await?;
        suite.erase(config.clone()).await?;
        suite.configuration_round_trip(&[
            config.clone(),
            json!({"stream": "sqlite/out", "path": path}),
            json!({"stream": "sqlite/out", "initSql": "CREATE TABLE IF NOT EXISTS t(id INTEGER PRIMARY KEY)"}),
        ])?;
        let mut authenticated = config.clone();
        authenticated["authToken"] = json!(TOKEN);
        let mut rotated = authenticated.clone();
        rotated["authToken"] = json!("fedcba9876543210");
        let mut larger = authenticated.clone();
        larger["maxSqlBytes"] = json!(4096);
        let mut moved = authenticated.clone();
        moved["path"] = json!(directory.path().join("other.db"));
        suite.assess_matrix(
            &authenticated,
            &[
                (authenticated.clone(), Some(UpdateBoundary::Pause)),
                (rotated.clone(), Some(UpdateBoundary::Pause)),
                (larger.clone(), Some(UpdateBoundary::Stop)),
                (moved, None),
            ],
        )?;
        suite.apply(authenticated.clone(), rotated, &probe).await?;
        suite.apply(authenticated, larger, &probe).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn conformance_current_thread() -> anyhow::Result<()> {
    step("conformance", conformance()).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_multi_thread() -> anyhow::Result<()> {
    step("conformance", conformance()).await
}

/// SQ-01. With the output queue full the worker stops taking commands, so an
/// accepted HTTP write waits on it. Stop used to drain REST before telling
/// the worker anything, so the request held Stop until the host timeout, and
/// every retry repeated the wait. Stop now puts the worker in its draining
/// state together with the REST drain: the worker stops waiting for output,
/// the accepted write finishes, the undelivered volatile output is reported,
/// and Stop returns within its deadline.
async fn stop_with_full_output_and_accepted_write() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("app.db");
    database(&path);
    let port = free_port();
    let mut config = rest_config(&path, port);
    config["outputCapacity"] = json!(1);
    config["shutdownTimeoutMs"] = json!(30_000);
    let mut source = create(Mode::Changes, config);
    step("start", source.start(&mut start_ctx()))
        .await
        .expect("start");
    let url = format!("http://127.0.0.1:{port}/api/tables/items");
    // The first commit fills the output queue; the second is held pending.
    for id in [1, 2] {
        let status = step(
            "write",
            client()
                .post(&url)
                .json(&json!({"id": id, "value": "queued"}))
                .send(),
        )
        .await
        .expect("write")
        .status();
        assert!(status.is_success(), "{status}");
    }
    // The third is accepted by REST and waits for the worker.
    let blocked = tokio::spawn({
        let url = url.clone();
        async move {
            client()
                .post(&url)
                .json(&json!({"id": 3, "value": "blocked"}))
                .send()
                .await
                .map(|response| response.status())
        }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!blocked.is_finished(), "the third write must be waiting");
    let started = tokio::time::Instant::now();
    step(
        "stop",
        source.stop(&stop_ctx(StopMode::Drain, Duration::from_secs(5))),
    )
    .await
    .expect("Stop must not wait on a request blocked behind undelivered output");
    assert!(started.elapsed() < Duration::from_secs(5));
    let status = step("blocked write", blocked)
        .await
        .expect("join")
        .expect("response");
    assert!(status.is_success(), "the accepted write finished: {status}");
    let count: i64 = rusqlite::Connection::open(&path)
        .and_then(|connection| {
            connection.query_row("SELECT count(*) FROM items", [], |row| row.get(0))
        })
        .expect("count");
    assert_eq!(count, 3, "the accepted write committed exactly once");
}

#[tokio::test(flavor = "current_thread")]
async fn stop_deadlock_current_thread() {
    stop_with_full_output_and_accepted_write().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_deadlock_multi_thread() {
    stop_with_full_output_and_accepted_write().await
}

/// Drain waits for an open transaction; on expiry Stop fails retryably and
/// keeps ownership, and a later Abort rolls it back promptly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn drain_timeout_keeps_ownership_and_abort_rolls_back() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("app.db");
    database(&path);
    let config = SqliteConfig {
        path: Some(path.clone()),
        tables: vec!["items".into()],
        table_keys: Vec::new(),
        output: SqliteOutput::Changes,
        replay: None,
        transactions: drasi_computation_contracts::SourceTransactionLimits {
            max_changes: std::num::NonZeroUsize::new(64).expect("limit"),
            max_bytes: std::num::NonZeroUsize::new(1 << 20).expect("limit"),
            max_duration_ms: std::num::NonZeroU64::new(60_000).expect("deadline"),
        },
        max_sql_bytes: std::num::NonZeroUsize::new(65536).expect("limit"),
        command_capacity: std::num::NonZeroUsize::new(4).expect("capacity"),
        output_capacity: std::num::NonZeroUsize::new(4).expect("capacity"),
        shutdown_timeout_ms: std::num::NonZeroU64::new(5000).expect("deadline"),
    };
    let mut source = SqliteSource::new(
        ComponentId::try_new(SOURCE_ID).expect("id"),
        StreamId::try_new("sqlite/out").expect("stream"),
        config,
    )
    .expect("source");
    step("start", source.start(&mut start_ctx()))
        .await
        .expect("start");
    let handle = source.handle();
    let (entered, inside) = tokio::sync::oneshot::channel();
    let open = tokio::spawn(async move {
        handle
            .transaction(|transaction| async move {
                transaction
                    .execute("INSERT INTO items VALUES(1,'open')")
                    .await?;
                let _ = entered.send(());
                std::future::pending::<()>().await;
                Ok(())
            })
            .await
    });
    step("transaction", inside).await.expect("entered");
    let error = step(
        "drain stop",
        source.stop(&stop_ctx(StopMode::Drain, Duration::from_millis(300))),
    )
    .await
    .expect_err("Drain cannot finish an open transaction before the deadline");
    assert_eq!(error.kind(), LifecycleErrorKind::Retryable);
    step(
        "abort stop",
        source.stop(&stop_ctx(StopMode::Abort, Duration::from_secs(5))),
    )
    .await
    .expect("Abort completes the retained stop");
    open.abort();
    let count: i64 = rusqlite::Connection::open(&path)
        .and_then(|connection| {
            connection.query_row("SELECT count(*) FROM items", [], |row| row.get(0))
        })
        .expect("count");
    assert_eq!(count, 0, "the aborted transaction rolled back");
}

/// SQ-02. Pause refuses REST and handle writes, holds output, still answers
/// reads, and Resume reopens exactly that.
#[tokio::test(flavor = "current_thread")]
async fn pause_gates_rest_and_handle_writes() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("app.db");
    database(&path);
    let port = free_port();
    let mut source = create(Mode::Changes, rest_config(&path, port));
    step("start", source.start(&mut start_ctx()))
        .await
        .expect("start");
    let url = format!("http://127.0.0.1:{port}");
    let post = |id: i64| {
        client()
            .post(format!("{url}/api/tables/items"))
            .json(&json!({"id": id, "value": "row"}))
            .send()
    };
    assert!(step("write", post(1))
        .await
        .expect("write")
        .status()
        .is_success());
    assert!(step("next", source.next()).await.expect("next").is_some());
    step(
        "pause",
        source.pause(&PauseContext::new(scope(
            "pause",
            Some(Duration::from_secs(5)),
        ))),
    )
    .await
    .expect("pause");
    let refused = step("paused write", post(2)).await.expect("response");
    assert_eq!(refused.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        refused
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("1")
    );
    let rows: Value = step(
        "paused read",
        client().get(format!("{url}/api/tables/items")).send(),
    )
    .await
    .expect("read")
    .json()
    .await
    .expect("rows");
    assert_eq!(
        rows,
        json!([{"id": 1, "value": "row"}]),
        "reads are answered"
    );
    step(
        "resume",
        source.resume(&ResumeContext::new(scope("resume", None))),
    )
    .await
    .expect("resume");
    assert!(step("write", post(3))
        .await
        .expect("write")
        .status()
        .is_success());
    assert!(step("next", source.next()).await.expect("next").is_some());
    step(
        "stop",
        source.stop(&stop_ctx(StopMode::Drain, Duration::from_secs(5))),
    )
    .await
    .expect("stop");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn paused_handle_writes_fail_typed_and_resume() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("app.db");
    database(&path);
    let id = ComponentId::try_new(SOURCE_ID).expect("id");
    let config = SourceConfig::from_value(
        Mode::Transactions,
        &json!({"stream": "sqlite/out", "path": path, "tables": ["items"]}),
    )
    .expect("config");
    let mut source = SqliteSource::build(id, config, component::Progress::None).expect("source");
    step("start", source.start(&mut start_ctx()))
        .await
        .expect("start");
    let handle = source.handle();
    step(
        "pause",
        source.pause(&PauseContext::new(scope(
            "pause",
            Some(Duration::from_secs(5)),
        ))),
    )
    .await
    .expect("pause");
    let error = step(
        "paused write",
        handle.execute("INSERT INTO items VALUES(1,'x')"),
    )
    .await
    .expect_err("writes are refused while paused");
    assert!(
        drasi_sqlite_common::native::error::is_paused(&error),
        "{error:#}"
    );
    let rows = step(
        "paused read",
        handle.query("SELECT count(*) AS n FROM items"),
    )
    .await
    .expect("read");
    assert_eq!(rows[0]["n"], 0);
    step(
        "resume",
        source.resume(&ResumeContext::new(scope("resume", None))),
    )
    .await
    .expect("resume");
    step("write", handle.execute("INSERT INTO items VALUES(1,'x')"))
        .await
        .expect("write after resume");
    assert!(step("next", source.next()).await.expect("next").is_some());
    step(
        "stop",
        source.stop(&stop_ctx(StopMode::Abort, Duration::from_secs(5))),
    )
    .await
    .expect("stop");
}

/// A cancelled Pause leaves the source running as before.
#[tokio::test(flavor = "current_thread")]
async fn cancelled_pause_reopens_admission() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("app.db");
    database(&path);
    let port = free_port();
    let mut source = create(Mode::Changes, rest_config(&path, port));
    step("start", source.start(&mut start_ctx()))
        .await
        .expect("start");
    let cancelled = scope("pause", Some(Duration::from_secs(5)));
    cancelled.cancellation().cancel();
    let error = step("pause", source.pause(&PauseContext::new(cancelled)))
        .await
        .expect_err("cancelled pause");
    assert_eq!(error.kind(), LifecycleErrorKind::Cancelled);
    let response = step(
        "write",
        client()
            .post(format!("http://127.0.0.1:{port}/api/tables/items"))
            .json(&json!({"id": 1, "value": "row"}))
            .send(),
    )
    .await
    .expect("write");
    assert!(response.status().is_success());
    step(
        "stop",
        source.stop(&stop_ctx(StopMode::Drain, Duration::from_secs(5))),
    )
    .await
    .expect("stop");
}

/// SQ-05 / D-6. An in-memory source creates its tables with initSql, which
/// runs before the authorizer but may not touch journal objects.
#[tokio::test(flavor = "current_thread")]
async fn init_sql_prepares_an_in_memory_source() {
    let port = free_port();
    let mut source = create(
        Mode::Changes,
        json!({
            "stream": "sqlite/out",
            "initSql": "CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT NOT NULL); PRAGMA user_version=7;",
            "restApi": true,
            "restPort": port,
        }),
    );
    step(
        "initialize",
        source.initialize(&InitializeContext::new(scope("initialize", None))),
    )
    .await
    .expect("initialize");
    step("start", source.start(&mut start_ctx()))
        .await
        .expect("start");
    let response = step(
        "write",
        client()
            .post(format!("http://127.0.0.1:{port}/api/tables/items"))
            .json(&json!({"id": 1, "value": "memory"}))
            .send(),
    )
    .await
    .expect("write");
    assert!(response.status().is_success(), "{}", response.status());
    assert!(step("next", source.next()).await.expect("next").is_some());
    step(
        "stop",
        source.stop(&stop_ctx(StopMode::Drain, Duration::from_secs(5))),
    )
    .await
    .expect("stop");

    let mut reserved = source_without_rest("CREATE TABLE __drasi_native_state_v1(x)");
    let error = step("start", reserved.start(&mut start_ctx()))
        .await
        .expect_err("initSql may not create journal objects");
    assert!(format!("{error:#}").contains("initSql"), "{error:#}");
    step(
        "stop",
        reserved.stop(&stop_ctx(StopMode::Abort, Duration::from_secs(5))),
    )
    .await
    .expect("stop after failed start");
}

fn source_without_rest(init_sql: &str) -> Box<dyn NativeSource> {
    create(
        Mode::Changes,
        json!({"stream": "sqlite/out", "initSql": init_sql}),
    )
}

/// `/api/sql` is off by default, needs the bearer token, and runs under the
/// source's authorizer: additive DDL and DML are allowed, while DROP/ALTER
/// TABLE, pragmas, transaction control and journal objects are refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sql_route_requires_authentication_and_the_authorizer() {
    let port = free_port();
    let mut source = create(
        Mode::Changes,
        json!({
            "stream": "sqlite/out",
            "restApi": true,
            "restPort": port,
            "sqlApi": true,
            "authToken": TOKEN,
        }),
    );
    step("start", source.start(&mut start_ctx()))
        .await
        .expect("start");
    let url = format!("http://127.0.0.1:{port}/api/sql");
    let sql = |body: Value, token: Option<&str>| {
        let mut request = client().post(&url).json(&body);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        request.send()
    };
    let execute = |statement: &str| json!({"execute": [{"sql": statement}]});
    let anonymous = step("anonymous", sql(execute("SELECT 1"), None))
        .await
        .expect("response");
    assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);
    let wrong = step(
        "wrong token",
        sql(execute("SELECT 1"), Some("fedcba9876543210")),
    )
    .await
    .expect("response");
    assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);
    let health = step(
        "health",
        client()
            .get(format!("http://127.0.0.1:{port}/health"))
            .send(),
    )
    .await
    .expect("health");
    assert!(health.status().is_success(), "health stays open");
    let created = step(
        "create",
        sql(
            execute("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT)"),
            Some(TOKEN),
        ),
    )
    .await
    .expect("create");
    assert!(created.status().is_success(), "{}", created.status());
    let inserted: Value = step(
        "insert",
        sql(
            json!({"execute": [
                {"sql": "INSERT INTO items VALUES(?, ?)", "params": [1, "one"]},
                {"sql": "INSERT INTO items VALUES(?, ?)", "params": [2, "two"]}
            ]}),
            Some(TOKEN),
        ),
    )
    .await
    .expect("insert")
    .json()
    .await
    .expect("counts");
    assert_eq!(inserted, json!({"rowsAffected": [1, 1]}));
    assert!(step("next", source.next()).await.expect("next").is_some());
    let rows: Value = step(
        "query",
        sql(
            json!({"query": {"sql": "SELECT value FROM items WHERE id = ?", "params": [2]}}),
            Some(TOKEN),
        ),
    )
    .await
    .expect("query")
    .json()
    .await
    .expect("rows");
    assert_eq!(rows, json!({"rows": [{"value": "two"}]}));
    // Additive DDL keeps every element ID and is captured from then on.
    let altered = step(
        "alter",
        sql(
            execute("ALTER TABLE items ADD COLUMN extra TEXT"),
            Some(TOKEN),
        ),
    )
    .await
    .expect("alter");
    assert_eq!(altered.status(), reqwest::StatusCode::OK);
    let inserted = step(
        "insert after alter",
        sql(
            execute("INSERT INTO items(id, value, extra) VALUES (3, 'three', 'added')"),
            Some(TOKEN),
        ),
    )
    .await
    .expect("insert after alter");
    assert_eq!(inserted.status(), reqwest::StatusCode::OK);
    assert!(step("next", source.next()).await.expect("next").is_some());
    for denied in [
        // Dropping a captured table would orphan its element IDs.
        "DROP TABLE items",
        "PRAGMA journal_mode=WAL",
        "BEGIN",
        "CREATE TABLE __drasi_native_state_v1(x)",
        "ATTACH DATABASE ':memory:' AS other",
    ] {
        let response = step("denied", sql(execute(denied), Some(TOKEN)))
            .await
            .expect("response");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::BAD_REQUEST,
            "{denied} must be refused"
        );
    }
    step(
        "stop",
        source.stop(&stop_ctx(StopMode::Drain, Duration::from_secs(5))),
    )
    .await
    .expect("stop");

    // Disabled by default.
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("app.db");
    database(&path);
    let port = free_port();
    let mut plain = create(Mode::Changes, rest_config(&path, port));
    step("start", plain.start(&mut start_ctx()))
        .await
        .expect("start");
    let missing = step(
        "disabled",
        client()
            .post(format!("http://127.0.0.1:{port}/api/sql"))
            .json(&execute("SELECT 1"))
            .send(),
    )
    .await
    .expect("response");
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    step(
        "stop",
        plain.stop(&stop_ctx(StopMode::Drain, Duration::from_secs(5))),
    )
    .await
    .expect("stop");
}

/// SQ-08. Bodies above maxRequestBytes get 413 and row listings page past the
/// read bound.
#[tokio::test(flavor = "current_thread")]
async fn rest_limits_and_paging() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("app.db");
    database(&path);
    {
        let connection = rusqlite::Connection::open(&path).expect("open");
        for id in 1..=25 {
            connection
                .execute("INSERT INTO items VALUES(?1,'seed')", [id])
                .expect("seed");
        }
    }
    let port = free_port();
    let mut config = rest_config(&path, port);
    config["maxTransactionChanges"] = json!(10);
    config["pageSize"] = json!(10);
    config["maxRequestBytes"] = json!(256);
    let mut source = create(Mode::Changes, config);
    step("start", source.start(&mut start_ctx()))
        .await
        .expect("start");
    let url = format!("http://127.0.0.1:{port}/api/tables/items");
    let large = step(
        "large body",
        client()
            .post(&url)
            .json(&json!({"id": 100, "value": "x".repeat(1024)}))
            .send(),
    )
    .await
    .expect("response");
    assert_eq!(large.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    let mut offset = Some("0".to_owned());
    let mut seen = Vec::new();
    while let Some(next) = offset {
        let response = step(
            "page",
            client().get(format!("{url}?limit=10&offset={next}")).send(),
        )
        .await
        .expect("page");
        let status = response.status();
        if !status.is_success() {
            panic!(
                "page failed: {status} {}",
                response.text().await.unwrap_or_default()
            );
        }
        offset = response
            .headers()
            .get("x-next-offset")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let rows: Vec<Value> = response.json().await.expect("rows");
        assert!(rows.len() <= 10);
        seen.extend(rows.into_iter().map(|row| row["id"].as_i64().expect("id")));
    }
    assert_eq!(seen, (1..=25).collect::<Vec<_>>());
    let unknown = step("bad query", client().get(format!("{url}?page=2")).send())
        .await
        .expect("response");
    assert_eq!(unknown.status(), reqwest::StatusCode::BAD_REQUEST);
    step(
        "stop",
        source.stop(&stop_ctx(StopMode::Drain, Duration::from_secs(5))),
    )
    .await
    .expect("stop");
}

/// A REST token rotation applies in place after a completed pause.
#[tokio::test(flavor = "current_thread")]
async fn token_rotation_applies_in_place() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("app.db");
    database(&path);
    let port = free_port();
    let mut current = rest_config(&path, port);
    current["authToken"] = json!(TOKEN);
    let mut proposed = current.clone();
    proposed["authToken"] = json!("fedcba9876543210");
    let factory = factory(Mode::Changes);
    let id = ComponentId::try_new(SOURCE_ID).expect("id");
    let mut source = create(Mode::Changes, current.clone());
    step("start", source.start(&mut start_ctx()))
        .await
        .expect("start");
    let request =
        drasi_computation_plugin_sdk::testing::update_request(&factory, &id, current, proposed);
    let management = source.management().expect("management");
    let assessment = management.assess(&request).expect("assess");
    let plan = assessment.plan().expect("supported");
    let update = drasi_computation_plugin_sdk::ApplyUpdateRequest {
        operation_id: "rotate".into(),
        request,
        plan,
        fingerprint: None,
    };
    let refused = management
        .apply(
            &drasi_computation_plugin_sdk::lifecycle::ApplyContext::new(
                scope("apply", None),
                UpdateBoundary::Pause,
            ),
            &update,
        )
        .await;
    assert!(
        refused.is_err(),
        "in-place apply requires a completed pause"
    );
    step(
        "pause",
        source.pause(&PauseContext::new(scope(
            "pause",
            Some(Duration::from_secs(5)),
        ))),
    )
    .await
    .expect("pause");
    source
        .management()
        .expect("management")
        .apply(
            &drasi_computation_plugin_sdk::lifecycle::ApplyContext::new(
                scope("apply", None),
                UpdateBoundary::Pause,
            ),
            &update,
        )
        .await
        .expect("apply");
    step(
        "resume",
        source.resume(&ResumeContext::new(scope("resume", None))),
    )
    .await
    .expect("resume");
    let url = format!("http://127.0.0.1:{port}/api/tables/items");
    let old = step(
        "old token",
        client()
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&json!({"id": 1, "value": "row"}))
            .send(),
    )
    .await
    .expect("response");
    assert_eq!(old.status(), reqwest::StatusCode::UNAUTHORIZED);
    let new = step(
        "new token",
        client()
            .post(&url)
            .bearer_auth("fedcba9876543210")
            .json(&json!({"id": 1, "value": "row"}))
            .send(),
    )
    .await
    .expect("response");
    assert!(new.status().is_success());
    step(
        "stop",
        source.stop(&stop_ctx(StopMode::Drain, Duration::from_secs(5))),
    )
    .await
    .expect("stop");
}

#[tokio::test(flavor = "current_thread")]
async fn erase_is_a_no_op_for_volatile_sources_and_requires_stop() {
    let directory = tempfile::tempdir().expect("directory");
    let path = directory.path().join("app.db");
    database(&path);
    let mut source = create(Mode::Changes, json!({"stream": "sqlite/out", "path": path}));
    step("start", source.start(&mut start_ctx()))
        .await
        .expect("start");
    let erase = |attempt| {
        DisposeContext::new(
            scope("erase", Some(Duration::from_secs(5))),
            DisposeReason::Removal,
            StateIntent::Erase,
            attempt,
        )
        .expect("erase context")
    };
    assert!(
        step("erase while running", source.dispose(&erase(1)))
            .await
            .is_err(),
        "dispose requires a stopped source"
    );
    step(
        "stop",
        source.stop(&stop_ctx(StopMode::Drain, Duration::from_secs(5))),
    )
    .await
    .expect("stop");
    for attempt in [1, 2] {
        step("erase", source.dispose(&erase(attempt)))
            .await
            .expect("volatile erase");
    }
    assert!(path.exists(), "erase never removes the user's database");
}
