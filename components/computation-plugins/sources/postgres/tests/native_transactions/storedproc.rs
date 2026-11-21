// Copyright 2026 The Drasi Authors.
// Licensed under the Apache License, Version 2.0.

//! Live tests of the native PostgreSQL stored-procedure sink's per-consumer
//! cursor, recovery and least-privilege behaviour. Build the three PostgreSQL
//! packages with dynamic-plugin before running (see `libraries.rs`).

use super::libraries::{
    count, indexes, options, plugins, prepare_destination, scope, sink_inputs, sink_settings,
    Plugins,
};
use super::*;
use drasi_computation_sink_storedproc_postgres::{create_table_sql, ProgressTable};
use drasi_host_sdk::computation::NativeConsumerProxy;
use serde_json::{json, Value};

const PROGRESS: &str = "public.drasi_delivery_progress";
const FAKE_STREAM: &str =
    "drasi-delivery-stream-v1-ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";

async fn consumer(
    plugins: &Plugins,
    path: &Path,
    graph: &str,
    component: u128,
    settings: Value,
) -> Result<NativeConsumerProxy> {
    plugins.sink.factories()[0]
        .create_consumer_with_inputs(
            component_id(component),
            settings,
            scope(graph),
            indexes(path)?,
            options(),
            sink_inputs()?,
        )
        .await
}

async fn started(
    plugins: &Plugins,
    path: &Path,
    graph: &str,
    component: u128,
    settings: Value,
) -> Result<NativeConsumerProxy> {
    let mut consumer = consumer(plugins, path, graph, component, settings).await?;
    consumer.initialize(&lifecycle_ctx::initialize()).await?;
    consumer.start(&lifecycle_ctx::start()).await?;
    Ok(consumer)
}

fn input(output: &OutputEnvelope) -> InputEnvelope {
    InputEnvelope { execution_context: Default::default(),
        port: PortId::try_new("in").expect("port"),
        envelope: output.envelope.clone(),
    }
}

/// The linked sink driven by a real host `DeliveryRunner`. The Host SDK's
/// `NativeConsumerProxy` refuses every later `handle` after a failed one until
/// Stop and Start; this drives the plugin's own `DeliveryHandler` to prove the
/// sink itself recovers without a restart.
struct Linked {
    runner: DeliveryRunner,
    sink: drasi_computation_sink_storedproc_postgres::PostgresStoredProcSink,
}

impl Linked {
    async fn start(path: &Path, graph: &str, component: u128, settings: Value) -> Result<Self> {
        use drasi_computation_plugin_sdk::{Factory, NativeComponent};
        let factory = drasi_computation_sink_storedproc_postgres::PostgresStoredProcFactory;
        let metadata = factory.metadata();
        let mut sink = factory.build(&drasi_computation_plugin_sdk::CreateRequest {
            id: component_id(component),
            implementation: metadata.implementation,
            configuration_version: metadata.configuration_version,
            configuration: settings,
            scope: Some(scope(graph)),
            inputs: sink_inputs()?,
            activation_credentials_bound: false,
            update_directive: None,
        })?;
        sink.initialize(&drasi_computation_plugin_sdk::lifecycle::InitializeContext::new(
            lifecycle_ctx::scope(),
        ))
        .await?;
        sink.start(&mut lifecycle_ctx::native_start()).await?;
        let mut codec = EnvelopeCodec::new(size(1 << 24));
        codec.register_schema(GraphChangeCodec::schema())?;
        let indexes = indexes(path)?
            .create_indexes(graph, &component_id(component))
            .await?;
        let runner = DeliveryRunner::new(
            DeliveryScope::new("linked".into(), graph.into(), component_id(component))?,
            indexes,
            codec,
            options(),
        )?;
        Ok(Self { runner, sink })
    }

    async fn handle(&mut self, input: InputEnvelope) -> Result<()> {
        self.runner.deliver(&input, &mut self.sink).await
    }

    async fn stop(mut self) -> Result<()> {
        use drasi_computation_plugin_sdk::NativeComponent;
        self.sink.stop(&lifecycle_ctx::native_stop()).await?;
        self.runner.shutdown().await
    }
}

/// The owner hash computed independently by PostgreSQL.
async fn owner(database: &Database, graph: &str, component: u128) -> Result<String> {
    Ok(database
        .client()
        .query_one(
            "SELECT encode(sha256(convert_to('drasi.storedproc/owner/v1', 'UTF8') || '\\x00'::bytea
                 || convert_to($1, 'UTF8') || '\\x00'::bytea || convert_to($2, 'UTF8')), 'hex')",
            &[&graph, &component_id(component).to_string()],
        )
        .await?
        .get(0))
}

