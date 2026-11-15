// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Real SQL Server tests never skip ARM/emulation failures. Build the three
//! matching native libraries, then run with Podman available.
//!
//! Live qualification is deferred (CW-78). The stored-procedure sink is an
//! external consumer with a destination cursor; its live scenarios need the
//! fixture to pre-create `<schema>.DrasiDeliveryProgress` and grant the
//! application login SELECT, INSERT, UPDATE and DELETE on it.

#[path = "fixtures/native_mssql/coverage.rs"]
mod coverage;
#[path = "fixtures/native_mssql/credentials.rs"]
mod credentials;
#[path = "fixtures/native_mssql/database.rs"]
mod database;
#[path = "common/hooks.rs"]
mod hooks;
#[path = "fixtures/native_sink_inputs.rs"]
mod sink_inputs;

use anyhow::{ensure, Context, Result};
use async_trait::async_trait;
use database::Database;
use drasi_core::computation::InMemoryComputationProvider;
use drasi_host_sdk::computation::{self, NativePlugin};
use drasi_lib::{computation::v1::*, DrasiLib};
use serde_json::{json, Value};
use std::{collections::BTreeMap, num::NonZeroUsize, path::PathBuf, sync::Arc, time::Duration};

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("id")
}
fn resource(value: &str) -> ResourceId {
    ResourceId::try_new(value).expect("resource")
}
fn stream(value: &str) -> StreamId {
    StreamId::try_new(format!("{value}/out")).expect("stream")
}
fn endpoint(value: &str, port: &str) -> Endpoint {
    Endpoint::new(id(value), PortId::try_new(port).expect("port"))
}

fn plugins(dynamic: bool) -> Result<Vec<Arc<NativePlugin>>> {
    if !dynamic {
        return [
            drasi_computation_source_mssql::plugin()?,
            drasi_computation_bootstrap_mssql::plugin()?,
            drasi_computation_sink_storedproc_mssql::plugin()?,
        ]
        .into_iter()
        .map(NativePlugin::from_definition)
        .collect();
    }
    let directory = match std::env::var_os("DRASI_NATIVE_PLUGIN_DIR") {
        Some(path) => PathBuf::from(path),
        None => std::env::current_exe()?
            .parent()
            .context("test executable directory")?
            .parent()
            .context("native library directory")?
            .join("plugins"),
    };
    ["source_mssql", "bootstrap_mssql", "sink_storedproc_mssql"]
        .into_iter()
        .map(|name| {
            computation::load(directory.join(format!(
                "{}drasi_computation_{name}{}",
                std::env::consts::DLL_PREFIX,
                std::env::consts::DLL_SUFFIX
            )))
        })
        .collect()
}

/// Flat native sink settings for the fixture's application login, merged
/// with `extra` (outputs, templates, timeouts).
fn sink_settings(db: &Database, extra: Value) -> Value {
    let connection = db.settings()["connection"].clone();
    let mut settings = json!({
        "host": connection["host"], "port": connection["port"],
        "database": connection["database"], "user": connection["user"],
        "password": connection["password"], "encryption": "required",
        "trustServerCertificate": connection["trust_server_certificate"],
        "progressTable": format!("{}.DrasiDeliveryProgress", db.schema),
        "commandTimeoutMs": 15000, "connectTimeoutMs": 15000, "retryBudgetMs": 10000,
    });
    if let (Some(settings), Some(extra)) = (settings.as_object_mut(), extra.as_object()) {
        settings.extend(extra.clone());
    }
    settings
}

fn consumer_options() -> DeliveryOptions {
    DeliveryOptions {
        scope: RecoveryScope::MemoryLifetime,
        max_streams: NonZeroUsize::new(8).expect("positive stream limit"),
        receipts_per_stream: NonZeroUsize::new(8).expect("positive receipt limit"),
        retry: DeliveryRetryPolicy::default(),
    }
}

