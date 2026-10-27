// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Requires Podman and the three separately rebuilt native MySQL libraries.

use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::computation::{ComputationIndexProvider, InMemoryComputationProvider};
use drasi_host_sdk::computation::{self, NativePlugin};
use drasi_index_rocksdb::{
    computation::RocksDbComputationProvider, RocksDbMemoryBudget, RocksIndexOptions,
};
use drasi_lib::{computation::v1::*, DrasiLib};
use serde_json::{json, Value};
use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::{mpsc, Semaphore};

const INSTANCE: &str = "native-mysql-family";
#[path = "fixtures/native_mysql/database.rs"]
mod database;
#[path = "fixtures/native_mysql/identity.rs"]
mod identity;
#[path = "fixtures/native_mysql/parity.rs"]
mod parity;
#[path = "fixtures/native_sink_inputs.rs"]
mod sink_inputs;
#[path = "fixtures/native_mysql/teardown.rs"]
mod teardown;
use database::Database;
fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("id")
}
fn resource(value: &str) -> ResourceId {
    ResourceId::try_new(value).expect("resource")
}
fn endpoint(value: &str, port: &str) -> Endpoint {
    Endpoint::new(id(value), PortId::try_new(port).expect("port"))
}
fn stream(value: &str) -> StreamId {
    StreamId::try_new(value).expect("stream")
}
fn scope() -> drasi_computation_plugin_sdk::Scope {
    drasi_computation_plugin_sdk::Scope {
        instance_id: INSTANCE.into(),
        graph_id: ComponentBatch::GRAPH_ID.into(),
        generation: 1,
    }
}

struct GateProvider(Arc<Semaphore>);
struct GatePipe {
    inner: Box<dyn Pipe>,
    gate: Arc<Semaphore>,
}
struct GateReceiver {
    inner: Box<dyn EnvelopeReceiver>,
    gate: Arc<Semaphore>,
}
impl PipeProvider for GateProvider {
    fn capabilities(&self) -> std::result::Result<PipeCapabilities, PipeError> {
        BoundedPipeConfig { capacity: 8 }.capabilities()
    }
    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        let provided = BoundedPipeConfig { capacity: 8 }.create()?;
        Ok(ProvidedPipe {
            pipe: Box::new(GatePipe {
                inner: provided.pipe,
                gate: self.0.clone(),
            }),
            control: provided.control,
        })
    }
}
impl Pipe for GatePipe {
    fn capabilities(&self) -> &PipeCapabilities {
        self.inner.capabilities()
    }
    fn sender(&self) -> Arc<dyn EnvelopeSender> {
        self.inner.sender()
    }
    fn take_receiver(&mut self) -> std::result::Result<Box<dyn EnvelopeReceiver>, PipeError> {
        Ok(Box::new(GateReceiver {
            inner: self.inner.take_receiver()?,
            gate: self.gate.clone(),
        }))
    }
}
#[async_trait]
impl EnvelopeReceiver for GateReceiver {
    async fn receive(&mut self) -> std::result::Result<Option<Delivery>, PipeError> {
        let permit = self.gate.clone().acquire_owned().await.ok();
        let delivery = self.inner.receive().await?;
        if delivery.is_some() {
            if let Some(permit) = permit {
                permit.forget();
            }
        }
        Ok(delivery)
    }
}

struct Capture {
    descriptor: ComponentDescriptor,
    sender: mpsc::Sender<ChangeEnvelope>,
}
#[async_trait]
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
#[async_trait]
impl EnvelopeSink for Capture {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.sender.send(input.envelope).await?;
        Ok(())
    }
}

async fn next(receiver: &mut mpsc::Receiver<ChangeEnvelope>) -> Result<ChangeEnvelope> {
    tokio::time::timeout(Duration::from_secs(10), receiver.recv())
        .await?
        .context("real query output")
}

struct Settings {
    source: Value,
    sink: Value,
}
#[async_trait]
impl ConfigurationResolver for Settings {
    fn validate_reference(&self, key: &str) -> Result<()> {
        anyhow::ensure!(matches!(key, "source" | "sink"), "unknown MySQL settings");
        Ok(())
    }
    async fn resolve(&self, key: &str) -> Result<Value> {
        self.validate_reference(key)?;
        Ok(if key == "source" {
            self.source.clone()
        } else {
            self.sink.clone()
        })
    }
}

