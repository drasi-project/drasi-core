// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Live proofs of the native MySQL stored-procedure consumer: destination
//! cursor, large inputs, replay without duplicate effects, reconnect, Erase
//! and the per-owner stream capacity. Requires Podman.

use super::*;
use drasi_computation_plugin_sdk::{Component, ControlSender, CreateRequest, Factory};
use drasi_core::interface::FailureMode;
use drasi_core_types::models::{
    Element, ElementMetadata, ElementPropertyMap, ElementReference, SourceChange,
};

const CALL: &str =
    "CALL effect({{param output.alias}},{{param operation}},{{param after.id}},{{param after.value}})";

pub(crate) fn delivery_options() -> DeliveryOptions {
    DeliveryOptions {
        scope: RecoveryScope::Failure(FailureMode::ProcessRestart),
        max_streams: NonZeroUsize::new(4).expect("streams"),
        receipts_per_stream: NonZeroUsize::new(4).expect("receipts"),
    }
}

fn rocks(path: &Path) -> Result<Arc<dyn ComputationIndexProvider>> {
    Ok(Arc::new(RocksDbComputationProvider::new(
        path,
        RocksIndexOptions::new(
            false,
            false,
            RocksDbMemoryBudget::from_total_budget_bytes(32 << 20)?,
        ),
    )))
}

/// Declares the graph-owned delivery resource every consumer sink needs.
pub(crate) fn declare_delivery(
    builder: ComponentBatchBuilder,
    path: &Path,
) -> Result<ComponentBatchBuilder> {
    Ok(builder
        .declare_resource(ResourceSpecification {
            id: resource("delivery"),
            role: ResourceRole::IndexBackend,
            ownership: ResourceOwnership::Borrowed,
            binding: "delivery".into(),
        })?
        .provide_resource(
            resource("delivery"),
            ResourceHandle::new(
                ResourceRole::IndexBackend,
                Arc::new(computation::NativeConsumerResource {
                    provider: rocks(path)?,
                    options: delivery_options(),
                }),
            ),
        )?)
}

/// The flat sink specification: literal fields, the password as a secret
/// reference to `sink-password`, and the delivery dependency.
pub(crate) fn sink_specification(
    factory: &computation::NativeFactory,
    settings: &Value,
) -> Result<ComponentSpecification> {
    let mut literal = settings.clone();
    literal
        .as_object_mut()
        .context("sink settings object")?
        .remove("password");
    let mut sink = factory.specification(id("procedures"), literal)?;
    sink.configuration.insert(
        "password".into(),
        ConfigurationValue::Reference {
            resource: resource("settings"),
            key: "sink-password".into(),
            secret: true,
        },
    );
    sink.dependencies
        .insert("consumer".into(), vec![resource("delivery")]);
    Ok(sink)
}

pub(crate) async fn consumer(
    factory: &computation::NativeFactory,
    component: &str,
    configuration: Value,
    path: &Path,
) -> Result<computation::NativeConsumerProxy> {
    let inputs = sink_inputs::inputs_for(&configuration);
    let mut consumer = factory
        .create_consumer_with_inputs(
            id(component),
            configuration,
            scope(),
            rocks(path)?,
            delivery_options(),
            inputs,
        )
        .await?;
    consumer.initialize(&crate::hooks::initialize_ctx()).await?;
    Ok(consumer)
}

fn item(id: i64, value: &str) -> SourceChange {
    SourceChange::Insert {
        element: Element::Node {
            metadata: ElementMetadata {
                reference: ElementReference::new("items", &id.to_string()),
                labels: Arc::from([Arc::from("items")]),
                effective_from: 1,
            },
            properties: ElementPropertyMap::from(json!({"id": id, "value": value})),
        },
    }
}