/// The sink through its actual consumer path: the host DeliveryRunner owns
/// input progress (in the returned directory, which must outlive the proxy),
/// the sink owns the destination cursor.
async fn create_sink(
    factory: &computation::NativeFactory,
    id: ComponentId,
    configuration: Value,
) -> Result<(computation::NativeConsumerProxy, tempfile::TempDir)> {
    let inputs = sink_inputs::inputs_for(&configuration);
    let state = tempfile::Builder::new()
        .prefix("native-mssql-consumer-")
        .tempdir()?;
    let provider = LegacyIndexProviderAdapter::new(Arc::new(
        drasi_index_rocksdb::RocksDbIndexProvider::new(state.path(), false, false),
    ));
    let consumer = factory
        .create_consumer_with_inputs(
            id,
            configuration,
            drasi_computation_plugin_sdk::Scope {
                instance_id: "native-mssql".into(),
                graph_id: ComponentBatch::GRAPH_ID.into(),
                generation: 1,
            },
            provider,
            consumer_options(),
            inputs,
        )
        .await?;
    Ok((consumer, state))
}

struct Settings(BTreeMap<String, Value>);
#[async_trait]
impl ConfigurationResolver for Settings {
    fn validate_reference(&self, key: &str) -> Result<()> {
        ensure!(self.0.contains_key(key), "unknown SQL Server test setting");
        Ok(())
    }
    async fn resolve(&self, key: &str) -> Result<Value> {
        self.validate_reference(key)?;
        Ok(self.0[key].clone())
    }
}
fn rows(result: &QueryResults) -> Result<Vec<Value>> {
    let mut rows = result
        .snapshot()?
        .rows
        .values()
        .map(|record| {
            Ok(QueryChangeCodec::row_values_to_json(
                &QueryChangeCodec::decode_row(record)?.values,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    rows.sort_by_key(|row| row["id"].as_i64());
    Ok(rows)
}
async fn wait_rows(result: &QueryResults, expected: Value) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(25), async {
        loop {
            if json!(rows(result)?) == expected {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("SQL Server rows deadline: actual {:?}", rows(result)))?
}

async fn exercise(dynamic: bool) -> Result<()> {
    let mut db = Database::new().await?;
    let plugins = plugins(dynamic)?;
    let owners = ["all", "high"]
        .into_iter()
        .map(|q| {
            Ok(Arc::new(QuerySourceProgress::new(
                ComponentBatch::GRAPH_ID,
                id(q),
            )?))
        })
        .collect::<Result<Vec<_>>>()?;
    let bootstrap_factory = &plugins[1].bootstrap_factories()[0];
    let mut queries = Vec::new();
    let mut results = Vec::new();
    for (index, name) in ["all", "high"].into_iter().enumerate() {
        let bootstrap = bootstrap_factory.create_provider(id(name),
            json!({"source":"items","stream":stream("items"),"settings":db.settings(),"labels":["Items"]}),
            Some(drasi_computation_plugin_sdk::Scope { instance_id:"native-mssql".into(),graph_id:ComponentBatch::GRAPH_ID.into(),generation:1 }),
            Some(owners[index].clone()))?;
        let query = ContinuousQueryTransformer::new_configured(
            ContinuousQueryDefinition {
                graph_id: ComponentBatch::GRAPH_ID.into(),
                id: id(name),
                query: format!(
                    "MATCH (n:Items) {} RETURN n.id AS id,n.value AS value",
                    if name == "high" {
                        "WHERE n.value >= 20"
                    } else {
                        ""
                    }
                ),
                language: ComputationQueryLanguage::Cypher,
                output_stream: stream(name),
                outbox_capacity: NonZeroUsize::new(32).expect("positive capacity"),
            },
            Arc::new(InMemoryComputationProvider),
            QueryOptions::default(),
            Default::default(),
            None,
        )
        .await?
        .with_source_progress(owners[index].clone())?
        .with_recovery_scope("native-mssql")?
        .with_bootstrap(bootstrap);
        results.push(query.results());
        queries.push(query);
    }
    let source_factory = plugins[0].factories()[0].clone();
    let mut source = source_factory
        .specification(id("items"), json!({"stream":stream("items"),"settings":{}}))?;
    source.configuration.insert(
        "settings".into(),
        ConfigurationValue::Reference {
            resource: resource("settings"),
            key: "source".into(),
            secret: true,
        },
    );
    source
        .dependencies
        .insert("consumer_progress".into(), vec![resource("consumers")]);
    let (sink, _sink_state) = create_sink(
        &plugins[2].factories()[0],
        id("effect"),
        sink_settings(&db, json!({
            "outputs":{"all":{"component":"all","port":"out"}},
            "defaultTemplate":{
                "added":{"template":db.scoped_sql("EXEC {{schema}}.ApplyEffect {{param operation}},{{param after.id}},{{param after.value}}")},
                "updated":{"template":db.scoped_sql("EXEC {{schema}}.ApplyEffect {{param operation}},{{param after.id}},{{param after.value}}")},
                "deleted":{"template":db.scoped_sql("EXEC {{schema}}.ApplyEffect {{param operation}},{{param before.id}},{{param before.value}}")}
            }
        })),
    )
    .await?;
    let mut builder = ComponentBatch::builder();
    for (name, role, handle) in [
        (
            "settings",
            ResourceRole::SecretStore,
            ResourceHandle::new(
                ResourceRole::SecretStore,
                Arc::new(ConfigurationResolverResource(Arc::new(Settings(
                    BTreeMap::from([("source".into(), db.settings())]),
                )))),
            ),
        ),
        (
            "consumers",
            ResourceRole::Checkpoint,
            ResourceHandle::new(
                ResourceRole::Checkpoint,
                Arc::new(ConsumerProgressSet::new(
                    ComponentBatch::GRAPH_ID,
                    id("items"),
                    owners.clone(),
                )?),
            ),
        ),
    ] {
        builder = builder
            .declare_resource(ResourceSpecification {
                id: resource(name),
                role,
                ownership: ResourceOwnership::Borrowed,
                binding: name.into(),
            })?
            .provide_resource(resource(name), handle)?;
    }
    builder = builder
        .component(
            drasi_lib::computation::v1::ComponentName::try_new("fixture-component-1")
                .expect("fixture component name"),
            source,
            source_factory,
        )
        .sink(
            drasi_lib::computation::v1::ComponentName::try_new("fixture-component-2")
                .expect("fixture component name"),
            Box::new(sink),
        )
        .bind_stream(endpoint("items", "out"), stream("items"));
    for query in queries {
        let name = query.descriptor().id().to_string();
        builder = builder
            .transformer(
                drasi_lib::computation::v1::ComponentName::try_new("fixture-transformer-1")
                    .expect("fixture component name"),
                Box::new(query),
            )
            .bind_stream(endpoint(&name, "out"), stream(&name))
            .connect(
                EdgeDefinition::new(endpoint("items", "out"), endpoint(&name, "in")),
                Box::new(BoundedPipeConfig { capacity: 1 }),
            );
    }
    builder = builder.connect(
        EdgeDefinition::new(endpoint("all", "out"), endpoint("effect", "in")),
        Box::new(BoundedPipeConfig { capacity: 1 }),
    );
    let core = DrasiLib::builder()
        .with_query_factory(
            drasi_lib::computation::v1::FactoryRegistry::standard()
                .get(
                    &drasi_lib::computation::v1::ImplementationIdentity::try_new(
                        "drasi/continuous-query",
                        "1",
                    )
                    .expect("reference query identity"),
                )
                .expect("reference query factory")
                .clone(),
        )
        .with_id("native-mssql")
        .with_components(builder.build()?)
        .build()
        .await?;
    let proof = async {
        core.start().await?;
        for name in ["all","high"] {
            let name = drasi_core_types::ComponentName::try_new(name)?;
            tokio::time::timeout(
                Duration::from_secs(25),
                core.component_by_name(&name)?.wait_started(),
            )
            .await??;
        }
        wait_rows(&results[0],json!([{"id":1,"value":10},{"id":2,"value":20}])).await?;
        wait_rows(&results[1],json!([{"id":2,"value":20}])).await?;
        let schema = tokio::time::timeout(Duration::from_secs(5),core.get_source_schema_by_name(&drasi_core_types::ComponentName::try_new("items").expect("fixture component name"))).await??
            .context("Host did not retain SQL Server schema capability at factory creation")?;
        ensure!(schema.nodes.iter().any(|node| node.label == "Items" &&
            node.properties.iter().any(|property| property.name == "value")),"public inspection did not return SQL Server catalog");
        db.sql("INSERT INTO {{schema}}.Items VALUES(3,30,N'live'); UPDATE {{schema}}.Items SET value=25 WHERE id=1;")?;
        wait_rows(&results[0],json!([{"id":1,"value":25},{"id":2,"value":20},{"id":3,"value":30}])).await?;
        wait_rows(&results[1],json!([{"id":1,"value":25},{"id":2,"value":20},{"id":3,"value":30}])).await?;
        db.sql("BEGIN TRANSACTION; UPDATE {{schema}}.Items SET value=999 WHERE id=1; ROLLBACK; UPDATE {{schema}}.Items SET id=4 WHERE id=1; DELETE FROM {{schema}}.Items WHERE id=2;")?;
        wait_rows(&results[0],json!([{"id":3,"value":30},{"id":4,"value":25}])).await?;
        wait_rows(&results[1],json!([{"id":3,"value":30},{"id":4,"value":25}])).await?;
        ensure!(owners.iter().all(|owner| owner.snapshot().checkpoints.contains_key(&SourceProgressKey::Source(drasi_core_types::ComponentId::try_new("00000000-0000-0000-0000-0000000001b3").expect("fixture UUID")))),"actual query checkpoints missing");
        core.stop().await?;
        ensure!(!db.sql("SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.Effects WHERE operation=N'UPDATE' AND id=1 AND value=25")?.starts_with('0'),"procedure update was not executed");
        db.sql("UPDATE {{schema}}.Items SET value=45 WHERE id=4;")?;
        core.start().await?;
        for name in ["all","high"] {
            let name = drasi_core_types::ComponentName::try_new(name)?;
            core.component_by_name(&name)?.wait_started().await?;
        }
        wait_rows(&results[0],json!([{"id":3,"value":30},{"id":4,"value":45}])).await?;
        wait_rows(&results[1],json!([{"id":3,"value":30},{"id":4,"value":45}])).await?;
        Ok::<_,anyhow::Error>(())
    }.await;
    let stopped = core.shutdown().await;
    let removed = db.remove();
    proof?;
    stopped?;
    removed?;
    Ok(())
}

#[test]
fn static_metadata_has_all_three_real_roles() -> Result<()> {
    let plugins = plugins(false)?;
    assert_eq!(plugins[0].factories().len(), 1);
    assert_eq!(plugins[1].bootstrap_factories().len(), 2);
    assert_eq!(plugins[2].factories().len(), 1);
    Ok(())
}
#[test]
fn dynamic_metadata_has_all_three_real_roles() -> Result<()> {
    let plugins = plugins(true)?;
    assert_eq!(plugins[0].factories().len(), 1);
    assert_eq!(plugins[1].bootstrap_factories().len(), 2);
    assert_eq!(plugins[2].factories().len(), 1);
    Ok(())
}
#[tokio::test]
#[ignore = "requires an explicitly configured isolated SQL Server fixture"]
async fn static_current_thread_real_sql_server() -> Result<()> {
    exercise(false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly configured isolated SQL Server fixture"]
async fn static_multi_thread_real_sql_server() -> Result<()> {
    exercise(false).await
}
#[tokio::test]
#[ignore = "requires an explicitly configured isolated SQL Server fixture"]
async fn dynamic_current_thread_real_sql_server() -> Result<()> {
    exercise(true).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an explicitly configured isolated SQL Server fixture"]
async fn dynamic_multi_thread_real_sql_server() -> Result<()> {
    exercise(true).await
}
