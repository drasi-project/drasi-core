// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::Result;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_lib::computation::v1::*;
use futures_util::StreamExt;
use std::sync::Arc;
use tokio::sync::Notify;

fn settings(database: &Database) -> MySqlConfig {
    let mut config = database.settings.clone();
    config.start = MySqlStartPosition::Snapshot {
        lock_timeout_ms: NonZeroU64::new(2000).unwrap(),
    };
    config
}

fn paired(
    config: MySqlConfig,
    owner: Arc<QuerySourceProgress>,
) -> Result<(MySqlSource, Arc<MySqlSnapshot>)> {
    MySqlSource::coordinated(
        ComponentId::try_new("source")?,
        StreamId::try_new("changes")?,
        config,
        owner,
    )
}

#[test]
fn native_snapshot_requires_opt_in_and_the_actual_paired_owner() -> Result<()> {
    let owner = progress()?;
    let mut config = super::config(3306, MySqlOutput::Transactions);
    config.replay = Some(MySqlRetention::UntilProcessed);
    config.start = MySqlStartPosition::Snapshot {
        lock_timeout_ms: NonZeroU64::new(2000).unwrap(),
    };
    let (_source, snapshot) = paired(config.clone(), owner.clone())?;
    assert!(MySqlSource::new(
        ComponentId::try_new("source")?,
        StreamId::try_new("changes")?,
        config.clone(),
        Some(owner.clone())
    )
    .is_err());
    assert!(MySqlSource::with_snapshot(
        ComponentId::try_new("source")?,
        StreamId::try_new("changes")?,
        config.clone(),
        progress()?,
        snapshot.clone()
    )
    .is_err());
    assert!(MySqlSource::with_snapshot(
        ComponentId::try_new("other")?,
        StreamId::try_new("changes")?,
        config.clone(),
        owner.clone(),
        snapshot
    )
    .is_err());
    config.start = MySqlStartPosition::Position {
        file: "mysql-bin.000001".into(),
        position: 4,
    };
    assert!(paired(config, owner).is_err());
    Ok(())
}

