// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_source_sqlite::{SqliteConfig, SqliteOutput};
use drasi_core::evaluation::context::QueryVariables;
use std::{
    collections::BTreeMap,
    net::TcpListener,
    num::{NonZeroU64, NonZeroUsize},
    process::Stdio,
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const LOCK_PATH: &str = "DRASI_QUERY_PLUGIN_SQLITE_LOCK_PATH";
const LOCK_READY: &str = "query-plugin-sqlite-lock-held";

struct SqliteReadLock {
    child: tokio::process::Child,
}

impl SqliteReadLock {
    async fn acquire(path: &Path) -> Result<Self> {
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "bootstrap::provider::sqlite_live::sqlite_read_lock_holder",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(LOCK_PATH, path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()?;
        let mut output =
            BufReader::new(child.stdout.take().context("SQLite lock holder output")?).lines();
        tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(line) = output.next_line().await? {
                if line == LOCK_READY {
                    return Ok::<(), anyhow::Error>(());
                }
            }
            anyhow::bail!("SQLite lock holder exited before readiness")
        })
        .await
        .context("SQLite lock holder did not become ready")??;
        child.stdout = Some(output.into_inner().into_inner());
        Ok(Self { child })
    }

    async fn release(mut self) -> Result<()> {
        self.child
            .stdin
            .as_mut()
            .context("SQLite lock holder input")?
            .write_all(b"release\n")
            .await?;
        let status = tokio::time::timeout(Duration::from_secs(5), self.child.wait())
            .await
            .context("SQLite lock holder did not exit after release")??;
        anyhow::ensure!(status.success(), "SQLite lock holder failed: {status}");
        Ok(())
    }
}

#[test]
#[ignore = "subprocess fixture launched by the interrupted-bootstrap cases"]
fn sqlite_read_lock_holder() -> Result<()> {
    let path = std::env::var_os(LOCK_PATH).context("SQLite lock holder requires a file path")?;
    let mut connection = rusqlite::Connection::open(PathBuf::from(path))?;
    let journal: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    anyhow::ensure!(
        journal == "delete",
        "exclusive read gate requires rollback journaling"
    );
    // A separate process also gates the SQLite instance bundled in the provider cdylib.
    let lock = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Exclusive)?;
    println!("\n{LOCK_READY}");
    std::io::Write::flush(&mut std::io::stdout())?;
    let mut command = String::new();
    std::io::stdin().read_line(&mut command)?;
    anyhow::ensure!(command == "release\n", "SQLite lock holder lost its owner");
    lock.rollback()?;
    Ok(())
}

fn source_library() -> PathBuf {
    library().with_file_name(format!(
        "{}drasi_computation_source_sqlite{}",
        std::env::consts::DLL_PREFIX,
        std::env::consts::DLL_SUFFIX
    ))
}

pub(super) fn source(path: &Path, rest_port: u16) -> Result<ComponentAddition> {
    let plugin = computation::load(source_library())?;
    let factory = plugin
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == "drasi.sqlite/changes")
        .context("existing native SQLite changes factory missing")?
        .clone();
    let settings = SqliteConfig {
        path: Some(path.to_owned()),
        tables: vec!["items".into()],
        table_keys: Vec::new(),
        output: SqliteOutput::Changes,
        replay: None,
        transactions: SourceTransactionLimits {
            max_changes: NonZeroUsize::new(64).expect("positive bound"),
            max_bytes: NonZeroUsize::new(1 << 20).expect("positive bound"),
            max_duration_ms: NonZeroU64::new(5000).expect("positive deadline"),
        },
        max_sql_bytes: NonZeroUsize::new(65536).expect("positive bound"),
        command_capacity: NonZeroUsize::new(8).expect("positive bound"),
        output_capacity: NonZeroUsize::new(4).expect("positive bound"),
        shutdown_timeout_ms: NonZeroU64::new(5000).expect("positive deadline"),
    };
    let specification = factory.specification(
        ComponentId::try_new("sqlite")?,
        json!({
            "stream": "sqlite/out",
            "settings": settings,
            "rest_api": {"host": "127.0.0.1", "port": rest_port}
        }),
    )?;
    Ok(ComponentAddition::from_specification(
        specification,
        factory,
    ))
}