/// A persistent producer's input on `<producer>/out`.
pub(crate) fn input(
    producer: &str,
    sequence: u64,
    changes: &[SourceChange],
) -> Result<InputEnvelope> {
    let producer = id(producer);
    let stream_name = format!("{producer}/out");
    let identity: GraphProducerIdentity = serde_json::from_value(json!({
        "construction_scope": INSTANCE, "graph_id": ComponentBatch::GRAPH_ID,
        "component_id": producer.as_str(), "stream": stream_name,
        "incarnation": "4b0c9a59-31d6-4bd8-9f41-3f5c8e3c8a01", "persistent": true
    }))?;
    let mut envelope =
        GraphChangeCodec::encode_changes(changes, stream(&stream_name), sequence, None)?;
    GraphProducerProgress::annotate(&mut envelope, &identity, sequence)?;
    Ok(InputEnvelope { execution_context: Default::default(),
        port: PortId::try_new("in")?,
        envelope,
    })
}

fn settings(database: &Database, template: &str) -> Value {
    let mut settings = database.sink_settings();
    for event in ["added", "updated", "deleted"] {
        settings["defaultTemplate"][event]["template"] = json!(template);
    }
    settings
}

fn factory(native: &[Arc<NativePlugin>]) -> Result<Arc<computation::NativeFactory>> {
    native[2]
        .factories()
        .iter()
        .find(|factory| {
            factory.metadata().implementation.name.as_ref()
                == drasi_computation_sink_storedproc_mysql::MYSQL_STOREDPROC
        })
        .cloned()
        .context("native procedure sink")
}

fn owner(component: &str) -> String {
    drasi_computation_sink_storedproc_mysql::owner(ComponentBatch::GRAPH_ID, id(component).as_str())
}

async fn rows_of(database: &Database, component: &str) -> Result<String> {
    database
        .sql(&format!(
            "SELECT COUNT(*) FROM drasi_delivery_progress WHERE owner='{}'",
            owner(component)
        ))
        .await
}

/// Procedure lifecycle through the host consumer proxy: a cancelled CALL is
/// KILLed and rolled back by Stop, the same input then applies once, and a
/// failing procedure rolls back with its operation.
pub(crate) async fn procedure_lifecycle(
    database: &Database,
    factory: &computation::NativeFactory,
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let wait = input("left", 1, &[item(900, "held")])?;
    let mut sink = consumer(
        factory,
        "held-procedure",
        settings(database, "CALL wait_effect({{param output.alias}})"),
        &directory.path().join("held"),
    )
    .await?;
    sink.start(&crate::hooks::start_ctx()).await?;
    {
        let mut pending = Box::pin(sink.handle(wait.clone()));
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
    }
    assert!(
        sink.handle(wait.clone()).await.is_err(),
        "the host fences a cancelled delivery until Stop"
    );
    sink.stop(&StopContext::new(
        crate::hooks::scope("abort"),
        StopMode::Abort,
    ))
    .await?;
    database
        .expect_count(
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE STATE='User sleep'",
            "0",
        )
        .await?;
    assert_eq!(
        database
            .sql("SELECT COUNT(*) FROM audit WHERE operation='wait'")
            .await?,
        "0",
        "the KILLed CALL rolled back"
    );
    sink.start(&crate::hooks::start_ctx()).await?;
    sink.handle(wait.clone()).await?;
    sink.stop(&crate::hooks::stop_ctx()).await?;
    assert_eq!(
        database
            .sql("SELECT COUNT(*) FROM audit WHERE operation='wait'")
            .await?,
        "1"
    );
    let mut failed = consumer(
        factory,
        "failed-procedure",
        settings(database, "CALL fail_effect({{param output.alias}})"),
        &directory.path().join("failed"),
    )
    .await?;
    let failing = input("left", 2, &[item(901, "fails")])?;
    failed.start(&crate::hooks::start_ctx()).await?;
    assert!(failed.handle(failing.clone()).await.is_err());
    failed.stop(&crate::hooks::stop_ctx()).await?;
    failed.start(&crate::hooks::start_ctx()).await?;
    assert!(failed.handle(failing).await.is_err());
    failed.stop(&crate::hooks::stop_ctx()).await?;
    assert_eq!(
        database
            .sql("SELECT COUNT(*) FROM audit WHERE operation='failed'")
            .await?,
        "0",
        "a failing procedure rolls back with its cursor row"
    );
    Ok(())
}