fn libraries(dynamic: bool) -> Result<Vec<Arc<NativePlugin>>> {
    if !dynamic {
        return [
            drasi_computation_source_mysql::plugin()?,
            drasi_computation_bootstrap_mysql::plugin()?,
            drasi_computation_sink_storedproc_mysql::plugin()?,
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
        .context("set the rebuilt native plugin directory")?;
    ["source_mysql", "bootstrap_mysql", "sink_storedproc_mysql"]
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

fn rows(results: &QueryResults) -> Result<Vec<Value>> {
    results
        .snapshot()?
        .rows
        .values()
        .map(|record| {
            Ok(QueryChangeCodec::row_values_to_json(
                &QueryChangeCodec::decode_row(record)?.values,
            ))
        })
        .collect()
}

async fn family(dynamic: bool) -> Result<()> {
    let mut database = Database::new().await?;
    database.initialize().await?;
    let native = libraries(dynamic)?;
    let source_factory = native[0]
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == drasi_computation_source_mysql::MYSQL_SHARED_CHANGES
        })
        .context("native row source")?
        .clone();
    let bootstrap = native[1]
        .bootstrap_factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == drasi_computation_bootstrap_mysql::MYSQL_SHARED_ROW_BOOTSTRAP
        })
        .context("native row bootstrap")?;
    let sink_factory = native[2]
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == drasi_computation_sink_storedproc_mysql::MYSQL_STOREDPROC
        })
        .context("native procedure sink")?
        .clone();
    let owners = ["left", "right"].map(|name| {
        Arc::new(
            QuerySourceProgress::new(ComponentBatch::GRAPH_ID, id(name))
                .expect("actual query progress"),
        )
    });
    let consumers = Arc::new(ConsumerProgressSet::new(
        ComponentBatch::GRAPH_ID,
        id("source"),
        owners.iter().cloned(),
    )?);
    let source_configuration =
        json!({"stream":"changes","settings":database.source_settings(),"persistent":false});
    assert!(source_factory
        .create_component(id("source"), source_configuration.clone())
        .is_err());
    assert!(source_factory
        .create_component_with_consumer_progress(
            id("different-source"),
            source_configuration.clone(),
            scope(),
            consumers.clone(),
        )
        .is_err());
    let mut wrong_graph = scope();
    wrong_graph.graph_id = "another-graph".into();
    assert!(source_factory
        .create_component_with_consumer_progress(
            id("source"),
            source_configuration.clone(),
            wrong_graph,
            consumers.clone(),
        )
        .is_err());
    let mut unready = source_factory.create_component_with_consumer_progress(
        id("source"),
        source_configuration,
        scope(),
        consumers.clone(),
    )?;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), unready.start())
            .await
            .is_err(),
        "uninitialized real owners must fence startup"
    );
    tokio::time::timeout(Duration::from_secs(5), unready.stop()).await??;
    assert_eq!(
        database.sql("SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID()").await?,
        "0",
        "waiting for actual owners must not open replication"
    );
    let bootstrap_configuration =
        json!({"source":"source","stream":"changes","settings":database.source_settings()});
    assert!(bootstrap
        .create_provider(
            id("left"),
            bootstrap_configuration.clone(),
            Some(scope()),
            None
        )
        .is_err());
    assert!(bootstrap
        .create_provider(
            id("right"),
            bootstrap_configuration.clone(),
            Some(scope()),
            Some(owners[0].clone()),
        )
        .is_err());
    let substitute = Arc::new(QuerySourceProgress::new(
        ComponentBatch::GRAPH_ID,
        id("left"),
    )?);
    let substitute_provider = bootstrap.create_provider(
        id("left"),
        bootstrap_configuration,
        Some(scope()),
        Some(substitute),
    )?;
    let actual = Arc::new(QuerySourceProgress::new(
        ComponentBatch::GRAPH_ID,
        id("left"),
    )?);
    let mut invalid = ContinuousQueryTransformer::new(
        query_definition("left"),
        Arc::new(InMemoryComputationProvider),
    )
    .await?
    .with_source_progress(actual)?
    .with_recovery_scope(INSTANCE)?
    .with_bootstrap(substitute_provider);
    let rejected = invalid
        .start()
        .await
        .expect_err("same name is not owner identity");
    invalid.stop().await?;
    assert!(
        format!("{rejected:#}").contains("actual progress owner"),
        "{rejected:#}"
    );
    let settings = ResourceHandle::new(
        ResourceRole::SecretStore,
        Arc::new(ConfigurationResolverResource(Arc::new(Settings {
            source: database.source_settings(),
            sink: database.sink_settings(),
        }))),
    );
    let mut builder = ComponentBatch::builder();
    for (name, role, handle) in [
        ("settings", ResourceRole::SecretStore, settings),
        (
            "consumers",
            ResourceRole::Checkpoint,
            ResourceHandle::new(ResourceRole::Checkpoint, consumers),
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
    let reference = |key: &str| ConfigurationValue::Reference {
        resource: resource("settings"),
        key: key.into(),
        secret: true,
    };
    let mut source = source_factory.specification(
        id("source"),
        json!({"stream":"changes","settings":{},"persistent":false}),
    )?;
    source
        .configuration
        .insert("settings".into(), reference("source"));
    source
        .dependencies
        .insert("consumer_progress".into(), vec![resource("consumers")]);
    let mut sink = sink_factory.specification(id("procedures"), json!({"settings":{}}))?;
    sink.configuration
        .insert("settings".into(), reference("sink"));
    builder = builder
        .component(source, source_factory)
        .component(sink, sink_factory.clone())
        .bind_stream(endpoint("source", "out"), stream("changes"));
    let mut results = Vec::new();
    let mut outputs = Vec::new();
    let gate = Arc::new(Semaphore::new(1));
    for owner in &owners {
        let name = owner.component_id().as_str();
        let provider = bootstrap.create_provider(
            owner.component_id().clone(),
            json!({"source":"source","stream":"changes","settings":database.source_settings()}),
            Some(scope()),
            Some(owner.clone()),
        )?;
        let query = ContinuousQueryTransformer::new_configured(
            query_definition(name),
            Arc::new(InMemoryComputationProvider),
            QueryOptions::default(),
            QueryExecutionSettings::default(),
            None,
        )
        .await?
        .with_source_progress(owner.clone())?
        .with_recovery_scope(INSTANCE)?
        .with_bootstrap(provider);
        results.push(query.results());
        let capture = format!("{name}-capture");
        let (sender, receiver) = mpsc::channel(32);
        outputs.push(receiver);
        builder = builder
            .query(Box::new(query))
            .sink(Box::new(Capture {
                descriptor: ComponentDescriptor::try_new(
                    id(&capture),
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                sender,
            }))
            .bind_stream(endpoint(name, "out"), stream(&format!("{name}/out")))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                if name == "right" {
                    Box::new(GateProvider(gate.clone())) as Box<dyn PipeProvider>
                } else {
                    Box::new(BoundedPipeConfig { capacity: 8 })
                },
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint("procedures", "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint(&capture, "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            );
    }
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_components(builder.build()?)
        .build()
        .await?;
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30),core.start()).await??;
        for query in &results { assert_eq!(rows(query)?,[json!({"id":1,"value":"initial"})]); }
        let schema = tokio::time::timeout(Duration::from_secs(1),core.get_source_schema("source"))
            .await??.context("actual native MySQL catalog schema")?;
        assert_eq!(schema.nodes.len(),1);
        assert_eq!(schema.nodes[0].label,"items");
        assert_eq!(schema.nodes[0].properties.iter().map(|property| property.name.as_str()).collect::<Vec<_>>(),
            ["id","value"]);
        assert_eq!(serde_json::to_value(schema.nodes[0].properties[0].data_type)?,json!("integer"));
        assert_eq!(serde_json::to_value(schema.nodes[0].properties[1].data_type)?,json!("string"));
        assert!(schema.nodes[0].properties[0].description.as_deref()
            .is_some_and(|description| description.contains("primary key")));
        database.sql("BEGIN; UPDATE items SET value='updated' WHERE id=1; \
            INSERT INTO items VALUES(2,'second'),(3,'third'); DELETE FROM items WHERE id=3; COMMIT;").await?;
        let mut actual_input = None;
        for _ in 0..4 {
            let input = next(&mut outputs[0]).await?;
            if input.changes().operations().len() == 1 { actual_input = Some(input); }
        }
        next(&mut outputs[1]).await?;
        let key = SourceProgressKey::Source("source".into());
        let partial = position(&owners[1], &key)?;
        assert_eq!(partial["offset"], 0);
        assert_eq!(partial["count"], 4);
        let previous = owners[0].snapshot().checkpoints[&key].sequence;
        tokio::time::timeout(Duration::from_secs(10), core.computation_component("source")?.stop()).await??;
        tokio::time::timeout(Duration::from_secs(10), core.computation_component("source")?.start()).await??;
        gate.add_permits(128);
        for _ in 0..3 { next(&mut outputs[1]).await?; }
        for output in &mut outputs {
            assert!(tokio::time::timeout(Duration::from_millis(100),output.recv()).await.is_err(),
                "covered queued/replayed rows were applied again");
        }
        for owner in &owners {
            let position = position(owner, &key)?;
            assert_eq!(position["offset"],3);
            assert_eq!(position["count"],4);
        }
        database.expect_count("SELECT COUNT(*) FROM audit", "8").await?;
        assert_eq!(database.sql("SELECT COUNT(*) FROM effects").await?,"4");
        for query in &results {
            let mut actual = rows(query)?;
            actual.sort_by_key(Value::to_string);
            assert_eq!(actual,[json!({"id":1,"value":"updated"}),json!({"id":2,"value":"second"})]);
        }
        database.sql("UPDATE items SET value='fresh' WHERE id=2").await?;
        for output in &mut outputs { next(output).await?; }
        for owner in &owners {
            assert!(owner.snapshot().checkpoints[&key].sequence > previous,
                "rewind must allocate above earlier consumer and source admissions");
        }
        let actual_input = actual_input.context("single real query change")?;
        procedure_lifecycle(&database,&sink_factory,actual_input.clone()).await?;
        identity::rotation(&database,&sink_factory,actual_input).await?;
        database.sql("BEGIN; DELETE FROM items; ROLLBACK;").await?;
        database.sql("UPDATE items SET value='barrier' WHERE id=2").await?;
        for output in &mut outputs { next(output).await?; }
        for output in &mut outputs {
            assert!(tokio::time::timeout(Duration::from_millis(100),output.recv()).await.is_err(),
                "rolled-back rows reached a query");
        }
        core.stop().await?;
        database.sql("UPDATE items SET value='offline' WHERE id=1").await?;
        tokio::time::timeout(Duration::from_secs(30),core.start()).await??;
        for query in &results { assert!(rows(query)?.contains(&json!({"id":1,"value":"offline"}))); }
        for owner in &owners { assert!(owner.snapshot().bootstrap_complete && !owner.snapshot().persistent); }
        Result::<()>::Ok(())
    }.await;
    gate.close();
    let cleanup = core.shutdown().await.map_err(anyhow::Error::from);
    let sessions = database
        .expect_count(
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID()",
            "0",
        )
        .await;
    let removed = database.cleanup();
    let errors: Vec<_> = [proof, cleanup, sessions, removed]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(
            drasi_lib::error::OperationFailures::new("native MySQL family proof failed", errors)
                .into(),
        )
    }
}

