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

use super::*;
use async_trait::async_trait;
use bytes::Bytes;
use drasi_source_postgres::native::PostgresSnapshot;
use futures::StreamExt;
use tokio::sync::Notify;

struct PausedSnapshot {
    inner: Arc<PostgresSnapshot>,
    prepared: Arc<Notify>,
    release: Arc<Notify>,
}

#[async_trait]
impl ComputationBootstrapProvider for PausedSnapshot {
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.inner.prepare_with_state(state).await
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("requires coordinated state")
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        let snapshot = self.inner.snapshot_with_state(state).await?;
        self.prepared.notify_one();
        let release = self.release.clone();
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(
                futures::stream::once(async move {
                    release.notified().await;
                    snapshot.changes
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
}

fn coordinated(
    config: PostgresTransactionConfig,
    progress: Arc<QuerySourceProgress>,
) -> Result<(PostgresTransactionSource, Arc<PostgresSnapshot>)> {
    PostgresTransactionSource::coordinated(
        ComponentId::try_new("source")?,
        StreamId::try_new("changes")?,
        config,
        progress,
    )
}

pub(super) async fn managed_slot(database: &Database) -> Result<String> {
    Ok(database
        .client()
        .query_one(
            "SELECT slot_name FROM pg_replication_slots WHERE slot_name LIKE 'native_slot\\_%'",
            &[],
        )
        .await?
        .get(0))
}

#[tokio::test(flavor = "current_thread")]
async fn exported_snapshot_and_concurrent_wal_have_no_gap_and_restart_uses_the_same_slot(
) -> Result<()> {
    let database = Database::new().await?;
    database
        .client()
        .batch_execute("INSERT INTO person VALUES (1, 'initial'), (2, 'deleted');")
        .await?;
    let path = tempfile::tempdir()?;
    let slot;
    {
        let progress = progress();
        let mut config = database.config.clone();
        config.io_timeout_ms = NonZeroU64::new(2000).expect("timeout");
        let (mut source, bootstrap) = coordinated(config, progress.clone())?;
        let prepared = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let mut query = query(path.path(), progress.clone())
            .await?
            .with_bootstrap(Arc::new(PausedSnapshot {
                inner: bootstrap,
                prepared: prepared.clone(),
                release: release.clone(),
            }));
        let writes = async {
            prepared.notified().await;
            tokio::time::sleep(Duration::from_millis(2200)).await;
            database
                .client()
                .batch_execute(
                    "BEGIN; UPDATE person SET name = 'middle' WHERE id = 1;
                 UPDATE person SET name = 'final' WHERE id = 1;
                 DELETE FROM person WHERE id = 2;
                 INSERT INTO person VALUES (3, 'during snapshot');
                 COMMIT;",
                )
                .await?;
            release.notify_one();
            Result::<()>::Ok(())
        };
        let (started, written, source_started) =
            tokio::time::timeout(Duration::from_secs(15), async {
                tokio::join!(query.start(), writes, source.start())
            })
            .await?;
        started?;
        written?;
        source_started?;
        assert_eq!(
            rows(&query),
            vec![
                serde_json::json!({"id":1, "name":"initial"}),
                serde_json::json!({"id":2, "name":"deleted"}),
            ]
        );
        slot = managed_slot(&database).await?;
        let group = next(&mut source).await?;
        apply(&mut query, group).await?;
        assert_eq!(
            rows(&query),
            vec![
                serde_json::json!({"id":1, "name":"final"}),
                serde_json::json!({"id":3, "name":"during snapshot"}),
            ]
        );
        source.stop().await?;
        query.stop().await?;
    }
    database
        .client()
        .batch_execute("INSERT INTO person VALUES (4, 'after stop');")
        .await?;
    {
        let progress = progress();
        let (mut source, bootstrap) = coordinated(database.config.clone(), progress.clone())?;
        let mut query = query(path.path(), progress)
            .await?
            .with_bootstrap(bootstrap);
        query.start().await?;
        assert_eq!(managed_slot(&database).await?, slot);
        assert_eq!(rows(&query).len(), 2);
        source.start().await?;
        let group = next(&mut source).await?;
        apply(&mut query, group).await?;
        assert_eq!(rows(&query).len(), 3);
        source.stop().await?;
        query.stop().await?;
    }
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
async fn interrupted_snapshot_requires_reset_and_replaces_only_its_owned_unfinished_slot(
) -> Result<()> {
    let database = Database::new().await?;
    database
        .client()
        .batch_execute("INSERT INTO person VALUES (1, 'before');")
        .await?;
    let path = tempfile::tempdir()?;
    let abandoned;
    {
        let progress = progress();
        let (_source, bootstrap) = coordinated(database.config.clone(), progress.clone())?;
        let prepared = Arc::new(Notify::new());
        let mut query = query(path.path(), progress.clone())
            .await?
            .with_bootstrap(Arc::new(PausedSnapshot {
                inner: bootstrap,
                prepared: prepared.clone(),
                release: Arc::new(Notify::new()),
            }));
        tokio::time::timeout(Duration::from_secs(15), async {
            tokio::select! {
                result = query.start() => anyhow::bail!("snapshot unexpectedly ended: {result:?}"),
                _ = prepared.notified() => Ok(()),
            }
        })
        .await??;
        assert!(!progress.snapshot().ready);
        assert!(progress.snapshot().checkpoints.is_empty());
        abandoned = managed_slot(&database).await?;
        query.stop().await?;
    }
    database
        .client()
        .batch_execute("DELETE FROM person; INSERT INTO person VALUES (2, 'after');")
        .await?;
    {
        let progress = progress();
        let (_source, bootstrap) = coordinated(database.config.clone(), progress.clone())?;
        let mut query = query(path.path(), progress)
            .await?
            .with_bootstrap(bootstrap);
        let error = query.start().await.expect_err("strict incomplete snapshot");
        assert!(matches!(
            error.downcast_ref::<QueryRecoveryError>(),
            Some(QueryRecoveryError::IncompleteBootstrap)
        ));
        query.stop().await?;
        assert_eq!(managed_slot(&database).await?, abandoned);
    }
    {
        let progress = progress();
        let (mut source, bootstrap) = coordinated(database.config.clone(), progress.clone())?;
        let mut query = query_with_recovery(path.path(), progress, QueryRecoveryPolicy::AutoReset)
            .await?
            .with_bootstrap(bootstrap);
        query.start().await?;
        assert_ne!(managed_slot(&database).await?, abandoned);
        assert_eq!(
            rows(&query),
            vec![serde_json::json!({"id":2, "name":"after"})]
        );
        let external: bool = database.client().query_one(
            "SELECT EXISTS (SELECT 1 FROM pg_replication_slots WHERE slot_name = 'native_slot')", &[],
        ).await?.get(0);
        assert!(external, "externally managed slot must remain untouched");
        source.start().await?;
        database
            .client()
            .batch_execute("UPDATE person SET name = 'live' WHERE id = 2;")
            .await?;
        let group = next(&mut source).await?;
        apply(&mut query, group).await?;
        assert_eq!(
            rows(&query),
            vec![serde_json::json!({"id":2, "name":"live"})]
        );
        source.stop().await?;
        query.stop().await?;
    }
    database.shutdown().await
}

struct CrashSnapshot(Arc<PostgresSnapshot>);

#[tokio::test(flavor = "current_thread")]
async fn completed_snapshot_refuses_missing_history_or_changed_publication_without_reinitializing(
) -> Result<()> {
    for damage in ["missing-slot", "publication-filter", "column-type"] {
        let database = Database::new().await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES (1, 'original');")
            .await?;
        let path = tempfile::tempdir()?;
        {
            let progress = progress();
            let (_source, bootstrap) = coordinated(database.config.clone(), progress.clone())?;
            let mut query = query(path.path(), progress)
                .await?
                .with_bootstrap(bootstrap);
            query.start().await?;
            query.stop().await?;
        }
        let slot = managed_slot(&database).await?;
        match damage {
            "missing-slot" => {
                database
                    .client()
                    .query_one("SELECT pg_drop_replication_slot($1)", &[&slot])
                    .await?;
            }
            "publication-filter" => {
                database
                    .client()
                    .batch_execute(
                        "ALTER PUBLICATION native_publication SET TABLE person WHERE (id > 10);",
                    )
                    .await?
            }
            _ => {
                database
                    .client()
                    .batch_execute("ALTER TABLE person ALTER COLUMN name TYPE varchar(100);")
                    .await?
            }
        };
        let progress = progress();
        let (mut source, bootstrap) = coordinated(database.config.clone(), progress.clone())?;
        let mut query = query_with_recovery(path.path(), progress, QueryRecoveryPolicy::AutoReset)
            .await?
            .with_bootstrap(bootstrap);
        query.start().await?;
        let error = source.start().await.expect_err("changed recovery boundary");
        if damage == "missing-slot" {
            assert!(error.to_string().contains("existing slot"), "{error:#}");
        } else {
            assert!(error.to_string().contains("binding"), "{error:#}");
        }
        let remaining: i64 = database
            .client()
            .query_one(
                "SELECT count(*) FROM pg_replication_slots WHERE slot_name LIKE 'native_slot\\_%'",
                &[],
            )
            .await?
            .get(0);
        assert_eq!(remaining, i64::from(damage != "missing-slot"));
        assert_eq!(
            rows(&query),
            vec![serde_json::json!({"id":1, "name":"original"})]
        );
        source.stop().await?;
        query.stop().await?;
        database.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_snapshot_row_never_completes_and_restarts_after_limit_increase() -> Result<()> {
    let database = Database::new().await?;
    database
        .client()
        .execute(
            "INSERT INTO person VALUES (1, $1)",
            &[&"large".repeat(4000)],
        )
        .await?;
    let path = tempfile::tempdir()?;
    let abandoned;
    {
        let progress = progress();
        let mut config = database.config.clone();
        config.max_protocol_bytes = size(4096);
        let (_source, bootstrap) = coordinated(config, progress.clone())?;
        let mut query = query(path.path(), progress.clone())
            .await?
            .with_bootstrap(bootstrap);
        let error = query.start().await.expect_err("oversized snapshot row");
        assert!(error.to_string().contains("byte limit"), "{error:#}");
        assert!(!progress.snapshot().ready && progress.snapshot().checkpoints.is_empty());
        abandoned = managed_slot(&database).await?;
        query.stop().await?;
    }
    let progress = progress();
    let (mut source, bootstrap) = coordinated(database.config.clone(), progress.clone())?;
    let mut query = query_with_recovery(path.path(), progress, QueryRecoveryPolicy::AutoReset)
        .await?
        .with_bootstrap(bootstrap);
    query.start().await?;
    assert_ne!(managed_slot(&database).await?, abandoned);
    assert_eq!(
        rows(&query),
        vec![serde_json::json!({"id":1, "name":"large".repeat(4000)})]
    );
    source.start().await?;
    source.stop().await?;
    query.stop().await?;
    database.shutdown().await
}

#[async_trait]
impl ComputationBootstrapProvider for CrashSnapshot {
    async fn prepare_with_state(&self, state: &dyn BootstrapState) -> Result<BootstrapPreparation> {
        self.0.prepare_with_state(state).await
    }
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        anyhow::bail!("requires coordinated state")
    }
    async fn snapshot_with_state(
        &self,
        state: &dyn BootstrapState,
    ) -> Result<ComputationBootstrapSnapshot> {
        let snapshot = self.0.snapshot_with_state(state).await?;
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(snapshot.changes.enumerate().map(|(index, change)| {
                if index == 1 {
                    // One row has already committed to the hidden initial state.
                    std::process::exit(83);
                }
                change
            })),
            watermarks: snapshot.watermarks,
        })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn postgres_snapshot_process_exit_helper() -> Result<()> {
    let Ok(point) = std::env::var("DRASI_PG_SNAPSHOT_CRASH") else {
        return Ok(());
    };
    anyhow::ensure!(
        point == "during" || point == "completed",
        "unknown snapshot crash boundary"
    );
    let config = serde_json::from_str(&std::env::var("DRASI_PG_TRANSACTION_CONFIG")?)?;
    let path = std::env::var("DRASI_PG_TRANSACTION_PATH")?;
    let progress = progress();
    let (_source, bootstrap) = coordinated(config, progress.clone())?;
    let bootstrap: Arc<dyn ComputationBootstrapProvider> = if point == "during" {
        Arc::new(CrashSnapshot(bootstrap))
    } else {
        bootstrap
    };
    let mut query = query(Path::new(&path), progress)
        .await?
        .with_bootstrap(bootstrap);
    query.start().await?;
    anyhow::ensure!(
        point == "completed",
        "partial snapshot did not reach its crash point"
    );
    std::process::exit(83)
}

#[tokio::test(flavor = "current_thread")]
async fn process_exit_during_or_after_snapshot_keeps_ownership_and_the_correct_recovery_boundary(
) -> Result<()> {
    for point in ["during", "completed"] {
        let database = Database::new().await?;
        database
            .client()
            .batch_execute("INSERT INTO person VALUES (1, 'old'), (2, 'deleted');")
            .await?;
        let path = tempfile::tempdir()?;
        let output = tokio::time::timeout(
            Duration::from_secs(20),
            tokio::process::Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "snapshot::postgres_snapshot_process_exit_helper",
                    "--nocapture",
                ])
                .env("DRASI_PG_SNAPSHOT_CRASH", point)
                .env(
                    "DRASI_PG_TRANSACTION_CONFIG",
                    serde_json::to_string(&database.config)?,
                )
                .env("DRASI_PG_TRANSACTION_PATH", path.path())
                .kill_on_drop(true)
                .output(),
        )
        .await??;
        anyhow::ensure!(
            output.status.code() == Some(83),
            "snapshot crash helper failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let before = managed_slot(&database).await?;
        database
            .client()
            .batch_execute(
                "BEGIN; UPDATE person SET name = 'new' WHERE id = 1;
             DELETE FROM person WHERE id = 2; INSERT INTO person VALUES (3, 'new'); COMMIT;",
            )
            .await?;
        let progress = progress();
        let (mut source, bootstrap) = coordinated(database.config.clone(), progress.clone())?;
        let recovery = if point == "during" {
            QueryRecoveryPolicy::AutoReset
        } else {
            QueryRecoveryPolicy::Strict
        };
        let mut query = query_with_recovery(path.path(), progress, recovery)
            .await?
            .with_bootstrap(bootstrap);
        query.start().await?;
        let after = managed_slot(&database).await?;
        if point == "during" {
            assert_ne!(before, after);
        } else {
            assert_eq!(before, after);
        }
        source.start().await?;
        if point == "completed" {
            let group = next(&mut source).await?;
            apply(&mut query, group).await?;
        }
        assert_eq!(
            rows(&query),
            vec![
                serde_json::json!({"id":1, "name":"new"}),
                serde_json::json!({"id":3, "name":"new"}),
            ]
        );
        source.stop().await?;
        query.stop().await?;
        database.shutdown().await?;
    }
    Ok(())
}
