// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Real SQL Server tests never skip ARM/emulation failures. Build the three
//! matching native libraries, then run with Podman available.

#[path = "fixtures/native_mssql/coverage.rs"]
mod coverage;
#[path = "fixtures/native_mssql/credentials.rs"]
mod credentials;
#[path = "fixtures/native_mssql/database.rs"]
mod database;
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
    let directory = std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR").map(|p| PathBuf::from(p).join("debug")))
        .context("DRASI_NATIVE_PLUGIN_DIR must point to freshly built SQL Server libraries")?;
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
    let sink_settings = json!({"connection":db.settings()["connection"],"outputs":{"all":{"component":"all","port":"out"}},
    "default_template":{
        "added":{"template":db.scoped_sql("EXEC {{schema}}.ApplyEffect {{param operation}},{{param after.id}},{{param after.value}}")},
        "updated":{"template":db.scoped_sql("EXEC {{schema}}.ApplyEffect {{param operation}},{{param after.id}},{{param after.value}}")},
        "deleted":{"template":db.scoped_sql("EXEC {{schema}}.ApplyEffect {{param operation}},{{param before.id}},{{param before.value}}")}
    }});
    let sink_factory = plugins[2].factories()[0].clone();
    let mut sink = sink_factory.specification(id("effect"), json!({"settings":{}}))?;
    sink.configuration.insert(
        "settings".into(),
        ConfigurationValue::Reference {
            resource: resource("settings"),
            key: "sink".into(),
            secret: true,
        },
    );
    let mut builder = ComponentBatch::builder();
    for (name, role, handle) in [
        (
            "settings",
            ResourceRole::SecretStore,
            ResourceHandle::new(
                ResourceRole::SecretStore,
                Arc::new(ConfigurationResolverResource(Arc::new(Settings(
                    BTreeMap::from([
                        ("source".into(), db.settings()),
                        ("sink".into(), sink_settings),
                    ]),
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
        .component(source, source_factory)
        .component(sink, sink_factory)
        .bind_stream(endpoint("items", "out"), stream("items"));
    for query in queries {
        let name = query.descriptor().id().to_string();
        builder = builder
            .query(Box::new(query))
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
        .with_id("native-mssql")
        .with_components(builder.build()?)
        .build()
        .await?;
    let proof = async {
        core.start().await?;
        for name in ["all","high"] { tokio::time::timeout(Duration::from_secs(25),core.computation_component(name)?.wait_started()).await??; }
        wait_rows(&results[0],json!([{"id":1,"value":10},{"id":2,"value":20}])).await?;
        wait_rows(&results[1],json!([{"id":2,"value":20}])).await?;
        let schema = tokio::time::timeout(Duration::from_secs(5),core.get_source_schema("items")).await??
            .context("Host did not retain SQL Server schema capability at factory creation")?;
        ensure!(schema.nodes.iter().any(|node| node.label == "Items" &&
            node.properties.iter().any(|property| property.name == "value")),"public inspection did not return SQL Server catalog");
        db.sql("INSERT INTO {{schema}}.Items VALUES(3,30,N'live'); UPDATE {{schema}}.Items SET value=25 WHERE id=1;")?;
        wait_rows(&results[0],json!([{"id":1,"value":25},{"id":2,"value":20},{"id":3,"value":30}])).await?;
        wait_rows(&results[1],json!([{"id":1,"value":25},{"id":2,"value":20},{"id":3,"value":30}])).await?;
        db.sql("BEGIN TRANSACTION; UPDATE {{schema}}.Items SET value=999 WHERE id=1; ROLLBACK; UPDATE {{schema}}.Items SET id=4 WHERE id=1; DELETE FROM {{schema}}.Items WHERE id=2;")?;
        wait_rows(&results[0],json!([{"id":3,"value":30},{"id":4,"value":25}])).await?;
        wait_rows(&results[1],json!([{"id":3,"value":30},{"id":4,"value":25}])).await?;
        ensure!(owners.iter().all(|owner| owner.snapshot().checkpoints.contains_key(&SourceProgressKey::Source("items".into()))),"actual query checkpoints missing");
        core.stop().await?;
        ensure!(!db.sql("SET NOCOUNT ON; SELECT COUNT(*) FROM {{schema}}.Effects WHERE operation=N'UPDATE' AND id=1 AND value=25")?.starts_with('0'),"procedure update was not executed");
        db.sql("UPDATE {{schema}}.Items SET value=45 WHERE id=4;")?;
        core.start().await?;
        for name in ["all","high"] { core.computation_component(name)?.wait_started().await?; }
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
async fn static_current_thread_real_sql_server() -> Result<()> {
    exercise(false).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_multi_thread_real_sql_server() -> Result<()> {
    exercise(false).await
}
#[tokio::test]
async fn dynamic_current_thread_real_sql_server() -> Result<()> {
    exercise(true).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dynamic_multi_thread_real_sql_server() -> Result<()> {
    exercise(true).await
}