fn query_definition(name: &str) -> ContinuousQueryDefinition {
    ContinuousQueryDefinition {
        graph_id: ComponentBatch::GRAPH_ID.into(),
        id: id(name),
        query: "MATCH (n:items) RETURN n.id AS id,n.value AS value".into(),
        language: ComputationQueryLanguage::Cypher,
        output_stream: stream(&format!("{name}/out")),
        outbox_capacity: NonZeroUsize::new(32).expect("outbox"),
    }
}

fn position(owner: &QuerySourceProgress, key: &SourceProgressKey) -> Result<Value> {
    let state = owner.snapshot();
    Ok(serde_json::from_slice(
        state.checkpoints[key]
            .source_position
            .as_deref()
            .context("actual source-specific row position")?,
    )?)
}

async fn procedure_lifecycle(
    database: &Database,
    factory: &computation::NativeFactory,
    envelope: ChangeEnvelope,
) -> Result<()> {
    let configuration = |template: &str, shutdown_ms: u64| {
        let mut settings = database.sink_settings();
        settings["shutdown_timeout_ms"] = json!(shutdown_ms);
        settings["command_timeout_ms"] = json!(5000);
        for event in ["added", "updated", "deleted"] {
            settings["default_template"][event]["template"] = json!(template);
        }
        json!({"settings":settings})
    };
    let input = || InputEnvelope {
        port: PortId::try_new("in").expect("input"),
        envelope: envelope.clone(),
    };
    let mut sink = sink_inputs::create(
        factory,
        id("held-procedure"),
        configuration("CALL wait_effect({{param output.alias}})", 50),
    )?;
    sink.start().await?;
    let mut pending = Box::pin(sink.handle(input()));
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            tokio::select! {
                result = &mut pending => {
                    result?;
                    anyhow::bail!("held native procedure completed before cancellation");
                }
                _ = tokio::time::sleep(Duration::from_millis(10)) => {
                    if database.sql("SELECT COUNT(*) FROM information_schema.PROCESSLIST \
                        WHERE (INFO LIKE '%wait_effect%' OR STATE='User sleep') AND ID<>CONNECTION_ID()").await? != "0"
                    {
                        return Result::<()>::Ok(());
                    }
                }
            }
        }
    })
    .await??;
    drop(pending);
    assert!(
        sink.handle(input()).await.is_err(),
        "pending CALL must not be replaced"
    );
    assert!(
        sink.stop().await.is_err(),
        "short stop must retain CALL ownership"
    );
    assert!(
        sink.start().await.is_err(),
        "pending cleanup cannot authorize restart"
    );
    tokio::time::sleep(Duration::from_millis(1100)).await;
    sink.stop().await?;
    assert_eq!(
        database
            .sql("SELECT COUNT(*) FROM audit WHERE operation='wait'")
            .await?,
        "1"
    );
    let mut failed = sink_inputs::create(
        factory,
        id("failed-procedure"),
        configuration("CALL fail_effect({{param output.alias}})", 5000),
    )?;
    failed.start().await?;
    assert!(failed.handle(input()).await.is_err());
    assert!(
        failed.handle(input()).await.is_err(),
        "submitted failed CALL is not retried"
    );
    failed.stop().await?;
    assert_eq!(
        database
            .sql("SELECT COUNT(*) FROM audit WHERE operation='failed'")
            .await?,
        "1"
    );
    Ok(())
}

