// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use tokio::time::timeout;

fn config(output: SqliteOutput) -> SqliteConfig {
    SqliteConfig {
        path: None,
        tables: Vec::new(),
        output,
        replay: None,
        transactions: SourceTransactionLimits {
            max_changes: NonZeroUsize::new(32).unwrap(),
            max_bytes: NonZeroUsize::new(1 << 20).unwrap(),
            max_duration_ms: NonZeroU64::new(5000).unwrap(),
        },
        max_sql_bytes: NonZeroUsize::new(1 << 16).unwrap(),
        command_capacity: NonZeroUsize::new(4).unwrap(),
        output_capacity: NonZeroUsize::new(4).unwrap(),
        shutdown_timeout_ms: NonZeroU64::new(5000).unwrap(),
    }
}
fn source(config: SqliteConfig) -> Result<SqliteSource> {
    SqliteSource::new(
        ComponentId::try_new("sqlite")?,
        StreamId::try_new("changes")?,
        config,
    )
}
async fn next(source: &mut SqliteSource) -> Result<OutputEnvelope> {
    timeout(Duration::from_secs(2), source.next())
        .await??
        .context("source ended")
}
async fn no_output(source: &mut SqliteSource) {
    assert!(timeout(Duration::from_millis(20), source.next())
        .await
        .is_err());
}

