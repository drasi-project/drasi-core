// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_core::computation::ComputationIndexProvider;
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::computation::v1::*;
use drasi_source_sqlite::native::{
    SqliteClientResource, SqliteConfig, SqliteOutput, SqliteReplayConfig, SqliteSource,
    SqliteSourceFactory,
};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::time::timeout;

#[path = "native/progress.rs"]
mod progress;
#[path = "native/snapshot.rs"]
mod snapshot;

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("positive test bound")
}
fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("test component ID")
}
fn stream(value: &str) -> StreamId {
    StreamId::try_new(value).expect("test stream ID")
}
fn limits() -> SourceTransactionLimits {
    SourceTransactionLimits {
        max_changes: size(64),
        max_bytes: size(1 << 20),
        max_duration_ms: NonZeroU64::new(5000).expect("transaction deadline"),
    }
}
fn config(path: &Path) -> SqliteConfig {
    SqliteConfig {
        path: Some(path.join("source.db")),
        tables: Vec::new(),
        output: SqliteOutput::Transactions,
        replay: Some(SqliteReplayConfig {
            max_transactions: size(8),
            max_bytes: size(8 << 20),
        }),
        transactions: limits(),
        max_sql_bytes: size(65536),
        command_capacity: size(4),
        output_capacity: size(2),
        shutdown_timeout_ms: NonZeroU64::new(5000).expect("cleanup deadline"),
    }
}
fn progress() -> Arc<QuerySourceProgress> {
    Arc::new(QuerySourceProgress::new("native-sqlite", id("query")).expect("query progress"))
}
async fn query(
    path: &Path,
    progress: Arc<QuerySourceProgress>,
) -> Result<ContinuousQueryTransformer> {
    query_with_recovery(path, progress, QueryRecoveryPolicy::Strict).await
}
async fn query_with_recovery(
    path: &Path,
    progress: Arc<QuerySourceProgress>,
    recovery: QueryRecoveryPolicy,
) -> Result<ContinuousQueryTransformer> {
    let provider: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
        path.join("query"),
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    ));
    ContinuousQueryTransformer::new_configured(
        ContinuousQueryDefinition {
            graph_id: "native-sqlite".into(),
            id: id("query"),
            query: "MATCH (n:items) RETURN n.id AS id, n.value AS value".into(),
            language: ComputationQueryLanguage::Cypher,
            output_stream: stream("results"),
            outbox_capacity: size(8),
        },
        provider,
        QueryOptions {
            recovery,
            ..Default::default()
        },
        QueryExecutionSettings {
            source_transactions: Some(limits()),
            ..Default::default()
        },
        None,
    )
    .await?
    .with_source_progress(progress)
}
fn source(config: SqliteConfig, progress: Arc<QuerySourceProgress>) -> Result<SqliteSource> {
    SqliteSource::new_replayable(id("source"), stream("changes"), config, progress)
}
async fn next(source: &mut SqliteSource) -> Result<OutputEnvelope> {
    timeout(Duration::from_secs(3), source.next())
        .await??
        .context("source ended")
}
async fn apply(
    query: &mut ContinuousQueryTransformer,
    output: OutputEnvelope,
) -> Result<Vec<OutputEnvelope>> {
    let result = query
        .transform(InputEnvelope {
            port: PortId::try_new("in")?,
            envelope: output.envelope,
        })
        .await?;
    query.delivery_completed(&result).await?;
    Ok(result)
}
fn rows(query: &ContinuousQueryTransformer) -> Vec<serde_json::Value> {
    let mut rows: Vec<_> = query
        .results()
        .snapshot()
        .expect("query result snapshot")
        .rows
        .values()
        .map(|row| {
            QueryChangeCodec::row_values_to_json(
                &QueryChangeCodec::decode_row(row)
                    .expect("query result row")
                    .values,
            )
        })
        .collect();
    rows.sort_by_key(|row| row.to_string());
    rows
}
fn journal_state(path: &Path) -> Result<(u64, u64, u64)> {
    Ok(
        rusqlite::Connection::open(path.join("source.db"))?.query_row(
            "SELECT head,retired,bytes FROM __drasi_native_state_v1 WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?,
    )
}

#[tokio::test(flavor = "current_thread")]
async fn durable_sqlite_replays_whole_transactions_and_exposes_only_final_query_rows() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(directory.path(), progress.clone()).await?;
    query.start().await?;
    let mut source = source(config(directory.path()), progress.clone())?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT)")
        .await?;
    handle
        .execute_batch(
            "INSERT INTO items VALUES(1,'first'); UPDATE items SET value='final';
         INSERT INTO items VALUES(2,'temporary'); DELETE FROM items WHERE id=2;",
        )
        .await?;
    let original = next(&mut source).await?;
    assert_eq!(
        SourceTransactionCodec::decode(&original.envelope, limits())?
            .into_changes()
            .len(),
        4
    );
    assert_eq!(
        journal_state_after_stop(&mut source, directory.path())
            .await?
            .0,
        1
    );
    source.start().await?;
    let replay = next(&mut source).await?;
    assert_eq!(
        original.envelope.system().source_position(),
        replay.envelope.system().source_position()
    );
    assert!(replay.envelope.system().sequence() > original.envelope.system().sequence());
    let duplicate = replay.clone();
    let output = apply(&mut query, replay).await?;
    assert_eq!(
        output
            .iter()
            .map(|output| output.envelope.changes().operations().len())
            .sum::<usize>(),
        1
    );
    assert_eq!(rows(&query), [serde_json::json!({"id":1,"value":"final"})]);
    assert!(apply(&mut query, duplicate).await?.is_empty());
    handle.query("SELECT id FROM items").await?;
    source.stop().await?;
    assert_eq!(journal_state(directory.path())?, (1, 1, 0));
    query.stop().await?;
    Ok(())
}