/// More than 4 096 changes in one input, delivered one transaction per
/// operation with the cursor tracking the completed prefix.
async fn large_input(database: &Database, factory: &computation::NativeFactory) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let changes: Vec<_> = (1..=5000).map(|id| item(id, "bulk")).collect();
    let large = input("left", 10, &changes)?;
    let mut sink = consumer(
        factory,
        "bulk-procedures",
        settings(database, CALL),
        directory.path(),
    )
    .await?;
    sink.start(&crate::hooks::start_ctx()).await?;
    tokio::time::timeout(Duration::from_secs(600), sink.handle(large)).await??;
    assert_eq!(sink.progress().await?[0].completed_operations, 5000);
    sink.stop(&crate::hooks::stop_ctx()).await?;
    assert_eq!(
        database
            .sql("SELECT COUNT(*) FROM effects WHERE value='bulk'")
            .await?,
        "5000"
    );
    assert_eq!(
        database
            .sql(&format!(
                "SELECT operations, completed, version FROM drasi_delivery_progress WHERE owner='{}'",
                owner("bulk-procedures")
            ))
            .await?,
        "5000\t5000\t4"
    );
    Ok(())
}

/// A failure inside a batch commits the completed prefix only; the retry
/// resumes there; a lost local ledger replays without duplicate effects.
async fn partial_failure_replay(
    database: &Database,
    factory: &computation::NativeFactory,
) -> Result<()> {
    database
        .sql(
            "CREATE TABLE failure_gate(enabled BOOLEAN NOT NULL, failed_id INT NOT NULL); \
             INSERT INTO failure_gate VALUES(true, 7003);\n\
             DELIMITER //\n\
             CREATE PROCEDURE gated_effect(IN q VARCHAR(64),IN op VARCHAR(16),IN i INT,IN v TEXT) BEGIN \
             INSERT INTO audit VALUES(q,op,i); \
             IF (SELECT enabled AND failed_id=i FROM failure_gate) THEN \
             SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT='intentional failure'; END IF; \
             INSERT INTO effects VALUES(q,i,v) ON DUPLICATE KEY UPDATE value=v; END //\n\
             DELIMITER ;\n",
        )
        .await?;
    let directory = tempfile::tempdir()?;
    let template = "CALL gated_effect({{param output.alias}},{{param operation}},{{param after.id}},{{param after.value}})";
    let batch = input(
        "right",
        20,
        &(7001..=7005)
            .map(|id| item(id, "gated"))
            .collect::<Vec<_>>(),
    )?;
    let mut sink = consumer(
        factory,
        "gated-procedures",
        settings(database, template),
        &directory.path().join("ledger"),
    )
    .await?;
    sink.start(&crate::hooks::start_ctx()).await?;
    assert!(sink.handle(batch.clone()).await.is_err());
    assert_eq!(sink.progress().await?[0].completed_operations, 2);
    let audited = "SELECT COUNT(*) FROM audit WHERE id BETWEEN 7001 AND 7005";
    assert_eq!(
        database.sql(audited).await?,
        "2",
        "the failed operation rolled back"
    );
    // A failed External delivery has settled: the same input is retried
    // through the host proxy without Stop/Start and resumes at the failed
    // operation.
    database
        .sql("UPDATE failure_gate SET enabled=false")
        .await?;
    sink.handle(batch.clone()).await?;
    sink.stop(&crate::hooks::stop_ctx()).await?;
    assert_eq!(database.sql(audited).await?, "5");
    // A fresh local ledger models a crash after the destination commit but
    // before the host recorded completion: the cursor skips every effect.
    let mut replay = consumer(
        factory,
        "gated-procedures",
        settings(database, template),
        &directory.path().join("lost-ledger"),
    )
    .await?;
    replay.start(&crate::hooks::start_ctx()).await?;
    replay.handle(batch).await?;
    replay.stop(&crate::hooks::stop_ctx()).await?;
    assert_eq!(database.sql(audited).await?, "5", "no effect repeated");
    Ok(())
}

