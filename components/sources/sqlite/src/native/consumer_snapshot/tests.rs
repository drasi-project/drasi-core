// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use std::num::{NonZeroU64, NonZeroUsize};

async fn cancelled_worker_retains_one_client() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("source.db");
    let lock = rusqlite::Connection::open(&path)?;
    lock.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'initial'); BEGIN EXCLUSIVE;"
    )?;
    let size = |value| NonZeroUsize::new(value).expect("test bound");
    let config = SqliteConfig {
        path: Some(path),
        tables: vec![],
        table_keys: vec![],
        output: SqliteOutput::Transactions,
        replay: Some(SqliteReplayConfig {
            max_transactions: size(4),
            max_bytes: size(1 << 20),
        }),
        transactions: SourceTransactionLimits {
            max_changes: size(8),
            max_bytes: size(65536),
            max_duration_ms: NonZeroU64::new(1000).expect("duration"),
        },
        max_sql_bytes: size(65536),
        command_capacity: size(4),
        output_capacity: size(1),
        shutdown_timeout_ms: NonZeroU64::new(10).expect("cleanup bound"),
    };
    let first = Arc::new(QuerySourceProgress::new(
        "sqlite-shared-cleanup",
        ComponentId::try_new("first")?,
    )?);
    let second = Arc::new(QuerySourceProgress::new(
        "sqlite-shared-cleanup",
        ComponentId::try_new("second")?,
    )?);
    let owners = Arc::new(ConsumerProgressSet::new(
        "sqlite-shared-cleanup",
        ComponentId::try_new("sqlite")?,
        [first.clone(), second],
    )?);
    let client = Arc::new(SqliteClientResource::new(
        ComponentId::try_new("sqlite")?,
        config.clone(),
    )?);
    let (source, snapshots) = SqliteSource::coordinated_with_consumers(
        ComponentId::try_new("sqlite")?,
        StreamId::try_new("changes")?,
        config.clone(),
        owners.clone(),
    )?;
    let source = source.with_client_resource(client.clone())?;
    let snapshot = snapshots.for_consumer(first.component_id())?;
    let mut query = ContinuousQueryTransformer::new_configured(
        ContinuousQueryDefinition {
            graph_id: "sqlite-shared-cleanup".into(),
            id: first.component_id().clone(),
            query: "MATCH (n:items) RETURN n.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: StreamId::try_new("first/out")?,
            outbox_capacity: size(4),
        },
        Arc::new(RocksDbComputationProvider::new(
            directory.path().join("query"),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        )),
        QueryOptions::default(),
        QueryExecutionSettings {
            source_transactions: Some(config.transactions),
            ..Default::default()
        },
        None,
    )
    .await?
    .with_source_progress(first)?
    .with_bootstrap(snapshot.clone());
    {
        let start = query.start();
        tokio::pin!(start);
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::select! {
                result = &mut start => anyhow::bail!("locked snapshot completed unexpectedly: {result:?}"),
                _ = async {
                    while !snapshots.inner.owns_worker().await { tokio::task::yield_now().await; }
                } => Ok::<_, anyhow::Error>(()),
            }
        }).await??;
    }
    assert_eq!(
        Arc::strong_count(source.client_owner.as_ref().expect("client owner")),
        2
    );
    drop(source);
    let replacement = || {
        SqliteSource::new_replayable_with_consumers(
            ComponentId::try_new("sqlite").expect("source"),
            StreamId::try_new("changes").expect("stream"),
            config.clone(),
            owners.clone(),
        )?
        .with_client_resource(client.clone())
    };
    assert!(replacement().is_err());
    assert!(
        tokio::time::timeout(Duration::from_millis(1), snapshot.stop())
            .await
            .is_err()
    );
    assert!(
        replacement().is_err(),
        "cancelled cleanup must retain the worker lease"
    );
    let before = tokio::time::Instant::now();
    let error = snapshot.stop().await.expect_err("blocked SQLite worker");
    assert!(matches!(
        error.downcast_ref::<drasi_lib::context::workers::WorkerCleanupError>(),
        Some(drasi_lib::context::workers::WorkerCleanupError::TimedOut { .. })
    ));
    assert!(before.elapsed() >= config.timeout());
    assert!(snapshots.inner.owns_worker().await);
    assert!(
        replacement().is_err(),
        "timed-out cleanup must retain the actual worker lease"
    );
    lock.execute_batch("ROLLBACK")?;
    drop(lock);
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match snapshot.stop().await {
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
    query.stop().await?;
    assert!(!snapshots.inner.owns_worker().await);
    assert!(!snapshots.inner.has_active_run()?);
    let mut replacement = replacement()?;
    replacement.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn group_snapshot_cancelled_cleanup_retains_client_current_thread() -> Result<()> {
    cancelled_worker_retains_one_client().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_snapshot_cancelled_cleanup_retains_client_multi_thread() -> Result<()> {
    cancelled_worker_retains_one_client().await
}
