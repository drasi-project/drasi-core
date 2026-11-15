// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Production-readiness behaviour against a real server: slot ownership and
//! erasing disposal, publication and schema fences, reconnection, pause and
//! source-schema discovery.

use super::*;
use crate::lifecycle_ctx;
use drasi_computation_source_postgres::SlotOwnership;
use drasi_postgres_common::replication::PostgresCdcError;
use serde_json::json;

fn managed(config: &PostgresTransactionConfig) -> PostgresTransactionConfig {
    let mut config = config.clone();
    config.slot_ownership = SlotOwnership::Managed;
    config.slot_name = None;
    config.start_lsn = None;
    config
}

async fn slots(database: &Database, pattern: &str) -> Result<Vec<String>> {
    Ok(database
        .client()
        .query(
            "SELECT slot_name FROM pg_replication_slots WHERE slot_name LIKE $1 ORDER BY slot_name",
            &[&pattern],
        )
        .await?
        .iter()
        .map(|row| row.get(0))
        .collect())
}

fn cdc_error(error: &anyhow::Error) -> Option<&PostgresCdcError> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<PostgresCdcError>())
}

async fn walsender_pid(database: &Database, slot: &str) -> Result<Option<i32>> {
    Ok(database
        .client()
        .query_one(
            "SELECT active_pid FROM pg_replication_slots WHERE slot_name = $1",
            &[&slot],
        )
        .await?
        .get(0))
}