async fn journal_state_after_stop(
    source: &mut SqliteSource,
    path: &Path,
) -> Result<(u64, u64, u64)> {
    source.stop().await?;
    journal_state(path)
}

#[tokio::test(flavor = "current_thread")]
async fn journal_capacity_and_failed_commit_roll_back_both_sql_and_replay_records() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(directory.path(), progress.clone()).await?;
    query.start().await?;
    let mut settings = config(directory.path());
    settings
        .replay
        .as_mut()
        .expect("replay configuration")
        .max_transactions = size(1);
    let mut source = source(settings, progress)?;
    source.start().await?;
    let handle = source.handle();
    handle.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT);
        CREATE TABLE child(id INTEGER PRIMARY KEY,parent INTEGER REFERENCES items(id) DEFERRABLE INITIALLY DEFERRED);").await?;
    assert!(handle
        .execute("INSERT INTO child VALUES(1,999)")
        .await
        .is_err());
    assert!(handle.query("SELECT * FROM child").await?.is_empty());
    handle
        .execute("INSERT INTO items VALUES(1,'first')")
        .await?;
    assert!(handle
        .execute("INSERT INTO items VALUES(2,'second')")
        .await
        .unwrap_err()
        .to_string()
        .contains("journal is full"));
    assert_eq!(handle.query("SELECT * FROM items").await?.len(), 1);
    apply(&mut query, next(&mut source).await?).await?;
    handle.query("SELECT 1").await?;
    handle
        .execute("INSERT INTO items VALUES(2,'second')")
        .await?;
    apply(&mut query, next(&mut source).await?).await?;
    handle.query("SELECT 1").await?;
    source.stop().await?;
    assert_eq!(journal_state(directory.path())?, (2, 2, 0));
    assert_eq!(rows(&query).len(), 2);
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn application_sql_cannot_shadow_or_attach_side_effects_to_the_replay_journal() -> Result<()>
{
    let directory = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(directory.path(), progress.clone()).await?;
    query.start().await?;
    let mut source = source(config(directory.path()), progress)?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT)")
        .await?;
    for sql in [
        "CREATE TEMP TABLE __drasi_native_outbox_v1(sequence INTEGER PRIMARY KEY,timestamp INTEGER,payload BLOB)",
        "CREATE TRIGGER unwanted AFTER INSERT ON __drasi_native_outbox_v1 BEGIN INSERT INTO items VALUES(99,'unrecorded'); END",
        "CREATE INDEX unwanted_index ON __drasi_native_outbox_v1(timestamp)",
        "CREATE VIEW __drasi_alias AS SELECT 1",
        "CREATE TABLE unwanted(id INTEGER PRIMARY KEY,parent INTEGER REFERENCES __drasi_native_state_v1(singleton))",
        "UPDATE __drasi_native_state_v1 SET retired=head",
    ] {
        assert!(handle.execute_batch(format!("INSERT INTO items VALUES(1,'rollback'); {sql}")).await.is_err(), "{sql}");
        assert!(handle.query("SELECT * FROM items").await?.is_empty());
    }
    assert!(handle
        .query("SELECT * FROM __drasi_native_outbox_v1")
        .await
        .is_err());
    handle
        .execute("INSERT INTO items VALUES(2,'valid')")
        .await?;
    apply(&mut query, next(&mut source).await?).await?;
    handle.query("SELECT 1").await?;
    source.stop().await?;
    assert_eq!(journal_state(directory.path())?, (1, 1, 0));
    assert_eq!(rows(&query), [serde_json::json!({"id":2,"value":"valid"})]);
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn replay_requires_real_file_storage_and_never_adopts_existing_data() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let progress = progress();
    let mut query = query(directory.path(), progress.clone()).await?;
    query.start().await?;
    for alias in ["", ":memory:", "file::memory:?cache=shared"] {
        let mut settings = config(directory.path());
        settings.path = Some(alias.into());
        let mut source = source(settings, progress.clone())?;
        assert!(source
            .start()
            .await
            .unwrap_err()
            .to_string()
            .contains("persistent database file"));
        source.stop().await?;
    }
    let connection = rusqlite::Connection::open(directory.path().join("source.db"))?;
    connection.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'existing');")?;
    drop(connection);
    let mut source = source(config(directory.path()), progress)?;
    assert!(source
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("coordinated bootstrap"));
    source.stop().await?;
    let connection = rusqlite::Connection::open(directory.path().join("source.db"))?;
    assert_eq!(
        connection.query_row("SELECT count(*) FROM items", [], |row| row
            .get::<_, usize>(0))?,
        1
    );
    assert_eq!(
        connection.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name='__drasi_native_state_v1'",
            [],
            |row| row.get::<_, usize>(0)
        )?,
        0
    );
    query.stop().await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn durable_start_refuses_rebinding_missing_history_and_external_schema_changes() -> Result<()>
{
    for fault in ["binding", "missing", "schema", "disable"] {
        let directory = tempfile::tempdir()?;
        let progress = progress();
        let mut query = query(directory.path(), progress.clone()).await?;
        query.start().await?;
        let settings = config(directory.path());
        let mut first = source(settings.clone(), progress.clone())?;
        first.start().await?;
        first
            .handle()
            .execute("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT)")
            .await?;
        first
            .handle()
            .execute("INSERT INTO items VALUES(1,'first')")
            .await?;
        first.stop().await?;
        let connection = rusqlite::Connection::open(directory.path().join("source.db"))?;
        match fault {
            "missing" => {
                connection.execute("DELETE FROM __drasi_native_outbox_v1", [])?;
            }
            "schema" => {
                connection.execute("ALTER TABLE items ADD COLUMN extra TEXT", [])?;
            }
            _ => {}
        }
        drop(connection);
        let mut next_settings = settings;
        let mut second = if fault == "disable" {
            next_settings.replay = None;
            SqliteSource::new(id("source"), stream("changes"), next_settings)?
        } else if fault == "binding" {
            next_settings.tables = vec!["items".into()];
            source(next_settings, progress)?
        } else {
            source(next_settings, progress)?
        };
        assert!(second.start().await.is_err(), "{fault}");
        second.stop().await?;
        query.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn query_deprovision_cannot_silently_resume_after_sqlite_history_retirement() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let owner = progress();
    let mut consumer = query(directory.path(), owner.clone()).await?;
    consumer.start().await?;
    let mut producer = source(config(directory.path()), owner)?;
    producer.start().await?;
    producer.handle().execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(1,'retained data')",
    ).await?;
    apply(&mut consumer, next(&mut producer).await?).await?;
    producer.handle().query("SELECT 1").await?;
    producer.stop().await?;
    assert_eq!(journal_state(directory.path())?, (1, 1, 0));
    consumer.stop().await?;
    consumer.deprovision().await?;
    drop(consumer);
    drop(producer);

    let owner = progress();
    let mut consumer = query(directory.path(), owner.clone()).await?;
    consumer.start().await?;
    let mut producer = source(config(directory.path()), owner)?;
    let error = producer.start().await.expect_err("missing retired cursor");
    assert!(
        error
            .to_string()
            .contains("no cursor for already retired history"),
        "{error:#}"
    );
    producer.stop().await?;
    consumer.stop().await?;
    assert_eq!(journal_state(directory.path())?, (1, 1, 0));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn reduced_limits_and_corrupt_records_never_discard_or_publish_retained_transactions(
) -> Result<()> {
    for fault in ["count", "bytes", "payload", "identity"] {
        let directory = tempfile::tempdir()?;
        let owner = progress();
        let mut consumer = query(directory.path(), owner.clone()).await?;
        consumer.start().await?;
        let mut settings = config(directory.path());
        let mut producer = source(settings.clone(), owner.clone())?;
        producer.start().await?;
        producer
            .handle()
            .execute("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT)")
            .await?;
        producer
            .handle()
            .execute("INSERT INTO items VALUES(1,'first')")
            .await?;
        producer
            .handle()
            .execute("INSERT INTO items VALUES(2,'second')")
            .await?;
        producer.stop().await?;
        let before = journal_state(directory.path())?;
        match fault {
            "count" => settings.replay.as_mut().context("replay")?.max_transactions = size(1),
            "bytes" => settings.replay.as_mut().context("replay")?.max_bytes = size(1),
            "payload" | "identity" => {
                let connection = rusqlite::Connection::open(directory.path().join("source.db"))?;
                if fault == "payload" {
                    connection.execute("UPDATE __drasi_native_outbox_v1 SET payload=zeroblob(length(payload)) WHERE sequence=1", [])?;
                } else {
                    connection.execute(
                        "UPDATE __drasi_native_state_v1 SET epoch=?1",
                        [uuid::Uuid::new_v4().to_string()],
                    )?;
                }
            }
            _ => unreachable!(),
        }
        drop(producer);
        let mut producer = source(settings, owner)?;
        match producer.start().await {
            Err(error) => assert!(
                error.to_string().contains("exceeds configured limits"),
                "{fault}: {error:#}"
            ),
            Ok(()) => {
                let error = next(&mut producer)
                    .await
                    .expect_err("invalid record must not publish");
                if fault == "identity" {
                    assert!(
                        error.to_string().contains("identity is damaged"),
                        "{error:#}"
                    );
                }
            }
        }
        producer.stop().await?;
        consumer.stop().await?;
        assert_eq!(journal_state(directory.path())?, before, "{fault}");
        assert!(rows(&consumer).is_empty(), "{fault}");
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_sqlite_connected_graph_delivers_only_the_final_transaction_result() -> Result<()> {
    for (factory_created, coordinated) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let directory = tempfile::tempdir()?;
        let progress = progress();
        let settings = config(directory.path());
        if coordinated {
            let connection = rusqlite::Connection::open(directory.path().join("source.db"))?;
            connection.execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT); INSERT INTO items VALUES(0,'initial');")?;
        }
        let (producer, snapshot) = if coordinated {
            let (producer, snapshot) = SqliteSource::coordinated(
                id("source"),
                stream("changes"),
                settings.clone(),
                progress.clone(),
            )?;
            (producer, Some(snapshot))
        } else {
            (source(settings.clone(), progress.clone())?, None)
        };
        let mut query = query(directory.path(), progress.clone()).await?;
        if let Some(snapshot) = &snapshot {
            query = query.with_bootstrap(snapshot.clone());
        }
        let results = query.results();
        let client = Arc::new(SqliteClientResource::new(id("source"), settings.clone())?);
        let handle = client.handle();
        let (sink, mut received) = drasi_reaction_application::NativeApplicationReaction::channel(
            "sink",
            QueryChangeCodec::schema().descriptor().clone(),
            size(1),
        )?;
        let endpoint = |component, port| {
            Endpoint::new(id(component), PortId::try_new(port).expect("test port"))
        };
        let graph = ComputationGraph::builder("native-sqlite")
            .query(Box::new(query))
            .sink(Box::new(sink));
        let graph = if factory_created {
            let factory = Arc::new(SqliteSourceFactory::default());
            let mut graph = graph;
            let mut resources = vec![
                (
                    "client",
                    ResourceRole::Component,
                    ResourceHandle::new(ResourceRole::Component, client.clone()),
                ),
                (
                    "progress",
                    ResourceRole::Checkpoint,
                    ResourceHandle::new(
                        ResourceRole::Checkpoint,
                        Arc::new(QuerySourceProgressResource(progress.clone())),
                    ),
                ),
            ];
            let mut dependencies = std::collections::BTreeMap::from([
                (Arc::from("client"), vec![ResourceId::try_new("client")?]),
                (
                    Arc::from("source_progress"),
                    vec![ResourceId::try_new("progress")?],
                ),
            ]);
            if let Some(snapshot) = snapshot {
                resources.push((
                    "snapshot",
                    ResourceRole::Bootstrap,
                    ResourceHandle::new(ResourceRole::Bootstrap, snapshot),
                ));
                dependencies.insert(
                    Arc::from("snapshot"),
                    vec![ResourceId::try_new("snapshot")?],
                );
            }
            for (name, role, handle) in resources {
                let resource = ResourceId::try_new(name)?;
                graph = graph
                    .declare_resource(ResourceSpecification {
                        id: resource.clone(),
                        role,
                        ownership: ResourceOwnership::Borrowed,
                        binding: Arc::from(name),
                    })?
                    .provide_resource(resource, handle)?;
            }
            graph.component(
                ComponentSpecification {
                    descriptor: SqliteSource::describe(id("source"), settings.output)?,
                    role: ComponentRole::Source,
                    completion: None,
                    implementation: factory.descriptor().implementation.clone(),
                    configuration_version: 1,
                    configuration: std::collections::BTreeMap::from([
                        (
                            Arc::from("coordinated_snapshot"),
                            ConfigurationValue::Literal(coordinated.into()),
                        ),
                        (
                            Arc::from("stream"),
                            ConfigurationValue::Literal("changes".into()),
                        ),
                        (
                            Arc::from("settings"),
                            ConfigurationValue::Literal(serde_json::to_value(settings)?),
                        ),
                    ]),
                    dependencies,
                },
                factory,
            )
        } else {
            graph.source(Box::new(producer.with_client_resource(client)?))
        };
        let mut graph = graph
            .bind_stream(endpoint("source", "out"), stream("changes"))
            .bind_stream(endpoint("query", "out"), stream("results"))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint("query", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .connect(
                EdgeDefinition::new(endpoint("query", "out"), endpoint("sink", "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            )
            .build()?;
        let run = graph.run()?;
        let control = run.control();
        let (run_result, result) = timeout(Duration::from_secs(20), async {
            tokio::join!(run, async {
                let result = async {
                    let report = control
                        .start_components(GraphRevision(1), GraphSelection::All)
                        .await?;
                    anyhow::ensure!(
                        report.summary == OperationSummary::Completed,
                        "graph startup: {report:?}"
                    );
                    assert_eq!(results.snapshot()?.rows.len(), usize::from(coordinated));
                    if !coordinated {
                        handle.execute("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT)").await?;
                    }
                    handle
                        .execute_batch(
                            "INSERT INTO items VALUES(1,'first'); UPDATE items SET value='final' WHERE id=1",
                        )
                        .await?;
                    let output = received.recv().await.context("sink closed")?;
                    let [ChangeOperation::Added { after, .. }] =
                        output.envelope.changes().operations()
                    else {
                        anyhow::bail!("expected one final transaction result");
                    };
                    assert_eq!(
                        QueryChangeCodec::row_values_to_json(
                            &QueryChangeCodec::decode_row(after)?.values
                        ),
                        serde_json::json!({"id":1,"value":"final"})
                    );
                    let report = control
                        .stop_components(GraphRevision(1), GraphSelection::All)
                        .await?;
                    anyhow::ensure!(
                        report.summary == OperationSummary::Completed,
                        "graph stop: {report:?}"
                    );
                    Result::<()>::Ok(())
                }
                .await;
                control.cancel();
                result
            })
        })
        .await?;
        result?;
        assert!(matches!(run_result, Err(GraphError::Cancelled)));
        graph.shutdown().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn sqlite_process_exit_helper() -> Result<()> {
    let Ok(mode) = std::env::var("DRASI_NATIVE_SQLITE_CRASH") else {
        return Ok(());
    };
    let path = std::env::var("DRASI_NATIVE_SQLITE_PATH")?;
    let path = Path::new(&path);
    let progress = progress();
    let mut query = query(path, progress.clone()).await?;
    query.start().await?;
    let mut source = source(config(path), progress)?;
    source.start().await?;
    let handle = source.handle();
    handle
        .execute("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT)")
        .await?;
    if mode == "uncommitted" {
        handle
            .transaction(|tx| async move {
                tx.execute("INSERT INTO items VALUES(1,'uncommitted')")
                    .await?;
                std::process::exit(84);
                #[allow(unreachable_code)]
                Result::<()>::Ok(())
            })
            .await?;
    }
    anyhow::ensure!(
        mode == "committed" || mode == "consumed",
        "unknown crash boundary"
    );
    handle
        .execute_batch("INSERT INTO items VALUES(1,'first'); UPDATE items SET value='final'")
        .await?;
    if mode == "consumed" {
        apply(&mut query, next(&mut source).await?).await?;
    }
    std::process::exit(84);
}

#[tokio::test(flavor = "current_thread")]
async fn actual_process_exits_recover_sql_and_replay_at_each_commit_boundary() -> Result<()> {
    for mode in ["uncommitted", "committed", "consumed"] {
        let directory = tempfile::tempdir()?;
        let mut child = tokio::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "sqlite_process_exit_helper", "--nocapture"])
            .env("DRASI_NATIVE_SQLITE_CRASH", mode)
            .env("DRASI_NATIVE_SQLITE_PATH", directory.path())
            .kill_on_drop(true)
            .spawn()?;
        assert_eq!(
            timeout(Duration::from_secs(20), child.wait())
                .await??
                .code(),
            Some(84)
        );
        let progress = progress();
        let mut query = query(directory.path(), progress.clone()).await?;
        query.start().await?;
        let mut source = source(config(directory.path()), progress)?;
        source.start().await?;
        if mode == "committed" {
            apply(&mut query, next(&mut source).await?).await?;
        } else {
            assert!(timeout(Duration::from_millis(40), source.next())
                .await
                .is_err());
        }
        source.handle().query("SELECT id FROM items").await?;
        source.stop().await?;
        if mode == "uncommitted" {
            assert!(rows(&query).is_empty());
            assert_eq!(journal_state(directory.path())?, (0, 0, 0));
        } else {
            assert_eq!(rows(&query), [serde_json::json!({"id":1,"value":"final"})]);
            assert_eq!(journal_state(directory.path())?, (1, 1, 0));
        }
        query.stop().await?;
    }
    Ok(())
}
