// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use crate::bootstrap::provider as sqlite;
use drasi_host_sdk::computation::NativeBootstrapResource;

pub(super) const TEXT: &str = "MATCH (n:items) RETURN sum(n.amount) AS total";

#[path = "durable_composite.rs"]
mod composite;
#[path = "durable_sqlite.rs"]
mod coordinated;
#[path = "evaluator.rs"]
mod evaluator;
#[path = "durable_bootstrap_lifecycle.rs"]
mod lifecycle;

fn reader(query: &NativeComponentProxy) -> Result<Arc<dyn SnapshotProvider>> {
    query
        .output_readers()
        .ports()
        .get(&PortId::try_new("out")?)
        .and_then(|readers| readers.snapshot())
        .cloned()
        .context("missing query snapshot")
}

pub(super) fn total(snapshot: &OutputSnapshot) -> Result<f64> {
    let mut rows = snapshot.records().values();
    let record = rows.next().context("missing aggregate row")?;
    assert!(rows.next().is_none());
    let decoded = QueryChangeCodec::decode_row(record)?;
    QueryChangeCodec::row_values_to_json(&decoded.values)["total"]
        .as_f64()
        .context("missing total")
}

pub(super) fn file_provider(
    database: &Path,
    generation: u64,
) -> Result<Arc<drasi_host_sdk::computation::NativeBootstrapProxy>> {
    let bootstrap = computation::load(sqlite::bootstrap_library())?;
    let bootstrap = bootstrap
        .bootstrap_factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.sqlite/file-bootstrap"
        })
        .context("missing SQLite file bootstrap")?;
    bootstrap.create_query_provider(
        component_id()?,
        sqlite::provider_configuration(database),
        scope(generation),
        None,
    )
}

fn file_query(database: &Path, indexes: &Path, generation: u64) -> Result<NativeComponentProxy> {
    let provider = file_provider(database, generation)?;
    let mut query = factory()?.create_component_with_processing(
        component_id()?,
        json!({"query": TEXT, "bootstrap": true}),
        scope(generation),
        super::provider(indexes)?,
    )?;
    query.bind_bootstrap(Arc::new(NativeBootstrapResource(provider)))?;
    Ok(query)
}

async fn file_bootstrap() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    let indexes = directory.path().join("indexes");
    sqlite::create_database(&database)?;
    let mut query = file_query(&database, &indexes, 1)?;
    query.initialize().await?;
    let progress = query
        .processing_progress_owner()
        .context("missing actual progress owner")?;
    assert!(!progress.snapshot().recovered);
    query.start().await?;
    assert!(query.continue_transform().await?.is_empty());
    let snapshot = reader(&query)?.snapshot().await?;
    assert_eq!(total(&snapshot)?, 30.0);
    assert_eq!(snapshot.cursor().sequence(), 0);
    assert!(snapshot.cursor().producer().persistent());
    let observed = super::shared_output::observation(&query).await?;
    assert_eq!(observed.cursor, *snapshot.cursor());
    assert_eq!(observed.live_results, 1);
    assert_eq!(observed.outbox_size, 0);
    assert_eq!(observed.oldest_sequence, None);
    assert!(progress.snapshot().ready && progress.snapshot().bootstrap_complete);
    assert!(
        progress.snapshot().checkpoints.is_empty(),
        "row ordinals are not source checkpoints"
    );
    let producer = snapshot.cursor().producer().clone();
    query.stop().await?;
    drop(query);

    rusqlite::Connection::open(&database)?.execute("INSERT INTO items VALUES (3, 30)", [])?;
    let mut query = file_query(&database, &indexes, 2)?;
    query.initialize().await?;
    query.start().await?;
    let reopened = reader(&query)?.snapshot().await?;
    assert_eq!(
        total(&reopened)?,
        30.0,
        "completed initialization must not read the file again"
    );
    assert_eq!(reopened.cursor().producer(), &producer);
    assert_eq!(reopened.cursor().sequence(), 0);
    let observed = super::shared_output::observation(&query).await?;
    assert_eq!(observed.live_results, 1);
    assert_eq!(observed.outbox_size, 0);
    assert!(query.continue_transform().await?.is_empty());
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_file_bootstrap_commits_and_reopens_current_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), file_bootstrap()).await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_file_bootstrap_commits_and_reopens_multi_thread() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(30), file_bootstrap()).await?
}

