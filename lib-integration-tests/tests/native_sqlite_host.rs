// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use anyhow::{Context, Result};
use drasi_bootstrap_sqlite::native::{SqliteBootstrapConfig, SqliteBootstrapProvider};
use drasi_lib::{computation::v1::*, DrasiLib, Query};
use drasi_source_sqlite::{
    native::{SqliteConfig, SqliteOutput, SqliteSource},
    TableKeyConfig,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::{
    num::{NonZeroU64, NonZeroUsize},
    path::Path,
    sync::Arc,
    time::Duration,
};

fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("positive test bound")
}

fn millis(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("positive test duration")
}

fn settings(path: &Path, keys: Vec<TableKeyConfig>) -> SqliteConfig {
    SqliteConfig {
        path: Some(path.to_path_buf()),
        tables: vec!["items".into()],
        table_keys: keys,
        output: SqliteOutput::Changes,
        replay: None,
        transactions: SourceTransactionLimits {
            max_changes: size(32),
            max_bytes: size(1 << 20),
            max_duration_ms: millis(5000),
        },
        max_sql_bytes: size(65536),
        command_capacity: size(4),
        output_capacity: size(2),
        shutdown_timeout_ms: millis(5000),
    }
}

async fn snapshot_then_live(
    name: &str,
    seed: &str,
    keys: Vec<TableKeyConfig>,
    mutation: &str,
    initial: Vec<Value>,
    updated: Value,
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("source.db");
    rusqlite::Connection::open(&path)?.execute_batch(seed)?;
    let core = lib_integration_tests::query_builder()
        .with_allow_external_component_ids(true)
        .with_id(format!("sqlite-file-{name}"))
        .build()
        .await?;
    let result = async {
        let source_id = ComponentId::try_new("112d9eea-2f2c-5bdd-b4cb-39779bace319")?;
        let stream = StreamId::try_new("source/out")?;
        let config = settings(&path, keys);
        let source = SqliteSource::new(source_id.clone(), stream.clone(), config.clone())?;
        let input = source.handle();
        let output = Endpoint::new(source_id.clone(), PortId::try_new("out")?);
        core.add_components(
            ComponentBatch::builder()
                .source(
                    drasi_lib::computation::v1::ComponentName::try_new("fixture-source-1")
                        .expect("fixture component name"),
                    Box::new(source),
                )
                .bind_stream(output.clone(), stream.clone())
                .build()?,
        )
        .await?;
        let mut pipeline = core.computation_pipeline()?.native_source(output)?;
        for query in ["primary", "observer"] {
            let query_name = ComponentName::try_new(query)?;
            let bootstrap = Arc::new(SqliteBootstrapProvider::new(
                source_id.clone(),
                stream.clone(),
                SqliteBootstrapConfig {
                    path: path.clone(),
                    tables: config.tables.clone(),
                    table_keys: config.table_keys.clone(),
                    max_tables: size(8),
                    max_bytes: size(1 << 16),
                    max_duration_ms: millis(5000),
                    output_capacity: size(1),
                    shutdown_timeout_ms: millis(5000),
                },
            )?);
            let query_id = ComponentId::derive_owned(&source_id, &format!("query/{query}"))?;
            let mut query_config = Query::cypher(query_name)
                .query("MATCH (n:items) RETURN n.id AS id,n.value AS value")
                .from_input_by_name(
                    drasi_core::ComponentName::try_new("source").expect("fixture component name"),
                )
                .enable_bootstrap(true)
                .build()?;
            query_config.inputs[0].component =
                drasi_lib::config::ComponentReference::by_id(source_id.clone());
            pipeline = pipeline
                .query_bootstrap(&query_id, bootstrap)?
                .query(query_config.with_assigned_id(query_id));
        }
        core.add_components(pipeline.build()?).await?;
        core.start().await?;
        let mut subscriptions = Vec::new();
        for query in ["primary", "observer"] {
            let query_name = ComponentName::try_new(query)?;
            let query_id = core.component_by_name(&query_name)?.id().clone();
            tokio::time::timeout(
                Duration::from_secs(5),
                core.component_by_id(&query_id)?.wait_started(),
            )
            .await??;
            let subscription = core.subscribe_query_results_by_name(&query_name).await?;
            assert_eq!(
                subscription.snapshot.to_vec(),
                initial,
                "{name}/{query} bootstrap"
            );
            subscriptions.push((query_name, subscription));
        }
        for (sql, expected) in [(mutation, vec![updated]), ("DELETE FROM items", Vec::new())] {
            input.execute(sql).await?;
            for (query_name, subscription) in &mut subscriptions {
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        let actual = core.get_query_results_by_name(query_name).await?;
                        if actual == expected {
                            return Ok::<_, anyhow::Error>(());
                        }
                        subscription
                            .stream
                            .next()
                            .await
                            .context("query reader closed")??;
                    }
                })
                .await
                .with_context(|| format!("{name}/{query_name} did not reach {expected:?}"))??;
            }
        }
        let source = core.component_by_name(
            &drasi_core::ComponentName::try_new("fixture-source-1")
                .expect("fixture component name"),
        )?;
        let source_generation = source.observed()?.generation;
        core.remove_query_by_name(
            &drasi_core::ComponentName::try_new("observer").expect("fixture component name"),
        )
        .await?;
        tokio::time::timeout(Duration::from_secs(5), source.wait_started()).await??;
        assert_eq!(source.observed()?.generation, source_generation);
        assert_eq!(
            core.component_by_name(
                &drasi_core::ComponentName::try_new("fixture-source-1")
                    .expect("fixture component name")
            )?
            .observed()?
            .lifecycle,
            ComponentLifecycle::Running
        );
        assert!(core
            .component_by_name(
                &drasi_core::ComponentName::try_new("primary").expect("fixture component name")
            )
            .is_ok());
        let journals: u64 = rusqlite::Connection::open_with_flags(
            &path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE lower(substr(name,1,8))='__drasi_'",
            [],
            |row| row.get(0),
        )?;
        assert_eq!(
            journals, 0,
            "ordinary bootstrap must not invent a journal or durable source boundary"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    let cleanup = core.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}

