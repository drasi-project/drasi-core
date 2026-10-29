// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Static and dynamically loaded SQLite source/bootstrap qualification.

use anyhow::{Context, Result};
use drasi_computation_source_sqlite::{
    SqliteConfig, SqliteOutput, SqliteReplayConfig, TableKeyConfig,
};
use drasi_core::computation::{ComputationIndexProvider, InMemoryComputationProvider};
use drasi_host_sdk::computation::{self, NativePlugin};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::{computation::v1::*, DrasiLib};
use serde_json::{json, Value};
use std::{
    net::TcpListener,
    num::{NonZeroU64, NonZeroUsize},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const INSTANCE: &str = "native-sqlite-components";
const DEADLINE: Duration = Duration::from_secs(10);

#[path = "coordinated.rs"]
mod coordinated;

type Events = Arc<Mutex<Vec<ChangeEnvelope>>>;
struct Capture {
    descriptor: ComponentDescriptor,
    events: Events,
}
#[async_trait::async_trait]
impl ComputationComponent for Capture {
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.descriptor
    }
    async fn start(&mut self) -> Result<()> {
        Ok(())
    }
    async fn stop(&mut self) -> Result<()> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl EnvelopeSink for Capture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        let mut events = self
            .events
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite result capture poisoned"))?;
        anyhow::ensure!(events.len() < 64, "SQLite fixture result bound exceeded");
        events.push(input.envelope);
        Ok(())
    }
}

struct Hosted {
    core: DrasiLib,
    results: Vec<QueryResults>,
    handled: Vec<Events>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TransactionMode {
    Volatile,
    Replay,
    Shared,
}

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("test component")
}
fn endpoint(component: &str, port: &str) -> Endpoint {
    Endpoint::new(id(component), PortId::try_new(port).expect("test port"))
}
fn size(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("positive test bound")
}
fn millis(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("positive test deadline")
}
fn settings(path: &Path) -> SqliteConfig {
    SqliteConfig {
        path: Some(path.to_owned()),
        tables: vec!["items".into()],
        table_keys: Vec::new(),
        output: SqliteOutput::Changes,
        replay: None,
        transactions: SourceTransactionLimits {
            max_changes: size(64),
            max_bytes: size(1 << 20),
            max_duration_ms: millis(5000),
        },
        max_sql_bytes: size(65536),
        command_capacity: size(8),
        output_capacity: size(4),
        shutdown_timeout_ms: millis(5000),
    }
}

fn load(package: &str) -> Result<Arc<NativePlugin>> {
    let directory = match std::env::var_os("DRASI_NATIVE_PLUGIN_DIR") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?
            .parent()
            .context("test executable directory")?
            .parent()
            .context("native library directory")?
            .to_owned(),
    };
    let library = directory.join(format!(
        "{}{}{}",
        std::env::consts::DLL_PREFIX,
        package.replace('-', "_"),
        std::env::consts::DLL_SUFFIX,
    ));
    anyhow::ensure!(
        library.is_file(),
        "build {package} --features dynamic-plugin before running this test"
    );
    computation::load(library)
}

struct Plugins {
    source: Arc<NativePlugin>,
    bootstrap: Arc<NativePlugin>,
}
fn plugins(dynamic: bool) -> Result<Plugins> {
    let plugins = if dynamic {
        Plugins {
            source: load("drasi-computation-source-sqlite")?,
            bootstrap: load("drasi-computation-bootstrap-sqlite")?,
        }
    } else {
        Plugins {
            source: NativePlugin::from_definition(drasi_computation_source_sqlite::plugin()?)?,
            bootstrap: NativePlugin::from_definition(drasi_computation_bootstrap_sqlite::plugin()?)?,
        }
    };
    anyhow::ensure!(
        plugins.source.factories().len() == 4,
        "unexpected SQLite source catalogue"
    );
    anyhow::ensure!(
        plugins.bootstrap.factories().is_empty(),
        "bootstrap library must not invent a component"
    );
    Ok(plugins)
}