#[tokio::test(flavor = "current_thread")]
async fn native_configuration_and_factory_reject_incompatible_bounds_and_resources() -> Result<()> {
    for invalid in [
        "commands",
        "outputs",
        "sql",
        "duration",
        "duplicate",
        "reserved",
    ] {
        let mut settings = config(SqliteOutput::Transactions);
        match invalid {
            "commands" => settings.command_capacity = NonZeroUsize::new(1).expect("positive"),
            "outputs" => {
                settings.output_capacity =
                    NonZeroUsize::new(tokio::sync::Semaphore::MAX_PERMITS + 1).expect("positive")
            }
            "sql" => {
                settings.max_sql_bytes = NonZeroUsize::new(i32::MAX as usize + 1).expect("positive")
            }
            "duration" => {
                settings.transactions.max_duration_ms =
                    NonZeroU64::new(i32::MAX as u64 + 1).expect("positive")
            }
            "duplicate" => settings.tables = vec!["items".into(), "items".into()],
            "reserved" => settings.tables = vec!["__DRASI_private".into()],
            _ => unreachable!(),
        }
        assert!(settings.validate().is_err(), "{invalid}");
        assert!(source(settings).is_err(), "{invalid}");
    }
    let factory = SqliteSourceFactory::default();
    let settings = config(SqliteOutput::Transactions);
    let specification = ComponentSpecification {
        descriptor: SqliteSource::describe(ComponentId::try_new("sqlite")?, settings.output)?,
        role: ComponentRole::Source,
        completion: None,
        implementation: factory.descriptor().implementation.clone(),
        configuration_version: 1,
        configuration: std::collections::BTreeMap::from([
            (
                Arc::from("stream"),
                ConfigurationValue::Literal("changes".into()),
            ),
            (
                Arc::from("settings"),
                ConfigurationValue::Literal(serde_json::to_value(&settings)?),
            ),
        ]),
        dependencies: std::collections::BTreeMap::from([(
            Arc::from("client"),
            vec![ResourceId::try_new("client")?],
        )]),
    };
    factory.validate(&specification)?;
    for invalid in ["schema", "progress", "snapshot", "flag"] {
        let mut specification = specification.clone();
        match invalid {
            "schema" => {
                specification.descriptor =
                    SqliteSource::describe(ComponentId::try_new("sqlite")?, SqliteOutput::Changes)?
            }
            "progress" => {
                specification.dependencies.insert(
                    Arc::from("source_progress"),
                    vec![ResourceId::try_new("progress")?],
                );
            }
            "snapshot" => {
                specification.configuration.insert(
                    Arc::from("coordinated_snapshot"),
                    ConfigurationValue::Literal(true.into()),
                );
            }
            "flag" => {
                specification.configuration.insert(
                    Arc::from("coordinated_snapshot"),
                    ConfigurationValue::Literal("true".into()),
                );
            }
            _ => unreachable!(),
        }
        assert!(factory.validate(&specification).is_err(), "{invalid}");
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn paired_snapshot_requires_the_actual_source_configuration_and_progress_owner() -> Result<()>
{
    let owner = || -> Result<_> {
        Ok(Arc::new(QuerySourceProgress::new(
            "test",
            ComponentId::try_new("query")?,
        )?))
    };
    let progress = owner()?;
    let mut settings = config(SqliteOutput::Transactions);
    settings.path = Some("not-opened-by-construction.db".into());
    settings.replay = Some(SqliteReplayConfig {
        max_transactions: NonZeroUsize::new(8).expect("positive"),
        max_bytes: NonZeroUsize::new(8 << 20).expect("positive"),
    });
    let make = |settings, progress| {
        SqliteSource::new_replayable(
            ComponentId::try_new("sqlite")?,
            StreamId::try_new("changes")?,
            settings,
            progress,
        )
    };
    let (producer, snapshot) = SqliteSource::coordinated(
        ComponentId::try_new("sqlite")?,
        StreamId::try_new("changes")?,
        settings.clone(),
        progress.clone(),
    )?;
    snapshot.validate_source(&producer)?;
    snapshot.validate_source(&make(settings.clone(), progress.clone())?)?;
    assert!(snapshot
        .validate_source(&make(settings.clone(), owner()?)?)
        .is_err());
    let mut changed = settings.clone();
    changed.tables = vec!["items".into()];
    assert!(snapshot
        .validate_source(&make(changed.clone(), progress.clone())?)
        .is_err());
    let mismatched = Arc::new(SqliteClientResource::new(
        ComponentId::try_new("sqlite")?,
        changed,
    )?);
    assert!(make(settings.clone(), progress.clone())?
        .with_client_resource(mismatched)
        .is_err());
    let client = Arc::new(SqliteClientResource::new(
        ComponentId::try_new("sqlite")?,
        settings.clone(),
    )?);
    let bound = producer.with_client_resource(client.clone())?;
    assert!(make(settings.clone(), progress.clone())?
        .with_client_resource(client.clone())
        .is_err());
    drop(bound);
    make(settings, progress)?.with_client_resource(client)?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn both_native_modes_publish_only_after_the_isolated_scope_commits() -> Result<()> {
    for mode in [SqliteOutput::Changes, SqliteOutput::Transactions] {
        let config = config(mode);
        let limits = config.transactions;
        let mut source = source(config)?;
        source.start().await?;
        let handle = source.handle();
        handle
            .execute("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT)")
            .await?;
        let (entered, started) = oneshot::channel();
        let (finish, resume) = oneshot::channel();
        let mut transaction = Box::pin(handle.transaction(|tx| async move {
            tx.execute("INSERT INTO items VALUES(1,'initial')").await?;
            tx.execute("UPDATE items SET value='final' WHERE id=1")
                .await?;
            entered.send(()).unwrap();
            resume.await?;
            Ok(())
        }));
        tokio::select! { result = &mut transaction => panic!("{result:?}"), result = started => result? }
        no_output(&mut source).await;
        let other = handle.query("SELECT value FROM items");
        tokio::pin!(other);
        assert!(timeout(Duration::from_millis(20), &mut other)
            .await
            .is_err());
        finish.send(()).unwrap();
        transaction.await?;
        assert_eq!(
            serde_json::to_value(other.await?)?,
            serde_json::json!([{"value":"final"}])
        );
        let output = next(&mut source).await?;
        let changes = match mode {
            SqliteOutput::Changes => GraphChangeCodec::decode_changes(&output.envelope)?,
            SqliteOutput::Transactions => {
                SourceTransactionCodec::decode(&output.envelope, limits)?.into_changes()
            }
        };
        assert_eq!(changes.len(), 2);
        source.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancellation_rolls_back_and_revokes_escaped_handles_before_other_work() -> Result<()> {
    let mut source = source(config(SqliteOutput::Transactions))?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let (entered, captured) = oneshot::channel();
    let mut transaction = Box::pin(handle.transaction(|tx| async move {
        tx.execute("INSERT INTO items VALUES(1)").await?;
        entered.send(tx).ok().context("capture closed")?;
        std::future::pending::<Result<()>>().await
    }));
    let tx = tokio::select! { result = &mut transaction => panic!("{result:?}"), result = captured => result? };
    let mut pending = Vec::new();
    for _ in 0..4 {
        let mut query = Box::pin(handle.query("SELECT id FROM items"));
        assert!(timeout(Duration::from_millis(5), &mut query).await.is_err());
        pending.push(query);
    }
    assert_eq!(source.commands.read().await.as_ref().unwrap().capacity(), 0);
    drop(pending);
    drop(transaction);
    assert!(tx.execute("INSERT INTO items VALUES(2)").await.is_err());
    assert!(handle.query("SELECT id FROM items").await?.is_empty());
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn caught_statement_errors_and_failed_commits_never_publish_or_leave_rows() -> Result<()> {
    let mut source = source(config(SqliteOutput::Transactions))?;
    source.start().await?;
    let handle = source.handle();
    handle.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY);
         CREATE TABLE children(id INTEGER PRIMARY KEY, parent INTEGER REFERENCES items(id) DEFERRABLE INITIALLY DEFERRED);"
    ).await?;
    let result = handle
        .transaction(|tx| async move {
            tx.execute("INSERT INTO items VALUES(1)").await?;
            assert!(tx
                .execute("INSERT OR FAIL INTO items VALUES(2),(2)")
                .await
                .is_err());
            Ok(())
        })
        .await;
    assert!(result.is_err());
    assert!(handle.query("SELECT * FROM items").await?.is_empty());
    assert!(handle
        .execute("INSERT INTO children VALUES(1,999)")
        .await
        .is_err());
    assert!(handle.query("SELECT * FROM children").await?.is_empty());
    no_output(&mut source).await;
    handle.execute("INSERT INTO items VALUES(3)").await?;
    next(&mut source).await?;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn parsed_savepoints_and_key_changes_preserve_exact_committed_changes() -> Result<()> {
    let config = config(SqliteOutput::Transactions);
    let limits = config.transactions;
    let mut source = source(config)?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id TEXT PRIMARY KEY, value TEXT)")
        .await?;
    handle
        .transaction(|tx| async move {
            tx.execute_batch(
                "INSERT INTO items VALUES('a','one');
             /* quoted identifier */ SAVEPOINT \"outer point\";
             INSERT INTO items VALUES('b','two');
             SAVEPOINT \"inner point\";
             INSERT INTO items VALUES('c','three');
             RELEASE \"outer point\";
             SAVEPOINT \"outer point\";
             INSERT INTO items VALUES('d','four');
             ROLLBACK TO \"outer point\";
             RELEASE \"outer point\";
             UPDATE items SET id='new:a' WHERE id='a';",
            )
            .await?;
            Ok(())
        })
        .await?;
    assert_eq!(handle.query("SELECT id FROM items").await?.len(), 3);
    let output = next(&mut source).await?;
    let changes = SourceTransactionCodec::decode(&output.envelope, limits)?.into_changes();
    assert_eq!(changes.len(), 5);
    assert!(matches!(
        &changes[3],
        drasi_core::models::SourceChange::Delete { .. }
    ));
    assert!(matches!(
        &changes[4],
        drasi_core::models::SourceChange::Insert { .. }
    ));
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn authorizer_denies_raw_boundaries_and_query_mutations() -> Result<()> {
    let mut source = source(config(SqliteOutput::Changes))?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    for sql in [
        "BEGIN",
        "END",
        "COMMIT",
        "ROLLBACK",
        "/* comment */ END TRANSACTION",
        "PRAGMA foreign_keys=OFF",
        "ATTACH ':memory:' AS extra",
    ] {
        assert!(
            handle
                .execute_batch(format!("INSERT INTO items VALUES(1); {sql};"))
                .await
                .is_err(),
            "{sql}"
        );
        assert!(handle.query("SELECT * FROM items").await?.is_empty());
    }
    assert!(handle
        .query("INSERT INTO items VALUES(1) RETURNING id")
        .await
        .is_err());
    assert!(handle
        .query("SELECT 1; INSERT INTO items VALUES(1)")
        .await
        .is_err());
    assert!(handle.query("SAVEPOINT outside_scope").await.is_err());
    assert!(handle
        .transaction(|tx| async move {
            tx.execute("INSERT INTO items VALUES(1)").await?;
            assert!(tx.query("SAVEPOINT query_escape").await.is_err());
            Ok(())
        })
        .await
        .is_err());
    assert!(handle.query("SELECT * FROM items").await?.is_empty());
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn stop_rolls_back_a_scope_and_old_handles_cannot_reach_replacement() -> Result<()> {
    let directory = tempfile::TempDir::new()?;
    let mut config = config(SqliteOutput::Transactions);
    config.path = Some(directory.path().join("source.db"));
    let mut source = source(config)?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let (entered, captured) = oneshot::channel();
    let (finish, resume) = oneshot::channel();
    let mut transaction = Box::pin(handle.transaction(|tx| async move {
        tx.execute("INSERT INTO items VALUES(1)").await?;
        entered.send(tx.clone()).ok().context("capture closed")?;
        resume.await?;
        tx.execute("INSERT INTO items VALUES(2)").await?;
        Ok(())
    }));
    let tx = tokio::select! { result = &mut transaction => panic!("{result:?}"), result = captured => result? };
    source.stop().await?;
    source.start().await?;
    assert!(tx.execute("INSERT INTO items VALUES(3)").await.is_err());
    finish.send(()).unwrap();
    assert!(transaction.await.is_err());
    assert!(handle.query("SELECT id FROM items").await?.is_empty());
    handle.execute("INSERT INTO items VALUES(4)").await?;
    next(&mut source).await?;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn oversized_capture_is_rejected_atomically_and_increased_limits_allow_retry() -> Result<()> {
    let directory = tempfile::TempDir::new()?;
    let mut config = config(SqliteOutput::Transactions);
    config.path = Some(directory.path().join("source.db"));
    config.transactions.max_changes = NonZeroUsize::new(1).unwrap();
    let mut first = source(config.clone())?;
    first.start().await?;
    first
        .handle()
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    assert!(first
        .handle()
        .execute("INSERT INTO items VALUES(1),(2)")
        .await
        .is_err());
    assert!(first
        .handle()
        .query("SELECT * FROM items")
        .await?
        .is_empty());
    no_output(&mut first).await;
    first.stop().await?;
    config.transactions.max_changes = NonZeroUsize::new(2).unwrap();
    let mut second = source(config)?;
    second.start().await?;
    second
        .handle()
        .execute("INSERT INTO items VALUES(1),(2)")
        .await?;
    next(&mut second).await?;
    second.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn local_validation_errors_also_abort_a_scope_even_when_caught() -> Result<()> {
    let mut source = source(config(SqliteOutput::Transactions))?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    for sql in ["SELECT \0".to_string(), "x".repeat(65537)] {
        assert!(handle
            .transaction(|tx| async move {
                tx.execute("INSERT INTO items VALUES(1)").await?;
                assert!(tx.execute(sql).await.is_err());
                Ok(())
            })
            .await
            .is_err());
        assert!(handle.query("SELECT * FROM items").await?.is_empty());
    }
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn idle_scopes_and_expensive_queries_obey_real_execution_deadlines() -> Result<()> {
    let mut config = config(SqliteOutput::Transactions);
    config.transactions.max_duration_ms = NonZeroU64::new(60).unwrap();
    let mut source = source(config)?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let (entered, ready) = oneshot::channel();
    let (finish, resume) = oneshot::channel();
    let mut transaction = Box::pin(handle.transaction(|tx| async move {
        tx.execute("INSERT INTO items VALUES(1)").await?;
        entered.send(()).unwrap();
        resume.await?;
        Ok(())
    }));
    tokio::select! { result = &mut transaction => panic!("{result:?}"), result = ready => result? }
    tokio::time::sleep(Duration::from_millis(90)).await;
    assert!(handle.query("SELECT * FROM items").await?.is_empty());
    finish.send(()).unwrap();
    assert!(transaction
        .await
        .unwrap_err()
        .to_string()
        .contains("deadline"));
    let began = tokio::time::Instant::now();
    assert!(timeout(Duration::from_secs(2), handle.query(
        "WITH RECURSIVE numbers(x) AS (VALUES(0) UNION ALL SELECT x+1 FROM numbers WHERE x<1000000000) SELECT sum(x) FROM numbers"
    )).await?.is_err());
    assert!(began.elapsed() >= Duration::from_millis(60));
    let began = tokio::time::Instant::now();
    let error = timeout(
        Duration::from_secs(2),
        handle.transaction(|_| std::future::pending::<Result<()>>()),
    )
    .await?
    .unwrap_err();
    assert!(error.to_string().contains("deadline"));
    assert!(began.elapsed() >= Duration::from_millis(60));
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_keys_cover_without_rowid_composites_blobs_and_cascades() -> Result<()> {
    let config = config(SqliteOutput::Transactions);
    let limits = config.transactions;
    let mut source = source(config)?;
    source.start().await?;
    let handle = source.handle();
    handle.execute_batch(
        "CREATE TABLE items(a TEXT,b TEXT,PRIMARY KEY(a,b)) WITHOUT ROWID;
         CREATE TABLE blobs(id PRIMARY KEY);
         CREATE TABLE parent(id INTEGER PRIMARY KEY);
         CREATE TABLE child(id INTEGER PRIMARY KEY,parent INTEGER REFERENCES parent(id) ON DELETE CASCADE);"
    ).await?;
    handle.execute_batch(
        "INSERT INTO items VALUES('a:b','c'),('a','b:c');
         INSERT INTO blobs VALUES(X'61'),('YQ==');
         INSERT INTO parent VALUES(1); INSERT INTO child VALUES(1,1); DELETE FROM parent WHERE id=1;"
    ).await?;
    let changes =
        SourceTransactionCodec::decode(&next(&mut source).await?.envelope, limits)?.into_changes();
    assert_eq!(changes.len(), 8);
    let references: std::collections::BTreeSet<_> = changes[..4]
        .iter()
        .map(|change| change.get_reference().element_id.to_string())
        .collect();
    assert_eq!(
        references.len(),
        4,
        "composite and BLOB/text keys must not collide"
    );
    assert!(handle.query("SELECT * FROM child").await?.is_empty());
    assert!(handle
        .query("SELECT 1 AS duplicate,2 AS duplicate")
        .await
        .is_err());
    assert!(handle
        .execute("INSERT INTO blobs VALUES(NULL)")
        .await
        .is_err());
    assert!(handle
        .execute("INSERT INTO items VALUES(CAST(X'ff' AS TEXT),'bad')")
        .await
        .is_err());
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn panicked_callback_rolls_back_before_the_next_handle_operation() -> Result<()> {
    let mut source = source(config(SqliteOutput::Transactions))?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let writer = handle.clone();
    let callback = tokio::spawn(async move {
        writer
            .transaction(|tx| async move {
                tx.execute("INSERT INTO items VALUES(1)").await?;
                panic!("native SQLite callback panic");
                #[allow(unreachable_code)]
                Result::<()>::Ok(())
            })
            .await
    });
    assert!(callback.await.unwrap_err().is_panic());
    assert!(handle.query("SELECT * FROM items").await?.is_empty());
    no_output(&mut source).await;
    source.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_and_timed_out_stop_retains_a_submitted_commit_and_client_owner() -> Result<()> {
    let directory = tempfile::TempDir::new()?;
    let mut config = config(SqliteOutput::Changes);
    config.path = Some(directory.path().join("source.db"));
    config.shutdown_timeout_ms = NonZeroU64::new(30).unwrap();
    let client = Arc::new(SqliteClientResource::new(
        ComponentId::try_new("sqlite")?,
        config.clone(),
    )?);
    let mut source = source(config.clone())?.with_client_resource(client.clone())?;
    source.start().await?;
    let handle = client.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY)")
        .await?;
    let reader = rusqlite::Connection::open(directory.path().join("source.db"))?;
    reader.execute_batch("BEGIN")?;
    reader.query_row("SELECT count(*) FROM items", [], |row| {
        row.get::<_, usize>(0)
    })?;
    let mut write = Box::pin(handle.execute("INSERT INTO items VALUES(1)"));
    assert!(timeout(Duration::from_millis(30), &mut write)
        .await
        .is_err());
    assert!(timeout(Duration::from_millis(1), source.stop())
        .await
        .is_err());
    let began = tokio::time::Instant::now();
    let error = source.stop().await.unwrap_err();
    assert!(error.is::<drasi_lib::context::workers::WorkerCleanupError>());
    assert!(began.elapsed() >= Duration::from_millis(30));
    assert!(source.worker.read().await.is_some());
    assert!(source.start().await.is_err());
    assert_eq!(client.owner.available_permits(), 0);
    assert!(SqliteSource::new(
        ComponentId::try_new("sqlite")?,
        StreamId::try_new("changes")?,
        config.clone()
    )?
    .with_client_resource(client.clone())
    .is_err());
    reader.execute_batch("COMMIT")?;
    assert_eq!(timeout(Duration::from_secs(2), &mut write).await??, 1);
    source.stop().await?;
    assert_eq!(
        reader.query_row("SELECT count(*) FROM items", [], |row| row
            .get::<_, usize>(0))?,
        1
    );
    drop(source);
    assert_eq!(client.owner.available_permits(), 1);
    let mut replacement = SqliteSource::new(
        ComponentId::try_new("sqlite")?,
        StreamId::try_new("changes")?,
        config,
    )?
    .with_client_resource(client)?;
    replacement.start().await?;
    assert_eq!(handle.query("SELECT * FROM items").await?.len(), 1);
    replacement.stop().await?;
    Ok(())
}