#[derive(Clone, Copy)]
enum TrackedMode {
    SingleTransactions,
    SharedTransactions,
    SharedRows,
    PositionalRows,
}

struct TrackedGraph {
    core: DrasiLib,
    owners: Vec<Arc<QuerySourceProgress>>,
    results: Vec<QueryResults>,
    outputs: Vec<mpsc::Receiver<ChangeEnvelope>>,
    gate: Arc<Semaphore>,
}

async fn tracked_graph(
    database: &Database,
    native: &[Arc<NativePlugin>],
    path: &Path,
    mode: TrackedMode,
    gate: Arc<Semaphore>,
) -> Result<TrackedGraph> {
    let shared = !matches!(mode, TrackedMode::SingleTransactions);
    let rows = matches!(mode, TrackedMode::SharedRows | TrackedMode::PositionalRows);
    let positional = matches!(mode, TrackedMode::PositionalRows);
    let source_name = match mode {
        TrackedMode::SingleTransactions => drasi_computation_source_mysql::MYSQL_TRANSACTIONS,
        TrackedMode::SharedTransactions => {
            drasi_computation_source_mysql::MYSQL_SHARED_TRANSACTIONS
        }
        TrackedMode::SharedRows => drasi_computation_source_mysql::MYSQL_SHARED_CHANGES,
        TrackedMode::PositionalRows => {
            drasi_computation_source_mysql::MYSQL_SHARED_POSITIONAL_CHANGES
        }
    };
    let bootstrap_name = match mode {
        TrackedMode::SingleTransactions => drasi_computation_bootstrap_mysql::MYSQL_BOOTSTRAP,
        TrackedMode::SharedTransactions => {
            drasi_computation_bootstrap_mysql::MYSQL_SHARED_TRANSACTION_BOOTSTRAP
        }
        TrackedMode::SharedRows | TrackedMode::PositionalRows => {
            drasi_computation_bootstrap_mysql::MYSQL_SHARED_ROW_BOOTSTRAP
        }
    };
    let source_factory = native[0]
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == source_name)
        .context("native transaction source")?
        .clone();
    let bootstrap = native[1]
        .bootstrap_factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == bootstrap_name)
        .context("native transaction bootstrap")?;
    let sink_factory = native[2]
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == drasi_computation_sink_storedproc_mysql::MYSQL_STOREDPROC
        })
        .context("native transaction result sink")?
        .clone();
    let mut settings = database.source_settings();
    settings["output"] = json!(if rows { "changes" } else { "transactions" });
    settings["replay"] = json!("until_processed");
    if positional {
        settings["start"] = json!({"mode":"start"});
        settings["table_keys"] = json!([{"table":"items","key_columns":["@0"]}]);
    }
    let limits: SourceTransactionLimits = serde_json::from_value(settings["transactions"].clone())?;
    let names = if shared {
        &["left", "right"][..]
    } else {
        &["left"][..]
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
    let progress = if shared {
        ResourceHandle::new(
            ResourceRole::Checkpoint,
            Arc::new(ConsumerProgressSet::new(
                ComponentBatch::GRAPH_ID,
                id("source"),
                owners.iter().cloned(),
            )?),
        )
    } else {
        ResourceHandle::new(
            ResourceRole::Checkpoint,
            Arc::new(QuerySourceProgressResource(owners[0].clone())),
        )
    };
    let mut builder = ComponentBatch::builder();
    for (name, role, handle) in [
        ("progress", ResourceRole::Checkpoint, progress),
        (
            "settings",
            ResourceRole::SecretStore,
            ResourceHandle::new(
                ResourceRole::SecretStore,
                Arc::new(ConfigurationResolverResource(Arc::new(Settings {
                    source: settings.clone(),
                    sink: database.sink_settings(),
                }))),
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
    let reference = |key: &str| ConfigurationValue::Reference {
        resource: resource("settings"),
        key: key.into(),
        secret: true,
    };
    let mut configuration = json!({"stream":"changes","settings":{}});
    if rows {
        configuration["persistent"] = json!(true);
    }
    let mut source = source_factory.specification(id("source"), configuration)?;
    source
        .configuration
        .insert("settings".into(), reference("source"));
    source.dependencies.insert(
        if shared {
            "consumer_progress"
        } else {
            "source_progress"
        }
        .into(),
        vec![resource("progress")],
    );
    let mut sink = sink_factory.specification(id("procedures"), json!({"settings":{}}))?;
    sink.configuration
        .insert("settings".into(), reference("sink"));
    builder = builder
        .component(source, source_factory)
        .component(sink, sink_factory)
        .bind_stream(endpoint("source", "out"), stream("changes"));
    let indexes: Arc<dyn ComputationIndexProvider> = Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    ));
    let mut results = Vec::new();
    let mut outputs = Vec::new();
    for owner in &owners {
        let name = owner.component_id().as_str();
        let mut definition = query_definition(name);
        if positional {
            definition.query = "MATCH (n:items) RETURN n.`@0` AS id,n.`@1` AS value".into();
        }
        let mut query = ContinuousQueryTransformer::new_configured(
            definition,
            indexes.clone(),
            QueryOptions {
                recovery: QueryRecoveryPolicy::Strict,
                ..Default::default()
            },
            QueryExecutionSettings {
                source_transactions: (!rows).then_some(limits),
                ..Default::default()
            },
            None,
        )
        .await?
        .with_source_progress(owner.clone())?
        .with_recovery_scope(INSTANCE)?;
        if !positional {
            query = query.with_bootstrap(bootstrap.create_provider(
                owner.component_id().clone(),
                json!({"source":"source","stream":"changes","settings":settings}),
                Some(scope()),
                Some(owner.clone()),
            )?);
        }
        results.push(query.results());
        let capture = format!("{name}-capture");
        let (sender, receiver) = mpsc::channel(32);
        outputs.push(receiver);
        builder = builder
            .query(Box::new(query))
            .sink(Box::new(Capture {
                descriptor: ComponentDescriptor::try_new(
                    id(&capture),
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                sender,
            }))
            .bind_stream(endpoint(name, "out"), stream(&format!("{name}/out")))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                if name == "right" {
                    Box::new(GateProvider(gate.clone())) as Box<dyn PipeProvider>
                } else {
                    Box::new(BoundedPipeConfig { capacity: 8 })
                },
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint("procedures", "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint(&capture, "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            );
    }
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_components(builder.build()?)
        .build()
        .await?;
    Ok(TrackedGraph {
        core,
        owners,
        results,
        outputs,
        gate,
    })
}

async fn transactions(dynamic: bool, shared: bool) -> Result<()> {
    let mut database = Database::new().await?;
    database.initialize().await?;
    database.sql("INSERT INTO items VALUES(2,'second')").await?;
    let native = libraries(dynamic)?;
    let path = tempfile::tempdir()?;
    let mode = if shared {
        TrackedMode::SharedTransactions
    } else {
        TrackedMode::SingleTransactions
    };
    let mut graph = tracked_graph(
        &database,
        &native,
        path.path(),
        mode,
        Arc::new(Semaphore::new(0)),
    )
    .await?;
    let mut committed = Vec::new();
    let mut transports = Vec::new();
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30), graph.core.start()).await??;
        for query in &graph.results {
            let mut actual = rows(query)?;
            actual.sort_by_key(Value::to_string);
            assert_eq!(
                actual,
                [
                    json!({"id":1,"value":"initial"}),
                    json!({"id":2,"value":"second"})
                ]
            );
        }
        graph.core.stop().await?;
        for output in &mut graph.outputs {
            while let Ok(initial) = output.try_recv() {
                for operation in initial.changes().operations() {
                    assert!(
                        matches!(operation, ChangeOperation::Added { .. }),
                        "initial snapshot must not invent mutations"
                    );
                }
            }
        }
        graph.core.start().await?;
        let heads = graph
            .results
            .iter()
            .map(|result| result.snapshot().map(|value| value.as_of_sequence))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        database
            .sql(
                "BEGIN; UPDATE items SET value='final' WHERE id=1; \
            DELETE FROM items WHERE id=2; INSERT INTO items VALUES(3,'new'); COMMIT;",
            )
            .await?;
        let left = next(&mut graph.outputs[0]).await?;
        assert_eq!(
            left.changes().operations().len(),
            3,
            "one transaction must remain one query envelope"
        );
        assert_eq!(graph.results[0].snapshot()?.as_of_sequence, heads[0] + 1);
        if shared {
            let key = SourceProgressKey::Source("source".into());
            assert_eq!(position(&graph.owners[0], &key)?["version"], 1);
            assert_eq!(
                position(&graph.owners[1], &key)?["version"],
                2,
                "held owner must retain its actual snapshot boundary"
            );
            graph.core.computation_component("source")?.stop().await?;
            graph.core.computation_component("source")?.start().await?;
            graph.gate.add_permits(64);
            let right = next(&mut graph.outputs[1]).await?;
            assert_eq!(right.changes().operations().len(), 3);
            assert_eq!(graph.results[1].snapshot()?.as_of_sequence, heads[1] + 1);
        }
        for output in &mut graph.outputs {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), output.recv())
                    .await
                    .is_err(),
                "whole-transaction rewind repeated committed effects"
            );
        }
        database
            .expect_count("SELECT COUNT(*) FROM audit", if shared { "6" } else { "3" })
            .await?;
        database
            .sql("BEGIN; DELETE FROM items; ROLLBACK; UPDATE items SET value='barrier' WHERE id=3")
            .await?;
        for (index, output) in graph.outputs.iter_mut().enumerate() {
            assert_eq!(next(output).await?.changes().operations().len(), 1);
            assert_eq!(
                graph.results[index].snapshot()?.as_of_sequence,
                heads[index] + 2
            );
        }
        for owner in &graph.owners {
            let state = owner.snapshot();
            assert!(state.persistent && state.bootstrap_complete && state.recovered);
            transports.push(state.transport_sequences[&stream("changes")]);
        }
        for query in &graph.results {
            committed.push(query.snapshot()?.as_of_sequence);
        }
        Result::<()>::Ok(())
    }
    .await;
    graph.gate.close();
    let cleanup = graph.core.shutdown().await.map_err(anyhow::Error::from);
    drop(graph);
    combine("first native MySQL transaction graph", [proof, cleanup])?;
    database
        .sql("UPDATE items SET value='offline' WHERE id=1")
        .await?;
    let mut recovered = tracked_graph(
        &database,
        &native,
        path.path(),
        mode,
        Arc::new(Semaphore::new(64)),
    )
    .await?;
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30), recovered.core.start()).await??;
        for (index, output) in recovered.outputs.iter_mut().enumerate() {
            assert_eq!(
                next(output).await?.changes().operations().len(),
                1,
                "cold owner reconstruction must replay only the missed committed transaction"
            );
            assert_eq!(
                recovered.results[index].snapshot()?.as_of_sequence,
                committed[index] + 1
            );
            assert!(rows(&recovered.results[index])?.contains(&json!({"id":1,"value":"offline"})));
            let state = recovered.owners[index].snapshot();
            assert!(state.persistent && state.bootstrap_complete && state.recovered);
            assert!(state.transport_sequences[&stream("changes")] > transports[index]);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), output.recv())
                    .await
                    .is_err(),
                "cold reconstruction repeated a snapshot or completed transaction"
            );
        }
        database
            .expect_count(
                "SELECT COUNT(*) FROM audit",
                if shared { "10" } else { "5" },
            )
            .await?;
        Result::<()>::Ok(())
    }
    .await;
    recovered.gate.close();
    let cleanup = recovered.core.shutdown().await.map_err(anyhow::Error::from);
    drop(recovered);
    let sessions = database.expect_count(
        "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID()", "0",
    ).await;
    let removed = database.cleanup();
    combine(
        "recovered native MySQL transaction graph",
        [proof, cleanup, sessions, removed],
    )
}