async fn owned_rows(database: &Database, table: &str, owner: &str) -> Result<i64> {
    Ok(database
        .client()
        .query_one(
            &format!("SELECT count(*) FROM {table} WHERE owner = $1"),
            &[&owner],
        )
        .await?
        .get(0))
}

async fn insert_row(database: &Database, owner: &str, stream: &str) -> Result<()> {
    database
        .client()
        .execute(
            &format!(
                "INSERT INTO {PROGRESS} (owner, stream_key, version, sequence, digest, operations, completed, epoch, generation, pending)
                 VALUES ($1, $2, 4, '1', decode(repeat('00', 32), 'hex'), 1, 1, '', 0, -1)"
            ),
            &[&owner, &stream],
        )
        .await?;
    Ok(())
}

/// Captures one query output per statement, in sequence order.
async fn captured_many(
    database: &Database,
    path: &Path,
    statements: &[&str],
) -> Result<(ContinuousQueryTransformer, Vec<OutputEnvelope>)> {
    let owner = progress();
    let mut query = query(path, owner.clone()).await?;
    query.start(&lifecycle_ctx::start()).await?;
    let mut source = source(database.config.clone(), owner)?;
    source.start(&lifecycle_ctx::start()).await?;
    let mut outputs = Vec::new();
    for statement in statements {
        database.client().batch_execute(statement).await?;
        let group = next(&mut source).await?;
        let mut produced = query
            .transform(InputEnvelope { execution_context: Default::default(),
                port: PortId::try_new("in")?,
                envelope: group.envelope,
            })
            .await?;
        anyhow::ensure!(produced.len() == 1, "one query output per statement");
        outputs.push(produced.remove(0));
    }
    source.stop(&lifecycle_ctx::stop()).await?;
    Ok((query, outputs))
}

