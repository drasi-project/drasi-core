// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Run with `--ignored` after building the four matching native PG/HTTP libraries.
//! Podman creates a fresh database; no installed/demo database or slot is used.

use anyhow::{Context, Result};
use async_trait::async_trait;
use axum::{routing::get, Json, Router};
use drasi_core::computation::InMemoryComputationProvider;
use drasi_host_sdk::computation::{self, NativePlugin};
use drasi_lib::{computation::v1::*, DrasiLib};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::Write,
    net::TcpListener as StdListener,
    num::NonZeroUsize,
    path::PathBuf,
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{oneshot, Notify, Semaphore},
};

const INSTANCE: &str = "postgres-http-composition";
const TIMEOUT: Duration = Duration::from_secs(30);

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("test ID")
}
fn resource(value: &str) -> ResourceId {
    ResourceId::try_new(value).expect("test resource")
}
fn stream(value: &str) -> StreamId {
    StreamId::try_new(format!("{value}/out")).expect("test stream")
}
fn endpoint(value: &str, port: &str) -> Endpoint {
    Endpoint::new(id(value), PortId::try_new(port).expect("test port"))
}
fn rows(results: &QueryResults) -> Result<Vec<Value>> {
    results
        .snapshot()?
        .rows
        .values()
        .map(|record| {
            let row = QueryChangeCodec::decode_row(record)?;
            Ok(QueryChangeCodec::row_values_to_json(&row.values))
        })
        .collect()
}

