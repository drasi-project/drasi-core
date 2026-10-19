// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use drasi_computation_plugin_abi as abi;
use drasi_computation_plugin_sdk::Scope;
use drasi_host_sdk::computation::NativeBootstrapProxy;
use drasi_source_sqlite::{
    native::{SqliteConfig, SqliteOutput, SqliteSource},
    TableKeyConfig,
};
use futures_util::{StreamExt, TryStreamExt};
use std::num::NonZeroU64;

const KIND: &str = "drasi.sqlite/file-bootstrap";
const INSTANCE: &str = "native-file-bootstrap";

fn plugin() -> Result<Arc<NativePlugin>> {
    let path = plugin_path(KIND);
    anyhow::ensure!(
        path.is_file(),
        "missing {}; run make build-native-sqlite-plugins",
        path.display()
    );
    computation::load(path)
}

fn configuration(path: &Path) -> Value {
    json!({
        "source": "source",
        "stream": "source/out",
        "settings": {
            "path": path,
            "tables": ["items"],
            "table_keys": [{"table":"items","key_columns":["code"]}],
            "max_tables": 16,
            "max_bytes": 65536,
            "max_duration_ms": 5000,
            "output_capacity": 1,
            "shutdown_timeout_ms": 5000
        }
    })
}

fn provider(plugin: &NativePlugin, path: &Path) -> Result<Arc<NativeBootstrapProxy>> {
    plugin.bootstrap_factories()[0].create_provider(
        id("query"),
        configuration(path),
        Some(Scope {
            instance_id: "sqlite-bootstrap".into(),
            graph_id: "graph".into(),
            generation: 1,
        }),
        None,
    )
}

#[test]
fn sqlite_bootstrap_only_catalogue_is_discovered_and_empty_base_without_extension_rejects(
) -> Result<()> {
    let native = plugin()?;
    assert!(
        native.factories().is_empty(),
        "bootstrap must not register a dummy component"
    );
    assert_eq!(native.bootstrap_factories().len(), 1);
    assert_eq!(
        native.bootstrap_factories()[0]
            .metadata()
            .implementation
            .name
            .as_ref(),
        KIND
    );
    assert!(!native.bootstrap_factories()[0].metadata().source_progress);
    let metadata =
        scan_plugin_metadata(&plugin_path(KIND)).context("bootstrap-only metadata discovery")?;
    assert_eq!(metadata.abi_family.as_deref(), Some("computation"));
    let mut registry = PluginRegistry::new();
    assert!(registry
        .register_computation_plugin(native.clone())?
        .is_empty());
    assert_eq!(registry.computation_bootstrap_metadata().len(), 1);
    assert_eq!(registry.computation_bootstrap_factories()?.len(), 1);

    // The successful load above pins these function pointers for process lifetime.
    let library = unsafe { libloading::Library::new(plugin_path(KIND))? };
    let metadata = unsafe { *library.get::<abi::MetadataFn>(abi::METADATA_SYMBOL)? };
    let entry = unsafe { *library.get::<abi::EntryFn>(abi::ENTRY_SYMBOL)? };
    let rejected = unsafe { NativePlugin::from_entry_points(metadata, entry) };
    assert!(
        rejected.is_err(),
        "an empty base alone is not an operational plugin"
    );
    let failure = rejected.err().context("missing bootstrap rejection")?;
    assert!(format!("{failure:#}").contains("no component or bootstrap factories"));
    assert!(
        native.bootstrap_factories()[0]
            .create_provider(
                id("query"),
                configuration(Path::new("not-opened.db")),
                None,
                None
            )
            .is_err(),
        "factory construction requires explicit query scope"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn sqlite_bootstrap_binary_preserves_errors_cancellation_and_stream_generations() -> Result<()>
{
    let native = plugin()?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("snapshot.db");
    let snapshot = provider(&native, &path)?;
    assert!(
        !path.exists(),
        "configuration-only construction must not open the file"
    );
    snapshot.prepare().await?;
    let mut failed = snapshot.snapshot().await?.changes;
    assert!(failed
        .next()
        .await
        .context("missing file failure")?
        .is_err());
    snapshot.stop().await?;
    assert!(
        !path.exists(),
        "read-only failure must not create an empty database"
    );
    rusqlite::Connection::open(&path)?.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY,code TEXT,value TEXT);
         INSERT INTO items VALUES(1,'one','before'),(2,'two','before'),(3,'three','before');",
    )?;
    snapshot.prepare().await?;
    let mut old = snapshot.snapshot().await?.changes;
    old.next().await.context("first old row")??;
    snapshot.stop().await?;
    snapshot.prepare().await?;
    let replacement = snapshot.snapshot().await?;
    assert!(replacement.watermarks.is_empty());
    assert!(old.next().await.context("stale stream failure")?.is_err());
    drop(old);
    let rows = tokio::time::timeout(TIMEOUT, replacement.changes.try_collect::<Vec<_>>()).await??;
    assert_eq!(rows.len(), 3);
    assert!(snapshot.complete_snapshot().await?.is_empty());
    assert!(snapshot.completion_state()?.is_none());
    snapshot.stop().await?;
    snapshot.prepare().await?;
    drop(snapshot.snapshot().await?);
    assert!(
        snapshot.prepare().await.is_err(),
        "dropped streams require awaited cleanup"
    );
    snapshot.stop().await?;
    Ok(())
}