#[test]
fn native_file_bootstrap_and_live_sql_share_identity_in_ordinary_hosted_queries() -> Result<()> {
    for threaded in [false, true] {
        let mut runtime = if threaded {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        runtime.worker_threads(2).enable_all().build()?.block_on(async {
            for (name, schema, key, mutation, updated_id, empty) in [
                ("primary", "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT);INSERT INTO items VALUES(1,'before')",
                    None, "UPDATE items SET id=2,value='after'", 2, false),
                ("composite", "CREATE TABLE items(id INTEGER,a BLOB,b TEXT,value TEXT,PRIMARY KEY(a,b)) WITHOUT ROWID;INSERT INTO items VALUES(1,X'61','a:b|c','before')",
                    None, "UPDATE items SET a=X'62',value='after'", 1, false),
                ("custom", "CREATE TABLE items(id INTEGER PRIMARY KEY,code TEXT,value TEXT);INSERT INTO items VALUES(1,'original:key','before')",
                    Some("code"), "UPDATE items SET code='changed:key',value='after'", 1, false),
                ("rowid", "CREATE TABLE items(id INTEGER,rowid TEXT,_rowid_ TEXT,value TEXT);INSERT INTO items(oid,id,rowid,_rowid_,value) VALUES(7,1,'user-rowid','user-alias','before')",
                    None, "UPDATE items SET oid=9,value='after'", 1, false),
                ("shadowed", "CREATE TABLE items(id INTEGER,rowid TEXT,_rowid_ TEXT,oid TEXT,value TEXT);INSERT INTO items VALUES(1,'rowid','_rowid_','oid','before')",
                    Some("id"), "UPDATE items SET id=2,value='after'", 2, false),
                ("empty", "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT)",
                    None, "INSERT INTO items VALUES(1,'after')", 1, true),
            ] {
                let keys = key.map(|key| TableKeyConfig { table: "items".into(), key_columns: vec![key.into()] })
                    .into_iter().collect();
                let initial = if empty { Vec::new() } else { vec![json!({"id":1,"value":"before"})] };
                snapshot_then_live(name, schema, keys, mutation, initial, json!({"id":updated_id,"value":"after"})).await?;
            }
            Ok::<_, anyhow::Error>(())
        })?;
    }
    Ok(())
}