/// Streams one committed row through `source` into `query`.
async fn deliver(
    database: &Database,
    source: &mut PostgresTransactionSource,
    query: &mut ContinuousQueryTransformer,
    progress: &QuerySourceProgress,
    sql: &str,
) -> Result<()> {
    database.client().batch_execute(sql).await?;
    let group = next(source).await?;
    apply(query, group).await?;
    anyhow::ensure!(committed(progress) > 0, "no committed cursor");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_erase_drops_only_managed_slots_idempotently_and_refuses_active_ones() -> Result<()>
{
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(path.path(), progress.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let config = managed(&database.config);
    let mut source = source(config.clone(), progress.clone())?;
    source.start(&lifecycle_ctx::start()).await?;
    let owned = slots(&database, "drasi\\_%").await?;
    assert_eq!(
        owned.len(),
        1,
        "the source created exactly its managed slot"
    );
    deliver(
        &database,
        &mut source,
        &mut query,
        &progress,
        "INSERT INTO person VALUES (1, 'managed')",
    )
    .await?;
    assert_eq!(rows(&query), vec![json!({"id": 1, "name": "managed"})]);

    // Another object for the same source finds the slot active and refuses.
    let mut other = super::source(config.clone(), progress.clone())?;
    let error = other
        .dispose(&lifecycle_ctx::erase())
        .await
        .expect_err("an active owned slot is never dropped");
    assert!(
        matches!(cdc_error(&error), Some(PostgresCdcError::SlotActive(_))),
        "{error:#}"
    );
    source.stop(&lifecycle_ctx::stop()).await?;
    super::tls::retired(&database).await?;

    // Restart resumes the same managed slot and committed cursor.
    source.start(&lifecycle_ctx::start()).await?;
    deliver(
        &database,
        &mut source,
        &mut query,
        &progress,
        "INSERT INTO person VALUES (2, 'resumed')",
    )
    .await?;
    assert_eq!(slots(&database, "drasi\\_%").await?, owned);
    source.stop(&lifecycle_ctx::stop()).await?;
    super::tls::retired(&database).await?;

    source.dispose(&lifecycle_ctx::erase()).await?;
    assert!(slots(&database, "drasi\\_%").await?.is_empty());
    assert_eq!(
        slots(&database, "native_slot").await?,
        vec!["native_slot".to_owned()],
        "an operator-owned slot is untouched"
    );
    other.dispose(&lifecycle_ctx::erase()).await?;

    // External ownership never drops its slot.
    let mut external = super::source(database.config.clone(), progress.clone())?;
    external.dispose(&lifecycle_ctx::erase()).await?;
    assert_eq!(slots(&database, "native_slot").await?.len(), 1);
    query.stop(&lifecycle_ctx::stop()).await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_missing_external_slot_and_truncate_publication_fail_before_streaming(
) -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(path.path(), progress.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let mut missing = database.config.clone().with_external_slot("absent_slot");
    missing.start_lsn = None;
    let error = super::source(missing, progress.clone())?
        .start(&lifecycle_ctx::start())
        .await
        .expect_err("an external slot is never created");
    assert!(
        matches!(
            cdc_error(&error),
            Some(PostgresCdcError::PositionUnavailable(_))
        ),
        "{error:#}"
    );
    assert!(slots(&database, "absent_slot").await?.is_empty());

    database
        .client()
        .batch_execute(
            "ALTER PUBLICATION native_publication SET (publish = 'insert, update, delete, truncate')",
        )
        .await?;
    let mut source = super::source(database.config.clone(), progress.clone())?;
    let error = source
        .start(&lifecycle_ctx::start())
        .await
        .expect_err("a TRUNCATE-publishing publication is refused");
    assert!(
        matches!(cdc_error(&error), Some(PostgresCdcError::Unsupported(detail)) if detail.contains("TRUNCATE")),
        "{error:#}"
    );
    source.stop(&lifecycle_ctx::stop()).await?;
    assert!(progress.snapshot().checkpoints.is_empty());
    query.stop(&lifecycle_ctx::stop()).await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_non_key_columns_can_change_but_replica_identity_changes_fence_the_cursor(
) -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(path.path(), progress.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let mut source = source(database.config.clone(), progress.clone())?;
    source.start(&lifecycle_ctx::start()).await?;
    deliver(
        &database,
        &mut source,
        &mut query,
        &progress,
        "INSERT INTO person VALUES (1, 'before')",
    )
    .await?;
    source.stop(&lifecycle_ctx::stop()).await?;
    super::tls::retired(&database).await?;

    database
        .client()
        .batch_execute("ALTER TABLE person ADD COLUMN nickname TEXT")
        .await?;
    source.start(&lifecycle_ctx::start()).await?;
    deliver(
        &database,
        &mut source,
        &mut query,
        &progress,
        "INSERT INTO person VALUES (2, 'after', 'two')",
    )
    .await?;
    assert_eq!(
        rows(&query),
        vec![
            json!({"id": 1, "name": "before"}),
            json!({"id": 2, "name": "after"})
        ]
    );
    source.stop(&lifecycle_ctx::stop()).await?;
    super::tls::retired(&database).await?;

    database
        .client()
        .batch_execute("ALTER TABLE person REPLICA IDENTITY FULL")
        .await?;
    let error = source
        .start(&lifecycle_ctx::start())
        .await
        .expect_err("a replica identity change invalidates the committed cursor");
    assert!(
        matches!(cdc_error(&error), Some(PostgresCdcError::SchemaChanged(_))),
        "{error:#}"
    );
    source.stop(&lifecycle_ctx::stop()).await?;
    query.stop(&lifecycle_ctx::stop()).await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_table_joining_an_all_tables_publication_is_a_typed_schema_change() -> Result<()> {
    let database = Database::new().await?;
    database
        .client()
        .batch_execute(
            "CREATE PUBLICATION every_table FOR ALL TABLES WITH (publish = 'insert, update, delete')",
        )
        .await?;
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(path.path(), progress.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let mut config = database.config.clone();
    config.publication = "every_table".into();
    config.tables = Vec::new();
    let mut source = source(config, progress.clone())?;
    source.start(&lifecycle_ctx::start()).await?;
    database
        .client()
        .batch_execute(
            "CREATE TABLE joined (id INTEGER PRIMARY KEY); INSERT INTO joined VALUES (1)",
        )
        .await?;
    let error = tokio::time::timeout(Duration::from_secs(8), source.next())
        .await?
        .expect_err("an unselected new table stops the source");
    assert!(
        matches!(cdc_error(&error), Some(PostgresCdcError::SchemaChanged(detail)) if detail.contains("joined")),
        "{error:#}"
    );
    source.stop(&lifecycle_ctx::stop()).await?;
    query.stop(&lifecycle_ctx::stop()).await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_transaction_mode_reconnects_after_the_walsender_is_terminated() -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(path.path(), progress.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let mut source = source(database.config.clone(), progress.clone())?;
    source.start(&lifecycle_ctx::start()).await?;
    deliver(
        &database,
        &mut source,
        &mut query,
        &progress,
        "INSERT INTO person VALUES (1, 'before')",
    )
    .await?;
    let before = walsender_pid(&database, "native_slot")
        .await?
        .context("active walsender")?;
    database
        .client()
        .execute("SELECT pg_terminate_backend($1)", &[&before])
        .await?;
    database
        .client()
        .batch_execute("INSERT INTO person VALUES (2, 'after')")
        .await?;
    let group = next(&mut source).await?;
    apply(&mut query, group).await?;
    assert_eq!(
        rows(&query),
        vec![
            json!({"id": 1, "name": "before"}),
            json!({"id": 2, "name": "after"})
        ]
    );
    let after = walsender_pid(&database, "native_slot").await?;
    assert!(after.is_some_and(|pid| pid != before), "a new walsender");
    source.stop(&lifecycle_ctx::stop()).await?;
    query.stop(&lifecycle_ctx::stop()).await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_pause_holds_output_and_outlasts_wal_sender_timeout() -> Result<()> {
    let database = Database::new().await?;
    let path = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(path.path(), progress.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let mut source = source(database.config.clone(), progress.clone())?;
    source.start(&lifecycle_ctx::start()).await?;
    deliver(
        &database,
        &mut source,
        &mut query,
        &progress,
        "INSERT INTO person VALUES (1, 'before')",
    )
    .await?;
    let pid = walsender_pid(&database, "native_slot").await?;
    source.pause(&lifecycle_ctx::pause()).await?;
    database
        .client()
        .batch_execute("INSERT INTO person VALUES (2, 'during')")
        .await?;
    // The fixture's wal_sender_timeout is 1.5 s; feedback keeps the session.
    assert!(
        tokio::time::timeout(Duration::from_secs(4), source.next())
            .await
            .is_err(),
        "a paused source emits nothing"
    );
    assert_eq!(
        walsender_pid(&database, "native_slot").await?,
        pid,
        "the replication session survived the pause"
    );
    source.resume(&lifecycle_ctx::resume()).await?;
    let group = next(&mut source).await?;
    apply(&mut query, group).await?;
    assert_eq!(
        rows(&query),
        vec![
            json!({"id": 1, "name": "before"}),
            json!({"id": 2, "name": "during"})
        ]
    );
    source.stop(&lifecycle_ctx::stop()).await?;
    query.stop(&lifecycle_ctx::stop()).await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn postgres_schema_reader_describes_emitted_labels_and_types() -> Result<()> {
    use drasi_computation_plugin_sdk::NativeSource;
    let database = Database::new().await?;
    let progress = progress();
    let source = drasi_computation_source_postgres::PostgresTransactionSource::new(
        component_id(77),
        StreamId::try_new("changes")?,
        database.config.clone(),
        SourceProgressReader::from(progress).read_only(),
        false,
    )?;
    let schema = source
        .source_schema_reader()
        .context("schema reader")?
        .describe_schema()
        .await?
        .context("schema")?;
    assert_eq!(schema.nodes.len(), 1);
    assert_eq!(schema.nodes[0].label, "person");
    let types: Vec<_> = schema.nodes[0]
        .properties
        .iter()
        .map(|property| format!("{}:{:?}", property.name, property.data_type))
        .collect();
    assert_eq!(types, ["id:Some(Integer)", "name:Some(String)"]);
    database.shutdown().await
}