async fn volatile_file_bootstrap() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("source.sqlite");
    sqlite::create_database(&database)?;
    let mut query = factory()?.create_component_with_processing(
        component_id()?,
        json!({"query": TEXT, "bootstrap": true, "volatile": true}),
        scope(1),
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )?;
    query.initialize().await?;
    query.bind_bootstrap(Arc::new(NativeBootstrapResource(file_provider(
        &database, 1,
    )?)))?;
    query.start().await?;
    let original = reader(&query)?.snapshot().await?;
    assert_eq!(total(&original)?, 30.0);
    assert!(!original.cursor().producer().persistent());
    assert!(
        !query
            .processing_progress_owner()
            .context("actual volatile owner")?
            .snapshot()
            .persistent
    );
    query.stop().await?;
    rusqlite::Connection::open(&database)?.execute("INSERT INTO items VALUES (3, 30)", [])?;
    query.start().await?;
    let retained = reader(&query)?.snapshot().await?;
    assert_eq!(total(&retained)?, 30.0);
    assert_eq!(retained.cursor().producer(), original.cursor().producer());
    query.stop().await?;
    query.delete_owned_resources().await?;
    assert_eq!(
        rusqlite::Connection::open(&database)?.query_row(
            "SELECT count(*) FROM items",
            [],
            |row| row.get::<_, i64>(0)
        )?,
        3
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_volatile_file_bootstrap_retains_state_current_thread() -> Result<()> {
    volatile_file_bootstrap().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_volatile_file_bootstrap_retains_state_multi_thread() -> Result<()> {
    volatile_file_bootstrap().await
}

struct InterruptedMemoryBootstrap(std::sync::atomic::AtomicBool);

#[async_trait]
impl ComputationBootstrapProvider for InterruptedMemoryBootstrap {
    async fn snapshot(&self) -> Result<ComputationBootstrapSnapshot> {
        let failing = self.0.load(std::sync::atomic::Ordering::Acquire);
        let row = if failing {
            item(10, 1)
        } else {
            item_with_id("replacement", 7, 2)
        };
        let mut changes = vec![Ok(input(SourceChange::Insert { element: row }, 1)?.envelope)];
        if failing {
            changes.push(Err(anyhow::anyhow!("interrupted memory bootstrap")));
        }
        Ok(ComputationBootstrapSnapshot {
            changes: Box::pin(futures_util::stream::iter(changes)),
            watermarks: Vec::new(),
        })
    }
}

async fn volatile_bootstrap_recovery() -> Result<()> {
    let provider = Arc::new(InterruptedMemoryBootstrap(
        std::sync::atomic::AtomicBool::new(true),
    ));
    let mut query = factory()?.create_component_with_processing(
        component_id()?,
        json!({"query": QUERY, "bootstrap": true, "volatile": true, "recovery": "auto_reset"}),
        scope(1),
        Arc::new(drasi_core::computation::InMemoryComputationProvider),
    )?;
    query.initialize().await?;
    query.bind_bootstrap(Arc::new(NativeBootstrapResource(provider.clone())))?;
    assert!(query.start().await.is_err());
    assert!(reader(&query)?.snapshot().await.is_err());
    provider
        .0
        .store(false, std::sync::atomic::Ordering::Release);
    query.start().await?;
    assert_eq!(total(&reader(&query)?.snapshot().await?)?, 7.0);
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn indexed_volatile_bootstrap_resets_partial_state_current_thread() -> Result<()> {
    volatile_bootstrap_recovery().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn indexed_volatile_bootstrap_resets_partial_state_multi_thread() -> Result<()> {
    volatile_bootstrap_recovery().await
}