fn values(id: i64, amount: i64) -> QueryVariables {
    [
        ("id".into(), VariableValue::from(json!(id))),
        ("amount".into(), VariableValue::from(json!(amount))),
    ]
    .into_iter()
    .collect()
}

fn assert_rows(
    snapshot: &OutputSnapshot,
    expected: &[(i64, i64)],
) -> Result<BTreeMap<i64, RecordId>> {
    assert_eq!(snapshot.records().len(), expected.len());
    let mut identities = BTreeMap::new();
    for (id, amount) in expected {
        let mut found = None;
        for record in snapshot.records().values() {
            let row = QueryChangeCodec::decode_row(record)?;
            if row.values.get("id") == Some(&VariableValue::from(json!(id))) {
                assert_eq!(row.values, values(*id, *amount));
                anyhow::ensure!(found.is_none(), "duplicate result for SQLite row {id}");
                found = Some(record.identity().clone());
            }
        }
        identities.insert(
            *id,
            found.with_context(|| format!("SQLite row {id} missing"))?,
        );
    }
    Ok(identities)
}

fn assert_database(path: &Path, expected: &[(i64, i64)]) -> Result<()> {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut statement = connection.prepare("SELECT id, amount FROM items ORDER BY id")?;
    let actual = statement
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert_eq!(actual, expected);
    Ok(())
}