/// The server kills the idle activation session; the next delivery
/// reconnects inside the same activation.
async fn reconnect_after_kill(
    database: &Database,
    factory: &computation::NativeFactory,
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut sink = consumer(
        factory,
        "killed-procedures",
        settings(database, CALL),
        directory.path(),
    )
    .await?;
    sink.start(&crate::hooks::start_ctx()).await?;
    sink.handle(input("left", 30, &[item(8001, "before-kill")])?)
        .await?;
    let sessions = database
        .sql("SELECT GROUP_CONCAT(ID) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID() AND COMMAND='Sleep'")
        .await?;
    anyhow::ensure!(
        sessions != "NULL" && !sessions.is_empty(),
        "sink session not found"
    );
    for session in sessions.split(',') {
        database.sql(&format!("KILL {session}")).await?;
    }
    sink.handle(input("left", 31, &[item(8002, "after-kill")])?)
        .await?;
    sink.stop(&crate::hooks::stop_ctx()).await?;
    assert_eq!(
        database
            .sql("SELECT COUNT(*) FROM effects WHERE id IN (8001,8002)")
            .await?,
        "2"
    );
    Ok(())
}

/// Erase removes only this owner's rows and is idempotent.
async fn erase_twice(database: &Database, factory: &computation::NativeFactory) -> Result<()> {
    let directory = tempfile::tempdir()?;
    for (component, sequence) in [("erased-procedures", 40), ("kept-procedures", 41)] {
        let mut sink = consumer(
            factory,
            component,
            settings(database, CALL),
            &directory.path().join(component),
        )
        .await?;
        sink.start(&crate::hooks::start_ctx()).await?;
        sink.handle(input(
            "left",
            sequence,
            &[item(9000 + sequence as i64, "erase")],
        )?)
        .await?;
        sink.stop(&crate::hooks::stop_ctx()).await?;
        assert_eq!(rows_of(database, component).await?, "1");
        if component == "erased-procedures" {
            sink.dispose(&crate::hooks::erase_ctx()).await?;
        } else {
            sink.dispose(&crate::hooks::dispose_ctx()).await?;
        }
    }
    assert_eq!(rows_of(database, "erased-procedures").await?, "0");
    let mut again = consumer(
        factory,
        "erased-procedures",
        settings(database, CALL),
        &directory.path().join("again"),
    )
    .await?;
    again.dispose(&crate::hooks::erase_ctx()).await?;
    assert_eq!(rows_of(database, "erased-procedures").await?, "0");
    assert_eq!(rows_of(database, "kept-procedures").await?, "1");
    Ok(())
}

/// `maxStreams` bounds the streams one owner may hold.
async fn stream_capacity(database: &Database, factory: &computation::NativeFactory) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let mut configuration = settings(database, CALL);
    configuration["maxStreams"] = json!(1);
    let mut sink = consumer(
        factory,
        "capped-procedures",
        configuration,
        directory.path(),
    )
    .await?;
    sink.start(&crate::hooks::start_ctx()).await?;
    sink.handle(input("left", 50, &[item(9501, "cap")])?)
        .await?;
    let error = sink
        .handle(input("right", 51, &[item(9502, "cap")])?)
        .await
        .expect_err("a second stream exceeds maxStreams");
    assert!(format!("{error:#}").contains("maxStreams"), "{error:#}");
    sink.stop(&crate::hooks::stop_ctx()).await?;
    assert_eq!(rows_of(database, "capped-procedures").await?, "1");
    Ok(())
}

