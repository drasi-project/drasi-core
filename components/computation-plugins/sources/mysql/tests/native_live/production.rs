// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Production behaviour of the native MySQL source against a real server:
//! erase, pause across server timeouts, reconnection after the server kills
//! the replication session, the DDL policy, schema discovery, large
//! transactions, database-wide selection and extended types.

use super::*;
use drasi_mysql_common::native::errors::SourceFailure;

fn source_failure(error: &anyhow::Error) -> Option<&SourceFailure> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<SourceFailure>())
}
use drasi_lib::computation::v1::{OutputEnvelope, SourceTransactionCodec};

fn untracked(port: u16, output: MySqlOutput) -> MySqlConfig {
    let mut settings = config(port, output);
    settings.reconnect_budget_ms = 30_000;
    settings
}

fn untracked_source(settings: MySqlConfig) -> Result<MySqlSource> {
    MySqlSource::new(
        ComponentId::try_new("2f0c3ad8-7c5f-5a62-9f14-1a6a3f3c2b10")?,
        StreamId::try_new("production")?,
        settings,
        None,
    )
}

fn transaction_ids(envelope: &OutputEnvelope, settings: &MySqlConfig) -> Result<Vec<String>> {
    Ok(
        SourceTransactionCodec::decode(&envelope.envelope, settings.transactions)?
            .into_changes()
            .iter()
            .map(|change| change.get_reference().element_id.to_string())
            .collect(),
    )
}

async fn quiet(source: &mut MySqlSource, millis: u64) -> bool {
    tokio::time::timeout(Duration::from_millis(millis), source.next())
        .await
        .is_err()
}