struct Database {
    container: String,
    port: u16,
}
impl Database {
    fn new() -> Result<Self> {
        let output = Command::new("podman")
            .args([
                "run",
                "--detach",
                "--publish",
                "127.0.0.1::5432",
                "--tmpfs",
                "/var/lib/postgresql/data:rw",
                "--env",
                "POSTGRES_PASSWORD=isolated-composition-test",
                "--label",
                "drasi.qualification=postgres-http-composition",
                "postgres:16-alpine",
                "-c",
                "wal_level=logical",
                "-c",
                "max_replication_slots=16",
                "-c",
                "max_wal_senders=16",
            ])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "Podman startup: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let container = String::from_utf8(output.stdout)?.trim().to_owned();
        anyhow::ensure!(
            container.len() == 64 && container.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid owned container ID"
        );
        let mut database = Self { container, port: 0 };
        let output = Command::new("podman")
            .args(["port", &database.container, "5432/tcp"])
            .output()?;
        anyhow::ensure!(output.status.success(), "Podman port lookup failed");
        let address = String::from_utf8(output.stdout)?;
        database.port = address
            .trim()
            .strip_prefix("127.0.0.1:")
            .context("loopback port")?
            .parse()?;
        Ok(database)
    }
    fn sql(&self, sql: &str) -> Result<String> {
        let mut child = Command::new("podman")
            .args([
                "exec",
                "-i",
                &self.container,
                "psql",
                "-X",
                "-qAt",
                "-v",
                "ON_ERROR_STOP=1",
                "-U",
                "postgres",
                "-h",
                "127.0.0.1",
                "-d",
                "postgres",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .context("SQL input")?
            .write_all(sql.as_bytes())?;
        let output = child.wait_with_output()?;
        anyhow::ensure!(
            output.status.success(),
            "isolated SQL: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(String::from_utf8(output.stdout)?.trim().to_owned())
    }
    fn settings(&self) -> Value {
        json!({
            "connection":{"host":"127.0.0.1","port":self.port,"database":"postgres",
                "user":"postgres","password":"isolated-composition-test","ssl_mode":"disable",
                "tables":["stocks"],"slot_name":"composition","publication_name":"composition",
                "table_keys":[{"table":"stocks","key_columns":["id"]}]},
            "start_lsn":"0/0","transactions":{"max_changes":64,"max_bytes":1048576,"max_duration_ms":10000},
            "max_protocol_bytes":1048576,"io_timeout_ms":10000,"feedback_interval_ms":100,
        })
    }
    fn remove(&mut self) -> Result<()> {
        if self.container.is_empty() {
            return Ok(());
        }
        let output = Command::new("podman")
            .args(["rm", "--force", &self.container])
            .output()?;
        anyhow::ensure!(
            output.status.success(),
            "owned database cleanup: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.container.clear();
        Ok(())
    }
}
impl Drop for Database {
    fn drop(&mut self) {
        if let Err(error) = self.remove() {
            eprintln!("owned PostgreSQL cleanup failed: {error:#}");
        }
    }
}

struct Settings(Value);
#[async_trait]
impl ConfigurationResolver for Settings {
    fn validate_reference(&self, key: &str) -> Result<()> {
        anyhow::ensure!(key == "postgres", "unknown test settings");
        Ok(())
    }
    async fn resolve(&self, key: &str) -> Result<Value> {
        self.validate_reference(key)?;
        Ok(self.0.clone())
    }
}

struct Output {
    descriptor: ComponentDescriptor,
}
#[async_trait]
impl ComputationComponent for Output {
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
#[async_trait]
impl EnvelopeSink for Output {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, _input: InputEnvelope) -> Result<()> {
        Ok(())
    }
}

fn plugins(dynamic: bool) -> Result<Vec<Arc<NativePlugin>>> {
    if !dynamic {
        return [
            drasi_computation_source_postgres::plugin()?,
            drasi_computation_bootstrap_postgres::plugin()?,
            drasi_computation_source_http::plugin()?,
            drasi_computation_bootstrap_http::plugin()?,
        ]
        .into_iter()
        .map(NativePlugin::from_definition)
        .collect();
    }
    let directory = std::env::var_os("DRASI_NATIVE_PLUGIN_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CARGO_TARGET_DIR").map(|path| PathBuf::from(path).join("debug"))
        })
        .context("set DRASI_NATIVE_PLUGIN_DIR or CARGO_TARGET_DIR for rebuilt libraries")?;
    [
        "source_postgres",
        "bootstrap_postgres",
        "source_http",
        "bootstrap_http",
    ]
    .into_iter()
    .map(|name| {
        computation::load(directory.join(format!(
            "{}drasi_computation_{name}{}",
            std::env::consts::DLL_PREFIX,
            std::env::consts::DLL_SUFFIX,
        )))
    })
    .collect()
}

async fn exercise(dynamic: bool) -> Result<()> {
    let mut database = Database::new()?;
    tokio::time::timeout(TIMEOUT, async {
        while !Command::new("podman")
            .args([
                "exec",
                &database.container,
                "pg_isready",
                "-h",
                "127.0.0.1",
                "-U",
                "postgres",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
            .success()
        {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        Result::<()>::Ok(())
    })
    .await??;
    database.sql(
        "CREATE TABLE stocks(id INTEGER PRIMARY KEY,symbol TEXT,name TEXT);
        INSERT INTO stocks VALUES(1,'AAPL','before'); ALTER TABLE stocks REPLICA IDENTITY FULL;
        CREATE PUBLICATION composition FOR TABLE stocks;
        SELECT pg_create_logical_replication_slot('composition','pgoutput');",
    )?;
    let feedback = database.sql(
        "SELECT confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name='composition'",
    )?;
    let native = plugins(dynamic)?;
    let owner = Arc::new(QuerySourceProgress::new(
        ComponentBatch::GRAPH_ID,
        id("joined"),
    )?);
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let entered = Arc::new(Notify::new());
    let gate = Arc::new(Semaphore::new(0));
    let snapshots = Arc::new(AtomicUsize::new(0));
    let (shutdown, stopped) = oneshot::channel();
    let route = {
        let entered = entered.clone();
        let gate = gate.clone();
        let snapshots = snapshots.clone();
        get(move || {
            let entered = entered.clone();
            let gate = gate.clone();
            let snapshots = snapshots.clone();
            async move {
                snapshots.fetch_add(1, Ordering::SeqCst);
                entered.notify_one();
                let _permit = gate.acquire().await.expect("test gate");
                Json(json!({"items":[{"id":"price_AAPL","symbol":"AAPL","price":175.5}]}))
            }
        })
    };
    let http = tokio::spawn(async move {
        axum::serve(listener, Router::new().route("/prices", route))
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
    });
    let result = async {
        let scope = drasi_computation_plugin_sdk::Scope {
            instance_id: INSTANCE.into(), graph_id: ComponentBatch::GRAPH_ID.into(), generation: 1,
        };
        let pg_factory = native[1].bootstrap_factories().iter()
            .find(|factory| factory.metadata().implementation.name.as_ref() == "drasi.postgres/shared-row-bootstrap")
            .context("configured PG bootstrap")?;
        let pg = pg_factory.create_provider(id("joined"),
            json!({"source":"stocks","stream":stream("stocks"),"settings":database.settings()}),
            Some(scope.clone()), Some(owner.clone()))?;
        let prices = native[3].bootstrap_factories().iter()
            .find(|factory| factory.metadata().implementation.name.as_ref() == "drasi.network/http-bootstrap")
            .context("HTTP bootstrap")?.create_provider(id("joined"), json!({
                "source":"price-feed","stream":stream("price-feed"),"outputCapacity":1,
                "settings":{"endpoints":[{"url":format!("http://{address}/prices"),
                    "response":{"itemsPath":"$.items","mappings":[{"elementType":"node","template":{
                        "id":"{{item.id}}","labels":["stock_prices"],
                        "properties":{"symbol":"{{item.symbol}}","price":"{{item.price}}"}}}]}}],
                    "maxRetries":0,"timeoutSeconds":20}
            }), Some(scope), None)?;
        let composite = Arc::new(CompositeBootstrapProvider::new(INSTANCE, ComponentBatch::GRAPH_ID, id("joined"),
            vec![(resource("pg"),pg.clone()),(resource("http"),prices.clone())])?);
        let query = ContinuousQueryTransformer::new_configured(
            ContinuousQueryDefinition {
                graph_id: ComponentBatch::GRAPH_ID.into(), id:id("joined"),
                query:"MATCH (s:stocks)-[:HAS_PRICE]->(p:stock_prices) RETURN s.name AS name,p.price AS price".into(),
                language:ComputationQueryLanguage::Cypher, output_stream:stream("joined"),
                outbox_capacity:NonZeroUsize::new(32).expect("capacity"),
            }, Arc::new(InMemoryComputationProvider), QueryOptions::default(),
            serde_json::from_value(json!({"joins":[{"id":"HAS_PRICE","keys":[
                {"label":"stocks","property":"symbol"},{"label":"stock_prices","property":"symbol"}]}]}))?,
            None,
        ).await?.with_source_progress(owner.clone())?.with_recovery_scope(INSTANCE)?
            .with_bootstrap(composite.clone());
        let results = query.results();
        let pg_source = native[0].factories().iter()
            .find(|factory| factory.metadata().implementation.name.as_ref() == "drasi.postgres/shared-changes")
            .context("shared PG source")?.clone();
        let mut specification = pg_source.specification(id("stocks"), json!({
            "stream":stream("stocks"),"settings":{},"persistent":false,
            "auto_create_slot":false,"start_from_slot":false,
        }))?;
        specification.configuration.insert("settings".into(), ConfigurationValue::Reference {
            resource:resource("settings"), key:"postgres".into(), secret:true,
        });
        specification.dependencies.insert("consumer_progress".into(), vec![resource("consumers")]);
        let price_port = StdListener::bind("127.0.0.1:0")?.local_addr()?.port();
        let price_factory = native[2].factories().iter()
            .find(|factory| factory.metadata().implementation.name.as_ref() == "drasi.network/http-source")
            .context("native price source")?.clone();
        let mut builder = ComponentBatch::builder();
        for (name, role, handle) in [
            ("settings",ResourceRole::SecretStore,ResourceHandle::new(ResourceRole::SecretStore,
                Arc::new(ConfigurationResolverResource(Arc::new(Settings(database.settings())))))),
            ("consumers",ResourceRole::Checkpoint,ResourceHandle::new(ResourceRole::Checkpoint,
                Arc::new(ConsumerProgressSet::new(ComponentBatch::GRAPH_ID,id("stocks"),vec![owner.clone()])?))),
        ] {
            builder = builder.declare_resource(ResourceSpecification {
                id:resource(name),role,ownership:ResourceOwnership::Borrowed,binding:name.into(),
            })?.provide_resource(resource(name),handle)?;
        }
        let core = DrasiLib::builder().with_id(INSTANCE).with_components(builder
            .component(specification,pg_source)
            .component(price_factory.specification(id("price-feed"),json!({
                "stream":stream("price-feed"),"sourceId":"price-feed","host":"127.0.0.1","port":price_port,
            }))?,price_factory)
            .query(Box::new(query))
            .sink(Box::new(Output { descriptor: ComponentDescriptor::try_new(id("output"), vec![
                PortDescriptor::new(PortId::try_new("in")?, PortDirection::Input,
                    QueryChangeCodec::schema().descriptor().clone(), PipeRequirements::default()),
            ])? }))
            .bind_stream(endpoint("stocks","out"),stream("stocks"))
            .bind_stream(endpoint("price-feed","out"),stream("price-feed"))
            .bind_stream(endpoint("joined","out"),stream("joined"))
            .connect(EdgeDefinition::new(endpoint("stocks","out"),endpoint("joined","in")),Box::new(BoundedPipeConfig{capacity:8}))
            .connect(EdgeDefinition::new(endpoint("price-feed","out"),endpoint("joined","in")),Box::new(BoundedPipeConfig{capacity:8}))
            .connect(EdgeDefinition::new(endpoint("joined","out"),endpoint("output","in")),Box::new(BoundedPipeConfig{capacity:8}))
            .build()?).build().await?;
        let proof = async {
            let initialize = async {
                core.start().await?;
                core.computation_component("joined")?.wait_started().await?;
                Result::<()>::Ok(())
            };
            let concurrent_write = async {
                entered.notified().await;
                database.sql("UPDATE stocks SET name='during-snapshot' WHERE id=1")?;
                anyhow::ensure!(!owner.snapshot().bootstrap_complete, "partial composition completed early");
                anyhow::ensure!(database.sql("SELECT confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name='composition'")? == feedback,
                    "incomplete HTTP initialization advanced PG feedback");
                gate.add_permits(1);
                Result::<()>::Ok(())
            };
            tokio::time::timeout(TIMEOUT, async {tokio::try_join!(initialize,concurrent_write)})
                .await.with_context(|| {
                    let snapshot = core.inspect_computation_graph().expect("inspection").snapshot();
                    let states: Vec<_> = snapshot.observed.components.iter().map(|(id, state)|
                        format!("{id}: {:?}/{:?} {:?}", state.realization, state.lifecycle, state.waiting_reason())
                    ).collect();
                    format!("mixed snapshot startup deadline: HTTP requests={}, {states:?}", snapshots.load(Ordering::SeqCst))
                })??;
            let wait = |name:&'static str, price:f64| {
                let results = results.clone();
                async move {
                    tokio::time::timeout(TIMEOUT,async {
                        loop {
                            let rows = rows(&results)?;
                            if rows.len() == 1 && rows[0]["name"] == name && rows[0]["price"] == price {
                                return Result::<()>::Ok(());
                            }
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    }).await.with_context(|| format!("mixed query row deadline: expected {name}/{price}, actual {:?}", rows(&results)))?
                }
            };
            wait("during-snapshot",175.5).await?;
            anyhow::ensure!(owner.snapshot().bootstrap_complete && !owner.snapshot().persistent, "wrong query completion/durability");
            reqwest::Client::new().post(format!("http://127.0.0.1:{price_port}/sources/price-feed/events"))
                .json(&json!({"operation":"update","element":{"type":"node","id":"price_AAPL",
                    "labels":["stock_prices"],"properties":{"symbol":"AAPL","price":200.0}}}))
                .send().await?.error_for_status()?;
            wait("during-snapshot",200.0).await?;
            database.sql("BEGIN; UPDATE stocks SET name='rolled-back' WHERE id=1; ROLLBACK;
                UPDATE stocks SET name='live' WHERE id=1;")?;
            wait("live",200.0).await?;
            core.stop().await?;
            let lease = composite.freeze_for_retirement()?;
            anyhow::ensure!(pg.freeze_for_retirement().is_err() && prices.freeze_for_retirement().is_err(),
                "composition did not hold its actual child lifecycles");
            lease.resume();
            database.sql("UPDATE stocks SET name='offline' WHERE id=1")?;
            core.start().await?;
            core.computation_component("joined")?.wait_started().await?;
            wait("offline",175.5).await?;
            anyhow::ensure!(snapshots.load(Ordering::SeqCst) == 2, "price initialization was skipped or duplicated");
            Result::<()>::Ok(())
        }.await;
        let cleanup = core.shutdown().await.map_err(anyhow::Error::from);
        match (proof,cleanup) {
            (Err(primary),Err(cleanup)) => Err(drasi_lib::error::OperationFailures::new(
                "mixed bootstrap proof and cleanup failed",vec![primary,cleanup]).into()),
            (Err(error),_) | (_,Err(error)) => Err(error),
            _ => Ok(()),
        }
    }.await;
    gate.close();
    let _ = shutdown.send(());
    let http_cleanup = tokio::time::timeout(TIMEOUT, http)
        .await
        .context("HTTP fixture cleanup")??;
    let database_cleanup = database.remove();
    let failures: Vec<_> = [result, http_cleanup.map_err(Into::into), database_cleanup]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(drasi_lib::error::OperationFailures::new(
            "mixed bootstrap qualification failed",
            failures,
        )
        .into())
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and rebuilt native PostgreSQL/HTTP libraries"]
async fn postgres_http_composition_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        exercise(dynamic).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and rebuilt native PostgreSQL/HTTP libraries"]
async fn postgres_http_composition_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        exercise(dynamic).await?;
    }
    Ok(())
}