/// The plugin itself recovers from a failed delivery without Stop/Start,
/// driven directly by a `DeliveryRunner` without the host proxy.
async fn plugin_retry_without_restart(database: &Database) -> Result<()> {
    database
        .sql("CREATE TABLE retry_gate(enabled BOOLEAN NOT NULL); INSERT INTO retry_gate VALUES(true);\n\
             DELIMITER //\n\
             CREATE PROCEDURE retry_effect(IN q VARCHAR(64),IN i INT) BEGIN \
             IF (SELECT enabled FROM retry_gate) THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT='not yet'; END IF; \
             INSERT INTO effects VALUES(q,i,'retried'); END //\n\
             DELIMITER ;\n")
        .await?;
    let configuration = settings(
        database,
        "CALL retry_effect({{param output.alias}},{{param after.id}})",
    );
    let factory = drasi_computation_sink_storedproc_mysql::MySqlStoredProcFactory;
    let request = CreateRequest {
        inputs: sink_inputs::inputs_for(&configuration),
        id: id("retry-procedures"),
        implementation: factory.metadata().implementation,
        configuration_version: 1,
        configuration,
        scope: Some(scope()),
        activation_credentials_bound: false,
        update_directive: None,
    };
    let Component::Consumer(mut sink) = factory
        .create(
            &request,
            ControlSender::default(),
            drasi_computation_plugin_sdk::ServiceBindings::none(),
        )?
        .component
    else {
        anyhow::bail!("expected the consumer");
    };
    let delivery_scope = DeliveryScope::new(
        INSTANCE.into(),
        ComponentBatch::GRAPH_ID.into(),
        id("retry-procedures"),
    )?;
    let mut codec = EnvelopeCodec::new(NonZeroUsize::new(64 << 20).expect("bound"));
    codec.register_schema(GraphChangeCodec::schema())?;
    let batch = DeliveryBatch::new(
        delivery_scope,
        input("left", 60, &[item(9601, "retry")])?,
        &codec,
    )?;
    use drasi_computation_plugin_sdk::lifecycle::{
        CancellationToken, OperationScope, StartContext, StopContext as SdkStop,
        StopMode as SdkMode,
    };
    let scope = || {
        OperationScope::new(
            "test",
            "retry-procedures",
            1,
            None,
            CancellationToken::new(),
        )
    };
    sink.start(&mut StartContext::new(scope(), None)).await?;
    let first = sink.handle(batch.item(0)?).await;
    assert!(first.is_err());
    assert_eq!(
        drasi_computation_contracts::delivery_failure_class(first.as_ref().expect_err("failed")),
        Some(drasi_computation_contracts::DeliveryFailureClass::Permanent),
        "a procedure SIGNAL is permanent"
    );
    database.sql("UPDATE retry_gate SET enabled=false").await?;
    sink.handle(batch.item(0)?).await?;
    sink.handle(batch.item(0)?).await?;
    sink.stop(&SdkStop::new(scope(), SdkMode::Drain)).await?;
    assert_eq!(
        database
            .sql("SELECT COUNT(*) FROM effects WHERE id=9601")
            .await?,
        "1",
        "the retried operation applied once"
    );
    Ok(())
}

/// One live output of an in-memory (volatile) query: its recovery identity
/// carries a per-run incarnation and it has no persistent progress.
fn volatile_input(incarnation: &str, sequence: u64, id: i64, value: &str) -> Result<InputEnvelope> {
    use drasi_core_types::evaluation::{
        context::QueryPartEvaluationContext, variable_value::VariableValue,
    };
    let query = self::id("left");
    let mut envelope = QueryChangeCodec::encode_evaluation(
        None,
        &query,
        SystemMetadata::new(stream(&format!("{query}/out")), sequence),
        &ResultNodeProjector::try_new("test/native-mysql-volatile/out", None)?,
        &[QueryPartEvaluationContext::Adding {
            after: [
                ("id".into(), VariableValue::Integer(id.into())),
                ("value".into(), VariableValue::from(value)),
            ]
            .into(),
            row_signature: id as u64,
        }],
        QueryOutputMetadata {
            query_id: query.clone(),
            input_origin: None,
            timestamp: chrono::DateTime::from_timestamp(1_700_000_000, 0).context("timestamp")?,
            metadata: Default::default(),
            profiling: None,
        },
    )?
    .context("volatile query output")?;
    let identity: QueryRecoveryIdentity = serde_json::from_value(json!({
        "construction_scope": INSTANCE, "graph_id": ComponentBatch::GRAPH_ID,
        "query_id": query.as_str(), "configuration_hash": 7, "incarnation": incarnation
    }))?;
    identity.annotate(&mut envelope, &query)?;
    Ok(InputEnvelope { execution_context: Default::default(),
        port: PortId::try_new("in")?,
        envelope,
    })
}