async fn persistent_rows(dynamic: bool) -> Result<()> {
    persistent_rows_profile(dynamic, false).await
}

async fn persistent_rows_profile(dynamic: bool, positional: bool) -> Result<()> {
    let mut database = Database::start(false, positional).await?;
    database.initialize().await?;
    let native = libraries(dynamic)?;
    let path = tempfile::tempdir()?;
    let mode = if positional {
        TrackedMode::PositionalRows
    } else {
        TrackedMode::SharedRows
    };
    let mut graph = tracked_graph(
        &database,
        &native,
        path.path(),
        mode,
        Arc::new(Semaphore::new(if positional { 2 } else { 1 })),
    )
    .await?;
    let mut committed = Vec::new();
    let mut transports = Vec::new();
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30), graph.core.start()).await??;
        if positional {
            for output in &mut graph.outputs {
                assert_eq!(next(output).await?.changes().operations().len(), 1);
            }
        }
        for query in &graph.results {
            assert_eq!(rows(query)?, [json!({"id":1,"value":"initial"})]);
        }
        database
            .sql(
                "BEGIN; UPDATE items SET value='updated' WHERE id=1; \
            INSERT INTO items VALUES(2,'second'),(3,'third'); DELETE FROM items WHERE id=3; COMMIT",
            )
            .await?;
        for _ in 0..4 {
            assert_eq!(
                next(&mut graph.outputs[0])
                    .await?
                    .changes()
                    .operations()
                    .len(),
                1
            );
        }
        next(&mut graph.outputs[1]).await?;
        let key = SourceProgressKey::Source("source".into());
        assert_eq!(position(&graph.owners[1], &key)?["offset"], 0);
        assert_eq!(position(&graph.owners[0], &key)?["offset"], 3);
        graph.core.computation_component("source")?.stop().await?;
        graph.core.computation_component("source")?.start().await?;
        graph.gate.add_permits(64);
        for _ in 0..3 {
            next(&mut graph.outputs[1]).await?;
        }
        for output in &mut graph.outputs {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), output.recv())
                    .await
                    .is_err(),
                "persistent partial-row replay repeated committed effects"
            );
        }
        for owner in &graph.owners {
            let state = owner.snapshot();
            assert!(state.persistent && state.recovered);
            assert_eq!(state.bootstrap_complete, !positional);
            assert_eq!(position(owner, &key)?["offset"], 3);
            transports.push(state.transport_sequences[&stream("changes")]);
        }
        for result in &graph.results {
            committed.push(result.snapshot()?.as_of_sequence);
        }
        database
            .expect_count(
                "SELECT COUNT(*) FROM audit",
                if positional { "10" } else { "8" },
            )
            .await?;
        Result::<()>::Ok(())
    }
    .await;
    graph.gate.close();
    let cleanup = graph.core.shutdown().await.map_err(anyhow::Error::from);
    drop(graph);
    combine("persistent native MySQL rows", [proof, cleanup])?;
    database
        .sql("UPDATE items SET value='offline' WHERE id=1")
        .await?;
    let mut recovered = tracked_graph(
        &database,
        &native,
        path.path(),
        mode,
        Arc::new(Semaphore::new(64)),
    )
    .await?;
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30), recovered.core.start()).await??;
        for (index, output) in recovered.outputs.iter_mut().enumerate() {
            assert_eq!(next(output).await?.changes().operations().len(), 1);
            assert_eq!(
                recovered.results[index].snapshot()?.as_of_sequence,
                committed[index] + 1
            );
            assert!(rows(&recovered.results[index])?.contains(&json!({"id":1,"value":"offline"})));
            let state = recovered.owners[index].snapshot();
            assert!(state.persistent && state.recovered);
            assert_eq!(state.bootstrap_complete, !positional);
            assert!(state.transport_sequences[&stream("changes")] > transports[index]);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), output.recv())
                    .await
                    .is_err(),
                "persistent row restore must not replay the initial snapshot or covered rows"
            );
        }
        database
            .expect_count(
                "SELECT COUNT(*) FROM audit",
                if positional { "12" } else { "10" },
            )
            .await?;
        Result::<()>::Ok(())
    }
    .await;
    recovered.gate.close();
    let cleanup = recovered.core.shutdown().await.map_err(anyhow::Error::from);
    drop(recovered);
    let sessions = database.expect_count(
        "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID()","0",
    ).await;
    let removed = database.cleanup();
    combine(
        "cold native MySQL row recovery",
        [proof, cleanup, sessions, removed],
    )
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn native_mysql_positional_recovery_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        persistent_rows_profile(dynamic, true).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and rebuilt native MySQL libraries"]
async fn native_mysql_positional_recovery_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        persistent_rows_profile(dynamic, true).await?;
    }
    Ok(())
}