fn values(results: &QueryResults) -> Result<Vec<Value>> {
    let mut values = results
        .snapshot()?
        .rows
        .values()
        .map(|record| {
            Ok(QueryChangeCodec::row_values_to_json(
                &QueryChangeCodec::decode_row(record)?.values,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    values.sort_by_key(|row| row["id"].as_i64());
    Ok(values)
}

async fn wait_values(results: &[QueryResults], expected: &[Value]) -> Result<()> {
    tokio::time::timeout(DEADLINE, async {
        loop {
            let actual = results.iter().map(values).collect::<Result<Vec<_>>>()?;
            if actual.iter().all(|rows| rows == expected) {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .with_context(|| format!("SQLite queries did not reach {expected:?}"))?
}

async fn ordinary_graph(
    plugins: &Plugins,
    path: &Path,
    port: u16,
    keys: Vec<TableKeyConfig>,
) -> Result<Hosted> {
    let factory = plugins
        .source
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == drasi_computation_source_sqlite::SQLITE_CHANGES
        })
        .context("SQLite changes factory")?;
    let mut config = settings(path);
    config.table_keys = keys.clone();
    let source = factory.create_component(
        id("source"),
        json!({
            "stream":"sqlite/out", "settings":config,
            "rest_api":{"host":"127.0.0.1","port":port},
        }),
    )?;
    let mut builder = ComponentBatch::builder()
        .source(Box::new(source))
        .bind_stream(endpoint("source", "out"), StreamId::try_new("sqlite/out")?);
    let mut results = Vec::new();
    let mut handled = Vec::new();
    for name in ["first", "second"] {
        let bootstrap = plugins.bootstrap.bootstrap_factories().iter().find(|factory| {
            factory.metadata().implementation.name.as_ref() == "drasi.sqlite/file-bootstrap"
        }).context("SQLite file bootstrap")?.create_provider(
            id(name),
            json!({"source":"source","stream":"sqlite/out","settings":{
                "path":path,"tables":["items"],"table_keys":keys,"max_tables":8,"max_bytes":1048576,
                "max_duration_ms":5000,"output_capacity":2,"shutdown_timeout_ms":5000,
            }}),
            Some(drasi_computation_plugin_sdk::Scope {
                instance_id: INSTANCE.into(), graph_id: ComponentBatch::GRAPH_ID.into(), generation: 1,
            }),
            None,
        )?;
        let query = ContinuousQueryTransformer::new(
            ContinuousQueryDefinition {
                graph_id: ComponentBatch::GRAPH_ID.into(),
                id: id(name),
                query: "MATCH (n:items) RETURN n.id AS id,n.value AS value".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new(format!("{name}/out"))?,
                outbox_capacity: size(32),
            },
            Arc::new(InMemoryComputationProvider),
        )
        .await?
        .with_recovery_scope(INSTANCE)?
        .with_bootstrap(bootstrap);
        results.push(query.results());
        let sink = format!("{name}-sink");
        let events: Events = Arc::default();
        let capture = Capture {
            descriptor: ComponentDescriptor::try_new(
                id(&sink),
                vec![PortDescriptor::new(
                    PortId::try_new("in")?,
                    PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(),
                    PipeRequirements::default(),
                )],
            )?,
            events: events.clone(),
        };
        handled.push(events);
        builder = builder
            .query(Box::new(query))
            .sink(Box::new(capture))
            .bind_stream(
                endpoint(name, "out"),
                StreamId::try_new(format!("{name}/out"))?,
            )
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint(&sink, "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            );
    }
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_components(builder.build()?)
        .build()
        .await?;
    Ok(Hosted {
        core,
        results,
        handled,
    })
}

fn port() -> Result<u16> {
    Ok(TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

async fn memory_file_bootstrap_and_rest() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = plugins(dynamic)?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("source.db");
        rusqlite::Connection::open(&path)?.execute_batch(
            "CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT NOT NULL);
             INSERT INTO items VALUES(1,'snapshot');",
        )?;
        let port = port()?;
        let url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
        let Hosted {
            core,
            results,
            handled,
        } = ordinary_graph(&plugins, &path, port, Vec::new()).await?;
        let result = async {
            core.start().await?;
            for name in ["first", "second", "source", "first-sink", "second-sink"] {
                tokio::time::timeout(DEADLINE, core.computation_component(name)?.wait_started())
                    .await
                    .with_context(|| {
                        format!(
                            "SQLite {name} startup (dynamic={dynamic}): {:?}",
                            core.computation_component(name)
                                .map(|component| component.observed())
                        )
                    })??;
            }
            wait_values(&results, &[json!({"id":1,"value":"snapshot"})]).await?;
            let schema = core
                .get_source_schema("source")
                .await?
                .context("public native source schema")?;
            anyhow::ensure!(
                schema.nodes.len() == 1 && schema.nodes[0].label == "items",
                "DrasiLib inspection did not use actual SQLite data schema"
            );
            anyhow::ensure!(
                schema.nodes[0]
                    .properties
                    .iter()
                    .map(|property| property.name.as_str())
                    .collect::<Vec<_>>()
                    == ["id", "value"],
                "source configuration fields were exposed as data columns"
            );
            let merged = core.get_graph_schema().await?;
            anyhow::ensure!(
                merged
                    .nodes
                    .get("items")
                    .is_some_and(|node| node.sources.contains("source")),
                "native SQLite was omitted from public graph schema"
            );
            let tables: Value = client
                .get(format!("{url}/api/tables"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::ensure!(
                tables == json!(["items"]),
                "REST exposed unexpected tables: {tables}"
            );
            let response: Value = client
                .post(format!("{url}/api/tables/items"))
                .json(&json!({"id":2,"value":"insert"}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::ensure!(
                response == json!({"success":true}),
                "SQL insert acknowledgement differs"
            );
            wait_values(
                &results,
                &[
                    json!({"id":1,"value":"snapshot"}),
                    json!({"id":2,"value":"insert"}),
                ],
            )
            .await?;
            client
                .put(format!("{url}/api/tables/items/2"))
                .json(&json!({"id":20,"value":"rekey"}))
                .send()
                .await?
                .error_for_status()?;
            wait_values(
                &results,
                &[
                    json!({"id":1,"value":"snapshot"}),
                    json!({"id":20,"value":"rekey"}),
                ],
            )
            .await?;
            client
                .post(format!("{url}/api/batch"))
                .json(&json!({"operations":[
                    {"op":"update","table":"items","id":"1","data":{"value":"updated"}},
                    {"op":"insert","table":"items","data":{"id":3,"value":"temporary"}},
                    {"op":"delete","table":"items","id":"3"}
                ]}))
                .send()
                .await?
                .error_for_status()?;
            wait_values(
                &results,
                &[
                    json!({"id":1,"value":"updated"}),
                    json!({"id":20,"value":"rekey"}),
                ],
            )
            .await?;
            let failed = client
                .post(format!("{url}/api/batch"))
                .json(&json!({"operations":[
                    {"op":"insert","table":"items","data":{"id":4,"value":"must-rollback"}},
                    {"op":"insert","table":"items","data":{"id":20,"value":"duplicate"}}
                ]}))
                .send()
                .await?;
            anyhow::ensure!(
                failed.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                "failed SQL batch returned success"
            );
            let absent = client
                .get(format!("{url}/api/tables/items/4"))
                .send()
                .await?;
            anyhow::ensure!(
                absent.status() == reqwest::StatusCode::NOT_FOUND,
                "failed SQL batch left a committed prefix"
            );
            client
                .delete(format!("{url}/api/tables/items/1"))
                .send()
                .await?
                .error_for_status()?;
            wait_values(&results, &[json!({"id":20,"value":"rekey"})]).await?;
            tokio::time::timeout(DEADLINE, async {
                loop {
                    if handled
                        .iter()
                        .all(|events| !events.lock().expect("result log").is_empty())
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .context("query outputs were not handled")?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = core.shutdown().await;
        result?;
        cleanup?;
        drop(core);
        let Hosted {
            core: restarted,
            results,
            ..
        } = ordinary_graph(&plugins, &path, port, Vec::new()).await?;
        let result = async {
            restarted.start().await?;
            wait_values(&results, &[json!({"id":20,"value":"rekey"})]).await?;
            anyhow::ensure!(
                restarted.get_source_schema("source").await?.is_some(),
                "restarted source schema was not available"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = restarted.shutdown().await;
        result?;
        cleanup?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn file_bootstrap_rest_current_thread_static_and_dynamic() -> Result<()> {
    memory_file_bootstrap_and_rest().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_bootstrap_rest_multi_thread_static_and_dynamic() -> Result<()> {
    memory_file_bootstrap_and_rest().await
}

async fn native_identity_parity() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = plugins(dynamic)?;
        for custom in [false, true] {
            let directory = tempfile::tempdir()?;
            let path = directory.path().join("source.db");
            let (sql, keys, initial, route, change, changed, changed_route) = if custom {
                (
                    "CREATE TABLE items(storage INTEGER,id TEXT NOT NULL,value TEXT);
                     INSERT INTO items VALUES(99,'old','snapshot');",
                    vec![TableKeyConfig {
                        table: "items".into(),
                        key_columns: vec!["id".into()],
                    }],
                    json!({"id":"old","value":"snapshot"}),
                    "old",
                    json!({"id":"new","value":"changed"}),
                    json!({"id":"new","value":"changed"}),
                    "new",
                )
            } else {
                (
                    "CREATE TABLE items(rowid TEXT,id INTEGER,value TEXT);
                     INSERT INTO items VALUES('shadowed',1,'snapshot');",
                    Vec::new(),
                    json!({"id":1,"value":"snapshot"}),
                    "1",
                    json!({"_rowid_":11,"id":20,"value":"changed"}),
                    json!({"id":20,"value":"changed"}),
                    "11",
                )
            };
            rusqlite::Connection::open(&path)?.execute_batch(sql)?;
            let port = port()?;
            let graph = ordinary_graph(&plugins, &path, port, keys).await?;
            let result = async {
                graph.core.start().await?;
                tokio::time::timeout(
                    DEADLINE,
                    graph.core.computation_component("source")?.wait_started(),
                )
                .await??;
                wait_values(&graph.results, &[initial]).await?;
                let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
                let base = format!("http://127.0.0.1:{port}/api/tables/items");
                client
                    .put(format!("{base}/{route}"))
                    .json(&change)
                    .send()
                    .await?
                    .error_for_status()?;
                wait_values(&graph.results, &[changed]).await?;
                anyhow::ensure!(
                    client.get(format!("{base}/{route}")).send().await?.status()
                        == reqwest::StatusCode::NOT_FOUND,
                    "old SQLite identity survived a rekey"
                );
                client
                    .delete(format!("{base}/{changed_route}"))
                    .send()
                    .await?
                    .error_for_status()?;
                wait_values(&graph.results, &[]).await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let cleanup = graph.core.shutdown().await;
            result?;
            cleanup?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_identity_current_thread_static_and_dynamic() -> Result<()> {
    native_identity_parity().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_identity_multi_thread_static_and_dynamic() -> Result<()> {
    native_identity_parity().await
}

async fn transaction_graph(
    plugins: &Plugins,
    path: &Path,
    indexes: &Path,
    port: u16,
    mode: TransactionMode,
) -> Result<(Hosted, Vec<Arc<QuerySourceProgress>>)> {
    let names = if mode != TransactionMode::Replay {
        vec!["first", "second"]
    } else {
        vec!["first"]
    };
    let owners = names
        .iter()
        .map(|name| {
            Ok(Arc::new(QuerySourceProgress::new(
                ComponentBatch::GRAPH_ID,
                id(name),
            )?))
        })
        .collect::<Result<Vec<_>>>()?;
    let kind = match mode {
        TransactionMode::Shared => drasi_computation_source_sqlite::SQLITE_SHARED,
        TransactionMode::Replay => drasi_computation_source_sqlite::SQLITE_REPLAY,
        TransactionMode::Volatile => drasi_computation_source_sqlite::SQLITE_TRANSACTIONS,
    };
    let factory = plugins
        .source
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == kind)
        .cloned()
        .context("SQLite replay factory")?;
    let mut config = settings(path);
    config.output = SqliteOutput::Transactions;
    config.replay = (mode != TransactionMode::Volatile).then(|| SqliteReplayConfig {
        max_transactions: size(8),
        max_bytes: size(8 << 20),
    });
    let mut specification = factory.specification(
        id("source"),
        json!({
            "stream":"sqlite/out","settings":config,
            "rest_api":{"host":"127.0.0.1","port":port},
        }),
    )?;
    let resource = ResourceId::try_new("progress")?;
    let handle = if mode == TransactionMode::Shared {
        specification
            .dependencies
            .insert("consumer_progress".into(), vec![resource.clone()]);
        Some(ResourceHandle::new(
            ResourceRole::Checkpoint,
            Arc::new(ConsumerProgressSet::new(
                ComponentBatch::GRAPH_ID,
                id("source"),
                owners.clone(),
            )?),
        ))
    } else if mode == TransactionMode::Replay {
        specification
            .dependencies
            .insert("source_progress".into(), vec![resource.clone()]);
        Some(ResourceHandle::new(
            ResourceRole::Checkpoint,
            Arc::new(QuerySourceProgressResource(owners[0].clone())),
        ))
    } else {
        None
    };
    let mut builder = ComponentBatch::builder();
    if let Some(handle) = handle {
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource.clone(),
                role: ResourceRole::Checkpoint,
                ownership: ResourceOwnership::Borrowed,
                binding: "actual-query-progress".into(),
            })?
            .provide_resource(resource, handle)?;
    }
    builder = builder
        .component(specification, factory)
        .bind_stream(endpoint("source", "out"), StreamId::try_new("sqlite/out")?);
    let mut results = Vec::new();
    let mut handled = Vec::new();
    for (name, owner) in names.iter().zip(&owners) {
        let provider: Arc<dyn ComputationIndexProvider> =
            Arc::new(RocksDbComputationProvider::new(
                indexes.join(name),
                RocksIndexOptions::new(
                    false,
                    false,
                    RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
                ),
            ));
        let query = ContinuousQueryTransformer::new_configured(
            ContinuousQueryDefinition {
                graph_id: ComponentBatch::GRAPH_ID.into(),
                id: id(name),
                query: "MATCH (n:items) RETURN n.id AS id,n.value AS value".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: StreamId::try_new(format!("{name}/out"))?,
                outbox_capacity: size(32),
            },
            provider,
            QueryOptions::default(),
            QueryExecutionSettings {
                source_transactions: Some(config.transactions),
                ..Default::default()
            },
            None,
        )
        .await?
        .with_source_progress(owner.clone())?
        .with_recovery_scope(INSTANCE)?;
        results.push(query.results());
        let sink = format!("{name}-sink");
        let events: Events = Arc::default();
        handled.push(events.clone());
        builder = builder
            .query(Box::new(query))
            .sink(Box::new(Capture {
                descriptor: ComponentDescriptor::try_new(
                    id(&sink),
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                events,
            }))
            .bind_stream(
                endpoint(name, "out"),
                StreamId::try_new(format!("{name}/out"))?,
            )
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint(&sink, "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            );
    }
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_components(builder.build()?)
        .build()
        .await?;
    Ok((
        Hosted {
            core,
            results,
            handled,
        },
        owners,
    ))
}

async fn persistent_replay(shared: bool) -> Result<()> {
    for dynamic in [false, true] {
        let plugins = plugins(dynamic)?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("source.db");
        rusqlite::Connection::open(&path)?
            .execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT NOT NULL);")?;
        let port = port()?;
        let url = format!("http://127.0.0.1:{port}");
        let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
        let (graph, owners) = transaction_graph(
            &plugins,
            &path,
            &directory.path().join("indexes"),
            port,
            if shared {
                TransactionMode::Shared
            } else {
                TransactionMode::Replay
            },
        )
        .await?;
        let result = async {
            graph.core.start().await?;
            tokio::time::timeout(
                DEADLINE,
                graph.core.computation_component("source")?.wait_started(),
            )
            .await??;
            client
                .post(format!("{url}/api/batch"))
                .json(&json!({"operations":[
                    {"op":"insert","table":"items","data":{"id":1,"value":"intermediate"}},
                    {"op":"update","table":"items","id":"1","data":{"value":"final"}},
                    {"op":"insert","table":"items","data":{"id":2,"value":"temporary"}},
                    {"op":"delete","table":"items","id":"2"}
                ]}))
                .send()
                .await?
                .error_for_status()?;
            wait_values(&graph.results, &[json!({"id":1,"value":"final"})]).await?;
            tokio::time::timeout(DEADLINE, async {
                loop {
                    if graph
                        .handled
                        .iter()
                        .all(|events| events.lock().expect("result log").len() == 1)
                    {
                        return Ok::<_, anyhow::Error>(());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await??;
            for owner in &owners {
                let progress = owner.snapshot();
                anyhow::ensure!(
                    progress.persistent,
                    "native replay lost persistent ownership"
                );
                let position = progress
                    .checkpoints
                    .get(&SourceProgressKey::Source("source".into()))
                    .context("SQLite handled checkpoint")?;
                anyhow::ensure!(
                    position.sequence == 1 && position.source_position.is_some(),
                    "transaction progress was not committed once"
                );
            }
            let failed = client
                .post(format!("{url}/api/batch"))
                .json(&json!({"operations":[
                    {"op":"insert","table":"items","data":{"id":3,"value":"rollback"}},
                    {"op":"insert","table":"items","data":{"id":1,"value":"duplicate"}}
                ]}))
                .send()
                .await?;
            anyhow::ensure!(
                failed.status() == reqwest::StatusCode::INTERNAL_SERVER_ERROR,
                "failed transaction was acknowledged"
            );
            anyhow::ensure!(
                client
                    .get(format!("{url}/api/tables/items/3"))
                    .send()
                    .await?
                    .status()
                    == reqwest::StatusCode::NOT_FOUND,
                "failed whole transaction left a prefix"
            );
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = graph.core.shutdown().await;
        result?;
        cleanup.with_context(|| {
            format!("SQLite replay shutdown (shared={shared}, dynamic={dynamic})")
        })?;
        drop(graph);
        let (restarted, _) = transaction_graph(
            &plugins,
            &path,
            &directory.path().join("indexes"),
            port,
            if shared {
                TransactionMode::Shared
            } else {
                TransactionMode::Replay
            },
        )
        .await?;
        let result = async {
            restarted.core.start().await?;
            tokio::time::timeout(
                DEADLINE,
                restarted
                    .core
                    .computation_component("source")?
                    .wait_started(),
            )
            .await??;
            wait_values(&restarted.results, &[json!({"id":1,"value":"final"})]).await?;
            client
                .delete(format!("{url}/api/tables/items/1"))
                .send()
                .await?
                .error_for_status()?;
            wait_values(&restarted.results, &[]).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = restarted.core.shutdown().await;
        result?;
        cleanup?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn persistent_single_current_thread_static_and_dynamic() -> Result<()> {
    persistent_replay(false).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_single_multi_thread_static_and_dynamic() -> Result<()> {
    persistent_replay(false).await
}

#[tokio::test(flavor = "current_thread")]
async fn persistent_shared_current_thread_static_and_dynamic() -> Result<()> {
    persistent_replay(true).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_shared_multi_thread_static_and_dynamic() -> Result<()> {
    persistent_replay(true).await
}

async fn shared_stop_drains_accepted_sql() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = plugins(dynamic)?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("source.db");
        rusqlite::Connection::open(&path)?
            .execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT NOT NULL);")?;
        let port = port()?;
        let address = format!("127.0.0.1:{port}");
        let (graph, _) = transaction_graph(
            &plugins,
            &path,
            &directory.path().join("indexes"),
            port,
            TransactionMode::Shared,
        )
        .await?;
        let result = async {
            graph.core.start().await?;
            tokio::time::timeout(DEADLINE, graph.core.computation_component("source")?.wait_started()).await??;
            let mut request = tokio::net::TcpStream::connect(&address).await?;
            let body = r#"{"id":1,"value":"accepted-during-stop"}"#;
            request.write_all(format!(
                "POST /api/tables/items HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
                body.len(),
            ).as_bytes()).await?;
            let mut interim = [0; 25];
            tokio::time::timeout(DEADLINE, request.read_exact(&mut interim)).await??;
            anyhow::ensure!(&interim == b"HTTP/1.1 100 Continue\r\n\r\n", "request was not accepted");
            let control = graph.core.computation_control()?;
            let revision = control.desired_snapshot().revision;
            let complete_request = async {
                let mut observed = control.subscribe_observed();
                tokio::time::timeout(DEADLINE, observed.wait_for(|state| {
                    state.components.get(&id("source")).is_some_and(|source| {
                        source.lifecycle == ComponentLifecycle::Stopping
                    })
                })).await.context("native source never entered stop")??;
                tokio::task::yield_now().await;
                request.write_all(body.as_bytes()).await?;
                let mut response = Vec::new();
                tokio::time::timeout(DEADLINE, request.read_to_end(&mut response)).await??;
                let response = String::from_utf8(response)?;
                anyhow::ensure!(response.starts_with("HTTP/1.1 200 OK"),
                    "progress retirement broke accepted SQL: {response}");
                Ok::<_, anyhow::Error>(())
            };
            let (stop, request) = tokio::join!(
                control.stop_components(revision, GraphSelection::Exact(vec![id("source")])),
                complete_request,
            );
            request?;
            let stop = stop?;
            anyhow::ensure!(stop.summary == OperationSummary::Completed, "native source stop failed: {stop:?}");
            let start = control.start_components(revision, GraphSelection::Exact(vec![id("source")])).await?;
            anyhow::ensure!(start.summary == OperationSummary::Completed, "native source restart failed: {start:?}");
            wait_values(&graph.results, &[json!({"id":1,"value":"accepted-during-stop"})]).await?;
            Ok::<_, anyhow::Error>(())
        }.await;
        let cleanup = graph.core.shutdown().await;
        result?;
        cleanup?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn persistent_shared_stop_drains_sql_current_thread() -> Result<()> {
    shared_stop_drains_accepted_sql().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persistent_shared_stop_drains_sql_multi_thread() -> Result<()> {
    shared_stop_drains_accepted_sql().await
}

async fn volatile_transactions() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = plugins(dynamic)?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("source.db");
        rusqlite::Connection::open(&path)?
            .execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT NOT NULL);")?;
        let port = port()?;
        let (graph, owners) = transaction_graph(
            &plugins,
            &path,
            &directory.path().join("indexes"),
            port,
            TransactionMode::Volatile,
        )
        .await?;
        let result = async {
            graph.core.start().await?;
            tokio::time::timeout(
                DEADLINE,
                graph.core.computation_component("source")?.wait_started(),
            )
            .await??;
            let client = reqwest::Client::builder().timeout(DEADLINE).build()?;
            client
                .post(format!("http://127.0.0.1:{port}/api/batch"))
                .json(&json!({"operations":[
                    {"op":"insert","table":"items","data":{"id":1,"value":"prefix"}},
                    {"op":"update","table":"items","id":"1","data":{"value":"committed"}},
                    {"op":"insert","table":"items","data":{"id":2,"value":"temporary"}},
                    {"op":"delete","table":"items","id":"2"}
                ]}))
                .send()
                .await?
                .error_for_status()?;
            wait_values(&graph.results, &[json!({"id":1,"value":"committed"})]).await?;
            tokio::time::timeout(DEADLINE, async {
                loop {
                    if graph
                        .handled
                        .iter()
                        .all(|events| events.lock().expect("result log").len() == 1)
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .context("whole transaction output was not handled")?;
            for owner in &owners {
                anyhow::ensure!(
                    owner.snapshot().persistent,
                    "complete transaction queries require their actual atomic storage"
                );
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let cleanup = graph.core.shutdown().await;
        result?;
        cleanup?;
        let journals: i64 = rusqlite::Connection::open(&path)?.query_row(
            "SELECT COUNT(*) FROM sqlite_schema WHERE name LIKE '__drasi_%'",
            [],
            |row| row.get(0),
        )?;
        anyhow::ensure!(
            journals == 0,
            "volatile transaction source created a recovery journal"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn volatile_transactions_current_thread_static_and_dynamic() -> Result<()> {
    volatile_transactions().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn volatile_transactions_multi_thread_static_and_dynamic() -> Result<()> {
    volatile_transactions().await
}

async fn cancelled_rest_cleanup() -> Result<()> {
    for (dynamic, cancel) in [(false, false), (true, false), (false, true), (true, true)] {
        let plugins = plugins(dynamic)?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("source.db");
        rusqlite::Connection::open(&path)?
            .execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT NOT NULL);")?;
        let mut config = settings(&path);
        config.shutdown_timeout_ms = millis(80);
        let port = port()?;
        let address = format!("127.0.0.1:{port}");
        let factory = plugins
            .source
            .factories()
            .iter()
            .find(|factory| {
                factory.metadata().implementation.name.as_ref()
                    == drasi_computation_source_sqlite::SQLITE_CHANGES
            })
            .context("SQLite changes factory")?;
        let mut source = factory.create_component(
            id("source"),
            json!({
                "stream":"sqlite/out","settings":config,"rest_api":{"host":"127.0.0.1","port":port},
            }),
        )?;
        source.start().await?;
        let mut request = tokio::net::TcpStream::connect(&address).await?;
        let body = r#"{"id":1,"value":"accepted"}"#;
        request.write_all(format!(
            "POST /api/tables/items HTTP/1.1\r\nHost: {address}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nExpect: 100-continue\r\nConnection: close\r\n\r\n",
            body.len(),
        ).as_bytes()).await?;
        let mut interim = [0; 25];
        tokio::time::timeout(DEADLINE, request.read_exact(&mut interim)).await??;
        anyhow::ensure!(
            &interim == b"HTTP/1.1 100 Continue\r\n\r\n",
            "request was not accepted"
        );
        if cancel {
            anyhow::ensure!(
                tokio::time::timeout(Duration::from_millis(20), source.stop())
                    .await
                    .is_err(),
                "in-flight native stop completed without draining the request"
            );
        } else {
            let start = tokio::time::Instant::now();
            source
                .stop()
                .await
                .expect_err("unfinished request must retain cleanup");
            anyhow::ensure!(
                start.elapsed() >= Duration::from_millis(80),
                "cleanup returned before its deadline"
            );
        }
        anyhow::ensure!(
            source.start().await.is_err(),
            "failed/cancelled cleanup permitted replacement"
        );
        request.write_all(body.as_bytes()).await?;
        let mut response = Vec::new();
        tokio::time::timeout(DEADLINE, request.read_to_end(&mut response)).await??;
        let response = String::from_utf8(response)?;
        anyhow::ensure!(
            response.starts_with("HTTP/1.1 200 OK"),
            "accepted native REST request failed: {response}"
        );
        source.stop().await?;
        let released = tokio::net::TcpListener::bind(&address).await?;
        drop(released);
        source.start().await?;
        let rows: Value = reqwest::Client::builder()
            .timeout(DEADLINE)
            .build()?
            .get(format!("http://{address}/api/tables/items"))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        anyhow::ensure!(
            rows == json!([{"id":1,"value":"accepted"}]),
            "accepted SQL was lost after cleanup/restart"
        );
        source.stop().await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn rest_cleanup_current_thread_static_and_dynamic() -> Result<()> {
    cancelled_rest_cleanup().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rest_cleanup_multi_thread_static_and_dynamic() -> Result<()> {
    cancelled_rest_cleanup().await
}

async fn native_schema_reader_lifecycle() -> Result<()> {
    use drasi_computation_plugin_sdk::{NativeComponent, NativeSource};
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("source.db");
    rusqlite::Connection::open(&path)?
        .execute_batch("CREATE TABLE items(id INTEGER PRIMARY KEY,value TEXT NOT NULL);")?;
    let mut source = drasi_computation_source_sqlite::SqliteSource::new(
        id("source"),
        StreamId::try_new("sqlite/out")?,
        settings(&path),
    )?;
    let reader = source
        .source_schema_reader()
        .context("native schema reader")?;
    anyhow::ensure!(
        reader.describe_schema().await.is_err(),
        "schema read opened an unstarted source"
    );
    source.start().await?;
    anyhow::ensure!(
        Arc::ptr_eq(
            &reader,
            &source.source_schema_reader().context("schema reader")?
        ),
        "native getter replaced its captured reader"
    );
    let schema = {
        let next = source.next();
        tokio::pin!(next);
        tokio::time::timeout(DEADLINE, async {
            tokio::select! {
                result = &mut next => anyhow::bail!("empty source unexpectedly produced data: {result:?}"),
                schema = reader.describe_schema() => schema,
            }
        }).await??.context("actual schema")?
    };
    anyhow::ensure!(
        schema.nodes.len() == 1 && schema.nodes[0].label == "items",
        "native schema is not actual table metadata"
    );
    let sql = source.handle();
    let (entered, started) = tokio::sync::oneshot::channel();
    let (hold, held) = tokio::sync::oneshot::channel::<()>();
    let transaction = tokio::spawn(async move {
        sql.transaction(|transaction| async move {
            transaction
                .execute("INSERT INTO items VALUES(1,'must-rollback')")
                .await?;
            entered
                .send(())
                .map_err(|_| anyhow::anyhow!("schema test owner dropped"))?;
            held.await?;
            Ok(())
        })
        .await
    });
    started.await?;
    let pending = reader.describe_schema();
    tokio::pin!(pending);
    anyhow::ensure!(
        tokio::time::timeout(Duration::from_millis(20), &mut pending)
            .await
            .is_err(),
        "ordinary metadata entered an active transaction"
    );
    source.stop().await?;
    anyhow::ensure!(
        transaction.await?.is_err(),
        "source stop did not abort its active SQL scope"
    );
    drop(hold);
    anyhow::ensure!(
        reader.describe_schema().await.is_err(),
        "stopped schema reader stayed active"
    );
    source.start().await?;
    anyhow::ensure!(
        pending.await.is_err(),
        "old schema request silently moved to a replacement worker"
    );
    anyhow::ensure!(
        reader.describe_schema().await? == Some(schema),
        "fresh schema generation was not activated"
    );
    source.stop().await?;
    drop(source);
    anyhow::ensure!(
        reader.describe_schema().await.is_err(),
        "schema reader outlived source retirement"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn native_schema_reader_current_thread() -> Result<()> {
    native_schema_reader_lifecycle().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_schema_reader_multi_thread() -> Result<()> {
    native_schema_reader_lifecycle().await
}

#[test]
fn factory_modes_are_explicit_and_constructors_do_not_open_sqlite() -> Result<()> {
    for dynamic in [false, true] {
        let plugins = plugins(dynamic)?;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("not-opened.db");
        for factory in plugins.source.factories() {
            let name = factory.metadata().implementation.name.as_ref();
            let mut config = settings(&path);
            if name != drasi_computation_source_sqlite::SQLITE_CHANGES {
                config.output = SqliteOutput::Transactions;
            }
            if name == drasi_computation_source_sqlite::SQLITE_REPLAY
                || name == drasi_computation_source_sqlite::SQLITE_SHARED
            {
                config.replay = Some(SqliteReplayConfig {
                    max_transactions: size(4),
                    max_bytes: size(4 << 20),
                });
                anyhow::ensure!(
                    factory
                        .create_component(
                            id("source"),
                            json!({"stream":"sqlite/out","settings":config})
                        )
                        .is_err(),
                    "replay factory accepted absent progress"
                );
            } else {
                let component = factory.create_component(
                    id("source"),
                    json!({"stream":"sqlite/out","settings":config}),
                )?;
                drop(component);
                config.output = match config.output {
                    SqliteOutput::Changes => SqliteOutput::Transactions,
                    _ => SqliteOutput::Changes,
                };
                anyhow::ensure!(
                    factory
                        .create_component(
                            id("source"),
                            json!({"stream":"sqlite/out","settings":config})
                        )
                        .is_err(),
                    "factory silently changed output mode"
                );
            }
            anyhow::ensure!(!path.exists(), "constructor opened SQLite");
        }
    }
    Ok(())
}