/// An in-memory query is refused without opt-in and delivered at least once
/// (deduplicated within one producer run) with `deliveryGuarantee:
/// atLeastOnce`; a new run starts a fresh cursor on the same stream row.
async fn volatile_queries(
    database: &Database,
    factory: &Arc<computation::NativeFactory>,
) -> Result<()> {
    const FIRST: &str = "6d1c2f0e-0d8b-4f43-9a5e-2a3b8c1d0e01";
    const SECOND: &str = "6d1c2f0e-0d8b-4f43-9a5e-2a3b8c1d0e02";
    let directory = tempfile::tempdir()?;
    let audited = "SELECT COUNT(*) FROM audit WHERE id BETWEEN 9701 AND 9703";
    let mut strict = consumer(
        factory,
        "strict-procedures",
        settings(database, CALL),
        &directory.path().join("strict"),
    )
    .await?;
    strict.start(&crate::hooks::start_ctx()).await?;
    // The graph refuses this pairing when it binds the edge (checked by
    // `graph_refuses_volatile_queries`); driven directly, the host refuses
    // the envelope as a permanent typed failure instead of degrading it.
    let error = strict
        .handle(volatile_input(FIRST, 1, 9701, "refused")?)
        .await
        .err()
        .context("the volatile input must be refused")?;
    assert_eq!(
        drasi_computation_contracts::delivery_failure_class(&error),
        Some(drasi_computation_contracts::DeliveryFailureClass::Permanent)
    );
    assert!(
        format!("{error:#}").contains("lacks the producer identity that ExactlyOnce"),
        "{error:#}"
    );
    assert_eq!(database.sql(audited).await?, "0");
    strict.stop(&crate::hooks::stop_ctx()).await?;

    let mut configuration = settings(database, CALL);
    configuration["deliveryGuarantee"] = json!("atLeastOnce");
    let mut sink = consumer(
        factory,
        "volatile-procedures",
        configuration,
        &directory.path().join("volatile"),
    )
    .await?;
    sink.start(&crate::hooks::start_ctx()).await?;
    let first = volatile_input(FIRST, 1, 9701, "first")?;
    sink.handle(first.clone()).await?;
    sink.handle(first).await?;
    sink.handle(volatile_input(FIRST, 2, 9702, "second")?)
        .await?;
    assert_eq!(
        database.sql(audited).await?,
        "2",
        "deduplicated within a run"
    );
    // A restarted producer reuses low sequences under a new epoch.
    sink.handle(volatile_input(SECOND, 1, 9703, "restarted")?)
        .await?;
    assert_eq!(database.sql(audited).await?, "3");
    assert_eq!(rows_of(database, "volatile-procedures").await?, "1");
    assert_eq!(
        database
            .sql(&format!(
                "SELECT LENGTH(epoch), version FROM drasi_delivery_progress WHERE owner='{}'",
                owner("volatile-procedures")
            ))
            .await?,
        "64\t4"
    );
    sink.stop(&crate::hooks::stop_ctx()).await?;
    graph_refuses_volatile_queries(database, factory).await?;
    Ok(())
}