async fn real_bootstrap_then_capture() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("live-items.db");
    create_database(&path)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let rest_port = listener.local_addr()?.port();
    drop(listener);
    let fixture = ProviderFixture::with_live_input(
        &path,
        "MATCH (n:items) RETURN n.id AS id, n.amount AS amount",
        Some(source(&path, rest_port)?),
    )
    .await?;
    let exercise = async {
        fixture.start().await?;
        let initial = fixture.snapshot().await?;
        let initial_ids = assert_rows(&initial, &[(1, 10), (2, 20)])?;
        assert_eq!(initial.cursor().sequence(), 0);
        fixture.assert_provider_stopped()?;
        assert!(fixture
            .received
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite result capture poisoned"))?
            .is_empty());

        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()?;
        let url = format!("http://127.0.0.1:{rest_port}/api/tables/items");
        let inserted: serde_json::Value = client
            .post(&url)
            .json(&json!({"id":3,"amount":5}))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        assert_eq!(inserted, json!({"success":true}));
        assert_database(&path, &[(1, 10), (2, 20), (3, 5)])?;
        let addition = fixture.output(1).await?;
        match QueryChangeCodec::decode_evaluation(&addition)?.as_slice() {
            [QueryPartEvaluationContext::Adding { after, .. }] => assert_eq!(after, &values(3, 5)),
            other => anyhow::bail!("unexpected captured SQL insert: {other:?}"),
        }
        let after_insert = fixture.snapshot().await?;
        let inserted_ids = assert_rows(&after_insert, &[(1, 10), (2, 20), (3, 5)])?;
        assert_eq!(inserted_ids[&1], initial_ids[&1]);
        assert_eq!(inserted_ids[&2], initial_ids[&2]);

        client
            .put(format!("{url}/1"))
            .json(&json!({"amount":12}))
            .send()
            .await?
            .error_for_status()?;
        assert_database(&path, &[(1, 12), (2, 20), (3, 5)])?;
        let update = fixture.output(2).await?;
        match QueryChangeCodec::decode_evaluation(&update)?.as_slice() {
            [QueryPartEvaluationContext::Updating { before, after, .. }] => {
                assert_eq!(before, &values(1, 10));
                assert_eq!(after, &values(1, 12));
            }
            other => anyhow::bail!("unexpected captured SQL update: {other:?}"),
        }
        let after_update = fixture.snapshot().await?;
        let updated_ids = assert_rows(&after_update, &[(1, 12), (2, 20), (3, 5)])?;
        assert_eq!(updated_ids[&1], initial_ids[&1]);
        assert_eq!(
            update.changes().operations()[0].identity(),
            &initial_ids[&1]
        );

        client
            .delete(format!("{url}/2"))
            .send()
            .await?
            .error_for_status()?;
        assert_database(&path, &[(1, 12), (3, 5)])?;
        let deletion = fixture.output(3).await?;
        match QueryChangeCodec::decode_evaluation(&deletion)?.as_slice() {
            [QueryPartEvaluationContext::Removing { before, .. }] => {
                assert_eq!(before, &values(2, 20))
            }
            other => anyhow::bail!("unexpected captured SQL delete: {other:?}"),
        }
        assert_eq!(
            deletion.changes().operations()[0].identity(),
            &initial_ids[&2]
        );
        let current = fixture.snapshot().await?;
        let final_ids = assert_rows(&current, &[(1, 12), (3, 5)])?;
        assert_eq!(final_ids[&1], initial_ids[&1]);
        assert_eq!(final_ids[&3], inserted_ids[&3]);
        assert_eq!(current.cursor().sequence(), 3);
        assert_eq!(current.cursor().epoch(), initial.cursor().epoch());
        assert_eq!(
            fixture
                .received
                .lock()
                .map_err(|_| anyhow::anyhow!("SQLite result capture poisoned"))?
                .len(),
            3
        );
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(20), exercise).await;
    tokio::time::timeout(Duration::from_secs(10), fixture.core.shutdown()).await??;
    result??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn existing_sqlite_source_bootstrap_and_query_process_real_writes_current_thread(
) -> Result<()> {
    real_bootstrap_then_capture().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn existing_sqlite_source_bootstrap_and_query_process_real_writes_multi_thread() -> Result<()>
{
    real_bootstrap_then_capture().await
}

async fn assert_unpublished(fixture: &ProviderFixture) -> Result<()> {
    anyhow::ensure!(
        fixture.snapshot().await.is_err(),
        "unfinished initialization exposed a result snapshot"
    );
    anyhow::ensure!(
        fixture
            .received
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite result capture poisoned"))?
            .is_empty(),
        "unfinished initialization emitted live results"
    );
    Ok(())
}

async fn interrupted_bootstrap_then_capture() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("interrupted-items.db");
    create_database(&path)?;
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let rest_port = listener.local_addr()?.port();
    drop(listener);
    let fixture = ProviderFixture::with_live_input(
        &path,
        "MATCH (n:items) RETURN n.id AS id, n.amount AS amount",
        Some(source(&path, rest_port)?),
    )
    .await?;
    let exercise = async {
        fixture.capture.start().await?;
        fixture.live.start().await?;
        let lifecycle = fixture.query.observed()?.lifecycle;
        anyhow::ensure!(
            !matches!(
                lifecycle,
                ComponentLifecycle::Starting | ComponentLifecycle::Running
            ),
            "query activated before the blocked-start request: {lifecycle:?}"
        );

        let lock = SqliteReadLock::acquire(&path).await?;
        {
            let probe = rusqlite::Connection::open(&path)?;
            probe.busy_timeout(Duration::ZERO)?;
            let blocked =
                probe.query_row("SELECT count(*) FROM items", [], |row| row.get::<_, i64>(0));
            anyhow::ensure!(
                matches!(blocked, Err(rusqlite::Error::SqliteFailure(error, _)) if error.code == rusqlite::ErrorCode::DatabaseBusy),
                "SQLite lock did not block an independent read"
            );
        }
        let startup = tokio::time::timeout(Duration::from_millis(250), fixture.query.start()).await;
        anyhow::ensure!(
            startup.is_err(),
            "startup did not remain blocked on the SQLite read: {startup:?}"
        );
        anyhow::ensure!(
            fixture.query.observed()?.lifecycle == ComponentLifecycle::Starting,
            "cancelled startup wait did not retain the accepted initialization"
        );
        match fixture.provider.freeze_for_retirement() {
            Err(error) => anyhow::ensure!(
                error
                    .downcast_ref::<drasi_computation_plugin_sdk::transport::Failure>()
                    .is_some_and(
                        |failure| failure.code == drasi_computation_plugin_abi::status::BUSY
                    ),
                "provider was not waiting in its real snapshot operation: {error:#}"
            ),
            Ok(held) => {
                held.resume();
                anyhow::bail!("blocked snapshot incorrectly reported completed provider cleanup");
            }
        }
        assert_unpublished(&fixture).await?;

        let stop = fixture.query.stop();
        tokio::pin!(stop);
        let early_stop = tokio::time::timeout(Duration::from_millis(100), &mut stop).await;
        anyhow::ensure!(
            early_stop.is_err(),
            "query stop completed while the SQLite read was still blocked: {early_stop:?}"
        );
        anyhow::ensure!(
            fixture.query.observed()?.lifecycle != ComponentLifecycle::Stopped,
            "query reported stopped before its provider worker could finish"
        );
        anyhow::ensure!(
            fixture.provider.snapshot().await.is_err(),
            "unfinished provider cleanup allowed another snapshot"
        );
        anyhow::ensure!(
            fixture.assert_provider_stopped().is_err(),
            "unfinished provider cleanup allowed retirement"
        );
        assert_unpublished(&fixture).await?;

        lock.release().await?;
        tokio::time::timeout(Duration::from_secs(5), &mut stop)
            .await
            .context("query stop did not finish after releasing the SQLite lock")??;
        anyhow::ensure!(
            fixture.query.observed()?.lifecycle == ComponentLifecycle::Stopped,
            "query stop did not publish a stopped lifecycle"
        );
        fixture.assert_provider_stopped()?;
        assert_unpublished(&fixture).await?;
        anyhow::ensure!(
            fixture.live.observed()?.lifecycle == ComponentLifecycle::Running,
            "stopping query initialization stopped the independent source"
        );

        fixture.query.start().await?;
        let restarted = fixture.snapshot().await?;
        let identities = assert_rows(&restarted, &[(1, 10), (2, 20)])?;
        assert_eq!(restarted.cursor().sequence(), 0);
        fixture.assert_provider_stopped()?;

        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()?;
        client
            .put(format!("http://127.0.0.1:{rest_port}/api/tables/items/1"))
            .json(&json!({"amount":42}))
            .send()
            .await?
            .error_for_status()?;
        assert_database(&path, &[(1, 42), (2, 20)])?;
        let update = fixture.output(1).await?;
        match QueryChangeCodec::decode_evaluation(&update)?.as_slice() {
            [QueryPartEvaluationContext::Updating { before, after, .. }] => {
                assert_eq!(before, &values(1, 10));
                assert_eq!(after, &values(1, 42));
            }
            other => {
                anyhow::bail!("unexpected captured update after interrupted bootstrap: {other:?}")
            }
        }
        assert_eq!(update.changes().operations()[0].identity(), &identities[&1]);
        let current = fixture.snapshot().await?;
        assert_eq!(assert_rows(&current, &[(1, 42), (2, 20)])?, identities);
        assert_eq!(current.cursor().sequence(), 1);
        assert_eq!(current.cursor().epoch(), restarted.cursor().epoch());
        Ok::<(), anyhow::Error>(())
    };
    let result = tokio::time::timeout(Duration::from_secs(20), exercise).await;
    let cleanup = tokio::time::timeout(Duration::from_secs(10), fixture.core.shutdown()).await;
    result??;
    cleanup??;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn interrupted_sqlite_bootstrap_stops_and_restarts_current_thread() -> Result<()> {
    interrupted_bootstrap_then_capture().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn interrupted_sqlite_bootstrap_stops_and_restarts_multi_thread() -> Result<()> {
    interrupted_bootstrap_then_capture().await
}