async fn finish(
    mut query: ContinuousQueryTransformer,
    outputs: &[OutputEnvelope],
    database: Database,
) -> Result<()> {
    query.delivery_completed(outputs).await?;
    query.stop(&lifecycle_ctx::stop()).await?;
    database.shutdown().await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn native_storedproc_erase_removes_only_its_owner_rows_and_repeats() -> Result<()> {
    let plugins = plugins(true)?;
    let database = Database::new().await?;
    prepare_destination(&database).await?;
    let directory = tempfile::tempdir()?;
    let (query, outputs) = captured_many(
        &database,
        &directory.path().join("query"),
        &["INSERT INTO person VALUES(1, 'erased')"],
    )
    .await?;
    let graph = "erase-graph";
    let mut first = started(
        &plugins,
        &directory.path().join("a"),
        graph,
        101,
        sink_settings(&database),
    )
    .await?;
    first.handle(input(&outputs[0])).await?;
    assert_eq!(count(&database).await?, 1);
    let mine = owner(&database, graph, 101).await?;
    let other = owner(&database, graph, 102).await?;
    assert_eq!(owned_rows(&database, PROGRESS, &mine).await?, 1);
    insert_row(&database, &other, FAKE_STREAM).await?;
    first.stop(&lifecycle_ctx::stop()).await?;
    first.dispose(&lifecycle_ctx::erase()).await?;
    drop(first);
    assert_eq!(owned_rows(&database, PROGRESS, &mine).await?, 0);
    assert_eq!(owned_rows(&database, PROGRESS, &other).await?, 1);
    // Erasing again (a retried dispose or a second object of the same
    // owner) is idempotent and still leaves the other owner untouched.
    let mut again = consumer(
        &plugins,
        &directory.path().join("a"),
        graph,
        101,
        sink_settings(&database),
    )
    .await?;
    again.initialize(&lifecycle_ctx::initialize()).await?;
    again.dispose(&lifecycle_ctx::erase()).await?;
    drop(again);
    assert_eq!(owned_rows(&database, PROGRESS, &mine).await?, 0);
    assert_eq!(owned_rows(&database, PROGRESS, &other).await?, 1);
    let tables: i64 = database
        .client()
        .query_one(
            "SELECT count(*) FROM pg_tables WHERE tablename = 'drasi_delivery_progress'",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(tables, 1, "Erase never drops the shared table");
    tls::retired(&database).await?;
    finish(query, &outputs, database).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn native_storedproc_stream_capacity_is_per_owner_and_recovers_without_restart() -> Result<()>
{
    let database = Database::new().await?;
    prepare_destination(&database).await?;
    let directory = tempfile::tempdir()?;
    let (query, outputs) = captured_many(
        &database,
        &directory.path().join("query"),
        &[
            "INSERT INTO person VALUES(1, 'one')",
            "INSERT INTO person VALUES(2, 'two')",
        ],
    )
    .await?;
    let graph = "capacity-graph";
    let mut settings = sink_settings(&database);
    settings["maxStreams"] = json!(1);
    let mut consumer =
        Linked::start(&directory.path().join("consumer"), graph, 101, settings).await?;
    // Another owner's streams do not count against this owner's limit.
    let foreign = owner(&database, graph, 103).await?;
    consumer.handle(input(&outputs[0])).await?;
    insert_row(&database, &foreign, FAKE_STREAM).await?;
    let mine = owner(&database, graph, 101).await?;
    assert_eq!(owned_rows(&database, PROGRESS, &mine).await?, 1);
    // Make this owner's only stream look like a different one: the real
    // stream is then a second stream and exceeds maxStreams.
    database
        .client()
        .execute(
            &format!("UPDATE {PROGRESS} SET stream_key = $2 || 'mine' WHERE owner = $1"),
            &[&mine, &FAKE_STREAM],
        )
        .await?;
    let error = consumer.handle(input(&outputs[1])).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("stream capacity (1) is exhausted"),
        "{error:#}"
    );
    assert_eq!(count(&database).await?, 1, "a refused stream has no effect");
    database
        .client()
        .execute(
            &format!("DELETE FROM {PROGRESS} WHERE owner = $1"),
            &[&mine],
        )
        .await?;
    consumer.handle(input(&outputs[1])).await?;
    assert_eq!(count(&database).await?, 2);
    assert_eq!(owned_rows(&database, PROGRESS, &foreign).await?, 1);
    consumer.stop().await?;
    finish(query, &outputs, database).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn native_storedproc_reconnects_after_its_backend_is_terminated() -> Result<()> {
    let plugins = plugins(true)?;
    let database = Database::new().await?;
    prepare_destination(&database).await?;
    let directory = tempfile::tempdir()?;
    let (query, outputs) = captured_many(
        &database,
        &directory.path().join("query"),
        &[
            "INSERT INTO person VALUES(1, 'before')",
            "INSERT INTO person VALUES(2, 'after')",
        ],
    )
    .await?;
    let mut consumer = started(
        &plugins,
        &directory.path().join("consumer"),
        "reconnect-graph",
        101,
        sink_settings(&database),
    )
    .await?;
    consumer.handle(input(&outputs[0])).await?;
    let terminated: i64 = database
        .client()
        .query_one(
            "SELECT count(*) FILTER (WHERE pg_terminate_backend(pid)) FROM pg_stat_activity
             WHERE application_name = 'drasi-native-storedproc-postgres'",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(terminated, 1);
    consumer.handle(input(&outputs[1])).await?;
    assert_eq!(count(&database).await?, 2);
    let sessions: i64 = database
        .client()
        .query_one(
            "SELECT count(*) FROM pg_stat_activity
             WHERE application_name = 'drasi-native-storedproc-postgres'",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(sessions, 1, "the sink replaced its terminated connection");
    consumer.stop(&lifecycle_ctx::stop()).await?;
    drop(consumer);
    tls::retired(&database).await?;
    finish(query, &outputs, database).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn native_storedproc_recovers_after_a_failed_delivery_without_restart() -> Result<()> {
    let database = Database::new().await?;
    prepare_destination(&database).await?;
    let directory = tempfile::tempdir()?;
    let (query, outputs) = captured_many(
        &database,
        &directory.path().join("query"),
        &["INSERT INTO person VALUES(2, 'gated')"],
    )
    .await?;
    database
        .client()
        .batch_execute("UPDATE failure_gate SET enabled = true, failed_id = 2")
        .await?;
    let mut consumer = Linked::start(
        &directory.path().join("consumer"),
        "recovery-graph",
        101,
        sink_settings(&database),
    )
    .await?;
    for _ in 0..2 {
        assert!(consumer.handle(input(&outputs[0])).await.is_err());
        assert_eq!(count(&database).await?, 0);
    }
    database
        .client()
        .batch_execute("UPDATE failure_gate SET enabled = false")
        .await?;
    consumer.handle(input(&outputs[0])).await?;
    assert_eq!(count(&database).await?, 1);
    // A repeated delivery of the completed input has no further effect.
    consumer.handle(input(&outputs[0])).await?;
    assert_eq!(count(&database).await?, 1);
    consumer.stop().await?;
    finish(query, &outputs, database).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn native_storedproc_least_privilege_role_uses_a_precreated_progress_table() -> Result<()> {
    let plugins = plugins(true)?;
    let database = Database::new().await?;
    prepare_destination(&database).await?;
    let table = ProgressTable::parse("drasi.progress")?;
    database
        .client()
        .batch_execute(&format!(
            "CREATE SCHEMA drasi;
             {};
             CREATE TABLE drasi.readonly (LIKE drasi.progress INCLUDING ALL);
             CREATE ROLE sink_role LOGIN PASSWORD 'sink-password';
             REVOKE CREATE ON SCHEMA public FROM PUBLIC;
             GRANT USAGE ON SCHEMA drasi TO sink_role;
             GRANT SELECT, INSERT, UPDATE, DELETE ON drasi.progress, drasi.progress_dead_letter
                 TO sink_role;
             GRANT SELECT ON drasi.readonly TO sink_role;
             GRANT SELECT, INSERT, UPDATE, DELETE ON mirrored, calls TO sink_role;
             GRANT SELECT ON failure_gate TO sink_role;",
            create_table_sql(&table)
        ))
        .await?;
    let directory = tempfile::tempdir()?;
    let (query, outputs) = captured_many(
        &database,
        &directory.path().join("query"),
        &["INSERT INTO person VALUES(1, 'least privilege')"],
    )
    .await?;
    let settings = |progress: &str| {
        let mut settings = sink_settings(&database);
        settings["user"] = json!("sink_role");
        settings["password"] = json!("sink-password");
        settings["progressTable"] = json!(progress);
        settings
    };
    for (progress, expected) in [
        (PROGRESS, "lacks CREATE on schema \"public\""),
        ("drasi.missing", "lacks CREATE on schema \"drasi\""),
        (
            "drasi.readonly",
            "lacks INSERT, UPDATE, DELETE on table drasi.readonly",
        ),
    ] {
        let mut refused = consumer(
            &plugins,
            &directory.path().join(progress),
            "privilege-graph",
            101,
            settings(progress),
        )
        .await?;
        refused.initialize(&lifecycle_ctx::initialize()).await?;
        let error = refused.start(&lifecycle_ctx::start()).await.unwrap_err();
        assert!(
            format!("{error:#}").contains(expected),
            "{progress}: {error:#}"
        );
        refused.stop(&lifecycle_ctx::stop()).await?;
        drop(refused);
    }
    let mut consumer = started(
        &plugins,
        &directory.path().join("granted"),
        "privilege-graph",
        101,
        settings("drasi.progress"),
    )
    .await?;
    consumer.handle(input(&outputs[0])).await?;
    assert_eq!(count(&database).await?, 1);
    let mine = owner(&database, "privilege-graph", 101).await?;
    assert_eq!(owned_rows(&database, "drasi.progress", &mine).await?, 1);
    let public: i64 = database
        .client()
        .query_one(
            "SELECT count(*) FROM pg_tables WHERE tablename = 'drasi_delivery_progress'",
            &[],
        )
        .await?
        .get(0);
    assert_eq!(
        public, 0,
        "nothing was created outside the configured table"
    );
    consumer.stop(&lifecycle_ctx::stop()).await?;
    consumer.dispose(&lifecycle_ctx::erase()).await?;
    drop(consumer);
    assert_eq!(owned_rows(&database, "drasi.progress", &mine).await?, 0);
    finish(query, &outputs, database).await
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires the dynamic PostgreSQL sink library (see libraries.rs)"]
async fn native_storedproc_rejects_template_injection_at_construction() -> Result<()> {
    let plugins = plugins(true)?;
    let directory = tempfile::tempdir()?;
    for template in [
        "CALL native_upsert('ADD', {{after.id}}, {{param after.name}})",
        "CALL native_upsert('ADD', {{{after.id}}}, {{param after.name}})",
        "CALL native_upsert('ADD', '{{after.name}}')",
        "CALL native_upsert({{json after}})",
        "CALL native_upsert({{> injected}})",
        "{{#custom after}}CALL native_upsert({{param after.id}}){{/custom}}",
        "{{#if after}}CALL native_upsert('{{after.name}}'){{/if}}",
    ] {
        let settings = json!({
            "host": "localhost", "database": "postgres", "user": "postgres",
            "sslMode": "disable",
            "defaultTemplate": {"added": {"template": template}}
        });
        let result = consumer(&plugins, directory.path(), "injection-graph", 101, settings).await;
        assert!(result.is_err(), "{template} was accepted");
    }
    Ok(())
}

/// One live output of an in-memory (volatile) query run, as the native
/// query emits it: no persistent progress and a per-run incarnation in its
/// recovery identity. (The transactional source requires persistent owner
/// progress, so the output is encoded directly.)
fn volatile_output(incarnation: &str, sequence: u64, id: i64, name: &str) -> Result<InputEnvelope> {
    use drasi_core::evaluation::{
        context::QueryPartEvaluationContext, variable_value::VariableValue,
    };
    let query = component_id(46);
    let mut envelope = QueryChangeCodec::encode_evaluation(
        None,
        &query,
        SystemMetadata::new(StreamId::try_new("results")?, sequence),
        &ResultNodeProjector::try_new("test/native-storedproc-volatile/out", None)?,
        &[QueryPartEvaluationContext::Adding {
            after: [
                ("id".into(), VariableValue::Integer(id.into())),
                ("name".into(), VariableValue::from(name)),
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
        "construction_scope": "volatile-graph", "graph_id": "volatile-graph",
        "query_id": query.as_str(), "configuration_hash": 7, "incarnation": incarnation
    }))?;
    identity.annotate(&mut envelope, &query)?;
    Ok(InputEnvelope { execution_context: Default::default(),
        port: PortId::try_new("in")?,
        envelope,
    })
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires a Docker-compatible PostgreSQL container runtime"]
async fn native_storedproc_delivers_in_memory_queries_only_at_least_once_by_opt_in() -> Result<()> {
    let plugins = plugins(true)?;
    let database = Database::new().await?;
    prepare_destination(&database).await?;
    let directory = tempfile::tempdir()?;
    let graph = "volatile-graph";
    const FIRST: &str = "5e0c1d2a-3b4c-4d5e-8f60-718293a4b501";
    const SECOND: &str = "5e0c1d2a-3b4c-4d5e-8f60-718293a4b502";
    let first = volatile_output(FIRST, 1, 1, "volatile")?;

    // Without opt-in the consumer refuses the volatile producer permanently,
    // naming the fix, and applies nothing.
    let mut strict = started(
        &plugins,
        &directory.path().join("strict"),
        graph,
        101,
        sink_settings(&database),
    )
    .await?;
    // The graph refuses this edge when it binds it (the exactly-once input
    // declaration against the producer's volatile identity); driven
    // directly, the host refuses the envelope as a permanent typed failure.
    let error = strict.handle(first.clone()).await.unwrap_err();
    assert_eq!(
        drasi_lib::computation::v1::delivery_failure_class(&error),
        Some(drasi_lib::computation::v1::DeliveryFailureClass::Permanent)
    );
    assert!(
        format!("{error:#}").contains("lacks the producer identity"),
        "{error:#}"
    );
    assert_eq!(count(&database).await?, 0);
    strict.stop(&lifecycle_ctx::stop()).await?;
    drop(strict);

    // With atLeastOnce the effect is applied once per producer run; a retry
    // of the same input is deduplicated by the destination cursor.
    let mut settings = sink_settings(&database);
    settings["deliveryGuarantee"] = json!("atLeastOnce");
    let mut opted = started(
        &plugins,
        &directory.path().join("opted"),
        graph,
        102,
        settings.clone(),
    )
    .await?;
    opted.handle(first.clone()).await?;
    opted.handle(first).await?;
    assert_eq!(count(&database).await?, 1);
    let mine = owner(&database, graph, 102).await?;
    assert_eq!(owned_rows(&database, PROGRESS, &mine).await?, 1);
    let epoch: String = database
        .client()
        .query_one(
            "SELECT epoch FROM public.drasi_delivery_progress WHERE owner = $1",
            &[&mine],
        )
        .await?
        .get(0);
    assert_eq!(epoch.len(), 64, "at-least-once rows carry their epoch");

    // A new producer incarnation restarts its sequence. Its first output is
    // delivered against the same stream row, which it overwrites with the new
    // epoch instead of rejecting it as an expired receipt.
    opted
        .handle(volatile_output(SECOND, 1, 2, "restarted")?)
        .await?;
    assert_eq!(count(&database).await?, 2);
    assert_eq!(
        owned_rows(&database, PROGRESS, &mine).await?,
        1,
        "the stream key excludes the incarnation"
    );
    let next: String = database
        .client()
        .query_one(
            "SELECT epoch FROM public.drasi_delivery_progress WHERE owner = $1",
            &[&mine],
        )
        .await?
        .get(0);
    assert_ne!(next, epoch);
    opted.stop(&lifecycle_ctx::stop()).await?;
    opted.dispose(&lifecycle_ctx::erase()).await?;
    drop(opted);
    database.shutdown().await
}