/// An idle source feeding the in-memory query, so the query is constructed
/// and its outgoing edge is compared.
struct IdleFeed(ComponentDescriptor);
#[async_trait]
impl ComputationComponent for IdleFeed {
    fn description(&self) -> drasi_lib::computation::v1::ComponentDescription {
        drasi_lib::computation::v1::ComponentDescription::new(
            drasi_lib::computation::v1::ProgressPersistence::Stateless,
        )
        .with_outputs(
            &self.0,
            drasi_lib::computation::v1::OutputDescription::UNTRACKED,
        )
    }
    fn descriptor(&self) -> &ComponentDescriptor {
        &self.0
    }
    drasi_lib::passive_component_lifecycle!();
}
#[async_trait]
impl EnvelopeSource for IdleFeed {
    async fn next(&mut self) -> Result<Option<OutputEnvelope>> {
        std::future::pending().await
    }
}

/// The graph refuses, when it binds the edge, to connect an exactly-once
/// procedure sink to a query on an in-memory index provider: the edge fails
/// and the sink node carries the terminal binding failure while both nodes
/// stay in the graph. With `deliveryGuarantee: atLeastOnce` the edge binds.
async fn graph_refuses_volatile_queries(
    database: &Database,
    factory: &Arc<computation::NativeFactory>,
) -> Result<()> {
    let directory = tempfile::tempdir()?;
    let build = |mut configuration: Value| -> Result<ComputationGraph> {
        // The sink's output aliases select this graph's query.
        configuration["outputs"] = json!({
            "query": {"component": database::fixture_id("graph-query"), "port": "out"}
        });
        let queries = Arc::new(ContinuousQueryFactory::default());
        let mut query = queries.specification_from_config(
            id("graph-query"),
            json!({"query": "MATCH (n:items) RETURN n.id AS id", "stream": "graph-query/out"}),
        )?;
        query
            .dependencies
            .insert("indexes".into(), vec![resource("memory")]);
        // The password is a secret reference, as in the other graph proofs.
        let sink = sink_specification(factory, &configuration)?;
        let edge =
            EdgeDefinition::new(endpoint("graph-query", "out"), endpoint("procedures", "in"));
        let builder = ComputationGraph::builder(ComponentBatch::GRAPH_ID)
            .declare_resource(ResourceSpecification {
                id: resource("memory"),
                role: ResourceRole::IndexBackend,
                ownership: ResourceOwnership::Borrowed,
                binding: "memory".into(),
            })?
            .provide_resource(
                resource("memory"),
                drasi_lib::computation::v1::QueryIndexProviderResource::handle(
                    ResourceRole::IndexBackend,
                    Arc::new(InMemoryComputationProvider),
                ),
            )?
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
                        source: database.source_settings(),
                        sink: database.sink_settings(),
                    }))),
                ),
            )?
            .declare_resource(ResourceSpecification {
                id: resource("delivery"),
                role: ResourceRole::IndexBackend,
                ownership: ResourceOwnership::Borrowed,
                binding: "delivery".into(),
            })?
            .provide_resource(
                resource("delivery"),
                ResourceHandle::new(
                    ResourceRole::IndexBackend,
                    Arc::new(computation::NativeConsumerResource {
                        provider: rocks(&directory.path().join("delivery"))?,
                        options: delivery_options(),
                    }),
                ),
            )?
            .source(
                name("graph-feed"),
                Box::new(IdleFeed(ComponentDescriptor::try_new(
                    id("graph-feed"),
                    vec![PortDescriptor::new(
                        PortId::try_new("out")?,
                        PortDirection::Output,
                        GraphChangeCodec::schema().descriptor().clone(),
                        PipeRequirements::default(),
                    )],
                )?)),
            )
            .bind_stream(endpoint("graph-feed", "out"), stream("graph-feed/out"))
            .connect(
                EdgeDefinition::new(endpoint("graph-feed", "out"), endpoint("graph-query", "in")),
                Box::new(BoundedPipeConfig { capacity: 4 }),
            )
            .component(name("graph-query"), query, queries)
            .component(name("procedures"), sink, factory.clone())
            .bind_stream(edge.from.clone(), stream("graph-query/out"))
            .connect(edge, Box::new(BoundedPipeConfig { capacity: 4 }));
        Ok(builder.build()?)
    };
    let edge = EdgeDefinition::new(endpoint("graph-query", "out"), endpoint("procedures", "in"));
    let deployed = |mut graph: ComputationGraph, edge: EdgeDefinition| async move {
        let watched = edge.clone();
        let run = graph.run()?;
        let control = run.control();
        let (outcome, observed) = tokio::join!(run, async {
            control.deployment_report().await?;
            // Deferred construction binds the edge after deployment; wait
            // until it is either bound or failed.
            let mut changes = control.subscribe_observed();
            let settled = tokio::time::timeout(
                Duration::from_secs(30),
                changes.wait_for(|state| {
                    state
                        .relationships
                        .get(&watched)
                        .is_some_and(|relationship| {
                            matches!(
                                relationship.binding,
                                BindingState::Bound | BindingState::Failed
                            )
                        })
                }),
            )
            .await;
            let observed = control.observed();
            control.cancel();
            anyhow::ensure!(
                matches!(settled, Ok(Ok(_))),
                "edge never settled: {:?}",
                observed.components
            );
            Result::<_>::Ok(observed)
        });
        anyhow::ensure!(matches!(outcome, Err(GraphError::Cancelled)), "{outcome:?}");
        graph.dispose().await?;
        let observed = observed?;
        Result::<_>::Ok((
            observed.relationships[&edge].clone(),
            observed.components[&id("procedures")].clone(),
            observed.components[&id("graph-query")].clone(),
        ))
    };
    let (relationship, sink, query) =
        deployed(build(settings(database, CALL))?, edge.clone()).await?;
    anyhow::ensure!(
        relationship.binding == BindingState::Failed,
        "{relationship:?}"
    );
    let failure = sink
        .failure
        .clone()
        .context("the sink carries the binding failure")?;
    anyhow::ensure!(
        failure.phase == FailurePhase::Binding
            && failure.disposition == FailureDisposition::Terminal
            && matches!(
                failure.cause.as_ref(),
                GraphError::Incompatible {
                    source: Incompatibility::ProducerIdentity { .. },
                    ..
                }
            ),
        "{failure:?}"
    );
    anyhow::ensure!(sink.realization == RealizationState::Blocked, "{sink:?}");
    anyhow::ensure!(query.failure.is_none(), "{query:?}");
    let mut opted = settings(database, CALL);
    opted["deliveryGuarantee"] = json!("atLeastOnce");
    let (relationship, sink, _) = deployed(build(opted)?, edge).await?;
    anyhow::ensure!(
        relationship.binding == BindingState::Bound && sink.failure.is_none(),
        "{relationship:?} {sink:?}"
    );
    Ok(())
}

pub(crate) async fn proofs(dynamic: bool) -> Result<()> {
    let mut database = Database::new().await?;
    database.initialize().await?;
    let native = libraries(dynamic)?;
    let factory = factory(&native)?;
    let proof = async {
        procedure_lifecycle(&database, &factory).await?;
        large_input(&database, &factory).await?;
        partial_failure_replay(&database, &factory).await?;
        reconnect_after_kill(&database, &factory).await?;
        erase_twice(&database, &factory).await?;
        stream_capacity(&database, &factory).await?;
        volatile_queries(&database, &factory).await?;
        if !dynamic {
            plugin_retry_without_restart(&database).await?;
        }
        Result::<()>::Ok(())
    }
    .await;
    let sessions = database
        .expect_count(
            "SELECT COUNT(*) FROM information_schema.PROCESSLIST WHERE USER='root' AND ID<>CONNECTION_ID()",
            "0",
        )
        .await;
    let removed = database.cleanup();
    combine(
        "native MySQL stored-procedure proofs",
        [proof, sessions, removed],
    )
}