async fn replication_threads(admin: &mut connection::Connection) -> Result<Vec<String>> {
    let rows = admin
        .query("SELECT ID FROM information_schema.PROCESSLIST WHERE COMMAND LIKE 'Binlog Dump%'")
        .await?;
    rows.values
        .iter()
        .map(|row| Ok(catalog::field(row, 0)?.to_owned()))
        .collect()
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_erase_twice_and_schema_discovery() -> Result<()> {
    let mut database = Database::new(MySqlRetention::Server).await?;
    let mut source = untracked_source(untracked(
        database.settings.connection.port,
        MySqlOutput::Changes,
    ))?;
    let reader = source.source_schema_reader().context("schema reader")?;
    source.start(&lifecycle_ctx::start()).await?;
    let schema = reader.describe_schema().await?.context("captured schema")?;
    assert_eq!(schema.nodes.len(), 1);
    assert_eq!(schema.nodes[0].label, "items");
    let names: Vec<_> = schema.nodes[0]
        .properties
        .iter()
        .map(|property| property.name.as_str())
        .collect();
    assert_eq!(names, ["id", "value"]);
    assert!(
        source.dispose(&lifecycle_ctx::erase()).await.is_err(),
        "erase refuses while running"
    );
    source.stop(&lifecycle_ctx::stop()).await?;
    source.dispose(&lifecycle_ctx::erase()).await?;
    source.dispose(&lifecycle_ctx::erase()).await?;
    assert!(reader.describe_schema().await.is_err(), "retired reader");
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_pause_longer_than_server_timeouts_resumes_without_loss() -> Result<()> {
    let mut database = Database::new(MySqlRetention::Server).await?;
    database
        .admin
        .execute("SET GLOBAL net_write_timeout=2, GLOBAL net_read_timeout=2")
        .await?;
    let settings = untracked(database.settings.connection.port, MySqlOutput::Transactions);
    let mut source = untracked_source(settings.clone())?;
    source.start(&lifecycle_ctx::start()).await?;
    database
        .admin
        .execute("INSERT INTO items VALUES (0, 'before')")
        .await?;
    assert_eq!(
        transaction_ids(&next(&mut source).await?, &settings)?.len(),
        1
    );
    // Small writes fit the pause buffer; large ones overflow it, so the
    // connection is closed deliberately and reopened on resume.
    for (round, value) in [(1, "x".repeat(16)), (2, "y".repeat(70))] {
        source.pause(&lifecycle_ctx::pause()).await?;
        let first = round * 1000;
        for id in first..first + 200 {
            database
                .admin
                .execute(&format!("INSERT INTO items VALUES ({id}, '{value}')"))
                .await?;
        }
        assert!(
            quiet(&mut source, 500).await,
            "nothing is emitted while paused"
        );
        // Longer than net_write_timeout and net_read_timeout.
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(quiet(&mut source, 100).await);
        source.resume(&lifecycle_ctx::resume()).await?;
        let mut seen = Vec::new();
        while seen.len() < 200 {
            seen.extend(transaction_ids(&next(&mut source).await?, &settings)?);
        }
        let expected: Vec<_> = (first..first + 200)
            .map(|id| {
                drasi_mysql_common::keys::transaction_element_id(
                    "test",
                    "items",
                    &[(
                        "id".into(),
                        drasi_core::models::ElementValue::Integer(id as i64),
                    )],
                )
            })
            .collect::<Result<_>>()?;
        assert_eq!(
            seen, expected,
            "round {round}: in order, no loss, no duplicate"
        );
        assert!(quiet(&mut source, 300).await, "round {round}: no replay");
    }
    source.stop(&lifecycle_ctx::stop()).await?;
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_reconnects_after_the_server_kills_replication_without_restart() -> Result<()> {
    let mut database = Database::new(MySqlRetention::Server).await?;
    let path = tempfile::tempdir()?;
    let owner = progress()?;
    let mut query = query(path.path(), owner.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let mut settings = database.settings.clone();
    settings.reconnect_budget_ms = 30_000;
    let mut tracked = source(settings.clone(), owner.clone())?;
    tracked.start(&lifecycle_ctx::start()).await?;
    let mut fast_settings = untracked(settings.connection.port, MySqlOutput::Changes);
    // Two replicas need distinct server IDs (a duplicate is error 1236).
    fast_settings.connection.server_id = std::num::NonZeroU32::new(875).unwrap();
    let mut fast = untracked_source(fast_settings)?;
    fast.start(&lifecycle_ctx::start()).await?;
    database
        .admin
        .execute("INSERT INTO items VALUES (1, 'first')")
        .await?;
    apply(&mut query, next(&mut tracked).await?).await?;
    next(&mut fast).await?;
    // An emitted but uncommitted transaction must not be repeated either.
    database
        .admin
        .execute("INSERT INTO items VALUES (2, 'second')")
        .await?;
    let second = next(&mut tracked).await?;
    next(&mut fast).await?;
    let threads = replication_threads(&mut database.admin).await?;
    assert_eq!(threads.len(), 2, "{threads:?}");
    for thread in threads {
        database
            .admin
            .execute(&format!("KILL CONNECTION {thread}"))
            .await?;
    }
    database
        .admin
        .execute("INSERT INTO items VALUES (3, 'third')")
        .await?;
    apply(&mut query, second).await?;
    let third = tokio::time::timeout(Duration::from_secs(20), tracked.next())
        .await??
        .context("third transaction")?;
    assert_eq!(transaction_ids(&third, &settings)?.len(), 1);
    apply(&mut query, third).await?;
    assert!(quiet(&mut tracked, 300).await, "no replay after reconnect");
    let fast_third = tokio::time::timeout(Duration::from_secs(20), fast.next())
        .await??
        .context("third change")?;
    assert_eq!(fast_third.envelope.changes().operations().len(), 1);
    assert!(quiet(&mut fast, 300).await, "no replay after reconnect");
    assert_eq!(query.results().snapshot()?.rows.len(), 3);
    assert_eq!(replication_threads(&mut database.admin).await?.len(), 2);
    tracked.stop(&lifecycle_ctx::stop()).await?;
    fast.stop(&lifecycle_ctx::stop()).await?;
    query.stop(&lifecycle_ctx::stop()).await?;
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_ddl_on_unselected_tables_is_ignored_and_key_changes_fence() -> Result<()> {
    let mut database = Database::new(MySqlRetention::Server).await?;
    database
        .admin
        .execute("CREATE TABLE audit (id INT PRIMARY KEY)")
        .await?;
    let mut settings = database.settings.clone();
    settings.tables = vec!["items".into()];
    let path = tempfile::tempdir()?;
    let owner = progress()?;
    let mut query = query(path.path(), owner.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let mut source = source(settings.clone(), owner.clone())?;
    source.start(&lifecycle_ctx::start()).await?;
    for statement in ["ANALYZE TABLE items", "OPTIMIZE TABLE items"] {
        database.admin.query(statement).await?;
    }
    for statement in [
        "ALTER TABLE audit ADD COLUMN note TEXT",
        "CREATE INDEX value_index ON items (value)",
        "ALTER TABLE items ADD COLUMN note VARCHAR(20) NULL",
        "TRUNCATE TABLE audit",
    ] {
        database.admin.execute(statement).await?;
    }
    database
        .admin
        .execute("INSERT INTO items VALUES (1, 'after ddl', 'note')")
        .await?;
    // Schema statements advance the cursor with empty transactions; the
    // first transaction with a change carries the new row.
    let mut row = None;
    while row.is_none() {
        let envelope = next(&mut source).await?;
        let changes = SourceTransactionCodec::decode(&envelope.envelope, settings.transactions)?
            .into_changes();
        apply(&mut query, envelope).await?;
        row = changes.into_iter().next();
    }
    let row = row.context("row")?;
    let drasi_core::models::SourceChange::Insert { element } = row else {
        anyhow::bail!("expected an insert, got {row:?}");
    };
    assert_eq!(
        element.get_properties().get("note"),
        Some(&drasi_core::models::ElementValue::String("note".into()))
    );
    let checkpoint = owner.snapshot().checkpoints.clone();
    database
        .admin
        .execute("ALTER TABLE items DROP PRIMARY KEY, ADD PRIMARY KEY (id, value)")
        .await?;
    let error = next(&mut source).await.unwrap_err();
    assert!(
        matches!(
            source_failure(&error),
            Some(SourceFailure::SchemaChanged(_))
        ),
        "{error:#}"
    );
    assert_eq!(owner.snapshot().checkpoints, checkpoint);
    source.stop(&lifecycle_ctx::stop()).await?;
    let error = source.start(&lifecycle_ctx::start()).await.unwrap_err();
    assert!(
        matches!(
            source_failure(&error),
            Some(SourceFailure::SchemaChanged(_))
        ),
        "{error:#}"
    );
    source.stop(&lifecycle_ctx::stop()).await?;
    query.stop(&lifecycle_ctx::stop()).await?;
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_truncate_of_a_selected_table_fences() -> Result<()> {
    let mut database = Database::new(MySqlRetention::Server).await?;
    let mut source = untracked_source(untracked(
        database.settings.connection.port,
        MySqlOutput::Transactions,
    ))?;
    source.start(&lifecycle_ctx::start()).await?;
    database.admin.execute("TRUNCATE TABLE items").await?;
    let error = next(&mut source).await.unwrap_err();
    assert!(
        matches!(
            source_failure(&error),
            Some(SourceFailure::SchemaChanged(_))
        ),
        "{error:#}"
    );
    source.stop(&lifecycle_ctx::stop()).await?;
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_large_transactions_are_published_whole() -> Result<()> {
    let mut database = Database::new(MySqlRetention::Server).await?;
    let mut settings = untracked(database.settings.connection.port, MySqlOutput::Transactions);
    settings.transactions.max_changes = std::num::NonZeroUsize::new(20_000).unwrap();
    settings.transactions.max_bytes = std::num::NonZeroUsize::new(64 << 20).unwrap();
    settings.max_protocol_bytes = std::num::NonZeroUsize::new(16 << 20).unwrap();
    let mut source = untracked_source(settings.clone())?;
    source.start(&lifecycle_ctx::start()).await?;
    database.admin.execute("BEGIN").await?;
    for batch in 0..20 {
        let values = (0..500)
            .map(|offset| format!("({}, 'row {}')", batch * 500 + offset, offset))
            .collect::<Vec<_>>()
            .join(",");
        database
            .admin
            .execute(&format!("INSERT INTO items VALUES {values}"))
            .await?;
    }
    database.admin.execute("COMMIT").await?;
    let envelope = tokio::time::timeout(Duration::from_secs(30), source.next())
        .await??
        .context("large transaction")?;
    assert_eq!(transaction_ids(&envelope, &settings)?.len(), 10_000);
    assert!(quiet(&mut source, 300).await);
    source.stop(&lifecycle_ctx::stop()).await?;
    database.admin.close().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Docker"]
async fn native_database_wide_selection_skips_unsupported_tables_and_decodes_extended_types(
) -> Result<()> {
    let mut database = Database::new(MySqlRetention::Server).await?;
    for statement in [
        "CREATE TABLE `order-items` (id INT PRIMARY KEY, shape POINT NULL, \
         label VARCHAR(20) CHARACTER SET latin1 NULL)",
        "CREATE TABLE keyless (name VARCHAR(10))",
    ] {
        database.admin.execute(statement).await?;
    }
    let mut settings = untracked(database.settings.connection.port, MySqlOutput::Changes);
    settings.tables = Vec::new();
    let mut source = untracked_source(settings)?;
    source.start(&lifecycle_ctx::start()).await?;
    database
        .admin
        .execute("INSERT INTO keyless VALUES ('ignored')")
        .await?;
    database
        .admin
        .execute(
            "INSERT INTO `order-items` VALUES (1, ST_GeomFromText('POINT(1.5 -2)'), \
             CONVERT(_utf8mb4'café' USING latin1))",
        )
        .await?;
    let envelope = next(&mut source).await?;
    let [drasi_lib::computation::v1::ChangeOperation::Added { after, .. }] =
        envelope.envelope.changes().operations()
    else {
        anyhow::bail!("expected one insert");
    };
    let element = drasi_lib::computation::v1::GraphChangeCodec::decode_element(after)?;
    let properties = element.properties();
    assert_eq!(
        format!("{:?}", properties.get("shape")),
        r#"Some(String("SRID=0;POINT(1.5 -2)"))"#
    );
    assert_eq!(
        format!("{:?}", properties.get("label")),
        r#"Some(String("café"))"#
    );
    assert!(quiet(&mut source, 300).await, "keyless table is skipped");
    source.stop(&lifecycle_ctx::stop()).await?;
    database.admin.close().await?;
    Ok(())
}