async fn strict_history(dynamic: bool, schema_change: bool) -> Result<()> {
    let mut database = Database::new().await?;
    database.initialize().await?;
    let native = libraries(dynamic)?;
    let path = tempfile::tempdir()?;
    let mut graph = tracked_graph(
        &database,
        &native,
        path.path(),
        TrackedMode::SingleTransactions,
        Arc::new(Semaphore::new(64)),
    )
    .await?;
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30), graph.core.start()).await??;
        database
            .sql("UPDATE items SET value='committed' WHERE id=1")
            .await?;
        next(&mut graph.outputs[0]).await?;
        assert_eq!(
            rows(&graph.results[0])?,
            [json!({"id":1,"value":"committed"})]
        );
        Result::<()>::Ok(())
    }
    .await;
    let cleanup = graph.core.shutdown().await.map_err(anyhow::Error::from);
    drop(graph);
    combine(
        "native MySQL checkpoint before history change",
        [proof, cleanup],
    )?;
    if schema_change {
        database
            .sql("ALTER TABLE items ADD COLUMN added INT NULL")
            .await?;
    } else {
        database.sql("FLUSH BINARY LOGS").await?;
        let current = database.sql("SHOW BINARY LOG STATUS").await?;
        let file = current
            .split('\t')
            .next()
            .context("owned binlog filename")?;
        anyhow::ensure!(
            file.strip_prefix("mysql-bin.").is_some_and(
                |suffix| !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
            ),
            "unexpected owned fixture binlog filename"
        );
        database
            .sql(&format!("PURGE BINARY LOGS TO '{file}'"))
            .await?;
    }
    let recovered = tracked_graph(
        &database,
        &native,
        path.path(),
        TrackedMode::SingleTransactions,
        Arc::new(Semaphore::new(64)),
    )
    .await?;
    let started = tokio::time::timeout(Duration::from_secs(20), recovered.core.start()).await;
    let proof = (|| {
        let error = started?.expect_err("changed native history must not silently resume");
        let message = format!("{error:#}");
        let expected = if schema_change {
            "binding or sequence mismatch"
        } else {
            "position is unavailable"
        };
        anyhow::ensure!(message.contains(expected), "{message}");
        anyhow::ensure!(
            recovered.owners[0].snapshot().reset_generation == 0,
            "strict recovery reset retained data"
        );
        Ok(())
    })();
    let cleanup = recovered.core.shutdown().await.map_err(anyhow::Error::from);
    drop(recovered);
    let sessions = database.expect_count(
        "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID()","0",
    ).await;
    let removed = database.cleanup();
    combine(
        "native MySQL strict changed-history recovery",
        [proof, cleanup, sessions, removed],
    )
}

