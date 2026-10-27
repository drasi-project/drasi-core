// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

use super::*;
use anyhow::{Context, Result};
use async_trait::async_trait;
use drasi_core::computation::InMemoryComputationProvider;
use drasi_lib::{computation::v1::*, DrasiLib};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::{mpsc, Semaphore};

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
        BoundedPipeConfig { capacity: 16 }.capabilities()
    }
    fn create(&self) -> std::result::Result<ProvidedPipe, PipeError> {
        let provided = BoundedPipeConfig { capacity: 16 }.create()?;
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

fn id(value: &str) -> ComponentId {
    ComponentId::try_new(value).expect("id")
}
fn procedure_inputs(outputs: &[&str]) -> Vec<drasi_lib::computation::v1::InputBinding> {
    outputs
        .iter()
        .map(|name| drasi_lib::computation::v1::InputBinding {
            input_port: PortId::try_new("in").unwrap(),
            output: drasi_lib::computation::v1::OutputEndpoint::new(
                id(name),
                PortId::try_new("out").unwrap(),
            ),
            stream: stream(&format!("{name}/out")),
        })
        .collect()
}

fn stream(value: &str) -> StreamId {
    StreamId::try_new(value).expect("stream")
}
fn endpoint(value: &str, port: &str) -> Endpoint {
    Endpoint::new(id(value), PortId::try_new(port).expect("port"))
}

struct Sink {
    descriptor: ComponentDescriptor,
    outputs: mpsc::Sender<ChangeEnvelope>,
}

struct Procedures(drasi_mysql_common::storedproc::runtime::MySqlStoredProcSink);
#[async_trait]
impl ComputationComponent for Procedures {
    fn descriptor(&self) -> &ComponentDescriptor {
        self.0.descriptor()
    }
    async fn start(&mut self) -> Result<()> {
        self.0.start().await
    }
    async fn stop(&mut self) -> Result<()> {
        self.0.stop().await
    }
}
#[async_trait]
impl EnvelopeSink for Procedures {
    fn completion(&self) -> SinkCompletion {
        self.0.completion()
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.0.handle(input).await
    }
}
#[async_trait]
impl ComputationComponent for Sink {
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
impl EnvelopeSink for Sink {
    fn completion(&self) -> SinkCompletion {
        SinkCompletion::Handled
    }
    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.outputs.send(input.envelope).await?;
        Ok(())
    }
}

fn values(results: &QueryResults) -> Result<Vec<Value>> {
    results
        .snapshot()?
        .rows
        .values()
        .map(|row| {
            Ok(QueryChangeCodec::row_values_to_json(
                &QueryChangeCodec::decode_row(row)?.values,
            ))
        })
        .collect()
}

async fn memory_rows() -> Result<()> {
    let container = Mysql::default()
        .with_env_var("MYSQL_DATABASE", "test")
        .with_env_var("MYSQL_ROOT_PASSWORD", "root")
        .with_cmd([
            "--log-bin=mysql-bin",
            "--binlog-format=ROW",
            "--binlog-row-image=FULL",
            "--binlog-row-metadata=FULL",
            "--server-id=1",
        ])
        .start()
        .await?;
    let mut settings = config(
        container.get_host_port_ipv4(3306).await?,
        MySqlOutput::Changes,
    );
    settings.replay = Some(MySqlRetention::Server);
    settings.start = MySqlStartPosition::Snapshot {
        lock_timeout_ms: NonZeroU64::new(5000).unwrap(),
    };
    let mut admin = connection::Connection::connect(
        &settings.connection,
        settings.max_protocol_bytes.get(),
        settings.timeout(),
    )
    .await?;
    admin
        .execute("CREATE TABLE items(id INT PRIMARY KEY,value VARCHAR(80) NOT NULL)")
        .await?;
    admin
        .execute("INSERT INTO items VALUES(1,'initial')")
        .await?;
    admin
        .execute(
            "CREATE TABLE effects(query_id VARCHAR(64),id INT,value TEXT,PRIMARY KEY(query_id,id))",
        )
        .await?;
    admin
        .execute("CREATE TABLE audit(query_id VARCHAR(64),operation VARCHAR(16),id INT)")
        .await?;
    admin.execute("CREATE PROCEDURE apply_effect(IN q VARCHAR(64),IN op VARCHAR(16),IN i INT,IN v TEXT) BEGIN \
        INSERT INTO audit VALUES(q,op,i); \
        IF op='DELETE' THEN DELETE FROM effects WHERE query_id=q AND id=i; \
        ELSE INSERT INTO effects VALUES(q,i,v) ON DUPLICATE KEY UPDATE value=v; END IF; END").await?;
    admin.execute("CREATE PROCEDURE wait_effect(IN q VARCHAR(64)) BEGIN DO SLEEP(1); INSERT INTO audit VALUES(q,'wait',900); END").await?;
    admin.execute("CREATE PROCEDURE fail_effect(IN q VARCHAR(64)) BEGIN INSERT INTO audit VALUES(q,'failed',901); SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT='expected failure'; END").await?;
    let owners = ["left", "right"].map(|name| {
        Arc::new(QuerySourceProgress::new(ComponentBatch::GRAPH_ID, id(name)).unwrap())
    });
    let consumers = Arc::new(ConsumerProgressSet::new(
        ComponentBatch::GRAPH_ID,
        id("source"),
        owners.iter().cloned(),
    )?);
    let source = MySqlSource::shared_rows(
        id("source"),
        stream("changes"),
        settings.clone(),
        consumers.clone(),
        false,
    )?;
    let mut builder = ComponentBatch::builder()
        .source(Box::new(source))
        .bind_stream(endpoint("source", "out"), stream("changes"));
    builder = builder.sink(Box::new(Procedures(
        drasi_mysql_common::storedproc::runtime::MySqlStoredProcSink::new(id("procedures"),
            serde_json::from_value(json!({
                "hostname":"127.0.0.1","port":settings.connection.port,
                "database":"test","user":"root","password":"root",
                "default_template":{
                    "added":{"template":"CALL apply_effect({{param output.alias}},{{param operation}},{{param after.id}},{{param after.value}})"},
                    "updated":{"template":"CALL apply_effect({{param output.alias}},{{param operation}},{{param after.id}},{{param after.value}})"},
                    "deleted":{"template":"CALL apply_effect({{param output.alias}},{{param operation}},{{param before.id}},NULL)"}
                }
            }))?,
            &procedure_inputs(&["left", "right"]))?
    )));
    let mut results = Vec::new();
    let mut outputs = Vec::new();
    let right_gate = Arc::new(Semaphore::new(1));
    for owner in &owners {
        let name = owner.component_id().as_str();
        let snapshot = Arc::new(MySqlSnapshot::for_row_consumer(
            id("source"),
            stream("changes"),
            settings.clone(),
            &consumers,
            owner.component_id(),
        )?);
        let query = ContinuousQueryTransformer::new_configured(
            ContinuousQueryDefinition {
                graph_id: ComponentBatch::GRAPH_ID.into(),
                id: owner.component_id().clone(),
                query: "MATCH (n:items) RETURN n.id AS id,n.value AS value".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: stream(&format!("{name}/out")),
                outbox_capacity: NonZeroUsize::new(32).unwrap(),
            },
            Arc::new(InMemoryComputationProvider),
            QueryOptions::default(),
            QueryExecutionSettings::default(),
            None,
        )
        .await?
        .with_source_progress(owner.clone())?
        .with_bootstrap(snapshot);
        results.push(query.results());
        let (sender, receiver) = mpsc::channel(64);
        outputs.push(receiver);
        let sink = format!("{name}-sink");
        builder = builder
            .query(Box::new(query))
            .sink(Box::new(Sink {
                descriptor: ComponentDescriptor::try_new(
                    id(&sink),
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                outputs: sender,
            }))
            .bind_stream(endpoint(name, "out"), stream(&format!("{name}/out")))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                if name == "right" {
                    Box::new(GateProvider(right_gate.clone())) as Box<dyn PipeProvider>
                } else {
                    Box::new(BoundedPipeConfig { capacity: 16 })
                },
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint(&sink, "in")),
                Box::new(BoundedPipeConfig { capacity: 4 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint("procedures", "in")),
                Box::new(BoundedPipeConfig { capacity: 16 }),
            );
    }
    let core = DrasiLib::builder()
        .with_id("native-mysql-rows")
        .with_components(builder.build()?)
        .build()
        .await?;
    let proof = async {
        tokio::time::timeout(Duration::from_secs(20), core.start()).await??;
        for result in &results {
            assert_eq!(values(result)?, [json!({"id":1,"value":"initial"})]);
        }
        admin.execute("BEGIN").await?;
        admin
            .execute("UPDATE items SET value='updated' WHERE id=1")
            .await?;
        admin
            .execute("INSERT INTO items VALUES(2,'second'),(3,'third')")
            .await?;
        admin.execute("DELETE FROM items WHERE id=3").await?;
        admin.execute("COMMIT").await?;
        let mut actual_input = None;
        for _ in 0..4 {
            let input = tokio::time::timeout(Duration::from_secs(10), outputs[0].recv())
                .await?
                .context("left output")?;
            if actual_input.is_none() && input.changes().operations().len() == 1 { actual_input = Some(input); }
        }
        tokio::time::timeout(Duration::from_secs(10), outputs[1].recv())
            .await?
            .context("right first output")?;
        let key = SourceProgressKey::Source("source".into());
        let partial = drasi_mysql_common::native::rows::RowPosition::decode(
            owners[1].snapshot().checkpoints[&key]
                .source_position
                .as_deref()
                .context("partial cursor")?,
        )?;
        assert_eq!(partial.offset, Some(0));
        assert!(!partial.completed());
        let previous = owners[0].snapshot().checkpoints[&key].sequence;
        tokio::time::timeout(
            Duration::from_secs(10),
            core.computation_component("source")?.stop(),
        )
        .await??;
        tokio::time::timeout(
            Duration::from_secs(10),
            core.computation_component("source")?.start(),
        )
        .await??;
        right_gate.add_permits(128);
        for _ in 0..3 {
            tokio::time::timeout(Duration::from_secs(10), outputs[1].recv())
                .await?
                .context("right remaining output")?;
        }
        for output in &mut outputs {
            assert!(
                tokio::time::timeout(Duration::from_millis(100), output.recv())
                    .await
                    .is_err(),
                "queued or replayed covered rows were reapplied"
            );
        }
        for result in &results {
            let mut actual = values(result)?;
            actual.sort_by_key(Value::to_string);
            assert_eq!(
                actual,
                [
                    json!({"id":1,"value":"updated"}),
                    json!({"id":2,"value":"second"})
                ]
            );
        }
        for owner in &owners {
            let state = owner.snapshot();
            assert!(state.bootstrap_complete && !state.persistent);
            let position = drasi_mysql_common::native::rows::RowPosition::decode(
                state.checkpoints[&SourceProgressKey::Source("source".into())]
                    .source_position
                    .as_deref()
                    .context("row cursor")?,
            )?;
            assert_eq!(position.offset, Some(3));
            assert_eq!(position.count, 4);
            assert!(position.completed());
        }
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let rows = admin.query("SELECT COUNT(*) FROM audit").await?;
                if catalog::field(&rows.values[0], 0)? == "8" {
                    return Result::<()>::Ok(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await??;
        assert_eq!(
            admin
                .query("SELECT query_id,id,value FROM effects ORDER BY query_id,id")
                .await?
                .values
                .len(),
            4
        );
        admin
            .execute("UPDATE items SET value='fresh' WHERE id=2")
            .await?;
        for output in &mut outputs {
            tokio::time::timeout(Duration::from_secs(10), output.recv())
                .await?
                .context("fresh row")?;
        }
        for owner in &owners {
            assert!(
                owner.snapshot().checkpoints[&key].sequence > previous,
                "row sequence must exceed every prior consumer and source admission"
            );
        }
        procedure_lifecycle(&mut admin, settings.connection.port, actual_input.context("actual query input")?).await?;
        admin.execute("BEGIN").await?;
        admin.execute("DELETE FROM items").await?;
        admin.execute("ROLLBACK").await?;
        core.stop().await?;
        admin
            .execute("UPDATE items SET value='offline' WHERE id=1")
            .await?;
        tokio::time::timeout(Duration::from_secs(20), core.start()).await??;
        for result in &results {
            assert!(values(result)?.contains(&json!({"id":1,"value":"offline"})));
            assert_eq!(values(result)?.len(), 2);
        }

        async fn procedure_lifecycle(admin: &mut connection::Connection, port: u16, envelope: ChangeEnvelope) -> Result<()> {
            use drasi_mysql_common::storedproc::runtime::MySqlStoredProcSink;
            let config = |template: &str, stop_ms: u64| serde_json::from_value(json!({
                "hostname":"127.0.0.1","port":port,"database":"test","user":"root","password":"root",
                "command_timeout_ms":5000,"shutdown_timeout_ms":stop_ms,
                "default_template":{
                    "added":{"template":template},"updated":{"template":template},"deleted":{"template":template}
                }
            }));
            let input = || InputEnvelope { port:PortId::try_new("in").unwrap(), envelope:envelope.clone() };
            let mut sink = MySqlStoredProcSink::new(id("owned-call"),config("CALL wait_effect({{param output.alias}})",50)?,&procedure_inputs(&["left","right"]))?;
            sink.start().await?;
            let mut handle = Box::pin(sink.handle(input()));
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    tokio::select! {
                        result = &mut handle => { result?; anyhow::bail!("held procedure completed too early"); }
                        _ = tokio::time::sleep(Duration::from_millis(10)) => {
                            let state = admin.query("SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE (INFO LIKE '%wait_effect%' OR STATE='User sleep') AND ID<>CONNECTION_ID()").await?;
                            if catalog::field(&state.values[0],0)? != "0" { return Result::<()>::Ok(()); }
                        }
                    }
                }
            }).await??;
            drop(handle);
            assert!(sink.handle(input()).await.is_err(), "interrupted work must block replacement input");
            assert!(sink.stop().await.is_err(), "short cleanup timeout must retain the operation");
            assert!(sink.start().await.is_err(), "cancelled cleanup must not permit restart");
            tokio::time::sleep(Duration::from_millis(1100)).await;
            sink.stop().await?;
            let count = admin.query("SELECT COUNT(*) FROM audit WHERE operation='wait'").await?;
            assert_eq!(catalog::field(&count.values[0],0)?,"1");
            let mut failing = MySqlStoredProcSink::new(id("failed-call"),config("CALL fail_effect({{param output.alias}})",5000)?,&procedure_inputs(&["left","right"]))?;
            failing.start().await?;
            assert!(failing.handle(input()).await.is_err());
            assert!(failing.handle(input()).await.is_err(), "failed submitted calls are not retried automatically");
            failing.stop().await?;
            let count = admin.query("SELECT COUNT(*) FROM audit WHERE operation='failed'").await?;
            assert_eq!(catalog::field(&count.values[0],0)?,"1");
            Ok(())
        }
        Result::<()>::Ok(())
    }
    .await;
    right_gate.close();
    let cleanup = core.shutdown().await.map_err(anyhow::Error::from);
    let admin_cleanup = admin.close().await;
    let failures: Vec<_> = [proof, cleanup, admin_cleanup]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(drasi_lib::error::OperationFailures::new("MySQL row proof failed", failures).into())
    }
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires isolated Podman MySQL"]
async fn native_memory_rows_current_thread() -> Result<()> {
    memory_rows().await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated Podman MySQL"]
async fn native_memory_rows_multi_thread() -> Result<()> {
    memory_rows().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires isolated Podman MySQL"]
async fn native_rows_preserve_logged_myisam_changes_when_innodb_rolls_back() -> Result<()> {
    for tracked in [false, true] {
        myisam_rows(tracked).await?;
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires isolated Podman MySQL"]
async fn native_rows_preserve_logged_myisam_changes_multi_thread() -> Result<()> {
    for tracked in [false, true] {
        myisam_rows(tracked).await?;
    }
    Ok(())
}

async fn myisam_rows(tracked: bool) -> Result<()> {
    let container = Mysql::default()
        .with_env_var("MYSQL_DATABASE", "test")
        .with_env_var("MYSQL_ROOT_PASSWORD", "root")
        .with_cmd([
            "--log-bin=mysql-bin",
            "--binlog-format=ROW",
            "--binlog-row-image=FULL",
            "--binlog-row-metadata=FULL",
            "--server-id=1",
        ])
        .start()
        .await?;
    let mut settings = config(
        container.get_host_port_ipv4(3306).await?,
        MySqlOutput::Changes,
    );
    settings.replay = tracked.then_some(MySqlRetention::Server);
    settings.tables = vec!["items".into(), "tx".into()];
    let mut admin = connection::Connection::connect(
        &settings.connection,
        settings.max_protocol_bytes.get(),
        settings.timeout(),
    )
    .await?;
    admin
        .execute("CREATE TABLE items(id INT PRIMARY KEY,value VARCHAR(80) NOT NULL) ENGINE=MyISAM")
        .await?;
    admin
        .execute("CREATE TABLE tx(id INT PRIMARY KEY,value VARCHAR(80) NOT NULL) ENGINE=InnoDB")
        .await?;
    let owners = ["left", "right"].map(|name| {
        Arc::new(QuerySourceProgress::new(ComponentBatch::GRAPH_ID, id(name)).unwrap())
    });
    let consumers = Arc::new(ConsumerProgressSet::new(
        ComponentBatch::GRAPH_ID,
        id("source"),
        owners.iter().cloned(),
    )?);
    let source = if tracked {
        MySqlSource::shared_rows(id("source"), stream("changes"), settings, consumers, false)?
    } else {
        MySqlSource::new(id("source"), stream("changes"), settings, None)?
    };
    let mut builder = ComponentBatch::builder()
        .source(Box::new(source))
        .bind_stream(endpoint("source", "out"), stream("changes"));
    let mut results = Vec::new();
    let mut outputs = Vec::new();
    for owner in &owners {
        let name = owner.component_id().as_str();
        let query = ContinuousQueryTransformer::new_configured(
            ContinuousQueryDefinition {
                graph_id: ComponentBatch::GRAPH_ID.into(),
                id: owner.component_id().clone(),
                query: "MATCH (n) RETURN n.id AS id,n.value AS value".into(),
                language: ComputationQueryLanguage::Cypher,
                output_stream: stream(&format!("{name}/out")),
                outbox_capacity: NonZeroUsize::new(32).unwrap(),
            },
            Arc::new(InMemoryComputationProvider),
            QueryOptions::default(),
            QueryExecutionSettings::default(),
            None,
        )
        .await?
        .with_source_progress(owner.clone())?;
        results.push(query.results());
        let (sender, receiver) = mpsc::channel(32);
        outputs.push(receiver);
        let sink = format!("{name}-sink");
        builder = builder
            .query(Box::new(query))
            .sink(Box::new(Sink {
                descriptor: ComponentDescriptor::try_new(
                    id(&sink),
                    vec![PortDescriptor::new(
                        PortId::try_new("in")?,
                        PortDirection::Input,
                        QueryChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?,
                outputs: sender,
            }))
            .bind_stream(endpoint(name, "out"), stream(&format!("{name}/out")))
            .connect(
                EdgeDefinition::new(endpoint("source", "out"), endpoint(name, "in")),
                Box::new(BoundedPipeConfig { capacity: 4 }),
            )
            .connect(
                EdgeDefinition::new(endpoint(name, "out"), endpoint(&sink, "in")),
                Box::new(BoundedPipeConfig { capacity: 4 }),
            );
    }
    let core = DrasiLib::builder()
        .with_id("mysql-nontransactional-rows")
        .with_components(builder.build()?)
        .build()
        .await?;
    let proof = async {
        core.start().await?;
        admin.execute("BEGIN").await?;
        admin
            .execute("INSERT INTO tx VALUES(1,'must roll back')")
            .await?;
        admin
            .execute("INSERT INTO items VALUES(1,'retained'),(2,'also retained')")
            .await?;
        admin.execute("ROLLBACK").await?;
        for output in &mut outputs {
            for _ in 0..2 {
                tokio::time::timeout(Duration::from_secs(10), output.recv())
                    .await?
                    .context("logged nontransactional row")?;
            }
            assert!(
                tokio::time::timeout(Duration::from_millis(100), output.recv())
                    .await
                    .is_err()
            );
        }
        for result in &results {
            let mut rows = values(result)?;
            rows.sort_by_key(Value::to_string);
            assert_eq!(
                rows,
                [
                    json!({"id":1,"value":"retained"}),
                    json!({"id":2,"value":"also retained"})
                ]
            );
        }
        Result::<()>::Ok(())
    }
    .await;
    let cleanup = core.shutdown().await.map_err(anyhow::Error::from);
    let connection_cleanup = admin.close().await;
    let failures: Vec<_> = [proof, cleanup, connection_cleanup]
        .into_iter()
        .filter_map(Result::err)
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(drasi_lib::error::OperationFailures::new(
            "MySQL nontransactional row proof failed",
            failures,
        )
        .into())
    }
}
