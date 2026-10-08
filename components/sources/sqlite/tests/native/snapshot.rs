// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_source_sqlite::native::SqliteSnapshot;
use futures::StreamExt;

fn seed(path: &Path, sql: &str) -> Result<()> {
    let connection = rusqlite::Connection::open(path.join("source.db"))?;
    connection.execute_batch(sql)?;
    Ok(())
}

fn paired(
    settings: SqliteConfig,
    owner: Arc<QuerySourceProgress>,
) -> Result<(SqliteSource, Arc<SqliteSnapshot>)> {
    SqliteSource::coordinated(id("source"), stream("changes"), settings, owner)
}

#[tokio::test(flavor = "current_thread")]
async fn empty_snapshot_commits_a_real_zero_boundary_before_the_first_live_transaction(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let owner = progress();
    let (mut producer, snapshot) = paired(config(directory.path()), owner.clone())?;
    let mut consumer = query(directory.path(), owner.clone())
        .await?
        .with_bootstrap(snapshot);
    consumer.start().await?;
    assert!(rows(&consumer).is_empty());
    let checkpoint = &owner.snapshot().checkpoints[&SourceProgressKey::Source("source".into())];
    assert_eq!(checkpoint.sequence, 0);
    assert!(
        checkpoint.source_position.is_some(),
        "zero is a real initialized journal position, not a fabricated cursor"
    );
    producer.start().await?;
    producer.handle().execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'first')").await?;
    apply(&mut consumer, next(&mut producer).await?).await?;
    assert_eq!(
        rows(&consumer),
        [serde_json::json!({"id":1,"value":"first"})]
    );
    assert_eq!(
        owner.snapshot().checkpoints[&SourceProgressKey::Source("source".into())].sequence,
        1
    );
    producer.stop().await?;
    consumer.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn snapshot_and_live_changes_share_keys_and_resume_without_reinitializing() -> Result<()> {
    let directory = tempfile::tempdir()?;
    seed(
        directory.path(),
        "CREATE TABLE items(id INTEGER,a BLOB,b TEXT,value TEXT,PRIMARY KEY(a,b)) WITHOUT ROWID;
        INSERT INTO items VALUES(1,X'61','a:b|c','one'),(2,'a','a:b|c','two'),
        (3,X'62','three','three'),(4,X'63','four','four');",
    )?;
    let mut settings = config(directory.path());
    settings.transactions.max_changes = size(2);
    let owner = progress();
    let (mut producer, snapshot) = paired(settings.clone(), owner.clone())?;
    let handle = producer.handle();
    assert!(handle.query("SELECT 1").await.is_err());
    let mut consumer = query(directory.path(), owner.clone())
        .await?
        .with_bootstrap(snapshot);
    consumer.start().await?;
    assert_eq!(
        rows(&consumer).len(),
        4,
        "snapshot is streamed, not capped at one transaction's row count"
    );
    assert!(owner.snapshot().bootstrap_complete);
    assert_eq!(
        owner.snapshot().checkpoints[&SourceProgressKey::Source("source".into())].sequence,
        0
    );
    producer.start().await?;
    handle
        .execute_batch("UPDATE items SET value='final' WHERE id=1; DELETE FROM items WHERE id=2")
        .await?;
    apply(&mut consumer, next(&mut producer).await?).await?;
    assert_eq!(
        rows(&consumer),
        [
            serde_json::json!({"id":1,"value":"final"}),
            serde_json::json!({"id":3,"value":"three"}),
            serde_json::json!({"id":4,"value":"four"}),
        ]
    );
    producer.stop().await?;
    consumer.stop().await?;
    drop(producer);
    drop(consumer);

    let owner = progress();
    let (mut producer, snapshot) = paired(settings, owner.clone())?;
    let mut consumer = query(directory.path(), owner)
        .await?
        .with_bootstrap(snapshot);
    consumer.start().await?;
    producer.start().await?;
    assert_eq!(rows(&consumer).len(), 3);
    assert!(
        timeout(Duration::from_millis(30), producer.next())
            .await
            .is_err(),
        "a completed snapshot must not replay as new input"
    );
    producer
        .handle()
        .execute("UPDATE items SET value='resumed' WHERE id=3")
        .await?;
    apply(&mut consumer, next(&mut producer).await?).await?;
    assert_eq!(rows(&consumer).len(), 3);
    assert!(rows(&consumer).contains(&serde_json::json!({"id":3,"value":"resumed"})));
    producer.stop().await?;
    consumer.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_initial_rows_require_explicit_reset_and_larger_limits() -> Result<()> {
    let directory = tempfile::tempdir()?;
    seed(
        directory.path(),
        "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT);
        INSERT INTO items VALUES(1,'small'),(2,replace(hex(zeroblob(2000)),'00','xx'));",
    )?;
    let owner = progress();
    let mut settings = config(directory.path());
    settings.transactions.max_bytes = size(1024);
    let (mut producer, snapshot) = paired(settings, owner.clone())?;
    let mut consumer = query(directory.path(), owner.clone())
        .await?
        .with_bootstrap(snapshot);
    consumer.start().await.expect_err("oversized snapshot row");
    assert!(!owner.snapshot().ready);
    assert!(!owner.snapshot().bootstrap_complete);
    assert!(producer
        .handle()
        .execute("INSERT INTO items VALUES(3,'unadmitted')")
        .await
        .is_err());
    producer.stop().await?;
    consumer.stop().await?;
    drop(producer);
    drop(consumer);

    let owner = progress();
    let (producer, snapshot) = paired(config(directory.path()), owner.clone())?;
    let mut consumer = query(directory.path(), owner)
        .await?
        .with_bootstrap(snapshot);
    let error = consumer
        .start()
        .await
        .expect_err("incomplete bootstrap requires reset");
    assert!(matches!(
        error.downcast_ref::<QueryRecoveryError>(),
        Some(QueryRecoveryError::IncompleteBootstrap)
    ));
    consumer.stop().await?;
    drop(producer);
    drop(consumer);

    let owner = progress();
    let (mut producer, snapshot) = paired(config(directory.path()), owner.clone())?;
    let mut consumer = query_with_recovery(directory.path(), owner, QueryRecoveryPolicy::AutoReset)
        .await?
        .with_bootstrap(snapshot);
    consumer.start().await?;
    assert_eq!(rows(&consumer).len(), 2);
    producer.start().await?;
    producer
        .handle()
        .execute("UPDATE items SET value='recovered' WHERE id=2")
        .await?;
    apply(&mut consumer, next(&mut producer).await?).await?;
    assert_eq!(
        rows(&consumer),
        [
            serde_json::json!({"id":1,"value":"small"}),
            serde_json::json!({"id":2,"value":"recovered"})
        ]
    );
    producer.stop().await?;
    consumer.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn coordinated_journals_reject_mode_changes_and_missing_initialization_identity() -> Result<()>
{
    for fault in ["plain", "marker", "journal", "binding", "reset"] {
        let directory = tempfile::tempdir()?;
        seed(directory.path(), "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'initial');")?;
        let owner = progress();
        let (producer, snapshot) = paired(config(directory.path()), owner.clone())?;
        let mut consumer = query(directory.path(), owner)
            .await?
            .with_bootstrap(snapshot);
        consumer.start().await?;
        consumer.stop().await?;
        if fault == "reset" {
            consumer.deprovision().await?;
        }
        drop(producer);
        drop(consumer);
        match fault {
            "marker" => seed(directory.path(), "DROP TABLE __drasi_native_bootstrap_v1")?,
            "journal" => seed(directory.path(), "DROP TABLE __drasi_native_bootstrap_v1; DROP TABLE __drasi_native_outbox_v1; DROP TABLE __drasi_native_state_v1")?,
            _ => {}
        }
        let owner = progress();
        let mut settings = config(directory.path());
        if fault == "binding" {
            settings.tables = vec!["items".into()];
        }
        let (mut producer, snapshot) = paired(settings, owner.clone())?;
        let mut consumer = query(directory.path(), owner.clone())
            .await?
            .with_bootstrap(snapshot);
        if fault == "binding" || fault == "reset" {
            consumer
                .start()
                .await
                .expect_err("changed or missing initial source binding");
        } else {
            consumer.start().await?;
            if fault == "plain" {
                producer = source(config(directory.path()), owner)?;
            }
            producer
                .start()
                .await
                .expect_err("lost initialization identity or mode change");
        }
        producer.stop().await?;
        consumer.stop().await?;
        let connection = rusqlite::Connection::open(directory.path().join("source.db"))?;
        assert_eq!(
            connection.query_row("SELECT count(*) FROM items", [], |row| row
                .get::<_, usize>(0))?,
            1
        );
    }
    Ok(())
}

struct ObservedSnapshot {
    inner: Arc<SqliteSnapshot>,
    started: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl ComputationBootstrapProvider for ObservedSnapshot {
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
        self.started.notify_one();
        Ok(snapshot)
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
async fn cancelled_snapshot_keeps_the_client_lease_until_blocked_sqlite_cleanup_finishes(
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    seed(directory.path(), "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'initial');")?;
    let blocker = rusqlite::Connection::open(directory.path().join("source.db"))?;
    blocker.execute_batch("BEGIN IMMEDIATE")?;
    let mut settings = config(directory.path());
    settings.shutdown_timeout_ms = NonZeroU64::new(20).context("timeout")?;
    let owner = progress();
    let client = Arc::new(SqliteClientResource::new(id("source"), settings.clone())?);
    let (producer, snapshot) = paired(settings.clone(), owner.clone())?;
    let producer = producer.with_client_resource(client.clone())?;
    let started = Arc::new(tokio::sync::Notify::new());
    let mut consumer = query(directory.path(), owner.clone())
        .await?
        .with_bootstrap(Arc::new(ObservedSnapshot {
            inner: snapshot,
            started: started.clone(),
        }));
    {
        let start = consumer.start();
        tokio::pin!(start);
        tokio::select! {
            result = &mut start => anyhow::bail!("snapshot unexpectedly finished while database was locked: {result:?}"),
            _ = started.notified() => {}
        }
    }
    drop(producer);
    let error = consumer
        .stop()
        .await
        .expect_err("blocking SQLite still owns its lease");
    assert!(
        error
            .downcast_ref::<drasi_lib::context::workers::WorkerCleanupError>()
            .is_some(),
        "{error:#}"
    );
    assert!(source(settings.clone(), owner.clone())?
        .with_client_resource(client.clone())
        .is_err());
    blocker.execute_batch("ROLLBACK")?;
    drop(blocker);
    timeout(Duration::from_secs(3), async {
        loop {
            match consumer.stop().await {
                Ok(()) => return Ok(()),
                Err(error)
                    if matches!(
                        error.downcast_ref::<drasi_lib::context::workers::WorkerCleanupError>(),
                        Some(drasi_lib::context::workers::WorkerCleanupError::TimedOut { .. })
                    ) => {}
                Err(error) => return Err(error),
            }
        }
    })
    .await??;
    let replacement = source(settings, owner)?.with_client_resource(client)?;
    drop(replacement);
    Ok(())
}

struct CrashSnapshot {
    inner: Arc<SqliteSnapshot>,
    mode: String,
}

struct RetainedSnapshot {
    inner: Arc<SqliteSnapshot>,
    calls: std::sync::atomic::AtomicUsize,
    saved: std::sync::Mutex<Option<ComputationBootstrapSnapshot>>,
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait]
impl ComputationBootstrapProvider for RetainedSnapshot {
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
        if self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
            *self.saved.lock().expect("test snapshot slot") = Some(snapshot);
            self.started.notify_one();
            return Ok(ComputationBootstrapSnapshot {
                changes: Box::pin(futures::stream::pending()),
                watermarks: Vec::new(),
            });
        }
        self.started.notify_one();
        self.release.notified().await;
        Ok(snapshot)
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
async fn retained_old_snapshot_stream_cannot_cancel_read_from_or_join_its_replacement() -> Result<()>
{
    for poll_old in [false, true] {
        let directory = tempfile::tempdir()?;
        seed(directory.path(), "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'one'),(2,'two'),(3,'three');")?;
        let owner = progress();
        let mut settings = config(directory.path());
        settings.output_capacity = size(1);
        let (_producer, snapshot) = paired(settings, owner.clone())?;
        let provider = Arc::new(RetainedSnapshot {
            inner: snapshot,
            calls: std::sync::atomic::AtomicUsize::new(0),
            saved: std::sync::Mutex::new(None),
            started: tokio::sync::Notify::new(),
            release: tokio::sync::Notify::new(),
        });
        let mut consumer =
            query_with_recovery(directory.path(), owner, QueryRecoveryPolicy::AutoReset)
                .await?
                .with_bootstrap(provider.clone());
        {
            let start = consumer.start();
            tokio::pin!(start);
            tokio::select! {
                result = &mut start => anyhow::bail!("held initial snapshot unexpectedly completed: {result:?}"),
                _ = provider.started.notified() => {}
            }
        }
        consumer.stop().await?;
        {
            let start = consumer.start();
            tokio::pin!(start);
            tokio::select! {
                result = &mut start => anyhow::bail!("replacement snapshot unexpectedly completed: {result:?}"),
                _ = provider.started.notified() => {}
            }
            let mut previous = provider
                .saved
                .lock()
                .expect("test snapshot slot")
                .take()
                .context("old snapshot")?;
            if poll_old {
                let error = timeout(Duration::from_secs(1), previous.changes.next())
                    .await?
                    .context("old stream must report retirement")?
                    .expect_err("old stream cannot return rows");
                assert!(error.to_string().contains("retired worker"), "{error:#}");
            }
            drop(previous);
            provider.release.notify_one();
            timeout(Duration::from_secs(5), start).await??;
        }
        assert_eq!(rows(&consumer).len(), 3);
        consumer.stop().await?;
    }
    Ok(())
}

struct CrashIntent<'a>(&'a dyn BootstrapState);

#[async_trait]
impl BootstrapState for CrashIntent<'_> {
    fn durability(&self) -> drasi_core::interface::StorageDurability {
        self.0.durability()
    }
    async fn read(&self) -> Result<Option<Bytes>> {
        self.0.read().await
    }
    async fn write(&self, state: Bytes) -> Result<()> {
        self.0.write(state).await?;
        std::process::exit(84);
    }
}

#[async_trait]
impl ComputationBootstrapProvider for CrashSnapshot {
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
        if self.mode == "intent" {
            return self.inner.snapshot_with_state(&CrashIntent(state)).await;
        }
        let mut snapshot = self.inner.snapshot_with_state(state).await?;
        if self.mode == "rows" {
            snapshot.changes = Box::pin(futures::stream::unfold(
                (snapshot.changes, false),
                |(mut changes, seen)| async move {
                    if seen {
                        std::process::exit(84);
                    }
                    changes.next().await.map(|row| (row, (changes, true)))
                },
            ));
        }
        Ok(snapshot)
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
async fn sqlite_snapshot_exit_helper() -> Result<()> {
    let Ok(mode) = std::env::var("DRASI_NATIVE_SQLITE_SNAPSHOT_CRASH") else {
        return Ok(());
    };
    let path = std::env::var("DRASI_NATIVE_SQLITE_PATH")?;
    let path = Path::new(&path);
    let owner = progress();
    let (_producer, snapshot) = paired(config(path), owner.clone())?;
    let mut consumer = query(path, owner)
        .await?
        .with_bootstrap(Arc::new(CrashSnapshot {
            inner: snapshot,
            mode: mode.clone(),
        }));
    consumer.start().await?;
    anyhow::ensure!(
        mode == "completed",
        "snapshot crash boundary was not reached"
    );
    std::process::exit(84);
}

#[tokio::test(flavor = "current_thread")]
async fn actual_process_exits_preserve_snapshot_intent_partial_rows_and_committed_handover(
) -> Result<()> {
    for mode in ["intent", "rows", "completed"] {
        let directory = tempfile::tempdir()?;
        seed(directory.path(), "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'one'),(2,'two');")?;
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "snapshot::sqlite_snapshot_exit_helper",
                "--nocapture",
            ])
            .env("DRASI_NATIVE_SQLITE_SNAPSHOT_CRASH", mode)
            .env("DRASI_NATIVE_SQLITE_PATH", directory.path())
            .kill_on_drop(true)
            .spawn()?;
        assert_eq!(
            timeout(Duration::from_secs(20), child.wait())
                .await??
                .code(),
            Some(84),
            "{mode}"
        );
        if mode != "completed" {
            let owner = progress();
            let (_producer, snapshot) = paired(config(directory.path()), owner.clone())?;
            let mut consumer = query(directory.path(), owner)
                .await?
                .with_bootstrap(snapshot);
            let error = consumer
                .start()
                .await
                .expect_err("unfinished initialization");
            assert!(
                matches!(
                    error.downcast_ref::<QueryRecoveryError>(),
                    Some(QueryRecoveryError::IncompleteBootstrap)
                ),
                "{mode}: {error:#}"
            );
            consumer.stop().await?;
        }
        let owner = progress();
        let (mut producer, snapshot) = paired(config(directory.path()), owner.clone())?;
        let policy = if mode == "completed" {
            QueryRecoveryPolicy::Strict
        } else {
            QueryRecoveryPolicy::AutoReset
        };
        let mut consumer = query_with_recovery(directory.path(), owner, policy)
            .await?
            .with_bootstrap(snapshot);
        consumer.start().await?;
        assert_eq!(
            rows(&consumer),
            [
                serde_json::json!({"id":1,"value":"one"}),
                serde_json::json!({"id":2,"value":"two"})
            ],
            "{mode}"
        );
        producer.start().await?;
        producer
            .handle()
            .execute("UPDATE items SET value='live' WHERE id=1")
            .await?;
        apply(&mut consumer, next(&mut producer).await?).await?;
        assert_eq!(
            rows(&consumer),
            [
                serde_json::json!({"id":1,"value":"live"}),
                serde_json::json!({"id":2,"value":"two"})
            ],
            "{mode}"
        );
        producer.stop().await?;
        consumer.stop().await?;
    }
    Ok(())
}