fn rows(query: &ContinuousQueryTransformer) -> Result<Vec<serde_json::Value>> {
    let mut rows = query
        .results()
        .snapshot()?
        .rows
        .values()
        .map(|record| {
            Ok(QueryChangeCodec::row_values_to_json(
                &QueryChangeCodec::decode_row(record)?.values,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    rows.sort_by_key(|row| row["id"].as_i64());
    Ok(rows)
}

struct Paused {
    inner: Arc<MySqlSnapshot>,
    prepared: Arc<Notify>,
    release: Arc<Notify>,
    exit_after: Option<usize>,
}

#[async_trait]
impl ComputationBootstrapProvider for Paused {
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        self.inner.snapshot().await
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        let snapshot = self.inner.snapshot_with_state(state).await?;
        self.prepared.notify_one();
        let release = self.release.clone();
        let exit_after = self.exit_after;
        let mut count = 0;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(
                futures_util::stream::once(async move {
                    release.notified().await;
                    snapshot.changes.inspect(move |_| {
                        count += 1;
                        if exit_after == Some(count) {
                            std::process::exit(86);
                        }
                    })
                })
                .flatten(),
            ),
            watermarks: snapshot.watermarks,
        })
    }
    async fn complete_snapshot(&self) -> Result<Vec<BootstrapWatermark>> {
        self.inner.complete_snapshot().await
    }
    fn completion_state(&self) -> Result<Option<Bytes>> {
        self.inner.completion_state()
    }
    async fn stop(&self) -> Result<()> {
        self.inner.stop().await
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_snapshot_releases_global_lock_before_scan_and_hands_over_without_gaps() -> Result<()>
{
    let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
    database
        .admin
        .execute(
            "INSERT INTO items VALUES (1, 'initial'), (2, 'deleted'), (4, 'four'), (5, 'five')",
        )
        .await?;
    let config = settings(&database);
    let path = tempfile::tempdir()?;
    let owner = progress()?;
    let (mut source, bootstrap) = paired(config.clone(), owner.clone())?;
    let prepared = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut query = query(path.path(), owner.clone())
        .await?
        .with_bootstrap(Arc::new(Paused {
            inner: bootstrap,
            prepared: prepared.clone(),
            release: release.clone(),
            exit_after: None,
        }));
    let writes = async {
        prepared.notified().await;
        tokio::time::timeout(Duration::from_secs(2), async {
            database.admin.execute("BEGIN").await?;
            database
                .admin
                .execute("UPDATE items SET value='final' WHERE id=1")
                .await?;
            database
                .admin
                .execute("DELETE FROM items WHERE id=2")
                .await?;
            database
                .admin
                .execute("INSERT INTO items VALUES (3, 'during snapshot')")
                .await?;
            database.admin.execute("COMMIT").await?;
            Result::<()>::Ok(())
        })
        .await
        .context("write remained blocked during the paused scan")?
        .context("concurrent snapshot write failed")?;
        let mut ddl = connection::Connection::connect(
            &config.connection,
            config.max_protocol_bytes.get(),
            config.timeout(),
        )
        .await?;
        ddl.execute("SET SESSION lock_wait_timeout=1").await?;
        let error = ddl
            .execute("ALTER TABLE items ADD COLUMN premature INT")
            .await
            .expect_err("snapshot metadata lock");
        assert!(
            error
                .downcast_ref::<MySqlServerError>()
                .is_some_and(|error| error.code == 1205),
            "{error:#}"
        );
        ddl.close().await?;
        release.notify_one();
        Result::<()>::Ok(())
    };
    let (started, written, streamed) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(query.start(), writes, source.start())
    })
    .await?;
    started?;
    written?;
    streamed?;
    assert_eq!(query.results().snapshot()?.rows.len(), 4);
    assert!(owner.snapshot().bootstrap_complete);
    assert_eq!(
        rows(&query)?,
        vec![
            serde_json::json!({"id":1,"value":"initial"}),
            serde_json::json!({"id":2,"value":"deleted"}),
            serde_json::json!({"id":4,"value":"four"}),
            serde_json::json!({"id":5,"value":"five"}),
        ]
    );
    let output = next(&mut source).await?;
    assert_eq!(
        SourceTransactionCodec::decode(&output.envelope, config.transactions)?
            .into_changes()
            .len(),
        3
    );
    apply(&mut query, output).await?;
    assert_eq!(
        rows(&query)?,
        vec![
            serde_json::json!({"id":1,"value":"final"}),
            serde_json::json!({"id":3,"value":"during snapshot"}),
            serde_json::json!({"id":4,"value":"four"}),
            serde_json::json!({"id":5,"value":"five"}),
        ]
    );
    source.stop().await?;
    query.stop().await?;
    drop(query);
    drop(source);
    database
        .admin
        .execute("INSERT INTO items VALUES (6, 'offline')")
        .await?;
    let owner = progress()?;
    let (mut source, bootstrap) = paired(config, owner.clone())?;
    let mut query = super::query(path.path(), owner)
        .await?
        .with_bootstrap(bootstrap);
    query.start().await?;
    assert_eq!(query.results().snapshot()?.rows.len(), 4);
    source.start().await?;
    apply(&mut query, next(&mut source).await?).await?;
    assert_eq!(query.results().snapshot()?.rows.len(), 5);
    source.stop().await?;
    query.stop().await?;
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_snapshot_retains_cleanup_across_cancelled_and_timed_out_stop() -> Result<()> {
    let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
    database
        .admin
        .execute("INSERT INTO items VALUES (1,'one'),(2,'two'),(3,'three')")
        .await?;
    let mut config = settings(&database);
    config.io_timeout_ms = NonZeroU64::new(1000).unwrap();
    let path = tempfile::tempdir()?;
    let owner = progress()?;
    let (_source, bootstrap) = paired(config, owner.clone())?;
    let prepared = Arc::new(Notify::new());
    let mut query = query(path.path(), owner.clone())
        .await?
        .with_bootstrap(Arc::new(Paused {
            inner: bootstrap.clone(),
            prepared: prepared.clone(),
            release: Arc::new(Notify::new()),
            exit_after: None,
        }));
    {
        let start = query.start();
        tokio::pin!(start);
        tokio::select! {
            result = &mut start => anyhow::bail!("paused snapshot unexpectedly completed: {result:?}"),
            paused = async {
                prepared.notified().await;
                database._container.pause().await
            } => paused?,
        }
    }
    let result = async {
        anyhow::ensure!(
            tokio::time::timeout(Duration::from_millis(50), bootstrap.stop())
                .await
                .is_err(),
            "cancelled cleanup completed against a paused server"
        );
        let error = tokio::time::timeout(Duration::from_secs(3), bootstrap.stop())
            .await?
            .expect_err("unobserved server cleanup");
        anyhow::ensure!(
            matches!(
                error.downcast_ref::<drasi_lib::context::workers::WorkerCleanupError>(),
                Some(drasi_lib::context::workers::WorkerCleanupError::TimedOut { .. })
            ),
            "{error:#}"
        );
        anyhow::ensure!(
            query.start().await.is_err(),
            "replacement started before cleanup"
        );
        anyhow::ensure!(
            !owner.snapshot().bootstrap_complete && owner.snapshot().checkpoints.is_empty(),
            "cancelled snapshot advanced progress"
        );
        Result::<()>::Ok(())
    }
    .await;
    database._container.unpause().await?;
    let cleanup = bootstrap.stop().await;
    query.stop().await?;
    result?;
    anyhow::ensure!(
        cleanup.is_err(),
        "cleanup discarded its earlier I/O failure"
    );
    let sessions = database.admin.query("SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID()").await?;
    anyhow::ensure!(
        catalog::field(&sessions.values[0], 0)? == "0",
        "snapshot sessions still active"
    );
    database.admin.close().await?;
    Ok(())
}

async fn wait_for_statement(admin: &mut connection::Connection, statement: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let rows = admin
                .query(&format!(
                    "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE INFO='{statement}'"
                ))
                .await?;
            if catalog::field(&rows.values[0], 0)? != "0" {
                return Result::<()>::Ok(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_snapshot_lock_wait_timeout_and_cancellation_retire_pending_global_locks(
) -> Result<()> {
    for cancel in [false, true] {
        let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
        database
            .admin
            .execute("CREATE TABLE unrelated (id INT PRIMARY KEY)")
            .await?;
        database
            .admin
            .execute("INSERT INTO items VALUES (1,'initial')")
            .await?;
        let mut config = settings(&database);
        config.start = MySqlStartPosition::Snapshot {
            lock_timeout_ms: NonZeroU64::new(if cancel { 2000 } else { 300 }).unwrap(),
        };
        let mut blocker = connection::Connection::connect(
            &config.connection,
            config.max_protocol_bytes.get(),
            Duration::from_secs(40),
        )
        .await?;
        let blocker_id = blocker.connection_id;
        let blocked = tokio::spawn(async move {
            let result = blocker.query("SELECT SLEEP(30) FROM items").await;
            let cleanup = blocker.close().await;
            (result, cleanup)
        });
        wait_for_statement(&mut database.admin, "SELECT SLEEP(30) FROM items").await?;
        let path = tempfile::tempdir()?;
        let owner = progress()?;
        let (_source, bootstrap) = paired(config, owner.clone())?;
        let mut query = query(path.path(), owner.clone())
            .await?
            .with_bootstrap(bootstrap);
        let result = async {
            if cancel {
                tokio::select! {
                    result = query.start() => anyhow::bail!("blocked snapshot unexpectedly completed: {result:?}"),
                    ready = wait_for_statement(&mut database.admin, "FLUSH TABLES WITH READ LOCK") => ready?,
                }
            } else {
                let error = tokio::time::timeout(Duration::from_secs(5), query.start()).await?
                    .expect_err("global lock acquisition deadline");
                anyhow::ensure!(error.to_string().contains("lock deadline"), "{error:#}");
            }
            query.stop().await?;
            tokio::time::timeout(Duration::from_secs(2), database.admin.execute("INSERT INTO unrelated VALUES (2)")).await??;
            let sessions = database.admin.query(&format!("SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID NOT IN (CONNECTION_ID(),{blocker_id})")).await?;
            anyhow::ensure!(catalog::field(&sessions.values[0],0)? == "0", "snapshot sessions still active");
            anyhow::ensure!(owner.snapshot().checkpoints.is_empty(), "failed snapshot advanced progress");
            Result::<()>::Ok(())
        }.await;
        let mut cleanup_connection = connection::Connection::connect(
            &database.settings.connection,
            database.settings.max_protocol_bytes.get(),
            database.settings.timeout(),
        )
        .await?;
        cleanup_connection
            .execute(&format!("KILL CONNECTION {blocker_id}"))
            .await?;
        let (blocked_result, cleanup) = blocked.await?;
        assert!(blocked_result.is_err());
        cleanup?;
        cleanup_connection.close().await?;
        result.with_context(|| format!("blocked snapshot cancellation={cancel}"))?;
        database
            .admin
            .execute("INSERT INTO items VALUES (2,'unblocked')")
            .await?;
        database.admin.close().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_snapshot_reused_binlog_contents_fail_before_live_admission() -> Result<()> {
    let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
    database
        .admin
        .execute("INSERT INTO items VALUES (1,'initial')")
        .await?;
    let config = settings(&database);
    let path = tempfile::tempdir()?;
    let owner = progress()?;
    let (source, bootstrap) = paired(config.clone(), owner.clone())?;
    let mut query = query(path.path(), owner.clone())
        .await?
        .with_bootstrap(bootstrap);
    query.start().await?;
    let saved = owner.snapshot().checkpoints[&SourceProgressKey::Source("source".into())].clone();
    let position: super::super::binlog::Position = serde_json::from_slice(
        saved
            .source_position
            .as_ref()
            .context("snapshot position")?,
    )?;
    query.stop().await?;
    drop(query);
    drop(source);
    let reset = if database.admin.version >= (8, 4, 0) {
        "RESET BINARY LOGS AND GTIDS"
    } else {
        "RESET MASTER"
    };
    database.admin.execute(reset).await?;
    let file_number = super::super::file_number(&position.file)?;
    anyhow::ensure!(file_number < 100, "unexpected fixture binlog count");
    for _ in 1..file_number {
        database.admin.execute("FLUSH BINARY LOGS").await?;
    }

    for index in 0..64 {
        database
            .admin
            .execute(&format!(
                "INSERT INTO items VALUES ({},'replacement-{index}')",
                index + 100
            ))
            .await?;
    }
    let owner = progress()?;
    let (mut source, bootstrap) = paired(config, owner.clone())?;
    let mut query = super::query(path.path(), owner.clone())
        .await?
        .with_bootstrap(bootstrap);
    query.start().await?;
    let error = source.start().await.expect_err("reused snapshot binlog");
    anyhow::ensure!(
        error.to_string().contains("different contents")
            || error.to_string().contains("not an event boundary"),
        "{error:#}"
    );
    assert_eq!(
        owner.snapshot().checkpoints[&SourceProgressKey::Source("source".into())],
        saved
    );
    assert_eq!(
        rows(&query)?,
        vec![serde_json::json!({"id":1,"value":"initial"})]
    );
    source.stop().await?;
    query.stop().await?;
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_snapshot_bounds_expanded_rows_without_completing_initialization() -> Result<()> {
    let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
    database
        .admin
        .execute("ALTER TABLE items ADD COLUMN payload BLOB")
        .await?;
    database
        .admin
        .execute("INSERT INTO items VALUES (1,'large',REPEAT(CHAR(255),700))")
        .await?;
    let mut config = settings(&database);
    config.max_protocol_bytes = NonZeroUsize::new(2048).unwrap();
    let path = tempfile::tempdir()?;
    let owner = progress()?;
    let (_source, snapshot) = paired(config.clone(), owner.clone())?;
    let mut query = query(path.path(), owner.clone())
        .await?
        .with_bootstrap(snapshot);
    let error = query.start().await.expect_err("expanded snapshot row");
    anyhow::ensure!(
        error.to_string().contains("decoded snapshot row"),
        "{error:#}"
    );
    assert!(!owner.snapshot().bootstrap_complete);
    assert!(owner.snapshot().checkpoints.is_empty());
    query.stop().await?;
    drop(query);
    config.max_protocol_bytes = NonZeroUsize::new(1024 * 1024).unwrap();
    config.start = MySqlStartPosition::Snapshot {
        lock_timeout_ms: NonZeroU64::new(3000).unwrap(),
    };
    let owner = progress()?;
    let (mut source, snapshot) = paired(config, owner.clone())?;
    let mut query = query_with_recovery(path.path(), owner, QueryRecoveryPolicy::AutoReset)
        .await?
        .with_bootstrap(snapshot);
    query.start().await?;
    assert_eq!(
        rows(&query)?,
        vec![serde_json::json!({"id":1,"value":"large"})]
    );
    source.start().await?;
    source.stop().await?;
    query.stop().await?;
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "invoked by the process-exit qualification parent"]
async fn native_snapshot_process_exit_helper() -> Result<()> {
    let Ok(point) = std::env::var("DRASI_MYSQL_SNAPSHOT_CRASH") else {
        return Ok(());
    };
    anyhow::ensure!(
        matches!(point.as_str(), "partial" | "completed"),
        "unknown snapshot crash point"
    );
    let config: MySqlConfig = serde_json::from_str(&std::env::var("DRASI_MYSQL_CRASH_CONFIG")?)?;
    let path = std::env::var("DRASI_MYSQL_CRASH_PATH")?;
    let owner = progress()?;
    let (_source, bootstrap) = paired(config, owner.clone())?;
    let release = Arc::new(Notify::new());
    release.notify_one();
    let mut query = query(std::path::Path::new(&path), owner)
        .await?
        .with_bootstrap(Arc::new(Paused {
            inner: bootstrap,
            prepared: Arc::new(Notify::new()),
            release,
            exit_after: (point == "partial").then_some(2),
        }));
    query.start().await?;
    anyhow::ensure!(
        point == "completed",
        "partial crash helper missed its boundary"
    );
    std::process::exit(86);
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_snapshot_process_exit_recovers_completed_state_and_explicitly_restarts_partial_state(
) -> Result<()> {
    for point in ["partial", "completed"] {
        let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
        database
            .admin
            .execute("INSERT INTO items VALUES (1,'one'),(2,'two'),(3,'three'),(4,'four')")
            .await?;
        let config = settings(&database);
        let path = tempfile::tempdir()?;
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new(std::env::current_exe()?)
                .args([
                    "--ignored",
                    "--exact",
                    "native::tests::snapshot::native_snapshot_process_exit_helper",
                    "--nocapture",
                ])
                .env("DRASI_MYSQL_SNAPSHOT_CRASH", point)
                .env("DRASI_MYSQL_CRASH_CONFIG", serde_json::to_string(&config)?)
                .env("DRASI_MYSQL_CRASH_PATH", path.path())
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        anyhow::ensure!(
            output.status.code() == Some(86),
            "snapshot crash missed {point}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        database
            .admin
            .execute("INSERT INTO items VALUES (5,'offline')")
            .await?;
        if point == "partial" {
            let owner = progress()?;
            let (_source, bootstrap) = paired(config.clone(), owner.clone())?;
            let mut query = query(path.path(), owner).await?.with_bootstrap(bootstrap);
            let error = query
                .start()
                .await
                .expect_err("partial snapshot must not resume as complete");
            assert!(matches!(
                error.downcast_ref::<QueryRecoveryError>(),
                Some(QueryRecoveryError::IncompleteBootstrap)
            ));
            query.stop().await?;
        }
        let owner = progress()?;
        let (mut source, bootstrap) = paired(config, owner.clone())?;
        let policy = if point == "partial" {
            QueryRecoveryPolicy::AutoReset
        } else {
            QueryRecoveryPolicy::Strict
        };
        let mut query = query_with_recovery(path.path(), owner, policy)
            .await?
            .with_bootstrap(bootstrap);
        query.start().await?;
        assert_eq!(
            query.results().snapshot()?.rows.len(),
            if point == "partial" { 5 } else { 4 }
        );
        source.start().await?;
        if point == "completed" {
            apply(&mut query, next(&mut source).await?).await?;
        }
        database
            .admin
            .execute("INSERT INTO items VALUES (6,'live')")
            .await?;
        apply(&mut query, next(&mut source).await?).await?;
        assert_eq!(query.results().snapshot()?.rows.len(), 6);
        source.stop().await?;
        query.stop().await?;
        database.admin.close().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_snapshot_checkpoint_replays_without_rescanning_after_rotation() -> Result<()> {
    let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
    database
        .admin
        .execute("INSERT INTO items VALUES (1,'initial')")
        .await?;
    let config = settings(&database);
    let path = tempfile::tempdir()?;
    let owner = progress()?;
    let (source, bootstrap) = paired(config.clone(), owner.clone())?;
    let mut query = query(path.path(), owner).await?.with_bootstrap(bootstrap);
    query.start().await?;
    query.stop().await?;
    drop(query);
    drop(source);
    database.admin.execute("FLUSH BINARY LOGS").await?;
    database
        .admin
        .execute("INSERT INTO items VALUES (2,'offline')")
        .await?;
    let owner = progress()?;
    let (mut source, bootstrap) = paired(config, owner.clone())?;
    let mut query = super::query(path.path(), owner)
        .await?
        .with_bootstrap(bootstrap);
    query.start().await?;
    assert_eq!(query.results().snapshot()?.rows.len(), 1);
    source.start().await?;
    apply(&mut query, next(&mut source).await?).await?;
    assert_eq!(query.results().snapshot()?.rows.len(), 2);
    source.stop().await?;
    query.stop().await?;
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_snapshot_cancelled_start_retires_sessions_and_blocks_incomplete_recovery(
) -> Result<()> {
    let mut database = Database::new(MySqlRetention::UntilProcessed).await?;
    database
        .admin
        .execute("INSERT INTO items VALUES (1,'initial'),(2,'two'),(3,'three'),(4,'four')")
        .await?;
    let config = settings(&database);
    let path = tempfile::tempdir()?;
    let owner = progress()?;
    let (_source, bootstrap) = paired(config.clone(), owner.clone())?;
    let prepared = Arc::new(Notify::new());
    let mut query = query(path.path(), owner.clone())
        .await?
        .with_bootstrap(Arc::new(Paused {
            inner: bootstrap,
            prepared: prepared.clone(),
            release: Arc::new(Notify::new()),
            exit_after: None,
        }));
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            result = query.start() => anyhow::bail!("snapshot unexpectedly completed: {result:?}"),
            _ = prepared.notified() => Result::<()>::Ok(()),
        }
    })
    .await??;
    query.stop().await?;
    assert!(!owner.snapshot().bootstrap_complete);
    assert!(owner.snapshot().checkpoints.is_empty());
    let rows = database.admin.query("SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID()").await?;
    assert_eq!(catalog::field(&rows.values[0], 0)?, "0");
    database
        .admin
        .execute("ALTER TABLE items ADD COLUMN after_stop INT")
        .await?;
    drop(query);
    let owner = progress()?;
    let (_source, bootstrap) = paired(config, owner.clone())?;
    let mut query = super::query(path.path(), owner)
        .await?
        .with_bootstrap(bootstrap);
    let error = query.start().await.expect_err("incomplete initialization");
    assert!(
        matches!(
            error.downcast_ref::<QueryRecoveryError>(),
            Some(QueryRecoveryError::IncompleteBootstrap)
        ),
        "{error:#}"
    );
    query.stop().await?;
    database.admin.close().await?;
    Ok(())
}