async fn hosted_file_snapshot() -> Result<()> {
    let native = plugin()?;
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("source.db");
    rusqlite::Connection::open(&path)?.execute_batch(
        "CREATE TABLE items(id INTEGER PRIMARY KEY,code,value TEXT);
         INSERT INTO items VALUES(1,X'61','before'),(2,'YQ==','other');",
    )?;
    let core = drasi_lib::DrasiLib::builder()
        .with_id(INSTANCE)
        .build()
        .await?;
    let result = async {
        let positive = |value| NonZeroUsize::new(value).expect("positive test bound");
        let source = SqliteSource::new(id("source"), stream("source/out"), SqliteConfig {
            path: Some(path.clone()),
            tables: vec!["items".into()],
            table_keys: vec![TableKeyConfig { table: "items".into(), key_columns: vec!["code".into()] }],
            output: SqliteOutput::Changes,
            replay: None,
            transactions: SourceTransactionLimits {
                max_changes: positive(32),
                max_bytes: positive(1 << 20),
                max_duration_ms: NonZeroU64::new(5000).expect("deadline"),
            },
            max_sql_bytes: positive(65536),
            command_capacity: positive(4),
            output_capacity: positive(2),
            shutdown_timeout_ms: NonZeroU64::new(5000).expect("cleanup deadline"),
        })?;
        let sql = source.handle();
        let output = endpoint("source", "out");
        assert!(core.add_components(ComponentBatch::builder()
            .source(Box::new(source)).bind_stream(output.clone(), stream("source/out")).build()?
        ).await?.committed);
        let mut pipeline = core.computation_pipeline()?.native_source(output)?;
        for query in ["query", "observer"] {
            let snapshot = native.bootstrap_factories()[0].create_provider(
                id(query), configuration(&path),
                Some(Scope {
                    instance_id: INSTANCE.into(),
                    graph_id: core.computation_control()?.desired_snapshot().id.to_string(),
                    generation: 1,
                }),
                None,
            )?;
            pipeline = pipeline.query_bootstrap(query, snapshot)?
                .query(drasi_lib::Query::cypher(query)
                    .query("MATCH (n:items) RETURN n.id AS id,n.value AS value")
                    .from_source("source").enable_bootstrap(true).build());
        }
        assert!(core.add_components(pipeline.build()?).await?.committed);
        core.start().await?;
        let mut subscriptions = Vec::new();
        for query in ["query", "observer"] {
            tokio::time::timeout(TIMEOUT, core.computation_component(query)?.wait_started()).await??;
            let subscription = core.subscribe_query_results(query).await?;
            let mut initial = subscription.snapshot.to_vec();
            initial.sort_by_key(Value::to_string);
            assert_eq!(initial, vec![json!({"id":1,"value":"before"}), json!({"id":2,"value":"other"})]);
            subscriptions.push((query, subscription));
        }
        for (statement, value, remove_primary) in [
            ("UPDATE items SET code='new:key',value='after' WHERE id=1; DELETE FROM items WHERE id=2", "after", false),
            ("UPDATE items SET value='after-removal' WHERE id=1", "after-removal", true),
        ] {
            if remove_primary {
                core.remove_query("query").await?;
                subscriptions.retain(|(query, _)| *query != "query");
            }
            sql.execute_batch(statement).await?;
            for (query, subscription) in &mut subscriptions {
                tokio::time::timeout(TIMEOUT, async {
                    loop {
                        if core.get_query_results(query).await? == vec![json!({"id":1,"value":value})] {
                            return Ok::<_, anyhow::Error>(());
                        }
                        subscription.stream.next().await.context("query stream closed")??;
                    }
                }).await??;
            }
        }
        assert_eq!(core.computation_component("source")?.observed()?.lifecycle, ComponentLifecycle::Running);
        Ok::<_, anyhow::Error>(())
    }.await;
    let cleanup = core.shutdown().await;
    result?;
    cleanup?;
    Ok(())
}

#[test]
fn sqlite_bootstrap_binary_initializes_ordinary_queries_and_matches_native_live_rekeys(
) -> Result<()> {
    for threaded in [false, true] {
        let mut runtime = if threaded {
            tokio::runtime::Builder::new_multi_thread()
        } else {
            tokio::runtime::Builder::new_current_thread()
        };
        runtime
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(hosted_file_snapshot())?;
    }
    Ok(())
}