fn combine<const N: usize>(operation: &str, outcomes: [Result<()>; N]) -> Result<()> {
    let errors: Vec<_> = outcomes.into_iter().filter_map(Result::err).collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(drasi_lib::error::OperationFailures::new(operation, errors).into())
    }
}

async fn untracked(dynamic: bool, transactions: bool) -> Result<()> {
    let mut database = Database::new().await?;
    database.initialize().await?;
    if !transactions {
        database
            .sql(
                "ALTER TABLE items ENGINE=MyISAM; \
            CREATE TABLE tx(id INT PRIMARY KEY,value VARCHAR(80) NOT NULL) ENGINE=InnoDB",
            )
            .await?;
    }
    let native = libraries(dynamic)?;
    let source_name = if transactions {
        drasi_computation_source_mysql::MYSQL_UNTRACKED_TRANSACTIONS
    } else {
        drasi_computation_source_mysql::MYSQL_CHANGES
    };
    let source_factory = native[0]
        .factories()
        .iter()
        .find(|factory| factory.metadata().implementation.name.as_ref() == source_name)
        .context("native untracked source")?
        .clone();
    let sink_factory = native[2]
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == drasi_computation_sink_storedproc_mysql::MYSQL_STOREDPROC
        })
        .context("native untracked output sink")?
        .clone();
    let mut settings = database.source_settings();
    settings["output"] = json!(if transactions {
        "transactions"
    } else {
        "changes"
    });
    settings["replay"] = Value::Null;
    settings["start"] = json!({"mode":"end"});
    if !transactions {
        settings["tables"] = json!(["items", "tx"]);
    }
    let limits: SourceTransactionLimits = serde_json::from_value(settings["transactions"].clone())?;
    let mut source =
        source_factory.specification(id("source"), json!({"stream":"changes","settings":{}}))?;
    let reference = |key: &str| ConfigurationValue::Reference {
        resource: resource("settings"),
        key: key.into(),
        secret: true,
    };
    source
        .configuration
        .insert("settings".into(), reference("source"));
    let mut sink = sink_factory.specification(id("procedures"), json!({"settings":{}}))?;
    sink.configuration
        .insert("settings".into(), reference("sink"));
    let mut builder = ComponentBatch::builder()
        .declare_resource(ResourceSpecification {
            id: resource("settings"),
            role: ResourceRole::SecretStore,
            ownership: ResourceOwnership::Borrowed,
            binding: "settings".into(),
        })?
        .provide_resource(
            resource("settings"),
            ResourceHandle::new(
                ResourceRole::SecretStore,
                Arc::new(ConfigurationResolverResource(Arc::new(Settings {
                    source: settings,
                    sink: database.sink_settings(),
                }))),
            ),
        )?
        .component(source, source_factory)
        .component(sink, sink_factory.clone())
        .bind_stream(endpoint("source", "out"), stream("changes"));
    let path = tempfile::tempdir()?;
    let indexes: Arc<dyn ComputationIndexProvider> = if transactions {
        Arc::new(RocksDbComputationProvider::new(
            path.path(),
            RocksIndexOptions::new(
                false,
                false,
                RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
            ),
        ))
    } else {
        Arc::new(InMemoryComputationProvider)
    };
    let mut results = Vec::new();
    let mut outputs = Vec::new();
    for name in ["left", "right"] {
        let mut definition = query_definition(name);
        definition.query = "MATCH (n) RETURN n.id AS id,n.value AS value".into();
        let query = ContinuousQueryTransformer::new_configured(
            definition,
            indexes.clone(),
            QueryOptions::default(),
            QueryExecutionSettings {
                source_transactions: transactions.then_some(limits),
                ..Default::default()
            },
            None,
        )
        .await?
        .with_recovery_scope(INSTANCE)?;
        results.push(query.results());
        let capture = format!("{name}-capture");
        let (sender, receiver) = mpsc::channel(32);
        outputs.push(receiver);
        builder = builder
            .query(Box::new(query))
            .sink(Box::new(Capture {
                descriptor: ComponentDescriptor::try_new(
                    id(&capture),
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                sender,
            }))
            .bind_stream(endpoint(name, "out"), stream(&format!("{name}/out")))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint("procedures", "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint(&capture, "in")),
                Box::new(BoundedPipeConfig { capacity: 8 }),
            );
    }
    let core = DrasiLib::builder()
        .with_id(INSTANCE)
        .with_components(builder.build()?)
        .build()
        .await?;
    let proof = async {
        tokio::time::timeout(Duration::from_secs(30),core.start()).await??;
        for query in &results { assert!(rows(query)?.is_empty(),"untracked streaming must not invent an initial snapshot"); }
        database.sql(if transactions {
            "BEGIN; INSERT INTO items VALUES(100,'first'),(101,'second'); COMMIT;"
        } else {
            "BEGIN; INSERT INTO tx VALUES(999,'must roll back'); INSERT INTO items VALUES(100,'first'),(101,'second'); ROLLBACK;"
        }).await?;
        let mut actual_input = None;
        for output in &mut outputs {
            for _ in 0..if transactions { 1 } else { 2 } {
                let envelope = next(output).await?;
                assert_eq!(envelope.changes().operations().len(),if transactions { 2 } else { 1 });
                if !transactions && actual_input.is_none() { actual_input = Some(envelope); }
            }
        }
        for query in &results {
            let mut actual = rows(query)?;
            actual.sort_by_key(Value::to_string);
            assert_eq!(actual,[json!({"id":100,"value":"first"}),json!({"id":101,"value":"second"})]);
        }
        database.expect_count("SELECT COUNT(*) FROM audit","4").await?;
        database.sql("BEGIN; UPDATE items SET value='changed' WHERE id=100; DELETE FROM items WHERE id=101; COMMIT").await?;
        for output in &mut outputs {
            for _ in 0..if transactions { 1 } else { 2 } {
                let envelope = next(output).await?;
                assert_eq!(envelope.changes().operations().len(),if transactions { 2 } else { 1 });
            }
            assert!(tokio::time::timeout(Duration::from_millis(100),output.recv()).await.is_err());
        }
        database.expect_count("SELECT COUNT(*) FROM audit","8").await?;
        assert_eq!(database.sql("SELECT query_id,id,value FROM effects ORDER BY query_id,id").await?,
            "left\t100\tchanged\nright\t100\tchanged");
        for query in &results { assert_eq!(rows(query)?,[json!({"id":100,"value":"changed"})]); }
        if let Some(input) = actual_input {
            procedure_lifecycle(&database, &sink_factory, input).await?;
        }
        Result::<()>::Ok(())
    }.await;
    let cleanup = core.shutdown().await.map_err(anyhow::Error::from);
    let sessions = database.expect_count(
        "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID()","0",
    ).await;
    let removed = database.cleanup();
    combine(
        "native untracked MySQL graph",
        [proof, cleanup, sessions, removed],
    )
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_family_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        family(dynamic).await?;
    }
    Ok(())
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_family_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        family(dynamic).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_single_transactions_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        transactions(dynamic, false).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_single_transactions_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        transactions(dynamic, false).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_shared_transactions_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        transactions(dynamic, true).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_shared_transactions_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        transactions(dynamic, true).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_persistent_rows_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        persistent_rows(dynamic).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_persistent_rows_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        persistent_rows(dynamic).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_strict_history_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        for schema_change in [false, true] {
            strict_history(dynamic, schema_change).await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_strict_history_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        for schema_change in [false, true] {
            strict_history(dynamic, schema_change).await?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_untracked_current_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        for transactions in [false, true] {
            untracked(dynamic, transactions).await.with_context(|| {
                format!("untracked transactions={transactions}, dynamic={dynamic}")
            })?;
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Podman and three rebuilt native MySQL libraries"]
async fn native_mysql_untracked_multi_thread_static_and_dynamic() -> Result<()> {
    for dynamic in [false, true] {
        for transactions in [false, true] {
            untracked(dynamic, transactions).await.with_context(|| {
                format!("untracked transactions={transactions}, dynamic={dynamic}")
            })?;
        }
    }
    Ok(())
}
